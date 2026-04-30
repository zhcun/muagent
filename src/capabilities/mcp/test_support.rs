//! Minimal in-process MCP server logic, shared between the stdio test binary
//! and the HTTP transport integration tests. Production code should not use
//! this — it exists to cover the protocol surface end-to-end without pulling
//! in a real third-party MCP server during CI.
//!
//! Exposes one tool (`calc_mul`) that multiplies two integers.

use serde_json::{json, Value};

/// Handle one incoming JSON-RPC message (request or notification).
///
/// Returns `Some(response)` for requests, `None` for notifications.
pub fn handle_rpc(req: &Value) -> Option<Value> {
    // Notifications have no "id" — swallow.
    req.get("id")?;
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    Some(match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "muagent-mcp-test-server", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "calc_mul",
                    "description": "Multiply two integers. Returns the product as plain text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "a": {"type":"integer"},
                            "b": {"type":"integer"}
                        },
                        "required": ["a","b"]
                    }
                }]
            }
        }),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(|s| s.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            if name != "calc_mul" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("unknown tool: {name}") }
                })
            } else {
                let a = args.get("a").and_then(|v| v.as_i64());
                let b = args.get("b").and_then(|v| v.as_i64());
                match (a, b) {
                    (Some(a), Some(b)) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type":"text","text": format!("{}", a * b)}],
                            "isError": false
                        }
                    }),
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type":"text","text": "missing integer args"}],
                            "isError": true
                        }
                    }),
                }
            }
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {method}") }
        }),
    })
}
