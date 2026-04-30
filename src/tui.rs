//! Small full-screen terminal UI for interactive `muagent` sessions.
//!
//! This module only owns terminal rendering and input state. Agent execution,
//! slash commands, and session management stay in the CLI binary so the core
//! runtime does not depend on presentation details.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalSession {
    terminal: TuiTerminal,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err);
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                return Err(err);
            }
        };
        if let Err(err) = terminal.clear() {
            let _ = disable_raw_mode();
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            return Err(err);
        }
        Ok(Self { terminal })
    }

    pub fn draw(&mut self, app: &TuiApp) -> io::Result<()> {
        self.terminal.draw(|frame| app.render(frame))?;
        Ok(())
    }

    pub fn read_key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) => Ok(Some(key)),
            _ => Ok(None),
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfig {
    pub provider: String,
    pub model: String,
    pub store: String,
    pub root: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserAction {
    None,
    Submit(String),
    Quit,
}

pub struct TuiApp {
    config: TuiConfig,
    status: String,
    input: String,
    messages: Vec<ChatMessage>,
    scroll_back: u16,
}

impl TuiApp {
    pub fn new(config: TuiConfig) -> Self {
        Self {
            config,
            status: "idle".into(),
            input: String::new(),
            messages: Vec::new(),
            scroll_back: 0,
        }
    }

    pub fn set_runtime(&mut self, provider: impl Into<String>, model: impl Into<String>) {
        self.config.provider = provider.into();
        self.config.model = model.into();
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
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

    pub fn handle_key(&mut self, key: KeyEvent) -> UserAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UserAction::None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => UserAction::Quit,
            KeyCode::Esc => UserAction::Quit,
            KeyCode::Enter => {
                let submitted = self.input.trim().to_string();
                self.input.clear();
                if submitted.is_empty() {
                    UserAction::None
                } else {
                    UserAction::Submit(submitted)
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
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
            KeyCode::Up => {
                self.scroll_back = self.scroll_back.saturating_add(1);
                UserAction::None
            }
            KeyCode::Down => {
                self.scroll_back = self.scroll_back.saturating_sub(1);
                UserAction::None
            }
            KeyCode::Char(ch) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    self.input.push(ch);
                }
                UserAction::None
            }
            _ => UserAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                "muAgent",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  provider={}  model={}  store={}  root={}",
                self.config.provider, self.config.model, self.config.store, self.config.root
            )),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Session"));
        frame.render_widget(header, chunks[0]);

        let message_lines = self.message_lines();
        let visible_rows = chunks[1].height.saturating_sub(2) as usize;
        let max_scroll = message_lines.len().saturating_sub(visible_rows) as u16;
        let scroll = max_scroll.saturating_sub(self.scroll_back);
        let messages = Paragraph::new(message_lines)
            .block(Block::default().borders(Borders::ALL).title("Messages"))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(messages, chunks[1]);

        let input_area = chunks[2];
        let input_width = input_area.width.saturating_sub(2) as usize;
        let input_view = tail_chars(&self.input, input_width);
        let input = Paragraph::new(input_view.as_str())
            .block(Block::default().borders(Borders::ALL).title("Input"));
        frame.render_widget(input, input_area);

        if input_area.width > 2 && input_area.height > 2 {
            let cursor_x = input_area.x
                + 1
                + input_view
                    .chars()
                    .count()
                    .min(input_width.saturating_sub(1)) as u16;
            frame.set_cursor_position(Position {
                x: cursor_x,
                y: input_area.y + 1,
            });
        }

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw(" Enter submit | /help commands | Esc/Ctrl-C quit | PgUp/PgDn scroll"),
        ]));
        frame.render_widget(footer, chunks[3]);
    }

    fn add(&mut self, role: ChatRole, text: impl Into<String>) {
        self.messages.push(ChatMessage {
            role,
            text: text.into(),
        });
        self.scroll_back = 0;
    }

    fn message_lines(&self) -> Vec<Line<'static>> {
        if self.messages.is_empty() {
            return vec![Line::from(Span::styled(
                "Type a message or /help.",
                Style::default().fg(Color::DarkGray),
            ))];
        }

        let mut lines = Vec::new();
        for message in &self.messages {
            let (label, style) = role_style(&message.role);
            for (idx, raw) in message.text.lines().enumerate() {
                let prefix = if idx == 0 { label } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), style),
                    Span::raw(raw.to_string()),
                ]));
            }
            lines.push(Line::raw(""));
        }
        lines
    }
}

fn role_style(role: &ChatRole) -> (&'static str, Style) {
    match role {
        ChatRole::User => (
            "> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ChatRole::Assistant => (
            "< ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        ChatRole::System => ("* ", Style::default().fg(Color::Yellow)),
        ChatRole::Error => ("! ", Style::default().fg(Color::Red)),
    }
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> TuiApp {
        TuiApp::new(TuiConfig {
            provider: "openrouter".into(),
            model: "openai/gpt-test".into(),
            store: "memory".into(),
            root: ".".into(),
        })
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
            UserAction::Submit("hi".into())
        );
    }

    #[test]
    fn escape_quits() {
        assert_eq!(
            app().handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::Quit
        );
    }

    #[test]
    fn tail_chars_preserves_short_text_and_trims_long_text() {
        assert_eq!(tail_chars("abc", 8), "abc");
        assert_eq!(tail_chars("abcdef", 3), "def");
    }
}
