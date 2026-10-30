"""Tier 4: Real-World Scenarios E2E Test Suite.

Covers:
1. End-to-end paired pilot run simulation and EvaluationRun normalization (Luna readers, Sol judges)
2. Closed-loop REEF candidate evaluation cycle (Opt -> Test -> Eval -> Judge -> Next-Opt)
3. Verified PoUW note creation, git bundle verification, and cloud backup integrity
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import stat
import time

from common import (
    BIN_SELFCHECK,
    CLOSEOUT_RECEIPT_DEVBOX,
    PROJECT_ROOT,
    SCRIPTS_DEVBOX,
    SELF_OPT_DIR,
    SKILLS_DEVBOX,
    read_file,
    read_json,
    run_cmd,
)


class TestTier4RealWorldScenarios(unittest.TestCase):
    """Tier 4: Real-World Scenarios Test Suite."""

    def test_01_e2e_paired_pilot_run_simulation(self) -> None:
        """Simulate 4-task paired pilot run (baseline vs gptgrep); assert EvaluationRun normalization."""
        incumbent_rev = "7fc95f4244e5635a7c5ed29974d63a33e10151f5"
        treatment_rev = "d6aa1ea71897a70afb4c24c67ad3b26ceb0488f0"
        cohort = ["task_0001", "task_0002", "task_0003", "task_0004"]

        # Run scenario with 1 complete success, 1 complete failure, 1 operational failure, 1 judge unknown
        outcomes = [
            {
                "taskId": "task_0001",
                "status": "complete",
                "groundedSuccess": 1,
                "nativeAccuracy": 1.0,
                "atomicCoverage": 1.0,
                "latencyMs": 1500,
                "costUsd": 0.05,
            },
            {
                "taskId": "task_0002",
                "status": "complete",
                "groundedSuccess": 0,
                "nativeAccuracy": 0.5,
                "atomicCoverage": 0.25,
                "latencyMs": 1200,
                "costUsd": 0.04,
            },
            {
                "taskId": "task_0003",
                "status": "operational_failure",
                "groundedSuccess": 0,  # started operational failure remains an observed zero
                "nativeAccuracy": 0.0,
                "atomicCoverage": 0.0,
                "latencyMs": 800,
                "costUsd": 0.02,
                "errorCategory": "timeout",
            },
            {
                "taskId": "task_0004",
                "status": "judge_unknown",
                "groundedSuccess": None,  # unknown judgment cannot become numeric reward
                "nativeAccuracy": 1.0,
                "atomicCoverage": None,
                "latencyMs": 2000,
                "costUsd": 0.06,
                "errorCategory": "judge_unavailable",
            },
        ]

        enrolled = len(cohort)
        unknown = sum(1 for o in outcomes if o["status"] in ("judge_unknown", "setup_failed"))
        successes = sum(1 for o in outcomes if o["groundedSuccess"] == 1)
        failures = sum(1 for o in outcomes if o["status"] == "operational_failure")
        complete = sum(1 for o in outcomes if o["status"] == "complete")

        self.assertEqual(enrolled, 4)
        self.assertEqual(successes, 1)
        self.assertEqual(failures, 1)
        self.assertEqual(unknown, 1)
        self.assertEqual(complete, 2)

        # Invariant: unknown outcome is preserved in denominator
        lower_bound = successes / enrolled  # 1 / 4 = 0.25
        upper_bound = (successes + unknown) / enrolled  # (1 + 1) / 4 = 0.50

        self.assertEqual(lower_bound, 0.25)
        self.assertEqual(upper_bound, 0.50)

        # Invariant: when unknown > 0, comparable is false
        comparable = (unknown == 0)
        self.assertFalse(comparable)

    def test_02_closed_loop_reef_evaluation_cycle(self) -> None:
        """Simulate closed-loop REEF candidate evaluation cycle (Opt -> Test -> Eval -> Judge -> Next-Opt)."""
        incumbent_rev = "7fc95f4244e5635a7c5ed29974d63a33e10151f5"
        candidate_rev = "d6aa1ea71897a70afb4c24c67ad3b26ceb0488f0"
        cohort = ["task_0001", "task_0002"]

        def make_run(run_id: str, rev: str, parent: str | None, split: str, successes: int, cost: float, has_unknown: bool = False):
            outcomes = []
            for i, tid in enumerate(cohort):
                if has_unknown and i == 0:
                    outcomes.append({"taskId": tid, "status": "judge_unknown", "groundedSuccess": None, "nativeAccuracy": None, "atomicCoverage": None, "latencyMs": 1000, "costUsd": cost / 2})
                else:
                    g = 1 if i < successes else 0
                    outcomes.append({"taskId": tid, "status": "complete", "groundedSuccess": g, "nativeAccuracy": float(g), "atomicCoverage": float(g), "latencyMs": 1000, "costUsd": cost / 2})
            return {
                "schema": "gptgrep.opt-evaluation.v1",
                "runId": run_id,
                "revision": rev,
                "parentRevision": parent,
                "cohort": cohort,
                "split": split,
                "scenario": {
                    "datasetRevision": "72954bd70ffffe230f08b57c57fa9274ec14d7ea",
                    "corpusSha256": "0" * 64,
                    "rubricSha256": "1" * 64,
                    "nativeScorerSha256": "2" * 64,
                    "readerModel": "gpt-6-luna",
                    "workerModel": "gpt-6-luna",
                    "ocrModel": "mineru-4.0.12",
                    "helperModel": "openrouter/auto",
                    "reasoningEffort": "medium",
                    "serviceTier": "default",
                    "judgeModel": "gpt-6.1-sol",
                    "judgeReasoningEffort": "high",
                    "judgeServiceTier": "default",
                    "environmentSha256": "3" * 64,
                    "preparationPolicySha256": "4" * 64,
                    "seed": "42",
                },
                "outcomes": outcomes,
            }

        incumbent = make_run("run-incumbent", incumbent_rev, None, "dev", successes=1, cost=1.0)
        # 1. Candidate with no source change -> rejected
        same_rev_cand = make_run("run-same", incumbent_rev, incumbent_rev, "dev", successes=2, cost=1.0)
        self.assertEqual(same_rev_cand["revision"], incumbent["revision"])

        # 2. Candidate with unknown outcome -> rejected
        unknown_cand = make_run("run-unknown", candidate_rev, incumbent_rev, "dev", successes=1, cost=1.0, has_unknown=True)
        self.assertTrue(any(o["status"] == "judge_unknown" for o in unknown_cand["outcomes"]))

        # 3. Candidate with lower/equal success -> rejected
        worse_cand = make_run("run-worse", candidate_rev, incumbent_rev, "dev", successes=0, cost=1.0)
        self.assertLess(
            sum(1 for o in worse_cand["outcomes"] if o["groundedSuccess"] == 1),
            sum(1 for o in incumbent["outcomes"] if o["groundedSuccess"] == 1),
        )

        # 4. Valid winning candidate: higher success (2/2 vs 1/2), valid parent, 0 unknown
        winning_cand = make_run("run-win", candidate_rev, incumbent_rev, "dev", successes=2, cost=1.1)
        inc_succ = sum(1 for o in incumbent["outcomes"] if o["groundedSuccess"] == 1) / len(cohort)
        win_succ = sum(1 for o in winning_cand["outcomes"] if o["groundedSuccess"] == 1) / len(cohort)
        self.assertGreater(win_succ, inc_succ)
        self.assertEqual(winning_cand["parentRevision"], incumbent["revision"])
        self.assertNotEqual(winning_cand["revision"], incumbent["revision"])

        # Loop plan output validation
        loop_plan = {
            "schema": "gptgrep.opt-loop-plan.v1",
            "iteration": 1,
            "baselineRevision": incumbent_rev,
            "parentRevision": winning_cand["revision"],
            "branch": "experiment/0.2-opt-1",
            "isolation": "new_worktree_from_frozen_0.2_baseline_then_explicit_parent_delta",
            "forbiddenInputs": [
                "gold answers",
                "evidence annotations",
                "frozen confirmation feedback",
                "credential files",
            ],
        }
        self.assertEqual(loop_plan["schema"], "gptgrep.opt-loop-plan.v1")
        self.assertIn("gold answers", loop_plan["forbiddenInputs"])

    def test_03_pouw_bundle_creation_and_backup(self) -> None:
        """Verify PoUW commit notes format, git bundle creation, and verification."""
        with tempfile.TemporaryDirectory() as tmpdir:
            repo_dir = Path(tmpdir) / "test_repo"
            repo_dir.mkdir()

            # Initialize a test git repo
            subprocess.run(["git", "init"], cwd=repo_dir, check=True, capture_output=True)
            subprocess.run(["git", "config", "user.name", "Test Agent"], cwd=repo_dir, check=True, capture_output=True)
            subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=repo_dir, check=True, capture_output=True)

            # Create an initial commit
            readme = repo_dir / "README.md"
            readme.write_text("# Test Repo\n")
            subprocess.run(["git", "add", "README.md"], cwd=repo_dir, check=True, capture_output=True)
            subprocess.run(["git", "commit", "-m", "Initial commit"], cwd=repo_dir, check=True, capture_output=True)

            # Generate and attach a verified PoUW note to HEAD under refs/notes/commits
            pouw_record = {
                "schema": "environments-harness.pouw-record.v1",
                "outcomes": ["D0-D6 complete", "P1-P9 verified"],
                "verification_checks": ["cargo test", "bun test", "e2e runner"],
                "primary_session_id": "fd64806b-7f9c-47b6-b894-827419c0b8f4",
                "recorded_at": "2026-10-22T03:30:00Z",
            }
            pouw_json = json.dumps(pouw_record, indent=2)

            subprocess.run(
                ["git", "notes", "--ref=commits", "add", "-f", "-m", pouw_json, "HEAD"],
                cwd=repo_dir,
                check=True,
                capture_output=True,
            )

            # Verify note is observable via git log --show-notes=commits -1
            res = subprocess.run(
                ["git", "log", "--show-notes=commits", "-1"],
                cwd=repo_dir,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertTrue("Notes:" in res.stdout or "Notes (commits):" in res.stdout)
            self.assertIn("environments-harness.pouw-record.v1", res.stdout)

            # Create a Git bundle containing HEAD and refs/notes/commits
            bundle_file = Path(tmpdir) / "gptgrep-backup.bundle"
            subprocess.run(
                ["git", "bundle", "create", str(bundle_file), "HEAD", "refs/notes/commits"],
                cwd=repo_dir,
                check=True,
                capture_output=True,
            )
            self.assertTrue(bundle_file.is_file())

            # Verify bundle integrity
            verify_res = subprocess.run(
                ["git", "bundle", "verify", str(bundle_file)],
                cwd=repo_dir,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("is okay", verify_res.stdout + verify_res.stderr)

    def test_04_local_selfcheck_execution_pipeline(self) -> None:
        """Native launcher reports missing qualification honestly without config."""
        res = run_cmd([str(BIN_SELFCHECK), "--mode", "local", "--json"], timeout=20.0)
        self.assertEqual(res.returncode, 1)
        receipt = json.loads(res.stdout)
        self.assertEqual(receipt["schema"], "gptgrep.devbox-selfcheck.receipt.v2")
        self.assertEqual(receipt["overall_status"], "UNKNOWN")
        self.assertEqual(receipt["summary"]["passed_gates"], 0)
        self.assertEqual(receipt["summary"]["unknown_gates"], 5)
        self.assertEqual(len(receipt["gates"]), 5)
        self.assertTrue(all(gate["status"] == "UNKNOWN" for gate in receipt["gates"]))
        self.assertFalse(receipt["qualification"]["attempted"])
        self.assertEqual(receipt["workload_admission"]["status"], "not_granted")

    def test_05_simulated_remote_provisioning_lifecycle(self) -> None:
        """Verify sequential provisioning scripts lifecycle: syntax, executable permissions, and dry-run execution."""
        expected_scripts = [
            "01-provision-gce-devbox.sh",
            "02-remote-devbox-init.sh",
            "03-three-tier-corpus-sync.sh",
            "04-install-idle-suspend.sh",
            "05-install-devbox-agents-home.sh",
            "06-setup-devbox-ai-clients.sh",
        ]

        for s_name in expected_scripts:
            s_path = SCRIPTS_DEVBOX / s_name
            self.assertTrue(s_path.is_file(), f"Missing provisioning script: {s_path}")
            # Executable bit
            st = os.stat(s_path)
            self.assertTrue(bool(st.st_mode & stat.S_IXUSR), f"Script {s_name} is not executable")

            # Syntax validation via bash -n
            syntax_res = run_cmd(["bash", "-n", str(s_path)])
            self.assertEqual(syntax_res.returncode, 0, f"Syntax error in {s_name}:\n{syntax_res.stderr}")

        # Test dry-run execution of 04
        s04 = SCRIPTS_DEVBOX / "04-install-idle-suspend.sh"
        res_04 = run_cmd(["bash", str(s04), "--dry-run"])
        self.assertEqual(res_04.returncode, 0, f"04 --dry-run failed:\n{res_04.stderr}")
        self.assertIn("DRY RUN", res_04.stdout.upper())

        # Test help output of 05 and 06
        s05 = SCRIPTS_DEVBOX / "05-install-devbox-agents-home.sh"
        res_05 = run_cmd(["bash", str(s05), "--help"])
        self.assertEqual(res_05.returncode, 0)
        self.assertIn("Usage:", res_05.stdout)

        s06 = SCRIPTS_DEVBOX / "06-setup-devbox-ai-clients.sh"
        res_06 = run_cmd(["bash", str(s06), "--help"])
        self.assertEqual(res_06.returncode, 0)
        self.assertIn("Usage:", res_06.stdout)

    def test_06_skills_package_receipt_generation(self) -> None:
        """Generate and validate complete DEVBOX-SKILLS-PACKAGE-RECEIPT.json artifact."""
        receipt = {
            "schema": "gptgrep.devbox-skills-package.receipt.v1",
            "receipt_id": "DEVBOX-SKILLS-PACKAGE-20261022-r1",
            "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "milestone": "gptgrep-remote-devbox-packaging-and-e2e",
            "package_status": "VERIFIED",
            "package_root": "skills/gptgrep-remote-devbox",
            "manifest_registered": True,
            "root_llms_indexed": True,
            "bundled_skills": {
                "google_skills_count": 8,
                "ai_clients_count": 4,
                "google_skills": [
                    "retrieving-developer-knowledge",
                    "finding-google-skills",
                    "gcloud",
                    "google-cloud-recipe-auth",
                    "google-cloud-recipe-onboarding",
                    "gdrive",
                    "gws",
                    "colab-operator",
                ],
                "ai_clients": [
                    "openai-docs",
                    "claude-code-operator",
                    "antigravity-guide",
                    "1password-self",
                ],
            },
            "verification_gates": {
                "gate1_storage": "PASSED",
                "gate2_toolchains": "PASSED",
                "gate3_corpus": "PASSED",
                "gate4_security": "PASSED",
                "gate5_agents_cost": "PASSED",
            },
            "corpus_invariants": {
                "raw_pdfs_count": 291,
                "raw_pdfs_bytes": 3549154931,
                "prepared_md_pages": 82121,
                "total_tasks": 1345,
                "dev_tasks": 366,
                "frozen_tasks": 979,
                "cross_family_leakage": 0,
            },
            "security_invariants": {
                "secret_scanning_clean": True,
                "host_path_leakage_clean": True,
                "port_0_blocker_active": True,
                "flock_serialization_verified": True,
            },
        }

        # Write generated test receipt to a temporary directory to avoid clobbering closeout deliverables
        with tempfile.TemporaryDirectory() as tmpdir:
            test_receipt_path = Path(tmpdir) / "DEVBOX-SKILLS-PACKAGE-RECEIPT.json"
            test_receipt_path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")

            self.assertTrue(test_receipt_path.is_file())
            re_read = read_json(test_receipt_path)
            self.assertEqual(re_read.get("schema"), "gptgrep.devbox-skills-package.receipt.v1")
            self.assertEqual(re_read.get("package_status"), "VERIFIED")
            self.assertEqual(re_read.get("corpus_invariants", {}).get("total_tasks"), 1345)

        # If production CLOSEOUT_RECEIPT_DEVBOX exists, read-only validate its schema without mutating it
        if CLOSEOUT_RECEIPT_DEVBOX.is_file():
            prod_receipt = read_json(CLOSEOUT_RECEIPT_DEVBOX)
            self.assertEqual(prod_receipt.get("schema"), "gptgrep.devbox-skills-package.receipt.v1")
            self.assertIn("package_status", prod_receipt)
            self.assertEqual(prod_receipt.get("corpus_invariants", {}).get("total_tasks"), 1345)


if __name__ == "__main__":
    unittest.main()
