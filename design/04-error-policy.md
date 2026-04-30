# 04 · Error Policy(v3 · Core 轻分类 · 策略全部 addon)

> v2 把"ErrorPolicy"写成一个独立"Policy 层"。v3 按 [reviews/15-v5-review §3.4](../reviews/15-v5-review.md) 的主张:
> **Policy 层不作为独立主架构层**;它是 wrapper / addon 的集合。
>
> Core 只承担最低职责:
> 1. 给每个 `RuntimeError` 一个 `ErrorClass` 分类(无歧义)
> 2. 提供一个扩展点(decorator pattern),host / addon 可注入 `ErrorPolicy` 决定如何处理各 class
>
> v2 的决策逻辑保留,但在 `muagent-errorpolicy` addon 中实现。Core 默认**直接 propagate 所有错误**;addon 提供"不静默吞"保证 + retry / stall 等更高级策略。

## 4.1 Core 侧最小表达

```rust
// muagent-core/src/error.rs

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("model error: {0}")]
    Model(ModelError),

    #[error("tool executor error: {0}")]
    ToolExecutor(ToolExecutorError),

    #[error("store error: {0}")]
    Store(StoreError),

    #[error("cancelled")]
    Cancelled,

    #[error("invariant violation: {0}")]
    InvariantViolation(&'static str),

    #[error("submit during run")]
    SubmitDuringRun,
}

impl RuntimeError {
    pub fn classify(&self) -> ErrorClass {
        use RuntimeError::*;
        match self {
            Model(e)        => e.classify(),
            ToolExecutor(e) => e.classify(),
            Store(e)        => ErrorClass::Store(e.classify()),
            Cancelled       => ErrorClass::Cancelled,
            InvariantViolation(_) | SubmitDuringRun => ErrorClass::Bug,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// 工具执行错(正常情况下 ToolExecutor 已转 ToolResult 回灌;走到这里是框架错)
    ToolFailure { retryable: bool },

    /// 模型提供方的瞬时错(连接断 / 5xx / timeout)
    ProviderTransient,

    /// 模型提供方的永久错(auth / 参数无效)
    ProviderFatal,

    /// ctx length 超了(触发 compaction 而非重试)
    ContextTooLong,

    /// store 错(应极少见:持久化失败要么重试要么放弃)
    Store(StoreErrClass),

    /// Bug(不变量被破坏、panic 被捕获等)
    Bug,

    /// 用户主动取消
    Cancelled,
}
```

(v3.1 已删 `RuntimeError::Approval` variant / `ErrorClass::PolicyDenied` / `ApprovalError`;approval 完全移出 Core。)

**Core 只负责"分类",不负责"决定怎么做"**。

每个子错误类型(`ModelError` / `ToolExecutorError` / ...)的 `classify()` 方法由其实现方提供;Core 只消费 `ErrorClass` 这个枚举。

### 子错误 `classify()` 规约(round-2 B10)

```rust
impl ModelError {
    pub fn classify(&self) -> ErrorClass {
        match self {
            Self::Transient(_)     => ErrorClass::ProviderTransient,
            Self::Fatal(_)         => ErrorClass::ProviderFatal,
            Self::Auth(_)          => ErrorClass::ProviderFatal,
            Self::InvalidRequest(_)=> ErrorClass::ProviderFatal,
            Self::ContextOverflow  => ErrorClass::ContextTooLong,
            Self::RateLimited { .. } => ErrorClass::ProviderTransient,
            Self::Parse(_)         => ErrorClass::ProviderTransient,  // 流截断等
        }
    }
}

impl ToolExecutorError {
    pub fn classify(&self) -> ErrorClass {
        match self {
            Self::UnknownTool(_)    => ErrorClass::Bug,    // 注册表问题
            Self::SchemaParse(_)    => ErrorClass::Bug,    // 框架问题
            Self::BundleUnavailable => ErrorClass::ToolFailure { retryable: false },
            Self::Internal(_)       => ErrorClass::Bug,
        }
    }
}

impl StoreError {
    pub fn classify(&self) -> StoreErrClass {
        match self {
            Self::Transient(_)  => StoreErrClass::Transient,
            Self::StaleState    => StoreErrClass::Conflict,   // 乐观并发冲突
            Self::Corrupt(_)    => StoreErrClass::Fatal,
            Self::Io(_)         => StoreErrClass::Transient,
        }
    }
}
```

