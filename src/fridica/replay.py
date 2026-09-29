"""Opt-in, complete boundary recording. Raw tapes are private and must be redacted.

A tape records call intent *before* the external effect, and its full result or
error afterward. Missing completions make it unsuitable for exact parity. This
does not pretend historical parent_turns.action_json is an external response.
"""
from __future__ import annotations

import asyncio
import base64
from dataclasses import asdict, is_dataclass
import hashlib
import hmac
import inspect
import json
import os
from pathlib import Path
import re

from .core import errors, models
from .workers.protocol import Outcome

TYPES = {cls.__name__: cls for cls in (models.Message, models.FridicaMeta, models.WorkerResult, Outcome)}
ERRORS = {name: getattr(errors, name) for name in
          ("BackendError", "SessionUnavailable", "DeliveryRejected", "DeliveryAmbiguous", "RateLimited")}
ERRORS.update({cls.__name__: cls for cls in (ValueError, KeyError, TypeError, OSError, TimeoutError)})


def encode(value):
    if is_dataclass(value):
        # Each boundary adapter must use a registered type; no dynamic imports on replay.
        name = type(value).__name__
        if name not in TYPES:
            raise TypeError(f"unregistered replay type: {name}")
        return {"$type": name, "value": encode(asdict(value))}
    if isinstance(value, bytes):
        return {"$bytes": base64.b64encode(value).decode()}
    if isinstance(value, tuple):
        return {"$tuple": [encode(v) for v in value]}
    if isinstance(value, list):
        return [encode(v) for v in value]
    if isinstance(value, dict):
        return {str(k): encode(v) for k, v in value.items()}
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    raise TypeError(f"unsupported replay value: {type(value).__name__}")


def decode(value):
    if isinstance(value, list):
        return [decode(v) for v in value]
    if not isinstance(value, dict):
        return value
    if "$bytes" in value:
        return base64.b64decode(value["$bytes"], validate=True)
    if "$tuple" in value:
        return tuple(decode(v) for v in value["$tuple"])
    if "$type" in value:
        data, name = decode(value["value"]), value["$type"]
        if name == "WorkerResult":
            return models.WorkerResult.from_dict(data)
        if name == "Outcome":
            return Outcome(models.WorkerResult.from_dict(data["result"]), data["backend_session_id"])
        if name == "Message":
            data["meta"] = models.FridicaMeta(**data["meta"]) if data["meta"] else None
            data["attachments"] = tuple(models.Attachment(**a) for a in data["attachments"])
        return TYPES[name](**data)
    return {k: decode(v) for k, v in value.items()}


class Recorder:
    def __init__(self, path: Path, clock):
        self.clock, self.seq, self.call_id = clock, 0, 0
        # Never append to or overwrite a prior corpus; exclusive creation prevents
        # accidental concatenation or following a pre-existing symbolic link.
        self.stream = os.fdopen(os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "w")

    def event(self, kind: str, payload):
        self.seq += 1
        self.stream.write(json.dumps({"seq": self.seq, "at": self.clock.now(), "kind": kind,
                                      "payload": encode(payload)}, sort_keys=True, allow_nan=False) + "\n")
        self.stream.flush()
        os.fsync(self.stream.fileno())

    async def call(self, operation, arguments, function):
        self.call_id += 1
        call_id = self.call_id
        self.event("call", {"id": call_id, "operation": operation, "arguments": arguments})
        try:
            result = await function()
        except BaseException as error:
            self.event("error", {"id": call_id, "type": type(error).__name__, "message": str(error),
                                 "retry_after": getattr(error, "retry_after", None)})
            raise
        self.event("result", {"id": call_id, "value": result})
        return result

    def close(self):
        self.stream.close()


def load(path: Path):
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    pending = set()
    seen = set()
    for seq, row in enumerate(rows, 1):
        if row["seq"] != seq:
            raise ValueError("noncontiguous tape ordering")
        payload = row["payload"]
        if row["kind"] == "call":
            if payload["id"] in seen:
                raise ValueError("duplicate call id")
            seen.add(payload["id"])
            pending.add(payload["id"])
        elif row["kind"] in ("result", "error"):
            if payload["id"] not in pending:
                raise ValueError("completion without call")
            pending.remove(payload["id"])
    if pending:
        raise ValueError("incomplete tape: external effects have unknown outcomes")
    return rows


