"""Synthetic binding tests only; these are not live evaluation evidence."""

import json
import copy
import tempfile
import unittest
from unittest.mock import patch
from argparse import Namespace
from pathlib import Path

from verify import digest, main, read_answer, run_rewardkit, validate_grounding, validate_rewardkit_scores


class JudgmentBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.answer = self.root / "answer.json"
        self.answer.write_text('{"answer":"synthetic answer","citations":[]}\n')
        self.answer_hash = digest(self.answer.read_bytes())
        self.expected = {"task_id": "synthetic-task", "corpus_sha256": "a" * 64, "rubric_sha256": "b" * 64}
        self.judgment = {"schema": "gptgrep.grounding-judgment.v1", **self.expected,
                         "answer_sha256": self.answer_hash, "status": "complete",
                         "grounded_success": 1, "atomic_coverage": 0.75, "reason": "synthetic fixture"}
        self.path = self.root / "judgment.json"

    def tearDown(self):
        self.temp.cleanup()

    def validate(self, changes=None):
        value = {**self.judgment, **(changes or {})}
        self.path.write_text(json.dumps(value))
        return validate_grounding(self.path, self.expected, self.answer_hash)

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
        for field in ["task_id", "answer_sha256", "corpus_sha256", "rubric_sha256"]:
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
        self.path.write_text(json.dumps(self.judgment))
        expected = self.root / "expected.json"
        expected.write_text(json.dumps({**self.expected, "schema": "gptgrep.verifier-binding.v1"}))
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
        self.path.write_text(json.dumps(self.judgment))
        for status in ["timeout", "failed", "setup_failed", "pending", "starting", "unrecognized"]:
            with self.subTest(status=status):
                expected = self.root / "expected.json"
                expected.write_text(json.dumps({**self.expected, "schema": "gptgrep.verifier-binding.v1", "reader_status": status}))
                output = self.root / status
                args = Namespace(answer=self.answer, judgment=self.path, expected=expected, gold=self.root / "gold.jsonl",
                                 native_scorer=self.root / "scorer.py", output_dir=output, rewardkit=True)
                with patch("verify.parse_args", return_value=args), patch("verify.native_score", return_value={"status": "complete", "metrics": {"native_accuracy": 0.0}}):
                    main()
                report = json.loads((output / "feedback.json").read_text())
                self.assertFalse(report["promotion_allowed"])
                self.assertEqual(report["independent_grounding"]["grounded_success"], 1)
                if status not in ("timeout", "failed"):
                    self.assertEqual(report["status"], "unknown")
                    self.assertNotIn("grounded_success", report["metrics"])
                else:
                    self.assertEqual(report["metrics"]["grounded_success"], 0)


if __name__ == "__main__":
    unittest.main()
