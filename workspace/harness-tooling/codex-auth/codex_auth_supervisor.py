#!/usr/bin/env python3
"""Shared Codex ChatGPT credential lease, refresh owner and per-run isolation.

One persistent ChatGPT login cache (the canonical auth home) serves every
managed consumer. This supervisor:

* serializes managed access with one kernel lease per auth source: a BSD
  ``flock`` on a lock file outside any checkout, bound to the auth source by a
  non-secret marker in the canonical auth home, exclusive or (when enabled)
  shared, held for the whole run. Consumers never receive the lease
  descriptor: any holder of it could release the lock for every holder;
* starts a lease guardian before any auth-using process. The guardian holds an
  inherited lease descriptor, so a cancelled, failing or killed supervisor
  cannot release the lease before the run's processes and labelled containers
  are gone and the canonical credential has been re-checked;
* gives each run its own ``CODEX_HOME``. Its ``auth.json`` is an absolute
  symlink to the canonical file only while the run holds the lease. The home
  also gets a generated pinned ``config.toml`` and an allowlisted environment
  that points the token and revoke endpoints at an unbindable loopback port,
  so consumers can never spend or revoke the shared refresh token;
* lets only the refresh owner (exclusive lease) refresh the credential through
  the official ``codex app-server``. It judges the result from file metadata
  and classifies failures from the owner's own log as permanent or transient;
* re-logs in only when the credential is unusable. The official device login
  runs in an empty staging home, so it never revokes or deletes the existing
  login, and the new ``auth.json`` is installed by an atomic rename after success;
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
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import unicodedata
import urllib.parse

SUPERVISOR_VERSION = "1.3.0"
CONFIG_SCHEMA = "gptgrep.codex-auth.config.v1"
RUN_SCHEMA = "gptgrep.codex-auth.run.v1"
LEDGER_SCHEMA = "gptgrep.codex-auth.ledger.v1"
BINDING_SCHEMA = "gptgrep.codex-auth.lease-binding.v1"
BINDING_NAME = ".gptgrep-codex-auth-binding.json"
# A consumer reports an auth failure by writing this record (in its results
# directory, named by GPTGREP_AUTH_FAILURE_RECORD) AND exiting 77. Exit 77 alone
# is also sysexits EX_NOPERM, so it no longer blocks other consumers.
AUTH_FAILURE_SCHEMA = "gptgrep.codex-auth.auth-failure.v1"
AUTH_FAILURE_RECORD = "auth-failure.json"
AUTH_FAILURE_CLASSES = ("AUTH_UNAUTHORIZED", "AUTH_NOT_LOGGED_IN", "AUTH_UNEXPECTED_ACCOUNT_TYPE",
                        "AUTH_REFRESH_REJECTED")
AUTH_FAILURE_RECORD_MAX_BYTES = 4096
CREDENTIAL_LINK_REPORT_LIMIT = 32

EXIT_PASS = 0
EXIT_USAGE = 64
EXIT_UNAVAILABLE = 69
EXIT_INTERNAL = 70
EXIT_RUN_EXISTS = 73
EXIT_AUTH_BUSY = 75
EXIT_UNKNOWN = 76
EXIT_BLOCKED_AUTH = 77
EXIT_QUARANTINED = 78
EXIT_TIMEOUT = 124
EXIT_CANCELLED = 130
STATE_EXIT = {"PASS": EXIT_PASS, "AUTH_BUSY": EXIT_AUTH_BUSY, "UNKNOWN": EXIT_UNKNOWN,
              "BLOCKED_AUTH": EXIT_BLOCKED_AUTH, "QUARANTINED": EXIT_QUARANTINED,
              "TIMEOUT": EXIT_TIMEOUT, "CANCELLED": EXIT_CANCELLED,
              "REFRESH_UNAVAILABLE": EXIT_UNAVAILABLE, "CONFIG_ERROR": EXIT_USAGE,
              "RELOGIN_NOT_REQUIRED": EXIT_USAGE}

# Port 0 can never have a listener: bind(0) assigns an ephemeral port, and a
# connect to port 0 fails before any byte is sent (EADDRNOTAVAIL on macOS,
# ECONNREFUSED on Linux). A fixed low port such as 9 can be bound by any local
# process (macOS allows unprivileged binds below 1024), and that process would
# then receive the refresh grant, refresh token included.
DEFAULT_BLOCKER_URL = "http://127.0.0.1:0/oauth/token"
# reqwest/hyper-util read the macOS system HTTP(S) proxy but ignore its
# ExceptionsList; NO_PROXY is honoured. Without this a request to the loopback
# blocker can be forwarded, body included, through a local or remote proxy.
LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1"
DEFAULT_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
STRIPPED_ALWAYS = ("OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN")
# The owner's failure classification needs Codex's WARN-level refresh lines
# (the app-server logs only ERROR by default). Every other target stays at ERROR.
OWNER_RUST_LOG = "error,codex_login=warn,codex_app_server=warn"
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
BLOCKER_REPROBE_SECONDS = 2.0

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
# A role either lets the supervisor generate this permission profile (tool
# commands denied the credential paths), declares that its consumer selects its
# own profile, or explicitly accepts unrestricted tool commands.
SUPERVISOR_PERMISSION_PROFILE = "gptgrep_codex_auth"
TOOL_PERMISSION_BASES = (":workspace", ":read-only")
TOOL_PERMISSION_MODES = ("consumer-managed", "unrestricted")
TOOL_PERMISSION_KEYS = frozenset({"extends", "network_enabled", "deny_paths", "read_paths"})
MAX_PROFILE_PATHS = 64
STOP_SCOPES = ("tree", "root-first")
# Codex 0.162.1 arguments that replace the generated profile for model tool
# commands. `--sandbox` and every flag that implies it switch config resolution
# to the legacy syntax, which silently ignores `default_permissions`
# (core/src/config/mod.rs resolve_permission_config_syntax). The others disable
# the sandbox, layer another config file, skip the generated config.toml, run
# threads on another app-server (`--remote`), or apply a raw sandbox state or
# extra socket access (`codex sandbox`). `--full-auto` (absent from 0.162.1) is
# refused for other builds.
CODEX_PROFILE_OVERRIDE_FLAGS = frozenset({
    "--sandbox", "-s", "--dangerously-bypass-approvals-and-sandbox", "--yolo", "--full-auto",
    "--approve-for-me", "--not-so-yolo", "--profile", "-p", "--ignore-user-config", "--remote",
    "--sandbox-state-json", "--sandbox-state-readable-root", "--allow-unix-socket"})
CODEX_CONFIG_FLAGS = frozenset({"-c", "--config"})
# First segment of a -c key. `projects` sets project trust, which loads a
# repository's .codex/config.toml above the generated one (that layer may set
# every key here); `mcp_servers` starts servers outside the sandbox;
# `approvals_reviewer` lets a model approve permission escalations.
CODEX_PROFILE_CONFIG_KEYS = frozenset({"sandbox_mode", "default_permissions", "permissions",
                                       "sandbox_workspace_write", "projects", "mcp_servers",
                                       "approvals_reviewer"})
# `-P/--permission-profile NAME` equals `-c default_permissions=NAME`; only the
# supervisor's own profile may be named.
CODEX_PERMISSION_PROFILE_FLAGS = frozenset({"-P", "--permission-profile", "--permissions-profile"})
# Keys the supervisor pins in the generated config.toml (auth store, login method,
# features, telemetry, history, log_dir) and config that runs commands outside the
# sandbox (hooks, plugins and their marketplaces; notify is pinned). For `features`
# only the pinned flags (and the whole table) count; `--enable` of a pinned flag too.
CODEX_PINNED_CONFIG_KEYS = frozenset(set(PINNED_CODEX_CONFIG) - {"features"} |
                                     {"log_dir", "hooks", "plugins", "marketplaces"})
CODEX_PINNED_FEATURES = frozenset(PINNED_CODEX_CONFIG["features"])
# The working directory and every added directory become writable project roots.
# A root inside a denied path is the more specific entry, so it reopens that path.
CODEX_WORKSPACE_ROOT_FLAGS = frozenset({"-C", "--cd", "--add-dir"})
CODEX_INSPECTED_VALUE_FLAGS = (CODEX_CONFIG_FLAGS | CODEX_PERMISSION_PROFILE_FLAGS | CODEX_WORKSPACE_ROOT_FLAGS |
                               {"--enable"})
CODEX_BOOLEAN_SHORT_FLAGS = frozenset("hVy")
# Codex executables: `codex`, release binaries (`codex-aarch64-apple-darwin`) and
# shims (`codex.js`), in any letter case; not `codex_auth_supervisor.py`.
CODEX_NAME_RE = re.compile(r"codex([-.].*)?")

# Owner failure classification from Codex 0.162.1 log lines
# (login/src/auth/manager.rs request_chatgpt_token_refresh and auth();
# app-server account_processor refresh_token_if_requested).
PERMANENT_FAILURE_CLASSES = ("REFRESH_TOKEN_REUSED", "REFRESH_TOKEN_EXPIRED", "REFRESH_TOKEN_REVOKED",
                             "INVALID_GRANT", "ACCOUNT_MISMATCH", "REFRESH_REJECTED")
TRANSIENT_FAILURE_CLASSES = ("RATE_LIMITED", "AUTHORITY_SERVER_ERROR", "TRANSPORT", "TRANSIENT_OTHER")
_PERMANENT_CODES = {"refresh_token_reused": "REFRESH_TOKEN_REUSED",
                    "refresh_token_expired": "REFRESH_TOKEN_EXPIRED",
                    "refresh_token_invalidated": "REFRESH_TOKEN_REVOKED",
                    "invalid_grant": "INVALID_GRANT"}
_PERMANENT_MESSAGES = (("refresh token was already used", "REFRESH_TOKEN_REUSED"),
                       ("refresh token has expired", "REFRESH_TOKEN_EXPIRED"),
                       ("refresh token was revoked", "REFRESH_TOKEN_REVOKED"),
                       ("signed in to another account", "ACCOUNT_MISMATCH"),
                       ("access token could not be refreshed", "REFRESH_REJECTED"))
_TRANSPORT_MARKERS = ("error sending request", "connection refused", "timed out", "dns error",
                      "tcp connect error", "connection reset")
_ERROR_CODE_RE = re.compile(r'error_code: some\("([a-z0-9_]+)"\)')
_STATUS_RE = re.compile(r"status=(\d{3})|failed to refresh token: (\d{3}) ")
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
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


def errno_name(exc):
    return errno.errorcode.get(exc.errno, "error") if getattr(exc, "errno", None) else type(exc).__name__


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


def read_small_json(path, limit=65536):
    """Read a small supervisor-owned JSON record (never a credential)."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as handle:
        return json.loads(handle.read(limit))


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


def fsync_directory(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    except OSError:
        return False
    try:
        os.fsync(fd)
        return True
    except OSError:
        return False
    finally:
        os.close(fd)


# --------------------------------------------------------------------------- config

def _abs(value, key):
    if not isinstance(value, str) or not value.startswith("/") or "\x00" in value or "\n" in value:
        raise SupervisorError("config_path_not_absolute", detail=key)
    # normpath keeps a leading "//" (POSIX leaves it implementation-defined); refuse it.
    if os.path.normpath(value) != value or value.startswith("//") or ".." in Path(value).parts:
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
    try:
        parsed.port
    except ValueError:
        raise SupervisorError("config_url_invalid", detail=key) from None
    host = parsed.hostname
    if host != "localhost":
        try:
            if not ipaddress.ip_address(host).is_loopback:
                raise SupervisorError("config_url_not_loopback", detail=key)
        except ValueError:
            raise SupervisorError("config_url_not_loopback", detail=key) from None
    return value


def _sibling_url(url, path):
    parsed = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, path, "", ""))


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


def _profile_paths(value, key, reason):
    """Up to MAX_PROFILE_PATHS absolute, normalized paths for the generated profile."""
    if not isinstance(value, list) or len(value) > MAX_PROFILE_PATHS:
        raise SupervisorError(reason, detail=key)
    return list(dict.fromkeys(str(_abs(item, key)) for item in value))


def _tool_permissions(value, key):
    """Required per role: who keeps model tool commands away from the credential."""
    if value is None:
        raise SupervisorError("config_role_tool_permissions_required", detail=key)
    if value in TOOL_PERMISSION_MODES:
        return value
    if isinstance(value, dict) and set(value) <= TOOL_PERMISSION_KEYS:
        extends = value.get("extends", ":workspace")
        network = value.get("network_enabled", False)
        if extends in TOOL_PERMISSION_BASES and isinstance(network, bool):
            paths = {name: _profile_paths(value.get(name, []), key + ".tool_permissions." + name,
                                          "config_role_tool_permissions_invalid")
                     for name in ("deny_paths", "read_paths")}
            denied = {_path_key(path) for path in paths["deny_paths"]}
            for path in paths["read_paths"]:
                if _path_key(path) in denied:
                    raise SupervisorError("config_role_read_path_equals_deny_path",
                                          detail=key + ".tool_permissions.read_paths:" + path)
            return {"extends": extends, "network_enabled": network, **paths}
    raise SupervisorError("config_role_tool_permissions_invalid", detail=key)


def _path_key(path):
    """Comparison form of a path: one leading slash, canonical decomposition and
    case folding. macOS volumes are normally case- and normalization-insensitive,
    while Codex matches entries by spelling and then resolves a root's on-disk
    spelling, so a differently spelled path to the same file must count as the same
    path. On a case-sensitive volume this over-refuses, which is acceptable."""
    text = str(path)
    if text.startswith("//"):
        text = "/" + text.lstrip("/")
    return unicodedata.normalize("NFD", unicodedata.normalize("NFD", text).casefold())


def _reopens(path, denied, resolve=False):
    """True when a `read` entry (or a writable root) at `path` would reopen the
    denied path `denied`: the same path or one inside it (Codex 0.162.1 applies
    the most specific entry), in any spelling. With `resolve`, symlinks in either
    path are resolved as well."""
    paths, denies = {str(path)}, {str(denied)}
    if resolve:
        paths.add(os.path.realpath(path))
        denies.add(os.path.realpath(denied))
    return any(is_within(_path_key(p), _path_key(d)) for p in paths for d in denies)


