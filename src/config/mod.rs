//! CLI configuration.
//!
//! The config pipeline is intentionally layered:
//!
//! 1. Built-in defaults define a runnable agent.
//! 2. User config (`~/.muagent/config.toml`) adds machine/user defaults.
//! 3. Project config (`.muagent/config.toml` from ancestors) narrows or overrides.
//! 4. Environment variables and `.env` override files.
//! 5. CLI flags override everything else.
//!
//! The final [`Config`] is fully typed and validated before runtime wiring.
//! Files use real TOML parsing; empty arrays are meaningful (`enabled = []`
//! means "expose none", while an omitted key means "use the default").

use std::path::PathBuf;

use crate::core::subagent::AgentDefinition;
use crate::core::thinking::ThinkingSupport;

mod agents;
mod file;
use file::{norm_key, FileConfig};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Codex,
    Anthropic,
    Google,
    OpenRouter,
}

impl Provider {
    pub fn cli_name(&self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Codex => "codex",
            Provider::Anthropic => "anthropic",
            Provider::Google => "google",
            Provider::OpenRouter => "openrouter",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::Codex => "Codex",
            Provider::Anthropic => "Anthropic",
            Provider::Google => "Google",
            Provider::OpenRouter => "OpenRouter",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub model: ModelConfig,
    pub fs: FsConfig,
    pub compaction: CompactionConfig,
    pub capabilities: CapabilityConfig,
    pub mcp: McpConfig,
    pub runtime: RuntimeConfig,
    pub agent_instructions: AgentInstructionConfig,
    pub subagents: SubagentConfig,
    pub store: StoreConfig,
}

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub capabilities: ModelCapabilityOverrides,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCapabilityOverrides {
    pub native_tool_use: Option<bool>,
    pub json_schema_mode: Option<bool>,
    pub vision: Option<bool>,
    pub streaming: Option<bool>,
    pub ctx_len: Option<u32>,
    pub prompt_cache: Option<bool>,
    pub thinking: Option<ThinkingSupport>,
}

#[derive(Clone, Debug)]
pub struct FsConfig {
    pub root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CompactionConfig {
    pub max_tokens: u32,
    pub threshold_ratio: f32,
    pub keep_tail_turns: u32,
    pub keep_recent_tokens: u32,
    pub root_task_pin_max_tokens: u32,
    pub summary_input_max_tokens: u32,
    pub summary_output_max_tokens: u32,
    pub restart_repair_window_tokens: u32,
    pub max_summary_rounds: u32,
    /// Optional: dedicated model for the summarizer, decoupled from the
    /// main agent model. Recommended for cost — a smaller / cheaper model
    /// (haiku, mini, flash) summarizes just as well for handoff prose and
    /// runs at a fraction of the price. If `None`, the main model is
    /// reused. Falls back to the main model's provider/key when `provider`
    /// is left at the default and only `model`/`base_url` are overridden.
    pub summarizer: Option<ModelConfig>,
}

#[derive(Clone, Debug)]
pub struct CapabilityConfig {
    /// `None` = expose all registered tools. `Some(list)` = expose only these names.
    pub tool_allowlist: Option<Vec<String>>,
    /// Tools hidden from the model and rejected at execution time.
    pub tool_denylist: Vec<String>,
    /// `None` = expose all discovered skill descriptions. `Some(list)` = only these ids.
    pub skill_allowlist: Option<Vec<String>>,
    /// Skill descriptions hidden from the model prompt.
    pub skill_denylist: Vec<String>,
    /// Auto-discover skills from `./.muagent/skills/` and `~/.muagent/skills/`.
    pub skill_autoload: bool,
}

#[derive(Clone, Debug)]
pub struct McpConfig {
    /// Legacy MCP SSE endpoints to connect and register as tools.
    pub sse_endpoints: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub cache_auto: bool,
    pub thinking_mode: ThinkingModeCfg,
    pub thinking_effort: Option<EffortCfg>,
}

#[derive(Clone, Debug)]
pub struct AgentInstructionConfig {
    pub enabled: bool,
    pub max_bytes_per_file: usize,
}

#[derive(Clone, Debug)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub definitions: Vec<AgentDefinition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingModeCfg {
    Off,
    Auto,
    Enabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortCfg {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug)]
pub enum StoreConfig {
    Memory,
    Jsonl(PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub config_file: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub store: Option<String>,
    pub root: Option<String>,
    pub cache: Option<String>,
    pub thinking: Option<String>,
    pub max_tokens: Option<u32>,
    pub tool_allowlist: Option<Vec<String>>,
    pub tool_denylist: Option<Vec<String>>,
    pub skill_allowlist: Option<Vec<String>>,
    pub skill_denylist: Option<Vec<String>>,
    pub skill_autoload: Option<bool>,
    pub mcp_sse: Option<Vec<String>>,
    pub agent_md: Option<bool>,
    pub agent_md_max_bytes: Option<usize>,
    pub subagents_enabled: Option<bool>,
    pub subagents: Option<Vec<AgentDefinition>>,
    pub log: Option<String>,
}

impl Config {
    /// Load config files and `.env`, then apply process env and CLI overrides.
    pub fn load(overrides: &ConfigOverrides) -> Result<Self, String> {
        for p in &[".env", "../.env", "../../.env"] {
            let _ = dotenvy::from_path(p);
        }

        let file_cfg = FileConfig::load(overrides.config_file.as_deref())?;
        file_cfg.warn_unknown_keys();

        let fs = FsConfig::from_sources(&file_cfg, overrides);
        let cfg = Self {
            model: ModelConfig::from_sources(&file_cfg, overrides)?,
            fs: fs.clone(),
            compaction: CompactionConfig::from_sources(&file_cfg, overrides)?,
            capabilities: CapabilityConfig::from_sources(&file_cfg, overrides)?,
            mcp: McpConfig::from_sources(&file_cfg, overrides),
            runtime: RuntimeConfig::from_sources(&file_cfg, overrides)?,
            agent_instructions: AgentInstructionConfig::from_sources(&file_cfg, overrides)?,
            subagents: SubagentConfig::from_sources(&file_cfg, overrides, &fs.root)?,
            store: StoreConfig::from_sources(&file_cfg, overrides),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if self.model.model.trim().is_empty() {
            return Err("model.model cannot be empty".into());
        }
        if self.model.base_url.trim().is_empty() {
            return Err("model.base_url cannot be empty".into());
        }
        let c = &self.compaction;
        if c.max_tokens == 0 {
            return Err("compaction.max_tokens must be > 0".into());
        }
        if !(c.threshold_ratio > 0.0 && c.threshold_ratio <= 1.0) {
            return Err("compaction.threshold_ratio must be in (0, 1]".into());
        }
        if c.summary_input_max_tokens == 0 {
            return Err("compaction.summary_input_max_tokens must be > 0".into());
        }
        if c.summary_output_max_tokens == 0 {
            return Err("compaction.summary_output_max_tokens must be > 0".into());
        }
        if c.max_summary_rounds == 0 {
            return Err("compaction.max_summary_rounds must be > 0".into());
        }
        if self.agent_instructions.max_bytes_per_file == 0 {
            return Err("agent_md.max_bytes must be > 0".into());
        }
        Ok(())
    }
}

impl ModelConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Result<Self, String> {
        let default_provider_raw = file_cfg
            .string(&["model.provider", "provider"])
            .unwrap_or_else(|| "openrouter".into());
        let default_provider = parse_provider(&default_provider_raw)?;

