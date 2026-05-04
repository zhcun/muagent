//! Configuration health report. Used by the CLI `--doctor`-style output and
//! the `/doctor` slash command in REPL/TUI.

use std::path::PathBuf;

use crate::cli_app::{home_dir, store_label};
use crate::config::{Config, Provider};

pub fn config_doctor_report(cfg: &Config) -> String {
    let mut lines = vec![
        "Configuration check".to_string(),
        format!(
            "provider={} model={} base_url={}",
            cfg.model.provider.cli_name(),
            cfg.model.model,
            cfg.model.base_url
        ),
        format!("store={} root={}", store_label(cfg), cfg.fs.root.display()),
    ];
    let hints = model_setup_hints(cfg);
    if hints.is_empty() {
        lines.push("model credentials look configured for this provider.".into());
    } else {
        lines.push("action required:".into());
        for hint in hints {
            lines.push(format!("- {hint}"));
        }
    }
    lines.push("Useful commands: /provider [name] [model_id], /model [model_id], /help".into());
    lines.join("\n")
}

pub fn model_setup_hints(cfg: &Config) -> Vec<String> {
    let has_key = cfg
        .model
        .api_key
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    match cfg.model.provider {
        Provider::OpenRouter if !has_key => vec![format!(
            "OpenRouter is selected but no key is configured. Set OPENROUTER_API_KEY, or add api_key_env = \"OPENROUTER_API_KEY\" under [providers.openrouter]. Current model: {}.",
            cfg.model.model
        )],
        Provider::OpenAi if !has_key && !is_local_base_url(&cfg.model.base_url) => vec![format!(
            "OpenAI is selected but no key is configured. Set OPENAI_API_KEY, or point base_url at a local OpenAI-compatible server. Current base_url: {}.",
            cfg.model.base_url
        )],
        Provider::Codex if !has_key && !codex_oauth_available() => vec![
            "Codex OAuth is selected but no login/token was found. Run `codex login`, or set OPENAI_CODEX_ACCESS_TOKEN plus OPENAI_CODEX_ACCOUNT_ID.".into(),
        ],
        Provider::Anthropic if !has_key => vec![
            "Anthropic is selected but no key is configured. Set ANTHROPIC_API_KEY or MUAGENT_API_KEY.".into(),
        ],
        Provider::Google if !has_key => vec![
            "Google is selected but no key is configured. Set GEMINI_API_KEY or MUAGENT_API_KEY.".into(),
        ],
        _ => Vec::new(),
    }
}

pub fn is_local_base_url(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("[::1]")
        || lower.contains("0.0.0.0")
}

pub fn codex_oauth_available() -> bool {
    env_nonempty("MUAGENT_CODEX_ACCESS_TOKEN")
        || env_nonempty("OPENAI_CODEX_ACCESS_TOKEN")
        || env_nonempty("MUAGENT_CODEX_AUTH_FILE")
        || env_nonempty("MUAGENT_OPENAI_CODEX_AUTH_FILE")
        || codex_default_auth_paths().iter().any(|path| path.is_file())
}

fn env_nonempty(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn codex_default_auth_paths() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".muagent").join("auth.json"),
        home.join(".pi").join("agent").join("auth.json"),
        home.join(".codex").join("auth.json"),
    ]
}
