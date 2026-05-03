//! Full-screen TUI loop. Runs the model on a Tokio task while the main thread
//! drives input + render at ~60 Hz, multiplexes intermediate updates from the
//! runner task into the chat panel, and queues additional user submissions
//! while a turn is in flight.
//!
//! The main `run_tui` loop is intentionally thin — input → action → branch
//! into one of three small async helpers (`handle_cancel` / `handle_quit` /
//! `handle_submit`). Per-turn state is bundled in `TuiLoopState` so each
//! helper takes one borrow instead of half a dozen.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    KeyCode as CtKeyCode, KeyEventKind as CtKeyEventKind, KeyModifiers as CtKeyModifiers,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::cli_app::commands::{handle, CmdAction};
use crate::cli_app::doctor::{config_doctor_report, model_setup_hints};
use crate::cli_app::driver::{submit_and_drive_with_updates, TuiUpdate};
use crate::cli_app::tui_helpers::{seed_tui_history_messages, TuiAppSink};
use crate::cli_app::{store_label, truncate, ReplRuntime, TUI_MAX_QUEUED_SUBMISSIONS};
use crate::config::{Config, ConfigOverrides};
use crate::core::clock::SystemClock;
use crate::core::run_state::RunState;
use crate::core::runner::Runner;
use crate::core::step::Step;
use crate::setup;
use crate::tui::{
    ShJobView, TerminalSession, TuiApp, TuiConfig, TuiEvent, UserAction, UserSubmission,
};

pub async fn run_tui(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    state: RunState,
    clock: &SystemClock,
    initial_prompt: Option<String>,
) -> Result<(), String> {
    let mut runtime = ReplRuntime {
        wired,
        cfg,
        overrides,
    };
    let mut app = TuiApp::new(tui_config(&runtime.cfg));
    if state.history.is_empty() {
        app.add_system("Type /help for commands.");
    } else {
        seed_tui_history_messages(&mut app, &state);
    }
    seed_tui_startup_hints(&mut app, &runtime.cfg);

    let mut terminal = TerminalSession::enter().map_err(|e| format!("tui init: {e}"))?;
    refresh_tui_sh_jobs(&mut app, &runtime).await;
    app.set_queue_depth(0, TUI_MAX_QUEUED_SUBMISSIONS);

    let mut loop_state = TuiLoopState::new(state);

    if let Some(prompt) = initial_prompt.filter(|p| !p.trim().is_empty()) {
        loop_state.inflight = Some(start_tui_prompt(
            &runtime,
            &mut loop_state.state_slot,
            &mut app,
            prompt,
        )?);
    }

    loop {
        drain_tui_run_updates(loop_state.inflight.as_mut(), &mut app);
        if loop_state
            .inflight
            .as_ref()
            .map(|run| run.handle.is_finished())
            .unwrap_or(false)
        {
            let handle = loop_state.inflight.take().expect("checked Some above");
            finish_tui_run(handle, &mut loop_state.state_slot, &mut app).await?;
            app.set_status("idle");
            app.add_activity("turn finished");
            loop_state.confirm_quit_idle = false;
            refresh_tui_sh_jobs(&mut app, &runtime).await;
            if loop_state.exit_when_idle {
                break;
            }
            if start_next_queued_tui_submission(&mut loop_state, &mut runtime, clock, &mut app)
                .await?
            {
                break;
            }
        }

        refresh_tui_sh_jobs(&mut app, &runtime).await;
        terminal
            .draw(|frame| app.render(frame))
            .map_err(|e| format!("tui draw: {e}"))?;

        let Some(event) = TerminalSession::read_event(Duration::from_millis(100))
            .map_err(|e| format!("tui input: {e}"))?
        else {
            continue;
        };

        let action = match event {
            TuiEvent::Key(key) => app.handle_key(key),
            TuiEvent::Paste(text) => app.handle_paste(text),
            TuiEvent::Mouse(m) => app.handle_mouse(m),
        };

        match action {
            UserAction::None => {}
            UserAction::Cancel => handle_cancel(&mut loop_state, &runtime, &mut app),
            UserAction::Quit => {
                if handle_quit(&mut loop_state, &runtime, &mut app) {
                    break;
                }
            }
            UserAction::Submit(submission) => {
                if handle_submit(submission, &mut loop_state, &mut runtime, clock, &mut app)
                    .await?
                {
                    break;
                }
            }
        }
    }

    Ok(())
}

