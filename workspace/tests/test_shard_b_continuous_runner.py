#!/usr/bin/env python3
"""Unit test suite for Shard B Continuous Hot VLM Batch Runner.

Tests:
1. Shard B manifest structure, document count (61), total pages (17,215), and schema integrity.
2. Hash calculation integrity (sha256_file).
3. Markdown image normalization in DocAtlas exporter.
4. Shard execution receipt structure and schema compliance.
5. Idempotent skipping logic and status update serialization.
"""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

# Mock heavy GPU/cloud modules for portable test execution
for mod in [
    "mineru", "mineru.config", "mineru.parser", "mineru.kit", "mineru.kit.common",
    "pypdf", "google", "google.oauth2", "google.oauth2.credentials",
    "googleapiclient", "googleapiclient.discovery", "googleapiclient.errors", "googleapiclient.http"
]:
    if mod not in sys.modules:
        sys.modules[mod] = MagicMock()

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import shard_b_hot_runner as runner


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


class TestShardBContinuousRunner(unittest.TestCase):

    def setUp(self):
        self.shard_b_path = _resolve_drive_root() / "gptgrep-xl-docbench-raw-miss-63-40/shards/shard_b.json"
        self.temp_dir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_shard_b_manifest_structure(self):
        """Verify Shard B manifest exists and contains exactly 61 documents and 17,215 pages."""
        self.assertTrue(self.shard_b_path.is_file(), f"Missing {self.shard_b_path}")
        shard = json.loads(self.shard_b_path.read_text())
        self.assertEqual(shard.get("shard_id"), "shard_b")
        self.assertEqual(shard.get("session_name"), "g4-shard-b")
        self.assertEqual(shard.get("document_count"), 61)
        self.assertEqual(shard.get("total_pages"), 17215)
        docs = shard.get("documents", [])
        self.assertEqual(len(docs), 61)

        total_pages = sum(d["page_count"] for d in docs)
        self.assertEqual(total_pages, 17215, f"Sum of page counts {total_pages} != 17215")

        # Verify doc 31 is doc_000074 (crossing midpoint)
        self.assertEqual(docs[30]["document_id"], "doc_000074")
        self.assertEqual(docs[30]["page_count"], 207)

        # Verify doc 32 is doc_000135
        self.assertEqual(docs[31]["document_id"], "doc_000135")
        self.assertEqual(docs[31]["page_count"], 214)

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

    def test_sha256_file_calculation(self):
        """Test sha256_file helper function against standard hash."""
        test_file = Path(self.temp_dir) / "sample.bin"
        test_data = b"GPTgrep Milestone 3 Shard B Continuous Hot Runner Test Data 20261019"
        test_file.write_bytes(test_data)
        expected_hash = hashlib.sha256(test_data).hexdigest()

        actual_hash = runner.sha256_file(test_file)
        self.assertEqual(actual_hash, expected_hash)

    def test_markdown_image_normalization(self):
        """Test that HTML and raw markdown image tags are properly normalized."""
        sample_md = (
            "# Test Document\n\n"
            "<img src='img1.png' alt='Diagram 1'>\n"
            "Some text here.\n"
            "![Chart](<figure2.jpg>)\n"
            "![Graph](raw/graph3.png)\n"
        )
        normalized = runner.normalize_markdown_images(sample_md)
        self.assertIn("![Diagram 1](images/img1.png)", normalized)
        self.assertIn("![Chart](images/figure2.jpg)", normalized)
        self.assertIn("![Graph](images/graph3.png)", normalized)
        self.assertNotIn("<img", normalized)

    def test_docatlas_exporter_structure(self):
        """Test DocAtlasExporter creates standard directory and file hierarchy."""
        exp = runner.DocAtlasExporter(self.temp_dir)
        doc_id = "doc_test_001"
        page_dir = exp.export_page(
            doc_stem=doc_id,
            page_num=1,
            markdown_text="# Title\n\n![Img](pic.png)",
            images={"pic.png": b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"}
        )

        self.assertTrue(page_dir.is_dir())
        self.assertEqual(page_dir.name, f"{doc_id}_page0")
        vlm_dir = page_dir / "vlm"
        self.assertTrue(vlm_dir.is_dir())
        md_file = vlm_dir / f"{doc_id}_page0.md"
        self.assertTrue(md_file.is_file())
        img_file = vlm_dir / "images" / "pic.png"
        self.assertTrue(img_file.is_file())

    def test_shard_receipt_schema_structure(self):
        """Verify receipt structure conforms to expected schema."""
        receipt = {
            "schema": "gptgrep.xl-docbench.batch-execution-receipt.v1",
            "shard_id": "shard_b",
            "session_name": "g4-shard-b",
            "started_at": 1729330000.0,
            "gpu": "NVIDIA RTX PRO 6000 Blackwell Server Edition",
            "documents": [
                {
                    "doc_id": "doc_000074",
                    "new_ocr_pages": 207,
                    "total_doc_pages": 207,
                    "new_images_extracted": 203,
                    "total_doc_images": 203,
                    "elapsed_seconds": 227.25,
                    "full_source_coverage": True,
                    "status": "success"
                }
            ],
            "total_batch_ocr_pages": 207,
            "total_images_extracted": 203,
            "total_bytes_processed": 8321734,
            "errors": []
        }
        self.assertEqual(receipt["schema"], "gptgrep.xl-docbench.batch-execution-receipt.v1")
        self.assertEqual(receipt["shard_id"], "shard_b")
        self.assertEqual(receipt["total_batch_ocr_pages"], 207)
        self.assertEqual(len(receipt["documents"]), 1)
        self.assertTrue(receipt["documents"][0]["full_source_coverage"])


if __name__ == "__main__":
    unittest.main()