        let provider_raw = overrides
            .provider
            .clone()
            .or_else(|| env_string("MUAGENT_PROVIDER"))
            .unwrap_or(default_provider_raw);
        let provider = parse_provider(&provider_raw)?;

        let default_base = default_base_url(&provider);
        let key_env = default_key_env(&provider);

        let base_url = overrides
            .base_url
            .clone()
            .or_else(|| env_string("MUAGENT_BASE_URL"))
            .or_else(|| provider_env_name(&provider, "BASE_URL").and_then(|name| env_string(&name)))
            .or_else(|| scoped_model_field(file_cfg, &provider, &default_provider, "base_url"))
            .unwrap_or_else(|| default_base.into());

        let model = overrides
            .model
            .clone()
            .or_else(|| env_string("MUAGENT_MODEL"))
            .or_else(|| provider_env_name(&provider, "MODEL").and_then(|name| env_string(&name)))
            .or_else(|| scoped_model_field(file_cfg, &provider, &default_provider, "model"))
            .unwrap_or_else(|| default_model(&provider).into());

        let api_key = env_string("MUAGENT_API_KEY")
            .or_else(|| env_string(key_env))
            .or_else(|| {
                scoped_model_field(file_cfg, &provider, &default_provider, "api_key_env")
                    .and_then(|name| env_string(&name))
            })
            .or_else(|| scoped_model_field(file_cfg, &provider, &default_provider, "api_key"));

        let capabilities =
            ModelCapabilityOverrides::from_sources(file_cfg, &provider, &default_provider, &model)?;

        Ok(Self {
            provider,
            base_url,
            model,
            api_key,
            capabilities,
        })
    }
}

impl ModelCapabilityOverrides {
    fn from_sources(
        file_cfg: &FileConfig,
        provider: &Provider,
        default_provider: &Provider,
        model: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            native_tool_use: parse_scoped_cap_bool(
                file_cfg,
                provider,
                default_provider,
                model,
                &["native_tool_use", "tool_use", "tool_calling"],
                "capabilities.native_tool_use",
            )?,
            json_schema_mode: parse_scoped_cap_bool(
                file_cfg,
                provider,
                default_provider,
                model,
                &["json_schema_mode", "json_schema"],
                "capabilities.json_schema_mode",
            )?,
            vision: parse_scoped_cap_bool(
                file_cfg,
                provider,
                default_provider,
                model,
                &["vision", "image", "images"],
                "capabilities.vision",
            )?,
            streaming: parse_scoped_cap_bool(
                file_cfg,
                provider,
                default_provider,
                model,
                &["streaming", "stream"],
                "capabilities.streaming",
            )?,
            ctx_len: parse_scoped_cap_u32(
                file_cfg,
                provider,
                default_provider,
                model,
                &[
                    "ctx_len",
                    "context_window",
                    "context_length",
                    "max_context_tokens",
                ],
                "capabilities.ctx_len",
            )?,
            prompt_cache: parse_scoped_cap_bool(
                file_cfg,
                provider,
                default_provider,
                model,
                &["prompt_cache", "cache"],
                "capabilities.prompt_cache",
            )?,
            thinking: parse_scoped_cap_thinking(
                file_cfg,
                provider,
                default_provider,
                model,
                &["thinking", "reasoning"],
                "capabilities.thinking",
            )?,
        })
    }
}

impl FsConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Self {
        let root = overrides
            .root
            .clone()
            .or_else(|| env_string("MUAGENT_ROOT"))
            .or_else(|| file_cfg.string(&["fs.root", "root", "fs_root"]))
            .map(expand_home)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        Self { root }
    }
}

