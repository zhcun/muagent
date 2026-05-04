//! CLI front-end runtime — REPL/TUI loops, slash commands, session picker,
//! event rendering, and other driver glue. Lives in the library so it can be
//! unit-tested and reused; `src/bin/muagent.rs` only does argv → dispatch.

pub mod commands;
pub mod doctor;
pub mod driver;
pub mod event_render;
pub mod image;
pub mod repl;
pub mod sessions;
pub mod sink;
pub mod state;
#[cfg(feature = "tui")]
pub mod tui_driver;
#[cfg(feature = "tui")]
pub mod tui_helpers;

use std::path::PathBuf;

use crate::config::{Config, ConfigOverrides, StoreConfig};
use crate::core::clock::{Clock, SystemClock};
use crate::setup;

/// Mutable runtime context shared by REPL/TUI command handlers. Holds the
/// wired runner plus the overrides that produced the current `Config`, so
/// `/model` and `/provider` can rebuild the runner without re-parsing argv.
pub struct ReplRuntime {
    pub wired: setup::Wired,
    pub cfg: Config,
    pub overrides: ConfigOverrides,
}

pub const DEFAULT_MAX_STEPS: usize = 10_000;

#[cfg(feature = "tui")]
pub const TUI_MAX_QUEUED_SUBMISSIONS: usize = 3;

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

pub fn now_ms() -> i64 {
    SystemClock.now_ms()
}

pub fn short_uuid(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn stdio_is_tty() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::isatty(libc::STDIN_FILENO) != 0 && libc::isatty(libc::STDOUT_FILENO) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

pub fn store_label(cfg: &Config) -> String {
    match &cfg.store {
        StoreConfig::Memory => "memory".to_string(),
        StoreConfig::Jsonl(p) => format!("jsonl:{}", p.display()),
    }
}

pub fn content_text(c: &crate::core::types::Content) -> String {
    use crate::core::types::{Content, ContentPart};
    match c {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub fn brief_msg(m: &crate::core::types::Message) -> String {
    use crate::core::types::Message;
    match m {
        Message::User { content } => format!("user: {}", truncate(&content_text(content), 140)),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let t = content_text(content);
            if tool_calls.is_empty() {
                format!("assistant: {}", truncate(&t, 140))
            } else {
                let names: Vec<String> = tool_calls.iter().map(|c| c.tool_name.clone()).collect();
                format!("assistant+tools{:?}: {}", names, truncate(&t, 100))
            }
        }
        Message::ToolResult { result, .. } => format!(
            "tool_result ok={}: {}",
            result.ok,
            truncate(&result.text(), 140)
        ),
        Message::System { content } => format!("system: {}", truncate(&content_text(content), 140)),
        Message::Observation { kind, text } => format!("obs {kind:?}: {}", truncate(text, 140)),
    }
}

pub fn print_banner(cfg: &Config) {
    println!(
        "μAgent v{} — provider={} model={}",
        env!("CARGO_PKG_VERSION"),
        cfg.model.provider.cli_name(),
        cfg.model.model
    );
    let store = store_label(cfg);
    println!(
        "  store={}  fs_root={}  sh=enabled",
        store,
        cfg.fs.root.display()
    );
    let thinking_label = match (cfg.runtime.thinking_mode, cfg.runtime.thinking_effort) {
        (crate::config::ThinkingModeCfg::Off, _) => "off".to_string(),
        (crate::config::ThinkingModeCfg::Auto, _) => "auto".to_string(),
        (crate::config::ThinkingModeCfg::Enabled, Some(e)) => format!("{e:?}").to_lowercase(),
        (crate::config::ThinkingModeCfg::Enabled, None) => "enabled".into(),
    };
    println!(
        "  max_tokens={} threshold={:.2} keep_tail={}  cache={}  thinking={}",
        cfg.compaction.max_tokens,
        cfg.compaction.threshold_ratio,
        cfg.compaction.keep_tail_turns,
        if cfg.runtime.cache_auto {
            "auto"
        } else {
            "disabled"
        },
        thinking_label
    );
    let tools = cfg
        .capabilities
        .tool_allowlist
        .as_ref()
        .map(|x| format!("{} tools", x.len()))
        .unwrap_or_else(|| "all".into());
    let tools = if cfg.capabilities.tool_denylist.is_empty() {
        tools
    } else {
        format!("{tools}, -{}", cfg.capabilities.tool_denylist.len())
    };
    let skills = cfg
        .capabilities
        .skill_allowlist
        .as_ref()
        .map(|x| format!("{} skills", x.len()))
        .unwrap_or_else(|| "all".into());
    let skills = if cfg.capabilities.skill_denylist.is_empty() {
        skills
    } else {
        format!("{skills}, -{}", cfg.capabilities.skill_denylist.len())
    };
    let autoload = if cfg.capabilities.skill_autoload {
        "on"
    } else {
        "off"
    };
    println!(
        "  tools={}  skills={}  skill_autoload={}  agent_md={}",
        tools,
        skills,
        autoload,
        if cfg.agent_instructions.enabled {
            "on"
        } else {
            "off"
        }
    );
    println!("Type /help for commands. Ctrl-D to quit.\n");
}
