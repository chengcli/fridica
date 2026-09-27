"""Capture scoped transport projections from frozen Python, without SSH/network.

Argument quoting and sandbox prefixes compare exactly. Remote templates compare
exit status/stdout after executing locally; watchdog implementation details and
real backend protocol behavior are outside this projection.
"""
import asyncio
from dataclasses import asdict
import json
import os
from pathlib import Path, PurePosixPath
import shlex
import subprocess
import tempfile
from unittest.mock import patch

from fridica.core.models import ArtifactRef
from fridica.exec.process import scrubbed_environment
from fridica.exec.sandbox import confinement, shell_words
from fridica.exec.ssh import remote_script
from fridica.workers.artifacts import check, collect


def main():
    root = Path(__file__).resolve().parents[1]
    words = ["", "plain", "a b", "it's", "$HOME/a", "`touch BAD`", "$(touch BAD)", "line\nbreak", "雪", "a\\b", 'a"b', "-n", "@%+=:,./-"]
    quotes = [{"input": word, "expected": shlex.quote(word)} for word in words]
    sandboxes = []
    for home in (None, "/home/owner"):
        for roots in (["/work"], ["/my repo", "/path'quote"], ["~/repo"] if home is None else ["/home/owner/repo"]):
            argv = confinement(map(PurePosixPath, roots), home=home)
            sandboxes.append({"roots": roots, "home": home, "argv": argv, "shell": shell_words(argv)})
    environments = []
    for extra in ({"OMP_NUM_THREADS": "3"}, {"SLACK_OVERRIDE": "restored", "HIDDEN": "xoxb-example"}):
        inherited = {"PATH": "/bin", "SLACK_ANY": "hidden", "CUSTOM": "private", "LOOKS_LIKE_TOKEN": "xapp-example", "KEEP": "yes"}
        with patch.dict(os.environ, inherited, clear=True):
            expected = scrubbed_environment(("CUSTOM",), extra)
        item = {"inherited": inherited, "excluded": ["CUSTOM"], "extra": extra, "expected": expected}
        if "SLACK_OVERRIDE" in extra:
            item["exception"] = {"reason": "v0.4 also scrubs overrides to prevent credential reinsertion", "removed_keys": ["SLACK_OVERRIDE", "HIDDEN"]}
        environments.append(item)
    remote = []
    with tempfile.TemporaryDirectory() as directory:
        home = Path(directory)
        (home / "my repo").mkdir()
        for word in words:
            command = ["sh", "-c", 'printf "%s|%s|%s" "$1" "$PWD" "$OMP_NUM_THREADS"', "-", word]
            script = remote_script(command, PurePosixPath("~/my repo"), env={"OMP_NUM_THREADS": "4"}, timeout=5)
            result = subprocess.run(["sh", "-c", script], env={**os.environ, "HOME": str(home)}, capture_output=True, timeout=10)
            remote.append({"command": command, "cwd": "~/my repo", "env": {"OMP_NUM_THREADS": "4"}, "timeout": 5, "expected": {"status": result.returncode, "stdout": result.stdout.decode().replace(str(home), "/HOME")}})
    contents = []
    for kind in ("png", "pdf", "md"):
        for data in (b"", b"# text", b"\x89PNG\r\n\x1a\nDATA", b"%PDF-1.7", b"\xff\xfe", "雪".encode()):
            ref = ArtifactRef("/work/answer." + kind, kind)
            try:
                check(ref, data)
                accepted = True
            except ValueError:
                accepted = False
            contents.append({"kind": kind, "data": list(data), "accepted": accepted})
    class Reader:
        async def read_file(self, path, **kwargs):
            return b"# text"
    references = []
    for path in ("/work/answer.md", "~/answer.MD", "relative.md", "../answer.md", "/work/../answer.md", "/work/answer.pdf", "/work//answer.md"):
        ref = ArtifactRef(path, "md")
        result = asyncio.run(collect(Reader(), PurePosixPath("/work"), (ref,)))[0]
        references.append({"reference": asdict(ref), "accepted": result.data is not None})
    corpus = {"quotes": quotes, "sandboxes": sandboxes, "environments": environments, "remote": remote, "contents": contents, "references": references}
    (root / "tests/corpus/transport.json").write_text(json.dumps(corpus, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"Captured {sum(map(len, corpus.values()))} scoped transport/artifact cases.")


if __name__ == "__main__":
    main()
