"""Injectable identifiers for deterministic capture and replay."""
from __future__ import annotations

import uuid


class Identifiers:
    def hex(self, namespace: str, size: int = 32) -> str:
        return uuid.uuid4().hex[:size]


class SequenceIds(Identifiers):
    def __init__(self):
        self.counter = 0

    def hex(self, namespace: str, size: int = 32) -> str:
        self.counter += 1
        # Prefix truncation must not discard the changing part of the counter.
        return f"{self.counter:0{size}x}"
