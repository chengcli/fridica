"""``fridica``: init, configure, doctor, start, dashboard, and the control commands."""

from __future__ import annotations

import argparse
import asyncio
from importlib.resources import files
import json
import logging
import os
from pathlib import Path
import re
import signal
import sys

from .. import __version__
from ..config import DEFAULT_CONFIG, editor, load_config
from ..control.client import ControlClient, ControlError, DaemonUnavailable
from ..core.errors import ConfigError


def _config_argument(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG, help=f"configuration file (default {DEFAULT_CONFIG})")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="fridica", description="Your Slack presence, backed by Claude Code and Codex workers")
    parser.add_argument("--version", action="version", version=__version__)
    commands = parser.add_subparsers(dest="command", required=True)

    _config_argument(commands.add_parser("init", help="write a starter configuration and contract"))
    configure = commands.add_parser("configure", help="set your Slack identity and channels")
    _config_argument(configure)
    configure.add_argument("--detect", action="store_true", help="detect identity and joined channels with your user token")
    configure.add_argument("--channel-name", action="append", help="with --detect, choose channels by name")
    configure.add_argument("--owner-id")
    configure.add_argument("--workspace-id")
    configure.add_argument("--channel-id", action="append", dest="channels")
    _config_argument(commands.add_parser("doctor", help="check the configuration and every machine"))
    start = commands.add_parser("start", help="connect to Slack and run")
    _config_argument(start)
    start.add_argument("--observe-only", action="store_true", help="record messages without calling agents or posting")
    dashboard = commands.add_parser("dashboard", help="serve the local dashboard")
    _config_argument(dashboard)
    dashboard.add_argument("--port", type=int, default=8765)

    status = commands.add_parser("status", help="daemon status")
    _config_argument(status)
    threads = commands.add_parser("threads", help="list threads, show one, or act on one")
    _config_argument(threads)
    threads.add_argument("id", nargs="?")
    threads.add_argument("action", nargs="?", choices=("resume", "pause", "close", "archive", "restore", "clean"))
    workers = commands.add_parser("workers", help="list workers or interrupt/stop one")
    _config_argument(workers)
    workers.add_argument("id", nargs="?")
    workers.add_argument("action", nargs="?", choices=("interrupt", "stop"))
    approvals = commands.add_parser("approvals", help="list pending approvals or decide one")
    _config_argument(approvals)
    approvals.add_argument("id", nargs="?")
    approvals.add_argument("decision", nargs="?", choices=("once", "session", "deny"))
    machines = commands.add_parser("machines", help="list machines and how busy they are")
    _config_argument(machines)
    outbox = commands.add_parser("outbox", help="list failed or ambiguous posts, or retry one")
    _config_argument(outbox)
    outbox.add_argument("id", nargs="?", type=int)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    logging.getLogger("slack_sdk").setLevel(logging.WARNING)
    try:
        handler = COMMANDS.get(args.command) or control_command
        return handler(args)
    except (ConfigError, ValueError) as error:
        print(f"fridica: {error}", file=sys.stderr)
        return 2
    except DaemonUnavailable as error:
        print(f"fridica: {error}", file=sys.stderr)
        return 3
    except ControlError as error:
        print(f"fridica: {error} (HTTP {error.status})", file=sys.stderr)
        return 4


def init(args) -> int:
    path = args.config.expanduser()
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as stream:
        stream.write(files("fridica.config").joinpath("template.toml").read_text())
    for name, package in (("contract.md", "fridica.parent"), ("manifest.yaml", "fridica")):
        target = path.parent / name
        resource = files(package).joinpath(name)
        if not target.exists() and resource.is_file():
            target.write_text(resource.read_text())
    print(f"Created {path}.")
    print("Next: fridica configure --detect, then describe your machines and workspaces, then fridica doctor.")
    print(f"Agent rules are in {path.parent / 'contract.md'}.")
    return 0


def configure(args) -> int:
    if args.detect:
        if args.owner_id or args.workspace_id or args.channels:
            raise ValueError("--detect cannot be combined with manual IDs")
        from ..config.discovery import discover_from_config, select_channels
        args.owner_id, args.workspace_id, channels, warnings = asyncio.run(discover_from_config(args.config))
        print(f"Detected owner {args.owner_id} in workspace {args.workspace_id}.")
        for warning in warnings:
            print(warning, file=sys.stderr)
        args.channels = select_channels(channels, args.channel_name)
    elif args.channel_name:
        raise ValueError("--channel-name requires --detect")
    changes: dict[str, dict] = {}
    if args.owner_id is not None:
        if not re.fullmatch(r"[UW][A-Z0-9]+", args.owner_id):
            raise ValueError("--owner-id must be a Slack member ID")
        changes.setdefault("owner", {})["slack_user"] = args.owner_id
    if args.workspace_id is not None:
        if not re.fullmatch(r"T[A-Z0-9]+", args.workspace_id):
            raise ValueError("--workspace-id must be a Slack team ID")
        changes.setdefault("slack", {})["workspace"] = args.workspace_id
    if args.channels:
        if not all(re.fullmatch(r"[CG][A-Z0-9]+", channel) for channel in args.channels):
            raise ValueError("--channel-id values must be channel IDs")
        changes.setdefault("slack", {})["channels"] = list(dict.fromkeys(args.channels))
    if not changes:
        raise ValueError("nothing to change; pass --detect or IDs")
    editor.update(args.config, changes, validate=False)
    print(f"Updated {args.config.expanduser()}. Run fridica doctor to check the rest of the configuration.")
    return 0


def doctor(args) -> int:
    from ..doctor.checks import report, run_checks
    return report(asyncio.run(run_checks(args.config)))


def start(args) -> int:
    if sys.platform not in ("linux", "darwin"):
        raise ValueError("Fridica runs on macOS or Linux")
    config = load_config(args.config)
    config.tokens()
    from ..app import serve

    async def run() -> None:
        task = asyncio.create_task(serve(config, observe_only=args.observe_only))
        loop = asyncio.get_running_loop()
        for signum in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(signum, task.cancel)
        try:
            await task
        except asyncio.CancelledError:
            logging.info("stopped")

    asyncio.run(run())
    return 0


def dashboard(args) -> int:
    from ..dashboard.server import run as run_dashboard
    config = load_config(args.config)
    run_dashboard(config, port=args.port)
    return 0


def control_command(args) -> int:
    config = load_config(args.config)
    client = ControlClient(config.state.control_socket)
    command = args.command
    if command == "status":
        result = client.call("GET", "/status")
    elif command == "threads":
        if args.action:
            result = client.call("POST", f"/threads/{args.id}/{args.action}", {"actor": config.owner.slack_user})
        else:
            result = client.call("GET", f"/threads/{args.id}" if args.id else "/threads")
    elif command == "workers":
        result = (client.call("POST", f"/workers/{args.id}/{args.action}") if args.action else client.call("GET", "/workers"))
    elif command == "approvals":
        if args.decision:
            result = client.call("POST", f"/approvals/{args.id}", {"decision": args.decision, "actor": config.owner.slack_user})
        else:
            result = client.call("GET", "/approvals")
    elif command == "machines":
        result = client.call("GET", "/machines")
    elif command == "outbox":
        result = client.call("POST", f"/outbox/{args.id}/retry") if args.id else client.call("GET", "/outbox")
    else:
        raise ValueError(f"unknown command {command}")
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


COMMANDS = {"init": init, "configure": configure, "doctor": doctor, "start": start, "dashboard": dashboard}
