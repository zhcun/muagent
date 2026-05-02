# μAgent 配置参考

`muagent` 推荐用配置文件作为长期默认值, 用环境变量保存密钥, 用 CLI 参数做临时覆盖。

## 配置文件位置

默认读取顺序:

1. 内置默认值
2. `~/.muagent/config.toml`
3. 从当前目录向上查找的 `.muagent/config.toml`
4. `.env`, `../.env`, `../../.env` 和进程环境变量
5. CLI 参数

越靠后的来源优先级越高。`--config-file <FILE>` 或 `MUAGENT_CONFIG=<FILE>` 会只读取指定
配置文件, 不再走默认的用户配置和项目配置搜索路径; 环境变量和 CLI 参数仍然会覆盖它。

## 为什么是 TOML

当前 canonical config 选择 TOML, 主要原因是:

- Rust 生态主流配置就是 TOML, `Cargo.toml` 本身也是 TOML。
- TOML 支持注释, 适合人手写; JSON 不支持注释, 更适合机器交换。
- TOML 的类型规则比 YAML 更窄, 不容易出现缩进、隐式 bool/string 等问题。
- 这个项目的配置是分层 profile, TOML 的 table 写法比 `.json` 更易读。

以后可以加 JSON/YAML import/export, 但建议内部主配置保持 TOML。

## 推荐书写顺序

配置文件建议按“先选择模型, 再写模型能力, 再写运行行为和工具边界”的顺序写:

1. `[model]`: 默认 active provider
2. `[providers.*]`: provider profile, 包括默认 model、base URL、key env
3. `[providers.*.models."<model-id>".capabilities]`: 具体模型的能力覆盖
4. `[runtime]`: cache / thinking 这类运行行为
5. `[store]`: session 持久化位置
6. `[fs]`, `[tools]`, `[skills]`, `[net_http]`, `[mcp]`: 工具和外部能力边界
7. `[compaction]`, `[agent_md]`: 长上下文和项目指令

安装不会自动创建配置文件:

```bash
mkdir -p ~/.muagent
$EDITOR ~/.muagent/config.toml
```

项目专用配置可以放在项目内:

```bash
mkdir -p .muagent
$EDITOR .muagent/config.toml
```

## 最小示例

OpenRouter 作为默认 provider:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"
```

OpenRouter 下某个具体模型的能力覆盖。这个配置只影响
`moonshotai/kimi-k2.6`, 不影响 OpenRouter 里的其他模型:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
```

OpenAI 作为默认 provider:

```toml
[model]
provider = "openai"

[providers.openai]
model = "gpt-5.4-nano"
api_key_env = "OPENAI_API_KEY"
```

OpenAI Codex / ChatGPT OAuth 作为默认 provider:

```toml
[model]
provider = "openai-codex"

[providers.openai_codex]
model = "gpt-5.5"
# base_url 默认是 https://chatgpt.com/backend-api
```

OpenAI Codex 推荐先运行 `codex login`, 让 `muagent` 复用 `~/.codex/auth.json`。
也可以复用 pi-mono 的 `~/.pi/agent/auth.json`。手动 token override 也支持, 但过期刷新能力
不如登录文件完整:

```toml
[model]
provider = "openai-codex"

[providers.openai_codex]
model = "gpt-5.5"
api_key_env = "OPENAI_CODEX_ACCESS_TOKEN"
```

```bash
export OPENAI_CODEX_ACCESS_TOKEN=...
export OPENAI_CODEX_ACCOUNT_ID=...
```

## 多 Provider 示例

这个配置默认走 OpenRouter, 但可以用 `--provider openai` 或 `--provider openai-codex`
临时切换:

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

[providers.openai_codex]
model = "gpt-5.5"

[providers.anthropic]
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

[providers.google]
model = "gemini-3.1-flash-lite-preview"
api_key_env = "GEMINI_API_KEY"
```

```bash
muagent "默认走 OpenRouter"
muagent --provider openai "这次走 OpenAI"
muagent --provider openai-codex "这次走 ChatGPT/Codex OAuth"
```

## 完整示例

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
# 如需关闭 shell 工具, 使用 tools.disabled = ["sh_exec"]。

[tools]
# 不写 enabled 时默认暴露所有已注册工具。
# enabled = [] 表示显式不暴露任何工具。
enabled = ["fs_read", "fs_write", "fs_list", "fs_stat", "fs_edit", "fs_rename", "fs_delete", "sh_exec"]
disabled = ["net_http"]

[skills]
# 不写 enabled 时默认暴露所有自动发现的 skill。
disabled = []

[net_http]
enabled = false

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
```

