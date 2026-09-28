//! Owner-only Unix HTTP transport. Accepted operations have an owner task so a
//! client disconnect cannot cancel a committed control effect halfway through.
use super::{Backend, Request, Response, BODY_LIMIT, RESPONSE_LIMIT};
use crate::{config::Config, core::Authority};
use fs2::FileExt;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs::{File, OpenOptions},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{oneshot, watch, Mutex, Semaphore},
    task::JoinSet,
};
/// A bearer secret has no Debug/Serialize implementation. Store it outside
/// worker workspaces;
/// only its digest remains in the server's capability map.
pub struct Capability {
    secret: String,
    authority: Authority,
}
impl Capability {
    pub fn generate(authority: Authority) -> Result<Self, Failure> {
        if !matches!(
            authority,
            Authority::Owner | Authority::Overseer | Authority::DesktopReadOnly
        ) {
            return Err(Failure::Configuration);
        }
        Ok(Self {
            secret: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            authority,
        })
    }
    pub fn secret(&self) -> &str {
        &self.secret
    }
}
pub enum Access {
    OwnerPeer,
    Capabilities(BTreeMap<[u8; 32], Authority>),
}
impl Access {
    pub fn capabilities(values: &[Capability]) -> Result<Self, Failure> {
        if values.is_empty() {
            return Err(Failure::Configuration);
        }
        Ok(Self::Capabilities(
            values
                .iter()
                .map(|c| {
                    (
                        Sha256::digest(c.secret.as_bytes()).into(),
                        c.authority.clone(),
                    )
                })
                .collect(),
        ))
    }
    fn authority(&self, request: &hyper::Request<Incoming>) -> Option<Authority> {
        let headers = request.headers();
        if headers.get_all("authorization").iter().count() > 1 {
            return None;
        }
        match self {
            Self::OwnerPeer => {
                if headers.contains_key("authorization") {
                    None
                } else {
                    Some(Authority::Owner)
                }
            }
            Self::Capabilities(values) => {
                let token = headers
                    .get("authorization")?
                    .to_str()
                    .ok()?
                    .strip_prefix("Bearer ")?;
                if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                values.get(&digest).cloned()
            }
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Configuration,
    UnsafePath,
    AlreadyRunning,
    Bind,
    Task,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control server failed: {self:?}")
    }
}
impl std::error::Error for Failure {}
#[derive(Clone)]
pub struct Options {
    pub connections: usize,
    pub operations: usize,
    pub body_timeout: Duration,
    pub shutdown_timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            connections: 64,
            operations: 16,
            body_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}
struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino && m.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
pub struct Server {
    stop: watch::Sender<bool>,
    done: Option<tokio::task::JoinHandle<Result<(), Failure>>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl Server {
    pub async fn bind(
        config: &Config,
        backend: Arc<dyn Backend>,
        access: Access,
        options: Options,
    ) -> Result<Self, Failure> {
        if options.connections == 0
            || options.connections > 256
            || options.operations == 0
            || options.operations > 64
            || options.body_timeout.is_zero()
            || options.body_timeout > Duration::from_secs(60)
            || options.shutdown_timeout.is_zero()
            || options.shutdown_timeout > Duration::from_secs(60)
        {
            return Err(Failure::Configuration);
        }
        let path = &config.state.control_socket;
        if !path.is_absolute() || path.as_os_str().as_bytes().len() > 103 {
            return Err(Failure::UnsafePath);
        }
        let parent = path.parent().ok_or(Failure::UnsafePath)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| Failure::UnsafePath)?;
        let metadata = std::fs::symlink_metadata(parent).map_err(|_| Failure::UnsafePath)?;
        if !metadata.is_dir()
            || metadata.uid() != users::get_current_uid()
            || metadata.mode() & 0o077 != 0
        {
            return Err(Failure::UnsafePath);
        }
        let parent = parent.canonicalize().map_err(|_| Failure::UnsafePath)?;
        let path = parent.join(path.file_name().ok_or(Failure::UnsafePath)?);
        for machine in &config.machines.machines {
            if machine.transport == "local" {
                for workspace in &machine.workspaces {
                    let workspace = workspace
                        .path
                        .canonicalize()
                        .map_err(|_| Failure::UnsafePath)?;
                    if path.starts_with(workspace) {
                        return Err(Failure::UnsafePath);
                    }
                }
            }
        }
        let lock_path = path.with_extension("sock.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(lock_path)
            .map_err(|_| Failure::UnsafePath)?;
        let metadata = lock.metadata().map_err(|_| Failure::UnsafePath)?;
        if !metadata.is_file()
            || metadata.uid() != users::get_current_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(Failure::UnsafePath);
        }
        FileExt::try_lock_exclusive(&lock).map_err(|_| Failure::AlreadyRunning)?;
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                if !m.file_type().is_socket() || m.uid() != users::get_current_uid() {
                    return Err(Failure::UnsafePath);
                }
                match tokio::time::timeout(Duration::from_millis(200), UnixStream::connect(&path))
                    .await
                {
                    Ok(Err(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    _ => return Err(Failure::AlreadyRunning),
                }
                std::fs::remove_file(&path).map_err(|_| Failure::UnsafePath)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Failure::UnsafePath),
        }
        let listener = UnixListener::bind(&path).map_err(|_| Failure::Bind)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| Failure::Bind)?;
        let guard = SocketGuard {
            path: path.clone(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            _lock: lock,
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Failure::Bind)?;
        let (stop, stopping) = watch::channel(false);
        let done = tokio::spawn(run(
            listener,
            guard,
            backend,
            Arc::new(access),
            options,
            stopping,
        ));
        Ok(Self {
            stop,
            done: Some(done),
        })
    }
    pub async fn close(mut self) -> Result<(), Failure> {
        self.stop.send_replace(true);
        self.done.take().unwrap().await.map_err(|_| Failure::Task)?
    }
}
type Tasks = Arc<Mutex<JoinSet<()>>>;
async fn run(
    listener: UnixListener,
    guard: SocketGuard,
    backend: Arc<dyn Backend>,
    access: Arc<Access>,
    options: Options,
    mut stop: watch::Receiver<bool>,
) -> Result<(), Failure> {
    let capacity = Arc::new(Semaphore::new(options.connections));
    let operations = Arc::new(Semaphore::new(options.operations));
    let tasks: Tasks = Arc::new(Mutex::new(JoinSet::new()));
    let mut connections = JoinSet::new();
    let mut result = Ok(());
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            incoming = listener.accept() => {
                let Ok((stream, _)) = incoming else {
                    result = Err(Failure::Bind);
                    break;
                };
                if !stream.peer_cred().is_ok_and(|c| c.uid() == users::get_current_uid()) {
                    continue;
                }
                let Ok(permit) = capacity.clone().try_acquire_owned() else { continue; };
                let backend = backend.clone();
                let access = access.clone();
                let tasks = tasks.clone();
                let operations = operations.clone();
                let deadline = options.body_timeout;
                connections.spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |request| serve(
                        request, backend.clone(), access.clone(), tasks.clone(),
                        operations.clone(), deadline,
                    ));
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false).max_buf_size(16*1024).max_headers(64)
                        .timer(TokioTimer::new()).header_read_timeout(deadline);
                    let _ = tokio::time::timeout(Duration::from_secs(60),
                        builder.serve_connection(TokioIo::new(stream), service)).await;
                });
            },
        }
    }
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let mut tasks = tasks.lock().await;
    if tokio::time::timeout(options.shutdown_timeout, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        result = Err(Failure::Task);
    }
    drop(guard);
    result
}
async fn serve(
    request: hyper::Request<Incoming>,
    backend: Arc<dyn Backend>,
    access: Arc<Access>,
    tasks: Tasks,
    operations: Arc<Semaphore>,
    deadline: Duration,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    if request.uri().scheme().is_some()
        || request.uri().authority().is_some()
        || request.headers().contains_key("origin")
    {
        return Ok(http(Response::error(400, "invalid_origin")));
    }
    let Some(authority) = access.authority(&request) else {
        return Ok(http(Response::error(401, "authentication_required")));
    };
    let method = request.method().as_str().to_owned();
    if authority == Authority::DesktopReadOnly && method != "GET" {
        return Ok(http(Response::error(403, "read_only_capability")));
    }
    let target = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    if super::api::target(&target).is_none() {
        return Ok(http(Response::error(400, "invalid_target")));
    }
    let body = match tokio::time::timeout(
        deadline,
        Limited::new(request.into_body(), BODY_LIMIT).collect(),
    )
    .await
    {
        Ok(Ok(body)) => body.to_bytes(),
        Ok(Err(_)) => return Ok(http(Response::error(413, "body_too_large_or_incomplete"))),
        Err(_) => return Ok(http(Response::error(408, "body_timeout"))),
    };
    let body = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) if v.is_object() => v,
            _ => return Ok(http(Response::error(400, "body_must_be_a_json_object"))),
        }
    };
    let Ok(permit) = operations.try_acquire_owned() else {
        return Ok(http(Response::error(503, "control_busy")));
    };
    let (sender, receiver) = oneshot::channel();
    {
        let mut tasks = tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _permit = permit;
            let response = backend
                .request(
                    Request {
                        method,
                        target,
                        body,
                    },
                    authority,
                )
                .await;
            let _ = sender.send(response);
        });
    }
    Ok(http(receiver.await.unwrap_or_else(|_| {
        Response::error(500, "control_task_failed")
    })))
}
fn http(response: Response) -> hyper::Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&response.body).unwrap_or_default();
    let (status, bytes) = if bytes.len() > RESPONSE_LIMIT {
        (413, b"{\"error\":\"response_too_large\"}".to_vec())
    } else {
        (response.status, bytes)
    };
    hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .header("connection", "close")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap()
}