impl CompactionConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Result<Self, String> {
        Ok(Self {
            max_tokens: overrides
                .max_tokens
                .or(parse_env_u32("MUAGENT_MAX_TOKENS")?)
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.max_tokens", "max_tokens"],
                    "compaction.max_tokens",
                )?)
                .unwrap_or(156_000),
            threshold_ratio: parse_env_f32("MUAGENT_COMPACTION_THRESHOLD")?
                .or(parse_file_f32(
                    file_cfg,
                    &["compaction.threshold_ratio", "compaction.threshold"],
                    "compaction.threshold_ratio",
                )?)
                .unwrap_or(0.8),
            keep_tail_turns: parse_env_u32("MUAGENT_KEEP_TAIL_TURNS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.keep_tail_turns", "keep_tail_turns"],
                    "compaction.keep_tail_turns",
                )?)
                .unwrap_or(4),
            keep_recent_tokens: parse_env_u32("MUAGENT_KEEP_RECENT_TOKENS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.keep_recent_tokens", "keep_recent_tokens"],
                    "compaction.keep_recent_tokens",
                )?)
                .unwrap_or(20_000),
            root_task_pin_max_tokens: parse_env_u32("MUAGENT_ROOT_TASK_PIN_MAX_TOKENS")?
                .or(parse_file_u32(
                    file_cfg,
                    &[
                        "compaction.root_task_pin_max_tokens",
                        "root_task_pin_max_tokens",
                    ],
                    "compaction.root_task_pin_max_tokens",
                )?)
                .unwrap_or(1_024),
            summary_input_max_tokens: parse_env_u32("MUAGENT_SUMMARY_INPUT_MAX_TOKENS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.summary_input_max_tokens"],
                    "compaction.summary_input_max_tokens",
                )?)
                .unwrap_or(100_000),
            summary_output_max_tokens: parse_env_u32("MUAGENT_SUMMARY_OUTPUT_MAX_TOKENS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.summary_output_max_tokens"],
                    "compaction.summary_output_max_tokens",
                )?)
                .unwrap_or(8_000),
            restart_repair_window_tokens: parse_env_u32("MUAGENT_RESTART_REPAIR_WINDOW_TOKENS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.restart_repair_window_tokens"],
                    "compaction.restart_repair_window_tokens",
                )?)
                .unwrap_or(300_000),
            max_summary_rounds: parse_env_u32("MUAGENT_MAX_SUMMARY_ROUNDS")?
                .or(parse_file_u32(
                    file_cfg,
                    &["compaction.max_summary_rounds"],
                    "compaction.max_summary_rounds",
                )?)
                .unwrap_or(4),
            // Independent summarizer model. Resolved purely from env so
            // hosts can opt in without touching their config file:
            //   MUAGENT_SUMMARIZER_MODEL=openai/gpt-5.4-nano
            //   MUAGENT_SUMMARIZER_BASE_URL=https://openrouter.ai/api/v1
            //   MUAGENT_SUMMARIZER_PROVIDER=openai (defaults to main)
            //   MUAGENT_SUMMARIZER_API_KEY=...      (defaults to main)
            // Setting `MUAGENT_SUMMARIZER_MODEL` alone is the typical case
            // — base_url / api_key inherit from the main model.
            summarizer: build_summarizer_config()?,
        })
    }
}

/// Resolve an optional summarizer ModelConfig from env vars only. Returns
/// `Ok(None)` when no `MUAGENT_SUMMARIZER_MODEL` is set; returns
/// `Ok(Some(_))` when at least the model name is provided. Provider /
/// base_url / api_key fall back to the main-model defaults via the
/// existing `default_*` helpers — that keeps "use a smaller/cheaper
/// summarizer on the same provider" a one-env-var change.
fn build_summarizer_config() -> Result<Option<ModelConfig>, String> {
    let Some(model) = env_string("MUAGENT_SUMMARIZER_MODEL") else {
        return Ok(None);
    };
    let provider_raw =
        env_string("MUAGENT_SUMMARIZER_PROVIDER").or_else(|| env_string("MUAGENT_PROVIDER"));
    let provider = match provider_raw {
        Some(s) => parse_provider(&s)?,
        None => Provider::OpenAi,
    };
    let base_url = env_string("MUAGENT_SUMMARIZER_BASE_URL")
        .or_else(|| env_string("MUAGENT_BASE_URL"))
        .unwrap_or_else(|| default_base_url(&provider).into());
    let api_key = env_string("MUAGENT_SUMMARIZER_API_KEY")
        .or_else(|| env_string("MUAGENT_API_KEY"))
        .or_else(|| env_string(default_key_env(&provider)));
    Ok(Some(ModelConfig {
        provider,
        base_url,
        model,
        api_key,
        capabilities: ModelCapabilityOverrides::default(),
    }))
}

impl CapabilityConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Result<Self, String> {
        let skill_autoload = overrides
            .skill_autoload
            .or(parse_env_bool("MUAGENT_SKILL_AUTOLOAD")?)
            .or(parse_file_bool(
                file_cfg,
                &["capabilities.skill_autoload", "skill_autoload"],
                "capabilities.skill_autoload",
            )?)
            .unwrap_or(true);

