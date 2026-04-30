//! `muagent` — one-shot or interactive CLI for the μAgent runtime.
//!
//! Reads layered config (see `config.rs`), spins up a Runner with default
//! tools (fs_read / fs_write / optional sh_exec), skill controls, auto
//! compaction, and a pluggable session store (`memory` by default, optional
//! JSONL on disk).
//!
//! REPL commands (line-prefix `/`):
//!   /help         show commands
//!   /new          start a new run (drop history)
//!   /tokens       show token usage so far
//!   /history      show the last 20 messages (brief)
//!   /model        show/switch model for this REPL session
//!   /provider     show/switch provider/model for this REPL session
//!   /skills       list registered skills
//!   /quit  |  /exit  |  Ctrl-D   exit

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use muagent::cli;
use muagent::config::{Config, ConfigOverrides};
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::types::{Content, ContentPart, Message};
use muagent::setup;
use muagent::tui::{TerminalSession, TuiApp, TuiConfig, UserAction};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const DEFAULT_MAX_STEPS: usize = 10_000;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // Parse argv first; may exit for --help/--version/unknown flag.
    let invocation = match cli::parse() {
        cli::Action::Exit(code) => return code,
        cli::Action::Run(invocation) => *invocation,
    };
    cli::init_tracing(invocation.config.log.as_deref());

    match run(invocation).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("fatal: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(invocation: cli::Invocation) -> Result<(), String> {
    let cfg = Config::load(&invocation.config)?;
    let wired = setup::wire(&cfg).await?;
    let clock = muagent::core::clock::SystemClock;
    let images = invocation.images;

    match invocation.mode {
        cli::RunMode::Repl => {
            if !images.is_empty() {
                return Err("--image requires a one-shot prompt".into());
            }
            let state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), clock.now_ms());
            print_banner(&cfg);
            run_repl(wired, cfg, invocation.config, state, &clock).await
        }
        cli::RunMode::Tui => {
            if !images.is_empty() {
                return Err("--image is not supported in --tui yet; use a one-shot prompt".into());
            }
            let state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), clock.now_ms());
            run_tui(wired, cfg, invocation.config, state, &clock).await
        }
        cli::RunMode::Prompt(prompt) => {
            let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), clock.now_ms());
            run_one_shot(&wired.runner, &mut state, &prompt, &images).await
        }
        cli::RunMode::ResumeLast { prompt } => {
            let mut state = resume_last_state(&wired, &clock).await?;
            if let Some(prompt) = prompt {
                run_one_shot(&wired.runner, &mut state, &prompt, &images).await
            } else {
                if !images.is_empty() {
                    return Err("--image requires a prompt when resuming".into());
                }
                print_banner(&cfg);
                println!(
                    "(continued session {}; new run {})",
                    state.session_id, state.run_id
                );
                run_repl(wired, cfg, invocation.config, state, &clock).await
            }
        }
        cli::RunMode::ResumeSession { session_id, prompt } => {
            let sid =
                Uuid::parse_str(&session_id).map_err(|e| format!("invalid session_id: {e}"))?;
            let mut state = resume_session_state(&wired, sid, &clock).await?;
            if let Some(prompt) = prompt {
                run_one_shot(&wired.runner, &mut state, &prompt, &images).await
            } else {
                if !images.is_empty() {
                    return Err("--image requires a prompt when resuming".into());
                }
                print_banner(&cfg);
                println!(
                    "(continued session {}; new run {})",
                    state.session_id, state.run_id
                );
                run_repl(wired, cfg, invocation.config, state, &clock).await
            }
        }
    }
}

struct ReplRuntime {
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
}

async fn run_repl(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    mut state: RunState,
    clock: &muagent::core::clock::SystemClock,
) -> Result<(), String> {
    let mut runtime = ReplRuntime {
        wired,
        cfg,
        overrides,
    };
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        prompt();
        let line = match stdin.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                println!();
                break;
            } // EOF (Ctrl-D)
            Err(e) => return Err(format!("stdin: {e}")),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('/') {
            match handle_command(trimmed, &mut state, &mut runtime, clock).await {
                CmdAction::Continue => continue,
                CmdAction::Quit => break,
                CmdAction::Reset(new_state) => {
                    state = *new_state;
                }
            }
            continue;
        }

        if let Err(e) = submit_and_drive(&runtime.wired.runner, &mut state, trimmed).await {
            error!("{e}");
            continue;
        }
        print_turn_result(&state);
    }
    Ok(())
}

