use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use muagent::core::prelude::{Content, ContentPart, RunState};
use muagent::core::types::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() {
    if let Err(err) = main_entry().await {
        eprintln!("agent-bench error: {err}");
        std::process::exit(2);
    }
}

#[derive(Clone, Debug, Default)]
pub struct BenchArgs {
    list: bool,
    tasks: Vec<String>,
    runs: usize,
    keep_workdir: bool,
    cli_bin: Option<String>,
    config_file: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    cache: Option<String>,
    thinking: Option<String>,
    help: bool,
}

impl BenchArgs {
    pub fn parse_from_env() -> Result<Self, String> {
        load_dotenv();

        let mut out = Self {
            runs: 1,
            cli_bin: std::env::var("MUAGENT_BENCH_CLI_BIN").ok(),
            config_file: std::env::var("MUAGENT_BENCH_CONFIG_FILE")
                .ok()
                .or_else(|| std::env::var("MUAGENT_CONFIG").ok()),
            provider: std::env::var("MUAGENT_BENCH_PROVIDER").ok(),
            model: std::env::var("MUAGENT_BENCH_MODEL").ok(),
            base_url: std::env::var("MUAGENT_BENCH_BASE_URL").ok(),
            cache: std::env::var("MUAGENT_BENCH_CACHE")
                .ok()
                .or_else(|| std::env::var("MUAGENT_CACHE").ok()),
            thinking: std::env::var("MUAGENT_BENCH_THINKING")
                .ok()
                .or_else(|| std::env::var("MUAGENT_THINKING").ok())
                .or_else(|| Some("high".into())),
            ..Default::default()
        };

        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--list" => out.list = true,
                "--keep-workdir" => out.keep_workdir = true,
                "--help" | "-h" => out.help = true,
                "--task" => out.tasks.push(next_arg(&mut it, "--task")?),
                "--runs" => {
                    let raw = next_arg(&mut it, "--runs")?;
                    out.runs = raw
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --runs `{raw}`"))?;
                    if out.runs == 0 {
                        return Err("--runs must be >= 1".into());
                    }
                }
                "--cli-bin" => out.cli_bin = Some(next_arg(&mut it, "--cli-bin")?),
                "--config-file" => out.config_file = Some(next_arg(&mut it, "--config-file")?),
                "--provider" => out.provider = Some(next_arg(&mut it, "--provider")?),
                "--model" => out.model = Some(next_arg(&mut it, "--model")?),
                "--base-url" => out.base_url = Some(next_arg(&mut it, "--base-url")?),
                "--cache" => out.cache = Some(next_arg(&mut it, "--cache")?),
                "--thinking" => out.thinking = Some(next_arg(&mut it, "--thinking")?),
                other => return Err(format!("unknown arg `{other}`")),
            }
        }

        Ok(out)
    }
}

#[derive(Clone, Copy)]
enum MatchMode {
    Exact,
    IgnoreCase,
    NormalizedAlnumIgnoreCase,
}

impl MatchMode {
    fn matches(self, actual: &str, expected: &str) -> bool {
        match self {
            Self::Exact => actual.trim() == expected.trim(),
            Self::IgnoreCase => actual.trim().eq_ignore_ascii_case(expected.trim()),
            Self::NormalizedAlnumIgnoreCase => {
                normalize_alnum(actual).eq_ignore_ascii_case(&normalize_alnum(expected))
            }
        }
    }
}

#[derive(Clone)]
struct TurnSpec {
    prompt: String,
    expected: String,
    match_mode: MatchMode,
    images: Vec<ImageAttachment>,
}

impl TurnSpec {
    fn text(prompt: String, expected: impl Into<String>, match_mode: MatchMode) -> Self {
        Self {
            prompt,
            expected: expected.into(),
            match_mode,
            images: vec![],
        }
    }
}

#[derive(Clone)]
struct ImageAttachment {
    path: PathBuf,
}

fn normalize_alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
}

#[derive(Clone)]
enum FinalCheck {
    FileEquals { path: PathBuf, expected: String },
    PathExists { path: PathBuf },
    PathMissing { path: PathBuf },
    All(Vec<FinalCheck>),
}

