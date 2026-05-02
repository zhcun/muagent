//! `sh.exec`:运行 shell 命令。只在 bundle 有 ProcessExec adapter 时可注册。
//!
//! 参数:
//! - `bin` (string, required):binary 名称或路径
//! - `args` (array of strings, optional)
//! - `stdin` (string, optional)
//! - `timeout_ms` (integer, optional, default 30000)

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::cancel::CancelToken;
use crate::core::prelude::{
    parse_args, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool, ToolDescriptor, ToolErr,
    ToolOk,
};

use crate::adapters::{AdapterBundle, CmdSpec};

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    bin: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    hard_timeout_ms: Option<u64>,
    #[serde(default)]
    background_after_ms: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn max_recommended_job_wait_ms() -> u64 {
    std::env::var("MUAGENT_SH_EXEC_MAX_RECOMMENDED_WAIT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms >= 60_000)
        .unwrap_or(60 * 60 * 1000)
}

fn recommended_job_wait_ms(snap: &crate::adapters::ExecJobSnapshot) -> u64 {
    let elapsed = snap.elapsed.as_secs();
    let wait_ms = if elapsed < 2 * 60 {
        60_000
    } else if elapsed < 10 * 60 {
        2 * 60 * 1000
    } else if elapsed < 30 * 60 {
        5 * 60 * 1000
    } else if elapsed < 60 * 60 {
        10 * 60 * 1000
    } else if elapsed < 2 * 60 * 60 {
        30 * 60 * 1000
    } else {
        60 * 60 * 1000
    };
    wait_ms.min(max_recommended_job_wait_ms())
}

fn default_hard_timeout_ms() -> u64 {
    std::env::var("MUAGENT_SH_EXEC_DEFAULT_HARD_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(60 * 60 * 1000)
}

fn outer_timeout() -> Duration {
    const DEFAULT_SECS: u64 = 24 * 60 * 60;
    std::env::var("MUAGENT_SH_EXEC_OUTER_TIMEOUT_SEC")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_SECS))
}

