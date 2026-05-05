//! Wire together everything a CLI session needs, from Config.

use std::sync::Arc;

use crate::adapters::linux::{LinuxFileSystem, LinuxProcessExec};
use crate::adapters::AdapterBundle;
use crate::adapters::ReqwestEgress;
use crate::capabilities::mcp::{register_mcp_tools, McpClient, SseTransport};
use crate::core::prelude::*;
use crate::prelude::*;
use crate::providers::AnthropicAdapter;
use crate::providers::GoogleGeminiAdapter;
use crate::providers::OpenAiAdapter;
use crate::providers::OpenAiCodexAdapter;
use crate::sessions::compaction::{CompactionBudget, RunnerCompactor, SummaryCompaction};
use crate::storage::JsonlSessionStore;
use crate::storage::MemorySessionStore;

use crate::config::{Config, ModelCapabilityOverrides, ModelConfig, Provider, StoreConfig};

pub struct Wired {
    pub runner: Arc<Runner>,
    pub skills: Arc<SkillManager>,
    pub sessions: SessionManager,
    pub adapters: Arc<AdapterBundle>,
}

pub async fn wire(cfg: &Config) -> Result<Wired, String> {
    let model_net = Arc::new(ReqwestEgress::new().map_err(|e| format!("net init: {e:?}"))?);
    let model = build_model_adapter(&cfg.model, model_net.clone())?;

    // Adapter bundle: filesystem workspace/default cwd + shell process execution.
    let fs = Arc::new(LinuxFileSystem::new(vec![cfg.fs.root.clone()]));
    let proc = Arc::new(LinuxProcessExec::new());
    let bundle = Arc::new(
        AdapterBundle::builder()
            .fs(fs)
            .proc(proc)
            .build()
            .map_err(|e| format!("adapter bundle: {e:?}"))?,
    );

    // Registry + built-in tools.
    let registry = Arc::new(CapabilityRegistry::new());
    crate::capabilities::tools::register_defaults(&registry, bundle.clone());
    for endpoint in &cfg.mcp.sse_endpoints {
        let transport = SseTransport::connect(endpoint)
            .await
            .map_err(|e| format!("mcp sse connect {endpoint}: {e}"))?;
        let client = Arc::new(McpClient::new(Box::new(transport)));
        let names = register_mcp_tools(&registry, client)
            .await
            .map_err(|e| format!("mcp sse register {endpoint}: {e}"))?;
        tracing::info!(
            endpoint = %endpoint,
            count = names.len(),
            tools = ?names,
            "mcp sse tools registered"
        );
    }

    let skills = Arc::new(SkillManager::new());

    // Auto-discover skills from ./.muagent/skills/ and ~/.muagent/skills/.
    if cfg.capabilities.skill_autoload {
        let loader = crate::capabilities::skills::loader::FilesystemSkillLoader::default_roots();
        match loader.load_into(&skills) {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(count = ids.len(), ids = ?ids, "skills loaded from filesystem");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "skill autoload failed"),
        }
    }

    // Store.
    let store: Arc<dyn SessionStore> = match &cfg.store {
        StoreConfig::Memory => Arc::new(MemorySessionStore::new()),
        StoreConfig::Jsonl(root) => Arc::new(
            JsonlSessionStore::open(root)
                .await
                .map_err(|e| format!("jsonl store open: {e:?}"))?,
        ),
    };
    let sessions = SessionManager::new(store.clone());

    // Executor + provider (host-configured tool filters applied on BOTH sides
    // so filtered tools never execute, even if the LLM hallucinates or
    // replays a tool name from earlier history).
    let mut executor_inner = DefaultToolExecutor::new(registry.clone());
    if let Some(list) = cfg.capabilities.tool_allowlist.clone() {
        executor_inner = executor_inner.with_tool_allowlist(list);
    }
    if !cfg.capabilities.tool_denylist.is_empty() {
        executor_inner = executor_inner.with_tool_denylist(cfg.capabilities.tool_denylist.clone());
    }
    let executor = Arc::new(executor_inner);

    let mut provider = DefaultToolSetProvider::new(registry).with_skills(skills.clone());
    if let Some(list) = cfg.capabilities.tool_allowlist.clone() {
        provider = provider.with_tool_allowlist(list);
    }
    if !cfg.capabilities.tool_denylist.is_empty() {
        provider = provider.with_tool_denylist(cfg.capabilities.tool_denylist.clone());
    }
    if let Some(list) = cfg.capabilities.skill_allowlist.clone() {
        provider = provider.with_skill_allowlist(list);
    }
    if !cfg.capabilities.skill_denylist.is_empty() {
        provider = provider.with_skill_denylist(cfg.capabilities.skill_denylist.clone());
    }

    // Compaction. Summarizer model is independent of the main agent model:
    // a smaller / cheaper model (haiku, mini, flash) does this prose
    // summarization just as well at a fraction of the price. Falls back to
    // the main model when no MUAGENT_SUMMARIZER_* env is set.
    let summarizer: Arc<dyn ModelAdapter> = match &cfg.compaction.summarizer {
        Some(sm) => {
            tracing::info!(
                summarizer_model = %sm.model,
                summarizer_provider = ?sm.provider,
                "using dedicated summarizer model"
            );
            build_model_adapter(sm, model_net.clone())?
        }
        None => model.clone(),
    };
    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(CompactionBudget {
            max_tokens: cfg.compaction.max_tokens,
            threshold_ratio: cfg.compaction.threshold_ratio,
            keep_tail_turns: cfg.compaction.keep_tail_turns,
            keep_recent_tokens: cfg.compaction.keep_recent_tokens,
            root_task_pin_max_tokens: cfg.compaction.root_task_pin_max_tokens,
            summary_target_chars: 1200,
            summary_input_max_tokens: cfg.compaction.summary_input_max_tokens,
            summary_output_max_tokens: cfg.compaction.summary_output_max_tokens,
            restart_repair_window_tokens: cfg.compaction.restart_repair_window_tokens,
            max_summary_rounds: cfg.compaction.max_summary_rounds,
        }),
        summarizer,
    ));

    let base_system = system_prompt(cfg);

    let cache_policy = if cfg.runtime.cache_auto {
        crate::core::cache::CachePolicy::Auto
    } else {
        crate::core::cache::CachePolicy::Disabled
    };

    let thinking = {
        use crate::config::{EffortCfg, ThinkingModeCfg};
        use crate::core::thinking::{ThinkingConfig, ThinkingEffort, ThinkingMode};
        let mode = match cfg.runtime.thinking_mode {
            ThinkingModeCfg::Off => ThinkingMode::Off,
            ThinkingModeCfg::Auto => ThinkingMode::Auto,
            ThinkingModeCfg::Enabled => ThinkingMode::Enabled,
        };
        let effort = cfg.runtime.thinking_effort.map(|e| match e {
            EffortCfg::Minimal => ThinkingEffort::Minimal,
            EffortCfg::Low => ThinkingEffort::Low,
            EffortCfg::Medium => ThinkingEffort::Medium,
            EffortCfg::High => ThinkingEffort::High,
            EffortCfg::Max => ThinkingEffort::Max,
        });
        ThinkingConfig {
            mode,
            effort,
            ..Default::default()
        }
    };

    let retry_policy = RetryPolicy::from_env()?;
    tracing::info!(
        target: "muagent::setup",
        kind = "runtime_wired",
        provider = cfg.model.provider.cli_name(),
        model = %cfg.model.model,
        base_url = %cfg.model.base_url,
        fs_root = %cfg.fs.root.display(),
        store = %store_label(&cfg.store),
        cache_auto = cfg.runtime.cache_auto,
        thinking_mode = ?cfg.runtime.thinking_mode,
        thinking_effort = ?cfg.runtime.thinking_effort,
        compaction_max_tokens = cfg.compaction.max_tokens,
        compaction_threshold = cfg.compaction.threshold_ratio,
        skill_autoload = cfg.capabilities.skill_autoload,
        "runtime wired"
    );

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .hook_model(cfg.model.model.clone())
        .base_system_prompt(base_system)
        .compactor(compactor)
        .cache_policy(cache_policy)
        .thinking(thinking)
        .retry_policy(retry_policy)
        .build()
        .map_err(|e| format!("build runner: {e:?}"))?;

    Ok(Wired {
        runner: Arc::new(runner),
        skills,
        sessions,
        adapters: bundle,
    })
}

