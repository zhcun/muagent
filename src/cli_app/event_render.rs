//! Translate `Event`s into chat-bound `TuiUpdate`s and one-line tool labels.
//! Keeps the presentation logic out of `core` (which only carries raw tool
//! name + args JSON).

use serde_json::Value;

use crate::cli_app::driver::TuiUpdate;
use crate::cli_app::truncate;
use crate::core::event::Event;

pub fn event_tui_updates(ev: &Event) -> Vec<TuiUpdate> {
    match ev {
        // ToolCallStart / End are paired into a single TuiUpdate::Tool by
        // the calling loop in `drive_until_terminal_with_updates`, since
        // the chat row needs args (from Start) plus ok/brief (from End).
        Event::ToolCallStart { .. } | Event::ToolCallEnd { .. } => vec![],
        Event::StepAdvanced { to, .. } => vec![TuiUpdate::Activity(stage_label(to).into())],
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
        Event::Paused { reason, .. } => {
            if reason == "host_requested" {
                vec![TuiUpdate::Activity("stopped".into())]
            } else {
                vec![TuiUpdate::Activity(format!("paused: {reason}"))]
            }
        }
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
fn one_line(s: &str, max: usize) -> String {
    let compact = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max {
        compact
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", compact.chars().take(keep).collect::<String>())
    }
}

pub fn tool_display_label(tool: &str, args: &serde_json::Value) -> String {
    fn s(v: &Value, key: &str) -> String {
        v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
    }
    let (display_name, body) = match tool {
        "sh_exec" => ("Bash", sh_exec_args_label(args)),
        "fs_read" => ("Read", file_arg_label(args, "uri")),
        "fs_write" => ("Write", write_arg_label(args)),
        "fs_edit" => ("Update", edit_arg_label(args)),
        "fs_list" => ("List", file_arg_label(args, "uri")),
        "fs_stat" => ("Stat", file_arg_label(args, "uri")),
        "fs_delete" => ("Delete", file_arg_label(args, "uri")),
        "fs_rename" => ("Rename", format!("{} → {}", s(args, "from"), s(args, "to"))),
        other => {
            let fallback = generic_args_label(args);
            return if fallback.is_empty() {
                other.to_string()
            } else {
                format!("{other}({fallback})")
            };
        }
    };
    format!("{display_name}({body})")
}

fn stage_label(to: &str) -> &'static str {
    match to {
        "model_turn" => "thinking",
        "tool_batch" => "using tools",
        "tool_intent" => "checking tool result",
        _ => "working",
    }
}

fn file_arg_label(args: &Value, key: &str) -> String {
    one_line(args.get(key).and_then(Value::as_str).unwrap_or(""), 96)
}

fn sh_exec_args_label(args: &Value) -> String {
    if let Some(action) = args.get("action").and_then(Value::as_str) {
        let job_id = args.get("job_id").and_then(Value::as_str).unwrap_or("");
        return one_line(&format!("{action} {job_id}"), 96);
    }

    let bin = args.get("bin").and_then(Value::as_str).unwrap_or("");
    let mut parts = Vec::new();
    if !bin.is_empty() {
        parts.push(bin.to_string());
    }
    if let Some(argv) = args.get("args").and_then(Value::as_array) {
        parts.extend(
            argv.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string),
        );
    }
    let mut label = parts.join(" ");
    if let Some(stdin) = args.get("stdin").and_then(Value::as_str) {
        if !label.is_empty() {
            label.push_str("; ");
        }
        label.push_str(&format!("stdin {} chars", stdin.chars().count()));
    }
    one_line(&label, 96)
}

fn write_arg_label(args: &Value) -> String {
    let uri = file_arg_label(args, "uri");
    let bytes = args
        .get("content")
        .and_then(Value::as_str)
        .map(|s| s.len())
        .unwrap_or(0);
    let mode = if args.get("append").and_then(Value::as_bool) == Some(true) {
        "append"
    } else {
        "write"
    };
    one_line(&format!("{uri}; {mode}; {bytes} bytes"), 96)
}

fn edit_arg_label(args: &Value) -> String {
    let uri = file_arg_label(args, "uri");
    let edits = args
        .get("edits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .or_else(|| (args.get("old_text").is_some() || args.get("new_text").is_some()).then_some(1))
        .unwrap_or(0);
    let mut parts = vec![
        uri,
        format!("{edits} edit{}", if edits == 1 { "" } else { "s" }),
    ];
    if args.get("dry_run").and_then(Value::as_bool) == Some(true) {
        parts.push("dry-run".into());
    }
    if args.get("replace_all").and_then(Value::as_bool) == Some(true) {
        parts.push("replace-all".into());
    }
    one_line(&parts.join("; "), 96)
}

fn generic_args_label(args: &Value) -> String {
    let Value::Object(map) = args else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (key, value) in map {
        let value = match value {
            Value::String(s) if matches!(key.as_str(), "content" | "stdin" | "b64") => {
                format!("{} chars", s.chars().count())
            }
            Value::String(s) => one_line(s, 40),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array(xs) => format!("{} items", xs.len()),
            Value::Object(_) => "{...}".into(),
            Value::Null => continue,
        };
        parts.push(format!("{key}={value}"));
    }
    one_line(&parts.join(", "), 96)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{event_tui_updates, tool_display_label};
    use crate::cli_app::driver::TuiUpdate;
    use crate::core::event::Event;

    #[test]
    fn built_in_tool_labels_show_current_arg_names() {
        assert_eq!(
            tool_display_label(
                "fs_read",
                &json!({"uri":"file:///tmp/project/src/main.rs","max_bytes":1000})
            ),
            "Read(file:///tmp/project/src/main.rs)"
        );
        assert_eq!(
            tool_display_label(
                "sh_exec",
                &json!({"bin":"cargo","args":["test","--lib"],"timeout_ms":30000})
            ),
            "Bash(cargo test --lib)"
        );
    }

    #[test]
    fn mutating_tool_labels_summarize_large_args() {
        assert_eq!(
            tool_display_label(
                "fs_write",
                &json!({"uri":"file:///tmp/a.txt","content":"hello","append":true})
            ),
            "Write(file:///tmp/a.txt; append; 5 bytes)"
        );
        assert_eq!(
            tool_display_label(
                "fs_edit",
                &json!({"uri":"file:///tmp/a.txt","edits":[{"old_text":"a","new_text":"b"}],"dry_run":true})
            ),
            "Update(file:///tmp/a.txt; 1 edit; dry-run)"
        );
    }

    #[test]
    fn step_activity_uses_user_facing_labels() {
        let updates = event_tui_updates(&Event::StepAdvanced {
            to: "model_turn".into(),
            seq: 1,
        });
        assert!(matches!(
            updates.as_slice(),
            [TuiUpdate::Activity(text)] if text == "thinking"
        ));
    }
}
