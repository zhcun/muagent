//! Stdio transport for MCP.
//!
//! Line-delimited JSON: each JSON-RPC message is one line on stdin/stdout.
//! (The official MCP spec also supports this framing; Content-Length framing
//! is only mandatory for LSP-style servers, and most MCP servers in the
//! wild accept newline framing over stdio.)

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::core::cancel::CancelToken;

use super::client::{McpClientError, Transport};

/// How to launch an MCP server child process.
#[derive(Clone, Debug)]
pub struct StdioSpawn {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<String>,
}

impl StdioSpawn {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: vec![],
            env: vec![],
            working_dir: None,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn cwd(mut self, d: impl Into<String>) -> Self {
        self.working_dir = Some(d.into());
        self
    }
}

pub struct StdioTransport {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

const CANCEL_POLL: Duration = Duration::from_millis(50);
const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

impl StdioTransport {
    pub async fn spawn(spec: &StdioSpawn) -> Result<Self, McpClientError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(d) = &spec.working_dir {
            cmd.current_dir(d);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpClientError::Transport(format!("spawn {}: {e}", spec.program)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::Transport("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::Transport("no stdout".into()))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            })),
        })
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn send(&self, msg: &Value, cancel: CancelToken) -> Result<(), McpClientError> {
        if cancel.triggered() {
            return Err(McpClientError::Transport("cancelled".into()));
        }
        let mut g = self.inner.lock().await;
        let mut line = serde_json::to_vec(msg)
            .map_err(|e| McpClientError::Transport(format!("serialize: {e}")))?;
        line.push(b'\n');
        write_all_with_cancel(&mut g.stdin, &line, cancel.child()).await?;
        flush_with_cancel(&mut g.stdin, cancel).await?;
        Ok(())
    }

    async fn recv(&self, cancel: CancelToken) -> Result<Value, McpClientError> {
        let mut g = self.inner.lock().await;
        let mut buf = String::new();
        let n = read_line_with_cancel(&mut g.stdout, &mut buf, cancel).await?;
        if n == 0 {
            return Err(McpClientError::Transport("server closed stdout".into()));
        }
        let v: Value = serde_json::from_str(buf.trim())
            .map_err(|e| McpClientError::Transport(format!("parse json: {e}; line={buf:?}")))?;
        Ok(v)
    }
}

async fn write_all_with_cancel(
    stdin: &mut ChildStdin,
    buf: &[u8],
    cancel: CancelToken,
) -> Result<(), McpClientError> {
    let mut written = 0;
    while written < buf.len() {
        if cancel.triggered() {
            return Err(McpClientError::Transport("cancelled".into()));
        }
        match tokio::time::timeout(CANCEL_POLL, stdin.write(&buf[written..])).await {
            Ok(Ok(0)) => {
                return Err(McpClientError::Transport(
                    "write: child stdin closed before message was sent".into(),
                ));
            }
            Ok(Ok(n)) => written += n,
            Ok(Err(e)) => return Err(McpClientError::Transport(format!("write: {e}"))),
            Err(_) => continue,
        }
    }
    Ok(())
}

async fn flush_with_cancel(
    stdin: &mut ChildStdin,
    cancel: CancelToken,
) -> Result<(), McpClientError> {
    loop {
        if cancel.triggered() {
            return Err(McpClientError::Transport("cancelled".into()));
        }
        match tokio::time::timeout(CANCEL_POLL, stdin.flush()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => return Err(McpClientError::Transport(format!("flush: {e}"))),
            Err(_) => continue,
        }
    }
}

async fn read_line_with_cancel(
    stdout: &mut BufReader<ChildStdout>,
    buf: &mut String,
    cancel: CancelToken,
) -> Result<usize, McpClientError> {
    let mut bytes = Vec::new();
    loop {
        if cancel.triggered() {
            return Err(McpClientError::Transport("cancelled".into()));
        }
        match tokio::time::timeout(CANCEL_POLL, read_line_chunk(stdout, &mut bytes)).await {
            Ok(Ok(LineState::Pending)) => {
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(McpClientError::Transport(format!(
                        "frame too large: over {MAX_FRAME_BYTES} bytes"
                    )));
                }
            }
            Ok(Ok(LineState::Done)) => {
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(McpClientError::Transport(format!(
                        "frame too large: over {MAX_FRAME_BYTES} bytes"
                    )));
                }
                let n = bytes.len();
                *buf = String::from_utf8(bytes)
                    .map_err(|e| McpClientError::Transport(format!("read: invalid utf-8: {e}")))?;
                return Ok(n);
            }
            Ok(Err(e)) => return Err(McpClientError::Transport(format!("read: {e}"))),
            Err(_) => continue,
        }
    }
}

enum LineState {
    Pending,
    Done,
}

async fn read_line_chunk<R>(reader: &mut R, bytes: &mut Vec<u8>) -> std::io::Result<LineState>
where
    R: AsyncBufRead + Unpin,
{
    let available = reader.fill_buf().await?;
    if available.is_empty() {
        return Ok(LineState::Done);
    }

    let (take, done) = match available.iter().position(|&b| b == b'\n') {
        Some(pos) => (pos + 1, true),
        None => (available.len(), false),
    };
    bytes.extend_from_slice(&available[..take]);
    reader.consume(take);
    Ok(if done {
        LineState::Done
    } else {
        LineState::Pending
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::{sleep, timeout};

    use super::{StdioSpawn, StdioTransport};
    use crate::capabilities::mcp::client::{McpClientError, Transport};
    use crate::core::cancel::CancelToken;

    #[tokio::test]
    async fn recv_returns_cancelled_while_child_is_still_running() {
        let spec = StdioSpawn::new("sh").arg("-c").arg("sleep 10");
        let transport = StdioTransport::spawn(&spec).await.unwrap();
        let cancel = CancelToken::new();
        let trigger = cancel.child();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            trigger.trigger();
        });

        let err = timeout(Duration::from_secs(1), transport.recv(cancel))
            .await
            .expect("recv should stop once cancelled")
            .expect_err("recv should return cancelled");
        assert!(matches!(err, McpClientError::Transport(message) if message.contains("cancelled")));
    }

    #[tokio::test]
    async fn recv_rejects_oversized_frame() {
        let spec = StdioSpawn::new("sh")
            .arg("-c")
            .arg("head -c 11000000 /dev/zero");
        let transport = StdioTransport::spawn(&spec).await.unwrap();

        let err = timeout(Duration::from_secs(2), transport.recv(CancelToken::never()))
            .await
            .expect("recv should fail once the frame exceeds its cap")
            .expect_err("recv should reject the frame");
        assert!(
            matches!(err, McpClientError::Transport(message) if message.contains("frame too large"))
        );
    }
}
