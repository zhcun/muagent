# μAgent 使用说明

这份文档放面向使用者的 CLI、REPL、TUI、工具和 skills 说明。配置字段和环境变量的完整参考
见 [CONFIG.md](CONFIG.md), 本地开发命令见 [DEVELOPMENT.md](DEVELOPMENT.md)。

## 安装 CLI

从当前仓库安装到本机:

```bash
npm install -g .
muagent --help
```

这条命令是本地 npm 安装, 不需要公开上传 npm 包。它会从当前仓库安装 `package.json`
里的 `muagent` bin, 并在安装时调用 Cargo 构建 Rust CLI。npm 会把命令 shim 放到 npm
的全局 bin 目录, 例如 Homebrew Node 常见是 `/opt/homebrew/bin/muagent`。

如果想更接近正式软件包安装, 但仍然不公开上传, 可以先打成本地 tarball 再安装:

```bash
npm pack
npm install -g ./muagent-0.1.0.tgz
```

不想全局安装, 也可以用本地包一次性执行:

```bash
npx --package . muagent --help
```

卸载本地全局安装:

```bash
npm uninstall -g muagent
```

查看 npm 全局安装前缀:

```bash
npm config get prefix
```

绕过 npm, 直接用 Cargo 源码安装:

```bash
cargo install --path . --bin muagent --force
```

Cargo 默认把可执行文件放到当前用户的 Cargo bin 目录:

```text
~/.cargo/bin/muagent
```

只要对应 bin 目录在 `PATH` 里, 安装后就是正常的全局命令。Cargo 路径可这样加入:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

如果需要系统级安装到 `/usr/local/bin`, 可以先构建 release binary, 再复制:

```bash
cargo build --release --bin muagent
sudo install -m 755 target/release/muagent /usr/local/bin/muagent
muagent --help
```

当前 npm 包只用于本地安装, 还没有发布到 npm registry, 所以写法是
`npm install -g .`, 不是 `npm install -g muagent`。如果以后要支持 `npx muagent` 或
`npm install -g muagent`, 需要把包和平台 binary 发布出去。

## 配置入口

`muagent` 启动时以配置文件为主, 再叠加 `.env` / 环境变量 / 命令行参数。安装命令不会
自动创建配置文件; 你可以先建一个用户级配置:

```bash
mkdir -p ~/.muagent
$EDITOR ~/.muagent/config.toml
```

推荐把密钥放在环境变量或 `.env`, 配置文件只引用环境变量名:

```bash
export OPENROUTER_API_KEY=sk-or-...
export OPENAI_API_KEY=sk-...
```

OpenRouter + OpenAI + OpenAI Codex 示例, 默认用 OpenRouter:

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
# 推荐先运行 `codex login`, 让 muagent 复用 ~/.codex/auth.json。
# 如果手动传 access token, 再打开下面这行:
# api_key_env = "OPENAI_CODEX_ACCESS_TOKEN"
```

完整配置字段、默认值、环境变量和 OpenAI Codex OAuth 细节见 [CONFIG.md](CONFIG.md)。

## CLI 用法

启动交互式 REPL:

```bash
muagent
```

启动可选的全屏 TUI:

```bash
muagent --tui
```

TUI 不是核心运行模式, 主要用于更舒服地交互。它复用同一套 slash commands, 例如
`/help`, `/model`, `/provider`, `/tokens`, `/history`, `/list`, `/continue`。`Esc`
或 `Ctrl-C` 退出, `PageUp` / `PageDown` 滚动消息区。

单次执行任务:

```bash
muagent "阅读 src/lib.rs 并解释模块导出"
muagent exec "帮我找出失败测试的原因"
```

携带本地图片输入:

```bash
muagent \
  --image ./screenshots/error.png \
  "说明这张截图里的报错"
```

支持的图片扩展名: `png`, `jpg`, `jpeg`, `webp`, `gif`。

继续最近的持久化 session:

```bash
muagent resume --last
muagent exec resume --last "继续刚才的任务"
```

继续指定 session:

```bash
muagent resume <SESSION_ID>
muagent exec resume <SESSION_ID> "下一步做什么"
```

如果 prompt 本身以 `-` 开头, 用 `--` 分隔参数:

```bash
muagent -- "- 这是一个以短横线开头的 prompt"
```

## REPL 命令

进入 REPL 后可使用:

| 命令 | 说明 |
|---|---|
| `/help` | 显示命令 |
| `/new` | 开始新 session |
| `/tokens` | 查看当前 session token / cost 统计 |
| `/history` | 打印最近 20 条消息摘要 |
| `/model` | 显示当前 provider / model |
| `/model <model_id>` | 切换当前 REPL 的 model, 不写回配置文件 |
| `/provider` | 显示当前 provider / model |
| `/provider <name> [model_id]` | 切换当前 REPL 的 provider / model, 不写回配置文件 |
| `/skills` | 列出已注册 skills |
| `/session` | 显示当前 session_id / run_id / step |
| `/list` | 列出持久化 sessions |
| `/continue <session_id>` | 继续某个 session |
| `/fork <run_id> <message_index>` | 从历史消息分叉新 session |
| `/search <query>` | 搜索持久化 session 历史 |
| `/quit`, `/exit` | 退出 |

## 工具与安全边界

内置工具:

- `fs_read`, `fs_write`, `fs_edit`, `fs_list`, `fs_stat`, `fs_delete`, `fs_rename`
- `net_http`, 默认注册; 可用 `MUAGENT_NET_HTTP=off` 或 `--disable-tools net_http` 关闭
- `sh_exec`, 只有配置了 `allow_sh` / `MUAGENT_ALLOW_SH` 后才注册

文件工具被限制在 `fs.root` / `--root` 下。`sh_exec` 只允许执行 allowlist 中的 binary
名称, 例如 `rg` 或 `cargo`; 没有配置 allowlist 时不会暴露 shell 工具。

## Skills 与 Agent 指令

启动时默认从以下位置自动加载 skills:

- `./.muagent/skills/`
- `~/.muagent/skills/`

每个 skill 目录需要包含 `SKILL.md`, 并在 frontmatter 中提供 `name` 和 `description`。
可以通过 `--skills`, `--disable-skills`, `--no-skills-autoload` 或对应环境变量控制加载。

Agent 指令文件默认开启, 会从工作区祖先目录和用户配置目录读取:

- `AGENT.md`
- `AGENTS.md`
- `CLAUDE.md`

关闭:

```bash
MUAGENT_AGENT_MD=off muagent
```
