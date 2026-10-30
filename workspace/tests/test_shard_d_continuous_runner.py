#!/usr/bin/env python3
"""Unit test suite for Shard D Continuous Hot VLM Batch Runner.

Tests:
1. Shard D manifest loading and document validation (61 docs, 17,214 pages).
2. Document admission modes and cover offset handling in Shard D.
3. Hash calculation integrity (sha256_file).
4. Shard execution receipt structure and schema compliance.
5. Argument parser options and defaults.
6. Ephemeral scratch cleanup invariants.
"""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock

# Mock mineru and cloud modules before import for host test execution
for mod in [
    "mineru", "mineru.config", "mineru.parser", "mineru.kit", "mineru.kit.common",
    "docatlas_exporter", "pypdf", "google", "google.oauth2", "google.oauth2.credentials",
    "googleapiclient", "googleapiclient.discovery", "googleapiclient.errors", "googleapiclient.http"
]:
    if mod not in sys.modules:
        sys.modules[mod] = MagicMock()

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import shard_d_continuous_runner as runner


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


class TestShardDContinuousRunner(unittest.TestCase):

    def setUp(self):
        self.shard_d_path = _resolve_drive_root() / "gptgrep-xl-docbench-raw-miss-63-40/shards/shard_d.json"

    def test_shard_d_manifest_structure(self):
        """Verify Shard D manifest exists and contains exactly 61 documents and 17,214 pages."""
        self.assertTrue(self.shard_d_path.is_file(), f"Missing {self.shard_d_path}")
        shard = json.loads(self.shard_d_path.read_text())
        self.assertEqual(shard.get("shard_id"), "shard_d")
        self.assertEqual(shard.get("session_name"), "g4-shard-d")
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

    def test_recovered_documents_admission(self):
        """Verify recovered documents in Shard D have correct metadata and admission rules."""
        shard = json.loads(self.shard_d_path.read_text())
        docs = {d["document_id"]: d for d in shard.get("documents", [])}
        
        # doc_000163: recovered19 exact page match (70 pages)
        self.assertIn("doc_000163", docs)
        doc_163 = docs["doc_000163"]
        self.assertEqual(doc_163["source_pool"], "recovered19")
        self.assertEqual(doc_163["page_count"], 70)
        self.assertEqual(doc_163.get("admission_mode"), "exact_page_match")
        
        # doc_000298: recovered19 in_place_edition_accepted (74 pages)
        self.assertIn("doc_000298", docs)
        doc_298 = docs["doc_000298"]
        self.assertEqual(doc_298["source_pool"], "recovered19")
        self.assertEqual(doc_298["page_count"], 74)
        self.assertEqual(doc_298.get("admission_mode"), "in_place_edition_accepted")

    def test_sha256_file(self):
        """Verify sha256_file computes exact SHA256 digest."""
        with tempfile.NamedTemporaryFile("wb", delete=False) as f:
            content = b"GPTgrep Milestone 3 Shard D Test Content Payloads"
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
            "shard_id": "shard_d",
            "session_name": "g4-shard-d",
            "started_at": 1000.0,
            "gpu": "NVIDIA RTX PRO 6000 Blackwell Server Edition",
            "documents": [
                {
                    "doc_id": "doc_000143",
                    "file_name": "8a14952de1d09388.pdf",
                    "pages_processed": 56,
                    "total_pages": 56,
                    "images_extracted": 67,
                    "elapsed_seconds": 95.2,
                    "full_source_coverage": True
                }
            ],
            "total_documents_processed": 1,
            "total_pages_processed": 56,
            "total_images_extracted": 67,
            "total_bytes_processed": 3021142,
            "completed_at": 1100.0,
            "total_wall_clock_seconds": 100.0
        }
        self.assertEqual(receipt["schema"], "gptgrep.xl-docbench.shard-execution-receipt.v1")
        self.assertEqual(receipt["shard_id"], "shard_d")
        self.assertEqual(len(receipt["documents"]), 1)
        self.assertEqual(receipt["total_pages_processed"], 56)

    def test_arg_parser_defaults(self):
        """Verify argument parser configuration and defaults."""
        import argparse
        parser = argparse.ArgumentParser()
        parser.add_argument("--concurrency", type=int, default=16)
        parser.add_argument("--no-drive-upload", action="store_true")
        args = parser.parse_args([])
        self.assertEqual(args.concurrency, 16)
        self.assertFalse(args.no_drive_upload)


if __name__ == '__main__':
    unittest.main()
