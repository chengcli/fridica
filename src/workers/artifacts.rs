//! Artifact validation and the local JobIo adapter. Remote reads and scoped
//! fetches remain unavailable until their trusted transport adapters are ported.
use super::protocol::{JobIo, NoJobIo, WorkerSpec};
use crate::{
    core::{delivery::AdapterFuture, worker::*},
    exec::local::{LocalTransport, ARTIFACT_LIMIT},
};
use std::path::{Component, Path, PathBuf};

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
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(async move {
            if artifacts.len() > 3 {
                return Err(WorkerFailure {
                    kind: Failure::Refusal,
                    code: "artifact_count_limit".into(),
                    backend_session_id: String::new(),
                });
            }
            let transport = LocalTransport {
                machine: spec.machine.clone(),
                home: self.home.clone(),
                excluded_env: spec.excluded_env.clone(),
            };
            let mut collected = vec![];
            for reference in artifacts {
                let result = async {
                    validate_reference(&reference)?;
                    if spec.machine.transport != "local" {
                        return Err("artifact_remote_adapter_unavailable");
                    }
                    let data = transport
                        .read_file(
                            reference.path.clone().into(),
                            vec![spec.workspace.path.clone()],
                            ARTIFACT_LIMIT,
                        )
                        .await
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
        })
    }
}