fn store_label(store: &StoreConfig) -> String {
    match store {
        StoreConfig::Memory => "memory".into(),
        StoreConfig::Jsonl(path) => format!("jsonl:{}", path.display()),
    }
}

const DEFAULT_SYSTEM_PROMPT_L0: &str = include_str!("prompts/default-system.md");

fn system_prompt(cfg: &Config) -> String {
    let mut s = String::from(DEFAULT_SYSTEM_PROMPT_L0);
    if cfg.agent_instructions.enabled {
        let instructions = crate::agent_instructions::load(
            &cfg.fs.root,
            cfg.agent_instructions.max_bytes_per_file,
        );
        s.push_str("\n\n");
        s.push_str(&instructions.render());
    }
    s.push_str("\nRuntime environment:\n");
    s.push_str(&format!(
        "- Current date: {}\n",
        crate::core::clock::utc_date_string(crate::core::clock::SystemClock.now_ms())
    ));
    s.push_str(&format!(
        "- Operating system: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    s.push_str(&format!(
        "- Workspace directory: {}\n",
        cfg.fs.root.display()
    ));
    s.push_str(
        "- Filesystem tools: use absolute file:// paths. The workspace directory \
         is default context, not an access boundary; host OS permissions and \
         tool guards still apply.\n",
    );
    s.push_str(
        "- Shell execution: enabled. sh_exec can run binaries available on PATH \
         from the workspace directory.\n",
    );
    if !cfg.mcp.sse_endpoints.is_empty() {
        s.push_str(&format!(
            "- External MCP SSE endpoints connected: {}. Their tools are available by their listed tool names.\n",
            cfg.mcp.sse_endpoints.len()
        ));
    }
    s
}

