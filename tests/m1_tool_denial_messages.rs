//! Offline: when a tool call is rejected (allowlist, unknown name, shell
//! denial), the executor returns a structured ToolResult the LLM can read
//! and act on — NOT a framework error. These are all recoverable conditions.

use std::path::PathBuf;
use std::sync::Arc;

use muagent::adapters::{
    linux::{LinuxFileSystem, LinuxProcessExec},
    AdapterBundle,
};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::prelude::*;
use serde_json::json;
use uuid::Uuid;

fn tmpdir() -> PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-deny-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn wire(allowlist: Option<Vec<&'static str>>) -> (Arc<CapabilityRegistry>, DefaultToolExecutor) {
    let tmp = tmpdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);

    let mut exec = DefaultToolExecutor::new(reg.clone());
    if let Some(xs) = allowlist {
        exec = exec.with_tool_allowlist(xs.iter().map(|s| s.to_string()));
    }
    (reg, exec)
}

async fn run(exec: &DefaultToolExecutor, name: &str, args: serde_json::Value) -> ToolResult {
    let ctx = ToolContext {
        session_id: Uuid::nil(),
        run_id: Uuid::nil(),
        turn: 0,
    };
    let call = PendingCall::new(Uuid::new_v4().to_string(), name, args);
    exec.execute(&call, &ctx, CancelToken::never())
        .await
        .expect("executor never returns Err")
}

// === Scenario 1: LLM calls a tool that was filtered out of the allowlist ===

#[tokio::test]
async fn read_only_session_rejects_write_with_friendly_message() {
    // Session is "read-only": only fs_read is allowed.
    let (_reg, exec) = wire(Some(vec!["fs_read"]));

    let r = run(
        &exec,
        "fs_write",
        json!({
            "uri": "file:///tmp/x.txt", "content": "hi"
        }),
    )
    .await;

    assert!(!r.ok, "must reject");
    assert!(
        !r.retryable,
        "LLM retrying the same tool won't help — not retryable"
    );
    assert!(r.text().contains("fs_write"));
    assert!(r.text().to_lowercase().contains("not available"));
    // Hint lists the actually-usable tools so the LLM can pick an alternative.
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(
        hint.contains("fs_read"),
        "hint should list available tools; got: {hint}"
    );
    assert!(!hint.contains("fs_write"));
}

// === Scenario 2: LLM hallucinates a tool name that was never registered ===

#[tokio::test]
async fn unknown_tool_returns_structured_result_not_framework_error() {
    let (_reg, exec) = wire(None);

    let r = run(&exec, "hallucinated_tool_name", json!({})).await;

    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.text().contains("hallucinated_tool_name"));
    assert!(
        r.text().to_lowercase().contains("does not exist")
            || r.text().to_lowercase().contains("not registered")
    );
    // Hint should suggest real tools.
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(hint.contains("fs_read"));
}

// === Scenario 3: sh_exec with a binary NOT in the adapter allowlist ===

#[tokio::test]
async fn sh_exec_denial_lists_allowed_binaries_in_hint() {
    let tmp = tmpdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["echo".into(), "cat".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    let exec = DefaultToolExecutor::new(reg);

    let r = run(&exec, "sh_exec", json!({"bin":"rm","args":["-rf","/"]})).await;

    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(
        r.text().to_lowercase().contains("not in the allowlist")
            || r.text().to_lowercase().contains("not in allowlist")
    );
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(
        hint.contains("echo"),
        "hint should list allowed bins; got: {hint}"
    );
    assert!(hint.contains("cat"));
    assert!(!hint.contains("rm"));
}

// === Scenario 4: allowlist + unknown — even if we pretend it's allowed,
// it still doesn't exist — should report "does not exist", not "not available". ===

#[tokio::test]
async fn allowlisted_but_unregistered_tool_still_unknown() {
    let (_reg, exec) = wire(Some(vec!["fs_read", "fake_tool"]));

    let r = run(&exec, "fake_tool", json!({})).await;
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(
        r.text().to_lowercase().contains("does not exist")
            || r.text().to_lowercase().contains("not registered")
    );
}

// === Scenario 5: allowlist of only fs_read → allowed tool still works ===

#[tokio::test]
async fn allowlisted_tool_still_runs_normally() {
    let tmp = tmpdir();
    let file = tmp.join("hi.txt");
    std::fs::write(&file, b"hello").unwrap();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    let exec = DefaultToolExecutor::new(reg).with_tool_allowlist(["fs_read".to_string()]);

    let r = run(
        &exec,
        "fs_read",
        json!({
            "uri": format!("file://{}", file.display())
        }),
    )
    .await;
    assert!(r.ok, "allowed tool should run; got: {:?}", r);
    assert_eq!(r.text(), "hello");
}

#[tokio::test]
async fn denylisted_tool_is_rejected_even_when_allowlisted() {
    let tmp = tmpdir();
    let file = tmp.join("hi.txt");
    std::fs::write(&file, b"hello").unwrap();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    let exec = DefaultToolExecutor::new(reg)
        .with_tool_allowlist(["fs_read".to_string()])
        .with_tool_denylist(["fs_read".to_string()]);

    let r = run(
        &exec,
        "fs_read",
        json!({
            "uri": format!("file://{}", file.display())
        }),
    )
    .await;
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.text().contains("fs_read"));
    assert!(r.text().to_lowercase().contains("not available"));
    assert_eq!(r.hint.as_deref(), Some("Available tools: "));
}
