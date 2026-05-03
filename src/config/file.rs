//! TOML config loading: file lookup, key flattening, and known-key validation.
//! Keeps the heavy parsing/key-recognition logic out of `mod.rs`, which
//! focuses on the typed `Config` shape and per-section overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{expand_home, env_string, home_dir, split_list};

#[derive(Clone, Debug, Default)]
pub(super) struct FileConfig {
    values: BTreeMap<String, ConfigValue>,
}

#[derive(Clone, Debug)]
pub(super) enum ConfigValue {
    String(String),
    Bool(bool),
    Integer(i64),
    Float(f64),
    List(Vec<String>),
}

impl FileConfig {
    pub(super) fn load(explicit_config: Option<&str>) -> Result<Self, String> {
        let explicit = explicit_config
            .map(ToOwned::to_owned)
            .or_else(|| env_string("MUAGENT_CONFIG"));
        let paths = config_paths(explicit.as_deref());
        let require_existing = explicit.is_some();

        let mut out = FileConfig::default();
        for path in paths {
            if path.is_file() {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("read config {}: {e}", path.display()))?;
                out.merge(
                    parse_config_text(&text)
                        .map_err(|e| format!("parse config {}: {e}", path.display()))?,
                );
            } else if require_existing {
                return Err(format!("config file not found: {}", path.display()));
            }
        }
        Ok(out)
    }

    pub(super) fn merge(&mut self, other: FileConfig) {
        self.values.extend(other.values);
    }

    pub(super) fn string(&self, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|key| self.values.get(&norm_key(key)).and_then(value_to_string))
    }

    pub(super) fn string_owned(&self, keys: &[String]) -> Option<String> {
        keys.iter()
            .find_map(|key| self.values.get(&norm_key(key)).and_then(value_to_string))
    }

    pub(super) fn list(&self, keys: &[&str]) -> Option<Vec<String>> {
        keys.iter().find_map(|key| {
            self.values
                .get(&norm_key(key))
                .and_then(|value| match value {
                    ConfigValue::List(xs) => Some(xs.clone()),
                    ConfigValue::String(s) => Some(split_list(s)),
                    _ => None,
                })
        })
    }

    pub(super) fn warn_unknown_keys(&self) {
        for key in self.values.keys().filter(|key| !is_known_config_key(key)) {
            tracing::warn!(key = %key, "unknown config key ignored");
        }
    }
}

fn value_to_string(value: &ConfigValue) -> Option<String> {
    match value {
        ConfigValue::String(s) => Some(s.clone()),
        ConfigValue::Bool(b) => Some(b.to_string()),
        ConfigValue::Integer(i) => Some(i.to_string()),
        ConfigValue::Float(f) => Some(f.to_string()),
        ConfigValue::List(_) => None,
    }
}

fn config_paths(explicit: Option<&str>) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![expand_home(path)];
    }

    let mut paths = Vec::new();
    if let Some(home) = home_dir() {
        paths.push(home.join(".muagent").join("config.toml"));
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut ancestors = cwd.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
    ancestors.reverse();
    for dir in ancestors {
        paths.push(dir.join(".muagent").join("config.toml"));
    }
    paths
}

pub(super) fn parse_config_text(text: &str) -> Result<FileConfig, String> {
    let root: toml::Value = text.parse::<toml::Value>().map_err(|e| e.to_string())?;
    let mut out = FileConfig::default();
    flatten_toml(None, &root, &mut out)?;
    Ok(out)
}

fn flatten_toml(
    prefix: Option<&str>,
    value: &toml::Value,
    out: &mut FileConfig,
) -> Result<(), String> {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key = norm_key(key);
                let full_key = match prefix {
                    Some(prefix) if !prefix.is_empty() => format!("{prefix}.{key}"),
                    _ => key,
                };
                flatten_toml(Some(&full_key), value, out)?;
            }
        }
        _ => {
            let key = prefix.ok_or_else(|| "config root must be a table".to_string())?;
            out.values.insert(key.to_string(), config_value(value)?);
        }
    }
    Ok(())
}

fn config_value(value: &toml::Value) -> Result<ConfigValue, String> {
    match value {
        toml::Value::String(s) => Ok(ConfigValue::String(s.clone())),
        toml::Value::Integer(i) => Ok(ConfigValue::Integer(*i)),
        toml::Value::Float(f) => Ok(ConfigValue::Float(*f)),
        toml::Value::Boolean(b) => Ok(ConfigValue::Bool(*b)),
        toml::Value::Array(xs) => xs
            .iter()
            .map(toml_scalar_to_string)
            .collect::<Result<Vec<_>, _>>()
            .map(ConfigValue::List),
        toml::Value::Datetime(dt) => Ok(ConfigValue::String(dt.to_string())),
        toml::Value::Table(_) => unreachable!("tables are flattened before conversion"),
    }
}

fn toml_scalar_to_string(value: &toml::Value) -> Result<String, String> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Datetime(dt) => Ok(dt.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            Err("arrays must contain scalar values only".into())
        }
    }
}

