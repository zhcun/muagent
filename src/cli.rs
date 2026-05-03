//! Argv parsing and tracing init for the `muagent` binary.
//!
//! Hand-rolled to avoid pulling in clap. Supports `-h`/`--help`,
//! `-V`/`--version`, UI mode flags, and flag overrides that map to the same env vars
//! `config::Config::load` reads. CLI flags win over env without mutating the
//! process environment.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use crate::config::{parse_list_arg, ConfigOverrides};

/// Single source of truth for interactive command list. Both `/help` and the CLI
/// `--help` read from here so the two views stay aligned. The TUI's tab
/// completion derives its candidate list from this same table via
/// [`repl_command_names`].
pub const REPL_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a new session (drop history)"),
    ("/tokens", "show token usage for current session"),
    ("/history", "print last 20 messages briefly"),
    ("/doctor", "check provider/config readiness"),
    ("/model [model_id]", "show or switch model for this session"),
    (
        "/provider [name] [model_id]",
        "show or switch provider/model for this session",
    ),
    ("/skills", "list registered skills"),
    ("/session", "show run/session ids and step"),
    ("/list", "list persisted sessions"),
    ("/continue <session_id>", "continue a persisted session"),
    (
        "/fork <run_id> <message_index>",
        "fork a run at message index",
    ),
    ("/search <query>", "search persisted session history"),
    ("/quit | /exit", "exit"),
];

/// Pull bare command names out of [`REPL_COMMANDS`] (e.g. `/model` from
/// `"/model [model_id]"`, both `/quit` and `/exit` from `"/quit | /exit"`).
/// The TUI feeds this into tab completion so the completion candidates are
/// always in sync with the documented command set.
pub fn repl_command_names() -> impl Iterator<Item = &'static str> {
    REPL_COMMANDS
        .iter()
        .flat_map(|(spec, _)| spec.split_whitespace())
        .filter(|tok| tok.starts_with('/'))
}

