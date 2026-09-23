"""Synthetic v4 state-machine tests; no native process, provider or task inputs."""
from __future__ import annotations
import copy
import hashlib
import json
import os
from pathlib import Path
from subprocess import CompletedProcess
import unittest
from unittest.mock import patch

import test_system_eval as legacy
from native_models import TOKEN_FIELDS
import native_builder_models as builder

system = legacy.system


def synthetic_price_card():
    tiers = {
        "fast": {"observed_tier_aliases": ["fast", "priority"], "bands": [
            {"id": "short", "max_input_tokens": 272000, "input": 2, "cached_input": .2, "cache_write_input": 2.5, "output": 10},
            {"id": "long", "max_input_tokens": None, "input": 4, "cached_input": .4, "cache_write_input": 5, "output": 15}]},
        "standard": {"observed_tier_aliases": ["standard"], "bands": [
            {"id": "short", "max_input_tokens": 272000, "input": 1, "cached_input": .1, "cache_write_input": 1.25, "output": 5},
            {"id": "long", "max_input_tokens": None, "input": 2, "cached_input": .2, "cache_write_input": 2.5, "output": 7.5}]},
    }
    return {"schema_version": "gptgrep.api-price-card.v1", "card_id": "synthetic-test-prices-not-published-rates",
            "currency": "USD", "token_unit": 1_000_000, "source_urls": ["https://example.invalid/synthetic-pricing"],
            "models": {model: {"tiers": copy.deepcopy(tiers)} for model in ("gpt-6-luna", "gpt-5.6-luna")}}


