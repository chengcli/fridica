//! Scoped-fetch JobIo wrapper. Ordinary workers receive only a local bare-repo
//! path and verified commit; they acquire no network or push capability.
use super::{
    inputs,
    protocol::{JobIo, WorkerSpec},
};
use crate::{
    config::registry::valid_fetch_ref,
    core::{delivery::AdapterFuture, time::Clock, worker::*},
    exec::fetch::{Fetched, Fetcher, Request},
    store::{fetch, Store},
};
use fridica_slack::files::Downloader;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::oneshot;
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
        .fetch_grant(&job.fetch_repo)
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
        repo: granted.to_string(),
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
    /// Reads Slack files for a job's `files`; `None` refuses such jobs.
    pub files: Option<Arc<dyn Downloader>>,
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
impl ScopedJobIo {
    /// Download the job's attached files and place them in the workspace. The
    /// result is recorded as `worker_files`; any failure fails the job, so a
    /// worker never runs believing it has a file it does not.
    async fn files(&self, spec: &WorkerSpec, job: &Job) -> Result<String, WorkerFailure> {
        if job.files.is_empty() {
            return Ok(String::new());
        }
        let Some(downloader) = &self.files else {
            return Err(failure(Failure::Refusal, "files_unsupported"));
        };
        let session = job.session_id.clone();
        let wanted = job.files.clone();
        let names: Vec<(String, String)> = self
            .store
            .call(move |c| {
                let rows: Vec<String> = c
                    .prepare("SELECT attachments_json FROM messages WHERE workspace||':'||channel||':'||root_ts=?")?
                    .query_map([session], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let attachments: Vec<serde_json::Value> = rows
                    .iter()
                    .filter_map(|r| serde_json::from_str::<Vec<serde_json::Value>>(r).ok())
                    .flatten()
                    .collect();
                Ok(wanted
                    .iter()
                    .map(|id| {
                        let name = attachments
                            .iter()
                            .find(|a| a["id"] == id.as_str())
                            .and_then(|a| a["name"].as_str())
                            .unwrap_or("")
                            .to_owned();
                        (id.clone(), name)
                    })
                    .collect())
            })
            .await
            .map_err(|_| failure(Failure::Execution, "files_storage_failed"))?;
        if names.iter().any(|(_, name)| name.is_empty()) {
            return Err(failure(Failure::Refusal, "files_not_in_thread"));
        }
        let directory = tempfile::Builder::new()
            .prefix("fridica-files-")
            .permissions(
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            )
            .tempdir()
            .map_err(|_| failure(Failure::Execution, "files_download_failed"))?;
        let unique = inputs::unique_names(&names);
        let mut downloaded = vec![];
        let mut record = vec![];
        for ((id, _), name) in names.iter().zip(unique) {
            let source = directory.path().join(&name);
            let saved = async {
                let url = downloader.resolve(id.clone()).await?;
                downloader
                    .save(url, source.clone(), inputs::INPUT_LIMIT)
                    .await
            }
            .await;
            match saved {
                Ok(size) => {
                    record.push(json!({"id":id,"name":name,"size":size}));
                    downloaded.push(inputs::Input {
                        id: id.clone(),
                        name,
                        source,
                        size,
                    });
                }
                Err(error) => {
                    record.push(json!({"id":id,"name":name,"error":error}));
                    self.record(job, json!({"files":record,"placed":false}))
                        .await?;
                    return Err(failure(
                        Failure::Execution,
                        if matches!(error, fridica_slack::files::Failure::TooLarge) {
                            "files_too_large"
                        } else {
                            "files_download_failed"
                        },
                    ));
                }
            }
        }
        let placed = self
            .artifacts
            .place(spec.clone(), job.clone(), downloaded)
            .await;
        self.record(
            job,
            json!({"files":record,"placed":placed.as_ref().map(|p| json!(p)).unwrap_or(json!(false))}),
        )
        .await?;
        Ok(inputs::context(&placed?))
    }
    async fn record(&self, job: &Job, payload: serde_json::Value) -> Result<(), WorkerFailure> {
        let now = self.clock.now();
        let payload = json!({"job_id":job.id,"attempt":job.attempt,"result":payload});
        self.store
            .call(move |c| {
                c.execute(
                    "INSERT INTO replay_events(kind,time,payload_json) VALUES('worker_files',?,?)",
                    rusqlite::params![now, payload.to_string()],
                )?;
                Ok(())
            })
            .await
            .map_err(|_| failure(Failure::Execution, "files_storage_failed"))
    }
}
impl JobIo for ScopedJobIo {
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move {
            let layout = inputs::layout(&spec);
            let files = self.files(&spec, &job).await?;
            let rest = self.fetch(spec, job).await?;
            Ok(format!("{layout}{files}{rest}"))
        })
    }
    fn place(
        &self,
        spec: WorkerSpec,
        job: Job,
        inputs: Vec<inputs::Input>,
    ) -> AdapterFuture<'_, Result<Vec<inputs::Placed>, WorkerFailure>> {
        self.artifacts.place(spec, job, inputs)
    }
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        self.artifacts.collect(spec, artifacts)
    }
}
impl ScopedJobIo {
    async fn fetch(&self, spec: WorkerSpec, job: Job) -> Result<String, WorkerFailure> {
        {
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
        }
    }
}