pub(super) fn norm_key(key: &str) -> String {
    key.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace('-', "_")
        .to_ascii_lowercase()
}

fn is_known_config_key(key: &str) -> bool {
    if is_model_capability_key(key) {
        return true;
    }
    if is_provider_model_key(key) {
        return true;
    }
    if is_provider_specific_key(key) {
        return true;
    }
    matches!(
        key,
        "provider"
            | "model"
            | "base_url"
            | "api_key"
            | "api_key_env"
            | "model.provider"
            | "model.model"
            | "model.base_url"
            | "model.api_key"
            | "model.api_key_env"
            | "openai.model"
            | "openai.base_url"
            | "openai.api_key"
            | "openai.api_key_env"
            | "openai_codex.model"
            | "openai_codex.base_url"
            | "openai_codex.api_key"
            | "openai_codex.api_key_env"
            | "anthropic.model"
            | "anthropic.base_url"
            | "anthropic.api_key"
            | "anthropic.api_key_env"
            | "google.model"
            | "google.base_url"
            | "google.api_key"
            | "google.api_key_env"
            | "openrouter.model"
            | "openrouter.base_url"
            | "openrouter.api_key"
            | "openrouter.api_key_env"
            | "store"
            | "store.path"
            | "storage.path"
            | "root"
            | "fs_root"
            | "fs.root"
            | "max_tokens"
            | "keep_tail_turns"
            | "compaction.max_tokens"
            | "compaction.threshold"
            | "compaction.threshold_ratio"
            | "compaction.keep_tail_turns"
            | "compaction.keep_recent_tokens"
            | "compaction.root_task_pin_max_tokens"
            | "compaction.summary_input_max_tokens"
            | "compaction.summary_output_max_tokens"
            | "compaction.restart_repair_window_tokens"
            | "compaction.max_summary_rounds"
            | "tools"
            | "tools.enabled"
            | "tools.disabled"
            | "tools.disable"
            | "disabled_tools"
            | "skills"
            | "skills.enabled"
            | "skills.disabled"
            | "skills.disable"
            | "disabled_skills"
            | "skill_autoload"
            | "capabilities.tools"
            | "capabilities.disabled_tools"
            | "capabilities.disable_tools"
            | "capabilities.skills"
            | "capabilities.disabled_skills"
            | "capabilities.disable_skills"
            | "capabilities.skill_autoload"
            | "net_http"
            | "net_http.enabled"
            | "http.enabled"
            | "mcp.sse"
            | "mcp.sse_endpoints"
            | "mcp_sse"
            | "cache"
            | "thinking"
            | "runtime.cache"
            | "runtime.thinking"
            | "agent.md"
            | "agent_md"
            | "agent_md.enabled"
            | "agent_md.max_bytes"
            | "agent_md.max_bytes_per_file"
    )
}

fn is_model_capability_key(key: &str) -> bool {
    let mut parts = key.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (
            Some("model"),
            Some("capabilities" | "caps"),
            Some(field),
            None
        ) if is_capability_field(field)
    )
}

fn is_provider_specific_key(key: &str) -> bool {
    let mut parts = key.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    let provider = if first == "providers" {
        let Some(provider) = parts.next() else {
            return false;
        };
        provider
    } else {
        first
    };
    if !matches!(
        provider,
        "openai" | "openai_codex" | "anthropic" | "google" | "openrouter"
    ) {
        return false;
    }
    match (parts.next(), parts.next(), parts.next()) {
        (Some("model" | "base_url" | "api_key" | "api_key_env"), None, None) => true,
        (Some("capabilities" | "caps"), Some(field), None) => is_capability_field(field),
        _ => false,
    }
}

fn is_provider_model_key(key: &str) -> bool {
    const PROVIDERS: [&str; 5] = [
        "openai",
        "openai_codex",
        "anthropic",
        "google",
        "openrouter",
    ];
    for provider in PROVIDERS {
        for prefix in [
            format!("providers.{provider}.models."),
            format!("{provider}.models."),
        ] {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            if let Some((model_id, field)) = rest.rsplit_once(".capabilities.") {
                return !model_id.is_empty() && is_capability_field(field);
            }
            if let Some((model_id, field)) = rest.rsplit_once(".caps.") {
                return !model_id.is_empty() && is_capability_field(field);
            }
            if let Some((model_id, field)) = rest.rsplit_once('.') {
                return !model_id.is_empty() && is_capability_field(field);
            }
        }
    }
    false
}

fn is_capability_field(field: &str) -> bool {
    matches!(
        field,
        "native_tool_use"
            | "tool_use"
            | "tool_calling"
            | "json_schema_mode"
            | "json_schema"
            | "vision"
            | "image"
            | "images"
            | "streaming"
            | "stream"
            | "ctx_len"
            | "context_window"
            | "context_length"
            | "max_context_tokens"
            | "prompt_cache"
            | "cache"
            | "thinking"
            | "reasoning"
    )
}
