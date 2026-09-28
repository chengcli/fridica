"""Fixed remote artifact reader. Check confinement before emitting any bytes."""
try:
    request = json.loads(sys.argv[1])
    home = path(os.environ["HOME"])

    def expanded(value):
        if value.startswith("~/"):
            value = home + value[1:]
        return path(value)

    limit = request["limit"]
    if type(limit) is not int or not 0 <= limit <= 20 * 1024 * 1024:
        raise Refused()
    roots = request["roots"]
    if not isinstance(roots, list) or not 1 <= len(roots) <= 16:
        raise Refused()
    target = os.path.realpath(expanded(request["path"]), strict=True)
    allowed = False
    for root in roots:
        resolved = os.path.realpath(expanded(root), strict=True)
        if within(target, resolved) and target != resolved:
            # Open the resolved root and target without following any links.
            os.close(directory(resolved))
            allowed = True
            break
    if not allowed:
        raise Refused()
    # Stable in-workspace symlinks match local transport behavior. A replaced
    # component after realpath is refused by the shared no-follow reader.
    fd = regular(target)
    with os.fdopen(fd, "rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise Refused()
    sys.stdout.buffer.write(b"fridica-artifact-v1\0" + len(data).to_bytes(8, "big") + data)
    sys.stdout.buffer.flush()
except (OSError, ValueError, KeyError, TypeError, AttributeError, OverflowError, Refused):
    sys.stderr.write("fridica artifact: read refused\n")
    sys.exit(97)
