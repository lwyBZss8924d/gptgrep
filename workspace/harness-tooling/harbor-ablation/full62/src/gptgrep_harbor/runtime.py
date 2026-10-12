"""Portable native runtime policy; auth contents never enter the adapter."""
from __future__ import annotations

import json
import os
from pathlib import PurePosixPath
import stat

from .common import copy_asset, digest, integer, parse, pinned, read, require, sha

CODEX_VERSION = "0.162.1"
CODEX_SOURCE_COMMIT = "092d3acd6bec3e3a14bdc7e7a2810ab628ab759d"
CODEX_HOME = "/opt/full62-home"
WORK_DIR = "/tmp/full62-work"
DECISIONS_BUDGET = "/opt/full62-private/decisions-budget.json"
DECISIONS_LEDGER_DIR = "/opt/full62-private/ledger"
BASE_TOOLS_PATH = "/usr/local/share/full62-base/provenance.json"
BASE_CAPABILITIES = ("python3", "POSIX shell", "Pillow==12.3.0", "pdftotext", "pdftoppm")
SANDBOX_HELPER_PATH = "/usr/local/bin/codex-resources/bwrap"
SANDBOX_HELPER_DATA = "/usr/local/share/full62-sandbox-helper"
SANDBOX_HELPER_MAX_BYTES = 8 * 1024 * 1024
CODE_MODE_HOST_PATH = "/usr/local/bin/codex-code-mode-host"
CODE_MODE_HOST_DATA = "/usr/local/share/full62-code-mode-host"
CODE_MODE_HOST_MAX_BYTES = 2**30
API_ENV = {"OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL", "OPENAI_ORG_ID",
           "OPENAI_ORGANIZATION", "OPENAI_PROJECT_ID", "AZURE_OPENAI_API_KEY",
           "AZURE_OPENAI_ENDPOINT", "OPENAI_ACCESS_TOKEN", "CHATGPT_ACCESS_TOKEN"}
PASS_ENV = {"PATH", "LANG", "LC_ALL", "TZ", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
            "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy",
            "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"}
# Trials are credential consumers, never refresh owners. Codex reads these
# variables on every token refresh or revoke. Port 0 can never have a listener
# (binding port 0 assigns an ephemeral port), so even a tool process sharing the
# container's network namespace cannot receive the request: any proactive,
# 401-recovery or explicit refresh fails before the shared refresh token leaves
# the process. A fixed low port such as 9 could be bound by such a process.
# Loopback is exempted from proxies so the blocked request is never forwarded.
REFRESH_BLOCKER_URL = "http://127.0.0.1:0/oauth/token"
REVOKE_BLOCKER_URL = "http://127.0.0.1:0/oauth/revoke"
LOOPBACK_NO_PROXY = ("127.0.0.1", "localhost", "::1")
# Every trial service carries this label with the runtime run_id, so the host
# supervisor (codex-auth `run --harbor-job-label`) can see the job's containers
# and keep the credential lease until they are gone.
AUTH_JOB_LABEL = "gptgrep.auth-job"


def native_identity(value):
    require(type(value) is dict and set(value) == {"version", "release_tag", "source_commit"}
            and value["version"] == CODEX_VERSION
            and value["release_tag"] == "rust-v" + CODEX_VERSION
            and value["source_commit"] == CODEX_SOURCE_COMMIT, "native_version_pin")
    return value


def sandbox_helper_identity(value, platform):
    """Validate caller-pinned release provenance; native digest compatibility needs a live probe."""
    targets = {"linux/amd64": "x86_64-unknown-linux-musl", "linux/arm64": "aarch64-unknown-linux-musl"}
    require(platform in targets, "native_helper_platform")
    require(isinstance(value, dict) and value.get("schema") == "gptgrep.native-sandbox-helper.v1" and
            value.get("release_tag") == "rust-v" + CODEX_VERSION and
            value.get("source_commit") == CODEX_SOURCE_COMMIT, "native_helper_release")
    binary = value.get("binary")
    require(isinstance(binary, dict) and set(binary) == {"path", "bytes", "sha256"} and
            binary["path"] == SANDBOX_HELPER_PATH, "native_helper_binary_pin")
    integer(binary["bytes"], 64, SANDBOX_HELPER_MAX_BYTES)
    digest(binary["sha256"])
    archive = value.get("archive")
    name = "bwrap-" + targets[platform] + ".tar.gz"
    require(isinstance(archive, dict) and archive.get("name") == name and
            archive.get("url") == "https://github.com/openai/codex/releases/download/rust-v" +
            CODEX_VERSION + "/" + name, "native_helper_official_asset")
    integer(archive.get("bytes"), 1, SANDBOX_HELPER_MAX_BYTES)
    digest(archive.get("sha256"))
    licenses = value.get("licenses")
    require(isinstance(licenses, list) and len(licenses) == 2 and
            all(isinstance(pin, dict) and set(pin) == {"path", "bytes", "sha256"} for pin in licenses) and
            {pin["path"] for pin in licenses} == {SANDBOX_HELPER_DATA + "/" + name
                for name in ("COPYING.bubblewrap", "LICENSE.codex")}, "native_helper_license_pins")
    for pin in licenses:
        integer(pin["bytes"], 1, 65536)
        digest(pin["sha256"])
    return value


