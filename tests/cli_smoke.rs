//! Offline smoke: launch the line REPL with `MUAGENT_STORE=memory`, feed "/help\n/quit\n",
//! and assert it prints the help banner without touching the network.
//!
//! This doesn't require API keys — no user turn is submitted, so the model is
//! never invoked.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

fn bin() -> String {
    env!("CARGO_BIN_EXE_muagent").to_string()
}

async fn run_output(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn muagent");
    timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("cli did not exit in 10s")
        .expect("wait")
}

#[tokio::test]
async fn cli_help_and_quit_without_network() {
    let mut cmd = Command::new(bin());
    cmd.arg("repl");
    cmd.env("MUAGENT_STORE", "memory")
        // Dummy key so init doesn't complain (OpenRouter default provider is OK
        // without a key at setup time; only a real turn would need it).
        .env("MUAGENT_PROVIDER", "openrouter")
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn muagent");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/help\n/quit\n").await.unwrap();
    drop(stdin);

    let out = timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("cli did not exit in 5s")
        .expect("wait");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("μAgent"), "banner missing: {stdout}");
    assert!(stdout.contains("/help"), "help output missing: {stdout}");
    assert!(
        stdout.contains("/skills"),
        "skills command missing: {stdout}"
    );
    assert!(
        stdout.contains("net_http:unrestricted"),
        "net_http should be unrestricted by default: {stdout}"
    );
    assert!(
        stdout.contains("agent_md=on"),
        "agent_md banner missing: {stdout}"
    );
    assert!(
        out.status.success(),
        "cli exit non-zero; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn cli_can_disable_http_tool_banner() {
    let mut cmd = Command::new(bin());
    cmd.arg("repl");
    cmd.env("MUAGENT_STORE", "memory")
        .env("MUAGENT_PROVIDER", "openrouter")
        .env("MUAGENT_NET_HTTP", "off")
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn muagent");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/quit\n").await.unwrap();
    drop(stdin);

    let out = timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("cli did not exit in 5s")
        .expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("net_http:disabled"), "{stdout}");
    assert!(
        out.status.success(),
        "cli exit non-zero; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn cli_help_flag_prints_usage_and_exits_zero() {
    let out = run_output(&["--help"]).await;

    assert!(
        out.status.success(),
        "--help must exit 0; got {:?}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"));
    assert!(stdout.contains("--provider"));
    assert!(stdout.contains("--tui"));
    assert!(stdout.contains("--repl"));
    assert!(stdout.contains("sessions"));
    assert!(stdout.contains("--image"));
    assert!(stdout.contains("--thinking"));
    assert!(stdout.contains("--disable-tools"));
    assert!(stdout.contains("--log"));
    assert!(stdout.contains("MUAGENT_AGENT_MD"));
    assert!(!stdout.contains("MUAGENT_ALLOW_SH"));
}

#[tokio::test]
async fn cli_version_flag() {
    let out = run_output(&["--version"]).await;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("muagent "));
}

#[tokio::test]
async fn cli_unknown_flag_exits_nonzero() {
    let out = run_output(&["--nope"]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown argument"));
}