## Provider 字段

`[model]` 决定默认 active provider。`[providers.<id>]` 保存每个 provider 自己的 profile。
当 active provider 等于 `[model].provider` 时, `[model]` 里的 `model`, `base_url`,
`api_key_env`, `api_key` 会优先于 `[providers.<id>]`。如果通过 CLI 或环境变量切换到别的
provider, 就读取对应的 `[providers.<id>]`。

支持字段:

| 字段 | 说明 |
|---|---|
| `provider` | 只在 `[model]` 下使用, 指定默认 provider |
| `model` | 模型 ID |
| `base_url` | API base URL; 不写则使用 provider 默认值 |
| `api_key_env` | 从指定环境变量读取密钥或 access token |
| `api_key` | 直接写密钥; 不推荐提交到 git |

模型能力覆盖支持三层。优先级从高到低:

1. `[providers.<id>.models."<model-id>".capabilities]`: 只影响这个 provider 下的这个模型
2. `[model.capabilities]`: 只在 `[model].provider` 当前默认 provider 上生效的快捷覆盖
3. `[providers.<id>.capabilities]`: provider-wide fallback, 只适合这个 provider profile 下所有模型能力都一致时使用

普通官方 provider 通常不需要写能力覆盖, 因为 adapter 会根据 provider/model 自动推断。
OpenRouter 这类聚合 provider 下有很多模型, 不要把某一个模型的能力写到
`[providers.openrouter.capabilities]`; 要写到模型级:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
ctx_len = 262144
```

TOML table 里的模型 ID 要加引号, 因为 OpenRouter 模型名通常包含 `/`, `.`, `-`。

支持字段:

| 字段 | 别名 | 说明 |
|---|---|---|
| `vision` | `image`, `images` | 模型是否支持图片输入; `moonshotai/kimi-k2.6` 这类非视觉模型可设为 `false` |
| `ctx_len` | `context_window`, `context_length`, `max_context_tokens` | 上下文窗口 token 数 |
| `prompt_cache` | `cache` | 高级覆盖项; 默认会按 provider/model 自动开启或关闭, 普通配置不用写 |
| `reasoning` | `thinking` | 低层 wire 能力: `none`, `supported`, `replay`; 不是 `high/medium/low` 这种推理强度 |
| `native_tool_use` | `tool_use`, `tool_calling` | 低层 adapter wire hint; 要关闭工具请用 `[tools]` |
| `json_schema_mode` | `json_schema` | 是否支持 JSON schema 输出约束 |
| `streaming` | `stream` | 是否支持 streaming; 当前 CLI 还未把 streaming 作为主路径 |

兼容旧值: `reasoning = "no_replay"` 等价于 `reasoning = "supported"`,
`reasoning = "full_replay"` 等价于 `reasoning = "replay"`。这不是运行时 thinking
effort; 如果要请求高强度推理, 写 `[runtime] thinking = "high"`。

Provider ID:

| Provider | 配置值 | 表名 | 默认模型 | 默认 base URL | 默认 key env |
|---|---|---|---|---|---|
| OpenAI | `openai` | `[providers.openai]` | `gpt-5.4-nano` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| OpenAI Codex | `openai-codex`, `openai_codex`, `codex`, `chatgpt` | `[providers.openai_codex]` | `gpt-5.5` | `https://chatgpt.com/backend-api` | `OPENAI_CODEX_ACCESS_TOKEN` |
| Anthropic | `anthropic`, `claude` | `[providers.anthropic]` | `claude-haiku-4-5` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| Google | `google`, `gemini` | `[providers.google]` | `gemini-3.1-flash-lite-preview` | `https://generativelanguage.googleapis.com` | `GEMINI_API_KEY` |
| OpenRouter | `openrouter` | `[providers.openrouter]` | `openai/gpt-5.4-nano` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |

## OpenAI Codex OAuth

`openai-codex` 不是普通 OpenAI API key provider。它调用 ChatGPT backend 的
`/codex/responses` endpoint, 使用 OAuth access token。

