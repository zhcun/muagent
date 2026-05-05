//! List, pick, and resume persisted sessions. Includes the interactive
//! arrow-key picker (TUI feature) and a stdin fallback.

use std::io::Write;

#[cfg(feature = "tui")]
use crossterm::{
    cursor,
    event::{self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self as ct_terminal, ClearType},
};

use crate::cli_app::state::{ensure_workspace_root, resume_session_state, workspace_root};
#[cfg(feature = "tui")]
use crate::cli_app::stdio_is_tty;
use crate::cli_app::{now_ms, truncate};
use crate::config::Config;
use crate::core::clock::SystemClock;
use crate::core::run_state::RunState;
use crate::core::store::RunStatus;
use crate::sessions::manager::SessionInfo;
use crate::setup;

pub async fn sessions_for_display(
    wired: &setup::Wired,
    cfg: &Config,
    all: bool,
    limit: Option<usize>,
) -> Result<Vec<SessionInfo>, String> {
    if all {
        wired
            .sessions
            .list_sessions(limit)
            .await
            .map_err(|e| format!("list sessions failed: {e}"))
    } else {
        wired
            .sessions
            .list_sessions_for_workspace(&workspace_root(cfg), limit)
            .await
            .map_err(|e| format!("list sessions failed: {e}"))
    }
}

pub async fn print_sessions(wired: &setup::Wired, cfg: &Config, all: bool) -> Result<(), String> {
    let sessions = sessions_for_display(wired, cfg, all, None).await?;
    if sessions.is_empty() {
        if all {
            println!("No persisted sessions.");
        } else {
            println!(
                "No persisted sessions for {}. Use `muagent sessions --all` to show every workspace.",
                workspace_root(cfg)
            );
        }
        return Ok(());
    }
    print_session_list(&sessions, all);
    Ok(())
}

pub async fn pick_session_state(
    wired: &setup::Wired,
    cfg: &Config,
    all: bool,
    clock: &SystemClock,
) -> Result<RunState, String> {
    let sessions = sessions_for_display(wired, cfg, all, Some(50)).await?;
    if sessions.is_empty() {
        return if all {
            Err("no persisted sessions found".into())
        } else {
            Err(format!(
                "no persisted sessions for {}; use `muagent resume --all` to choose from every workspace",
                workspace_root(cfg)
            ))
        };
    }
    let selected = select_session_index(&sessions, all)?;
    let Some(session) = sessions.get(selected) else {
        return Err(format!("selection out of range: {}", selected + 1));
    };
    let mut state = resume_session_state(wired, session.session_id, clock).await?;
    ensure_workspace_root(&mut state, cfg);
    Ok(state)
}

pub fn print_session_list(sessions: &[SessionInfo], show_workspace: bool) {
    for (idx, s) in sessions.iter().enumerate() {
        let line = session_summary_line(idx, s, false);
        println!("{line}");
        if show_workspace {
            println!(
                "      workspace {}",
                compact_session_path(s.workspace_root.as_deref().unwrap_or("(unknown)"), 96)
            );
        }
    }
}

pub fn select_session_index(
    sessions: &[SessionInfo],
    show_workspace: bool,
) -> Result<usize, String> {
    #[cfg(feature = "tui")]
    if stdio_is_tty() {
        return interactive_session_picker(sessions, show_workspace);
    }
    prompt_session_index(sessions, show_workspace)
}

fn prompt_session_index(sessions: &[SessionInfo], show_workspace: bool) -> Result<usize, String> {
    print_session_list(sessions, show_workspace);
    print!("Select session [1-{}] (q to cancel): ", sessions.len());
    let _ = std::io::stdout().flush();
    let mut raw = String::new();
    std::io::stdin()
        .read_line(&mut raw)
        .map_err(|e| format!("stdin: {e}"))?;
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("q") || trimmed.is_empty() {
        return Err("resume cancelled".into());
    }
    let idx = trimmed
        .parse::<usize>()
        .map_err(|_| format!("invalid selection `{trimmed}`"))?;
    if idx == 0 || idx > sessions.len() {
        return Err(format!("selection out of range: {idx}"));
    }
    Ok(idx - 1)
}

#[cfg(feature = "tui")]
fn interactive_session_picker(
    sessions: &[SessionInfo],
    show_workspace: bool,
) -> Result<usize, String> {
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = ct_terminal::disable_raw_mode();
        }
    }

    ct_terminal::enable_raw_mode().map_err(|e| format!("resume picker raw mode: {e}"))?;
    let _guard = RawModeGuard;
    let mut selected = 0usize;
    let mut drawn_lines = 0usize;

    loop {
        drawn_lines = render_session_picker(sessions, show_workspace, selected, drawn_lines)
            .map_err(|e| format!("resume picker render: {e}"))?;
        let event = ct_event::read().map_err(|e| format!("resume picker input: {e}"))?;
        let CtEvent::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            CtKeyCode::Up | CtKeyCode::Char('k') => selected = selected.saturating_sub(1),
            CtKeyCode::Down | CtKeyCode::Char('j') => {
                selected = (selected + 1).min(sessions.len().saturating_sub(1));
            }
            CtKeyCode::PageUp => selected = selected.saturating_sub(10),
            CtKeyCode::PageDown => {
                selected = (selected + 10).min(sessions.len().saturating_sub(1));
            }
            CtKeyCode::Home => selected = 0,
            CtKeyCode::End => selected = sessions.len().saturating_sub(1),
            CtKeyCode::Enter => {
                println!();
                return Ok(selected);
            }
            CtKeyCode::Char('q') | CtKeyCode::Esc => {
                println!();
                return Err("resume cancelled".into());
            }
            CtKeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                println!();
                return Err("resume cancelled".into());
            }
            _ => {}
        }
    }
}

