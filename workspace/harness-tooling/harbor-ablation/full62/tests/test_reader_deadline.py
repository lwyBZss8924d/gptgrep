"""Synthetic question-exposure/deadline checks; no native or provider execution."""
from __future__ import annotations

import unittest
from unittest.mock import patch

from gptgrep_harbor import reader
from gptgrep_harbor.common import canonical, sha
from gptgrep_harbor.runtime import CODEX_VERSION
import test_contracts as fixtures


class ReaderDeadlineTests(unittest.TestCase):
    def execute(self, arm, *, config_delay=2, binary_delay=3, bundle_delay=5,
                question_delay=0.25, fail_at=None, answer_delay=0.5, envelope_delay=0):
        config = fixtures.LegacyTreatmentReaderTests().config()
        config["binding"]["arm"] = arm
        config["raw_document"] = "/data/corpus/synthetic.txt"
        question = b"synthetic question"
        now = [1000.0]
        outputs, calls, observed = {}, [], {}
        wall_start_ms = 1800000000000

        def advance(seconds):
            now[0] += seconds

        def read(path, *args):
            if path == "/opt/full62/reader.json":
                return canonical(config)
            self.assertEqual(path, "/synthetic/instruction.txt")
            # The original-clock receipt exists before any question bytes leave
            # the reader, even when reading/validating them itself takes time.
            marker = outputs["/logs/agent/attempt-start.json"]
            observed["deadline_at_question_exposure"] = marker["original_deadline_unix_ms"]
            advance(question_delay)
            return question

        def check(stage, seconds):
            advance(seconds)
            if fail_at == stage:
                raise ValueError("private fixture detail must not enter the receipt")

        def put(path, value):
            outputs[path] = value
            if path == "/answer/answer.json":
                advance(answer_delay)

        def encode(value):
            if value.get("schema") == "gptgrep.harbor.answer.v1":
                advance(envelope_delay)
            return canonical(value)

        def run(argv, **kwargs):
            calls.append((argv, kwargs, now[0]))
            if argv[-1] == "--version":
                data = ("codex-cli " + CODEX_VERSION).encode()
                advance(1)
            else:
                data = (fixtures.NativeAccountingTests().events() if arm == "raw_only" else fixtures.legacy_report())
                advance(2)
            return {"stdout": data, "stderr": b"", "elapsed_ms": 1, "return_code": 0,
                    "failure": None, "stdin_bytes": len(kwargs.get("input_bytes") or b""),
                    "stdin_bytes_sent": len(kwargs.get("input_bytes") or b""), "stdin_complete": True}

        with patch.object(reader, "read", side_effect=read), \
             patch.object(reader, "put", side_effect=put), \
             patch.object(reader, "canonical", side_effect=encode), \
             patch.object(reader, "check_config", side_effect=lambda *_: check("config", config_delay)), \
             patch.object(reader, "copy_asset", side_effect=lambda *_: check("binary", binary_delay)), \
             patch.object(reader, "check_product_bundle", side_effect=lambda *_: check("bundle", bundle_delay)), \
             patch.object(reader, "pinned", return_value=canonical({"ledger_path": "/opt/full62-private/ledger/decisions.json"})), \
             patch.object(reader, "native_environment", return_value={}), \
             patch.object(reader.Path, "exists", return_value=False), \
             patch.object(reader.time, "monotonic", side_effect=lambda: now[0]), \
             patch.object(reader.time, "time_ns", side_effect=lambda: int((wall_start_ms / 1000 + now[0] - 1000) * 10**9)), \
             patch.object(reader, "run", side_effect=run), \
             patch("sys.argv", ["reader", "--instruction", "/synthetic/instruction.txt"]):
            reader.main()
        return outputs, calls, observed

    def test_question_read_and_common_and_treatment_validation_consume_original_clock(self):
        for arm, preparation in (("raw_only", 5.25), ("gptgrep", 10.25)):
            with self.subTest(arm=arm):
                output, calls, seen = self.execute(arm)
                operation = output["/logs/agent/operational.json"]
                self.assertEqual(operation["status"], "response")
                self.assertEqual(seen["deadline_at_question_exposure"], 1800000020000)
                self.assertEqual(operation["original_deadline_unix_ms"], seen["deadline_at_question_exposure"])
                self.assertTrue(operation["clock_started_before_question_read"])
                self.assertEqual(len(calls), 2)
                self.assertAlmostEqual(calls[1][1]["timeout"], 20 - preparation - 1)
                self.assertEqual(operation["cumulative_query_wall_ms"], round((preparation + 1 + 2) * 1000))
                self.assertEqual(operation["answer_persistence_wall_ms"], 500)
                self.assertEqual(operation["answer_sha256"], sha(canonical(output["/answer/answer.json"])))

    def test_terminal_persistence_is_separately_observed_outside_query_budget(self):
        output, _, _ = self.execute("raw_only", answer_delay=20)
        operation = output["/logs/agent/operational.json"]
        self.assertEqual(operation["status"], "response")
        self.assertEqual(operation["cumulative_query_wall_ms"], 8250)
        self.assertEqual(operation["answer_persistence_wall_ms"], 20000)
        self.assertEqual(operation["query_wall_scope"],
                         "question_read_through_answer_envelope_hash; terminal_writes_excluded")

    def test_answer_envelope_construction_cannot_qualify_after_deadline(self):
        output, _, _ = self.execute("raw_only", envelope_delay=20)
        operation = output["/logs/agent/operational.json"]
        self.assertEqual(operation["status"], "operational_failure")
        self.assertEqual(operation["failure_stage"], "answer_validation")
        self.assertIsNone(output["/answer/answer.json"]["answer"])
        self.assertGreaterEqual(operation["cumulative_query_wall_ms"], operation["deadline_ms"])

    def test_validation_deadline_exhaustion_stops_before_any_native_launch(self):
        for arm, arguments, stage in (
            ("raw_only", {"binary_delay": 20}, "common_binary_validation"),
            ("gptgrep", {"bundle_delay": 20}, "arm_bundle_validation"),
        ):
            with self.subTest(arm=arm):
                output, calls, _ = self.execute(arm, **arguments)
                operation = output["/logs/agent/operational.json"]
                self.assertEqual(calls, [])
                self.assertEqual(operation["status"], "operational_failure")
                self.assertEqual(operation["failure_stage"], stage)
                self.assertGreaterEqual(operation["cumulative_query_wall_ms"], operation["deadline_ms"])
                self.assertEqual(operation["attempts"], [])
                self.assertIsNone(output["/answer/answer.json"]["answer"])

    def test_post_question_validation_failure_retains_bounded_redacted_receipt(self):
        for failing, stage in (("config", "common_config_validation"),
                               ("binary", "common_binary_validation"),
                               ("bundle", "arm_bundle_validation")):
            with self.subTest(failing=failing):
                output, calls, _ = self.execute("gptgrep", fail_at=failing)
                operation = output["/logs/agent/operational.json"]
                self.assertEqual(calls, [])
                self.assertEqual((operation["failure"], operation["failure_stage"]), ("reader_failed", stage))
                self.assertEqual(operation["status"], "operational_failure")
                self.assertGreater(operation["cumulative_query_wall_ms"], 0)
                self.assertLess(len(canonical(operation)), 16384)
                self.assertNotIn(b"private fixture detail", canonical(operation))
                self.assertIsNone(output["/answer/answer.json"]["answer"])


if __name__ == "__main__":
    unittest.main()
