//! Trusted adapter interfaces; workers never receive daemon control capabilities.
use crate::{
    config::registry::{Machine, Workspace},
    core::{delivery::AdapterFuture, worker::*},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkerSpec {
    pub worker_id: String,
    pub machine: Machine,
    pub workspace: Workspace,
    pub backend: String,
    pub instructions: String,
    pub model: String,
    pub reasoning_effort: String,
    pub job_timeout: f64,
    pub idle_timeout: f64,
    pub excluded_env: Vec<String>,
    pub slot: usize,
}
impl WorkerSpec {
    pub fn create_cwd(&self) -> bool {
        self.slot > 0 && self.workspace.subfolders
    }
    pub fn confined(&self) -> bool {
        self.workspace.policy.gpu_confine == Some(true)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub job_id: String,
    pub attempt: u32,
    pub brief: String,
    pub resume: String,
    /// The backend session to fork into this worker's new session; empty for
    /// none. Never set together with `resume`.
    #[serde(default)]
    pub fork_from: String,
}
pub trait ApprovalHandler: Send + Sync {
    /// Called before supervisor configuration changes become visible.
    fn reconfigure(
        &self,
        _config: Arc<crate::config::Config>,
    ) -> AdapterFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn request(
        &self,
        worker: WorkerRecord,
        job: Job,
        request: ApprovalRequest,
    ) -> AdapterFuture<'_, ApprovalDecision>;
    fn cancel(&self, _worker_id: String) -> AdapterFuture<'_, ()> {
        Box::pin(async {})
    }
}
pub struct DenyApprovals;
impl ApprovalHandler for DenyApprovals {
    fn request(
        &self,
        _worker: WorkerRecord,
        _job: Job,
        _request: ApprovalRequest,
    ) -> AdapterFuture<'_, ApprovalDecision> {
        Box::pin(async { ApprovalDecision::Deny })
    }
}
pub trait Worker: Send + Sync {
    fn alive(&self) -> bool;
    fn busy(&self) -> bool;
    fn run(
        &self,
        request: RunRequest,
        worker: WorkerRecord,
        job: Job,
        approvals: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>>;
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>>;
    /// Must terminate and reap the backend/process group before returning success.
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>>;
}
pub trait Factory: Send + Sync {
    /// Reject snapshots incompatible with immutable adapter settings before
    /// publishing configuration or changing approval policy. No external I/O.
    fn validate_config(&self, _config: &crate::config::Config) -> anyhow::Result<()> {
        Ok(())
    }
    /// Run bounded admission after durable claim and before fetch/backend I/O.
    /// Implementations must retain their probe permits through cancellation cleanup.
    fn admit(
        &self,
        _config: Arc<crate::config::Config>,
        _spec: WorkerSpec,
    ) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
    }

    /// Supply the full contract/rules and repository context. The supervisor
    /// never silently substitutes an abbreviated prompt for the owner rules.
    fn instructions(
        &self,
        config: &crate::config::Config,
        worker: &WorkerRecord,
    ) -> anyhow::Result<String>;
    /// Construct a handle only; external startup belongs in run(), after durable admission.
    fn create(&self, spec: WorkerSpec) -> Result<Arc<dyn Worker>, WorkerFailure>;
}
pub trait JobIo: Send + Sync {
    /// Optional trusted scoped-fetch adapter. Return appended factual context.
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>>;
    /// Place downloaded files in the worker's workspace; where each landed.
    fn place(
        &self,
        _spec: WorkerSpec,
        _job: Job,
        _inputs: Vec<super::inputs::Input>,
    ) -> AdapterFuture<'_, Result<Vec<super::inputs::Placed>, WorkerFailure>> {
        Box::pin(async {
            Err(WorkerFailure {
                kind: Failure::Refusal,
                code: "files_unsupported".into(),
                backend_session_id: String::new(),
            })
        })
    }
    /// The job attempt's progress file (`file`, in the slot workspace) while
    /// it runs (#105), at most `PROGRESS_LIMIT` bytes. `None` when there is
    /// none, or it cannot be read: progress is best effort, never a failure.
    fn progress(&self, _spec: WorkerSpec, _file: String) -> AdapterFuture<'_, Option<Vec<u8>>> {
        Box::pin(async { None })
    }
    /// Read only validated artifacts confined to the slot workspace.
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>>;
}
/// Until transport adapters are installed, requested fetches visibly refuse and
/// artifact references are retained as rejected records, without reading files.
pub struct NoJobIo;
impl JobIo for NoJobIo {
    fn prepare(
        &self,
        _spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move {
            if job.fetch_repo.is_empty() {
                Ok(String::new())
            } else {
                Err(WorkerFailure {
                    kind: Failure::Refusal,
                    code: "fetch_adapter_unavailable".into(),
                    backend_session_id: String::new(),
                })
            }
        })
    }
    fn collect(
        &self,
        _spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(async move {
            Ok(artifacts
                .into_iter()
                .map(|reference| CollectedArtifact {
                    reference,
                    data: None,
                    error: "artifact_adapter_unavailable".into(),
                })
                .collect())
        })
    }
}
