//! Offline long-session memory test.
//!
//! This covers the user-facing invariant for long chats: once old turns are
//! summarized, the next model turn should see the summary as usable context
//! and the persisted session should continue from the compacted history, not
//! from the full old transcript.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::{Content, ContentPart, Message, ObsKind};
use muagent::prelude::SessionManager;
use muagent::sessions::compaction::{CompactionBudget, RunnerCompactor, SummaryCompaction};
use muagent::storage::MemorySessionStore;
use uuid::Uuid;

const OLD_RAW_MARKER: &str = "raw-only-marker-should-be-gone";
const SUMMARY_FACT: &str = "The user asked to remember project codename VIOLET-17.";
const CURRENT_QUESTION: &str = "current direct question asks for the project codename";

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

struct SummaryAwareModel {
    saw_compacted_request: Mutex<bool>,
}

impl SummaryAwareModel {
    fn new() -> Self {
        Self {
            saw_compacted_request: Mutex::new(false),
        }
    }

    fn saw_compacted_request(&self) -> bool {
        *self.saw_compacted_request.lock().unwrap()
    }
}

#[async_trait]
impl ModelAdapter for SummaryAwareModel {
    fn caps(&self) -> LlmCaps {
        LlmCaps {
            native_tool_use: true,
            ctx_len: 8192,
            ..Default::default()
        }
    }

    async fn turn(
        &self,
        req: ModelRequest,
        _cancel: CancelToken,
    ) -> Result<ModelReply, ModelError> {
        let transcript = render_messages(&req.messages);

        assert!(
            transcript.contains("[conversation summary of"),
            "model request should include a compaction summary; got:\n{transcript}"
        );
        assert!(
            transcript.contains("VIOLET-17"),
            "summary fact should reach the next model turn; got:\n{transcript}"
        );
        assert!(
            !transcript.contains(OLD_RAW_MARKER),
            "raw compacted marker leaked into model context; got:\n{transcript}"
        );
        assert!(
            transcript.contains(CURRENT_QUESTION),
            "current user question must remain in context; got:\n{transcript}"
        );

        *self.saw_compacted_request.lock().unwrap() = true;
        Ok(reply::text("VIOLET-17"))
    }
}

#[tokio::test]
async fn long_session_compaction_summary_is_used_for_continuation_memory() {
    let summarizer = Arc::new(CannedModel::named(
        "summarizer",
        vec![reply::text(SUMMARY_FACT)],
    ));
    let main = Arc::new(SummaryAwareModel::new());
    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(CompactionBudget {
            max_tokens: 4_000,
            threshold_ratio: 0.5,
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
        .base_system_prompt("You are a compact long-session assistant.")
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    state.history = long_history_with_old_fact(10);
    let before_submit_len = state.history.len();

    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(format!("{CURRENT_QUESTION}. Return only the codename.")),
            },
        )
        .await
        .unwrap();

    let mut events = Vec::new();
    for _ in 0..8 {
        let out = runner.step(&mut state).await.unwrap();
        events.extend(out.events);
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
    }

    assert!(matches!(
        state.step,
        Step::Done {
            ref final_text
        } if final_text == "VIOLET-17"
    ));
    assert!(
        main.saw_compacted_request(),
        "main model should have received the compacted context"
    );
    assert_eq!(
        summarizer.remaining(),
        0,
        "summarizer should be called exactly once"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::HistoryCompacted { .. })),
        "runner should emit HistoryCompacted"
    );
    assert!(
        state.history.len() < before_submit_len,
        "compacted history should be shorter than original: before={before_submit_len} after={}",
        state.history.len()
    );
    assert!(history_contains_summary_fact(&state.history));
    assert!(
        !render_messages(&state.history).contains(OLD_RAW_MARKER),
        "old raw transcript should have been replaced by the summary"
    );

    let mgr = SessionManager::new(store.clone());
    let continued = mgr.continue_session(state.session_id, 1_000).await.unwrap();
    assert!(history_contains_summary_fact(&continued.history));
    assert!(
        !render_messages(&continued.history).contains(OLD_RAW_MARKER),
        "continued session should inherit compacted history, not resurrect old raw turns"
    );
}

fn long_history_with_old_fact(turns: usize) -> Vec<Message> {
    // Layout:
    // - turn 0 = initial user request, holds the fact "VIOLET-17". Must be
    //   preserved verbatim across compactions (Codex / pi-mono invariant);
    //   we explicitly want the first ask to survive.
    // - turn 1 = a later user message containing OLD_RAW_MARKER. This is
    //   what compaction MUST replace with a summary — it's a mid-session
    //   raw turn that has no business being in the working context after
    //   the conversation has moved on.
    let mut history = Vec::new();
    for i in 0..turns {
        let intro = match i {
            0 => "Remember project codename VIOLET-17. ".to_string(),
            1 => format!("{OLD_RAW_MARKER}. "),
            _ => String::new(),
        };
        let filler = format!("old-turn-{i} {}", "x".repeat(2_000));
        history.push(Message::User {
            content: Content::text(format!("{intro}{filler}")),
        });
        history.push(Message::Assistant {
            content: Content::text(format!("noted old-turn-{i} {}", "y".repeat(2_000))),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    history
}

fn history_contains_summary_fact(history: &[Message]) -> bool {
    history.iter().any(|m| match m {
        Message::Observation {
            kind: ObsKind::Summary,
            text,
        } => text.contains("VIOLET-17") && text.contains("[conversation summary of"),
        _ => false,
    })
}

fn render_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| match m {
            Message::User { content } => format!("USER: {}", content_text(content)),
            Message::System { content } => format!("SYSTEM: {}", content_text(content)),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => format!(
                "ASSISTANT: {} tool_calls={}",
                content_text(content),
                tool_calls.len()
            ),
            Message::ToolResult { result, .. } => format!("TOOL_RESULT: {}", result.text()),
            Message::Observation { kind, text } => format!("OBS {kind:?}: {text}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