认证解析顺序:

1. 配置里的 `api_key` 或 `api_key_env`
2. `MUAGENT_CODEX_ACCESS_TOKEN` 或 `OPENAI_CODEX_ACCESS_TOKEN`
3. `MUAGENT_CODEX_AUTH_FILE` 或 `MUAGENT_OPENAI_CODEX_AUTH_FILE`
4. `~/.muagent/auth.json`
5. `~/.pi/agent/auth.json`
6. `~/.codex/auth.json`

手动 token 需要 account id。可以通过 JWT 自动提取, 也可以显式设置:

```bash
export OPENAI_CODEX_ACCESS_TOKEN=...
export OPENAI_CODEX_ACCOUNT_ID=...
```

如果使用 auth 文件, 过期 token 会尝试用 refresh token 刷新并写回原文件。推荐方式:

```bash
codex login
muagent --provider openai-codex "用 ChatGPT/Codex OAuth 跑一次"
```

## 文件和 Shell

```toml
[fs]
root = "."
# sh_exec 默认注册; 如需关闭, 使用 tools.disabled = ["sh_exec"]。
```

| 字段 | 默认值 | 环境变量 | CLI |
|---|---:|---|---|
| `fs.root` | 当前工作目录 | `MUAGENT_ROOT` | `--root <DIR>` |

需要关闭 shell 工具时:

```bash
muagent --disable-tools sh_exec
```

## Store

```toml
store = "jsonl:~/.muagent/sessions"
# 或:
[store]
path = "jsonl:~/.muagent/sessions"
```

| 值 | 说明 |
|---|---|
| 不配置 | 默认 `jsonl:~/.muagent/sessions` |
| `memory` 或空字符串 | 只在内存里保存, 退出即丢失 |
| `jsonl:/path/to/store` | JSONL 持久化 |
| `/path/to/store` | 等价于 JSONL 持久化 |

环境变量: `MUAGENT_STORE`; CLI: `--store <SPEC>`。

## Tools 和 Skills

```toml
[tools]
enabled = ["fs_read", "fs_write", "fs_list"]
disabled = ["net_http"]

[skills]
enabled = ["filesystem"]
disabled = ["marketing-ideas"]
```

`enabled` 不写表示使用默认行为: tools 暴露所有已注册工具, skills 暴露所有已发现 skill。
`enabled = []` 是有效配置, 表示显式暴露空集合。

| 配置 | 环境变量 | CLI |
|---|---|---|
| `tools.enabled` | `MUAGENT_TOOLS` | `--tools`, `--enable-tools` |
| `tools.disabled` | `MUAGENT_DISABLE_TOOLS` | `--disable-tools` |
| `skills.enabled` | `MUAGENT_SKILLS` | `--skills`, `--enable-skills` |
| `skills.disabled` | `MUAGENT_DISABLE_SKILLS` | `--disable-skills` |
| `capabilities.skill_autoload` | `MUAGENT_SKILL_AUTOLOAD` | `--no-skills-autoload` |

兼容别名也支持: `[capabilities] tools`, `disabled_tools`, `skills`, `disabled_skills`。

## net_http 和 MCP

```toml
[net_http]
enabled = true

[mcp]
sse_endpoints = ["http://127.0.0.1:10086/sse"]
```

| 配置 | 默认值 | 环境变量 | CLI |
|---|---:|---|---|
| `net_http.enabled` | `true` | `MUAGENT_NET_HTTP` | 可用 `--disable-tools net_http` 隐藏 |
| `mcp.sse_endpoints` / `mcp.sse` | `[]` | `MUAGENT_MCP_SSE` | `--mcp-sse <URLS>` |

## Runtime

```toml
[runtime]
cache = true
thinking = "high"
```

| 字段 | 默认值 | 环境变量 | CLI |
|---|---:|---|---|
| `runtime.cache` | `true` | `MUAGENT_CACHE` | `--cache auto`, `--cache off` |
| `runtime.thinking` | `high` | `MUAGENT_THINKING` | `--thinking <MODE>` |

`thinking` 支持: `off`, `auto`, `minimal`, `low`, `medium`, `high`, `max`, `xhigh`。
布尔值支持: `on`, `1`, `true`, `yes`, `enabled`, `auto`, `off`, `0`, `false`, `no`,
`disabled`。

