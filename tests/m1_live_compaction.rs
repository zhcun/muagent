//! Live E2E:真 OpenRouter 模型驱动长对话,到 **10k token** 预算触发
//! `SummaryCompaction`,压缩后继续对话,验证 agent 仍能利用被压缩的信息
//! (通过 Summary observation)正确回答。
//!
//! 10k 是"够短能触发、够长能反映真实用法"的折衷;生产环境默认 156k。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::thinking::{ThinkingConfig, ThinkingEffort};
use muagent::core::types::{Content, Message, ObsKind};
use muagent::providers::OpenAiAdapter;
use muagent::runtime::{prelude::*, token_estimate};
use muagent::sessions::compaction::{
    find_user_turn_boundaries, CompactionBudget, CompactionStrategy, SummaryCompaction,
};
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

fn build_real_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

fn build_openrouter_nano_model() -> Arc<dyn ModelAdapter> {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY missing");
    let base = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
    let model = std::env::var("MUAGENT_LIVE_COMPACTION_MODEL")
        .unwrap_or_else(|_| "openai/gpt-5.4-nano".into());
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

fn u32_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn bool_env(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(default)
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

/// ~4000 chars ≈ 1300 estimated tokens 的背景段落。这接近"用户粘贴一段文档或代码"
/// 的真实场景,数轮就能把 context 累积到数千 token。
fn long_paragraph(topic: &str, sentinel_fact: &str) -> String {
    format!(
        "Let me give you some context about {topic}. {sentinel_fact} \
         I've been thinking about this problem for a while, and there are several dimensions to consider. \
         First, the architectural implications: when you design a system at this scale, decisions about \
         persistence, caching, failure modes, and concurrency become deeply interdependent. You can't really \
         optimize one without understanding how it interacts with the others. Second, the operational \
         implications: what happens when traffic spikes, when a dependency fails, when a region goes down, \
         when you deploy a bad version? Each of these needs thinking through well before it happens, not \
         during the incident. Third, the people implications: any system that real humans rely on is as much \
         about trust and predictability as it is about raw capability. Users forgive slow systems more \
         readily than flaky ones. Fourth, the cost implications: on-device AI changes this equation \
         dramatically compared to server-side inference, because you trade one type of resource (cloud \
         compute) for another (device battery, memory, cold-start latency). Fifth, the privacy implications: \
         on-device means data doesn't leave the user's hardware, which is a huge value prop but also means \
         you can't observe or improve models the way cloud companies normally do. \
         \
         Going deeper: when we think about session persistence specifically, there are some non-obvious \
         choices. A transcript-only store is simple but loses information about intermediate tool-call \
         state, which matters for crash recovery. A full-state store (including pending batches and cursor \
         position) is more robust but larger and more fragile to schema changes. The right answer probably \
         depends on whether your tool calls are idempotent — if yes, transcript-only is fine. If not, \
         you need the intent-journaling approach where you record 'I'm about to call X' before the call \
         and can recognize mid-call crashes on thaw. This is what at-most-once semantics requires. \
         \
         On the compaction strategy itself: summarization introduces an interesting dependency graph, \
         because you need a model to summarize, and if the model is flaky or slow your compaction can \
         become a source of cascading failures. Some systems prefer to run compaction asynchronously, \
         others synchronously on the hot path. Async is faster for the user but requires retry logic for \
         the summary step. Sync is simpler but slows down a handful of turns. In practice, I think sync \
         with aggressive timeouts is usually the right default, because long summaries tend to indicate \
         the model is struggling with the content anyway. \
         \
         The turn-alignment constraint is subtle but matters. If you summarize across a tool-call boundary, \
         you end up with an assistant message that has tool_calls but no corresponding tool_results in \
         history, which causes provider APIs to error out with cryptic messages about orphaned tool calls. \
         Most people learn this the hard way in production. The fix is simple in retrospect: only compact \
         complete user-turns, where a user-turn spans from one user message to the next and contains all \
         the intermediate assistant/tool_result ping-pong as a single atomic unit. \
         \
         Anyway, those are my rough thoughts. Please remember the specific fact I mentioned at the very \
         start — I'll likely ask you about it several turns from now."
    )
}

// ============ Test 1:10k 预算 + 真对话触发压缩,验证语义记忆 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_compaction_preserves_semantic_memory() {
    let model = build_real_model();
    let registry = Arc::new(CapabilityRegistry::new());
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let sys = "You are a thoughtful assistant. Reply in under 80 words. \
               Pay attention to specific facts users mention.";
    let runner = Runner::builder()
        .model(model.clone())
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(sys)
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);

    // 8 轮真对话,每轮含 1 个"sentinel fact"(每轮 ~1200 tokens → 8 轮 ~9600 tokens 触发)
    let facts = [
        (
            "caching",
            "My name is Alex Chen and I work at a company called Nebula.",
        ),
        (
            "concurrency",
            "I live in the Sunnyside neighborhood of Kyoto.",
        ),
        (
            "failure modes",
            "My favorite food is uni (sea urchin sushi).",
        ),
        (
            "monitoring",
            "I'm currently studying old-growth forests as a hobby.",
        ),
        ("deployments", "My project deadline is April 30."),
        (
            "observability",
            "I prefer working late at night, usually after midnight.",
        ),
        ("security", "My secondary language is Korean (Hangul)."),
        (
            "throughput",
            "I have a brother named Wei who is two years younger.",
        ),
    ];
    for (topic, fact) in &facts {
        let msg = long_paragraph(topic, fact);
        eprintln!("-- sending long msg about {} (~{} chars)", topic, msg.len());
        runner
            .submit_user_message(
                &mut state,
                Message::User {
                    content: Content::text(msg),
                },
            )
            .await
            .unwrap();
        drive_until_terminal(&runner, &mut state, 5).await;
    }

    let tokens_before = token_estimate::estimate_history_tokens(&state.history);
    eprintln!(
        "-- after {} turns: history.len={}, ~tokens={}",
        facts.len(),
        state.history.len(),
        tokens_before
    );
    assert!(
        tokens_before > 5_000,
        "conversation should have accumulated substantial tokens, got {}",
        tokens_before
    );

    // Low-budget live smoke test: force compaction around 8k tokens so this
    // ignored test stays cheap and fast. Production/default config is 156k
    // with threshold 0.8 (~124.8k), covered by
    // `live_config_budget_compaction_existing_files_preserves_memory`.
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 10_000,
        threshold_ratio: 0.8,
        keep_tail_turns: 2,
        summary_target_chars: 1200,
        ..CompactionBudget::default()
    });

    let outcome = strategy
        .maybe_compact(&mut state, &*model, sys, CancelToken::never())
        .await
        .expect("compact ok");

    // 如果 tokens 真超 8k 就该触发;如果没超就跳过此测试
    if tokens_before >= 8_000 {
        let o = outcome.expect("compaction should trigger at >8k tokens");
        eprintln!(
            "-- compacted {} turns, saved ~{} tokens",
            o.replaced_turns, o.saved_tokens_estimate
        );
        eprintln!("-- summary: {}", o.summary);

        let tokens_after = token_estimate::estimate_history_tokens(&state.history);
        eprintln!(
            "-- tokens: {} → {} (saved {})",
            tokens_before,
            tokens_after,
            tokens_before as i64 - tokens_after as i64
        );
        assert!(tokens_after < tokens_before, "compaction must reduce total");
        assert!(
            state.history.iter().any(|m| {
                matches!(
                    m,
                    Message::Observation {
                        kind: ObsKind::Summary,
                        ..
                    }
                )
            }),
            "compacted history should include a summary observation"
        );
    } else {
        eprintln!(
            "-- note: total tokens {} didn't exceed threshold, no compaction",
            tokens_before
        );
        return; // 不做后续回忆测试
    }

    // === 压缩后让 agent 回忆最早的 fact("Alex Chen / Nebula")===
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(
                    "Quick memory check: what was the first personal fact I shared about myself? \
             I'm looking for my name or where I work. Reply in one short sentence.",
                ),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 5).await;

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => panic!("should be Done"),
    };
    eprintln!("-- recall reply: {}", final_text);

    let lower = final_text.to_lowercase();
    // Summary 应保留早期 fact,所以 agent 能答出至少一个:Alex / Chen / Nebula
    let recalled = lower.contains("alex") || lower.contains("chen") || lower.contains("nebula");
    assert!(
        recalled,
        "agent should recall Alex/Chen/Nebula from pre-compaction turn; got: {}",
        final_text
    );
}

