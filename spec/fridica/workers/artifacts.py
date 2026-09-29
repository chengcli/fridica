"""Files a job names in its WorkerResult: read from the worker's machine, checked, and kept for upload.

A file is used only when its real path lies inside the worker's workspace, its
content matches its declared kind (PNG, PDF, or UTF-8 Markdown), and it fits the
upload limit.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import PurePosixPath

from ..core.models import ArtifactRef
from ..exec.transport import ARTIFACT_LIMIT, Transport

SUFFIXES = {"png": ".png", "pdf": ".pdf", "md": ".md"}


@dataclass(frozen=True)
class Collected:
    ref: ArtifactRef
    data: bytes | None
    error: str = ""


def check(ref: ArtifactRef, data: bytes) -> None:
    if ref.kind == "png" and not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("not a PNG image")
    if ref.kind == "pdf" and not data.startswith(b"%PDF-"):
        raise ValueError("not a PDF document")
    if ref.kind == "md":
        try:
            data.decode("utf-8")
        except UnicodeDecodeError:
            raise ValueError("not a UTF-8 Markdown file") from None


async def collect(transport: Transport, root: PurePosixPath, refs: tuple[ArtifactRef, ...]) -> list[Collected]:
    collected = []
    for ref in refs:
        path = PurePosixPath(ref.path)
        try:
            if not (path.is_absolute() or str(path).startswith("~/")) or ".." in path.parts:
                raise ValueError("artifact paths must be absolute")
            if path.suffix.lower() != SUFFIXES[ref.kind]:
                raise ValueError(f"a {ref.kind} artifact must end in {SUFFIXES[ref.kind]}")
            data = await transport.read_file(path, roots=(root,), limit=ARTIFACT_LIMIT)
            check(ref, data)
            collected.append(Collected(ref, data))
        except (ValueError, OSError) as error:
            collected.append(Collected(ref, None, str(error)))
    return collected
