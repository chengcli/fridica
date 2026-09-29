//! Scoped-fetch JobIo wrapper. Ordinary workers receive only a local bare-repo
//! path and verified commit; they acquire no network or push capability.
use super::protocol::{JobIo, WorkerSpec};
use crate::{
    config::registry::valid_fetch_ref,
    core::{delivery::AdapterFuture, time::Clock, worker::*},
    exec::fetch::{Fetched, Fetcher, Request},
    store::{fetch, Store},
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::oneshot;
use unicode_casefold::UnicodeCaseFold;
fn failure(kind: Failure, code: &str) -> WorkerFailure {
    WorkerFailure {
        kind,
        code: code.into(),
        backend_session_id: String::new(),
    }
}
pub fn request(spec: &WorkerSpec, job: &Job) -> Result<Request, WorkerFailure> {
    let policy = &spec.workspace.policy;
    if policy.validate().is_err()
        || policy.mode != "write"
        || !policy.network.is_empty()
        || policy.approvals == "auto"
        || !policy.auto_approve.is_empty()
        || policy.gpu_confine == Some(true)
    {
        return Err(failure(Failure::Refusal, "fetch_invalid_policy"));
    }
    let granted = policy
        .fetch_repos
        .iter()
        .find(|repo| {
            repo.as_str()
                .case_fold()
                .eq(job.fetch_repo.as_str().case_fold())
        })
        .ok_or_else(|| failure(Failure::Refusal, "fetch_repository_not_granted"))?;
    if !valid_fetch_ref(&job.fetch_ref) {
        return Err(failure(Failure::Refusal, "fetch_invalid_ref"));
    }
    if job.id.is_empty()
        || job.id.len() > 64
        || !job
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || job.worker_id != spec.worker_id
    {
        return Err(failure(Failure::Refusal, "fetch_invalid_job"));
    }
    let leaf = if job.attempt <= 1 {
        format!(".fridica-fetch-{}", job.id)
    } else {
        format!(".fridica-fetch-{}-attempt-{}", job.id, job.attempt)
    };
    let request = Request {
        machine: spec.machine.clone(),
        workspace: spec.workspace.path.clone(),
        excluded_env: spec.excluded_env.clone(),
        repo: granted.clone(),
        reference: job.fetch_ref.clone(),
        leaf,
        create: spec.create_cwd(),
    };
    if !request.valid() {
        return Err(failure(Failure::Refusal, "fetch_invalid_request"));
    }
    Ok(request)
}
pub fn context(job: &Job, fetched: &Fetched) -> String {
    format!("\n\nFridica fetched {} {} at {} into the bare repository {}. Inspect it locally; this does not grant network or push access.",job.fetch_repo,job.fetch_ref,fetched.commit,fetched.path)
}
#[derive(Clone)]
pub struct ScopedJobIo {
    pub store: Store,
    pub fetcher: Arc<dyn Fetcher>,
    pub artifacts: Arc<dyn JobIo>,
    pub clock: Arc<dyn Clock>,
}
impl ScopedJobIo {
    async fn run(
        &self,
        request: Request,
        job: Job,
        reply: &mut oneshot::Sender<Result<String, WorkerFailure>>,
    ) -> Result<String, WorkerFailure> {
        let seq = fetch::begin(&self.store, job.clone(), json!(request), self.clock.now())
            .await
            .map_err(|_| failure(Failure::Execution, "fetch_intent_storage_failed"))?
            .ok_or_else(|| failure(Failure::Cancelled, "fetch_job_not_active"))?;
        let result=tokio::select! {biased;
            _=reply.closed()=>Err(failure(Failure::Cancelled,"fetch_cancelled")),
            result=self.fetcher.fetch(request.clone())=>result.map_err(|e|failure(Failure::Execution,e.0)),
        }.and_then(|result|if result.valid(&request){Ok(result)}else{Err(failure(Failure::Refusal,"fetch_invalid_result"))});
        let recorded = match &result {
            Ok(fetched) => json!(fetched),
            Err(error) => json!({"error":error.code,"kind":error.kind}),
        };
        let active = fetch::finish(&self.store, job.clone(), seq, recorded, self.clock.now())
            .await
            .map_err(|_| failure(Failure::Execution, "fetch_completion_storage_failed"))?;
        if active {
            result.map(|fetched| context(&job, &fetched))
        } else {
            Err(failure(Failure::Cancelled, "fetch_job_not_active"))
        }
    }
}
impl JobIo for ScopedJobIo {
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move {
            if job.fetch_repo.is_empty() {
                if !job.fetch_ref.is_empty() {
                    return Err(failure(Failure::Refusal, "fetch_repository_not_granted"));
                }
                return self.artifacts.prepare(spec, job).await;
            }
            let request = request(&spec, &job)?;
            let (reply, wait) = oneshot::channel();
            let io = self.clone();
            // Complete the intent even when cancellation arrives during a queued
            // database write. Dropping fetch() closes the helper's stdin channel.
            tokio::spawn(async move {
                let mut reply = reply;
                let result = io.run(request, job, &mut reply).await;
                let _ = reply.send(result);
            });
            wait.await
                .unwrap_or_else(|_| Err(failure(Failure::Execution, "fetch_storage_failed")))
        })
    }
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        self.artifacts.collect(spec, artifacts)
    }
}
