//! Rendering pipeline: `render` orchestrates the five vertical chunks
//! (header, main panel, activity, input, footer) and the per-panel
//! sub-renderers it calls. Lives in its own `impl TuiApp` block — pure
//! presentation, no input handling, no state mutation.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::adapters::ExecJobState;

use super::super::ChatRole;
use super::super::style::{
    dim_dot_separator, format_bytes, format_chars, format_elapsed, format_turn_meta,
    job_state_label, job_state_style, one_line, plural, push_code_block_line, push_code_fence_rule,
    push_wrapped_message_line, role_body_style, role_style, selected_window_start,
    spinner_frame_at, text_lines,
};
use super::TuiApp;
use super::types::TuiPanel;

/// Build the `[████░░░░] 32% / 128k` fill bar shown in the footer.
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
    let used_label = compact_token_label(used);
    let max_label = compact_token_label(max);
    vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
        Span::styled(
            "░".repeat(BAR_WIDTH.saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("] ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{used_label}/{max_label}"),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}%", (pct * 100.0) as u32),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ),
    ]
}

fn compact_token_label(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        if tokens % 1_000_000 == 0 {
            format!("{}m", tokens / 1_000_000)
        } else {
            format!("{:.1}m", tokens as f64 / 1_000_000.0)
        }
    } else if tokens >= 1000 {
        if tokens % 1000 == 0 {
            format!("{}k", tokens / 1000)
        } else {
            format!("{:.1}k", tokens as f64 / 1000.0)
        }
    } else {
        tokens.to_string()
    }
}

fn composer_block(disabled: bool) -> Block<'static> {
    let border_style = if disabled {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
}

fn disabled_input_style(
    input: &tui_textarea::TextArea<'static>,
) -> tui_textarea::TextArea<'static> {
    let mut input = input.clone();
    input.set_style(Style::default().fg(Color::DarkGray));
    input.set_cursor_style(Style::default().fg(Color::DarkGray));
    input.set_placeholder_style(Style::default().fg(Color::DarkGray));
    input
}

