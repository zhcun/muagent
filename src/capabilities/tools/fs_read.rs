//! `fs_read` — read a file (or a slice of one) with paginated offset + cap.
//!
//! Design notes:
//! - `max_bytes` defaults to 131 072 (128 KiB), hard cap at 4 MiB (adapter-
//!   level abs cap is 16 MiB).
//! - `offset` lets the agent read the next chunk of a large file without
//!   re-reading from the start. `from_end=true` reads the tail chunk.
//!   Combined with the returned `total_bytes`, the agent always knows where
//!   to pick up.
//! - Truncation is **explicit**: when the response is shorter than the file,
//!   the tool appends a one-line sentinel the agent can see:
//!   `-- (truncated; read N of M bytes; pass offset=N for more) --`
//! - **Binary detection**: if the first 8 KiB of the slice contain a null
//!   byte, the content is treated as binary. Instead of returning garbage
//!   UTF-8-lossy output, we return a short structured note with file size
//!   and a hex preview of the first 64 bytes. The agent can ask for text
//!   semantics explicitly via `force_text=true` if it really wants the
//!   lossy decode.
//! - **Images**: recognized image files are returned as image attachments
//!   for the next model call. Do not use `force_text=true` for OCR/visual
//!   inspection tasks unless lossy PNG/JPEG bytes are explicitly needed.
//! - **Per-line truncation**: any single line exceeding 2000 chars is
//!   cut off and tagged with ` <line truncated>`. Long minified JSON /
//!   base64 blobs won't explode the tool output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::prelude::{
    parse_args, CancelToken, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool,
    ToolDescriptor, ToolErr, ToolOk,
};

use crate::adapters::{AdapterBundle, ReadOpts, Uri};

/// Typed args. The `schema_json` declared in `descriptor()` MUST mirror this
/// shape — keeping them next to each other avoids drift.
#[derive(Deserialize)]
struct Args {
    uri: String,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
    #[serde(default)]
    from_end: bool,
    #[serde(default)]
    force_text: bool,
}

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

// Defaults sized for "leaves context for the rest of the session":
// ≈ 50 KiB ≈ 10-15k tokens depending on tokenizer. Agent can always
// paginate with offset for more. Hard cap keeps a single reckless call
// from eating the whole context.
const DEFAULT_MAX_BYTES: u64 = 50 * 1024; // 50 KiB (~ 10-15k tokens)
const HARD_CAP_BYTES: u64 = 256 * 1024; // 256 KiB (~ 50-80k tokens)
const MAX_LINE_CHARS: usize = 2000; // per-line truncation threshold
const BINARY_SNIFF_BYTES: usize = 8192; // scan first 8 KiB for null bytes

/// Max image file size we'll include as an attachment. Images below this
/// comfortably stay under 10k tokens on every provider's billing formula
/// (Anthropic: (w*h)/750; OpenAI: 85 + 170·ceil(w/512)·ceil(h/512);
/// Gemini: flat 258). A 1 MiB cap covers typical screenshots and web
/// images at decent quality while rejecting 4K photos that'd blow past
/// 10k tokens.
const IMAGE_MAX_BYTES: usize = 1024 * 1024;