def _is_run_path(path, runs_root, parts, resolve=False):
    """True for RUNS_ROOT/<role>/<run-id>/<parts...>, or a path inside it, for every
    role and run id and in any spelling (with `resolve`, also once symlinks are
    resolved)."""
    paths, roots = {str(path)}, {str(runs_root)}
    if resolve:
        paths.add(os.path.realpath(path))
        roots.add(os.path.realpath(runs_root))
    for candidate in paths:
        for root in roots:
            key, base = _path_key(candidate), _path_key(root)
            if is_within(key, base) and Path(key).relative_to(base).parts[2:2 + len(parts)] == tuple(parts):
                return True
    return False


def _is_run_auth_link(path, runs_root, resolve=False):
    """True for RUNS_ROOT/<role>/<run-id>/codex-home/auth.json, or a path inside it."""
    return _is_run_path(path, runs_root, ("codex-home", "auth.json"), resolve)


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
        self.binding_path = self.canonical_auth_home / BINDING_NAME
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
        # Denied to model tool commands of every role with the generated profile.
        self.extra_denied_paths = _profile_paths(raw.get("extra_denied_paths", []), "extra_denied_paths",
                                                 "config_extra_denied_paths")
        self.blocker_url = _loopback_url(raw.get("blocker_url", DEFAULT_BLOCKER_URL), "blocker_url")
        self.revoke_blocker_url = _sibling_url(self.blocker_url, "/oauth/revoke")
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
        relogin = raw.get("relogin", {})
        if not isinstance(relogin, dict) or set(relogin) - {"experimental_issuer"}:
            raise SupervisorError("config_relogin")
        issuer = relogin.get("experimental_issuer")
        # Test-only: a loopback fake device-code issuer for qualification.
        self.relogin_issuer = None if issuer is None else _loopback_url(issuer, "relogin.experimental_issuer")
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
            grace = role.get("term_grace_seconds")
            role_path = role.get("path_env")
            if role_path is not None and (not isinstance(role_path, str) or
                                          not all(p.startswith("/") for p in role_path.split(":"))):
                raise SupervisorError("config_role_path_env", detail=str(name))
            scope = role.get("stop_scope", "tree")
            if scope not in STOP_SCOPES:
                raise SupervisorError("config_role_stop_scope", detail=str(name))
            self.roles[name] = {
                "path_env": role_path,
                "passthrough_env": _env_names(role.get("passthrough_env", []), "roles." + name, allow_openrouter),
                "allow_openrouter": allow_openrouter,
                "deadline_seconds": None if deadline is None else _number(
                    deadline, "roles." + name + ".deadline_seconds", 1, 7 * 86400),
                "term_grace_seconds": None if grace is None else _number(
                    grace, "roles." + name + ".term_grace_seconds", 0, 3600),
                "stop_scope": scope,
                "codex_config": _codex_config_extra(role.get("codex_config"), "roles." + name + ".codex_config"),
                "tool_permissions": _tool_permissions(role.get("tool_permissions"), "roles." + name),
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
        self.auth_block_path = Path(str(self.lease_path) + ".auth-blocked")
        self.ledger_path = Path(str(self.lease_path) + ".ledger.jsonl")
        if is_within(self.runs_root, self.canonical_auth_home) or is_within(self.canonical_auth_home, self.runs_root):
            raise SupervisorError("config_runs_root_overlaps_auth_home")
        if is_within(self.lease_path, self.canonical_auth_home) or is_within(self.lease_path, self.runs_root):
            raise SupervisorError("config_lease_inside_auth_home_or_runs_root")
        # Tool commands are denied the lease directory (markers, ledger); it must not hold run directories.
        if is_within(self.runs_root, self.lease_path.parent):
            raise SupervisorError("config_lease_directory_contains_runs_root")
        for name, role in self.roles.items():
            if isinstance(role["tool_permissions"], dict):
                for path in role["tool_permissions"]["read_paths"]:
                    self._check_read_path(name, path)

    def _check_read_path(self, role, path):
        """A role's read carve-out may open a path below one of its own denies, but
        never the canonical auth home, any run's auth link, the lease directory or
        an `extra_denied_paths` entry (equal to or inside any of them)."""
        detail = "roles." + role + ".tool_permissions.read_paths:" + path
        if _reopens(path, self.canonical_auth_home) or _reopens(path, self.lease_path.parent) \
                or _is_run_auth_link(path, self.runs_root):
            raise SupervisorError("config_role_read_path_reopens_credential", detail=detail)
        if any(_reopens(path, denied) for denied in self.extra_denied_paths):
            raise SupervisorError("config_role_read_path_reopens_extra_deny", detail=detail)

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
    except OSError as exc:
        return {"exists": None, "error": errno_name(exc)}
    return {"exists": True, "regular": stat.S_ISREG(info.st_mode), "symlink": stat.S_ISLNK(info.st_mode),
            "mode": oct(stat.S_IMODE(info.st_mode)), "uid_matches": info.st_uid == os.getuid(),
            "size": info.st_size, "mtime_ns": info.st_mtime_ns, "ino": info.st_ino, "dev": info.st_dev,
            "nlink": info.st_nlink}


def canonical_home_check(cfg):
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
    return info


def canonical_preflight(cfg):
    canonical_home_check(cfg)
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
    # Codex saves in place and a re-login installs by rename, so the file always has
    # one link. Another link is a credential copy at an unknown place (it survives a
    # rotation, and an archive or tool reading it reads the credential).
    if meta["nlink"] != 1:
        raise SupervisorError("canonical_auth_hardlinked", "QUARANTINED", EXIT_QUARANTINED,
                              detail="nlink=%d" % meta["nlink"])
    return meta


def preflight_or_quarantine(cfg, run_id, operation, receipt):
    """The canonical preflight of a command about to use the credential. An extra hard
    link also writes the quarantine marker: its location is unknown, so a human must
    find and remove it (README, failure recovery)."""
    try:
        return canonical_preflight(cfg)
    except SupervisorError as exc:
        if exc.reason == "canonical_auth_hardlinked":
            receipt["quarantine"] = write_quarantine(cfg, "CANONICAL_HARDLINKED", run_id, {
                "operation": operation, "phase": "preflight",
                "canonical_nlink": canonical_metadata(cfg).get("nlink")})
        raise


def canonical_changed(before, after):
    keys = ("exists", "mtime_ns", "size", "ino", "dev")
    return any(before.get(k) != after.get(k) for k in keys)


def canonical_identities(*metas):
    """(st_dev, st_ino) of the canonical file in the given lstat records."""
    return {(meta["dev"], meta["ino"]) for meta in metas if meta and meta.get("exists") and "ino" in meta}


def _symlink_reaches_credential(cfg, dirpath, name, dirfd, identities):
    """A symlink through which the credential can be read: it resolves (through any
    chain of links) to the canonical file, or to a directory that contains it, which an
    archive that follows symlinks would copy. stat/realpath only; nothing is opened."""
    try:
        target = os.stat(name, dir_fd=dirfd)
        if (target.st_dev, target.st_ino) in identities:
            return True
    except OSError:
        pass
    resolved = os.path.realpath(os.path.join(dirpath, name))
    return is_within(_path_key(cfg.canonical_auth_file), _path_key(resolved))


def _remove_credential_link(cfg, dirfd, name, info, kind):
    """Remove one extra link, never the credential. The entry is unlinked by name in its
    own directory descriptor, so no symlink in its path can redirect the removal. A hard
    link is removed only while the canonical path still names the same file and so keeps
    another link: a consumer that moved or replaced the canonical file may have left the
    only copy of the original here, which stays for review."""
    item = {"kind": kind, "removed": False}
    if kind == "hardlink":
        try:
            canonical = os.lstat(cfg.canonical_auth_file)
        except OSError:
            canonical = None
        if canonical is None or not stat.S_ISREG(canonical.st_mode) or canonical.st_nlink < 2 or \
                (canonical.st_dev, canonical.st_ino) != (info.st_dev, info.st_ino):
            item["kept"] = "canonical_path_keeps_no_other_link"
            return item
    try:
        current = os.stat(name, dir_fd=dirfd, follow_symlinks=False)
        if (current.st_dev, current.st_ino, stat.S_IFMT(current.st_mode)) != \
                (info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode)):
            item["kept"] = "entry_changed"
            return item
        os.unlink(name, dir_fd=dirfd)
        item["removed"] = True
    except OSError as exc:
        item["error"] = errno_name(exc)
    return item


def credential_links(cfg, layout, identities):
    """Find and remove hard links and symlinks to the canonical credential below the run
    directory, other than the run's own auth link (`codex-home/auth.json`, which
    `link_state` judges). A consumer can create them through its link, and an archive of
    `results/` would then carry the credential. Metadata only (lstat, stat, readlink via
    realpath): nothing is opened. The walk never follows a symlink (`os.fwalk` re-checks
    each directory it enters). Returns a non-secret summary with run-relative paths."""
    top = str(layout.run_dir)
    links, errors = [], []
    try:
        for dirpath, dirnames, filenames, dirfd in os.fwalk(top, follow_symlinks=False,
                                                            onerror=lambda exc: errors.append(errno_name(exc))):
            rel = os.path.relpath(dirpath, top)
            for name in dirnames + filenames:
                if rel == "codex-home" and name == "auth.json":
                    continue
                try:
                    info = os.stat(name, dir_fd=dirfd, follow_symlinks=False)
                except OSError as exc:
                    errors.append(errno_name(exc))
                    continue
                if stat.S_ISREG(info.st_mode) and (info.st_dev, info.st_ino) in identities:
                    kind = "hardlink"
                elif stat.S_ISLNK(info.st_mode) and _symlink_reaches_credential(cfg, dirpath, name, dirfd,
                                                                                identities):
                    kind = "symlink"
                else:
                    continue
                item = _remove_credential_link(cfg, dirfd, name, info, kind)
                item["path"] = os.path.normpath(os.path.join(rel, name))
                links.append(item)
                if name in dirnames:
                    dirnames.remove(name)  # a symlink is never entered; do not revisit a removed one
    except OSError as exc:
        errors.append(errno_name(exc))
    return {"found": len(links), "removed": sum(1 for item in links if item["removed"]),
            "links": links[:CREDENTIAL_LINK_REPORT_LIMIT], "scan_errors": len(errors),
            "canonical_nlink_after": canonical_metadata(cfg).get("nlink")}