class EnrichmentExecutionTests(unittest.TestCase):
    def setUp(self):
        self.fixture = legacy.ResumeExecutionTests("runTest")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.args = self.fixture.args
        self.args.stage = "plan"
        self.args.model = self.args.planner_model = "gpt-6-luna"
        self.args.judge_model = "gpt-6-luna"
        self.args.judge_reasoning_effort = "max"
        self.args.experimental_query_plan = self.args.experimental_enrichment = True
        self.args.experimental_evidence_roles = False
        self.args.builder_model = None
        self.args.max_builder_calls = self.args.max_builder_jev_calls = 16
        self.args.resume_enrichment = False
        self.args.expected_enrichment_plan_sha256 = None
        self.args.original_pageindex_results = None
        self.args.price_card = self.fixture.root / "synthetic-price-card.json"
        self.args.price_card.write_bytes(builder.encoded(synthetic_price_card()))
        self.counts = self.fixture.counts
        self.counts.update(plan=0, enrich=0)
        self.capability = True
        self.enrich_mode = "complete"
        self.navigation_report = True
        self.original_process = self.fixture.process
        self.fixture.stack.enter_context(patch.object(system, "owned_process", side_effect=self.process))
        self.fixture.stack.enter_context(patch("bridge.owned_process", side_effect=self.process))

    def plan(self):
        self.args.stage = "plan"
        result = system.execute(self.args)
        self.assertEqual(result["status"], "plan_prepared")
        self.args.expected_enrichment_plan_sha256 = result["expected_enrichment_plan_sha256"]
        return result

    def run_enriched(self):
        self.args.stage = "run"
        return system.execute(self.args)

    def process(self, arguments, cwd, timeout, **hooks):
        operation = arguments[1]
        if operation == "--schema":
            options = {"navigation-overlay-sha256": {"type": "string"},
                       "navigation-max-jev-calls": {"type": "integer"},
                       "navigation-max-request-bytes": {"type": "integer"}} if self.capability else {}
            return CompletedProcess(arguments, 0, json.dumps({"commands": {"ask": {"options": options},
                "enrich": {"plan_only_model_calls": 0, "model_assisted": True}}}).encode(), b"")
        if operation == "enrich":
            self.fixture.seen_arguments.append(arguments)
            if hooks.get("on_start") is not None:
                hooks["on_start"](999999)
            if "--plan-only" in arguments:
                return self.plan_response(arguments)
            return self.enrich_response(arguments)
        result = self.original_process(arguments, cwd, timeout, **hooks)
        report = json.loads(result.stdout)
        if operation == "index":
            corpus = Path(arguments[2])
            current = builder.read_json(corpus / ".gptgrep/CURRENT.json")
            path = corpus / ".gptgrep/generations" / current["generation"] / "manifest.json"
            manifest = builder.read_json(path)
            for document in manifest["documents"]:
                document["nodes"] = [{"id": "root"}]
            path.write_bytes(builder.encoded(manifest))
            current["manifest_sha256"] = builder.digest(path)
            (corpus / ".gptgrep/CURRENT.json").write_bytes(builder.encoded(current))
        elif operation == "ask":
            index = self.counts["ask"]
            records = []
            for role, amount in (("query_planner", 10), ("final_reader", 20)):
                records.append({"attempt_id": f"{role}-{index}", "role": role, "status": "completed",
                    "requested_model": self.args.model, "requested_reasoning_effort": self.args.reasoning_effort, "requested_service_tier": "fast",
                    "model": self.args.model, "model_provider": "openai", "effective_reasoning_effort": self.args.reasoning_effort,
                    "effective_service_tier": "priority", "thread_id": f"{role}-thread-{index}", "turn_id": f"{role}-turn-{index}",
                    "usage": {"total": {key: amount for key in TOKEN_FIELDS.values()}}, "elapsed_ms": 1,
                    "server_retry_notifications": 0, "accounting_complete": True})
            report.update(model_attempts=records, thread_id=records[-1]["thread_id"], turn_id=records[-1]["turn_id"], usage=records[-1]["usage"])
            if self.navigation_report:
                publication = builder.read_json(Path(arguments[-1]) / ".gptgrep/NAVIGATION.json")
                report["navigation"] = {**publication, "schema_version": "gptgrep.navigation-query.v1", "status": "completed",
                    "document_scope": next(value.split("=", 1)[1] for value in arguments if value.startswith("--document=")),
                    "hint_scan_complete": True}
        elif operation == "host-complete":
            report.update(thread_id=f"judge-thread-{self.counts['judge']}", turn_id=f"judge-turn-{self.counts['judge']}")
        return CompletedProcess(arguments, result.returncode, json.dumps(report).encode(), result.stderr)

    def plan_response(self, arguments):
        self.counts["plan"] += 1
        corpus = Path(arguments[2])
        generation = builder.read_json(corpus / ".gptgrep/CURRENT.json")
        source_hashes = {row["doc_id"]: builder.digest(corpus / row["doc_id"]) for row in self.fixture.rows}
        canonical = system.native_snapshot(corpus, source_hashes)
        binding, documents = system.enrichment_plan_inputs(corpus, {"generation_binding": generation}, canonical, self.args)
        binding.update({name: builder.fingerprint(name) for name in ("builder_prompt_sha256", "builder_schema_sha256",
                        "support_prompt_sha256", "support_schema_sha256")})
        units = []
        for name, document in sorted(documents.items()):
            anchor = {"anchor_id": "nav_" + document["document_id"], "byte_start": 0,
                      "byte_end": len(canonical[name]), "sha256": hashlib.sha256(canonical[name]).hexdigest()}
            units.append({"document": document, "anchor": anchor, "builder_input_sha256": builder.fingerprint({"anchor": anchor}),
                          "builder_input_bytes": 900, "worst_case_jev_template_sha256": builder.fingerprint({"worst": anchor}),
                          "worst_case_jev_request_bytes": 12000})
        plan = {"schema_version": "gptgrep.enrichment-plan.v1", "binding": binding, "documents": [documents[name] for name in sorted(documents)],
                "units": units, "unit_count": len(units), "worst_case_builder_calls": len(units), "worst_case_jev_calls": len(units)}
        plan["plan_sha256"] = builder.fingerprint({name: plan[name] for name in ("binding", "documents", "units")})
        path = Path(arguments[arguments.index("--plan-output") + 1])
        with path.open("xb") as stream:
            stream.write(builder.encoded(plan))
        compact = {"schema_version": "gptgrep.enrich-plan.v1", "status": "plan_prepared", "plan_sha256": plan["plan_sha256"],
                   "unit_count": len(units), "new_model_calls": 0, "worst_case_within_declared_caps": True}
        return CompletedProcess(arguments, 0, builder.encoded(compact), b"")

    def enrich_response(self, arguments):
        self.counts["enrich"] += 1
        corpus = Path(arguments[2])
        path = Path(arguments[arguments.index("--ledger-path") + 1])
        plan = builder.read_json(path.parent / "plan.json")
        lines = [builder.parse(line) for line in path.read_bytes().splitlines()] if path.exists() else []

        def append(payload):
            previous = lines[-1]["sha256"] if lines else None
            line = {"sequence": len(lines), "previous_sha256": previous, "payload": payload}
            line["sha256"] = builder.fingerprint(line)
            with path.open("ab") as stream:
                stream.write(builder.encoded(line) + b"\n")
            lines.append(line)

        if not lines:
            append({"event": "bound", "binding": plan["binding"], "plan_sha256": plan["plan_sha256"]})
        windows = [line["payload"]["window"] for line in lines if line["payload"]["event"] == "window_completed"]
        reused = len(windows)
        calls = [line["payload"]["reservation"] for line in lines if line["payload"]["event"] == "call_reserved"]
        for unit in plan["units"][reused:]:
            builder_id = len(calls)
            reservation = {"call_id": builder_id, "kind": "builder", "anchor_id": unit["anchor"]["anchor_id"],
                "requested_model": plan["binding"]["builder_model"], "request_sha256": unit["builder_input_sha256"], "request_bytes": unit["builder_input_bytes"]}
            append({"event": "call_reserved", "reservation": reservation}); calls.append(reservation)
            observed = {"model": plan["binding"]["builder_model"], "provider": "openai", "thread_id": f"builder-thread-{builder_id}",
                        "turn_id": f"builder-turn-{builder_id}", "response_id": None, "effective_reasoning_effort": plan["binding"]["reasoning_effort"],
                        "effective_service_tier": "priority", "server_retry_notifications": 0,
                        "usage": {"total": {key: 10 for key in TOKEN_FIELDS.values()}}}
            if self.enrich_mode == "wrong_model":
                observed["model"] = "wrong-observed-model"
            failed = self.enrich_mode == "failed"
            append({"event": "call_finished", "receipt": {"call_id": builder_id, "elapsed_ms": 1,
                   "error_code": "enrich_builder_call_failed" if failed else None, "observed": None if failed else observed}})
            if failed:
                append({"event": "failed", "code": "enrich_builder_call_failed"})
                break
            drafts = [] if self.enrich_mode == "empty" else [{"anchor_id": unit["anchor"]["anchor_id"], "hint": "A synthetic source topic."}]
            jev_id = None
            if drafts:
                jev_id = len(calls)
                reservation = {"call_id": jev_id, "kind": "jev", "anchor_id": unit["anchor"]["anchor_id"],
                               "requested_model": plan["binding"]["jev_model"], "request_sha256": builder.fingerprint(drafts), "request_bytes": 1200}
                append({"event": "call_reserved", "reservation": reservation}); calls.append(reservation)
                append({"event": "call_finished", "receipt": {"call_id": jev_id, "elapsed_ms": 1, "error_code": None,
                    "observed": {"model": plan["binding"]["jev_model"], "provider": "synthetic", "thread_id": None, "turn_id": None,
                                 "response_id": f"jev-{jev_id}", "effective_reasoning_effort": None, "effective_service_tier": None,
                                 "server_retry_notifications": None, "usage": {"prompt_tokens": 11, "completion_tokens": 2,
                                     "total_tokens": 13, "cost": builder.parse(b"1e-6")}}}})
            window = {"document": unit["document"], "anchor": unit["anchor"], "builder_call_id": builder_id,
                      "jev_call_id": jev_id, "drafts": drafts, "support": ["supported"] * len(drafts)}
            append({"event": "window_completed", "window": window}); windows.append(window)
            if self.enrich_mode == "incomplete":
                append({"event": "checkpoint", "cursor": {"document_path": plan["units"][len(windows)]["document"]["path"], "offset_bytes": 0}, "reason": "window_run_limit"})
                break

        def summary(kind):
            selected = [call for call in calls if call["kind"] == kind]
            receipts = {line["payload"]["receipt"]["call_id"]: line["payload"]["receipt"] for line in lines if line["payload"]["event"] == "call_finished"}
            values = [receipts[call["call_id"]] for call in selected]
            observed = [value["observed"] for value in values]
            known = [value["usage"]["total"]["totalTokens"] if kind == "builder" else value["usage"]["total_tokens"] for value in observed if value is not None]
            return {"attempted_calls": len(selected), "completed_calls": sum(value["error_code"] is None for value in values),
                    "failed_calls": sum(value["error_code"] is not None for value in values), "unobserved_calls": sum(value is None for value in observed),
                    "missing_usage_calls": sum(value is None for value in observed), "models": sorted({value["model"] for value in observed if value is not None}),
                    "known_total_tokens": sum(known) if known else None, "missing_total_tokens": len(selected) - len(known), "total_tokens_overflowed": False}

        full = len(windows) == plan["unit_count"]
        coverage, publication = None, None
        if full:
            documents = []
            for document in plan["documents"]:
                own = [window for window in windows if window["document"] == document]
                hints = [{"origin": "model_derived_navigation_only", "target": {"kind": "chunk", "anchor_id": window["anchor"]["anchor_id"]},
                          "hint": draft["hint"], "anchor_ids": [window["anchor"]["anchor_id"]]} for window in own for draft in window["drafts"]]
                documents.append({**document, "windows": [window["anchor"] for window in own], "hints": hints})
            total = sum(document["text_bytes"] for document in documents)
            coverage = {"documents_total": len(documents), "documents_with_hints": sum(bool(document["hints"]) for document in documents),
                        "nodes_total": len(documents), "nodes_with_hints": 0, "hints": sum(len(document["hints"]) for document in documents),
                        "raw_windows": len(windows), "hint_window_references": sum(len(document["hints"]) for document in documents),
                        "canonical_text_bytes": total, "covered_text_bytes": total, "documents_with_complete_windows": len(documents),
                        "partial_source_coverage": False, "partial_hint_coverage": True}
            binding = plan["binding"]
            overlay = {"schema_version": "gptgrep.navigation-overlay.v1", "generation": binding["source"]["generation"],
                       "manifest_sha256": binding["source"]["manifest_sha256"], "coverage": coverage, "documents": documents,
                       "producer": {"model": binding["builder_model"], "reasoning_effort": binding["reasoning_effort"], "service_tier": "fast",
                         "prompt_sha256": binding["builder_prompt_sha256"], "schema_sha256": binding["builder_schema_sha256"],
                         "jev": {"requested_model": binding["jev_model"], "actual_models": summary("jev")["models"],
                                 "logical_calls_attempted": summary("jev")["attempted_calls"], "validated_responses": summary("jev")["completed_calls"],
                                 "prompt_sha256": binding["support_prompt_sha256"], "schema_sha256": binding["support_schema_sha256"]}}}
            raw = builder.encoded(overlay)
            artifact_sha = hashlib.sha256(raw).hexdigest()
            append({"event": "publication_prepared", "artifact_sha256": artifact_sha})
            artifact = corpus / ".gptgrep/navigation-overlays" / (artifact_sha + ".json")
            artifact.parent.mkdir(exist_ok=True); artifact.write_bytes(raw)
            publication = {"schema_version": "gptgrep.navigation-overlay.v1", "generation": binding["source"]["generation"],
                           "manifest_sha256": binding["source"]["manifest_sha256"], "artifact_sha256": artifact_sha}
            (corpus / ".gptgrep/NAVIGATION.json").write_bytes(builder.encoded(publication))
            append({"event": "published", "publication": publication})
        status = "complete" if full else "incomplete" if self.enrich_mode == "incomplete" else "failed"
        report = {"schema_version": "gptgrep.enrich.v1", "status": status, "reason": None,
                  "source": plan["binding"]["source"], "ledger_path": str(path), "plan_sha256": plan["plan_sha256"],
                  "documents_completed": len(windows), "windows_completed": len(windows), "windows_reused": reused,
                  "next_cursor": None if full else {"document_path": plan["units"][len(windows)]["document"]["path"], "offset_bytes": 0},
                  "resume_safe": status == "incomplete", "builder": summary("builder"), "jev": summary("jev"),
                  "coverage": coverage, "publication": publication, "publication_state": "published" if full else "not_published", "elapsed_ms": 1}
        return CompletedProcess(arguments, 0 if full else 2, builder.encoded(report), b"")

    def test_missing_navigation_capability_stops_before_any_index_or_model(self):
        self.capability = False
        with self.assertRaisesRegex(ValueError, "navigation_cli_contract_unavailable_zero_model"):
            self.plan()
        self.assertEqual(self.counts, {"index": 0, "ask": 0, "judge": 0, "plan": 0, "enrich": 0})

    def test_relative_launcher_is_bound_before_run_directory_changes(self):
        launcher = self.fixture.root / "synthetic-launcher"
        launcher.write_text("#!/bin/sh\nexit 0\n")
        launcher.chmod(0o700)
        self.args.codex_bin = os.path.relpath(launcher, Path.cwd())
        result = self.plan()
        self.assertEqual(result["codex_bin"], str(launcher.resolve()))
        self.assertEqual(self.args.codex_bin, str(launcher.resolve()))
        self.assertEqual(self.counts["ask"], 0)

    def test_missing_explicit_launcher_rejects_before_index(self):
        self.args.codex_bin = "missing/synthetic-launcher"
        with self.assertRaisesRegex(ValueError, "codex_launcher_path_unavailable"):
            self.plan()
        self.assertEqual(self.counts["index"], 0)

    def test_zero_model_full_plan_and_strict_caps_are_immutable(self):
        result = self.plan()
        self.assertEqual(result["schema_version"], "gptgrep.system-eval.v4")
        self.assertEqual(result["enrichment_prepared"]["unit_count"], 2)
        self.assertEqual(self.counts, {"index": 1, "ask": 0, "judge": 0, "plan": 1, "enrich": 0})
        self.assertEqual(result["profile"]["roles"]["builder"]["model"], "gpt-6-luna")
        self.assertFalse(result["build"]["generative"])
        self.args.max_model_calls += 1
        with self.assertRaisesRegex(ValueError, "conditions changed"):
            self.plan()
        self.assertEqual(self.counts["enrich"], 0)

    def test_plan_required_and_all_rows_retained_when_preparation_is_unavailable(self):
        report = self.run_enriched()
        self.assertEqual(report["summary"]["question_denominator"], 2)
        self.assertEqual(report["summary"]["materialized_cases"], 2)
        self.assertEqual(report["summary"]["failed_cases"], 2)
        self.assertEqual((self.counts["ask"], self.counts["judge"], self.counts["enrich"]), (0, 0, 0))

    def test_complete_gate_profiles_navigation_and_reuse_without_reroll(self):
        self.plan()
        result = self.run_enriched()
        self.assertEqual(result["status"], "completed", result.get("enrichment_result"))
        self.assertEqual(result["summary"]["correct"], 1)
        self.assertEqual(result["host_invocations"], 5)
        accounting = result["enrichment_result"]["accounting"]
        self.assertEqual(accounting["builder"]["known_token_subtotal"], 20)
        self.assertAlmostEqual(accounting["jev"]["known_cost_subtotal_usd"], 2e-6)
        self.assertEqual(result["summary"]["native_model_turn_accounting"]["token_totals"]["total_tokens"]["total"], 60)
        before = dict(self.counts)
        again = self.run_enriched()
        self.assertEqual(again["summary"]["correct"], 1)
        self.assertEqual(self.counts, before)
        for arguments in self.fixture.seen_arguments:
            if arguments[1] == "ask":
                self.assertIn("--navigation-overlay-sha256", arguments)
                self.assertIn("--navigation-max-jev-calls", arguments)

    def test_incomplete_gate_resumes_same_ledger_and_counts_builder_once(self):
        self.plan(); self.enrich_mode = "incomplete"
        first = self.run_enriched()
        self.assertEqual(first["summary"]["failed_cases"], 2)
        self.assertEqual((self.counts["ask"], self.counts["judge"]), (0, 0))
        ledger = self.args.run_dir / "enrichment/ledger.jsonl"
        prefix = ledger.read_bytes()
        self.args.resume_enrichment = True; self.enrich_mode = "complete"
        final = self.run_enriched()
        self.assertEqual(final["status"], "completed", final.get("enrichment_result"))
        self.assertTrue(ledger.read_bytes().startswith(prefix))
        self.assertEqual(final["host_invocations"], 6)
        self.assertEqual(final["enrichment_result"]["accounting"]["builder"]["known_token_subtotal"], 20)

    def test_resume_rejects_rehashed_mutation_of_an_already_receipted_ledger_prefix(self):
        self.plan(); self.enrich_mode = "incomplete"
        self.run_enriched()
        path = self.args.run_dir / "enrichment/ledger.jsonl"
        lines = [builder.parse(raw) for raw in path.read_bytes().splitlines()]
        for line in lines:
            if line["payload"]["event"] == "checkpoint":
                line["payload"]["reason"] = "tamper_run_limit"
        previous = None
        for number, line in enumerate(lines):
            line["previous_sha256"] = previous
            line["sha256"] = builder.fingerprint({"sequence": number, "previous_sha256": previous, "payload": line["payload"]})
            previous = line["sha256"]
        path.write_bytes(b"".join(builder.encoded(line) + b"\n" for line in lines))
        self.args.resume_enrichment = True; self.enrich_mode = "complete"
        blocked = self.run_enriched()
        self.assertIn("prior_ledger_prefix_changed", blocked["enrichment_result"]["error"])
        self.assertEqual(self.counts["enrich"], 1)
        self.assertEqual(self.counts["ask"], 0)

    def test_failed_builder_keeps_unknown_usage_and_cannot_resume_or_admit_reader(self):
        self.plan(); self.enrich_mode = "failed"
        result = self.run_enriched()
        accounting = result["enrichment_result"]["accounting"]
        self.assertTrue(accounting["available"])
        self.assertEqual(accounting["builder"]["summary"]["attempted_calls"], 1)
        self.assertIsNone(accounting["builder"]["total_tokens"])
        self.assertEqual(result["summary"]["failed_cases"], 2)
        self.args.resume_enrichment = True
        self.run_enriched()
        self.assertEqual(self.counts["enrich"], 1)
        self.assertEqual((self.counts["ask"], self.counts["judge"]), (0, 0))

    def test_navigation_report_is_required_even_with_a_published_overlay(self):
        self.plan(); self.navigation_report = False
        result = self.run_enriched()
        self.assertEqual(result["enrichment_result"]["status"], "complete")
        self.assertEqual(result["summary"]["completed_responses"], 0)
        self.assertEqual(self.counts["judge"], 0)
        self.assertFalse(result["summary"]["comparison_eligible"])

    def test_zero_accepted_hints_still_requires_full_raw_plan_and_navigation(self):
        self.plan(); self.enrich_mode = "empty"
        result = self.run_enriched()
        self.assertEqual(result["status"], "completed", result.get("enrichment_result"))
        self.assertEqual(result["enrichment_result"]["report"]["coverage"]["hints"], 0)
        self.assertEqual(result["enrichment_result"]["accounting"]["jev"]["summary"]["attempted_calls"], 0)

    def test_plan_file_tamper_and_wrong_actual_builder_block_reader(self):
        self.plan()
        path = self.args.run_dir / "enrichment/plan.json"
        original = path.read_bytes(); path.write_bytes(original + b" ")
        result = self.run_enriched()
        self.assertEqual(result["summary"]["failed_cases"], 2)
        self.assertEqual(self.counts["enrich"], 0)
        path.write_bytes(original); self.enrich_mode = "wrong_model"
        result = self.run_enriched()
        self.assertEqual(result["summary"]["failed_cases"], 2)
        self.assertEqual((self.counts["ask"], self.counts["judge"]), (0, 0))

    def test_enrich_argv_payload_exclude_questions_gold_and_external_references(self):
        self.plan()
        prepared = builder.read_json(self.args.run_dir / "enrichment/prepared.json")
        payload = system.enrichment_payload(self.args, prepared, resume=False)
        self.assertFalse(set(payload) & {"question", "answer", "answer_format", "evidence_pages", "source_row", "rows"})
        serialized = builder.encoded(payload).decode()
        for row in self.fixture.rows:
            self.assertNotIn(row["question"], serialized)
            self.assertNotIn(row["answer"], serialized)
        for command in self.fixture.seen_arguments:
            if command[1] == "enrich":
                self.assertNotIn("--question", command)
                self.assertNotIn("--benchmark", command)

    def test_rust_float_spelling_is_preserved_for_chain_hashes(self):
        payload = builder.parse(b'{"usage":{"cost":1e-6}}')
        self.assertEqual(builder.encoded(payload), b'{"usage":{"cost":1e-6}}')
        self.assertEqual(builder.fingerprint(payload), hashlib.sha256(b'{"usage":{"cost":1e-6}}').hexdigest())

    def test_original_results_and_adapted_trace_are_distinct_hash_bound_references(self):
        original = self.fixture.root / "original-results.json"; original.write_text('{"aggregate":true}')
        adapted = self.fixture.root / "adapted-summary.json"; adapted.write_text('{"trace":true}')
        self.args.original_pageindex_results, self.args.baseline_summary = original, adapted
        result = self.plan()
        refs = result["external_reference_bindings"]
        self.assertEqual(set(refs), {"original_pageindex_results", "adapted_trace_summary"})
        self.assertNotEqual(refs["original_pageindex_results"]["scope"], refs["adapted_trace_summary"]["scope"])

    def test_max_and_xhigh_profiles_are_explicit_and_must_match_across_roles(self):
        self.args.reasoning_effort = self.args.builder_reasoning_effort = "max"
        planned = self.plan()
        for role in ("builder", "query_planner", "chat", "judge"):
            self.assertEqual(planned["profile"]["roles"][role]["model"], "gpt-6-luna")
            self.assertEqual(planned["profile"]["roles"][role]["reasoning_effort"], "max")
            self.assertEqual(planned["profile"]["roles"][role]["service_tier"], "fast")
        result = self.run_enriched()
        self.assertEqual(result["status"], "completed", result.get("enrichment_result"))
        request = next(builder.read_json(path) for path in (self.args.run_dir / "calls").glob("*.request.json")
                       if builder.read_json(path).get("operation") == "ask")
        self.assertEqual(request["planner_reasoning_effort"], "max")
        self.args.run_dir = self.fixture.root / "xhigh-run"
        self.args.reasoning_effort = self.args.builder_reasoning_effort = self.args.judge_reasoning_effort = "xhigh"
        self.assertEqual(self.plan()["profile"]["roles"]["builder"]["reasoning_effort"], "xhigh")
        self.args.builder_reasoning_effort = "max"
        with self.assertRaisesRegex(ValueError, "profiles_must_match"):
            self.plan()

    def test_new_enriched_runs_reject_old_model_effort_or_implicit_judge(self):
        original = vars(self.args).copy()
        for changed in ({"model": "gpt-5.6-luna"}, {"reasoning_effort": "high"},
                        {"judge_model": None}, {"judge_reasoning_effort": "high"}, {"service_tier": "default"}):
            vars(self.args).clear(); vars(self.args).update(original); vars(self.args).update(changed)
            with self.assertRaisesRegex(ValueError, "enrichment_.*(gpt6|profiles_must_match)"):
                self.plan()
        self.assertEqual((self.counts["index"], self.counts["enrich"], self.counts["ask"], self.counts["judge"]), (0, 0, 0, 0))

    def test_judge_profile_change_requires_new_immutable_run_identity(self):
        planned = self.plan()
        self.assertEqual(planned["enrichment"]["model_policy"], "gpt6_fast_xhigh_or_max_all_model_roles_v1")
        manifest = (self.args.run_dir / "manifest.json").read_bytes()
        self.args.judge_reasoning_effort = "xhigh"
        with self.assertRaisesRegex(ValueError, "conditions changed"):
            self.plan()
        self.assertEqual((self.args.run_dir / "manifest.json").read_bytes(), manifest)
        self.assertEqual((self.counts["enrich"], self.counts["ask"], self.counts["judge"]), (0, 0, 0))

    def test_explicit_capacity_arguments_and_price_card_digest_are_frozen(self):
        self.args.enrich_window_bytes = 32768
        self.args.max_builder_calls = self.args.max_builder_jev_calls = 256
        self.args.enrich_deadline_seconds = 7200
        planned = self.plan()
        self.assertEqual(planned["enrichment"]["window_bytes"], 32768)
        self.assertEqual(planned["enrichment"]["deadline_seconds"], 7200)
        self.assertEqual(planned["price_card"]["sha256"], builder.digest(self.args.price_card))
        command = next(command for command in self.fixture.seen_arguments if command[1] == "enrich")
        self.assertEqual(command[command.index("--window-bytes") + 1], "32768")
        self.assertEqual(command[command.index("--max-builder-calls") + 1], "256")
        self.args.price_card.write_bytes(self.args.price_card.read_bytes() + b" ")
        with self.assertRaisesRegex(ValueError, "conditions changed"):
            self.run_enriched()
        self.assertEqual(self.counts["enrich"], 0)


