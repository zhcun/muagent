# 15 · Long-term / 跨 session 记忆(addon · 默认关闭)

> 本文档描述 `muagent-memory` addon crate。**默认不启用**,host 主动依赖才生效。
> 提供三层记忆:**Facts**(命名事实)/ **Episodes**(向量检索的消息片段)/ **Summaries**(会话摘要)。
> 不装 addon 时,Core + Default Shell 行为与 v3.1 完全一致(只有 session-scoped KV `session.note`);装上后,跨 session 的检索 / 自动提取 / 自动回填才出现。

## 15.1 为什么做 addon 而不是默认

| 原因 | 说明 |
|---|---|
| 增加重量级依赖 | 嵌入模型(本地 23MB–200MB)或云 embedding API;向量索引库(in-process HNSW / 第三方向量后端) |
| 隐私含义 | 持久化跨 session 记忆是 privacy sensitive,用户应**显式**同意 |
| 非通用需求 | 一次性 CLI / 无状态 API / stateless agent 都不需要 |
| 可能的 memory poisoning | 恶意输入 → 被记住 → 影响未来 session;用户须能看、能删 |

**默认:完全不装**。Host 按需 `cargo add muagent-memory`。

## 15.2 三层记忆模型

```
┌─────────────────────────────────────────────────────────────┐
│ Facts(命名事实)                                            │
│   小、结构化、按 key 检索                                     │
│   "user.name" → "mike";"user.pref.language" → "中文"          │
│   显式存取:memory.remember / memory.recall                   │
│   自动抽取:可选 middleware,每 session 结束后扫                │
├─────────────────────────────────────────────────────────────┤
│ Episodes(消息片段)                                          │
│   完整或窗口化的历史消息,带 embedding,按相似度检索            │
│   适合"我之前跟你说过 X 来着?"这类问题                        │
│   可选自动存:每 N 个 turn / 每个 session 完结                 │
├─────────────────────────────────────────────────────────────┤
│ Summaries(摘要)                                            │
│   每个 session 生成一个摘要,带 embedding + 主题标签           │
│   "2025-03-15 我们讨论了 X 项目的 API 设计"                   │
│   自动创建:session 达到 Done 或第 N 轮时                     │
└─────────────────────────────────────────────────────────────┘

   ↑ 检索  
   └── 两种使用路径:
       1. 显式 tool:memory.search / memory.recall(agent 主动查)
       2. 自动回填:AutoRecallProvider 在 ModelTurn 前 prompt 注入
```

## 15.3 Core trait

```rust
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dim(&self) -> u32;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn put_fact(&self, fact: Fact) -> Result<MemoryId, MemError>;
    async fn put_episode(&self, ep: Episode, embedding: &[f32]) -> Result<MemoryId, MemError>;
    async fn put_summary(&self, sm: Summary, embedding: &[f32]) -> Result<MemoryId, MemError>;

    async fn get_fact(&self, key: &str) -> Result<Option<Fact>, MemError>;

    /// 按 embedding 相似度检索(episodes + summaries)
    async fn search_vec(&self, query_embedding: &[f32], filter: MemoryFilter, k: usize)
        -> Result<Vec<MemoryHit>, MemError>;

    /// 按关键字 / 标签 / 时间检索(facts 为主)
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<MemoryItem>, MemError>;

    async fn delete(&self, id: MemoryId) -> Result<(), MemError>;

    /// 对应"忘记一次对话"的 UI 操作(合规 / GDPR)
    async fn forget_session(&self, session_id: SessionId) -> Result<u32, MemError>;

    /// 使用量
    async fn stats(&self) -> Result<MemoryStats, MemError>;
}
```

## 15.4 数据类型