pub struct FsRead {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsRead {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_read".into(),
            description: "Read a file as text. Defaults: max_bytes=51200 (50 KiB, \
                 ≈ 10–15k tokens); hard cap 262144 (256 KiB) per call. \
                 These caps are deliberately small to leave room in \
                 context for the rest of the session — if you need more, \
                 paginate with `offset`, don't ask for a huge chunk. \
                 Head is the default (`offset=0`); pass `from_end=true` \
                 to read the tail chunk. \
                 Lines longer than 2000 chars get truncated with a \
                 `<line truncated>` marker. Binary files (null bytes in \
                 the first 8 KiB) return a short metadata note + hex \
                 preview instead of UTF-8 garbage — pass `force_text=true` \
                 to decode lossy text anyway. \
                 Image files are returned as image attachments for visual \
                 inspection; do not use `force_text=true` for image/OCR \
                 tasks unless you explicitly need lossy PNG/JPEG bytes. \
                 When the file has more data than was returned, the \
                 response appends `-- (truncated; ...)` with the exact \
                 `offset` to pass next. \
                 Errors: directory URI → use fs_list; missing file → \
                 structured not_found; offset past EOF → empty + sentinel."
                .into(),
            schema_json: json!({
                "type":"object",
                "properties": {
                    "uri": {"type":"string","description":"Complete URI, e.g. file:///abs/path"},
                    "offset": {"type":"integer","minimum":0,"default":0,
                               "description":"Byte offset to start reading from."},
                    "max_bytes": {"type":"integer","minimum":1,"default":51200,
                                  "description":"Max bytes this call. Capped at 262144 (256 KiB); larger values silently clamp."},
                    "from_end": {"type":"boolean","default":false,
                                 "description":"If true, read the last max_bytes of the file. Omit offset when using this."},
                    "force_text": {"type":"boolean","default":false,
                                   "description":"If true, always UTF-8-lossy decode even when the content looks binary. Leave false for image/OCR tasks so recognized images are attached visually."}
                },
                "required": ["uri"],
            }),
            timeout: Duration::from_secs(10),
            max_out_tokens: 16_384,
            concurrency: Concurrency::Parallel,
            side_effects: SideEffects::ReadOnly,
            idempotency: Idempotency::Idempotent,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsRead {
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
        let max_bytes = a.max_bytes.min(HARD_CAP_BYTES);
        let uri = Uri::new(&a.uri);

        // Stat first so we can report total size and detect truncation cleanly.
        let meta = self.bundle.fs.stat(&uri).await.map_err(super::map_fs_err)?;
        if meta.is_dir {
            return Err(ToolErr::deny(format!("`{}` is a directory", uri.as_str()))
                .with_hint("use fs_list to enumerate a directory"));
        }
        let total = meta.size;

        let effective_offset = if a.from_end {
            total.saturating_sub(max_bytes)
        } else {
            a.offset
        };

        // EOF-past-offset is valid: return empty but correct metadata.
        if effective_offset >= total {
            return Ok(ToolOk::text(format!(
                "-- (empty; offset={} is at or past total_bytes={total}) --",
                effective_offset
            )));
        }

        // Content-type sniff: peek the first ~32 bytes via a cheap read so
        // we can decide *before* loading the whole max_bytes slice whether
        // this is an image (needs full file, bounded by IMAGE_MAX_BYTES),
        // non-image binary (refuse immediately), or text (normal flow).
        if !a.force_text && effective_offset == 0 {
            let peek = self
                .bundle
                .fs
                .read(&uri, ReadOpts::range(0, 32))
                .await
                .map_err(super::map_fs_err)?;

            // Image: use meta.size (real total) to gate size, not max_bytes.
            if let Some(mime) = sniff_image_mime(&peek) {
                if total > IMAGE_MAX_BYTES as u64 {
                    return Err(ToolErr::deny(format!(
                        "image too large to include: {} bytes (cap {} / ~10k tokens)",
                        total, IMAGE_MAX_BYTES
                    ))
                    .with_hint("downsize first (e.g. convert to PNG at ≤1024x1024) and re-read"));
                }
                // Load the whole image.
                let full = self
                    .bundle
                    .fs
                    .read(&uri, ReadOpts::range(0, total as usize))
                    .await
                    .map_err(super::map_fs_err)?;
                let b64 = base64_encode(&full);
                let part = crate::core::types::ContentPart::Image {
                    uri: None,
                    b64: Some(b64),
                    mime: mime.to_string(),
                };
                return Ok(ToolOk::text(format!(
                    "-- ({mime} image; total_bytes={total}; attached as an image for visual inspection; do not re-read with force_text for OCR) --"
                ))
                .with_attachment(part));
            }

            // Non-image binary (ELF, zip, pdf, db, ...): refuse.
            // The peek + an extra slice up to BINARY_SNIFF_BYTES catch null
            // bytes reliably for anything not starting with null.
            let sniff_upper = BINARY_SNIFF_BYTES.min(total as usize);
            let sniff_buf = if peek.len() >= sniff_upper {
                peek.clone()
            } else {
                self.bundle
                    .fs
                    .read(&uri, ReadOpts::range(0, sniff_upper))
                    .await
                    .map_err(super::map_fs_err)?
            };
            if sniff_buf.contains(&0u8) {
                let hex = hex_preview(&sniff_buf[..16.min(sniff_buf.len())]);
                return Err(ToolErr::deny(format!(
                    "file appears to be binary (non-image, non-text); total_bytes={total}, first 16 bytes: {hex}"
                )).with_hint(
                    "refusing to load binary as text. If you need the bytes, use a tool that \
                     understands them (e.g. sh_exec with `file`, `xxd`, `hexdump`). Pass \
                     force_text=true if you accept lossy UTF-8 decoding.",
                ));
            }
        }

        let data = self
            .bundle
            .fs
            .read(&uri, ReadOpts::range(effective_offset, max_bytes as usize))
            .await
            .map_err(super::map_fs_err)?;
        let read_n = data.len() as u64;
        let end = effective_offset + read_n;

        let text = decode_and_truncate_lines(&data);

        let out = if a.from_end && effective_offset > 0 {
            let previous_offset = effective_offset.saturating_sub(max_bytes);
            format!(
                "-- (tail; total_bytes={total}; read {read_n} bytes from offset={effective_offset}; \
                 pass offset=0 for head or offset={previous_offset} max_bytes={max_bytes} for previous chunk) --\n{text}"
            )
        } else if end >= total {
            // Reached EOF within this call — no sentinel needed.
            text
        } else {
            // More remains. Append explicit sentinel so the agent knows
            // where to resume and doesn't silently believe it read everything.
            format!(
                "{text}\n-- (truncated; read {read_n} of {total} bytes; \
                 pass offset={end} for more) --"
            )
        };
        Ok(ToolOk::text(out))
    }
}

/// UTF-8-lossy decode, then truncate any line longer than `MAX_LINE_CHARS`.
/// Truncated lines get a trailing ` <line truncated>` marker so the agent
/// sees that content was dropped.
fn decode_and_truncate_lines(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for line in text.split_inclusive('\n') {
        if !first { /* split_inclusive keeps the '\n', no separator needed */ }
        first = false;
        let newline = line.ends_with('\n');
        let body_end = if newline { line.len() - 1 } else { line.len() };
        let body = &line[..body_end];
        // Count CHARACTERS, not bytes — keeps multi-byte UTF-8 happy.
        if body.chars().count() > MAX_LINE_CHARS {
            // Take the first MAX_LINE_CHARS chars.
            let cut = body
                .char_indices()
                .nth(MAX_LINE_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(body.len());
            out.push_str(&body[..cut]);
            out.push_str(" <line truncated>");
        } else {
            out.push_str(body);
        }
        if newline {
            out.push('\n');
        }
    }
    out
}

fn hex_preview(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Sniff common image formats by magic bytes. Returns the MIME type.
fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 8 && data[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg");
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Minimal base64 encoder. Kept private to avoid a crate-wide dep just for
/// this.
fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let chunks = bytes.chunks_exact(3);
    let tail = chunks.remainder();
    for c in chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(T[((n >> 6) & 0x3f) as usize] as char);
        out.push(T[(n & 0x3f) as usize] as char);
    }
    match tail.len() {
        1 => {
            let n = (tail[0] as u32) << 16;
            out.push(T[((n >> 18) & 0x3f) as usize] as char);
            out.push(T[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((tail[0] as u32) << 16) | ((tail[1] as u32) << 8);
            out.push(T[((n >> 18) & 0x3f) as usize] as char);
            out.push(T[((n >> 12) & 0x3f) as usize] as char);
            out.push(T[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_pass_through() {
        let s = "hello\nworld\n";
        assert_eq!(decode_and_truncate_lines(s.as_bytes()), s);
    }

    #[test]
    fn long_line_gets_truncated() {
        let long = "x".repeat(2500);
        let input = format!("short\n{long}\nend\n");
        let out = decode_and_truncate_lines(input.as_bytes());
        assert!(out.contains("short\n"));
        assert!(out.contains("end\n"));
        assert!(out.contains("<line truncated>"));
        // First 2000 'x' + marker + trailing lines; original 2500-'x' line shouldn't be there whole.
        assert!(!out.contains(&"x".repeat(2500)));
    }

    #[test]
    fn hex_preview_format() {
        assert_eq!(hex_preview(&[0x00, 0x1f, 0xff]), "00 1f ff");
    }

    #[test]
    fn utf8_lossy_on_invalid_bytes() {
        // Invalid UTF-8 shouldn't panic.
        let bytes = [b'o', b'k', 0xff, 0xfe, b'\n'];
        let out = decode_and_truncate_lines(&bytes);
        assert!(out.contains("ok"));
        assert!(out.ends_with('\n'));
    }
}
