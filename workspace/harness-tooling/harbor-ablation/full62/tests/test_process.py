"""Focused process ownership regressions; no model or provider subprocess."""
from pathlib import Path
import errno
import os
import signal
import subprocess
import sys
import unittest
from unittest.mock import patch

from gptgrep_harbor.process import run


class ProcessOwnershipTests(unittest.TestCase):
    def test_completed_child_group_is_not_signalled_after_reap(self):
        with patch("gptgrep_harbor.process.os.killpg") as signal_group:
            result = run(
                [sys.executable, "-I", "-B", "-c", "print('synthetic-complete')"],
                cwd=str(Path(__file__).resolve().parent), timeout=5, env={})
        self.assertEqual(result["return_code"], 0)
        self.assertIsNone(result["failure"])
        self.assertEqual(result["stdout"], b"synthetic-complete\n")
        signal_group.assert_not_called()

    def test_permission_error_still_requires_bounded_owned_child_reap(self):
        cases = [
            ("import time; time.sleep(10)", 0.15, "child_failed_or_limit", False),
            ("import os,signal,time; os.kill(os.getppid(),signal.SIGTERM); time.sleep(10)",
             3.5, "cancelled", False),
            ("import time; time.sleep(10)", 0.15, "child_cleanup_failed", True),
        ]
        for script, deadline, failure, wait_fails in cases:
            with self.subTest(failure=failure):
                children = []
                waits = []
                signals = []
                original_popen = subprocess.Popen
                original_killpg = os.killpg
                handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGTERM, signal.SIGINT)}

                def spawn(*args, **kwargs):
                    child = original_popen(*args, **kwargs)
                    children.append(child)
                    original_wait = child.wait
                    waits.append(original_wait)
                    if wait_fails:
                        def fail_wait(timeout=None):
                            self.assertEqual(timeout, 2)
                            raise subprocess.TimeoutExpired(child.args, timeout)
                        child.wait = fail_wait
                    return child

                def signal_group(pid, signum):
                    self.assertIsNone(children[0].returncode)
                    self.assertEqual(pid, children[0].pid)
                    signals.append(signum)
                    if signum == signal.SIGKILL:
                        raise PermissionError(errno.EPERM, "synthetic unreaped group")
                    original_killpg(pid, signum)

                try:
                    with patch("gptgrep_harbor.process.subprocess.Popen", side_effect=spawn), \
                            patch("gptgrep_harbor.process.os.killpg", side_effect=signal_group):
                        result = run([sys.executable, "-I", "-B", "-c", script],
                                     cwd=str(Path(__file__).resolve().parent),
                                     timeout=deadline, env={}, input_bytes=b"x" * 262144)
                    self.assertEqual(result["failure"], failure)
                    self.assertEqual(signals, [signal.SIGTERM, signal.SIGKILL])
                    self.assertFalse(result["stdin_complete"])
                    if wait_fails:
                        self.assertIsNone(result["return_code"])
                        self.assertIsNone(children[0].returncode)
                    else:
                        self.assertEqual(result["return_code"], -signal.SIGTERM)
                        self.assertEqual(children[0].returncode, -signal.SIGTERM)
                finally:
                    # Reap the test-owned child even against the rejected preimage.
                    for child, wait in zip(children, waits):
                        wait(timeout=2)
                        for stream in (child.stdin, child.stdout, child.stderr):
                            if stream is not None:
                                stream.close()
                    for sig, handler in handlers.items():
                        signal.signal(sig, handler)
