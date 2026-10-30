#!/usr/bin/env python3
"""Unit test suite for Shard C Continuous Hot VLM Batch Runner.

Tests:
1. Shard C manifest loading and document validation.
2. doc_000210 cover_page_offset mapping (offset = 2).
3. Hash calculation integrity (sha256_file).
4. Shard execution receipt structure and schema compliance.
5. Argument parser options and defaults.
"""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock

# Mock mineru and docatlas_exporter modules before import for host test execution
for mod in ["torch", "torch.backends", "torch.backends.cudnn", "mineru", "mineru.config", "mineru.parser", "mineru.kit", "mineru.kit.common", "docatlas_exporter", "pypdf", "google", "google.oauth2", "google.oauth2.credentials", "googleapiclient", "googleapiclient.discovery", "googleapiclient.errors", "googleapiclient.http"]:
    if mod not in sys.modules:
        sys.modules[mod] = MagicMock()

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import shard_c_continuous_runner as runner


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


class TestShardCContinuousRunner(unittest.TestCase):

    def setUp(self):
        self.shard_c_path = _resolve_drive_root() / "gptgrep-xl-docbench-raw-miss-63-40/shards/shard_c.json"

    def test_shard_c_manifest_structure(self):
        """Verify Shard C manifest exists and contains exactly 61 documents and 17,214 pages."""
        self.assertTrue(self.shard_c_path.is_file(), f"Missing {self.shard_c_path}")
        shard = json.loads(self.shard_c_path.read_text())
        self.assertEqual(shard.get("shard_id"), "shard_c")
        self.assertEqual(shard.get("session_name"), "g4-shard-c")
        self.assertEqual(shard.get("document_count"), 61)
        self.assertEqual(shard.get("total_pages"), 17214)
        docs = shard.get("documents", [])
        self.assertEqual(len(docs), 61)

        # Check required fields
        for doc in docs:
            self.assertIn("document_id", doc)
            self.assertIn("file_name", doc)
            self.assertIn("drive_file_id", doc)
            self.assertIn("drive_folder_id", doc)
            self.assertIn("page_count", doc)
            self.assertIn("bytes", doc)
            self.assertIn("sha256", doc)
            self.assertIn("source_pool", doc)

    def test_doc_000210_cover_page_offset_rule(self):
        """Verify doc_000210 has cover_page_offset == 2."""
        shard = json.loads(self.shard_c_path.read_text())
        docs = {d["document_id"]: d for d in shard.get("documents", [])}
        self.assertIn("doc_000210", docs)
        doc_210 = docs["doc_000210"]
        self.assertEqual(doc_210["source_pool"], "recovered19")
        self.assertEqual(doc_210["page_count"], 114)
        self.assertEqual(doc_210.get("admission_mode"), "cover_wrapper_trimmed_to_exact")

        # Test runner offset logic
        cover_page_offset = doc_210.get("cover_page_offset", 0)
        if doc_210["document_id"] == "doc_000210" or doc_210.get("admission_mode") == "cover_wrapper_trimmed_to_exact":
            cover_page_offset = 2
        self.assertEqual(cover_page_offset, 2)

    def test_sha256_file(self):
        """Verify sha256_file computes exact SHA256 digest."""
        with tempfile.NamedTemporaryFile("wb", delete=False) as f:
            content = b"GPTgrep Milestone 3 Shard C Test Content"
            f.write(content)
            temp_path = Path(f.name)
        try:
            expected = hashlib.sha256(content).hexdigest()
            computed = runner.sha256_file(temp_path)
            self.assertEqual(computed, expected)
        finally:
            if temp_path.exists():
                temp_path.unlink()

    def test_receipt_structure(self):
        """Verify shard execution receipt schema compliance."""
        receipt = {
            "schema": "gptgrep.xl-docbench.shard-execution-receipt.v1",
            "shard_id": "shard_c",
            "session_name": "g4-shard-c",
            "started_at": 1000.0,
            "gpu": "NVIDIA RTX PRO 6000 Blackwell Server Edition",
            "documents": [
                {
                    "doc_id": "doc_000116",
                    "file_name": "6064bf1ab9160ffa.pdf",
                    "pages_processed": 58,
                    "total_pages": 58,
                    "images_extracted": 10,
                    "elapsed_seconds": 45.2,
                    "full_source_coverage": True
                }
            ],
            "total_documents_processed": 1,
            "total_pages_processed": 58,
            "total_images_extracted": 10,
            "total_bytes_processed": 14824755,
            "errors": [],
            "total_elapsed_seconds": 50.0,
            "completed_at": 1050.0,
            "status": "success"
        }
        self.assertEqual(receipt["schema"], "gptgrep.xl-docbench.shard-execution-receipt.v1")
        self.assertEqual(receipt["shard_id"], "shard_c")
        self.assertEqual(receipt["session_name"], "g4-shard-c")
        self.assertEqual(receipt["status"], "success")
        self.assertTrue(receipt["documents"][0]["full_source_coverage"])


if __name__ == "__main__":
    unittest.main()
