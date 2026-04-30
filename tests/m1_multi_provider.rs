//! M1-P3 多模型 provider 测试:OpenAI 多模态 / Anthropic / Gemini。
//!
//! 用 MockNet 覆盖请求序列化 + 响应反序列化,避免发真实 HTTP。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::adapters::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{ModelAdapter, ModelError, ModelRequest};
use muagent::core::types::{Content, ContentPart, Message, ObsKind};
use serde_json::json;

// ============ MockNet (records the last request body) ============

/// Captured outbound request: (url, body, headers). Aliased so the type
/// signature stays readable in the struct field below.
type CapturedReq = (String, Vec<u8>, HashMap<String, String>);

struct MockNet {
    reply: Mutex<Option<(u16, Vec<u8>)>>,
    last: Mutex<Option<CapturedReq>>,
}

impl MockNet {
    fn new(status: u16, body: serde_json::Value) -> Self {
        Self {
            reply: Mutex::new(Some((status, serde_json::to_vec(&body).unwrap()))),
            last: Mutex::new(None),
        }
    }
    fn new_raw(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            reply: Mutex::new(Some((status, body.into()))),
            last: Mutex::new(None),
        }
    }
    fn last_body(&self) -> Vec<u8> {
        self.last.lock().unwrap().as_ref().unwrap().1.clone()
    }
    fn last_url(&self) -> String {
        self.last.lock().unwrap().as_ref().unwrap().0.clone()
    }
    fn last_headers(&self) -> HashMap<String, String> {
        self.last.lock().unwrap().as_ref().unwrap().2.clone()
    }
}

#[async_trait]
impl NetEgress for MockNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        *self.last.lock().unwrap() = Some((
            req.url.clone(),
            req.body.clone().unwrap_or_default(),
            req.headers.clone(),
        ));
        let (status, body) = self.reply.lock().unwrap().clone().unwrap();
        Ok(HttpResp {
            status,
            headers: HashMap::new(),
            body,
        })
    }
}

// ============ OpenAI multimodal ============

