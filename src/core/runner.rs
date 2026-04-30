//! Runner + `step` FSM。

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use crate::core::cache::CachePolicy;
use crate::core::cancel::CancelToken;
use crate::core::clock::Clock;
use crate::core::clock::SystemClock;
use crate::core::compactor::Compactor;
use crate::core::error::{ErrorClass, ModelError, RuntimeError, StoreErrClass, StoreError};
use crate::core::event::Event;
use crate::core::model::{LlmCaps, ModelAdapter, ModelReply, ModelRequest};
use crate::core::prompt::{render_cacheable_blocks, render_runtime_blocks, PromptBlock};
use crate::core::provider::{ActiveToolSet, ActiveToolSetProvider};
use crate::core::retry::RetryPolicy;
use crate::core::run_state::RunState;
use crate::core::run_state::Usage;
use crate::core::step::{PauseReason, Step};
use crate::core::store::SessionStore;
use crate::core::summary_recall::insert_summary_recall_before_latest_user;
use crate::core::thinking::ThinkingConfig;
use crate::core::tool::{
    Idempotency, PendingCall, SideEffects, ToolContext, ToolDescriptor, ToolExecutor, ToolResult,
    TOOL_PROTOCOL_ERROR_TOOL,
};
use crate::core::types::{Content, Message};
use futures::FutureExt;
use sha2::{Digest, Sha256};

/// How `Runner` populates `ModelRequest::prompt_cache_key`.
///
/// The provider uses this string as a routing-affinity hint: requests
/// sharing the same key (and the same prefix bytes) are sent to the same
/// engine replica, where the cached KV is already warm. **Picking the right
/// granularity is the difference between a cold first turn and an instant
/// hit on every new session of the same agent.**
///
/// Empirical numbers on `openai/gpt-5.4-nano` via OpenRouter (5-turn loop,
/// ~7.5k token prompt at turn 5):
/// - `Session` (per-conversation): turn-1 cache_read = 0; turn-5 ratio = 78%.
/// - `PrefixHash` (per-agent-config): turn-1 cache_read = 2816; turn-5
///   ratio = 92%. Same prefix bytes, only the routing key differs.
///
/// The trade-off is OpenAI's published per-(prefix, key) throughput
/// ceiling of ~15 RPM. For multi-tenant servers serving many concurrent
/// users, `Session` keeps each user's traffic on their own replica;
/// `PrefixHash` would funnel them all to one and hit the limit.
#[derive(Clone, Debug)]
pub enum CacheKeyStrategy {
    /// Hash of the final stable prompt prefix plus canonical tool schemas.
    /// **Default.** Best for CLIs and single-user agents where every session
    /// shares the same identity and cross-session cache reuse is the goal.
    PrefixHash,
    /// Use the per-conversation `state.session_id`. Best for high-RPS
    /// multi-tenant servers where each session must route independently
    /// to stay under the per-key throughput ceiling.
    Session,
    /// A caller-supplied stable key. Use for fleet-level keys (e.g. one
    /// key per deployed agent name) when neither default fits.
    Fixed(String),
    /// Don't send a `prompt_cache_key` at all. Fall back to the provider's
    /// automatic prefix routing.
    None,
}

impl Default for CacheKeyStrategy {
    fn default() -> Self {
        CacheKeyStrategy::PrefixHash
    }
}

fn hash_cache_fingerprint(cacheable_prefix: &str, tools: &[ToolDescriptor]) -> String {
    let mut h = Sha256::new();
    h.update(b"muagent-prompt-v2\0");
    h.update(cacheable_prefix.as_bytes());

    let mut ordered_tools: Vec<&ToolDescriptor> = tools.iter().collect();
    ordered_tools.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in ordered_tools {
        h.update(b"\0tool\0");
        match serde_json::to_string(tool) {
            Ok(s) => h.update(s.as_bytes()),
            Err(_) => {
                h.update(tool.name.as_bytes());
                h.update(b"\0");
                h.update(tool.description.as_bytes());
            }
        }
    }

    hash_prefix_hex(h)
}

