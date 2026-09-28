//! Credential-free scoped fetch on a configured execution machine. The helper
//! is fixed application code, not a model-supplied command or campaign executor.
use super::{
    process::{self, Launch},
    shell, ssh,
};
use crate::{
    config::registry::{github_repo, valid_fetch_ref, Machine},
    core::delivery::AdapterFuture,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, PathBuf},
    time::Duration,
};

pub const HELPER: &str = include_str!("fetch_helper.py");
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub machine: Machine,
    pub workspace: PathBuf,
    pub excluded_env: Vec<String>,
    pub repo: String,
    pub reference: String,
    pub leaf: String,
    pub create: bool,
}
impl Request {
    pub fn valid(&self) -> bool {
        github_repo(&self.repo)
            && valid_fetch_ref(&self.reference)
            && self.leaf.strip_prefix(".fridica-fetch-").is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 100
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            })
            && self
                .workspace
                .to_str()
                .is_some_and(|s| !s.contains('\0') && (s.starts_with('/') || s.starts_with("~/")))
            && !self
                .workspace
                .components()
                .any(|c| c == Component::ParentDir)
    }
    pub fn path(&self) -> String {
        self.workspace
            .join(&self.leaf)
            .to_string_lossy()
            .into_owned()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetched {
    pub path: String,
    pub commit: String,
}
impl Fetched {
    pub fn valid(&self, request: &Request) -> bool {
        self.path == request.path()
            && (40..=64).contains(&self.commit.len())
            && self.commit.bytes().all(|c| c.is_ascii_hexdigit())
    }
}
#[derive(Clone, Copy, Debug)]
pub struct FetchError(pub &'static str);
pub trait Fetcher: Send + Sync {
    fn fetch(&self, request: Request) -> AdapterFuture<'_, Result<Fetched, FetchError>>;
}
/// Tool paths are trusted adapter configuration; they are never part of a job
/// or model response. Custom host installations may explicitly replace them.
pub struct SystemFetcher {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub ssh_control_directory: PathBuf,
    pub python: PathBuf,
    pub git: PathBuf,
    pub timeout: Duration,
}
impl SystemFetcher {
    pub fn new(
        home: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        ssh_control_directory: PathBuf,
    ) -> Self {
        Self {
            home,
            environment,
            ssh_control_directory,
            python: "/usr/bin/python3".into(),
            git: "/usr/bin/git".into(),
            timeout: Duration::from_secs(600),
        }
    }
    pub fn launch(&self, request: &Request) -> Result<Launch, FetchError> {
        if !request.valid()
            || !self.python.is_absolute()
            || !self.git.is_absolute()
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(600)
        {
            return Err(FetchError("fetch_invalid_request"));
        }
        let python = self
            .python
            .to_str()
            .ok_or(FetchError("fetch_invalid_tools"))?;
        let git = self.git.to_str().ok_or(FetchError("fetch_invalid_tools"))?;
        let payload=serde_json::json!({"repo":request.repo,"reference":request.reference,"leaf":request.leaf,"workspace":request.workspace,"create":request.create,"git":git,"timeout":self.timeout.as_secs_f64()}).to_string();
        let command = vec![
            python.into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            HELPER.into(),
            payload,
        ];
        if shell::validate(&command).is_err() {
            return Err(FetchError("fetch_invalid_tools"));
        }
        match request.machine.transport.as_str() {
            "local" => Ok(Launch {
                argv: command,
                cwd: Some("/".into()),
                env: BTreeMap::from([
                    ("HOME".into(), self.home.as_os_str().to_owned()),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ]),
            }),
            "ssh" => {
                // SSH retains the owner's existing configuration and agent, but
                // neither forwarding nor Git credentials are added. The helper
                // independently cleans child groups on channel EOF.
                let info = std::fs::symlink_metadata(&self.ssh_control_directory)
                    .map_err(|_| FetchError("fetch_ssh_control_directory"))?;
                use std::os::unix::fs::MetadataExt;
                if !info.is_dir()
                    || info.uid() != users::get_current_uid()
                    || info.mode() & 0o077 != 0
                {
                    return Err(FetchError("fetch_ssh_control_directory"));
                }
                let script = format!(
                    "exec /usr/bin/env -i \"HOME=$HOME\" PATH=/usr/bin:/bin {}",
                    shell::join(&command)
                );
                let argv =
                    ssh::command(&request.machine.host, &script, &self.ssh_control_directory)
                        .map_err(|_| FetchError("fetch_invalid_transport"))?;
                Ok(Launch {
                    argv,
                    cwd: None,
                    env: process::scrubbed_environment(
                        self.environment.clone(),
                        &request.excluded_env,
                        &BTreeMap::new(),
                    ),
                })
            }
            _ => Err(FetchError("fetch_unsupported_transport")),
        }
    }
}
impl Fetcher for SystemFetcher {
    fn fetch(&self, request: Request) -> AdapterFuture<'_, Result<Fetched, FetchError>> {
        Box::pin(async move {
            let launch = self.launch(&request)?;
            let completed = process::run_with_open_stdin(
                launch,
                self.timeout + Duration::from_secs(65),
                64 * 1024,
            )
            .await
            .map_err(|_| FetchError("fetch_process_failed"))?;
            if completed.returncode != 0 {
                return Err(FetchError(
                    if request.machine.transport == "ssh" && completed.returncode == 255 {
                        "fetch_remote_disconnected"
                    } else {
                        "fetch_failed"
                    },
                ));
            }
            let result: Fetched = serde_json::from_slice(&completed.stdout)
                .map_err(|_| FetchError("fetch_invalid_result"))?;
            if !result.valid(&request) {
                return Err(FetchError("fetch_invalid_result"));
            }
            Ok(result)
        })
    }
}
