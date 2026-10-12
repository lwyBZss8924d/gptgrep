"""Shared fakes for the codex-auth qualification suite.

Everything here is synthetic: unsigned fake JWTs, loopback fake token
authority / ChatGPT backend / trap listeners, fake credential homes under the
suite's own temporary root. Nothing reads or references a real credential.
"""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import pwd
import secrets
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = Path(__file__).resolve().parent
TOOL_DIR = HERE.parent
FIXTURES = HERE / "fixtures"
SUPERVISOR = TOOL_DIR / "codex_auth_supervisor.py"
if str(TOOL_DIR) not in sys.path:
    sys.path.insert(0, str(TOOL_DIR))

import codex_auth_supervisor as sup  # noqa: E402

PY = sys.executable
CODEX_BIN = os.environ.get("GPTGREP_AUTH_TEST_CODEX_BIN") or None
CODEX_SHA256 = os.environ.get("GPTGREP_AUTH_TEST_CODEX_SHA256") or None
SANDBOX = os.environ.get("GPTGREP_AUTH_TEST_SANDBOX", "none")
_RESERVED = [int(p) for p in os.environ.get("GPTGREP_AUTH_TEST_PORTS", "").split(",") if p.strip()]
_PORT_LOCK = threading.Lock()
_PORT_INDEX = [0]


def test_root():
    root = Path(os.environ.get("GPTGREP_AUTH_TEST_ROOT") or tempfile.gettempdir())
    root = Path(os.path.realpath(root))
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    return root


def real_credential_roots():
    home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    return [home / ".gptgrep", home / ".codex"]


def assert_fake_path(path):
    """Every fake credential path must be outside the real credential roots."""
    resolved = Path(os.path.realpath(path))
    for root in real_credential_roots():
        if resolved == root or root in resolved.parents:
            raise AssertionError("fake path resolves under a real credential root")
    return resolved


def next_port():
    """Ports reserved by the runner (and allowed by its seatbelt profile)."""
    if not _RESERVED:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            return probe.getsockname()[1]
    with _PORT_LOCK:
        port = _RESERVED[_PORT_INDEX[0] % len(_RESERVED)]
        _PORT_INDEX[0] += 1
        return port


def b64url(value):
    raw = value if isinstance(value, bytes) else json.dumps(value, separators=(",", ":")).encode()
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def fake_jwt(label, exp, account="acct-fake-0001"):
    """Unsigned (alg=none) fake JWT; the payload carries a test-only label."""
    payload = {"exp": int(exp), "fake_label": label, "email": "fake-user@example.invalid",
               "https://api.openai.com/auth": {"chatgpt_account_id": account, "chatgpt_plan_type": "plus",
                                               "chatgpt_user_id": "user-fake-0001"}}
    return ".".join([b64url({"alg": "none", "typ": "JWT"}), b64url(payload), b64url(b"FAKE-UNSIGNED")])


def jwt_label(token):
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        return json.loads(base64.urlsafe_b64decode(payload)).get("fake_label")
    except (IndexError, ValueError):
        return None


class FakeCredential:
    """A fake canonical ChatGPT auth home. All strings are synthetic canaries."""

    def __init__(self, home, nonce):
        self.home = assert_fake_path(home)
        self.home.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.home, 0o700)
        self.path = self.home / "auth.json"
        self.nonce = nonce
        self.issued = []

    def token_set(self, generation, exp):
        tokens = {"id_token": fake_jwt("FAKE-ID-%s-%d" % (self.nonce, generation), exp),
                  "access_token": fake_jwt("FAKE-AT-%s-%d" % (self.nonce, generation), exp),
                  "refresh_token": "FAKE-RT-%s-%d" % (self.nonce, generation),
                  "account_id": "acct-fake-0001"}
        self.issued.append(tokens)
        return tokens

    def document(self, tokens):
        return {"auth_mode": "chatgpt", "OPENAI_API_KEY": None, "tokens": tokens,
                "last_refresh": "2026-10-01T00:00:00Z"}

    def write(self, generation, exp, in_place=True):
        data = json.dumps(self.document(self.token_set(generation, exp)), indent=2).encode()
        if in_place and self.path.exists():
            fd = os.open(self.path, os.O_WRONLY | os.O_TRUNC | os.O_NOFOLLOW)
        else:
            fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        os.chmod(self.path, 0o600)
        return self.issued[-1]

    def age(self, seconds):
        ts = time.time() - seconds
        os.utime(self.path, (ts, ts))

    def meta(self):
        info = os.lstat(self.path)
        return {"ino": info.st_ino, "size": info.st_size, "mtime_ns": info.st_mtime_ns,
                "mode": stat.S_IMODE(info.st_mode), "regular": stat.S_ISREG(info.st_mode)}

    def canaries(self):
        values = set()
        for tokens in self.issued:
            values.update(v for k, v in tokens.items() if k != "account_id")
        return values