        Ok(Self {
            tool_allowlist: overrides
                .tool_allowlist
                .clone()
                .or_else(|| env_list("MUAGENT_TOOLS"))
                .or_else(|| file_cfg.list(&["capabilities.tools", "tools", "tools.enabled"])),
            tool_denylist: overrides
                .tool_denylist
                .clone()
                .or_else(|| env_list("MUAGENT_DISABLE_TOOLS"))
                .or_else(|| {
                    file_cfg.list(&[
                        "capabilities.disabled_tools",
                        "capabilities.disable_tools",
                        "tools.disabled",
                        "tools.disable",
                        "disabled_tools",
                    ])
                })
                .unwrap_or_default(),
            skill_allowlist: overrides
                .skill_allowlist
                .clone()
                .or_else(|| env_list("MUAGENT_SKILLS"))
                .or_else(|| file_cfg.list(&["capabilities.skills", "skills", "skills.enabled"])),
            skill_denylist: overrides
                .skill_denylist
                .clone()
                .or_else(|| env_list("MUAGENT_DISABLE_SKILLS"))
                .or_else(|| {
                    file_cfg.list(&[
                        "capabilities.disabled_skills",
                        "capabilities.disable_skills",
                        "skills.disabled",
                        "skills.disable",
                        "disabled_skills",
                    ])
                })
                .unwrap_or_default(),
            skill_autoload,
        })
    }
}

impl McpConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Self {
        let sse_endpoints = overrides
            .mcp_sse
            .clone()
            .or_else(|| env_list("MUAGENT_MCP_SSE"))
            .or_else(|| file_cfg.list(&["mcp.sse", "mcp.sse_endpoints", "mcp_sse"]))
            .unwrap_or_default();
        Self { sse_endpoints }
    }
}

impl RuntimeConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Result<Self, String> {
        let cache_auto = overrides
            .cache
            .as_deref()
            .map(|raw| parse_bool(raw).ok_or_else(|| format!("invalid --cache value `{raw}`")))
            .transpose()?
            .or(parse_env_bool("MUAGENT_CACHE")?)
            .or(parse_file_bool(
                file_cfg,
                &["runtime.cache", "cache"],
                "runtime.cache",
            )?)
            .unwrap_or(true);

        let thinking_raw = overrides
            .thinking
            .clone()
            .or_else(|| env_string("MUAGENT_THINKING"))
            .or_else(|| file_cfg.string(&["runtime.thinking", "thinking"]));
        let (thinking_mode, thinking_effort) = parse_thinking(thinking_raw.as_deref())?;

        Ok(Self {
            cache_auto,
            thinking_mode,
            thinking_effort,
        })
    }
}

impl AgentInstructionConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Result<Self, String> {
        Ok(Self {
            enabled: overrides
                .agent_md
                .or(parse_env_bool("MUAGENT_AGENT_MD")?)
                .or(parse_file_bool(
                    file_cfg,
                    &["agent_md.enabled", "agent_md", "agent.md"],
                    "agent_md.enabled",
                )?)
                .unwrap_or(true),
            max_bytes_per_file: overrides
                .agent_md_max_bytes
                .or(parse_env_usize("MUAGENT_AGENT_MD_MAX_BYTES")?)
                .or(parse_file_usize(
                    file_cfg,
                    &["agent_md.max_bytes", "agent_md.max_bytes_per_file"],
                    "agent_md.max_bytes",
                )?)
                .unwrap_or(64 * 1024),
        })
    }
}

impl SubagentConfig {
    fn from_sources(
        file_cfg: &FileConfig,
        overrides: &ConfigOverrides,
        root: &std::path::Path,
    ) -> Result<Self, String> {
        let enabled = overrides
            .subagents_enabled
            .or(parse_env_bool("MUAGENT_SUBAGENTS")?)
            .or(parse_file_bool(
                file_cfg,
                &[
                    "subagents.enabled",
                    "subagents.enable",
                    "subagent_tools.enabled",
                    "agent_tool.enabled",
                ],
                "subagents.enabled",
            )?)
            .unwrap_or(true);

        let mut definitions = if enabled {
            agents::load_agent_definitions(root)
        } else {
            Vec::new()
        };
        if let Some(extra) = overrides.subagents.clone() {
            definitions.extend(extra);
        }

        Ok(Self {
            enabled,
            definitions: validate_agent_definitions(definitions)?,
        })
    }
}

fn validate_agent_definitions(
    definitions: Vec<AgentDefinition>,
) -> Result<Vec<AgentDefinition>, String> {
    let mut out = std::collections::BTreeMap::new();
    for def in definitions {
        if def.name.trim().is_empty() {
            return Err("subagent name cannot be empty".into());
        }
        out.insert(def.name.clone(), def);
    }
    Ok(out.into_values().collect())
}

impl StoreConfig {
    fn from_sources(file_cfg: &FileConfig, overrides: &ConfigOverrides) -> Self {
        let store_spec = overrides
            .store
            .clone()
            .or_else(|| env_string("MUAGENT_STORE"))
            .or_else(|| file_cfg.string(&["store", "store.path", "storage.path"]));
        match store_spec {
            None => StoreConfig::Jsonl(default_store_root()),
            Some(s) if s.trim().is_empty() || s == "memory" => StoreConfig::Memory,
            Some(s) if s.starts_with("jsonl:") => {
                StoreConfig::Jsonl(expand_home(s.trim_start_matches("jsonl:")))
            }
            Some(path) => StoreConfig::Jsonl(expand_home(path)),
        }
    }
}

pub fn parse_list_arg(raw: &str) -> Vec<String> {
    split_list(raw)
}

