from __future__ import annotations

import asyncio
from dataclasses import asdict, replace
import json
import logging
import os
import re
import time
import uuid

from .config import Config
from .models import AgentBackend, AgentResult, ConversationContext, Decision, Message, Transport
from .store import Store
from .replies import format_reply


logger = logging.getLogger(__name__)


class RateLimited(Exception):
    def __init__(self, retry_after: float):
        self.retry_after = max(1, min(retry_after, 3600))


class DeliveryRejected(Exception):
    def __init__(self, code: str = "unknown_error"):
        self.code = code if isinstance(code, str) and re.fullmatch(r"[a-z][a-z0-9_]{0,63}", code) else "unknown_error"
        super().__init__(self.code)


class Replica:
    def __init__(self, config: Config, store: Store, agent: AgentBackend | None, transport: Transport, observe_only: bool = False, config_path=None):
        self.base_config = config
        self.config_path = config_path
        self.config_revision = None
        self.config = config
        self.store = store
        self.agent = agent
        self.transport = transport
        self.observe_only = observe_only
        self._lock = asyncio.Lock()
        self.store.bind(config.owner_id, config.workspace_id)
        if not observe_only:
            for row in store.connection.execute("SELECT e.payload FROM tasks t JOIN events e ON e.event_id=("
                "SELECT x.event_id FROM events x WHERE x.workspace=t.workspace AND x.channel=t.channel AND x.thread=t.thread ORDER BY x.timestamp DESC LIMIT 1) "
                "WHERE t.workspace=? AND t.status='waiting' AND t.control_state='active'", (config.workspace_id,)).fetchall():
                message = Message(**json.loads(row['payload']))
                if message.channel_id in config.channels:
                    store.pause_loop(message, config.max_turns, config.max_wait_replies)
        self.permissions = None
        if config.file_access:
            from .permissions import Permissions
            self.permissions = Permissions(config, store, agent)

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
            self.agent = None if self.observe_only else create_backend(current)
            self.config = current
            if current.file_access:
                from .permissions import Permissions
                self.permissions = Permissions(current, self.store, self.agent)
        with self.store.connection:
            self.store.connection.execute('CREATE TABLE IF NOT EXISTS configuration_runtime (id INTEGER PRIMARY KEY,revision TEXT,pid INTEGER)')
            self.store.connection.execute('INSERT OR REPLACE INTO configuration_runtime VALUES(1,?,?)',(revision,os.getpid()))
        self.config_revision = revision

    def receive(self, message: Message) -> bool:
        if message.workspace_id != self.config.workspace_id or message.channel_id not in self.config.channels:
            return False
        return self.store.add(message)

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
            if message.workspace_id != self.config.workspace_id or message.channel_id not in self.config.channels:
                self.store.mark(message.event_id, "observed", Decision.IGNORE.value)
                return
            if self.observe_only:
                if row["state"] == "ready":
                    return
                self.store.mark(message.event_id, "observed", Decision.OBSERVE.value)
                logger.info("Observed event %s in %s; no agent or delivery", message.event_id, message.channel_id)
                return
            task = self.store.task(message)
            if task and task['control_state'] != 'active':
                if row['state'] == 'pending':
                    if task['control_state'] == 'cleaned':
                        with self.store.connection:
                            self.store.connection.execute("UPDATE events SET payload=? WHERE event_id=?",
                                (json.dumps(asdict(replace(message, text=''))), message.event_id))
                    self.store.mark(message.event_id, 'observed', task['control_state'])
                return
            if task and row['state'] == 'pending' and float(message.timestamp) <= task['reset_at']:
                self.store.mark(message.event_id, 'observed', 'before_resume')
                return
            if row["state"] == "ready":
                await self._deliver(message)
                return
            if self.permissions and self.permissions.awaiting(message):
                return
            task = self.store.task(message)
            if self.store.awaiting_delivery(message):
                return
            mentioned = f"<@{self.config.owner_id}>" in message.text
            active = task is not None and task["status"] == "waiting"
            task_id = task["task_id"] if task else (message.task_id or uuid.uuid4().hex)
            turn = max(task["turns"] if task else 0, 0 if task and task['reset_at'] else message.turn) + 1
            if task and message.sender_id != self.config.owner_id:
                self.store.pause_loop(message, self.config.max_turns, self.config.max_wait_replies)
                if turn > self.config.max_turns:
                    with self.store.connection:
                        self.store.connection.execute("UPDATE tasks SET control_state='paused',pause_reason='Thread turn limit reached.',control_revision=control_revision+1 WHERE workspace=? AND channel=? AND thread=? AND control_state='active'", (message.workspace_id,message.channel_id,message.thread_id))
                if self.store.task(message)['control_state'] == 'paused':
                    self.store.mark(message.event_id, 'observed', 'paused')
                    return
            session = task["session"] if task and self.config.resume_sessions else None
            if session and time.time() - task["updated"] > self.config.session_timeout:
                logger.info("Session for thread %s is idle beyond session_timeout; a new one will start", message.thread_id)
                session = None
            context = ConversationContext(
                self.store.context(message, self.config.context_limit), self.config.owner_id, self.config.profile, task_id, turn,
                session=session,
            )
            mandatory = mentioned and not message.generated and message.sender_id != self.config.owner_id
            if mandatory and (turn > self.config.max_turns or (task and task["status"] not in {"complete", "waiting"})):
                explanation = (
                    "This thread has reached its action limit. Please start a new thread for a new task."
                    if turn > self.config.max_turns else
                    "An earlier task in this thread needs local inspection before I can continue. I haven't retried it."
                )
                result = AgentResult(explanation, "blocked")
                self.store.save_notice(message, result, task_id, task["turns"] if task else 0)
                await self._deliver(message)
                return
            decision = Decision.OBSERVE
            if message.sender_id == self.config.owner_id:
                decision = Decision.IGNORE if message.generated else Decision.OBSERVE
            elif message.generated and message.task_status in {"complete", "blocked"}:
                decision = Decision.IGNORE
            elif turn > self.config.max_turns:
                decision = Decision.IGNORE
            elif message.generated and not (mentioned or active):
                decision = Decision.IGNORE
            elif task and task["status"] not in {"complete", "waiting"}:
                decision = Decision.IGNORE
            elif mentioned or active:
                decision = Decision.RESPOND
            elif self.config.general_messages and not self.store.cooling_down(message, self.config.cooldown):
                try:
                    decision = await self.agent.classify(message, context)
                    if not isinstance(decision, Decision):
                        decision = Decision.OBSERVE
                except Exception:
                    logger.warning("Classification failed for event %s; observing", message.event_id)
            if decision != Decision.RESPOND:
                self.store.mark(message.event_id, "observed", decision.value)
                return
            self.store.begin(message, task_id, turn)
            try:
                responder = self.permissions or self.agent
                result = await responder.respond(message, context)
                if not isinstance(result, AgentResult) or result.status not in {"complete", "waiting", "blocked"}:
                    raise ValueError("invalid agent result")
                if self.config.resume_sessions and result.session != context.session:
                    self.store.save_session(message, result.session)
                result = AgentResult(result.text, result.status)
                if not isinstance(result.text, str) or not result.text.strip() or len(result.text) > 3500:
                    raise ValueError("agent response must contain 1 to 3500 characters")
                if result.status == "waiting" and f"<@{message.sender_id}>" not in result.text:
                    result = AgentResult(f"<@{message.sender_id}> {result.text}", result.status)
                result = AgentResult(format_reply(result.text, message, context), result.status)
                if not result.text.strip() or len(result.text) > 3500:
                    raise ValueError("formatted response must contain 1 to 3500 characters")
            except asyncio.CancelledError:
                self.store.mark(message.event_id, "interrupted")
                raise
            except Exception:
                logger.error("Agent failed for event %s; inspect local state before retrying", message.event_id)
                result = AgentResult(
                    "I couldn't complete this request. Please check Fridica locally before retrying; partial changes may exist.",
                    "blocked",
                )
            self.store.save_result(message, result)
            await self._deliver(message)

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
        self.store.add(Message(
            event_id="outgoing:" + message.workspace_id + ":" + message.channel_id + ":" + timestamp,
            workspace_id=message.workspace_id, channel_id=message.channel_id, sender_id=self.config.owner_id,
            text=result.text, timestamp=timestamp, thread_id=message.thread_id, generated=True,
            task_id=row["task_id"], turn=row["turn"],
            task_status=result.status,
        ), state="sent")
        if result.status == 'waiting':
            self.store.pause_loop(message, self.config.max_turns, self.config.max_wait_replies)

    async def run(self) -> None:
        while True:
            try:
                async with self._lock:
                    self.reload_config(idle=True)
            except (ValueError, OSError):
                logger.error('Configuration could not be loaded; processing is paused until it is repaired.')
                await asyncio.sleep(5)
                continue
            if self.permissions and not self.observe_only:
                async with self._lock:
                    await self.permissions.process_approved(self)
            for row in self.store.pending():
                await self.process(Message(**json.loads(row["payload"])))
            await asyncio.sleep(0.25)
