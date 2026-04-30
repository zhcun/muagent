//! M0 Core 必过的 5 条验收测试。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::step::{PauseReason, Step};
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::{Content, Message, ObsKind};
use muagent::storage::MemorySessionStore;
use serde_json::json;
use uuid::Uuid;

// ================ Mock ToolExecutor ================

struct MockTools {
    results: Mutex<std::collections::HashMap<String, ToolResult>>,
    at_most_once: std::collections::HashSet<String>,
}

impl MockTools {
    fn new() -> Self {
        Self {
            results: Mutex::new(std::collections::HashMap::new()),
            at_most_once: std::collections::HashSet::new(),
        }
    }
    fn with_result(self, name: &str, r: ToolResult) -> Self {
        self.results.lock().unwrap().insert(name.to_string(), r);
        self
    }
    fn with_at_most_once(mut self, name: &str) -> Self {
        self.at_most_once.insert(name.to_string());
        self
    }
}

#[async_trait]
impl ToolExecutor for MockTools {
    async fn execute(
        &self,
        call: &PendingCall,
        _ctx: &ToolContext,
        _cancel: CancelToken,
    ) -> Result<ToolResult, muagent::core::error::ToolExecutorError> {
        let r = self.results.lock().unwrap().get(&call.tool_name).cloned();
        Ok(r.unwrap_or_else(|| {
            ToolResult::err(format!("no mock for {}", call.tool_name), false, None)
        }))
    }
    fn idempotency_for(&self, call: &PendingCall) -> Idempotency {
        if self.at_most_once.contains(&call.tool_name) {
            Idempotency::AtMostOnce
        } else {
            Idempotency::Idempotent
        }
    }
}

// ================ Helpers ================

fn pc(id: &str, name: &str) -> PendingCall {
    PendingCall::new(id, name, json!({}))
}
fn new_state() -> RunState {
    RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0)
}

fn build_runner(
    model: Arc<dyn ModelAdapter>,
    tools: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,
) -> Runner {
    Runner::builder()
        .model(model)
        .tools(tools)
        .store(store)
        .tools_provider(|_state: &RunState| ActiveToolSet::default())
        .build()
        .expect("runner build")
}

fn build_runner_with_retry(
    model: Arc<dyn ModelAdapter>,
    tools: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,
    retry: RetryPolicy,
) -> Runner {
    Runner::builder()
        .model(model)
        .tools(tools)
        .store(store)
        .tools_provider(|_state: &RunState| ActiveToolSet::default())
        .retry_policy(retry)
        .build()
        .expect("runner build")
}

async fn drive_until_terminal(runner: &Runner, state: &mut RunState, max: usize) {
    for _ in 0..max {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            break;
        }
        runner.step(state).await.unwrap();
    }
}

// ================ Test 1:ToolBatch multi-call sequential ================

#[tokio::test]
async fn t1_tool_batch_multi_call_sequential() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls(
            "use 3 tools",
            vec![pc("a", "t_a"), pc("b", "t_b"), pc("c", "t_c")],
        ),
        reply::text("done"),
    ]));
    let tools = Arc::new(
        MockTools::new()
            .with_result("t_a", ToolResult::ok("A"))
            .with_result("t_b", ToolResult::ok("B"))
            .with_result("t_c", ToolResult::ok("C")),
    );
    let runner = build_runner(model, tools, store.clone());

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    drive_until_terminal(&runner, &mut state, 50).await;
    assert!(matches!(state.step, Step::Done { .. }));

    let tr_count = state
        .history
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(tr_count, 3);

    let events = store.query_events(state.run_id, 0).await.unwrap();
    let starts = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCallStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCallEnd { .. }))
        .count();
    assert_eq!(starts, 3);
    assert_eq!(ends, 3);
}

// ================ Test 2:AtMostOnce 中断不重跑 ================

