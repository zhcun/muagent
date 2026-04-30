//! Thinking (reasoning) — **Core contract**.
//!
//! See `design/17-thinking-design.md`. Short version:
//!
//! - Thinking is protocol, not UI: provider-returned reasoning artifacts
//!   can influence the next turn's correctness, so Core owns the types
//!   and the replay rules.
//! - Visible text, tool calls, and thinking are three distinct concerns
//!   on an Assistant turn. Don't merge them.
//! - `ReplayPolicy::MustReplayUnmodified` is non-negotiable: Anthropic's
//!   extended-thinking protocol requires verbatim round-trip of thinking
//!   blocks (including `redacted_thinking`) in tool-use continuations.
//! - Prompt cache and thinking are separate concerns but interact —
//!   thinking blocks that round-trip are themselves cacheable prefix
//!   content on the provider side.

use serde::{Deserialize, Serialize};

// ============================================================================
// Request side — what the host asks the model to do
// ============================================================================

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Explicitly disabled. No reasoning tokens will be requested.
    Off,
    /// Let the adapter pick a sensible default for this model family.
    /// Providers without native thinking degrade silently to `Off`.
    #[default]
    Auto,
    /// Explicitly enabled with the attached `effort` / `budget`.
    Enabled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingBudget {
    /// Absolute token budget. Adapters clamp to provider limits.
    Tokens(u32),
    /// Coarse 0–100 slider. Adapter maps to provider-specific values.
    Relative(u8),
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingVisibility {
    /// Default — don't surface thinking to end users. (Audit only.)
    #[default]
    Hidden,
    /// Provider-returned summary may be shown to users, full text may not.
    SummaryAllowed,
    /// Host has opted in to showing raw thinking to end users (e.g. debug).
    UserVisible,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    pub effort: Option<ThinkingEffort>,
    pub budget: Option<ThinkingBudget>,
    pub visibility: ThinkingVisibility,
}

impl ThinkingConfig {
    pub fn off() -> Self {
        Self {
            mode: ThinkingMode::Off,
            ..Default::default()
        }
    }
    pub fn auto() -> Self {
        Self {
            mode: ThinkingMode::Auto,
            ..Default::default()
        }
    }
    pub fn enabled_effort(effort: ThinkingEffort) -> Self {
        Self {
            mode: ThinkingMode::Enabled,
            effort: Some(effort),
            ..Default::default()
        }
    }
    pub fn enabled_budget(tokens: u32) -> Self {
        Self {
            mode: ThinkingMode::Enabled,
            budget: Some(ThinkingBudget::Tokens(tokens)),
            ..Default::default()
        }
    }
    pub fn is_on(&self) -> bool {
        self.mode != ThinkingMode::Off
    }
}

// ============================================================================
// Response side — provider-returned artifacts that may need to round-trip
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingKind {
    /// A short, provider-generated summary of the reasoning.
    SummaryText,
    /// Full reasoning text.
    FullText,
    /// Opaque redacted payload (Anthropic's `redacted_thinking`): encrypted
    /// by the provider and must be forwarded unchanged on tool continuation.
    RedactedOpaque,
    /// Provider-opaque bytes/JSON (e.g. OpenAI Responses API reasoning items).
    ProviderOpaque,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    /// Nothing to round-trip. Safe to drop.
    #[default]
    Never,
    /// Replay if present; provider accepts omission too.
    Optional,
    /// Provider REQUIRES verbatim replay on the next turn. Dropping or
    /// mutating breaks the protocol (Anthropic extended thinking).
    MustReplayUnmodified,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "payload_kind", rename_all = "snake_case")]
pub enum ThinkingPayload {
    Text { text: String },
    OpaqueBytes { b64: String },
    Json { value: serde_json::Value },
}

/// One thinking artifact attached to an assistant turn. Multiple artifacts
/// per turn are allowed (Anthropic can return several consecutive blocks;
/// OpenAI Responses API can produce multiple reasoning items).
///
/// `provider_signature` is provider-specific data the adapter must preserve
/// verbatim for replay (e.g. Anthropic's `signature` field on thinking blocks,
/// OpenAI's `encrypted_content` for reasoning items).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ThinkingArtifact {
    pub provider: String,
    pub kind: ThinkingKind,
    pub replay: ReplayPolicy,
    pub visibility: ThinkingVisibility,
    pub payload: ThinkingPayload,
    /// Provider-specific opaque data to forward unchanged (e.g. Anthropic
    /// `signature` for `thinking` blocks, or the whole block JSON for
    /// `redacted_thinking`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_signature: Option<String>,
}

impl ThinkingArtifact {
    /// True if the adapter MUST emit this artifact on the next tool-use turn.
    pub fn must_replay(&self) -> bool {
        matches!(self.replay, ReplayPolicy::MustReplayUnmodified)
    }

    /// Whether this artifact is safe to surface to end users under the
    /// given visibility policy.
    pub fn user_visible_under(&self, host_policy: ThinkingVisibility) -> bool {
        use ThinkingKind::*;
        use ThinkingVisibility::*;
        match (host_policy, self.kind) {
            // Never show redacted/opaque, regardless of host policy.
            (_, RedactedOpaque) | (_, ProviderOpaque) => false,
            (UserVisible, _) => true,
            (SummaryAllowed, SummaryText) => true,
            _ => false,
        }
    }
}

// ============================================================================
// Capability declaration
// ============================================================================

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSupport {
    /// Adapter has no native reasoning. `ThinkingMode::Auto` degrades to `Off`.
    #[default]
    None,
    /// Native reasoning available, but replay across tool-use turns is not
    /// required (e.g. OpenAI Chat Completions).
    NoReplay,
    /// Full support including `MustReplayUnmodified` round-trip (Anthropic
    /// extended thinking, OpenAI Responses API with reasoning items).
    FullReplay,
}
