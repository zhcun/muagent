//! Cumulative file-operation ledger for compaction.
//!
//! Why this exists: when a coding agent's session is compacted, the raw
//! tool_call/tool_result pairs that *named* the files get summarized into
//! prose. The summarizer LLM may or may not preserve the exact file paths.
//! That's a real problem for follow-up turns: the model needs to know
//! "I've already read src/foo.rs" or "I've modified Cargo.toml" without
//! re-reading them.
//!
//! pi-mono's compaction (`packages/coding-agent/docs/compaction.md`)
//! addresses this with a `CompactionEntry.details.{readFiles, modifiedFiles}`
//! that accumulates **across** every compaction. Codex tracks file changes
//! through its tool history layer. We do the same here, but render the
//! ledger as a deterministic markdown section embedded in the summary
//! observation — that keeps the data inside the prompt the model reads,
//! and survives session reload because it's part of `state.history`.
//!
//! ## Where the data comes from
//!
//! - **New tool_use turns** in the slice being compressed: extracted from
//!   `Message::Assistant.tool_calls` by sniffing tool name + args.
//! - **Previous summary observation** in the compressed slice (if there
//!   is a chain of compactions): parsed back from its `## Files Touched`
//!   section. This is what makes the ledger truly cumulative — round N+1
//!   inherits everything round N saw.

use std::collections::BTreeSet;

use crate::core::types::{Message, ObsKind};
use serde_json::Value;

/// One bucket per file lifecycle category. Buckets use `BTreeSet` so the
/// rendered output is deterministic — important for prompt-cache
/// stability of the summary observation across follow-up turns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileLedger {
    /// Files the agent has at least *read* / listed / stat'd. Used for
    /// "I've already looked at this; don't re-read" decisions.
    pub read: BTreeSet<String>,
    /// Files the agent has modified (write, edit, delete, rename src/dst).
    /// Strictly stronger evidence of state change than `read`.
    pub modified: BTreeSet<String>,
}

const LEDGER_HEADER: &str = "## Files Touched (cumulative across compactions)";
const LEDGER_READ_HEADER: &str = "### Read";
const LEDGER_MODIFIED_HEADER: &str = "### Modified";

impl FileLedger {
    /// Walk a slice of history and accumulate file ops:
    /// 1. Tool calls in `Message::Assistant.tool_calls` whose name is a
    ///    known fs-touching builtin.
    /// 2. Any prior `Observation { kind: Summary }` containing a previously
    ///    rendered ledger — those entries get folded back in so successive
    ///    compactions don't shed earlier file knowledge.
    pub fn from_history_slice(messages: &[Message]) -> Self {
        let mut led = Self::default();
        for m in messages {
            led.absorb_message(m);
        }
        led
    }

    pub fn absorb_message(&mut self, m: &Message) {
        match m {
            Message::Assistant { tool_calls, .. } => {
                for c in tool_calls {
                    self.absorb_tool_call(&c.tool_name, &c.args);
                }
            }
            Message::Observation {
                kind: ObsKind::Summary,
                text,
            } => {
                self.absorb_rendered(text);
            }
            _ => {}
        }
    }

