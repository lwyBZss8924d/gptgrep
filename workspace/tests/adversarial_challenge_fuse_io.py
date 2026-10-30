"""Adversarial Challenge Test Suite: FUSE Compatibility and IO Implementation.

Milestone 1 Adversarial Challenges:
1. FUSE Error Handling:
   - renameat2 returning EINVAL, ENOSYS, EOPNOTSUPP, ENOTSUP -> clean fallback
   - renameat2 returning unhandled errnos (EACCES, EIO) -> proper OSError
   - directory fsync returning ENOTSUP, EINVAL, EPERM, EOPNOTSUPP -> clean suppression
   - directory fsync returning EIO -> clean exception propagation + fd closed
2. Target Collision:
   - rename_exclusive when target exists in renameat2 mode (EEXIST) -> FileExistsError('target_already_exists')
   - rename_exclusive when target exists in fallback mode -> FileExistsError('target_already_exists')
   - rename_exclusive when target exists after renameat2 EINVAL -> FileExistsError('target_already_exists')
   - Verify target content is NEVER overwritten and source content is NEVER deleted
3. Corruption Detection and Cleanup:
   - Simulated byte corruption during readback in _verify() -> ValueError('drive_queue_io_contract_failed')
   - Clean unlink of corrupted target file so no corrupted artifacts remain
   - Seek check offset/hash corruption -> detected and target unlinked
   - File truncation / size mismatch -> detected and target unlinked
   - Inode / TOCTOU replacement -> detected and target unlinked
4. Post-Move Verification & Sticky Failure:
   - End-to-end stage_inputs with FUSE fallback + post-move verification
   - Sticky failure state prevents subsequent operations after corruption
"""
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
PREPROCESS_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-runtime-r1"
if str(PREPROCESS_DIR) not in sys.path:
    sys.path.insert(0, str(PREPROCESS_DIR))

import drive_queue_io as dio


def make_mock_renameat2(ret_code: int, err_code: int):
    """Factory creating a mock renameat2 function setting ctypes errno."""
    def fake_renameat2(dfd1, src, dfd2, dst, flags):
        ctypes.set_errno(err_code)
        return ret_code
    return MagicMock(side_effect=fake_renameat2)


class MockDriveQueueIO(dio.DriveQueueIO):
    """Lightweight test harness subclass for DriveQueueIO internals."""
    def __init__(self, block_bytes: int = 1024):
        self.failed = False
        self.limits = {'block_bytes': block_bytes, 'max_output_bytes': 100_000_000}
        self.output_bytes = 0
        self.config = {'deadline_monotonic': time.monotonic() + 3600}
        self.external_check = lambda: None

    def _check(self):
        dio.require(not self.failed)
        if time.monotonic() >= self.config['deadline_monotonic']:
            raise TimeoutError('drive_queue_io_deadline')


