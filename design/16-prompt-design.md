# 16 · Prompt 设计(v3.1 · Core Contract + Shell Default · 缓存友好)

> 目前文档已经定义了 `RunState`、`ToolExecutor`、`SessionManager`、`CompactionStrategy`。
> 但还有一个关键对象没有被正式设计:
>
> **Prompt 本身。**
>
> 对 agent runtime 来说,`prompt` 不是一段随便拼出来的字符串,而是:
> - 成本边界
> - 延迟边界
> - cache 命中边界
> - tool-use 可靠性边界
> - thinking / tool loop 连续性边界
> - 多平台 / skill / MCP / memory 编排边界

**关键立场**:

- **Prompt 的结构、持久化边界、cache 语义属于 Core contract**
- **默认 prompt 文案、skill/MCP/archive/memory 的注入策略属于 Default Shell**

## 16.1 为什么需要单独设计 Prompt

如果没有 prompt 设计约束,系统会自然滑向这些坏模式:

- 把当前时间、随机 id、budget 计数、roots 列表每轮都塞进 system prompt 开头
- skill / MCP / archive / memory 提示以不稳定顺序拼接
- 工具 schema 已经在 API `tools` 里出现,system prompt 又重复描述一遍
- 把静态规则和动态事实揉成一个巨大字符串
- 每轮都改 system prompt 头部,导致 prompt cache 几乎失效

这会直接带来:

- cache 命中率下降
- latency 增大
- cost 增大
- tool 调用稳定性下降
- 不同 provider 的行为更难对齐

## 16.2 Prompt 为什么属于 Core

如果从第一性原理判断:

**凡是会影响"下一次模型到底看到了什么"的东西,都属于 core。**

Prompt 满足这个条件,因为它直接决定:

- 哪些 tool schema 会被模型看到
- 哪些 system/developer 规则参与本轮推理
- 哪些动态 runtime facts 进入上下文
- 哪些历史 messages / tool results / thinking artifacts 会被重放
- 续跑 / thaw / tool continuation 后,模型看到的上下文是否和中断前一致

因此应该区分:

- **Core 负责**
  - Prompt 的分层结构
  - `build_request` 的规范
  - 哪些部分是 cacheable prefix
  - 哪些部分是 dynamic tail
  - Prompt version / cache key / runtime facts 的协议
- **Shell 负责**
  - 默认基础提示词文案
  - skill / MCP / archive / memory 的默认注入
  - provider-specific 的具体模板实现

## 16.3 外部参考得到的几个硬结论

### OpenAI

OpenAI 的 prompt caching 文档强调:

- cache 命中依赖 **exact prefix match**
- 静态内容应放前面
- 动态内容应放后面
- `tools` 也属于可缓存前缀的一部分
- 可以用 `prompt_cache_key` 改善共享前缀的路由命中
- 较新模型支持 `prompt_cache_retention`

这意味着:

**只要你把当前时间放进 system prompt 前缀,就等于主动破坏缓存。**

### Anthropic / Claude

Anthropic 的 prompt caching 文档更明确:

- 缓存按 `tools → system → messages` 的顺序形成前缀层级
- 静态内容应该放在前面
- 一个 cache breakpoint 通常就够
- 如果不同段更新频率不同,可以用多个 breakpoint
- 默认 TTL 为 5 分钟,也支持更长 TTL

这意味着:

**prompt 的“分层”不是风格问题,而是性能问题。**

### Codex

Codex 的官方开源 prompt 与 `AGENTS.md` 机制说明了另一点:

- harness 有一层稳定的基础指令
- repo / 目录级 `AGENTS.md` 再叠加
- 用户输入和运行时信息在外层追加

这不是显式的 caching 文档,但它给了一个很合理的实现方向:

**稳定基础前缀 + 分层追加局部规则 + 最后才放动态上下文。**

## 16.4 Prompt 的四层模型

Core 应把 prompt 明确拆成四层,Default Shell 提供默认实现:

### L0 · Invariant Prefix

几乎不变,目标是最大化缓存:

- 产品身份
- 输出风格与沟通规则
- tool-use 总协议
- 安全边界的高层说明
- 与模型快照绑定的长期稳定指令

这层应尽量:

- 按版本管理
- 只在升级 prompt 版本时变化
- 不含任何运行时事实

### L1 · Session-Sticky Prefix

在一个 session 内相对稳定,允许缓存,但变化频率高于 L0:

- repo / workspace 的 `AGENTS.md` 规则
- 激活 skill 的 `prompt_hint`
- archive 路径提示
- 长期记忆 auto-recall 片段
- session 级别的语言 / domain 约束

这层允许在 session 之间不同,但**不应在每个 turn 都变**。

### L2 · Dynamic Runtime Context

这是高频变化层,必须放在 cacheable prefix 后面:

- 当前时间 / 日期 / 时区
- 电量 / 网络 / 背景预算
- 当前 roots / 权限可见性
- 当前 run_id / step / attempt
- 最新 capability 漂移
- host 注入的临时提示