def refresh_due(cfg):
    """True when the canonical file is older than the policy age or a consumer reported an auth failure."""
    if auth_block_state(cfg):
        return True
    meta = canonical_metadata(cfg)
    if not meta.get("mtime_ns"):
        return True
    return time.time() - meta["mtime_ns"] / 1e9 > cfg.refresh_max_age_seconds


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
    the lock when the last descriptor sharing this open file description closes.

    Any holder of that open file description can release the lock for all holders
    with flock(LOCK_UN). It is therefore shared only with the lease guardian and
    the pinned official owner and re-login processes, never with a consumer (whose
    model tool commands would inherit it)."""

    def __init__(self, cfg):
        self.cfg = cfg
        self.path = cfg.lease_path
        self.fd = None
        self.mode = None
        self.sidecar = None

    def open(self, create=True):
        ensure_private_dir(self.path.parent, create=create)
        flags = os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC | (os.O_CREAT if create else 0)
        try:
            fd = os.open(self.path, flags, 0o600)
        except FileNotFoundError:
            raise SupervisorError("lease_file_missing", detail="a bound lease file was removed; see rebind-lease") \
                from None
        except OSError as exc:
            if exc.errno == errno.ELOOP:
                raise SupervisorError("lease_path_is_symlink") from None
            raise SupervisorError("lease_open_failed", detail=errno_name(exc)) from None
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

    def acquire(self, mode, wait_seconds, poll=0.05, should_stop=None):
        operation = fcntl.LOCK_EX if mode == "exclusive" else fcntl.LOCK_SH
        start = time.monotonic()
        deadline = start + wait_seconds
        while True:
            try:
                fcntl.flock(self.fd, operation | fcntl.LOCK_NB)
                self.mode = mode
                return True, round(time.monotonic() - start, 3)
            except BlockingIOError:
                if time.monotonic() >= deadline or (should_stop and should_stop()):
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
            record = read_small_json(self.sidecar)
        except (OSError, ValueError):
            record = {}
        record.update(fields)
        try:
            write_json_atomic(self.sidecar, record)
        except OSError:
            pass

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


# --------------------------------------------------------------------------- lease binding

def read_binding(cfg):
    """The non-secret marker in the canonical auth home that names its one lease file."""
    try:
        info = os.lstat(cfg.binding_path)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise SupervisorError("lease_binding_unreadable", detail=errno_name(exc)) from None
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > 65536:
        raise SupervisorError("lease_binding_unsafe")
    try:
        record = read_small_json(cfg.binding_path)
    except (OSError, ValueError):
        raise SupervisorError("lease_binding_unreadable") from None
    if not isinstance(record, dict) or record.get("schema") != BINDING_SCHEMA or \
            not {"auth_source_id", "lease_path", "lease_dev", "lease_ino"} <= set(record):
        raise SupervisorError("lease_binding_unreadable")
    return record


def _binding_record(cfg, lease_fd):
    info = os.fstat(lease_fd)
    return {"schema": BINDING_SCHEMA, "auth_source_id": cfg.auth_source_id, "lease_path": str(cfg.lease_path),
            "lease_dev": info.st_dev, "lease_ino": info.st_ino, "bound_at": utc_iso()}


def verify_binding_identity(cfg, binding):
    if binding["auth_source_id"] != cfg.auth_source_id or binding["lease_path"] != str(cfg.lease_path):
        raise SupervisorError("lease_binding_mismatch",
                              detail="this auth source is bound to another lease path; every config must use it")


def bind_or_verify(cfg, lease_fd, binding):
    """Every checkout using this auth source must use the same lease file.

    The first managed command records the lease path and the lock file's
    identity next to the credential; later commands with another lease path, or
    after the lock file was deleted and recreated (which would split the lock),
    are refused."""
    info = os.fstat(lease_fd)
    created = False
    if binding is None:
        canonical_home_check(cfg)
        record = _binding_record(cfg, lease_fd)
        try:
            write_new_private(cfg.binding_path, json.dumps(record, indent=2, sort_keys=True) + "\n")
            binding, created = record, True
        except FileExistsError:
            binding = read_binding(cfg)
    verify_binding_identity(cfg, binding)
    if (binding["lease_dev"], binding["lease_ino"]) != (info.st_dev, info.st_ino):
        raise SupervisorError("lease_file_replaced", detail="the bound lease file was replaced; see rebind-lease")
    return {"lease_path": binding["lease_path"], "created": created, "bound_at": binding.get("bound_at")}


# --------------------------------------------------------------------------- markers and ledger

def read_marker(path):
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return None
    except OSError:
        return {"present": True, "unreadable": True}
    if not stat.S_ISREG(info.st_mode):
        return {"present": True, "unreadable": True}
    try:
        record = read_small_json(path)
    except (OSError, ValueError):
        record = {"unreadable": True}
    if not isinstance(record, dict):
        record = {"unreadable": True}
    record["present"] = True
    return record


def quarantine_state(cfg):
    return read_marker(cfg.quarantine_path)


def auth_block_state(cfg):
    return read_marker(cfg.auth_block_path)


def write_marker(path, schema, reason, run_id, details, clear_with):
    record = {"schema": schema, "reason": reason, "run_id": run_id, "created_at": utc_iso(),
              "details": details, "clear_with": clear_with}
    try:
        write_new_private(path, json.dumps(record, indent=2, sort_keys=True) + "\n")
        return {"created": True, "path": str(path), "reason": reason}
    except FileExistsError:
        return {"created": False, "already_present": True, "path": str(path), "reason": reason}


def write_quarantine(cfg, reason, run_id, details):
    return write_marker(cfg.quarantine_path, "gptgrep.codex-auth.quarantine.v1", reason, run_id, details,
                        "codex_auth_supervisor.py clear-quarantine --run-id ID --confirm --reason TEXT")


def write_auth_block(cfg, reason, run_id, details):
    return write_marker(cfg.auth_block_path, "gptgrep.codex-auth.auth-block.v1", reason, run_id, details,
                        "a successful owner refresh or relogin, or "
                        "codex_auth_supervisor.py clear-quarantine --auth-block --run-id ID --confirm --reason TEXT")


def archive_marker(path, run_id):
    if not os.path.lexists(path):
        return None
    archived = Path(str(path) + ".cleared-" + utc_stamp() + "-" + run_id)
    os.rename(path, archived)
    return str(archived)


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
        self.auth_failure_record = self.results / AUTH_FAILURE_RECORD
        self.auth_link = self.codex_home / "auth.json"
        self.marker = self.state_dir / "lease-holder.marker"
        self.events = self.state_dir / "supervisor-events.jsonl"
        self.linked = False
        self.mode_restored = False

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

    def create(self, cfg, extra_codex_config, tool_permissions=None, config_text=None):
        """Directories, generated config and marker. The auth link comes later,
        only once the run holds the lease (link_auth). `config_text` is a config
        already rendered (and checked) for this layout before anything was created."""
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
        text = config_text if config_text is not None else render_codex_config(cfg, self, extra_codex_config,
                                                                                tool_permissions)
        write_new_private(self.codex_home / "config.toml", text)
        fd = os.open(self.marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        os.close(fd)
        return text

    def link_auth(self, cfg):
        os.symlink(str(cfg.canonical_auth_file), self.auth_link)
        self.linked = True

    def _home_is_physical(self):
        try:
            info = os.lstat(self.codex_home)
        except OSError:
            return False
        return stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode) and info.st_uid == os.getuid()

    def _restore_home_mode(self):
        """A consumer can chmod its own CODEX_HOME; the directory belongs to the supervisor."""
        if not self._home_is_physical():
            return False
        try:
            os.chmod(self.codex_home, 0o700)
        except OSError:
            return False
        self.mode_restored = True
        return True

    def link_state(self, cfg):
        for attempt in (0, 1):
            try:
                info = os.lstat(self.auth_link)
                break
            except FileNotFoundError:
                return {"present": False}
            except PermissionError:
                if attempt == 0 and self._restore_home_mode():
                    continue
                return {"present": None, "error": "EACCES"}
            except OSError as exc:
                return {"present": None, "error": errno_name(exc)}
        if not stat.S_ISLNK(info.st_mode):
            return {"present": True, "symlink": False, "regular": stat.S_ISREG(info.st_mode),
                    "home_physical": self._home_is_physical()}
        try:
            target = os.readlink(self.auth_link)
        except OSError as exc:
            return {"present": True, "symlink": True, "target_matches": None, "error": errno_name(exc)}
        return {"present": True, "symlink": True, "target_matches": target == str(cfg.canonical_auth_file),
                "home_physical": self._home_is_physical()}

    def release_auth_link(self, cfg):
        """Remove this run's link to the canonical credential; never the credential.

        A regular file left in its place (a per-run login or copy) is kept for
        human review and reported, because deleting it would erase evidence."""
        state = self.link_state(cfg)
        if state.get("symlink") and state.get("home_physical"):
            try:
                os.unlink(self.auth_link)
                self.linked = False
                return dict(state, released=True)
            except OSError as exc:
                return dict(state, released=False, error=errno_name(exc))
        if not state.get("present"):
            self.linked = False if state.get("present") is False else self.linked
        return dict(state, released=False)

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


def permission_denials(cfg, layout):
    """Paths model tool commands are always denied in the supervisor permission
    profile: the canonical auth home, this run's auth link, the lease directory
    (lock, markers, ledger) and the configured `extra_denied_paths`."""
    base = [str(cfg.canonical_auth_home), str(layout.auth_link), str(cfg.lease_path.parent)]
    return base + [path for path in cfg.extra_denied_paths if path not in base]


def profile_filesystem(cfg, layout, tool_permissions):
    """Filesystem table of the generated profile: the supervisor's denies, the
    role's denies, then the role's read carve-outs. Codex 0.162.1 applies the most
    specific entry, so a carve-out can open a path below a role deny. Config load
    refuses one that reopens a supervisor deny; this re-checks against the run's
    actual link with symlinks resolved, before anything is written or launched."""
    denied = permission_denials(cfg, layout)
    entries = {path: "deny" for path in denied + tool_permissions["deny_paths"]}
    for path in tool_permissions["read_paths"]:
        if any(_reopens(path, item, resolve=True) for item in denied) or \
                _is_run_auth_link(path, cfg.runs_root, resolve=True):
            raise SupervisorError("role_read_path_resolves_into_denied_path", detail=path)
        entries[path] = "read"
    return entries


def tool_credential_access(cfg, layout, tool_permissions):
    if isinstance(tool_permissions, dict):
        filesystem = profile_filesystem(cfg, layout, tool_permissions)
        return {"policy": "supervisor_profile", "profile": SUPERVISOR_PERMISSION_PROFILE,
                "extends": tool_permissions["extends"], "network_enabled": tool_permissions["network_enabled"],
                "denied_paths": [path for path, access in filesystem.items() if access == "deny"],
                "read_paths": [path for path, access in filesystem.items() if access == "read"],
                "applies_to": "tool commands of consumers that do not select another permission profile"}
    if tool_permissions == "consumer-managed":
        return {"policy": "consumer_managed", "verified_by_supervisor": False}
    return {"policy": "unrestricted", "tool_commands_can_read_credential": True}


def codex_config_tree(cfg, layout, extra, tool_permissions=None):
    tree = json.loads(json.dumps(PINNED_CODEX_CONFIG))
    merged = dict(cfg.codex_config)
    merged.update(extra or {})
    root = {k: v for k, v in tree.items() if not isinstance(v, dict)}
    root.update(merged)
    root["log_dir"] = str(layout.log_dir)
    tables = {k: v for k, v in tree.items() if isinstance(v, dict)}
    if isinstance(tool_permissions, dict):
        root["default_permissions"] = SUPERVISOR_PERMISSION_PROFILE
        tables["permissions"] = {SUPERVISOR_PERMISSION_PROFILE: {
            "extends": tool_permissions["extends"],
            "filesystem": profile_filesystem(cfg, layout, tool_permissions),
            "network": {"enabled": bool(tool_permissions["network_enabled"])}}}
    result = dict(sorted(root.items()))
    result.update(tables)
    return result


def render_codex_config(cfg, layout, extra, tool_permissions=None):
    header = ("# Generated by codex_auth_supervisor " + SUPERVISOR_VERSION +
              "; per-run CODEX_HOME. Do not copy the canonical config.toml here.\n")
    return header + render_toml(codex_config_tree(cfg, layout, extra, tool_permissions))


def base_env(cfg, layout, run_id):
    env = {"PATH": cfg.path_env, "LANG": cfg.locale, "LC_ALL": cfg.locale,
           "HOME": str(layout.home), "TMPDIR": str(layout.tmp),
           "CODEX_SQLITE_HOME": str(layout.state_dir),
           "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED": "1",
           "NO_PROXY": LOOPBACK_NO_PROXY, "no_proxy": LOOPBACK_NO_PROXY,
           "GPTGREP_AUTH_RUN_ID": run_id, "GPTGREP_AUTH_SOURCE_ID": cfg.auth_source_id}
    env.update({k: str(v) for k, v in layout.xdg().items()})
    return env


def build_consumer_env(cfg, role, layout, parent_env):
    """Allowlist only: nothing is inherited unless named for the role. No lease
    descriptor is named or passed: the lease guardian holds the lease for the run."""
    spec = cfg.role(role)
    env = base_env(cfg, layout, layout.run_id)
    if spec["path_env"]:
        env["PATH"] = spec["path_env"]
    env["CODEX_HOME"] = str(layout.codex_home)
    env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = cfg.blocker_url
    # Codex derives the revoke endpoint from the refresh override when unset; make it explicit.
    env["CODEX_REVOKE_TOKEN_URL_OVERRIDE"] = cfg.revoke_blocker_url
    # Where a consumer writes its auth-failure record before exiting 77.
    env["GPTGREP_AUTH_FAILURE_RECORD"] = str(layout.auth_failure_record)
    passed = []
    for name in spec["passthrough_env"]:
        if name in parent_env:
            env[name] = parent_env[name]
            passed.append(name)
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
    # The owner never logs out; any revoke must fail instead of reaching the authority.
    env["CODEX_REVOKE_TOKEN_URL_OVERRIDE"] = cfg.revoke_blocker_url
    env["RUST_LOG"] = OWNER_RUST_LOG
    for name in STRIPPED_ALWAYS + ("OPENROUTER_API_KEY",):
        env.pop(name, None)
    return env


def build_relogin_env(cfg, layout, codex_home):
    """The device login reaches only the issuer; refresh and revoke hit the blocker."""
    env = base_env(cfg, layout, layout.run_id)
    env["CODEX_HOME"] = str(codex_home)
    env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = cfg.blocker_url
    env["CODEX_REVOKE_TOKEN_URL_OVERRIDE"] = cfg.revoke_blocker_url
    return env


def substitute(argv, layout):
    values = {"{" + k + "}": str(v) for k, v in layout.placeholders().items()}
    result = []
    for arg in argv:
        for key, value in values.items():
            arg = arg.replace(key, value)
        result.append(arg)
    return result


def _is_codex_name(path):
    return CODEX_NAME_RE.fullmatch(os.path.basename(str(path)).casefold()) is not None


def invokes_codex(cfg, executable, path_env, cwd):
    """The consumer executable is Codex: the configured binary (the same file, also
    through a symlink or hard link), or a program whose name or symlink target's
    name is `codex` or starts with `codex-` or `codex.`, in any letter case. It is
    resolved the way the launch will resolve it: a bare name through the child's
    PATH, a relative path against the child's working directory. An executable that
    cannot be resolved is treated as Codex, so it is inspected (it cannot launch)."""
    if _is_codex_name(executable):
        return True
    if "/" in executable:
        resolved = os.path.normpath(os.path.join(str(cwd), executable))
    else:
        resolved = shutil.which(executable, path=path_env)
        if resolved is None:
            return True
    try:
        info = os.stat(resolved)
    except OSError:
        return True
    if _is_codex_name(os.path.realpath(resolved)):
        return True
    try:
        pinned = os.stat(cfg.codex_bin)
    except OSError:
        return False
    return (info.st_dev, info.st_ino) == (pinned.st_dev, pinned.st_ino)


def _codex_options(argv):
    """(option, value) pairs of a Codex command line, up to `--`. A long option keeps
    its inline `=value`; a short cluster ends at its first letter that is not a
    boolean flag, and the rest of the cluster is that option's value. The value of
    -c/--config, -P/--permission-profile, -C/--cd and --add-dir is the next argument
    when not inline, unless that argument starts with `-` (clap then reports a
    missing value); any other argument is inspected on its own."""
    options = []
    index = 1
    while index < len(argv):
        token = argv[index]
        index += 1
        if token == "--":
            break
        if token.startswith("--"):
            name, sep, value = token.partition("=")
            inline = value if sep else None
        elif token.startswith("-") and len(token) > 1:
            letters = token[1:]
            pos = 0
            while pos < len(letters) - 1 and letters[pos] in CODEX_BOOLEAN_SHORT_FLAGS:
                pos += 1
            name, rest = "-" + letters[pos], letters[pos + 1:]
            inline = (rest[1:] if rest.startswith("=") else rest) if rest else None
        else:
            continue
        if inline is None and name in CODEX_INSPECTED_VALUE_FLAGS and index < len(argv) \
                and not argv[index].startswith("-"):
            inline = argv[index]
            index += 1
        options.append((name, inline))
    return options


def codex_profile_override(argv):
    """The first argument of a Codex command line that would replace the generated
    permission profile or undo a pinned key for model tool commands (a flag name,
    `-c KEY` or `--enable FEATURE`), or None."""
    for name, value in _codex_options(argv):
        if name in CODEX_PROFILE_OVERRIDE_FLAGS:
            return name
        if name in CODEX_CONFIG_FLAGS:
            segments = [part.strip().strip("\"'") for part in (value or "").partition("=")[0].split(".")]
            key = segments[0]
            if key in CODEX_PROFILE_CONFIG_KEYS or key in CODEX_PINNED_CONFIG_KEYS:
                return name + " " + key
            if key == "features" and (len(segments) == 1 or segments[1] in CODEX_PINNED_FEATURES):
                return name + " " + ".".join(segments[:2])
        if name == "--enable" and (value or "").strip() in CODEX_PINNED_FEATURES:
            return name + " " + value.strip()
        if name in CODEX_PERMISSION_PROFILE_FLAGS and value != SUPERVISOR_PERMISSION_PROFILE:
            return name
    return None


def _codex_root_path(base, value, home):
    """A -C/--add-dir value as Codex resolves it: a leading `~` or `~/` expands to
    HOME (the run's home), then the value is resolved against `base`."""
    if value == "~":
        value = str(home)
    elif value.startswith("~/"):
        value = os.path.join(str(home), value[2:].lstrip("/"))
    return os.path.normpath(os.path.join(str(base), value))


def _inside_own_dirs(path, own_keys):
    """True when a root is one of this run's own directories or lies inside one, both
    as written (lexically normalized, as Codex resolves `-C`/`--add-dir`) and with
    symlinks resolved (as Codex's sandbox canonicalizes each writable root)."""
    return all(any(is_within(_path_key(form), key) for key in own_keys)
               for form in (path, os.path.realpath(path)))


def codex_project_root_violation(argv, layout, denied, runs_root):
    """(reason, `FLAG PATH`) for the first `-C/--cd` or `--add-dir` of a Codex command
    line that Codex would turn into an unsafe writable project root, or None.

    This run's own work, results, tmp and home directories are its intended writable
    roots: Codex makes the working directory one even without `-C`. A root that is one
    of them, or lies inside one, both as written and resolved, is allowed before any
    other check, also when a broad deny such as `/Users` contains the runs root (the
    production Judge runs `codex exec -C {work_dir}` with that deny).

    Any other root is checked. A project root is more specific than any deny above it,
    so a root equal to or inside a denied path reopens that path
    (`consumer_argv_reopens_denied_path`). A root inside the runs root, or one that
    contains it, would let tool commands rewrite a generated config.toml or the
    supervisor's records of this or another run (`consumer_argv_root_overlaps_run_state`).
    Values are resolved like Codex does: `~` is the run's home, relative values are
    resolved against the consumer's working directory, and an added directory also
    against each `-C` directory. Spellings and symlinks are compared as in `_reopens`."""
    options = _codex_options(argv)
    cwd, home = layout.work_dir, layout.home
    own = [str(path) for path in (layout.work_dir, layout.results, layout.tmp, layout.home)]
    own_keys = [_path_key(path) for path in dict.fromkeys(own + [os.path.realpath(path) for path in own])]
    bases = [str(cwd)] + [_codex_root_path(cwd, value, home)
                          for name, value in options if name in ("-C", "--cd") and value]
    for name, value in options:
        if name not in CODEX_WORKSPACE_ROOT_FLAGS or not value:
            continue
        paths = [_codex_root_path(base, value, home) for base in (bases if name == "--add-dir" else bases[:1])]
        paths = [path for path in paths if not _inside_own_dirs(path, own_keys)]
        for path in paths:
            if any(_reopens(path, item, resolve=True) for item in denied):
                return "consumer_argv_reopens_denied_path", name + " " + path
        for path in paths:
            if _reopens(path, runs_root, resolve=True) or _reopens(runs_root, path, resolve=True):
                return "consumer_argv_root_overlaps_run_state", name + " " + path
    return None


def consumer_argv_guard(cfg, layout, spec, argv):
    """Refuse, before launch, a Codex command line that would replace the generated
    profile or reopen one of its denies. Only the consumer's own argv is inspected:
    a program that launches Codex itself (a GPTgrep binary, a shell, a wrapper) is not."""
    tool_permissions = spec["tool_permissions"]
    if not isinstance(tool_permissions, dict):
        return "not_applicable"
    if not invokes_codex(cfg, argv[0], spec["path_env"] or cfg.path_env, layout.work_dir):
        return "not_a_codex_command"
    override = codex_profile_override(argv)
    if override is not None:
        raise SupervisorError("consumer_argv_overrides_permission_profile", detail=override)
    denied = permission_denials(cfg, layout) + tool_permissions["deny_paths"]
    violation = codex_project_root_violation(argv, layout, denied, cfg.runs_root)
    if violation is not None:
        raise SupervisorError(violation[0], detail=violation[1])
    return "checked"


def blocker_probe(cfg):
    """'closed' (refused or unassignable, as for port 0), 'blocked' (a local
    sandbox or firewall denied the connect) or 'unreachable:*': nothing can
    receive a request sent to the blocker. 'listener' means something could."""
    parsed = urllib.parse.urlsplit(cfg.blocker_url)
    port = parsed.port if parsed.port is not None else (443 if parsed.scheme == "https" else 80)
    host = parsed.hostname
    try:
        family = socket.AF_INET6 if ":" in host else socket.AF_INET
        with socket.socket(family, socket.SOCK_STREAM) as probe:
            probe.settimeout(1.0)
            probe.connect((host, port))
    except ConnectionRefusedError:
        return "closed"
    except OSError as exc:
        if exc.errno == errno.EADDRNOTAVAIL:
            return "closed"
        if exc.errno in (errno.EPERM, errno.EACCES):
            return "blocked"
        return "unreachable:" + (errno.errorcode.get(exc.errno, "error") if exc.errno else "timeout")
    return "listener"


def check_blocker_closed(cfg):
    """A listener on the blocker port could harvest refresh tokens; refuse it."""
    if not cfg.blocker_must_be_closed:
        return "not_checked"
    result = blocker_probe(cfg)
    if result == "listener":
        raise SupervisorError("blocker_port_has_listener", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
    return result


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


def process_alive(pid, start_time, ps_bin="/bin/ps"):
    """True when pid exists and (if recorded) still has the same start time."""
    current = process_start_time(pid, ps_bin)
    return current is not None and (start_time is None or current == start_time)


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

    def __init__(self, cfg, root_pid, known=None, adopt_root=True):
        self.cfg = cfg
        self.root = root_pid
        self.known = dict(known or {})
        self.snapshots = 0
        # Only the launcher may adopt the root by pid alone. A guardian rebuilding
        # the set later must not adopt a reused pid as the run's root.
        self.adopt_root = adopt_root

    def snapshot(self):
        try:
            table = process_table(self.cfg)
        except (OSError, subprocess.TimeoutExpired):
            return None
        self.snapshots += 1
        if self.adopt_root and self.root in table and self.root not in self.known:
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


def sweep_settled(cfg, tracker, layout, exclude, settle_seconds=None, should_stop=None):
    """Re-sweep briefly so just-killed processes are not reported as survivors."""
    settle = cfg.sweep_settle_seconds if settle_seconds is None else settle_seconds
    deadline = time.monotonic() + settle
    while True:
        result = sweep(cfg, tracker, layout, exclude)
        if (not result["survivor_pids"] and not result["inconclusive"]) or time.monotonic() >= deadline:
            return result
        if should_stop and should_stop():
            result["interrupted_by_signal"] = True
            return result
        time.sleep(0.2)


def docker_containers(cfg, label):
    if cfg.docker_bin is None:
        return None, "docker_not_configured"
    env = {"PATH": cfg.path_env, "LC_ALL": "C"}
    env.update(cfg.docker_env)
    try:
        out = subprocess.run([str(cfg.docker_bin), "ps", "-a", "-q", "--filter", "label=" + label],
                             capture_output=True, text=True, timeout=20, env=env, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, type(exc).__name__
    if out.returncode != 0:
        return None, "docker_exit_" + str(out.returncode)
    return [line.strip() for line in out.stdout.splitlines() if line.strip()], None


class DockerWatch:
    """Polls a Harbor job label while the job runs, then waits for it to clear.

    "No container has the label" proves nothing unless the label was seen on
    at least one container during the run: a label nothing applies would make
    the wait vacuous, so that case ends UNKNOWN instead of cleared."""

    def __init__(self, cfg, label):
        self.cfg = cfg
        self.label = label
        self.polls = 0
        self.errors = 0
        self.last_error = None
        self.max_seen = 0
        self.first_seen_at = None
        self._last = 0.0

    def poll(self, force=False):
        now = time.monotonic()
        if not force and now - self._last < self.cfg.docker_poll_seconds:
            return None
        self._last = now
        ids, error = docker_containers(self.cfg, self.label)
        self.polls += 1
        if ids is None:
            self.errors += 1
            self.last_error = error
            return None
        if ids:
            self.max_seen = max(self.max_seen, len(ids))
            if self.first_seen_at is None:
                self.first_seen_at = utc_iso()
        return ids

    def wait_clear(self, should_stop=None):
        deadline = time.monotonic() + self.cfg.docker_wait_seconds
        remaining = None
        interrupted = False
        while True:
            ids = self.poll(force=True)
            if ids == []:
                remaining = 0
                break
            remaining = None if ids is None else len(ids)
            if time.monotonic() >= deadline:
                break
            if should_stop and should_stop():
                interrupted = True
                break
            time.sleep(self.cfg.docker_poll_seconds)
        cleared = remaining == 0
        matched = self.max_seen > 0
        return {"label": self.label, "polls": self.polls, "errors": self.errors, "last_error": self.last_error,
                "max_seen_during_run": self.max_seen, "first_seen_at": self.first_seen_at,
                "label_matched": matched, "remaining": remaining, "cleared": cleared and matched,
                "interrupted_by_signal": interrupted}


# --------------------------------------------------------------------------- lease guardian

class Guardian:
    """A lease guardian started before an auth-using process is launched.

    It inherits the lease descriptor and blocks on a pipe from the supervisor.
    A "release" verdict (sent only after a clean sweep) makes it exit at once.
    A "keep" verdict, or EOF because the supervisor died, makes it hold the lease
    until the run's processes and labelled containers are gone; it then
    re-checks the canonical credential, removes the run's auth link and exits.
    """

    def __init__(self, cfg, layout, lease, kind, baseline, docker_label=None):
        self.cfg = cfg
        self.layout = layout
        self.kind = kind
        self.lease = lease
        self.baseline = baseline
        self.docker_label = docker_label
        self.state_path = layout.state_dir / "lease-guardian-state.json"
        self.respawns = 0
        self._spawn()

    def _spawn(self):
        cfg, layout = self.cfg, self.layout
        record = {"run_dir": str(layout.run_dir), "role": layout.role, "run_id": layout.run_id,
                  "kind": self.kind, "sidecar": str(self.lease.sidecar) if self.lease.sidecar else None,
                  "docker_label": self.docker_label, "baseline": self.baseline, "supervisor_pid": os.getpid(),
                  "config_snapshot": cfg.raw, "config_source": cfg.source_path}
        path = layout.state_dir / "lease-guardian-input.json"
        argv = [sys.executable, "-I", "-B", os.path.abspath(__file__), "_lease-guardian", "--input", str(path)]
        try:
            write_json_atomic(path, record)
            self.proc = subprocess.Popen(argv, cwd="/", env={"PATH": cfg.path_env, "LC_ALL": "C"},
                                         stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL, start_new_session=True,
                                         pass_fds=(self.lease.fd,))
        except OSError as exc:
            raise SupervisorError("guardian_spawn_failed", "INTERNAL_ERROR", EXIT_INTERNAL,
                                  detail=errno_name(exc)) from None
        self.pid = self.proc.pid
        self.closed = False
        self.known_count = -1
        self.lease.update_sidecar(guardian_pid=self.pid)
        layout.event("lease_guardian_started", guardian_pid=self.pid, kind=self.kind)

    def ensure_alive(self):
        """Respawn a guardian that died while the run is still in progress."""
        if self.closed or self.alive():
            return False
        self._spawn()
        self.respawns += 1
        return True

    def update(self, tracker):
        """Publish the tracked pid set so a takeover sweep knows the run's processes."""
        if tracker is None or len(tracker.known) == self.known_count:
            return
        try:
            write_json_atomic(self.state_path, {"root_pid": tracker.root,
                                                "known": {str(k): v for k, v in tracker.known.items()}})
            self.known_count = len(tracker.known)
        except OSError:
            pass

    def alive(self):
        return self.proc.poll() is None

    def verdict(self, verdict, **fields):
        """Send the one verdict. Returns False when the guardian could not take it."""
        if self.closed:
            return False
        self.closed = True
        if not self.alive():
            return False
        try:
            self.proc.stdin.write((json.dumps(dict(fields, verdict=verdict), default=str) + "\n").encode())
            self.proc.stdin.flush()
            self.proc.stdin.close()
        except (OSError, ValueError):
            return False
        if verdict == "release":
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass
        return True


def hand_to_guardian(cfg, layout, lease, guardian, receipt, baseline, docker_label, reason):
    """Keep the lease past supervisor exit. A lost guardian is replaced; if no
    guardian can run, this process keeps the lease itself until clear."""
    for _attempt in range(2):
        if guardian.verdict("keep", baseline=baseline, docker_label=docker_label, reason=reason):
            receipt["lease"]["retained_by_guardian_pid"] = guardian.pid
            return guardian.pid
        try:
            guardian._spawn()
            receipt["anomalies"].append({"code": "LEASE_GUARDIAN_REPLACED"})
        except SupervisorError:
            break
    receipt["anomalies"].append({"code": "LEASE_GUARDIAN_UNAVAILABLE_SUPERVISOR_KEPT_LEASE"})
    receipt["lease"]["retained_by_guardian_pid"] = os.getpid()
    _finish(layout, dict(receipt, timings=dict(receipt["timings"])), "UNKNOWN", EXIT_UNKNOWN)
    receipt["guardian_inline"] = guardian_keep(cfg, layout, guardian.kind, "inline", baseline, docker_label,
                                               str(lease.sidecar) if lease.sidecar else None)
    return os.getpid()


def guardian_keep(cfg, layout, kind, mode, baseline, docker_label, sidecar):
    """Hold the lease (the caller's descriptor) until the run's tracked
    processes, run-directory holders and labelled containers are gone, then
    re-check the canonical credential and remove the run's auth link."""
    try:
        state = read_small_json(layout.state_dir / "lease-guardian-state.json", 4 << 20)
    except (OSError, ValueError):
        state = {}
    root = state.get("root_pid")
    tracker = Tracker(cfg, int(root) if root else -1, {int(k): v for k, v in (state.get("known") or {}).items()},
                      adopt_root=False)
    quiet = 0
    polls = 0
    while quiet < 2:
        polls += 1
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
    outcome = {"schema": "gptgrep.codex-auth.guardian.v1", "mode": mode, "kind": kind,
               "guardian_pid": os.getpid(), "polls": polls, "released_at": utc_iso(),
               "docker_label": docker_label}
    after = canonical_metadata(cfg)
    changed = baseline is None or canonical_changed(baseline, after)
    outcome.update(canonical_after=after, canonical_changed_since_baseline=changed)
    if changed:
        reason = ("OWNER_REFRESH_UNRECORDED" if kind == "owner" and mode == "takeover"
                  else "CANONICAL_CHANGED_AFTER_SUPERVISOR")
        outcome["quarantine"] = write_quarantine(cfg, reason, layout.run_id, {
            "operation": kind, "guardian_mode": mode})
    if kind == "owner" and mode == "takeover":
        ledger(cfg, {"run_id": layout.run_id, "decision": "OWNER_INTERRUPTED", "canonical_changed": changed,
                     "canonical_before": baseline, "canonical_after": after})
    # Survivors may have linked the credential into the run directory after the
    # supervisor's own scan.
    links = credential_links(cfg, layout, canonical_identities(baseline, after))
    outcome["credential_links"] = links
    if links["found"] or (links["canonical_nlink_after"] or 1) > 1:
        outcome["credential_link_quarantine"] = write_quarantine(
            cfg, "CREDENTIAL_LINK_IN_RUN_DIR" if links["found"] else "CANONICAL_HARDLINKED", layout.run_id, {
                "operation": kind, "guardian_mode": mode, "found": links["found"], "removed": links["removed"],
                "canonical_nlink": links["canonical_nlink_after"]})
    link = layout.release_auth_link(cfg)
    outcome["auth_link_final"] = link
    if link.get("present") and not link.get("released"):
        outcome["link_quarantine"] = write_quarantine(cfg, "RUN_AUTH_LINK_REPLACED", layout.run_id, {
            "operation": kind, "guardian_mode": mode, "path": str(layout.auth_link)})
    try:
        write_json_atomic(layout.state_dir / "lease-guardian-result.json", outcome)
        if mode == "takeover" and not (layout.run_dir / "receipt.json").exists():
            write_json_atomic(layout.run_dir / "receipt.json", {
                "schema": RUN_SCHEMA, "supervisor_version": SUPERVISOR_VERSION, "run_id": layout.run_id,
                "role": layout.role, "operation": kind, "state": "UNKNOWN",
                "exit_code": EXIT_UNKNOWN, "supervisor_lost": True, "guardian": outcome,
                "credential_contents_read_by_supervisor": False})
    except OSError:
        pass
    layout.event("lease_guardian_released", guardian_pid=os.getpid(), mode=mode)
    if sidecar:
        lease = Lease(cfg)
        lease.sidecar = Path(sidecar)
        lease.update_sidecar(released_at=utc_iso(), released_by="lease_guardian_" + mode)
    return outcome


def lease_guardian_main(input_path):
    """Hold the inherited lease until the supervisor releases it or, without a
    release, until survivors and labelled containers are gone.

    Uses the config snapshot taken at spawn time, so a later edit of the config
    file cannot make the guardian exit early. Any unexpected error is retried
    and never releases the lease (fail closed)."""
    for sig in (signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, signal.SIG_IGN)
    try:
        line = sys.stdin.readline()
    except (OSError, ValueError):
        line = ""
    try:
        message = json.loads(line) if line.strip() else None
    except ValueError:
        message = None
    if isinstance(message, dict) and message.get("verdict") == "release":
        return EXIT_PASS
    mode = "keep" if isinstance(message, dict) and message.get("verdict") == "keep" else "takeover"
    message = message if isinstance(message, dict) else {}
    while True:
        try:
            record = read_small_json(input_path, 4 << 20)
            cfg = Config(record["config_snapshot"], record["config_source"])
            layout = Layout(cfg, record["role"], record["run_id"])
            if str(layout.run_dir) != record["run_dir"]:
                raise SupervisorError("guardian_run_dir_mismatch")
            layout.event("lease_guardian_" + mode, guardian_pid=os.getpid(), reason=message.get("reason"))
            guardian_keep(cfg, layout, record["kind"], mode, message.get("baseline") or record.get("baseline"),
                          message.get("docker_label", record.get("docker_label")), record.get("sidecar"))
            return EXIT_PASS
        except Exception:  # noqa: BLE001 - fail closed: keep holding the lease and retry
            time.sleep(5)


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
    """Record SIGINT/SIGTERM/SIGHUP instead of dying; installed for the whole
    command so no signal can skip the sweep, the receipt or the guardian hand-off."""

    def __init__(self):
        self.received = None
        self.count = 0
        self.previous = {}

    def install(self):
        if self.previous:
            return self
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            self.previous[sig] = signal.signal(sig, self._handle)
        return self

    def _handle(self, signum, _frame):
        self.count += 1
        if self.received is None:
            self.received = signal.Signals(signum).name

    def restore(self):
        for sig, handler in self.previous.items():
            signal.signal(sig, handler if handler is not None else signal.SIG_DFL)
        self.previous = {}


def _deadline_for(cfg, role, requested):
    if requested is not None:
        return float(requested)
    role_deadline = cfg.role(role)["deadline_seconds"]
    return float(role_deadline if role_deadline is not None else cfg.default_deadline_seconds)


def _final_state(child_code, timed_out, cancelled, survivors, docker_uncleared, inconclusive, quarantined,
                 auth_failed=False):
    if survivors or docker_uncleared or inconclusive:
        return "UNKNOWN"
    if quarantined:
        return "QUARANTINED"
    if auth_failed:
        return "BLOCKED_AUTH"
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


def acquire_or_busy(cfg, lease, mode, wait_seconds, layout, receipt, signals=None, check_binding=True):
    """'acquired', 'busy' or 'cancelled'. The lease binding is verified before waiting."""
    binding = read_binding(cfg) if check_binding else None
    if binding is not None:
        verify_binding_identity(cfg, binding)  # before touching (or creating) any lock file
    lease.open(create=binding is None)
    if check_binding:
        receipt["lease"]["binding"] = bind_or_verify(cfg, lease.fd, binding)
    should_stop = (lambda: signals.received is not None) if signals is not None else None
    ok, waited = lease.acquire(mode, wait_seconds, should_stop=should_stop)
    receipt["lease"].update({"wait_seconds": wait_seconds, "waited_seconds": waited, "acquired": ok})
    if not ok:
        if signals is not None and signals.received:
            layout.event("cancelled_while_waiting", signal=signals.received)
            return "cancelled"
        layout.event("auth_busy", waited_seconds=waited)
        return "busy"
    receipt["timings"]["lease_acquired_at"] = utc_iso()
    receipt["lease"]["sidecar"] = lease.write_sidecar(layout.run_id, layout.role, receipt["operation"])
    receipt["lease"]["sidecar_path"] = str(lease.sidecar)
    layout.event("lease_acquired", mode=mode, waited_seconds=waited)
    return "acquired"


def stop_process_tree(cfg, proc, tracker, grace, allow_kill, scope="tree"):
    """Stop a launched tree. 'root-first' signals only the root and waits
    `grace` for its own cleanup (Harbor tears down its containers on SIGTERM)
    before the remaining tree gets SIGTERM, the global grace, then SIGKILL."""
    if scope == "root-first":
        try:
            os.kill(proc.pid, signal.SIGTERM)
        except OSError:
            pass
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline and proc.poll() is None:
            tracker.snapshot()
            time.sleep(min(cfg.sweep_interval_seconds, 0.2))
        if proc.poll() is not None and not tracker.alive(tracker.snapshot()):
            return "terminated"
        grace = cfg.term_grace_seconds
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
    """Non-secret (class, kind) of a refresh failure from the owner's own log.

    kind is 'permanent' (needs a re-login), 'transient' (retry later; the
    credential was not judged invalid) or None (unclassified)."""
    try:
        with open(path, "rb") as handle:
            lines = handle.read(4 << 20).decode("utf-8", "replace").splitlines()
    except OSError:
        return None, None
    permanent, transient = set(), []
    for raw in lines:
        line = _ANSI_RE.sub("", raw).lower()
        if "refresh" not in line or ("codex_login" not in line and "account_processor" not in line):
            continue
        for code in _ERROR_CODE_RE.findall(line):
            if code in _PERMANENT_CODES:
                permanent.add(_PERMANENT_CODES[code])
        for needle, cls in _PERMANENT_MESSAGES:
            if needle in line:
                permanent.add(cls)
                break
        for first, second in _STATUS_RE.findall(line):
            status = int(first or second)
            if status == 401:
                permanent.add("REFRESH_REJECTED")
            elif status == 429:
                transient.append("RATE_LIMITED")
            elif 500 <= status <= 599:
                transient.append("AUTHORITY_SERVER_ERROR")
        if any(marker in line for marker in _TRANSPORT_MARKERS):
            transient.append("TRANSPORT")
        if "failed to refresh token while getting account" in line:
            transient.append("TRANSIENT_OTHER")
    for cls in PERMANENT_FAILURE_CLASSES:  # most specific first; REFRESH_REJECTED last
        if cls in permanent:
            return cls, "permanent"
    for cls in TRANSIENT_FAILURE_CLASSES:
        if cls in transient:
            return cls, "transient"
    return None, None


def owner_refresh(cfg, run_id, force, lease, parent_env, receipt, layout):
    """Refresh through the official app-server; the caller holds the EXCLUSIVE lease.

    Returns (receipt, state). Once the owner process exists, every path ends
    either in a clean sweep and a released guardian or in UNKNOWN with the
    lease handed to the guardian."""
    assert lease.mode == "exclusive"
    receipt["refresh"] = {"forced": bool(force), "max_age_seconds": cfg.refresh_max_age_seconds}
    before = preflight_or_quarantine(cfg, run_id, "refresh", receipt)
    receipt["canonical_before"] = before
    age = max(0.0, time.time() - before["mtime_ns"] / 1e9)
    auth_block = auth_block_state(cfg)
    receipt["refresh"].update(canonical_age_seconds=round(age, 3), auth_block_present=bool(auth_block))
    if not force and not auth_block and age <= cfg.refresh_max_age_seconds:
        receipt["refresh"]["decision"] = "SKIPPED_FRESH"
        ledger(cfg, {"run_id": run_id, "decision": "SKIPPED_FRESH", "forced": False,
                     "canonical_age_seconds": round(age, 3), "canonical_before": before})
        return receipt, "PASS"
    layout.link_auth(cfg)
    env = build_owner_env(cfg, layout, parent_env, layout.codex_home)
    argv = [str(cfg.codex_bin), "app-server", "--listen", "stdio://", "--strict-config"]
    receipt.update({"argv": argv, "env_keys": sorted(env)})
    guardian = Guardian(cfg, layout, lease, "owner", baseline=before)
    receipt["lease"]["guardian_pid"] = guardian.pid
    stderr_log = _open_log(layout.log_dir / "owner-app-server.stderr.log")
    layout.event("owner_launch")
    receipt["timings"]["owner_started_at"] = utc_iso()
    try:
        # The pinned official app-server (no tool commands) keeps the lease even if the
        # supervisor and the guardian are both killed during its in-place save.
        proc = subprocess.Popen(argv, cwd=str(layout.work_dir), env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=stderr_log, start_new_session=True,
                                pass_fds=(lease.fd,))
    except OSError as exc:
        guardian.verdict("release")
        receipt["auth_link_final"] = layout.release_auth_link(cfg)
        raise SupervisorError("owner_launch_failed", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH,
                              detail=errno_name(exc)) from None
    finally:
        stderr_log.close()
    tracker = Tracker(cfg, proc.pid)
    try:
        state = _owner_session(cfg, run_id, force, receipt, layout, proc, tracker, guardian, before, age, auth_block)
    except Exception as exc:  # noqa: BLE001 - after launch every failure keeps the lease
        receipt["anomalies"].append({"code": "SUPERVISOR_POST_LAUNCH_ERROR", "error_type": type(exc).__name__})
        state = "UNKNOWN"
    if state == "UNKNOWN":
        after = receipt.get("canonical_after") or before
        hand_to_guardian(cfg, layout, lease, guardian, receipt, after, None, "owner_unknown")
    else:
        receipt["auth_link_final"] = layout.release_auth_link(cfg)
        guardian.verdict("release")
    return receipt, state


def _owner_session(cfg, run_id, force, receipt, layout, proc, tracker, guardian, before, age, auth_block):
    tracker.snapshot()
    guardian.update(tracker)
    client = RpcClient(proc)
    steps = []
    timeout = cfg.owner_rpc_timeout_seconds
    # The three requests are not interrupted by a signal: an in-flight refresh
    # may be in the middle of Codex's in-place save. A signal is recorded and
    # honoured after the owner has stopped.
    try:
        steps.append(classify_response("initialize", client.request(
            "initialize", {"clientInfo": {"name": "gptgrep_codex_auth_owner", "title": None,
                                          "version": SUPERVISOR_VERSION}}, timeout)))
        client.notify("initialized")
        tracker.snapshot()
        guardian.update(tracker)
        steps.append(classify_response("account/read", client.request(
            "account/read", {"refreshToken": True}, timeout)))
        steps.append(classify_response("account/read", client.request("account/read", {}, timeout)))
    except RpcError as exc:
        steps.append({"method": "rpc", "ok": False, "error": str(exc)})
    tracker.snapshot()
    guardian.update(tracker)
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
    if guardian.ensure_alive():
        receipt["anomalies"].append({"code": "LEASE_GUARDIAN_RESPAWNED"})
    result = sweep_settled(cfg, tracker, layout, exclude={guardian.pid})
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
    failure_class, failure_kind = classify_owner_log(layout.log_dir / "owner-app-server.stderr.log")
    receipt["refresh"].update({"canonical_changed": changed, "inode_preserved": after.get("ino") == before.get("ino"),
                               "integrity_ok": bool(integrity), "account_chatgpt": bool(account_ok),
                               "owner_log_failure_class": failure_class, "owner_log_failure_kind": failure_kind})
    survivors = bool(result["survivor_pids"]) or result["inconclusive"]
    if any(a["code"] == "OWNER_SIGKILLED_AFTER_GRACE" for a in receipt["anomalies"]):
        decision, state, reason = "ANOMALY", "QUARANTINED", "OWNER_SIGKILLED_AFTER_GRACE"
    elif changed and not integrity:
        decision, state, reason = "ANOMALY", "QUARANTINED", "CANONICAL_INTEGRITY_CHANGED"
        receipt["anomalies"].append({"code": reason})
    elif changed and integrity and account_ok:
        decision, state, reason = "REFRESHED", "PASS", None
    elif not changed and failure_kind == "transient" and account_ok:
        # Network, rate limit or authority 5xx: the credential was not judged
        # invalid and still reads as a chatgpt account. No quarantine and no
        # re-login; retry the refresh later.
        decision, state, reason = "REFRESH_DEFERRED_TRANSIENT", "REFRESH_UNAVAILABLE", None
    else:
        decision, state, reason = "AUTH_REFRESH_FAILED", "BLOCKED_AUTH", "AUTH_REFRESH_FAILED"
    if reason:
        receipt["quarantine"] = write_quarantine(cfg, reason, run_id, {
            "operation": "refresh", "owner_log_failure_class": failure_class, "failure_kind": failure_kind,
            "canonical_changed": changed})
    if decision == "REFRESHED" and auth_block:
        receipt["auth_block_cleared"] = archive_marker(cfg.auth_block_path, run_id)
    if survivors:
        state = "UNKNOWN"
    receipt["refresh"]["decision"] = decision
    ledger(cfg, {"run_id": run_id, "decision": decision, "forced": bool(force),
                 "canonical_age_seconds": round(age, 3), "canonical_before": before, "canonical_after": after,
                 "canonical_changed": changed, "inode_preserved": after.get("ino") == before.get("ino"),
                 "owner_exit_code": proc.returncode, "owner_stop": stop, "failure_class": failure_class,
                 "failure_kind": failure_kind, "auth_block_cleared": bool(receipt.get("auth_block_cleared")),
                 "binary": {"path": str(cfg.codex_bin), "sha256": cfg.codex_bin_sha256}})
    return state


def require_codex_pin(cfg):
    if not cfg.codex_bin_sha256 or not cfg.verify_binary_pins:
        raise SupervisorError("codex_bin_pin_required_for_owner_and_relogin")


def command_refresh(cfg, args):
    layout = Layout(cfg, "_owner", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "refresh", layout, "exclusive")
    lease = Lease(cfg)
    signals = Signals().install()
    try:
        require_codex_pin(cfg)
        receipt["binary_pins"] = cfg.verify_pins()
        quarantine = quarantine_state(cfg)
        if quarantine:
            receipt["quarantine"] = quarantine
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        outcome = acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt, signals)
        if outcome == "busy":
            ledger(cfg, {"run_id": args.run_id, "decision": "AUTH_BUSY", "forced": bool(args.force)})
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        if outcome == "cancelled":
            return _finish(layout, receipt, "CANCELLED", EXIT_CANCELLED)
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        receipt, state = owner_refresh(cfg, args.run_id, args.force, lease, dict(os.environ), receipt, layout)
        receipt["cancel_requested"] = signals.received
        if state != "UNKNOWN" and lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, state, STATE_EXIT[state])
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        if layout.linked and "retained_by_guardian_pid" not in receipt["lease"]:
            layout.release_auth_link(cfg)
        signals.restore()
        lease.close()


# --------------------------------------------------------------------------- consumer run

def _pre_refresh(cfg, args, lease, receipt, wait, signals, mode, policy):
    """Owner refresh before the consumer, under the same lease. Returns a final
    state to stop with, or None to continue with the consumer."""
    due = policy == "always" or refresh_due(cfg)
    if not due:
        receipt["pre_refresh_result"] = {"state": "PASS", "decision": "SKIPPED_FRESH", "lease_upgraded": False}
        return None
    require_codex_pin(cfg)
    if lease.mode != "exclusive":
        # No child holds the descriptor yet, so converting shared -> exclusive is safe.
        ok, waited = lease.acquire("exclusive", wait, should_stop=lambda: signals.received is not None)
        receipt["lease"]["upgraded_for_pre_refresh"] = {"acquired": ok, "waited_seconds": waited}
        if not ok:
            return "CANCELLED" if signals.received else "AUTH_BUSY"
        lease.update_sidecar(mode="exclusive")
    owner_layout = Layout(cfg, "_owner", args.run_id + "-pre")
    owner_layout.create(cfg, {})
    owner_receipt = _base_receipt(cfg, "refresh", owner_layout, "exclusive")
    owner_receipt["binary_pins"] = receipt["binary_pins"]
    owner_receipt["lease"].update(dict(receipt["lease"], note="lease held by parent run"))
    try:
        owner_receipt, owner_state = owner_refresh(cfg, owner_layout.run_id, policy == "always", lease,
                                                   dict(os.environ), owner_receipt, owner_layout)
    except SupervisorError as exc:
        owner_receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        owner_state = exc.state
    finally:
        if owner_layout.linked and "retained_by_guardian_pid" not in owner_receipt["lease"]:
            owner_layout.release_auth_link(cfg)
    _finish(owner_layout, owner_receipt, owner_state, STATE_EXIT.get(owner_state, EXIT_USAGE))
    receipt["pre_refresh_result"] = {"state": owner_state,
                                     "decision": owner_receipt.get("refresh", {}).get("decision"),
                                     "owner_run_dir": str(owner_layout.run_dir)}
    if owner_state != "PASS":
        if owner_receipt["lease"].get("retained_by_guardian_pid"):
            receipt["lease"]["retained_by_guardian_pid"] = owner_receipt["lease"]["retained_by_guardian_pid"]
        return owner_state
    if mode == "shared":
        lease.convert_to_shared(wait)
        lease.update_sidecar(mode="shared")
        receipt["lease"]["converted_to_shared"] = True
    return None


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
    tool_permissions = spec["tool_permissions"]
    # The profile and the argv are checked, and the config rendered once, before any
    # directory, lease or link exists.
    access = tool_credential_access(cfg, layout, tool_permissions)
    access["consumer_argv_guard"] = consumer_argv_guard(cfg, layout, spec, substitute(args.command, layout))
    config_text = render_codex_config(cfg, layout, extra, tool_permissions)
    if args.dry_run:
        env, passed = build_consumer_env(cfg, role, layout, dict(os.environ))
        return {"schema": RUN_SCHEMA, "operation": "run", "dry_run": True, "role": role, "run_id": args.run_id,
                "lease_mode": mode, "deadline_seconds": deadline, "layout": layout.describe(),
                "argv": substitute(args.command, layout), "env_keys": sorted(env), "passthrough_env": passed,
                "codex_config_toml": config_text, "tool_credential_access": access,
                "pre_refresh": pre_refresh, "blocker_url": cfg.blocker_url,
                "binary_pins": cfg.pins_summary()}, EXIT_PASS
    layout.create(cfg, extra, tool_permissions, config_text=config_text)
    receipt = _base_receipt(cfg, "run", layout, mode)
    receipt.update({"deadline_seconds": deadline, "harbor_job_label": args.harbor_job_label,
                    "live_request_attempted": {"yes": True, "no": False}.get(args.live_request_attempted),
                    "live_request_attempted_source": "caller", "pre_refresh": pre_refresh,
                    "caller_fields": dict(args.manifest_field or []),
                    "tool_credential_access": access})
    layout.event("created")
    lease = Lease(cfg)
    signals = Signals().install()
    try:
        receipt["binary_pins"] = cfg.verify_pins()
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        initial_mode = "exclusive" if pre_refresh == "always" else mode
        outcome = acquire_or_busy(cfg, lease, initial_mode, wait, layout, receipt, signals)
        if outcome != "acquired":
            state = "AUTH_BUSY" if outcome == "busy" else "CANCELLED"
            return _finish(layout, receipt, state, STATE_EXIT[state])
        if quarantine_state(cfg):
            receipt["quarantine"] = quarantine_state(cfg)
            return _finish(layout, receipt, "QUARANTINED", EXIT_QUARANTINED)
        if pre_refresh != "never":
            stop_state = _pre_refresh(cfg, args, lease, receipt, wait, signals, mode, pre_refresh)
            if stop_state is not None:
                return _finish(layout, receipt, stop_state, STATE_EXIT.get(stop_state, EXIT_USAGE))
        if signals.received:
            receipt["cancelled"] = signals.received
            return _finish(layout, receipt, "CANCELLED", EXIT_CANCELLED)
        block = auth_block_state(cfg)
        if block:
            receipt["auth_block"] = block
            receipt["refusal"] = {"reason": "auth_blocked_pending_owner_refresh", "detail": None}
            return _finish(layout, receipt, "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
        return _run_consumer(cfg, args, spec, layout, lease, receipt, signals, deadline)
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        if layout.linked and "retained_by_guardian_pid" not in receipt["lease"]:
            receipt["auth_link_final"] = layout.release_auth_link(cfg)
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        if layout.linked and "retained_by_guardian_pid" not in receipt["lease"]:
            layout.release_auth_link(cfg)
        signals.restore()
        lease.close()


def _run_consumer(cfg, args, spec, layout, lease, receipt, signals, deadline):
    before = preflight_or_quarantine(cfg, args.run_id, "run", receipt)
    receipt["canonical_before"] = before
    receipt["blocker_probe"] = check_blocker_closed(cfg)
    layout.link_auth(cfg)
    env, passed = build_consumer_env(cfg, args.role, layout, dict(os.environ))
    argv = substitute(args.command, layout)
    receipt.update({"argv": argv, "cwd": str(layout.work_dir), "env_keys": sorted(env),
                    "passthrough_env": passed, "stripped_env": list(STRIPPED_ALWAYS)})
    write_json_atomic(layout.run_dir / "manifest.json", dict(receipt, state="RUNNING"))
    docker = DockerWatch(cfg, args.harbor_job_label) if args.harbor_job_label else None
    guardian = Guardian(cfg, layout, lease, "consumer", baseline=before, docker_label=args.harbor_job_label)
    receipt["lease"]["guardian_pid"] = guardian.pid
    marker_fd = os.open(layout.marker, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    stdout_log = _open_log(layout.log_dir / "child.stdout.log")
    stderr_log = _open_log(layout.log_dir / "child.stderr.log")
    try:
        # Only the marker descriptor is inherited (survivor tracking). The lease
        # descriptor stays with the supervisor and the guardian: a model tool command
        # holding it could release the lease for everyone with flock(LOCK_UN).
        proc = subprocess.Popen(argv, cwd=str(layout.work_dir), env=env,
                                stdin=None if args.stdin == "inherit" else subprocess.DEVNULL,
                                stdout=stdout_log, stderr=stderr_log, start_new_session=True,
                                pass_fds=(marker_fd,))
    except OSError as exc:
        receipt["launch_error"] = errno_name(exc)
        receipt["auth_link_final"] = layout.release_auth_link(cfg)
        guardian.verdict("release")
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, "FAILED", 1)
    finally:
        stdout_log.close()
        stderr_log.close()
        os.close(marker_fd)
    # From here on nothing may release the lease without a clean sweep: every
    # exception, signal or uncleared resource ends UNKNOWN with the guardian.
    tracker = Tracker(cfg, proc.pid)
    try:
        state = _supervise(cfg, args, spec, layout, receipt, proc, tracker, guardian, docker, signals,
                           before, deadline)
    except Exception as exc:  # noqa: BLE001
        receipt["anomalies"].append({"code": "SUPERVISOR_POST_LAUNCH_ERROR", "error_type": type(exc).__name__})
        state = "UNKNOWN"
    if state == "UNKNOWN":
        hand_to_guardian(cfg, layout, lease, guardian, receipt, before, args.harbor_job_label, "run_unknown")
        receipt["retry_policy"] = "no_automatic_retry; verify this run's own resources first"
        return _finish(layout, receipt, state, EXIT_UNKNOWN)
    receipt["auth_link_final"] = layout.release_auth_link(cfg)
    if lease.sidecar:
        lease.update_sidecar(released_at=utc_iso())
    result = _finish(layout, receipt, state, _exit_for(state, receipt.get("child_exit_code")))
    guardian.verdict("release")
    return result


def _supervise(cfg, args, spec, layout, receipt, proc, tracker, guardian, docker, signals, before, deadline):
    receipt["timings"]["child_started_at"] = utc_iso()
    layout.event("child_started", pid=proc.pid)
    tracker.snapshot()
    guardian.update(tracker)
    grace = spec["term_grace_seconds"] if spec["term_grace_seconds"] is not None else cfg.term_grace_seconds
    scope = spec["stop_scope"]
    started = time.monotonic()
    timed_out = False
    cancelled = None
    stop_result = None
    blocker_listener = False
    next_probe = 0.0
    while proc.poll() is None:
        tracker.snapshot()
        if guardian.ensure_alive():
            receipt["anomalies"].append({"code": "LEASE_GUARDIAN_RESPAWNED"})
        guardian.update(tracker)
        if docker is not None:
            docker.poll()
        if cfg.blocker_must_be_closed and time.monotonic() >= next_probe:
            next_probe = time.monotonic() + BLOCKER_REPROBE_SECONDS
            if blocker_probe(cfg) == "listener":
                blocker_listener = True
                receipt["anomalies"].append({"code": "BLOCKER_LISTENER_APPEARED", "phase": "running"})
                layout.event("blocker_listener_appeared")
                stop_result = stop_process_tree(cfg, proc, tracker, grace, allow_kill=True, scope=scope)
                break
        if signals.received and not cancelled:
            cancelled = signals.received
            layout.event("cancel_forwarded", signal=cancelled)
            stop_result = stop_process_tree(cfg, proc, tracker, grace, allow_kill=True, scope=scope)
            break
        if time.monotonic() - started > deadline:
            timed_out = True
            layout.event("deadline_reached", deadline_seconds=deadline)
            stop_result = stop_process_tree(cfg, proc, tracker, grace, allow_kill=True, scope=scope)
            break
        time.sleep(cfg.sweep_interval_seconds)
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        pass
    child_code = proc.returncode
    receipt["timings"]["child_exited_at"] = utc_iso()
    receipt["child_exit_code"] = child_code
    receipt["deadline_hit"] = timed_out
    receipt["cancelled"] = cancelled
    receipt["stop"] = stop_result
    layout.event("child_exited", exit_code=child_code)
    signals_at_exit = signals.count

    def interrupted():
        # A new signal after the child exited means "stop waiting": the result
        # becomes UNKNOWN and the guardian keeps the lease.
        return signals.count > signals_at_exit

    if guardian.ensure_alive():
        receipt["anomalies"].append({"code": "LEASE_GUARDIAN_RESPAWNED"})
    result = sweep_settled(cfg, tracker, layout, exclude={guardian.pid}, should_stop=interrupted)
    receipt["sweep"] = result
    guardian.update(tracker)
    docker_uncleared = False
    if docker is not None:
        docker_result = docker.wait_clear(should_stop=interrupted)
        receipt["docker"] = docker_result
        docker_uncleared = not docker_result["cleared"]
        if not docker_result["label_matched"]:
            receipt["anomalies"].append({"code": "HARBOR_LABEL_NEVER_MATCHED", "label": docker.label})
    if cfg.blocker_must_be_closed and blocker_probe(cfg) == "listener" and not blocker_listener:
        blocker_listener = True
        receipt["anomalies"].append({"code": "BLOCKER_LISTENER_APPEARED", "phase": "after_exit"})
    receipt["signals_after_child_exit"] = signals.count - signals_at_exit
    after = canonical_metadata(cfg)
    receipt["canonical_after"] = after
    link = layout.link_state(cfg)
    receipt["auth_link_after"] = link
    if layout.mode_restored:
        receipt["anomalies"].append({"code": "RUN_CODEX_HOME_MODE_RESTORED"})
    changed = canonical_changed(before, after)
    receipt["canonical_changed"] = changed
    identities = canonical_identities(before, after)
    links = credential_links(cfg, layout, identities)
    receipt["credential_links"] = links
    reasons = []
    if changed:
        reasons.append("CANONICAL_CHANGED_BY_CONSUMER")
    if blocker_listener:
        reasons.append("BLOCKER_LISTENER_APPEARED")
    link_inconclusive = link.get("present") is None
    if link_inconclusive:
        receipt["anomalies"].append({"code": "RUN_AUTH_LINK_UNINSPECTABLE"})
    elif not link.get("present"):
        receipt["anomalies"].append({"code": "RUN_AUTH_LINK_REMOVED"})
    elif not (link.get("symlink") and link.get("target_matches") and link.get("home_physical")):
        # A per-run login or credential copy, or a redirected link: forbidden.
        reasons.append("RUN_AUTH_LINK_REPLACED")
    if links["found"]:
        # A hard link or symlink to the credential below the run directory: removed, but
        # it shows the consumer reached for a copy of the credential.
        reasons.append("CREDENTIAL_LINK_IN_RUN_DIR")
    if (links["canonical_nlink_after"] or 1) > 1:
        # An extra hard link remains somewhere outside this run directory.
        reasons.append("CANONICAL_HARDLINKED")
    if links["scan_errors"]:
        receipt["anomalies"].append({"code": "CREDENTIAL_LINK_SCAN_INCOMPLETE", "errors": links["scan_errors"]})
    for reason in reasons:
        if reason != "BLOCKER_LISTENER_APPEARED":
            receipt["anomalies"].append({"code": reason})
    if reasons:
        receipt["quarantine"] = write_quarantine(cfg, reasons[0], args.run_id, {
            "role": args.role, "operation": "run", "reasons": reasons})
    # Exit 77 is also sysexits EX_NOPERM: it blocks other consumers only together with
    # the consumer's auth-failure record.
    auth_class = None
    if child_code == EXIT_BLOCKED_AUTH:
        auth_class, record = read_auth_failure_record(layout, identities)
        receipt["auth_failure_record"] = record
        if auth_class is None:
            receipt["anomalies"].append({"code": "CONSUMER_EXIT_77_WITHOUT_AUTH_FAILURE_RECORD"})
        else:
            receipt["auth_block"] = write_auth_block(cfg, "CONSUMER_REPORTED_AUTH_FAILURE", args.run_id, {
                "role": args.role, "child_exit_code": child_code, "auth_failure_class": auth_class})
    auth_failed = auth_class is not None and not timed_out and not cancelled
    return _final_state(child_code, timed_out, cancelled, result["survivor_pids"], docker_uncleared,
                        result["inconclusive"] or link_inconclusive, bool(reasons), auth_failed)


def read_auth_failure_record(layout, identities):
    """(class or None, non-secret summary) of the consumer's auth-failure record,
    `results/auth-failure.json` = {"schema": AUTH_FAILURE_SCHEMA, "class": one of
    AUTH_FAILURE_CLASSES}. It is consumer-written, so it is read only when it is this
    uid's regular file with one link that is not the credential and at most 4 KiB
    (lstat, then O_NOFOLLOW and the same checks on the opened descriptor)."""
    path = layout.auth_failure_record
    summary = {"present": False, "valid": False}

    def unsafe(info):
        return (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or
                info.st_size > AUTH_FAILURE_RECORD_MAX_BYTES or (info.st_dev, info.st_ino) in identities)

    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return None, summary
    except OSError as exc:
        return None, dict(summary, present=None, rejected=errno_name(exc))
    summary["present"] = True
    if unsafe(info):
        return None, dict(summary, rejected="unsafe_file")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError as exc:
        return None, dict(summary, rejected=errno_name(exc))
    with os.fdopen(fd, "rb") as handle:
        opened = os.fstat(handle.fileno())
        if unsafe(opened) or (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
            return None, dict(summary, rejected="changed_while_opening")
        data = handle.read(AUTH_FAILURE_RECORD_MAX_BYTES + 1)
    try:
        record = json.loads(data)
    except ValueError:
        return None, dict(summary, rejected="invalid_json")
    if not isinstance(record, dict) or record.get("schema") != AUTH_FAILURE_SCHEMA or \
            record.get("class") not in AUTH_FAILURE_CLASSES:
        return None, dict(summary, rejected="schema_or_class")
    return record["class"], dict(summary, valid=True, failure_class=record["class"])


# --------------------------------------------------------------------------- maintenance

def _pinned_cli(cfg, layout):
    return [str(cfg.codex_bin), "-c", 'cli_auth_credentials_store="file"', "-c", 'forced_login_method="chatgpt"',
            "-c", "log_dir=" + json.dumps(str(layout.log_dir))]


def relogin_gate(cfg, layout):
    """A re-login is allowed only when the credential is unusable: missing or
    empty, quarantined after a permanent owner refresh failure, or reported as
    not logged in by the official `login status` (which reads it; this does not)."""
    basis = []
    meta = canonical_metadata(cfg)
    if not meta.get("exists"):
        basis.append("canonical_auth_missing")
    elif meta.get("regular") and meta.get("size", 0) <= 0:
        basis.append("canonical_auth_empty")
    quarantine = quarantine_state(cfg) or {}
    details = quarantine.get("details") or {}
    if quarantine.get("present") and details.get("failure_kind") == "permanent":
        basis.append("quarantine_permanent_refresh_failure:" + str(details.get("owner_log_failure_class")))
    gate = {"canonical": meta, "basis": basis}
    if basis:
        gate["allowed"] = True
        return gate
    verify_home = layout.run_dir / "verify-home"
    os.mkdir(verify_home, 0o700)
    link = verify_home / "auth.json"
    os.symlink(str(cfg.canonical_auth_file), link)
    try:
        env = build_relogin_env(cfg, layout, verify_home)
        status = subprocess.run(_pinned_cli(cfg, layout) + ["login", "status"], env=env, cwd=str(layout.work_dir),
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=120)
        gate["login_status_exit_code"] = status.returncode
    except (OSError, subprocess.TimeoutExpired) as exc:
        gate["login_status_error"] = type(exc).__name__
    finally:
        os.unlink(link)
    if gate.get("login_status_exit_code") not in (None, 0):
        basis.append("login_status_not_logged_in")
    gate["allowed"] = bool(basis)
    return gate


RELOGIN_NOTICE = ("codex-auth relogin: the official device login revokes and deletes the auth.json in the "
                  "CODEX_HOME it runs in before it shows a code. It runs here in an empty staging home, so the "
                  "existing login is neither revoked nor deleted. The new auth.json replaces the canonical file by "
                  "an atomic rename only after the login and `login status` succeed. If you decline, or the code "
                  "expires, nothing changes and the state stays BLOCKED_AUTH.\n")


def command_relogin(cfg, args):
    if not args.confirm_relogin:
        raise SupervisorError("relogin_requires_confirm_flag")
    layout = Layout(cfg, "_relogin", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "relogin", layout, "exclusive")
    lease = Lease(cfg)
    signals = Signals().install()
    staging_auth = layout.codex_home / "auth.json"
    try:
        require_codex_pin(cfg)
        receipt["binary_pins"] = cfg.verify_pins()
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        outcome = acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt, signals)
        if outcome != "acquired":
            state = "AUTH_BUSY" if outcome == "busy" else "CANCELLED"
            return _finish(layout, receipt, state, STATE_EXIT[state])
        canonical_home_check(cfg)
        if os.stat(layout.codex_home).st_dev != os.stat(cfg.canonical_auth_home).st_dev:
            raise SupervisorError("relogin_staging_not_on_canonical_filesystem", "BLOCKED_AUTH", EXIT_BLOCKED_AUTH)
        receipt["quarantine_present"] = bool(quarantine_state(cfg))
        before = canonical_metadata(cfg)
        receipt["canonical_before"] = before
        gate = relogin_gate(cfg, layout)
        receipt["gate"] = gate
        if not gate["allowed"]:
            receipt["refusal"] = {"reason": "relogin_not_required_credential_usable",
                                  "detail": "use refresh, or clear a non-permanent quarantine after review"}
            ledger(cfg, {"run_id": args.run_id, "decision": "RELOGIN_REFUSED_CREDENTIAL_USABLE"})
            if lease.sidecar:
                lease.update_sidecar(released_at=utc_iso())
            return _finish(layout, receipt, "RELOGIN_NOT_REQUIRED", EXIT_USAGE)
        env = build_relogin_env(cfg, layout, layout.codex_home)
        login_argv = _pinned_cli(cfg, layout) + ["login", "--device-auth"]
        if cfg.relogin_issuer:
            login_argv += ["--experimental_issuer", cfg.relogin_issuer]
        status_argv = _pinned_cli(cfg, layout) + ["login", "status"]
        receipt.update({"argv": [login_argv, status_argv], "env_keys": sorted(env),
                        "staging_codex_home": str(layout.codex_home), "terminal": "passthrough_not_captured"})
        sys.stderr.write(RELOGIN_NOTICE)
        sys.stderr.flush()
        layout.event("relogin_start")
        login = subprocess.run(login_argv, env=env, cwd=str(layout.work_dir), pass_fds=(lease.fd,))
        status = subprocess.run(status_argv, env=env, cwd=str(layout.work_dir), pass_fds=(lease.fd,),
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        receipt.update({"login_exit_code": login.returncode, "status_exit_code": status.returncode,
                        "cancel_requested": signals.received})
        staged = _staged_login(staging_auth, cfg)
        receipt["staging_auth"] = {k: v for k, v in staged.items() if k != "info"}
        installed = False
        if login.returncode == 0 and status.returncode == 0 and staged.get("ok"):
            os.rename(staging_auth, cfg.canonical_auth_file)
            receipt["canonical_dir_fsynced"] = fsync_directory(cfg.canonical_auth_home)
            installed = True
        elif staged.get("present"):
            # A login this attempt created but could not verify is not installed; it
            # is not the shared credential, and leaving it would leave a copy behind.
            try:
                os.unlink(staging_auth)
                receipt["staging_credential_discarded"] = True
            except OSError as exc:
                receipt["staging_credential_discard_error"] = errno_name(exc)
        after = canonical_metadata(cfg)
        receipt.update({"installed": installed, "canonical_after": after,
                        "canonical_changed": canonical_changed(before, after)})
        if installed and not (after.get("regular") and after.get("ino") == staged["info"].st_ino and
                              after.get("mode") == "0o600"):
            receipt["anomalies"].append({"code": "RELOGIN_INSTALL_UNEXPECTED_RESULT"})
            installed = False
        state = "PASS" if installed else "BLOCKED_AUTH"
        if installed:
            receipt["auth_block_cleared"] = archive_marker(cfg.auth_block_path, args.run_id)
        ledger(cfg, {"run_id": args.run_id, "decision": "RELOGIN_" + state, "login_exit_code": login.returncode,
                     "status_exit_code": status.returncode, "installed": installed, "gate_basis": gate["basis"],
                     "canonical_before": before, "canonical_after": after})
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, state, STATE_EXIT[state])
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        signals.restore()
        lease.close()


def _staged_login(path, cfg):
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return {"present": False, "ok": False}
    except OSError as exc:
        return {"present": None, "ok": False, "error": errno_name(exc)}
    ok = (stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o077 and
          info.st_size > 0 and info.st_nlink == 1)
    try:
        same_fs = info.st_dev == os.stat(cfg.canonical_auth_home).st_dev
    except OSError:
        same_fs = False
    return {"present": True, "ok": ok and same_fs, "regular": stat.S_ISREG(info.st_mode),
            "mode": oct(stat.S_IMODE(info.st_mode)), "size": info.st_size, "same_filesystem": same_fs,
            "info": info}


def command_clear_quarantine(cfg, args):
    if not args.confirm:
        raise SupervisorError("clear_quarantine_requires_confirm_flag")
    reason = (args.reason or "").strip()
    if not reason or len(reason) > 400 or "\n" in reason:
        raise SupervisorError("clear_quarantine_reason_required")
    layout = Layout(cfg, "_maintenance", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "clear-auth-block" if args.auth_block else "clear-quarantine", layout,
                            "exclusive")
    lease = Lease(cfg)
    try:
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        if acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt) != "acquired":
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        path = cfg.auth_block_path if args.auth_block else cfg.quarantine_path
        current = read_marker(path)
        receipt["marker_before"] = current
        if current:
            receipt["archived_marker"] = archive_marker(path, args.run_id)
        receipt["reason"] = redact(reason, 400)
        kind = "AUTH_BLOCK" if args.auth_block else "QUARANTINE"
        ledger(cfg, {"run_id": args.run_id, "decision": (kind + "_CLEARED") if current else ("NO_" + kind),
                     "reason": redact(reason, 400)})
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, "PASS", EXIT_PASS)
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        lease.close()


