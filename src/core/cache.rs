//! Prompt cache policy.
//!
//! Prompt caching is a provider-side feature that lets clients mark stable
//! prefixes of a request so the provider can reuse KV cache across turns.
//! Done right, it saves ~90% on input token cost and cuts latency. Policies:
//!
//! - `Disabled`: send no cache hints. Most providers still do some implicit
//!   caching but without explicit markers the hit rate is lower.
//! - `Auto`: the adapter decides where to place breakpoints. Conservative
//!   implementations mark the system prompt, the tool schemas, and the
//!   oldest user turn — giving a stable "system + tools + first turn" prefix
//!   that subsequent turns hit.
//!
//! Core's job is just to carry the policy and accumulate returned cache
//! usage stats. Wire-format translation lives in each provider's adapter.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CachePolicy {
    /// No explicit cache hints. Providers with automatic server-side caching
    /// (OpenAI today) still benefit; providers that require explicit markers
    /// (Anthropic) won't cache.
    #[default]
    Disabled,

    /// Let the adapter mark stable prefixes (system, tools, oldest user turn).
    Auto,
}

/// How `Runner` populates `ModelRequest::prompt_cache_key`.
///
/// The provider uses this string as a routing-affinity hint: requests
/// sharing the same key (and the same prefix bytes) are sent to the same
/// engine replica, where the cached KV is already warm. **Picking the right
/// granularity is the difference between a cold first turn and an instant
/// hit on every new session of the same agent.**
///
/// Empirical numbers on `openai/gpt-5.4-nano` via OpenRouter (5-turn loop,
/// ~7.5k token prompt at turn 5):
/// - `Session` (per-conversation): turn-1 cache_read = 0; turn-5 ratio = 78%.
/// - `PrefixHash` (per-agent-config): turn-1 cache_read = 2816; turn-5
///   ratio = 92%. Same prefix bytes, only the routing key differs.
///
/// The trade-off is OpenAI's published per-(prefix, key) throughput
/// ceiling of ~15 RPM. For multi-tenant servers serving many concurrent
/// users, `Session` keeps each user's traffic on their own replica;
/// `PrefixHash` would funnel them all to one and hit the limit.
#[derive(Clone, Debug, Default)]
pub enum CacheKeyStrategy {
    /// Hash of the final stable prompt prefix plus canonical tool schemas.
    /// **Default.** Best for CLIs and single-user agents where every session
    /// shares the same identity and cross-session cache reuse is the goal.
    #[default]
    PrefixHash,
    /// Use the per-conversation `state.session_id`. Best for high-RPS
    /// multi-tenant servers where each session must route independently
    /// to stay under the per-key throughput ceiling.
    Session,
    /// A caller-supplied stable key. Use for fleet-level keys (e.g. one
    /// key per deployed agent name) when neither default fits.
    Fixed(String),
    /// Don't send a `prompt_cache_key` at all. Fall back to the provider's
    /// automatic prefix routing.
    None,
}
