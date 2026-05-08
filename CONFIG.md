# μAgent Configuration

`muagent` uses config files for durable defaults, environment variables for
secrets, and CLI flags for temporary overrides.

## Load Order

Config sources are loaded in this order:

1. Built-in defaults
2. `~/.muagent/config.toml`
3. `.muagent/config.toml` files found from the current directory upward
4. `.env`, `../.env`, `../../.env`, and process environment variables
5. CLI flags

Later sources override earlier sources. `--config-file <FILE>` or
`MUAGENT_CONFIG=<FILE>` loads only that config file instead of the default user
and project config search paths. Environment variables and CLI flags still
override the selected file.

## Model Resolution

Provider selection starts with `[model].provider` or top-level `provider`.
`MUAGENT_PROVIDER` and `--provider` override that default for the current
process.

For the active provider, `model` and `base_url` are resolved in this order:

1. CLI flag: `--model`, `--base-url`
2. Generic environment variable: `MUAGENT_MODEL`, `MUAGENT_BASE_URL`
3. Provider-specific environment variable, such as `OPENAI_MODEL`,
   `OPENROUTER_MODEL`, or `GEMINI_BASE_URL`
4. Config field scoped to the active provider
5. Built-in provider default

API keys and OAuth access tokens are resolved in this order:

1. `MUAGENT_API_KEY`
2. The provider's default key environment variable, such as
   `OPENROUTER_API_KEY` or `OPENAI_API_KEY`
3. The environment variable named by `api_key_env`
4. Literal `api_key` in config

This means a provider's standard key environment variable wins over a custom
`api_key_env` for the same provider. If you need to test a different key, unset
the standard key variable or use `MUAGENT_API_KEY` for that process.

## Why TOML

TOML is the canonical config format because it is common in Rust projects,
supports comments, has narrower type rules than YAML, and is readable for
profile-oriented configuration.

## Recommended File Shape

Write config in this order:

1. `[model]`: default active provider
2. `[providers.*]`: provider profiles, including model, base URL, and key env
3. `[providers.*.models."<model-id>".capabilities]`: per-model capability
   overrides
4. `[runtime]`: cache and thinking behavior
5. `[store]`: session persistence
6. `[fs]`, `[tools]`, `[skills]`, `[mcp]`: capability boundaries
7. `[compaction]`, `[agent_md]`: long-context and project-instruction settings

Create a user-level config:

```bash
mkdir -p ~/.muagent
$EDITOR ~/.muagent/config.toml
```

Create a project-level config:

```bash
mkdir -p .muagent
$EDITOR .muagent/config.toml
```

## Minimal Examples

OpenRouter as the default provider:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"
```

Per-model capability override for an OpenRouter model. This affects only
`moonshotai/kimi-k2.6`:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
```

OpenAI as the default provider:

```toml
[model]
provider = "openai"

[providers.openai]
model = "gpt-5.4-nano"
api_key_env = "OPENAI_API_KEY"
```

Codex OAuth as the default provider:

```toml
[model]
provider = "codex"

[providers.codex]
model = "gpt-5.5"
# base_url defaults to https://chatgpt.com/backend-api
```

Prefer `codex login` so `muagent` can reuse `~/.codex/auth.json`. Manual access
token overrides are supported, but login files provide better refresh behavior:

```toml
[model]
provider = "codex"

[providers.codex]
model = "gpt-5.5"
api_key_env = "OPENAI_CODEX_ACCESS_TOKEN"
```

```bash
export OPENAI_CODEX_ACCESS_TOKEN=...
export OPENAI_CODEX_ACCOUNT_ID=...
```

## Multiple Providers

This config uses OpenRouter by default and allows temporary switches with
`--provider openai` or `--provider codex`:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false

[providers.openai]
model = "gpt-5.4-nano"
api_key_env = "OPENAI_API_KEY"

[providers.codex]
model = "gpt-5.5"

[providers.anthropic]
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

