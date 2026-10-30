"""Clean Shutdown and Storage Governance Test."""
import json
import os
from pathlib import Path
import shutil
import unittest

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])


def _resolve_volume_path() -> Path:
    env_vol = os.environ.get("GPTGREP_STORAGE_VOLUME")
    if env_vol:
        return Path(env_vol)
    if len(WORKTREE_ROOT.parts) >= 3 and WORKTREE_ROOT.parts[1] == "Volumes":
        return Path(*WORKTREE_ROOT.parts[:3])
    return WORKTREE_ROOT


VOLUME_PATH = _resolve_volume_path()
TMPDIR_PATH = VOLUME_PATH / "development-artifacts" / "t" / "s01"
STOP_RECEIPT_PATH = WORKTREE_ROOT / "workspace/handoff/google-agy-xl-docbench-ocr-20261018-r1/GPU-STOP-RECEIPT.json"
MIN_STORAGE_RESERVE_BYTES = 100 * 1024 * 1024 * 1024  # 100 GB absolute minimum threshold check


class TestCleanShutdown(unittest.TestCase):

    def test_gpu_stop_receipt_schema_and_verification(self):
        self.assertTrue(STOP_RECEIPT_PATH.is_file(), "GPU-STOP-RECEIPT.json must exist")
        data = json.loads(STOP_RECEIPT_PATH.read_text(encoding='utf-8'))
        self.assertEqual(data.get('schema'), 'gptgrep.private.user-handoff-gpu-stop.v1')
        self.assertTrue(data.get('stopped_verified'), "stopped_verified must be True")
        self.assertTrue(data.get('selected_endpoint_absent'), "selected_endpoint_absent must be True")

    def test_storage_reserve_governance(self):
        if VOLUME_PATH.exists():
            usage = shutil.disk_usage(str(VOLUME_PATH))
            # Verify volume has substantial headroom
            self.assertGreaterEqual(
                usage.free,
                MIN_STORAGE_RESERVE_BYTES,
                f"Storage reserve on {VOLUME_PATH} below threshold: {usage.free / 1e9:.2f} GB"
            )

    def test_tmpdir_path_governance(self):
        # Ensure TMPDIR does not leak uncleared huge artifacts
        if TMPDIR_PATH.exists():
            # If s01 exists, it should be a directory and under 10GB
            total_size = sum(f.stat().st_size for f in TMPDIR_PATH.rglob('*') if f.is_file())
            self.assertLessEqual(total_size, 10 * 1024 * 1024 * 1024, "TMPDIR accumulated excessive uncleaned data")


if __name__ == '__main__':
    unittest.main()
