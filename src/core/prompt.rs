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
                utc_date_from_unix_ms(self.now_ms)
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

fn utc_date_from_unix_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil calendar conversion, adapted for days since Unix
// epoch. This avoids pulling a time crate into core for one stable date line.
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += if m <= 2 { 1 } else { 0 };
    (y as i32, m as u32, d as u32)
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
