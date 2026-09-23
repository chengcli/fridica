"""The conversation loop: decide whether to act on a Slack message, act, deliver, and follow through.

``Replica`` is the only place that knows the rules of engagement (mentions, turn
budgets, loop pauses, session continuity, wrap-ups and debriefs). It leaves
persistence to ``Store``, Slack I/O to a ``Transport``, and model calls to an
``AgentBackend``. ``process`` reads top to bottom as the pipeline for one message:
gate, budget, decide, respond, deliver, follow up.
"""
from __future__ import annotations

import asyncio
from dataclasses import replace
import json
import logging
import os
import re
import time
import uuid

from . import collaboration
from .config import Config
from .models import AgentBackend, AgentResult, ConversationContext, Decision, Message, Transport
from .replies import format_reply, permalinks
from .store import Store

logger = logging.getLogger(__name__)
REPLY_LIMIT = 3500
LINKED_TEXT_LIMIT = 20000
OPEN_STATUSES = {"complete", "waiting"}

# Texts Fridica posts on its own, outside the agent's replies.
UNAVAILABLE = "I couldn't complete this request. Please check Fridica locally before retrying; partial changes may exist."
LIMIT_NOTICE = "This thread has reached its action limit. Please start a new thread for a new task."
INSPECTION_NOTICE = "An earlier task in this thread needs local inspection before I can continue. I haven't retried it."
STOP_NOTICE = ("This thread has reached its turn limit, so I'm stopping here. I've posted a summary of the discussion "
               "in the channel; please continue in that new thread.")
STOP_NOTICE_NO_SUMMARY = ("This thread has reached its turn limit, so I'm stopping here. I couldn't post a summary; "
                          "please start a new thread to continue.")
SUMMARY_HEADER = "Continuing from a thread that reached its turn limit. Summary of the discussion so far:\n\n"
SUMMARY_FOOTER = "\n\nReply in this thread to continue."
DEBRIEF_HEADER = "Debrief: this discussion is finished.\n\n"
HEAVY_FAILED_NOTICE = "The longer job I started for this thread did not finish. Please check locally before asking again."
HEAVY_INTERRUPTED_NOTICE = "The longer job I started for this thread was interrupted by a restart and was not resumed."


class RateLimited(Exception):
    def __init__(self, retry_after: float):
        self.retry_after = max(1, min(retry_after, 3600))


class DeliveryRejected(Exception):
    def __init__(self, code: str = "unknown_error"):
        self.code = code if isinstance(code, str) and re.fullmatch(r"[a-z][a-z0-9_]{0,63}", code) else "unknown_error"
        super().__init__(self.code)


