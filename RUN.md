# Runbook

This file is a short command reference for building, testing, running, and
troubleshooting the current checkout. User-facing CLI usage is in
[USAGE.md](USAGE.md).

## Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustc --version      # expected: 1.75 or newer
```

## Build And Test

```bash
cd /Users/mike/jiang/muagent
cargo build
cargo test
```

Expected result:

- the `muagent` crate builds
- non-ignored tests pass
- live provider tests stay ignored unless explicitly requested with credentials

## Run Focused Tests

```bash
cargo test -p muagent --test m0_core t1_tool_batch_multi_call_sequential
cargo test -p muagent --test m0_shell t6_panic_becomes_tool_result
cargo test --test cli_smoke
```

## Run The CLI From Source

```bash
cargo run --bin muagent -- --help
cargo run --bin muagent -- exec "Summarize this repository."
cargo run --bin muagent -- repl
```

## Repository Layout

```text
muagent/
├── Cargo.toml                             # main crate
├── src/
│   ├── lib.rs                             # module exports and prelude
│   ├── bin/
│   │   ├── muagent.rs                     # CLI entry point
│   │   └── muagent-mcp-test-server.rs     # MCP test server
│   ├── core/                              # Runner, RunState, traits, protocol
│   ├── runtime/                           # executor and tool-set provider
│   ├── providers/                         # OpenAI, Anthropic, Google adapters
│   ├── storage/                           # JSONL and memory SessionStore
│   ├── adapters/                          # filesystem, process, reqwest adapters
│   ├── capabilities/                      # built-in tools, skills, MCP
│   ├── sessions/                          # session manager, archive, compaction
│   ├── config.rs
│   ├── setup.rs                           # default wiring
│   └── prompts/
├── tests/                                 # integration tests
└── evals/                                 # local benchmark binary
```

## Selected Integration Tests

Core tests with mock implementations:

1. `t1_tool_batch_multi_call_sequential`: a model returns three tool calls; all
   run sequentially; every `ToolResult` enters history; the run returns to
   `ModelTurn` and completes.
2. `t2_at_most_once_interrupt`: recovery from an interrupted `ToolIntent`
   injects an interrupted `ToolResult` so the model can continue safely.
3. `t3_cancel_triggers_paused`: `runner.cancel()` causes the next step to
   persist `Step::Paused { HostRequested }`.
4. `t4_event_persistence_and_replay`: `(run_id, seq)` events are monotonic;
   `query_events(run_id, since)` can slice the stream; failed saves do not
   append partial events.
5. `t5_schema_migration`: current schema parses, future schema reports
   `Incompatible`, and v0 reports `Corrupt`.

Shell/default executor tests:

6. `t6_panic_becomes_tool_result`: a panicking tool becomes a retryable internal
   `ToolResult` error.
7. `t7_guard_deny_becomes_tool_result`: guard denial becomes a non-retryable
   `ToolResult`.
8. `t8_timeout_becomes_tool_result`: timeout becomes a retryable timeout
   `ToolResult`.

## Troubleshooting

- `command not found: cargo`: install Rust with rustup and reload the shell
  environment.
- `error[E0432]: unresolved import`: check the corresponding `mod.rs` or
  `src/lib.rs` export.
- Macro resolution or Tokio runtime errors: check the `tokio` features in
  `Cargo.toml`; tests generally need `macros`, `rt`, and `time`.
- Live provider failures: confirm the provider key, model, base URL, and
  whether the test is expected to be ignored by default.

## Providers

| Provider | Module | Notes |
|---|---|---|
| OpenAI | `muagent::providers::openai` | Chat/completions-compatible adapter with tool use and image input |
| Ollama | `muagent::providers::openai` | Use `base_url = "http://127.0.0.1:11434/v1"` and no API key |
| OpenRouter | `muagent::providers::openai` | Use `https://openrouter.ai/api/v1` with an OpenRouter key |
| Anthropic Claude | `muagent::providers::anthropic` | Messages API with tool and image blocks |
| Google Gemini | `muagent::providers::google` | `generateContent`, function calls, inline data, and system instructions |

When `Content::Parts` includes `ContentPart::Image { b64 | uri, mime }`, each
adapter encodes the image in the format required by that provider API.

Rust adapter example:

```rust
use muagent::adapters::ReqwestEgress;
use muagent::providers::{AnthropicAdapter, GoogleGeminiAdapter, OpenAiAdapter};
use std::sync::Arc;

let net = Arc::new(ReqwestEgress::new()?);

let openai = OpenAiAdapter::new(
    net.clone(),
    "https://api.openai.com/v1",
    "gpt-5.4-nano",
    Some("sk-...".into()),
);

let ollama = OpenAiAdapter::new(
    net.clone(),
    "http://127.0.0.1:11434/v1",
    "llama3.2",
    None,
);

let openrouter = OpenAiAdapter::new(
    net.clone(),
    "https://openrouter.ai/api/v1",
    "anthropic/claude-haiku-4.5",
    Some("sk-or-...".into()),
);

let anthropic = AnthropicAdapter::new(
    net.clone(),
    "https://api.anthropic.com",
    "claude-haiku-4-5",
    "sk-ant-...",
);

let google = GoogleGeminiAdapter::new(
    net.clone(),
    "https://generativelanguage.googleapis.com",
    "gemini-3.1-flash-lite-preview",
    "AIza...",
);
```

## Local Agent Benchmark

List tasks:

```bash
cargo run -p muagent --bin agent_bench -- --list
```

Run the full local benchmark:

```bash
OPENAI_API_KEY=... \
cargo run -p muagent --bin agent_bench --
```

Run one task multiple times:

```bash
OPENAI_API_KEY=... \
cargo run -p muagent --bin agent_bench -- \
  --provider openai \
  --model gpt-5.4-nano \
  --task csv_best_region \
  --runs 3
```

The benchmark is a small, reproducible local suite inspired by agent benchmarks
with verifiable answers. It is not a bundled copy of GAIA or WebArena.

References:

- GAIA paper: https://huggingface.co/papers/2311.12983
- GAIA dataset card: https://huggingface.co/datasets/gaia-benchmark/GAIA
- OpenAI models page: https://platform.openai.com/docs/models
