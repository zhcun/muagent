//! 默认内置 tools。
//!
//! 这些 tool 持有 `Arc<AdapterBundle>`,运行时从 bundle 读 adapter。
//! 当 host 没提供对应 adapter(无 proc / 无 net)时,工具自动跳过注册。

pub mod fs_delete;
pub mod fs_edit;
pub mod fs_list;
pub mod fs_read;
pub mod fs_rename;
pub mod fs_stat;
pub mod fs_write;
pub mod sh_exec;

use std::sync::Arc;

use crate::core::prelude::{CapabilityRegistry, ToolErr};

use crate::adapters::AdapterBundle;

/// FsErr → ToolErr 映射(fs_* 工具共用)。
pub(crate) fn map_fs_err(e: crate::adapters::FsErr) -> ToolErr {
    use crate::adapters::FsErr::*;
    match e {
        NotFound(s) => ToolErr {
            msg: format!("not found: {s}"),
            retryable: true,
            hint: Some("call fs_list(parent) to check available paths".into()),
        },
        PermissionDenied(s) => ToolErr {
            msg: format!("permission denied: {s}"),
            retryable: false,
            hint: Some("root may be read-only; try a different root".into()),
        },
        DirectoryNotEmpty(s) => ToolErr {
            msg: format!("directory not empty: {s}"),
            retryable: false,
            hint: Some(
                "fs_delete only removes empty directories; list and remove contents first".into(),
            ),
        },
        UnsupportedScheme(s) => ToolErr {
            msg: format!("unsupported scheme: {s}"),
            retryable: false,
            hint: Some("only file:// is supported".into()),
        },
        EscapeOutsideRoot(s) => ToolErr {
            msg: format!("outside all roots: {s}"),
            retryable: false,
            hint: Some("paths must be within allowed roots".into()),
        },
        StaleBookmark(s) => ToolErr {
            msg: format!("stale bookmark: {s}"),
            retryable: false,
            hint: Some("request root access again".into()),
        },
        TooLarge => ToolErr::retry("too large").with_hint("pass smaller max_bytes"),
        Io(s) => ToolErr::retry(s),
        NotSupported => ToolErr::deny("operation not supported on this platform"),
    }
}

/// Register the default built-in tools that are broadly safe and universally useful.
///
/// - `fs_*` (read / edit / write / list / stat / delete / rename): always registered
/// - `sh_exec`: only if `bundle.proc` is provided
pub fn register_defaults(registry: &CapabilityRegistry, bundle: Arc<AdapterBundle>) {
    registry.register(Arc::new(fs_edit::FsEdit::new(bundle.clone())));
    registry.register(Arc::new(fs_read::FsRead::new(bundle.clone())));
    registry.register(Arc::new(fs_write::FsWrite::new(bundle.clone())));
    registry.register(Arc::new(fs_list::FsList::new(bundle.clone())));
    registry.register(Arc::new(fs_stat::FsStat::new(bundle.clone())));
    registry.register(Arc::new(fs_delete::FsDelete::new(bundle.clone())));
    registry.register(Arc::new(fs_rename::FsRename::new(bundle.clone())));
    if bundle.proc.is_some() {
        registry.register(Arc::new(sh_exec::ShExec::new(bundle.clone())));
    }
}
