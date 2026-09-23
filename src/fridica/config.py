from __future__ import annotations

import math
import os
from pathlib import Path, PurePath, PurePosixPath
import re
import tomllib
import tempfile
import unicodedata
from dataclasses import dataclass

import tomlkit


DEFAULT_CONFIG = Path.home() / ".config" / "fridica" / "config.toml"
DEFAULT_STATE = Path.home() / ".local" / "state" / "fridica" / "state.sqlite3"


def within_casefold(path: Path, root: Path) -> bool:
    """Compare deny boundaries conservatively on case-insensitive filesystems."""
    return Path(unicodedata.normalize('NFD', str(path)).casefold()).is_relative_to(
        Path(unicodedata.normalize('NFD', str(root)).casefold()))


SSH_HOST = re.compile(r"(?:[A-Za-z0-9_][A-Za-z0-9_.-]*@)?[A-Za-z0-9][A-Za-z0-9_.-]*")


def parse_root(text: str) -> tuple[str | None, PurePath]:
    """Split a workspace root into its SSH host (None when local) and its path.

    ``dart9:/mnt/project`` names ``/mnt/project`` on the SSH host ``dart9`` (an alias
    from ``~/.ssh/config`` or ``user@host``). A leading ``/``, ``~`` or ``.`` always
    means a local path, so colons inside local paths are never mistaken for a host.
    """
    if not isinstance(text, str) or not text:
        raise ValueError("workspace roots must be nonempty paths")
    if text[0] in "/~." or ":" not in text:
        return None, Path(text).expanduser()
    host, _separator, remainder = text.partition(":")
    if not SSH_HOST.fullmatch(host):
        raise ValueError(f"invalid SSH host in workspace root {text!r}")
    if not remainder.startswith("/"):
        raise ValueError(f"remote workspace root {text!r} must use an absolute path after the host")
    return host, PurePosixPath(remainder)


@dataclass(frozen=True)
class Resources:
    """Hardware on the working host that heavy tasks may use; declared, not measured.

    The values travel to the agent as data so it can judge what a request needs, and
    the persistent worker inherits matching ``OMP_NUM_THREADS`` and
    ``CUDA_VISIBLE_DEVICES`` limits. None means unspecified.
    """
    cpus: int | None = None
    gpus: tuple[int, ...] | None = None
    gpu_type: str = ""
    memory_gb: float | None = None
    notes: str = ""
    gpu_access: bool = True
    """Run heavy tasks outside the filesystem sandbox when GPUs are declared, so the devices are reachable."""

    def __post_init__(self) -> None:
        if not isinstance(self.gpu_access, bool):
            raise ValueError("resources.gpu_access must be boolean")
        if self.cpus is not None and (isinstance(self.cpus, bool) or not isinstance(self.cpus, int) or self.cpus < 1):
            raise ValueError("resources.cpus must be a positive integer")
        if self.gpus is not None:
            if not isinstance(self.gpus, (list, tuple)) or any(
                    isinstance(index, bool) or not isinstance(index, int) or index < 0 for index in self.gpus):
                raise ValueError("resources.gpus must list GPU device indices such as [0, 1]")
            if len(set(self.gpus)) != len(self.gpus):
                raise ValueError("resources.gpus lists a device twice")
            object.__setattr__(self, "gpus", tuple(self.gpus))
        if self.memory_gb is not None and (isinstance(self.memory_gb, bool) or not isinstance(self.memory_gb, (int, float))
                                           or not math.isfinite(self.memory_gb) or self.memory_gb <= 0):
            raise ValueError("resources.memory_gb must be a positive number")
        for name in ("gpu_type", "notes"):
            value = getattr(self, name)
            if not isinstance(value, str) or len(value) > 1000:
                raise ValueError(f"resources.{name} must be a string of at most 1000 characters")

    @property
    def gpu_worker(self) -> bool:
        """True when heavy tasks need Fridica's own confinement to reach the GPUs: GPUs declared and gpu_access on.

        Both backends sandbox commands with bubblewrap, whose minimal ``/dev`` hides the
        GPU device nodes. A GPU worker therefore runs with the backend's sandbox off but
        inside Fridica's bubblewrap, which exposes ``/dev`` and confines writes to the
        host's designated roots (see ``remote.confinement``).
        """
        return bool(self.gpus) and self.gpu_access

    def payload(self) -> dict:
        """The JSON-friendly description sent to the agent; unspecified values are left out."""
        data = {"cpus": self.cpus, "gpus": list(self.gpus) if self.gpus is not None else None,
                "gpu_type": self.gpu_type, "memory_gb": self.memory_gb, "notes": self.notes,
                "gpu_access": self.gpu_worker if self.gpus else None}
        return {key: value for key, value in data.items() if value not in (None, "")}

    def environment(self) -> dict[str, str]:
        """Environment variables that hold the persistent worker to the declared resources."""
        variables = {}
        if self.cpus is not None:
            variables["OMP_NUM_THREADS"] = str(self.cpus)
        if self.gpus is not None:
            variables["CUDA_VISIBLE_DEVICES"] = ",".join(str(index) for index in self.gpus)
        return variables


