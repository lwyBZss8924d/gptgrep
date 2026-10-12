"""Run the codex-auth fake qualification suite inside a network/file guard.

usage: python3 -I -B tests/run_qualification.py --results-dir DIR
           [--codex-bin PATH --codex-sha256 HEX] [--pattern 'test_*.py']

Outer process: creates DIR/<UTC-stamp>/, reserves loopback ports and, on macOS,
re-executes itself under `sandbox-exec` with a profile that
  * denies all outbound network except the reserved loopback ports and the
    closed blocker port 9 (system proxies, DNS and real endpoints are unreachable);
  * denies any file access below the user's real ~/.gptgrep and ~/.codex.
Inner process: runs unittest discovery and writes logs plus summary.json.
Integration tests that execute a real Codex binary refuse to run unguarded.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shutil
import socket
import subprocess
import sys
import time
import unittest

HERE = Path(__file__).resolve().parent
SANDBOX_EXEC = "/usr/bin/sandbox-exec"


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def reserve_ports(count):
    sockets, ports = [], []
    for _ in range(count):
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        sockets.append(sock)
        ports.append(sock.getsockname()[1])
    for sock in sockets:
        sock.close()
    return ports


def system_proxy_ports():
    """Ports of configured macOS system HTTP/HTTPS/SOCKS proxies (for the guard self-check)."""
    try:
        out = subprocess.run(["/usr/sbin/scutil", "--proxy"], capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.TimeoutExpired):
        return []
    return sorted({int(m) for m in re.findall(r"(?:HTTP|HTTPS|SOCKS)Port : (\d+)", out)})


def profile(ports, home):
    rules = ["(version 1)", "(allow default)", "(deny network-outbound)"]
    rules += ['(allow network-outbound (remote ip "localhost:%d"))' % port for port in sorted(set(ports) | {9})]
    for root in (Path(home) / ".gptgrep", Path(home) / ".codex"):
        rules.append('(deny file-read* file-write* (subpath %s))' % json.dumps(str(root)))
    return "\n".join(rules) + "\n"


def outer(args):
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    results_dir = Path(os.path.realpath(args.results_dir))
    out = results_dir / stamp
    out.mkdir(mode=0o700, parents=True)
    (out / "tmp").mkdir(mode=0o700)
    (out / "tmp" / "home").mkdir(mode=0o700)
    (out / "tmp" / "root").mkdir(mode=0o700)
    (out / "logs").mkdir(mode=0o700)
    meta = {"stamp": stamp, "results_dir": str(out), "python": sys.version.split()[0],
            "platform": platform.platform(), "machine": platform.machine()}
    if args.codex_bin:
        actual = sha256(args.codex_bin)
        if args.codex_sha256 and actual != args.codex_sha256:
            print(json.dumps({"error": "codex_binary_pin_mismatch"}))
            return 2
        meta["codex"] = {"path": args.codex_bin, "sha256": actual, "pin_verified": bool(args.codex_sha256)}
    ports = reserve_ports(args.ports)
    home = pwd.getpwuid(os.getuid()).pw_dir
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LANG": "en_US.UTF-8", "HOME": str(out / "tmp" / "home"),
           "TMPDIR": str(out / "tmp"), "GPTGREP_AUTH_TEST_ROOT": str(out / "tmp" / "root"),
           "GPTGREP_AUTH_TEST_PORTS": ",".join(map(str, ports)),
           "GPTGREP_AUTH_TEST_PROXY_PORTS": ",".join(map(str, system_proxy_ports())),
           "GPTGREP_AUTH_TEST_OBSERVATIONS": str(out / "observations.jsonl")}
    if args.codex_bin:
        env["GPTGREP_AUTH_TEST_CODEX_BIN"] = args.codex_bin
        env["GPTGREP_AUTH_TEST_CODEX_SHA256"] = meta["codex"]["sha256"]
    inner = [sys.executable, "-I", "-B", str(Path(__file__).resolve()), "--inner", "--out", str(out),
             "--pattern", args.pattern]
    if Path(SANDBOX_EXEC).exists():
        text = profile(ports, home)
        (out / "sandbox.sb").write_text(text)
        meta["sandbox"] = {"kind": "seatbelt", "profile_sha256": hashlib.sha256(text.encode()).hexdigest(),
                           "allowed_loopback_ports": sorted(set(ports) | {9}),
                           "denied_roots": ["~/.gptgrep", "~/.codex"]}
        env["GPTGREP_AUTH_TEST_SANDBOX"] = "seatbelt"
        argv = [SANDBOX_EXEC, "-f", str(out / "sandbox.sb"), *inner]
    else:
        if args.codex_bin:
            print(json.dumps({"error": "refusing_real_binary_without_network_guard"}))
            return 2
        meta["sandbox"] = {"kind": "none"}
        argv = inner
    (out / "runner-meta.json").write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n")
    proc = subprocess.run(argv, env=env, cwd=str(out / "tmp"))
    summary_path = out / "summary.json"
    if summary_path.exists():
        summary = json.loads(summary_path.read_text())
        summary["runner"] = meta
        summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"summary": str(summary_path), "counts": summary["counts"]}))
    return proc.returncode


class Collector(unittest.TextTestResult):
    def __init__(self, *a, **k):
        super().__init__(*a, **k)
        self.records = []
        self._start = {}

    def startTest(self, test):
        self._start[test.id()] = time.monotonic()
        super().startTest(test)

    def _add(self, test, outcome, detail=None):
        seconds = round(time.monotonic() - self._start.get(test.id(), time.monotonic()), 3)
        item = {"id": test.id(), "outcome": outcome, "seconds": seconds}
        if detail:
            item["detail"] = detail[-1500:]
        self.records.append(item)

    def addSuccess(self, test):
        super().addSuccess(test)
        self._add(test, "passed")

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self._add(test, "failed", self._exc_info_to_string(err, test))

    def addError(self, test, err):
        super().addError(test, err)
        self._add(test, "error", self._exc_info_to_string(err, test))

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self._add(test, "skipped", reason)

    def addSubTest(self, test, subtest, err):
        super().addSubTest(test, subtest, err)
        if err is not None:
            self._add(subtest, "failed", self._exc_info_to_string(err, subtest))


def codex_version():
    binary = os.environ.get("GPTGREP_AUTH_TEST_CODEX_BIN")
    if not binary:
        return None
    home = Path(os.environ["GPTGREP_AUTH_TEST_ROOT"]) / "version-probe-home"
    home.mkdir(mode=0o700, exist_ok=True)
    env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "CODEX_HOME": str(home / "codex-home"),
           "TMPDIR": os.environ["TMPDIR"]}
    (home / "codex-home").mkdir(mode=0o700, exist_ok=True)
    out = subprocess.run([binary, "--version"], capture_output=True, text=True, env=env, timeout=60)
    return out.stdout.strip()


def inner(args):
    out = Path(args.out)
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    loader = unittest.TestLoader()
    suite = loader.discover(str(HERE), pattern=args.pattern, top_level_dir=str(HERE))
    with open(out / "logs" / "unittest.log", "w") as log:
        runner = unittest.TextTestRunner(stream=log, verbosity=2, resultclass=Collector)
        result = runner.run(suite)
    counts = {"total": result.testsRun, "passed": sum(1 for r in result.records if r["outcome"] == "passed"),
              "failed": len(result.failures), "errors": len(result.errors), "skipped": len(result.skipped)}
    summary = {"schema": "gptgrep.codex-auth.qualification.v1", "started_at": started,
               "finished_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
               "counts": counts, "tests": result.records, "codex_version": codex_version(),
               "guard": os.environ.get("GPTGREP_AUTH_TEST_SANDBOX", "none"),
               "fake_credentials_only": True,
               "scope": ["lease semantics", "layout/env allowlist", "owner refresh (fake and official binary)",
                         "consumer refresh blocker", "guarded-reload adoption", "401 fail-closed",
                         "shared/exclusive concurrency", "lifecycle deadline/cancel/unknown/keeper",
                         "CODEX_API_KEY stripping", "leak canaries", "fake re-login", "fake docker label sweep"],
               "limitations": ["Fake credentials and loopback fakes only: no real token authority, ChatGPT backend "
                               "or model was contacted; live behaviour remains unqualified.",
                               "Process-tree tracking cannot see a daemon that escapes between samples, closes "
                               "inherited descriptors and leaves the run directory.",
                               "Harbor containers were simulated with a fake docker CLI."]}
    observations = out / "observations.jsonl"
    if observations.exists():
        summary["observations"] = [json.loads(line) for line in observations.read_text().splitlines() if line]
    (out / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    return 0 if result.wasSuccessful() else 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--results-dir")
    parser.add_argument("--codex-bin")
    parser.add_argument("--codex-sha256")
    parser.add_argument("--pattern", default="test_*.py")
    parser.add_argument("--ports", type=int, default=32)
    parser.add_argument("--inner", action="store_true")
    parser.add_argument("--out")
    args = parser.parse_args()
    if args.inner:
        return inner(args)
    if not args.results_dir:
        parser.error("--results-dir is required")
    return outer(args)


if __name__ == "__main__":
    sys.exit(main())
