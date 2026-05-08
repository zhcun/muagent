//! High-level Rust SDK facade.
//!
//! This module intentionally lives outside `core`: it wraps the existing
//! `Runner` FSM with a host-friendly `Agent` API while leaving core traits,
//! state transitions, and persistence semantics unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::{Config, ConfigOverrides};
use crate::core::clock::{Clock, SystemClock};
use crate::core::error::{RuntimeError, StoreError};
use crate::core::event::{RunId, SessionId};
use crate::core::hook::HookDispatcher;
use crate::core::model::ModelStreamEvent;
use crate::core::run_state::{RunState, Usage};
use crate::core::runner::{Runner, StepOutput};
use crate::core::step::{PauseReason, Step};
use crate::core::subagent::AgentDefinition;
use crate::core::types::{Content, Message};
use crate::sessions::manager::{SearchHit, SessionInfo};
use crate::setup;

/// Default SDK run fuse. Matches the CLI's generous upper bound without
/// pulling SDK code through CLI modules.
pub const DEFAULT_MAX_STEPS: usize = 10_000;

/// SDK-layer error type. Core errors are preserved instead of stringified
/// where callers can reasonably recover or classify them.
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("config: {0}")]
    Config(String),

    #[error("setup: {0}")]
    Setup(String),

    #[error("submit: {0}")]
    Submit(#[source] RuntimeError),

    #[error("step: {0}")]
    Step(#[source] RuntimeError),

    #[error("session store: {0}")]
    Session(#[source] StoreError),

    #[error("run failed: {0}")]
    RunFailed(String),

    #[error("run paused: {0:?}")]
    RunPaused(PauseReason),

    #[error("step budget exceeded after {max_steps} steps; final step={step:?}")]
    StepBudgetExceeded { max_steps: usize, step: Step },

    #[error("{0} is unavailable on an Agent built from raw parts")]
    Unavailable(&'static str),

    #[error("unknown agent: {0}")]
    UnknownAgent(String),
}

/// Events emitted by the SDK facade while a query is running.
///
/// `CoreEvent` carries the exact core step events returned by `Runner`;
/// SDK-specific variants cover non-persistent streaming deltas and query
/// terminal status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionStarted {
        session_id: SessionId,
        run_id: RunId,
        resumed: bool,
    },
    AssistantText {
        text: String,
    },
    AssistantStreamReset,
    CoreEvent {
        event: crate::core::event::Event,
    },
    Result {
        final_text: String,
        session_id: SessionId,
        run_id: RunId,
        usage: Usage,
    },
    Error {
        message: String,
        stage: String,
    },
}

/// Final output for one SDK query.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AgentResponse {
    pub final_text: String,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub usage: Usage,
    pub events: Vec<AgentEvent>,
}

/// Builder for the default Rust SDK experience.
///
/// It uses the same configuration and wiring pipeline as the CLI, but returns
/// a programmatic `Agent` instead of starting a REPL/TUI process.
#[derive(Clone)]
pub struct AgentBuilder {
    overrides: ConfigOverrides,
    config: Option<Config>,
    hooks: Option<Arc<dyn HookDispatcher>>,
    max_steps: usize,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            overrides: ConfigOverrides::default(),
            config: None,
            hooks: None,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

impl std::fmt::Debug for AgentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBuilder")
            .field("overrides", &self.overrides)
            .field("config", &self.config)
            .field("hooks", &self.hooks.as_ref().map(|_| "<hook dispatcher>"))
            .field("max_steps", &self.max_steps)
            .finish()
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fully prepared config. When set, environment/file overrides on
    /// this builder are ignored.
    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Replace all config overrides at once.
    pub fn overrides(mut self, overrides: ConfigOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    pub fn config_file(mut self, path: impl Into<String>) -> Self {
        self.overrides.config_file = Some(path.into());
        self
    }

    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.overrides.provider = Some(provider.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.overrides.model = Some(model.into());
        self
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.overrides.base_url = Some(base_url.into());
        self
    }

    pub fn root(mut self, root: impl Into<String>) -> Self {
        self.overrides.root = Some(root.into());
        self
    }

    /// Store override, e.g. `"memory"` or `"jsonl:/path/to/store"`.
    pub fn store(mut self, store: impl Into<String>) -> Self {
        self.overrides.store = Some(store.into());
        self
    }

    /// Expose only these tool names to the model and executor.
    ///
    /// Passing an empty list disables all registered tools for this agent.
    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.overrides.tool_allowlist = Some(to_string_vec(tools));
        self
    }