fn hash_prefix_hex(h: Sha256) -> String {
    let bytes = h.finalize();
    let mut out = String::with_capacity(16);
    for b in &bytes[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    format!("muagent-{out}")
}

fn active_prompt_blocks(ats: &ActiveToolSet) -> Vec<PromptBlock> {
    if !ats.prompt_blocks.is_empty() {
        return ats.prompt_blocks.clone();
    }
    if ats.prompt_augmentation.trim().is_empty() {
        Vec::new()
    } else {
        vec![PromptBlock::session_sticky(
            "active_tool_set.prompt_augmentation",
            ats.prompt_augmentation.clone(),
        )]
    }
}

fn model_capability_prompt(caps: &LlmCaps, tools: &[ToolDescriptor]) -> String {
    let has_fs_read = has_tool(tools, "fs_read");
    let has_fs_stat = has_tool(tools, "fs_stat");
    let has_sh_exec = has_tool(tools, "sh_exec");

    let mut out = String::from("## Model capability guidance\n");
    if caps.vision {
        out.push_str(
            "- This model supports image inputs. If visual content matters, inspect the image instead of guessing from filenames, MIME types, or surrounding text.\n",
        );
        if has_fs_read {
            out.push_str(
                "- `fs_read` can return supported PNG/JPEG/GIF/WebP files as image attachments visible to this model. Use it when screenshots, photos, diagrams, UI states, charts, or OCR-relevant images are material; leave `force_text=false` for visual inspection.\n",
            );
        } else {
            out.push_str(
                "- No local image-reading tool is active. You can still reason about images already attached by the user or returned by other tools.\n",
            );
        }
    } else {
        out.push_str(
            "- This model does not support image inputs. Image attachments from tools are omitted before the next model turn.\n",
        );
        if has_fs_read {
            out.push_str(
                "- Do not use `fs_read` on image files for visual inspection or OCR. It is still valid for text files. For image knowledge, use available text-producing alternatives: ",
            );
            let mut alternatives = Vec::new();
            if has_fs_stat {
                alternatives.push("`fs_stat` for file metadata");
            }
            if has_sh_exec {
                alternatives
                    .push("`sh_exec` with OCR or image-processing commands exposed by the shell allowlist");
            }
            alternatives.push("a user-provided description");
            out.push_str(&alternatives.join(", "));
            out.push_str(".\n");
        } else {
            out.push_str(
                "- Use only available text-producing tools or ask the user for a description when the task depends on image content.\n",
            );
        }
    }
    out
}

fn adapt_tool_descriptors_for_caps(
    tools: &[ToolDescriptor],
    caps: &LlmCaps,
) -> Vec<ToolDescriptor> {
    tools
        .iter()
        .map(|tool| {
            let mut tool = tool.clone();
            if tool.name == "fs_read" && !tool.description.contains("Model capability note:") {
                let note = if caps.vision {
                    "Model capability note: this model supports vision, so supported image attachments returned by `fs_read` are visible on the next model turn. Leave `force_text=false` for visual inspection."
                } else {
                    "Model capability note: this model does not support vision. Do not call `fs_read` on image files for visual inspection or OCR; image attachments are omitted before the next model turn. Use text-producing alternatives instead."
                };
                tool.description = format!("{}\n\n{note}", tool.description.trim_end());
            }
            tool
        })
        .collect()
}

fn has_tool(tools: &[ToolDescriptor], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

fn append_section(out: &mut String, section: &str) {
    let section = section.trim();
    if section.is_empty() {
        return;
    }
    if !out.trim().is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(section);
}

/// Snapshot of the mutating fields of `RunState`. Cheap for ordinary appends:
/// it clones metadata ledgers but not `history`; a `truncate` rolls back the
/// last pushed messages.
///
/// Used by `Runner::commit` to make state mutation + persist atomic: if
/// the store rejects the write, the in-memory state is rolled back to
/// exactly what the caller had before. Without this, a transient store
/// error would leave `state.event_seq` advanced past disk and every
/// subsequent CAS check would fail with `StaleState`, bricking the run
/// until the host reloaded from disk.
struct StateSnapshot {
    step: Step,
    event_seq: u64,
    updated_ms: i64,
    history_len: usize,
    history_ids: Vec<String>,
    next_message_seq: u64,
    next_checkpoint_seq: u64,
    compaction_checkpoints: Vec<crate::core::run_state::CompactionCheckpoint>,
    usage: Usage,
}

impl StateSnapshot {
    fn take(state: &RunState) -> Self {
        Self {
            step: state.step.clone(),
            event_seq: state.event_seq,
            updated_ms: state.updated_ms,
            history_len: state.history.len(),
            history_ids: state.history_ids.clone(),
            next_message_seq: state.next_message_seq,
            next_checkpoint_seq: state.next_checkpoint_seq,
            compaction_checkpoints: state.compaction_checkpoints.clone(),
            usage: state.usage.clone(),
        }
    }

    fn restore(self, state: &mut RunState) {
        state.step = self.step;
        state.event_seq = self.event_seq;
        state.updated_ms = self.updated_ms;
        state.history.truncate(self.history_len);
        state.history_ids = self.history_ids;
        state.next_message_seq = self.next_message_seq;
        state.next_checkpoint_seq = self.next_checkpoint_seq;
        state.compaction_checkpoints = self.compaction_checkpoints;
        state.usage = self.usage;
    }
}

/// Runner::step 的返回。`advanced = false` 表示本次 step 是 no-op
/// (例如已经 Done / Failed / Paused)。
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub events: Vec<Event>,
    pub advanced: bool,
}

pub struct Runner {
    model: Arc<dyn ModelAdapter>,
    tools: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,
    tools_provider: Arc<dyn ActiveToolSetProvider>,
    clock: Arc<dyn Clock>,
    base_system_prompt: String,
    /// Per-conversation cancel handle. Behind a Mutex so `submit_user_message`
    /// can swap in a fresh `CancelToken` for each new round — otherwise a
    /// previous `cancel()` would stick across `/new`-style resets and trip
    /// the cancel gate in every subsequent step. Concrete bug source pre-fix:
    /// once host called `cancel()`, every later `step()` immediately Paused.
    cancel_token: Mutex<CancelToken>,
    compactor: Option<Arc<dyn Compactor>>,
    cache_policy: CachePolicy,
    thinking_config: ThinkingConfig,
    retry_policy: RetryPolicy,
    cache_key_strategy: CacheKeyStrategy,
    summary_recall: bool,
}

impl Runner {
    pub fn builder() -> RunnerBuilder {
        RunnerBuilder::default()
    }

    /// Trigger the *current* round's cancel token. Cooperative — tools must
    /// honor it (they receive it via `Tool::run`'s `cancel` parameter).
    pub fn cancel(&self) {
        self.cancel_token.lock().unwrap().trigger()
    }

    /// Snapshot of the current round's cancel token (useful for hosts that
    /// want to wire their own Ctrl-C handler).
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel_token.lock().unwrap().clone()
    }

    /// 提交一条新用户消息。
    /// 仅在 `Ready` / `Done` / `Failed` 状态下合法;其它状态返 `SubmitDuringRun`。
    ///
    /// **Transactional**: routed through `Runner::commit`, so a `persist`
    /// failure rolls `state` back to exactly what the caller passed in.
    pub async fn submit_user_message(
        &self,
        state: &mut RunState,
        msg: Message,
    ) -> Result<(), RuntimeError> {
        match &state.step {
            Step::Ready | Step::Done { .. } | Step::Failed { .. } => {}
            _ => return Err(RuntimeError::SubmitDuringRun),
        }
        // Reset cancel token for the new conversational round (prior
        // round's Ctrl-C should not stick).
        *self.cancel_token.lock().unwrap() = CancelToken::new();
        let now = self.clock.now_ms();
        self.commit(state, |s| {
            s.push_message(msg);
            s.step = Step::Ready;
            s.updated_ms = now;
            let seq = s.next_seq();
            vec![Event::UserMessage { seq }]
        })
        .await?;
        Ok(())
    }

    /// Run one FSM step.
    pub async fn step(&self, state: &mut RunState) -> Result<StepOutput, RuntimeError> {
        // Cancel gate
        if self.cancel_token.lock().unwrap().triggered() && !state.step.is_terminal_or_paused() {
            let events = self.pause_host_requested(state).await?;
            return Ok(StepOutput {
                events,
                advanced: true,
            });
        }

        match state.step.clone() {
            Step::Ready => self.on_ready(state).await,
            Step::ModelTurn => self.on_model_turn(state).await,
            Step::ToolBatch { calls, cursor } => self.on_tool_batch(state, calls, cursor).await,
            Step::ToolIntent { call, .. } => self.on_tool_intent_recover(state, call).await,
            Step::Paused { .. } | Step::Done { .. } | Step::Failed { .. } => Ok(StepOutput {
                events: vec![],
                advanced: false,
            }),
        }
    }

    async fn on_ready(&self, state: &mut RunState) -> Result<StepOutput, RuntimeError> {
        let events = self
            .commit(state, |s| {
                s.step = Step::ModelTurn;
                let seq = s.next_seq();
                vec![Event::StepAdvanced {
                    to: "model_turn".into(),
                    seq,
                }]
            })
            .await?;
        Ok(StepOutput {
            events,
            advanced: true,
        })
    }

    /// Call the model with retry on transient / rate-limit errors.
    /// Other error classes (Auth, Fatal, InvalidRequest, Parse,
    /// ContextOverflow) surface immediately — retrying won't help.
    async fn model_turn_with_retry(&self, req: ModelRequest) -> Result<ModelReply, RuntimeError> {
        let mut attempt: u32 = 1;
        let mut req = req;
        let mut added_empty_reply_continuation = false;
        loop {
            // Snapshot the cancel token for this attempt — never hold the
            // Mutex across an await (clippy::await_holding_lock; would
            // deadlock if a peer tried to swap the token mid-call).
            let cancel = self.cancel_token.lock().unwrap().child();
            let attempt_start_ms = self.clock.now_ms();
            match self.model.turn(req.clone(), cancel).await {
                Ok(reply) => {
                    let duration_ms = (self.clock.now_ms() - attempt_start_ms).max(0);
                    let empty = model_reply_is_empty(&reply);
                    tracing::info!(
                        target: "muagent::model",
                        attempt,
                        duration_ms,
                        prompt_tokens = reply.usage.prompt_tokens,
                        completion_tokens = reply.usage.completion_tokens,
                        cache_read_tokens = reply.usage.cache_read_tokens,
                        cache_write_tokens = reply.usage.cache_write_tokens,
                        thinking_tokens = reply.usage.thinking_tokens,
                        tool_calls = reply.tool_calls.len(),
                        text_chars = reply.text.chars().count(),
                        empty,
                        "model turn completed"
                    );
                    if empty {
                        let e = ModelError::Transient("empty model response".into());
                        if attempt >= self.retry_policy.max_attempts {
                            return Err(e.into());
                        }
                        if !added_empty_reply_continuation
                            && history_ends_with_tool_result(&req.messages)
                        {
                            req.messages.push(Message::User {
                                content: Content::text("Please continue."),
                            });
                            added_empty_reply_continuation = true;
                        }
                        attempt += 1;
                        let wait = self.retry_policy.backoff_for(attempt, None);
                        self.clock.sleep(wait).await;
                        continue;
                    }
                    return Ok(reply);
                }
                Err(ModelError::Cancelled) => {
                    let duration_ms = (self.clock.now_ms() - attempt_start_ms).max(0);
                    tracing::info!(
                        target: "muagent::model",
                        attempt,
                        duration_ms,
                        "model turn cancelled"
                    );
                    return Err(RuntimeError::Cancelled);
                }
                Err(e) => {
                    let duration_ms = (self.clock.now_ms() - attempt_start_ms).max(0);
                    let (retryable, retry_after_ms) = match &e {
                        ModelError::Transient(_) => (true, None),
                        ModelError::RateLimited { retry_after_ms } => (true, *retry_after_ms),
                        _ => (false, None),
                    };
                    tracing::warn!(
                        target: "muagent::model",
                        attempt,
                        duration_ms,
                        retryable,
                        retry_after_ms,
                        max_attempts = self.retry_policy.max_attempts,
                        error = %e,
                        "model turn failed"
                    );
                    if !retryable || attempt >= self.retry_policy.max_attempts {
                        return Err(e.into());
                    }
                    attempt += 1;
                    let wait = self.retry_policy.backoff_for(attempt, retry_after_ms);
                    self.clock.sleep(wait).await;
                }
            }
        }
    }

    async fn on_model_turn(&self, state: &mut RunState) -> Result<StepOutput, RuntimeError> {
        let ats = match AssertUnwindSafe(self.tools_provider.provide(state))
            .catch_unwind()
            .await
        {
            Ok(ats) => ats,
            Err(panic) => {
                tracing::warn!(
                    target: "muagent::provider",
                    panic = %panic_brief(panic),
                    "active tool-set provider panicked; continuing with no dynamic tools"
                );
                ActiveToolSet::default()
            }
        };

        // Build prompt blocks. Cacheable blocks stay in the stable prefix;
        // runtime blocks are appended later after day-level facts.
        let model_caps = self.model.caps();
        let tools = adapt_tool_descriptors_for_caps(&ats.tools, &model_caps);
        let prompt_blocks = active_prompt_blocks(&ats);
        let mut system = self.base_system_prompt.clone();
        append_section(&mut system, &model_capability_prompt(&model_caps, &tools));
        append_section(&mut system, &render_cacheable_blocks(&prompt_blocks));

        // Optional auto-compaction. Run it against a cloned candidate state:
        // third-party compactors are allowed to mutate their input, but those
        // mutations must not leak unless Runner commits the final history and
        // emits a HistoryCompacted event.
        let mut pre_events: Vec<Event> = Vec::new();
        if let Some(c) = &self.compactor {
            let mut candidate = state.clone();
            let cancel = self.cancel_token.lock().unwrap().child();
            match c.maybe_compact(&mut candidate, &system, cancel).await {
                Ok(Some(ev)) => {
                    let snap_history = state.history.clone();
                    let compacted_history = candidate.history;
                    let compacted_history_ids = candidate.history_ids;
                    let compacted_next_message_seq = candidate.next_message_seq;
                    let compacted_next_checkpoint_seq = candidate.next_checkpoint_seq;
                    let compacted_checkpoints = candidate.compaction_checkpoints;
                    let event = Event::HistoryCompacted {
                        replaced_turns: ev.replaced_turns,
                        replaced_messages: ev.replaced_messages,
                        saved_tokens_estimate: ev.saved_tokens_estimate,
                        checkpoint_id: ev.checkpoint_id,
                        summary_message_id: ev.summary_message_id,
                        first_kept_message_id: ev.first_kept_message_id,
                        seq: 0, // assigned inside commit
                    };
                    let committed = self
                        .commit(state, move |s| {
                            s.history = compacted_history;
                            s.history_ids = compacted_history_ids;
                            s.next_message_seq = compacted_next_message_seq;
                            s.next_checkpoint_seq = compacted_next_checkpoint_seq;
                            s.compaction_checkpoints = compacted_checkpoints;
                            let seq = s.next_seq();
                            let mut e = event;
                            if let Event::HistoryCompacted {
                                seq: ref mut esq, ..
                            } = e
                            {
                                *esq = seq;
                            }
                            vec![e]
                        })
                        .await;
                    match committed {
                        Ok(es) => pre_events.extend(es),
                        Err(e) => {
                            // StateSnapshot only truncates history on rollback;
                            // compaction replaces the middle, so restore the
                            // full pre-compaction history on persist failure.
                            state.history = snap_history;
                            return Err(e);
                        }
                    }
                }
                Ok(None) => {}
                Err(RuntimeError::Cancelled) => {
                    let mut events = pre_events;
                    events.extend(self.pause_host_requested(state).await?);
                    return Ok(StepOutput {
                        events,
                        advanced: true,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        target: "muagent::compaction",
                        error = %e,
                        class = error_class_label(e.classify()),
                        "auto-compaction failed; continuing without compaction"
                    );
                }
            }
        }

        // L2 runtime facts. Keep minimal by default — day-level date + turn.
        let facts = crate::core::prompt::RuntimeFacts {
            now_ms: self.clock.now_ms(),
            turn: state.usage.turns.saturating_add(1),
            extra: vec![],
        };
        let mut runtime_context = facts.render();
        append_section(&mut runtime_context, &render_runtime_blocks(&prompt_blocks));

        let prompt_cache_key = match &self.cache_key_strategy {
            CacheKeyStrategy::PrefixHash => Some(hash_cache_fingerprint(&system, &tools)),
            CacheKeyStrategy::Session => Some(state.session_id.to_string()),
            CacheKeyStrategy::Fixed(s) => Some(s.clone()),
            CacheKeyStrategy::None => None,
        };

        let mut messages = state.history.clone();
        if self.summary_recall {
            insert_summary_recall_before_latest_user(&mut messages);
        }

        let req = ModelRequest {
            system,
            runtime_context,
            messages,
            tools,
            temperature: None,
            stream: false,
            cache: self.cache_policy,
            thinking: self.thinking_config.clone(),
            // Routing-affinity key. Strategy is picked at Runner build time;
            // see `CacheKeyStrategy` docs for the cold-start vs throughput
            // trade-off. Empirically `PrefixHash` (the default) takes turn-1
            // cache_read from 0 to ~37% on `openai/gpt-5.4-nano` because
            // every new session lands on a backend that already cached the
            // agent's stable prefix from prior sessions.
            prompt_cache_key,
        };

        let reply = match self.model_turn_with_retry(req).await {
            Ok(reply) => reply,
            Err(RuntimeError::Cancelled) => {
                let mut events = pre_events;
                events.extend(self.pause_host_requested(state).await?);
                return Ok(StepOutput {
                    events,
                    advanced: true,
                });
            }
            Err(RuntimeError::Store(e)) => return Err(RuntimeError::Store(e)),
            Err(e) => {
                let mut events = pre_events;
                events.extend(self.fail_run(state, e).await?);
                return Ok(StepOutput {
                    events,
                    advanced: true,
                });
            }
        };

        // Commit the model reply atomically: usage updates + assistant push
        // + step transition + events. On persist failure, roll back so a
        // retry can re-call the model without state divergence.
        let usage = reply.usage.clone();
        let text = reply.text.clone();
        let thinking = reply.thinking.clone();
        let tool_calls = reply.tool_calls.clone();
        let post_events = self
            .commit(state, |s| {
                s.usage.tokens_prompt = s.usage.tokens_prompt.saturating_add(usage.prompt_tokens);
                s.usage.tokens_completion = s
                    .usage
                    .tokens_completion
                    .saturating_add(usage.completion_tokens);
                s.usage.cost_usd += usage.cost_usd.unwrap_or(0.0);
                s.usage.turns = s.usage.turns.saturating_add(1);
                s.usage.tokens_cache_read = s
                    .usage
                    .tokens_cache_read
                    .saturating_add(usage.cache_read_tokens);
                s.usage.tokens_cache_write = s
                    .usage
                    .tokens_cache_write
                    .saturating_add(usage.cache_write_tokens);
                s.usage.tokens_thinking = s
                    .usage
                    .tokens_thinking
                    .saturating_add(usage.thinking_tokens);

                s.push_assistant_with_thinking(&text, tool_calls.clone(), thinking);
                let asst_seq = s.next_seq();
                let mut events = vec![Event::AssistantMessage {
                    text: text.clone(),
                    seq: asst_seq,
                }];
                if tool_calls.is_empty() {
                    s.step = Step::Done { final_text: text };
                    let end_seq = s.next_seq();
                    events.push(Event::SessionEnd {
                        ok: true,
                        seq: end_seq,
                    });
                } else {
                    s.step = Step::ToolBatch {
                        calls: tool_calls,
                        cursor: 0,
                    };
                }
                events
            })
            .await?;

        let mut events = pre_events;
        events.extend(post_events);
        Ok(StepOutput {
            events,
            advanced: true,
        })
    }

    async fn pause_host_requested(&self, state: &mut RunState) -> Result<Vec<Event>, RuntimeError> {
        let now = self.clock.now_ms();
        self.commit(state, |s| {
            s.step = Step::Paused {
                reason: PauseReason::HostRequested,
            };
            s.updated_ms = now;
            let seq = s.next_seq();
            vec![Event::Paused {
                reason: "host_requested".into(),
                seq,
            }]
        })
        .await
    }

    async fn fail_run(
        &self,
        state: &mut RunState,
        err: RuntimeError,
    ) -> Result<Vec<Event>, RuntimeError> {
        let class = error_class_label(err.classify()).to_string();
        let brief = error_brief(&err, 500);
        let reason = brief.clone();
        let now = self.clock.now_ms();
        self.commit(state, move |s| {
            s.step = Step::Failed { reason };
            s.updated_ms = now;
            let err_seq = s.next_seq();
            let end_seq = s.next_seq();
            vec![
                Event::ErrorRaised {
                    class,
                    brief,
                    seq: err_seq,
                },
                Event::SessionEnd {
                    ok: false,
                    seq: end_seq,
                },
            ]
        })
        .await
    }

    async fn on_tool_batch(
        &self,
        state: &mut RunState,
        calls: Vec<PendingCall>,
        cursor: usize,
    ) -> Result<StepOutput, RuntimeError> {
        if cursor >= calls.len() {
            // Emit a StepAdvanced event for this transition. Without it,
            // we'd persist a state change with no events at the same
            // event_seq as the prior save — JsonlStore (and any strict
            // SessionStore) flags that as `StaleState`. The matching
            // pattern is in on_ready.
            let events = self
                .commit(state, |s| {
                    s.step = Step::ModelTurn;
                    let seq = s.next_seq();
                    vec![Event::StepAdvanced {
                        to: "model_turn".into(),
                        seq,
                    }]
                })
                .await?;
            return Ok(StepOutput {
                events,
                advanced: true,
            });
        }

        let call = calls[cursor].clone();
        let internal_result = internal_tool_result(&call);

        // AtMostOnce protection: persist intent before execute. Same
        // StepAdvanced trick — the state change must be reflected in
        // event_seq so durable stores can distinguish it from a stale
        // overwrite.
        let idem = if internal_result.is_some() {
            Idempotency::Idempotent
        } else {
            self.tools.idempotency_for(&call)
        };
        if idem == Idempotency::AtMostOnce {
            let now = self.clock.now_ms();
            let call_for_intent = call.clone();
            self.commit(state, |s| {
                s.step = Step::ToolIntent {
                    call: call_for_intent,
                    intent_ms: now,
                };
                let seq = s.next_seq();
                vec![Event::StepAdvanced {
                    to: "tool_intent".into(),
                    seq,
                }]
            })
            .await?;
        }

        let ctx = ToolContext {
            session_id: state.session_id,
            run_id: state.run_id,
            turn: state.usage.turns,
        };
        let start_ms = self.clock.now_ms();
        let result = match internal_result {
            Some(result) => result,
            None => {
                let cancel = self.cancel_token.lock().unwrap().child();
                self.tools
                    .execute(&call, &ctx, cancel)
                    .await
                    .unwrap_or_else(ToolResult::framework_error)
            }
        };
        let duration_ms = (self.clock.now_ms() - start_ms).max(0) as u32;

        // Audit write. Best-effort: a store error here must not abort the
        // turn (audit is observability, not correctness), but we DO surface
        // it via tracing so it shows up in operator logs / monitoring.
        let audit = crate::core::store::ToolAuditRecord {
            ts_ms: start_ms,
            session_id: state.session_id,
            run_id: state.run_id,
            call_id: call.id.clone(),
            tool_name: call.tool_name.clone(),
            side_effects: side_effects_label(if call.tool_name == TOOL_PROTOCOL_ERROR_TOOL {
                SideEffects::ReadOnly
            } else {
                self.tools.side_effects_for(&call)
            }),
            ok: result.ok,
            retryable: result.retryable,
            args_hash: call.args_hash.clone(),
            args_sanitized: crate::core::sanitize::sanitize_json(call.args.clone()),
            brief: result.brief(),
            duration_ms,
        };
        if let Err(e) = self.store.record_tool_audit(&audit).await {
            tracing::warn!(
                target: "muagent::audit",
                error = %e,
                tool = %call.tool_name,
                call_id = %call.id,
                "tool audit write failed (turn continues; audit lost)"
            );
        }

        // Single transactional commit:state mutation + both events. If
        // persist fails, rollback puts us back in `ToolBatch{cursor=cursor}`
        // and the next step retries (Idempotent tools re-exec; AtMostOnce
        // already persisted intent so recover path triggers).
        let call_id = call.id.clone();
        let tool_name = call.tool_name.clone();
        let res_ok = result.ok;
        let res_retryable = result.retryable;
        let res_brief = result.brief();
        let result_for_push = result.clone();
        let events = self
            .commit(state, |s| {
                let start_seq = s.next_seq();
                s.push_tool_result(&call_id, &result_for_push);
                let end_seq = s.next_seq();
                s.usage.tool_calls = s.usage.tool_calls.saturating_add(1);
                s.step = Step::ToolBatch {
                    calls,
                    cursor: cursor + 1,
                };
                vec![
                    Event::ToolCallStart {
                        call_id: call_id.clone(),
                        tool: tool_name,
                        seq: start_seq,
                    },
                    Event::ToolCallEnd {
                        call_id,
                        ok: res_ok,
                        retryable: res_retryable,
                        brief: res_brief,
                        seq: end_seq,
                    },
                ]
            })
            .await?;
        Ok(StepOutput {
            events,
            advanced: true,
        })
    }

    async fn on_tool_intent_recover(
        &self,
        state: &mut RunState,
        call: PendingCall,
    ) -> Result<StepOutput, RuntimeError> {
        // All mutation + event emission inside one transactional commit so
        // a persist failure leaves us in the original ToolIntent state and
        // the next step retries cleanly.
        let recover_id = call.id;
        let events = self
            .commit(state, |s| {
                let r = ToolResult::err(
                    "Previous execution was interrupted; effect status unknown. \
                     Use read-only tools to verify state before retrying.",
                    false,
                    Some("Verify before retry".into()),
                );
                s.push_tool_result(&recover_id, &r);

                // Wire-format hygiene: orphan tool_uses in the prior Assistant
                // message need synthetic tool_results, otherwise Anthropic /
                // Gemini reject the next model call.
                let mut events: Vec<Event> = Vec::new();
                let recover_seq = s.next_seq();
                events.push(Event::ToolIntentRecovered {
                    call_id: recover_id.clone(),
                    seq: recover_seq,
                });
                for orphan_id in find_orphan_tool_calls(&s.history) {
                    let skipped = ToolResult::err(
                        "Skipped: an earlier tool in this batch was interrupted \
                         (AtMostOnce protection); reissue this call if still needed.",
                        false,
                        None,
                    );
                    s.push_tool_result(&orphan_id, &skipped);
                    let seq = s.next_seq();
                    events.push(Event::ToolIntentRecovered {
                        call_id: orphan_id,
                        seq,
                    });
                }
                s.step = Step::ModelTurn;
                events
            })
            .await?;
        Ok(StepOutput {
            events,
            advanced: true,
        })
    }

    /// **The single state-mutation choke-point.** Snapshots `state`'s mutating
    /// fields, runs the closure (which mutates `state` and returns the events
    /// to persist), then calls `save_delta`. On Err, rolls back so the caller's
    /// `state` is exactly as it was on entry — preventing the
    /// `state.event_seq` divergence bug class.
    ///
    /// Use this for *every* persist call site in the FSM; do not call
    /// `persist` directly except in paths that have already advanced state
    /// asynchronously and accept that a persist failure means "reload from
    /// disk to recover" (currently: nothing — all paths use `commit`).
    async fn commit(
        &self,
        state: &mut RunState,
        f: impl FnOnce(&mut RunState) -> Vec<Event>,
    ) -> Result<Vec<Event>, RuntimeError> {
        let snap = StateSnapshot::take(state);
        let events = f(state);
        state.updated_ms = self.clock.now_ms();
        state.ensure_history_ids();
        state.retain_active_compaction_checkpoints();
        if let Err(e) = state.validate_history_identity() {
            snap.restore(state);
            return Err(RuntimeError::Store(StoreError::Corrupt(format!(
                "history identity invariant failed before save: {e}"
            ))));
        }
        match self.store.save_delta(state, &events).await {
            Ok(()) => Ok(events),
            Err(e) => {
                snap.restore(state);
                Err(RuntimeError::Store(e))
            }
        }
    }
}

fn side_effects_label(s: SideEffects) -> String {
    match s {
        SideEffects::ReadOnly => "read_only".into(),
        SideEffects::Mutating => "mutating".into(),
        SideEffects::Destructive => "destructive".into(),
        SideEffects::CapabilityMutation => "capability_mutation".into(),
    }
}

fn error_class_label(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::ToolFailure { retryable: true } => "tool_failure_retryable",
        ErrorClass::ToolFailure { retryable: false } => "tool_failure",
        ErrorClass::ProviderTransient => "provider_transient",
        ErrorClass::ProviderFatal => "provider_fatal",
        ErrorClass::ContextTooLong => "context_too_long",
        ErrorClass::Store(StoreErrClass::Transient) => "store_transient",
        ErrorClass::Store(StoreErrClass::Conflict) => "store_conflict",
        ErrorClass::Store(StoreErrClass::Fatal) => "store_fatal",
        ErrorClass::Bug => "bug",
        ErrorClass::Cancelled => "cancelled",
    }
}

