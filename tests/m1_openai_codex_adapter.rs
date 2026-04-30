use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::adapters::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{
    Concurrency, Idempotency, ModelAdapter, ModelRequest, SideEffects, ToolDescriptor,
};
use muagent::core::types::{Content, Message};
use muagent::oauth::OpenAiCodexAuth;
use muagent::providers::OpenAiCodexAdapter;
use serde_json::json;

struct MockNet {
    replies: Mutex<Vec<(u16, Vec<u8>)>>,
    last_req: Mutex<Option<HttpReq>>,
}

impl MockNet {
    fn new(replies: Vec<(u16, Vec<u8>)>) -> Self {
        Self {
            replies: Mutex::new(replies),
            last_req: Mutex::new(None),
        }
    }

    fn last_req(&self) -> HttpReq {
        self.last_req.lock().unwrap().clone().unwrap()
    }
}

#[async_trait]
impl NetEgress for MockNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        *self.last_req.lock().unwrap() = Some(req);
        let mut replies = self.replies.lock().unwrap();
        if replies.is_empty() {
            return Err(NetErr::Io("no mock response".into()));
        }
        let (status, body) = replies.remove(0);
        Ok(HttpResp {
            status,
            headers: HashMap::new(),
            body,
        })
    }
}

fn fs_read_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "fs_read".into(),
        description: "read a file".into(),
        schema_json: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        timeout: std::time::Duration::from_secs(5),
        max_out_tokens: 1024,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::ReadOnly,
        idempotency: Idempotency::Idempotent,
    }
}

fn codex_text_sse(text: &str) -> Vec<u8> {
    format!(
        "data: {{\"type\":\"response.output_item.added\",\"item\":{{\"type\":\"message\"}}}}\n\n\
         data: {{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"usage\":{{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}}}}\n\n\
         data: [DONE]\n\n",
        serde_json::to_string(text).unwrap()
    )
    .into_bytes()
}

#[tokio::test]
async fn codex_adapter_sends_oauth_headers_and_responses_body() {
    let net = Arc::new(MockNet::new(vec![(200, codex_text_sse("hi"))]));
    let adapter = OpenAiCodexAdapter::new(
        net.clone(),
        "https://chatgpt.com/backend-api",
        "gpt-5.3-codex",
        None,
    )
    .with_auth(OpenAiCodexAuth::from_static_token(
        "access_token_123",
        "acct_123",
    ));

    let reply = adapter
        .turn(
            ModelRequest {
                system: "system prompt".into(),
                runtime_context: "runtime facts".into(),
                messages: vec![Message::User {
                    content: Content::text("hello"),
                }],
                tools: vec![fs_read_tool()],
                temperature: None,
                stream: false,
                cache: Default::default(),
                thinking: Default::default(),
                prompt_cache_key: Some("cache-key-1".into()),
            },
            CancelToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(reply.text, "hi");
    assert_eq!(reply.usage.prompt_tokens, 4);

    let req = net.last_req();
    assert_eq!(req.url, "https://chatgpt.com/backend-api/codex/responses");
    assert_eq!(
        req.headers.get("Authorization").map(String::as_str),
        Some("Bearer access_token_123")
    );
    assert_eq!(
        req.headers.get("chatgpt-account-id").map(String::as_str),
        Some("acct_123")
    );
    assert_eq!(
        req.headers.get("OpenAI-Beta").map(String::as_str),
        Some("responses=experimental")
    );

    let body: serde_json::Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
    assert_eq!(body["model"], "gpt-5.3-codex");
    assert_eq!(body["instructions"], "system prompt");
    assert_eq!(body["prompt_cache_key"], "cache-key-1");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "fs_read");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"][0]["text"], "runtime facts");
    assert_eq!(body["input"][1]["content"][0]["text"], "hello");
}

#[tokio::test]
async fn codex_adapter_parses_tool_call_sse() {
    let sse = br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"fs_read","arguments":""}}

data: {"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/a\"}"}

data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"fs_read","arguments":"{\"path\":\"/tmp/a\"}"}}

data: {"type":"response.completed","response":{"status":"completed"}}

"#
    .to_vec();
    let net = Arc::new(MockNet::new(vec![(200, sse)]));
    let adapter = OpenAiCodexAdapter::new(
        net,
        "https://chatgpt.com/backend-api",
        "gpt-5.3-codex",
        None,
    )
    .with_auth(OpenAiCodexAuth::from_static_token(
        "access_token_123",
        "acct_123",
    ));

    let reply = adapter
        .turn(
            ModelRequest {
                system: String::new(),
                runtime_context: String::new(),
                messages: vec![Message::User {
                    content: Content::text("read"),
                }],
                tools: vec![fs_read_tool()],
                temperature: None,
                stream: false,
                cache: Default::default(),
                thinking: Default::default(),
                prompt_cache_key: None,
            },
            CancelToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "call_1");
    assert_eq!(reply.tool_calls[0].tool_name, "fs_read");
    assert_eq!(reply.tool_calls[0].args["path"], "/tmp/a");
}