pub async fn run_tui_setup_error(cfg: Config, error: String) -> Result<(), String> {
    let mut app = TuiApp::new(tui_config(&cfg));
    app.set_status("setup");
    app.add_error(format!("runtime setup failed: {error}"));
    app.add_system("The TUI started in setup mode because the model runtime could not be wired.");
    app.add_system(config_doctor_report(&cfg));
    app.add_system("Fix the config, then restart muagent. Press Esc, q, or Ctrl-C to exit.");

    let mut terminal = TerminalSession::enter().map_err(|e| format!("tui init: {e}"))?;
    loop {
        terminal
            .draw(|frame| app.render(frame))
            .map_err(|e| format!("tui draw: {e}"))?;
        let Some(event) = TerminalSession::read_event(Duration::from_millis(200))
            .map_err(|e| format!("tui input: {e}"))?
        else {
            continue;
        };
        let TuiEvent::Key(key) = event else {
            continue;
        };
        let pressed = matches!(key.kind, CtKeyEventKind::Press | CtKeyEventKind::Repeat);
        if !pressed {
            continue;
        }
        let ctrl = key.modifiers.contains(CtKeyModifiers::CONTROL);
        match key.code {
            CtKeyCode::Esc | CtKeyCode::Char('q') => break,
            CtKeyCode::Char('c') | CtKeyCode::Char('d') if ctrl => break,
            _ => {}
        }
    }
    Ok(())
}

/// Per-iteration mutable state. Exists to keep `run_tui`'s signature flat
/// — the alternative is passing 4 mutable references into every helper.
struct TuiLoopState {
    state_slot: Option<RunState>,
    inflight: Option<TuiInflightRun>,
    queued: VecDeque<UserSubmission>,
    /// Set when a Ctrl-C lands during an inflight run; once the turn drains
    /// to idle, the loop exits.
    exit_when_idle: bool,
    /// Two-stage Ctrl-C confirmation when an idle quit would silently discard
    /// queued submissions. Reset on any forward progress (Submit / Cancel /
    /// turn completion) so the user is not asked to re-confirm out of context.
    confirm_quit_idle: bool,
}

impl TuiLoopState {
    fn new(state: RunState) -> Self {
        Self {
            state_slot: Some(state),
            inflight: None,
            queued: VecDeque::new(),
            exit_when_idle: false,
            confirm_quit_idle: false,
        }
    }
}

fn handle_cancel(loop_state: &mut TuiLoopState, runtime: &ReplRuntime, app: &mut TuiApp) {
    if loop_state.inflight.is_some() {
        loop_state.exit_when_idle = false;
        loop_state.confirm_quit_idle = false;
        runtime.wired.runner.cancel();
        app.set_status("cancelling");
        app.add_activity("cancel requested");
        app.add_system("cancel requested for the current task.");
    }
    // Idle cancel is a no-op — Esc with nothing to cancel should not write
    // a noisy activity entry.
}

/// Returns `true` if the loop should exit immediately.
fn handle_quit(loop_state: &mut TuiLoopState, runtime: &ReplRuntime, app: &mut TuiApp) -> bool {
    if loop_state.inflight.is_some() {
        if loop_state.exit_when_idle {
            if let Some(handle) = loop_state.inflight.take() {
                handle.handle.abort();
            }
            if !loop_state.queued.is_empty() {
                app.add_error(format!(
                    "discarded {} queued input(s) on abort.",
                    loop_state.queued.len()
                ));
            }
            return true;
        }
        loop_state.exit_when_idle = true;
        runtime.wired.runner.cancel();
        app.set_status("cancelling");
        app.add_activity("cancel requested; will exit when current task stops");
        let warn = if loop_state.queued.is_empty() {
            "cancel requested; press Ctrl-C again to abort and exit.".to_string()
        } else {
            format!(
                "cancel requested; press Ctrl-C again to abort and discard {} queued input(s).",
                loop_state.queued.len()
            )
        };
        app.add_system(warn);
        false
    } else if loop_state.queued.is_empty() {
        true
    } else if loop_state.confirm_quit_idle {
        app.add_error(format!(
            "discarded {} queued input(s) on quit.",
            loop_state.queued.len()
        ));
        true
    } else {
        loop_state.confirm_quit_idle = true;
        app.add_system(format!(
            "press Ctrl-C again to quit and discard {} queued input(s).",
            loop_state.queued.len()
        ));
        false
    }
}

