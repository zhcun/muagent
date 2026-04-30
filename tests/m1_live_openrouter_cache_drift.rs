//! Live experiment: where exactly does a per-turn-changing string in the
//! prefix actually kill prompt-cache on `openai/gpt-5.4-nano` (via
//! OpenRouter)?
//!
//! Three scenarios are run; the printed `cache_read_tokens` numbers are the
//! evidence:
//!
//! - **stable_prefix**: nothing in the prefix changes; only an appended
//!   user message grows. cache_read should grow across turns.
//! - **dynamic_prefix**: a per-turn-changing line ("turn: N") sits in a
//!   SEPARATE second system message between the cacheable system text and
//!   the message history. Empirical result: cache_read STILL grows — OpenAI
//!   hashes per-message, not strict cumulative byte prefix.
//! - **dynamic_inside_system_text_probe**: the per-turn-changing line is
//!   concatenated INTO the cacheable system message text. Empirical result:
//!   cache_read FLATLINES at whatever falls before the changing line.
//!
//! Conclusion (for OpenAI / OpenRouter): per-turn data only kills cache
//! if it lives **inside** a message that is otherwise cacheable. Putting
//! it as a separate message — which is exactly what sagent does for L2
//! `runtime_context` — does not break history caching for OpenAI.
//!
//! Anthropic, by contrast, advertises strict byte-prefix matching with
//! `cache_control` breakpoints and may behave differently. This file does
//! not currently include an Anthropic-side test.
//!
//! Run: `cargo test -p muagent --test m1_live_openrouter_cache_drift -- --ignored --nocapture`
//!
//! Reads `OPENROUTER_API_KEY` from env or `.env`. Costs are tiny (<$0.01).

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

/// ~1500 tokens of stable text. Has to be > 1024 to cross OpenAI's minimum
/// cacheable prefix size.
fn big_stable_system() -> String {
    let mut s = String::from(
        "You are a cache probe. Reply only with: OK\n\
         Below is a long stable instruction block, intentionally repeated verbatim across turns:\n",
    );
    for i in 0u32..600 {
        s.push_str(&format!(
            "Stable instruction line {i:04}: alpha beta gamma delta epsilon zeta value {:08x}.\n",
            i.wrapping_mul(0x01f1_23bb)
        ));
    }
    s
}

