//! Codex-style lifecycle hooks for the core runner.
//!
//! Core owns the typed lifecycle contract and the deterministic merge
//! semantics. It deliberately does not read hook config files or execute
//! shell commands; hosts can build those adapters on top of `HookDispatcher`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::cancel::CancelToken;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HookEventName {
    #[serde(rename = "SessionStart")]
    SessionStart,
    #[serde(rename = "PreToolUse")]
    PreToolUse,
    #[serde(rename = "PostToolUse")]
    PostToolUse,
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit,
    #[serde(rename = "Stop")]
    Stop,
}

impl HookEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEventName::SessionStart => "SessionStart",
            HookEventName::PreToolUse => "PreToolUse",
            HookEventName::PostToolUse => "PostToolUse",
            HookEventName::UserPromptSubmit => "UserPromptSubmit",
            HookEventName::Stop => "Stop",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
}

impl SessionStartSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStartSource::Startup => "startup",
            SessionStartSource::Resume => "resume",
            SessionStartSource::Clear => "clear",
        }
    }
}

/// Flat, command-hook-compatible input. Field names intentionally mirror
/// Codex hook JSON (`hook_event_name`, `tool_use_id`, `tool_input`, ...).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HookInput {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub hook_event_name: HookEventName,
    pub model: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SessionStartSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_hook_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HookOutput {
    #[serde(default = "default_continue", rename = "continue")]
    pub continue_run: bool,
    #[serde(
        default,
        rename = "stopReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,
    #[serde(
        default,
        rename = "systemMessage",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<String>,
    #[serde(default, rename = "suppressOutput")]
    pub suppress_output: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HookDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(
        default,
        rename = "hookSpecificOutput",
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

impl Default for HookOutput {
    fn default() -> Self {
        Self {
            continue_run: true,
            stop_reason: None,
            system_message: None,
            suppress_output: false,
            decision: None,
            reason: None,
            hook_specific_output: None,
        }
    }
}

impl HookOutput {
    pub fn additional_context(&self, event: HookEventName) -> Option<&str> {
        match (&self.hook_specific_output, event) {
            (
                Some(HookSpecificOutput::SessionStart {
                    additional_context: Some(s),
                }),
                HookEventName::SessionStart,
            )
            | (
                Some(HookSpecificOutput::UserPromptSubmit {
                    additional_context: Some(s),
                }),
                HookEventName::UserPromptSubmit,
            )
            | (
                Some(HookSpecificOutput::PostToolUse {
                    additional_context: Some(s),
                }),
                HookEventName::PostToolUse,
            )
            | (
                Some(HookSpecificOutput::Stop {
                    additional_context: Some(s),
                }),
                HookEventName::Stop,
            ) => non_empty(s),
            _ => None,
        }
    }

    pub fn block_reason(&self, event: HookEventName) -> Option<String> {
        if !self.continue_run {
            return Some(
                self.stop_reason
                    .clone()
                    .or_else(|| self.reason.clone())
                    .unwrap_or_else(|| "blocked by hook".into()),
            );
        }
        if self.decision == Some(HookDecision::Block) {
            return Some(
                self.reason
                    .clone()
                    .unwrap_or_else(|| "blocked by hook".into()),
            );
        }
        match (&self.hook_specific_output, event) {
            (
                Some(HookSpecificOutput::PreToolUse {
                    permission_decision: Some(HookPermissionDecision::Deny),
                    permission_decision_reason,
                }),
                HookEventName::PreToolUse,
            ) => Some(
                permission_decision_reason
                    .clone()
                    .unwrap_or_else(|| "blocked by PreToolUse hook".into()),
            ),
            _ => None,
        }
    }

    pub fn stop_continuation_reason(&self) -> Option<String> {
        if !self.continue_run {
            return None;
        }
        if self.decision == Some(HookDecision::Block) {
            return Some(
                self.reason
                    .clone()
                    .or_else(|| self.stop_reason.clone())
                    .unwrap_or_else(|| "Continue.".into()),
            );
        }
        None
    }
}

fn default_continue() -> bool {
    true
}

fn non_empty(s: &str) -> Option<&str> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Block,
    Approve,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookPermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    #[serde(rename = "SessionStart")]
    SessionStart {
        #[serde(
            default,
            rename = "additionalContext",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "PreToolUse")]
    PreToolUse {
        #[serde(
            default,
            rename = "permissionDecision",
            skip_serializing_if = "Option::is_none"
        )]
        permission_decision: Option<HookPermissionDecision>,
        #[serde(
            default,
            rename = "permissionDecisionReason",
            skip_serializing_if = "Option::is_none"
        )]
        permission_decision_reason: Option<String>,
    },
    #[serde(rename = "PostToolUse")]
    PostToolUse {
        #[serde(
            default,
            rename = "additionalContext",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit {
        #[serde(
            default,
            rename = "additionalContext",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "Stop")]
    Stop {
        #[serde(
            default,
            rename = "additionalContext",
            skip_serializing_if = "Option::is_none"
        )]
        additional_context: Option<String>,
    },
}

#[async_trait]
pub trait HookDispatcher: Send + Sync {
    async fn dispatch(&self, input: HookInput, cancel: CancelToken) -> HookOutput;
}

#[derive(Default)]
pub struct NoopHookDispatcher;

#[async_trait]
impl HookDispatcher for NoopHookDispatcher {
    async fn dispatch(&self, _input: HookInput, _cancel: CancelToken) -> HookOutput {
        HookOutput::default()
    }
}