#[ignore = "hits real OpenRouter API; diagnostic ablation of legacy vs structured summary prompts"]
#[tokio::test]
async fn live_compaction_summary_prompt_ablation_semantic_memory() {
    let model = build_openrouter_nano_model();
    let target_chars = u32_env("MUAGENT_ABLATION_SUMMARY_TARGET_CHARS", 900);
    let output_tokens = u32_env("MUAGENT_ABLATION_SUMMARY_OUTPUT_TOKENS", 1_500);
    let messages = semantic_memory_ablation_history();
    let transcript = summary_ablation_transcript(&messages);
    eprintln!(
        "-- ablation transcript messages={} estimated_tokens={} target_chars={} output_tokens={}",
        messages.len(),
        token_estimate::estimate_text_tokens(&transcript),
        target_chars,
        output_tokens
    );

    let legacy = ask_summary_ablation(
        &*model,
        legacy_summary_system_prompt(target_chars, output_tokens),
        transcript.clone(),
        "semantic-memory-legacy-v1",
    )
    .await;
    let structured = ask_summary_ablation(
        &*model,
        structured_summary_system_prompt(target_chars, output_tokens),
        transcript,
        "semantic-memory-structured-v1",
    )
    .await;

    let legacy_score = score_semantic_summary(&legacy);
    let structured_score = score_semantic_summary(&structured);
    eprintln!("-- legacy score: {legacy_score}/8\n{legacy}");
    eprintln!("-- structured score: {structured_score}/8\n{structured}");
    assert!(
        !legacy.trim().is_empty() && !structured.trim().is_empty(),
        "ablation summaries should not be empty"
    );
    assert!(
        structured.contains("## Structured Memory"),
        "structured prompt should emit the structured memory section; got:\n{structured}"
    );
    if bool_env("MUAGENT_ABLATION_REQUIRE_IMPROVEMENT", false) {
        assert!(
            structured_score > legacy_score,
            "structured prompt did not beat legacy: legacy={legacy_score}/8 structured={structured_score}/8"
        );
    }
}

