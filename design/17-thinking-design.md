# 17 · Thinking 设计(v3.1 · Core Contract + Shell UI)

> 这里说的 `thinking` 不是 UI 上一个"思考中动画"。
>
> 这里说的是:
> - 模型原生返回的 reasoning / thinking artifacts
> - 它们在 tool loop 中如何被保留与重放
> - 它们如何影响 cache、resume、cost、observability
>
> 结论先写在前面:
>
> **Thinking 协议属于 Core。**
> **Thinking 的展示效果属于 Shell/UI。**

## 17.1 为什么 Thinking 属于 Core

从第一性原理看,判断标准很简单:

**凡是会影响下一次模型调用正确性和跨 step 恢复正确性的东西,都属于 Core。**

Thinking 满足这个条件,因为它会影响:

- tool 调用前后的 reasoning continuity
- tool result 返回后的后续推理质量
- provider 对下一轮请求的接受与否
- crash / thaw 后能否正确续跑
- prompt cache 与 token 成本

如果不把 thinking 设计成正式协议,系统就会出现两种错法:

- 把它当纯 UI 文本,随手显示或丢弃
- 把它当 provider 私有细节,不进入 RunState / resume 语义

这两种都不对。

## 17.2 Core 与 Shell 的边界

### Core 负责

- 定义 thinking 的请求参数
- 定义 thinking artifacts 的统一表示
- 定义 replay / preserve / drop 语义
- 定义 tool loop 里的 thinking continuity
- 定义持久化与 thaw 规则
- 定义 thinking 的可见性策略与审计边界

### Shell / UI 负责

- 是否显示"思考中"
- 怎样渲染 thinking 摘要
- 是否用打字效果/折叠面板展示
- redacted_thinking 的用户文案
- 面向产品的开关与默认值

## 17.3 外部参考得到的硬结论

### OpenAI

OpenAI 在 reasoning best practices 里明确指出:

- 在 `Responses API` 下,某些 reasoning items 会在后续请求中被自动纳入上下文
- 对复杂 tool use,应保留前一次 tool call 相邻的 reasoning items
- `Chat Completions` 是无状态的,不会把 reasoning items 放回上下文

这意味着:

**thinking 不是纯显示层信息,而是会影响下一轮 tool-use 质量的上下文。**

### Anthropic / Claude

Anthropic 的 extended thinking 文档更直接:

- 开启 thinking 后,响应会返回 `thinking` blocks
- tool use 期间,必须把最后 assistant turn 的 thinking blocks **完整且不修改地**传回
- Claude 4 还支持 interleaved thinking
- 可能出现 `redacted_thinking`

这意味着:

**thinking artifacts 不只是可选日志,而是 provider 协议的一部分。**

Anthropic 的 prompt caching 文档还补了一条:

- thinking blocks 不能直接显式 cache
- 但在 tool continuation 时会作为请求内容一起被缓存/读取
- 如果加入新的非 tool-result user 内容,旧 thinking blocks 会被剥离

这说明:

**thinking replay 与 prompt caching 是耦合的。**

## 17.4 μAgent 应定义的请求侧抽象

```rust
pub enum ThinkingMode {
    Off,
    Auto,
    Enabled,
}

pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    pub effort: Option<ThinkingEffort>,
    pub budget: Option<ThinkingBudget>,
    pub visibility: ThinkingVisibility,
}

pub enum ThinkingEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

pub enum ThinkingBudget {
    Tokens(u32),
    Relative(u8),   // 例如 0-100 的相对档位,由 adapter 映射
}
```

解释:

- `ThinkingConfig` 是 **Core contract**
- 不同 provider:
  - OpenAI 可映射到 `reasoning.effort`
  - Anthropic 可映射到 `thinking.enabled + budget_tokens`
  - 本地模型可映射为 `Off` 或自定义档位

Core 不要求 provider 全支持,但要求 `ModelAdapter::caps()` 明确声明支持面。

## 17.5 μAgent 应定义的响应侧抽象

Thinking 的核心问题不是"看起来像什么",而是"能否被正确 replay"。

因此不应只用一个 `String` 表示,而应保留 provider 语义:

```rust
pub struct ThinkingArtifact {
    pub provider: String,
    pub kind: ThinkingKind,
    pub replay: ReplayPolicy,
    pub visibility: ThinkingVisibility,
    pub payload: ThinkingPayload,
}

pub enum ThinkingKind {
    SummaryText,
    FullText,
    RedactedOpaque,
    ProviderOpaque,
}

pub enum ReplayPolicy {
    Never,
    Optional,
    MustReplayUnmodified,
}

pub enum ThinkingVisibility {
    Hidden,
    SummaryAllowed,
    UserVisible,
}

pub enum ThinkingPayload {
    Text(String),
    OpaqueBytes(Vec<u8>),
    Json(serde_json::Value),
}
```

关键点:

- Core 不能强行把所有 provider 的 thinking 都降成纯文本
- 需要允许 `opaque` / `redacted` / `must replay unmodified`

## 17.6 Assistant Turn 里的 Thinking 位置

Thinking 不应混进普通 `assistant.text`。

更合理的是作为 assistant turn 的 sidecar:

```rust
pub struct AssistantTurn {
    pub visible_parts: Vec<AssistantPart>,
    pub thinking: Vec<ThinkingArtifact>,
    pub tool_calls: Vec<PendingCall>,
}
```

