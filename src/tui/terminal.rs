//! Terminal lifecycle: enter raw / alternate-screen mode on construction,
//! restore on drop, and wrap crossterm's input polling into a small typed
//! enum. Knows nothing about `TuiApp` — `draw` takes a closure so the app
//! state can stay isolated in `app.rs`.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEvent, MouseEvent,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalSession {
    terminal: TuiTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    /// Wheel / click events delivered by crossterm's mouse capture. The
    /// app handler currently only acts on scroll up/down, but exposing the
    /// raw event keeps room for future click-to-select / drag interactions.
    Mouse(MouseEvent),
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        let _ = execute!(stdout, EnableBracketedPaste);
        // Mouse capture lets us route wheel events to scroll the chat. The
        // tradeoff: the terminal's native click-and-drag selection is
        // intercepted, so users select text via Shift+drag (most terminals
        // honour this as a "passthrough" gesture).
        let _ = execute!(stdout, EnableMouseCapture);

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                let _ = execute!(
                    io::stdout(),
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                return Err(err);
            }
        };
        if let Err(err) = terminal.clear() {
            let _ = disable_raw_mode();
            let _ = execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            return Err(err);
        }
        Ok(Self { terminal })
    }

    /// Render one frame. Caller passes a closure so the session does not
    /// depend on `TuiApp` — keeping presentation logic out of the lifecycle
    /// module.
    pub fn draw<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal.draw(render)?;
        Ok(())
    }

    pub fn read_event(timeout: Duration) -> io::Result<Option<TuiEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) => Ok(Some(TuiEvent::Key(key))),
            Event::Paste(text) => Ok(Some(TuiEvent::Paste(text))),
            Event::Mouse(m) => Ok(Some(TuiEvent::Mouse(m))),
            _ => Ok(None),
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}