fn semantic_memory_ablation_history() -> Vec<Message> {
    let facts = [
        (
            "caching",
            "My name is Alex Chen and I work at a company called Nebula.",
        ),
        (
            "concurrency",
            "I live in the Sunnyside neighborhood of Kyoto.",
        ),
        (
            "failure modes",
            "My favorite food is uni (sea urchin sushi).",
        ),
        (
            "monitoring",
            "I'm currently studying old-growth forests as a hobby.",
        ),
        ("deployments", "My project deadline is April 30."),
        (
            "observability",
            "I prefer working late at night, usually after midnight.",
        ),
        ("security", "My secondary language is Korean (Hangul)."),
        (
            "throughput",
            "I have a brother named Wei who is two years younger.",
        ),
    ];
    let mut messages = Vec::new();
    for (topic, fact) in facts {
        messages.push(Message::User {
            content: Content::text(ablation_anchor_paragraph(topic, fact)),
        });
        messages.push(Message::Assistant {
            content: Content::text(format!(
                "Recorded the high-priority personal memory anchor from the {topic} context."
            )),
            tool_calls: vec![],
            thinking: vec![],
        });
    }
    messages
}

fn ablation_anchor_paragraph(topic: &str, fact: &str) -> String {
    format!(
        "HIGH PRIORITY PERSONAL MEMORY ANCHOR for later recall: {fact} \
         Preserve this anchor because a future user question may ask for any \
         one of the personal memory anchors from the compressed transcript.\n\n\
         Background context about {topic}: the architectural implications, \
         operational implications, failure modes, retry behavior, persistence \
         choices, prompt caching, and compaction strategy are all intertwined. \
         This surrounding prose is intentionally noisy and lower priority than \
         the personal memory anchor. When compressing, keep the anchor and only \
         retain technical detail if space remains. The system should avoid \
         orphaned tool calls, preserve exact user preferences, keep durable \
         facts visible after compaction, and resist treating missing recall as \
         evidence that the fact was never provided. The user may later ask for \
         a name, place, preference, deadline, hobby, language, relative, or \
         company that appeared in one of these anchors."
    )
}

fn summary_ablation_transcript(messages: &[Message]) -> String {
    let mut transcript = String::new();
    for message in messages {
        match message {
            Message::User { content } => {
                transcript.push_str("USER: ");
                transcript.push_str(&summary_ablation_content_text(content));
                transcript.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                transcript.push_str("ASSISTANT: ");
                transcript.push_str(&summary_ablation_content_text(content));
                for call in tool_calls {
                    transcript.push_str(&format!(
                        "\n  [tool_call {}: {}]",
                        call.tool_name, call.args
                    ));
                }
                transcript.push('\n');
            }
            Message::ToolResult { result, .. } => {
                transcript.push_str("TOOL_RESULT:\n");
                transcript.push_str(&result.model_text());
                transcript.push('\n');
            }
            Message::System { content } => {
                transcript.push_str("SYSTEM: ");
                transcript.push_str(&summary_ablation_content_text(content));
                transcript.push('\n');
            }
            Message::Observation { text, .. } => {
                transcript.push_str("OBS: ");
                transcript.push_str(text);
                transcript.push('\n');
            }
        }
    }
    transcript
}