这里的 `thinking = "high"` 是运行时推理强度。模型能力里的
`[providers.*.models."<model>".capabilities] reasoning = "supported"` 只是说明底层
adapter 是否会发送 reasoning 字段, 两者不是同一个配置。

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

| 字段 | 默认值 | 环境变量 |
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

独立 summarizer 目前只通过环境变量配置:

```bash
export MUAGENT_SUMMARIZER_MODEL=openai/gpt-5.4-nano
export MUAGENT_SUMMARIZER_PROVIDER=openrouter
export MUAGENT_SUMMARIZER_BASE_URL=https://openrouter.ai/api/v1
export MUAGENT_SUMMARIZER_API_KEY=sk-or-...
```

只设置 `MUAGENT_SUMMARIZER_MODEL` 时, provider/base_url/key 会尽量继承主模型环境。

## Agent Instruction 文件

```toml
[agent_md]
enabled = true
max_bytes = 65536
```

启动时会读取工作区祖先和用户配置目录里的 `AGENT.md`, `AGENTS.md`, `CLAUDE.md`。

| 字段 | 默认值 | 环境变量 |
|---|---:|---|
| `agent_md.enabled` | `true` | `MUAGENT_AGENT_MD` |
| `agent_md.max_bytes` / `agent_md.max_bytes_per_file` | `65536` | `MUAGENT_AGENT_MD_MAX_BYTES` |

## 常用环境变量

| 环境变量 | 说明 |
|---|---|
| `MUAGENT_CONFIG` | 指定单个 config 文件 |
| `MUAGENT_PROVIDER` | active provider |
| `MUAGENT_MODEL` | active model |
| `MUAGENT_BASE_URL` | active provider base URL |
| `MUAGENT_API_KEY` | 覆盖所有普通 provider key |
| `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL` | OpenAI |
| `OPENROUTER_API_KEY`, `OPENROUTER_MODEL`, `OPENROUTER_BASE_URL` | OpenRouter |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ANTHROPIC_BASE_URL` | Anthropic |
| `GEMINI_API_KEY`, `GEMINI_MODEL`, `GEMINI_BASE_URL` | Google |
| `OPENAI_CODEX_ACCESS_TOKEN`, `OPENAI_CODEX_ACCOUNT_ID`, `OPENAI_CODEX_MODEL`, `OPENAI_CODEX_BASE_URL` | OpenAI Codex |
| `MUAGENT_CODEX_ACCESS_TOKEN`, `MUAGENT_CODEX_ACCOUNT_ID`, `MUAGENT_CODEX_REFRESH_TOKEN` | OpenAI Codex override |
| `MUAGENT_STORE` | Session store |
| `MUAGENT_ROOT` | 文件工具根目录 |
| `MUAGENT_NET_HTTP` | 是否注册 `net_http` |
| `MUAGENT_TOOLS`, `MUAGENT_DISABLE_TOOLS` | 工具 allowlist / denylist |
| `MUAGENT_SKILLS`, `MUAGENT_DISABLE_SKILLS`, `MUAGENT_SKILL_AUTOLOAD` | skill 配置 |
| `MUAGENT_CACHE`, `MUAGENT_THINKING` | runtime 配置 |
| `MUAGENT_LOG` | tracing filter, 例如 `muagent=debug,info` |
| `MUAGENT_MAX_STEPS` | agent step safety limit |
| `MUAGENT_BAD_TOOL_EVENT_LIMIT` | 连续 timeout/security/error tool 事件熔断 |

## CLI 覆盖

```bash
muagent \
  --config-file .muagent/config.toml \
  --provider openai \
  --model gpt-5.4-nano \
  --root . \
  --disable-tools net_http \
  "跑一下相关测试"
```

CLI 参数只影响当前进程, 不会写回配置文件。REPL/TUI 里的 `/model` 和 `/provider` 也是当前
session 临时切换, 不会修改 `config.toml`。

## TOML 解析细节

- key 会转成小写, `-` 会规范化成 `_`; `openai-codex` 和 `openai_codex` 等价。
- list 字段可以写 TOML array, 环境变量和 CLI list 使用逗号分隔。
- 空数组有意义: `enabled = []` 表示显式暴露空集合。
- 未识别 key 会被忽略并写 warning log。