    pub fn enable_tools<I, S>(self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools(tools)
    }

    pub fn no_tools(self) -> Self {
        self.tools(std::iter::empty::<String>())
    }

    /// Add one configured subagent. When at least one subagent is configured,
    /// setup exposes the `spawn_sub_agent` delegation tool by default unless
    /// disabled.
    pub fn subagent(mut self, definition: AgentDefinition) -> Self {
        self.overrides
            .subagents
            .get_or_insert_with(Vec::new)
            .push(definition);
        self
    }

    pub fn subagents<I>(mut self, definitions: I) -> Self
    where
        I: IntoIterator<Item = AgentDefinition>,
    {
        self.overrides.subagents = Some(definitions.into_iter().collect());
        self
    }

    pub fn subagent_tools(mut self, enabled: bool) -> Self {
        self.overrides.subagents_enabled = Some(enabled);
        self
    }

    pub fn no_subagent_tools(self) -> Self {
        self.subagent_tools(false)
    }

    /// Attach in-process lifecycle hooks for policy, audit, and context
    /// injection. Hooks run at the core runner boundary.
    pub fn hooks(mut self, hooks: Arc<dyn HookDispatcher>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn no_hooks(mut self) -> Self {
        self.hooks = None;
        self
    }

    /// Hide these tool names from the model and reject them at execution time.
    pub fn disable_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.overrides.tool_denylist = Some(to_string_vec(tools));
        self
    }

    /// Expose only these skills in the prompt.
    pub fn skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.overrides.skill_allowlist = Some(to_string_vec(skills));
        self
    }

    pub fn enable_skills<I, S>(self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.skills(skills)
    }

    pub fn disable_skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.overrides.skill_denylist = Some(to_string_vec(skills));
        self
    }

    pub fn skill_autoload(mut self, enabled: bool) -> Self {
        self.overrides.skill_autoload = Some(enabled);
        self
    }

    pub fn no_skills_autoload(self) -> Self {
        self.skill_autoload(false)
    }

    pub fn mcp_sse<I, S>(mut self, endpoints: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.overrides.mcp_sse = Some(to_string_vec(endpoints));
        self
    }

    /// Runtime prompt-cache mode. Use `"auto"`/`"on"`/`"true"` or `"off"`.
    pub fn cache(mut self, mode: impl Into<String>) -> Self {
        self.overrides.cache = Some(mode.into());
        self
    }

    /// Runtime thinking mode, e.g. `"off"`, `"auto"`, `"enabled"`,
    /// `"high"`, or another value accepted by `Config`.
    pub fn thinking(mut self, mode: impl Into<String>) -> Self {
        self.overrides.thinking = Some(mode.into());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.overrides.max_tokens = Some(max_tokens);
        self
    }

    /// Enable or disable `AGENT.md` / `AGENTS.md` / `CLAUDE.md` loading.
    pub fn agent_md(mut self, enabled: bool) -> Self {
        self.overrides.agent_md = Some(enabled);
        self
    }

    pub fn agent_md_max_bytes(mut self, max_bytes_per_file: usize) -> Self {
        self.overrides.agent_md_max_bytes = Some(max_bytes_per_file);
        self
    }

    pub fn max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    pub async fn build(self) -> Result<Agent, SdkError> {
        let config = match self.config {
            Some(config) => config,
            None => Config::load(&self.overrides).map_err(SdkError::Config)?,
        };
        Agent::from_config_with_hooks_and_max_steps(config, self.hooks, self.max_steps).await
    }
}

