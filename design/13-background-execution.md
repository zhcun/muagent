# 13 · iOS / Android 后台执行(v3.1 · 基于 RunState)

> 回答疑问:**可以在后台运行,但两家系统都有明确限制,不能"像 Linux daemon 一样 24/7 自由跑"**。
>
> μAgent 的策略:**runner 做成 step-level FSM,任意 step 边界都能冻结/解冻**,配合系统提供的合法后台窗口 + 前台服务 / 通知机制。
>
> freeze/thaw 的对象是 **`RunState`**(含当前 `Step` / `history` / `usage` / `event_seq` / `schema_version`),而不只是 session transcript。这样能恢复"停在哪个 ToolBatch cursor"而不只是"聊到哪了"。

## 13.1 能与不能的快速总览

| 场景 | iOS | Android |
|---|---|---|
| 用户打开 app,app 在前台 | ✅ 无限制 | ✅ 无限制 |
| 用户按 Home / 切走 app | ⚠️ 约 30 秒 grace | ⚠️ 默认会被系统杀(取决于 OEM) |
| 充电时做"批量 / 维护"工作 | ✅ `BGProcessingTask` 数分钟 | ✅ WorkManager(可设 `requiresCharging`) |
| 周期性刷新(如每 30 分钟拉一次) | ✅ `BGAppRefreshTask` ~30 秒/次 | ✅ WorkManager 最小 15 分钟 |
| 语音 / 播放 / 导航 / VoIP 类 app 持续运行 | ✅ 专用 background mode | ✅ Foreground Service(需通知) |
| 推送 / 静默推送唤起做几秒活 | ✅ silent push + content-available | ✅ FCM data message + high priority |
| 常驻几小时的"agent daemon" | ❌ 不可能 | ⚠️ Foreground Service(必须显示通知) |
| Doze / 省电模式下定时任务 | ⚠️ 受限 | ⚠️ 受限,需 `setAndAllowWhileIdle` 等 |

**核心结论**:μAgent **不是 always-on daemon**。agent 一次 `run()` 在合法窗口内完成、随时可冻结,不做 iOS 禁止的"无理由长驻"。

## 13.2 iOS 后台执行模型详解

### 13.2.1 App 生命周期状态

```
                ┌── active ──┐
inactive ──────┤            ├──── background ─── suspended ─── terminated
                └── (前台) ──┘     (~30s grace)
```

### 13.2.2 可用的后台模式

| 机制 | 能做多久 | 触发 | 适合 μAgent 场景 |
|---|---|---|---|
| **Background Task**(`beginBackgroundTask`) | 30 秒 | app 进入 background 时主动申请 | 让当前 step 自然完成并 persist,或 cancel 后下次 step 进 Paused |
| **BGAppRefreshTask** | ~30 秒 / 次 | 系统按启发式调度 | 定期拉 MCP server 的 event / 摘要推送 |
| **BGProcessingTask** | 数分钟(充电+WiFi) | 系统调度 | 长推理、本地索引重建、模型微调 |
| **Silent Push**(`content-available:1`) | ~30 秒 | 远程推送 | 云端事件唤起 agent 做简短回应 |
| **Background Fetch**(deprecated, 被 BGAppRefreshTask 取代) | — | — | — |
| **Audio / VoIP / Location / CarPlay background modes** | 持续 | 需在 Info.plist 声明且真实使用 | 语音助手 / 位置触发的 agent |
| **PushKit(VoIP push)** | 较长 | VoIP push | 严格意义 VoIP app,滥用会被拒审 |

### 13.2.3 `Clock::budget_hint` 的 iOS 实现

```swift
class IOSClock: Clock {
    private let app = UIApplication.shared

    func nowMs() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

    func budgetHint() -> BudgetHint {
        // 前台:Unlimited
        if app.applicationState == .active { return .unlimited }

        let rem = app.backgroundTimeRemaining
        // UIKit 约定:没在后台任务中时返回 DBL_MAX
        if rem > 1e10 { return .unlimited }
        return .iosBackground(remainingMs: Int64(rem * 1000))
    }
}
```

`WithBudget` decorator 在**每个 step 开头**调用 `Clock::budget_hint()`(v3.1 新签名);触发 soft_floor(<500ms)时:`state.step = Step::Paused { BudgetExceeded { dim: "background_time" } }` + `SessionStore::save_delta` 落盘并返回。下次前台激活/BG task 唤起 thaw RunState 继续(round-3 A4:是 step 粒度,不是 turn 粒度)。

### 13.2.4 BGTaskScheduler 注册

