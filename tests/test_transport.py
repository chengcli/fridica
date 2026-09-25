import asyncio
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import time
import sys

import pytest

from fridica.core.errors import BackendError
from fridica.exec import process
from fridica.exec.sandbox import confinement, prepare_script, shell_words
from fridica.exec.slurm import SlurmNotImplemented
from fridica.exec.ssh import control_directory, remote_script, ssh_command
from fridica.exec.transport import make_transport
from fridica.machines.registry import Machine, Policy, Resources, Slurm, Workspace


def machine(name="dart9", transport="ssh", workspaces=(), **changes):
    return Machine(name=name, transport=transport, workspaces=tuple(workspaces), backends=("codex",),
                   default_backend="codex", policy=Policy(), host=name if transport != "local" else "", **changes)


def test_ssh_command_shape_and_control_directory(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    argv = ssh_command("dart9", "exec sh -c true")
    assert argv[:2] == ["ssh", "-T"] and argv[-3:] == ["--", "dart9", "exec sh -c true"]
    assert "BatchMode=yes" in argv and "ControlMaster=auto" in argv and f"ControlPath={tmp_path}/fridica/%C" in argv
    assert (tmp_path / "fridica").stat().st_mode & 0o777 == 0o700
    (tmp_path / "loose").mkdir()
    (tmp_path / "loose" / "fridica").mkdir(mode=0o755)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path / "loose"))
    with pytest.raises(OSError):
        control_directory()


@pytest.mark.parametrize("shell", ["bash", "sh"])
def test_remote_script_quotes_arguments_env_and_home_relative_cwd(tmp_path, monkeypatch, shell):
    home = tmp_path / "home"
    (home / "my repo").mkdir(parents=True)
    monkeypatch.setenv("HOME", str(home))
    hostile = "it's $HOME `touch INJECTED` \\ \"q\""
    script = remote_script(["sh", "-c", 'printf "%s|%s|%s" "$1" "$PWD" "$OMP_NUM_THREADS"', "-", hostile],
                           PurePosixPath("~/my repo"), env={"OMP_NUM_THREADS": "4"}, timeout=5)
    assert script.startswith("exec sh -c ")
    result = subprocess.run([shell, "-c", script], capture_output=True, text=True, cwd=tmp_path)
    assert result.stdout == f"{hostile}|{home / 'my repo'}|4"
    assert not (tmp_path / "INJECTED").exists()
    missing = remote_script(["true"], PurePosixPath(tmp_path / "missing"))
    assert subprocess.run([shell, "-c", missing], capture_output=True).returncode == 98


def test_confinement_binds_writable_roots_and_keeps_settings_read_only():
    words = confinement([PurePosixPath("/work"), PurePosixPath("~/repo")], home=None)
    assert words[:2] == ["bwrap", "--die-with-parent"] and words[-1] == "--"
    assert ["--bind", "/work", "/work"] == words[words.index("/work") - 1:words.index("/work") + 2]
    assert "$HOME/repo" in words and "$HOME/.codex/config.toml" in words
    quoted = shell_words(words)
    assert "\"$HOME/\"repo" in quoted and "\"$HOME/\".claude/settings.json" in quoted
    local = confinement([Path("/work")], home="/home/me")
    assert "/home/me/.claude/hooks" in local


