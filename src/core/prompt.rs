//! Prompt four-layer model (per design/16-prompt-design.md).
//!
//! - **L0 Invariant**: product identity, tool-use protocol. Cache target.
//! - **L1 Session-Sticky**: agent instruction files, skill hints, archive path. Cache target.
//! - **L2 Runtime Context**: current time, battery, roots. **Never cached.**
//! - **L3 Conversation Tail**: history + current input. Lives in `messages`.
//!
//! The key invariant: **L2 must NOT be part of the cacheable system prefix**.
//! Adapters that support prompt caching emit `system` = L0+L1 with a cache
//! marker, and then a SEPARATE system block/message for L2 (uncached).
//! This way time/battery/etc. changing per turn don't blow the prefix cache.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::clock::utc_date_string;
use crate::core::model::LlmCaps;
use crate::core::provider::ActiveToolSet;
use crate::core::tool::ToolDescriptor;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PromptAuthority {
    #[default]
    System,
    Developer,
    User,
    Tool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptStability {
    Invariant,
    #[default]
    SessionSticky,
    TurnDynamic,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    #[default]
    Prefix,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PromptPosition {
    Front,
    #[default]
    Middle,
    Tail,
}

/// A first-class prompt fragment.
///
/// `PromptBlock` is the small protocol layer above raw strings: hosts can
/// state where a block belongs, whether it is cacheable, and how stable it is.
/// Runner then assembles the final provider request deterministically instead
/// of concatenating every host contribution into the system prompt.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptBlock {
    pub id: String,
    pub text: String,
    pub authority: PromptAuthority,
    pub stability: PromptStability,
    pub cache: CacheScope,
    pub position: PromptPosition,
    pub token_budget: u32,
    pub source_hash: String,
}

impl PromptBlock {
    pub fn session_sticky(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            authority: PromptAuthority::Developer,
            stability: PromptStability::SessionSticky,
            cache: CacheScope::Prefix,
            position: PromptPosition::Middle,
            token_budget: 0,
            source_hash: String::new(),
        }
    }

    pub fn turn_dynamic(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            authority: PromptAuthority::Developer,
            stability: PromptStability::TurnDynamic,
            cache: CacheScope::Never,
            position: PromptPosition::Tail,
            token_budget: 0,
            source_hash: String::new(),
        }
    }

    pub fn is_cacheable_prefix(&self) -> bool {
        self.cache == CacheScope::Prefix && self.stability != PromptStability::TurnDynamic
    }
}

pub fn render_cacheable_blocks(blocks: &[PromptBlock]) -> String {
    render_blocks(blocks.iter().filter(|b| b.is_cacheable_prefix()))
}

pub fn render_runtime_blocks(blocks: &[PromptBlock]) -> String {
    render_blocks(blocks.iter().filter(|b| !b.is_cacheable_prefix()))
}

fn render_blocks<'a>(blocks: impl Iterator<Item = &'a PromptBlock>) -> String {
    let mut ordered: Vec<&PromptBlock> = blocks.filter(|b| !b.text.trim().is_empty()).collect();
    ordered.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.authority.cmp(&b.authority))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut out = String::new();
    for b in ordered {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(b.text.trim());
    }
    out
}

/// The legacy four-layer plan. New hosts should prefer [`PromptBlock`] for
/// prompt contributions; Runner still renders this shape at the adapter
/// boundary as cacheable `system` plus uncached `runtime_context`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PromptPlan {
    /// L0 — invariant prefix (persona, tool-use protocol).
    pub invariant: String,
    /// L1 — session-sticky prefix (agent instructions, skill hints, archive).
    pub session_sticky: String,
    /// L2 — dynamic runtime context. Re-rendered every turn.
    /// Must not leak into the cacheable prefix.
    pub runtime_context: String,
}

impl PromptPlan {
    /// Render L0+L1 as the cacheable system text. Blank lines separate
    /// the layers for readability; adapters typically pass this as a
    /// single cache-marked block.
    pub fn cacheable_prefix(&self) -> String {
        let mut s = String::new();
        if !self.invariant.is_empty() {
            s.push_str(&self.invariant);
        }
        if !self.session_sticky.is_empty() {
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(&self.session_sticky);
        }
        s
    }

    /// True when there's something to pin under L2.
    pub fn has_runtime_context(&self) -> bool {
        !self.runtime_context.trim().is_empty()
    }
}

