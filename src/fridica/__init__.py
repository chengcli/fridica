"""Local personal agents for Slack."""

from .models import AgentBackend, AgentResult, ConversationContext, Decision, Message

__version__ = "0.1.0"
__all__ = ["AgentBackend", "AgentResult", "ConversationContext", "Decision", "Message"]

