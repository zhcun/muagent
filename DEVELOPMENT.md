# μAgent 开发说明

这份文档放本地开发、测试、benchmark 和源码目录说明。用户侧 CLI 用法见 [USAGE.md](USAGE.md),
配置参考见 [CONFIG.md](CONFIG.md), 跨平台构建和部署见 [BUILD.md](BUILD.md)。

## 开发命令

```bash
make build      # cargo build --workspace
make test       # cargo test --workspace
make clippy     # cargo clippy --all-targets -- -D warnings
```

也可以直接运行:

```bash
cargo build
cargo test
cargo test --test cli_smoke
cargo test --test m0_core
```

开发时不想安装也可以直接跑当前工作区源码:

```bash
cargo run --bin muagent -- "临时跑一次当前源码版本"
```

真实 provider / live 类测试通常需要对应 API key, 并可能被标记为 ignored。离线开发优先跑
普通测试和 CLI smoke 测试。

## Benchmark

列出本地 agent benchmark 任务:

```bash
cargo run --bin agent_bench -- --list
```

运行示例:

```bash
OPENAI_API_KEY=sk-... \
cargo run --bin agent_bench -- \
  --provider openai \
  --model gpt-5.4-nano \
  --task csv_best_region \
  --runs 3
```

这个 runner 使用仓库内自带的 22 个轻量 case, 定义在 `evals/agent_bench.rs`。外部
benchmark harness、下载数据和运行产物属于本地实验内容, 不纳入仓库。

## 跨平台编译

本地开发:

```bash
make build
```

Raspberry Pi / Linux 静态产物建议使用 `cargo-zigbuild`:

```bash
cargo install cargo-zigbuild
brew install zig

make pi          # aarch64-unknown-linux-musl
make pi-gnu      # aarch64-unknown-linux-gnu
make pi32        # armv7-unknown-linux-musleabihf
make linux-x86   # x86_64-unknown-linux-musl
```

更完整的 target 矩阵、部署和验证说明见 [BUILD.md](BUILD.md)。

## 目录结构

```text
muagent/
├── Cargo.toml
├── package.json
├── src/
│   ├── bin/              # muagent CLI 和 MCP 测试 server
│   ├── core/             # Runner / FSM / traits / protocol
│   ├── runtime/          # 默认 executor / tool-set provider
│   ├── providers/        # OpenAI / Anthropic / Google / Codex adapters
│   ├── capabilities/     # builtin tools / skills / MCP
│   ├── sessions/         # session 管理 / 压缩 / archive
│   ├── storage/          # JSONL / memory store
│   ├── tui.rs            # 可选 ratatui/crossterm 交互界面
│   └── adapters/         # fs / process / reqwest / linux adapters
├── tests/                # integration tests
├── evals/                # 本地 22-case benchmark binary
├── design/               # 架构设计文档
├── RUN.md                # 运行、测试、验收细节
└── BUILD.md              # 构建和跨平台部署
```

## 设计文档

- [design/00-README.md](design/00-README.md): 设计文档索引
- [design/01-overview.md](design/01-overview.md): 项目概览
- [design/02-architecture.md](design/02-architecture.md): 核心架构
- [design/03-core-loop.md](design/03-core-loop.md): core loop
- [design/06-capabilities.md](design/06-capabilities.md): capabilities / tools
- [design/10-observability-security.md](design/10-observability-security.md): observability 和安全
- [design/11-roadmap.md](design/11-roadmap.md): roadmap
- [design/14-sessions-memory.md](design/14-sessions-memory.md): session 与历史压缩
- [design/16-prompt-design.md](design/16-prompt-design.md): prompt 与 cache 设计
- [design/17-thinking-design.md](design/17-thinking-design.md): thinking / reasoning artifact 设计
