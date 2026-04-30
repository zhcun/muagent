# 11 · 路线图(v3.1 · Core + Default Shell 优先)

> 原则:**M0 证明 v3.1 内核边界正确**(4 Core trait / Step FSM / step-level persistence / ToolIntent / SessionArchive);**M1 打满 Default Shell 的默认体验**(skill / MCP / Session 管理 / Compaction);移动端 / 系统 LLM 依次推进。目标硬件是能跑 Linux 的 SBC 及以上(Raspberry Pi 类,200 MB+ RAM)——MCU 不在目标范围(见 §11.9)。
> v3.1 里 **approval / permission 已删**,不在近程 milestone 内。

## 11.1 原则

- **内核先冻结,再扩功能**。M0 让 Core 4 trait 和 Step FSM 定型,其它靠外层装配。
- **每个 milestone 必须能 end-to-end 演示一个独立价值**。
- **正确性(错误 / 幂等 / step-persistence)从 M0 做到位**,不挪到后期。
- **功能面(第几种平台 / 第几种 backend)靠并行的独立 crate 推进**。

## 11.2 M0 · Core 边界成型(2 周)

**目标**:Linux 上跑通最小 agent,具备 v3.1 承诺的全部正确性。
**必须同时交付 Core + 最小 Shell**——Core 自己没 ToolExecutor 实现、跑不起来。

交付物:
- `muagent-core` crate:
  - `RunState` / `Step` 枚举 / `Runner::step` FSM
  - 4 个 Core trait:`ModelAdapter` / `ToolExecutor` / `SessionStore` / `ActiveToolSetProvider`;`Clock` 可选 helper
  - `Event` 最小集 / `RuntimeError` / `ErrorClass`
- `muagent-shell` crate(Default Shell,默认必需):
  - `DefaultToolExecutor`:schema / guard / timeout / catch_unwind(`GuardOutcome` 只 Allow/Deny)
  - `Tool` trait + `#[tool]` 过程宏最小版
  - `Idempotency` 三态语义 + `Step::ToolIntent` 的 at-most-once 协议
  - `CapabilityRegistry` + `SkillManager` 骨架(实际 skill / MCP 在 M1)
  - `DefaultToolSetProvider`(先返"全部活跃 tools",lazy describe 在 M1)
  - 内置 3 个 tool(见下)
- `muagent-storage-jsonl` crate:
  - Append-only JSONL on disk(per-session 文件 + meta.json)
  - `save_delta(state, events)` CAS via `event_seq`(stale-state 检测)
  - `schema_version` 字段 + 前向兼容反序列化
  - **Why JSONL in M0**: zero native deps,跨 mobile / WASM 环境都能跑,
    host script(grep / jq)也能直接读。对一个单进程顺序写的 session log,
    先上 append-only 文件比引入更重的数据库更合适。
- `muagent-adapters-linux` crate:
  - `FileSystem` / `ProcessExec` / `NetEgress` / `Clock` / `Secrets`(最小版)
- `muagent-model-openai` crate:`ModelAdapter` 走 `NetEgress` 的实现
- `muagent-cli` crate:REPL channel

内置工具(3 个):
- `fs.read` / `fs.write`(ReadOnly / Mutating)
- `sh.exec`(Destructive + AtMostOnce)

**M0 必过的验收测试**

Core(mock 实现):
1. **ToolBatch 多 call 顺序执行**:mock ToolExecutor → 注入 `[a,b,c]` → 三个都执行完 → 回 ModelTurn。
2. **AtMostOnce 中断不重跑**:mock ToolExecutor 让 tool 声明 AtMostOnce,save_delta 后 panic;thaw 看到 `Step::ToolIntent` → 注入 interrupted ToolResult。
3. **cancel 触发 Paused**:`runner.cancel()` → 下次 step 开头 → `Step::Paused { HostRequested }` → persist。
4. **Event at-least-once**:`MemorySessionStore::inject_crash_after_save(n)`;thaw 重发事件,订阅方 `(run_id, seq)` 去重正好一份。
5. **schema migration**:手工 v0 JSON → `RunState::migrate_from(v0)` → 返 v1 RunState。