fn error_brief(err: &RuntimeError, max_chars: usize) -> String {
    let text = err.to_string();
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn panic_brief(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = p.downcast_ref::<String>() {
        return s.clone();
    }
    "non-string panic payload".into()
}

fn model_reply_is_empty(reply: &ModelReply) -> bool {
    reply.text.trim().is_empty() && reply.tool_calls.is_empty()
}

fn history_ends_with_tool_result(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResult { .. }))
}

fn internal_tool_result(call: &PendingCall) -> Option<ToolResult> {
    if call.tool_name != TOOL_PROTOCOL_ERROR_TOOL {
        return None;
    }

    let message = call
        .args
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Tool call could not be parsed.");
    let hint = call
        .args
        .get("hint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut result = ToolResult::err(message, true, hint);
    result.detail = call.args.get("errors").cloned();
    Some(result)
}

/// Walk back from the tail looking for the most recent Assistant message
/// that issued tool_calls. Return any of its call_ids that don't have a
/// matching ToolResult appearing AFTER it in history.
///
/// Used by `on_tool_intent_recover` to fabricate `skipped` results for
/// orphans so the next model turn doesn't violate provider wire format
/// (Anthropic / Gemini insist every tool_use has a paired tool_result).
fn find_orphan_tool_calls(history: &[Message]) -> Vec<String> {
    // Find the latest Assistant message with tool_calls.
    let mut assistant_idx: Option<usize> = None;
    for (i, m) in history.iter().enumerate().rev() {
        if let Message::Assistant { tool_calls, .. } = m {
            if !tool_calls.is_empty() {
                assistant_idx = Some(i);
                break;
            }
        }
    }
    let Some(idx) = assistant_idx else {
        return Vec::new();
    };
    let issued: Vec<String> = match &history[idx] {
        Message::Assistant { tool_calls, .. } => tool_calls.iter().map(|c| c.id.clone()).collect(),
        _ => unreachable!(),
    };
    let answered: std::collections::HashSet<String> = history[idx + 1..]
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    issued
        .into_iter()
        .filter(|id| !answered.contains(id))
        .collect()
}

// =============== Builder ===============

#[derive(Default)]
pub struct RunnerBuilder {
    model: Option<Arc<dyn ModelAdapter>>,
    tools: Option<Arc<dyn ToolExecutor>>,
    store: Option<Arc<dyn SessionStore>>,
    tools_provider: Option<Arc<dyn ActiveToolSetProvider>>,
    clock: Option<Arc<dyn Clock>>,
    base_system_prompt: String,
    cancel_token: Option<CancelToken>,
    compactor: Option<Arc<dyn Compactor>>,
    cache_policy: CachePolicy,
    thinking_config: ThinkingConfig,
    retry_policy: RetryPolicy,
    cache_key_strategy: CacheKeyStrategy,
    summary_recall: bool,
}

impl RunnerBuilder {
    pub fn model(mut self, m: Arc<dyn ModelAdapter>) -> Self {
        self.model = Some(m);
        self
    }
    pub fn tools(mut self, t: Arc<dyn ToolExecutor>) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn store(mut self, s: Arc<dyn SessionStore>) -> Self {
        self.store = Some(s);
        self
    }
    pub fn tools_provider<P: ActiveToolSetProvider + 'static>(mut self, p: P) -> Self {
        self.tools_provider = Some(Arc::new(p));
        self
    }
    pub fn tools_provider_arc(mut self, p: Arc<dyn ActiveToolSetProvider>) -> Self {
        self.tools_provider = Some(p);
        self
    }
    pub fn clock<C: Clock + 'static>(mut self, c: C) -> Self {
        self.clock = Some(Arc::new(c));
        self
    }
    pub fn base_system_prompt(mut self, s: impl Into<String>) -> Self {
        self.base_system_prompt = s.into();
        self
    }
    pub fn cancel_token(mut self, t: CancelToken) -> Self {
        self.cancel_token = Some(t);
        self
    }
    pub fn compactor(mut self, c: Arc<dyn Compactor>) -> Self {
        self.compactor = Some(c);
        self
    }
    pub fn cache_policy(mut self, p: CachePolicy) -> Self {
        self.cache_policy = p;
        self
    }
    pub fn thinking(mut self, t: ThinkingConfig) -> Self {
        self.thinking_config = t;
        self
    }
    pub fn retry_policy(mut self, p: RetryPolicy) -> Self {
        self.retry_policy = p;
        self
    }
    /// Pick how `prompt_cache_key` is filled on each model request. See
    /// `CacheKeyStrategy` for the cold-start vs throughput trade-off and
    /// the empirical numbers behind the default (`PrefixHash`).
    pub fn cache_key_strategy(mut self, s: CacheKeyStrategy) -> Self {
        self.cache_key_strategy = s;
        self
    }
    /// Enable an experimental non-persistent recall pass that copies a few
    /// query-relevant lines from compacted summaries next to the latest user
    /// request. It does not alter the cacheable system prefix or stored
    /// history.
    pub fn summary_recall(mut self, enabled: bool) -> Self {
        self.summary_recall = enabled;
        self
    }

    pub fn build(self) -> Result<Runner, RuntimeError> {
        Ok(Runner {
            model: self
                .model
                .ok_or(RuntimeError::InvariantViolation("model missing"))?,
            tools: self
                .tools
                .ok_or(RuntimeError::InvariantViolation("tools missing"))?,
            store: self
                .store
                .ok_or(RuntimeError::InvariantViolation("store missing"))?,
            tools_provider: self.tools_provider.unwrap_or_else(|| {
                use crate::core::provider::ActiveToolSet;
                Arc::new(|_state: &RunState| ActiveToolSet::default())
                    as Arc<dyn ActiveToolSetProvider>
            }),
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            base_system_prompt: self.base_system_prompt,
            cancel_token: Mutex::new(self.cancel_token.unwrap_or_default()),
            compactor: self.compactor,
            cache_policy: self.cache_policy,
            thinking_config: self.thinking_config,
            retry_policy: self.retry_policy,
            cache_key_strategy: self.cache_key_strategy,
            summary_recall: self.summary_recall,
        })
    }
}
