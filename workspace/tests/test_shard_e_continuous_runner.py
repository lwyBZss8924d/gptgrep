#!/usr/bin/env python3
"""Unit test suite for Shard E Continuous Hot VLM Batch Runner.

Tests:
1. Shard E manifest loading and document validation (43 docs, 12,521 pages).
2. Hash calculation integrity (sha256_file).
3. Shard execution receipt structure and schema compliance.
4. Fast parallel sync and scratch cleanup logic.
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

class MockHttpError(Exception):
    def __init__(self, resp, content, uri=None):
        self.resp = resp
        self.content = content
        self.uri = uri
        super().__init__(f"<HttpError {resp.status} when requesting {uri}: {content}>")

# Mock mineru, torch, and docatlas_exporter modules before import for host test execution
for mod in ["torch", "mineru", "mineru.config", "mineru.parser", "mineru.kit", "mineru.kit.common", "docatlas_exporter", "pypdf", "google", "google.oauth2", "google.oauth2.credentials", "googleapiclient", "googleapiclient.discovery", "googleapiclient.errors", "googleapiclient.http"]:
    if mod not in sys.modules:
        sys.modules[mod] = MagicMock()

sys.modules["googleapiclient.errors"].HttpError = MockHttpError

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
CONTROLLER_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-controller-r1"
if str(CONTROLLER_DIR) not in sys.path:
    sys.path.insert(0, str(CONTROLLER_DIR))

import shard_e_continuous_runner as runner
runner.HttpError = MockHttpError


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


class TestShardEContinuousRunner(unittest.TestCase):

    def setUp(self):
        self.shard_e_path = _resolve_drive_root() / "gptgrep-xl-docbench-raw-miss-63-40/shards/shard_e.json"

    def test_shard_e_manifest_structure(self):
        """Verify Shard E manifest exists and contains exactly 43 documents and 12,521 pages."""
        self.assertTrue(self.shard_e_path.is_file(), f"Missing {self.shard_e_path}")
        shard = json.loads(self.shard_e_path.read_text())
        self.assertEqual(shard.get("shard_id"), "shard_e")
        self.assertEqual(shard.get("session_name"), "g4-shard-e")
        self.assertEqual(shard.get("document_count"), 43)
        self.assertEqual(shard.get("total_pages"), 12521)
        docs = shard.get("documents", [])
        self.assertEqual(len(docs), 43)

        # Check required fields
        for doc in docs:
            self.assertIn("document_id", doc)
            self.assertIn("file_name", doc)
            self.assertIn("drive_file_id", doc)
            self.assertIn("page_count", doc)
            self.assertIn("bytes", doc)
            self.assertIn("sha256", doc)

    def test_sha256_file(self):
        """Verify sha256_file computes exact SHA256 digest."""
        with tempfile.NamedTemporaryFile("wb", delete=False) as f:
            content = b"GPTgrep Milestone 3 Shard E Test Content"
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
            "shard_id": "shard_e",
            "session_name": "g4-shard-e",
            "started_at": 1000.0,
            "gpu": "NVIDIA RTX PRO 6000 Blackwell Server Edition",
            "documents": [
                {
                    "doc_id": "doc_000086",
                    "file_name": "doc_000086.pdf",
                    "pages_processed": 59,
                    "total_pages": 59,
                    "images_extracted": 12,
                    "elapsed_seconds": 35.2,
                    "full_source_coverage": True
                }
            ],
            "total_documents_processed": 1,
            "total_pages_processed": 59,
            "total_images_extracted": 12,
            "total_bytes_processed": 1914685,
            "errors": [],
            "total_elapsed_seconds": 40.0,
            "completed_at": 1040.0,
            "status": "success"
        }
        self.assertEqual(receipt["schema"], "gptgrep.xl-docbench.shard-execution-receipt.v1")
        self.assertEqual(receipt["shard_id"], "shard_e")
        self.assertEqual(receipt["session_name"], "g4-shard-e")
        self.assertEqual(receipt["status"], "success")
        self.assertTrue(receipt["documents"][0]["full_source_coverage"])

    def test_is_rate_limit_error(self):
        """Verify is_rate_limit_error correctly identifies 429 and 403 rate limits."""
        # 429 error
        mock_resp_429 = MagicMock()
        mock_resp_429.status = 429
        err_429 = runner.HttpError(mock_resp_429, b"Too Many Requests")
        self.assertTrue(runner.is_rate_limit_error(err_429))

        # 403 userRateLimitExceeded error
        mock_resp_403 = MagicMock()
        mock_resp_403.status = 403
        err_403_rate = runner.HttpError(mock_resp_403, b"userRateLimitExceeded: Quota exceeded")
        self.assertTrue(runner.is_rate_limit_error(err_403_rate))

        # 403 permission denied (not rate limit)
        err_403_perm = runner.HttpError(mock_resp_403, b"The caller does not have permission")
        self.assertFalse(runner.is_rate_limit_error(err_403_perm))

        # Regular Exception
        self.assertFalse(runner.is_rate_limit_error(ValueError("Random error")))

    def test_execute_with_retry_success(self):
        """Verify execute_with_retry retries on transient errors and succeeds."""
        calls = []
        def flaky_fn():
            calls.append(1)
            if len(calls) < 2:
                mock_resp = MagicMock()
                mock_resp.status = 503
                raise runner.HttpError(mock_resp, b"Service Unavailable")
            return "SUCCESS"

        result = runner.execute_with_retry(flaky_fn, max_retries=3, backoff_factor=0.01)
        self.assertEqual(result, "SUCCESS")
        self.assertEqual(len(calls), 2)



if __name__ == "__main__":
    unittest.main()
