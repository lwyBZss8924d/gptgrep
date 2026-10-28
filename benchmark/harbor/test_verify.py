"""Synthetic binding tests only; these are not live evaluation evidence."""

import json
import copy
import tempfile
import unittest
from unittest.mock import patch
from argparse import Namespace
from pathlib import Path

from verify import (MAX_INPUT_BYTES, MAX_JSON_OUTPUT_BYTES, MAX_REASON_BYTES, digest, main,
                    read_answer, reason_record, run_rewardkit, validate_grounding,
                    validate_rewardkit_scores, write_json)


class JudgmentBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.answer = self.root / "answer.json"
        self.answer.write_text('{"answer":"synthetic answer","citations":[]}\n')
        self.answer_hash = digest(self.answer.read_bytes())
        self.expected = {"schema": "gptgrep.verifier-binding.v2", "execution_mode": "live",
                         "task_id": "synthetic-task", "corpus_sha256": "a" * 64, "rubric_sha256": "b" * 64,
                         "answer_sha256": self.answer_hash, "reader_status": "completed", "reader_valid_start": True,
                         "reader_thread_id": "synthetic-reader", "reader_turn_id": "synthetic-turn",
                         "reader_binding_sha256": "c" * 64}
        self.judgment = {**self.expected, "schema": "gptgrep.grounding-judgment.v1",
                         "answer_sha256": self.answer_hash, "status": "complete",
                         "grounded_success": 1, "atomic_coverage": 0.75, "reason": "synthetic fixture"}
        self.path = self.root / "judgment.json"

    def tearDown(self):
        self.temp.cleanup()

    def validate(self, changes=None):
        value = {**self.judgment, **(changes or {})}
        self.path.write_text(json.dumps(value))
        return validate_grounding(self.path, self.expected, self.answer_hash)

    def write_expected(self, changes=None):
        value = {**self.expected, **(changes or {})}
        value.pop("reader_binding_sha256", None)
        path = self.root / "expected.json"
        path.write_text(json.dumps(value))
        self.path.write_text(json.dumps({**self.judgment, "reader_binding_sha256": digest(path.read_bytes())}))
        return path

    def test_complete_metrics_are_independent(self):
        result = self.validate()
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["grounded_success"], 1)
        self.assertEqual(result["atomic_coverage"], 0.75)

    def test_missing_or_unknown_judgment_never_becomes_zero(self):
        for result in [validate_grounding(None, self.expected, self.answer_hash),
                       self.validate({"status": "unknown", "grounded_success": None, "atomic_coverage": None})]:
            self.assertEqual(result["status"], "unknown")
            self.assertIsNone(result["grounded_success"])
            self.assertIsNone(result["atomic_coverage"])

    def test_every_binding_is_required(self):
        for field in ["task_id", "answer_sha256", "corpus_sha256", "rubric_sha256", "reader_binding_sha256"]:
            with self.subTest(field=field):
                self.assertEqual(self.validate({field: "wrong"})["status"], "unknown")

    def test_complete_requires_all_valid_scores(self):
        for changes in [{"grounded_success": None}, {"grounded_success": True},
                        {"grounded_success": 0.5}, {"atomic_coverage": None},
                        {"atomic_coverage": -1}, {"atomic_coverage": 2}, {"atomic_coverage": float("nan")}]:
            with self.subTest(changes=changes):
                self.assertEqual(self.validate(changes)["status"], "unknown")

    def test_failure_is_observed_zero_when_judge_completed(self):
        result = self.validate({"grounded_success": 0, "atomic_coverage": 0})
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["grounded_success"], 0)

    def test_answer_hash_covers_citations_and_whitespace(self):
        first = read_answer(self.answer)
        self.answer.write_text('{"answer":"synthetic answer","citations":[{"document_id":"x","page":1}]}\n')
        self.assertNotEqual(first["sha256"], read_answer(self.answer)["sha256"])
        self.assertEqual(read_answer(self.answer)["status"], "success")

    def test_reason_preview_preserves_utf8_and_full_reason_identity(self):
        reason = "document \U0001f4c4\n" * 5000
        result = self.validate({"reason": reason})
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["grounded_success"], 1)
        self.assertEqual(result["atomic_coverage"], 0.75)
        self.assertLessEqual(len(result["reason"].encode("utf-8")), MAX_REASON_BYTES)
        self.assertNotIn("\ufffd", result["reason"])
        self.assertTrue(reason.startswith(result["reason"]))
        self.assertEqual(result["reason_retention"], {
            "sha256": digest(reason.encode("utf-8")), "utf8_bytes": len(reason.encode("utf-8")),
            "preview_utf8_bytes": len(result["reason"].encode("utf-8")), "truncated": True})
        self.assertEqual(result["judgment_sha256"], digest(self.path.read_bytes()))
        self.assertEqual(json.loads(self.path.read_text())["reason"], reason)
        self.assertFalse(reason_record("short")["reason_retention"]["truncated"])

    def test_output_byte_bound_precedes_creation_or_truncation(self):
        path = self.root / "bounded-output.json"
        oversized = {"reason": "x" * MAX_JSON_OUTPUT_BYTES}
        with self.assertRaisesRegex(ValueError, "byte bound"):
            write_json(path, oversized)
        self.assertFalse(path.exists())
        path.write_text("retained prior bytes")
        with self.assertRaisesRegex(ValueError, "byte bound"):
            write_json(path, oversized)
        self.assertEqual(path.read_text(), "retained prior bytes")

    def test_oversized_and_symlinked_inputs_cannot_be_scored(self):
        self.answer.write_bytes(b"x" * (MAX_INPUT_BYTES + 1))
        with self.assertRaisesRegex(ValueError, "bounded regular"):
            read_answer(self.answer)
        self.path.write_bytes(b"x" * (MAX_INPUT_BYTES + 1))
        result = validate_grounding(self.path, self.expected, self.answer_hash)
        self.assertEqual(result["status"], "unknown")
        self.assertIsNone(result["grounded_success"])
        self.answer.unlink()
        self.answer.symlink_to(self.path)
        with self.assertRaisesRegex(ValueError, "bounded regular"):
            read_answer(self.answer)

    def test_rewardkit_keeps_dimensions_and_omits_unknown_grounding(self):
        report = self.root / "feedback.json"
        report.write_text(json.dumps({"schema": "gptgrep.xl-verifier-feedback.v1",
                                     "native": {"status": "complete"}, "grounding": {"status": "unknown"},
                                     "metrics": {"native_accuracy": 1.0, "native_token_f1": 0.4, "native_anls": 0.2}}))
        rewards = run_rewardkit(self.root, report)
        self.assertEqual(rewards, {"native_accuracy": 1.0, "native_token_f1": 0.4, "native_anls": 0.2})
        self.assertNotIn("grounded_success", rewards)
        self.assertNotIn("reward", rewards)

    def test_rewardkit_accepts_observed_grounding_zero_without_blending(self):
        report = self.root / "feedback.json"
        report.write_text(json.dumps({"schema": "gptgrep.xl-verifier-feedback.v1",
                                     "native": {"status": "complete"},
                                     "grounding": {"status": "complete", "reason": "synthetic"},
                                     "metrics": {"native_accuracy": 1.0, "grounded_success": 0, "atomic_coverage": 0.3}}))
        rewards = run_rewardkit(self.root, report)
        self.assertEqual(rewards, {"native_accuracy": 1.0, "grounded_success": 0, "atomic_coverage": 0.3})

    def test_rewardkit_retains_bounded_reason_and_original_judgment_pointer(self):
        reason = "\U0001f4c4\n\"\u0000" * 10000
        grounding = self.validate({"reason": reason})
        report = self.root / "feedback.json"
        metrics = {"native_accuracy": 1.0, "native_token_f1": 0.4, "native_anls": 0.2,
                   "grounded_success": 1, "atomic_coverage": 0.75}
        write_json(report, {"schema": "gptgrep.xl-verifier-feedback.v1", "native": {"status": "complete"},
                            "grounding": grounding, "metrics": metrics})
        self.assertEqual(run_rewardkit(self.root, report), metrics)
        details_path = self.root / "reward-details.json"
        details = json.loads(details_path.read_text())
        self.assertLessEqual(details_path.stat().st_size, MAX_JSON_OUTPUT_BYTES)
        for key in ("grounded_success", "atomic_coverage"):
            retained = details[key]["criteria"][0]["reasoning"]
            self.assertLess(len(retained.encode("utf-8")), 3 * 1024)
            self.assertIn(grounding["judgment_sha256"], retained)
            self.assertIn(digest(reason.encode("utf-8")), retained)
        self.assertEqual(json.loads(self.path.read_text())["reason"], reason)

    def test_rewardkit_rejects_invalid_dimensions_before_creating_criteria(self):
        report = self.root / "feedback.json"
        for metrics in [{}, {"unexpected": 1}, {"native_accuracy": True}, {"native_accuracy": "x" * 10000}]:
            report.write_text(json.dumps({"schema": "gptgrep.xl-verifier-feedback.v1", "metrics": metrics}))
            with self.subTest(metrics=list(metrics)), self.assertRaisesRegex(ValueError, "validated metric"):
                run_rewardkit(self.root, report)
            self.assertFalse((self.root / "rewardkit-criteria").exists())

    def test_rewardkit_requires_new_output_directory_for_a_second_run(self):
        report = self.root / "feedback.json"
        write_json(report, {"schema": "gptgrep.xl-verifier-feedback.v1", "native": {"status": "complete"},
                            "grounding": {"status": "unknown"}, "metrics": {"native_accuracy": 1.0}})
        self.assertEqual(run_rewardkit(self.root, report), {"native_accuracy": 1.0})
        before = sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*"))
        with self.assertRaises(FileExistsError):
            run_rewardkit(self.root, report)
        self.assertEqual(sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*")), before)

    def test_rewardkit_preserves_raw_fraction_and_validates_rounded_aggregate(self):
        report = self.root / "feedback.json"
        metrics = {"native_token_f1": 0.181818, "native_anls": 0.666667}
        report.write_text(json.dumps({"schema": "gptgrep.xl-verifier-feedback.v1",
                                     "native": {"status": "complete"}, "grounding": {"status": "unknown"},
                                     "metrics": metrics}))
        self.assertEqual(run_rewardkit(self.root, report), {"native_token_f1": 0.1818, "native_anls": 0.6667})
        self.assertEqual(json.loads(report.read_text())["metrics"], metrics)
        details = json.loads((self.root / "reward-details.json").read_text())
        self.assertEqual(details["native_token_f1"]["criteria"][0]["raw"], 0.181818)

    def test_rewardkit_rejects_divergent_or_missing_evidence(self):
        metrics = {"native_token_f1": 0.181818}
        rewards = {"native_token_f1": 0.1818}
        detail = {"kind": "programmatic", "score": 0.1818,
                  "criteria": [{"name": "xl_metric:native_token_f1", "raw": 0.181818,
                                "value": 0.1818, "weight": 1.0}]}
        validate_rewardkit_scores(metrics, rewards, {"native_token_f1": detail})
        for value in [0.181810, None, True, "0.181818", float("nan"), float("inf"), -0.1, 1.1]:
            with self.subTest(raw=value):
                changed = copy.deepcopy(detail)
                changed["criteria"][0]["raw"] = value
                with self.assertRaises(ValueError):
                    validate_rewardkit_scores(metrics, rewards, {"native_token_f1": changed})
        for field in ["raw", "value", "weight", "name"]:
            with self.subTest(missing=field):
                changed = copy.deepcopy(detail)
                del changed["criteria"][0][field]
                with self.assertRaises(ValueError):
                    validate_rewardkit_scores(metrics, rewards, {"native_token_f1": changed})
        for changed in [{}, {"native_token_f1": None}, {"native_token_f1": detail, "grounded_success": detail}]:
            with self.subTest(details=changed), self.assertRaises(ValueError):
                validate_rewardkit_scores(metrics, rewards, changed)
        for field, value in [("score", 0.182), ("kind", "llm"), ("error", "failed"),
                             ("criteria", []), ("criteria", detail["criteria"] * 2)]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                validate_rewardkit_scores(metrics, rewards, {"native_token_f1": {**detail, field: value}})
        for field, value in [("value", 0.182), ("name", "xl_metric:native_accuracy"),
                             ("weight", 0.5), ("weight", True), ("error", "failed")]:
            with self.subTest(criterion_field=field):
                changed = copy.deepcopy(detail)
                changed["criteria"][0][field] = value
                with self.assertRaises(ValueError):
                    validate_rewardkit_scores(metrics, rewards, {"native_token_f1": changed})
        for changed in [{}, {"native_token_f1": 0.182}, {"native_token_f1": float("nan")},
                        {"native_token_f1": True}, {"native_token_f1": 0.1818, "native_accuracy": 0}]:
            with self.subTest(rewards=changed), self.assertRaises(ValueError):
                validate_rewardkit_scores(metrics, changed, {"native_token_f1": detail})

    def test_rewardkit_validation_failure_keeps_feedback_unknown(self):
        expected = self.write_expected()
        output = self.root / "unknown-reward"
        args = Namespace(answer=self.answer, judgment=self.path, expected=expected, gold=self.root / "gold.jsonl",
                         native_scorer=self.root / "scorer.py", output_dir=output, rewardkit=True)
        metrics = {"native_token_f1": 0.181818}
        with patch("verify.parse_args", return_value=args), patch("verify.native_score", return_value={"status": "complete", "metrics": metrics}), \
                patch("verify.validate_rewardkit_scores", side_effect=ValueError("synthetic raw mismatch")):
            self.assertEqual(main(), 2)
        feedback = json.loads((output / "feedback.json").read_text())
        self.assertEqual(feedback["status"], "unknown")
        self.assertEqual(feedback["rewardkit_status"], "unknown")
        self.assertFalse(feedback["promotion_allowed"])
        self.assertEqual(feedback["metrics"]["native_token_f1"], 0.181818)

    def test_partial_answer_cannot_rescue_failed_reader_and_setup_stays_unknown(self):
        for status in ["timeout", "failed", "setup_failed", "pending", "starting", "unrecognized"]:
            with self.subTest(status=status):
                expected = self.write_expected({"reader_status": status, "reader_valid_start": status != "setup_failed"})
                output = self.root / status
                args = Namespace(answer=self.answer, judgment=self.path, expected=expected, gold=self.root / "gold.jsonl",
                                 native_scorer=self.root / "scorer.py", output_dir=output, rewardkit=True)
                with patch("verify.parse_args", return_value=args), patch("verify.native_score", return_value={"status": "complete", "metrics": {"native_accuracy": 0.0}}):
                    main()
                report = json.loads((output / "feedback.json").read_text())
                self.assertFalse(report["promotion_allowed"])
                if status in ("timeout", "failed", "setup_failed"):
                    self.assertEqual(report["independent_grounding"]["grounded_success"], 1)
                if status not in ("timeout", "failed"):
                    self.assertEqual(report["status"], "unknown")
                    self.assertNotIn("grounded_success", report["metrics"])
                else:
                    self.assertEqual(report["metrics"]["grounded_success"], 0)

    def test_unsealed_export_cannot_score_and_changed_answer_fails_binding(self):
        for changes in [{"schema": "gptgrep.verifier-binding.v1"}, {"answer_sha256": "d" * 64},
                        {"reader_thread_id": None}, {"reader_valid_start": False}]:
            expected = self.write_expected(changes)
            args = Namespace(answer=self.answer, judgment=self.path, expected=expected, gold=self.root / "gold.jsonl",
                             native_scorer=self.root / "scorer.py", output_dir=self.root / "rejected", rewardkit=True)
            with patch("verify.parse_args", return_value=args), patch("verify.native_score") as scorer:
                self.assertEqual(main(), 2)
                scorer.assert_not_called()
            self.assertFalse(json.loads((args.output_dir / "feedback.json").read_text())["promotion_allowed"])


if __name__ == "__main__":
    unittest.main()
