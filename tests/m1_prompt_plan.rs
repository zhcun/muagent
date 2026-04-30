//! Offline: PromptPlan types + runtime_context wire placement.
//! Design invariant (16-prompt-design): L2 runtime context MUST sit
//! AFTER the cacheable L0+L1 prefix and MUST NOT carry a cache marker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::net::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::prelude::*;
use muagent::core::testing::reply;
use muagent::core::types::{Content, Message, ObsKind};
use muagent::providers::AnthropicAdapter;
use muagent::providers::OpenAiAdapter;
use muagent::storage::MemorySessionStore;
use serde_json::{json, Value};
use uuid::Uuid;

// === shared capturing net ===

struct CapturingNet {
    captured: Arc<Mutex<Option<Value>>>,
    canned: Value,
}

#[async_trait]
impl NetEgress for CapturingNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        let body: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
        *self.captured.lock().unwrap() = Some(body);
        let mut h = HashMap::new();
        h.insert("content-type".into(), "application/json".into());
        Ok(HttpResp {
            status: 200,
            headers: h,
            body: serde_json::to_vec(&self.canned).unwrap(),
        })
    }
}

struct CapturingModel {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    caps: LlmCaps,
}

#[async_trait]
impl ModelAdapter for CapturingModel {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(
        &self,
        req: ModelRequest,
        _cancel: CancelToken,
    ) -> Result<ModelReply, ModelError> {
        self.requests.lock().unwrap().push(req);
        Ok(reply::text("done"))
    }
}

struct NoopTools;

#[async_trait]
impl ToolExecutor for NoopTools {
    async fn execute(
        &self,
        _call: &PendingCall,
        _ctx: &ToolContext,
        _cancel: CancelToken,
    ) -> Result<ToolResult, ToolExecutorError> {
        Ok(ToolResult::ok(""))
    }

    fn idempotency_for(&self, _call: &PendingCall) -> Idempotency {
        Idempotency::Idempotent
    }
}

async fn captured_runner_request(prompt_augmentation: &'static str) -> ModelRequest {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(CapturingModel {
        requests: requests.clone(),
        caps: runner_test_caps(false),
    });
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(NoopTools))
        .store(Arc::new(MemorySessionStore::new()))
        .base_system_prompt("base stable prompt")
        .tools_provider(move |_state: &RunState| ActiveToolSet {
            prompt_augmentation: prompt_augmentation.to_string(),
            ..Default::default()
        })
        .build()
        .expect("runner build");

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
            },
        )
        .await
        .unwrap();
    for _ in 0..3 {
        runner.step(&mut state).await.unwrap();
        if !requests.lock().unwrap().is_empty() {
            break;
        }
    }

    let req = requests
        .lock()
        .unwrap()
        .pop()
        .expect("runner should issue one model request");
    req
}

async fn captured_runner_request_with_caps(
    caps: LlmCaps,
    tools: Vec<ToolDescriptor>,
) -> ModelRequest {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(CapturingModel {
        requests: requests.clone(),
        caps,
    });
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(NoopTools))
        .store(Arc::new(MemorySessionStore::new()))
        .base_system_prompt("base stable prompt")
        .tools_provider(move |_state: &RunState| ActiveToolSet {
            tools: tools.clone(),
            ..Default::default()
        })
        .build()
        .expect("runner build");

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("inspect image if needed"),
            },
        )
        .await
        .unwrap();
    for _ in 0..3 {
        runner.step(&mut state).await.unwrap();
        if !requests.lock().unwrap().is_empty() {
            break;
        }
    }

    let req = requests
        .lock()
        .unwrap()
        .pop()
        .expect("runner should issue one model request");
    req
}

fn runner_test_caps(vision: bool) -> LlmCaps {
    LlmCaps {
        native_tool_use: true,
        vision,
        ctx_len: 8192,
        prompt_cache: true,
        ..Default::default()
    }
}

