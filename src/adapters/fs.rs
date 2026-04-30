//! `FileSystem` adapter trait + URI / Root / errors。
//!
//! URI 语法:`<scheme>://[<root-id>]/<path>`。例:
//! - `sandbox://notes/a.md`(app 沙盒内)
//! - `file:///Users/mike/foo.txt`(Linux/Mac 绝对路径)
//! - `bookmark://<UUID>/Downloads/x.pdf`(iOS 用户授权的根)
//!
//! 不允许 `..` 逃逸;所有 uri 必须落在 `roots()` 返回的某个 Root 下。

use std::path::PathBuf;

use async_trait::async_trait;

// ============ URI / Root ============

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uri(pub String);

impl Uri {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> Option<&str> {
        self.0.split("://").next().filter(|s| !s.is_empty())
    }

    /// 简易校验:禁 `..` 段。
    pub fn has_dotdot_escape(&self) -> bool {
        let after = self.0.split("://").nth(1).unwrap_or("");
        after.split('/').any(|seg| seg == "..")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: String,
    pub uri_prefix: String,
    pub writable: bool,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub size: u64,
    pub is_dir: bool,
    pub mtime_ms: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ReadOpts {
    /// Start reading at this byte offset. 0 = from the beginning.
    pub offset: Option<u64>,
    /// Return at most this many bytes. When the file has more data beyond
    /// `offset + max_bytes`, the adapter **truncates silently**; the caller
    /// is expected to detect truncation by comparing returned length against
    /// the file's `stat().size` and reading more with a larger offset.
    /// Absolute adapter-level cap is 16 MiB regardless of what's passed.
    pub max_bytes: Option<usize>,
}

impl ReadOpts {
    pub fn max(n: usize) -> Self {
        Self {
            max_bytes: Some(n),
            offset: None,
        }
    }
    pub fn range(offset: u64, max_bytes: usize) -> Self {
        Self {
            offset: Some(offset),
            max_bytes: Some(max_bytes),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct WriteOpts {
    pub append: bool,
    pub create_dirs: bool,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub uri: Uri,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FsErr {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("permission denied on root '{0}'")]
    PermissionDenied(String),

    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),

    #[error("unsupported scheme: {0}")]
    UnsupportedScheme(String),

    #[error("escape outside root: {0}")]
    EscapeOutsideRoot(String),

    #[error("stale bookmark: {0}")]
    StaleBookmark(String),

    #[error("io: {0}")]
    Io(String),

    #[error("too large")]
    TooLarge,

    #[error("not supported")]
    NotSupported,
}

// ============ Trait ============

#[async_trait]
pub trait FileSystem: Send + Sync {
    /// 当前设备可读写的根集合。
    fn roots(&self) -> Vec<Root>;

    async fn stat(&self, uri: &Uri) -> Result<Meta, FsErr>;

    async fn read(&self, uri: &Uri, opts: ReadOpts) -> Result<Vec<u8>, FsErr>;

    async fn write(&self, uri: &Uri, bytes: &[u8], opts: WriteOpts) -> Result<(), FsErr>;

    async fn list(&self, uri: &Uri) -> Result<Vec<Entry>, FsErr>;

    async fn delete(&self, uri: &Uri) -> Result<(), FsErr>;

    async fn rename(&self, from: &Uri, to: &Uri) -> Result<(), FsErr>;

    /// 请求新 root 访问(iOS DocumentPicker / Android SAF;Linux 一般不支持)。
    async fn request_root_access(&self, _purpose: &str) -> Result<Root, FsErr> {
        Err(FsErr::NotSupported)
    }
}

// ============ 工具函数 ============

/// 给定 uri,找对应 Root(若存在)并返回相对路径。
pub fn resolve_within_roots<'a>(uri: &Uri, roots: &'a [Root]) -> Result<(&'a Root, String), FsErr> {
    if uri.has_dotdot_escape() {
        return Err(FsErr::EscapeOutsideRoot(uri.0.clone()));
    }
    for r in roots {
        if let Some(rel) = uri.0.strip_prefix(&r.uri_prefix) {
            return Ok((r, rel.to_string()));
        }
    }
    Err(FsErr::EscapeOutsideRoot(uri.0.clone()))
}

/// 把 scheme+root 解析为 host 绝对路径(Linux 实现会用)。
pub fn uri_to_abs_path(uri: &Uri, root: &Root, root_abs: &std::path::Path) -> PathBuf {
    let rel = uri.0.strip_prefix(&root.uri_prefix).unwrap_or("");
    root_abs.join(rel.trim_start_matches('/'))
}
