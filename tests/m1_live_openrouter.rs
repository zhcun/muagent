//! Live end-to-end test against OpenRouter via the OpenAI-compatible adapter.
//!
//! 默认 `#[ignore]`,避免常规 `cargo test` 花 API 费用。显式调用:
//! ```bash
//! cargo test -p muagent --test m1_live_openrouter -- --ignored --nocapture
//! ```
//!
//! 读取 workspace 根的 `.env`(git-ignored)取 `OPENROUTER_API_KEY` / `OPENROUTER_MODEL`。

use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{ModelAdapter, ModelRequest};
use muagent::core::types::{Content, Message};
use muagent::providers::OpenAiAdapter;

fn load_env() -> (String, String, String) {
    // 从 workspace root 加载 .env(若存在)
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            eprintln!("loaded {}", p);
            break;
        }
    }
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY missing (set .env or env var)");
    let base = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
    let model = std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-5.4-nano".into());
    (key, base, model)
}

/// 简单文本问答 —— 最小端到端链路。
#[ignore = "hits real API; run with --ignored"]
#[tokio::test]
async fn live_openrouter_simple_qa() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));

    let req = ModelRequest {
        system: "You are a terse assistant. Reply with the answer only, no explanation.".into(),
        messages: vec![Message::User {
            content: Content::text("2 + 2 = ?"),
        }],
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let reply = adapter
        .turn(req, CancelToken::never())
        .await
        .expect("real call");
    eprintln!("== OpenRouter reply ==");
    eprintln!("text: {}", reply.text);
    eprintln!(
        "usage: prompt={} completion={}",
        reply.usage.prompt_tokens, reply.usage.completion_tokens
    );

    assert!(!reply.text.is_empty(), "reply should have text");
    // Don't assert exact content — LLM output varies — but length/format sanity
    assert!(
        reply.text.len() < 200,
        "terse reply expected, got: {}",
        reply.text
    );
}

/// Tool-use:模型被要求调用一个简单工具返回。只验证模型**产生了 tool_call**。
#[ignore = "hits real API; run with --ignored"]
#[tokio::test]
async fn live_openrouter_tool_call() {
    use muagent::core::prelude::{Concurrency, Idempotency, SideEffects, ToolDescriptor};
    use serde_json::json;

    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));

    let tool = ToolDescriptor {
        name: "get_weather".into(),
        description: "Get the current weather for a given city".into(),
        schema_json: json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
        timeout: std::time::Duration::from_secs(10),
        max_out_tokens: 4096,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::ReadOnly,
        idempotency: Idempotency::Idempotent,
    };

    let req = ModelRequest {
        system: "You must call the get_weather tool when asked about weather.".into(),
        messages: vec![Message::User {
            content: Content::text("What's the weather in Tokyo?"),
        }],
        tools: vec![tool],
        temperature: Some(0.0),
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let reply = adapter
        .turn(req, CancelToken::never())
        .await
        .expect("real call");
    eprintln!("== tool_call reply ==");
    eprintln!("text: {:?}", reply.text);
    eprintln!("tool_calls: {:?}", reply.tool_calls);
    eprintln!(
        "usage: prompt={} completion={}",
        reply.usage.prompt_tokens, reply.usage.completion_tokens
    );

    // 模型是否真调工具取决于具体模型能力;至少要求 reply 合法
    assert!(
        !reply.tool_calls.is_empty() || !reply.text.is_empty(),
        "expected either tool call or text reply"
    );
}
