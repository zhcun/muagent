//! Rendering pipeline: `render` orchestrates the five vertical chunks
//! (header, main panel, activity, input, footer) and the per-panel
//! sub-renderers it calls. Lives in its own `impl TuiApp` block — pure
//! presentation, no input handling, no state mutation.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::adapters::ExecJobState;

use super::super::style::{
    dim_dot_separator, format_bytes, format_chars, format_elapsed, format_turn_meta,
    job_state_label, job_state_style, one_line, panel_block, plural, push_code_block_line,
    push_code_fence_rule, push_wrapped_message_line, role_style, selected_window_start,
    spinner_frame_at, status_color, status_dot, text_lines,
};
use super::super::ChatRole;
use super::types::TuiPanel;
use super::TuiApp;

/// Build the `ctx [████░░░░] 32% / 128k tok` fill bar shown in the header.
/// Color shifts yellow above 80% and red above 95% to flag context pressure
/// before the user hits a hard limit.
fn context_bar_spans(used: u32, max: u32) -> Vec<Span<'static>> {
    let pct = if max == 0 {
        0.0
    } else {
        (used as f64 / max as f64).clamp(0.0, 1.0)
    };
    const BAR_WIDTH: usize = 8;
    let filled = (pct * BAR_WIDTH as f64).round() as usize;
    let bar_color = if pct >= 0.95 {
        Color::Red
    } else if pct >= 0.80 {
        Color::Yellow
    } else {
        Color::Green
    };
    let max_label = if max >= 1000 {
        format!("{}k", max / 1000)
    } else {
        format!("{max}")
    };
    vec![
        Span::styled("ctx ", Style::default().fg(Color::DarkGray)),
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
        Span::styled(
            "░".repeat(BAR_WIDTH.saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("] ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}%", (pct * 100.0) as u32),
            Style::default()
                .fg(bar_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {max_label} tok"),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}
impl TuiApp {
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // Activity panel uses a single dim top-rule (Borders::TOP) instead
        // of full borders, so it only needs N+1 rows for N visible lines.
        let activity_height = if self.queued_inputs.is_empty() { 3 } else { 5 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(activity_height),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(area);

        // Header reads as `provider · model · store · root`, with the labels
        // dimmed and the values in default fg so the eye lands on what is
        // actually variable. A context fill bar appears at the end when
        // the active model's window size is known.
        let mut header_spans = vec![
            Span::styled("provider ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                self.config.provider.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            dim_dot_separator(),
            Span::styled("model ", Style::default().fg(Color::DarkGray)),
            Span::styled(self.config.model.clone(), Style::default().fg(Color::White)),
            dim_dot_separator(),
            Span::styled("store ", Style::default().fg(Color::DarkGray)),
            Span::raw(self.config.store.clone()),
            dim_dot_separator(),
            Span::styled("root ", Style::default().fg(Color::DarkGray)),
            Span::raw(self.config.root.clone()),
        ];
        if let Some(max) = self.context_window {
            header_spans.push(dim_dot_separator());
            header_spans.extend(context_bar_spans(self.last_prompt_tokens, max));
        }
        let header = Paragraph::new(Line::from(header_spans)).block(panel_block("μAgent"));
        frame.render_widget(header, chunks[0]);

        match self.panel {
            TuiPanel::Chat => self.render_messages(frame, chunks[1]),
            TuiPanel::Jobs => self.render_jobs(frame, chunks[1]),
            TuiPanel::JobDetail => self.render_job_detail(frame, chunks[1]),
        }

        self.render_activity(frame, chunks[2]);

        let input_area = chunks[3];
        let input_block = panel_block("Input");
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

        // Footer help avoids "Up/Down hist" (which lies — the keys also
        // navigate the slash popup and the jobs panel) and mentions
        // Shift+drag so users know how to escape mouse capture and
        // select text natively.
        let help = if self.pastes.is_empty() {
            "Ctrl-B jobs · Enter submit · ↑↓ nav · Tab cmd · Esc cancel · Shift+drag select · Ctrl-C quit"
        } else {
            "Ctrl-B jobs · Enter submit · Backspace paste · Tab cmd · Esc cancel · Ctrl-C quit"
        };
        let mut spans = self.render_status_indicator();
        if !self.sh_jobs.is_empty() {
            spans.push(dim_dot_separator());
            spans.extend(status_dot(self.sh_job_status_text(), Color::Yellow));
        }
        spans.push(dim_dot_separator());
        spans.push(Span::styled(
            help.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        let footer = Paragraph::new(Line::from(spans));
        frame.render_widget(footer, chunks[4]);

        // Slash command popup: rendered last so it overlays the message
        // area and floats just above the input box, like a typeahead.
        self.render_slash_popup(frame, chunks[1], chunks[3]);
    }

    /// Floating popup of REPL command matches, anchored to the bottom-left
    /// of the message area immediately above the input box. Drawn last so
    /// it overlays whatever was below.
    fn render_slash_popup(
        &self,
        frame: &mut Frame<'_>,
        messages_area: ratatui::layout::Rect,
        input_area: ratatui::layout::Rect,
    ) {
        let entries = self.slash_popup_entries();
        if entries.is_empty() {
            return;
        }
        let selected = self.slash_popup_selected.min(entries.len() - 1);

        // Use display width (not byte length) so non-ASCII command names or
        // descriptions still align cleanly.
        use unicode_width::UnicodeWidthStr;
        let max_spec_w = entries
            .iter()
            .map(|(s, _)| UnicodeWidthStr::width(*s))
            .max()
            .unwrap_or(0);
        let max_desc_w = entries
            .iter()
            .map(|(_, d)| UnicodeWidthStr::width(*d))
            .max()
            .unwrap_or(0);
        // Width fits the longest "▎ spec  desc" entry, capped to two-thirds
        // of the terminal so it never crowds the chat completely.
        let max_width = (input_area.width.saturating_mul(2) / 3).max(40);
        // 2 (▎ ) + spec + 2 (gap) + desc, plus 2 borders + 2 padding
        let content_w = (2 + max_spec_w + 2 + max_desc_w).min(max_width as usize - 4);
        let width = (content_w as u16).saturating_add(4).min(input_area.width);

        // Cap visible rows so the popup never overflows above the header
        // (chunks[0] sits at row 0..3, message area starts at row 3).
        let max_panel_height = input_area.y.saturating_sub(messages_area.y);
        let max_visible = entries
            .len()
            .min(8)
            .min(max_panel_height.saturating_sub(2) as usize)
            .max(1);
        let height = (max_visible as u16).saturating_add(2); // borders

        let x = input_area.x.saturating_add(1);
        let y = input_area.y.saturating_sub(height).max(messages_area.y);
        let area = ratatui::layout::Rect { x, y, width, height };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Commands ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));

        let start = selected.saturating_sub(max_visible.saturating_sub(1));
        let mut lines = Vec::new();
        for (idx, (spec, desc)) in entries.iter().enumerate().skip(start).take(max_visible) {
            let is_sel = idx == selected;
            let bar = Span::styled(
                if is_sel { "▎ " } else { "  " },
                Style::default().fg(Color::Cyan),
            );
            let spec_style = if is_sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            // Pad spec column to max display width + 2-cell gap.
            let pad = max_spec_w
                .saturating_sub(UnicodeWidthStr::width(*spec))
                .saturating_add(2);
            lines.push(Line::from(vec![
                bar,
                Span::styled((*spec).to_string(), spec_style),
                Span::raw(" ".repeat(pad)),
                Span::styled((*desc).to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        // Clear background under the popup so it doesn't blend with the
        // message area's text underneath.
        frame.render_widget(ratatui::widgets::Clear, area);
        let popup = Paragraph::new(lines).block(block);
        frame.render_widget(popup, area);
    }

    /// Footer status: animated braille spinner + elapsed/tokens during a
    /// running turn, otherwise a static colored dot.
    fn render_status_indicator(&self) -> Vec<Span<'static>> {
        match self.turn_started {
            Some(started) => {
                let elapsed = started.elapsed();
                // Renamed from `frame` to `glyph` to avoid shadowing the
                // `frame: &mut Frame<'_>` parameter that other render
                // methods take — easier to grep and to read.
                let glyph = spinner_frame_at(elapsed);
                let accent = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                vec![
                    Span::styled(format!("{glyph} "), accent),
                    Span::styled(self.status.clone(), accent),
                    Span::styled(
                        format!(" {}", format_turn_meta(elapsed, self.turn_tokens)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]
            }
            None => status_dot(self.status.clone(), status_color(&self.status)),
        }
    }

    fn render_messages(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        // Pre-compute lines + scroll math up front so the title can flag
        // "↓ N more rows below" when the user has paged back. Without
        // this hint scroll-back feels invisible — the view just sits
        // there with no breadcrumb of how much fresh content arrived.
        let probe_block = panel_block("Messages");
        let inner = probe_block.inner(area);
        let message_lines = self.message_lines(inner.width);
        let visible_rows = inner.height as usize;
        let max_scroll = message_lines.len().saturating_sub(visible_rows) as u16;
        let scroll_back = self.scroll_back.min(max_scroll);
        let scroll = max_scroll.saturating_sub(scroll_back);

        let block = if scroll_back == 0 {
            probe_block
        } else {
            // Rebuild with a richer two-span title; can't mutate the existing
            // probe block's title, but the rest of the chrome matches.
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .padding(Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " Messages ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("· ↓ {scroll_back} rows · End to follow "),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]))
        };

        let messages = Paragraph::new(message_lines)
            .block(block)
            .scroll((scroll, 0));
        frame.render_widget(messages, area);
    }

    fn render_activity(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        // Replace the full box with a single dim top-rule so the activity
        // strip blends into the layout instead of carving out a fourth
        // hard-bordered region. Saves a vertical row and feels less boxy.
        let block = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .padding(Padding::horizontal(1))
            .title(Span::styled(
                " Activity ",
                Style::default().fg(Color::DarkGray),
            ));
        let inner = block.inner(area);
        let inner_rows = inner.height as usize;
        let inner_cols = inner.width as usize;

        let mut lines = Vec::new();

        // Queued submissions surface first — they're the user's pending
        // work and most actionable. Show up to QUEUE_VISIBLE; if there are
        // more queued, replace the third row with a dim "+N more" so the
        // user knows submissions exist beyond what fits.
        const QUEUE_VISIBLE: usize = 3;
        let total_queued = self.queued_inputs.len();
        let shown = total_queued.min(QUEUE_VISIBLE);
        let hidden_count = total_queued.saturating_sub(QUEUE_VISIBLE);
        // If extras exist we shave one visible row to make room for the
        // "+N more" tail so the panel height budget stays the same.
        let visible_queue = if hidden_count > 0 { shown - 1 } else { shown };
        for (idx, queued) in self.queued_inputs.iter().take(visible_queue).enumerate() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("next {}: ", idx + 1),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                // Single-line: long queued prompts get truncated so they
                // don't wrap and steal rows from other entries.
                Span::raw(one_line(queued, inner_cols.saturating_sub(8).max(8))),
            ]));
        }
        if hidden_count > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "+{} more queued",
                    total_queued.saturating_sub(visible_queue)
                ),
                Style::default().fg(Color::Blue),
            )));
        }

        let remaining = inner_rows.saturating_sub(lines.len());
        if remaining > 0 {
            let start = self.activity.len().saturating_sub(remaining);
            let recent = &self.activity[start..];
            if recent.is_empty() && self.queued_inputs.is_empty() {
                lines.push(Line::from(Span::styled(
                    "ready · /help commands · /doctor setup check",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for entry in recent {
                    // Each activity row stays exactly one line; long
                    // entries get truncated rather than wrapping and
                    // pushing earlier rows off-screen.
                    lines.push(Line::from(Span::styled(
                        one_line(entry, inner_cols.max(8)),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        // No wrap: each row is already one line by construction, and Wrap
        // would re-introduce the row-stealing behaviour we just fixed.
        let activity = Paragraph::new(lines).block(block);
        frame.render_widget(activity, area);
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
                let base = job_state_style(&job.state);
                let style = if selected {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                let marker = if selected { "▎ " } else { "  " };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{}{}  {}  out={} err={}  {}",
                        marker,
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

        // Build a single title row with the panel name in cyan and the live
        // job stats / key hints in dim grey so they don't compete.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .padding(Padding::horizontal(1))
            .title(Line::from(vec![
                Span::styled(
                    " Background sh jobs ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "· {} · Enter detail · Esc close ",
                        self.sh_job_status_text()
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        let jobs = Paragraph::new(lines)
            .block(block)
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

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .padding(Padding::horizontal(1))
            .title(Line::from(vec![
                Span::styled(
                    " sh job detail ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "· Esc list · Ctrl-B close ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        let detail = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.job_detail_scroll, 0));
        frame.render_widget(detail, area);
    }

    pub(super) fn sh_job_status_text(&self) -> String {
        let running = self
            .sh_jobs
            .iter()
            .filter(|job| job.state == ExecJobState::Running)
            .count();
        format!("sh bg: {running}/{}", self.sh_jobs.len())
    }

    pub(super) fn paste_summary_line(&self) -> Line<'static> {
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

    pub(super) fn message_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.messages.is_empty() {
            return vec![Line::from(Span::styled(
                "Type a message or /help.",
                Style::default().fg(Color::DarkGray),
            ))];
        }

        let width = width.max(4) as usize;
        let mut lines = Vec::new();
        let mut prev_role: Option<&ChatRole> = None;

        for (i, message) in self.messages.iter().enumerate() {
            // A tool row that follows an Assistant or another Tool drops its
            // own ▎ bar and indents instead — visually nesting under the
            // preceding assistant turn so a flurry of tool calls reads as
            // one cluster instead of N rainbow stripes.
            let in_cluster = matches!(message.role, ChatRole::Tool)
                && matches!(prev_role, Some(ChatRole::Assistant) | Some(ChatRole::Tool));

            let (label, style) = if in_cluster {
                ("  ", Style::default().fg(Color::Magenta))
            } else {
                role_style(&message.role)
            };

            // Track ``` fences within this message so code blocks render
            // with a dim │ rule and a fence horizontal rule, separating
            // code visually from prose without doing full Markdown.
            let mut in_code = false;
            for raw in message.text.lines() {
                let trimmed = raw.trim_start();
                if trimmed.starts_with("```") {
                    let lang = if in_code {
                        ""
                    } else {
                        trimmed.trim_start_matches('`').trim()
                    };
                    push_code_fence_rule(&mut lines, label, style, lang, width);
                    in_code = !in_code;
                    continue;
                }
                if in_code {
                    push_code_block_line(&mut lines, label, style, raw, width);
                } else {
                    push_wrapped_message_line(&mut lines, label, style, raw, width);
                }
            }

            // Skip the trailing blank line when the next message also lives
            // in this cluster (current Assistant/Tool followed by a Tool).
            let next_extends_cluster = self.messages.get(i + 1).is_some_and(|next| {
                matches!(next.role, ChatRole::Tool)
                    && matches!(message.role, ChatRole::Assistant | ChatRole::Tool)
            });
            if !next_extends_cluster {
                lines.push(Line::raw(""));
            }

            prev_role = Some(&message.role);
        }
        lines
    }
}
