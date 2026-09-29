"""Resolve a delegation selector ("a cuda machine", "snowy", "the kintera workspace") to a placement."""

from __future__ import annotations

from dataclasses import dataclass

from ..core.errors import MatchError
from .registry import Machine, Registry, Workspace


@dataclass(frozen=True)
class Selector:
    machine: str = ""
    tags: tuple[str, ...] = ()
    workspace: str = ""
    backend: str = ""


@dataclass(frozen=True)
class Placement:
    machine: Machine
    workspace: Workspace
    backend: str


def resolve(registry: Registry, selector: Selector, *, sticky_machine: str = "", sticky_workspace: str = "",
            busy: dict[str, int] | None = None) -> Placement:
    """Pick a machine, workspace, and backend.

    Precedence: an explicit machine, then capability tags, then the thread's sticky
    machine, then the registry default. When a workspace is named but the chosen
    fallback lacks it, the unique machine that has it wins. Ambiguity is an error
    listing the candidates, so the parent can repair its choice instead of Fridica
    guessing.
    """
    busy = busy or {}
    workspace_name = selector.workspace

    def fits(machine: Machine) -> bool:
        return (set(selector.tags) <= set(machine.tags)
                and (not workspace_name or machine.workspace(workspace_name) is not None)
                and (not selector.backend or selector.backend in machine.backends))

    if selector.machine:
        machine = registry.get(selector.machine)
        if machine is None:
            raise MatchError(f"unknown machine {selector.machine!r}", registry.names)
        missing = set(selector.tags) - set(machine.tags)
        if missing:
            raise MatchError(f"machine {machine.name} lacks {', '.join(sorted(missing))}",
                             tuple(item.name for item in registry.machines if fits(item)))
    elif selector.tags:
        candidates = [item for item in registry.machines if fits(item)]
        if not candidates:
            raise MatchError(f"no machine has {', '.join(selector.tags)}"
                             + (f" and workspace {workspace_name!r}" if workspace_name else ""), registry.names)
        preferred = [item for item in candidates if item.name in (sticky_machine, registry.default)]
        machine = min(preferred or candidates,
                      key=lambda item: (busy.get(item.name, 0) / item.max_jobs, registry.machines.index(item)))
    else:
        fallback = registry.get(sticky_machine) or registry[registry.default]
        machine = fallback
        if workspace_name and fallback.workspace(workspace_name) is None:
            holders = [item for item in registry.machines if item.workspace(workspace_name) is not None]
            if len(holders) != 1:
                raise MatchError(
                    f"workspace {workspace_name!r} is " + ("not configured" if not holders else "on several machines; name one"),
                    tuple(item.name for item in holders) or registry.names)
            machine = holders[0]

    workspace = _workspace(machine, workspace_name, sticky_workspace if machine.name == sticky_machine or not sticky_machine else "")
    backend = selector.backend or machine.default_backend
    if backend not in machine.backends:
        raise MatchError(f"machine {machine.name} has no {backend} backend", machine.backends)
    return Placement(machine, workspace, backend)


def _workspace(machine: Machine, name: str, sticky: str) -> Workspace:
    if name:
        workspace = machine.workspace(name)
        if workspace is None:
            raise MatchError(f"machine {machine.name} has no workspace {name!r}",
                             tuple(item.name for item in machine.workspaces))
        return workspace
    if sticky and machine.workspace(sticky) is not None:
        return machine.workspace(sticky)
    if len(machine.workspaces) == 1:
        return machine.workspaces[0]
    raise MatchError(f"machine {machine.name} has several workspaces; name one",
                     tuple(item.name for item in machine.workspaces))
