//! Live E2E: prompt cache actually hits on second turn.
//!
//! Strategy:
//! 1. Build a real AnthropicAdapter pointed at a real Claude model.
//! 2. Craft a big stable system prompt (~3000 tokens of filler to be safely
//!    above the cache eligibility minimum).
//! 3. Turn 1: prompt → MUST create cache (cache_write > 0). Anthropic's
//!    minimum cacheable prefix is ~1024 tokens on Haiku/Sonnet-class models,
//!    hence the 3000-token cushion.
//! 4. Turn 2: same system + one additional user message → MUST read cache
//!    (cache_read > 0), ideally with cache_write ≈ 0.

use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::types::{Content, Message};
use muagent::providers::AnthropicAdapter;

fn load_env() -> Option<(String, String, String)> {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
    let base =
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());
    Some((key, base, model))
}

fn big_system() -> String {
    // ~3000 tokens of stable filler — exceeds Anthropic's 1024-token
    // cache minimum for Haiku/Sonnet-class models.
    let mut s = String::from(
        "You are a research assistant. Always answer in ONE short sentence. \
         The following is stable background you must retain across turns:\n\n",
    );
    for i in 0..400 {
        s.push_str(&format!(
            "Reference fact #{i}: the canonical color for item {i} is #{:06x}.\n",
            (i * 0x1e83f) & 0xFFFFFF,
        ));
    }
    s
}

#[ignore = "hits real Anthropic API; costs a few cents"]
#[tokio::test]
async fn live_anthropic_cache_hit_on_second_turn() {
    let (key, base, model) = match load_env() {
        Some(v) => v,
        None => {
            eprintln!("skip: ANTHROPIC_API_KEY not set");
            return;
        }
    };
    let net = Arc::new(ReqwestEgress::new().unwrap());
    let adapter = AnthropicAdapter::new(net, &base, &model, key);

    let system = big_system();

    // === Turn 1 ===
    let req1 = ModelRequest {
        system: system.clone(),
        messages: vec![Message::User {
            content: Content::text("Say 'first' in one word."),
        }],
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let r1 = adapter
        .turn(req1, CancelToken::never())
        .await
        .expect("turn 1");
    eprintln!(
        "turn1: prompt={} cache_write={} cache_read={} completion={}",
        r1.usage.prompt_tokens,
        r1.usage.cache_write_tokens,
        r1.usage.cache_read_tokens,
        r1.usage.completion_tokens
    );
    assert!(
        r1.usage.cache_write_tokens > 0,
        "turn 1 should WRITE cache (got write={}, read={})",
        r1.usage.cache_write_tokens,
        r1.usage.cache_read_tokens
    );

    // === Turn 2: same stable prefix, one new user message ===
    let req2 = ModelRequest {
        system,
        messages: vec![
            Message::User {
                content: Content::text("Say 'first' in one word."),
            },
            Message::Assistant {
                content: Content::text(r1.text.clone()),
                tool_calls: vec![],
                thinking: vec![],
            },
            Message::User {
                content: Content::text("Now say 'second' in one word."),
            },
        ],
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: CachePolicy::Auto,
        thinking: Default::default(),
        prompt_cache_key: None,
        runtime_context: String::new(),
    };
    let r2 = adapter
        .turn(req2, CancelToken::never())
        .await
        .expect("turn 2");
    eprintln!(
        "turn2: prompt={} cache_write={} cache_read={} completion={}",
        r2.usage.prompt_tokens,
        r2.usage.cache_write_tokens,
        r2.usage.cache_read_tokens,
        r2.usage.completion_tokens
    );

    assert!(
        r2.usage.cache_read_tokens > 0,
        "turn 2 should READ from cache (got read={}, write={})",
        r2.usage.cache_read_tokens,
        r2.usage.cache_write_tokens
    );

    // Cache read should be substantial — most of turn 1's write.
    assert!(
        r2.usage.cache_read_tokens >= r1.usage.cache_write_tokens / 2,
        "cache hit too small: turn1 wrote {}, turn2 read {}",
        r1.usage.cache_write_tokens,
        r2.usage.cache_read_tokens
    );
}
