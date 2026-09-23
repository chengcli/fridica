from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Protocol


class Decision(str, Enum):
    IGNORE = "ignore"
    OBSERVE = "observe"
    RESPOND = "respond"


@dataclass(frozen=True)
class Message:
    event_id: str
    workspace_id: str
    channel_id: str
    sender_id: str
    text: str
    timestamp: str
    thread_id: str
    generated: bool = False
    task_id: str | None = None
    turn: int = 0
    task_status: str | None = None


@dataclass(frozen=True)
class ConversationContext:
    messages: list[Message]
    owner_id: str
    profile: str
    task_id: str
    turn: int
    session: str | None = None
    task: dict | None = None
    worker: dict | None = None
    """State of this thread's heavy-task worker (``state``, ``since``), or None when none was started."""


@dataclass(frozen=True)
class AgentResult:
    text: str
    status: str = "complete"
    session: str | None = None
    finished: bool = False
    """The agent judged the whole discussion finished: request resolved, every action item done or handed off."""
    send: bool = True
    update: dict | None = None
    escalate: str = ""
    """A self-contained brief for the persistent heavy-task worker; empty when the reply is the whole answer."""


class AgentBackend(Protocol):
    async def classify(self, message: Message, context: ConversationContext) -> Decision: ...

    async def respond(self, message: Message, context: ConversationContext) -> AgentResult: ...

    async def summarize(self, context: ConversationContext) -> str: ...

    async def debrief(self, context: ConversationContext) -> str: ...

    async def work(self, brief: str, context: ConversationContext, resume: str | None) -> tuple[str, str | None]:
        """Run ``brief`` on the persistent heavy-task worker; return the report and the worker thread to resume later."""
        ...

    async def close(self) -> None:
        """Stop any persistent worker processes."""
        ...


class Transport(Protocol):
    async def send(self, message: Message, result: AgentResult, task_id: str, turn: int) -> str: ...

    async def announce(self, message: Message, text: str, task_id: str) -> str:
        """Post ``text`` as a new top-level message in the message's channel and return its timestamp."""
        ...
