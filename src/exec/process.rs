//! Bounded subprocess I/O and process-group ownership on Linux/macOS.
use anyhow::{bail, Context, Result};
use rustix::process::{kill_process, kill_process_group, Pid, Signal};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::oneshot,
};

pub const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
pub const DIAGNOSTIC_LIMIT: usize = 500;
const TOKEN_PREFIXES: [&str; 4] = ["xoxp-", "xoxb-", "xapp-", "xoxe-"];

/// Contains private environment values: deliberately has no Debug implementation.
#[derive(Clone)]
pub struct Launch {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
}
/// Scrub after applying overrides too, so a resource/adapter override cannot
/// accidentally restore a daemon token. Non-Unicode owner environment survives.
pub fn scrubbed_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    excluded: &[String],
    extra: &BTreeMap<String, String>,
) -> BTreeMap<OsString, OsString> {
    let mut environment: BTreeMap<_, _> = inherited.into_iter().collect();
    environment.extend(extra.iter().map(|(k, v)| (k.into(), v.into())));
    environment.retain(|key, value| {
        let name = key.to_string_lossy();
        !excluded.iter().any(|k| key == k.as_str())
            && !name.to_ascii_uppercase().contains("SLACK")
            && !name.to_ascii_uppercase().starts_with("FRIDICA_")
            && !TOKEN_PREFIXES
                .iter()
                .any(|prefix| value.to_string_lossy().starts_with(prefix))
    });
    environment
}

/// Redact before truncation, including tokens not supplied in the secret list.
/// This is the only helper intended for local log diagnostics; raw output is not.
pub fn diagnostic(bytes: &[u8], limit: usize, secrets: &[String]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let mut secrets: Vec<_> = secrets.iter().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        text = text.replace(secret, "[redacted]");
    }
    for prefix in TOKEN_PREFIXES {
        while let Some(start) = text.find(prefix) {
            let end = text[start..]
                .find(|c: char| c.is_whitespace() || ['\'', '"', '<', '>'].contains(&c))
                .map_or(text.len(), |i| start + i);
            text.replace_range(start..end, "[redacted]");
        }
    }
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let skip = line.chars().count().saturating_sub(limit);
    line.chars().skip(skip).collect()
}

/// Owns the entire process group, including children that outlive the leader.
/// Explicit terminate waits for the leader to be reaped. Drop is the synchronous
/// last resort: group SIGKILL plus Tokio's best-effort child reaper.
pub struct Process {
    child: Child,
    group: Pid,
    terminated: bool,
}
impl Process {
    pub fn start(launch: &Launch) -> Result<Self> {
        let Some(program) = launch.argv.first().filter(|s| !s.is_empty()) else {
            bail!("empty process command");
        };
        let mut command = Command::new(program);
        command
            .args(&launch.argv[1..])
            .env_clear()
            .envs(&launch.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = &launch.cwd {
            command.current_dir(cwd);
        }
        // Do not include arguments, environment values or OS-supplied path text.
        let child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("process start failed ({:?})", e.kind()))?;
        let group = Pid::from_raw(child.id().context("missing process id")? as i32)
            .context("invalid process id")?;
        Ok(Self {
            child,
            group,
            terminated: false,
        })
    }
    pub fn id(&self) -> u32 {
        self.group.as_raw_nonzero().get() as u32
    }
    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.finish_group()?;
        }
        Ok(status)
    }
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        let status = self.wait_child().await?;
        self.finish_group()?;
        Ok(status)
    }
    async fn wait_child(&mut self) -> Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            // Also recheck periodically: embedded runtimes can miss/coalesce a
            // SIGCHLD wakeup. A missed wakeup must not turn a completed command
            // into a timeout or leave descendants alive until the next signal.
            tokio::select! {
                status = self.child.wait() => return Ok(status?),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
        }
    }
    /// stdout EOF is not proof that a process exited. None means status remains
    /// unknown after a bounded wait, never an invented successful exit code.
    pub async fn eof_status(&mut self, grace: Duration) -> Result<Option<ExitStatus>> {
        match tokio::time::timeout(grace, self.wait()).await {
            Ok(status) => Ok(Some(status?)),
            Err(_) => Ok(None),
        }
    }
    fn finish_group(&mut self) -> Result<()> {
        if !self.terminated {
            // Disarm immediately on observing exit: never retain a reaped PID
            // as a signal target for later idle cleanup (it could be reused).
            self.signal(Signal::KILL)?;
            self.terminated = true;
        }
        Ok(())
    }
    fn signal(&self, signal: Signal) -> Result<()> {
        signal_group(self.group, signal, kill_process_group, kill_process)
    }
    pub async fn terminate(&mut self, grace: Duration) -> Result<()> {
        if self.terminated {
            self.child.stdin.take();
            self.child.stdout.take();
            self.child.stderr.take();
            return Ok(());
        }
        self.child.stdin.take();
        self.signal(Signal::TERM)?;
        let _ = tokio::time::timeout(grace, self.wait_child()).await;
        // Even an exited leader can have live descendants holding our pipes.
        self.signal(Signal::KILL)?;
        tokio::time::timeout(grace.max(Duration::from_millis(100)), self.wait_child())
            .await
            .context("process termination remains unconfirmed")??;
        self.child.stdout.take();
        self.child.stderr.take();
        self.terminated = true;
        Ok(())
    }
}