#[tokio::test]
async fn openai_multimodal_image_in_user_message() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"I see a cat"}}],
            "usage":{"prompt_tokens":50,"completion_tokens":4}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );

    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![
                ContentPart::Text {
                    text: "what is in this image?".into(),
                },
                ContentPart::Image {
                    uri: None,
                    b64: Some("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".into()),
                    mime: "image/png".into(),
                },
            ]),
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
    assert_eq!(r.text, "I see a cat");

    // Verify the request body contains multimodal content array
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let user_content = &msgs.last().unwrap()["content"];
    assert!(
        user_content.is_array(),
        "multimodal content should be an array"
    );
    let arr = user_content.as_array().unwrap();
    assert!(arr.iter().any(|p| p["type"] == "text"));
    let img = arr
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("image_url part");
    assert!(img["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn openai_tool_result_reaches_next_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"done"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );
    let call =
        muagent::core::tool::PendingCall::new("call_1", "fs_read", json!({"uri":"file:///x"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "call_1".into(),
                result: muagent::core::tool::ToolResult::ok("file contents"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msg = &body["messages"][2];
    assert_eq!(msg["role"], "tool");
    assert_eq!(msg["tool_call_id"], "call_1");
    assert_eq!(msg["content"], "file contents");
}

#[tokio::test]
async fn openai_tool_result_image_is_bridged_after_tool_result_block() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"done"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );
    let image_call = muagent::core::tool::PendingCall::new(
        "call_img",
        "fs_read",
        json!({"uri":"file:///x.png"}),
    );
    let text_call = muagent::core::tool::PendingCall::new(
        "call_txt",
        "fs_read",
        json!({"uri":"file:///x.txt"}),
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("inspect"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![image_call, text_call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "call_img".into(),
                result: muagent::core::tool::ToolResult::ok_parts(vec![
                    ContentPart::Text {
                        text: "-- (image/png image; attached for the model) --".into(),
                    },
                    ContentPart::Image {
                        uri: None,
                        b64: Some("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".into()),
                        mime: "image/png".into(),
                    },
                ]),
            },
            Message::ToolResult {
                call_id: "call_txt".into(),
                result: muagent::core::tool::ToolResult::ok("plain text"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);

    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "call_img");
    assert!(msgs[2]["content"].as_str().unwrap().contains("image/png"));

    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_txt");
    assert_eq!(msgs[3]["content"], "plain text");

    assert_eq!(msgs[4]["role"], "user");
    let bridge = msgs[4]["content"].as_array().unwrap();
    assert!(bridge[0]["text"].as_str().unwrap().contains("call_img"));
    let img = bridge
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("bridged image");
    assert!(img["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn openai_tool_error_hint_reaches_next_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"done"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );
    let call =
        muagent::core::tool::PendingCall::new("call_1", "fs_read", json!({"uri":"file:///x"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "call_1".into(),
                result: muagent::core::tool::ToolResult::err(
                    "missing file",
                    false,
                    Some("try fs_list on the parent directory".into()),
                ),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let content = body["messages"][2]["content"].as_str().unwrap();
    assert!(content.contains("tool error: missing file"));
    assert!(content.contains("retryable: false"));
    assert!(content.contains("hint: try fs_list on the parent directory"));
}

#[tokio::test]
async fn openai_skips_empty_assistant_history_message() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"ok"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("first"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![],
                thinking: vec![],
            },
            Message::User {
                content: Content::text("second"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().all(|m| m["role"] == "user"));
}

#[tokio::test]
async fn openai_normalizes_empty_user_content_before_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"ok"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("sk-x".into()),
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![]),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    assert_eq!(body["messages"][0]["content"], "[empty content]");
}

// ============ Anthropic basic ============

#[tokio::test]
async fn anthropic_text_reply_and_usage() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"hello from claude"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":30,"output_tokens":5}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );

    let req = ModelRequest {
        system: "you are kind".into(),
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
    assert_eq!(r.text, "hello from claude");
    assert_eq!(r.usage.prompt_tokens, 30);
    assert_eq!(r.usage.completion_tokens, 5);

    // Verify request:system as top-level field;headers set
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    assert_eq!(body["system"], "you are kind");
    assert!(body["messages"].as_array().unwrap().len() == 1);
    let headers = net.last_headers();
    assert_eq!(headers.get("x-api-key"), Some(&"sk-ant-x".to_string()));
    assert!(headers.contains_key("anthropic-version"));
    assert!(net.last_url().contains("/v1/messages"));
}

#[tokio::test]
async fn anthropic_skips_empty_assistant_history_message() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":10,"output_tokens":1}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("first"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![],
                thinking: vec![],
            },
            Message::User {
                content: Content::text("second"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().all(|m| m["role"] == "user"));
}

#[tokio::test]
async fn anthropic_normalizes_empty_user_content_before_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":10,"output_tokens":1}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![]),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    assert_eq!(body["messages"][0]["content"][0]["text"], "[empty content]");
}

#[tokio::test]
async fn anthropic_tool_use_roundtrip() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[
                {"type":"text","text":"let me read that file"},
                {"type":"tool_use","id":"toolu_01","name":"fs_read","input":{"uri":"file:///x"}}
            ],
            "usage":{"input_tokens":40,"output_tokens":8}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net,
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("read x"),
        }],
        tools: vec![muagent::core::prelude::ToolDescriptor {
            name: "fs_read".into(),
            description: "read".into(),
            schema_json: json!({"type":"object","properties":{"uri":{"type":"string"}}}),
            timeout: std::time::Duration::from_secs(5),
            max_out_tokens: 4096,
            concurrency: muagent::core::prelude::Concurrency::Parallel,
            side_effects: muagent::core::prelude::SideEffects::ReadOnly,
            idempotency: muagent::core::prelude::Idempotency::Idempotent,
        }],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "let me read that file");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "toolu_01");
    assert_eq!(r.tool_calls[0].tool_name, "fs_read");
}

#[tokio::test]
async fn anthropic_tool_result_reaches_next_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"done"}],
            "usage":{"input_tokens":10,"output_tokens":1}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );
    let call =
        muagent::core::tool::PendingCall::new("toolu_1", "fs_read", json!({"uri":"file:///x"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "toolu_1".into(),
                result: muagent::core::tool::ToolResult::ok("file contents"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let block = &body["messages"][2]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "toolu_1");
    assert_eq!(block["content"], "file contents");
    assert_eq!(block["is_error"], false);
}

#[tokio::test]
async fn anthropic_batches_consecutive_tool_results_in_one_user_message() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"done"}],
            "usage":{"input_tokens":10,"output_tokens":1}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );
    let call_a =
        muagent::core::tool::PendingCall::new("toolu_a", "fs_read", json!({"uri":"file:///a"}));
    let call_b =
        muagent::core::tool::PendingCall::new("toolu_b", "fs_read", json!({"uri":"file:///b"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read both"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call_a, call_b],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "toolu_a".into(),
                result: muagent::core::tool::ToolResult::ok("a contents"),
            },
            Message::ToolResult {
                call_id: "toolu_b".into(),
                result: muagent::core::tool::ToolResult::ok("b contents"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2]["role"], "user");
    let blocks = msgs[2]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["tool_use_id"], "toolu_a");
    assert_eq!(blocks[0]["content"], "a contents");
    assert_eq!(blocks[1]["tool_use_id"], "toolu_b");
    assert_eq!(blocks[1]["content"], "b contents");
}

