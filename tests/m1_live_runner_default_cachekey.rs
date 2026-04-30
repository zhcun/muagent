//! Live: measure first-turn cache_read under session-id and prefix-hash keys.
//!
//! OpenAI/OpenRouter cache behavior can vary by backend routing and recent
//! cache state. This test keeps prompt bytes identical and prints cache_read
//! for a fresh session-style key and a stable prefix-hash-style key so changes
//! in the runner's stable prefix are visible without assuming one provider
//! routing behavior.
//!
//! Run: `cargo test -p muagent --test m1_live_runner_default_cachekey -- --ignored --nocapture --test-threads=1`

use std::sync::Arc;
use std::time::Duration;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{ModelAdapter, ModelRequest};
use muagent::core::thinking::ThinkingConfig;
use muagent::core::types::{Content, Message};
use muagent::providers::OpenAiAdapter;
use uuid::Uuid;

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
        "You are an agent.\nFollow tool-use protocol.\n\nFiller below for cacheable prefix:\n",
    );
    for i in 0u32..120 {
        s.push_str(&format!(
            "Stable line {i:03}: lorem ipsum dolor sit amet token-{:08x}\n",
            i.wrapping_mul(0x01f1_23bb)
        ));
    }
    s
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn cold_session_first_turn_session_id_vs_prefix_hash() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = realistic_system();

    eprintln!("== model={model} ==");

    // Build a single ModelRequest helper used for both cases.
    let make_req = |cache_key: Option<String>| ModelRequest {
        system: system.clone(),
        runtime_context: String::new(),
        messages: vec![Message::User {
            content: Content::text("Reply OK."),
        }],
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: CachePolicy::Auto,
        thinking: ThinkingConfig::off(),
        prompt_cache_key: cache_key,
    };

    // Round 1: warm the cache under a stable PrefixHash-style key.
    let stable_key = "muagent-test-prefix-hash-stable".to_string();
    eprintln!("\n[warm] sending one request under stable prefix-hash key…");
    let warmup = adapter
        .turn(make_req(Some(stable_key.clone())), CancelToken::never())
        .await
        .expect("warmup");
    eprintln!(
        "  warmup: prompt={} cache_read={} (writes a cache entry under stable_key)",
        warmup.usage.prompt_tokens, warmup.usage.cache_read_tokens,
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Round 2a: simulate a NEW session under Session-strategy = a fresh UUID.
    // Some providers still report a prefix cache hit if identical bytes were
    // recently cached elsewhere, so this is measured rather than asserted.
    let session_uuid = format!("session-{}", Uuid::new_v4());
    eprintln!("\n[A] new-session under Session-strategy key={session_uuid}");
    let a = adapter
        .turn(make_req(Some(session_uuid)), CancelToken::never())
        .await
        .expect("session strategy");
    eprintln!(
        "  turn1: prompt={} cache_read={} ratio={:.1}%",
        a.usage.prompt_tokens,
        a.usage.cache_read_tokens,
        100.0 * a.usage.cache_read_tokens as f64 / a.usage.prompt_tokens.max(1) as f64
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Round 2b: simulate a NEW session under PrefixHash-strategy (same stable
    // key as the warmup). The prefix bytes are identical to A; only the key
    // differs. Providers that honor routing affinity should prefer the warmed
    // backend; providers with broader prefix caches may make A and B similar.
    eprintln!("\n[B] new-session under PrefixHash-strategy key={stable_key}");
    let b = adapter
        .turn(make_req(Some(stable_key)), CancelToken::never())
        .await
        .expect("prefix hash strategy");
    eprintln!(
        "  turn1: prompt={} cache_read={} ratio={:.1}%",
        b.usage.prompt_tokens,
        b.usage.cache_read_tokens,
        100.0 * b.usage.cache_read_tokens as f64 / b.usage.prompt_tokens.max(1) as f64
    );

    eprintln!(
        "\n=== finding ===\n\
         Session-strategy first turn cache_read: {}\n\
         PrefixHash-strategy first turn cache_read: {}\n\
         Both sent byte-identical prompts. The only difference is the key\n\
         used to route; compare these numbers to see whether this provider\n\
         currently exposes any routing-affinity difference.",
        a.usage.cache_read_tokens, b.usage.cache_read_tokens,
    );
}
