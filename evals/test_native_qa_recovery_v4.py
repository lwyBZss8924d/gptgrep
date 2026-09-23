"""Synthetic metadata-only v4 QA recovery plans; no process/model calls."""
from __future__ import annotations
import copy
import fcntl
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/pageindex_baseline"))
import native_qa_recovery_v4 as recovery


def put(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(recovery.encoded(value) + b"\n")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def model(role, status, *, started=False, cause=None):
    return {"attempt_id": role + "-1", "role": role, "status": status,
            "requested_model": "gpt-6-luna", "requested_reasoning_effort": "xhigh", "requested_service_tier": "fast",
            "model": "gpt-6-luna" if started else None, "model_provider": "openai" if started else None,
            "thread_id": role + "-thread" if started else None, "turn_id": role + "-turn" if started else None,
            "effective_service_tier": "priority" if started else None, "usage": None,
            "accounting_complete": status == "completed", "error": cause}


def index_report(*, missing_tokens=True, missing_cost=False, model_complete=True, calls=200):
    model_cost = {"complete": model_complete, "total_usd_equivalent_decimal": "1.25" if model_complete else None}
    jev = {"summary": {"attempted_calls": calls, "completed_calls": calls, "failed_calls": 0,
                        "unobserved_calls": 0, "missing_total_tokens": calls if missing_tokens else 0},
           "accounting_complete": not missing_tokens, "missing_cost_calls": 1 if missing_cost else 0,
           "known_cost_subtotal_usd": .070038822 if calls else None,
           "measured_cost_usd": None if missing_cost or not calls else .070038822}
    return {"enrichment_result": {"status": "complete", "accounting": {"available": True, "validated_chain": True,
             "builder": {"api_price_equivalent": copy.deepcopy(model_cost), "standard_normalized_api_price_equivalent": copy.deepcopy(model_cost)},
             "jev": jev}}}


class Fixture:
    def __init__(self, root):
        self.root = root.resolve()
        self.origin = self.root / "synthetic-gpt6-origin"
        self.origin.mkdir()
        (self.origin / ".owner.lock").touch()
        corpus = self.origin / "corpus"
        corpus.mkdir()
        raw = corpus / "synthetic.txt"
        raw.write_bytes(b"Only invented source material.\n")
        doc_id = hashlib.sha256(raw.name.encode()).hexdigest()[:24]
        generation = corpus / ".gptgrep/generations/g-synthetic"
        text = generation / "text" / (doc_id + ".txt")
        text.parent.mkdir(parents=True)
        text.write_bytes(raw.read_bytes())
        put(generation / "manifest.json", {"documents": [{"path": raw.name, "id": doc_id,
            "source_sha256": digest(raw), "text_sha256": digest(text), "nodes": [{"id": "root"}]}]})
        self.source = {"schema_version": "gptgrep.v1", "generation": "g-synthetic", "manifest_sha256": digest(generation / "manifest.json")}
        put(corpus / ".gptgrep/CURRENT.json", self.source)
        put(self.origin / "build.json", {"status": "completed", "generation_binding": self.source})
        self.seal = self.root / "synthetic-seal"
        self.seal.mkdir()
        (self.seal / "gptgrep").write_bytes(b"synthetic native binary; never executed")
        runner = self.seal / "runner/scripts/gptgrep_system_eval.py"
        runner.parent.mkdir(parents=True)
        runner.write_text("# synthetic frozen runner; never imported\n")
        self.judge = self.root / "synthetic-judge"
        self.judge.write_bytes(b"synthetic original judge; never executed")
        self.card = self.root / "synthetic-price-card.json"
        put(self.card, {"synthetic": True})
        put(self.seal / "source.json", {"source_revision": "synthetic-frozen-revision",
            "binary_sha256": digest(self.seal / "gptgrep"),
            "frozen_runner_files": {"scripts/gptgrep_system_eval.py": digest(runner)}})
        same = {"model": "gpt-6-luna", "reasoning_effort": "xhigh", "service_tier": "fast"}
        self.manifest = {"schema_version": "gptgrep.system-eval.v4", "question_count": 62, "source_rows": list(range(62)),
            "document_count": 1, "source_hashes": {raw.name: digest(raw)}, "max_host_invocations": 256,
            "binary_sha256": digest(self.seal / "gptgrep"), "judge_binary_sha256": digest(self.judge),
            "runner_sha256": digest(runner), "adapter_files": {},
            "price_card": {"path": str(self.card), "sha256": digest(self.card)},
            "profile": {"roles": {"chat": same, "builder": same, "query_planner": same,
                "judge": {"model": "gpt-5.6-luna", "reasoning_effort": "high", "service_tier": "fast"}}}}
        put(self.origin / "manifest.json", self.manifest)
        artifact = corpus / ".gptgrep/navigation-overlays" / (hashlib.sha256(b"synthetic overlay").hexdigest() + ".json")
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"synthetic overlay")
        self.publication = {"schema_version": "gptgrep.navigation-overlay.v1", "generation": self.source["generation"],
                            "manifest_sha256": self.source["manifest_sha256"], "artifact_sha256": digest(artifact)}
        put(corpus / ".gptgrep/NAVIGATION.json", self.publication)
        plan_sha = hashlib.sha256(b"synthetic full plan semantics").hexdigest()
        put(self.origin / "enrichment/plan.json", {"synthetic_full_plan": True, "plan_sha256": plan_sha})
        (self.origin / "enrichment/ledger.jsonl").write_bytes(b"synthetic frozen builder ledger\n")
        binding = recovery.fingerprint(self.manifest)
        put(self.origin / "enrichment/prepared.json", {"run_binding": binding, "generation_binding": self.source,
            "plan_file_sha256": digest(self.origin / "enrichment/plan.json"), "plan_sha256": plan_sha})
        put(self.origin / "enrichment/ready.json", {"run_binding": binding, "publication": self.publication,
            "plan_file_sha256": digest(self.origin / "enrichment/plan.json"), "plan_sha256": plan_sha,
            "ledger_sha256": digest(self.origin / "enrichment/ledger.jsonl"),
            "ledger_bytes": (self.origin / "enrichment/ledger.jsonl").stat().st_size})
        self.receipts, self.cases = [], []
        self.call(1, "native_enrich", "enrichment:native", "completed", {"status": "complete"})
        cause = {"kind": "failed_turn", "codex_error_info": "responseStreamConnectionFailed",
                 "http_status_code": 503, "will_retry": None}
        for row in range(62):
            ordinal = row + 2
            case = {"source_row": row, "doc_id": raw.name, "case_identity": f"synthetic-case-{row}",
                    "status": "failed" if row in (0, 1, 2, 3, 5, 6, 7) else "completed",
                    "judge": {"status": "not_started" if row in (0, 1, 2, 3, 5, 6, 7) else "completed", "equivalent": row != 4}}
            planner = model("query_planner", "completed", started=True)
            if row == 0:
                snapshots = [[model("query_planner", "reserved")], [model("query_planner", "failed")]]
                code, stage = "host_query_plan_failed", "query_plan"
            elif row == 1:
                snapshots = [[planner, model("final_reader", "reserved")], [planner, model("final_reader", "failed")]]
                code, stage = "host_codex_failed", "codex"
            elif row in (2, 5, 6, 7):
                snapshots = [[planner, model("final_reader", "running", started=True)]]
                if row == 6:
                    snapshots.append([planner, model("final_reader", "process_completed", started=True)])
                snapshots.append([planner, model("final_reader", "failed", started=True, cause=cause if row in (6, 7) else None)])
                code, stage = ("host_jev_search_failed" if row == 2 else "host_codex_failed"), "codex"
            else:
                snapshots = []
                code = stage = None
            if snapshots:
                pid, began = 10000 + ordinal, 1_000_000 + ordinal * 1000
                ledger = corpus / ".gptgrep/host-attempts" / f"attempt-{began + 10}-{pid}-0.jsonl"
                ledger.parent.mkdir(parents=True, exist_ok=True)
                workflow = {"generation": self.source["generation"], "document_scope": raw.name,
                            "query_sha256": hashlib.sha256(f"opaque-query-digest-{row}".encode()).hexdigest()}
                events = [{"schema_version": "gptgrep.jev-attempt.v1", "event": "workflow_bound", "generation": self.source["generation"], "workflow": workflow, "model_attempts": []}]
                for records in snapshots:
                    events.append({"schema_version": "gptgrep.jev-attempt.v1", "event": "model_attempt_updated",
                                   "generation": self.source["generation"], "workflow": workflow, "model_attempts": records})
                if row == 2:
                    events.append({**events[-1], "event": "search_failed", "search": {"status": "failed"}})
                events.append({**events[-1], "event": "failed"})
                ledger.write_bytes(b"".join(recovery.encoded(event) + b"\n" for event in events))
                navigation = {**self.publication, "schema_version": "gptgrep.navigation-query.v1", "status": "completed",
                              "hint_scan_complete": True, "document_scope": raw.name}
                failure = {"code": code, "stage": stage, "generation": self.source["generation"], "ledger_path": str(ledger),
                           "model_attempts": snapshots[-1], "cause": cause if row in (6, 7) else None, "navigation": navigation}
                report = {"code": code, "host_retrieval": failure}
                receipt = self.call(ordinal, "native_ask", f"answer:native:row-{row}", "failed", report,
                                    host_pid=pid, process_started_unix_ns=began, process_finished_unix_ns=began + 100)
            else:
                report = {"status": "insufficient_evidence" if row == 3 else "completed", "answer": "Synthetic retained answer; never inspected."}
                receipt = self.call(ordinal, "native_ask", f"answer:native:row-{row}", "failed" if row == 3 else "completed", report)
            case["host_receipt"] = receipt
            self.cases.append(case)
            put(self.origin / "cases" / f"{row:03d}" / "case.json", case)
        for case in self.cases:
            if case["judge"]["status"] == "completed":
                self.call(len(self.receipts) + 1, "completion", f"judge:native:row-{case['source_row']}", "completed", {"status": "completed"})
        self.write_receipts()
        self.summary = {"status": "incomplete", "host_invocations": len(self.receipts), "cases": copy.deepcopy(self.cases), **index_report()}
        # Frozen summary carries derived fields not rewritten into case.json.
        for case in self.summary["cases"]:
            case["cumulative_reader_wall_ms"] = 1
        self.refresh_enrichment_snapshot()
        self.declaration = self.root / "recovery-declaration.json"
        self.refresh_declaration()
        self.inputs = recovery.Inputs(self.origin, self.seal, self.declaration, self.judge)

    def call(self, ordinal, operation, phase, status, report, **extra):
        base = self.origin / "calls" / f"{ordinal:05d}"
        base.parent.mkdir(exist_ok=True)
        request = Path(str(base) + ".request.json")
        # Deliberately not JSON: the recovery planner only hashes request bytes.
        request.write_bytes(f"opaque synthetic request {ordinal}".encode())
        response = Path(str(base) + ".response.json")
        put(response, report)
        receipt = {"ordinal": ordinal, "operation": operation, "phase": phase, "status": status,
                   "request_sha256": digest(request), "response_sha256": digest(response), **extra}
        self.receipts.append(receipt)
        return receipt

    def write_receipts(self):
        (self.origin / "host-calls.jsonl").write_bytes(b"".join(recovery.encoded(receipt) + b"\n" for receipt in self.receipts))

    def refresh_declaration(self):
        publication = recovery.read_json(self.origin / "corpus/.gptgrep/NAVIGATION.json")
        files = ["enrichment/prepared.json", "enrichment/ready.json", "enrichment/plan.json", "enrichment/ledger.jsonl",
                 "corpus/.gptgrep/NAVIGATION.json", "corpus/.gptgrep/navigation-overlays/" + publication["artifact_sha256"] + ".json"]
        put(self.declaration, {"schema_version": "gptgrep.v4-qa-recovery-declaration.v1",
            "origin_manifest_sha256": digest(self.origin / "manifest.json"), "origin_summary_sha256": digest(self.origin / "summary.json"),
            "origin_host_calls_sha256": digest(self.origin / "host-calls.jsonl"),
            "enrichment_file_sha256": {name: digest(self.origin / name) for name in files},
            "seal_source_sha256": digest(self.seal / "source.json"), "max_outer_invocations": 256, "price_card_sha256": digest(self.card)})

    def refresh_enrichment_snapshot(self):
        enriched = self.summary["enrichment_result"]
        enriched.update(prepared=recovery.read_json(self.origin / "enrichment/prepared.json"),
                        ready=recovery.read_json(self.origin / "enrichment/ready.json"), reader_admitted=True,
                        report={"publication": recovery.read_json(self.origin / "corpus/.gptgrep/NAVIGATION.json")})
        enriched["accounting"].update(ledger_sha256=digest(self.origin / "enrichment/ledger.jsonl"),
                                      ledger_bytes=(self.origin / "enrichment/ledger.jsonl").stat().st_size)
        self.summary["reader_binding"] = copy.deepcopy(enriched["ready"])
        put(self.origin / "summary.json", self.summary)


class RecoveryPlanTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.fixture = Fixture(Path(self.temporary.name))

    def test_all62_retained_with_only_proven_pre_final_candidates(self):
        result = recovery.audit(self.fixture.inputs)
        self.assertEqual(result["question_denominator"], 62)
        self.assertEqual([item["source_row"] for item in result["selected"]], [0, 1, 2, 7])
        by_row = {item["source_row"]: item for item in result["attempt_decisions"]}
        self.assertFalse(by_row[3]["eligible"])
        self.assertTrue(by_row[3]["semantic_disposition"])
        self.assertFalse(by_row[3]["cost_policy"]["exclude_from_experimental_comparison_cost"])
        self.assertFalse(by_row[4]["eligible"])  # Wrong completed answer is retained.
        self.assertFalse(by_row[5]["eligible"])  # Started reader, generic error.
        self.assertFalse(by_row[6]["eligible"])  # Earlier process_completed wins.
        self.assertTrue(by_row[7]["cost_policy"]["exclude_from_experimental_comparison_cost"])
        self.assertFalse(by_row[2]["cost_policy"]["exclude_from_experimental_comparison_cost"])
        self.assertEqual(result["new_model_calls"], 0)

    def test_plan_is_atomic_idempotent_and_never_mutates_origin(self):
        before = recovery.retained.inventory(self.fixture.origin)
        first = recovery.prepare_plan(self.fixture.inputs)
        self.assertEqual(first, recovery.prepare_plan(self.fixture.inputs))
        self.assertEqual(before, recovery.retained.inventory(self.fixture.origin))
        plan = recovery.read_json(Path(first["path"]))
        self.assertFalse(plan["executor_contract"]["implemented"])
        self.assertFalse(plan["executor_contract"]["plan_is_execution_authorization"])
        self.assertEqual(plan["executor_contract"]["maximum_new_outer_calls"], 8)
        self.assertNotIn("Synthetic retained answer", json.dumps(plan))

    def test_live_origin_lock_blocks_before_any_sidecar_creation(self):
        with (self.fixture.origin / ".owner.lock").open("rb") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(ValueError, "still_owned_or_live"):
                recovery.prepare_plan(self.fixture.inputs)
        self.assertFalse(self.fixture.origin.with_name(self.fixture.origin.name + "-v4-qa-recovery").exists())

    def test_gpt56_and_high_are_out_of_recovery_scope(self):
        for model_name, effort in (("gpt-5.6-luna", "xhigh"), ("gpt-6-luna", "high")):
            manifest = copy.deepcopy(self.fixture.manifest)
            manifest["profile"]["roles"]["chat"] = {"model": model_name, "reasoning_effort": effort, "service_tier": "fast"}
            put(self.fixture.origin / "manifest.json", manifest)
            self.fixture.refresh_declaration()
            with self.assertRaisesRegex(ValueError, "only_gpt6_fast_xhigh_or_max"):
                recovery.audit(self.fixture.inputs)

    def test_pending_reservation_and_nonterminal_summary_cannot_be_planned(self):
        path = self.fixture.origin / "calls" / f"{len(self.fixture.receipts)+1:05d}.request.json"
        path.write_bytes(b"unreceipted reservation")
        with self.assertRaisesRegex(ValueError, "unfinished_outer"):
            recovery.audit(self.fixture.inputs)
        path.unlink()
        summary = copy.deepcopy(self.fixture.summary); summary["status"] = "running"
        put(self.fixture.origin / "summary.json", summary); self.fixture.refresh_declaration()
        with self.assertRaisesRegex(ValueError, "terminal_full_case"):
            recovery.audit(self.fixture.inputs)

    def test_receipt_table_and_final_case_receipt_are_pinned(self):
        receipts = self.fixture.origin / "host-calls.jsonl"
        original = receipts.read_bytes()
        receipts.write_bytes(original + b"\n")
        with self.assertRaisesRegex(ValueError, "origin_freeze_changed"):
            recovery.audit(self.fixture.inputs)
        receipts.write_bytes(original)
        path = self.fixture.origin / "cases/000/case.json"
        case = recovery.read_json(path)
        case["host_receipt"]["elapsed_ms"] = 7
        put(path, case)
        self.fixture.summary["cases"][0]["host_receipt"] = case["host_receipt"]
        put(self.fixture.origin / "summary.json", self.fixture.summary)
        self.fixture.refresh_declaration()
        with self.assertRaisesRegex(ValueError, "case_receipt_unbound"):
            recovery.audit(self.fixture.inputs)

    def test_changed_source_overlay_runner_and_price_card_are_rejected(self):
        targets = [self.fixture.origin / "corpus/synthetic.txt", self.fixture.origin / "corpus/.gptgrep/NAVIGATION.json",
                   self.fixture.seal / "runner/scripts/gptgrep_system_eval.py", self.fixture.card]
        for path in targets:
            before = path.read_bytes()
            path.write_bytes(before + b"changed")
            with self.assertRaises((ValueError, json.JSONDecodeError)):
                recovery.audit(self.fixture.inputs)
            path.write_bytes(before)

    def test_coordinated_enrichment_tamper_cannot_rebind_frozen_snapshot(self):
        for target in ("ledger", "plan", "publication"):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                fixture = Fixture(Path(temporary))
                before_summary = (fixture.origin / "summary.json").read_bytes()
                before_declaration = fixture.declaration.read_bytes()
                ready_path, prepared_path = fixture.origin / "enrichment/ready.json", fixture.origin / "enrichment/prepared.json"
                ready, prepared = recovery.read_json(ready_path), recovery.read_json(prepared_path)
                if target == "ledger":
                    ledger = fixture.origin / "enrichment/ledger.jsonl"
                    ledger.write_bytes(ledger.read_bytes() + b"coordinated replacement\n")
                    ready.update(ledger_sha256=digest(ledger), ledger_bytes=ledger.stat().st_size)
                elif target == "plan":
                    plan = fixture.origin / "enrichment/plan.json"
                    put(plan, {"synthetic_full_plan": False, "plan_sha256": hashlib.sha256(b"changed semantics").hexdigest()})
                    prepared["plan_file_sha256"] = ready["plan_file_sha256"] = digest(plan)
                    prepared["plan_sha256"] = ready["plan_sha256"] = recovery.read_json(plan)["plan_sha256"]
                else:
                    raw = b"coordinated overlay replacement"
                    replacement = hashlib.sha256(raw).hexdigest()
                    (fixture.origin / "corpus/.gptgrep/navigation-overlays" / (replacement + ".json")).write_bytes(raw)
                    ready["publication"]["artifact_sha256"] = replacement
                    put(fixture.origin / "corpus/.gptgrep/NAVIGATION.json", ready["publication"])
                put(ready_path, ready); put(prepared_path, prepared)
                with self.assertRaisesRegex(ValueError, "frozen_enrichment"):
                    recovery.audit(fixture.inputs)
                self.assertEqual((fixture.origin / "summary.json").read_bytes(), before_summary)
                self.assertEqual(fixture.declaration.read_bytes(), before_declaration)

    def test_frozen_enrichment_file_bytes_and_snapshot_presence_are_required(self):
        prepared = self.fixture.origin / "enrichment/prepared.json"
        original = prepared.read_bytes()
        prepared.write_bytes(original + b" ")  # Same JSON value is still different frozen evidence.
        with self.assertRaisesRegex(ValueError, "frozen_enrichment_file_digest"):
            recovery.audit(self.fixture.inputs)
        prepared.write_bytes(original)
        self.fixture.summary["enrichment_result"].pop("prepared")
        put(self.fixture.origin / "summary.json", self.fixture.summary)
        self.fixture.refresh_declaration()
        with self.assertRaisesRegex(ValueError, "frozen_enrichment_snapshot"):
            recovery.audit(self.fixture.inputs)

    def test_request_bytes_and_ledger_identity_are_bound_without_request_parsing(self):
        self.assertTrue(recovery.audit(self.fixture.inputs)["selected"])
        path = self.fixture.origin / "calls/00002.request.json"
        path.write_bytes(b"changed opaque bytes")
        with self.assertRaisesRegex(ValueError, "request_bytes_changed"):
            recovery.audit(self.fixture.inputs)

    def test_torn_or_unbound_failure_timeline_is_not_absence_proof(self):
        response = recovery.read_json(self.fixture.origin / "calls/00003.response.json")
        ledger = Path(response["host_retrieval"]["ledger_path"])
        with ledger.open("ab") as stream:
            stream.write(b'{"torn"')
        with self.assertRaisesRegex(ValueError, "ledger_incomplete"):
            recovery.audit(self.fixture.inputs)

    def test_remaining_cap_is_cumulative_and_cannot_reset(self):
        manifest = copy.deepcopy(self.fixture.manifest)
        manifest["max_host_invocations"] = len(self.fixture.receipts) + 7
        put(self.fixture.origin / "manifest.json", manifest)
        self.fixture.refresh_declaration()
        declaration = recovery.read_json(self.fixture.declaration)
        declaration["max_outer_invocations"] = manifest["max_host_invocations"]
        put(self.fixture.declaration, declaration)
        # Update synthetic preparation binding to isolate the remaining-cap check.
        for name in ("prepared", "ready"):
            path = self.fixture.origin / "enrichment" / (name + ".json")
            value = recovery.read_json(path); value["run_binding"] = recovery.fingerprint(manifest); put(path, value)
        self.fixture.refresh_enrichment_snapshot()
        self.fixture.refresh_declaration()
        declaration = recovery.read_json(self.fixture.declaration)
        declaration["max_outer_invocations"] = manifest["max_host_invocations"]
        put(self.fixture.declaration, declaration)
        with self.assertRaisesRegex(ValueError, "remaining_budget_insufficient"):
            recovery.audit(self.fixture.inputs)

    def test_scores_are_not_selection_inputs(self):
        before = [item["source_row"] for item in recovery.audit(self.fixture.inputs)["selected"]]
        for row, case in enumerate(self.fixture.summary["cases"]):
            case["judge"]["equivalent"] = not case["judge"]["equivalent"]
            physical = recovery.read_json(self.fixture.origin / "cases" / f"{row:03d}" / "case.json")
            physical["judge"]["equivalent"] = case["judge"]["equivalent"]
            put(self.fixture.origin / "cases" / f"{row:03d}" / "case.json", physical)
        put(self.fixture.origin / "summary.json", self.fixture.summary); self.fixture.refresh_declaration()
        self.assertEqual(before, [item["source_row"] for item in recovery.audit(self.fixture.inputs)["selected"]])