这样可以区分:

- 用户看见的输出
- provider 协议要求 replay 的 thinking
- 即将执行的 tool calls

## 17.7 Tool Loop 中的 Thinking Continuity

### 基本规则

当 assistant 发出 tool calls 后,下一轮模型调用不一定只是:

```text
history + tool_result
```

还可能必须是:

```text
history + assistant thinking artifacts + tool_result
```

否则 provider 可能:

- 丢失 reasoning continuity
- 降低 tool-use 质量
- 直接报错

### Core 不变量

1. **若 provider 要求 replay,Core 必须把 thinking artifacts durably 保存下来**
2. **若 replay_policy = MustReplayUnmodified,中间层不得改写**
3. **thinking replay 的窗口至少覆盖最后一个 assistant turn**

## 17.8 OpenAI / Anthropic / Local 的 replay 语义

### OpenAI

建议语义:

- `Responses API` 优先
- adapter 尽量保留与最近 function call 相邻的 reasoning items
- 若 provider 支持 `previous_response_id`,优先用 provider 原生 continuation
- 若走 `Chat Completions`,则认为 `thinking replay = unavailable`

### Anthropic

建议语义:

- 最后一个 assistant turn 的 consecutive thinking blocks 必须原样保存
- tool result 返回时,adapter 原样重放这些 thinking blocks
- 若出现 `redacted_thinking`,也必须保留并原样传回

### Local / OSS 模型

建议语义:

- 若没有 native thinking 协议,`ThinkingMode` 直接退化为 `Off`
- 不把 `<thinking>...</thinking>` 这类 prompt 技巧误当成 core thinking artifact

## 17.9 Thinking 与 Prompt 的关系

Prompt 设计和 thinking 设计必须分开:

- `Prompt prefix cache` 关注稳定前缀
- `Thinking replay` 关注上一轮 reasoning continuity

二者关联在于:

- thinking artifacts 可能进入后续请求
- 一旦进入,就会影响 cache 与 token 计费

因此 Core 应显式区分:

```rust
pub struct ModelRequest {
    pub prompt_plan: PromptPlan,
    pub thinking_replay: Vec<ThinkingArtifact>,
    pub tools: Vec<ToolDescriptor>,
}
```

不要把 thinking 简单并进 `prompt_augmentation`。

## 17.10 持久化与 Thaw

Thinking 属于恢复正确性问题,因此必须进入 durable state。

最少要求:

- 当前 run 最近一个 assistant turn 的 thinking artifacts 可恢复
- `ToolBatch` / `ToolIntent` 中断后,若 provider 需要 replay,thaw 后仍能取回
- schema version 升级时,thinking artifact 格式能迁移或安全丢弃

建议:

- `RunState` 不直接把 thinking 摊平到顶层
- 而是通过 `history` 中的 assistant turn sidecar 或单独的 `replay_state` 保存

## 17.11 Thinking 的安全与可见性

默认不应把 provider 原始 thinking 直接展示给终端用户。

推荐默认策略:

- `visibility = Hidden`
- provider 若只返回 summarized thinking,可按 `SummaryAllowed` 展示
- redacted / encrypted thinking 永远不直接展示

安全边界:

- 默认审计只记录 metadata
  - provider
  - kind
  - token counts
  - replay_policy
- 原始 thinking 文本默认不入普通日志
- 若 host 显式开启 debug / verbose audit,再单独落盘

## 17.12 Thinking 的事件与流式输出

thinking 是 core concept,但不意味着一定要对用户流式展示。

建议把旧的 `ThinkingDelta` 口径升级为更通用的 part-stream:

```rust
pub enum ModelPartDelta {
    VisibleText(String),
    ThinkingText(String),
    ThinkingSummary(String),
    ToolCall(PendingCall),
}
```

这样:

- Core 可以表达 provider 流式返回的 thinking
- Shell 可以选择:
  - 完全不显示
  - 只显示"思考中"
  - 显示 summary
  - debug 模式下显示全文

## 17.13 默认规则

Default Shell 的推荐默认值:

1. 默认 `ThinkingMode = Auto`
2. 不支持 native thinking 的 provider 自动退到 `Off`
3. 用户可见 thinking 默认 `Hidden`
4. 若 provider 要求 replay,最近 assistant turn 的 thinking durably 保存
5. 不用 fake chain-of-thought 文本伪装成 native thinking
6. Prompt cache 指标与 thinking replay 指标分开观测

## 17.14 对现有设计文档的影响

- [03-core-loop](03-core-loop.md)
  - `ModelReply` 需要容纳 `thinking artifacts`
  - `build_request` 需要区分 `prompt_plan` 与 `thinking_replay`
- [09-sdk](09-sdk.md)
  - FFI 需要明确 thinking 是否暴露给宿主
  - 流式事件模型应避免旧的 `ThinkingDelta` 命名歧义
- [10-observability-security](10-observability-security.md)
  - 增加 thinking metadata 审计规则
  - 增加 visible/billed token 区分
- [16-prompt-design](16-prompt-design.md)
  - Prompt 只描述 prefix / tail / runtime facts
  - replay thinking 单独建模

## 17.15 参考

- OpenAI Reasoning Best Practices  
  https://developers.openai.com/api/docs/guides/reasoning-best-practices
- Anthropic Extended Thinking  
  https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking
- Anthropic Prompt Caching(thinking blocks)  
  https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
