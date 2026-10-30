"""Tier 2 Test: Pre-model Drive Runtime Probe."""
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
PREPROCESS_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-runtime-r1"
if str(PREPROCESS_DIR) not in sys.path:
    sys.path.insert(0, str(PREPROCESS_DIR))

import drive_probe as probe


class TestTier2PreModelProbe(unittest.TestCase):

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: shutil.rmtree(self.temp_dir, ignore_errors=True))

    def test_probe_receipt_schema_and_capabilities(self):
        target = Path(self.temp_dir) / "probe_target"
        target.mkdir()

        receipt = probe.probe_drive_runtime(target, min_free_bytes=1024)
        self.assertEqual(receipt['schema'], 'gptgrep.drive.runtime-probe-receipt.v1')
        self.assertEqual(receipt['target_path'], str(target.resolve()))
        
        caps = receipt['capabilities']
        self.assertTrue(caps['accessibility'], "accessibility must pass")
        self.assertTrue(caps['free_space'], "free space must pass")
        self.assertTrue(caps['write_readback_sha256'], "write/readback SHA256 must pass")
        self.assertTrue(caps['atomic_replace'], "atomic replace must pass")
        self.assertTrue(caps['non_blocking_lock_safety'], "non-blocking lock safety must pass")
        self.assertTrue(receipt['passed'], "overall probe must pass")

    def test_probe_fails_early_when_directory_unwritable(self):
        nonexistent = Path("/nonexistent_root_dir_for_test_fail_early/sub")
        receipt = probe.probe_drive_runtime(nonexistent)
        self.assertFalse(receipt['passed'])
        self.assertFalse(receipt['capabilities']['accessibility'])


if __name__ == '__main__':
    unittest.main()
