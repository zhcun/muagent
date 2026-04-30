# μAgent

μAgent 是一个用 Rust 写的 on-device agent runtime。当前仓库提供可直接运行的
`muagent` CLI, 支持单次任务、交互式 REPL、可选 TUI、持久化 session、自动压缩、
内置文件/网络/shell 工具、skills 自动加载, 以及 OpenAI / OpenRouter / Anthropic /
Google / OpenAI Codex OAuth 等模型后端。

项目仍在快速迭代。这个 README 只保留新使用者入口; 更完整的使用、配置、构建和开发说明
请看后面的文档索引。

## 快速开始

前置要求:

- Node.js 16+
- Rust 1.75+
- 至少一个模型 provider 的凭证, 例如 `OPENROUTER_API_KEY` 或 `OPENAI_API_KEY`

```bash
git clone <repo-url>
cd sagent
npm install -g .
muagent --help
```

`npm install -g .` 是本地安装, 不需要发布 npm 包。也可以绕过 npm 直接用 Cargo 安装:

```bash
cargo install --path . --bin muagent --force
```

创建用户级配置:

```bash
mkdir -p ~/.muagent
$EDITOR ~/.muagent/config.toml
```

最小 OpenRouter 配置:

```toml
[model]
provider = "openrouter"

[providers.openrouter]
model = "openai/gpt-5.4-nano"
api_key_env = "OPENROUTER_API_KEY"
```

OpenRouter 下某个具体模型的能力覆盖要写到模型级, 不要写到整个 provider。例如
`moonshotai/kimi-k2.6` 不支持图片输入:

```toml
[providers.openrouter]
model = "moonshotai/kimi-k2.6"
api_key_env = "OPENROUTER_API_KEY"

[providers.openrouter.models."moonshotai/kimi-k2.6".capabilities]
vision = false
```

密钥建议放在环境变量或 `.env`:

```bash
export OPENROUTER_API_KEY=sk-or-...
```

运行一次任务:

```bash
muagent "总结一下这个仓库的结构"
```

## 基本用法

交互式 REPL:

```bash
muagent
```

单次执行:

```bash
muagent "阅读 src/lib.rs 并解释模块导出"
muagent exec "帮我找出失败测试的原因"
```

继续最近的持久化 session:

```bash
muagent resume --last
muagent exec resume --last "继续刚才的任务"
```

临时切换 provider / model:

```bash
muagent --provider openai --model gpt-5.4-nano "列出当前项目的测试入口"
muagent --provider openai-codex --model gpt-5.5 "继续分析当前改动"
```

CLI、REPL、TUI、工具、skills 和 agent instruction 文件的完整使用说明见
[USAGE.md](USAGE.md)。配置字段、默认值、环境变量和 OAuth 细节见 [CONFIG.md](CONFIG.md)。

## 目录结构

```text
sagent/
├── Cargo.toml
├── package.json
├── src/                 # core runtime, CLI, providers, tools, sessions, storage
├── tests/               # integration tests
├── evals/               # local 22-case benchmark binary
├── design/              # architecture design docs
├── CONFIG.md            # configuration reference
├── USAGE.md             # CLI and runtime usage
├── DEVELOPMENT.md       # local development and benchmark notes
├── BUILD.md             # cross-platform build and deployment
└── RUN.md               # detailed run/test notes and historical acceptance checklist
```

## 文档索引

- [USAGE.md](USAGE.md): 安装方式、CLI、REPL/TUI、工具安全边界、skills
- [CONFIG.md](CONFIG.md): 配置文件字段、默认值、provider、环境变量、OpenAI Codex OAuth
- [DEVELOPMENT.md](DEVELOPMENT.md): 本地开发命令、测试、benchmark、源码目录说明
- [BUILD.md](BUILD.md): 交叉编译、Raspberry Pi / Linux 产物和部署
- [RUN.md](RUN.md): 更细的运行、测试、验收记录
- [design/00-README.md](design/00-README.md): 设计文档索引
- [design/02-architecture.md](design/02-architecture.md): 核心架构
- [design/14-sessions-memory.md](design/14-sessions-memory.md): session 与历史压缩
- [design/16-prompt-design.md](design/16-prompt-design.md): prompt 与 cache 设计
- [design/17-thinking-design.md](design/17-thinking-design.md): thinking / reasoning artifact 设计
