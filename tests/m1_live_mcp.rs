//! Live E2E:agent 通过 MCP server 调用远程工具(stdio transport)。
//!
//! 流程:
//! 1. 编译 muagent-mcp-test-server(一次;cargo build 幂等)
//! 2. 以 stdio spawn 它,跑 McpClient::initialize + list_tools
//! 3. 用 McpSkill 包装远程工具,作为 AlwaysOn skill 装进 SkillManager
//! 4. 让真 LLM 回答 "计算 23 × 47";它必须调到 calc_mul

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::capabilities::mcp::prelude::*;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::types::{Content, Message};
use muagent::prelude::*;
use muagent::providers::OpenAiAdapter;
use muagent::storage::MemorySessionStore;
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

/// Locate (and build if needed) the test MCP server binary. Works from any
/// cwd because CARGO_MANIFEST_DIR points at this test crate.
fn mcp_test_server_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap().to_path_buf();
    // Build (idempotent — cargo is fast when nothing changed).
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "-p",
            "muagent-mcp",
            "--bin",
            "muagent-mcp-test-server",
        ])
        .current_dir(&workspace)
        .status()
        .expect("cargo build mcp test server");
    assert!(status.success(), "failed to build muagent-mcp-test-server");
    workspace.join("target/debug/muagent-mcp-test-server")
}

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_mcp_agent_calls_remote_tool() {
    let model = build_real_model();
    let registry = Arc::new(CapabilityRegistry::new());

    // Spin up MCP server over stdio and register its tools directly.
    let bin = mcp_test_server_path();
    eprintln!("-- spawning MCP server at {}", bin.display());
    let spec = StdioSpawn::new(bin.to_string_lossy().to_string());
    let transport = StdioTransport::spawn(&spec).await.expect("stdio spawn");
    let client = Arc::new(McpClient::new(Box::new(transport)));
    register_mcp_tools(&registry, client)
        .await
        .expect("register mcp tools");

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    // Sanity: calc_mul should be in the active tool set already (AlwaysOn).
    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    eprintln!("-- active tools: {:?}", names);
    assert!(
        names.contains(&"calc_mul"),
        "calc_mul should be in the active tool set; got {:?}",
        names
    );

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are a precise assistant. For multiplication, ALWAYS use the \
             calc_mul tool — do not multiply in your head.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("What is 23 times 47? Use the tool."),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 12).await;

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done, got {:?}", other),
    };
    eprintln!("-- final: {}", final_text);

    // 23 * 47 = 1081. LLM should quote it from the tool result.
    assert!(
        final_text.contains("1081"),
        "agent should use calc_mul and report 1081; got: {}",
        final_text
    );

    // history should include a calc_mul tool_call
    let used = state.history.iter().any(|m| match m {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.iter().any(|c| c.tool_name == "calc_mul")
        }
        _ => false,
    });
    assert!(used, "calc_mul should have been called");
}
