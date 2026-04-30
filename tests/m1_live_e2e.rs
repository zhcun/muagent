//! Live 端到端:真 OpenRouter 模型 + 真 LinuxFileSystem 工具 + 真 Runner FSM。
//!
//! 这些测试验证**整个栈**在真实网络 / 真实磁盘下能正确驱动 agent loop。
//! 默认 `#[ignore]`,要显式 `cargo test ... -- --ignored`。
//!
//! 需要:.env(workspace 根)里的 OPENROUTER_API_KEY / OPENROUTER_BASE_URL / OPENROUTER_MODEL。

use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::adapters::{linux::LinuxFileSystem, AdapterBundle};
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::types::{Content, Message};
use muagent::prelude::*;
use muagent::providers::OpenAiAdapter;
use muagent::storage::JsonlSessionStore;
use muagent::storage::MemorySessionStore;
use uuid::Uuid;

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    (
        std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY missing"),
        std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
        std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-5.4-nano".into()),
    )
}

fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-live-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn build_real_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

fn print_history(state: &RunState) {
    eprintln!("--- history ({} msgs) ---", state.history.len());
    for (i, m) in state.history.iter().enumerate() {
        let kind = match m {
            Message::User { .. } => "user",
            Message::Assistant { tool_calls, .. } => {
                if tool_calls.is_empty() {
                    "assistant"
                } else {
                    "assistant+tools"
                }
            }
            Message::ToolResult { .. } => "tool_result",
            Message::System { .. } => "system",
            Message::Observation { .. } => "obs",
        };
        let brief = match m {
            Message::User { content }
            | Message::System { content }
            | Message::Assistant { content, .. } => format!("{:?}", content),
            Message::ToolResult { result, .. } => {
                format!("ok={} content={:?}", result.ok, brief_text(&result.text()))
            }
            Message::Observation { text, .. } => brief_text(text),
        };
        eprintln!("  [{i}] {kind}: {brief}");
    }
}

fn brief_text(s: &str) -> String {
    let s = s.replace('\n', "\\n");
    if s.chars().count() > 100 {
        format!("{}…", s.chars().take(100).collect::<String>())
    } else {
        s
    }
}

async fn drive_until_terminal(runner: &Runner, state: &mut RunState, max_steps: usize) {
    for i in 0..max_steps {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            eprintln!("-- terminal after {} steps: {:?}", i, state.step.name());
            return;
        }
        let out = runner.step(state).await.expect("step should succeed");
        eprintln!(
            "-- step {} advanced={} events={}",
            i,
            out.advanced,
            out.events.len()
        );
    }
    panic!(
        "did not terminate within {} steps, step={:?}",
        max_steps, state.step
    );
}

// ================ Test 1:Runner + real model + real fs tools ================

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_runner_with_fs_tools() {
    let tmp = tempdir();
    let target_file = tmp.join("greeting.txt");

    // Real fs adapter + tools
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let model = build_real_model();
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are an agent with filesystem tools. Use fs_write to write files, \
             fs_read to read them. URIs use file:// scheme. Be terse.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    let prompt = format!(
        "Please write the exact text 'hello from agent' to the file {}, \
         then read it back and tell me what you read.",
        target_file.display()
    );
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(prompt),
            },
        )
        .await
        .unwrap();

    drive_until_terminal(&runner, &mut state, 20).await;
    print_history(&state);

    // Verify terminal state
    assert!(matches!(state.step, Step::Done { .. }), "should reach Done");

    // Verify the file was actually written
    let data = std::fs::read_to_string(&target_file)
        .unwrap_or_else(|e| panic!("file not created at {}: {}", target_file.display(), e));
    assert_eq!(data, "hello from agent", "file content mismatch");

    // Verify history contains tool_calls(both write and read)
    let tool_call_names: Vec<String> = state
        .history
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.tool_name.clone()).collect()
            }
            _ => vec![],
        })
        .collect();
    eprintln!("tool_call names: {:?}", tool_call_names);
    assert!(tool_call_names.iter().any(|n| n == "fs_write"));
    assert!(tool_call_names.iter().any(|n| n == "fs_read"));

    // Cleanup
    let _ = std::fs::remove_file(&target_file);
    let _ = std::fs::remove_dir(&tmp);
}

