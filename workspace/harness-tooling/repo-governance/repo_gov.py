#!/usr/bin/env python3
"""GPTgrep Unified Repository Governance Helper.

Provides fail-closed public safety scanning (working tree, arbitrary git refs,
plain directories, and commit history ranges), branch naming enforcement,
organization boundary checks, and worktree-aware pre-push hook installation.

Design invariant: This file contains ZERO personal identifiers, private
hostnames, account emails, or workstation paths. Optional local denylist
extensions are loaded from untracked paths (e.g. .local/repo-gov-denylist.txt
or <git-common-dir>/info/repo-gov-denylist.txt) when present.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
from typing import Iterable, Sequence

VALID_ROLES = ("core", "homelab-private", "kwb")
ZERO_SHA = "0" * 40

SKIP_DIR_NAMES = frozenset(
    {
        ".git",
        ".local",
        ".tgrep",
        ".venv",
        "__pycache__",
        "node_modules",
        "target",
    }
)


@dataclass(frozen=True)
class PatternRule:
    rule_id: str
    description: str
    regex: re.Pattern[str]


@dataclass(frozen=True)
class SafetyFinding:
    source: str
    location: str
    line: int
    rule_id: str
    match: str


def _builtin_rules() -> list[PatternRule]:
    """Return built-in generic public-safety rules (zero personal tokens)."""
    lan_a = r"\b10" + r"\.1" + r"\.1\.\d{1,3}\b"
    lan_b = r"\b192" + r"\.168" + r"\.138\.\d{1,3}\b"
    raw_specs: list[tuple[str, str, str]] = [
        ("lan_ipv4_subnet_a", "Private RFC1918 LAN IPv4 address", lan_a),
        ("lan_ipv4_subnet_b", "Private RFC1918 LAN IPv4 address", lan_b),
        (
            "macos_user_home_path",
            "Non-placeholder macOS /Users/<account> path",
            r"/Users/(?!(?:runner|ubuntu|root|YOU|you|me|fixture|private|my-project|my-other-repo|example|user|alice|bob)\b)[A-Za-z0-9][A-Za-z0-9_.-]*",
        ),
        (
            "macos_volume_path",
            "Non-placeholder macOS /Volumes/<disk> path",
            r"/Volumes/(?!(?:fixture|device|Data|USB|Macintosh|example)\b)[A-Za-z0-9][A-Za-z0-9_.-]*",
        ),
        (
            "private_homelab_hostname",
            "Private homelab node or mDNS hostname",
            r"\b(?:PVE-\d{2,4}|mac-server-\d{2,4}|[A-Za-z0-9-]*MacBook\.local)\b",
        ),
        (
            "onepassword_concrete_uuid",
            "Concrete 26-char 1Password vault/item UUID in op:// URI",
            r"op://(?:[A-Za-z0-9_.-]+/)?[a-z0-9]{26}(?:/|\b)",
        ),
        (
            "apple_privaterelay_email",
            "Apple Private Relay email address",
            r"\b[a-z0-9]{8,16}@privaterelay\.appleid\.com\b",
        ),
        (
            "google_oauth_client_id",
            "Concrete Google OAuth client ID",
            r"\b\d{10,14}-[a-z0-9]{32}\.apps\.googleusercontent\.com\b",
        ),
        (
            "openrouter_api_key",
            "Raw OpenRouter API key",
            r"\bsk-or-v1-[A-Za-z0-9]{20,}\b",
        ),
        (
            "github_token",
            "Raw GitHub token",
            r"\bgh[opsu]_[A-Za-z0-9]{20,}\b",
        ),
        (
            "huggingface_token",
            "Raw Hugging Face token",
            r"\bhf_[A-Za-z0-9]{20,}\b",
        ),
        (
            "onepassword_service_token",
            "Raw 1Password service account token",
            r"\bops_[A-Za-z0-9]{20,}\b",
        ),
        (
            "pem_private_key",
            "PEM private key block header",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ),
    ]
    return [PatternRule(rule_id, desc, re.compile(pat)) for rule_id, desc, pat in raw_specs]


def _git_common_dir(repo: Path) -> Path | None:
    res = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "--git-common-dir"],
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        return None
    raw = res.stdout.strip()
    if not raw:
        return None
    p = Path(raw)
    if not p.is_absolute():
        p = (repo / p).resolve()
    return p


def _load_denylist_rules(
    repo: Path | None,
    explicit_files: Sequence[str] = (),
) -> list[PatternRule]:
    candidate_paths: list[Path] = []
    for f in explicit_files:
        if f:
            candidate_paths.append(Path(f))
    env_val = os.environ.get("GPTGREP_REPO_GOV_DENYLIST", "").strip()
    if env_val:
        for part in env_val.split(os.pathsep):
            if part.strip():
                candidate_paths.append(Path(part.strip()))
    if repo is not None:
        candidate_paths.append(repo / ".local" / "repo-gov-denylist.txt")
        common_dir = _git_common_dir(repo)
        if common_dir is not None:
            candidate_paths.append(common_dir / "info" / "repo-gov-denylist.txt")

    seen_files: set[Path] = set()
    rules: list[PatternRule] = []
    idx = 0
    for path in candidate_paths:
        try:
            resolved = path.resolve()
        except OSError:
            continue
        if resolved in seen_files or not resolved.is_file():
            continue
        seen_files.add(resolved)
        for raw_line in resolved.read_text(encoding="utf-8", errors="replace").splitlines():
            entry = raw_line.strip()
            if not entry or entry.startswith("#"):
                continue
            idx += 1
            if entry.startswith("literal:"):
                pat_str = re.escape(entry[len("literal:") :])
            else:
                pat_str = entry
            try:
                compiled = re.compile(pat_str)
            except re.error:
                compiled = re.compile(re.escape(entry))
            rules.append(
                PatternRule(
                    rule_id=f"local_denylist_{idx}",
                    description="Local denylist pattern",
                    regex=compiled,
                )
            )
    return rules


def _is_probably_binary(data: bytes) -> bool:
    return b"\x00" in data[:8192]


def _scan_text_lines(
    lines: Iterable[tuple[int, str]],
    source: str,
    location: str,
    rules: Sequence[PatternRule],
) -> list[SafetyFinding]:
    findings: list[SafetyFinding] = []
    for line_no, text in lines:
        for rule in rules:
            for m in rule.regex.finditer(text):
                findings.append(
                    SafetyFinding(
                        source=source,
                        location=location,
                        line=line_no,
                        rule_id=rule.rule_id,
                        match=m.group(0),
                    )
                )
    return findings


def _scan_plain_dir(root: Path, rules: Sequence[PatternRule]) -> tuple[int, list[SafetyFinding]]:
    findings: list[SafetyFinding] = []
    scanned = 0
    root_resolved = root.resolve()
    for dirpath, dirnames, filenames in os.walk(root_resolved):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIR_NAMES)
        for fname in sorted(filenames):
            fpath = Path(dirpath) / fname
            if fpath.is_symlink() or not fpath.is_file():
                continue
            try:
                raw = fpath.read_bytes()
            except OSError:
                continue
            if _is_probably_binary(raw):
                continue
            scanned += 1
            rel = str(fpath.relative_to(root_resolved))
            txt = raw.decode("utf-8", errors="replace")
            findings.extend(
                _scan_text_lines(enumerate(txt.splitlines(), 1), "dir", rel, rules)
            )
    return scanned, findings


def _scan_working_tree_tracked(repo: Path, rules: Sequence[PatternRule]) -> tuple[int, list[SafetyFinding]]:
    res = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "-z"],
        capture_output=True,
        check=False,
    )
    if res.returncode != 0:
        raise RuntimeError(f"git ls-files failed: {res.stderr.decode('utf-8', errors='replace').strip()}")
    paths = [p.decode("utf-8", errors="replace") for p in res.stdout.split(b"\x00") if p]
    findings: list[SafetyFinding] = []
    scanned = 0
    for rel in paths:
        fpath = repo / rel
        if fpath.is_symlink() or not fpath.is_file():
            continue
        try:
            raw = fpath.read_bytes()
        except OSError:
            continue
        if _is_probably_binary(raw):
            continue
        scanned += 1
        txt = raw.decode("utf-8", errors="replace")
        findings.extend(_scan_text_lines(enumerate(txt.splitlines(), 1), "tracked", rel, rules))
    return scanned, findings


def _scan_git_ref_tree(repo: Path, ref: str, rules: Sequence[PatternRule]) -> tuple[int, list[SafetyFinding]]:
    ls_res = subprocess.run(
        ["git", "-C", str(repo), "ls-tree", "-r", "-z", ref],
        capture_output=True,
        check=False,
    )
    if ls_res.returncode != 0:
        raise RuntimeError(
            f"git ls-tree failed for ref '{ref}': {ls_res.stderr.decode('utf-8', errors='replace').strip()}"
        )
    entries: list[tuple[str, str]] = []
    for raw_entry in ls_res.stdout.split(b"\x00"):
        if not raw_entry:
            continue
        meta, _, path_bytes = raw_entry.partition(b"\t")
        meta_parts = meta.decode("utf-8", errors="replace").split()
        if len(meta_parts) != 3:
            continue
        mode, obj_type, sha = meta_parts
        if obj_type != "blob" or mode == "120000":
            continue
        entries.append((sha, path_bytes.decode("utf-8", errors="replace")))

    if not entries:
        return 0, []

    batch_input = "".join(f"{sha}\n" for sha, _ in entries).encode("utf-8")
    cat_res = subprocess.run(
        ["git", "-C", str(repo), "cat-file", "--batch"],
        input=batch_input,
        capture_output=True,
        check=False,
    )
    if cat_res.returncode != 0:
        raise RuntimeError("git cat-file --batch failed")

    buf = cat_res.stdout
    offset = 0
    scanned = 0
    findings: list[SafetyFinding] = []
    for _, rel_path in entries:
        nl = buf.find(b"\n", offset)
        if nl == -1:
            break
        header = buf[offset:nl].decode("utf-8", errors="replace").split()
        offset = nl + 1
        if len(header) < 3 or header[1] == "missing":
            continue
        size = int(header[2])
        blob = buf[offset : offset + size]
        offset += size + 1  # trailing newline
        if _is_probably_binary(blob):
            continue
        scanned += 1
        txt = blob.decode("utf-8", errors="replace")
        findings.extend(
            _scan_text_lines(enumerate(txt.splitlines(), 1), f"ref:{ref}", rel_path, rules)
        )
    return scanned, findings


def _scan_history_range(repo: Path, history_range: str, rules: Sequence[PatternRule]) -> list[SafetyFinding]:
    proc = subprocess.Popen(
        [
            "git",
            "-C",
            str(repo),
            "log",
            "--no-notes",
            "-p",
            "--no-color",
            "--format=___COMMIT___%H",
            history_range,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert proc.stdout is not None
    assert proc.stderr is not None

    findings: list[SafetyFinding] = []
    current_commit = "unknown"
    current_file = "<commit-message>"
    in_diff = False
    added_line_idx = 0

    for raw_line in proc.stdout:
        line = raw_line.decode("utf-8", errors="replace").rstrip("\r\n")
        if line.startswith("___COMMIT___"):
            current_commit = line[len("___COMMIT___") :].strip()
            current_file = "<commit-message>"
            in_diff = False
            added_line_idx = 0
            continue
        if line.startswith("diff --git "):
            in_diff = True
            added_line_idx = 0
            continue
        if in_diff:
            if line.startswith("+++ b/"):
                current_file = line[len("+++ b/") :]
                added_line_idx = 0
                continue
            if line.startswith("+++ "):
                continue
            if line.startswith("+"):
                added_line_idx += 1
                added_content = line[1:]
                findings.extend(
                    _scan_text_lines(
                        [(added_line_idx, added_content)],
                        f"history:{current_commit[:12]}",
                        current_file,
                        rules,
                    )
                )
        else:
            added_line_idx += 1
            findings.extend(
                _scan_text_lines(
                    [(added_line_idx, line)],
                    f"commit_msg:{current_commit[:12]}",
                    "<commit-message>",
                    rules,
                )
            )

    stderr_text = proc.stderr.read().decode("utf-8", errors="replace").strip()
    ret = proc.wait()
    if ret != 0:
        raise RuntimeError(f"git log failed for range '{history_range}': {stderr_text}")
    return findings


BRANCH_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("main", re.compile(r"^main$")),
    ("dev_alpha_line", re.compile(r"^dev/\d+\.\d+\.x-alpha\.(?:x|\d+)$")),
    (
        "dev_opt_evo",
        re.compile(r"^dev/opt-evo/[a-z0-9]+(?:-[a-z0-9]+)*/[a-z0-9]+(?:-[a-z0-9]+)*$"),
    ),
    ("delivery", re.compile(r"^delivery/[a-z0-9]+(?:-[a-z0-9._]+)*$")),
    ("release", re.compile(r"^release/v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$")),
]


def validate_branch_name(branch: str) -> tuple[bool, str | None, str]:
    normalized = branch.strip()
    if normalized.startswith("refs/heads/"):
        normalized = normalized[len("refs/heads/") :]
    for category, pat in BRANCH_PATTERNS:
        if pat.match(normalized):
            return True, category, normalized
    return False, None, normalized


def _classify_remote_url(remote_url: str) -> dict[str, bool]:
    u = remote_url.strip()
    u_lower = u.lower()
    is_disabled = "disabled_manual_release_sync_only" in u_lower or u.startswith("DISABLED")
    is_github = "github.com" in u_lower
    is_huggingface = "huggingface.co" in u_lower or "hf.co" in u_lower
    is_celhive = bool(
        re.search(r"github\.com[:/]celhive(?:-ai)?(?:/|$)", u_lower)
        or re.search(r"(?:^|[/:])celhive-ai(?:/|$)", u_lower)
    )
    return {
        "is_disabled": is_disabled,
        "is_github": is_github,
        "is_huggingface": is_huggingface,
        "is_celhive": is_celhive,
        "is_public_cloud": is_github or is_huggingface,
    }


def evaluate_org_boundary(
    role: str,
    remote_url: str,
    refspecs: Sequence[str] = (),
) -> tuple[bool, list[str]]:
    if role not in VALID_ROLES:
        return False, [f"Invalid role '{role}'; expected one of {VALID_ROLES}"]

    info = _classify_remote_url(remote_url)
    reasons: list[str] = []

    if info["is_disabled"]:
        reasons.append(
            f"Direct push disabled by remote URL policy ({remote_url}); use manual release sync workflow_dispatch."
        )

    if info["is_celhive"] and role == "homelab-private":
        reasons.append(
            f"Role 'homelab-private' is strictly prohibited from pushing to CelHive organization remote ({remote_url})."
        )

    for spec in refspecs:
        s = spec.strip()
        if not s:
            continue
        if ":" in s:
            local_ref, remote_ref = s.split(":", 1)
        else:
            local_ref, remote_ref = s, s
        local_ref = local_ref.lstrip("+")

        if info["is_public_cloud"]:
            if local_ref.startswith("refs/notes/") or remote_ref.startswith("refs/notes/"):
                reasons.append(
                    f"Pushing private PoUW notes ({s}) to non-LAN remote ({remote_url}) is strictly forbidden."
                )

        if info["is_celhive"]:
            norm_remote = (
                remote_ref[len("refs/heads/") :]
                if remote_ref.startswith("refs/heads/")
                else remote_ref
            )
            if norm_remote.startswith("dev/opt-evo/"):
                reasons.append(
                    f"Experimental opt-evo branch '{norm_remote}' must not be pushed to CelHive organization remote."
                )

        if info["is_github"] and remote_ref.startswith("refs/heads/"):
            ok_branch, _, norm_name = validate_branch_name(remote_ref)
            if not ok_branch:
                reasons.append(
                    f"Target GitHub branch '{norm_name}' violates repository branch naming governance."
                )

    return len(reasons) == 0, reasons


def _resolve_new_branch_history_range(repo: Path, local_sha: str) -> str | None:
    """Find best history range <base>..<local_sha> when pushing a new branch to GitHub."""
    for base_candidate in ("refs/remotes/origin/main", "refs/heads/main"):
        chk = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", base_candidate],
            capture_output=True,
            text=True,
            check=False,
        )
        if chk.returncode == 0:
            base_sha = chk.stdout.strip()
            mb = subprocess.run(
                ["git", "-C", str(repo), "merge-base", base_sha, local_sha],
                capture_output=True,
                text=True,
                check=False,
            )
            if mb.returncode == 0 and mb.stdout.strip():
                return f"{mb.stdout.strip()}..{local_sha}"
    return None


def _render_pre_push_hook(role: str, fallback_script: Path) -> str:
    return f"""#!/bin/sh