这层不应该污染 L0/L1。

### L3 · Conversation Tail

真正的对话历史与当前输入:

- 历史 messages
- 最近 tool results
- 当前 user input
- 多模态内容

这层天然最动态。

## 16.5 缓存视角下的分类规则

| 内容 | 变化频率 | 应放位置 |
|---|---|---|
| 产品规则 / persona / tool-use 协议 | 很低 | `L0` |
| `AGENTS.md` / skill 提示 / archive 提示 | session 级 | `L1` |
| 当前时间 / 电量 / budget / roots | 每 turn 级 | `L2` |
| 历史对话 / tool_result / 当前用户输入 | 每 turn 级 | `L3` |

一个最核心的判断标准:

**凡是会高频变化的东西,默认都不应该进入 cacheable prefix。**

## 16.6 明确禁止的做法

### 16.6.1 禁止把当前时间放进 system prompt 头部

反例:

```text
You are μAgent. Current time is 2026-04-22T18:03:11+04:00 ...
```

问题:

- 每轮都变
- 直接破坏 OpenAI 的 exact prefix match
- 也破坏 Anthropic 的 `tools → system → messages` 前缀复用

正确做法:

- 放到 `L2 Dynamic Runtime Context`
- 或干脆让模型调 `sys.time.now`

### 16.6.2 禁止把 request id / run id / event seq 放进前缀

这些都是 tracing / audit 字段,不是模型理解任务所必需的稳定规则。

### 16.6.3 禁止把完整 tool 文档重复写进 system prompt

如果 provider 已支持原生 `tools`:

- tool schema
- tool 名称
- 参数约束

应主要依赖 API 的 `tools` 字段。

system prompt 里只保留:

- 什么时候应该用工具
- 工具调用的一般性策略
- 少量高层规则

### 16.6.4 禁止不稳定排序

下面这些内容一旦顺序不稳定,缓存就会变差:

- tools 列表
- skill 提示
- MCP server 提示
- archive part 列表摘要
- memory 注入片段

必须 canonicalize:

- 按稳定 key 排序
- 按固定模板拼接
- 避免哈希表遍历顺序泄漏到 prompt

## 16.7 μAgent 的建议 Prompt 编排

Core 应定义显式的 `PromptPlan` contract,Default Shell 提供默认 `PromptAssembler`。

```rust
pub struct PromptPlan {
    pub invariant_prefix: Vec<PromptBlock>,
    pub session_prefix: Vec<PromptBlock>,
    pub runtime_context: Vec<PromptBlock>,
    pub conversation_tail: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub cache_key: Option<String>,
}
```

### 建议的组装顺序

#### 原生 tool-use provider

```text
tools
→ invariant system/developer blocks
→ session-sticky blocks
→ dynamic runtime blocks
→ conversation history
→ current user turn
```

#### ReAct-fallback provider

```text
invariant system/developer blocks
→ fallback tool-use grammar instructions
→ session-sticky blocks
→ dynamic runtime blocks
→ conversation history
→ current user turn
```

关键点:

- `tool schema` 与 `tool list` 尽量脱离 system prompt
- 动态 facts 只能进后段
- prompt 拼接必须稳定、可测试、可 diff

## 16.8 `build_request` 的职责应该更明确

现在 [03-core-loop](03-core-loop.md) 只写了:

- `base_system_prompt`
- `tools_provider.provide(state)`
- `build_request(state, &ats)`

但没有定义 `build_request` 的 prompt 规范。

建议把它明确成:

```rust
fn build_request(
    state: &RunState,
    ats: &ActiveToolSet,
    runtime_facts: &RuntimeFacts,
    prompt_profile: &PromptProfile,
) -> ModelRequest
```

其中:

- `PromptProfile` 决定 L0/L1 模板
- `ActiveToolSet` 决定 tools 与 session 级 augmentation
- `RuntimeFacts` 决定 L2 动态上下文
- `RunState.history` 决定 L3 tail

## 16.9 `RuntimeFacts` 应单独建模

为了避免所有动态信息都挤进 `prompt_augmentation: String`,建议单独抽:

```rust
pub struct RuntimeFacts {
    pub now: Option<DateTimeInfo>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub background_budget_ms: Option<u32>,
    pub roots_digest: Option<String>,
    pub network_state: Option<String>,
}
```

然后由 `PromptAssembler` 决定:

- 哪些 facts 真需要注入
- 注入成文本块还是 observation
- 是否应改为工具查询而不是 prompt 注入

## 16.10 `prompt_augmentation` 也应分层,不要只是一个字符串

当前 [03-core-loop](03-core-loop.md) / [06-capabilities](06-capabilities.md) 里的:

```rust
pub struct ActiveToolSet {
    pub tools: Vec<ToolDescriptor>,
    pub prompt_augmentation: String,
    pub version: u64,
}
```