LOCAL = "local"


@dataclass(frozen=True)
class Host:
    """One machine the agent may work on: its designated writable roots and declared hardware.

    The primary host is where ``workspace`` lives (``local`` or an SSH alias) and where
    every per-turn reply runs. Further hosts come from roots in ``additional_workspaces``
    with another ``host:`` prefix; they only run heavy tasks, each confined to its own
    roots.
    """
    name: str
    roots: tuple[PurePath, ...]
    resources: Resources = Resources()

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not (self.name == LOCAL or SSH_HOST.fullmatch(self.name)):
            raise ValueError("host names must be local or an SSH host alias")
        if not self.roots:
            raise ValueError(f"host {self.name} has no workspace roots")
        if not isinstance(self.resources, Resources):
            raise ValueError(f"resources for host {self.name} must be a table")

    @property
    def remote(self) -> bool:
        return self.name != LOCAL

    @property
    def ssh_host(self) -> str | None:
        return self.name if self.remote else None

    @property
    def workspace(self) -> PurePath:
        """The working directory for this host's agent processes: its first root."""
        return self.roots[0]

    def label(self, path: PurePath) -> str:
        return f"{self.name}:{path}" if self.remote else str(path)

    def payload(self) -> dict:
        """The JSON-friendly description handed to the reply agent so it can pick a host for heavy work."""
        return {"name": self.name, "roots": [str(root) for root in self.roots], "resources": self.resources.payload()}


