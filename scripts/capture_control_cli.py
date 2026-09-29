"""Capture frozen CLI requests and exit codes without a daemon or credentials."""
import contextlib
import io
import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from fridica.cli import main as cli
from fridica.control.client import ControlError, DaemonUnavailable

ROOT = Path(__file__).resolve().parents[1]
SESSION = "TTEAM:CROOM:100.1"
config = SimpleNamespace(owner=SimpleNamespace(slack_user="UOWNER"), state=SimpleNamespace(control_socket="/unused"))
commands = [["status"], ["machines"], ["threads"], ["threads", SESSION]]
commands += [["threads", SESSION, action] for action in ("pause", "resume", "close", "archive", "restore", "clean")]
commands += [["workers"], ["workers", "worker-1"], ["workers", "worker-1", "interrupt"], ["workers", "worker-1", "stop"]]
commands += [["approvals"], ["approvals", "approval-1"]]
commands += [["approvals", "approval-1", decision] for decision in ("once", "session", "deny")]
commands += [["outbox"], ["outbox", "1"]]

class Client:
    def __init__(self, _socket):
        pass

    def call(self, method, target, body=None):
        return dict(method=method, target=target, body=body or {})

rows = []
with patch.object(cli, "load_config", return_value=config), patch.object(cli, "ControlClient", Client):
    for args in commands:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = cli.main(args)
        rows.append(dict(args=args, exit=code, request=json.loads(out.getvalue())))
    errors = []
    for error in (ValueError("invalid input"), DaemonUnavailable("unavailable"), ControlError(409, "refused")):
        with patch.object(Client, "call", side_effect=error), contextlib.redirect_stderr(io.StringIO()):
            code = cli.main(["status"])
        errors.append(dict(kind=type(error).__name__, exit=code))

(ROOT / "tests/corpus/control_cli.json").write_text(json.dumps(dict(
    provenance="Frozen v0.3.11 CLI with synthetic config/client; no live services",
    commands=list(cli.build_parser()._subparsers._group_actions[0].choices),
    exceptions=["Rust omits body.actor and derives authority from the authenticated connection", "Invalid/unsafe identifiers and uncertain responses are rejected without retries"],
    cases=rows, errors=errors), indent=2) + "\n")
print(f"Captured {len(rows)} CLI request projections and {len(errors)} exit codes")
