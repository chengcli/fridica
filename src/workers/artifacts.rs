//! Artifact validation and local/SSH JobIo adapters. Compose with ScopedJobIo
//! to enable trusted repository fetches.
use super::{
    inputs,
    protocol::{JobIo, NoJobIo, WorkerSpec},
};
use crate::exec::ssh::SshTransport;
use crate::{
    core::{delivery::AdapterFuture, worker::*},
    exec::local::{LocalTransport, ARTIFACT_LIMIT},
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    time::Duration,
};

fn placement_failed() -> WorkerFailure {
    WorkerFailure {
        kind: Failure::Execution,
        code: "files_placement_failed".into(),
        backend_session_id: String::new(),
    }
}
pub fn validate_reference(reference: &ArtifactRef) -> Result<(), &'static str> {
    let path = Path::new(&reference.path);
    if reference.path.contains('\0')
        || !(path.is_absolute() || reference.path.starts_with("~/"))
        || path.components().any(|c| c == Component::ParentDir)
    {
        return Err("artifact_requires_absolute_path");
    }
    let suffix = match reference.kind.as_str() {
        "png" => "png",
        "pdf" => "pdf",
        "md" => "md",
        _ => return Err("artifact_kind_unsupported"),
    };
    if !path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(suffix))
    {
        return Err("artifact_suffix_mismatch");
    }
    Ok(())
}
pub fn validate_content(reference: &ArtifactRef, data: &[u8]) -> Result<(), &'static str> {
    if data.len() > ARTIFACT_LIMIT {
        return Err("artifact_size_limit");
    }
    match reference.kind.as_str() {
        "png" if !data.starts_with(b"\x89PNG\r\n\x1a\n") => Err("artifact_invalid_png"),
        "pdf" if !data.starts_with(b"%PDF-") => Err("artifact_invalid_pdf"),
        "md" if std::str::from_utf8(data).is_err() => Err("artifact_invalid_utf8"),
        "png" | "pdf" | "md" => Ok(()),
        _ => Err("artifact_kind_unsupported"),
    }
}
pub struct LocalJobIo {
    pub home: PathBuf,
}
impl JobIo for LocalJobIo {
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move { NoJobIo.prepare(spec, job).await })
    }
    fn place(
        &self,
        spec: WorkerSpec,
        job: Job,
        inputs: Vec<inputs::Input>,
    ) -> AdapterFuture<'_, Result<Vec<inputs::Placed>, WorkerFailure>> {
        let home = self.home.clone();
        Box::pin(async move {
            let _ = job;
            tokio::task::spawn_blocking(move || inputs::place_local(&home, &spec, &inputs))
                .await
                .map_err(|_| placement_failed())?
                .map_err(|_| placement_failed())
        })
    }
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(collect(&self.home, None, spec, artifacts))
    }
}

/// Trusted host environment and SSH settings are never taken from worker output.
pub struct SystemJobIo {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub ssh_control_directory: PathBuf,
    pub read_timeout: Duration,
}
impl JobIo for SystemJobIo {
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move { NoJobIo.prepare(spec, job).await })
    }
    fn place(
        &self,
        spec: WorkerSpec,
        job: Job,
        inputs: Vec<inputs::Input>,
    ) -> AdapterFuture<'_, Result<Vec<inputs::Placed>, WorkerFailure>> {
        Box::pin(async move {
            let _ = job;
            match spec.machine.transport.as_str() {
                "local" => {
                    let home = self.home.clone();
                    tokio::task::spawn_blocking(move || inputs::place_local(&home, &spec, &inputs))
                        .await
                        .map_err(|_| placement_failed())?
                }
                "ssh" => {
                    inputs::place_remote(
                        &self.ssh_control_directory,
                        &self.environment,
                        &spec,
                        &inputs,
                    )
                    .await
                }
                _ => Err(anyhow::anyhow!("unsupported transport")),
            }
            .map_err(|_| placement_failed())
        })
    }
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(collect(&self.home, Some(self), spec, artifacts))
    }
}
async fn collect(
    home: &Path,
    remote: Option<&SystemJobIo>,
    spec: WorkerSpec,
    artifacts: Vec<ArtifactRef>,
) -> Result<Vec<CollectedArtifact>, WorkerFailure> {
    if artifacts.len() > 3 {
        return Err(WorkerFailure {
            kind: Failure::Refusal,
            code: "artifact_count_limit".into(),
            backend_session_id: String::new(),
        });
    }
    let mut collected = vec![];
    for reference in artifacts {
        let result = async {
            validate_reference(&reference)?;
            let data = match spec.machine.transport.as_str() {
                "local" => {
                    LocalTransport {
                        machine: spec.machine.clone(),
                        home: home.into(),
                        excluded_env: spec.excluded_env.clone(),
                    }
                    .read_file(
                        reference.path.clone().into(),
                        vec![spec.workspace.path.clone()],
                        ARTIFACT_LIMIT,
                    )
                    .await
                }
                "ssh" => {
                    let remote = remote.ok_or("artifact_remote_adapter_unavailable")?;
                    SshTransport {
                        machine: spec.machine.clone(),
                        excluded_env: spec.excluded_env.clone(),
                        control_directory: remote.ssh_control_directory.clone(),
                    }
                    .read_file(
                        reference.path.clone().into(),
                        vec![spec.workspace.path.clone()],
                        ARTIFACT_LIMIT,
                        remote.environment.clone(),
                        remote.read_timeout,
                    )
                    .await
                }
                _ => return Err("artifact_transport_unsupported"),
            }
            .map_err(|_| "artifact_read_failed")?;
            validate_content(&reference, &data)?;
            Ok(data)
        }
        .await;
        collected.push(match result {
            Ok(data) => CollectedArtifact {
                reference,
                data: Some(data),
                error: String::new(),
            },
            Err(error) => CollectedArtifact {
                reference,
                data: None,
                error: error.into(),
            },
        });
    }
    Ok(collected)
}