/// What main decides to do after parsing argv.
pub enum Action {
    /// Proceed with these env overrides applied.
    Run(Box<Invocation>),
    /// Print message and exit with the given code.
    Exit(ExitCode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub mode: RunMode,
    pub images: Vec<String>,
    pub config: ConfigOverrides,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunMode {
    Repl,
    Tui {
        prompt: Option<String>,
    },
    Exec(String),
    ResumePicker {
        all: bool,
    },
    ResumeLast {
        prompt: Option<String>,
        tui: bool,
    },
    ResumeSession {
        session_id: String,
        prompt: Option<String>,
        tui: bool,
    },
    ListSessions {
        all: bool,
    },
}

impl RunMode {
    pub fn uses_tui_session(&self) -> bool {
        #[cfg(not(feature = "tui"))]
        {
            false
        }
        #[cfg(feature = "tui")]
        matches!(
            self,
            RunMode::Tui { .. }
                | RunMode::ResumePicker { .. }
                | RunMode::ResumeLast {
                    prompt: None,
                    tui: true,
                }
                | RunMode::ResumeSession {
                    prompt: None,
                    tui: true,
                    ..
                }
        )
    }
}

/// Parse argv; on `--help`/`--version`, print and return Exit. On flags,
/// set env vars in-process so `config::Config::from_env` picks them up.
pub fn parse() -> Action {
    let args: Vec<String> = std::env::args().skip(1).collect();
    parse_from(args)
}

fn parse_from(args: Vec<String>) -> Action {
    let mut i = 0;
    let mut images = Vec::new();
    let mut config = ConfigOverrides::default();
    let mut tui = true;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                return Action::Exit(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("muagent {}", env!("CARGO_PKG_VERSION"));
                return Action::Exit(ExitCode::SUCCESS);
            }
            "--tui" => {
                tui = true;
            }
            "--repl" => {
                tui = false;
            }
            "--config-file" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.config_file = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--provider" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.provider = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--model" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.model = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "-m" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.model = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--image" | "-i" => {
                if let Some(v) = next_val(&args, &mut i) {
                    push_image_list(&mut images, &v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--base-url" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.base_url = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--store" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.store = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--root" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.root = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--mcp-sse" => {
                if let Some(v) = next_val(&args, &mut i) {
                    push_config_list(&mut config.mcp_sse, &v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--cache" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.cache = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--thinking" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.thinking = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--max-tokens" => {
                if let Some(v) = next_val(&args, &mut i) {
                    match v.parse::<u32>() {
                        Ok(n) => config.max_tokens = Some(n),
                        Err(_) => {
                            eprintln!("muagent: `{a}` requires an integer value");
                            return Action::Exit(ExitCode::from(2));
                        }
                    }
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--log" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.log = Some(v);
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--tools" | "--enable-tools" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.tool_allowlist = Some(parse_list_arg(&v));
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--disable-tools" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.tool_denylist = Some(parse_list_arg(&v));
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--skills" | "--enable-skills" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.skill_allowlist = Some(parse_list_arg(&v));
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--disable-skills" => {
                if let Some(v) = next_val(&args, &mut i) {
                    config.skill_denylist = Some(parse_list_arg(&v));
                } else {
                    return Action::Exit(missing(a));
                }
            }
            "--no-skills-autoload" => {
                config.skill_autoload = Some(false);
            }
            "exec" => {
                return parse_exec(&args[i + 1..], images, config);
            }
            "resume" => {
                return parse_resume(&args[i + 1..], false, tui, images, config);
            }
            "sessions" | "session" | "list" => {
                return parse_sessions(&args[i + 1..], images, config);
            }
            "repl" => {
                return run(RunMode::Repl, images, config);
            }
            "--" => {
                if i + 1 >= args.len() {
                    return run(
                        if tui {
                            RunMode::Tui { prompt: None }
                        } else {
                            RunMode::Repl
                        },
                        images,
                        config,
                    );
                }
                return run(
                    RunMode::Tui {
                        prompt: Some(args[i + 1..].join(" ")),
                    },
                    images,
                    config,
                );
            }
            other => {
                if other.starts_with('-') {
                    eprintln!("muagent: unknown argument `{other}`. Try --help.");
                    return Action::Exit(ExitCode::from(2));
                }
                return run(
                    RunMode::Tui {
                        prompt: Some(args[i..].join(" ")),
                    },
                    images,
                    config,
                );
            }
        }
        i += 1;
    }
    run(
        if tui {
            RunMode::Tui { prompt: None }
        } else {
            RunMode::Repl
        },
        images,
        config,
    )
}

fn parse_exec(args: &[String], mut images: Vec<String>, mut config: ConfigOverrides) -> Action {
    if args.is_empty() {
        eprintln!("muagent: `exec` requires a prompt, or `exec resume --last <prompt>`");
        return Action::Exit(ExitCode::from(2));
    }
    let mut i = 0;
    while i < args.len() {
        match parse_common_option(args, &mut i, &mut config, Some(&mut images), None) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(action) => return action,
        }
        match args[i].as_str() {
            "--image" | "-i" => {
                if let Some(v) = args.get(i + 1) {
                    push_image_list(&mut images, v);
                    i += 2;
                } else {
                    return Action::Exit(missing(&args[i]));
                }
            }
            "resume" => return parse_resume(&args[i + 1..], true, false, images, config),
            "--" => {
                if i + 1 >= args.len() {
                    break;
                }
                return run(RunMode::Exec(args[i + 1..].join(" ")), images, config);
            }
            other if other.starts_with('-') => {
                eprintln!("muagent: unknown exec argument `{other}`. Try --help.");
                return Action::Exit(ExitCode::from(2));
            }
            _ => return run(RunMode::Exec(args[i..].join(" ")), images, config),
        }
    }
    eprintln!("muagent: `exec` requires a prompt, or `exec resume --last <prompt>`");
    Action::Exit(ExitCode::from(2))
}

fn parse_resume(
    args: &[String],
    prompt_required: bool,
    mut tui: bool,
    mut images: Vec<String>,
    mut config: ConfigOverrides,
) -> Action {
    if args.is_empty() && !prompt_required {
        return run(RunMode::ResumePicker { all: false }, images, config);
    }
    if args.is_empty() {
        eprintln!("muagent: `exec resume` requires a prompt, `--last <prompt>`, or a session id");
        return Action::Exit(ExitCode::from(2));
    }

    let mut i = 0;
    let mut last = false;
    let mut all = false;
    let mut session_id: Option<String> = None;
    let mut prompt_start: Option<usize> = None;
    while i < args.len() {
        match parse_common_option(args, &mut i, &mut config, Some(&mut images), Some(&mut tui)) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(action) => return action,
        }
        let a = &args[i];
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                return Action::Exit(ExitCode::SUCCESS);
            }
            "--last" => {
                last = true;
                i += 1;
            }
            "--all" => {
                all = true;
                i += 1;
            }
            "--" => {
                i += 1;
                prompt_start = Some(i);
                break;
            }
            other if other.starts_with('-') => {
                eprintln!("muagent: unknown resume argument `{other}`. Try --help.");
                return Action::Exit(ExitCode::from(2));
            }
            other => {
                if !last && session_id.is_none() && uuid::Uuid::parse_str(other).is_ok() {
                    session_id = Some(other.to_string());
                    i += 1;
                    continue;
                }
                prompt_start = Some(i);
                break;
            }
        }
    }

    let prompt = prompt_start
        .filter(|start| *start < args.len())
        .map(|start| args[start..].join(" "));
    if prompt_required && prompt.as_deref().unwrap_or("").trim().is_empty() {
        eprintln!("muagent: `exec resume` requires a prompt");
        return Action::Exit(ExitCode::from(2));
    }

    let tui = tui && prompt.is_none() && !prompt_required;
    if last {
        run(RunMode::ResumeLast { prompt, tui }, images, config)
    } else if let Some(session_id) = session_id {
        run(
            RunMode::ResumeSession {
                session_id,
                prompt,
                tui,
            },
            images,
            config,
        )
    } else if prompt.is_some() {
        run(RunMode::ResumeLast { prompt, tui }, images, config)
    } else {
        run(RunMode::ResumePicker { all }, images, config)
    }
}

fn parse_sessions(args: &[String], images: Vec<String>, mut config: ConfigOverrides) -> Action {
    if !images.is_empty() {
        eprintln!("muagent: sessions does not accept --image");
        return Action::Exit(ExitCode::from(2));
    }
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match parse_common_option(args, &mut i, &mut config, None, None) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(action) => return action,
        }
        let a = &args[i];
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                return Action::Exit(ExitCode::SUCCESS);
            }
            "--all" => all = true,
            other => {
                eprintln!("muagent: unknown sessions argument `{other}`. Try --help.");
                return Action::Exit(ExitCode::from(2));
            }
        }
        i += 1;
    }
    run(RunMode::ListSessions { all }, images, config)
}

