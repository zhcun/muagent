//! `fs.write`:写文件。
//!
//! 参数:
//! - `uri` (string, required)
//! - `content` (string, required)
//! - `append` (bool, default false)
//! - `create_dirs` (bool, default false)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::prelude::{
    parse_args, CancelToken, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool,
    ToolDescriptor, ToolErr, ToolOk,
};

use crate::adapters::{AdapterBundle, Uri, WriteOpts};

#[derive(Deserialize)]
struct Args {
    uri: String,
    content: String,
    #[serde(default)]
    append: bool,
    #[serde(default)]
    create_dirs: bool,
}

pub struct FsWrite {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

const MAX_CONTENT_BYTES: usize = 1024 * 1024; // 1 MiB per call

impl FsWrite {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_write".into(),
            description: "Write text content to a file. Use fs_edit for small targeted \
                 modifications; use fs_write for new files, append-only chunks, or \
                 intentional complete rewrites. OVERWRITES by default — pass \
                 append=true to append, create_dirs=true to mkdir parents. \
                 Per-call content cap: 1 MiB (1048576 bytes); larger writes \
                 must be chunked by the caller (use append=true for parts \
                 2+). Exceeding the cap returns a non-retryable deny, not \
                 silent truncation."
                .into(),
            schema_json: json!({
                "type":"object",
                "properties": {
                    "uri": {"type":"string"},
                    "content": {"type":"string"},
                    "append": {"type":"boolean","default":false},
                    "create_dirs": {"type":"boolean","default":false},
                },
                "required":["uri","content"],
            }),
            timeout: Duration::from_secs(10),
            max_out_tokens: 512,
            concurrency: Concurrency::Exclusive,
            side_effects: SideEffects::Mutating,
            idempotency: Idempotency::AtMostOnce,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsWrite {
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
        let uri = Uri::new(&a.uri);
        if uri.has_dotdot_escape() {
            return GuardOutcome::Deny {
                reason: "path contains `..`".into(),
                hint: Some("use absolute paths within a root".into()),
            };
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        let a: Args = parse_args(&args)?;

        if a.content.len() > MAX_CONTENT_BYTES {
            return Err(ToolErr::deny(format!(
                "content is {} bytes; per-call cap is {} (1 MiB)",
                a.content.len(),
                MAX_CONTENT_BYTES
            ))
            .with_hint("split into chunks and write with append=true"));
        }

        let uri = Uri::new(&a.uri);
        let before = self.bundle.fs.stat(&uri).await.ok();
        self.bundle
            .fs
            .write(
                &uri,
                a.content.as_bytes(),
                WriteOpts {
                    append: a.append,
                    create_dirs: a.create_dirs,
                },
            )
            .await
            .map_err(super::map_fs_err)?;
        let after = self.bundle.fs.stat(&uri).await.ok();

        let mode = if a.append { "append" } else { "overwrite" };
        let previous_size = before.as_ref().map(|m| m.size);
        let current_size = after.as_ref().map(|m| m.size);
        let previous = previous_size
            .map(|n| n.to_string())
            .unwrap_or_else(|| "missing_or_unknown".into());
        let current = current_size
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into());

        // Line count of what we just wrote — append-mode contributes new
        // lines on top of the existing file; overwrite mode replaces the
        // file outright so this represents the new total. We don't claim
        // lines_removed for overwrite (would require re-reading the old
        // file) and report 0 for append/new-file paths where it's known.
        let lines_added = count_lines_in_content(&a.content);
        let known_lines_removed: Option<usize> = if a.append || previous_size.is_none() {
            Some(0)
        } else {
            None
        };

        Ok(ToolOk::text(format!(
            "fs_write ok: wrote {} bytes to {} (mode={mode}, previous_size={previous}, current_size={current})",
            a.content.len(),
            uri.as_str()
        ))
        .with_detail(json!({
            "uri": uri.as_str(),
            "mode": mode,
            "bytes_written": a.content.len(),
            "previous_size": previous_size,
            "current_size": current_size,
            "lines_added": lines_added,
            "lines_removed": known_lines_removed,
        })))
    }
}

/// Count "logical lines" in a payload buffer. An empty content has 0 lines;
/// a buffer without a trailing newline still counts the partial last line.
/// Used purely for the diff-stats summary in `ToolResult.detail`.
fn count_lines_in_content(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|b| *b == b'\n').count();
    if s.ends_with('\n') {
        nl
    } else {
        nl + 1
    }
}