class Replica:
    def __init__(self, config: Config, store: Store, agent: AgentBackend | None, transport: Transport,
                 observe_only: bool = False, config_path=None):
        self.base_config = config
        self.config_path = config_path
        self.config_revision = None
        self.config = config
        self.store = store
        self.agent = agent
        self.transport = transport
        self.observe_only = observe_only
        self._lock = asyncio.Lock()
        self._background: set[asyncio.Task] = set()
        self._recovered = False
        self.store.bind(config.owner_id, config.workspace_id)
        if not observe_only:
            # Threads that were waiting for information when the daemon stopped may already be looping.
            for message in store.waiting_threads(config.workspace_id):
                if message.channel_id in config.channels:
                    store.pause_loop(message, config.max_turns, config.max_wait_replies)
        self.permissions = None
        if config.file_access:
            from .permissions import Permissions
            self.permissions = Permissions(config, store, agent)

    # ----- configuration -----

    def reload_config(self, *, idle=False):
        if self.config_path is None:
            return
        from .settings import configured_snapshot, fingerprint
        if idle and fingerprint(self.config_path) == self.config_revision:
            return
        current, revision = configured_snapshot(self.base_config, self.config_path)
        if revision == self.config_revision:
            return
        if current != self.config:
            from .agents import create_backend
            previous, self.agent = self.agent, (None if self.observe_only else create_backend(current))
            self.config = current
            self._spawn(self._close_agent(previous))
            if current.file_access:
                from .permissions import Permissions
                self.permissions = Permissions(current, self.store, self.agent)
        with self.store.connection:
            self.store.connection.execute('CREATE TABLE IF NOT EXISTS configuration_runtime (id INTEGER PRIMARY KEY,revision TEXT,pid INTEGER)')
            self.store.connection.execute('INSERT OR REPLACE INTO configuration_runtime VALUES(1,?,?)', (revision, os.getpid()))
        self.config_revision = revision

    def _spawn(self, coroutine) -> asyncio.Task | None:
        """Run ``coroutine`` in the background and keep a reference so ``run`` can cancel it on shutdown."""
        try:
            task = asyncio.get_running_loop().create_task(coroutine)
        except RuntimeError:
            coroutine.close()
            return None
        self._background.add(task)
        task.add_done_callback(self._background.discard)
        return task

    @staticmethod
    async def _close_agent(agent) -> None:
        close = getattr(agent, "close", None)
        if close is not None:
            try:
                await close()
            except Exception as error:
                logger.warning("Heavy-task workers did not stop cleanly (%s)", type(error).__name__)

    # ----- intake -----

    def _in_scope(self, message: Message) -> bool:
        return message.workspace_id == self.config.workspace_id and message.channel_id in self.config.channels

    def _mentions_owner(self, message: Message) -> bool:
        return f"<@{self.config.owner_id}>" in message.text

    def receive(self, message: Message) -> bool:
        if not self._in_scope(message):
            return False
        return self.store.add(message)

    # ----- the pipeline for one message -----

    async def process(self, message: Message) -> None:
        async with self._lock:
            try:
                self.reload_config()
            except (ValueError, OSError):
                logger.error('Configuration could not be loaded; this request remains queued.')
                return
            row = self.store.get(message.event_id)
            if row is None or row["state"] not in {"pending", "ready"}:
                return
            if not await self._gate(message, row):
                return
            task = self.store.task(message)
            task_id, turn = self._budget(message, task)
            if task and message.sender_id != self.config.owner_id and await self._enforce_turn_limit(message, task, turn):
                return
            history = self.store.context(message, self.config.context_limit)
            context = ConversationContext(
                history, self.config.owner_id, self.config.profile,
                task_id, turn, session=self._session(message, task),
                task=collaboration.snapshot(self.store.connection, self.config, message.channel_id, message.thread_id)["data"] if task else {},
                worker=self.store.worker(message), linked=await self._linked(message, history),
            )
            mentioned = self._mentions_owner(message)
            if mentioned and not message.generated and task and task['status'] not in OPEN_STATUSES:
                noticed = self.store.connection.execute("SELECT 1 FROM events WHERE workspace=? AND channel=? AND thread=? AND reply_only=1 AND state IN ('sent','ready','sending','ambiguous') LIMIT 1", self.store._key(message)).fetchone()
                if not noticed:
                    self.store.save_notice(message, AgentResult(INSPECTION_NOTICE, 'blocked'), task_id, task['turns'])
                    await self._deliver(message)
                else:
                    self.store.mark(message.event_id, 'observed', 'blocked')
                return
            decision = await self._decide(message, task, turn, mentioned, context)
            if decision != Decision.RESPOND:
                self.store.mark(message.event_id, "observed", decision.value)
                return
            self.store.begin(message, task_id, turn)
            result = await self._respond(message, context)
            try:
                file_operation = self.store.connection.execute('SELECT 1 FROM file_requests WHERE event_id=?', (message.event_id,)).fetchone() is not None
                result = collaboration.prepare(self.store.connection, self.config, message, result, file_operation=file_operation)
            except ValueError as error:
                # Only a missing task row or channel can get here; the reply itself is still good.
                logger.error("Task notes could not be prepared for event %s (%s); replying without notes", message.event_id, error)
            if not result.send:
                status = 'blocked' if result.status == 'blocked' else task['status'] if task else 'complete'
                with self.store.connection:
                    self.store.connection.execute("UPDATE events SET state='observed',decision='silent',result=? WHERE event_id=?",
                                                  (json.dumps({'text': '', 'status': status, 'send': False}), message.event_id))
                    self.store.connection.execute('UPDATE tasks SET status=? WHERE workspace=? AND channel=? AND thread=?',
                                                  (status, *self.store._key(message)))
                collaboration.pause_stalled(self.store.connection, self.config, message)
                return
            self.store.save_result(message, result)
            await self._deliver(message)

    async def _gate(self, message: Message, row) -> bool:
        """Early exits that need no model: scope, observe-only, paused threads, pending deliveries."""
        if not self._in_scope(message):
            self.store.mark(message.event_id, "observed", Decision.IGNORE.value)
            return False
        if self.observe_only:
            if row["state"] != "ready":
                self.store.mark(message.event_id, "observed", Decision.OBSERVE.value)
                logger.info("Observed event %s in %s; no agent or delivery", message.event_id, message.channel_id)
            return False
        task = self.store.task(message)
        if task and task['control_state'] != 'active':
            if row['state'] == 'pending':
                if task['control_state'] == 'cleaned':
                    self.store.clear_text(message)
                self.store.mark(message.event_id, 'observed', task['control_state'])
            return False
        if task and row['state'] == 'pending' and float(message.timestamp) <= task['reset_at']:
            self.store.mark(message.event_id, 'observed', 'before_resume')
            return False
        if row["state"] == "ready":
            await self._deliver(message)
            return False
        if task and task["status"] == "blocked":
            self.store.mark(message.event_id, "observed", "blocked")
            return False
        if self.permissions and self.permissions.awaiting(message):
            return False
        return not self.store.awaiting_delivery(message)

    def _budget(self, message: Message, task) -> tuple[str, int]:
        """The task identifier and the turn number this message would use."""
        task_id = task["task_id"] if task else (message.task_id or uuid.uuid4().hex)
        inherited = 0 if task and task['reset_at'] else message.turn
        return task_id, max(task["turns"] if task else 0, inherited) + 1

    async def _enforce_turn_limit(self, message: Message, task, turn: int) -> bool:
        """Pause a looping or exhausted thread; True when the message was absorbed by the pause."""
        self.store.pause_loop(message, self.config.max_turns, self.config.max_wait_replies)
        if turn > self.config.max_turns:
            self.store.pause_for_turn_limit(message)
        paused = self.store.task(message)
        if paused['control_state'] != 'paused':
            return False
        self.store.mark(message.event_id, 'observed', 'paused')
        if paused['turns'] >= self.config.max_turns and paused['continuation'] is None:
            await self._wrap_up(message)
        return True

    def _session(self, message: Message, task) -> str | None:
        """The backend session to resume for this thread, unless continuity is off or the thread went idle."""
        if not task or not self.config.resume_sessions or not task["session"]:
            return None
        if time.time() - task["updated"] > self.config.session_timeout:
            logger.info("Session for thread %s is idle beyond session_timeout; a new one will start", message.thread_id)
            return None
        return task["session"]

    async def _linked(self, message: Message, history: list[Message]) -> tuple[dict, ...]:
        """Fetch the Slack messages this thread links to, so a request that points at another thread can be read.

        Links in ``message`` come first, then links in ``history``, so later turns still see a linked spec.
        Only links into configured channels are followed: anyone in those channels can trigger
        the agent, so following links elsewhere would expose channels they cannot read.
        Links into the current thread are skipped because history already carries them.
        """
        fetch = getattr(self.transport, "fetch", None)
        linked, budget = [], LINKED_TEXT_LIMIT
        links = permalinks("\n".join(entry.text for entry in [message, *reversed(history)])) if fetch else []
        for link, channel, timestamp, root in links:
            if budget <= 0:
                break
            if channel not in self.config.channels:
                linked.append({"link": link, "error": "not in a channel I watch"})
                continue
            if channel == message.channel_id and (root or timestamp) == message.thread_id:
                continue
            try:
                entries = await fetch(channel, timestamp, root)
            except Exception as error:
                logger.warning("Linked message %s in %s could not be fetched (%s)", timestamp, channel, type(error).__name__)
                linked.append({"link": link, "error": "could not be fetched"})
                continue
            if not entries:
                linked.append({"link": link, "error": "not found"})
            for entry in entries:
                text = entry["text"][:budget]
                budget -= len(text)
                linked.append({"link": link, "sender": entry["sender"], "text": text})
                if budget <= 0:
                    break
        return tuple(linked)

    async def _decide(self, message: Message, task, turn: int, mentioned: bool, context: ConversationContext) -> Decision:
        """Whether to reply: owner messages and closed generated exchanges never trigger, mentions always do."""
        active = task is not None and task["status"] == "waiting"
        if message.sender_id == self.config.owner_id:
            return Decision.IGNORE if message.generated else Decision.OBSERVE
        if message.generated and message.task_status in {"complete", "blocked"} and not (mentioned or active):
            return Decision.IGNORE
        if turn > self.config.max_turns:
            return Decision.IGNORE
        if message.generated and not (mentioned or active):
            return Decision.IGNORE
        if task and task["status"] not in OPEN_STATUSES:
            return Decision.IGNORE
        if mentioned or active:
            return Decision.RESPOND
        if self.config.general_messages and not self.store.cooling_down(message, self.config.cooldown):
            try:
                decision = await self.agent.classify(message, context)
                if isinstance(decision, Decision):
                    return decision
            except Exception:
                logger.warning("Classification failed for event %s; observing", message.event_id)
        return Decision.OBSERVE

    async def _respond(self, message: Message, context: ConversationContext) -> AgentResult:
        """Run the agent and normalise its reply; any failure becomes the generic blocked notice."""
        try:
            responder = self.permissions or self.agent
            result = await responder.respond(message, context)
            if not isinstance(result, AgentResult) or result.status not in {"complete", "waiting", "blocked"}:
                raise ValueError("invalid agent result")
            if self.config.resume_sessions and result.session != context.session:
                self.store.save_session(message, result.session)
            return self._format(result, message, context)
        except asyncio.CancelledError:
            self.store.mark(message.event_id, "interrupted")
            raise
        except Exception:
            logger.error("Agent failed for event %s; inspect local state before retrying", message.event_id)
            return AgentResult(UNAVAILABLE, "blocked")

    @staticmethod
    def _format(result: AgentResult, message: Message, context: ConversationContext) -> AgentResult:
        """Bound the text, address the sender when waiting, and render known IDs as mentions."""
        text = result.text
        if type(result.send) is not bool:
            raise ValueError("invalid send decision")
        if not result.send:
            if not isinstance(text, str) or len(text) > REPLY_LIMIT:
                raise ValueError("invalid silent response")
            return replace(result, text="", session=None, finished=False)
        if not isinstance(text, str) or not text.strip() or len(text) > REPLY_LIMIT:
            raise ValueError("agent response must contain 1 to 3500 characters")
        if result.status == "waiting" and f"<@{message.sender_id}>" not in text:
            text = f"<@{message.sender_id}> {text}"
        text = format_reply(text, message, context)
        if not text.strip() or len(text) > REPLY_LIMIT:
            raise ValueError("formatted response must contain 1 to 3500 characters")
        return replace(result, text=text, session=None, finished=bool(result.finished) and result.status == "complete")

    # ----- delivery and follow-through -----

    async def _deliver(self, message: Message) -> None:
        row = self.store.get(message.event_id)
        result = AgentResult(**json.loads(row["result"]))
        self.store.mark(message.event_id, "sending")
        try:
            timestamp = await self.transport.send(message, result, row["task_id"], row["turn"])
        except RateLimited as error:
            if row["attempts"] >= 5:
                self.store.mark(message.event_id, "failed")
            else:
                self.store.retry(message.event_id, error.retry_after)
            return
        except DeliveryRejected as error:
            self.store.mark(message.event_id, "failed")
            logger.error("Slack rejected reply for event %s: %s", message.event_id, error.code)
            if error.code == "missing_scope":
                logger.error("Check chat:write under Slack User Token Scopes, reinstall the app, and update the user token if changed.")
            return
        except asyncio.CancelledError:
            self.store.mark(message.event_id, "ambiguous")
            raise
        except Exception:
            self.store.mark(message.event_id, "ambiguous")
            logger.error("Delivery uncertain for event %s; will not automatically resend", message.event_id)
            return
        self.store.delivered(message, timestamp)
        self._record_outgoing(message, result.text, timestamp, thread=message.thread_id,
                              task_id=row["task_id"], turn=row["turn"], status=result.status)
        if result.status == 'waiting':
            self.store.pause_loop(message, self.config.max_turns, self.config.max_wait_replies)
        if not row["reply_only"]:
            collaboration.pause_stalled(self.store.connection, self.config, message)
            self._escalate(message, result)
            await self._follow_up(message, result)

    # ----- heavy tasks -----

    def _escalate(self, message: Message, result: AgentResult) -> None:
        """Hand a delivered reply's brief to the thread's persistent worker on the chosen host, unless one is running."""
        brief = getattr(result, "escalate", "")
        if not brief or not self.config.heavy_tasks or self.agent is None:
            return
        host = getattr(result, "escalate_host", "") or self.config.heavy_hosts[0].name
        task = self.store.task(message)
        if task is None or task["worker_state"] == "running":
            logger.info("Thread %s already has a heavy task running; brief ignored", message.thread_id)
            return
        # A worker thread only continues on the host that created it; rows written before hosts
        # were recorded came from the workspace's own host.
        previous = task["worker_host"] or self.config.primary.name
        thread = task["worker_thread"] if previous == host else None
        self.store.save_worker(message, "running", thread, host)
        if self._spawn(self._heavy(message, brief, host)) is None:
            self.store.save_worker(message, task["worker_state"], task["worker_thread"], task["worker_host"])
            return
        logger.info("Thread %s: heavy task started on %s", message.thread_id, host)

    async def _heavy(self, message: Message, brief: str, host: str) -> None:
        """Run one escalated job outside the pipeline lock, then post its report in the thread."""
        task = self.store.task(message)
        context = replace(self._thread_context(message, task), worker=self.store.worker(message))
        thread = task["worker_thread"]
        try:
            try:
                report, thread = await self.agent.work(brief, context, task["worker_thread"], host)
                if not isinstance(report, str) or not report.strip() or len(report) > REPLY_LIMIT:
                    raise ValueError("invalid heavy-task report")
                text, state = format_reply(report, message, context), "done"
            except asyncio.CancelledError:
                raise
            except Exception as error:
                logger.error("Heavy task for thread %s failed (%s): %s", message.thread_id, type(error).__name__, error or "no detail")
                text, state = HEAVY_FAILED_NOTICE, "failed"
            async with self._lock:
                self.store.save_worker(message, state, thread, host)
                await self._post_notice(message, text, task["task_id"], task["turns"])
        except asyncio.CancelledError:
            self.store.save_worker(message, "interrupted", thread, host)
            raise

    async def _post_notice(self, message: Message, text: str, task_id: str, turn: int) -> None:
        """Post ``text`` in the thread and record it as our own message; failures are logged, never retried."""
        try:
            timestamp = await self.transport.send(message, AgentResult(text, "complete"), task_id, turn)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            logger.error("Could not post to thread %s (%s)", message.thread_id, type(error).__name__)
            return
        self._record_outgoing(message, text, timestamp, thread=message.thread_id, task_id=task_id, turn=turn, status="complete")

    async def _recover_workers(self) -> None:
        """Tell threads whose heavy job was cut off by a restart; the job is not resumed automatically."""
        self._recovered = True
        if self.observe_only:
            return
        for message in self.store.interrupted_workers(self.config.workspace_id):
            task = self.store.task(message)
            if task is None or message.channel_id not in self.config.channels:
                continue
            self.store.save_worker(message, "failed", task["worker_thread"], task["worker_host"])
            await self._post_notice(message, HEAVY_INTERRUPTED_NOTICE, task["task_id"], task["turns"])

    async def _follow_up(self, message: Message, result: AgentResult) -> None:
        """After a delivered reply: debrief a finished discussion, or wrap up a thread at its turn limit."""
        task = self.store.task(message)
        if task is None or result.status == "blocked":
            return
        if collaboration.snapshot(self.store.connection, self.config, message.channel_id, message.thread_id)["no_progress"] >= collaboration.LIMIT:
            return
        if result.finished and result.status == "complete":
            await self._debrief(message)
            return
        # A thread whose last reply proposed a file operation still awaiting the owner's decision
        # is left alone so the approval flow can finish first.
        if (task["turns"] >= self.config.max_turns and task["continuation"] is None
                and not self.store.has_open_file_requests(message)):
            self.store.pause_loop(message, self.config.max_turns, self.config.max_wait_replies)
            await self._wrap_up(message)

    def _record_outgoing(self, message: Message, text: str, timestamp: str, *, thread: str,
                         task_id: str, turn: int, status: str) -> None:
        """Store a message Fridica posted so it counts as thread context and is not re-ingested."""
        self.store.add(Message(
            event_id="outgoing:" + message.workspace_id + ":" + message.channel_id + ":" + timestamp,
            workspace_id=message.workspace_id, channel_id=message.channel_id, sender_id=self.config.owner_id,
            text=text, timestamp=timestamp, thread_id=thread, generated=True,
            task_id=task_id, turn=turn, task_status=status,
        ), state="sent")

    def _thread_context(self, message: Message, task) -> ConversationContext:
        """The whole thread, for summaries and debriefs."""
        history = self.store.thread_messages(message, self.config.context_limit)
        return ConversationContext(history, self.config.owner_id, self.config.profile, task["task_id"], task["turns"],
                                   session=task["session"], task=collaboration.snapshot(self.store.connection, self.config, message.channel_id, message.thread_id)["data"])

    async def _digest(self, produce, context: ConversationContext, label: str, thread: str) -> str | None:
        """Ask the agent for a summary or debrief; None (logged) when it fails or is unusable."""
        try:
            text = await produce(context)
            if not isinstance(text, str) or not text.strip() or len(text) > 3000:
                raise ValueError(f"invalid {label}")
            return text
        except asyncio.CancelledError:
            raise
        except Exception as error:
            logger.error("%s for thread %s unavailable (%s)", label.capitalize(), thread, type(error).__name__)
            return None

    async def _post_root(self, message: Message, text: str, task_id: str, turn: int) -> str:
        """Post a new top-level channel message and record it as the root of its own thread."""
        root_ts = await self.transport.announce(message, text, task_id)
        self._record_outgoing(message, text, root_ts, thread=root_ts, task_id=task_id, turn=turn, status="complete")
        return root_ts

    async def _debrief(self, message: Message) -> None:
        """Post the closing debrief of a discussion the agent declared finished.

        Runs once per finish: a thread that continues afterwards can be debriefed
        again only after further replies. A finished thread at its turn limit gets a
        debrief instead of a continuation summary. Failures are logged, not retried.
        """
        task = self.store.task(message)
        if task is None or self.agent is None or not self.store.claim_debrief(message):
            return
        task = self.store.task(message)
        try:
            context = self._thread_context(message, task)
            debrief = await self._digest(self.agent.debrief, context, "debrief", message.thread_id)
            if debrief is None:
                return
            try:
                root_ts = await self._post_root(message, DEBRIEF_HEADER + format_reply(debrief, message, context),
                                                task["task_id"], task["turns"])
            except asyncio.CancelledError:
                raise
            except Exception as error:
                logger.error("Debrief for thread %s could not be posted (%s)", message.thread_id, type(error).__name__)
                return
            if task["turns"] >= self.config.max_turns:
                self.store.mark_debriefed_at_limit(message)
            self.store.record_debrief(message)
            logger.info("Thread %s finished; debrief posted as %s", message.thread_id, root_ts)
        finally:
            with self.store.connection:
                self.store.connection.execute('UPDATE tasks SET digest_pending=0 WHERE workspace=? AND channel=? AND thread=?', self.store._key(message))

    async def _wrap_up(self, message: Message) -> None:
        """Close out a thread that reached ``max_turns``.

        Posts a stop notice in the thread, asks the agent for a summary of the whole
        thread, posts that summary as a new top-level channel message, and seeds the
        new thread with a fresh turn budget and the old thread's session. Nobody is
        @mentioned in the new thread, so it only continues when a person replies to
        it; two agents cannot chain threads forever. Runs at most once per thread;
        failures are recorded, not retried.
        """
        task = self.store.task(message)
        if task is None or self.agent is None or not self.store.begin_continuation(message):
            return
        task = self.store.task(message)
        context = self._thread_context(message, task)
        try:
            summary = await self._digest(self.agent.summarize, context, "summary", message.thread_id)
        except asyncio.CancelledError:
            self.store.finish_continuation(message, "failed", None, None)
            raise
        notice = STOP_NOTICE if summary else STOP_NOTICE_NO_SUMMARY
        try:
            stop_ts = await self.transport.send(message, AgentResult(notice, "complete"), task["task_id"], task["turns"])
            self._record_outgoing(message, notice, stop_ts, thread=message.thread_id,
                                  task_id=task["task_id"], turn=task["turns"], status="complete")
            if summary is None:
                self.store.finish_continuation(message, "failed", None, None)
                return
            new_task_id = uuid.uuid4().hex
            root_ts = await self._post_root(message, SUMMARY_HEADER + format_reply(summary, message, context) + SUMMARY_FOOTER,
                                            new_task_id, 0)
        except asyncio.CancelledError:
            self.store.finish_continuation(message, "failed", None, None)
            raise
        except Exception as error:
            logger.error("Could not post the wrap-up for thread %s (%s); inspect locally", message.thread_id, type(error).__name__)
            self.store.finish_continuation(message, "failed", None, None)
            return
        self.store.finish_continuation(message, root_ts, new_task_id, task["session"])
        logger.info("Thread %s reached its turn limit; summary posted as %s", message.thread_id, root_ts)

    # ----- main loop -----

    async def run(self) -> None:
        try:
            while True:
                try:
                    async with self._lock:
                        self.reload_config(idle=True)
                except (ValueError, OSError):
                    logger.error('Configuration could not be loaded; processing is paused until it is repaired.')
                    await asyncio.sleep(5)
                    continue
                if not self._recovered:
                    async with self._lock:
                        await self._recover_workers()
                if self.permissions and not self.observe_only:
                    async with self._lock:
                        await self.permissions.process_approved(self)
                for row in self.store.pending():
                    await self.process(Message(**json.loads(row["payload"])))
                await asyncio.sleep(0.25)
        finally:
            await self.shutdown()

    async def shutdown(self) -> None:
        """Stop background heavy tasks and their worker processes."""
        for task in list(self._background):
            task.cancel()
        if self._background:
            await asyncio.gather(*self._background, return_exceptions=True)
        await self._close_agent(self.agent)
