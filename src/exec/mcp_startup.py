"""Disable every MCP server in the target's Codex settings before unrestricted
Codex startup.

This is not a filesystem sandbox. Owner settings and backend authentication stay
in place. No backend, MCP executable, shell or discovery command runs while
reading settings; only final per-alias CLI overrides are added.
"""


def main():
    request = json.loads(sys.argv[1])
    home = path(os.environ["HOME"] if request["home"] is None else request["home"])
    workspace = request["workspace"]
    if workspace.startswith("~/"):
        workspace = home + workspace[1:]
    workspace = path(workspace)
    environment = worker_environment(request["excluded_env"])
    environment["HOME"] = home
    command = sys.argv[2:]
    if not command or os.path.basename(command[0]) != "codex" or command[1:2] != ["app-server"]:
        raise Refused()
    # Pin the working directory and reject symlink traversal before settings
    # discovery. Close it before exec; no helper descriptors reach the backend.
    fd = directory(workspace, create=request["create"])
    try:
        _, aliases = settings_snapshots(home, workspace, [], [], request, environment,
                                        command, codex_only=True)
        for alias in aliases:
            command += ["-c", "mcp_servers." + json.dumps(alias, ensure_ascii=False) + ".enabled=false"]
        os.fchdir(fd)
    finally:
        os.close(fd)
    os.execvpe(command[0], command, environment)


try:
    main()
except (OSError, ValueError, KeyError, TypeError, ImportError, RecursionError, Refused):
    sys.stderr.write("fridica worker MCP: setup refused\n")
    sys.exit(97)