fn signal_group(
    group: Pid,
    signal: Signal,
    group_signal: impl FnOnce(Pid, Signal) -> rustix::io::Result<()>,
    leader_signal: impl FnOnce(Pid, Signal) -> rustix::io::Result<()>,
) -> Result<()> {
    match group_signal(group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(rustix::io::Errno::PERM) => match leader_signal(group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e.into()),
        },
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
#[path = "../../tests/support/process_signals.rs"]
mod signal_tests;
impl Drop for Process {
    fn drop(&mut self) {
        if !self.terminated {
            let _ = self.signal(Signal::KILL);
        }
    }
}
pub struct Completed {
    pub returncode: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl Completed {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}
pub fn returncode(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(1))
}
async fn read_limited(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0; 65536];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        if count > limit.saturating_sub(output.len()) {
            bail!("process output exceeded the size limit");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

/// The owner task survives caller cancellation long enough to terminate and reap.
/// Dropping the caller closes the result channel and requests that cleanup.
pub async fn run_once(
    launch: Launch,
    input: Vec<u8>,
    timeout: Duration,
    limit: usize,
) -> Result<Completed> {
    run_command(launch, input, timeout, limit, false, None).await
}

/// Keep a process permit in the cleanup owner task, including after the caller
/// is cancelled. Queued work cannot reuse capacity while termination is pending.
pub async fn run_once_with_permit(
    launch: Launch,
    input: Vec<u8>,
    timeout: Duration,
    limit: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Completed> {
    run_command(launch, input, timeout, limit, false, Some(permit)).await
}

/// Target-host helpers watch channel EOF for crash/disconnect cleanup. Keep the
/// pipe open while draining output; the owner closes it on cancellation or exit.
pub async fn run_with_open_stdin(
    launch: Launch,
    timeout: Duration,
    limit: usize,
) -> Result<Completed> {
    run_command(launch, vec![], timeout, limit, true, None).await
}

/// Open-stdin variant retaining capacity until cancellation cleanup completes.
pub async fn run_with_open_stdin_with_permit(
    launch: Launch,
    timeout: Duration,
    limit: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Completed> {
    run_command(launch, vec![], timeout, limit, true, Some(permit)).await
}

async fn run_command(
    launch: Launch,
    input: Vec<u8>,
    timeout: Duration,
    limit: usize,
    keep_stdin: bool,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Completed> {
    if timeout.is_zero() {
        bail!("process deadline must be positive");
    }
    let mut process = Process::start(&launch)?;
    let (mut sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let _permit = permit;
        let mut stdin = process.stdin().unwrap();
        let stdout = process.stdout().unwrap();
        let stderr = process.stderr().unwrap();
        let result = {
            let communicate = async {
                let write = async {
                    // BrokenPipe is an ordinary early process exit, whose status
                    // and stderr still need to be collected.
                    if let Err(e) = stdin.write_all(&input).await {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            return Err(e.into());
                        }
                    }
                    Ok::<_, anyhow::Error>(keep_stdin.then_some(stdin))
                };
                let (_, stdout, stderr, status) = tokio::try_join!(
                    write,
                    read_limited(stdout, limit),
                    read_limited(stderr, limit),
                    process.wait()
                )?;
                Ok(Completed {
                    returncode: returncode(status),
                    stdout,
                    stderr,
                })
            };
            tokio::select! {biased;
                _ = sender.closed() => Err(anyhow::anyhow!("process caller cancelled")),
                result = tokio::time::timeout(timeout, communicate) => result.unwrap_or_else(|_|Err(anyhow::anyhow!("process timed out"))),
            }
        };
        let cleanup = process.terminate(Duration::from_secs(1)).await;
        let result = match cleanup {
            Ok(()) => result,
            Err(e) => Err(e),
        };
        let _ = sender.send(result);
    });
    receiver.await.context("process owner task stopped")?
}