impl FinalCheck {
    fn verify(&self) -> Result<(), String> {
        match self {
            Self::FileEquals { path, expected } => {
                let got = fs::read_to_string(path)
                    .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
                if got == *expected {
                    Ok(())
                } else {
                    Err(format!(
                        "file {} mismatch: expected {:?}, got {:?}",
                        path.display(),
                        expected,
                        got
                    ))
                }
            }
            Self::PathExists { path } => {
                if path.exists() {
                    Ok(())
                } else {
                    Err(format!("expected path to exist: {}", path.display()))
                }
            }
            Self::PathMissing { path } => {
                if path.exists() {
                    Err(format!("expected path to be missing: {}", path.display()))
                } else {
                    Ok(())
                }
            }
            Self::All(checks) => {
                for check in checks {
                    check.verify()?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone)]
struct PreparedTask {
    name: &'static str,
    workdir: PathBuf,
    roots: Vec<PathBuf>,
    turns: Vec<TurnSpec>,
    final_check: Option<FinalCheck>,
}

#[derive(Clone, Copy)]
struct TaskDef {
    name: &'static str,
    summary: &'static str,
    prepare: fn() -> Result<PreparedTask, String>,
}

#[derive(Clone, Debug)]
struct TurnResult {
    expected: String,
    actual: String,
    matched: bool,
}

#[derive(Clone, Debug)]
pub struct BenchRunResult {
    task: String,
    workdir: PathBuf,
    trace_path: PathBuf,
    run_index: usize,
    passed: bool,
    infra_error: bool,
    failure: Option<String>,
    duration_ms: u128,
    runner_steps: usize,
    user_turns: usize,
    llm_calls: u32,
    tool_calls: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    thinking_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    final_step: String,
    turn_results: Vec<TurnResult>,
}

pub async fn main_entry() -> Result<(), String> {
    let args = BenchArgs::parse_from_env()?;
    if args.help {
        print_help();
        return Ok(());
    }

    let suite = builtin_suite();
    if args.list {
        print_task_list(&suite);
        return Ok(());
    }

    let selected = select_tasks(&suite, &args.tasks)?;
    let cli_bin = resolve_cli_bin(&args).await?;

    println!(
        "agent-bench mode=cli cli={} provider={} model={} cache={} thinking={} tasks={} runs={}",
        cli_bin.display(),
        args.provider.as_deref().unwrap_or("(cli config)"),
        args.model.as_deref().unwrap_or("(cli config)"),
        args.cache.as_deref().unwrap_or("(cli config/default)"),
        args.thinking.as_deref().unwrap_or("(cli config/default)"),
        selected.len(),
        args.runs
    );

    let mut results = Vec::new();
    for run_index in 0..args.runs {
        for task in &selected {
            let prepared = (task.prepare)()?;
            let result = run_task(&prepared, &cli_bin, &args, run_index).await;
            print_run_result(&result);
            if !args.keep_workdir && result.passed {
                let _ = fs::remove_dir_all(&prepared.workdir);
            }
            results.push(result);
        }
    }

    print_summary(&results);
    Ok(())
}

fn print_help() {
    println!(
        "Usage: cargo run -p muagent --bin agent_bench -- [options]\n\
         \n\
         Options:\n\
           --list                 List built-in benchmark tasks\n\
           --task <name>          Run only the named task (repeatable)\n\
           --runs <n>             Repeat each task n times (default: 1)\n\
           --cli-bin <path>       Use this muagent binary (default: build/use target muagent)\n\
           --config-file <file>   Pass through to muagent --config-file\n\
           --provider <name>      openai | openrouter | anthropic | google\n\
           --model <id>           Override model id\n\
           --base-url <url>       Override provider base URL\n\
           --cache <mode>         auto | off. Passed only when set\n\
           --thinking <mode>      off | auto | minimal | low | medium | high | max\n\
                                  Passed only when set\n\
           --keep-workdir         Keep temp workdirs for inspection\n\
           --help                 Show this message\n\
         \n\
         Environment:\n\
           MUAGENT_BENCH_CLI_BIN / MUAGENT_BENCH_CONFIG_FILE\n\
           MUAGENT_PROVIDER / MUAGENT_MODEL / MUAGENT_BASE_URL / MUAGENT_API_KEY\n\
           OPENAI_API_KEY / OPENROUTER_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY"
    );
}

fn print_task_list(tasks: &[TaskDef]) {
    println!("Built-in tasks:");
    for task in tasks {
        println!("  {:<24} {}", task.name, task.summary);
    }
}

fn select_tasks<'a>(suite: &'a [TaskDef], filters: &[String]) -> Result<Vec<&'a TaskDef>, String> {
    if filters.is_empty() {
        return Ok(suite.iter().collect());
    }

    let mut selected = Vec::new();
    for filter in filters {
        let task = suite
            .iter()
            .find(|task| task.name == filter)
            .ok_or_else(|| format!("unknown task `{filter}`"))?;
        selected.push(task);
    }
    Ok(selected)
}

fn print_run_result(result: &BenchRunResult) {
    let status = if result.passed { "PASS" } else { "FAIL" };
    let mut detail = String::new();
    if let Some(failure) = &result.failure {
        let _ = write!(detail, " failure={failure:?}");
    }
    if !result.turn_results.is_empty() {
        let turns = result
            .turn_results
            .iter()
            .enumerate()
            .map(|(idx, turn)| {
                format!(
                    "t{}={} actual={:?} expected={:?}",
                    idx + 1,
                    if turn.matched { "ok" } else { "bad" },
                    turn.actual,
                    turn.expected
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let _ = write!(detail, " {turns}");
    }
    println!(
        "[{}] {:<24} run={} time={}ms runner_steps={} user_turns={} llm_calls={} tool_calls={} prompt_tok={} completion_tok={} thinking_tok={} cache_read_tok={} cache_write_tok={} final_step={} workdir={} trace={}{}",
        status,
        result.task,
        result.run_index + 1,
        result.duration_ms,
        result.runner_steps,
        result.user_turns,
        result.llm_calls,
        result.tool_calls,
        result.prompt_tokens,
        result.completion_tokens,
        result.thinking_tokens,
        result.cache_read_tokens,
        result.cache_write_tokens,
        result.final_step,
        result.workdir.display(),
        result.trace_path.display(),
        detail
    );
}

fn print_summary(results: &[BenchRunResult]) {
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let infra_errors = results.iter().filter(|r| r.infra_error).count();
    let total_ms: u128 = results.iter().map(|r| r.duration_ms).sum();
    let total_prompt: u32 = results.iter().map(|r| r.prompt_tokens).sum();
    let total_completion: u32 = results.iter().map(|r| r.completion_tokens).sum();
    let total_thinking: u32 = results.iter().map(|r| r.thinking_tokens).sum();
    let total_cache_read: u32 = results.iter().map(|r| r.cache_read_tokens).sum();
    let total_cache_write: u32 = results.iter().map(|r| r.cache_write_tokens).sum();
    let total_llm_calls: u32 = results.iter().map(|r| r.llm_calls).sum();
    let total_tools: u32 = results.iter().map(|r| r.tool_calls).sum();
    let total_runner_steps: usize = results.iter().map(|r| r.runner_steps).sum();
    let avg_ms = if total == 0 {
        0.0
    } else {
        total_ms as f64 / total as f64
    };

    println!();
    println!(
        "summary score={}/{} ({:.1}%) avg_time={:.1}ms avg_runner_steps={:.2} avg_llm_calls={:.2} total_llm_calls={} total_tools={} total_prompt_tok={} total_completion_tok={} total_thinking_tok={} total_cache_read_tok={} total_cache_write_tok={} infra_errors={}",
        passed,
        total,
        if total == 0 {
            0.0
        } else {
            passed as f64 * 100.0 / total as f64
        },
        avg_ms,
        if total == 0 {
            0.0
        } else {
            total_runner_steps as f64 / total as f64
        },
        if total == 0 {
            0.0
        } else {
            total_llm_calls as f64 / total as f64
        },
        total_llm_calls,
        total_tools,
        total_prompt,
        total_completion,
        total_thinking,
        total_cache_read,
        total_cache_write,
        infra_errors
    );

    let mut by_task: BTreeMap<&str, Vec<&BenchRunResult>> = BTreeMap::new();
    for result in results {
        by_task.entry(&result.task).or_default().push(result);
    }

    for (task, rows) in by_task {
        let passed = rows.iter().filter(|r| r.passed).count();
        let avg_ms = rows.iter().map(|r| r.duration_ms).sum::<u128>() as f64 / rows.len() as f64;
        let avg_runner_steps =
            rows.iter().map(|r| r.runner_steps).sum::<usize>() as f64 / rows.len() as f64;
        let avg_llm_calls =
            rows.iter().map(|r| r.llm_calls).sum::<u32>() as f64 / rows.len() as f64;
        let avg_tools = rows.iter().map(|r| r.tool_calls).sum::<u32>() as f64 / rows.len() as f64;
        println!(
            "  {:<24} pass={}/{} avg_time={:.1}ms avg_runner_steps={:.2} avg_llm_calls={:.2} avg_tools={:.2}",
            task,
            passed,
            rows.len(),
            avg_ms,
            avg_runner_steps,
            avg_llm_calls,
            avg_tools
        );
    }
}

#[derive(Clone, Debug)]
struct CliTurnTrace {
    command: Vec<String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u128,
}

#[derive(Clone, Debug)]
struct StoreMetrics {
    runner_steps: usize,
    llm_calls: u32,
    tool_calls: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    thinking_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    final_step: String,
    latest_state: Option<RunState>,
}

impl Default for StoreMetrics {
    fn default() -> Self {
        Self {
            runner_steps: 0,
            llm_calls: 0,
            tool_calls: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            thinking_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            final_step: "missing_state".into(),
            latest_state: None,
        }
    }
}

async fn run_task(
    task: &PreparedTask,
    cli_bin: &Path,
    args: &BenchArgs,
    run_index: usize,
) -> BenchRunResult {
    let started = Instant::now();
    let trace_path = task.workdir.join("agent_bench_trace.txt");
    let store_dir = task.workdir.join(".muagent-bench-store");
    let mut cli_traces = Vec::new();
    let mut turn_results = Vec::new();
    let mut failure = None;
    let mut infra_error = false;

    for (turn_idx, turn) in task.turns.iter().enumerate() {
        let trace = match run_cli_turn(cli_bin, args, task, &store_dir, turn_idx, turn).await {
            Ok(trace) => trace,
            Err(err) => {
                failure = Some(format!("turn {} cli spawn failed: {err}", turn_idx + 1));
                infra_error = true;
                break;
            }
        };

        let status = trace.status;
        let actual = trace.stdout.trim().to_string();
        let matched = status == Some(0) && turn.match_mode.matches(&actual, &turn.expected);
        turn_results.push(TurnResult {
            expected: turn.expected.clone(),
            actual: actual.clone(),
            matched,
        });
        cli_traces.push(trace);

        if status != Some(0) {
            failure = Some(format!(
                "turn {} cli exited with status {:?}",
                turn_idx + 1,
                status
            ));
            infra_error = true;
            break;
        }
        if !matched {
            failure = Some(format!(
                "turn {} answer mismatch: expected {:?}, got {:?}",
                turn_idx + 1,
                turn.expected,
                actual
            ));
            break;
        }
    }

    if failure.is_none() {
        if let Some(check) = &task.final_check {
            if let Err(err) = check.verify() {
                failure = Some(err);
            }
        }
    }

    let metrics = match read_store_metrics(&store_dir) {
        Ok(metrics) => metrics,
        Err(err) => {
            if failure.is_none() {
                failure = Some(format!("read jsonl store failed: {err}"));
                infra_error = true;
            }
            StoreMetrics::default()
        }
    };

    let passed = failure.is_none();
    if let Err(err) = write_trace(TraceReport {
        task,
        run_index,
        store_dir: &store_dir,
        cli_traces: &cli_traces,
        metrics: &metrics,
        turn_results: &turn_results,
        failure: &failure,
        trace_path: &trace_path,
    }) {
        eprintln!("agent-bench trace write failed: {err}");
    }

    BenchRunResult {
        task: task.name.to_string(),
        workdir: task.workdir.clone(),
        trace_path,
        run_index,
        passed,
        infra_error,
        failure,
        duration_ms: started.elapsed().as_millis(),
        runner_steps: metrics.runner_steps,
        user_turns: task.turns.len(),
        llm_calls: metrics.llm_calls,
        tool_calls: metrics.tool_calls,
        prompt_tokens: metrics.prompt_tokens,
        completion_tokens: metrics.completion_tokens,
        thinking_tokens: metrics.thinking_tokens,
        cache_read_tokens: metrics.cache_read_tokens,
        cache_write_tokens: metrics.cache_write_tokens,
        final_step: metrics.final_step,
        turn_results,
    }
}

struct TraceReport<'a> {
    task: &'a PreparedTask,
    run_index: usize,
    store_dir: &'a Path,
    cli_traces: &'a [CliTurnTrace],
    metrics: &'a StoreMetrics,
    turn_results: &'a [TurnResult],
    failure: &'a Option<String>,
    trace_path: &'a Path,
}

fn write_trace(report: TraceReport<'_>) -> Result<(), String> {
    let task = report.task;
    let metrics = report.metrics;
    let mut out = String::new();
    let _ = writeln!(out, "task={}", task.name);
    let _ = writeln!(out, "run={}", report.run_index + 1);
    let _ = writeln!(out, "workdir={}", task.workdir.display());
    let _ = writeln!(out, "store={}", report.store_dir.display());
    let _ = writeln!(out, "final_step={}", metrics.final_step);
    let _ = writeln!(out, "passed={}", report.failure.is_none());
    if let Some(failure) = report.failure {
        let _ = writeln!(out, "failure={failure}");
    }
    let _ = writeln!(
        out,
        "usage llm_calls={} tool_calls={} prompt_tok={} completion_tok={} thinking_tok={} cache_read_tok={} cache_write_tok={}",
        metrics.llm_calls,
        metrics.tool_calls,
        metrics.prompt_tokens,
        metrics.completion_tokens,
        metrics.thinking_tokens,
        metrics.cache_read_tokens,
        metrics.cache_write_tokens,
    );

    out.push_str("\nturns:\n");
    for (idx, turn) in task.turns.iter().enumerate() {
        let _ = writeln!(out, "- turn {}", idx + 1);
        let _ = writeln!(out, "  prompt: {}", truncate_for_trace(&turn.prompt, 1000));
        let _ = writeln!(out, "  expected: {}", turn.expected);
        if let Some(result) = report.turn_results.get(idx) {
            let _ = writeln!(out, "  matched: {}", result.matched);
            let _ = writeln!(
                out,
                "  actual: {}",
                truncate_for_trace(&result.actual, 1000)
            );
        }
    }

    out.push_str("\ncli:\n");
    for (idx, trace) in report.cli_traces.iter().enumerate() {
        let _ = writeln!(out, "- turn {}", idx + 1);
        let _ = writeln!(out, "  status: {:?}", trace.status);
        let _ = writeln!(out, "  duration_ms: {}", trace.duration_ms);
        let _ = writeln!(out, "  command: {}", shell_command(&trace.command));
        let _ = writeln!(
            out,
            "  stdout: {}",
            truncate_for_trace(trace.stdout.trim(), 4000)
        );
        let _ = writeln!(
            out,
            "  stderr: {}",
            truncate_for_trace(trace.stderr.trim(), 4000)
        );
    }

    out.push_str("\nhistory:\n");
    if let Some(state) = &metrics.latest_state {
        for (idx, msg) in state.history.iter().enumerate() {
            render_trace_message(&mut out, idx, msg);
        }
    } else {
        out.push_str("(no persisted run state)\n");
    }

    fs::write(report.trace_path, out)
        .map_err(|e| format!("write {}: {e}", report.trace_path.display()))
}

fn render_trace_message(out: &mut String, idx: usize, msg: &Message) {
    match msg {
        Message::User { content } => {
            let _ = writeln!(out, "[{idx}] user");
            render_trace_content(out, content);
        }
        Message::System { content } => {
            let _ = writeln!(out, "[{idx}] system");
            render_trace_content(out, content);
        }
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let _ = writeln!(out, "[{idx}] assistant");
            render_trace_content(out, content);
            if !tool_calls.is_empty() {
                out.push_str("  tool_calls:\n");
                for call in tool_calls {
                    let _ = writeln!(
                        out,
                        "  - id={} name={} args={}",
                        call.id,
                        call.tool_name,
                        truncate_for_trace(&call.args.to_string(), 1000)
                    );
                }
            }
        }
        Message::ToolResult { call_id, result } => {
            let _ = writeln!(
                out,
                "[{idx}] tool_result call_id={} ok={} retryable={}",
                call_id, result.ok, result.retryable
            );
            if let Some(hint) = &result.hint {
                let _ = writeln!(out, "  hint: {}", truncate_for_trace(hint, 1000));
            }
            render_trace_content(out, &result.content);
        }
        Message::Observation { kind, text } => {
            let _ = writeln!(out, "[{idx}] observation kind={kind:?}");
            let _ = writeln!(out, "  text: {}", truncate_for_trace(text, 2000));
        }
    }
}

fn render_trace_content(out: &mut String, content: &Content) {
    match content {
        Content::Text(text) => {
            let _ = writeln!(out, "  text: {}", truncate_for_trace(text, 2000));
        }
        Content::Parts(parts) => {
            out.push_str("  parts:\n");
            for (idx, part) in parts.iter().enumerate() {
                match part {
                    ContentPart::Text { text } => {
                        let _ = writeln!(
                            out,
                            "  - part[{idx}] text: {}",
                            truncate_for_trace(text, 2000)
                        );
                    }
                    ContentPart::Image { uri, b64, mime } => {
                        let _ = writeln!(
                            out,
                            "  - part[{idx}] image mime={} uri={} b64_len={}",
                            mime,
                            uri.as_deref().unwrap_or("-"),
                            b64.as_ref().map(|s| s.len()).unwrap_or(0)
                        );
                    }
                    ContentPart::Data { mime, b64 } => {
                        let _ = writeln!(
                            out,
                            "  - part[{idx}] data mime={} b64_len={}",
                            mime,
                            b64.len()
                        );
                    }
                }
            }
        }
    }
}

fn truncate_for_trace(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

async fn resolve_cli_bin(args: &BenchArgs) -> Result<PathBuf, String> {
    if let Some(raw) = &args.cli_bin {
        let path = PathBuf::from(raw);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!("--cli-bin does not exist: {}", path.display()));
    }

    let output = tokio::process::Command::new("cargo")
        .args(["build", "-p", "muagent", "--bin", "muagent"])
        .output()
        .await
        .map_err(|e| format!("spawn cargo build for muagent cli: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build -p muagent --bin muagent failed: {}",
            truncate_for_trace(&String::from_utf8_lossy(&output.stderr), 4000)
        ));
    }

    for candidate in cli_bin_candidates()? {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("muagent binary was built but could not be located".into())
}

fn cli_bin_candidates() -> Result<Vec<PathBuf>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut dirs = Vec::new();
    if let Some(parent) = exe.parent() {
        dirs.push(parent.to_path_buf());
        if parent.file_name().and_then(|s| s.to_str()) == Some("deps") {
            if let Some(profile_dir) = parent.parent() {
                dirs.push(profile_dir.to_path_buf());
            }
        }
    }

    let name = cli_exe_name();
    let mut out = Vec::new();
    for dir in dirs {
        let candidate = dir.join(name);
        if !out.iter().any(|p: &PathBuf| p == &candidate) {
            out.push(candidate);
        }
    }
    Ok(out)
}

fn cli_exe_name() -> &'static str {
    if cfg!(windows) {
        "muagent.exe"
    } else {
        "muagent"
    }
}

