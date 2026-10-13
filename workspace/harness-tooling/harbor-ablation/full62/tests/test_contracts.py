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
from gptgrep_harbor.native import codex_accounting, codex_result, output_schema_bytes, prompt_bytes
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

    def test_summary_records_treatment_tool_cap_against_uncapped_bare_arm(self):
        export, records, runtime, blobs = synthetic_results()
        runtime["max_tool_calls"] = 10000
        scope = self.aggregate(export, records, runtime, blobs)["profile"]["budget_scope"]
        self.assertEqual((scope["treatment_max_tool_calls"], scope["raw_tool_call_cap"]), (10000, "none"))
        self.assertEqual(scope["tool_calls"], "treatment_enforced_raw_observed_only")

    def test_abstention_counts_come_from_retained_verdicts_and_operation_records(self):
        export, records, runtime, blobs = synthetic_results()
        by_arm = {arm: [record for record, row in zip(records, export["tasks"]) if row["binding"]["arm"] == arm]
                  for arm in ("gptgrep", "raw_only")}

        def judged(record, verdict):
            judgment = parse(blobs[record["judgment"]])
            judgment.update(label=int(verdict["equivalent"]), verdict=verdict,
                            verdict_sha256=sha(canonical(verdict)))
            blobs[record["judgment"]] = canonical(judgment)

        for record in by_arm["gptgrep"][:3]:
            judged(record, {"equivalent": False, "abstained": True, "reason": "synthetic"})
            operation = parse(blobs[record["operation"]])
            operation["insufficient_evidence"] = True
            blobs[record["operation"]] = canonical(operation)
        for record in by_arm["gptgrep"][3:10] + by_arm["raw_only"][:5]:
            judged(record, {"equivalent": True, "abstained": False, "reason": "synthetic"})
        judged(by_arm["raw_only"][5], {"equivalent": True, "abstained": True, "reason": "synthetic"})
        summary = self.aggregate(export, records, runtime, blobs)
        self.assertEqual(summary["search"]["gptgrep"]["intentional_abstentions"],
                         {"product_insufficient_evidence": 3, "judge_abstained": 3,
                          "judged_abstention_unobserved": 52, "answerability_slices": None})
        self.assertEqual(summary["search"]["raw_only"]["intentional_abstentions"],
                         {"product_insufficient_evidence": 0, "judge_abstained": 1,
                          "judged_abstention_unobserved": 56, "answerability_slices": None})
        self.assertTrue(any("answerability" in note for note in summary["limitations"]))
        # A retained verdict must match its digest and its label.
        for change in ({"abstained": False}, {"equivalent": True}):
            with self.subTest(change=change):
                original = blobs[by_arm["gptgrep"][0]["judgment"]]
                judgment = parse(original)
                judgment["verdict"].update(change)
                if "equivalent" in change:
                    judgment["verdict_sha256"] = sha(canonical(judgment["verdict"]))
                blobs[by_arm["gptgrep"][0]["judgment"]] = canonical(judgment)
                with self.assertRaisesRegex(ValueError, "judge_verdict_binding"):
                    self.aggregate(export, records, runtime, blobs)
                blobs[by_arm["gptgrep"][0]["judgment"]] = original

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

    single_field_schema = {"type": "object", "additionalProperties": False, "required": ["equivalent"],
                           "properties": {"equivalent": {"type": "boolean"}}}

    def judge_main(self, stdout, answer="synthetic answer", schema=None, schema_fault=None, **overrides):
        schema = self.single_field_schema if schema is None else schema
        directory = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(directory.cleanup)
        schema_file = Path(directory.name).resolve() / "judge-output-schema.json"
        data = output_schema_bytes(schema)
        schema_file.write_bytes(data)
        pin = {"path": str(schema_file), "bytes": len(data), "sha256": sha(data)}
        if schema_fault == "changed_file":
            schema_file.write_bytes(data.replace(b'"boolean"', b'"string" ', 1))
        elif schema_fault == "other_contract":
            schema = {**schema, "required": list(schema["required"]) + ["synthetic_extra"]}
        config = {"binding": self.binding(), "codex_binary": "/synthetic/codex",
                  "reader_base_binary": {"path": "/synthetic/codex"}, "judge_profile": PROFILE,
                  "judge_deadline_seconds": 20, "native_config_sha256": "e" * 64,
                  "base_tool_environment": {},
                  "question": "synthetic question", "reference": "synthetic reference", "answer_format": "text",
                  "contract": {"prompt": "Q:{question}\nR:{answer}\nF:{answer_format}\nA:{response}",
                               "response_schema": schema},
                  "output_schema": pin}
        envelope = {"schema": "gptgrep.harbor.answer.v1", "binding": self.binding(),
                    "status": "response", "answer": answer}
        outputs = {}
        with patch.object(verifier, "read", side_effect=[canonical(config), canonical(envelope)]), \
             patch.object(verifier, "JUDGE_OUTPUT_SCHEMA", str(schema_file)), \
             patch.object(verifier, "check_config"), patch.object(verifier, "copy_asset"), \
             patch.object(verifier, "native_environment", return_value={}), \
             patch.object(verifier.Path, "exists", return_value=False), \
             patch.object(verifier, "put", side_effect=lambda path, value: outputs.update({path: value})), \
             patch.object(verifier, "run", side_effect=[self.native_run_result(("codex-cli " + CODEX_VERSION).encode()),
                                                        self.native_run_result(stdout, **overrides)]) as child:
            verifier.main()
        self.judge_calls = child.call_count
        if child.call_count:
            argv = child.call_args.args[0]
            self.assertEqual(argv[-2:], ["--", "-"])
            self.assertEqual(argv[argv.index("--output-schema") + 1], str(schema_file))
            self.assertNotIn(answer, argv)
            # The frozen prompt bytes are unchanged by structured output.
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

    frozen_like_schema = {"type": "object", "additionalProperties": False,
                          "required": ["equivalent", "abstained", "reason"],
                          "properties": {"equivalent": {"type": "boolean"}, "abstained": {"type": "boolean"},
                                         "reason": {"type": "string"}}}

    def test_schema_valid_structured_judge_reply_is_parsed(self):
        verdict = {"equivalent": True, "abstained": False, "reason": "synthetic"}
        output = self.judge_main(self.events(canonical(verdict).decode()), schema=self.frozen_like_schema)
        result = output["/logs/verifier/judgment.json"]
        self.assertEqual((result["status"], result["label"]), ("judged", 1))
        self.assertTrue(result["judge_attempts"][0]["output_schema_verified"])
        self.assertEqual(output["/logs/verifier/reward.json"]["answer_equivalence"], 1)
        # The validated verdict stays reviewable beside its digest.
        self.assertEqual(result["verdict"], verdict)
        self.assertEqual(result["verdict_sha256"], sha(canonical(verdict)))

    def test_schema_file_lists_properties_in_the_frozen_required_order(self):
        # A key-sorted contract file (as the frozen-contract builder writes it)
        # keeps the upstream generation order only in `required`.
        sorted_contract = parse(canonical({**self.frozen_like_schema, "properties": {
            "equivalent": {"type": "boolean", "description": "same answer"},
            "abstained": {"type": "boolean", "description": "no answer"},
            "reason": {"type": "string", "description": "short"}}}))
        self.assertEqual(list(sorted_contract["properties"]), ["abstained", "equivalent", "reason"])
        data = output_schema_bytes(sorted_contract)
        written = json.loads(data)
        self.assertEqual(list(written["properties"]), ["equivalent", "abstained", "reason"])
        self.assertEqual(list(written), sorted(written))
        self.assertEqual(list(written["properties"]["equivalent"]), ["description", "type"])
        self.assertEqual(written, sorted_contract)
        # Deterministic: a reordered but equal contract yields the same bytes.
        self.assertEqual(output_schema_bytes(json.loads(data)), data)
        for broken in ({**sorted_contract, "required": ["equivalent", "abstained", "abstained"]},
                       {**sorted_contract, "required": ["equivalent", "abstained"]},
                       {**sorted_contract, "required": [["equivalent"], "abstained", "reason"]}):
            with self.subTest(required=broken["required"]), \
                    self.assertRaisesRegex(ValueError, "output_schema_property_order"):
                output_schema_bytes(broken)

    def test_non_json_or_schema_invalid_judge_reply_stays_unknown(self):
        for reply in ("Equivalent: yes", '{"equivalent": true', '{"equivalent": true, "abstained": false}'):
            with self.subTest(reply=reply):
                output = self.judge_main(self.events(reply), schema=self.frozen_like_schema)
                self.assertEqual(self.judge_calls, 2)
                result = output["/logs/verifier/judgment.json"]
                self.assertEqual((result["status"], result["label"]), ("judge_unavailable", None))
                self.assertNotIn("answer_equivalence", output["/logs/verifier/reward.json"])

    def test_output_schema_pin_is_verified_before_any_native_process(self):
        for fault in ("changed_file", "other_contract"):
            with self.subTest(fault=fault):
                output = self.judge_main(self.events('{"equivalent":true}'), schema_fault=fault)
                self.assertEqual(self.judge_calls, 0)
                result = output["/logs/verifier/judgment.json"]
                self.assertEqual((result["status"], result["label"]), ("judge_unavailable", None))
                self.assertFalse(result["judge_attempts"][0]["output_schema_verified"])
                self.assertNotIn("/logs/verifier/attempt-start.json", output)

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


