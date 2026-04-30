# 05 · Adapters(v3.1 · Shell 实现细节 · 无 PermissionBroker)

> v1 叫 "PAL",作为 Core 的主世界观。
> v2 降级为 ToolExecutor 实现的依赖容器。
> v3 明确是 shell 实现细节,不是架构主层。
> v3.1 进一步简化:**`PermissionBroker` 从 AdapterBundle 完全移除**;`GuardOutcome` 只剩 `Allow` / `Deny`,没有 `NeedPermission` / `NeedExternalDecision`。

## 5.1 Core 看不到 Adapter

Core 的 3 个必备 trait:`ModelAdapter` / `ToolExecutor` / `SessionStore`。

`FileSystem` / `ProcessExec` / `InterApp` / `NetEgress` / 等等全部是 Shell 或 host 层面的概念。Core 完全不感知。

## 5.2 Default Shell 的 `AdapterBundle`

`muagent-shell` 提供参考形态:

```rust
pub struct AdapterBundle {
    pub fs:          Arc<dyn FileSystem>,
    pub proc:        Option<Arc<dyn ProcessExec>>,
    pub interapp:    Option<Arc<dyn InterApp>>,
    pub net:         Arc<dyn NetEgress>,
    pub secrets:     Arc<dyn Secrets>,
    pub model_store: Arc<dyn ModelStore>,
    pub notifier:    Option<Arc<dyn Notifier>>,
    pub image_codec: Option<Arc<dyn ImageCodec>>,
    pub camera:      Option<Arc<dyn Camera>>,
    pub mic:         Option<Arc<dyn Microphone>>,
    pub screen:      Option<Arc<dyn Screen>>,
    pub sensors:     Option<Arc<dyn Sensors>>,
    pub hw_gpio:     Option<Arc<dyn HwGpio>>,
}
```

**没有 `PermissionBroker`**。OS 权限完全由具体 tool 内部处理(见 §5.6)。

## 5.3 冻结约束

`AdapterBundle` 在 Runner/ToolExecutor 构造后冻结。

## 5.4 `DefaultToolExecutor`(shell 参考实现)

```rust
pub struct DefaultToolExecutor {
    registry: Arc<ToolRegistry>,
    adapters: Arc<AdapterBundle>,
    metrics:  Arc<ToolMetrics>,
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(&self, call: &PendingCall)
        -> Result<ToolResult, ToolExecutorError>
    {
        let tool = self.registry.resolve(&call.tool_name)
            .ok_or(ToolExecutorError::UnknownTool(call.tool_name.clone()))?;

        // 1) schema 校验
        if let Err(e) = validate_schema(&tool.descriptor().schema, &call.args) {
            return Ok(ToolResult::err(format!("args invalid: {e}"), true, None));
        }

        // 2) guard:静态/参数级检查
        match tool.guard(&call.args, &self.adapters) {
            GuardOutcome::Allow => {}
            GuardOutcome::Deny { reason, hint } =>
                return Ok(ToolResult::err(reason, false, hint)),
        }

        // 3) sandbox 执行,panic-safe + timeout
        let fut = tool.run(call.args.clone(), ToolCtx::new(&self.adapters));
        let run = tokio::time::timeout(tool.descriptor().timeout, fut);
        let outcome = AssertUnwindSafe(run).catch_unwind().await;

        let result = match outcome {
            Ok(Ok(Ok(ok)))  => ToolResult::ok(ok.content, ok.parts,
                                               tool.descriptor().max_out_tokens),
            Ok(Ok(Err(te))) => ToolResult::err(te.msg, te.retryable, te.hint),
            Ok(Err(_to))    => ToolResult::err("timeout".into(), true,
                                Some("try smaller input".into())),
            Err(panic)      => {
                let msg = sanitize_panic(panic);
                ToolResult::err(format!("internal: {msg}"), true, None)
            }
        };
        Ok(result)
    }

    fn idempotency_for(&self, call: &PendingCall) -> Idempotency {
        self.registry.resolve(&call.tool_name)
            .map(|t| t.idempotency_for_args(&call.args))
            .unwrap_or(Idempotency::AtMostOnce)
    }
}
```

## 5.5 `GuardOutcome`(简化)

```rust
pub enum GuardOutcome {
    Allow,
    Deny { reason: String, hint: Option<String> },
}
```

只两个 variant。没有 `NeedPermission` / `NeedExternalDecision`。

**含义**:tool 的 guard **只做同步、纯函数式、不弹对话框**的检查(例如 schema 额外约束、路径越权、sandbox 根校验)。任何"需要让用户做决定"的事情都由 tool 自己在 `run()` 里处理,最终以 `ToolResult` 形式表达结果。

## 5.6 OS 权限怎么办(v3.1)

权限管理**完全在 tool 实现内部**。典型模式:

```rust
#[tool(name = "sys.calendar.list_events")]
async fn calendar_list(ctx: &ToolCtx<'_>, range: String) -> Result<Vec<Event>, ToolErr> {
    // tool 自己内部处理授权
    match ctx.adapters.interapp.as_ref() {
        Some(ia) => ia.invoke("sys.calendar.list_events",
                              json!({"range": range}), ctx.cancel.clone()).await
            .map_err(|e| match e {
                InterAppErr::NotAuthorized => tool_err_nonretry(
                    "calendar permission not granted;\
                     prompt user to enable in system Settings"),
                _ => tool_err_retry(e.to_string()),
            }),
        None => Err(tool_err_nonretry("calendar not available on this platform")),
    }
}
```

- Adapter 的实现(iOS 的 `AppIntentsBridge` 等)**自己**在第一次调用时触发 OS 授权对话框。
- 若对话框被用户拒绝或系统已知拒绝,adapter 返 `InterAppErr::NotAuthorized`。
- tool 把这个错包成 `ToolResult { ok:false, retryable:false, hint:"..." }` 回灌给 LLM。
- LLM 看到提示,告诉用户去设置中启用。

**Runtime / Core 全程不知道"权限"概念**。权限是 adapter + tool 的私事。

## 5.7 Adapter trait 签名

```rust
#[async_trait] pub trait FileSystem: Send + Sync { /* roots / stat / read / write / list / delete / rename / request_root_access */ }
#[async_trait] pub trait ProcessExec: Send + Sync { /* available / allowlist / run */ }
#[async_trait] pub trait InterApp: Send + Sync { /* list_actions / describe / invoke */ }
#[async_trait] pub trait NetEgress: Send + Sync { /* http / open_sse / open_ws / policy */ }
#[async_trait] pub trait Secrets: Send + Sync { /* get / put / delete */ }
#[async_trait] pub trait ModelStore: Send + Sync { /* list / ensure / delete / total_bytes */ }
#[async_trait] pub trait Notifier: Send + Sync { /* post */ }
#[async_trait] pub trait ImageCodec: Send + Sync { /* decode / encode_jpeg / resize / heic_to_jpeg */ }
#[async_trait] pub trait Camera: Send + Sync { /* capture / pick_from_library */ }
#[async_trait] pub trait Microphone: Send + Sync { /* record / stream */ }
#[async_trait] pub trait Screen: Send + Sync { /* capture */ }
#[async_trait] pub trait Sensors: Send + Sync { /* read / subscribe */ }
#[async_trait] pub trait HwGpio: Send + Sync { /* read / write / configure */ }
```

`FileSystem::request_root_access(purpose)` 保留——它返回一个新的 `Root`,由 adapter 内部同步处理 `UIDocumentPickerViewController` / SAF picker 等 UI 流程。tool 或 host 调用它来**扩展可读写根**,不是一个"runtime 层级的权限"概念。

## 5.8 Optional Adapter → 内置 tool 桥接

| Adapter | 触发的内置 tool | SideEffects |
|---|---|---|
| `Notifier`    | `sys.notify(title, body)`                    | Mutating |
| `Camera`      | `camera.capture(max_dim)` / `camera.pick`    | Mutating |
| `Microphone`  | `mic.record(duration_s)`                     | Mutating |
| `Screen`      | `screen.capture(region)`                     | ReadOnly |
| `Sensors`     | `sensor.read(kind)`                          | ReadOnly |
| `HwGpio`      | `hw.gpio.read/write`                         | Mutating/Destructive |

Adapter 缺失 → 对应 tool 不进 active tool set → LLM 看不到。

## 5.9 各平台 AdapterBundle 构造(示例)

### Linux / Pi
```rust
pub fn linux_bundle(cfg: LinuxConfig) -> AdapterBundle {
    AdapterBundle {
        fs: Arc::new(LinuxFileSystem::new(cfg.fs_roots)),
        proc: Some(Arc::new(LinuxProcessExec::with_allowlist(cfg.cmd_allow))),
        interapp: Some(Arc::new(DbusInterApp::new())),
        net: Arc::new(ReqwestEgress::new(cfg.net_policy)),
        secrets: Arc::new(FileSecrets::at("~/.muagent/secrets")),
        model_store: Arc::new(FsModelStore::at("~/.muagent/models")),
        notifier: Some(Arc::new(LibNotifyNotifier::new())),
        image_codec: Some(Arc::new(ImageCrateCodec)),
        camera: cfg.enable_v4l2.then(|| Arc::new(V4l2Camera::new()) as _),
        mic: cfg.enable_alsa.then(|| Arc::new(CpalMicrophone::new()) as _),
        screen: cfg.enable_screen.then(|| Arc::new(ScrotScreen::new()) as _),
        sensors: None,
        hw_gpio: cfg.enable_gpio.then(|| Arc::new(SysfsGpio::new()) as _),
    }
}
```

### iOS(Swift 侧 via UniFFI)
```swift
func buildBundle() -> AdapterBundle {
    AdapterBundle(
        fs: IOSFileSystem(),
        proc: nil,                            // iOS 无 sh.exec
        interapp: AppIntentsBridge(),
        net: URLSessionEgress(),
        secrets: KeychainSecrets(),
        modelStore: IOSModelStore(),
        notifier: UserNotificationsNotifier(),
        imageCodec: CoreImageCodec(),
        camera: PHPickerCamera(),
        mic: AVAudioMicrophone(),
        screen: nil,
        sensors: CoreMotionSensors(),
        hwGpio: nil
    )
}
```

