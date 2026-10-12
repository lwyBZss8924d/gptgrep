#!/usr/bin/env python3
"""Shared Codex ChatGPT credential lease, refresh owner and per-run isolation.

One persistent ChatGPT login cache (the canonical auth home) serves every
managed consumer. This supervisor:

* serializes managed access with one kernel lease per auth source: a BSD
  ``flock`` on a lock file outside any checkout, exclusive or (when enabled)
  shared, held for the whole process lifetime including descendants;
* gives each run its own ``CODEX_HOME`` whose ``auth.json`` is an absolute
  symlink to the canonical file, a generated pinned ``config.toml`` and an
  allowlisted environment that points the token endpoint at a closed
  loopback port, so consumers can never spend the shared refresh token;
* lets only the refresh owner (exclusive lease) refresh the credential through
  the official ``codex app-server`` and judges the result from file metadata;
* records non-secret manifests, receipts and an owner ledger.

The supervisor never opens, reads, hashes or decodes credential contents.
It is Python 3.12+ standard library only. Run ``--help`` for the interface and
README.md for the operational runbook.
"""
from __future__ import annotations

import argparse
import ctypes
import datetime as _dt
import errno
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import urllib.parse

SUPERVISOR_VERSION = "1.0.0"
CONFIG_SCHEMA = "gptgrep.codex-auth.config.v1"
RUN_SCHEMA = "gptgrep.codex-auth.run.v1"
LEDGER_SCHEMA = "gptgrep.codex-auth.ledger.v1"

EXIT_PASS = 0
EXIT_USAGE = 64
EXIT_RUN_EXISTS = 73
EXIT_AUTH_BUSY = 75
EXIT_UNKNOWN = 76
EXIT_BLOCKED_AUTH = 77
EXIT_QUARANTINED = 78
EXIT_INTERNAL = 70
EXIT_TIMEOUT = 124
EXIT_CANCELLED = 130
STATE_EXIT = {"PASS": EXIT_PASS, "AUTH_BUSY": EXIT_AUTH_BUSY, "UNKNOWN": EXIT_UNKNOWN,
              "BLOCKED_AUTH": EXIT_BLOCKED_AUTH, "QUARANTINED": EXIT_QUARANTINED,
              "TIMEOUT": EXIT_TIMEOUT, "CANCELLED": EXIT_CANCELLED}

DEFAULT_BLOCKER_URL = "http://127.0.0.1:9/oauth/token"
# reqwest/hyper-util read the macOS system HTTP(S) proxy but ignore its
# ExceptionsList; NO_PROXY is honoured. Without this a request to the loopback
# blocker can be forwarded, body included, through a local or remote proxy.
LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1"
DEFAULT_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
STRIPPED_ALWAYS = ("OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN")
PLACEHOLDERS = ("codex_home", "state_dir", "log_dir", "work_dir", "home", "tmp", "results", "run_dir")
NAME_RE = re.compile(r"[a-z][a-z0-9_-]{0,31}")
RUN_ID_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,95}")
ENV_NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,127}")
LABEL_RE = re.compile(r"[A-Za-z0-9_.-]{1,128}(=[A-Za-z0-9_.:/@+-]{1,256})?")
HEX64_RE = re.compile(r"[0-9a-f]{64}")
FORBIDDEN_PASSTHROUGH = set(STRIPPED_ALWAYS) | {
    "PATH", "HOME", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "NO_PROXY", "no_proxy",
    "RUST_LOG", "RUST_BACKTRACE", "SSLKEYLOGFILE", "BASH_ENV", "ENV", "PYTHONSTARTUP",
    "PYTHONPATH", "DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"}
FORBIDDEN_PASSTHROUGH_PREFIXES = ("CODEX_", "OP_SESSION", "XDG_", "GPTGREP_AUTH_")

# Generated for every per-run and owner CODEX_HOME. Keys verified against the
# 0.162.1 config schema (codex-rs/core/config.schema.json). The canonical
# home's config.toml, skills and state are never copied.
PINNED_CODEX_CONFIG = {
    "cli_auth_credentials_store": "file",
    "forced_login_method": "chatgpt",
    "check_for_update_on_startup": False,
    "notify": [],
    "features": {"use_agent_identity": False, "secret_auth_storage": False, "memories": False,
                 "hooks": False, "plugins": False, "apps": False},
    "memories": {"generate_memories": False, "use_memories": False},
    "cloud": {"skills": {"enabled": False}},
    "analytics": {"enabled": False},
    "feedback": {"enabled": False},
    "history": {"persistence": "none"},
}
EXTRA_CODEX_CONFIG_KEYS = frozenset({
    "chatgpt_base_url", "openai_base_url", "model", "model_reasoning_effort",
    "model_reasoning_summary", "model_verbosity", "service_tier", "project_doc_max_bytes",
    "web_search", "approval_policy"})

REFRESH_FAILURE_PATTERNS = (
    ("REFRESH_TOKEN_EXPIRED", "refresh token has expired"),
    ("REFRESH_TOKEN_REUSED", "refresh token was already used"),
    ("REFRESH_TOKEN_REVOKED", "refresh token was revoked"),
    ("ACCOUNT_MISMATCH", "signed in to another account"),
    ("REFRESH_UNKNOWN", "access token could not be refreshed"),
    ("TRANSPORT", "error sending request"),
)
_JWT_RE = re.compile(r"[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{2,}")
_BEARER_RE = re.compile(r"(?i)bearer\s+\S+")
_LONG_TOKEN_RE = re.compile(r"[A-Za-z0-9_+/=-]{40,}")


class SupervisorError(Exception):
    """A refusal with a stable reason code; never carries secret values."""

    def __init__(self, reason, state="CONFIG_ERROR", exit_code=EXIT_USAGE, detail=None):
        super().__init__(reason)
        self.reason = reason
        self.state = state
        self.exit_code = exit_code
        self.detail = detail


def utc_iso(ts=None):
    ts = time.time() if ts is None else ts
    return _dt.datetime.fromtimestamp(ts, _dt.timezone.utc).isoformat(timespec="milliseconds")


def utc_stamp():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")


def redact(text, limit=240):
    """Second-line scrubber for free text recorded by the supervisor."""
    if text is None:
        return None
    text = str(text)
    text = _BEARER_RE.sub("Bearer [REDACTED]", text)
    text = _JWT_RE.sub("[REDACTED_JWT]", text)
    text = _LONG_TOKEN_RE.sub("[REDACTED]", text)
    return text[:limit]


# --------------------------------------------------------------------------- files

def ensure_private_dir(path, create=False, create_parents=False):
    """Require a real directory owned by this uid with no group/other bits."""
    path = Path(path)
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        if not create:
            raise SupervisorError("directory_missing", detail=str(path))
        if create_parents:
            path.parent.mkdir(parents=True, exist_ok=True)
        try:
            os.mkdir(path, 0o700)
        except FileExistsError:
            pass
        info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise SupervisorError("directory_not_physical", detail=str(path))
    if info.st_uid != os.getuid():
        raise SupervisorError("directory_foreign_owner", detail=str(path))
    if info.st_mode & 0o077:
        raise SupervisorError("directory_not_private_0700", detail=str(path))
    return info


