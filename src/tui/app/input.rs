//! Input handling: key dispatch, paste buffering, take_submission, panel-key
//! routing, in-process input history, and slash-command tab completion.
//! Lives in its own `impl TuiApp` block alongside `mod.rs` and `render.rs`.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

use super::super::style::{common_prefix, new_input, paste_line_count, trim_newlines};
use super::types::{InputDraft, PasteBlock, TuiPanel, UserAction, UserSubmission};
use super::TuiApp;

const MAX_INPUT_HISTORY: usize = 100;
const MAX_HISTORY_ENTRY_CHARS: usize = 8000;

impl TuiApp {
    pub fn handle_paste(&mut self, text: String) -> UserAction {
        if text.is_empty() {
            return UserAction::None;
        }

        if self.panel != TuiPanel::Chat {
            // Buffer the paste as a paste block so it appears in the input
            // area the moment the user returns to Chat, instead of being
            // silently dropped. An activity hint keeps the action visible.
            self.pastes.push(PasteBlock::new(text));
            self.add_activity("paste buffered (jobs panel was open)");
            return UserAction::None;
        }

        let line_count = paste_line_count(&text);
        let char_count = text.chars().count();
        self.slash_popup_suppressed = false;
        if line_count <= 1 && char_count <= 400 {
            self.leave_history_navigation();
            self.input.insert_str(text);
        } else {
            self.leave_history_navigation();
            self.pastes.push(PasteBlock::new(text));
        }
        UserAction::None
    }

