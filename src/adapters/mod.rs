//! Adapter 层(Shell 实现细节)。Core 不感知这些。
//!
//! `AdapterBundle` 在 Runner 构造后冻结(v3.1 设计约束)。
//! Tool 实现通过持有 `Arc<AdapterBundle>` 访问需要的 adapter。

pub mod bundle;
pub mod fs;
pub mod proc_exec;
pub mod reqwest;

#[cfg(unix)]
pub mod linux;

pub use bundle::AdapterBundle;
pub use fs::{Entry, FileSystem, FsErr, Meta, ReadOpts, Root, Uri, WriteOpts};
pub use proc_exec::{CmdSpec, ExecErr, ExecJobSnapshot, ExecJobState, ExitOut, ProcessExec};
pub use reqwest::ReqwestEgress;

// NetEgress 现在住在 core::net,这里重新导出仅为 ergonomic 访问。
pub use crate::core::net::{
    check_model_status, net_err_to_model, HttpMethod, HttpReq, HttpResp, NetEgress, NetErr,
};
