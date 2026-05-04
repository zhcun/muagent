//! Value types used by `TuiApp`. Kept in their own file so `mod.rs` can
//! focus on the state machine and `render.rs` / `input.rs` on behaviour.

use crate::adapters::{ExecJobSnapshot, ExecJobState};

use super::super::style::paste_line_count;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfig {
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub root: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserAction {
    None,
    Submit(UserSubmission),
    Cancel,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSubmission {
    pub prompt: String,
    pub is_slash_command: bool,
    pub(in crate::tui::app) input_text: String,
    pub(in crate::tui::app) pastes: Vec<PasteBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui::app) struct PasteBlock {
    pub(in crate::tui::app) text: String,
    pub(in crate::tui::app) line_count: usize,
    pub(in crate::tui::app) char_count: usize,
}

impl PasteBlock {
    pub(in crate::tui::app) fn new(text: String) -> Self {
        Self {
            line_count: paste_line_count(&text),
            char_count: text.chars().count(),
            text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui::app) struct InputDraft {
    pub(in crate::tui::app) input_text: String,
    pub(in crate::tui::app) pastes: Vec<PasteBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShJobView {
    pub job_id: String,
    pub state: ExecJobState,
    pub code: Option<i32>,
    pub command: String,
    pub elapsed_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub output_truncated: bool,
    pub error: Option<String>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

impl ShJobView {
    pub fn from_snapshot(snap: ExecJobSnapshot) -> Self {
        Self {
            job_id: snap.job_id,
            state: snap.state,
            code: snap.code,
            command: snap.command,
            elapsed_ms: snap.elapsed.as_millis() as u64,
            stdout_bytes: snap.stdout_bytes,
            stderr_bytes: snap.stderr_bytes,
            output_truncated: snap.output_truncated,
            error: snap.error,
            stdout_tail: String::from_utf8_lossy(&snap.stdout_tail).into_owned(),
            stderr_tail: String::from_utf8_lossy(&snap.stderr_tail).into_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui::app) enum TuiPanel {
    Chat,
    Jobs,
    JobDetail,
}
