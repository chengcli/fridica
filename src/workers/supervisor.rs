//! One supervisor per daemon. Durable job permits are distinct from sticky
//! placement and the live process pool. Closing processes retain their permits.
use super::protocol::*;
use crate::{
    config::Config,
    core::{time::Clock, worker::*},
    store::{
        work::{self, Completed, Completion},
        Store,
    },
};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{watch, Mutex},
    task::JoinHandle,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Signal {
    Run,
    Interrupt,
    Stop,
    Shutdown,
}
struct Live {
    worker_id: String,
    spec: WorkerSpec,
    worker: Arc<dyn Worker>,
    retiring: bool,
}
struct Running {
    worker_id: String,
    control: watch::Sender<Signal>,
    task: JoinHandle<Result<TaskEnd>>,
}
struct TaskEnd {
    retire: bool,
    closed: bool,
}
struct State {
    config: Arc<Config>,
    live: Vec<Live>,
    running: BTreeMap<String, Running>,
    closing: bool,
    observe_only: bool,
}
pub struct Options {
    pub stop_grace: Duration,
    pub observe_only: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(10),
            observe_only: false,
        }
    }
}
pub struct Supervisor {
    store: Store,
    factory: Arc<dyn Factory>,
    approvals: Arc<dyn ApprovalHandler>,
    io: Arc<dyn JobIo>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    stop_grace: Duration,
}
impl Supervisor {
    pub fn new(
        store: Store,
        config: Arc<Config>,
        factory: Arc<dyn Factory>,
        approvals: Arc<dyn ApprovalHandler>,
        io: Arc<dyn JobIo>,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<Self> {
        let stop_grace = options.stop_grace;
        if stop_grace.is_zero()
            || !config.limits.job_timeout.is_finite()
            || config.limits.job_timeout <= 0.
            || Duration::try_from_secs_f64(config.limits.job_timeout).is_err()
        {
            bail!("invalid worker deadline");
        }
        factory.validate_config(&config)?;
        Ok(Self {
            store,
            factory,
            approvals,
            io,
            clock,
            state: Mutex::new(State {
                config,
                live: vec![],
                running: BTreeMap::new(),
                closing: false,
                observe_only: options.observe_only,
            }),
            stop_grace,
        })
    }
    async fn reap(&self, s: &mut State) -> Result<()> {
        let mut done = Vec::new();
        for (id, r) in &s.running {
            if r.task.is_finished()
                || work::get_job(&self.store, id.clone()).await?.status != "running"
            {
                done.push(id.clone());
            }
        }
        let mut error = None;
        for id in done {
            let r = s.running.remove(&id).unwrap();
            match r.task.await {
                Ok(Ok(end)) => {
                    if end.retire {
                        if end.closed {
                            s.live.retain(|v| v.worker_id != r.worker_id);
                        } else if let Some(l) =
                            s.live.iter_mut().find(|l| l.worker_id == r.worker_id)
                        {
                            l.retiring = true;
                        }
                    }
                }
                outcome => {
                    if let Some(l) = s.live.iter_mut().find(|l| l.worker_id == r.worker_id) {
                        l.retiring = true;
                    }
                    error = Some(match outcome {
                        Ok(Err(e)) => e,
                        Err(e) => e.into(),
                        _ => unreachable!(),
                    });
                }
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        Ok(())
    }
    async fn close_live(&self, s: &mut State, index: usize) -> Result<bool> {
        let worker = s.live[index].worker.clone();
        let was_retiring = s.live[index].retiring;
        s.live[index].retiring = true;
        let closed = close_worker(&worker, self.stop_grace).await;
        if closed {
            s.live.remove(index);
        } else if !was_retiring {
            record_close_failure(&self.store, &s.live[index].worker_id, self.clock.now()).await?;
        }
        Ok(closed)
    }
    pub async fn reconfigure(&self, config: Arc<Config>) -> Result<()> {
        let mut s = self.state.lock().await;
        if s.closing {
            bail!("supervisor is closing");
        }
        if Duration::try_from_secs_f64(config.limits.job_timeout).is_err()
            || config.limits.job_timeout <= 0.
        {
            bail!("invalid worker deadline");
        }
        self.factory.validate_config(&config)?;
        self.approvals.reconfigure(config.clone()).await?;
        s.config = config;
        Ok(())
    }
    pub async fn schedule(&self) -> Result<Vec<String>> {
        let mut s = self.state.lock().await;
        self.reap(&mut s).await?;
        if s.closing || s.observe_only {
            return Ok(vec![]);
        }
        let config = s.config.clone();
        let now = self.clock.now();
        let snapshot = work::snapshot(&self.store).await?;
        let mut records: BTreeMap<_, _> = snapshot
            .workers
            .into_iter()
            .map(|w| (w.id.clone(), w))
            .collect();
        let waiting: HashSet<_> = snapshot
            .queued
            .iter()
            .map(|j| j.worker_id.clone())
            .collect();
        let mut occupied: HashSet<_> = snapshot
            .running
            .iter()
            .filter_map(|j| {
                records
                    .get(&j.worker_id)
                    .map(|w| (w.machine.clone(), w.slot))
            })
            .collect();
        let mut busy: HashSet<_> = snapshot
            .running
            .iter()
            .map(|j| j.worker_id.clone())
            .collect();
        busy.extend(s.running.values().map(|r| r.worker_id.clone()));
        let mut per_machine: BTreeMap<String, usize> = BTreeMap::new();
        let mut assigned: BTreeMap<(String, usize), usize> = BTreeMap::new();
        for j in &snapshot.running {
            if let Some(w) = records.get(&j.worker_id) {
                *per_machine.entry(w.machine.clone()).or_default() += 1;
            }
        }
        for w in records.values().filter(|w| w.status != "stopped") {
            *assigned.entry((w.machine.clone(), w.slot)).or_default() += 1;
        }
        let mut total = snapshot.running.len();
        let mut started = vec![];
        'jobs: for j in snapshot.queued {
            if total >= config.limits.max_jobs {
                break;
            }
            let w = records
                .get(&j.worker_id)
                .context("queued job has no worker")?;
            let Some(m) = config.machines.get(&w.machine) else {
                work::claim(&self.store, j.id.clone(), 1, config.clone(), now).await?;
                continue;
            };
            if w.status == "stopped" {
                work::claim(&self.store, j.id.clone(), 1, config.clone(), now).await?;
                continue;
            }
            if busy.contains(&w.id) || *per_machine.get(&m.name).unwrap_or(&0) >= m.max_jobs {
                continue;
            }
            let slot = if w.slot > 0 && w.slot <= m.max_jobs {
                if occupied.contains(&(m.name.clone(), w.slot)) {
                    continue;
                }
                w.slot
            } else {
                let Some(slot) = (1..=m.max_jobs)
                    .filter(|slot| !occupied.contains(&(m.name.clone(), *slot)))
                    .min_by_key(|slot| {
                        (*assigned.get(&(m.name.clone(), *slot)).unwrap_or(&0), *slot)
                    })
                else {
                    continue;
                };
                slot
            };
            let mut view = w.clone();
            view.slot = slot;
            let spec = make_spec(&config, &view).and_then(|mut spec| {
                spec.instructions = self.factory.instructions(&config, &view)?;
                if spec.instructions.trim().is_empty() {
                    bail!("worker instructions are empty");
                }
                Ok(spec)
            });
            // Retiring or changed-policy processes must be confirmed closed first.
            if let Some(i) = s.live.iter().position(|l| l.worker_id == w.id) {
                let expired = w.updated != 0.
                    && now - w.updated > config.limits.session_timeout
                    && j.retry_of.is_empty();
                let changed = expired || spec.as_ref().map_or(true, |spec| spec != &s.live[i].spec);
                if (changed || s.live[i].retiring || !s.live[i].worker.alive())
                    && !self.close_live(&mut s, i).await?
                {
                    continue;
                }
            }
            if !s.live.iter().any(|l| l.worker_id == w.id) {
                loop {
                    let engaged: HashSet<_> =
                        s.running.values().map(|r| r.worker_id.clone()).collect();
                    let occupied_processes: Vec<_> = s
                        .live
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| {
                            l.spec.machine.name == m.name
                                && (l.worker.alive()
                                    || l.retiring
                                    || engaged.contains(&l.worker_id))
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if occupied_processes.len() < m.max_workers {
                        break;
                    }
                    let victim = occupied_processes.into_iter().find(|i| {
                        let l = &s.live[*i];
                        !engaged.contains(&l.worker_id)
                            && !waiting.contains(&l.worker_id)
                            && !l.worker.busy()
                    });
                    let Some(i) = victim else {
                        continue 'jobs;
                    };
                    if !self.close_live(&mut s, i).await? {
                        continue 'jobs;
                    }
                }
            }
            let Some((job, record)) =
                work::claim(&self.store, j.id.clone(), slot, config.clone(), now).await?
            else {
                continue;
            };
            let spec = match spec {
                Ok(spec) => spec,
                Err(_) => {
                    work::complete(
                        &self.store,
                        job.id,
                        job.attempt,
                        failed("worker_configuration_unavailable", Failure::Refusal),
                        now,
                    )
                    .await?;
                    continue;
                }
            };
            let worker = if let Some(l) = s.live.iter().find(|l| l.worker_id == record.id) {
                l.worker.clone()
            } else {
                match self.factory.create(spec.clone()) {
                    Ok(worker) => {
                        s.live.push(Live {
                            worker_id: record.id.clone(),
                            spec: spec.clone(),
                            worker: worker.clone(),
                            retiring: false,
                        });
                        worker
                    }
                    Err(failure) => {
                        work::complete(
                            &self.store,
                            job.id,
                            job.attempt,
                            Completion {
                                outcome: Err(failure),
                                artifacts: vec![],
                                interrupted: false,
                                stopped: false,
                                allow_retry: false,
                            },
                            now,
                        )
                        .await?;
                        continue;
                    }
                }
            };
            let (control, receiver) = watch::channel(Signal::Run);
            let task = tokio::spawn(run_task(
                Task {
                    store: self.store.clone(),
                    worker,
                    factory: self.factory.clone(),
                    spec,
                    job: job.clone(),
                    record: record.clone(),
                    config: config.clone(),
                    approvals: self.approvals.clone(),
                    io: self.io.clone(),
                    clock: self.clock.clone(),
                    grace: self.stop_grace,
                },
                receiver,
            ));
            s.running.insert(
                job.id.clone(),
                Running {
                    worker_id: record.id.clone(),
                    control,
                    task,
                },
            );
            busy.insert(record.id.clone());
            occupied.insert((record.machine.clone(), slot));
            *assigned.entry((record.machine.clone(), slot)).or_default() +=
                usize::from(record.slot != w.slot);
            *per_machine.entry(record.machine.clone()).or_default() += 1;
            total += 1;
            started.push(job.id);
            records.insert(record.id.clone(), record);
        }
        Ok(started)
    }
    pub async fn processes(&self) -> BTreeMap<String, String> {
        let state = self.state.lock().await;
        state
            .live
            .iter()
            .filter(|l| l.worker.alive())
            .map(|l| {
                let status = if state.running.values().any(|r| r.worker_id == l.worker_id) {
                    "busy"
                } else {
                    "alive"
                };
                (l.worker_id.clone(), status.into())
            })
            .collect()
    }
    pub async fn interrupt(&self, worker_id: &str) -> Result<bool> {
        let mut s = self.state.lock().await;
        self.reap(&mut s).await?;
        let Some(r) = s.running.values().find(|r| r.worker_id == worker_id) else {
            return Ok(false);
        };
        let id = worker_id.to_owned();
        let now = self.clock.now();
        self.store.call(move|c|{
            let tx = c.transaction()?;
            tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'owner','worker.interrupt',?,'{}')",rusqlite::params![now,id])?;
            tx.execute("UPDATE approvals SET status='cancelled',decided_by='system',decided_at=? WHERE worker_id=? AND status='pending'",rusqlite::params![now,id])?;
            tx.commit()?;
            Ok(())
        }).await?;
        r.control.send_replace(Signal::Interrupt);
        Ok(true)
    }
    pub async fn stop(&self, worker_id: &str) -> Result<()> {
        let mut s = self.state.lock().await;
        work::stop(&self.store, worker_id.into(), self.clock.now()).await?;
        if let Some(id) = s
            .running
            .iter()
            .find(|(_, r)| r.worker_id == worker_id)
            .map(|(id, _)| id.clone())
        {
            s.running[&id].control.send_replace(Signal::Stop);
            // The task's adapter operations and close all have deadlines.
            let r = s.running.remove(&id).unwrap();
            let end = r.task.await??;
            if end.closed {
                s.live.retain(|l| l.worker_id != worker_id);
            } else if let Some(l) = s.live.iter_mut().find(|l| l.worker_id == worker_id) {
                l.retiring = true;
            }
        } else if let Some(i) = s.live.iter().position(|l| l.worker_id == worker_id) {
            self.close_live(&mut s, i).await?;
        }
        if s.live
            .iter()
            .any(|l| l.worker_id == worker_id && l.retiring)
        {
            bail!("worker process termination remains unconfirmed");
        }
        Ok(())
    }
    pub async fn settle(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        self.reap(&mut s).await
    }
    pub async fn close(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        s.closing = true;
        for r in s.running.values() {
            r.control.send_replace(Signal::Shutdown);
        }
        let running = std::mem::take(&mut s.running);
        let mut failure = None;
        for (_, r) in running {
            if let Err(e) = r.task.await.context("worker task panicked").and_then(|r| r) {
                failure = Some(e);
            }
        }
        // A failed close must not prevent termination of independent processes.
        for i in (0..s.live.len()).rev() {
            match self.close_live(&mut s, i).await {
                Ok(true) => {}
                Ok(false) => {
                    failure = Some(anyhow::anyhow!(
                        "worker process termination remains unconfirmed"
                    ))
                }
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(())
    }
}
fn make_spec(config: &Config, w: &WorkerRecord) -> Result<WorkerSpec> {
    let m = config.machines.get(&w.machine).context("machine removed")?;
    if m.transport == "slurm" {
        bail!("Slurm execution is unsupported");
    }
    let space = m.workspace(&w.workspace).context("workspace removed")?;
    if !m.backends.contains(&w.backend) {
        bail!("backend removed");
    }
    Ok(WorkerSpec {
        worker_id: w.id.clone(),
        machine: m.for_slot(w.slot),
        workspace: space.for_slot(w.slot),
        backend: w.backend.clone(),
        instructions: String::new(),
        model: String::new(),
        reasoning_effort: String::new(),
        job_timeout: config.limits.job_timeout,
        idle_timeout: config.limits.worker_idle,
        excluded_env: config.secret_env().into_iter().map(str::to_owned).collect(),
        slot: w.slot,
    })
}
pub fn frame(job: &Job, w: &WorkerRecord) -> String {
    let role = if w.role == "general" {
        String::new()
    } else {
        format!(" Your role: {}.", w.role)
    };
    format!(
        "You are worker {} on {}, workspace {}.{} Deliverable: {}.\n\n{}",
        w.id, w.machine, w.workspace, role, job.deliverable, job.brief
    )
}
fn failed(code: &str, kind: Failure) -> Completion {
    Completion {
        outcome: Err(WorkerFailure {
            kind,
            code: code.into(),
            backend_session_id: String::new(),
        }),
        artifacts: vec![],
        interrupted: false,
        stopped: false,
        allow_retry: false,
    }
}
async fn close_worker(worker: &Arc<dyn Worker>, grace: Duration) -> bool {
    matches!(
        tokio::time::timeout(grace, worker.close()).await,
        Ok(Ok(()))
    ) && !worker.alive()
}
struct Task {
    store: Store,
    factory: Arc<dyn Factory>,
    worker: Arc<dyn Worker>,
    spec: WorkerSpec,
    job: Job,
    record: WorkerRecord,
    config: Arc<Config>,
    approvals: Arc<dyn ApprovalHandler>,
    io: Arc<dyn JobIo>,
    clock: Arc<dyn Clock>,
    grace: Duration,
}
async fn run_task(t: Task, mut control: watch::Receiver<Signal>) -> Result<TaskEnd> {
    let stale = t.record.updated != 0.
        && t.clock.now() - t.record.updated > t.config.limits.session_timeout;
    let resume = if stale && t.job.retry_of.is_empty() {
        String::new()
    } else {
        t.record.backend_session_id.clone()
    };
    let in_backend = AtomicBool::new(false);
    let mut operation = Box::pin(async {
        t.factory.admit(t.config.clone(), t.spec.clone()).await?;
        let prepared = t.io.prepare(t.spec.clone(), t.job.clone()).await?;
        let mut brief = frame(&t.job, &t.record);
        brief.push_str(&prepared);
        let request = RunRequest {
            job_id: t.job.id.clone(),
            attempt: t.job.attempt,
            brief,
            resume,
        };
        let payload = json!({"request":request,"spec":t.spec});
        let now = t.clock.now();
        t.store.call(move|c|{c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('worker_call',?,?,0)",rusqlite::params![now,payload.to_string()])?;Ok(())}).await.map_err(|_|WorkerFailure{kind:Failure::Execution,code:"worker_intent_storage_failed".into(),backend_session_id:String::new()})?;
        in_backend.store(true, Ordering::SeqCst);
        let outcome = t
            .worker
            .run(
                request,
                t.record.clone(),
                t.job.clone(),
                t.approvals.clone(),
            )
            .await?;
        if !["done", "partial", "failed", "needs_input"].contains(&outcome.result.status.as_str()) {
            return Err(WorkerFailure {
                kind: Failure::Refusal,
                code: "invalid_worker_result".into(),
                backend_session_id: outcome.backend_session_id,
            });
        }
        let artifacts = match t
            .io
            .collect(t.spec.clone(), outcome.result.artifacts.clone())
            .await
        {
            Ok(artifacts) => artifacts,
            Err(error) => outcome
                .result
                .artifacts
                .iter()
                .cloned()
                .map(|reference| CollectedArtifact {
                    reference,
                    data: None,
                    error: error.code.clone(),
                })
                .collect(),
        };
        Ok((outcome, artifacts))
    });
    let timeout = tokio::time::sleep(Duration::from_secs_f64(t.config.limits.job_timeout));
    tokio::pin!(timeout);
    let (result, signal) = tokio::select! {biased;
        _=control.changed()=>{
            let signal=*control.borrow_and_update();
            let _=tokio::time::timeout(t.grace,t.approvals.cancel(t.record.id.clone())).await;
            let _=tokio::time::timeout(t.grace,t.worker.interrupt()).await;
            let result=if in_backend.load(Ordering::SeqCst) && signal!=Signal::Shutdown{
                tokio::time::timeout(t.grace,&mut operation).await.unwrap_or_else(|_|Err(WorkerFailure{kind:if signal==Signal::Stop{Failure::Cancelled}else{Failure::Interrupted},code:"worker_interrupted".into(),backend_session_id:String::new()}))
            }else{Err(WorkerFailure{kind:if signal==Signal::Stop{Failure::Cancelled}else{Failure::Interrupted},code:"worker_interrupted".into(),backend_session_id:String::new()})};
            (result,signal)
        },
        _=&mut timeout=>{
            let _=tokio::time::timeout(t.grace,t.worker.interrupt()).await;
            (Err(WorkerFailure{kind:Failure::Execution,code:"job_timeout".into(),backend_session_id:String::new()}),Signal::Run)
        },
        result=&mut operation=>(result,Signal::Run),
    };
    drop(operation);
    let retire = t.record.ephemeral || result.is_err() || signal != Signal::Run;
    // Wake adapter-owned approval waiters on timeout/failure as well as controls.
    if retire && signal == Signal::Run {
        let _ = tokio::time::timeout(t.grace, t.approvals.cancel(t.record.id.clone())).await;
    }
    let closed = if retire {
        close_worker(&t.worker, t.grace).await
    } else {
        false
    };
    if retire && !closed {
        record_close_failure(&t.store, &t.record.id, t.clock.now()).await?;
    }
    let (outcome, artifacts) = match result {
        Ok((o, a)) => (Ok(o), a),
        Err(e) => (Err(e), vec![]),
    };
    let completion = Completion {
        outcome,
        artifacts,
        interrupted: signal != Signal::Run,
        stopped: signal == Signal::Stop,
        allow_retry: signal == Signal::Run,
    };
    let finished = work::complete(
        &t.store,
        t.job.id.clone(),
        t.job.attempt,
        completion,
        t.clock.now(),
    )
    .await?;
    if finished == Completed::Stale {
        return Ok(TaskEnd {
            retire: true,
            closed: close_worker(&t.worker, t.grace).await,
        });
    }
    Ok(TaskEnd { retire, closed })
}

async fn record_close_failure(store: &Store, worker_id: &str, now: f64) -> Result<()> {
    let id = worker_id.to_owned();
    store.call(move|c|{
        c.execute("INSERT INTO health_events(kind,details_json,created) VALUES('worker_close_unconfirmed',?,?)",
            rusqlite::params![json!({"worker_id":id}).to_string(),now])?;
        Ok(())
    }).await
}
