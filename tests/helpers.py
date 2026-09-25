from pathlib import Path
import textwrap

OWNER = "UOWNER"
TEAM = "TTEAM"
ROOM = "CROOM"


def base_config(workspace: Path, state: Path, machines: str = "") -> str:
    return textwrap.dedent(f"""
        [owner]
        slack_user = "{OWNER}"
        profile = "Planetary atmospheres."

        [slack]
        workspace = "{TEAM}"
        channels = ["{ROOM}", "COTHER"]

        [machines.local]
        transport = "local"
        backends = ["claude", "codex"]

        [machines.local.workspaces]
        project = "{workspace}"

        [state]
        path = "{state}"
    """) + textwrap.dedent(machines)


FAKE_SSH = '''#!{python}
"""A stand-in for ssh: checks the client's options and runs the remote command line locally with bash -c."""
import json, os, pathlib, subprocess, sys
arguments = sys.argv[1:]
assert arguments[0] == "-T", arguments
assert "BatchMode=yes" in arguments and "--" in arguments, arguments
host, script = arguments[arguments.index("--") + 1], arguments[arguments.index("--") + 2]
assert len(arguments) == arguments.index("--") + 3, arguments
log = os.environ.get("SSH_LOG")
if log:
    pathlib.Path(log).open("a").write(json.dumps({{"host": host, "script": script}}) + "\\n")
if os.environ.get("SSH_FAIL"):
    print("ssh: connect to host " + host + " port 22: Connection refused", file=sys.stderr)
    sys.exit(255)
sys.exit(subprocess.run(["bash", "-c", script]).returncode)
'''