async fn run_cli_turn(
    cli_bin: &Path,
    args: &BenchArgs,
    task: &PreparedTask,
    store_dir: &Path,
    turn_idx: usize,
    turn: &TurnSpec,
) -> Result<CliTurnTrace, String> {
    let mut argv = build_cli_args(args, task, store_dir)?;
    for image in &turn.images {
        argv.push("--image".into());
        argv.push(image.path.display().to_string());
    }
    argv.push("exec".into());
    if turn_idx > 0 {
        argv.push("resume".into());
        argv.push("--last".into());
    }
    argv.push(turn.prompt.clone());

    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(cli_bin);
    cmd.args(&argv);
    if std::env::var("MUAGENT_API_KEY").is_err() {
        if let Ok(key) = std::env::var("MUAGENT_BENCH_API_KEY") {
            cmd.env("MUAGENT_API_KEY", key);
        }
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("spawn {}: {e}", cli_bin.display()))?;

    let mut command = Vec::with_capacity(argv.len() + 1);
    command.push(cli_bin.display().to_string());
    command.extend(argv);
    Ok(CliTurnTrace {
        command,
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        duration_ms: started.elapsed().as_millis(),
    })
}

fn build_cli_args(
    args: &BenchArgs,
    task: &PreparedTask,
    store_dir: &Path,
) -> Result<Vec<String>, String> {
    let root = match task.roots.as_slice() {
        [root] => root,
        [] => &task.workdir,
        _ => {
            return Err(format!(
                "task {} has multiple roots, but muagent accepts one --root",
                task.name
            ));
        }
    };

    let mut argv = Vec::new();
    if let Some(value) = &args.config_file {
        argv.push("--config-file".into());
        argv.push(value.clone());
    }
    if let Some(value) = &args.provider {
        argv.push("--provider".into());
        argv.push(value.clone());
    }
    if let Some(value) = &args.model {
        argv.push("--model".into());
        argv.push(value.clone());
    }
    if let Some(value) = &args.base_url {
        argv.push("--base-url".into());
        argv.push(value.clone());
    }
    if let Some(value) = &args.cache {
        argv.push("--cache".into());
        argv.push(value.clone());
    }
    if let Some(value) = &args.thinking {
        argv.push("--thinking".into());
        argv.push(value.clone());
    }
    argv.push("--store".into());
    argv.push(format!("jsonl:{}", store_dir.display()));
    argv.push("--root".into());
    argv.push(root.display().to_string());
    Ok(argv)
}

