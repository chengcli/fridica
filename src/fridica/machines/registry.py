"""The machine registry: every execution environment a worker may run on.

Agents never construct SSH commands. The parent names a machine (or capability
tags such as ``cuda``) and a workspace; the registry says how to reach it, which
backends it has, what the workers there may do, and how many may run at once.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
import math
from pathlib import PurePath
import re

TRANSPORTS = ("local", "ssh", "slurm")
BACKENDS = ("claude", "codex")
POLICY_MODES = ("read-only", "write", "full")
APPROVAL_MODES = ("never", "on-request", "untrusted", "auto")
CLAUDE_PROMPTS = ("host", "none")
SSH_HOST = re.compile(r"(?:[A-Za-z0-9_][A-Za-z0-9_.-]*@)?[A-Za-z0-9][A-Za-z0-9_.-]*")
NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,31}")
DOMAIN = re.compile(r"\*|(?:\*\.)?[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*")


@dataclass(frozen=True)
class Policy:
    """What workers may do: sandbox mode, network, and how approvals are handled."""
    mode: str = "write"
    network: tuple[str, ...] = ()
    """Domains workers may reach. Claude enforces the list; Codex only supports all-or-nothing, so any entry
    means full network access for Codex workers."""
    approvals: str = "auto"
    """never: refuse anything outside the policy; on-request: ask the owner when the worker needs more than its
    sandbox; untrusted: ask for edits and most commands too; auto: the backend's own AI reviewer decides (Claude's
    auto permission mode, Codex's auto_review), and whatever it escalates comes to the owner."""
    approval_timeout: float = 1800.0
    auto_approve: tuple[str, ...] = ()
    auto_deny: tuple[str, ...] = ()
    gpu_confine: bool | None = None
    """Run the backend with its own sandbox off inside Fridica's bubblewrap, which exposes /dev for GPUs.

    None (the default) means automatic: on for write-mode workspaces of machines that declare ``resources.gpus``,
    because the backends' own sandboxes hide the GPU devices. The loader resolves it to True or False per workspace.
    Confined jobs share the host's network regardless of ``network`` (see ``exec.sandbox``)."""
    claude_prompts: str = "host"
    """host: Claude asks Fridica about tools outside its allowlist; none: those are denied outright."""

    def __post_init__(self) -> None:
        if self.mode not in POLICY_MODES:
            raise ValueError(f"policy.mode must be one of {', '.join(POLICY_MODES)}")
        if self.approvals not in APPROVAL_MODES:
            raise ValueError(f"policy.approvals must be one of {', '.join(APPROVAL_MODES)}")
        if self.claude_prompts not in CLAUDE_PROMPTS:
            raise ValueError("policy.claude_prompts must be host or none")
        if self.gpu_confine is not None and not isinstance(self.gpu_confine, bool):
            raise ValueError("policy.gpu_confine must be true or false")
        if not _positive(self.approval_timeout):
            raise ValueError("policy.approval_timeout must be a positive number of seconds")
        for name in ("network", "auto_approve", "auto_deny"):
            value = getattr(self, name)
            if not isinstance(value, (list, tuple)) or not all(isinstance(item, str) and item for item in value):
                raise ValueError(f"policy.{name} must be a list of nonempty strings")
            object.__setattr__(self, name, tuple(value))
        for domain in self.network:
            if not DOMAIN.fullmatch(domain):
                raise ValueError(f"policy.network entry {domain!r} is not a host name or *.pattern")

    def override(self, values: dict) -> Policy:
        unknown = set(values) - set(POLICY_FIELDS)
        if unknown:
            raise ValueError(f"unknown policy keys: {', '.join(sorted(unknown))}")
        return replace(self, **values)

    @property
    def any_network(self) -> bool:
        return "*" in self.network


POLICY_FIELDS = ("mode", "network", "approvals", "approval_timeout", "auto_approve", "auto_deny", "gpu_confine",
                 "claude_prompts")


