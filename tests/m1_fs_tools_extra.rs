//! Offline: fs_list / fs_stat / fs_delete / fs_rename work end-to-end
//! against the Linux FS adapter, enforce root-escape denial, and return
//! useful errors.

use std::path::PathBuf;
use std::sync::Arc;

use muagent::adapters::{linux::LinuxFileSystem, AdapterBundle};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::prelude::*;
use serde_json::json;
use uuid::Uuid;

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-fs-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn wire(root: PathBuf) -> (Arc<CapabilityRegistry>, PathBuf) {
    let fs = Arc::new(LinuxFileSystem::new(vec![root.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);
    (registry, root)
}

async fn call(
    registry: Arc<CapabilityRegistry>,
    name: &str,
    args: serde_json::Value,
) -> ToolResult {
    let exec = DefaultToolExecutor::new(registry);
    let call = PendingCall::new(format!("c-{}", Uuid::new_v4()), name, args);
    exec.execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .expect("exec")
}

fn uri_of(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

#[tokio::test]
async fn fs_stat_file_returns_json() {
    let root = tmp();
    std::fs::write(root.join("a.txt"), b"hello").unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(
        reg.clone(),
        "fs_stat",
        json!({"uri": uri_of(&root.join("a.txt"))}),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    let v: serde_json::Value = serde_json::from_str(&r.text()).unwrap();
    assert_eq!(v["size"], 5);
    assert_eq!(v["is_dir"], false);
}

#[tokio::test]
async fn fs_list_shows_entries() {
    let root = tmp();
    std::fs::write(root.join("a"), b"1").unwrap();
    std::fs::write(root.join("b"), b"22").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(reg.clone(), "fs_list", json!({"uri": uri_of(&root)})).await;
    assert!(r.ok, "{:?}", r);
    assert!(r.text().contains("file"));
    assert!(r.text().contains("dir"));
    assert!(r.text().contains("/a"));
    assert!(r.text().contains("/b"));
    assert!(r.text().contains("/sub"));
}

#[tokio::test]
async fn fs_list_empty_dir() {
    let root = tmp();
    let (reg, _) = wire(root.clone());
    let r = call(reg.clone(), "fs_list", json!({"uri": uri_of(&root)})).await;
    assert!(r.ok);
    assert!(r.text().contains("empty"));
}

#[tokio::test]
async fn fs_delete_removes_file() {
    let root = tmp();
    let p = root.join("gone.txt");
    std::fs::write(&p, b"bye").unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(reg.clone(), "fs_delete", json!({"uri": uri_of(&p)})).await;
    assert!(r.ok, "{:?}", r);
    assert!(!p.exists());
}

#[tokio::test]
async fn fs_delete_removes_empty_directory_only() {
    let root = tmp();
    let p = root.join("empty");
    std::fs::create_dir(&p).unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(reg.clone(), "fs_delete", json!({"uri": uri_of(&p)})).await;
    assert!(r.ok, "{:?}", r);
    assert!(!p.exists());
}

#[tokio::test]
async fn fs_delete_rejects_non_empty_directory() {
    let root = tmp();
    let p = root.join("non-empty");
    std::fs::create_dir(&p).unwrap();
    std::fs::write(p.join("keep.txt"), b"keep").unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(reg.clone(), "fs_delete", json!({"uri": uri_of(&p)})).await;
    assert!(!r.ok, "{:?}", r);
    assert!(!r.retryable, "{:?}", r);
    assert!(r.text().contains("directory not empty"), "{:?}", r);
    assert!(p.join("keep.txt").exists());
}

#[tokio::test]
async fn fs_delete_can_remove_workspace_directory_when_empty() {
    let root = tmp();
    let (reg, _) = wire(root.clone());

    let r = call(reg.clone(), "fs_delete", json!({"uri": uri_of(&root)})).await;
    assert!(r.ok, "{:?}", r);
    assert!(!root.exists());
}

#[tokio::test]
async fn fs_rename_moves_file() {
    let root = tmp();
    let a = root.join("a.txt");
    let b = root.join("b.txt");
    std::fs::write(&a, b"content").unwrap();
    let (reg, _) = wire(root.clone());

    let r = call(
        reg.clone(),
        "fs_rename",
        json!({
            "from": uri_of(&a),
            "to":   uri_of(&b),
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    assert!(!a.exists());
    assert_eq!(std::fs::read(&b).unwrap(), b"content");
}

#[tokio::test]
async fn fs_rename_can_move_workspace_directory() {
    let root = tmp();
    let moved = root.with_file_name(format!(
        "{}-moved",
        root.file_name().unwrap().to_string_lossy()
    ));
    let (reg, _) = wire(root.clone());

    let r = call(
        reg.clone(),
        "fs_rename",
        json!({
            "from": uri_of(&root),
            "to":   uri_of(&moved),
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    assert!(!root.exists());
    assert!(moved.exists());
    let _ = std::fs::remove_dir(&moved);
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_create_dirs_follows_absolute_symlink_target() {
    use std::os::unix::fs::symlink;

    let root = tmp();
    let outside = tmp();
    symlink(&outside, root.join("link")).unwrap();
    let (reg, _) = wire(root.clone());
    let outside_target = outside.join("newdir").join("pwn.txt");

    let r = call(
        reg.clone(),
        "fs_write",
        json!({
            "uri": uri_of(&root.join("link").join("newdir").join("pwn.txt")),
            "content": "escaped",
            "create_dirs": true,
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    assert!(
        outside_target.exists(),
        "absolute file paths are not constrained by the configured workspace root"
    );
    assert_eq!(std::fs::read_to_string(outside_target).unwrap(), "escaped");
}

#[tokio::test]
async fn guard_rejects_dotdot_escape() {
    let root = tmp();
    let (reg, _) = wire(root.clone());

    let bad = "file:///tmp/../etc/passwd";
    for tool in ["fs_stat", "fs_list", "fs_delete"] {
        let r = call(reg.clone(), tool, json!({"uri": bad})).await;
        assert!(!r.ok, "{tool} must deny .. paths");
    }
    let r = call(
        reg.clone(),
        "fs_rename",
        json!({"from": bad, "to": uri_of(&root.join("x"))}),
    )
    .await;
    assert!(!r.ok);
}

#[tokio::test]
async fn fs_delete_errors_on_missing_file() {
    let root = tmp();
    let (reg, _) = wire(root.clone());
    let r = call(
        reg.clone(),
        "fs_delete",
        json!({"uri": uri_of(&root.join("nope.txt"))}),
    )
    .await;
    assert!(!r.ok);
    assert!(r.text().to_lowercase().contains("not found") || r.text().contains("not found"));
}
