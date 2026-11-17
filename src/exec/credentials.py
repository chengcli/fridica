"""Transfer worker credentials over SSH stdin, never shell arguments or files."""
import json
import os
import struct
import subprocess
import sys
import threading

LIMIT = 262144


def read_exact(size):
    chunks = []
    while size:
        chunk = os.read(0, size)
        if not chunk:
            raise ValueError("incomplete credential frame")
        chunks.append(chunk)
        size -= len(chunk)
    return b"".join(chunks)


def receive():
    size = struct.unpack("!I", read_exact(4))[0]
    if size > LIMIT:
        raise ValueError("credential frame too large")
    values = json.loads(read_exact(size))
    if not isinstance(values, dict) or any(
        not isinstance(k, str) or not k or "=" in k or "\0" in k
        or not isinstance(v, str) or "\0" in v for k, v in values.items()
    ):
        raise ValueError("invalid credential frame")
    os.environ.update(values)
    os.execv("/bin/sh", ["sh", "-c", sys.argv[2]])


def send():
    names = json.loads(sys.argv[2])
    payload = json.dumps({name: os.environ[name] for name in names}).encode()
    if len(payload) > LIMIT:
        raise ValueError("credential frame too large")
    child = subprocess.Popen(sys.argv[3:], stdin=subprocess.PIPE)

    def pump():
        try:
            child.stdin.write(struct.pack("!I", len(payload)) + payload)
            child.stdin.flush()
            while True:
                chunk = os.read(0, 65536)
                if not chunk:
                    break
                child.stdin.write(chunk)
                child.stdin.flush()
        except (BrokenPipeError, OSError):
            pass
        finally:
            try:
                child.stdin.close()
            except OSError:
                pass

    threading.Thread(target=pump, daemon=True).start()
    code = child.wait()
    return code if code >= 0 else 128 - code


try:
    if sys.argv[1] == "receive":
        receive()
    elif sys.argv[1] == "send":
        sys.exit(send())
    else:
        raise ValueError("unknown credential operation")
except Exception:
    print("worker credential transfer failed", file=sys.stderr)
    sys.exit(97)