所有子错误类型定义在对应 crate 的 `error.rs`,**不进 Core**;Core 只持有 `RuntimeError` 的包装 variant。

## 4.2 ErrorPolicy trait(Policy 层)

```rust
// muagent-policy/src/error_policy.rs

#[async_trait]
pub trait ErrorPolicy: Send + Sync {
    /// Runner.step 返回 Err 时,由此 policy 决定怎么转化。
    async fn handle(
        &self,
        err: RuntimeError,
        state: &mut RunState,
        ctx: &ErrorCtx<'_>,
    ) -> ErrorAction;
}

pub enum ErrorAction {
    /// 把错误编码成一条 Observation 回灌进 history,下一个 step 继续
    InjectObservation { text: String, obs_kind: ObsKind },

    /// 对当前 step 做一次重试(不改 RunState 其它内容)
    Retry { after: Duration, attempt: u8 },

    /// 把 state.step 设为 Failed 并返回
    Fail { reason: RuntimeError },

    /// 传播给 Runner 调用方(host)
    Propagate { err: RuntimeError },

    /// 进入 Paused,等外部决定
    Pause { reason: PauseReason },
}

pub struct ErrorCtx<'a> {
    pub attempt: u8,
    pub recent_errors: &'a [ErrorRecord],
    pub retry_budget: &'a mut RetryBudget,
}
```

## 4.3 默认实现 `DefaultErrorPolicy`

```rust
pub struct DefaultErrorPolicy {
    pub provider_retry_max: u8,
    pub store_retry_max: u8,
    pub retry_window: Duration,
}

impl ErrorPolicy for DefaultErrorPolicy {
    async fn handle(&self, err: RuntimeError, state: &mut RunState,
                    ctx: &ErrorCtx<'_>) -> ErrorAction
    {
        match err.classify() {
            ErrorClass::ToolFailure { .. } => {
                // Core 约定:ToolExecutor 层应直接产出 ToolResult 回灌,不会变成 Err。
                // 走到此处说明子系统实现漏了 → 降级为 Observation。
                ErrorAction::InjectObservation {
                    text: format!("tool executor error (unexpected path): {err}"),
                    obs_kind: ObsKind::System,
                }
            }
            ErrorClass::ProviderTransient => {
                if ctx.retry_budget.consume() && ctx.attempt < self.provider_retry_max {
                    ErrorAction::Retry {
                        after: exp_backoff(ctx.attempt),
                        attempt: ctx.attempt + 1,
                    }
                } else {
                    ErrorAction::Propagate { err }
                }
            }
            ErrorClass::ProviderFatal => ErrorAction::Propagate { err },
            ErrorClass::ContextTooLong => {
                // 只发信号;实际 compaction 由 CompactionPolicy 接力
                ErrorAction::InjectObservation {
                    text: "[ctx too long; compaction should be triggered]".into(),
                    obs_kind: ObsKind::System,
                }
            }
            ErrorClass::Store(_) => {
                if ctx.attempt < self.store_retry_max {
                    ErrorAction::Retry {
                        after: Duration::from_millis(100),
                        attempt: ctx.attempt + 1,
                    }
                } else {
                    ErrorAction::Propagate { err }
                }
            }
            ErrorClass::Bug => {
                // 关键:pi #2188 的修复核心
                // 不静默吞,降级为 observation 给 LLM,同时记 audit
                // v3.1:state 不持有 error_trail 字段;以 Event::ErrorRaised 事件记录,
                // 历史审计从 SessionStore::query_events 查
                // events.push(Event::ErrorRaised { class: "bug".into(),
                //     brief: err.to_string(), seq: state.next_seq() });
                ErrorAction::InjectObservation {
                    text: format!("runtime internal error (recovered): {err}"),
                    obs_kind: ObsKind::System,
                }
            }
            ErrorClass::Cancelled => ErrorAction::Pause {
                reason: PauseReason::HostRequested,
            },
        }
    }
}
```

