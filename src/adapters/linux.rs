//! Linux 实现:FileSystem(基于 absolute `file://` paths)+ ProcessExec。
//!
//! M1-P1 里 NetEgress Linux 实现先不做(需要 reqwest);M3 阶段加。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::core::cancel::CancelToken;

use super::fs::{Entry, FileSystem, FsErr, Meta, ReadOpts, Root, Uri, WriteOpts};
use super::proc_exec::{CmdSpec, ExecErr, ExecJobSnapshot, ExecJobState, ExitOut, ProcessExec};

// ================ LinuxFileSystem ================

#[derive(Clone)]
pub struct LinuxFileSystem {
    /// Host-visible workspace/default roots. These are advertised to the
    /// model and used as default shell cwd, but do not constrain absolute
    /// `file://` paths on desktop/server platforms.
    roots: Vec<(String, PathBuf, bool)>,
}

impl LinuxFileSystem {
    /// 用一组绝对路径作为 advertised workspace roots。路径会被
    /// canonicalize(resolve symlinks + 去 `..`),这样 `/var/folders/...`
    /// 和 `/private/var/folders/...` 这种 macOS symlink 等价写法展示一致。
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let roots = roots
            .into_iter()
            .map(|p| {
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                let id = canon.display().to_string();
                (id, canon, true)
            })
            .collect();
        Self { roots }
    }

    fn root_list(&self) -> Vec<Root> {
        self.roots
            .iter()
            .map(|(id, path, writable)| Root {
                id: id.clone(),
                uri_prefix: format!("file://{}/", path.display()),
                writable: *writable,
                description: format!("Linux directory {}", path.display()),
            })
            .collect()
    }

    fn resolve(&self, uri: &Uri) -> Result<PathBuf, FsErr> {
        if uri.has_dotdot_escape() {
            return Err(FsErr::EscapeOutsideRoot(uri.0.clone()));
        }
        let scheme = uri
            .scheme()
            .ok_or_else(|| FsErr::UnsupportedScheme(uri.0.clone()))?;
        if scheme != "file" {
            return Err(FsErr::UnsupportedScheme(scheme.into()));
        }

        // Extract the raw path after `file://`.
        let raw_path = uri
            .0
            .strip_prefix("file://")
            .ok_or_else(|| FsErr::UnsupportedScheme(uri.0.clone()))?;
        let path = PathBuf::from(raw_path);
        if !path.is_absolute() {
            return Err(FsErr::EscapeOutsideRoot(uri.0.clone()));
        }

        // Normalize the nearest existing ancestor so symlinks are resolved
        // before reads/writes. No configured-root boundary is enforced here;
        // OS permissions and host policy decide what can be accessed.
        Ok(canonicalize_existing_prefix(&path))
    }
}

#[async_trait]
impl FileSystem for LinuxFileSystem {
    fn roots(&self) -> Vec<Root> {
        self.root_list()
    }

    async fn stat(&self, uri: &Uri) -> Result<Meta, FsErr> {
        let path = self.resolve(uri)?;
        let md = tokio::fs::metadata(&path).await.map_err(io_err)?;
        Ok(Meta {
            size: md.len(),
            is_dir: md.is_dir(),
            mtime_ms: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        })
    }

    async fn read(&self, uri: &Uri, opts: ReadOpts) -> Result<Vec<u8>, FsErr> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        const ABS_CAP: usize = 16 * 1024 * 1024;

        let path = self.resolve(uri)?;
        let mut file = tokio::fs::File::open(&path).await.map_err(io_err)?;

        if let Some(off) = opts.offset {
            if off > 0 {
                file.seek(std::io::SeekFrom::Start(off))
                    .await
                    .map_err(io_err)?;
            }
        }

