# Stream JSON Output For `muagent exec`

This document specifies a structured stdout format for `muagent exec` (and
`muagent exec resume`) so host applications can drive μAgent as a backend
and observe a turn end-to-end. The bare `muagent` command keeps its
default TUI behavior; this proposal only affects `exec` mode.

The spec is written from a concrete integration: Thorb (a local-first
personal CFO) wants to use μAgent as a third AI backend alongside Claude
Code, Codex, and Gemini CLI. Those three already expose a streaming JSON
output mode, and the host adapter parses one JSON event per line. Without
an equivalent in μAgent, the host either has to tail the on-disk session
JSONL (delayed, lossy for streaming text) or fall back to the final-text-
only stdout that `muagent exec` prints today.

## Goal

```bash
muagent exec --output-format stream-json "Summarize this repo."
```

emits one JSON event per line on stdout until the turn finishes.

## Required: The Flag

A new flag on `muagent exec` and `muagent exec resume`:

| Flag | Behavior |
|---|---|
| `--output-format stream-json` | Switch stdout from human-readable text to an NDJSON event stream |

Stream invariants:

- **NDJSON**: one JSON object per line, terminated with `\n`. No
  pretty-printing, no multi-line records.
- **stdout is event-only**: all logs, progress, warnings, debug output go
  to stderr. A single non-JSON byte on stdout breaks every host parser.
- **Flush per event**: each line is flushed at emission. Without this,
  the host cannot present streaming progress.
- **Order is meaningful**: events arrive in emission order; hosts use
  arrival order, not timestamps.
- **Forward compatibility**: hosts ignore unknown fields and unknown
  event types. Adding fields or new event types is non-breaking;
  renaming or removing existing ones is breaking.

The TUI, the line REPL, and `muagent` (no subcommand) are unaffected.

## Required: The Minimum Event Set

Four event types are sufficient for a host to render a complete turn.
This is the same floor that the Codex backend already provides to Thorb
today, so the bar is not arbitrary.

### `session_started`

If `muagent exec` generates the session id internally (analogous to
Codex's `thread.started` and Gemini's `init`), the **first line** must
carry it:

```json
{"type": "session_started", "session_id": "abc123", "resumed": false}
```

If μAgent instead accepts a host-supplied session id via a CLI flag
(analogous to Claude Code's `--session-id`), this event can be omitted —
the host already knows the id. Pick one model. The current μAgent CLI
generates ids internally, so the first-line event is the natural fit.

The exact field name (`session_started` vs `thread.started` vs `init`)
does not matter; what matters is: **first line of the stream, carries a
stable session id string**.

### `assistant_text`

Streaming assistant text. Zero or more events per turn; the host
concatenates them in arrival order.

```json
{"type": "assistant_text", "text": "Looking at "}
{"type": "assistant_text", "text": "the repository structure..."}
```

The corresponding internal signal already exists as
`TuiUpdate::AssistantDelta` (see `src/cli_app/driver.rs`).

### `result`

The terminal event for the success path. Exactly one per turn.

```json
{
  "type": "result",
  "final_text": "...",
  "is_error": false,
  "session_id": "abc123",
  "cost_usd": null,
  "usage": null
}
```

`cost_usd` and `usage` are nullable; populate them when the provider
reports them.

### `error`

The terminal event for the failure path. Replaces `result` on errors.

```json
{
  "type": "error",
  "message": "...",
  "stage": "submit"
}
```

`stage` is a coarse classifier — `submit`, `step`, `tool`, `provider`,
`cancelled`, or similar — that lets hosts decide whether to retry or
surface the failure to the user.

A turn ends with **exactly one** of `result` or `error`, then the
process exits. The process exit code does not need to encode the error;
the host trusts the event stream.

## Optional: Tool Events

Tool events are not part of the minimum. Codex's existing integration
with Thorb has no tool events and the backend works fine; the user
simply does not see a "running tool" panel in the chat UI.

If μAgent emits them, hosts can render the richer UI. Suggested schema:

```json
{
  "type": "tool_call_start",
  "tool_call_id": "call_42",
  "tool_name": "sh_exec",
  "input": {"command": "ls -la"}
}
```

```json
{
  "type": "tool_call_result",
  "tool_call_id": "call_42",
  "ok": true,
  "output": "...",
  "error": null
}
```

Constraints:

- `tool_call_id` matches between `start` and `result`.
- `input` is the raw tool input object. Hosts inspect fields like
  `command` for semantic labelling (e.g. mapping `thorb add-txn ...`
  to "Add transaction").
- `tool_name` matches μAgent's existing built-in names (`fs_read`,
  `fs_write`, `fs_edit`, `fs_list`, `fs_stat`, `fs_delete`,
  `fs_rename`, `sh_exec`). Renaming these is a breaking change for
  any host that relies on tool name to render or classify.

The internal sources are already there: `TuiUpdate::ToolStart` and
`TuiUpdate::Tool` in `src/cli_app/driver.rs`.

## Out Of Scope

This spec deliberately does not request:

- **Hooks over stdin/stdout.** Hosts that need policy enforcement
  apply it at the OS sandbox layer (process-level filesystem and
  exec restrictions). The existing in-process Rust `HookDispatcher`
  does not need a CLI shim for this integration.
- **Skill content interpretation.** Hosts mount skill directories
  under `--root`/`./.muagent/skills/`; μAgent's existing autodiscovery
  is sufficient.
- **Replacing the TUI or REPL.** This spec only affects `muagent exec`
  and `muagent exec resume`.
- **Image input semantics.** `--image` keeps its current behavior.
- **Token / usage events mid-turn.** Useful, but not required for a
  working backend; can be added later as a non-breaking addition.

## Delivery Order

If the work has to ship in slices:

1. `--output-format stream-json` flag + `assistant_text` + `result`
   + `error`. This alone unblocks host integration at Codex parity.
2. `session_started` first-line event (if μAgent keeps internal-id
   generation).
3. Tool events (`tool_call_start` / `tool_call_result`).
4. Token / usage event for richer UI surfaces.

After step 1 a host can run `muagent exec` as an AI backend end-to-end.