fn read_store_metrics(store_dir: &Path) -> Result<StoreMetrics, String> {
    let runs_dir = store_dir.join("runs");
    let mut metrics = StoreMetrics::default();
    let entries = match fs::read_dir(&runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(metrics),
        Err(e) => return Err(format!("read {}: {e}", runs_dir.display())),
    };

    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir {}: {e}", runs_dir.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let body =
            fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut last_state = None;
        for (idx, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| format!("parse {} line {}: {e}", path.display(), idx + 1))?;
            if let Some(events) = value.get("events").and_then(|v| v.as_array()) {
                metrics.runner_steps += events
                    .iter()
                    .filter(|event| {
                        event.get("kind").and_then(|kind| kind.as_str()) == Some("step_advanced")
                    })
                    .count();
            }
            let state_value = value
                .get("state")
                .ok_or_else(|| format!("missing state in {} line {}", path.display(), idx + 1))?;
            let state: RunState = serde_json::from_value(state_value.clone())
                .map_err(|e| format!("parse state {} line {}: {e}", path.display(), idx + 1))?;
            last_state = Some(state);
        }

        if let Some(state) = last_state {
            metrics.llm_calls = metrics.llm_calls.saturating_add(state.usage.turns);
            metrics.tool_calls = metrics.tool_calls.saturating_add(state.usage.tool_calls);
            metrics.prompt_tokens = metrics
                .prompt_tokens
                .saturating_add(state.usage.tokens_prompt);
            metrics.completion_tokens = metrics
                .completion_tokens
                .saturating_add(state.usage.tokens_completion);
            metrics.thinking_tokens = metrics
                .thinking_tokens
                .saturating_add(state.usage.tokens_thinking);
            metrics.cache_read_tokens = metrics
                .cache_read_tokens
                .saturating_add(state.usage.tokens_cache_read);
            metrics.cache_write_tokens = metrics
                .cache_write_tokens
                .saturating_add(state.usage.tokens_cache_write);

            let replace_latest = metrics
                .latest_state
                .as_ref()
                .map(|latest| state.updated_ms > latest.updated_ms)
                .unwrap_or(true);
            if replace_latest {
                metrics.final_step = state.step.name().to_string();
                metrics.latest_state = Some(state);
            }
        }
    }

    Ok(metrics)
}

fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn next_arg(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("missing value for {flag}"))
}

fn load_dotenv() {
    for path in [".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(path).is_ok() {
            break;
        }
    }
}

fn tempdir(prefix: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    Ok(path)
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn write_text_file(path: &Path, body: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_binary_file(path: &Path, body: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

fn render_text_png(text: &str) -> Vec<u8> {
    const SCALE: usize = 12;
    const GLYPH_W: usize = 7;
    const GLYPH_H: usize = 9;
    const SPACING: usize = 2;
    const MARGIN: usize = 32;

    let chars = text.chars().count().max(1);
    let width = MARGIN * 2 + chars * GLYPH_W * SCALE + chars.saturating_sub(1) * SPACING * SCALE;
    let height = MARGIN * 2 + GLYPH_H * SCALE;
    let mut rgb = vec![255u8; width * height * 3];

    for y in 0..height {
        for x in 0..width {
            if x < 3 || y < 3 || x + 3 >= width || y + 3 >= height {
                set_rgb(&mut rgb, width, x, y, [40, 40, 40]);
            }
        }
    }

    let mut x0 = MARGIN;
    for ch in text.chars() {
        let glyph = glyph_7x9(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for (col, bit) in bits.as_bytes().iter().enumerate() {
                if *bit == b'1' {
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            set_rgb(
                                &mut rgb,
                                width,
                                x0 + col * SCALE + dx,
                                MARGIN + row * SCALE + dy,
                                [5, 8, 12],
                            );
                        }
                    }
                }
            }
        }
        x0 += (GLYPH_W + SPACING) * SCALE;
    }

    encode_png_rgb(width as u32, height as u32, &rgb)
}

fn set_rgb(rgb: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 3]) {
    let idx = (y * width + x) * 3;
    rgb[idx..idx + 3].copy_from_slice(&color);
}

fn glyph_7x9(ch: char) -> [&'static str; 9] {
    match ch.to_ascii_uppercase() {
        'A' => [
            "0011100", "0100010", "1000001", "1000001", "1111111", "1000001", "1000001", "1000001",
            "1000001",
        ],
        'B' => [
            "1111110", "1000001", "1000001", "1111110", "1000001", "1000001", "1000001", "1000001",
            "1111110",
        ],
        'C' => [
            "0011110", "0100001", "1000000", "1000000", "1000000", "1000000", "1000000", "0100001",
            "0011110",
        ],
        'D' => [
            "1111100", "1000010", "1000001", "1000001", "1000001", "1000001", "1000001", "1000010",
            "1111100",
        ],
        'E' => [
            "1111111", "1000000", "1000000", "1000000", "1111110", "1000000", "1000000", "1000000",
            "1111111",
        ],
        'G' => [
            "0011110", "0100001", "1000000", "1000000", "1001111", "1000001", "1000001", "0100001",
            "0011110",
        ],
        'N' => [
            "1000001", "1100001", "1010001", "1001001", "1000101", "1000011", "1000001", "1000001",
            "1000001",
        ],
        'O' => [
            "0011100", "0100010", "1000001", "1000001", "1000001", "1000001", "1000001", "0100010",
            "0011100",
        ],
        'S' => [
            "0111110", "1000001", "1000000", "1000000", "0111110", "0000001", "0000001", "1000001",
            "0111110",
        ],
        'T' => [
            "1111111", "0001000", "0001000", "0001000", "0001000", "0001000", "0001000", "0001000",
            "0001000",
        ],
        'X' => [
            "1000001", "0100010", "0010100", "0001000", "0001000", "0010100", "0100010", "1000001",
            "1000001",
        ],
        '2' => [
            "0111110", "1000001", "0000001", "0000010", "0000100", "0001000", "0010000", "0100000",
            "1111111",
        ],
        '4' => [
            "0001000", "0011000", "0101000", "1001000", "1001000", "1111111", "0001000", "0001000",
            "0001000",
        ],
        _ => [
            "0000000", "0000000", "0000000", "0000000", "0000000", "0000000", "0000000", "0000000",
            "0000000",
        ],
    }
}

fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let row_len = width as usize * 3;
    let mut scanlines = Vec::with_capacity((row_len + 1) * height as usize);
    for row in 0..height as usize {
        scanlines.push(0);
        let start = row * row_len;
        scanlines.extend_from_slice(&rgb[start..start + row_len]);
    }

    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1A\n");

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    append_png_chunk(&mut png, b"IHDR", &ihdr);

    let idat = zlib_store(&scanlines);
    append_png_chunk(&mut png, b"IDAT", &idat);
    append_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn append_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);

    let mut crc_data = Vec::with_capacity(kind.len() + data.len());
    crc_data.extend_from_slice(kind);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let len = remaining.min(65_535);
        let final_block = if offset + len == data.len() { 1 } else { 0 };
        out.push(final_block);
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&(!(len as u16)).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + len]);
        offset += len;
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn builtin_suite() -> Vec<TaskDef> {
    vec![
        TaskDef {
            name: "fs_roundtrip_exact",
            summary: "write a file, read it back, answer exactly",
            prepare: prepare_fs_roundtrip_exact,
        },
        TaskDef {
            name: "invoice_total_nested",
            summary: "aggregate nested JSON invoices with tool use",
            prepare: prepare_invoice_total_nested,
        },
        TaskDef {
            name: "log_error_count",
            summary: "count ERROR lines in a log file",
            prepare: prepare_log_error_count,
        },
        TaskDef {
            name: "csv_best_region",
            summary: "compute best region from CSV net revenue",
            prepare: prepare_csv_best_region,
        },
        TaskDef {
            name: "two_turn_contact_memory",
            summary: "two-turn follow-up over the same contact record",
            prepare: prepare_two_turn_contact_memory,
        },
        TaskDef {
            name: "project_owner_lookup",
            summary: "look up a project owner from JSON",
            prepare: prepare_project_owner_lookup,
        },
        TaskDef {
            name: "pending_bug_count",
            summary: "count open bug tickets in JSON",
            prepare: prepare_pending_bug_count,
        },
        TaskDef {
            name: "markdown_heading_count",
            summary: "count markdown level-2 headings",
            prepare: prepare_markdown_heading_count,
        },
        TaskDef {
            name: "yaml_prod_port",
            summary: "extract prod port from a YAML-like config",
            prepare: prepare_yaml_prod_port,
        },
        TaskDef {
            name: "rename_report_file",
            summary: "rename a report file and verify the move",
            prepare: prepare_rename_report_file,
        },
        TaskDef {
            name: "delete_stale_file",
            summary: "delete a stale file and verify it is gone",
            prepare: prepare_delete_stale_file,
        },
        TaskDef {
            name: "inventory_restock_count",
            summary: "count inventory items below reorder level",
            prepare: prepare_inventory_restock_count,
        },
        TaskDef {
            name: "expenses_q2_total",
            summary: "sum approved Q2 expenses across files",
            prepare: prepare_expenses_q2_total,
        },
        TaskDef {
            name: "tsv_fastest_runner",
            summary: "find the fastest runner from TSV data",
            prepare: prepare_tsv_fastest_runner,
        },
        TaskDef {
            name: "latest_release_channel",
            summary: "return the channel of the newest stable release",
            prepare: prepare_latest_release_channel,
        },
        TaskDef {
            name: "two_turn_project_memory",
            summary: "two-turn follow-up over the same project record",
            prepare: prepare_two_turn_project_memory,
        },
        TaskDef {
            name: "todo_note_count",
            summary: "count markdown notes tagged with #todo",
            prepare: prepare_todo_note_count,
        },
        TaskDef {
            name: "earliest_event_title",
            summary: "find the earliest event title from CSV",
            prepare: prepare_earliest_event_title,
        },
        TaskDef {
            name: "enabled_flag_highest_rollout",
            summary: "pick the enabled flag with the highest rollout",
            prepare: prepare_enabled_flag_highest_rollout,
        },
        TaskDef {
            name: "nested_markdown_file_count",
            summary: "count markdown files recursively",
            prepare: prepare_nested_markdown_file_count,
        },
        TaskDef {
            name: "image_direct_text",
            summary: "read text from a direct PNG image attachment",
            prepare: prepare_image_direct_text,
        },
        TaskDef {
            name: "image_fs_read_text",
            summary: "read text from an image attachment returned by fs_read",
            prepare: prepare_image_fs_read_text,
        },
    ]
}

fn prepare_fs_roundtrip_exact() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-roundtrip")?;
    let target = workdir.join("outbox").join("greeting.txt");

    let expected = "alpha beta 1729";
    Ok(PreparedTask {
        name: "fs_roundtrip_exact",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Write the exact text `{expected}` to {}. Then read the file back and return only the file contents.",
                file_uri(&target)
            ),
            expected: expected.to_string(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: Some(FinalCheck::FileEquals {
            path: target,
            expected: expected.to_string(),
        }),
    })
}