fn parse_provider(raw: &str) -> Result<Provider, String> {
    match raw.to_lowercase().as_str() {
        "openai" => Ok(Provider::OpenAi),
        "openai-codex" | "openai_codex" | "codex" | "chatgpt" => Ok(Provider::Codex),
        "anthropic" | "claude" => Ok(Provider::Anthropic),
        "google" | "gemini" => Ok(Provider::Google),
        "openrouter" => Ok(Provider::OpenRouter),
        other => Err(format!("unknown provider: {other}")),
    }
}

fn default_base_url(provider: &Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "https://api.openai.com/v1",
        Provider::Codex => "https://chatgpt.com/backend-api",
        Provider::Anthropic => "https://api.anthropic.com",
        Provider::Google => "https://generativelanguage.googleapis.com",
        Provider::OpenRouter => "https://openrouter.ai/api/v1",
    }
}

fn default_key_env(provider: &Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "OPENAI_API_KEY",
        Provider::Codex => "OPENAI_CODEX_ACCESS_TOKEN",
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::Google => "GEMINI_API_KEY",
        Provider::OpenRouter => "OPENROUTER_API_KEY",
    }
}

fn default_model(provider: &Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "gpt-5.4-nano",
        Provider::Codex => "gpt-5.5",
        Provider::Anthropic => "claude-haiku-4-5",
        Provider::Google => "gemini-3.1-flash-lite-preview",
        Provider::OpenRouter => "openai/gpt-5.4-nano",
    }
}

fn provider_config_ids(provider: &Provider) -> &'static [&'static str] {
    match provider {
        Provider::OpenAi => &["openai"],
        Provider::Codex => &["codex", "openai_codex"],
        Provider::Anthropic => &["anthropic"],
        Provider::Google => &["google"],
        Provider::OpenRouter => &["openrouter"],
    }
}

fn provider_env_name(provider: &Provider, suffix: &str) -> Option<String> {
    match (provider, suffix) {
        (Provider::OpenAi, "MODEL") => Some("OPENAI_MODEL".into()),
        (Provider::OpenAi, "BASE_URL") => Some("OPENAI_BASE_URL".into()),
        (Provider::Codex, "MODEL") => Some("OPENAI_CODEX_MODEL".into()),
        (Provider::Codex, "BASE_URL") => Some("OPENAI_CODEX_BASE_URL".into()),
        (Provider::Anthropic, "MODEL") => Some("ANTHROPIC_MODEL".into()),
        (Provider::Anthropic, "BASE_URL") => Some("ANTHROPIC_BASE_URL".into()),
        (Provider::Google, "MODEL") => Some("GEMINI_MODEL".into()),
        (Provider::Google, "BASE_URL") => Some("GEMINI_BASE_URL".into()),
        (Provider::OpenRouter, "MODEL") => Some("OPENROUTER_MODEL".into()),
        (Provider::OpenRouter, "BASE_URL") => Some("OPENROUTER_BASE_URL".into()),
        _ => None,
    }
}

fn scoped_model_field(
    file_cfg: &FileConfig,
    provider: &Provider,
    default_provider: &Provider,
    field: &str,
) -> Option<String> {
    let mut keys = Vec::new();
    if provider == default_provider {
        keys.push(format!("model.{field}"));
        keys.push(field.to_string());
    }
    for id in provider_config_ids(provider) {
        keys.push(format!("providers.{id}.{field}"));
        keys.push(format!("{id}.{field}"));
    }
    file_cfg.string_owned(&keys)
}

fn scoped_cap_field(
    file_cfg: &FileConfig,
    provider: &Provider,
    default_provider: &Provider,
    model: &str,
    fields: &[&str],
) -> Option<String> {
    let model_id = norm_key(model);
    let mut keys = Vec::new();
    for id in provider_config_ids(provider) {
        for field in fields {
            keys.push(format!(
                "providers.{id}.models.{model_id}.capabilities.{field}"
            ));
            keys.push(format!("providers.{id}.models.{model_id}.caps.{field}"));
            keys.push(format!("providers.{id}.models.{model_id}.{field}"));
            keys.push(format!("{id}.models.{model_id}.capabilities.{field}"));
            keys.push(format!("{id}.models.{model_id}.caps.{field}"));
            keys.push(format!("{id}.models.{model_id}.{field}"));
        }
    }
    if provider == default_provider {
        for field in fields {
            keys.push(format!("model.capabilities.{field}"));
            keys.push(format!("model.caps.{field}"));
        }
    }
    for id in provider_config_ids(provider) {
        for field in fields {
            keys.push(format!("providers.{id}.capabilities.{field}"));
            keys.push(format!("providers.{id}.caps.{field}"));
            keys.push(format!("{id}.capabilities.{field}"));
            keys.push(format!("{id}.caps.{field}"));
        }
    }
    file_cfg.string_owned(&keys)
}

fn parse_scoped_cap_bool(
    file_cfg: &FileConfig,
    provider: &Provider,
    default_provider: &Provider,
    model: &str,
    fields: &[&str],
    label: &str,
) -> Result<Option<bool>, String> {
    scoped_cap_field(file_cfg, provider, default_provider, model, fields)
        .map(|raw| parse_bool(&raw).ok_or_else(|| format!("invalid {label} value `{raw}`")))
        .transpose()
}

fn parse_scoped_cap_u32(
    file_cfg: &FileConfig,
    provider: &Provider,
    default_provider: &Provider,
    model: &str,
    fields: &[&str],
    label: &str,
) -> Result<Option<u32>, String> {
    scoped_cap_field(file_cfg, provider, default_provider, model, fields)
        .map(|raw| {
            raw.parse::<u32>()
                .map_err(|_| format!("invalid {label} value `{raw}`"))
        })
        .transpose()
}

