from __future__ import annotations

import asyncio
import json
import logging
import re
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
    def __init__(self, config: Config, store: Store, agent: AgentBackend | None, transport: Transport, observe_only: bool = False):
        self.config = config
        self.store = store
        self.agent = agent
        self.transport = transport
        self.observe_only = observe_only
        self._lock = asyncio.Lock()
        self.store.bind(config.owner_id, config.workspace_id)

    def receive(self, message: Message) -> bool:
        if message.workspace_id != self.config.workspace_id or message.channel_id not in self.config.channels:
            return False
        return self.store.add(message)

    async def process(self, message: Message) -> None:
        async with self._lock:
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
            if row["state"] == "ready":
                await self._deliver(message)
                return
            task = self.store.task(message)
            if self.store.awaiting_delivery(message):
                return
            mentioned = f"<@{self.config.owner_id}>" in message.text
            active = task is not None and task["status"] == "waiting"
            task_id = task["task_id"] if task else (message.task_id or uuid.uuid4().hex)
            turn = max(task["turns"] if task else 0, message.turn) + 1
            context = ConversationContext(
                self.store.context(message, self.config.context_limit), self.config.owner_id, self.config.profile, task_id, turn
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
                result = await self.agent.respond(message, context)
                if not isinstance(result, AgentResult) or result.status not in {"complete", "waiting", "blocked"}:
                    raise ValueError("invalid agent result")
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

    async def run(self) -> None:
        while True:
            for row in self.store.pending():
                await self.process(Message(**json.loads(row["payload"])))
            await asyncio.sleep(0.25)
