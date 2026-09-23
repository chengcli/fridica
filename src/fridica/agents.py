"""Agent backends: how one Claude Code or Codex run is launched, parsed, and resumed.

``CLIBackend`` implements the ``AgentBackend`` protocol on top of a CLI. Its two
subclasses only differ in how they build the command line, extract a session
identifier, and read the structured result. Subprocess plumbing lives in
``runner``, prompts and schemas in ``prompts``, and environment checks in
``checks``; the names used by older imports are re-exported at the bottom.
"""
from __future__ import annotations

from dataclasses import replace
import json
import logging
from pathlib import Path, PurePath
import re
import shutil  # noqa: F401  (patched by tests through this module)
import subprocess  # noqa: F401
import sys  # noqa: F401
import tempfile
import uuid

from .config import Config
from .contract import Contract, load_contract
from .models import AgentBackend, AgentResult, ConversationContext, Decision, Message
from .repos import Repo, load_repos
from .prompts import (CLASSIFICATION_SCHEMA, DEBRIEF_SCHEMA, ESCALATE_LIMIT, FILE_PLAN_SCHEMA, REPLY_LIMIT, REPORT_LIMIT,
                      RESPONSE_SCHEMA, SUMMARY_SCHEMA, checked_details, conversation_prompt, digest_prompt, plan_prompt, truncate,
                      worker_prompt)
from . import remote
from .runner import OUTPUT_LIMIT, BackendError, SessionUnavailable
from .runner import run as _run  # module attribute so tests can substitute the subprocess runner
from .worker import ClaudeWorker, CodexWorker, Workers

logger = logging.getLogger(__name__)
SESSION_ID = re.compile(r"[0-9A-Za-z][0-9A-Za-z_-]{7,63}")
UNAVAILABLE = "I couldn't complete this request. Please check Fridica locally before retrying; partial changes may exist."
FAILURES = (BackendError, ValueError, KeyError, TypeError, TimeoutError, OSError)


def _valid_session(value: object) -> str | None:
    return value if isinstance(value, str) and SESSION_ID.fullmatch(value) else None


def _describe_denials(denials: object) -> str:
    """Summarise denied tool calls for the local log: tool name plus the command's first word or the target path."""
    parts = []
    for denial in (denials if isinstance(denials, list) else [])[:5]:
        if not isinstance(denial, dict):
            continue
        name = str(denial.get("tool_name", "?"))
        params = denial.get("tool_input") if isinstance(denial.get("tool_input"), dict) else {}
        command = params.get("command")
        target = command.split(maxsplit=1)[0] if isinstance(command, str) and command.split() else ""
        target = target or params.get("file_path") or params.get("path") or ""
        target = " ".join(str(target).split())[:120]
        parts.append(f"{name}({target})" if target else name)
    return ", ".join(parts) or "unknown tool"


