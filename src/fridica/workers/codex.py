"""``codex app-server`` over stdio: JSON-RPC on JSONL, one thread per worker, one turn per job.

Approval requests the server raises (commands, file changes, extra permissions) go
to the job's approval handler, which is how the owner decides from the dashboard or
CLI. The turn's ``outputSchema`` makes the final message a WorkerResult.

``app-server`` does not accept ``--ignore-user-config``, so the machine owner's
``~/.codex/config.toml`` (including MCP servers) applies on top of the switches here.
"""

from __future__ import annotations

import json
import logging
from typing import Any

from ..core.errors import BackendError, SessionUnavailable
from .base import JsonlWorker
from .protocol import ALLOW_ONCE, ALLOW_SESSION, ApprovalRequest
from .result import RESULT_SCHEMA

logger = logging.getLogger(__name__)
CLIENT_INFO = {"name": "fridica", "title": "Fridica", "version": "2"}
FEATURES_OFF = (
    'web_search="disabled"', "allow_login_shell=false", "features.apps=false", "features.plugins=false",
    "features.hooks=false", "features.multi_agent=false", "features.browser_use=false", "features.computer_use=false",
    "features.image_generation=false", "features.shell_snapshot=false", "features.memories=false",
    "features.skill_search=false", "features.skip_host_skill_discovery=true", "features.code_mode=false",
)
V2_DECISIONS = {ALLOW_ONCE: "accept", ALLOW_SESSION: "acceptForSession"}
V1_DECISIONS = {ALLOW_ONCE: "approved", ALLOW_SESSION: "approved_for_session"}
APPROVAL_METHODS = {
    "item/commandExecution/requestApproval": "command", "item/fileChange/requestApproval": "file_change",
    "item/permissions/requestApproval": "permissions", "execCommandApproval": "command",
    "applyPatchApproval": "file_change",
}