def test_local_launch_scrubs_tokens_and_adds_resource_limits(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxp-secret")
    monkeypatch.setenv("SOMETHING", "xoxb-also-secret")
    monkeypatch.setenv("KEEP", "yes")
    local = machine("local", "local", [Workspace("w", tmp_path, Policy())], resources=Resources(cpus=3, gpus=(1,)))
    spec = make_transport(local).launch(["codex", "app-server"], tmp_path, env={"EXTRA": "1"})
    assert spec.argv == ["codex", "app-server"] and spec.cwd == tmp_path
    assert "SLACK_USER_TOKEN" not in spec.env and "SOMETHING" not in spec.env and spec.env["KEEP"] == "yes"
    assert spec.env["OMP_NUM_THREADS"] == "3" and spec.env["CUDA_VISIBLE_DEVICES"] == "1" and spec.env["EXTRA"] == "1"
    confined = make_transport(local).launch(["codex"], tmp_path, confine=(tmp_path / "only",))
    assert confined.argv[0] == "bwrap" and str(tmp_path / "only") in confined.argv
    assert (tmp_path / "home/.claude/settings.json").read_text() == "{}"
    assert (tmp_path / "home/.codex/config.toml").read_text() == "" and (tmp_path / "home/.claude/hooks").is_dir()


def test_remote_confinement_prepares_settings_without_clobbering(tmp_path, monkeypatch):
    home = tmp_path / "home"
    (home / ".codex").mkdir(parents=True)
    (home / ".codex" / "config.toml").write_text("model = 'mine'\n")
    monkeypatch.setenv("HOME", str(home))
    spec = make_transport(machine()).launch(["true"], PurePosixPath(tmp_path), confine=(PurePosixPath("~/w"),))
    script = spec.argv[-1]
    assert '"$HOME/"w' in script and "bwrap" in script and ".claude/settings.json" in script
    subprocess.run(["sh", "-c", remote_script(["true"], PurePosixPath(tmp_path), prepare=prepare_script())], check=True)
    assert (home / ".claude/settings.json").read_text() == "{}"
    assert (home / ".codex/config.toml").read_text() == "model = 'mine'\n"


def test_ssh_round_trip_runs_on_the_host_in_the_workspace(fake_ssh, tmp_path):
    work = tmp_path / "remote"
    work.mkdir()
    transport = make_transport(machine(workspaces=[Workspace("w", PurePosixPath(work), Policy())],
                                       resources=Resources(cpus=2)))
    completed = asyncio.run(transport.run(["sh", "-c", 'cat; echo "$PWD $OMP_NUM_THREADS"'], PurePosixPath(work),
                                          stdin=b"hello ", timeout=10))
    assert completed.returncode == 0 and completed.text == f"hello {work} 2\n"
    assert json.loads(fake_ssh.read_text().splitlines()[0])["host"] == "dart9"


def test_ssh_failures_are_explained(fake_ssh, tmp_path, monkeypatch):
    transport = make_transport(machine(workspaces=[Workspace("w", PurePosixPath(tmp_path), Policy())]))
    monkeypatch.setenv("SSH_FAIL", "1")
    completed = asyncio.run(transport.run(["true"], PurePosixPath(tmp_path), timeout=10))
    assert completed.returncode == 255 and "ssh dart9" in transport.failure(255, "")
    monkeypatch.delenv("SSH_FAIL")
    completed = asyncio.run(transport.run(["true"], PurePosixPath(tmp_path / "nope"), timeout=10))
    assert completed.returncode == 98 and "does not exist" in transport.failure(98, "")


def test_spawned_process_speaks_over_stdio(fake_ssh, tmp_path):
    transport = make_transport(machine(workspaces=[Workspace("w", PurePosixPath(tmp_path), Policy())]))

    async def scenario():
        child = await transport.spawn(["sh", "-c", "read line; echo got:$line"], PurePosixPath(tmp_path))
        child.stdin.write(b"ping\n")
        await child.stdin.drain()
        line = await child.stdout.readline()
        await child.wait()
        return line

    assert asyncio.run(scenario()) == b"got:ping\n"


@pytest.mark.parametrize("remote", [False, True])
def test_read_file_stays_inside_roots(fake_ssh, tmp_path, remote):
    root = tmp_path / "root"
    root.mkdir()
    (root / "plot.png").write_bytes(b"\x89PNG\r\n\x1a\nDATA")
    (tmp_path / "secret.md").write_text("no")
    (root / "link.md").symlink_to(tmp_path / "secret.md")
    kind = machine(transport="ssh") if remote else machine("local", "local")
    transport = make_transport(kind)
    roots = (PurePosixPath(root),)
    assert asyncio.run(transport.read_file(PurePosixPath(root / "plot.png"), roots=roots)).endswith(b"DATA")
    for bad in (tmp_path / "secret.md", root / "link.md", root / "missing.md"):
        with pytest.raises(ValueError):
            asyncio.run(transport.read_file(PurePosixPath(bad), roots=roots))
    with pytest.raises(ValueError, match="larger"):
        asyncio.run(transport.read_file(PurePosixPath(root / "plot.png"), roots=roots, limit=4))


def test_slurm_is_registered_but_not_runnable(tmp_path):
    slurm = make_transport(machine("gl", "slurm", slurm=Slurm(partition="gpu")))
    with pytest.raises(SlurmNotImplemented, match="not support"):
        slurm.launch(["codex"], PurePosixPath("/w"))


def test_run_once_kills_the_process_group_on_timeout(tmp_path):
    marker = tmp_path / "grandchild"
    script = f"(sleep 3; touch {marker}) & sleep 30"

    async def scenario():
        with pytest.raises(BackendError, match="timed out"):
            await process.run_once(["sh", "-c", script], cwd=tmp_path, env=dict(os.environ), timeout=0.5)
        await asyncio.sleep(3.5)

    asyncio.run(scenario())
    assert not marker.exists()


def test_run_once_bounds_output(tmp_path):
    with pytest.raises(BackendError, match="size limit"):
        asyncio.run(process.run_once(["sh", "-c", "yes | head -c 5000"], cwd=tmp_path, env=dict(os.environ),
                                     timeout=5, limit=1000))


def test_diagnostic_is_a_bounded_single_line():
    assert process.diagnostic(b"a\n  b\n" + b"x" * 1000, limit=10) == "x" * 10


BSD_REALPATH = r'''#!{python}
"""Behaves like macOS realpath: no -e option, and missing paths are errors."""
import os, sys
args = [arg for arg in sys.argv[1:] if arg != "--"]
if any(arg.startswith("-") for arg in args):
    print("realpath: illegal option", file=sys.stderr); sys.exit(1)
path = args[0]
if not os.path.exists(path):
    print(f"realpath: {path}: No such file or directory", file=sys.stderr); sys.exit(1)
print(os.path.realpath(path))
'''


def test_remote_read_works_with_bsd_realpath(fake_ssh, tmp_path, monkeypatch):
    binaries = tmp_path / "bsd"
    binaries.mkdir()
    (binaries / "realpath").write_text(BSD_REALPATH.replace("{python}", sys.executable))
    (binaries / "realpath").chmod(0o700)
    monkeypatch.setenv("PATH", str(binaries) + os.pathsep + os.environ["PATH"])
    root = tmp_path / "root"
    root.mkdir()
    (root / "notes.md").write_text("# hi")
    transport = make_transport(machine())
    roots = (PurePosixPath(root),)
    assert asyncio.run(transport.read_file(PurePosixPath(root / "notes.md"), roots=roots)) == b"# hi"
    with pytest.raises(ValueError):
        asyncio.run(transport.read_file(PurePosixPath(root / "missing.md"), roots=roots))


def test_terminate_survives_eperm_from_killpg(tmp_path, monkeypatch):
    """macOS returns EPERM for a process group that exited but was not reaped yet."""
    def eperm(pid, sig):
        raise PermissionError(1, "Operation not permitted")

    async def scenario():
        child = await process.start(["sh", "-c", "sleep 30"], cwd=tmp_path, env=dict(os.environ))
        monkeypatch.setattr(process.os, "killpg", eperm)
        await asyncio.wait_for(process.terminate(child, grace=0.5), 5)
        return child.returncode

    assert asyncio.run(scenario()) is not None


def _unraisable(run):
    """Run ``run`` in a fresh event loop and return exceptions the garbage collector reported afterwards."""
    import gc
    seen, previous = [], sys.unraisablehook
    sys.unraisablehook = seen.append
    try:
        asyncio.run(run())
        gc.collect()
    finally:
        sys.unraisablehook = previous
    return [str(item.exc_value) for item in seen]


def test_a_cancelled_run_releases_its_subprocess_before_the_loop_closes(tmp_path):
    async def scenario():
        task = asyncio.create_task(process.run_once(["sh", "-c", "sleep 30"], cwd=tmp_path, env=dict(os.environ),
                                                    timeout=60))
        await asyncio.sleep(0.3)
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    assert _unraisable(scenario) == []


def test_teardown_releases_pipes_held_by_an_escaped_grandchild(tmp_path):
    """A grandchild in its own session survives the group kill and keeps the pipes open."""
    marker = tmp_path / "escaped.pid"
    script = f"setsid sh -c 'echo $$ > {marker}; sleep 30' & sleep 30"

    async def cancelled_run():
        task = asyncio.create_task(process.run_once(["sh", "-c", script], cwd=tmp_path, env=dict(os.environ), timeout=60))
        await asyncio.sleep(0.5)
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    async def closed_worker():
        child = await process.start(["sh", "-c", script], cwd=tmp_path, env=dict(os.environ))
        await asyncio.sleep(0.5)
        await process.terminate(child)
        process.release(child)

    try:
        assert _unraisable(cancelled_run) == []
        assert _unraisable(closed_worker) == []
    finally:
        if marker.exists():
            try:
                os.kill(int(marker.read_text()), 9)
            except (ProcessLookupError, ValueError):
                pass


def _running(pid: str) -> bool:
    try:
        state = Path(f"/proc/{pid}/stat").read_text().split(")")[1].split()[0]
    except FileNotFoundError:
        return False
    return state != "Z"


@pytest.mark.skipif(not Path("/proc").is_dir(), reason="needs /proc")
@pytest.mark.parametrize("shell", ["bash", "sh"])
def test_long_lived_remote_agents_die_with_their_channel(tmp_path, shell):
    """Without a terminal sshd sends no hang-up; the watchdog stops the agent and its tools when stdin closes."""
    agent_pid, tool_pid = tmp_path / "agent", tmp_path / "tool"
    script = remote_script(["sh", "-c", f"echo $$ > {agent_pid}; (sleep 300 & echo $! > {tool_pid}; wait) & "
                                        "while read line; do echo got:$line; done; sleep 300"], PurePosixPath(tmp_path))
    child = subprocess.Popen([shell, "-c", script], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                             start_new_session=True)
    try:
        child.stdin.write("ping\n")
        child.stdin.flush()
        assert child.stdout.readline() == "got:ping\n"
        time.sleep(0.3)
        child.stdin.close()  # the SSH channel closed
        child.wait(timeout=20)
        time.sleep(0.5)
        assert not _running(agent_pid.read_text().strip()) and not _running(tool_pid.read_text().strip())
    finally:
        for marker in (agent_pid, tool_pid):
            if marker.exists():
                try:
                    os.kill(int(marker.read_text()), 9)
                except (ProcessLookupError, ValueError):
                    pass


@pytest.mark.parametrize("shell", ["bash", "sh"])
def test_a_long_lived_agent_that_exits_passes_its_status(tmp_path, shell):
    script = remote_script(["sh", "-c", "read line; echo done:$line; exit 7"], PurePosixPath(tmp_path))
    child = subprocess.run([shell, "-c", script], input="x\n", capture_output=True, text=True, timeout=20)
    assert (child.returncode, child.stdout) == (7, "done:x\n")
    assert not list(Path(os.environ.get("TMPDIR", "/tmp")).glob("fridica-*.in"))
