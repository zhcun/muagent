//! Slash-command dispatch. One generic `handle` function over `CommandSink`
//! drives both the line REPL (`StdoutSink`) and the TUI (`TuiAppSink`).

use uuid::Uuid;

use crate::cli_app::doctor::config_doctor_report;
use crate::cli_app::sessions::{session_summary_line, sessions_for_display};
use crate::cli_app::sink::{CommandSink, SessionResetKind};
use crate::cli_app::state::{ensure_workspace_root, new_run_state};
use crate::cli_app::{brief_msg, truncate, ReplRuntime};
use crate::config::Config;
use crate::core::clock::{Clock, SystemClock};
use crate::core::run_state::RunState;
use crate::setup;

pub enum CmdAction {
    Continue,
    Quit,
    // Boxed: RunState is large (~hundreds of bytes incl. history vec) and
    // the Continue/Quit variants are unit — without the indirection clippy
    // (rightly) flags the enum size as wasted on the common path.
    Reset(Box<RunState>),
}

/// Dispatch a slash-command line. The sink decides how to render output and
/// how to respond to state-changing actions (`/new`, `/continue`, `/fork`,
/// `/model`, `/provider`).
pub async fn handle<S: CommandSink>(
    line: &str,
    state: &mut RunState,
    runtime: &mut ReplRuntime,
    clock: &SystemClock,
    sink: &mut S,
) -> CmdAction {
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "/quit" | "/exit" => CmdAction::Quit,
        "/help" => {
            // Two-step to avoid `&self` (`help_text`) + `&mut self` (`info`) on
            // the same sink in one expression. Compiles thanks to NLL but the
            // explicit form is robust to future trait-method tweaks.
            let help = sink.help_text();
            sink.info(help);
            CmdAction::Continue
        }
        "/new" => {
            let next = new_run_state(&runtime.cfg, clock);
            sink.on_session_replaced(&next, SessionResetKind::New);
            CmdAction::Reset(Box::new(next))
        }
        "/tokens" => {
            sink.info(token_summary(state));
            CmdAction::Continue
        }
        "/history" => {
            let start = state.history.len().saturating_sub(20);
            let lines: Vec<String> = state
                .history
                .iter()
                .enumerate()
                .skip(start)
                .map(|(i, m)| format!("[{i}] {}", brief_msg(m)))
                .collect();
            if lines.is_empty() {
                sink.info("(history is empty)".into());
            } else {
                sink.lines(lines);
            }
            CmdAction::Continue
        }
        "/doctor" => {
            sink.info(config_doctor_report(&runtime.cfg));
            CmdAction::Continue
        }
        "/model" => {
            let model = it.next();
            if let Some(model) = model {
                if it.next().is_some() {
                    sink.error("usage: /model [model_id]".into());
                    return CmdAction::Continue;
                }
                match switch_runtime_model(runtime, None, Some(model.to_string())).await {
                    Ok(()) => {
                        sink.on_runtime_switched(runtime);
                        sink.status(format!(
                            "model switched: provider={} model={}",
                            runtime.cfg.model.provider.cli_name(),
                            runtime.cfg.model.model
                        ));
                    }
                    Err(e) => sink.error(format!("model switch failed: {e}")),
                }
            } else {
                sink.info(format!(
                    "provider={} model={}",
                    runtime.cfg.model.provider.cli_name(),
                    runtime.cfg.model.model
                ));
            }
            CmdAction::Continue
        }
        "/provider" => {
            let provider = it.next();
            let model = it.next();
            if it.next().is_some() {
                sink.error("usage: /provider [name] [model_id]".into());
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
                        sink.on_runtime_switched(runtime);
                        sink.status(format!(
                            "provider switched: provider={} model={}",
                            runtime.cfg.model.provider.cli_name(),
                            runtime.cfg.model.model
                        ));
                    }
                    Err(e) => sink.error(format!("provider switch failed: {e}")),
                }
            } else {
                sink.info(format!(
                    "provider={} model={}",
                    runtime.cfg.model.provider.cli_name(),
                    runtime.cfg.model.model
                ));
            }
            CmdAction::Continue
        }
        "/skills" => {
            let skills = runtime.wired.skills.all_skills();
            if skills.is_empty() {
                sink.info("(no skills registered)".into());
            } else {
                let lines: Vec<String> = skills
                    .into_iter()
                    .map(|skill| {
                        let desc: String = skill.description().chars().take(120).collect();
                        format!("{} — {}", skill.id(), desc)
                    })
                    .collect();
                sink.lines(lines);
            }
            CmdAction::Continue
        }
        "/session" => {
            sink.info(format!(
                "session_id={} run_id={} step={:?}",
                state.session_id, state.run_id, state.step
            ));
            CmdAction::Continue
        }
        "/list" => {
            let limit = it.next().and_then(|s| s.parse::<usize>().ok());
            match sessions_for_display(&runtime.wired, &runtime.cfg, false, limit).await {
                Ok(xs) if xs.is_empty() => sink.info("(no persisted sessions)".into()),
                Ok(xs) => {
                    let lines = xs
                        .iter()
                        .enumerate()
                        .map(|(idx, s)| session_summary_line(idx, s, false))
                        .collect();
                    sink.lines(lines);
                }
                Err(e) => sink.error(format!("list failed: {e}")),
            }
            CmdAction::Continue
        }
        "/continue" => {
            let Some(raw) = it.next() else {
                sink.error("usage: /continue <session_id>".into());
                return CmdAction::Continue;
            };
            let sid = match Uuid::parse_str(raw) {
                Ok(id) => id,
                Err(e) => {
                    sink.error(format!("invalid session_id: {e}"));
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .continue_session(sid, clock.now_ms())
                .await
            {
                Ok(mut next) => {
                    ensure_workspace_root(&mut next, &runtime.cfg);
                    sink.on_session_replaced(&next, SessionResetKind::Continued);
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    sink.error(format!("continue failed: {e}"));
                    CmdAction::Continue
                }
            }
        }
        "/fork" => {
            let Some(raw_run) = it.next() else {
                sink.error("usage: /fork <run_id> <message_index>".into());
                return CmdAction::Continue;
            };
            let Some(raw_index) = it.next() else {
                sink.error("usage: /fork <run_id> <message_index>".into());
                return CmdAction::Continue;
            };
            let run_id = match Uuid::parse_str(raw_run) {
                Ok(id) => id,
                Err(e) => {
                    sink.error(format!("invalid run_id: {e}"));
                    return CmdAction::Continue;
                }
            };
            let index = match raw_index.parse::<usize>() {
                Ok(i) => i,
                Err(e) => {
                    sink.error(format!("invalid message_index: {e}"));
                    return CmdAction::Continue;
                }
            };
            match runtime
                .wired
                .sessions
                .fork_from(run_id, index, clock.now_ms())
                .await
            {
                Ok(mut next) => {
                    ensure_workspace_root(&mut next, &runtime.cfg);
                    let intro = format!(
                        "forked run {}; new session {} run {}",
                        run_id, next.session_id, next.run_id
                    );
                    sink.on_session_replaced(&next, SessionResetKind::Forked { intro });
                    CmdAction::Reset(Box::new(next))
                }
                Err(e) => {
                    sink.error(format!("fork failed: {e}"));
                    CmdAction::Continue
                }
            }
        }
        "/search" => {
            let query = it.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                sink.error("usage: /search <query>".into());
                return CmdAction::Continue;
            }
            match runtime.wired.sessions.search(&query, Some(20)).await {
                Ok(xs) if xs.is_empty() => sink.info("(no matches)".into()),
                Ok(xs) => {
                    let lines = xs
                        .into_iter()
                        .map(|h| {
                            format!(
                                "session={} run={} msg={} {}",
                                h.session_id,
                                h.run_id,
                                h.message_index,
                                truncate(&h.brief, 140)
                            )
                        })
                        .collect();
                    sink.lines(lines);
                }
                Err(e) => sink.error(format!("search failed: {e}")),
            }
            CmdAction::Continue
        }
        _ => {
            sink.error("unknown command; try /help".into());
            CmdAction::Continue
        }
    }
}

pub async fn switch_runtime_model(
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

fn token_summary(state: &RunState) -> String {
    let u = &state.usage;
    let cache_hit_pct = if u.tokens_prompt > 0 {
        (u.tokens_cache_read as f64 / u.tokens_prompt as f64) * 100.0
    } else {
        0.0
    };
    format!(
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
    )
}