#[cfg(feature = "tui")]
fn render_session_picker(
    sessions: &[SessionInfo],
    show_workspace: bool,
    selected: usize,
    previous_lines: usize,
) -> std::io::Result<usize> {
    let mut stdout = std::io::stdout();
    for _ in 0..previous_lines {
        execute!(stdout, cursor::MoveUp(1))?;
    }
    execute!(
        stdout,
        cursor::MoveToColumn(0),
        ct_terminal::Clear(ClearType::FromCursorDown)
    )?;

    let mut lines = 0usize;
    let (_, height) = ct_terminal::size().unwrap_or((80, 24));
    let rows_available = (height as usize).saturating_sub(3).max(4);
    let item_rows = if show_workspace { 2 } else { 1 };
    let visible_items = (rows_available / item_rows)
        .clamp(3, 20)
        .min(sessions.len());
    let start = picker_window_start(selected, visible_items, sessions.len());
    let end = (start + visible_items).min(sessions.len());

    println!(
        "Resume session · ↑/↓ or j/k · PgUp/PgDn · Enter resume · q/Esc cancel · {}-{} of {}",
        start + 1,
        end,
        sessions.len()
    );
    lines += 1;
    for (idx, session) in sessions.iter().enumerate().skip(start).take(visible_items) {
        println!("{}", session_summary_line(idx, session, idx == selected));
        lines += 1;
        if show_workspace {
            println!(
                "      workspace {}",
                compact_session_path(session.workspace_root.as_deref().unwrap_or("(unknown)"), 96)
            );
            lines += 1;
        }
    }
    stdout.flush()?;
    Ok(lines)
}

pub fn session_summary_line(idx: usize, session: &SessionInfo, selected: bool) -> String {
    let marker = if selected { "▎" } else { " " };
    let title = session
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("(no user prompt)");
    let run_label = if session.run_count == 1 {
        "run"
    } else {
        "runs"
    };
    format!(
        "{marker} {:>2}  {:<64}  {} · {} · {} {} · id {}",
        idx + 1,
        truncate(title, 64),
        session_time_label(session.updated_ms),
        status_label(session.latest_status),
        session.run_count,
        run_label,
        session.session_id
    )
}

fn picker_window_start(selected: usize, visible: usize, len: usize) -> usize {
    if len == 0 || visible == 0 {
        return 0;
    }
    let max_start = len.saturating_sub(visible);
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(max_start)
}

fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Active => "active",
        RunStatus::Paused => "paused",
        RunStatus::Done => "done",
        RunStatus::Failed => "failed",
    }
}

fn session_time_label(updated_ms: i64) -> String {
    let rel = relative_time_label(updated_ms, now_ms());
    let abs = local_time_label(updated_ms);
    format!("{rel} {abs}")
}

fn relative_time_label(updated_ms: i64, now_ms: i64) -> String {
    let delta = now_ms.saturating_sub(updated_ms);
    let secs = delta / 1000;
    if secs < 10 {
        "刚刚".into()
    } else if secs < 60 {
        format!("{secs}秒前")
    } else if secs < 60 * 60 {
        format!("{}分钟前", secs / 60)
    } else if secs < 60 * 60 * 24 {
        format!("{}小时前", secs / 3600)
    } else if secs < 60 * 60 * 24 * 7 {
        format!("{}天前", secs / (60 * 60 * 24))
    } else {
        "较早".into()
    }
}

fn local_time_label(ms: i64) -> String {
    #[cfg(unix)]
    {
        let secs = (ms / 1000) as libc::time_t;
        let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
        let ptr = unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) };
        if !ptr.is_null() {
            let tm = unsafe { tm.assume_init() };
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            );
        }
    }
    ms.to_string()
}

fn compact_session_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return truncate(path, max_chars);
    }
    let mut suffix = String::new();
    for part in parts.iter().rev() {
        let candidate = if suffix.is_empty() {
            (*part).to_string()
        } else {
            format!("{part}/{suffix}")
        };
        if candidate.chars().count() + 4 > max_chars {
            break;
        }
        suffix = candidate;
    }
    if suffix.is_empty() {
        truncate(path, max_chars)
    } else {
        format!(".../{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{compact_session_path, picker_window_start, session_summary_line};
    use crate::core::store::RunStatus;
    use crate::sessions::manager::SessionInfo;

    #[test]
    fn session_summary_prioritizes_title_and_copyable_id() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let session = SessionInfo {
            session_id: id,
            run_count: 2,
            updated_ms: 0,
            latest_status: RunStatus::Done,
            workspace_root: Some("/workspace/project".into()),
            title: Some("Investigate TUI display polish".into()),
        };

        let line = session_summary_line(0, &session, true);
        assert!(line.starts_with("▎  1"), "{line}");
        assert!(line.contains("Investigate TUI display polish"), "{line}");
        assert!(line.contains("done"), "{line}");
        assert!(line.contains("2 runs"), "{line}");
        assert!(
            line.contains("id 11111111-2222-3333-4444-555555555555"),
            "{line}"
        );
        assert!(!line.contains("runs="), "{line}");
    }

    #[test]
    fn picker_window_tracks_selection_near_bottom() {
        assert_eq!(picker_window_start(0, 5, 20), 0);
        assert_eq!(picker_window_start(4, 5, 20), 0);
        assert_eq!(picker_window_start(5, 5, 20), 1);
        assert_eq!(picker_window_start(19, 5, 20), 15);
    }

    #[test]
    fn compact_session_path_keeps_tail() {
        assert_eq!(
            compact_session_path("/very/long/path/to/a/workspace/project/frontend", 28),
            ".../project/frontend"
        );
    }
}