@dataclass(frozen=True)
class Config:
    owner_id: str
    workspace_id: str
    channels: tuple[str, ...]
    workspace: Path
    additional_workspaces: tuple[Path, ...] = ()
    backend: str = "claude"
    model: str | None = None
    reasoning_effort: str | None = None
    max_wait_replies: int = 3
    profile: str = ""
    state_path: Path = DEFAULT_STATE
    app_token_env: str = "SLACK_APP_TOKEN"
    user_token_env: str = "SLACK_USER_TOKEN"
    timeout: float = 600
    context_limit: int = 50
    cooldown: float = 60
    max_turns: int = 6
    general_messages: bool = True
    resume_sessions: bool = True
    session_timeout: float = 14 * 86400
    contract: Path | None = None
    repos: Path | None = None
    file_access: bool = False
    read_only_workspaces: tuple[Path, ...] = ()
    allowed_domains: tuple[str, ...] = ()
    ssh_host: str | None = None
    heavy_tasks: bool = False
    heavy_task_timeout: float = 4 * 3600
    heavy_task_idle: float = 1800
    resources: Resources = Resources()
    remote_hosts: tuple[Host, ...] = ()

    @property
    def remote(self) -> bool:
        """True when the workspace roots live on an SSH host and every per-turn run happens there."""
        return self.ssh_host is not None

    def root_label(self, path: PurePath) -> str:
        """The configured spelling of a primary-host root: ``host:/path`` when remote, the local path otherwise."""
        return f"{self.ssh_host}:{path}" if self.remote else str(path)

    @property
    def primary(self) -> Host:
        """The host that owns ``workspace`` and runs the per-turn replies."""
        return Host(self.ssh_host or LOCAL, (self.workspace, *self.additional_workspaces), self.resources)

    @property
    def hosts(self) -> tuple[Host, ...]:
        """Every host heavy tasks may run on; the primary host first."""
        return (self.primary, *self.remote_hosts)

    def host(self, name: str) -> Host:
        for host in self.hosts:
            if host.name == name:
                return host
        raise KeyError(name)

    def root_labels(self) -> list[str]:
        """The configured spelling of every writable root on every host."""
        return [host.label(root) for host in self.hosts for root in host.roots]

    def __post_init__(self) -> None:
        for label, value, pattern in (
            ("owner_id", self.owner_id, r"[UW][A-Z0-9]+"),
            ("workspace_id", self.workspace_id, r"T[A-Z0-9]+"),
        ):
            if not isinstance(value, str) or not re.fullmatch(pattern, value):
                raise ValueError(f"{label} must be a Slack ID")
        if not isinstance(self.channels, (list, tuple)) or not self.channels:
            raise ValueError("channels must explicitly list allowed Slack channels")
        if any(not isinstance(channel, str) or not re.fullmatch(r"[CG][A-Z0-9]+", channel) for channel in self.channels):
            raise ValueError("channels must contain Slack channel IDs")
        if self.backend not in ("claude", "codex"):
            raise ValueError("backend must be claude or codex")
        if not self.state_path.is_absolute():
            raise ValueError("state_path must be an absolute path")
        for name in ("timeout", "cooldown", "context_limit", "max_turns", "max_wait_replies", "session_timeout",
                     "heavy_task_timeout", "heavy_task_idle"):
            value = getattr(self, name)
            minimum = 0 if name in {"cooldown", "session_timeout", "heavy_task_idle"} else 1
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < minimum:
                raise ValueError(f"{name} must be a finite number >= {minimum}")
            if name in {"context_limit", "max_turns", "max_wait_replies"} and not isinstance(value, int):
                raise ValueError(f"{name} must be an integer")
        for name in ("general_messages", "resume_sessions", "file_access", "heavy_tasks"):
            if not isinstance(getattr(self, name), bool):
                raise ValueError(f"{name} must be boolean")
        if not isinstance(self.resources, Resources):
            raise ValueError("resources must be a [resources] table")
        if self.heavy_tasks and self.file_access:
            raise ValueError("heavy_tasks requires the agent's native workspace tools; disable file_access")
        if not isinstance(self.remote_hosts, tuple) or any(not isinstance(host, Host) or not host.remote for host in self.remote_hosts):
            raise ValueError("remote_hosts must be remote Host entries")
        names = [host.name for host in self.remote_hosts]
        if len(set(names)) != len(names) or (self.ssh_host or LOCAL) in names:
            raise ValueError("each host may appear once among the workspace roots")
        if self.remote_hosts and self.file_access:
            raise ValueError("file_access requires every workspace root on the local machine")
        for host in self.remote_hosts:
            for directory in host.roots:
                if not isinstance(directory, PurePosixPath) or not directory.is_absolute() or directory == PurePosixPath("/"):
                    raise ValueError(f"roots on {host.name} must be absolute POSIX paths below the filesystem root")
        if not isinstance(self.profile, str) or (self.model is not None and not isinstance(self.model, str)):
            raise ValueError("profile and model must be strings")
        if self.reasoning_effort not in {None, "low", "medium", "high", "xhigh", "max", "ultra"}:
            raise ValueError("invalid reasoning_effort")
        if self.contract is not None and (not isinstance(self.contract, Path) or not self.contract.is_absolute()
                                          or not self.contract.is_file()):
            raise ValueError("contract must be an existing Markdown file")
        if self.repos is not None and (not isinstance(self.repos, Path) or not self.repos.is_absolute()
                                       or not self.repos.is_file()):
            raise ValueError("repos must be an existing TOML file")
        for name in ("app_token_env", "user_token_env"):
            value = getattr(self, name)
            if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
                raise ValueError(f"{name} must name an environment variable")
        if self.read_only_workspaces and not self.file_access:
            raise ValueError("read_only_workspaces requires file_access")
        if not isinstance(self.allowed_domains, (list, tuple)) or any(
            not isinstance(domain, str)
            or (domain != "*" and not re.fullmatch(r"(\*\.)?([a-z0-9-]{1,63}\.)+[a-z]{2,63}", domain.lower()))
            for domain in self.allowed_domains
        ):
            raise ValueError('allowed_domains must list host names such as github.com or *.example.org, or "*" for every host')
        roots = (self.workspace, *self.additional_workspaces, *self.read_only_workspaces)
        if self.remote:
            if not isinstance(self.ssh_host, str) or not SSH_HOST.fullmatch(self.ssh_host):
                raise ValueError("ssh_host must be an SSH host alias such as dart9 or user@dart9")
            if self.file_access:
                raise ValueError("file_access requires local workspace roots; remove the SSH host or disable file_access")
            for directory in roots:
                if not isinstance(directory, PurePosixPath) or not directory.is_absolute() or directory == PurePosixPath("/"):
                    raise ValueError("remote workspace roots must be absolute POSIX paths below the filesystem root")
            return
        for directory in roots:
            if not directory.is_absolute() or not directory.is_dir():
                raise ValueError("workspace roots must be existing absolute directories")
            if directory.samefile(Path.home()) or directory.samefile(Path("/")):
                raise ValueError("choose a project directory, not the home or filesystem root")
        if self.file_access:
            package = Path(__file__).resolve().parent
            for directory in (self.workspace, *self.additional_workspaces):
                if within_casefold(package, directory.resolve()) or within_casefold(directory.resolve(), package):
                    raise ValueError("Fridica's installed code must be outside writable roots")
        if within_casefold(self.state_path.resolve(), self.workspace.resolve()) or any(
            within_casefold(self.state_path.resolve(), directory.resolve()) for directory in (*self.additional_workspaces, *self.read_only_workspaces)
        ):
            raise ValueError("state_path must be outside agent workspaces")

    def tokens(self) -> tuple[str, str]:
        app = os.environ.get(self.app_token_env, "")
        user = os.environ.get(self.user_token_env, "")
        if not app.startswith("xapp-"):
            raise ValueError(f"set {self.app_token_env} to a Slack app-level token")
        if not user.startswith("xoxp-"):
            raise ValueError(f"set {self.user_token_env} to a Slack user token")
        return app, user


