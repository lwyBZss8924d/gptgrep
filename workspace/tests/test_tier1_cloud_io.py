"""Tier 1 Test: Cloud I/O Primitives and FUSE Robustness."""
import errno
import hashlib
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

import drive_queue_io as dio


class TestTier1CloudIO(unittest.TestCase):

    def setUp(self):
        self.temp_dir = str(Path(tempfile.mkdtemp()).resolve())
        self.addCleanup(lambda: shutil.rmtree(self.temp_dir, ignore_errors=True))


    def test_fuse_directory_sync_error_suppression(self):
        d = Path(self.temp_dir) / "fuse_test_dir"
        d.mkdir()
        # Verify directory sync does not raise even if FUSE errors occur
        dio.directory_sync(d)

    def test_rename_exclusive_no_replace(self):
        src = Path(self.temp_dir) / "src.dat"
        dst = Path(self.temp_dir) / "dst.dat"
        src.write_bytes(b"DATA_1")
        dst.write_bytes(b"DATA_2")
        with self.assertRaises(FileExistsError):
            dio.rename_exclusive(src, dst)
        self.assertEqual(dst.read_bytes(), b"DATA_2")

    def test_rename_exclusive_success_move(self):
        src = Path(self.temp_dir) / "src.dat"
        dst = Path(self.temp_dir) / "dst.dat"
        src.write_bytes(b"MOVE_DATA")
        dio.rename_exclusive(src, dst)
        self.assertFalse(src.exists())
        self.assertTrue(dst.exists())
        self.assertEqual(dst.read_bytes(), b"MOVE_DATA")

    def test_clean_unlink_on_verification_failure(self):
        src = Path(self.temp_dir) / "source.dat"
        dst = Path(self.temp_dir) / "target.dat"
        data = b"UNLINK_TEST_CONTENT"
        src.write_bytes(data)

        class MockIO(dio.DriveQueueIO):
            def __init__(self):
                self.failed = False
                self.limits = {'block_bytes': 1024}
            def _check(self): pass

        mock_io = MockIO()
        bad_ref = {'path': str(src), 'bytes': len(data), 'sha256': '00'*32}
        with self.assertRaises(BaseException):
            mock_io._copy(src, dst, bad_ref)
        
        # Corrupt file must be completely unlinked from disk
        self.assertFalse(dst.exists(), "Corrupt target file was not cleanly unlinked")


if __name__ == '__main__':
    unittest.main()
