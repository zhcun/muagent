# 14 · Session 管理 · 历史压缩(v3.1 · Default Shell 默认功能)

> v3 曾把 session 管理和 compaction 分别描述为"shell 内的细节"和"optional addon"。
> v3.1 明确:**它们是 Default Shell 的必需功能**,和 skill/MCP/tool 一样开箱即用。
> 没有 session 管理的 agent 不可用(用户想不起之前聊过什么);没有 compaction 的 agent 第一次长对话就崩。

## 14.1 概念区分

| 概念 | 作用 | 层级 |
|---|---|---|
| **RunId** | 单次 `Runner::step` 执行流的唯一 id | Core |
| **SessionId** | 用户视角的"会话"——**可以包含多个 RunId**(续聊 / fork) | Core(在 RunState 里),管理在 Shell |
| **Run**(RunState) | 一次从 Ready 到 Done/Failed/Paused 的执行状态 | Core |
| **Session**(逻辑) | 共享 SessionId 的一组 Run,UI 里呈现为"一段对话" | Shell |
| **Turn** | 单个 `assistant(+tool_calls)(+tool_results)*` 单元 | 协议约定 |
| **Step** | FSM 状态转移的原子单元(最小持久化单位) | Core |

**关键**:
- `Runner::step` 看到的是 `RunState`(一个 run)
- 用户看到的是 `Session`(一段对话,可能跨多次 run)
- Shell 的 `SessionManager` 把 SessionId 下的多个 Run 组织起来展示

## 14.2 Run 与 Session 的关系

```
Session: "明天行程规划"
 └── Run 1 (run_id=A, session_id=S1, parent=None)
 │    user: 明天下午有空时间?
 │    assistant: ... [tool_calls fs.calendar.list]
 │    tool_result: 14:00-16:00 free
 │    → Done
 └── Run 2 (run_id=B, session_id=S1, parent=A)   ← 续聊
 │    user: 那帮我 14 点定个会议
 │    assistant: ... [tool_call calendar.create_event]
 │    → Done
 └── Run 3 (run_id=C, session_id=S1, parent=B)   ← 又一次续聊
      user: 把会议改到 15 点
      ...
```

用户视角:**一个 session = 一段完整对话**。Shell 按 session_id 把 Run 3/2/1 的 history 连起来展示。

fork 场景:从某个 Run 某个 turn 分出一个新 Run,但保留原始 SessionId 关系:
```
Run X (parent=None)
 └── Run Y (parent=X, same session)       // 续聊
 └── Run Z (parent=X, same session, fork) // 从 X 的某 turn 起不同走向
```

`RunState.parent_run_id` 表达这种关系;`SessionId` 保持一致或 Shell 创建子 session。

## 14.3 Core 的 SessionStore trait(含 session 管理原语)

