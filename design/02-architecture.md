# 02 · 分层架构(v3.1 · Core + 默认 Shell · 无挂起/审批/权限)

> v3.1 把 v3 的三层简化到实用两层:
> - **Core Runtime**(极小内核)
> - **Default Shell**(skill / MCP / tool / storage / model-adapter 集合;**默认需要**,host 直接用它)
> - ~~Plugins~~ 保留作未来扩展点,**当前为空**(approval / permission / stall / budget 等不在本阶段范围)

## 2.1 分层图

```
┌───────────────────────────────────────────────────────────────┐
│  Host App / Daemon       Swift / Kotlin / Rust CLI / Python    │
├───────────────────────────────────────────────────────────────┤
│  SDK bindings            UniFFI-generated                      │
├───────────────────────────────────────────────────────────────┤
│  Default Shell(默认使用形态;包含了"真正好用的 agent"所需一切) │
│  - CapabilityRegistry / SkillManager / McpClient               │
│  - DefaultToolSetProvider(TOC-first + lazy describe)          │
│  - DefaultToolExecutor(消费 AdapterBundle,含三段门禁)        │
│  - SessionManager · list / continue / fork / search / 自动 title│
│  - CompactionStrategy · 默认 summary 压缩(80% ctx 触发)      │
│  - SessionArchive · JSONL 存档 + prompt 注入路径(script 可读)│
│  - JsonlSessionStore(kv / list_runs / query_events)           │
│  - ModelAdapter factories(OpenAI / Ollama / MLX / ExecuTorch) │
│  - 内置 tools:fs.* / sh.* / net.http / sys.notify / session.* │
│  - CLI channel / iOS BGTask helper / Android FGS helper        │
│                                                                │
│  host 组装 Shell 的 DefaultToolExecutor + 自己提供的 Adapter    │
│  bundle,然后传进 Runner 即可得到一个可用 agent。               │
├───────────────────────────────────────────────────────────────┤
│  Core Runtime(pure Rust)                                      │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │ RunState                                                 │  │
│  │ Runner::step                                             │  │
│  │   FSM: Ready → ModelTurn → ToolBatch → ToolIntent        │  │
│  │        → Done / Failed / Paused                          │  │
│  │ 4 个 Core trait:                                         │  │
│  │   ModelAdapter          · 模型调用                         │  │
│  │   ToolExecutor          · 工具执行(直接返 ToolResult)      │  │
│  │   SessionStore          · durable state                   │  │
│  │   ActiveToolSetProvider · 本 step 的 tool set + prompt     │  │
│  │ Core protocols:                                           │  │
│  │   PromptPlan / PromptProfile / RuntimeFacts               │  │
│  │   ThinkingConfig / ThinkingArtifacts / replay semantics   │  │
│  │ 可选 helper: Clock                                        │  │
│  │ RuntimeError / Event / Step                              │  │
│  └─────────────────────────────────────────────────────────┘  │
├───────────────────────────────────────────────────────────────┤
│  Plugins(可选 addon,按需引入)                               │
│  - muagent-memory(跨 session 长期记忆;见 14-sessions-memory   │
│    / 15-long-term-memory):默认不启用,host 显式依赖才生效     │
│  - approval / permission / stall 独立 addon / 企业网关等       │
│    继续保留在 future;v3.1 不落地                              │
└───────────────────────────────────────────────────────────────┘
```

## 2.2 Core 的 trait 集(4 个)

```rust
pub trait ModelAdapter           { /* caps, turn(req) -> reply */ }
pub trait ToolExecutor           { /* execute(call) -> ToolResult */ }
pub trait SessionStore           { /* save_delta, load_run, list_runs, kv_* */ }
pub trait ActiveToolSetProvider  { /* provide(state) -> ActiveToolSet */ }

// 可选 helper
pub trait Clock { /* now_ms, budget_hint */ }
```

`ActiveToolSetProvider` 带 `Fn(&RunState) -> ActiveToolSet` 的 blanket impl,host 可传闭包,也可自定义 struct 实现(如 Shell 的 `DefaultToolSetProvider` 或 addon 的 `AutoRecallProvider`)。

除 4 个 trait 外,Core 还应定义两个**协议层对象**:

- **Prompt contract**:请求分层、cacheable prefix、dynamic runtime context、`build_request` 规范
- **Thinking contract**:reasoning artifacts、tool-loop replay、durable persistence、visibility

它们不是 Shell 细节,因为都会直接影响下一次模型调用的正确性。

Core 不定义:`FileSystem` / `ProcessExec` / `InterApp` / `NetEgress` / `Secrets` / `Camera` / `ImageCodec` 等 Adapter 概念——它们是 **Shell 或 host 端**给 ToolExecutor 实现用的辅助。

Core 连"挂起"概念都没有——`Step::Suspended` / `ToolOutcome::Suspend` / `Resumption` 全部不在 v3.1 范围。

## 2.3 Default Shell 提供的能力(default · 必选)

