//! Core protocol types for subagents.
//!
//! Core only defines stable data shapes. Runtime/setup layers decide how
//! definitions are loaded, wired, exposed as tools, and executed.

use serde::{Deserialize, Serialize};

use crate::core::event::{CallId, RunId, SessionId};
use crate::core::run_state::Usage;

/// Default tool name used when subagents are exposed to a parent agent.
pub const SUBAGENT_TOOL_NAME: &str = "spawn_sub_agent";

/// Conservative fuse for one subagent invocation when a definition does not
/// provide its own budget.
pub const DEFAULT_SUBAGENT_MAX_STEPS: usize = 1_000;

/// Programmatic/file-backed definition of one specialized agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    /// System-prompt-like instructions specific to this subagent.
    pub instructions: String,
    /// `None` inherits the parent/default tool set. `Some([])` exposes no
    /// tools. Runtime layers must remove the subagent delegation tool so
    /// subagent invocations stay one level deep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// `None` inherits the parent/default skill prompt set. `Some([])` exposes
    /// no skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Optional model override. Setup layers interpret this relative to the
    /// configured provider/key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
    #[serde(default)]
    pub context_mode: SubagentContextMode,
}

impl AgentDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            instructions: instructions.into(),
            tools: None,
            skills: None,
            model: None,
            max_steps: None,
            context_mode: SubagentContextMode::default(),
        }
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = Some(to_string_vec(tools));
        self
    }

    pub fn skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.skills = Some(to_string_vec(skills));
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = Some(max_steps.max(1));
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentContextMode {
    /// Fresh conversation. The parent provides only the explicit task string.
    #[default]
    Fresh,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentInvocation {
    pub agent_name: String,
    pub task: String,
    pub parent_run_id: RunId,
    pub parent_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<CallId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubagentResult {
    pub agent_name: String,
    pub final_text: String,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub usage: Usage,
}

fn to_string_vec<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}
