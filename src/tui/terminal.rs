//! Terminal lifecycle: enter raw / alternate-screen mode on construction,
//! restore on drop, and wrap crossterm's input polling into a small typed
//! enum. Knows nothing about `TuiApp` — `draw` takes a closure so the app
//! state can stay isolated in `app.rs`.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, MouseEvent,
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
    /// Mouse events, when the terminal sends them. We intentionally do not
    /// enable mouse capture by default so native text selection/copy keeps
    /// working in common terminals.
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

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
                return Err(err);
            }
        };
        if let Err(err) = terminal.clear() {
            let _ = disable_raw_mode();
            let _ = execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
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