@dataclass(frozen=True)
class Resources:
    """Declared (not measured) hardware; shown to the parent and enforced through environment limits."""
    cpus: int | None = None
    gpus: tuple[int, ...] | None = None
    gpu_type: str = ""
    memory_gb: float | None = None
    notes: str = ""

    def __post_init__(self) -> None:
        if self.cpus is not None and (isinstance(self.cpus, bool) or not isinstance(self.cpus, int) or self.cpus < 1):
            raise ValueError("resources.cpus must be a positive integer")
        if self.gpus is not None:
            if not isinstance(self.gpus, (list, tuple)) or any(
                    isinstance(index, bool) or not isinstance(index, int) or index < 0 for index in self.gpus):
                raise ValueError("resources.gpus must list GPU device indices such as [0, 1]")
            if len(set(self.gpus)) != len(self.gpus):
                raise ValueError("resources.gpus lists a device twice")
            object.__setattr__(self, "gpus", tuple(self.gpus))
        if self.memory_gb is not None and not _positive(self.memory_gb):
            raise ValueError("resources.memory_gb must be a positive number")
        for name in ("gpu_type", "notes"):
            value = getattr(self, name)
            if not isinstance(value, str) or len(value) > 1000:
                raise ValueError(f"resources.{name} must be a string of at most 1000 characters")

    def payload(self) -> dict:
        data = {"cpus": self.cpus, "gpus": list(self.gpus) if self.gpus is not None else None,
                "gpu_type": self.gpu_type, "memory_gb": self.memory_gb, "notes": self.notes}
        return {key: value for key, value in data.items() if value not in (None, "")}

    def environment(self) -> dict[str, str]:
        variables = {}
        if self.cpus is not None:
            variables["OMP_NUM_THREADS"] = str(self.cpus)
        if self.gpus is not None:
            variables["CUDA_VISIBLE_DEVICES"] = ",".join(str(index) for index in self.gpus)
        return variables


@dataclass(frozen=True)
class Workspace:
    name: str
    path: PurePath
    policy: Policy
    """Effective policy: the machine's, with this workspace's overrides applied."""

    @property
    def writable(self) -> bool:
        return self.policy.mode != "read-only"


@dataclass(frozen=True)
class Slurm:
    account: str = ""
    partition: str = ""
    gres: str = ""
    time: str = ""
    extra: tuple[str, ...] = ()


@dataclass(frozen=True)
class Machine:
    name: str
    transport: str
    workspaces: tuple[Workspace, ...]
    backends: tuple[str, ...]
    default_backend: str
    policy: Policy
    host: str = ""
    """SSH destination (an ~/.ssh/config alias or user@host); for slurm, the login host."""
    tags: tuple[str, ...] = ()
    resources: Resources = Resources()
    max_workers: int = 2
    max_jobs: int = 1
    slurm: Slurm | None = None
    description: str = ""

    @property
    def remote(self) -> bool:
        return self.transport != "local"

    def workspace(self, name: str) -> Workspace | None:
        return next((item for item in self.workspaces if item.name == name), None)

    def payload(self, busy: int = 0) -> dict:
        """What the parent sees: names and capabilities, never filesystem paths."""
        data = {"name": self.name, "tags": list(self.tags), "backends": list(self.backends),
                "default_backend": self.default_backend,
                "workspaces": {item.name: item.policy.mode for item in self.workspaces},
                "resources": self.resources.payload(), "busy_jobs": busy, "max_jobs": self.max_jobs}
        if self.description:
            data["description"] = self.description
        return data


@dataclass(frozen=True)
class Registry:
    machines: tuple[Machine, ...]
    default: str
    _index: dict = field(default=None, compare=False, repr=False)

    def __post_init__(self) -> None:
        if not self.machines:
            raise ValueError("configure at least one [machines.<name>] table")
        names = [machine.name for machine in self.machines]
        if len(set(names)) != len(names):
            raise ValueError("machine names must be unique")
        if self.default not in names:
            raise ValueError(f"default machine {self.default!r} is not configured")
        object.__setattr__(self, "_index", {machine.name: machine for machine in self.machines})

    def get(self, name: str) -> Machine | None:
        return self._index.get(name)

    def __getitem__(self, name: str) -> Machine:
        return self._index[name]

    @property
    def names(self) -> tuple[str, ...]:
        return tuple(self._index)

    def payload(self, busy: dict[str, int] | None = None) -> list[dict]:
        busy = busy or {}
        return [machine.payload(busy.get(machine.name, 0)) for machine in self.machines]


def _positive(value) -> bool:
    return not isinstance(value, bool) and isinstance(value, (int, float)) and math.isfinite(value) and value > 0
