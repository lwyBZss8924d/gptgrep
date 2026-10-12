"""Discriminating tests for the evaluation oracle; no model calls."""

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/eval.py"
SPEC = importlib.util.spec_from_file_location("gptgrep_eval", SCRIPT)
assert SPEC and SPEC.loader
evaluation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evaluation)


class EvaluationOracleTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="gptgrep-oracle-")
        self.root = Path(self.directory.name)
        self.source = self.root / "sample.txt"
        self.source.write_text("Header\nGrounded evidence.\n", encoding="utf-8")
        self.hit = {
            "path": "sample.txt",
            "line_start": 2,
            "line_end": 2,
            "page_start": None,
            "page_end": None,
            "text": "Grounded evidence.",
            "score": 1.0,
            "source_sha256": hashlib.sha256(self.source.read_bytes()).hexdigest(),
            "source_fresh": True,
            "citation": "sample.txt:2",
        }

    def tearDown(self):
        self.directory.cleanup()

    def test_valid_source_and_excerpt(self):
        self.assertEqual(evaluation.validate_hit(self.hit, self.root)["path"], "sample.txt")

    def test_stale_bytes_cannot_claim_fresh(self):
        self.source.write_text("Header\nInvented evidence.\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "digest"):
            evaluation.validate_hit(self.hit, self.root)

    def test_freshness_boolean_is_required(self):
        with self.assertRaisesRegex(ValueError, "fresh"):
            evaluation.validate_hit({**self.hit, "source_fresh": False}, self.root)

    def test_fabricated_excerpt_rejected(self):
        with self.assertRaisesRegex(ValueError, "Excerpt"):
            evaluation.validate_hit({**self.hit, "text": "A plausible fabrication."}, self.root)

    def test_invalid_line_bounds_rejected(self):
        with self.assertRaisesRegex(ValueError, "bounds"):
            evaluation.validate_hit({**self.hit, "line_start": 0}, self.root)

    def test_outside_corpus_rejected(self):
        with self.assertRaisesRegex(ValueError, "escapes"):
            evaluation.validate_hit({**self.hit, "path": "../outside.txt"}, self.root)

    def test_nonfinite_json_rejected(self):
        with self.assertRaisesRegex(ValueError, "Non-finite"):
            evaluation.decode_json('{"score": NaN}')

    def test_crlf_slice_preserves_internal_carriage_returns(self):
        raw = "Header\r\nCafé\r\nGrounded evidence.\r\n".encode()
        start, end = len(b"Header\r\n"), len(raw) - 2
        hit = {**self.hit, "line_start": 2, "line_end": 3,
               "text": raw[start:end].decode(), "byte_start": start, "byte_end": end,
               "column_start": 1}
        evaluation.validate_text_span(hit, raw)
        with self.assertRaisesRegex(ValueError, "exact byte"):
            evaluation.validate_text_span({**hit, "text": hit["text"].replace("\r\n", "\n")}, raw)

    def test_long_line_slice_uses_utf8_bytes_not_character_offsets(self):
        prefix = "雪" * 5000
        raw = ("Header\n" + prefix + "TARGET evidence" + "雪" * 5000 + "\n").encode()
        start = len(("Header\n" + prefix).encode())
        end = start + len(b"TARGET evidence")
        hit = {**self.hit, "text": "TARGET evidence", "byte_start": start, "byte_end": end,
               "column_start": len(prefix.encode()) + 1, "text_truncated": True}
        evaluation.validate_text_span(hit, raw)
        with self.assertRaises(ValueError):
            evaluation.validate_text_span({**hit, "byte_start": len("Header\n" + prefix)}, raw)

    def test_byte_range_cannot_name_different_line(self):
        with self.assertRaisesRegex(ValueError, "containment"):
            evaluation.validate_text_span(
                {**self.hit, "byte_start": 0, "byte_end": 6, "text": "Header"}, self.source.read_bytes()
            )

    def test_partial_byte_range_rejected(self):
        with self.assertRaisesRegex(ValueError, "Partial byte"):
            evaluation.validate_text_span({**self.hit, "byte_start": 7}, self.source.read_bytes())

    def test_utf8_boundary_split_rejected(self):
        raw = "Header\n雪TARGET\n".encode()
        with self.assertRaises(UnicodeDecodeError):
            evaluation.validate_text_span({**self.hit, "byte_start": 8, "byte_end": 10, "text": "雪"}, raw)

    def test_duplicate_hits_do_not_inflate_recall(self):
        relevant = [{"path": "a"}, {"path": "b"}]
        result = evaluation.quality([{"path": "a"}, {"path": "a"}], relevant, False)
        self.assertEqual(result["recall_at_k"], 0.5)
        self.assertFalse(result["passed"])

    def test_rank_and_partial_evidence_are_separate(self):
        result = evaluation.quality(
            [{"path": "wrong"}, {"path": "right", "text": "the correct quote"}],
            [{"path": "right", "contains": "correct quote"}],
            False,
        )
        self.assertEqual(result["recall_at_k"], 1.0)
        self.assertEqual(result["mrr_at_k"], 0.5)

    def test_correct_file_without_evidence_is_not_relevant(self):
        result = evaluation.quality(
            [{"path": "right", "text": "unrelated section"}],
            [{"path": "right", "contains": "required evidence"}],
            False,
        )
        self.assertEqual(result["recall_at_k"], 0.0)

    def test_no_answer_does_not_become_recall_one(self):
        result = evaluation.quality([], [], True)
        self.assertIsNone(result["recall_at_k"])
        self.assertTrue(result["no_answer_correct"])

    def test_exact_regex_forbids_extra_matches(self):
        result = evaluation.quality([{"path": "a"}, {"path": "extra"}], [{"path": "a"}], True)
        self.assertFalse(result["passed"])

    def test_percentile_interpolation_and_invalid_inputs(self):
        self.assertEqual(evaluation.percentile([1, 2, 3, 4], 0.5), 2.5)
        self.assertAlmostEqual(evaluation.percentile([1, 2, 3, 4], 0.95), 3.85)
        with self.assertRaises(ValueError):
            evaluation.percentile([1, float("inf")], 0.95)

    def test_cost_requires_a_receipt_for_every_observed_request(self):
        result = evaluation.summarize_provider_cost([{"jev_requests": 2, "jev_usage": [{"cost": 0.01}]}])
        self.assertIsNone(result["measured_provider_cost_usd"])
        self.assertEqual(result["provider_cost_missing_requests"], 1)

    def test_explicit_zero_cost_and_decimal_sum_are_preserved(self):
        result = evaluation.summarize_provider_cost([
            {"jev_requests": 2, "jev_usage": [{"cost": 0}, {"cost": 0.000001}]},
            {"jev_requests": 1, "jev_usage": [{"cost": 0.000002}]},
        ])
        self.assertEqual(result["measured_provider_cost_decimal"], "0.000003")
        self.assertEqual(result["provider_cost_missing_requests"], 0)

    def test_nonfinite_or_unmatched_cost_cannot_be_totalled(self):
        for metrics in [
            {"jev_requests": 1, "jev_usage": [{"cost": "Infinity"}]},
            {"jev_requests": 1, "jev_usage": [{"cost": 0.01}, {"cost": 0.02}]},
        ]:
            self.assertIsNone(evaluation.summarize_provider_cost([metrics])["measured_provider_cost_usd"])

    def test_retried_attempt_without_receipt_keeps_total_unknown(self):
        # A bounded retry: two attempts, one validated response with cost.
        result = evaluation.summarize_provider_cost([
            {"jev_calls_attempted": 2, "jev_requests": 1, "jev_usage": [{"cost": 0.01}],
             "jev_retried_attempts": [{"stage": "evidence_reranking", "http_status": 502,
                                       "reported_usage": None}]},
        ])
        self.assertIsNone(result["measured_provider_cost_usd"])
        self.assertEqual(result["provider_cost_missing_requests"], 1)
        self.assertEqual(result["cost_status"], "incomplete_provider_cost_receipts")

    def test_no_provider_request_is_not_a_cost_measurement(self):
        result = evaluation.summarize_provider_cost([{"jev_requests": 0, "jev_usage": []}])
        self.assertIsNone(result["measured_provider_cost_usd"])
        self.assertEqual(result["provider_cost_missing_requests"], 0)


if __name__ == "__main__":
    unittest.main()
