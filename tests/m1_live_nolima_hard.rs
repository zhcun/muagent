//! Live NoLiMa-Hard diagnostic.
//!
//! NoLiMa is designed to avoid literal matching: the needle and question have
//! minimal lexical overlap, so models must infer a latent association inside a
//! long haystack. This makes it a better non-saturated long-context probe than
//! simple exact-anchor retrieval.
//!
//! Data source:
//! - https://huggingface.co/datasets/amodaresi/NoLiMa
//! - Download with `bench/context_cases/download_nolima.sh`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use muagent::adapters::ReqwestEgress;
use muagent::core::cache::CachePolicy;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::thinking::{ThinkingConfig, ThinkingEffort};
use muagent::core::types::{Content, Message};
use muagent::providers::OpenAiAdapter;
use muagent::runtime::token_estimate;
use serde_json::Value;

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

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn nolima_root() -> PathBuf {
    std::env::var_os("MUAGENT_NOLIMA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("bench/context_cases/data/nolima"))
}

fn u32_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

fn usize_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn bool_env(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(default)
}

fn split_segments(text: &str, segments: usize) -> Vec<String> {
    let segments = segments.max(1);
    let chars: Vec<char> = text.chars().collect();
    let chunk_len = chars.len().div_ceil(segments).max(1);
    chars
        .chunks(chunk_len)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn thinking_from_env() -> ThinkingConfig {
    match std::env::var("MUAGENT_NOLIMA_THINKING")
        .unwrap_or_else(|_| "off".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "minimal" => ThinkingConfig::enabled_effort(ThinkingEffort::Minimal),
        "low" => ThinkingConfig::enabled_effort(ThinkingEffort::Low),
        "medium" => ThinkingConfig::enabled_effort(ThinkingEffort::Medium),
        "high" => ThinkingConfig::enabled_effort(ThinkingEffort::High),
        "max" | "xhigh" => ThinkingConfig::enabled_effort(ThinkingEffort::Max),
        "auto" => ThinkingConfig::auto(),
        _ => ThinkingConfig::off(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    Baseline,
    StepBack,
    Bridge,
    Extract,
    HydeExtract,
    GuardedHyde,
    MarginGuarded,
    GuidedMargin,
    StatefulRead,
    ChunkVote,
    StructuredMemory,
    VerifiedMargin,
    HybridMemory,
    Compare,
}

impl Strategy {
    fn from_env() -> Self {
        match std::env::var("MUAGENT_NOLIMA_STRATEGY")
            .unwrap_or_else(|_| "baseline".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "stepback" | "step_back" => Strategy::StepBack,
            "bridge" => Strategy::Bridge,
            "extract" | "evidence" => Strategy::Extract,
            "hyde" | "hyde_extract" => Strategy::HydeExtract,
            "guarded" | "guarded_hyde" | "safe_hyde" => Strategy::GuardedHyde,
            "margin" | "margins" | "wim" | "chunked" | "chunked_margin" => Strategy::MarginGuarded,
            "guided_margin" | "guided_margins" | "hyde_margin" | "card_margin" => {
                Strategy::GuidedMargin
            }
            "stateful" | "stateful_read" | "stream" | "streaming" | "streaming_memory" => {
                Strategy::StatefulRead
            }
            "vote" | "chunk_vote" | "chunkvote" | "map_reduce" | "mapreduce" => Strategy::ChunkVote,
            "structured" | "structured_memory" | "cosmir" | "memory_table" => {
                Strategy::StructuredMemory
            }
            "verified_margin" | "verify_margin" | "margin_verify" | "evidence_verify" => {
                Strategy::VerifiedMargin
            }
            "hybrid" | "hybrid_memory" | "structured_margin" | "memory_margin" => {
                Strategy::HybridMemory
            }
            "compare" => Strategy::Compare,
            _ => Strategy::Baseline,
        }
    }
}

#[derive(Clone, Debug)]
struct EvalItem {
    case_index: usize,
    case_id: String,
    test_id: String,
    args: Vec<String>,
    question_kind: String,
    question_template: String,
}

fn load_hard_cases() -> Vec<Value> {
    let path = nolima_root().join("needlesets/needle_set_hard.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing NoLiMa hard needle set at {}. Run `bench/context_cases/download_nolima.sh` first.",
            path.display()
        )
    });
    serde_json::from_str::<Vec<Value>>(&text).expect("NoLiMa hard JSON")
}

fn load_books(count: usize) -> Vec<(usize, String)> {
    let count = count.clamp(1, 5);
    (1..=count)
        .map(|book_id| {
            let path = nolima_root().join(format!("haystack/rand_shuffle/rand_book_{book_id}.txt"));
            let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                panic!(
                    "missing NoLiMa haystack at {}. Run `bench/context_cases/download_nolima.sh` first.",
                    path.display()
                )
            });
            (book_id, text)
        })
        .collect()
}

