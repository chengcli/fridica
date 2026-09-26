"""One thread's actor: drains its inbox strictly in order and applies each outcome atomically.

Threads run in parallel; within a thread everything is serial. An inbox item's
effects (posts, jobs, workers, session changes, parent-call records, and the item
leaving the inbox) commit in one transaction, so a crash either leaves the item
to be retried from scratch or fully applied, never half done.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, replace
import logging
import os
from typing import Any, Protocol
import uuid

from ..config.schema import Config
from ..core.models import FridicaMeta, InboxItem, Job, OutboxItem, ThreadSession, WorkerRecord
from ..parent.actions import Action, Reply, Rules
from ..parent.agent import ParentUnavailable, unavailable_reply
from ..slack import links, render
from ..store import StaleSession, Store
from . import context as contexts
from . import policy

logger = logging.getLogger(__name__)
INSPECTION_NOTICE = "An earlier task in this thread needs a local look before I can continue. I haven't retried it."
DEBRIEF_HEADER = "Debrief: this discussion is finished.\n\n"
INTERRUPTED = "One of the jobs I started for this thread was interrupted by a restart and was not resumed."
RESUME_MARGIN = 1e-6
GITHUB_WAIT = 20.0
ATTEMPTS = 3


class Runtime(Protocol):
    """What an actor needs from the daemon."""
    config: Config
    store: Store
    bus: Any
    clock: Any
    parent: Any
    slack: Any
    supervisor: Any
    parent_slots: Any
    observe_only: bool
    github: Any

    def repositories(self) -> tuple[dict, ...]: ...

    def spawn(self, coroutine) -> None: ...


@dataclass(frozen=True)
class Outcome:
    """Everything one inbox item changes, applied in one transaction by ``commit``."""
    session: ThreadSession
    posts: tuple[OutboxItem, ...] = ()
    workers: tuple[WorkerRecord, ...] = ()
    jobs: tuple[Job, ...] = ()
    reported: tuple[str, ...] = ()
    note: dict | None = None
    verdict: tuple[str, str] | None = None
    cooldown: bool = False
    calls: tuple[dict, ...] = ()
    action: dict | None = None
    follow_ups: tuple[tuple[str, str, dict], ...] = ()
    """Inbox items to add: (kind, ref, payload)."""
    wipe: bool = False
    """Erase the thread's message text (the owner's clean action)."""


class ThreadActor:
    def __init__(self, runtime: Runtime, session_id: str):
        self.rt = runtime
        self.session_id = session_id

    @property
    def store(self) -> Store:
        return self.rt.store

    @property
    def config(self) -> Config:
        return self.rt.config

    async def run(self) -> None:
        """Process pending items until the inbox is empty."""
        while True:
            item = self.store.inbox.claim(self.session_id)
            if item is None:
                return
            try:
                await self.handle(item)
            except Exception as error:
                # An uncommitted item gets a few more attempts (a transient database error or a stale session
                # usually clears); a committed one is left alone. Persisting the count stops endless retries.
                if isinstance(error, StaleSession):
                    logger.info("thread %s changed while item %s was processed", self.session_id, item.id)
                else:
                    logger.exception("could not process %s item %s in thread %s", item.kind, item.id, self.session_id)
                state = self.store.inbox.retry_or_drop(item.id, ATTEMPTS)
                if state == "dropped":
                    logger.error("giving up on %s item %s in thread %s after %d attempts", item.kind, item.id,
                                 self.session_id, ATTEMPTS)
                    self.store.audit.record("fridica", "inbox.dropped", self.rt.clock.now(), target=self.session_id,
                                            details={"item": item.id, "kind": item.kind})
                elif state == "pending":
                    return  # stop this pass; the manager's sweep retries the item shortly

    async def handle(self, item: InboxItem) -> None:
        handler = {"message": self.on_message, "worker_result": self.on_worker, "worker_interrupted": self.on_worker,
                   "control": self.on_control, "owner_instruction": self.on_owner_instruction,
                   "debrief": self.on_debrief}.get(item.kind)
        if handler is None:
            self.store.inbox.finish(item.id, "dropped")
            return
        await handler(item)

    # ----- committing -----

    def commit(self, item: InboxItem, outcome: Outcome) -> ThreadSession:
        now = self.rt.clock.now()
        with self.store.transaction():
            for call in outcome.calls:
                self.store.parent_turns.add(self.session_id, item.id, call, now,
                                            action=outcome.action if call.get("call") in ("decide", "repair") else None)
            for worker in outcome.workers:
                self.store.workers.add(worker, now)
            for job in outcome.jobs:
                self.store.jobs.add(job, now)
                self.store.workers.set_status(job.worker_id, "queued", now)
            if outcome.reported:
                self.store.jobs.mark_reported(list(outcome.reported))
            for post in outcome.posts:
                if not self.store.outbox.enqueue(post, now):
                    logger.warning("thread %s: post %s was already queued; not posting it twice", self.session_id,
                                   post.idem_key)
            if outcome.note:
                _, current = self.store.notes.current(self.session_id)
                merged = {**current, **outcome.note}
                if merged != current:
                    self.store.notes.write(self.session_id, merged, "parent", now, source=str(item.id))
            if outcome.verdict and item.kind == "message":
                self.store.messages.set_verdict(item.ref, f"{outcome.verdict[0]}: {outcome.verdict[1]}")
            if outcome.cooldown:
                self.store.cooldowns.mark(outcome.session.key.workspace, outcome.session.key.channel, now)
            for kind, ref, payload in outcome.follow_ups:
                self.store.inbox.add(self.session_id, kind, now, ref=ref, payload=payload)
            if outcome.wipe:
                self.store.messages.wipe(outcome.session.key)
                self.store.inbox.wipe_instructions(self.session_id)
            session = self.store.threads.save(outcome.session, now)
            self.store.inbox.finish(item.id)
        if outcome.posts:
            self.rt.bus.outbox.ring()
        if outcome.jobs:
            self.rt.bus.jobs.ring()
        if outcome.follow_ups:
            self.rt.bus.thread(self.session_id)
        return session

    def settle(self, item: InboxItem, verdict: tuple[str, str] | None = None, calls: list | tuple = ()) -> None:
        """Finish an item that changes nothing but its bookkeeping."""
        session = self.store.threads.get(self.session_id)
        self.commit(item, Outcome(session=session, verdict=verdict, calls=tuple(calls)))

    # ----- messages -----

    async def on_message(self, item: InboxItem) -> None:
        message = self.store.messages.get(item.ref)
        session = self.store.threads.get(self.session_id)
        if message is None or session is None:
            self.store.inbox.finish(item.id, "dropped")
            return
        config, now = self.config, self.rt.clock.now()
        if message.workspace != config.slack.workspace or message.channel not in config.slack.channels:
            self.settle(item, ("ignore", "not a configured channel"))
            return
        verdict = policy.gate(message, session, owner=config.owner.slack_user, limits=config.limits,
                              general_messages=config.slack.general_messages,
                              cooling=self.store.cooldowns.cooling(message.workspace, message.channel, now,
                                                                   config.slack.cooldown),
                              observe_only=self.rt.observe_only, resumed=bool(item.payload.get("resumed")))
        if verdict.kind in ("ignore", "observe"):
            self.settle(item, (verdict.kind, verdict.reason))
            return
        if verdict.kind == "notice":
            # Once per blocked period: a resume changes reset_at.
            post = self.post(session, f"{session.id}:inspection:{session.reset_at}:{session.turns}", "notice",
                             INSPECTION_NOTICE, status="blocked", turn=session.turns)
            self.commit(item, Outcome(session=session, posts=(post,), verdict=("notice", verdict.reason)))
            return
        ledger: list[dict] = []
        trigger = {"kind": "message", "resumed": bool(item.payload.get("resumed")),
                   "message": contexts.message_view(message)}
        if verdict.kind == "triage":
            async with self.rt.parent_slots:
                decision = await self.rt.parent.triage(self.context(session, trigger), ledger=ledger)
            if decision != "respond":
                self.settle(item, (decision, "triage"), ledger)
                return
        history = self.store.messages.thread(session.key, limit=contexts.HISTORY_LIMIT)
        linked = await links.linked(self.rt.slack, config.slack.channels, message, history)
        github = await self.github_state(session, first=message.text, history=history)
        action = await self.decide(self.context(session, trigger, linked=linked, github=github), session, ledger)
        self.apply(item, session, action, turn=verdict.turn, requester=message.sender, ledger=ledger,
                   unsolicited=verdict.kind == "triage" and session.turns == 0, verdict=("respond", verdict.reason))

    def context(self, session: ThreadSession, trigger: dict, *, linked: tuple[dict, ...] = (),
                github: tuple[dict, ...] = ()):
        return contexts.build(self.store, self.config, session, trigger, repositories=self.rt.repositories(),
                              busy=self.rt.supervisor.busy_by_machine(), linked=linked, github=github)

    async def github_state(self, session: ThreadSession, *, first: str = "", history=None) -> tuple[dict, ...]:
        """Current state of GitHub links in ``first`` and the thread (newest first); empty when turned off.

        Bounded by GITHUB_WAIT: a slow or failing GitHub costs the parent this block, never the reply.
        """
        if self.rt.github is None or not self.config.github.enabled:
            return ()
        if history is None:
            history = self.store.messages.thread(session.key, limit=contexts.HISTORY_LIMIT)
        texts = [first, *(item.text for item in reversed(history))]
        try:
            return await asyncio.wait_for(self.rt.github.linked(texts), GITHUB_WAIT)
        except Exception as error:
            logger.warning("thread %s: GitHub state unavailable (%s)", session.id, type(error).__name__)
            return ()

    def rules(self, session: ThreadSession) -> Rules:
        config = self.config
        return Rules(registry=config.machines, session=session, workers=tuple(self.store.workers.for_session(session.id)),
                     may_delegate=config.slack.may_delegate(session.key.channel),
                     max_delegations=config.limits.max_delegations_per_turn,
                     max_workers=config.limits.max_workers_per_thread, busy=self.rt.supervisor.busy_by_machine(),
                     reply_chars=config.limits.reply_chars)

    async def decide(self, context, session: ThreadSession, ledger: list) -> Action:
        async with self.rt.parent_slots:
            try:
                return await self.rt.parent.decide(context, self.rules(session), ledger=ledger)
            except ParentUnavailable:
                return Action(reply=unavailable_reply())

    # ----- applying an action -----

    def post(self, session: ThreadSession, key: str, kind: str, text: str, *, status: str = "complete", turn: int = 0,
             thread: bool = True, after: str = "") -> OutboxItem:
        meta = FridicaMeta(owner=self.config.owner.slack_user, session=session.id, turn=turn, status=status, kind=kind)
        return OutboxItem(key, session.id, kind, session.key.channel, session.key.root_ts if thread else None, text,
                          meta=meta, after=after)

    def apply(self, item: InboxItem, session: ThreadSession, action: Action, *, turn: int, requester: str,
              ledger: list, unsolicited: bool = False, verdict: tuple[str, str] | None = None, reported: tuple[str, ...] = (),
              kind: str = "reply", artifacts: tuple[dict, ...] = ()) -> ThreadSession:
        config, reply = self.config, action.reply
        posts: list[OutboxItem] = []
        text = ""
        if reply.send:
            people = render.participants(config.owner.slack_user, self.store.messages.thread(session.key, limit=100))
            text, details = render.reply_text(reply.text, reply.details, status=reply.status, requester=requester,
                                              people=people, limit=config.limits.reply_chars)
            key = f"{item.id}:{kind}"
            posts.append(self.post(session, key, kind, text, status=reply.status, turn=max(session.turns, turn)))
            if details:
                posts.append(OutboxItem(f"{item.id}:details", session.id, "upload", session.key.channel,
                                        session.key.root_ts, filename=f"details-{item.id}.md",
                                        blob=details.encode("utf-8"), after=key))
            for index, artifact in enumerate(artifacts):
                posts.append(OutboxItem(f"{item.id}:artifact:{index}", session.id, "upload", session.key.channel,
                                        session.key.root_ts, filename=os.path.basename(artifact["path"]) or f"file-{index}",
                                        blob=artifact["blob"], after=key))
        workers, jobs = [], []
        group = str(item.id) if action.delegations else ""
        context = dict(action.context)
        for delegation in action.delegations:
            worker_id = delegation.worker_id
            if not worker_id:
                placement = delegation.placement
                worker_id = "w" + uuid.uuid4().hex[:8]
                workers.append(WorkerRecord(worker_id, session.id, placement.machine.name, placement.workspace.name,
                                            placement.backend, role=delegation.role, ephemeral=delegation.ephemeral))
                if not delegation.ephemeral:
                    context.update(machine=placement.machine.name, workspace=placement.workspace.name)
            jobs.append(Job("j" + uuid.uuid4().hex[:10], worker_id, session.id, delegation.brief, join_group=group,
                            inbox_id=item.id, deliverable=delegation.deliverable))
        still_working = any(job.id not in reported for job in self.store.jobs.active_in_session(session.id))
        updated = policy.advance(session, send=reply.send, status=reply.status, text=text, turn=turn,
                                 delegated=bool(jobs), working=still_working, note_kind=action.note.get("kind", "result"),
                                 summary=action.summary, decisions=action.decisions, context=context,
                                 limits=config.limits)
        note = {key: value for key, value in action.note.items() if key != "kind"} or None
        # A finished discussion gets a channel debrief, as its own item so it runs after this one commits.
        follow_ups = (("debrief", "", {"after": f"{item.id}:{kind}"}),) if reply.finished else ()
        session = self.commit(item, Outcome(
            session=updated, posts=tuple(posts), workers=tuple(workers), jobs=tuple(jobs), reported=reported,
            note=note, verdict=verdict, cooldown=unsolicited and reply.send, calls=tuple(ledger),
            action=_action_record(action), follow_ups=follow_ups))
        for control in action.controls:
            self.rt.spawn(self._control_worker(control.worker_id, control.op))
        return session

    async def _control_worker(self, worker_id: str, op: str) -> None:
        if op == "interrupt":
            await self.rt.supervisor.interrupt(worker_id)
        else:
            await self.rt.supervisor.stop(worker_id)

    # ----- worker results -----

    async def on_worker(self, item: InboxItem) -> None:
        job = self.store.jobs.get(item.ref)
        if job is None or job.reported or self.rt.observe_only:
            self.store.inbox.finish(item.id)
            return
        group = self.store.jobs.group(job.join_group) if job.join_group else [job]
        if any(member.status in ("queued", "running") for member in group):
            self.store.inbox.finish(item.id)  # the last job of the group reports for all of them
            return
        unreported = [member for member in group if not member.reported]
        session = self.store.threads.get(self.session_id)
        ids = tuple(member.id for member in unreported)
        if session is None or session.control != "active" or not unreported:
            if session is not None:
                self.commit(item, Outcome(session=session, reported=ids))
            else:
                self.store.inbox.finish(item.id)
            return
        if self._resume_interrupted(item, session, unreported):
            return
        requester = self._requester(job)
        artifacts = tuple(artifact for member in unreported for artifact in self.store.artifacts.for_job(member.id)
                          if artifact["status"] == "ready")
        ledger: list[dict] = []
        only = unreported[0] if len(group) == 1 else None
        if (only is not None and only.status == "done" and only.result and only.result.status == "done"
                and only.result.report and self.config.limits.report_fast_path):
            context = {"branch": only.result.machine_state.branch} if only.result.machine_state.branch else {}
            action = Action(reply=Reply(send=True, text=only.result.report, status="complete"), context=context)
            kind = "report"
        elif only is not None and only.status == "interrupted" and only.result is None:
            action = Action(reply=Reply(send=True, text=INTERRUPTED, status="complete"))
            kind = "notice"
        else:
            trigger = {"kind": "worker_results", "results": [self._result_view(member) for member in unreported]}
            github = await self.github_state(session)
            action = await self.decide(self.context(session, trigger, github=github), session, ledger)
            kind = "report"
        self.apply(item, session, action, turn=session.turns, requester=requester, ledger=ledger, reported=ids,
                   kind=kind, artifacts=artifacts)

    def _result_view(self, job: Job) -> dict:
        worker = self.store.workers.get(job.worker_id)
        return {"worker_id": job.worker_id, "machine": worker.machine if worker else "", "workspace": worker.workspace if worker else "",
                "role": worker.role if worker else "", "job_status": job.status, "error": job.error[:500],
                "brief": job.brief[:1000], "result": job.result.to_dict() if job.result else None}

    def _requester(self, job: Job) -> str:
        origin = self.store.inbox.get(job.inbox_id) if job.inbox_id else None
        while origin is not None and origin.kind != "message":
            first = self.store.jobs.get(origin.ref) if origin.kind in ("worker_result", "worker_interrupted") else None
            origin = self.store.inbox.get(first.inbox_id) if first and first.inbox_id else None
        message = self.store.messages.get(origin.ref) if origin else None
        return message.sender if message else ""

    def _resume_interrupted(self, item: InboxItem, session: ThreadSession, jobs: list[Job]) -> bool:
        """With auto_resume, rerun jobs a restart interrupted, once, continuing their backend sessions."""
        if not self.config.limits.auto_resume:
            return False
        retry = [job for job in jobs if job.status == "interrupted" and job.error == "daemon stopped" and job.attempt <= 1]
        if not retry or len(retry) != len(jobs):
            return False
        copies = tuple(replace(job, id="j" + uuid.uuid4().hex[:10], status="queued", attempt=job.attempt,
                               result=None, error="", inbox_id=job.inbox_id) for job in retry)
        self.commit(item, Outcome(session=session, jobs=copies, reported=tuple(job.id for job in retry)))
        return True

    # ----- debrief -----

    async def on_debrief(self, item: InboxItem) -> None:
        session = self.store.threads.get(self.session_id)
        if session is None or session.control != "active" or session.debriefed_turn >= session.turns or self.rt.observe_only:
            self.store.inbox.finish(item.id)
            return
        ledger: list[dict] = []
        try:
            async with self.rt.parent_slots:
                debrief = await self.rt.parent.debrief(self.context(session, {"kind": "debrief"}), ledger=ledger)
        except Exception as error:
            logger.warning("thread %s: debrief unavailable (%s)", session.id, error)
            self.settle(item, calls=ledger)
            return
        after = item.payload.get("after", "")
        post = self.post(session, f"{item.id}:debrief", "debrief_root", DEBRIEF_HEADER + debrief, thread=False,
                         turn=session.turns, after=after if after and self.store.outbox.get(after) else "")
        self.commit(item, Outcome(session=replace(session, debriefed_turn=session.turns), posts=(post,),
                                  calls=tuple(ledger)))

    # ----- owner controls -----

    async def on_owner_instruction(self, item: InboxItem) -> None:
        session = self.store.threads.get(self.session_id)
        if (session is None or session.control in ("closed", "archived", "cleaned")
                or session.key.channel not in self.config.slack.channels
                or self.rt.observe_only):
            self.store.inbox.finish(item.id, "dropped")
            return
        session = replace(session, control="active", pause_reason="", wait_streak=0, no_progress=0,
                          status="complete" if session.status == "blocked" else session.status)
        trigger = {"kind": "owner_instruction", "text": item.payload["text"]}
        ledger: list[dict] = []
        github = await self.github_state(session, first=item.payload["text"])
        action = await self.decide(self.context(session, trigger, github=github), session, ledger)
        self.apply(item, session, action, turn=session.turns + 1,
                   requester=self.config.owner.slack_user, ledger=ledger)

    async def on_control(self, item: InboxItem) -> None:
        session = self.store.threads.get(self.session_id)
        action, actor = item.payload.get("action"), item.payload.get("actor", "owner")
        if session is None:
            self.store.inbox.finish(item.id, "dropped")
            return
        now = self.rt.clock.now()
        follow_ups: tuple = ()
        if action == "pause":
            updated = replace(session, control="paused", pause_reason="Paused by the owner.")
        elif action in ("close", "archive"):
            updated = replace(session, control="closed" if action == "close" else "archived")
        elif action == "restore":
            updated = replace(session, control="active", pause_reason="")
        elif action == "resume":
            latest = self.store.messages.latest_unanswered(session.key)
            # reset_at is compared with Slack timestamps: everything up to the newest message so far is history,
            # except the unanswered message that the resume replays.
            newest = self.store.messages.thread(session.key, limit=1)
            boundary = float(latest.ts) - RESUME_MARGIN if latest else (float(newest[-1].ts) if newest else 0.0)
            updated = replace(session, control="active", pause_reason="", turns=0, wait_streak=0, no_progress=0,
                              debriefed_turn=0,
                              status="complete" if session.status in ("blocked", "working") else session.status,
                              reset_at=boundary)
            if latest is not None:
                follow_ups = (("message", latest.event_id, {"resumed": True}),)
        elif action == "clean":
            updated = replace(session, control="cleaned", summary="", decisions=())
        else:
            self.store.inbox.finish(item.id, "dropped")
            return
        self.store.audit.record(actor, f"thread.{action}", now, target=session.id)
        self.commit(item, Outcome(session=updated, follow_ups=follow_ups, wipe=action == "clean"))
        if action in ("close", "archive", "clean"):
            for worker in self.store.workers.for_session(session.id):
                if worker.status != "stopped":
                    self.rt.spawn(self.rt.supervisor.stop(worker.id))


def _action_record(action: Action) -> dict:
    return {"reply": {"send": action.reply.send, "status": action.reply.status, "discussion": action.reply.discussion,
                      "chars": len(action.reply.text)},
            "delegations": [{"worker_id": item.worker_id,
                             "machine": item.placement.machine.name if item.placement else "",
                             "workspace": item.placement.workspace.name if item.placement else "",
                             "backend": item.placement.backend if item.placement else "",
                             "role": item.role, "ephemeral": item.ephemeral, "deliverable": item.deliverable}
                            for item in action.delegations],
            "controls": [{"worker_id": item.worker_id, "op": item.op} for item in action.controls],
            "context": action.context, "decisions": list(action.decisions)}
