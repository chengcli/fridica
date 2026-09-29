"""Capture resolved configuration and validation acceptance from frozen v0.3.11.

The fixtures compare public configuration after removing the two replaced legacy
loop limits. v0.4 attention, path hardening and migration choices have Rust tests.
No credentials are read. Run with PYTHONPATH=spec.
"""
import ast
from dataclasses import asdict
import json
from pathlib import Path, PurePath
import tempfile
import unicodedata

from fridica.config import load_config

BASE = '''
[owner]
slack_user = "UOWNER"
[slack]
workspace = "TTEAM"
channels = ["CROOM", "COTHER"]
[machines.local]
backends = ["claude", "codex"]
[machines.local.workspaces]
project = "__ROOT__/project"
[state]
path = "__ROOT__/state.sqlite3"
'''


def clean(value, root):
    if isinstance(value, dict):
        return {key: clean(v, root) for key, v in value.items() if key not in ("_index", "fingerprint", "max_wait_replies", "max_no_progress")}
    if isinstance(value, (list, tuple)):
        return [clean(v, root) for v in value]
    if isinstance(value, PurePath):
        value = str(value)
    if isinstance(value, str):
        return value.replace(str(root), "__ROOT__").replace(str(Path.home()), "__HOME__")
    return value


def cases():
    yield "defaults", BASE
    for section, values in {
        "parent": ["backend='codex'\nreasoning_effort='high'", "timeout=1\ncontext_chars=2000", "model='custom'\ntriage_model='small'", "repos='repos.toml'"],
        "policy": ["mode='read-only'", "network=['*']\napprovals='never'", "fetch_repos=['owner/project']\napprovals='on-request'", "gpu_confine=false"],
        "github": ["enabled=false\ncache_seconds=0\ntoken_env='GH_READ_TOKEN'"],
        "limits": ["max_wait_replies=9\nmax_no_progress=2\nauto_resume=true\njob_timeout=60", "reply_chars=500", "reply_chars=12000", "report_fast_path=false"],
    }.items():
        for i, value in enumerate(values):
            yield f"{section}-{i}", BASE + f"\n[{section}]\n{value}\n"
    yield "owner-files", BASE.replace('slack_user = "UOWNER"', 'slack_user = "UOWNER"\ncontract="contract-custom.md"\nprofile="scientist"')
    yield "slack-options", BASE.replace('workspace = "TTEAM"', 'workspace = "TTEAM"\ndelegate_channels=[]\ncooldown=0\ngeneral_messages=false')
    yield "policy-inheritance", BASE + '''
[policy]
network=["*.example.com"]
approvals="on-request"
[machines.box]
host="box"
policy={approvals="never"}
[machines.box.workspaces]
data={path="/data",policy={mode="read-only"}}
work="/work"
careful={path="/careful",policy={approvals="auto"},subfolders=false}
'''
    yield "gpu-auto", BASE + '''
[machines.gpu]
host="gpu"
resources={gpus=[0,1],cpus=32,memory_gb=128,notes="test"}
[machines.gpu.workspaces]
work="/work"
notes={path="/notes",policy={mode="read-only"}}
raw={path="/raw",policy={mode="full"}}
plain={path="/plain",policy={gpu_confine=false}}
'''
    yield "slurm-config-only", BASE + '''
[machines.batch]
transport="slurm"
host="batch"
slurm={account="science",partition="gpu",extra=["--nodes=1"]}
[machines.batch.workspaces]
work="~/work"
'''
    root = Path(__file__).resolve().parents[1]
    template = (root / "spec/fridica/config/template.toml").read_text()
    template = template.replace('"~/project"', '"__ROOT__/project"').replace(
        '"~/.local/state/fridica/state.sqlite3"', '"__ROOT__/state.sqlite3"')
    yield "packaged-template", template
    head, rest = template.split("# [machines.snowy]", 1)
    machines, tail = ("# [machines.snowy]" + rest).split("[state]", 1)
    machines = "\n".join(line[2:] if line.startswith("# ") else line for line in machines.splitlines())
    yield "packaged-template-all-machines", head + machines + "\n[state]" + tail
    tree = ast.parse((root / "spec/tests/test_config.py").read_text())
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name == "test_rejects_invalid_machines":
            rows = ast.literal_eval(node.decorator_list[0].args[1])
            for i, (text, _) in enumerate(rows):
                yield f"baseline-invalid-machine-{i}", BASE + text
    for section, values in {
        "policy": ["mode='bad'", "approvals='bad'", "network=['https://github.com']", "fetch_repos=['owner/..']", "fetch_repos=['owner/repo','OWNER/REPO']", "approval_timeout=0", "approval_timeout=true", "auto_approve=['']", "gpu_confine='auto'", "claude_prompts='bad'", "fetch_repos=['owner/repo']", "unknown=true"],
        "parent": ["backend='gpt'", "default_machine='missing'", "context_chars=1999", "context_chars=true", "timeout=0", "timeout=nan", "reasoning_effort='bad'", "repos='missing.toml'"],
        "limits": ["max_jobs=0", "max_wait_replies=0", "max_jobs=true", "max_jobs=1.5", "auto_resume=1", "reply_chars=499", "reply_chars=12001", "max_turns=6"],
        "github": ["enabled=1", "cache_seconds=-1", "token_env='NOT-AN-ENV'"],
    }.items():
        for i, value in enumerate(values):
            yield f"invalid-{section}-{i}", BASE + f"\n[{section}]\n{value}\n"
    for i, replacement in enumerate(['["#general"]', '["CROOM","CROOM"]', '[]', 'true']):
        yield f"invalid-channels-{i}", BASE.replace('["CROOM", "COTHER"]', replacement)
    for i, options in enumerate(['cpus=0', 'cpus=true', 'gpus=[0,0]', 'gpus=[-1]', 'gpus=[false]', 'memory_gb=0', 'memory_gb=nan']):
        yield f"invalid-resources-{i}", BASE.replace('[machines.local]', f'[machines.local]\nresources={{{options}}}')
    yield "unknown-root", BASE + "\n[unknown]\nvalue=true"
    yield "missing-machine", BASE.split('[machines.local]')[0]
    yield "missing-workspace", BASE.replace('__ROOT__/project', '__ROOT__/absent')
    yield "state-in-workspace", BASE.replace('__ROOT__/state.sqlite3', '__ROOT__/project/state.sqlite3')


def main():
    rows = []
    with tempfile.TemporaryDirectory(prefix="fc-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        (root / "project").mkdir()
        (root / "etc").mkdir()
        (root / "etc/repos.toml").write_text("# fixture\n")
        (root / "etc/contract-custom.md").write_text("Fixture contract\n")
        path = root / "etc/config.toml"
        for name, source in cases():
            path.write_text(source.replace("__ROOT__", str(root)))
            row = {"id": name, "source": source}
            try:
                config = load_config(path)
                row["expected"] = clean(asdict(config), root)
            except (ValueError, TypeError) as error:
                row["error"] = True
                row["diagnostic"] = str(error).replace(str(root), "__ROOT__")
            rows.append(row)
    path = Path(__file__).resolve().parents[1] / "tests/corpus/config.json"
    path.write_text(json.dumps(rows, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"Captured {len(rows)} configuration fixtures.")
    folds = [{"input": c, "expected": unicodedata.normalize("NFD", c).casefold()}
             for c in map(chr, range(0x110000)) if c.casefold() != c]
    path.with_name("path_folds.json").write_text(json.dumps(folds, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