fn case_id(case: &Value) -> &str {
    case.get("id").and_then(Value::as_str).unwrap_or("unknown")
}

fn fill_template(template: &str, character: &str, args: &[String]) -> String {
    let mut out = template.replace("{CHAR}", character);
    for (idx, arg) in args.iter().enumerate() {
        out = out.replace(&format!("{{{}}}", idx + 1), arg);
    }
    out
}

fn expand_items(cases: &[Value]) -> Vec<EvalItem> {
    let mut items = Vec::new();
    for (case_index, case) in cases.iter().enumerate() {
        let tests = case
            .get("tests")
            .and_then(Value::as_object)
            .expect("case tests object");
        let questions = case
            .get("questions")
            .and_then(Value::as_object)
            .expect("questions object");

        for (test_id, test) in tests {
            let args: Vec<String> = test
                .get("input_args")
                .and_then(Value::as_array)
                .expect("input_args")
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect();
            for question_kind in ["twohop2", "twohop", "onehop"] {
                if let Some(question_template) =
                    questions.get(question_kind).and_then(Value::as_str)
                {
                    items.push(EvalItem {
                        case_index,
                        case_id: case_id(case).to_string(),
                        test_id: test_id.clone(),
                        args: args.clone(),
                        question_kind: question_kind.to_string(),
                        question_template: question_template.to_string(),
                    });
                }
            }
        }
    }
    items
}

fn build_haystack(book: &str, needle: &str, target_tokens: u32, depth: f32) -> String {
    let target_chars = (target_tokens as usize).saturating_mul(3);
    let chars: Vec<char> = book.chars().collect();
    let slice_len = target_chars.min(chars.len());
    let insert_idx = ((slice_len as f32) * depth.clamp(0.05, 0.95)).round() as usize;
    let prefix: String = chars.iter().take(insert_idx).collect();
    let suffix: String = chars
        .iter()
        .skip(insert_idx)
        .take(slice_len - insert_idx)
        .collect();
    format!("{prefix}\n{needle}\n{suffix}")
}

fn normalize_answer(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn looks_empty_or_unknown_answer(answer: &str) -> bool {
    let normalized = normalize_answer(answer);
    normalized.is_empty()
        || normalized == "none"
        || normalized == "unknown"
        || normalized == "noanswer"
        || normalized.contains("nonefound")
        || normalized.contains("cannotdetermine")
}

fn evidence_has_usable_snippet(evidence: &str) -> bool {
    let lower = evidence.to_ascii_lowercase();
    if evidence.trim().is_empty()
        || lower.contains("none found")
        || lower.contains("no verbatim")
        || lower.contains("no relevant")
    {
        return false;
    }

    evidence.lines().any(|line| {
        let line = line.trim();
        if !line.starts_with('-') || line.to_ascii_lowercase().contains("none") {
            return false;
        }
        line.split_whitespace().any(|word| {
            let word = word.trim_matches(|c: char| !c.is_ascii_alphabetic());
            word.chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
        })
    })
}

fn line_has_support_relation(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        " next to ",
        " where ",
        " lives",
        " living ",
        " visited",
        " visit ",
        " been to",
        " went to",
        " traveled",
        " travelled",
        " saw ",
        " seen ",
        " painting",
        " museum",
        " institution",
        " city",
        " country",
        " engineer",
        " named ",
    ]
    .iter()
    .any(|cue| lower.contains(cue))
}

fn line_has_answer_support_relation(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        " next to ",
        " where ",
        " lives",
        " living ",
        " visited",
        " visit ",
        " been to",
        " went to",
        " traveled",
        " travelled",
        " saw ",
        " seen ",
        " painting",
        " museum",
        " engineer",
        " named ",
    ]
    .iter()
    .any(|cue| lower.contains(cue))
}

