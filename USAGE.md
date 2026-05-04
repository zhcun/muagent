# μAgent Usage

This document covers user-facing installation, CLI modes, TUI/REPL commands,
tools, skills, and agent instruction files. See [CONFIG.md](CONFIG.md) for the
complete configuration reference and [DEVELOPMENT.md](DEVELOPMENT.md) for local
development commands.

## Install The CLI

### Release Install

Install the latest release on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/zhcun/muagent/main/scripts/install.sh | sh
muagent --help
```

The installer selects the latest GitHub Release by default and supports macOS
Apple Silicon, macOS Intel, and Linux x64.

Run the same command again to upgrade; it skips work when the installed version
already matches latest.

Uninstall:

```bash
curl -fsSL https://raw.githubusercontent.com/zhcun/muagent/main/scripts/install.sh | sh -s -- --uninstall
```

### Source Install

Source installs require Rust/Cargo. The local npm shim also requires Node.js.
Install Rust first if needed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
cargo --version
rustc --version
```

Install from the current checkout:

```bash
npm install -g .
muagent --help
```

This is a local npm install. It does not require publishing the package to the
npm registry. The install script builds the Rust CLI and places the `muagent`
shim in npm's global binary directory, for example `/opt/homebrew/bin/muagent`
with a common Homebrew Node setup.

To test a package artifact without publishing:

```bash
npm pack
npm install -g ./muagent-0.1.0.tgz
```

To run without a global install:

```bash
npx --package . muagent --help
```

Uninstall the local global package:

```bash
npm uninstall -g muagent
```

Check npm's global prefix:

```bash
npm config get prefix
```

Install directly with Cargo:

```bash
cargo install --path . --bin muagent --force
```

Cargo installs to the current user's Cargo binary directory:

```text
~/.cargo/bin/muagent
```

Make sure that directory is in `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

For a system-level install, build a release binary and copy it into a system
binary directory:

```bash
cargo build --release --bin muagent
sudo install -m 755 target/release/muagent /usr/local/bin/muagent
muagent --help
```

The npm package is currently intended for local installs, so use
`npm install -g .` rather than `npm install -g muagent`.

## Configure A Provider

For the default OpenRouter provider, an environment variable is enough:

```dotenv
OPENROUTER_API_KEY=sk-or-...
```

Put that in a `.env` file in the workspace where you run `muagent`, then run:

```bash
muagent exec "Summarize this repository."
```

Use provider flags for temporary switches:

```dotenv
OPENAI_API_KEY=sk-...
```

```bash
muagent --provider openai --model gpt-5.4-nano exec "Run the focused tests."
```

Create `~/.muagent/config.toml` only when you want durable defaults. Example
with OpenRouter as the default provider, plus OpenAI and OpenAI Codex profiles
for explicit switching:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"

[providers.openai]
model = "gpt-5.4-nano"
api_key_env = "OPENAI_API_KEY"

[providers.openai_codex]
model = "gpt-5.5"
# Prefer `codex login`; muagent can reuse ~/.codex/auth.json.
# For a manual access token, uncomment the next line:
# api_key_env = "OPENAI_CODEX_ACCESS_TOKEN"
```

See [CONFIG.md](CONFIG.md) for all fields, defaults, environment variables, and
OpenAI Codex OAuth behavior.

## Command Forms

| Form | Behavior |
|---|---|
| `muagent` | Start the full-screen TUI |
| `muagent <PROMPT>` | Start the TUI and submit an initial prompt |
| `muagent exec <PROMPT>` | Run one task and exit |
| `muagent repl` or `muagent --repl` | Start the line-mode REPL |
| `muagent resume` | Pick a persisted session from the current workspace |
| `muagent resume --last [PROMPT]` | Resume the latest session in the current workspace |
| `muagent resume <SESSION_ID> [PROMPT]` | Resume a specific session |
| `muagent exec resume --last <PROMPT>` | Resume the latest session, run one turn, and exit |
| `muagent sessions` | List persisted sessions for the current workspace |
| `muagent sessions --all` | List persisted sessions across all workspaces |

Config flags can be placed before the command. Most config flags are also
accepted inside `exec`, `resume`, and `sessions`, but putting them before the
command is easier to read:

```bash
muagent --provider openai --model gpt-5.4-nano exec "Run the focused tests."
muagent --store memory --disable-tools sh_exec "Inspect this repository only."
```

## CLI Modes

Start the full-screen TUI:

```bash
muagent
```

Start the line-mode REPL:

```bash
muagent repl
# or
muagent --repl
```

Run one task and exit:

```bash
muagent exec "Read src/lib.rs and explain the exported modules."
muagent exec "Find the cause of the failing tests."
```

Start the TUI with an initial prompt:

```bash
muagent "Read src/lib.rs and explain the exported modules."
```

Attach local images to a one-shot prompt:

```bash
muagent exec \
  --image ./screenshots/error.png \
  "Explain the error shown in this screenshot."
```

Supported image extensions are `png`, `jpg`, `jpeg`, `webp`, and `gif`.
Multiple images can be provided with a comma-separated list or repeated flags:

```bash
muagent exec \
  --image ./a.png,./b.jpg \
  --image ./c.webp \
  "Compare these screenshots."
```

Images require a one-shot prompt. They are accepted by `exec` and by resume
commands that include a prompt. They are not accepted by `sessions` or by the
TUI.

Resume persisted sessions:

```bash
muagent resume              # choose from sessions in the current workspace
muagent resume --last
muagent resume --all        # choose from every workspace
muagent resume "Continue the previous task."
muagent exec resume --last "Continue the previous task."
```

List sessions:

```bash
muagent sessions
muagent sessions --all
```

Resume a specific session:

```bash
muagent resume <SESSION_ID>
muagent exec resume <SESSION_ID> "What should we do next?"
```

If a prompt starts with `-`, separate flags from the prompt with `--`:

```bash
muagent -- "- This prompt starts with a dash."
```

## Options Reference

| Option | Description |
|---|---|
| `-h`, `--help` | Print help |
| `-V`, `--version` | Print version |
| `--tui` | Use the full-screen TUI |
| `--repl` | Use the line REPL when no one-shot prompt is supplied |
| `--config-file <FILE>` | Load a specific TOML config file |
| `--provider <NAME>` | Select `openai`, `openai-codex`, `anthropic`, `google`, or `openrouter` |
| `-m`, `--model <ID>` | Override the model ID |
| `--base-url <URL>` | Override the provider base URL |
| `-i`, `--image <PATHS>` | Attach image files to a one-shot prompt |
| `--store <SPEC>` | Use `memory`, `jsonl:/path/to/store`, or a plain JSONL store path |
| `--root <DIR>` | Set the filesystem sandbox root for file tools |
| `--mcp-sse <URLS>` | Register comma-separated MCP SSE endpoints; repeated flags append |
| `--cache <MODE>` | `auto` or `off` |
| `--thinking <MODE>` | `high`, `auto`, `off`, `minimal`, `low`, `medium`, `max`, or `xhigh` |
| `--max-tokens <N>` | Set the context budget used by automatic compaction |
| `--log <FILTER>` | Set a tracing filter such as `muagent=debug,info` |
| `--tools <LIST>`, `--enable-tools <LIST>` | Expose only the listed tools |
| `--disable-tools <LIST>` | Hide and reject the listed tools |
| `--skills <LIST>`, `--enable-skills <LIST>` | Expose only the listed skill IDs |
| `--disable-skills <LIST>` | Hide the listed skill IDs |
| `--no-skills-autoload` | Disable automatic skill discovery |

List arguments use commas:

```bash
muagent --disable-tools sh_exec,net_http "Analyze without shell or network."
muagent --tools fs_read,fs_list,fs_stat "Inspect files without write tools."
```

Useful environment-only controls:

| Variable | Description |
|---|---|
| `MUAGENT_LOG` | Tracing filter; falls back to `RUST_LOG` |
| `MUAGENT_MAX_STEPS` | Safety limit for the model/tool loop |
| `MUAGENT_BAD_TOOL_EVENT_LIMIT` | Stops repeated timeout/security/error tool loops |
| `MUAGENT_AGENT_MD` | Set to `off` to disable agent instruction files |
| `MUAGENT_AGENT_MD_MAX_BYTES` | Per-file byte cap for agent instruction files |

## Common Recipes

Run without writing persistent session history:

```bash
muagent --store memory exec "Try this once and discard the session."
```

Constrain file access to the current repository and disable shell/network tools:

```bash
muagent \
  --root . \
  --disable-tools sh_exec,net_http \
  exec "Review the public documentation."
```

Debug a provider or config issue:

```bash
MUAGENT_LOG=muagent=debug,info \
muagent --provider openrouter --model openai/gpt-5.4-nano exec "Say hello."
```

Use a temporary project config:

```bash
muagent --config-file .muagent/config.toml exec "Use this project's defaults."
```

## TUI Notes

The TUI is the default interactive mode. It uses the same slash commands as the
REPL, including `/help`, `/model`, `/provider`, `/tokens`, `/history`, `/list`,
and `/continue`.

Common controls:

- `Esc` or `Ctrl-C`: exit
- `PageUp` / `PageDown`: scroll the message area
- `Up` / `Down`: browse input history when the input is empty or single-line
- bracketed paste: short text goes into the input; long or multi-line paste is
  summarized in the UI as `[pasted N lines]` and submitted in full
- `Ctrl-B` or `F2`: open the background `sh_exec` job list

## TUI / REPL Commands

| Command | Description |
|---|---|
| `/help` | Show commands |
| `/new` | Start a new session |
| `/tokens` | Show token and cost counters for the current session |
| `/history` | Print a brief summary of the last 20 messages |
| `/model` | Show the current provider and model |
| `/model <model_id>` | Switch the current session's model without editing config |
| `/provider` | Show the current provider and model |
| `/provider <name> [model_id]` | Switch the current session's provider and optional model |
| `/skills` | List registered skills |
| `/session` | Show the current session, run, and step |
| `/list` | List persisted sessions |
| `/continue <session_id>` | Continue a persisted session |
| `/fork <run_id> <message_index>` | Fork a session from a historical message |
| `/search <query>` | Search persisted session history |
| `/quit`, `/exit` | Exit |

## Tools And Boundaries

Built-in tools:

- `fs_read`, `fs_write`, `fs_edit`, `fs_list`, `fs_stat`, `fs_delete`,
  `fs_rename`
- `net_http`, registered by default and removable with `MUAGENT_NET_HTTP=off`
  or `--disable-tools net_http`
- `sh_exec`, registered by default and able to run binaries on `PATH` or by
  explicit path

File tools are restricted to `fs.root` / `--root`. Disable shell execution with:

```bash
muagent --disable-tools sh_exec
```

## Skills And Agent Instructions

By default, `muagent` discovers skills from:

- `./.muagent/skills/`
- `~/.muagent/skills/`

Each skill directory must contain `SKILL.md` with frontmatter fields for `name`
and `description`. Skill loading can be controlled with `--skills`,
`--disable-skills`, `--no-skills-autoload`, and the matching environment
variables.

Agent instruction files are enabled by default. `muagent` reads these filenames
from workspace ancestor directories and the user config directory:

- `AGENT.md`
- `AGENTS.md`
- `CLAUDE.md`

Disable them for one run:

```bash
MUAGENT_AGENT_MD=off muagent
```
