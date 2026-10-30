"""Common constants, paths, and helpers for E2E opaque-box test suites."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import subprocess
from typing import Any

# Authoritative directory anchors
PROJECT_ROOT = Path(__file__).resolve().parents[3]
WORKSPACE_DIR = PROJECT_ROOT / "workspace"
EXEC_PLAN_DEVBOX = WORKSPACE_DIR / "exec-plans" / "remote-devbox-harness-20261022-r1"
EXEC_PLAN_REEF = WORKSPACE_DIR / "exec-plans" / "xl-reef-readiness-20261021-r1"
SCRIPTS_DEVBOX = EXEC_PLAN_DEVBOX / "scripts"
CONFIGS_DEVBOX = EXEC_PLAN_DEVBOX / "configs"
CODEX_AUTH_TOOLING = WORKSPACE_DIR / "harness-tooling" / "codex-auth"
BENCHMARK_DIR = PROJECT_ROOT / "benchmark"
SELF_OPT_DIR = PROJECT_ROOT / "self-opt"
HANDOFF_OCR = WORKSPACE_DIR / "handoff" / "google-agy-xl-docbench-ocr-20261018-r1"
HANDOFF_MISSING63 = WORKSPACE_DIR / "handoff" / "xl-docbench-missing63-20261018"
HANDOFF_REEF = WORKSPACE_DIR / "handoff" / "xl-reef-readiness-20261021-r1"
SKILLS_DEVBOX = PROJECT_ROOT / "skills" / "gptgrep-remote-devbox"
BIN_SELFCHECK = PROJECT_ROOT / "bin" / "gptgrep-devbox-selfcheck"
RESOURCE_MANIFEST = WORKSPACE_DIR / "harness-config" / "resource-manifest.json"
ROOT_LLMS = PROJECT_ROOT / "llms.txt"
CLOSEOUT_RECEIPT_DEVBOX = WORKSPACE_DIR / "handoff" / "closeout" / "DEVBOX-SKILLS-PACKAGE-RECEIPT.json"


def read_json(path: Path | str) -> dict[str, Any] | list[Any]:
    """Read and parse a JSON file."""
    p = Path(path)
    if not p.is_file():
        raise FileNotFoundError(f"JSON file not found: {p}")
    with p.open("r", encoding="utf-8") as f:
        return json.load(f)


def read_file(path: Path | str) -> str:
    """Read a text file."""
    p = Path(path)
    if not p.is_file():
        raise FileNotFoundError(f"File not found: {p}")
    return p.read_text(encoding="utf-8")


def run_cmd(
    cmd: list[str] | str,
    cwd: Path | str | None = None,
    env: dict[str, str] | None = None,
    shell: bool = False,
    timeout: float = 30.0,
) -> subprocess.CompletedProcess[str]:
    """Execute a subprocess and return completed process."""
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    return subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else str(PROJECT_ROOT),
        env=merged_env,
        shell=shell,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def get_harbor_version() -> str | None:
    """Return installed Harbor CLI version string or None if uninstalled."""
    res = run_cmd(["harbor", "--version"])
    if res.returncode == 0:
        raw = res.stdout.strip()
        match = re.search(r"(\d+\.\d+\.\d+)", raw)
        return match.group(1) if match else raw
    return None


def parse_semver(ver: str) -> tuple[int, int, int]:
    """Parse semver string into (major, minor, patch) integer tuple."""
    match = re.search(r"^v?(\d+)\.(\d+)\.(\d+)", ver.strip())
    if not match:
        raise ValueError(f"Invalid semver string: {ver}")
    return int(match.group(1)), int(match.group(2)), int(match.group(3))
