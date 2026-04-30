//! Live A/B: muagent's current "rebuild L2 every turn" orchestration vs the
//! Codex-style "bake env once, diff-only afterwards" orchestration.
//!
//! Both run against `openai/gpt-5.4-nano` via OpenRouter and report
//! `cache_read` per turn. The wire body is shaped exactly as each adapter
//! would emit, so the comparison is what the model actually sees, not a
//! mock.
//!
//! Run: `cargo test -p muagent --test m1_live_orchestration_compare -- --ignored --nocapture --test-threads=1`

use std::sync::Arc;
use std::time::Duration;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{ModelAdapter, ModelRequest};
use muagent::core::thinking::ThinkingConfig;
use muagent::core::types::{Content, Message};
use muagent::providers::OpenAiAdapter;

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY missing (set .env or env var)");
    let base = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
    let model = std::env::var("OPENROUTER_CACHE_MODEL")
        .or_else(|_| std::env::var("OPENROUTER_MODEL"))
        .unwrap_or_else(|_| "openai/gpt-5.4-nano".into());
    (key, base, model)
}

/// Stable system block, ~3k tokens — close to a realistic CLI default-system
/// + agent_instructions + skills section.
fn realistic_system() -> String {
    let mut s = String::from(
        "You are an agent running in a user's workspace. Help the user complete \
         practical tasks using local files, commands, and tools.\n\n\
         Tool use protocol:\n\
         - Tool definitions are supplied separately as provider tool schemas.\n\
         - Use tools when an answer depends on local state, file content, or command output.\n\
         - Read-only tools are normal first steps.\n\
         - Treat tool results as authoritative.\n\
         - Tool errors must reach the next turn with enough info to recover.\n\n\
         Skills:\n\
         - Skills are listed separately as names + descriptions.\n\
         - Read SKILL.md only when relevant to the current task.\n\n\
         Stable prompt intent: this block is byte-stable for prompt-cache reuse.\n\
         Below: stable instruction filler so the cacheable prefix exceeds the \
         provider minimum (1024 tokens).\n",
    );
    for i in 0u32..120 {
        s.push_str(&format!(
            "Stable line {i:03}: lorem ipsum dolor sit amet consectetur adipiscing \
             elit sed do eiusmod tempor incididunt ut labore et dolore magna \
             token-{:08x}\n",
            i.wrapping_mul(0x01f1_23bb)
        ));
    }
    s
}

/// One realistic agent-loop turn worth of history: a moderately big
/// tool_result (~600 tokens) + the next user prompt.
fn agent_loop_step(i: usize) -> Vec<Message> {
    let mut user =
        format!("Step {i:03}: please review the deterministic block below and reply OK.\n");
    for j in 0..40u32 {
        user.push_str(&format!(
            "  step{i:03}-detail{j:02}: lorem ipsum dolor sit amet token-{:08x}\n",
            (i as u32 * 1009 + j).wrapping_mul(0x01f1_23bb)
        ));
    }
    let mut asst = String::from("OK — acknowledged. Summary:\n");
    for j in 0..10u32 {
        asst.push_str(&format!(
            "  ack{i:03}-{j:02}: deterministic ack token-{:08x}\n",
            (i as u32 * 7919 + j).wrapping_mul(0x9e37_79b1)
        ));
    }
    vec![
        Message::User {
            content: Content::text(user),
        },
        Message::Assistant {
            content: Content::text(asst),
            tool_calls: vec![],
            thinking: vec![],
        },
    ]
}

/// Codex-style env-context message that goes into history ONCE at session
/// start. Mirrors `EnvironmentContext::body()` from
/// codex-rs/core/src/context/environment_context.rs but kept compact.
fn codex_style_env_message() -> Message {
    let body = format!(
        "<environment_context>\n  \
         <cwd>/Users/test/workspace</cwd>\n  \
         <shell>zsh</shell>\n  \
         <current_date>2026-04-27</current_date>\n  \
         <timezone>UTC</timezone>\n  \
         <os>macos (aarch64)</os>\n  \
         <fs_root>/Users/test/workspace</fs_root>\n\
         </environment_context>"
    );
    Message::User {
        content: Content::text(body),
    }
}