def code_mode_host_identity(value, platform, runtime_binary):
    """Bind the bundled companion to the same release, platform and native CLI."""
    targets = {"linux/amd64": "x86_64-unknown-linux-musl", "linux/arm64": "aarch64-unknown-linux-musl"}
    require(platform in targets, "native_code_mode_host_platform")
    require(isinstance(value, dict) and value.get("schema") == "gptgrep.native-code-mode-host.v1" and
            value.get("release_tag") == "rust-v" + CODEX_VERSION and
            value.get("source_commit") == CODEX_SOURCE_COMMIT and
            value.get("platform") == platform, "native_code_mode_host_release")
    require(value.get("runtime_binary") == runtime_binary and
            str(PurePosixPath(runtime_binary["path"]).with_name("codex-code-mode-host")) ==
            CODE_MODE_HOST_PATH, "native_code_mode_host_pair")
    binary = value.get("binary")
    require(isinstance(binary, dict) and set(binary) == {"path", "bytes", "sha256"} and
            binary["path"] == CODE_MODE_HOST_PATH, "native_code_mode_host_binary_pin")
    integer(binary["bytes"], 64, CODE_MODE_HOST_MAX_BYTES)
    digest(binary["sha256"])
    archive = value.get("archive")
    name = "codex-code-mode-host-" + targets[platform] + ".tar.gz"
    require(isinstance(archive, dict) and archive.get("name") == name and
            archive.get("url") == "https://github.com/openai/codex/releases/download/rust-v" +
            CODEX_VERSION + "/" + name, "native_code_mode_host_official_asset")
    integer(archive.get("bytes"), 1, CODE_MODE_HOST_MAX_BYTES)
    digest(archive.get("sha256"))
    signature = value.get("sigstore")
    require(isinstance(signature, dict) and set(signature) == {"path", "bytes", "sha256"} and
            signature["path"] == CODE_MODE_HOST_DATA + "/host.sigstore",
            "native_code_mode_host_sigstore_pin")
    integer(signature["bytes"], 1, 65536)
    digest(signature["sha256"])
    # This field reports evidence supplied by the owner. Pin checks do not verify
    # the cryptographic signature or prove that the host protocol has executed.
    require(value.get("attestation_verification") in ("not_performed", "verified") and
            value.get("source_and_digest_consistency_verified") is True,
            "native_code_mode_host_evidence")
    return value


def check_code_mode_host(value, platform):
    """Check installed companion bytes and architecture without launching it."""
    copy_asset(value["binary"], CODE_MODE_HOST_MAX_BYTES)
    copy_asset(value["sigstore"], 65536)
    mode = os.stat(value["binary"]["path"]).st_mode
    require(mode & 0o111 and not mode & 0o6000, "native_code_mode_host_executable_mode")
    with open(value["binary"]["path"], "rb") as source:
        header = source.read(64)
    machine = {"linux/amd64": 62, "linux/arm64": 183}[platform]
    require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01" and
            int.from_bytes(header[18:20], "little") == machine, "native_code_mode_host_linux_elf")


def base_tools_identity(pin):
    """A common image's offline build receipt; not model permission proof."""
    value = parse(pinned(pin, 65536))
    require(value["schema"] == "gptgrep.harbor.base-tools.v1" and
            value["pillow_version"] == "12.3.0" and
            value["native_cli_version"] == CODEX_VERSION and
            value["native_source_commit"] == CODEX_SOURCE_COMMIT and
            value["python_version"].startswith("3.13.") and
            value["pillow_pixel_check"] == "passed" and
            value["capabilities"] == list(BASE_CAPABILITIES), "common_base_tool_versions")
    require(set(value["binaries"]) == {"python3", "sh", "uv", "codex", "pdftotext", "pdftoppm"},
            "common_base_tool_set")
    for name, binary in value["binaries"].items():
        require(set(binary) == {"path", "bytes", "sha256"} and
                isinstance(binary["path"], str) and binary["path"].startswith("/") and
                ".." not in PurePosixPath(binary["path"]).parts, "common_tool_binary_pin")
        integer(binary["bytes"], 1, 2**30 if name == "codex" else 2**27)
        digest(binary["sha256"])
    helpers = value.get("native_sandbox_helpers")
    require(isinstance(helpers, dict) and set(helpers) == {"bwrap"}, "native_sandbox_helper_required")
    sandbox_helper_identity(helpers["bwrap"], value["build_inputs"]["platform"])
    require("native_code_mode_host" in value, "native_code_mode_host_required")
    code_mode_host_identity(value["native_code_mode_host"], value["build_inputs"]["platform"],
                            value["binaries"]["codex"])
    return value


