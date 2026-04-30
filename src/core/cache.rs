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
