//! `net_http`: give the agent unrestricted outbound HTTP through the existing
//! `NetEgress` adapter. This tool only shapes args/results.
//!
//! Designed for JSON/text APIs. Binary response bodies are returned as
//! base64 when the Content-Type isn't obviously text.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::core::cancel::CancelToken;
use crate::core::net::{HttpMethod, HttpReq, NetEgress};
use crate::core::prelude::{
    parse_args, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool, ToolDescriptor, ToolErr,
    ToolOk,
};

const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024;

#[derive(Deserialize)]
struct Args {
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

pub struct NetHttp {
    net: Arc<dyn NetEgress>,
    desc: ToolDescriptor,
}

impl NetHttp {
    pub fn new(net: Arc<dyn NetEgress>) -> Self {
        let desc = ToolDescriptor {
            name: "net_http".into(),
            description: "HTTP request with method, url, optional headers and body. \
                 Returns JSON {status, headers, body, body_preview_kind, \
                 body_start_byte, body_end_byte_exclusive, body_bytes, \
                 body_returned_bytes, body_truncated, body_next}. `body` \
                 is a bounded head preview capped at 32768 bytes before \
                 JSON/base64 encoding. \
                 `body` is text when \
                 the response Content-Type is text/* / application/json / \
                 application/xml / application/yaml; otherwise it's \
                 {\"base64\": \"...\"}. \
                 Idempotency: GET/HEAD are Idempotent, everything else is \
                 AtMostOnce (won't replay on crash — use with care)."
                .into(),
            schema_json: json!({
                "type": "object",
                "properties": {
                    "method": {"type":"string","enum":["GET","POST","PUT","DELETE","PATCH","HEAD","OPTIONS"]},
                    "url":    {"type":"string","description":"Full URL including scheme."},
                    "headers":{"type":"object","description":"Optional header map."},
                    "body":   {"type":"string","description":"Optional body (pass JSON as a string)."},
                },
                "required": ["method","url"],
            }),
            timeout: Duration::from_secs(30),
            max_out_tokens: 16_384,
            concurrency: Concurrency::Parallel,
            side_effects: SideEffects::Mutating, // POST/PUT/DELETE can mutate remote state
            idempotency: Idempotency::AtMostOnce, // safer default for unknown servers
        };
        Self { net, desc }
    }
}

fn parse_method(s: &str) -> Option<HttpMethod> {
    use HttpMethod::*;
    Some(match s.to_ascii_uppercase().as_str() {
        "GET" => Get,
        "POST" => Post,
        "PUT" => Put,
        "DELETE" => Delete,
        "PATCH" => Patch,
        "HEAD" => Head,
        "OPTIONS" => Options,
        _ => return None,
    })
}

fn is_texty(ct: &str) -> bool {
    let ct = ct.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.starts_with("application/json")
        || ct.starts_with("application/xml")
        || ct.starts_with("application/javascript")
        || ct.starts_with("application/yaml")
        || ct.starts_with("application/x-yaml")
}

#[async_trait]
impl Tool for NetHttp {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    // net_http is dynamically Idempotent for GET/HEAD, AtMostOnce for the rest.
    // We only need `method` here, so peek at it directly — full Args parse is
    // unnecessary for an idempotency hint and we don't want to deny on
    // malformed args at this stage (run_ctxless will surface that).
    fn idempotency_for_args(&self, args: &Value) -> Idempotency {
        match args.get("method").and_then(|v| v.as_str()) {
            Some(m) if matches!(m.to_ascii_uppercase().as_str(), "GET" | "HEAD") => {
                Idempotency::Idempotent
            }
            _ => Idempotency::AtMostOnce,
        }
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
        if parse_method(&a.method).is_none() {
            return GuardOutcome::Deny {
                reason: format!("unknown method `{}`", a.method),
                hint: Some("use GET/POST/PUT/DELETE/PATCH/HEAD/OPTIONS".into()),
            };
        }
        // Only bail on obviously-bad URLs; NetEgress itself is unrestricted.
        if !a.url.contains("://") {
            return GuardOutcome::Deny {
                reason: "url missing scheme".into(),
                hint: Some("include https:// or http://".into()),
            };
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        let a: Args = parse_args(&args)?;
        let method = parse_method(&a.method).ok_or_else(|| ToolErr::deny("invalid method"))?;
        let body = a.body.map(|s| s.into_bytes());

        let can_retry_range = matches!(method, HttpMethod::Get | HttpMethod::Head);
        let req = HttpReq {
            method,
            url: a.url,
            headers: a.headers,
            body,
        };
        let resp = self.net.http(req, cancel).await.map_err(map_net_err)?;

        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

        let body_bytes = resp.body.len();
        let body_returned_bytes = body_bytes.min(MAX_RESPONSE_BODY_BYTES);
        let body_truncated = body_bytes > body_returned_bytes;
        let body_preview = &resp.body[..body_returned_bytes];

        let body_value = if is_texty(&ct) {
            Value::String(String::from_utf8_lossy(body_preview).into_owned())
        } else {
            json!({ "base64": base64_encode(body_preview) })
        };

        let mut header_map = Map::new();
        for (k, v) in &resp.headers {
            header_map.insert(k.clone(), Value::String(v.clone()));
        }

        Ok(ToolOk::text(
            serde_json::to_string(&json!({
                "status": resp.status,
                "headers": header_map,
                "body": body_value,
                "body_preview_kind": "head",
                "body_start_byte": 0,
                "body_end_byte_exclusive": body_returned_bytes,
                "body_bytes": body_bytes,
                "body_returned_bytes": body_returned_bytes,
                "body_truncated": body_truncated,
                "body_next": if body_truncated && can_retry_range {
                    json!({
                        "note": "preview is the response head; if the server supports Range, retry this GET with Range: bytes=<start>-<end> for another slice or Range: bytes=-32768 for tail"
                    })
                } else {
                    Value::Null
                },
            }))
            .unwrap(),
        ))
    }
}

fn map_net_err(e: crate::core::net::NetErr) -> ToolErr {
    use crate::core::net::NetErr::*;
    match e {
        Denied(s) => ToolErr::deny(format!("denied: {s}")),
        Dns(s) => ToolErr::retry(format!("dns: {s}")),
        Connect(s) => ToolErr::retry(format!("connect: {s}")),
        Tls(s) => ToolErr::deny(format!("tls: {s}")),
        Timeout => ToolErr::retry("timeout"),
        HttpStatus { status, reason } if matches!(status, 408 | 429 | 500..=599) => {
            ToolErr::retry(format!("http {status}: {reason}"))
        }
        HttpStatus { status, reason } => ToolErr::deny(format!("http {status}: {reason}")),
        Io(s) => ToolErr::retry(format!("io: {s}")),
        Cancelled => ToolErr::retry("cancelled"),
    }
}

// Minimal base64 encoder — avoid adding a dep just for this one function.
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
    fn b64_basic() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }
}
