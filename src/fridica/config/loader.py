"""TOML → :class:`Config`, with every cross-field rule in one place."""

from __future__ import annotations

from dataclasses import fields, replace
import hashlib
import math
import os
from pathlib import Path, PurePath, PurePosixPath
import re
import tomllib
import unicodedata

from ..core.errors import ConfigError
from ..machines.registry import (
    BACKENDS, NAME, POLICY_FIELDS, SSH_HOST, TRANSPORTS, Machine, Policy, Registry, Resources, Slurm, Workspace,
)
from .schema import (
    DEFAULT_STATE, REASONING_EFFORTS, Config, Limits, OwnerConfig, ParentConfig, SlackConfig, StateConfig,
)

USER_ID = re.compile(r"[UW][A-Z0-9]+")
TEAM_ID = re.compile(r"T[A-Z0-9]+")
CHANNEL_ID = re.compile(r"[CG][A-Z0-9]+")
ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
SOCKET_PATH_LIMIT = 100
TOP_LEVEL = {"owner", "slack", "parent", "limits", "policy", "machines", "state"}
MACHINE_DEFAULTS = {field.name: field.default for field in fields(Machine)}
MACHINE_KEYS = {"transport", "host", "tags", "backends", "default_backend", "max_workers", "max_jobs", "policy",
                "resources", "slurm", "description", "workspaces"}


def load_config(path: Path) -> Config:
    path = path.expanduser()
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        raise ConfigError(f"{path} does not exist; run fridica init") from None
    try:
        data = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise ConfigError(f"{path}: {error}") from None
    try:
        config = parse(data, base=path.parent)
    except ConfigError as error:
        raise ConfigError(f"{path}: {error}") from None
    except (TypeError, ValueError) as error:
        raise ConfigError(f"{path}: {error}") from None
    config = replace(config, path=path.resolve(), fingerprint=hashlib.sha256(raw).hexdigest())
    _protect(config)
    return config


def parse(data: dict, *, base: Path) -> Config:
    _keys(data, TOP_LEVEL, "top level")
    for required in ("owner", "slack", "machines"):
        if required not in data:
            raise ConfigError(f"missing [{required}] section")
    owner = _owner(_table(data, "owner"), base)
    slack = _slack(_table(data, "slack"))
    parent_data = _table(data, "parent")
    policy = Policy(**_subset(_table(data, "policy"), POLICY_FIELDS, "[policy]"))
    limits = _limits(_table(data, "limits"))
    state = _state(_table(data, "state"), base)
    parent_backend = parent_data.get("backend", ParentConfig.backend)
    machines = tuple(_machine(name, _table(data["machines"], name), policy, parent_backend, base)
                     for name in _table(data, "machines"))
    default = parent_data.get("default_machine") or (machines[0].name if machines else "")
    registry = Registry(machines, default)
    parent = _parent(parent_data, base, default)
    config = Config(owner=owner, slack=slack, machines=registry, parent=parent, limits=limits, policy=policy,
                    state=state)
    _cross_checks(config)
    return config


def _owner(data: dict, base: Path) -> OwnerConfig:
    _keys(data, {"slack_user", "profile", "contract"}, "[owner]")
    user = data.get("slack_user")
    if not isinstance(user, str) or not USER_ID.fullmatch(user):
        raise ConfigError("owner.slack_user must be a Slack member ID such as U012ABCDEF")
    profile = data.get("profile", "")
    if not isinstance(profile, str) or len(profile) > 4000:
        raise ConfigError("owner.profile must be a string of at most 4000 characters")
    contract = _file(data["contract"], base, "owner.contract") if "contract" in data else None
    if contract is None and (base / "contract.md").is_file():
        contract = (base / "contract.md").resolve()
    return OwnerConfig(user, profile, contract)


