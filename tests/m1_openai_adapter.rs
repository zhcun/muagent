//! M1-P3 测试:`OpenAiAdapter` 用 mock NetEgress 覆盖常见响应形态。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::adapters::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{
    CapabilityRegistry, Concurrency, Idempotency, ModelAdapter, ModelRequest, SideEffects,
    ToolContext, ToolDescriptor, ToolExecutor,
};
use muagent::core::tool::{PendingCall, ToolResult, TOOL_PROTOCOL_ERROR_TOOL};
use muagent::core::types::{Content, Message};
use muagent::prelude::DefaultToolExecutor;
use muagent::providers::OpenAiAdapter;
use serde_json::json;

struct MockNet {
    replies: Mutex<Vec<(u16, Vec<u8>)>>,
    last_body: Mutex<Option<Vec<u8>>>,
}

impl MockNet {
    fn new(replies: Vec<(u16, serde_json::Value)>) -> Self {
        Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|(s, v)| (s, serde_json::to_vec(&v).unwrap()))
                    .collect(),
            ),
            last_body: Mutex::new(None),
        }
    }
}

#[async_trait]
impl NetEgress for MockNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        *self.last_body.lock().unwrap() = req.body.clone();
        let mut q = self.replies.lock().unwrap();
        if q.is_empty() {
            return Err(NetErr::Io("no more mock replies".into()));
        }
        let (status, body) = q.remove(0);
        Ok(HttpResp {
            status,
            headers: HashMap::new(),
            body,
        })
    }
}

fn make_adapter(net: Arc<dyn NetEgress>) -> OpenAiAdapter {
    OpenAiAdapter::new(
        net,
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-fake".into()),
    )
}

fn fs_read_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "fs_read".into(),
        description: "read a file".into(),
        schema_json: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        }),
        timeout: std::time::Duration::from_secs(5),
        max_out_tokens: 1024,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::ReadOnly,
        idempotency: Idempotency::Idempotent,
    }
}

// Test 1: simple text reply
#[tokio::test]
async fn openai_text_reply() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {"role":"assistant","content":"hello from mock"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}
        }),
    )]));
    let a = make_adapter(net);

    let req = ModelRequest {
        system: "you are helpful".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "hello from mock");
    assert_eq!(r.usage.prompt_tokens, 10);
    assert_eq!(r.usage.completion_tokens, 3);
    assert!(r.tool_calls.is_empty());
}

// Test 2: tool call reply
#[tokio::test]
async fn openai_tool_call_reply() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {
                    "role":"assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "tc_1",
                        "type": "function",
                        "function": {
                            "name": "fs_read",
                            "arguments": "{\"uri\":\"file:///tmp/x\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28}
        }),
    )]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("read it"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].tool_name, "fs_read");
    assert_eq!(
        r.tool_calls[0].args.get("uri").unwrap().as_str().unwrap(),
        "file:///tmp/x"
    );
}

#[tokio::test]
async fn openai_parses_cache_usage_details() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {"role":"assistant","content":"ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10339,
                "completion_tokens": 60,
                "prompt_tokens_details": {
                    "cached_tokens": 4096,
                    "cache_write_tokens": 8192
                }
            }
        }),
    )]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "stable prefix".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.usage.cache_read_tokens, 4096);
    assert_eq!(r.usage.cache_write_tokens, 8192);
}

#[tokio::test]
async fn openai_native_invalid_tool_args_become_protocol_error_call() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {
                    "role":"assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "tc_bad",
                        "type": "function",
                        "function": {
                            "name": "fs_read",
                            "arguments": "{\"uri\":"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        }),
    )]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("read it"),
        }],
        tools: vec![fs_read_tool()],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].tool_name, TOOL_PROTOCOL_ERROR_TOOL);
    assert!(r.tool_calls[0].args["message"]
        .as_str()
        .unwrap()
        .contains("tc_bad"));
}

#[tokio::test]
async fn openai_refusal_text_is_not_treated_as_empty_reply() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {
                    "role":"assistant",
                    "content": null,
                    "refusal": "I can't help with that."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        }),
    )]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("unsafe"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "I can't help with that.");
    assert!(r.tool_calls.is_empty());
}