[providers.google]
model = "gemini-3.1-flash-lite-preview"
api_key_env = "GEMINI_API_KEY"
```

```bash
muagent "Use the default OpenRouter profile."
muagent --provider openai "Use OpenAI for this run."
muagent --provider codex "Use Codex OAuth for this run."
```

## Complete Example

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"

[providers.openai]
model = "gpt-5.4-nano"
api_key_env = "OPENAI_API_KEY"

[providers.codex]
model = "gpt-5.5"

[providers.anthropic]
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

[providers.google]
model = "gemini-3.1-flash-lite-preview"
api_key_env = "GEMINI_API_KEY"

[runtime]
cache = true
thinking = "high"

[store]
path = "jsonl:~/.muagent/sessions"

[fs]
root = "."

[tools]
# Omit enabled to expose every registered tool.
# enabled = [] explicitly exposes no tools.
enabled = ["fs_read", "fs_write", "fs_list", "fs_stat", "fs_edit", "fs_rename", "fs_delete", "sh_exec"]

[skills]
# Omit enabled to expose every discovered skill.
disabled = []

[mcp]
sse_endpoints = ["http://127.0.0.1:10086/sse"]

[compaction]
max_tokens = 156000
threshold_ratio = 0.8
keep_tail_turns = 4
keep_recent_tokens = 20000
root_task_pin_max_tokens = 1024
summary_input_max_tokens = 100000
summary_output_max_tokens = 8000
restart_repair_window_tokens = 300000
max_summary_rounds = 4

[agent_md]
enabled = true
max_bytes = 65536

[subagents]
# When enabled and at least one .muagent/agents/*.md definition exists, the
# `spawn_sub_agent` delegation tool is exposed by default.
enabled = true
```

Subagent invocations are capped at depth 1. A subagent never receives the
`spawn_sub_agent` tool, even if it is inherited or listed in `tools`. A parent
agent can run at most 8 subagent calls concurrently.

## Common Recipes

Use a `.env` file for secrets and keep config portable:

```bash
cat > .env <<'EOF'
OPENROUTER_API_KEY=sk-or-...
OPENAI_API_KEY=sk-...
EOF
```

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"
```

Disable shell tools while keeping read-only filesystem inspection:

```toml
[tools]
enabled = ["fs_read", "fs_list", "fs_stat"]
disabled = ["sh_exec"]
```

Use throwaway in-memory sessions:

```toml
[store]
path = "memory"
```

Use a project-local session store:

```toml
[store]
path = "jsonl:.muagent/sessions"
```

Define a project subagent:

```bash
mkdir -p .muagent/agents
$EDITOR .muagent/agents/reviewer.md
```

```markdown
---
name: reviewer
description: Reviews code changes for correctness issues.
tools: fs_read, fs_list
max_steps: 200
---
Read the relevant files and report concrete bugs only.
```

Disable subagent tools:

```toml
[subagents]
enabled = false
```

Tune compaction for a smaller context model:

```toml
[compaction]
max_tokens = 32000
threshold_ratio = 0.75
keep_tail_turns = 4
keep_recent_tokens = 8000
summary_input_max_tokens = 24000
summary_output_max_tokens = 4000
```

Run a cheaper summarizer than the main model:

```bash
export MUAGENT_SUMMARIZER_PROVIDER=openrouter
export MUAGENT_SUMMARIZER_MODEL=openai/gpt-5.4-nano
export MUAGENT_SUMMARIZER_API_KEY="$OPENROUTER_API_KEY"
```

## Provider Fields

`[model]` selects the default active provider. `[providers.<id>]` stores a
provider-specific profile. When the active provider is the same provider named
in `[model].provider`, values in `[model]` override the matching provider
profile fields. If a CLI flag or environment variable switches to another
provider, `muagent` reads that provider's profile instead.

Supported fields:

| Field | Description |
|---|---|
| `provider` | Only valid under `[model]`; selects the default provider |
| `model` | Model ID |
| `base_url` | API base URL; omitted values use the provider default |
| `api_key_env` | Environment variable that contains an API key or access token |
| `api_key` | Literal secret; avoid committing this to git |

Capability overrides are resolved in this order:

1. `[providers.<id>.models."<model-id>".capabilities]`: one provider and one
   model
2. `[model.capabilities]`: shorthand for the default provider named in
   `[model].provider`
3. `[providers.<id>.capabilities]`: provider-wide fallback

Official providers usually do not need explicit capability overrides because
the adapter infers them. Aggregator providers such as OpenRouter host many
models with different capabilities, so model-level overrides are preferred:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
ctx_len = 262144
```

Quote model IDs in TOML table names because they often contain `/`, `.`, or
`-`.

Capability fields:

| Field | Aliases | Description |
|---|---|---|
| `vision` | `image`, `images` | Whether the model supports image input |
| `ctx_len` | `context_window`, `context_length`, `max_context_tokens` | Context window size in tokens |
| `prompt_cache` | `cache` | Low-level prompt-cache capability override |
| `reasoning` | `thinking` | Wire capability: `none`, `supported`, or `replay` |
| `native_tool_use` | `tool_use`, `tool_calling` | Adapter wire hint for native tool calls |
| `json_schema_mode` | `json_schema` | Whether JSON-schema constrained output is supported |
| `streaming` | `stream` | Whether streaming is supported |

Compatibility aliases: `reasoning = "no_replay"` is equivalent to
`reasoning = "supported"`, and `reasoning = "full_replay"` is equivalent to
`reasoning = "replay"`. This is not runtime thinking effort. Use
`[runtime] thinking = "high"` to request high reasoning effort.

Provider IDs:

| Provider | Config value | Table | Default model | Default base URL | Default key env |
|---|---|---|---|---|---|
| OpenAI | `openai` | `[providers.openai]` | `gpt-5.4-nano` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| Codex | `codex` (`openai-codex`, `openai_codex`, and `chatgpt` are aliases) | `[providers.codex]` | `gpt-5.5` | `https://chatgpt.com/backend-api` | `OPENAI_CODEX_ACCESS_TOKEN` |
| Anthropic | `anthropic`, `claude` | `[providers.anthropic]` | `claude-haiku-4-5` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| Google | `google`, `gemini` | `[providers.google]` | `gemini-3.1-flash-lite-preview` | `https://generativelanguage.googleapis.com` | `GEMINI_API_KEY` |
| OpenRouter | `openrouter` | `[providers.openrouter]` | `openai/gpt-5.4-nano` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |

## Codex OAuth

`codex` is not a standard OpenAI API-key provider. It calls the ChatGPT
backend `/codex/responses` endpoint with an OAuth access token.

Credential lookup order:

1. Any access token resolved by the general model config path. In practice this
   includes `MUAGENT_API_KEY`, `OPENAI_CODEX_ACCESS_TOKEN`, the configured
   `api_key_env`, or literal `api_key`.
2. `MUAGENT_CODEX_ACCESS_TOKEN` or `OPENAI_CODEX_ACCESS_TOKEN`
3. `MUAGENT_CODEX_AUTH_FILE` or `MUAGENT_OPENAI_CODEX_AUTH_FILE`
4. `~/.muagent/auth.json`
5. `~/.pi/agent/auth.json`
6. `~/.codex/auth.json`

Manual tokens need an account ID. `muagent` can usually extract it from the JWT,
or you can set it explicitly:

```bash
export OPENAI_CODEX_ACCESS_TOKEN=...
export OPENAI_CODEX_ACCOUNT_ID=...
```

If an auth file is used, expired tokens are refreshed with the refresh token and
written back to the same file when possible. Recommended flow:

```bash
codex login
muagent --provider codex "Run one task with Codex OAuth."
```

## Files And Shell

```toml
[fs]
root = "."
```

| Field | Default | Environment | CLI |
|---|---:|---|---|
| `fs.root` | Current working directory | `MUAGENT_ROOT` | `--root <DIR>` |

`fs.root` is the workspace/default cwd advertised to the agent and used by
shell execution. File tools accept absolute `file://` paths; this setting is
not a filesystem access boundary on desktop/server platforms.

Disable shell execution:

```bash
muagent --disable-tools sh_exec
```

## Store

```toml
store = "jsonl:~/.muagent/sessions"
# or:
[store]
path = "jsonl:~/.muagent/sessions"
```

| Value | Description |
|---|---|
| Omitted | Defaults to `jsonl:~/.muagent/sessions` |
| `memory` or empty string | In-memory sessions only; discarded on exit |
| `jsonl:/path/to/store` | JSONL persistence |
| `/path/to/store` | Equivalent to JSONL persistence |

Environment variable: `MUAGENT_STORE`. CLI flag: `--store <SPEC>`.

## Tools And Skills

```toml
[tools]
enabled = ["fs_read", "fs_write", "fs_list"]
disabled = ["sh_exec"]

[skills]
enabled = ["filesystem"]
disabled = ["marketing-ideas"]
```