pub struct ShExec {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl ShExec {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "sh_exec".into(),
            description: "Execute a shell binary. BLOCKS until process \
                 exits or timeout_ms fires (whichever is first); on timeout \
                 the process is killed with SIGKILL. \
                 For multi-line shell scripts or commands with complex \
                 quoting/heredocs, prefer putting the full script in stdin \
                 and leaving args empty; use args for simple argv values or \
                 wrapper flags only. \
                 In default mode=auto, timeout_ms is the foreground wait \
                 window (default 30000 ms), not a reason to throw away \
                 partial output. If the command is still running after that \
                 window, it is kept in the background and the result returns \
                 a job_id to wait/poll/kill with this same tool. \
                 Prefer action=wait for background jobs; it returns as soon \
                 as the job exits. If timeout_ms is omitted on wait, sh_exec \
                 uses an adaptive backoff starting at 60000 ms and growing up \
                 to 30-60 minutes for very long jobs. Avoid tight polling. \
                 hard_timeout_ms is the max process runtime before kill \
                 (default 1 hour). Use mode=sync to get old blocking \
                 behavior where timeout_ms kills the process. \
                 Output cap: 4 MiB combined stdout+stderr; beyond that, the \
                 sync call returns output_too_large. Background jobs keep a \
                 tail buffer instead of losing all output. \
                 If you don't know how long something will take OR it may \
                 prompt for input, rely on mode=auto and poll the returned \
                 job_id."
                .into(),
            schema_json: json!({
                "type":"object",
                "properties": {
                    "bin":   {"type":"string","description":"Binary name or path to execute."},
                    "args":  {"type":"array","items":{"type":"string"},"default":[],
                              "description":"Simple argv values for the binary. For shell scripts or heredocs, prefer stdin."},
                    "stdin": {"type":"string","description":"Optional stdin payload; process's stdin closes after. Prefer this for multi-line shell scripts."},
                    "timeout_ms":{"type":"integer","minimum":1,"default":30000,
                                  "description":"In mode=auto, wait this many ms before returning a background job_id if still running. In action=wait, omit this to use adaptive backoff, or pass the recommended_wait_ms from the previous running result. In mode=sync, kill after this many ms."},
                    "hard_timeout_ms":{"type":"integer","minimum":1,"default":3600000,
                                  "description":"In mode=auto, kill the background job after this many ms. Defaults to 1 hour."},
                    "background_after_ms":{"type":"integer","minimum":1,
                                  "description":"Optional foreground wait override before returning a running job_id."},
                    "mode":{"type":"string","enum":["auto","sync"],"default":"auto",
                                  "description":"auto keeps long commands in the background; sync blocks until exit or timeout_ms kills the process."},
                    "action":{"type":"string","enum":["poll","wait","kill"],
                                  "description":"Only set this with a non-empty job_id to operate on an existing background job. Omit action when starting a new command."},
                    "job_id":{"type":"string","description":"Background job id returned by a previous sh_exec call."},
                },
                "required":[],
            }),
            // Outer safety timer. CmdSpec::timeout is the user-visible
            // process timeout; this is a last-resort framework cap.
            timeout: outer_timeout(),
            max_out_tokens: 4096,
            concurrency: Concurrency::Exclusive,
            side_effects: SideEffects::Destructive,
            idempotency: Idempotency::AtMostOnce,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for ShExec {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    fn guard(&self, args: &Value) -> GuardOutcome {
        let a: Args = match parse_args(args) {
            Ok(a) => a,
            Err(e) => {
                return GuardOutcome::Deny {
                    reason: e.msg,
                    hint: e.hint,
                }
            }
        };
        // Guard only does cheap pure checks; execution errors are reported by
        // the ProcessExec adapter.
        if a.action.is_some() && !a.job_id.as_deref().unwrap_or_default().is_empty() {
            return GuardOutcome::Allow;
        }
        if a.bin.as_deref().unwrap_or_default().is_empty() {
            return GuardOutcome::Deny {
                reason: "empty `bin`".into(),
                hint: None,
            };
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        let proc = self
            .bundle
            .proc
            .as_ref()
            .ok_or_else(|| ToolErr::deny("sh.exec not available: no ProcessExec adapter"))?;

        let a: Args = parse_args(&args)?;

        if let Some(action) = a
            .action
            .as_deref()
            .filter(|_| !a.job_id.as_deref().unwrap_or_default().is_empty())
        {
            let job_id = a.job_id.as_deref().unwrap_or_default();
            return match action {
                "poll" => {
                    let snap = proc.poll(job_id).await.map_err(map_exec_err)?;
                    Ok(snapshot_ok(snap))
                }
                "wait" => {
                    let wait_ms = match a.timeout_ms {
                        Some(wait_ms) => wait_ms,
                        None => {
                            let snap = proc.poll(job_id).await.map_err(map_exec_err)?;
                            if snap.state != crate::adapters::ExecJobState::Running {
                                return Ok(snapshot_ok(snap));
                            }
                            recommended_job_wait_ms(&snap)
                        }
                    };
                    let snap = wait_for_job(proc.as_ref(), job_id, wait_ms, cancel).await?;
                    Ok(snapshot_ok(snap))
                }
                "kill" => {
                    let snap = proc.kill(job_id).await.map_err(map_exec_err)?;
                    Ok(snapshot_ok(snap))
                }
                other => Err(ToolErr::deny(format!("unknown sh_exec action `{other}`"))
                    .with_hint("use action=poll, wait, or kill")),
            };
        }

        let bin = a.bin.as_deref().unwrap_or_default();
        let mut spec = CmdSpec::new(bin, a.args);
        spec.cwd = first_file_root_path(&self.bundle);
        spec.stdin = a.stdin.map(|s| s.into_bytes());
        spec.max_output_bytes = 4 * 1024 * 1024;

        let mode = a.mode.as_deref().unwrap_or("auto");
        if mode == "sync" {
            spec.timeout = Duration::from_millis(a.timeout_ms.unwrap_or_else(default_timeout_ms));
            let out = proc.run(&spec, cancel).await.map_err(map_exec_err)?;
            return Ok(exit_ok(out));
        }

        spec.timeout =
            Duration::from_millis(a.hard_timeout_ms.unwrap_or_else(default_hard_timeout_ms));
        let foreground_ms = a
            .background_after_ms
            .or(a.timeout_ms)
            .unwrap_or_else(default_timeout_ms);
        let snap = proc.spawn(&spec).await.map_err(map_exec_err)?;
        let snap = wait_for_job(proc.as_ref(), &snap.job_id, foreground_ms, cancel).await?;
        if snap.state == crate::adapters::ExecJobState::Running {
            return Ok(snapshot_ok(snap));
        }
        if snap.output_truncated {
            return Err(ToolErr::retry(snapshot_text(&snap, true))
                .with_hint("output too large; reduce output or redirect it to a file"));
        }
        return Ok(snapshot_ok(snap));
    }
}

fn exit_ok(out: crate::adapters::ExitOut) -> ToolOk {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let content = format!(
        "exit {}{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.code,
        if out.truncated { " (truncated)" } else { "" },
        stdout,
        stderr,
    );

    ToolOk {
        content: crate::core::types::Content::Text(content),
        detail: Some(json!({
            "exit": out.code,
            "truncated": out.truncated,
        })),
    }
}

async fn wait_for_job(
    proc: &dyn crate::adapters::ProcessExec,
    job_id: &str,
    wait_ms: u64,
    cancel: CancelToken,
) -> Result<crate::adapters::ExecJobSnapshot, ToolErr> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let snap = proc.poll(job_id).await.map_err(|e| {
            ToolErr::retry(format!("failed to poll sh job `{job_id}`: {e}"))
                .with_hint("check the job_id returned by sh_exec")
        })?;
        if snap.state != crate::adapters::ExecJobState::Running {
            return Ok(snap);
        }
        if cancel.triggered() || Instant::now() >= deadline {
            return Ok(snap);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn snapshot_ok(snap: crate::adapters::ExecJobSnapshot) -> ToolOk {
    let state = format!("{:?}", snap.state).to_lowercase();
    let recommended_wait_ms = if snap.state == crate::adapters::ExecJobState::Running {
        Some(recommended_job_wait_ms(&snap))
    } else {
        None
    };
    ToolOk {
        content: crate::core::types::Content::Text(snapshot_text(&snap, false)),
        detail: Some(json!({
            "job_id": snap.job_id,
            "state": state,
            "exit": snap.code,
            "stdout_bytes": snap.stdout_bytes,
            "stderr_bytes": snap.stderr_bytes,
            "output_truncated": snap.output_truncated,
            "elapsed_ms": snap.elapsed.as_millis() as u64,
            "command": snap.command,
            "error": snap.error,
            "recommended_wait_ms": recommended_wait_ms,
        })),
    }
}

fn snapshot_text(snap: &crate::adapters::ExecJobSnapshot, output_too_large: bool) -> String {
    let stdout = String::from_utf8_lossy(&snap.stdout_tail);
    let stderr = String::from_utf8_lossy(&snap.stderr_tail);
    let elapsed = snap.elapsed.as_secs_f64();
    if snap.state == crate::adapters::ExecJobState::Running {
        let recommended_wait_ms = recommended_job_wait_ms(snap);
        let recommended_wait_sec = recommended_wait_ms / 1000;
        return format!(
            "background job running: {job_id}\nstate: running\nelapsed_sec: {elapsed:.1}\nstdout_bytes: {stdout_bytes}\nstderr_bytes: {stderr_bytes}\noutput_truncated: {truncated}\nrecommended_wait_ms: {recommended_wait_ms}\n\nNext check: prefer wait; it returns early if the job exits. Avoid repeated immediate polls. Suggested wait is {recommended_wait_sec}s and grows for long-running jobs.\nWait with:\n{{\"action\":\"wait\",\"job_id\":\"{job_id}\",\"timeout_ms\":{recommended_wait_ms}}}\nPoll once if you only need an immediate progress snapshot:\n{{\"action\":\"poll\",\"job_id\":\"{job_id}\"}}\nKill with:\n{{\"action\":\"kill\",\"job_id\":\"{job_id}\"}}\n\n--- stdout tail ---\n{stdout}\n--- stderr tail ---\n{stderr}",
            job_id = snap.job_id,
            stdout_bytes = snap.stdout_bytes,
            stderr_bytes = snap.stderr_bytes,
            truncated = snap.output_truncated,
        );
    }

    let state = format!("{:?}", snap.state).to_lowercase();
    let exit = snap.code.unwrap_or(-1);
    let prefix = if output_too_large {
        "output too large\n"
    } else {
        ""
    };
    format!(
        "{prefix}job_id: {job_id}\nstate: {state}\nexit {exit}\nelapsed_sec: {elapsed:.1}\nstdout_bytes: {stdout_bytes}\nstderr_bytes: {stderr_bytes}\noutput_truncated: {truncated}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        job_id = snap.job_id,
        stdout_bytes = snap.stdout_bytes,
        stderr_bytes = snap.stderr_bytes,
        truncated = snap.output_truncated,
    )
}

fn first_file_root_path(bundle: &AdapterBundle) -> Option<String> {
    bundle.fs.roots().into_iter().find_map(|root| {
        let path = root.uri_prefix.strip_prefix("file://")?;
        let trimmed = path.trim_end_matches('/');
        Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        })
    })
}

fn map_exec_err(e: crate::adapters::ExecErr) -> ToolErr {
    use crate::adapters::ExecErr::*;
    match e {
        NotAvailable => ToolErr::deny("sh.exec not available on this platform"),
        Timeout => ToolErr::retry("timeout").with_hint("try smaller input or longer timeout_ms"),
        Killed => ToolErr::retry("killed"),
        Io(s) => ToolErr::retry(s),
        OutputTooLarge => ToolErr::retry("output too large")
            .with_hint("reduce output or use fs_read with max_bytes"),
    }
}
