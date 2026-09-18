"""Local personal agents for Slack."""

from importlib.metadata import PackageNotFoundError, version

from .models import AgentBackend, AgentResult, ConversationContext, Decision, Message

try:
    __version__ = version("fridica")
except PackageNotFoundError:
    __version__ = "0+unknown"
__all__ = ["AgentBackend", "AgentResult", "ConversationContext", "Decision", "Message"]
