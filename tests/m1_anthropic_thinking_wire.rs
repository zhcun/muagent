//! Offline: AnthropicAdapter thinking wire format.
//! - Enabled mode → body has `thinking: {type:"enabled", budget_tokens}`.
//! - Thinking blocks in response are parsed as ThinkingArtifact with
//!   ReplayPolicy::MustReplayUnmodified.
//! - On replay, an Assistant message carrying attached thinking emits
//!   `thinking` blocks back to the wire in the correct order (thinking
//!   before text/tool_use).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::net::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::prelude::*;
use muagent::core::thinking::{
    ReplayPolicy, ThinkingArtifact, ThinkingConfig, ThinkingEffort, ThinkingKind, ThinkingMode,
    ThinkingPayload, ThinkingVisibility,
};
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

fn build(canned: Value) -> (Arc<AnthropicAdapter>, Arc<Mutex<Option<Value>>>) {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured_body: captured.clone(),
        canned_response: canned,
    });
    (
        Arc::new(
            AnthropicAdapter::new(net, "https://api.anthropic.com", "claude-haiku-4-5", "k")
                .with_max_tokens(64_000),
        ),
        captured,
    )
}

fn simple_response() -> Value {
    json!({
        "content": [{"type":"text","text":"ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

#[tokio::test]
async fn auto_mode_does_not_force_thinking() {
    // Auto is conservative for Anthropic — doesn't force thinking unless
    // explicitly enabled. Avoids surprise billing on cheap haiku models.
    let (a, captured) = build(simple_response());
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: ThinkingConfig::auto(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    assert!(
        body.get("thinking").is_none(),
        "Auto must NOT enable thinking by default; body: {}",
        body
    );
}

#[tokio::test]
async fn enabled_effort_high_sets_budget() {
    let (a, captured) = build(simple_response());
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: ThinkingConfig::enabled_effort(ThinkingEffort::High),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let t = body.get("thinking").expect("thinking missing");
    assert_eq!(t["type"], "enabled");
    let budget = t["budget_tokens"].as_u64().unwrap();
    assert!(
        budget >= 16_000,
        "high effort should map to >= 16k budget, got {budget}"
    );
}

#[tokio::test]
async fn enabled_explicit_token_budget() {
    let (a, captured) = build(simple_response());
    let mut t = ThinkingConfig::enabled_budget(8_000);
    // Make sure explicit wins over effort
    t.effort = Some(ThinkingEffort::Low);
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: t,
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(body["thinking"]["budget_tokens"], 8_000);
}

#[tokio::test]
async fn parses_thinking_blocks_into_must_replay_artifacts() {
    let resp = json!({
        "content": [
            {"type":"thinking","thinking":"let me think about it...","signature":"abc123"},
            {"type":"redacted_thinking","data":"ENCRYPTED_BLOB=="},
            {"type":"text","text":"answer"}
        ],
        "stop_reason":"end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 12}
    });
    let (a, _) = build(resp);
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: ThinkingConfig::enabled_effort(ThinkingEffort::Medium),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let reply = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(reply.text, "answer");
    assert_eq!(reply.thinking.len(), 2);
    // First: full thinking text with signature.
    assert_eq!(reply.thinking[0].kind, ThinkingKind::FullText);
    assert_eq!(reply.thinking[0].replay, ReplayPolicy::MustReplayUnmodified);
    assert_eq!(reply.thinking[0].visibility, ThinkingVisibility::Hidden);
    assert_eq!(
        reply.thinking[0].provider_signature.as_deref(),
        Some("abc123")
    );
    match &reply.thinking[0].payload {
        ThinkingPayload::Text { text } => assert!(text.contains("let me think")),
        other => panic!("expected text payload, got {other:?}"),
    }
    // Second: redacted / opaque.
    assert_eq!(reply.thinking[1].kind, ThinkingKind::RedactedOpaque);
    assert_eq!(reply.thinking[1].replay, ReplayPolicy::MustReplayUnmodified);
    match &reply.thinking[1].payload {
        ThinkingPayload::OpaqueBytes { b64 } => assert_eq!(b64, "ENCRYPTED_BLOB=="),
        other => panic!("expected opaque payload, got {other:?}"),
    }
    // thinking_tokens estimated from text length (redacted has no cost here).
    assert!(reply.usage.thinking_tokens > 0);
}

#[tokio::test]
async fn replays_thinking_blocks_on_next_turn() {
    let (a, captured) = build(simple_response());

    // Simulate a history where the assistant already emitted thinking blocks
    // in a previous turn and must replay them unchanged on this turn.
    let prev_thinking = vec![
        ThinkingArtifact {
            provider: "anthropic".into(),
            kind: ThinkingKind::FullText,
            replay: ReplayPolicy::MustReplayUnmodified,
            visibility: ThinkingVisibility::Hidden,
            payload: ThinkingPayload::Text {
                text: "reasoning step 1".into(),
            },
            provider_signature: Some("sig-A".into()),
        },
        ThinkingArtifact {
            provider: "anthropic".into(),
            kind: ThinkingKind::RedactedOpaque,
            replay: ReplayPolicy::MustReplayUnmodified,
            visibility: ThinkingVisibility::Hidden,
            payload: ThinkingPayload::OpaqueBytes {
                b64: "OPAQUE==".into(),
            },
            provider_signature: None,
        },
    ];

    let req = ModelRequest {
        system: "s".into(),
        messages: vec![
            Message::User {
                content: Content::text("q1"),
            },
            Message::Assistant {
                content: Content::text("reasoning out loud"),
                tool_calls: vec![PendingCall::new("tc1", "some_tool", json!({"x": 1}))],
                thinking: prev_thinking,
            },
            Message::ToolResult {
                call_id: "tc1".into(),
                result: ToolResult::ok("tool output"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: ThinkingConfig {
            mode: ThinkingMode::Enabled,
            effort: Some(ThinkingEffort::Medium),
            ..Default::default()
        },
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
    let blocks = asst["content"].as_array().unwrap();

    // Order contract: thinking blocks FIRST, then text, then tool_use.
    assert_eq!(
        blocks[0]["type"], "thinking",
        "first block must be thinking (Anthropic protocol); got {:?}",
        blocks
    );
    assert_eq!(blocks[0]["thinking"], "reasoning step 1");
    assert_eq!(blocks[0]["signature"], "sig-A");
    assert_eq!(blocks[1]["type"], "redacted_thinking");
    assert_eq!(blocks[1]["data"], "OPAQUE==");
    // Then the visible text.
    assert_eq!(blocks[2]["type"], "text");
    assert_eq!(blocks[2]["text"], "reasoning out loud");
    // Then the tool_use.
    assert_eq!(blocks[3]["type"], "tool_use");
    assert_eq!(blocks[3]["id"], "tc1");
}

#[tokio::test]
async fn off_mode_omits_thinking_field() {
    let (a, captured) = build(simple_response());
    let req = ModelRequest {
        system: "s".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: ThinkingConfig::off(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    assert!(body.get("thinking").is_none());
}
