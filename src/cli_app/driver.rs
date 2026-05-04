//! Drive a `Runner` to completion, fan events out to log/TUI sinks, and
//! enforce the per-run safety budgets (max steps, no-progress fuse).

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::cli_app::event_render::{event_tui_updates, tool_display_label, tool_result_extra_line};
use crate::cli_app::image::user_content;
use crate::cli_app::truncate;
use crate::cli_app::DEFAULT_MAX_STEPS;
use crate::core::event::Event;
use crate::core::run_state::RunState;
use crate::core::runner::Runner;
use crate::core::step::{PauseReason, Step};
use crate::core::types::{Content, Message};

/// Typed channel update from the inflight runner task. Lets the runner emit
/// chat-bound assistant text alongside the existing one-line activity briefs
/// so the chat panel reflects intermediate steps instead of going dark until
/// the turn finishes.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub enum TuiUpdate {
    Activity(String),
    Assistant(String),
    /// Prompt tokens reported for the latest model request. This is the
    /// current context size for that request, unlike `RunState.usage`, which
    /// accumulates prompt tokens across the whole session.
    PromptTokens(u32),
    /// Per-step token delta (prompt + completion). The TUI accumulates these
    /// into the in-flight turn counter shown next to the spinner.
    Tokens(u32),
    /// One completed tool call rendered as a chat row. Built by pairing the
    /// preceding `Event::ToolCallStart` (carries args) with `ToolCallEnd`
    /// (carries ok / brief / detail), since the runner emits them together.
    /// `extra` is an optional second-line continuation (e.g. `+3 -1`).
    Tool {
        display: String,
        ok: bool,
        brief: String,
        extra: Option<String>,
    },
}

pub async fn run_one_shot(
    runner: &Runner,
    state: &mut RunState,
    prompt: &str,
    images: &[String],
) -> Result<(), String> {
    submit_and_drive_with_content(runner, state, user_content(prompt, images)?).await?;
    match &state.step {
        Step::Done { final_text } => {
            println!("{final_text}");
            Ok(())
        }
        Step::Failed { reason, .. } => Err(format!("run failed: {reason}")),
        Step::Paused { reason } => Err(format!("run paused: {reason:?}")),
        other => Err(format!("run did not finish; final step={other:?}")),
    }
}

pub async fn submit_and_drive(
    runner: &Runner,
    state: &mut RunState,
    prompt: &str,
) -> Result<(), String> {
    submit_and_drive_with_updates(runner, state, prompt, None).await
}

pub async fn submit_and_drive_with_updates(
    runner: &Runner,
    state: &mut RunState,
    prompt: &str,
    updates: Option<mpsc::UnboundedSender<TuiUpdate>>,
) -> Result<(), String> {
    submit_and_drive_with_content_and_updates(runner, state, Content::text(prompt), updates).await
}

pub async fn submit_and_drive_with_content(
    runner: &Runner,
    state: &mut RunState,
    content: Content,
) -> Result<(), String> {
    submit_and_drive_with_content_and_updates(runner, state, content, None).await
}

pub async fn submit_and_drive_with_content_and_updates(
    runner: &Runner,
    state: &mut RunState,
    content: Content,
    updates: Option<mpsc::UnboundedSender<TuiUpdate>>,
) -> Result<(), String> {
    runner
        .submit_user_message(state, Message::User { content })
        .await
        .map_err(|e| format!("submit_user_message failed: {e:?}"))?;
    send_tui_activity(&updates, "submitted");
    drive_until_terminal_with_updates(runner, state, updates).await
}

pub async fn drive_until_terminal_with_updates(
    runner: &Runner,
    state: &mut RunState,
    updates: Option<mpsc::UnboundedSender<TuiUpdate>>,
) -> Result<(), String> {
    let budget = StepBudget::from_env();
    let mut bad_tool_events = 0usize;
    for _ in 0..budget.max_steps {
        if is_terminal(&state.step) {
            return Ok(());
        }
        send_tui_activity(&updates, step_activity_label(&state.step));
        let prompt_before = state.usage.tokens_prompt;
        let completion_before = state.usage.tokens_completion;
        let out = runner.step(state).await.map_err(|e| {
            error!(?e, "runner step failed");
            format!("runner step failed: {e:?}")
        })?;
        emit_step_token_delta(&updates, state, prompt_before, completion_before);
        process_step_events(&updates, &out.events, &mut bad_tool_events, &budget)?;
    }
    if is_terminal(&state.step) {
        return Ok(());
    }
    warn!(
        max = budget.max_steps,
        "hit step budget without reaching terminal state"
    );
    Err(format!(
        "hit step budget without reaching terminal state; step={:?}",
        state.step
    ))
}

fn step_activity_label(step: &Step) -> &'static str {
    match step {
        Step::Ready => "preparing",
        Step::ModelTurn => "thinking",
        Step::ToolBatch { .. } => "using tools",
        Step::ToolIntent { .. } => "checking tool result",
        Step::Paused {
            reason: PauseReason::HostRequested,
        } => "stopped",
        Step::Paused { .. } => "paused",
        Step::Done { .. } => "done",
        Step::Failed { .. } => "failed",
    }
}

struct StepBudget {
    max_steps: usize,
    /// 0 disables the no-progress fuse.
    bad_tool_event_limit: usize,
}

impl StepBudget {
    fn from_env() -> Self {
        Self {
            max_steps: std::env::var("MUAGENT_MAX_STEPS")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_MAX_STEPS),
            bad_tool_event_limit: std::env::var("MUAGENT_BAD_TOOL_EVENT_LIMIT")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(0),
        }
    }
}