/// Structured dynamic facts that Runner gathers each turn and renders into
/// `PromptPlan::runtime_context`.
///
/// **Cache invariant — see Vipra & Liu, "Don't Break the Cache" (arxiv
/// 2601.06007), and OpenAI Cookbook *Prompt Caching 201*:** session-
/// identifiers, turn counters, request UUIDs, and other per-turn data
/// embedded anywhere in the prompt destroy prefix matches and bust the
/// cache. The arxiv paper specifically calls out that "system prompt
/// dominates" cache savings — so any per-turn drift here directly bleeds
/// into hit-rate. Anything rendered to text MUST stay byte-stable across
/// consecutive turns of a typical session.
///
/// Concretely:
/// - `now_ms` is kept at ms precision for host code (logging, retry
///   decisions) but rendered only as a UTC date — sub-day precision is not
///   exposed to the model so the prefix stays stable within a day.
/// - `turn` is held for host observability (telemetry, tracing) but
///   intentionally not rendered: the model can already count assistant
///   turns in `history`, and rendering it would change the L2 bytes every
///   turn.
/// - `extra` entries should be rare and stable. If a host needs to inject
///   per-turn data, do so as an explicit `Observation { kind: Steering }`
///   message rather than via `extra` — it keeps the cache prefix clean.
#[derive(Clone, Debug, Default)]
pub struct RuntimeFacts {
    pub now_ms: i64,
    pub turn: u32,
    pub extra: Vec<(String, String)>,
}

impl RuntimeFacts {
    /// Format as `"<key>: <value>"` lines, prefixed by a one-line header.
    /// Only fields that are stable within a UTC day are emitted; see the
    /// struct docs for the cache rationale.
    pub fn render(&self) -> String {
        let mut out = String::from("## Runtime context\n");
        if self.now_ms > 0 {
            out.push_str(&format!(
                "current_date_utc: {}\n",
                utc_date_string(self.now_ms)
            ));
        }
        for (k, v) in &self.extra {
            out.push_str(&format!("{k}: {v}\n"));
        }
        // If only header, treat as empty.
        if out.lines().count() <= 1 {
            String::new()
        } else {
            out
        }
    }
}

// ---------- Request-time prompt assembly helpers ----------
//
// These are pure functions that Runner uses every model turn to compose the
// final cacheable prefix, runtime context, and tool descriptors. Living
// here (not in `runner`) keeps the FSM file focused on state transitions.

/// Extract `PromptBlock`s contributed by an `ActiveToolSet`. Falls back to
/// a single session-sticky block built from the legacy `prompt_augmentation`
/// field when no structured blocks are provided.
pub fn blocks_from_active_set(ats: &ActiveToolSet) -> Vec<PromptBlock> {
    if !ats.prompt_blocks.is_empty() {
        return ats.prompt_blocks.clone();
    }
    if ats.prompt_augmentation.trim().is_empty() {
        Vec::new()
    } else {
        vec![PromptBlock::session_sticky(
            "active_tool_set.prompt_augmentation",
            ats.prompt_augmentation.clone(),
        )]
    }
}

/// Generate a "## Model capability guidance" section that tells the model
/// how vision / file-reading interact for the active tool set. Output is
/// stable for a given (caps, tool-name set), so it stays inside the
/// cacheable prefix.
pub fn capability_hint(caps: &LlmCaps, tools: &[ToolDescriptor]) -> String {
    let has_fs_read = has_tool(tools, "fs_read");
    let has_fs_stat = has_tool(tools, "fs_stat");
    let has_sh_exec = has_tool(tools, "sh_exec");

    let mut out = String::from("## Model capability guidance\n");
    if caps.vision {
        out.push_str(
            "- This model supports image inputs. If visual content matters, inspect the image instead of guessing from filenames, MIME types, or surrounding text.\n",
        );
        if has_fs_read {
            out.push_str(
                "- `fs_read` can return supported PNG/JPEG/GIF/WebP files as image attachments visible to this model. Use it when screenshots, photos, diagrams, UI states, charts, or OCR-relevant images are material; leave `force_text=false` for visual inspection.\n",
            );
        } else {
            out.push_str(
                "- No local image-reading tool is active. You can still reason about images already attached by the user or returned by other tools.\n",
            );
        }
    } else {
        out.push_str(
            "- This model does not support image inputs. Image attachments from tools are omitted before the next model turn.\n",
        );
        if has_fs_read {
            out.push_str(
                "- Do not use `fs_read` on image files for visual inspection or OCR. It is still valid for text files. For image knowledge, use available text-producing alternatives: ",
            );
            let mut alternatives = Vec::new();
            if has_fs_stat {
                alternatives.push("`fs_stat` for file metadata");
            }
            if has_sh_exec {
                alternatives.push("`sh_exec` with available OCR or image-processing commands");
            }
            alternatives.push("a user-provided description");
            out.push_str(&alternatives.join(", "));
            out.push_str(".\n");
        } else {
            out.push_str(
                "- Use only available text-producing tools or ask the user for a description when the task depends on image content.\n",
            );
        }
    }
    out
}

