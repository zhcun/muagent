//! TUI-side helpers shared by `commands` and `tui_driver` — placed here to
//! break the soft cycle the two modules used to form (commands needed
//! tui_driver helpers; tui_driver invoked commands::handle_tui_command).

use crate::cli_app::sink::{render_help, CommandSink, SessionResetKind};
use crate::cli_app::{content_text, ReplRuntime};
use crate::config::{EffortCfg, ThinkingModeCfg};
use crate::core::run_state::RunState;
use crate::core::types::{Content, Message};
use crate::tui::TuiApp;

/// Sink that routes command output into the TUI chat panel.
pub struct TuiAppSink<'a> {
    pub app: &'a mut TuiApp,
    had_error: bool,
}

impl<'a> TuiAppSink<'a> {
    pub fn new(app: &'a mut TuiApp) -> Self {
        Self {
            app,
            had_error: false,
        }
    }

    pub fn had_error(&self) -> bool {
        self.had_error
    }
}

impl CommandSink for TuiAppSink<'_> {
    fn info(&mut self, msg: String) {
        self.app.add_system(msg);
    }
    fn error(&mut self, msg: String) {
        self.had_error = true;
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
                self.app.replace_input_history_texts(Vec::<String>::new());
                self.app.add_activity("New session");
            }
            SessionResetKind::Continued => {
                self.app.clear_messages();
                seed_tui_history_messages(self.app, next);
                seed_tui_input_history(self.app, next);
            }
            SessionResetKind::Forked { intro } => {
                self.app.clear_messages();
                seed_tui_history_messages_with_intro(self.app, next, intro);
                seed_tui_input_history(self.app, next);
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
        provider_label(&runtime.cfg.model.provider),
        runtime.cfg.model.model.clone(),
        thinking_label(
            runtime.cfg.runtime.thinking_mode,
            runtime.cfg.runtime.thinking_effort,
        ),
    );
    app.set_context_window(runtime.cfg.model.capabilities.ctx_len);
    app.set_last_prompt_tokens(0);
}

pub fn thinking_label(mode: ThinkingModeCfg, effort: Option<EffortCfg>) -> String {
    match (mode, effort) {
        (ThinkingModeCfg::Off, _) => "off".into(),
        (ThinkingModeCfg::Auto, _) => "auto".into(),
        (ThinkingModeCfg::Enabled, Some(EffortCfg::Minimal)) => "minimal".into(),
        (ThinkingModeCfg::Enabled, Some(EffortCfg::Low)) => "low".into(),
        (ThinkingModeCfg::Enabled, Some(EffortCfg::Medium)) => "medium".into(),
        (ThinkingModeCfg::Enabled, Some(EffortCfg::High)) => "high".into(),
        (ThinkingModeCfg::Enabled, Some(EffortCfg::Max)) => "max".into(),
        (ThinkingModeCfg::Enabled, None) => "on".into(),
    }
}

pub fn provider_label(provider: &impl std::fmt::Debug) -> String {
    match format!("{provider:?}").as_str() {
        "OpenAi" => "openai".into(),
        "OpenAiCodex" | "Codex" => "codex".into(),
        "Anthropic" => "anthropic".into(),
        "Google" => "google".into(),
        "OpenRouter" => "openrouter".into(),
        other => other.to_lowercase(),
    }
}

pub fn seed_tui_history_messages(app: &mut TuiApp, state: &RunState) {
    seed_tui_history_messages_with_intro(
        app,
        state,
        format!("continued session {}", state.session_id),
    );
}

pub fn seed_tui_input_history(app: &mut TuiApp, state: &RunState) {
    let inputs = state
        .history
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => {
                let text = content_text(content);
                (!text.trim().is_empty()).then_some(text)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    app.replace_input_history_texts(inputs);
}

pub fn seed_tui_history_messages_with_intro(
    app: &mut TuiApp,
    state: &RunState,
    intro: impl Into<String>,
) {
    const MAX_RESUME_MESSAGES: usize = 8;
    app.add_activity(intro.into());

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
