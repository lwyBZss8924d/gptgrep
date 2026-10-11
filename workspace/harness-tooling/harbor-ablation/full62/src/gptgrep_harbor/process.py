"""One bounded child process per phase; no retry or renewed deadline."""
from __future__ import annotations

import os
import selectors
import signal
import subprocess
import time

from .common import MAX_JSON, finite_number, require


def run(argv, *, cwd, timeout, env, input_bytes=None):
    require(finite_number(timeout) and 0 < timeout <= 3600, "timeout")
    require(input_bytes is None or isinstance(input_bytes, bytes) and len(input_bytes) <= MAX_JSON,
            "stdin_bound")
    started = time.monotonic()
    child = None
    streams = {"stdout": bytearray(), "stderr": bytearray()}
    selector = selectors.DefaultSelector()
    failure = None
    old = {}
    cancelled = False
    sent = 0

    def cancel(_signum, _frame):
        nonlocal cancelled
        cancelled = True

    for sig in (signal.SIGTERM, signal.SIGINT):
        old[sig] = signal.signal(sig, cancel)
    try:
        require(not cancelled, "cancelled")
        child = subprocess.Popen(argv, cwd=cwd, env=env,
                                 stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
        for name in streams:
            stream = getattr(child, name)
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, selectors.EVENT_READ, name)
        if input_bytes is not None:
            if input_bytes:
                os.set_blocking(child.stdin.fileno(), False)
                selector.register(child.stdin, selectors.EVENT_WRITE, "stdin")
            else:
                child.stdin.close()
        while selector.get_map() or child.poll() is None:
            if cancelled:
                raise ValueError("cancelled")
            remaining = timeout - (time.monotonic() - started)
            if remaining <= 0:
                raise TimeoutError("deadline")
            for key, _ in selector.select(min(0.1, remaining)):
                if key.data == "stdin":
                    try:
                        sent += os.write(key.fd, input_bytes[sent:sent + 65536])
                    except BlockingIOError:
                        continue
                    except BrokenPipeError:
                        failure = "stdin_incomplete"
                    if sent == len(input_bytes) or failure == "stdin_incomplete":
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                    continue
                try:
                    data = os.read(key.fd, 65536)
                except BlockingIOError:
                    continue
                if not data:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                output = streams[key.data]
                require(len(output) + len(data) <= MAX_JSON, "stream_limit")
                output.extend(data)
    except (ValueError, TimeoutError, subprocess.TimeoutExpired, OSError):
        failure = "cancelled" if cancelled else "child_failed_or_limit"
    finally:
        # Signal only the live group created by this invocation. Drain descriptors
        # never extends the fixed deadline; descendants in the group are stopped.
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=1)
        for key in list(selector.get_map().values()):
            key.fileobj.close()
        selector.close()
        for sig, handler in old.items():
            signal.signal(sig, handler)
    return {"return_code": child.returncode if child else None,
            "failure": failure, "elapsed_ms": round((time.monotonic() - started) * 1000),
            "stdin_bytes": len(input_bytes) if input_bytes is not None else 0,
            "stdin_bytes_sent": sent,
            "stdin_complete": input_bytes is None or sent == len(input_bytes),
            "stdout": bytes(streams["stdout"]), "stderr": bytes(streams["stderr"])}
