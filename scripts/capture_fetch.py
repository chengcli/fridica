"""Frozen scoped-fetch selector/output projections; no network or credentials.

The Rust target helper uses private staging, fixed tools, checked publication
and channel-EOF cleanup. These are explicit hardening differences, not an exact
replay of Python's four subprocesses. First-attempt context remains compatible;
Rust retries use fresh destinations instead of reusing the first directory.
"""
import asyncio
import json
from pathlib import Path, PurePosixPath
from types import SimpleNamespace
from fridica.machines.registry import Policy
from fridica.workers.fetch import fetch_repo


async def capture():
    cases = []
    sha = "a" * 40
    inputs = [(repo, "refs/heads/main", "j1", sha) for repo in ["Owner/Repo", "owner/repo", "Other/Repo", "https://github.com/Owner/Repo", "Kwner/Repo", "", "../Repo"]]
    inputs += [("Owner/Repo", ref, "j1", sha) for ref in ["HEAD", "refs/pull/12/head", "refs/heads/feature/a", "refs/heads/雪", "refs/heads/a..b", "refs/heads/a.lock", "refs/heads/a//b", "refs/heads/a:refs/heads/b", "--upload-pack=evil", "refs/pull/0/head", "refs/tags/v1", "refs/heads/-bad", "", "a"*40, "A"*64, "a"*41, "a"*39, "a"*65]]
    inputs += [("Owner/Repo", "HEAD", job, sha) for job in ["a", "a"*64, "a"*65, "a-b_c", "../a", "a.b", "a;bad", "雪", ""]]
    inputs += [("Owner/Repo", "HEAD", "j1", commit) for commit in ["A"*64, "b"*41, "a"*39, "a"*65, "not-a-commit", "", "\n"+sha+"\n"]]
    policy = Policy(fetch_repos=("Owner/Repo", "KwNeR/Repo"), approvals="on-request")
    for repo, ref, job, commit in inputs:
        calls = []
        class Transport:
            async def run(self, command, workspace, **options):
                calls.append(command)
                return SimpleNamespace(returncode=0, stdout=commit.encode() if "rev-parse" in command else b"")
        try:
            path, result = await fetch_repo(Transport(), PurePosixPath("/work"), policy, repo, ref, job)
            command = next(command for command in calls if "fetch" in command)
            url = command[command.index("--no-recurse-submodules")+1]
            expected = {"path":path,"commit":result,"repo":url.removeprefix("https://github.com/").removesuffix(".git")}
            error = None
        except ValueError:
            expected, error = None, "selector"
        except Exception:
            expected, error = None, "commit"
        cases.append({"repo":repo,"reference":ref,"job_id":job,"commit_output":commit,"expected":expected,"error":error})
    root = Path(__file__).resolve().parents[1]
    (root / "tests/corpus/fetch.json").write_text(json.dumps(cases,ensure_ascii=False,sort_keys=True,separators=(",",":"))+"\n")
    print(f"Captured {len(cases)} scoped fetch projections.")


if __name__ == "__main__":
    asyncio.run(capture())