```rust
pub type MemoryId = String;

#[derive(Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: MemoryId,
    pub key: String,              // "user.name" / "user.pref.language" / "project.x.deadline"
    pub value: String,
    pub tags: Vec<String>,        // 用户分类
    pub source_session: Option<SessionId>,  // 来源(用于按 session 遗忘 / 审计)
    pub confidence: f32,          // 自动抽取 0.0–1.0;显式存为 1.0
    pub created_ms: i64,
    pub updated_ms: i64,
    pub accessed_count: u32,      // 用于 LRU 清理
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: MemoryId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_range: Range<u32>,
    pub text: String,             // 消息片段或拼接文本
    pub created_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Summary {
    pub id: MemoryId,
    pub session_id: SessionId,
    pub text: String,
    pub topics: Vec<String>,
    pub created_ms: i64,
}

pub struct MemoryHit {
    pub item: MemoryItem,         // Fact / Episode / Summary
    pub score: f32,
}

pub enum MemoryItem {
    Fact(Fact),
    Episode(Episode),
    Summary(Summary),
}

pub struct MemoryFilter {
    pub kinds: Vec<MemoryKind>,   // 只查某几类
    pub tags: Vec<String>,
    pub session_ids: Vec<SessionId>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

pub enum MemoryKind { Fact, Episode, Summary }
```

## 15.5 Addon 暴露的 meta-tool(通过 MemorySkill)

| tool | side_effects | 作用 |
|---|---|---|
| `memory.remember(key, value, tags?)` | Mutating | 显式存一条 Fact |
| `memory.recall(key)` | ReadOnly | 按 key 查 Fact |
| `memory.search(query, kinds?, k?)` | ReadOnly | 按语义 search 返 top-K |
| `memory.list(tag?, kind?, limit?)` | ReadOnly | 列出,UI 展示用 |
| `memory.forget(id \| key)` | Destructive | 删除;`user_confirm=always` |

Shell 把 `MemorySkill` 注册为 skill(默认 `Activation::AlwaysOn` when addon present)。

## 15.6 自动化 middleware(都是可选 per config)

### 15.6.1 AutoRecallProvider(prompt 注入)

包装 Shell 的 `ActiveToolSetProvider`:

```rust
pub struct AutoRecallProvider<P> {
    inner: P,
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn EmbeddingProvider>,
    top_k: u32,                    // 默认 3
    min_score: f32,                // 默认 0.75
    token_cap: u32,                // 注入总 token 上限,默认 400
}

impl<P: ActiveToolSetProvider> ActiveToolSetProvider for AutoRecallProvider<P> {
    fn provide(&self, state: &RunState) -> ActiveToolSet {
        let mut ats = self.inner.provide(state);
        // 取最近几条 user/assistant msg 作为查询
        let query = make_query_from_recent(&state.history, tail=4);
        let emb = self.embedder.embed(&[query]).await?.remove(0);
        let hits = self.store.search_vec(&emb, filter, self.top_k).await?;
        // 过 min_score,拼成 prompt fragment,接在 prompt_augmentation 后
        let relevant = hits.into_iter()
            .filter(|h| h.score >= self.min_score)
            .collect();
        let injected = format_memory_augmentation(relevant, self.token_cap);
        ats.prompt_augmentation.push_str("\n\n");
        ats.prompt_augmentation.push_str(&injected);
        ats
    }
}
```

### 15.6.2 AutoExtractMiddleware(session 结束后抽取)

```rust
pub struct AutoExtractMiddleware {
    extractor_model: Arc<dyn ModelAdapter>,   // 可以是轻量小模型
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn EmbeddingProvider>,
    config: AutoExtractConfig,
}

impl AutoExtractMiddleware {
    /// Shell 订阅 Event::SessionEnd;session 完成后后台跑
    pub async fn on_session_end(&self, session_id: SessionId, history: &[Message]) {
        // 1) 生成 Summary(摘要 + 主题提取)
        let sm = self.extractor_model.turn(summary_request(history)).await?;
        let emb = self.embedder.embed(&[sm.text.clone()]).await?.remove(0);
        self.store.put_summary(Summary { ... }, &emb).await?;

        // 2) 提取 Facts(用 structured output / JSON schema)
        let facts = self.extractor_model.turn(fact_extract_request(history)).await?;
        for f in facts { self.store.put_fact(f).await?; }

        // 3) (可选)存 Episodes:按窗口切成段,每段 embed + 存
        if self.config.store_episodes {
            for chunk in window(history, size=4) {
                let emb = self.embedder.embed(&[chunk.text.clone()]).await?.remove(0);
                self.store.put_episode(Episode { ... }, &emb).await?;
            }
        }
    }
}
```

Fire-and-forget,不阻塞用户下一次操作。

## 15.7 Storage 实现

### 15.7.1 Schema(逻辑层,与具体后端无关)

addon 的逻辑表(各后端按自己习惯落库;以下是 reference shape):

