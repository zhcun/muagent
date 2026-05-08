#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use muagent::adapters::linux::{LinuxFileSystem, LinuxProcessExec};
use muagent::core::prelude::*;
use muagent::core::testing::{reply, CannedModel};
use muagent::prelude::{register_defaults, AdapterBundle, Agent, AgentEvent, AgentTeam};
use muagent::runtime::{
    DefaultToolExecutor, DefaultToolSetProvider, SubagentExecutor, SubagentTool,
};
use muagent::storage::MemorySessionStore;
use serde_json::json;
use uuid::Uuid;

const OPENROUTER_GPT54NANO_CONFIG: &str = "config/openrouter-gpt54nano.toml";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn pc(id: &str, name: &str, args: serde_json::Value) -> PendingCall {
    PendingCall::new(id, name, args)
}

fn write_text_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, content).expect("write fixture file");
}

fn write_contacts_fixture(path: &Path) {
    write_text_file(
        path,
        r#"[
  {"name":"Mara Chen","code":"SFO-118","city":"San Francisco"},
  {"name":"Nina Patel","code":"DXB-442","city":"Dubai"},
  {"name":"Jon Park","code":"BER-007","city":"Berlin"}
]"#,
    );
}

fn sdk_agent(root: &Path, replies: Vec<ModelReply>, tool_allowlist: Option<Vec<String>>) -> Agent {
    sdk_agent_with_registry(root, replies, tool_allowlist, |_| {})
}

fn sdk_agent_with_registry<F>(
    root: &Path,
    replies: Vec<ModelReply>,
    tool_allowlist: Option<Vec<String>>,
    register_extra: F,
) -> Agent
where
    F: FnOnce(&CapabilityRegistry),
{
    let fs = Arc::new(LinuxFileSystem::new(vec![root.to_path_buf()]));
    let proc = Arc::new(LinuxProcessExec::new());
    let bundle = Arc::new(
        AdapterBundle::builder()
            .fs(fs)
            .proc(proc)
            .build()
            .expect("adapter bundle"),
    );
    let registry = Arc::new(CapabilityRegistry::new());
    register_defaults(&registry, bundle);
    register_extra(&registry);

    let mut executor = DefaultToolExecutor::new(registry.clone());
    let mut provider = DefaultToolSetProvider::new(registry);
    if let Some(list) = tool_allowlist {
        executor = executor.with_tool_allowlist(list.clone());
        provider = provider.with_tool_allowlist(list);
    }

    let runner = Runner::builder()
        .model(Arc::new(CannedModel::new(replies)))
        .tools(Arc::new(executor))
        .tools_provider(provider)
        .store(Arc::new(MemorySessionStore::new()))
        .base_system_prompt("SDK benchmark smoke test.")
        .build()
        .expect("runner build");

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    state.workspace_root = Some(root.display().to_string());
    Agent::from_parts(Arc::new(runner), state)
}

struct StaticSubagentExecutor;

#[async_trait::async_trait]
impl SubagentExecutor for StaticSubagentExecutor {
    async fn invoke(
        &self,
        definition: AgentDefinition,
        invocation: SubagentInvocation,
        _cancel: CancelToken,
    ) -> Result<SubagentResult, String> {
        Ok(SubagentResult {
            agent_name: definition.name,
            final_text: format!("subagent handled: {}", invocation.task),
            run_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            usage: Usage {
                tokens_prompt: 7,
                tokens_completion: 3,
                cost_usd: 0.25,
                turns: 2,
                tool_calls: 1,
                ..Default::default()
            },
        })
    }
}

async fn live_openrouter_agent(root: &Path, max_steps: usize, tools: &[&str]) -> Agent {
    Agent::builder()
        .config_file(OPENROUTER_GPT54NANO_CONFIG)
        .root(root.display().to_string())
        .tools(tools.iter().copied())
        .store("memory")
        .agent_md(false)
        .max_steps(max_steps)
        .build()
        .await
        .expect("build live OpenRouter SDK agent")
}

fn assert_trimmed_eq(actual: &str, expected: &str) {
    assert_eq!(actual.trim(), expected);
}

