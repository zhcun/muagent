# M0 运行说明

## 前置:安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustc --version      # 期望 >= 1.75
```

## 编译 + 跑测试

```bash
cd /Users/mike/jiang/muagent
cargo build
cargo test
```

期望输出:
- 主 crate `muagent` 编译通过
- 非 ignored 测试全部通过;真实 provider/live 测试默认 ignored

## 跑单个测试

```bash
cargo test -p muagent --test m0_core t1_tool_batch_multi_call_sequential
cargo test -p muagent --test m0_shell t6_panic_becomes_tool_result
```

## 当前目录布局

```
muagent/
├── Cargo.toml                             # 主 crate: muagent
├── src/
│   ├── lib.rs                             # 模块导出 + prelude
│   ├── bin/
│   │   ├── muagent.rs                     # CLI
│   │   └── muagent-mcp-test-server.rs     # MCP 测试 server
│   ├── core/                              # Runner / RunState / trait / protocol
│   ├── runtime/                           # executor / tool-set provider
│   ├── providers/                         # OpenAI / Anthropic / Google adapters
│   ├── storage/                           # JSONL / memory SessionStore
│   ├── adapters/                          # fs / process / reqwest / linux adapters
│   ├── capabilities/                      # builtin tools / skills / MCP
│   ├── sessions/                          # session manager / archive / compaction
│   ├── config.rs
│   ├── setup.rs                           # 默认装配
│   └── prompts/
├── tests/                                 # integration tests
├── evals/                                 # local 22-case benchmark binary
└── design/                                # architecture notes
```

## M0 验收测试清单

**Core(mock 实现)**:
1. `t1_tool_batch_multi_call_sequential` — LLM 返 `[a,b,c]` → 三个都执行 → 全部 ToolResult 进 history → 回 ModelTurn → Done
2. `t2_at_most_once_interrupt` — AtMostOnce 工具模拟"上次中断在 ToolIntent" → on_tool_intent_recover 注入 interrupted ToolResult → LLM 收到后继续
3. `t3_cancel_triggers_paused` — `runner.cancel()` → 下次 step 开头 → `Step::Paused { HostRequested }` → persist
4. `t4_event_persistence_and_replay` — 事件 `(run_id, seq)` 严格单调;`query_events(run_id, since)` 可任意切片;crash injector 让 save 失败时 store 端事件数不增
5. `t5_schema_migration` — 当前 schema 正常解析;过高 schema → `Incompatible`;v0 → `Corrupt`(未实现)

**Shell(DefaultToolExecutor)**:
6. `t6_panic_becomes_tool_result` — tool `panic!("boom")` → catch_unwind → `ToolResult { ok:false, retryable:true, content:"internal: boom" }`
7. `t7_guard_deny_becomes_tool_result` — guard 返 Deny → `ToolResult { ok:false, retryable:false, hint:"..." }`
8. `t8_timeout_becomes_tool_result` — tool 跑 5s 但 timeout=50ms → `ToolResult { ok:false, retryable:true, content:"timeout" }`

## 编译错误排查

- `command not found: cargo` → 装 rustup,见上
- `error[E0432]: unresolved import` → 检查 `src/lib.rs` / 对应 `mod.rs` 是否导出了模块
- `macro resolution` / `tokio` 相关 → 确保 Cargo.toml 的 tokio features 含 `macros` + `rt` + `time`
- 任何 cargo 错误贴出来,我可以逐条修

## Model providers(M1 已接)

| Provider | 模块 | 备注 |
|---|---|---|
| **OpenAI** | `muagent::providers::openai` | chat/completions + tool_use + 多模态 image_url |
| **Ollama**(本地) | `muagent::providers::openai` | `base_url = "http://127.0.0.1:11434/v1"`,`api_key = None` |
| **OpenRouter** | `muagent::providers::openai` | `base_url = "https://openrouter.ai/api/v1"` + OpenRouter key |
| **Anthropic Claude** | `muagent::providers::anthropic` | Messages API;tool_use / tool_result blocks;base64 image |
| **Google Gemini** | `muagent::providers::google` | generateContent;functionCall / inlineData;systemInstruction |

多模态:`Content::Parts` 含 `ContentPart::Image { b64 \| uri, mime }` 时,所有 adapter 都会按各自 API 的 image 格式编码。

用法示例(Rust):
```rust
use muagent::providers::{AnthropicAdapter, GoogleGeminiAdapter, OpenAiAdapter};
use muagent::adapters::ReqwestEgress;
use std::sync::Arc;

let net = Arc::new(ReqwestEgress::new()?);

