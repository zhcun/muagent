//! Translate `Event`s into chat-bound `TuiUpdate`s and one-line tool labels.
//! Keeps the presentation logic out of `core` (which only carries raw tool
//! name + args JSON).

use serde_json::Value;

use crate::cli_app::driver::TuiUpdate;
use crate::cli_app::truncate;
use crate::core::event::Event;

pub fn event_tui_updates(ev: &Event) -> Vec<TuiUpdate> {
    match ev {
        // ToolCallStart / End are handled by `drive_until_terminal_with_updates`.
        // The driver sends ToolStart before awaiting the step, then uses End
        // to update that running row with ok/brief/detail.
        Event::ToolCallStart { .. } | Event::ToolCallEnd { .. } => vec![],
        Event::StepAdvanced { to, .. } => vec![TuiUpdate::Activity(stage_label(to).into())],
        Event::AssistantMessage { text, .. } => {
            // Completed assistant messages are the durable version of any
            // live deltas the TUI may already have rendered. Empty-text turns
            // (model called tools without speaking) only get an activity hint
            // to avoid blank chat lines.
            let trimmed = text.trim();
            if trimmed.is_empty() {
                vec![
                    TuiUpdate::AssistantStreamReset,
                    TuiUpdate::Activity("assistant: (tool call only)".into()),
                ]
            } else {
                vec![
                    TuiUpdate::Assistant(text.clone()),
                    TuiUpdate::Activity(format!("responding: {}", truncate(trimmed, 120))),
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
        Event::AssistantDelta { text, .. } => vec![TuiUpdate::AssistantDelta(text.clone())],
        Event::SessionStart { .. } | Event::UserMessage { .. } => vec![],
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
        "sh_exec" => return sh_exec_display_label(args),
        "fs_read" => ("Read", file_arg_label(args, "uri")),
        "fs_write" => ("Write", write_arg_label(args)),
        "fs_edit" => ("Update", edit_arg_label(args)),
        "fs_list" => ("List", file_arg_label(args, "uri")),
        "fs_stat" => ("Stat", file_arg_label(args, "uri")),
        "fs_delete" => ("Delete", file_arg_label(args, "uri")),
        "fs_rename" => (
            "Rename",
            format!(
                "{} → {}",
                display_file_uri(&s(args, "from")),
                display_file_uri(&s(args, "to"))
            ),
        ),
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

fn sh_exec_display_label(args: &Value) -> String {
    match args.get("action").and_then(Value::as_str) {
        Some("wait") => "Wait(background job)".into(),
        Some("poll") => "Check(background job)".into(),
        Some("kill") => "Stop(background job)".into(),
        Some(_) => format!("Job({})", sh_exec_args_label(args)),
        None => format!("Bash({})", sh_exec_args_label(args)),
    }
}

fn stage_label(to: &str) -> &'static str {
    match to {
        "model_turn" => "thinking",
        "tool_batch" => "working",
        "tool_intent" => "checking tool result",
        _ => "working",
    }
}

fn file_arg_label(args: &Value, key: &str) -> String {
    one_line(
        &display_file_uri(args.get(key).and_then(Value::as_str).unwrap_or("")),
        96,
    )
}

fn display_file_uri(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("file://") else {
        return raw.to_string();
    };
    let path = if let Some(localhost) = rest.strip_prefix("localhost/") {
        format!("/{localhost}")
    } else {
        rest.to_string()
    };
    compact_display_path(&path)
}

fn compact_display_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let path_buf = std::path::Path::new(path);
        if let Ok(rest) = path_buf.strip_prefix(&cwd) {
            if !rest.as_os_str().is_empty() {
                return rest.to_string_lossy().to_string();
            }
        }
    }

    let home_short = shorten_home(path);
    let parts = home_short
        .split('/')
        .filter(|part| !part.is_empty() && *part != "~")
        .collect::<Vec<_>>();
    if home_short.chars().count() <= 64 && parts.len() <= 6 {
        return home_short;
    }
    let keep = parts.len().min(5);
    let suffix = parts[parts.len().saturating_sub(keep)..].join("/");
    if home_short.starts_with("~/") {
        format!("~/.../{suffix}")
    } else {
        format!(".../{suffix}")
    }
}

fn shorten_home(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME").and_then(|home| home.into_string().ok()) else {
        return path.to_string();
    };
    if home == "/" || !path.starts_with(&home) {
        return path.to_string();
    }
    match path.strip_prefix(&home) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn sh_exec_args_label(args: &Value) -> String {
    if let Some(action) = args.get("action").and_then(Value::as_str) {
        return match action {
            "wait" => "wait for background job".into(),
            "poll" => "check background job".into(),
            "kill" => "stop background job".into(),
            other => one_line(&format!("{other} background job"), 96),
        };
    }

    let bin = args.get("bin").and_then(Value::as_str).unwrap_or("");
    let stdin = args.get("stdin").and_then(Value::as_str);
    if is_shell_bin(bin) {
        if let Some(summary) = stdin.and_then(shell_stdin_summary) {
            return summary;
        }
    }

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
    if let Some(stdin) = stdin {
        if !label.is_empty() {
            label.push_str("; ");
        }
        label.push_str(&format!("stdin {} chars", stdin.chars().count()));
    }
    one_line(&label, 96)
}

fn is_shell_bin(bin: &str) -> bool {
    bin.rsplit('/')
        .next()
        .is_some_and(|name| matches!(name, "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh"))
}

fn shell_stdin_summary(stdin: &str) -> Option<String> {
    let commands = stdin
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let first = commands.first().copied()?;
    let mut summary = one_line(first, 90);
    if commands.len() > 1 {
        summary.push_str(" …");
    }
    Some(summary)
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
        "sh_exec" => {
            let exit = n(detail, "exit");
            let stdout = n(detail, "stdout_bytes").unwrap_or(0);
            let stderr = n(detail, "stderr_bytes").unwrap_or(0);
            let command = detail
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let stdout_tail = detail
                .get("stdout_tail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let stderr_tail = detail
                .get("stderr_tail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let state = detail
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let status = match (exit, state.is_empty()) {
                (Some(code), _) => format!("exit {code}"),
                (None, false) => state.to_string(),
                (None, true) => return None,
            };
            let mut parts = Vec::new();
            if !command.trim().is_empty() {
                parts.push(one_line(command, 56));
            }
            parts.push(format!("{status} · out {stdout}B · err {stderr}B"));
            if let Some(tail) = output_tail_summary("out", stdout_tail) {
                parts.push(tail);
            }
            if let Some(tail) = output_tail_summary("err", stderr_tail) {
                parts.push(tail);
            }
            Some(one_line(&parts.join(" · "), 160))
        }
        _ => None,
    }
}

fn output_tail_summary(label: &str, text: &str) -> Option<String> {
    let compact = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    (!compact.is_empty()).then(|| format!("{label}: {}", one_line(&compact, 64)))
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
            "Read(/tmp/project/src/main.rs)"
        );
        assert_eq!(
            tool_display_label(
                "fs_read",
                &json!({"uri":"file:///opt/work/findpet/frontend/src/app/pets/page.tsx"})
            ),
            "Read(.../frontend/src/app/pets/page.tsx)"
        );
        assert_eq!(
            tool_display_label(
                "sh_exec",
                &json!({"bin":"cargo","args":["test","--lib"],"timeout_ms":30000})
            ),
            "Bash(cargo test --lib)"
        );
        assert_eq!(
            tool_display_label(
                "sh_exec",
                &json!({"bin":"bash","stdin":"find src -maxdepth 2 -type f\nwc -l src/lib.rs"})
            ),
            "Bash(find src -maxdepth 2 -type f …)"
        );
        assert_eq!(
            tool_display_label(
                "sh_exec",
                &json!({"action":"wait","job_id":"sh_5eec44a5e0b04ea5bf9f46f74d694fa6"})
            ),
            "Wait(background job)"
        );
    }

    #[test]
    fn file_uri_compaction_requires_real_path_prefix() {
        let cwd = std::env::current_dir().unwrap();
        let sibling = format!("{}2/src/main.rs", cwd.display());
        let label = tool_display_label("fs_read", &json!({"uri": format!("file://{sibling}")}));
        let expected_tail = format!(
            "{}2/src/main.rs",
            cwd.file_name().unwrap().to_string_lossy()
        );

        assert!(!label.contains("Read(2/src/main.rs)"), "{label}");
        assert!(label.contains(&expected_tail), "{label}");
    }

    #[test]
    fn mutating_tool_labels_summarize_large_args() {
        assert_eq!(
            tool_display_label(
                "fs_write",
                &json!({"uri":"file:///tmp/a.txt","content":"hello","append":true})
            ),
            "Write(/tmp/a.txt; append; 5 bytes)"
        );
        assert_eq!(
            tool_display_label(
                "fs_edit",
                &json!({"uri":"file:///tmp/a.txt","edits":[{"old_text":"a","new_text":"b"}],"dry_run":true})
            ),
            "Update(/tmp/a.txt; 1 edit; dry-run)"
        );
    }

    #[test]
    fn sh_exec_extra_line_summarizes_exit_and_output() {
        assert_eq!(
            super::tool_result_extra_line(
                "sh_exec",
                &json!({"exit":7,"stdout_bytes":12,"stderr_bytes":3})
            ),
            Some("exit 7 · out 12B · err 3B".into())
        );
        assert_eq!(
            super::tool_result_extra_line(
                "sh_exec",
                &json!({"state":"running","stdout_bytes":12,"stderr_bytes":3})
            ),
            Some("running · out 12B · err 3B".into())
        );
        assert_eq!(
            super::tool_result_extra_line(
                "sh_exec",
                &json!({
                    "state":"exited",
                    "exit":0,
                    "stdout_bytes":20,
                    "stderr_bytes":0,
                    "command":"bash",
                    "stdout_tail":"found 12 files\nok\n"
                })
            ),
            Some("bash · exit 0 · out 20B · err 0B · out: found 12 files / ok".into())
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
