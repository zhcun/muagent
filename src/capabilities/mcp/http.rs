//! HTTP transport for MCP (Streamable-HTTP style).
//!
//! Each JSON-RPC message is POSTed to a single endpoint. The server replies
//! with **either**:
//! - `Content-Type: application/json`  — one JSON value (request response or
//!   notification). We queue it for the next `recv()`.
//! - `Content-Type: text/event-stream` — an SSE stream. Each `data:` line
//!   is a JSON message; we parse and queue every one.
//! - no body (empty 200/202) — valid when the message being sent is itself
//!   a notification (no id, no expected response).
//!
//! Unlike stdio, HTTP is request/response at the transport layer but the
//! MCP JSON-RPC layer is still logically duplex — so we queue parsed
//! messages and let `recv()` drain. If `recv()` is called when the queue
//! is empty, it errors (HTTP transport does not keep a long-poll open).
//! That's acceptable because `McpClient::call` always sends before it
//! receives.
//!
//! Out of scope for this milestone:
//! - Legacy HTTP+SSE split-endpoint transport (separate /sse GET +
//!   /messages POST). The Streamable-HTTP single-endpoint variant is the
//!   current spec direction and simpler to implement.
//! - Session resumption (Mcp-Session-Id header). Easy to add later.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::core::cancel::CancelToken;
use crate::core::net::{HttpMethod, HttpReq, NetEgress};

use super::client::{McpClientError, Transport};

pub struct HttpTransport {
    net: Arc<dyn NetEgress>,
    endpoint: String,
    inbox: Mutex<VecDeque<Value>>,
    io_lock: Mutex<()>,
}

impl HttpTransport {
    pub fn new(net: Arc<dyn NetEgress>, endpoint: impl Into<String>) -> Self {
        Self {
            net,
            endpoint: endpoint.into(),
            inbox: Mutex::new(VecDeque::new()),
            io_lock: Mutex::new(()),
        }
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn send(&self, msg: &Value, cancel: CancelToken) -> Result<(), McpClientError> {
        let _g = self.io_lock.lock().await;

        let body = serde_json::to_vec(msg)
            .map_err(|e| McpClientError::Transport(format!("serialize: {e}")))?;
        let mut headers = std::collections::HashMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert(
            "Accept".into(),
            "application/json, text/event-stream".into(),
        );

        let req = HttpReq {
            method: HttpMethod::Post,
            url: self.endpoint.clone(),
            headers,
            body: Some(body),
        };
        let resp = self
            .net
            .http(req, cancel)
            .await
            .map_err(|e| McpClientError::Transport(format!("http: {e}")))?;

        if resp.status < 200 || resp.status >= 300 {
            return Err(McpClientError::Transport(format!(
                "http status {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }

        // Empty body (e.g. 202 Accepted for notifications) → nothing to queue.
        if resp.body.is_empty() {
            return Ok(());
        }

        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();

        let parsed: Vec<Value> = if ct.contains("text/event-stream") {
            parse_sse(&resp.body)
                .map_err(|e| McpClientError::Transport(format!("sse parse: {e}")))?
        } else {
            // Default: treat as JSON.
            let v: Value = serde_json::from_slice(&resp.body)
                .map_err(|e| McpClientError::Transport(format!("json parse: {e}")))?;
            match v {
                Value::Array(xs) => xs,
                other => vec![other],
            }
        };

        let mut inbox = self.inbox.lock().await;
        for v in parsed {
            inbox.push_back(v);
        }
        Ok(())
    }

    async fn recv(&self, cancel: CancelToken) -> Result<Value, McpClientError> {
        let mut inbox = self.inbox.lock().await;
        if let Some(v) = inbox.pop_front() {
            Ok(v)
        } else if cancel.triggered() {
            Err(McpClientError::Transport("cancelled".into()))
        } else {
            Err(McpClientError::Transport(
                "no queued response (HTTP transport receives only as a side effect of send)".into(),
            ))
        }
    }
}

fn parse_sse(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut data_buf = String::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            // event boundary — flush accumulated data
            if !data_buf.is_empty() {
                let v: Value = serde_json::from_str(&data_buf)
                    .map_err(|e| format!("invalid json in sse data: {e}; data={data_buf:?}"))?;
                out.push(v);
                data_buf.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim_start());
        }
        // Other SSE fields (event:, id:, retry:) are ignored for this milestone.
    }
    // Trailing chunk without blank line.
    if !data_buf.is_empty() {
        let v: Value = serde_json::from_str(&data_buf)
            .map_err(|e| format!("invalid json in sse tail data: {e}; data={data_buf:?}"))?;
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_sse;
    use serde_json::json;

    #[test]
    fn sse_single_event() {
        let body = b"data: {\"id\":1,\"result\":\"ok\"}\n\n";
        let parsed = parse_sse(body).unwrap();
        assert_eq!(parsed, vec![json!({"id":1,"result":"ok"})]);
    }

    #[test]
    fn sse_multiple_events() {
        let body = b"data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let parsed = parse_sse(body).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn sse_multiline_data() {
        let body = b"data: {\"id\":\ndata: 1}\n\n";
        let parsed = parse_sse(body).unwrap();
        assert_eq!(parsed, vec![json!({"id": 1})]);
    }

    #[test]
    fn sse_ignores_event_field() {
        let body = b"event: message\ndata: {\"id\":1}\n\n";
        let parsed = parse_sse(body).unwrap();
        assert_eq!(parsed, vec![json!({"id": 1})]);
    }
}