// OpenAI
let a1 = OpenAiAdapter::new(net.clone(), "https://api.openai.com/v1", "gpt-5.4-nano", Some("sk-...".into()));

// Ollama(零配置本地)
let a2 = OpenAiAdapter::new(net.clone(), "http://127.0.0.1:11434/v1", "llama3.2", None);

// OpenRouter(单 key 访问几十个模型)
let a3 = OpenAiAdapter::new(net.clone(), "https://openrouter.ai/api/v1",
    "anthropic/claude-haiku-4.5", Some("sk-or-...".into()));

// Anthropic 原生
let a4 = AnthropicAdapter::new(net.clone(), "https://api.anthropic.com",
    "claude-haiku-4-5", "sk-ant-...");

// Google Gemini
let a5 = GoogleGeminiAdapter::new(net.clone(),
    "https://generativelanguage.googleapis.com",
    "gemini-3.1-flash-lite-preview", "AIza...");
```

## 本地 Agent Benchmark

仓库现在带了一个轻量 benchmark runner:

```bash
cargo run -p muagent --bin agent_bench -- --list
```

默认会优先读取现有 provider 环境变量;如果是 OpenAI 路线,默认模型是 `gpt-5.4-nano`;如果是 OpenRouter 路线,默认模型是 `openai/gpt-5.4-nano`。

示例:

```bash
# 直接跑整套 22 题,其中包含 2 个真实 PNG 图片输入任务
OPENAI_API_KEY=... \
cargo run -p muagent --bin agent_bench --

# 指定模型 / 只跑某一题 / 重复 3 次
OPENAI_API_KEY=... \
cargo run -p muagent --bin agent_bench -- \
  --provider openai \
  --model gpt-5.4-nano \
  --task csv_best_region \
  --runs 3
```

这个 benchmark 不是直接内嵌 GAIA/WebArena 数据集,而是参考 GAIA 那类“答案明确、需要工具、可验证”的 agent 任务设计成本地可复现小套件,避免外部网站和大数据集把评测稳定性拖垮。参考:
- GAIA paper: https://huggingface.co/papers/2311.12983
- GAIA dataset card: https://huggingface.co/datasets/gaia-benchmark/GAIA
- OpenAI models page: https://platform.openai.com/docs/models

## CLI 配置文件

CLI 会按顺序读取用户配置和项目配置:

1. `~/.muagent/config.toml`
2. 从当前目录向上查找的 `.muagent/config.toml`(越靠近当前项目优先级越高)
3. `--config-file <FILE>` 或 `MUAGENT_CONFIG=<FILE>` 可指定单个配置文件

优先级是:命令行参数 / 环境变量 / `.env` > 项目配置 > 用户配置 > 内置默认值。
配置文件使用标准 TOML。空数组是有效配置:例如 `enabled = []`
表示显式不暴露任何工具;不写 `enabled` 才表示使用默认的"全部已注册工具"。

示例:

```toml
[model]
provider = "openrouter"
model = "openai/gpt-5.4-nano"
# 推荐把 secret 放在环境变量里;也可以直接写 api_key。
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter]
# provider-specific profile. 上面的 [model] 是默认 active provider 的快捷层;
# 切到别的 provider 时不会泄漏 openrouter 的 model/api_key。
# base_url = "https://openrouter.ai/api/v1"
# model = "openai/gpt-5.4-nano"

[providers.google]
model = "gemini-3.1-flash-lite-preview"
api_key_env = "GEMINI_API_KEY"

[tools]
# 不写 enabled 时默认暴露所有已注册工具。
enabled = ["fs_read", "fs_write", "fs_list", "sh_exec", "net_http"]
disabled = ["net_http"]

[skills]
enabled = ["filesystem"]
disabled = ["marketing-ideas"]

[fs]
root = "."
# sh_exec 默认注册; 如需关闭, 在 tools.disabled 中加入 "sh_exec"。

[compaction]
max_tokens = 156000
threshold_ratio = 0.8
keep_tail_turns = 4
summary_input_max_tokens = 100000
summary_output_max_tokens = 8000
restart_repair_window_tokens = 300000
max_summary_rounds = 4

[runtime]
cache = true
thinking = "high"

[agent_md]
enabled = true
max_bytes = 65536
```

## M0 后续(M1)

按 11-roadmap §11.3:
- SessionManager / CompactionStrategy / SessionArchive 分片
- McpClient + HttpSseTransport + StdioTransport
- Skill trait + `#[skill]` 宏
- 几个内置 tool:`fs.*` / `sh.exec` / `net.http`
- CLI REPL 通过 `/new /list /continue /fork /search`

M0 绿灯之后开始 M1。