fn parse_scoped_cap_thinking(
    file_cfg: &FileConfig,
    provider: &Provider,
    default_provider: &Provider,
    model: &str,
    fields: &[&str],
    label: &str,
) -> Result<Option<ThinkingSupport>, String> {
    scoped_cap_field(file_cfg, provider, default_provider, model, fields)
        .map(|raw| {
            parse_thinking_support(&raw).ok_or_else(|| format!("invalid {label} value `{raw}`"))
        })
        .transpose()
}

fn parse_thinking_support(raw: &str) -> Option<ThinkingSupport> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "0" | "false" | "disabled" => Some(ThinkingSupport::None),
        "supported" | "basic" | "no_replay" | "no-replay" | "noreplay" | "openai" => {
            Some(ThinkingSupport::NoReplay)
        }
        "replay" | "full" | "full_replay" | "full-replay" | "fullreplay" | "on" | "1" | "true" => {
            Some(ThinkingSupport::FullReplay)
        }
        _ => None,
    }
}

fn parse_thinking(raw: Option<&str>) -> Result<(ThinkingModeCfg, Option<EffortCfg>), String> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off") | Some("0") | Some("disabled") | Some("false") | Some("none") => {
            Ok((ThinkingModeCfg::Off, None))
        }
        Some("auto") => Ok((ThinkingModeCfg::Auto, None)),
        Some("high") | Some("1") | Some("true") | Some("on") | Some("enabled") | Some("")
        | None => Ok((ThinkingModeCfg::Enabled, Some(EffortCfg::High))),
        Some("minimal") => Ok((ThinkingModeCfg::Enabled, Some(EffortCfg::Minimal))),
        Some("low") => Ok((ThinkingModeCfg::Enabled, Some(EffortCfg::Low))),
        Some("medium") => Ok((ThinkingModeCfg::Enabled, Some(EffortCfg::Medium))),
        Some("max") | Some("xhigh") => Ok((ThinkingModeCfg::Enabled, Some(EffortCfg::Max))),
        Some(other) => Err(format!("unknown thinking value: {other}")),
    }
}

fn default_store_root() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".muagent")
        .join("sessions")
}

