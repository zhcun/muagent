//! Live E2E: agent calls MCP tool over HTTP transport.
//!
//! Same test shape as m1_live_mcp (stdio), but the MCP server runs as an
//! in-process HTTP server reachable via HttpTransport + ReqwestEgress.

use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::capabilities::mcp::prelude::*;
use muagent::capabilities::mcp::test_support::handle_rpc;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::types::{Content, Message};
use muagent::prelude::*;
use muagent::providers::OpenAiAdapter;
use muagent::storage::MemorySessionStore;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    (
        std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY"),
        std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
        std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-5.4-nano".into()),
    )
}

fn build_real_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

async fn drive_until_terminal(runner: &Runner, state: &mut RunState, max: usize) {
    for _ in 0..max {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            return;
        }
        runner.step(state).await.expect("step");
    }
}

// === tiny MCP HTTP server (inline) ===

async fn start_mcp_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _ = handle(sock).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/mcp")
}

async fn handle(mut sock: TcpStream) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&buf[..idx]).unwrap_or("");
            let len: usize = headers
                .split("\r\n")
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case("Content-Length"))
                .and_then(|(_, v)| v.trim().parse().ok())
                .unwrap_or(0);
            let end = idx + 4 + len;
            while buf.len() < end {
                let mut t = [0u8; 4096];
                let n = sock.read(&mut t).await?;
                if n == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&t[..n]);
            }
            let body = &buf[idx + 4..end];
            let req: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return Ok(()),
            };
            let resp = handle_rpc(&req);
            match resp {
                None => {
                    sock.write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                }
                Some(r) => {
                    let b = serde_json::to_vec(&r).unwrap();
                    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                    sock.write_all(h.as_bytes()).await?;
                    sock.write_all(&b).await?;
                }
            }
            let _ = sock.shutdown().await;
            return Ok(());
        }
        let mut t = [0u8; 4096];
        let n = sock.read(&mut t).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&t[..n]);
    }
}

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_mcp_over_http_agent_calls_remote_tool() {
    let url = start_mcp_server().await;
    eprintln!("-- MCP http server at {url}");

    let net = Arc::new(ReqwestEgress::new().unwrap());
    let transport = HttpTransport::new(net.clone(), &url);
    let client = Arc::new(McpClient::new(Box::new(transport)));

    let model = build_real_model();
    let registry = Arc::new(CapabilityRegistry::new());
    register_mcp_tools(&registry, client)
        .await
        .expect("register mcp tools");

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"calc_mul"), "calc_mul missing: {names:?}");

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt("You are precise. For multiplication, ALWAYS use the calc_mul tool.")
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("What is 19 times 23? Use the tool."),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 12).await;

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done, got {other:?}"),
    };
    eprintln!("-- final: {final_text}");

    // 19 * 23 = 437
    assert!(
        final_text.contains("437"),
        "agent should use calc_mul and report 437; got: {final_text}"
    );

    let used = state.history.iter().any(|m| match m {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.iter().any(|c| c.tool_name == "calc_mul")
        }
        _ => false,
    });
    assert!(used);
}