/// Build an `n`-turn history of LARGE user/assistant exchanges. Each turn
/// is intentionally > 128 tokens so it lands on the next 128-token cache
/// chunk boundary that OpenAI's prompt cache tracks — otherwise the cache
/// can't actually demonstrate growth past the system block.
fn fixed_history(n: usize) -> Vec<Message> {
    let mut h = Vec::with_capacity(n * 2);
    for i in 0..n {
        // ~600 tokens of stable user content per turn.
        let mut user_text = format!(
            "Step {i:03}: please review the following deterministic block and acknowledge with exactly: OK\n"
        );
        for j in 0..50u32 {
            user_text.push_str(&format!(
                "  detail-line {j:03}/{i:03}: lorem ipsum dolor sit amet consectetur \
                 adipiscing elit sed do eiusmod tempor incididunt ut labore et \
                 dolore magna aliqua deterministic-token {:08x}\n",
                (i as u32 * 1009 + j).wrapping_mul(0x01f1_23bb)
            ));
        }
        h.push(Message::User {
            content: Content::text(user_text),
        });
        // ~150 tokens of stable assistant reply.
        let mut asst_text = String::from("OK. acknowledged. summary follows:\n");
        for j in 0..15u32 {
            asst_text.push_str(&format!(
                "  ack-line {j:02}/{i:03}: deterministic confirmation token {:08x}\n",
                (i as u32 * 7919 + j).wrapping_mul(0x9e37_79b1)
            ));
        }
        h.push(Message::Assistant {
            content: Content::text(asst_text),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    h
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn stable_prefix_grows_cache_read_over_turns() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = big_stable_system();

    eprintln!(
        "== stable_prefix == provider=openrouter model={model} system_chars={} ",
        system.len()
    );

    let mut reads = Vec::new();
    let mut prompts = Vec::new();
    // Each turn appends one more user/assistant pair to history. The system
    // block AND prior history are byte-identical to the previous turn — only
    // the very last user message is new.
    for turns_in_history in 0usize..=4 {
        let mut messages = fixed_history(turns_in_history);
        messages.push(Message::User {
            content: Content::text("Final probe — reply OK."),
        });
        let req = ModelRequest {
            system: system.clone(),
            runtime_context: String::new(),
            messages,
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
            .unwrap_or_else(|e| panic!("history={turns_in_history}: {e}"));
        eprintln!(
            "  history_turns={turns_in_history} prompt={} cache_read={} cache_write={}",
            reply.usage.prompt_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
        );
        reads.push(reply.usage.cache_read_tokens);
        prompts.push(reply.usage.prompt_tokens);
        // Stay inside cache TTL.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    eprintln!("  cache_reads={:?} prompts={:?}", reads, prompts);
    // Hard assertion: the cache must EXTEND with history — i.e. the last
    // turn's cache_read must be strictly greater than the first turn's.
    // This is the property that's destroyed by inserting a per-turn-changing
    // string into the prefix; see `dynamic_prefix_string_collapses_history_cache`
    // for the foil. If this assertion ever flips, the core has regressed —
    // most likely because something started rendering per-turn data into
    // `runtime_context` again.
    let first = reads[0];
    let last = *reads.last().unwrap();
    assert!(
        last > first,
        "stable prefix did NOT extend cache_read with history: first={first} \
         last={last} reads={reads:?}. Either the core leaked per-turn data \
         into the prefix, or the test history is below the provider's chunk \
         boundary."
    );
}

/// Probe: put the per-turn-changing string **inside the system text itself**
/// (concatenated, not a separate message). If cache_read still grows with
/// history, OpenAI's prompt cache must be using content-defined / per-chunk
/// independent hashing — small mid-prefix changes don't propagate. If
/// cache_read flatlines at whatever falls *before* the changing line, the
/// cache is doing strict positional/cumulative hashing.
#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn dynamic_inside_system_text_probe() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let stable_system = big_stable_system();

    eprintln!(
        "== dynamic_inside_system == provider=openrouter model={model} stable_chars={} ",
        stable_system.len()
    );

    let mut reads = Vec::new();
    let mut prompts = Vec::new();
    for turns_in_history in 0usize..=4 {
        let mut messages = fixed_history(turns_in_history);
        messages.push(Message::User {
            content: Content::text("Final probe — reply OK."),
        });
        // Inline the per-turn change into the cacheable text itself.
        let system = format!(
            "{stable_system}\n## Runtime context\nturn: {}\n",
            turns_in_history + 1
        );
        let req = ModelRequest {
            system,
            runtime_context: String::new(),
            messages,
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
            .unwrap_or_else(|e| panic!("history={turns_in_history}: {e}"));
        eprintln!(
            "  history_turns={turns_in_history} prompt={} cache_read={} cache_write={}",
            reply.usage.prompt_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
        );
        reads.push(reply.usage.cache_read_tokens);
        prompts.push(reply.usage.prompt_tokens);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!(
        "  cache_reads={:?} prompts={:?}\n\
         decision rule: if cache_read GROWS with history -> OpenAI is using \
         per-chunk content-independent hashing (mid-prefix changes don't \
         break later chunks). If it FLATLINES -> strict positional hashing.",
        reads, prompts
    );
}

#[ignore = "hits real OpenRouter API; run with --ignored --nocapture"]
#[tokio::test]
async fn dynamic_prefix_string_collapses_history_cache() {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    let adapter = OpenAiAdapter::new(net, &base, &model, Some(key));
    let system = big_stable_system();

    eprintln!(
        "== dynamic_prefix == provider=openrouter model={model} system_chars={} ",
        system.len()
    );

    // Same shape as the stable test, but `runtime_context` carries a string
    // that changes EVERY turn. This mirrors what `RuntimeFacts::render()`
    // used to emit when it included `turn: N`. Bytes after this block —
    // i.e., the entire history — depend on it for prefix matching.
    let mut reads = Vec::new();
    let mut prompts = Vec::new();
    for turns_in_history in 0usize..=4 {
        let mut messages = fixed_history(turns_in_history);
        messages.push(Message::User {
            content: Content::text("Final probe — reply OK."),
        });
        // Per-turn-varying line, *just* like the old runtime_context.
        let runtime_context = format!(
            "## Runtime context\ncurrent_date_utc: 2026-04-27\nturn: {}\n",
            turns_in_history + 1
        );
        let req = ModelRequest {
            system: system.clone(),
            runtime_context,
            messages,
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
            .unwrap_or_else(|e| panic!("history={turns_in_history}: {e}"));
        eprintln!(
            "  history_turns={turns_in_history} prompt={} cache_read={} cache_write={}",
            reply.usage.prompt_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
        );
        reads.push(reply.usage.cache_read_tokens);
        prompts.push(reply.usage.prompt_tokens);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    eprintln!(
        "  cache_reads={:?} prompts={:?}\n\
         empirical finding for openai/gpt-5.4-nano via OpenRouter:\n\
         cache_read EXTENDS across turns even with a per-turn-changing\n\
         system message in the middle — i.e. OpenAI hashes message content\n\
         per-message, not as a strict cumulative byte prefix. Compare with\n\
         `dynamic_inside_system_text_probe`: when the changing string is\n\
         concatenated INTO the system text itself, cache_read flatlines.\n\
         Implication: for OpenAI, sagent's L2 runtime_context as a separate\n\
         message is fine; the cache killer would only be per-turn data\n\
         leaking INSIDE a single cacheable message's text.",
        reads, prompts
    );
    // Note: cache_read DOES grow here, contrary to a strict-prefix-match
    // hypothesis. We do NOT assert the opposite — the test exists to
    // document the actual provider behavior, not enforce a model that
    // turned out to be wrong.
}