def _slack(data: dict) -> SlackConfig:
    _keys(data, {field.name for field in fields(SlackConfig)}, "[slack]")
    workspace = data.get("workspace")
    if not isinstance(workspace, str) or not TEAM_ID.fullmatch(workspace):
        raise ConfigError("slack.workspace must be a Slack team ID such as T012ABCDEF")
    channels = _ids(data.get("channels"), "slack.channels")
    if not channels:
        raise ConfigError("slack.channels must list at least one channel ID")
    delegate = data.get("delegate_channels")
    if delegate is not None:
        delegate = _ids(delegate, "slack.delegate_channels")
        if not set(delegate) <= set(channels):
            raise ConfigError("slack.delegate_channels must be a subset of slack.channels")
    values = {"workspace": workspace, "channels": channels, "delegate_channels": delegate}
    for name in ("app_token_env", "user_token_env"):
        if name in data:
            if not isinstance(data[name], str) or not ENV_NAME.fullmatch(data[name]):
                raise ConfigError(f"slack.{name} must be an environment variable name")
            values[name] = data[name]
    if "general_messages" in data:
        values["general_messages"] = _bool(data["general_messages"], "slack.general_messages")
    if "cooldown" in data:
        values["cooldown"] = _number(data["cooldown"], "slack.cooldown", minimum=0)
    return SlackConfig(**values)


def _parent(data: dict, base: Path, default_machine: str) -> ParentConfig:
    _keys(data, {field.name for field in fields(ParentConfig)}, "[parent]")
    values: dict = {"default_machine": default_machine}
    if "backend" in data:
        if data["backend"] not in BACKENDS:
            raise ConfigError("parent.backend must be claude or codex")
        values["backend"] = data["backend"]
    for name in ("model", "triage_model"):
        if name in data:
            if not isinstance(data[name], str) or len(data[name]) > 200:
                raise ConfigError(f"parent.{name} must be a model name")
            values[name] = data[name]
    if "reasoning_effort" in data:
        if data["reasoning_effort"] not in REASONING_EFFORTS:
            raise ConfigError(f"parent.reasoning_effort must be one of {', '.join(REASONING_EFFORTS[1:])}")
        values["reasoning_effort"] = data["reasoning_effort"]
    if "timeout" in data:
        values["timeout"] = _number(data["timeout"], "parent.timeout", minimum=1)
    if "context_chars" in data:
        values["context_chars"] = _integer(data["context_chars"], "parent.context_chars", minimum=2000)
    if "repos" in data:
        values["repos"] = _file(data["repos"], base, "parent.repos")
    return ParentConfig(**values)


def _limits(data: dict) -> Limits:
    allowed = {field.name: field for field in fields(Limits)}
    _keys(data, set(allowed), "[limits]")
    values = {}
    for name, value in data.items():
        default = allowed[name].default
        if isinstance(default, bool):
            values[name] = _bool(value, f"limits.{name}")
        elif isinstance(default, int):
            values[name] = _integer(value, f"limits.{name}", minimum=1)
        else:
            values[name] = _number(value, f"limits.{name}", minimum=1)
    limits = Limits(**values)
    if not 500 <= limits.reply_chars <= 12000:
        raise ConfigError("limits.reply_chars must be between 500 and 12000 (Slack's practical message size)")
    return limits


def _state(data: dict, base: Path) -> StateConfig:
    _keys(data, {"path", "control_socket"}, "[state]")
    path = _path(data.get("path", str(DEFAULT_STATE)), base, "state.path")
    if "control_socket" in data:
        socket = _path(data["control_socket"], base, "state.control_socket")
        if len(str(socket).encode()) > SOCKET_PATH_LIMIT:
            raise ConfigError("state.control_socket path is too long for a Unix socket; set a shorter one")
    else:
        socket = path.with_name("control.sock")
        if len(str(socket).encode()) > SOCKET_PATH_LIMIT:
            # Unix socket paths are limited to about 100 bytes; fall back to a short per-user directory.
            runtime = os.environ.get("XDG_RUNTIME_DIR")
            directory = Path(runtime) / "fridica" if runtime and Path(runtime).is_dir() else Path(f"/tmp/fridica-{os.getuid()}")
            socket = directory / f"control-{hashlib.sha256(str(path).encode()).hexdigest()[:12]}.sock"
    return StateConfig(path, socket)


