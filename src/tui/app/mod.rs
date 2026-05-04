//! `TuiApp` — the interactive state machine for the TUI.
//!
//! Owns input buffer, paste queue, message log, panel selection, sh job
//! cache, and the per-turn spinner / token counter. Pure presentation
//! helpers live in `super::style`; terminal lifecycle lives in
//! `super::terminal`.
//!
//! This file contains the struct definition and "state mutator" surface
//! (everything host code calls to push state into the TUI). The other
//! files in this module hold separate `impl TuiApp` blocks:
//!
//! - [`input`] — key dispatch, paste handling, slash completion,
//!   in-process input history.
//! - [`render`] — `render` orchestration and per-panel sub-renderers.
//! - [`types`] — value types (UserAction / UserSubmission / ShJobView /
//!   private PasteBlock / InputDraft / TuiPanel).

use std::time::Instant;

use tui_textarea::TextArea;

use super::style::new_input;
use super::{ChatMessage, ChatRole};

mod input;
mod render;
mod types;

pub use types::{ShJobView, TuiConfig, UserAction, UserSubmission};

use types::{InputDraft, PasteBlock, TuiPanel};

pub struct TuiApp {
    pub(in crate::tui::app) config: TuiConfig,
    pub(in crate::tui::app) status: String,
    pub(in crate::tui::app) activity: Vec<String>,
    pub(in crate::tui::app) queue_len: usize,
    pub(in crate::tui::app) queue_limit: usize,
    pub(in crate::tui::app) queued_inputs: Vec<String>,
    pub(in crate::tui::app) input: TextArea<'static>,
    pub(in crate::tui::app) pastes: Vec<PasteBlock>,
    pub(in crate::tui::app) input_history: Vec<InputDraft>,
    pub(in crate::tui::app) history_cursor: Option<usize>,
    pub(in crate::tui::app) history_draft: Option<InputDraft>,
    pub(in crate::tui::app) panel: TuiPanel,
    pub(in crate::tui::app) sh_jobs: Vec<ShJobView>,
    pub(in crate::tui::app) selected_job: usize,
    pub(in crate::tui::app) job_detail_scroll: u16,
    pub(in crate::tui::app) messages: Vec<ChatMessage>,
    pub(in crate::tui::app) running_tools: Vec<(String, usize)>,
    pub(in crate::tui::app) scroll_back: usize,
    /// Set when the current status entered "running"; cleared on idle. The
    /// footer reads this to render an animated spinner with the elapsed
    /// turn time alongside the status word.
    pub(in crate::tui::app) turn_started: Option<Instant>,
    /// Tokens consumed by the in-flight turn (prompt + completion delta
    /// summed across steps). Reset to zero each time a new turn starts.
    pub(in crate::tui::app) turn_tokens: u32,
    /// Highlighted entry inside the slash-command popup. Auto-clamped to the
    /// current match count on every keypress / render so it stays in range
    /// as the user narrows the prefix.
    pub(in crate::tui::app) slash_popup_selected: usize,
    /// Set by Esc to hide slash suggestions without clearing the user's
    /// partially typed command. Cleared on the next edit / completion.
    pub(in crate::tui::app) slash_popup_suppressed: bool,
    /// Total context window in tokens for the active model. `None` hides
    /// the header fill bar — host code provides this when the model's
    /// window is known.
    pub(in crate::tui::app) context_window: Option<u32>,
    /// Prompt tokens sent on the most recent model step (i.e. the current
    /// context fill, not a cumulative session total). Set by host code
    /// from `state.usage` snapshots; defaults to 0 until the first turn.
    pub(in crate::tui::app) last_prompt_tokens: u32,
}

impl TuiApp {
    pub fn new(config: TuiConfig) -> Self {
        Self {
            config,
            status: "idle".into(),
            activity: Vec::new(),
            queue_len: 0,
            queue_limit: 0,
            queued_inputs: Vec::new(),
            input: new_input(),
            pastes: Vec::new(),
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: None,
            panel: TuiPanel::Chat,
            sh_jobs: Vec::new(),
            selected_job: 0,
            job_detail_scroll: 0,
            messages: Vec::new(),
            running_tools: Vec::new(),
            scroll_back: 0,
            turn_started: None,
            turn_tokens: 0,
            slash_popup_selected: 0,
            slash_popup_suppressed: false,
            context_window: None,
            last_prompt_tokens: 0,
        }
    }

