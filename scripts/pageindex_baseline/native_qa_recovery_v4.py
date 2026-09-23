"""Zero-model, metadata-only v4 QA recovery audit and immutable plan.

This module has no executor, model transport, benchmark loader, or judge. It
never reads request JSON fields or answer/gold text. It hashes original files
and examines only terminal status, identity, protocol and accounting metadata.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile

sys.dont_write_bytecode = True
import native_recovery as retained

SCHEMA = "gptgrep.v4-qa-recovery.v1"
POLICY = "gpt6_pre_final_technical_v1"
MAX_JSON = 32 * 1024 * 1024
ALLOWED_CODES = {"host_codex_failed", "host_query_plan_failed", "host_jev_search_failed",
                 "host_jev_initial_timeout", "host_navigation_failed", "host_navigation_timeout"}
REMOTE_INFO = {"rateLimitExceeded", "serverOverloaded", "httpConnectionFailed",
               "responseStreamConnectionFailed", "internalServerError", "responseStreamDisconnected",
               "responseTooManyFailedAttempts"}
TERMINAL_PROTOCOL_KINDS = {"terminal_error", "failed_turn", "interrupted_turn"}
NONRECOVERABLE_INFO = {"contextWindowExceeded", "sessionBudgetExceeded", "usageLimitExceeded",
                       "cyberPolicy", "misalignmentPolicyViolation", "unauthorized", "badRequest",
                       "threadRollbackFailed", "sandboxError", "activeTurnNotSteerable"}
MODEL_STATES = {"reserved", "running", "process_completed", "completed", "failed", "interrupted"}


def require(value, code):
    if not value:
        raise ValueError(code)


def encoded(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()


def fingerprint(value):
    return hashlib.sha256(encoded(value)).hexdigest()


def sha(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value), "qa_recovery_invalid_digest")
    return value


def read_json(path, cap=MAX_JSON):
    return retained.read_json(path, cap)


def profile(value, *, reader=False):
    require(isinstance(value, dict) and set(value) == {"model", "reasoning_effort", "service_tier"}, "qa_recovery_profile_invalid")
    if reader:
        require(value["model"] == "gpt-6-luna" and value["reasoning_effort"] in ("xhigh", "max")
                and value["service_tier"] == "fast", "qa_recovery_only_gpt6_fast_xhigh_or_max_admitted")
    return value


@dataclass(frozen=True)
class Inputs:
    origin: Path
    seal: Path
    declaration: Path
    judge_binary: Path


@contextmanager
def terminal_owner(origin):
    """Acquire the existing origin lock read-only before inspecting run files."""
    origin = Path(origin).resolve()
    fd = os.open(retained.regular(origin / ".owner.lock"), os.O_RDONLY | os.O_NOFOLLOW)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ValueError("qa_recovery_origin_still_owned_or_live") from error
        yield origin
    finally:
        os.close(fd)


def _typed_cause(value):
    if not isinstance(value, dict) or value.get("kind") not in TERMINAL_PROTOCOL_KINDS | {"tool_budget_exhausted", "identity_mismatch", "malformed_error", "invalid_turn_status"}:
        return None
    info, status = value.get("codex_error_info"), value.get("http_status_code")
    require(info is None or isinstance(info, str), "qa_recovery_typed_error_invalid")
    require(status is None or type(status) is int and 100 <= status <= 599, "qa_recovery_typed_http_status_invalid")
    require(value.get("will_retry") is None or type(value["will_retry"]) is bool, "qa_recovery_retry_metadata_invalid")
    return {key: value.get(key) for key in ("kind", "codex_error_info", "http_status_code", "will_retry")}


def classify_cost(cause, *, semantic_disposition, pre_final_proven):
    """Never infer transport cause from generic codes, messages or absent usage."""
    result = {"policy": "typed_communication_remote_only_v1", "classification": "unclassified",
              "exclude_from_experimental_comparison_cost": False,
              "retain_all_attempt_usage_and_unknown_billing": True, "typed_cause": cause}
    if semantic_disposition:
        return {**result, "classification": "semantic_or_completed_disposition", "reason": "final_output_is_not_a_transport_failure"}
    if cause is None or cause["kind"] not in TERMINAL_PROTOCOL_KINDS or cause["will_retry"] is True:
        return {**result, "reason": "no_terminal_typed_communication_remote_proof"}
    info, status = cause["codex_error_info"], cause["http_status_code"]
    typed_remote = info in REMOTE_INFO
    if status is not None and 400 <= status < 500 and status not in (408, 429):
        typed_remote = False
    if not typed_remote:
        return {**result, "classification": "other_typed_failure", "reason": "typed_failure_is_not_admitted_remote_communication"}
    return {**result, "classification": "typed_communication_remote_failure",
            "exclude_from_experimental_comparison_cost": pre_final_proven,
            "reason": "typed_pre_final_failure" if pre_final_proven else "final_disposition_not_proven"}


def _ledger_metadata(origin, receipt, failure, case, source, publication, manifest):
    path = Path(failure.get("ledger_path", ""))
    require(path.parent == origin / "corpus/.gptgrep/host-attempts"
            and retained.NATIVE_LEDGER.fullmatch(path.name), "qa_recovery_ledger_path_invalid")
    parts = path.stem.split("-")
    pid = receipt.get("host_pid")
    require(type(pid) is int and pid > 1 and int(parts[2]) == pid, "qa_recovery_ledger_process_unbound")
    began, ended = receipt.get("process_started_unix_ns"), receipt.get("process_finished_unix_ns")
    require(type(began) is int and type(ended) is int and began <= int(parts[1]) <= ended, "qa_recovery_ledger_time_unbound")
    raw = retained.regular(path).read_bytes()
    require(len(raw) <= MAX_JSON and raw.endswith(b"\n"), "qa_recovery_ledger_incomplete")
    events = retained.read_events(path)
    require(events and len(events) <= 1024 and events[-1].get("event") == "failed", "qa_recovery_terminal_failure_ledger_missing")
    workflow = None
    snapshots, reader_snapshots = [], []
    fatal_search = False
    for event in events:
        require(event.get("schema_version") == "gptgrep.jev-attempt.v1" and event.get("generation") == source["generation"], "qa_recovery_ledger_generation_changed")
        value = event.get("workflow")
        if value is not None:
            require(isinstance(value, dict) and value.get("generation") == source["generation"]
                    and value.get("document_scope") == case["doc_id"], "qa_recovery_workflow_scope_changed")
            sha(value.get("query_sha256"))
            require(workflow is None or workflow == value, "qa_recovery_workflow_changed")
            workflow = value
        records = event.get("model_attempts", [])
        require(isinstance(records, list) and len(records) <= 2, "qa_recovery_model_snapshots_invalid")
        if records:
            require(workflow is not None and value == workflow, "qa_recovery_model_before_workflow_binding")
        roles = set()
        for record in records:
            role = record.get("role")
            require(role in ("query_planner", "final_reader") and role not in roles and record.get("status") in MODEL_STATES, "qa_recovery_model_role_invalid")
            roles.add(role)
            expected = manifest["profile"]["roles"]["query_planner" if role == "query_planner" else "chat"]
            require(all(record.get("requested_" + key) == expected[key] for key in ("model", "reasoning_effort", "service_tier")), "qa_recovery_model_profile_changed")
            if role == "final_reader":
                reader_snapshots.append(record)
        snapshots.append(records)
        search = event.get("search")
        if event.get("event") == "search_failed" and isinstance(search, dict) and search.get("status") == "failed":
            fatal_search = True
    require(workflow is not None and snapshots[-1] == failure.get("model_attempts"), "qa_recovery_failure_snapshot_changed")
    navigation = failure.get("navigation")
    if navigation is not None:
        require(navigation.get("schema_version") == "gptgrep.navigation-query.v1"
                and navigation.get("status") in ("completed", "completed_empty")
                and navigation.get("hint_scan_complete") is True
                and all(navigation.get(key) == publication[key] for key in ("generation", "manifest_sha256", "artifact_sha256"))
                and navigation.get("document_scope") == case["doc_id"], "qa_recovery_navigation_binding_changed")
    final = snapshots[-1]
    relevant = next((record for record in final if record["role"] == "final_reader"), None)
    if relevant is None:
        relevant = next((record for record in final if record["role"] == "query_planner"), None)
    candidates = [cause for cause in (_typed_cause(failure.get("cause")), _typed_cause((relevant or {}).get("error"))) if cause is not None]
    require(not candidates or all(cause == candidates[0] for cause in candidates), "qa_recovery_conflicting_terminal_causes")
    cause = candidates[0] if candidates else None
    completed = any(record["status"] in ("process_completed", "completed") for record in reader_snapshots)
    no_reader = not reader_snapshots
    before_thread = bool(reader_snapshots) and all(all(record.get(key) is None for key in ("thread_id", "turn_id", "model", "model_provider")) for record in reader_snapshots)
    latest_failed = not reader_snapshots or reader_snapshots[-1]["status"] in ("failed", "interrupted")
    typed_terminal = cause is not None and cause["kind"] in TERMINAL_PROTOCOL_KINDS and cause["will_retry"] is not True
    fatal_jev = (failure.get("code") == "host_jev_search_failed" and failure.get("stage") == "codex" and fatal_search)
    boundary = ("before_final_reader" if no_reader else "reader_before_observed_thread" if before_thread
                else "typed_failed_reader_turn" if typed_terminal else "fatal_jev_tool_before_reader_completion" if fatal_jev
                else "unproven_started_reader_disposition")
    proven = not completed and latest_failed and (no_reader or before_thread or typed_terminal or fatal_jev)
    return {"ledger_path": str(path), "ledger_sha256": hashlib.sha256(raw).hexdigest(),
            "workflow_query_sha256": workflow["query_sha256"], "reader_process_completed_seen": completed,
            "pre_final_proven": proven, "boundary": boundary, "typed_cause": cause,
            "usage_availability": [{"role": record["role"], "status": record["status"],
                "usage_present": isinstance(record.get("usage"), dict), "accounting_complete": record.get("accounting_complete")}
                for record in final]}


def assess_attempt(origin, receipt, case, source, publication, manifest):
    ordinal = receipt["ordinal"]
    base = origin / "calls" / f"{ordinal:05d}"
    request = Path(str(base) + ".request.json")
    response = Path(str(base) + ".response.json")
    require(retained.digest(request) == receipt.get("request_sha256"), "qa_recovery_request_bytes_changed")
    result = {"source_row": case["source_row"], "origin_ordinal": ordinal, "request_sha256": receipt["request_sha256"],
              "eligible": False, "reason": None, "response_sha256": None, "semantic_disposition": False}
    if not response.exists():
        return {**result, "reason": "missing_response_boundary_unknown", "cost_policy": classify_cost(None, semantic_disposition=False, pre_final_proven=False)}
    require(retained.digest(response) == receipt.get("response_sha256"), "qa_recovery_response_bytes_changed")
    report = read_json(response)
    result["response_sha256"] = receipt["response_sha256"]
    # Only type/presence is inspected. Answer text is never accessed or emitted.
    semantic = (receipt.get("status") == "completed"
                or report.get("status") in ("completed", "insufficient_evidence", "abstained")
                or "answer" in report or report.get("insufficient_evidence") is True)
    if semantic:
        return {**result, "semantic_disposition": True, "reason": "completed_answer_or_semantic_abstention_retained",
                "cost_policy": classify_cost(None, semantic_disposition=True, pre_final_proven=False)}
    failure = report.get("host_retrieval")
    if receipt.get("status") != "failed" or not isinstance(failure, dict):
        return {**result, "reason": "no_bound_typed_native_failure", "cost_policy": classify_cost(None, semantic_disposition=False, pre_final_proven=False)}
    require(report.get("code") == failure.get("code") and failure.get("generation") == source["generation"], "qa_recovery_failure_identity_changed")
    proof = _ledger_metadata(origin, receipt, failure, case, source, publication, manifest)
    cause = proof["typed_cause"]
    prohibited = cause is not None and (cause["codex_error_info"] in NONRECOVERABLE_INFO
                  or cause["kind"] in {"tool_budget_exhausted", "identity_mismatch", "malformed_error", "invalid_turn_status"})
    eligible = (failure["code"] in ALLOWED_CODES and failure.get("stage") in
                ("query_plan", "query_plan_prepare", "initial_search", "codex", "navigation")
                and proof["pre_final_proven"] and not prohibited)
    return {**result, "eligible": eligible, "reason": proof["boundary"] if eligible else "final_boundary_or_failure_category_not_admitted",
            "failure_code": failure["code"], "failure_stage": failure.get("stage"), "proof": proof,
            "cost_policy": classify_cost(cause, semantic_disposition=False, pre_final_proven=proof["pre_final_proven"] and not prohibited)}


def _decimal(value):
    try:
        result = Decimal(str(value))
        return result if result.is_finite() and 0 <= result <= Decimal("1e40") else None
    except (InvalidOperation, ValueError):
        return None


def index_cost_view(report):
    """Derived index cost; missing Jev tokens do not erase complete USD receipts."""
    accounting = report.get("enrichment_result", {}).get("accounting", {})
    builder = accounting.get("builder") or {}
    jev = accounting.get("jev") or {}
    summary = jev.get("summary") or {}
    counts = [summary.get(key) for key in ("attempted_calls", "completed_calls", "failed_calls", "unobserved_calls")]
    valid_counts = all(type(value) is int and value >= 0 for value in counts)
    known = _decimal(jev.get("known_cost_subtotal_usd"))
    measured = _decimal(jev.get("measured_cost_usd"))
    zero = valid_counts and counts == [0, 0, 0, 0] and known in (None, Decimal(0)) and measured in (None, Decimal(0))
    complete = (accounting.get("available") is True and accounting.get("validated_chain") is True and valid_counts and counts[0] == counts[1]
                and counts[2] == counts[3] == 0 and jev.get("missing_cost_calls") == 0
                and (zero or known is not None and measured == known))
    if zero and complete:
        measured = Decimal(0)
    projections = {}
    for name, field in (("effective_tier", "api_price_equivalent"), ("standard_normalized", "standard_normalized_api_price_equivalent")):
        model = builder.get(field) or {}
        model_total = _decimal(model.get("total_usd_equivalent_decimal"))
        both = report.get("enrichment_result", {}).get("status") == "complete" and complete and model.get("complete") is True and model_total is not None
        total = model_total + measured if both else None
        projections[name] = {"cost_complete": both, "model_api_equivalent_usd": float(model_total) if model.get("complete") is True and model_total is not None else None,
                             "jev_reported_usd": float(measured) if complete else None,
                             "index_total_usd_equivalent": float(total) if total is not None else None,
                             "actual_chatgpt_billing_usd": None}
    return {"schema_version": "gptgrep.v4-index-cost-view.v1", "g5_effect": "none; indexing is excluded from QA cost gate",
            "enrichment_status": report.get("enrichment_result", {}).get("status"),
            "jev_cost_receipts_complete": complete, "jev_token_accounting_complete": jev.get("accounting_complete"),
            "jev_missing_token_contributions": summary.get("missing_total_tokens"),
            "jev_known_cost_subtotal_usd": float(known) if known is not None else None,
            "projections": projections, "original_report_modified": False, "missing_receipts_filled_from_rates": False}


def _audit_locked(inputs, origin):
    before = retained.inventory(origin)
    seal = inputs.seal.resolve()
    declaration = read_json(inputs.declaration)
    require(declaration.get("schema_version") == "gptgrep.v4-qa-recovery-declaration.v1", "qa_recovery_declaration_invalid")
    require(not inputs.declaration.resolve().is_relative_to(origin), "qa_recovery_declaration_must_be_external")
    manifest_path, summary_path = origin / "manifest.json", origin / "summary.json"
    require(retained.digest(manifest_path) == declaration["origin_manifest_sha256"]
            and retained.digest(summary_path) == declaration["origin_summary_sha256"]
            and retained.digest(origin / "host-calls.jsonl") == declaration["origin_host_calls_sha256"], "qa_recovery_origin_freeze_changed")
    manifest, summary = read_json(manifest_path), read_json(summary_path)
    require(manifest.get("schema_version") == "gptgrep.system-eval.v4" and manifest.get("question_count") == 62
            and len(manifest.get("source_rows", [])) == len(set(manifest["source_rows"])) == 62,
            "qa_recovery_requires_frozen_full62_v4")
    reader = profile(manifest["profile"]["roles"]["chat"], reader=True)
    require(profile(manifest["profile"]["roles"]["query_planner"]) == reader
            and profile(manifest["profile"]["roles"]["builder"]) == reader, "qa_recovery_model_configuration_changed")
    require(summary.get("status") in ("completed", "incomplete") and len(summary.get("cases", [])) == 62
            and {case.get("source_row") for case in summary["cases"]} == set(manifest["source_rows"]), "qa_recovery_terminal_full_case_inventory_missing")
    require(summary.get("enrichment_result", {}).get("status") == "complete", "qa_recovery_enrichment_not_complete")
    consumed, receipts = retained.reservations(origin)
    require(consumed == set(receipts) and all(call.get("status") in ("completed", "failed", "interrupted") for call in receipts.values())
            and summary.get("host_invocations") == len(receipts), "qa_recovery_unfinished_outer_reservations")
    cap = manifest["max_host_invocations"]
    require(type(cap) is int and cap == declaration["max_outer_invocations"] and len(consumed) <= cap, "qa_recovery_cumulative_cap_changed")
    source_path = seal / "source.json"
    require(retained.digest(source_path) == declaration["seal_source_sha256"], "qa_recovery_source_seal_changed")
    sealed = read_json(source_path)
    require(retained.digest(seal / "gptgrep") == manifest["binary_sha256"] == sealed["binary_sha256"], "qa_recovery_native_binary_changed")
    require(retained.digest(inputs.judge_binary) == manifest["judge_binary_sha256"], "qa_recovery_origin_judge_binary_changed")
    runner = seal / "runner/scripts/gptgrep_system_eval.py"
    require(retained.digest(runner) == manifest["runner_sha256"], "qa_recovery_frozen_runner_changed")
    require({name: entry["sha256"] for name, entry in retained.inventory(seal / "runner").items()} == sealed["frozen_runner_files"], "qa_recovery_runner_tree_changed")
    for name, expected in manifest["adapter_files"].items():
        require(Path(name).name == name and retained.digest(seal / "runner/scripts/pageindex_baseline" / name) == expected, "qa_recovery_frozen_adapter_changed")
    card = Path(manifest["price_card"]["path"])
    require(retained.digest(card) == manifest["price_card"]["sha256"] == declaration["price_card_sha256"], "qa_recovery_price_card_changed")
    build = read_json(origin / "build.json")
    source = read_json(origin / "corpus/.gptgrep/CURRENT.json", 4096)
    require(build.get("status") == "completed" and source == build["generation_binding"], "qa_recovery_generation_changed")
    generation = source["generation"]
    require(isinstance(generation, str) and re.fullmatch(r"[A-Za-z0-9-]+", generation), "qa_recovery_generation_invalid")
    base = origin / "corpus/.gptgrep/generations" / generation
    require(retained.digest(base / "manifest.json") == source["manifest_sha256"], "qa_recovery_index_manifest_changed")
    documents = read_json(base / "manifest.json")["documents"]
    require(len(documents) == manifest["document_count"] and {doc["path"] for doc in documents} == set(manifest["source_hashes"]), "qa_recovery_source_coverage_changed")
    for doc in documents:
        require(re.fullmatch(r"[0-9a-f]{24}", doc["id"]) and doc["source_sha256"] == manifest["source_hashes"][doc["path"]], "qa_recovery_document_identity_changed")
        path = origin / "corpus" / doc["path"]
        require(path.resolve().is_relative_to(origin / "corpus") and retained.digest(path) == doc["source_sha256"], "qa_recovery_raw_source_changed")
        require(retained.digest(base / "text" / (doc["id"] + ".txt")) == doc["text_sha256"], "qa_recovery_canonical_text_changed")
    ready = read_json(origin / "enrichment/ready.json")
    prepared = read_json(origin / "enrichment/prepared.json")
    frozen = summary["enrichment_result"]
    require(isinstance(frozen.get("prepared"), dict) and isinstance(frozen.get("ready"), dict)
            and prepared == frozen["prepared"] and ready == frozen["ready"]
            and summary.get("reader_binding") == ready and frozen.get("reader_admitted") is True,
            "qa_recovery_frozen_enrichment_snapshot_changed_or_missing")
    ledger_path = origin / "enrichment/ledger.jsonl"
    accounting = frozen.get("accounting") or {}
    require(accounting.get("available") is True and accounting.get("validated_chain") is True
            and accounting.get("ledger_sha256") == ready.get("ledger_sha256") == retained.digest(ledger_path)
            and type(ready.get("ledger_bytes")) is int
            and accounting.get("ledger_bytes") == ready["ledger_bytes"] == ledger_path.stat().st_size,
            "qa_recovery_frozen_enrichment_accounting_changed_or_missing")
    frozen_publication = ready.get("publication") or {}
    require((frozen.get("report") or {}).get("publication") == frozen_publication,
            "qa_recovery_frozen_enrichment_publication_changed_or_missing")
    enrichment_files = {"enrichment/prepared.json", "enrichment/ready.json", "enrichment/plan.json", "enrichment/ledger.jsonl",
                        "corpus/.gptgrep/NAVIGATION.json", "corpus/.gptgrep/navigation-overlays/" + sha(frozen_publication.get("artifact_sha256")) + ".json"}
    declared_files = declaration.get("enrichment_file_sha256")
    require(isinstance(declared_files, dict) and set(declared_files) == enrichment_files
            and all(retained.digest(origin / name) == sha(expected) for name, expected in declared_files.items()),
            "qa_recovery_frozen_enrichment_file_digest_changed_or_missing")
    require(prepared["run_binding"] == fingerprint(manifest) and ready["run_binding"] == prepared["run_binding"]
            and prepared["generation_binding"] == source, "qa_recovery_enrichment_binding_changed")
    require(retained.digest(origin / "enrichment/plan.json") == prepared["plan_file_sha256"] == ready["plan_file_sha256"]
            and sha(prepared.get("plan_sha256")) == ready.get("plan_sha256")
                == read_json(origin / "enrichment/plan.json").get("plan_sha256"), "qa_recovery_builder_evidence_changed")
    publication = read_json(origin / "corpus/.gptgrep/NAVIGATION.json", 4096)
    require(set(publication) == {"schema_version", "generation", "manifest_sha256", "artifact_sha256"}
            and publication["schema_version"] == "gptgrep.navigation-overlay.v1"
            and publication == ready["publication"] and all(publication[key] == source[key] for key in ("generation", "manifest_sha256")), "qa_recovery_overlay_binding_changed")
    artifact = origin / "corpus/.gptgrep/navigation-overlays" / (sha(publication["artifact_sha256"]) + ".json")
    require(retained.digest(artifact) == publication["artifact_sha256"], "qa_recovery_overlay_artifact_changed")
    cases = {case["source_row"]: case for case in summary["cases"]}
    decisions = []
    for row in manifest["source_rows"]:
        case = read_json(origin / "cases" / f"{row:03d}" / "case.json")
        keys = ("source_row", "doc_id", "status", "case_identity", "reader_attempt", "host_receipt")
        require({key: case.get(key) for key in keys} == {key: cases[row].get(key) for key in keys}
                and case.get("source_row") == row, "qa_recovery_final_case_state_changed")
        phase = f"answer:native:row-{row}"
        final_receipt = case.get("host_receipt")
        if final_receipt is not None:
            require(isinstance(final_receipt, dict) and final_receipt.get("ordinal") in receipts
                    and final_receipt == receipts[final_receipt["ordinal"]]
                    and final_receipt.get("operation") == "native_ask" and final_receipt.get("phase") == phase,
                    "qa_recovery_case_receipt_unbound")
        calls = [call for call in receipts.values() if call.get("operation") == "native_ask" and call.get("phase") == phase]
        if not calls:
            decisions.append({"source_row": row, "eligible": False, "reason": "no_failed_native_attempt_to_recover"})
            continue
        outcomes = [assess_attempt(origin, call, case, source, publication, manifest) for call in sorted(calls, key=lambda value: value["ordinal"])]
        final_exists = case.get("status") == "completed" or any(value["semantic_disposition"] or value.get("proof", {}).get("reader_process_completed_seen") for value in outcomes)
        if final_exists:
            for value in outcomes:
                value["eligible"] = False
                value["reason"] = "final_reader_disposition_retained_for_slot"
        latest = outcomes[-1]
        if len(outcomes) > 1 and not all(value.get("proof", {}).get("pre_final_proven") for value in outcomes[:-1]) and not final_exists:
            latest["eligible"] = False
            latest["reason"] = "earlier_attempt_final_boundary_unproven"
        decisions.extend(outcomes)
    selected = [value for value in decisions if value["eligible"]]
    # One candidate per source row: earlier failed attempts remain cost/reliability evidence.
    selected = [value for value in selected if value["origin_ordinal"] == max(item["origin_ordinal"] for item in decisions if item["source_row"] == value["source_row"])]
    require(len(consumed) + 2 * len(selected) <= cap, "qa_recovery_remaining_budget_insufficient")
    require(retained.inventory(origin) == before, "qa_recovery_origin_changed_during_audit")
    return {"schema_version": SCHEMA, "policy": POLICY, "origin": str(origin),
            "origin_manifest_sha256": retained.digest(manifest_path), "origin_summary_sha256": retained.digest(summary_path),
            "origin_host_calls_sha256": retained.digest(origin / "host-calls.jsonl"),
            "source_revision": sealed["source_revision"], "source_seal_sha256": retained.digest(source_path),
            "native_binary": {"path": str(seal / "gptgrep"), "sha256": manifest["binary_sha256"]},
            "frozen_runner": {"path": str(runner), "sha256": manifest["runner_sha256"]},
            "runner_files": sealed["frozen_runner_files"], "price_card_sha256": manifest["price_card"]["sha256"],
            "source_binding": source, "overlay": publication, "reader_profile": reader,
            "origin_judge_profile": manifest["profile"]["roles"]["judge"], "origin_files": before,
            "question_denominator": 62, "origin_outer_reservations": len(consumed), "max_outer_invocations": cap,
            "selected": selected, "attempt_decisions": decisions, "new_model_calls": 0,
            "index_cost_view": index_cost_view(summary), "declaration_sha256": retained.digest(inputs.declaration),
            "implementation_files": {str(Path(__file__).resolve()): retained.digest(Path(__file__).resolve()),
                                     str(Path(retained.__file__).resolve()): retained.digest(Path(retained.__file__).resolve())},
            "comparison_eligible": False, "G5_accepted": False,
            "limits": ["Only source-bound pre-final technical attempts are selected; no score-based selection.",
                       "Generic failure codes/text never imply remote communication or cost exclusion.",
                       "Failed attempts, observed usage and unknown billing remain separate from comparison exclusions.",
                       "No request question fields or answer/gold text were inspected.",
                       "Future judge/model policy is not silently applied to retained origin judgments."]}


def audit(inputs):
    with terminal_owner(inputs.origin) as origin:
        return _audit_locked(inputs, origin)


def _atomic_new(path, value):
    raw = encoded(value) + b"\n"
    require(len(raw) <= MAX_JSON, "qa_recovery_plan_byte_limit")
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".qa-plan-", delete=True) as stream:
        os.chmod(stream.name, 0o600)
        stream.write(raw); stream.flush(); os.fsync(stream.fileno())
        os.link(stream.name, path, follow_symlinks=False)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def prepare_plan(inputs):
    with terminal_owner(inputs.origin) as origin:
        result = _audit_locked(inputs, origin)
        sidecar = origin.with_name(origin.name + "-v4-qa-recovery")
        require(not sidecar.is_symlink(), "qa_recovery_sidecar_redirected")
        sidecar.mkdir(mode=0o700, exist_ok=True)
        plan = {"schema_version": SCHEMA, "status": "zero_model_plan", "audit": result,
                "executor_contract": {"version": "gptgrep.v4-qa-recovery-executor.v1", "implemented": False,
                    "native_ask_operation": "replay exact hash-bound retained request using frozen runner/binary and overlay",
                    "maximum_new_reader_calls": len(result["selected"]), "maximum_new_outer_calls": 2 * len(result["selected"]),
                    "same_cumulative_outer_cap": result["max_outer_invocations"], "no_completed_answer_reroll": True,
                    "new_attempts_and_costs": "retain in separate sidecar; do not rewrite origin results",
                    "allowed_origin_additions": "only bound new corpus/.gptgrep/host-attempts ledgers during a separately admitted execution",
                    "must_reconcile_first_completed_sidecar_outcomes_before_new_call": True,
                    "judge_execution": "requires separate explicit profile/protocol admission; no automatic legacy judge call",
                    "future_new_judge_model": "gpt-6-luna", "future_new_judge_efforts": ["xhigh", "max"],
                    "future_new_judge_service_tier": "fast", "plan_is_execution_authorization": False}}
        path = sidecar / "plan.json"
        if path.exists():
            require(read_json(path) == plan, "qa_recovery_immutable_plan_changed")
        else:
            _atomic_new(path, plan)
        return {"status": "plan_prepared", "path": str(path), "sha256": retained.digest(path),
                "selected_native_attempts": len(result["selected"]), "new_model_calls": 0,
                "comparison_cost_exclusions": sum(item.get("cost_policy", {}).get("exclude_from_experimental_comparison_cost") is True for item in result["attempt_decisions"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("audit", "plan"):
        command = commands.add_parser(name)
        for flag in ("origin", "seal", "declaration", "judge-binary"):
            command.add_argument("--" + flag, type=Path, required=True)
    view = commands.add_parser("index-cost-view", help="Derived metadata-only view; never changes original reports")
    view.add_argument("--summary", type=Path, required=True)
    view.add_argument("--summary-sha256", required=True)
    args = parser.parse_args()
    try:
        if args.command == "index-cost-view":
            require(retained.digest(args.summary) == sha(args.summary_sha256), "qa_recovery_summary_digest_changed")
            result = {"source_summary_sha256": args.summary_sha256, **index_cost_view(read_json(args.summary))}
        else:
            inputs = Inputs(args.origin, args.seal, args.declaration, args.judge_binary)
            result = prepare_plan(inputs) if args.command == "plan" else audit(inputs)
        print(json.dumps(result, sort_keys=True, ensure_ascii=False, allow_nan=False))
        return 0
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(json.dumps({"status": "blocked", "error_type": type(error).__name__,
                          "code": str(error) if isinstance(error, ValueError) else "qa_recovery_evidence_unavailable", "new_model_calls": 0}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
