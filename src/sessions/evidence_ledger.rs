//! Deterministic evidence ledger for compaction.
//!
//! Natural-language summaries are lossy. For exact identifiers, error codes,
//! and memory anchors, we want a small cumulative section that is extracted
//! from the transcript deterministically and appended to the summary. This
//! mirrors `FileLedger`, but for high-value facts that are not file paths.

use std::collections::BTreeMap;

use crate::core::types::{Content, Message, ObsKind};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvidenceLedger {
    exact_identifiers: BTreeMap<String, EvidenceEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EvidenceEntry {
    key: String,
    value: String,
    source: String,
    context: String,
}

const LEDGER_HEADER: &str = "## Evidence Ledger (cumulative exact facts)";
const LEDGER_IDENTIFIERS_HEADER: &str = "### Exact Identifiers";
const MAX_ENTRIES: usize = 64;
const MAX_CONTEXT_CHARS: usize = 180;

impl EvidenceLedger {
    pub fn from_history_slice(messages: &[Message]) -> Self {
        let mut ledger = Self::default();
        for m in messages {
            ledger.absorb_message(m);
        }
        ledger
    }

    pub fn absorb_message(&mut self, m: &Message) {
        match m {
            Message::User { content } => self.absorb_text("user", &content_text(content)),
            Message::Assistant { content, .. } => {
                self.absorb_text("assistant", &content_text(content));
            }
            Message::ToolResult { result, .. } => {
                self.absorb_text("tool_result", &result.model_text());
            }
            Message::System { content } => self.absorb_text("system", &content_text(content)),
            Message::Observation { kind, text } => {
                if matches!(kind, ObsKind::Summary) {
                    self.absorb_rendered(text);
                }
                self.absorb_text("observation", text);
            }
        }
    }

    pub fn absorb_text(&mut self, source: &str, text: &str) {
        for line in text.lines() {
            let line = collapse_ws(line);
            if line.is_empty() {
                continue;
            }
            for value in identifier_candidates(&line) {
                let key = infer_key(&line);
                let context = context_around(&line, &value);
                self.insert(key, value, source.to_string(), context);
            }
        }
    }

    pub fn absorb_rendered(&mut self, text: &str) {
        let Some(start) = text.find(LEDGER_HEADER) else {
            return;
        };
        let body = &text[start + LEDGER_HEADER.len()..];
        let mut in_identifiers = false;
        for raw in body.lines() {
            let line = raw.trim();
            if line.starts_with("##") && !line.starts_with("###") {
                break;
            }
            if line == LEDGER_IDENTIFIERS_HEADER {
                in_identifiers = true;
                continue;
            }
            if line.starts_with("###") {
                in_identifiers = false;
                continue;
            }
            if !in_identifiers {
                continue;
            }
            let Some(rest) = line.strip_prefix("- ") else {
                continue;
            };
            let key = field(rest, "key").unwrap_or_else(|| "exact identifier".into());
            let Some(value) = field(rest, "value") else {
                continue;
            };
            let source = field(rest, "source").unwrap_or_else(|| "prior_summary".into());
            let context = field(rest, "context").unwrap_or_default();
            self.insert(key, value, source, context);
        }
    }

    pub fn render(&self) -> String {
        if self.exact_identifiers.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(256);
        out.push_str(LEDGER_HEADER);
        out.push('\n');
        out.push_str(LEDGER_IDENTIFIERS_HEADER);
        out.push('\n');
        for entry in self.exact_identifiers.values() {
            out.push_str("- key=");
            out.push_str(&entry.key);
            out.push_str(" | value=");
            out.push_str(&entry.value);
            out.push_str(" | source=");
            out.push_str(&entry.source);
            if !entry.context.is_empty() {
                out.push_str(" | context=");
                out.push_str(&entry.context);
            }
            out.push('\n');
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.exact_identifiers.is_empty()
    }

    fn insert(&mut self, key: String, value: String, source: String, context: String) {
        if self.exact_identifiers.len() >= MAX_ENTRIES
            && !self.exact_identifiers.contains_key(&value)
        {
            return;
        }
        self.exact_identifiers
            .entry(value.clone())
            .and_modify(|existing| {
                if existing.key == "exact identifier" && key != "exact identifier" {
                    existing.key = key.clone();
                }
                if !existing.source.split("; ").any(|s| s == source) {
                    existing.source.push_str("; ");
                    existing.source.push_str(&source);
                }
                if existing.context.is_empty() && !context.is_empty() {
                    existing.context = context.clone();
                }
            })
            .or_insert(EvidenceEntry {
                key,
                value,
                source,
                context,
            });
    }
}

fn identifier_candidates(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.' | '/' | '@') {
            buf.push(ch);
        } else {
            push_candidate(&mut out, &buf);
            buf.clear();
        }
    }
    push_candidate(&mut out, &buf);
    out
}

