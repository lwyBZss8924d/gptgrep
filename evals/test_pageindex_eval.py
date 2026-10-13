"""External benchmark metrics and provenance checks without external PDFs or calls."""

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/pageindex_eval.py"
sys.path.insert(0, str(SCRIPT.parent))
SPEC = importlib.util.spec_from_file_location("pageindex_eval", SCRIPT)
assert SPEC and SPEC.loader
external = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(external)


class PageIndexMetricTests(unittest.TestCase):
    def test_reference_materializes_gold_only_after_row_digest_check(self):
        row = {"doc_id": "original.pdf", "question": "Original fixture question?", "answer": "fixture answer",
               "evidence_pages": "[1]", "answer_format": "Str", "task_type": "lookup", "doc_type": "fixture"}
        metadata = {"doc_id": row["doc_id"]}
        metadata.update(row_index_zero_based=0,
                        row_sha256=hashlib.sha256(json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                        question_sha256=hashlib.sha256(row["question"].encode()).hexdigest())
        self.assertNotIn("answer", metadata)
        self.assertEqual(external.materialize_subset([row], [metadata])[0]["answer"], row["answer"])
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            external.materialize_subset([{**row, "answer": "changed"}], [metadata])
        for field in ("question", "answer", "evidence_pages", "answer_format", "task_type", "doc_type"):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "provenance only"):
                external.materialize_subset([row], [{**metadata, field: row[field]}])

    def test_same_page_in_wrong_document_does_not_count(self):
        result = external.page_quality([{"path": "wrong.pdf", "page_start": 2, "page_end": 2}], "right.pdf", {2})
        self.assertEqual(result["physical_page_recall_at_k"], 0.0)
        self.assertEqual(result["document_mrr_at_k"], 0.0)

    def test_below_threshold_fallback_hits_are_not_scored_as_retrieved_pages(self):
        flagged = {"path": "right.pdf", "page_start": 2, "page_end": 2, "below_threshold": True}
        result = external.page_quality([flagged], "right.pdf", {2})
        self.assertEqual(result["document_recall_at_k"], 0.0)
        self.assertEqual(result["physical_page_recall_at_k"], 0.0)
        self.assertEqual(result["recovered_gold_pages"], [])
        passing = {"path": "right.pdf", "page_start": 2, "page_end": 2, "below_threshold": False}
        result = external.page_quality([flagged, passing], "right.pdf", {2})
        # Ranks count only scored hits, as in scripts/eval.py.
        self.assertEqual(result["physical_page_mrr_at_k"], 1.0)
        self.assertEqual(result["physical_page_recall_at_k"], 1.0)

    def test_duplicate_hits_do_not_inflate_page_recall(self):
        hits = [{"path": "right.pdf", "page_start": 1, "page_end": 1}] * 2
        result = external.page_quality(hits, "right.pdf", {1, 2})
        self.assertEqual(result["physical_page_recall_at_k"], 0.5)

    def test_physical_page_must_agree_with_canonical_lines(self):
        with tempfile.TemporaryDirectory(prefix="gptgrep-page-oracle-") as directory:
            root = Path(directory)
            raw = b"opaque source bytes; parsing is not under test here"
            (root / "source.pdf").write_bytes(raw)
            parsed = {"source.pdf": {
                "text": "Alpha\nBeta\n",
                "pages": [{"number": 1, "line_start": 1, "line_end": 1},
                          {"number": 2, "line_start": 2, "line_end": 2}],
            }}
            hit = {
                "path": "source.pdf", "source_sha256": hashlib.sha256(raw).hexdigest(),
                "source_fresh": True, "coordinate_system": "extracted_lines_and_source_pages",
                "text": "Beta", "line_start": 2, "line_end": 2, "byte_start": 6, "byte_end": 10,
                "page_start": 2, "page_end": 2, "score": 1.0, "citation": "source.pdf:p2-p2",
            }
            self.assertEqual(external.validate_pdf_hit(hit, root, parsed)["path"], "source.pdf")
            with self.assertRaisesRegex(ValueError, "line mapping"):
                external.validate_pdf_hit({**hit, "page_start": 1, "page_end": 1}, root, parsed)


if __name__ == "__main__":
    unittest.main()