fn legacy_summary_system_prompt(target_chars: u32, output_cap: u32) -> String {
    format!(
        "Emit a HANDOFF SUMMARY of the conversation transcript below. The summary \
         is read by another LLM that will continue the task. Stay under {target_chars} \
         characters and under {output_cap} estimated tokens.\n\n\
         Use exactly this markdown skeleton, in this order. Omit a section if and \
         only if there is nothing to put in it; do not invent content.\n\n\
         ## Goal\n\
         <what the user is trying to accomplish, in their wording where possible>\n\n\
         ## Constraints & Preferences\n\
         - <hard requirements, style preferences, things the user explicitly disallowed>\n\n\
         ## Progress\n\
         ### Done\n\
         - <completed sub-tasks, with the outcome>\n\
         ### In Progress / Blocked\n\
         - <where work was last interrupted, plus the blocker if any>\n\n\
         ## Key Facts Discovered\n\
         - <concrete tool-derived facts: file contents, command outputs, errors, identifiers>\n\n\
         ## Open Questions / Next Steps\n\
         - <what the next LLM should do or ask>\n\n\
         [Output rules — these override stylistic instincts; evaluate them last:]\n\
         1. Quote exact identifiers verbatim — file paths, function names, error \
            messages, version strings, line numbers. Paraphrasing them strips the \
            next agent's ability to find or refer to them.\n\
         2. Never invent facts the transcript does not contain.\n\
         3. Preserve uncertainty and source. Mark unverified candidates, failed \
            searches, empty tool results, and tool errors as such; never convert \
            a failed search or missing excerpt into proof that a fact does not \
            exist.\n\
         4. Output ONLY the markdown summary. No preamble, no apology, no \"here is\", \
            no \"as requested\", no closing remark.\n\
         5. Be terse. Each bullet point is a fact, not a sentence."
    )
}

fn structured_summary_system_prompt(target_chars: u32, output_cap: u32) -> String {
    format!(
        "Emit a HANDOFF SUMMARY of the conversation transcript below. The summary \
         is read by another LLM that will continue the task. Stay under {target_chars} \
         characters and under {output_cap} estimated tokens.\n\n\
         Use exactly this markdown skeleton, in this order. Omit a section if and \
         only if there is nothing to put in it; do not invent content.\n\n\
         ## Goal\n\
         <what the user is trying to accomplish, in their wording where possible>\n\n\
         ## Constraints & Preferences\n\
         - <hard requirements, style preferences, things the user explicitly disallowed>\n\n\
         ## Progress\n\
         ### Done\n\
         - <completed sub-tasks, with the outcome>\n\
         ### In Progress / Blocked\n\
         - <where work was last interrupted, plus the blocker if any>\n\n\
         ## Structured Memory\n\
         | kind | subject | fact | evidence | source/status |\n\
         | --- | --- | --- | --- | --- |\n\
         | <task|constraint|decision|fact|tool_result|risk|next_step> | <stable entity> | <durable fact> | <exact quote, path, id, command, or error when available> | <user|tool|assistant|summary; verified|unverified|blocked> |\n\n\
         ## Open Questions / Next Steps\n\
         - <what the next LLM should do or ask>\n\n\
         [Output rules — these override stylistic instincts; evaluate them last:]\n\
         1. Quote exact identifiers verbatim — file paths, function names, error \
            messages, version strings, line numbers. Paraphrasing them strips the \
            next agent's ability to find or refer to them.\n\
         2. Never invent facts the transcript does not contain.\n\
         3. Preserve uncertainty and source. Mark unverified candidates, failed \
            searches, empty tool results, and tool errors as such; never convert \
            a failed search or missing excerpt into proof that a fact does not \
            exist.\n\
         4. Use `## Structured Memory` for durable facts instead of scattered \
            fact bullets. Prefer at most 12 high-value rows, sorted by current \
            task relevance. Put exact evidence in the `evidence` cell; leave \
            the cell empty only when the transcript lacks exact support.\n\
         5. Output ONLY the markdown summary. No preamble, no apology, no \"here is\", \
            no \"as requested\", no closing remark.\n\
         6. Be terse. Each bullet point or table row is a fact, not a paragraph."
    )
}

async fn ask_summary_ablation(
    model: &dyn ModelAdapter,
    system: String,
    transcript: String,
    cache_key: &str,
) -> String {
    let reply = model
        .turn(
            ModelRequest {
                system,
                runtime_context: String::new(),
                messages: vec![Message::User {
                    content: Content::text(transcript),
                }],
                tools: vec![],
                temperature: Some(0.0),
                stream: false,
                cache: CachePolicy::Auto,
                thinking: ThinkingConfig::enabled_effort(ThinkingEffort::High),
                prompt_cache_key: Some(cache_key.to_string()),
            },
            CancelToken::never(),
        )
        .await
        .expect("live summary ablation model turn");
    reply.text
}