async fn run_tui(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    mut state: RunState,
    clock: &muagent::core::clock::SystemClock,
) -> Result<(), String> {
    let mut runtime = ReplRuntime {
        wired,
        cfg,
        overrides,
    };
    let mut app = TuiApp::new(tui_config(&runtime.cfg));
    app.add_system("Full-screen TUI is optional. Type /help for commands.");

    let mut terminal = TerminalSession::enter().map_err(|e| format!("tui init: {e}"))?;
    loop {
        terminal.draw(&app).map_err(|e| format!("tui draw: {e}"))?;

        let Some(key) = TerminalSession::read_key(Duration::from_millis(100))
            .map_err(|e| format!("tui input: {e}"))?
        else {
            continue;
        };

        match app.handle_key(key) {
            UserAction::None => {}
            UserAction::Quit => break,
            UserAction::Submit(line) => {
                let trimmed = line.trim();
                if trimmed.starts_with('/') {
                    match handle_tui_command(trimmed, &mut state, &mut runtime, clock, &mut app)
                        .await
                    {
                        CmdAction::Continue => {}
                        CmdAction::Quit => break,
                        CmdAction::Reset(new_state) => {
                            state = *new_state;
                        }
                    }
                    continue;
                }

                app.add_user(trimmed.to_string());
                app.set_status("running");
                terminal.draw(&app).map_err(|e| format!("tui draw: {e}"))?;

                match submit_and_drive(&runtime.wired.runner, &mut state, trimmed).await {
                    Ok(()) => append_tui_turn_result(&mut app, &state),
                    Err(e) => app.add_error(e),
                }
                app.set_status("idle");
            }
        }
    }

    Ok(())
}

async fn run_one_shot(
    runner: &Runner,
    state: &mut RunState,
    prompt: &str,
    images: &[String],
) -> Result<(), String> {
    submit_and_drive_with_content(runner, state, user_content(prompt, images)?).await?;
    match &state.step {
        Step::Done { final_text } => {
            println!("{final_text}");
            Ok(())
        }
        Step::Failed { reason, .. } => Err(format!("run failed: {reason}")),
        Step::Paused { reason } => Err(format!("run paused: {reason:?}")),
        other => Err(format!("run did not finish; final step={other:?}")),
    }
}

async fn submit_and_drive(
    runner: &Runner,
    state: &mut RunState,
    prompt: &str,
) -> Result<(), String> {
    submit_and_drive_with_content(runner, state, Content::text(prompt)).await
}

async fn submit_and_drive_with_content(
    runner: &Runner,
    state: &mut RunState,
    content: Content,
) -> Result<(), String> {
    runner
        .submit_user_message(state, Message::User { content })
        .await
        .map_err(|e| format!("submit_user_message failed: {e:?}"))?;
    drive_until_terminal(runner, state).await
}

fn user_content(prompt: &str, image_paths: &[String]) -> Result<Content, String> {
    if image_paths.is_empty() {
        return Ok(Content::text(prompt));
    }

    let mut parts = Vec::with_capacity(image_paths.len() + 1);
    parts.push(ContentPart::Text {
        text: prompt.to_string(),
    });
    for image in image_paths {
        let path = Path::new(image);
        let bytes = std::fs::read(path).map_err(|e| format!("read image `{image}`: {e}"))?;
        parts.push(ContentPart::Image {
            uri: None,
            b64: Some(base64_encode(&bytes)),
            mime: image_mime(path)?,
        });
    }
    Ok(Content::Parts(parts))
}

