# 06 · Capabilities(v3 · Default Shell 层的能力管理)

> v1 把 Capability 当 Core 心智模型。
> v2 把它抽成"Capability 层",但它仍然靠 `ActiveToolSetResolver` 和 Core 耦合。
> v3 按 [reviews/15-v5-review §3.5 / §3.6](../reviews/15-v5-review.md) 收敛:
> **Capability 完全是 Default Shell 的事,Core 不知道**。
>
> Core 只需要 host / shell 在每个 step 前给它一份 "本 step 的 tool descriptors + prompt augmentation"(见 [03 §3.5](03-core-loop.md))。
> 这份东西**怎么来的、从哪挑的、什么时候激活**,Core 不 care。

## 6.1 心智模型(三类来源,一个接口)

```
       shell 的内部世界            ← 只在 default shell 里看到
┌────────────────────────────────┐
│  内置 Tool(builtin)             │
│  Skill(capability bundle)       │
│  MCP server(remote / in-proc)   │
│  InterApp 自动桥接(iOS / Android)│
│  自定义 tool(host 注册)         │
└────────────┬───────────────────┘
             │
             ▼  shell 聚合
      ActiveToolSetProvider(host 传入 Runner 的闭包)
             │
             ▼
  ┌──────────────────────────────┐
  │  ActiveToolSet               │   ← Core 唯一看到的东西
  │    tools: Vec<ToolDescriptor>│
  │    prompt_augmentation: String│
  └──────────────────────────────┘
```

Shell 提供**默认的**聚合逻辑(TOC-first / lazy describe / 激活追踪);host 自由替换。

## 6.2 `ActiveToolSet`(Core 看到的契约)

```rust
// muagent-core 里定义(与 Core trait 配套)
pub struct ActiveToolSet {
    pub tools: Vec<ToolDescriptor>,
    pub prompt_augmentation: String,
    pub version: u64,
}
```

Core 的 Runner 构造时接受一个闭包:
```rust
type ToolsProvider = Arc<dyn Fn(&RunState) -> ActiveToolSet + Send + Sync>;
```

每次 `on_model_turn` 前调一次。Resolver / lazy describe / skill activation 全部在闭包内部完成,**Core 不知道细节**。

## 6.3 Shell 默认聚合器:`DefaultToolSetProvider`

```rust
// muagent-shell/src/provider.rs
pub struct DefaultToolSetProvider {
    registry: Arc<CapabilityRegistry>,
    skill_mgr: Arc<SkillManager>,
    mcp: Arc<McpClient>,
}

impl DefaultToolSetProvider {
    pub fn build(&self, state: &RunState) -> ActiveToolSet {
        let mut tools = Vec::new();
        let mut toc_lines = Vec::new();

        // 1) AlwaysOn 内置 tool
        for t in self.registry.always_on_tools() {
            tools.push(t.descriptor().clone());
            toc_lines.push(format_toc_line(t.descriptor()));
        }

        // 2) Skill:激活的暴露 tools,未激活的只挂 TOC
        for s in self.skill_mgr.all_skills() {
            if self.skill_mgr.is_active(s.id(), state) {
                for t in s.tools() { tools.push(t.descriptor().clone()); }
            } else {
                toc_lines.push(format!("skill:{} - {}", s.id(), s.toc_entry()));
            }
        }

        // 3) MCP:TOC 一行;已 describe 的 server 暴露 tools
        for server in self.mcp.all_servers() {
            if let Some(ts) = self.mcp.described_tools(&server) {
                for t in ts { tools.push(t.descriptor().clone()); }
            } else {
                toc_lines.push(format!("mcp:{} - (call cap.describe)", server));
            }
        }

        // 4) meta-tool(cap.list / cap.describe / cap.deactivate / session.note)
        for t in self.registry.meta_tools() { tools.push(t.descriptor().clone()); }

        ActiveToolSet {
            tools,
            prompt_augmentation: format_prompt_augmentation(&toc_lines, &self.skill_mgr),
            version: self.compute_version(state),
        }
    }
}

impl DefaultToolSetProvider {
    pub fn into_closure(self) -> ToolsProvider {
        let this = Arc::new(self);
        Arc::new(move |state| this.build(state))
    }
}
```

Host 组装:
```rust
let provider = DefaultToolSetProvider::new(registry, skill_mgr, mcp).into_closure();
let runner = Runner::builder()
    .model(model).tools(tool_exec).store(store)
    .tools_provider(provider)
    .build();
```

## 6.4 Tool / Skill / MCP 的统一抽象(shell 内部)

仍然把三类东西抽成 capability,只在**挂载时机**上不同:

| 来源 | TOC 占用 | 激活机制 |
|---|---|---|
| **内置 Tool** | 一行 / tool | AlwaysOn |
| **Skill** | 一行 / skill | LazyOnCall / RouterMatch / AlwaysOn |
| **MCP server** | 一行 / server | 双重 lazy(connect + describe) |
| **InterApp action** | 按 category 聚合成 Skill | LazyOnCall |
| **自定义 Tool** | 一行 / tool | AlwaysOn(host 注册时决定) |

