# μAgent 设计文档索引(v3.1 · Core + 默认 Shell · 无审批/权限)

μAgent 是一个跨平台、开源的 on-device agent 运行时。
核心用 **Rust** 写,通过 Adapter 层适配 iOS / Android / Linux SBC(Raspberry Pi 等)。

**v3.1 说明**:在 v3 三层架构基础上,按用户决定**进一步收缩**:

- **删除** `approval` / `permission` 所有机制(Core / Step / trait / addon 全部不保留)
- **删除** `Step::Suspended` / `ToolOutcome` / `Resumption` 通用挂起机制(YAGNI)
- `ToolExecutor::execute` 直接返回 `ToolResult`(更简单)
- **skill / MCP / tool 作为 Default Shell 的默认功能**(非可选 addon)
- 当前架构实质**两层**:Core + 默认 Shell;Plugins 位置保留,暂不实现任何 plugin
- 未来若真需要审批/权限,用"新增 Step variant + 前向兼容"方式演进

保留(v3 继承):
- Core **4 个 trait**:`ModelAdapter` / `ToolExecutor` / `SessionStore` / `ActiveToolSetProvider`(最后一个带 `Fn(&RunState)->ActiveToolSet` blanket impl,host 可传闭包也可自定义 struct);+ 可选 helper `Clock`
- **Prompt / Thinking 属于 Core contract**:前者定义请求分层与 cacheable prefix,后者定义 reasoning artifacts 的 replay / persistence / visibility 语义;默认文案与 UI 效果由 Shell 提供
- `ToolBatch` 多 call(LLM 协议需要)
- `ToolIntent` 中断协议(opt-in per tool)
- step 粒度持久化 + at-least-once 事件 `(run_id, seq)`
- `RunState.schema_version` + migration

v3.1 新增 Default Shell 必需功能:
- **SessionManager**:多会话管理(list / continue / fork / search / 自动 title),跨 Run 共享 SessionId
- **CompactionStrategy**:自动历史压缩(默认 `SummaryCompactionStrategy`,80% ctx 触发,turn-aligned)
- **SessionArchive**:明文 JSONL 存档(长会话按阈值自动分片,每片配 brief 目录)+ prompt 自动注入路径;没装长期记忆 addon 时,LLM 用 `fs.*` / `sh.exec` grep 脚本就能搜跨 session 历史
- **SessionStore KV**:session-scoped key-value(`session.note/notes_list` meta-tool 使用)

v3.1 可选 addon(默认关):
- **`muagent-memory`**:内嵌向量检索的长期记忆(Facts / Episodes / Summaries + auto-recall / auto-extract)。见 [15-long-term-memory](15-long-term-memory.md)

## 阅读顺序

| # | 文件 | 内容 |
|---|---|---|
| 01 | [overview](01-overview.md) | 项目定位、目标、非目标、与同类对比 |
| 02 | [architecture](02-architecture.md) | **v3.1**:Core + Default Shell 两层;Plugins 空置 |
| 03 | [core-loop](03-core-loop.md) | **v3.1**:FSM(Ready/ModelTurn/ToolBatch/ToolIntent/Done/Failed/Paused);ToolExecutor 直接返 ToolResult |
| 04 | [error-policy](04-error-policy.md) | ErrorPolicy / Stall / Budget 作为外层 decorator / addon |
| 05 | [adapters](05-adapters.md) | **v3.1**:Shell 实现细节;`PermissionBroker` 彻底删除;`GuardOutcome` 只 Allow/Deny |
| 06 | [capabilities](06-capabilities.md) | Default Shell 的子模块:skill / MCP / tools / TOC-first |
| 07 | [filesystem](07-filesystem.md) | 通用 fs.* 工具 + 平台特定 URI / 权限 |
| 08 | [platforms](08-platforms.md) | 各平台能力矩阵、Model backend 优先级 |
| 09 | [sdk](09-sdk.md) | UniFFI 导出 + Swift / Kotlin 使用形态 |
| 10 | [observability-security](10-observability-security.md) | 事件持久化契约、安全边界 |
| 11 | [roadmap](11-roadmap.md) | M0 以"最小 Core + 默认 Shell"为目标 |
| 12 | [multimodal](12-multimodal.md) | 图像 / 音频 / PDF 多模态输入输出 |
| 13 | [background-execution](13-background-execution.md) | iOS / Android 背景执行(step 粒度 freeze/thaw) |
| 14 | [sessions-memory](14-sessions-memory.md) | **Session 管理 · 历史压缩**(Default Shell 默认功能) |
| 15 | [long-term-memory](15-long-term-memory.md) | **跨 session 长期记忆**(`muagent-memory` addon · 默认关闭) |
| 16 | [prompt-design](16-prompt-design.md) | **Prompt core contract**:cacheable prefix / dynamic tail / provider-specific caching 策略 |
| 17 | [thinking-design](17-thinking-design.md) | **Thinking core contract**:reasoning artifacts / replay / persistence / visibility |

