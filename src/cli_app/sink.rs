//! Output sink for slash-command handlers. The two front-ends (line REPL and
//! full-screen TUI) carry the same command set with different rendering:
//! REPL writes to stdout/stderr, TUI writes into a `TuiApp` panel. Wrapping
//! that difference behind a trait collapses two parallel match tables into
//! one generic handler.

use crate::cli::REPL_COMMANDS;
use crate::cli_app::ReplRuntime;
use crate::core::run_state::RunState;

/// Render the slash-command help table. `indent`, `separator`, and `pad`
/// control the per-row layout; both front-end sinks call this so the
/// catalog stays in one place. REPL prefers a fixed-width left column for
/// terminal alignment; TUI prefers tight `cmd - desc` rows that wrap
/// nicely in a narrow panel.
pub fn render_help(indent: &str, separator: &str, pad: usize) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(REPL_COMMANDS.len() + 1);
    lines.push("Commands:".to_string());
    for (cmd, desc) in REPL_COMMANDS {
        if pad > 0 {
            lines.push(format!("{indent}{cmd:<pad$}{separator}{desc}"));
        } else {
            lines.push(format!("{indent}{cmd}{separator}{desc}"));
        }
    }
    lines.join("\n")
}

/// What kind of session swap a command performed. The sink renders this in
/// front-end-specific ways: REPL prints a one-liner; TUI clears the chat
/// panel and seeds it with the new session's recent history.
pub enum SessionResetKind {
    New,
    Continued,
    /// Forked from a specific run; the intro line is sink-rendered for TUI.
    Forked { intro: String },
}

/// Front-end-agnostic write surface for command handlers.
pub trait CommandSink {
    /// Plain informational line.
    fn info(&mut self, msg: String);
    /// Error or usage line. REPL writes to stderr; TUI tags it visually.
    fn error(&mut self, msg: String);
    /// Action-success status. REPL wraps in parens for the historical
    /// `(continued session ...)` look; TUI emits as plain system text.
    fn status(&mut self, msg: String);
    /// Atomic multi-line block (e.g. a session list, search hits). REPL
    /// prints each line; TUI joins them into one panel entry so the lines
    /// stay grouped under the same scroll anchor.
    fn lines(&mut self, lines: Vec<String>);

    /// Notify of a state-changing session swap (`/new`, `/continue`, `/fork`).
    /// Default: no-op for sinks that don't need to refresh stateful UI.
    fn on_session_replaced(&mut self, _next: &RunState, _kind: SessionResetKind) {}

    /// Notify after `/model` or `/provider` swapped the runtime config.
    fn on_runtime_switched(&mut self, _runtime: &ReplRuntime) {}

    /// Help text. Two front-ends prefer different layouts.
    fn help_text(&self) -> String;
}

/// Sink for the line REPL — println / eprintln, no panel state.
pub struct StdoutSink;

impl CommandSink for StdoutSink {
    fn info(&mut self, msg: String) {
        println!("{msg}");
    }
    fn error(&mut self, msg: String) {
        eprintln!("{msg}");
    }
    fn status(&mut self, msg: String) {
        println!("({msg})");
    }
    fn lines(&mut self, lines: Vec<String>) {
        for line in lines {
            println!("  {line}");
        }
    }
    fn on_session_replaced(&mut self, next: &RunState, kind: SessionResetKind) {
        match kind {
            SessionResetKind::New => println!("(new session)"),
            SessionResetKind::Continued => println!(
                "(continued session {}; new run {})",
                next.session_id, next.run_id
            ),
            SessionResetKind::Forked { intro } => println!("({intro})"),
        }
    }
    fn help_text(&self) -> String {
        // REPL: two-space indent + 18-char left column for terminal alignment.
        render_help("  ", " ", 18)
    }
}