class CLIBackend:
    """Shared control flow for CLI-backed agents; subclasses supply the command line and parsing."""

    worker_type = None  # the persistent heavy-task worker class for this CLI

    def __init__(self, config: Config):
        self.config = config
        self.workers = Workers(config, self.worker_type)

    def contract(self) -> Contract:
        """Reload the owner's contract so edits apply to the next run."""
        return load_contract(self.config.contract)

    def repositories(self) -> tuple[Repo, ...]:
        """Reload the owner's repository list so edits apply to the next run."""
        return load_repos(self.config.repos)

    # ----- the four calls the replica makes -----

    async def classify(self, message: Message, context: ConversationContext) -> Decision:
        try:
            prompt = conversation_prompt(message, context, True, self.contract(), self.repositories())
            result, _session = await self._invoke(prompt, True)
            return Decision(result["decision"])
        except FAILURES:
            return Decision.OBSERVE

    async def respond(self, message: Message, context: ConversationContext) -> AgentResult:
        try:
            contract, repositories = self.contract(), self.repositories()
            session = context.session if self.config.resume_sessions else None
            options = {"heavy": self.config.heavy_tasks, "resources": self.config.resources.payload(),
                       "hosts": [host.payload() for host in self.config.heavy_hosts]}
            try:
                prompt = conversation_prompt(message, context, False, contract, repositories, **options)
                result, session = await self._invoke(prompt, False, session)
            except SessionUnavailable:
                if session is None:
                    raise
                logger.info("Session for task %s is no longer available; starting a new one", context.task_id)
                fresh = conversation_prompt(message, replace(context, session=None), False, contract, repositories, **options)
                result, session = await self._invoke(fresh, False, None)
            return self._result(result, session)
        except FAILURES as error:
            logger.warning("Agent response unavailable (%s): %s", type(error).__name__, error or "no detail")
            return AgentResult(text=UNAVAILABLE, status="blocked")

    async def summarize(self, context: ConversationContext) -> str:
        """Summarize a thread that hit its turn limit; a stateless, tool-less run governed by ``## Thread summaries``."""
        return await self._digest(context, self.contract().summaries, SUMMARY_SCHEMA, "summary")

    async def debrief(self, context: ConversationContext) -> str:
        """Write the closing debrief of a finished discussion; a stateless, tool-less run governed by ``## Debriefs``."""
        return await self._digest(context, self.contract().debriefs, DEBRIEF_SCHEMA, "debrief")

    async def plan(self, message, context, files, roots, *, feedback: str = "") -> dict:
        """Propose one scoped file operation (file-access mode); tool-less and stateless.

        ``feedback`` is why the controller rejected the previous plan, so the model can correct it.
        """
        prompt = plan_prompt(message, context, self.contract(), files, roots, self.repositories(),
                             heavy=self.config.heavy_tasks, hosts=[host.payload() for host in self.config.heavy_hosts],
                             feedback=feedback)
        result, _session = await self._invoke(prompt, True, schema=FILE_PLAN_SCHEMA)
        return result

    async def work(self, brief: str, context: ConversationContext, resume: str | None, host: str = "") -> tuple[str, str | None]:
        """Run an escalated brief on this thread's persistent worker for ``host`` and return its report and thread id.

        Failures propagate as ``BackendError`` (or ``TimeoutError``) so the replica can
        tell the thread that the job did not finish; nothing is retried.
        """
        target = self.config.host(host) if host else self.config.heavy_hosts[0]
        prompt = worker_prompt(brief, context, self.contract(), self.repositories(), target.resources.payload(), target.payload())
        report, thread = await self.workers.get(context.task_id, target).run(prompt, resume)
        return truncate(report, REPORT_LIMIT), thread

    async def close(self) -> None:
        await self.workers.close()

    # ----- shared mechanics -----

    def _result(self, result: dict, session: str | None) -> AgentResult:
        """Validate a structured reply and turn it into an ``AgentResult``."""
        text = result.get("text")
        send = result.get("send", True)
        if type(send) is not bool or not isinstance(text, str) or (send and not text.strip()) or len(text) > REPLY_LIMIT:
            raise BackendError("Agent returned an invalid response.")
        from .collaboration import validate
        update = result.get("update")
        if update is not None:
            update = validate(update)
        if result.get("status") not in {"complete", "waiting", "blocked"}:
            raise BackendError("Agent returned an invalid status.")
        finished = result.get("discussion") == "finished" and result["status"] == "complete"
        escalate = result.get("escalate", "")
        host = result.get("escalate_host", "")
        if not isinstance(escalate, str) or len(escalate) > ESCALATE_LIMIT or not isinstance(host, str):
            raise BackendError("Agent returned an invalid heavy-task brief.")
        escalate, host = self.config.route_escalation(escalate, host)
        try:
            details = checked_details(result.get("details", ""))
        except ValueError:
            raise BackendError("Agent returned invalid details.") from None
        return AgentResult(text=text, status=result["status"], session=session, finished=finished and send, send=send,
                           update=update, escalate=escalate, escalate_host=host, details=details)

    async def _digest(self, context: ConversationContext, instruction: str, schema: dict, key: str) -> str:
        result, _session = await self._invoke(digest_prompt(context, instruction), True, schema=schema)
        text = result.get(key)
        if not isinstance(text, str) or not text.strip():
            raise BackendError(f"Agent returned an empty {key}.")
        return truncate(text)

    async def _invoke(self, prompt: str, classify: bool, session: str | None = None, *,
                      schema: dict | None = None) -> tuple[dict, str | None]:
        """Run one agent turn; return its structured result and the session that now holds the thread.

        ``classify`` selects the stateless, tool-less command shape used for
        classification, summaries, debriefs, and file plans. ``session`` is an existing
        backend session to resume; when it is None and continuity is enabled, a fresh
        session is started and its identifier is returned so the caller can persist it.
        """
        if session is not None and not SESSION_ID.fullmatch(session):
            raise BackendError("Stored session identifier is malformed.")
        persist = not classify and self.config.resume_sessions
        resume = persist and session is not None
        if persist and not resume:
            session = str(uuid.uuid4())
        schema_text = json.dumps(schema or (CLASSIFICATION_SCHEMA if classify else RESPONSE_SCHEMA))
        if self.config.remote:
            # The scratch directory and schema file are created on the SSH host by the
            # wrapper script; nothing about the run touches the local filesystem.
            directory = remote.remote_directory()
            command = self.command(directory, schema_text, classify, session if persist else None, resume)
            argv, cwd = remote.launch(self.config, command, directory if classify else self.config.workspace,
                                      files={"schema.json": schema_text}, directory=directory,
                                      timeout=self.config.timeout)
            output = await _run(argv, prompt, cwd, self.config)
        else:
            with tempfile.TemporaryDirectory(prefix="fridica-agent-") as temporary:
                directory = Path(temporary)
                (directory / "schema.json").write_text(schema_text)
                command = self.command(directory, schema_text, classify, session if persist else None, resume)
                output = await _run(command, prompt, directory if classify else self.config.workspace, self.config)
        if classify:
            self.verify_stateless(output)
        return self.parse(output), (self.session_id(output) if persist else None)

    # ----- backend-specific hooks -----

    def command(self, directory: PurePath, schema: str, classify: bool,
                session: str | None = None, resume: bool = False) -> list[str]:
        """The CLI argv for one run. ``directory`` is the run's scratch directory (which holds
        ``schema.json`` with the text ``schema``) on the host where the CLI runs."""
        raise NotImplementedError

    def parse(self, output: str) -> dict:
        """The structured result of a completed run, read from its stdout."""
        raise NotImplementedError

    def session_id(self, output: str) -> str | None:
        """Extract the backend's session identifier from a completed run, if any."""
        return None

    def verify_stateless(self, output: str) -> None:
        """Raise if a tool-less run shows evidence of tool use."""