fn fs_read_descriptor_for_prompt_test() -> ToolDescriptor {
    ToolDescriptor {
        name: "fs_read".into(),
        description: "Read a file.".into(),
        schema_json: json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string"}
            },
            "required": ["uri"]
        }),
        timeout: std::time::Duration::from_secs(10),
        max_out_tokens: 4096,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::ReadOnly,
        idempotency: Idempotency::Idempotent,
    }
}

// === PromptPlan unit tests (already covered in core's inline tests;
// here we add a quick integration-layer smoke) ===

#[test]
fn prompt_plan_cacheable_prefix_excludes_runtime_context() {
    let p = PromptPlan {
        invariant: "You are μAgent.".into(),
        session_sticky: "Workspace: /home/dev".into(),
        runtime_context: "current_date_utc: 2023-11-14\nturn: 5".into(),
    };
    let cp = p.cacheable_prefix();
    assert!(cp.contains("You are μAgent."));
    assert!(cp.contains("Workspace: /home/dev"));
    assert!(!cp.contains("current_date_utc"));
    assert!(!cp.contains("turn: 5"));
}

#[test]
fn runtime_facts_skips_empty_fields() {
    let r = RuntimeFacts {
        now_ms: 0,
        turn: 0,
        extra: vec![],
    };
    assert_eq!(r.render(), "");
    let r2 = RuntimeFacts {
        now_ms: 1_700_000_000_000,
        turn: 0,
        extra: vec![],
    };
    assert!(r2.render().contains("current_date_utc: 2023-11-14"));
    assert!(!r2.render().contains("now_ms"));
    assert!(!r2.render().contains("turn"));
}

#[tokio::test]
async fn runner_prefix_hash_tracks_final_cacheable_prefix() {
    let a = captured_runner_request("## Skill hints\n- use alpha workflow").await;
    let b = captured_runner_request("## Skill hints\n- use beta workflow").await;

    assert!(a.system.contains("alpha workflow"));
    assert!(b.system.contains("beta workflow"));
    assert_ne!(
        a.prompt_cache_key, b.prompt_cache_key,
        "PrefixHash must change when the cacheable prompt augmentation changes"
    );
}

#[tokio::test]
async fn runner_tells_vision_models_fs_read_can_attach_images() {
    let req = captured_runner_request_with_caps(
        runner_test_caps(true),
        vec![fs_read_descriptor_for_prompt_test()],
    )
    .await;

    assert!(req.system.contains("This model supports image inputs"));
    assert!(req
        .system
        .contains("`fs_read` can return supported PNG/JPEG/GIF/WebP"));
    assert!(req.system.contains("leave `force_text=false`"));

    let fs_read = req
        .tools
        .iter()
        .find(|tool| tool.name == "fs_read")
        .expect("fs_read tool");
    assert!(fs_read.description.contains("this model supports vision"));
    assert!(fs_read
        .description
        .contains("visible on the next model turn"));
}

#[tokio::test]
async fn runner_tells_non_vision_models_not_to_fs_read_images() {
    let req = captured_runner_request_with_caps(
        runner_test_caps(false),
        vec![fs_read_descriptor_for_prompt_test()],
    )
    .await;

    assert!(req
        .system
        .contains("This model does not support image inputs"));
    assert!(req
        .system
        .contains("Do not use `fs_read` on image files for visual inspection or OCR"));

    let fs_read = req
        .tools
        .iter()
        .find(|tool| tool.name == "fs_read")
        .expect("fs_read tool");
    assert!(fs_read
        .description
        .contains("this model does not support vision"));
    assert!(fs_read.description.contains("text-producing alternatives"));
}

