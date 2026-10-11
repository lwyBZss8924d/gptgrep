"""Synthetic regressions; all temporary files use the selected external root."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from gptgrep_harbor.adapter import Full62Adapter, validate_cohort, env_templates
import gptgrep_harbor.adapter as adapter_module
from gptgrep_harbor.aggregate import paired, summarize, time_bounds
from gptgrep_harbor.common import MAX_ANSWER, MAX_NATIVE_PROMPT_BYTES, PROFILE, canonical, parse, sha
from gptgrep_harbor.native import codex_accounting, codex_result, prompt_bytes
from gptgrep_harbor.reader import product_accounting
from gptgrep_harbor import reader, verifier
from gptgrep_harbor.runtime import CODEX_VERSION
from gptgrep_harbor.verifier import grade, transport_reward
from gptgrep_harbor.process import run as bounded_process


def synthetic_cohort():
    return {"schema": "gptgrep.harbor.cohort.v1", "source_cohort_sha256": "a" * 64,
            "tasks": [{"pair_id": f"synthetic-{i}", "document_id": f"generated-{i % 34}.txt",
                       "question": f"What number is printed in synthetic record {i}?",
                       "reference": str(i * 17), "answer_format": "integer"} for i in range(62)]}


def synthetic_results():
    rows, inputs, blobs = [], [], {}
    runtime = {"deadline_seconds": 20, "bootstrap_seed": 19, "profile": PROFILE,
               "primary_metric": "answer_equivalence_delta"}
    export = {"expected_pairs": 62, "expected_documents": 34,
              "cohort": {"sha256": "a" * 64}, "runtime": {"sha256": "b" * 64},
              "judge_contract": {"sha256": "c" * 64}, "tasks": rows}
    for index, source in enumerate(synthetic_cohort()["tasks"]):
        for arm in ("gptgrep", "raw_only"):
            name = f"synthetic-{index}-{arm}"
            binding = {"pair_id": source["pair_id"], "document_id": source["document_id"], "arm": arm,
                       "cohort_sha256": "a" * 64, "runtime_sha256": "b" * 64, "judge_sha256": "c" * 64,
                       "question_sha256": sha(source["question"].encode()), "document_sha256": "d" * 64}
            answer = {"schema": "gptgrep.harbor.answer.v1", "binding": binding,
                      "status": "response", "answer": "a wholly synthetic response"}
            operation = {"schema": "gptgrep.harbor.operation.v1", "binding": binding,
                         "status": "response", "answer_sha256": sha(canonical(answer)),
                         "deadline_ms": 20000, "cumulative_query_wall_ms": 500,
                         "attempts": [{"ordinal": 0, "cost_complete": False, "cost_usd": None}]}
            judgment = {"schema": "gptgrep.harbor.judgment.v1", "binding": binding,
                        "label": 1, "status": "judged", "answer_envelope_sha256": sha(canonical(answer))}
            record = {"task_name": name, "answer": name + "/answer", "operation": name + "/operation",
                      "judgment": name + "/judgment", "grounding": None}
            for field, body in (("answer", answer), ("operation", operation), ("judgment", judgment)):
                blobs[record[field]] = canonical(body)
            rows.append({"task_name": name, "binding": binding})
            inputs.append(record)
    return export, inputs, runtime, blobs


class CohortTests(unittest.TestCase):
    def test_duplicate_pair_rejected_even_with_correct_counts(self):
        cohort = synthetic_cohort()
        cohort["tasks"][-1]["pair_id"] = cohort["tasks"][0]["pair_id"]
        with self.assertRaisesRegex(ValueError, "duplicate_pair"):
            validate_cohort(cohort)

    def test_missing_cohort_row_rejected(self):
        cohort = synthetic_cohort()
        cohort["tasks"].pop()
        with self.assertRaisesRegex(ValueError, "full62_required"):
            validate_cohort(cohort)

    def test_reader_projection_rejects_extra_gold_or_evidence_fields(self):
        cohort = synthetic_cohort()
        cohort["tasks"][0]["evidence_pages"] = [7]
        with self.assertRaisesRegex(ValueError, "row_fields"):
            validate_cohort(cohort)

    def test_secret_values_cannot_enter_runtime_config(self):
        with self.assertRaisesRegex(ValueError, "environment_reference_only"):
            env_templates({"CODEX_API_KEY": "synthetic-secret-value"})
        with self.assertRaisesRegex(ValueError, "api_auth_environment_forbidden"):
            env_templates({"CODEX_API_KEY": "${DEV_TEST_EVAL_KEY}"})
        self.assertEqual(env_templates({"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"}),
                         {"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"})

    def test_duplicate_json_keys_are_not_silently_rebound(self):
        with self.assertRaisesRegex(ValueError, "duplicate_json_key"):
            parse(b'{"arm":"gptgrep","arm":"raw_only"}')


class PrimaryJudgeTests(unittest.TestCase):
    def fixture(self):
        export, records, _, blobs = synthetic_results()
        envelope = parse(blobs[records[0]["answer"]])
        config = {"binding": export["tasks"][0]["binding"], "question": "A generic question",
                  "reference": "A generic reference", "answer_format": "text",
                  "contract": {"prompt": "Q:{question}\nR:{answer}\nF:{answer_format}\nA:{response}",
                               "response_schema": {"type": "object", "required": ["equivalent"],
                                                   "properties": {"equivalent": {"type": "boolean"}}}}}
        return config, envelope

    def test_complete_answer_and_only_primary_fields_enter_judge(self):
        config, envelope = self.fixture()
        envelope["answer"] = "synthetic-first " + "x" * 40000 + " synthetic-final"
        seen = []
        def invoke(prompt, schema):
            seen.append(prompt)
            return {"equivalent": True}, {"cost_usd": None}
        result = grade(config, envelope, invoke)
        self.assertEqual(result["label"], 1)
        self.assertTrue(seen[0].endswith("synthetic-final"))
        self.assertNotIn("document_sha256", seen[0])
        self.assertNotIn(config["binding"]["pair_id"], seen[0])

    def test_unavailable_judge_is_unknown_without_primary_reward_scalar(self):
        config, envelope = self.fixture()
        def unavailable(_prompt, _schema):
            raise TimeoutError("synthetic")
        result = grade(config, envelope, unavailable)
        self.assertIsNone(result["label"])
        reward = transport_reward(result)
        self.assertNotIn("answer_equivalence", reward)
        self.assertEqual((reward["answer_equivalence_lower"], reward["answer_equivalence_upper"]), (0, 1))

    def test_operational_failure_zero_does_not_invoke_judge(self):
        config, envelope = self.fixture()
        envelope.update(status="operational_failure", answer=None)
        result = grade(config, envelope, lambda *_: self.fail("Judge called for operational failure"))
        self.assertEqual(result["label"], 0)

    def test_primary_boolean_rejects_string_approximation(self):
        config, envelope = self.fixture()
        result = grade(config, envelope, lambda *_: ({"equivalent": "true"}, {}))
        self.assertIsNone(result["label"])


class PairedTests(unittest.TestCase):
    def aggregate(self, export, records, runtime, blobs):
        with patch("gptgrep_harbor.aggregate.pinned", side_effect=lambda pin, *args: blobs[pin]):
            return summarize(export, records, runtime)

    def test_missing_outputs_remain_zero_in_62_denominator(self):
        export, records, runtime, blobs = synthetic_results()
        records[0].update(answer=None, operation=None, judgment=None)
        result = self.aggregate(export, records, runtime, blobs)
        self.assertEqual(result["search"]["gptgrep"]["accuracy"], 61 / 62)
        self.assertEqual(result["search"]["gptgrep"]["missing_tasks"], 1)

    def test_verifier_start_failure_with_valid_answer_is_unknown(self):
        export, records, runtime, blobs = synthetic_results()
        records[0]["judgment"] = None
        result = self.aggregate(export, records, runtime, blobs)
        self.assertIsNone(result["search"]["gptgrep"]["accuracy"])
        self.assertEqual(result["search"]["gptgrep"]["accuracy_bounds"], [61 / 62, 1])
        self.assertEqual(result["paired"]["delta_bounds"], [-1 / 62, 0])

    def test_duplicate_result_and_missing_binding_rejected(self):
        export, records, runtime, blobs = synthetic_results()
        records[-1] = copy.deepcopy(records[0])
        with self.assertRaisesRegex(ValueError, "missing_or_duplicate_result_binding"):
            self.aggregate(export, records, runtime, blobs)

    def test_cross_arm_document_binding_mismatch_rejected(self):
        export, records, runtime, blobs = synthetic_results()
        export["tasks"][1]["binding"]["document_sha256"] = "e" * 64
        with self.assertRaises(ValueError):
            self.aggregate(export, records, runtime, blobs)

    def test_unknown_cost_is_not_free(self):
        export, records, runtime, blobs = synthetic_results()
        result = self.aggregate(export, records, runtime, blobs)
        self.assertIsNone(result["search"]["gptgrep"]["query_cost_usd"])
        self.assertEqual(result["search"]["gptgrep"]["query_cost_unknown_tasks"], 62)

    def test_swap_negates_paired_delta_and_interval(self):
        rows = [{"pair_id": str(i), "document_id": str(i % 34),
                 "gptgrep": int(i % 5 != 0), "raw_only": int(i % 3 != 0)} for i in range(62)]
        first = paired(rows, 734)
        second = paired([{**r, "gptgrep": r["raw_only"], "raw_only": r["gptgrep"]} for r in rows], 734)
        self.assertAlmostEqual(first["exact_delta"], -second["exact_delta"])
        self.assertAlmostEqual(first["cluster_interval"][0], -second["cluster_interval"][1])
        self.assertEqual(first["mcnemar_exact_two_sided_p"], second["mcnemar_exact_two_sided_p"])

    def test_failure_aware_deadline_and_unknown_time(self):
        self.assertEqual(time_bounds(0, "judged", 500, 20000), (20000, 20000))
        self.assertEqual(time_bounds(None, "judge_unavailable", 500, 20000), (500, 20000))
        self.assertEqual(time_bounds(1, "judged", None, 20000), (0, 20000))


class NativeAccountingTests(unittest.TestCase):
    usage = {"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}

    def events(self, text="synthetic"):
        message = [] if text is None else [{"type": "item.completed", "item": {"type": "agent_message", "text": text}}]
        return b"\n".join(canonical(event) for event in message + [{"type": "turn.completed", "usage": self.usage}])

    def failed_streams(self):
        complete = self.events()
        return {"missing_answer": self.events(None), "blank_answer": self.events("  "),
                "late_error": complete + b'\n{"type":"error"}',
                "partial_suffix": complete + b'\n{"type":',
                "duplicate_completion": complete + b"\n" + self.events(None)}

    def test_observed_usage_survives_invalid_answer_error_partial_and_duplicate_streams(self):
        for name, data in self.failed_streams().items():
            with self.subTest(name=name):
                info = codex_accounting(data)
                self.assertEqual(info["native_completed_turn_usage"][0]["usage"], self.usage)
                self.assertEqual(info["usage"], None if name == "duplicate_completion" else self.usage)
                self.assertTrue(info["native_stream_incomplete"])
                self.assertIsNone(info["physical_provider_requests"])
                self.assertIsNone(info["cost_usd"])
                self.assertFalse(info["served_profile_verified"])
                with self.assertRaises(ValueError):
                    codex_result(data)
        self.assertFalse(codex_accounting(self.events(None))["native_answer_valid"])
        self.assertTrue(codex_accounting(self.failed_streams()["late_error"])["native_error_observed"])
        self.assertEqual(codex_accounting(self.failed_streams()["partial_suffix"])["native_malformed_events"], 1)
        ambiguous = codex_accounting(self.failed_streams()["duplicate_completion"])
        self.assertTrue(ambiguous["native_completion_ambiguous"])
        self.assertEqual(len(ambiguous["native_completed_turn_usage"]), 2)

    def test_invalid_usage_is_not_invented_and_projection_has_a_finite_observation_limit(self):
        invalid = canonical({"type": "turn.completed", "usage": {**self.usage, "cached_input_tokens": 101}})
        info = codex_accounting(invalid)
        self.assertIsNone(info["usage"])
        self.assertEqual(info["native_completed_turn_usage"], [])
        self.assertEqual(info["native_invalid_usage_events"], 1)
        repeated = codex_accounting(b"\n".join([self.events(None)] * 65))
        self.assertIsNone(repeated["usage"])
        self.assertEqual(repeated["native_completed_turns"], 65)
        self.assertEqual(len(repeated["native_completed_turn_usage"]), 64)
        self.assertEqual(repeated["native_usage_observations_omitted"], 1)
        self.assertFalse(repeated["accounting_projection_complete"])

    def binding(self):
        return {"arm": "raw_only", "pair_id": "synthetic", "document_id": "synthetic-document",
                "cohort_sha256": "a" * 64, "runtime_sha256": "b" * 64, "judge_sha256": "c" * 64,
                "question_sha256": sha(b"synthetic question"), "document_sha256": "d" * 64}

    def native_run_result(self, stdout, **overrides):
        return {"stdout": stdout, "stderr": b"", "elapsed_ms": 10, "return_code": 0,
                "failure": None, "stdin_bytes": 20, "stdin_bytes_sent": 20, "stdin_complete": True, **overrides}

    def reader_main(self, stdout, **overrides):
        config = {"binding": self.binding(), "profile": PROFILE, "codex_binary": "/synthetic/codex",
                  "gptgrep_binary": "/synthetic/gptgrep", "raw_document": "/data/corpus/synthetic.txt",
                  "reader_base_binary": {"path": "/synthetic/codex"}, "max_input_bytes": 8192,
                  "native_cli": {}, "native_config_sha256": "e" * 64, "deadline_seconds": 20,
                  "base_tool_environment": {},
                  "permitted_tools": [], "generic_capabilities": [], "qualification_mismatches": [],
                  "budget_scope": {}}
        outputs = {}
        with patch.object(reader, "read", side_effect=[canonical(config), b"synthetic question"]), \
             patch.object(reader, "check_config"), patch.object(reader, "copy_asset"), \
             patch.object(reader, "native_environment", return_value={}), \
             patch.object(reader.Path, "exists", return_value=False), \
             patch.object(reader, "put", side_effect=lambda path, value: outputs.update({path: value})), \
             patch("sys.argv", ["reader", "--instruction", "/synthetic/instruction.txt"]), \
             patch.object(reader, "run", side_effect=[self.native_run_result(("codex-cli " + CODEX_VERSION).encode()),
                                                      self.native_run_result(stdout, **overrides)]) as child:
            reader.main()
        argv = child.call_args.args[0]
        self.assertEqual(argv[-2:], ["--", "-"])
        self.assertNotIn("synthetic question", argv)
        self.assertTrue(child.call_args.kwargs["input_bytes"].endswith(b"synthetic question"))
        self.assertLess(child.call_args.kwargs["timeout"], config["deadline_seconds"])
        return outputs

    def judge_main(self, stdout, answer="synthetic answer", **overrides):
        config = {"binding": self.binding(), "codex_binary": "/synthetic/codex",
                  "reader_base_binary": {"path": "/synthetic/codex"}, "judge_profile": PROFILE,
                  "judge_deadline_seconds": 20, "native_config_sha256": "e" * 64,
                  "base_tool_environment": {},
                  "question": "synthetic question", "reference": "synthetic reference", "answer_format": "text",
                  "contract": {"prompt": "Q:{question}\nR:{answer}\nF:{answer_format}\nA:{response}",
                      "response_schema": {"type": "object", "required": ["equivalent"],
                                          "properties": {"equivalent": {"type": "boolean"}}}}}
        envelope = {"schema": "gptgrep.harbor.answer.v1", "binding": self.binding(),
                    "status": "response", "answer": answer}
        outputs = {}
        with patch.object(verifier, "read", side_effect=[canonical(config), canonical(envelope)]), \
             patch.object(verifier, "check_config"), patch.object(verifier, "copy_asset"), \
             patch.object(verifier, "native_environment", return_value={}), \
             patch.object(verifier.Path, "exists", return_value=False), \
             patch.object(verifier, "put", side_effect=lambda path, value: outputs.update({path: value})), \
             patch.object(verifier, "run", side_effect=[self.native_run_result(("codex-cli " + CODEX_VERSION).encode()),
                                                        self.native_run_result(stdout, **overrides)]) as child:
            verifier.main()
        if child.call_count:
            argv = child.call_args.args[0]
            self.assertEqual(argv[-2:], ["--", "-"])
            self.assertNotIn(answer, argv)
            expected = config["contract"]["prompt"].format(question=config["question"], answer=config["reference"],
                                                         answer_format=config["answer_format"], response=answer).encode()
            self.assertEqual(child.call_args.kwargs["input_bytes"], expected)
            self.assertLess(child.call_args.kwargs["timeout"], config["judge_deadline_seconds"])
        return outputs

    def test_reader_and_judge_entrypoints_retain_usage_before_answer_or_process_acceptance(self):
        cases = [(name, data, {}) for name, data in self.failed_streams().items()]
        cases.append(("failed_process", self.events(), {"return_code": 7}))
        for name, data, overrides in cases:
            with self.subTest(name=name):
                output = self.reader_main(data, **overrides)
                self.assertEqual(output["/answer/answer.json"]["status"], "operational_failure")
                raw = output["/logs/agent/operational.json"]["attempts"][0]
                output = self.judge_main(data, **overrides)
                result = output["/logs/verifier/judgment.json"]
                self.assertIsNone(result["label"])
                self.assertEqual(result["status"], "judge_unavailable")
                for attempt in (raw, result["judge_attempts"][0]):
                    self.assertEqual(attempt["native_completed_turn_usage"][0]["usage"], self.usage)
                    self.assertEqual(attempt["usage"], None if name == "duplicate_completion" else self.usage)
                    self.assertIsNone(attempt["physical_provider_requests"])
                    self.assertFalse(attempt["cost_complete"])
                    self.assertEqual(attempt["native_process_incomplete"], name == "failed_process")
                    if name != "failed_process":
                        self.assertTrue(attempt["native_stream_incomplete"])

    def test_complete_maximum_answer_uses_judge_stdin_with_independent_prompt_bound(self):
        answer = "x" * MAX_ANSWER
        output = self.judge_main(self.events('{"equivalent":true}'), answer=answer)
        self.assertEqual(output["/logs/verifier/judgment.json"]["label"], 1)
        payload = "é" * (MAX_NATIVE_PROMPT_BYTES // 2)
        self.assertEqual(len(prompt_bytes(payload)), MAX_NATIVE_PROMPT_BYTES)
        with self.assertRaisesRegex(ValueError, "complete_native_prompt_bound"):
            prompt_bytes(payload + "x")

    def test_cancelled_before_spawn_cannot_launch_child(self):
        calls = []
        def signal_change(number, handler):
            calls.append(number)
            if len(calls) == 2:
                handler(number, None)
            return lambda *_: None
        with patch("gptgrep_harbor.process.signal.signal", side_effect=signal_change), \
             patch("gptgrep_harbor.process.subprocess.Popen") as spawn:
            result = bounded_process(["/synthetic/not-executed"], cwd="/", timeout=2, env={})
        spawn.assert_not_called()
        self.assertEqual(result["failure"], "cancelled")

    def test_usage_subset_not_double_counted_and_duplicate_completion_rejected(self):
        events = [{"type": "item.completed", "item": {"type": "agent_message", "text": "synthetic"}},
                  {"type": "turn.completed", "usage": {"input_tokens": 100, "cached_input_tokens": 40,
                                                         "output_tokens": 10}}]
        data = b"\n".join(canonical(event) for event in events)
        _, info = codex_result(data)
        self.assertEqual(info["usage"]["input_tokens"], 100)
        self.assertIsNone(info["cost_usd"])
        with self.assertRaisesRegex(ValueError, "duplicate_native_completion"):
            codex_result(data + b"\n" + canonical(events[-1]))

    def test_failed_product_report_retains_known_cost_and_unknown_total(self):
        report = {"status": "failed", "error": {"jev": {"usage": [{"cost": 0.125}, {"cost": None}]}}}
        result = product_accounting(report)
        self.assertEqual(result["reported_retrieval_cost_subtotal"], 0.125)
        self.assertIsNone(result["cost_usd"])
        self.assertFalse(result["cost_complete"])


class MaterializationIsolationTests(unittest.TestCase):
    def test_raw_context_has_no_gold_or_index_and_verifier_has_reference(self):
        # A future test run must have an explicitly admitted temporary root.
        with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as temporary:
            root = Path(temporary).resolve()
            raw = root / "synthetic-source.txt"
            raw.write_bytes(b"A synthetic raw document with an arbitrary printed number.")
            index = root / "synthetic-index.bin"
            index.write_bytes(b"synthetic prebuilt index bytes")
            def pin(path):
                data = path.read_bytes()
                return {"path": str(path), "bytes": len(data), "sha256": sha(data)}
            document = {"document_id": "arbitrary-document.txt", "relative_path": "arbitrary-document.txt",
                        "pin": pin(raw)}
            runtime = {"documents": [document], "index_files": [{"relative_path": ".gptgrep/index.bin", "pin": pin(index)}],
                       "max_asset_bytes": 1048576, "images": {arm: "synthetic/image@sha256:" + "a" * 64
                                                              for arm in ("gptgrep", "raw_only", "judge")},
                       "reader_base_binary": {"path": "/opt/codex", "bytes": 1, "sha256": "b" * 64},
                       "gptgrep_binary_pin": {"path": "/opt/gptgrep", "bytes": 1, "sha256": "c" * 64},
                       "gptgrep_bundle": {"provenance": {"path": "/synthetic/product-provenance.json", "bytes": 1, "sha256": "c" * 64}},
                       "permitted_tools": {"raw_only": ["native shell"], "gptgrep": ["native shell", "GPTgrep"]},
                       "generic_capabilities": ["raw document read"], "qualification_mismatches": [],
                       "base_tool_environment": {"image": "synthetic/base@sha256:" + "f" * 64,
                           "provenance": {"path": "/synthetic/base-tools.json", "bytes": 1, "sha256": "f" * 64}},
                       "codex_binary": "/opt/codex", "gptgrep_binary": "/opt/gptgrep", "codex_home": "/opt/full62-home",
                       "native_ptc": True, "codex_permissions": "full62_reader",
                       "platform": "linux/amd64",
                       "native_cli": {"version": "0.162.1", "release_tag": "rust-v0.162.1", "source_commit": "a" * 40},
                       "auth": {"method": "chatgpt_device_file", "reader_file": "/synthetic/reader/auth.json",
                                "judge_file": "/synthetic/judge/auth.json"},
                       "decisions_budget": {"path": "/synthetic/decisions-budget.json", "bytes": 1, "sha256": "e" * 64},
                       "decisions_ledger_dir": "/synthetic/ledger",
                       "jev_model": "synthetic-model", "reader_mode": "legacy_text", "deadline_seconds": 20, "judge_deadline_seconds": 20,
                       "max_tool_calls": 8, "max_input_bytes": 8192, "judge_profile": PROFILE,
                       "resources": {"cpus": 1, "memory_mb": 512, "storage_mb": 512}}
            owner = object.__new__(Full62Adapter)
            owner.runtime = runtime
            owner.judge = {"schema": "gptgrep.harbor.primary-judge.v1", "prompt": "synthetic rubric",
                           "response_schema": {"type": "object"}}
            row = {"pair_id": "synthetic-pair", "document_id": document["document_id"],
                   "question": "Read the generic number.", "reference": "PRIVATE_SYNTHETIC_REFERENCE_ONLY",
                   "answer_format": "text"}
            binding = {"arm": "raw_only", "pair_id": row["pair_id"], "document_id": row["document_id"]}
            destination = root / "generated-raw-task"
            owner._task(destination, Path(adapter_module.__file__).parent, row, document, binding)
            self.assertFalse((destination / "environment/corpus/.gptgrep/index.bin").exists())
            self.assertEqual((destination / "environment/corpus/arbitrary-document.txt").read_bytes(), raw.read_bytes())
            reader = (destination / "environment/reader.json").read_text()
            self.assertNotIn(row["reference"], reader)
            self.assertNotIn("reference", json.loads(reader))
            self.assertEqual(json.loads((destination / "tests/judge.json").read_text())["reference"], row["reference"])
            docker = (destination / "environment/Dockerfile").read_text()
            self.assertNotIn("COPY .", docker)
            self.assertIn("environment_mode = \"separate\"", (destination / "task.toml").read_text())


if __name__ == "__main__":
    unittest.main()