def _machine(name: str, data: dict, default_policy: Policy, parent_backend: str, base: Path) -> Machine:
    if not NAME.fullmatch(name):
        raise ConfigError(f"machine name {name!r} must be lowercase letters, digits, - or _")
    label = f"[machines.{name}]"
    _keys(data, MACHINE_KEYS, label)
    transport = data.get("transport", "local" if name == "local" else "ssh")
    if transport not in TRANSPORTS:
        raise ConfigError(f"{label} transport must be one of {', '.join(TRANSPORTS)}")
    host = data.get("host", "")
    if transport == "local":
        if host:
            raise ConfigError(f"{label} is local and takes no host")
    elif not isinstance(host, str) or not SSH_HOST.fullmatch(host):
        raise ConfigError(f"{label} host must be an ~/.ssh/config alias or user@host")
    tags = _names(data.get("tags", []), f"{label} tags")
    backends = tuple(data.get("backends", [parent_backend]))
    if not backends or any(item not in BACKENDS for item in backends) or len(set(backends)) != len(backends):
        raise ConfigError(f"{label} backends must list claude and/or codex")
    default_backend = data.get("default_backend", backends[0])
    if default_backend not in backends:
        raise ConfigError(f"{label} default_backend must be one of its backends")
    max_workers = _integer(data.get("max_workers", MACHINE_DEFAULTS["max_workers"]), f"{label} max_workers", minimum=1)
    max_jobs = _integer(data.get("max_jobs", MACHINE_DEFAULTS["max_jobs"]), f"{label} max_jobs", minimum=1)
    if max_jobs > max_workers:
        raise ConfigError(f"{label} max_jobs cannot exceed max_workers")
    policy = default_policy.override(_table(data, "policy")) if "policy" in data else default_policy
    resources_data = _table(data, "resources")
    _keys(resources_data, {field.name for field in fields(Resources)}, f"{label} resources")
    resources = Resources(**resources_data)
    slurm = None
    if transport == "slurm":
        slurm_data = _table(data, "slurm")
        _keys(slurm_data, {field.name for field in fields(Slurm)}, f"{label} slurm")
        slurm = Slurm(**{key: tuple(value) if key == "extra" else value for key, value in slurm_data.items()})
    elif "slurm" in data:
        raise ConfigError(f"{label} slurm options need transport = \"slurm\"")
    if policy.gpu_confine and not resources.gpus:
        raise ConfigError(f"{label} policy.gpu_confine needs resources.gpus")
    workspaces_data = _table(data, "workspaces")
    if not workspaces_data:
        raise ConfigError(f"{label} needs at least one entry under [machines.{name}.workspaces]")
    workspaces = tuple(_workspace(name, item, value, policy, transport, base) for item, value in workspaces_data.items())
    for workspace in workspaces:
        if workspace.policy.gpu_confine and not resources.gpus:
            raise ConfigError(f"{label} workspace {workspace.name}: policy.gpu_confine needs resources.gpus")
        if workspace.policy.gpu_confine and workspace.policy.mode == "read-only":
            raise ConfigError(f"{label} workspace {workspace.name}: gpu_confine cannot enforce read-only; "
                              "drop one of them")
    workspaces = tuple(replace(workspace, policy=replace(workspace.policy, gpu_confine=_confined(workspace.policy, resources)))
                       for workspace in workspaces)
    policy = replace(policy, gpu_confine=bool(policy.gpu_confine))
    description = data.get("description", "")
    if not isinstance(description, str) or len(description) > 1000:
        raise ConfigError(f"{label} description must be at most 1000 characters")
    return Machine(name=name, transport=transport, workspaces=workspaces, backends=backends,
                   default_backend=default_backend, policy=policy, host=host, tags=tags, resources=resources,
                   max_workers=max_workers, max_jobs=max_jobs, slurm=slurm, description=description)


def _workspace(machine: str, name: str, value, policy: Policy, transport: str, base: Path) -> Workspace:
    label = f"machines.{machine}.workspaces.{name}"
    if not NAME.fullmatch(name):
        raise ConfigError(f"{label}: workspace names must be lowercase letters, digits, - or _")
    subfolders = None
    if isinstance(value, dict):
        _keys(value, {"path", "policy", "subfolders"}, label)
        text = value.get("path")
        if "policy" in value:
            policy = policy.override(_table(value, "policy"))
        if "subfolders" in value:
            subfolders = _bool(value["subfolders"], f"{label}.subfolders")
            if subfolders and policy.mode == "read-only":
                raise ConfigError(f"{label}: subfolders need a writable workspace")
    else:
        text = value
    if subfolders is None:
        subfolders = policy.mode != "read-only"  # on by default wherever jobs can write
    if not isinstance(text, str) or not text:
        raise ConfigError(f"{label} must be a path")
    if transport == "local":
        path = Path(text).expanduser()
        if not path.is_absolute():
            path = (base / path)
        path = path.resolve()
        if not path.is_dir():
            raise ConfigError(f"{label}: {path} is not an existing directory")
        if path in (Path.home().resolve(), Path("/")):
            raise ConfigError(f"{label}: your home directory or / is too broad for a workspace")
        return Workspace(name, path, policy, subfolders)
    if not (text.startswith("/") or text.startswith("~/")):
        raise ConfigError(f"{label}: remote paths must be absolute or start with ~/")
    path = PurePosixPath(text)
    if ".." in path.parts:
        raise ConfigError(f"{label}: remote paths may not contain ..")
    if str(path) in ("/", "//", "~"):
        raise ConfigError(f"{label}: the remote home directory or / is too broad for a workspace")
    return Workspace(name, path, policy, subfolders)