#[tokio::test]
async fn anthropic_tool_error_hint_reaches_next_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"done"}],
            "usage":{"input_tokens":10,"output_tokens":1}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk-ant-x",
    );
    let call =
        muagent::core::tool::PendingCall::new("toolu_1", "fs_read", json!({"uri":"file:///x"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "toolu_1".into(),
                result: muagent::core::tool::ToolResult::err(
                    "missing file",
                    false,
                    Some("try fs_list on the parent directory".into()),
                ),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let block = &body["messages"][2]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["is_error"], true);
    let content = block["content"].as_str().unwrap();
    assert!(content.contains("tool error: missing file"));
    assert!(content.contains("retryable: false"));
    assert!(content.contains("hint: try fs_list on the parent directory"));
}

#[tokio::test]
async fn anthropic_image_input() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "content":[{"type":"text","text":"I see a chart"}],
            "usage":{"input_tokens":100,"output_tokens":6}
        }),
    ));
    let a = muagent::providers::AnthropicAdapter::new(
        net.clone(),
        "https://api.anthropic.com",
        "claude-haiku-4-5",
        "sk",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![
                ContentPart::Text {
                    text: "analyze this".into(),
                },
                ContentPart::Image {
                    uri: None,
                    b64: Some("iVBOR".into()),
                    mime: "image/png".into(),
                },
            ]),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let _ = a.turn(req, CancelToken::never()).await.unwrap();

    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let user = &body["messages"][0];
    let content_arr = user["content"].as_array().unwrap();
    let img = content_arr
        .iter()
        .find(|c| c["type"] == "image")
        .expect("image block");
    assert_eq!(img["source"]["type"], "base64");
    assert_eq!(img["source"]["media_type"], "image/png");
    assert_eq!(img["source"]["data"], "iVBOR");
}

#[tokio::test]
async fn anthropic_401_auth() {
    let net = Arc::new(MockNet::new(401, json!({"error":"invalid_api_key"})));
    let a = muagent::providers::AnthropicAdapter::new(net, "https://api.anthropic.com", "c", "bad");
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
}

// ============ Google Gemini basic ============

#[tokio::test]
async fn gemini_summary_observation_is_authoritative_user_context() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "content":{"role":"model","parts":[{"text":"ok"}]},
                "finish_reason":"STOP"
            }],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "AIza-fake",
    );
    let req = ModelRequest {
        system: "base rules".into(),
        messages: vec![
            Message::Observation {
                kind: ObsKind::Summary,
                text: "older task facts".into(),
            },
            Message::User {
                content: Content::text("continue"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let summary = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
    assert!(summary.contains("[conversation summary]"));
    assert!(summary.contains("Authoritative prior conversation memory"));
    assert!(summary.contains("older task facts"));
    assert_eq!(body["contents"][1]["role"], "user");
}

#[tokio::test]
async fn gemini_text_reply_and_usage() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "content":{"role":"model","parts":[{"text":"hello from gemini"}]},
                "finish_reason":"STOP"
            }],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":3}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "AIza-fake",
    );
    let req = ModelRequest {
        system: "you are kind".into(),
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
    assert_eq!(r.text, "hello from gemini");
    assert_eq!(r.usage.prompt_tokens, 20);
    assert_eq!(r.usage.completion_tokens, 3);

    // Verify system instruction separated
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "you are kind"
    );
    assert!(net
        .last_url()
        .contains("gemini-3.1-flash-lite-preview:generateContent"));
    assert_eq!(
        net.last_headers().get("x-goog-api-key"),
        Some(&"AIza-fake".to_string())
    );
}

