# 10 · 可观测性 × 安全边界

## 10.1 事件与 EventBus(v3:Core 不定义 EventBus)

v3 按 [R5 §4.4](../reviews/15-v5-review.md) 收敛:**Core 不定义 EventBus**。
- Core 的 `Runner::step` 返回 `StepOutput { events, advanced }`;events 是本 step 产生的原始列表。
- 是否广播、是否订阅、是否去重,**由 SDK / host / addon 决定**。
- SessionStore 原子写入 events 时同步持久化(`save_delta(state, events)` 一个事务)——这是 Core 的**持久化契约**,不是事件模型。

### SDK 层的 EventBus(可选 convenience)

`muagent-ffi` / CLI / shell 可能各自包装一个:

```rust
pub type EventSeq = u64;

pub struct EventEnvelope {
    pub run_id: RunId,
    pub seq: EventSeq,
    pub ts_ms: i64,
    pub event: Event,
}

// SDK 层的订阅辅助,不是 Core 概念
pub struct EventFanout {
    tx: broadcast::Sender<EventEnvelope>,
}
```

这只是方便 host 同时接 UI / tracing / audit 多个订阅者的工具类;host 自己不 fan out 也完全 OK——直接读 `StepOutput.events`。

**at-least-once 契约**:
- 事件在 RunState 写入同一事务内追加,确保"已暴露的事件必已持久化"。
- crash 后 thaw,同一 step 可能重新发出事件(例如 ToolCallStart 在 ToolBatch 推进事务之前、事务失败重试后再跑)。
- Host 订阅方**必须按 `(run_id, seq)` 去重**。SDK 层(UniFFI 事件迭代器)提供默认去重器,Host 可关闭。
- `seq` 在 `SessionStore::save_delta` 的同一事务里递增;崩溃后 thaw,下一条事件 seq 从 `state.event_seq + 1` 开始,不会倒退。

**设计点**:
- `broadcast` channel:多订阅者(UI + 日志 + 远端上报)。
- 订阅者故障 / 超时不影响其它订阅者和 Runner。
- 过滤器按 `AuditLevel` 决定 UI 显示什么(Minimal / Standard / Verbose);**审计记录(file-backed store)不受过滤影响**,合规场景始终完整。

## 10.2 事件模型(完整清单)