/// Returns `true` if the loop should exit (slash-command quit).
async fn handle_submit(
    submission: UserSubmission,
    loop_state: &mut TuiLoopState,
    runtime: &mut ReplRuntime,
    clock: &SystemClock,
    app: &mut TuiApp,
) -> Result<bool, String> {
    loop_state.confirm_quit_idle = false;
    if loop_state.inflight.is_some() {
        enqueue_submission(submission, loop_state, app);
        return Ok(false);
    }
    match process_tui_submission(submission, &mut loop_state.state_slot, runtime, clock, app)
        .await?
    {
        TuiSubmissionOutcome::Continue => Ok(false),
        TuiSubmissionOutcome::Quit => Ok(true),
        TuiSubmissionOutcome::Started(handle) => {
            loop_state.inflight = Some(handle);
            Ok(false)
        }
    }
}

fn enqueue_submission(submission: UserSubmission, loop_state: &mut TuiLoopState, app: &mut TuiApp) {
    if loop_state.queued.len() >= TUI_MAX_QUEUED_SUBMISSIONS {
        app.add_activity("queue full");
        if app.is_input_blank() {
            // Safe to refill the input box — user has not typed anything new
            // since pressing Enter.
            app.restore_submission(submission);
            app.add_error(format!(
                "queued input limit reached ({TUI_MAX_QUEUED_SUBMISSIONS}); your input was restored to the draft."
            ));
        } else {
            // User is mid-typing something else. Don't overwrite their
            // draft — surface the dropped text so they can paste it back.
            app.add_error(format!(
                "queued input limit reached ({TUI_MAX_QUEUED_SUBMISSIONS}); dropped: {}",
                truncate(&submission.prompt.replace('\n', " "), 200)
            ));
        }
        return;
    }
    let position = loop_state.queued.len() + 1;
    loop_state.queued.push_back(submission);
    sync_tui_queue(app, &loop_state.queued);
    app.add_activity(format!(
        "queued input #{position}/{TUI_MAX_QUEUED_SUBMISSIONS}"
    ));
    app.add_system(format!(
        "queued input #{position}; it will run after the current turn."
    ));
}

struct TuiRunComplete {
    state: RunState,
    result: Result<(), String>,
}

pub struct TuiInflightRun {
    handle: JoinHandle<TuiRunComplete>,
    updates: mpsc::UnboundedReceiver<TuiUpdate>,
}

enum TuiSubmissionOutcome {
    Continue,
    Quit,
    Started(TuiInflightRun),
}

fn start_tui_prompt(
    runtime: &ReplRuntime,
    state_slot: &mut Option<RunState>,
    app: &mut TuiApp,
    prompt: String,
) -> Result<TuiInflightRun, String> {
    let state = state_slot
        .take()
        .ok_or_else(|| "internal tui state unavailable while run is active".to_string())?;
    app.add_user(prompt.clone());
    app.set_status("running");
    app.add_activity("turn started");
    Ok(spawn_tui_run(runtime.wired.runner.clone(), state, prompt))
}

fn spawn_tui_run(runner: Arc<Runner>, mut state: RunState, prompt: String) -> TuiInflightRun {
    let (tx, updates) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let result =
            submit_and_drive_with_updates(runner.as_ref(), &mut state, &prompt, Some(tx)).await;
        TuiRunComplete { state, result }
    });
    TuiInflightRun { handle, updates }
}

async fn finish_tui_run(
    mut run: TuiInflightRun,
    state_slot: &mut Option<RunState>,
    app: &mut TuiApp,
) -> Result<(), String> {
    drain_tui_run_updates(Some(&mut run), app);
    let complete = run
        .handle
        .await
        .map_err(|e| format!("tui run task failed: {e}"))?;
    match &complete.result {
        Ok(()) => append_tui_turn_result(app, &complete.state),
        Err(e) => app.add_error(e.clone()),
    }
    *state_slot = Some(complete.state);
    Ok(())
}

fn drain_tui_run_updates(run: Option<&mut TuiInflightRun>, app: &mut TuiApp) {
    let Some(run) = run else {
        return;
    };
    while let Ok(update) = run.updates.try_recv() {
        match update {
            TuiUpdate::Activity(text) => app.add_activity(text),
            TuiUpdate::Assistant(text) => app.add_assistant(text),
            TuiUpdate::Tokens(delta) => app.add_turn_tokens(delta),
            TuiUpdate::Tool {
                display,
                ok,
                brief,
                extra,
            } => app.add_tool_call(display, ok, brief, extra),
        }
    }
}