```swift
// AppDelegate.swift
BGTaskScheduler.shared.register(
    forTaskWithIdentifier: "com.example.muagent.refresh",
    using: nil) { task in
        Task {
            let agent = try buildAgent(/* model + adapters + store */)
            // 从 SessionStore 挑选一个可 resume 的 run(state.step != Done/Failed)
            if let runId = await findPendingRun() {
                let runState = try await agent.thaw(runId: runId)
                let run = try await agent.resume(runState: runState)
                task.expirationHandler = {
                    // 系统告知要超时了:cancel 后 Runner 下次 step 开头进 Paused
                    run.cancel()
                }
                _ = try await run.awaitOutput()
            }
            task.setTaskCompleted(success: true)
        }
    }
```

要点:
- `expirationHandler` 必须调 `cancel()`;Runner 下次 step 开头会感知 cancel,把 `state.step` 置为 `Step::Paused { HostRequested }` 并 `save_delta` 落盘。step 粒度的原子性保证丢不了超过一个 step 的进度(round-3 A4)。
- `task.setTaskCompleted(success:true/false)` 必调,否则系统下次调度你这个 app 会降权。

### 13.2.5 主动 BackgroundTask 抢救

用户按 Home 的一瞬间,当前 step 可能正在跑:
```swift
class AgentBackgroundGuard {
    var taskId: UIBackgroundTaskIdentifier = .invalid

    func onDidEnterBackground(agent: Agent) {
        taskId = UIApplication.shared.beginBackgroundTask {
            agent.cancel()   // 兜底:30s 结束前强制取消
            UIApplication.shared.endBackgroundTask(self.taskId)
        }
    }

    func onCurrentTurnFinished() {
        if taskId != .invalid {
            UIApplication.shared.endBackgroundTask(taskId)
            taskId = .invalid
        }
    }
}
```

这给了 Runner **大约 30 秒**来:做完当前 step、或者让下一次 step 开头感知 cancel 并 Paused 落盘。

### 13.2.6 Silent Push 驱动的 agent

场景:云端发来"新邮件到了,需要 agent 摘要"。

```swift
func application(_ app: UIApplication,
                 didReceiveRemoteNotification userInfo: [AnyHashable : Any],
                 fetchCompletionHandler: @escaping (UIBackgroundFetchResult) -> Void) {
    Task {
        let agent = try await buildAgent(.silentPushProfile)
        let out = try await agent.run(makeInputFromPush(userInfo))
        fetchCompletionHandler(.newData)
    }
}
```

`.silentPushProfile`:`Budget::tokens=2_000, turns=4, wall=20s`。
不要贪心——超时会降低 app 后续获得 push 唤起的优先级。

### 13.2.7 不可能的场景

- 持续 1 小时的"代码索引重建"(`BGProcessingTask` 最多几分钟)。
- "用户装了 app,2 周没打开,agent 在后台自己定期跑" —— iOS 启发式会完全停止你的 BG task 调度。
- 不走 push / 不走 UI / 不走 location / 不走 audio 的"无理由常驻"。

## 13.3 Android 后台执行模型详解

### 13.3.1 后台执行层次

| 机制 | 能跑多久 | 需要 | 说明 |
|---|---|---|---|
| **Foreground Service** | 无限(用户可杀) | **必须显示持续通知** | 长 agent 的唯一合法办法 |
| **WorkManager** | 单次 ~10 分钟 | `WorkManager` 调度 | 推荐的常规后台做法,兼容 Doze |
| **JobScheduler**(底层) | 类 WorkManager | API 21+ | 建议直接用 WorkManager |
| **AlarmManager `setExactAndAllowWhileIdle`** | 一次性唤起执行 | 特殊用途 | 慎用,受 Doze 影响 |
| **BroadcastReceiver + JobIntentService** | 短时 | 系统事件触发 | 启动时做点事 |
| **FCM High Priority** | ~10 秒 | 服务器推 | 相当于 iOS silent push |
| **Accessibility Service** | 无限 | 用户显式开 | 自动化 / UI 操控场景 |

### 13.3.2 Foreground Service 模板

```kotlin
class MuAgentService : LifecycleService() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notif = NotificationCompat.Builder(this, CHAN_ID)
            .setSmallIcon(R.drawable.ic_agent)
            .setContentTitle("μAgent 正在运行")
            .setContentText(currentSessionBrief)
            .setOngoing(true)
            .build()
        startForeground(NOTIF_ID, notif,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)  // API 34+ 需声明类型

        lifecycleScope.launch {
            val agent = buildAgent(...)
            val run = agent.run(intent!!.getStringExtra("input")!!)
            run.events.collect { event -> updateNotifFromEvent(event) }
        }
        return START_STICKY
    }
}
```

必要声明(`AndroidManifest.xml`):
```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_DATA_SYNC" />
<service
    android:name=".MuAgentService"
    android:foregroundServiceType="dataSync"
    android:exported="false" />
```

### 13.3.3 WorkManager(推荐)