见 [09-sdk §9.7](09-sdk.md#97-event-类型跨语言一致)。

事件分组(v3.1):
- **Core**:SessionStart / SessionEnd / UserMessage / AssistantDelta / AssistantMessage / ToolCallStart / ToolCallEnd / ToolIntentRecovered / StepAdvanced / Paused / ErrorRaised
- **Shell(Default)**:CapabilityActivated / McpDescribed / RootsChanged / SessionArchiveRotated / SessionArchiveBriefReady / SessionArchived / SessionArchiveFail / HistoryCompacted
- **Addon(muagent-memory,默认关)**:MemoryRecalled / MemoryStored / MemoryDeleted / MemoryAutoExtracted

每条事件必带:`run_id`、`seq`、`ts_ms`。

- `(run_id, seq)` 是去重锚点(at-least-once);
- `ts_ms` 仅辅助 UI 排序,**不是一致性锚点**;
- `correlation_id`(若是 tool call 产生的事件)用 `call_id`。

## 10.3 On-disk Layout(`muagent-storage-jsonl`)

Plain JSONL on disk —— **no native dependencies, no SQL engine**。Host 上的
`grep` / `jq` 可以直接消费;mobile / WASM 也不需要额外数据库绑定。

```
{store_root}/
├── runs/
│   └── {run_id}.jsonl         ← append-only;每行 1 条 { state, events[] }
├── kv/
│   └── {session_id}.json      ← {"key": { updated_ms, value_hex }}
└── audit/
    └── {run_id}.jsonl         ← append-only ToolAuditRecord
```

**save_delta(state, events) 约束**:
1. 在同一把 store 级 I/O 锁下读取该 run 最后一条记录
2. 用 `expected_base = state.event_seq - events.len()` 对比上次已落盘的 `event_seq`
3. 匹配时追加一条新的 `{ state, events }` 记录;不匹配就返回 `StaleState`

`query_events(run_id, since_seq)` 通过重放该 run 的 JSONL 记录并按 `(run_id, seq)`
去重得到结果。`kv_put` 则走 `tmp + rename` 的原子写路径,避免半写入文件。

**fields(每条 audit 行)**:`{ts_ms, run_id, session_id, call_id, tool_name,
side_effects, ok, retryable, args_hash, args_sanitized, brief, duration_ms}`
—— args 只存 SHA-256 + sanitized JSON,brief 截短,**不存 args 原文**。

## 10.4 PII / 敏感数据处理

**默认不写入 audit 的**:
- Tool args 原文(只存 SHA-256 hash)
- Tool result 原文(只存 `brief`,按 hint 规则截取)
- Secrets / token / password(任何名含 `secret|password|token|api_key` 的字段自动 `***`)

host app 想保留完整日志:显式 `AuditLevel::Verbose` + 加密存储。

`SecretStr` 包装器:
```rust
pub struct SecretStr(String);
impl fmt::Debug   for SecretStr { fn fmt(...) -> ... { write!(f, "***") } }
impl fmt::Display for SecretStr { fn fmt(...) -> ... { write!(f, "***") } }
// 只通过 expose_secret() 拿明文
```

## 10.5 日志脱敏(`sanitize`)

所有进 event payload、进 `brief` 的字符串都过一道正则 sanitizer:
```rust
fn sanitize(s: &str) -> String {
    REGEXES.iter().fold(s.to_string(), |acc, (re, repl)| re.replace_all(&acc, *repl).into_owned())
}
// 正则列表:
//   Bearer token (sk-..., pk-..., eyJ...JWT)
//   email
//   IPv4/IPv6(可选)
//   绝对路径前缀(/var/mobile/.../ → <iOS-sandbox>/...)
//   UUID
```

## 10.6 安全边界总览

```
┌───────────────────────────────────────────────────────────────┐
│ 外部(LLM / network / user 输入)                              │
├───────────────────────────────────────────────────────────────┤
│ LLM 可见面(严格最小化)                                      │
│   - system prompt (μAgent 内置 + skill prompt_hint)           │
│   - TOC lines (已过三层门禁)                                  │
│   - active tool schemas                                       │
│   - sanitized tool_result.content                             │
│   - message history(已裁剪)                                  │
├───────────────────────────────────────────────────────────────┤
│ 策略执行层(Rust,LLM 不能绕过)                              │
│   - CapabilityRegistry(白名单 + 三层门禁)                    │
│   - Tool.guard(...)(v3.1:只 Allow/Deny 两种 outcome)        │
│   - OS 授权 in-tool 处理(Adapter 触发系统对话框,失败回       │
│     ToolResult::err;runtime 不关心权限)                      │
│   - NetEgress.policy()                                        │
├───────────────────────────────────────────────────────────────┤
│ 沙盒隔离层                                                    │
│   - ProcessExec allowlist                                     │
│   - FileSystem roots                                          │
│   - Tool timeout / memory cap                                 │
│   - OS sandbox(iOS/Android 容器)                            │
└───────────────────────────────────────────────────────────────┘
```

**核心不变量**:
1. **策略在代码里,不在 prompt 里**。"不要删除系统文件" 不是 LLM 的约束,是 `guard_uri` 的 reject。
2. **LLM 不能要求提升权限"而不经过用户"**。OS 授权由具体 Adapter/Tool 内部调系统 API(例如 iOS `PHPhotoLibrary.requestAuthorization`)触发,对话框由 OS 呈现,用户决定。失败 → Adapter 返 `NotAuthorized` → tool 返 `ToolResult { ok:false, retryable:false, hint:"permission denied; enable in Settings" }` → LLM 看到后让用户去设置启用。Runtime 全程不管。
3. **所有可副作用操作都有审计痕迹**。`tool_audit` 表可查。
4. **网络经 NetEgress 统一出站,便于观测和替换具体实现**。

## 10.7 威胁模型

| 威胁 | 缓解 |
|---|---|
| Prompt injection 让 LLM 调 `fs.delete('/')` | guard_uri 拒绝越权路径;`fs.delete` 标 `destructive` + `user_confirm=always` |
| LLM 被诱导泄露 secrets | `SecretStr` 不暴露给 tool_result;`system.get_secret` 不是工具,只能宿主代码访问 |
| 恶意 MCP server 返回巨大 response 吃光内存 | tool `max_out_tokens` 控制模型可见输出;宿主负责进程级资源治理 |
| MCP server 访问网络资源 | NetEgress 统一观测出站;不在运行时做隐藏 host/网段拦截 |
| Tool panic 带敏感信息进日志 | `sanitize_panic` + `SecretStr` |
| 背景 agent 偷偷长时间运行 | `BudgetExceeded{WallTime}` + `BackgroundBudget` 双保险 |
| 权限升级:LLM 要求 iOS 权限以访问敏感数据 | 权限对话框由 Adapter 直接调系统 API 触发,用户决定;LLM 不能绕过。Runtime 不持有权限 API |
| Skill Pack 携带恶意代码 | Skill Pack 源码可审计;动态加载仅限 Linux,发布前 checksum 验证 |
| Session 历史被恶意读出 | Storage 加密选项(iOS 默认 app 沙盒加密;Linux/macOS 可选文件系统或磁盘级加密) |
| Model 文件被篡改 | `ModelRef.expected_sha256` 校验,不匹配拒绝加载 |

## 10.8 审计导出

```rust
#[tool(name = "system.audit_export")]
async fn audit_export(ctx: &ToolCtx<'_>, since_ms: i64)
    -> Result<AuditBundle, ToolErr>
{
    let events = ctx.pal.storage.query_events(EventQuery::since(since_ms)).await?;
    let audits = ctx.pal.storage.query_tool_audit(since_ms).await?;
    Ok(AuditBundle { events, audits, exported_at: now_ms() })
}
```

用户可主动导出用于排查 / 合规 / 隐私审查。

## 10.9 调试 UI 建议(host app 参考)

推荐 host app 提供一个 dev panel(仅 debug build):
- 实时 TOC view(当前有哪些 cap 可用)
- 事件流实时显示(过滤 kind)
- Tool call 详情(args / result / 耗时 / retry)
- 错误分布图(哪个 tool 错得多,为什么)
- Session tree(支持 fork 切换)

## 10.10 遥测(可选,默认关闭)

框架本身不主动上报。提供 `TelemetrySink` trait,host 可实现把特定事件(`ErrorRaised`、`StallDetected`)转发到 Sentry / Crashlytics。

## 10.11 事件契约速览

- **传递语义**:at-least-once,`(run_id, seq)` 去重。
- **持久化时机**:`SessionStore::save_delta(state, events)` 同事务写入;在此之前发布的事件是"尝试性的"(UI 可先看,但不是审计真相)。
- **顺序保证**:同一 `run_id` 内 `seq` 严格单调;跨 run 无顺序保证。
- **审计**:v3.1 不含 approval 机制;所有 tool 执行都走 ToolCallStart / ToolCallEnd 事件,含 `args_hash` 供事后追溯。
- **敏感字段**:`ToolCallStart.args_brief` 过 sanitizer;完整 args 只进 `tool_audit.args_hash`(SHA-256),不进事件 payload。
