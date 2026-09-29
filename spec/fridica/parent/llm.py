"""One tool-less, stateless structured-output call to the local Claude or Codex CLI.

The parent never runs tools: every call gets a JSON schema, no tools, no MCP, no
session persistence, and a throwaway working directory. Its context is rebuilt
from the thread session each time, so nothing accumulates in a backend session.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
import tempfile
from typing import Protocol

from ..core.errors import BackendError
from ..exec import process

logger = logging.getLogger(__name__)
FEATURES_OFF = (
    'approval_policy="never"', 'web_search="disabled"', "allow_login_shell=false", "features.apps=false",
    "features.plugins=false", "features.hooks=false", "features.multi_agent=false", "features.browser_use=false",
    "features.computer_use=false", "features.image_generation=false", "features.shell_snapshot=false",
    "features.memories=false", "features.skill_search=false", "features.skip_host_skill_discovery=true",
    "features.code_mode=false", "features.code_mode_host=false", "features.request_permissions_tool=false",
    "features.shell_tool=false", "features.unified_exec=false", "features.view_image=false", "project_doc_max_bytes=0",
    "sandbox_workspace_write.network_access=false",
)
CLAUDE_SETTINGS = {"disableAllHooks": True, "disableClaudeAiConnectors": True, "enabledPlugins": {},
                   "autoMemoryEnabled": False}


class StructuredLLM(Protocol):
    backend: str

    async def call(self, prompt: str, schema: dict, *, model: str = "") -> dict: ...


class CliLLM:
    backend = ""

    def __init__(self, *, model: str = "", reasoning_effort: str = "", timeout: float = 180.0,
                 excluded_env: tuple[str, ...] = ()):
        self.model = model
        self.reasoning_effort = reasoning_effort
        self.timeout = timeout
        self.excluded_env = excluded_env

    def command(self, directory: Path, schema: str, model: str) -> list[str]:
        raise NotImplementedError

    def parse(self, output: str) -> dict:
        raise NotImplementedError

    async def call(self, prompt: str, schema: dict, *, model: str = "") -> dict:
        schema_text = json.dumps(schema)
        with tempfile.TemporaryDirectory(prefix="fridica-parent-") as temporary:
            directory = Path(temporary)
            (directory / "schema.json").write_text(schema_text)
            command = self.command(directory, schema_text, model or self.model)
            completed = await process.run_once(command, stdin=prompt.encode(), cwd=directory,
                                               env=process.scrubbed_environment(self.excluded_env), timeout=self.timeout)
        if completed.returncode:
            raise BackendError(f"{self.backend} exited with status {completed.returncode}: "
                               f"{process.diagnostic(completed.stderr)}")
        return self.parse(completed.text)


class ClaudeLLM(CliLLM):
    backend = "claude"

    def command(self, directory: Path, schema: str, model: str) -> list[str]:
        command = ["claude", "-p", "--output-format", "json", "--json-schema", schema, "--setting-sources", "",
                   "--settings", json.dumps(CLAUDE_SETTINGS), "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
                   "--disable-slash-commands", "--no-chrome", "--permission-mode", "dontAsk", "--tools", "",
                   "--no-session-persistence"]
        if model:
            command += ["--model", model]
        if self.reasoning_effort:
            command += ["--effort", self.reasoning_effort]
        return command

    def parse(self, output: str) -> dict:
        try:
            envelope = json.loads(output)
        except ValueError:
            raise BackendError("claude returned output that is not JSON") from None
        if not isinstance(envelope, dict) or envelope.get("is_error"):
            raise BackendError("claude reported an error: " + str(envelope.get("result", ""))[:300]
                               if isinstance(envelope, dict) else "claude returned an unexpected envelope")
        if envelope.get("permission_denials"):
            raise BackendError("the tool-less parent call attempted to use tools")
        result = envelope.get("structured_output")
        if not isinstance(result, dict):
            raise BackendError("claude did not return structured output")
        return result


class CodexLLM(CliLLM):
    backend = "codex"

    def command(self, directory: Path, schema: str, model: str) -> list[str]:
        command = ["codex", "exec", "--ignore-user-config", "--ignore-rules", "--ephemeral", "--skip-git-repo-check",
                   "--sandbox", "read-only", "--output-schema", str(directory / "schema.json"), "--json",
                   "--color", "never"]
        settings = list(FEATURES_OFF)
        if self.reasoning_effort:
            settings.append("model_reasoning_effort=" + json.dumps(self.reasoning_effort))
        for setting in settings:
            command += ["-c", setting]
        if model:
            command += ["--model", model]
        return command + ["-"]

    def parse(self, output: str) -> dict:
        text = None
        for line in output.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if not isinstance(event, dict):
                continue
            item = event.get("item") if isinstance(event.get("item"), dict) else {}
            if item.get("type") not in (None, "reasoning", "agent_message", "error"):
                raise BackendError("the tool-less parent call attempted to use tools")
            if event.get("type") == "item.completed" and item.get("type") == "agent_message":
                text = item.get("text")
        if not isinstance(text, str) or not text:
            raise BackendError("codex did not return a structured response")
        try:
            result = json.loads(text)
        except ValueError:
            raise BackendError("codex returned a response that is not JSON") from None
        if not isinstance(result, dict):
            raise BackendError("codex returned a response that is not an object")
        return result


def make_llm(backend: str, **options) -> CliLLM:
    return {"claude": ClaudeLLM, "codex": CodexLLM}[backend](**options)
