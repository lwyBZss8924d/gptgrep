#!/usr/bin/env python3
"""Unit tests for monitor_slot_release.py."""
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch, MagicMock

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import monitor_slot_release as monitor


class TestMonitorSlotRelease(unittest.TestCase):

    def test_check_stop_receipts_detects_receipt(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir)
            fake_receipt = tmp_path / "GPU-STOP-RECEIPT-shard-d.json"
            fake_receipt.write_text('{"status": "stopped"}')

            with patch.object(monitor, "HANDOFF_DIR", tmp_path), \
                 patch.object(monitor, "DRIVE_OCR_DIR", Path("/nonexistent")):
                res = monitor.check_stop_receipts()
                self.assertIsNotNone(res)
                self.assertIn("GPU-STOP-RECEIPT-shard-d.json", res)

    def test_check_stop_receipts_none_when_empty(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir)
            with patch.object(monitor, "HANDOFF_DIR", tmp_path), \
                 patch.object(monitor, "DRIVE_OCR_DIR", Path("/nonexistent")):
                res = monitor.check_stop_receipts()
                self.assertIsNone(res)

    def test_get_active_sessions_parses_output(self):
        fake_stdout = "[g4-shard-a] id1 | Hardware: G4\n[g4-shard-b] id2 | Hardware: G4\n[g4-shard-d] id3 | Hardware: G4\n"
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout=fake_stdout)
            sessions = monitor.get_active_sessions()
            self.assertEqual(sessions, ["g4-shard-a", "g4-shard-b", "g4-shard-d"])

    def test_get_active_sessions_detects_fewer_sessions(self):
        fake_stdout = "[g4-shard-a] id1 | Hardware: G4\n[g4-shard-b] id2 | Hardware: G4\n"
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout=fake_stdout)
            sessions = monitor.get_active_sessions()
            self.assertEqual(len(sessions), 2)
            self.assertNotIn("g4-shard-d", sessions)

    def test_get_active_sessions_returns_none_on_error(self):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=1, stderr="SSLError timeout")
            sessions = monitor.get_active_sessions()
            self.assertIsNone(sessions)


if __name__ == "__main__":
    unittest.main()