        let max = opts.max_bytes.unwrap_or(ABS_CAP).min(ABS_CAP);
        let mut buf = Vec::new();
        let _ = (&mut file)
            .take(max as u64)
            .read_to_end(&mut buf)
            .await
            .map_err(io_err)?;
        Ok(buf)
    }

    async fn write(&self, uri: &Uri, bytes: &[u8], opts: WriteOpts) -> Result<(), FsErr> {
        let path = self.resolve(uri)?;
        if opts.create_dirs {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
            }
        }
        if opts.append {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .await
                .map_err(io_err)?;
            f.write_all(bytes).await.map_err(io_err)?;
            f.flush().await.map_err(io_err)?;
        } else {
            tokio::fs::write(&path, bytes).await.map_err(io_err)?;
        }
        Ok(())
    }

    async fn list(&self, uri: &Uri) -> Result<Vec<Entry>, FsErr> {
        let path = self.resolve(uri)?;
        let mut rd = tokio::fs::read_dir(&path).await.map_err(io_err)?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(io_err)? {
            let ft = entry.file_type().await.map_err(io_err)?;
            let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            let child_uri = Uri::new(format!("file://{}", entry.path().display()));
            out.push(Entry {
                uri: child_uri,
                is_dir: ft.is_dir(),
                size,
            });
        }
        Ok(out)
    }

    async fn delete(&self, uri: &Uri) -> Result<(), FsErr> {
        let path = self.resolve(uri)?;
        let md = tokio::fs::metadata(&path).await.map_err(io_err)?;
        if md.is_dir() {
            tokio::fs::remove_dir(&path).await.map_err(io_err)
        } else {
            tokio::fs::remove_file(&path).await.map_err(io_err)
        }
    }

    async fn rename(&self, from: &Uri, to: &Uri) -> Result<(), FsErr> {
        let p_from = self.resolve(from)?;
        let p_to = self.resolve(to)?;
        tokio::fs::rename(&p_from, &p_to).await.map_err(io_err)
    }
}

fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }

    let mut cur = path;
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match cur.canonicalize() {
            Ok(mut base) => {
                for component in missing.iter().rev() {
                    base.push(component);
                }
                return base;
            }
            Err(_) => {
                let Some(name) = cur.file_name() else {
                    return path.to_path_buf();
                };
                missing.push(name.to_os_string());
                let Some(parent) = cur.parent() else {
                    return path.to_path_buf();
                };
                cur = parent;
            }
        }
    }
}

fn io_err(e: std::io::Error) -> FsErr {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => FsErr::NotFound(e.to_string()),
        ErrorKind::PermissionDenied => FsErr::PermissionDenied(e.to_string()),
        ErrorKind::DirectoryNotEmpty => FsErr::DirectoryNotEmpty(e.to_string()),
        _ => FsErr::Io(e.to_string()),
    }
}

// ================ LinuxProcessExec ================

#[derive(Clone)]
pub struct LinuxProcessExec {
    jobs: Arc<Mutex<HashMap<String, Arc<ManagedJob>>>>,
}

#[derive(Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

struct PipeChunk {
    kind: PipeKind,
    bytes: Vec<u8>,
}

struct ManagedJob {
    inner: Mutex<ManagedInner>,
}

struct ManagedInner {
    job_id: String,
    command: String,
    started: Instant,
    pid: Option<u32>,
    state: ExecJobState,
    code: Option<i32>,
    stdout_tail: Vec<u8>,
    stderr_tail: Vec<u8>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    output_truncated: bool,
    kill_requested: bool,
    error: Option<String>,
    tail_cap: usize,
    stdout_closed: bool,
    stderr_closed: bool,
}

impl LinuxProcessExec {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for LinuxProcessExec {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ProcessExec for LinuxProcessExec {
    fn available(&self) -> bool {
        true
    }

    async fn run(&self, spec: &CmdSpec, cancel: CancelToken) -> Result<ExitOut, ExecErr> {
        let mut cmd = Command::new(&spec.bin);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        if spec.stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }

        let mut child = cmd.spawn().map_err(|e| ExecErr::Io(e.to_string()))?;
        let child_pid = child.id();

        let (tx, mut rx) = mpsc::channel::<Result<PipeChunk, String>>(8);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(read_pipe(stdout, PipeKind::Stdout, tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(read_pipe(stderr, PipeKind::Stderr, tx.clone()));
        }
        drop(tx);

        // Feed stdin if provided
        if let Some(data) = &spec.stdin {
            if let Some(mut si) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                si.write_all(data)
                    .await
                    .map_err(|e| ExecErr::Io(e.to_string()))?;
            }
        }