def legacy_report(**changes):
    """A synthetic gptgrep.host.v1 ask report; no task text."""
    report = {"schema_version": "gptgrep.host.v1", "status": "completed", "answer": "A complete synthetic answer.",
              "model": PROFILE["model"], "requested_reasoning_effort": "max", "requested_service_tier": "fast",
              "effective_reasoning_effort": "max", "effective_service_tier": "priority", "citations": [],
              "jev": {"usage": [{"cost": 0.0125}]}}
    report.update(changes)
    return canonical(report)


class LegacyTreatmentReaderTests(unittest.TestCase):
    """The treatment entrypoint and primary Judge for reader_mode=legacy_text."""
    question = "synthetic question"
    schema = NativeAccountingTests.frozen_like_schema

    def binding(self):
        return {"arm": "gptgrep", "pair_id": "synthetic", "document_id": "synthetic-document",
                "cohort_sha256": "a" * 64, "runtime_sha256": "b" * 64, "judge_sha256": "c" * 64,
                "question_sha256": sha(self.question.encode()), "document_sha256": "d" * 64}

    def config(self):
        return {"binding": self.binding(), "profile": PROFILE, "codex_binary": "/synthetic/codex",
                "gptgrep_binary": "/synthetic/gptgrep", "codex_home": "/opt/full62-home",
                "reader_base_binary": {"path": "/synthetic/codex"}, "max_input_bytes": 8192, "max_tool_calls": 8,
                "native_cli": {}, "native_config_sha256": "e" * 64, "deadline_seconds": 20,
                "base_tool_environment": {}, "permitted_tools": [], "generic_capabilities": [],
                "qualification_mismatches": [], "budget_scope": {}, "reader_mode": "legacy_text",
                "jev_model": "synthetic-decisions", "native_ptc": True, "codex_permissions": "full62_reader",
                "source_relative_path": "synthetic.txt",
                "decisions_budget": {"path": "/opt/full62-private/decisions-budget.json", "bytes": 1,
                                     "sha256": "f" * 64}}

    def run_reader(self, stdout, environ=None, config=None, claim_root="/synthetic/unmounted-claim"):
        outputs = {}
        version = {"stdout": ("codex-cli " + CODEX_VERSION).encode(), "stderr": b"", "elapsed_ms": 1,
                   "return_code": 0, "failure": None, "stdin_bytes": 0, "stdin_bytes_sent": 0, "stdin_complete": True}
        product = {**version, "stdout": stdout, "elapsed_ms": 10}
        budget = canonical({"ledger_path": "/opt/full62-private/ledger/decisions.json"})
        config = self.config() if config is None else config
        with patch("gptgrep_harbor.decisions.CLAIM_ROOT", str(claim_root)), \
             patch.object(reader, "read", side_effect=[canonical(config), self.question.encode()]), \
             patch.object(reader, "check_config"), patch.object(reader, "copy_asset"), \
             patch.object(reader, "check_product_bundle"), \
             patch.object(reader, "pinned", return_value=budget) as budget_read, \
             patch.object(reader.Path, "exists", return_value=False), \
             patch.object(reader, "put", side_effect=lambda path, value: outputs.update({path: value})), \
             patch.dict(os.environ, environ or {"PATH": "/usr/bin"}, clear=True), \
             patch("sys.argv", ["reader", "--instruction", "/synthetic/instruction.txt"]), \
             patch.object(reader, "run", side_effect=[version, product]) as child:
            reader.main()
        self.budget_reads = budget_read.call_count
        return outputs, child

    def judge_config(self):
        return {"binding": self.binding(), "question": self.question, "reference": "synthetic reference",
                "answer_format": "text",
                "contract": {"prompt": "Q:{question}\nR:{answer}\nF:{answer_format}\nA:{response}",
                             "response_schema": self.schema}}

    def test_product_result_accepts_only_completed_answers_and_nonempty_abstentions(self):
        abstention = "The supplied document does not state this."
        answer, metadata = reader.product_result(legacy_report(status="insufficient_evidence", answer=abstention),
                                                 "legacy_text")
        self.assertEqual(answer, abstention)
        self.assertEqual((metadata["product_status"], metadata["insufficient_evidence"]),
                         ("insufficient_evidence", True))
        _, completed = reader.product_result(legacy_report(), "legacy_text")
        self.assertEqual((completed["product_status"], completed["insufficient_evidence"]), ("completed", False))
        for status in ("failed", "interrupted", "partial", "", None):
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "product_not_completed"):
                reader.product_result(legacy_report(status=status), "legacy_text")
        for empty in ("", "  \n", None):
            with self.subTest(answer=empty), self.assertRaisesRegex(ValueError, "product_answer"):
                reader.product_result(legacy_report(status="insufficient_evidence", answer=empty), "legacy_text")

    def test_legacy_intentional_abstention_is_judged_and_recorded(self):
        abstention = "The supplied document does not state this; no answer is given."
        outputs, _ = self.run_reader(legacy_report(status="insufficient_evidence", answer=abstention))
        envelope = outputs["/answer/answer.json"]
        self.assertEqual((envelope["status"], envelope["answer"]), ("response", abstention))
        operation = outputs["/logs/agent/operational.json"]
        self.assertEqual(operation["status"], "response")
        self.assertIs(operation["insufficient_evidence"], True)
        self.assertIs(operation["attempts"][0]["insufficient_evidence"], True)
        self.assertNotIn("failure", operation)
        prompts = []
        def judge(prompt, schema):
            prompts.append(prompt)
            return {"equivalent": False, "abstained": True, "reason": "synthetic"}, {}
        result = grade(self.judge_config(), envelope, judge)
        self.assertEqual((result["status"], result["label"]), ("judged", 0))
        self.assertTrue(prompts[0].endswith(abstention))
        completed, _ = self.run_reader(legacy_report())
        self.assertIs(completed["/logs/agent/operational.json"]["insufficient_evidence"], False)

    def test_treatment_product_keeps_openrouter_key_while_reader_tools_have_no_network(self):
        import tomllib
        from gptgrep_harbor.runtime import model_config
        outputs, child = self.run_reader(legacy_report(),
                                         environ={"PATH": "/usr/bin", "OPENROUTER_API_KEY": "synthetic-only"})
        version_call, product_call = child.call_args_list
        # Decisions requests come from the GPTgrep host process, outside the
        # Codex tool sandbox whose network the shared reader policy disables.
        self.assertEqual(product_call.args[0][0], "/synthetic/gptgrep")
        self.assertEqual(product_call.kwargs["env"]["OPENROUTER_API_KEY"], "synthetic-only")
        self.assertNotIn("OPENROUTER_API_KEY", version_call.kwargs["env"])
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")
        policy = tomllib.loads(model_config(PROFILE, "reader").decode())
        self.assertEqual(policy["web_search"], "disabled")
        self.assertIs(policy["permissions"]["full62_reader"]["network"]["enabled"], False)

    def test_existing_context_ask_still_joins_the_mounted_budget(self):
        outputs, child = self.run_reader(legacy_report())
        self.assertEqual(self.budget_reads, 1)
        self.assertEqual(child.call_args_list[1].args[0][1:4],
                         ["--decisions-budget", "/opt/full62-private/decisions-budget.json", "ask"])
        self.assertEqual(outputs["/logs/agent/operational.json"]["decisions_mode"], "existing_context")

    def claim_root(self):
        directory = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(directory.cleanup)
        return Path(directory.name).resolve()

    def test_invocation_bounded_ask_claims_once_and_keeps_its_cost_receipt(self):
        config = {**self.config(), "decisions_mode": "invocation_bounded"}
        del config["decisions_budget"]
        jev = {"requests": 2, "attempted_calls": 2, "unobserved_attempts": 0, "accounting_complete": True,
               "usage": [{"cost": 0.0125}, {"cost": 0.0075}]}
        for status, expected in (("completed", "response"), ("failed", "operational_failure")):
            with self.subTest(status=status):
                report = legacy_report(status=status, jev=jev)
                if status == "failed":
                    # A failed Ask reports its Decisions accounting under error.
                    report = canonical({"status": "failed", "error": {"jev": jev}})
                claim_root = self.claim_root()
                outputs, child = self.run_reader(report, config=config, claim_root=claim_root)
                self.assertEqual(self.budget_reads, 0)
                argv = child.call_args_list[1].args[0]
                self.assertEqual(argv[:2], ["/synthetic/gptgrep", "ask"])
                self.assertNotIn("--decisions-budget", argv)
                self.assertEqual(outputs["/answer/answer.json"]["status"], expected)
                operation = outputs["/logs/agent/operational.json"]
                self.assertEqual(operation["decisions_mode"], "invocation_bounded")
                receipt = operation["attempts"][0]["decisions_cost"]
                self.assertTrue(receipt["cost_complete"])
                self.assertAlmostEqual(receipt["reported_cost_usd"], 0.02)
                self.assertEqual((receipt["validated_requests"], receipt["priced_rows"]), (2, 2))
                # The create-new claim and the host copy of the receipt live in the
                # trial's own host directory, outside the Harbor job directory.
                claim = (claim_root / "claim.json").read_bytes()
                self.assertEqual(json.loads(claim)["binding_sha256"], sha(canonical(self.binding())))
                self.assertEqual(operation["decisions_claim"]["sha256"], sha(claim))
                host = (claim_root / "decisions-cost.json").read_bytes()
                self.assertEqual(json.loads(host)["decisions_cost"], receipt)
                self.assertEqual(json.loads(host)["product_report_sha256"], sha(report))
                self.assertEqual(operation["decisions_receipt"]["sha256"], sha(host))
                # A re-executed trial (for example in a resumed Harbor job) finds
                # the claim and fails closed before the Ask: no Decisions spend.
                again, child = self.run_reader(report, config=config, claim_root=claim_root)
                self.assertEqual(child.call_count, 1)
                self.assertEqual(again["/answer/answer.json"]["status"], "operational_failure")
                repeated = again["/logs/agent/operational.json"]
                self.assertEqual((repeated["failure"], repeated["attempts"]), ("reader_failed", []))
                self.assertNotIn("decisions_receipt", repeated)
                self.assertEqual((claim_root / "claim.json").read_bytes(), claim)

    def test_invocation_bounded_ask_requires_its_claim_mount(self):
        config = {**self.config(), "decisions_mode": "invocation_bounded"}
        del config["decisions_budget"]
        missing = self.claim_root() / "not-mounted"
        outputs, child = self.run_reader(legacy_report(), config=config, claim_root=missing)
        self.assertEqual(child.call_count, 1)
        self.assertEqual(outputs["/answer/answer.json"]["status"], "operational_failure")
        self.assertFalse(missing.exists())

    def test_bare_prompt_states_the_enforced_no_external_search_policy(self):
        prompt = reader.raw_prompt({"raw_document": "/data/corpus/synthetic.txt"}, self.question).decode()
        self.assertIn("No external search or network access is available.", prompt)
        self.assertNotIn("Do not use external search", prompt)
        self.assertTrue(prompt.endswith("Question:\n" + self.question))

    def test_other_legacy_statuses_are_zero_and_never_judged(self):
        for status, answer in (("failed", "partial synthetic text"), ("interrupted", "partial synthetic text"),
                               ("insufficient_evidence", "   ")):
            with self.subTest(status=status, answer=answer):
                outputs, _ = self.run_reader(legacy_report(status=status, answer=answer))
                envelope = outputs["/answer/answer.json"]
                self.assertEqual((envelope["status"], envelope["answer"]), ("operational_failure", None))
                operation = outputs["/logs/agent/operational.json"]
                self.assertEqual(operation["failure"], "reader_failed")
                self.assertNotIn("insufficient_evidence", operation)
                result = grade(self.judge_config(), envelope, lambda *_: self.fail("Judge called"))
                self.assertEqual((result["status"], result["label"]), ("operational_failure", 0))
                self.assertEqual(transport_reward(result)["answer_equivalence"], 0)


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
                       "platform": "linux/amd64", "run_id": "synthetic-full62",
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
                           "response_schema": {"type": "object", "additionalProperties": False,
                                               "required": ["equivalent"],
                                               "properties": {"equivalent": {"type": "boolean"}}}}
            row = {"pair_id": "synthetic-pair", "document_id": document["document_id"],
                   "question": "Read the generic number.", "reference": "PRIVATE_SYNTHETIC_REFERENCE_ONLY",
                   "answer_format": "text"}
            binding = {"arm": "raw_only", "pair_id": row["pair_id"], "document_id": row["document_id"]}
            destination = root / "generated-raw-task"
            owner._task(destination, Path(adapter_module.__file__).parent, row, document, binding)
            self.assertFalse((destination / "environment/corpus/.gptgrep/index.bin").exists())
            for directory in ("environment", "tests"):
                compose = json.loads((destination / directory / "docker-compose.yaml").read_text())
                self.assertEqual(compose["services"]["main"]["labels"], {"gptgrep.auth-job": "synthetic-full62"})
            self.assertEqual((destination / "environment/corpus/arbitrary-document.txt").read_bytes(), raw.read_bytes())
            reader = (destination / "environment/reader.json").read_text()
            self.assertNotIn(row["reference"], reader)
            self.assertNotIn("reference", json.loads(reader))
            self.assertEqual(json.loads((destination / "tests/judge.json").read_text())["reference"], row["reference"])
            docker = (destination / "environment/Dockerfile").read_text()
            self.assertNotIn("COPY .", docker)
            self.assertIn("environment_mode = \"separate\"", (destination / "task.toml").read_text())


