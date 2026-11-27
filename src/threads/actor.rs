//! Durable message, owner-instruction, obligation-due and worker-result turns.
//! Parent I/O never holds a database transaction. Version checks fence owner
//! controls arriving during a call; all resulting state effects commit together.
use super::commit::{commit, event_id, Outcome, Rules};
use crate::{
    attention::{self, Capacity},
    config::{Attention, Config},
    core::{
        parent::{Decision, Disposition, HandoffKind, Parent, ParentRequest, ReplyStatus},
        policy::{attention_gate, GateInput},
        time::{Clock, Identifiers},
    },
    store::Shared,
};
use anyhow::{bail, Context, Result};
use fridica_core::store::{
    ParentTurn, Settlement, Store as Backend, TriageSettlement, TurnFailure, TurnInput, TurnRetry,
};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};

#[derive(Debug, PartialEq)]
pub enum Step {
    Idle,
    Observed,
    Deferred,
    Committed,
    Stale,
    Failed,
    Unsupported,
}

/// How an inbox item settles without a parent call.
enum Quiet {
    /// Settle with this verdict; `until` keeps the item pending until then.
    Settle {
        verdict: &'static str,
        until: Option<f64>,
    },
    /// A worker's progress note goes to the thread as a notice.
    Progress,
    /// The thread's external driver acts on it.
    External,
    /// A kind this daemon does not know: kept, and looked at again later.
    Unsupported,
    /// It needs a turn.
    No,
}
/// A turn that will answer: who it answers, and how it is counted.
struct Respond {
    /// `owner`, `human` or `peer`: the class reply limits are kept per.
    class: String,
    turn: u64,
    verdict: String,
    /// The first reply in a thread nobody addressed to the owner.
    unsolicited: bool,
}
/// What the attention gate made of a message.
enum Gate {
    Respond(Respond),
    /// Settled by the gate alone, with this verdict.
    Settle(String),
    /// The triage call failed temporarily: retry the turn later.
    Retry((&'static str, f64)),
    /// Settled by the triage call, with this verdict.
    Triage(String),
}
/// The turns that answer without the attention gate: every kind but a message.
fn respond_without_gate(kind: &str, request: &ParentRequest) -> Option<Respond> {
    let turns = request.session["turns"].as_u64().unwrap_or(0);
    let (class, turn) = match kind {
        // A report continues the turn that started the work.
        "worker_result" | "worker_interrupted" => (
            request.trigger["origin"]["class"]
                .as_str()
                .unwrap_or("human"),
            turns,
        ),
        "owner_instruction" => ("owner", turns.saturating_add(1)),
        // The class of the turn that handed off or whose post was refused,
        // so reply limits still hold.
        "handoff" | "post_refused" => (
            match request.trigger["payload"]["class"].as_str() {
                Some("owner") => "owner",
                Some("human") => "human",
                _ => "peer",
            },
            turns.saturating_add(1),
        ),
        "obligation_due" => (
            if request.trigger["source_peer"] == true {
                "peer"
            } else {
                "human"
            },
            turns.saturating_add(1),
        ),
        _ => return None,
    };
    Some(Respond {
        class: class.into(),
        turn,
        verdict: "respond: owner instruction or attention due".into(),
        unsolicited: false,
    })
}

/// One parent call of a turn, as `parent_turns` keeps it.
pub(super) struct Call {
    /// The request as it was sent.
    pub(super) request: Value,
    pub(super) response: Option<Value>,
    /// The parent's own failure code (e.g. `parent_rate_limited`), never its
    /// output.
    pub(super) failure: Option<String>,
    pub(super) created: f64,
    /// Why the response was not a usable decision.
    pub(super) validation_error: Option<String>,
    /// Why the turn settled without a decision from this call.
    pub(super) settlement_error: Option<&'static str>,
}
impl Call {
    pub(super) fn is_triage(&self) -> bool {
        self.request["call"] == "triage"
    }
    /// The request with the failure code, as the audit keeps it.
    pub(super) fn context(&self) -> Value {
        let mut context = self.request.clone();
        if let Some(code) = &self.failure {
            context["failure"] = json!(code);
        }
        context
    }
    pub(super) fn turn(&self, context: &Value, error: &str, blocked: Option<String>) -> ParentTurn {
        ParentTurn {
            call: self.request["call"].as_str().map(str::to_owned),
            response: json!(self.response).to_string(),
            context: context.to_string(),
            error: error.into(),
            created: json!(self.created).as_f64(),
            blocked,
        }
    }
}

pub struct Actor<P: Parent> {
    /// None supports reply-only operation; delegation requires a validated config.
    pub config: Option<Arc<Config>>,
    pub store: Shared,
    pub parent: Arc<P>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn Identifiers>,
    pub owner: String,
    pub limits: Attention,
    pub observe_only: bool,
    pub parent_timeout: Duration,
    /// Probes machine load before a decision; None keeps placement by job counts.
    pub machine_load: Option<Arc<crate::machines::probe::Monitor>>,
}

impl<P: Parent> Actor<P> {
    /// An actor for a validated configuration: the owner, attention limits and
    /// parent timeout are the configuration's. It acts (not observe-only) and
    /// places by job counts until a caller sets those fields.
    pub fn configured(
        config: Arc<Config>,
        store: Shared,
        parent: Arc<P>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn Identifiers>,
    ) -> Result<Self> {
        Ok(Self {
            owner: config.owner.slack_user.clone(),
            limits: config.attention.clone(),
            parent_timeout: Duration::try_from_secs_f64(config.parent.timeout)?,
            config: Some(config),
            store,
            parent,
            clock,
            ids,
            observe_only: false,
            machine_load: None,
        })
    }
    /// The same actor calling another parent.
    pub fn with_parent<Q: Parent>(self, parent: Arc<Q>) -> Actor<Q> {
        Actor {
            config: self.config,
            store: self.store,
            parent,
            clock: self.clock,
            ids: self.ids,
            owner: self.owner,
            limits: self.limits,
            observe_only: self.observe_only,
            parent_timeout: self.parent_timeout,
            machine_load: self.machine_load,
        }
    }
    /// Attach current machine readings next to `busy` so placement and the parent
    /// see them; they are recorded with the request, keeping replay exact.
    async fn attach_load(&self, request: &mut ParentRequest) -> Result<()> {
        let (Some(config), Some(monitor)) = (&self.config, &self.machine_load) else {
            return Ok(());
        };
        if !config.placement.probe {
            return Ok(());
        }
        let loads = monitor
            .assess(config, &self.store, self.clock.as_ref())
            .await?;
        if let Some(machines) = request.session["machines"].as_array_mut() {
            for machine in machines {
                if let Some(load) = machine["name"].as_str().and_then(|name| loads.get(name)) {
                    machine["load"] = json!(load);
                }
            }
        }
        request.session["work"]["load"] = json!(loads);
        Ok(())
    }
    pub async fn step(&self, session: String) -> Result<Step> {
        if !self.observe_only {
            let pending_session = session.clone();
            if self
                .store
                .transact(move |u| u.worker_control_pending(&pending_session))
                .await?
            {
                return Ok(Step::Deferred);
            }
        }
        let Some((id, kind)) =
            attention::claim_due(&self.store, session.clone(), self.clock.now()).await?
        else {
            return Ok(Step::Idle);
        };
        match self.handle(id, &kind, session.clone()).await {
            Ok(step) => Ok(step),
            Err(_error) => {
                let now = self.clock.now();
                let recorded = self
                    .store
                    .transact(move |u| {
                        let (attempts, state) = u.inbox_attempts(id)?;
                        // Cleaning may discard this turn while an external call is
                        // failing. Its late error must not create fresh signal work.
                        if state != "processing" {
                            return Ok(false);
                        }
                        u.fail_turn(&TurnFailure {
                            id,
                            session,
                            state: if attempts >= 2 { "dropped" } else { "pending" }.into(),
                            not_before: now + 30.,
                            signal: format!("inbox-failed:{id}"),
                            source: json!({"inbox_id":id}).to_string(),
                            details: json!({"inbox_id":id,"attempt":attempts+1}).to_string(),
                            now,
                        })?;
                        Ok(true)
                    })
                    .await?;
                Ok(if recorded { Step::Failed } else { Step::Stale })
            }
        }
    }

    async fn handle(&self, id: i64, kind: &str, session: String) -> Result<Step> {
        let origin = self
            .config
            .as_ref()
            .map_or(crate::config::overleaf::DEFAULT_ORIGIN, |config| {
                config.overleaf.origin()
            });
        let mut request = load(
            &self.store,
            id,
            session.clone(),
            self.owner.clone(),
            origin.to_owned(),
        )
        .await?;
        if let Some(config) = &self.config {
            let busy = serde_json::from_value(request.session["work"]["busy"].clone())?;
            request.session["machines"] = json!(config.machines.payload(&busy));
            if crate::github::mcp::available(config) {
                request.session["github_reviews"] = json!(true);
            }
        }
        // Read after the turn's input: a driver change bumps the thread's
        // version, so a turn that read the old driver commits nothing.
        let lookup = session.clone();
        let external = self
            .store
            .transact(move |u| u.thread_driver(&lookup))
            .await?
            == "external";
        if external {
            // Only an externally driven thread's request says so, so a
            // parent-driven thread's recorded requests stay as they were.
            request.session["driver"] = json!("external");
        }
        match self.quiet(kind, external, &request) {
            Quiet::Settle { verdict, until } => {
                return settle(&self.store, id, &request, verdict.into(), until).await
            }
            Quiet::Progress => return super::progress::handle(self, request).await,
            Quiet::External => return settle_external(&self.store, id, &request).await,
            Quiet::Unsupported => {
                let until = Some(self.clock.now() + 60.);
                let outcome = settle(&self.store, id, &request, String::new(), until).await?;
                return Ok(if outcome == Step::Observed {
                    Step::Unsupported
                } else {
                    outcome
                });
            }
            Quiet::No => {}
        }
        let mut calls = Vec::new();
        let respond = match respond_without_gate(kind, &request) {
            Some(respond) => respond,
            None => match self.gate(id, &session, &mut request, &mut calls).await? {
                Gate::Respond(respond) => respond,
                Gate::Settle(verdict) => {
                    return settle(&self.store, id, &request, verdict, None).await
                }
                Gate::Retry(temporary) => {
                    return self
                        .retry_later(id, session, request, calls, temporary)
                        .await
                }
                Gate::Triage(verdict) => {
                    let now = self.clock.now();
                    return settle_triage(&self.store, id, session, request, verdict, calls, now)
                        .await;
                }
            },
        };
        if let Capacity::Deferred(_) = attention::reserve(
            &self.store,
            session.clone(),
            id,
            respond.class,
            self.clock.now(),
            self.ids.next("reply"),
            self.limits.clone(),
        )
        .await?
        {
            return Ok(Step::Deferred);
        }
        let reply_limit = self.config.as_ref().map_or(7000, |c| c.limits.reply_chars);
        let worker_result = matches!(kind, "worker_result" | "worker_interrupted");
        let mut result = if worker_result {
            super::results::direct(
                &request,
                self.config
                    .as_ref()
                    .is_some_and(|c| c.limits.report_fast_path),
                reply_limit,
            )?
        } else {
            None
        };
        if result.is_none() && worker_result {
            if let Some(scope) =
                delegation_scope(self.config.as_deref(), &request).filter(|s| s.allowed)
            {
                result = super::results::recover_local(&self.store, scope, &mut request).await?;
            }
        }
        let decision = match result {
            Some(decision) => decision,
            None => {
                self.attach_load(&mut request).await?;
                match self.decide(&mut request, &mut calls).await? {
                    Ok(decision) => decision,
                    Err(temporary) => {
                        return self
                            .retry_later(id, session, request, calls, temporary)
                            .await
                    }
                }
            }
        };
        let work = super::delegation::prepare(
            &decision,
            &request,
            delegation_scope(self.config.as_deref(), &request),
            Some(self.ids.as_ref()),
        )?;
        commit(
            &self.store,
            id,
            session,
            Outcome {
                request,
                decision,
                work,
                calls,
                verdict: respond.verdict,
                turn: respond.turn,
                cooldown: if respond.unsolicited {
                    self.config.as_ref().map(|c| c.slack.cooldown)
                } else {
                    None
                },
            },
            Rules {
                owner: self.owner.clone(),
                now: self.clock.now(),
                signal: self.limits.streak_signal,
                reply_limit,
            },
        )
        .await
    }
    /// Whether the item settles without a parent call, and how. The checks
    /// run in this order; the first that applies decides.
    fn quiet(&self, kind: &str, external: bool, request: &ParentRequest) -> Quiet {
        let worker_result = matches!(kind, "worker_result" | "worker_interrupted");
        let now = self.clock.now();
        if self.observe_only || request.session["control"] != "active" {
            return Quiet::Settle {
                verdict: if self.observe_only {
                    "observe: observe-only mode"
                } else {
                    "observe: thread is paused or closed"
                },
                until: worker_result.then_some(now + 60.),
            };
        }
        if kind == "debrief" {
            // Drain work queued by an older daemon without a parent call or post.
            return Quiet::Settle {
                verdict: "observe: debrief feature removed",
                until: None,
            };
        }
        if kind == "worker_progress" {
            return Quiet::Progress;
        }
        // An external driver (fridica#130) reads worker results and study
        // posts on the event feed (`job_result`, `peer_post`) and acts on
        // them itself: they are settled here, with no parent call and no
        // reply. Anyone else addressing the owner still gets a parent turn,
        // which may not delegate.
        if external
            && (worker_result
                || (kind == "message"
                    && request.trigger["message"]["meta"]["kind"]
                        .as_str()
                        .is_some_and(|k| k.starts_with("study_"))))
        {
            return Quiet::External;
        }
        if worker_result
            && (request.trigger["pending"] == true
                || request.trigger["results"]
                    .as_array()
                    .is_none_or(|r| r.is_empty()))
        {
            return Quiet::Settle {
                verdict: "observe: group still working or already reported",
                until: None,
            };
        }
        if !matches!(
            kind,
            "message"
                | "owner_instruction"
                | "obligation_due"
                | "worker_result"
                | "worker_interrupted"
                | "handoff"
                | "post_refused"
        ) {
            return Quiet::Unsupported;
        }
        if kind == "obligation_due" {
            let ref_id = request.trigger["ref"].as_str().unwrap_or("");
            if !request.obligations.iter().any(|o| {
                o["id"] == ref_id
                    && matches!(o["state"].as_str(), Some("open" | "deferred"))
                    && o["due"].as_f64().is_some_and(|due| due <= now)
            }) {
                return Quiet::Settle {
                    verdict: "observe: obligation no longer due",
                    until: None,
                };
            }
            if request.session["parent_review_required"] == true {
                return Quiet::Settle {
                    verdict: "observe: parent failure needs owner review",
                    until: Some(now + 60.),
                };
            }
        }
        Quiet::No
    }
    /// The attention gate for a message, with the triage call when the gate
    /// asks for one.
    async fn gate(
        &self,
        id: i64,
        session: &str,
        request: &mut ParentRequest,
        calls: &mut Vec<Call>,
    ) -> Result<Gate> {
        let m = &request.trigger["message"];
        let gate = GateInput {
            owner: self.owner.clone(),
            text: m["text"].as_str().unwrap_or("").into(),
            sender: m["sender"].as_str().unwrap_or("").into(),
            generated: !m["meta"].is_null(),
            meta_kind: m["meta"]["kind"].as_str().unwrap_or("").into(),
            meta_status: m["meta"]["status"].as_str().unwrap_or("").into(),
            peer_turn: m["meta"]["turn"].as_u64().unwrap_or(0),
            control: "active".into(),
            status: request.session["status"].as_str().unwrap_or("new").into(),
            turns: request.session["turns"].as_u64().unwrap_or(0),
            reset_at: request.session["reset_at"].as_f64().unwrap_or(0.),
            ts: m["ts"].as_str().unwrap_or("0").parse()?,
            general_messages: self
                .config
                .as_ref()
                .is_some_and(|c| c.slack.general_messages),
            cooling: request.session["last_unsolicited"]
                .as_f64()
                .is_some_and(|last| {
                    self.config
                        .as_ref()
                        .is_some_and(|c| self.clock.now() - last < c.slack.cooldown)
                }),
            observe_only: false,
            resumed: request.trigger["payload"]["resumed"] == true,
        };
        // Addressed messages in a blocked thread reach the parent so it can
        // decide whether the new instruction actually resolves the blocker.
        let outcome = attention_gate(&gate, gate.text.contains(&format!("<@{}>", self.owner)));
        let mut verdict = format!("{}: {}", outcome.kind, outcome.reason);
        if matches!(outcome.kind.as_str(), "ignore" | "observe") {
            return Ok(Gate::Settle(verdict));
        }
        let mut unsolicited = false;
        if outcome.kind == "triage" {
            request.call = "triage".into();
            let (raw, call) = self.call(request).await?;
            calls.push(call);
            if let Some(temporary) = temporary(calls) {
                return Ok(Gate::Retry(temporary));
            }
            let choice = raw
                .as_ref()
                .and_then(|r| r.as_object())
                .filter(|r| r.len() == 1)
                .and_then(|r| r.get("decision"))
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "respond" | "observe" | "ignore"))
                .unwrap_or("observe");
            if choice != "respond" {
                return Ok(Gate::Triage(format!("{choice}: triage")));
            }
            // Avoid another external call after a pause/resume changed the
            // decision context while triage was in flight.
            if !current(&self.store, id, session, request).await? {
                return Ok(Gate::Triage("observe: stale triage".into()));
            }
            verdict = format!("respond: {}", outcome.reason);
            unsolicited = gate.turns == 0;
        }
        Ok(Gate::Respond(Respond {
            class: if request.trigger["payload"]["owner_trigger"] == true {
                "owner"
            } else if gate.generated {
                "peer"
            } else {
                "human"
            }
            .into(),
            turn: outcome.turn,
            verdict,
            unsolicited,
        }))
    }
    /// Ask the parent for the turn's decision, and once more with the reason
    /// if its first answer is not a valid one. A failed or twice-invalid
    /// parent yields the blocked stand-in; a temporary failure is returned
    /// for the turn to be retried later.
    async fn decide(
        &self,
        request: &mut ParentRequest,
        calls: &mut Vec<Call>,
    ) -> Result<std::result::Result<Decision, (&'static str, f64)>> {
        for round in 0..2 {
            request.call = if round == 0 { "decide" } else { "repair" }.into();
            let (raw, mut call) = self.call(request).await?;
            let now = self.clock.now();
            if let Some(temporary) = temporary(std::slice::from_ref(&call)) {
                calls.push(call);
                return Ok(Err(temporary));
            }
            let Some(raw) = raw else {
                call.settlement_error = Some("parent_unavailable");
                calls.push(call);
                return Ok(Ok(super::failure::blocked(false)));
            };
            match validate(&raw, request, now).and_then(|mut d| {
                if let Err(question) = super::overleaf::check(&mut d, request) {
                    return Ok(super::overleaf::clarify(question));
                }
                super::delegation::prepare(
                    &d,
                    request,
                    delegation_scope(self.config.as_deref(), request),
                    None,
                )?;
                Ok(d)
            }) {
                Ok(decision) => {
                    calls.push(call);
                    return Ok(Ok(decision));
                }
                Err(error) => {
                    // Kept with the call, so both the decide and the repair
                    // errors are in parent_turns.
                    call.validation_error = Some(error.to_string());
                    request.previous = Some(raw);
                    request.errors = vec![error.to_string()];
                    if round == 1 {
                        call.settlement_error = Some("parent_invalid_after_repair");
                    }
                    calls.push(call);
                }
            }
        }
        Ok(Ok(super::failure::blocked(true)))
    }
    /// The parent hit its backend's usage limit (#107). That is temporary:
    /// the item stays pending and is retried after `failure::RATE_LIMIT_RETRY`, the
    /// thread is neither blocked nor marked for owner review, and nothing is
    /// posted. The call is kept in `parent_turns` with its error.
    async fn retry_later(
        &self,
        id: i64,
        session: String,
        request: ParentRequest,
        calls: Vec<Call>,
        (code, delay): (&'static str, f64),
    ) -> Result<Step> {
        let now = self.clock.now();
        // The whole turn is redone later, so only the refused call and any
        // triage are kept: an earlier decide that failed validation must not
        // read as a later, successful one (it would clear owner review).
        let calls = calls
            .iter()
            .filter(|c| c.failure.as_deref() == Some(code) || c.is_triage())
            .map(|call| {
                let error = if call.failure.as_deref() == Some(code) {
                    code
                } else {
                    ""
                };
                call.turn(&call.context(), error, None)
            })
            .collect();
        let retry = TurnRetry {
            id,
            session,
            version: request.session["version"].as_i64(),
            calls,
            retry_at: now + delay,
            details: json!({"inbox_id":id,"retry_at":now+delay}).to_string(),
            now,
        };
        let current = self.store.transact(move |u| u.retry_turn(&retry)).await?;
        Ok(if current { Step::Deferred } else { Step::Stale })
    }
    pub(super) async fn call(&self, request: &ParentRequest) -> Result<(Option<Value>, Call)> {
        let started = self.clock.now();
        let encoded = serde_json::to_string(request)?;
        let call_id = self
            .store
            .transact(move |u| u.record("parent_call", started, &encoded, false))
            .await?;
        let timeout = self.parent_timeout.saturating_add(
            self.parent
                .preparation_timeout(request)
                .min(Duration::from_secs(30)),
        );
        let response = tokio::time::timeout(timeout, self.parent.decide(request.clone())).await;
        let (raw, error, failure) = match response {
            Ok(Ok(value)) => (Some(value), None, None),
            Ok(Err(failure)) => (None, Some("parent_unavailable"), Some(failure)),
            Err(_) => (None, Some("parent_timeout"), None),
        };
        let now = self.clock.now();
        let outcome = json!({"call_id":call_id,"response":raw,"error":error,"failure":failure});
        self.store
            .transact(move |u| {
                u.complete(call_id, true)?;
                u.record("parent_result", now, &outcome.to_string(), true)?;
                Ok(())
            })
            .await?;
        if failure
            .as_ref()
            .is_some_and(super::failure::prevents_effects)
        {
            bail!("parent evidence recording or context boundary failed");
        }
        let call = Call {
            request: serde_json::to_value(request)?,
            response: raw.clone(),
            failure: failure.map(|f| f.code),
            created: now,
            validation_error: None,
            settlement_error: None,
        };
        Ok((raw, call))
    }
}

/// Characters of the source thread's state a hand-off carries.
pub(super) const HANDOFF_CONTEXT: usize = 6000;
/// A turn started by a hand-off of this many hops hands off no further.
const MAX_HANDOFF_HOPS: u64 = 2;
pub(super) fn handoff_hop(request: &ParentRequest) -> u64 {
    if request.trigger["kind"] == "handoff" {
        request.trigger["payload"]["hop"].as_u64().unwrap_or(1)
    } else {
        0
    }
}
/// A temporary failure of the last call, with its code and how long until the
/// turn is retried: the backend's usage limit (#107), or a lost refresh of
/// its login. Neither blocks the thread or asks for owner review.
fn temporary(calls: &[Call]) -> Option<(&'static str, f64)> {
    match calls.last()?.failure.as_deref()? {
        super::failure::RATE_LIMITED => Some((
            super::failure::RATE_LIMITED,
            super::failure::RATE_LIMIT_RETRY,
        )),
        crate::parent::AUTH_CONTENDED => {
            Some((crate::parent::AUTH_CONTENDED, crate::parent::AUTH_RETRY))
        }
        _ => None,
    }
}

async fn load(
    store: &impl Backend,
    id: i64,
    session: String,
    owner: String,
    origin: String,
) -> Result<ParentRequest> {
    store.transact(move|u| {
        let TurnInput{thread:data,kind,reference,payload,message,from_peer,history,obligations,review_required:review}=u.turn_input(&session,id)?;
        let mut trigger=json!({"kind":kind,"ref":reference,"payload":serde_json::from_str::<Value>(&payload)?});
        if let Some(raw)=message {
            trigger["message"]=serde_json::from_str(&raw)?;
        }
        if kind=="post_refused" {
            // The refused text itself: it never became a message, so the
            // history does not have it.
            let raw=u.refused_post(&session,reference.parse::<i64>().unwrap_or(0))?;
            trigger["refused"]=raw.map(|r:String|serde_json::from_str::<Value>(&r)).transpose()?.unwrap_or(Value::Null);
        }
        if let Some(peer)=from_peer {
            trigger["source_peer"]=json!(peer);
        }
        let mut session_data:Value=serde_json::from_str(&data)?;
        session_data["parent_review_required"]=json!(review);
        if session_data["turns"] == 0 {
            let recent=u.earlier_roots(session_data["workspace"].as_str(),session_data["channel"].as_str(),session_data["root_ts"].as_str())?;
            let recent:Vec<Value>=recent.iter().rev().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?;
            session_data["channel_context"]=json!(crate::parent::context::bounded(&recent,4000));
        }
        let (revision, notes)=super::effects::notes(u,&session)?;
        session_data["notes"]=json!({"revision":revision,"data":notes});
        let (decisions,debriefed)=u.turn_decisions(&session)?;
        session_data["decisions"]=serde_json::from_str(&decisions)?;
        session_data["debriefed_turn"]=json!(debriefed);
        session_data["work"]=u.work_context(&session)?;
        let linked=super::linked::views(u,&session)?;
        if !linked.is_empty() {session_data["linked_threads"]=json!(linked);}
        let files=u.delegable_files(&session)?;
        if !files.is_empty() {session_data["files"]=json!(files);}
        // Posts of this thread that did not reach Slack: refused by the egress
        // gate (failed, with the rule's code) or of unknown fate (ambiguous).
        // Codes only, never the text, so the term does not spread (#119).
        let undelivered=u.undelivered_posts(&session)?;
        if !undelivered.is_empty() {session_data["undelivered"]=undelivered.iter().map(|s|serde_json::from_str(s)).collect::<std::result::Result<Vec<Value>,_>>()?.into();}
        let last=u.last_unsolicited(session_data["workspace"].as_str(),session_data["channel"].as_str())?;
        session_data["last_unsolicited"]=json!(last);
        if matches!(kind.as_str(),"worker_result"|"worker_interrupted") {
            let snapshot=super::results::load(u,&session,&reference)?;
            for (key,value) in snapshot.as_object().context("invalid result snapshot")? {trigger[key]=value.clone();}
        }
        let mut result=ParentRequest{github_state:vec![],linked:vec![],inbox_id:id,call:"decide".into(),session:session_data,trigger,
            history:history.iter().rev().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,
            obligations:obligations.iter().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,previous:None,errors:vec![]};
        super::overleaf::attach(u, &mut result, &owner, &origin)?;
        Ok(result)
    }).await
}

/// Whether a valid decision ends the owner-review hold a failed parent call
/// left on the thread: the hold stands only while the parent blocks again.
pub(super) fn hold_clears(request: &ParentRequest, decision: &Decision) -> bool {
    request.session["parent_review_required"] == true
        && !decision
            .reply
            .as_ref()
            .is_some_and(|r| matches!(r.status, ReplyStatus::Blocked))
}

fn validate(raw: &Value, request: &ParentRequest, now: f64) -> Result<Decision> {
    let mut decision: Decision =
        serde_json::from_value(raw.clone()).context("invalid parent response schema")?;
    super::effects::validate(&mut decision)?;
    if let Some(reply) = &mut decision.reply {
        reply.text = reply
            .text
            .trim_matches(crate::core::render::whitespace)
            .to_owned();
        reply.details = reply
            .details
            .trim_matches(crate::core::render::whitespace)
            .to_owned();
        if !matches!(reply.status, ReplyStatus::Complete) {
            reply.discussion = crate::core::parent::Discussion::Ongoing;
        }
        if reply.text.is_empty() && reply.details.is_empty() {
            reply.send = false;
        }
    }
    if decision.reply.as_ref().is_some_and(|r| !r.send) {
        if decision
            .reply
            .as_ref()
            .is_some_and(|r| !r.answers.is_empty())
        {
            bail!("an unsent reply cannot answer obligations");
        }
        decision.reply = None;
    }
    // A thread still blocked by the stand-in for a failed parent call (#141):
    // nobody decided that block, so this successful turn reopens it unless
    // the parent blocks again on its own. Without this the parent reads the
    // stand-in note as the owner's hold and repeats it, or trips the
    // explicit-decision rule below and settles blocked once more.
    if hold_clears(request, &decision) && request.session["status"] == "blocked" {
        decision.reopen_blocked = true;
    }
    let mut addressed = HashSet::new();
    let known: HashSet<&str> = request
        .obligations
        .iter()
        .filter(|o| {
            matches!(o["state"].as_str(), Some("open" | "deferred"))
                || (o["state"] == "awaiting_delivery"
                    && o["deliveries"]
                        .as_array()
                        .is_some_and(|d| !d.is_empty() && d.iter().all(|p| p["state"] == "failed")))
        })
        .filter_map(|o| o["id"].as_str())
        .collect();
    if let Some(reply) = &decision.reply {
        if request.session["status"] == "blocked"
            && !decision.reopen_blocked
            && !matches!(reply.status, ReplyStatus::Blocked)
        {
            bail!("reopening a blocked discussion requires an explicit decision");
        }
        if reply.text.chars().count() > 40000 || reply.details.chars().count() > 40000 {
            bail!("reply or details exceed configured bounds");
        }
        if (matches!(reply.status, ReplyStatus::Blocked)
            || (request.session["status"] == "blocked" && !decision.reopen_blocked))
            && !reply.answers.is_empty()
        {
            bail!("blocked notices do not satisfy obligations");
        }
        for id in &reply.answers {
            if !known.contains(id.as_str()) || !addressed.insert(id.as_str()) {
                bail!("unknown or duplicate answer obligation");
            }
        }
    }
    for disposition in &decision.dispositions {
        if !known.contains(disposition.id()) || !addressed.insert(disposition.id()) {
            bail!("unknown or duplicate obligation disposition");
        }
        match disposition {
            Disposition::Declined { reason, .. } | Disposition::Deferred { reason, .. }
                if reason.trim().is_empty() =>
            {
                bail!("disposition needs a visible reason")
            }
            Disposition::Deferred { until, .. } if !until.is_finite() || *until <= now => {
                bail!("deferral must be in the future")
            }
            _ => {}
        }
    }
    for ask in &decision.asks {
        if ask.summary.trim().is_empty() || !ask.due.is_finite() || ask.due < now {
            bail!("invalid extracted ask");
        }
    }
    // Hand-offs (#108) go only to linked threads, at most once per thread, and
    // a turn a hand-off started never hands back to where it came from.
    // A paused thread cannot act on a hand-off, so none goes there.
    let linked: HashSet<&str> = request.session["linked_threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["control"] == "active")
        .filter_map(|t| t["thread"].as_str())
        .collect();
    let came_from = (request.trigger["kind"] == "handoff")
        .then(|| request.trigger["payload"]["from"].as_str())
        .flatten();
    let mut targets = HashSet::new();
    if decision.handoffs.len() > 3 {
        bail!("at most 3 hand-offs per turn");
    }
    if !decision.handoffs.is_empty() && handoff_hop(request) >= MAX_HANDOFF_HOPS {
        bail!("a chain of hand-offs stops after {MAX_HANDOFF_HOPS} hops");
    }
    for handoff in &decision.handoffs {
        if !linked.contains(handoff.thread.as_str()) || !targets.insert(handoff.thread.as_str()) {
            bail!("a hand-off must target a distinct, active linked thread");
        }
        if came_from == Some(handoff.thread.as_str()) {
            bail!("a hand-off cannot go back to the thread it came from");
        }
        let note = handoff.note.trim();
        if note.is_empty() || note.chars().count() > 2000 {
            bail!("a hand-off needs a note of at most 2000 characters");
        }
        if !handoff.answers.is_empty() && handoff.kind != HandoffKind::Post {
            bail!("only a post hand-off can answer obligations");
        }
        for id in &handoff.answers {
            if !known.contains(id.as_str()) || !addressed.insert(id.as_str()) {
                bail!("unknown or duplicate hand-off obligation");
            }
        }
    }
    Ok(decision)
}

/// Quiet/paused decisions need the same version fence as model decisions.
/// Otherwise a late observation can consume an owner-resumed inbox item, or
/// resurrect an item which a concurrent clean deliberately dropped.
pub(super) async fn settle(
    store: &impl Backend,
    id: i64,
    request: &ParentRequest,
    verdict: String,
    until: Option<f64>,
) -> Result<Step> {
    let settlement = settlement(id, request, verdict, until)?;
    let current = store.transact(move |u| u.settle_turn(&settlement)).await?;
    Ok(if current { Step::Observed } else { Step::Stale })
}
/// A settlement under the version the request was read at.
fn settlement(
    id: i64,
    request: &ParentRequest,
    verdict: String,
    until: Option<f64>,
) -> Result<Settlement> {
    Ok(Settlement {
        id,
        session: request.session["id"]
            .as_str()
            .context("missing session ID")?
            .to_owned(),
        version: request.session["version"]
            .as_i64()
            .context("missing session version")?,
        event: event_id(request).map(str::to_owned),
        verdict,
        until,
    })
}

/// Settle an externally driven thread's worker result or study post: the
/// item finishes under the same version fence as any settlement, and the
/// finished jobs it carries count as reported, all in one unit of work.
async fn settle_external(store: &impl Backend, id: i64, request: &ParentRequest) -> Result<Step> {
    let settlement = settlement(id, request, "observe: external driver".into(), None)?;
    let session = settlement.session.clone();
    let jobs: Vec<Option<String>> = request.trigger["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| !matches!(r["job_status"].as_str(), Some("queued" | "running")))
        .map(|r| r["id"].as_str().map(str::to_owned))
        .collect();
    let current = store
        .transact(move |u| {
            let current = u.settle_turn(&settlement)?;
            if current && !jobs.is_empty() {
                u.mark_reported(&session, &jobs)?;
            }
            Ok(current)
        })
        .await?;
    Ok(if current { Step::Observed } else { Step::Stale })
}

/// Startup recovery keeps unsent reservations reusable by the same inbox item.
pub async fn recover(store: &impl Backend) -> Result<usize> {
    store.transact(|u| u.recover_turns()).await
}

async fn current(
    store: &impl Backend,
    id: i64,
    session: &str,
    request: &ParentRequest,
) -> Result<bool> {
    let session = session.to_owned();
    let version = request.session["version"].as_i64();
    store
        .transact(move |u| u.turn_live(&session, version, id))
        .await
}
async fn settle_triage(
    store: &impl Backend,
    id: i64,
    session: String,
    request: ParentRequest,
    verdict: String,
    calls: Vec<Call>,
    now: f64,
) -> Result<Step> {
    let triage = TriageSettlement {
        id,
        session,
        version: request.session["version"].as_i64(),
        calls: calls
            .into_iter()
            .map(|call| ParentTurn {
                call: Some("triage".into()),
                ..call.turn(&call.request, "", None)
            })
            .collect(),
        event: event_id(&request).map(str::to_owned),
        verdict,
        now,
    };
    store
        .transact(move |u| {
            if !u.settle_triage(&triage)? {
                return Ok(Step::Stale);
            }
            u.record(
                "triage_commit",
                now,
                &json!({"inbox_id":id,"verdict":triage.verdict}).to_string(),
                true,
            )?;
            Ok(Step::Observed)
        })
        .await
}

/// What this thread's turn may delegate to: a channel must be configured and
/// allowed to delegate.
fn delegation_scope<'a>(
    config: Option<&'a Config>,
    request: &ParentRequest,
) -> Option<super::delegation::Scope<'a>> {
    config.map(|config| {
        let channel = request.session["channel"].as_str().unwrap_or("");
        super::delegation::Scope {
            // An externally driven thread's work is its driver's to start.
            allowed: config.slack.channels.iter().any(|c| c == channel)
                && config.slack.may_delegate(channel)
                && request.session["driver"] != "external",
            limits: &config.limits,
            machines: &config.machines,
            roles: crate::config::roles::worker_roles(),
        }
    })
}
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::{
        attention::Message,
        core::Authority,
        threads::controls::{self, Control},
    };
    use fridica_core::store::InboxEntry;