class _Recorder:
    def __init__(self):
        self.lock = threading.Lock()
        self.events = []

    def add(self, **event):
        event["t"] = time.time()
        with self.lock:
            self.events.append(event)

    def where(self, **match):
        with self.lock:
            return [e for e in self.events if all(e.get(k) == v for k, v in match.items())]


class FakeAuthority:
    """Loopback OAuth token endpoint implementing the refresh grant."""

    def __init__(self, credential, mode="ok", lifetime=3600):
        self.credential = credential
        self.mode = mode
        self.lifetime = lifetime
        self.recorder = _Recorder()
        self.generation = 100
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length)
                try:
                    grant = json.loads(body)
                except ValueError:
                    grant = {}
                outer.recorder.add(kind="token", path=self.path, grant_type=grant.get("grant_type"),
                                   refresh_token=grant.get("refresh_token"))
                if outer.mode == "reused":
                    data = json.dumps({"error": {"code": "refresh_token_reused", "message": "fake"}}).encode()
                    self.send_response(401)
                else:
                    outer.generation += 1
                    tokens = outer.credential.token_set(outer.generation, time.time() + outer.lifetime)
                    data = json.dumps({k: tokens[k] for k in ("id_token", "access_token", "refresh_token")}).encode()
                    self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(("127.0.0.1", next_port()), Handler)
        self.url = "http://127.0.0.1:%d/oauth/token" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def requests(self):
        return self.recorder.where(kind="token")

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class FakeBackend:
    """Loopback ChatGPT backend (chatgpt_base_url) and model provider (openai_base_url).

    Records the fake label of every bearer it sees; never real tokens.
    """

    def __init__(self, mode="ok"):
        self.mode = mode
        self.recorder = _Recorder()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def _reply(self, status, payload=None):
                data = b"" if payload is None else json.dumps(payload).encode()
                self.send_response(status)
                if payload is not None:
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _record(self, method):
                auth = self.headers.get("Authorization") or ""
                label = jwt_label(auth[7:]) if auth.lower().startswith("bearer ") else None
                outer.recorder.add(kind="backend", method=method, path=self.path.split("?")[0], bearer=label,
                                   account=self.headers.get("chatgpt-account-id"))

            def do_GET(self):
                self._record("GET")
                if outer.mode == "unauthorized":
                    return self._reply(401, {"error": {"code": "token_expired", "message": "fake 401"}})
                if self.path.split("?")[0].endswith("/accounts/check"):
                    account = self.headers.get("chatgpt-account-id") or "acct-fake-0001"
                    return self._reply(200, {"accounts": [{"id": account,
                                                           "workspace_backend_origin": "https://fake-backend.invalid",
                                                           "account_routing_override": "NO_CONSTRAINT"}]})
                return self._reply(404)

            def do_POST(self):
                self._record("POST")
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                if outer.mode == "unauthorized":
                    return self._reply(401, {"error": {"code": "token_expired", "message": "fake 401"}})
                return self._reply(400, {"error": {"message": "fake backend: model calls are not served",
                                                   "type": "invalid_request_error"}})

        self.server = ThreadingHTTPServer(("127.0.0.1", next_port()), Handler)
        port = self.server.server_address[1]
        self.chatgpt_base_url = "http://127.0.0.1:%d/backend-api" % port
        self.openai_base_url = "http://127.0.0.1:%d/backend-api/codex" % port
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def requests(self, **match):
        return self.recorder.where(kind="backend", **match)

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class Trap:
    """A blocker stand-in that counts connection attempts and closes them unread."""

    def __init__(self):
        self.count = 0
        self.lock = threading.Lock()
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", next_port()))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        self.url = "http://127.0.0.1:%d/oauth/token" % self.port
        self.running = True
        threading.Thread(target=self._loop, daemon=True).start()

    def _loop(self):
        while self.running:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            with self.lock:
                self.count += 1
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            conn.close()

    def stop(self):
        self.running = False
        try:
            self.sock.close()
        except OSError:
            pass


def write_config(path, values):
    path = Path(path)
    data = {"schema": sup.CONFIG_SCHEMA}
    data.update(values)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as handle:
        json.dump(data, handle, indent=2, sort_keys=True)
    os.chmod(path, 0o600)
    return path


