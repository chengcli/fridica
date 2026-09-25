"""``claude -p`` with stream-json on both ends: one user message in, one result out, per job.

Tool calls outside the allowlist arrive as ``control_request``/``can_use_tool`` on
stdout (the Agent SDK's control protocol, enabled by ``--permission-prompt-tool
stdio``) and are answered with ``control_response``. The same bytes flow through
``ssh -T``, so remote workers ask for approval exactly like local ones.
"""

from __future__ import annotations

import json
import logging
import uuid

from ..core.errors import BackendError
from .base import JsonlWorker
from .protocol import ALLOW_ONCE, ALLOW_SESSION, ApprovalRequest
from .result import FORMAT_NOTE

logger = logging.getLogger(__name__)
READ_TOOLS = "Read,Glob,Grep"
ALL_TOOLS = "Bash,Read,Glob,Grep,Edit,Write"


class ClaudeWorker(JsonlWorker):
    backend = "claude"

    def __init__(self, spec, transport=None):
        super().__init__(spec, transport)
        self.control_id = 0
        self.in_turn = False

    @property
    def policy(self):
        return self.spec.workspace.policy

    def settings(self) -> dict:
        """The ``--settings`` document: hooks, plugins, connectors, and memory off; sandbox per policy."""
        sandboxed = not (self.spec.confined or self.policy.mode == "full")
        sandbox = {"enabled": False}
        if sandboxed:
            sandbox = {"enabled": True, "failIfUnavailable": True, "autoAllowBashIfSandboxed": True,
                       "allowUnsandboxedCommands": self.policy.approvals != "never", "excludedCommands": [],
                       "network": {"allowedDomains": list(self.policy.network), "allowLocalBinding": False}}
        return {"disableAllHooks": True, "disableClaudeAiConnectors": True, "enabledPlugins": {},
                "autoMemoryEnabled": False, "sandbox": sandbox}

    def command(self) -> list[str]:
        policy = self.policy
        if policy.mode == "read-only":
            tools, allowed, mode = READ_TOOLS, READ_TOOLS, "default"
        else:
            tools, allowed = ALL_TOOLS, READ_TOOLS
            if self.spec.confined:
                allowed = "Bash," + allowed  # Fridica's bubblewrap confines the process instead of Claude's sandbox
            mode = "acceptEdits" if policy.approvals != "untrusted" else "default"
            if policy.mode == "full" and policy.approvals == "never":
                mode = "bypassPermissions"
        command = ["claude", "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                   "--setting-sources", "", "--settings", json.dumps(self.settings()),
                   "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--disable-slash-commands",
                   "--no-chrome", "--permission-mode", mode, "--tools", tools, "--allowedTools", allowed,
                   "--append-system-prompt", self.spec.instructions]
        if policy.approvals == "never" or policy.claude_prompts == "none":
            command += ["--permission-prompts", "none"]
        else:
            command += ["--permission-prompt-tool", "stdio"]
        command += ["--resume", self.resume] if self.resume else ["--session-id", self.session]
        if self.spec.model:
            command += ["--model", self.spec.model]
        if self.spec.reasoning_effort:
            command += ["--effort", self.spec.reasoning_effort]
        return command

    def prepare(self, resume: str) -> None:
        # --resume continues an earlier session; --session-id names a new one up front.
        self.resume = resume
        self.session = resume or str(uuid.uuid4())

    def job_prompt(self, brief: str) -> str:
        return f"{brief}\n\n{FORMAT_NOTE}"

    async def handshake(self) -> None:
        self.control_id += 1
        await self.send({"type": "control_request", "request_id": f"fridica-{self.control_id}",
                         "request": {"subtype": "initialize", "hooks": None}})

    async def job(self, prompt: str) -> str:
        await self.send({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}})
        self.in_turn = True
        try:
            while True:
                message = await self.receive()
                kind = message.get("type")
                if isinstance(message.get("session_id"), str) and kind in {"system", "result"}:
                    self.session = message["session_id"]
                if kind == "control_request":
                    await self.control(message)
                elif kind == "result":
                    if message.get("is_error"):
                        detail = " ".join(str(message.get("result") or message.get("subtype") or "error").split())[:500]
                        raise BackendError(f"job failed: {detail}")
                    result = message.get("result")
                    return result if isinstance(result, str) else ""
        finally:
            self.in_turn = False

    async def control(self, message: dict) -> None:
        request = message.get("request") if isinstance(message.get("request"), dict) else {}
        request_id = message.get("request_id")
        if request.get("subtype") != "can_use_tool":
            await self.send({"type": "control_response", "response": {
                "subtype": "error", "request_id": request_id, "error": "not supported by Fridica"}})
            return
        tool = str(request.get("tool_name") or "tool")
        tool_input = request.get("input") if isinstance(request.get("input"), dict) else {}
        target = tool_input.get("command") or tool_input.get("file_path") or tool_input.get("path") or ""
        summary = f"{tool}: {str(target)[:300]}" if target else f"Use {tool}"
        decision = await self.approve(ApprovalRequest(
            "command" if tool == "Bash" else "tool", summary,
            {"tool": tool, "input": tool_input, "reason": request.get("description") or request.get("decision_reason")},
            str(request_id), cache_key=f"{tool}:{target}" if target else ""))
        if decision in (ALLOW_ONCE, ALLOW_SESSION):
            response = {"behavior": "allow", "updatedInput": tool_input}
        else:
            response = {"behavior": "deny", "message": "The owner declined this action; continue without it."}
        await self.send({"type": "control_response", "response": {
            "subtype": "success", "request_id": request_id, "response": response}})

    async def interrupt_backend(self) -> None:
        if not self.alive or not self.in_turn:
            return
        self.control_id += 1
        try:
            await self.send({"type": "control_request", "request_id": f"fridica-{self.control_id}",
                             "request": {"subtype": "interrupt"}})
        except BackendError:
            await self.close()
