"""Synthetic regressions; all temporary files use the selected external root."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from gptgrep_harbor.adapter import Full62Adapter, validate_cohort, env_templates
import gptgrep_harbor.adapter as adapter_module
from gptgrep_harbor.aggregate import paired, summarize, time_bounds
from gptgrep_harbor.common import MAX_ANSWER, MAX_NATIVE_PROMPT_BYTES, PROFILE, canonical, parse, sha
from gptgrep_harbor.native import codex_accounting, codex_result, output_schema_bytes, prompt_bytes
from gptgrep_harbor.reader import product_accounting
from gptgrep_harbor import decisions as decisions_module, reader, verifier
from gptgrep_harbor.runtime import CODEX_VERSION, READER_SECCOMP_BYTES, READER_SECCOMP_SHA256
from gptgrep_harbor.verifier import grade, transport_reward
from gptgrep_harbor.process import run as bounded_process


def synthetic_cohort():
    return {"schema": "gptgrep.harbor.cohort.v1", "source_cohort_sha256": "a" * 64,
            "tasks": [{"pair_id": f"synthetic-{i}", "document_id": f"generated-{i % 34}.txt",
                       "question": f"What number is printed in synthetic record {i}?",
                       "reference": str(i * 17), "answer_format": "integer"} for i in range(62)]}


# Generic markers of GPTgrep's reader contract (answer rules, citation and
# abstention schema). None may reach the raw-only prompt in any reader mode.
GPTGREP_RULE_MARKERS = ("Answer rules", "answer rules", "Lead with the direct answer", "commit to",
                        "in-text references", "footnote", "hedging", "insufficient_evidence",
                        "citation", "GPTgrep", "gptgrep")
RAW_READER_CONTRACT = {"schema": "gptgrep.harbor.reader-contract.v1", "arm": "raw_only",
                       "base_instructions": "codex_default", "prompt": "common_task_prompt",
                       "gptgrep_contract_received": False, "answer_rules_received": False,
                       "answer_rules_description_sha256": None}


def assert_no_gptgrep_rule_text(case, prompt):
    for marker in GPTGREP_RULE_MARKERS:
        case.assertNotIn(marker, prompt)


def synthetic_results():
    rows, inputs, blobs = [], [], {}
    runtime = {"deadline_seconds": 20, "bootstrap_seed": 19, "profile": PROFILE,
               "primary_metric": "answer_equivalence_delta", "reader_mode": "legacy_text",
               "jev_model": "synthetic-decisions", "max_tool_calls": 10000, "qualification_mismatches": []}
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
        self.assertEqual(scope["treatment_tool_cap_parity"], "adapter_maximum")
        runtime["max_tool_calls"] = 500
        scope = self.aggregate(export, records, runtime, blobs)["profile"]["budget_scope"]
        self.assertEqual(scope["treatment_tool_cap_parity"], "declared_deviation")

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

    def test_summary_keeps_the_declared_reader_contract_out_of_qualification_mismatches(self):
        export, records, runtime, blobs = synthetic_results()
        generic, declared = {"code": "observed generic mismatch"}, {"code": "reader_contract_differs"}
        runtime["qualification_mismatches"] = [generic, declared]
        profile = self.aggregate(export, records, runtime, blobs)["profile"]
        self.assertEqual(profile["qualification_mismatches"], [generic])
        self.assertEqual(profile["declared_treatment_differences"], [declared])
        contract = profile["treatment_feature_profile"]["reader_contract"]
        self.assertIs(contract["declared_treatment"], True)
        self.assertIn("answer_rules", contract["components"])
        self.assertIs(contract["baseline_receives_gptgrep_rules"], False)
        # The export's frozen profile must be the one the admitted runtime declares.
        export["preflight"] = {"treatment_feature_profile": profile["treatment_feature_profile"]}
        self.aggregate(export, records, runtime, blobs)
        export["preflight"]["treatment_feature_profile"] = {**profile["treatment_feature_profile"],
                                                            "reader_mode": "rich_multimodal"}
        with self.assertRaisesRegex(ValueError, "treatment_profile_join"):
            self.aggregate(export, records, runtime, blobs)

    def test_only_the_exact_reader_contract_entry_once_is_a_declared_difference(self):
        exact = {"code": "reader_contract_differs"}
        shaped = {"code": "reader_contract_differs", "arm": "raw_only", "observation": "synthetic bare deviation"}
        generic = {"code": "observed generic mismatch"}
        listed = {"code": ["reader_contract_differs"]}
        self.assertEqual(adapter_module.qualification([exact, shaped, generic, exact, "reader_contract_differs", listed]),
                         {"mismatches": [shaped, generic, exact, "reader_contract_differs", listed],
                          "declared_treatment_differences": [exact]})
        self.assertEqual(adapter_module.qualification([shaped, exact]),
                         {"mismatches": [shaped], "declared_treatment_differences": [exact]})
        # Nothing but that one entry drops out of the summary's mismatches.
        export, records, runtime, blobs = synthetic_results()
        runtime["qualification_mismatches"] = [shaped, exact, exact]
        profile = self.aggregate(export, records, runtime, blobs)["profile"]
        self.assertEqual((profile["qualification_mismatches"], profile["declared_treatment_differences"]),
                         ([shaped, exact], [exact]))

    def test_summary_checks_each_arm_reader_contract_against_the_declaration(self):
        export, records, runtime, blobs = synthetic_results()
        unobserved = self.aggregate(export, records, runtime, blobs)["reader_contract"]
        self.assertEqual((unobserved["raw_only_records_unobserved"], unobserved["treatment_records_without_identity"],
                          unobserved["treatment_records_without_answer_rules_description"],
                          unobserved["treatment_product_provenance_sha256"]), (62, 62, 62, []))
        treatment = {"schema": "gptgrep.harbor.reader-contract.v1", "arm": "gptgrep",
                     "base_instructions": "gptgrep_reader_contract", "reader_mode": "legacy_text",
                     "answer_rules_received": True, "answer_rules_description_sha256": "1" * 64,
                     "identity": "product_provenance_sha256", "product_provenance_sha256": "9" * 64}

        def stamp(index, arm, contract):
            record = [r for r, row in zip(records, export["tasks"]) if row["binding"]["arm"] == arm][index]
            operation = parse(blobs[record["operation"]])
            operation["reader_contract"] = contract
            blobs[record["operation"]] = canonical(operation)
        for index in range(62):
            stamp(index, "gptgrep", treatment)
            stamp(index, "raw_only", RAW_READER_CONTRACT)
        result = self.aggregate(export, records, runtime, blobs)
        self.assertEqual(result["reader_contract"],
                         {"declared_treatment": True, "treatment_identity": "product_provenance_sha256",
                          "treatment_product_provenance_sha256": ["9" * 64],
                          "treatment_identity_consistent": True, "treatment_records_without_identity": 0,
                          "treatment_answer_rules_expected": True,
                          "treatment_answer_rules_description_sha256": ["1" * 64],
                          "treatment_records_without_answer_rules_description": 0,
                          "raw_only_records_without_gptgrep_contract": 62,
                          "raw_only_records_reporting_gptgrep_contract": 0,
                          "raw_only_records_unobserved": 0})
        self.assertFalse(any("product provenance" in note for note in result["limitations"]))
        # A raw-only record that reports the contract is visible.
        stamp(0, "raw_only", {**RAW_READER_CONTRACT, "gptgrep_contract_received": True})
        summary = self.aggregate(export, records, runtime, blobs)["reader_contract"]
        self.assertEqual((summary["raw_only_records_reporting_gptgrep_contract"],
                          summary["raw_only_records_without_gptgrep_contract"]), (1, 61))
        # rich_multimodal treatment trials have no answer-rules description by design,
        # but every treatment record still carries its build identity.
        stamp(0, "gptgrep", {**treatment, "reader_mode": "rich_multimodal", "answer_rules_received": False,
                             "answer_rules_description_sha256": None})
        runtime["reader_mode"] = "rich_multimodal"
        summary = self.aggregate(export, records, runtime, blobs)["reader_contract"]
        self.assertEqual((summary["treatment_records_without_identity"],
                          summary["treatment_records_without_answer_rules_description"]), (0, 0))

    def test_summary_binds_the_treatment_identity_to_the_build_not_the_rules_description(self):
        # The product's --schema description of its answer rules is written separately
        # from the rule text the reader receives: an unchanged description digest cannot
        # show that two builds gave the reader the same rules. Only the bundle
        # provenance digest binds the build.
        export, records, runtime, blobs = synthetic_results()
        treatment = {"schema": "gptgrep.harbor.reader-contract.v1", "arm": "gptgrep",
                     "base_instructions": "gptgrep_reader_contract", "reader_mode": "legacy_text",
                     "answer_rules_received": True, "answer_rules_description_sha256": "1" * 64,
                     "identity": "product_provenance_sha256"}
        for index, (record, row) in enumerate((r, row) for r, row in zip(records, export["tasks"])
                                              if row["binding"]["arm"] == "gptgrep"):
            operation = parse(blobs[record["operation"]])
            operation["reader_contract"] = {**treatment, "product_provenance_sha256": ("8" if index else "9") * 64}
            blobs[record["operation"]] = canonical(operation)
        result = self.aggregate(export, records, runtime, blobs)
        contract = result["reader_contract"]
        self.assertEqual(contract["treatment_answer_rules_description_sha256"], ["1" * 64])
        self.assertEqual(contract["treatment_product_provenance_sha256"], ["8" * 64, "9" * 64])
        self.assertIs(contract["treatment_identity_consistent"], False)
        self.assertNotIn("treatment_answer_rules_sha256", contract)
        self.assertEqual([note for note in result["limitations"] if "product provenance" in note],
                         ["2 distinct product provenance digests among the treatment records: the treatment trials "
                          "did not all run one product build, so their reader contracts may differ."])

    def test_summary_flags_pairs_with_a_pre_existing_trial_claim(self):
        export, records, runtime, blobs = synthetic_results()
        self.assertIsNone(self.aggregate(export, records, runtime, blobs)["trial_claims"])
        runtime.update(decisions_mode="invocation_bounded", decisions_max_submissions_per_trial=8)
        claim = {"path": "/opt/full62-private/claim/claim.json", "bytes": 1, "sha256": "3" * 64}
        for index, (record, row) in enumerate(zip(records, export["tasks"])):
            operation = parse(blobs[record["operation"]])
            if index == 1:
                # A resumed job re-executed this raw-only trial: its claim was present.
                answer = parse(blobs[record["answer"]])
                answer.update(status="operational_failure", answer=None)
                blobs[record["answer"]] = canonical(answer)
                operation.update(status="operational_failure", answer_sha256=sha(canonical(answer)),
                                 failure="reader_failed", failure_code="trial_already_claimed")
                judgment = parse(blobs[record["judgment"]])
                judgment.update(status="operational_failure", label=0, answer_envelope_sha256=sha(canonical(answer)))
                blobs[record["judgment"]] = canonical(judgment)
            else:
                operation["trial_claim"] = claim
            blobs[record["operation"]] = canonical(operation)
        self.assertEqual(export["tasks"][1]["binding"]["arm"], "raw_only")
        summary = self.aggregate(export, records, runtime, blobs)
        self.assertEqual(summary["trial_claims"],
                         {"claimed_arms": ["gptgrep", "raw_only"], "pairs_with_pre_existing_claim": 1,
                          "arms": {"gptgrep": {"claimed": 62, "pre_existing_claim": 0, "unclaimed": 0},
                                   "raw_only": {"claimed": 61, "pre_existing_claim": 1, "unclaimed": 0}}})
        self.assertEqual(summary["search"]["raw_only"]["operational_failure_codes"], {"trial_already_claimed": 1})
        self.assertTrue(any("found its claim already present" in note for note in summary["limitations"]))

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
                  "budget_scope": {}, "reader_mode": "legacy_text"}
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
        # Even in a legacy_text run, the raw-only arm receives only the common
        # task prompt: none of GPTgrep's reader contract, and it says so.
        self.assertEqual(child.call_args.kwargs["input_bytes"], reader.raw_prompt(config, "synthetic question"))
        assert_no_gptgrep_rule_text(self, child.call_args.kwargs["input_bytes"].decode())
        self.assertEqual(outputs["/logs/agent/operational.json"]["reader_contract"], RAW_READER_CONTRACT)
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

    def test_raw_only_trial_claims_its_one_reader_process_in_invocation_bounded_mode(self):
        directory = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(directory.cleanup)
        claim_root = Path(directory.name).resolve()
        config = {"binding": self.binding(), "profile": PROFILE, "codex_binary": "/synthetic/codex",
                  "gptgrep_binary": "/synthetic/gptgrep", "raw_document": "/data/corpus/synthetic.txt",
                  "reader_base_binary": {"path": "/synthetic/codex"}, "max_input_bytes": 8192,
                  "native_cli": {}, "native_config_sha256": "e" * 64, "deadline_seconds": 20,
                  "base_tool_environment": {}, "permitted_tools": [], "generic_capabilities": [],
                  "qualification_mismatches": [], "budget_scope": {}, "reader_mode": "legacy_text",
                  "decisions_mode": "invocation_bounded"}

        def execute():
            outputs = {}
            with patch("gptgrep_harbor.decisions.CLAIM_ROOT", str(claim_root)), \
                 patch.object(reader, "read", side_effect=[canonical(config), b"synthetic question"]), \
                 patch.object(reader, "check_config"), patch.object(reader, "copy_asset"), \
                 patch.object(reader, "native_environment", return_value={}), \
                 patch.object(reader.Path, "exists", return_value=False), \
                 patch.object(reader, "put", side_effect=lambda path, value: outputs.update({path: value})), \
                 patch("sys.argv", ["reader", "--instruction", "/synthetic/instruction.txt"]), \
                 patch.object(reader, "run", side_effect=[
                     self.native_run_result(("codex-cli " + CODEX_VERSION).encode()),
                     self.native_run_result(self.events())]) as child:
                reader.main()
            return outputs, child
        outputs, child = execute()
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")
        claim = (claim_root / "claim.json").read_bytes()
        self.assertEqual(json.loads(claim)["binding_sha256"], sha(canonical(self.binding())))
        self.assertEqual(outputs["/logs/agent/operational.json"]["trial_claim"]["sha256"], sha(claim))
        self.assertEqual(child.call_count, 2)
        # A re-executed bare trial fails closed before Codex starts, like the treatment.
        again, child = execute()
        self.assertEqual(child.call_count, 1)
        self.assertEqual(again["/answer/answer.json"]["status"], "operational_failure")
        self.assertEqual(again["/logs/agent/operational.json"]["failure_code"], "trial_already_claimed")
        self.assertEqual((claim_root / "claim.json").read_bytes(), claim)

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


SYNTHETIC_ANSWER_RULES = "Synthetic answer rules that the pinned product reports for its text reader."


def product_schema_report(rules=SYNTHETIC_ANSWER_RULES, request_bytes=65536, max_submissions=200):
    """A synthetic `gptgrep --schema` report carrying only the fields the reader checks."""
    budget = {"init_options": {"max-submissions": {"type": "integer", "minimum": 1, "maximum": max_submissions}}}
    return canonical({"schema_version": "gptgrep.cli.v1",
                      "commands": {"ask": {"reader_answer_rules": rules}, "decisions-budget": budget},
                      "decisions": {"limits": {"text_envelope_bytes": request_bytes}}})


class FakeTrialLedger:
    """Synthetic stand-in for the product's offline `decisions-budget init` and `status`; nothing is executed."""

    def __init__(self, remaining=6, init=None, status=None):
        self.remaining, self.init_changes, self.status_changes = remaining, init or {}, status
        self.control = None

    def __call__(self, argv, result):
        options = dict(zip(argv[3::2], argv[4::2]))
        if argv[2] == "init":
            control = {"schema_version": "gptgrep.decisions-budget-contract.v1", "run_id": options["--run-id"],
                       "plan_digest": sha(Path(options["--plan-file"]).read_bytes()),
                       "deadline_unix_ms": 1000 * (10**9 + int(options["--deadline-seconds"])),
                       "max_submissions": int(options["--max-submissions"]), "ledger_path": options["--ledger"]}
            data = canonical(control)
            Path(options["--output"]).write_bytes(data)
            self.control = control
            report = {"schema_version": "gptgrep.decisions-budget-init.v1", "status": "initialized",
                      "control_schema": control["schema_version"],
                      **{key: control[key] for key in ("run_id", "plan_digest", "deadline_unix_ms",
                                                         "max_submissions", "ledger_path")},
                      "max_concurrent_requests": 4, "control_path": options["--output"],
                      "control_sha256": sha(data), "control_mode": "0600", "joins_existing": False}
            report.update(self.init_changes)
            return {**result, "stdout": canonical(report)}
        if self.status_changes == "unavailable":
            return {**result, "return_code": 2, "stdout": b"{}"}
        limit = self.control["max_submissions"]
        used = limit - self.remaining
        status = {"schema_version": "gptgrep.decisions-budget-status.v1", "run_id": self.control["run_id"],
                  "plan_digest": self.control["plan_digest"], "ledger_path": self.control["ledger_path"],
                  "max_submissions": limit, "max_concurrent_requests": 4,
                  "reservations": {"total": used, "reserved": 0, "in_flight": 0, "unknown": 0, "terminal": used},
                  "send_started": used, "submission_observations": used, "remaining_submissions": self.remaining,
                  "occupied_slots": 0, "requested_models": {}, "deadline_unix_ms": self.control["deadline_unix_ms"],
                  "deadline_state": "open", "wall_remaining_ms": 1, "anchor_remaining_ms": 1,
                  "admission_open": self.remaining > 0, "read_only": True}
        return {**result, "stdout": canonical(status)}


