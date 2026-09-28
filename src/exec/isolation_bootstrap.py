"""Select a system Python with tomllib; never search a worker-controlled PATH."""
import os
import sys

if sys.platform in ("linux", "darwin") and sys.version_info >= (3, 11):
    source = sys.argv.pop(1)
    exec(compile(source, "<fridica-isolation>", "exec"))
elif sys.platform in ("linux", "darwin"):
    candidates = ("/usr/bin/python3.14", "/usr/bin/python3.13",
                  "/usr/bin/python3.12", "/usr/bin/python3.11")
    if sys.platform == "darwin":
        candidates += ("/opt/homebrew/bin/python3.14", "/opt/homebrew/bin/python3.13",
                       "/opt/homebrew/bin/python3.12", "/opt/homebrew/bin/python3.11",
                       "/usr/local/bin/python3.14", "/usr/local/bin/python3.13",
                       "/usr/local/bin/python3.12", "/usr/local/bin/python3.11")
    for executable in candidates:
        if os.path.isfile(executable):
            try:
                os.execve(executable, [executable, "-I", "-S", "-c", sys.argv[1]] + sys.argv[2:], os.environ)
            except OSError:
                break
    sys.stderr.write("fridica worker isolation: setup refused\n")
    sys.exit(97)
else:
    sys.stderr.write("fridica worker isolation: setup refused\n")
    sys.exit(97)
