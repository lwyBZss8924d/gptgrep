#!/usr/bin/env python3
"""Unit tests for launch_shard_e.py."""
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch, MagicMock

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import launch_shard_e as launcher


class TestLaunchShardE(unittest.TestCase):

    def test_run_cmd_success(self):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout="success\n", stderr="")
            res = launcher.run_cmd(["echo", "hello"])
            self.assertEqual(res.returncode, 0)

    def test_allocate_session(self):
        with patch("launch_shard_e.run_cmd") as mock_run_cmd:
            mock_run_cmd.return_value = MagicMock(returncode=0, stdout="ok", stderr="")
            res = launcher.allocate_session()
            self.assertTrue(res)

    def test_upload_payloads_verifies_files_exist(self):
        self.assertTrue(launcher.SHARD_E_MANIFEST.exists(), f"Missing {launcher.SHARD_E_MANIFEST}")
        self.assertTrue(launcher.EXPORTER_SCRIPT.exists(), f"Missing {launcher.EXPORTER_SCRIPT}")
        self.assertTrue(launcher.RUNNER_SCRIPT.exists(), f"Missing {launcher.RUNNER_SCRIPT}")


if __name__ == "__main__":
    unittest.main()