fn evidence_supports_answer(evidence: &str, answer: &str) -> bool {
    let normalized_answer = normalize_answer(answer);
    if normalized_answer.is_empty() {
        return false;
    }

    evidence.lines().any(|line| {
        normalize_answer(line).contains(&normalized_answer)
            && line_has_answer_support_relation(line)
    })
}

fn support_only_evidence(evidence: &str) -> String {
    let mut filtered = String::new();
    for line in evidence.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('-') && line_has_support_relation(trimmed) {
            filtered.push_str(trimmed);
            filtered.push('\n');
        }
    }
    if filtered.trim().is_empty() {
        evidence.to_string()
    } else {
        filtered
    }
}

fn choose_guarded_answer(baseline_answer: &str, evidence: &str, extracted_answer: &str) -> String {
    if !looks_empty_or_unknown_answer(extracted_answer)
        && evidence_has_usable_snippet(evidence)
        && evidence_supports_answer(evidence, extracted_answer)
    {
        extracted_answer.to_string()
    } else {
        baseline_answer.to_string()
    }
}

async fn ask_model(
    model: &dyn ModelAdapter,
    prompt: String,
    cache_key: String,
    thinking: ThinkingConfig,
) -> String {
    let reply = model
        .turn(
            ModelRequest {
                system: String::new(),
                runtime_context: String::new(),
                messages: vec![Message::User {
                    content: Content::text(prompt),
                }],
                tools: vec![],
                temperature: Some(0.0),
                stream: false,
                cache: CachePolicy::Auto,
                thinking,
                prompt_cache_key: Some(cache_key),
            },
            CancelToken::never(),
        )
        .await
        .expect("live model turn");
    reply.text
}

async fn bridge_terms(model: &dyn ModelAdapter, question: &str, cache_key: String) -> String {
    let prompt = format!(
        "Generate compact world-knowledge bridge terms for this long-context \
         retrieval question. The terms should help find an indirect sentence \
         in a book snippet. Do not answer the question. Do not invent or use \
         character names. Return only a comma-separated list of places, \
         landmarks, institutions, foods, paintings, or concepts that could \
         imply the target.\n\nQuestion: {question}"
    );
    ask_model(model, prompt, cache_key, ThinkingConfig::off()).await
}

async fn extract_relevant_evidence(
    model: &dyn ModelAdapter,
    haystack: &str,
    question: &str,
    guidance: &str,
    cache_key: String,
) -> String {
    let prompt = format!(
        "You are an evidence extractor for a long-context question. Extract \
         only short verbatim snippets from the book snippet that mention a \
         named character and may answer the question through world knowledge \
         or commonsense. The relevant sentence may not share words with the \
         question. Prefer standalone facts about a person's location, travel, \
         diet, restriction, museum or painting visit, institution, city, \
         country, or role. Ignore narrative distractors unless they directly \
         connect a named character to such a fact.\n\n\
         Return at most 12 bullets. Each bullet must be copied from the book \
         snippet as verbatim text. Do not answer the question.\n\n\
         Question: {question}\n\n\
         Question-only search guidance (not evidence; use only to recognize \
         indirect matches):\n{guidance}\n\n\
         Book snippet:\n{haystack}"
    );
    ask_model(model, prompt, cache_key, thinking_from_env()).await
}

async fn hypothetical_evidence_patterns(
    model: &dyn ModelAdapter,
    question: &str,
    cache_key: String,
) -> String {
    let prompt = format!(
        "Generate compact hypothetical evidence patterns for this question. \
         The answer will be a character name hidden in a long book snippet. \
         Do not answer and do not use character names. Write 6-10 short \
         patterns of evidence sentences that could imply the answer, using \
         world knowledge. Include famous museums, artworks, institutions, \
         cities, countries, foods, diets, or roles when relevant.\n\n\
         Example style: \"someone saw the original <artwork> painting\", \
         \"someone lives near <institution>\", \"there was an engineer living \
         in <city>\".\n\n\
         Question: {question}"
    );
    ask_model(model, prompt, cache_key, ThinkingConfig::off()).await
}

