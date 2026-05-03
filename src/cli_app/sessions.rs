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
use crate::cli_app::{now_ms, short_uuid, truncate};
use crate::config::Config;
use crate::core::clock::SystemClock;
use crate::core::run_state::RunState;
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
                "    root={}",
                s.workspace_root.as_deref().unwrap_or("(unknown)")
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
            CtKeyCode::Up => selected = selected.saturating_sub(1),
            CtKeyCode::Down => {
                selected = (selected + 1).min(sessions.len().saturating_sub(1));
            }
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
    println!(
        "Select session with ↑/↓, Enter to resume, q/Esc to cancel ({} found):",
        sessions.len()
    );
    lines += 1;
    for (idx, session) in sessions.iter().enumerate() {
        println!("{}", session_summary_line(idx, session, idx == selected));
        lines += 1;
        if show_workspace {
            println!(
                "    root={}",
                session.workspace_root.as_deref().unwrap_or("(unknown)")
            );
            lines += 1;
        }
    }
    stdout.flush()?;
    Ok(lines)
}

pub fn session_summary_line(idx: usize, session: &SessionInfo, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let title = session
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("(no user prompt)");
    format!(
        "{marker} {:>2}. {:<18} {:<10} runs={}  {}  [{}]",
        idx + 1,
        session_time_label(session.updated_ms),
        format!("{:?}", session.latest_status),
        session.run_count,
        truncate(title, 90),
        short_uuid(&session.session_id.to_string())
    )
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