## 4.4 ToolExecutor 侧的错误边界

Policy 的 `InjectObservation` 是**降级兜底**,不是主路径。正常情况下:

- Tool 执行异常 → `ToolExecutor::execute` 捕获,返回 **`ToolResult { ok: false, retryable, hint }`**,**不是** `Err`。
- `ToolExecutor::execute` **永远不 panic**——用 `catch_unwind` 包裹实际执行,panic 转 `ToolResult { ok: false, retryable: true, content: "internal panic (recovered): ..." }`。
- 只有 ToolExecutor 本身的框架级错(注册表损坏、schema 解析失败)才返回 `Err(ToolExecutorError)`。

**这一层的 try/catch 在 muagent-tool-executor crate,不在 Core**。Core 只是契约(看到 Err 就走 ErrorPolicy)。

## 4.5 Stall Policy(默认装配的 decorator)

`StallPolicy` 不是 ErrorPolicy 的一部分,是一个独立 decorator,监听 `Event::ToolCallEnd` 流:

```rust
pub struct StallPolicy {
    warn_threshold: u32,     // 默认 3
    abort_threshold: u32,    // 默认 6
    state: Mutex<StallState>,
}

pub enum StallOutcome {
    Ok,
    InjectSteering(String),
    RequestAbort,
}

impl StallPolicy {
    pub fn observe(&self, ev: &Event) -> StallOutcome { ... }
}
```

Host 组装:
```rust
let runner = Runner::new(...);
let stall = StallPolicy::default();
let stall_runner = WithStall::new(runner, stall);
```

**机制(round-2 B3 修复)**:v2 最初说 "`WithStall` 在 step 后读 events",但 step 已经 persist 了——再改 state 又是另一次事务,容易出错。正确做法是**step 之前检查**:

```rust
#[async_trait]
impl<R: RunnerLike> RunnerLike for WithStall<R> {
    async fn step(&mut self, state: &mut RunState, cancel: &CancelToken)
        -> Result<StepOutput, RuntimeError>
    {
        // 仅在 Step::ModelTurn 前检查(即 LLM 即将"看见"history 时)
        if matches!(state.step, Step::ModelTurn) {
            match self.policy.observe(&state.history) {
                StallOutcome::Ok => {}
                StallOutcome::InjectSteering(msg) => {
                    state.history.push(Message::Observation {
                        kind: ObsKind::Steering, text: msg,
                    });
                    // 不自己 persist;让 inner.step 在末尾与本 turn 其它改动一起落盘
                }
                StallOutcome::RequestAbort => {
                    state.step = Step::Failed {
                        reason: RuntimeError::InvariantViolation("stall: stuck loop"),
                    };
                    // 这里需要 persist——因为 inner.step 不会再跑
                    return Ok(StepOutput {
                        events: vec![Event::StallAborted { seq: state.next_seq() }],
                        advanced: true,
                    });
                }
            }
        }
        self.inner.step(state, cancel).await
    }
}
```

**默认启用,可禁用**。pi 死循环 bug 在 MVP 就被修掉,而不是挪到后期。

> 注:`StallPolicy::observe` 现在看 `&state.history`(不是 event stream)。实现需读最近若干条 `tool_result`,判断重复失败签名。这个接口纯函数化、易测。

## 4.6 Retry:只在 ErrorPolicy 层做(round-2 A6)

