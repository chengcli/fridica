//! Keep the owner grant outside the model process. Only MCP bytes cross the job socket.
use anyhow::{anyhow, ensure, Result};
use rustix::{
    fd::AsFd,
    fs::{fcntl_getfl, fcntl_setfl, OFlags},
    process::{getppid, kill_process, Pid, Signal},
};
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(20);
const LIMIT: usize = 64 * 1024;
const READY: &[u8] = b"fridica-overleaf-broker:ready\n";

pub fn run(
    workspace: &Path,
    worker: &str,
    channel: &str,
    project: &str,
    command: &[String],
) -> Result<()> {
    ensure!(
        !command.is_empty() && !command[0].is_empty(),
        "native broker backend command missing"
    );
    let raw = std::env::var(crate::config::overleaf::NATIVE_ENV)
        .map_err(|_| anyhow!("scoped native session missing"))?;
    crate::config::overleaf::task_session(&raw, worker, channel, project)?;
    let raw = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|_| anyhow!("invalid scoped native session"))?
        .to_string();
    let workspace = workspace_root(workspace)?;
    let directory = tempfile::Builder::new()
        .prefix(".ol-")
        .tempdir_in(&workspace)
        .map_err(|_| anyhow!("cannot create private native broker directory"))?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| anyhow!("cannot protect native broker directory"))?;
    let socket = directory.path().join("s");
    let mut process = Command::new(std::env::current_exe()?)
        .args(["overleaf-broker-serve", "--socket"])
        .arg(&socket)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--worker",
            worker,
            "--channel",
            channel,
            "--project",
            project,
        ])
        .env_remove(crate::config::overleaf::NATIVE_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| anyhow!("cannot start native broker"))?;
    let mut input = process
        .stdin
        .take()
        .ok_or_else(|| anyhow!("native broker input missing"))?;
    let mut output = process
        .stdout
        .take()
        .ok_or_else(|| anyhow!("native broker readiness missing"))?;
    let mut broker = ChildGuard {
        child: process,
        stopped: false,
    };
    nonblocking(&input)?;
    nonblocking(&output)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut header: VecDeque<_> = format!("{raw}\n").into_bytes().into();
    let mut ready = Vec::new();
    while ready != READY {
        ensure!(
            Instant::now() < deadline,
            "native broker readiness timed out"
        );
        ensure!(
            broker
                .child
                .try_wait()
                .map_err(|_| anyhow!("cannot inspect native broker"))?
                .is_none(),
            "native broker exited before readiness"
        );
        write_pending(&mut input, &mut header)
            .map_err(|_| anyhow!("cannot send native broker grant"))?;
        let mut bytes = [0; 64];
        match output.read(&mut bytes) {
            Ok(0) => return Err(anyhow!("native broker readiness closed")),
            Ok(n) => {
                ready.extend_from_slice(&bytes[..n]);
                ensure!(
                    ready.len() <= READY.len() && READY.starts_with(&ready),
                    "invalid native broker readiness"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(anyhow!("cannot read native broker readiness")),
        }
        thread::sleep(POLL);
    }
    ensure!(header.is_empty(), "native broker grant was not delivered");
    drop(input);
    drop(output);
    let directory = directory.keep();
    // exec replaces the original raw startup environment before any model code runs.
    let error = Command::new(&command[0])
        .args(&command[1..])
        .env(
            crate::config::overleaf::NATIVE_ENV,
            format!("unix:{}", socket.display()),
        )
        .exec();
    let _ = broker.stop();
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir(&directory);
    Err(anyhow!(
        "cannot start native broker backend ({:?})",
        error.kind()
    ))
}

pub fn serve(
    path: &Path,
    workspace: &Path,
    worker: &str,
    channel: &str,
    project: &str,
) -> Result<()> {
    ensure!(
        std::env::var_os(crate::config::overleaf::NATIVE_ENV).is_none(),
        "native broker must receive its grant only over stdin"
    );
    let parent = getppid().ok_or_else(|| anyhow!("native broker parent missing"))?;
    ensure!(parent != Pid::INIT, "native broker parent exited");
    let workspace = workspace_root(workspace)?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("native broker socket directory missing"))?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|_| anyhow!("native broker directory missing"))?;
    ensure!(
        path.is_absolute() && path.file_name().is_some_and(|name| name == "s"),
        "invalid native broker socket path"
    );
    ensure!(
        directory.parent() == Some(workspace.as_path()),
        "native broker directory is outside its workspace"
    );
    ensure!(
        directory.canonicalize()? == directory
            && metadata.is_dir()
            && !metadata.file_type().is_symlink(),
        "native broker directory must be resolved"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "native broker directory must be private (0700)"
    );
    let _cleanup = SocketCleanup {
        socket: path.to_owned(),
    };
    let mut raw = String::new();
    // One header, no credential in argv or the process's initial environment.
    let mut input = io::stdin().lock().take(16386);
    let length = io::BufRead::read_line(&mut input, &mut raw)
        .map_err(|_| anyhow!("cannot read native broker grant"))?;
    ensure!(
        length <= 16385 && raw.ends_with('\n'),
        "native broker grant header is invalid"
    );
    raw.pop();
    crate::config::overleaf::task_session(&raw, worker, channel, project)?;
    // These are dedicated broker/proxy processes; short relative names avoid AF_UNIX path limits.
    std::env::set_current_dir(directory)
        .map_err(|_| anyhow!("cannot enter native broker directory"))?;
    let listener =
        UnixListener::bind("s").map_err(|_| anyhow!("cannot bind native broker socket"))?;
    fs::set_permissions("s", fs::Permissions::from_mode(0o600))
        .map_err(|_| anyhow!("cannot protect native broker socket"))?;
    listener.set_nonblocking(true)?;
    io::stdout()
        .write_all(READY)
        .map_err(|_| anyhow!("cannot report native broker readiness"))?;
    io::stdout()
        .flush()
        .map_err(|_| anyhow!("cannot report native broker readiness"))?;
    while getppid() == Some(parent) {
        match listener.accept() {
            Ok((socket, _)) => {
                crate::config::overleaf::task_session(&raw, worker, channel, project)?;
                let mut command = Command::new(std::env::current_exe()?);
                command
                    .arg("overleaf-mcp")
                    .arg("--workspace-write")
                    .arg(&workspace)
                    .args([
                        "--native-worker",
                        worker,
                        "--native-channel",
                        channel,
                        "--thread-project",
                        project,
                    ])
                    .arg("--native-session-stdin")
                    .current_dir(&workspace);
                // A failed/disconnected client can reconnect; receipt policy decides all writes.
                let _ = bridge(socket, command, &raw, parent);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => return Err(anyhow!("native broker accept failed")),
        }
    }
    Ok(())
}

