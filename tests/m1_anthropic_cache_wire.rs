//! Offline: AnthropicAdapter puts `cache_control` in the right places when
//! `CachePolicy::Auto` is set, and parses cache usage from the response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::net::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::prelude::*;
use muagent::core::types::{Content, Message};
use muagent::providers::AnthropicAdapter;
use serde_json::{json, Value};

struct CapturingNet {
    captured_body: Arc<Mutex<Option<Value>>>,
    canned_response: Value,
}

#[async_trait]
impl NetEgress for CapturingNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        let body: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
        *self.captured_body.lock().unwrap() = Some(body);
        let mut headers = HashMap::new();
        headers.insert("content-type".into(), "application/json".into());
        Ok(HttpResp {
            status: 200,
            headers,
            body: serde_json::to_vec(&self.canned_response).unwrap(),
        })
    }
}

fn canned_response(cache_create: u32, cache_read: u32, input: u32, output: u32) -> Value {
    json!({
        "content": [{"type":"text","text":"ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_creation_input_tokens": cache_create,
            "cache_read_input_tokens": cache_read,
        }
    })
}

fn build_adapter(canned: Value) -> (Arc<AnthropicAdapter>, Arc<Mutex<Option<Value>>>) {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured_body: captured.clone(),
        canned_response: canned,
    });
    (
        Arc::new(AnthropicAdapter::new(
            net,
            "https://api.anthropic.com",
            "claude-haiku-4-5",
            "k",
        )),
        captured,
    )
}

fn tool_desc(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: format!("{name} tool"),
        schema_json: json!({"type":"object"}),
        timeout: std::time::Duration::from_secs(1),
        max_out_tokens: 100,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::ReadOnly,
        idempotency: Idempotency::Idempotent,
    }
}

#[tokio::test]
async fn auto_cache_marks_system_last_tool_and_first_user() {
    let (adapter, captured) = build_adapter(canned_response(0, 0, 10, 5));
    let req = ModelRequest {
        system: "you are a test bot".into(),
        messages: vec![
            Message::User {
                content: Content::text("first question"),
            },
            Message::Assistant {
                content: Content::text("first answer"),
                tool_calls: vec![],
                thinking: vec![],
            },
            Message::User {
                content: Content::text("second question"),
            },
        ],
        tools: vec![tool_desc("alpha"), tool_desc("omega")],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    adapter.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().expect("no body captured");

    assert_eq!(
        body["cache_control"]["type"], "ephemeral",
        "native Anthropic should use top-level automatic caching for growing history"
    );

    // system: array with single block carrying cache_control
    let sys = body.get("system").expect("system missing");
    let sys_arr = sys.as_array().expect("system should be array under Auto");
    assert_eq!(sys_arr.len(), 1);
    assert_eq!(sys_arr[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sys_arr[0]["text"], "you are a test bot");

    // tools: last tool gets cache_control; previous tools don't
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools[0].get("cache_control"), None);
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

    // first user message gets cache_control on its last content block.
    // Its position is fixed (history is append-only), so the wire bytes
    // around it stay byte-stable across turns — which is the precondition
    // for the cache hit to actually fire on Anthropic's strict-prefix cache.
    let msgs = body["messages"].as_array().unwrap();
    let first = &msgs[0];
    assert_eq!(first["role"], "user");
    let content = first["content"]
        .as_array()
        .expect("user content should be normalized to array");
    assert_eq!(
        content.last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );

    // second user message (NOT first) should not be marked
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    // its content could still be a string (no marker needed)
    let has_marker = match &last["content"] {
        Value::Array(arr) => arr.last().and_then(|b| b.get("cache_control")).is_some(),
        _ => false,
    };
    assert!(!has_marker, "second user turn must not be cache-marked");
}

#[tokio::test]
async fn disabled_cache_sends_no_markers() {
    let (adapter, captured) = build_adapter(canned_response(0, 0, 10, 5));
    let req = ModelRequest {
        system: "hi".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![tool_desc("alpha")],
        temperature: None,
        stream: false,
        cache: CachePolicy::Disabled,
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    adapter.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();

    // system still a plain string
    assert!(body["system"].is_string());
    // no cache_control anywhere
    let s = serde_json::to_string(&body).unwrap();
    assert!(
        !s.contains("cache_control"),
        "no markers expected; body:\n{s}"
    );
}

#[tokio::test]
async fn parses_cache_usage_from_response() {
    let (adapter, _) = build_adapter(canned_response(120, 800, 40, 10));
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let reply = adapter.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(reply.usage.cache_write_tokens, 120);
    assert_eq!(reply.usage.cache_read_tokens, 800);
    // prompt_tokens is fresh + cache write + cache read (all billable input)
    assert_eq!(reply.usage.prompt_tokens, 40 + 120 + 800);
    assert_eq!(reply.usage.completion_tokens, 10);
}