| 表 | 字段 |
|---|---|
| `mem_facts` | id, key, value, tags(JSON array), source_session, confidence, created_ms, updated_ms, accessed_count |
| `mem_episodes` | id, session_id, run_id, turn_start, turn_end, text, created_ms |
| `mem_summaries` | id, session_id, text, topics(JSON array), created_ms |
| 向量索引 | id ↔ embedding(384-dim for `all-MiniLM-L6-v2`) |

向量索引和元数据表按 id 对齐。

### 15.7.2 后端选项(按需)

约束:**不强加重型原生向量库或数据库依赖** —— 主仓库已经从
`muagent-storage-jsonl` 出发不带 SQL 引擎,addon 应保持同一态度。可选实现:

- `InMemoryMemoryStore`(纯测试 / 单进程)
- `JsonlVecMemoryStore`(JSONL 元数据 + 内存中 HNSW 索引,启动时重建;<10K
  facts 量级最简单)
- `LanceDbMemoryStore`(桌面 / 服务端,native deps OK 的环境)
- `PgVectorMemoryStore`(企业;走 NetEgress,无本地依赖)
- 第三方 plug-in 实现 `MemoryStore` trait

默认 ship 哪一个:M6 启动时再定;倾向 `JsonlVecMemoryStore` 走"轻量先过"。

## 15.8 Embedding Provider 实现

| impl | 平台 | 说明 |
|---|---|---|
| `OnnxEmbedding` | Linux / macOS / iOS / Android | 本地;all-MiniLM-L6-v2 约 23MB |
| `OllamaEmbedding` | Linux / Pi | 本地 Ollama 跑 `nomic-embed-text` 等 |
| `OpenAIEmbedding` | 任意 | 云端 API(比如 `text-embedding-3-small`);会走 NetEgress |
| `MlxEmbedding` | iOS / macOS | MLX 跑 embedding model |

## 15.9 Host 组装示例

```rust
use muagent_memory::{MemoryAddon, JsonlVecMemoryStore, OnnxEmbedding, AutoRecallProvider};

// 1) 构造 store + embedder(默认 JSONL + in-process HNSW;无 native deps)
let mem_store = JsonlVecMemoryStore::open("~/.muagent/memory").await?;
let embedder  = OnnxEmbedding::load("~/.muagent/models/all-MiniLM-L6-v2.onnx")?;

// 2) 装入 shell
let memory = MemoryAddon::new(mem_store, embedder, MemoryConfig {
    auto_recall: true,           // 默认 true(装了就用)
    auto_extract: true,
    store_episodes: false,       // 默认不存 raw 历史(省空间 + 隐私)
    top_k: 3,
    min_score: 0.75,
    ..Default::default()
});

// 3) 注册 MemorySkill + 装 AutoRecallProvider wrapper
let tools_provider = memory.wrap_provider(default_shell_provider);
let agent = Builder::new()
    .withModel(model)
    .withToolExecutor(memory.wrap_tool_executor(default_tool_executor))
    .withSessionStore(jsonl_store)
    .withToolsProvider(tools_provider)
    .build();

// session 结束时调 middleware
agent.on_event(Event::SessionEnd { .. }, |evt| memory.on_session_end(evt));
```

**关键:不调 `muagent-memory` 这个 crate 就完全没有长期记忆能力**,Shell 行为回落到 v3.1 的 session-scoped KV。

## 15.10 事件

```rust
pub enum Event {
    ...
    MemoryRecalled { hits: u32, query_brief: String, seq: u64 },
    MemoryStored { kind: String, id: String, brief: String, seq: u64 },
    MemoryDeleted { id: String, seq: u64 },
    MemoryAutoExtracted { facts: u32, summaries: u32, episodes: u32, seq: u64 },
}
```

UI 可以显示"已记忆 5 条事实"等提示,尤其在自动提取时用户想知道"agent 记了我什么"。

## 15.11 安全与隐私