    /// Heuristic mapping from builtin fs tool name + args → file paths.
    /// Unknown tool names are silently ignored (e.g. `sh_exec`, MCP tools)
    /// because their args don't reliably name a file path.
    pub fn absorb_tool_call(&mut self, tool_name: &str, args: &Value) {
        let push_uri = |bucket: &mut BTreeSet<String>, key: &str| {
            if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    bucket.insert(s.to_string());
                }
            }
        };
        match tool_name {
            "fs_read" | "fs_list" | "fs_stat" => push_uri(&mut self.read, "uri"),
            "fs_edit" | "fs_write" | "fs_delete" => push_uri(&mut self.modified, "uri"),
            "fs_rename" => {
                push_uri(&mut self.modified, "from");
                push_uri(&mut self.modified, "to");
            }
            _ => {}
        }
    }

    /// Parse a previously rendered ledger out of free-form text. Tolerates
    /// LLM paraphrasing around it; only requires the markers we ourselves
    /// write. Robust to extra whitespace and missing sections.
    pub fn absorb_rendered(&mut self, text: &str) {
        let Some(start) = text.find(LEDGER_HEADER) else {
            return;
        };
        let body = &text[start + LEDGER_HEADER.len()..];
        // Scan line by line, keyed by the two sub-headers we emit.
        let mut bucket: Option<&mut BTreeSet<String>> = None;
        for raw in body.lines() {
            let line = raw.trim();
            if line.starts_with("##") && !line.starts_with("###") {
                // Hit the next top-level section — stop.
                break;
            }
            if line == LEDGER_READ_HEADER {
                bucket = Some(&mut self.read);
                continue;
            }
            if line == LEDGER_MODIFIED_HEADER {
                bucket = Some(&mut self.modified);
                continue;
            }
            if let Some(rest) = line.strip_prefix("- ") {
                if let Some(b) = bucket.as_deref_mut() {
                    if !rest.is_empty() {
                        b.insert(rest.to_string());
                    }
                }
            }
        }
    }

    /// Emit the canonical markdown form. Empty ledger renders empty so we
    /// can unconditionally concatenate without producing dangling headers.
    pub fn render(&self) -> String {
        if self.read.is_empty() && self.modified.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(128);
        out.push_str(LEDGER_HEADER);
        out.push('\n');
        if !self.read.is_empty() {
            out.push_str(LEDGER_READ_HEADER);
            out.push('\n');
            for p in &self.read {
                out.push_str("- ");
                out.push_str(p);
                out.push('\n');
            }
        }
        if !self.modified.is_empty() {
            out.push_str(LEDGER_MODIFIED_HEADER);
            out.push('\n');
            for p in &self.modified {
                out.push_str("- ");
                out.push_str(p);
                out.push('\n');
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.modified.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool::PendingCall;
    use crate::core::types::Content;
    use serde_json::json;

    fn asst_with_call(name: &str, args: Value) -> Message {
        Message::Assistant {
            content: Content::text(""),
            tool_calls: vec![PendingCall::new("c1", name, args)],
            thinking: vec![],
        }
    }

    #[test]
    fn extracts_fs_read_uri() {
        let mut l = FileLedger::default();
        l.absorb_tool_call("fs_read", &json!({"uri": "file:///a.rs"}));
        assert!(l.read.contains("file:///a.rs"));
        assert!(l.modified.is_empty());
    }

    #[test]
    fn extracts_fs_write_into_modified_not_read() {
        let mut l = FileLedger::default();
        l.absorb_tool_call("fs_write", &json!({"uri": "file:///b.rs", "content": "x"}));
        assert!(l.modified.contains("file:///b.rs"));
        assert!(l.read.is_empty());
    }

    #[test]
    fn extracts_fs_edit_into_modified_not_read() {
        let mut l = FileLedger::default();
        l.absorb_tool_call(
            "fs_edit",
            &json!({"uri": "file:///b.rs", "old_text": "a", "new_text": "b"}),
        );
        assert!(l.modified.contains("file:///b.rs"));
        assert!(l.read.is_empty());
    }

    #[test]
    fn extracts_fs_rename_both_sides() {
        let mut l = FileLedger::default();
        l.absorb_tool_call(
            "fs_rename",
            &json!({"from": "file:///old.rs", "to": "file:///new.rs"}),
        );
        assert!(l.modified.contains("file:///old.rs"));
        assert!(l.modified.contains("file:///new.rs"));
    }

    #[test]
    fn ignores_unknown_tools() {
        let mut l = FileLedger::default();
        l.absorb_tool_call("sh_exec", &json!({"command": "rm -rf /"}));
        l.absorb_tool_call("net_http", &json!({"url": "https://x"}));
        assert!(l.is_empty());
    }

    #[test]
    fn render_then_absorb_roundtrips() {
        let mut a = FileLedger::default();
        a.read.insert("file:///x".into());
        a.read.insert("file:///y".into());
        a.modified.insert("file:///z".into());
        let rendered = a.render();
        let mut b = FileLedger::default();
        b.absorb_rendered(&rendered);
        assert_eq!(a, b);
    }

    #[test]
    fn absorb_rendered_tolerates_surrounding_summary_prose() {
        // Simulate an LLM-produced summary that wraps our deterministic
        // ledger section. We must still recover the entries exactly.
        let mut a = FileLedger::default();
        a.read.insert("file:///parsed-back.rs".into());
        let summary = format!(
            "## Goal\nDo the thing.\n\n## Progress\n- did stuff\n\n{}\n## Open Questions\n- none\n",
            a.render()
        );
        let mut b = FileLedger::default();
        b.absorb_rendered(&summary);
        assert_eq!(a, b);
    }

    #[test]
    fn from_history_slice_merges_calls_and_prior_summary() {
        // Round 1's summary already lists `old.rs` as read.
        let mut prior = FileLedger::default();
        prior.read.insert("file:///old.rs".into());
        let summary_text = format!("## Goal\n…\n\n{}\n", prior.render());
        let history = vec![
            Message::Observation {
                kind: ObsKind::Summary,
                text: summary_text,
            },
            asst_with_call("fs_read", json!({"uri": "file:///new.rs"})),
            asst_with_call("fs_write", json!({"uri": "file:///out.rs", "content": "x"})),
        ];
        let led = FileLedger::from_history_slice(&history);
        assert!(
            led.read.contains("file:///old.rs"),
            "must inherit from prior summary"
        );
        assert!(led.read.contains("file:///new.rs"));
        assert!(led.modified.contains("file:///out.rs"));
    }

    #[test]
    fn render_is_deterministic_under_insertion_order() {
        let mut a = FileLedger::default();
        a.read.insert("zzz".into());
        a.read.insert("aaa".into());
        let mut b = FileLedger::default();
        b.read.insert("aaa".into());
        b.read.insert("zzz".into());
        assert_eq!(a.render(), b.render(), "BTreeSet keeps render byte-stable");
    }
}