见 [03-core-loop §3.4](03-core-loop.md#34-core-的-3-个必备-trait)。Core 提供:

```rust
#[async_trait]
pub trait SessionStore {
    async fn save_delta(&self, state: &RunState, events: &[Event]) -> ...;
    async fn load_run(&self, id: RunId) -> ...;

    async fn list_runs(&self, filter: RunFilter) -> ...;
    async fn delete_run(&self, id: RunId) -> ...;
    async fn query_events(&self, q: EventQuery) -> ...;

    async fn kv_get/put/list(&self, session_id, key, ...) -> ...;
}
```

KV 是 **session-scoped**:`session.note("foo", "bar")` 存到 `(session_id, "foo")`,后续同 session 的 Run 都能读。

## 14.4 Shell 的 SessionManager(用户视角的 session 操作)

```rust
// muagent-shell/src/session_manager.rs
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
}

impl SessionManager {
    /// 开一个新 session(返回 SessionId)
    pub async fn new_session(&self, title: Option<String>) -> Result<SessionId, ...>;

    /// 在给定 session 下创建新 Run(续聊),把先前 Run 的 history 合并
    pub async fn continue_session(&self, session_id: SessionId)
        -> Result<RunState, ...>;

    /// fork:从某个 Run 的某个 turn 分叉新 Run
    pub async fn fork_from(&self, run_id: RunId, at_turn: u32)
        -> Result<RunState, ...>;

    /// 列出所有 session(UI 侧栏用)
    pub async fn list_sessions(&self, filter: SessionFilter)
        -> Result<Vec<SessionInfo>, ...>;

    /// 列出某 session 下的所有 run(timeline)
    pub async fn list_runs_in_session(&self, session_id: SessionId)
        -> Result<Vec<RunHeader>, ...>;

    /// 删除整个 session(及其所有 run)
    pub async fn delete_session(&self, session_id: SessionId) -> Result<(), ...>;

    /// 重命名 / 加标签
    pub async fn rename_session(&self, session_id: SessionId, title: String)
        -> Result<(), ...>;

    /// 搜索(标题 + history 全文 / 时间范围)
    pub async fn search(&self, query: &str, opts: SearchOpts)
        -> Result<Vec<SearchHit>, ...>;
}

pub struct SessionInfo {
    pub session_id: SessionId,
    pub title: String,           // 自动从第一轮 user message 摘要;用户可覆盖
    pub created_ms: i64,
    pub updated_ms: i64,
    pub run_count: u32,
    pub total_turns: u32,
    pub latest_status: RunStatus,
    pub preview: String,         // 最新 assistant 的前 200 字
}
```

### 自动 title 生成

第一次 `Runner::step` 把 user message 写入 history 后,Shell 监听 `Event::UserMessage`,调 ModelAdapter 发个小请求:"请用一句话(≤10 字)概括这段话的主题"。结果作为 title 存 KV。

失败降级:用 user message 前 20 字。

这个操作是 fire-and-forget,不阻塞主 loop。

## 14.5 默认 CLI / UI 约定

Shell 提供的 CLI / 移动端 demo 默认支持:

| 命令 / 操作 | SessionManager 调用 |
|---|---|
| `/new` / 新建会话按钮 | `new_session(None)` |
| `/list` / 会话列表栏 | `list_sessions(...)` |
| `/continue <id>` / 点击某会话 | `continue_session(...)` |
| `/fork <run_id> <turn>` / fork 按钮 | `fork_from(...)` |
| `/rename <id> <title>` | `rename_session(...)` |
| `/delete <id>` | `delete_session(...)` |
| `/search <kw>` | `search(...)` |

iOS / Android demo app 左侧栏展示 sessions,点击切换。

## 14.6 历史压缩(CompactionStrategy)

### 14.6.1 为什么 Default

没压缩 → 20 轮对话后超 ctx → 直接 `ContextOverflow` 错误 → 用户需手动开新 session。这不是"可用"的 agent。

Default Shell 自带 `SummaryCompactionStrategy`:模型驱动的摘要式压缩,**默认启用,阈值 = 80% of `LlmCaps::ctx_len`**。

### 14.6.2 Trait

```rust
// muagent-shell/src/compaction.rs
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// 在每次 ModelTurn 之前检查;返回 Some 表示要压缩
    /// 实现必须是 idempotent + 纯观察(不改 state,除非返回 Plan 后由 apply 执行)
    async fn plan(&self, state: &RunState, ctx: &CompactionCtx<'_>)
        -> Option<CompactionPlan>;

    /// 按 plan 执行压缩(修改 state.history)
    async fn apply(&self, state: &mut RunState, plan: CompactionPlan)
        -> Result<CompactionOutcome, CompactionError>;
}

pub struct CompactionCtx<'a> {
    pub caps: &'a LlmCaps,
    pub tokens_used_in_ctx: u32,
    pub threshold: u32,
}

pub struct CompactionPlan {
    /// 将要被压缩的 turn 范围 [start, end)
    /// Shell 保证 turn-aligned(绝不跨 turn 边界切)
    pub target_turns: std::ops::Range<usize>,
    pub method: CompactionMethod,
}

pub enum CompactionMethod {
    /// 用模型生成摘要,替换这些 turn 为一条 ObsKind::Summary observation
    Summary { style: SummaryStyle },
    /// 直接丢弃(少用;丢掉可能重要信息)
    DropOldest,
}

pub struct CompactionOutcome {
    pub summary_text: String,
    pub replaced_turns: u32,
    pub saved_tokens_estimate: u32,
}
```

### 14.6.3 Turn-aligned 强约束

(继承 R2 A7 的强约束)

**Compaction 只能以完整 turn 为最小单位操作**。

Turn 定义:
```
turn = user_msg
     | assistant_msg [with tool_calls]
       followed by ALL corresponding tool_result messages
       (optionally interspersed with Observation messages)
```

禁止:
- 单独丢弃 `assistant.tool_calls` 而保留对应的 `tool_result`(provider 多半 400)
- 单独丢弃 `tool_result` 而保留 `assistant.tool_calls`(同上)
- 半拆一条 `assistant` 消息

违反 turn-aligned 的实现在 PR review 时直接拒绝。

### 14.6.4 默认 SummaryCompactionStrategy 实现

```rust
pub struct SummaryCompactionStrategy {
    pub threshold_ratio: f32,       // 默认 0.8
    pub keep_tail_turns: u32,       // 默认 4(保留最近 4 个 turn 不压)
    pub summarizer: Arc<dyn ModelAdapter>,  // 可用主模型或一个轻量模型
}

#[async_trait]
impl CompactionStrategy for SummaryCompactionStrategy {
    async fn plan(&self, state: &RunState, ctx: &CompactionCtx<'_>)
        -> Option<CompactionPlan>
    {
        if ctx.tokens_used_in_ctx < ctx.threshold { return None; }

        // 找 turn 边界
        let turns = mark_turn_boundaries(&state.history);
        if turns.len() <= self.keep_tail_turns as usize + 1 {
            // 太短了压不动,等用户手动 new_session
            return None;
        }

        // 保留最近 keep_tail_turns 个,其余压缩
        let end = turns.len() - self.keep_tail_turns as usize;
        Some(CompactionPlan {
            target_turns: 0..end,
            method: CompactionMethod::Summary {
                style: SummaryStyle::default(),
            },
        })
    }

    async fn apply(&self, state: &mut RunState, plan: CompactionPlan)
        -> Result<CompactionOutcome, CompactionError>
    {
        // 抽出要压缩的 msg 切片
        let to_summarize = extract_turns(&state.history, plan.target_turns.clone());

        // 调模型生成摘要(小请求,不走主 loop)
        let summary = self.summarizer.turn(
            ModelRequest::for_summary(&to_summarize),
            CancelToken::none(),
        ).await?.text;

        // 替换:删除 target_turns 范围的 messages,插入一条 Summary observation
        let saved_tokens = estimate_tokens(&to_summarize).saturating_sub(
                           estimate_tokens_text(&summary));
        splice_turns(&mut state.history, plan.target_turns,
                     Message::Observation {
                         kind: ObsKind::Summary,
                         text: summary.clone(),
                     });

        Ok(CompactionOutcome {
            summary_text: summary,
            replaced_turns: (plan.target_turns.end - plan.target_turns.start) as u32,
            saved_tokens_estimate: saved_tokens,
        })
    }
}
```

### 14.6.5 Shell 装配 Compaction

Shell 的 default Runner 构造:
```rust
// muagent-shell/src/runner_builder.rs
pub fn default_runner(
    model: Arc<dyn ModelAdapter>,
    tool_executor: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,
    tools_provider: ToolsProvider,
) -> impl RunnerLike {
    let core_runner = muagent_core::Runner::new(
        model.clone(), tool_executor, store, tools_provider);

    // 默认装 compaction wrapper
    let runner = WithCompaction::new(
        core_runner,
        SummaryCompactionStrategy {
            threshold_ratio: 0.8,
            keep_tail_turns: 4,
            summarizer: model.clone(),   // 用主模型做摘要
        },
    );
    runner
}

pub struct WithCompaction<R> { inner: R, strategy: Arc<dyn CompactionStrategy> }

impl<R: RunnerLike> RunnerLike for WithCompaction<R> {
    async fn step(&mut self, state: &mut RunState, cancel: &CancelToken)
        -> Result<StepOutput, RuntimeError>
    {
        // 仅在 ModelTurn 开头检查
        if matches!(state.step, Step::ModelTurn) {
            let ctx = CompactionCtx::from(state, /* caps */);
            if let Some(plan) = self.strategy.plan(state, &ctx).await {
                let outcome = self.strategy.apply(state, plan).await
                    .map_err(RuntimeError::from)?;
                // 产生 Event::HistoryCompacted,随 inner.step 的事务一起落盘
                // ...
            }
        }
        self.inner.step(state, cancel).await
    }
}
```

Host 不想要压缩 → `Runner::new` 直接用,不走 `default_runner`。

### 14.6.6 手动触发 / 强制压缩

Shell 暴露给 host:
```rust
impl SessionManager {
    pub async fn compact_now(&self, run_id: RunId) -> Result<CompactionOutcome, ...>;
}
```

UI 可以给用户一个"压缩对话历史"按钮。

### 14.6.7 Events

```rust
Event::HistoryCompacted {
    replaced_turns: u32,
    saved_tokens_estimate: u32,
    summary_brief: String,          // 前 100 字
    seq: u64,
}
```

UI 显示:"此处已压缩 N 轮对话"。

## 14.7 Long-term memory(跨 session 记忆)

- **Session 内**:完整 history + 自动压缩(本文 §14.6)
- **跨 Session 同 user**:`session.note(key, value)` / `session.notes_list` meta-tool,KV 级(不含语义检索)
- **跨 Session 语义检索 / 自动抽取**:由可选 addon **`muagent-memory`** 提供,**默认关闭**。详见 [15-long-term-memory](15-long-term-memory.md)

三者各司其职,加载哪层取决于 host 需要:

```
[ Core ]                                                      (强制)
   └── RunState.history  ← 当前 Run 的 message 列表
[ Default Shell ]                                             (默认)
   └── SessionManager.continue_session  ← 合并同 SessionId 多 Run history
   └── CompactionStrategy               ← 长 history 自动摘要
   └── session.note(session-scoped KV)  ← 单 session 命名事实
[ Addon: muagent-memory ]                                     (默认关)
   └── Facts       ← 跨 session 命名事实
   └── Episodes    ← 跨 session 向量检索消息片段
   └── Summaries   ← 跨 session 摘要 + 主题索引
   └── AutoRecall  ← prompt 注入相关记忆
   └── AutoExtract ← session 结束后自动存事实 / 摘要
```

## 14.8 SessionArchive · script 可读的历史存档(Default · 默认开)

没装 `muagent-memory` addon 时,也要让 agent 能访问跨 session 历史——**但不付嵌入模型的代价**。
做法:Default Shell 把所有 session 以**明文 JSONL + meta.json**形式存档,并**把存档路径注入到 system prompt**。LLM 可以用已有的 `fs.list` / `fs.read` / `sh.exec`(rg/grep/jq/awk)自己写脚本搜索。

这是个**极简跨 session 查询方案**,99% 个人场景已经够用。只有需要语义检索或自动回填时才升级到 `muagent-memory` addon。

### 14.8.1 存档布局

```
<archive_root>/
├── index.jsonl                    ← 所有 session 头部(append only)
├── <session_id>/
│   ├── meta.json                  ← {title, created_ms, updated_ms, topics, turn_count,
│   │                                   part_count, archived_msg_count, status}
│   ├── parts.jsonl                ← 已封存 part 的索引(append only)
│   │                                一行一条:{part, file, turn_range, bytes, brief}
│   ├── transcript-0001.jsonl      ← 最老的已封存 part
│   ├── transcript-0002.jsonl
│   ├── ...
│   ├── transcript-current.jsonl   ← 当前追加的 part(达阈值后会被轮转改名)
│   └── summary.txt                ← 存在压缩时,最新的整体摘要
└── <session_id_2>/
    ...
```

**单文件时的简化形态**:小 session 只有 `transcript-current.jsonl`,`parts.jsonl` 为空或不存在;只有超过阈值才开始分片。

`<archive_root>` 默认:
- Linux/Pi: `~/.muagent/sessions/`
- macOS: `~/Library/Application Support/MuAgent/sessions/`
- iOS: `<app sandbox>/Documents/MuAgent/sessions/`
- Android: `<app files dir>/muagent/sessions/`

存档**由 Shell 在 SessionStore.save_delta 之后以 fire-and-forget 方式写**(不阻塞主 loop,失败只记事件)。

### 14.8.2 Prompt 注入

Default `ToolSetProvider` 在 `prompt_augmentation` 尾部附加:

```text
## Session archive

Your past sessions are archived at:
  {archive_root}

Top-level:
  index.jsonl                   one line per session with title + timestamps + id

Per-session layout (long sessions are split into parts):
  <session_id>/
    meta.json                   {title, timestamps, topics, turn_count,
                                  part_count, archived_msg_count, status}
    parts.jsonl                 index of sealed parts (append-only);
                                each line: {part, file, turn_range, bytes, brief}
    transcript-0001.jsonl       sealed earlier part (oldest)
    transcript-0002.jsonl       ...
    transcript-current.jsonl    active part (growing; smallest at newest turns)
    summary.txt                 (optional) latest compaction summary

Search strategy for long sessions:
  1. Read parts.jsonl first — each line has a "brief" describing what that part covers.
     This is your table of contents; don't read full part files blindly.
  2. Pick the parts you want and fs.read those specific files.
  3. transcript-current.jsonl is always the most recent activity.

You can use:
  - fs.list / fs.read / fs.stat  (always available)
  - sh.exec with rg / grep / jq / awk  (if shell tool is available)

Examples (when shell is available):
  # Recent 5 sessions by time:
  ls -t {archive_root} | head -5
  # Briefs of all parts for a session (fast skim):
  cat {archive_root}/<session_id>/parts.jsonl | jq -r '.brief'
  # Find sessions mentioning a keyword:
  grep -l "<keyword>" {archive_root}/*/transcript-*.jsonl
  # Titles of last 10 sessions:
  tail -10 {archive_root}/index.jsonl | jq -r .title

When long-term memory addon is loaded, prefer memory.search for semantic
queries; use archive for exact keyword / grep-style search regardless.
```

注入的 `{archive_root}` 是**当前平台的真实路径**(URI 或绝对路径)。

### 14.8.3 写入时机

- `on_save_delta(state, events)`:
  1. `transcript-current.jsonl` 追加 `state.history[meta.archived_msg_count..]` 的新 messages
  2. `meta.json.archived_msg_count` / `updated_ms` / `turn_count` 更新
  3. **检查是否需要轮转**(见 14.8.11);要轮转就走轮转流程
- `Event::HistoryCompacted`:写/更新 `summary.txt`,老 transcript part 保持完整(archive ≠ 工作 history,不删)
- `Event::SessionEnd`:更新 `meta.json.status = "done"` + 追加到 `index.jsonl`;触发一次 final 轮转(把 current 封存成最后一 part)可选
- 自动 title 生成完毕:更新 `meta.json.title`

写入路径都经 Adapter 的 `FileSystem`;同一 session 内写入必须串行(每个 session 一把轻量 mutex,不同 session 并行 OK)。

### 14.8.4 与 SessionStore(JSONL)的关系

两套存储**冗余共存**(尽管都用 JSONL,目录结构和读路径不同):
- `SessionStore`:主存储,带 CAS / event_seq 严格序、`list_runs` / `query_events` / kv 等结构化 API
- `SessionArchive`:**辅助 script 可读视图**,扁平化的 transcript + brief,LLM 友好,只追加、不保证强一致

典型情况下两者数据一致;极端情况下 archive 落后几 step 也不影响正确性(Runner 只认 SessionStore)。

### 14.8.5 可关闭

```rust
pub struct ShellConfig {
    pub session_archive: bool,        // 默认 true
    pub session_archive_root: Option<PathBuf>,  // 默认见 14.8.1
    pub session_archive_prompt_hint: bool,      // 默认 true
    ...
}
```

**何时关**:
- 隐私场景(不想把 transcript 写进明文文件)
- 磁盘空间紧
- 纯 API 后端(只 Runner + Model,不需要 agent 翻历史)

关掉后 prompt 不注入路径,也不写 archive 文件。只 SessionStore 自己工作。

### 14.8.6 与 `muagent-memory` addon 的关系

**正交**。addon ON 时 archive 仍然写(除非显式关)。LLM 可以混着用:
- 精确匹配(grep keyword)→ archive
- 语义检索(类似问题的 session)→ memory.search

addon 可以读 archive 作为自己 auto_extract 的数据源:archive 的 transcript.jsonl 是扁平的 LLM-friendly 格式,比解析 SessionStore 的 events.jsonl 更直接。

### 14.8.7 事件

```rust
Event::SessionArchived         { session_id: SessionId, bytes_written: u64, seq: u64 }
Event::SessionArchiveFail      { session_id: SessionId, reason: String, seq: u64 }
Event::SessionArchiveRotated   { session_id: SessionId, part: u32,
                                  file: String, bytes: u64,
                                  turn_range: (u32, u32), seq: u64 }
Event::SessionArchiveBriefReady{ session_id: SessionId, part: u32,
                                  brief: String, seq: u64 }
```

archive 失败不影响主流程;host 可订阅做 UI 警示。

### 14.8.8 平台差异

| 平台 | 默认 archive 可用性 | LLM 脚本能力 |
|---|---|---|
| Linux / macOS / Pi | ✅ 完全 | 完整:rg / grep / jq / awk via `sh.exec` |
| iOS | ✅(app 沙盒内) | **只 `fs.list` / `fs.read`**(iOS 无 `sh.exec`);LLM 自己 JSON 解析 |
| Android | ✅(app files dir) | 部分 `sh.exec`(allowlist);通常依赖 `fs.*` |

iOS 场景下,`fs.list` 列目录 + `fs.read` 读 JSONL 完全够用。LLM 自己解析 JSON 一行一行看。

### 14.8.9 安全

- archive root **限在 app 沙盒或用户配置的根内**;其它应用不可访问(iOS/Android sandbox)
- 写入前过 `sanitize()` 去 secret(与 event / audit 相同规则)
- `memory.forget_session`(装 addon 时)+ `SessionManager::delete_session` 都同步删 archive 目录
- archive 文件可被 OS 自身加密(iOS Data Protection / FileVault 等)

### 14.8.10 默认"轻量"使用路径

没装 addon、只用 archive 的 agent 搜索跨 session:
```
user:  上次我们聊的那个 "xx 项目" 的 API 设计,你当时提了什么来着?

LLM(prompt 里已知道 archive_root + 多 part 布局):
  1. 调 fs.read(archive_root + "/index.jsonl") 或 sh.exec ls -t
     → 找候选 session id
  2. 对候选 session:fs.read(<sid>/parts.jsonl)
     → 每条 part 有 brief(已摘要),快速定位"哪个 part 谈了 API 设计"
  3. 只读相关 part:fs.read(<sid>/transcript-NNNN.jsonl)
  4. 提取内容给用户
```

关键:**"看得到,拿得到"的历史**,不依赖嵌入模型。parts.jsonl 的 brief 是 LLM 快速导航的"目录",避免盲读几 MB 的 transcript。

### 14.8.11 分片 / 轮转策略

**为什么要分片**:
- 单 `transcript.jsonl` 超过 MB 级后,`fs.read` 被 `max_out_tokens` 截断(默认 4096 tokens),LLM 只能看到文件头或尾部一小段
- 长 session 线性扫描慢(iOS 上 fs.read 一个大文件也慢)
- LLM 写 `sh.exec grep` 在巨大文件上跑也慢
- 无法提供"目录级"快速浏览

**触发规则**(任一满足即轮转,但**必须在 turn 边界**执行):
```rust
pub struct ArchiveRotation {
    pub max_bytes: u64,            // 默认 512 KiB
    pub max_turns: u32,            // 默认 50 turn
    pub max_messages: u32,         // 默认 200 message
    pub generate_briefs: bool,     // 默认 true
    pub brief_max_chars: usize,    // 默认 300
    pub brief_model: Option<Arc<dyn ModelAdapter>>,  // None = 复用主模型
}
```

**关键约束**:**绝不在 turn 中间切**。Turn 定义见 [04 §4.7](04-error-policy.md#47-compaction-policyround-2-a7turn-aligned-约束);一条 `assistant` 与其 `tool_calls` 对应的 `tool_result` 必须同在一个 part 里。如果 part 阈值到了但最后一个 turn 还没结束,**延迟到 turn 完成**再轮转。

**轮转流程**(原子):
```
[ 伪代码 ]
lock(session_id)
if current_bytes >= max_bytes OR current_msgs_in_part >= max_messages
   OR current_turns_in_part >= max_turns:
   if last_message is mid-turn: return    // 等 turn 完成
   next_part = meta.part_count + 1
   seal_name = format!("transcript-{:04}.jsonl", next_part)

   // 1. flush 当前 writer
   // 2. 原子 rename transcript-current.jsonl → seal_name
   fs.rename(uri_current, uri_seal)

   // 3. 生成 brief(fire-and-forget,不阻塞主 loop)
   spawn async:
       brief = brief_model.turn(part_messages, brief_prompt).text
       update parts.jsonl line with .brief = brief

   // 4. append parts.jsonl
   fs.append(parts_jsonl, serialize(PartInfo {
       part: next_part,
       file: seal_name,
       turn_range: (current_part_start_turn..current_turn),
       bytes: current_bytes,
       brief: "<pending>",
       sealed_ms: now(),
   }))

   // 5. 创建新空 transcript-current.jsonl
   fs.write(uri_current, b"")

   // 6. 更新 meta.json
   meta.part_count = next_part
unlock
```

**`PartInfo.brief`** 由 `brief_model` 后台生成,内容约束:
- ≤ 300 字符
- 用中文/英文(跟主模型语言匹配)
- 只摘主题(不摘具体操作细节)

例子:
```json
{"part":1,"file":"transcript-0001.jsonl","turn_range":[0,48],
 "bytes":504832,"brief":"讨论 project-x API 接口的设计初稿。LLM 建议用 REST 并给出了 3 个路由示例。用户提出了鉴权方式的疑问。",
 "sealed_ms":1714000000000}
```

没装 brief_model 或生成失败:brief = `"(no brief)"`。不影响 archive 完整性。

**故障恢复**:
- rename 失败(可能 fs 错误):下次 save_delta 重试,skippable
- parts.jsonl append 失败但 seal 成功了:脏状态;修复脚本 / Shell 启动时校验 meta.part_count vs 目录实际文件数,自动修 parts.jsonl
- brief 生成失败:entry 保留 `"<pending>"`,可稍后手动 regenerate

**跨 Run**(同 session 多次 Run)的累计统计:
- `meta.turn_count` 是 session 整体 turn;`archived_msg_count` 是已写 archive 的 message 数
- 新 Run 的 `submit_user_message` 推进 `turn_count`;save_delta 跟进 archive 的 `archived_msg_count`
- 轮转按 part 内独立 turn 计数(即本 part 从 0 开始数),避免"跨 run 转了但跨 part 跨 turn 边界"的复杂情况

**Host 可关**:
```rust
ShellConfig {
    session_archive_rotation: Some(ArchiveRotation::disabled()),  // 不轮转,所有写进 current
}
```
仅极简场景用;通常不建议。

**压缩 / 归档**(可选后续):
- N 天后把老 part 自动 `gzip`,文件名变 `.jsonl.gz`。`prompts_augmentation` 里提一嘴 LLM 可以 `gunzip -c` 或 Shell 自动解压
- Retention policy:超 N 天或超 M 个 part 的自动删除
- v3.1 MVP 不实装,留 config hook

**事件**:
```rust
Event::SessionArchiveRotated {
    session_id: SessionId,
    part: u32,
    file: String,
    bytes: u64,
    turn_range: (u32, u32),
    seq: u64,
}
Event::SessionArchiveBriefReady {
    session_id: SessionId,
    part: u32,
    brief: String,
    seq: u64,
}
```

UI 可以展示"会话已归档 N 部分"等统计。

### 14.8.12 参数建议(per 场景)

| 场景 | max_bytes | max_turns | max_messages | 说明 |
|---|---|---|---|---|
| iOS app(手机) | 256 KiB | 30 | 120 | 小文件,fs.read 一口气读完不会被截 |
| macOS/Linux 桌面 | 1 MiB | 100 | 400 | 桌面磁盘、内存宽裕 |
| Pi daemon | 512 KiB | 50 | 200 | 平衡 |
| 服务端 / API | 4 MiB | 500 | 2000 | 大文件,grep 快 |

参数可由 `ShellConfig.rotation` 或 profile 预置。

## 14.9 Default Shell 保证

Shell 构造完的 Runner 默认行为:
- 首次 `submit_user_message` 后自动生成 session title(fire-and-forget 摘要)
- 每个 ModelTurn 前检查压缩阈值,必要时压缩
- **每次 save_delta 后同步追加 SessionArchive(JSONL + meta.json)**
- **Prompt 注入 archive 路径 + 用法提示**
- `list_sessions` / `continue_session` / `fork_from` API 可用
- `session.note` / `session.notes_list` meta-tool 可用
- CLI / iOS / Android demo 默认显示 session 列表

这些都是"开箱即用",host 用 `muagent::default_agent(...)` 直接拿到完整能力。

## 14.10 v3 → v3.1 的增改

| v3 | v3.1 |
|---|---|
| `SessionStore` 只有 save_delta / load_run | 扩展:`list_runs` / `delete_run` / `query_events` / `kv_*` |
| 没有明确的 SessionManager | Shell 里有 `SessionManager` 一等类,支持列表/续聊/fork/搜索 |
| Compaction 是 "可选 addon" | **Default Shell 默认装配**(`SummaryCompactionStrategy`,threshold 80%) |
| 自动 session title | 新增:首次消息后 fire-and-forget 摘要 |
| KV 不明确 | 新增:session-scoped kv 作为 Core SessionStore 能力 |
| 无跨 session 脚本可读接口 | 新增:**SessionArchive**(JSONL + meta.json);prompt 默认注入路径 |

## 14.11 测试要点(M1)

1. **session fork**:从 Run A turn 2 分叉出 Run B,B 的 history 前 2 turn 与 A 相同,第 3 turn 起独立。
2. **续聊合并 history**:`continue_session(S)` 新建的 Run 包含 S 下全部历史 run 的 history(按时间顺序)。
3. **压缩触发**:构造一个长 history 超过 80% ctx_len → ModelTurn 前自动触发 → history 变短 + Event::HistoryCompacted 发出。
4. **turn-aligned 不违反**:强制压缩一个假 history(assistant.tool_calls 后紧跟 tool_result)→ compaction 要么整 turn 压,要么不压,**绝不**只压 assistant 不压对应 tool_result。
5. **title 自动生成**:提交 user message "帮我整理照片",500ms 内 `SessionInfo.title` 变成 "整理照片" 或类似。失败时用前 20 字兜底。
6. **kv scope**:`session.note` 跨 Run 可见(同 session_id 下),不同 session 不可见。
7. **SessionArchive 落盘**:每次 `save_delta` 后 `<archive_root>/<sid>/transcript-current.jsonl` 新增对应 messages;`meta.json` 的 `updated_ms` / `turn_count` / `archived_msg_count` 跟进。
8. **SessionArchive prompt 注入**:默认 profile 下 `ActiveToolSet.prompt_augmentation` 末尾含 `archive_root` 绝对路径、多 part 布局说明、parts.jsonl 用法示例。
9. **SessionArchive LLM 可用**:给 agent 问"上次聊 X 的是哪个 session" → LLM 先读 `index.jsonl` + `parts.jsonl`,再读相关 part 文件能找到。
10. **archive 关闭**:`ShellConfig.session_archive = false` 时不写文件、prompt 也不注入提示。
11. **轮转触发**:注入足够长对话使 `transcript-current.jsonl > max_bytes` →  在下一 turn 结束后轮转 → `transcript-0001.jsonl` 出现,`parts.jsonl` 新增一行,`transcript-current.jsonl` 变空。
12. **轮转 turn-aligned**:人工构造一个 turn(assistant + 3 个 tool_result)在轮转阈值处 → 轮转延迟到最后一个 tool_result 落地后才发生,绝不半切。
13. **Brief 生成**:轮转后 fire-and-forget 调 brief_model → `Event::SessionArchiveBriefReady` 发出 → `parts.jsonl` 对应行 `.brief` 从 `"<pending>"` 变成实际摘要。
14. **故障恢复**:模拟 rename 成功但 parts.jsonl append 失败 → Shell 启动时校验目录扫描 vs meta → 自动补写 parts.jsonl 条目。