太扁平了。

建议升级成:

```rust
pub struct ActiveToolSet {
    pub tools: Vec<ToolDescriptor>,
    pub stable_blocks: Vec<PromptBlock>,   // skill hints / archive hint / stable memory
    pub dynamic_blocks: Vec<PromptBlock>,  // very fresh facts, default empty
    pub version: u64,
}
```

这样才能明确:

- 哪些内容适合缓存
- 哪些内容不适合缓存

## 16.11 OpenAI / Claude 的 provider 策略

### OpenAI

对 OpenAI provider 的默认策略:

- 优先最大化前缀稳定性
- static content 放前
- dynamic content 放后
- 对共享前缀设置稳定的 `prompt_cache_key`
- 长会话或长静态前缀场景可启用更长 retention
- 通过 `usage.prompt_tokens_details.cached_tokens` 监控命中情况

### Anthropic / Claude

对 Claude provider 的默认策略:

- 使用 automatic caching 作为默认起点
- 当不同段变化频率差异明显时,再加显式 cache breakpoints
- 把 `tools` / `system` / `messages` 的边界设计稳定
- 通过 `cache_read_input_tokens` / `cache_creation_input_tokens` 监控

## 16.12 对 `skill / MCP / SessionArchive / Memory` 的具体约束

### Skill

- `prompt_hint` 默认视为 `L1 Session-Sticky`
- 一个 session 内尽量不改写 wording
- 激活顺序必须稳定

### MCP

- 不要把完整 MCP tool schema 展开到 system prompt
- TOC-first 只放稳定摘要
- 真正工具详情尽量留在 API `tools` 或 describe 结果里
- server 顺序要稳定

### SessionArchive

- archive 根路径可以放 `L1`
- 但 part 级滚动统计、最近 part 编号不要每轮都刷新到前缀
- 目录结构说明应稳定

### Long-term Memory

- auto-recall 注入默认归为 `L1`,但只注入高置信、少量、稳定片段
- 低置信或高波动 facts 不应持续放进前缀

## 16.13 Prompt 优化流程

Prompt 设计不能只靠主观感觉,要进工程闭环。

### 版本化

- `PromptProfile` 要有版本号
- 与模型快照一起记录
- prompt 变更视作可评估变更

### Evals

- 每次改 prompt:
  - 测 tool-use 成功率
  - 测 cache hit rate
  - 测 latency
  - 测 token 成本

### 观测指标

- OpenAI:
  - `cached_tokens`
  - prompt tokens
  - latency
- Anthropic:
  - `cache_read_input_tokens`
  - `cache_creation_input_tokens`
  - latency

### 目标

- 不追求“system prompt 越长越聪明”
- 追求“prefix 越稳定越值钱,动态注入越少越准”

## 16.14 推荐默认规则

Default Shell 的默认 `PromptProfile` 建议:

1. L0 基础规则固定,按版本管理
2. `AGENTS.md` / skill hints / archive hint 进入 L1
3. 当前时间**不进入**基础 system prompt
4. roots / budget / network 等动态状态默认不主动注入,能靠工具拿就靠工具拿
5. 若必须注入动态 facts,放在最后一个 developer/system block 或 observation block
6. tool list / skill list / MCP list 全部稳定排序
7. 任何 prompt 变更都要配 eval 与 cache 指标观察

## 16.15 对现有设计文档的影响

这份文档落地后,建议后续同步修改:

- [02-architecture](02-architecture.md)
  - 在 Core 区明确 `PromptPlan / PromptProfile / RuntimeFacts` 是 core protocol
- [03-core-loop](03-core-loop.md)
  - 明确 `build_request` 的职责
  - 把 `base_system_prompt` 扩展为 `PromptProfile`
- [06-capabilities](06-capabilities.md)
  - 把 `prompt_augmentation: String` 升级成分层 block
- [10-observability-security](10-observability-security.md)
  - 增加 prompt cache / prompt version / cache hit 观测指标
- [14-sessions-memory](14-sessions-memory.md)
  - 说明 archive 注入属于 `L1`,不要把高频动态信息塞进 archive hint

## 16.16 参考

- OpenAI Prompt Caching  
  https://platform.openai.com/docs/guides/prompt-caching/prompt-caching
- OpenAI Prompting  
  https://developers.openai.com/api/docs/guides/prompting
- OpenAI Reasoning Best Practices  
  https://developers.openai.com/api/docs/guides/reasoning-best-practices
- OpenAI model pages(cached input / Codex family)  
  https://developers.openai.com/api/docs/models/compare  
  https://developers.openai.com/api/docs/models/gpt-5.2-codex
- Anthropic Prompt Caching  
  https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
- Codex open-source prompt / AGENTS hierarchy  
  https://github.com/openai/codex/blob/main/codex-rs/core/prompt_with_apply_patch_instructions.md  
  https://github.com/openai/codex/blob/main/docs/agents_md.md