    /// Tell the TUI how big the active model's context window is so the
    /// header can draw a fill bar. `None` hides the bar entirely.
    pub fn set_context_window(&mut self, max_tokens: Option<u32>) {
        self.context_window = max_tokens;
    }

    /// Update the last-known prompt-tokens count for the context fill bar.
    /// Host code typically calls this after each turn from `state.usage`.
    pub fn set_last_prompt_tokens(&mut self, tokens: u32) {
        self.last_prompt_tokens = tokens;
    }

    pub fn set_runtime(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) {
        self.config.provider = provider.into();
        self.config.model = model.into();
        self.config.effort = effort.into();
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        let next = status.into();
        let was_active = matches!(self.status.as_str(), "running" | "cancelling");
        let is_active = matches!(next.as_str(), "running" | "cancelling");
        if !was_active && is_active {
            // Start the turn timer + reset the per-turn token counter so the
            // footer spinner shows fresh elapsed/usage for this turn.
            self.turn_started = Some(Instant::now());
            self.turn_tokens = 0;
        } else if was_active && !is_active {
            self.turn_started = None;
        }
        self.status = next;
    }

    /// Accumulate per-step token deltas the runner ships through the update
    /// channel. Footer renders this beside the spinner during a running
    /// turn; cleared automatically when the turn ends.
    pub fn add_turn_tokens(&mut self, delta: u32) {
        self.turn_tokens = self.turn_tokens.saturating_add(delta);
    }

    pub fn set_queue_depth(&mut self, len: usize, limit: usize) {
        self.queue_len = len;
        self.queue_limit = limit;
    }

    pub fn set_queued_inputs(&mut self, inputs: Vec<String>, limit: usize) {
        self.queue_len = inputs.len();
        self.queue_limit = limit;
        self.queued_inputs = inputs;
    }

    pub(in crate::tui::app) fn is_submit_blocked_by_queue(&self) -> bool {
        self.queue_limit > 0
            && self.queue_len >= self.queue_limit
            && matches!(self.status.as_str(), "running" | "cancelling")
    }

    pub(in crate::tui::app) fn is_submit_locked(&self) -> bool {
        self.status == "cancelling" || self.is_submit_blocked_by_queue()
    }

    pub fn add_activity(&mut self, text: impl Into<String>) {
        const MAX_ACTIVITY_LINES: usize = 20;
        self.activity.push(text.into());
        if self.activity.len() > MAX_ACTIVITY_LINES {
            self.activity.remove(0);
        }
    }

    pub fn restore_submission(&mut self, submission: UserSubmission) {
        self.set_input_draft(InputDraft {
            input_text: submission.input_text,
            pastes: submission.pastes,
        });
        self.panel = TuiPanel::Chat;
        self.job_detail_scroll = 0;
    }

    /// `true` when the user has nothing in the input area — no typed text and
    /// no attached pastes. Callers use this to decide whether it is safe to
    /// `restore_submission` without overwriting in-progress work.
    pub fn is_input_blank(&self) -> bool {
        self.pastes.is_empty() && self.input.lines().iter().all(|line| line.is_empty())
    }

    pub fn add_user(&mut self, text: impl Into<String>) {
        self.add(ChatRole::User, text);
    }

    pub fn add_assistant(&mut self, text: impl Into<String>) {
        self.add(ChatRole::Assistant, text);
    }

    pub fn add_system(&mut self, text: impl Into<String>) {
        self.add(ChatRole::System, text);
    }

    pub fn add_warning(&mut self, text: impl Into<String>) {
        self.scroll_back = 0;
        self.panel = TuiPanel::Chat;
        self.job_detail_scroll = 0;
        self.add(ChatRole::Warning, text);
    }

