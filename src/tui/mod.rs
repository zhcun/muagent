//! Small full-screen terminal UI for interactive `muagent` sessions.
//!
//! This module owns terminal rendering and interactive input state. Agent
//! execution, slash commands, and session management stay in the binary so
//! the core runtime does not depend on presentation details.
//!
//! Submodules:
//!
//! - [`terminal`] — raw-mode / alternate-screen lifecycle and event polling.
//!   Knows nothing about [`TuiApp`]; `draw` accepts a closure.
//! - [`style`] — pure presentation helpers: rounded panel chrome, status
//!   dots, spinner frames, byte / token / elapsed formatters, role colours.
//! - [`app`] — the [`TuiApp`] state machine: input, paste queue, message
//!   log, panels, sh-job cache, per-turn spinner / token counter, and the
//!   `render` orchestration.

mod app;
mod style;
mod terminal;

pub use app::{ShJobView, TuiApp, TuiConfig, UserAction, UserSubmission};
pub use terminal::{TerminalSession, TuiEvent, TuiTerminal};

/// Visual role of a chat row. Defined here (top of the module) because both
/// `app` (which constructs `ChatMessage`s) and `style` (which colours them)
/// need to reference it; keeping it at the module root avoids a cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Warning,
    Error,
    /// A tool invocation rendered inline in the conversation flow, e.g.
    /// `⏺ Bash(sleep 10) ✓` or `⏺ Update(src/foo.rs) ✗ failed`.
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}
