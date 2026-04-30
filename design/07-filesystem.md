# 07 · 文件系统:通用工具 × 平台 URI × 权限

> **回答核心疑问**:
> 是的,**每个平台都有 `fs.read` / `fs.write` 这些基本工具**。它们是 **Default Shell 的内置 tool**,跨平台同名、给 LLM 看的"一套 API"。Core 本身不认识 `fs.*`。
> 不同的是 **URI 命名空间** 和 **平台授权细节** —— 这两层差异由 Shell 的 `FileSystem` Adapter 实现吸收(见 [05-adapters](05-adapters.md)),LLM 不用关心。

## 7.1 核心原则

```
┌──────────────────────────────────────────────────┐
│ LLM 看到的工具(跨平台不变):                    │
│   fs.read / fs.write / fs.list / fs.stat /       │
│   fs.delete / fs.rename                          │
├──────────────────────────────────────────────────┤
│ LLM 看到的"在哪里能读写"(平台相关,运行时给): │
│   fs.roots() → [                                 │
│     { id: "sandbox", writable: true, ... },      │
│     { id: "bookmark:abc", writable: true, ... }, │
│     { id: "media", writable: false, ... }        │
│   ]                                              │
└──────────────────────────────────────────────────┘
                         │
                         ▼ (Shell 的 FileSystem Adapter 实现)
┌──────────────────────────────────────────────────┐
│  iOS:                                           │
│    sandbox://   → NSFileManager app container    │
│    bookmark://<UUID>/  → security-scoped         │
│    photos://    → PHPhotoLibrary(仅元数据)      │
│                                                  │
│  Android:                                        │
│    sandbox://   → Context.filesDir               │
│    saf://<tree-uri>/  → Storage Access Framework │
│    media://     → MediaStore                     │
│                                                  │
│  Linux/Pi:                                       │
│    file:///absolute/path  → 真实文件系统(带    │
│                           roots allow-list)     │
└──────────────────────────────────────────────────┘
```

## 7.2 URI 语法

```
scheme ":" "//" [ root-id ] "/" path

例:
  sandbox://notes/2026-04-21.md
  bookmark://7F3A-21B0/Downloads/report.pdf
  file:///home/mike/jiang/muagent/Cargo.toml
  saf://content%3A%2F%2Fcom.android.externalstorage.documents%2Ftree%2Fprimary%253A.../docs/a.md
  flash://main/config.json
```

- `scheme` 标识 root 类型。
- `root-id` 可选,用于同 scheme 多根(如多个 bookmark)。
- `path` 是相对 root 的路径,必须规范化(不允许 `..` 逃逸)。

## 7.3 Tool 定义(跨平台同名)

```rust
#[tool(name = "fs.read", side_effects = "read_only", concurrency = "parallel")]
/// 读取文件为文本或字节。
///
/// 先用 `fs.list` 或 `fs.roots` 了解可用路径。
async fn fs_read(
    ctx: &ToolCtx<'_>,
    #[desc("完整 URI,例如 sandbox://foo.txt")] uri: String,
    #[default(65536)] #[desc("最多字节数")] max_bytes: u32,
    #[default(false)] #[desc("以 base64 返回二进制")] binary: bool,
) -> Result<ReadOut, ToolErr> {
    let uri = Uri::parse(&uri).map_err(|e| tool_err_nonretry(e.to_string()))?;
    let data = ctx.pal.fs.read(&uri, ReadOpts::max(max_bytes as usize)).await?;
    Ok(if binary { ReadOut::B64(base64::encode(data)) }
       else      { ReadOut::Text(String::from_utf8_lossy(&data).into_owned()) })
}

#[tool(name = "fs.write", side_effects = "mutating", concurrency = "exclusive")]
async fn fs_write(
    ctx: &ToolCtx<'_>,
    uri: String,
    content: String,
    #[default(false)] append: bool,
    #[default(false)] create_dirs: bool,
) -> Result<WriteOut, ToolErr> { ... }

#[tool(name = "fs.list", side_effects = "read_only")]
async fn fs_list(ctx: &ToolCtx<'_>, uri: String) -> Result<Vec<Entry>, ToolErr> { ... }

#[tool(name = "fs.roots", side_effects = "read_only")]
async fn fs_roots(ctx: &ToolCtx<'_>) -> Result<Vec<Root>, ToolErr> {
    Ok(ctx.pal.fs.roots())
}

#[tool(name = "fs.stat", ...)]       // 文件元信息
#[tool(name = "fs.delete", side_effects = "destructive", idempotency = "at_most_once")]
#[tool(name = "fs.rename", side_effects = "mutating", concurrency = "exclusive")]
```

**注意**:所有 tool 的名字、schema、语义**跨平台一致**。LLM 学会一次就能用。

## 7.4 为什么 URI 而不是裸路径