| 模块 | 作用 |
|---|---|
| **CapabilityRegistry** | 注册所有 tool / skill / MCP 的统一索引 |
| **SkillManager** | activation / deactivation / 跨会话 state |
| **McpClient** | MCP 客户端,支持 HTTP-SSE / Stdio / InProc transport;双重 lazy |
| **DefaultToolSetProvider** | 把 tool/skill/MCP 汇成 `ActiveToolSet`,含 TOC-first |
| **DefaultToolExecutor** | 三段门禁(schema / guard / sandbox)+ `catch_unwind` + timeout |
| **SessionManager** | list / continue / fork / delete / search sessions;自动 session title(见 [14-sessions-memory](14-sessions-memory.md)) |
| **CompactionStrategy** | 默认 `SummaryCompactionStrategy`(80% ctx_len 阈值,保留 tail 4 turn);turn-aligned 强约束 |
| **SessionArchive** | 明文 JSONL + meta.json 存档,prompt 自动注入路径;LLM 可用 `fs.*` / `sh.exec` grep 等搜索跨 session 历史,**无需嵌入模型**(见 14.8) |
| **AdapterBundle** | FileSystem / ProcessExec / InterApp / NetEgress / Secrets / Camera / Mic / ImageCodec / ... |
| **Builtin tools** | `fs.read/write/list/stat/delete/rename`、`sh.exec`、`net.http`、`sys.notify`、`cap.list/describe/deactivate`、`session.note/notes_list` |
| **JsonlSessionStore** | `SessionStore` 默认实现(含 kv / list_runs / delete_run / query_events;append-only JSONL on disk,no SQL dependency) |
| **Model factories** | 默认 `ModelAdapter` 实现:OpenAI-compat / Ollama / llama.cpp / MLX(iOS) / ExecuTorch(Android) |
| **Channels / helpers** | CLI REPL(带 /new /list /fork /search)/ iOS BGTask / Android FGS / MQTT 等模板 |

**用户视角:导入 `muagent` = 导入 Core + Default Shell。** 这是正常使用形态。**包含完整的 session 管理和自动历史压缩**,"聊长了崩"和"新对话找不到旧对话"的问题在 default 下不存在。

Host 正常依赖 `muagent`。Core 现在作为 `src/core/` 内部模块维护;等协议稳定且确实有独立复用需求时,再拆成 `muagent-core` crate。

## 2.4 目录布局

```
muagent/
├── Cargo.toml                     // 主 package: muagent
├── src/
│   ├── core/                      // Runner / RunState / protocol traits
│   ├── runtime/                   // DefaultToolExecutor / DefaultToolSetProvider
│   ├── providers/                 // OpenAI / Anthropic / Google adapters
│   ├── storage/                   // JSONL / memory SessionStore
│   ├── adapters/                  // fs / process / http / linux / reqwest
│   ├── capabilities/              // builtin tools / skills / MCP
│   ├── sessions/                  // manager / archive / compaction
│   ├── config.rs
│   ├── setup.rs                   // 默认装配
│   └── bin/
│       ├── muagent.rs             // CLI
│       └── muagent-mcp-test-server.rs
├── tests/                         // integration tests
├── evals/                         // local 22-case benchmark binary
└── design/
```

## 2.5 依赖方向(单向)

```
host / CLI → muagent
              ├─ core          (protocol + FSM, no concrete IO)
              ├─ runtime       (executor + active tool-set provider)
              ├─ providers     (LLM provider adapters)
              ├─ capabilities  (tools / skills / MCP)
              ├─ adapters      (host/system IO)
              ├─ storage       (SessionStore backends)
              └─ sessions      (UX-level session lifecycle)
```

Core 不依赖其它上层模块。其它模块依赖 Core 的协议类型,由 `setup.rs`
在默认装配里连起来。

## 2.6 一次请求的数据流

```
Host submits user message
    │
    ▼
Runner::step(&mut state)
    │
    │  match state.step:
    │   Ready       → push input,转 ModelTurn
    │   ModelTurn   → tools_provider(state) → ActiveToolSet
    │              → ModelAdapter::turn(req) → reply
    │              → 无 tool_calls: Done
    │              → 有 tool_calls: ToolBatch
    │   ToolBatch  → 取 calls[cursor]
    │              → 若 AtMostOnce:先 ToolIntent
    │              → ToolExecutor::execute(call) → ToolResult
    │              → push 结果到 history,cursor++
    │              → 批次完 → ModelTurn
    │   ToolIntent → thaw 恢复:生成 "interrupted" ToolResult 回灌 → ModelTurn
    │   Done/Failed/Paused → no-op,等 host 继续或取消
    │
    ▼
host 订阅 StepOutput.events
```

没有审批中断、没有权限中断、没有挂起协议。纯 FSM。

## 2.6.1 Shell 如何订阅 Core 事件(v3.1 规范)

v3.1 Core 没有 EventBus,只返 `StepOutput { events }`。那 Shell 的 auto-title / SessionArchive / Compaction trigger 等**依赖事件流**的功能怎么工作?