# Managed by gptgrep-repo-gov install-hooks (role={role}).
# Worktree-aware pre-push governance gate: enforces org boundary, blocks
# refs/notes/* to non-LAN remotes, and runs public-safety scan for GitHub pushes.
set -eu

REMOTE_NAME="${{1:-}}"
REMOTE_URL="${{2:-}}"
ROLE="{role}"
FALLBACK_GOV="{fallback_script}"

TOPLEVEL="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -n "$TOPLEVEL" ] && [ -f "$TOPLEVEL/workspace/harness-tooling/repo-governance/repo_gov.py" ]; then
  GOV_SCRIPT="$TOPLEVEL/workspace/harness-tooling/repo-governance/repo_gov.py"
  REPO_PATH="$TOPLEVEL"
elif [ -f "$FALLBACK_GOV" ]; then
  GOV_SCRIPT="$FALLBACK_GOV"
  REPO_PATH="$(git rev-parse --git-dir)"
else
  echo "[gptgrep-repo-gov] ERROR: Cannot locate repo_gov.py; refusing push fail-closed." >&2
  exit 1
fi

STDIN_LINES="$(cat)"
exec python3 "$GOV_SCRIPT" pre-push-gate \\
  --repo "$REPO_PATH" \\
  --role "$ROLE" \\
  --remote-name "$REMOTE_NAME" \\
  --remote-url "$REMOTE_URL" \\
  --updates "$STDIN_LINES"