        let timeout = spec.timeout.min(Duration::from_secs(3600));
        let timeout_sleep = tokio::time::sleep(timeout);
        tokio::pin!(timeout_sleep);
        let cancel_wait = async {
            loop {
                if cancel.triggered() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::pin!(cancel_wait);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut total_output = 0_u64;
        let mut pipes_closed = false;
        let exit_code = loop {
            tokio::select! {
                _ = &mut cancel_wait => {
                    kill_child_tree(child_pid, &mut child).await;
                    return Err(ExecErr::Killed);
                }
                _ = &mut timeout_sleep => {
                    kill_child_tree(child_pid, &mut child).await;
                    return Err(ExecErr::Timeout);
                }
                status = child.wait() => {
                    let status = status.map_err(|e| ExecErr::Io(e.to_string()))?;
                    break status.code().unwrap_or(-1);
                }
                msg = rx.recv(), if !pipes_closed => {
                    match msg {
                        Some(Ok(chunk)) => {
                            let dst = match chunk.kind {
                                PipeKind::Stdout => &mut stdout,
                                PipeKind::Stderr => &mut stderr,
                            };
                            if !append_capped(
                                dst,
                                &chunk.bytes,
                                &mut total_output,
                                spec.max_output_bytes,
                            ) {
                                kill_child_tree(child_pid, &mut child).await;
                                return Err(ExecErr::OutputTooLarge);
                            }
                        }
                        Some(Err(e)) => {
                            kill_child_tree(child_pid, &mut child).await;
                            return Err(ExecErr::Io(e));
                        }
                        None => pipes_closed = true,
                    }
                }
            }
        };

        if !pipes_closed {
            pipes_closed = drain_pipe_channel(
                &mut rx,
                &mut stdout,
                &mut stderr,
                &mut total_output,
                spec.max_output_bytes,
                pipe_drain_grace(),
            )
            .await?;
        }
        if !pipes_closed {
            if let Some(pid) = child_pid {
                kill_process_group(pid);
            }
            let _ = drain_pipe_channel(
                &mut rx,
                &mut stdout,
                &mut stderr,
                &mut total_output,
                spec.max_output_bytes,
                pipe_kill_drain_grace(),
            )
            .await?;
        }

        Ok(ExitOut {
            code: exit_code,
            stdout,
            stderr,
            truncated: false,
        })
    }

    async fn spawn(&self, spec: &CmdSpec) -> Result<ExecJobSnapshot, ExecErr> {
        let mut cmd = Command::new(&spec.bin);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        if spec.stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }

        let mut child = cmd.spawn().map_err(|e| ExecErr::Io(e.to_string()))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let stdin_data = spec.stdin.clone();
        let job_id = format!("sh_{}", uuid::Uuid::new_v4().simple());
        let tail_cap = spec.max_output_bytes.min(4 * 1024 * 1024) as usize;
        let job = Arc::new(ManagedJob {
            inner: Mutex::new(ManagedInner {
                job_id: job_id.clone(),
                command: render_command(spec),
                started: Instant::now(),
                pid,
                state: ExecJobState::Running,
                code: None,
                stdout_tail: Vec::new(),
                stderr_tail: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
                kill_requested: false,
                error: None,
                tail_cap,
                stdout_closed: stdout.is_none(),
                stderr_closed: stderr.is_none(),
            }),
        });

        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .insert(job_id, job.clone());

        if let Some(stdout) = stdout {
            tokio::spawn(read_managed_pipe(stdout, PipeKind::Stdout, job.clone()));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(read_managed_pipe(stderr, PipeKind::Stderr, job.clone()));
        }
        if let (Some(mut stdin), Some(data)) = (stdin, stdin_data) {
            let job_for_stdin = job.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(&data).await {
                    let mut inner = job_for_stdin.inner.lock().expect("job mutex poisoned");
                    if inner.error.is_none() {
                        inner.error = Some(format!("stdin write: {e}"));
                    }
                }
            });
        }

        tokio::spawn(wait_managed_child(job.clone(), child, spec.timeout));
        Ok(snapshot(&job))
    }

    async fn poll(&self, job_id: &str) -> Result<ExecJobSnapshot, ExecErr> {
        let job = self
            .jobs
            .lock()
            .expect("jobs mutex poisoned")
            .get(job_id)
            .cloned()
            .ok_or_else(|| ExecErr::Io(format!("unknown sh job `{job_id}`")))?;
        Ok(snapshot(&job))
    }

    async fn kill(&self, job_id: &str) -> Result<ExecJobSnapshot, ExecErr> {
        let job = self
            .jobs
            .lock()
            .expect("jobs mutex poisoned")
            .get(job_id)
            .cloned()
            .ok_or_else(|| ExecErr::Io(format!("unknown sh job `{job_id}`")))?;
        {
            let mut inner = job.inner.lock().expect("job mutex poisoned");
            if inner.state == ExecJobState::Running {
                inner.kill_requested = true;
                inner.state = ExecJobState::Killed;
                if let Some(pid) = inner.pid {
                    kill_process_group(pid);
                }
            }
        }
        wait_for_managed_pipe_drain(&job, pipe_drain_grace()).await;
        Ok(snapshot(&job))
    }

