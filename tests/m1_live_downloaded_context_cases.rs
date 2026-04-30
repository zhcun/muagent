//! Live long-context cases backed by downloaded public haystack text.
//!
//! Data source: Paul Graham essays from the Needle-in-a-Haystack corpus used by
//! RULER-style long-context retrieval tests. Download with:
//!
//! ```bash
//! bench/context_cases/download.sh
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::compactor::Compactor;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::thinking::ThinkingConfig;
use muagent::core::types::{Content, Message, ObsKind};
use muagent::providers::OpenAiAdapter;
use muagent::runtime::{prelude::*, token_estimate};
use muagent::sessions::compaction::{CompactionBudget, RunnerCompactor, SummaryCompaction};
use muagent::storage::MemorySessionStore;
use uuid::Uuid;

const ANCHOR_CODE: &str = "PG-NIAH-7429-SUMMIT";
const ANCHOR_OWNER: &str = "MIRA-SOL-3185";
const LIVE_SYS: &str = "You are evaluating long-context memory. Answer exact \
    memory anchors only. If the requested anchor is not present in the visible \
    conversation context, answer UNKNOWN. Do not infer or invent anchors.";

const ESSAY_FILES: &[&str] = &[
    "avg.txt",
    "before.txt",
    "desres.txt",
    "gap.txt",
    "gh.txt",
    "siliconvalley.txt",
    "startuplessons.txt",
    "superangels.txt",
    "useful.txt",
    "web20.txt",
    "wisdom.txt",
    "worked.txt",
];

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
    let model = std::env::var("MUAGENT_LIVE_CONTEXT_MODEL")
        .or_else(|_| std::env::var("OPENROUTER_MODEL"))
        .unwrap_or_else(|_| "openai/gpt-5.4-nano".into());
    (key, base, model)
}

fn build_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    eprintln!("provider=openrouter model={model}");
    let net = Arc::new(ReqwestEgress::new().expect("reqwest egress"));
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

fn u32_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

fn bool_env(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(default)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn context_root() -> PathBuf {
    std::env::var_os("MUAGENT_CONTEXT_CASE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("bench/context_cases/data/paul_graham"))
}

fn load_paul_graham_essays() -> Vec<(String, String)> {
    let root = context_root();
    let mut essays = Vec::new();
    let mut missing = Vec::new();
    for file in ESSAY_FILES {
        let path = root.join(file);
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => essays.push(((*file).to_string(), text)),
            _ => missing.push(path),
        }
    }
    assert!(
        missing.is_empty(),
        "missing downloaded context case files under {}. Run `bench/context_cases/download.sh` first. Missing: {:?}",
        root.display(),
        missing
    );
    essays
}

fn anchor_for_chunk(index: usize) -> &'static str {
    match index {
        1 | 2 => {
            "HIGH PRIORITY MEMORY ANCHOR: the exact RULER case code is \
             PG-NIAH-7429-SUMMIT. Preserve this exact identifier for a later \
             memory check."
        }
        5 | 6 => {
            "HIGH PRIORITY MEMORY ANCHOR: the exact RULER case owner is \
             MIRA-SOL-3185. Preserve this exact identifier for a later memory \
             check."
        }
        _ => "",
    }
}