#[tokio::test]
async fn t2_at_most_once_interrupt() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls("use x", vec![pc("x", "t_x")]),
        reply::text("verified"),
    ]));
    let tools = Arc::new(
        MockTools::new()
            .with_result("t_x", ToolResult::ok("X"))
            .with_at_most_once("t_x"),
    );
    let runner = build_runner(model, tools, store.clone());

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("run x"),
            },
        )
        .await
        .unwrap();

    // Ready → ModelTurn → ToolBatch
    runner.step(&mut state).await.unwrap();
    runner.step(&mut state).await.unwrap();

    // Simulate: thaw at Step::ToolIntent(上次 crash 在 AtMostOnce tool 执行中)
    let call = pc("x", "t_x");
    state.step = Step::ToolIntent {
        call: call.clone(),
        intent_ms: 0,
    };

    runner.step(&mut state).await.unwrap(); // recovery
    assert!(matches!(state.step, Step::ModelTurn));
    match state.history.last().unwrap() {
        Message::ToolResult { result, .. } => {
            assert!(!result.ok);
            assert!(result.text().to_lowercase().contains("interrupted"));
        }
        _ => panic!("expected injected tool_result"),
    }

    // Continue: ModelTurn "verified" → Done
    runner.step(&mut state).await.unwrap();
    assert!(matches!(state.step, Step::Done { .. }));
}

// ================ Test 3:cancel → Paused ================

#[tokio::test]
async fn t3_cancel_triggers_paused() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![reply::text("ok")]));
    let tools = Arc::new(MockTools::new());
    let runner = build_runner(model, tools, store);

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("x"),
            },
        )
        .await
        .unwrap();

    runner.cancel();
    let out = runner.step(&mut state).await.unwrap();
    assert!(matches!(
        &state.step,
        Step::Paused {
            reason: PauseReason::HostRequested
        }
    ));
    assert!(out.events.iter().any(|e| matches!(e, Event::Paused { .. })));
}

// Regression: cancel must propagate end-to-end into Tool::run. Pre-fix the
// executor accepted a CancelToken parameter and then immediately threw it
// away — Tool::run had no cancel parameter at all.
#[tokio::test]
async fn t3a_cancel_reaches_tool() {
    use muagent::core::cancel::CancelToken;
    use muagent::core::prelude::*;
    use serde_json::Value;

    struct CancelObserver {
        desc: ToolDescriptor,
        observed: std::sync::Mutex<Option<bool>>,
    }
    #[async_trait::async_trait]
    impl Tool for CancelObserver {
        fn descriptor(&self) -> &ToolDescriptor {
            &self.desc
        }
        async fn run_ctxless(&self, _args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
            *self.observed.lock().unwrap() = Some(cancel.triggered());
            Ok(ToolOk::text("ran"))
        }
    }

    let observer = Arc::new(CancelObserver {
        desc: ToolDescriptor {
            name: "obs".into(),
            description: "".into(),
            schema_json: json!({"type":"object"}),
            timeout: std::time::Duration::from_secs(1),
            max_out_tokens: 64,
            concurrency: muagent::core::tool::Concurrency::Parallel,
            side_effects: muagent::core::tool::SideEffects::ReadOnly,
            idempotency: Idempotency::Idempotent,
        },
        observed: std::sync::Mutex::new(None),
    });
    let registry = Arc::new(muagent::core::tool::CapabilityRegistry::new());
    registry.register(observer.clone());
    let executor = Arc::new(muagent::runtime::executor::DefaultToolExecutor::new(
        registry,
    ));

    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls("call obs", vec![pc("c1", "obs")]),
        reply::text("done"),
    ]));
    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .build()
        .unwrap();

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("go"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 20).await;

    let observed = *observer.observed.lock().unwrap();
    assert_eq!(
        observed,
        Some(false),
        "Tool::run must receive a CancelToken (not None, and not pre-triggered)"
    );
}

