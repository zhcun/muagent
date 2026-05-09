# μAgent Rust SDK

`muagent::sdk` is a high-level wrapper around the existing core runner. It
does not change `src/core`; it packages the configured model, tools, store,
and session state into a stateful `Agent` API for embedding μAgent in another
Rust application.

## Quick Start

```rust
use muagent::sdk::{Agent, AgentEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut agent = Agent::builder()
        .provider("openai")
        .model("gpt-5.4-nano")
        .tools(["fs_read", "fs_list", "sh_exec"])
        .disable_tools(["fs_delete"])
        .store("memory")
        .build()
        .await?;

    let output = agent
        .query_with_events("Summarize this repository.", |event| {
            if let AgentEvent::AssistantText { text } = event {
                print!("{text}");
            }
        })
        .await?;

    println!("\n\nFinal answer:\n{}", output.final_text);
    Ok(())
}
```

The builder uses the same `Config` and `setup::wire` pipeline as the CLI, so
`~/.muagent/config.toml`, `.env`, provider environment variables, skills,
tools, MCP SSE endpoints, JSONL stores, compaction, retries, and model
capability overrides work the same way.

## Tools, Skills, And Instructions

Tool selection works like the CLI allowlist/denylist:

```rust
let mut agent = Agent::builder()
    .tools(["fs_read", "fs_list"])      // expose only these tools
    .disable_tools(["sh_exec"])         // additionally hide/reject these
    .skills(["git"])                    // expose only these skills
    .disable_skills(["legacy"])         // hide these skills
    .no_skills_autoload()               // skip ./.muagent/skills + ~/.muagent/skills
    .mcp_sse(["http://127.0.0.1:3000/sse"])
    .agent_md(true)
    .agent_md_max_bytes(32 * 1024)
    .build()
    .await?;
```

`agent_md(true)` enables the same project instruction loader used by the CLI:
`AGENT.md`, `AGENTS.md`, `agent.md`, `agents.md`, `CLAUDE.md`, and `claude.md`
from the workspace ancestry and user config directories. You can also pass a
full config file with `.config_file("./muagent.toml")` or a typed `Config` with
`.config(config)`.

## Main Types

- `AgentBuilder`: SDK configuration entry point. Use direct builder methods
  for common overrides or pass a prepared `Config`/`ConfigOverrides`.
- `Agent`: stateful runtime facade. Consecutive `query` calls continue the
  current run state.
- `AgentEvent`: SDK event stream. `CoreEvent` wraps core step events returned
  by `Runner`; `AssistantText` carries non-persistent streaming model deltas.
- `AgentResponse`: final text, session/run ids, usage, and all events captured
  during the query.
- `SdkError`: SDK-layer error type that preserves core submit/step/store
  errors where possible.

## Common Calls

```rust
let response = agent.query("Write a changelog.").await?;
println!("{}", response.final_text);

agent.new_session();

agent.continue_session(session_id).await?;
agent.fork_from(run_id, 12).await?;

let sessions = agent.list_sessions(Some(20)).await?;
let hits = agent.search_sessions("release notes", Some(10)).await?;
```

## Hooks And Policy

SDK builders can attach in-process lifecycle hooks. This reuses the same core
hook protocol documented in [HOOKS.md](HOOKS.md), so hosts can enforce policy,
audit tool calls, inject context, or block unsafe operations without relying on
the model to call a tool voluntarily.

```rust
use std::sync::Arc;
use async_trait::async_trait;
use muagent::core::prelude::*;
use muagent::sdk::Agent;

struct PolicyHooks;

#[async_trait]
impl HookDispatcher for PolicyHooks {
    async fn dispatch(&self, input: HookInput, _cancel: CancelToken) -> HookOutput {
        if input.hook_event_name == HookEventName::PreToolUse
            && input.tool_name.as_deref() == Some("sh_exec")
        {
            return HookOutput {
                hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                    permission_decision: Some(HookPermissionDecision::Deny),
                    permission_decision_reason: Some("shell disabled by host policy".into()),
                }),
                ..Default::default()
            };
        }
        HookOutput::default()
    }
}

let mut agent = Agent::builder()
    .hooks(Arc::new(PolicyHooks))
    .build()
    .await?;
```

## Subagents

Subagents are specialized agent definitions that can be exposed to the parent
agent as the default `spawn_sub_agent` tool. This is enabled when at least one
subagent is configured. The parent passes a task string; the subagent runs in a
fresh, isolated conversation and returns only its final text as the tool result.

```rust
use muagent::core::prelude::AgentDefinition;
use muagent::sdk::Agent;

let reviewer = AgentDefinition::new(
    "reviewer",
    "Reviews code changes for correctness issues.",
    "Read the relevant files and report concrete bugs only.",
)
.tools(["fs_read", "fs_list"])
.max_steps(200);

let mut agent = Agent::builder()
    .subagent(reviewer)
    .tools(["fs_read", "fs_list"]) // `spawn_sub_agent` is added automatically
    .build()
    .await?;
```

File-backed subagents are loaded from `~/.muagent/agents/*.md` and
`<workspace>/.muagent/agents/*.md`:

```markdown
---
name: reviewer
description: Reviews code changes for correctness issues.
tools: fs_read, fs_list
max_steps: 200
---
Read the relevant files and report concrete bugs only.
```

Set `subagents.enabled = false` in config or call `.no_subagent_tools()` to
disable the `spawn_sub_agent` tool.

Subagent calls are deliberately one level deep: subagents never receive the
`spawn_sub_agent` tool, even if it is inherited or listed explicitly. One parent
agent can run at most 8 subagent calls concurrently.

## Multiple Agents

The SDK does not provide a built-in multi-agent container. If a host wants to
coordinate several `Agent` instances itself, hold them as ordinary Rust values
and call `agent.query(...)` directly — each `Agent` retains its own session
state across calls.

For LLM-driven multi-agent setups (one agent spawns and supervises others as
subprocesses, with file-based status), see
[team/DESIGN.md](team/DESIGN.md). That layer exposes two opt-in tools
(`worker_admin`, `agent_msg`) so the orchestrator's model decides who works,
not the host program.

For advanced hosts that wire their own `ModelAdapter`, `ToolExecutor`, or
`SessionStore`, use `Agent::from_parts(runner, state)` and keep using core
traits directly.