/// Programmatic SDK agent.
///
/// `Agent` is stateful: consecutive `query` calls continue the same run state
/// unless you call `new_session`, `continue_session`, or `fork_from`.
pub struct Agent {
    runner: Arc<Runner>,
    state: RunState,
    wired: Option<setup::Wired>,
    max_steps: usize,
}

/// Small host-driven multi-agent container.
///
/// This intentionally does not invent a planner/worker protocol. It gives
/// SDK callers a typed place to hold multiple independent `Agent`s and route
/// work between them using application policy.
#[derive(Default)]
pub struct AgentTeam {
    agents: BTreeMap<String, Agent>,
}

impl AgentTeam {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_agent(mut self, name: impl Into<String>, agent: Agent) -> Self {
        self.insert(name, agent);
        self
    }

    pub fn insert(&mut self, name: impl Into<String>, agent: Agent) -> Option<Agent> {
        self.agents.insert(name.into(), agent)
    }

    pub fn remove(&mut self, name: &str) -> Option<Agent> {
        self.agents.remove(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.agents.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.agents.keys().map(String::as_str)
    }

    pub fn agent(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    pub fn agent_mut(&mut self, name: &str) -> Option<&mut Agent> {
        self.agents.get_mut(name)
    }

    pub async fn query(
        &mut self,
        name: &str,
        prompt: impl Into<String>,
    ) -> Result<AgentResponse, SdkError> {
        self.agent_mut(name)
            .ok_or_else(|| SdkError::UnknownAgent(name.to_string()))?
            .query(prompt)
            .await
    }

    pub async fn query_with_events<F>(
        &mut self,
        name: &str,
        prompt: impl Into<String>,
        on_event: F,
    ) -> Result<AgentResponse, SdkError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        self.agent_mut(name)
            .ok_or_else(|| SdkError::UnknownAgent(name.to_string()))?
            .query_with_events(prompt, on_event)
            .await
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub async fn from_env() -> Result<Self, SdkError> {
        AgentBuilder::new().build().await
    }

    pub async fn from_overrides(overrides: ConfigOverrides) -> Result<Self, SdkError> {
        AgentBuilder::new().overrides(overrides).build().await
    }

    pub async fn from_config(config: Config) -> Result<Self, SdkError> {
        Self::from_config_with_max_steps(config, DEFAULT_MAX_STEPS).await
    }

    pub async fn from_config_with_max_steps(
        config: Config,
        max_steps: usize,
    ) -> Result<Self, SdkError> {
        Self::from_config_with_hooks_and_max_steps(config, None, max_steps).await
    }

    pub async fn from_config_with_hooks_and_max_steps(
        config: Config,
        hooks: Option<Arc<dyn HookDispatcher>>,
        max_steps: usize,
    ) -> Result<Self, SdkError> {
        let wired = setup::wire_with_hooks(&config, hooks)
            .await
            .map_err(SdkError::Setup)?;
        let state = new_run_state(&config, SystemClock);
        Ok(Self {
            runner: wired.runner.clone(),
            state,
            wired: Some(wired),
            max_steps: max_steps.max(1),
        })
    }

    /// Build an SDK facade around a custom runner/state pair. This is useful
    /// for tests and advanced hosts that inject their own model/tool/store.
    pub fn from_parts(runner: Arc<Runner>, state: RunState) -> Self {
        Self {
            runner,
            state,
            wired: None,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    pub fn runner(&self) -> &Arc<Runner> {
        &self.runner
    }

    pub fn state(&self) -> &RunState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut RunState {
        &mut self.state
    }

    pub fn into_state(self) -> RunState {
        self.state
    }

    pub fn session_id(&self) -> SessionId {
        self.state.session_id
    }

    pub fn run_id(&self) -> RunId {
        self.state.run_id
    }

    /// Access the default shell wiring when this agent was built from config.
    pub fn wired(&self) -> Option<&setup::Wired> {
        self.wired.as_ref()
    }

    pub fn cancel(&self) {
        self.runner.cancel();
    }

    /// Start a fresh session/run while preserving the current workspace root.
    pub fn new_session(&mut self) {
        let workspace_root = self.state.workspace_root.clone();
        self.state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), SystemClock.now_ms());
        self.state.workspace_root = workspace_root;
    }

    pub async fn continue_session(&mut self, session_id: SessionId) -> Result<(), SdkError> {
        let mut next = {
            let wired = self
                .wired
                .as_ref()
                .ok_or(SdkError::Unavailable("session manager"))?;
            wired
                .sessions
                .continue_session(session_id, SystemClock.now_ms())
                .await
                .map_err(SdkError::Session)?
        };
        if next.workspace_root.is_none() {
            next.workspace_root = self.state.workspace_root.clone();
        }
        self.state = next;
        Ok(())
    }

    pub async fn fork_from(
        &mut self,
        run_id: RunId,
        at_message_index: usize,
    ) -> Result<(), SdkError> {
        let mut next = {
            let wired = self
                .wired
                .as_ref()
                .ok_or(SdkError::Unavailable("session manager"))?;
            wired
                .sessions
                .fork_from(run_id, at_message_index, SystemClock.now_ms())
                .await
                .map_err(SdkError::Session)?
        };
        if next.workspace_root.is_none() {
            next.workspace_root = self.state.workspace_root.clone();
        }
        self.state = next;
        Ok(())
    }

    pub async fn list_sessions(&self, limit: Option<usize>) -> Result<Vec<SessionInfo>, SdkError> {
        let wired = self
            .wired
            .as_ref()
            .ok_or(SdkError::Unavailable("session manager"))?;
        wired
            .sessions
            .list_sessions(limit)
            .await
            .map_err(SdkError::Session)
    }

    pub async fn search_sessions(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>, SdkError> {
        let wired = self
            .wired
            .as_ref()
            .ok_or(SdkError::Unavailable("session manager"))?;
        wired
            .sessions
            .search(query, limit)
            .await
            .map_err(SdkError::Session)
    }

    /// Run one text query and return the final assistant output.
    pub async fn query(&mut self, prompt: impl Into<String>) -> Result<AgentResponse, SdkError> {
        self.query_content(Content::text(prompt.into())).await
    }

    /// Run one query with arbitrary content parts.
    pub async fn query_content(&mut self, content: Content) -> Result<AgentResponse, SdkError> {
        self.query_content_with_events(content, |_| {}).await
    }

    /// Run one query and synchronously receive SDK events as they occur.
    ///
    /// The callback receives streaming `AssistantText` deltas while the model
    /// call is still in flight when the configured provider supports streaming.
    pub async fn query_with_events<F>(
        &mut self,
        prompt: impl Into<String>,
        on_event: F,
    ) -> Result<AgentResponse, SdkError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        self.query_content_with_events(Content::text(prompt.into()), on_event)
            .await
    }

    pub async fn query_content_with_events<F>(
        &mut self,
        content: Content,
        mut on_event: F,
    ) -> Result<AgentResponse, SdkError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let mut events = Vec::new();
        let resumed = self.state.event_seq > 0 || !self.state.history.is_empty();
        emit(
            AgentEvent::SessionStarted {
                session_id: self.state.session_id,
                run_id: self.state.run_id,
                resumed,
            },
            &mut events,
            &mut on_event,
        );

        if let Err(e) = self
            .runner
            .submit_user_message(&mut self.state, Message::User { content })
            .await
        {
            let message = e.to_string();
            emit_error(&mut events, &mut on_event, message, "submit");
            return Err(SdkError::Submit(e));
        }

        for _ in 0..self.max_steps {
            if self.state.step.is_terminal_or_paused() {
                return self.finish(events, on_event);
            }

            let out = self
                .step_once_with_stream(&mut events, &mut on_event)
                .await?;
            for event in out.events {
                emit(AgentEvent::CoreEvent { event }, &mut events, &mut on_event);
            }
        }

        if self.state.step.is_terminal_or_paused() {
            return self.finish(events, on_event);
        }

        let step = self.state.step.clone();
        emit_error(
            &mut events,
            &mut on_event,
            format!(
                "step budget exceeded after {} steps; final step={step:?}",
                self.max_steps
            ),
            "step",
        );
        Err(SdkError::StepBudgetExceeded {
            max_steps: self.max_steps,
            step,
        })
    }

    async fn step_once_with_stream<F>(
        &mut self,
        events: &mut Vec<AgentEvent>,
        on_event: &mut F,
    ) -> Result<StepOutput, SdkError>
    where
        F: FnMut(AgentEvent),
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let step_result = {
            let step_fut = self
                .runner
                .step_with_model_stream(&mut self.state, Some(tx));
            tokio::pin!(step_fut);
            let mut stream_open = true;
            loop {
                tokio::select! {
                    result = &mut step_fut => break result,
                    maybe = rx.recv(), if stream_open => {
                        match maybe {
                            Some(event) => emit_stream_event(event, events, on_event),
                            None => stream_open = false,
                        }
                    }
                }
            }
        };

        while let Ok(event) = rx.try_recv() {
            emit_stream_event(event, events, on_event);
        }

        match step_result {
            Ok(out) => Ok(out),
            Err(e) => {
                let message = e.to_string();
                emit_error(events, on_event, message, "step");
                Err(SdkError::Step(e))
            }
        }
    }

