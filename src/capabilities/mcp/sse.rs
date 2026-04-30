//! Legacy MCP HTTP+SSE transport.
//!
//! Some harnesses, including Toolathlon's decoupled gateway, still expose the
//! older two-endpoint MCP transport:
//! - `GET /sse` opens a long-lived SSE stream and sends an `endpoint` event.
//! - JSON-RPC messages are POSTed to that endpoint.
//! - JSON-RPC responses arrive back on the SSE stream as `message` events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::core::cancel::CancelToken;

use super::client::{McpClientError, Transport};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CANCEL_POLL: Duration = Duration::from_millis(50);
const MAX_SSE_EVENT_BYTES: usize = 10 * 1024 * 1024;

pub struct SseTransport {
    client: reqwest::Client,
    post_url: String,
    state: Arc<Mutex<SseState>>,
    notify: Arc<Notify>,
    reader: JoinHandle<()>,
}

#[derive(Default)]
struct SseState {
    inbox: VecDeque<Value>,
    closed: Option<String>,
}

#[derive(Default)]
struct SseEvent {
    event: Option<String>,
    data: String,
}

impl SseTransport {
    pub async fn connect(sse_url: impl Into<String>) -> Result<Self, McpClientError> {
        let sse_url = sse_url.into();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| McpClientError::Transport(format!("sse client: {e}")))?;
        let state = Arc::new(Mutex::new(SseState::default()));
        let notify = Arc::new(Notify::new());
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let reader = tokio::spawn(read_sse_loop(
            client.clone(),
            sse_url.clone(),
            state.clone(),
            notify.clone(),
            endpoint_tx,
        ));

        let endpoint = tokio::time::timeout(CONNECT_TIMEOUT, endpoint_rx)
            .await
            .map_err(|_| McpClientError::Transport("sse endpoint timeout".into()))?
            .map_err(|_| McpClientError::Transport("sse reader closed before endpoint".into()))?
            .map_err(McpClientError::Transport)?;
        let post_url = resolve_endpoint(&sse_url, &endpoint)?;

        Ok(Self {
            client,
            post_url,
            state,
            notify,
            reader,
        })
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[async_trait::async_trait]
impl Transport for SseTransport {
    async fn send(&self, msg: &Value, cancel: CancelToken) -> Result<(), McpClientError> {
        if cancel.triggered() {
            return Err(McpClientError::Transport("cancelled".into()));
        }
        let body = serde_json::to_vec(msg)
            .map_err(|e| McpClientError::Transport(format!("serialize: {e}")))?;
        let request = self
            .client
            .post(&self.post_url)
            .header("Content-Type", "application/json")
            .body(body);

        let resp = tokio::select! {
            resp = request.send() => resp
                .map_err(|e| McpClientError::Transport(format!("sse post: {e}")))?,
            _ = wait_cancel(cancel.child()) => {
                return Err(McpClientError::Transport("cancelled".into()));
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(McpClientError::Transport(format!(
                "sse post status {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        Ok(())
    }

    async fn recv(&self, cancel: CancelToken) -> Result<Value, McpClientError> {
        loop {
            {
                let mut state = self.state.lock().await;
                if let Some(v) = state.inbox.pop_front() {
                    return Ok(v);
                }
                if let Some(reason) = &state.closed {
                    return Err(McpClientError::Transport(reason.clone()));
                }
            }

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(CANCEL_POLL) => {
                    if cancel.triggered() {
                        return Err(McpClientError::Transport("cancelled".into()));
                    }
                }
            }
        }
    }
}

async fn read_sse_loop(
    client: reqwest::Client,
    sse_url: String,
    state: Arc<Mutex<SseState>>,
    notify: Arc<Notify>,
    endpoint_tx: oneshot::Sender<Result<String, String>>,
) {
    let mut endpoint_tx = Some(endpoint_tx);
    let resp = match client
        .get(&sse_url)
        .header("Accept", "text/event-stream")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = format!("sse connect: {e}");
            send_endpoint(&mut endpoint_tx, Err(msg.clone()));
            close_state(&state, &notify, msg).await;
            return;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let msg = format!("sse status {status}");
        send_endpoint(&mut endpoint_tx, Err(msg.clone()));
        close_state(&state, &notify, msg).await;
        return;
    }

    let mut stream = resp.bytes_stream();
    let mut line_buf = String::new();
    let mut event = SseEvent::default();
    let mut endpoint_sent = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let msg = format!("sse read: {e}");
                if !endpoint_sent {
                    send_endpoint(&mut endpoint_tx, Err(msg.clone()));
                }
                close_state(&state, &notify, msg).await;
                return;
            }
        };
        line_buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = line_buf.find('\n') {
            let mut line = line_buf[..pos].to_string();
            line_buf.drain(..=pos);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Err(e) = handle_sse_line(
                &line,
                &mut event,
                &state,
                &notify,
                &mut endpoint_tx,
                &mut endpoint_sent,
            )
            .await
            {
                if !endpoint_sent {
                    send_endpoint(&mut endpoint_tx, Err(e.clone()));
                }
                close_state(&state, &notify, e).await;
                return;
            }
        }

        if line_buf.len() > MAX_SSE_EVENT_BYTES {
            let msg = format!("sse line too large: over {MAX_SSE_EVENT_BYTES} bytes");
            if !endpoint_sent {
                send_endpoint(&mut endpoint_tx, Err(msg.clone()));
            }
            close_state(&state, &notify, msg).await;
            return;
        }
    }