async fn extract_margin_evidence(
    model: &dyn ModelAdapter,
    haystack: &str,
    question: &str,
    guidance: &str,
    cache_key: String,
) -> String {
    let chunk_count = usize_env("MUAGENT_NOLIMA_MARGIN_CHUNKS", 6);
    let mut evidence = String::new();
    for (idx, segment) in split_segments(haystack, chunk_count)
        .into_iter()
        .enumerate()
    {
        let prompt = format!(
            "You are writing margin notes for one segment of a long-context \
             retrieval task. Extract only short verbatim snippets from this \
             segment that mention a named character and a concrete clue that \
             could answer the question indirectly through world knowledge. \
             Useful clues include artworks, museums, institutions, cities, \
             countries, roles, living in a place, being next to a place, or \
             travel/visit facts.\n\n\
             Return at most 4 bullets copied verbatim from this segment. If \
             the segment has no plausible clue, return exactly '- None'. Do \
             not answer the question and do not use facts from outside this \
             segment.\n\n\
             Question-only search guidance (not evidence; use only to \
             recognize indirect matches):\n{guidance}\n\n\
             Question: {question}\n\n\
             Segment {}/{}:\n{}",
            idx + 1,
            chunk_count,
            segment
        );
        let margin = ask_model(
            model,
            prompt,
            format!("{cache_key}-margin-{idx}"),
            thinking_from_env(),
        )
        .await;
        if evidence_has_usable_snippet(&margin) {
            evidence.push_str(&margin);
            if !evidence.ends_with('\n') {
                evidence.push('\n');
            }
        }
    }
    if evidence.trim().is_empty() {
        "- None".to_string()
    } else {
        evidence
    }
}

async fn build_stateful_evidence(
    model: &dyn ModelAdapter,
    haystack: &str,
    question: &str,
    cache_key: String,
) -> String {
    let chunk_count = usize_env("MUAGENT_NOLIMA_MARGIN_CHUNKS", 4);
    let mut state = "- None".to_string();
    for (idx, segment) in split_segments(haystack, chunk_count)
        .into_iter()
        .enumerate()
    {
        let prompt = format!(
            "You are reading a long context sequentially and maintaining a \
             tiny evidence ledger for the final answer. Do not answer yet.\n\n\
             Question: {question}\n\n\
             Current ledger:\n{state}\n\n\
             New segment {}/{}:\n{}\n\n\
             Update the ledger using only the current ledger and the new \
             segment. Keep at most 3 candidate bullets. A valid candidate \
             must contain one named character and one concrete clue in the \
             same evidence sentence. The clue must plausibly imply the \
             question target through ordinary world knowledge. Prefer \
             candidates with an explicit relation such as lives near, named, \
             visited, saw an original painting, museum, institution, city, \
             or country. Drop weak or unrelated candidates even if they \
             contain names. Copy the evidence sentence verbatim inside each \
             bullet. If no valid candidate exists, return '- None'.\n\n\
             Output format only:\n\
             - character=<name> | evidence=\"<verbatim sentence>\" | bridge=<short bridge>\n",
            idx + 1,
            chunk_count,
            segment
        );
        state = ask_model(
            model,
            prompt,
            format!("{cache_key}-state-{idx}"),
            thinking_from_env(),
        )
        .await;
    }
    state
}

async fn build_chunk_vote_evidence(
    model: &dyn ModelAdapter,
    haystack: &str,
    question: &str,
    cache_key: String,
) -> String {
    let chunk_count = usize_env("MUAGENT_NOLIMA_MARGIN_CHUNKS", 4);
    let mut evidence = String::new();
    for (idx, segment) in split_segments(haystack, chunk_count)
        .into_iter()
        .enumerate()
    {
        let prompt = format!(
            "You are one worker reading one segment of a long-context question. \
             Decide whether this segment alone contains a candidate answer.\n\n\
             Question: {question}\n\n\
             Segment {}/{}:\n{}\n\n\
             If this segment contains a sentence that can answer the question \
             through ordinary world knowledge, output exactly one candidate \
             bullet. The bullet must bind one named character to one quoted \
             verbatim evidence sentence from this segment. The evidence must \
             contain the character and the concrete clue in the same sentence \
             or adjacent clause. If there is no such candidate, output exactly \
             '- None'. Do not use facts from other segments. Do not answer \
             from narrative vibes.\n\n\
             Output format:\n\
             - character=<name> | evidence=\"<verbatim sentence>\" | bridge=<why this clue implies the question target>\n",
            idx + 1,
            chunk_count,
            segment
        );
        let candidate = ask_model(
            model,
            prompt,
            format!("{cache_key}-vote-{idx}"),
            thinking_from_env(),
        )
        .await;
        if evidence_has_usable_snippet(&candidate) {
            evidence.push_str(&candidate);
            if !evidence.ends_with('\n') {
                evidence.push('\n');
            }
        }
    }
    if evidence.trim().is_empty() {
        "- None".to_string()
    } else {
        evidence
    }
}

