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
from .config import DEFAULT_CONFIG, TEMPLATE, load_config


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
    for name, help_text in (("init", "Write an example configuration"), ("doctor", "Check local configuration and prerequisites"), ("start", "Listen to Slack")):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
        if name == "start":
            command.add_argument("--observe-only", action="store_true", help="Record events without invoking agents or posting")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    logging.getLogger("slack_sdk").setLevel(logging.CRITICAL)
    try:
        if args.command == "init":
            path = args.config.expanduser()
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as stream:
                stream.write(TEMPLATE)
            manifest = path.parent / "manifest.yaml"
            if not manifest.exists():
                resource = files("fridica").joinpath("manifest.yaml")
                if resource.is_file():
                    with manifest.open("x") as stream:
                        stream.write(resource.read_text())
            print(f"Created {path}. Set your identity, channels, workspace, and token environment variables.")
            return 0
        if args.command == "doctor":
            from .doctor import run_doctor
            return run_doctor(args.config)
        config = load_config(args.config)
        if sys.platform not in {"darwin", "linux"}:
            raise ValueError("fridica supports macOS and Linux")
        config.tokens()
        from .agents import check_backend
        if not args.observe_only:
            problems = check_backend(config)
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
