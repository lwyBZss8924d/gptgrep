"""Synthetic checks for nested model accounting; no provider calls or task data."""
import copy
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts/pageindex_baseline"))
from native_models import PLANNER_PROFILE, TOKEN_FIELDS, attempts_from_report, usage_summary

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("planned_system_eval", ROOT / "scripts/gptgrep_system_eval.py")
system = importlib.util.module_from_spec(spec)
spec.loader.exec_module(system)


def attempt(role, amount=10):
    return {"attempt_id": role, "role": role, "status": "completed",
            "requested_model": "gpt-5.6-luna", "requested_reasoning_effort": "max",
            "requested_service_tier": "fast", "model": "gpt-5.6-luna", "model_provider": "openai",
            "effective_reasoning_effort": "max", "effective_service_tier": "priority",
            "thread_id": "thread-" + role, "turn_id": "turn-" + role,
            "usage": {"total": {key: amount for key in TOKEN_FIELDS.values()},
                      "last": {key: 99999 for key in TOKEN_FIELDS.values()}, "modelContextWindow": 1000000},
            "elapsed_ms": 12, "server_retry_notifications": 0, "accounting_complete": True}


class NativeModelAccountingTests(unittest.TestCase):
    def report(self):
        return {"status": "completed", "usage_scope": "final_reader",
                "usage": attempt("final_reader", 20)["usage"],
                "model_attempts": [attempt("query_planner", 10), attempt("final_reader", 20)]}

    def test_planner_and_reader_are_counted_without_last_or_context_or_legacy_duplication(self):
        records = attempts_from_report(self.report(), PLANNER_PROFILE, required=True)
        total = usage_summary(records)
        self.assertEqual(total["attempted_calls"], 2)
        self.assertEqual(total["observed_turns"], 2)
        self.assertEqual(total["token_totals"]["total_tokens"]["total"], 30)
        self.assertEqual(total["roles"]["query_planner"]["attempted_calls"], 1)
        self.assertEqual(total["roles"]["final_reader"]["attempted_calls"], 1)
        self.assertTrue(total["accounting_complete"])
        self.assertNotIn("last", records[0]["usage"])
        self.assertNotIn("modelContextWindow", records[0]["usage"])

    def test_partial_failed_observations_remain_known_subtotals(self):
        report = self.report()
        report["status"] = "failed"
        report["model_attempts"][1].update(status="interrupted", accounting_complete=False)
        records = attempts_from_report(report, PLANNER_PROFILE, required=True)
        total = usage_summary(records)
        self.assertEqual(total["token_totals"]["total_tokens"]["known_subtotal"], 30)
        self.assertIsNone(total["token_totals"]["total_tokens"]["total"])
        self.assertFalse(total["accounting_complete"])
        report["model_attempts"][1]["usage"] = None
        total = usage_summary(attempts_from_report(report, PLANNER_PROFILE, required=True))
        self.assertEqual(total["token_totals"]["total_tokens"]["known_subtotal"], 10)
        self.assertEqual(total["token_totals"]["total_tokens"]["missing_contributions"], 1)

    def test_failed_planner_does_not_invent_a_reader_or_usage(self):
        step = attempt("query_planner")
        step.update(status="failed", model=None, model_provider=None, thread_id=None,
                    turn_id=None, usage=None, accounting_complete=False)
        report = {"host_retrieval": {"model_attempts": [step]}}
        total = usage_summary(attempts_from_report(report, PLANNER_PROFILE, required=True))
        self.assertEqual(total["attempted_calls"], 1)
        self.assertEqual(total["observed_turns"], 0)
        self.assertEqual(total["roles"]["final_reader"]["attempted_calls"], 0)
        self.assertIsNone(total["token_totals"]["total_tokens"]["total"])
        self.assertIsNone(attempts_from_report({}, PLANNER_PROFILE))
        self.assertFalse(usage_summary(None)["available"])
        with self.assertRaises(ValueError):
            attempts_from_report({}, PLANNER_PROFILE, required=True)

    def test_last_only_or_empty_total_does_not_count_as_observed_total_usage(self):
        for observed in ({"last": {"totalTokens": 500}}, {"total": {}}, {"total": None}, {}):
            step = attempt("query_planner")
            step.update(status="failed", usage=observed, accounting_complete=False)
            records = attempts_from_report({"host_retrieval": {"model_attempts": [step]}},
                                           PLANNER_PROFILE, required=True)
            total = usage_summary(records)
            self.assertEqual(total["missing_usage_attempts"], 1)
            self.assertIsNone(total["token_totals"]["total_tokens"]["total"])
            self.assertEqual(total["token_totals"]["total_tokens"]["known_subtotal"], 0)

    def test_conflicting_or_duplicate_role_and_turn_identity_is_rejected(self):
        for mutate in (
            lambda r: r["model_attempts"].append(attempt("query_planner")),
            lambda r: r["model_attempts"][1].update(role="query_planner"),
            lambda r: r["model_attempts"][1].update(attempt_id="query_planner"),
            lambda r: r["model_attempts"][1].update(thread_id="thread-query_planner", turn_id="turn-query_planner"),
        ):
            report = self.report()
            mutate(report)
            with self.assertRaises(ValueError):
                attempts_from_report(report, PLANNER_PROFILE, required=True)
        records = attempts_from_report(self.report(), PLANNER_PROFILE, required=True)
        duplicate = usage_summary([*records, copy.deepcopy(records[0])])
        self.assertEqual(duplicate["duplicate_turn_observations"], 1)
        self.assertEqual(duplicate["token_totals"]["total_tokens"]["total"], 30)
        changed = copy.deepcopy(records[0])
        changed["usage"]["total"]["inputTokens"] += 1
        with self.assertRaises(ValueError):
            usage_summary([*records, changed])

    def test_invalid_numbers_profiles_and_incomplete_success_do_not_pass(self):
        mutations = [
            lambda r: r["model_attempts"][0].update(requested_model="another-model"),
            lambda r: r["model_attempts"][0].update(effective_service_tier="flex"),
            lambda r: r["model_attempts"][0].update(thread_id=None),
            lambda r: r["model_attempts"][0].update(status="failed"),
            lambda r: r["model_attempts"][0].update(elapsed_ms=float("nan")),
            lambda r: r["model_attempts"].pop(),
        ]
        for value in (True, -1, 1.5, 2**64):
            mutations.append(lambda r, value=value: r["model_attempts"][0]["usage"]["total"].update(totalTokens=value))
        for mutate in mutations:
            report = self.report()
            mutate(report)
            with self.assertRaises(ValueError):
                attempts_from_report(report, PLANNER_PROFILE, required=True)

    def test_query_planning_keeps_original_reader_inputs_and_excludes_gold(self):
        args = argparse.Namespace(jev_model="typesafe/jev-1.13", codex_bin="codex",
                                  codex_home=Path("/synthetic/runtime"), model="gpt-5.6-luna",
                                  reasoning_effort="max", service_tier="fast", timeout=180,
                                  max_tool_calls=12, experimental_query_plan=True)
        row = {"source_row": 1, "question": "What  does the invented rule cover?",
               "doc_id": "invented.md", "answer": "PRIVATE_GOLD", "evidence_pages": "[999]"}
        altered = {**row, "answer": "DIFFERENT_GOLD", "evidence_pages": "[123]", "task_type": "secret"}
        first = system.ask_arguments(Path("tool"), Path("/synthetic/corpus"), row, args)
        self.assertEqual(first, system.ask_arguments(Path("tool"), Path("/synthetic/corpus"), altered, args))
        self.assertLess(first.index("--experimental-query-plan"), first.index("--"))
        self.assertIn(row["question"], first)
        self.assertEqual(system.reader_payload(row, args), system.reader_payload(altered, args))
        self.assertNotIn("PRIVATE_GOLD", repr(first))
        self.assertTrue(system.reader_payload(row, args)["experimental_query_plan"])
        args.experimental_query_plan = False
        self.assertNotIn("experimental_query_plan", system.reader_payload(row, args))
        self.assertNotIn("--experimental-query-plan", system.ask_arguments(Path("tool"), Path("/synthetic/corpus"), row, args))

    def test_planner_usage_is_recovered_before_any_jev_request_with_original_workflow_binding(self):
        question = hashlib.sha256(b"an original synthetic request").hexdigest()
        workflow = {"query_sha256": question, "document_scope": "invented.md", "generation": "g-synthetic"}
        event = {"schema_version": "gptgrep.jev-attempt.v1", "event": "failed",
                 "generation": "g-synthetic", "workflow": workflow,
                 "model_attempts": [attempt("query_planner")], "attempted_calls": 0,
                 "requests": 0, "unobserved_attempts": 0, "accounting_complete": False}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "attempt.jsonl"
            path.write_text(json.dumps(event) + "\n")
            recovered = system.ledger_recovery(path, generation="g-synthetic", query_sha256=question,
                                               document="invented.md", reader_profile=PLANNER_PROFILE, planned=True)
            self.assertTrue(recovered["case_identity_verified"])
            self.assertEqual(recovered["jev"]["requests"], 0)
            self.assertFalse(recovered["jev"]["accounting_complete"])
            self.assertEqual(recovered["model_turn_accounting"]["attempted_calls"], 1)
            self.assertEqual(recovered["model_turn_accounting"]["token_totals"]["total_tokens"]["total"], 10)
            for broken in ({**event, "workflow": {**workflow, "query_sha256": "foreign"}},
                           {key: value for key, value in event.items() if key != "workflow"}):
                path.write_text(json.dumps(broken) + "\n")
                with self.assertRaises(ValueError):
                    system.ledger_recovery(path, generation="g-synthetic", query_sha256=question,
                                           document="invented.md", reader_profile=PLANNER_PROFILE, planned=True)


if __name__ == "__main__":
    unittest.main()
