//! Live E2E: the agent actually discovers a filesystem skill, reads its
//! SKILL.md, runs the script it references, and uses the output.
//!
//! Scaffold:
//!   tmp/
//!   └── skills/
//!       └── motd-greeter/
//!           ├── SKILL.md          — says "run scripts/get_motd.sh first"
//!           └── scripts/
//!               └── get_motd.sh   — echoes a unique random-looking token
//!
//! We verify 3 things from `state.history`:
//!   (1) The agent read SKILL.md (fs_read call against that path).
//!   (2) The agent ran scripts/get_motd.sh via sh_exec.
//!   (3) The agent's final text contains the exact token printed by the script.
//!
//! This is an end-to-end proof that the whole Anthropic-style skill loop
//! works with a real LLM: progressive disclosure (prompt → SKILL.md →
//! scripts) with no framework magic, just the fs + sh tools the agent
//! already has.

use std::path::PathBuf;
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

// --- A unique-looking token the agent can only get by running the script. ---
const MOTD_TOKEN: &str = "MOTD-Q7F3R-XK2A";

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    // Nano is fine once the adapter path-resolution bug is fixed. Override
    // with MUAGENT_SKILL_E2E_MODEL if you want a different model.
    (
        std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY missing"),
        std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
        std::env::var("MUAGENT_SKILL_E2E_MODEL").unwrap_or_else(|_| "openai/gpt-5.4-nano".into()),
    )
}

fn build_real_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

fn write_motd_skill(root: &std::path::Path) -> PathBuf {
    let skills_dir = root.join("skills");
    let skill_dir = skills_dir.join("motd-greeter");
    let reference_dir = skill_dir.join("reference");
    std::fs::create_dir_all(&reference_dir).unwrap();

    // The session-specific token lives in a plain reference file.
    let token_file = reference_dir.join("session_token.txt");
    std::fs::write(&token_file, format!("{MOTD_TOKEN}\n")).unwrap();
    let token_file_canon = token_file.canonicalize().unwrap_or(token_file.clone());
    let token_file_uri = format!("file://{}", token_file_canon.display());

    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            r#"---
name: motd-greeter
description: Greet the user with the session MOTD token; full instructions inside.
---

# MOTD Greeter — REQUIRED procedure for greetings

The session's greeting token is stored in this file:

    {token_file_uri}

## Step 1 — fetch the token

Call `fs_read` with:

    {{ "uri": "{token_file_uri}" }}

The file's entire content is the token, followed by a newline. Strip
the trailing newline.

## Step 2 — reply to the user

Your reply MUST contain the token verbatim on the FIRST line, then a
short friendly greeting on the next line. Example shape:

    <TOKEN>
    Nice to see you!

## Rules

- Do not guess the token. It is generated per session and can ONLY be
  obtained by fs_reading the file above.
- Do not skip Step 1.
- The path is absolute and already canonical; use it exactly as given.
"#
        ),
    )
    .unwrap();

    skills_dir
}