#[tokio::test]
async fn gemini_prompt_feedback_block_is_provider_fatal() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "promptFeedback": {"blockReason": "SAFETY"},
            "usageMetadata":{"promptTokenCount":20}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net,
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "AIza-fake",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("blocked"),
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

#[tokio::test]
async fn gemini_blocking_finish_reason_without_content_is_provider_fatal() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "finishReason":"MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net,
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-pro-preview",
        "k",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("call tool"),
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

#[tokio::test]
async fn gemini_skips_empty_assistant_history_message() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "content":{"role":"model","parts":[{"text":"ok"}]},
                "finish_reason":"STOP"
            }],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":3}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "AIza-fake",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("first"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![],
                thinking: vec![],
            },
            Message::User {
                content: Content::text("second"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 2);
    assert!(contents.iter().all(|m| m["role"] == "user"));
}

#[tokio::test]
async fn gemini_normalizes_empty_user_parts_before_request() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "content":{"role":"model","parts":[{"text":"ok"}]},
                "finish_reason":"STOP"
            }],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":3}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "AIza-fake",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![]),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let parts = body["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["text"], "[empty content]");
}

#[tokio::test]
async fn gemini_function_call_output() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{
                "content":{
                    "role":"model",
                    "parts":[
                        {"text":"let me read that"},
                        {"functionCall":{"name":"fs_read","args":{"uri":"file:///x"}}}
                    ]
                }
            }],
            "usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":10}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net,
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-pro-preview",
        "k",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::text("read"),
        }],
        tools: vec![muagent::core::prelude::ToolDescriptor {
            name: "fs_read".into(),
            description: "read".into(),
            schema_json: json!({"type":"object"}),
            timeout: std::time::Duration::from_secs(5),
            max_out_tokens: 4096,
            concurrency: muagent::core::prelude::Concurrency::Parallel,
            side_effects: muagent::core::prelude::SideEffects::ReadOnly,
            idempotency: muagent::core::prelude::Idempotency::Idempotent,
        }],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let r = a.turn(req, CancelToken::never()).await.unwrap();
    assert_eq!(r.text, "let me read that");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].tool_name, "fs_read");
    assert_eq!(r.tool_calls[0].args["uri"], "file:///x");
}

#[tokio::test]
async fn gemini_tool_result_uses_function_response() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"done"}]}}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-pro-preview",
        "k",
    );
    let call = muagent::core::tool::PendingCall::new(
        "fc_fs_read_1",
        "fs_read",
        json!({"uri":"file:///x"}),
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "fc_fs_read_1".into(),
                result: muagent::core::tool::ToolResult::ok("file contents"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let part = &body["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(part["name"], "fs_read");
    assert_eq!(part["response"]["ok"], true);
    assert_eq!(part["response"]["retryable"], false);
    assert_eq!(part["response"]["content"], "file contents");
}

#[tokio::test]
async fn gemini_tool_result_image_is_nested_inside_function_response() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"done"}]}}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-pro-preview",
        "k",
    );
    let call =
        muagent::core::tool::PendingCall::new("fc_img", "fs_read", json!({"uri":"file:///x.png"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("inspect image"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "fc_img".into(),
                result: muagent::core::tool::ToolResult::ok_parts(vec![
                    ContentPart::Text {
                        text: "image attached".into(),
                    },
                    ContentPart::Image {
                        uri: None,
                        b64: Some("AAAA".into()),
                        mime: "image/png".into(),
                    },
                ]),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let function_response = &body["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(function_response["name"], "fs_read");
    assert_eq!(function_response["response"]["content"], "image attached");
    assert_eq!(
        function_response["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(function_response["parts"][0]["inlineData"]["data"], "AAAA");
    assert_eq!(
        function_response["parts"][0]["inlineData"]["displayName"],
        "tool_image_1.png"
    );
    assert!(function_response["response"].get("attachments").is_none());
}

#[tokio::test]
async fn gemini_batches_consecutive_tool_results_in_one_user_content() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"done"}]}}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-pro-preview",
        "k",
    );
    let call_a =
        muagent::core::tool::PendingCall::new("fc_a", "fs_read", json!({"uri":"file:///a"}));
    let call_b =
        muagent::core::tool::PendingCall::new("fc_b", "fs_stat", json!({"uri":"file:///b"}));
    let req = ModelRequest {
        system: "".into(),
        messages: vec![
            Message::User {
                content: Content::text("read both"),
            },
            Message::Assistant {
                content: Content::text(""),
                tool_calls: vec![call_a, call_b],
                thinking: vec![],
            },
            Message::ToolResult {
                call_id: "fc_a".into(),
                result: muagent::core::tool::ToolResult::ok("a contents"),
            },
            Message::ToolResult {
                call_id: "fc_b".into(),
                result: muagent::core::tool::ToolResult::ok("b stat"),
            },
        ],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };

    let _ = a.turn(req, CancelToken::never()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[2]["role"], "user");
    let parts = contents[2]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["functionResponse"]["name"], "fs_read");
    assert_eq!(
        parts[0]["functionResponse"]["response"]["content"],
        "a contents"
    );
    assert_eq!(parts[1]["functionResponse"]["name"], "fs_stat");
    assert_eq!(
        parts[1]["functionResponse"]["response"]["content"],
        "b stat"
    );
}

