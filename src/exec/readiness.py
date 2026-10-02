"""Read-only target readiness. Never invoke a backend or discovery command."""

# Every facet is checked and every refusal reported, one per line, so one
# run shows everything to fix. Facets: backend present, workspace readable,
# settings sound.
FAILURES = (OSError, ValueError, KeyError, TypeError, ImportError, RecursionError, Refused)
refusals = []
try:
    import shutil
    request = json.loads(sys.argv[1])
    backend = request["backend"]
    if backend not in ("codex", "claude"):
        raise Refused()
    environment = worker_environment(request["excluded_env"])
    if not shutil.which(backend, path=environment.get("PATH", "")):
        refusals.append("backend-missing")
    if not request.get("parent", False):
        home = path(os.environ["HOME"] if request["home"] is None else request["home"])
        workspace = request["workspace"]
        if workspace.startswith("~/"):
            workspace = home + workspace[1:]
        workspace = path(workspace)
        try:
            os.close(directory(home))
            os.close(directory(workspace))
        except FAILURES:
            refusals.append("workspace-refused")
        if backend == "codex":
            try:
                settings_snapshots(home, workspace, [], [], request, environment,
                                   ["codex", "app-server"], codex_only=True)
            except FAILURES:
                refusals.append("settings-refused")
        # Claude starts with --strict-mcp-config and explicit empty MCP/settings.
except SystemExit:
    raise
except FAILURES:
    refusals.append("settings-refused")
for refusal in refusals:
    print("fridica-readiness:" + refusal, flush=True)
if refusals:
    sys.exit(97)
print("fridica-readiness:ready", flush=True)