**没有 permission broker**。iOS 权限由具体 Adapter(例如 `AppIntentsBridge` / `PHPickerCamera`)内部处理。

### Android(Kotlin via UniFFI)
```kotlin
fun buildBundle(ctx: Context): AdapterBundle = AdapterBundle(
    fs = AndroidFileSystem(ctx),
    proc = AndroidProcessExec.restricted(),
    interapp = IntentBridge(ctx),
    net = OkHttpEgress(),
    secrets = EncryptedPrefSecrets(ctx),
    modelStore = AndroidModelStore(ctx),
    notifier = NotificationManagerNotifier(ctx),
    imageCodec = AndroidImageCodec(),
    camera = CameraXCamera(ctx),
    mic = MediaRecorderMic(ctx),
    screen = null,
    sensors = SensorManagerSensors(ctx),
    hwGpio = null,
)
```

## 5.10 host 可以不用 AdapterBundle

Core 只需要一个 `Arc<dyn ToolExecutor>`。host 完全可以:
- 自己写 ToolExecutor,不消费 AdapterBundle
- 直接用 `tokio::fs` / `reqwest` 实现 tool
- 任何需要的地方自己处理 OS 权限(典型 tool 内部模式,见 §5.6)

## 5.11 v3 → v3.1 差异

| v3 | v3.1 |
|---|---|
| `AdapterBundle.perm: Arc<dyn PermissionBroker>` | **删除** |
| `GuardOutcome::NeedExternalDecision { kind, payload }` | **删除** |
| `GuardOutcome::NeedPermission(Permission)` | **删除** |
| `ToolExecutor::execute -> Result<ToolOutcome, ...>` | → `Result<ToolResult, ...>` |
| 说 "approval / permission 通过 Suspend 机制处理" | **删除**;权限由 tool 实现自己管 |
| `muagent-approval` / `muagent-permission` addon crate 计划 | **删除**(future 再议) |

## 5.12 保留

所有其它 Adapter trait 签名、AdapterBundle 冻结约束、三段门禁(schema / guard / sandbox)、panic-safe 包装、Idempotency 校验。

## 5.13 Implementation gotchas(实现适配器时的真实踩坑记录)

### 5.13.1 Root paths 和 URI 必须**两头都 canonicalize**

**现象**:host 把 `/var/folders/xxx` 作为 `FileSystem` 的 root 传进来;agent 拿到的 skill folder URI 是 `file:///private/var/folders/xxx/...`(因为系统其它地方 canonicalize 过了)。`fs_read` 返回 `"outside all roots: ..."`。

**Root cause**:adapter 把 host 给的路径原样存进 roots 列表,然后用字符串 `starts_with` 比对进来的 URI 路径。macOS 上 `/var` 是 `/private/var` 的 symlink(Linux 的 `/var` 也常被容器化软链);两条路径**语义等价但字节不同**,starts_with 失败。

**同类陷阱**(未来其它平台会中):
- **macOS**: `/var/folders` ↔ `/private/var/folders`,`/tmp` ↔ `/private/tmp`
- **Linux**: `/var/run` ↔ `/run`,某些发行版 `/bin` 是 `/usr/bin` 的 symlink
- **iOS**: Data Container 路径里有 UUID 段,沙盒内应用看到的路径 vs `bookmarks` 解析出的绝对路径可能不一致
- **Android**: `/sdcard` ↔ `/storage/emulated/0` ↔ `/storage/self/primary`,`getExternalFilesDir()` 返回的路径跨 app 不稳定
- **Windows**(未来):`C:\Users\x` 的大小写差异、`\\?\` 长路径前缀、NTFS junctions

**正确实现**(所有 `FileSystem` adapter 都应这样):

1. **构造时** canonicalize 每个 root,存规范路径:
   ```rust
   let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
   ```
2. **resolve 时** 对进来的 URI 路径也 canonicalize,再和 root 比对:
   ```rust
   let canon_path = match path.canonicalize() {
       Ok(p) => p,
       Err(_) => {
           // 文件可能还不存在(fs_write 新建场景),对 parent canonicalize
           match path.parent().and_then(|p| p.canonicalize().ok()) {
               Some(parent) => parent.join(path.file_name().unwrap_or_default()),
               None => path.clone(),
           }
       }
   };
   ```
3. **starts_with** 比对而非字符串前缀 —— `PathBuf::starts_with` 按路径段比较,避免 `/foo-bar` 误匹 `/foo` 这类 bug。

**测试方法**:Live skill E2E 测试(tmp 目录里放 `SKILL.md` + reference file,让 agent 按协议读)最能暴露这类问题 —— 因为不同代码路径产出的路径写法会不一致。纯离线测试里 host 一般只用自己构造的路径,不跨 canonicalize 边界,查不出来。

### 5.13.2(预留)

未来适配 iOS/Android/Windows 时踩到的坑记在这里。命名规则:`5.13.N <一句话现象>`。