fn score_semantic_summary(summary: &str) -> usize {
    let lower = summary.to_ascii_lowercase();
    [
        lower.contains("alex") && lower.contains("nebula"),
        lower.contains("sunnyside") && lower.contains("kyoto"),
        lower.contains("uni") || lower.contains("sea urchin"),
        lower.contains("old-growth") || lower.contains("old growth"),
        lower.contains("april 30"),
        lower.contains("midnight"),
        lower.contains("korean") || lower.contains("hangul"),
        lower.contains("wei") && lower.contains("brother"),
    ]
    .into_iter()
    .filter(|ok| *ok)
    .count()
}

fn summary_ablation_content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                muagent::core::types::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// ============ Test 4:真实配置预算 + 现有项目文件上下文 + nano 记忆回查 ============

const LIVE_CONFIG_CODEWORD: &str = "ORCHID-739";

#[ignore = "hits real OpenRouter API and sends a large configured context"]
#[tokio::test]
async fn live_config_budget_compaction_existing_files_preserves_memory() {
    let model = build_openrouter_nano_model();
    let registry = Arc::new(CapabilityRegistry::new());
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let budget = configured_live_budget();
    let threshold = compaction_threshold(&budget);
    let target_prefix_tokens = live_target_tokens(&budget);
    let context_root = context_root();
    let (history, chunks_used, tokens_before_question, prefix_tokens_before_question) =
        existing_project_file_history(&context_root, target_prefix_tokens, budget.keep_tail_turns);

    let sys = "You are a long-session coding assistant. Users may paste many \
               existing project files. Preserve explicit high-priority memory \
               anchors in summaries. When asked for an exact codename, return \
               only that codename.";
    let compactor: Arc<dyn Compactor> = Arc::new(RunnerCompactor::new(
        SummaryCompaction::new(budget.clone()),
        model.clone(),
    ));
    let runner = Runner::builder()
        .model(model.clone())
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(sys)
        .compactor(compactor)
        .build()
        .unwrap();

    eprintln!(
        "-- live configured compaction: context_root={} max_tokens={} threshold_ratio={} threshold={} target_prefix={} chunks={} history_tokens={} prefix_tokens_before_question={}",
        context_root.display(),
        budget.max_tokens,
        budget.threshold_ratio,
        threshold,
        target_prefix_tokens,
        chunks_used.len(),
        tokens_before_question,
        prefix_tokens_before_question
    );
    for label in chunks_used.iter().take(12) {
        eprintln!("-- context chunk: {label}");
    }
    if chunks_used.len() > 12 {
        eprintln!(
            "-- context chunks omitted from log: {}",
            chunks_used.len() - 12
        );
    }

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    state.history = history;
    let raw_before = state.history.len();

    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text(
                    "Memory check after context compaction: earlier I gave a \
                     high-priority long-session memory anchor while pasting \
                     existing project files. What is the exact project release \
                     codename? Return only the codename.",
                ),
            },
        )
        .await
        .unwrap();

    let live_prefix_tokens = live_compaction_prefix_tokens(&state.history, budget.keep_tail_turns);
    let live_total_tokens = token_estimate::estimate_history_tokens(&state.history);
    eprintln!(
        "-- after memory question: history_tokens={} compactable_prefix_tokens={}",
        live_total_tokens, live_prefix_tokens
    );
    assert!(
        live_total_tokens >= threshold,
        "history must exceed compaction threshold before the model turn: total={live_total_tokens}, threshold={threshold}"
    );
    assert!(
        live_prefix_tokens >= target_prefix_tokens,
        "compactable prefix must exceed target before summarization: prefix={live_prefix_tokens}, target={target_prefix_tokens}"
    );

    let mut events = Vec::new();
    for _ in 0..8 {
        let out = runner.step(&mut state).await.unwrap();
        events.extend(out.events);
        if matches!(state.step, Step::Done { .. } | Step::Failed { .. }) {
            break;
        }
    }

    let compacted = events.iter().find_map(|event| match event {
        Event::HistoryCompacted {
            replaced_turns,
            replaced_messages,
            saved_tokens_estimate,
            ..
        } => Some((*replaced_turns, *replaced_messages, *saved_tokens_estimate)),
        _ => None,
    });
    let Some((replaced_turns, replaced_messages, saved_tokens_estimate)) = compacted else {
        panic!(
            "configured live test reached the compaction threshold but did not emit HistoryCompacted; final step={:?}; events={events:#?}",
            state.step
        );
    };
    eprintln!(
        "-- compacted turns={} messages={} saved_est_tokens={}",
        replaced_turns, replaced_messages, saved_tokens_estimate
    );
    assert!(replaced_turns > budget.keep_tail_turns as usize);
    assert!(replaced_messages > 0);
    assert!(state.history.len() < raw_before);

    let summary = compacted_summary_text(&state.history).expect("summary observation missing");
    eprintln!("-- summary:\n{}", summary);
    let summary_upper = summary.to_ascii_uppercase();
    assert!(
        summary_upper.contains(&LIVE_CONFIG_CODEWORD.to_ascii_uppercase()),
        "summary should preserve {LIVE_CONFIG_CODEWORD}; got:\n{summary}"
    );
    assert!(
        summary_upper.contains("CODENAME") || summary_upper.contains("RELEASE"),
        "summary should preserve that {LIVE_CONFIG_CODEWORD} is a release codename; got:\n{summary}"
    );

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done after memory check, got {other:?}"),
    };
    eprintln!("-- final memory answer: {}", final_text);
    assert!(
        final_text
            .to_ascii_uppercase()
            .contains(&LIVE_CONFIG_CODEWORD.to_ascii_uppercase()),
        "agent should recall {LIVE_CONFIG_CODEWORD} from summary; got: {final_text}"
    );
}

