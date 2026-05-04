//! Live probe: OpenRouter prompt-cache reporting through the OpenAI adapter.
//!
//! This is intentionally ignored. Run explicitly with:
//! ```bash
//! cargo test -p muagent --test m1_live_openrouter_cache -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use muagent::adapters::linux::LinuxFileSystem;
use muagent::adapters::AdapterBundle;
use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::{ModelAdapter, ModelRequest, ToolDescriptor};
use muagent::core::thinking::ThinkingConfig;
use muagent::core::types::{Content, Message};
use muagent::prelude::{register_defaults, CapabilityRegistry};
use muagent::providers::OpenAiAdapter;

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            eprintln!("loaded {p}");
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

fn big_stable_system() -> String {
    let mut s = String::from(
        "You are a cache probe. Answer with exactly the requested one-word token.\n\
         The following stable prefix is deliberately long and repeated verbatim:\n",
    );
    for i in 0u32..700 {
        s.push_str(&format!(
            "Stable cache line {i:04}: token-group alpha beta gamma delta value {:08x}.\n",
            i.wrapping_mul(0x01f1_23bb)
        ));
    }
    s
}

fn cli_default_system_from_source() -> String {
    let mut system = String::from(include_str!("../src/prompts/default-system.md"));
    system.push_str("\nRuntime environment:\n");
    system.push_str(&format!("- Current date: {}\n", current_utc_date()));
    system.push_str(&format!(
        "- Operating system: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    system.push_str(&format!(
        "- Filesystem root: {}\n",
        std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .display()
    ));
    system.push_str("- Shell execution: disabled.\n");
    system
}

fn current_utc_date() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += if m <= 2 { 1 } else { 0 };
    (y as i32, m as u32, d as u32)
}

fn default_builtin_tools() -> Vec<ToolDescriptor> {
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let fs = Arc::new(LinuxFileSystem::new(vec![root]));
    let bundle = Arc::new(
        AdapterBundle::builder()
            .fs(fs)
            .build()
            .expect("adapter bundle"),
    );
    let registry = Arc::new(CapabilityRegistry::new());
    register_defaults(&registry, bundle);
    registry
        .list()
        .into_iter()
        .map(|tool| tool.descriptor().clone())
        .collect()
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn live_openrouter_repeated_prefix_reports_cache_tokens() {
    let (key, base, model) = load_env();
    eprintln!("provider=openrouter model={model}");

    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = big_stable_system();

    let mut reads = Vec::new();
    for idx in 1..=3 {
        let req = ModelRequest {
            system: system.clone(),
            runtime_context: String::new(),
            messages: vec![Message::User {
                content: Content::text("Reply with exactly: OK"),
            }],
            tools: vec![],
            temperature: Some(0.0),
            stream: false,
            cache: CachePolicy::Auto,
            thinking: ThinkingConfig::off(),
            prompt_cache_key: None,
        };
        let reply = adapter
            .turn(req, CancelToken::never())
            .await
            .unwrap_or_else(|e| panic!("turn {idx}: {e}"));
        eprintln!(
            "turn{idx}: text={:?} prompt={} completion={} cache_read={} cache_write={} thinking={}",
            reply.text,
            reply.usage.prompt_tokens,
            reply.usage.completion_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
            reply.usage.thinking_tokens
        );
        assert!(
            !reply.text.trim().is_empty(),
            "turn {idx} returned empty text"
        );
        reads.push(reply.usage.cache_read_tokens);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let hit = reads.iter().skip(1).any(|n| *n > 0);
    eprintln!(
        "cache_hit_reported={hit} later_cache_reads={:?}",
        &reads[1..]
    );
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn live_openrouter_cli_default_prompt_and_tools_cache_probe() {
    let (key, base, model) = load_env();
    eprintln!("provider=openrouter model={model}");

    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = cli_default_system_from_source();
    let tools = default_builtin_tools();
    eprintln!(
        "cli_default_probe: system_chars={} tool_count={}",
        system.len(),
        tools.len()
    );

    let mut reads = Vec::new();
    for idx in 1..=3 {
        let req = ModelRequest {
            system: system.clone(),
            runtime_context: String::new(),
            messages: vec![Message::User {
                content: Content::text("Reply with exactly: OK"),
            }],
            tools: tools.clone(),
            temperature: Some(0.0),
            stream: false,
            cache: CachePolicy::Auto,
            thinking: ThinkingConfig::off(),
            prompt_cache_key: None,
        };
        let reply = adapter
            .turn(req, CancelToken::never())
            .await
            .unwrap_or_else(|e| panic!("turn {idx}: {e}"));
        eprintln!(
            "turn{idx}: text={:?} prompt={} completion={} cache_read={} cache_write={} thinking={}",
            reply.text,
            reply.usage.prompt_tokens,
            reply.usage.completion_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
            reply.usage.thinking_tokens
        );
        assert!(
            !reply.text.trim().is_empty() || !reply.tool_calls.is_empty(),
            "turn {idx} returned no text and no tool calls"
        );
        reads.push(reply.usage.cache_read_tokens);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let hit = reads.iter().skip(1).any(|n| *n > 0);
    eprintln!(
        "cli_default_cache_hit_reported={hit} later_cache_reads={:?}",
        &reads[1..]
    );
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn live_openrouter_cli_default_prompt_and_tools_with_cache_key_probe() {
    let (key, base, model) = load_env();
    eprintln!("provider=openrouter model={model}");

    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = cli_default_system_from_source();
    let tools = default_builtin_tools();
    let cache_key = "muagent-live-cli-default-tools-stable-v2".to_string();
    eprintln!(
        "cli_default_with_key_probe: system_chars={} tool_count={} key={}",
        system.len(),
        tools.len(),
        cache_key
    );

    let mut reads = Vec::new();
    for idx in 1..=3 {
        let req = ModelRequest {
            system: system.clone(),
            runtime_context: String::new(),
            messages: vec![Message::User {
                content: Content::text("Reply with exactly: OK"),
            }],
            tools: tools.clone(),
            temperature: Some(0.0),
            stream: false,
            cache: CachePolicy::Auto,
            thinking: ThinkingConfig::off(),
            prompt_cache_key: Some(cache_key.clone()),
        };
        let reply = adapter
            .turn(req, CancelToken::never())
            .await
            .unwrap_or_else(|e| panic!("turn {idx}: {e}"));
        eprintln!(
            "turn{idx}: text={:?} prompt={} completion={} cache_read={} cache_write={} thinking={}",
            reply.text,
            reply.usage.prompt_tokens,
            reply.usage.completion_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
            reply.usage.thinking_tokens
        );
        assert!(
            !reply.text.trim().is_empty() || !reply.tool_calls.is_empty(),
            "turn {idx} returned no text and no tool calls"
        );
        reads.push(reply.usage.cache_read_tokens);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let hit = reads.iter().skip(1).any(|n| *n > 0);
    eprintln!(
        "cli_default_with_key_cache_hit_reported={hit} later_cache_reads={:?}",
        &reads[1..]
    );
}
