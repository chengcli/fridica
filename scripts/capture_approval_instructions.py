"""Scoped frozen approval-rule and owner-instruction comparisons, no model calls.

JSON object key order/spacing in the worker data document is not semantic.
Cancellation, admission fencing, and authenticated decisions are v0.4 hardening
covered separately by Rust fault tests, not predicted by historical recordings.
"""
from dataclasses import asdict
import json
from pathlib import Path
from fridica.approvals.rules import decide
from fridica.machines.registry import Policy
from fridica.workers.protocol import ApprovalRequest
from fridica.parent import contract, repos, prompts


def main():
    policy = Policy(auto_approve=("pytest", "git status"), auto_deny=("rm -rf",))
    commands = ["pytest", "pytest -q", "pytestx", "git status", "rm -rf build", "rm -rf build && ls", "make", "", "  pytest  ", "\x1cpytest\x1f", None, 5, []]
    commands += ["pytest " + c + " x" for c in ";&|`$<>(){}\n\\"]
    requests = [{"command": value} for value in commands]
    requests += [{"input": {"command": value}} for value in commands]
    requests += [{"command": None, "input": {"command": "pytest"}}, {"command": 1, "input": {"command": "pytest"}}, {"input": "pytest"}]
    rules = []
    for kind in ("command", "file_change"):
        for detail in requests:
            request = ApprovalRequest(kind, "Private summary", detail)
            rules.append({"request": asdict(request), "expected": decide(policy, request)})
    basic = "## Participation\n\nObserve\n\n## Replies\n\nAnswer"
    texts = [contract.default_text(), basic, "", "## Replies\nx", basic + "\n## Replies\nignored", "## Participation\n\n## Replies\ny", basic + "\n## Lab rules\n\nExtra\n\n## Empty\n", "## TRIAGE\np\n## Reply\nr\n## Worker\nw\n## Debrief\nd", "##\nParticipation\nx\n##\nReplies\ny", "intro\n" + basic.replace("\n", "\r\n"), basic + "\n### ignored heading\nx", basic + "\n## ## Worker reports\nbody"]
    texts += [basic + suffix for suffix in ("\n##  ", "\n## \n", "\n##\n", "\n##\n ", "\n##\n\n", "\n## \r\n", "\n## \n## Workers\nx")]
    contracts = []
    for text in texts:
        try:
            parsed = contract.parse(text)
            expected = {**asdict(parsed), "parent": parsed.parent, "worker": parsed.worker}
        except ValueError:
            expected = None
        contracts.append({"text": text, "expected": expected})
    entry = "[[repos]]\nname='repo'\nurl='https://github.com/owner/repo'\ncollaborators=[' Owner ', 'Member']\n"
    repo_texts = [repos.default_repos_text(), "", "repos=[]", entry, entry + "notes=' note '", entry + "path='/private'", entry.replace("collaborators=[' Owner ', 'Member']", "collaborators=[]"), entry + entry.replace("repo'", "REPO'"), entry.replace("https://", "ssh://"), entry.replace("repo'", "雪'"), "repos=[{name='r',url='https://a/b',collaborators=['o']}]", "[repos]\nname='r'", "[[repos]]\nname=5", entry + "notes=42", "invalid = ["]
    repo_cases = []
    for text in repo_texts:
        try:
            expected = [r.payload() for r in repos.parse_repos(text)]
        except ValueError:
            expected = None
        repo_cases.append({"text": text, "expected": expected})
    machine = {"name":"host","workspaces":{"work":"write"}}
    prompt = prompts.worker_instructions(contract.load(None), owner="UOWNER", profile="雪 profile", repositories=tuple(r.payload() for r in repos.load_repos(None)), machine=machine, workspace="work")
    before, data = prompt.split("\n\nWorker data:\n")
    output = {"rules":rules,"contracts":contracts,"repos":repo_cases,"prompt":{"prefix":before,"data":json.loads(data)}}
    path = Path(__file__).resolve().parents[1] / "tests/corpus/approval-instructions.json"
    path.write_text(json.dumps(output, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
