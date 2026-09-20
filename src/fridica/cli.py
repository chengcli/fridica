from __future__ import annotations

import argparse
import asyncio
import logging
from importlib.resources import files
import os
from pathlib import Path
import signal
import sys

from . import __version__
from .config import DEFAULT_CONFIG, TEMPLATE, load_config, set_slack_ids


async def _start(config, observe_only: bool) -> None:
    from .agents import create_backend
    from .slack import serve
    from .store import Store

    store = Store(config.state_path)
    worker = asyncio.create_task(serve(config, store, None if observe_only else create_backend(config), observe_only))
    loop = asyncio.get_running_loop()
    for signum in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(signum, worker.cancel)
    try:
        for row in store.attention():
            logging.warning("Event %s needs local inspection (%s)", row["event_id"], row["state"])
        await worker
    except asyncio.CancelledError:
        logging.info("Stopped")
    finally:
        for signum in (signal.SIGTERM, signal.SIGINT):
            loop.remove_signal_handler(signum)
        store.close()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="A local personal Slack agent")
    parser.add_argument("--version", action="version", version=__version__)
    commands = parser.add_subparsers(dest="command", required=True)
    configure = commands.add_parser("configure", help="Set Slack IDs in an existing configuration")
    configure.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    configure.add_argument("--detect", action="store_true", help="Detect identity and discover joined channels using your Slack token")
    configure.add_argument("--channel-name", action="append", help="With --detect, select channels by name instead of prompting")
    configure.add_argument("--owner-id", help="Slack member ID of the authorized owner")
    configure.add_argument("--workspace-id", help="Slack workspace ID (not a local directory)")
    configure.add_argument("--channel-id", action="append", dest="channels",
                           help="Channel ID; repeat to replace the full configured channel list")
    for name, help_text in (("init", "Write an example configuration"), ("doctor", "Check local configuration and prerequisites"), ("start", "Listen to Slack")):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
        if name == "start":
            command.add_argument("--observe-only", action="store_true", help="Record events without invoking agents or posting")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    logging.getLogger("slack_sdk").setLevel(logging.CRITICAL)
    try:
        if args.command == "configure":
            if args.detect:
                if args.owner_id or args.workspace_id or args.channels:
                    raise ValueError("--detect cannot be combined with manual ID options")
                from .discovery import discover_from_config, select_channels
                args.owner_id, args.workspace_id, channels, warnings = asyncio.run(discover_from_config(args.config))
                print(f"Detected owner {args.owner_id} in workspace {args.workspace_id}.")
                for warning in warnings:
                    print(warning, file=sys.stderr)
                args.channels = select_channels(channels, args.channel_name)
            elif args.channel_name:
                raise ValueError("--channel-name requires --detect")
            set_slack_ids(args.config, owner_id=args.owner_id, workspace_id=args.workspace_id,
                          channels=args.channels)
            print(f"Updated {args.config.expanduser()}. Restart Fridica to apply the changes.")
            if args.owner_id is not None or args.workspace_id is not None:
                print("The IDs must match your Slack user token. A different identity requires a separate state_path.")
            print("Run fridica doctor with the same --config path to check the complete configuration.")
            return 0
        if args.command == "init":
            path = args.config.expanduser()
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as stream:
                stream.write(TEMPLATE)
            for name in ("manifest.yaml", "contract.md"):
                target = path.parent / name
                if not target.exists():
                    resource = files("fridica").joinpath(name)
                    if resource.is_file():
                        with target.open("x") as stream:
                            stream.write(resource.read_text())
            print(f"Created {path}. Set your identity, channels, workspace, and token environment variables.")
            print(f"Agent rules are in {path.parent / 'contract.md'}; edit them to change how your persona behaves.")
            return 0
        if args.command == "doctor":
            from .doctor import run_doctor
            return run_doctor(args.config)
        config = load_config(args.config)
        if sys.platform not in {"darwin", "linux"}:
            raise ValueError("fridica supports macOS and Linux")
        config.tokens()
        from .agents import check_backend, check_sandbox
        if not args.observe_only:
            problems = check_backend(config) + check_sandbox(config)
            if problems:
                for problem in problems:
                    print(problem, file=sys.stderr)
                return 1
        asyncio.run(_start(config, args.observe_only))
        return 0
    except (ValueError, OSError) as error:
        if isinstance(error, ValueError):
            print(f"Configuration error: {error}", file=sys.stderr)
        else:
            print(f"Local file or process operation failed ({type(error).__name__}).", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(f"Fridica stopped ({type(error).__name__}); check Slack scopes, connectivity, and local agent setup.", file=sys.stderr)
        return 1