class CodexBackend(CLIBackend):
    worker_type = CodexWorker
    FEATURES_OFF = [
        'approval_policy="never"', 'web_search="disabled"', "allow_login_shell=false",
        "features.apps=false", "features.plugins=false", "features.hooks=false",
        "features.multi_agent=false", "features.browser_use=false", "features.computer_use=false",
        "features.image_generation=false", "features.shell_snapshot=false",
        "features.memories=false", "features.skill_search=false",
        "features.skip_host_skill_discovery=true", "features.code_mode=false",
        "features.request_permissions_tool=false",
    ]
    TOOLS_OFF = ["features.shell_tool=false", "features.unified_exec=false",
                 "features.view_image=false", "project_doc_max_bytes=0"]

    def command(self, directory: PurePath, schema: str, classify: bool,
                session: str | None = None, resume: bool = False) -> list[str]:
        persist = not classify and session is not None
        # The structured reply is the final agent_message in the --json stream, so no
        # result file needs to be read back from the host that ran the command.
        outputs = ["--output-schema", str(directory / "schema.json"), "--json"]
        if persist and resume:
            # `codex exec resume` lacks --sandbox, --add-dir, and --color; the equivalent
            # settings are supplied through -c so the resumed turn keeps the same policy.
            command = ["codex", "exec", "resume", session, "--ignore-user-config", "--ignore-rules",
                       "--skip-git-repo-check", *outputs]
        else:
            command = [
                "codex", "exec", "--ignore-user-config", "--ignore-rules",
                *([] if persist else ["--ephemeral"]),
                "--skip-git-repo-check",
                *([] if classify and self.config.file_access else
                  ["--sandbox", "read-only" if classify else "workspace-write"]),
                *outputs, "--color", "never",
            ]
        planner = classify and self.config.file_access
        settings = list(self.FEATURES_OFF)
        settings.append("features.code_mode_host=" + ("false" if classify else "true"))
        if not planner:
            network = "true" if not classify and self.config.allowed_domains else "false"
            settings.append(f"sandbox_workspace_write.network_access={network}")
        if classify:
            settings += self.TOOLS_OFF
        if planner:
            command += ["--strict-config"]
            settings += [
                'default_permissions="fridica_planner"',
                'permissions.fridica_planner.filesystem={":minimal"="read",'
                + json.dumps(str(directory.resolve())) + '="read"}',
                'permissions.fridica_planner.network.enabled=false',
            ]
        if persist and resume:
            settings.append('sandbox_mode="workspace-write"')
            if self.config.additional_workspaces:
                roots = json.dumps([str(workspace) for workspace in self.config.additional_workspaces])
                settings.append(f"sandbox_workspace_write.writable_roots={roots}")
        if self.config.reasoning_effort:
            settings.append("model_reasoning_effort=" + json.dumps(self.config.reasoning_effort))
        for setting in settings:
            command += ["-c", setting]
        if self.config.model:
            command += ["--model", self.config.model]
        if not classify and not (persist and resume):
            for workspace in self.config.additional_workspaces:
                command += ["--add-dir", str(workspace)]
        return command + ["-"]

    def parse(self, output: str) -> dict:
        text = None
        for event in self._events(output):
            item = event.get("item")
            if event.get("type") == "item.completed" and isinstance(item, dict) and item.get("type") == "agent_message":
                text = item.get("text")
        if not isinstance(text, str) or not text or len(text) > OUTPUT_LIMIT:
            raise BackendError("Codex did not return a bounded structured response.")
        try:
            result = json.loads(text)
        except ValueError:
            raise BackendError("Codex returned an invalid response.") from None
        if not isinstance(result, dict):
            raise BackendError("Codex returned an invalid response.")
        return result

    def session_id(self, output: str) -> str | None:
        for event in self._events(output):
            if event.get("type") == "thread.started":
                return _valid_session(event.get("thread_id"))
        return None

    def verify_stateless(self, output: str) -> None:
        for line in output.splitlines():
            item = json.loads(line).get("item", {})
            if item.get("type") not in {None, "reasoning", "agent_message", "error"}:
                raise BackendError("Classifier attempted to use tools.")

    @staticmethod
    def _events(output: str):
        for line in output.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if isinstance(event, dict):
                yield event


