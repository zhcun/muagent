//! Offline: Runner retries model on Transient / RateLimited; fails fast on
//! Auth / Fatal / InvalidRequest.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::error::ModelError;
use muagent::core::prelude::*;
use muagent::core::retry::RetryPolicy;
use muagent::core::step::Step;
use muagent::core::thinking::{
    ReplayPolicy, ThinkingArtifact, ThinkingKind, ThinkingPayload, ThinkingVisibility,
};
use muagent::core::types::{Content, Message};
use muagent::storage::MemorySessionStore;
use serde_json::json;

/// A scripted mock that returns each queued outcome in order.
/// `outcomes` is consumed front-to-back; when empty, panics.
enum Outcome {
    Ok(ModelReply),
    Err(ModelError),
}

struct ScriptedModel {
    outcomes: Mutex<Vec<Outcome>>,
    calls: Mutex<u32>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    fn new(outcomes: Vec<Outcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }
    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelAdapter for ScriptedModel {
    fn caps(&self) -> LlmCaps {
        LlmCaps::default()
    }
    async fn turn(&self, r: ModelRequest, _c: CancelToken) -> Result<ModelReply, ModelError> {
        *self.calls.lock().unwrap() += 1;
        self.requests.lock().unwrap().push(r);
        let mut q = self.outcomes.lock().unwrap();
        if q.is_empty() {
            panic!("scripted model: out of outcomes");
        }
        match q.remove(0) {
            Outcome::Ok(r) => Ok(r),
            Outcome::Err(e) => Err(e),
        }
    }
}

struct NoToolsExecutor;
#[async_trait]
impl ToolExecutor for NoToolsExecutor {
    async fn execute(
        &self,
        _c: &PendingCall,
        _ctx: &ToolContext,
        _k: CancelToken,
    ) -> Result<ToolResult, muagent::core::error::ToolExecutorError> {
        Ok(ToolResult::ok("unreachable"))
    }
    fn idempotency_for(&self, _c: &PendingCall) -> Idempotency {
        Idempotency::Idempotent
    }
}

fn done_reply(text: &str) -> ModelReply {
    ModelReply {
        text: text.into(),
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: vec![],
    }
}

fn thinking_only_reply() -> ModelReply {
    ModelReply {
        text: String::new(),
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: vec![ThinkingArtifact {
            provider: "anthropic".into(),
            kind: ThinkingKind::FullText,
            replay: ReplayPolicy::MustReplayUnmodified,
            visibility: ThinkingVisibility::Hidden,
            payload: ThinkingPayload::Text {
                text: "hidden only".into(),
            },
            provider_signature: None,
        }],
    }
}

fn build_runner(model: Arc<ScriptedModel>, policy: RetryPolicy) -> Runner {
    Runner::builder()
        .model(model)
        .tools(Arc::new(NoToolsExecutor))
        .store(Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>)
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .retry_policy(policy)
        .build()
        .unwrap()
}

async fn drive(runner: &Runner, state: &mut RunState, max: usize) {
    for _ in 0..max {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            return;
        }
        let _ = runner.step(state).await;
        if state.step.name() == "failed" {
            return;
        }
    }
}

// === Tests ===

#[tokio::test]
async fn succeeds_after_transient_failures() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::Transient("DNS wobble".into())),
        Outcome::Err(ModelError::Transient("502 Bad Gateway".into())),
        Outcome::Ok(done_reply("done")),
    ]));
    // Tight policy so the test doesn't sleep long.
    let policy = RetryPolicy {
        max_attempts: 4,
        initial_backoff_ms: 10,
        max_backoff_ms: 50,
        multiplier: 2.0,
    };
    let runner = build_runner(model.clone(), policy);

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();
    drive(&runner, &mut state, 10).await;

    assert!(
        matches!(state.step, Step::Done { .. }),
        "should reach Done after 2 retries; got {:?}",
        state.step
    );
    assert_eq!(model.call_count(), 3, "3 attempts: 2 failures + 1 success");
}

#[tokio::test]
async fn rate_limited_with_retry_after_is_honored() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::RateLimited {
            retry_after_ms: Some(120),
        }),
        Outcome::Ok(done_reply("ok")),
    ]));
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1, // tiny computed backoff
        max_backoff_ms: 500,   // ≥ the retry_after
        multiplier: 2.0,
    };
    let runner = build_runner(model.clone(), policy);

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    let t0 = Instant::now();
    drive(&runner, &mut state, 10).await;
    let elapsed = t0.elapsed();

    assert!(matches!(state.step, Step::Done { .. }));
    assert_eq!(model.call_count(), 2);
    assert!(
        elapsed.as_millis() >= 100,
        "should have waited at least ~retry_after_ms (120ms); elapsed={elapsed:?}"
    );
}