#[tokio::test]
async fn runner_summary_recall_is_ephemeral_and_keeps_latest_user_last() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(CapturingModel {
        requests: requests.clone(),
        caps: runner_test_caps(false),
    });
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(NoopTools))
        .store(Arc::new(MemorySessionStore::new()))
        .base_system_prompt("base stable prompt")
        .summary_recall(true)
        .build()
        .expect("runner build");

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    state.history = vec![Message::Observation {
        kind: ObsKind::Summary,
        text: "## Key Facts\n\
               - key=case code | value=PG-NIAH-7429-SUMMIT\n\
               - key=case owner | value=MIRA-SOL-3185\n"
            .into(),
    }];
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("Return the exact RULER case code and case owner."),
            },
        )
        .await
        .unwrap();

    for _ in 0..3 {
        runner.step(&mut state).await.unwrap();
        if !requests.lock().unwrap().is_empty() {
            break;
        }
    }
    let req = requests
        .lock()
        .unwrap()
        .pop()
        .expect("runner should issue one model request");

    assert!(matches!(req.messages.last(), Some(Message::User { .. })));
    let recall = req.messages.iter().find_map(|m| match m {
        Message::Observation {
            kind: ObsKind::Summary,
            text,
        } if text.contains("Relevant prior memory") => Some(text),
        _ => None,
    });
    let recall = recall.expect("request should contain ephemeral summary recall");
    assert!(recall.contains("PG-NIAH-7429-SUMMIT"));
    assert!(recall.contains("MIRA-SOL-3185"));
    assert!(
        !state.history.iter().any(|m| matches!(
            m,
            Message::Observation {
                text,
                ..
            } if text.contains("Relevant prior memory")
        )),
        "summary recall must not be persisted into RunState history"
    );
}

// === Anthropic wire: runtime_context is a 2nd block WITHOUT cache_control ===

#[tokio::test]
async fn anthropic_runtime_context_is_uncached_second_block() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "content":[{"type":"text","text":"ok"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
    });
    let a = AnthropicAdapter::new(net, "https://api.anthropic.com", "claude-haiku-4-5", "k");
    let req = ModelRequest {
        system: "You are μAgent. Be terse.".into(),
        runtime_context: "## Runtime context\ncurrent_date_utc: 2023-11-14\nturn: 3\n".into(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();

    let blocks = body["system"].as_array().expect("system should be array");
    assert_eq!(blocks.len(), 2);
    // First block: L0+L1 cacheable
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(blocks[0]["text"], "You are μAgent. Be terse.");
    // Second block: L2 runtime context, NOT cached
    assert!(
        blocks[1].get("cache_control").is_none(),
        "L2 runtime_context must not carry cache_control; block: {}",
        blocks[1]
    );
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .contains("current_date_utc"));
}

#[tokio::test]
async fn anthropic_without_runtime_context_keeps_single_block() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "content":[{"type":"text","text":"ok"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
    });
    let a = AnthropicAdapter::new(net, "https://api.anthropic.com", "claude-haiku-4-5", "k");
    let req = ModelRequest {
        system: "hi".into(),
        runtime_context: String::new(),
        messages: vec![Message::User {
            content: Content::text("q"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let blocks = body["system"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
}

// === OpenAI wire: runtime_context becomes a SEPARATE system message ===

#[tokio::test]
async fn openai_runtime_context_is_separate_second_system_msg() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "You are μAgent.".into(),
        runtime_context: "## Runtime context\ncurrent_date_utc: 2023-11-14\nturn: 3\n".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();

    let msgs = body["messages"].as_array().unwrap();
    // Two system messages at the front, then user.
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "You are μAgent.");
    assert_eq!(msgs[1]["role"], "system");
    let second: &str = msgs[1]["content"].as_str().unwrap();
    assert!(second.contains("current_date_utc"));
    assert_eq!(msgs[2]["role"], "user");
}

#[tokio::test]
async fn openai_without_runtime_context_has_single_system_msg() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "You are μAgent.".into(),
        runtime_context: String::new(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: Default::default(),
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let system_count = msgs.iter().filter(|m| m["role"] == "system").count();
    assert_eq!(system_count, 1);
}

#[tokio::test]
async fn openrouter_gemini_marks_system_cache_and_moves_runtime_context_to_user() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://openrouter.ai/api/v1",
        "google/gemini-3.1-flash-lite-preview",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "You are μAgent.".into(),
        runtime_context: "## Runtime context\ncurrent_date_utc: 2023-11-14\nturn: 3\n".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let msgs = body["messages"].as_array().unwrap();

    assert_eq!(msgs[0]["role"], "system");
    let system_blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(system_blocks[0]["text"], "You are μAgent.");
    assert_eq!(
        system_blocks[0]["cache_control"]["type"], "ephemeral",
        "Gemini-over-OpenRouter needs an explicit cache breakpoint"
    );

    assert_eq!(msgs[1]["role"], "user");
    assert!(msgs[1]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("current_date_utc"));
    assert!(msgs[1]["content"][0].get("cache_control").is_none());
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"], "hi");
}

#[tokio::test]
async fn openrouter_anthropic_uses_top_level_auto_cache() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://openrouter.ai/api/v1",
        "anthropic/claude-haiku-4.5",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "You are μAgent.".into(),
        runtime_context: "## Runtime context\ncurrent_date_utc: 2023-11-14\n".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "You are μAgent.");
    assert_eq!(msgs[1]["role"], "system");
    assert!(msgs[1]["content"]
        .as_str()
        .unwrap()
        .contains("current_date_utc"));
    assert_eq!(msgs[2]["role"], "user");

    let messages_text = serde_json::to_string(&body["messages"]).unwrap();
    assert!(
        !messages_text.contains("cache_control"),
        "Anthropic-over-OpenRouter should use top-level automatic caching"
    );
}