fn build_long_context_history() -> Vec<Message> {
    let essays = load_paul_graham_essays();
    let mut history = vec![
        Message::User {
            content: Content::text(
                "We are starting a RULER-style long-context memory exercise. \
                 I will paste public essay haystack text and may embed exact \
                 memory anchors. Do not answer the memory check until I ask.",
            ),
        },
        Message::Assistant {
            content: Content::text("Acknowledged."),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];

    for (idx, (name, text)) in essays.into_iter().enumerate() {
        let anchor = anchor_for_chunk(idx);
        let content = format!(
            "Context chunk {idx:02} from public Paul Graham essay `{name}`.\n\
             {anchor}\n\n\
             ```text\n{text}\n```"
        );
        history.push(Message::User {
            content: Content::text(content),
        });
        history.push(Message::Assistant {
            content: Content::text(format!("Recorded context chunk {idx:02} from `{name}`.")),
            tool_calls: vec![],
            thinking: vec![],
        });
    }

    history
}

fn memory_question() -> Message {
    Message::User {
        content: Content::text(
            "Memory check: return the exact RULER case code and the exact \
             RULER case owner from the earlier haystack context. Use exactly \
             this format: code=<value> owner=<value>. If either value is not \
             visible, use UNKNOWN for that value.",
        ),
    }
}

fn score_answer(text: &str) -> usize {
    let upper = text.to_ascii_uppercase();
    [ANCHOR_CODE, ANCHOR_OWNER]
        .iter()
        .filter(|needle| upper.contains(&needle.to_ascii_uppercase()))
        .count()
}

async fn ask_model(model: &dyn ModelAdapter, history: Vec<Message>, cache_key: &str) -> String {
    let reply = model
        .turn(
            ModelRequest {
                system: LIVE_SYS.to_string(),
                runtime_context: String::new(),
                messages: history,
                tools: vec![],
                temperature: Some(0.0),
                stream: false,
                cache: CachePolicy::Auto,
                thinking: ThinkingConfig::off(),
                prompt_cache_key: Some(cache_key.to_string()),
            },
            CancelToken::never(),
        )
        .await
        .expect("live model turn");
    reply.text
}

fn tail_only_history(history: &[Message]) -> Vec<Message> {
    let mut out = Vec::new();
    out.extend(history.iter().take(2).cloned());
    out.extend(
        history
            .iter()
            .skip(history.len().saturating_sub(4))
            .cloned(),
    );
    out.push(memory_question());
    out
}

fn compacted_summary_text(history: &[Message]) -> Option<String> {
    history.iter().find_map(|message| match message {
        Message::Observation {
            kind: ObsKind::Summary,
            text,
        } => Some(text.clone()),
        _ => None,
    })
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

#[ignore = "hits real OpenRouter API and sends downloaded long-context cases"]
#[tokio::test]
async fn live_downloaded_paul_graham_case_compaction_preserves_anchors() {
    let model = build_model();
    let history = build_long_context_history();
    let joined_history = format!("{history:?}");
    assert!(
        joined_history.contains(ANCHOR_CODE) && joined_history.contains(ANCHOR_OWNER),
        "test builder must place both anchors in the downloaded context history"
    );
    let tokens_before_question = token_estimate::estimate_history_tokens(&history);
    eprintln!(
        "-- downloaded context: messages={} estimated_tokens={}",
        history.len(),
        tokens_before_question
    );
    assert!(
        tokens_before_question > 45_000,
        "downloaded haystack should be substantial, got ~{tokens_before_question} tokens"
    );

    let mut raw_history = history.clone();
    raw_history.push(memory_question());
    let raw_answer = ask_model(&*model, raw_history, "pg-context-raw-v1").await;
    let raw_score = score_answer(&raw_answer);
    eprintln!("-- raw full-context answer: {raw_answer}");
    eprintln!("-- raw full-context score: {raw_score}/2");
    if raw_score < 2 {
        eprintln!(
            "-- note: raw full-context retrieval missed at least one anchor; \
             keeping this as a diagnostic because the compaction test is the \
             actual gate"
        );
    }

    let tail_answer = ask_model(&*model, tail_only_history(&history), "pg-context-tail-v1").await;
    let tail_score = score_answer(&tail_answer);
    eprintln!("-- tail-only baseline answer: {tail_answer}");
    eprintln!("-- tail-only baseline score: {tail_score}/2");
    assert_eq!(
        tail_score, 0,
        "tail-only baseline should not recover anchors that are outside the retained tail"
    );

    let registry = Arc::new(CapabilityRegistry::new());
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let budget = CompactionBudget {
        max_tokens: u32_env("MUAGENT_CONTEXT_CASE_MAX_TOKENS", 55_000),
        threshold_ratio: 0.8,
        keep_tail_turns: 2,
        summary_target_chars: u32_env("MUAGENT_CONTEXT_CASE_SUMMARY_TARGET_CHARS", 2200),
        summary_input_max_tokens: u32_env("MUAGENT_CONTEXT_CASE_SUMMARY_INPUT_MAX_TOKENS", 90_000),
        summary_output_max_tokens: u32_env("MUAGENT_CONTEXT_CASE_SUMMARY_OUTPUT_MAX_TOKENS", 4_000),
        max_summary_rounds: 2,
        ..CompactionBudget::default()
    };
    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(budget.clone()),
        model.clone(),
    ));
    let summary_recall = bool_env("MUAGENT_CONTEXT_CASE_SUMMARY_RECALL", false);
    let runner = Runner::builder()
        .model(model.clone())
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(LIVE_SYS)
        .compactor(compactor)
        .thinking(ThinkingConfig::off())
        .summary_recall(summary_recall)
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    state.history = history;
    runner
        .submit_user_message(&mut state, memory_question())
        .await
        .unwrap();
    let tokens_at_question = token_estimate::estimate_history_tokens(&state.history);
    eprintln!(
        "-- before runner step: history_tokens={} max_tokens={} threshold={} summary_target_chars={} summary_input_max_tokens={} summary_output_max_tokens={} summary_recall={}",
        tokens_at_question,
        budget.max_tokens,
        (budget.max_tokens as f32 * budget.threshold_ratio) as u32,
        budget.summary_target_chars,
        budget.summary_input_max_tokens,
        budget.summary_output_max_tokens,
        summary_recall
    );

    drive_until_terminal(&runner, &mut state, 8).await;

    let summary = compacted_summary_text(&state.history).expect("summary observation missing");
    let summary_score = score_answer(&summary);
    eprintln!("-- compacted summary score: {summary_score}/2");
    eprintln!("-- compacted summary:\n{summary}");
    assert_eq!(
        summary_score, 2,
        "compaction summary should preserve both exact anchors"
    );

    let compacted_answer = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done after compacted memory check, got {other:?}"),
    };
    let compacted_score = score_answer(&compacted_answer);
    eprintln!("-- compacted answer: {compacted_answer}");
    eprintln!("-- compacted answer score: {compacted_score}/2");
    assert_eq!(
        compacted_score, 2,
        "current compaction should preserve both downloaded-context anchors"
    );
}
