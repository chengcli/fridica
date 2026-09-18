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


@dataclass(frozen=True)
class AgentResult:
    text: str
    status: str = "complete"


class AgentBackend(Protocol):
    async def classify(self, message: Message, context: ConversationContext) -> Decision: ...

    async def respond(self, message: Message, context: ConversationContext) -> AgentResult: ...


class Transport(Protocol):
    async def send(self, message: Message, result: AgentResult, task_id: str, turn: int) -> str: ...