pub fn proxy(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "native broker socket path must be absolute"
    );
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("native broker socket directory missing"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("native broker socket name missing"))?;
    std::env::set_current_dir(directory)
        .map_err(|_| anyhow!("native broker socket directory unavailable"))?;
    let mut output = UnixStream::connect(name).map_err(|_| anyhow!("native broker unavailable"))?;
    let mut input = output
        .try_clone()
        .map_err(|_| anyhow!("cannot connect native broker input"))?;
    // Do not join a reader blocked on the model's stdin after the broker has closed.
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut input);
        let _ = input.shutdown(Shutdown::Write);
    });
    io::copy(&mut output, &mut io::stdout().lock())
        .map_err(|_| anyhow!("native broker stream closed"))?;
    Ok(())
}

fn bridge(mut socket: UnixStream, mut command: Command, raw: &str, parent: Pid) -> Result<()> {
    socket.set_nonblocking(true)?;
    let mut process = command
        .env_remove(crate::config::overleaf::NATIVE_ENV)
        .env_remove("OVERLEAF_GIT_TOKEN")
        .env_remove("ADS_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| anyhow!("cannot start scoped native MCP"))?;
    let input = process
        .stdin
        .take()
        .ok_or_else(|| anyhow!("native MCP input missing"))?;
    let mut output = process
        .stdout
        .take()
        .ok_or_else(|| anyhow!("native MCP output missing"))?;
    let mut child = ChildGuard {
        child: process,
        stopped: false,
    };
    nonblocking(&input)?;
    nonblocking(&output)?;
    let mut input = Some(input);
    let mut to_child: VecDeque<_> = format!("{raw}\n").into_bytes().into();
    let mut to_client = VecDeque::new();
    let mut client_eof = false;
    let mut output_eof = false;
    let mut exited = None;
    loop {
        if getppid() != Some(parent) {
            break;
        }
        if !client_eof && to_child.len() < LIMIT {
            client_eof = read_chunk(&mut socket, &mut to_child)
                .map_err(|_| anyhow!("native MCP client disconnected"))?;
        }
        if let Some(pipe) = &mut input {
            write_pending(pipe, &mut to_child).map_err(|_| anyhow!("native MCP input closed"))?;
        }
        if client_eof && to_child.is_empty() {
            drop(input.take());
        }
        if !output_eof && to_client.len() < LIMIT {
            output_eof = read_chunk(&mut output, &mut to_client)
                .map_err(|_| anyhow!("native MCP output closed"))?;
        }
        write_pending(&mut socket, &mut to_client)
            .map_err(|_| anyhow!("native MCP client disconnected"))?;
        if output_eof && to_client.is_empty() {
            break;
        }
        if child
            .child
            .try_wait()
            .map_err(|_| anyhow!("cannot inspect native MCP"))?
            .is_some()
        {
            let deadline = exited.get_or_insert_with(|| Instant::now() + Duration::from_secs(1));
            if Instant::now() >= *deadline {
                break;
            }
        }
        thread::sleep(POLL);
    }
    drop(input);
    child.stop()
}

fn nonblocking(fd: &impl AsFd) -> Result<()> {
    let flags = fcntl_getfl(fd).map_err(|_| anyhow!("cannot inspect native broker pipe"))?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|_| anyhow!("cannot set native broker pipe nonblocking"))?;
    Ok(())
}
fn read_chunk(reader: &mut impl Read, buffer: &mut VecDeque<u8>) -> io::Result<bool> {
    let mut bytes = [0; 8192];
    match reader.read(&mut bytes) {
        Ok(0) => Ok(true),
        Ok(n) => {
            buffer.extend(&bytes[..n]);
            Ok(false)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}
fn write_pending(writer: &mut impl Write, buffer: &mut VecDeque<u8>) -> io::Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    match writer.write(buffer.as_slices().0) {
        Ok(0) => Err(io::ErrorKind::WriteZero.into()),
        Ok(n) => {
            buffer.drain(..n);
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn workspace_root(workspace: &Path) -> Result<PathBuf> {
    let root = match workspace.strip_prefix("~") {
        Ok(rest) => std::env::home_dir()
            .ok_or_else(|| anyhow!("worker home missing"))?
            .join(rest),
        Err(_) => workspace.to_owned(),
    };
    ensure!(
        root.is_absolute() && root.canonicalize()? == root,
        "native broker requires a resolved workspace"
    );
    Ok(root)
}
struct SocketCleanup {
    socket: PathBuf,
}
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        if let Some(directory) = self.socket.parent() {
            let _ = fs::remove_dir(directory);
        }
    }
}
struct ChildGuard {
    child: Child,
    stopped: bool,
}
impl ChildGuard {
    fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.child.stdin.take();
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        if let Some(pid) = Pid::from_raw(self.child.id() as i32) {
            let _ = kill_process(pid, Signal::TERM);
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(POLL);
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(POLL);
        }
        Err(anyhow!("native broker child did not stop"))
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_header_stays_private_and_client_eof_closes_the_child() {
        let (client, socket) = UnixStream::pair().unwrap();
        let parent = getppid().unwrap();
        let peer = thread::spawn(move || {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "IFS= read -r grant; test -z \"$OVERLEAF_NATIVE_SESSION\" || exit 9; exec cat",
            ]);
            bridge(socket, command, "dummy-grant", parent).unwrap();
        });
        let mut client = client;
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1}\n")
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert_eq!(response, "{\"jsonrpc\":\"2.0\",\"id\":1}\n");
        peer.join().unwrap();
    }
}
