import os
from pathlib import Path
import subprocess
import sys

import pytest


GUARD = Path(__file__).resolve().parents[1] / "src/exec/overleaf_git.py"
PROJECT = "1" * 24
OTHER = "2" * 24


@pytest.mark.parametrize("scope,fetch,extra,allowed", [
    (PROJECT, PROJECT, "", True),
    (PROJECT, PROJECT, OTHER, False),
    (PROJECT, OTHER, "", False),
    ("unbound", PROJECT, "", False),
])
def test_push_checks_every_effective_url(tmp_path, scope, fetch, extra, allowed):
    # Real Git resolves the configuration. The stub records the final push
    # without connecting to a remote or requiring a credential.
    subprocess.run(["git", "init", "-q", tmp_path], check=True)
    origin = f"https://git.overleaf.com/{fetch}"
    subprocess.run(["git", "remote", "add", "origin", origin], cwd=tmp_path, check=True)
    for project in [PROJECT, extra] if extra else [PROJECT]:
        subprocess.run(["git", "config", "--add", "remote.origin.pushurl",
                        f"https://git.overleaf.com/{project}"], cwd=tmp_path, check=True)
    stub = tmp_path / "git-stub"
    stub.write_text('#!/bin/sh\nif [ "$1" = push ]; then touch pushed; exit 0; fi\nexec git "$@"\n')
    stub.chmod(0o700)
    result = subprocess.run([sys.executable, GUARD, "push", "origin", "HEAD:main"], cwd=tmp_path,
                            env={**os.environ, "FRIDICA_REAL_GIT": str(stub), "FRIDICA_THREAD_PROJECT": scope})
    assert (result.returncode == 0) == allowed
    assert (tmp_path / "pushed").exists() == allowed
