# μAgent Development

This document covers local development, tests, benchmarks, and the source tree.
User-facing CLI usage is in [USAGE.md](USAGE.md), configuration is in
[CONFIG.md](CONFIG.md), and cross-platform builds are in [BUILD.md](BUILD.md).

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

Run the current workspace version without installing:

```bash
cargo run --bin muagent -- "Run one task with the current checkout."
```

Live provider tests usually require API keys and may be marked `ignored`. For
offline development, prefer unit/integration tests and CLI smoke tests.

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
│   ├── tui.rs            # optional ratatui/crossterm UI
│   └── adapters/         # filesystem, process, reqwest, platform adapters
├── tests/                # integration tests
├── evals/                # local benchmark binary
├── RUN.md                # run/test commands and selected test map
└── BUILD.md              # build and deployment notes
```