    #[tokio::test]
    async fn stale_observation_or_deferral_cannot_consume_resume_or_undo_clean() {
        for until in [None, Some(100.)] {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::store::Store::open(dir.path().join("db"))
                .await
                .unwrap();
            let session = "TTEAM:CROOM:100.1".to_owned();
            let id = attention::intake(
                &store,
                Message {
                    event_id: "event".into(),
                    workspace: "TTEAM".into(),
                    channel: "CROOM".into(),
                    ts: "100.1".into(),
                    thread_ts: None,
                    sender: "UALICE".into(),
                    text: "<@UOWNER> help".into(),
                    files: vec![],
                    source: "socket".into(),
                    meta: None,
                    attachments: vec![],
                },
                "UOWNER".into(),
                20.,
                900.,
                "ask".into(),
            )
            .await
            .unwrap()
            .unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Pause {
                    reason: "Hold".into(),
                },
                Authority::Owner,
                20.,
            )
            .await
            .unwrap();
            attention::claim_due(&store, session.clone(), 20.)
                .await
                .unwrap()
                .unwrap();
            let paused = load(
                &store,
                id,
                session.clone(),
                String::new(),
                crate::config::overleaf::DEFAULT_ORIGIN.into(),
            )
            .await
            .unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Resume,
                Authority::Owner,
                21.,
            )
            .await
            .unwrap();
            assert_eq!(
                settle(&store, id, &paused, "observe: paused".into(), until)
                    .await
                    .unwrap(),
                Step::Stale
            );
            let inbox = session.clone();
            let (state, InboxEntry { payload, .. }) = store
                .transact(move |u| {
                    let (_, state) = u.inbox_attempts(id)?;
                    Ok((state, u.inbox_entry(id, &inbox)?.context("inbox item")?))
                })
                .await
                .unwrap();
            assert_eq!(state, "pending");
            assert_eq!(
                serde_json::from_str::<Value>(&payload).unwrap()["owner_trigger"],
                true
            );
            // Claimable at time 0: the stale settlement deferred nothing.
            let claimed = attention::claim_due(&store, session.clone(), 0.)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(claimed.0, id);
            let before_clean = load(
                &store,
                id,
                session.clone(),
                String::new(),
                crate::config::overleaf::DEFAULT_ORIGIN.into(),
            )
            .await
            .unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Clean,
                Authority::Owner,
                22.,
            )
            .await
            .unwrap();
            controls::apply(&store, session, Control::Restore, Authority::Owner, 23.)
                .await
                .unwrap();
            assert_eq!(
                settle(
                    &store,
                    id,
                    &before_clean,
                    "observe: old input".into(),
                    until
                )
                .await
                .unwrap(),
                Step::Stale
            );
            let (_, state) = store.transact(move |u| u.inbox_attempts(id)).await.unwrap();
            assert_eq!(state, "dropped");
        }
    }
}