def set_slack_ids(path: Path, *, owner_id: str | None = None,
                  workspace_id: str | None = None, channels: list[str] | None = None) -> None:
    updates = {}
    for name, value, pattern in (
        ("owner_id", owner_id, r"[UW][A-Z0-9]+"),
        ("workspace_id", workspace_id, r"T[A-Z0-9]+"),
    ):
        if value is not None:
            if not re.fullmatch(pattern, value):
                raise ValueError(f"{name} must be a Slack ID")
            updates[name] = value
    if channels is not None:
        if not channels or any(not re.fullmatch(r"[CG][A-Z0-9]+", channel) for channel in channels):
            raise ValueError("channels must contain Slack channel IDs")
        updates["channels"] = list(dict.fromkeys(channels))
    if not updates:
        raise ValueError("provide --owner-id, --workspace-id, or --channel-id")
    path = path.expanduser()
    if path.is_symlink():
        raise ValueError("refusing to replace a symlink configuration; specify its target path")
    if not path.is_file():
        raise ValueError("configuration not found; run fridica init with the same --config path first")
    original = path.read_text()
    try:
        document = tomlkit.parse(original)
    except ValueError:
        raise ValueError("configuration is not valid TOML; repair it before updating IDs") from None
    for name, value in updates.items():
        document[name] = value
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix=".fridica-config-", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(tomlkit.dumps(document))
            stream.flush()
            os.fsync(stream.fileno())
        if path.is_symlink() or path.read_text() != original:
            raise ValueError("configuration changed during update; retry with the latest file")
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def load_config(path: Path, *, contents: bytes | None = None) -> Config:
    values = tomllib.loads((path.expanduser().read_bytes() if contents is None else contents).decode("utf-8"))
    allowed = set(Config.__dataclass_fields__)
    if set(values) - allowed:
        raise ValueError("unknown configuration fields: " + ", ".join(sorted(set(values) - allowed)))
    for required in ("owner_id", "workspace_id", "channels", "workspace"):
        if required not in values:
            raise ValueError(f"missing configuration field: {required}")
    if "state_path" in values:
        if not isinstance(values["state_path"], str) or not values["state_path"]:
            raise ValueError("state_path must be a nonempty path")
        values["state_path"] = Path(values["state_path"]).expanduser()
    if not isinstance(values["workspace"], str) or not values["workspace"]:
        raise ValueError("workspace must be a nonempty path")
    for key in ("additional_workspaces", "read_only_workspaces"):
        roots = values.get(key, [])
        if not isinstance(roots, list) or any(not isinstance(root, str) or not root for root in roots):
            raise ValueError(f"{key} must be a list of paths")
    if "ssh_host" in values and values["ssh_host"] is not None and (
            not isinstance(values["ssh_host"], str) or not SSH_HOST.fullmatch(values["ssh_host"])):
        raise ValueError("ssh_host must be an SSH host alias such as dart9 or user@dart9")
    explicit = values.get("ssh_host")
    workspace_host, values["workspace"] = parse_root(values["workspace"])
    if explicit is not None and workspace_host is not None and explicit != workspace_host:
        raise ValueError("ssh_host disagrees with the host: prefix on the workspace roots")
    primary = workspace_host or explicit
    if workspace_host is None and explicit is not None:
        # ssh_host given explicitly with a plain workspace path: the workspace is remote.
        values["workspace"] = PurePosixPath(str(values["workspace"]))
    values["ssh_host"] = primary
    additional = [parse_root(root) for root in values.get("additional_workspaces", [])]
    read_only = [parse_root(root) for root in values.get("read_only_workspaces", [])]
    if primary is not None and any(root_host is None for root_host, _path in (*additional, *read_only)):
        if workspace_host is None:
            # Explicit ssh_host with plain paths everywhere: every root is on that host.
            additional = [(primary, PurePosixPath(str(path))) if root_host is None else (root_host, path) for root_host, path in additional]
            read_only = [(primary, PurePosixPath(str(path))) if root_host is None else (root_host, path) for root_host, path in read_only]
        else:
            raise ValueError("the workspace is remote, so prefix every other root with its host: too")
    if any(root_host != primary for root_host, _path in read_only):
        raise ValueError("read_only_workspaces must be on the same host as workspace")
    values["read_only_workspaces"] = tuple(path for _root_host, path in read_only)
    values["additional_workspaces"] = tuple(path for root_host, path in additional if root_host == primary)
    others: dict[str, list[PurePath]] = {}
    for root_host, root in additional:
        if root_host != primary:
            others.setdefault(root_host, []).append(root)
    tables = values.pop("resources", None)
    per_host: dict[str, dict] = {}
    if tables is not None:
        if not isinstance(tables, dict):
            raise ValueError("resources must be a [resources] table or [resources.<host>] tables")
        if tables and any(isinstance(table, dict) for table in tables.values()):
            if not all(isinstance(table, dict) for table in tables.values()):
                raise ValueError("resources must be either one [resources] table or only [resources.<host>] tables, not both")
            per_host = dict(tables)
            known = {primary or LOCAL, *others}
            for name in per_host:
                if name not in known:
                    raise ValueError(f"[resources.{name}] names a host that has no workspace roots; known hosts: "
                                     + ", ".join(sorted(known)))
        else:
            per_host = {primary or LOCAL: tables}
    def resources_for(name: str) -> Resources:
        table = per_host.get(name, {})
        if not isinstance(table, dict) or set(table) - set(Resources.__dataclass_fields__):
            raise ValueError(f"[resources{'' if name == (primary or LOCAL) else '.' + name}] accepts cpus, gpus, gpu_type, memory_gb, notes, and gpu_access")
        return Resources(**table)
    values["resources"] = resources_for(primary or LOCAL)
    values["remote_hosts"] = tuple(Host(name, tuple(paths), resources_for(name)) for name, paths in others.items())
    domains = values.get("allowed_domains", [])
    if not isinstance(domains, list):
        raise ValueError("allowed_domains must be a list of host names")
    values["allowed_domains"] = tuple(dict.fromkeys(str(domain).lower() for domain in domains))
    if "contract" in values:
        if not isinstance(values["contract"], str) or not values["contract"]:
            raise ValueError("contract must be a nonempty path")
        contract = Path(values["contract"]).expanduser()
        values["contract"] = contract if contract.is_absolute() else (path.expanduser().parent / contract).resolve()
    elif (path.expanduser().parent / "contract.md").is_file():
        values["contract"] = path.expanduser().parent / "contract.md"
    if "repos" in values:
        if not isinstance(values["repos"], str) or not values["repos"]:
            raise ValueError("repos must be a nonempty path")
        repos = Path(values["repos"]).expanduser()
        values["repos"] = repos if repos.is_absolute() else (path.expanduser().parent / repos).resolve()
    config = Config(**values)
    if config.file_access:
        roots = (config.workspace, *config.additional_workspaces, *config.read_only_workspaces)
        for protected in (path.expanduser(), config.contract, config.repos):
            if protected and any(within_casefold(protected.resolve(), root.resolve()) for root in roots):
                raise ValueError("configuration, contract, and repository list must be outside file access roots")
    return config