class PriceProjectionTests(unittest.TestCase):
    def setUp(self):
        self.card = builder.validate_price_card(synthetic_price_card())
        self.record = {"role": "final_reader", "model": "gpt-6-luna", "thread_id": "synthetic-thread",
                       "turn_id": "synthetic-turn", "effective_service_tier": "priority", "accounting_complete": True,
                       "usage": {"total": {"inputTokens": 1000, "cachedInputTokens": 200,
                                           "cacheWriteInputTokens": 50, "outputTokens": 100},
                                 "last": {"inputTokens": 999999}, "modelContextWindow": 999999}}

    def test_standard_normalization_and_effective_fast_price_keep_billing_unobserved(self):
        fast = builder.price_model_turns([self.record], price_card=self.card)
        standard = builder.price_model_turns([self.record], price_card=self.card, normalization="standard")
        self.assertAlmostEqual(fast["total_usd_equivalent"], .002665)
        self.assertAlmostEqual(standard["total_usd_equivalent"], .0013325)
        self.assertIsNone(standard["actual_billing_usd"])
        self.assertEqual(standard["steps"][0]["effective_service_tier"], "priority")
        self.assertEqual(standard["steps"][0]["rate_tier"], "standard")

    def test_missing_tier_cache_write_unknown_band_or_empty_turns_never_price_as_zero(self):
        for change in ("tier", "cache_write", "long_aggregate", "unknown_model", "partial"):
            record = copy.deepcopy(self.record)
            if change == "tier": record["effective_service_tier"] = None
            if change == "cache_write": del record["usage"]["total"]["cacheWriteInputTokens"]
            if change == "long_aggregate": record["usage"]["total"]["inputTokens"] = 272001
            if change == "unknown_model": record["model"] = "unpriced-model"
            if change == "partial": record["accounting_complete"] = False
            priced = builder.price_model_turns([record], price_card=self.card, normalization="standard")
            self.assertFalse(priced["complete"], change)
            self.assertIsNone(priced["total_usd_equivalent"], change)
        empty = builder.price_model_turns([], price_card=self.card, normalization="standard")
        self.assertFalse(empty["complete"])
        self.assertIsNone(empty["total_usd_equivalent"])

    def test_actual_turn_duplicates_count_once_but_conflicts_and_missing_roles_block(self):
        duplicate = builder.price_model_turns([self.record, copy.deepcopy(self.record)], price_card=self.card)
        self.assertEqual(duplicate["distinct_observed_turns"], 1)
        self.assertAlmostEqual(duplicate["total_usd_equivalent"], .002665)
        changed = copy.deepcopy(self.record); changed["usage"]["total"]["outputTokens"] += 1
        with self.assertRaisesRegex(ValueError, "conflicting_model_turn"):
            builder.price_model_turns([self.record, changed], price_card=self.card)
        missing = builder.price_model_turns([self.record], price_card=self.card,
                    expected_roles={"query_planner": 1, "final_reader": 1})
        self.assertFalse(missing["expected_role_coverage_complete"])
        self.assertIsNone(missing["total_usd_equivalent"])

    def test_observed_jev_receipts_are_required_and_never_filled_from_page_rate(self):
        model = builder.price_model_turns([self.record], price_card=self.card, normalization="standard")
        unknown = builder.combined_model_jev_cost(model, {"attempted_calls": 1, "accounting_complete": False,
            "measured_cost_usd": None, "known_cost_subtotal_usd": None}, 1)
        self.assertFalse(unknown["complete"])
        self.assertIsNone(unknown["per_question_usd_equivalent"])
        known = builder.combined_model_jev_cost(model, {"attempted_calls": 1, "accounting_complete": True,
            "measured_cost_usd": .0001, "known_cost_subtotal_usd": .0001}, 1)
        self.assertAlmostEqual(known["per_question_usd_equivalent"], .0014325)
        self.assertFalse(known["jev_rate_filled_missing_receipt"])

    def test_dual_gate_is_strict_qa_only_and_requires_full62_model_coverage(self):
        summary = {"question_denominator": 62, "comparison_eligible": True, "correct": 61}
        cost = {"complete": True, "question_denominator": 62, "per_question_usd_equivalent_decimal": "0.003607",
                "model_api_equivalent": {"expected_roles": {"query_planner": 62, "final_reader": 62}, "expected_role_coverage_complete": True,
                                         "normalization": "standard_api_prices"}}
        self.assertFalse(builder.qa_dual_gate_observation(summary, cost)["joint_conditions_observed"])
        cost["per_question_usd_equivalent_decimal"] = "0.003606999"
        self.assertTrue(builder.qa_dual_gate_observation(summary, cost)["joint_conditions_observed"])
        cost["model_api_equivalent"]["expected_role_coverage_complete"] = False
        self.assertIsNone(builder.qa_dual_gate_observation(summary, cost)["qa_cost_met"])
        self.assertFalse(builder.qa_dual_gate_observation({**summary, "correct": 60}, cost)["quality_met"])

    def test_original_index_partial_costs_remain_separate_and_unknown(self):
        result = builder.original_index_costs([
            {"doc_id": "a", "flash_index_cost_usd": .25, "questions": ["must not leave metadata"]},
            {"doc_id": "b", "flash_index_cost_usd": None}], ["a", "b"])
        self.assertEqual(result["known_index_cost_subtotal_usd"], .25)
        self.assertEqual(result["unknown_cost_documents"], 1)
        self.assertIsNone(result["complete_index_cost_usd"])
        self.assertNotIn("questions", json.dumps(result))


if __name__ == "__main__":
    unittest.main()
