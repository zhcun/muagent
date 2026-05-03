//! `RunState` lifecycle helpers — fresh allocation, workspace tagging, resume.

use uuid::Uuid;

use crate::config::Config;
use crate::core::clock::{Clock, SystemClock};
use crate::core::run_state::RunState;
use crate::setup;

pub fn new_run_state(cfg: &Config, clock: &SystemClock) -> RunState {
    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), clock.now_ms());
    ensure_workspace_root(&mut state, cfg);
    state
}

pub fn ensure_workspace_root(state: &mut RunState, cfg: &Config) {
    if state.workspace_root.is_none() {
        state.workspace_root = Some(workspace_root(cfg));
    }
}

pub fn workspace_root(cfg: &Config) -> String {
    cfg.fs
        .root
        .canonicalize()
        .unwrap_or_else(|_| cfg.fs.root.clone())
        .display()
        .to_string()
}

pub async fn resume_last_state(
    wired: &setup::Wired,
    cfg: &Config,
    clock: &SystemClock,
) -> Result<RunState, String> {
    let sessions = wired
        .sessions
        .list_sessions_for_workspace(&workspace_root(cfg), Some(1))
        .await
        .map_err(|e| format!("list sessions failed: {e}"))?;
    let session = sessions.first().ok_or_else(|| {
        format!(
            "no persisted sessions found for {}; use `muagent resume --all` to choose from every workspace",
            workspace_root(cfg)
        )
    })?;
    resume_session_state(wired, session.session_id, clock).await
}

pub async fn resume_session_state(
    wired: &setup::Wired,
    session_id: Uuid,
    clock: &SystemClock,
) -> Result<RunState, String> {
    wired
        .sessions
        .continue_session(session_id, clock.now_ms())
        .await
        .map_err(|e| format!("continue session failed: {e}"))
}