fn parse_common_option(
    args: &[String],
    i: &mut usize,
    config: &mut ConfigOverrides,
    images: Option<&mut Vec<String>>,
    tui: Option<&mut bool>,
) -> Result<bool, Action> {
    let a = &args[*i];
    match a.as_str() {
        "--tui" => {
            if let Some(tui) = tui {
                *tui = true;
                *i += 1;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        "--repl" => {
            if let Some(tui) = tui {
                *tui = false;
                *i += 1;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        "--config-file" => value(args, i, a, |v| config.config_file = Some(v)),
        "--provider" => value(args, i, a, |v| config.provider = Some(v)),
        "--model" | "-m" => value(args, i, a, |v| config.model = Some(v)),
        "--base-url" => value(args, i, a, |v| config.base_url = Some(v)),
        "--store" => value(args, i, a, |v| config.store = Some(v)),
        "--root" => value(args, i, a, |v| config.root = Some(v)),
        "--cache" => value(args, i, a, |v| config.cache = Some(v)),
        "--thinking" => value(args, i, a, |v| config.thinking = Some(v)),
        "--log" => value(args, i, a, |v| config.log = Some(v)),
        "--mcp-sse" => value(args, i, a, |v| push_config_list(&mut config.mcp_sse, &v)),
        "--tools" | "--enable-tools" => value(args, i, a, |v| {
            config.tool_allowlist = Some(parse_list_arg(&v))
        }),
        "--disable-tools" => value(args, i, a, |v| {
            config.tool_denylist = Some(parse_list_arg(&v))
        }),
        "--skills" | "--enable-skills" => value(args, i, a, |v| {
            config.skill_allowlist = Some(parse_list_arg(&v))
        }),
        "--disable-skills" => value(args, i, a, |v| {
            config.skill_denylist = Some(parse_list_arg(&v))
        }),
        "--no-skills-autoload" => {
            config.skill_autoload = Some(false);
            *i += 1;
            Ok(true)
        }
        "--max-tokens" => {
            let Some(v) = args.get(*i + 1).cloned() else {
                return Err(Action::Exit(missing(a)));
            };
            match v.parse::<u32>() {
                Ok(n) => {
                    config.max_tokens = Some(n);
                    *i += 2;
                    Ok(true)
                }
                Err(_) => {
                    eprintln!("muagent: `{a}` requires an integer value");
                    Err(Action::Exit(ExitCode::from(2)))
                }
            }
        }
        "--image" | "-i" => {
            let Some(images) = images else {
                return Ok(false);
            };
            let Some(v) = args.get(*i + 1) else {
                return Err(Action::Exit(missing(a)));
            };
            push_image_list(images, v);
            *i += 2;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn value(
    args: &[String],
    i: &mut usize,
    a: &str,
    apply: impl FnOnce(String),
) -> Result<bool, Action> {
    let Some(v) = args.get(*i + 1).cloned() else {
        return Err(Action::Exit(missing(a)));
    };
    apply(v);
    *i += 2;
    Ok(true)
}

fn run(mode: RunMode, images: Vec<String>, config: ConfigOverrides) -> Action {
    Action::Run(Box::new(Invocation {
        mode,
        images,
        config,
    }))
}

fn push_image_list(out: &mut Vec<String>, raw: &str) {
    out.extend(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string),
    );
}

fn push_config_list(slot: &mut Option<Vec<String>>, raw: &str) {
    slot.get_or_insert_with(Vec::new)
        .extend(parse_list_arg(raw));
}

fn next_val(args: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    args.get(*i).cloned()
}

fn missing(flag: &str) -> ExitCode {
    eprintln!("muagent: `{flag}` requires a value");
    ExitCode::from(2)
}

fn print_help() {
    let bin = "muagent";
    println!(
        "\
{bin} {ver} — agent terminal for the μAgent runtime.

USAGE:
    {bin} [OPTIONS]                Start the TUI
    {bin} [OPTIONS] <PROMPT>       Start the TUI with an initial prompt
    {bin} [OPTIONS] exec <PROMPT>
    {bin} [OPTIONS] resume
    {bin} [OPTIONS] resume [PROMPT]
    {bin} [OPTIONS] resume --last [PROMPT]
    {bin} [OPTIONS] resume <SESSION_ID> [PROMPT]
    {bin} [OPTIONS] exec resume --last <PROMPT>
    {bin} [OPTIONS] exec resume <SESSION_ID> <PROMPT>
    {bin} [OPTIONS] sessions [--all]
    {bin} [OPTIONS] repl

OPTIONS:
  -h, --help              Print this help and exit.
  -V, --version           Print version and exit.
      --tui               Start the full-screen terminal UI (default without a prompt).
      --repl              Start the line REPL instead of the TUI when no prompt is given.

      --config-file <FILE>
                          Load this config.toml instead of the default config
                          search path. (env: MUAGENT_CONFIG)
      --provider <NAME>   Model provider: openai | openai-codex | anthropic | google | openrouter
                          (default: openrouter; env: MUAGENT_PROVIDER)
  -m, --model <ID>        Model id (env: MUAGENT_MODEL).
      --base-url <URL>    Override API base URL (env: MUAGENT_BASE_URL).
  -i, --image <PATHS>     Attach image file(s) to the prompt. Comma-separated
                          paths and repeated --image flags are both accepted.
      --store <SPEC>      Session store. Default: `jsonl:~/.muagent/sessions`.
                          Use `memory` for throwaway sessions, or
                          `jsonl:/path/to/store` for a file-backed JSONL store.
                          A plain path is also treated as JSONL. (env: MUAGENT_STORE)
      --root <DIR>        Sandbox root for fs tools (env: MUAGENT_ROOT).
      --mcp-sse <URLS>    Comma-separated legacy MCP SSE endpoint(s), e.g.
                          http://127.0.0.1:10086/sse. Repeated flags are
                          accepted. (env: MUAGENT_MCP_SSE)
      --cache <MODE>      auto (default) | off. (env: MUAGENT_CACHE)
      --thinking <MODE>   high (default) | auto | off | minimal | low | medium | max.
                          (env: MUAGENT_THINKING)
      --max-tokens <N>    Context budget for auto-compaction
                          (default 156000; env: MUAGENT_MAX_TOKENS).
                          Advanced summary env knobs:
                          MUAGENT_SUMMARY_INPUT_MAX_TOKENS,
                          MUAGENT_SUMMARY_OUTPUT_MAX_TOKENS,
                          MUAGENT_RESTART_REPAIR_WINDOW_TOKENS,
                          MUAGENT_MAX_SUMMARY_ROUNDS.
      --log <FILTER>      tracing EnvFilter, e.g. `muagent=debug,info`.
                          (env: MUAGENT_LOG, falls back to RUST_LOG)
                          Advanced env: MUAGENT_MAX_STEPS controls the
                          model/tool loop step safety limit (default 10000).
                          MUAGENT_BAD_TOOL_EVENT_LIMIT can stop repeated
                          timeout/security/error tool loops (default off).
      --tools <LIST>, --enable-tools <LIST>
                          Comma-separated tool-name allowlist. Default:
                          all registered tools load. (env: MUAGENT_TOOLS)
      --disable-tools <LIST>
                          Comma-separated tool-name denylist.
                          (env: MUAGENT_DISABLE_TOOLS)
      --skills <LIST>, --enable-skills <LIST>
                          Comma-separated skill-id allowlist. Default:
                          all discovered skills load. (env: MUAGENT_SKILLS)
      --disable-skills <LIST>
                          Comma-separated skill-id denylist.
                          (env: MUAGENT_DISABLE_SKILLS)
      --no-skills-autoload
                          Don't auto-load skills from `./.muagent/skills/`
                          and `~/.muagent/skills/`. (env: MUAGENT_SKILL_AUTOLOAD=off)

ENVIRONMENT:
  One of these must be set (depending on --provider):
    OPENROUTER_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY
  For --provider openai-codex, run `codex login` or pi-mono login first;
  existing ~/.codex/auth.json / ~/.pi/agent/auth.json OAuth credentials are used.
  Or set MUAGENT_API_KEY to override all.
  Agent instruction files are loaded from AGENT.md / AGENTS.md / CLAUDE.md
  in the workspace ancestors and user config dirs. Set MUAGENT_AGENT_MD=off
  to disable, or MUAGENT_AGENT_MD_MAX_BYTES to change the per-file cap.

CONFIG FILES:
  Defaults are loaded from ~/.muagent/config.toml and then project
  .muagent/config.toml files from parent directories to the current directory.
  Env vars and CLI flags override config files. Example:
    [model]
    provider = \"openrouter\"
    model = \"openai/gpt-5.4-nano\"
    api_key_env = \"OPENROUTER_API_KEY\"
    [providers.google]
    model = \"gemini-3.1-flash-lite-preview\"
    api_key_env = \"GEMINI_API_KEY\"
    [tools]
    # enabled = [] means expose no tools; omit enabled to expose all registered tools.
    disabled = [\"net_http\"]
    [compaction]
    max_tokens = 156000
    summary_input_max_tokens = 100000
    summary_output_max_tokens = 8000

INTERACTIVE COMMANDS:
  {repl_commands}",
        bin = bin,
        ver = env!("CARGO_PKG_VERSION"),
        repl_commands = REPL_COMMANDS
            .iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(" "),
    );
}

/// Initialize the tracing subscriber. Respects MUAGENT_LOG → RUST_LOG.
/// TUI sessions default to quiet logging so stderr does not paint over the
/// alternate-screen UI.
pub fn init_tracing(cli_filter: Option<&str>, tui_session: bool) {
    let explicit_filter = cli_filter
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("MUAGENT_LOG").ok())
        .or_else(|| std::env::var("RUST_LOG").ok());
    let filter = explicit_filter.unwrap_or_else(|| {
        if tui_session {
            "off".into()
        } else {
            "muagent=info,warn".into()
        }
    });
    let env_filter =
        EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("muagent=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(atty_stderr())
        .try_init();
}

fn atty_stderr() -> bool {
    // Without dragging in `atty`: Unix-ish heuristic. Good enough for UX.
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        isatty(2) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_from, Action, Invocation, RunMode};

    fn mode(args: &[&str]) -> RunMode {
        invocation(args).mode
    }

    fn invocation(args: &[&str]) -> Invocation {
        match parse_from(args.iter().map(|s| s.to_string()).collect()) {
            Action::Run(invocation) => *invocation,
            Action::Exit(code) => panic!("parse exited with {code:?}"),
        }
    }

    #[test]
    fn no_args_runs_tui_by_default() {
        assert_eq!(mode(&[]), RunMode::Tui { prompt: None });
    }

    #[test]
    fn repl_subcommand_runs_line_repl() {
        assert_eq!(mode(&["repl"]), RunMode::Repl);
    }

    #[test]
    fn singular_session_lists_sessions() {
        assert_eq!(mode(&["session"]), RunMode::ListSessions { all: false });
    }

    #[test]
    fn positional_prompt_starts_tui_with_initial_prompt() {
        assert_eq!(
            mode(&["Explain", "this", "codebase"]),
            RunMode::Tui {
                prompt: Some("Explain this codebase".into())
            }
        );
    }

    #[test]
    fn exec_prompt_runs_one_shot() {
        assert_eq!(
            mode(&["exec", "Fix", "the", "bug"]),
            RunMode::Exec("Fix the bug".into())
        );
    }

    #[test]
    fn tui_flag_runs_tui_mode() {
        assert_eq!(mode(&["--tui"]), RunMode::Tui { prompt: None });
    }

    #[test]
    fn exec_resume_last_accepts_prompt() {
        assert_eq!(
            mode(&["exec", "resume", "--last", "Fix", "it"]),
            RunMode::ResumeLast {
                prompt: Some("Fix it".into()),
                tui: false,
            }
        );
    }

    #[test]
    fn separator_allows_prompt_starting_with_dash() {
        assert_eq!(
            mode(&["--", "- First name: Yusuf"]),
            RunMode::Tui {
                prompt: Some("- First name: Yusuf".into())
            }
        );
    }

    #[test]
    fn exec_separator_allows_prompt_starting_with_dash() {
        assert_eq!(
            mode(&["exec", "--", "- First name: Yusuf"]),
            RunMode::Exec("- First name: Yusuf".into())
        );
    }

    #[test]
    fn resume_last_separator_allows_prompt_starting_with_dash() {
        assert_eq!(
            mode(&["resume", "--last", "--", "- First name: Yusuf"]),
            RunMode::ResumeLast {
                prompt: Some("- First name: Yusuf".into()),
                tui: false,
            }
        );
    }

    #[test]
    fn resume_without_prompt_uses_tui_by_default() {
        assert_eq!(
            mode(&["resume", "--last"]),
            RunMode::ResumeLast {
                prompt: None,
                tui: true,
            }
        );
    }

    #[test]
    fn resume_without_prompt_can_use_line_repl() {
        assert_eq!(
            mode(&["--repl", "resume", "--last"]),
            RunMode::ResumeLast {
                prompt: None,
                tui: false,
            }
        );
    }

    #[test]
    fn resume_accepts_config_flags_after_subcommand() {
        let got = invocation(&[
            "resume",
            "--provider",
            "openai-codex",
            "--model",
            "gpt-5.4",
            "--root",
            "/tmp/work",
            "--store",
            "memory",
        ]);
        assert_eq!(got.mode, RunMode::ResumePicker { all: false });
        assert_eq!(got.config.provider.as_deref(), Some("openai-codex"));
        assert_eq!(got.config.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(got.config.root.as_deref(), Some("/tmp/work"));
        assert_eq!(got.config.store.as_deref(), Some("memory"));
    }

    #[test]
    fn resume_repl_flag_after_subcommand_is_respected() {
        assert_eq!(
            mode(&["resume", "--repl", "--last"]),
            RunMode::ResumeLast {
                prompt: None,
                tui: false,
            }
        );
    }

    #[test]
    fn exec_resume_accepts_config_flags_after_resume() {
        let got = invocation(&[
            "exec",
            "resume",
            "--provider",
            "openai-codex",
            "--last",
            "continue",
        ]);
        assert_eq!(
            got.mode,
            RunMode::ResumeLast {
                prompt: Some("continue".into()),
                tui: false,
            }
        );
        assert_eq!(got.config.provider.as_deref(), Some("openai-codex"));
    }

    #[test]
    fn resume_session_accepts_config_flags_after_session_id() {
        let session_id = "36dc8f89-57f0-46cf-9510-f6641fc86706";
        let got = invocation(&[
            "resume",
            session_id,
            "--provider",
            "openai-codex",
            "--model",
            "gpt-5.4-nano",
            "continue",
        ]);
        assert_eq!(
            got.mode,
            RunMode::ResumeSession {
                session_id: session_id.into(),
                prompt: Some("continue".into()),
                tui: false,
            }
        );
        assert_eq!(got.config.provider.as_deref(), Some("openai-codex"));
        assert_eq!(got.config.model.as_deref(), Some("gpt-5.4-nano"));
    }

    #[test]
    fn exec_resume_session_accepts_config_flags_after_session_id() {
        let session_id = "36dc8f89-57f0-46cf-9510-f6641fc86706";
        let got = invocation(&[
            "exec",
            "resume",
            session_id,
            "--provider",
            "openai-codex",
            "continue",
        ]);
        assert_eq!(
            got.mode,
            RunMode::ResumeSession {
                session_id: session_id.into(),
                prompt: Some("continue".into()),
                tui: false,
            }
        );
        assert_eq!(got.config.provider.as_deref(), Some("openai-codex"));
    }

    #[test]
    fn image_flag_accepts_comma_list() {
        let got = invocation(&["--image", "a.png,b.jpg", "exec", "Summarize", "these"]);
        assert_eq!(got.mode, RunMode::Exec("Summarize these".into()));
        assert_eq!(got.images, vec!["a.png", "b.jpg"]);
    }

    #[test]
    fn config_flags_fill_overrides_without_env_mutation() {
        let got = invocation(&[
            "--disable-tools",
            "net_http,sh_exec",
            "--max-tokens",
            "42000",
            "exec",
            "run",
            "task",
        ]);
        assert_eq!(got.mode, RunMode::Exec("run task".into()));
        assert_eq!(
            got.config.tool_denylist,
            Some(vec!["net_http".into(), "sh_exec".into()])
        );
        assert_eq!(got.config.max_tokens, Some(42_000));
    }

    #[test]
    fn bare_resume_opens_picker() {
        assert_eq!(mode(&["resume"]), RunMode::ResumePicker { all: false });
    }

    #[test]
    fn resume_prompt_continues_last_non_interactively() {
        assert_eq!(
            mode(&["resume", "continue", "the", "task"]),
            RunMode::ResumeLast {
                prompt: Some("continue the task".into()),
                tui: false,
            }
        );
    }

    #[test]
    fn sessions_lists_current_workspace_by_default() {
        assert_eq!(mode(&["sessions"]), RunMode::ListSessions { all: false });
        assert_eq!(
            mode(&["list", "--all"]),
            RunMode::ListSessions { all: true }
        );
    }

    #[test]
    fn exec_resume_session_accepts_prompt() {
        assert_eq!(
            mode(&[
                "exec",
                "resume",
                "7f9f9a2e-1b3c-4c7a-9b0e-000000000000",
                "Implement",
                "the",
                "plan",
            ]),
            RunMode::ResumeSession {
                session_id: "7f9f9a2e-1b3c-4c7a-9b0e-000000000000".into(),
                prompt: Some("Implement the plan".into()),
                tui: false,
            }
        );
    }

    #[test]
    fn resume_session_separator_allows_prompt_starting_with_dash() {
        assert_eq!(
            mode(&[
                "resume",
                "7f9f9a2e-1b3c-4c7a-9b0e-000000000000",
                "--",
                "- First name: Yusuf",
            ]),
            RunMode::ResumeSession {
                session_id: "7f9f9a2e-1b3c-4c7a-9b0e-000000000000".into(),
                prompt: Some("- First name: Yusuf".into()),
                tui: false,
            }
        );
    }
}