#[tokio::test]
async fn openrouter_openai_uses_plain_automatic_cache_path() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://openrouter.ai/api/v1",
        "openai/gpt-5.4-nano",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "You are μAgent.".into(),
        runtime_context: "## Runtime context\ncurrent_date_utc: 2023-11-14\nturn: 3\n".into(),
        messages: vec![Message::User {
            content: Content::text("hi"),
        }],
        tools: vec![],
        temperature: None,
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(
        !body_text.contains("cache_control"),
        "OpenRouter OpenAI routes use provider automatic caching; body={body_text}"
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], "You are μAgent.");
    assert_eq!(msgs[1]["role"], "system");
}

#[tokio::test]
async fn openai_summary_observation_is_user_context_not_system_instruction() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "choices": [{"message":{"role":"assistant","content":"ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }),
    });
    let a = OpenAiAdapter::new(
        net,
        "https://api.openai.com/v1",
        "gpt-5.4-nano",
        Some("k".into()),
    );
    let req = ModelRequest {
        system: "base rules".into(),
        runtime_context: String::new(),
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
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "user");
    let summary_content = msgs[1]["content"].as_str().unwrap();
    assert!(summary_content.contains("[conversation summary]"));
    assert!(summary_content.contains("Authoritative prior conversation memory"));
    assert!(summary_content.contains("older task facts"));
    assert_eq!(msgs[2]["role"], "user");
}

#[tokio::test]
async fn anthropic_summary_observation_stays_out_of_system_blocks() {
    let captured = Arc::new(Mutex::new(None));
    let net = Arc::new(CapturingNet {
        captured: captured.clone(),
        canned: json!({
            "content":[{"type":"text","text":"ok"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
    });
    let a = AnthropicAdapter::new(net, "https://api.anthropic.com", "claude-haiku-4-5", "k");
    let req = ModelRequest {
        system: "base rules".into(),
        runtime_context: String::new(),
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
        cache: CachePolicy::Disabled,
        thinking: Default::default(),
        prompt_cache_key: None,
    };
    a.turn(req, CancelToken::never()).await.unwrap();
    let body = captured.lock().unwrap().clone().unwrap();

    let system = serde_json::to_string(&body["system"]).unwrap();
    assert!(!system.contains("older task facts"));
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    let summary_content = msgs[0]["content"].as_str().unwrap();
    assert!(summary_content.contains("[conversation summary]"));
    assert!(summary_content.contains("Authoritative prior conversation memory"));
    assert!(summary_content.contains("older task facts"));
    assert_eq!(msgs[1]["role"], "user");
}