def check_base_tools(config):
    import PIL
    import shutil
    import sys
    from pathlib import Path
    value = base_tools_identity(config["base_tool_environment"]["provenance"])
    require(PIL.__version__ == "12.3.0" and sys.version.split()[0] == value["python_version"],
            "installed_base_python_pillow")
    require(value["binaries"]["codex"] == config["reader_base_binary"], "base_codex_binary_join")
    for name, binary in value["binaries"].items():
        found = shutil.which(name)
        require(found is not None and str(Path(found).resolve()) == binary["path"], "common_tool_path")
        # The native CLI is checked by the reader/verifier's existing binary gate.
        if name != "codex":
            copy_asset(binary, 2**27)
    helper = value["native_sandbox_helpers"]["bwrap"]
    copy_asset(helper["binary"], SANDBOX_HELPER_MAX_BYTES)
    mode = os.stat(helper["binary"]["path"]).st_mode
    require(mode & 0o111 and not mode & 0o6000, "native_helper_executable_mode")
    for pin in helper["licenses"]:
        copy_asset(pin, 65536)
    check_code_mode_host(value["native_code_mode_host"], value["build_inputs"]["platform"])


def auth_paths(value):
    require(type(value) is dict and set(value) == {"method", "reader_file", "judge_file"}
            and value["method"] == "chatgpt_device_file", "device_auth_required")
    for key in ("reader_file", "judge_file"):
        path = value[key]
        require(isinstance(path, str) and path.startswith("/") and
                PurePosixPath(path).as_posix() == path and
                ".." not in PurePosixPath(path).parts and
                PurePosixPath(path).name == "auth.json" and
                not any(c in path for c in ("$", "\n", "\r", "\x00")), "auth_file_reference")
    return value


def model_config(profile, role):
    """Both reader arms receive exactly these same bytes, filesystem and network policy."""
    require(role in ("reader", "judge"), "native_role")
    name = "full62_" + role
    settings = {
        "model": profile["model"], "model_reasoning_effort": profile["reasoning_effort"],
        "service_tier": profile["service_tier"], "model_provider": "openai",
        "cli_auth_credentials_store": "file", "forced_login_method": "chatgpt",
        "approval_policy": "never", "default_permissions": name,
        # No reader or Judge has external access: no hosted web search here and
        # no model-tool network below. GPTgrep's Decisions requests come from its
        # host process, outside this Codex tool sandbox.
        "web_search": "disabled",
    }
    lines = [key + " = " + json.dumps(value) for key, value in settings.items()]
    enabled = "true" if role == "reader" else "false"
    lines += ["", "[features]", "shell_tool = " + enabled, "view_image = " + enabled]
    if role == "reader":
        lines += [feature + " = true" for feature in (
            "unified_exec", "multi_agent", "plugins", "apps", "tool_suggest")]
        lines += ["", "[features.code_mode]", "enabled = true",
                  "", "[features.code_mode_host]", "enabled = true"]
    else:
        lines += ["code_mode_only = false", "code_mode_prewarm = false",
                  "", "[features.code_mode]", "enabled = false",
                  "direct_only_tool_namespaces = []",
                  "", "[features.code_mode_host]", "enabled = false",
                  "disable_in_process_fallback = true"]
    lines += ["", "[permissions." + name + ".filesystem]", '":minimal" = "read"']
    if role == "reader":
        lines += ['"/data/corpus" = "read"', json.dumps(WORK_DIR) + ' = "write"']
    for path in (CODEX_HOME, "/root", "/home", "/tests", "/solution", "/logs", "/answer",
                 "/opt/full62", "/opt/full62-private", "/proc", "/run/secrets"):
        lines.append(json.dumps(path) + ' = "deny"')
    if role == "judge":
        lines.append('"/data" = "deny"')
    lines += ["", "[permissions." + name + ".network]", "enabled = false", ""]
    return "\n".join(lines).encode()


def auth_compose(auth_file, platform, job_id):
    # JSON is a YAML subset. Docker mounts the one file at runtime; COPY never
    # touches it. Refresh happens only on the host, by the supervised auth owner
    # before the job; trials keep the read-only mount and the refresh blocker.
    # Harbor gives each trial its own compose project, so the job-wide label is
    # the only way to see all of this job's containers from the host.
    require(isinstance(job_id, str) and 0 < len(job_id) <= 80 and
            all(c.isalnum() or c == "-" for c in job_id), "auth_job_label")
    return {"services": {"main": {"platform": platform, "labels": {AUTH_JOB_LABEL: job_id}, "volumes": [{
        "type": "bind", "source": auth_file, "target": CODEX_HOME + "/auth.json",
        "read_only": True, "bind": {"create_host_path": False},
    }]}}}