class NativeDirectToolPolicyTests(unittest.TestCase):
    def test_reader_native_capabilities_preserve_named_policy_and_profile(self):
        import tomllib
        from gptgrep_harbor.runtime import model_config
        config = tomllib.loads(model_config(PROFILE, "reader").decode())
        # Native code, shell, image and sub-agent tools stay on; host-side
        # connector, plugin and tool-suggestion features are off for both arms.
        self.assertEqual(config["features"], {
            "shell_tool": True, "view_image": True, "unified_exec": True,
            "multi_agent": True, "plugins": False, "apps": False,
            "tool_suggest": False, "code_mode": {"enabled": True}, "code_mode_host": {"enabled": True}})
        self.assertEqual(config["web_search"], "disabled")
        self.assertEqual(config["default_permissions"], "full62_reader")
        self.assertEqual(config["model"], PROFILE["model"])
        self.assertEqual(config["model_reasoning_effort"], PROFILE["reasoning_effort"])
        self.assertEqual(config["service_tier"], PROFILE["service_tier"])
        policy = config["permissions"]["full62_reader"]
        self.assertFalse(policy["network"]["enabled"])
        self.assertEqual(policy["filesystem"]["/data/corpus"], "read")
        self.assertEqual(policy["filesystem"]["/tmp/full62-work"], "write")
        self.assertEqual(policy["filesystem"]["/opt/full62-home"], "deny")
        self.assertEqual(policy["filesystem"]["/tests"], "deny")
        self.assertEqual(policy["filesystem"]["/logs"], "deny")

    def test_judge_keeps_empty_direct_namespace_and_native_tools_disabled(self):
        import tomllib
        from gptgrep_harbor.runtime import model_config
        config = tomllib.loads(model_config(PROFILE, "judge").decode())
        self.assertEqual(config["features"]["code_mode"],
                         {"enabled": False, "direct_only_tool_namespaces": []})
        self.assertFalse(config["features"]["shell_tool"])
        self.assertFalse(config["features"]["view_image"])
        # Codex enables these by default; the Judge disables them explicitly.
        for feature in ("apps", "plugins", "tool_suggest"):
            self.assertIs(config["features"][feature], False)
        self.assertEqual(config["default_permissions"], "full62_judge")
        self.assertEqual(config["web_search"], "disabled")
        self.assertFalse(config["permissions"]["full62_judge"]["network"]["enabled"])
        self.assertEqual(config["permissions"]["full62_judge"]["filesystem"]["/data"], "deny")
        self.assertNotIn("/tmp/full62-work",
                         config["permissions"]["full62_judge"]["filesystem"])


if __name__ == "__main__":
    unittest.main()
