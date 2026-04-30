# 03 · Core Loop(v3.1 · 极简 FSM · 无挂起/审批/权限)

> v3.1 按用户指示继续收缩:**approval / permission 全部移出设计**;不保留 `Step::Suspended` / `ToolOutcome` / `Resumption` 通用挂起机制。
> Core 的 FSM 纯粹是 `think → act → observe` 循环,不处理任何"等外部决定"的场景。
> 未来若需要审批,可以**新加 Step variant 扩展**(`Step` 是序列化枚举,可前向兼容加字段);目前不做。

## 3.1 职责

Core 只做:
1. 维护 durable `RunState`
2. FSM 单步转移:`step(state) → new state + events`
3. 调 `ModelAdapter`、调 `ToolExecutor`
4. `SessionStore::save_delta` 每 step 原子落盘

**不做**:审批、权限、TOC-first、stall 检测、retry、compaction、事件广播、预算管理。

## 3.2 `Step` 枚举

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Step {
    Ready,

    /// 调 ModelAdapter,解析回复,分派 tool_calls
    ModelTurn,

    /// 本 turn 的 tool_calls;按 cursor 顺序执行;结果立即进 state.history
    ToolBatch {
        calls: Vec<PendingCall>,
        cursor: usize,
    },

    /// AtMostOnce tool 已持久化执行意图;crash 恢复时不重跑
    ToolIntent {
        call: PendingCall,
        intent_ms: i64,
    },

    /// 资源到界 / host 请求暂停
    Paused { reason: PauseReason },

    Done { final_text: String },
    Failed { reason: RuntimeError },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PauseReason {
    BudgetExceeded { dim: String },
    HostRequested,
}
```

**约束**:`Step` 必须是 `Serialize + Deserialize + Clone`。字段**只能加不能删**(前向兼容)。将来新增 variant(例如 `Suspended`)时,bump `RunState.schema_version`。

## 3.3 `RunState`

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct RunState {
    /// 协议版本。thaw 时按 migrate_from 规则处理
    pub schema_version: u32,

    pub run_id: RunId,
    pub session_id: SessionId,
    pub parent_run_id: Option<RunId>,

    pub step: Step,
    pub history: Vec<Message>,

    /// step 级事件单调序号;host 订阅方 at-least-once 去重锚点
    pub event_seq: u64,

    pub usage: Usage,

    pub created_ms: i64,
    pub updated_ms: i64,
}

pub struct Usage {
    pub tokens_prompt: u32,
    pub tokens_completion: u32,
    pub cost_usd: f64,
    pub turns: u32,
    pub tool_calls: u32,
}

impl RunState {
    pub const CURRENT_SCHEMA: u32 = 1;
    pub fn migrate_from(raw: Value, from: u32) -> Result<Self, StoreError> { ... }
}
```

## 3.4 Core 的 3 个必备 trait

```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn caps(&self) -> LlmCaps;
    async fn turn(&self, req: ModelRequest, cancel: CancelToken)
        -> Result<ModelReply, ModelError>;
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// 直接返回 ToolResult。任何外部决定(审批/权限/异步等待)都由实现方
    /// 内部完成后再返回最终 ToolResult。Core 不感知挂起概念。
    async fn execute(&self, call: &PendingCall)
        -> Result<ToolResult, ToolExecutorError>;

    fn idempotency_for(&self, call: &PendingCall) -> Idempotency;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 原子写:RunState 新版本 + 相关 events(同一事务)
    async fn save_delta(&self, state: &RunState, events: &[Event])
        -> Result<(), StoreError>;

    async fn load_run(&self, id: RunId) -> Result<Option<RunState>, StoreError>;

    // v3.1:基础 session 管理 API(Default Shell 的 SessionManager 在此之上构建)
    async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunHeader>, StoreError>;
    async fn delete_run(&self, id: RunId) -> Result<(), StoreError>;

    // 事件查询(用于 UI 展示历史 / compaction 可能用到)
    async fn query_events(&self, q: EventQuery) -> Result<Vec<Event>, StoreError>;

    // KV(session.note 等 meta-tool 使用;按 session_id 命名空间)
    async fn kv_get(&self, session_id: SessionId, key: &str)
        -> Result<Option<Vec<u8>>, StoreError>;
    async fn kv_put(&self, session_id: SessionId, key: &str, value: &[u8])
        -> Result<(), StoreError>;
    async fn kv_list(&self, session_id: SessionId, prefix: &str)
        -> Result<Vec<(String, Vec<u8>)>, StoreError>;
}

/// 给 list_runs 用的轻量过滤
pub struct RunFilter {
    pub session_id: Option<SessionId>,        // 只列某 session 下的 run
    pub status: Option<RunStatus>,             // active / done / paused / failed
    pub since_ms: Option<i64>,
    pub limit: Option<usize>,
}

/// list_runs 返回的头部信息(不含完整 history;列表/UI 用)
#[derive(Clone, Serialize, Deserialize)]
pub struct RunHeader {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub parent_run_id: Option<RunId>,
    pub title: Option<String>,                 // 由第一条 user message 摘要而来
    pub status: RunStatus,
    pub turns: u32,
    pub updated_ms: i64,
}

pub enum RunStatus { Active, Paused, Done, Failed }
```

### 可选 helper trait(非 Core 必备)

```rust
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn budget_hint(&self) -> BudgetHint { BudgetHint::Unlimited }
}
```

Runner 构造时 host 传 Clock 或用默认 `SystemClock`。

## 3.5 Runner 构造

```rust
pub struct Runner {
    model: Arc<dyn ModelAdapter>,
    tools: Arc<dyn ToolExecutor>,
    store: Arc<dyn SessionStore>,

    /// 每 step 前调用,提供本 step 的 active tool set + prompt augmentation
    tools_provider: Arc<dyn ActiveToolSetProvider>,

    base_system_prompt: String,
    cancel_token: CancelToken,
    clock: Arc<dyn Clock>,
}

/// Core 看到的 tool set 提供者(trait,不是闭包)
/// - 为什么 trait 而非闭包:允许实现方持有状态(如 registry / skill 激活表)、
///   支持 addon 用 new-type 模式包装(如 `AutoRecallProvider<Inner>` 见 15-long-term-memory)
#[async_trait]
pub trait ActiveToolSetProvider: Send + Sync {
    async fn provide(&self, state: &RunState) -> ActiveToolSet;
}

/// 方便小场景:任意 `Fn(&RunState) -> ActiveToolSet` 闭包自动实现 trait
impl<F> ActiveToolSetProvider for F
where F: Fn(&RunState) -> ActiveToolSet + Send + Sync
{
    async fn provide(&self, state: &RunState) -> ActiveToolSet { (self)(state) }
}

pub struct ActiveToolSet {
    pub tools: Vec<ToolDescriptor>,
    pub prompt_augmentation: String,
    pub version: u64,
}
```

## 3.6 `Runner::step` FSM 伪代码

```rust
impl Runner {
    pub async fn step(&mut self, state: &mut RunState)
        -> Result<StepOutput, RuntimeError>
    {
        if self.cancel_token.triggered() {
            state.step = Step::Paused { reason: PauseReason::HostRequested };
            self.persist(state, &[]).await?;
            return Ok(StepOutput { events: vec![], advanced: true });
        }
        state.updated_ms = self.clock.now_ms();

        match state.step.clone() {
            Step::Ready          => self.on_ready(state).await,
            Step::ModelTurn      => self.on_model_turn(state).await,
            Step::ToolBatch { .. }  => self.on_tool_batch(state).await,
            Step::ToolIntent { .. } => self.on_tool_intent_recover(state).await,
            Step::Paused { .. } | Step::Done { .. } | Step::Failed { .. }
                => Ok(StepOutput { events: vec![], advanced: false }),
        }
    }

    async fn on_ready(&self, state: &mut RunState)
        -> Result<StepOutput, RuntimeError>
    {
        state.step = Step::ModelTurn;
        let events = vec![Event::StepAdvanced { to: "model_turn".into(),
                                                seq: state.next_seq() }];
        self.persist(state, &events).await?;
        Ok(StepOutput { events, advanced: true })
    }

    async fn on_model_turn(&self, state: &mut RunState)
        -> Result<StepOutput, RuntimeError>
    {
        let ats = self.tools_provider.provide(state).await;
        let req = self.build_request(state, &ats);
        let reply = self.model.turn(req, self.cancel_token.child()).await
            .map_err(RuntimeError::Model)?;

        state.usage.tokens_prompt     += reply.usage.prompt_tokens;
        state.usage.tokens_completion += reply.usage.completion_tokens;
        state.usage.cost_usd          += reply.usage.cost_usd.unwrap_or(0.0);
        state.usage.turns             += 1;
        state.push_assistant(&reply);

        let mut events = vec![Event::AssistantMessage {
            text: reply.text.clone(), seq: state.next_seq() }];

        if reply.tool_calls.is_empty() {
            state.step = Step::Done { final_text: reply.text };
            events.push(Event::SessionEnd { ok: true, seq: state.next_seq() });
        } else {
            state.step = Step::ToolBatch { calls: reply.tool_calls, cursor: 0 };
        }
        self.persist(state, &events).await?;
        Ok(StepOutput { events, advanced: true })
    }

    async fn on_tool_batch(&self, state: &mut RunState)
        -> Result<StepOutput, RuntimeError>
    {
        let Step::ToolBatch { calls, cursor } = state.step.clone()
            else { return Err(RuntimeError::InvariantViolation("tool_batch")); };
        if cursor >= calls.len() {
            state.step = Step::ModelTurn;
            self.persist(state, &[]).await?;
            return Ok(StepOutput { events: vec![], advanced: true });
        }
        let call = &calls[cursor];

        // AtMostOnce 保护:crash 中断不重跑
        let idem = self.tools.idempotency_for(call);
        if idem == Idempotency::AtMostOnce {
            state.step = Step::ToolIntent {
                call: call.clone(), intent_ms: self.clock.now_ms(),
            };
            self.persist(state, &[]).await?;
        }

        let mut events = vec![Event::ToolCallStart {
            call_id: call.id.clone(), tool: call.tool_name.clone(),
            seq: state.next_seq(),
        }];
        let result = self.tools.execute(call).await
            .unwrap_or_else(|e| ToolResult::framework_error(e));

        state.push_tool_result(&call.id, &result);
        events.push(Event::ToolCallEnd {
            call_id: call.id.clone(), ok: result.ok,
            retryable: result.retryable, brief: result.brief(),
            seq: state.next_seq(),
        });
        state.usage.tool_calls += 1;
        state.step = Step::ToolBatch { calls, cursor: cursor + 1 };
        self.persist(state, &events).await?;
        Ok(StepOutput { events, advanced: true })
    }

    async fn on_tool_intent_recover(&self, state: &mut RunState)
        -> Result<StepOutput, RuntimeError>
    {
        let Step::ToolIntent { call, .. } = state.step.clone()
            else { return Err(RuntimeError::InvariantViolation("tool_intent")); };
        let r = ToolResult {
            ok: false, retryable: false,
            content: "Previous execution was interrupted; effect status unknown. \
                      Use read-only tools to verify state before retrying."
                     .into(),
            hint: Some("Verify before retry".into()),
            detail: None,
        };
        state.push_tool_result(&call.id, &r);
        state.step = Step::ModelTurn;
        let events = vec![Event::ToolIntentRecovered {
            call_id: call.id, seq: state.next_seq(),
        }];
        self.persist(state, &events).await?;
        Ok(StepOutput { events, advanced: true })
    }

    async fn persist(&self, state: &RunState, events: &[Event])
        -> Result<(), RuntimeError>
    {
        state.event_seq = state.event_seq.saturating_add(events.len() as u64);
        self.store.save_delta(state, events).await.map_err(RuntimeError::Store)
    }
}
```

## 3.7 `Event` 类型(Core 侧)

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    SessionStart { run_id: RunId, seq: u64 },
    SessionEnd   { ok: bool, seq: u64 },
    UserMessage  { seq: u64 },
    AssistantMessage { text: String, seq: u64 },
    AssistantDelta   { text: String, seq: u64 },   // stream

    ToolCallStart       { call_id: CallId, tool: String, seq: u64 },
    ToolCallEnd         { call_id: CallId, ok: bool, retryable: bool, brief: String, seq: u64 },
    ToolIntentRecovered { call_id: CallId, seq: u64 },

    StepAdvanced { to: String, seq: u64 },
    Paused       { reason: String, seq: u64 },
    ErrorRaised  { class: String, brief: String, seq: u64 },
}
```

v3.1 Core 的事件就这些。Shell / addon 可以通过自己的通道发扩展事件,但 Core 不定义。

## 3.8 持久化不变量(保留)

- `SessionStore::save_delta(state, events)` 是原子事务
- 事件 `(run_id, seq)` 单调;host 订阅方去重
- `schema_version` 差异 → `RunState::migrate_from` 处理

## 3.9 用户输入 API

```rust
impl Runner {
    pub async fn submit_user_message(&self, state: &mut RunState, msg: Message)
        -> Result<(), RuntimeError>
    {
        match &state.step {
            Step::Ready | Step::Done { .. } => {
                state.history.push(msg);
                state.step = Step::Ready;
                self.persist(state, &[Event::UserMessage { seq: state.next_seq() }]).await?;
                Ok(())
            }
            _ => Err(RuntimeError::SubmitDuringRun),
        }
    }
    pub fn cancel(&self) { self.cancel_token.trigger(); }
}
```

## 3.10 并发约束

每 `run_id` 的 `step` 必须**串行调用**。SDK 层用 Mutex 保护。`SessionStore::save_delta` 可用乐观并发控制(`UPDATE ... WHERE event_seq = $expected`)。

## 3.11 测试要点

M0 必过:
1. **ToolBatch 多 call 顺序执行**:LLM 返 [a, b, c] → 依次执行 → 全 push 到 history → 回 ModelTurn
2. **AtMostOnce 中断不重跑**:执行中 kill → thaw 看到 ToolIntent → 注入"状态未知" → LLM 看到后 verify
3. **panic 被 ToolExecutor 内部 catch**:tool 主动 panic → 包成 ToolResult(retryable=true) → state.step 不变成 Failed
4. **Event at-least-once**:模拟 store save 后 crash,重启后 thaw,同一 step events 再次出现,订阅方按 seq 去重
5. **schema migration**:v0 JSON → `migrate_from(v0)` → v1 RunState
6. **cancel 触发 Paused**:run 过程中 `runner.cancel()` → 下次 step 立即进 Paused{HostRequested}

## 3.12 v3 → v3.1 移除清单

- `Step::Suspended { kind, payload, since_ms, pending_call_id }` → **删除**
- `ToolOutcome` 枚举 → **删除**(ToolExecutor 直接返 ToolResult)
- `Resumption` 枚举 → **删除**
- `Runner::resume` 方法 → **删除**
- `Runner::resume_with` 方法 → **删除**
- `Event::Suspended` / `Event::Resumed` → **删除**

若未来加回审批或其它"等外部决定"场景:
- 加回 `Step::Suspended` variant(前向兼容:旧 RunState 不含此 variant 依然可解析)
- 加回 `ToolOutcome` 或用 `ToolExecutor::execute` 返的 `ToolResult` 的 `retryable + hint` 字段向 LLM 表达"这事儿需要人工处理"
- 新建 addon crate

bump `RunState::CURRENT_SCHEMA` = 2 并写 migration。