async fn build_structured_memory_evidence(
    model: &dyn ModelAdapter,
    haystack: &str,
    question: &str,
    cache_key: String,
) -> String {
    let chunk_count = usize_env("MUAGENT_NOLIMA_MARGIN_CHUNKS", 4);
    let mut evidence = String::new();
    for (idx, segment) in split_segments(haystack, chunk_count)
        .into_iter()
        .enumerate()
    {
        let prompt = format!(
            "You are writing a structured memory table for one segment of a \
             long-context question. The answer is usually implied by an \
             indirect clue, not by repeating the question words. Use a fixed \
             Extract -> Infer -> Refine cycle, but output only the final \
             refined rows.\n\n\
             Question: {question}\n\n\
             Segment {}/{}:\n{}\n\n\
             Extract: find short verbatim sentences or adjacent clauses in \
             this segment that mention a named character plus a concrete \
             clue. Useful indirect clues include seeing an original painting, \
             visiting or being near a museum/institution/city/country, living \
             in or next to a place, having a named role, food/diet constraints, \
             or other facts that ordinary world knowledge can map to the \
             question. Infer: explain the bridge from that clue to the \
             question target. Refine: keep plausible rows even when the \
             target place/entity is not named literally in the evidence.\n\n\
             Return at most 3 rows. If there is no plausible row, return \
             exactly '- None'. Do not use facts from other segments. Do not \
             answer from narrative vibes.\n\n\
             Output format only:\n\
             - character=<name> | clue=\"<verbatim evidence>\" | bridge=<short bridge> | target=<question condition>\n",
            idx + 1,
            chunk_count,
            segment
        );
        let rows = ask_model(
            model,
            prompt,
            format!("{cache_key}-structured-{idx}"),
            thinking_from_env(),
        )
        .await;
        if evidence_has_usable_snippet(&rows) {
            evidence.push_str(&rows);
            if !evidence.ends_with('\n') {
                evidence.push('\n');
            }
        }
    }
    if evidence.trim().is_empty() {
        "- None".to_string()
    } else {
        evidence
    }
}

async fn answer_from_evidence(
    model: &dyn ModelAdapter,
    question: &str,
    evidence: &str,
    cache_key: String,
) -> String {
    let prompt = format!(
        "Answer the question using only the extracted evidence below plus \
         ordinary world knowledge needed to connect places, institutions, \
         artworks, foods, or constraints. If multiple character names appear, \
         choose the one whose evidence implies the question target. Return \
         only the final character name.\n\n\
         Question: {question}\n\n\
         Extracted evidence:\n{evidence}"
    );
    ask_model(model, prompt, cache_key, thinking_from_env()).await
}

async fn verify_answer_from_evidence(
    model: &dyn ModelAdapter,
    question: &str,
    evidence: &str,
    candidate_answer: &str,
    cache_key: String,
) -> String {
    let prompt = format!(
        "Verify a candidate answer using only the extracted evidence below \
         plus ordinary world knowledge needed to connect places, institutions, \
         artworks, foods, or constraints. The evidence must support the \
         candidate character, not merely mention a related topic. If supported, \
         return only the candidate character name. If not supported, return \
         exactly 'None'.\n\n\
         Question: {question}\n\n\
         Candidate answer: {candidate_answer}\n\n\
         Extracted evidence:\n{evidence}"
    );
    ask_model(model, prompt, cache_key, thinking_from_env()).await
}

fn apply_strategy_prompt(
    base_prompt: &str,
    question: &str,
    strategy: Strategy,
    bridge: &str,
) -> String {
    match strategy {
        Strategy::Baseline
        | Strategy::Extract
        | Strategy::HydeExtract
        | Strategy::GuardedHyde
        | Strategy::MarginGuarded
        | Strategy::GuidedMargin
        | Strategy::StatefulRead
        | Strategy::ChunkVote
        | Strategy::StructuredMemory
        | Strategy::VerifiedMargin
        | Strategy::HybridMemory
        | Strategy::Compare => base_prompt.to_string(),
        Strategy::StepBack => format!(
            "Answer the long-context question using latent association, not \
             literal keyword matching. First identify which kind of hidden \
             fact would imply the answer, then search the snippet for a \
             character tied to a related entity/place/constraint. Use world \
             knowledge only to bridge from the question to the relevant \
             snippet sentence. Ignore unrelated character names. Think \
             internally; output only the final character name.\n\n{base_prompt}"
        ),
        Strategy::Bridge => format!(
            "Search guidance generated from the question only. These are not \
             evidence and may be incomplete; use them only as bridge terms \
             for locating relevant snippet sentences.\n\
             Question: {question}\n\
             Bridge terms: {bridge}\n\n\
             When answering, rely on the book snippet as evidence and output \
             only the final character name.\n\n{base_prompt}"
        ),
    }
}