```kotlin
class AgentWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val agent = buildAgent(inputData.toProfile())
        val out = try { agent.run(inputData.getString("input")!!).awaitOutput() }
                  catch (e: Throwable) { return Result.retry() }
        return Result.success(out.toWorkOutput())
    }
}

// 一次性
OneTimeWorkRequestBuilder<AgentWorker>()
    .setConstraints(Constraints.Builder()
        .setRequiredNetworkType(NetworkType.CONNECTED)
        .setRequiresBatteryNotLow(true)
        .build())
    .setInputData(workDataOf("input" to userQuery))
    .build()

// 周期
PeriodicWorkRequestBuilder<AgentWorker>(15, TimeUnit.MINUTES)  // 最小 15min
```

### 13.3.4 Doze / Battery Optimization

- 手机闲置屏幕关闭后进 Doze,wake locks 被限制,网络窗口只开短暂。
- WorkManager 的任务会被 **延迟** 到 Doze maintenance window。
- 需要"即时"唤起只能靠 FCM high priority。

对 μAgent 的影响:
- Backend = 本地 LLM 时,Doze 下 CPU 受限,inference 变慢——这 OK。
- Backend = 远端时,Doze 期间 HTTP 可能被掐,`ProviderTransient` 重试即可。
- 任何长 agent 必须走 Foreground Service + 通知。

### 13.3.5 OEM 厂商"激进杀手"

小米 / 华为 / OPPO / VIVO 的省电策略比 AOSP 激进。常见现象:app 退到后台 5 分钟就被 kill,连 WorkManager 都不调度。

缓解:
- 引导用户把 app 加到"后台白名单"。
- Hint 文案:"如果定时任务不稳定,请在设置 → 电池 → 允许后台活动。"
- 永远假定 agent 可能随时被杀:`RunState freeze` 在**每个 step 结束**落盘一次(step 粒度,不是 turn)。

### 13.3.6 Android Background Budget

```kotlin
class AndroidClock : Clock {
    override fun nowMs(): Long = System.currentTimeMillis()

    override fun budgetHint(): BudgetHint = when (lifecycleTier) {
        Tier.Foreground, Tier.ForegroundService -> BudgetHint.Unlimited
        Tier.Worker            -> BudgetHint.AndroidWorker(totalCapMs = 10 * 60_000)
        Tier.BackgroundReceiver-> BudgetHint.Custom(remainingMs = 10_000, source = "receiver")
    }
}
```

Core 会按这个粗略 hint 决定是否提前 freeze。

## 13.4 μAgent 的设计契合点

### 13.4.1 RunState freeze / thaw(替代 v1 的 Session freeze)

持久化对象是 `RunState`,包含:
- `step` — 停在哪个 FSM 状态(Ready / ModelTurn / ToolBatch { calls, cursor } / ToolIntent / Paused / Done / Failed)
- `history` — 用户可见 transcript(含 tool_calls 与 tool_result 成对数据)
- `event_seq` — 事件单调序号,host 订阅方 at-least-once 去重锚点
- `usage` — 预算进度(tokens / turns / tool_calls / cost)
- `schema_version` — 跨 Core 版本的 thaw migration 锚点

后台执行的三个触发路径都走同一机制:
- 进 background(`Clock::budget_hint()` 的 soft_floor 触顶)→ Runner.step 开头检测 → `Step::Paused { BudgetExceeded { dim: "background_time" } }` → persist → return
- background task 唤起 → `SessionStore::load_run(id)` → RunState → 调 `RunState::resume_hint()` 把 Paused 还原为上次的工作 step → 继续 `Runner::step` 循环
- OEM 杀进程 → 冷启动 → 同上(SessionStore 里还在 —— state.json + events.jsonl)

### 13.4.2 AtMostOnce 工具的"正在执行"中断

评审指出的一个真实难点:**`Step::ToolBatch` 里某个 `AtMostOnce` 工具正在执行时进程被杀**,thaw 后不能重跑(可能导致重复扣款 / 重复发消息)。