Omitting `enabled` uses the default behavior: all registered tools and all
discovered skills are exposed. `enabled = []` is meaningful and exposes none.

| Config | Environment | CLI |
|---|---|---|
| `tools.enabled` | `MUAGENT_TOOLS` | `--tools`, `--enable-tools` |
| `tools.disabled` | `MUAGENT_DISABLE_TOOLS` | `--disable-tools` |
| `skills.enabled` | `MUAGENT_SKILLS` | `--skills`, `--enable-skills` |
| `skills.disabled` | `MUAGENT_DISABLE_SKILLS` | `--disable-skills` |
| `capabilities.skill_autoload` | `MUAGENT_SKILL_AUTOLOAD` | `--no-skills-autoload` |

Compatibility aliases are also supported: `[capabilities] tools`,
`disabled_tools`, `skills`, and `disabled_skills`.

The allowlist is applied before the denylist. For example, this exposes only
the three read-only filesystem tools and then removes `fs_stat`:

```toml
[tools]
enabled = ["fs_read", "fs_list", "fs_stat"]
disabled = ["fs_stat"]
```

## MCP

```toml
[mcp]
sse_endpoints = ["http://127.0.0.1:10086/sse"]
```

| Config | Default | Environment | CLI |
|---|---:|---|---|
| `mcp.sse_endpoints` / `mcp.sse` | `[]` | `MUAGENT_MCP_SSE` | `--mcp-sse <URLS>` |

## Runtime

```toml
[runtime]
cache = true
thinking = "high"
```

| Field | Default | Environment | CLI |
|---|---:|---|---|
| `runtime.cache` | `true` | `MUAGENT_CACHE` | `--cache auto`, `--cache off` |
| `runtime.thinking` | `high` | `MUAGENT_THINKING` | `--thinking <MODE>` |

`thinking` supports `off`, `auto`, `minimal`, `low`, `medium`, `high`, `max`,
and `xhigh`. Boolean parsing also accepts `on`, `1`, `true`, `yes`, `enabled`,
`auto`, `off`, `0`, `false`, `no`, and `disabled`.

`runtime.thinking = "high"` is runtime reasoning effort. A model capability
override such as
`[providers.*.models."<model>".capabilities] reasoning = "supported"` only
controls whether the adapter can send reasoning fields.

## Compaction

```toml
[compaction]
max_tokens = 156000
threshold_ratio = 0.8
keep_tail_turns = 4
keep_recent_tokens = 20000
root_task_pin_max_tokens = 1024
summary_input_max_tokens = 100000
summary_output_max_tokens = 8000
restart_repair_window_tokens = 300000
max_summary_rounds = 4
```

| Field | Default | Environment |
|---|---:|---|
| `compaction.max_tokens` | `156000` | `MUAGENT_MAX_TOKENS` |
| `compaction.threshold_ratio` | `0.8` | `MUAGENT_COMPACTION_THRESHOLD` |
| `compaction.keep_tail_turns` | `4` | `MUAGENT_KEEP_TAIL_TURNS` |
| `compaction.keep_recent_tokens` | `20000` | `MUAGENT_KEEP_RECENT_TOKENS` |
| `compaction.root_task_pin_max_tokens` | `1024` | `MUAGENT_ROOT_TASK_PIN_MAX_TOKENS` |
| `compaction.summary_input_max_tokens` | `100000` | `MUAGENT_SUMMARY_INPUT_MAX_TOKENS` |
| `compaction.summary_output_max_tokens` | `8000` | `MUAGENT_SUMMARY_OUTPUT_MAX_TOKENS` |
| `compaction.restart_repair_window_tokens` | `300000` | `MUAGENT_RESTART_REPAIR_WINDOW_TOKENS` |
| `compaction.max_summary_rounds` | `4` | `MUAGENT_MAX_SUMMARY_ROUNDS` |

The dedicated summarizer is configured only through environment variables:

```bash
export MUAGENT_SUMMARIZER_MODEL=openai/gpt-5.4-nano
export MUAGENT_SUMMARIZER_PROVIDER=openrouter
export MUAGENT_SUMMARIZER_BASE_URL=https://openrouter.ai/api/v1
export MUAGENT_SUMMARIZER_API_KEY=sk-or-...
```

