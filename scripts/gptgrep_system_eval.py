#!/usr/bin/env python3
"""Evaluate the actual GPTgrep native build + mandatory Jev + Codex reader on pinned tasks."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
import re
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import time
import uuid
import jsonschema
from threading import Lock

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts/pageindex_baseline"))
import locks
import profiles
import cohorts
from bridge import AdapterError, LocalCodex, json_bytes, owned_process, _retain_accounting, _receipt_tool_budget
from role_hosts import RoleHost
from native_models import DEFAULT_PLANNER_MODEL, attempts_from_report, planner_profile, planner_profile_from_payload, usage_summary
from run import checkpoint_attempt, fingerprint, judge_constants, private_directory, run_case_pool, task_concurrency_report, write_json


def query_strategy_flags(args) -> tuple[bool, bool]:
    planned = getattr(args, "experimental_query_plan", False)
    evidence_roles = getattr(args, "experimental_evidence_roles", False)
    if type(planned) is not bool or type(evidence_roles) is not bool:
        raise ValueError("Experimental strategy flags must be booleans")
    if evidence_roles and not planned:
        raise ValueError("Evidence-role selection requires experimental query planning")
    if getattr(args, "planner_model", None) is not None and not planned:
        raise ValueError("Planner model requires experimental query planning")
    return planned, evidence_roles


def selected_planner_profile(args) -> dict:
    model = getattr(args, "planner_model", None)
    return planner_profile(DEFAULT_PLANNER_MODEL if model is None else model,
                           getattr(args, "reasoning_effort", None) or "max")


def normalize_codex_launcher(value: str) -> str:
    """Bind explicit launcher paths before host calls change working directory."""
    if not isinstance(value, str) or not value or value != value.strip():
        raise ValueError("codex_launcher_invalid")
    if os.sep not in value and (os.altsep is None or os.altsep not in value):
        return value  # A bare command remains a normal PATH lookup.
    launcher = Path(value).expanduser().resolve()
    if not launcher.is_file() or not os.access(launcher, os.X_OK):
        raise ValueError("codex_launcher_path_unavailable")
    return str(launcher)


def _builder_models():
    # The legacy branch does not import or depend on the enrichment validator.
    import native_builder_models
    return native_builder_models


def enrichment_enabled(args) -> bool:
    value = getattr(args, "experimental_enrichment", False)
    if type(value) is not bool:
        raise ValueError("Experimental enrichment flag must be boolean")
    return value


def navigation_arguments(args) -> list[str]:
    if not enrichment_enabled(args):
        return []
    publication = getattr(args, "_enrichment_publication", None)
    if not isinstance(publication, dict):
        raise ValueError("enrichment_complete_overlay_required_before_reader")
    return ["--navigation-overlay-sha256", publication["artifact_sha256"],
            "--navigation-max-jev-calls", str(args._enrichment_config["navigation_max_jev_calls"]),
            "--navigation-max-request-bytes", str(args._enrichment_config["navigation_max_request_bytes"])]


def ask_arguments(binary: Path, root: Path, row: dict, args) -> list[str]:
    # Only original question and known-document scope enter retrieval; gold stays in the evaluator.
    planned, evidence_roles = query_strategy_flags(args)
    return [str(binary), "ask", "--document=" + row["doc_id"],
            "--jev-model", args.jev_model, "--codex-bin", args.codex_bin,
            "--codex-home", str(args.codex_home.expanduser().resolve()), "--model", args.model,
            "--reasoning-effort", args.reasoning_effort, "--service-tier", args.service_tier,
            "--timeout", str(args.timeout),
            "--max-tool-calls", str(args.max_tool_calls),
            "--max-input-bytes", str(getattr(args, "max_input_bytes", 262144))] + (
                ["--experimental-query-plan", "--planner-model", selected_planner_profile(args)["model"]] if planned else []
            ) + (["--experimental-evidence-roles"] if evidence_roles else []) + navigation_arguments(args) + [
                "--json", "--", row["question"], str(root)]


def build_arguments(binary: Path, corpus: Path, optimize_merge: bool) -> list[str]:
    return [str(binary), "index", str(corpus), "--json"] + (["--optimize-merge"] if optimize_merge else [])


def enrichment_config(args, profile):
    if not enrichment_enabled(args):
        return None
    bm = _builder_models()
    planned, _ = query_strategy_flags(args)
    bm.require(planned and args.rows == "all" and not args.retry_failed,
               "enrichment_requires_complete_cohort_planning_and_no_blanket_retry")
    reader = profile["roles"]["chat"]
    builder = {"model": getattr(args, "builder_model", None) or reader["model"],
               "reasoning_effort": getattr(args, "builder_reasoning_effort", None) or reader["reasoning_effort"], "service_tier": "fast"}
    bm.require(builder["model"] == "gpt-6-luna" and builder["reasoning_effort"] in ("xhigh", "max"),
               "enrichment_requires_gpt6_xhigh_or_max")
    bm.require(reader == builder == selected_planner_profile(args), "enrichment_builder_planner_reader_profiles_must_match")
    judge = profile["roles"]["judge"]
    bm.require(judge["model"] == "gpt-6-luna" and judge["reasoning_effort"] in ("xhigh", "max")
               and judge["service_tier"] == "fast", "enrichment_requires_explicit_gpt6_fast_xhigh_or_max_judge")
    bm.require(args.jev_model == "typesafe/jev-1.13", "enrichment_fixed_jev_profile_changed")
    values = {
        "builder": builder, "jev_model": args.jev_model,
        "max_builder_calls": getattr(args, "max_builder_calls", None),
        "max_jev_calls": getattr(args, "max_builder_jev_calls", None),
        "window_bytes": getattr(args, "enrich_window_bytes", 8192),
        "max_hints_per_window": getattr(args, "enrich_max_hints_per_window", 4),
        "max_ledger_bytes": getattr(args, "enrich_max_ledger_bytes", 64 * 1024 * 1024),
        "max_windows_per_run": getattr(args, "enrich_max_windows_per_run", 65536),
        "deadline_seconds": getattr(args, "enrich_deadline_seconds", 900),
        "call_timeout_secs": getattr(args, "builder_timeout", 180),
        "max_input_bytes": args.max_input_bytes,
        "navigation_max_jev_calls": getattr(args, "navigation_max_jev_calls", 32),
        "navigation_max_request_bytes": getattr(args, "navigation_max_request_bytes", 2 * 1024 * 1024),
        "outer_invocation_cap": args.max_model_calls,
    }
    for name in ("max_builder_calls", "max_jev_calls", "max_windows_per_run"):
        bm.integer(values[name], positive=True, maximum=65536)
    bm.require(4 <= values["window_bytes"] <= 65536 and 1 <= values["max_hints_per_window"] <= 4,
               "enrichment_window_or_hint_bound_invalid")
    bm.require(256 * 1024 <= values["max_ledger_bytes"] <= 256 * 1024 * 1024, "enrichment_ledger_cap_invalid")
    bm.integer(values["deadline_seconds"], positive=True, maximum=86400)
    bm.integer(values["call_timeout_secs"], positive=True, maximum=900)
    bm.integer(values["navigation_max_jev_calls"], positive=True, maximum=128)
    bm.integer(values["navigation_max_request_bytes"], positive=True, maximum=8 * 1024 * 1024)
    bm.integer(values["outer_invocation_cap"], positive=True)
    return values


def enrichment_preflight(binary, run_dir):
    process = owned_process([str(binary), "--schema"], run_dir, 30)
    bm = _builder_models()
    bm.require(process.returncode == 0 and len(process.stdout) <= 1024 * 1024,
               "enrichment_cli_schema_unavailable_zero_model_preflight")
    contract = bm.parse(process.stdout)
    bm.check_cli_contract(contract)
    for name in ("navigation-max-jev-calls", "navigation-max-request-bytes"):
        bm.require(contract["commands"]["ask"]["options"].get(name, {}).get("type") == "integer",
                   "enrichment_navigation_budget_cli_contract_unavailable_zero_model_preflight")
    return hashlib.sha256(process.stdout).hexdigest()


def enrichment_arguments(binary, corpus, args, directory, *, plan_only=False, semantic_sha=None, resume=False):
    config = args._enrichment_config
    command = [str(binary), "enrich", str(corpus), "--ledger-path", str(directory / "ledger.jsonl"),
               "--builder-model", config["builder"]["model"], "--reasoning-effort", config["builder"]["reasoning_effort"], "--service-tier", "fast",
               "--jev-model", config["jev_model"], "--codex-bin", args.codex_bin,
               "--codex-home", str(args.codex_home.expanduser().resolve()),
               "--timeout", str(config["call_timeout_secs"]), "--max-input-bytes", str(config["max_input_bytes"]),
               "--window-bytes", str(config["window_bytes"]), "--max-hints-per-window", str(config["max_hints_per_window"]),
               "--max-builder-calls", str(config["max_builder_calls"]), "--max-jev-calls", str(config["max_jev_calls"]),
               "--max-ledger-bytes", str(config["max_ledger_bytes"]), "--max-windows-per-run", str(config["max_windows_per_run"]),
               "--deadline-seconds", str(config["deadline_seconds"]), "--json"]
    if plan_only:
        command += ["--plan-only", "--plan-output", str(directory / "plan.json")]
    if semantic_sha is not None:
        command += ["--expected-plan-sha256", semantic_sha]
    if resume:
        command += ["--resume"]
    return command


def enrichment_new_json(path, value):
    bm = _builder_models()
    path = Path(path)
    bm.require(not path.is_symlink() and all(not parent.is_symlink() for parent in path.parents), "enrichment_artifact_redirected")
    raw = bm.encoded(value) + b"\n"
    bm.require(len(raw) <= bm.MAX_LEDGER_BYTES, "enrichment_artifact_byte_limit")
    with path.open("xb") as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())


def enrichment_plan_inputs(corpus, build, canonical, args):
    bm = _builder_models()
    generation = build["generation_binding"]
    path = corpus / ".gptgrep/generations" / generation["generation"] / "manifest.json"
    bm.require(bm.digest(path) == generation["manifest_sha256"], "enrichment_generation_manifest_changed")
    manifest = bm.read_json(path, 128 * 1024 * 1024)
    documents = {doc["path"]: {"document_id": doc["id"], "path": doc["path"], "source_sha256": doc["source_sha256"],
                               "text_sha256": doc["text_sha256"], "text_bytes": len(canonical[doc["path"]])}
                 for doc in manifest["documents"]}
    config = args._enrichment_config
    binding = {"schema_version": "gptgrep.enrich.binding.v1", "root": str(corpus.resolve()),
               "source": {"generation": generation["generation"], "manifest_sha256": generation["manifest_sha256"],
                          "documents_total": len(documents), "nodes_total": sum(len(doc["nodes"]) for doc in manifest["documents"])},
               "builder_model": config["builder"]["model"], "reasoning_effort": config["builder"]["reasoning_effort"], "service_tier": "fast",
               "codex_bin": args.codex_bin, "codex_home": str(args.codex_home.expanduser().resolve()),
               **{key: config[key] for key in ("call_timeout_secs", "max_input_bytes", "jev_model", "window_bytes",
                                              "max_hints_per_window", "max_builder_calls", "max_jev_calls", "max_ledger_bytes")}}
    return binding, documents


def prepare_enrichment(binary, corpus, args, run_dir, manifest, build, canonical):
    bm = _builder_models()
    directory = run_dir / "enrichment"
    private_directory(directory)
    plan_path, prepared_path = directory / "plan.json", directory / "prepared.json"
    expected, documents = enrichment_plan_inputs(corpus, build, canonical, args)
    if prepared_path.exists():
        prepared = bm.read_json(prepared_path)
        bm.require(prepared["run_binding"] == run_binding(manifest) and prepared["plan_file_sha256"] == bm.digest(plan_path),
                   "enrichment_prepared_binding_changed")
        plan = bm.validate_plan(bm.read_json(plan_path), expected, documents, canonical)
        bm.require(prepared["plan_sha256"] == plan["plan_sha256"], "enrichment_prepared_plan_changed")
    else:
        bm.require(args.stage == "plan", "enrichment_requires_zero_model_plan_stage_first")
        bm.require(not plan_path.exists() and not (directory / "ledger.jsonl").exists(), "enrichment_unbound_preparation_artifact")
        process = owned_process(enrichment_arguments(binary, corpus, args, directory, plan_only=True), run_dir, args.build_timeout)
        bm.require(process.returncode == 0 and len(process.stdout) <= 1024 * 1024, "enrichment_zero_model_plan_failed")
        compact = bm.parse(process.stdout)
        bm.require(compact.get("schema_version") == "gptgrep.enrich-plan.v1" and compact.get("status") == "plan_prepared"
                   and compact.get("new_model_calls") == 0 and compact.get("worst_case_within_declared_caps") is True,
                   "enrichment_zero_model_plan_or_caps_invalid")
        plan = bm.validate_plan(bm.read_json(plan_path), expected, documents, canonical)
        bm.require(compact.get("plan_sha256") == plan["plan_sha256"] and compact.get("unit_count") == plan["unit_count"],
                   "enrichment_compact_plan_differs")
        prepared = {"schema_version": "gptgrep.system-enrichment-prepared.v1", "run_binding": run_binding(manifest),
                    "plan_sha256": plan["plan_sha256"], "plan_file_sha256": bm.digest(plan_path),
                    "generation_binding": build["generation_binding"], "config": args._enrichment_config,
                    "zero_model_plan_stdout_sha256": hashlib.sha256(process.stdout).hexdigest(),
                    "ledger_path": str(directory / "ledger.jsonl"), "unit_count": plan["unit_count"]}
        enrichment_new_json(directory / "plan-receipt.json", compact)
        enrichment_new_json(prepared_path, prepared)
    bm.require(prepared["generation_binding"] == build["generation_binding"] and prepared["config"] == args._enrichment_config
               and prepared["ledger_path"] == str(directory / "ledger.jsonl"), "enrichment_prepared_conditions_changed")
    if args.stage == "run":
        bm.require(getattr(args, "expected_enrichment_plan_sha256", None) == plan["plan_sha256"],
                   "enrichment_explicit_expected_plan_sha256_required")
    return prepared, plan


def enrichment_payload(args, prepared, *, resume):
    # Explicit allowlist: no task row, question, gold, annotations or answer state.
    return {"operation": "enrich", "phase": "enrichment:native", **args._enrichment_config["builder"],
            "config": args._enrichment_config, "plan_sha256": prepared["plan_sha256"],
            "plan_file_sha256": prepared["plan_file_sha256"], "generation_binding": prepared["generation_binding"],
            "ledger_path": prepared["ledger_path"], "resume": resume}


def enrichment_accounting_view(ledger):
    if ledger is None:
        return {"available": False, "builder": None, "jev": None, "unknown_attempts": True}
    builder = {key: value for key, value in ledger["builder"].items() if key != "model_turns"}
    turns = ledger["builder"]["model_turns"]
    builder.update(observed_model_turns=len(turns), model_turn_identity_sha256=fingerprint(
        [[turn["thread_id"], turn["turn_id"]] for turn in turns]))
    return {"available": True, "ledger_sha256": ledger["ledger_sha256"], "ledger_bytes": ledger["ledger_bytes"],
            "validated_chain": ledger["validated_chain"], "validation_error": ledger["validation_error"],
            "builder": builder, "jev": ledger["jev"], "windows_completed": len(ledger["windows"]),
            "scope": "One cumulative enrichment ledger, counted once across all outer enrichment invocations",
            "limits": ledger["limits"]}


def invoke_enrichment(shared, arguments, payload, timeout, plan, *, price_card=None):
    bm = _builder_models()
    report, ledger = {}, None
    with shared.external_attempt(payload, phase=payload["phase"], model=payload["model"], effort=payload["reasoning_effort"],
                                 service_tier=payload["service_tier"], role="index", operation="native_enrich") as attempt:
        receipt = attempt.receipt
        response_path = shared.run_dir / "calls" / f"{receipt['ordinal']:05d}.response.json"
        try:
            process = attempt.run(arguments, timeout=timeout)
            bm.require(len(process.stdout) <= 4 * 1024 * 1024, "enrichment_report_byte_limit")
            with response_path.open("xb") as stream:
                stream.write(process.stdout)
                stream.flush()
                os.fsync(stream.fileno())
            receipt.update(response_sha256=bm.digest(response_path), exit_code=process.returncode)
            report = bm.parse(process.stdout)
            ledger = bm.validate_ledger(Path(payload["ledger_path"]), plan, allow_partial=report.get("status") != "complete", price_card=price_card)
            bm.validate_enrich_report(report, ledger, plan, Path(payload["ledger_path"]))
            bm.require(process.returncode == (0 if report["status"] == "complete" else 2), "enrichment_exit_status_differs")
            receipt.update(status="completed" if report["status"] == "complete" else "failed", enrichment_status=report["status"],
                           error_code=None if report["status"] == "complete" else "enrichment_" + report["status"])
        except Exception as error:
            receipt.update(status="failed", error_code="enrichment_outer_or_validation_failed", error_type=type(error).__name__)
            if ledger is None and Path(payload["ledger_path"]).exists():
                try:
                    ledger = bm.validate_ledger(Path(payload["ledger_path"]), plan, allow_partial=True, price_card=price_card)
                except (ValueError, OSError, KeyError, TypeError):
                    pass
            if hasattr(error, "cleanup"):
                receipt["timeout_cleanup"] = error.cleanup
        receipt["enrichment_accounting"] = enrichment_accounting_view(ledger)
        receipt["usage_scope"] = "outer_process_only; builder/Jev usage is in the separate cumulative ledger"
    return report, shared.call_by_ordinal(receipt["ordinal"]), ledger


def run_enrichment(binary, corpus, args, run_dir, shared, prepared, plan):
    bm = _builder_models()
    directory = run_dir / "enrichment"
    ledger_path = directory / "ledger.jsonl"
    attempts = [call for call in shared.calls if call.get("operation") == "native_enrich"]
    prior, report, ledger = None, {}, None
    for position, call in enumerate(attempts):
        retained = call.get("enrichment_accounting") or {}
        if retained.get("available") is True:
            size = bm.integer(retained.get("ledger_bytes"), maximum=args._enrichment_config["max_ledger_bytes"])
            expected = bm.sha(retained.get("ledger_sha256"))
            observed = hashlib.sha256()
            with bm.regular(ledger_path).open("rb") as stream:
                remaining = size
                while remaining:
                    block = stream.read(min(1024 * 1024, remaining))
                    bm.require(block, "enrichment_prior_ledger_truncated")
                    observed.update(block)
                    remaining -= len(block)
            bm.require(observed.hexdigest() == expected, "enrichment_prior_ledger_prefix_changed")
        request_path = run_dir / "calls" / f"{call['ordinal']:05d}.request.json"
        response_path = run_dir / "calls" / f"{call['ordinal']:05d}.response.json"
        request = bm.read_json(request_path)
        bm.require(call.get("request_sha256") == bm.digest(request_path)
                   and request == enrichment_payload(args, prepared, resume=request.get("resume")), "enrichment_retained_request_changed")
        bm.require(type(request.get("resume")) is bool, "enrichment_resume_binding_invalid")
        if response_path.exists():
            bm.require(call.get("response_sha256") == bm.digest(response_path), "enrichment_retained_response_changed")
            value = bm.read_json(response_path, 4 * 1024 * 1024)
            if value.get("status") == "complete":
                bm.require(call.get("status") == "completed", "enrichment_completed_response_lacks_valid_receipt")
                bm.require(position == len(attempts) - 1, "enrichment_attempt_after_completed_publication")
                report, prior = value, call
                break
            report = value
        prior = call
    if prior is not None:
        ledger = bm.validate_ledger(ledger_path, plan, allow_partial=report.get("status") != "complete", price_card=args._price_card) if ledger_path.exists() else None
        if report.get("status") == "complete":
            bm.validate_enrich_report(report, ledger, plan, ledger_path)
        elif not getattr(args, "resume_enrichment", False):
            return {"status": "incomplete" if report.get("status") == "incomplete" else "failed", "report": report,
                    "accounting": enrichment_accounting_view(ledger), "outer_invocations": len(attempts), "reader_admitted": False}
        else:
            bm.require(report.get("status") == "incomplete" and report.get("resume_safe") is True and ledger is not None
                       and ledger["resume_safe"] and ledger["validated_chain"], "enrichment_failed_pending_or_uncommitted_work_cannot_resume")
            bm.validate_enrich_report(report, ledger, plan, ledger_path)
    else:
        bm.require(not ledger_path.exists() and not getattr(args, "resume_enrichment", False), "enrichment_orphan_ledger_or_invalid_resume")
    if report.get("status") != "complete":
        resume = prior is not None
        payload = enrichment_payload(args, prepared, resume=resume)
        report, receipt, ledger = invoke_enrichment(shared,
            enrichment_arguments(binary, corpus, args, directory, semantic_sha=plan["plan_sha256"], resume=resume),
            payload, args._enrichment_config["deadline_seconds"] + 60, plan, price_card=args._price_card)
        attempts = [call for call in shared.calls if call.get("operation") == "native_enrich"]
        if receipt.get("status") != "completed" or report.get("status") != "complete":
            return {"status": "incomplete" if report.get("status") == "incomplete" else "failed", "report": report,
                    "accounting": enrichment_accounting_view(ledger), "outer_invocations": len(attempts), "reader_admitted": False}
    publication = bm.validate_publication(corpus, report, ledger, plan)
    ready = {"schema_version": "gptgrep.system-enrichment-ready.v1", "run_binding": prepared["run_binding"], "plan_file_sha256": prepared["plan_file_sha256"],
             "plan_sha256": plan["plan_sha256"], "publication": publication, "ledger_sha256": ledger["ledger_sha256"],
             "ledger_bytes": ledger["ledger_bytes"]}
    ready_path = directory / "ready.json"
    if ready_path.exists():
        bm.require(bm.read_json(ready_path) == ready, "enrichment_ready_binding_changed")
    else:
        enrichment_new_json(ready_path, ready)
    args._enrichment_publication = publication
    return {"status": "complete", "report": report, "prepared": prepared, "ready": ready,
            "accounting": enrichment_accounting_view(ledger), "outer_invocations": len(attempts), "reader_admitted": True}


def enrichment_preparation_failure(manifest, rows, run_dir, build, enrichment, *, stage):
    cases = []
    for row in rows:
        path = run_dir / "cases" / f"{row['source_row']:03d}" / "case.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists():
            case = locks.read_json(path)
            if case.get("reader_attempt") is not None or case.get("host_receipt") is not None:
                raise ValueError("enrichment_preparation_changed_beneath_retained_reader")
        case = {"source_row": row["source_row"], "doc_id": row["doc_id"], "status": stage,
                "case_identity": fingerprint({"row": row, "run_binding": run_binding(manifest)}),
                "reader_not_started": True, "judge": {"status": "not_started"}}
        write_json(path, case)
        cases.append(case)
    report = {**manifest, "status": "incomplete", "build_result": build, "enrichment_result": enrichment,
              "cases": cases, "summary": summarize(cases, len(rows)), "comparison_eligible": False,
              "host_invocations": len((run_dir / "host-calls.jsonl").read_text().splitlines()) if (run_dir / "host-calls.jsonl").exists() else 0,
              "model_usage_note": "Incomplete preparation does not admit readers; failed/unknown builder work remains separately accounted."}
    write_json(run_dir / "summary.json", report)
    return report


def enrichment_cost_projection(args, cumulative, shared, enrichment, summary, manifest):
    bm = _builder_models()
    turns = [record for attempt in cumulative["attempts"] for record in (attempt.get("model_attempts") or [])]
    unknown = cumulative["native_model_turn_accounting"]["unavailable_native_ask_accounting"]
    jev_calls = [attempt["cost"].get("attempted_calls") for attempt in cumulative["attempts"]]
    qa_jev = {"attempted_calls": sum(jev_calls) if all(type(value) is int for value in jev_calls) else None,
              "accounting_complete": cumulative["accounting_complete"],
              "measured_cost_usd": cumulative["measured_jev_cost_usd"],
              "known_cost_subtotal_usd": cumulative["known_jev_cost_subtotal_usd"]}
    count = summary["question_denominator"]
    expected_roles = {"query_planner": count, "final_reader": count}
    actual = bm.combined_model_jev_cost(bm.price_model_turns(turns, unknown_groups=unknown, price_card=args._price_card, expected_roles=expected_roles), qa_jev, count)
    standard = bm.combined_model_jev_cost(bm.price_model_turns(turns, unknown_groups=unknown, price_card=args._price_card, normalization="standard", expected_roles=expected_roles), qa_jev, count)
    index = enrichment["accounting"]
    if index.get("available"):
        observed = index["jev"]
        index_jev = {"attempted_calls": observed["summary"]["attempted_calls"],
                     "accounting_complete": observed["accounting_complete"] and observed["missing_cost_calls"] == 0,
                     "measured_cost_usd": observed["measured_cost_usd"], "known_cost_subtotal_usd": observed["known_cost_subtotal_usd"]}
        index_projection = {
            "effective_tier": bm.combined_model_jev_cost(index["builder"]["api_price_equivalent"], index_jev),
            "standard_normalized": bm.combined_model_jev_cost(index["builder"]["standard_normalized_api_price_equivalent"], index_jev),
            "deterministic_index_model_calls": 0, "local_compute_cost_measured": False,
            "excluded_from_g5": True,
        }
    else:
        index_projection = {"complete": False, "excluded_from_g5": True}
    judge_records = [{"role": "independent_judge", "model": call.get("model"), "thread_id": call.get("thread_id"),
                      "turn_id": call.get("turn_id"), "effective_service_tier": call.get("effective_service_tier"),
                      "usage": call.get("usage"), "accounting_complete": call.get("status") == "completed"}
                     for call in shared.calls if call.get("phase", "").startswith("judge:native:")]
    references = {}
    metadata = manifest["external_reference_bindings"].get("original_pageindex_documents")
    if metadata:
        references["original_pageindex_index"] = {**bm.original_index_costs(bm.read_json(Path(metadata["path"])), sorted(manifest["source_hashes"])),
                                                  "source_sha256": metadata["sha256"]}
    adapted = manifest["external_reference_bindings"].get("adapted_trace_summary")
    if adapted:
        raw = bm.read_json(Path(adapted["path"]))
        origin = raw.get("historical_index_origin")
        references["adapted_live_index"] = {"source_sha256": adapted["sha256"], "billing_usd": None,
            "scope": "Historical index origin is separate from the original benchmark and G5; missing usage is not zero",
            "origin_usage_missing": origin.get("origin_usage_missing") if isinstance(origin, dict) else None}
    return {"price_card": manifest["price_card"], "actual_account_billing_usd": None,
            "qa_standard_normalized": standard, "qa_effective_tier_api_equivalent": actual,
            "standard_normalized_qa_usd_per_question": standard["per_question_usd_equivalent"],
            "effective_tier_qa_api_equivalent_usd_per_question": actual["per_question_usd_equivalent"],
            "index_enrichment_separate": index_projection,
            "independent_judge_separate": bm.price_model_turns(judge_records, price_card=args._price_card),
            "external_index_references": references,
            "g5_dual_gate_observation": bm.qa_dual_gate_observation(summary, standard)}


def jev_receipt(report: dict) -> dict | None:
    value = report.get("jev")
    if isinstance(value, dict):
        return value
    for field in ("host_retrieval", "retrieval", "error"):
        nested = report.get(field)
        if isinstance(nested, dict) and isinstance(nested.get("jev"), dict):
            return nested["jev"]
    return None


def service_tier_metadata(requested: str, report: dict) -> dict:
    return {"requested_service_tier": requested,
            "reported_requested_service_tier": report.get("requested_service_tier"),
            "effective_service_tier": report.get("effective_service_tier")}


def tier_matches(requested: str, observed: str | None) -> bool:
    # The local Codex protocol may acknowledge fast as its priority alias.
    # Absence is unobserved, never confirmation that the requested tier applied.
    if observed is None:
        return True
    if not isinstance(observed, str):
        return False
    return ("priority" if requested == "fast" else requested) == ("priority" if observed == "fast" else observed)


def jev_cost(jev: dict | None) -> dict:
    if jev is None:
        return {"attempted_calls": None, "validated_responses": None, "measured_cost_usd": None,
                "missing_cost_receipts": None, "accounting_complete": False}
    attempted, responses = jev.get("attempted_calls"), jev.get("requests")
    usage = jev.get("usage", [])
    known = []
    if isinstance(usage, list):
        for value in usage:
            cost = value.get("cost") if isinstance(value, dict) else None
            if type(cost) in (float, int) and math.isfinite(cost) and cost >= 0:
                known.append(cost)
    valid_counts = type(attempted) is int and type(responses) is int and 0 <= responses <= attempted
    missing = max(0, attempted - len(known)) if valid_counts else None
    complete = valid_counts and jev.get("accounting_complete") is True and len(known) == attempted and len(usage) == attempted
    return {"attempted_calls": attempted if valid_counts else None,
            "validated_responses": responses if valid_counts else None,
            "measured_cost_usd": math.fsum(known) if complete and attempted else None,
            "known_cost_subtotal_usd": math.fsum(known) if known else None,
            "missing_cost_receipts": missing, "accounting_complete": complete,
            "attempts_are_physical_requests": False}


def native_metrics(report: dict, doc_id: str, gold_pages: set[int]) -> dict:
    tools = report.get("tool_calls", report.get("host_retrieval", {}).get("receipts", report.get("receipts", [])))
    tools = tools if isinstance(tools, list) else []
    cited = report.get("citations", [])
    cited = cited if isinstance(cited, list) else []
    pages = set()
    for tool in tools:
        if tool.get("success") is not True:
            continue
        for evidence in tool.get("evidence", []):
            if (evidence.get("path") == doc_id and type(evidence.get("page_start")) is int and type(evidence.get("page_end")) is int
                    and type(evidence.get("byte_start")) is int and type(evidence.get("byte_end")) is int
                    and evidence["byte_end"] > evidence["byte_start"]):
                pages.update(range(evidence["page_start"], evidence["page_end"] + 1))
    jev = jev_receipt(report)
    initial = jev.get("initial_status") if jev else None
    required = bool(jev and jev.get("required") is True)
    observed = bool(required and type(jev.get("requests")) is int and jev["requests"] > 0
                    and initial in ("reranked", "filtered_all"))
    successful = report.get("status") == "completed"
    searches = jev.get("searches", []) if jev else []
    search_times = [search.get("metrics", {}).get("elapsed_ms") for search in searches]
    return {
        "jev_core_observed": observed, "jev_initial_status": initial, "jev": jev,
        "jev_accounting": jev_cost(jev), "tool_calls": len(tools),
        "tool_successes": sum(tool.get("success") is True for tool in tools),
        "required_initial_tool_calls": sum(tool.get("required_initial") is True for tool in tools),
        "reader_tool_calls": sum(tool.get("required_initial") is not True for tool in tools),
        "tool_failures": sum(tool.get("success") is False for tool in tools),
        "invalid_argument_errors": sum(tool.get("error_code") == "invalid_arguments" for tool in tools)
            if any("error_code" in tool for tool in tools) else None,
        "accessed_physical_pages": sorted(pages), "page_access_recall": len(pages & gold_pages) / len(gold_pages) if gold_pages else None,
        "page_access_semantics": "Physical pages touched by delivered nonempty byte windows; not full-page coverage or semantic entailment",
        "citation_count": len(cited), "citation_validation_completed": successful,
        "cited_target_pages": sorted({page for citation in cited if citation.get("path") == doc_id
                                     for page in range(citation.get("page_start", 1), citation.get("page_end", 0) + 1)}),
        "native_host_usage": report.get("usage"), "native_host_elapsed_ms": report.get("elapsed_ms", report.get("host_retrieval", {}).get("elapsed_ms")),
        "requested_service_tier": report.get("requested_service_tier"),
        "effective_service_tier": report.get("effective_service_tier"),
        "jev_search_elapsed_ms": sum(search_times) if search_times and all(isinstance(value, (int, float)) for value in search_times) else None,
        "native_startup_elapsed_ms": None, "model_inference_elapsed_ms": None,
        "timing_components_may_overlap": True,
        "ledger_path": report.get("ledger_path", report.get("host_retrieval", {}).get("ledger_path")),
        "billing_usd": None,
    }


def evidence_role_policy(report: dict, payload: dict) -> dict | None:
    """Qualify the executed strategy, separately from the requested command flag."""
    searches = (report.get("jev") or {}).get("searches", [])
    observed = [(search, (search.get("plan") or {}).get("evidence_roles"))
                for search in searches if (search.get("plan") or {}).get("evidence_roles") is not None]
    if payload.get("experimental_evidence_roles") is not True:
        if observed:
            raise ValueError("Native workflow enabled an unrequested evidence-role strategy")
        return None
    if len(observed) != 1 or observed[0][0].get("required_initial") is not True:
        raise ValueError("Native workflow did not execute the requested initial evidence-role strategy")
    search, role = observed[0]
    if not isinstance(role, dict) or role.get("strategy") != "jev-evidence-role-v1":
        raise ValueError("Native evidence-role policy differs")
    for field in ("original_question_sha256", "decision_contract_sha256", "request_sha256"):
        if not isinstance(role.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", role[field]):
            raise ValueError("Native evidence-role request/rubric identity is unavailable")
    if role["original_question_sha256"] != hashlib.sha256(payload["question"].encode()).hexdigest():
        raise ValueError("Native evidence-role question differs")
    counts = [role.get(field) for field in ("score_question_count", "choice_question_count", "question_count")]
    if not all(type(value) is int for value in counts):
        raise ValueError("Native evidence-role question accounting is unavailable")
    scores, choices, total = counts
    if not 1 <= scores == choices <= 24 or total != scores + choices:
        raise ValueError("Native evidence-role question accounting differs")
    if scores != (search["plan"].get("coverage") or {}).get("union_candidates"):
        raise ValueError("Native evidence-role candidate count differs from union coverage")
    if type(role.get("request_bytes")) is not int or not 0 < role["request_bytes"] <= 65536:
        raise ValueError("Native evidence-role encoded request bound differs")
    if role.get("delivered_set_sufficiency") != "unassessed":
        raise ValueError("Native evidence-role strategy makes an unsupported sufficiency claim")
    return {field: role[field] for field in (
        "strategy", "original_question_sha256", "decision_contract_sha256", "request_sha256",
        "request_bytes", "question_count", "score_question_count", "choice_question_count", "delivered_set_sufficiency")}


def invoke_native(shared: LocalCodex, arguments: list[str], payload: dict, timeout: int,
                  on_reserved=None) -> tuple[dict, dict]:
    with shared.external_attempt(payload, phase=payload["phase"], model=payload["model"],
                                 effort=payload["reasoning_effort"], service_tier=payload["service_tier"],
                                 role="chat", operation="native_ask") as attempt:
        receipt = attempt.receipt
        if on_reserved is not None:
            on_reserved(receipt)
        response_path = shared.run_dir / "calls" / f"{receipt['ordinal']:05d}.response.json"
        report = {}
        try:
            process = attempt.run(arguments, timeout=timeout)
            if len(process.stdout) > 4 * 1024 * 1024:
                raise ValueError("Native report exceeded4MiB")
            with response_path.open("xb") as output:
                output.write(process.stdout)
            report = locks.read_json(response_path)
            _retain_accounting(receipt, report)
            receipt["response_sha256"] = locks.digest(response_path)
            receipt["exit_code"] = process.returncode
            receipt["jev"] = jev_receipt(report)
            receipt["retrieval_failure"] = report.get("retrieval")
            receipt["host_retrieval_failure"] = report.get("host_retrieval")
            model_records = attempts_from_report(
                report, {"model": payload["model"], "reasoning_effort": payload["reasoning_effort"],
                         "service_tier": payload["service_tier"]},
                required=payload.get("experimental_query_plan") is True,
                planner_profile=planner_profile_from_payload(payload),
            )
            if model_records is not None:
                receipt["model_attempts"] = model_records
                receipt["model_turn_accounting"] = usage_summary(model_records)
                receipt["usage_scope"] = "final_reader"
            receipt["query_plan"] = report.get("query_plan", (report.get("host_retrieval") or {}).get("query_plan"))
            receipt.update(service_tier_metadata(payload["service_tier"], report))
            if process.returncode != 0 or report.get("status") != "completed":
                raise RuntimeError(str(report.get("code", "native_ask_failed")))
            if report.get("model") != payload["model"] or report.get("requested_reasoning_effort") != payload["reasoning_effort"]:
                raise ValueError("Native reader changed model/effort")
            if not tier_matches(payload["service_tier"], report.get("requested_service_tier")):
                raise ValueError("Native reader changed requested service tier")
            if not tier_matches(payload["service_tier"], report.get("effective_service_tier")):
                raise ValueError("Native reader acknowledged a different service tier")
            if report.get("auth_mode") != "chatgpt" or report.get("model_provider") != "openai" or not report.get("thread_id") or not report.get("turn_id"):
                raise ValueError("Native reader identity is unavailable")
            role_policy = evidence_role_policy(report, payload)
            if role_policy is not None:
                receipt["evidence_role_policy"] = role_policy
            if "navigation_overlay" in payload:
                receipt["navigation"] = _builder_models().navigation_receipt(report, payload)
            receipt.update(status="completed", model=report["model"], model_provider=report["model_provider"],
                           thread_id=report["thread_id"], turn_id=report["turn_id"], usage=report.get("usage"),
                           reported_host_elapsed_ms=report.get("elapsed_ms"))
        except Exception as error:
            receipt.update(status="failed", error=str(error), error_code=getattr(error, "code", report.get("code", type(error).__name__)))
            if hasattr(error, "cleanup"):
                receipt["timeout_cleanup"] = error.cleanup
    return report, shared.call_by_ordinal(receipt["ordinal"])


def ledger_recovery(path: Path, *, generation=None, query_sha256=None, document=None,
                    reader_profile=None, planned=False, planner_profile=None) -> dict:
    """Retain cumulative latest-per-search evidence when the final CLI JSON is absent."""
    data = path.read_bytes()
    if len(data) > 4 * 1024 * 1024:
        raise ValueError("Native Jev attempt ledger exceeds its protocol bound")
    events, torn = [], False
    lines = data.splitlines()
    for number, line in enumerate(lines):
        try:
            event = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            if number != len(lines) - 1:
                raise ValueError("Native Jev ledger has a malformed middle record")
            torn = True
            break
        if not isinstance(event, dict) or event.get("schema_version") != "gptgrep.jev-attempt.v1":
            raise ValueError("Native Jev ledger schema differs")
        events.append(event)
    if generation is not None and any(event.get("generation") != generation for event in events):
        raise ValueError("Native ledger generation differs from the case snapshot")
    searches = {}
    for event in events:
        search = event.get("search")
        if isinstance(search, dict) and isinstance(search.get("search_id"), str):
            searches[search["search_id"]] = search
    last = events[-1] if events else {}
    initial = [search for search in searches.values() if search.get("required_initial") is True]
    identity_verified = query_sha256 is None
    workflow_verified = False
    for event in events:
        workflow = event.get("workflow")
        if workflow is not None:
            if not isinstance(workflow, dict) or (query_sha256 is not None and workflow.get("query_sha256") != query_sha256) or workflow.get("document_scope") != document or (generation is not None and workflow.get("generation") != generation):
                raise ValueError("Native model ledger workflow binding differs")
            workflow_verified = True
        if event.get("model_attempts") and workflow is None:
            raise ValueError("Native model observations precede a bound workflow")
    if workflow_verified:
        identity_verified = True
    if query_sha256 is not None:
        if len(initial) == 1:
            if initial[0].get("query_sha256") != query_sha256 or initial[0].get("document_scope") != document:
                raise ValueError("Native ledger query/document scope differs from its owning case")
            identity_verified = True
        elif last.get("attempted_calls", 0) and not workflow_verified:
            raise ValueError("Native ledger has model attempts without a unique initial query binding")
        if any(search.get("document_scope") != document or search.get("generation") not in (None, generation) for search in searches.values()):
            raise ValueError("Native ledger later search scope/generation differs from its owning case")
    usage, models = [], []
    for search in searches.values():
        metrics = search.get("metrics", {})
        usage.extend(metrics.get("jev_usage", []))
        models.extend(metrics.get("jev_models", []))
    # Counters in the final complete event are cumulative across searches.
    # This is recovered accounting, not a fabricated successful HostReport.
    jev = {"required": True, "initial_status": last.get("initial_status"),
           "requests": last.get("requests"), "attempted_calls": last.get("attempted_calls"),
           "unobserved_attempts": last.get("unobserved_attempts"),
           "models": sorted(set(models)), "usage": usage, "searches": list(searches.values()),
           "accounting_complete": bool(identity_verified and not torn and last.get("event") == "completed" and last.get("accounting_complete") is True)}
    model_snapshot = next((event["model_attempts"] for event in reversed(events) if "model_attempts" in event), None)
    tool_receipts = [event["receipt"] for event in events
                     if isinstance(event.get("receipt"), dict) and isinstance(event["receipt"].get("tool"), str)]
    tool_budget = _receipt_tool_budget(tool_receipts)
    model_records = None
    if model_snapshot is not None:
        if not workflow_verified:
            raise ValueError("Native model snapshot has no workflow binding")
        if reader_profile is not None:
            model_records = attempts_from_report({"model_attempts": model_snapshot}, reader_profile,
                                                  required=planned, planner_profile=planner_profile)
    return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data),
            "complete_events": len(events), "torn_trailing_record": torn,
            "terminal_event": last.get("event"), "source": "durable_host_ledger", "case_identity_verified": identity_verified, "jev": jev,
            "model_attempts": model_records, "model_turn_accounting": usage_summary(model_records),
            "tool_budget": tool_budget}


def owned_ledger_paths(corpus: Path, receipt: dict, prior_names=(), *, not_before_unix_ns=None) -> list[Path]:
    """A concurrent reader may inspect only its exact owned native process ledger."""
    pid = receipt.get("host_pid")
    if type(pid) is not int or pid <= 0 or type(not_before_unix_ns) is not int:
        return []
    prior = set(prior_names)
    return sorted(path for path in (corpus / ".gptgrep/host-attempts").glob(f"attempt-*-{pid}-*.jsonl")
                  if path.name not in prior and re.fullmatch(rf"attempt-\d+-{pid}-\d+\.jsonl", path.name)
                  and int(path.name.split("-")[1]) >= not_before_unix_ns
                  and int(path.name.split("-")[1]) <= receipt.get("process_finished_unix_ns", 2**128))


def retained_reader_start(case_dir: Path, case: dict, shared: LocalCodex, payload: dict, identity: str) -> dict:
    attempt_dir = case_dir / "reader-attempts" / f"{case['reader_attempt']:04d}"
    path = attempt_dir / "start.json"
    if path.exists():
        start = locks.read_json(path)
        if start.get("case_identity") != identity:
            raise ValueError("Retained reader start identity differs")
        return start
    claimed = {locks.read_json(saved).get("host_ordinal") for saved in (case_dir / "reader-attempts").glob("*/start.json")}
    orphan = [call for call in shared.calls_for_phase(payload["phase"]) if call["ordinal"] not in claimed]
    if len(orphan) > 1:
        raise ValueError("Ambiguous retained reader reservation; refuse a replacement")
    if not orphan:
        return {"host_ordinal": None, "case_identity": identity, "prior_ledgers": []}
    call = orphan[0]
    if call.get("request_sha256") != hashlib.sha256(json_bytes(payload)).hexdigest():
        raise ValueError("Retained reader reservation payload differs")
    start = {"host_ordinal": call["ordinal"], "case_identity": identity, "prior_ledgers": [], "recovered_reservation": True}
    write_json(path, start)
    return start


def bounded_stage(items: list, concurrency: int, function, shared: LocalCodex, run_dir: Path, name: str) -> list:
    started = time.perf_counter()
    started_ns = time.time_ns()
    execution_id = uuid.uuid4().hex
    stage_path = run_dir / f"stage-{name}.json"
    checkpoint_attempt(stage_path, {"stage": name, "execution_id": execution_id, "status": "started",
                                   "configured_task_concurrency": concurrency, "selected_tasks": len(items),
                                   "started_unix_ns": started_ns, "wall_ms": None})
    task_records, intervals = [], []
    mutex = Lock()
    role = "reader" if name == "readers" else "judge"
    status = "completed"
    def measured(item):
        row = item if role == "reader" else item[0]
        phase = f"{'answer' if role == 'reader' else 'judge'}:native:row-{row['source_row']}"
        before = {call["ordinal"] for call in shared.calls_for_phase(phase)}
        result = None
        try:
            result = function(item)
            return result
        finally:
            after = {call["ordinal"] for call in shared.calls_for_phase(phase)}
            state = (result.get("status") if role == "reader" else result.get("judge", {}).get("status", "not_run")) if result is not None else "interrupted"
            record = {"source_row": row["source_row"], "status": state, "new_final_receipts": len(after - before),
                      "returned_case": result is not None, "reused_completed": state == "completed" and after == before}
            with mutex:
                task_records.append(record)
    try:
        return run_case_pool(items, measured, concurrency, shared, role, intervals)
    except BaseException:
        status = "interrupted"
        raise
    finally:
        checkpoint_attempt(stage_path, {
            "stage": name, "execution_id": execution_id, "status": status, "configured_task_concurrency": concurrency,
            "selected_tasks": len(items), "completed_task_results": sum(row["returned_case"] for row in task_records),
            "not_started_tasks": len(items) - len(task_records), "reused_completed_tasks": sum(row["reused_completed"] for row in task_records),
            "tasks_with_new_receipts": sum(row["new_final_receipts"] > 0 for row in task_records),
            "interrupted_tasks": sum(row["status"] == "interrupted" for row in task_records),
            "tasks": sorted(task_records, key=lambda row: row["source_row"]),
            "task_intervals": intervals, "task_concurrency": task_concurrency_report(intervals),
            "started_unix_ns": started_ns, "finished_unix_ns": time.time_ns(),
            "wall_ms": (time.perf_counter() - started) * 1000,
        })


def stage_history(run_dir: Path) -> dict:
    executions = {}
    for path in sorted((run_dir / "attempts").glob("stage-*/*.json")):
        record = locks.read_json(path)
        executions[record["execution_id"]] = record
    records = list(executions.values())
    known = [record["wall_ms"] for record in records if isinstance(record.get("wall_ms"), (float, int))]
    missing = len(records) - len(known)
    return {"executions": records, "known_active_stage_wall_ms_subtotal": math.fsum(known),
            "missing_stage_durations": missing, "active_stage_wall_ms": math.fsum(known) if missing == 0 else None,
            "latest_invocation_is_not_automatically_cold": True,
            "task_concurrency": task_concurrency_report([interval for record in records for interval in record.get("task_intervals", [])])}


def summarize(cases: list[dict], expected: int) -> dict:
    judged = [case for case in cases if case.get("judge", {}).get("status") == "completed"]
    completed = [case for case in cases if case.get("status") == "completed"]
    complete = expected > 0 and len(cases) == expected and len(completed) == expected and len(judged) == expected
    core = all(case.get("metrics", {}).get("jev_core_observed") for case in completed) and len(completed) == expected
    correct = sum(case["judge"]["equivalent"] is True for case in judged)
    latency = [case["elapsed_ms"] for case in cases if isinstance(case.get("elapsed_ms"), (int, float))]
    costs = [case.get("cumulative_jev_cost", case.get("metrics", {}).get("jev_accounting", {})) for case in cases]
    cost_complete = expected > 0 and len(costs) == expected and all(cost.get("accounting_complete") for cost in costs)
    known_costs = [cost["known_cost_subtotal_usd"] for cost in costs if cost.get("known_cost_subtotal_usd") is not None]
    return {
        "question_denominator": expected, "materialized_cases": len(cases),
        "completed_responses": len(completed), "failed_cases": expected - len(completed),
        "judged": len(judged), "judge_unavailable": expected - len(judged), "correct": correct,
        "comparison_eligible": complete and core,
        "answer_equivalence_accuracy": correct / expected if complete and core and expected else None,
        "observed_judge_accuracy": correct / expected if len(judged) == expected and expected else None,
        "observed_judge_accuracy_lower_bound": correct / expected if expected else None,
        "mean_page_access_recall": statistics.mean(case["metrics"]["page_access_recall"] for case in cases)
            if expected > 0 and len(cases) == expected and all(isinstance(case.get("metrics", {}).get("page_access_recall"), (int, float)) for case in cases) else None,
        "latency_ms": {"samples": len(latency), "median": statistics.median(latency) if latency else None,
                       "p95": sorted(latency)[min(len(latency) - 1, math.ceil(len(latency) * .95) - 1)] if latency else None},
        "jev_cost_accounting_complete": cost_complete,
        "measured_jev_cost_usd": math.fsum(known_costs) if cost_complete and known_costs else None,
        "known_jev_cost_subtotal_usd": math.fsum(known_costs) if known_costs else None,
        "total_provider_billing_usd": None,
        "latency_scope": "latest selected reader attempt; all attempt times remain in cumulative accounting",
        "no_universal_winner_claim": True,
    }


def compare_reports(native: dict, baseline: dict) -> dict:
    reasons = []
    if native.get("summary", {}).get("comparison_eligible") is not True or baseline.get("baseline_eligible") is not True:
        reasons.append("Both systems must have complete eligible outcomes")
    for field in ("source_rows", "source_hashes", "question_sha256", "cohort_manifest_sha256"):
        if native.get(field) != baseline.get(field):
            reasons.append(f"Different {field}")
    for role in ("chat", "judge"):
        if native.get("profile", {}).get("roles", {}).get(role) != baseline.get("profile", {}).get("roles", {}).get(role):
            reasons.append(f"Different {role} profile")
    for role in ("reader", "judge"):
        if native.get("host_concurrency_by_role", {}).get(role) != baseline.get("host_concurrency_by_role", {}).get(role):
            reasons.append(f"Different {role} concurrency")
    if native.get("qa_stage_strategy") != baseline.get("qa_stage_strategy"):
        reasons.append("Different reader/judge stage strategy")
    if not native.get("known_document_scope") or not baseline.get("known_document_scope"):
        reasons.append("Known-document protocol scope is not established for both")
    baselines = baseline.get("answers", [])
    if len({row.get("variant") for row in baselines}) != 1:
        reasons.append("Select one baseline variant per paired comparison")
    base_map = {row["source_row"]: row for row in baselines}
    pairs = []
    if not reasons:
        for case in native["cases"]:
            other = base_map[case["source_row"]]
            for field in ("rubric_sha256", "schema_sha256"):
                if case["judge"].get(field) != other["judge"].get(field):
                    reasons.append(f"Different judge {field}")
            pairs.append({"source_row": case["source_row"], "native_correct": case["judge"]["equivalent"],
                          "baseline_correct": other["judge"]["equivalent"],
                          "native_page_recall": case["metrics"]["page_access_recall"],
                          "baseline_page_recall": other.get("page_access_recall"),
                          "native_latency_ms": case["elapsed_ms"], "baseline_latency_ms": other.get("elapsed_ms")})
    return {"compatible": not reasons, "reasons": reasons, "paired_cases": pairs if not reasons else [],
            "g5_pass": None, "universal_winner": None,
            "limits": ["Fresh-host SDK versus persistent native-reader overhead differs",
                       "Baseline citation fidelity may be unavailable; do not substitute zero",
                       "Native page access touches byte windows; baseline page tools return whole pages",
                       "Paired quality, grounding, build work and model usage require joint interpretation"]}


def materialize_corpus(source_directory: Path, corpus: Path, source_hashes: dict) -> None:
    corpus.mkdir(exist_ok=True)
    if {path.name for path in corpus.iterdir() if not path.name.startswith(".")} - set(source_hashes):
        raise ValueError("Private corpus contains an unexpected document")
    for name, expected in source_hashes.items():
        destination = corpus / name
        if destination.parent != corpus or destination.suffix.lower() != ".pdf":
            raise ValueError("Only source-relative PDF filenames enter the evaluation corpus")
        if destination.exists():
            if locks.digest(destination) != expected:
                raise ValueError("Existing private source differs; never overwrite a source beneath citations")
            continue
        if (corpus / ".gptgrep").exists():
            raise ValueError("An indexed corpus has a missing source; use a new private run")
        temporary = corpus / (".copying-" + name)
        shutil.copyfile(source_directory / name, temporary)
        if locks.digest(temporary) != expected:
            raise ValueError("Source changed during private corpus copy")
        temporary.replace(destination)
        if locks.digest(destination) != expected:
            raise ValueError("Source changed during private corpus copy")


def run_binding(manifest: dict) -> str:
    if manifest.get("schema_version") == "gptgrep.system-eval.v4":
        return fingerprint(manifest)
    # The invocation ceiling may change; existing attempts still consume it.
    # This never changes reader conditions or the selected cohort.
    return fingerprint({key: value for key, value in manifest.items() if key != "max_host_invocations"})


def reuse_manifest(path: Path, manifest: dict) -> str:
    binding = run_binding(manifest)
    if path.exists():
        if run_binding(locks.read_json(path)) != binding:
            raise ValueError("Native run source/binary/profile/query/input conditions changed; use a new run directory")
    else:
        with path.open("x", encoding="utf-8") as output:
            json.dump(manifest, output, ensure_ascii=False, indent=2, allow_nan=False)
            output.flush()
            os.fsync(output.fileno())
    return binding


def can_retry_reader(case: dict, retry_failed: bool) -> bool:
    status = case.get("status")
    return status in ("not_run", "budget_blocked", "build_failed") or (retry_failed and status != "completed")


def completed_host_response(ordinal: int, request: dict, model: str, effort: str,
                            phase: str, shared: LocalCodex, service_tier: str) -> tuple[dict, dict] | None:
    """Reconcile durable outcomes before deciding whether a call may be retried."""
    if type(ordinal) is not int or ordinal < 1:
        raise ValueError("Invalid retained host ordinal")
    call = shared.call_by_ordinal(ordinal)
    if call is None:
        return None
    request_path, response_path = (shared.run_dir / "calls" / f"{ordinal:05d}.{kind}.json" for kind in ("request", "response"))
    expected = hashlib.sha256(json_bytes(request)).hexdigest()
    if call.get("ordinal") != ordinal or call.get("request_sha256") != expected or locks.digest(request_path) != expected:
        raise ValueError("Retained host request binding differs")
    if locks.read_json(request_path) != request:
        raise ValueError("Retained host request contents differ")
    if call.get("status") != "completed":
        if response_path.exists():
            if call.get("response_sha256") and call["response_sha256"] != locks.digest(response_path):
                raise ValueError("Retained host response digest differs")
            try:
                previous = locks.read_json(response_path)
            except (ValueError, UnicodeDecodeError):
                previous = None
            if isinstance(previous, dict) and previous.get("status") == "completed":
                raise ValueError("A retained completed response lacks a validated completion receipt; refuse a replacement call")
        return None
    if (call.get("phase"), call.get("requested_model"), call.get("requested_effort")) != (phase, model, effort):
        raise ValueError("Retained host phase or profile differs")
    if not tier_matches(service_tier, call.get("requested_service_tier")):
        raise ValueError("Retained host requested service tier differs")
    if locks.digest(response_path) != call.get("response_sha256"):
        raise ValueError("Retained host response digest differs")
    report = locks.read_json(response_path)
    if report.get("status") != "completed" or report.get("model") != model or report.get("requested_reasoning_effort") != effort:
        raise ValueError("Retained host outcome or profile differs")
    if report.get("effective_reasoning_effort") not in (None, effort):
        raise ValueError("Retained host effective effort differs")
    if not tier_matches(service_tier, report.get("requested_service_tier")):
        raise ValueError("Retained host reported service tier differs")
    if not tier_matches(service_tier, report.get("effective_service_tier")):
        raise ValueError("Retained host acknowledged service tier differs")
    if report.get("auth_mode") != "chatgpt" or report.get("model_provider") != "openai":
        raise ValueError("Retained host runtime differs")
    identity = report.get("thread_id"), report.get("turn_id")
    if not all(isinstance(value, str) and value for value in identity) or identity != (call.get("thread_id"), call.get("turn_id")):
        raise ValueError("Retained host native session differs")
    if request.get("operation") == "ask":
        model_records = attempts_from_report(report, {"model": model, "reasoning_effort": effort, "service_tier": service_tier},
                                             required=request.get("experimental_query_plan") is True,
                                             planner_profile=planner_profile_from_payload(request))
        if model_records is not None and call.get("model_attempts") != model_records:
            raise ValueError("Retained nested model accounting differs from its bound response")
        if call.get("evidence_role_policy") != evidence_role_policy(report, request):
            raise ValueError("Retained evidence-role qualification differs from its bound response")
        if "navigation_overlay" in request and call.get("navigation") != _builder_models().navigation_receipt(report, request):
            raise ValueError("Retained navigation receipt differs from its bound response")
    return report, call


def validate_cached_reader(case: dict, row: dict, args, run_dir: Path, shared: LocalCodex,
                           corpus: Path, source_hashes: dict, canonical: dict, generation: dict) -> dict:
    receipt = case["host_receipt"]
    ordinal = receipt["ordinal"]
    if type(ordinal) is not int or ordinal < 1 or shared.call_by_ordinal(ordinal) != receipt:
        raise ValueError("Cached reader receipt differs from the cumulative host ledger")
    payload = reader_payload(row, args)
    completed = completed_host_response(ordinal, payload, args.model, args.reasoning_effort, payload["phase"], shared, args.service_tier)
    if completed is None:
        raise ValueError("Cached reader has no completed host outcome")
    report, _ = completed
    if report.get("status") != "completed" or report.get("generation") != generation["generation"]:
        raise ValueError("Cached reader status or index generation differs")
    if report.get("model") != args.model or report.get("requested_reasoning_effort") != args.reasoning_effort:
        raise ValueError("Cached reader model or effort differs")
    if not isinstance(report.get("answer"), str) or not isinstance(report.get("tool_calls"), list) or not isinstance(report.get("citations"), list) or not isinstance(report.get("jev"), dict):
        raise ValueError("Cached reader response structure differs")
    verify_native_evidence(report, corpus, {row["doc_id"]: source_hashes[row["doc_id"]]}, canonical)
    return report


def reader_payload(row: dict, args) -> dict:
    planned, evidence_roles = query_strategy_flags(args)
    payload = {"operation": "ask", "phase": f"answer:native:row-{row['source_row']}",
            "question": row["question"], "document": row["doc_id"],
            "model": args.model, "reasoning_effort": args.reasoning_effort, "service_tier": args.service_tier}
    if planned:
        payload["experimental_query_plan"] = True
        payload["planner_model"] = selected_planner_profile(args)["model"]
        payload["planner_reasoning_effort"] = selected_planner_profile(args)["reasoning_effort"]
        payload["max_input_bytes"] = getattr(args, "max_input_bytes", 262144)
    if evidence_roles:
        payload["experimental_evidence_roles"] = True
    if enrichment_enabled(args):
        navigation_arguments(args)  # Fail closed when enrichment is not complete.
        payload["navigation_overlay"] = dict(args._enrichment_publication)
        payload["navigation_max_jev_calls"] = args._enrichment_config["navigation_max_jev_calls"]
        payload["navigation_max_request_bytes"] = args._enrichment_config["navigation_max_request_bytes"]
    return payload


def recover_judge(ordinal: int, prompt: str, constants: dict, host: RoleHost) -> tuple[dict, dict] | None:
    request = {"instructions": prompt, "state": {}, "schema": constants["SCHEMA"]}
    completed = completed_host_response(ordinal, request, host.model, host.effort, host.phase, host.shared, host.service_tier)
    if completed is None:
        return None
    report, _ = completed
    value = report.get("value")
    jsonschema.Draft202012Validator(constants["SCHEMA"]).validate(value)
    return value, report


def validate_cached_judge(judge: dict, prompt: str, constants: dict, host: RoleHost) -> None:
    ordinal = judge.get("host_ordinal")
    if type(ordinal) is not int or ordinal < 1 or host.shared.call_by_ordinal(ordinal) is None:
        raise ValueError("Cached judge has no cumulative host receipt")
    completed = recover_judge(ordinal, prompt, constants, host)
    if completed is None:
        raise ValueError("Cached judge has no completed host outcome")
    value, _ = completed
    if any(judge.get(key) != value.get(key) for key in constants["SCHEMA"].get("required", [])):
        raise ValueError("Cached judge verdict differs from its native response")


def cumulative_accounting(shared: LocalCodex, run_dir: Path) -> dict:
    """Count every native ask attempt, including retries and missing final output."""
    recoveries = {}
    for path in (run_dir / "cases").glob("*/reader-attempts/*/accounting.json"):
        value = locks.read_json(path)
        ordinal = value["ordinal"]
        if value.get("host_started") is False and ordinal is None:
            continue
        if ordinal in recoveries:
            raise ValueError("Duplicate native-attempt accounting ordinal")
        recoveries[ordinal] = value
    attempts = []
    for call in shared.calls:
        if call.get("operation") != "native_ask" and not call.get("phase", "").startswith("answer:native:"):
            continue
        ordinal = call["ordinal"]
        jev = call.get("jev")
        source = "host_receipt"
        if jev is None:
            response = run_dir / "calls" / f"{ordinal:05d}.response.json"
            if response.exists() and call.get("response_sha256") == locks.digest(response):
                try:
                    jev = jev_receipt(locks.read_json(response))
                    source = "retained_cli_response"
                except (ValueError, TypeError):
                    pass
        if jev is None and ordinal in recoveries:
            jev, source = recoveries[ordinal].get("jev"), "durable_host_ledger"
        model_records = call.get("model_attempts")
        model_source = "host_receipt" if model_records is not None else "unavailable"
        if model_records is None:
            response = run_dir / "calls" / f"{ordinal:05d}.response.json"
            request = run_dir / "calls" / f"{ordinal:05d}.request.json"
            if (response.exists() and request.exists() and call.get("response_sha256") == locks.digest(response)
                    and call.get("request_sha256") == locks.digest(request)):
                payload = locks.read_json(request)
                try:
                    model_records = attempts_from_report(
                        locks.read_json(response),
                        {"model": payload["model"], "reasoning_effort": payload["reasoning_effort"],
                         "service_tier": payload["service_tier"]},
                        required=payload.get("experimental_query_plan") is True,
                        planner_profile=planner_profile_from_payload(payload),
                    )
                    if model_records is not None:
                        model_source = "bound_native_response"
                except (KeyError, ValueError, TypeError):
                    # Retain unavailable accounting; never fabricate a model step.
                    pass
        if model_records is None and ordinal in recoveries:
            model_records = recoveries[ordinal].get("model_attempts")
            if model_records is not None:
                model_source = "bound_durable_host_ledger"
        attempts.append({"ordinal": ordinal, "phase": call.get("phase"), "status": call.get("status"),
                         "source": source if jev is not None else "unavailable", "jev": jev,
                         "cost": jev_cost(jev), "elapsed_ms": call.get("elapsed_ms"), "host_usage": call.get("usage"),
                         "legacy_host_usage_scope": "final_reader", "model_attempts": model_records,
                         "model_accounting_source": model_source})
    costs = [attempt["cost"] for attempt in attempts]
    known = [cost["known_cost_subtotal_usd"] for cost in costs if cost.get("known_cost_subtotal_usd") is not None]
    complete = bool(costs) and all(cost.get("accounting_complete") for cost in costs)
    durations = [attempt["elapsed_ms"] for attempt in attempts if isinstance(attempt["elapsed_ms"], (int, float))]
    duration_missing = len(attempts) - len(durations)
    duration_known = math.fsum(durations)
    unavailable_models = sum(attempt.get("model_attempts") is None for attempt in attempts)
    model_totals = usage_summary([record for attempt in attempts for record in (attempt.get("model_attempts") or [])])
    model_totals["unavailable_native_ask_accounting"] = unavailable_models
    model_totals["attempted_calls_known_subtotal"] = model_totals["attempted_calls"]
    if unavailable_models:
        model_totals["attempted_calls"] = None
        model_totals["accounting_complete"] = False
        for total in model_totals["token_totals"].values():
            total["total"] = None
    return {"native_ask_attempts": len(attempts), "attempts": attempts,
            "accounting_complete": complete,
            "unknown_accounting_attempts": sum(not cost.get("accounting_complete") for cost in costs),
            "known_jev_cost_subtotal_usd": math.fsum(known) if known else None,
            "measured_jev_cost_usd": math.fsum(known) if complete and known else None,
            "native_ask_wall_ms": duration_known if duration_missing == 0 else None,
            "native_ask_wall_ms_known_subtotal": duration_known,
            "native_ask_wall_ms_missing": duration_missing,
            "native_model_turn_accounting": model_totals,
            "native_model_accounting_scope": "Planner and final reader steps across all native ask attempts; judge turns remain in their own host ledger. Do not add legacy final-reader usage again."}


def native_snapshot(corpus: Path, source_hashes: dict) -> dict:
    pointer = locks.read_json(corpus / ".gptgrep/CURRENT.json")
    generation = pointer["generation"]
    if not isinstance(generation, str) or not re.fullmatch(r"[A-Za-z0-9-]+", generation):
        raise ValueError("Invalid native generation")
    directory = corpus / ".gptgrep/generations" / generation
    manifest_file = directory / "manifest.json"
    if locks.digest(manifest_file) != pointer["manifest_sha256"]:
        raise ValueError("Native manifest digest mismatch")
    documents = locks.read_json(manifest_file)["documents"]
    result = {}
    for doc in documents:
        if doc["path"] not in source_hashes or doc["source_sha256"] != source_hashes[doc["path"]] or not re.fullmatch(r"[0-9a-f]{24}", doc["id"]):
            raise ValueError("Native document provenance mismatch")
        data = (directory / "text" / (doc["id"] + ".txt")).read_bytes()
        if hashlib.sha256(data).hexdigest() != doc["text_sha256"]:
            raise ValueError("Canonical extraction digest mismatch")
        result[doc["path"]] = data
    if set(result) != set(source_hashes):
        raise ValueError("Native snapshot document coverage differs")
    return result


def verify_native_evidence(report: dict, corpus: Path, sources: dict, text: dict) -> dict:
    for name, expected in sources.items():
        if locks.digest(corpus / name) != expected:
            raise ValueError("Selected source changed during native retrieval")
    evidence = [item for tool in report.get("tool_calls", []) if tool.get("success") is True for item in tool.get("evidence", [])]
    citations = report.get("citations", [])
    for item in evidence + citations:
        name = item.get("path")
        if name not in sources or item.get("source_sha256") != sources[name] or locks.digest(corpus / name) != sources[name]:
            raise ValueError("Native evidence source is stale or outside the selected corpus")
        start, end = item.get("byte_start"), item.get("byte_end")
        if type(start) is not int or type(end) is not int or not 0 <= start <= end <= len(text[name]):
            raise ValueError("Native evidence byte range is invalid")
        excerpt = text[name][start:end]
        excerpt.decode("utf-8")
        if hashlib.sha256(excerpt).hexdigest() != item.get("excerpt_sha256"):
            raise ValueError("Native excerpt differs from its source-bound canonical extraction")
    issued = {(item["node_id"], item["byte_start"], item["byte_end"], item["excerpt_sha256"]) for item in evidence}
    if any((item["node_id"], item["byte_start"], item["byte_end"], item["excerpt_sha256"]) not in issued for item in citations):
        raise ValueError("Native citation was not issued as evidence")
    return {"source_digest_verified": True, "raw_evidence_integrity_verified": True if evidence else None,
            "citation_byte_integrity_verified": True if citations else None,
            "issued_citation_coverage": 1.0 if citations else None,
            "semantic_citation_entailment_verified": None}


def execute(args) -> dict:
    args.codex_bin = normalize_codex_launcher(args.codex_bin)
    planned, evidence_roles = query_strategy_flags(args)
    profile = profiles.resolve(args)
    planner = selected_planner_profile(args) if planned else None
    if planned:
        profile["roles"]["query_planner"] = dict(planner)
    profile["roles"]["index"] = {"engine": "deterministic_native", "model": None, "reasoning_effort": None, "service_tier": None}
    profile["index_effort_note"] = "Native build is deterministic; no Jev or generative indexing stage is claimed."
    enrichment = enrichment_config(args, profile)
    if enrichment is not None:
        args._enrichment_config = enrichment
        profile["roles"]["builder"] = dict(enrichment["builder"])
        if getattr(args, "price_card", None) is None:
            raise ValueError("enrichment_requires_explicit_price_card")
        price_path = args.price_card.expanduser().resolve()
        args._price_card = _builder_models().validate_price_card(_builder_models().read_json(price_path, 1024 * 1024))
    if not 1 <= args.reader_concurrency <= 64 or not 1 <= args.judge_concurrency <= 64:
        raise ValueError("Reader/judge concurrency must be1..64")
    binary = args.binary.expanduser().resolve()
    judge_binary = (args.judge_binary or args.binary).expanduser().resolve()
    benchmark, upstream = args.benchmark.expanduser().resolve(), args.upstream.expanduser().resolve()
    judge_source = args.judge_source.expanduser().resolve()
    if enrichment is not None:
        run_dir = args.run_dir.expanduser().resolve()
        private_directory(run_dir)
        cli_contract_sha256 = enrichment_preflight(binary, run_dir)
    verified = locks.verify(upstream, benchmark, judge_source)
    questions = locks.read_json(benchmark / "questions.json")
    groups, cohort_sha = cohorts.load(len(questions), locks.digest(benchmark / "questions.json"))
    indices = cohorts.select(args.rows, len(questions), groups)
    rows = [{"source_row": index, **questions[index]} for index in indices]
    names = sorted({row["doc_id"] for row in rows})
    run_dir = args.run_dir.expanduser().resolve()
    private_directory(run_dir)
    corpus = run_dir / "corpus"
    manifest = {
        "schema_version": "gptgrep.system-eval.v3", "variant": "gptgrep-native-jev",
        "profile": profile, "source_rows": indices, "question_count": len(rows), "document_count": len(names),
        "cohort_manifest_sha256": cohort_sha,
        "question_sha256": fingerprint(rows), "source_hashes": {name: locks.digest(benchmark / "documents" / name) for name in names},
        "binary_sha256": locks.digest(binary), "judge_binary_sha256": locks.digest(judge_binary),
        "source_and_dependencies": verified, "known_document_scope": True,
        "max_host_invocations": args.max_model_calls, "host_timeout_secs": args.timeout,
        "host_concurrency_by_role": {"reader": args.reader_concurrency, "judge": args.judge_concurrency},
        "qa_stage_strategy": "readers_then_judges",
        "host_input_cap": args.max_input_bytes, "build_timeout_secs": args.build_timeout,
        "service_tier": args.service_tier,
        "effective_tier_basis": "Codex thread/start acknowledgement; not independent provider billing confirmation",
        "codex_bin": args.codex_bin, "codex_home": str(args.codex_home.expanduser().resolve()),
        "max_tool_calls": args.max_tool_calls, "jev_model_requested": args.jev_model,
        "judge_source_sha256": locks.digest(judge_source / "eval/judge.py"),
        "adapter_files": {name: locks.digest(REPO / "scripts/pageindex_baseline" / name)
                          for name in ("bridge.py", "role_hosts.py", "profiles.py", "locks.py", "run.py", "cohorts.py", "native_models.py")},
        "runner_sha256": locks.digest(Path(__file__)),
        "build": {"engine": "native LiteParse/Rust tree/tgrep index", "generative": False,
                  "jev_indexing_stage": False, "optimize_merge": args.optimize_merge},
    }
    if planned:
        manifest["query_strategy"] = {
            "experimental_query_plan": True, "planner": dict(planner),
            "max_planner_turns_per_native_ask": 1, "max_native_model_attempts_per_ask": 2,
            "planner_input_cap": min(args.max_input_bytes, 32768), "planner_output_cap": 4096,
            "planner_timeout_secs": min(args.timeout, 45),
            "max_alternate_queries": 2, "routing_concurrency": 2, "union_candidates": 24,
            "final_relevance_question": "original question unchanged",
            "deadline": "one timeout shared by planner, routing, rerank and final reader",
            "model_turn_admission_upper_bound": 2 * args.max_model_calls,
            "budget_note": "max_host_invocations caps outer processes; each native ask admits at most two model steps. Nested attempts are reported separately; this is not a provider request count.",
        }
        if evidence_roles:
            manifest["query_strategy"]["experimental_evidence_roles"] = True
            manifest["query_strategy"]["evidence_role_selection"] = {
                "policy": "jev-evidence-role-v1",
                "score_questions_per_candidate": 1, "choice_questions_per_candidate": 1,
                "maximum_final_questions": 48, "complete_request_byte_cap": 65536,
                "shared_role_definition_byte_cap": 2048,
                "private_diagnostic_block_byte_cap": 24576,
                "ordering": "original literal anchor, evidence role, relevance, source coordinates",
                "roles": ["direct_support", "source_local_incomplete", "background", "no_support"],
                "eligibility_and_score_floor": "unchanged",
                "initial_jev_operation_cap": 4,
                "delivered_set_sufficiency": "unassessed",
                "scope_changes": "invalidate the delivered role hint",
                "cost_note": "Extra judgments and request bytes are measured within the existing union call; request count alone does not establish cost neutrality.",
            }
    if enrichment is not None:
        if args.max_model_calls < 2 * len(rows) + 1:
            raise ValueError("enrichment_outer_cap_cannot_cover_one_complete_primary_cohort")
        manifest.update(schema_version="gptgrep.system-eval.v4", variant="gptgrep-native-enriched-navigation",
                        cli_contract_sha256=cli_contract_sha256,
                        enrichment={"protocol": "raw-complete-overlay-v1", **enrichment,
                                    "prepared_binding": "enrichment/prepared.json", "reader_binding": "enrichment/ready.json",
                                    "no_reader_fallback": True, "no_failed_call_retry": True,
                                    "logical_primary_model_turn_upper_bound": enrichment["max_builder_calls"] + 3 * len(rows),
                                    "scope": "Deterministic index, separate raw-only builder, selected-model planner/reader, separately bound GPT-6 judge, fixed Jev",
                                    "model_policy": "gpt6_fast_xhigh_or_max_all_model_roles_v1"})
        manifest["price_card"] = {"path": str(price_path), "sha256": locks.digest(price_path), "card_id": args._price_card["card_id"],
                                  "schema_version": args._price_card["schema_version"]}
        manifest["cost_comparison_policy"] = {
            "g5_scope": "QA planner+reader Standard-normalized API-equivalent plus observed ask Jev cost",
            "g5_excludes": ["deterministic_index", "enrichment_builder_and_support_jev", "independent_judge"],
            "price_normalization": "Standard is a comparison normalization; live service remains the requested Fast tier",
            "baseline_tier_basis": "Original PageIndex Standard is inferred from README/no Fast declaration, not billing proof",
            "qa_strict_threshold_usd_equivalent": _builder_models().QA_COST_THRESHOLD,
            "full_cohort_questions": 62, "quality_min_correct": 61,
            "actual_chatgpt_billing_observed": False, "missing_metering_or_receipts": "unavailable; gate does not pass",
            "original_baseline_judge_comparison": "Original aggregate has no retained per-task predictions to rejudge; GPT-6 judge comparison is cross-protocol. Adapted R8 rejudgments remain separate references.",
        }
        manifest["adapter_files"]["native_builder_models.py"] = locks.digest(REPO / "scripts/pageindex_baseline/native_builder_models.py")
        manifest["query_strategy"]["navigation"] = {
            "required": True, "report_schema": "gptgrep.navigation-query.v1",
            "expected_overlay_binding": "enrichment/ready.json", "full_hint_scan_required": True,
            "max_jev_calls": enrichment["navigation_max_jev_calls"],
            "max_request_bytes": enrichment["navigation_max_request_bytes"], "citation_authority": False,
        }
        references = {}
        for attribute, name, scope in (("original_pageindex_results", "original_pageindex_results", "original_oss_aggregate_reference"),
                                       ("baseline_summary", "adapted_trace_summary", "live_adapted_trace_reference")):
            value = getattr(args, attribute, None)
            if value is not None:
                path = value.expanduser().resolve()
                references[name] = {"path": str(path), "sha256": locks.digest(path), "scope": scope}
        original_documents = getattr(args, "original_pageindex_documents", None) or benchmark / "documents.json"
        if original_documents.exists():
            path = original_documents.expanduser().resolve()
            references["original_pageindex_documents"] = {"path": str(path), "sha256": locks.digest(path), "scope": "original_oss_index_cost_metadata_only"}
        manifest["external_reference_bindings"] = references
        manifest["comparison_note"] = "Original PageIndex results and adapted live trace are separate references; model/profile differences require explicit external interpretation."
    with (run_dir / ".owner.lock").open("a") as owner:
        fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
        binding = reuse_manifest(run_dir / "manifest.json", manifest)
        checkpoint_attempt(run_dir / "invocation.json", {"stage": args.stage, "run_binding": binding,
                           "max_host_invocations": args.max_model_calls, "retry_failed": args.retry_failed})
        if args.stage == "plan" and enrichment is None:
            ledger = run_dir / "host-calls.jsonl"
            count = len(ledger.read_text().splitlines()) if ledger.exists() else 0
            return {**manifest, "status": "plan_prepared", "comparison_eligible": False,
                    "host_invocations": count, "new_host_invocations": 0}
        if args.max_model_calls <= 0:
            raise ValueError("Live evaluation requires an explicit positive host-invocation budget")
        materialize_corpus(benchmark / "documents", corpus, manifest["source_hashes"])
        shared = None if enrichment is not None and args.stage == "plan" else LocalCodex(judge_binary, args.codex_bin, args.codex_home.expanduser().resolve(), run_dir,
                            args.max_model_calls, args.timeout, args.model, args.reasoning_effort, args.max_input_bytes,
                            service_tier=args.service_tier, reader_concurrency=args.reader_concurrency,
                            judge_concurrency=args.judge_concurrency)
        build_path = run_dir / "build.json"
        build = locks.read_json(build_path) if build_path.exists() else {"status": "not_built"}
        if build["status"] != "completed" and (build["status"] == "not_built" or args.retry_failed):
            retained_calls = shared.calls if shared is not None else [json.loads(line) for line in (run_dir / "host-calls.jsonl").read_text().splitlines()] if (run_dir / "host-calls.jsonl").exists() else []
            if any(call.get("phase", "").startswith("answer:native:") or call.get("operation") == "native_enrich" for call in retained_calls):
                raise ValueError("Refuse to rebuild an index beneath retained reader attempts or citations")
            attempt = int(build.get("build_attempt", 0)) + 1
            build = {"status": "started", "build_attempt": attempt, "model_invocations": 0, "jev_indexing_stage": False}
            checkpoint_attempt(build_path, build)
            started = time.perf_counter()
            try:
                process = owned_process(build_arguments(binary, corpus, args.optimize_merge), run_dir, args.build_timeout)
                raw_path = run_dir / f"build-{attempt:04d}.response.json"
                with raw_path.open("xb") as output:
                    output.write(process.stdout)
                built = locks.read_json(raw_path)
                build.update(result=built, exit_code=process.returncode, response_sha256=locks.digest(raw_path))
                if process.returncode != 0 or built.get("indexed_files") != len(names):
                    raise ValueError("Native corpus build did not complete every selected document")
                native_snapshot(corpus, manifest["source_hashes"])
                build.update(status="completed", generation_binding=locks.read_json(corpus / ".gptgrep/CURRENT.json"))
            except Exception as error:
                build.update(status="failed", error=str(error))
            build["wall_ms"] = (time.perf_counter() - started) * 1000
            checkpoint_attempt(build_path, build)
        build_ok = build["status"] == "completed"
        if build_ok and locks.read_json(corpus / ".gptgrep/CURRENT.json") != build["generation_binding"]:
            raise ValueError("Cached native generation changed; never rebuild beneath saved citations")
        canonical = native_snapshot(corpus, manifest["source_hashes"]) if build_ok else {}
        enrichment_result = None
        if enrichment is not None:
            if not build_ok:
                if shared is not None:
                    shared.close(cancel=True)
                return enrichment_preparation_failure(manifest, rows, run_dir, build,
                    {"status": "not_started", "reader_admitted": False, "accounting": {"available": False}}, stage="build_failed")
            enrichment_plan = None
            try:
                prepared, enrichment_plan = prepare_enrichment(binary, corpus, args, run_dir, manifest, build, canonical)
                if args.stage == "plan":
                    return {**manifest, "status": "plan_prepared", "comparison_eligible": False, "build_result": build,
                            "enrichment_prepared": prepared, "host_invocations": 0, "new_host_invocations": 0,
                            "expected_enrichment_plan_sha256": prepared["plan_sha256"]}
                enrichment_result = run_enrichment(binary, corpus, args, run_dir, shared, prepared, enrichment_plan)
            except Exception as error:
                retained_accounting = {"available": False}
                ledger_path = run_dir / "enrichment/ledger.jsonl"
                retained_changed = any(marker in str(error) for marker in (
                    "prior_ledger_", "retained_request_changed", "retained_response_changed", "ready_binding_changed"))
                if enrichment_plan is not None and ledger_path.exists() and not retained_changed:
                    try:
                        retained_accounting = enrichment_accounting_view(_builder_models().validate_ledger(
                            ledger_path, enrichment_plan, allow_partial=True, price_card=args._price_card))
                    except (ValueError, OSError, KeyError, TypeError):
                        pass
                enrichment_result = {"status": "failed", "reader_admitted": False, "error_type": type(error).__name__,
                                     "error": str(error) if isinstance(error, ValueError) else "enrichment_preparation_or_admission_failed",
                                     "accounting": retained_accounting}
            if enrichment_result.get("reader_admitted") is not True:
                if shared is not None:
                    shared.close(cancel=True)
                return enrichment_preparation_failure(manifest, rows, run_dir, build, enrichment_result,
                    stage="enrichment_incomplete" if enrichment_result["status"] == "incomplete" else "enrichment_failed")
        constants = judge_constants(judge_source)
        def read_case(row):
            case_dir = run_dir / "cases" / f"{row['source_row']:03d}"
            case_dir.mkdir(parents=True, exist_ok=True)
            case_path = case_dir / "case.json"
            identity = fingerprint({"row": row, "run_binding": binding})
            initial = {"source_row": row["source_row"], "doc_id": row["doc_id"], "status": "not_run", "case_identity": identity}
            case = locks.read_json(case_path) if case_path.exists() else initial.copy()
            if case.get("case_identity") != identity:
                raise ValueError("Cached native case inputs differ")
            if enrichment is not None and case.get("status") in ("build_failed", "enrichment_failed", "enrichment_incomplete"):
                if case.get("reader_attempt") is not None or case.get("host_receipt") is not None:
                    raise ValueError("enrichment_failed_preparation_has_retained_reader")
                case = initial.copy()
            if case["status"] != "completed" and case.get("reader_attempt") is not None:
                attempt_dir = case_dir / "reader-attempts" / f"{case['reader_attempt']:04d}"
                payload = reader_payload(row, args)
                start = retained_reader_start(case_dir, case, shared, payload, identity)
                accounting_path = attempt_dir / "accounting.json"
                accounting = locks.read_json(accounting_path) if accounting_path.exists() else {}
                never_started = accounting.get("host_started") is False
                if never_started:
                    if accounting.get("ordinal") is not None or start["host_ordinal"] is not None:
                        raise ValueError("Retained no-invocation marker differs")
                completed = None if start["host_ordinal"] is None else completed_host_response(
                    start["host_ordinal"], payload, args.model, args.reasoning_effort, payload["phase"], shared, args.service_tier)
                if completed is not None:
                    if not build_ok:
                        raise ValueError("Retained completed reader has no validated stable index")
                    saved_report, receipt = completed
                    recovered_case = {**case, "host_receipt": receipt}
                    saved_report = validate_cached_reader(recovered_case, row, args, run_dir, shared, corpus,
                                                          manifest["source_hashes"], canonical, build["generation_binding"])
                    metrics = native_metrics(saved_report, row["doc_id"], set(json.loads(row["evidence_pages"])))
                    metrics.update(verify_native_evidence(saved_report, corpus,
                                   {row["doc_id"]: manifest["source_hashes"][row["doc_id"]]}, canonical))
                    recovered_case.update(status="completed", elapsed_ms=receipt.get("elapsed_ms"),
                                          metrics=metrics, recovered_completed_reader=True)
                    recovered_case.pop("error", None)
                    recovered_case.pop("error_code", None)
                    accounting_path = attempt_dir / "accounting.json"
                    if not accounting_path.exists():
                        write_json(accounting_path, {"ordinal": receipt["ordinal"], "jev": jev_receipt(saved_report),
                                   "model_attempts": receipt.get("model_attempts")})
                    case = recovered_case
                    checkpoint_attempt(case_path, case)
            if case["status"] in ("queued", "started", "validating"):
                # The previous owner ended without a final case checkpoint. Keep
                # that attempt and its uncertain usage; do not replay it implicitly.
                attempt_dir = case_dir / "reader-attempts" / f"{case['reader_attempt']:04d}"
                start = retained_reader_start(case_dir, case, shared, reader_payload(row, args), identity)
                accounting_path = attempt_dir / "accounting.json"
                if not accounting_path.exists():
                    if start["host_ordinal"] is None:
                        # The case checkpoint preceded request creation. No native
                        # process can have started without that durable request.
                        write_json(accounting_path, {"ordinal": None, "host_started": False, "jev": None})
                    else:
                        receipt = shared.call_by_ordinal(start["host_ordinal"]) or {}
                        fresh = owned_ledger_paths(corpus, receipt, start["prior_ledgers"],
                                                   not_before_unix_ns=start.get("reservation_observed_unix_ns"))
                        try:
                            recovery_path = attempt_dir / "ledger-recovery.json"
                            recovered = locks.read_json(recovery_path) if recovery_path.exists() else [ledger_recovery(
                                path, generation=build["generation_binding"]["generation"],
                                query_sha256=hashlib.sha256(row["question"].encode()).hexdigest(), document=row["doc_id"],
                                reader_profile=profile["roles"]["chat"], planned=planned,
                                planner_profile=planner) for path in fresh]
                            if not recovery_path.exists():
                                write_json(recovery_path, recovered)
                            write_json(accounting_path, {"ordinal": start["host_ordinal"],
                                       "jev": recovered[0]["jev"] if len(recovered) == 1 else None,
                                       "model_attempts": recovered[0].get("model_attempts") if len(recovered) == 1 else None})
                        except Exception as error:
                            write_json(accounting_path, {"ordinal": start["host_ordinal"], "jev": None, "error": str(error)})
                case.update(status="interrupted", error="Prior reader attempt has no completed case checkpoint")
                checkpoint_attempt(case_path, case)
            report = None
            if not build_ok:
                if case["status"] == "completed":
                    raise ValueError("Completed reader has no validated stable index")
                case.update(status="build_failed", error="Native corpus build did not complete")
                checkpoint_attempt(case_path, case)
            elif case["status"] == "completed":
                report = validate_cached_reader(case, row, args, run_dir, shared, corpus,
                                                manifest["source_hashes"], canonical, build["generation_binding"])
            elif can_retry_reader(case, args.retry_failed):
                previous_case = case
                attempt = int(case.get("reader_attempt", 0)) + 1
                attempt_dir = case_dir / "reader-attempts" / f"{attempt:04d}"
                attempt_dir.mkdir(parents=True)
                ledger_directory = corpus / ".gptgrep/host-attempts"
                before_ledgers = set(ledger_directory.glob("*.jsonl"))
                case = {**initial, "status": "queued", "reader_attempt": attempt}
                checkpoint_attempt(case_path, case)
                def reserved(receipt):
                    write_json(attempt_dir / "start.json", {"host_ordinal": receipt["ordinal"],
                               "case_identity": identity, "reservation_observed_unix_ns": time.time_ns(),
                               "prior_ledgers": sorted(path.name for path in before_ledgers)})
                    case.update(status="started")
                    checkpoint_attempt(case_path, case)
                try:
                    report, receipt = invoke_native(shared, ask_arguments(binary, corpus, row, args), reader_payload(row, args),
                                                    args.timeout + 60, on_reserved=reserved)
                    write_json(attempt_dir / "native-response.json", report)
                    case.update(status="validating", elapsed_ms=receipt["elapsed_ms"], host_receipt=receipt,
                                metrics=native_metrics(report, row["doc_id"], set(json.loads(row["evidence_pages"]))))
                    checkpoint_attempt(case_path, case)
                    start = locks.read_json(attempt_dir / "start.json")
                    recovered = [ledger_recovery(path, generation=build["generation_binding"]["generation"],
                                 query_sha256=hashlib.sha256(row["question"].encode()).hexdigest(), document=row["doc_id"],
                                 reader_profile=profile["roles"]["chat"], planned=planned,
                                 planner_profile=planner)
                                 for path in owned_ledger_paths(corpus, receipt, start["prior_ledgers"],
                                 not_before_unix_ns=start["reservation_observed_unix_ns"])]
                    write_json(attempt_dir / "ledger-recovery.json", recovered)
                    case["native_attempt_ledgers"] = [{key: value for key, value in item.items() if key != "jev"} for item in recovered]
                    if jev_receipt(report) is None and len(recovered) == 1:
                        report = {**report, "jev": recovered[0]["jev"], "jev_receipt_source": "durable_host_ledger"}
                    write_json(attempt_dir / "accounting.json", {"ordinal": receipt["ordinal"], "jev": jev_receipt(report),
                               "model_attempts": receipt.get("model_attempts", recovered[0].get("model_attempts") if len(recovered) == 1 else None)})
                    case.update(status=receipt["status"], elapsed_ms=receipt["elapsed_ms"], host_receipt=receipt,
                                metrics=native_metrics(report, row["doc_id"], set(json.loads(row["evidence_pages"]))))
                    case["metrics"]["jev_receipt_source"] = report.get("jev_receipt_source", "final_cli_report" if jev_receipt(report) is not None else "unavailable")
                    if receipt["status"] == "completed":
                        if report.get("generation") != build["generation_binding"]["generation"]:
                            raise ValueError("Native response used a changed index generation")
                        validation_started = time.perf_counter()
                        case["metrics"].update(verify_native_evidence(report, corpus,
                            {row["doc_id"]: manifest["source_hashes"][row["doc_id"]]}, canonical))
                        case["evidence_validation_ms"] = (time.perf_counter() - validation_started) * 1000
                        response = report.get("answer", "")
                        if not isinstance(response, str):
                            raise ValueError("Native response has no answer string")
                    else:
                        case["error"] = receipt.get("error")
                except Exception as error:
                    if getattr(error, "code", None) == "call_budget":
                        case = {**previous_case, "reader_attempt": attempt, "retry_budget_blocked": True}
                        if case["status"] in ("not_run", "budget_blocked", "build_failed"):
                            case["status"] = "budget_blocked"
                    else:
                        case.update(status="failed", error=str(error), error_code=getattr(error, "code", type(error).__name__))
                checkpoint_attempt(case_path, case)
            return case

        def judge_case(item):
            row, case = item
            case_dir = run_dir / "cases" / f"{row['source_row']:03d}"
            case_path = case_dir / "case.json"
            judge_host = RoleHost(shared, "judge", profile["roles"]["judge"], phase=f"judge:native:row-{row['source_row']}")
            report = validate_cached_reader(case, row, args, run_dir, shared, corpus, manifest["source_hashes"], canonical, build["generation_binding"]) if case["status"] == "completed" else None
            if case["status"] == "completed":
                response = report["answer"]
                prompt = constants["PROMPT"].format(question=" ".join(row["question"].split()),
                                                   answer=row["answer"], answer_format=row["answer_format"],
                                                   response=response[:constants["MAX_RESPONSE_CHARS"]])
                judge = case.get("judge", {})
                if judge.get("status") == "queued" and judge.get("host_ordinal") is None:
                    retained = shared.calls_for_phase(judge_host.phase)
                    if retained:
                        judge = {**judge, "host_ordinal": max(call["ordinal"] for call in retained)}
                        case["judge"] = judge
                if judge.get("status") != "completed" and judge.get("host_ordinal") is not None:
                    recovered = recover_judge(judge["host_ordinal"], prompt, constants, judge_host)
                    if recovered is not None:
                        verdict, completion_report = recovered
                        case["judge"] = judge = {"status": "completed", **verdict, "model": judge_host.model,
                            "reasoning_effort": judge_host.effort, "host_ordinal": judge["host_ordinal"],
                            **service_tier_metadata(judge_host.service_tier, completion_report),
                            "rubric_sha256": fingerprint(constants["PROMPT"]), "schema_sha256": fingerprint(constants["SCHEMA"]),
                            "response_truncated": len(response) > constants["MAX_RESPONSE_CHARS"], "recovered_completed_judge": True}
                        checkpoint_attempt(case_path, case)
                if judge.get("status") == "completed":
                    validate_cached_judge(judge, prompt, constants, judge_host)
                else:
                    if judge.get("status") in ("queued", "started"):
                        case["judge"] = judge = {**judge, "status": "unavailable", "error": "Prior judge attempt was interrupted"}
                        checkpoint_attempt(case_path, case)
                    if not judge or judge.get("status") == "budget_blocked" or args.retry_failed:
                        previous_judge = judge
                        case["judge"] = {"status": "queued"}
                        checkpoint_attempt(case_path, case)
                        def reserved(receipt):
                            case["judge"] = {"status": "started", "host_ordinal": receipt["ordinal"]}
                            checkpoint_attempt(case_path, case)
                        try:
                            verdict, completion_report = judge_host.complete(prompt, {}, constants["SCHEMA"], on_reserved=reserved)
                            ordinal = case["judge"]["host_ordinal"]
                            case["judge"] = {"status": "completed", **verdict, "model": judge_host.model,
                                             "reasoning_effort": judge_host.effort, "host_ordinal": ordinal,
                                             **service_tier_metadata(judge_host.service_tier, completion_report),
                                             "rubric_sha256": fingerprint(constants["PROMPT"]), "schema_sha256": fingerprint(constants["SCHEMA"]),
                                             "response_truncated": len(response) > constants["MAX_RESPONSE_CHARS"]}
                        except Exception as error:
                            if getattr(error, "code", None) == "call_budget":
                                case["judge"] = {**previous_judge, "retry_budget_blocked": True}
                                if not previous_judge or previous_judge.get("status") == "budget_blocked":
                                    case["judge"]["status"] = "budget_blocked"
                            else:
                                case["judge"] = {**case["judge"], "status": "unavailable", "error": str(error)}
                        checkpoint_attempt(case_path, case)
            return case

        try:
            readers = bounded_stage(rows, args.reader_concurrency, read_case, shared, run_dir, "readers")
            cases = bounded_stage(list(zip(rows, readers)), args.judge_concurrency, judge_case, shared, run_dir, "judges")
        except BaseException as error:
            # Pools have cancelled and drained owned work before returning here.
            # Preserve one materialized outcome for every originally selected row.
            partial = []
            for row in rows:
                path = run_dir / "cases" / f"{row['source_row']:03d}" / "case.json"
                partial.append(locks.read_json(path) if path.exists() else {
                    "source_row": row["source_row"], "doc_id": row["doc_id"], "status": "not_started"})
            checkpoint_attempt(run_dir / "interruption.json", {
                "schema_version": manifest["schema_version"], "status": "interrupted", "error_type": type(error).__name__,
                "source_rows": indices, "question_denominator": len(rows), "cases": partial,
                "host_invocations": len(shared.calls), "comparison_eligible": False,
                "stage_execution_history": stage_history(run_dir),
                "owned_process_concurrency": shared.concurrency_report(),
            })
            raise
        finally:
            shared.close(cancel=True)
        if locks.digest(binary) != manifest["binary_sha256"] or locks.digest(judge_binary) != manifest["judge_binary_sha256"]:
            raise ValueError("Executable changed during the evaluation")
        cumulative = cumulative_accounting(shared, run_dir)
        for case in cases:
            attempts = [attempt for attempt in cumulative["attempts"] if attempt["phase"] == f"answer:native:row-{case['source_row']}"]
            costs = [attempt["cost"] for attempt in attempts]
            known = [cost["known_cost_subtotal_usd"] for cost in costs if cost.get("known_cost_subtotal_usd") is not None]
            case["cumulative_jev_cost"] = {"accounting_complete": bool(costs) and all(cost["accounting_complete"] for cost in costs),
                                           "known_cost_subtotal_usd": math.fsum(known) if known else None,
                                           "native_ask_attempts": len(attempts),
                                           "unknown_accounting_attempts": sum(not cost["accounting_complete"] for cost in costs)}
            known = [attempt["elapsed_ms"] for attempt in attempts if isinstance(attempt["elapsed_ms"], (int, float))]
            missing = len(attempts) - len(known)
            case["cumulative_reader_wall_ms"] = math.fsum(known) if missing == 0 else None
            case["cumulative_reader_wall_ms_known_subtotal"] = math.fsum(known)
            case["cumulative_reader_wall_ms_missing"] = missing
        summary = summarize(cases, len(rows))
        summary.update(jev_cost_accounting_complete=cumulative["accounting_complete"],
                       measured_jev_cost_usd=cumulative["measured_jev_cost_usd"],
                       known_jev_cost_subtotal_usd=cumulative["known_jev_cost_subtotal_usd"],
                       cost_scope="all native ask attempts, including failed/interrupted retries",
                       native_model_turn_accounting=cumulative["native_model_turn_accounting"],
                       native_model_usage_scope=cumulative["native_model_accounting_scope"])
        observed_wall = [call["elapsed_ms"] for call in shared.calls
                         if type(call.get("elapsed_ms")) in (int, float) and math.isfinite(call["elapsed_ms"]) and call["elapsed_ms"] >= 0]
        missing_wall = len(shared.calls) - len(observed_wall)
        known_wall = math.fsum(observed_wall)
        report = {**manifest, "status": "completed" if summary["comparison_eligible"] else "incomplete",
                  "build_result": build, "summary": summary, "cases": cases, "host_invocations": len(shared.calls),
                  "completed_host_turns": sum(call["status"] == "completed" for call in shared.calls),
                  "cumulative_attempt_accounting": cumulative,
                  "all_host_wall_ms": known_wall if missing_wall == 0 else None,
                  "all_host_wall_ms_known_subtotal": known_wall, "all_host_wall_ms_missing": missing_wall,
                  "host_usage_missing": sum(call.get("usage") is None for call in shared.calls),
                  "stage_execution_history": stage_history(run_dir),
                  "owned_process_concurrency": shared.concurrency_report(),
                  "cohorts": {name: summarize([case for case in cases if case["source_row"] in members], len(set(indices) & members))
                              for name, members in groups.items()},
                  "timing_note": "Native ask latency includes Jev and Codex. Fresh-host SDK and persistent native-reader overhead remain distinct.",
                  "model_usage_note": "Host invocations are not provider-internal request counts; unknown usage/billing stays null."}
        if enrichment is not None:
            report["enrichment_result"] = enrichment_result
            report["reader_binding"] = enrichment_result["ready"]
            report["summary"]["enrichment_accounting"] = enrichment_result["accounting"]
            report["summary"]["enrichment_usage_scope"] = "Builder model turns and support Jev are counted once from their cumulative ledger, separately from native asks and judges."
            report["cost_projection"] = enrichment_cost_projection(args, cumulative, shared, enrichment_result, summary, manifest)
            report["summary"].update({name: report["cost_projection"][name] for name in (
                "standard_normalized_qa_usd_per_question", "effective_tier_qa_api_equivalent_usd_per_question", "g5_dual_gate_observation")})
            if locks.digest(price_path) != manifest["price_card"]["sha256"]:
                raise ValueError("enrichment_price_card_changed_during_evaluation")
            report["timing_note"] += " Deterministic index and enrichment preparation are separate retained stages; summed outer durations are not concurrent cohort wall time."
            for reference in manifest["external_reference_bindings"].values():
                if locks.digest(Path(reference["path"])) != reference["sha256"]:
                    raise ValueError("enrichment_external_reference_changed")
        if args.baseline_summary:
            report["paired_comparison"] = compare_reports(report, locks.read_json(args.baseline_summary.expanduser().resolve()))
        write_json(run_dir / "summary.json", report)
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", choices=["plan", "run"], default="plan")
    parser.add_argument("--rows", default="all")
    parser.add_argument("--upstream", type=Path, required=True)
    parser.add_argument("--benchmark", type=Path, required=True)
    parser.add_argument("--judge-source", type=Path, required=True)
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/gptgrep")
    parser.add_argument("--judge-binary", type=Path)
    parser.add_argument("--baseline-summary", type=Path, help="Read only after native evaluation; compare compatible complete outcomes")
    parser.add_argument("--codex-bin", default="codex")
    parser.add_argument("--codex-home", type=Path, required=True)
    parser.add_argument("--model")
    parser.add_argument("--reasoning-effort")
    profiles.add_arguments(parser)
    parser.add_argument("--jev-model", default="typesafe/jev-1.13")
    parser.add_argument("--max-model-calls", type=int, default=0)
    parser.add_argument("--max-input-bytes", type=int, default=262144)
    parser.add_argument("--max-tool-calls", type=int, default=12)
    parser.add_argument("--reader-concurrency", type=int, default=5)
    parser.add_argument("--judge-concurrency", type=int, default=5)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--build-timeout", type=int, default=300)
    parser.add_argument("--optimize-merge", action="store_true")
    parser.add_argument("--retry-failed", action="store_true", help="Retry failed/interrupted work; completed reader/judge outcomes are always reused")
    parser.add_argument("--experimental-query-plan", action="store_true",
                        help="Evaluate the explicit additional Luna planner and bounded multi-query initial retrieval; all nested model steps are accounted separately")
    parser.add_argument("--planner-model", help=f"Query-planner model, independent of reader and judge; defaults to {DEFAULT_PLANNER_MODEL} for new planned runs. Requires --experimental-query-plan.")
    parser.add_argument("--experimental-evidence-roles", action="store_true",
                        help="Add bounded Jev evidence-role judgments to the planned union request; requires --experimental-query-plan")
    parser.add_argument("--experimental-enrichment", action="store_true",
                        help="Strict v4 full-cohort raw builder + bound navigation; plan stage indexes and writes a zero-model full enrichment plan")
    parser.add_argument("--builder-model", help="Enrichment model; must match the selected planner and reader")
    parser.add_argument("--builder-reasoning-effort", choices=["high", "xhigh", "max"],
                        help="Enrichment effort; defaults to and must match the selected planner/reader effort")
    parser.add_argument("--max-builder-calls", type=int)
    parser.add_argument("--max-builder-jev-calls", type=int)
    parser.add_argument("--builder-timeout", type=int, default=180)
    parser.add_argument("--enrich-window-bytes", type=int, default=8192)
    parser.add_argument("--enrich-max-hints-per-window", type=int, default=4)
    parser.add_argument("--enrich-max-ledger-bytes", type=int, default=64 * 1024 * 1024)
    parser.add_argument("--enrich-max-windows-per-run", type=int, default=65536)
    parser.add_argument("--enrich-deadline-seconds", type=int, default=900)
    parser.add_argument("--expected-enrichment-plan-sha256")
    parser.add_argument("--resume-enrichment", action="store_true", help="Resume only a validated safe incomplete checkpoint under the same fixed ledger and caps")
    parser.add_argument("--navigation-max-jev-calls", type=int, default=32)
    parser.add_argument("--navigation-max-request-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--original-pageindex-results", type=Path, help="Hash-bind the original aggregate results separately from the adapted --baseline-summary trace")
    parser.add_argument("--original-pageindex-documents", type=Path, help="Original documents.json numeric index-cost metadata; defaults to BENCHMARK/documents.json when present")
    parser.add_argument("--price-card", type=Path, help="Required v4 versioned API price-card JSON; exact SHA is immutable in the run manifest")
    args = parser.parse_args()
    if min(args.timeout, args.build_timeout, args.max_tool_calls, args.max_input_bytes) <= 0 or args.max_model_calls < 0:
        parser.error("Invalid execution bounds")
    try:
        report = execute(args)
    except Exception as error:
        report = {"schema_version": "gptgrep.system-eval.v3", "status": "failed", "error": str(error)}
    print(json.dumps({key: report.get(key) for key in (
        "schema_version", "status", "variant", "question_count", "document_count", "host_invocations", "summary", "error"
    )}, indent=2))
    return 0 if report["status"] in ("completed", "plan_prepared") else 1


if __name__ == "__main__":
    raise SystemExit(main())