/// Construct a `ModelAdapter` from a `ModelConfig`. Used for both the main
/// agent model and (optionally) a separate summarizer. Centralizing it
/// keeps the env-key requirements consistent — same `MUAGENT_API_KEY`
/// fallback behavior for either role.
fn build_model_adapter(
    m: &ModelConfig,
    net: Arc<dyn crate::core::net::NetEgress>,
) -> Result<Arc<dyn ModelAdapter>, String> {
    Ok(match m.provider {
        Provider::OpenAi | Provider::OpenRouter => {
            let adapter = OpenAiAdapter::new(net, &m.base_url, &m.model, m.api_key.clone());
            let caps = apply_caps_overrides(adapter.caps(), &m.capabilities);
            Arc::new(adapter.with_caps(caps))
        }
        Provider::Codex => {
            let adapter = OpenAiCodexAdapter::new(net, &m.base_url, &m.model, m.api_key.clone());
            let caps = apply_caps_overrides(adapter.caps(), &m.capabilities);
            Arc::new(adapter.with_caps(caps))
        }
        Provider::Anthropic => {
            let key = m
                .api_key
                .clone()
                .ok_or_else(|| "ANTHROPIC_API_KEY (or MUAGENT_API_KEY) required".to_string())?;
            let adapter = AnthropicAdapter::new(net, &m.base_url, &m.model, key);
            let caps = apply_caps_overrides(adapter.caps(), &m.capabilities);
            Arc::new(adapter.with_caps(caps))
        }
        Provider::Google => {
            let key = m
                .api_key
                .clone()
                .ok_or_else(|| "GEMINI_API_KEY (or MUAGENT_API_KEY) required".to_string())?;
            let adapter = GoogleGeminiAdapter::new(net, &m.base_url, &m.model, key);
            let caps = apply_caps_overrides(adapter.caps(), &m.capabilities);
            Arc::new(adapter.with_caps(caps))
        }
    })
}

fn apply_caps_overrides(
    mut caps: crate::core::model::LlmCaps,
    overrides: &ModelCapabilityOverrides,
) -> crate::core::model::LlmCaps {
    if let Some(v) = overrides.native_tool_use {
        caps.native_tool_use = v;
    }
    if let Some(v) = overrides.json_schema_mode {
        caps.json_schema_mode = v;
    }
    if let Some(v) = overrides.vision {
        caps.vision = v;
    }
    if let Some(v) = overrides.streaming {
        caps.streaming = v;
    }
    if let Some(v) = overrides.ctx_len {
        caps.ctx_len = v;
    }
    if let Some(v) = overrides.prompt_cache {
        caps.prompt_cache = v;
    }
    if let Some(v) = overrides.thinking {
        caps.thinking = v;
    }
    caps
}
