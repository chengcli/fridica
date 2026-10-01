#!/usr/bin/env python3
"""Exercise the installed native archive offline, outside the source checkout.

Only synthetic tokens, a held local CONNECT proxy and scripted backend probes are
used. This never authenticates to Slack or sends model requests.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import platform
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("candidate_install", ROOT / "packaging/install.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

LINUX = platform.system() == "Linux"
# Operator tools need 3.11+; macOS /usr/bin/python3 is older, so use this interpreter there.
PYTHON = "/usr/bin/python3" if LINUX else sys.executable

BACKEND = '''#!/usr/bin/python3
import json, os, sys
assert not any('SLACK' in k or k.startswith('FRIDICA_') for k in os.environ)
args = sys.argv[1:]
if args == ['--help']:
    print('--input-format --permission-prompts --json-schema --setting-sources --strict-mcp-config --append-system-prompt --session-id dontAsk "auto"')
elif args == ['auth', 'status']:
    print(json.dumps({'loggedIn': True}))
else:
    raise SystemExit('unexpected backend/model invocation')
'''


def run(binary, *args, env, cwd, ok=True):
    result = subprocess.run([str(binary), *map(str, args)], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=60)
    if (result.returncode == 0) != ok:
        raise AssertionError(f"command {args}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout if ok else result.stderr


def smoke(archive, expected, report):
    manifest, _ = installer.verify(archive, expected)
    checks = []
    with tempfile.TemporaryDirectory(prefix="fridica-package-", dir="/var/tmp") as tmp:
        root = Path(tmp).resolve()
        installation = root / "installation"
        installer.install(archive, expected, installation)
        checks.append("verified archive installed into new private directory")
        binary = installation / "bin/fridica-candidate"
        for directory in ("home", "project", "bin", "private"):
            (root / directory).mkdir(mode=0o700)
        environment = {"HOME": str(root / "home"), "PATH": f"{root}/bin:/usr/bin:/bin",
                       "XDG_RUNTIME_DIR": str(root / "private"),
                       "LANG": "C.UTF-8", "SLACK_APP_TOKEN": "xapp-synthetic", "SLACK_USER_TOKEN": "xoxp-synthetic"}
        # env_clear equivalent: no PYTHONPATH, shell config, agent or real credentials.
        def command(*args, ok=True):
            return run(binary, *args, env=environment, cwd=root, ok=ok)
        assert json.loads(command("build-info")) == manifest["build"]
        assert manifest["build"]["version"] in command("--version")
        for line in command("assets", "--list").splitlines():
            digest, name = line.split("  ", 1)
            assert hashlib.sha256((installation / "share/assets" / name).read_bytes()).hexdigest() == digest
        checks.append("the native binary and every embedded asset match manifest")
        config = root / "private/config.toml"
        command("init", "--config", config)
        assert config.stat().st_mode & 0o777 == 0o600
        command("configure", "--config", config, "--owner-id", "UOWNER", "--workspace-id", "TTEAM", "--channel-id", "CROOM")
        # Complete target and inventory with synthetic deployment inputs.
        config.write_text(f'''# installed smoke fixture
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[parent]
backend="claude"
[machines.local]
backends=["claude"]
[machines.local.policy]
gpu_confine=false
[machines.local.workspaces]
project={json.dumps(str(root / 'project'))}
[state]
path="state.sqlite3"
control_socket="control.sock"
[github]
enabled=false
[isolation]
mcp_inventory_complete=true
''')
        fake = root / "bin/claude"
        fake.write_text(BACKEND)
        fake.chmod(0o700)
        for helper in ("bwrap", "socat"):
            path = root / "bin" / helper
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o700)
        fingerprint = json.loads(command("check-config", "--config", config))["fingerprint"]
        assert not (root / "private/state.sqlite3").exists()
        ready = json.loads(command("start", "--check-ready", "--config", config))
        assert ready["startup_checks_passed"] and "active_launch_ready" not in ready
        diagnostics = json.loads(command("doctor", "--config", config, "--json"))
        assert not diagnostics["cancelled"]
        command("init-state", "--config", config)
        command("init-state", "--config", config)
        db = root / "private/state.sqlite3"
        # The schema this candidate installs is the number of migrations it ships.
        latest = len(list((installation / "share/assets/migrations").glob("*.sql")))
        assert latest >= 6
        with sqlite3.connect(db) as conn:
            assert conn.execute("SELECT value FROM meta WHERE key='schema_version'").fetchone() == (str(latest),)
        checks.append(f"offline init/configure/readiness/doctor/fresh v{latest} initialization")
        unit = command("service-print", "--config", config, "--environment-file", root / "private/credentials.env")
        # The default unit runs the daemon; --observe-only is an explicit opt-in.
        assert "--observe-only" not in unit and "--active" not in unit and "synthetic" not in unit
        observer_unit = command("service-print", "--config", config, "--environment-file", root / "private/credentials.env", "--observe-only")
        assert observer_unit.replace(" --observe-only", "") == unit
        unit_path = root / "fridica-candidate.service"
        unit_path.write_text(unit)
        if LINUX:
            run("/usr/bin/systemd-analyze", "verify", unit_path, env=environment, cwd=root)
            checks.append("generated daemon systemd unit passes systemd-analyze verify")
        else:
            checks.append("generated daemon systemd unit content (systemd-analyze skipped: not Linux)")

        def lifecycle(active):
            with socket.socket() as proxy:
                proxy.bind(("127.0.0.1", 0))
                proxy.listen()
                proxy.settimeout(30)
                environment["HTTPS_PROXY"] = f"http://127.0.0.1:{proxy.getsockname()[1]}"
                environment["NO_PROXY"] = ""
                args = ["start", "--config", str(config)]
                args += [] if active else ["--observe-only"]
                child = subprocess.Popen([binary, *args], env=environment, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    connection, _ = proxy.accept()
                    with connection:
                        connection.settimeout(5)
                        request = connection.recv(4096)
                        assert request.startswith(b"CONNECT slack.com:443 ") and b"synthetic" not in request
                        status = json.loads(command("status", "--config", config))
                        assert status["observe_only"] == (not active)
                        assert "another Fridica process" in command(*args, ok=False)
                        # Snapshot tool must honor the running daemon's lock.
                        result = subprocess.run([PYTHON, installation / "share/state_snapshot.py", "backup", "--database", db, "--config", config, "--output", root / "locked-backup"], env=environment, cwd=root, capture_output=True)
                        assert result.returncode != 0 and not (root / "locked-backup").exists()
                        child.send_signal(signal.SIGTERM if active else signal.SIGINT)
                        out, err = child.communicate(timeout=10)
                        assert child.returncode == 0, (out, err)
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.communicate(timeout=10)
                assert not (root / "private/control.sock").exists()
                with sqlite3.connect(db) as conn:
                    assert conn.execute("SELECT slack_status FROM runtime").fetchone() == ("stopped",)
                    assert conn.execute("SELECT count(*) FROM jobs").fetchone() == (0,)
                    assert conn.execute("SELECT count(*) FROM outbox").fetchone() == (0,)

        lifecycle(False)
        checks.append("installed observer: status, exclusive lock, snapshot refusal, SIGINT drain; no jobs/posts")
        before = config.read_text()
        fake.unlink()
        assert "readiness checks did not pass" in command("start", "--config", config, ok=False)
        fake.write_text(BACKEND.replace("'loggedIn': True", "'loggedIn': False"))
        fake.chmod(0o700)
        assert "doctor checks did not pass" in command("start", "--config", config, ok=False)
        fake.write_text(BACKEND.replace("print(json.dumps({'loggedIn': True}))",
            f"open({str(config)!r}, 'a').write('\\n# changed during probes\\n'); print(json.dumps({{'loggedIn': True}}))"))
        assert "configuration changed" in command("start", "--config", config, ok=False)
        config.write_text(before)
        fake.write_text(BACKEND)
        lifecycle(True)
        checks.append("default start reruns readiness/auth, refuses config changes during probes, serves controls and drains SIGTERM with Slack held locally")

        snapshot = installation / "share/state_snapshot.py"
        def snapshot_command(*args):
            return run(PYTHON, snapshot, *args, env=environment, cwd=root)
        snapshot_command("backup", "--database", db, "--config", config, "--output", root / "snapshot")
        snapshot_command("restore", "--snapshot", root / "snapshot", "--output", root / "recovery")
        with sqlite3.connect(root / "recovery/state.sqlite3") as conn:
            assert conn.execute("PRAGMA integrity_check").fetchone() == ("ok",)
            assert conn.execute("SELECT slack_status FROM runtime").fetchone() == ("stopped",)
        assert (root / "recovery/config.toml").read_bytes() == config.read_bytes()
        checks.append("fresh-state SQLite snapshot and explicit restoration into new directory")

        # Build a complete legacy fixture using only the packaged immutable DDL.
        upgrade = root / "upgrade"
        upgrade.mkdir(mode=0o700)
        legacy = upgrade / "state.sqlite3"
        legacy_config = upgrade / "config.toml"
        legacy_config.write_text(before + '\n[limits]\nmax_wait_replies=5\n')
        original_config = legacy_config.read_bytes()
        with sqlite3.connect(legacy) as conn:
            for version in range(1, 6):
                conn.executescript((installation / f"share/assets/migrations/{version:03}.sql").read_text())
                conn.execute("INSERT OR REPLACE INTO meta VALUES('schema_version',?)", (str(version),))
        snapshot_command("backup", "--database", legacy, "--config", legacy_config, "--output", root / "legacy-snapshot")
        args = ["migrate", "--database", legacy, "--config", legacy_config]
        plan = json.loads(command(*args, "--dry-run"))
        assert plan["from"] == 5 and plan["to"] == latest
        command(*args)
        command(*args)
        command(*args, "--rollback")
        with sqlite3.connect(legacy) as conn:
            assert conn.execute("SELECT value FROM meta WHERE key='schema_version'").fetchone() == ("5",)
        assert legacy_config.read_bytes() == original_config
        # Rollback preserves its journal; a fresh rehearsal pair starts a new migration.
        second = root / "second-upgrade"
        snapshot_command("restore", "--snapshot", root / "legacy-snapshot", "--output", second)
        args = ["migrate", "--database", second / "state.sqlite3", "--config", second / "config.toml"]
        command(*args)
        with sqlite3.connect(second / "state.sqlite3") as conn:
            conn.execute("INSERT INTO meta VALUES('post_migration_write','1')")
        assert "durable mutations" in command(*args, "--rollback", ok=False)
        snapshot_command("restore", "--snapshot", root / "legacy-snapshot", "--output", root / "explicit-recovery")
        with sqlite3.connect(root / "explicit-recovery/state.sqlite3") as conn:
            assert conn.execute("SELECT value FROM meta WHERE key='schema_version'").fetchone() == ("5",)
            assert conn.execute("SELECT value FROM meta WHERE key='post_migration_write'").fetchone() is None
        checks.append("legacy v5: dry-run, repeated migration, rollback, durable-write refusal and explicit backup restoration")
        result = {"build": manifest["build"], "archive_sha256": expected,
                  "synthetic_config_fingerprint": fingerprint, "passed": checks,
                  "platform": f"{platform.system()} {platform.machine()}",
                  "deployment_ready": False, "scope": "isolated synthetic candidate packaging rehearsal; no live credentials or Slack"}
        report.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    smoke(args.archive.resolve(), args.sha256, args.report.resolve())


if __name__ == "__main__":
    main()