**答:Shell 的 `default_runner()` 返回一个 wrapper,它在内部消费事件,再把事件原样透传给调用方。**

```rust
// src/setup.rs / src/runtime/*
pub fn default_runner(
    model: Arc<dyn ModelAdapter>,
    tool_executor: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,
    tools_provider: Arc<dyn ActiveToolSetProvider>,
    shell_hooks: ShellHooks,
) -> impl RunnerLike {
    let core = muagent_core::Runner::new(model.clone(), tool_executor,
                                          store.clone(), tools_provider);
    // 层层包装(自底向上)
    let r = WithCompaction::new(core, SummaryCompactionStrategy::default(model.clone()));
    let r = WithSessionArchive::new(r, SessionArchive::new(store.clone()));
    let r = WithAutoTitle::new(r, model.clone());
    let r = WithBudget::new(r, ...);
    r
}

// 每个 wrapper 的模板:
impl<R: RunnerLike> RunnerLike for WithSessionArchive<R> {
    async fn step(&mut self, state: &mut RunState, cancel: &CancelToken)
        -> Result<StepOutput, RuntimeError>
    {
        let out = self.inner.step(state, cancel).await?;
        // 观察 events,触发 archive 写入;out.events 原样透传
        self.archive.apply_events(state, &out.events).await?;
        Ok(out)
    }
}
```

**关键不变量**:
- 每个 wrapper 的 `step` 只**观察**或**追加**事件,不修改 inner 已发的事件
- 观察失败(例如 archive 写盘失败)只发 warning 事件,不失败 inner 的推进
- 事件有序:inner.events 先,wrapper 追加的在后;host 订阅 step 返回的 events 看到的顺序是从里到外递增

Host 订阅点:
```rust
let out = runner.step(&mut state, &cancel).await?;
for ev in out.events {
    dispatch_to_ui(ev);
    dispatch_to_tracing(ev);
    ...
}
```

没有"全局 EventBus"。所有事件**只从 step 返回值走**。

## 2.7 v3.1 的"审批/权限"实现建议(非本期范围)

如果未来确实需要审批或权限:
- **审批**:最简做法是 Host 在 LLM 回复后检查 tool_calls,对危险操作**拦截** → 显示 UI → 用户决定 → 根据结果**修改 RunState 的 history**(注入一条"用户已批准/拒绝"的 observation)→ 继续 `Runner::step`。不需要改 Core。
- **权限**:Tool 实现内部直接调系统 API(iOS 的 `PHPhotoLibrary.requestAuthorization` 等),授权对话框在 tool 执行期间被系统弹出。tool 返回 `ToolResult { ok:false, retryable:false, hint:"permission denied" }`。LLM 看到后决定下一步。
- 这些都不需要 Core 改动。

若真需要把"挂起"变成 Core 一等概念(跨进程可恢复):再做一版把 `Step::Suspended` 加回来(新 variant,前向兼容)。当前**不做**。

## 2.8 与 v3 的差异(删减清单)

| v3 | v3.1 |
|---|---|
| Core 的 `Step::Suspended { kind, payload }` | **删除** |
| `ToolOutcome` 枚举 | **删除**(ToolExecutor 直接返 `ToolResult`) |
| `Resumption` 枚举 + `Runner::resume(...)` | **删除** |
| `Event::Suspended` / `Event::Resumed` | **删除** |
| `muagent-approval` / `muagent-permission` 作为计划 addon | **删除**(future 再考虑) |
| 架构图里的 Plugins 层列表含 approval/permission/stall/budget/... | 保留"未来扩展点"语义,**列表清空** |
| 三层架构 | 实质**两层**(Core + Default Shell),Plugins 空置 |

## 2.9 保留(相对 v3 未变)

- Core 3 trait(ModelAdapter / ToolExecutor / SessionStore)
- Clock 可选
- `ActiveToolSet` / `ToolsProvider` 闭包机制
- Default Shell 容纳 skill / MCP / tool / InterApp / capability registry
- ToolBatch 多 call 支持(协议需要)
- ToolIntent 协议(opt-in per tool)
- SessionStore 必备
- step 粒度持久化 + at-least-once 事件 `(run_id, seq)`
- RunState.schema_version + migration

## 2.10 核心不变量(v3.1)

1. Core 3 个必备 trait,没有更多。
2. FSM 没有"等外部决定"状态;所有外部决定必须在 ToolExecutor 实现内部完成。
3. skill / MCP / tool 是 Default Shell 的**默认功能**;不是可选。
4. approval / permission / stall / budget / compaction **不在本期范围**;需要时 host 自己或未来 addon 解决。
5. 持久化粒度 = step;RunState 带 schema_version。
6. AtMostOnce tool 中断不重跑(ToolIntent 协议)。
7. 并行仅限 ReadOnly(编译期校验)。
8. 平台差异在 URI 与实现,不在 tool 名字。