#[tokio::test]
async fn gemini_image_inline_data() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]}}],
            "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}
        }),
    ));
    let a = muagent::providers::GoogleGeminiAdapter::new(
        net.clone(),
        "https://generativelanguage.googleapis.com",
        "gemini-3.1-flash-lite-preview",
        "k",
    );
    let req = ModelRequest {
        system: "".into(),
        messages: vec![Message::User {
            content: Content::Parts(vec![
                ContentPart::Text {
                    text: "what?".into(),
                },
                ContentPart::Image {
                    uri: None,
                    b64: Some("AAAA".into()),
                    mime: "image/jpeg".into(),
                },
            ]),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        runtime_context: String::new(),
        prompt_cache_key: None,
    };
    let _ = a.turn(req, CancelToken::never()).await.unwrap();

    let body: serde_json::Value = serde_json::from_slice(&net.last_body()).unwrap();
    let parts = body["contents"][0]["parts"].as_array().unwrap();
    let img = parts
        .iter()
        .find(|p| p.get("inlineData").is_some())
        .expect("inlineData");
    assert_eq!(img["inlineData"]["mimeType"], "image/jpeg");
    assert_eq!(img["inlineData"]["data"], "AAAA");
}

// ============ Ollama / OpenRouter compatibility (OpenAI adapter) ============

#[tokio::test]
async fn openai_adapter_works_for_ollama_baseurl() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"local reply"}}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "http://127.0.0.1:11434/v1", // Ollama default
        "llama3.2",
        None, // Ollama 通常无 key
    );
    let req = ModelRequest {
        system: "".into(),
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
    assert_eq!(r.text, "local reply");
    assert!(net.last_url().starts_with("http://127.0.0.1:11434/v1"));
    // 无 api_key → 无 Authorization header
    assert!(!net.last_headers().contains_key("Authorization"));
}

#[tokio::test]
async fn openai_adapter_works_for_openrouter_baseurl() {
    let net = Arc::new(MockNet::new(
        200,
        json!({
            "choices":[{"message":{"role":"assistant","content":"via openrouter"}}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}
        }),
    ));
    let a = muagent::providers::OpenAiAdapter::new(
        net.clone(),
        "https://openrouter.ai/api/v1",
        "anthropic/claude-haiku-4.5",
        Some("sk-or-x".into()),
    );
    let req = ModelRequest {
        system: "".into(),
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
    assert_eq!(r.text, "via openrouter");
    assert!(net.last_url().starts_with("https://openrouter.ai/api/v1"));
    assert_eq!(
        net.last_headers().get("Authorization"),
        Some(&"Bearer sk-or-x".to_string())
    );
    assert_eq!(
        net.last_headers().get("Accept-Encoding"),
        Some(&"identity".to_string())
    );
}

#[tokio::test]
async fn openrouter_malformed_200_response_is_transient() {
    let net = Arc::new(MockNet::new_raw(200, b"not json".to_vec()));
    let a = muagent::providers::OpenAiAdapter::new(
        net,
        "https://openrouter.ai/api/v1",
        "openai/gpt-5.4-nano",
        Some("sk-or-x".into()),
    );
    let req = ModelRequest {
        system: "".into(),
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

    let err = a.turn(req, CancelToken::never()).await.unwrap_err();
    assert!(matches!(err, ModelError::Transient(_)));
}
