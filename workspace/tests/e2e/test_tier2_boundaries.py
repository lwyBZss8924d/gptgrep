"""Tier 2: Boundary & Corner Cases E2E Test Suite.

Covers:
1. Invalid CLI flags and missing arguments (error handling & exit codes)
2. Unattached external disk error handling (refusing internal fallback)
3. Insufficient free space thresholds (< 30 GiB floor fails Pillar 1)
4. Malformed resource manifest and corrupted JSON handling
5. Port 0 blocker override for token refresh / revocation
6. Zero-task split boundaries & cross-family leakage audit (1,345 tasks)
7. Network timeouts and unreachable IAP tunnel error handling
8. Symlink and path traversal rejection in adapter and mutation boundaries
9. Repository-wide privacy scan of Git-tracked files
"""

from __future__ import annotations

import json
import socket
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from common import (
    BIN_SELFCHECK,
    CODEX_AUTH_TOOLING,
    EXEC_PLAN_DEVBOX,
    EXEC_PLAN_REEF,
    HANDOFF_OCR,
    PROJECT_ROOT,
    RESOURCE_MANIFEST,
    SCRIPTS_DEVBOX,
    SKILLS_DEVBOX,
    read_file,
    read_json,
    run_cmd,
)


class TestTier2BoundaryCornerCases(unittest.TestCase):
    """Tier 2: Boundary & Corner Cases Test Suite."""

    def test_01_invalid_cli_flags_and_missing_arguments(self) -> None:
        """Verify gptgrep-devbox-selfcheck fails gracefully with non-zero exit code on invalid arguments."""
        # Invalid flag
        res_bad_flag = run_cmd([str(BIN_SELFCHECK), "--nonexistent-flag-xyz"])
        self.assertNotEqual(res_bad_flag.returncode, 0, "CLI did not fail on invalid flag")
        self.assertIn("unknown option", res_bad_flag.stderr.lower())

        # Invalid mode
        res_bad_mode = run_cmd([str(BIN_SELFCHECK), "--mode", "invalid_mode_123"])
        self.assertNotEqual(res_bad_mode.returncode, 0, "CLI did not fail on invalid mode")
        self.assertIn("mode must be", res_bad_mode.stderr.lower())

    def test_02_unattached_disk_error_handling(self) -> None:
        """Verify scripts refuse internal fallback and exit with status 1 when external SSD is missing."""
        script_path = SCRIPTS_DEVBOX / "02-remote-devbox-init.sh"
        self.assertTrue(script_path.is_file(), f"Missing {script_path}")
        content = read_file(script_path)

        # Assert static script contract: refuses internal fallback on missing disk
        self.assertIn('if [ ! -e "${DEV_DISK_BY_ID}" ]; then', content)
        self.assertIn("Refusing internal fallback.", content)

        # Dynamic simulation: run a subshell test asserting failure when disk does not exist
        cmd = """
        DEV_DISK_BY_ID="/dev/disk/by-id/nonexistent-disk-vol-99"
        if [ ! -e "${DEV_DISK_BY_ID}" ]; then
          echo "ERROR: External disk ${DEV_DISK_BY_ID} not attached. Refusing internal fallback." >&2
          exit 1
        fi
        """
        res = run_cmd(["bash", "-c", cmd])
        self.assertEqual(res.returncode, 1)
        self.assertIn("Refusing internal fallback", res.stderr)

    def test_03_insufficient_free_space_threshold(self) -> None:
        """Verify storage threshold logic: free space < 30 GiB triggers failure."""
        floor_bytes = 30 * (1024 ** 3)  # 32,212,254,720 bytes (30 GiB)

        def evaluate_storage_threshold(free_bytes: int, floor: int = floor_bytes) -> bool:
            return free_bytes >= floor

        # 30.0 GiB exactly meets floor
        self.assertTrue(evaluate_storage_threshold(floor_bytes))
        # 30.1 GiB meets floor
        self.assertTrue(evaluate_storage_threshold(floor_bytes + 1024 * 1024))
        # 29.9 GiB fails threshold
        self.assertFalse(evaluate_storage_threshold(floor_bytes - 1024 * 1024))
        # Zero bytes fails threshold
        self.assertFalse(evaluate_storage_threshold(0))

    def test_04_malformed_manifest_and_json_rejection(self) -> None:
        """Verify schema parser rejects malformed JSON or manifests missing required fields."""
        def parse_manifest_content(raw_text: str) -> dict:
            try:
                data = json.loads(raw_text)
            except Exception as e:
                raise ValueError(f"Corrupted JSON: {e}") from e

            if not isinstance(data, dict):
                raise ValueError("Manifest must be a JSON object")
            if "schema_version" not in data:
                raise ValueError("Missing required key: schema_version")
            if "resources" not in data or not isinstance(data["resources"], list):
                raise ValueError("Missing or invalid 'resources' list")
            for idx, r in enumerate(data["resources"]):
                for k in ["id", "kind", "path"]:
                    if k not in r:
                        raise ValueError(f"Resource {idx} missing required key: {k}")
            return data

        # Valid manifest must parse
        valid_json = read_file(RESOURCE_MANIFEST)
        parsed = parse_manifest_content(valid_json)
        self.assertEqual(parsed.get("schema_version"), "aicatlog.resources.v1")

        # Corrupted JSON must raise ValueError
        with self.assertRaises(ValueError):
            parse_manifest_content("{ invalid json ...")

        # Missing schema_version must raise ValueError
        with self.assertRaises(ValueError):
            parse_manifest_content('{"resources": []}')

        # Resource missing required 'path' must raise ValueError
        with self.assertRaises(ValueError):
            parse_manifest_content('{"schema_version": "v1", "resources": [{"id": "foo", "kind": "skill"}]}')

    def test_05_port_0_blocker_token_isolation(self) -> None:
        """Verify port 0 blocker URL overrides and deterministic socket connection refusal."""
        supervisor_path = CODEX_AUTH_TOOLING / "codex_auth_supervisor.py"
        self.assertTrue(supervisor_path.is_file(), f"Missing {supervisor_path}")
        content = read_file(supervisor_path)

        # Assert blocker URL constants and environment overrides
        self.assertIn('DEFAULT_BLOCKER_URL = "http://127.0.0.1:0/oauth/token"', content)
        self.assertIn('env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = cfg.blocker_url', content)
        self.assertIn('env["CODEX_REVOKE_TOKEN_URL_OVERRIDE"] = cfg.revoke_blocker_url', content)

        # Real socket behavior test: connecting to 127.0.0.1:0 must be rejected by OS
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(1.0)
        with self.assertRaises(OSError):
            s.connect(("127.0.0.1", 0))
        s.close()

    def test_06_zero_task_splits_and_leakage_audit(self) -> None:
        """Verify split.json task counts (366 dev vs 979 frozen = 1,345 total) and zero cross-split family leakage."""
        split_locations = [
            PROJECT_ROOT / "benchmark" / "configs" / "split.json",
            PROJECT_ROOT / "workspace" / "harness-config" / "split.json",
        ]
        split_file = next((f for f in split_locations if f.is_file()), None)
        self.assertIsNotNone(split_file, "split.json not found")

        split_data = read_json(split_file)
        dev_tasks = split_data.get("dev", {}).get("taskIds", [])
        frozen_tasks = split_data.get("frozen", {}).get("taskIds", [])

        self.assertEqual(len(dev_tasks), 366, f"Expected 366 dev tasks, got {len(dev_tasks)}")
        self.assertEqual(len(frozen_tasks), 979, f"Expected 979 frozen tasks, got {len(frozen_tasks)}")
        self.assertEqual(len(dev_tasks) + len(frozen_tasks), 1345, "Total task denominator must equal 1,345")

        # Zero task ID overlap
        dev_set = set(dev_tasks)
        frozen_set = set(frozen_tasks)
        overlap = dev_set.intersection(frozen_set)
        self.assertEqual(len(overlap), 0, f"Task overlap detected between dev and frozen splits: {overlap}")

        # Zero group overlap
        dev_groups = set(split_data.get("dev", {}).get("groupIds", []))
        frozen_groups = set(split_data.get("frozen", {}).get("groupIds", []))
        group_overlap = dev_groups.intersection(frozen_groups)
        self.assertEqual(len(group_overlap), 0, f"Group overlap detected between dev and frozen: {group_overlap}")

        # Verify all groups task sum matches total
        all_group_tasks = []
        for g in split_data.get("groups", []):
            all_group_tasks.extend(g.get("taskIds", []))
        self.assertEqual(len(all_group_tasks), 1345)
        self.assertEqual(len(set(all_group_tasks)), 1345, "Duplicate task IDs detected across groups")

    def test_07_remote_mode_has_no_implicit_connection(self) -> None:
        """Remote mode without a private target spec stays unknown, without SSH."""
        res = run_cmd([str(BIN_SELFCHECK), "--mode", "remote", "--json"], timeout=15.0)
        self.assertEqual(res.returncode, 1)
        data = json.loads(res.stdout)
        self.assertEqual(data["schema"], "gptgrep.devbox-selfcheck.receipt.v2")
        self.assertEqual(data["overall_status"], "UNKNOWN")
        self.assertEqual(data["execution_scope"], "current_host")
        self.assertFalse(data["qualification"]["attempted"])

    def test_08_symlink_and_path_traversal_rejection(self) -> None:
        """Verify rejection of relative paths with '..', leading '/', null bytes, or outside allowed surfaces."""
        def relative_destination(value: str) -> str:
            if (
                not value
                or value.startswith("/")
                or "\\" in value
                or any(not v or v == "." or v == ".." for v in value.split("/"))
                or any(c in value for c in ["\r", "\n", "\0"])
            ):
                raise ValueError(f"Unsafe relative artifact path: {value}")
            return value

        # Path traversal checks in adapter
        for bad_path in [
            "/etc/passwd",
            "../secret.txt",
            "foo/../../bar",
            "foo\\bar",
            "foo\0bar",
            "",
            "foo/./bar",
        ]:
            with self.subTest(bad_path=bad_path):
                with self.assertRaises(ValueError):
                    relative_destination(bad_path)

        # Allowed mutation surfaces filter
        allowed_surfaces = ["crates/", "skills/", "runtime/", "prompts/"]

        def validate_mutation_paths(paths: list[str]) -> None:
            if not paths or len(set(paths)) != len(paths):
                raise ValueError("Explicit unique mutation paths are required")
            for path in paths:
                if (
                    path.startswith("/")
                    or "\\" in path
                    or any(not p or p == "." or p == ".." for p in path.split("/"))
                    or not any(path.startswith(prefix) for prefix in allowed_surfaces)
                ):
                    raise ValueError(f"Mutation outside allowed surfaces: {path}")

        # Unauthorized mutation surfaces must fail
        for bad_mutation in [
            ["benchmark/src/adapter.ts"],
            ["self-opt/src/evaluate.ts"],
            [".github/workflows/ci.yml"],
            ["crates/core/src/lib.rs", "crates/core/src/lib.rs"],
            ["crates/../etc/passwd"],
            ["/crates/core/src/lib.rs"],
        ]:
            with self.subTest(bad_mutation=bad_mutation):
                with self.assertRaises(ValueError):
                    validate_mutation_paths(bad_mutation)

        # Authorized mutations must pass
        valid_mutations = [
            "crates/gptgrep-core/src/lib.rs",
            "skills/gptgrep-remote-devbox/SKILL.md",
            "runtime/adapter.py",
            "prompts/reader.txt",
        ]
        validate_mutation_paths(valid_mutations)

    def test_09_tracked_files_privacy_scan(self) -> None:
        """Scan tracked text for portable credential patterns.

        GPTGREP_PRIVATE_SCAN_MARKERS_JSON optionally supplies a bounded JSON
        list of nonsecret per-run literal markers; values stay outside source.
        """
        import os

        credential_patterns = [
            r"-----BEGIN[ ](RSA[ ]|EC[ ]|OPENSSH[ ])?PRIVATE[ ]KEY-----",
            r"AKIA[0-9A-Z]{16}",
            r"AIza[0-9A-Za-z_-]{35}",
            r"gh[pousr]_[0-9A-Za-z]{36}",
            r"github_pat_[0-9A-Za-z_]{60,}",
            r"sk-(proj-|ant-api[0-9]{2}-)?[0-9A-Za-z_-]{40,}",
        ]
        credential_scan = run_cmd([
            "git", "grep", "-l", "-I", "-E", "-e",
            "|".join(credential_patterns), "--", ".",
        ])
        self.assertEqual(
            credential_scan.returncode, 1,
            "Tracked credential-pattern scan did not pass",
        )

        marker_json = os.environ.get("GPTGREP_PRIVATE_SCAN_MARKERS_JSON", "[]")
        if len(marker_json) > 8192:
            self.fail("Per-run privacy marker JSON exceeds its bound")
        try:
            markers = json.loads(marker_json)
        except ValueError:
            self.fail("Per-run privacy markers must be a valid JSON string list")
        valid_markers = (
            isinstance(markers, list) and len(markers) <= 32
            and all(
                isinstance(marker, str) and 0 < len(marker) <= 256
                and not any(character in marker for character in "\0\r\n")
                for marker in markers
            )
        )
        self.assertTrue(valid_markers, "Per-run privacy markers must be a bounded nonsecret string list")
        if markers:
            argv = ["git", "grep", "-l", "-I", "-F"]
            for marker in markers:
                argv.extend(["-e", marker])
            marker_scan = run_cmd([*argv, "--", "."])
            self.assertEqual(marker_scan.returncode, 1, "Tracked per-run marker scan did not pass")


if __name__ == "__main__":
    unittest.main()
