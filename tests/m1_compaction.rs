//! M1 Compaction 的**离线**测试:token 估算精度 + turn 边界 + 压缩逻辑。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::error::ModelError;
use muagent::core::prelude::*;
use muagent::core::tool::PendingCall;
use muagent::core::types::{Content, ContentPart, Message, ObsKind};
use muagent::runtime::token_estimate;
use muagent::sessions::compaction::{
    find_user_turn_boundaries, CompactionBudget, CompactionStrategy, SummaryCompaction,
};
use serde_json::json;

// ============ Token estimator 精度 ============

#[test]
fn estimate_text_english() {
    // "Hello world" = 11 chars, 11 bytes → max(ceil(11/3), ceil(11/4)) = max(4, 3) = 4
    let est = token_estimate::estimate_text_tokens("Hello world");
    assert!(
        (3..=5).contains(&est),
        "English 11 chars → ~3-4 tokens, got {est}"
    );
}

#[test]
fn estimate_text_chinese_is_upper_bound() {
    // 中文"你好世界" = 4 chars, 12 bytes → max(2, 3) = 3
    // 实际 ~4 tokens(tiktoken),估算 3,比实际低——这是故意保守点
    // 但 bytes/4 覆盖了非 ASCII 场景(中文 UTF-8 3 bytes/字)
    let est = token_estimate::estimate_text_tokens("你好世界");
    assert!(
        est >= 3,
        "Chinese 4 chars → at least 3 tokens (bytes/4), got {est}"
    );
}

#[test]
fn estimate_long_text_monotonic() {
    let short = token_estimate::estimate_text_tokens(&"a".repeat(100));
    let long = token_estimate::estimate_text_tokens(&"a".repeat(1000));
    assert!(long > short * 5); // should scale roughly linearly
}

#[test]
fn estimate_image_has_fixed_cost() {
    let msg = Message::User {
        content: Content::Parts(vec![
            ContentPart::Text {
                text: "what?".into(),
            },
            ContentPart::Image {
                uri: None,
                b64: Some("AAAA".into()), // 4 chars;but image tokens are fixed high
                mime: "image/png".into(),
            },
        ]),
    };
    let est = token_estimate::estimate_message_tokens(&msg);
    assert!(
        est >= token_estimate::IMAGE_TOKEN_COST,
        "image should dominate: got {est}"
    );
}

#[test]
fn estimate_assistant_tool_calls_add_tokens() {
    let base = Message::Assistant {
        content: Content::text("ok"),
        tool_calls: vec![],
        thinking: vec![],
    };
    let with_calls = Message::Assistant {
        content: Content::text("ok"),
        tool_calls: vec![PendingCall::new(
            "c1",
            "fs_read",
            json!({"uri":"file:///etc/hosts"}),
        )],
        thinking: vec![],
    };
    let base_est = token_estimate::estimate_message_tokens(&base);
    let with_est = token_estimate::estimate_message_tokens(&with_calls);
    assert!(
        with_est > base_est + 8,
        "tool_call should add tokens: base={base_est} with={with_est}"
    );
}

// ============ Turn boundary detection ============

