//! Live quantification: with all current fixes applied, what fraction of
//! the prompt is still uncached, and where does it come from?
//!
//! Three back-to-back scenarios on `openai/gpt-5.4-nano` via OpenRouter.
//! Each runs 5 turns and the test prints `cache_read / prompt` for every
//! turn so we can read off exactly which structural choice costs which
//! tokens.
//!
//! - **A. muagent-current**: system stable, L2 = "current_date_utc: ..." (byte-
//!   stable within day, but separate uncached system message), tools sorted,
//!   `prompt_cache_key` = a fixed session-equivalent string.
//! - **B. no-L2**: same as A but `runtime_context` is empty. Tests whether
//!   removing L2 entirely improves cache_read materially. If A and B are
//!   indistinguishable, L2 is free in practice and removing it gains nothing.
//! - **C. cache-key=prefix-hash**: same as A but `prompt_cache_key` is a
//!   stable hash of (system + tools), shared across all probes. Tests the
//!   "cross-session affinity" angle — runs back-to-back here so it should
//!   primarily reveal whether changing the key alters routing under
//!   identical prefix.
//!
//! Run: `cargo test -p muagent --test m1_live_orchestration_quantify -- --ignored --nocapture --test-threads=1`

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

fn realistic_system() -> String {
    let mut s = String::from(
        "You are an agent running in a user's workspace. Help the user complete \
         tasks using local files, commands, and tools.\n\n\
         Tool use protocol:\n\
         - Tool definitions are supplied separately as provider tool schemas.\n\
         - Use tools when an answer depends on local state.\n\
         - Treat tool results as authoritative.\n\n\
         Below: stable instruction filler so cacheable prefix exceeds 1024.\n",
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

fn agent_loop_step(i: usize) -> Vec<Message> {
    let mut user = format!("Step {i:03}: please review the following block.\n");
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

async fn run_turn(
    adapter: &OpenAiAdapter,
    system: &str,
    runtime_context: &str,
    cache_key: Option<String>,
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
        prompt_cache_key: cache_key,
    };
    adapter
        .turn(req, CancelToken::never())
        .await
        .expect("turn")
        .usage
}

fn fresh_history() -> Vec<Message> {
    Vec::new()
}

fn append_turn(history: &mut Vec<Message>, turn: u32) {
    history.push(Message::User {
        content: Content::text(format!("Turn {turn} probe — reply OK.")),
    });
    history.push(Message::Assistant {
        content: Content::text("OK"),
        tool_calls: vec![],
        thinking: vec![],
    });
    history.extend(agent_loop_step(turn as usize));
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn quantify_remaining_uncached_tokens() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = realistic_system();
    let l2 = "## Runtime context\ncurrent_date_utc: 2026-04-27\n";
    let session_a = "quantify-a-fixed-cache-key".to_string();
    let session_b = "quantify-b-fixed-cache-key".to_string();
    let prefix_hash_key = "quantify-shared-prefix-hash-2026".to_string();

    eprintln!("== model={model} ==");

    // ---- A: current muagent (L2 byte-stable, prompt_cache_key = "session" ish) ----
    let mut history = fresh_history();
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    eprintln!("\n[A] system_stable + L2_stable + key=session_a");
    for turn in 1u32..=5 {
        let mut current = history.clone();
        current.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        let usage = run_turn(&adapter, &system, l2, Some(session_a.clone()), current).await;
        prompts.push(usage.prompt_tokens);
        reads.push(usage.cache_read_tokens);
        let uncached = usage.prompt_tokens.saturating_sub(usage.cache_read_tokens);
        eprintln!(
            "  turn{turn:02} prompt={} cache_read={} uncached={} ratio={:.1}%",
            usage.prompt_tokens,
            usage.cache_read_tokens,
            uncached,
            100.0 * usage.cache_read_tokens as f64 / usage.prompt_tokens.max(1) as f64
        );
        append_turn(&mut history, turn);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let a_prompts = prompts;
    let a_reads = reads;

    // ---- B: same as A but L2 = "" (no second system message at all) ----
    let mut history = fresh_history();
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    eprintln!("\n[B] system_stable + L2_empty + key=session_b");
    for turn in 1u32..=5 {
        let mut current = history.clone();
        current.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        let usage = run_turn(&adapter, &system, "", Some(session_b.clone()), current).await;
        prompts.push(usage.prompt_tokens);
        reads.push(usage.cache_read_tokens);
        let uncached = usage.prompt_tokens.saturating_sub(usage.cache_read_tokens);
        eprintln!(
            "  turn{turn:02} prompt={} cache_read={} uncached={} ratio={:.1}%",
            usage.prompt_tokens,
            usage.cache_read_tokens,
            uncached,
            100.0 * usage.cache_read_tokens as f64 / usage.prompt_tokens.max(1) as f64
        );
        append_turn(&mut history, turn);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let b_prompts = prompts;
    let b_reads = reads;

    // ---- C: same prefix as A, but key = stable hash (simulates cross-session affinity) ----
    let mut history = fresh_history();
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    eprintln!("\n[C] system_stable + L2_stable + key=prefix_hash");
    for turn in 1u32..=5 {
        let mut current = history.clone();
        current.push(Message::User {
            content: Content::text(format!("Turn {turn} probe — reply OK.")),
        });
        let usage = run_turn(
            &adapter,
            &system,
            l2,
            Some(prefix_hash_key.clone()),
            current,
        )
        .await;
        prompts.push(usage.prompt_tokens);
        reads.push(usage.cache_read_tokens);
        let uncached = usage.prompt_tokens.saturating_sub(usage.cache_read_tokens);
        eprintln!(
            "  turn{turn:02} prompt={} cache_read={} uncached={} ratio={:.1}%",
            usage.prompt_tokens,
            usage.cache_read_tokens,
            uncached,
            100.0 * usage.cache_read_tokens as f64 / usage.prompt_tokens.max(1) as f64
        );
        append_turn(&mut history, turn);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let c_prompts = prompts;
    let c_reads = reads;

    eprintln!("\n=== summary ===");
    eprintln!("           prompts                cache_reads");
    eprintln!("A current : {a_prompts:?}  {a_reads:?}");
    eprintln!("B no-L2   : {b_prompts:?}  {b_reads:?}");
    eprintln!("C key=hash: {c_prompts:?}  {c_reads:?}");
    let a5 = (a_prompts[4], a_reads[4]);
    let b5 = (b_prompts[4], b_reads[4]);
    let c5 = (c_prompts[4], c_reads[4]);
    eprintln!(
        "\nturn5 uncached: A={} B={} C={} (smaller is better)",
        a5.0 - a5.1,
        b5.0 - b5.1,
        c5.0 - c5.1
    );
    eprintln!(
        "turn5 ratio:    A={:.1}% B={:.1}% C={:.1}%",
        100.0 * a5.1 as f64 / a5.0 as f64,
        100.0 * b5.1 as f64 / b5.0 as f64,
        100.0 * c5.1 as f64 / c5.0 as f64,
    );
}
