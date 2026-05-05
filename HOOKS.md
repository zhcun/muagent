# μAgent Hooks

μAgent exposes Codex-style lifecycle hooks at the core runner boundary. Hooks
are meant for host applications and embedded users that need deterministic
policy, audit, context injection, or tool-result shaping without relying on the
model to call a tool voluntarily.

The current implementation is a core API, not a CLI hook runner. Core defines
the typed protocol and calls the hook dispatcher at stable lifecycle points.
Hosts can adapt that API to in-process callbacks, command hooks, remote policy
engines, or compatibility layers for Codex / Claude Code style hook files.

## What Core Provides

- A `HookDispatcher` trait in `muagent::core::hook`.
- A no-op default dispatcher, so existing runners keep the same behavior.
- Codex-style event names and JSON field names.
- Runner integration for `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, and `Stop`.
- Typed hook outcomes for blocking, approving, adding context, replacing tool
  results, and requesting one more model turn.

`PermissionRequest` is intentionally not implemented. μAgent's default posture
is higher automation: policy decisions should happen in `PreToolUse`, static
tool filters, tool guards, sandboxing, and host-level dispatchers rather than a
human approval pause.

## Lifecycle

| Event | When it runs | What it can do |
|---|---|---|
| `SessionStart` | Before the first submitted user message in a new run. A resumed run reports `source = "resume"`. | Add `Observation { kind: Steering }` context or block the run before user input is stored. |
| `UserPromptSubmit` | After `SessionStart`, before the user message is committed. | Block the prompt or add steering context after the user message. |
| `PreToolUse` | After the model emits a tool call and before executor dispatch. Internal protocol-repair pseudo-tools skip this hook. | Deny the call. Denial becomes a non-retryable `ToolResult`; the real tool is not executed. |
| `PostToolUse` | After a real tool returns and before the result is committed to history. | Replace the result with a hook error, or append additional model-visible context. |
| `Stop` | When the model returns final text with no tool calls, before `SessionEnd`. | Request another model turn by returning `decision = "block"` with a `reason`; optionally add steering context. |

All hook panics are caught by the runner. A panicking dispatcher is logged and
treated as no-op for that hook call.

## Rust Integration

Attach a dispatcher with `RunnerBuilder::hooks`. `RunnerBuilder::hook_model`
sets the model label that appears in hook input JSON; setup code should pass
the configured model id when it knows it.

```rust
use std::sync::Arc;

use async_trait::async_trait;
use muagent::core::prelude::*;

struct PolicyHooks;

#[async_trait]
impl HookDispatcher for PolicyHooks {
    async fn dispatch(&self, input: HookInput, _cancel: CancelToken) -> HookOutput {
        match input.hook_event_name {
            HookEventName::PreToolUse if input.tool_name.as_deref() == Some("sh_exec") => {
                HookOutput {
                    hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                        permission_decision: Some(HookPermissionDecision::Deny),
                        permission_decision_reason: Some(
                            "shell execution is disabled by host policy".into(),
                        ),
                    }),
                    ..Default::default()
                }
            }
            HookEventName::UserPromptSubmit => HookOutput {
                hook_specific_output: Some(HookSpecificOutput::UserPromptSubmit {
                    additional_context: Some(
                        "Host policy: prefer read-only inspection before edits.".into(),
                    ),
                }),
                ..Default::default()
            },
            _ => HookOutput::default(),
        }
    }
}

let runner = Runner::builder()
    .model(model)
    .tools(tools)
    .store(store)
    .tools_provider(provider)
    .hooks(Arc::new(PolicyHooks))
    .hook_model("openai/gpt-5.4-nano")
    .build()?;
```

## Hook Input

`HookInput` is serializable and uses Codex-style field names:

```json
{
  "session_id": "3c75...",
  "run_id": "6b66...",
  "transcript_path": null,
  "cwd": "/workspace/project",
  "hook_event_name": "PreToolUse",
  "model": "openai/gpt-5.4-nano",
  "turn_id": "2",
  "source": null,
  "prompt": null,
  "tool_name": "fs_read",
  "tool_use_id": "call_123",
  "tool_input": { "uri": "src/lib.rs" },
  "tool_response": null,
  "stop_hook_active": null,
  "last_assistant_message": null
}
```

Fields are optional when they do not apply to the event:

- `source`: `startup`, `resume`, or `clear` for `SessionStart`.
- `prompt`: plain-text projection of the submitted user message for
  `UserPromptSubmit`.
- `tool_name`, `tool_use_id`, and `tool_input`: present for tool hooks.
- `tool_response`: serialized `ToolResult`, present for `PostToolUse`.
- `last_assistant_message`: final assistant text, present for `Stop`.
- `transcript_path`: reserved for host adapters that expose a transcript file;
  core currently leaves it unset.

## Hook Output

The default output is allow-and-continue:

```json
{
  "continue": true,
  "suppressOutput": false
}
```

Block a submitted prompt or session start:

```json
{
  "continue": false,
  "stopReason": "workspace is read-only"
}
```

Deny a tool call before execution:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "shell execution is disabled"
  }
}
```

Add prompt/session steering context:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "Prefer read-only tools before mutating files."
  }
}
```

Append context to a tool result:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "The file is generated; do not hand-edit it."
  }
}
```

Ask the model to continue instead of ending:

```json
{
  "decision": "block",
  "reason": "Add a concise verification summary before stopping."
}
```

For `Stop`, `decision = "block"` keeps the run active by injecting the reason
as a synthetic user continuation and returning to `Ready`. The next `step()`
will call the model again.

## Semantics And Safety

- Hooks are deterministic lifecycle gates. They should not be used for normal
  model-visible capabilities; expose those as tools instead.
- `PreToolUse` is the place for policy denial. It runs before at-most-once tool
  intent persistence, so a denied call is never treated as an interrupted
  side-effectful execution.
- `PostToolUse` cannot undo side effects. It can only replace or augment the
  `ToolResult` that will be shown to the model and persisted in history.
- Additional context is persisted as `Observation { kind: Steering }` for
  session/prompt/stop hooks, and appended to tool-result content for
  `PostToolUse`.
- Hook dispatch receives a `CancelToken`. Command-hook adapters should honor it
  and should add their own timeouts.
- Core does not execute hook commands, read hook config, or trust project-local
  files. Those are host responsibilities.

## Compatibility Notes

The protocol intentionally mirrors Codex hook event names and common JSON
fields (`hook_event_name`, `tool_name`, `tool_use_id`, `tool_input`,
`tool_response`, `hookSpecificOutput`, `permissionDecision`). A host can map
these typed structs to command stdin/stdout JSON with minimal translation.

Claude Code has a similar lifecycle model, especially around
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`, but its matcher and
command configuration format should live in an adapter layer rather than core.

## Tests

Focused coverage lives in `tests/m0_core.rs`:

- session and prompt hooks add steering context;
- `PreToolUse` denial skips executor dispatch;
- `PostToolUse` can replace a tool result;
- `Stop` can request one more model turn.

Run the hook/core tests with:

```bash
cargo test --test m0_core hook
```