裸路径(例如 `"/tmp/foo.txt"`)在三个问题上跨不过去:
1. iOS 沙盒路径是动态的,`/var/mobile/Containers/Data/Application/<UUID>/Documents/` —— 每次重装变。
2. Android 的 SAF 是 `content://` URI,根本不是 POSIX 路径。
3. 权限是按**根**授予的("用户允许访问 Downloads 文件夹"),裸路径表达不了。

用 URI 的好处:
- LLM 只需知道 root id,path 相对就行(`sandbox://notes/a.md`)。
- 不同平台同一个 URI 写法,LLM 不需重学。
- 权限语义天然绑到 scheme / root_id。

## 7.5 `roots()` 的作用与动态性

```rust
fn roots(&self) -> Vec<Root>;
```

返回 **当前可读写的根集合**。这个集合在以下情况下会变:
- 用户通过 DocumentPicker / SAF 授予了一个新文件夹 → 新增一个 `bookmark://` / `saf://` root。
- 用户吊销了授权(系统设置 → Privacy)→ root 消失。
- 外部存储插拔(Pi 上插 USB)→ 新增 `usb://`(可选)。

Core 在每次 `ContextBuilder::build` 时**不**重新查询 roots(太贵),而是:
- Adapter 实现维护一个内存缓存 + 变更通知。
- Core 把 roots 作为 `session.system_state` 的一部分注入 system prompt 头部。
- 变更触发 `Event::RootsChanged` 并在下一 turn 的 system prompt 中更新。

## 7.6 guard:路径越权检查

所有 `fs.*` tool 的 guard 都调用同一个 helper:

```rust
fn guard_uri(uri_str: &str, needed: Access, pal: &Pal) -> GuardOutcome {
    let uri = match Uri::parse(uri_str) {
        Ok(u) => u,
        Err(e) => return GuardOutcome::Deny {
            reason: format!("bad URI: {e}"),
            hint: Some("call fs.roots() to see valid roots".into()),
        },
    };
    let roots = pal.fs.roots();
    let root = match roots.iter().find(|r| uri.belongs_to(r)) {
        Some(r) => r,
        None => return GuardOutcome::Deny {
            reason: format!("uri {uri} is outside all roots"),
            hint: Some(format!("available roots: {}",
                roots.iter().map(|r| &r.uri_prefix)
                     .collect::<Vec<_>>().join(", "))),
        },
    };
    if needed == Access::Write && !root.writable {
        return GuardOutcome::Deny {
            reason: format!("root {} is read-only", root.id),
            hint: Some("try sandbox:// for writable storage".into()),
        };
    }
    if uri.has_dotdot_escape() {
        return GuardOutcome::Deny {
            reason: "path contains `..`".into(),
            hint: Some("use absolute paths within a root".into()),
        };
    }
    GuardOutcome::Allow
}
```

**关键**:guard 是 LLM 看不到的原生代码。LLM 不能通过任何 prompt 绕过路径越权检查。

## 7.7 各平台 `FileSystem` 实现要点

### iOS(`platforms/ios/Sources/MuAgentAdapters/IOSFileSystem.swift`)

```swift
final class IOSFileSystem: FileSystem {
    func roots() -> [Root] {
        var rs: [Root] = [
            Root(id: "sandbox",
                 uriPrefix: "sandbox://",
                 writable: true,
                 description: "App private storage")
        ]
        for b in bookmarkStore.all() {
            rs.append(Root(id: "bookmark:\(b.id)",
                           uriPrefix: "bookmark://\(b.id)/",
                           writable: b.hasWriteAccess,
                           description: "User folder: \(b.displayName)"))
        }
        return rs
    }

    func read(uri: Uri, opts: ReadOpts) async throws -> Data {
        switch uri.scheme {
        case "sandbox":
            let url = appDocsURL.appendingPathComponent(uri.path)
            return try await readLimited(url: url, max: opts.maxBytes)

        case "bookmark":
            let bookmarkId = uri.rootId
            let url = try bookmarkStore.resolve(bookmarkId)  // 可能抛 stale bookmark
            let gate = url.startAccessingSecurityScopedResource()
            defer { if gate { url.stopAccessingSecurityScopedResource() } }
            return try await readLimited(
                url: url.appendingPathComponent(uri.path),
                max: opts.maxBytes)

        default:
            throw FsErr.unsupportedScheme(uri.scheme)
        }
    }
    ...
}
```

- `sandbox://` 永远可用、永远可写。
- `bookmark://` 需用户首次授权(通过 `UIDocumentPickerViewController`),之后系统缓存可跨启动。
- 若 `bookmark` stale(文件夹被删/移动),`request_root_access` 让 LLM 知道要重新请求。
- `photos://` 用 PhotoKit,只暴露资产 id + metadata,不直接暴露 binary(太大);真需二进制走专门的 `photos.export_jpeg` tool。

### Android(`platforms/android/.../AndroidFileSystem.kt`)

