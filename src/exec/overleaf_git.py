#!/usr/bin/python3 -I
"""Check Interleaf's effective destination immediately before Git transfers."""
import os
import subprocess
import sys
from urllib.parse import urlsplit

git = os.environ["FRIDICA_REAL_GIT"]
project = os.environ["FRIDICA_THREAD_PROJECT"]
args = sys.argv[1:]
i = 0
while args[i:i + 1] == ["-c"]:
    i += 2
command = args[i:i + 1]


def verify(url):
    parsed = urlsplit(url)
    if parsed.hostname == "git.overleaf.com" or project != "unbound":
        if (project == "unbound" or parsed.scheme != "https"
                or parsed.hostname != "git.overleaf.com"
                or parsed.port not in (None, 443)
                or parsed.path.rstrip("/") != "/" + project):
            sys.exit("Git destination does not match the thread-confirmed Overleaf project")


if command == ["push"] or (project != "unbound" and command == ["fetch"]):
    # Interleaf fetches and pushes origin. Ask Git for every effective URL,
    # including pushurl and insteadOf rewrites, without logging credentials.
    checks = [[], ["--push"]] if command == ["push"] else [[]]
    for flags in checks:
        urls = subprocess.check_output([git, *args[:i], "remote", "get-url", *flags, "--all", "origin"],
                                       text=True).splitlines()
        if not urls:
            sys.exit("Missing Git origin")
        for url in urls:
            verify(url)
elif project != "unbound" and command == ["clone"]:
    url = subprocess.check_output([git, *args[:i], "ls-remote", "--get-url", args[i+1]], text=True)
    verify(url.strip())

os.execv(git, [git, *args])