async fn drive(runner: &Runner, state: &mut RunState, max: usize) {
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

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_agent_follows_filesystem_skill() {
    let tmp_raw = std::env::temp_dir().join(format!("muagent-skill-e2e-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_raw).unwrap();
    // Canonicalize so /var/folders → /private/var/folders on macOS. Otherwise
    // the FS root's starts_with check fails on the agent's canonicalized URIs.
    let tmp = tmp_raw.canonicalize().unwrap_or(tmp_raw);
    let skills_root = write_motd_skill(&tmp);
    // Sanity print so we can verify what the agent will see in SKILL.md.
    let skill_md_path = skills_root.join("motd-greeter/SKILL.md");
    eprintln!(
        "-- SKILL.md contents:\n{}",
        std::fs::read_to_string(&skill_md_path).unwrap()
    );

    // FS adapter rooted at tmp (so agent can read SKILL.md + the script path).
    // sh_exec allows `sh` so the agent can do `sh -c 'sh /.../get_motd.sh'`.
    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let proc = Arc::new(LinuxProcessExec::new(vec!["sh".into()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).proc(proc).build().unwrap());

    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    // Load the skill via the filesystem loader pointed at our temp skills root.
    let skills = Arc::new(SkillManager::new());
    let loaded = muagent::capabilities::skills::loader::FilesystemSkillLoader::new()
        .with_root(&skills_root)
        .load_into(&skills)
        .unwrap();
    assert_eq!(loaded, vec!["motd-greeter".to_string()]);

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry).with_skills(skills.clone());
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let model = build_real_model();

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(
            "You are a task-executing agent. You have these tools: fs_read, \
             fs_list, fs_write, fs_stat, fs_delete, fs_rename, sh_exec.\n\n\
             SKILL PROTOCOL (mandatory):\n\
             1. If any skill listed under `## Skills` matches the user's \
                request, fs_read that skill's SKILL.md FIRST.\n\
             2. SKILL.md contains the canonical procedure. When it shows \
                a tool call specification like \"Tool: sh_exec, Arguments: \
                {...}\", EMIT that tool call exactly as specified. Do not \
                paraphrase, do not re-interpret, do not ask for \
                confirmation — execute it.\n\
             3. After the tool returns, follow SKILL.md's remaining steps \
                to shape your final reply.\n\
             4. You have agency. When SKILL.md says \"run this\", run it. \
                When it says \"do not do X\", don't do X.\n\n\
             Failure mode to avoid: reading SKILL.md and then responding \
             to the user without executing the tool calls it specified. \
             That is a protocol violation.",
        )
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(
                    "Please greet me using the motd-greeter skill. Follow its \
             SKILL.md instructions exactly — including running any \
             scripts it specifies.",
                ),
            },
        )
        .await
        .unwrap();
    drive(&runner, &mut state, 20).await;

    // === Assertions =========================================================

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done, got {other:?}; history: {:?}", state.history),
    };

    // Gather all tool calls the agent made, by tool_name + serialized args.
    let tool_calls: Vec<(String, String)> = state
        .history
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .map(|c| (c.tool_name.clone(), c.args.to_string()))
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect();
    eprintln!("-- tool calls:");
    for (n, a) in &tool_calls {
        eprintln!("   {n}({a})");
    }
    eprintln!("-- final: {final_text}");

    eprintln!("\n-- FULL HISTORY --");
    for (i, m) in state.history.iter().enumerate() {
        match m {
            Message::User { content } => eprintln!("[{i}] user: {:?}", content),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                eprintln!(
                    "[{i}] assistant: content={:?} calls={:?}",
                    content,
                    tool_calls
                        .iter()
                        .map(|c| format!("{}({})", c.tool_name, c.args))
                        .collect::<Vec<_>>()
                );
            }
            Message::ToolResult { call_id, result } => {
                let brief: String = result.text().chars().take(400).collect();
                eprintln!(
                    "[{i}] tool_result call={call_id} ok={} content={:?}",
                    result.ok, brief
                );
            }
            Message::System { content } => eprintln!("[{i}] system: {:?}", content),
            Message::Observation { text, .. } => eprintln!("[{i}] obs: {text}"),
        }
    }

    // (1) Agent must read SKILL.md.
    let read_skill_md = tool_calls
        .iter()
        .any(|(name, args)| name == "fs_read" && args.contains("SKILL.md"));
    assert!(
        read_skill_md,
        "agent should fs_read the skill's SKILL.md; tool calls: {tool_calls:?}"
    );

    // (2) Agent must have fs_read the reference file SKILL.md pointed at.
    let read_reference = tool_calls
        .iter()
        .any(|(name, args)| name == "fs_read" && args.contains("session_token.txt"));
    assert!(
        read_reference,
        "agent should fs_read reference/session_token.txt per SKILL.md; tool calls: {tool_calls:?}"
    );

    // (3) Final response must contain the exact token.
    assert!(
        final_text.contains(MOTD_TOKEN),
        "agent's final reply should contain the MOTD token {MOTD_TOKEN}; got: {final_text}"
    );

    // Cleanup.
    let _ = std::fs::remove_dir_all(&tmp);
}