def native_environment(environ, *, product=False):
    # No inherited shell startup, arbitrary Codex overrides or alternative API
    # account route. Proxy and CA routing retain their explicit environment.
    require(not any(environ.get(key) for key in API_ENV), "api_auth_environment_forbidden")
    result = {key: value for key, value in environ.items() if key in PASS_ENV}
    if product and environ.get("OPENROUTER_API_KEY"):
        result["OPENROUTER_API_KEY"] = environ["OPENROUTER_API_KEY"]
    result.update(CODEX_HOME=CODEX_HOME, HOME="/root", TMPDIR=WORK_DIR,
                  TMP=WORK_DIR, TEMP=WORK_DIR, XDG_CACHE_HOME=CODEX_HOME + "/cache",
                  XDG_CONFIG_HOME=CODEX_HOME + "/config", XDG_DATA_HOME=CODEX_HOME + "/data",
                  XDG_STATE_HOME=CODEX_HOME + "/state", PYTHONDONTWRITEBYTECODE="1",
                  CODEX_REFRESH_TOKEN_URL_OVERRIDE=REFRESH_BLOCKER_URL,
                  CODEX_REVOKE_TOKEN_URL_OVERRIDE=REVOKE_BLOCKER_URL)
    no_proxy = loopback_no_proxy(result.get("NO_PROXY"), result.get("no_proxy"))
    result.update(NO_PROXY=no_proxy, no_proxy=no_proxy)
    return result


def loopback_no_proxy(*values):
    """Merge caller NO_PROXY entries and always exempt loopback."""
    entries = []
    for value in values:
        for entry in (value or "").split(","):
            entry = entry.strip()
            if entry and entry not in entries:
                entries.append(entry)
    for host in LOOPBACK_NO_PROXY:
        if host not in entries:
            entries.append(host)
    return ",".join(entries)


def native_wrapper():
    """Product launches the exact pinned native CLI with a clean environment."""
    from .common import copy_asset, parse
    import sys
    config = parse(read("/opt/full62/reader.json"))
    check_config(config, "reader")
    copy_asset(config["reader_base_binary"], 2**30)
    args = sys.argv[1:]
    require(not any(arg in ("--sandbox", "-s", "--dangerously-bypass-approvals-and-sandbox",
                            "--yolo", "--full-auto") or arg.startswith(("--sandbox=", "sandbox_mode="))
                    or arg.startswith("-s")
                    for arg in args), "legacy_sandbox_override_forbidden")
    protected = {"default_permissions": '"full62_reader"', "model_provider": '"openai"',
                 "cli_auth_credentials_store": '"file"', "forced_login_method": '"chatgpt"',
                 "approval_policy": '"never"'}
    for index, arg in enumerate(args):
        override = args[index + 1] if arg in ("-c", "--config") and index + 1 < len(args) else (
            arg[len("--config="):] if arg.startswith("--config=") else None)
        if override is not None:
            key, separator, value = override.partition("=")
            require(separator and not key.startswith(("sandbox", "permissions.")) and
                    (key not in protected or value == protected[key]), "native_policy_override_forbidden")
    binary = config["codex_binary"]
    overrides = ["-c", 'default_permissions="full62_reader"', "-c", 'model_provider="openai"',
                 "-c", 'cli_auth_credentials_store="file"', "-c", 'forced_login_method="chatgpt"']
    os.execve(binary, [binary, *overrides, *args], native_environment(os.environ))


def check_config(config, role):
    expected = model_config(config["profile"] if role == "reader" else config["judge_profile"], role)
    require(config["codex_home"] == CODEX_HOME and
            config["native_config_sha256"] == sha(expected) and
            read(CODEX_HOME + "/config.toml") == expected, "native_config_mismatch")
    native_identity(config["native_cli"])
    check_base_tools(config)
    # Metadata only. Neither the adapter nor diagnostics read or hash auth.json.
    info = os.lstat(CODEX_HOME + "/auth.json")
    require(stat.S_ISREG(info.st_mode) and info.st_size > 0, "auth_mount_missing")


def validate_asset_path(value):
    parts = PurePosixPath(value).parts
    for part in parts:
        lower = part.lower()
        require(not (lower in {".env", "auth.json", "credentials", "credentials.json", "token",
                             "tokens.json", "gold", "judge", "judgment.json", "tests", "solution"}
                     or lower.startswith((".env.", "credentials.", "auth."))
                     or lower.endswith((".pem", ".key"))), "private_or_evaluator_asset")
    return value