/// Append a one-shot capability note onto each tool description that needs
/// one. Currently only `fs_read` cares — vision-capable models can inspect
/// returned images, vision-less models must not call it on image files.
pub fn adapt_tool_descriptors(tools: &[ToolDescriptor], caps: &LlmCaps) -> Vec<ToolDescriptor> {
    tools
        .iter()
        .map(|tool| {
            let mut tool = tool.clone();
            if tool.name == "fs_read" && !tool.description.contains("Model capability note:") {
                let note = if caps.vision {
                    "Model capability note: this model supports vision, so supported image attachments returned by `fs_read` are visible on the next model turn. Leave `force_text=false` for visual inspection."
                } else {
                    "Model capability note: this model does not support vision. Do not call `fs_read` on image files for visual inspection or OCR; image attachments are omitted before the next model turn. Use text-producing alternatives instead."
                };
                tool.description = format!("{}\n\n{note}", tool.description.trim_end());
            }
            tool
        })
        .collect()
}

fn has_tool(tools: &[ToolDescriptor], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

/// Append `section` to `out` with a `\n\n` separator if both sides are
/// non-empty. No-op when `section` is blank.
pub fn append_section(out: &mut String, section: &str) {
    let section = section.trim();
    if section.is_empty() {
        return;
    }
    if !out.trim().is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(section);
}

/// Stable hash of the cacheable prompt prefix + canonical tool schemas.
/// Used as the `prompt_cache_key` value with `CacheKeyStrategy::PrefixHash`.
/// The version tag lets us bump the hash deterministically if the input
/// space ever changes shape.
pub fn cache_fingerprint(cacheable_prefix: &str, tools: &[ToolDescriptor]) -> String {
    let mut h = Sha256::new();
    h.update(b"muagent-prompt-v2\0");
    h.update(cacheable_prefix.as_bytes());

    let mut ordered_tools: Vec<&ToolDescriptor> = tools.iter().collect();
    ordered_tools.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in ordered_tools {
        h.update(b"\0tool\0");
        match serde_json::to_string(tool) {
            Ok(s) => h.update(s.as_bytes()),
            Err(_) => {
                h.update(tool.name.as_bytes());
                h.update(b"\0");
                h.update(tool.description.as_bytes());
            }
        }
    }

    let bytes = h.finalize();
    let mut out = String::with_capacity(16);
    for b in &bytes[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    format!("muagent-{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cacheable_prefix_joins_l0_and_l1() {
        let p = PromptPlan {
            invariant: "You are μAgent.".into(),
            session_sticky: "Workspace: /home".into(),
            runtime_context: "should not appear".into(),
        };
        let s = p.cacheable_prefix();
        assert!(s.contains("You are μAgent."));
        assert!(s.contains("Workspace: /home"));
        assert!(!s.contains("should not appear"));
    }

    #[test]
    fn runtime_facts_render_includes_nonzero_fields() {
        let r = RuntimeFacts {
            now_ms: 1_700_000_000_000,
            turn: 3,
            extra: vec![("battery".into(), "72%".into())],
        };
        let s = r.render();
        assert!(s.contains("current_date_utc: 2023-11-14"));
        assert!(!s.contains("now_ms"));
        // `turn` is held for host use but NOT rendered — see RuntimeFacts
        // docs. Cache hit-rate research (arxiv 2601.06007) treats turn
        // counters as a cache-busting anti-pattern.
        assert!(!s.contains("turn:"));
        assert!(s.contains("battery: 72%"));
    }

    #[test]
    fn runtime_facts_render_byte_stable_across_turns_within_day() {
        // The whole reason `turn` is suppressed: two consecutive turns
        // inside the same UTC day must produce byte-identical
        // runtime_context, so any cache breakpoint downstream of L2
        // sees a matching prefix.
        let a = RuntimeFacts {
            now_ms: 1_700_000_000_000,
            turn: 5,
            extra: vec![],
        };
        let b = RuntimeFacts {
            now_ms: 1_700_000_000_000 + 60_000,
            turn: 6,
            extra: vec![],
        };
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn runtime_facts_empty_renders_empty_string() {
        assert!(RuntimeFacts::default().render().is_empty());
    }
}