    fn finish<F>(
        &self,
        mut events: Vec<AgentEvent>,
        mut on_event: F,
    ) -> Result<AgentResponse, SdkError>
    where
        F: FnMut(AgentEvent),
    {
        match &self.state.step {
            Step::Done { final_text } => {
                let final_text = final_text.clone();
                emit(
                    AgentEvent::Result {
                        final_text: final_text.clone(),
                        session_id: self.state.session_id,
                        run_id: self.state.run_id,
                        usage: self.state.usage.clone(),
                    },
                    &mut events,
                    &mut on_event,
                );
                Ok(AgentResponse {
                    final_text,
                    session_id: self.state.session_id,
                    run_id: self.state.run_id,
                    usage: self.state.usage.clone(),
                    events,
                })
            }
            Step::Failed { reason } => {
                let reason = reason.clone();
                emit_error(&mut events, &mut on_event, reason.clone(), "run");
                Err(SdkError::RunFailed(reason))
            }
            Step::Paused { reason } => {
                let reason = reason.clone();
                emit_error(
                    &mut events,
                    &mut on_event,
                    format!("run paused: {reason:?}"),
                    "cancelled",
                );
                Err(SdkError::RunPaused(reason))
            }
            step => {
                emit_error(
                    &mut events,
                    &mut on_event,
                    format!("run stopped before terminal state: {step:?}"),
                    "step",
                );
                Err(SdkError::StepBudgetExceeded {
                    max_steps: self.max_steps,
                    step: step.clone(),
                })
            }
        }
    }
}