| 风险 | 缓解 |
|---|---|
| 存 secret 进 memory | 每次 `put_*` 前过 `sanitize()`,匹配到 secret pattern → refuse + warn event |
| Memory poisoning(恶意 session 灌入错误记忆) | Fact 带 `source_session`;UI `memory.list` 按来源分组;用户点 "forget this session" 一次清掉 |
| 敏感话题的历史被 episode 存下 | 默认 `store_episodes = false`;用户显式开启才存 raw 历史 |
| 用户"被遗忘权" | `memory.forget(id)` / `store.forget_session(sid)` 两个接口都支持硬删除;GDPR 流程有 hook |
| 自动抽取模型跑错提取出骚扰内容 | `confidence < 0.6` 的 Fact 默认不进入 `AutoRecall` 注入,只在 `memory.list` 里展示供用户核验 |
| 向量库膨胀 | `MemoryConfig.max_items` + LRU / time-based eviction;停用 `store_episodes` 大幅降占用 |

## 15.12 默认关闭的含义

- `muagent` 主 crate / `muagent-shell` 都**不依赖** `muagent-memory`
- 用户 `cargo add muagent` 得到的 agent 只有 `session.note`(session-scoped KV),没有跨 session 记忆
- 想要长期记忆:**显式** `cargo add muagent-memory` + 调用 `MemoryAddon::wrap_*`
- 关掉 auto_recall / auto_extract:config 里设 false,addon 安装了但不自动运行(只在 agent 显式调 `memory.*` tool 时生效)

## 15.13 和 SessionManager / Compaction 的区别

| 层 | 存什么 | 范围 | 默认 |
|---|---|---|---|
| `RunState.history` | 当前 Run 的完整 message 列表 | 单 Run | Core 强制 |
| `SessionManager` 的 `continue_session` | 同 SessionId 下多个 Run 的 history 合并 | 单 Session | Shell 默认 |
| `CompactionStrategy` | 替换旧 turn 为 Summary observation | 单 Run 内 | Shell 默认 |
| `session.note` KV | 当前 Session 下的键值对 | 单 Session | Shell 默认 |
| `muagent-memory` Facts | 用户级命名事实 | **跨 Session** | **Addon 默认关** |
| `muagent-memory` Episodes | embedding 索引的历史消息片段 | **跨 Session** | **Addon 默认关** |
| `muagent-memory` Summaries | 每 Session 的摘要 + 主题 | **跨 Session** | **Addon 默认关** |

关键线:**session 内 = Shell 默认管;跨 session = addon 开才有**。

## 15.14 何时选择开启 / 不开启

**建议开启**:
- 个人助理 app(记住用户偏好)
- 长期伴随型 agent
- 企业知识管理 agent(跨会议 / 跨项目)
- 个人笔记 agent

**不建议开启**:
- 一次性 CLI 工具
- 无状态 API 后端
- 隐私敏感环境(医疗 / 法律 / 金融客户场景)
- 移动端且空间紧张(23MB+ 嵌入模型 + 索引)

## 15.15 测试要点

1. **不装 addon 时**:Core + Shell 行为零变化,M0/M1 现有测试全绿
2. **explicit remember/recall**:`memory.remember("user.name", "mike")` → 下一 session `memory.recall("user.name")` 返 "mike"
3. **semantic search**:连续几个 session 谈不同主题,`memory.search("咖啡")` 只返回谈咖啡的 session 的 summary
4. **auto recall 注入**:装 `AutoRecallProvider`,连续两 session;第二次开头问"我喜欢什么饮料" → 不调任何 tool 就能答,因为 prompt 里已注入 Fact
5. **forget session**:`memory.forget_session(sid)` → 该 session 的 Fact / Episode / Summary 全从 mem_* 表删除
6. **secret filter**:`memory.remember("my_pw", "xxx")` 含密码模式 → 拒绝并发 warning 事件
7. **低 confidence 不注入**:自动抽取出一条 confidence=0.5 的 Fact → 不进入 auto_recall 的 prompt,但 `memory.list` 能看到

## 15.16 未来扩展点(不在本设计)

- **Memory sharing / 多 agent 共享**:多个 agent 读同一 MemoryStore(家庭助理、团队助理)
- **Time-decay reasoning**:年久失效 / 用户修改事实时 old 版本归档而非删除
- **Memory graph**:Facts 之间建立关系(knowledge graph)
- **Hybrid retrieval**:BM25 + 向量混合检索
- **User-approved memory**:每次自动抽取的 Fact 让用户确认后才进 auto_recall(类 ChatGPT Memory 的 confirm UX)

这些都是后续 release 的题目,v3.1 addon 的范围就到 15.1–15.15 为止。
