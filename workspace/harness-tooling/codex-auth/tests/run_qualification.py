"""Run the codex-auth fake qualification suite inside a network/file guard.

usage: python3 -I -B tests/run_qualification.py --results-dir DIR
           [--codex-bin PATH --codex-sha256 HEX] [--pattern 'test_*.py']
           [--unguarded-tool-sandbox [--docker-bin PATH --docker-host URL --docker-image IMAGE]]

Outer process: creates DIR/<UTC-stamp>/, reserves loopback ports and, on macOS,
re-executes itself under `sandbox-exec` with a profile that
  * denies all outbound network except the reserved loopback ports and the
    closed blocker port 9 (system proxies, DNS and real endpoints are unreachable);
  * denies any file access below the user's real ~/.gptgrep and ~/.codex.
Inner process: runs unittest discovery and writes logs plus summary.json.
Integration tests that execute a real Codex binary refuse to run unguarded.

--unguarded-tool-sandbox (explicit, official binary only) adds a second pass
for unguarded_test_*.py. Codex's own tool sandbox cannot be applied inside the
seatbelt guard, so that pass runs without it; every endpoint is a loopback fake,
the refresh/revoke blocker is port 0, and HTTP(S)/ALL proxies point at a
loopback trap whose connection count must stay 0. With --docker-bin,
--docker-host and --docker-image (a local image with python3, never pulled) the
same pass runs the minimal non-live container checks: `--network none`, fake
credential, a label unique to the test.
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
    code = proc.returncode
    unguarded = None
    if args.unguarded_tool_sandbox:
        if not args.codex_bin:
            print(json.dumps({"error": "unguarded_tool_sandbox_requires_official_codex_bin"}))
            return 2
        # Separate fake world, no seatbelt: see the module docstring.
        env_unguarded = dict(env, GPTGREP_AUTH_TEST_SANDBOX="proxy-trap",
                             GPTGREP_AUTH_TEST_ROOT=str(out / "tmp" / "root-unguarded"))
        if args.docker_image:
            if not (args.docker_bin and args.docker_host):
                print(json.dumps({"error": "docker_image_requires_docker_bin_and_host"}))
                return 2
            env_unguarded.update(GPTGREP_AUTH_TEST_DOCKER_BIN=args.docker_bin,
                                 GPTGREP_AUTH_TEST_DOCKER_HOST=args.docker_host,
                                 GPTGREP_AUTH_TEST_DOCKER_IMAGE=args.docker_image)
            meta["docker"] = {"bin": args.docker_bin, "image": args.docker_image, "network": "none",
                              "pull": "never"}
        (out / "tmp" / "root-unguarded").mkdir(mode=0o700)
        second = subprocess.run([sys.executable, "-I", "-B", str(Path(__file__).resolve()), "--inner", "--out",
                                 str(out), "--pattern", "unguarded_test_*.py", "--summary-name",
                                 "summary-unguarded.json", "--log-name", "unguarded.log"],
                                env=env_unguarded, cwd=str(out / "tmp"))
        code = code or second.returncode
        path = out / "summary-unguarded.json"
        unguarded = json.loads(path.read_text()) if path.exists() else {"error": "no_summary"}
    summary_path = out / "summary.json"
    if summary_path.exists():
        summary = json.loads(summary_path.read_text())
        summary["runner"] = meta
        if unguarded is not None:
            summary["unguarded_tool_sandbox"] = {"guard": "proxy-trap", "counts": unguarded.get("counts"),
                                                 "tests": unguarded.get("tests")}
        summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"summary": str(summary_path), "counts": summary["counts"],
                          "unguarded_counts": (unguarded or {}).get("counts")}))
    return code


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
    with open(out / "logs" / args.log_name, "w") as log:
        runner = unittest.TextTestRunner(stream=log, verbosity=2, resultclass=Collector)
        result = runner.run(suite)
    counts = {"total": result.testsRun, "passed": sum(1 for r in result.records if r["outcome"] == "passed"),
              "failed": len(result.failures), "errors": len(result.errors), "skipped": len(result.skipped)}
    summary = {"schema": "gptgrep.codex-auth.qualification.v1", "started_at": started,
               "finished_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
               "counts": counts, "tests": result.records, "codex_version": codex_version(),
               "guard": os.environ.get("GPTGREP_AUTH_TEST_SANDBOX", "none"), "pattern": args.pattern,
               "fake_credentials_only": True,
               "scope": ["lease semantics and binding", "layout/env allowlist and auth-link lifetime",
                         "owner refresh and failure classes (fake and official binary)",
                         "consumer refresh blocker (port 0) and re-probe", "guarded-reload adoption",
                         "401 fail-closed and the exit-77 auth contract", "shared/exclusive concurrency",
                         "lifecycle deadline/cancel/unknown, lease guardian (signals, exceptions, killed supervisor)",
                         "CODEX_API_KEY stripping", "leak canaries", "staged re-login (fake and official binary)",
                         "fake docker label gate",
                         "extra_denied_paths and role deny/read paths (config, generated profile, receipts; "
                         "enforcement via codex sandbox in the unguarded pass)",
                         "consumer argv guard against legacy-sandbox and permission-source arguments and "
                         "project roots inside denied paths; the run's own directories as roots below a deny "
                         "that contains runs_root (production Judge shape)",
                         "no lease descriptor inherited by consumers or their tool commands",
                         "hard links and symlinks to the credential in run directories (preflight link count, "
                         "post-run and guardian scans)", "account-probe protected outputs",
                         "exit 77 counts only with the auth-failure record",
                         "real-binary tests refuse without a verified guard",
                         "concurrent first commands bind the auth source once"],
               "limitations": ["Fake credentials and loopback fakes only: no real token authority, ChatGPT backend "
                               "or model was contacted; live behaviour remains unqualified.",
                               "Process-tree tracking cannot see a daemon that escapes between samples, closes "
                               "inherited descriptors and leaves the run directory.",
                               "Harbor containers were simulated with a fake docker CLI.",
                               "No GPTgrep product, OCR, Document or Judge call chain ran under the supervisor."]}
    observations = out / "observations.jsonl"
    if observations.exists():
        summary["observations"] = [json.loads(line) for line in observations.read_text().splitlines() if line]
    (out / args.summary_name).write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    return 0 if result.wasSuccessful() else 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--results-dir")
    parser.add_argument("--codex-bin")
    parser.add_argument("--codex-sha256")
    parser.add_argument("--pattern", default="test_*.py")
    parser.add_argument("--ports", type=int, default=32)
    parser.add_argument("--unguarded-tool-sandbox", action="store_true")
    parser.add_argument("--docker-bin")
    parser.add_argument("--docker-host")
    parser.add_argument("--docker-image")
    parser.add_argument("--inner", action="store_true")
    parser.add_argument("--out")
    parser.add_argument("--summary-name", default="summary.json")
    parser.add_argument("--log-name", default="unittest.log")
    args = parser.parse_args()
    if args.inner:
        return inner(args)
    if not args.results_dir:
        parser.error("--results-dir is required")
    return outer(args)


if __name__ == "__main__":
    sys.exit(main())