class Replay:
    def __init__(self, rows, clock=None):
        self.rows, self.index, self.clock = rows, 0, clock
        self.changed = asyncio.Condition()

    def take(self):
        if self.index >= len(self.rows):
            raise AssertionError("replay exhausted")
        row = self.rows[self.index]
        self.index += 1
        if self.clock is not None:
            self.clock.set(row["at"])
        return row

    async def call(self, operation, arguments, function=None):
        async with self.changed:
            row = self.take()
            expected = row["payload"]
            if row["kind"] != "call" or expected["operation"] != operation or expected["arguments"] != encode(arguments):
                raise AssertionError(f"replay call mismatch at sequence {row['seq']}")
            call_id = expected["id"]
            self.changed.notify_all()
            # Concurrent requests may finish in a different order than they start.
            await asyncio.wait_for(self.changed.wait_for(lambda:
                self.index < len(self.rows) and self.rows[self.index]["kind"] in ("result", "error")
                and self.rows[self.index]["payload"]["id"] == call_id), timeout=5)
            row = self.take()
            self.changed.notify_all()
        if row["kind"] == "error":
            error = row["payload"]
            if error["type"] == "RateLimited":
                raise errors.RateLimited(error["retry_after"])
            cls = ERRORS.get(error["type"])
            if cls is None:
                raise ValueError(f"unscripted exception type: {error['type']}")
            raise cls(error["message"])
        return decode(row["payload"]["value"])

    def assert_consumed(self):
        if self.index != len(self.rows):
            raise AssertionError(f"{len(self.rows) - self.index} unexplained replay events remain")


class Proxy:
    """Wrap an async adapter, e.g. the parent LLM, Slack API, or GitHub snapshots."""
    def __init__(self, adapter, tape, namespace):
        self.adapter, self.tape, self.namespace = adapter, tape, namespace

    def __getattr__(self, name):
        attribute = getattr(self.adapter, name)
        if not inspect.iscoroutinefunction(attribute):
            return attribute

        async def call(*args, **kwargs):
            return await self.tape.call(f"{self.namespace}.{name}", {"args": args, "kwargs": kwargs},
                                        lambda: attribute(*args, **kwargs))
        return call


def compare(expected, actual, exceptions=()):
    """Compare all durable effect dimensions. Exceptions name exact JSON paths,
    include a reason, and must actually account for a difference (no stale waivers).
    """
    differences = {}
    def visit(left, right, path):
        if type(left) is not type(right):
            differences[path] = (left, right)
        elif isinstance(left, dict):
            for key in sorted(left.keys() | right.keys()):
                if key not in left or key not in right:
                    differences[f"{path}/{key}"] = (left.get(key), right.get(key))
                else:
                    visit(left[key], right[key], f"{path}/{key}")
        elif isinstance(left, list):
            if len(left) != len(right):
                differences[path] = (left, right)
            else:
                for index, (a, b) in enumerate(zip(left, right)):
                    visit(a, b, f"{path}/{index}")
        elif left != right:
            differences[path] = (left, right)
    visit(expected, actual, "")
    for exception in exceptions:
        if not exception.get("reason") or exception["path"] not in differences:
            raise AssertionError("parity exception is unexplained or stale")
        del differences[exception["path"]]
    if differences:
        raise AssertionError(f"unexplained parity differences at {sorted(differences)}")


class Redactor:
    """Conservative structural export: pseudonymize sensitive fields consistently.

    A schema supplies the keys whose values are safe protocol enums. Everything
    else is pseudonymized, including unknown fields and dictionary keys under
    dynamic maps. Prose is removed; these exports support relationship assertions,
    not exact behavioral claims. Capture synthetic scripts for exact parity.
    """
    def __init__(self, salt: bytes):
        if len(salt) < 16:
            raise ValueError("redaction salt must have at least 16 bytes")
        self.salt = salt

    def pseudonym(self, value):
        return "redacted_" + hmac.new(self.salt, value.encode(), hashlib.sha256).hexdigest()[:24]

    def value(self, value):
        if isinstance(value, str):
            # Composite thread identifiers preserve their links to workspace,
            # channel and root-ts columns when all three are redacted separately.
            if re.fullmatch(r"[TW][A-Z0-9]+:[CG][A-Z0-9]+:[0-9.]+", value):
                return ":".join(self.pseudonym(part) for part in value.split(":"))
            return self.pseudonym(value) if value else ""
        if isinstance(value, list):
            return [self.value(v) for v in value]
        if isinstance(value, dict):
            return {self.pseudonym(str(k)): self.value(v) for k, v in value.items()}
        return value

    def table(self, rows, columns):
        """Only explicitly supplied schema column names survive; JSON blobs recurse."""
        result = []
        for row in rows:
            output = {}
            for column in columns:
                if column not in row:
                    continue
                value = row[column]
                if column.endswith("_json") and isinstance(value, str):
                    try:
                        value = json.loads(value)
                    except ValueError:
                        pass  # Malformed historical blobs are still removed.
                output[column] = self.value(value)
            result.append(output)
        return result