async fn process_tui_submission(
    submission: UserSubmission,
    state_slot: &mut Option<RunState>,
    runtime: &mut ReplRuntime,
    clock: &SystemClock,
    app: &mut TuiApp,
) -> Result<TuiSubmissionOutcome, String> {
    let prompt = submission.prompt;
    let trimmed = prompt.trim();
    if submission.is_slash_command {
        let state = state_slot
            .as_mut()
            .ok_or_else(|| "internal tui state unavailable while run is active".to_string())?;
        let mut sink = TuiAppSink::new(app);
        return Ok(
            match handle(trimmed, state, runtime, clock, &mut sink).await {
                CmdAction::Continue => TuiSubmissionOutcome::Continue,
                CmdAction::Quit => TuiSubmissionOutcome::Quit,
                CmdAction::Reset(new_state) => {
                    *state = *new_state;
                    TuiSubmissionOutcome::Continue
                }
            },
        );
    }

    start_tui_prompt(runtime, state_slot, app, prompt).map(TuiSubmissionOutcome::Started)
}

async fn start_next_queued_tui_submission(
    loop_state: &mut TuiLoopState,
    runtime: &mut ReplRuntime,
    clock: &SystemClock,
    app: &mut TuiApp,
) -> Result<bool, String> {
    while loop_state.inflight.is_none() {
        let Some(submission) = loop_state.queued.pop_front() else {
            sync_tui_queue(app, &loop_state.queued);
            return Ok(false);
        };
        sync_tui_queue(app, &loop_state.queued);
        match process_tui_submission(submission, &mut loop_state.state_slot, runtime, clock, app)
            .await?
        {
            TuiSubmissionOutcome::Continue => {}
            TuiSubmissionOutcome::Quit => return Ok(true),
            TuiSubmissionOutcome::Started(handle) => {
                loop_state.inflight = Some(handle);
            }
        }
    }
    Ok(false)
}

fn sync_tui_queue(app: &mut TuiApp, queued: &VecDeque<UserSubmission>) {
    app.set_queued_inputs(
        queued.iter().map(queued_submission_label).collect(),
        TUI_MAX_QUEUED_SUBMISSIONS,
    );
}

fn queued_submission_label(submission: &UserSubmission) -> String {
    truncate(&submission.prompt.replace('\n', " "), 120)
}

async fn refresh_tui_sh_jobs(app: &mut TuiApp, runtime: &ReplRuntime) {
    let Some(proc) = runtime.wired.adapters.proc.as_ref() else {
        app.set_sh_jobs(Vec::new());
        return;
    };
    match proc.list_jobs().await {
        Ok(jobs) => app.set_sh_jobs(jobs.into_iter().map(ShJobView::from_snapshot).collect()),
        Err(e) => debug!(error = %e, "sh job refresh failed"),
    }
}

fn append_tui_turn_result(app: &mut TuiApp, state: &RunState) {
    match &state.step {
        Step::Done { final_text } => {
            // Successful turns already streamed their assistant text via
            // Event::AssistantMessage (see event_tui_updates). Only synthesise
            // a placeholder when the model returned literally nothing so the
            // user is not left staring at their own prompt.
            if final_text.trim().is_empty() {
                app.add_assistant("(no final text)");
            }
        }
        Step::Failed { reason, .. } => {
            app.add_error(format!("failed: {reason}"));
        }
        Step::Paused { reason } => {
            app.add_system(format!("paused: {reason:?}"));
        }
        other => {
            app.add_system(format!("run did not finish; step={other:?}"));
        }
    }
}

fn tui_config(cfg: &Config) -> TuiConfig {
    TuiConfig {
        provider: format!("{:?}", cfg.model.provider),
        model: cfg.model.model.clone(),
        store: store_label(cfg),
        root: cfg.fs.root.display().to_string(),
    }
}

fn seed_tui_startup_hints(app: &mut TuiApp, cfg: &Config) {
    let hints = model_setup_hints(cfg);
    if hints.is_empty() {
        app.add_activity("config ready");
    } else {
        for hint in hints {
            app.add_system(format!("Setup hint: {hint}"));
        }
        app.add_activity("setup hints available");
    }
}
