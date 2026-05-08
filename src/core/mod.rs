//! μAgent Core Runtime
//!
//! v3.1 极小内核:
//! - 4 个必备 trait:`ModelAdapter` / `ToolExecutor` / `SessionStore` /
//!   `ActiveToolSetProvider`
//! - 可选 helpers:`Clock` / `HookDispatcher`
//! - FSM:`Ready → ModelTurn → ToolBatch → ToolIntent → Done / Failed / Paused`
//! - `RunState` 带 `schema_version`;step 级原子持久化;
//!   事件 `(run_id, seq)` at-least-once
//!
//! Core-level protocol concerns(除了运行时 FSM):
//! - **Prompt cache** (`cache`):跨 provider 的 prefix caching 策略,影响
//!   成本与延迟。见 `CachePolicy`。
//! - **Thinking / reasoning** (`thinking`):provider 原生 reasoning artifacts
//!   的统一表示 + 跨 tool-loop 的 replay 规则。见 `design/17-thinking-design.md`
//!   和 `thinking::{ThinkingConfig, ThinkingArtifact, ReplayPolicy}`。
//!   两者并列为一等公民,因为它们都影响下一次模型调用的**正确性**。
//!
//! 本模块 **不做 I/O**。host 通过 trait 注入 model / tool / store。

pub mod cache;
pub mod cancel;
pub mod clock;
pub mod compactor;
pub mod error;
pub mod event;
pub mod hook;
pub mod model;
pub mod net;
pub mod prompt;
pub mod provider;
pub mod retry;
pub mod run_state;
pub mod runner;
pub mod sanitize;
pub mod step;
pub mod store;
pub mod subagent;
pub mod summary_recall;
pub mod thinking;
pub mod tool;
pub mod types;
pub mod wire;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub mod prelude {
    //! 常用类型重新导出。
    pub use crate::core::clock::SystemClock;

    pub use crate::core::{
        cache::{CacheKeyStrategy, CachePolicy},
        cancel::CancelToken,
        clock::{BudgetHint, Clock},
        compactor::{CompactionEvent, Compactor},
        error::{
            ErrorClass, ModelError, RuntimeError, StoreErrClass, StoreError, ToolExecutorError,
        },
        event::{CallId, Event, EventSeq, RunId, SessionId, TurnId},
        hook::{
            HookDecision, HookDispatcher, HookEventName, HookInput, HookOutput,
            HookPermissionDecision, HookSpecificOutput, NoopHookDispatcher, SessionStartSource,
        },
        model::{LlmCaps, ModelAdapter, ModelReply, ModelRequest, ModelStreamEvent, TokenUsage},
        net::{
            check_model_status, net_err_to_model, HttpMethod, HttpReq, HttpResp, NetEgress, NetErr,
        },
        prompt::{
            CacheScope, PromptAuthority, PromptBlock, PromptPlan, PromptPosition, PromptStability,
            RuntimeFacts,
        },
        provider::{ActiveToolSet, ActiveToolSetProvider},
        retry::RetryPolicy,
        run_state::{CompactionCheckpoint, MessageIdRange, RunState, Usage, CURRENT_SCHEMA},
        runner::{Runner, RunnerBuilder, StepOutput},
        step::{PauseReason, Step},
        store::{AuditFilter, RunFilter, RunHeader, RunStatus, SessionStore, ToolAuditRecord},
        subagent::{
            AgentDefinition, SubagentContextMode, SubagentInvocation, SubagentResult,
            DEFAULT_SUBAGENT_MAX_STEPS, SUBAGENT_TOOL_NAME,
        },
        thinking::{
            ReplayPolicy, ThinkingArtifact, ThinkingBudget, ThinkingConfig, ThinkingEffort,
            ThinkingKind, ThinkingMode, ThinkingPayload, ThinkingSupport, ThinkingVisibility,
        },
        tool::{
            parse_args, CapabilityRegistry, Concurrency, GuardOutcome, Idempotency, PendingCall,
            SideEffects, Tool, ToolContext, ToolDescriptor, ToolErr, ToolExecutor, ToolOk,
            ToolResult, TOOL_PROTOCOL_ERROR_TOOL,
        },
        types::{Content, ContentPart, Message, ObsKind},
        wire::prepare_messages_for_caps,
    };
}