class ClaudeBackend(CLIBackend):
    worker_type = ClaudeWorker

    @staticmethod
    def settings(config: Config, classify: bool) -> dict:
        """The ``--settings`` document: hooks, plugins, connectors and memory off; the sandbox mandatory."""
        return {
            "disableAllHooks": True, "disableClaudeAiConnectors": True,
            "enabledPlugins": {}, "autoMemoryEnabled": False,
            "sandbox": {
                "enabled": True, "failIfUnavailable": True,
                "autoAllowBashIfSandboxed": True, "allowUnsandboxedCommands": False,
                "excludedCommands": [],
                "network": {"allowedDomains": [] if classify else list(config.allowed_domains),
                            "allowLocalBinding": False},
            },
        }

    def command(self, directory: PurePath, schema: str, classify: bool,
                session: str | None = None, resume: bool = False) -> list[str]:
        command = [
            "claude", "-p", "--output-format", "json", "--json-schema", schema,
            "--setting-sources", "", "--settings", json.dumps(self.settings(self.config, classify)),
            "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
            "--disable-slash-commands", "--no-chrome",
            "--permission-mode", "dontAsk" if classify else "acceptEdits",
            "--tools", "" if classify else "Bash,Read,Glob,Grep,Edit,Write",
        ]
        if classify or session is None:
            command += ["--no-session-persistence"]
        else:
            command += ["--resume" if resume else "--session-id", session]
        if not classify:
            command += ["--allowedTools", "Read,Glob,Grep"]
            for workspace in self.config.additional_workspaces:
                command += ["--add-dir", str(workspace)]
        if self.config.model:
            command += ["--model", self.config.model]
        return command

    def session_id(self, output: str) -> str | None:
        try:
            envelope = json.loads(output)
        except ValueError:
            return None
        return _valid_session(envelope.get("session_id")) if isinstance(envelope, dict) else None

    def parse(self, output: str) -> dict:
        envelope = json.loads(output)
        if not isinstance(envelope, dict) or envelope.get("is_error"):
            raise BackendError("Claude reported an execution error.")
        denials = envelope.get("permission_denials")
        if denials:
            # Denied tool calls are a normal part of a sandboxed run: Claude is told about each
            # denial and adapts, and the contract requires it to report what it could not do.
            logger.warning("Claude was denied %d tool call(s) and continued without them: %s",
                           len(denials) if isinstance(denials, list) else 1, _describe_denials(denials))
        result = envelope.get("structured_output")
        if not isinstance(result, dict):
            raise BackendError("Claude did not return a structured response.")
        return result


def create_backend(config: Config) -> AgentBackend:
    if config.backend == "codex":
        return CodexBackend(config)
    if config.backend == "claude":
        return ClaudeBackend(config)
    raise ValueError(f"Unsupported backend: {config.backend}")


# Names kept for callers and tests written against the previous single-module layout.
from .checks import check_authentication, check_backend, check_sandbox  # noqa: E402,F401
from .runner import DIAGNOSTIC_LIMIT, diagnostic as _diagnostic  # noqa: E402,F401
from .prompts import conversation_prompt as _prompt  # noqa: E402,F401