def live_holders(cfg):
    holders = []
    if cfg.holders_dir.is_dir():
        for path in sorted(cfg.holders_dir.glob("*.json")):
            try:
                record = read_small_json(path)
            except (OSError, ValueError):
                continue
            if record.get("released_at") is None and isinstance(record.get("pid"), int):
                record["alive"] = process_alive(record["pid"], record.get("process_start_time"), cfg.ps_bin)
                holders.append(record)
    return holders


def command_rebind_lease(cfg, args):
    """Human-confirmed rebinding of the auth source to the configured lease file
    (after the lock file was deleted, or a deliberate lease move)."""
    if not args.confirm:
        raise SupervisorError("rebind_lease_requires_confirm_flag")
    reason = (args.reason or "").strip()
    if not reason or len(reason) > 400 or "\n" in reason:
        raise SupervisorError("rebind_lease_reason_required")
    layout = Layout(cfg, "_maintenance", args.run_id)
    layout.create(cfg, {})
    receipt = _base_receipt(cfg, "rebind-lease", layout, "exclusive")
    lease = Lease(cfg)
    try:
        canonical_home_check(cfg)
        wait = cfg.lease_wait_seconds if args.wait_seconds is None else args.wait_seconds
        if acquire_or_busy(cfg, lease, "exclusive", wait, layout, receipt, check_binding=False) != "acquired":
            return _finish(layout, receipt, "AUTH_BUSY", EXIT_AUTH_BUSY)
        alive = [h for h in live_holders(cfg) if h["alive"] and h.get("run_id") != args.run_id]
        if alive:
            receipt["live_holders"] = [{k: h.get(k) for k in ("pid", "run_id", "role", "operation")} for h in alive]
            raise SupervisorError("rebind_refused_live_holders", "AUTH_BUSY", EXIT_AUTH_BUSY)
        previous = read_binding(cfg)
        receipt["binding_before"] = previous
        if previous is not None:
            receipt["archived_binding"] = str(Path(str(cfg.binding_path) + ".replaced-" + utc_stamp()))
            os.rename(cfg.binding_path, receipt["archived_binding"])
        record = _binding_record(cfg, lease.fd)
        write_new_private(cfg.binding_path, json.dumps(record, indent=2, sort_keys=True) + "\n")
        receipt["binding_after"] = record
        receipt["reason"] = redact(reason, 400)
        ledger(cfg, {"run_id": args.run_id, "decision": "LEASE_REBOUND", "reason": redact(reason, 400),
                     "lease_path": str(cfg.lease_path)})
        if lease.sidecar:
            lease.update_sidecar(released_at=utc_iso())
        return _finish(layout, receipt, "PASS", EXIT_PASS)
    except SupervisorError as exc:
        receipt["refusal"] = {"reason": exc.reason, "detail": exc.detail}
        return _finish(layout, receipt, exc.state, exc.exit_code)
    finally:
        lease.close()