```kotlin
class AndroidFileSystem(private val ctx: Context) : FileSystem {
    override fun roots(): List<Root> = buildList {
        add(Root(id = "sandbox", uriPrefix = "sandbox://", writable = true,
                 description = "App files dir"))
        for (uri in persistedUriPermissions()) {
            add(Root(id = "saf:${uri.encoded}",
                     uriPrefix = "saf://${uri.encoded}/",
                     writable = uri.isWritable,
                     description = "SAF tree: ${uri.displayName}"))
        }
        add(Root(id = "media", uriPrefix = "media://", writable = false,
                 description = "MediaStore (read only metadata)"))
    }
    override suspend fun read(uri: Uri, opts: ReadOpts): ByteArray = when (uri.scheme) {
        "sandbox" -> File(ctx.filesDir, uri.path).readLimited(opts.maxBytes)
        "saf"     -> ctx.contentResolver.openInputStream(uri.safTreeUri())!!
                        .useLimited(opts.maxBytes)
        "media"   -> mediaStoreRead(uri)
        else -> throw FsErr.UnsupportedScheme(uri.scheme)
    }
    ...
}
```

### Linux/Pi(`crates/muagent-pal-linux/src/fs.rs`)

```rust
pub struct LinuxFileSystem {
    allow: Vec<PathBuf>,   // 配置的根(默认 [$HOME/.muagent, /tmp/muagent])
}

impl FileSystem for LinuxFileSystem {
    fn roots(&self) -> Vec<Root> {
        self.allow.iter().map(|p| Root {
            id: p.display().to_string(),
            uri_prefix: format!("file://{}/", p.display()),
            writable: is_writable(p),
            description: format!("Directory {}", p.display()),
        }).collect()
    }
    async fn read(&self, uri: &Uri, opts: ReadOpts) -> Result<Bytes, FsErr> {
        let path = resolve_to_allowed(uri, &self.allow)?;
        tokio::fs::read(&path).await.map(Bytes::from).map_err(FsErr::from)
    }
    ...
}
```

- Linux 默认**只开少数根**,避免 agent 误读 `/etc/shadow`。
- 通过 CLI flag 或 profile 扩展:`muagent --fs-root $HOME/projects/foo`。

## 7.8 给 LLM 的 system prompt 片段

```
File system tools:
  fs.read, fs.write, fs.list, fs.stat, fs.delete, fs.rename

Always-available URIs depend on platform. Available roots right now:
  sandbox://                 (writable) App private storage
  bookmark://7F3A-21B0/      (writable) User folder: Downloads
  photos://                  (readonly) Photos library (metadata only)

Rules:
- All paths MUST use a URI with a scheme listed above.
- Relative paths or `..` are rejected. Normalize before calling.
- If you need a new folder that's not in `fs.roots()`, call `fs.request_root_access(purpose)` — this triggers the OS document picker so the user can grant access. A new root will appear in `fs.roots()` afterwards.
- For binary content, pass binary=true to fs.read (base64 output).
```

## 7.9 与 Skill 的交互

一些 Skill 会同时读写多种根(例如 `skill-notes` 在沙盒写笔记,在 bookmark 根导出)。
Skill 内部 tool 复用同样的 `pal.fs.*`,guard 自动生效。

Skill 永远不能绕过 Adapter 层:
- 不能直接用 `std::fs::File::open`(编译时被禁,Core crate 没 `std::fs` 依赖)。
- 只能拿 `ToolCtx::pal::fs` 的 trait object 调用,策略链必走。

## 7.10 错误语义(回灌给 LLM)

| Adapter FsErr | ToolResult |
|---|---|
| `NotFound` | ok=false, retryable=true, hint="call fs.list(parent) first" |
| `PermissionDenied(root_id)` | ok=false, retryable=false, hint="root permission lacking; try fs.request_root_access or switch root" |
| `Quota` | ok=false, retryable=false, hint="free space first" |
| `StaleBookmark` | ok=false, retryable=false, hint="root expired; request_root_access again" |
| `UnsupportedScheme(s)` | ok=false, retryable=false, hint="available schemes: sandbox://, ..." |
| `TooLarge` | ok=false, retryable=true, hint="pass smaller max_bytes or use fs.stat first" |
| `Io(msg)` | ok=false, retryable=true, hint=None |

## 7.11 总结表

| 项 | LLM 感知(跨平台) | Adapter 实现(平台各异) |
|---|---|---|
| Tool 名 | `fs.read` 等 6 个 | 同 |
| URI scheme | 动态由 `fs.roots()` 列出 | 各自挂不同 backend |
| 权限 | 通过 guard 拒绝 + hint 请求 | iOS DocumentPicker / Android SAF / Linux config |
| 错误语义 | 统一 `ToolResult{retryable, hint}` | 把 OS 异常映射到 FsErr enum |

**一句话总结**:**工具同名 + URI 分 scheme + 平台差异在 Adapter**——LLM 视角是一套 API,Adapter 视角是多套实现;guard 只做路径越权/写权限等静态规则,不做 "approval" / "permission" 对话。
