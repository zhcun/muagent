//! TUI-side helpers shared by `commands` and `tui_driver` — placed here to
//! break the soft cycle the two modules used to form (commands needed
//! tui_driver helpers; tui_driver invoked commands::handle_tui_command).

use crate::cli_app::sink::{render_help, CommandSink, SessionResetKind};
use crate::cli_app::{content_text, ReplRuntime};
use crate::core::run_state::RunState;
use crate::core::types::{Content, Message};
use crate::tui::TuiApp;

/// Sink that routes command output into the TUI chat panel.
pub struct TuiAppSink<'a> {
    pub app: &'a mut TuiApp,
}

impl<'a> TuiAppSink<'a> {
    pub fn new(app: &'a mut TuiApp) -> Self {
        Self { app }
    }
}

impl CommandSink for TuiAppSink<'_> {
    fn info(&mut self, msg: String) {
        self.app.add_system(msg);
    }
    fn error(&mut self, msg: String) {
        self.app.add_error(msg);
    }
    fn status(&mut self, msg: String) {
        self.app.add_system(msg);
    }
    fn lines(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        self.app.add_system(lines.join("\n"));
    }
    fn on_session_replaced(&mut self, next: &RunState, kind: SessionResetKind) {
        match kind {
            SessionResetKind::New => {
                self.app.clear_messages();
                self.app
                    .add_system(format!("new session {}", next.session_id));
            }
            SessionResetKind::Continued => {
                self.app.clear_messages();
                seed_tui_history_messages(self.app, next);
            }
            SessionResetKind::Forked { intro } => {
                self.app.clear_messages();
                seed_tui_history_messages_with_intro(self.app, next, intro);
            }
        }
    }
    fn on_runtime_switched(&mut self, runtime: &ReplRuntime) {
        sync_tui_runtime(self.app, runtime);
    }
    fn help_text(&self) -> String {
        // TUI: tight `cmd - desc` rows so the panel doesn't waste columns
        // on padding when it's already narrow.
        render_help("", " - ", 0)
    }
}

pub fn sync_tui_runtime(app: &mut TuiApp, runtime: &ReplRuntime) {
    app.set_runtime(
        format!("{:?}", runtime.cfg.model.provider),
        runtime.cfg.model.model.clone(),
    );
}

pub fn seed_tui_history_messages(app: &mut TuiApp, state: &RunState) {
    seed_tui_history_messages_with_intro(
        app,
        state,
        format!("continued session {}", state.session_id),
    );
}

pub fn seed_tui_history_messages_with_intro(
    app: &mut TuiApp,
    state: &RunState,
    intro: impl Into<String>,
) {
    const MAX_RESUME_MESSAGES: usize = 8;
    app.add_system(intro.into());

    let start = state.history.len().saturating_sub(MAX_RESUME_MESSAGES);
    let mut shown = 0usize;
    for message in state.history.iter().skip(start) {
        match message {
            Message::User { content } => {
                app.add_user(history_content_preview(content));
                shown += 1;
            }
            Message::Assistant { content, .. } => {
                let preview = history_content_preview(content);
                if !preview.trim().is_empty() {
                    app.add_assistant(preview);
                    shown += 1;
                }
            }
            Message::System { content } => {
                let preview = history_content_preview(content);
                if !preview.trim().is_empty() {
                    app.add_system(preview);
                    shown += 1;
                }
            }
            Message::ToolResult { .. } | Message::Observation { .. } => {}
        }
    }

    if shown == 0 {
        app.add_system("(no recent user/assistant messages)");
    }
}

fn history_content_preview(content: &Content) -> String {
    const MAX_CHARS: usize = 1200;
    let mut text = content_text(content);
    if text.trim().is_empty() {
        text = "(empty message)".into();
    }
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let preview = text.chars().take(MAX_CHARS).collect::<String>();
    format!(
        "{preview}\n[truncated; full message is {} chars]",
        text.chars().count()
    )
}
