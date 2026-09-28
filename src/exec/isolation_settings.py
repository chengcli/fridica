"""Bounded settings snapshots for the fixed confinement helper.

Uses the helper's no-follow file opens. Never executes a discovery command or
prints source text. Unsupported/ambiguous inputs refuse admission. This is not
an inventory of arbitrary credential copies, external includes or opaque wrappers.
"""


def settings_snapshots(home, workspace, masks, state, request, environment, command):
    import datetime
    import math
    import tomllib
    from urllib.parse import urlsplit, urlunsplit

    # Custom state roots need their own private-file and writable-mount inventory.
    for key, expected in [("CODEX_HOME", home + "/.codex"),
                          ("CLAUDE_CONFIG_DIR", home + "/.claude")]:
        if key in environment and path(environment[key]) != expected:
            raise Refused()

    limit = 1024 * 1024
    candidates = {
        home + "/.codex/config.toml", home + "/.claude.json",
        home + "/.claude/settings.json", home + "/.claude/settings.local.json",
        "/etc/codex/config.toml", "/etc/codex/managed_config.toml",
    }
    try:
        fd = directory(home + "/.codex")
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
                        candidates.add(home + "/.codex/" + entry.name)
    finally:
        if fd is not None:
            os.close(fd)
    ancestor = workspace
    while True:
        candidates.update(ancestor.rstrip("/") + suffix for suffix in (
            "/.codex/config.toml", "/.mcp.json", "/.claude/settings.json",
            "/.claude/settings.local.json"))
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

    def endpoint(value):
        if not isinstance(value, str):
            return ""
        parts = urlsplit(value)
        # Queries/fragments may hold credentials; compare only the endpoint.
        authority = parts.netloc.rsplit("@", 1)[-1].lower()
        return urlunsplit((parts.scheme, authority, parts.path, "", ""))

    aliases = set(request.get("mcp_aliases", []))
    urls = {endpoint(value) for value in request.get("mcp_urls", [])}
    # Reuse the backend factory's explicit disable list as a sanitization
    # identity too, so an opaque wrapper cannot retain credentials in its file.
    if os.path.basename(command[0]) == "codex" and command[1:2] == ["app-server"]:
        for index, word in enumerate(command[:-1]):
            if word in ("-c", "--config"):
                override = tomllib.loads(command[index + 1]).get("mcp_servers", {})
                if not isinstance(override, dict):
                    raise Refused()
                for alias, value in override.items():
                    if isinstance(value, dict) and value.get("enabled") is False:
                        aliases.add(alias)

    def fridica_reference(value, depth=0):
        if depth > 64:
            raise Refused()
        if isinstance(value, str):
            return "FRIDICA_" in value.upper()
        if isinstance(value, dict):
            return any(fridica_reference(key, depth + 1) or fridica_reference(v, depth + 1)
                       for key, v in value.items())
        if isinstance(value, list):
            return any(fridica_reference(v, depth + 1) for v in value)
        return False

    def recognized(alias, server):
        command = server.get("command", "")
        args = server.get("args", [])
        if not isinstance(command, str) or not isinstance(args, list):
            raise Refused()
        return (alias in aliases or alias.lower() == "fridica"
                or os.path.basename(command) == "fridica"
                or (os.path.basename(command).startswith("python")
                    and any(args[i:i + 2] == ["-m", "fridica"] for i in range(len(args))))
                or endpoint(server.get("url")) in urls
                or fridica_reference(server))

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
