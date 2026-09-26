import asyncio
import subprocess
from pathlib import PurePosixPath

import pytest

from fridica.exec.local import LocalTransport
from fridica.exec.ssh import SshTransport
from fridica.machines.registry import Policy


@pytest.mark.parametrize("remote", [False, True])
def test_fetched_ref_is_a_local_bare_repo_without_a_push_remote(config, workspace, tmp_path, fake_ssh, remote,
                                                                 monkeypatch):
    from fridica.workers.fetch import _fetch_ref, fetch_repo

    source = tmp_path / "source"
    source.mkdir()
    subprocess.run(["git", "init", "-q", "--initial-branch=main", str(source)], check=True)
    (source / "file.txt").write_text("review me\n")
    for args in (["git", "-C", str(source), "add", "file.txt"],
                 ["git", "-C", str(source), "-c", "user.name=Test", "-c", "user.email=test@example.com",
                  "commit", "-qm", "Initial"]):
        subprocess.run(args, check=True)
    expected = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    transport = SshTransport(config.machines["snowy"]) if remote else LocalTransport(config.machines["local"])
    work_path = PurePosixPath(workspace) if remote else workspace
    with monkeypatch.context() as env:
        env.setenv("GIT_DIR", str(tmp_path / "wrong-repo"))
        env.setenv("GIT_TEMPLATE_DIR", str(tmp_path / "wrong-template"))
        target, sha = asyncio.run(_fetch_ref(transport, work_path, str(source), "refs/heads/main", "j1"))
    assert sha == expected
    assert subprocess.check_output(["git", "--git-dir", target, "show", "FETCH_HEAD:file.txt"], text=True) == "review me\n"
    assert subprocess.check_output(["git", "--git-dir", target, "remote", "-v"], text=True) == ""
    with pytest.raises(ValueError, match="not granted"):
        asyncio.run(fetch_repo(transport, work_path, Policy(fetch_repos=("chengcli/snapy",)),
                               "other/snapy", "refs/heads/main", "j2"))
    with pytest.raises(ValueError, match="invalid GitHub ref"):
        asyncio.run(fetch_repo(transport, work_path, Policy(fetch_repos=("chengcli/snapy",)),
                               "chengcli/snapy", "--upload-pack=evil", "j3"))