fn prepare_invoice_total_nested() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-invoices")?;
    let invoices = [
        (
            "incoming/april/a.json",
            r#"{"id":"a","status":"paid","amount":17}"#,
        ),
        (
            "incoming/april/b.json",
            r#"{"id":"b","status":"void","amount":99}"#,
        ),
        (
            "archive/2026/c.json",
            r#"{"id":"c","status":"paid","amount":8}"#,
        ),
        (
            "archive/2026/d.json",
            r#"{"id":"d","status":"paid","amount":12}"#,
        ),
    ];
    for (rel, body) in invoices {
        let path = workdir.join(rel);
        write_text_file(&path, body)?;
    }

    Ok(PreparedTask {
        name: "invoice_total_nested",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Inside {} there are JSON invoice files in nested folders. Compute the total `amount` for every invoice whose `status` is `paid`. Return only the integer total.",
                file_uri(&workdir)
            ),
            expected: "37".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_log_error_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-logs")?;
    let log_path = workdir.join("logs").join("app.log");

    let lines = [
        "2026-04-23T10:00:00Z INFO boot",
        "2026-04-23T10:00:01Z ERROR auth",
        "2026-04-23T10:00:02Z INFO warmup",
        "2026-04-23T10:00:03Z ERROR queue",
        "2026-04-23T10:00:04Z WARN retry",
        "2026-04-23T10:00:05Z ERROR cache",
        "2026-04-23T10:00:06Z INFO idle",
        "2026-04-23T10:00:07Z error lowercase_should_not_count",
        "2026-04-23T10:00:08Z ERROR billing",
        "2026-04-23T10:00:09Z INFO done",
    ]
    .join("\n");
    write_text_file(&log_path, &format!("{lines}\n"))?;

    Ok(PreparedTask {
        name: "log_error_count",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Count how many lines in {} contain the exact uppercase substring `ERROR`. Return only the integer count.",
                file_uri(&log_path)
            ),
            expected: "4".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_csv_best_region() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-csv")?;
    let csv_path = workdir.join("sales.csv");
    let body = "\
region,revenue,refunds\n\
west,100,10\n\
west,60,0\n\
north,70,10\n\
north,90,20\n\
east,120,40\n\
east,50,5\n\
south,130,15\n";
    write_text_file(&csv_path, body)?;

    Ok(PreparedTask {
        name: "csv_best_region",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "In {} each row has region,revenue,refunds. Compute each region's total net revenue where net = revenue - refunds. Return only the region with the highest total net revenue.",
                file_uri(&csv_path)
            ),
            expected: "west".into(),
            match_mode: MatchMode::IgnoreCase,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_two_turn_contact_memory() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-memory")?;
    let contacts_path = workdir.join("contacts.json");
    let body = r#"[
  {"name":"Mara Chen","code":"SFO-118","city":"San Francisco"},
  {"name":"Nina Patel","code":"DXB-442","city":"Dubai"},
  {"name":"Jon Park","code":"BER-007","city":"Berlin"}
]"#;
    write_text_file(&contacts_path, body)?;

    Ok(PreparedTask {
        name: "two_turn_contact_memory",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![
            TurnSpec {
                prompt: format!(
                    "Find the entry for `Nina Patel` in {}. Return only its `code` value.",
                    file_uri(&contacts_path)
                ),
                expected: "DXB-442".into(),
                match_mode: MatchMode::Exact,
                images: vec![],
            },
            TurnSpec {
                prompt: "Now return only the same entry's city value in lowercase.".into(),
                expected: "dubai".into(),
                match_mode: MatchMode::Exact,
                images: vec![],
            },
        ],
        final_check: None,
    })
}

fn prepare_project_owner_lookup() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-project-owner")?;
    let path = workdir.join("projects.json");
    let body = r#"[
  {"name":"atlas","owner":"rhea","status":"active"},
  {"name":"cinder","owner":"noah","status":"paused"},
  {"name":"lumen","owner":"ivy","status":"active"}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "project_owner_lookup",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and return only the owner of the project named `cinder`.",
                file_uri(&path)
            ),
            expected: "noah".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_pending_bug_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-bugs")?;
    let path = workdir.join("incidents.json");
    let body = r#"[
  {"id":"A-1","kind":"bug","status":"open"},
  {"id":"A-2","kind":"task","status":"open"},
  {"id":"A-3","kind":"bug","status":"closed"},
  {"id":"A-4","kind":"bug","status":"open"},
  {"id":"A-5","kind":"bug","status":"open"}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "pending_bug_count",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Count how many entries in {} have `kind` = `bug` and `status` = `open`. Return only the integer count.",
                file_uri(&path)
            ),
            expected: "3".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_markdown_heading_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-headings")?;
    let path = workdir.join("guide.md");
    let body = "\
# Product Guide
\n\
## Overview
Text
\n\
## Setup
Text
\n\
### Appendix
Text
\n\
## Troubleshooting
Text
";
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "markdown_heading_count",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Count the markdown headings in {} that start with the exact prefix `## `. Return only the integer count.",
                file_uri(&path)
            ),
            expected: "3".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_yaml_prod_port() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-yaml")?;
    let path = workdir.join("config.yml");
    let body = "\
env:
  dev:
    port: 3000
  staging:
    port: 8080
  prod:
    port: 8443
";
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "yaml_prod_port",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and return only the prod port value.",
                file_uri(&path)
            ),
            expected: "8443".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_rename_report_file() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-rename")?;
    let source = workdir.join("draft").join("report.txt");
    let target = workdir.join("final").join("report.txt");
    write_text_file(&source, "ship checklist complete\n")?;
    let target_parent = target
        .parent()
        .ok_or_else(|| format!("no parent for {}", target.display()))?;
    fs::create_dir_all(target_parent)
        .map_err(|e| format!("create {}: {e}", target_parent.display()))?;

    Ok(PreparedTask {
        name: "rename_report_file",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Rename {} to {}. After renaming, return only the destination path's filename.",
                file_uri(&source),
                file_uri(&target)
            ),
            expected: "report.txt".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: Some(FinalCheck::All(vec![
            FinalCheck::FileEquals {
                path: target,
                expected: "ship checklist complete\n".into(),
            },
            FinalCheck::PathExists {
                path: workdir.join("final").join("report.txt"),
            },
            FinalCheck::PathMissing { path: source },
        ])),
    })
}

fn prepare_delete_stale_file() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-delete")?;
    let stale = workdir.join("queue").join("stale.tmp");
    write_text_file(&stale, "remove me\n")?;
    write_text_file(&workdir.join("queue").join("keep-a.txt"), "a\n")?;
    write_text_file(&workdir.join("queue").join("keep-b.txt"), "b\n")?;

    Ok(PreparedTask {
        name: "delete_stale_file",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Delete {}. Then count how many files remain in {} and return only that integer.",
                file_uri(&stale),
                file_uri(&workdir.join("queue"))
            ),
            expected: "2".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: Some(FinalCheck::PathMissing { path: stale }),
    })
}

