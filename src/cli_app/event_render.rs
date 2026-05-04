//! Translate `Event`s into chat-bound `TuiUpdate`s and one-line tool labels.
//! Keeps the presentation logic out of `core` (which only carries raw tool
//! name + args JSON).

use crate::cli_app::driver::TuiUpdate;
use crate::cli_app::truncate;
use crate::core::event::Event;

pub fn event_tui_updates(ev: &Event) -> Vec<TuiUpdate> {
    match ev {
        // ToolCallStart / End are paired into a single TuiUpdate::Tool by
        // the calling loop in `drive_until_terminal_with_updates`, since
        // the chat row needs args (from Start) plus ok/brief (from End).
        Event::ToolCallStart { .. } | Event::ToolCallEnd { .. } => vec![],
        Event::StepAdvanced { to, .. } => vec![TuiUpdate::Activity(format!("advanced: {to}"))],
        Event::AssistantMessage { text, .. } => {
            // Stream every assistant turn into the chat panel as it arrives
            // so the user sees intermediate reasoning instead of waiting until
            // the run finishes. Empty-text turns (model called tools without
            // speaking) only get an activity hint to avoid blank chat lines.
            let trimmed = text.trim();
            if trimmed.is_empty() {
                vec![TuiUpdate::Activity("assistant: (tool call only)".into())]
            } else {
                vec![
                    TuiUpdate::Assistant(text.clone()),
                    TuiUpdate::Activity(format!("assistant: {}", truncate(trimmed, 120))),
                ]
            }
        }
        Event::HistoryCompacted {
            replaced_turns,
            saved_tokens_estimate,
            ..
        } => vec![TuiUpdate::Activity(format!(
            "compacted {replaced_turns} turns, saved ~{saved_tokens_estimate} tokens"
        ))],
        Event::Paused { reason, .. } => vec![TuiUpdate::Activity(format!("paused: {reason}"))],
        Event::ErrorRaised { brief, .. } => {
            vec![TuiUpdate::Activity(format!(
                "error: {}",
                truncate(brief, 120)
            ))]
        }
        Event::SessionEnd { ok, .. } => vec![TuiUpdate::Activity(if *ok {
            "session ended: ok".into()
        } else {
            "session ended: failed".into()
        })],
        Event::SessionStart { .. } | Event::UserMessage { .. } | Event::AssistantDelta { .. } => {
            vec![]
        }
        Event::ToolIntentRecovered { .. } => {
            vec![TuiUpdate::Activity("tool intent recovered".into())]
        }
    }
}

/// Render a tool invocation as a one-line chat label, e.g. `Bash(sleep 10)`,
/// `Read(src/foo.rs)`, `Update(src/tui.rs)`.
pub fn tool_display_label(tool: &str, args: &serde_json::Value) -> String {
    fn s(v: &serde_json::Value, key: &str) -> String {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    }
    fn one_line(s: &str, max: usize) -> String {
        let compact = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.chars().count() <= max {
            compact
        } else {
            let keep = max.saturating_sub(1);
            format!("{}…", compact.chars().take(keep).collect::<String>())
        }
    }
    let (display_name, body) = match tool {
        "sh_exec" => {
            let cmd = {
                let c = s(args, "cmd");
                if c.is_empty() {
                    s(args, "command")
                } else {
                    c
                }
            };
            ("Bash", one_line(&cmd, 80))
        }
        "fs_read" => ("Read", s(args, "path")),
        "fs_write" => ("Write", s(args, "path")),
        "fs_edit" => ("Update", s(args, "path")),
        "fs_list" => ("List", s(args, "path")),
        "fs_stat" => ("Stat", s(args, "path")),
        "fs_delete" => ("Delete", s(args, "path")),
        "fs_rename" => ("Rename", format!("{} → {}", s(args, "from"), s(args, "to"))),
        other => {
            let fallback = if let serde_json::Value::Object(map) = args {
                map.values()
                    .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
                    .map(|s| one_line(s, 80))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return if fallback.is_empty() {
                other.to_string()
            } else {
                format!("{other}({fallback})")
            };
        }
    };
    format!("{display_name}({body})")
}

/// Render a one-line "extra" string from a tool's structured `detail` for
/// inline display under the chat row (the `⎿ ...` continuation). Returns
/// `None` for tools without a meaningful summary so the chat row stays a
/// single line.
pub fn tool_result_extra_line(tool: &str, detail: &serde_json::Value) -> Option<String> {
    fn n(v: &serde_json::Value, key: &str) -> Option<i64> {
        v.get(key).and_then(serde_json::Value::as_i64)
    }
    match tool {
        "fs_edit" | "fs_write" => {
            let added = n(detail, "lines_added");
            let removed = n(detail, "lines_removed");
            match (added, removed) {
                (Some(a), Some(r)) if a + r > 0 => Some(format!("+{a} -{r}")),
                (Some(a), None) if a > 0 => Some(format!("+{a}")),
                _ => None,
            }
        }
        _ => None,
    }
}