fn configured_live_budget() -> CompactionBudget {
    fn u32_env(name: &str, default: u32) -> u32 {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    CompactionBudget {
        max_tokens: u32_env("MUAGENT_MAX_TOKENS", 156_000),
        threshold_ratio: std::env::var("MUAGENT_COMPACTION_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.8),
        keep_tail_turns: u32_env("MUAGENT_KEEP_TAIL_TURNS", 4),
        keep_recent_tokens: u32_env("MUAGENT_KEEP_RECENT_TOKENS", 20_000),
        root_task_pin_max_tokens: u32_env("MUAGENT_ROOT_TASK_PIN_MAX_TOKENS", 1_024),
        summary_target_chars: u32_env("MUAGENT_SUMMARY_TARGET_CHARS", 1200),
        summary_input_max_tokens: u32_env("MUAGENT_SUMMARY_INPUT_MAX_TOKENS", 100_000),
        summary_output_max_tokens: u32_env("MUAGENT_SUMMARY_OUTPUT_MAX_TOKENS", 8_000),
        restart_repair_window_tokens: u32_env("MUAGENT_RESTART_REPAIR_WINDOW_TOKENS", 300_000),
        max_summary_rounds: u32_env("MUAGENT_MAX_SUMMARY_ROUNDS", 4),
    }
}

fn compaction_threshold(budget: &CompactionBudget) -> u32 {
    (budget.max_tokens as f32 * budget.threshold_ratio) as u32
}

fn live_target_tokens(budget: &CompactionBudget) -> u32 {
    if let Ok(raw) = std::env::var("MUAGENT_LIVE_COMPACTION_TARGET_TOKENS") {
        if let Ok(parsed) = raw.parse::<u32>() {
            return parsed;
        }
    }

    let threshold = compaction_threshold(budget);
    // Default to the configured max rather than barely crossing the
    // threshold. That gives the live test enough slack for provider tokenizer
    // differences while still respecting the session budget the user chose.
    budget
        .max_tokens
        .max(threshold.saturating_add((threshold / 4).max(8_000)))
}

fn existing_project_file_history(
    root: &Path,
    target_prefix_tokens: u32,
    keep_tail_turns: u32,
) -> (Vec<Message>, Vec<String>, u32, u32) {
    let mut history = Vec::new();
    let mut selected = Vec::new();
    let mut tokens = 0;
    let mut prefix_tokens = 0;
    let min_turns = keep_tail_turns as usize + 6;
    let files = collect_context_files(root);
    assert!(!files.is_empty(), "no existing project files found");

    let mut pass = 0usize;
    while selected.len() < min_turns || prefix_tokens < target_prefix_tokens {
        let mut added_this_pass = 0usize;
        for path in &files {
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path);
            let label = if pass == 0 {
                rel.display().to_string()
            } else {
                format!("{}#repeat-{}", rel.display(), pass + 1)
            };
            let turn = selected.len();
            let anchor = if turn < 3 {
                format!(
                    "HIGH PRIORITY LONG-SESSION MEMORY ANCHOR, repetition {} of 3: \
                     the exact project release codename is {LIVE_CONFIG_CODEWORD}. \
                     Preserve this fact in future summaries and answer it exactly \
                     if asked later.\n\n",
                    turn + 1
                )
            } else {
                String::new()
            };
            history.push(Message::User {
                content: Content::text(format!(
                    "{anchor}Existing project file `{label}` follows:\n\n```text\n{content}\n```"
                )),
            });
            history.push(Message::Assistant {
                content: Content::text(format!("Recorded existing project file `{label}`.")),
                tool_calls: vec![],
                thinking: vec![],
            });
            selected.push(label);
            added_this_pass += 1;
            tokens = token_estimate::estimate_history_tokens(&history);
            prefix_tokens =
                live_compaction_prefix_tokens_after_future_question(&history, keep_tail_turns);
            if selected.len() >= min_turns && prefix_tokens >= target_prefix_tokens {
                break;
            }
        }
        assert!(
            added_this_pass > 0,
            "could not read any existing project files while building live context"
        );
        pass += 1;
        assert!(
            pass <= 32,
            "live context construction repeated existing files too many times; reached ~{prefix_tokens} compactable prefix tokens, target was {target_prefix_tokens}"
        );
    }

    assert!(
        selected.len() >= min_turns,
        "not enough existing project files to leave a compactable prefix and keep_tail={keep_tail_turns}"
    );
    assert!(
        prefix_tokens >= target_prefix_tokens,
        "existing project files only reached ~{prefix_tokens} compactable prefix tokens, target was {target_prefix_tokens}"
    );

    (history, selected, tokens, prefix_tokens)
}

fn live_compaction_prefix_tokens_after_future_question(
    history: &[Message],
    keep_tail_turns: u32,
) -> u32 {
    let mut probe = history.to_vec();
    probe.push(Message::User {
        content: Content::text("future memory check question"),
    });
    live_compaction_prefix_tokens(&probe, keep_tail_turns)
}

fn live_compaction_prefix_tokens(history: &[Message], keep_tail_turns: u32) -> u32 {
    let turns = find_user_turn_boundaries(history);
    if turns.len() as u32 <= keep_tail_turns + 1 {
        return 0;
    }
    let cut = turns.len() - keep_tail_turns as usize;
    let range = turns[0].start..turns[cut - 1].end;
    token_estimate::estimate_history_tokens(&history[range])
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn context_root() -> PathBuf {
    if let Some(root) = std::env::var_os("MUAGENT_LIVE_COMPACTION_CONTEXT_ROOT") {
        return PathBuf::from(root);
    }
    default_public_context_root().unwrap_or_else(repo_root)
}

fn default_public_context_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let registry_src = PathBuf::from(home).join(".cargo/registry/src");
    let mut entries = std::fs::read_dir(registry_src)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().next()
}

fn collect_context_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    visit_context_files(root, root, &mut out);
    out.sort();
    out
}