    /// Wheel up/down scrolls the chat history if the terminal reports mouse
    /// events. Default terminal setup leaves mouse capture off so native text
    /// selection remains available.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> UserAction {
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_back = self.scroll_back.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_back = self.scroll_back.saturating_sub(3);
            }
            _ => {}
        }
        UserAction::None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UserAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UserAction::None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => UserAction::Quit,
            // Ctrl-D mirrors Ctrl-C so users coming from the CLI prompt
            // (which advertises "Ctrl-D to quit") get the same affordance.
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => UserAction::Quit,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_jobs_panel();
                UserAction::None
            }
            _ if self.panel != TuiPanel::Chat => self.handle_panel_key(key),
            KeyCode::Esc if !self.slash_popup_entries().is_empty() => {
                self.slash_popup_suppressed = true;
                self.slash_popup_selected = 0;
                UserAction::None
            }
            KeyCode::Esc => UserAction::Cancel,
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.leave_history_navigation();
                self.input.insert_newline();
                UserAction::None
            }
            KeyCode::Enter if !self.slash_popup_entries().is_empty() => {
                // Popup is showing matches: Enter accepts the highlighted
                // command instead of submitting whatever the user partially
                // typed.
                self.accept_slash_popup();
                UserAction::None
            }
            KeyCode::Enter if self.is_submit_locked() => {
                // Keep the draft exactly where it is. Once queue pressure
                // clears or cancellation fully settles, Enter will submit
                // again.
                UserAction::None
            }
            KeyCode::Enter => self
                .take_submission()
                .map(UserAction::Submit)
                .unwrap_or(UserAction::None),
            KeyCode::Backspace if self.input.is_empty() && !self.pastes.is_empty() => {
                self.leave_history_navigation();
                self.pastes.pop();
                UserAction::None
            }
            // Popup nav guards call slash_popup_entries() once per arm
            // (instead of the old active() + entries() pair). The arms
            // below cascade to history nav when the popup isn't showing.
            KeyCode::Up if !self.slash_popup_entries().is_empty() => {
                self.slash_popup_selected = self.slash_popup_selected.saturating_sub(1);
                UserAction::None
            }
            KeyCode::Down if !self.slash_popup_entries().is_empty() => {
                let max = self.slash_popup_entries().len().saturating_sub(1);
                self.slash_popup_selected = (self.slash_popup_selected + 1).min(max);
                UserAction::None
            }
            KeyCode::Up if self.should_browse_history() => {
                self.recall_previous_input();
                UserAction::None
            }
            KeyCode::Down if self.should_browse_history() => {
                self.recall_next_input();
                UserAction::None
            }
            KeyCode::Tab => {
                self.complete_slash_command();
                UserAction::None
            }
            KeyCode::PageUp => {
                self.scroll_back = self.scroll_back.saturating_add(8);
                UserAction::None
            }
            KeyCode::PageDown => {
                self.scroll_back = self.scroll_back.saturating_sub(8);
                UserAction::None
            }
            // End / G jump straight to the bottom of the chat (cancel any
            // pending scroll-back) — common less/vim convention. `G`
            // requires an empty input so it doesn't intercept regular
            // typing of the letter g.
            KeyCode::End => {
                self.scroll_back = 0;
                UserAction::None
            }
            KeyCode::Char('g')
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.input.is_empty()
                    && self.pastes.is_empty() =>
            {
                self.scroll_back = 0;
                UserAction::None
            }
            KeyCode::Home if self.input.is_empty() && self.pastes.is_empty() => {
                // Symmetric jump-to-top so the user can quickly skim the
                // beginning of a long transcript without holding PageUp.
                self.scroll_back = usize::MAX;
                UserAction::None
            }
            _ => {
                self.leave_history_navigation();
                // Any text edit changes the popup match set; reset selection
                // so the user always sees the first match highlighted.
                self.slash_popup_suppressed = false;
                self.slash_popup_selected = 0;
                self.input.input(key);
                UserAction::None
            }
        }
    }

    pub(super) fn take_submission(&mut self) -> Option<UserSubmission> {
        let input = self.input.lines().join("\n");
        let input_trimmed = input.trim();
        let has_pastes = !self.pastes.is_empty();

        let mut prompt_parts = Vec::new();
        if !input_trimmed.is_empty() {
            prompt_parts.push(input_trimmed.to_string());
        }
        for paste in &self.pastes {
            let text = trim_newlines(&paste.text);
            if !text.trim().is_empty() {
                prompt_parts.push(text.to_string());
            }
        }

        if prompt_parts.is_empty() {
            self.input = new_input();
            self.pastes.clear();
            return None;
        }

        let input_text = input_trimmed.to_string();
        let pastes = self.pastes.clone();
        let submission = UserSubmission {
            prompt: prompt_parts.join("\n\n"),
            is_slash_command: !has_pastes && input_trimmed.starts_with('/'),
            input_text,
            pastes,
        };
        self.push_input_history(InputDraft {
            input_text: submission.input_text.clone(),
            pastes: submission.pastes.clone(),
        });
        self.input = new_input();
        self.pastes.clear();
        self.leave_history_navigation();
        Some(submission)
    }

    pub(super) fn handle_panel_key(&mut self, key: KeyEvent) -> UserAction {
        match self.panel {
            TuiPanel::Chat => UserAction::None,
            TuiPanel::Jobs => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.panel = TuiPanel::Chat;
                    UserAction::None
                }
                KeyCode::Enter if !self.sh_jobs.is_empty() => {
                    self.panel = TuiPanel::JobDetail;
                    self.job_detail_scroll = 0;
                    UserAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_job = self.selected_job.saturating_sub(1);
                    UserAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !self.sh_jobs.is_empty() {
                        self.selected_job = (self.selected_job + 1).min(self.sh_jobs.len() - 1);
                    }
                    UserAction::None
                }
                KeyCode::PageUp => {
                    self.selected_job = self.selected_job.saturating_sub(8);
                    UserAction::None
                }
                KeyCode::PageDown => {
                    if !self.sh_jobs.is_empty() {
                        self.selected_job = (self.selected_job + 8).min(self.sh_jobs.len() - 1);
                    }
                    UserAction::None
                }
                KeyCode::Home => {
                    self.selected_job = 0;
                    UserAction::None
                }
                KeyCode::End => {
                    self.selected_job = self.sh_jobs.len().saturating_sub(1);
                    UserAction::None
                }
                _ => UserAction::None,
            },
            TuiPanel::JobDetail => match key.code {
                // Esc / q / Enter all go back exactly one level. Previously
                // q skipped straight to Chat, which mismatched the Jobs
                // panel where q == Esc; now both follow the same "back one"
                // semantic so users can rely on muscle memory.
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.panel = TuiPanel::Jobs;
                    UserAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.job_detail_scroll = self.job_detail_scroll.saturating_sub(1);
                    UserAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.job_detail_scroll = self.job_detail_scroll.saturating_add(1);
                    UserAction::None
                }
                KeyCode::PageUp => {
                    self.job_detail_scroll = self.job_detail_scroll.saturating_sub(8);
                    UserAction::None
                }
                KeyCode::PageDown => {
                    self.job_detail_scroll = self.job_detail_scroll.saturating_add(8);
                    UserAction::None
                }
                KeyCode::Home => {
                    self.job_detail_scroll = 0;
                    UserAction::None
                }
                _ => UserAction::None,
            },
        }
    }

    pub(super) fn toggle_jobs_panel(&mut self) {
        self.panel = match self.panel {
            TuiPanel::Chat => TuiPanel::Jobs,
            TuiPanel::Jobs | TuiPanel::JobDetail => TuiPanel::Chat,
        };
        self.job_detail_scroll = 0;
    }

    pub(super) fn current_input(&self) -> String {
        self.input.lines().join("\n")
    }

    pub fn replace_input_history_texts<I, S>(&mut self, texts: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.input_history.clear();
        self.leave_history_navigation();
        for text in texts {
            self.push_input_history(InputDraft {
                input_text: text.into(),
                pastes: Vec::new(),
            });
        }
    }

    fn current_draft(&self) -> InputDraft {
        InputDraft {
            input_text: self.current_input(),
            pastes: self.pastes.clone(),
        }
    }

    pub fn restore_queued_submissions_to_draft(
        &mut self,
        submissions: Vec<UserSubmission>,
    ) -> usize {
        let mut parts = Vec::new();
        let current = draft_prompt_text(&self.current_draft());
        if !current.trim().is_empty() {
            parts.push(current);
        }

        let mut restored = 0usize;
        for UserSubmission {
            prompt,
            input_text,
            pastes,
            ..
        } in submissions
        {
            let draft = InputDraft { input_text, pastes };
            let mut text = draft_prompt_text(&draft);
            if text.trim().is_empty() {
                text = prompt.trim().to_string();
            }
            if !text.trim().is_empty() {
                parts.push(text);
                restored += 1;
            }
        }

        if restored > 0 {
            self.set_input_draft(InputDraft {
                input_text: parts.join("\n\n"),
                pastes: Vec::new(),
            });
            self.leave_history_navigation();
            self.panel = TuiPanel::Chat;
            self.job_detail_scroll = 0;
        }
        restored
    }

    fn should_browse_history(&self) -> bool {
        if self.history_cursor.is_some() {
            return true;
        }
        self.pastes.is_empty() && self.input.lines().len() <= 1
    }

    fn recall_previous_input(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        let idx = match self.history_cursor {
            Some(0) => 0,
            Some(idx) => idx.saturating_sub(1),
            None => {
                self.history_draft = Some(self.current_draft());
                self.input_history.len() - 1
            }
        };
        self.history_cursor = Some(idx);
        self.set_input_draft_from_history(idx);
    }

    fn recall_next_input(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };

        if idx + 1 < self.input_history.len() {
            let next = idx + 1;
            self.history_cursor = Some(next);
            self.set_input_draft_from_history(next);
        } else {
            let draft = self.history_draft.take().unwrap_or(InputDraft {
                input_text: String::new(),
                pastes: Vec::new(),
            });
            self.history_cursor = None;
            self.set_input_draft(draft);
        }
    }

    fn set_input_draft_from_history(&mut self, idx: usize) {
        if let Some(draft) = self.input_history.get(idx).cloned() {
            self.set_input_draft(draft);
        }
    }

    fn set_input_text(&mut self, text: &str) {
        self.leave_history_navigation();
        let mut input = new_input();
        if !text.is_empty() {
            input.insert_str(text);
        }
        self.input = input;
        self.pastes.clear();
        self.slash_popup_suppressed = false;
    }

    pub(super) fn set_input_draft(&mut self, draft: InputDraft) {
        let mut input = new_input();
        if !draft.input_text.is_empty() {
            input.insert_str(&draft.input_text);
        }
        self.input = input;
        self.pastes = draft.pastes;
        self.slash_popup_suppressed = false;
    }

    pub(super) fn leave_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft = None;
    }

    fn push_input_history(&mut self, draft: InputDraft) {
        let draft = normalized_draft(draft);
        if draft_prompt_text(&draft).chars().count() > MAX_HISTORY_ENTRY_CHARS {
            return;
        }
        if draft.input_text.is_empty() && draft.pastes.is_empty() {
            return;
        }
        if self
            .input_history
            .last()
            .map(|last| last == &draft)
            .unwrap_or(false)
        {
            return;
        }

        self.input_history.push(draft);
        if self.input_history.len() > MAX_INPUT_HISTORY {
            self.input_history.remove(0);
        }
    }

    /// Single source of truth for slash-popup state. Returns the matching
    /// `(spec, description)` pairs when the popup should render, and an
    /// empty vec otherwise. Callers ask "is popup active?" via
    /// `!entries.is_empty()` — folded into one walk so handlers don't
    /// iterate `REPL_COMMANDS` twice per keypress.
    pub(super) fn slash_popup_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.history_cursor.is_some() {
            return Vec::new();
        }
        if self.slash_popup_suppressed {
            return Vec::new();
        }
        if self.panel != TuiPanel::Chat || !self.pastes.is_empty() {
            return Vec::new();
        }
        let input = self.current_input();
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
            return Vec::new();
        }
        let prefix = trimmed.to_string();
        crate::cli::REPL_COMMANDS
            .iter()
            .filter_map(|(spec, desc)| {
                spec.split_whitespace()
                    .find(|tok| tok.starts_with('/') && tok.starts_with(&prefix))
                    .map(|_| (*spec, *desc))
            })
            .collect()
    }

    /// Replace the input with the highlighted command and hide the popup.
    /// The trailing space lets the user start typing args immediately.
    fn accept_slash_popup(&mut self) {
        let entries = self.slash_popup_entries();
        if entries.is_empty() {
            return;
        }
        let idx = self.slash_popup_selected.min(entries.len() - 1);
        let spec = entries[idx].0;
        // Pull the first /name token out of the spec (the bare command).
        let name = spec
            .split_whitespace()
            .find(|t| t.starts_with('/'))
            .unwrap_or(spec);
        self.set_input_text(&format!("{name} "));
        self.slash_popup_selected = 0;
    }

    fn complete_slash_command(&mut self) {
        if !self.pastes.is_empty() {
            return;
        }
        let input = self.current_input();
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') || trimmed.split_whitespace().count() > 1 {
            return;
        }
        let prefix = trimmed;
        let matches = crate::cli::repl_command_names()
            .filter(|cmd| cmd.starts_with(prefix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [single] => self.set_input_text(&format!("{single} ")),
            [] => {}
            many => {
                let common = common_prefix(many);
                if common.len() > prefix.len() {
                    self.set_input_text(common);
                }
            }
        }
    }
}

fn normalized_draft(mut draft: InputDraft) -> InputDraft {
    draft.input_text = draft.input_text.trim().to_string();
    draft
        .pastes
        .retain(|paste| !trim_newlines(&paste.text).trim().is_empty());
    draft
}

fn draft_prompt_text(draft: &InputDraft) -> String {
    let mut parts = Vec::new();
    if !draft.input_text.trim().is_empty() {
        parts.push(draft.input_text.trim().to_string());
    }
    for paste in &draft.pastes {
        let text = trim_newlines(&paste.text);
        if !text.trim().is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.join("\n\n")
}