    pub fn add_error(&mut self, text: impl Into<String>) {
        self.scroll_back = 0;
        self.panel = TuiPanel::Chat;
        self.job_detail_scroll = 0;
        self.add(ChatRole::Error, text);
    }

    /// Append a tool invocation as a one- or two-line chat row, styled
    /// distinct from assistant / system messages. `display` is the human
    /// label (e.g. `"Bash(sleep 10)"`); `ok` selects the trailing ✓ / ✗
    /// glyph; `brief` is appended for failed calls; `extra` (optional) is
    /// rendered as a `⎿ ...` continuation line — used for diff stats like
    /// `+3 -1` on file-edit tools.
    pub fn add_tool_call(
        &mut self,
        display: impl Into<String>,
        ok: bool,
        brief: impl Into<String>,
        extra: Option<String>,
    ) {
        let display = display.into();
        let brief = brief.into();
        let text = completed_tool_call_text(&display, ok, &brief, extra.as_deref());
        self.add(ChatRole::Tool, text);
    }

    pub fn add_tool_call_started(
        &mut self,
        call_id: impl Into<String>,
        display: impl Into<String>,
    ) {
        let call_id = call_id.into();
        if call_id.is_empty() {
            return;
        }
        if self
            .running_tools
            .iter()
            .any(|(existing, _)| existing == &call_id)
        {
            return;
        }
        let idx = self.add(ChatRole::Tool, format!("⏺ {}", display.into()));
        self.running_tools.push((call_id, idx));
    }

    pub fn finish_tool_call(
        &mut self,
        call_id: impl AsRef<str>,
        display: impl Into<String>,
        ok: bool,
        brief: impl Into<String>,
        extra: Option<String>,
    ) {
        let display = display.into();
        let brief = brief.into();
        let text = completed_tool_call_text(&display, ok, &brief, extra.as_deref());
        let call_id = call_id.as_ref();
        if let Some(pos) = self
            .running_tools
            .iter()
            .position(|(existing, _)| existing == call_id)
        {
            let (_, idx) = self.running_tools.remove(pos);
            if let Some(message) = self.messages.get_mut(idx) {
                message.text = text;
                return;
            }
        }
        self.add(ChatRole::Tool, text);
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.running_tools.clear();
        self.scroll_back = 0;
        self.panel = TuiPanel::Chat;
        self.job_detail_scroll = 0;
    }

    pub fn set_sh_jobs(&mut self, jobs: Vec<ShJobView>) {
        self.sh_jobs = jobs;
        if self.sh_jobs.is_empty() {
            self.selected_job = 0;
            if matches!(self.panel, TuiPanel::JobDetail) {
                self.panel = TuiPanel::Jobs;
            }
        } else if self.selected_job >= self.sh_jobs.len() {
            self.selected_job = self.sh_jobs.len() - 1;
        }
    }

    pub(in crate::tui::app) fn add(&mut self, role: ChatRole, text: impl Into<String>) -> usize {
        let text = text.into();
        // If the user has scrolled back to read older messages, keep them
        // anchored to the same content instead of yanking them to the new
        // bottom. We approximate the added height as the message's logical
        // line count plus a trailing blank line; wrapping at narrow widths
        // can drift this slightly but that beats unconditionally jumping.
        if self.scroll_back > 0 {
            let added_lines = text.lines().count().max(1) + 1;
            self.scroll_back = self.scroll_back.saturating_add(added_lines);
        }
        self.messages.push(ChatMessage { role, text });
        self.messages.len() - 1
    }
}

fn completed_tool_call_text(display: &str, ok: bool, brief: &str, extra: Option<&str>) -> String {
    let mut text = if ok {
        format!("⏺ {display} ✓")
    } else {
        let trimmed = brief.trim();
        if trimmed.is_empty() {
            format!("⏺ {display} ✗")
        } else {
            format!("⏺ {display} ✗  {trimmed}")
        }
    };
    if let Some(extra) = extra.map(str::trim).filter(|s| !s.is_empty()) {
        text.push('\n');
        text.push_str("  ⎿ ");
        text.push_str(extra);
    }
    text
}

#[cfg(test)]
mod tests;