def write_new_private(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(data if isinstance(data, bytes) else data.encode())


def write_json_atomic(path, value):
    path = Path(path)
    temp = path.with_name("." + path.name + ".tmp-" + str(os.getpid()) + "-" + str(threading.get_ident()))
    write_new_private(temp, json.dumps(value, indent=2, sort_keys=True, default=str) + "\n")
    os.replace(temp, path)


def append_jsonl(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(fd, "a") as handle:
        handle.write(json.dumps(value, sort_keys=True, separators=(",", ":"), default=str) + "\n")


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def is_within(child, parent):
    try:
        Path(child).relative_to(parent)
        return True
    except ValueError:
        return False


# --------------------------------------------------------------------------- config

def _abs(value, key):
    if not isinstance(value, str) or not value.startswith("/") or "\x00" in value or "\n" in value:
        raise SupervisorError("config_path_not_absolute", detail=key)
    if os.path.normpath(value) != value or ".." in Path(value).parts:
        raise SupervisorError("config_path_not_normalized", detail=key)
    return Path(value)


def _number(value, key, low, high, kind=float):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not low <= value <= high:
        raise SupervisorError("config_number_out_of_range", detail=key)
    return kind(value)


def _loopback_url(value, key):
    parsed = urllib.parse.urlsplit(value) if isinstance(value, str) else None
    if parsed is None or parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise SupervisorError("config_url_invalid", detail=key)
    host = parsed.hostname
    if host != "localhost":
        try:
            if not ipaddress.ip_address(host).is_loopback:
                raise SupervisorError("config_url_not_loopback", detail=key)
        except ValueError:
            raise SupervisorError("config_url_not_loopback", detail=key) from None
    return value


def _env_names(values, key, allow_openrouter):
    if not isinstance(values, list) or len(values) > 64:
        raise SupervisorError("config_env_list_invalid", detail=key)
    names = []
    for name in values:
        if not isinstance(name, str) or not ENV_NAME_RE.fullmatch(name):
            raise SupervisorError("config_env_name_invalid", detail=key)
        if name in FORBIDDEN_PASSTHROUGH or name.startswith(FORBIDDEN_PASSTHROUGH_PREFIXES):
            raise SupervisorError("config_env_name_forbidden", detail=key + ":" + name)
        if name == "OPENROUTER_API_KEY" and not allow_openrouter:
            raise SupervisorError("config_openrouter_not_allowed_for_role", detail=key)
        names.append(name)
    return names


def _codex_config_extra(value, key):
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise SupervisorError("config_codex_config_invalid", detail=key)
    result = {}
    for name, item in value.items():
        if name not in EXTRA_CODEX_CONFIG_KEYS:
            raise SupervisorError("config_codex_config_key_not_allowed", detail=key + ":" + str(name))
        if isinstance(item, bool) or not isinstance(item, (str, int)):
            raise SupervisorError("config_codex_config_value_invalid", detail=key + ":" + name)
        if isinstance(item, str) and any(c in item for c in ("\n", "\r", "\x00")):
            raise SupervisorError("config_codex_config_value_invalid", detail=key + ":" + name)
        result[name] = item
    return result


class Config:
    """Validated supervisor configuration (JSON, schema gptgrep.codex-auth.config.v1)."""

    def __init__(self, raw, source_path):
        if not isinstance(raw, dict) or raw.get("schema") != CONFIG_SCHEMA:
            raise SupervisorError("config_schema")
        self.source_path = source_path
        self.raw = raw
        self.auth_source_id = raw.get("auth_source_id")
        if not isinstance(self.auth_source_id, str) or not NAME_RE.fullmatch(self.auth_source_id):
            raise SupervisorError("config_auth_source_id")
        # Compare the effective user, not a "~" string that may expand for another account.
        if os.geteuid() != os.getuid():
            raise SupervisorError("setuid_context_refused")
        self.expected_user = raw.get("expected_user")
        if self.expected_user is not None and pwd.getpwuid(os.geteuid()).pw_name != self.expected_user:
            raise SupervisorError("unexpected_effective_user")
        self.canonical_auth_home = _abs(raw.get("canonical_auth_home"), "canonical_auth_home")
        self.canonical_auth_file = self.canonical_auth_home / "auth.json"
        self.lease_path = _abs(raw.get("lease_path"), "lease_path")
        self.runs_root = _abs(raw.get("runs_root"), "runs_root")
        self.codex_bin = _abs(raw.get("codex_bin"), "codex_bin")
        self.codex_bin_sha256 = raw.get("codex_bin_sha256")
        if self.codex_bin_sha256 is not None and not (isinstance(self.codex_bin_sha256, str)
                                                      and HEX64_RE.fullmatch(self.codex_bin_sha256)):
            raise SupervisorError("config_codex_bin_sha256")
        self.binary_pins = []
        for pin in raw.get("binary_pins", []):
            if not isinstance(pin, dict) or set(pin) != {"name", "path", "sha256"} or \
                    not NAME_RE.fullmatch(str(pin["name"])) or not HEX64_RE.fullmatch(str(pin["sha256"])):
                raise SupervisorError("config_binary_pin")
            self.binary_pins.append({"name": pin["name"], "path": _abs(pin["path"], "binary_pins"),
                                     "sha256": pin["sha256"]})
        self.verify_binary_pins = raw.get("verify_binary_pins", True) is True
        self.blocker_url = _loopback_url(raw.get("blocker_url", DEFAULT_BLOCKER_URL), "blocker_url")
        self.blocker_must_be_closed = raw.get("blocker_must_be_closed", True) is True
        self.allow_shared_consumers = raw.get("allow_shared_consumers", False) is True
        self.lease_wait_seconds = _number(raw.get("lease_wait_seconds", 30), "lease_wait_seconds", 0, 86400)
        self.default_deadline_seconds = _number(raw.get("default_deadline_seconds", 21600),
                                                "default_deadline_seconds", 1, 7 * 86400)
        self.term_grace_seconds = _number(raw.get("term_grace_seconds", 10), "term_grace_seconds", 0, 3600)
        self.sweep_interval_seconds = _number(raw.get("sweep_interval_seconds", 0.5),
                                              "sweep_interval_seconds", 0.05, 60)
        self.keeper_poll_seconds = _number(raw.get("keeper_poll_seconds", 2), "keeper_poll_seconds", 0.1, 600)
        self.sweep_settle_seconds = _number(raw.get("sweep_settle_seconds", 2), "sweep_settle_seconds", 0, 120)
        self.path_env = raw.get("path_env", DEFAULT_PATH)
        if not isinstance(self.path_env, str) or not all(p.startswith("/") for p in self.path_env.split(":")):
            raise SupervisorError("config_path_env")
        self.locale = raw.get("locale", "en_US.UTF-8")
        if not isinstance(self.locale, str) or not re.fullmatch(r"[A-Za-z0-9_.@-]{1,40}", self.locale):
            raise SupervisorError("config_locale")
        self.ps_bin = _abs(raw.get("ps_bin", "/bin/ps"), "ps_bin")
        self.lsof_bin = _abs(raw.get("lsof_bin", "/usr/sbin/lsof"), "lsof_bin")
        policy = raw.get("refresh_policy", {})
        if not isinstance(policy, dict):
            raise SupervisorError("config_refresh_policy")
        self.refresh_max_age_seconds = _number(policy.get("max_age_seconds", 43200),
                                               "refresh_policy.max_age_seconds", 60, 30 * 86400)
        self.pre_run_refresh = policy.get("pre_run", "never")
        if self.pre_run_refresh not in ("never", "if-stale", "always"):
            raise SupervisorError("config_refresh_policy_pre_run")
        owner = raw.get("owner", {})
        if not isinstance(owner, dict):
            raise SupervisorError("config_owner")
        url = owner.get("refresh_token_url_override")
        self.owner_refresh_token_url = None if url is None else _loopback_url(url, "owner.refresh_token_url_override")
        self.owner_rpc_timeout_seconds = _number(owner.get("rpc_timeout_seconds", 120),
                                                 "owner.rpc_timeout_seconds", 1, 3600)
        self.owner_exit_wait_seconds = _number(owner.get("exit_wait_seconds", 30), "owner.exit_wait_seconds", 0, 3600)
        self.owner_term_grace_seconds = _number(owner.get("term_grace_seconds", 120),
                                                "owner.term_grace_seconds", 0, 3600)
        self.owner_passthrough_env = _env_names(owner.get("passthrough_env", []), "owner.passthrough_env", False)
        self.codex_config = _codex_config_extra(raw.get("codex_config"), "codex_config")
        roles = raw.get("roles")
        if not isinstance(roles, dict) or not roles:
            raise SupervisorError("config_roles")
        self.roles = {}
        for name, role in roles.items():
            if not NAME_RE.fullmatch(str(name)) or not isinstance(role, dict):
                raise SupervisorError("config_role", detail=str(name))
            allow_openrouter = role.get("allow_openrouter", False) is True
            deadline = role.get("deadline_seconds")
            role_path = role.get("path_env")
            if role_path is not None and (not isinstance(role_path, str) or
                                          not all(p.startswith("/") for p in role_path.split(":"))):
                raise SupervisorError("config_role_path_env", detail=str(name))
            self.roles[name] = {
                "path_env": role_path,
                "passthrough_env": _env_names(role.get("passthrough_env", []), "roles." + name, allow_openrouter),
                "allow_openrouter": allow_openrouter,
                "deadline_seconds": None if deadline is None else _number(
                    deadline, "roles." + name + ".deadline_seconds", 1, 7 * 86400),
                "codex_config": _codex_config_extra(role.get("codex_config"), "roles." + name + ".codex_config"),
            }
        docker = raw.get("docker", {})
        if not isinstance(docker, dict):
            raise SupervisorError("config_docker")
        self.docker_bin = None if docker.get("bin") is None else _abs(docker.get("bin"), "docker.bin")
        docker_env = docker.get("env", {})
        if not isinstance(docker_env, dict) or not all(
                isinstance(k, str) and k.startswith(("DOCKER_", "BUILDX_")) and ENV_NAME_RE.fullmatch(k)
                and isinstance(v, str) and "\n" not in v for k, v in docker_env.items()):
            raise SupervisorError("config_docker_env")
        self.docker_env = dict(docker_env)
        self.docker_wait_seconds = _number(docker.get("wait_seconds", 120), "docker.wait_seconds", 0, 3600)
        self.docker_poll_seconds = _number(docker.get("poll_seconds", 2), "docker.poll_seconds", 0.1, 60)
        self.holders_dir = Path(str(self.lease_path) + ".holders")
        self.quarantine_path = Path(str(self.lease_path) + ".quarantine")
        self.ledger_path = Path(str(self.lease_path) + ".ledger.jsonl")
        if is_within(self.runs_root, self.canonical_auth_home) or is_within(self.canonical_auth_home, self.runs_root):
            raise SupervisorError("config_runs_root_overlaps_auth_home")
        if is_within(self.lease_path, self.canonical_auth_home) or is_within(self.lease_path, self.runs_root):
            raise SupervisorError("config_lease_inside_auth_home_or_runs_root")

    @classmethod
    def load(cls, path):
        path = Path(path)
        if not path.is_absolute():
            path = Path.cwd() / path
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            raise SupervisorError("config_missing") from None
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise SupervisorError("config_file_unsafe")
        with open(path, "rb") as handle:
            try:
                raw = json.loads(handle.read(1 << 20))
            except ValueError:
                raise SupervisorError("config_invalid_json") from None
        return cls(raw, str(path))

    def role(self, name):
        if name not in self.roles:
            raise SupervisorError("unknown_role", detail=name)
        return self.roles[name]

    def pins_summary(self):
        pins = [{"name": "codex", "path": str(self.codex_bin), "sha256": self.codex_bin_sha256}]
        pins += [{"name": p["name"], "path": str(p["path"]), "sha256": p["sha256"]} for p in self.binary_pins]
        return pins

    def verify_pins(self):
        results = []
        for pin in self.pins_summary():
            item = dict(pin, verified=None)
            if pin["sha256"] and self.verify_binary_pins:
                try:
                    actual = file_sha256(pin["path"])
                except OSError:
                    raise SupervisorError("binary_pin_unreadable", detail=pin["name"]) from None
                if actual != pin["sha256"]:
                    raise SupervisorError("binary_pin_mismatch", detail=pin["name"])
                item["verified"] = True
            results.append(item)
        return results


# --------------------------------------------------------------------------- canonical

def canonical_metadata(cfg):
    """lstat-only metadata of the canonical credential; never opens it."""
    try:
        info = os.lstat(cfg.canonical_auth_file)
    except FileNotFoundError:
        return {"exists": False}
    return {"exists": True, "regular": stat.S_ISREG(info.st_mode), "symlink": stat.S_ISLNK(info.st_mode),
            "mode": oct(stat.S_IMODE(info.st_mode)), "uid_matches": info.st_uid == os.getuid(),
            "size": info.st_size, "mtime_ns": info.st_mtime_ns, "ino": info.st_ino, "dev": info.st_dev,
            "nlink": info.st_nlink}


def canonical_preflight(cfg):
    home = cfg.canonical_auth_home
    try:
        info = os.lstat(home)
    except FileNotFoundError:
        raise SupervisorError("canonical_home_missing", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH) from None
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise SupervisorError("canonical_home_not_directory", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if info.st_uid != os.getuid():
        raise SupervisorError("canonical_home_foreign_owner", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if info.st_mode & 0o077:
        raise SupervisorError("canonical_home_not_0700", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if os.path.realpath(home) != str(home):
        raise SupervisorError("canonical_home_not_physical_path", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    meta = canonical_metadata(cfg)
    if not meta["exists"]:
        raise SupervisorError("canonical_auth_missing", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if meta["symlink"] or not meta["regular"]:
        raise SupervisorError("canonical_auth_not_regular_file", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if not meta["uid_matches"]:
        raise SupervisorError("canonical_auth_foreign_owner", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if meta["mode"] != "0o600":
        raise SupervisorError("canonical_auth_not_0600", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    if meta["size"] <= 0:
        raise SupervisorError("canonical_auth_empty", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    return meta


def canonical_changed(before, after):
    keys = ("exists", "mtime_ns", "size", "ino", "dev")
    return any(before.get(k) != after.get(k) for k in keys)


# --------------------------------------------------------------------------- lease

def process_start_time(pid, ps_bin="/bin/ps"):
    entry = _process_entry(pid, ps_bin)
    if entry is None:
        return None
    try:
        return utc_iso(float(entry[2]))
    except ValueError:
        return entry[2]


class Lease:
    """flock on one lock file per auth source. Never LOCK_UN; the kernel releases
    the lock when the last descriptor sharing this open file description closes."""

    def __init__(self, cfg):
        self.cfg = cfg
        self.path = cfg.lease_path
        self.fd = None
        self.mode = None
        self.sidecar = None

    def open(self):
        ensure_private_dir(self.path.parent, create=True)
        try:
            fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        except OSError as exc:
            if exc.errno == errno.ELOOP:
                raise SupervisorError("lease_path_is_symlink") from None
            raise SupervisorError("lease_open_failed", detail=errno.errorcode.get(exc.errno)) from None
        info = os.fstat(fd)
        reason = None
        if not stat.S_ISREG(info.st_mode):
            reason = "lease_not_regular_file"
        elif info.st_uid != os.getuid():
            reason = "lease_foreign_owner"
        elif info.st_mode & 0o077:
            reason = "lease_not_0600"
        elif info.st_nlink != 1:
            reason = "lease_hardlinked"
        if reason:
            os.close(fd)
            raise SupervisorError(reason)
        self.fd = fd
        return self

    def acquire(self, mode, wait_seconds, poll=0.05):
        operation = fcntl.LOCK_EX if mode == "exclusive" else fcntl.LOCK_SH
        start = time.monotonic()
        deadline = start + wait_seconds
        while True:
            try:
                fcntl.flock(self.fd, operation | fcntl.LOCK_NB)
                self.mode = mode
                return True, round(time.monotonic() - start, 3)
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    return False, round(time.monotonic() - start, 3)
                time.sleep(poll)

    def convert_to_shared(self, wait_seconds):
        ok, _ = self.acquire("shared", wait_seconds)
        if not ok:
            raise SupervisorError("lease_downgrade_failed", "UNKNOWN", EXIT_UNKNOWN)

    def write_sidecar(self, run_id, role, operation):
        ensure_private_dir(self.cfg.holders_dir, create=True)
        self.sidecar = self.cfg.holders_dir / (role + "--" + run_id + ".json")
        record = {"pid": os.getpid(), "host_id": socket.gethostname(),
                  "process_start_time": process_start_time(os.getpid(), self.cfg.ps_bin),
                  "run_id": run_id, "role": role, "operation": operation, "mode": self.mode,
                  "acquired_at": utc_iso(), "released_at": None}
        write_json_atomic(self.sidecar, record)
        return record

    def update_sidecar(self, **fields):
        if self.sidecar is None:
            return
        try:
            with open(self.sidecar, "rb") as handle:
                record = json.loads(handle.read(65536))
        except (OSError, ValueError):
            record = {}
        record.update(fields)
        write_json_atomic(self.sidecar, record)

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


def probe_lease(cfg):
    """Report whether the lease is free without keeping any lock."""
    try:
        info = os.lstat(cfg.lease_path)
    except FileNotFoundError:
        return "absent"
    if not stat.S_ISREG(info.st_mode):
        return "unsafe"
    fd = os.open(cfg.lease_path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            return "held_exclusive"
        try:
            os.close(fd)
            fd = os.open(cfg.lease_path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return "held_shared"
        return "free"
    finally:
        os.close(fd)


# --------------------------------------------------------------------------- quarantine and ledger

def quarantine_state(cfg):
    try:
        info = os.lstat(cfg.quarantine_path)
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(info.st_mode):
        return {"present": True, "unreadable": True}
    try:
        with open(cfg.quarantine_path, "rb") as handle:
            record = json.loads(handle.read(65536))
    except (OSError, ValueError):
        record = {"unreadable": True}
    record["present"] = True
    return record


def write_quarantine(cfg, reason, run_id, details):
    record = {"schema": "gptgrep.codex-auth.quarantine.v1", "reason": reason, "run_id": run_id,
              "created_at": utc_iso(), "details": details,
              "clear_with": "codex_auth_supervisor.py clear-quarantine --run-id ID --confirm --reason TEXT"}
    try:
        write_new_private(cfg.quarantine_path, json.dumps(record, indent=2, sort_keys=True) + "\n")
        return {"created": True, "path": str(cfg.quarantine_path), "reason": reason}
    except FileExistsError:
        return {"created": False, "already_present": True, "path": str(cfg.quarantine_path), "reason": reason}


def ledger(cfg, entry):
    entry = dict(entry, schema=LEDGER_SCHEMA, auth_source_id=cfg.auth_source_id, recorded_at=utc_iso())
    append_jsonl(cfg.ledger_path, entry)


def last_ledger_entries(cfg, count=5):
    try:
        with open(cfg.ledger_path, "rb") as handle:
            lines = handle.read().splitlines()[-count:]
    except OSError:
        return []
    result = []
    for line in lines:
        try:
            result.append(json.loads(line))
        except ValueError:
            pass
    return result


# --------------------------------------------------------------------------- layout, config, environment

class Layout:
    def __init__(self, cfg, role, run_id):
        if not RUN_ID_RE.fullmatch(run_id or ""):
            raise SupervisorError("run_id_invalid")
        self.role = role
        self.run_id = run_id
        self.role_dir = cfg.runs_root / role
        self.run_dir = self.role_dir / run_id
        self.codex_home = self.run_dir / "codex-home"
        self.state_dir = self.run_dir / "state"
        self.log_dir = self.state_dir / "logs"
        self.home = self.run_dir / "home"
        self.tmp = self.run_dir / "tmp"
        self.work_dir = self.run_dir / "work"
        self.results = self.run_dir / "results"
        self.auth_link = self.codex_home / "auth.json"
        self.marker = self.state_dir / "lease-holder.marker"
        self.events = self.state_dir / "supervisor-events.jsonl"

    def placeholders(self):
        return {"codex_home": self.codex_home, "state_dir": self.state_dir, "log_dir": self.log_dir,
                "work_dir": self.work_dir, "home": self.home, "tmp": self.tmp, "results": self.results,
                "run_dir": self.run_dir}

    def xdg(self):
        return {"XDG_CONFIG_HOME": self.home / ".config", "XDG_CACHE_HOME": self.home / ".cache",
                "XDG_DATA_HOME": self.home / ".local" / "share", "XDG_STATE_HOME": self.home / ".local" / "state"}

    def describe(self):
        values = {k: str(v) for k, v in self.placeholders().items()}
        values["auth_link"] = str(self.auth_link)
        return values

    def create(self, cfg, extra_codex_config):
        ensure_private_dir(cfg.runs_root, create=True, create_parents=True)
        if os.path.realpath(cfg.runs_root) != str(cfg.runs_root):
            raise SupervisorError("runs_root_not_physical_path")
        ensure_private_dir(self.role_dir, create=True)
        try:
            os.mkdir(self.run_dir, 0o700)
        except FileExistsError:
            raise SupervisorError("RUN_ALREADY_EXISTS", "RUN_ALREADY_EXISTS", EXIT_RUN_EXISTS) from None
        for path in (self.codex_home, self.state_dir, self.log_dir, self.home, self.tmp, self.work_dir,
                     self.results, *self.xdg().values()):
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
            os.chmod(path, 0o700)
        if is_within(self.codex_home, self.tmp):
            raise SupervisorError("codex_home_under_tmpdir")
        os.symlink(str(cfg.canonical_auth_file), self.auth_link)
        text = render_codex_config(cfg, self, extra_codex_config)
        write_new_private(self.codex_home / "config.toml", text)
        fd = os.open(self.marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        os.close(fd)
        return text

    def link_state(self, cfg):
        try:
            info = os.lstat(self.auth_link)
        except FileNotFoundError:
            return {"present": False}
        if not stat.S_ISLNK(info.st_mode):
            return {"present": True, "symlink": False}
        return {"present": True, "symlink": True,
                "target_matches": os.readlink(self.auth_link) == str(cfg.canonical_auth_file)}

    def event(self, name, **fields):
        try:
            append_jsonl(self.events, dict(fields, event=name, at=utc_iso()))
        except OSError:
            pass


def _toml_key(key):
    return key if re.fullmatch(r"[A-Za-z0-9_-]+", key) else json.dumps(key)


def _toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=True)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(v) for v in value) + "]"
    raise SupervisorError("toml_value_unsupported")


def render_toml(tree):
    lines = []

    def emit(prefix, table):
        scalars = [(k, v) for k, v in table.items() if not isinstance(v, dict)]
        tables = [(k, v) for k, v in table.items() if isinstance(v, dict)]
        if prefix and scalars:
            lines.append("")
            lines.append("[" + prefix + "]")
        for key, value in scalars:
            lines.append(_toml_key(key) + " = " + _toml_value(value))
        for key, value in tables:
            emit((prefix + "." if prefix else "") + _toml_key(key), value)

    emit("", tree)
    return "\n".join(lines).lstrip("\n") + "\n"


def codex_config_tree(cfg, layout, extra):
    tree = json.loads(json.dumps(PINNED_CODEX_CONFIG))
    merged = dict(cfg.codex_config)
    merged.update(extra or {})
    root = {k: v for k, v in tree.items() if not isinstance(v, dict)}
    root.update(merged)
    root["log_dir"] = str(layout.log_dir)
    tables = {k: v for k, v in tree.items() if isinstance(v, dict)}
    result = dict(sorted(root.items()))
    result.update(tables)
    return result


def render_codex_config(cfg, layout, extra):
    header = ("# Generated by codex_auth_supervisor " + SUPERVISOR_VERSION +
              "; per-run CODEX_HOME. Do not copy the canonical config.toml here.\n")
    return header + render_toml(codex_config_tree(cfg, layout, extra))


def base_env(cfg, layout, run_id):
    env = {"PATH": cfg.path_env, "LANG": cfg.locale, "LC_ALL": cfg.locale,
           "HOME": str(layout.home), "TMPDIR": str(layout.tmp),
           "CODEX_SQLITE_HOME": str(layout.state_dir),
           "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED": "1",
           "NO_PROXY": LOOPBACK_NO_PROXY, "no_proxy": LOOPBACK_NO_PROXY,
           "GPTGREP_AUTH_RUN_ID": run_id, "GPTGREP_AUTH_SOURCE_ID": cfg.auth_source_id}
    env.update({k: str(v) for k, v in layout.xdg().items()})
    return env


def build_consumer_env(cfg, role, layout, parent_env, lease_fd=None):
    """Allowlist only: nothing is inherited unless named for the role."""
    spec = cfg.role(role)
    env = base_env(cfg, layout, layout.run_id)
    if spec["path_env"]:
        env["PATH"] = spec["path_env"]
    env["CODEX_HOME"] = str(layout.codex_home)
    env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = cfg.blocker_url
    passed = []
    for name in spec["passthrough_env"]:
        if name in parent_env:
            env[name] = parent_env[name]
            passed.append(name)
    if lease_fd is not None:
        env["GPTGREP_AUTH_LEASE_FD"] = str(lease_fd)
    for name in STRIPPED_ALWAYS:
        env.pop(name, None)
    if not spec["allow_openrouter"]:
        env.pop("OPENROUTER_API_KEY", None)
    return env, passed


def build_owner_env(cfg, layout, parent_env, codex_home):
    env = base_env(cfg, layout, layout.run_id)
    env["CODEX_HOME"] = str(codex_home)
    for name in cfg.owner_passthrough_env:
        if name in parent_env:
            env[name] = parent_env[name]
    if cfg.owner_refresh_token_url:
        env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = cfg.owner_refresh_token_url
    for name in STRIPPED_ALWAYS + ("OPENROUTER_API_KEY",):
        env.pop(name, None)
    return env


def substitute(argv, layout):
    values = {"{" + k + "}": str(v) for k, v in layout.placeholders().items()}
    result = []
    for arg in argv:
        for key, value in values.items():
            arg = arg.replace(key, value)
        result.append(arg)
    return result


def check_blocker_closed(cfg):
    """A listener on the blocker port could harvest refresh tokens; refuse it."""
    if not cfg.blocker_must_be_closed:
        return "not_checked"
    parsed = urllib.parse.urlsplit(cfg.blocker_url)
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    host = parsed.hostname
    try:
        family = socket.AF_INET6 if ":" in host else socket.AF_INET
        with socket.socket(family, socket.SOCK_STREAM) as probe:
            probe.settimeout(1.0)
            probe.connect((host, port))
    except ConnectionRefusedError:
        return "closed"
    except OSError as exc:
        return "unreachable:" + (errno.errorcode.get(exc.errno, "error") if exc.errno else "timeout")
    raise SupervisorError("blocker_port_has_listener", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)


# --------------------------------------------------------------------------- processes

class _ProcBsdInfo(ctypes.Structure):
    """struct proc_bsdinfo from <sys/proc_info.h> (PROC_PIDTBSDINFO)."""
    _fields_ = [("pbi_flags", ctypes.c_uint32), ("pbi_status", ctypes.c_uint32), ("pbi_xstatus", ctypes.c_uint32),
                ("pbi_pid", ctypes.c_uint32), ("pbi_ppid", ctypes.c_uint32), ("pbi_uid", ctypes.c_uint32),
                ("pbi_gid", ctypes.c_uint32), ("pbi_ruid", ctypes.c_uint32), ("pbi_rgid", ctypes.c_uint32),
                ("pbi_svuid", ctypes.c_uint32), ("pbi_svgid", ctypes.c_uint32), ("rfu_1", ctypes.c_uint32),
                ("pbi_comm", ctypes.c_char * 16), ("pbi_name", ctypes.c_char * 32),
                ("pbi_nfiles", ctypes.c_uint32), ("pbi_pgid", ctypes.c_uint32), ("pbi_pjobc", ctypes.c_uint32),
                ("e_tdev", ctypes.c_uint32), ("e_tpgid", ctypes.c_uint32), ("pbi_nice", ctypes.c_int32),
                ("pbi_start_tvsec", ctypes.c_uint64), ("pbi_start_tvusec", ctypes.c_uint64)]


_PROC_PIDTBSDINFO = 3
_SZOMB = 5
_LIBPROC = []


def _libproc():
    """macOS libproc via ctypes: no setuid helper (ps is setuid root and cannot
    run inside a seatbelt sandbox), only this uid's processes are visible."""
    if not _LIBPROC:
        lib = None
        if sys.platform == "darwin" and ctypes.sizeof(_ProcBsdInfo) == 136:
            try:
                lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
                lib.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
                lib.proc_listallpids.restype = ctypes.c_int
                lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p,
                                             ctypes.c_int]
                lib.proc_pidinfo.restype = ctypes.c_int
            except (OSError, AttributeError):
                lib = None
        _LIBPROC.append(lib)
    return _LIBPROC[0]


def _bsdinfo(lib, pid):
    info = _ProcBsdInfo()
    if lib.proc_pidinfo(pid, _PROC_PIDTBSDINFO, 0, ctypes.byref(info), ctypes.sizeof(info)) != ctypes.sizeof(info):
        return None
    if info.pbi_status == _SZOMB:
        return None  # exited, awaiting reap: can no longer use the credential
    return (int(info.pbi_ppid), int(info.pbi_pgid),
            "%d.%06d" % (info.pbi_start_tvsec, info.pbi_start_tvusec))


def _procfs_entry(pid):
    try:
        with open("/proc/%d/stat" % pid, "rb") as handle:
            data = handle.read().decode("utf-8", "replace")
    except OSError:
        return None
    fields = data[data.rindex(")") + 2:].split()
    if fields[0] == "Z":
        return None
    return int(fields[1]), int(fields[2]), fields[19]


def _ps_table(ps_bin):
    out = subprocess.run([str(ps_bin), "-A", "-o", "pid=,ppid=,pgid=,stat=,lstart="], capture_output=True,
                         text=True, timeout=30, env={"LC_ALL": "C", "PATH": DEFAULT_PATH})
    if out.returncode != 0:
        raise OSError("ps_failed")
    table = {}
    for line in out.stdout.splitlines():
        parts = line.split()
        if len(parts) < 5 or not all(p.isdigit() for p in parts[:3]) or parts[3].startswith("Z"):
            continue
        table[int(parts[0])] = (int(parts[1]), int(parts[2]), " ".join(parts[4:]))
    return table


def _process_entry(pid, ps_bin="/bin/ps"):
    lib = _libproc()
    if lib is not None:
        return _bsdinfo(lib, pid)
    if os.path.isdir("/proc/self"):
        return _procfs_entry(pid)
    try:
        return _ps_table(ps_bin).get(pid)
    except (OSError, subprocess.TimeoutExpired):
        return None


def process_table(cfg):
    """{pid: (ppid, pgid, start-identity)} for live, visible, non-zombie processes."""
    lib = _libproc()
    if lib is not None:
        count = lib.proc_listallpids(None, 0)
        if count <= 0:
            raise OSError("proc_listallpids_failed")
        buffer = (ctypes.c_int * (count + 256))()
        count = lib.proc_listallpids(buffer, ctypes.sizeof(buffer))
        if count <= 0:
            raise OSError("proc_listallpids_failed")
        table = {}
        for pid in buffer[:count]:
            entry = _bsdinfo(lib, pid) if pid > 0 else None
            if entry is not None:
                table[pid] = entry
        return table
    if os.path.isdir("/proc/self"):
        table = {}
        for name in os.listdir("/proc"):
            if name.isdigit():
                entry = _procfs_entry(int(name))
                if entry is not None:
                    table[int(name)] = entry
        return table
    return _ps_table(cfg.ps_bin)


class Tracker:
    """Periodic pid-tree tracking for one launched root process.

    Membership: the root, transitive children by ppid (including ones later
    reparented, once observed), and members of the root's process group or
    session. A pid is identified by (pid, start time) so reuse is not confused.
    """

    def __init__(self, cfg, root_pid, known=None):
        self.cfg = cfg
        self.root = root_pid
        self.known = dict(known or {})
        self.snapshots = 0

    def snapshot(self):
        try:
            table = process_table(self.cfg)
        except (OSError, subprocess.TimeoutExpired):
            return None
        self.snapshots += 1
        if self.root in table and self.root not in self.known:
            self.known[self.root] = table[self.root][2]
        changed = True
        while changed:
            changed = False
            for pid, (ppid, pgid, start) in table.items():
                if pid in self.known:
                    continue
                parent = table.get(ppid)
                if (ppid in self.known and parent is not None and parent[2] == self.known[ppid]) or \
                        (pgid == self.root and self.root in self.known):
                    self.known[pid] = start
                    changed = True
        for pid, (_ppid, _pgid, start) in table.items():
            if pid in self.known or pid == os.getpid():
                continue
            try:
                if os.getsid(pid) == self.root:
                    self.known[pid] = start
            except OSError:
                pass
        return table

    def alive(self, table):
        if table is None:
            return {}
        return {pid: start for pid, start in self.known.items() if pid in table and table[pid][2] == start}

    def signal_all(self, sig, exclude=()):
        table = self.snapshot()
        alive = self.alive(table)
        sent = 0
        for pid in alive:
            if pid in exclude:
                continue
            pgid = table[pid][1]
            if pgid in alive and pgid not in exclude:
                try:
                    os.killpg(pgid, sig)
                    sent += 1
                except OSError:
                    pass
            try:
                os.kill(pid, sig)
                sent += 1
            except OSError:
                pass
        return sorted(alive)


def lsof_holders(cfg, run_dir, marker, lease_path):
    """One lsof pass: processes with the marker, the lease file, or anything
    (including cwd) under the run directory open. Returns (dict, error)."""
    try:
        out = subprocess.run([str(cfg.lsof_bin), "-n", "-P", "-l", "-w", "-F", "pfn", "+D", str(run_dir),
                              "--", str(marker), str(lease_path)], capture_output=True, text=True, timeout=120,
                             env={"LC_ALL": "C", "PATH": DEFAULT_PATH})
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, type(exc).__name__
    if out.returncode not in (0, 1):
        return None, "lsof_exit_" + str(out.returncode)
    found = {"marker": set(), "lease": set(), "run_dir": set()}
    pid = None
    prefix = str(run_dir) + "/"
    for line in out.stdout.splitlines():
        if line.startswith("p") and line[1:].isdigit():
            pid = int(line[1:])
        elif line.startswith("n") and pid is not None:
            name = line[1:]
            if name == str(marker):
                found["marker"].add(pid)
            if name == str(lease_path):
                found["lease"].add(pid)
            if name == str(run_dir) or name.startswith(prefix):
                found["run_dir"].add(pid)
    return found, None


def sweep(cfg, tracker, layout, exclude):
    table = tracker.snapshot()
    alive = set(tracker.alive(table))
    holders, error = lsof_holders(cfg, layout.run_dir, layout.marker, cfg.lease_path)
    exclude = set(exclude) | {os.getpid()}
    holders = holders or {"marker": set(), "lease": set(), "run_dir": set()}
    survivors = (alive | holders["marker"] | holders["run_dir"]) - exclude
    errors = [error] if error else []
    if table is None:
        errors.append("ps_unavailable")
    lease_other = sorted(holders["lease"] - exclude - survivors)
    return {"survivor_pids": sorted(survivors), "tracked_pids_total": len(tracker.known),
            "tracker_snapshots": tracker.snapshots,
            "tracked_alive": sorted(alive - exclude),
            "inherited_fd_holders": sorted(holders["marker"] - exclude),
            "run_dir_open_file_holders": sorted(holders["run_dir"] - exclude),
            "lease_file_open_by_unattributed_pids": len(lease_other),
            "inconclusive": bool(errors), "errors": errors}


def sweep_settled(cfg, tracker, layout, exclude, settle_seconds=None):
    """Re-sweep briefly so just-killed processes are not reported as survivors."""
    settle = cfg.sweep_settle_seconds if settle_seconds is None else settle_seconds
    deadline = time.monotonic() + settle
    while True:
        result = sweep(cfg, tracker, layout, exclude)
        if (not result["survivor_pids"] and not result["inconclusive"]) or time.monotonic() >= deadline:
            return result
        time.sleep(0.2)


def docker_containers(cfg, label):
    if cfg.docker_bin is None:
        return None, "docker_not_configured"
    env = {"PATH": cfg.path_env, "LC_ALL": "C"}
    env.update(cfg.docker_env)
    try:
        out = subprocess.run([str(cfg.docker_bin), "ps", "-a", "-q", "--filter", "label=" + label],
                             capture_output=True, text=True, timeout=60, env=env, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, type(exc).__name__
    if out.returncode != 0:
        return None, "docker_exit_" + str(out.returncode)
    return [line.strip() for line in out.stdout.splitlines() if line.strip()], None


def wait_docker_clear(cfg, label):
    deadline = time.monotonic() + cfg.docker_wait_seconds
    polls = 0
    while True:
        ids, error = docker_containers(cfg, label)
        polls += 1
        if ids == []:
            return {"label": label, "remaining": 0, "polls": polls, "cleared": True}
        if time.monotonic() >= deadline:
            return {"label": label, "remaining": None if ids is None else len(ids), "polls": polls,
                    "cleared": False, "error": error}
        time.sleep(cfg.docker_poll_seconds)


def spawn_keeper(cfg, layout, lease, tracker, docker_label=None):
    """Retain the lease after supervisor exit until confirmed survivors (processes
    and, for Harbor runs, labelled containers) are gone."""
    record = {"run_dir": str(layout.run_dir), "role": layout.role, "run_id": layout.run_id,
              "root_pid": tracker.root, "known": {str(k): v for k, v in tracker.known.items()},
              "sidecar": str(lease.sidecar) if lease.sidecar else None, "docker_label": docker_label,
              "config_snapshot": cfg.raw, "config_source": cfg.source_path}
    path = layout.state_dir / "lease-keeper-input.json"
    write_json_atomic(path, record)
    argv = [sys.executable, "-I", "-B", os.path.abspath(__file__), "_lease-keeper", "--input", str(path)]
    proc = subprocess.Popen(argv, cwd="/", env={"PATH": cfg.path_env, "LC_ALL": "C"},
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True, pass_fds=(lease.fd,))
    return proc.pid


def lease_keeper_main(input_path):
    """Hold the inherited lease until confirmed survivors are gone.

    Uses the config snapshot taken at spawn time, so a later edit of the config file
    cannot make the keeper exit early; any error counts as "not clear yet"."""
    with open(input_path, "rb") as handle:
        record = json.loads(handle.read(4 << 20))
    cfg = Config(record["config_snapshot"], record["config_source"])
    layout = Layout(cfg, record["role"], record["run_id"])
    if layout.run_dir != Path(record["run_dir"]):
        raise SupervisorError("keeper_run_dir_mismatch")
    tracker = Tracker(cfg, int(record["root_pid"]), {int(k): v for k, v in record["known"].items()})
    docker_label = record.get("docker_label")
    quiet = 0
    while quiet < 2:
        try:
            result = sweep(cfg, tracker, layout, exclude={os.getpid()})
            clear = not result["survivor_pids"] and not result["inconclusive"]
            if clear and docker_label:
                ids, _error = docker_containers(cfg, docker_label)
                clear = ids == []
        except Exception:  # noqa: BLE001 - never release on an unexpected error
            clear = False
        quiet = quiet + 1 if clear else 0
        if quiet < 2:
            time.sleep(cfg.keeper_poll_seconds)
    layout.event("lease_keeper_released", keeper_pid=os.getpid())
    if record.get("sidecar"):
        lease = Lease(cfg)
        lease.sidecar = Path(record["sidecar"])
        lease.update_sidecar(released_at=utc_iso(), released_by="lease_keeper_after_survivors_exited")
    return EXIT_PASS


# --------------------------------------------------------------------------- JSON-RPC

class RpcError(Exception):
    pass


class RpcClient:
    """Minimal newline-delimited JSON-RPC client for `codex app-server --listen stdio://`.

    Message bodies are kept in memory only; callers record classifications.
    """

    def __init__(self, proc):
        self.proc = proc
        self.write_lock = threading.Lock()
        self.cond = threading.Condition()
        self.responses = {}
        self.notifications = []
        self.server_requests = 0
        self.closed = False
        self.next_id = 1
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self):
        try:
            for raw in self.proc.stdout:
                try:
                    message = json.loads(raw)
                except ValueError:
                    continue
                if not isinstance(message, dict):
                    continue
                if "method" in message and "id" in message:
                    self.server_requests += 1
                    try:
                        self._send({"id": message["id"],
                                    "error": {"code": -32601, "message": "not supported by this client"}})
                    except RpcError:
                        pass
                elif "id" in message:
                    with self.cond:
                        self.responses[message["id"]] = message
                        self.cond.notify_all()
                elif "method" in message:
                    with self.cond:
                        self.notifications.append(str(message.get("method"))[:80])
        finally:
            with self.cond:
                self.closed = True
                self.cond.notify_all()

    def _send(self, message):
        data = (json.dumps(message) + "\n").encode()
        with self.write_lock:
            try:
                self.proc.stdin.write(data)
                self.proc.stdin.flush()
            except (BrokenPipeError, ValueError, OSError) as exc:
                raise RpcError("stdin_closed") from exc

    def notify(self, method, params=None):
        message = {"method": method}
        if params is not None:
            message["params"] = params
        self._send(message)

    def request(self, method, params, timeout):
        request_id = self.next_id
        self.next_id += 1
        message = {"id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self._send(message)
        deadline = time.monotonic() + timeout
        with self.cond:
            while request_id not in self.responses:
                if self.closed:
                    raise RpcError("connection_closed")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RpcError("timeout")
                self.cond.wait(min(remaining, 0.5))
            return self.responses.pop(request_id)

    def close_stdin(self):
        with self.write_lock:
            try:
                self.proc.stdin.close()
            except OSError:
                pass


def classify_response(method, message):
    """Non-secret projection of a JSON-RPC response (no email, tokens or bodies)."""
    if message is None:
        return {"method": method, "ok": False, "error": "no_response"}
    if "error" in message:
        error = message.get("error") or {}
        return {"method": method, "ok": False, "error_code": error.get("code"),
                "error_message": redact(error.get("message"))}
    result = message.get("result") or {}
    item = {"method": method, "ok": True}
    if method == "account/read":
        account = result.get("account") or {}
        item["account_type"] = account.get("type")
        item["requires_openai_auth"] = result.get("requiresOpenaiAuth")
    return item


# --------------------------------------------------------------------------- run helpers

class Signals:
    def __init__(self):
        self.received = None
        self.previous = {}

    def install(self):
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            self.previous[sig] = signal.signal(sig, self._handle)

    def _handle(self, signum, _frame):
        if self.received is None:
            self.received = signal.Signals(signum).name

    def restore(self):
        for sig, handler in self.previous.items():
            signal.signal(sig, handler)


def _deadline_for(cfg, role, requested):
    if requested is not None:
        return float(requested)
    role_deadline = cfg.role(role)["deadline_seconds"]
    return float(role_deadline if role_deadline is not None else cfg.default_deadline_seconds)


def _final_state(child_code, timed_out, cancelled, survivors, docker_uncleared, inconclusive, quarantined):
    if survivors or docker_uncleared or inconclusive:
        return "UNKNOWN"
    if quarantined:
        return "QUARANTINED"
    if timed_out:
        return "TIMEOUT"
    if cancelled:
        return "CANCELLED"
    return "PASS" if child_code == 0 else "FAILED"


def _exit_for(state, child_code):
    if state == "FAILED":
        return child_code if isinstance(child_code, int) and 1 <= child_code <= 63 else 1
    return STATE_EXIT.get(state, EXIT_USAGE)


def _open_log(path):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    return os.fdopen(fd, "wb")


def _base_receipt(cfg, operation, layout, lease_mode):
    return {"schema": RUN_SCHEMA, "supervisor_version": SUPERVISOR_VERSION, "operation": operation,
            "run_id": layout.run_id, "role": layout.role, "auth_source_id": cfg.auth_source_id,
            "auth_backend": "file", "canonical_auth_home": str(cfg.canonical_auth_home),
            "lease": {"path": str(cfg.lease_path), "mode": lease_mode}, "layout": layout.describe(),
            "codex_home": str(layout.codex_home), "binary_pins": cfg.pins_summary(),
            "blocker_url": cfg.blocker_url, "credential_contents_read_by_supervisor": False,
            "timings": {"started_at": utc_iso()}, "anomalies": []}


def _finish(layout, receipt, state, exit_code):
    receipt["state"] = state
    receipt["exit_code"] = exit_code
    receipt["timings"]["finished_at"] = utc_iso()
    try:
        write_json_atomic(layout.run_dir / "receipt.json", receipt)
    except OSError:
        receipt["receipt_write_failed"] = True
    layout.event("finished", state=state, exit_code=exit_code)
    return receipt, exit_code


def acquire_or_busy(cfg, lease, mode, wait_seconds, layout, receipt):
    lease.open()
    ok, waited = lease.acquire(mode, wait_seconds)
    receipt["lease"].update({"wait_seconds": wait_seconds, "waited_seconds": waited, "acquired": ok})
    if not ok:
        layout.event("auth_busy", waited_seconds=waited)
        return False
    receipt["timings"]["lease_acquired_at"] = utc_iso()
    receipt["lease"]["sidecar"] = lease.write_sidecar(layout.run_id, layout.role, receipt["operation"])
    receipt["lease"]["sidecar_path"] = str(lease.sidecar)
    layout.event("lease_acquired", mode=mode, waited_seconds=waited)
    return True


def stop_process_tree(cfg, proc, tracker, grace, allow_kill):
    """SIGTERM tracked groups, wait `grace`, then SIGKILL only when allowed."""
    tracker.signal_all(signal.SIGTERM)
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        if proc.poll() is not None and not tracker.alive(tracker.snapshot()):
            return "terminated"
        time.sleep(min(cfg.sweep_interval_seconds, 0.2))
    if proc.poll() is None or tracker.alive(tracker.snapshot()):
        if not allow_kill:
            return "still_running_after_grace"
        tracker.signal_all(signal.SIGKILL)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pass
        return "killed"
    return "terminated"


# --------------------------------------------------------------------------- owner refresh

def classify_owner_log(path):
    """Best-effort, non-secret class of a refresh failure from the owner's own log lines."""
    try:
        with open(path, "rb") as handle:
            lines = handle.read(4 << 20).decode("utf-8", "replace").splitlines()
    except OSError:
        return None
    relevant = [line.lower() for line in lines if "refresh" in line.lower()]
    for code, needle in REFRESH_FAILURE_PATTERNS:
        if any(needle in line for line in relevant):
            return code
    return None


def owner_refresh(cfg, run_id, force, lease, parent_env, receipt=None, layout=None):
    """Refresh through the official app-server; the caller holds the EXCLUSIVE lease."""
    assert lease.mode == "exclusive"
    if layout is None:
        layout = Layout(cfg, "_owner", run_id)
        layout.create(cfg, {})
    if receipt is None:
        receipt = _base_receipt(cfg, "refresh", layout, "exclusive")
    receipt["refresh"] = {"forced": bool(force), "max_age_seconds": cfg.refresh_max_age_seconds}
    before = canonical_preflight(cfg)
    receipt["canonical_before"] = before
    age = max(0.0, time.time() - before["mtime_ns"] / 1e9)
    receipt["refresh"]["canonical_age_seconds"] = round(age, 3)
    if not force and age <= cfg.refresh_max_age_seconds:
        receipt["refresh"]["decision"] = "SKIPPED_FRESH"
        ledger(cfg, {"run_id": run_id, "decision": "SKIPPED_FRESH", "forced": False,
                     "canonical_age_seconds": round(age, 3), "canonical_before": before})
        return receipt, "PASS", layout, None
    env = build_owner_env(cfg, layout, parent_env, layout.codex_home)
    argv = [str(cfg.codex_bin), "app-server", "--listen", "stdio://", "--strict-config"]
    receipt.update({"argv": argv, "env_keys": sorted(env)})
    stderr_log = _open_log(layout.log_dir / "owner-app-server.stderr.log")
    layout.event("owner_launch")
    receipt["timings"]["owner_started_at"] = utc_iso()
    proc = subprocess.Popen(argv, cwd=str(layout.work_dir), env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=stderr_log, start_new_session=True,
                            pass_fds=(lease.fd,))
    stderr_log.close()
    tracker = Tracker(cfg, proc.pid)
    tracker.snapshot()
    client = RpcClient(proc)
    steps = []
    timeout = cfg.owner_rpc_timeout_seconds
    try:
        steps.append(classify_response("initialize", client.request(
            "initialize", {"clientInfo": {"name": "gptgrep_codex_auth_owner", "title": None,
                                          "version": SUPERVISOR_VERSION}}, timeout)))
        client.notify("initialized")
        tracker.snapshot()
        steps.append(classify_response("account/read", client.request(
            "account/read", {"refreshToken": True}, timeout)))
        steps.append(classify_response("account/read", client.request("account/read", {}, timeout)))
    except RpcError as exc:
        steps.append({"method": "rpc", "ok": False, "error": str(exc)})
    tracker.snapshot()
    client.close_stdin()
    stop = "stdin_closed"
    try:
        proc.wait(timeout=cfg.owner_exit_wait_seconds)
    except subprocess.TimeoutExpired:
        layout.event("owner_sigterm", grace_seconds=cfg.owner_term_grace_seconds)
        stop = "sigterm"
        result = stop_process_tree(cfg, proc, tracker, cfg.owner_term_grace_seconds, allow_kill=True)
        if result == "killed":
            stop = "sigkill_after_grace"
            receipt["anomalies"].append({"code": "OWNER_SIGKILLED_AFTER_GRACE",
                                         "note": "in-place save may have been interrupted"})
    receipt["timings"]["owner_exited_at"] = utc_iso()
    receipt["owner"] = {"pid": proc.pid, "exit_code": proc.returncode, "stop": stop, "rpc": steps,
                        "server_requests_declined": client.server_requests,
                        "notifications": len(client.notifications)}
    result = sweep_settled(cfg, tracker, layout, exclude=set())
    receipt["sweep"] = result
    after = canonical_metadata(cfg)
    receipt["canonical_after"] = after
    link = layout.link_state(cfg)
    receipt["auth_link_after"] = link
    changed = canonical_changed(before, after)
    integrity = (after.get("exists") and after.get("regular") and not after.get("symlink") and
                 after.get("mode") == "0o600" and after.get("uid_matches") and
                 after.get("ino") == before.get("ino") and after.get("dev") == before.get("dev") and
                 link.get("symlink") and link.get("target_matches"))
    account_ok = len(steps) >= 3 and steps[2].get("ok") and steps[2].get("account_type") == "chatgpt"
    failure_class = classify_owner_log(layout.log_dir / "owner-app-server.stderr.log")
    receipt["refresh"].update({"canonical_changed": changed, "inode_preserved": after.get("ino") == before.get("ino"),
                               "integrity_ok": bool(integrity), "account_chatgpt": bool(account_ok),
                               "owner_log_failure_class": failure_class})
    survivors = bool(result["survivor_pids"]) or result["inconclusive"]
    if any(a["code"] == "OWNER_SIGKILLED_AFTER_GRACE" for a in receipt["anomalies"]):
        decision, state, reason = "ANOMALY", "QUARANTINED", "OWNER_SIGKILLED_AFTER_GRACE"
    elif changed and not integrity:
        decision, state, reason = "ANOMALY", "QUARANTINED", "CANONICAL_INTEGRITY_CHANGED"
        receipt["anomalies"].append({"code": reason})
    elif changed and integrity and account_ok:
        decision, state, reason = "REFRESHED", "PASS", None
    else:
        decision, state, reason = "AUTH_REFRESH_FAILED", "BLOCKED_AUTH", "AUTH_REFRESH_FAILED"
    if reason:
        receipt["quarantine"] = write_quarantine(cfg, reason, run_id, {
            "operation": "refresh", "owner_log_failure_class": failure_class, "canonical_changed": changed})
    if survivors:
        state = "UNKNOWN"
    receipt["refresh"]["decision"] = decision
    ledger(cfg, {"run_id": run_id, "decision": decision, "forced": bool(force),
                 "canonical_age_seconds": round(age, 3), "canonical_before": before, "canonical_after": after,
                 "canonical_changed": changed, "inode_preserved": after.get("ino") == before.get("ino"),
                 "owner_exit_code": proc.returncode, "owner_stop": stop, "failure_class": failure_class,
                 "binary": {"path": str(cfg.codex_bin), "sha256": cfg.codex_bin_sha256}})
    return receipt, state, layout, tracker


def require_codex_pin(cfg):
    if not cfg.codex_bin_sha256 or not cfg.verify_binary_pins:
        raise SupervisorError("codex_bin_pin_required_for_owner_and_relogin")


def command_refresh(cfg, args):
    layout = Layout(cfg, "_owner", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "refresh", layout, "exclusive")
    lease = Lease(cfg)
    try:
        require_codex_pin(cfg)
        receipt["binary_pins"] = cfg.verify_pins()
        quarantine = quarantine_state(cfg)
        if quarantine:
            receipt["quarantine"] = quarantine
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        if not acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt):
            ledger(cfg, {"run_id": args.run_id, "decision": "AUTH_BUSY", "forced": bool(args.force)})
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        receipt, state, layout, tracker = owner_refresh(cfg, args.run_id, args.force, lease, dict(os.environ),
                                                        receipt, layout)
        if state == "UNKNOWN" and tracker is not None:
            receipt["lease"]["retained_by_keeper_pid"] = spawn_keeper(cfg, layout, lease, tracker)
        elif lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, state, STATE_EXIT[state])
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        lease.close()


# --------------------------------------------------------------------------- consumer run

def command_run(cfg, args):
    role = args.role
    spec = cfg.role(role)
    if role.startswith("_"):
        raise SupervisorError("role_reserved")
    if not args.command:
        raise SupervisorError("command_required")
    mode = args.lease
    if mode == "shared" and not cfg.allow_shared_consumers:
        raise SupervisorError("shared_lease_not_enabled")
    deadline = _deadline_for(cfg, role, args.deadline_seconds)
    if args.harbor_job_label is not None and not LABEL_RE.fullmatch(args.harbor_job_label):
        raise SupervisorError("harbor_job_label_invalid")
    pre_refresh = args.pre_refresh or cfg.pre_run_refresh
    layout = Layout(cfg, role, args.run_id)
    extra = spec["codex_config"]
    if args.dry_run:
        env, passed = build_consumer_env(cfg, role, layout, dict(os.environ))
        return {"schema": RUN_SCHEMA, "operation": "run", "dry_run": True, "role": role, "run_id": args.run_id,
                "lease_mode": mode, "deadline_seconds": deadline, "layout": layout.describe(),
                "argv": substitute(args.command, layout), "env_keys": sorted(env), "passthrough_env": passed,
                "codex_config_toml": render_codex_config(cfg, layout, extra), "pre_refresh": pre_refresh,
                "blocker_url": cfg.blocker_url, "binary_pins": cfg.pins_summary()}, EXIT_PASS
    layout.create(cfg, extra)
    receipt = _base_receipt(cfg, "run", layout, mode)
    receipt.update({"deadline_seconds": deadline, "harbor_job_label": args.harbor_job_label,
                    "live_request_attempted": {"yes": True, "no": False}.get(args.live_request_attempted),
                    "live_request_attempted_source": "caller", "pre_refresh": pre_refresh,
                    "caller_fields": dict(args.manifest_field or [])})
    layout.event("created")
    lease = Lease(cfg)
    signals = Signals()
    try:
        receipt["binary_pins"] = cfg.verify_pins()
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        initial_mode = "exclusive" if pre_refresh != "never" else mode
        if not acquire_or_busy(cfg, lease, initial_mode, wait, layout, receipt):
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        if pre_refresh != "never":
            require_codex_pin(cfg)
            owner_layout = Layout(cfg, "_owner", args.run_id + "-pre")
            owner_layout.create(cfg, {})
            owner_receipt = _base_receipt(cfg, "refresh", owner_layout, "exclusive")
            owner_receipt["binary_pins"] = receipt["binary_pins"]
            owner_receipt["lease"].update(dict(receipt["lease"], note="lease held by parent run"))
            try:
                owner_receipt, owner_state, owner_layout, tracker = owner_refresh(
                    cfg, owner_layout.run_id, pre_refresh == "always", lease, dict(os.environ),
                    owner_receipt, owner_layout)
            except SupervisorError as exc:
                owner_receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
                owner_state, tracker = exc.state, None
            _finish(owner_layout, owner_receipt, owner_state, STATE_EXIT.get(owner_state, EXIT_USAGE))
            receipt["pre_refresh_result"] = {"state": owner_state,
                                             "decision": owner_receipt.get("refresh", {}).get("decision"),
                                             "owner_run_dir": str(owner_layout.run_dir)}
            if owner_state != "PASS":
                if owner_state == "UNKNOWN" and tracker is not None:
                    receipt["lease"]["retained_by_keeper_pid"] = spawn_keeper(cfg, owner_layout, lease, tracker)
                return _finish(layout, receipt, owner_state, STATE_EXIT.get(owner_state, EXIT_USAGE))
            if mode == "shared":
                lease.convert_to_shared(wait)
                lease.update_sidecar(mode="shared")
                receipt["lease"]["converted_to_shared"] = True
        receipt["blocker_probe"] = check_blocker_closed(cfg)
        before = canonical_preflight(cfg)
        receipt["canonical_before"] = before
        env, passed = build_consumer_env(cfg, role, layout, dict(os.environ), lease.fd)
        argv = substitute(args.command, layout)
        receipt.update({"argv": argv, "cwd": str(layout.work_dir), "env_keys": sorted(env),
                        "passthrough_env": passed, "stripped_env": list(STRIPPED_ALWAYS)})
        write_json_atomic(layout.run_dir / "manifest.json", dict(receipt, state="RUNNING"))
        marker_fd = os.open(layout.marker, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        stdout_log = _open_log(layout.log_dir / "child.stdout.log")
        stderr_log = _open_log(layout.log_dir / "child.stderr.log")
        signals.install()
        try:
            proc = subprocess.Popen(argv, cwd=str(layout.work_dir), env=env,
                                    stdin=None if args.stdin == "inherit" else subprocess.DEVNULL,
                                    stdout=stdout_log, stderr=stderr_log, start_new_session=True,
                                    pass_fds=(lease.fd, marker_fd))
        except OSError as exc:
            signals.restore()
            receipt["launch_error"] = errno.errorcode.get(exc.errno, "error") if exc.errno else type(exc).__name__
            return _finish(layout, receipt, "FAILED", 1)
        finally:
            stdout_log.close()
            stderr_log.close()
            os.close(marker_fd)
        receipt["timings"]["child_started_at"] = utc_iso()
        layout.event("child_started", pid=proc.pid)
        tracker = Tracker(cfg, proc.pid)
        started = time.monotonic()
        timed_out = False
        cancelled = None
        stop_result = None
        while proc.poll() is None:
            tracker.snapshot()
            if signals.received and not cancelled:
                cancelled = signals.received
                layout.event("cancel_forwarded", signal=cancelled)
                stop_result = stop_process_tree(cfg, proc, tracker, cfg.term_grace_seconds, allow_kill=True)
                break
            if time.monotonic() - started > deadline:
                timed_out = True
                layout.event("deadline_reached", deadline_seconds=deadline)
                stop_result = stop_process_tree(cfg, proc, tracker, cfg.term_grace_seconds, allow_kill=True)
                break
            time.sleep(cfg.sweep_interval_seconds)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            pass
        signals.restore()
        child_code = proc.returncode
        receipt["timings"]["child_exited_at"] = utc_iso()
        receipt["child_exit_code"] = child_code
        receipt["deadline_hit"] = timed_out
        receipt["cancelled"] = cancelled
        receipt["stop"] = stop_result
        layout.event("child_exited", exit_code=child_code)
        result = sweep_settled(cfg, tracker, layout, exclude=set())
        receipt["sweep"] = result
        docker_uncleared = False
        if args.harbor_job_label:
            docker = wait_docker_clear(cfg, args.harbor_job_label)
            receipt["docker"] = docker
            docker_uncleared = not docker["cleared"]
        after = canonical_metadata(cfg)
        receipt["canonical_after"] = after
        receipt["auth_link_after"] = layout.link_state(cfg)
        changed = canonical_changed(before, after)
        receipt["canonical_changed"] = changed
        quarantined = False
        if changed:
            receipt["anomalies"].append({"code": "CANONICAL_CHANGED_BY_CONSUMER"})
            receipt["quarantine"] = write_quarantine(cfg, "CANONICAL_CHANGED_BY_CONSUMER", args.run_id,
                                                     {"role": role, "operation": "run"})
            quarantined = True
        link = receipt["auth_link_after"]
        if not (link.get("symlink") and link.get("target_matches")):
            receipt["anomalies"].append({"code": "RUN_AUTH_LINK_REMOVED_OR_REPLACED"})
        state = _final_state(child_code, timed_out, cancelled, result["survivor_pids"], docker_uncleared,
                             result["inconclusive"], quarantined)
        if state == "UNKNOWN":
            receipt["lease"]["retained_by_keeper_pid"] = spawn_keeper(
                cfg, layout, lease, tracker, args.harbor_job_label if docker_uncleared else None)
            receipt["retry_policy"] = "no_automatic_retry; verify this run's own resources first"
        elif lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, state, _exit_for(state, child_code))
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        signals.restore()
        lease.close()


# --------------------------------------------------------------------------- maintenance

def command_relogin(cfg, args):
    if not args.confirm_relogin:
        raise SupervisorError("relogin_requires_confirm_flag")
    layout = Layout(cfg, "_relogin", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "relogin", layout, "exclusive")
    lease = Lease(cfg)
    try:
        require_codex_pin(cfg)
        receipt["binary_pins"] = cfg.verify_pins()
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        if not acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt):
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        ensure_private_dir(cfg.canonical_auth_home)
        receipt["quarantine_present"] = bool(quarantine_state(cfg))
        before = canonical_metadata(cfg)
        receipt["canonical_before"] = before
        env = base_env(cfg, layout, args.run_id)
        env["CODEX_HOME"] = str(cfg.canonical_auth_home)
        pinned = [str(cfg.codex_bin), "-c", 'cli_auth_credentials_store="file"',
                  "-c", 'forced_login_method="chatgpt"', "-c", "log_dir=" + json.dumps(str(layout.log_dir))]
        login_argv = pinned + ["login", "--device-auth"]
        status_argv = pinned + ["login", "status"]
        receipt.update({"argv": [login_argv, status_argv], "env_keys": sorted(env),
                        "terminal": "passthrough_not_captured"})
        layout.event("relogin_start")
        login = subprocess.run(login_argv, env=env, cwd=str(layout.work_dir), pass_fds=(lease.fd,))
        status = subprocess.run(status_argv, env=env, cwd=str(layout.work_dir), pass_fds=(lease.fd,))
        after = canonical_metadata(cfg)
        receipt.update({"login_exit_code": login.returncode, "status_exit_code": status.returncode,
                        "canonical_after": after, "canonical_changed": canonical_changed(before, after)})
        state = "PASS" if login.returncode == 0 and status.returncode == 0 else "BLOCKED_AUTH"
        ledger(cfg, {"run_id": args.run_id, "decision": "RELOGIN_" + state, "login_exit_code": login.returncode,
                     "status_exit_code": status.returncode, "canonical_before": before, "canonical_after": after})
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, state, STATE_EXIT[state])
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        lease.close()


def command_clear_quarantine(cfg, args):
    if not args.confirm:
        raise SupervisorError("clear_quarantine_requires_confirm_flag")
    reason = (args.reason or "").strip()
    if not reason or len(reason) > 400 or "\n" in reason:
        raise SupervisorError("clear_quarantine_reason_required")
    layout = Layout(cfg, "_maintenance", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "clear-quarantine", layout, "exclusive")
    lease = Lease(cfg)
    try:
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        if not acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt):
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        current = quarantine_state(cfg)
        receipt["quarantine_before"] = current
        if current:
            archived = Path(str(cfg.quarantine_path) + ".cleared-" + utc_stamp() + "-" + args.run_id)
            os.rename(cfg.quarantine_path, archived)
            receipt["archived_marker"] = str(archived)
        receipt["reason"] = redact(reason, 400)
        ledger(cfg, {"run_id": args.run_id, "decision": "QUARANTINE_CLEARED" if current else "NO_QUARANTINE",
                     "reason": redact(reason, 400)})
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, "PASS", EXIT_PASS)
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        lease.close()


def command_status(cfg, _args):
    holders = []
    if cfg.holders_dir.is_dir():
        for path in sorted(cfg.holders_dir.glob("*.json")):
            try:
                with open(path, "rb") as handle:
                    record = json.loads(handle.read(65536))
            except (OSError, ValueError):
                continue
            if record.get("released_at") is None:
                holders.append(record)
    try:
        meta = canonical_preflight(cfg)
        canonical = dict(meta, preflight="ok")
    except SupervisorError as exc:
        canonical = dict(canonical_metadata(cfg), preflight=exc.reason)
    age = None
    if canonical.get("mtime_ns"):
        age = round(max(0.0, time.time() - canonical["mtime_ns"] / 1e9), 3)
    return {"schema": "gptgrep.codex-auth.status.v1", "auth_source_id": cfg.auth_source_id,
            "lease": {"path": str(cfg.lease_path), "probe": probe_lease(cfg)},
            "unreleased_holder_records": holders, "quarantine": quarantine_state(cfg), "canonical": canonical,
            "refresh_policy": {"max_age_seconds": cfg.refresh_max_age_seconds, "canonical_age_seconds": age,
                               "refresh_due": None if age is None else age > cfg.refresh_max_age_seconds},
            "ledger_tail": last_ledger_entries(cfg), "blocker_url": cfg.blocker_url,
            "credential_contents_read_by_supervisor": False}, EXIT_PASS


def command_account_probe(args):
    """Consumer-side, non-model live check: initialize + account/read (refreshToken=false).

    Must run inside `run` (the refresh blocker must already be in the environment).
    Records only the account type and success/error classification."""
    blocker = os.environ.get("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
    try:
        _loopback_url(blocker, "CODEX_REFRESH_TOKEN_URL_OVERRIDE")
    except SupervisorError:
        raise SupervisorError("account_probe_requires_supervised_consumer_env") from None
    if "CODEX_HOME" not in os.environ:
        raise SupervisorError("account_probe_requires_supervised_consumer_env")
    codex_bin = _abs(args.codex_bin, "--codex-bin")
    out = Path(args.out)
    stderr_path = Path(args.stderr_log) if args.stderr_log else Path(os.devnull)
    with open(stderr_path, "ab") as stderr_log:
        proc = subprocess.Popen([str(codex_bin), "app-server", "--listen", "stdio://", "--strict-config"],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr_log)
    client = RpcClient(proc)
    steps = []
    try:
        steps.append(classify_response("initialize", client.request(
            "initialize", {"clientInfo": {"name": "gptgrep_codex_auth_probe", "title": None,
                                          "version": SUPERVISOR_VERSION}}, args.timeout)))
        client.notify("initialized")
        steps.append(classify_response("account/read", client.request("account/read", {}, args.timeout)))
    except RpcError as exc:
        steps.append({"method": "rpc", "ok": False, "error": str(exc)})
    client.close_stdin()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    ok = len(steps) == 2 and steps[1].get("ok") and steps[1].get("account_type") == "chatgpt"
    result = {"schema": "gptgrep.codex-auth.account-probe.v1", "rpc": steps, "app_server_exit": proc.returncode,
              "refresh_requested": False, "model_request_sent": False, "account_ok": bool(ok)}
    write_new_private(out, json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result, (EXIT_PASS if ok else EXIT_BLOCKED_AUTH)


# --------------------------------------------------------------------------- CLI

def _manifest_field(value):
    allowed = {"source_commit", "source_snapshot_manifest_hash", "source_snapshot_hash_kind",
               "gptgrep_entrypoint", "native_codex_version", "harbor_version", "provider", "model",
               "profile", "sandbox", "budget_scope", "run_state_root", "result_root"}
    key, sep, item = value.partition("=")
    if not sep or key not in allowed or len(item) > 200 or any(c in item for c in "\n\r\x00"):
        raise argparse.ArgumentTypeError("manifest field must be KEY=VALUE with an allowed key")
    return key, item


def build_parser():
    parser = argparse.ArgumentParser(prog="codex_auth_supervisor.py", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", default=os.environ.get("GPTGREP_CODEX_AUTH_CONFIG"),
                        help="supervisor config JSON (or GPTGREP_CODEX_AUTH_CONFIG)")
    sub = parser.add_subparsers(dest="command_name", required=True)
    sub.add_parser("status", help="non-secret lease, quarantine, canonical metadata and ledger summary")
    refresh = sub.add_parser("refresh", help="owner refresh under the EXCLUSIVE lease (policy or --force)")
    refresh.add_argument("--run-id", required=True)
    refresh.add_argument("--force", action="store_true", help="refresh even if the canonical file is fresh")
    refresh.add_argument("--wait-seconds", type=float)
    run = sub.add_parser("run", help="run one consumer command under the lease with an isolated CODEX_HOME")
    run.add_argument("--role", required=True)
    run.add_argument("--run-id", required=True)
    run.add_argument("--lease", choices=("exclusive", "shared"), default="exclusive")
    run.add_argument("--deadline-seconds", type=float)
    run.add_argument("--wait-seconds", type=float)
    run.add_argument("--harbor-job-label")
    run.add_argument("--pre-refresh", choices=("never", "if-stale", "always"))
    run.add_argument("--live-request-attempted", choices=("yes", "no", "unknown"), default="unknown")
    run.add_argument("--manifest-field", action="append", type=_manifest_field)
    run.add_argument("--stdin", choices=("null", "inherit"), default="null")
    run.add_argument("--dry-run", action="store_true")
    run.add_argument("command", nargs=argparse.REMAINDER)
    relogin = sub.add_parser("relogin", help="maintenance device re-login under the EXCLUSIVE lease")
    relogin.add_argument("--run-id", required=True)
    relogin.add_argument("--confirm-relogin", action="store_true")
    relogin.add_argument("--wait-seconds", type=float)
    clear = sub.add_parser("clear-quarantine", help="human-confirmed removal of the quarantine marker")
    clear.add_argument("--run-id", required=True)
    clear.add_argument("--confirm", action="store_true")
    clear.add_argument("--reason")
    clear.add_argument("--wait-seconds", type=float)
    probe = sub.add_parser("account-probe", help="inside `run` only: non-model account/read through the "
                                                  "official app-server without refreshing")
    probe.add_argument("--codex-bin", required=True)
    probe.add_argument("--out", required=True)
    probe.add_argument("--stderr-log")
    probe.add_argument("--timeout", type=float, default=60)
    keeper = sub.add_parser("_lease-keeper", help=argparse.SUPPRESS)
    keeper.add_argument("--input", required=True)
    return parser


def main(argv=None):
    os.umask(0o077)
    parser = build_parser()
    args = parser.parse_args(argv)
    if getattr(args, "command", None) and args.command[:1] == ["--"]:
        args.command = args.command[1:]
    if args.command_name == "_lease-keeper":
        return lease_keeper_main(args.input)
    if args.command_name == "account-probe":
        try:
            result, code = command_account_probe(args)
        except SupervisorError as exc:
            result, code = {"state": exc.state, "refusal": {"reason": exc.reason}}, exc.exit_code
        print(json.dumps(result, sort_keys=True, default=str))
        return code
    if not args.config:
        parser.error("--config is required")
    try:
        cfg = Config.load(args.config)
        for name in ("wait_seconds",):
            value = getattr(args, name, None)
            if value is not None and not 0 <= value <= 86400:
                raise SupervisorError("wait_seconds_out_of_range")
        if getattr(args, "deadline_seconds", None) is not None and not 0 < args.deadline_seconds <= 7 * 86400:
            raise SupervisorError("deadline_out_of_range")
        handler = {"status": command_status, "refresh": command_refresh, "run": command_run,
                   "relogin": command_relogin, "clear-quarantine": command_clear_quarantine}[args.command_name]
        result, code = handler(cfg, args)
    except SupervisorError as exc:
        result = {"schema": RUN_SCHEMA, "state": exc.state, "refusal": {"reason": exc.reason, "detail": exc.detail}}
        code = exc.exit_code
    except Exception as exc:  # noqa: BLE001 - report a stable, non-secret classification
        result = {"schema": RUN_SCHEMA, "state": "INTERNAL_ERROR", "error_type": type(exc).__name__}
        code = EXIT_INTERNAL
    print(json.dumps(result, sort_keys=True, default=str))
    sys.stdout.flush()
    return code


if __name__ == "__main__":
    raise SystemExit(main())
