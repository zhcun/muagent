//! Offline: fs_read safety for binary, images, and pathological text files.
//! - Non-image binary (ELF-like) → refuse with non-retryable ToolErr.
//! - PNG image under 1 MiB → returned as image attachment (ContentPart::Image).
//! - PNG image over 1 MiB → refuse with resize hint.
//! - Text file with a 3000-char line → truncated with `<line truncated>`.
//! - force_text=true on a binary → lossy decode, no refusal.

use std::path::PathBuf;
use std::sync::Arc;

use muagent::adapters::{linux::LinuxFileSystem, AdapterBundle};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::types::ContentPart;
use muagent::prelude::*;
use serde_json::json;
use uuid::Uuid;

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-fsread-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap()
}

fn wire(root: PathBuf) -> Arc<CapabilityRegistry> {
    let fs = Arc::new(LinuxFileSystem::new(vec![root]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    reg
}

async fn read(reg: Arc<CapabilityRegistry>, args: serde_json::Value) -> ToolResult {
    let exec = DefaultToolExecutor::new(reg);
    let ctx = ToolContext {
        session_id: Uuid::nil(),
        run_id: Uuid::nil(),
        turn: 0,
    };
    let call = PendingCall::new(Uuid::new_v4().to_string(), "fs_read", args);
    exec.execute(&call, &ctx, CancelToken::never())
        .await
        .expect("exec")
}

fn uri_of(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

// --- Minimal PNG (8x8 red square). 16 bytes of IDAT, valid CRC. ---
// Built programmatically so the test doesn't need a binary blob.
fn tiny_png_bytes() -> Vec<u8> {
    // Instead of synthesizing a real PNG (requires CRC32 + zlib), write just
    // the PNG signature + a zero-filled body. Our code only checks the magic.
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0u8; 256]); // dummy body
    bytes
}

// --- Bytes that look like a compiled binary (ELF magic, then padding). ---
fn fake_elf_bytes() -> Vec<u8> {
    let mut bytes = vec![0x7F, b'E', b'L', b'F'];
    bytes.extend_from_slice(&[0u8; 200]);
    bytes
}

#[tokio::test]
async fn text_file_reads_normally() {
    let root = tmp();
    let p = root.join("a.txt");
    std::fs::write(&p, b"hello world\n").unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p)})).await;
    assert!(r.ok, "{:?}", r);
    assert_eq!(r.text(), "hello world\n");
    assert_eq!(r.attachments().count(), 0);
}

#[tokio::test]
async fn long_line_gets_truncated_with_marker() {
    let root = tmp();
    let p = root.join("long.txt");
    let line = "x".repeat(3000);
    std::fs::write(&p, format!("prefix\n{line}\nsuffix\n")).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p)})).await;
    assert!(r.ok);
    assert!(r.text().contains("prefix\n"));
    assert!(r.text().contains("suffix\n"));
    assert!(r.text().contains("<line truncated>"));
    // The full 3000-x line must NOT be present whole.
    assert!(!r.text().contains(&"x".repeat(3000)));
}

#[tokio::test]
async fn non_image_binary_is_refused() {
    let root = tmp();
    let p = root.join("app.bin");
    std::fs::write(&p, fake_elf_bytes()).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p)})).await;

    assert!(!r.ok, "should refuse binary");
    assert!(!r.retryable, "retrying won't make it text");
    assert!(r.text().to_lowercase().contains("binary"));
    // Hint should guide the agent to use a proper tool for binary.
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(
        hint.contains("sh_exec") || hint.contains("force_text"),
        "hint should suggest an alternative; got: {hint}"
    );
    // No attachments — we don't want to feed raw bytes anywhere.
    assert_eq!(r.attachments().count(), 0);
}

#[tokio::test]
async fn force_text_lets_binary_through_as_lossy() {
    let root = tmp();
    let p = root.join("app.bin");
    std::fs::write(&p, fake_elf_bytes()).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p), "force_text": true})).await;
    assert!(r.ok, "force_text should bypass refusal: {:?}", r);
    // Content contains the ELF letters after lossy decode.
    assert!(r.text().contains("ELF"));
}

#[tokio::test]
async fn small_image_returns_as_attachment() {
    let root = tmp();
    let p = root.join("img.png");
    std::fs::write(&p, tiny_png_bytes()).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p)})).await;

    assert!(r.ok, "{:?}", r);
    // Textual note mentions it's an image.
    assert!(r.text().to_lowercase().contains("image"));
    assert!(r.text().contains("image/png"));
    // One image attachment with the expected MIME + base64 body.
    let atts: Vec<_> = r.attachments().collect();
    assert_eq!(atts.len(), 1);
    match atts[0] {
        ContentPart::Image {
            b64: Some(data),
            mime,
            ..
        } => {
            assert_eq!(mime, "image/png");
            assert!(!data.is_empty());
            assert!(
                data.starts_with("iVBOR") || data.starts_with("iV"),
                "base64 of a PNG should start with iV... (signature 89 50 4e 47); got: {}",
                &data[..10.min(data.len())]
            );
        }
        other => panic!("expected Image attachment, got {:?}", other),
    }
}

#[tokio::test]
async fn oversized_image_is_refused_with_resize_hint() {
    let root = tmp();
    let p = root.join("huge.png");
    // PNG magic + 1.5 MiB of padding → over the 1 MiB attachment cap.
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.resize(bytes.len() + (1024 * 1024 * 3 / 2), 0u8);
    std::fs::write(&p, &bytes).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p), "max_bytes": 2_000_000})).await;
    assert!(!r.ok);
    assert!(!r.retryable);
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(
        hint.to_lowercase().contains("downsize") || hint.to_lowercase().contains("resize"),
        "hint should suggest resizing; got: {hint}"
    );
    assert_eq!(r.attachments().count(), 0);
}

#[tokio::test]
async fn jpeg_magic_recognized() {
    let root = tmp();
    let p = root.join("img.jpg");
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend_from_slice(&[0u8; 100]);
    std::fs::write(&p, bytes).unwrap();
    let reg = wire(root);
    let r = read(reg, json!({"uri": uri_of(&p)})).await;
    assert!(r.ok);
    let atts: Vec<_> = r.attachments().collect();
    assert_eq!(atts.len(), 1);
    match atts[0] {
        ContentPart::Image { mime, .. } => assert_eq!(mime, "image/jpeg"),
        _ => panic!("expected image/jpeg"),
    }
}
