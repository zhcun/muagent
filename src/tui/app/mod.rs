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
    pub(in crate::tui::app) scroll_back: u16,
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
    /// Total context window in tokens for the active model. `None` hides
    /// the header's `ctx [████░░░░] 32% / 128k` bar — host code provides
    /// this when the model's window is known.
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
            scroll_back: 0,
            turn_started: None,
            turn_tokens: 0,
            slash_popup_selected: 0,
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

    pub fn set_runtime(&mut self, provider: impl Into<String>, model: impl Into<String>) {
        self.config.provider = provider.into();
        self.config.model = model.into();
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        let next = status.into();
        let was_running = self.status == "running";
        let is_running = next == "running";
        if !was_running && is_running {
            // Start the turn timer + reset the per-turn token counter so the
            // footer spinner shows fresh elapsed/usage for this turn.
            self.turn_started = Some(Instant::now());
            self.turn_tokens = 0;
        } else if was_running && !is_running {
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

    pub fn add_error(&mut self, text: impl Into<String>) {
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
        if let Some(extra) = extra.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            text.push('\n');
            text.push_str("  ⎿ ");
            text.push_str(extra);
        }
        self.add(ChatRole::Tool, text);
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.scroll_back = 0;
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

    pub(in crate::tui::app) fn add(&mut self, role: ChatRole, text: impl Into<String>) {
        let text = text.into();
        // If the user has scrolled back to read older messages, keep them
        // anchored to the same content instead of yanking them to the new
        // bottom. We approximate the added height as the message's logical
        // line count plus a trailing blank line; wrapping at narrow widths
        // can drift this slightly but that beats unconditionally jumping.
        if self.scroll_back > 0 {
            let added_lines = text.lines().count().max(1) + 1;
            self.scroll_back = self.scroll_back.saturating_add(added_lines as u16);
        }
        self.messages.push(ChatMessage { role, text });
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::adapters::ExecJobState;

    use super::super::style::{wrap_by_display_width, SPINNER_FRAMES};
    use super::types::{PasteBlock, TuiPanel};
    use super::*;

    fn app() -> TuiApp {
        TuiApp::new(TuiConfig {
            provider: "openrouter".into(),
            model: "openai/gpt-test".into(),
            store: "memory".into(),
            root: ".".into(),
        })
    }

    fn submission(
        prompt: &str,
        input_text: &str,
        pastes: &[&str],
        is_slash_command: bool,
    ) -> UserSubmission {
        UserSubmission {
            prompt: prompt.into(),
            is_slash_command,
            input_text: input_text.into(),
            pastes: pastes
                .iter()
                .map(|text| PasteBlock::new((*text).into()))
                .collect(),
        }
    }

    #[test]
    fn enter_submits_trimmed_input() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("hi", "hi", &[], false))
        );
    }

    #[test]
    fn escape_cancels() {
        assert_eq!(
            app().handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::Cancel
        );
    }

    #[test]
    fn ctrl_c_quits() {
        assert_eq!(
            app().handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            UserAction::Quit
        );
    }

    #[test]
    fn ctrl_d_also_quits() {
        assert_eq!(
            app().handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            UserAction::Quit
        );
    }

    #[test]
    fn textarea_handles_cursor_editing() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("h!i", "h!i", &[], false))
        );
    }

    #[test]
    fn multiline_paste_submits_full_content() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission(
                "s\n\none\ntwo\nthree",
                "s",
                &["one\ntwo\nthree"],
                false
            ))
        );
    }

    #[test]
    fn pasted_slash_text_is_not_treated_as_command() {
        let mut app = app();
        assert_eq!(
            app.handle_paste("/help\nnot a command".into()),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission(
                "/help\nnot a command",
                "",
                &["/help\nnot a command"],
                false
            ))
        );
    }

    #[test]
    fn pasted_input_history_recalls_summary_but_resubmits_full_content() {
        let mut app = app();
        assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(_)
        ));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        let screen = render_text(&app, 80, 20);
        assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission(
                "one\ntwo\nthree",
                "",
                &["one\ntwo\nthree"],
                false
            ))
        );
    }

    #[test]
    fn restored_submission_keeps_paste_as_input_summary() {
        let mut app = app();
        app.restore_submission(submission(
            "prefix\n\none\ntwo\nthree",
            "prefix",
            &["one\ntwo\nthree"],
            false,
        ));

        let screen = render_text(&app, 80, 20);
        assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission(
                "prefix\n\none\ntwo\nthree",
                "prefix",
                &["one\ntwo\nthree"],
                false
            ))
        );
    }

    #[test]
    fn clear_messages_removes_old_transcript() {
        let mut app = app();
        app.add_user("old");
        app.add_assistant("reply");
        app.clear_messages();

        assert!(app.messages.is_empty());
        let screen = render_text(&app, 80, 20);
        assert!(screen.contains("Type a message or /help."), "{screen}");
    }

    #[test]
    fn up_down_browses_input_history_and_restores_draft() {
        let mut app = app();
        type_text(&mut app, "first");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(_)
        ));

        type_text(&mut app, "draft");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("draft", "draft", &[], false))
        );
    }

    #[test]
    fn up_recalls_previous_input() {
        let mut app = app();
        type_text(&mut app, "first");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(_)
        ));
        type_text(&mut app, "second");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(_)
        ));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("first", "first", &[], false))
        );
    }

    #[test]
    fn up_in_multiline_input_keeps_editor_behavior() {
        let mut app = app();
        type_text(&mut app, "a");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            UserAction::None
        );
        type_text(&mut app, "b");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("a\nb", "a\nb", &[], false))
        );
    }

    #[test]
    fn tab_completes_slash_command() {
        let mut app = app();
        type_text(&mut app, "/he");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("/help", "/help", &[], true))
        );
    }

    #[test]
    fn ctrl_b_opens_job_list_and_enter_opens_detail() {
        let mut app = app();
        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Jobs);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::JobDetail);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Jobs);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Chat);
    }

    #[test]
    fn q_goes_back_one_level_in_both_job_panels() {
        let mut app = app();
        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Jobs);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::JobDetail);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Jobs);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.panel, TuiPanel::Chat);
    }

    #[test]
    fn job_selection_clamps_after_refresh() {
        let mut app = app();
        app.set_sh_jobs(vec![
            job("sh_1", ExecJobState::Running, None, "sleep 10"),
            job("sh_2", ExecJobState::Exited, Some(0), "true"),
        ]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.selected_job, 1);

        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
        assert_eq!(app.selected_job, 0);
    }

    #[test]
    fn escape_cancels_chat_view() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::Cancel
        );
    }

    #[test]
    fn paste_is_buffered_while_jobs_panel_is_open() {
        let mut app = app();
        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );

        assert_eq!(app.handle_paste("hidden\npaste".into()), UserAction::None);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("hidden\npaste", "", &["hidden\npaste"], false))
        );
    }

    #[test]
    fn ctrl_b_panel_roundtrip_preserves_chat_draft() {
        let mut app = app();
        type_text(&mut app, "draft");
        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("draft", "draft", &[], false))
        );
    }

    #[test]
    fn render_snapshot_shows_footer_and_paste_summary() {
        let mut app = app();
        app.set_status("idle");
        app.set_queued_inputs(vec!["next prompt".into()], 3);
        app.add_system("ready");
        app.set_sh_jobs(vec![
            job("sh_1", ExecJobState::Running, None, "sleep 10"),
            job("sh_2", ExecJobState::Exited, Some(0), "true"),
        ]);
        assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);

        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("μAgent"), "{screen}");
        assert!(screen.contains("Messages"), "{screen}");
        assert!(screen.contains("Activity"), "{screen}");
        assert!(screen.contains("next 1:"), "{screen}");
        assert!(screen.contains("next prompt"), "{screen}");
        assert!(screen.contains("Input"), "{screen}");
        assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
        assert!(screen.contains("sh bg: 1/2"), "{screen}");
        assert!(screen.contains("Ctrl-B jobs"), "{screen}");
        assert!(screen.contains("Tab cmd"), "{screen}");
        assert!(screen.contains("Ctrl-C quit"), "{screen}");
    }

    #[test]
    fn scrolled_back_view_is_preserved_when_new_messages_arrive() {
        let mut app = app();
        for i in 0..30 {
            app.add_assistant(format!("line {i}"));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            UserAction::None
        );
        let before = render_text(&app, 80, 12);

        app.add_assistant("STREAMED");
        let after = render_text(&app, 80, 12);
        // The exact title differs because the new "↓ N rows" indicator
        // reflects how much content is below the viewport — adding a
        // message increases that count. What matters is the anchored
        // viewport: whichever line was visible before is still visible
        // after, and the freshly-streamed message is NOT pulled into view.
        let anchor_line = before
            .lines()
            .find(|l| l.contains("line "))
            .expect("anchor line in before");
        assert!(after.contains(anchor_line), "{after}");
        assert!(!after.contains("STREAMED"), "{after}");
    }

    #[test]
    fn end_key_jumps_back_to_bottom() {
        let mut app = app();
        for i in 0..30 {
            app.add_assistant(format!("line {i}"));
        }
        // Page up to introduce scroll-back, then End resets it.
        let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(app.scroll_back > 0);
        let _ = app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.scroll_back, 0);
    }

    #[test]
    fn lowercase_g_jumps_to_bottom_only_when_input_is_empty() {
        let mut app = app();
        app.add_assistant("hi");
        let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(app.scroll_back > 0);
        // Empty input + plain g resets scroll.
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.scroll_back, 0);

        // Non-empty input: g must type the letter, not jump.
        type_text(&mut app, "hello");
        let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let before = app.scroll_back;
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.scroll_back, before, "g must not jump when input is non-empty");
    }

    #[test]
    fn queue_panel_shows_plus_more_when_truncated() {
        let mut app = app();
        app.set_queued_inputs(
            (1..=5).map(|i| format!("prompt {i}")).collect(),
            10,
        );
        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("next 1:"), "{screen}");
        assert!(screen.contains("next 2:"), "{screen}");
        // With 5 queued and a 3-row budget, expect a "+N more" tail.
        assert!(screen.contains("more queued"), "{screen}");
    }

    #[test]
    fn pinned_view_follows_new_messages() {
        let mut app = app();
        for i in 0..30 {
            app.add_assistant(format!("line {i}"));
        }
        app.add_assistant("STREAMED");
        let screen = render_text(&app, 80, 12);
        assert!(screen.contains("STREAMED"), "{screen}");
    }

    #[test]
    fn is_input_blank_tracks_typed_text_and_pastes() {
        let mut app = app();
        assert!(app.is_input_blank());

        type_text(&mut app, "hi");
        assert!(!app.is_input_blank());

        let _ = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.is_input_blank());

        let _ = app.handle_paste("a\nb\nc".into());
        assert!(!app.is_input_blank());
    }

    #[test]
    fn running_status_renders_spinner_with_meta() {
        let mut app = app();
        app.set_status("running");
        app.add_turn_tokens(1234);
        let screen = render_text(&app, 120, 22);
        let has_spinner = SPINNER_FRAMES.iter().any(|f| screen.contains(f));
        assert!(has_spinner, "{screen}");
        assert!(screen.contains("running"), "{screen}");
        assert!(screen.contains("1.2k tok"), "{screen}");
    }

    #[test]
    fn idle_status_renders_dot_not_spinner() {
        let mut app = app();
        app.set_status("idle");
        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("● idle"), "{screen}");
        assert!(
            !SPINNER_FRAMES.iter().any(|f| screen.contains(f)),
            "{screen}"
        );
    }

    #[test]
    fn turn_started_clears_when_status_returns_to_idle() {
        let mut app = app();
        app.set_status("running");
        app.add_turn_tokens(500);
        app.set_status("idle");
        app.set_status("running");
        let screen = render_text(&app, 120, 22);
        assert!(!screen.contains("500 tok"), "{screen}");
    }

    #[test]
    fn tool_call_renders_as_compact_chat_row() {
        let mut app = app();
        app.add_tool_call("Bash(sleep 10)", true, "", None);
        app.add_tool_call("Update(src/foo.rs)", false, "permission denied", None);
        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("⏺ Bash(sleep 10) ✓"), "{screen}");
        assert!(screen.contains("⏺ Update(src/foo.rs) ✗"), "{screen}");
        assert!(screen.contains("permission denied"), "{screen}");
    }

    #[test]
    fn mouse_wheel_scrolls_messages_panel() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let mut app = app();
        for i in 0..30 {
            app.add_assistant(format!("line {i}"));
        }

        // Wheel up scrolls back; wheel down scrolls forward. Three rows
        // per tick matches the handler step.
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_back, 3);
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_back, 6);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_back, 3);
    }

    #[test]
    fn context_bar_renders_when_window_is_set() {
        let mut app = app();
        app.set_context_window(Some(128_000));
        app.set_last_prompt_tokens(40_960); // 32% fill
        let screen = render_text(&app, 140, 22);
        assert!(screen.contains("ctx ["), "{screen}");
        assert!(screen.contains("32%"), "{screen}");
        assert!(screen.contains("/ 128k tok"), "{screen}");
    }

    #[test]
    fn context_bar_hidden_when_window_is_none() {
        let app = app();
        let screen = render_text(&app, 140, 22);
        assert!(!screen.contains("ctx ["), "{screen}");
    }

    #[test]
    fn slash_popup_shows_matches_and_enter_fills_input() {
        let mut app = app();
        type_text(&mut app, "/he");
        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("Commands"), "{screen}");
        assert!(screen.contains("/help"), "{screen}");
        assert!(screen.contains("show this help"), "{screen}");

        // Enter accepts the highlighted match (first one = /help) and
        // fills the input with the canonical name plus a trailing space.
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        // Submitting now sends the chosen command.
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("/help", "/help", &[], true))
        );
    }

    #[test]
    fn slash_popup_arrow_keys_navigate_matches() {
        let mut app = app();
        type_text(&mut app, "/h");
        // Two matches: /help and /history. Down once, then Enter picks /history.
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(submission("/history", "/history", &[], true))
        );
    }

    #[test]
    fn fenced_code_block_renders_with_dim_rule() {
        let mut app = app();
        app.add_assistant("Here is some Rust:\n```rust\nfn foo() {}\n```\nDone.");
        let screen = render_text(&app, 120, 22);
        // Code body line gets the │ continuation rule.
        assert!(screen.contains("│ fn foo() {}"), "{screen}");
        // Fence opener gets a dim horizontal rule with the language label.
        assert!(screen.contains("─ rust"), "{screen}");
        // Surrounding prose lines stay intact.
        assert!(screen.contains("Here is some Rust:"), "{screen}");
        assert!(screen.contains("Done."), "{screen}");
    }

    #[test]
    fn tool_calls_after_assistant_render_without_role_bar() {
        let mut app = app();
        app.add_assistant("Looking at the code");
        app.add_tool_call("Read(src/foo.rs)", true, "", None);
        app.add_tool_call("Update(src/foo.rs)", true, "", Some("+3 -1".into()));
        app.add_assistant("Done.");
        let screen = render_text(&app, 120, 22);

        // Each tool row drops its own ▎ bar — only the surrounding
        // assistant rows keep theirs. Counting ▎ in the rendered screen
        // catches accidental regressions to per-tool stripes.
        let bar_count = screen.matches('▎').count();
        // Exactly two assistant rows = two ▎ markers.
        assert_eq!(bar_count, 2, "{screen}");
        assert!(screen.contains("⏺ Read(src/foo.rs) ✓"), "{screen}");
        assert!(screen.contains("⏺ Update(src/foo.rs) ✓"), "{screen}");
        assert!(screen.contains("Done."), "{screen}");
    }

    #[test]
    fn tool_call_diff_stats_render_as_continuation_line() {
        let mut app = app();
        app.add_tool_call("Update(src/tui.rs)", true, "", Some("+3 -1".into()));
        let screen = render_text(&app, 120, 22);
        assert!(screen.contains("⏺ Update(src/tui.rs) ✓"), "{screen}");
        assert!(screen.contains("⎿ +3 -1"), "{screen}");
    }

    #[test]
    fn footer_hides_sh_bg_badge_when_no_jobs() {
        let app = app();
        let screen = render_text(&app, 100, 22);
        assert!(!screen.contains("sh bg"), "{screen}");
    }

    #[test]
    fn activity_panel_shows_recent_history_lines() {
        let mut app = app();
        for i in 0..5 {
            app.add_activity(format!("step {i}"));
        }
        let screen = render_text(&app, 100, 22);
        assert!(screen.contains("step 4"), "{screen}");
        assert!(screen.contains("step 3"), "{screen}");
        assert!(!screen.contains("status: idle"), "{screen}");
    }

    #[test]
    fn long_wrapped_messages_scroll_to_bottom() {
        let mut app = app();
        app.add_assistant(format!("{}END", "x".repeat(800)));

        let screen = render_text(&app, 40, 24);
        assert!(screen.contains("END"), "{screen}");
    }

    #[test]
    fn wide_message_wrapping_uses_terminal_display_width() {
        assert_eq!(
            wrap_by_display_width("你好abc", 4),
            vec!["你好".to_string(), "abc".to_string()]
        );
    }

    #[test]
    fn render_snapshot_shows_job_list_and_detail() {
        let mut app = app();
        app.set_sh_jobs(vec![
            job("sh_1", ExecJobState::Running, None, "sleep 10"),
            job("sh_2", ExecJobState::Exited, Some(0), "true"),
        ]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );

        let list = render_text(&app, 100, 22);
        assert!(list.contains("Background sh jobs"), "{list}");
        assert!(list.contains("running"), "{list}");
        assert!(list.contains("sleep 10"), "{list}");
        assert!(list.contains("exit 0"), "{list}");

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        let detail = render_text(&app, 100, 22);
        assert!(detail.contains("sh job detail"), "{detail}");
        assert!(detail.contains("job_id: sh_1"), "{detail}");
        assert!(detail.contains("command: sleep 10"), "{detail}");
        assert!(detail.contains("stdout: 12B"), "{detail}");
    }

    #[test]
    fn render_small_terminal_does_not_panic() {
        let mut app = app();
        app.add_system("ready");
        app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UserAction::None
        );

        let _ = render_text(&app, 32, 10);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::None
        );
        let _ = render_text(&app, 32, 10);
    }

    fn type_text(app: &mut TuiApp, text: &str) {
        for ch in text.chars() {
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                UserAction::None
            );
        }
    }

    fn job(id: &str, state: ExecJobState, code: Option<i32>, command: &str) -> ShJobView {
        ShJobView {
            job_id: id.into(),
            state,
            code,
            command: command.into(),
            elapsed_ms: 1500,
            stdout_bytes: 12,
            stderr_bytes: 0,
            output_truncated: false,
            error: None,
            stdout_tail: "ok".into(),
            stderr_tail: String::new(),
        }
    }

    fn render_text(app: &TuiApp, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let width = buffer.area.width as usize;
        let mut out = String::new();
        for row in buffer.content.chunks(width) {
            let mut line = String::new();
            for cell in row {
                line.push_str(cell.symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}