Shell(DefaultToolExecutor):
6. **panic 转 ToolResult**:tool `panic!("x")` → `catch_unwind` → `ToolResult { ok:false, retryable:true, content:"internal: x" }`。
7. **guard deny 转 ToolResult**:路径越权 → `GuardOutcome::Deny` → `ToolResult::err(reason, false, hint)`。
8. **timeout**:tool 跑超 `descriptor().timeout` → `ToolResult::err("timeout", true, ...)`。

**必须两件一起交付**:`muagent-core` 跑通 1–5,`muagent-shell` 跑通 6–8,缺一不可。

## 11.3 M1 · Capability + MCP + Session 全家桶(3 周)

**目标**:Default Shell 变得"真正好用":skill + MCP + Session 管理 + 自动压缩 + 存档全开。

交付物:
- `muagent-shell` 扩展:
  - `ActiveToolSetProvider` 的 `DefaultToolSetProvider`(TOC-first + lazy describe)
  - `McpClient` + `HttpSseTransport` + `StdioTransport`
  - `InterAppSkillBridge`(Linux 端占位,iOS 在 M2)
  - **`SessionManager`**:list / continue / fork / delete / search / 自动 title(fire-and-forget 摘要)
  - **`SummaryCompactionStrategy` + `WithCompaction` decorator**:80% ctx 阈值,默认装配
  - **`SessionArchive`**:明文 JSONL 分片存档 + prompt 自动注入 archive_root + brief-per-part
- `muagent-macros` 的 `#[skill]` 宏
- `skills/skill-git` / `skills/skill-sysmon`
- Meta-tool:`cap.list` / `cap.describe` / `cap.deactivate` / `session.note` / `session.notes_list`

**M1 验收**:
- Pi 上 `muagent` 连 Ollama + `skill-sysmon`,回答"系统状态如何"
- 连 `github` MCP server,LLM 先见 `mcp:github` 一行,调 `cap.describe` 展开再执行
- **Session 管理**:CLI 里 `/new` / `/list` / `/continue <id>` / `/fork <run_id> <turn>` 全能用
- **自动压缩**:连续对话超 80% ctx 触发 `Event::HistoryCompacted`,turn-aligned 不损坏 tool_call/tool_result 对
- **SessionArchive 分片**:触发阈值后 `transcript-current.jsonl` → `transcript-0001.jsonl` + `parts.jsonl` 新增一行 + `SessionArchiveBriefReady` 事件发出
- **Archive LLM 可用**:给 agent 问"上次聊 X",LLM 先读 `index.jsonl` + `parts.jsonl` 再精准挑 part 读取
- **KV note**:`session.note("likes","coffee")` → 下 run 同 session `session.notes_list("*")` 能读回

## 11.4 M2 · iOS SDK + MLX Demo(3 周)

**目标**:iPhone 上离线 agent 跑起来。

交付物:
- `muagent-ffi` crate(UniFFI)+ XCFramework 构建脚本
- `platforms/ios/MuAgentAdapters`:
  - `IOSFileSystem`(sandbox + security-scoped bookmark)
  - `MLXBackend`(ModelAdapter impl)
  - `AppIntentsBridge`(EventKit / Contacts)—— **OS 授权在 adapter 内部处理**,失败回 `ToolResult::err("permission denied; enable in Settings")`
  - `IOSClock`(`budget_hint()` 返 `BudgetHint::IosBackground`)
  - `KeychainSecrets`
- SwiftUI demo app:聊天 UI / Dev panel(事件流 / TOC / RunState) / Shortcuts Action
- 打包 Llama 3.2 3B Q4 MLX 权重
- `skills/skill-calendar`(iOS-first)

**M2 验收**:iPhone 14+ 离线回答"明天下午有空的时间"。BGTask 唤醒能 thaw RunState 续跑。

## 11.5 M3 · Android SDK + llama.cpp Demo(2 周)

- AAR + `muagent-adapters-android`
- `LlamaCppVulkanBackend`
- `IntentBridge`
- Compose demo + Foreground Service
- Doze 下自动 Paused → WorkManager 唤起续跑

## 11.6 M4 · 系统 LLM Backend(3 周)

