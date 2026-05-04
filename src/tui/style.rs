//! Pure presentation helpers — colours, blocks, spinner, formatters.
//!
//! Everything here is a free function or const with no `TuiApp` state. The
//! `app` module composes these into rendered widgets. Splitting them out
//! keeps the state machine focused on logic rather than visual chrome.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::adapters::ExecJobState;

use super::ChatRole;

pub(super) fn dim_dot_separator() -> Span<'static> {
    Span::styled(" · ", Style::default().fg(Color::DarkGray))
}

/// 10-frame braille spinner; advances every 100 ms so a typical turn shows
/// a clearly animated indicator without flickering.
pub(super) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) fn spinner_frame_at(elapsed: Duration) -> &'static str {
    let idx = (elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

pub(super) fn format_turn_meta(elapsed: Duration, tokens: u32) -> String {
    let secs = elapsed.as_secs();
    let elapsed_text = if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    };
    if tokens == 0 {
        format!("({elapsed_text})")
    } else {
        format!("({elapsed_text} · {})", format_tokens(tokens))
    }
}

pub(super) fn format_tokens(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k tok", n as f64 / 1000.0)
    } else {
        format!("{n} tok")
    }
}

pub(super) fn format_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(super) fn format_chars(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k chars", chars as f64 / 1000.0)
    } else {
        format!("{} {}", chars, plural(chars, "char", "chars"))
    }
}

pub(super) fn one_line(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let keep = max_chars.saturating_sub(1);
        format!("{}...", compact.chars().take(keep).collect::<String>())
    }
}

pub(super) fn text_lines(text: &str) -> Vec<Line<'static>> {
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

pub(super) fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

pub(super) fn paste_line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

pub(super) fn trim_newlines(text: &str) -> &str {
    text.trim_matches(|ch| ch == '\n' || ch == '\r')
}

pub(super) fn common_prefix<'a>(items: &[&'a str]) -> &'a str {
    let Some(first) = items.first().copied() else {
        return "";
    };
    let mut end = first.len();
    for item in &items[1..] {
        while end > 0 && !item.starts_with(&first[..end]) {
            end -= 1;
        }
    }
    &first[..end]
}

pub(super) fn selected_window_start(selected: usize, visible_rows: usize, len: usize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let max_start = len.saturating_sub(visible_rows);
    selected
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start)
}

pub(super) fn job_state_label(state: &ExecJobState, code: Option<i32>) -> String {
    match state {
        ExecJobState::Running => "running".into(),
        ExecJobState::Exited => format!("exit {}", code.unwrap_or(-1)),
        ExecJobState::TimedOut => "timed out".into(),
        ExecJobState::Killed => "killed".into(),
        ExecJobState::Error => "error".into(),
    }
}

pub(super) fn job_state_style(state: &ExecJobState) -> Style {
    match state {
        ExecJobState::Running => Style::default().fg(Color::Yellow),
        ExecJobState::Exited => Style::default().fg(Color::Green),
        ExecJobState::TimedOut | ExecJobState::Killed | ExecJobState::Error => {
            Style::default().fg(Color::Red)
        }
    }
}

pub(super) fn role_style(role: &ChatRole) -> (&'static str, Style) {
    // Single colored ▎ accent bar conveys role purely via colour and a
    // vertical rule — quieter than the old "> " / "< " / "* " / "! "
    // character prefixes and lets the message body breathe.
    let color = match role {
        ChatRole::User => Color::Green,
        ChatRole::Assistant => Color::Cyan,
        ChatRole::System => Color::Yellow,
        ChatRole::Warning => Color::Yellow,
        ChatRole::Error => Color::Red,
        // Tool rows live alongside the assistant's narrative; using a
        // dim magenta keeps them visually distinct from a plain assistant
        // turn without competing with the more important ▎ accents.
        ChatRole::Tool => Color::Magenta,
    };
    (
        "▎ ",
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

pub(super) fn role_body_style(role: &ChatRole) -> Style {
    match role {
        ChatRole::System => Style::default().fg(Color::DarkGray),
        ChatRole::Warning => Style::default().fg(Color::Yellow),
        ChatRole::Error => Style::default().fg(Color::Red),
        ChatRole::Tool => Style::default().fg(Color::Magenta),
        ChatRole::User | ChatRole::Assistant => Style::default(),
    }
}

pub(super) fn push_wrapped_message_line(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    style: Style,
    body_style: Style,
    raw: &str,
    width: usize,
) {
    let content_width = width.saturating_sub(UnicodeWidthStr::width(label)).max(1);
    for (idx, part) in wrap_by_display_width(raw, content_width)
        .into_iter()
        .enumerate()
    {
        let prefix = if idx == 0 { label } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(part, body_style),
        ]));
    }
}

/// Push one row of code-block body, indented under the role bar with a dim
/// `│` rule and the body in a slightly distinct fg so code stands apart
/// from prose without resorting to a full Markdown renderer.
pub(super) fn push_code_block_line(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    label_style: Style,
    raw: &str,
    width: usize,
) {
    const RULE: &str = " │ ";
    let prefix_w = UnicodeWidthStr::width(label) + UnicodeWidthStr::width(RULE);
    let content_width = width.saturating_sub(prefix_w).max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let body = Style::default().fg(Color::White);
    for (idx, part) in wrap_by_display_width(raw, content_width)
        .into_iter()
        .enumerate()
    {
        let lead = if idx == 0 { label } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(lead.to_string(), label_style),
            Span::styled(RULE.to_string(), dim),
            Span::styled(part, body),
        ]));
    }
}

/// Render a `\`\`\`lang` or trailing `\`\`\`` fence as a thin dim horizontal
/// rule. Replaces the raw fence text so the fence doesn't compete with the
/// code body for attention.
pub(super) fn push_code_fence_rule(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    label_style: Style,
    lang: &str,
    width: usize,
) {
    let prefix_w = UnicodeWidthStr::width(label);
    let inner_w = width.saturating_sub(prefix_w).max(4);
    let rule = if lang.is_empty() {
        "─".repeat(inner_w)
    } else {
        let lead = format!("─ {lang} ");
        let trail = inner_w.saturating_sub(UnicodeWidthStr::width(lead.as_str()));
        format!("{lead}{}", "─".repeat(trail))
    };
    lines.push(Line::from(vec![
        Span::styled(label.to_string(), label_style),
        Span::styled(rule, Style::default().fg(Color::DarkGray)),
    ]));
}

pub(super) fn wrap_by_display_width(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !current.is_empty() && current_width + char_width > width {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    lines.push(current);
    lines
}

pub(super) fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_cursor_line_style(Style::default());
    // A reverse-video cursor blends with the panel's chrome instead of
    // overlaying a saturated cyan block on whatever colour scheme the
    // user's terminal already provides.
    input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    input.set_placeholder_text("Type a message or /help.");
    input.set_placeholder_style(Style::default().fg(Color::DarkGray));
    input
}