def command_status(cfg, _args):
    holders = [h for h in live_holders(cfg)]
    try:
        meta = canonical_preflight(cfg)
        canonical = dict(meta, preflight="ok")
    except SupervisorError as exc:
        canonical = dict(canonical_metadata(cfg), preflight=exc.reason)
    age = None
    if canonical.get("mtime_ns"):
        age = round(max(0.0, time.time() - canonical["mtime_ns"] / 1e9), 3)
    try:
        binding = read_binding(cfg)
        binding_state = {"present": binding is not None,
                         "matches_config": None if binding is None else binding["lease_path"] == str(cfg.lease_path)}
    except SupervisorError as exc:
        binding_state = {"present": True, "error": exc.reason}
    return {"schema": "gptgrep.codex-auth.status.v1", "auth_source_id": cfg.auth_source_id,
            "lease": {"path": str(cfg.lease_path), "probe": probe_lease(cfg), "binding": binding_state},
            "unreleased_holder_records": holders, "quarantine": quarantine_state(cfg),
            "auth_block": auth_block_state(cfg), "canonical": canonical,
            "refresh_policy": {"max_age_seconds": cfg.refresh_max_age_seconds, "canonical_age_seconds": age,
                               "refresh_due": None if age is None else age > cfg.refresh_max_age_seconds},
            "ledger_tail": last_ledger_entries(cfg), "blocker": {"url": cfg.blocker_url,
                                                                 "probe": blocker_probe(cfg)},
            "credential_contents_read_by_supervisor": False}, EXIT_PASS


