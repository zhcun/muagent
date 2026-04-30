# 08 · 平台能力矩阵 · Backend 优先级

> round-2 B8:macOS 显式列出。macOS 与 iOS 共用 Apple Foundation Models backend,但作为桌面 Unix 还具备 `sh.exec` 等完整能力。

## 8.1 能力存在性(按 Capability id)

图例:✅ 原生支持 · ⚠️ 受限 · ❌ 不可用 · 🔁 用替代能力代偿

| Capability id | Linux/Pi | macOS | iOS | Android |
|---|:-:|:-:|:-:|:-:|
| `fs.read` / `fs.write` / `fs.list` | ✅ | ✅ sandbox + bookmark | ✅ sandbox + bookmark | ✅ sandbox + SAF |
| `fs.delete` / `fs.rename` | ✅ | ✅ | ✅ | ✅ |
| `sh.exec` | ✅(allowlist) | ✅(allowlist) | ❌ | ⚠️(allowlist,非 root) |
| `sh.which` / `sh.env` | ✅ | ✅ | ❌ | ⚠️ |
| `interapp.*`(虚拟 skill) | ✅ D-Bus/exec | ✅ AppleScript / App Intents | ✅✅ App Intents / Shortcuts | ✅ Intents |
| `sys.calendar` | ⚠️ CalDAV | ✅ EventKit | ✅ EventKit | ✅ CalendarProvider |
| `sys.contacts` | ⚠️ vCard | ✅ | ✅ | ✅ |
| `sys.reminders` | 🔁 `sys.todo` | ✅ | ✅ | 🔁 |
| `sys.notifications` | ✅ libnotify | ✅ UN | ✅ UN | ✅ NotificationManager |
| `sys.location` | ⚠️ GeoClue | ✅ CoreLocation | ✅ CoreLocation | ✅ FusedLocation |
| `sys.health` | ❌ | ⚠️ | ✅ HealthKit | ⚠️ Health Connect |
| `sys.photos`(元数据+导出) | ❌ | ✅ PhotoKit | ✅ PhotoKit | ✅ MediaStore |
| `sys.pasteboard` | ✅ xclip/wl-copy | ✅ | ✅ | ✅ |
| `sys.keychain` / secrets | ✅ libsecret | ✅ Keychain | ✅ Keychain | ✅ Encrypted SP |
| `net.http` | ✅ | ✅ URLSession | ✅ URLSession | ✅ OkHttp |
| `net.dns_lookup` | ✅ | ✅ | ⚠️ | ⚠️ |
| `hw.gpio` / `hw.i2c` / `hw.spi` | ✅ sysfs | ❌ | ❌ | ❌(通常) |
| `sensor.accel` / `gyro` / `light` | ❌ | ⚠️ Mac laptop only | ✅ CoreMotion | ✅ SensorManager |
| `sensor.ble_scan` | ✅ BlueZ | ✅ CoreBluetooth | ✅ CoreBluetooth | ✅ |
| `screen.capture` | ✅ scrot/grim | ✅ ScreenCaptureKit | ❌ | ⚠️ MediaProjection |
| `llm.*`(LLM backend) | ✅ Ollama/llama.cpp | ✅ Apple FM/MLX/llama.cpp | ✅ Apple FM/MLX/llama.cpp | ✅ AICore/ExecuTorch/llama.cpp | 🔁 remote |

**macOS 与 iOS 差异**:
- macOS 是桌面 Unix,`sh.exec` / `screen.capture` / `sh.which` 都有,适合开发者工具类 agent 场景。
- iOS 仍然禁 `sh.exec`,`screen.capture` 也禁(app 只能截自己的 view)。
- 两者共享 `Apple Foundation Models` backend(macOS 14+ / iOS 26+)、EventKit / PhotoKit 等系统框架。

## 8.2 ModelAdapter 优先级(运行时 fallback 链)

### iOS(按可用性依次 fallback)
```
1. Apple Foundation Models        (iOS 26+ 且 eligible 设备,tool_use 原生)
2. MLX-Swift + 本地 Gguf/MLX 权重  (iPhone 12+ 推荐 3B Q4)
3. llama.cpp + Metal               (老设备兼容)
4. 远端 OpenAI-compat / Claude     (fallback,需网络)
```

决策逻辑:
```rust
// host-side 装配,挑最佳 ModelAdapter(不是 runtime 内部概念)
fn pick_model(profile: &RuntimeProfile, candidates: &ModelCandidates)
    -> Arc<dyn ModelAdapter>
{
    if profile.model.prefer_system && candidates.apple_fm.availability() == Ready {
        return candidates.apple_fm.clone();
    }
    if let Some(local) = &candidates.mlx {
        if local.availability() == Ready { return local.clone(); }
    }
    candidates.llamacpp_metal.clone()
        .unwrap_or_else(|| candidates.remote_fallback.clone())
}
```