// Regression: cancel must NOT be sticky across rounds. Pre-fix, once host
// called runner.cancel(), every subsequent submit_user_message → step would
// immediately Pause because the cancel token was shared and never reset.
#[tokio::test]
async fn t3b_cancel_not_sticky_across_rounds() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![
        reply::text("first round"),
        reply::text("second round"),
    ]));
    let tools = Arc::new(MockTools::new());
    let runner = build_runner(model, tools, store);

    // Round 1: cancel mid-flight, expect Paused.
    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("round 1"),
            },
        )
        .await
        .unwrap();
    runner.cancel();
    runner.step(&mut state).await.unwrap();
    assert!(
        matches!(&state.step, Step::Paused { .. }),
        "round 1 must Pause after cancel"
    );

    // Round 2: brand new submit. Should NOT inherit the cancel.
    state.step = Step::Done {
        final_text: "ack pause".into(),
    }; // simulate host clearing pause
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("round 2"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 10).await;
    assert!(
        matches!(&state.step, Step::Done { .. }),
        "round 2 must complete normally; got {:?}",
        state.step
    );
}

// ================ Test 4:Event persistence + replay ================

#[tokio::test]
async fn t4_event_persistence_and_replay() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![reply::text("one")]));
    let tools = Arc::new(MockTools::new());
    let runner = build_runner(model, tools, store.clone());

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hello"),
            },
        )
        .await
        .unwrap();

    drive_until_terminal(&runner, &mut state, 10).await;
    assert!(matches!(state.step, Step::Done { .. }));

    let all = store.query_events(state.run_id, 0).await.unwrap();
    assert!(all.len() >= 3);
    for pair in all.windows(2) {
        assert!(pair[0].seq() < pair[1].seq(), "seq must be monotonic");
    }

    // At-least-once:injector 模拟保存失败;Runner 不会推进存储端 state
    store.inject_crash_after_save(0); // very next save will fail

    // Snapshot state before the failing submit; we expect rollback.
    let prev_event_seq = state.event_seq;
    let prev_history_len = state.history.len();
    let prev_step = state.step.clone();

    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("again"),
            },
        )
        .await
        .unwrap_err();

    // Transactional contract: failed submit must leave `state` unchanged
    // (otherwise next persist gets StaleState forever).
    assert_eq!(state.event_seq, prev_event_seq, "event_seq must roll back");
    assert_eq!(
        state.history.len(),
        prev_history_len,
        "history must roll back"
    );
    assert!(
        matches!(
            (&state.step, &prev_step),
            (Step::Done { .. }, Step::Done { .. })
        ),
        "step must roll back"
    );

    store.disable_crash_injection();
    let all2 = store.query_events(state.run_id, 0).await.unwrap();
    // Event count 没有因失败的 save 而增加
    assert_eq!(all2.len(), all.len());

    // Crucially: after the failed submit, a fresh submit should SUCCEED.
    // Pre-fix this would CAS-fail with StaleState because state.event_seq
    // had been left advanced past disk.
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("retry"),
            },
        )
        .await
        .expect("submit must succeed after rollback from previous failure");
}

// ================ Test 5:schema migration ================

#[tokio::test]
async fn t5_schema_migration() {
    let current = serde_json::to_value(new_state()).unwrap();
    let got = RunState::migrate_from(current, CURRENT_SCHEMA).unwrap();
    assert_eq!(got.schema_version, CURRENT_SCHEMA);

    let err = RunState::migrate_from(json!({}), CURRENT_SCHEMA + 10).unwrap_err();
    assert!(matches!(
        err,
        muagent::core::error::StoreError::Incompatible { .. }
    ));

    let err = RunState::migrate_from(json!({}), 0).unwrap_err();
    assert!(matches!(err, muagent::core::error::StoreError::Corrupt(_)));
}

#[test]
fn history_identity_validation_catches_parallel_ledger_drift() {
    let mut state = new_state();
    state.history.push(Message::User {
        content: Content::text("one"),
    });
    state.history.push(Message::Observation {
        kind: ObsKind::Summary,
        text: "summary".into(),
    });
    state.history_ids = vec!["m000000000001".into()];

    let err = state.validate_history_identity().unwrap_err();
    assert!(err.contains("history length"));

    state.ensure_history_ids();
    state.history_ids[1] = state.history_ids[0].clone();
    let err = state.validate_history_identity().unwrap_err();
    assert!(err.contains("duplicate history id"));
}