class CodexWorker(JsonlWorker):
    backend = "codex"

    def __init__(self, spec, transport=None):
        super().__init__(spec, transport)
        self.next_id = 1
        self.turn_id = ""
        self.pending_interrupt = False

    # ----- policy mapping -----

    @property
    def policy(self):
        return self.spec.workspace.policy

    def sandbox_mode(self) -> str:
        if self.spec.confined or self.policy.mode == "full":
            return "danger-full-access"
        return "read-only" if self.policy.mode == "read-only" else "workspace-write"

    def sandbox_policy(self) -> dict[str, Any]:
        network = bool(self.policy.network)
        if self.spec.confined or self.policy.mode == "full":
            return {"type": "dangerFullAccess"}
        if self.policy.mode == "read-only":
            return {"type": "readOnly", "networkAccess": network}
        return {"type": "workspaceWrite", "networkAccess": network, "writableRoots": []}

    def cwd(self) -> dict:
        """The absolute workspace path; a ``~/`` path is left to the process cwd the transport set."""
        path = str(self.spec.workspace.path)
        return {} if path.startswith("~") else {"cwd": path}

    def command(self) -> list[str]:
        settings = [*FEATURES_OFF, "features.code_mode_host=true",
                    "sandbox_workspace_write.network_access=" + ("true" if self.policy.network else "false")]
        if self.policy.approvals == "never":
            settings.append("features.request_permissions_tool=false")
        if self.spec.reasoning_effort:
            settings.append("model_reasoning_effort=" + json.dumps(self.spec.reasoning_effort))
        command = ["codex", "app-server"]
        for setting in settings:
            command += ["-c", setting]
        return command

    # ----- JSON-RPC -----

    async def request(self, method: str, params: dict) -> dict:
        identifier = self.next_id
        self.next_id += 1
        await self.send({"id": identifier, "method": method, "params": params})
        while True:
            message = await self.receive()
            if message.get("id") == identifier and "method" not in message:
                if "error" in message:
                    error = message["error"]
                    detail = error.get("message") if isinstance(error, dict) else error
                    raise BackendError(f"{method} failed: {' '.join(str(detail).split())[:500]}")
                result = message.get("result")
                return result if isinstance(result, dict) else {}
            await self.dispatch(message)

    async def dispatch(self, message: dict) -> None:
        """Answer server requests; notifications outside a turn are ignored."""
        if "method" not in message or "id" not in message:
            return
        method = message["method"]
        kind = APPROVAL_METHODS.get(method)
        if kind is None:
            logger.warning("worker %s: declining unsupported server request %s", self.spec.worker_id, method)
            await self.send({"id": message["id"], "error": {"code": -32601, "message": "not supported by Fridica"}})
            return
        params = message.get("params") if isinstance(message.get("params"), dict) else {}
        decision = await self.approve(describe(kind, params, str(message["id"])))
        await self.send({"id": message["id"], "result": answer(method, kind, params, decision)})

    async def handshake(self) -> None:
        self.next_id = 1
        await self.request("initialize", {"clientInfo": CLIENT_INFO, "capabilities": {"experimentalApi": False}})
        await self.send({"method": "initialized", "params": {}})
        params = {**self.cwd(), "sandbox": self.sandbox_mode(), "approvalPolicy": self.policy.approvals,
                  "developerInstructions": self.spec.instructions}
        if self.spec.model:
            params["model"] = self.spec.model
        thread = None
        if self.resume:
            try:
                thread = (await self.request("thread/resume", {"threadId": self.resume, **params})).get("thread")
            except BackendError as error:
                logger.info("worker %s could not resume thread %s (%s)", self.spec.worker_id, self.resume, error)
                raise SessionUnavailable(str(error)) from None
        if not isinstance(thread, dict) or not isinstance(thread.get("id"), str):
            thread = (await self.request("thread/start", {**params, "ephemeral": False})).get("thread")
        if not isinstance(thread, dict) or not isinstance(thread.get("id"), str):
            raise BackendError("codex app-server did not return a thread")
        self.session = thread["id"]

    async def job(self, prompt: str) -> str:
        params = {"threadId": self.session, "input": [{"type": "text", "text": prompt}], **self.cwd(),
                  "sandboxPolicy": self.sandbox_policy(), "outputSchema": RESULT_SCHEMA}
        self.pending_interrupt = False
        result = await self.request("turn/start", params)
        turn = result.get("turn") if isinstance(result.get("turn"), dict) else {}
        self.turn_id = turn.get("id") or ""
        if self.pending_interrupt:
            await self._send_interrupt()
        report = None
        try:
            while True:
                message = await self.receive()
                method = message.get("method")
                if method is None:
                    continue
                if "id" in message:
                    await self.dispatch(message)
                    continue
                params = message.get("params") if isinstance(message.get("params"), dict) else {}
                mine = (not self.turn_id or params.get("turnId") == self.turn_id
                        or (params.get("turn") or {}).get("id") == self.turn_id)
                if not mine:
                    continue
                if method == "item/completed":
                    item = params.get("item") if isinstance(params.get("item"), dict) else {}
                    if item.get("type") == "agentMessage" and isinstance(item.get("text"), str):
                        report = item["text"]
                elif method == "turn/completed":
                    completed = params.get("turn") if isinstance(params.get("turn"), dict) else {}
                    status = completed.get("status")
                    if status == "interrupted":
                        raise BackendError("the job was interrupted")
                    if status != "completed":
                        error = completed.get("error") if isinstance(completed.get("error"), dict) else {}
                        detail = " ".join(str(error.get("message", status)).split())[:500]
                        raise BackendError(f"turn {status or 'failed'}: {detail}")
                    if report is None:
                        for item in completed.get("items") or []:
                            if isinstance(item, dict) and item.get("type") == "agentMessage":
                                report = item.get("text")
                    return report or ""
                elif method == "error" and params.get("willRetry") is False:
                    error = params.get("error") if isinstance(params.get("error"), dict) else {}
                    raise BackendError("turn failed: " + " ".join(str(error.get("message", "unknown error")).split())[:500])
        finally:
            self.turn_id = ""

    async def interrupt_backend(self) -> None:
        if not self.alive:
            return
        if not self.turn_id:
            self.pending_interrupt = True  # sent as soon as turn/start tells us the turn id
            return
        await self._send_interrupt()

    async def _send_interrupt(self) -> None:
        self.pending_interrupt = False
        try:
            await self.send({"id": self.next_id, "method": "turn/interrupt",
                             "params": {"threadId": self.session, "turnId": self.turn_id}})
            self.next_id += 1
        except BackendError:
            await self.close()


def describe(kind: str, params: dict, request_id: str) -> ApprovalRequest:
    if kind == "command":
        command = params.get("command")
        if isinstance(command, list):
            command = " ".join(str(part) for part in command)
        command = str(command or "a command")
        return ApprovalRequest("command", f"Run `{command[:300]}`", {"command": command, "cwd": params.get("cwd"),
                               "reason": params.get("reason")}, request_id, cache_key=f"command:{command}")
    if kind == "file_change":
        root = params.get("grantRoot")
        summary = f"Write files under {root}" if root else "Apply file changes"
        return ApprovalRequest("file_change", summary, {"reason": params.get("reason"), "grant_root": root,
                               "changes": params.get("changes")}, request_id, cache_key=f"files:{root or ''}")
    return ApprovalRequest("permissions", "Grant extra permissions: " + json.dumps(params.get("permissions"))[:300],
                           {"permissions": params.get("permissions"), "reason": params.get("reason")}, request_id)


def answer(method: str, kind: str, params: dict, decision: str) -> dict:
    if kind == "permissions":
        if decision in (ALLOW_ONCE, ALLOW_SESSION):
            return {"permissions": params.get("permissions") or {},
                    "scope": "session" if decision == ALLOW_SESSION else "turn"}
        return {"permissions": {}}
    if method in ("execCommandApproval", "applyPatchApproval"):
        return {"decision": V1_DECISIONS.get(decision, "denied")}
    return {"decision": V2_DECISIONS.get(decision, "decline")}
