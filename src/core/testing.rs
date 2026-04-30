//! Opt-in test scaffolding (`testing` feature).
//!
//! Five separate test files each defined their own `MockModel` with the same
//! "pop from a canned queue" body. Centralising the pattern as `CannedModel`
//! + a `reply` builder module saves ~150 LoC and removes the per-file drift
//!   ("does this MockModel panic on empty or return Err?").
//!
//! # Scope
//!
//! Intentionally **only** the bits that were truly duplicated:
//! - [`CannedModel`] — a `ModelAdapter` that pops a pre-supplied queue
//! - [`reply`] — short builders for the most common `ModelReply` shapes
//!
//! Per-test `ToolExecutor` mocks are NOT extracted: each test wants
//! different behavior (echo, no-op, hashmap lookup, …) and at ~10 lines
//! each, inlining keeps the test readable end-to-end. Heavy abstraction in
//! test code generally hurts more than it helps.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::model::{LlmCaps, ModelAdapter, ModelReply, ModelRequest};

/// `ModelAdapter` that returns pre-built `ModelReply`s in FIFO order.
///
/// On empty queue it **panics with a name** rather than returning an error —
/// in tests this surfaces "you wired N replies but the runner asked for N+1"
/// directly at the point of failure, with no `unwrap`/`?` chain to follow.
pub struct CannedModel {
    name: String,
    caps: LlmCaps,
    replies: Mutex<VecDeque<ModelReply>>,
}

impl CannedModel {
    /// Construct an unnamed canned model. Use [`Self::named`] for tests
    /// that build multiple models (e.g. main + summarizer) so panics name
    /// the offender.
    pub fn new(replies: Vec<ModelReply>) -> Self {
        Self::named("model", replies)
    }

    pub fn named(name: impl Into<String>, replies: Vec<ModelReply>) -> Self {
        Self {
            name: name.into(),
            caps: LlmCaps {
                native_tool_use: true,
                streaming: false,
                ctx_len: 8192,
                ..Default::default()
            },
            replies: Mutex::new(replies.into()),
        }
    }

    pub fn with_caps(mut self, caps: LlmCaps) -> Self {
        self.caps = caps;
        self
    }

    /// Convenience: how many queued replies remain (asserting `0` at the
    /// end of a test catches "model was over-prepared" mistakes).
    pub fn remaining(&self) -> usize {
        self.replies.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelAdapter for CannedModel {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(
        &self,
        _req: ModelRequest,
        _cancel: CancelToken,
    ) -> Result<ModelReply, ModelError> {
        let mut q = self.replies.lock().unwrap();
        match q.pop_front() {
            Some(r) => Ok(r),
            None => panic!("CannedModel `{}`: ran out of canned replies", self.name),
        }
    }
}

/// `ModelReply` builders for the three patterns that show up in every
/// test: text-only assistant turn, assistant turn with tool calls, and
/// final text (alias of `text` — kept separate for readability of intent).
pub mod reply {
    use crate::core::model::{ModelReply, TokenUsage};
    use crate::core::tool::PendingCall;

    /// Plain text reply, no tool calls. Closes the turn loop.
    pub fn text(s: impl Into<String>) -> ModelReply {
        ModelReply {
            text: s.into(),
            tool_calls: vec![],
            usage: usage(10, 5),
            thinking: vec![],
        }
    }

    /// Assistant turn that requests tool calls.
    pub fn with_calls(text: impl Into<String>, calls: Vec<PendingCall>) -> ModelReply {
        ModelReply {
            text: text.into(),
            tool_calls: calls,
            usage: usage(20, 10),
            thinking: vec![],
        }
    }

    /// Alias of `text` — lets a test read `reply::done("here you go")`
    /// when intent is "this closes the conversation".
    pub fn done(s: impl Into<String>) -> ModelReply {
        text(s)
    }

    fn usage(prompt: u32, completion: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cost_usd: None,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            thinking_tokens: 0,
        }
    }
}

/// Shared `SessionStore` contract tests. Run **the same suite** against
/// every backend (memory / jsonl / future stores) so they can't drift away
/// from the trait's documented semantics.
///
/// Why this exists: pre-fix, `MemorySessionStore` accepted out-of-order
/// writes that `JsonlSessionStore` would reject. Tests against memory
/// passed; production sessions on JSONL hit `StaleState` errors. A shared
/// suite makes "memory weaker than prod" structurally impossible.
///
/// Usage in a backend's test file:
/// ```ignore
/// #[tokio::test]
/// async fn contract_save_delta_rejects_stale_state() {
///     crate::core::testing::store_contract::save_delta_rejects_stale_state(
///         MyStore::new()
///     ).await;
/// }
/// ```
pub mod store_contract {
    use uuid::Uuid;