#[test]
fn history_identity_validation_accepts_active_checkpoint_ranges() {
    let mut state = new_state();
    state.history.push(Message::Observation {
        kind: ObsKind::Summary,
        text: "summary".into(),
    });
    state.history_ids.push("m000000000010".into());
    state.compaction_checkpoints.push(CompactionCheckpoint {
        checkpoint_id: "c000000000001".into(),
        summary_message_id: "m000000000010".into(),
        removed_message_range: MessageIdRange {
            first_message_id: Some("m000000000001".into()),
            last_message_id: Some("m000000000009".into()),
            count: 9,
        },
        summary_input_message_range: MessageIdRange {
            first_message_id: Some("m000000000001".into()),
            last_message_id: Some("m000000000009".into()),
            count: 9,
        },
        removed_message_ids: vec![],
        summary_input_message_ids: vec![],
        first_kept_message_id: Some("m000000000011".into()),
        pinned_message_ids: vec![],
        replaced_turns: 4,
        replaced_messages: 9,
        tokens_before: 10_000,
        tokens_after: 1_000,
    });

    state.validate_history_identity().unwrap();
}

#[tokio::test]
async fn empty_model_reply_is_rejected_before_history_write() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![reply::text("")]));
    let tools = Arc::new(MockTools::new());
    let runner = build_runner_with_retry(model, tools, store.clone(), RetryPolicy::never());

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("empty?"),
            },
        )
        .await
        .unwrap();

    runner.step(&mut state).await.unwrap(); // Ready -> ModelTurn
    let out = runner.step(&mut state).await.unwrap();
    assert!(matches!(state.step, Step::Failed { .. }));
    assert!(out.events.iter().any(|e| matches!(
        e,
        Event::ErrorRaised { class, .. } if class == "provider_transient"
    )));
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, Event::SessionEnd { ok: false, .. })));
    assert_eq!(state.history.len(), 1, "empty assistant must not be stored");

    let events = store.query_events(state.run_id, 0).await.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ErrorRaised { .. })));
}

#[tokio::test]
async fn tool_set_provider_panic_does_not_stop_model_turn() {
    let store = Arc::new(MemorySessionStore::new());
    let model = Arc::new(CannedModel::new(vec![reply::text("done")]));
    let runner = Runner::builder()
        .model(model.clone())
        .tools(Arc::new(MockTools::new()))
        .store(store)
        .tools_provider(|_state: &RunState| -> ActiveToolSet { panic!("provider failed") })
        .build()
        .unwrap();

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 4).await;

    assert!(matches!(state.step, Step::Done { .. }));
    assert_eq!(model.remaining(), 0, "main model should still be called");
}

#[tokio::test]
async fn protocol_error_call_is_core_internal_not_executor_dependent() {
    let store = Arc::new(MemorySessionStore::new());
    let parse_error = PendingCall::new(
        "fallback_tool_protocol_error_1",
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "message": "Tool-call protocol error: bad JSON",
            "hint": "Retry with a valid <tool_call> block.",
            "errors": [{"message":"bad JSON","raw":"<tool_call>{"}]
        }),
    );
    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls("", vec![parse_error]),
        reply::text("recovered"),
    ]));
    let tools = Arc::new(MockTools::new());
    let runner = build_runner(model, tools, store);

    let mut state = new_state();
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("bad fallback call"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 20).await;

    let protocol_result = state
        .history
        .iter()
        .find_map(|m| match m {
            Message::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .expect("protocol tool result");
    assert!(!protocol_result.ok);
    assert!(protocol_result.retryable);
    assert!(protocol_result.model_text().contains("bad JSON"));
    assert!(!protocol_result.model_text().contains("no mock"));
    assert!(matches!(state.step, Step::Done { .. }));
}

#[test]
fn tool_result_model_text_starts_with_structured_status_header() {
    let result = ToolResult::err(
        "permission denied by sandbox",
        false,
        Some("request escalation before retrying".into()),
    );
    let text = result.model_text();
    assert!(
        text.starts_with("TOOL_RESULT ok=false"),
        "tool result must lead with a machine-readable status line; got:\n{text}"
    );
    assert!(text.contains("retryable=false"));
    assert!(text.contains("hint=request escalation before retrying"));
}
