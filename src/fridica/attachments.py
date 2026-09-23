"""Files a heavy-task worker attaches to its report: a summary figure, a Markdown note, a PDF.

The worker ends its report with one ``ATTACH: <absolute path>`` line per file (``FIGURE:``
is accepted too) naming files it wrote inside its host's roots. ``split_attachments``
removes those lines from the report, and ``load`` reads each file from the worker's host
(through the shared SSH connection for a remote host). A file is used only when its real
path, with symlinks resolved, lies inside one of that host's roots, its content matches
its type (PNG, PDF or UTF-8 Markdown), and it is within ``ATTACHMENT_LIMIT``.
"""
from __future__ import annotations

import asyncio
from pathlib import Path, PurePosixPath
import re
import shlex

from . import remote
from .config import Host
from .prompts import MAX_ATTACHMENTS

ATTACHMENT_LIMIT = 20 * 1024 * 1024
KINDS = {".png": "PNG image", ".pdf": "PDF document", ".md": "Markdown file"}
ATTACH_LINE = re.compile(r"^[ \t]*(?:ATTACH|FIGURE):[ \t]*(\S[^\n]*?)[ \t]*$", re.MULTILINE)
READ_TIMEOUT = 60


def split_attachments(report: str) -> tuple[str, list[str]]:
    """The report without its ``ATTACH:`` lines, and the distinct paths they name, at most MAX_ATTACHMENTS."""
    paths = []
    for value in ATTACH_LINE.findall(report):
        value = value.strip("`'\"")
        if value not in paths:
            paths.append(value)
    if not paths:
        return report, []
    return re.sub(r"\n{3,}", "\n\n", ATTACH_LINE.sub("", report)).strip(), paths[:MAX_ATTACHMENTS]


def _inside(path: PurePosixPath, roots) -> bool:
    return any(path == root or path.is_relative_to(root) for root in roots)


def _checked(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if not path.is_absolute() or ".." in path.parts or path.suffix.lower() not in KINDS or "\n" in value or "\0" in value:
        raise ValueError("An attachment must be an absolute path to a .png, .pdf or .md file")
    return path


def _validated(path: PurePosixPath, data: bytes) -> bytes:
    if len(data) > ATTACHMENT_LIMIT:
        raise ValueError("The attachment is larger than the upload limit")
    suffix = path.suffix.lower()
    if suffix == ".png" and not data.startswith(b"\x89PNG\r\n\x1a\n") or suffix == ".pdf" and not data.startswith(b"%PDF-"):
        raise ValueError(f"The attachment is not a {KINDS[suffix]}")
    if suffix == ".md":
        try:
            data.decode("utf-8")
        except UnicodeDecodeError:
            raise ValueError("The attachment is not a Markdown file") from None
    return data


async def load(host: Host, value: str) -> bytes:
    """The file at ``value`` on ``host``; ValueError when it is outside the host's roots or not an allowed file."""
    path = _checked(value)
    if not host.remote:
        real = Path(path).resolve(strict=True)
        roots = [Path(root).resolve() for root in host.roots]
        if not any(real == root or real.is_relative_to(root) for root in roots):
            raise ValueError("The attachment is outside the host's roots")
        with open(real, "rb") as stream:
            return _validated(path, stream.read(ATTACHMENT_LIMIT + 1))
    # One round trip: the resolved file path, then each root's resolved path, a NUL, then the bytes.
    roots = " ".join(shlex.quote(str(root)) for root in host.roots)
    script = (f"f=$(realpath -e -- {shlex.quote(str(path))}) || exit 3; printf '%s\\n' \"$f\"; "
              f"for r in {roots}; do realpath -e -- \"$r\" 2>/dev/null || echo; done; "
              f"printf '\\0'; head -c {ATTACHMENT_LIMIT + 1} -- \"$f\"")
    process = await asyncio.create_subprocess_exec(*remote.ssh_command(host, script), stdin=asyncio.subprocess.DEVNULL,
                                                   stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
    try:
        output, _ = await asyncio.wait_for(process.communicate(), READ_TIMEOUT)
    except asyncio.TimeoutError:
        process.kill()
        raise ValueError("Reading the attachment timed out") from None
    header, separator, data = output.partition(b"\0")
    if process.returncode != 0 or not separator:
        raise ValueError("The attachment could not be read")
    real, *resolved = header.decode("utf-8", "replace").splitlines()
    allowed = [*host.roots, *(PurePosixPath(item) for item in resolved if item)]
    if not _inside(PurePosixPath(real), allowed):
        raise ValueError("The attachment is outside the host's roots")
    return _validated(path, data)