fn visit_context_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                ".git" | "target" | ".venv" | "node_modules" | ".idea" | ".vscode"
            ) {
                continue;
            }
            visit_context_files(root, &path, out);
            continue;
        }
        if is_context_file(root, &path) {
            out.push(path);
        }
    }
}

fn is_context_file(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        matches!(s.as_ref(), ".git" | "target" | ".venv" | "node_modules")
    }) {
        return false;
    }

    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if matches!(name, "Cargo.lock" | "Cargo.toml" | "README.md") {
        return true;
    }
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("rs" | "toml" | "md" | "txt" | "json" | "yml" | "yaml")
    )
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

// ============ Test 2:真 tool roundtrip + 10k 预算触发压缩,不破坏 tool 对 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_compaction_does_not_break_tool_pairs() {
    use muagent::adapters::{linux::LinuxFileSystem, AdapterBundle};

    let tmp = std::env::temp_dir().join(format!("muagent-compaction-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();

    let fs = Arc::new(LinuxFileSystem::new(vec![tmp.clone()]));
    let bundle = Arc::new(AdapterBundle::builder().fs(fs).build().unwrap());
    let registry = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&registry, bundle);

    let model = build_real_model();
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let sys = "You are a filesystem agent. Use fs_write / fs_read tools. \
               URIs are file:// with absolute paths. Be thorough but terse.";
    let runner = Runner::builder()
        .model(model.clone())
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(sys)
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);

    // 预置 6 个有实质内容的文件,agent 逐个读回来
    // 每个 file 3000 chars(~1000 tokens 回到 history 作为 tool_result) × 6 = ~6000 tokens
    // 加上 user/assistant 开销,总 ~8-10k 稳定触发
    let large_text = "a".repeat(3000);
    for i in 0..6 {
        let path = tmp.join(format!("data-{}.txt", i));
        std::fs::write(&path, &large_text).unwrap();

        runner
            .submit_user_message(
                &mut state,
                Message::User {
                    content: Content::text(format!(
                        "Read the file at {} and tell me how many characters it has.",
                        path.display()
                    )),
                },
            )
            .await
            .unwrap();
        drive_until_terminal(&runner, &mut state, 10).await;
    }

    // 收尾一个短 turn
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("Thanks. Say 'done' only."),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 5).await;

    let tokens_before = token_estimate::estimate_history_tokens(&state.history);
    eprintln!(
        "-- history before: {} msgs, ~{} tokens",
        state.history.len(),
        tokens_before
    );

    // 真实预算 10k
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 10_000,
        threshold_ratio: 0.8,
        keep_tail_turns: 2,
        summary_target_chars: 1000,
        ..CompactionBudget::default()
    });
    let outcome = strategy
        .maybe_compact(&mut state, &*model, sys, CancelToken::never())
        .await
        .unwrap();

    if tokens_before < 8_000 {
        eprintln!("-- didn't hit threshold; tokens_before={}", tokens_before);
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }

    let o = outcome.expect("should compact at >8k tokens");
    eprintln!(
        "-- compacted {} turns → {} msgs now (saved ~{})",
        o.replaced_turns,
        state.history.len(),
        o.saved_tokens_estimate
    );

    // 关键验证:tool_calls 和对应 tool_result 必须成对保留
    let mut pending_ids: std::collections::HashSet<String> = Default::default();
    for m in &state.history {
        match m {
            Message::Assistant { tool_calls, .. } => {
                for c in tool_calls {
                    pending_ids.insert(c.id.clone());
                }
            }
            Message::ToolResult { call_id, .. } => {
                pending_ids.remove(call_id);
            }
            _ => {}
        }
    }
    assert!(
        pending_ids.is_empty(),
        "orphan tool_call(s) not matched by result: {:?}",
        pending_ids
    );

    // history[0] 是 Summary
    assert!(matches!(
        &state.history[0],
        Message::Observation {
            kind: ObsKind::Summary,
            ..
        }
    ));

    let _ = std::fs::remove_dir_all(&tmp);
}