fn emit<F>(event: AgentEvent, events: &mut Vec<AgentEvent>, on_event: &mut F)
where
    F: FnMut(AgentEvent),
{
    on_event(event.clone());
    events.push(event);
}

fn emit_error<F>(
    events: &mut Vec<AgentEvent>,
    on_event: &mut F,
    message: String,
    stage: impl Into<String>,
) where
    F: FnMut(AgentEvent),
{
    emit(
        AgentEvent::Error {
            message,
            stage: stage.into(),
        },
        events,
        on_event,
    );
}

fn emit_stream_event<F>(event: ModelStreamEvent, events: &mut Vec<AgentEvent>, on_event: &mut F)
where
    F: FnMut(AgentEvent),
{
    match event {
        ModelStreamEvent::TextDelta(text) => {
            emit(AgentEvent::AssistantText { text }, events, on_event);
        }
        ModelStreamEvent::Reset => {
            emit(AgentEvent::AssistantStreamReset, events, on_event);
        }
    }
}

fn to_string_vec<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

fn new_run_state(config: &Config, clock: SystemClock) -> RunState {
    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), clock.now_ms());
    state.workspace_root = Some(workspace_root(config));
    state
}

fn workspace_root(config: &Config) -> String {
    config
        .fs
        .root
        .canonicalize()
        .unwrap_or_else(|_| config.fs.root.clone())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::core::cancel::CancelToken;
    use crate::core::error::{ModelError, ToolExecutorError};
    use crate::core::hook::NoopHookDispatcher;
    use crate::core::model::{LlmCaps, ModelAdapter, ModelReply, ModelRequest, TokenUsage};
    use crate::core::prelude::{ActiveToolSet, ToolContext, ToolExecutor};
    use crate::core::testing::{reply, CannedModel};
    use crate::core::tool::{Idempotency, PendingCall, ToolResult};
    use crate::storage::MemorySessionStore;

    struct NoopTools;

    #[async_trait]
    impl ToolExecutor for NoopTools {
        async fn execute(
            &self,
            call: &PendingCall,
            _ctx: &ToolContext,
            _cancel: CancelToken,
        ) -> Result<ToolResult, ToolExecutorError> {
            Ok(ToolResult::err(
                format!("no test tool registered: {}", call.tool_name),
                false,
                None,
            ))
        }

        fn idempotency_for(&self, _call: &PendingCall) -> Idempotency {
            Idempotency::Idempotent
        }
    }

    fn test_runner(model: Arc<dyn ModelAdapter>) -> Arc<Runner> {
        Arc::new(
            Runner::builder()
                .model(model)
                .tools(Arc::new(NoopTools))
                .store(Arc::new(MemorySessionStore::new()))
                .tools_provider(|_state: &RunState| ActiveToolSet::default())
                .build()
                .expect("runner build"),
        )
    }

    fn test_agent(model: Arc<dyn ModelAdapter>) -> Agent {
        let state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
        Agent::from_parts(test_runner(model), state)
    }

    #[test]
    fn builder_exposes_capability_and_agent_md_overrides() {
        let builder = Agent::builder()
            .tools(["fs_read", "fs_list"])
            .disable_tools(["sh_exec"])
            .skills(["git"])
            .disable_skills(["legacy"])
            .skill_autoload(false)
            .mcp_sse(["http://127.0.0.1:3000/sse"])
            .cache("off")
            .thinking("auto")
            .max_tokens(42)
            .subagent(
                AgentDefinition::new("reviewer", "review code", "Find correctness issues")
                    .tools(["fs_read"]),
            )
            .hooks(Arc::new(NoopHookDispatcher))
            .agent_md(false)
            .agent_md_max_bytes(256);

        assert_eq!(
            builder.overrides.tool_allowlist.as_deref(),
            Some(["fs_read".to_string(), "fs_list".to_string()].as_slice())
        );
        assert_eq!(
            builder.overrides.tool_denylist.as_deref(),
            Some(["sh_exec".to_string()].as_slice())
        );
        assert_eq!(
            builder.overrides.skill_allowlist.as_deref(),
            Some(["git".to_string()].as_slice())
        );
        assert_eq!(
            builder.overrides.skill_denylist.as_deref(),
            Some(["legacy".to_string()].as_slice())
        );
        assert_eq!(builder.overrides.skill_autoload, Some(false));
        assert_eq!(
            builder.overrides.mcp_sse.as_deref(),
            Some(["http://127.0.0.1:3000/sse".to_string()].as_slice())
        );
        assert_eq!(builder.overrides.cache.as_deref(), Some("off"));
        assert_eq!(builder.overrides.thinking.as_deref(), Some("auto"));
        assert_eq!(builder.overrides.max_tokens, Some(42));
        assert_eq!(
            builder
                .overrides
                .subagents
                .as_ref()
                .map(|defs| defs[0].name.as_str()),
            Some("reviewer")
        );
        assert!(builder.hooks.is_some());
        assert_eq!(builder.overrides.agent_md, Some(false));
        assert_eq!(builder.overrides.agent_md_max_bytes, Some(256));
    }

    #[tokio::test]
    async fn query_returns_final_text_and_events() {
        let model = Arc::new(CannedModel::new(vec![reply::text("hello from sdk")]));
        let mut agent = test_agent(model);

        let response = agent.query("hi").await.unwrap();

        assert_eq!(response.final_text, "hello from sdk");
        assert!(matches!(agent.state().step, Step::Done { .. }));
        assert!(response
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { resumed: false, .. })));
        assert!(response.events.iter().any(
            |e| matches!(e, AgentEvent::Result { final_text, .. } if final_text == "hello from sdk")
        ));
    }

    #[tokio::test]
    async fn agent_team_routes_queries_by_name() {
        let researcher = test_agent(Arc::new(CannedModel::new(vec![reply::text("research")])));
        let reviewer = test_agent(Arc::new(CannedModel::new(vec![reply::text("review")])));
        let mut team = AgentTeam::new()
            .with_agent("researcher", researcher)
            .with_agent("reviewer", reviewer);

        assert_eq!(
            team.names().collect::<Vec<_>>(),
            vec!["researcher", "reviewer"]
        );
        assert_eq!(
            team.query("reviewer", "check this")
                .await
                .unwrap()
                .final_text,
            "review"
        );
        assert!(matches!(
            team.query("missing", "hi").await.unwrap_err(),
            SdkError::UnknownAgent(name) if name == "missing"
        ));
    }

    struct StreamingModel {
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl ModelAdapter for StreamingModel {
        fn caps(&self) -> LlmCaps {
            LlmCaps {
                native_tool_use: true,
                streaming: true,
                ctx_len: 8192,
                ..Default::default()
            }
        }

        async fn turn(
            &self,
            _req: ModelRequest,
            _cancel: CancelToken,
        ) -> Result<ModelReply, ModelError> {
            Ok(ModelReply {
                text: "hello".into(),
                tool_calls: vec![],
                usage: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    ..Default::default()
                },
                thinking: vec![],
            })
        }

        async fn turn_stream(
            &self,
            req: ModelRequest,
            cancel: CancelToken,
            stream: Option<mpsc::UnboundedSender<ModelStreamEvent>>,
        ) -> Result<ModelReply, ModelError> {
            *self.calls.lock().unwrap() += 1;
            if let Some(tx) = stream {
                let _ = tx.send(ModelStreamEvent::TextDelta("hel".into()));
                let _ = tx.send(ModelStreamEvent::TextDelta("lo".into()));
            }
            self.turn(req, cancel).await
        }
    }

    #[tokio::test]
    async fn query_with_events_emits_streaming_text() {
        let model = Arc::new(StreamingModel {
            calls: Mutex::new(0),
        });
        let mut agent = test_agent(model.clone());
        let mut seen = Vec::new();

        let response = agent
            .query_with_events("hi", |event| seen.push(event))
            .await
            .unwrap();

        assert_eq!(response.final_text, "hello");
        assert_eq!(*model.calls.lock().unwrap(), 1);
        let streamed = seen
            .iter()
            .filter_map(|event| match event {
                AgentEvent::AssistantText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(streamed, "hello");
    }
}