fn paste_summary_style(disabled: bool) -> Style {
    if disabled {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
}

impl TuiApp {
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // Activity is a quiet status strip. Keep it nearly invisible when
        // idle so the transcript and input remain the primary surfaces.
        let activity_rows =
            self.queue_activity_rows() + usize::from(self.should_show_activity_row());
        let activity_height = if activity_rows == 0 {
            1
        } else {
            (activity_rows + 1).min(5) as u16
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(4),
                Constraint::Length(activity_height),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(area);

        let header = Paragraph::new(self.header_line(chunks[0].width));
        frame.render_widget(header, chunks[0]);

        match self.panel {
            TuiPanel::Chat => self.render_messages(frame, chunks[1]),
            TuiPanel::Jobs => self.render_jobs(frame, chunks[1]),
            TuiPanel::JobDetail => self.render_job_detail(frame, chunks[1]),
        }

        self.render_activity(frame, chunks[2]);

        let input_area = chunks[3];
        let submit_blocked_by_queue =
            matches!(self.panel, TuiPanel::Chat) && self.is_submit_blocked_by_queue();
        let input_block = composer_block(submit_blocked_by_queue);
        let input_inner = input_block.inner(input_area);
        frame.render_widget(input_block, input_area);
        let disabled_input;
        let input = if submit_blocked_by_queue {
            disabled_input = disabled_input_style(&self.input);
            &disabled_input
        } else {
            &self.input
        };
        if self.pastes.is_empty() {
            frame.render_widget(input, input_inner);
        } else {
            let input_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(input_inner);
            frame.render_widget(
                Paragraph::new(
                    self.paste_summary_line_with_style(paste_summary_style(
                        submit_blocked_by_queue,
                    )),
                ),
                input_chunks[0],
            );
            frame.render_widget(input, input_chunks[1]);
        }

        // Mouse capture is intentionally left off so terminal selection/copy
        // stays native.
        let help = self.footer_help_text();
        let mut spans = self.footer_context_spans();
        if self.running_sh_job_count() > 0 {
            if !spans.is_empty() {
                spans.push(dim_dot_separator());
            }
            spans.push(Span::styled(
                self.sh_job_status_text(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.queue_len > 0 {
            if !spans.is_empty() {
                spans.push(dim_dot_separator());
            }
            spans.extend(self.queue_footer_spans());
        }
        if !spans.is_empty() {
            spans.push(dim_dot_separator());
        }
        let help_style = if submit_blocked_by_queue {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(help.to_string(), help_style));
        let footer = Paragraph::new(Line::from(spans));
        frame.render_widget(footer, chunks[4]);

        // Slash command popup: rendered last so it overlays the message
        // area and floats just above the input box, like a typeahead.
        self.render_slash_popup(frame, chunks[1], chunks[3]);
    }

    fn should_show_activity_row(&self) -> bool {
        self.status != "idle"
    }

    fn queue_activity_rows(&self) -> usize {
        self.queued_inputs.len().min(self.queue_limit.max(1)).min(3)
    }

    fn header_line(&self, width: u16) -> Line<'static> {
        let width = width as usize;
        let show_provider = width >= 72;
        let provider_width = if show_provider {
            UnicodeWidthStr::width(self.config.provider.as_str()) + 3
        } else {
            0
        };
        let effort_width = UnicodeWidthStr::width(self.config.effort.as_str());
        let model_budget = if width >= 120 {
            42
        } else if width >= 90 {
            30
        } else {
            20
        };
        let fixed_width = UnicodeWidthStr::width("μAgent") + effort_width + provider_width + 9;
        let workspace_budget = width
            .saturating_sub(fixed_width)
            .saturating_sub(model_budget)
            .clamp(8, 44);

        let mut left = vec![
            Span::styled(
                "μAgent",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            dim_dot_separator(),
            Span::styled(
                compact_workspace_label(&self.config.root, workspace_budget),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            dim_dot_separator(),
            Span::styled(
                one_line(&self.config.model, model_budget),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            dim_dot_separator(),
            Span::styled(
                self.config.effort.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if show_provider {
            left.push(dim_dot_separator());
            left.push(Span::styled(
                self.config.provider.clone(),
                Style::default().fg(Color::Blue),
            ));
        }

        Line::from(left)
    }

    fn footer_help_text(&self) -> &'static str {
        if self.status == "setup" {
            return "Esc/q quit · Ctrl-C quit";
        }

        match self.panel {
            TuiPanel::Jobs if self.sh_jobs.is_empty() => "Esc back · Ctrl-B chat · Ctrl-C quit",
            TuiPanel::Jobs => "↑↓ · Enter · Esc back · Ctrl-C quit",
            TuiPanel::JobDetail => "↑↓ · PgUp/PgDn · Esc back · Ctrl-C quit",
            TuiPanel::Chat if self.status == "cancelling" && self.scroll_back > 0 => {
                "Enter locked · End bottom · Ctrl-C quit"
            }
            TuiPanel::Chat if self.status == "cancelling" => {
                "Enter locked · draft kept · Ctrl-C quit"
            }
            TuiPanel::Chat if self.is_submit_locked() && self.scroll_back > 0 => {
                "Enter locked · End bottom · Esc stop · Ctrl-C quit"
            }
            TuiPanel::Chat if self.is_submit_locked() => "Enter locked · draft kept · Ctrl-C quit",
            TuiPanel::Chat
                if matches!(self.status.as_str(), "running" | "cancelling")
                    && self.scroll_back > 0 =>
            {
                "End bottom · Enter queue · Esc stop · Ctrl-C quit"
            }
            TuiPanel::Chat if self.scroll_back > 0 => "PgUp/PgDn · End bottom · Ctrl-C quit",
            TuiPanel::Chat if matches!(self.status.as_str(), "running" | "cancelling") => {
                "Enter queue · Esc stop · Ctrl-C quit"
            }
            TuiPanel::Chat if self.pastes.is_empty() => {
                "Enter · ↑↓ history · Tab · Ctrl-B jobs · Ctrl-C quit"
            }
            TuiPanel::Chat => "Enter · Backspace paste · Ctrl-B jobs · Ctrl-C quit",
        }
    }

    fn footer_context_spans(&self) -> Vec<Span<'static>> {
        match self.context_window {
            Some(max) => context_bar_spans(self.last_prompt_tokens, max),
            None => Vec::new(),
        }
    }

    fn queue_footer_spans(&self) -> Vec<Span<'static>> {
        let color = if self.is_submit_blocked_by_queue() {
            Color::Yellow
        } else {
            Color::Blue
        };
        vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                format!("{}/{} queued", self.queue_len, self.queue_limit.max(1)),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]
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
        // 2 (▎ ) + spec + 2 (gap) + desc. The popup is intentionally
        // borderless/titleless so it reads like command suggestions, not a
        // modal panel.
        let content_w = (2 + max_spec_w + 2 + max_desc_w).min(max_width as usize);
        let width = (content_w as u16).min(input_area.width);

        // Cap visible rows so the popup never overflows above the header
        // (chunks[0] sits at row 0..3, message area starts at row 3).
        let max_panel_height = input_area.y.saturating_sub(messages_area.y);
        let max_visible = entries.len().min(8).min(max_panel_height as usize).max(1);
        let height = max_visible as u16;

        let x = input_area.x.saturating_add(1);
        let y = input_area.y.saturating_sub(height).max(messages_area.y);
        let area = ratatui::layout::Rect {
            x,
            y,
            width,
            height,
        };

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
        let popup = Paragraph::new(lines);
        frame.render_widget(popup, area);
    }

    fn render_messages(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        // Pre-compute lines + scroll math up front so the title can flag
        // "↓ N more rows below" when the user has paged back. Without
        // this hint scroll-back feels invisible — the view just sits
        // there with no breadcrumb of how much fresh content arrived.
        let message_lines = self.message_lines(area.width);
        let visible_rows = area.height as usize;
        let max_scroll = message_lines.len().saturating_sub(visible_rows);
        let scroll_back = self.scroll_back.min(max_scroll);
        let first_visible = max_scroll.saturating_sub(scroll_back);

        let visible_lines: Vec<Line<'static>> = if scroll_back == 0 {
            message_lines
                .into_iter()
                .skip(first_visible)
                .take(visible_rows)
                .collect()
        } else {
            let mut lines = vec![Line::from(Span::styled(
                format!(" ↓ {scroll_back} rows · End to follow "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))];
            lines.extend(
                message_lines
                    .into_iter()
                    .skip(first_visible)
                    .take(visible_rows.saturating_sub(1)),
            );
            lines
        };
        // Build the viewport ourselves instead of relying on Paragraph's u16
        // scroll offset. Long sessions can exceed 65k rendered rows.
        let messages = Paragraph::new(visible_lines);
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
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        let inner_rows = inner.height as usize;
        let inner_cols = inner.width as usize;

        let mut lines = Vec::new();

        // Queued submissions surface first. Each queue entry owns exactly
        // one row; the first row is the next submission that will run after
        // the current turn finishes.
        let total_queued = self.queued_inputs.len();
        let reserved_activity = usize::from(self.should_show_activity_row());
        let max_queue_rows = inner_rows
            .saturating_sub(reserved_activity)
            .min(self.queue_limit.max(1))
            .min(3);
        let hidden_count = total_queued.saturating_sub(max_queue_rows);
        let visible_queue = if hidden_count > 0 {
            max_queue_rows.saturating_sub(1)
        } else {
            max_queue_rows
        }
        .min(total_queued);
        for (idx, queued) in self.queued_inputs.iter().take(visible_queue).enumerate() {
            lines.push(self.queue_line(idx, queued, inner_cols));
        }
        if hidden_count > 0 {
            lines.push(Line::from(Span::styled(
                format!("+{} queued", hidden_count),
                Style::default().fg(Color::Blue),
            )));
        }

        let remaining = inner_rows.saturating_sub(lines.len());
        if remaining > 0 {
            let before_activity = lines.len();
            let visible_activity = self
                .activity
                .iter()
                .filter(|entry| !is_low_signal_activity(entry))
                .collect::<Vec<_>>();
            let start = visible_activity.len().saturating_sub(remaining);
            for entry in visible_activity.into_iter().skip(start) {
                // Each activity row stays exactly one line; long entries get
                // truncated rather than wrapping and pushing earlier rows
                // off-screen.
                lines.push(self.activity_line(entry, inner_cols.max(8)));
            }
            if lines.len() == before_activity && self.status != "idle" {
                lines.push(self.activity_line(self.activity_fallback_entry(), inner_cols.max(8)));
            }
        }

        // No wrap: each row is already one line by construction, and Wrap
        // would re-introduce the row-stealing behaviour we just fixed.
        let activity = Paragraph::new(lines).block(block);
        frame.render_widget(activity, area);
    }

    fn queue_line(&self, idx: usize, queued: &str, width: usize) -> Line<'static> {
        let marker_style = if idx == 0 {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Blue)
        };
        let text_style = if idx == 0 {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = format!("{} ", idx + 1);
        let text_width = width.saturating_sub(marker.chars().count()).max(8);
        Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(one_line(queued, text_width), text_style),
        ])
    }

    fn activity_line(&self, entry: &str, width: usize) -> Line<'static> {
        let entry = entry.trim();
        let (text, color, bold) =
            if matches!(entry, "thinking" | "using tools" | "checking tool result") {
                let label =
                    activity_footer_label(entry).unwrap_or_else(|| title_case_status(entry));
                let glyph = self
                    .turn_started
                    .map(|started| spinner_frame_at(started.elapsed()))
                    .unwrap_or("⠋");
                let meta = self
                    .turn_started
                    .map(|started| {
                        format!(" {}", format_turn_meta(started.elapsed(), self.turn_tokens))
                    })
                    .unwrap_or_default();
                (format!("{glyph} {label}{meta}"), Color::Yellow, true)
            } else if let Some(label) = entry.strip_prefix("Running ") {
                let glyph = self
                    .turn_started
                    .map(|started| spinner_frame_at(started.elapsed()))
                    .unwrap_or("⠋");
                let meta = self
                    .turn_started
                    .map(|started| {
                        format!(" {}", format_turn_meta(started.elapsed(), self.turn_tokens))
                    })
                    .unwrap_or_default();
                (format!("{glyph} {label}{meta}"), Color::Yellow, true)
            } else if let Some(label) = entry.strip_prefix("Tool failed: ") {
                (format!("failed · {label}"), Color::Red, true)
            } else if entry.starts_with("error:") {
                (entry.to_string(), Color::Red, true)
            } else if entry == "stopped" || entry == "stopping current task" {
                let glyph = self
                    .turn_started
                    .map(|started| spinner_frame_at(started.elapsed()))
                    .unwrap_or("⠋");
                let meta = self
                    .turn_started
                    .map(|started| {
                        format!(" {}", format_turn_meta(started.elapsed(), self.turn_tokens))
                    })
                    .unwrap_or_default();
                (format!("{glyph} Stopping{meta}"), Color::Red, true)
            } else if entry.starts_with("paused:") {
                (title_case_status(entry), Color::Yellow, true)
            } else if let Some(label) = activity_footer_label(entry) {
                (label, Color::DarkGray, false)
            } else {
                (entry.to_string(), Color::DarkGray, false)
            };
        let style = if bold {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        Line::from(Span::styled(one_line(&text, width), style))
    }

    fn activity_fallback_entry(&self) -> &'static str {
        match self.status.as_str() {
            "cancelling" => "stopping current task",
            "setup" => "setup",
            _ => "thinking",
        }
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
                        "{}{} · {} · out {} · err {} · {}",
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
                    " sh jobs ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· {} ", self.sh_job_status_text()),
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
            Line::from(vec![
                Span::styled(job.job_id.clone(), Style::default().fg(Color::Cyan)),
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    job_state_label(&job.state, job.code),
                    job_state_style(&job.state),
                ),
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_elapsed(job.elapsed_ms),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::raw(format!("$ {}", job.command)),
            Line::raw(format!(
                "out {} · err {} · {}",
                format_bytes(job.stdout_bytes),
                format_bytes(job.stderr_bytes),
                if job.output_truncated {
                    "truncated"
                } else {
                    "complete"
                }
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
            .title(Line::from(vec![Span::styled(
                " sh job ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]));
        let detail = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.job_detail_scroll, 0));
        frame.render_widget(detail, area);
    }

    pub(super) fn sh_job_status_text(&self) -> String {
        format!("sh {}", self.running_sh_job_count())
    }

    pub(super) fn running_sh_job_count(&self) -> usize {
        self.sh_jobs
            .iter()
            .filter(|job| job.state == ExecJobState::Running)
            .count()
    }

    fn paste_summary_line_with_style(&self, style: Style) -> Line<'static> {
        Line::from(Span::styled(self.paste_summary_text(), style))
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
            let running_tool = self
                .running_tools
                .iter()
                .any(|(_, message_idx)| *message_idx == i);

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
                let body_style = role_body_style(&message.role);
                if in_code {
                    push_code_block_line(&mut lines, label, style, raw, width);
                } else if running_tool {
                    push_running_tool_line(
                        &mut lines,
                        label,
                        style,
                        raw,
                        width,
                        self.running_tool_sweep_offset(),
                    );
                } else {
                    push_wrapped_message_line(&mut lines, label, style, body_style, raw, width);
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

    fn running_tool_sweep_offset(&self) -> usize {
        self.turn_started
            .map(|started| (started.elapsed().as_millis() / 80) as usize)
            .unwrap_or(0)
    }
}

fn push_running_tool_line(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    label_style: Style,
    raw: &str,
    width: usize,
    offset: usize,
) {
    let content_width = width.saturating_sub(UnicodeWidthStr::width(label)).max(1);
    let text = one_line(raw, content_width);
    let chars = text.chars().collect::<Vec<_>>();
    let len = chars.len().max(1);
    let head = offset % (len + 8);
    let mut spans = vec![Span::styled(label.to_string(), label_style)];
    for (idx, ch) in chars.into_iter().enumerate() {
        let distance = head.saturating_sub(idx);
        let style = if head < len && idx == head {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else if head < len && distance <= 3 && idx <= head {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    lines.push(Line::from(spans));
}

fn activity_footer_label(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if is_low_signal_activity(entry) {
        return None;
    }
    let label = match entry {
        other if other.starts_with("Queued ") => return None,
        "preparing" => "Preparing".into(),
        "thinking" => "Thinking".into(),
        "using tools" => "Using tools".into(),
        "checking tool result" => "Checking result".into(),
        "stopping current task" | "stopped" => "Stopping".into(),
        "done" | "turn finished" => "Wrapping up".into(),
        other if other.starts_with("Running ") => {
            other.strip_prefix("Running ").unwrap_or(other).to_string()
        }
        other if other.starts_with("Tool failed: ") => other
            .strip_prefix("Tool failed: ")
            .map(|label| format!("Failed {label}"))
            .unwrap_or_else(|| other.to_string()),
        other if other.starts_with("Tool ") => return None,
        other if other.starts_with("assistant:") => "Receiving reply".into(),
        other => title_case_status(other),
    };
    Some(one_line(&label, 48))
}

fn is_low_signal_activity(entry: &str) -> bool {
    let entry = entry.trim();
    if entry.starts_with("Tool finished:")
        || entry.starts_with("assistant:")
        || entry.starts_with("Queued ")
        || entry == "session ended: ok"
    {
        return true;
    }
    matches!(
        entry,
        "" | "turn started"
            | "user message submitted"
            | "submitted"
            | "stopped"
            | "done"
            | "turn finished"
            | "config ready"
    )
}

fn title_case_status(status: &str) -> String {
    let mut chars = status.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = first.to_uppercase().collect::<String>();
    out.push_str(chars.as_str());
    out.replace('_', " ")
}

fn compact_workspace_label(root: &str, max_chars: usize) -> String {
    let root = shorten_home(root);
    if root.chars().count() <= max_chars {
        return root;
    }
    compact_root_label(&root, max_chars)
}

fn shorten_home(root: &str) -> String {
    let Some(home) = std::env::var_os("HOME").and_then(|home| home.into_string().ok()) else {
        return root.to_string();
    };
    if home == "/" || !root.starts_with(&home) {
        return root.to_string();
    }
    match root.strip_prefix(&home) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => root.to_string(),
    }
}

fn compact_root_label(root: &str, max_chars: usize) -> String {
    let trimmed = root.trim_end_matches('/');
    let parts = trimmed
        .split('/')
        .filter(|part| !part.is_empty() && *part != "~")
        .collect::<Vec<_>>();
    let leaf = parts.last().copied().unwrap_or(trimmed);
    if leaf.is_empty() {
        return "/".into();
    }
    if parts.len() >= 2 {
        let candidate = format!(".../{}/{}", parts[parts.len() - 2], leaf);
        if candidate.chars().count() <= max_chars {
            return candidate;
        }
    }
    one_line(&format!(".../{leaf}"), max_chars)
}