## 评审与讨论

主设计之外的评审、提案、历史材料放同级的 `../reviews/`:

| # | 文件 | 内容 |
|---|---|---|
| R1 | [../reviews/14-runtime-review](../reviews/14-runtime-review.md) | 第三方评审:第一性原理 + 小内核 + 外层(已整合入 v2)。文中引用 v1 文件名(`05-pal.md` / `04-error-model.md`),保留为历史 |
| R2 | [../reviews/15-v2-self-review](../reviews/15-v2-self-review.md) | 作者自审(round 2):v2 的 8 个真实 bug + 10 个规范缺口(已整合入 v2.1) |
| R3 | [../reviews/15-v3-review](../reviews/15-v3-review.md) | 第三方 round-3:跨文档一致性 + permission 边界 + SDK 滞后(已整合入 v2.2) |
| R4 | [../reviews/16-external-benchmark](../reviews/16-external-benchmark.md) | 对照 OpenAI Agents SDK / PydanticAI / smolagents / goose / LangGraph(尚未评估) |
| R5 | [../reviews/15-v5-review](../reviews/15-v5-review.md) | 第三方 round-5:极简主义质询(选择性整合入 v3);v3.1 进一步删 approval/permission |
| R6 | [../reviews/17-final-review](../reviews/17-final-review.md) | 最后一轮收口 review:v3.1 主架构成立,但 04 / 07 / 09 / 10 / 11 / 12 / 13 仍有旧语义残留 |

> v3 里曾规划的 `14-approval-policy.md`(`muagent-approval` addon)已**删除**。

## 设计原则(v3.1 不变量)

1. **Core 极小**:4 个 trait(`ModelAdapter` / `ToolExecutor` / `SessionStore` / `ActiveToolSetProvider`),其中 `ActiveToolSetProvider` 带 `Fn` blanket impl 可传闭包;1 个可选 helper `Clock`;外加两个**核心协议对象**:`Prompt` / `Thinking`。
2. **无挂起机制**:Core FSM 没有"等外部决定"状态。任何需要外部的事情都由 ToolExecutor 实现内部完成,最终以 `ToolResult` 返回。
3. **错误不静默**:Core `Runner::step` 返 `Result<StepOutput, RuntimeError>`;调用方决定分派;Shell 默认组装含 stall / budget / errorpolicy decorator。
4. **OS 权限 in-tool**:adapter 直接触发系统对话框(`PHPhotoLibrary.requestAuthorization` 等);失败 → `ToolResult::err("permission denied; enable in Settings")` 回灌。Runtime 不感知权限概念。
5. **Capability / Skill / MCP / TOC-first 属于 Default Shell**:Core 通过 `ActiveToolSetProvider` trait 获取本 step 的 tool 集合;具体来源(是 Shell 的 registry/skill/MCP 聚合,还是 host 自定义)对 Core 不可见。
6. **持久化粒度 = step**:`SessionStore::save_delta(state, events)` 一个事务;事件 `(run_id, seq)` 单调,at-least-once。`RunState.schema_version` 带 migration。
7. **AtMostOnce 工具中断不重跑**:`Step::ToolIntent` 协议;opt-in per tool。
8. **并行仅限 ReadOnly**:编译期强制 `Concurrency::Parallel ⇒ SideEffects::ReadOnly`。
9. **平台差异在 URI 与实现,不在 tool 名字**:`fs.read` 跨平台都叫这个名字。
10. **用户视角 = Core + Default Shell**:`cargo add muagent` 得到完整形态;Core 目前是 `src/core/` 内部模块,先不拆成独立 crate。
11. **Prompt / Thinking 的结构属于 Core,具体文案与展示属于 Shell**:Core 定义 prompt 分层、thinking replay/persistence;Shell 决定默认提示词、skill/MCP 注入和 UI 呈现。