"""


def cmd_check_public_safety(args: argparse.Namespace) -> int:
    repo = Path(args.repo).resolve() if args.repo else None
    plain_dir = Path(args.dir).resolve() if args.dir else None
    if plain_dir is None and repo is None:
        repo = Path.cwd().resolve()

    rules = _builtin_rules() + _load_denylist_rules(
        repo=repo if repo is not None else plain_dir,
        explicit_files=args.denylist_file or (),
    )

    findings: list[SafetyFinding] = []
    scanned_files = 0
    try:
        if plain_dir is not None:
            if not plain_dir.is_dir():
                raise RuntimeError(f"Directory does not exist: {plain_dir}")
            scanned_files, dir_findings = _scan_plain_dir(plain_dir, rules)
            findings.extend(dir_findings)
        else:
            assert repo is not None
            if args.ref:
                scanned_files, ref_findings = _scan_git_ref_tree(repo, args.ref, rules)
                findings.extend(ref_findings)
            else:
                scanned_files, wt_findings = _scan_working_tree_tracked(repo, rules)
                findings.extend(wt_findings)

        if args.history_range:
            if repo is None:
                raise RuntimeError("--history-range requires a git --repo")
            findings.extend(_scan_history_range(repo, args.history_range, rules))
    except RuntimeError as exc:
        if args.json:
            print(json.dumps({"ok": False, "error": str(exc)}, indent=2))
        else:
            print(f"[gptgrep-repo-gov] ERROR: {exc}", file=sys.stderr)
        return 2

    ok = len(findings) == 0
    payload = {
        "ok": ok,
        "scanned_files": scanned_files,
        "ref": args.ref,
        "history_range": args.history_range,
        "findings_count": len(findings),
        "findings": [asdict(f) for f in findings[:200]],
    }
    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        if ok:
            scope = f"dir={plain_dir}" if plain_dir else f"repo={repo} ref={args.ref or 'WORKTREE'}"
            hist = f" history={args.history_range}" if args.history_range else ""
            print(f"[gptgrep-repo-gov] PASS: 0 public-safety findings ({scanned_files} files scanned; {scope}{hist})")
        else:
            print(
                f"[gptgrep-repo-gov] FAIL: {len(findings)} public-safety finding(s) detected:",
                file=sys.stderr,
            )
            for f in findings[:50]:
                print(
                    f"  - [{f.rule_id}] {f.source} {f.location}:{f.line} -> {f.match}",
                    file=sys.stderr,
                )
    return 0 if ok else 1


def cmd_check_branch_name(args: argparse.Namespace) -> int:
    ok, category, normalized = validate_branch_name(args.branch)
    payload = {
        "ok": ok,
        "branch": args.branch,
        "normalized": normalized,
        "category": category,
    }
    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        if ok:
            print(f"[gptgrep-repo-gov] PASS: branch '{normalized}' matches '{category}'")
        else:
            print(
                f"[gptgrep-repo-gov] FAIL: branch '{normalized}' does not match allowed naming patterns "
                "(main | dev/<maj>.<min>.x-alpha.<x|n> | dev/opt-evo/<goal>/<ckpt> | delivery/<slug> | release/<semver>)",
                file=sys.stderr,
            )
    return 0 if ok else 1


def cmd_check_org_boundary(args: argparse.Namespace) -> int:
    ok, reasons = evaluate_org_boundary(
        role=args.role,
        remote_url=args.remote_url,
        refspecs=args.refspec or (),
    )
    payload = {
        "ok": ok,
        "role": args.role,
        "remote_url": args.remote_url,
        "reasons": reasons,
    }
    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        if ok:
            print(f"[gptgrep-repo-gov] PASS: org boundary verified (role={args.role}, remote={args.remote_url})")
        else:
            for r in reasons:
                print(f"[gptgrep-repo-gov] BLOCKED: {r}", file=sys.stderr)
    return 0 if ok else 1


def cmd_install_hooks(args: argparse.Namespace) -> int:
    repo = Path(args.repo).resolve()
    common_dir = _git_common_dir(repo)
    if common_dir is None:
        msg = f"Not a valid Git repository or worktree: {repo}"
        if args.json:
            print(json.dumps({"ok": False, "error": msg}, indent=2))
        else:
            print(f"[gptgrep-repo-gov] ERROR: {msg}", file=sys.stderr)
        return 2

    hooks_dir = common_dir / "hooks"
    hooks_dir.mkdir(parents=True, exist_ok=True)
    hook_path = hooks_dir / "pre-push"
    this_script = Path(__file__).resolve()
    hook_content = _render_pre_push_hook(args.role, this_script)
    hook_path.write_text(hook_content, encoding="utf-8")
    current_mode = hook_path.stat().st_mode
    hook_path.chmod(current_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    payload = {
        "ok": True,
        "repo": str(repo),
        "role": args.role,
        "git_common_dir": str(common_dir),
        "hook_path": str(hook_path),
    }
    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        print(f"[gptgrep-repo-gov] Installed worktree-aware pre-push hook at {hook_path} (role={args.role})")
    return 0


def cmd_pre_push_gate(args: argparse.Namespace) -> int:
    repo = Path(args.repo).resolve()
    refspecs: list[str] = []
    updates: list[tuple[str, str, str, str]] = []
    for raw_line in (args.updates or "").splitlines():
        parts = raw_line.strip().split()
        if len(parts) != 4:
            continue
        local_ref, local_sha, remote_ref, remote_sha = parts
        updates.append((local_ref, local_sha, remote_ref, remote_sha))
        refspecs.append(f"{local_ref}:{remote_ref}")

    ok_boundary, reasons = evaluate_org_boundary(
        role=args.role,
        remote_url=args.remote_url,
        refspecs=refspecs,
    )
    if not ok_boundary:
        for r in reasons:
            print(f"[gptgrep-repo-gov] PRE-PUSH BLOCKED: {r}", file=sys.stderr)
        return 1

    info = _classify_remote_url(args.remote_url)
    if not info["is_public_cloud"]:
        return 0

    rules = _builtin_rules() + _load_denylist_rules(repo=repo)
    all_findings: list[SafetyFinding] = []
    for local_ref, local_sha, remote_ref, remote_sha in updates:
        if local_sha == ZERO_SHA:
            continue  # branch deletion
        _, ref_findings = _scan_git_ref_tree(repo, local_sha, rules)
        all_findings.extend(ref_findings)

        hist_range: str | None = None
        if remote_sha != ZERO_SHA:
            chk = subprocess.run(
                ["git", "-C", str(repo), "cat-file", "-e", f"{remote_sha}^{{commit}}"],
                capture_output=True,
                check=False,
            )
            if chk.returncode == 0:
                hist_range = f"{remote_sha}..{local_sha}"
        if hist_range is None:
            hist_range = _resolve_new_branch_history_range(repo, local_sha)
        if hist_range:
            all_findings.extend(_scan_history_range(repo, hist_range, rules))

    if all_findings:
        print(
            f"[gptgrep-repo-gov] PRE-PUSH BLOCKED: {len(all_findings)} public-safety finding(s) in push to {args.remote_url}:",
            file=sys.stderr,
        )
        for f in all_findings[:30]:
            print(f"  - [{f.rule_id}] {f.source} {f.location}:{f.line} -> {f.match}", file=sys.stderr)
        return 1

    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gptgrep-repo-gov",
        description="GPTgrep unified repository governance & public-safety CLI",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_safety = sub.add_parser("check-public-safety", help="Scan tracked files, refs, dirs, and commit history")
    p_safety.add_argument("--repo", default=None, help="Git repository or worktree path (default: cwd)")
    p_safety.add_argument("--ref", default=None, help="Git ref/treeish to scan instead of working tree")
    p_safety.add_argument("--history-range", default=None, help="Git revision range <base..head> to scan diffs/messages")
    p_safety.add_argument("--dir", default=None, help="Plain directory path to scan recursively")
    p_safety.add_argument("--denylist-file", action="append", default=[], help="Additional local denylist file")
    p_safety.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    p_safety.set_defaults(func=cmd_check_public_safety)

    p_branch = sub.add_parser("check-branch-name", help="Validate branch naming convention")
    p_branch.add_argument("branch", help="Branch name or refs/heads/<branch>")
    p_branch.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    p_branch.set_defaults(func=cmd_check_branch_name)

    p_org = sub.add_parser("check-org-boundary", help="Enforce organization & notes remote push boundaries")
    p_org.add_argument("--role", required=True, choices=VALID_ROLES, help="Repository governance role")
    p_org.add_argument("--remote-url", required=True, help="Target remote push URL")
    p_org.add_argument("--refspec", action="append", default=[], help="Optional local:remote refspec(s)")
    p_org.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    p_org.set_defaults(func=cmd_check_org_boundary)

    p_hooks = sub.add_parser("install-hooks", help="Install worktree-aware pre-push hook into git-common-dir")
    p_hooks.add_argument("--repo", default=".", help="Git repository or worktree path")
    p_hooks.add_argument("--role", required=True, choices=VALID_ROLES, help="Repository governance role")
    p_hooks.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    p_hooks.set_defaults(func=cmd_install_hooks)

    p_gate = sub.add_parser("pre-push-gate", help="Internal pre-push hook entrypoint")
    p_gate.add_argument("--repo", required=True)
    p_gate.add_argument("--role", required=True, choices=VALID_ROLES)
    p_gate.add_argument("--remote-name", default="")
    p_gate.add_argument("--remote-url", required=True)
    p_gate.add_argument("--updates", default="")
    p_gate.set_defaults(func=cmd_pre_push_gate)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    sys.exit(main())
