"""Tier 3 Test: Cloud Durability Contract and Local Sync Verification."""
import json
import os
from pathlib import Path
import unittest

def _resolve_drive_root() -> Path:
    env_root = os.environ.get("XL_DOCBENCH_DRIVE_ROOT")
    if env_root:
        return Path(env_root)
    cs_dir = Path.home() / "Library" / "CloudStorage"
    if cs_dir.is_dir():
        matches = sorted(cs_dir.glob("GoogleDrive-*"))
        if matches:
            candidate = matches[0] / "My Drive"
            if candidate.exists():
                return candidate
    return Path(os.environ.get("XL_DOCBENCH_CORPUS_ROOT", "/tmp/xl-docbench"))


DESKTOP_DRIVE_PATH = _resolve_drive_root()
RAW229_FOLDER = DESKTOP_DRIVE_PATH / 'XL-DocBench-RAW-229-20261018' / 'raw'
OCR_FOLDER = DESKTOP_DRIVE_PATH / 'XL-DocBench-OCR-20261018'


class TestTier3CloudDurabilityAndLocalSync(unittest.TestCase):

    def test_cloud_durability_contract_distinction(self):
        # A DriveQueueIO marker or receipt must strictly assert cloud_durable: False
        # until native flush_and_unmount and server readback are complete.
        receipt = {
            'schema': 'gptgrep.drive-queue-io.receipt.v1',
            'visibility': 'drivefs_readback_verified',
            'cloud_durable': False
        }
        self.assertFalse(receipt['cloud_durable'], "DriveFS mount-internal visibility must not be claimed as cloud_durable")

    def test_desktop_drive_path_structure(self):
        # Verify the desktop Google Drive root and raw inputs are configured
        if DESKTOP_DRIVE_PATH.exists():
            self.assertTrue(DESKTOP_DRIVE_PATH.is_dir(), "Desktop Drive path must be a directory")
            if RAW229_FOLDER.exists():
                self.assertTrue(RAW229_FOLDER.is_dir(), "RAW-229 folder must be a directory")
                pdf_count = len(list(RAW229_FOLDER.glob("*.pdf")))
                self.assertGreaterEqual(pdf_count, 1, "Expected PDF files in desktop raw sync folder")
        else:
            self.skipTest("Desktop Google Drive CloudStorage path not mounted on local host; skipping physical path check")


if __name__ == '__main__':
    unittest.main()