#[test]
fn turn_boundaries_basic() {
    let h = vec![
        Message::User {
            content: Content::text("Q1"),
        },
        Message::Assistant {
            content: Content::text("A1"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("Q2"),
        },
        Message::Assistant {
            content: Content::text("A2"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    let turns = find_user_turn_boundaries(&h);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0], 0..2);
    assert_eq!(turns[1], 2..4);
}

#[test]
fn turn_boundaries_with_tool_roundtrip_in_single_turn() {
    let h = vec![
        Message::User {
            content: Content::text("write"),
        },
        Message::Assistant {
            content: Content::text(""),
            tool_calls: vec![PendingCall::new(
                "a",
                "fs_write",
                json!({"uri":"x","content":"y"}),
            )],
            thinking: vec![],
        },
        Message::ToolResult {
            call_id: "a".into(),
            result: ToolResult::ok("wrote"),
        },
        Message::Assistant {
            content: Content::text("done"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("next"),
        },
        Message::Assistant {
            content: Content::text("ok"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    let turns = find_user_turn_boundaries(&h);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0], 0..4); // 整个 write+tool_result+done 是 turn 1
    assert_eq!(turns[1], 4..6);
}

#[test]
fn turn_boundaries_with_system_prefix() {
    let h = vec![
        Message::System {
            content: Content::text("You are helpful"),
        },
        Message::User {
            content: Content::text("Q"),
        },
        Message::Assistant {
            content: Content::text("A"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    let turns = find_user_turn_boundaries(&h);
    // system prefix 不单独成 turn(没触发 User 切换);整个 0..3 一个 turn
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0], 0..3);
}

// ============ Mock summarizer for compaction ============

struct MockSummarizer {
    reply: String,
    calls: Mutex<u32>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl MockSummarizer {
    fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            calls: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ModelAdapter for MockSummarizer {
    fn caps(&self) -> LlmCaps {
        LlmCaps::default()
    }
    async fn turn(
        &self,
        r: ModelRequest,
        _c: CancelToken,
    ) -> Result<ModelReply, muagent::core::error::ModelError> {
        *self.calls.lock().unwrap() += 1;
        self.requests.lock().unwrap().push(r);
        Ok(ModelReply {
            text: self.reply.clone(),
            tool_calls: vec![],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                cost_usd: None,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                thinking_tokens: 0,
            },
            thinking: vec![],
        })
    }
}

// ============ Compaction logic ============

fn long_history(n_turns: usize) -> Vec<Message> {
    let mut h = Vec::new();
    for _i in 0..n_turns {
        h.push(Message::User {
            content: Content::text("x".repeat(1000)),
        });
        h.push(Message::Assistant {
            content: Content::text("y".repeat(1000)),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    h
}

fn labeled_history(prefix: &str, n_turns: usize, fill_chars: usize) -> Vec<Message> {
    let mut h = Vec::new();
    for i in 0..n_turns {
        h.push(Message::User {
            content: Content::text(format!("{prefix}-user-{i} {}", "x".repeat(fill_chars))),
        });
        h.push(Message::Assistant {
            content: Content::text(format!("{prefix}-assistant-{i} {}", "y".repeat(fill_chars))),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    h
}

fn request_transcript(req: &ModelRequest) -> String {
    match req.messages.first() {
        Some(Message::User {
            content: Content::Text(text),
        }) => text.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn compaction_triggers_when_over_budget() {
    let summarizer = Arc::new(MockSummarizer::new("SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 5_000,    // 故意调低
        threshold_ratio: 0.5, // 触发于 2500 tokens
        keep_tail_turns: 2,
        summary_target_chars: 500,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(10); // 10 turns × ~670 tokens = 远超 2500

    let outcome = strategy
        .maybe_compact(
            &mut state,
            &*summarizer,
            "You are terse.",
            CancelToken::never(),
        )
        .await
        .unwrap()
        .expect("should compact");

    assert_eq!(
        outcome.replaced_turns, 8,
        "keep 2 tail turns, compact first 8"
    );
    assert!(outcome.saved_tokens_estimate > 0);
    assert_eq!(outcome.summary, "SUMMARY");
    assert_eq!(*summarizer.calls.lock().unwrap(), 1);

    // History 结构:先一条 Summary observation,然后保留 2 个 tail turn(每 turn 2 msg)
    assert_eq!(state.history.len(), 1 + 2 * 2);
    match &state.history[0] {
        Message::Observation {
            kind: ObsKind::Summary,
            text,
        } => {
            assert!(text.contains("SUMMARY"));
            assert!(text.contains("summary of"));
            assert!(text.contains("compaction checkpoint"));
            assert!(text.contains("first_kept_index="));
            assert!(text.contains("tokens_before="));
        }
        _ => panic!("first message should be Summary observation"),
    }
}

#[tokio::test]
async fn compaction_records_stable_checkpoint_ids() {
    let summarizer = Arc::new(MockSummarizer::new("SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 5_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 2,
        root_task_pin_max_tokens: 0,
        summary_target_chars: 500,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(10);
    state.ensure_history_ids();
    let before_ids = state.history_ids.clone();

    let outcome = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    assert_eq!(state.history_ids.len(), state.history.len());
    assert_eq!(state.compaction_checkpoints.len(), 1);
    let checkpoint = &state.compaction_checkpoints[0];
    assert_eq!(checkpoint.summary_message_id, state.history_ids[0]);
    assert!(checkpoint.removed_message_ids.is_empty());
    assert!(checkpoint.summary_input_message_ids.is_empty());
    assert_eq!(
        checkpoint.removed_message_range.first_message_id.as_ref(),
        before_ids.first()
    );
    assert_eq!(
        checkpoint.removed_message_range.last_message_id.as_ref(),
        before_ids.get(outcome.replaced_messages - 1)
    );
    assert_eq!(
        checkpoint.removed_message_range.count,
        outcome.replaced_messages
    );
    assert_eq!(
        checkpoint.summary_input_message_range.count,
        outcome.replaced_messages
    );
    assert_eq!(
        checkpoint.first_kept_message_id.as_ref(),
        before_ids.get(outcome.replaced_messages)
    );
    assert_eq!(
        &state.history_ids[1..],
        &before_ids[outcome.replaced_messages..]
    );

    let Message::Observation {
        kind: ObsKind::Summary,
        text,
    } = &state.history[0]
    else {
        panic!("first message should be Summary observation");
    };
    assert!(text.contains("checkpoint_id=c"));
    assert!(text.contains("summary_message_id=m"));
    assert!(text.contains("first_kept_id=m"));
}

#[tokio::test]
async fn compaction_transcript_preserves_structured_tool_error_head_and_tail() {
    let summarizer = Arc::new(MockSummarizer::new("SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 0,
        summary_target_chars: 500,
        summary_input_max_tokens: 10_000,
        ..CompactionBudget::default()
    });

    let long_error = format!(
        "permission denied at start\n{}\nTAIL_ERROR_CODE_42",
        "x".repeat(2_000)
    );
    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = vec![
        Message::User {
            content: Content::text("run the command and recover from failures"),
        },
        Message::Assistant {
            content: Content::text(""),
            tool_calls: vec![PendingCall::new(
                "c1",
                "exec_command",
                json!({"cmd":"make"}),
            )],
            thinking: vec![],
        },
        Message::ToolResult {
            call_id: "c1".into(),
            result: ToolResult::err(
                long_error,
                false,
                Some("request escalation before retrying".into()),
            ),
        },
        Message::Assistant {
            content: Content::text("the command failed"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("latest user request must stay verbatim outside compaction"),
        },
    ];

    strategy
        .maybe_compact(
            &mut state,
            &*summarizer,
            "You are terse.",
            CancelToken::never(),
        )
        .await
        .unwrap()
        .expect("should compact");

    let requests = summarizer.requests.lock().unwrap();
    let transcript = request_transcript(&requests[0]);
    assert!(transcript.contains("TOOL_RESULT ok=false"));
    assert!(transcript.contains("hint=request escalation before retrying"));
    assert!(transcript.contains("TAIL_ERROR_CODE_42"));
    assert!(transcript.contains("tool result truncated for compaction"));
}

#[tokio::test]
async fn compaction_summary_prompt_requests_structured_memory_table() {
    let summarizer = Arc::new(MockSummarizer::new("SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 0,
        summary_target_chars: 500,
        summary_input_max_tokens: 10_000,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = vec![
        Message::User {
            content: Content::text(format!(
                "Remember that the case code is PG-NIAH-7429-SUMMIT. {}",
                "x".repeat(2_000)
            )),
        },
        Message::Assistant {
            content: Content::text("Recorded.".repeat(200)),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("latest request: continue the task"),
        },
    ];

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    let requests = summarizer.requests.lock().unwrap();
    let system = &requests[0].system;
    assert!(system.contains("## User Directives"));
    assert!(system.contains("## Tool Evidence"));
    assert!(system.contains("## Structured Memory"));
    assert!(system.contains("| kind | subject | fact | evidence | source/status |"));
    assert!(system.contains("Keep user directives separate from tool evidence"));
    assert!(system.contains("Use `## Structured Memory` for durable facts"));
}

#[tokio::test]
async fn compaction_appends_deterministic_evidence_ledger_for_exact_identifiers() {
    let summarizer = Arc::new(MockSummarizer::new(
        "## Goal\n- Preserve memory anchors.\n\n## Key Facts Discovered\n- The model prose omitted the exact anchors.",
    ));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_500,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 500,
        summary_input_max_tokens: 10_000,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = vec![
        Message::User {
            content: Content::text("Start a long-context memory exercise."),
        },
        Message::Assistant {
            content: Content::text("Acknowledged."),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text(
                "HIGH PRIORITY MEMORY ANCHOR: the exact RULER case code is \
                 PG-NIAH-7429-SUMMIT. Preserve this exact identifier.",
            ),
        },
        Message::Assistant {
            content: Content::text("Recorded."),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text(
                "HIGH PRIORITY MEMORY ANCHOR: the exact RULER case owner is \
                 MIRA-SOL-3185. Preserve this exact identifier.",
            ),
        },
        Message::Assistant {
            content: Content::text("Recorded."),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    state.history.extend(labeled_history("tail", 4, 700));

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    let summary = state
        .history
        .iter()
        .find_map(|m| match m {
            Message::Observation {
                kind: ObsKind::Summary,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .expect("summary observation present");

    assert!(
        summary.contains("## Evidence Ledger (cumulative exact facts)"),
        "summary should carry deterministic evidence ledger even when LLM prose omits anchors; got:\n{summary}"
    );
    assert!(summary.contains("key=case code"));
    assert!(summary.contains("value=PG-NIAH-7429-SUMMIT"));
    assert!(summary.contains("key=case owner"));
    assert!(summary.contains("value=MIRA-SOL-3185"));
}

#[tokio::test]
async fn compaction_starts_from_summary_inside_repair_window() {
    let summarizer = Arc::new(MockSummarizer::new("MERGED-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 2_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 500,
        summary_input_max_tokens: 10_000,
        restart_repair_window_tokens: 50_000,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = labeled_history("outside-window", 4, 700);
    state.history.push(Message::Observation {
        kind: ObsKind::Summary,
        text: "ANCHOR-SUMMARY previous compacted facts".into(),
    });
    state
        .history
        .extend(labeled_history("after-summary", 5, 700));
    state.history.extend(labeled_history("tail", 1, 10));

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact from existing summary");

    let requests = summarizer.requests.lock().unwrap();
    let transcript = request_transcript(&requests[0]);
    assert!(
        transcript.contains("ANCHOR-SUMMARY"),
        "existing summary inside repair window should seed catch-up compaction"
    );
    assert!(
        !transcript.contains("outside-window-user-0"),
        "raw prefix before the in-window summary should be dropped from the summarizer input"
    );
    // The initial root task survives as a lower-priority summary anchor, not
    // as a fresh current user turn.
    assert!(
        matches!(&state.history[0], Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]")),
        "first message must be the root task anchor"
    );
    assert!(
        state.history.iter().any(|m| matches!(
            m,
            Message::Observation {
                kind: ObsKind::Summary,
                ..
            }
        )),
        "compacted history must contain a summary observation"
    );
}

#[tokio::test]
async fn compaction_omits_prefix_when_no_summary_in_repair_window() {
    let summarizer = Arc::new(MockSummarizer::new("WINDOW-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 2_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 500,
        summary_input_max_tokens: 3_000,
        restart_repair_window_tokens: 2_000,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history.push(Message::Observation {
        kind: ObsKind::Summary,
        text: "VERY-OLD-SUMMARY outside restart window".into(),
    });
    state.history.extend(labeled_history("too-old", 8, 900));
    state
        .history
        .extend(labeled_history("inside-window", 4, 450));
    state.history.extend(labeled_history("tail", 1, 10));

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact recent repair window");

    let requests = summarizer.requests.lock().unwrap();
    let transcript = request_transcript(&requests[0]);
    assert!(
        !transcript.contains("VERY-OLD-SUMMARY"),
        "summary outside restart repair window should not be pulled back into context"
    );
    assert!(
        transcript.contains("older transcript omitted"),
        "windowed catch-up should leave an explicit omission marker"
    );
    assert!(
        transcript.contains("inside-window"),
        "recent complete turns should still be summarized"
    );
    // First slot is the root-task memory anchor. The omission-marked summary
    // follows somewhere later.
    assert!(
        matches!(&state.history[0], Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]")),
        "first message should be the root task anchor"
    );
    let summary_with_marker = state.history.iter().find_map(|m| match m {
        Message::Observation {
            kind: ObsKind::Summary,
            text,
        } => Some(text.clone()),
        _ => None,
    });
    assert!(
        summary_with_marker
            .as_deref()
            .map(|t| t.contains("older transcript before this repair window is omitted"))
            .unwrap_or(false),
        "final working context should retain the omission marker even if the summarizer ignores it; got {summary_with_marker:?}"
    );
}

#[tokio::test]
async fn compaction_uses_multiple_bounded_summary_rounds() {
    let summarizer = Arc::new(MockSummarizer::new("ROUND-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_500,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 500,
        summary_input_max_tokens: 1_500,
        restart_repair_window_tokens: 50_000,
        max_summary_rounds: 3,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = labeled_history("round", 18, 900);

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact in multiple rounds");

    let requests = summarizer.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "max_summary_rounds should bound catch-up work per maybe_compact call"
    );
    for req in requests.iter() {
        let transcript = request_transcript(req);
        let user_lines = transcript.matches("USER:").count();
        assert!(
            user_lines <= 3,
            "single summary request should stay near the configured input window; got {user_lines} user turns"
        );
    }
}

#[tokio::test]
async fn compaction_allows_zero_tail_turns() {
    let summarizer = Arc::new(MockSummarizer::new("ALL-COMPACTED"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 2_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 0,
        summary_target_chars: 500,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(6);

    let outcome = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("keep_tail_turns=0 should still compact");

    assert_eq!(
        outcome.replaced_turns, 5,
        "the latest user turn is hard-protected even with keep_tail_turns=0"
    );
    // With `keep_tail_turns=0`, the latest user turn is the hard boundary:
    // `[summary, latest_user, latest_assistant]`. The synthetic long_history
    // uses identical text for every user turn, so the short-root pin is
    // already satisfied by the latest user message and does not duplicate it.
    assert_eq!(state.history.len(), 3);
    assert!(matches!(
        &state.history[0],
        Message::Observation {
            kind: ObsKind::Summary,
            ..
        }
    ));
}

#[tokio::test]
async fn compaction_skips_single_latest_turn_when_zero_tail() {
    let summarizer = Arc::new(MockSummarizer::new("SINGLE-TURN-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 500,
        threshold_ratio: 0.5,
        keep_tail_turns: 0,
        summary_target_chars: 500,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(1);

    let outcome = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap();

    assert!(
        outcome.is_none(),
        "the only/latest user turn must not be compacted; split-turn support is the future escape hatch"
    );
    assert_eq!(*summarizer.calls.lock().unwrap(), 0);
    assert_eq!(state.history, long_history(1));
}

#[tokio::test]
async fn compaction_clamps_summary_output_tokens() {
    let summarizer = Arc::new(MockSummarizer::new("z".repeat(3_000)));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 2_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 2_000,
        summary_output_max_tokens: 100,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(6);

    let outcome = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    assert!(
        token_estimate::estimate_text_tokens(&outcome.summary) <= 100,
        "summary output should be clamped to configured token cap"
    );
}

#[tokio::test]
async fn compaction_skips_when_under_budget() {
    let summarizer = Arc::new(MockSummarizer::new("S"));
    let strategy = SummaryCompaction::new(CompactionBudget::default()); // 156k max

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(3); // ~2000 tokens,远低于 156k

    let r = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap();
    assert!(r.is_none(), "under budget → no compaction");
    assert_eq!(*summarizer.calls.lock().unwrap(), 0);
    assert_eq!(state.history.len(), 6); // unchanged
}

#[tokio::test]
async fn compaction_preserves_tool_roundtrip_in_tail() {
    let summarizer = Arc::new(MockSummarizer::new("SUM"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 2_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 1, // keep only last user-turn
        summary_target_chars: 200,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    // Build history with tool roundtrip in LAST turn
    state.history = vec![
        // turn 1 (old)
        Message::User {
            content: Content::text("q1".repeat(500)),
        },
        Message::Assistant {
            content: Content::text("a1".repeat(500)),
            tool_calls: vec![],
            thinking: vec![],
        },
        // turn 2 (old)
        Message::User {
            content: Content::text("q2".repeat(500)),
        },
        Message::Assistant {
            content: Content::text("a2".repeat(500)),
            tool_calls: vec![],
            thinking: vec![],
        },
        // turn 3 (tail, keep) — tool roundtrip
        Message::User {
            content: Content::text("please read x"),
        },
        Message::Assistant {
            content: Content::text(""),
            tool_calls: vec![PendingCall::new("tr", "fs_read", json!({"uri":"x"}))],
            thinking: vec![],
        },
        Message::ToolResult {
            call_id: "tr".into(),
            result: ToolResult::ok("content here"),
        },
        Message::Assistant {
            content: Content::text("read ok"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];

    let outcome = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    assert!(
        outcome.replaced_turns >= 1,
        "hybrid token tail may keep more than the configured turn tail, but should still compact old context"
    );

    // Tail turn(4 msg:user + assistant+tools + tool_result + assistant)必须完整保留
    let tail_start = state.history.len() - 4;
    assert!(matches!(&state.history[tail_start], Message::User { .. }));
    match &state.history[tail_start + 1] {
        Message::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "tr");
        }
        _ => panic!("expected assistant with tool_calls"),
    }
    match &state.history[tail_start + 2] {
        Message::ToolResult { call_id, .. } => assert_eq!(call_id, "tr"),
        _ => panic!("expected tool_result"),
    }
    assert!(matches!(
        &state.history[tail_start + 3],
        Message::Assistant { .. }
    ));
}

#[tokio::test]
async fn compaction_skips_when_not_enough_turns() {
    // 只有 2 个 turn,keep_tail=4 → 没得压
    let summarizer = Arc::new(MockSummarizer::new("S"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 100,       // 极低
        threshold_ratio: 0.01, // 强制触发
        keep_tail_turns: 4,
        summary_target_chars: 100,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(2);

    let r = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap();
    assert!(r.is_none(), "not enough turns to compact");
    assert_eq!(state.history.len(), 4); // unchanged
}

#[tokio::test]
async fn compaction_skips_empty_summary_without_mutating_history() {
    let summarizer = Arc::new(MockSummarizer::new("   \n\t "));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 5_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 2,
        summary_target_chars: 500,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(10);
    let before = state.history.clone();

    let r = strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap();

    assert!(r.is_none(), "empty summary must not replace history");
    assert_eq!(state.history, before);
    assert_eq!(*summarizer.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn compaction_respects_cancel_without_calling_summarizer() {
    let summarizer = Arc::new(MockSummarizer::new("SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 5_000,
        threshold_ratio: 0.5,
        keep_tail_turns: 2,
        summary_target_chars: 500,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history = long_history(10);
    let cancel = CancelToken::new();
    cancel.trigger();

    let err = strategy
        .maybe_compact(&mut state, &*summarizer, "", cancel)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        muagent::sessions::compaction::CompactionError::Model(ModelError::Cancelled)
    ));
    assert_eq!(*summarizer.calls.lock().unwrap(), 0);
}

#[test]
fn default_budget_is_156k() {
    let b = CompactionBudget::default();
    assert_eq!(b.max_tokens, 156_000);
    assert_eq!(b.threshold_ratio, 0.8);
    assert_eq!(b.keep_tail_turns, 4);
    assert_eq!(b.keep_recent_tokens, 20_000);
    assert_eq!(b.root_task_pin_max_tokens, 1_024);
    assert_eq!(b.summary_input_max_tokens, 100_000);
    assert_eq!(b.summary_output_max_tokens, 8_000);
    assert_eq!(b.restart_repair_window_tokens, 300_000);
    assert_eq!(b.max_summary_rounds, 4);
}

#[tokio::test]
async fn compaction_pins_initial_user_request_as_lower_priority_anchor_across_rounds() {
    // The user's original ask must survive every compaction, but as prior
    // memory rather than a fresh current user turn.
    let initial_question =
        "Refactor src/lib.rs::Foo::bar to take a &mut self and return io::Result<()>; \
         keep the existing call sites green and add tests."
            .to_string();

    let summarizer = Arc::new(MockSummarizer::new("PARAPHRASED-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_500,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 300,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    // Layout: [initial_user] then 12 follow-up turns (so total tokens
    // exceeds the 750 trigger and the initial question lands in the slice
    // that would otherwise be summarized).
    state.history.push(Message::User {
        content: Content::text(initial_question.clone()),
    });
    state.history.extend(labeled_history("followup", 12, 600));

    // Round 1.
    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("first compaction must run");
    assert!(
        state.history.iter().any(
            |m| matches!(m, Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]") && text.contains(&initial_question))
        ),
        "after round 1 the initial user question must still be verbatim in a root anchor; got {:#?}",
        state.history
    );
    assert!(
        matches!(&state.history[0], Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]") && text.contains("Later user directives override")),
        "after round 1 the initial user question must be a lower-priority root anchor at history[0]"
    );

    // Inflate again so a SECOND compaction is needed. This simulates a long
    // session where the agent has kept working past the first summary.
    state.history.extend(labeled_history("post-r1", 8, 600));
    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("second compaction must run");
    assert!(
        state.history.iter().any(
            |m| matches!(m, Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]") && text.contains(&initial_question))
        ),
        "after round 2 the initial user question must still be verbatim in the root anchor; the summarizer paraphrasing it is NOT enough"
    );
    assert!(
        matches!(&state.history[0], Message::Observation { kind: ObsKind::User, text } if text.contains("[root task anchor]") && text.contains("Later user directives override")),
        "after round 2 the root anchor must still be at history[0]"
    );
}

#[tokio::test]
async fn compaction_does_not_pin_long_initial_user_context() {
    let long_initial = format!(
        "PASTED-CORPUS-START {}\nPASTED-CORPUS-END",
        "doc ".repeat(2_000)
    );
    let summarizer = Arc::new(MockSummarizer::new("LONG-ROOT-SUMMARY"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_500,
        threshold_ratio: 0.5,
        keep_tail_turns: 0,
        keep_recent_tokens: 0,
        root_task_pin_max_tokens: 64,
        summary_target_chars: 300,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history.push(Message::User {
        content: Content::text(long_initial.clone()),
    });
    state.history.push(Message::Assistant {
        content: Content::text("read the pasted corpus"),
        tool_calls: vec![],
        thinking: vec![],
    });
    state.history.extend(labeled_history("middle", 4, 600));
    state.history.push(Message::User {
        content: Content::text("latest request stays verbatim"),
    });

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("should compact");

    assert!(
        !state.history.iter().any(
            |m| matches!(m, Message::User { content: Content::Text(t) } if t == &long_initial)
        ),
        "long initial pasted context must not be re-pinned verbatim"
    );
    assert!(
        state.history.iter().any(
            |m| matches!(m, Message::User { content: Content::Text(t) } if t == "latest request stays verbatim")
        ),
        "latest user request must remain verbatim"
    );
}

#[tokio::test]
async fn compaction_accumulates_file_ledger_across_rounds() {
    // After two compactions with fs_read/fs_write calls in between, the
    // final summary observation must list ALL the files touched across
    // both rounds — not just the most recent. Ledger flows through the
    // summary's `## Files Touched` section deterministically (no
    // dependence on the summarizer LLM faithfully repeating paths).
    let summarizer = Arc::new(MockSummarizer::new("LLM-PROSE"));
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 1_500,
        threshold_ratio: 0.5,
        keep_tail_turns: 1,
        summary_target_chars: 300,
        max_summary_rounds: 1,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), 0);
    state.history.push(Message::User {
        content: Content::text("Refactor the parser.".to_string()),
    });
    // Round 1 source: two fs ops + filler so we trip the threshold.
    state.history.push(Message::Assistant {
        content: Content::text("looking"),
        tool_calls: vec![PendingCall::new(
            "c1",
            "fs_read",
            json!({"uri": "file:///old-round/a.rs"}),
        )],
        thinking: vec![],
    });
    state.history.push(Message::Assistant {
        content: Content::text("editing"),
        tool_calls: vec![PendingCall::new(
            "c2",
            "fs_write",
            json!({"uri": "file:///old-round/b.rs", "content": "x"}),
        )],
        thinking: vec![],
    });
    state.history.extend(labeled_history("filler-1", 8, 600));

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("first compaction must run");

    let summary_after_r1 = state
        .history
        .iter()
        .find_map(|m| match m {
            Message::Observation {
                kind: ObsKind::Summary,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .expect("summary observation present after r1");
    assert!(
        summary_after_r1.contains("## Files Touched"),
        "round-1 summary must carry the ledger; got:\n{summary_after_r1}"
    );
    assert!(
        summary_after_r1.contains("file:///old-round/a.rs"),
        "round-1 ledger must include the read uri"
    );
    assert!(
        summary_after_r1.contains("file:///old-round/b.rs"),
        "round-1 ledger must include the modified uri"
    );

    // Round 2: more fs ops happen, then another threshold trip. The new
    // summary must inherit round-1's paths AND add the new ones.
    state.history.push(Message::Assistant {
        content: Content::text("further work"),
        tool_calls: vec![PendingCall::new(
            "c3",
            "fs_read",
            json!({"uri": "file:///round2/c.rs"}),
        )],
        thinking: vec![],
    });
    state.history.push(Message::Assistant {
        content: Content::text("rename pass"),
        tool_calls: vec![PendingCall::new(
            "c4",
            "fs_rename",
            json!({"from": "file:///round2/d.rs", "to": "file:///round2/e.rs"}),
        )],
        thinking: vec![],
    });
    state.history.extend(labeled_history("filler-2", 8, 600));

    strategy
        .maybe_compact(&mut state, &*summarizer, "", CancelToken::never())
        .await
        .unwrap()
        .expect("second compaction must run");

    let summary_after_r2 = state
        .history
        .iter()
        .find_map(|m| match m {
            Message::Observation {
                kind: ObsKind::Summary,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .expect("summary observation present after r2");
    // Cumulative invariant: every fs uri ever touched is in the new ledger.
    for needle in [
        "file:///old-round/a.rs",
        "file:///old-round/b.rs",
        "file:///round2/c.rs",
        "file:///round2/d.rs",
        "file:///round2/e.rs",
    ] {
        assert!(
            summary_after_r2.contains(needle),
            "cumulative ledger must include {needle}; full summary:\n{summary_after_r2}"
        );
    }
}