#[tokio::test]
async fn openai_content_filter_empty_reply_fails_without_retry_signal() {
    let net = Arc::new(MockNet::new(vec![(
        200,
        json!({
            "choices": [{
                "message": {"role":"assistant","content": null},
                "finish_reason": "content_filter"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 0}
        }),
    )]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("unsafe"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let err = a.turn(req, CancelToken::never()).await.unwrap_err();
    assert!(matches!(err, muagent::core::error::ModelError::Fatal(_)));
}

// Test 3: 429 → Transient
#[tokio::test]
async fn openai_429_transient() {
    let net = Arc::new(MockNet::new(vec![(429, json!({"error": "rate limit"}))]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let err = a.turn(req, CancelToken::never()).await.unwrap_err();
    assert!(matches!(
        err,
        muagent::core::error::ModelError::Transient(_)
    ));
    assert_eq!(
        err.classify(),
        muagent::core::error::ErrorClass::ProviderTransient
    );
}

// Test 4: 401 → Auth → ProviderFatal
#[tokio::test]
async fn openai_401_auth() {
    let net = Arc::new(MockNet::new(vec![(401, json!({"error": "invalid key"}))]));
    let a = make_adapter(net);
    let req = ModelRequest {
        system: "".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let err = a.turn(req, CancelToken::never()).await.unwrap_err();
    assert!(matches!(err, muagent::core::error::ModelError::Auth(_)));
    assert_eq!(
        err.classify(),
        muagent::core::error::ErrorClass::ProviderFatal
    );
}

// Test 5: request body contains tools + api key headers
#[tokio::test]
async fn openai_request_body_shape() {
    let net_inner = MockNet::new(vec![(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"ok"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}
        }),
    )]);
    let net_arc = Arc::new(net_inner);
    let a = OpenAiAdapter::new(
        net_arc.clone(),
        "https://api.openai.com/v1",
        "gpt-x",
        Some("sk-abc".into()),
    );

    let req = ModelRequest {
        system: "S".into(),
        messages: vec![Message::User {
            content: Content::text("U"),
        }],
        tools: vec![fs_read_tool()],
        temperature: Some(0.3),
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let _ = a.turn(req, CancelToken::never()).await.unwrap();

    let body_bytes = net_arc.last_body.lock().unwrap().clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["model"], "gpt-x");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "fs_read");
    let temp = body["temperature"].as_f64().unwrap();
    assert!((temp - 0.3).abs() < 1e-4, "temperature mismatch: {temp}");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "user");
    // caps
    assert!(a.caps().native_tool_use);
}

#[tokio::test]
async fn openai_fallback_tool_protocol_for_non_native_endpoint() {
    let net_inner = MockNet::new(vec![(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"<tool_call>{\"name\":\"fs_read\",\"arguments\":{\"path\":\"README.md\"},\"reason\":\"need the file\"}</tool_call>"}}],
            "usage":{"prompt_tokens":12,"completion_tokens":6}
        }),
    )]);
    let net_arc = Arc::new(net_inner);
    let a = OpenAiAdapter::new(
        net_arc.clone(),
        "https://example.com/v1",
        "local-model",
        None,
    );
    assert!(!a.caps().native_tool_use);

    let req = ModelRequest {
        system: "S".into(),
        messages: vec![Message::User {
            content: Content::text("read README"),
        }],
        tools: vec![fs_read_tool()],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();

    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].tool_name, "fs_read");
    assert_eq!(
        r.tool_calls[0].args.get("path").unwrap().as_str().unwrap(),
        "README.md"
    );
    assert_eq!(
        r.tool_calls[0].reason_from_llm.as_deref(),
        Some("need the file")
    );

    let body_bytes = net_arc.last_body.lock().unwrap().clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body.get("tools").is_none());
    let msgs = body["messages"].as_array().unwrap();
    let system = msgs[0]["content"].as_str().unwrap();
    assert!(system.contains("Tool call transport fallback"));
    assert!(system.contains("fs_read"));
    assert!(system.contains("\"required\":[\"path\"]"));
}