/// Muagent-current style: rebuild a runtime_context block every turn with a
/// per-turn `turn: N` line. Returned as a string the adapter will emit as
/// a SECOND system message (per current OpenAI adapter wiring).
fn muagent_runtime_context(turn: u32) -> String {
    format!("## Runtime context\ncurrent_date_utc: 2026-04-27\nturn: {turn}\n")
}

async fn run_turn(
    adapter: &OpenAiAdapter,
    system: &str,
    runtime_context: &str,
    messages: Vec<Message>,
) -> muagent::core::prelude::TokenUsage {
    let req = ModelRequest {
        system: system.into(),
        runtime_context: runtime_context.into(),
        messages,
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: CachePolicy::Auto,
        thinking: ThinkingConfig::off(),
        prompt_cache_key: None,
    };
    let reply = adapter.turn(req, CancelToken::never()).await.expect("turn");
    reply.usage
}

/// **A: Muagent-current orchestration.** Every turn:
///   - system (stable)
///   - runtime_context = "...turn: N..."  (per-turn-changing, separate
///     system message in wire body)
///   - history: append-only (no env baked in)
#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn a_muagent_current_pattern() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = realistic_system();

    let mut history: Vec<Message> = Vec::new();
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    eprintln!(
        "== A: muagent-current == system_chars={} model={model}",
        system.len()
    );
    for turn in 1u32..=5 {
        // Take a snapshot of history before the new user prompt.
        let mut current = history.clone();
        current.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        let usage = run_turn(&adapter, &system, &muagent_runtime_context(turn), current).await;
        prompts.push(usage.prompt_tokens);
        reads.push(usage.cache_read_tokens);
        eprintln!(
            "  turn{turn:02} prompt={} cache_read={} ratio={:.1}%",
            usage.prompt_tokens,
            usage.cache_read_tokens,
            100.0 * usage.cache_read_tokens as f64 / usage.prompt_tokens.max(1) as f64
        );
        // Append "what really happened" to history as if the runner did it,
        // so future turns see growth. Use deterministic content.
        history.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        history.push(Message::Assistant {
            content: Content::text("OK"),
            tool_calls: vec![],
            thinking: vec![],
        });
        history.extend(agent_loop_step(turn as usize));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("  prompts={prompts:?} reads={reads:?}");
}

/// **B: Codex-style orchestration.** At session start: env_context bundled
/// as a single user message into history. Subsequent turns:
///   - system (stable, same as A)
///   - runtime_context: empty (NO L2 block; nothing to rebuild)
///   - history: env_context + growing turns (steady state has no diffs)
#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn b_codex_style_pattern() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = realistic_system();

    // Env is baked into history ONCE — never rebuilt afterwards.
    let mut history: Vec<Message> = vec![codex_style_env_message()];
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    eprintln!(
        "== B: codex-style == system_chars={} model={model}",
        system.len()
    );
    for turn in 1u32..=5 {
        let mut current = history.clone();
        current.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        // Steady state: no runtime_context emitted. The model client gets
        // pure (system, history) — exactly the Codex shape.
        let usage = run_turn(&adapter, &system, "", current).await;
        prompts.push(usage.prompt_tokens);
        reads.push(usage.cache_read_tokens);
        eprintln!(
            "  turn{turn:02} prompt={} cache_read={} ratio={:.1}%",
            usage.prompt_tokens,
            usage.cache_read_tokens,
            100.0 * usage.cache_read_tokens as f64 / usage.prompt_tokens.max(1) as f64
        );
        history.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        history.push(Message::Assistant {
            content: Content::text("OK"),
            tool_calls: vec![],
            thinking: vec![],
        });
        history.extend(agent_loop_step(turn as usize));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("  prompts={prompts:?} reads={reads:?}");
}