fn prepare_inventory_restock_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-inventory")?;
    let path = workdir.join("inventory.json");
    let body = r#"[
  {"sku":"A","stock":2,"reorder_level":5},
  {"sku":"B","stock":9,"reorder_level":4},
  {"sku":"C","stock":1,"reorder_level":3},
  {"sku":"D","stock":7,"reorder_level":7},
  {"sku":"E","stock":0,"reorder_level":2}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "inventory_restock_count",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and count how many items have `stock` strictly less than `reorder_level`. Return only the integer count.",
                file_uri(&path)
            ),
            expected: "3".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_expenses_q2_total() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-expenses")?;
    let files = [
        (
            "q1/team-a.json",
            r#"{"quarter":"Q1","approved":true,"amount":11}"#,
        ),
        (
            "q2/team-a.json",
            r#"{"quarter":"Q2","approved":true,"amount":30}"#,
        ),
        (
            "q2/team-b.json",
            r#"{"quarter":"Q2","approved":false,"amount":50}"#,
        ),
        (
            "q2/team-c.json",
            r#"{"quarter":"Q2","approved":true,"amount":14}"#,
        ),
        (
            "q3/team-a.json",
            r#"{"quarter":"Q3","approved":true,"amount":99}"#,
        ),
    ];
    for (rel, body) in files {
        write_text_file(&workdir.join(rel), body)?;
    }

    Ok(PreparedTask {
        name: "expenses_q2_total",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Inside {} there are expense JSON files. Sum `amount` only for files where `quarter` is `Q2` and `approved` is true. Return only the integer total.",
                file_uri(&workdir)
            ),
            expected: "44".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_tsv_fastest_runner() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-tsv")?;
    let path = workdir.join("results.tsv");
    let body = "\
name\tseconds\n\
aya\t305\n\
ben\t298\n\
li\t301\n\
zoe\t312\n";
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "tsv_fastest_runner",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and return only the name with the smallest `seconds` value.",
                file_uri(&path)
            ),
            expected: "ben".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_latest_release_channel() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-releases")?;
    let path = workdir.join("releases.json");
    let body = r#"[
  {"version":"1.8.0","stable":true,"channel":"stable"},
  {"version":"1.9.0-rc1","stable":false,"channel":"rc"},
  {"version":"1.8.2","stable":true,"channel":"stable-hotfix"},
  {"version":"1.7.9","stable":true,"channel":"stable"}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "latest_release_channel",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {}. Among entries where `stable` is true, find the newest version and return only its `channel` value.",
                file_uri(&path)
            ),
            expected: "stable-hotfix".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_two_turn_project_memory() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-project-memory")?;
    let path = workdir.join("projects.json");
    let body = r#"[
  {"name":"apollo","owner":"tess","deadline":"2026-06-01","code":"AP-1"},
  {"name":"mercury","owner":"omar","deadline":"2026-08-15","code":"ME-9"},
  {"name":"zephyr","owner":"lina","deadline":"2026-05-20","code":"ZE-4"}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "two_turn_project_memory",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![
            TurnSpec {
                prompt: format!(
                    "Find the project in {} owned by `omar`. Return only its `code`.",
                    file_uri(&path)
                ),
                expected: "ME-9".into(),
                match_mode: MatchMode::Exact,
                images: vec![],
            },
            TurnSpec {
                prompt: "Now return only the same project's deadline.".into(),
                expected: "2026-08-15".into(),
                match_mode: MatchMode::Exact,
                images: vec![],
            },
        ],
        final_check: None,
    })
}

fn prepare_todo_note_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-todo-notes")?;
    let files = [
        ("notes/a.md", "#todo fix import path\n"),
        ("notes/b.md", "plain note\n"),
        ("notes/archive/c.md", "#todo migrate schema\n"),
        ("notes/archive/d.md", "#done ship release\n"),
        ("notes/e.md", "#todo update tests\n"),
    ];
    for (rel, body) in files {
        write_text_file(&workdir.join(rel), body)?;
    }

    Ok(PreparedTask {
        name: "todo_note_count",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Inside {} there are markdown notes. Count how many `.md` files contain the exact tag `#todo`. Return only the integer count.",
                file_uri(&workdir.join("notes"))
            ),
            expected: "3".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_earliest_event_title() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-events")?;
    let path = workdir.join("events.csv");
    let body = "\
date,title\n\
2026-09-01,launch\n\
2026-04-28,kickoff\n\
2026-05-05,design-review\n\
2026-04-30,retro\n";
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "earliest_event_title",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and return only the title of the earliest date.",
                file_uri(&path)
            ),
            expected: "kickoff".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_enabled_flag_highest_rollout() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-flags")?;
    let path = workdir.join("flags.json");
    let body = r#"[
  {"name":"alpha","enabled":true,"rollout":25},
  {"name":"beta","enabled":false,"rollout":90},
  {"name":"gamma","enabled":true,"rollout":80},
  {"name":"delta","enabled":true,"rollout":60}
]"#;
    write_text_file(&path, body)?;

    Ok(PreparedTask {
        name: "enabled_flag_highest_rollout",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: format!(
                "Read {} and return only the `name` of the enabled flag with the highest `rollout` value.",
                file_uri(&path)
            ),
            expected: "gamma".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_nested_markdown_file_count() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-md-count")?;
    let files = [
        ("docs/a.md", "a\n"),
        ("docs/b.txt", "b\n"),
        ("docs/guides/c.md", "c\n"),
        ("docs/guides/d.md", "d\n"),
        ("docs/assets/e.png", "not really a png\n"),
        ("docs/archive/f.md", "f\n"),
    ];
    for (rel, body) in files {
        write_text_file(&workdir.join(rel), body)?;
    }

    Ok(PreparedTask {
        name: "nested_markdown_file_count",
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        turns: vec![TurnSpec {
            prompt: format!(
                "Count how many files under {} have the `.md` extension, recursively. Return only the integer count.",
                file_uri(&workdir.join("docs"))
            ),
            expected: "4".into(),
            match_mode: MatchMode::Exact,
            images: vec![],
        }],
        final_check: None,
    })
}

fn prepare_image_direct_text() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-image-direct")?;
    let image_path = workdir.join("text.png");
    let png = render_text_png("TEXT");
    write_binary_file(&image_path, &png)?;

    Ok(PreparedTask {
        name: "image_direct_text",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec {
            prompt: "The attached PNG screenshot contains one uppercase label. Return only the exact text shown in the image, with no spaces.".into(),
            expected: "TEXT".into(),
            match_mode: MatchMode::NormalizedAlnumIgnoreCase,
            images: vec![ImageAttachment { path: image_path }],
        }],
        final_check: None,
    })
}

fn prepare_image_fs_read_text() -> Result<PreparedTask, String> {
    let workdir = tempdir("muagent-bench-image-fs")?;
    let image_path = workdir.join("screenshots").join("code.png");
    let png = render_text_png("TEXT");
    write_binary_file(&image_path, &png)?;

    Ok(PreparedTask {
        name: "image_fs_read_text",
        workdir: workdir.clone(),
        roots: vec![workdir],
        turns: vec![TurnSpec::text(
            format!(
                "Call fs_read on the PNG screenshot at {} without force_text (omit force_text or set it to false). fs_read will attach the image for visual inspection; inspect that returned image attachment. Return only the exact uppercase text shown in the image, with no spaces.",
                file_uri(&image_path)
            ),
            "TEXT",
            MatchMode::NormalizedAlnumIgnoreCase,
        )],
        final_check: None,
    })
}

#[cfg(test)]
mod tests {
    use super::builtin_suite;

    #[test]
    fn builtin_task_names_are_unique() {
        let suite = builtin_suite();
        let mut seen = std::collections::BTreeSet::new();
        for task in suite {
            assert!(seen.insert(task.name), "duplicate task {}", task.name);
        }
    }

    #[test]
    fn builtin_suite_has_expected_task_count() {
        assert_eq!(builtin_suite().len(), 22);
    }
}