def _confined(policy: Policy, resources: Resources) -> bool:
    """Resolve automatic GPU confinement: write-mode workspaces on machines that declare GPUs.

    Read-only workspaces keep the backend sandbox (confinement cannot enforce read-only), and full-mode ones have
    no sandbox hiding the GPUs in the first place.
    """
    if policy.gpu_confine is not None:
        return policy.gpu_confine
    return bool(resources.gpus) and policy.mode == "write"


def _cross_checks(config: Config) -> None:
    for machine in config.machines.machines:
        if machine.transport != "local":
            continue
        for workspace in machine.workspaces:
            if _within(config.state.path, workspace.path):
                raise ConfigError(f"state.path must be outside workspace {machine.name}:{workspace.name}")


def _protect(config: Config) -> None:
    """Workers must not be able to rewrite Fridica itself, its configuration, or its rules."""
    import fridica
    protected = [Path(fridica.__file__).resolve().parent, config.path]
    protected += [item for item in (config.owner.contract, config.parent.repos) if item is not None]
    for machine in config.machines.machines:
        if machine.transport != "local":
            continue
        for workspace in machine.workspaces:
            if not workspace.writable:
                continue
            for item in protected:
                if _within(item, workspace.path):
                    raise ConfigError(f"{item} is inside writable workspace {machine.name}:{workspace.name};"
                                      " move it out or make the workspace read-only")


def fingerprint(path: Path) -> str:
    try:
        return hashlib.sha256(path.expanduser().read_bytes()).hexdigest()
    except OSError:
        return ""


def _within(path: Path, root: PurePath) -> bool:
    fold = lambda item: Path(unicodedata.normalize("NFD", str(item)).casefold())  # noqa: E731
    return fold(Path(path).resolve()).is_relative_to(fold(root))


def _table(data: dict, key: str) -> dict:
    value = data.get(key, {})
    if not isinstance(value, dict):
        raise ConfigError(f"{key} must be a table")
    return value


def _keys(data: dict, allowed: set[str], label: str) -> None:
    unknown = set(data) - allowed
    if unknown:
        raise ConfigError(f"unknown keys in {label}: {', '.join(sorted(unknown))}")


def _subset(data: dict, allowed: tuple[str, ...], label: str) -> dict:
    _keys(data, set(allowed), label)
    return dict(data)


def _ids(value, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) and CHANNEL_ID.fullmatch(item) for item in value):
        raise ConfigError(f"{label} must list channel IDs such as C012ABCDEF")
    if len(set(value)) != len(value):
        raise ConfigError(f"{label} lists a channel twice")
    return tuple(value)


def _names(value, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) and NAME.fullmatch(item) for item in value):
        raise ConfigError(f"{label} must list lowercase names")
    return tuple(value)


def _bool(value, label: str) -> bool:
    if not isinstance(value, bool):
        raise ConfigError(f"{label} must be true or false")
    return value


def _integer(value, label: str, *, minimum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ConfigError(f"{label} must be an integer of at least {minimum}")
    return value


def _number(value, label: str, *, minimum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < minimum:
        raise ConfigError(f"{label} must be a number of at least {minimum:g}")
    return float(value)


def _path(value, base: Path, label: str) -> Path:
    if not isinstance(value, str) or not value:
        raise ConfigError(f"{label} must be a path")
    path = Path(value).expanduser()
    return (path if path.is_absolute() else base / path).resolve()


def _file(value, base: Path, label: str) -> Path:
    path = _path(value, base, label)
    if not path.is_file():
        raise ConfigError(f"{label}: {path} is not a file")
    return path
