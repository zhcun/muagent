//! Core-side `Compactor` trait.
//!
//! History compaction is a Shell-layer policy, but the **decision to run it**
//! lives in the Runner (so it can be invoked right before a model turn, when
//! we know exactly what's about to be sent over the wire).
//!
//! Runner holds `Option<Arc<dyn Compactor>>`; if set, it calls `maybe_compact`
//! at the start of each `on_model_turn` and emits `Event::HistoryCompacted`
//! when the strategy actually shortens history.

use async_trait::async_trait;

use crate::core::cancel::CancelToken;
use crate::core::error::RuntimeError;
use crate::core::run_state::RunState;

/// What a compactor did. Feed this back as an event.
#[derive(Clone, Debug)]
pub struct CompactionEvent {
    /// How many user-turn-aligned chunks were collapsed into a summary.
    pub replaced_turns: usize,
    /// How many raw messages got replaced by the summary.
    pub replaced_messages: usize,
    /// Rough number of tokens saved vs. the pre-compaction history.
    pub saved_tokens_estimate: u32,
    /// Stable checkpoint metadata for replay/debug timelines.
    pub checkpoint_id: Option<String>,
    pub summary_message_id: Option<String>,
    pub first_kept_message_id: Option<String>,
}

#[async_trait]
pub trait Compactor: Send + Sync {
    /// Called by Runner before each model turn.
    ///
    /// Implementations should be a no-op (`Ok(None)`) when nothing needs
    /// compacting. On success, `state.history` is mutated in place.
    ///
    /// `system_prompt` is what Runner is about to send as system; the
    /// compactor gets it for token-budget estimation.
    async fn maybe_compact(
        &self,
        state: &mut RunState,
        system_prompt: &str,
        cancel: CancelToken,
    ) -> Result<Option<CompactionEvent>, RuntimeError>;

    /// Drop bookkeeping that no longer corresponds to live history (e.g.
    /// compaction checkpoints whose summary message was itself rolled out
    /// of `history`). Called by Runner inside `commit` so the persisted
    /// state stays self-consistent.
    ///
    /// Default no-op: a compactor that doesn't track durable bookkeeping
    /// has nothing to retain.
    fn retain_active_state(&self, _state: &mut RunState) {}

    /// Validate compaction-specific invariants on `state` before persist.
    /// Called by Runner after `RunState::validate_history_identity`. Keeps
    /// compaction-shaped consistency checks out of `core` proper.
    ///
    /// Default `Ok(())`.
    fn validate_state(&self, _state: &RunState) -> Result<(), String> {
        Ok(())
    }
}
