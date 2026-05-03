# μAgent Development

This document covers local development, tests, benchmarks, and the source tree.
User-facing CLI usage is in [USAGE.md](USAGE.md), configuration is in
[CONFIG.md](CONFIG.md), and cross-platform builds are in [BUILD.md](BUILD.md).

## Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustc --version      # expected: 1.75 or newer
```

## Common Commands

```bash
make build      # cargo build --workspace
make test       # cargo test --workspace
make clippy     # cargo clippy --all-targets -- -D warnings
```

Direct Cargo commands:

```bash
cargo build
cargo test
cargo test --test cli_smoke
cargo test --test m0_core
```

Run focused tests:

```bash
cargo test -p muagent --test m0_core t1_tool_batch_multi_call_sequential
cargo test -p muagent --test m0_shell t6_panic_becomes_tool_result
```

Run the current workspace version without installing:

```bash
cargo run --bin muagent -- --help
cargo run --bin muagent -- "Run one task with the current checkout."
cargo run --bin muagent -- repl
```

Live provider tests usually require API keys and may be marked `ignored`. For
offline development, prefer unit/integration tests and CLI smoke tests.

## Troubleshooting

- `command not found: cargo`: install Rust with rustup and reload the shell.
- `error[E0432]: unresolved import`: check the corresponding `mod.rs` or
  `src/lib.rs` export.
- Macro resolution or Tokio runtime errors: check the `tokio` features in
  `Cargo.toml`; tests generally need `macros`, `rt`, and `time`.
- Live provider failures: confirm the provider key, model, base URL, and
  whether the test is expected to be ignored by default.

## Benchmark

List local benchmark tasks:

```bash
cargo run --bin agent_bench -- --list
```

Run one benchmark task:

```bash
OPENAI_API_KEY=sk-... \
cargo run --bin agent_bench -- \
  --provider openai \
  --model gpt-5.4-nano \
  --task csv_best_region \
  --runs 3
```

The benchmark runner uses 22 lightweight local cases defined in
`evals/agent_bench.rs`. External benchmark harnesses, downloaded datasets, and
generated run artifacts are local experiment data and are not part of the
repository.

## Cross-Platform Builds

Local build:

```bash
make build
```

Raspberry Pi and static Linux artifacts are intended to use `cargo-zigbuild`:

```bash
cargo install cargo-zigbuild
brew install zig

make pi          # aarch64-unknown-linux-musl
make pi-gnu      # aarch64-unknown-linux-gnu
make pi32        # armv7-unknown-linux-musleabihf
make linux-x86   # x86_64-unknown-linux-musl
```

See [BUILD.md](BUILD.md) for the target matrix, deployment commands, and
artifact checks.

## Source Layout

```text
muagent/
├── Cargo.toml
├── package.json
├── src/
│   ├── bin/              # CLI and MCP test server
│   ├── core/             # Runner, FSM, traits, protocol
│   ├── runtime/          # default executor and tool-set provider
│   ├── providers/        # OpenAI, Anthropic, Google, Codex adapters
│   ├── capabilities/     # built-in tools, skills, MCP
│   ├── sessions/         # session manager, compaction, archive
│   ├── storage/          # JSONL and memory stores
│   ├── tui/              # optional ratatui/crossterm UI
│   ├── adapters/         # filesystem, process, reqwest, platform adapters
│   ├── cli.rs            # CLI argument parsing and dispatch
│   ├── agent_instructions.rs  # AGENT.md / AGENTS.md / CLAUDE.md loader
│   ├── oauth.rs          # OpenAI Codex OAuth helpers
│   └── setup.rs          # default wiring
├── tests/                # integration tests
├── evals/                # local benchmark binary
└── BUILD.md              # build and deployment notes
```
