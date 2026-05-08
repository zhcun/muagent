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
use crate::runtime::subagent_tool::{SubagentExecutor, SubagentTool};

pub struct Wired {
    pub runner: Arc<Runner>,
    pub skills: Arc<SkillManager>,
    pub sessions: SessionManager,
    pub adapters: Arc<AdapterBundle>,
}

pub async fn wire(cfg: &Config) -> Result<Wired, String> {
    wire_with_hooks(cfg, None).await
}

pub async fn wire_with_hooks(
    cfg: &Config,
    hooks: Option<Arc<dyn HookDispatcher>>,
) -> Result<Wired, String> {
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

    if cfg.subagents.enabled && !cfg.subagents.definitions.is_empty() {
        let executor = Arc::new(ConfiguredSubagentExecutor {
            model_net: model_net.clone(),
            base_model_config: cfg.model.clone(),
            base_model: model.clone(),
            registry: registry.clone(),
            skills: skills.clone(),
            hooks: hooks.clone(),
            base_system: base_system.clone(),
            compactor: compactor.clone(),
            cache_policy,
            thinking: thinking.clone(),
            retry_policy,
            workspace_root: workspace_root(cfg),
            parent_tool_allowlist: cfg.capabilities.tool_allowlist.clone(),
            parent_tool_denylist: cfg.capabilities.tool_denylist.clone(),
            parent_skill_allowlist: cfg.capabilities.skill_allowlist.clone(),
            parent_skill_denylist: cfg.capabilities.skill_denylist.clone(),
        });
        let tool = SubagentTool::new(cfg.subagents.definitions.clone(), executor);
        if !tool.is_empty() {
            registry.register(Arc::new(tool));
        }
    }

    let effective_tool_allowlist = parent_tool_allowlist_with_subagents(
        cfg.capabilities.tool_allowlist.clone(),
        &cfg.capabilities.tool_denylist,
        cfg.subagents.enabled && !cfg.subagents.definitions.is_empty(),
    );

    // Executor + provider (host-configured tool filters applied on BOTH sides
    // so filtered tools never execute, even if the LLM hallucinates or
    // replays a tool name from earlier history).
    let mut executor_inner = DefaultToolExecutor::new(registry.clone());
    if let Some(list) = effective_tool_allowlist.clone() {
        executor_inner = executor_inner.with_tool_allowlist(list);
    }
    if !cfg.capabilities.tool_denylist.is_empty() {
        executor_inner = executor_inner.with_tool_denylist(cfg.capabilities.tool_denylist.clone());
    }
    let executor = Arc::new(executor_inner);

    let mut provider = DefaultToolSetProvider::new(registry.clone()).with_skills(skills.clone());
    if let Some(list) = effective_tool_allowlist {
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

    let mut runner_builder = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .hook_model(cfg.model.model.clone())
        .base_system_prompt(base_system)
        .compactor(compactor)
        .cache_policy(cache_policy)
        .thinking(thinking)
        .retry_policy(retry_policy);
    if let Some(hooks) = hooks {
        runner_builder = runner_builder.hooks(hooks);
    }
    let runner = runner_builder
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

struct ConfiguredSubagentExecutor {
    model_net: Arc<dyn crate::core::net::NetEgress>,
    base_model_config: ModelConfig,
    base_model: Arc<dyn ModelAdapter>,
    registry: Arc<CapabilityRegistry>,
    skills: Arc<SkillManager>,
    hooks: Option<Arc<dyn HookDispatcher>>,
    base_system: String,
    compactor: Arc<dyn Compactor>,
    cache_policy: CachePolicy,
    thinking: ThinkingConfig,
    retry_policy: RetryPolicy,
    workspace_root: String,
    parent_tool_allowlist: Option<Vec<String>>,
    parent_tool_denylist: Vec<String>,
    parent_skill_allowlist: Option<Vec<String>>,
    parent_skill_denylist: Vec<String>,
}

#[async_trait::async_trait]
impl SubagentExecutor for ConfiguredSubagentExecutor {
    async fn invoke(
        &self,
        definition: AgentDefinition,
        invocation: SubagentInvocation,
        cancel: CancelToken,
    ) -> Result<SubagentResult, String> {
        let model = self.model_for(&definition)?;
        let tool_allowlist = inherited_or_defined_allowlist(
            definition.tools.clone(),
            self.parent_tool_allowlist.clone(),
            SUBAGENT_TOOL_NAME,
        );
        let tool_denylist =
            inherited_denylist(self.parent_tool_denylist.clone(), SUBAGENT_TOOL_NAME);

        let mut executor_inner = DefaultToolExecutor::new(self.registry.clone());
        if let Some(list) = tool_allowlist.clone() {
            executor_inner = executor_inner.with_tool_allowlist(list);
        }
        if !tool_denylist.is_empty() {
            executor_inner = executor_inner.with_tool_denylist(tool_denylist.clone());
        }

        let mut provider =
            DefaultToolSetProvider::new(self.registry.clone()).with_skills(self.skills.clone());
        if let Some(list) = tool_allowlist {
            provider = provider.with_tool_allowlist(list);
        }
        if !tool_denylist.is_empty() {
            provider = provider.with_tool_denylist(tool_denylist);
        }
        let skill_allowlist = definition
            .skills
            .clone()
            .or_else(|| self.parent_skill_allowlist.clone());
        if let Some(list) = skill_allowlist {
            provider = provider.with_skill_allowlist(list);
        }
        if !self.parent_skill_denylist.is_empty() {
            provider = provider.with_skill_denylist(self.parent_skill_denylist.clone());
        }

        let mut runner_builder = Runner::builder()
            .model(model)
            .tools(Arc::new(executor_inner))
            .store(Arc::new(MemorySessionStore::new()))
            .tools_provider(provider)
            .hook_model(
                definition
                    .model
                    .clone()
                    .unwrap_or_else(|| self.base_model_config.model.clone()),
            )
            .base_system_prompt(subagent_system_prompt(&self.base_system, &definition))
            .compactor(self.compactor.clone())
            .cache_policy(self.cache_policy)
            .thinking(self.thinking.clone())
            .retry_policy(self.retry_policy)
            .cancel_token(cancel.child());
        if let Some(hooks) = &self.hooks {
            runner_builder = runner_builder.hooks(hooks.clone());
        }
        let runner = runner_builder
            .build()
            .map_err(|e| format!("build subagent runner: {e}"))?;

        let now = crate::core::clock::SystemClock.now_ms();
        let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), now);
        state.workspace_root = Some(self.workspace_root.clone());
        state.parent_run_id = Some(invocation.parent_run_id);
        runner
            .submit_user_message(
                &mut state,
                Message::User {
                    content: Content::text(invocation.task),
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        let max_steps = definition
            .max_steps
            .unwrap_or(DEFAULT_SUBAGENT_MAX_STEPS)
            .max(1);
        for _ in 0..max_steps {
            if state.step.is_terminal_or_paused() {
                break;
            }
            runner.step(&mut state).await.map_err(|e| e.to_string())?;
        }

        match &state.step {
            Step::Done { final_text } => Ok(SubagentResult {
                agent_name: definition.name,
                final_text: final_text.clone(),
                run_id: state.run_id,
                session_id: state.session_id,
                usage: state.usage,
            }),
            Step::Failed { reason } => Err(format!("subagent failed: {reason}")),
            Step::Paused { reason } => Err(format!("subagent paused: {reason:?}")),
            step => Err(format!(
                "subagent exceeded step budget {max_steps}; final step={step:?}"
            )),
        }
    }
}

impl ConfiguredSubagentExecutor {
    fn model_for(&self, definition: &AgentDefinition) -> Result<Arc<dyn ModelAdapter>, String> {
        let Some(model) = definition.model.clone() else {
            return Ok(self.base_model.clone());
        };
        let mut cfg = self.base_model_config.clone();
        cfg.model = model;
        build_model_adapter(&cfg, self.model_net.clone())
    }
}

fn subagent_system_prompt(base_system: &str, definition: &AgentDefinition) -> String {
    format!(
        "{base_system}\n\nSubagent profile:\n- Name: {}\n- Description: {}\n\nSubagent instructions:\n{}",
        definition.name,
        if definition.description.trim().is_empty() {
            "(none)"
        } else {
            definition.description.as_str()
        },
        definition.instructions
    )
}

// Subagent nesting is intentionally unsupported: every subagent runner gets
// the subagent delegation tool removed from its allowlist and added to its
// denylist.
fn inherited_or_defined_allowlist(
    defined: Option<Vec<String>>,
    inherited: Option<Vec<String>>,
    remove_default: &str,
) -> Option<Vec<String>> {
    defined.or(inherited).map(|list| {
        list.into_iter()
            .filter(|name| name != remove_default)
            .collect()
    })
}

fn inherited_denylist(mut inherited: Vec<String>, deny_default: &str) -> Vec<String> {
    if !inherited.iter().any(|name| name == deny_default) {
        inherited.push(deny_default.to_string());
    }
    inherited
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use crate::core::error::RuntimeError;
    use crate::core::net::{HttpReq, HttpResp, NetEgress, NetErr};
    use crate::core::testing::{reply, CannedModel};

    struct NeverNet;

    #[async_trait]
    impl NetEgress for NeverNet {
        async fn http(&self, _req: HttpReq, _cancel: CancelToken) -> Result<HttpResp, NetErr> {
            Err(NetErr::Denied(
                "network should be unused in this test".into(),
            ))
        }
    }

    struct NoopCompactor;

    #[async_trait]
    impl Compactor for NoopCompactor {
        async fn maybe_compact(
            &self,
            _state: &mut RunState,
            _system_prompt: &str,
            _cancel: CancelToken,
        ) -> Result<Option<CompactionEvent>, RuntimeError> {
            Ok(None)
        }
    }

    struct SpyReadTool {
        ran: Arc<AtomicBool>,
        desc: ToolDescriptor,
    }

    impl SpyReadTool {
        fn new(ran: Arc<AtomicBool>) -> Self {
            Self {
                ran,
                desc: ToolDescriptor {
                    name: "fs_read".into(),
                    description: "test read tool".into(),
                    schema_json: json!({"type":"object"}),
                    timeout: std::time::Duration::from_secs(1),
                    max_out_tokens: 128,
                    concurrency: Concurrency::Parallel,
                    side_effects: SideEffects::ReadOnly,
                    idempotency: Idempotency::Idempotent,
                },
            }
        }
    }

    #[async_trait]
    impl Tool for SpyReadTool {
        fn descriptor(&self) -> &ToolDescriptor {
            &self.desc
        }

        async fn run(
            &self,
            _args: Value,
            _ctx: &ToolContext,
            _cancel: CancelToken,
        ) -> Result<ToolOk, ToolErr> {
            self.ran.store(true, Ordering::SeqCst);
            Ok(ToolOk::text("DXB-442"))
        }
    }

    #[derive(Default)]
    struct DenyFsReadHook {
        saw_fs_read: AtomicBool,
    }

    #[async_trait]
    impl HookDispatcher for DenyFsReadHook {
        async fn dispatch(&self, input: HookInput, _cancel: CancelToken) -> HookOutput {
            if input.hook_event_name == HookEventName::PreToolUse
                && input.tool_name.as_deref() == Some("fs_read")
            {
                self.saw_fs_read.store(true, Ordering::SeqCst);
                return HookOutput {
                    hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                        permission_decision: Some(HookPermissionDecision::Deny),
                        permission_decision_reason: Some("blocked in subagent".into()),
                    }),
                    ..Default::default()
                };
            }
            HookOutput::default()
        }
    }

    #[test]
    fn subagent_allowlist_removes_delegation_tool_even_when_defined() {
        let allowlist = inherited_or_defined_allowlist(
            Some(vec![
                "fs_read".to_string(),
                SUBAGENT_TOOL_NAME.to_string(),
                "fs_list".to_string(),
            ]),
            Some(vec!["sh_exec".to_string()]),
            SUBAGENT_TOOL_NAME,
        )
        .unwrap();

        assert_eq!(allowlist, vec!["fs_read", "fs_list"]);
    }

    #[test]
    fn subagent_allowlist_removes_delegation_tool_when_inherited() {
        let allowlist = inherited_or_defined_allowlist(
            None,
            Some(vec![
                "fs_read".to_string(),
                SUBAGENT_TOOL_NAME.to_string(),
                "fs_list".to_string(),
            ]),
            SUBAGENT_TOOL_NAME,
        )
        .unwrap();

        assert_eq!(allowlist, vec!["fs_read", "fs_list"]);
    }

    #[test]
    fn subagent_denylist_always_denies_delegation_tool() {
        let denylist = inherited_denylist(vec!["sh_exec".to_string()], SUBAGENT_TOOL_NAME);

        assert!(denylist.iter().any(|name| name == "sh_exec"));
        assert!(denylist.iter().any(|name| name == SUBAGENT_TOOL_NAME));
    }

    #[test]
    fn empty_parent_allowlist_stays_empty_even_with_subagents() {
        let allowlist = parent_tool_allowlist_with_subagents(Some(Vec::new()), &[], true).unwrap();

        assert!(allowlist.is_empty());
    }

    #[test]
    fn non_empty_parent_allowlist_gets_subagent_tool() {
        let allowlist =
            parent_tool_allowlist_with_subagents(Some(vec!["fs_read".into()]), &[], true).unwrap();

        assert_eq!(allowlist, vec!["fs_read", SUBAGENT_TOOL_NAME]);
    }

    #[tokio::test]
    async fn subagent_runner_inherits_hooks_for_internal_tools() {
        let tool_ran = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(CapabilityRegistry::new());
        registry.register(Arc::new(SpyReadTool::new(tool_ran.clone())));
        let hooks = Arc::new(DenyFsReadHook::default());
        let executor = ConfiguredSubagentExecutor {
            model_net: Arc::new(NeverNet),
            base_model_config: ModelConfig {
                provider: Provider::OpenAi,
                base_url: "https://example.invalid".into(),
                model: "test-model".into(),
                api_key: Some("test".into()),
                capabilities: ModelCapabilityOverrides::default(),
            },
            base_model: Arc::new(CannedModel::new(vec![
                reply::with_calls(
                    "read",
                    vec![PendingCall::new("read_contacts", "fs_read", json!({}))],
                ),
                reply::text("done"),
            ])),
            registry,
            skills: Arc::new(SkillManager::new()),
            hooks: Some(hooks.clone()),
            base_system: "base".into(),
            compactor: Arc::new(NoopCompactor),
            cache_policy: CachePolicy::Disabled,
            thinking: ThinkingConfig::default(),
            retry_policy: RetryPolicy::default(),
            workspace_root: ".".into(),
            parent_tool_allowlist: Some(vec!["fs_read".into(), SUBAGENT_TOOL_NAME.into()]),
            parent_tool_denylist: Vec::new(),
            parent_skill_allowlist: None,
            parent_skill_denylist: Vec::new(),
        };

        let result = executor
            .invoke(
                AgentDefinition::new("reviewer", "review", "check").tools(["fs_read"]),
                SubagentInvocation {
                    agent_name: "reviewer".into(),
                    task: "read contacts".into(),
                    parent_run_id: uuid::Uuid::new_v4(),
                    parent_session_id: uuid::Uuid::new_v4(),
                    parent_call_id: None,
                },
                CancelToken::never(),
            )
            .await
            .unwrap();

        assert_eq!(result.final_text, "done");
        assert!(hooks.saw_fs_read.load(Ordering::SeqCst));
        assert!(!tool_ran.load(Ordering::SeqCst));
    }
}

fn parent_tool_allowlist_with_subagents(
    allowlist: Option<Vec<String>>,
    denylist: &[String],
    has_subagents: bool,
) -> Option<Vec<String>> {
    let Some(mut allowlist) = allowlist else {
        return None;
    };
    if allowlist.is_empty() {
        return Some(allowlist);
    }
    if has_subagents
        && !denylist.iter().any(|name| name == SUBAGENT_TOOL_NAME)
        && !allowlist.iter().any(|name| name == SUBAGENT_TOOL_NAME)
    {
        allowlist.push(SUBAGENT_TOOL_NAME.to_string());
    }
    Some(allowlist)
}

fn workspace_root(config: &Config) -> String {
    config
        .fs
        .root
        .canonicalize()
        .unwrap_or_else(|_| config.fs.root.clone())
        .display()
        .to_string()
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