def ask_calls(child):
    """The product Ask launches among a mocked reader's child processes."""
    return [call for call in child.call_args_list if "ask" in call.args[0]]


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
                "gptgrep_bundle": {"provenance": {"path": "/synthetic/provenance.json", "bytes": 1,
                                                  "sha256": "9" * 64}},
                "reader_base_binary": {"path": "/synthetic/codex"}, "max_input_bytes": 8192, "max_tool_calls": 8,
                "native_cli": {}, "native_config_sha256": "e" * 64, "deadline_seconds": 20,
                "base_tool_environment": {}, "permitted_tools": [], "generic_capabilities": [],
                "qualification_mismatches": [], "budget_scope": {}, "reader_mode": "legacy_text",
                "jev_model": "synthetic-decisions", "native_ptc": True, "codex_permissions": "full62_reader",
                "source_relative_path": "synthetic.txt",
                "decisions_budget": {"path": "/opt/full62-private/decisions-budget.json", "bytes": 1,
                                     "sha256": "f" * 64}}

    def run_reader(self, stdout, environ=None, config=None, claim_root="/synthetic/unmounted-claim",
                   schema=None, ledger=None):
        outputs = {}
        version = {"stdout": ("codex-cli " + CODEX_VERSION).encode(), "stderr": b"", "elapsed_ms": 1,
                   "return_code": 0, "failure": None, "stdin_bytes": 0, "stdin_bytes_sent": 0, "stdin_complete": True}
        product = {**version, "stdout": stdout, "elapsed_ms": 10}
        report = {**version, "stdout": product_schema_report(), **(schema or {})}
        budget = canonical({"ledger_path": "/opt/full62-private/ledger/decisions.json"})
        config = self.config() if config is None else config
        self.ledger = FakeTrialLedger() if ledger is None else ledger

        def child(argv, **_kwargs):
            # The pinned product's offline contract report, the native version
            # check, the trial ledger commands, then the one Ask.
            if argv[1:] == ["--schema"]:
                return report
            if argv[1] == "decisions-budget":
                return self.ledger(argv, version)
            return version if argv[1:] == ["--version"] else product
        process = Mock(side_effect=child)
        with patch("gptgrep_harbor.decisions.CLAIM_ROOT", str(claim_root)), \
             patch.object(reader, "read", side_effect=[canonical(config), self.question.encode()]), \
             patch.object(reader, "check_config"), patch.object(reader, "copy_asset"), \
             patch.object(reader, "check_product_bundle"), \
             patch.object(reader, "pinned", return_value=budget) as budget_read, \
             patch.object(reader.Path, "exists", return_value=False), \
             patch.object(reader, "put", side_effect=lambda path, value: outputs.update({path: value})), \
             patch.dict(os.environ, environ or {"PATH": "/usr/bin"}, clear=True), \
             patch("sys.argv", ["reader", "--instruction", "/synthetic/instruction.txt"]), \
             patch.object(reader, "run", process), patch("gptgrep_harbor.decisions.run", process):
            reader.main()
        self.budget_reads = budget_read.call_count
        return outputs, process

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
        schema_call, version_call, product_call = child.call_args_list
        # Decisions requests come from the GPTgrep host process, outside the
        # Codex tool sandbox whose network the shared reader policy disables.
        self.assertEqual(product_call, ask_calls(child)[0])
        self.assertEqual(product_call.args[0][0], "/synthetic/gptgrep")
        self.assertEqual(product_call.kwargs["env"]["OPENROUTER_API_KEY"], "synthetic-only")
        self.assertNotIn("OPENROUTER_API_KEY", version_call.kwargs["env"])
        # The offline contract report needs no credential.
        self.assertEqual(schema_call.args[0], ["/synthetic/gptgrep", "--schema"])
        self.assertNotIn("OPENROUTER_API_KEY", schema_call.kwargs["env"])
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")
        policy = tomllib.loads(model_config(PROFILE, "reader").decode())
        self.assertEqual(policy["web_search"], "disabled")
        self.assertIs(policy["permissions"]["full62_reader"]["network"]["enabled"], False)

    def test_existing_context_ask_still_joins_the_mounted_budget(self):
        outputs, child = self.run_reader(legacy_report())
        self.assertEqual(self.budget_reads, 1)
        self.assertEqual(ask_calls(child)[0].args[0][1:4],
                         ["--decisions-budget", "/opt/full62-private/decisions-budget.json", "ask"])
        self.assertEqual(outputs["/logs/agent/operational.json"]["decisions_mode"], "existing_context")

    def claim_root(self):
        directory = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(directory.cleanup)
        return Path(directory.name).resolve()

    def invocation_config(self):
        config = {**self.config(), "decisions_mode": "invocation_bounded", "decisions_max_submissions_per_trial": 8}
        del config["decisions_budget"]
        return config

    def ledger_calls(self, child, command):
        return [call for call in child.call_args_list if call.args[0][1:3] == ["decisions-budget", command]]

    def test_invocation_bounded_ask_joins_its_own_product_ledger_with_the_trial_limit(self):
        claim_root = self.claim_root()
        outputs, child = self.run_reader(legacy_report(), config=self.invocation_config(), claim_root=claim_root,
                                         environ={"PATH": "/usr/bin", "OPENROUTER_API_KEY": "synthetic-only"})
        control = claim_root / "decisions-budget.json"
        (init,), (status,) = self.ledger_calls(child, "init"), self.ledger_calls(child, "status")
        options = dict(zip(init.args[0][3::2], init.args[0][4::2]))
        self.assertEqual((options["--max-submissions"], options["--plan-file"], options["--ledger"],
                          options["--output"]),
                         ("8", str(claim_root / "claim.json"), str(claim_root / "decisions-ledger.json"), str(control)))
        self.assertTrue(0 < int(options["--deadline-seconds"]) <= 20)
        # Exactly the per-trial options and then the fixed ones the frozen profile declares.
        with patch("gptgrep_harbor.decisions.CLAIM_ROOT", str(claim_root)):
            declared = decisions_module.trial_budget_init_options(8)
        self.assertEqual((init.args[0][1:3], init.args[0][3:7:2], init.args[0][7:]),
                         (["decisions-budget", "init"], list(decisions_module.TRIAL_BUDGET_INIT_PER_TRIAL), declared))
        self.assertEqual(status.args[0][1:], ["decisions-budget", "status", "--control", str(control), "--json"])
        # Ledger commands are offline: no provider credential.
        for call in (init, status):
            self.assertNotIn("OPENROUTER_API_KEY", call.kwargs["env"])
        # The ledger exists before the one Ask, which joins it.
        order = [call.args[0][1:3] for call in child.call_args_list]
        self.assertLess(order.index(["decisions-budget", "init"]), order.index(["--decisions-budget", str(control)]))
        self.assertEqual(ask_calls(child)[0].args[0][1:4], ["--decisions-budget", str(control), "ask"])
        operation = outputs["/logs/agent/operational.json"]
        budget = operation["decisions_trial_budget"]
        data = control.read_bytes()
        self.assertEqual(budget["control"], {"path": str(control), "bytes": len(data), "sha256": sha(data)})
        self.assertEqual((budget["max_submissions"], budget["enforced_by"], budget["plan_sha256"]),
                         (8, "product_decisions_budget_ledger", operation["trial_claim"]["sha256"]))
        # The mirrored limits behind the admitted bound match the product's own report.
        self.assertEqual(operation["decisions_product_limits"],
                         {"schema": "gptgrep.harbor.decisions-product-limits.v1", "source": "product_schema",
                          "mirrored": {"max_submissions": 200, "max_request_bytes": 65536},
                          "product_reported": {"max_submissions": 200, "max_request_bytes": 65536}, "match": True})
        self.assertEqual((budget["remaining_submissions"], budget["limit_reached"], budget["limit_bound"]),
                         (6, False, False))
        self.assertEqual(budget["status_after_ask"]["reservations"]["total"], 2)
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")
        self.assertNotIn("failure_code", operation)

    def test_binding_trial_limit_is_a_recorded_operational_failure_and_never_retried(self):
        failed = canonical({"status": "failed", "error": {"jev": {"requests": 8, "attempted_calls": 9}}})
        cases = (("failed_at_limit", failed, FakeTrialLedger(remaining=0), "operational_failure", True, True),
                 ("completed_at_limit", legacy_report(), FakeTrialLedger(remaining=0), "response", True, False),
                 ("failed_below_limit", failed, FakeTrialLedger(remaining=3), "operational_failure", False, False),
                 ("status_unavailable", failed, FakeTrialLedger(status="unavailable"), "operational_failure",
                  None, None))
        for name, report, ledger, status, reached, bound in cases:
            with self.subTest(case=name):
                outputs, child = self.run_reader(report, config=self.invocation_config(),
                                                 claim_root=self.claim_root(), ledger=ledger)
                self.assertEqual(len(ask_calls(child)), 1)
                envelope = outputs["/answer/answer.json"]
                self.assertEqual(envelope["status"], status)
                operation = outputs["/logs/agent/operational.json"]
                budget = operation["decisions_trial_budget"]
                self.assertEqual((budget["limit_reached"], budget["limit_bound"]), (reached, bound))
                self.assertEqual(operation.get("failure_code"),
                                 "decisions_submission_limit_reached" if bound else None)
                if bound:
                    # A recorded, judged failure: label 0 without any Judge call.
                    result = grade(self.judge_config(), envelope, lambda *_: self.fail("Judge called"))
                    self.assertEqual((result["status"], result["label"]), ("operational_failure", 0))

    def test_trial_ledger_must_exist_before_the_ask_or_the_trial_stops_unspent(self):
        cases = {"init_failed": FakeTrialLedger(init={"status": "joined"}),
                 "other_limit": FakeTrialLedger(init={"max_submissions": 9}),
                 "joins_existing": FakeTrialLedger(init={"joins_existing": True}),
                 "other_control": FakeTrialLedger(init={"control_sha256": "0" * 64})}
        for name, ledger in cases.items():
            with self.subTest(case=name):
                claim_root = self.claim_root()
                outputs, child = self.run_reader(legacy_report(), config=self.invocation_config(),
                                                 claim_root=claim_root, ledger=ledger)
                self.assertEqual(ask_calls(child), [])
                operation = outputs["/logs/agent/operational.json"]
                self.assertEqual(operation["failure_code"], "decisions_trial_budget_unavailable")
                self.assertNotIn("decisions_trial_budget", operation)
                self.assertTrue((claim_root / "claim.json").exists())
        # An unset limit stops before the claim, the ledger and the Ask.
        config = self.invocation_config()
        del config["decisions_max_submissions_per_trial"]
        claim_root = self.claim_root()
        outputs, child = self.run_reader(legacy_report(), config=config, claim_root=claim_root)
        self.assertEqual((ask_calls(child), self.ledger_calls(child, "init")), ([], []))
        self.assertEqual(outputs["/logs/agent/operational.json"]["failure_code"], "decisions_max_submissions_per_trial")
        self.assertEqual(list(claim_root.iterdir()), [])

    def test_invocation_bounded_ask_claims_once_and_keeps_its_cost_receipt(self):
        config = {**self.config(), "decisions_mode": "invocation_bounded", "decisions_max_submissions_per_trial": 8}
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
                argv = ask_calls(child)[0].args[0]
                self.assertEqual(argv[:4], ["/synthetic/gptgrep", "--decisions-budget",
                                            str(claim_root / "decisions-budget.json"), "ask"])
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
                self.assertEqual(operation["trial_claim"]["sha256"], sha(claim))
                host = (claim_root / "decisions-cost.json").read_bytes()
                self.assertEqual(json.loads(host)["decisions_cost"], receipt)
                self.assertEqual(json.loads(host)["product_report_sha256"], sha(report))
                self.assertEqual(operation["decisions_receipt"]["sha256"], sha(host))
                # A re-executed trial (for example in a resumed Harbor job) finds
                # the claim and fails closed before the Ask: no Decisions spend.
                again, child = self.run_reader(report, config=config, claim_root=claim_root)
                self.assertEqual((ask_calls(child), self.ledger_calls(child, "init")), ([], []))
                self.assertEqual(again["/answer/answer.json"]["status"], "operational_failure")
                repeated = again["/logs/agent/operational.json"]
                self.assertEqual((repeated["failure"], repeated["attempts"]), ("reader_failed", []))
                self.assertEqual(repeated["failure_code"], "trial_already_claimed")
                self.assertNotIn("decisions_receipt", repeated)
                self.assertEqual((claim_root / "claim.json").read_bytes(), claim)

    def test_product_report_must_confirm_the_mirrored_limits_before_the_claim(self):
        cases = {"larger_requests": {"request_bytes": 131072}, "more_submissions": {"max_submissions": 400},
                 "unreported": {"request_bytes": None}}
        for name, changes in cases.items():
            with self.subTest(case=name):
                claim_root = self.claim_root()
                outputs, child = self.run_reader(legacy_report(), config=self.invocation_config(),
                                                 claim_root=claim_root,
                                                 schema={"stdout": product_schema_report(**changes)})
                self.assertEqual((ask_calls(child), self.ledger_calls(child, "init")), ([], []))
                self.assertEqual(list(claim_root.iterdir()), [])
                operation = outputs["/logs/agent/operational.json"]
                self.assertEqual(operation["failure_code"], "decisions_product_limits_mismatch")
                limits = operation["decisions_product_limits"]
                self.assertIs(limits["match"], False)
                self.assertEqual(limits["product_reported"],
                                 {"max_submissions": changes.get("max_submissions", 200),
                                  "max_request_bytes": changes.get("request_bytes", 65536)})
        # Other Decisions modes do not rely on the mirrored request bound.
        outputs, _ = self.run_reader(legacy_report(), schema={"stdout": product_schema_report(request_bytes=1)})
        self.assertNotIn("decisions_product_limits", outputs["/logs/agent/operational.json"])
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")

    def test_invocation_bounded_ask_requires_its_claim_mount(self):
        config = {**self.config(), "decisions_mode": "invocation_bounded", "decisions_max_submissions_per_trial": 8}
        del config["decisions_budget"]
        missing = self.claim_root() / "not-mounted"
        outputs, child = self.run_reader(legacy_report(), config=config, claim_root=missing)
        self.assertEqual(ask_calls(child), [])
        self.assertEqual(outputs["/answer/answer.json"]["status"], "operational_failure")
        self.assertFalse(missing.exists())

    def test_bare_prompt_states_the_enforced_no_external_search_policy(self):
        # The bare prompt is the same common task prompt in every reader mode.
        for mode in ("legacy_text", "rich_multimodal"):
            with self.subTest(reader_mode=mode):
                prompt = reader.raw_prompt({"raw_document": "/data/corpus/synthetic.txt", "reader_mode": mode},
                                           self.question).decode()
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

    def test_treatment_records_its_build_identity_and_the_product_rules_description(self):
        outputs, child = self.run_reader(legacy_report())
        operation = outputs["/logs/agent/operational.json"]
        contract = operation["reader_contract"]
        # The identity is the verified bundle's provenance digest, which binds the
        # rule text compiled into the product.
        self.assertEqual((contract["identity"], contract["product_provenance_sha256"]),
                         ("product_provenance_sha256", "9" * 64))
        # The --schema field is the product's own description of its rules, not the
        # rule text: its digest is recorded under that name and never as the identity.
        self.assertEqual(contract["answer_rules_description_sha256"], sha(SYNTHETIC_ANSWER_RULES.encode()))
        self.assertEqual(contract["answer_rules_description_source"], "product_schema:commands.ask.reader_answer_rules")
        self.assertNotIn("answer_rules_sha256", contract)
        self.assertEqual(contract["product_schema_sha256"], sha(product_schema_report()))
        self.assertEqual((contract["arm"], contract["base_instructions"], contract["answer_rules_received"]),
                         ("gptgrep", "gptgrep_reader_contract", True))
        self.assertEqual(outputs["/answer/answer.json"]["status"], "response")
        # Observed once, before the query timer and the Ask.
        self.assertEqual(child.call_args_list[0].args[0], ["/synthetic/gptgrep", "--schema"])
        self.assertEqual(len(ask_calls(child)), 1)
        # A changed description changes only its own digest; the identity follows the bundle.
        changed, _ = self.run_reader(legacy_report(), schema={"stdout": product_schema_report("Changed rules.")})
        changed = changed["/logs/agent/operational.json"]["reader_contract"]
        self.assertEqual((changed["answer_rules_description_sha256"], changed["product_provenance_sha256"]),
                         (sha(b"Changed rules."), "9" * 64))
        # The materialized split travels unchanged: the declared contract difference is no mismatch.
        declared = [{"code": "reader_contract_differs"}]
        config = {**self.config(), "qualification_mismatches": [{"code": "observed generic mismatch"}],
                  "declared_treatment_differences": declared}
        split, _ = self.run_reader(legacy_report(), config=config)
        operation = split["/logs/agent/operational.json"]
        self.assertEqual((operation["qualification_mismatches"], operation["declared_treatment_differences"]),
                         ([{"code": "observed generic mismatch"}], declared))

    def test_treatment_without_a_reported_contract_stops_before_the_ask(self):
        cases = {"process_failure": {"return_code": 2},
                 "child_failure": {"failure": "child_failed_or_limit"},
                 "not_json": {"stdout": b"not a schema"},
                 "missing_field": {"stdout": canonical({"schema_version": "gptgrep.cli.v1", "commands": {"ask": {}}})},
                 "empty_rules": {"stdout": product_schema_report("  ")},
                 "not_text": {"stdout": product_schema_report(["rules"])}}
        for name, schema in cases.items():
            with self.subTest(case=name):
                outputs, child = self.run_reader(legacy_report(), schema=schema)
                self.assertEqual(ask_calls(child), [])
                self.assertEqual(outputs["/answer/answer.json"]["status"], "operational_failure")
                operation = outputs["/logs/agent/operational.json"]
                self.assertIsNone(operation["reader_contract"])
                self.assertEqual((operation["failure"], operation["failure_code"]),
                                 ("reader_failed", "product_reader_contract_unavailable"))


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
                       "reader_seccomp_profile": {"path": "/synthetic/reader-seccomp.json",
                                                  "bytes": READER_SECCOMP_BYTES, "sha256": READER_SECCOMP_SHA256},
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
            # No reader role is materialized without the approved seccomp pin.
            owner.runtime = {key: value for key, value in runtime.items() if key != "reader_seccomp_profile"}
            with self.assertRaisesRegex(ValueError, "reader_seccomp_profile_required"):
                owner._task(destination, Path(adapter_module.__file__).parent, row, document, binding)
            self.assertFalse(destination.exists())
            owner.runtime = runtime
            owner._task(destination, Path(adapter_module.__file__).parent, row, document, binding)
            self.assertFalse((destination / "environment/corpus/.gptgrep/index.bin").exists())
            for directory in ("environment", "tests"):
                compose = json.loads((destination / directory / "docker-compose.yaml").read_text())
                self.assertEqual(compose["services"]["main"]["labels"], {"gptgrep.auth-job": "synthetic-full62"})
                # Only the reader role carries the approved seccomp profile.
                self.assertEqual(compose["services"]["main"].get("security_opt"),
                                 ["seccomp=/synthetic/reader-seccomp.json"] if directory == "environment" else None)
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
        # connector, plugin, tool-suggestion and image-generation features and
        # account cloud Skills are off for both arms.
        self.assertEqual(config["features"], {
            "shell_tool": True, "view_image": True, "unified_exec": True,
            "multi_agent": True, "plugins": False, "apps": False, "tool_suggest": False,
            "image_generation": False, "code_mode": {"enabled": True}, "code_mode_host": {"enabled": True}})
        self.assertEqual(config["cloud"], {"skills": {"enabled": False}})
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
        # The Judge disables the same host-side tools and account cloud Skills.
        for feature in ("apps", "plugins", "tool_suggest", "image_generation"):
            self.assertIs(config["features"][feature], False)
        self.assertEqual(config["cloud"], {"skills": {"enabled": False}})
        self.assertEqual(config["default_permissions"], "full62_judge")
        self.assertEqual(config["web_search"], "disabled")
        self.assertFalse(config["permissions"]["full62_judge"]["network"]["enabled"])
        self.assertEqual(config["permissions"]["full62_judge"]["filesystem"]["/data"], "deny")
        self.assertNotIn("/tmp/full62-work",
                         config["permissions"]["full62_judge"]["filesystem"])


