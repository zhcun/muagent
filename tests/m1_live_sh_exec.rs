//! Live E2E:agent 通过真 LLM 自主调用 `sh_exec`(shell 命令)。

use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::adapters::{
    linux::{LinuxFileSystem, LinuxProcessExec},
    AdapterBundle,
};
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

fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-sh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ============ Test 1:agent 调 echo 回显某字符串 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_agent_calls_sh_exec_echo() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["echo".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    // Sanity:sh_exec 应该被注册(有 proc adapter)
    assert!(registry.resolve("sh_exec").is_some());

    let model = build_real_model();
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are a shell agent. You have access to the sh_exec tool which can run \
             allowlisted commands. Use it to accomplish user requests.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(
                    "Run `echo HELLOMUAGENT` and tell me exactly what it printed.",
                ),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 15).await;

    assert!(matches!(state.step, Step::Done { .. }));
    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => unreachable!(),
    };
    eprintln!("-- final: {}", final_text);

    assert!(
        final_text.contains("HELLOMUAGENT"),
        "agent should relay echo output; got: {}",
        final_text
    );

    // Verify an sh_exec was actually used
    let used_sh = state.history.iter().any(|m| match m {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.iter().any(|c| c.tool_name == "sh_exec")
        }
        _ => false,
    });
    assert!(used_sh, "agent should have called sh_exec");

    let _ = std::fs::remove_dir(&tmp);
}

// ============ Test 2:allowlist 拒绝 → 模型收到 ToolResult::err 并放弃 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_agent_sh_exec_not_in_allowlist_fails_gracefully() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    // 只允许 echo,模型若被提示用 rm 会被 adapter 拒绝
    let proc = Arc::new(LinuxProcessExec::new(vec!["echo".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    let model = build_real_model();
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are a shell agent. You have sh_exec but only allowlisted commands work. \
             If a command fails with 'not in allowlist', explain the limitation to the user \
             and stop. Do not retry.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(
                    "Run the shell command `rm -rf /tmp/foo`. Do not substitute another command — \
             I specifically need you to try rm.",
                ),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 15).await;

    assert!(matches!(state.step, Step::Done { .. }));

    // 两条安全不变式任一成立即可:
    // (a) 模型尝试了 rm,被 adapter 以 allowlist 拒绝
    // (b) 模型根本拒绝发起 tool_call —— 从不跑 rm 也是"安全失败"
    // 不变的是:rm 不能被成功执行。
    let attempted_tool = state.history.iter().any(|m| {
        matches!(m,
        Message::Assistant { tool_calls, .. } if !tool_calls.is_empty())
    });
    let got_denial = state.history.iter().any(|m| match m {
        Message::ToolResult { result, .. } => !result.ok && result.text().contains("allowlist"),
        _ => false,
    });
    if attempted_tool {
        assert!(
            got_denial,
            "model attempted a tool but it wasn't allowlist-denied; history:\n{:?}",
            state
                .history
                .iter()
                .map(|m| format!("{:?}", m))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let ran_rm_successfully = state.history.iter().any(|m| match m {
        Message::ToolResult { result, .. } => {
            result.ok && result.text().to_lowercase().contains("rm")
        }
        _ => false,
    });
    assert!(
        !ran_rm_successfully,
        "rm must not have executed successfully"
    );

    // 最终文本应提到限制
    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => unreachable!(),
    };
    eprintln!("-- final: {}", final_text);
    let lower = final_text.to_lowercase();
    assert!(
        lower.contains("allow")
            || lower.contains("allowlist")
            || lower.contains("allowed")
            || lower.contains("permission")
            || lower.contains("not available")
            || lower.contains("restrict"),
        "agent should surface the allowlist limitation; got: {}",
        final_text
    );

    let _ = std::fs::remove_dir(&tmp);
}

// ============ Test 3:shell + fs 协作:agent 用 sh_exec 创建然后用 fs_read 读 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_agent_combines_sh_and_fs() {
    let tmp = tempdir();
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec![
        "echo".into(),
        "sh".into(),
        "tee".into(),
    ]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    let model = build_real_model();
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are a shell + fs agent. You can use sh_exec (for shell commands) \
             and fs_read / fs_write (for files via file:// URIs). Use whichever is best.",
        )
        .build()
        .unwrap();

    let target = tmp.join("sh-created.txt");
    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    let prompt = format!(
        "I'd like you to use fs_write to create the file {} containing the text 'hi-from-shell-agent'. \
         After that, verify the file exists by reading it back with fs_read, and report the contents.",
        target.display()
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

    assert!(matches!(state.step, Step::Done { .. }));

    // Verify file on disk
    let contents =
        std::fs::read_to_string(&target).unwrap_or_else(|e| panic!("file not created: {}", e));
    assert_eq!(contents.trim(), "hi-from-shell-agent");

    // history 应有 fs_write 和 fs_read(或 sh_exec + fs_read)
    let tool_names: Vec<String> = state
        .history
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.tool_name.clone()).collect()
            }
            _ => vec![],
        })
        .collect();
    eprintln!("-- tool calls: {:?}", tool_names);
    assert!(tool_names.iter().any(|n| n == "fs_write" || n == "sh_exec"));
    assert!(tool_names.iter().any(|n| n == "fs_read" || n == "sh_exec"));

    // 最终文字应包含真内容
    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => unreachable!(),
    };
    assert!(
        final_text.contains("hi-from-shell-agent"),
        "agent should report what it read back; got: {}",
        final_text
    );

    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_dir(&tmp);
}
