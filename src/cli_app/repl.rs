//! Line-mode REPL loop.

use std::io::Write;

use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::error;

use crate::cli_app::commands::{handle, CmdAction};
use crate::cli_app::driver::{print_turn_result, submit_and_drive};
use crate::cli_app::sink::StdoutSink;
use crate::cli_app::ReplRuntime;
use crate::config::{Config, ConfigOverrides};
use crate::core::clock::SystemClock;
use crate::core::run_state::RunState;
use crate::setup;

pub async fn run_repl(
    wired: setup::Wired,
    cfg: Config,
    overrides: ConfigOverrides,
    mut state: RunState,
    clock: &SystemClock,
) -> Result<(), String> {
    let mut runtime = ReplRuntime {
        wired,
        cfg,
        overrides,
    };
    let mut sink = StdoutSink;
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
            match handle(trimmed, &mut state, &mut runtime, clock, &mut sink).await {
                CmdAction::Continue => {}
                CmdAction::Quit => break,
                CmdAction::Reset(new_state) => state = *new_state,
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

fn prompt() {
    print!("> ");
    let _ = std::io::stdout().flush();
}