class ReaderContractTests(unittest.TestCase):
    """GPTgrep's reader contract is the treatment's; the raw-only arm receives none of it."""

    COMMON_TASK_PROMPT = ("Answer the question using only the protocol-scoped raw document at "
                          "/data/corpus/generic.txt. You may inspect that document with native tools. "
                          "No external search or network access is available. Give the complete answer "
                          "and source locations when available. Question:\nGeneric question?")

    def test_raw_only_prompt_is_the_common_task_prompt_in_every_reader_mode(self):
        config = {"raw_document": "/data/corpus/generic.txt"}
        for mode in ("legacy_text", "rich_multimodal", None):
            with self.subTest(reader_mode=mode):
                prompt = reader.raw_prompt({**config, "reader_mode": mode}, "Generic question?").decode()
                self.assertEqual(prompt, self.COMMON_TASK_PROMPT)
                assert_no_gptgrep_rule_text(self, prompt)
        # The adapter keeps no mirrored copy of the product's rule text.
        for name in ("READER_ANSWER_RULES", "shared_answer_rules"):
            self.assertFalse(hasattr(reader, name), name)

    def test_raw_only_record_states_that_it_received_no_gptgrep_contract(self):
        with patch.object(reader, "run", side_effect=AssertionError("no product call for the bare arm")):
            for mode in ("legacy_text", "rich_multimodal"):
                with self.subTest(reader_mode=mode):
                    record, schema, failure = reader.observe_reader_contract({"reader_mode": mode},
                                                                             {"arm": "raw_only"})
                    self.assertEqual((schema, failure), (None, None))
                    self.assertEqual(record, RAW_READER_CONTRACT)

    def test_rich_treatment_receives_no_answer_rules_and_needs_no_schema_report(self):
        config = {"reader_mode": "rich_multimodal",
                  "gptgrep_bundle": {"provenance": {"path": "/synthetic/p.json", "bytes": 1, "sha256": "9" * 64}}}
        with patch.object(reader, "run", side_effect=AssertionError("no schema report in rich_multimodal")):
            record, schema, failure = reader.observe_reader_contract(config, {"arm": "gptgrep"})
        self.assertEqual((schema, failure), (None, None))
        self.assertEqual(record, {"schema": "gptgrep.harbor.reader-contract.v1", "arm": "gptgrep",
                                  "base_instructions": "gptgrep_reader_contract", "reader_mode": "rich_multimodal",
                                  "answer_rules_received": False, "answer_rules_description_sha256": None,
                                  "identity": "product_provenance_sha256",
                                  "product_provenance_sha256": "9" * 64})


if __name__ == "__main__":
    unittest.main()