fn image_mime(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => Ok("image/png".into()),
        "jpg" | "jpeg" => Ok("image/jpeg".into()),
        "webp" => Ok("image/webp".into()),
        "gif" => Ok("image/gif".into()),
        _ => Err(format!(
            "unsupported image extension for `{}`; supported: png, jpg, jpeg, webp, gif",
            path.display()
        )),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied().unwrap_or(0);
        let b2 = bytes.get(i + 2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;

        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

async fn resume_last_state(
    wired: &setup::Wired,
    clock: &muagent::core::clock::SystemClock,
) -> Result<RunState, String> {
    let sessions = wired
        .sessions
        .list_sessions(Some(1))
        .await
        .map_err(|e| format!("list sessions failed: {e}"))?;
    let session = sessions.first().ok_or_else(|| {
        "no persisted sessions found; start one with `muagent \"your task\"` first".to_string()
    })?;
    resume_session_state(wired, session.session_id, clock).await
}

async fn resume_session_state(
    wired: &setup::Wired,
    session_id: Uuid,
    clock: &muagent::core::clock::SystemClock,
) -> Result<RunState, String> {
    use muagent::core::clock::Clock;
    wired
        .sessions
        .continue_session(session_id, clock.now_ms())
        .await
        .map_err(|e| format!("continue session failed: {e}"))
}

async fn drive_until_terminal(runner: &Runner, state: &mut RunState) -> Result<(), String> {
    let max_steps = std::env::var("MUAGENT_MAX_STEPS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_STEPS);
    let bad_tool_event_limit = std::env::var("MUAGENT_BAD_TOOL_EVENT_LIMIT")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0);
    let mut bad_tool_events = 0usize;
    for _ in 0..max_steps {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            return Ok(());
        }
        match runner.step(state).await {
            Ok(out) => {
                for ev in &out.events {
                    log_event(ev);
                    if is_bad_tool_event(ev) {
                        bad_tool_events += 1;
                    } else if matches!(ev, Event::ToolCallEnd { .. }) {
                        bad_tool_events = 0;
                    }
                    if bad_tool_event_limit > 0 && bad_tool_events >= bad_tool_event_limit {
                        return Err(format!(
                            "tool no-progress guard tripped after {bad_tool_events} timeout/security/error tool events"
                        ));
                    }
                }
            }
            Err(e) => {
                error!(?e, "runner step failed");
                return Err(format!("runner step failed: {e:?}"));
            }
        }
    }
    if matches!(
        state.step,
        Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
    ) {
        return Ok(());
    }
    warn!(
        max = max_steps,
        "hit step budget without reaching terminal state"
    );
    Err(format!(
        "hit step budget without reaching terminal state; step={:?}",
        state.step
    ))
}

fn is_bad_tool_event(ev: &Event) -> bool {
    let Event::ToolCallEnd {
        ok,
        retryable,
        brief,
        ..
    } = ev
    else {
        return false;
    };
    let brief_lc = brief.to_ascii_lowercase();
    (!*ok && *retryable)
        || brief_lc.contains("timeout")
        || brief_lc.contains("timed out")
        || brief.contains("Security violation")
}

fn log_event(ev: &Event) {
    match ev {
        Event::ToolCallStart { tool, call_id, .. } => {
            info!(tool = %tool, call_id = %call_id, "tool call start");
        }
        Event::ToolCallEnd {
            ok, brief, call_id, ..
        } => {
            if *ok {
                info!(call_id = %call_id, brief = %truncate(brief, 160), "tool call ok");
            } else {
                warn!(call_id = %call_id, brief = %truncate(brief, 160), "tool call err");
            }
        }
        Event::HistoryCompacted {
            replaced_turns,
            saved_tokens_estimate,
            ..
        } => {
            info!(
                turns = *replaced_turns,
                saved_tokens = *saved_tokens_estimate,
                "history compacted",
            );
        }
        Event::ErrorRaised { class, brief, .. } => {
            error!(class = %class, brief = %brief, "runtime error");
        }
        Event::Paused { reason, .. } => {
            warn!(reason = %reason, "run paused");
        }
        other => debug!(?other, "event"),
    }
}

fn print_turn_result(state: &RunState) {
    match &state.step {
        Step::Done { final_text } => {
            println!("\n{final_text}\n");
        }
        Step::Failed { reason, .. } => {
            eprintln!("(failed: {reason})");
        }
        Step::Paused { reason } => {
            eprintln!("(paused: {reason:?})");
        }
        _ => {}
    }
}

enum CmdAction {
    Continue,
    Quit,
    // Boxed: RunState is large (~hundreds of bytes incl. history vec) and
    // the Continue/Quit variants are unit — without the indirection clippy
    // (rightly) flags the enum size as wasted on the common path.
    Reset(Box<RunState>),
}

async fn handle_command(
    line: &str,
    state: &mut RunState,
    runtime: &mut ReplRuntime,
    clock: &muagent::core::clock::SystemClock,
) -> CmdAction {
    use muagent::core::clock::Clock;
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "/quit" | "/exit" => CmdAction::Quit,
        "/help" => {
            print_help();
            CmdAction::Continue
        }
        "/new" => {
            println!("(new session)");
            CmdAction::Reset(Box::new(RunState::new(
                Uuid::new_v4(),
                Uuid::new_v4(),
                clock.now_ms(),
            )))
        }
        "/tokens" => {
            let u = &state.usage;
            let cache_hit_pct = if u.tokens_prompt > 0 {
                (u.tokens_cache_read as f64 / u.tokens_prompt as f64) * 100.0
            } else {
                0.0
            };
            println!(
                "prompt={} (cache_read={} write={} hit={:.1}%) completion={} \
                     thinking={} turns={} tool_calls={} cost_usd={:.4}",
                u.tokens_prompt,
                u.tokens_cache_read,
                u.tokens_cache_write,
                cache_hit_pct,
                u.tokens_completion,
                u.tokens_thinking,
                u.turns,
                u.tool_calls,
                u.cost_usd
            );
            CmdAction::Continue
        }
        "/history" => {
            let start = state.history.len().saturating_sub(20);
            for (i, m) in state.history.iter().enumerate().skip(start) {
                println!("  [{i}] {}", brief_msg(m));
            }
            CmdAction::Continue
        }
        "/model" => {
            let model = it.next();
            if let Some(model) = model {
                if it.next().is_some() {
                    eprintln!("usage: /model [model_id]");
                    return CmdAction::Continue;
                }
                match switch_runtime_model(runtime, None, Some(model.to_string())).await {
                    Ok(()) => println!(
                        "(model switched: provider={:?} model={})",
                        runtime.cfg.model.provider, runtime.cfg.model.model
                    ),
                    Err(e) => eprintln!("model switch failed: {e}"),
                }
            } else {
                println!(
                    "provider={:?} model={}",
                    runtime.cfg.model.provider, runtime.cfg.model.model
                );
            }
            CmdAction::Continue
        }
        "/provider" => {
            let provider = it.next();
            let model = it.next();
            if it.next().is_some() {
                eprintln!("usage: /provider [name] [model_id]");
                return CmdAction::Continue;
            }
            if let Some(provider) = provider {
                match switch_runtime_model(
                    runtime,
                    Some(provider.to_string()),
                    model.map(ToString::to_string),
                )
                .await
                {
                    Ok(()) => println!(
                        "(provider switched: provider={:?} model={})",
                        runtime.cfg.model.provider, runtime.cfg.model.model
                    ),
                    Err(e) => eprintln!("provider switch failed: {e}"),
                }
            } else {
                println!(
                    "provider={:?} model={}",
                    runtime.cfg.model.provider, runtime.cfg.model.model
                );
            }
            CmdAction::Continue
        }
        "/skills" => {
            let skills = runtime.wired.skills.all_skills();
            if skills.is_empty() {
                println!("  (no skills registered)");
            } else {
                for skill in skills {
                    let desc: String = skill.description().chars().take(120).collect();
                    println!("  {} — {}", skill.id(), desc);
                }
            }
            CmdAction::Continue
        }
        "/session" => {
            // Read from `state` — the previously-captured outer `session_id`
            // would go stale after `/new` (which reseeds the state with a
            // brand new session_id), causing /session to print the wrong id.
            println!(
                "session_id={} run_id={} step={:?}",
                state.session_id, state.run_id, state.step
            );
            CmdAction::Continue
        }
        "/list" => {
            let limit = it.next().and_then(|s| s.parse::<usize>().ok());
            match runtime.wired.sessions.list_sessions(limit).await {
                Ok(xs) if xs.is_empty() => println!("  (no persisted sessions)"),
                Ok(xs) => {
                    for s in xs {
                        println!(
                            "  session={} runs={} status={:?} updated_ms={}",
                            s.session_id, s.run_count, s.latest_status, s.updated_ms
                        );
                    }
                }
                Err(e) => eprintln!("list failed: {e}"),
            }
            CmdAction::Continue
        }
        "/continue" => {
            let Some(raw) = it.next() else {
                eprintln!("usage: /continue <session_id>");
                return CmdAction::Continue;
            };
            let sid = match Uuid::parse_str(raw) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("invalid session_id: {e}");
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .continue_session(sid, clock.now_ms())
                .await
            {
                Ok(next) => {
                    println!(
                        "(continued session {}; new run {})",
                        next.session_id, next.run_id
                    );
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    eprintln!("continue failed: {e}");
                    CmdAction::Continue
                }
            }
        }
        "/fork" => {
            let Some(raw_run) = it.next() else {
                eprintln!("usage: /fork <run_id> <message_index>");
                return CmdAction::Continue;
            };
            let Some(raw_index) = it.next() else {
                eprintln!("usage: /fork <run_id> <message_index>");
                return CmdAction::Continue;
            };
            let run_id = match Uuid::parse_str(raw_run) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("invalid run_id: {e}");
                    return CmdAction::Continue;
                }
            };
            let index = match raw_index.parse::<usize>() {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("invalid message_index: {e}");
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .fork_from(run_id, index, clock.now_ms())
                .await
            {
                Ok(next) => {
                    println!(
                        "(forked run {}; new session {} run {})",
                        run_id, next.session_id, next.run_id
                    );
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    eprintln!("fork failed: {e}");
                    CmdAction::Continue
                }
            }
        }
        "/search" => {
            let query = it.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                eprintln!("usage: /search <query>");
                return CmdAction::Continue;
            }
            match runtime.wired.sessions.search(&query, Some(20)).await {
                Ok(xs) if xs.is_empty() => println!("  (no matches)"),
                Ok(xs) => {
                    for h in xs {
                        println!(
                            "  session={} run={} msg={} {}",
                            h.session_id,
                            h.run_id,
                            h.message_index,
                            truncate(&h.brief, 140)
                        );
                    }
                }
                Err(e) => eprintln!("search failed: {e}"),
            }
            CmdAction::Continue
        }
        _ => {
            eprintln!("unknown command; try /help");
            CmdAction::Continue
        }
    }
}

