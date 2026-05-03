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
        /// Raw JSON args the tool was invoked with. Recorded verbatim so any
        /// consumer (TUI, audit replay, JSON exporter) can render a faithful
        /// "what was called" view without joining against `RunState.history`.
        /// Defaulted on deserialise so older event logs that pre-date this
        /// field still load.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        args: serde_json::Value,
        seq: EventSeq,
    },
    ToolCallEnd {
        call_id: CallId,
        ok: bool,
        retryable: bool,
        brief: String,
        /// Optional structured detail the tool returned alongside its text
        /// content (mirrors `ToolResult.detail`). Carries machine-readable
        /// per-tool metadata — e.g. `{ "lines_added": 3, "lines_removed": 1 }`
        /// for fs_edit, `{ "exit_code": 0 }` for sh_exec — so UIs and audit
        /// replay don't need to parse `brief` strings or join state history
        /// to render structured indicators. Defaulted on deserialise so
        /// older event logs continue to load.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        detail: serde_json::Value,
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