具体 trait 定义(Skill / McpClient / Transport 等)在 shell crate 内;与 v2.2 相比**签名不变**,只是位置从 "Capability Layer" 改为 "shell 的 capability 子模块"。

## 6.5 Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;
    fn guard(&self, args: &Value, adapters: &AdapterBundle) -> GuardOutcome;
    async fn run(&self, args: Value, ctx: ToolCtx<'_>) -> Result<ToolOk, ToolErr>;

    /// 动态幂等性(round-2 A5);默认返回 descriptor().idempotency
    fn idempotency_for_args(&self, _args: &Value) -> Idempotency {
        self.descriptor().idempotency
    }
}

pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub schema: RootSchema,
    pub timeout: Duration,
    pub max_out_tokens: u32,
    pub concurrency: Concurrency,           // Parallel(ReadOnly only) | Exclusive
    pub side_effects: SideEffects,          // ReadOnly | CapabilityMutation | Mutating | Destructive
    pub idempotency: Idempotency,           // 默认;可按 args 覆盖
    pub requires_adapters: Vec<AdapterReq>,
}

pub enum SideEffects { ReadOnly, CapabilityMutation, Mutating, Destructive }
pub enum Idempotency { Idempotent, AtMostOnce, AtLeastOnce }
pub enum Concurrency { Parallel, Exclusive }
```

**编译期校验**(`#[tool]` proc-macro):
- `side_effects = Destructive` ⇒ `idempotency ∈ { AtMostOnce, Idempotent }`
- `concurrency = Parallel` ⇒ `side_effects = ReadOnly`
- `side_effects = CapabilityMutation` ⇒ `requires_adapters.is_empty()`

## 6.6 meta-tool(shell 层,不是 Core)

shell 默认注册这些 meta-tool:

| 名字 | side_effects | 作用 |
|---|---|---|
| `cap.list` | ReadOnly | 返回当前 TOC |
| `cap.describe(id)` | CapabilityMutation | 展开并激活 skill / MCP server |
| `cap.deactivate(id)` | CapabilityMutation | 卸下 |
| `session.note(text)` | Mutating | 写笔记到 session kv |
| `session.fork(name)` | Mutating | 分子会话 |

(v2.2 里的 `system.request_permission` meta-tool 已**删除**——v3.1 里完全没有"权限"概念,OS 授权由具体 tool 在内部处理,tool 失败时通过 `ToolResult { ok:false, hint: "permission denied; enable in Settings" }` 回灌给 LLM。)

## 6.7 Skill trait

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn id(&self) -> &str;
    fn version(&self) -> Version;
    fn toc_entry(&self) -> &str;
    fn prompt_hint(&self) -> &str;

    fn tools(&self) -> &[Arc<dyn Tool>];
    fn activation(&self) -> Activation;

    async fn on_activate(&self, ctx: &SkillCtx<'_>) -> Result<(), SkillErr> { Ok(()) }
    async fn on_deactivate(&self, ctx: &SkillCtx<'_>) -> Result<(), SkillErr> { Ok(()) }
    fn state_namespace(&self) -> Option<&str> { None }
}

pub enum Activation {
    AlwaysOn,
    LazyOnCall,
    RouterMatch { patterns: Vec<String> },
    Triggered { by: Vec<TriggerSpec> },
}
```

## 6.8 McpClient(双重 lazy · shell 层)

与 v2.2 相同,不重述。详见 [v2.2 归档],签名不变。

## 6.9 InterApp 自动桥接

Shell 把 `adapters.interapp.list_actions()` 按 category 聚合成虚拟 Skill(同 v2.2)。LLM 只看到 `interapp:calendar` 一行 TOC,`cap.describe` 才展开。

## 6.10 host 可以完全不用 shell 的 Capability 体系

如果 host 非常简单(例如只有 3 个固定工具),可以:
- 自己写一个 `tools_provider` 闭包,每次返回相同的静态 tool list
- 完全不引入 `muagent-shell` 的 CapabilityRegistry / SkillManager / McpClient
- Core 正常工作

这是 v3 相对 v2.2 的关键灵活性来源:**capability 是可选层**。

## 6.11 与 v2.2 的差异

| 维度 | v2.2 | v3 |
|---|---|---|
| 所属层 | "Capability Layer"(主架构层) | **Default Shell 的子模块** |
| Core 是否持有 | Core 持有 `Arc<dyn ActiveToolSetResolver>` | Core 只接受 `Fn(&RunState) -> ActiveToolSet` 闭包 |
| Capability Registry 是否 Core 概念 | 是 | **否**,shell 概念 |
| Skill / MCP / InterApp | 三者统一抽象,位于 Capability 层 | 三者仍统一抽象,但整个集合搬到 shell |
| meta-tool 位置 | Capability 层 | shell 层;`system.request_permission` 彻底删除(v3.1 无权限概念) |
| 非 shell host 能否用 | 理论能,但绕开 resolver 麻烦 | 直接写个闭包即可 |