#[ignore = "hits real OpenRouter API and sends NoLiMa-Hard long-context cases"]
#[tokio::test]
async fn live_nolima_hard_raw_context_is_not_saturated() {
    let model = build_model();
    let cases = load_hard_cases();
    let items = expand_items(&cases);
    let book_count = usize_env("MUAGENT_NOLIMA_BOOK_COUNT", 1);
    let books = load_books(book_count);
    let target_tokens = u32_env("MUAGENT_NOLIMA_CONTEXT_TOKENS", 48_000);
    let total_items = items.len().saturating_mul(books.len());
    let limit = usize_env("MUAGENT_NOLIMA_CASE_LIMIT", 6).min(total_items);
    let require_nonsaturated = bool_env("MUAGENT_NOLIMA_REQUIRE_NONSATURATED", false);
    let strategy = Strategy::from_env();
    let thinking = thinking_from_env();

    eprintln!(
        "-- NoLiMa-Hard diagnostic: cases={} base_items={} books={} target_context_tokens={} strategy={:?} thinking={:?}",
        limit,
        items.len(),
        books.len(),
        target_tokens,
        strategy,
        thinking
    );

    let mut correct = 0usize;
    let mut correct_baseline = 0usize;
    let mut correct_candidate = 0usize;
    for idx in 0..limit {
        let item = &items[idx % items.len()];
        let (book_id, book) = &books[(idx / items.len()) % books.len()];
        let case = &cases[item.case_index];
        let chars = case
            .get("character_set")
            .and_then(Value::as_array)
            .expect("character_set");
        let expected = chars
            .get(idx % chars.len())
            .and_then(Value::as_str)
            .expect("character");
        let needle_template = case.get("needle").and_then(Value::as_str).expect("needle");
        let task_template = case
            .get("task_template")
            .and_then(Value::as_str)
            .expect("task_template");

        let needle = fill_template(needle_template, expected, &item.args);
        let question = fill_template(&item.question_template, expected, &item.args);
        let depth = 0.15 + 0.7 * ((idx % limit.max(1)) as f32 / limit.max(1) as f32);
        let haystack = build_haystack(book, &needle, target_tokens, depth);
        let prompt = task_template
            .replace("{haystack}", &haystack)
            .replace("{question}", &question);
        let estimated_tokens = token_estimate::estimate_text_tokens(&prompt);
        let cache_case_key = format!(
            "{}-{}-{}-book{}-idx{}",
            item.case_id, item.test_id, item.question_kind, book_id, idx
        );
        let answer = ask_model(
            &*model,
            prompt.clone(),
            format!("nolima-hard-baseline-{cache_case_key}-{target_tokens}"),
            thinking.clone(),
        )
        .await;
        let ok = normalize_answer(&answer).contains(&normalize_answer(expected));
        if ok {
            correct += 1;
            correct_baseline += 1;
        }
        eprintln!(
            "-- case {idx}: book={} id={} test={} q={} depth={:.2} prompt_est_tokens={} expected={} baseline_answer={:?} baseline_ok={}",
            book_id,
            item.case_id,
            item.test_id,
            item.question_kind,
            depth,
            estimated_tokens,
            expected,
            answer,
            ok
        );

        if matches!(
            strategy,
            Strategy::StepBack
                | Strategy::Bridge
                | Strategy::Extract
                | Strategy::HydeExtract
                | Strategy::GuardedHyde
                | Strategy::MarginGuarded
                | Strategy::GuidedMargin
                | Strategy::StatefulRead
                | Strategy::ChunkVote
                | Strategy::StructuredMemory
                | Strategy::VerifiedMargin
                | Strategy::HybridMemory
                | Strategy::Compare
        ) {
            let candidate_strategy = match strategy {
                Strategy::StepBack => Strategy::StepBack,
                Strategy::Bridge => Strategy::Bridge,
                Strategy::Extract => Strategy::Extract,
                Strategy::HydeExtract | Strategy::Compare => Strategy::HydeExtract,
                Strategy::GuardedHyde => Strategy::GuardedHyde,
                Strategy::MarginGuarded => Strategy::MarginGuarded,
                Strategy::GuidedMargin => Strategy::GuidedMargin,
                Strategy::StatefulRead => Strategy::StatefulRead,
                Strategy::ChunkVote => Strategy::ChunkVote,
                Strategy::StructuredMemory => Strategy::StructuredMemory,
                Strategy::VerifiedMargin => Strategy::VerifiedMargin,
                Strategy::HybridMemory => Strategy::HybridMemory,
                Strategy::Baseline => Strategy::Baseline,
            };
            let mut bridge = String::new();
            let mut evidence = String::new();
            let candidate_answer = if matches!(
                candidate_strategy,
                Strategy::Extract
                    | Strategy::HydeExtract
                    | Strategy::GuardedHyde
                    | Strategy::MarginGuarded
                    | Strategy::GuidedMargin
                    | Strategy::StatefulRead
                    | Strategy::ChunkVote
                    | Strategy::StructuredMemory
                    | Strategy::VerifiedMargin
                    | Strategy::HybridMemory
            ) {
                let guidance = if matches!(
                    candidate_strategy,
                    Strategy::MarginGuarded
                        | Strategy::StatefulRead
                        | Strategy::ChunkVote
                        | Strategy::StructuredMemory
                        | Strategy::VerifiedMargin
                        | Strategy::HybridMemory
                ) {
                    ""
                } else if matches!(
                    candidate_strategy,
                    Strategy::HydeExtract | Strategy::GuardedHyde | Strategy::GuidedMargin
                ) {
                    bridge = hypothetical_evidence_patterns(
                        &*model,
                        &question,
                        format!(
                            "nolima-hard-{:?}-patterns-{cache_case_key}-{target_tokens}",
                            candidate_strategy
                        ),
                    )
                    .await;
                    bridge.as_str()
                } else {
                    ""
                };
                evidence = if matches!(candidate_strategy, Strategy::StatefulRead) {
                    build_stateful_evidence(
                        &*model,
                        &haystack,
                        &question,
                        format!("nolima-hard-stateful-evidence-{cache_case_key}-{target_tokens}"),
                    )
                    .await
                } else if matches!(candidate_strategy, Strategy::ChunkVote) {
                    build_chunk_vote_evidence(
                        &*model,
                        &haystack,
                        &question,
                        format!("nolima-hard-chunk-vote-{cache_case_key}-{target_tokens}"),
                    )
                    .await
                } else if matches!(candidate_strategy, Strategy::StructuredMemory) {
                    build_structured_memory_evidence(
                        &*model,
                        &haystack,
                        &question,
                        format!("nolima-hard-structured-evidence-{cache_case_key}-{target_tokens}"),
                    )
                    .await
                } else if matches!(candidate_strategy, Strategy::HybridMemory) {
                    let structured = build_structured_memory_evidence(
                        &*model,
                        &haystack,
                        &question,
                        format!(
                            "nolima-hard-hybrid-structured-evidence-{cache_case_key}-{target_tokens}"
                        ),
                    )
                    .await;
                    if evidence_has_usable_snippet(&structured) {
                        structured
                    } else {
                        extract_margin_evidence(
                            &*model,
                            &haystack,
                            &question,
                            "",
                            format!(
                                "nolima-hard-hybrid-margin-evidence-{cache_case_key}-{target_tokens}"
                            ),
                        )
                        .await
                    }
                } else if matches!(
                    candidate_strategy,
                    Strategy::MarginGuarded | Strategy::GuidedMargin | Strategy::VerifiedMargin
                ) {
                    extract_margin_evidence(
                        &*model,
                        &haystack,
                        &question,
                        guidance,
                        format!(
                            "nolima-hard-{:?}-margin-evidence-{cache_case_key}-{target_tokens}",
                            candidate_strategy
                        ),
                    )
                    .await
                } else {
                    extract_relevant_evidence(
                        &*model,
                        &haystack,
                        &question,
                        guidance,
                        format!(
                            "nolima-hard-{:?}-extract-evidence-{cache_case_key}-{target_tokens}",
                            candidate_strategy
                        ),
                    )
                    .await
                };
                let answer_evidence = if matches!(
                    candidate_strategy,
                    Strategy::GuardedHyde
                        | Strategy::MarginGuarded
                        | Strategy::GuidedMargin
                        | Strategy::StatefulRead
                        | Strategy::ChunkVote
                        | Strategy::StructuredMemory
                        | Strategy::VerifiedMargin
                        | Strategy::HybridMemory
                ) {
                    support_only_evidence(&evidence)
                } else {
                    evidence.clone()
                };
                let extracted_answer = answer_from_evidence(
                    &*model,
                    &question,
                    &answer_evidence,
                    format!(
                        "nolima-hard-{:?}-answer-{cache_case_key}-{target_tokens}",
                        candidate_strategy
                    ),
                )
                .await;
                if matches!(
                    candidate_strategy,
                    Strategy::GuardedHyde
                        | Strategy::MarginGuarded
                        | Strategy::GuidedMargin
                        | Strategy::StatefulRead
                        | Strategy::ChunkVote
                        | Strategy::StructuredMemory
                        | Strategy::VerifiedMargin
                        | Strategy::HybridMemory
                ) {
                    if matches!(candidate_strategy, Strategy::VerifiedMargin) {
                        let verified_answer = verify_answer_from_evidence(
                            &*model,
                            &question,
                            &answer_evidence,
                            &extracted_answer,
                            format!(
                                "nolima-hard-{:?}-verify-{cache_case_key}-{target_tokens}",
                                candidate_strategy
                            ),
                        )
                        .await;
                        if !looks_empty_or_unknown_answer(&verified_answer)
                            && normalize_answer(&verified_answer)
                                .contains(&normalize_answer(&extracted_answer))
                            && evidence_has_usable_snippet(&evidence)
                        {
                            extracted_answer
                        } else {
                            answer
                        }
                    } else {
                        choose_guarded_answer(&answer, &evidence, &extracted_answer)
                    }
                } else {
                    extracted_answer
                }
            } else {
                bridge = if matches!(candidate_strategy, Strategy::Bridge) {
                    bridge_terms(
                        &*model,
                        &question,
                        format!("nolima-hard-bridge-terms-{cache_case_key}-{target_tokens}"),
                    )
                    .await
                } else {
                    String::new()
                };
                let candidate_prompt =
                    apply_strategy_prompt(&prompt, &question, candidate_strategy, &bridge);
                ask_model(
                    &*model,
                    candidate_prompt,
                    format!(
                        "nolima-hard-candidate-{:?}-{cache_case_key}-{target_tokens}",
                        candidate_strategy
                    ),
                    thinking.clone(),
                )
                .await
            };
            let candidate_ok =
                normalize_answer(&candidate_answer).contains(&normalize_answer(expected));
            if candidate_ok {
                correct_candidate += 1;
            }
            eprintln!(
                "-- case {idx}: strategy={:?} bridge={:?} evidence={:?} answer={:?} ok={}",
                candidate_strategy, bridge, evidence, candidate_answer, candidate_ok
            );
        }
    }

    if matches!(
        strategy,
        Strategy::StepBack
            | Strategy::Bridge
            | Strategy::Extract
            | Strategy::HydeExtract
            | Strategy::GuardedHyde
            | Strategy::MarginGuarded
            | Strategy::GuidedMargin
            | Strategy::StatefulRead
            | Strategy::ChunkVote
            | Strategy::StructuredMemory
            | Strategy::VerifiedMargin
            | Strategy::HybridMemory
            | Strategy::Compare
    ) {
        eprintln!(
            "-- NoLiMa-Hard baseline score: {}/{} ({:.1}%)",
            correct_baseline,
            limit,
            100.0 * correct_baseline as f64 / limit.max(1) as f64
        );
        eprintln!(
            "-- NoLiMa-Hard candidate score: {}/{} ({:.1}%)",
            correct_candidate,
            limit,
            100.0 * correct_candidate as f64 / limit.max(1) as f64
        );
    } else {
        eprintln!(
            "-- NoLiMa-Hard score: {}/{} ({:.1}%)",
            correct,
            limit,
            100.0 * correct as f64 / limit.max(1) as f64
        );
    }
    if require_nonsaturated {
        assert!(
            correct < limit,
            "NoLiMa-Hard sample saturated at {correct}/{limit}; raise MUAGENT_NOLIMA_CONTEXT_TOKENS or MUAGENT_NOLIMA_CASE_LIMIT"
        );
    }
}
