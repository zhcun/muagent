# 09 · SDK 导出与绑定

## 9.1 绑定方案:UniFFI

选 [UniFFI](https://mozilla.github.io/uniffi-rs/)(Mozilla)。
从一份 Rust 源 + `.udl` / proc-macro attribute 自动生成:
- **Swift**(iOS / macOS)
- **Kotlin**(Android / JVM)
- **Python**(服务器脚本)
- **可选 C ABI**(通用 FFI)

不选 JNI / ObjC bridge 手写:维护成本高,手抖就是 UB。

## 9.2 muagent-ffi 结构

```
crates/muagent-ffi/
├── Cargo.toml                    # crate-type = ["cdylib", "staticlib"]
├── uniffi.toml
├── muagent.udl                   # interface 声明(也可用 proc-macro)
├── build.rs                      # 触发 uniffi-bindgen
└── src/
    ├── lib.rs                    # 顶层,uniffi::include_scaffolding!
    ├── builder.rs                # AgentBuilder
    ├── agent.rs                  # Agent
    ├── run.rs                    # AgentRun(异步事件迭代)
    ├── profile.rs                # RuntimeProfile 的 FFI 形态
    ├── events.rs                 # Event / ToolResult / ...
    └── pal_bridge.rs             # Adapter 反向注入(宿主传入实现)
```

## 9.3 muagent.udl(精简)

```idl
namespace muagent {
  AgentBuilder builder();
};

dictionary RuntimeProfile {
    Platform platform;
    sequence<string> allowed_caps;
    sequence<string> denied_caps;
    Budget budgets;
    ModelSpec model;
    RoutingPolicy routing;
    AuditLevel audit;
};

enum Platform { "IOS", "Android", "Linux", "Mcu", "Macos" };

interface AgentBuilder {
    constructor();

    // v3.1:只暴露真实存在的边界
    AgentBuilder with_profile(RuntimeProfile p);
    AgentBuilder with_model(ModelAdapter m);                   // Core trait
    AgentBuilder with_tool_executor(ToolExecutor t);            // Core trait
    AgentBuilder with_session_store(SessionStore s);            // Core trait
    AgentBuilder with_tools_provider(ActiveToolSetProvider p);  // Core trait
    AgentBuilder with_clock(Clock c);                           // optional

    // Shell 扩展入口(host 用 Default Shell 时常用):
    AgentBuilder register_tool(Tool t);
    AgentBuilder register_skill(Skill s);
    AgentBuilder add_mcp_server(string name, string url);

    [Throws=AgentError] Agent build();
};

interface Agent {
    [Throws=AgentError] AgentRun run(string user_input);
    void cancel();
    // Shell 方法(host 用 Default Shell 时有)
    sequence<string> toc();                      // 当前 active tool 名录
    [Async] SessionManager session_manager();    // list/continue/fork/search sessions
};

interface AgentRun {
    // 宿主以 async 方式迭代事件
    [Async, Throws=AgentError] Event? next_event();
    [Async, Throws=AgentError] AgentOutput await_output();
};

// 宿主实现的 callback 接口(Adapter 反向注入)
callback interface FileSystem {
    sequence<Root> roots();
    [Throws=FsErr, Async] bytes read(string uri, ReadOpts opts);
    [Throws=FsErr, Async] void  write(string uri, bytes data, WriteOpts opts);
    [Throws=FsErr, Async] sequence<Entry> list(string uri);
    // ...
};
// Core callback interfaces(v3:只 3 个必备 + 1 个可选)
callback interface ModelAdapter { ... };
callback interface ToolExecutor { ... };
callback interface SessionStore { ... };
callback interface Clock { ... };                  // optional

// Shell-provided adapters(host 若用 default shell 需要实现这些)
callback interface FileSystem { ... };
callback interface InterApp { ... };               // optional
// ... ProcessExec / NetEgress / Secrets / ModelStore / ImageCodec / Camera / ...

// v3.1:无 approval / permission plugin callback;预留位置未来使用
```

## 9.4 Swift 使用形态

```swift
import MuAgent

// 1) 实现 Adapter(大部分有默认实现,只需覆盖必要的)
class MyFileSystem: FileSystem {
    func roots() -> [Root] { ... }
    func read(uri: String, opts: ReadOpts) async throws -> Data { ... }
    // ...
}

// 2) 构造 agent(v3.1 · 3 个 Core trait)
let toolExecutor = DefaultToolExecutor(adapters: buildIOSBundle())
let agent = try Builder()
    .withProfile(.iOSDefault)
    .withModel(AppleFMBackend())                   // ModelAdapter
    .withToolExecutor(toolExecutor)                 // ToolExecutor
    .withSessionStore(JsonlStore(path: storeDir))  // SessionStore
    .withToolsProvider(defaultShellProvider)        // Shell 的 ActiveToolSetProvider
    .build()

// 4) 运行 + 事件流(v3.1 事件集)
let run = try await agent.run(userInput: "明天下午有空的时间")
for try await event in run.events {
    switch event {
    // Core events
    case .sessionStart(let e):             logSessionStart(e)
    case .stepAdvanced(let to):            updateStepChip(to)
    case .userMessage:                     break
    case .assistantDelta(let text):        streamToUI(text)
    case .assistantMessage(let m):         finalizeStream(m)
    case .toolCallStart(let info):         showSpinner(info.tool)
    case .toolCallEnd(let info):           hideSpinner(info.tool, ok: info.ok,
                                                        retryable: info.retryable)
    case .toolIntentRecovered(let id):     toast("Interrupted tool re-entered; " +
                                                 "please verify before retrying.")
    case .paused(let reason):              showResumeButton(reason)
    case .errorRaised(let e):              toast(e.brief)
    case .sessionEnd(let ok):              finish(ok)

    // Shell events(使用 Default Shell 时)
    case .capabilityActivated(let id):     refreshToolMenu()
    case .mcpDescribed(let info):          refreshToolMenu()
    case .rootsChanged:                    refreshRootsList()
    case .sessionArchiveRotated(let info): updateArchiveBadge(info.part)
    case .sessionArchiveBriefReady(let i): archiveTOCAdd(i)
    case .historyCompacted(let info):      toast("Older turns summarized")

    default: break
    }
}
```

## 9.5 Kotlin 使用形态

```kotlin
class MyFileSystem : FileSystem {
    override fun roots(): List<Root> = ...
    override suspend fun read(uri: String, opts: ReadOpts): ByteArray = ...
    // ...
}

// v3.1:直接用 DefaultToolExecutor
val adapters = buildAndroidBundle(context, activity)
val toolExec: ToolExecutor = DefaultToolExecutor(adapters)

val agent = Builder()
    .withProfile(RuntimeProfile.androidDefault())
    .withModel(AICoreBackend(context))
    .withToolExecutor(toolExec)
    .withSessionStore(JsonlStore.default(context))
    .withToolsProvider(defaultShellProvider)
    .build()

viewModelScope.launch {
    val run = agent.run("帮我找下周的会议")
    run.events.collect { event ->
        when (event) {
            // Core events
            is Event.StepAdvanced             -> /* 更新 step chip */
            is Event.AssistantDelta           -> /* 流式渲染 */
            is Event.AssistantMessage         -> /* 最终消息 */
            is Event.ToolCallStart            -> /* 显示 spinner */
            is Event.ToolCallEnd              -> /* 结束 spinner */
            is Event.ToolIntentRecovered      -> toast("恢复中断的工具调用,请先核验状态")
            is Event.Paused                   -> showResumeButton(event)
            is Event.ErrorRaised              -> toast(event.brief)
            is Event.SessionEnd               -> break

            // Shell events
            is Event.SessionArchiveRotated    -> updateArchiveBadge(event.part)
            is Event.SessionArchiveBriefReady -> archiveTOCAdd(event)
            is Event.HistoryCompacted         -> toast("Older turns summarized")
            is Event.CapabilityActivated      -> refreshToolMenu()
            is Event.McpDescribed             -> refreshToolMenu()
            else -> Unit
        }
    }
}
```

## 9.6 CLI / Rust 直接使用(不经 FFI)

```rust
use muagent_core::prelude::*;
use muagent_adapters_linux::linux_bundle;
use muagent_model_ollama::OllamaBackend;
use muagent_approval_policies::AutoApproveReadOnly;
use muagent_storage_jsonl::JsonlSessionStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Adapter bundle 是 ToolExecutor 的依赖;Core 看不到它
    let adapters = linux_bundle(LinuxConfig {
        fs_roots: vec![std::env::home_dir()?.join(".muagent")],
        ..Default::default()
    })?;

    let model = OllamaBackend::at("http://127.0.0.1:11434");
    let store = JsonlSessionStore::open("~/.muagent/runs").await?;

    PolicyRegistry::global().register("auto_read_only", AutoApproveReadOnly);

    let agent = AgentBuilder::new()
        .profile(RuntimeProfile::raspberry_pi())
        .with_model(Arc::new(model))
        .with_adapters(Arc::new(adapters))
        .with_session_store(Arc::new(store))
        .with_approval_policy("auto_read_only")
        .register_skill(muagent_skill_git::GitSkill::default())
        .register_skill(muagent_skill_sysmon::SysmonSkill::default())
        .add_mcp_server("github", "https://mcp.github.example.com/sse")
        .build()?;

    let mut run = agent.run("git 仓库状态如何").await?;
    while let Some(e) = run.next_event().await? {
        println!("{:?}", e);
    }
    Ok(())
}
```

## 9.7 Event 类型(跨语言一致)

```rust
/// v3.1:Core + shell 事件全集(approval/permission 事件已删除)
/// 所有事件带 seq(EventSeq);host 订阅方按 (run_id, seq) 去重(at-least-once)。
#[derive(Serialize, Deserialize, uniffi::Enum)]
pub enum Event {
    // Core(muagent-core 发出)
    SessionStart         { run_id: String, seq: u64 },
    SessionEnd           { ok: bool, seq: u64 },
    UserMessage          { seq: u64 },
    AssistantDelta       { text: String, seq: u64 },
    AssistantMessage     { text: String, seq: u64 },
    ToolCallStart        { call_id: String, tool: String, seq: u64 },
    ToolCallEnd          { call_id: String, ok: bool, retryable: bool, brief: String, seq: u64 },
    ToolIntentRecovered  { call_id: String, seq: u64 },
    StepAdvanced         { to: String, seq: u64 },
    Paused               { reason: String, seq: u64 },
    ErrorRaised          { class: String, brief: String, seq: u64 },

    // Shell(muagent-shell 发出)
    CapabilityActivated  { id: String, seq: u64 },
    McpDescribed         { server: String, tools_added: u32, seq: u64 },
    RootsChanged         { seq: u64 },
}
```

如果将来引入 approval/permission/stall 等 addon,它们可以在**自己的事件类型**里发,或 shell 侧扩展该枚举。v3.1 **不为未来 addon 保留事件槽**。

订阅方契约(round-2 §10.1):
- 事件带 `(run_id, seq)`,`seq` 在每个 run 内单调递增。
- at-least-once:崩溃 thaw 后同一 step 的事件可能重发。
- SDK 层默认带 `(run_id, seq)` dedupe;host 可禁用以获取原始流。

## 9.8 构建与分发

### iOS
```bash
# 1) 编多架构 staticlib
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
cargo build -p muagent-ffi --release --target aarch64-apple-ios
cargo build -p muagent-ffi --release --target aarch64-apple-ios-sim
cargo build -p muagent-ffi --release --target x86_64-apple-ios

# 2) 合 XCFramework
xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libmuagent_ffi.a \
        -headers crates/muagent-ffi/include \
    -library target/aarch64-apple-ios-sim/release/libmuagent_ffi.a \
        -headers crates/muagent-ffi/include \
    -output MuAgent.xcframework

# 3) 生成 Swift bindings
uniffi-bindgen generate --language swift crates/muagent-ffi/muagent.udl \
    --out-dir platforms/ios/Sources/MuAgent
```

分发:SPM(Swift Package Manager)package,XCFramework 作为 binary target。

### Android
```bash
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
    --platform 26 build --release -p muagent-ffi
uniffi-bindgen generate --language kotlin crates/muagent-ffi/muagent.udl \
    --out-dir platforms/android/muagent-android/src/main/kotlin
```

分发:AAR,发布到 Maven Central。

### Linux
分发:`muagent` 单二进制(`cargo install muagent-cli`)。
也提供 `libmuagent.so` + C header 给 Python / 其它语言用。

## 9.9 Adapter 反向注入的性能注意点

UniFFI 的 callback interface 跨语言调用有 overhead(每次 ~1-10μs)。
影响:
- `FileSystem::read` 被 tool 调用——这是"用户级"操作,完全可接受。
- `Clock::now_ms()` 可能被调用频繁——内部可缓存(Rust native `Instant::now()` 比 FFI 闭包快几个量级);`Clock` trait 主要用在有语义的地方(例如 `budget_hint`)。
- `Storage::append_event` 被 EventBus 每条事件调用——量级不高(每秒几十条),可接受。

原则:**Adapter trait 方法要"粗粒度"、带语义**,不暴露细粒度原语。

## 9.10 版本策略

- Core API `v0.x` 到 `v1.0` 前允许 breaking。
- FFI 层稳定承诺晚于 Core 稳定(Core 稳定后再考虑 UniFFI ABI 稳定)。
- Skill Pack ABI 与 Core semver 绑定。