#[tokio::test]
async fn openai_fallback_renders_tool_history_as_text_not_native_roles() {
    let net_inner = MockNet::new(vec![(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"done"}}],
            "usage":{"prompt_tokens":20,"completion_tokens":3}
        }),
    )]);
    let net_arc = Arc::new(net_inner);
    let a = OpenAiAdapter::new(
        net_arc.clone(),
        "https://example.com/v1",
        "local-model",
        None,
    );

    let req = ModelRequest {
        system: "S".into(),
        messages: vec![
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![PendingCall::new(
                    "fallback_tool_call_1",
                    "fs_read",
                    json!({"path":"README.md"}),
                )],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "fallback_tool_call_1".into(),
                result: ToolResult::ok("file body"),
            },
        ],
        tools: vec![fs_read_tool()],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let _ = a.turn(req, CancelToken::never()).await.unwrap();

    let body_bytes = net_arc.last_body.lock().unwrap().clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(msgs.iter().all(|m| m["role"] != "tool"));
    assert!(msgs.iter().all(|m| m.get("tool_calls").is_none()));
    assert!(msgs.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("<tool_call>"))));
    assert!(msgs.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("[tool_result id=fallback_tool_call_1]"))));
}

#[tokio::test]
async fn openai_fallback_parses_multiple_json_shapes() {
    let net_inner = MockNet::new(vec![(
        200,
        json!({
            "choices":[{
                "message":{
                    "role":"assistant",
                    "content":"<tool_calls>[{\"tool_name\":\"fs_read\",\"args\":\"{\\\"path\\\":\\\"A.md\\\"}\"},{\"function\":{\"name\":\"fs_read\",\"arguments\":\"{\\\"path\\\":\\\"B.md\\\"}\"}}]</tool_calls>"
                }
            }],
            "usage":{"prompt_tokens":20,"completion_tokens":3}
        }),
    )]);
    let a = OpenAiAdapter::new(
        Arc::new(net_inner),
        "https://example.com/v1",
        "local-model",
        None,
    );

    let req = ModelRequest {
        system: "S".into(),
        messages: vec![Message::User {
            content: Content::text("read files"),
        }],
        tools: vec![fs_read_tool()],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();

    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[0].args["path"], "A.md");
    assert_eq!(r.tool_calls[1].args["path"], "B.md");
}

#[tokio::test]
async fn openai_fallback_parse_error_returns_protocol_error_call() {
    let net_inner = MockNet::new(vec![(
        200,
        json!({
            "choices":[{
                "message":{
                    "role":"assistant",
                    "content":"<tool_call>{\"name\":\"fs_read\",\"arguments\":</tool_call>"
                }
            }],
            "usage":{"prompt_tokens":20,"completion_tokens":3}
        }),
    )]);
    let a = OpenAiAdapter::new(
        Arc::new(net_inner),
        "https://example.com/v1",
        "local-model",
        None,
    );

    let req = ModelRequest {
        system: "S".into(),
        messages: vec![Message::User {
            content: Content::text("read file"),
        }],
        tools: vec![fs_read_tool()],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();

    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].tool_name, TOOL_PROTOCOL_ERROR_TOOL);
    assert!(r.tool_calls[0].args["message"]
        .as_str()
        .unwrap()
        .contains("Tool-call protocol error"));
    assert!(r.tool_calls[0].args["hint"]
        .as_str()
        .unwrap()
        .contains("<tool_call>"));
}

#[tokio::test]
async fn protocol_error_tool_returns_retryable_result_for_llm() {
    let exec = DefaultToolExecutor::new(Arc::new(CapabilityRegistry::new()));
    let call = PendingCall::new(
        "fallback_tool_protocol_error_1",
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "message": "Tool-call protocol error: bad JSON",
            "hint": "Retry with a valid <tool_call> block.",
            "errors": [{"message":"bad JSON","raw":"<tool_call>{"}]
        }),
    );

    assert_eq!(exec.idempotency_for(&call), Idempotency::Idempotent);
    assert_eq!(exec.side_effects_for(&call), SideEffects::ReadOnly);

    let result = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(result.retryable);
    assert!(result.model_text().contains("bad JSON"));
    assert!(result.model_text().contains("Retry with a valid"));
}