    if !event.data.is_empty() {
        let _ = flush_event(
            &mut event,
            &state,
            &notify,
            &mut endpoint_tx,
            &mut endpoint_sent,
        )
        .await;
    }
    if !endpoint_sent {
        send_endpoint(&mut endpoint_tx, Err("sse closed before endpoint".into()));
    }
    close_state(&state, &notify, "sse stream closed".into()).await;
}

async fn handle_sse_line(
    line: &str,
    event: &mut SseEvent,
    state: &Arc<Mutex<SseState>>,
    notify: &Arc<Notify>,
    endpoint_tx: &mut Option<oneshot::Sender<Result<String, String>>>,
    endpoint_sent: &mut bool,
) -> Result<(), String> {
    if line.is_empty() {
        return flush_event(event, state, notify, endpoint_tx, endpoint_sent).await;
    }
    if line.starts_with(':') {
        return Ok(());
    }
    if let Some(rest) = line.strip_prefix("event:") {
        event.event = Some(rest.trim_start().to_string());
        return Ok(());
    }
    if let Some(rest) = line.strip_prefix("data:") {
        if event.data.len() + rest.len() > MAX_SSE_EVENT_BYTES {
            return Err(format!(
                "sse event too large: over {MAX_SSE_EVENT_BYTES} bytes"
            ));
        }
        if !event.data.is_empty() {
            event.data.push('\n');
        }
        event.data.push_str(rest.trim_start());
    }
    Ok(())
}

async fn flush_event(
    event: &mut SseEvent,
    state: &Arc<Mutex<SseState>>,
    notify: &Arc<Notify>,
    endpoint_tx: &mut Option<oneshot::Sender<Result<String, String>>>,
    endpoint_sent: &mut bool,
) -> Result<(), String> {
    let event_name = event.event.take().unwrap_or_else(|| "message".to_string());
    let data = std::mem::take(&mut event.data);
    if data.is_empty() {
        return Ok(());
    }

    match event_name.as_str() {
        "endpoint" => {
            if !*endpoint_sent {
                send_endpoint(endpoint_tx, Ok(data));
                *endpoint_sent = true;
            }
        }
        "message" => {
            let v: Value = serde_json::from_str(&data)
                .map_err(|e| format!("invalid sse message json: {e}; data={data:?}"))?;
            let mut state = state.lock().await;
            state.inbox.push_back(v);
            notify.notify_waiters();
        }
        _ => {}
    }
    Ok(())
}

fn send_endpoint(
    endpoint_tx: &mut Option<oneshot::Sender<Result<String, String>>>,
    value: Result<String, String>,
) {
    if let Some(tx) = endpoint_tx.take() {
        let _ = tx.send(value);
    }
}

async fn close_state(state: &Arc<Mutex<SseState>>, notify: &Arc<Notify>, reason: String) {
    let mut state = state.lock().await;
    state.closed = Some(reason);
    notify.notify_waiters();
}

fn resolve_endpoint(sse_url: &str, endpoint: &str) -> Result<String, McpClientError> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let base = reqwest::Url::parse(sse_url)
        .map_err(|e| McpClientError::Transport(format!("invalid sse url: {e}")))?;
    let joined = base
        .join(endpoint)
        .map_err(|e| McpClientError::Transport(format!("invalid sse endpoint: {e}")))?;
    Ok(joined.to_string())
}

async fn wait_cancel(cancel: CancelToken) {
    while !cancel.triggered() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_endpoint;

    #[test]
    fn resolves_absolute_endpoint() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:10086/sse", "http://x/messages").unwrap(),
            "http://x/messages"
        );
    }

    #[test]
    fn resolves_root_relative_endpoint() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:10086/sse", "/messages/?session_id=abc").unwrap(),
            "http://127.0.0.1:10086/messages/?session_id=abc"
        );
    }
}