    async fn list_jobs(&self) -> Result<Vec<ExecJobSnapshot>, ExecErr> {
        let jobs = self
            .jobs
            .lock()
            .expect("jobs mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        // The closure can't be elided: `jobs.iter()` yields `&Arc<ManagedJob>`
        // but `snapshot` takes `&ManagedJob`. The closure makes the auto-deref
        // through `Arc::Deref` explicit at the call site.
        #[allow(clippy::redundant_closure)]
        let mut snapshots = jobs.iter().map(|job| snapshot(job)).collect::<Vec<_>>();
        snapshots.sort_by_key(|s| std::cmp::Reverse(s.elapsed));
        Ok(snapshots)
    }
}

async fn read_pipe<R>(mut reader: R, kind: PipeKind, tx: mpsc::Sender<Result<PipeChunk, String>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buf = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                let chunk = PipeChunk {
                    kind,
                    bytes: buf[..n].to_vec(),
                };
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string())).await;
                return;
            }
        }
    }
}

async fn read_managed_pipe<R>(mut reader: R, kind: PipeKind, job: Arc<ManagedJob>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buf = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                mark_managed_pipe_closed(&job, kind);
                return;
            }
            Ok(n) => {
                let mut inner = job.inner.lock().expect("job mutex poisoned");
                let tail_cap = inner.tail_cap;
                let bytes = &buf[..n];
                match kind {
                    PipeKind::Stdout => {
                        inner.stdout_bytes = inner.stdout_bytes.saturating_add(bytes.len() as u64);
                        append_tail(&mut inner.stdout_tail, bytes, tail_cap);
                        if inner.stdout_tail.len() >= tail_cap
                            && inner.stdout_bytes as usize > tail_cap
                        {
                            inner.output_truncated = true;
                        }
                    }
                    PipeKind::Stderr => {
                        inner.stderr_bytes = inner.stderr_bytes.saturating_add(bytes.len() as u64);
                        append_tail(&mut inner.stderr_tail, bytes, tail_cap);
                        if inner.stderr_tail.len() >= tail_cap
                            && inner.stderr_bytes as usize > tail_cap
                        {
                            inner.output_truncated = true;
                        }
                    }
                }
            }
            Err(e) => {
                let mut inner = job.inner.lock().expect("job mutex poisoned");
                if inner.error.is_none() {
                    inner.error = Some(e.to_string());
                }
                match kind {
                    PipeKind::Stdout => inner.stdout_closed = true,
                    PipeKind::Stderr => inner.stderr_closed = true,
                }
                if inner.state == ExecJobState::Running {
                    inner.state = ExecJobState::Error;
                }
                return;
            }
        }
    }
}

async fn wait_managed_child(
    job: Arc<ManagedJob>,
    mut child: tokio::process::Child,
    timeout: Duration,
) {
    let child_pid = child.id();
    let timeout_sleep = tokio::time::sleep(timeout);
    tokio::pin!(timeout_sleep);
    let kill_poll = tokio::time::sleep(Duration::from_millis(100));
    tokio::pin!(kill_poll);
    let mut timeout_fired = false;
    let mut forced_state: Option<ExecJobState> = None;

    loop {
        tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                let pipes_closed = wait_for_managed_pipe_drain(&job, pipe_drain_grace()).await;
                if !pipes_closed {
                    if let Some(pid) = child_pid {
                        kill_process_group(pid);
                    }
                    wait_for_managed_pipe_drain(&job, pipe_kill_drain_grace()).await;
                }
                let mut inner = job.inner.lock().expect("job mutex poisoned");
                inner.code = Some(code);
                if inner.state == ExecJobState::Running {
                    inner.state = forced_state.unwrap_or(ExecJobState::Exited);
                }
                return;
            }
            _ = &mut timeout_sleep, if !timeout_fired => {
                timeout_fired = true;
                forced_state = Some(ExecJobState::TimedOut);
                kill_child_tree(child_pid, &mut child).await;
            }
            _ = &mut kill_poll => {
                let should_kill = {
                    let inner = job.inner.lock().expect("job mutex poisoned");
                    inner.kill_requested
                };
                if should_kill {
                    forced_state.get_or_insert(ExecJobState::Killed);
                    kill_child_tree(child_pid, &mut child).await;
                }
                kill_poll.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(100));
            }
        }
    }
}