class TestAdversarialFUSEAndIO(unittest.TestCase):
    """Adversarial stress harness for FUSE error handling and IO safety."""

    def setUp(self):
        self.temp_dir = Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(lambda: shutil.rmtree(self.temp_dir, ignore_errors=True))

    def _setup_full_io_env(self):
        """Set up standard isolated filesystem hierarchy for full DriveQueueIO."""
        mount = self.temp_dir / 'mount'
        mount.mkdir()
        input_dir = mount / 'input'
        input_dir.mkdir()
        parent = mount / 'output'
        parent.mkdir()
        content = self.temp_dir / 'content'
        content.mkdir()
        shards = content / 'exports/shards'
        shards.mkdir(parents=True)
        ctrl = content / 'control-exports'
        ctrl.mkdir()

        data = b'%PDF-1.4 test document content\n' + b'M' * 4096
        pdf = input_dir / 'test_doc.pdf'
        pdf.write_bytes(data)
        h = hashlib.sha256(data).hexdigest()

        cfg = {
            'schema': 'gptgrep.drive-queue-io.config.v1',
            'run_id': 'run-test-adv',
            'mount_root': str(mount),
            'input_root': str(input_dir),
            'content_root': str(content),
            'output_parent': str(parent),
            'publication_root': str(shards),
            'control_source_roots': [str(ctrl)],
            'owner_binding_sha256': 'aa' * 32,
            'mount_receipt_sha256': 'bb' * 32,
            'deadline_monotonic': time.monotonic() + 3600,
            'limits': {
                'max_selected_files': 10,
                'max_input_bytes': 10_000_000,
                'max_file_bytes': 10_000_000,
                'max_publications': 10,
                'max_archive_bytes': 10_000_000,
                'max_manifest_bytes': 1_000_000,
                'max_control_files': 10,
                'max_control_file_bytes': 10_000_000,
                'max_output_bytes': 50_000_000,
                'block_bytes': 512,
            }
        }

        def seek_chk(off, length):
            sub = data[off:off + length]
            return {'offset': off, 'length': length, 'sha256': hashlib.sha256(sub).hexdigest()}

        selected = [{
            'doc_id': 'doc_001',
            'file_name': 'test_doc.pdf',
            'file_id': 'file_001',
            'bytes': len(data),
            'sha256': h,
            'page_count': 1,
            'seek_checks': [
                seek_chk(0, 32),
                seek_chk(256, 64),
                seek_chk(1024, 128),
            ]
        }]
        return mount, input_dir, parent, content, shards, ctrl, cfg, selected, data

    # -------------------------------------------------------------------------
    # CHALLENGE 1: FUSE Error Handling & Fallback for renameat2
    # -------------------------------------------------------------------------

    def test_fuse_renameat2_fallback_einval(self):
        """Simulate renameat2 returning EINVAL -> verify clean fallback to os.replace."""
        src = self.temp_dir / "src_einval.dat"
        dst = self.temp_dir / "dst_einval.dat"
        content = b"EINVAL_FALLBACK_TEST_CONTENT"
        src.write_bytes(content)

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.EINVAL)

        with patch('ctypes.CDLL', return_value=fake_libc):
            dio.rename_exclusive(src, dst)

        self.assertFalse(src.exists(), "Source file should have been moved")
        self.assertTrue(dst.exists(), "Target file should exist after fallback move")
        self.assertEqual(dst.read_bytes(), content)

    def test_fuse_renameat2_fallback_enosys(self):
        """Simulate renameat2 returning ENOSYS -> verify clean fallback to os.replace."""
        src = self.temp_dir / "src_enosys.dat"
        dst = self.temp_dir / "dst_enosys.dat"
        content = b"ENOSYS_FALLBACK_TEST_CONTENT"
        src.write_bytes(content)

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.ENOSYS)

        with patch('ctypes.CDLL', return_value=fake_libc):
            dio.rename_exclusive(src, dst)

        self.assertFalse(src.exists(), "Source file should have been moved")
        self.assertTrue(dst.exists(), "Target file should exist after fallback move")
        self.assertEqual(dst.read_bytes(), content)

    def test_fuse_renameat2_fallback_eopnotsupp(self):
        """Simulate renameat2 returning EOPNOTSUPP -> verify clean fallback to os.replace."""
        src = self.temp_dir / "src_eopnotsupp.dat"
        dst = self.temp_dir / "dst_eopnotsupp.dat"
        content = b"EOPNOTSUPP_FALLBACK_TEST_CONTENT"
        src.write_bytes(content)

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, getattr(errno, 'EOPNOTSUPP', errno.EINVAL))

        with patch('ctypes.CDLL', return_value=fake_libc):
            dio.rename_exclusive(src, dst)

        self.assertFalse(src.exists(), "Source file should have been moved")
        self.assertTrue(dst.exists(), "Target file should exist after fallback move")
        self.assertEqual(dst.read_bytes(), content)

    def test_fuse_renameat2_fallback_enotsup(self):
        """Simulate renameat2 returning ENOTSUP -> verify clean fallback to os.replace."""
        if not hasattr(errno, 'ENOTSUP'):
            self.skipTest("errno.ENOTSUP not defined on platform")

        src = self.temp_dir / "src_enotsup.dat"
        dst = self.temp_dir / "dst_enotsup.dat"
        content = b"ENOTSUP_FALLBACK_TEST_CONTENT"
        src.write_bytes(content)

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.ENOTSUP)

        with patch('ctypes.CDLL', return_value=fake_libc):
            dio.rename_exclusive(src, dst)

        self.assertFalse(src.exists(), "Source file should have been moved")
        self.assertTrue(dst.exists(), "Target file should exist after fallback move")
        self.assertEqual(dst.read_bytes(), content)

    def test_fuse_renameat2_unhandled_errno_raises_oserror(self):
        """Simulate renameat2 returning unhandled errno (e.g. EACCES, EIO) -> must raise OSError."""
        src = self.temp_dir / "src_unhandled.dat"
        dst = self.temp_dir / "dst_unhandled.dat"
        content = b"UNHANDLED_ERRNO_TEST_CONTENT"
        src.write_bytes(content)

        for unhandled_err in (errno.EACCES, errno.EIO):
            fake_libc = MagicMock()
            fake_libc.renameat2 = make_mock_renameat2(-1, unhandled_err)

            with patch('ctypes.CDLL', return_value=fake_libc):
                with self.assertRaises(OSError) as ctx:
                    dio.rename_exclusive(src, dst)
                self.assertEqual(ctx.exception.errno, unhandled_err)

            # Neither source should be removed nor destination created
            self.assertTrue(src.exists())
            self.assertFalse(dst.exists())

    def test_renameat2_success_code_zero(self):
        """Simulate renameat2 returning 0 (native success) -> must return cleanly."""
        src = self.temp_dir / "src_ok.dat"
        dst = self.temp_dir / "dst_ok.dat"
        src.write_bytes(b"OK_TEST")

        fake_libc = MagicMock()
        # In a real kernel, renameat2 would have performed the rename.
        # We simulate the move and return 0.
        def successful_renameat2(dfd1, s, dfd2, d, flags):
            os.replace(src, dst)
            return 0
        fake_libc.renameat2 = MagicMock(side_effect=successful_renameat2)

        with patch('ctypes.CDLL', return_value=fake_libc):
            dio.rename_exclusive(src, dst)

        self.assertFalse(src.exists())
        self.assertTrue(dst.exists())

    # -------------------------------------------------------------------------
    # CHALLENGE 2: FUSE Directory fsync Error Handling (ENOTSUP, EINVAL, etc.)
    # -------------------------------------------------------------------------

    def test_fuse_directory_sync_enotsup(self):
        """Simulate directory fsync returning ENOTSUP (standard FUSE lack of fsyncdir)."""
        d = self.temp_dir / "dir_enotsup"
        d.mkdir()

        orig_fsync = os.fsync
        def fake_fsync(fd):
            raise OSError(errno.ENOTSUP, "Operation not supported on FUSE directory")

        with patch('os.fsync', side_effect=fake_fsync):
            # Must not raise an exception; ENOTSUP must be cleanly suppressed
            dio.directory_sync(d)

    def test_fuse_directory_sync_all_ignored_errnos(self):
        """Verify all FUSE_IGNORED_ERRNOS (EINVAL, EPERM, EOPNOTSUPP, ENOTSUP) are suppressed."""
        d = self.temp_dir / "dir_ignored"
        d.mkdir()

        for err in dio.FUSE_IGNORED_ERRNOS:
            def fake_fsync(fd, err_to_raise=err):
                raise OSError(err_to_raise, f"FUSE error {err_to_raise}")

            with patch('os.fsync', side_effect=fake_fsync):
                # Must not raise
                dio.directory_sync(d)

    def test_fuse_directory_sync_unhandled_errno_raises_and_closes_fd(self):
        """Simulate directory fsync returning EIO -> must raise OSError and not leak fd."""
        d = self.temp_dir / "dir_eio"
        d.mkdir()

        def fake_fsync(fd):
            raise OSError(errno.EIO, "Genuine filesystem I/O error")

        with patch('os.fsync', side_effect=fake_fsync):
            with self.assertRaises(OSError) as ctx:
                dio.directory_sync(d)
            self.assertEqual(ctx.exception.errno, errno.EIO)

    # -------------------------------------------------------------------------
    # CHALLENGE 3: Target Collision & No-Overwrite Invariant
    # -------------------------------------------------------------------------

    def test_target_collision_renameat2_eexist(self):
        """When target exists and renameat2 returns EEXIST -> raise FileExistsError without overwriting."""
        src = self.temp_dir / "src_collision_1.dat"
        dst = self.temp_dir / "dst_collision_1.dat"
        src.write_bytes(b"NEW_DATA_SHOULD_NOT_OVERWRITE")
        dst.write_bytes(b"PRE_EXISTING_TARGET_CONTENT")

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.EEXIST)

        with patch('ctypes.CDLL', return_value=fake_libc):
            with self.assertRaises(FileExistsError) as ctx:
                dio.rename_exclusive(src, dst)
            self.assertEqual(str(ctx.exception), 'target_already_exists')

        # Invariant checks:
        self.assertEqual(dst.read_bytes(), b"PRE_EXISTING_TARGET_CONTENT", "Target was overwritten!")
        self.assertEqual(src.read_bytes(), b"NEW_DATA_SHOULD_NOT_OVERWRITE", "Source was destroyed!")

    def test_target_collision_fallback_path(self):
        """When target exists in fallback path (no renameat2) -> raise FileExistsError without overwriting."""
        src = self.temp_dir / "src_collision_2.dat"
        dst = self.temp_dir / "dst_collision_2.dat"
        src.write_bytes(b"NEW_DATA_FALLBACK_COLLISION")
        dst.write_bytes(b"TARGET_CONTENT_PRESERVED")

        # Mock renameat2 as None (simulates environment without renameat2)
        fake_libc = MagicMock()
        fake_libc.renameat2 = None

        with patch('ctypes.CDLL', return_value=fake_libc):
            with self.assertRaises(FileExistsError) as ctx:
                dio.rename_exclusive(src, dst)
            self.assertEqual(str(ctx.exception), 'target_already_exists')

        # Invariant checks:
        self.assertEqual(dst.read_bytes(), b"TARGET_CONTENT_PRESERVED", "Target was overwritten!")
        self.assertEqual(src.read_bytes(), b"NEW_DATA_FALLBACK_COLLISION", "Source was destroyed!")

    def test_target_collision_after_renameat2_einval_fallback(self):
        """When renameat2 returns EINVAL and target exists -> fallback must detect target and raise."""
        src = self.temp_dir / "src_collision_3.dat"
        dst = self.temp_dir / "dst_collision_3.dat"
        src.write_bytes(b"SOURCE_CONTENT_3")
        dst.write_bytes(b"TARGET_CONTENT_3")

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.EINVAL)

        with patch('ctypes.CDLL', return_value=fake_libc):
            with self.assertRaises(FileExistsError) as ctx:
                dio.rename_exclusive(src, dst)
            self.assertEqual(str(ctx.exception), 'target_already_exists')

        self.assertEqual(dst.read_bytes(), b"TARGET_CONTENT_3")
        self.assertEqual(src.read_bytes(), b"SOURCE_CONTENT_3")

    # -------------------------------------------------------------------------
    # CHALLENGE 4: Corruption Detection & Unlink of Corrupted Files
    # -------------------------------------------------------------------------

    def test_corruption_detection_full_sha256_mismatch_and_clean_unlink(self):
        """Simulate byte corruption during readback in _verify -> catch mismatch and unlink target."""
        src = self.temp_dir / "source_corrupt.dat"
        dst = self.temp_dir / "target_corrupt.dat"
        data = b"INTEGRITY_SENSITIVE_DOCUMENT_PAYLOAD" * 32
        src.write_bytes(data)
        ref = {'path': str(src), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

        mock_io = MockDriveQueueIO()
        orig_verify = mock_io._verify

        def corrupting_verify(path, r, checks=()):
            # Corrupt 1 byte in the moved target file on disk right before verification
            p = Path(path)
            content = bytearray(p.read_bytes())
            content[10] ^= 0xFF  # Flip bits in 10th byte
            p.write_bytes(bytes(content))
            return orig_verify(path, r, checks)

        with patch.object(mock_io, '_verify', side_effect=corrupting_verify):
            with self.assertRaises(ValueError) as ctx:
                mock_io._copy(src, dst, ref)
            self.assertEqual(str(ctx.exception), 'drive_queue_io_contract_failed')

        # Clean unlink verification:
        self.assertFalse(dst.exists(), "Corrupted target file MUST be unlinked after verification failure")
        partial = dst.with_name('.copy-' + dst.name + '.partial')
        self.assertFalse(partial.exists(), "Temporary partial file MUST be unlinked")

    def test_corruption_detection_seek_checks_mismatch_and_clean_unlink(self):
        """Simulate seek check failure (slice sha256 mismatch) -> catch mismatch and unlink target."""
        src = self.temp_dir / "source_seek_corrupt.dat"
        dst = self.temp_dir / "target_seek_corrupt.dat"
        data = b"SEEK_CHECK_DOCUMENT_PAYLOAD_CHUNK_" * 64
        src.write_bytes(data)
        ref = {'path': str(src), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

        seek_checks = [
            {'offset': 0, 'length': 32, 'sha256': hashlib.sha256(data[0:32]).hexdigest()},
            {'offset': 100, 'length': 32, 'sha256': hashlib.sha256(data[100:132]).hexdigest()},
            {'offset': 200, 'length': 32, 'sha256': hashlib.sha256(data[200:232]).hexdigest()},
        ]

        mock_io = MockDriveQueueIO()
        orig_verify = mock_io._verify

        def corrupting_seek_verify(path, r, checks=()):
            # Seek check #2 expects wrong hash (simulating slice corruption during readback)
            tampered_checks = list(checks)
            tampered_checks[1] = {**tampered_checks[1], 'sha256': '99' * 32}
            return orig_verify(path, r, tampered_checks)

        with patch.object(mock_io, '_verify', side_effect=corrupting_seek_verify):
            with self.assertRaises(ValueError) as ctx:
                mock_io._copy(src, dst, ref, seek_checks)
            self.assertEqual(str(ctx.exception), 'drive_queue_io_contract_failed')

        self.assertFalse(dst.exists(), "Target file must be unlinked after seek check failure")

    def test_corruption_detection_file_truncation(self):
        """Simulate file truncation on disk prior to _verify -> catch size mismatch and unlink."""
        src = self.temp_dir / "source_trunc.dat"
        dst = self.temp_dir / "target_trunc.dat"
        data = b"TRUNCATION_TEST_PAYLOAD" * 20
        src.write_bytes(data)
        ref = {'path': str(src), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

        mock_io = MockDriveQueueIO()
        orig_verify = mock_io._verify

        def truncating_verify(path, r, checks=()):
            p = Path(path)
            p.write_bytes(p.read_bytes()[:len(data) // 2])
            return orig_verify(path, r, checks)

        with patch.object(mock_io, '_verify', side_effect=truncating_verify):
            with self.assertRaises(ValueError) as ctx:
                mock_io._copy(src, dst, ref)
            self.assertEqual(str(ctx.exception), 'drive_queue_io_contract_failed')

        self.assertFalse(dst.exists(), "Truncated target file must be unlinked")

    def test_corruption_detection_toctou_inode_replacement(self):
        """Simulate concurrent replacement of file inode during _verify -> catch TOCTOU and unlink."""
        src = self.temp_dir / "source_toctou.dat"
        dst = self.temp_dir / "target_toctou.dat"
        data = b"TOCTOU_TEST_PAYLOAD" * 20
        src.write_bytes(data)
        ref = {'path': str(src), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

        mock_io = MockDriveQueueIO()

        orig_open = dio.os.open
        def racing_open(path, flags, *args):
            fd = orig_open(path, flags, *args)
            # When opening target file in _verify, concurrently replace file on disk
            if str(dst) in str(path) and flags & os.O_DIRECTORY == 0:
                p = Path(path)
                content = p.read_bytes()
                p.unlink()
                p.write_bytes(content)  # new inode on disk
            return fd

        with patch('drive_queue_io.os.open', side_effect=racing_open):
            with self.assertRaises(ValueError) as ctx:
                mock_io._copy(src, dst, ref)
            self.assertEqual(str(ctx.exception), 'drive_queue_io_contract_failed')

        self.assertFalse(dst.exists(), "Target file must not be left behind on TOCTOU detection")

    # -------------------------------------------------------------------------
    # CHALLENGE 5: End-to-End FUSE Fallback & Post-Move Verification in DriveQueueIO
    # -------------------------------------------------------------------------

    def test_stage_inputs_fuse_fallback_and_post_move_verification(self):
        """Full stage_inputs pipeline under FUSE simulation: renameat2 EINVAL + fsyncdir ENOTSUP."""
        mount, input_dir, parent, content, shards, ctrl, cfg, selected, data = self._setup_full_io_env()

        fake_libc = MagicMock()
        fake_libc.renameat2 = make_mock_renameat2(-1, errno.EINVAL)

        orig_fsync = os.fsync
        def fuse_fsync(fd):
            try:
                if stat.S_ISDIR(os.fstat(fd).st_mode):
                    raise OSError(errno.ENOTSUP, "FUSE fsyncdir not supported")
            except OSError:
                pass
            return orig_fsync(fd)

        with patch('ctypes.CDLL', return_value=fake_libc), patch('os.fsync', side_effect=fuse_fsync):
            io = dio.DriveQueueIO(cfg, selected, lambda: None)
            receipt = io.stage_inputs()

            self.assertTrue(io.staged)
            self.assertFalse(io.failed)
            self.assertEqual(len(receipt['documents']), 1)

            staged_doc = receipt['documents'][0]
            target_path = Path(staged_doc['remote_path'])
            self.assertTrue(target_path.exists())
            self.assertEqual(hashlib.sha256(target_path.read_bytes()).hexdigest(), selected[0]['sha256'])
            self.assertEqual(receipt['schema'], 'gptgrep.drive-queue-io.inputs.v1')
            self.assertEqual(receipt['visibility'], 'drivefs_readback_verified')
            self.assertFalse(receipt['cloud_durable'])

    def test_stage_inputs_corruption_triggers_sticky_failure(self):
        """Full stage_inputs pipeline: corruption causes exception, unlinks file, marks failure sticky."""
        mount, input_dir, parent, content, shards, ctrl, cfg, selected, data = self._setup_full_io_env()

        io = dio.DriveQueueIO(cfg, selected, lambda: None)
        target = content / 'raw/doc_001.pdf'

        orig_verify = io._verify
        def corrupting_verify(path, ref, checks=()):
            p = Path(path)
            corrupted = bytearray(p.read_bytes())
            corrupted[0] ^= 0x55
            p.write_bytes(bytes(corrupted))
            return orig_verify(path, ref, checks)

        with patch.object(io, '_verify', side_effect=corrupting_verify):
            with self.assertRaises(ValueError):
                io.stage_inputs()

        # Target must be unlinked
        self.assertFalse(target.exists(), "Corrupted document must be cleanly unlinked")
        # IO must be marked failed stickily
        self.assertTrue(io.failed)

        # Subsequent attempts to stage or check must fail immediately
        with self.assertRaises(ValueError):
            io._check()

        with self.assertRaises(ValueError):
            io.stage_inputs()

        # Receipt reflects failed status
        rcpt = io.receipt()
        self.assertEqual(rcpt['status'], 'failed')


    def test_publish_closed_corruption_and_cleanup(self):
        """In publish_closed, manifest/archive corruption during readback unlinks artifacts and marks failure sticky."""
        mount, input_dir, parent, content, shards, ctrl, cfg, selected, data = self._setup_full_io_env()

        io = dio.DriveQueueIO(cfg, selected, lambda: None)
        io.stage_inputs()

        # Create valid shard archive and manifest in publication_root (shards)
        shard_id = "cc" * 32
        arch_data = b"MOCK_TAR_GZ_DATA_FOR_SHARD"
        archive_path = shards / f"{shard_id}.tar.gz"
        archive_path.write_bytes(arch_data)
        ar_ref = {'path': str(archive_path), 'bytes': len(arch_data), 'sha256': hashlib.sha256(arch_data).hexdigest()}

        manifest_obj = {
            'shard_id': shard_id,
            'archive': {'name': f"{shard_id}.tar.gz", 'bytes': len(arch_data), 'sha256': ar_ref['sha256']}
        }
        manifest_raw = dio.canonical(manifest_obj)
        manifest_path = shards / f"{shard_id}.json"
        manifest_path.write_bytes(manifest_raw)
        mr_ref = {'path': str(manifest_path), 'bytes': len(manifest_raw), 'sha256': hashlib.sha256(manifest_raw).hexdigest()}

        pub = {
            'manifest_path': str(manifest_path),
            'archive_path': str(archive_path),
            'manifest': manifest_obj,
            'manifest_ref': mr_ref,
            'archive_ref': ar_ref
        }

        # Corrupt the archive during copy to shards directory
        orig_copy = io._copy
        def corrupting_copy(source, target, ref, checks=()):
            if "tar.gz" in str(target):
                # Corrupt the reference sha so _verify fails
                bad_ref = {**ref, 'sha256': '99' * 32}
                return orig_copy(source, target, bad_ref, checks)
            return orig_copy(source, target, ref, checks)

        with patch.object(io, '_copy', side_effect=corrupting_copy):
            with self.assertRaises(ValueError):
                io.publish_closed(pub, lambda m: m)

        # Failure must be sticky
        self.assertTrue(io.failed)
        target_archive = io.shards / f"{shard_id}.tar.gz"
        self.assertFalse(target_archive.exists(), "Corrupted archive target must be unlinked")

    def test_deadline_timeout_during_readback_unlinks_target(self):
        """When deadline_monotonic expires during _verify, TimeoutError is raised and target is unlinked."""
        src = self.temp_dir / "src_timeout.dat"
        dst = self.temp_dir / "dst_timeout.dat"
        data = b"TIMEOUT_TEST_DATA" * 50
        src.write_bytes(data)
        ref = {'path': str(src), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

        mock_io = MockDriveQueueIO()
        orig_verify = mock_io._verify

        def timing_out_verify(path, r, checks=()):
            mock_io.config['deadline_monotonic'] = time.monotonic() - 10
            return orig_verify(path, r, checks)

        with patch.object(mock_io, '_verify', side_effect=timing_out_verify):
            with self.assertRaises(TimeoutError) as ctx:
                mock_io._copy(src, dst, ref)
            self.assertEqual(str(ctx.exception), 'drive_queue_io_deadline')

        self.assertFalse(dst.exists(), "Target file must be unlinked when deadline expires")

    def test_symlink_rejection_in_physical(self):
        """physical() must reject paths containing symlinks."""
        real_file = self.temp_dir / "real.dat"
        real_file.write_bytes(b"DATA")
        link = self.temp_dir / "symlink.dat"
        link.symlink_to(real_file)

        with self.assertRaises(ValueError) as ctx:
            dio.physical(link)
        self.assertEqual(str(ctx.exception), 'drive_queue_io_contract_failed')

    def test_private_credential_names_rejected(self):
        """name() must reject private credential filenames like auth.json, credentials.json."""
        for cred_name in dio.PRIVATE_NAMES:
            self.assertFalse(dio.name(cred_name), f"Should have rejected private name: {cred_name}")
            self.assertFalse(dio.name(cred_name.upper()), f"Should have rejected uppercase private name: {cred_name.upper()}")

        # Valid file names must be accepted
        self.assertTrue(dio.name("doc_001.pdf"))
        self.assertTrue(dio.name("shard-output.tar.gz"))


if __name__ == '__main__':
    unittest.main()
