//! `muagent` — entry point. Parses argv, wires runtime, dispatches to the
//! appropriate cli_app driver. Everything beyond dispatch lives in
//! `muagent::cli_app::*`.

use std::process::ExitCode;

use muagent::cli;
use muagent::cli_app::driver::run_one_shot;
use muagent::cli_app::print_banner;
use muagent::cli_app::repl::run_repl;
use muagent::cli_app::sessions::{pick_session_state, print_sessions};
use muagent::cli_app::state::{
    ensure_workspace_root, new_run_state, resume_last_state, resume_session_state,
};
#[cfg(feature = "tui")]
use muagent::cli_app::tui_driver::{run_tui, run_tui_setup_error};
use muagent::config::{Config, ConfigOverrides};
use muagent::setup;
use tracing::error;
use uuid::Uuid;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // Parse argv first; may exit for --help/--version/unknown flag.
    let invocation = match cli::parse() {
        cli::Action::Exit(code) => return code,
        cli::Action::Run(invocation) => *invocation,
    };
    cli::init_tracing(
        invocation.config.log.as_deref(),
        invocation.mode.uses_tui_session(),
    );

    match run(invocation).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("fatal: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(invocation: cli::Invocation) -> Result<(), String> {
    let wants_tui = invocation.mode.uses_tui_session();
    let cfg = Config::load(&invocation.config)?;
    let wired = match setup::wire(&cfg).await {
        Ok(wired) => wired,
        Err(e) if wants_tui => return run_tui_setup_error_dispatch(cfg, e).await,
        Err(e) => return Err(e),
    };
    let clock = muagent::core::clock::SystemClock;
    let images = invocation.images;

    match invocation.mode {
        cli::RunMode::Repl => {
            if !images.is_empty() {
                return Err("--image requires a one-shot prompt".into());
            }
            let state = new_run_state(&cfg, &clock);
            print_banner(&cfg);
            run_repl(wired, cfg, invocation.config, state, &clock).await
        }
        cli::RunMode::Tui { prompt } => {
            if !images.is_empty() {
                return Err("--image is not supported in --tui yet; use a one-shot prompt".into());
            }
            let state = new_run_state(&cfg, &clock);
            run_tui_dispatch(wired, cfg, invocation.config, state, &clock, prompt).await
        }
        cli::RunMode::Exec(prompt) => {
            let mut state = new_run_state(&cfg, &clock);
            run_one_shot(&wired.runner, &mut state, &prompt, &images).await
        }
        cli::RunMode::ResumePicker { all } => {
            if !images.is_empty() {
                return Err("--image requires a prompt when resuming".into());
            }
            let state = pick_session_state(&wired, &cfg, all, &clock).await?;
            run_tui_dispatch(wired, cfg, invocation.config, state, &clock, None).await
        }
        cli::RunMode::ResumeLast { prompt, tui } => {
            let mut state = resume_last_state(&wired, &cfg, &clock).await?;
            ensure_workspace_root(&mut state, &cfg);
            dispatch_resumed(
                wired,
                cfg,
                invocation.config,
                state,
                &clock,
                prompt,
                tui,
                &images,
            )
            .await
        }
        cli::RunMode::ResumeSession {
            session_id,
            prompt,
            tui,
        } => {
            let sid =
                Uuid::parse_str(&session_id).map_err(|e| format!("invalid session_id: {e}"))?;
            let mut state = resume_session_state(&wired, sid, &clock).await?;
            ensure_workspace_root(&mut state, &cfg);
            dispatch_resumed(
                wired,
                cfg,
                invocation.config,
                state,
                &clock,
                prompt,
                tui,
                &images,
            )
            .await
        }
        cli::RunMode::ListSessions { all } => {
            if !images.is_empty() {
                return Err("sessions does not accept --image".into());
            }
            print_sessions(&wired, &cfg, all).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_resumed(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    mut state: muagent::core::run_state::RunState,
    clock: &muagent::core::clock::SystemClock,
    prompt: Option<String>,
    tui: bool,
    images: &[String],
) -> Result<(), String> {
    if let Some(prompt) = prompt {
        return run_one_shot(&wired.runner, &mut state, &prompt, images).await;
    }
    if !images.is_empty() {
        return Err("--image requires a prompt when resuming".into());
    }
    if tui {
        run_tui_dispatch(wired, cfg, overrides, state, clock, None).await
    } else {
        print_banner(&cfg);
        println!(
            "(continued session {}; new run {})",
            state.session_id, state.run_id
        );
        run_repl(wired, cfg, overrides, state, clock).await
    }
}

#[cfg(feature = "tui")]
async fn run_tui_dispatch(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    state: muagent::core::run_state::RunState,
    clock: &muagent::core::clock::SystemClock,
    initial_prompt: Option<String>,
) -> Result<(), String> {
    run_tui(wired, cfg, overrides, state, clock, initial_prompt).await
}

#[cfg(not(feature = "tui"))]
async fn run_tui_dispatch(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    mut state: muagent::core::run_state::RunState,
    clock: &muagent::core::clock::SystemClock,
    initial_prompt: Option<String>,
) -> Result<(), String> {
    if let Some(prompt) = initial_prompt.filter(|p| !p.trim().is_empty()) {
        return run_one_shot(&wired.runner, &mut state, &prompt, &[]).await;
    }
    print_banner(&cfg);
    println!("(compiled without TUI support; using line REPL)");
    run_repl(wired, cfg, overrides, state, clock).await
}

#[cfg(feature = "tui")]
async fn run_tui_setup_error_dispatch(cfg: Config, error: String) -> Result<(), String> {
    run_tui_setup_error(cfg, error).await
}

#[cfg(not(feature = "tui"))]
async fn run_tui_setup_error_dispatch(_cfg: Config, error: String) -> Result<(), String> {
    Err(error)
}