fn is_terminal(step: &Step) -> bool {
    matches!(
        step,
        Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
    )
}

fn emit_step_token_delta(
    updates: &Option<mpsc::UnboundedSender<TuiUpdate>>,
    state: &RunState,
    prompt_before: u32,
    completion_before: u32,
) {
    let prompt_delta = state.usage.tokens_prompt.saturating_sub(prompt_before);
    let completion_delta = state
        .usage
        .tokens_completion
        .saturating_sub(completion_before);
    if prompt_delta > 0 {
        send_tui_update(updates, TuiUpdate::PromptTokens(prompt_delta));
    }
    let token_delta = prompt_delta.saturating_add(completion_delta);
    if token_delta > 0 {
        send_tui_update(updates, TuiUpdate::Tokens(token_delta));
    }
}

/// Walk one step's events: log them, render Start/End pairs into
/// `TuiUpdate::Tool` rows, fan everything else through `event_tui_updates`,
/// and update the no-progress fuse counter. Returns `Err` only when the
/// fuse trips.
fn process_step_events(
    updates: &Option<mpsc::UnboundedSender<TuiUpdate>>,
    events: &[Event],
    bad_tool_events: &mut usize,
    budget: &StepBudget,
) -> Result<(), String> {
    // ToolCallStart carries args; ToolCallEnd carries ok/brief. The runner
    // emits them adjacently within one step's event batch, so we keep the
    // most recent unmatched Start in this small slot and consume it when
    // the matching End arrives.
    let mut pending_tool: Option<(String, serde_json::Value)> = None;
    for ev in events {
        log_event(ev);
        match ev {
            Event::ToolCallStart { tool, args, .. } => {
                let display = tool_display_label(tool, args);
                pending_tool = Some((tool.clone(), args.clone()));
                send_tui_activity(updates, format!("Running {display}"));
            }
            Event::ToolCallEnd {
                ok, brief, detail, ..
            } => {
                emit_tool_call_end(updates, pending_tool.take(), *ok, brief, detail);
            }
            _ => {
                for update in event_tui_updates(ev) {
                    send_tui_update(updates, update);
                }
            }
        }
        if is_bad_tool_event(ev) {
            *bad_tool_events += 1;
        } else if matches!(ev, Event::ToolCallEnd { .. }) {
            *bad_tool_events = 0;
        }
        if budget.bad_tool_event_limit > 0 && *bad_tool_events >= budget.bad_tool_event_limit {
            return Err(format!(
                "tool no-progress guard tripped after {bad_tool_events} \
                 timeout/security/error tool events"
            ));
        }
    }
    Ok(())
}

fn emit_tool_call_end(
    updates: &Option<mpsc::UnboundedSender<TuiUpdate>>,
    pending: Option<(String, serde_json::Value)>,
    ok: bool,
    brief: &str,
    detail: &serde_json::Value,
) {
    let (tool, args) = pending.unwrap_or_default();
    let display = if tool.is_empty() {
        "(unknown tool)".to_string()
    } else {
        tool_display_label(&tool, &args)
    };
    let extra = tool_result_extra_line(&tool, detail);
    send_tui_update(
        updates,
        TuiUpdate::Tool {
            display: display.clone(),
            ok,
            brief: brief.to_string(),
            extra,
        },
    );
    let outcome = if ok { "finished" } else { "failed" };
    send_tui_activity(
        updates,
        format!("Tool {outcome}: {}", truncate(&display, 80)),
    );
}

pub fn send_tui_update(updates: &Option<mpsc::UnboundedSender<TuiUpdate>>, update: TuiUpdate) {
    if let Some(tx) = updates {
        let _ = tx.send(update);
    }
}

pub fn send_tui_activity(
    updates: &Option<mpsc::UnboundedSender<TuiUpdate>>,
    text: impl Into<String>,
) {
    send_tui_update(updates, TuiUpdate::Activity(text.into()));
}

pub fn is_bad_tool_event(ev: &Event) -> bool {
    let Event::ToolCallEnd {
        ok,
        retryable,
        brief,
        ..
    } = ev
    else {
        return false;
    };
    let brief_lc = brief.to_ascii_lowercase();
    (!*ok && *retryable)
        || brief_lc.contains("timeout")
        || brief_lc.contains("timed out")
        || brief.contains("Security violation")
}

pub fn log_event(ev: &Event) {
    match ev {
        Event::ToolCallStart { tool, call_id, .. } => {
            debug!(tool = %tool, call_id = %call_id, "tool call start");
        }
        Event::ToolCallEnd {
            ok, brief, call_id, ..
        } => {
            if *ok {
                debug!(call_id = %call_id, brief = %truncate(brief, 120), "tool call ok");
            } else {
                warn!(call_id = %call_id, brief = %truncate(brief, 120), "tool call err");
            }
        }
        Event::HistoryCompacted {
            replaced_turns,
            saved_tokens_estimate,
            ..
        } => {
            info!(
                turns = *replaced_turns,
                saved_tokens = *saved_tokens_estimate,
                "history compacted",
            );
        }
        Event::ErrorRaised { class, brief, .. } => {
            error!(class = %class, brief = %brief, "runtime error");
        }
        Event::Paused { reason, .. } => {
            warn!(reason = %reason, "run paused");
        }
        other => debug!(?other, "event"),
    }
}

pub fn print_turn_result(state: &RunState) {
    match &state.step {
        Step::Done { final_text } => {
            println!("\n{final_text}\n");
        }
        Step::Failed { reason, .. } => {
            eprintln!("(failed: {reason})");
        }
        Step::Paused { reason } => {
            eprintln!("(paused: {reason:?})");
        }
        _ => {}
    }
}
