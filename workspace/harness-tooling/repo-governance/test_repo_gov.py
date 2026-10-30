"""Unit tests for workspace/harness-tooling/repo-governance/repo_gov.py.

Covers:
1. Public safety scan across tracked working tree, arbitrary git refs, plain
   directories, and local denylist extensions.
2. Commit history range scan (--history-range <base..head>) catching leaks
   removed in later commits while passing clean history ranges.
3. Branch naming validation across main, dev alpha lines, dev/opt-evo/*,
   delivery/*, release/*, and rejected unmanaged names.
4. Organization boundary checks (homelab-private blocked from CelHive,
   DISABLED_MANUAL_RELEASE_SYNC_ONLY pushurl blocked, refs/notes/* blocked on
   GitHub while permitted on LAN bare hubs).
5. Worktree-aware pre-push hook generation and live end-to-end execution.

Design invariant: All synthetic test tokens are assembled dynamically at runtime
so this test file itself contains zero literal forbidden tokens.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import repo_gov


def _init_git_repo(path: Path) -> None:
    subprocess.run(["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test Bot"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "bot@example.invalid"], cwd=path, check=True, capture_output=True)


def _git_commit_all(path: Path, message: str) -> str:
    subprocess.run(["git", "add", "-A"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", message], cwd=path, check=True, capture_output=True)
    out = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path, text=True)
    return out.strip()


class TestRepoGovernance(unittest.TestCase):
    def test_branch_naming_validation(self) -> None:
        valid_branches = [
            "main",
            "refs/heads/main",
            "dev/0.2.x-alpha.x",
            "dev/0.2.x-alpha.3",
            "dev/opt-evo/xl-c3-baseline-135/lan-preflight-ready-r1",
            "delivery/xl-docbench-source-20261028-r3",
            "release/0.2.0",
            "release/v0.2.0-alpha.1",
        ]
        for br in valid_branches:
            with self.subTest(branch=br):
                ok, category, _ = repo_gov.validate_branch_name(br)
                self.assertTrue(ok, f"Expected valid branch: {br}")
                self.assertIsNotNone(category)

        invalid_branches = [
            "feature/random",
            "dev/0.2.x-alpha.x-xl-docbench-20261016",
            "dev/opt-evo/UPPERCASE/ckpt",
            "dev/opt-evo/missing-checkpoint",
            "wip",
            "codex/scratch-branch",
        ]
        for br in invalid_branches:
            with self.subTest(branch=br):
                ok, _, _ = repo_gov.validate_branch_name(br)
                self.assertFalse(ok, f"Expected invalid branch: {br}")

    def test_org_boundary_enforcement(self) -> None:
        # homelab-private -> CelHive is strictly blocked
        ok, reasons = repo_gov.evaluate_org_boundary(
            role="homelab-private",
            remote_url="https://github.com/CelHive-AI/gptgrep-opt.git",
        )
        self.assertFalse(ok)
        self.assertTrue(any("homelab-private" in r for r in reasons))

        # Disabled pushurl sentinel is blocked
        ok_dis, reasons_dis = repo_gov.evaluate_org_boundary(
            role="core",
            remote_url="DISABLED_MANUAL_RELEASE_SYNC_ONLY",
        )
        self.assertFalse(ok_dis)
        self.assertTrue(any("manual release sync" in r for r in reasons_dis))

        # refs/notes/* to GitHub is blocked for every role
        ok_notes, reasons_notes = repo_gov.evaluate_org_boundary(
            role="core",
            remote_url="https://github.com/lwyBZss8924d/gptgrep.git",
            refspecs=["refs/notes/commits:refs/notes/commits"],
        )
        self.assertFalse(ok_notes)
        self.assertTrue(any("PoUW notes" in r for r in reasons_notes))

        # refs/notes/* to LAN SSH bare hub is permitted
        ok_lan, reasons_lan = repo_gov.evaluate_org_boundary(
            role="core",
            remote_url="ssh://lan-hub.internal/srv/git/gptgrep.git",
            refspecs=["refs/notes/commits:refs/notes/commits"],
        )
        self.assertTrue(ok_lan, f"LAN hub should allow notes sync: {reasons_lan}")

        # Valid dev/0.2.x-alpha.x push to personal public GitHub passes boundary check
        ok_pub, reasons_pub = repo_gov.evaluate_org_boundary(
            role="core",
            remote_url="https://github.com/lwyBZss8924d/gptgrep.git",
            refspecs=["dev/0.2.x-alpha.x:refs/heads/dev/0.2.x-alpha.x"],
        )
        self.assertTrue(ok_pub, f"Expected valid personal GitHub branch push: {reasons_pub}")

    def test_public_safety_scan_and_denylist(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plain = Path(tmp) / "plain"
            plain.mkdir()
            # Allowed placeholder paths must pass
            (plain / "docs.md").write_text(
                "Example paths: /Users/YOU/.config, /Users/runner/work, /Volumes/fixture/data, "
                "op://AGENTS/<ITEM_UUID>/credential\n",
                encoding="utf-8",
            )
            rc_clean = repo_gov.main(["check-public-safety", "--dir", str(plain)])
            self.assertEqual(rc_clean, 0)

            # Construct synthetic forbidden tokens dynamically (zero literal leaks in source)
            synthetic_ip = ".".join(["10", "1", "1", "245"])
            synthetic_user_path = "/" + "Users" + "/" + " workstation_op" .strip() + "/repo"
            synthetic_op = "op://AGENTS/" + ("a1b2c3d4e5f6g7h8i9j0k1l2m3") + "/credential"
            synthetic_tok = "sk-or-v1-" + ("9" * 28)

            (plain / "leak.txt").write_text(
                f"host={synthetic_ip}\npath={synthetic_user_path}\nop={synthetic_op}\nkey={synthetic_tok}\n",
                encoding="utf-8",
            )
            rc_leak = repo_gov.main(["check-public-safety", "--dir", str(plain)])
            self.assertEqual(rc_leak, 1)

            # Custom local denylist test
            (plain / "leak.txt").unlink()
            (plain / "custom.txt").write_text("project = internal-canary-id-9988\n", encoding="utf-8")
            denylist = Path(tmp) / "denylist.txt"
            denylist.write_text("# comment\nliteral:internal-canary-id-9988\n", encoding="utf-8")
            rc_deny = repo_gov.main(
                ["check-public-safety", "--dir", str(plain), "--denylist-file", str(denylist)]
            )
            self.assertEqual(rc_deny, 1)

    def test_history_range_scan_catches_reverted_leak(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            _init_git_repo(repo)

            (repo / "README.md").write_text("# Clean baseline\n", encoding="utf-8")
            base_sha = _git_commit_all(repo, "chore: initial baseline")

            # Commit 2 introduces a forbidden token
            bad_secret = "gho_" + ("X" * 26)
            (repo / "config.txt").write_text(f"token={bad_secret}\n", encoding="utf-8")
            _git_commit_all(repo, "wip: accidental token")

            # Commit 3 removes the token from HEAD tree
            (repo / "config.txt").write_text("token=<REDACTED>\n", encoding="utf-8")
            dirty_head = _git_commit_all(repo, "fix: redact token")

            # Head tree alone is clean:
            rc_head_only = repo_gov.main(
                ["check-public-safety", "--repo", str(repo), "--ref", dirty_head]
            )
            self.assertEqual(rc_head_only, 0)

            # History range base..dirty_head MUST fail because Commit 2 leaked bad_secret!
            rc_hist_dirty = repo_gov.main(
                [
                    "check-public-safety",
                    "--repo",
                    str(repo),
                    "--ref",
                    dirty_head,
                    "--history-range",
                    f"{base_sha}..{dirty_head}",
                ]
            )
            self.assertEqual(rc_hist_dirty, 1)

            # History-clean commit-tree squash rooted at base_sha MUST pass both head and history!
            tree_sha = subprocess.check_output(
                ["git", "rev-parse", f"{dirty_head}^{{tree}}"], cwd=repo, text=True
            ).strip()
            clean_sha = subprocess.check_output(
                ["git", "commit-tree", tree_sha, "-p", base_sha, "-m", "feat: clean delivery"],
                cwd=repo,
                text=True,
            ).strip()
            rc_hist_clean = repo_gov.main(
                [
                    "check-public-safety",
                    "--repo",
                    str(repo),
                    "--ref",
                    clean_sha,
                    "--history-range",
                    f"{base_sha}..{clean_sha}",
                ]
            )
            self.assertEqual(rc_hist_clean, 0)

    def test_worktree_aware_hook_installation_and_gate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            primary = Path(tmp) / "primary"
            primary.mkdir()
            _init_git_repo(primary)
            (primary / "README.md").write_text("# Clean\n", encoding="utf-8")
            base_sha = _git_commit_all(primary, "initial")

            wt = Path(tmp) / "wt"
            subprocess.run(
                ["git", "-C", str(primary), "worktree", "add", "-b", "dev/0.2.x-alpha.x", str(wt), "main"],
                check=True,
                capture_output=True,
            )

            # Install hook from linked worktree -> must land in primary's common .git/hooks/pre-push
            rc = repo_gov.main(["install-hooks", "--repo", str(wt), "--role", "core"])
            self.assertEqual(rc, 0)

            hook_path = primary / ".git" / "hooks" / "pre-push"
            self.assertTrue(hook_path.is_file())
            self.assertTrue(bool(os.stat(hook_path).st_mode & stat.S_IXUSR))

            # Simulate pre-push blocking refs/notes/commits to GitHub
            notes_update = f"refs/notes/commits {base_sha} refs/notes/commits {repo_gov.ZERO_SHA}"
            rc_block = repo_gov.main(
                [
                    "pre-push-gate",
                    "--repo",
                    str(wt),
                    "--role",
                    "core",
                    "--remote-name",
                    "origin",
                    "--remote-url",
                    "https://github.com/lwyBZss8924d/gptgrep.git",
                    "--updates",
                    notes_update,
                ]
            )
            self.assertEqual(rc_block, 1)

            # Simulate pre-push allowing clean dev/0.2.x-alpha.x to GitHub
            head_update = f"refs/heads/dev/0.2.x-alpha.x {base_sha} refs/heads/dev/0.2.x-alpha.x {repo_gov.ZERO_SHA}"
            rc_allow = repo_gov.main(
                [
                    "pre-push-gate",
                    "--repo",
                    str(wt),
                    "--role",
                    "core",
                    "--remote-name",
                    "origin",
                    "--remote-url",
                    "https://github.com/lwyBZss8924d/gptgrep.git",
                    "--updates",
                    head_update,
                ]
            )
            self.assertEqual(rc_allow, 0)


if __name__ == "__main__":
    unittest.main()
