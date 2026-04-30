//! Offline test: spawn the bundled test MCP server over stdio and run
//! initialize → tools/list → tools/call.

use std::sync::Arc;

use muagent::capabilities::mcp::prelude::*;
use serde_json::json;

fn server_bin() -> String {
    env!("CARGO_BIN_EXE_muagent-mcp-test-server").to_string()
}

#[tokio::test]
async fn stdio_initialize_list_and_call() {
    let spec = StdioSpawn::new(server_bin());
    let transport = StdioTransport::spawn(&spec).await.expect("spawn");
    let client = Arc::new(McpClient::new(Box::new(transport)));

    let server = client.initialize().await.expect("initialize");
    assert_eq!(server, "muagent-mcp-test-server");

    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "calc_mul");
    assert!(tools[0].input_schema.get("properties").is_some());

    // Happy path.
    let (text, is_error) = client
        .call_tool("calc_mul", json!({"a": 6, "b": 7}))
        .await
        .expect("call");
    assert!(!is_error);
    assert_eq!(text, "42");

    // Bad args → isError=true.
    let (err_text, is_error) = client
        .call_tool("calc_mul", json!({"a": "oops"}))
        .await
        .expect("call");
    assert!(is_error);
    assert!(err_text.contains("missing"));

    // Unknown tool → JSON-RPC error.
    let err = client
        .call_tool("nonexistent", json!({}))
        .await
        .expect_err("should rpc-error");
    assert!(err.to_string().contains("unknown tool"));
}
