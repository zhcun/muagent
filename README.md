# μAgent

A minimal harness for a complete local agent.

`muagent` keeps the interface small while shipping the full agent loop: model
adapters, filesystem/shell tools, resumable sessions, long-context
compaction, skill and MCP loading, image inputs, and terminal-first workflows
through one-shot `exec` and a full-screen TUI.

It works with OpenRouter, OpenAI, OpenAI Codex OAuth, Anthropic, and Google.

## Install

### Release

Install the latest release on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/zhcun/muagent/main/scripts/install.sh | sh
muagent --help
```

Run the same command again to upgrade; it skips work when the installed version
already matches latest.

Uninstall:

```bash
curl -fsSL https://raw.githubusercontent.com/zhcun/muagent/main/scripts/install.sh | sh -s -- --uninstall
```

### Source

Build and install from a checkout:

```bash
git clone https://github.com/zhcun/muagent.git
cd muagent
npm install -g .
muagent --help
```

Or install the Rust binary directly:

```bash
cargo install --path . --bin muagent --force
```

## Configure

Use `~/.muagent/config.toml` for provider, model, and credentials. Pick one,
or keep multiple provider sections in the same file.

OpenRouter:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key = "sk-or-..."
```

OpenAI:

```toml
[model]
provider = "openai"

[providers.openai]
model = "gpt-5.4-nano"
api_key = "sk-..."
```

OpenAI Codex OAuth:

```toml
[model]
provider = "openai-codex"

[providers.openai_codex]
model = "gpt-5.5"
```

For Codex OAuth, run `codex login` once; `muagent` reuses
`~/.codex/auth.json`. See [CONFIG.md](CONFIG.md) for more providers, project
config, and model capability overrides.

## Run

```bash
# Basic modes
muagent exec "Summarize this project."
muagent "Open the TUI with this initial prompt."
muagent

# Provider and model overrides
muagent --provider openai exec "List the test entry points."
muagent --provider openai --model gpt-5.4-nano exec "List the test entry points."
muagent --provider openai-codex --model gpt-5.5 exec "Review the current diff."

# Alternate config, workspace root, and session store
muagent --config-file ./config.toml exec "Use this config file only."
muagent --root /path/to/project exec "Inspect this project."
muagent --store memory exec "Run without saving the session."
muagent --store jsonl:~/.muagent/sessions exec "Use an explicit session store."

# Images
muagent exec --image ./screenshot.png "Explain this screenshot."
muagent exec --image ./a.png,./b.jpg "Compare these images."

# Sessions
muagent sessions
muagent sessions --all
muagent resume --last
muagent resume <SESSION_ID>
muagent exec resume --last "Continue the previous task."
muagent exec resume <SESSION_ID> "Continue this session and run one turn."
muagent --provider openai resume --last
```

## Documentation

- [USAGE.md](USAGE.md): CLI modes, TUI commands, tools, skills, and session
  usage
- [CONFIG.md](CONFIG.md): provider defaults, config files, model capabilities,
  and OAuth
- [DEVELOPMENT.md](DEVELOPMENT.md): local commands, tests, benchmarks, and
  source layout
- [BUILD.md](BUILD.md): cross-platform build and deployment notes
- [RELEASING.md](RELEASING.md): release process and assets
