//! Offline: Runner auto-compaction wiring.
//!
//! Preload a bloated history, let Runner take one step, assert that
//! compaction fired BEFORE the model turn and that the Event::HistoryCompacted
//! was persisted.

use std::sync::Arc;

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::Content;
use muagent::sessions::compaction::{CompactionBudget, RunnerCompactor, SummaryCompaction};
use muagent::storage::MemorySessionStore;

struct NoopExecutor;

#[async_trait]
impl ToolExecutor for NoopExecutor {
    async fn execute(
        &self,
        _c: &PendingCall,
        _ctx: &ToolContext,
        _t: CancelToken,
    ) -> Result<ToolResult, muagent::core::error::ToolExecutorError> {
        Ok(ToolResult::ok("ok"))
    }
    fn idempotency_for(&self, _c: &PendingCall) -> Idempotency {
        Idempotency::Idempotent
    }
}

struct FailingCompactor;

#[async_trait]
impl Compactor for FailingCompactor {
    async fn maybe_compact(
        &self,
        _state: &mut RunState,
        _system_prompt: &str,
        _cancel: CancelToken,
    ) -> Result<Option<CompactionEvent>, RuntimeError> {
        Err(ModelError::Transient("summarizer temporarily unavailable".into()).into())
    }
}

fn long_text(n: usize) -> String {
    "x".repeat(n)
}

#[tokio::test]
async fn runner_auto_compacts_before_model_turn() {
    // Summarizer: a separate model that returns "SUMMARY" deterministically.
    let summarizer = Arc::new(CannedModel::named(
        "summarizer",
        vec![reply::text("SUMMARY")],
    ));

    // Main model: returns a final short text, ends the turn.
    let main = Arc::new(CannedModel::named("main", vec![reply::text("done")]));

    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(CompactionBudget {
            max_tokens: 5_000,
            threshold_ratio: 0.5, // fires at ~2500 estimated tokens
            keep_tail_turns: 2,
            summary_target_chars: 500,
            ..CompactionBudget::default()
        }),
        summarizer.clone(),
    ));

    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let runner = Runner::builder()
        .model(main.clone())
        .tools(Arc::new(NoopExecutor))
        .store(store.clone())
        .compactor(compactor)
        .base_system_prompt("You are terse.")
        .build()
        .unwrap();

    // Preload 10 user turns with bloated content → well over 2500 tokens.
    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    for _ in 0..10 {
        state.history.push(Message::User {
            content: Content::text(long_text(1000)),
        });
        state.history.push(Message::Assistant {
            content: Content::text(long_text(1000)),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    let before = state.history.len();

    // Drive until terminal.
    state.step = Step::Ready;
    let mut all_events: Vec<Event> = Vec::new();
    for _ in 0..8 {
        let out = runner.step(&mut state).await.expect("step");
        all_events.extend(out.events);
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
    }

    // History shorter now.
    let after = state.history.len();
    assert!(
        after < before,
        "expected compaction to shorten history: before={before} after={after}"
    );

    // Event was emitted.
    let hc = all_events.iter().find_map(|e| match e {
        Event::HistoryCompacted {
            replaced_turns,
            replaced_messages,
            saved_tokens_estimate,
            checkpoint_id,
            summary_message_id,
            first_kept_message_id,
            ..
        } => Some((
            *replaced_turns,
            *replaced_messages,
            *saved_tokens_estimate,
            checkpoint_id.clone(),
            summary_message_id.clone(),
            first_kept_message_id.clone(),
        )),
        _ => None,
    });
    let (turns, msgs, saved, checkpoint_id, summary_message_id, first_kept_message_id) =
        hc.expect("HistoryCompacted event not emitted");
    assert!(turns >= 8, "should have compacted ~8 turns, got {turns}");
    assert!(msgs >= 16, "expected ~16 replaced messages, got {msgs}");
    assert!(saved > 0);
    assert!(checkpoint_id
        .as_deref()
        .is_some_and(|id| id.starts_with('c')));
    assert!(summary_message_id
        .as_deref()
        .is_some_and(|id| id.starts_with('m')));
    assert!(first_kept_message_id
        .as_deref()
        .is_some_and(|id| id.starts_with('m')));

    // Summarizer must have been called exactly once.
    // (can't inspect directly, but if no reply remains it was consumed)
    assert_eq!(
        summarizer.remaining(),
        0,
        "summarizer should have been called once"
    );
}

#[tokio::test]
async fn runner_skips_compaction_when_under_budget() {
    // Empty queue → if called, panic with name. Exactly the assertion we want.
    let summarizer = Arc::new(CannedModel::named("summarizer", vec![]));
    let main = Arc::new(CannedModel::named("main", vec![reply::text("short")]));

    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(CompactionBudget::default()), // 156k
        summarizer.clone(),
    ));

    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let runner = Runner::builder()
        .model(main.clone())
        .tools(Arc::new(NoopExecutor))
        .store(store.clone())
        .compactor(compactor)
        .build()
        .unwrap();

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history.push(Message::User {
        content: Content::text("hi"),
    });
    state.step = Step::Ready;

    let mut all_events: Vec<Event> = Vec::new();
    for _ in 0..4 {
        let out = runner.step(&mut state).await.unwrap();
        all_events.extend(out.events);
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
    }

    // No HistoryCompacted event.
    assert!(!all_events
        .iter()
        .any(|e| matches!(e, Event::HistoryCompacted { .. })));
}

#[tokio::test]
async fn runner_continues_when_optional_compactor_fails() {
    let main = Arc::new(CannedModel::named("main", vec![reply::text("done")]));
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let runner = Runner::builder()
        .model(main.clone())
        .tools(Arc::new(NoopExecutor))
        .store(store)
        .compactor(Arc::new(FailingCompactor))
        .build()
        .unwrap();

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history.push(Message::User {
        content: Content::text("continue even if summary service is down"),
    });
    state.step = Step::Ready;

    let mut all_events: Vec<Event> = Vec::new();
    for _ in 0..4 {
        let out = runner.step(&mut state).await.unwrap();
        all_events.extend(out.events);
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
    }

    assert!(matches!(state.step, Step::Done { .. }));
    assert_eq!(main.remaining(), 0, "main model should still run");
    assert!(!all_events
        .iter()
        .any(|e| matches!(e, Event::HistoryCompacted { .. })));
    assert!(!all_events.iter().any(|e| matches!(
        e,
        Event::SessionEnd { ok: false, .. } | Event::ErrorRaised { .. }
    )));
}