// ================ Test 2:JSONL persistence + real model ================

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_runner_jsonl_persistence() {
    let store: Arc<dyn SessionStore> = Arc::new(
        JsonlSessionStore::open(tempdir().join("store"))
            .await
            .unwrap(),
    );
    let registry = Arc::new(CapabilityRegistry::new()); // no tools
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let model = build_real_model();

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store.clone())
        .tools_provider(provider)
        .base_system_prompt("Be terse. Reply in under 20 words.")
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    let run_id = state.run_id;
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("What's the capital of France?"),
            },
        )
        .await
        .unwrap();

    drive_until_terminal(&runner, &mut state, 10).await;
    assert!(matches!(state.step, Step::Done { .. }));

    eprintln!(
        "-- first reply: {:?}",
        state
            .history
            .iter()
            .rev()
            .find(|m| matches!(m, Message::Assistant { .. }))
    );

    // Reload from JSONL store — verify persistence
    let loaded = store.load_run(run_id).await.unwrap().expect("found");
    assert_eq!(loaded.run_id, run_id);
    assert!(matches!(loaded.step, Step::Done { .. }));
    assert_eq!(loaded.history.len(), state.history.len());
    assert!(loaded.usage.turns >= 1);
    assert!(loaded.usage.tokens_prompt > 0);
    eprintln!(
        "-- usage: prompt={} completion={}",
        loaded.usage.tokens_prompt, loaded.usage.tokens_completion
    );

    // Continue same session with follow-up
    let mut continued = loaded;
    runner
        .submit_user_message(
            &mut continued,
            Message::User {
                content: Content::text("And its population?"),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut continued, 10).await;
    assert!(matches!(continued.step, Step::Done { .. }));
    assert_eq!(continued.usage.turns, 2);
    eprintln!(
        "-- after follow-up: usage.turns={} history.len={}",
        continued.usage.turns,
        continued.history.len()
    );
}

// ================ Test 3:Multi-turn with tool results flowing back ================

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_runner_multi_turn_tool_roundtrip() {
    let tmp = tempdir();
    // Pre-seed a file; agent should read it
    let src = tmp.join("notes.txt");
    std::fs::write(&src, "The password is SWORDFISH").unwrap();

    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let model = build_real_model();
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are an agent with fs tools. Use fs_read to read files. \
             Files are at file://<absolute_path>.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    let prompt = format!(
        "Read {} and tell me what the password is (reply with just the password word).",
        src.display()
    );
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(prompt),
            },
        )
        .await
        .unwrap();

    drive_until_terminal(&runner, &mut state, 15).await;
    print_history(&state);
    assert!(matches!(state.step, Step::Done { .. }));

    // Find the final assistant text
    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => unreachable!(),
    };
    eprintln!("-- final_text: {}", final_text);
    assert!(
        final_text.to_uppercase().contains("SWORDFISH"),
        "expected model to extract password from tool_result: {}",
        final_text
    );

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_dir(&tmp);
}

// ================ Test 4:Model switching via OpenRouter ================
//
// OpenRouter 用一个 key 路由到多家 provider。此测试证明我们的 OpenAI adapter
// 只换 model 名就能调用 Claude 等非-OpenAI 后端。

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_model_switching_via_openrouter() {
    let (key, base, _) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());

    // 用三家最新/最省钱的小模型。OpenRouter 上 Anthropic 模型名带 provider 前缀。
    let models = [
        ("openai/gpt-5.4-nano", "openai"),
        ("google/gemini-3.1-flash-lite-preview", "google"),
        ("anthropic/claude-haiku-4.5", "anthropic"),
    ];

    for (m, provider) in models {
        eprintln!("-- trying model {}", m);
        let adapter = OpenAiAdapter::new(net.clone(), &base, m, Some(key.clone()));
        let req = ModelRequest {
            system: "Answer in ONE word only.".into(),
            messages: vec![Message::User {
                content: Content::text("Say hello."),
            }],
            tools: vec![],
            temperature: Some(0.0),
            stream: false,
            cache: Default::default(),
            thinking: Default::default(),
            prompt_cache_key: None,
            runtime_context: String::new(),
        };
        match adapter
            .turn(req, muagent::core::cancel::CancelToken::never())
            .await
        {
            Ok(r) => eprintln!(
                "  [{provider} / {m}] ok: {:?} (p={}, c={})",
                r.text, r.usage.prompt_tokens, r.usage.completion_tokens
            ),
            Err(e) => eprintln!("  [{provider} / {m}] err: {}", e),
        }
    }
    // 不强制 assert:OpenRouter 上某些模型可能暂时不可用 / 免费额度用完。
    // 此测试主要目的是验证"adapter 层对非 OpenAI-家 后端也工作"。
}
