//! ModelAdapter trait + request/reply types。

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::core::cache::CachePolicy;
use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::thinking::{ThinkingArtifact, ThinkingConfig, ThinkingSupport};
use crate::core::tool::{PendingCall, ToolDescriptor};
use crate::core::types::Message;

#[derive(Clone, Debug, Default)]
pub struct LlmCaps {
    pub native_tool_use: bool,
    pub json_schema_mode: bool,
    pub vision: bool,
    pub streaming: bool,
    pub ctx_len: u32,
    /// Provider-declared support for prompt prefix caching.
    pub prompt_cache: bool,
    /// Provider-declared support for reasoning / thinking protocol.
    /// `ThinkingConfig::Auto` silently downgrades to `Off` when `None`.
    pub thinking: ThinkingSupport,
}

/// Normalized token accounting across providers.
///
/// **Adapter contract** (consume this when adding a new ModelAdapter):
///
/// - [`prompt_tokens`](Self::prompt_tokens) — TOTAL prompt tokens for billing,
///   *including* any cached portion. Anthropic-style APIs that report
///   `input_tokens` + `cache_creation` + `cache_read` as disjoint numbers
///   must SUM them. OpenAI/Google APIs already include the cached portion in
///   their top-level `prompt_tokens` / `prompt_token_count`, so report as-is.
/// - [`cache_read_tokens`](Self::cache_read_tokens) — subset of `prompt_tokens`
///   served from cache (cheap). 0 if provider has no cache or no hits.
/// - [`cache_write_tokens`](Self::cache_write_tokens) — input tokens written
///   to cache *this turn*. Only Anthropic reports this; OpenAI/Google use
///   transparent caches with no per-turn write accounting → always 0.
/// - [`completion_tokens`](Self::completion_tokens) — output tokens. On OpenAI
///   reasoning models, hidden reasoning tokens are NOT included here (they
///   live in `thinking_tokens`). On Anthropic extended thinking, reasoning
///   IS included but ALSO tracked separately in `thinking_tokens`.
/// - [`thinking_tokens`](Self::thinking_tokens) — reasoning tokens billed
///   this turn (visibility into the cost of `<thinking>` content).
///
/// Invariant: `cache_read_tokens <= prompt_tokens`. Adapters violating this
/// indicate a parsing bug.
#[derive(Clone, Debug, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cost_usd: Option<f64>,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub thinking_tokens: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ModelRequest {
    /// L0 + L1 system prefix. Stable, cacheable.
    pub system: String,
    /// L2 runtime context (time, turn, host facts). Re-rendered each turn;
    /// adapters send this AFTER `system` with no cache marker so it doesn't
    /// invalidate the prefix cache.
    pub runtime_context: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub temperature: Option<f32>,
    pub stream: bool,
    /// Prompt-cache policy. See `CachePolicy` docs. Default `Disabled`.
    pub cache: CachePolicy,
    /// Reasoning / thinking config. See `ThinkingConfig` docs. Default `Auto`
    /// on providers that advertise `ThinkingSupport != None`, else ignored.
    pub thinking: ThinkingConfig,
    /// Routing-affinity hint for prompt-cache backends. Providers that
    /// support a stable per-session cache key (OpenAI's `prompt_cache_key`
    /// on Chat Completions and Responses; OpenRouter pass-through) use it
    /// to land subsequent requests with the same prefix on the same engine
    /// replica. OpenAI publishes 60% → 87% hit-rate on real workloads when
    /// this is set (see Cookbook *Prompt Caching 201*). Adapters that don't
    /// understand the hint MUST ignore it.
    ///
    /// Runner defaults this to a hash of the final stable prompt prefix plus
    /// canonical tool schemas, so sessions with the same agent configuration
    /// can reuse a warm prefix. Hosts with high concurrent traffic can switch
    /// to `CacheKeyStrategy::Session` to spread load across affinity keys.
    pub prompt_cache_key: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelReply {
    pub text: String,
    pub tool_calls: Vec<PendingCall>,
    pub usage: TokenUsage,
    /// Reasoning artifacts emitted by the provider. Adapters attach these
    /// so Runner can preserve them on the assistant turn for replay
    /// (see `ReplayPolicy::MustReplayUnmodified`).
    pub thinking: Vec<ThinkingArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    Reset,
}

#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn caps(&self) -> LlmCaps;

    async fn turn(&self, req: ModelRequest, cancel: CancelToken) -> Result<ModelReply, ModelError>;

    async fn turn_stream(
        &self,
        req: ModelRequest,
        cancel: CancelToken,
        stream: Option<mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ModelReply, ModelError> {
        let _ = stream;
        self.turn(req, cancel).await
    }
}
