//! High-level MCP client: initialize + tools/list + tools/call.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::core::cancel::CancelToken;

use super::jsonrpc::{Request, Response};

#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("tool `{0}` not found")]
    UnknownTool(String),
}

/// Abstract transport for JSON-RPC messages.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, msg: &Value, cancel: CancelToken) -> Result<(), McpClientError>;
    async fn recv(&self, cancel: CancelToken) -> Result<Value, McpClientError>;
}

/// One tool exposed by an MCP server, as reported by `tools/list`.
#[derive(Clone, Debug)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpClient {
    transport: Box<dyn Transport>,
    next_id: AtomicU64,
    /// All requests go through a single in-flight slot to avoid interleaving.
    /// Since we send request → wait → response, serialize on this mutex.
    io_lock: Mutex<()>,
    server_name: Mutex<Option<String>>,
}

impl McpClient {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            next_id: AtomicU64::new(1),
            io_lock: Mutex::new(()),
            server_name: Mutex::new(None),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn call(
        &self,
        method: &str,
        params: Option<Value>,
        cancel: CancelToken,
    ) -> Result<Value, McpClientError> {
        let _g = self.io_lock.lock().await;
        let id = self.next_id();
        let req = Request::new(id, method, params);
        let req_v = serde_json::to_value(&req)
            .map_err(|e| McpClientError::Protocol(format!("encode: {e}")))?;
        self.transport.send(&req_v, cancel.child()).await?;
        // Drain messages until we get a response matching our id.
        // (We ignore any server-sent notifications in this milestone.)
        loop {
            let v = self.transport.recv(cancel.child()).await?;
            // Notifications have no "id" — ignore.
            if v.get("id").is_none() {
                continue;
            }
            let resp: Response = serde_json::from_value(v)
                .map_err(|e| McpClientError::Protocol(format!("decode response: {e}")))?;
            if resp.id != Some(id) {
                continue;
            }
            if let Some(err) = resp.error {
                return Err(McpClientError::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    /// MCP `initialize`. Returns the server's advertised name.
    pub async fn initialize(&self) -> Result<String, McpClientError> {
        let result = self
            .call(
                "initialize",
                Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "clientInfo": { "name": "muagent-mcp", "version": "0.1.0" }
                })),
                CancelToken::never(),
            )
            .await?;

        let name = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("mcp-server")
            .to_string();
        *self.server_name.lock().await = Some(name.clone());

        // Per MCP spec: send "notifications/initialized" (id-less).
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let _ = self.transport.send(&note, CancelToken::never()).await;
        Ok(name)
    }

    pub async fn server_name(&self) -> Option<String> {
        self.server_name.lock().await.clone()
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpClientError> {
        let v = self
            .call("tools/list", Some(json!({})), CancelToken::never())
            .await?;
        let arr = v
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .ok_or_else(|| McpClientError::Protocol("tools/list: missing tools[]".into()))?;
        let mut out = Vec::with_capacity(arr.len());
        for t in arr {
            let name = t
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return Err(McpClientError::Protocol("tool missing name".into()));
            }
            let description = t
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object"}));
            out.push(McpToolDescriptor {
                name,
                description,
                input_schema,
            });
        }
        Ok(out)
    }

    /// Returns (text, is_error).
    pub async fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> Result<(String, bool), McpClientError> {
        self.call_tool_cancelable(name, args, CancelToken::never())
            .await
    }

    pub async fn call_tool_cancelable(
        &self,
        name: &str,
        args: Value,
        cancel: CancelToken,
    ) -> Result<(String, bool), McpClientError> {
        let v = self
            .call(
                "tools/call",
                Some(json!({
                    "name": name,
                    "arguments": args,
                })),
                cancel,
            )
            .await?;
        let is_error = v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        for part in &content {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }
        if text.is_empty() && !content.is_empty() {
            text = serde_json::to_string(&content).unwrap_or_default();
        }
        Ok((text, is_error))
    }
}