class CostPolicyTests(unittest.TestCase):
    def test_typed_remote_only_and_semantic_abstention_always_retained(self):
        remote = {"kind": "failed_turn", "codex_error_info": "httpConnectionFailed", "http_status_code": 503, "will_retry": None}
        self.assertTrue(recovery.classify_cost(remote, semantic_disposition=False, pre_final_proven=True)["exclude_from_experimental_comparison_cost"])
        for cause in (None, {**remote, "http_status_code": 401}, {**remote, "codex_error_info": "badRequest"}, {**remote, "will_retry": True}):
            self.assertFalse(recovery.classify_cost(cause, semantic_disposition=False, pre_final_proven=True)["exclude_from_experimental_comparison_cost"])
        self.assertFalse(recovery.classify_cost(remote, semantic_disposition=True, pre_final_proven=True)["exclude_from_experimental_comparison_cost"])
        self.assertFalse(recovery.classify_cost(remote, semantic_disposition=False, pre_final_proven=False)["exclude_from_experimental_comparison_cost"])
        with self.assertRaisesRegex(ValueError, "retry_metadata_invalid"):
            recovery._typed_cause({**remote, "will_retry": 1})

    def test_complete_jev_usd_does_not_require_token_completeness_for_index_cost(self):
        source = index_report(missing_tokens=True)
        before = copy.deepcopy(source)
        view = recovery.index_cost_view(source)
        self.assertTrue(view["jev_cost_receipts_complete"])
        self.assertFalse(view["jev_token_accounting_complete"])
        self.assertAlmostEqual(view["projections"]["standard_normalized"]["index_total_usd_equivalent"], 1.320038822)
        self.assertEqual(source, before)
        self.assertIn("excluded", view["g5_effect"])

    def test_missing_cost_unknown_model_or_partial_index_remains_unavailable(self):
        for source in (index_report(missing_cost=True), index_report(model_complete=False)):
            self.assertIsNone(recovery.index_cost_view(source)["projections"]["effective_tier"]["index_total_usd_equivalent"])
        source = index_report(); source["enrichment_result"]["status"] = "incomplete"
        self.assertIsNone(recovery.index_cost_view(source)["projections"]["effective_tier"]["index_total_usd_equivalent"])

    def test_zero_jev_calls_are_known_zero_but_absent_accounting_is_not(self):
        empty = recovery.index_cost_view(index_report(calls=0))
        self.assertTrue(empty["jev_cost_receipts_complete"])
        self.assertEqual(empty["projections"]["effective_tier"]["index_total_usd_equivalent"], 1.25)
        self.assertFalse(recovery.index_cost_view({})["jev_cost_receipts_complete"])


if __name__ == "__main__":
    unittest.main()