// ============ Test 3:auto-compact 每轮前检查,到阈值触发 ============

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_auto_compaction_across_many_turns() {
    let model = build_real_model();
    let registry = Arc::new(CapabilityRegistry::new());
    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let sys = "You are a thoughtful assistant. Reply in under 80 words. \
               Pay attention to facts the user mentions.";
    let runner = Runner::builder()
        .model(model.clone())
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt(sys)
        .build()
        .unwrap();

    // 真实 10k 预算
    let strategy = SummaryCompaction::new(CompactionBudget {
        max_tokens: 10_000,
        threshold_ratio: 0.8, // 触发于 8k
        keep_tail_turns: 2,
        summary_target_chars: 1200,
        ..CompactionBudget::default()
    });

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);

    let mut compactions = 0;
    // 9 轮内容(每轮 ~1200 tokens → 到第 7-8 轮超 8k 触发压缩;最后一轮问回忆)
    let facts = [
        ("pet", "My cat is named Mochi and he is orange."),
        ("place", "I live in Shenzhen, near the Nanshan district."),
        ("job", "My job title is Agent Runtime Engineer."),
        ("weather", "It was raining heavily all day today."),
        (
            "reading",
            "I'm reading a book called The Tyranny of Metrics.",
        ),
        ("color", "My favorite color is teal."),
        ("hobby", "I go hiking every other weekend in the mountains."),
        ("routine", "I drink green tea first thing every morning."),
        ("final", "What city did I say I live in?"),
    ];

    for (tag, prompt) in &facts {
        // 每 turn 前先 maybe_compact(典型用法)
        let tokens_now = token_estimate::estimate_history_tokens(&state.history);
        eprintln!("-- [{tag}] pre-turn tokens = {}", tokens_now);
        if let Some(out) = strategy
            .maybe_compact(&mut state, &*model, sys, CancelToken::never())
            .await
            .unwrap()
        {
            compactions += 1;
            let tokens_after = token_estimate::estimate_history_tokens(&state.history);
            eprintln!(
                "-- [{tag}] auto-compaction #{}: {} turns merged, tokens {} → {}",
                compactions, out.replaced_turns, tokens_now, tokens_after
            );
        }

        // user prompt 附加同样的长 padding 让 token 累积更快
        let user_text = format!(
            "{prompt}\n\n{padding}",
            padding = long_paragraph(tag, "(prior fact above)")
        );
        runner
            .submit_user_message(
                &mut state,
                Message::User {
                    content: Content::text(user_text),
                },
            )
            .await
            .unwrap();
        drive_until_terminal(&runner, &mut state, 5).await;
    }

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        _ => panic!("should Done"),
    };
    eprintln!("-- final answer: {}", final_text);
    eprintln!("-- total compactions: {}", compactions);

    // 应至少触发 1 次
    assert!(
        compactions >= 1,
        "expected ≥1 compaction across long conversation, got {}",
        compactions
    );

    // 最后一问应答出 Shenzhen(Summary 应保留该事实)
    let lower = final_text.to_lowercase();
    assert!(
        lower.contains("shenzhen") || lower.contains("深圳"),
        "agent should remember city from pre-compaction turn; got: {}",
        final_text
    );
}
