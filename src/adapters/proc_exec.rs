//! ProcessExec adapter trait(shell 命令执行)。

use std::time::Duration;

use async_trait::async_trait;

use crate::core::cancel::CancelToken;

#[derive(Clone, Debug)]
pub struct CmdSpec {
    pub bin: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub max_output_bytes: u64,
}

impl CmdSpec {
    pub fn new(bin: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            bin: bin.into(),
            args,
            cwd: None,
            env: vec![],
            stdin: None,
            timeout: Duration::from_secs(10),
            max_output_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExitOut {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecJobState {
    Running,
    Exited,
    TimedOut,
    Killed,
    Error,
}

#[derive(Clone, Debug)]
pub struct ExecJobSnapshot {
    pub job_id: String,
    pub state: ExecJobState,
    pub code: Option<i32>,
    pub stdout_tail: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub output_truncated: bool,
    pub elapsed: Duration,
    pub command: String,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecErr {
    #[error("not available on this platform")]
    NotAvailable,

    #[error("timeout")]
    Timeout,

    #[error("killed")]
    Killed,

    #[error("io: {0}")]
    Io(String),

    #[error("output too large")]
    OutputTooLarge,
}

#[async_trait]
pub trait ProcessExec: Send + Sync {
    /// `true` = 本平台支持 shell 执行(Linux/macOS yes;iOS/MCU no)。
    fn available(&self) -> bool;

    async fn run(&self, spec: &CmdSpec, cancel: CancelToken) -> Result<ExitOut, ExecErr>;

    async fn spawn(&self, _spec: &CmdSpec) -> Result<ExecJobSnapshot, ExecErr> {
        Err(ExecErr::NotAvailable)
    }

    async fn poll(&self, _job_id: &str) -> Result<ExecJobSnapshot, ExecErr> {
        Err(ExecErr::NotAvailable)
    }

    async fn kill(&self, _job_id: &str) -> Result<ExecJobSnapshot, ExecErr> {
        Err(ExecErr::NotAvailable)
    }

    async fn list_jobs(&self) -> Result<Vec<ExecJobSnapshot>, ExecErr> {
        Ok(Vec::new())
    }
}