class Env:
    """One isolated fake world: credential home, lease location, runs root, config."""

    def __init__(self, name, **config):
        self.nonce = secrets.token_hex(4)
        self.root = assert_fake_path(test_root() / ("%s-%s" % (name, self.nonce)))
        self.root.mkdir(mode=0o700)
        self.credential = FakeCredential(self.root / "accounts" / "fake", self.nonce)
        (self.root / "locks").mkdir(mode=0o700)
        self.home = self.root / "parent-home"
        self.home.mkdir(mode=0o700)
        self.lease_path = self.root / "locks" / "fake.lease"
        self.runs_root = self.root / "runs"
        values = {"auth_source_id": "fake", "canonical_auth_home": str(self.credential.home),
                  "lease_path": str(self.lease_path), "runs_root": str(self.runs_root),
                  "codex_bin": "/usr/bin/false", "lease_wait_seconds": 0, "term_grace_seconds": 2,
                  "sweep_interval_seconds": 0.2, "sweep_settle_seconds": 1, "keeper_poll_seconds": 0.5,
                  "roles": {"dev": {}, "test": {},
                            "harbor": {"passthrough_env": ["DOCKER_HOST", "DOCKER_CONFIG"]},
                            "judge": {"passthrough_env": ["OPENROUTER_API_KEY"], "allow_openrouter": True}}}
        values.update(config)
        self.values = values
        self.config_path = write_config(self.root / "config.json", values)
        for path in (self.credential.home, self.lease_path, self.runs_root, self.config_path):
            assert_fake_path(path)

    def reconfigure(self, **changes):
        self.values.update(changes)
        self.config_path = write_config(self.config_path, self.values)
        return self.config_path

    def cfg(self):
        return sup.Config.load(self.config_path)

    def parent_env(self, **extra):
        env = {"PATH": "/usr/bin:/bin", "HOME": str(self.home), "LANG": "en_US.UTF-8",
               "TMPDIR": str(test_root())}
        env.update(extra)
        return env

    def run_dir(self, role, run_id):
        return self.runs_root / role / run_id

    def supervisor_argv(self, *args):
        return [PY, "-I", "-B", str(SUPERVISOR), "--config", str(self.config_path), *args]

    def supervisor(self, *args, env=None, timeout=180):
        proc = subprocess.run(self.supervisor_argv(*args), capture_output=True, text=True, timeout=timeout,
                              env=env or self.parent_env(), cwd=str(self.root))
        return proc.returncode, last_json(proc.stdout), proc

    def popen(self, *args, env=None):
        return subprocess.Popen(self.supervisor_argv(*args), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, env=env or self.parent_env(), cwd=str(self.root))

    def ledger(self):
        path = Path(str(self.lease_path) + ".ledger.jsonl")
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

    def quarantine_marker(self):
        return Path(str(self.lease_path) + ".quarantine")

    def sidecar(self, role, run_id):
        return Path(str(self.lease_path) + ".holders") / ("%s--%s.json" % (role, run_id))


def observe(name, **values):
    """Append a non-secret observation (counts, booleans, states) for the run summary."""
    target = os.environ.get("GPTGREP_AUTH_TEST_OBSERVATIONS")
    if not target:
        return
    with open(target, "a") as handle:
        handle.write(json.dumps(dict(values, name=name, at=time.time()), sort_keys=True, default=str) + "\n")


def last_json(text):
    for line in reversed((text or "").splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def wait_for(predicate, timeout=20, interval=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


def script(path, body, mode=0o700):
    """Write an executable test fixture script with this interpreter."""
    path = Path(path)
    path.write_text("#!%s -IB\n%s" % (PY, body))
    os.chmod(path, mode)
    return path


def sha256_file(path):
    return sup.file_sha256(path)


def scan_for_leaks(roots, needles, skip=(), counter=None):
    """Return [(path, needle)] for any needle found in regular files under roots.
    `counter` (a list) receives the number of files scanned, so callers can
    prove the scan was not vacuous.

    Symlinks are never followed (a run's auth.json link points at the fake
    canonical credential, which legitimately contains the fake tokens)."""
    hits = []
    skip = [Path(s) for s in skip]
    needles = [n.encode() if isinstance(n, str) else n for n in needles if n]
    for root in roots:
        root = Path(root)
        if not root.exists():
            continue
        candidates = [root] if root.is_file() else []
        if root.is_dir():
            for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
                for name in filenames:
                    candidates.append(Path(dirpath) / name)
        for path in candidates:
            if any(path == s or s in path.parents for s in skip):
                continue
            try:
                if not stat.S_ISREG(os.lstat(path).st_mode):
                    continue
                data = path.read_bytes()
            except OSError:
                continue
            if counter is not None:
                counter.append(str(path))
            for needle in needles:
                if needle in data:
                    hits.append((str(path), needle.decode(errors="replace")[:24]))
    return hits


def lease_side_files(env):
    base = str(env.lease_path)
    return [Path(base + ".ledger.jsonl"), Path(base + ".holders"), Path(base + ".quarantine")] + \
        list(Path(env.lease_path.parent).glob(env.lease_path.name + ".quarantine.cleared-*"))