- `AppleFMBackend`(iOS/macOS 26+)
- `AICoreBackend`(Android;AICore + Gemma 4)
- Backend fallback 链 + `LlmCaps::native_tool_use = false` 下的 grammar / ReAct-fallback
- **grammar-constrained 解码**(llama.cpp GBNF / MLX logit processor)为小模型 tool-use 背书

## 11.7 M5 · Skill 生态(2 周)

- Skill pack manifest
- 示范:`skill-notes` / `skill-tts` / `skill-web-fetch`
- Linux 动态 skill 加载(可选)

## 11.8 M6 · `muagent-memory` addon(2 周)

- `EmbeddingProvider` + `MemoryStore` trait
- Default vector backend TBD(候选:in-process HNSW / external embeddings API);
  目标是不强加重型 native 向量库依赖
- `OnnxEmbedding`(all-MiniLM-L6-v2)
- `AutoRecallProvider` + `AutoExtractMiddleware`
- `memory.*` meta-tool 集
- iOS/Android 本地 embedding 可跑

**M6 验收**:装 addon → 跨 session 问"我之前提过的偏好" → auto_recall 命中 + prompt 注入 → LLM 答对。

## 11.9 On-device 硬件目标

**目标硬件**:能跑 Linux 的设备及以上。

| 类别 | 代表 | RAM | OS |
|------|------|-----|----|
| **SBC(Cortex-A)** | Raspberry Pi 4/5, Rock 5, Orange Pi | 2 – 16 GB | Linux |
| Edge AI | Jetson Orin, Coral | 8 – 64 GB + NPU | Linux |
| 手机 / 平板 | iPhone / Android 旗舰 | 8 – 16 GB + NPU | iOS / Android |

SBC / Edge 已由 `aarch64-unknown-linux-musl` 静态 binary 覆盖(scp 即跑);
手机走 M2 / M3 的 FFI adapter bundle。

## 11.10 并行进行项

- **性能基线**:每 PR 跑 `Runner::step` 微基准 + ContextBuilder + ToolExecutor 开销
- **错误注入测试**:每 milestone 补一轮 fault-injection harness
- **安全审计**:M2 / M4 后各做一次威胁模型复查
- **v3→v3.1 升级指南**:若后期真要加回 approval,文档化"Step variant 加法 + schema_version bump"流程

## 11.11 版本策略

| 版本 | 里程碑 | ABI 稳定度 |
|---|---|---|
| 0.1 | M0 | Core API 会 break |
| 0.2 | M1 | Shell API 可能仍 break |
| 0.3 | M2 | UniFFI binding 可能 break |
| 0.4 | M3 | ibid |
| 0.5 | M4 | Core trait 冻结 |
| 0.6 | M5 | Skill Pack ABI 冻结 |
| 0.7 | M6 | Memory addon ABI 冻结 |
| 1.0 | M5+ 稳定 + 审计 | SemVer 承诺 |

## 11.12 风险与对策

| 风险 | 对策 |
|---|---|
| FSM + RunState 复杂度超预期 | M0 死守 4 trait;RunState 加字段需评审 |
| at-most-once UI 不友好("中断,状态未知") | M2 UI 专门为 ToolIntent 场景设计恢复流 |
| MCP 协议演进快 | `rmcp` crate 跟进;Transport 抽象 |
| UniFFI async + callback 边角 bug | M2 做早期 FFI spike |
| iOS 背景不可预测 | freeze/thaw 基于 RunState 做到 step 粒度 |
| SessionArchive 分片阈值在移动端内存紧时太激进 | profile 可配;实测后调默认 |
| "内核极简"与实际功能诉求冲突 | 三层结构各司其职(Core 不可动) |

## 11.13 v3.1 之后的方向(非近程目标)

- Skill Marketplace
- Multi-agent / sub-agent
- Diff-based session persistence
- Pluggable response grammar
- Voice channel(BLE + on-device ASR + TTS)
- (真要做时)审批 / 远端审批网关 —— 加回 `Step::Suspended { kind, payload }` variant + `muagent-approval` addon
- (真要做时)OS 权限运行时协议 —— 同上