fn assert_trimmed_eq_ignore_case(actual: &str, expected: &str) {
    assert!(
        actual.trim().eq_ignore_ascii_case(expected),
        "expected {expected:?}, got {actual:?}"
    );
}

fn assert_last_non_empty_line_trimmed_eq(actual: &str, expected: &str) {
    let last_line = actual
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    assert_eq!(last_line, expected, "full output was {actual:?}");
}

fn assert_first_normalized_word_eq_ignore_case(actual: &str, expected: &str) {
    let first = actual.trim().split_whitespace().next().unwrap_or("");
    let normalized =
        first.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
    assert!(
        normalized.eq_ignore_ascii_case(expected),
        "expected first normalized word {expected:?}, got {actual:?}"
    );
}

fn tool_start_sequence(response: &muagent::sdk::AgentResponse) -> Vec<String> {
    response
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::CoreEvent {
                event: Event::ToolCallStart { tool, .. },
            } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

fn successful_tool_names(response: &muagent::sdk::AgentResponse) -> BTreeSet<String> {
    let mut tool_by_call = BTreeMap::new();
    let mut ok_tools = BTreeSet::new();

    for event in &response.events {
        match event {
            AgentEvent::CoreEvent {
                event: Event::ToolCallStart { call_id, tool, .. },
            } => {
                tool_by_call.insert(call_id.clone(), tool.clone());
            }
            AgentEvent::CoreEvent {
                event:
                    Event::ToolCallEnd {
                        call_id, ok: true, ..
                    },
            } => {
                if let Some(tool) = tool_by_call.get(call_id) {
                    ok_tools.insert(tool.clone());
                }
            }
            _ => {}
        }
    }

    ok_tools
}

fn assert_successful_tool_used(response: &muagent::sdk::AgentResponse, tool: &str) {
    let tools = successful_tool_names(response);
    assert!(
        tools.contains(tool),
        "expected successful {tool} call, got successful tools {tools:?}"
    );
}

#[tokio::test]
async fn sdk_runs_fs_roundtrip_exact_case() {
    let tmp = TempDir::new("muagent-sdk-bench-roundtrip");
    let target = tmp.path().join("outbox").join("greeting.txt");
    let expected = "alpha beta 1729";

    let replies = vec![
        reply::with_calls(
            "write file",
            vec![pc(
                "write_greeting",
                "fs_write",
                json!({
                    "uri": file_uri(&target),
                    "content": expected,
                    "create_dirs": true,
                }),
            )],
        ),
        reply::with_calls(
            "read file back",
            vec![pc(
                "read_greeting",
                "fs_read",
                json!({
                    "uri": file_uri(&target),
                }),
            )],
        ),
        reply::text(expected),
    ];
    let mut agent = sdk_agent(tmp.path(), replies, None);
    let mut seen_tool_end = false;

    let response = agent
        .query_with_events(
            format!(
                "Write the exact text `{expected}` to {}. Then read the file back and return only the file contents.",
                file_uri(&target)
            ),
            |event| {
                if let AgentEvent::CoreEvent {
                    event: Event::ToolCallEnd { call_id, ok, .. },
                } = event
                {
                    if call_id == "read_greeting" && ok {
                        seen_tool_end = true;
                    }
                }
            },
        )
        .await
        .expect("sdk query");

    assert_eq!(response.final_text, expected);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
    assert!(seen_tool_end, "SDK should surface core tool events");
}

#[tokio::test]
async fn sdk_runs_subagent_as_tool_case() {
    let tmp = TempDir::new("muagent-sdk-bench-subagent");
    let replies = vec![
        reply::with_calls(
            "delegate review",
            vec![pc(
                "delegate_reviewer",
                SUBAGENT_TOOL_NAME,
                json!({
                    "subagent": "reviewer",
                    "task": "Check whether Nina Patel's code is DXB-442.",
                }),
            )],
        ),
        reply::text("delegated"),
    ];
    let reviewer = AgentDefinition::new(
        "reviewer",
        "Checks contact data",
        "Return whether the requested contact fact is correct.",
    )
    .tools(["fs_read"]);
    let mut agent = sdk_agent_with_registry(
        tmp.path(),
        replies,
        Some(vec![SUBAGENT_TOOL_NAME.into()]),
        |registry| {
            registry.register(Arc::new(SubagentTool::new(
                vec![reviewer],
                Arc::new(StaticSubagentExecutor),
            )));
        },
    );

    let response = agent
        .query("Ask the reviewer subagent to check Nina Patel's code. Return only delegated.")
        .await
        .expect("subagent tool query");

    assert_eq!(response.final_text, "delegated");
    assert_eq!(tool_start_sequence(&response), vec![SUBAGENT_TOOL_NAME]);
    assert_successful_tool_used(&response, SUBAGENT_TOOL_NAME);
    let detail = response
        .events
        .iter()
        .find_map(|event| match event {
            AgentEvent::CoreEvent {
                event: Event::ToolCallEnd { detail, .. },
            } if detail.is_object() => Some(detail),
            _ => None,
        })
        .expect("subagent detail");
    assert_eq!(detail["agent_name"], "reviewer");
    assert_eq!(
        detail["final_text"],
        "subagent handled: Check whether Nina Patel's code is DXB-442."
    );
    assert_eq!(response.usage.tokens_prompt, 37);
    assert_eq!(response.usage.tokens_completion, 18);
    assert_eq!(response.usage.turns, 4);
    assert_eq!(response.usage.tool_calls, 2);
    assert_eq!(response.usage.cost_usd, 0.25);
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_subagent_as_tool_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-subagent");
    let contacts_path = tmp.path().join("contacts.json");
    write_contacts_fixture(&contacts_path);

    let reviewer = AgentDefinition::new(
        "reviewer",
        "Reads the contact file and verifies requested contact facts.",
        "Use the available file read tool when a file path is provided. Return only the exact requested value.",
    )
    .tools(["fs_read"]);
    let mut agent = Agent::builder()
        .config_file(OPENROUTER_GPT54NANO_CONFIG)
        .root(tmp.path().display().to_string())
        .tools([SUBAGENT_TOOL_NAME])
        .subagent(reviewer)
        .store("memory")
        .agent_md(false)
        .max_steps(140)
        .build()
        .await
        .expect("build live OpenRouter subagent SDK agent");

    let response = agent
        .query(format!(
            "Use the `reviewer` subagent with the `spawn_sub_agent` tool to read {} and find the entry for `Nina Patel`. Return only its `code` value.",
            file_uri(&contacts_path)
        ))
        .await
        .expect("live subagent query");

    eprintln!(
        "subagent parent tools: {:?}",
        tool_start_sequence(&response)
    );
    eprintln!("subagent parent answer: {:?}", response.final_text);

    assert_trimmed_eq(&response.final_text, "DXB-442");
    assert_eq!(tool_start_sequence(&response), vec![SUBAGENT_TOOL_NAME]);
    assert_successful_tool_used(&response, SUBAGENT_TOOL_NAME);
    let detail = response
        .events
        .iter()
        .find_map(|event| match event {
            AgentEvent::CoreEvent {
                event: Event::ToolCallEnd { detail, ok, .. },
            } if *ok && detail.is_object() => Some(detail),
            _ => None,
        })
        .expect("subagent tool detail");
    eprintln!("subagent detail: {detail}");
    assert_eq!(detail["agent_name"], "reviewer");
    assert_trimmed_eq(detail["final_text"].as_str().unwrap_or_default(), "DXB-442");
    assert!(
        detail["usage"]["tool_calls"].as_u64().unwrap_or(0) >= 1,
        "subagent should use fs_read, detail was {detail}"
    );
}

#[tokio::test]
async fn sdk_runs_two_turn_contact_memory_case() {
    let tmp = TempDir::new("muagent-sdk-bench-memory");
    let contacts_path = tmp.path().join("contacts.json");
    write_contacts_fixture(&contacts_path);

    let replies = vec![
        reply::with_calls(
            "read contacts",
            vec![pc(
                "read_contacts",
                "fs_read",
                json!({
                    "uri": file_uri(&contacts_path),
                }),
            )],
        ),
        reply::text("DXB-442"),
        reply::text("dubai"),
    ];
    let mut agent = sdk_agent(tmp.path(), replies, None);

    let first = agent
        .query(format!(
            "Find the entry for `Nina Patel` in {}. Return only its `code` value.",
            file_uri(&contacts_path)
        ))
        .await
        .expect("first turn");
    let second = agent
        .query("Now return only the same entry's city value in lowercase.")
        .await
        .expect("second turn");

    assert_eq!(first.final_text, "DXB-442");
    assert_eq!(second.final_text, "dubai");
    assert_eq!(agent.state().usage.turns, 3);
    assert_eq!(
        agent
            .state()
            .history
            .iter()
            .filter(|msg| matches!(msg, Message::User { .. }))
            .count(),
        2
    );
    assert!(agent.state().history.len() >= 5);
}

#[tokio::test]
async fn sdk_team_runs_multi_turn_handoff_case() {
    let tmp = TempDir::new("muagent-sdk-bench-team");
    let contacts_path = tmp.path().join("contacts.json");
    write_contacts_fixture(&contacts_path);

    let researcher_replies = vec![
        reply::with_calls(
            "read contacts",
            vec![pc(
                "research_contacts",
                "fs_read",
                json!({
                    "uri": file_uri(&contacts_path),
                }),
            )],
        ),
        reply::text("DXB-442"),
        reply::text("dubai"),
    ];
    let reviewer_replies = vec![
        reply::with_calls(
            "review contacts",
            vec![pc(
                "review_contacts",
                "fs_read",
                json!({
                    "uri": file_uri(&contacts_path),
                }),
            )],
        ),
        reply::text("approved: DXB-442"),
    ];

    let researcher = sdk_agent(tmp.path(), researcher_replies, Some(vec!["fs_read".into()]));
    let reviewer = sdk_agent(tmp.path(), reviewer_replies, Some(vec!["fs_read".into()]));
    let mut team = AgentTeam::new()
        .with_agent("researcher", researcher)
        .with_agent("reviewer", reviewer);

    let mut route_trace = Vec::new();
    let first = team
        .query(
            "researcher",
            format!(
                "Find the entry for `Nina Patel` in {}. Return only its `code` value.",
                file_uri(&contacts_path)
            ),
        )
        .await
        .expect("researcher first turn");
    route_trace.push("researcher:first");
    let review = team
        .query(
            "reviewer",
            format!(
                "Verify this researcher result for Nina Patel's code by reading {}: `{}`. Return `approved: CODE` if it is correct.",
                file_uri(&contacts_path),
                first.final_text
            ),
        )
        .await
        .expect("reviewer turn");
    route_trace.push("reviewer");
    let second = team
        .query(
            "researcher",
            "Now return only the same entry's city value in lowercase.",
        )
        .await
        .expect("researcher second turn");
    route_trace.push("researcher:second");

    assert_eq!(
        route_trace,
        vec!["researcher:first", "reviewer", "researcher:second"]
    );
    assert_eq!(first.final_text, "DXB-442");
    assert_eq!(review.final_text, "approved: DXB-442");
    assert_eq!(second.final_text, "dubai");
    assert_eq!(tool_start_sequence(&first), vec!["fs_read"]);
    assert_eq!(tool_start_sequence(&review), vec!["fs_read"]);
    assert_eq!(tool_start_sequence(&second), Vec::<String>::new());
    assert_successful_tool_used(&first, "fs_read");
    assert_successful_tool_used(&review, "fs_read");
    assert_eq!(
        team.agent("researcher")
            .unwrap()
            .state()
            .history
            .iter()
            .filter(|msg| matches!(msg, Message::User { .. }))
            .count(),
        2
    );
    assert_eq!(
        team.agent("reviewer")
            .unwrap()
            .state()
            .history
            .iter()
            .filter(|msg| matches!(msg, Message::User { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn sdk_runs_rename_report_file_case_with_selected_tools() {
    let tmp = TempDir::new("muagent-sdk-bench-rename");
    let source = tmp.path().join("draft").join("report.txt");
    let target = tmp.path().join("final").join("report.txt");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&source, "ship checklist complete\n").unwrap();

    let replies = vec![
        reply::with_calls(
            "rename report",
            vec![pc(
                "rename_report",
                "fs_rename",
                json!({
                    "from": file_uri(&source),
                    "to": file_uri(&target),
                }),
            )],
        ),
        reply::text("report.txt"),
    ];
    let mut agent = sdk_agent(
        tmp.path(),
        replies,
        Some(vec!["fs_read".into(), "fs_rename".into()]),
    );

    let response = agent
        .query(format!(
            "Rename {} to {}. After renaming, return only the destination path's filename.",
            file_uri(&source),
            file_uri(&target)
        ))
        .await
        .expect("rename turn");

    assert_eq!(response.final_text, "report.txt");
    assert!(!source.exists());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "ship checklist complete\n"
    );
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_fs_roundtrip_config_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-roundtrip");
    let target = tmp.path().join("outbox").join("greeting.txt");
    let expected = "alpha beta 1729";

    let mut agent = live_openrouter_agent(tmp.path(), 80, &["fs_write", "fs_read"]).await;

    let mut tool_calls = 0usize;
    let response = agent
        .query_with_events(
            format!(
                "Write the exact text `{expected}` to {}. Then read the file back and return only the file contents. Return only the file contents.",
                file_uri(&target)
            ),
            |event| {
                if matches!(
                    event,
                    AgentEvent::CoreEvent {
                        event: Event::ToolCallEnd { ok: true, .. }
                    }
                ) {
                    tool_calls += 1;
                }
            },
        )
        .await
        .expect("live OpenRouter SDK query");

    assert_trimmed_eq(&response.final_text, expected);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
    assert!(tool_calls >= 2, "expected fs_write and fs_read tool calls");
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_log_error_count_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-logs");
    let log_path = tmp.path().join("logs").join("app.log");
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
    write_text_file(&log_path, &format!("{lines}\n"));

    let mut agent = live_openrouter_agent(tmp.path(), 80, &["fs_read"]).await;
    let response = agent
        .query(format!(
            "Use the available file read tool to read {}. In the file contents, count how many lines contain the exact uppercase substring `ERROR`. Lines containing lowercase `error` do not count. Return only the integer count.",
            file_uri(&log_path)
        ))
        .await
        .expect("live log count query");

    assert_trimmed_eq(&response.final_text, "4");
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_csv_best_region_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-csv");
    let csv_path = tmp.path().join("sales.csv");
    let body = "\
region,revenue,refunds\n\
west,100,10\n\
west,60,0\n\
north,70,10\n\
north,90,20\n\
east,120,40\n\
east,50,5\n\
south,130,15\n";
    write_text_file(&csv_path, body);

    let mut agent = live_openrouter_agent(tmp.path(), 80, &["fs_read"]).await;
    let response = agent
        .query(format!(
            "In {} each row has region,revenue,refunds. Compute each region's total net revenue where net = revenue - refunds. Return only the region with the highest total net revenue.",
            file_uri(&csv_path)
        ))
        .await
        .expect("live csv query");

    assert_trimmed_eq_ignore_case(&response.final_text, "west");
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_invoice_total_nested_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-invoices");
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
        write_text_file(&tmp.path().join(rel), body);
    }

    let mut agent = live_openrouter_agent(tmp.path(), 100, &["fs_list", "fs_read"]).await;
    let response = agent
        .query(format!(
            "Inside {} there are JSON invoice files in nested folders. Compute the total `amount` for every invoice whose `status` is `paid`. Return only the integer total.",
            file_uri(tmp.path())
        ))
        .await
        .expect("live invoice query");

    assert_trimmed_eq(&response.final_text, "37");
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_two_turn_contact_memory_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-memory");
    let contacts_path = tmp.path().join("contacts.json");
    write_contacts_fixture(&contacts_path);

    let mut agent = live_openrouter_agent(tmp.path(), 100, &["fs_read"]).await;
    let first = agent
        .query(format!(
            "Find the entry for `Nina Patel` in {}. Return only its `code` value.",
            file_uri(&contacts_path)
        ))
        .await
        .expect("live contact first turn");
    let second = agent
        .query("Now return only the same entry's city value in lowercase.")
        .await
        .expect("live contact second turn");

    assert_trimmed_eq(&first.final_text, "DXB-442");
    assert_trimmed_eq(&second.final_text, "dubai");
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_team_multi_turn_handoff_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-team");
    let contacts_path = tmp.path().join("contacts.json");
    write_contacts_fixture(&contacts_path);

    let researcher = live_openrouter_agent(tmp.path(), 100, &["fs_read"]).await;
    let reviewer = live_openrouter_agent(tmp.path(), 100, &["fs_read"]).await;
    let mut team = AgentTeam::new()
        .with_agent("researcher", researcher)
        .with_agent("reviewer", reviewer);

    let mut route_trace = Vec::new();
    let first = team
        .query(
            "researcher",
            format!(
                "Find the entry for `Nina Patel` in {}. Return only its `code` value.",
                file_uri(&contacts_path)
            ),
        )
        .await
        .expect("live team researcher first turn");
    route_trace.push("researcher:first");
    let review = team
        .query(
            "reviewer",
            format!(
                "Do not trust the provided answer; check {}. A researcher returned `{}` as Nina Patel's code. Return only `approved` if the file confirms that exact code, otherwise return only `rejected`.",
                file_uri(&contacts_path),
                first.final_text.trim()
            ),
        )
        .await
        .expect("live team reviewer turn");
    route_trace.push("reviewer");
    let second = team
        .query(
            "researcher",
            "Now return only the same entry's city value in lowercase.",
        )
        .await
        .expect("live team researcher second turn");
    route_trace.push("researcher:second");

    eprintln!("team route: {route_trace:?}");
    eprintln!("researcher first tools: {:?}", tool_start_sequence(&first));
    eprintln!("reviewer tools: {:?}", tool_start_sequence(&review));
    eprintln!(
        "researcher second tools: {:?}",
        tool_start_sequence(&second)
    );
    eprintln!("researcher first answer: {:?}", first.final_text);
    eprintln!("reviewer answer: {:?}", review.final_text);
    eprintln!("researcher second answer: {:?}", second.final_text);

    assert_eq!(
        route_trace,
        vec!["researcher:first", "reviewer", "researcher:second"]
    );
    assert_trimmed_eq(&first.final_text, "DXB-442");
    assert_first_normalized_word_eq_ignore_case(&review.final_text, "approved");
    assert_last_non_empty_line_trimmed_eq(&second.final_text, "dubai");
    assert_successful_tool_used(&first, "fs_read");
    assert_successful_tool_used(&review, "fs_read");
    assert!(
        team.agent("researcher")
            .unwrap()
            .state()
            .history
            .iter()
            .filter(|msg| matches!(msg, Message::User { .. }))
            .count()
            >= 2
    );
    assert!(
        team.agent("reviewer")
            .unwrap()
            .state()
            .history
            .iter()
            .filter(|msg| matches!(msg, Message::User { .. }))
            .count()
            >= 1
    );
}

#[tokio::test]
#[ignore = "live OpenRouter test; requires OPENROUTER_API_KEY or .env"]
async fn sdk_live_openrouter_gpt54nano_runs_rename_report_file_case() {
    let tmp = TempDir::new("muagent-sdk-live-openrouter-rename");
    let source = tmp.path().join("draft").join("report.txt");
    let target = tmp.path().join("final").join("report.txt");
    write_text_file(&source, "ship checklist complete\n");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();

    let mut agent = live_openrouter_agent(tmp.path(), 100, &["fs_rename"]).await;
    let response = agent
        .query(format!(
            "Rename {} to {}. After renaming, return only the destination path's filename.",
            file_uri(&source),
            file_uri(&target)
        ))
        .await
        .expect("live rename query");

    assert_trimmed_eq(&response.final_text, "report.txt");
    assert!(!source.exists());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "ship checklist complete\n"
    );
}