**早期设计 bug**:v2 最初同时有 `WithRetry<ModelAdapter>` 和 `ErrorAction::Retry`——双层重试导致最坏情况尝试 outer × inner = O(n²) 次,无法配置统一预算。

**修**:**去掉 `WithRetry` decorator**。只在 ErrorPolicy 层决策重试。

```rust
// Runner 的 on_model_turn 遇到 ModelError 时:
match self.model.turn(req, cancel).await {
    Ok(reply) => { /* 正常流程 */ }
    Err(e) => {
        // 委托 ErrorPolicy 决策
        let action = self.error_policy.handle(
            RuntimeError::Model(e), state, &err_ctx).await;
        match action {
            ErrorAction::Retry { after, attempt } => {
                events.push(Event::ProviderRetry {
                    attempt, seq: state.next_seq()
                });
                // 保存事件(持久化失败观察),然后 sleep 并 return(下次 step 再试)
                self.persist(state, &events).await?;
                tokio::time::sleep(after).await;
                return Ok(StepOutput { events, advanced: false });
            }
            // 其它 ErrorAction 照常处理
            _ => { ... }
        }
    }
}
```

这样**所有重试都通过 ErrorPolicy.handle 做决策**,host 想改重试语义只需替换 ErrorPolicy。
Note:v3.1 Core 的 Event 枚举不含 `ProviderRetry`;如果装了 errorpolicy addon,它可通过 addon 自己的事件类型(或 Shell 侧的 `ErrorRaised { class: "provider_transient" }`)表达"正在重试"。

## 4.7 Compaction Policy(round-2 A7:turn-aligned 约束)

```rust
#[async_trait]
pub trait CompactionPolicy: Send + Sync {
    async fn maybe_compact(&self, state: &mut RunState,
                           ctx: &CompactionCtx<'_>) -> Option<CompactionOutcome>;
}

pub struct CompactionOutcome {
    pub replaced_turns: usize,
    pub summary_text: String,
    pub saved_tokens: u32,
}
```

触发时机(Shell 层,非 Core):
- `WithCompaction` decorator 在 `Runner::step` 进入 `ModelTurn` 之前调 `plan(state, ctx)`。
- 触发条件:`tokens_used_in_ctx > threshold`(默认 80% of `LlmCaps::ctx_len`)或 `Event::ContextTooLong` 出现过。
- 默认策略 `SummaryCompactionStrategy`:用 ModelAdapter 调用一次生成摘要,替换最旧 N 个 turn(保留 tail 4 个)。

**关键不变量(round-2 A7 修复)**:

> Compaction 只能以**完整 turn** 为单位操作。

**turn** 定义:
```
turn = user_msg
     | assistant_msg [with tool_calls]
       followed by all corresponding tool_result messages
       (optionally interspersed with Observation messages)
```

禁止:
- 单独丢弃 `assistant.tool_calls` 而保留它们的 `tool_result`(悬挂引用 → provider 400)
- 单独丢弃 `tool_result` 而保留 `assistant.tool_calls`(同上)
- 半拆一条 `assistant` 消息

compaction 实现必须:
1. 识别 message 列表里的 turn 边界。
2. 要替换的每个 turn 必须整体被 summary observation 替换。
3. Summary observation 的 `kind` 标记为 `ObsKind::Summary`,不混淆为 LLM 原生输出。