async fn handle_tui_command(
    line: &str,
    state: &mut RunState,
    runtime: &mut ReplRuntime,
    clock: &muagent::core::clock::SystemClock,
    app: &mut TuiApp,
) -> CmdAction {
    use muagent::core::clock::Clock;
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "/quit" | "/exit" => CmdAction::Quit,
        "/help" => {
            app.add_system(tui_help_text());
            CmdAction::Continue
        }
        "/new" => {
            app.add_system("(new session)");
            CmdAction::Reset(Box::new(RunState::new(
                Uuid::new_v4(),
                Uuid::new_v4(),
                clock.now_ms(),
            )))
        }
        "/tokens" => {
            app.add_system(token_summary(state));
            CmdAction::Continue
        }
        "/history" => {
            let start = state.history.len().saturating_sub(20);
            let lines = state
                .history
                .iter()
                .enumerate()
                .skip(start)
                .map(|(i, m)| format!("[{i}] {}", brief_msg(m)))
                .collect::<Vec<_>>();
            app.add_system(if lines.is_empty() {
                "(history is empty)".into()
            } else {
                lines.join("\n")
            });
            CmdAction::Continue
        }
        "/model" => {
            let model = it.next();
            if let Some(model) = model {
                if it.next().is_some() {
                    app.add_error("usage: /model [model_id]");
                    return CmdAction::Continue;
                }
                match switch_runtime_model(runtime, None, Some(model.to_string())).await {
                    Ok(()) => {
                        sync_tui_runtime(app, runtime);
                        app.add_system(format!(
                            "model switched: provider={:?} model={}",
                            runtime.cfg.model.provider, runtime.cfg.model.model
                        ));
                    }
                    Err(e) => app.add_error(format!("model switch failed: {e}")),
                }
            } else {
                app.add_system(format!(
                    "provider={:?} model={}",
                    runtime.cfg.model.provider, runtime.cfg.model.model
                ));
            }
            CmdAction::Continue
        }
        "/provider" => {
            let provider = it.next();
            let model = it.next();
            if it.next().is_some() {
                app.add_error("usage: /provider [name] [model_id]");
                return CmdAction::Continue;
            }
            if let Some(provider) = provider {
                match switch_runtime_model(
                    runtime,
                    Some(provider.to_string()),
                    model.map(ToString::to_string),
                )
                .await
                {
                    Ok(()) => {
                        sync_tui_runtime(app, runtime);
                        app.add_system(format!(
                            "provider switched: provider={:?} model={}",
                            runtime.cfg.model.provider, runtime.cfg.model.model
                        ));
                    }
                    Err(e) => app.add_error(format!("provider switch failed: {e}")),
                }
            } else {
                app.add_system(format!(
                    "provider={:?} model={}",
                    runtime.cfg.model.provider, runtime.cfg.model.model
                ));
            }
            CmdAction::Continue
        }
        "/skills" => {
            let skills = runtime.wired.skills.all_skills();
            if skills.is_empty() {
                app.add_system("(no skills registered)");
            } else {
                app.add_system(
                    skills
                        .into_iter()
                        .map(|skill| {
                            let desc: String = skill.description().chars().take(120).collect();
                            format!("{} - {}", skill.id(), desc)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            CmdAction::Continue
        }
        "/session" => {
            app.add_system(format!(
                "session_id={} run_id={} step={:?}",
                state.session_id, state.run_id, state.step
            ));
            CmdAction::Continue
        }
        "/list" => {
            let limit = it.next().and_then(|s| s.parse::<usize>().ok());
            match runtime.wired.sessions.list_sessions(limit).await {
                Ok(xs) if xs.is_empty() => app.add_system("(no persisted sessions)"),
                Ok(xs) => app.add_system(
                    xs.into_iter()
                        .map(|s| {
                            format!(
                                "session={} runs={} status={:?} updated_ms={}",
                                s.session_id, s.run_count, s.latest_status, s.updated_ms
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => app.add_error(format!("list failed: {e}")),
            }
            CmdAction::Continue
        }
        "/continue" => {
            let Some(raw) = it.next() else {
                app.add_error("usage: /continue <session_id>");
                return CmdAction::Continue;
            };
            let sid = match Uuid::parse_str(raw) {
                Ok(id) => id,
                Err(e) => {
                    app.add_error(format!("invalid session_id: {e}"));
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .continue_session(sid, clock.now_ms())
                .await
            {
                Ok(next) => {
                    app.add_system(format!(
                        "continued session {}; new run {}",
                        next.session_id, next.run_id
                    ));
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    app.add_error(format!("continue failed: {e}"));
                    CmdAction::Continue
                }
            }
        }
        "/fork" => {
            let Some(raw_run) = it.next() else {
                app.add_error("usage: /fork <run_id> <message_index>");
                return CmdAction::Continue;
            };
            let Some(raw_index) = it.next() else {
                app.add_error("usage: /fork <run_id> <message_index>");
                return CmdAction::Continue;
            };
            let run_id = match Uuid::parse_str(raw_run) {
                Ok(id) => id,
                Err(e) => {
                    app.add_error(format!("invalid run_id: {e}"));
                    return CmdAction::Continue;
                }
            };
            let index = match raw_index.parse::<usize>() {
                Ok(i) => i,
                Err(e) => {
                    app.add_error(format!("invalid message_index: {e}"));
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .fork_from(run_id, index, clock.now_ms())
                .await
            {
                Ok(next) => {
                    app.add_system(format!(
                        "forked run {}; new session {} run {}",
                        run_id, next.session_id, next.run_id
                    ));
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    app.add_error(format!("fork failed: {e}"));
                    CmdAction::Continue
                }
            }
        }
        "/search" => {
            let query = it.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                app.add_error("usage: /search <query>");
                return CmdAction::Continue;
            }
            match runtime.wired.sessions.search(&query, Some(20)).await {
                Ok(xs) if xs.is_empty() => app.add_system("(no matches)"),
                Ok(xs) => app.add_system(
                    xs.into_iter()
                        .map(|h| {
                            format!(
                                "session={} run={} msg={} {}",
                                h.session_id,
                                h.run_id,
                                h.message_index,
                                truncate(&h.brief, 140)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => app.add_error(format!("search failed: {e}")),
            }
            CmdAction::Continue
        }
        _ => {
            app.add_error("unknown command; try /help");
            CmdAction::Continue
        }
    }
}

async fn switch_runtime_model(
    runtime: &mut ReplRuntime,
    provider: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    let mut overrides = runtime.overrides.clone();
    if let Some(provider) = provider {
        overrides.provider = Some(provider);
        overrides.model = model;
    } else if let Some(model) = model {
        overrides.model = Some(model);
    }

    let cfg = Config::load(&overrides)?;
    let wired = setup::wire(&cfg).await?;
    runtime.overrides = overrides;
    runtime.cfg = cfg;
    runtime.wired = wired;
    Ok(())
}

fn append_tui_turn_result(app: &mut TuiApp, state: &RunState) {
    match &state.step {
        Step::Done { final_text } => {
            if final_text.trim().is_empty() {
                app.add_assistant("(no final text)");
            } else {
                app.add_assistant(final_text.clone());
            }
        }
        Step::Failed { reason, .. } => {
            app.add_error(format!("failed: {reason}"));
        }
        Step::Paused { reason } => {
            app.add_system(format!("paused: {reason:?}"));
        }
        other => {
            app.add_system(format!("run did not finish; step={other:?}"));
        }
    }
}

fn sync_tui_runtime(app: &mut TuiApp, runtime: &ReplRuntime) {
    app.set_runtime(
        format!("{:?}", runtime.cfg.model.provider),
        runtime.cfg.model.model.clone(),
    );
}

fn tui_config(cfg: &Config) -> TuiConfig {
    TuiConfig {
        provider: format!("{:?}", cfg.model.provider),
        model: cfg.model.model.clone(),
        store: store_label(cfg),
        root: cfg.fs.root.display().to_string(),
    }
}

fn tui_help_text() -> String {
    let mut out = String::from("Commands:");
    for (cmd, desc) in cli::REPL_COMMANDS {
        out.push('\n');
        out.push_str(cmd);
        out.push_str(" - ");
        out.push_str(desc);
    }
    out
}

fn token_summary(state: &RunState) -> String {
    let u = &state.usage;
    let cache_hit_pct = if u.tokens_prompt > 0 {
        (u.tokens_cache_read as f64 / u.tokens_prompt as f64) * 100.0
    } else {
        0.0
    };
    format!(
        "prompt={} (cache_read={} write={} hit={:.1}%) completion={} thinking={} turns={} tool_calls={} cost_usd={:.4}",
        u.tokens_prompt,
        u.tokens_cache_read,
        u.tokens_cache_write,
        cache_hit_pct,
        u.tokens_completion,
        u.tokens_thinking,
        u.turns,
        u.tool_calls,
        u.cost_usd
    )
}

fn store_label(cfg: &Config) -> String {
    match &cfg.store {
        muagent::config::StoreConfig::Memory => "memory".to_string(),
        muagent::config::StoreConfig::Jsonl(p) => format!("jsonl:{}", p.display()),
    }
}

fn print_banner(cfg: &Config) {
    println!(
        "μAgent v{} — provider={:?} model={}",
        env!("CARGO_PKG_VERSION"),
        cfg.model.provider,
        cfg.model.model
    );
    let store = store_label(cfg);
    println!(
        "  store={}  fs_root={}  sh={}",
        store,
        cfg.fs.root.display(),
        if cfg.fs.sh_allowlist.is_empty() {
            "disabled".into()
        } else {
            cfg.fs.sh_allowlist.join(",")
        }
    );
    let thinking_label = match (cfg.runtime.thinking_mode, cfg.runtime.thinking_effort) {
        (muagent::config::ThinkingModeCfg::Off, _) => "off".to_string(),
        (muagent::config::ThinkingModeCfg::Auto, _) => "auto".to_string(),
        (muagent::config::ThinkingModeCfg::Enabled, Some(e)) => format!("{e:?}").to_lowercase(),
        (muagent::config::ThinkingModeCfg::Enabled, None) => "enabled".into(),
    };
    println!(
        "  max_tokens={} threshold={:.2} keep_tail={}  cache={}  thinking={}",
        cfg.compaction.max_tokens,
        cfg.compaction.threshold_ratio,
        cfg.compaction.keep_tail_turns,
        if cfg.runtime.cache_auto {
            "auto"
        } else {
            "disabled"
        },
        thinking_label
    );
    let tools = cfg
        .capabilities
        .tool_allowlist
        .as_ref()
        .map(|x| format!("{} tools", x.len()))
        .unwrap_or_else(|| "all".into());
    let tools = if cfg.capabilities.tool_denylist.is_empty() {
        tools
    } else {
        format!("{tools}, -{}", cfg.capabilities.tool_denylist.len())
    };
    let skills = cfg
        .capabilities
        .skill_allowlist
        .as_ref()
        .map(|x| format!("{} skills", x.len()))
        .unwrap_or_else(|| "all".into());
    let skills = if cfg.capabilities.skill_denylist.is_empty() {
        skills
    } else {
        format!("{skills}, -{}", cfg.capabilities.skill_denylist.len())
    };
    let autoload = if cfg.capabilities.skill_autoload {
        "on"
    } else {
        "off"
    };
    println!(
        "  tools={}  skills={}  skill_autoload={}  agent_md={}",
        tools,
        skills,
        autoload,
        if cfg.agent_instructions.enabled {
            "on"
        } else {
            "off"
        }
    );
    let net = if cfg.net_http.enabled {
        "net_http:unrestricted"
    } else {
        "net_http:disabled"
    };
    println!("  {net}");
    println!("Type /help for commands. Ctrl-D to quit.\n");
}

fn print_help() {
    println!("Commands:");
    for (cmd, desc) in cli::REPL_COMMANDS {
        println!("  {:<18} {}", cmd, desc);
    }
}

fn prompt() {
    print!("> ");
    let _ = std::io::stdout().flush();
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn brief_msg(m: &Message) -> String {
    match m {
        Message::User { content } => format!("user: {}", truncate(&content_text(content), 140)),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let t = content_text(content);
            if tool_calls.is_empty() {
                format!("assistant: {}", truncate(&t, 140))
            } else {
                let names: Vec<String> = tool_calls.iter().map(|c| c.tool_name.clone()).collect();
                format!("assistant+tools{:?}: {}", names, truncate(&t, 100))
            }
        }
        Message::ToolResult { result, .. } => format!(
            "tool_result ok={}: {}",
            result.ok,
            truncate(&result.text(), 140)
        ),
        Message::System { content } => format!("system: {}", truncate(&content_text(content), 140)),
        Message::Observation { kind, text } => format!("obs {kind:?}: {}", truncate(text, 140)),
    }
}

fn content_text(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                muagent::core::types::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

// Force Arc<_> use so rustc keeps the import even in debug builds without warnings.
#[allow(dead_code)]
fn _force_arc_unused() -> Option<Arc<()>> {
    None
}

#[cfg(test)]
mod image_arg_tests {
    use super::*;

    #[test]
    fn local_image_arg_is_sent_as_inline_b64_not_local_uri() {
        let path = std::env::temp_dir().join(format!("muagent-cli-image-{}.png", Uuid::new_v4()));
        std::fs::write(&path, [1_u8, 2, 3]).unwrap();

        let content = user_content("read it", &[path.display().to_string()]).unwrap();
        let Content::Parts(parts) = content else {
            panic!("expected multipart content");
        };
        let ContentPart::Image { uri, b64, mime } = &parts[1] else {
            panic!("expected image part");
        };
        assert!(uri.is_none());
        assert_eq!(b64.as_deref(), Some("AQID"));
        assert_eq!(mime, "image/png");

        let _ = std::fs::remove_file(path);
    }
}
