//! Event 枚举(Core 最小集)+ id 类型。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type EventSeq = u64;

pub type RunId = Uuid;
pub type SessionId = Uuid;
pub type TurnId = u32;
pub type CallId = String;

/// Core 侧的事件最小集。Shell / addon 可在自己的 enum 里追加扩展;此 enum
/// 保留未来 `#[non_exhaustive]` 的可能性。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    SessionStart {
        run_id: RunId,
        seq: EventSeq,
    },
    SessionEnd {
        ok: bool,
        seq: EventSeq,
    },
    UserMessage {
        seq: EventSeq,
    },
    AssistantDelta {
        text: String,
        seq: EventSeq,
    },
    AssistantMessage {
        text: String,
        seq: EventSeq,
    },
    ToolCallStart {
        call_id: CallId,
        tool: String,
        seq: EventSeq,
    },
    ToolCallEnd {
        call_id: CallId,
        ok: bool,
        retryable: bool,
        brief: String,
        seq: EventSeq,
    },
    ToolIntentRecovered {
        call_id: CallId,
        seq: EventSeq,
    },
    StepAdvanced {
        to: String,
        seq: EventSeq,
    },
    Paused {
        reason: String,
        seq: EventSeq,
    },
    ErrorRaised {
        class: String,
        brief: String,
        seq: EventSeq,
    },
    HistoryCompacted {
        replaced_turns: usize,
        replaced_messages: usize,
        saved_tokens_estimate: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_kept_message_id: Option<String>,
        seq: EventSeq,
    },
}

impl Event {
    pub fn seq(&self) -> EventSeq {
        use Event::*;
        match self {
            SessionStart { seq, .. }
            | SessionEnd { seq, .. }
            | UserMessage { seq }
            | AssistantDelta { seq, .. }
            | AssistantMessage { seq, .. }
            | ToolCallStart { seq, .. }
            | ToolCallEnd { seq, .. }
            | ToolIntentRecovered { seq, .. }
            | StepAdvanced { seq, .. }
            | Paused { seq, .. }
            | ErrorRaised { seq, .. }
            | HistoryCompacted { seq, .. } => *seq,
        }
    }
}
