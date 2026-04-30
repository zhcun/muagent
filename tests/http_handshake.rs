//! Offline test: spin up a tiny in-process HTTP/1.1 server that speaks MCP
//! JSON-RPC, drive the full handshake through `HttpTransport`.
//!
//! The test server supports two modes picked by query param on the endpoint:
//! - default (`/mcp`): respond with `Content-Type: application/json`.
//! - `/mcp?sse=1`: respond with `Content-Type: text/event-stream` wrapping
//!   the same JSON in a single SSE event.

use std::io::ErrorKind;
use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::capabilities::mcp::prelude::*;
use muagent::capabilities::mcp::test_support::handle_rpc;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn start_server(sse_mode: bool) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _ = handle_conn(sock, sse_mode).await;
            });
        }
    });
    Ok(format!("http://127.0.0.1:{port}/mcp"))
}

async fn start_server_or_skip(sse_mode: bool) -> Option<String> {
    match start_server(sse_mode).await {
        Ok(url) => Some(url),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping http_handshake test: local bind not permitted in this environment");
            None
        }
        Err(err) => panic!("bind: {err}"),
    }
}

async fn handle_conn(mut sock: TcpStream, sse_mode: bool) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    // Read until we see \r\n\r\n (headers end), then handle one request.
    // Test server is one-shot: each connection serves a single request and
    // closes — that's why this is a single loop, not a nested one.
    loop {
        if let Some(idx) = find_subslice(&buf, b"\r\n\r\n") {
            let headers_end = idx + 4;
            let headers = std::str::from_utf8(&buf[..idx]).unwrap_or("");
            let content_length = parse_content_length(headers);
            let body_start = headers_end;
            let body_end = body_start + content_length;
            while buf.len() < body_end {
                let mut tmp = [0u8; 4096];
                let n = sock.read(&mut tmp).await?;
                if n == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = &buf[body_start..body_end];
            let req_val: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => {
                    write_status(&mut sock, 400, b"bad json").await?;
                    return Ok(());
                }
            };

            let maybe_resp = handle_rpc(&req_val);
            match maybe_resp {
                None => {
                    // Notification — 202 Accepted, empty body.
                    sock.write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                }
                Some(resp) if sse_mode => {
                    let data = serde_json::to_string(&resp).unwrap();
                    let body = format!("event: message\ndata: {data}\n\n");
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    sock.write_all(hdr.as_bytes()).await?;
                    sock.write_all(body.as_bytes()).await?;
                }
                Some(resp) => {
                    let body = serde_json::to_vec(&resp).unwrap();
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    sock.write_all(hdr.as_bytes()).await?;
                    sock.write_all(&body).await?;
                }
            }
            let _ = sock.shutdown().await;
            return Ok(());
        }
        let mut tmp = [0u8; 4096];
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("Content-Length") {
                return v.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

async fn write_status(sock: &mut TcpStream, code: u16, body: &[u8]) -> std::io::Result<()> {
    let hdr = format!(
        "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(hdr.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}

fn build_net() -> Arc<ReqwestEgress> {
    Arc::new(ReqwestEgress::new().unwrap())
}

#[tokio::test]
async fn http_json_response_happy_path() {
    let Some(url) = start_server_or_skip(false).await else {
        return;
    };
    let net = build_net();
    let transport = HttpTransport::new(net, &url);
    let client = Arc::new(McpClient::new(Box::new(transport)));

    let server = client.initialize().await.expect("initialize");
    assert_eq!(server, "muagent-mcp-test-server");

    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "calc_mul");

    let (text, is_error) = client
        .call_tool("calc_mul", json!({"a": 6, "b": 7}))
        .await
        .expect("call");
    assert!(!is_error);
    assert_eq!(text, "42");
}

#[tokio::test]
async fn http_sse_response_happy_path() {
    let Some(url) = start_server_or_skip(true).await else {
        return;
    };
    let net = build_net();
    let transport = HttpTransport::new(net, &url);
    let client = Arc::new(McpClient::new(Box::new(transport)));

    client.initialize().await.expect("initialize");
    let (text, _) = client
        .call_tool("calc_mul", json!({"a": 11, "b": 3}))
        .await
        .expect("call");
    assert_eq!(text, "33");
}

#[tokio::test]
async fn http_unknown_tool_surfaces_rpc_error() {
    let Some(url) = start_server_or_skip(false).await else {
        return;
    };
    let net = build_net();
    let transport = HttpTransport::new(net, &url);
    let client = Arc::new(McpClient::new(Box::new(transport)));

    client.initialize().await.expect("initialize");
    let err = client
        .call_tool("nope", json!({}))
        .await
        .expect_err("should rpc-error");
    assert!(err.to_string().contains("unknown tool"));
}