TEMPLATE = '''# Create your own Slack app using slack/manifest.yaml.
owner_id = "U_REPLACE"
workspace_id = "T_REPLACE"
channels = ["C_REPLACE"]
workspace = "~/projects/your-project"
# A working folder on another machine over SSH: "dart9:/mnt/your-project", where dart9
# is an alias from ~/.ssh/config that connects without a prompt. Every per-turn run then
# happens on that host, which needs the backend CLI installed and signed in.
# Roots on other hosts, such as "dart9:/mnt/data1/projects", make those hosts available
# for heavy tasks only, each confined to its own roots. file_access is local-only.
additional_workspaces = []
# Use scoped file operations instead of the agent's native workspace tools.
file_access = false
read_only_workspaces = []
# Hosts that task commands may reach, e.g. ["github.com", "*.pypi.org"]; ["*"] allows
# every host. Empty keeps task-command network access disabled. Codex cannot filter
# by host: any entry enables full network access for Codex tasks.
allowed_domains = []
backend = "claude"
# model = "your-preferred-model"
# reasoning_effort = "low"  # Codex only
profile = "My projects and expertise: ..."
app_token_env = "SLACK_APP_TOKEN"
user_token_env = "SLACK_USER_TOKEN"
general_messages = true
context_limit = 50
timeout = 600
cooldown = 60
max_turns = 6
max_wait_replies = 3
resume_sessions = true
# Seconds of thread inactivity after which a stored session is not resumed (2 weeks).
session_timeout = 1209600
# Heavy tasks: when a request needs long-running or hardware-heavy work, the reply
# agent may hand a brief to a persistent worker (codex app-server or a streaming
# claude session) on the working host, which reports back to the Slack thread when it
# is done. Anyone in an allowed channel can trigger such work, so this is opt-in.
heavy_tasks = false
# Seconds one heavy job may run, and seconds of inactivity before the worker
# process exits (it is resumed by thread when the next job arrives).
heavy_task_timeout = 14400
heavy_task_idle = 1800
# Hardware heavy tasks may use, per host: [resources.local] for this machine and
# [resources.<alias>] for each SSH host among the roots (a plain [resources] table
# describes the workspace's host). The agent sees these values as data and picks the
# host for a job; the worker runs with matching OMP_NUM_THREADS and CUDA_VISIBLE_DEVICES.
# Declaring gpus runs the worker inside Fridica's own bubblewrap (the backend sandbox
# hides GPU devices): full /dev, writes only to that host's roots; needs bwrap there.
# Set gpu_access = false to keep the backend sandbox and forgo the GPUs.
# [resources.dart9]
# cpus = 8
# gpus = [0, 1]
# gpu_type = "NVIDIA A100 80GB"
# memory_gb = 128
# notes = "Long jobs go through Slurm: srun --gres=gpu:1."
# gpu_access = true

# Agent rules live in contract.md beside this file (created by fridica init).
# Uncomment to use a different file; relative paths resolve from this directory.
# contract = "contract.md"
# The repository list is shared and ships with Fridica; change it with a pull
# request to main. Uncomment only to test a local copy before opening that PR.
# repos = "repos.toml"
'''