解决方案(已在 [03 §3.2 Step::ToolIntent](03-core-loop.md#32-step-枚举) + [§3.4 on_tool_intent_recover](03-core-loop.md#34-runnerstep)):
1. `ToolExecutor::execute` 对 `Idempotency::AtMostOnce` 的工具,**先持久化 `Step::ToolIntent`**,再真正执行。
2. 被中断:thaw 看到 `Step::ToolIntent` → 不重跑 → 生成 `ToolResult { ok: false, content: "interrupted, effect status unknown; verify before retry", retryable: false }` 回灌。
3. LLM 看到这条 obs,会先用 read-only 工具确认副作用状态(例如 `fs.stat` / `sys.calendar.get_event`),再决定如何继续。

这个机制让 at-most-once 契约**跨进程崩溃**仍然成立。

### 13.4.3 每 step 落盘(而不是每 turn)

v3.1 的持久化粒度是"**每 step**"。`Runner::step` 每次状态变化(Ready → ModelTurn → ToolBatch → ToolBatch cursor++ → ModelTurn ...)都在单事务里写 RunState + 对应 events。

成本:典型 step 增量 5–20KB,WAL 模式下 ~2–5ms。一个复杂 session 一生 200–500 次 step 级 commit,总开销可控(~1s)。
收益:**任何点被杀都最多丢半个 step**(例如 ToolBatch 刚执行完 call_k 但还没推进 cursor 就崩——重启后重跑 call_k,如果是 Idempotent 无所谓,如果是 AtMostOnce 走 ToolIntent 保护)。

### 13.4.4 Budget 感知背景

`Budget` 的 `BackgroundTime` 维度专门为此设:
```rust
pub struct Budget {
    ...
    pub background_floor: Duration,   // 默认 500ms,低于此阈值触发 freeze
}
```

### 13.4.5 事件 `Paused`(v3.1 · 没有 SessionResumed)

```rust
pub enum Event {
    ...
    Paused { reason: String, seq: u64 },
    // v3.1 没有显式 Resumed 事件;恢复靠:
    //   1) host 调 SessionStore::load_run → RunState
    //   2) 直接 runner.step(&mut state) 继续
    //   3) 下一条产生的事件(例如 StepAdvanced)就代表继续了
}
```

UI 可呈现"后台时间用尽,已暂停,前台点击继续"。若 host 想要显式"Resumed"信号,自己在调 `runner.step` 之前发一条 host-level 事件即可。

## 13.5 典型 iOS 场景设计

| 场景 | 模式 | Profile |
|---|---|---|
| 用户主动问一句话 | 前台 | `ios_default` |
| 远程邮件到了,摘要 | Silent push + 完成 handler | `ios_silent_push`(tokens=2k, turns=4, wall=20s) |
| 每 30 分钟同步日程 | BGAppRefreshTask | `ios_refresh`(tokens=1k, turns=3, wall=25s) |
| 晚上充电时重建本地 embedding 索引 | BGProcessingTask | `ios_processing`(tokens=20k, turns=100, wall=300s) |
| Siri / Shortcuts 调 | 前台短时 | `ios_shortcut`(tokens=4k, turns=10, wall=20s) |
| 语音助手(持续) | audio background mode | `ios_voice`(专用) |

## 13.6 典型 Android 场景设计

| 场景 | 模式 | Profile |
|---|---|---|
| 用户主动问 | 前台 | `android_default` |
| 长 agent(用户持续看) | Foreground Service + 通知 | `android_fgs` |
| 周期同步 | WorkManager periodic 15min | `android_worker`(wall=10min) |
| 即时推送回应 | FCM high priority | `android_fcm`(tokens=2k, turns=4) |
| UI 自动化 | Accessibility Service | `android_a11y`(专用) |

## 13.7 给 Host App 开发者的原则

1. **每个 agent 运行都要有一个"合法理由"**:用户请求 / push / 周期任务 / 前台服务。没有理由就不跑。
2. **永远假设会被打断**。turn 级别的幂等和 session 落盘是底线。
3. **通知语言要诚实**。Android Foreground Service 的通知不能写"正在同步" —— 要写真实内容,否则 Play Store 审核拒。
4. **iOS 不要用 VoIP push 硬撑常驻 agent** —— 被抓到会整条 app 家族被拒审。
5. **给用户一个"工作台"屏**,让用户看到 agent 当前状态、可手动唤醒。这比偷偷摸摸后台跑安全,也更符合平台审核偏好。

## 13.8 与错误模型的配合

背景执行产生的状态都是 **正常暂停**,不是错误:
- `Step::Paused { BudgetExceeded { BackgroundTime } }` → 由 `WithBudget` decorator 设(见 [04 §4.8](04-error-policy.md#48-budgetpolicy预算检查-decorator))
- `Step::Paused { HostRequested }`(由 `expirationHandler` / FGS 被杀触发的 `cancel()`)→ Runner 下次 step 开头感知
- `Step::Paused { BudgetExceeded { dim: "background_time" } }`(由 `WithBudget` 检测 `Clock::budget_hint()` 设)

**都不是 `Step::Failed`**。Paused 状态下 `SessionStore::load_run` 拿回来的 RunState 可以直接继续 `Runner::step`。Core 保证 **任何暂停都留下可恢复的 RunState**,Host 只需存 `run_id`。

## 13.9 (已删)Approval 背景处理

v3.1 里 Core 没有 approval 机制,也就不存在"背景中等待审批"的特殊 step。若未来加回审批,需要重新设计此节。