When only `MUAGENT_SUMMARIZER_MODEL` is set, provider, base URL, and key are
inherited from the main model where possible.

## Agent Instruction Files

```toml
[agent_md]
enabled = true
max_bytes = 65536
```

At startup, `muagent` reads `AGENT.md`, `AGENTS.md`, and `CLAUDE.md` from
workspace ancestors and the user config directory.

| Field | Default | Environment |
|---|---:|---|
| `agent_md.enabled` | `true` | `MUAGENT_AGENT_MD` |
| `agent_md.max_bytes` / `agent_md.max_bytes_per_file` | `65536` | `MUAGENT_AGENT_MD_MAX_BYTES` |

## Common Environment Variables

| Environment variable | Description |
|---|---|
| `MUAGENT_CONFIG` | Single config file path |
| `MUAGENT_PROVIDER` | Active provider |
| `MUAGENT_MODEL` | Active model |
| `MUAGENT_BASE_URL` | Active provider base URL |
| `MUAGENT_API_KEY` | Override key for standard providers |
| `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL` | OpenAI |
| `OPENROUTER_API_KEY`, `OPENROUTER_MODEL`, `OPENROUTER_BASE_URL` | OpenRouter |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ANTHROPIC_BASE_URL` | Anthropic |
| `GEMINI_API_KEY`, `GEMINI_MODEL`, `GEMINI_BASE_URL` | Google |
| `OPENAI_CODEX_ACCESS_TOKEN`, `OPENAI_CODEX_ACCOUNT_ID`, `OPENAI_CODEX_MODEL`, `OPENAI_CODEX_BASE_URL` | Codex |
| `MUAGENT_CODEX_ACCESS_TOKEN`, `MUAGENT_CODEX_ACCOUNT_ID`, `MUAGENT_CODEX_REFRESH_TOKEN` | Codex override |
| `MUAGENT_STORE` | Session store |
| `MUAGENT_ROOT` | Workspace/default cwd for file and shell tools |
| `MUAGENT_TOOLS`, `MUAGENT_DISABLE_TOOLS` | Tool allowlist and denylist |
| `MUAGENT_SKILLS`, `MUAGENT_DISABLE_SKILLS`, `MUAGENT_SKILL_AUTOLOAD` | Skill settings |
| `MUAGENT_CACHE`, `MUAGENT_THINKING` | Runtime settings |
| `MUAGENT_LOG` | Tracing filter, for example `muagent=debug,info` |
| `MUAGENT_MAX_STEPS` | Agent step safety limit |
| `MUAGENT_BAD_TOOL_EVENT_LIMIT` | Fuse for consecutive timeout/security/error tool events |

## CLI Overrides

```bash
muagent \
  --config-file .muagent/config.toml \
  --provider openai \
  --model gpt-5.4-nano \
  --root . \
  --disable-tools sh_exec \
  "Run the relevant tests."
```

CLI flags affect only the current process. `/model` and `/provider` inside the
REPL/TUI affect only the current session and do not edit `config.toml`.

## Diagnostics

Useful checks:

```bash
muagent --help
MUAGENT_LOG=muagent=debug,info muagent --provider openrouter exec "hello"
muagent --config-file ~/.muagent/config.toml exec "hello"
```

Common failures:

- `unknown provider`: use one of `openrouter`, `openai`, `codex`,
  `anthropic`, or `google`.
- `config file not found`: `--config-file` and `MUAGENT_CONFIG` require the
  file to exist.
- `invalid runtime.cache`: use supported boolean values such as `true`,
  `false`, `on`, or `off`.
- `unknown thinking value`: use `off`, `auto`, `minimal`, `low`, `medium`,
  `high`, `max`, or `xhigh`.
- Authentication failures: check `MUAGENT_API_KEY` first, then the
  provider-specific key variable, then the config `api_key_env` target.
- Unexpected tool exposure: inspect both `enabled` and `disabled`; a denylist
  entry removes a tool even when it is also in the allowlist.

## TOML Parsing Notes

- Keys are lowercased and `-` is normalized to `_`; `openai-codex` and
  `openai_codex` remain equivalent legacy names for `codex`.
- List fields can be TOML arrays. Environment variables and CLI lists are
  comma-separated.
- Empty arrays are meaningful: `enabled = []` explicitly exposes no entries.
- Unknown keys are ignored and reported through warning logs.