def classify_probe(steps):
    """OK, AUTH_* (explicit auth failure) or SERVICE_UNAVAILABLE (network, rate
    limit or server error, which must never be read as an auth failure)."""
    if len(steps) == 2 and steps[1].get("ok"):
        account_type = steps[1].get("account_type")
        if account_type == "chatgpt":
            return "OK"
        return "AUTH_NOT_LOGGED_IN" if account_type is None else "AUTH_UNEXPECTED_ACCOUNT_TYPE"
    message = (steps[-1].get("error_message") or "").lower() if steps else ""
    if "unauthorized" in message or "(401)" in message or "(403)" in message or "forbidden" in message:
        return "AUTH_UNAUTHORIZED"
    return "SERVICE_UNAVAILABLE"


def _probe_protected(codex_home):
    """What an account probe may never write into: the canonical auth home and the lease
    directory (named in this run's manifest, which the supervisor writes before launch)
    and the codex-home of every run below the runs root. Also the canonical file's
    identity, by stat through the run's own link (metadata only)."""
    home = Path(os.path.abspath(codex_home))
    run_dir = home.parent
    try:
        if home.name != "codex-home":
            raise ValueError("not a run home")
        manifest = read_small_json(run_dir / "manifest.json", 4 << 20)
        auth_home = _abs(manifest["canonical_auth_home"], "manifest.canonical_auth_home")
        lease_dir = _abs(manifest["lease"]["path"], "manifest.lease.path").parent
    except (OSError, ValueError, KeyError, TypeError, SupervisorError):
        raise SupervisorError("account_probe_requires_supervised_consumer_env") from None
    identities = set()
    for candidate in (home / "auth.json", auth_home / "auth.json"):
        try:
            info = os.stat(candidate)
        except OSError:
            continue
        identities.add((info.st_dev, info.st_ino))
    return [auth_home, lease_dir], run_dir.parent.parent, identities