    use crate::core::error::StoreError;
    use crate::core::event::Event;
    use crate::core::run_state::RunState;
    use crate::core::store::SessionStore;

    /// Stale-state CAS: writing with `event_seq` < the stored value must
    /// return `StoreError::StaleState`. Without this every store would
    /// silently accept time-travel writes.
    pub async fn save_delta_rejects_stale_state<S: SessionStore>(store: S) {
        let mut s1 = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
        s1.event_seq = 5;
        store.save_delta(&s1, &[]).await.unwrap();

        let mut s_old = s1.clone();
        s_old.event_seq = 3;
        let err = store.save_delta(&s_old, &[]).await.unwrap_err();
        match err {
            StoreError::StaleState { expected, actual } => {
                assert_eq!(expected, 3);
                assert_eq!(actual, 5);
            }
            e => panic!("expected StaleState, got {e:?}"),
        }
    }

    /// Re-saving the same state with the same seq is allowed (idempotent
    /// no-op refresh). Forward progress requires matching events:
    /// `state.event_seq - prev.state.event_seq == events.len()`. The seq is
    /// not a free counter — each increment must correspond to an event.
    pub async fn save_delta_accepts_idempotent_and_forward<S: SessionStore>(store: S) {
        let mut s = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
        s.event_seq = 5;
        store.save_delta(&s, &[]).await.unwrap();
        // Same seq, same content → idempotent no-op
        store.save_delta(&s, &[]).await.unwrap();
        // Advancing seq REQUIRES matching events (this is what defines the
        // seq advance — Runner calls `next_seq` only when allocating an
        // event id, so state.event_seq always matches the highest event).
        s.event_seq = 6;
        store
            .save_delta(&s, &[Event::UserMessage { seq: 6 }])
            .await
            .unwrap();
    }

    /// `load_run` after `save_delta` must return the same state.
    pub async fn save_then_load_roundtrip<S: SessionStore>(store: S) {
        let mut s = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
        s.event_seq = 1;
        s.usage.turns = 7;
        let id = s.run_id;
        store
            .save_delta(&s, &[Event::UserMessage { seq: 1 }])
            .await
            .unwrap();
        let loaded = store.load_run(id).await.unwrap().expect("must find");
        assert_eq!(loaded.run_id, id);
        assert_eq!(loaded.event_seq, 1);
        assert_eq!(loaded.usage.turns, 7);
    }

    /// Events accumulate (append-only) and `query_events` returns them
    /// in seq order.
    pub async fn query_events_returns_in_seq_order<S: SessionStore>(store: S) {
        let mut s = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
        let id = s.run_id;
        s.event_seq = 1;
        store
            .save_delta(&s, &[Event::UserMessage { seq: 1 }])
            .await
            .unwrap();
        s.event_seq = 2;
        store
            .save_delta(
                &s,
                &[Event::AssistantMessage {
                    text: "hi".into(),
                    seq: 2,
                }],
            )
            .await
            .unwrap();

        let evs = store.query_events(id, 0).await.unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], Event::UserMessage { seq: 1 }));
        assert!(matches!(evs[1], Event::AssistantMessage { seq: 2, .. }));

        // since_seq filter
        let evs = store.query_events(id, 1).await.unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], Event::AssistantMessage { seq: 2, .. }));
    }

    /// `kv_put` / `kv_get` round-trip per session.
    pub async fn kv_put_get_roundtrip<S: SessionStore>(store: S) {
        let sid = Uuid::new_v4();
        store.kv_put(sid, "k", b"v").await.unwrap();
        let got = store.kv_get(sid, "k").await.unwrap();
        assert_eq!(got.as_deref(), Some(b"v".as_slice()));
        // Different session_id is isolated
        let other = Uuid::new_v4();
        let got = store.kv_get(other, "k").await.unwrap();
        assert!(got.is_none());
    }
}