fn push_candidate(out: &mut Vec<String>, raw: &str) {
    let value = raw
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if is_exact_identifier(&value) && !out.contains(&value) {
        out.push(value);
    }
}

fn is_exact_identifier(value: &str) -> bool {
    if !(6..=96).contains(&value.len()) {
        return false;
    }
    if matches!(value, "UNKNOWN" | "NULL" | "NONE") {
        return false;
    }
    let has_upper = value.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    let has_strong_separator =
        value.chars().any(|c| matches!(c, '-' | '_' | '/' | '@')) || value.contains("::");
    has_upper && has_digit && has_strong_separator
}

fn infer_key(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if lower.contains("case code") {
        "case code".into()
    } else if lower.contains("case owner") {
        "case owner".into()
    } else if lower.contains("error") {
        "error code".into()
    } else if lower.contains("identifier") || lower.contains("anchor") {
        "identifier".into()
    } else {
        "exact identifier".into()
    }
}

fn context_around(line: &str, value: &str) -> String {
    let collapsed = collapse_ws(line);
    let char_count = collapsed.chars().count();
    if char_count <= MAX_CONTEXT_CHARS {
        return collapsed;
    }
    let Some(byte_pos) = collapsed.find(value) else {
        return collapsed.chars().take(MAX_CONTEXT_CHARS).collect();
    };
    let prefix: String = collapsed[..byte_pos].chars().rev().take(70).collect();
    let prefix: String = prefix.chars().rev().collect();
    let suffix_start = byte_pos + value.len();
    let suffix: String = collapsed[suffix_start..].chars().take(70).collect();
    format!("...{prefix}{value}{suffix}...")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn field(line: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(" | ").unwrap_or(rest.len());
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn content_text(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                crate::core::types::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Message;

    #[test]
    fn extracts_anchor_identifiers_from_user_text() {
        let text = "HIGH PRIORITY MEMORY ANCHOR: the exact RULER case code is \
            PG-NIAH-7429-SUMMIT. Preserve this exact identifier.";
        let mut ledger = EvidenceLedger::default();
        ledger.absorb_text("user", text);
        let rendered = ledger.render();
        assert!(rendered.contains("## Evidence Ledger"));
        assert!(rendered.contains("key=case code"));
        assert!(rendered.contains("value=PG-NIAH-7429-SUMMIT"));
    }

    #[test]
    fn render_then_absorb_roundtrips() {
        let mut a = EvidenceLedger::default();
        a.absorb_text(
            "user",
            "The exact RULER case owner is MIRA-SOL-3185 for later.",
        );
        let rendered = a.render();
        let mut b = EvidenceLedger::default();
        b.absorb_rendered(&rendered);
        assert_eq!(a, b);
    }

    #[test]
    fn does_not_treat_ordinary_citation_as_identifier() {
        let mut ledger = EvidenceLedger::default();
        ledger.absorb_text("user", "Analects VII:36 appears in the essay text.");
        assert!(ledger.is_empty());
    }

    #[test]
    fn from_history_slice_merges_prior_summary_and_new_text() {
        let mut prior = EvidenceLedger::default();
        prior.absorb_text("user", "The exact RULER case code is PG-NIAH-7429-SUMMIT.");
        let history = vec![
            Message::Observation {
                kind: ObsKind::Summary,
                text: prior.render(),
            },
            Message::User {
                content: Content::text("The exact RULER case owner is MIRA-SOL-3185."),
            },
        ];
        let ledger = EvidenceLedger::from_history_slice(&history);
        let rendered = ledger.render();
        assert!(rendered.contains("PG-NIAH-7429-SUMMIT"));
        assert!(rendered.contains("MIRA-SOL-3185"));
    }
}
