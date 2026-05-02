//! Small full-screen terminal UI for interactive `muagent` sessions.
//!
//! This module only owns terminal rendering and input state. Agent execution,
//! slash commands, and session management stay in the CLI binary so the core
//! runtime does not depend on presentation details.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tui_textarea::TextArea;

use crate::adapters::{ExecJobSnapshot, ExecJobState};

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalSession {
    terminal: TuiTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
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

    pub fn draw(&mut self, app: &TuiApp) -> io::Result<()> {
        self.terminal.draw(|frame| app.render(frame))?;
        Ok(())
    }

    pub fn read_event(timeout: Duration) -> io::Result<Option<TuiEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) => Ok(Some(TuiEvent::Key(key))),
            Event::Paste(text) => Ok(Some(TuiEvent::Paste(text))),
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
    Submit(UserSubmission),
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSubmission {
    pub prompt: String,
    pub display: String,
    pub is_slash_command: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteBlock {
    text: String,
    line_count: usize,
    char_count: usize,
}

impl PasteBlock {
    fn new(text: String) -> Self {
        Self {
            line_count: paste_line_count(&text),
            char_count: text.chars().count(),
            text,
        }
    }
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
enum TuiPanel {
    Chat,
    Jobs,
    JobDetail,
}

pub struct TuiApp {
    config: TuiConfig,
    status: String,
    input: TextArea<'static>,
    pastes: Vec<PasteBlock>,
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: Option<String>,
    panel: TuiPanel,
    sh_jobs: Vec<ShJobView>,
    selected_job: usize,
    job_detail_scroll: u16,
    messages: Vec<ChatMessage>,
    scroll_back: u16,
}

impl TuiApp {
    pub fn new(config: TuiConfig) -> Self {
        Self {
            config,
            status: "idle".into(),
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

    pub fn handle_paste(&mut self, text: String) -> UserAction {
        if self.panel != TuiPanel::Chat || text.is_empty() {
            return UserAction::None;
        }

        let line_count = paste_line_count(&text);
        let char_count = text.chars().count();
        if line_count <= 1 && char_count <= 400 {
            self.leave_history_navigation();
            self.input.insert_str(text);
        } else {
            self.leave_history_navigation();
            self.pastes.push(PasteBlock::new(text));
        }
        UserAction::None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UserAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UserAction::None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => UserAction::Quit,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_jobs_panel();
                UserAction::None
            }
            KeyCode::F(2) => {
                self.toggle_jobs_panel();
                UserAction::None
            }
            _ if self.panel != TuiPanel::Chat => self.handle_panel_key(key),
            KeyCode::Esc => UserAction::Quit,
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.leave_history_navigation();
                self.input.insert_newline();
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
            KeyCode::Up if self.should_browse_history() => {
                self.recall_previous_input();
                UserAction::None
            }
            KeyCode::Down if self.should_browse_history() => {
                self.recall_next_input();
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
            _ => {
                self.leave_history_navigation();
                self.input.input(key);
                UserAction::None
            }
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(5),
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

        match self.panel {
            TuiPanel::Chat => self.render_messages(frame, chunks[1]),
            TuiPanel::Jobs => self.render_jobs(frame, chunks[1]),
            TuiPanel::JobDetail => self.render_job_detail(frame, chunks[1]),
        }

        let input_area = chunks[2];
        let input_block = Block::default().borders(Borders::ALL).title("Input");
        let input_inner = input_block.inner(input_area);
        frame.render_widget(input_block, input_area);
        if self.pastes.is_empty() {
            frame.render_widget(&self.input, input_inner);
        } else {
            let input_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(input_inner);
            frame.render_widget(Paragraph::new(self.paste_summary_line()), input_chunks[0]);
            frame.render_widget(&self.input, input_chunks[1]);
        }

        let help = if self.pastes.is_empty() {
            " Ctrl-B/F2 sh jobs | Enter submit | Up/Down history | Esc/Ctrl-C quit"
        } else {
            " Ctrl-B/F2 sh jobs | Enter submit | Backspace removes paste | Esc/Ctrl-C quit"
        };
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(
                format!(" {} ", self.sh_job_status_text()),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ),
            Span::raw(help),
        ]));
        frame.render_widget(footer, chunks[3]);
    }

    fn render_messages(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let message_lines = self.message_lines();
        let visible_rows = area.height.saturating_sub(2) as usize;
        let max_scroll = message_lines.len().saturating_sub(visible_rows) as u16;
        let scroll = max_scroll.saturating_sub(self.scroll_back);
        let messages = Paragraph::new(message_lines)
            .block(Block::default().borders(Borders::ALL).title("Messages"))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(messages, area);
    }

    fn render_jobs(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let visible_rows = area.height.saturating_sub(2) as usize;
        let start = selected_window_start(self.selected_job, visible_rows, self.sh_jobs.len());
        let mut lines = Vec::new();
        if self.sh_jobs.is_empty() {
            lines.push(Line::from(Span::styled(
                "No background sh jobs yet.",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (idx, job) in self
                .sh_jobs
                .iter()
                .enumerate()
                .skip(start)
                .take(visible_rows.max(1))
            {
                let selected = idx == self.selected_job;
                let style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    job_state_style(&job.state)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} {}  {}  out={} err={}  {}",
                        if selected { ">" } else { " " },
                        job_state_label(&job.state, job.code),
                        format_elapsed(job.elapsed_ms),
                        format_bytes(job.stdout_bytes),
                        format_bytes(job.stderr_bytes),
                        one_line(&job.command, 96),
                    ),
                    style,
                )));
            }
        }

        let jobs = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(
                "Background sh jobs - {} - Enter detail - Esc close",
                self.sh_job_status_text()
            )))
            .wrap(Wrap { trim: false });
        frame.render_widget(jobs, area);
    }

    fn render_job_detail(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let Some(job) = self.sh_jobs.get(self.selected_job) else {
            self.render_jobs(frame, area);
            return;
        };

        let mut lines = vec![
            Line::raw(format!("job_id: {}", job.job_id)),
            Line::raw(format!("state: {}", job_state_label(&job.state, job.code))),
            Line::raw(format!("elapsed: {}", format_elapsed(job.elapsed_ms))),
            Line::raw(format!("command: {}", job.command)),
            Line::raw(format!(
                "stdout: {}  stderr: {}  truncated: {}",
                format_bytes(job.stdout_bytes),
                format_bytes(job.stderr_bytes),
                job.output_truncated
            )),
        ];
        if let Some(error) = &job.error {
            lines.push(Line::raw(format!("error: {error}")));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "--- stdout tail ---",
            Style::default().fg(Color::Green),
        )));
        lines.extend(text_lines(&job.stdout_tail));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "--- stderr tail ---",
            Style::default().fg(Color::Red),
        )));
        lines.extend(text_lines(&job.stderr_tail));

        let detail = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("sh job detail - Esc list - Ctrl-B close"),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.job_detail_scroll, 0));
        frame.render_widget(detail, area);
    }

    fn add(&mut self, role: ChatRole, text: impl Into<String>) {
        self.messages.push(ChatMessage {
            role,
            text: text.into(),
        });
        self.scroll_back = 0;
    }

    fn take_submission(&mut self) -> Option<UserSubmission> {
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

        let mut display_parts = Vec::new();
        if !input_trimmed.is_empty() {
            display_parts.push(input_trimmed.to_string());
        }
        if has_pastes {
            display_parts.push(self.paste_summary_text());
        }

        let submission = UserSubmission {
            prompt: prompt_parts.join("\n\n"),
            display: display_parts.join("\n"),
            is_slash_command: !has_pastes && input_trimmed.starts_with('/'),
        };
        if !input_trimmed.is_empty() {
            self.push_input_history(input_trimmed);
        }
        self.input = new_input();
        self.pastes.clear();
        self.leave_history_navigation();
        Some(submission)
    }

    fn handle_panel_key(&mut self, key: KeyEvent) -> UserAction {
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
                KeyCode::Up => {
                    self.selected_job = self.selected_job.saturating_sub(1);
                    UserAction::None
                }
                KeyCode::Down => {
                    if !self.sh_jobs.is_empty() {
                        self.selected_job = (self.selected_job + 1).min(self.sh_jobs.len() - 1);
                    }
                    UserAction::None
                }
                _ => UserAction::None,
            },
            TuiPanel::JobDetail => match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.panel = TuiPanel::Jobs;
                    UserAction::None
                }
                KeyCode::Char('q') => {
                    self.panel = TuiPanel::Chat;
                    UserAction::None
                }
                KeyCode::Up => {
                    self.job_detail_scroll = self.job_detail_scroll.saturating_sub(1);
                    UserAction::None
                }
                KeyCode::Down => {
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
                _ => UserAction::None,
            },
        }
    }

    fn toggle_jobs_panel(&mut self) {
        self.panel = match self.panel {
            TuiPanel::Chat => TuiPanel::Jobs,
            TuiPanel::Jobs | TuiPanel::JobDetail => TuiPanel::Chat,
        };
        self.job_detail_scroll = 0;
    }

    fn sh_job_status_text(&self) -> String {
        let running = self
            .sh_jobs
            .iter()
            .filter(|job| job.state == ExecJobState::Running)
            .count();
        format!("sh bg: {running}/{}", self.sh_jobs.len())
    }

    fn current_input(&self) -> String {
        self.input.lines().join("\n")
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
                self.history_draft = Some(self.current_input());
                self.input_history.len() - 1
            }
        };
        self.history_cursor = Some(idx);
        self.set_input_text_from_history(idx);
    }

    fn recall_next_input(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };

        if idx + 1 < self.input_history.len() {
            let next = idx + 1;
            self.history_cursor = Some(next);
            self.set_input_text_from_history(next);
        } else {
            let draft = self.history_draft.take().unwrap_or_default();
            self.history_cursor = None;
            self.set_input_text(&draft);
        }
    }

    fn set_input_text_from_history(&mut self, idx: usize) {
        if let Some(text) = self.input_history.get(idx).cloned() {
            self.set_input_text(&text);
        }
    }

    fn set_input_text(&mut self, text: &str) {
        let mut input = new_input();
        input.insert_str(text);
        self.input = input;
    }

    fn leave_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft = None;
    }

    fn push_input_history(&mut self, text: &str) {
        const MAX_INPUT_HISTORY: usize = 100;
        const MAX_HISTORY_ENTRY_CHARS: usize = 8000;

        let text = text.trim();
        if text.is_empty() || text.chars().count() > MAX_HISTORY_ENTRY_CHARS {
            return;
        }
        if self
            .input_history
            .last()
            .map(|last| last == text)
            .unwrap_or(false)
        {
            return;
        }

        self.input_history.push(text.to_string());
        if self.input_history.len() > MAX_INPUT_HISTORY {
            self.input_history.remove(0);
        }
    }

    fn paste_summary_line(&self) -> Line<'static> {
        Line::from(Span::styled(
            self.paste_summary_text(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    }

    fn paste_summary_text(&self) -> String {
        let blocks = self.pastes.len();
        let lines: usize = self.pastes.iter().map(|paste| paste.line_count).sum();
        let chars: usize = self.pastes.iter().map(|paste| paste.char_count).sum();
        if blocks == 1 {
            format!(
                "[pasted {} {}, {}]",
                lines,
                plural(lines, "line", "lines"),
                format_chars(chars)
            )
        } else {
            format!(
                "[pasted {} blocks, {} {}, {}]",
                blocks,
                lines,
                plural(lines, "line", "lines"),
                format_chars(chars)
            )
        }
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

fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_cursor_line_style(Style::default());
    input.set_cursor_style(Style::default().fg(Color::Black).bg(Color::Cyan));
    input.set_placeholder_text("Type a message or /help.");
    input.set_placeholder_style(Style::default().fg(Color::DarkGray));
    input
}

fn paste_line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn format_chars(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k chars", chars as f64 / 1000.0)
    } else {
        format!("{} {}", chars, plural(chars, "char", "chars"))
    }
}

fn selected_window_start(selected: usize, visible_rows: usize, len: usize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let max_start = len.saturating_sub(visible_rows);
    selected
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start)
}

