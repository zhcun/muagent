//! Offline: fs_read paging (offset + truncation sentinel), fs_write content cap,
//! fs_read of a directory fails with helpful hint, sh_exec default timeout.

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

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-paging-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn wire_fs_only(root: PathBuf) -> Arc<CapabilityRegistry> {
    let fs = Arc::new(LinuxFileSystem::new(vec![root]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    reg
}

fn wire_with_sh(root: PathBuf) -> Arc<CapabilityRegistry> {
    let fs = Arc::new(LinuxFileSystem::new(vec![root]));
    let proc = Arc::new(LinuxProcessExec::new());
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    reg
}

async fn call(reg: Arc<CapabilityRegistry>, name: &str, args: serde_json::Value) -> ToolResult {
    let exec = DefaultToolExecutor::new(reg);
    let ctx = ToolContext {
        session_id: Uuid::nil(),
        run_id: Uuid::nil(),
        turn: 0,
    };
    let call = PendingCall::new(Uuid::new_v4().to_string(), name, args);
    exec.execute(&call, &ctx, CancelToken::never())
        .await
        .expect("exec")
}

fn uri_of(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

// ============ fs_read paging ============

#[tokio::test]
async fn small_file_read_whole_no_sentinel() {
    let root = tmp();
    let p = root.join("small.txt");
    std::fs::write(&p, b"hello").unwrap();
    let reg = wire_fs_only(root.clone());

    let r = call(reg, "fs_read", json!({"uri": uri_of(&p)})).await;
    assert!(r.ok, "{:?}", r);
    // No truncation sentinel because we read the whole thing.
    assert_eq!(r.text(), "hello");
    assert!(!r.text().contains("truncated"));
}

#[tokio::test]
async fn large_file_returns_truncation_sentinel_with_next_offset() {
    let root = tmp();
    let p = root.join("big.txt");
    // 200 KiB — larger than default 128 KiB per call.
    let data: Vec<u8> = (0..200_000u64).map(|i| b'a' + (i % 26) as u8).collect();
    std::fs::write(&p, &data).unwrap();
    let reg = wire_fs_only(root.clone());

    let r = call(reg, "fs_read", json!({"uri": uri_of(&p)})).await;
    assert!(r.ok);
    // Must mention read/total bytes and a resumable offset.
    assert!(
        r.text().contains("truncated"),
        "expected truncation sentinel; got last line: {}",
        r.text().lines().last().unwrap_or("")
    );
    // Default max_bytes is 50 KiB = 51200 (deliberately conservative so
    // a single fs_read can't eat the context budget).
    assert!(
        r.text().contains("51200"),
        "should show default bytes read (50 KiB)"
    );
    assert!(r.text().contains("200000"), "should show total file size");
    assert!(r.text().contains("offset=51200"), "should guide next call");
}

#[tokio::test]
async fn offset_continues_where_last_read_left_off() {
    let root = tmp();
    let p = root.join("seq.txt");
    // Fill with distinct segments so we can verify slicing.
    let mut data = Vec::new();
    for i in 0..4 {
        data.extend(std::iter::repeat(b'A' + i).take(40_000));
    }
    // Total = 160 000 bytes: 40k A, 40k B, 40k C, 40k D.
    std::fs::write(&p, &data).unwrap();
    let reg = wire_fs_only(root.clone());

    let r1 = call(
        reg.clone(),
        "fs_read",
        json!({
            "uri": uri_of(&p), "offset": 0, "max_bytes": 40_000
        }),
    )
    .await;
    assert!(r1.ok);
    assert!(r1.text().starts_with("AAAA"));
    assert!(r1.text().contains("truncated"));
    assert!(r1.text().contains("offset=40000"));

    let r2 = call(
        reg.clone(),
        "fs_read",
        json!({
            "uri": uri_of(&p), "offset": 40_000, "max_bytes": 40_000
        }),
    )
    .await;
    assert!(r2.ok);
    assert!(r2.text().starts_with("BBBB"));
    assert!(!r2.text().starts_with("AAAA"));
}

#[tokio::test]
async fn from_end_reads_tail_with_head_and_previous_chunk_guidance() {
    let root = tmp();
    let p = root.join("tail.txt");
    std::fs::write(&p, b"HEAD-middle-TAIL").unwrap();
    let reg = wire_fs_only(root.clone());

    let r = call(
        reg,
        "fs_read",
        json!({
            "uri": uri_of(&p), "from_end": true, "max_bytes": 4
        }),
    )
    .await;
    assert!(r.ok);
    assert!(r.text().starts_with("-- (tail;"));
    assert!(r.text().contains("offset=12"));
    assert!(r.text().contains("pass offset=0 for head"));
    assert!(r.text().contains("TAIL"));
    assert!(!r.text().contains("HEAD"));
}

#[tokio::test]
async fn offset_past_eof_returns_empty_sentinel() {
    let root = tmp();
    let p = root.join("tiny.txt");
    std::fs::write(&p, b"short").unwrap();
    let reg = wire_fs_only(root.clone());

    let r = call(reg, "fs_read", json!({"uri": uri_of(&p), "offset": 1000})).await;
    assert!(r.ok);
    assert!(r.text().contains("empty"));
    assert!(r.text().contains("total_bytes=5"));
}

#[tokio::test]
async fn fs_read_on_directory_rejects_with_hint() {
    let root = tmp();
    let reg = wire_fs_only(root.clone());
    let r = call(reg, "fs_read", json!({"uri": uri_of(&root)})).await;
    assert!(!r.ok);
    assert!(r.text().to_lowercase().contains("directory"));
    assert!(r.hint.as_deref().unwrap_or("").contains("fs_list"));
}

// ============ fs_write size cap ============

#[tokio::test]
async fn fs_write_rejects_oversized_content() {
    let root = tmp();
    let reg = wire_fs_only(root.clone());
    let huge = "x".repeat(2 * 1024 * 1024); // 2 MiB, over 1 MiB cap
    let r = call(
        reg,
        "fs_write",
        json!({
            "uri": uri_of(&root.join("big.txt")),
            "content": huge,
        }),
    )
    .await;
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.text().contains("1048576") || r.text().contains("1 MiB"));
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(
        hint.contains("append=true"),
        "hint should suggest chunking with append=true; got: {hint}"
    );
}

#[tokio::test]
async fn fs_write_allows_under_cap() {
    let root = tmp();
    let reg = wire_fs_only(root.clone());
    let payload = "x".repeat(500_000); // 500 KB
    let p = root.join("ok.txt");
    let r = call(
        reg,
        "fs_write",
        json!({
            "uri": uri_of(&p), "content": payload,
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 500_000);
}

// ============ sh_exec default timeout ============

#[tokio::test]
async fn sh_exec_default_timeout_is_30s_not_5s() {
    // Run a command that sleeps 6s without passing timeout_ms.
    // With the old 5s default, this would timeout. With 30s, it succeeds.
    let root = tmp();
    let reg = wire_with_sh(root);
    let t0 = std::time::Instant::now();
    let r = call(
        reg,
        "sh_exec",
        json!({
            "bin":"sh","args":["-c","sleep 6; printf done"]
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(
        r.ok,
        "sleep 6 should complete within default 30s timeout; got: {:?}",
        r
    );
    assert!(r.text().contains("done"));
    assert!(
        elapsed.as_secs() >= 6,
        "should have actually waited ~6s, got {:?}",
        elapsed
    );
    assert!(
        elapsed.as_secs() < 10,
        "should not have waited close to 30s"
    );
}

#[tokio::test]
async fn sh_exec_auto_background_can_be_polled() {
    let root = tmp();
    let reg = wire_with_sh(root);

    let r = call(
        reg.clone(),
        "sh_exec",
        json!({
            "bin":"sh",
            "args":["-c","printf start; sleep 1; printf done"],
            "timeout_ms":50,
            "hard_timeout_ms":5000,
        }),
    )
    .await;

    assert!(r.ok, "{:?}", r);
    assert!(r.text().contains("background job running"), "{:?}", r);
    assert!(r.text().contains("\"timeout_ms\":60000"), "{:?}", r);
    assert!(
        r.text().contains("Avoid repeated immediate polls"),
        "{:?}",
        r
    );
    let job_id = r.detail.as_ref().unwrap()["job_id"].as_str().unwrap();
    assert_eq!(r.detail.as_ref().unwrap()["recommended_wait_ms"], 60000);

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let r = call(
        reg,
        "sh_exec",
        json!({
            "action":"poll",
            "job_id":job_id,
        }),
    )
    .await;

    assert!(r.ok, "{:?}", r);
    assert_eq!(r.detail.as_ref().unwrap()["state"], "exited");
    assert!(r.text().contains("start"));
    assert!(r.text().contains("done"));
}

#[tokio::test]
async fn sh_exec_ignores_empty_job_action_when_starting_command() {
    let root = tmp();
    let reg = wire_with_sh(root);

    let r = call(
        reg,
        "sh_exec",
        json!({
            "action":"wait",
            "job_id":"",
            "bin":"sh",
            "args":["-c","printf ok"],
            "timeout_ms":30000,
        }),
    )
    .await;

    assert!(r.ok, "{:?}", r);
    assert!(r.text().contains("ok"));
}

#[tokio::test]
async fn sh_exec_sync_mode_still_times_out() {
    let root = tmp();
    let reg = wire_with_sh(root);

    let r = call(
        reg,
        "sh_exec",
        json!({
            "bin":"sh",
            "args":["-c","sleep 1; printf done"],
            "mode":"sync",
            "timeout_ms":50,
        }),
    )
    .await;

    assert!(!r.ok, "{:?}", r);
    assert!(r.retryable);
    assert!(r.text().contains("timeout"));
}

#[tokio::test]
async fn sh_exec_sync_returns_after_child_exit_even_if_descendant_holds_pipe() {
    let root = tmp();
    let reg = wire_with_sh(root);

    let run = call(
        reg,
        "sh_exec",
        json!({
            "bin":"sh",
            "args":["-c","printf done; sleep 2 &"],
            "mode":"sync",
            "timeout_ms":5000,
        }),
    );
    let r = tokio::time::timeout(std::time::Duration::from_millis(1200), run)
        .await
        .expect("sh_exec should not wait for a pipe leaked into a background child");

    assert!(r.ok, "{:?}", r);
    assert_eq!(r.detail.as_ref().unwrap()["exit"], 0);
    assert_eq!(r.detail.as_ref().unwrap()["stdout_bytes"], 4);
    assert_eq!(r.detail.as_ref().unwrap()["stderr_bytes"], 0);
    assert!(r.text().contains("exit 0"));
    assert!(r.text().contains("done"));
}

#[tokio::test]
async fn sh_exec_rejects_output_over_cap_without_buffering_all() {
    let root = tmp();
    let reg = wire_with_sh(root);

    let r = call(
        reg,
        "sh_exec",
        json!({
            "bin":"sh",
            "args":["-c","yes x | head -c 5000000"],
            "timeout_ms":10000,
        }),
    )
    .await;

    assert!(!r.ok, "{:?}", r);
    assert!(r.retryable);
    assert!(r.text().contains("output too large"));
}
