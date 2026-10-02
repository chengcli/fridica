"""Bounded settings snapshots for the fixed confinement helper.

Uses the helper's no-follow file opens. Never executes a discovery command or
prints source text. Unsupported/ambiguous inputs refuse admission. Every MCP
server found in a backend settings layer is disabled: workers run without MCP.
"""


def settings_snapshots(home, workspace, masks, state, request, environment, command, *, codex_only=False):
    import datetime
    import math
    import tomllib
    from urllib.parse import urlsplit, urlunsplit

    codex_home = home + "/.codex"
    if codex_only:
        # Unrestricted workers retain custom backend state/authentication roots.
        # Discovery still validates the path and reads only supported layers.
        codex_home = path(environment.get("CODEX_HOME", codex_home))
    else:
        # Confined custom state roots require a writable-mount inventory.
        for key, expected in [("CODEX_HOME", codex_home),
                              ("CLAUDE_CONFIG_DIR", home + "/.claude")]:
            if key in environment and path(environment[key]) != expected:
                raise Refused()

    limit = 1024 * 1024
    candidates = {codex_home + "/config.toml", "/etc/codex/config.toml",
                  "/etc/codex/managed_config.toml"}
    if not codex_only:
        candidates.update({home + "/.claude.json", home + "/.claude/settings.json",
                           home + "/.claude/settings.local.json"})
    try:
        fd = directory(codex_home)
    except FileNotFoundError:
        fd = None
    try:
        if fd is not None:
            # Iterate instead of materializing an unbounded directory listing.
            with os.scandir(fd) as entries:
                for count, entry in enumerate(entries):
                    if count >= 4096:
                        raise Refused()
                    if entry.name.endswith(".config.toml"):
                        candidates.add(codex_home + "/" + entry.name)
    finally:
        if fd is not None:
            os.close(fd)
    suffixes = ["/.codex/config.toml"]
    if not codex_only:
        suffixes += ["/.mcp.json", "/.claude/settings.json", "/.claude/settings.local.json"]
    ancestor = workspace
    while True:
        candidates.update(ancestor.rstrip("/") + suffix for suffix in suffixes)
        if ancestor == "/":
            break
        ancestor = os.path.dirname(ancestor)
    if len(candidates) > 256:
        raise Refused()

    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise Refused()
            result[key] = value
        return result

    def invalid_constant(_):
        raise Refused()

    documents = []
    total = 0
    for name in sorted(candidates):
        # Do not restore a layer already hidden by a private-directory mask.
        if (any(within(name, mask) for mask in masks)
                and not any(within(name, root) for root in [workspace] + state)):
            continue
        try:
            fd = regular(name)
        except FileNotFoundError:
            continue
        with os.fdopen(fd, "rb") as source:
            raw = source.read(limit + 1)
        total += len(raw)
        if len(raw) > limit or total > 8 * limit or len(documents) >= 64:
            raise Refused()
        raw = raw.decode("utf-8")
        document = (tomllib.loads(raw) if name.endswith(".toml") else
                    json.loads(raw, object_pairs_hook=unique, parse_constant=invalid_constant))
        if not isinstance(document, dict):
            raise Refused()
        documents.append((name, document))

    aliases = set()

    def recognized(alias, server):
        # Every server, whatever it points at: a worker gets no MCP at all.
        command = server.get("command", "")
        args = server.get("args", [])
        if not isinstance(command, str) or not isinstance(args, list):
            raise Refused()
        return True

    def visit(value, clean=False, depth=0):
        if depth > 64:
            raise Refused()
        if isinstance(value, list):
            for child in value:
                visit(child, clean, depth + 1)
        elif isinstance(value, dict):
            for key, child in list(value.items()):
                if key in ("mcp_servers", "mcpServers"):
                    if not isinstance(child, dict):
                        raise Refused()
                    for alias, server in list(child.items()):
                        if (not alias or len(alias) > 1024 or any(ord(c) < 32 or ord(c) == 127 for c in alias)
                                or not isinstance(server, dict)):
                            raise Refused()
                        if recognized(alias, server):
                            aliases.add(alias)
                            if clean:
                                if key == "mcp_servers":
                                    child[alias] = {"command": "/bin/false", "enabled": False}
                                else:
                                    del child[alias]
                else:
                    visit(child, clean, depth + 1)

    # Discover identities across every layer before removing their credentials.
    for _, document in documents:
        visit(document)
    if len(aliases) > 128:
        raise Refused()

    if codex_only:
        return [], sorted(aliases)

    def toml(value, depth=0):
        if depth > 64:
            raise Refused()
        if isinstance(value, str):
            return json.dumps(value, ensure_ascii=False)
        if isinstance(value, bool):
            return "true" if value else "false"
        if isinstance(value, int):
            return str(value)
        if isinstance(value, float):
            return repr(value) if math.isfinite(value) else ("nan" if math.isnan(value) else
                                                            ("inf" if value > 0 else "-inf"))
        if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
            return value.isoformat()
        if isinstance(value, list):
            return "[" + ", ".join(toml(v, depth + 1) for v in value) + "]"
        if isinstance(value, dict):
            return "{ " + ", ".join(toml(k, depth + 1) + " = " + toml(v, depth + 1)
                                    for k, v in value.items()) + " }"
        raise Refused()

    snapshots = []
    for name, document in documents:
        visit(document, clean=True)
        if name.endswith(".toml"):
            text = "\n".join(toml(k) + " = " + toml(v) for k, v in document.items()) + "\n"
            # Refuse rather than emitting subtly invalid TOML for an exotic value.
            tomllib.loads(text)
        else:
            text = json.dumps(document, ensure_ascii=False, allow_nan=False)
        data = text.encode("utf-8")
        if len(data) > 2 * limit:
            raise Refused()
        snapshots.append((name, data))
    return snapshots, sorted(aliases)
