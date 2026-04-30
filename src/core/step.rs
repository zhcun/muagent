//! Step FSM 枚举 + PauseReason。

use serde::{Deserialize, Serialize};

use crate::core::tool::PendingCall;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum Step {
    Ready,
    ModelTurn,
    /// 本 turn 的 tool_calls;按 cursor 顺序执行;结果立即进 state.history
    ToolBatch {
        calls: Vec<PendingCall>,
        cursor: usize,
    },
    /// AtMostOnce tool 已持久化执行意图;thaw 时看到 = 上次中断在 tool 执行中
    ToolIntent {
        call: PendingCall,
        intent_ms: i64,
    },
    /// 资源 / host 主动暂停;不是错误
    Paused {
        reason: PauseReason,
    },
    Done {
        final_text: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PauseReason {
    BudgetExceeded { dim: String },
    HostRequested,
}

impl Step {
    pub fn name(&self) -> &'static str {
        use Step::*;
        match self {
            Ready => "ready",
            ModelTurn => "model_turn",
            ToolBatch { .. } => "tool_batch",
            ToolIntent { .. } => "tool_intent",
            Paused { .. } => "paused",
            Done { .. } => "done",
            Failed { .. } => "failed",
        }
    }

    pub fn is_terminal_or_paused(&self) -> bool {
        matches!(
            self,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        )
    }
}