fn job_state_label(state: &ExecJobState, code: Option<i32>) -> String {
    match state {
        ExecJobState::Running => "running".into(),
        ExecJobState::Exited => format!("exit {}", code.unwrap_or(-1)),
        ExecJobState::TimedOut => "timed out".into(),
        ExecJobState::Killed => "killed".into(),
        ExecJobState::Error => "error".into(),
    }
}

fn job_state_style(state: &ExecJobState) -> Style {
    match state {
        ExecJobState::Running => Style::default().fg(Color::Yellow),
        ExecJobState::Exited => Style::default().fg(Color::Green),
        ExecJobState::TimedOut | ExecJobState::Killed | ExecJobState::Error => {
            Style::default().fg(Color::Red)
        }
    }
}

fn format_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let keep = max_chars.saturating_sub(1);
        format!("{}...", compact.chars().take(keep).collect::<String>())
    }
}

fn text_lines(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from(Span::styled(
            "(empty)",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    text.lines()
        .map(|line| Line::raw(line.to_string()))
        .collect()
}

fn trim_newlines(text: &str) -> &str {
    text.trim_matches(|ch| ch == '\n' || ch == '\r')
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
            UserAction::Submit(UserSubmission {
                prompt: "hi".into(),
                display: "hi".into(),
                is_slash_command: false,
            })
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
            UserAction::Submit(UserSubmission {
                prompt: "h!i".into(),
                display: "h!i".into(),
                is_slash_command: false,
            })
        );
    }

    #[test]
    fn multiline_paste_is_submitted_but_displayed_as_summary() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UserAction::Submit(UserSubmission {
                prompt: "s\n\none\ntwo\nthree".into(),
                display: "s\n[pasted 3 lines, 13 chars]".into(),
                is_slash_command: false,
            })
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
            UserAction::Submit(UserSubmission {
                prompt: "/help\nnot a command".into(),
                display: "[pasted 2 lines, 19 chars]".into(),
                is_slash_command: false,
            })
        );
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
            UserAction::Submit(UserSubmission {
                prompt: "draft".into(),
                display: "draft".into(),
                is_slash_command: false,
            })
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
            UserAction::Submit(UserSubmission {
                prompt: "first".into(),
                display: "first".into(),
                is_slash_command: false,
            })
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
            UserAction::Submit(UserSubmission {
                prompt: "a\nb".into(),
                display: "a\nb".into(),
                is_slash_command: false,
            })
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
    fn escape_still_quits_chat_view() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UserAction::Quit
        );
    }

    #[test]
    fn paste_is_ignored_while_jobs_panel_is_open() {
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
            UserAction::None
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
            UserAction::Submit(UserSubmission {
                prompt: "draft".into(),
                display: "draft".into(),
                is_slash_command: false,
            })
        );
    }

    #[test]
    fn render_snapshot_shows_footer_and_paste_summary() {
        let mut app = app();
        app.set_status("idle");
        app.add_system("ready");
        app.set_sh_jobs(vec![
            job("sh_1", ExecJobState::Running, None, "sleep 10"),
            job("sh_2", ExecJobState::Exited, Some(0), "true"),
        ]);
        assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);

        let screen = render_text(&app, 100, 22);
        assert!(screen.contains("muAgent"), "{screen}");
        assert!(screen.contains("Messages"), "{screen}");
        assert!(screen.contains("Input"), "{screen}");
        assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
        assert!(screen.contains("sh bg: 1/2"), "{screen}");
        assert!(screen.contains("Ctrl-B/F2 sh jobs"), "{screen}");
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
        assert!(detail.contains("--- stdout tail ---"), "{detail}");
        assert!(detail.contains("ok"), "{detail}");
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