fn snapshot(job: &ManagedJob) -> ExecJobSnapshot {
    let inner = job.inner.lock().expect("job mutex poisoned");
    ExecJobSnapshot {
        job_id: inner.job_id.clone(),
        state: inner.state.clone(),
        code: inner.code,
        stdout_tail: inner.stdout_tail.clone(),
        stderr_tail: inner.stderr_tail.clone(),
        stdout_bytes: inner.stdout_bytes,
        stderr_bytes: inner.stderr_bytes,
        output_truncated: inner.output_truncated,
        elapsed: inner.started.elapsed(),
        command: inner.command.clone(),
        error: inner.error.clone(),
    }
}

fn append_tail(dst: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    if cap == 0 {
        dst.clear();
        return;
    }
    dst.extend_from_slice(bytes);
    if dst.len() > cap {
        let drop = dst.len() - cap;
        dst.drain(..drop);
    }
}

async fn drain_pipe_channel(
    rx: &mut mpsc::Receiver<Result<PipeChunk, String>>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    total_output: &mut u64,
    max_output_bytes: u64,
    grace: Duration,
) -> Result<bool, ExecErr> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Ok(chunk))) => {
                let ok = match chunk.kind {
                    PipeKind::Stdout => {
                        append_capped(stdout, &chunk.bytes, total_output, max_output_bytes)
                    }
                    PipeKind::Stderr => {
                        append_capped(stderr, &chunk.bytes, total_output, max_output_bytes)
                    }
                };
                if !ok {
                    return Err(ExecErr::OutputTooLarge);
                }
            }
            Ok(Some(Err(e))) => return Err(ExecErr::Io(e)),
            Ok(None) => return Ok(true),
            Err(_) => return Ok(false),
        }
    }
}

fn mark_managed_pipe_closed(job: &ManagedJob, kind: PipeKind) {
    let mut inner = job.inner.lock().expect("job mutex poisoned");
    match kind {
        PipeKind::Stdout => inner.stdout_closed = true,
        PipeKind::Stderr => inner.stderr_closed = true,
    }
}

async fn wait_for_managed_pipe_drain(job: &ManagedJob, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        {
            let inner = job.inner.lock().expect("job mutex poisoned");
            if inner.stdout_closed && inner.stderr_closed {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn pipe_drain_grace() -> Duration {
    Duration::from_millis(200)
}

fn pipe_kill_drain_grace() -> Duration {
    Duration::from_millis(100)
}

fn render_command(spec: &CmdSpec) -> String {
    if is_shell_bin(&spec.bin) {
        if let Some(summary) = spec.stdin.as_deref().and_then(shell_stdin_summary) {
            return summary;
        }
    }
    if spec.args.is_empty() {
        spec.bin.clone()
    } else {
        format!("{} {}", spec.bin, spec.args.join(" "))
    }
}

fn is_shell_bin(bin: &str) -> bool {
    bin.rsplit('/')
        .next()
        .is_some_and(|name| matches!(name, "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh"))
}

fn shell_stdin_summary(stdin: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdin);
    let commands = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let first = commands.first().copied()?;
    let mut summary = one_line(first, 90);
    if commands.len() > 1 {
        summary.push_str(" ...");
    }
    Some(summary)
}

fn one_line(s: &str, max: usize) -> String {
    let compact = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max {
        compact
    } else {
        let keep = max.saturating_sub(3);
        format!("{}...", compact.chars().take(keep).collect::<String>())
    }
}

async fn kill_child_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
    let _ = child.kill().await;
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn append_capped(dst: &mut Vec<u8>, bytes: &[u8], total: &mut u64, cap: u64) -> bool {
    let next = total.saturating_add(bytes.len() as u64);
    if next > cap {
        let remaining = cap.saturating_sub(*total) as usize;
        if remaining > 0 {
            dst.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
        }
        *total = cap;
        false
    } else {
        dst.extend_from_slice(bytes);
        *total = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::render_command;
    use crate::adapters::CmdSpec;

    #[test]
    fn render_command_summarizes_shell_stdin() {
        let mut spec = CmdSpec::new("bash", Vec::new());
        spec.stdin = Some(b"# comment\nfind src -type f\nwc -l src/lib.rs\n".to_vec());
        spec.timeout = Duration::from_secs(30);

        assert_eq!(render_command(&spec), "find src -type f ...");
    }
}