def _probe_output_path(value, protected, runs_root, flag):
    """An output of the account probe, refused when it is equal to or inside the canonical
    auth home, the lease directory or any run's codex-home (in any spelling, symlinks
    resolved): an append through `{codex_home}/auth.json` would write into the credential."""
    path = os.path.abspath(value)
    if any(_reopens(path, root, resolve=True) for root in protected) or \
            _is_run_path(path, runs_root, ("codex-home",), resolve=True):
        raise SupervisorError("account_probe_output_in_protected_path", detail=flag)
    return path


def _open_probe_log(path, identities):
    """Append to the stderr log without ever reaching the credential: the last component
    is never followed (O_NOFOLLOW), a missing file is created 0600, and the opened file
    must be this uid's regular file with a single link that is not the canonical file (an
    existing hard link would otherwise let the append write into the credential)."""
    try:
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                     0o600)
    except OSError as exc:
        raise SupervisorError("account_probe_stderr_log_refused", detail=errno_name(exc)) from None
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or \
            (info.st_dev, info.st_ino) in identities:
        os.close(fd)
        raise SupervisorError("account_probe_stderr_log_refused", detail="not_a_private_single_link_file")
    os.set_blocking(fd, True)
    return os.fdopen(fd, "ab")


def command_account_probe(args):
    """Consumer-side, non-model live check: initialize + account/read (refreshToken=false).

    Must run inside `run` (the refresh blocker and the run's manifest must exist).
    Records only the account type and a failure class. Exit 0 (account readable),
    77 with the auth-failure record (explicit auth failure: the supervisor's
    BLOCKED_AUTH contract) or 69 (service unavailable: network, rate limit or server
    error). `--out`, `--stderr-log` and the record are refused inside the canonical
    auth home, the lease directory or any run's codex-home."""
    blocker = os.environ.get("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
    try:
        _loopback_url(blocker, "CODEX_REFRESH_TOKEN_URL_OVERRIDE")
    except SupervisorError:
        raise SupervisorError("account_probe_requires_supervised_consumer_env") from None
    if "CODEX_HOME" not in os.environ:
        raise SupervisorError("account_probe_requires_supervised_consumer_env")
    codex_bin = _abs(args.codex_bin, "--codex-bin")
    protected, runs_root, identities = _probe_protected(os.environ["CODEX_HOME"])
    out = _probe_output_path(args.out, protected, runs_root, "--out")
    record_path = os.environ.get("GPTGREP_AUTH_FAILURE_RECORD")
    if record_path:
        record_path = _probe_output_path(record_path, protected, runs_root, "GPTGREP_AUTH_FAILURE_RECORD")
    if args.stderr_log:
        stderr_log = _open_probe_log(_probe_output_path(args.stderr_log, protected, runs_root, "--stderr-log"),
                                     identities)
    else:
        stderr_log = open(os.devnull, "ab")
    with stderr_log:
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
    outcome = classify_probe(steps)
    result = {"schema": "gptgrep.codex-auth.account-probe.v1", "rpc": steps, "app_server_exit": proc.returncode,
              "refresh_requested": False, "model_request_sent": False, "account_ok": outcome == "OK",
              "outcome": outcome}
    if outcome.startswith("AUTH_") and record_path:
        try:
            write_new_private(record_path, json.dumps({"schema": AUTH_FAILURE_SCHEMA, "class": outcome}) + "\n")
            result["auth_failure_record_written"] = True
        except OSError as exc:
            result["auth_failure_record_error"] = errno_name(exc)
    write_new_private(out, json.dumps(result, indent=2, sort_keys=True) + "\n")
    if outcome == "OK":
        return result, EXIT_PASS
    return result, (EXIT_BLOCKED_AUTH if outcome.startswith("AUTH_") else EXIT_UNAVAILABLE)


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
    run.add_argument("--harbor-job-label", help="KEY=VALUE label the job stamps on every container "
                                                "(full62: gptgrep.auth-job=<runtime run_id>)")
    run.add_argument("--pre-refresh", choices=("never", "if-stale", "always"))
    run.add_argument("--live-request-attempted", choices=("yes", "no", "unknown"), default="unknown")
    run.add_argument("--manifest-field", action="append", type=_manifest_field)
    run.add_argument("--stdin", choices=("null", "inherit"), default="null")
    run.add_argument("--dry-run", action="store_true")
    run.add_argument("command", nargs=argparse.REMAINDER)
    relogin = sub.add_parser("relogin", help="maintenance device re-login (only for an unusable credential) "
                                             "in an empty staging home under the EXCLUSIVE lease")
    relogin.add_argument("--run-id", required=True)
    relogin.add_argument("--confirm-relogin", action="store_true")
    relogin.add_argument("--wait-seconds", type=float)
    clear = sub.add_parser("clear-quarantine", help="human-confirmed removal of the quarantine marker "
                                                    "(or, with --auth-block, of the consumer auth-block marker)")
    clear.add_argument("--run-id", required=True)
    clear.add_argument("--confirm", action="store_true")
    clear.add_argument("--auth-block", action="store_true")
    clear.add_argument("--reason")
    clear.add_argument("--wait-seconds", type=float)
    rebind = sub.add_parser("rebind-lease", help="human-confirmed rebinding of the auth source to the "
                                                 "configured lease file (no live holders allowed)")
    rebind.add_argument("--run-id", required=True)
    rebind.add_argument("--confirm", action="store_true")
    rebind.add_argument("--reason")
    rebind.add_argument("--wait-seconds", type=float)
    probe = sub.add_parser("account-probe", help="inside `run` only: non-model account/read through the "
                                                  "official app-server without refreshing")
    probe.add_argument("--codex-bin", required=True)
    probe.add_argument("--out", required=True)
    probe.add_argument("--stderr-log")
    probe.add_argument("--timeout", type=float, default=60)
    guardian = sub.add_parser("_lease-guardian", help=argparse.SUPPRESS)
    guardian.add_argument("--input", required=True)
    return parser


def main(argv=None):
    os.umask(0o077)
    parser = build_parser()
    args = parser.parse_args(argv)
    if getattr(args, "command", None) and args.command[:1] == ["--"]:
        args.command = args.command[1:]
    if args.command_name == "_lease-guardian":
        return lease_guardian_main(args.input)
    if args.command_name == "account-probe":
        try:
            result, code = command_account_probe(args)
        except SupervisorError as exc:
            result, code = {"state": exc.state, "refusal": {"reason": exc.reason, "detail": exc.detail}}, exc.exit_code
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
                   "relogin": command_relogin, "clear-quarantine": command_clear_quarantine,
                   "rebind-lease": command_rebind_lease}[args.command_name]
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
