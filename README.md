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

- GitHub CLI authenticated with access to `zhcun/muagent`, for the recommended
  GitHub Release install path
- Credentials for at least one model provider, such as `OPENROUTER_API_KEY` or
  `OPENAI_API_KEY`
- Rust/Cargo 1.75+ is only needed for source installs or local development
- Node.js 16+ is only needed if you install through the local `npm install -g .`
  shim

Install the latest internal release on macOS or Linux:

```bash
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Linux-x86_64) target="x86_64-unknown-linux-musl" ;;
  *) echo "unsupported platform"; exit 1 ;;
esac

tmp="$(mktemp -d)"
gh release download --repo zhcun/muagent --pattern "muagent-*-${target}.tar.gz" --dir "$tmp"
tar -xzf "$tmp"/muagent-*-"${target}".tar.gz -C "$tmp"
sudo install -m 755 "$tmp"/muagent-*-"${target}"/muagent /usr/local/bin/muagent
muagent --help
```

For Windows, download `muagent-*-x86_64-pc-windows-msvc.zip` from the latest
GitHub Release and add the extracted directory to `PATH`.

The npm package is not published to the npm registry. For local development
from a checkout, install the source-built npm shim:

```bash
git clone <repo-url>
cd muagent
npm install -g .
muagent --help
```

`npm install -g .` builds the native Rust `muagent` binary with Cargo on the
installing machine. You can also install directly with Cargo:

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
├── BUILD.md             # cross-platform build and deployment
└── RELEASING.md         # internal GitHub Release process
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
- [RELEASING.md](RELEASING.md): internal GitHub Release process and assets