fn env_raw(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn env_string(name: &str) -> Option<String> {
    env_raw(name).filter(|s| !s.trim().is_empty())
}

fn env_list(name: &str) -> Option<Vec<String>> {
    env_raw(name).map(|s| split_list(&s))
}

fn parse_env_bool(name: &str) -> Result<Option<bool>, String> {
    env_string(name)
        .map(|s| parse_bool(&s).ok_or_else(|| format!("invalid {name} value `{s}`")))
        .transpose()
}

fn parse_env_u32(name: &str) -> Result<Option<u32>, String> {
    env_string(name)
        .map(|s| {
            s.parse::<u32>()
                .map_err(|_| format!("invalid {name} value `{s}`"))
        })
        .transpose()
}

fn parse_env_usize(name: &str) -> Result<Option<usize>, String> {
    env_string(name)
        .map(|s| {
            s.parse::<usize>()
                .map_err(|_| format!("invalid {name} value `{s}`"))
        })
        .transpose()
}

fn parse_env_f32(name: &str) -> Result<Option<f32>, String> {
    env_string(name)
        .map(|s| {
            s.parse::<f32>()
                .map_err(|_| format!("invalid {name} value `{s}`"))
        })
        .transpose()
}

fn parse_file_bool(cfg: &FileConfig, keys: &[&str], label: &str) -> Result<Option<bool>, String> {
    cfg.string(keys)
        .map(|s| parse_bool(&s).ok_or_else(|| format!("invalid {label} value `{s}`")))
        .transpose()
}

fn parse_file_u32(cfg: &FileConfig, keys: &[&str], label: &str) -> Result<Option<u32>, String> {
    cfg.string(keys)
        .map(|s| {
            s.parse::<u32>()
                .map_err(|_| format!("invalid {label} value `{s}`"))
        })
        .transpose()
}

fn parse_file_usize(cfg: &FileConfig, keys: &[&str], label: &str) -> Result<Option<usize>, String> {
    cfg.string(keys)
        .map(|s| {
            s.parse::<usize>()
                .map_err(|_| format!("invalid {label} value `{s}`"))
        })
        .transpose()
}

fn parse_file_f32(cfg: &FileConfig, keys: &[&str], label: &str) -> Result<Option<f32>, String> {
    cfg.string(keys)
        .map(|s| {
            s.parse::<f32>()
                .map_err(|_| format!("invalid {label} value `{s}`"))
        })
        .transpose()
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn expand_home(raw: impl AsRef<str>) -> PathBuf {
    let raw = raw.as_ref();
    if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().unwrap_or_else(|| PathBuf::from(".")).join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" | "enabled" | "auto" => Some(true),
        "off" | "0" | "false" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::file::parse_config_text;
    use super::{
        AgentInstructionConfig, CapabilityConfig, CompactionConfig, Config, ConfigOverrides,
        EffortCfg, FileConfig, FsConfig, ModelConfig, Provider, RuntimeConfig, StoreConfig,
        ThinkingModeCfg,
    };
    use crate::core::thinking::ThinkingSupport;

    #[test]
    fn parses_model_config() {
        let cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"
            model = "openai/gpt-5.4-nano"
            api_key = "sk-or-test"
            [tools]
            disabled = ["sh_exec"]
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.string(&["model.provider"]).as_deref(),
            Some("openrouter")
        );
        assert_eq!(
            cfg.string(&["model.api_key"]).as_deref(),
            Some("sk-or-test")
        );
        assert_eq!(
            cfg.list(&["tools.disabled"]).unwrap(),
            vec!["sh_exec".to_string()]
        );
    }

    #[test]
    fn real_toml_parser_handles_dotted_provider_tables() {
        let cfg = parse_config_text(
            r#"
            [providers.openrouter]
            model = "openai/gpt-5.4-nano"
            api_key_env = "OPENROUTER_API_KEY"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.string(&["providers.openrouter.model"]).as_deref(),
            Some("openai/gpt-5.4-nano")
        );
        assert_eq!(
            cfg.string(&["providers.openrouter.api_key_env"]).as_deref(),
            Some("OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn empty_arrays_are_explicit_values() {
        let cfg = parse_config_text(
            r#"
            [tools]
            enabled = []
            "#,
        )
        .unwrap();
        assert_eq!(cfg.list(&["tools.enabled"]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn later_config_overrides_earlier_config() {
        let mut cfg = parse_config_text("model = \"a\"").unwrap();
        let later = parse_config_text("model = \"b\"").unwrap();
        cfg.merge(later);
        assert_eq!(cfg.string(&["model"]).as_deref(), Some("b"));
    }

    #[test]
    fn missing_config_keys_are_empty() {
        let cfg = FileConfig::default();
        assert!(cfg.string(&["model.api_key"]).is_none());
    }

    #[test]
    fn cli_overrides_win_over_files() {
        let mut file_cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"
            model = "anthropic/claude-haiku-4.5"
            "#,
        )
        .unwrap();
        file_cfg.merge(FileConfig::default());
        let overrides = ConfigOverrides {
            model: Some("openai/gpt-5.4-nano".into()),
            ..Default::default()
        };
        let model = super::ModelConfig::from_sources(&file_cfg, &overrides).unwrap();
        assert_eq!(model.model, "openai/gpt-5.4-nano");
    }

    #[test]
    fn openrouter_key_can_use_any_cli_model() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"
            model = "openai/gpt-5.4-nano"

            [providers.openrouter]
            api_key = "sk-or-test"
            "#,
        )
        .unwrap();
        let overrides = ConfigOverrides {
            model: Some("anthropic/claude-haiku-4.5".into()),
            ..Default::default()
        };
        let model = super::ModelConfig::from_sources(&file_cfg, &overrides).unwrap();
        assert_eq!(model.provider, Provider::OpenRouter);
        assert_eq!(model.model, "anthropic/claude-haiku-4.5");
        assert_eq!(model.api_key.as_deref(), Some("sk-or-test"));
    }

    #[test]
    fn provider_specific_model_is_used_when_provider_changes() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"
            model = "openai/gpt-5.4-nano"
            api_key = "sk-or-test"

            [providers.google]
            model = "gemini-3.1-flash-lite-preview"
            api_key = "gemini-test"
            "#,
        )
        .unwrap();
        let overrides = ConfigOverrides {
            provider: Some("google".into()),
            ..Default::default()
        };
        let model = super::ModelConfig::from_sources(&file_cfg, &overrides).unwrap();
        assert_eq!(model.provider, Provider::Google);
        assert_eq!(model.model, "gemini-3.1-flash-lite-preview");
        assert_eq!(model.api_key.as_deref(), Some("gemini-test"));
    }

    #[test]
    fn codex_provider_uses_subscription_defaults() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "codex"

            "#,
        )
        .unwrap();
        let model =
            super::ModelConfig::from_sources(&file_cfg, &ConfigOverrides::default()).unwrap();
        assert_eq!(model.provider, Provider::Codex);
        assert_eq!(model.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(model.model, "gpt-5.5");
    }

    #[test]
    fn codex_provider_reads_canonical_table_before_legacy_alias() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "codex"

            [providers.codex]
            model = "gpt-5.5"

            [providers.openai_codex]
            model = "legacy-model"
            "#,
        )
        .unwrap();
        let model =
            super::ModelConfig::from_sources(&file_cfg, &ConfigOverrides::default()).unwrap();
        assert_eq!(model.provider, Provider::Codex);
        assert_eq!(model.model, "gpt-5.5");
    }

    #[test]
    fn codex_provider_still_reads_legacy_table() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "codex"

            [providers.openai_codex]
            model = "legacy-model"
            "#,
        )
        .unwrap();
        let model =
            super::ModelConfig::from_sources(&file_cfg, &ConfigOverrides::default()).unwrap();
        assert_eq!(model.provider, Provider::Codex);
        assert_eq!(model.model, "legacy-model");
    }

    #[test]
    fn model_capability_overrides_are_provider_and_model_scoped() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"

            [providers.openrouter]
            model = "moonshotai/kimi-k2.6"

            [providers.openrouter.capabilities]
            vision = true
            ctx_len = 128000

            [providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
            vision = false
            ctx_len = 262144
            thinking = "none"

            [providers.openrouter.models."openai/gpt-5.4-nano".capabilities]
            vision = true
            ctx_len = 400000
            reasoning = "supported"

            [providers.openai.capabilities]
            images = true
            context_window = 128000
            reasoning = "supported"
            "#,
        )
        .unwrap();

        let openrouter =
            super::ModelConfig::from_sources(&file_cfg, &ConfigOverrides::default()).unwrap();
        assert_eq!(openrouter.capabilities.vision, Some(false));
        assert_eq!(openrouter.capabilities.ctx_len, Some(262_144));
        assert_eq!(
            openrouter.capabilities.thinking,
            Some(ThinkingSupport::None)
        );

        let openrouter_openai_route = super::ModelConfig::from_sources(
            &file_cfg,
            &ConfigOverrides {
                model: Some("openai/gpt-5.4-nano".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(openrouter_openai_route.capabilities.vision, Some(true));
        assert_eq!(openrouter_openai_route.capabilities.ctx_len, Some(400_000));
        assert_eq!(
            openrouter_openai_route.capabilities.thinking,
            Some(ThinkingSupport::NoReplay)
        );

        let openrouter_other_route = super::ModelConfig::from_sources(
            &file_cfg,
            &ConfigOverrides {
                model: Some("qwen/qwen3-coder".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(openrouter_other_route.capabilities.vision, Some(true));
        assert_eq!(openrouter_other_route.capabilities.ctx_len, Some(128_000));

        let openai = super::ModelConfig::from_sources(
            &file_cfg,
            &ConfigOverrides {
                provider: Some("openai".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(openai.capabilities.vision, Some(true));
        assert_eq!(openai.capabilities.ctx_len, Some(128_000));
        assert_eq!(
            openai.capabilities.thinking,
            Some(ThinkingSupport::NoReplay)
        );
    }

    #[test]
    fn default_model_scope_does_not_leak_to_other_provider() {
        let file_cfg = parse_config_text(
            r#"
            [model]
            provider = "openrouter"
            model = "openai/gpt-5.4-nano"
            api_key = "sk-or-test"
            "#,
        )
        .unwrap();
        let overrides = ConfigOverrides {
            provider: Some("google".into()),
            ..Default::default()
        };
        let model = super::ModelConfig::from_sources(&file_cfg, &overrides).unwrap();
        assert_eq!(model.provider, Provider::Google);
        assert_eq!(model.model, "gemini-3.1-flash-lite-preview");
        assert!(model.api_key.is_none());
    }

    #[test]
    fn config_validation_rejects_bad_compaction() {
        let mut cfg = valid_config_for_test();
        cfg.compaction.max_tokens = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("compaction.max_tokens"));

        cfg.compaction.max_tokens = 10;
        cfg.validate().unwrap();
    }

    #[test]
    fn thinking_default_is_enabled_high() {
        assert_eq!(
            super::parse_thinking(None).unwrap(),
            (ThinkingModeCfg::Enabled, Some(EffortCfg::High))
        );
    }

    #[test]
    fn thinking_true_and_on_enable_high_effort() {
        for raw in ["true", "1", "on", "enabled", "high"] {
            assert_eq!(
                super::parse_thinking(Some(raw)).unwrap(),
                (ThinkingModeCfg::Enabled, Some(EffortCfg::High)),
                "raw={raw}"
            );
        }
    }

    #[test]
    fn thinking_xhigh_aliases_max_effort() {
        for raw in ["max", "xhigh"] {
            assert_eq!(
                super::parse_thinking(Some(raw)).unwrap(),
                (ThinkingModeCfg::Enabled, Some(EffortCfg::Max)),
                "raw={raw}"
            );
        }
    }

    #[test]
    fn thinking_auto_is_still_explicit() {
        assert_eq!(
            super::parse_thinking(Some("auto")).unwrap(),
            (ThinkingModeCfg::Auto, None)
        );
    }

    #[test]
    fn runtime_config_defaults_to_high_thinking() {
        let runtime =
            super::RuntimeConfig::from_sources(&FileConfig::default(), &ConfigOverrides::default())
                .unwrap();
        assert_eq!(runtime.thinking_mode, ThinkingModeCfg::Enabled);
        assert_eq!(runtime.thinking_effort, Some(EffortCfg::High));
    }

    #[test]
    fn agent_md_overrides_win_over_file_config() {
        let file_cfg = parse_config_text(
            r#"
            [agent_md]
            enabled = true
            max_bytes = 4096
            "#,
        )
        .unwrap();
        let cfg = super::AgentInstructionConfig::from_sources(
            &file_cfg,
            &ConfigOverrides {
                agent_md: Some(false),
                agent_md_max_bytes: Some(128),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_bytes_per_file, 128);
    }

    fn valid_config_for_test() -> Config {
        Config {
            model: ModelConfig {
                provider: Provider::OpenRouter,
                base_url: "https://openrouter.ai/api/v1".into(),
                model: "openai/gpt-5.4-nano".into(),
                api_key: None,
                capabilities: super::ModelCapabilityOverrides::default(),
            },
            fs: FsConfig { root: ".".into() },
            compaction: CompactionConfig {
                max_tokens: 156_000,
                threshold_ratio: 0.8,
                keep_tail_turns: 4,
                keep_recent_tokens: 20_000,
                root_task_pin_max_tokens: 1_024,
                summary_input_max_tokens: 100_000,
                summary_output_max_tokens: 8_000,
                restart_repair_window_tokens: 300_000,
                max_summary_rounds: 4,
                summarizer: None,
            },
            capabilities: CapabilityConfig {
                tool_allowlist: None,
                tool_denylist: Vec::new(),
                skill_allowlist: None,
                skill_denylist: Vec::new(),
                skill_autoload: true,
            },
            mcp: super::McpConfig {
                sse_endpoints: Vec::new(),
            },
            runtime: RuntimeConfig {
                cache_auto: true,
                thinking_mode: ThinkingModeCfg::Enabled,
                thinking_effort: Some(EffortCfg::High),
            },
            agent_instructions: AgentInstructionConfig {
                enabled: true,
                max_bytes_per_file: 64 * 1024,
            },
            subagents: super::SubagentConfig {
                enabled: true,
                definitions: Vec::new(),
            },
            store: StoreConfig::Memory,
        }
    }
}
