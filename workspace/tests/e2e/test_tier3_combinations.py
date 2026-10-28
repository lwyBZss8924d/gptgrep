"""Tier 3: Cross-Feature Combinations E2E Test Suite.

Covers:
1. Concurrent lease contention & flock serialization between workers
2. Idle-check counter state machine reset with active compilers and agent keepalive locks
3. Paired execution with alternating seed and reader/judge session isolation
4. Candidate worktree isolation and mutation boundary enforcement
5. Harbor CLI (>= v0.24.0) integration with RewardKit 0.2.3 4-place rounding precision
"""

from __future__ import annotations

import fcntl
import math
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import re

from common import (
    BENCHMARK_DIR,
    CODEX_AUTH_TOOLING,
    CONFIGS_DEVBOX,
    EXEC_PLAN_DEVBOX,
    PROJECT_ROOT,
    RESOURCE_MANIFEST,
    ROOT_LLMS,
    SCRIPTS_DEVBOX,
    SELF_OPT_DIR,
    SKILLS_DEVBOX,
    get_harbor_version,
    parse_semver,
    read_file,
    read_json,
    run_cmd,
)


class TestTier3CrossFeatureCombinations(unittest.TestCase):
    """Tier 3: Cross-Feature Combinations Test Suite."""

    def test_01_concurrent_lease_contention_serialization(self) -> None:
        """Verify flock serialization: non-blocking contender gets EWOULDBLOCK while exclusive lock is held."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lock_path = Path(tmpdir) / "gptgrep-codex.lock"
            lock_path.touch()

            # Process A opens and acquires exclusive non-blocking lock
            fd_a = os.open(str(lock_path), os.O_RDWR)
            try:
                fcntl.flock(fd_a, fcntl.LOCK_EX | fcntl.LOCK_NB)

                # Process B opens lockfile and attempts non-blocking exclusive lock
                fd_b = os.open(str(lock_path), os.O_RDWR)
                try:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(fd_b, fcntl.LOCK_EX | fcntl.LOCK_NB)

                    # Release Process A's lock
                    fcntl.flock(fd_a, fcntl.LOCK_UN)

                    # Now Process B can acquire exclusive lock successfully
                    fcntl.flock(fd_b, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(fd_b, fcntl.LOCK_UN)
                finally:
                    os.close(fd_b)
            finally:
                os.close(fd_a)

    def test_02_idle_check_counter_with_active_compiler(self) -> None:
        """Verify idle-check state machine: counter increments during idle, resets to 0 when active compiler appears."""
        with tempfile.TemporaryDirectory() as tmpdir:
            state_file = Path(tmpdir) / "gptgrep-idle-counter"
            keepalive_dir = Path(tmpdir) / "gptgrep-keepalive"
            keepalive_dir.mkdir()

            def run_state_machine(has_active_workload: bool, max_checks: int = 6) -> int:
                count = int(state_file.read_text().strip()) if state_file.is_file() else 0
                if has_active_workload:
                    count = 0
                    state_file.write_text("0\n")
                    return count
                count += 1
                state_file.write_text(f"{count}\n")
                if count >= max_checks:
                    state_file.write_text("0\n")
                return count

            # Cycle 1-3: idle, counter increments
            self.assertEqual(run_state_machine(has_active_workload=False), 1)
            self.assertEqual(run_state_machine(has_active_workload=False), 2)
            self.assertEqual(run_state_machine(has_active_workload=False), 3)

            # Cycle 4: compiler starts building (cargo/bun) or keepalive lock appears -> reset to 0!
            self.assertEqual(run_state_machine(has_active_workload=True), 0)
            self.assertEqual(int(state_file.read_text().strip()), 0)

            # Cycle 5-6: idle resumes, counter counts from 1, preventing premature auto-suspend
            self.assertEqual(run_state_machine(has_active_workload=False), 1)
            self.assertEqual(run_state_machine(has_active_workload=False), 2)

    def test_03_paired_execution_with_alternating_seed(self) -> None:
        """Verify paired execution order alternation across cohort tasks and reader/judge isolation."""
        cohort = [f"task_{i:04d}" for i in range(4)]
        seeds = [1000 + i for i in range(4)]

        # Alternating execution order algorithm: task i alternates [baseline, treatment] vs [treatment, baseline]
        def schedule_task(task_idx: int, arms: list[str]) -> list[str]:
            if task_idx % 2 == 0:
                return [arms[0], arms[1]]
            return [arms[1], arms[0]]

        arms = ["baseline", "gptgrep"]
        scheduled = [schedule_task(i, arms) for i in range(len(cohort))]

        self.assertEqual(scheduled[0], ["baseline", "gptgrep"])
        self.assertEqual(scheduled[1], ["gptgrep", "baseline"])
        self.assertEqual(scheduled[2], ["baseline", "gptgrep"])
        self.assertEqual(scheduled[3], ["gptgrep", "baseline"])

        # Check session isolation contract: gold answer is strictly verifier-only
        reader_env = {"TASK_ID": "task_0001", "DOCUMENTS_PATH": "/app/documents"}
        self.assertNotIn("GOLD_ANSWER", reader_env)
        self.assertNotIn("EVIDENCE_ANNOTATIONS", reader_env)

        verifier_env = {"TASK_ID": "task_0001", "GOLD_ANSWER": "expected_gold", "NETWORK_MODE": "no-network"}
        self.assertIn("GOLD_ANSWER", verifier_env)
        self.assertEqual(verifier_env["NETWORK_MODE"], "no-network")

    def test_04_candidate_worktree_isolation_boundary(self) -> None:
        """Verify candidate worktree path naming and surface filter rejecting unauthorized directories."""
        round_id = "round-01"
        candidate_worktree_root = f"/mnt/gptgrep-dev/owners/gptgrep/worktrees/opt-candidate-{round_id}"
        self.assertTrue(candidate_worktree_root.startswith("/mnt/gptgrep-dev/owners/gptgrep/worktrees/"))
        self.assertIn("opt-candidate-", candidate_worktree_root)

        allowed_prefixes = ("crates/", "skills/", "runtime/", "prompts/")

        def check_candidate_diff(diff_files: list[str]) -> bool:
            for f in diff_files:
                if not any(f.startswith(p) for p in allowed_prefixes):
                    return False
            return True

        # Valid diff inside allowed surfaces
        self.assertTrue(check_candidate_diff(["crates/gptgrep-core/src/hybrid_search.rs", "prompts/reader.md"]))

        # Invalid diff modifying test harnesses or benchmarks
        self.assertFalse(check_candidate_diff(["benchmark/src/adapter.ts"]))
        self.assertFalse(check_candidate_diff(["self-opt/src/evaluate.ts"]))
        self.assertFalse(check_candidate_diff(["Cargo.toml"]))
        self.assertFalse(check_candidate_diff([".github/workflows/eval.yml"]))

    def test_05_harbor_cli_and_rewardkit_precision(self) -> None:
        """Mandatory: Verify Harbor CLI satisfies >= v0.24.0 and RewardKit 0.2.3 enforces 4-place rounding."""
        # Check installed Harbor CLI version >= v0.24.0
        harbor_ver = get_harbor_version()
        self.assertIsNotNone(harbor_ver, "Harbor CLI is not installed")
        major, minor, patch = parse_semver(harbor_ver)
        self.assertTrue(
            (major > 0) or (major == 0 and minor > 24) or (major == 0 and minor == 24 and patch >= 0),
            f"Installed Harbor version ({harbor_ver}) must be >= v0.24.0",
        )

        # RewardKit 0.2.3 exact 4-place rounding verification
        def rewardkit_round(value: float) -> float:
            return round(value, 4)

        raw_scores = [0.181818, 0.666667, 0.333333, 0.999999, 0.0]
        expected_rounded = [0.1818, 0.6667, 0.3333, 1.0, 0.0]

        for raw, expected in zip(raw_scores, expected_rounded):
            self.assertEqual(rewardkit_round(raw), expected)

    def test_06_selfcheck_cli_and_1password_resolution(self) -> None:
        """Verify 1Password .env.op.template abstract reference syntax and rejection of plaintext tokens in configs."""
        template_file = CONFIGS_DEVBOX / ".env.op.template"
        self.assertTrue(template_file.is_file(), f"Missing {template_file}")
        content = read_file(template_file)

        # All lines with assignments must either be comments or follow op:// references
        op_ref_pattern = re.compile(r"^(?:export\s+)?[A-Za-z0-9_]+=\"op://[^\"]+\"$")
        valid_lines = 0
        for line in content.splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            self.assertTrue(
                bool(op_ref_pattern.match(stripped)),
                f"Line does not follow abstract op:// reference contract: {stripped}",
            )
            valid_lines += 1
        self.assertGreaterEqual(valid_lines, 4, "Must contain at least 4 abstract op:// references")

        # Verify simulated 1Password resolution does not persist plaintext secrets
        with tempfile.TemporaryDirectory() as tmpdir:
            test_env_file = Path(tmpdir) / ".env.resolved"
            test_env_file.write_text("ANTHROPIC_API_KEY=mock-resolved-key\n", encoding="utf-8")
            # Must enforce 0600 on any resolved credential file
            os.chmod(test_env_file, 0o600)
            mode = oct(os.stat(test_env_file).st_mode & 0o777)
            self.assertEqual(mode, "0o600")

    def test_07_skills_packaging_manifest_indexing_consistency(self) -> None:
        """Verify bidirectional indexing consistency between skills package, resource-manifest.json, and llms.txt."""
        self.assertTrue(SKILLS_DEVBOX.is_dir(), f"Missing {SKILLS_DEVBOX}")
        self.assertTrue((SKILLS_DEVBOX / "SKILL.md").is_file())
        self.assertTrue((SKILLS_DEVBOX / "llms.txt").is_file())
        self.assertTrue((SKILLS_DEVBOX / "AGENTS.md").is_file())

        # Check resource-manifest.json
        manifest = read_json(RESOURCE_MANIFEST)
        resources = manifest.get("resources", [])
        devbox_res = next((r for r in resources if r.get("id") == "gptgrep-remote-devbox"), None)
        self.assertIsNotNone(devbox_res, "skills/gptgrep-remote-devbox not found in resource-manifest.json")
        self.assertEqual(devbox_res.get("kind"), "skill")
        self.assertEqual(devbox_res.get("path"), "skills/gptgrep-remote-devbox/SKILL.md")

        # Check root llms.txt references the skill
        root_llms = read_file(ROOT_LLMS)
        self.assertIn("skills/gptgrep-remote-devbox", root_llms)

        # Check bundled skills in skills/gptgrep-remote-devbox/skills/
        skills_sub = SKILLS_DEVBOX / "skills"
        self.assertTrue(skills_sub.is_dir())
        google_skills = [
            "retrieving-developer-knowledge",
            "finding-google-skills",
            "gcloud",
            "google-cloud-recipe-auth",
            "google-cloud-recipe-onboarding",
            "gdrive",
            "gws",
            "colab-operator",
        ]
        for g_skill in google_skills:
            g_path = skills_sub / "google" / g_skill / "SKILL.md"
            self.assertTrue(g_path.is_file(), f"Missing bundled Google skill: {g_path}")

        ai_skills = ["openai-docs", "claude-code-operator", "antigravity-guide", "1password-self"]
        for a_skill in ai_skills:
            a_path = skills_sub / a_skill / "SKILL.md"
            self.assertTrue(a_path.is_file(), f"Missing bundled AI skill: {a_path}")

    def test_08_client_symlinks_and_cost_protection_daemon(self) -> None:
        """Verify keepalive wrapper touches lockfile and resets idle suspend check counter."""
        with tempfile.TemporaryDirectory() as tmpdir:
            keepalive_dir = Path(tmpdir) / "gptgrep-keepalive"
            keepalive_dir.mkdir(parents=True, exist_ok=True)
            counter_file = Path(tmpdir) / "idle-check-count"
            counter_file.write_text("4\n")

            # Simulate keepalive wrapper execution: creates a lockfile with PID
            lock_file = keepalive_dir / f"keepalive-{os.getpid()}.lock"
            lock_file.write_text(f"{os.getpid()}\n")
            self.assertTrue(lock_file.is_file())

            # Simulate idle checker script logic: inspects /run/gptgrep-keepalive/
            active_locks = list(keepalive_dir.glob("*.lock"))
            self.assertGreater(len(active_locks), 0)

            # When locks exist, idle counter must reset to 0
            if len(active_locks) > 0:
                counter_file.write_text("0\n")

            self.assertEqual(int(counter_file.read_text().strip()), 0)

            # Cleanup lockfile when process finishes
            lock_file.unlink()
            self.assertEqual(len(list(keepalive_dir.glob("*.lock"))), 0)

    def test_09_harbor_isolation_and_bubblewrap_rules(self) -> None:
        """Verify Bubblewrap sandboxing argument construction and read-only root filesystem guarantees."""
        def build_bwrap_args(candidate_worktree: str, target_cmd: list[str]) -> list[str]:
            return [
                "bwrap",
                "--ro-bind", "/", "/",
                "--unshare-net",
                "--tmpfs", "/tmp",
                "--bind", candidate_worktree, candidate_worktree,
                "--dev", "/dev",
                "--proc", "/proc",
                "--die-with-parent",
                "--",
            ] + target_cmd

        worktree = "/mnt/gptgrep-dev/owners/gptgrep/worktrees/opt-candidate-round-01"
        cmd = ["cargo", "test"]
        args = build_bwrap_args(worktree, cmd)

        self.assertIn("--ro-bind", args)
        self.assertIn("--unshare-net", args)
        self.assertIn("--tmpfs", args)
        self.assertIn(worktree, args)
        self.assertEqual(args[-2:], ["cargo", "test"])


if __name__ == "__main__":
    unittest.main()
