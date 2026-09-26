"""Fetch an owner-granted GitHub ref into a new bare repository before a worker starts."""

from __future__ import annotations

from pathlib import PurePath
import re

from ..core.errors import BackendError
from ..exec.transport import Transport
from ..machines.registry import Policy, valid_fetch_ref

JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
COMMIT = re.compile(r"[a-fA-F0-9]{40,64}")
GIT_ENV = {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_COUNT": "0",
           "GIT_CONFIG_PARAMETERS": "", "GIT_TERMINAL_PROMPT": "0", "GIT_ASKPASS": "/usr/bin/false",
           "GIT_SSH_COMMAND": "/usr/bin/false"}
ISOLATED_GIT = ('exec /usr/bin/env -i "HOME=$HOME" "PATH=$PATH" '
                + " ".join(f"{name}={value}" for name, value in GIT_ENV.items()) + ' "$@"')


async def fetch_repo(transport: Transport, workspace: PurePath, policy: Policy, repo: str, ref: str,
                     job_id: str, *, create: bool = False) -> tuple[str, str]:
    granted = next((entry for entry in policy.fetch_repos if entry.casefold() == repo.casefold()), "")
    if not granted:
        raise ValueError(f"repository {repo!r} is not granted for fetch")
    if not valid_fetch_ref(ref):
        raise ValueError("invalid GitHub ref for fetch")
    return await _fetch_ref(transport, workspace, f"https://github.com/{granted}.git", ref, job_id, create=create)


async def _fetch_ref(transport: Transport, workspace: PurePath, url: str, ref: str, job_id: str,
                     *, create: bool = False) -> tuple[str, str]:
    if not JOB_ID.fullmatch(job_id):
        raise ValueError("invalid job ID for fetch")
    leaf = f".fridica-fetch-{job_id}"

    async def git(command: list[str], timeout: float = 30, *, first: bool = False) -> bytes:
        if command[0] == "git":
            command = ["sh", "-c", ISOLATED_GIT, "fridica-git", *command]
        result = await transport.run(command, workspace, timeout=timeout, create=create and first)
        if result.returncode:
            raise BackendError("repository fetch failed; check the allowed repo and ref")
        return result.stdout

    await git(["mkdir", "-m", "700", leaf], first=True)  # exclusive; never reuse an agent-controlled Git directory
    await git(["git", "init", "--bare", "-q", leaf])
    await git(["git", "-C", leaf, "-c", "http.followRedirects=false", "fetch", "--quiet", "--no-tags",
               "--no-recurse-submodules", url, ref], timeout=600)
    sha = (await git(["git", "-C", leaf, "rev-parse", "--verify", "FETCH_HEAD^{commit}"])).decode().strip()
    if not COMMIT.fullmatch(sha):
        raise BackendError("repository fetch returned an invalid commit")
    return str(workspace / leaf), sha
