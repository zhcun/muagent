//! M1-P1 集成测试:Runner + MockModel + 真 LinuxFileSystem + 真 ProcessExec + 内置 tools
//!
//! 目标:端到端证明 Adapter + 内置 tool + FSM + Shell ToolExecutor 全链路通。

use std::sync::Arc;

use muagent::adapters::{
    linux::{LinuxFileSystem, LinuxProcessExec},
    AdapterBundle,
};
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::{Content, Message};
use muagent::prelude::*;
use muagent::storage::MemorySessionStore;
use serde_json::json;
use uuid::Uuid;

fn call(id: &str, name: &str, args: serde_json::Value) -> PendingCall {
    PendingCall::new(id, name, args)
}

// -------- Helpers --------

async fn drive_until_terminal(runner: &Runner, state: &mut RunState, max: usize) {
    for _ in 0..max {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            break;
        }
        runner.step(state).await.unwrap();
    }
}

// -------- Test 1:fs.write + fs.read round-trip --------

#[tokio::test]
async fn m1_fs_write_and_read() {
    // Set up tmpdir as the only root
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());

    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle.clone());

    // Verify tools registered
    assert!(registry.resolve("fs_read").is_some());
    assert!(registry.resolve("fs_edit").is_some());
    assert!(registry.resolve("fs_write").is_some());
    // sh.exec not registered because bundle has no proc adapter
    assert!(registry.resolve("sh_exec").is_none());

    // Write then read
    let uri = format!("file://{}/hello.txt", tmp.display());

    let model = Arc::new(CannedModel::new(vec![
        // turn 1: write
        reply::with_calls(
            "I'll write the file",
            vec![call(
                "w1",
                "fs_write",
                json!({
                    "uri": uri.clone(),
                    "content": "hello world",
                }),
            )],
        ),
        // turn 2: read
        reply::with_calls(
            "now reading",
            vec![call("r1", "fs_read", json!({ "uri": uri.clone() }))],
        ),
        // turn 3: final
        reply::text("file contents: hello world"),
    ]));

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);

    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("write and read a file"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 30).await;

    assert!(matches!(state.step, Step::Done { .. }));

    // Verify file actually written to disk
    let abs = tmp.join("hello.txt");
    let data = std::fs::read_to_string(&abs).unwrap();
    assert_eq!(data, "hello world");

    // History should contain 2 tool_results, both ok
    let tr_count = state
        .history
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(tr_count, 2);
    for m in &state.history {
        if let Message::ToolResult { result, .. } = m {
            assert!(
                result.ok,
                "tool result should be ok, got: {}",
                result.text()
            );
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&abs);
    let _ = std::fs::remove_dir(&tmp);
}

#[tokio::test]
async fn m1_fs_edit_replaces_unique_text_with_line_log() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    let path = tmp.join("edit.txt");
    std::fs::write(&path, "alpha\nneedle\nomega\n").unwrap();
    let uri = format!("file://{}", path.display());
    let c = call(
        "e1",
        "fs_edit",
        json!({
            "uri": uri,
            "old_text": "needle",
            "new_text": "changed",
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(r.ok, "{:?}", r);
    assert!(r.text().contains("fs_edit ok"));
    assert!(r.text().contains("first_changed_line=2"));
    assert!(r.text().contains("diff:"));
    assert!(r.text().contains("-2 needle"));
    assert!(r.text().contains("+2 changed"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "alpha\nchanged\nomega\n"
    );
    assert_eq!(r.detail.as_ref().unwrap()["replacements"], 1);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&tmp);
}

#[tokio::test]
async fn m1_fs_edit_rejects_duplicate_match_without_writing() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    let path = tmp.join("dup.txt");
    std::fs::write(&path, "x\nx\n").unwrap();
    let c = call(
        "e1",
        "fs_edit",
        json!({
            "uri": format!("file://{}", path.display()),
            "old_text": "x",
            "new_text": "y",
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok, "{:?}", r);
    assert!(r.text().contains("found 2 occurrences"));
    assert!(r.text().contains("lines 1,2"));
    assert!(r
        .hint
        .as_deref()
        .unwrap_or("")
        .contains("surrounding context"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nx\n");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&tmp);
}

#[tokio::test]
async fn m1_fs_edit_dry_run_validates_without_writing() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    let path = tmp.join("dry.txt");
    std::fs::write(&path, "x\nx\n").unwrap();
    let c = call(
        "e1",
        "fs_edit",
        json!({
            "uri": format!("file://{}", path.display()),
            "old_text": "x",
            "new_text": "y",
            "replace_all": true,
            "expected_replacements": 2,
            "dry_run": true,
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(r.ok, "{:?}", r);
    assert!(r.text().contains("fs_edit dry_run"));
    assert!(r.text().contains("-1 x"));
    assert!(r.text().contains("+1 y"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nx\n");
    assert_eq!(r.detail.as_ref().unwrap()["dry_run"], true);
    assert_eq!(r.detail.as_ref().unwrap()["replacements"], 2);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&tmp);
}

// -------- Test 2:fs.write 对越权路径 → ToolResult(err) --------

#[tokio::test]
async fn m1_fs_write_escape_root_denied() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());

    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    // Write outside root
    let c = call(
        "x",
        "fs_write",
        json!({
            "uri": "file:///etc/passwd",
            "content": "hack",
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();
    assert!(!r.ok);
    assert!(!r.retryable); // 越权硬错,不重试
    assert!(r.text().contains("outside all roots"));

    let _ = std::fs::remove_dir(&tmp);
}

// -------- Test 3:sh.exec echo --------

#[tokio::test]
async fn m1_sh_exec_echo() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["echo".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    // sh.exec should now be registered
    assert!(registry.resolve("sh_exec").is_some());

    let executor = DefaultToolExecutor::new(registry);
    let c = call(
        "s1",
        "sh_exec",
        json!({
            "bin": "echo",
            "args": ["hello", "from", "tool"],
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();
    assert!(r.ok);
    assert!(r.text().contains("hello from tool"));
    assert!(r.text().contains("exit 0"));

    let _ = std::fs::remove_dir(&tmp);
}

#[tokio::test]
async fn m1_sh_exec_nonzero_exit_is_still_tool_output() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["sh".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    let c = call(
        "s1",
        "sh_exec",
        json!({
            "bin": "sh",
            "args": ["-c", "printf failed; exit 7"],
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(r.ok, "{:?}", r);
    assert_eq!(r.detail.as_ref().unwrap()["exit"], 7);
    assert!(r.text().contains("exit 7"));
    assert!(r.text().contains("failed"));

    let _ = std::fs::remove_dir(&tmp);
}

// -------- Test 4:sh.exec 不在 allowlist → err --------

#[tokio::test]
async fn m1_sh_exec_not_in_allowlist() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["echo".into()])); // no "rm"
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    let executor = DefaultToolExecutor::new(registry);

    let c = call(
        "s2",
        "sh_exec",
        json!({
            "bin": "rm",
            "args": ["-rf", "/tmp/anything"],
        }),
    );
    let r = executor
        .execute(&c, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.text().contains("allowlist"));

    let _ = std::fs::remove_dir(&tmp);
}

// -------- small test helper --------

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let name = format!("muagent-m1-{}", Uuid::new_v4());
    let p = base.join(name);
    std::fs::create_dir_all(&p).unwrap();
    p
}