### Android
```
1. AICore + Gemini Nano 4 / Gemma 4  (DP 2026-04;tool_use 开发中)
2. ExecuTorch + 本地权重               (Meta 生产级)
3. llama.cpp + Vulkan                 (广兼容)
4. 远端 cloud
```

### Linux / Pi
```
1. 本机 Ollama HTTP (http://127.0.0.1:11434)
2. llama.cpp 进程内 (gguf + CPU/CUDA)
3. 远端 OpenAI-compat
```

## 8.3 小模型 + tool use 的兼容策略

很多 3B 级别本地模型(DeepSeek-R1-Distill 1.5B、Llama 3.2 1B)**不原生支持 tool_use**。Core 根据 `LlmCaps::native_tool_use` 切换:

- **native 支持**:直接用 provider 的 tool_use 协议。
- **不支持**:启用 **ReAct-fallback 模式**:
  - ContextBuilder 在 system prompt 里加一段"如果需要调用工具,输出 `<tool_call>{...}</tool_call>` 块"。
  - LlmStream 收到 text 后,Core 的 `ToolCallParser` 扫 `<tool_call>...</tool_call>` 抽取。
  - 如果 LLM backend 支持 **grammar-constrained decoding**(llama.cpp GBNF / MLX logit processor),把 tool schema 编译成 grammar 约束输出结构。

## 8.4 RuntimeProfile 预置

```rust
impl RuntimeProfile {
    pub fn ios_default() -> Self {
        Self {
            platform: Platform::IOS,
            allowed_caps: vec!["fs.*", "sys.*", "interapp.*", "net.http",
                               "skill:*", "mcp:*", "cap.*", "session.*",
                               "system.*"].into(),
            denied_caps: vec!["sh.*", "hw.*"].into(),
            budgets: Budget::ios_mobile(),  // tokens=8k, turns=20, wall=60s
            model: ModelSpec::prefer_system_or_mlx("llama-3.2-3b-q4"),
            routing: RoutingPolicy::LocalFirst,
            audit: AuditLevel::Standard,
        }
    }

    pub fn android_default() -> Self { ... }

    pub fn raspberry_pi() -> Self {
        Self {
            platform: Platform::Linux,
            allowed_caps: vec!["fs.*", "sh.*", "net.*", "sys.notifications",
                               "hw.gpio", "skill:*", "mcp:*", "cap.*",
                               "session.*", "system.*"].into(),
            denied_caps: vec![],
            budgets: Budget::long_running(),
            model: ModelSpec::ollama_local("qwen2.5:7b-instruct"),
            routing: RoutingPolicy::LocalFirst,
            audit: AuditLevel::Verbose,
        }
    }

}
```

## 8.5 Channel × 平台兼容

| Channel | Linux | iOS | Android |
|---|:-:|:-:|:-:|
| CLI / stdin | ✅ | ❌ | ❌ |
| SDK call(进程内) | ✅ | ✅ | ✅ |
| HTTP server | ✅ | ⚠️ 仅 loopback | ⚠️ |
| BLE peripheral | ✅ | ✅ | ✅ |
| WebSocket | ✅ | ✅ | ✅ |
| 系统 IPC(Shortcuts / App Intents) | ❌ | ✅ | ⚠️ |
| MQTT | ✅ | ✅ | ✅ |

## 8.6 iOS 背景执行模型

iOS app 进入 background 后:
- 常规 app:最多 **~30 秒** 后被 suspend。
- `BGProcessingTask`(电充时,最多数分钟~小时)。
- `BGAppRefreshTask`(周期,每次 ~30s)。
- 通过 `UNNotification`、`PushKit` 唤起可短时运行。

μAgent 的 **冻结-解冻** 机制:
- `Clock::budget_hint()` 在每个 step 开头被 `WithBudget` decorator 查询,`soft_floor_breached(500ms)` 为真时 `state.step = Step::Paused { BudgetExceeded { dim: "background_time" } }` + `SessionStore::save_delta` 落盘。
- 下次前台激活或背景任务唤起时,`SessionStore::load_run(run_id)` → RunState → 直接 `Runner::step` 继续。
- UI 呈现"会话已暂停,点击继续"。

## 8.7 Android 背景执行模型

- 前台服务(Foreground Service)+ 长时间运行通知。
- WorkManager 的周期任务(~15 分钟最小间隔)。
- Doze 模式下网络受限。

μAgent 的做法:
- Long-running agent → 宿主 Foreground Service + 持续通知。
- 定期任务 → WorkManager wrapper。
- `BatteryOptimization` exclusion 提示 host app 去申请。

## 8.8 Linux 部署形态

- **单用户 CLI**:`muagent run "帮我整理 ~/Downloads"`。
- **systemd 服务**:`muagentd.service` 监听 HTTP / BLE / MQTT。
- **Docker**:发布 `muagent:alpine-arm64` 镜像,挂载配置目录。