**v3.1:Default Shell 默认启用**(详见 [14-sessions-memory §14.6](14-sessions-memory.md#146-历史压缩compactionstrategy))。
没压缩的 agent 长聊就崩。用户导入 `muagent` 默认就得到自动压缩。想要极简 Core 自管的 host 可显式关掉。

## 4.8 BudgetPolicy(预算检查 decorator)

```rust
pub struct WithBudget<R> {
    inner: R,
    floor_background_ms: u32,
    clock: Arc<dyn Clock>,
}

impl<R: RunnerLike> RunnerLike for WithBudget<R> {
    async fn step(&mut self, state: &mut RunState, cancel: &CancelToken)
        -> Result<StepOutput, RuntimeError>
    {
        // 硬预算
        if state.usage.tokens_prompt + state.usage.tokens_completion
            >= self.cfg.tokens_total {
            state.step = Step::Paused { reason: PauseReason::BudgetExceeded {
                dim: "tokens".into() } };
            return Ok(StepOutput { events: vec![], advanced: true });
        }
        if state.usage.turns >= self.cfg.turns_max {
            state.step = Step::Paused { reason: PauseReason::BudgetExceeded {
                dim: "turns".into() } };
            return Ok(StepOutput { events: vec![], advanced: true });
        }
        // 背景时间软预算(iOS / Android Worker 等)
        if self.clock.budget_hint().soft_floor_breached(self.floor_background_ms) {
            state.step = Step::Paused { reason: PauseReason::BudgetExceeded {
                dim: "background_time".into() } };
            return Ok(StepOutput { events: vec![], advanced: true });
        }
        self.inner.step(state, cancel).await
    }
}
```

## 4.9 默认 decorator 叠放顺序

```
host → WithBudget            (最外层,最早门禁)
         → WithErrorPolicy   (捕捉 inner 的 Err)
             → WithStall     (观察 events 插 steering)
                 → Runner    (Core FSM)

ModelAdapter 侧:
host → WithTracing → 真实 backend          (round-2 A6:retry 已移到 ErrorPolicy)
```

## 4.10 事件跟踪

Core 已经在 `Runner::step` 的持久化里 emit 了所有必要事件(见 [03-core-loop §3.5](03-core-loop.md))。
Policy 层不需要新 emit,只**读**事件流。`TracingPolicy` 是一个特殊 decorator,把事件转发到 OpenTelemetry / Sentry / 本地 JSONL。

## 4.11 测试(错误处理)

```rust
#[tokio::test]
async fn provider_fatal_propagates() {
    let model = MockModel::returning(Err(ModelError::Fatal("auth".into())));
    let runner = Runner::new(model, ..., ..);
    let policy = DefaultErrorPolicy::default();
    let mut wrapped = WithErrorPolicy::new(runner, policy);
    let err = wrapped.step(&mut state, ...).await.unwrap_err();
    assert!(matches!(err, RuntimeError::Model(ModelError::Fatal(_))));
}

#[tokio::test]
async fn bug_class_injects_observation_not_propagates() {
    let runner = MockRunner::always_errors(RuntimeError::InvariantViolation("x"));
    let policy = DefaultErrorPolicy::default();
    let mut wrapped = WithErrorPolicy::new(runner, policy);
    let out = wrapped.step(&mut state, ...).await.unwrap();
    assert!(state.history.iter().any(|m|
        matches!(m, Message::Observation { text, .. } if text.contains("internal error"))));
    assert!(!matches!(state.step, Step::Failed { .. }));
}

#[tokio::test]
async fn stall_injects_steering_after_3_same_errors() {
    // 3 次相同 tool_name + 相同 error signature
    // 第 3 次后下 step 应看到 Observation(ObsKind::Steering)
    ...
}

#[tokio::test]
async fn stall_aborts_after_6() {
    // 6 次相同失败 → WithStall 把 state.step 设为 Failed(Stuck)
    ...
}
```

## 4.12 与 pi #2188 的关系(最终定位)

| pi 的问题 | v1 方案 | v2 方案 |
|---|---|---|
| 顶层 catch 吞错,产出空 assistant 消息 | 在 `run_loop` 顶层 `match e.domain()` 穷尽分派 | `ErrorPolicy` trait 显式决定每个 `ErrorClass` 的动作,`ErrorClass::Bug` 走 `InjectObservation` 而非静默 |
| tool 错误不带 retryable hint | `ToolResult { retryable, hint }` | 保留,进 Core `ToolResult` 契约 |
| 同一错误反复烧 token | `StallDetector` 进 loop | `StallPolicy` decorator,默认装配 |
| panic 漏到外层 | ToolExecutor `catch_unwind` | ToolExecutor 实现契约,Core 看到 Err 仍能转 `Bug` 分类回灌 |

**关键不变量(v2 强化)**:不论路径如何,**任何错误都有一个明确的 `ErrorClass`**,ErrorPolicy 对其的反应是显式的。**无歧义 = 无静默**。

---

## 4.13 v3 修订说明

按 [R5 §3.4](../reviews/15-v5-review.md):
- **ErrorPolicy 不是独立架构层**。它是一个 decorator pattern,wrapper 放在 host 侧或 `muagent-errorpolicy` addon 里。
- `DefaultErrorPolicy` / `WithErrorPolicy` / `WithStall` / `WithBudget` 仍然存在,但作为**addon 可选提供**。Host 不装 addon 时:
  - Core 直接 propagate 错误给调用方(符合 Rust 惯例)
  - "pi #2188 防静默"只在 addon 装上时才主动修复(通过把 Bug 分类转 InjectObservation 回灌)
  - 不装 addon 的 host 必须自己处理 `Err(RuntimeError)` 返回值
- **StallPolicy** 从默认装配改为 addon 提供的可选 decorator。但 `muagent-shell` 的 default profile 会 **默认启用**,host 用 shell 时自动得到 stall 保护。
- **RetryPolicy** 不存在独立 addon;由 `ErrorPolicy` 处理 `ProviderTransient` 类做退避重试。
- **CompactionStrategy** v3.1 **默认装配进 Default Shell**(`SummaryCompactionStrategy`,80% 阈值);长对话自动摘要。只有极简 Core 模式才关闭。详见 [14-sessions-memory](14-sessions-memory.md)。
- **BudgetPolicy** 是 `muagent-budget` addon 提供的 decorator;包含 iOS 背景时间感知逻辑。

装配示例(host 用 shell 默认组合):
```rust
// shell 已内置:WithErrorPolicy + WithStall + WithBudget
let runner = muagent_shell::default_runner(model, tools, store, tools_provider);
```

装配示例(host 自己挑):
```rust
let runner = Runner::new(model, tools, store, tools_provider);
let runner = muagent_errorpolicy::WithErrorPolicy::new(runner, DefaultErrorPolicy::default());
// 不加 stall / budget 也 OK,Core 直接跑
```

## 4.14 v3 对 Core 的错误契约

Core 的 `Runner::step` 返回 `Result<StepOutput, RuntimeError>`:
- `Ok(StepOutput)`:step 成功推进或 no-op(Done / Failed / Paused 等终态)
- `Err(RuntimeError)`:调用方决定怎么办——
  - 直接 propagate(默认 Rust 习惯)
  - 用 `WithErrorPolicy` wrapper 拦截转 InjectObservation(保 pi #2188 防线)
  - 自定义 ErrorPolicy 实现

Core 本身 **不做** "bug 转 observation"的降级——这个行为属于 ErrorPolicy 策略,由 addon 或 shell 默认组合实现。

这个改动符合 R5 的"极简"精神:Core 不抱产品级保证,只抱正确性(错误不丢、不静默)。更高层的"友好度"靠 addon 兜底。

## 4.15 与 v2.2 的差异

| 维度 | v2.2 | v3 |
|---|---|---|
| Policy Layer | 主架构层 | 不是层,是 decorator / addon |
| ErrorPolicy trait | 在 muagent-policy crate | 在 muagent-errorpolicy addon crate |
| StallPolicy | 默认装配(Core 不知道但 shell 默认加) | addon;shell 默认加;host 直接用 Core 时不加 |
| CompactionPolicy | 非默认 | addon;不默认 |
| BudgetPolicy | decorator | addon |
| Bug → Observation 降级 | 承诺 | 仍承诺,但由 WithErrorPolicy 实现(非 Core 自动) |
