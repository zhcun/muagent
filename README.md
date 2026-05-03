# μAgent

μAgent is a Rust-based agent runtime for working inside a local workspace. The
repository includes the `muagent` CLI with a terminal UI, one-shot execution,
a line-mode REPL, persistent sessions, automatic context compaction, built-in
file/network/shell tools, skill loading, and model adapters for OpenAI,
OpenRouter, Anthropic, Google, and OpenAI Codex OAuth.

This README is the user entry point. The linked documents contain the complete
usage, configuration, build, and development references.

## Quick Start

Requirements:

- Rust/Cargo 1.75+
- Credentials for at least one model provider, such as `OPENROUTER_API_KEY` or
  `OPENAI_API_KEY`
- Node.js 16+ is only needed if you install through the `npm install -g .`
  shim; the `cargo install` path below has no Node dependency

Install Rust first if it is not already available:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
cargo --version
rustc --version
```

Install the CLI from this checkout:

```bash
git clone <repo-url>
cd muagent
npm install -g .
muagent --help
```

`npm install -g .` performs a local install. It does not require publishing to
the npm registry; the package install step builds the native Rust `muagent`
binary with Cargo. You can also install directly with Cargo:

```bash
cargo install --path . --bin muagent --force
```

Create a user-level config file:

```bash
mkdir -p ~/.muagent
$EDITOR ~/.muagent/config.toml
```

Minimal OpenRouter config:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"
```

Provider-wide capability overrides are rarely correct for aggregator providers.
For example, if a specific OpenRouter model does not support image input, scope
the override to that model:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
```

Keep secrets in environment variables or `.env`:

```bash
export OPENROUTER_API_KEY=sk-or-...
```

Run one task:

```bash
muagent exec "Summarize the structure of this repository."
```

## Basic Usage

Start the full-screen TUI:

```bash
muagent
```

Start the line-mode REPL:

```bash
muagent repl
```

Run one-shot tasks:

```bash
muagent exec "Read src/lib.rs and explain the exported modules."
muagent exec "Find out why the tests are failing."
```

Resume persisted sessions:

```bash
muagent resume
muagent resume --last
muagent resume "Continue the previous task."
muagent exec resume --last "Continue the previous task."
muagent sessions
```

Temporarily switch provider or model:

```bash
muagent --provider openai --model gpt-5.4-nano "List the test entry points."
muagent --provider openai-codex --model gpt-5.5 "Analyze the current changes."
```

See [USAGE.md](USAGE.md) for complete CLI, REPL, TUI, tool, skill, and agent
instruction usage. See [CONFIG.md](CONFIG.md) for config fields, defaults,
environment variables, and OpenAI Codex OAuth details.

## Repository Layout

```text
muagent/
├── Cargo.toml
├── package.json
├── src/                 # runtime, CLI, providers, tools, sessions, storage
├── tests/               # integration tests
├── evals/               # local benchmark binary
├── CONFIG.md            # configuration reference
├── USAGE.md             # CLI and runtime usage
├── DEVELOPMENT.md       # local development notes
└── BUILD.md             # cross-platform build and deployment
```

## Documentation

- [USAGE.md](USAGE.md): installation, CLI modes, TUI/REPL commands, tools,
  skills, and agent instruction files
- [CONFIG.md](CONFIG.md): config files, defaults, providers, environment
  variables, model capabilities, and OAuth
- [DEVELOPMENT.md](DEVELOPMENT.md): local development commands, tests,
  benchmarks, and source layout
- [BUILD.md](BUILD.md): cross-compilation, Raspberry Pi/Linux targets, and
  deployment checks