#[tokio::test]
async fn auth_error_fails_immediately_no_retry() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::Auth("bad key".into())),
        // Would return Ok if retry happened (it shouldn't).
        Outcome::Ok(done_reply("should not reach")),
    ]));
    let policy = RetryPolicy::default();
    let runner = build_runner(model.clone(), policy);

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    let _ = runner.step(&mut state).await; // Ready → ModelTurn
    let out = runner.step(&mut state).await.unwrap();
    assert!(out.advanced);
    assert!(
        matches!(state.step, Step::Failed { .. }),
        "auth errors should terminally fail the run"
    );
    assert!(out.events.iter().any(|e| matches!(
        e,
        Event::ErrorRaised { class, .. } if class == "provider_fatal"
    )));
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, Event::SessionEnd { ok: false, .. })));
    assert_eq!(model.call_count(), 1, "no retries on Auth");

    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("try again"),
            },
        )
        .await
        .unwrap();
    assert!(matches!(state.step, Step::Ready));
}

#[tokio::test]
async fn invalid_request_fails_immediately_no_retry() {
    let model = Arc::new(ScriptedModel::new(vec![Outcome::Err(
        ModelError::InvalidRequest("bad json".into()),
    )]));
    let runner = build_runner(model.clone(), RetryPolicy::default());

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    let _ = runner.step(&mut state).await;
    let r = runner.step(&mut state).await;
    assert!(r.is_ok());
    assert!(matches!(state.step, Step::Failed { .. }));
    assert_eq!(model.call_count(), 1);
}

#[tokio::test]
async fn exhausted_retries_propagate_the_last_error() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::Transient("1".into())),
        Outcome::Err(ModelError::Transient("2".into())),
        Outcome::Err(ModelError::Transient("3".into())),
    ]));
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1,
        max_backoff_ms: 10,
        multiplier: 2.0,
    };
    let runner = build_runner(model.clone(), policy);

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    let _ = runner.step(&mut state).await;
    let r = runner.step(&mut state).await;
    assert!(r.is_ok(), "should terminally fail after max_attempts");
    assert!(matches!(state.step, Step::Failed { .. }));
    assert_eq!(model.call_count(), 3, "exactly max_attempts calls");
}

#[tokio::test]
async fn thinking_only_reply_is_retried_as_empty_visible_response() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Ok(thinking_only_reply()),
        Outcome::Ok(done_reply("visible answer")),
    ]));
    let policy = RetryPolicy {
        max_attempts: 2,
        initial_backoff_ms: 1,
        max_backoff_ms: 1,
        multiplier: 1.0,
    };
    let runner = build_runner(model.clone(), policy);

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();
    drive(&runner, &mut state, 10).await;

    match &state.step {
        Step::Done { final_text } => assert_eq!(final_text, "visible answer"),
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(model.call_count(), 2);
}

#[tokio::test]
async fn empty_reply_after_tool_result_retries_with_continuation_prompt() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Ok(done_reply("")),
        Outcome::Ok(done_reply("continued")),
    ]));
    let policy = RetryPolicy {
        max_attempts: 2,
        initial_backoff_ms: 1,
        max_backoff_ms: 1,
        multiplier: 1.0,
    };
    let runner = build_runner(model.clone(), policy);

    let call = PendingCall::new("call_1", "fs_read", json!({"uri":"file:///x"}));
    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = vec![
        Message::User {
            content: Content::text("read"),
        },
        Message::Assistant {
            content: Content::text(""),
            tool_calls: vec![call],
            thinking: vec![],
        },
        Message::ToolResult {
            call_id: "call_1".into(),
            result: ToolResult::ok("file contents"),
        },
    ];
    state.step = Step::ModelTurn;

    let out = runner.step(&mut state).await.unwrap();
    assert!(out.advanced);
    match &state.step {
        Step::Done { final_text } => assert_eq!(final_text, "continued"),
        other => panic!("expected Done, got {other:?}"),
    }

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[0].messages.last(),
        Some(Message::ToolResult { .. })
    ));
    match requests[1].messages.last() {
        Some(Message::User {
            content: Content::Text(text),
        }) => assert_eq!(text, "Please continue."),
        other => panic!("expected continuation user message, got {other:?}"),
    }
}

#[tokio::test]
async fn cancelled_model_turn_pauses_without_retry() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::Cancelled),
        Outcome::Ok(done_reply("should not reach")),
    ]));
    let runner = build_runner(model.clone(), RetryPolicy::default());

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();

    let _ = runner.step(&mut state).await;
    let out = runner.step(&mut state).await.unwrap();
    assert!(out.advanced);
    assert!(
        matches!(state.step, Step::Paused { .. }),
        "cancelled model turns should pause the run; got {:?}",
        state.step
    );
    assert_eq!(model.call_count(), 1, "cancel must not retry");
}

#[tokio::test]
async fn never_policy_disables_retry() {
    let model = Arc::new(ScriptedModel::new(vec![
        Outcome::Err(ModelError::Transient("oops".into())),
        // A second reply shouldn't be touched.
        Outcome::Ok(done_reply("should not reach")),
    ]));
    let runner = build_runner(model.clone(), RetryPolicy::never());

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();
    let _ = runner.step(&mut state).await;
    let r = runner.step(&mut state).await;
    assert!(r.is_ok());
    assert!(matches!(state.step, Step::Failed { .. }));
    assert_eq!(model.call_count(), 1);
}
