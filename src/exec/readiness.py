"""Read-only target readiness. Never invoke a backend or discovery command."""

stage = "settings"
try:
    import shutil
    request = json.loads(sys.argv[1])
    backend = request["backend"]
    if backend not in ("codex", "claude"):
        raise Refused()
    environment = worker_environment(request["excluded_env"])
    if not shutil.which(backend, path=environment.get("PATH", "")):
        print("fridica-readiness:backend-missing", flush=True)
        sys.exit(97)
    if not request.get("parent", False):
        home = path(os.environ["HOME"] if request["home"] is None else request["home"])
        workspace = request["workspace"]
        if workspace.startswith("~/"):
            workspace = home + workspace[1:]
        workspace = path(workspace)
        stage = "workspace"
        os.close(directory(home))
        os.close(directory(workspace))
        stage = "settings"
        if backend == "codex":
            settings_snapshots(home, workspace, [], [], request, environment,
                               ["codex", "app-server"], codex_only=True)
        # Claude starts with --strict-mcp-config and explicit empty MCP/settings.
    print("fridica-readiness:ready", flush=True)
except SystemExit:
    raise
except (OSError, ValueError, KeyError, TypeError, ImportError, RecursionError, Refused):
    print("fridica-readiness:" + stage + "-refused", flush=True)
    sys.exit(97)
