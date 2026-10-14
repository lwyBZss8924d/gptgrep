"""Sandbox entrypoint: one query, one original deadline, no gold input."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
from .product_bundle import PRODUCT_PROVENANCE, PRODUCT_ROOT, check_product_bundle
import time

from .common import MAX_ANSWER, MAX_READER_QUESTION_BYTES, PROFILE, canonical, copy_asset, finite_number, integer, parse, pinned, put, read, require, sha, validate_binding
from .native import codex_accounting, codex_argv, codex_result, prompt_bytes
from .process import run
from .runtime import CODEX_VERSION, DECISIONS_BUDGET, DECISIONS_LEDGER_DIR, check_config, native_environment
from .decisions import (TRIAL_BUDGET, acquire_writer, claim_invocation, prepare as prepare_decisions,
                        record_invocation, selected_mode)


# A completed legacy report is a response. An insufficient_evidence report with
# a non-empty answer is a completed intentional abstention, judged under the
# frozen answerability rubric. Every other status is an operational failure.
LEGACY_ANSWER_STATUSES = ("completed", "insufficient_evidence")


def reader_argv(config, binding, task):
    require(len(task.encode()) <= min(MAX_READER_QUESTION_BYTES, config["max_input_bytes"]),
            "instruction_bytes_limit")
    if binding["arm"] == "gptgrep":
        require(config.get("native_ptc") is True and config.get("codex_permissions") == "full62_reader",
                "shared_native_reader_required")
        require(config["reader_mode"] in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
        mode_flags = ["--multimodal"] if config["reader_mode"] == "rich_multimodal" else []
        decisions = selected_mode(config)
        require((decisions == "preparation") == (config.get("decisions_preparation") is not None),
                "decisions_mode")
        if decisions == "preparation":
            require(config["reader_mode"] == "rich_multimodal"
                    and config["decisions_preparation"]["max_concurrent_batches"] == 1,
                    "decisions_preparation_mode")
            mode_flags += ["--max-concurrent-batches", "1"]
            budget = ["--decisions-budget", TRIAL_BUDGET]
        elif decisions == "existing_context":
            budget = ["--decisions-budget", DECISIONS_BUDGET]
        else:
            # invocation_bounded: no shared ledger or budget file. The Ask keeps
            # the product's own single-invocation context (at most 200 submissions).
            require(config["reader_mode"] == "legacy_text", "decisions_invocation_bounded_mode")
            budget = []
        return [config["gptgrep_binary"], *budget,
                "ask", "--native-ptc", *mode_flags, "--document=" + config["source_relative_path"],
                "--jev-model", config["jev_model"], "--codex-bin", "/opt/full62/codex-native",
                "--codex-home", config["codex_home"], "--codex-permissions", config["codex_permissions"],
                "--model", PROFILE["model"],
                "--reasoning-effort", "max", "--service-tier", "fast",
                "--timeout", str(config["deadline_seconds"]),
                "--max-tool-calls", str(config["max_tool_calls"]),
                "--max-input-bytes", str(config["max_input_bytes"]),
                "--json", "--", task, "/data/corpus"]
    require(binding["arm"] == "raw_only", "arm")
    return codex_argv(config["codex_binary"])


def task_prompt(config, question):
    # Both front doors receive these exact task bytes. Native default base
    # instructions remain the bare arm's; GPTgrep's reader contract is treatment.
    require(config.get("reader_mode") in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
    return prompt_bytes("Document: " + config["raw_document"] + "\nQuestion: " + question)


def prompt_observation(config, binding, question, task_input):
    """Retain the exact adapter input, without claiming an observed model prompt."""
    bare = binding["arm"] == "raw_only"
    data = task_input
    path = "/logs/agent/frontdoor-input.txt"
    put(path, data)
    return {"common_question": {"bytes": len(question.encode("utf-8")),
                                 "sha256": sha(question.encode("utf-8"))},
            "frontdoor_input": {"path": path, "bytes": len(data), "sha256": sha(data),
                                "transport": "native_stdin" if bare else "gptgrep_question_argument"},
            "scope": "exact_adapter_input_not_effective_native_model_prompt",
            "base_instructions": {
                "selection": "native_default" if bare else "gptgrep_reader_contract",
                "adapter_override": False,
                "expected_full_sha256": None, "effective_sha256": None,
                "effective_bytes_observed": False,
                "availability_binding": {"reader_binary": config["reader_base_binary"],
                                         "native_config_sha256": config["native_config_sha256"],
                                         "product_provenance": (config.get("gptgrep_bundle", {}).get("provenance")
                                                                if not bare else None)},
                "limit": "Binary/configuration pins do not reveal effective base-instruction bytes."},
            "treatment_contract_applied_to_bare": False}


def product_accounting(report):
    # Preserve the product's payload-free model_attempts.native_ptc observations,
    # including failures; native counts are not issued GPTgrep tools or citations.
    rich = report
    if isinstance(report.get("multimodal_host"), dict):
        rich = report["multimodal_host"]
    retrieval = rich.get("initial_retrieval")
    if not isinstance(retrieval, dict) and isinstance(report.get("multimodal_retrieval"), dict):
        retrieval = report["multimodal_retrieval"].get("report")
    if isinstance(retrieval, dict):
        costs = [item["usage"]["cost"] for item in retrieval.get("attempts", [])
                 if isinstance(item, dict) and isinstance(item.get("usage"), dict)
                 and finite_number(item["usage"].get("cost"))]
        return {"usage": rich.get("model_usage"), "model_attempts": rich.get("model_attempts"),
                "initial_retrieval": retrieval,
                "reported_retrieval_cost_subtotal": sum(costs) if costs else None,
                "reported_retrieval_cost_receipts": len(costs),
                "reported_retrieval_cost_currency_verified": False,
                "retrieval_accounting_scope": "retained_initial_retrieval_report",
                "cost_usd": None, "cost_complete": False, "physical_provider_requests": None,
                "usage_scope": "model_usage_summarizes_model_attempts_not_additive"}
    jev = report.get("jev")
    if not isinstance(jev, dict):
        for key in ("host_retrieval", "retrieval", "error"):
            value = report.get(key)
            if isinstance(value, dict) and isinstance(value.get("jev"), dict):
                jev = value["jev"]
                break
    costs = []
    if isinstance(jev, dict) and isinstance(jev.get("usage"), list):
        costs = [row["cost"] for row in jev["usage"]
                 if isinstance(row, dict) and finite_number(row.get("cost"))]
    return {"usage": report.get("usage"), "model_attempts": report.get("model_attempts"),
            "jev": jev if isinstance(jev, dict) else None,
            "reported_retrieval_cost_subtotal": sum(costs) if costs else None,
            "reported_retrieval_cost_receipts": len(costs),
            "reported_retrieval_cost_currency_verified": False,
            "decisions_cost": decisions_cost(jev),
            "cost_usd": None, "cost_complete": False,
            "physical_provider_requests": None,
            "usage_scope": "final_reader_summary_not_additive_to_model_attempts"}


def decisions_cost(jev):
    """One trial's reported Decisions cost receipt, retained even for failed Asks.

    The cost is known only when every attempted call returned a validated
    response with a finite reported cost; otherwise it stays unknown (null).
    """
    counts = {key: jev.get(key) if isinstance(jev, dict) else None
              for key in ("requests", "attempted_calls", "unobserved_attempts")}
    usage = jev.get("usage") if isinstance(jev, dict) and isinstance(jev.get("usage"), list) else []
    costs = [row["cost"] for row in usage if isinstance(row, dict) and finite_number(row.get("cost"))]
    accounted = isinstance(jev, dict) and jev.get("accounting_complete") is True
    complete = (accounted and all(type(value) is int and value >= 0 for value in counts.values())
                and counts["unobserved_attempts"] == 0
                and counts["attempted_calls"] == counts["requests"] == len(usage) == len(costs))
    return {"schema": "gptgrep.harbor.decisions-cost.v1", "observed": isinstance(jev, dict),
            "validated_requests": counts["requests"], "attempted_calls": counts["attempted_calls"],
            "unobserved_attempts": counts["unobserved_attempts"], "usage_rows": len(usage),
            "priced_rows": len(costs), "accounting_complete": accounted,
            "reported_cost_usd": sum(costs) if complete else None, "cost_complete": complete,
            "currency_verified": False}


def product_result(data, reader_mode):
    report = parse(data)
    require(reader_mode in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
    if reader_mode == "rich_multimodal":
        require(report.get("schema_version") == "gptgrep.rich_multimodal.host.v1", "rich_product_schema")
        attempts = report.get("model_attempts")
        require(isinstance(attempts, list) and attempts and
                all(isinstance(a, dict) and a.get("requested_model") == PROFILE["model"] and
                    a.get("requested_reasoning_effort") == PROFILE["reasoning_effort"] and
                    a.get("requested_service_tier") in ("fast", "priority") for a in attempts),
                "product_requested_profile")
        require(type(report.get("insufficient_evidence")) is bool and
                isinstance(report.get("citations"), list) and
                isinstance(report.get("initial_retrieval"), dict), "rich_product_fields")
    else:
        require(report.get("schema_version") != "gptgrep.rich_multimodal.host.v1" and
                report.get("status") in LEGACY_ANSWER_STATUSES, "product_not_completed")
    # An empty abstention is not a completed answer: it stays an operational failure.
    answer = report.get("answer")
    require(isinstance(answer, str) and answer.strip() and
            len(answer.encode()) <= MAX_ANSWER, "product_answer")
    if reader_mode == "rich_multimodal":
        return answer, {**product_accounting(report), "reader_mode": reader_mode,
                        "reported_profiles": [{"model": a.get("model"),
                           "reasoning_effort": a.get("effective_reasoning_effort"),
                           "service_tier": a.get("effective_service_tier")} for a in attempts],
                        "requested_tier_normalized": "priority", "served_profile_verified": False,
                        "citations": report["citations"], "citation_count": len(report["citations"]),
                        "insufficient_evidence": report["insufficient_evidence"],
                        "citation_observation": report.get("citation_observation"),
                        "native_qualification": report.get("qualification"),
                        "tool_calls": report.get("tool_calls"), "semantic_grounding": "unassessed"}
    require(report.get("model") == PROFILE["model"] and
            report.get("requested_reasoning_effort") == PROFILE["reasoning_effort"] and
            report.get("requested_service_tier") in ("fast", "priority"), "product_requested_profile")
    # Preserve complete bounded native attempt records separately from the final
    # reader summary. The latter is not added a second time by aggregation.
    attempts = report.get("model_attempts")
    require(attempts is None or isinstance(attempts, list), "attempts_shape")
    return answer, {**product_accounting(report),
                    "product_status": report["status"],
                    "insufficient_evidence": report["status"] == "insufficient_evidence",
                    "retrieval": report.get("retrieval"),
                    "cost_usd": None, "cost_complete": False,
                    "physical_provider_requests": None,
                    "reported_profile": {"model": report.get("model"),
                       "reasoning_effort": report.get("effective_reasoning_effort"),
                       "service_tier": report.get("effective_service_tier")},
                    "reported_tier_normalized": ("priority" if report.get("effective_service_tier") in
                                                  ("fast", "priority") else report.get("effective_service_tier")),
                    "requested_tier_normalized": "priority",
                    "served_profile_verified": False,
                    "citation_count": len(report.get("citations", [])),
                    "semantic_grounding": "unassessed"}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--instruction", required=True)
    args = parser.parse_args()
    config = parse(read("/opt/full62/reader.json"))
    binding = validate_binding(config["binding"])
    # These question-independent checks establish the receipt identity and its
    # finite budget. Question reading through complete answer-envelope
    # construction/hashing belongs to it; immutable terminal observer writes do not.
    timeout = integer(config["deadline_seconds"], 1, 86400)
    question_limit = min(MAX_READER_QUESTION_BYTES, integer(config["max_input_bytes"], 1))
    require(not any(Path(path).exists() for path in
                    ("/answer/answer.json", "/logs/agent/operational.json", "/logs/agent/attempt-start.json")),
            "reader_outputs_already_exist")
    started = time.monotonic()
    deadline_unix_ms = time.time_ns() // 1000000 + timeout * 1000
    outcome = {"schema": "gptgrep.harbor.answer.v1", "binding": binding,
               "status": "operational_failure", "answer": None}
    operation = {"schema": "gptgrep.harbor.operation.v1", "binding": binding,
                 "requested_profile": PROFILE, "attempts": [], "deadline_ms": timeout * 1000,
                 "original_deadline_unix_ms": deadline_unix_ms,
                 "clock_started_before_question_read": True,
                 "reader_base_binary": config["reader_base_binary"],
                 "native_cli": config["native_cli"], "native_config_sha256": config["native_config_sha256"],
                 "base_tool_environment": config["base_tool_environment"],
                 "reader_mode": config["reader_mode"] if binding["arm"] == "gptgrep" else "raw_only",
                 "effective_permissions_verified": False,
                 "permitted_tools": config["permitted_tools"],
                 "generic_capabilities": config["generic_capabilities"],
                 "qualification_mismatches": config["qualification_mismatches"],
                 "budget_scope": config["budget_scope"],
                 "outer_retry_count": 0, "provider_internal_retry_count": None,
                 "index_excluded": True, "grounding": "unassessed"}
    put("/logs/agent/attempt-start.json", {"binding": binding, "profile": PROFILE,
                                          "original_deadline_unix_ms": deadline_unix_ms,
                                          "clock_started_before_question_read": True})

    def remaining():
        seconds = timeout - (time.monotonic() - started)
        require(seconds > 0, "reader_deadline")
        return seconds

    writer = None
    stage = "question_validation"
    try:
        question_data = read(args.instruction, question_limit)
        question = question_data.decode("utf-8")
        require(sha(question_data) == binding["question_sha256"], "question_binding")
        remaining()
        stage = "common_config_validation"
        require(config["profile"] == PROFILE, "profile")
        check_config(config, "reader")
        remaining()
        stage = "common_binary_validation"
        require(config["reader_base_binary"]["path"] == config["codex_binary"], "reader_binary_path")
        copy_asset(config["reader_base_binary"], 2**30)
        remaining()
        stage = "arm_bundle_validation"
        if binding["arm"] == "raw_only":
            require(not any(Path(path).exists() for path in
                            (config["gptgrep_binary"], PRODUCT_ROOT, PRODUCT_PROVENANCE)),
                    "bare_image_has_treatment_binary")
        else:
            check_product_bundle(config)
        remaining()
        stage = "native_version_validation"
        env = native_environment(os.environ, product=binding["arm"] == "gptgrep")
        version = run([config["codex_binary"], "--version"], cwd="/data/corpus",
                      timeout=min(5, remaining()), env=native_environment(os.environ))
        require(version["return_code"] == 0 and version["failure"] is None and
                version["stdout"].strip() == ("codex-cli " + CODEX_VERSION).encode(), "native_version_mismatch")
        operation["observed_native_version"] = version["stdout"].decode().strip()
        decisions = None
        if binding["arm"] == "gptgrep":
            decisions = operation["decisions_mode"] = selected_mode(config)
        stage = "query_preparation"
        task_input = task_prompt(config, question)
        argv = reader_argv(config, binding, task_input.decode("utf-8"))
        native_input = task_input if binding["arm"] == "raw_only" else None
        operation["prompt_observation"] = prompt_observation(config, binding, question, task_input)
        if decisions == "preparation":
            writer = acquire_writer()
            env, operation["decisions_preparation"] = prepare_decisions(
                config, binding, question, task_input, argv, started=started,
                deadline_unix_ms=deadline_unix_ms, environ=os.environ)
        elif decisions == "existing_context":
            budget = parse(pinned(config["decisions_budget"], 16384))
            require(Path(budget["ledger_path"]).parent == Path(DECISIONS_LEDGER_DIR),
                    "decisions_ledger_mount_contract")
        # invocation_bounded joins no ledger: the Ask creates its own context and
        # its reported Decisions cost receipt is kept per trial below.
        if decisions == "invocation_bounded":
            # Create-new, immediately before the one Ask: a re-executed trial
            # stops here as an operational failure with no Decisions spend.
            operation["decisions_claim"] = claim_invocation(binding, deadline_unix_ms)
        stage = "reader_process"
        result = run(argv, cwd="/data/corpus", timeout=remaining(), env=env, input_bytes=native_input)
        attempt = {"ordinal": 0, "elapsed_ms": result["elapsed_ms"],
                   "return_code": result["return_code"], "failure": result["failure"],
                   "stdout_sha256": sha(result["stdout"]), "stdout_bytes": len(result["stdout"]),
                   "stderr_sha256": sha(result["stderr"]), "stderr_bytes": len(result["stderr"]),
                   "stdin_bytes": result["stdin_bytes"], "stdin_bytes_sent": result["stdin_bytes_sent"],
                   "stdin_complete": result["stdin_complete"],
                   "native_process_incomplete": (result["failure"] is not None or result["return_code"] != 0
                                                  or not result["stdin_complete"]),
                   "cost_complete": False, "cost_usd": None, "usage": None}
        operation["attempts"].append(attempt)
        if binding["arm"] == "gptgrep":
            # Preserve complete typed output, including visual/text citation and
            # generation identities, outside the primary verifier transfer.
            put("/logs/agent/product-report.json", result["stdout"])
            operation["product_report"] = {"path": "/logs/agent/product-report.json",
                "bytes": len(result["stdout"]), "sha256": sha(result["stdout"])}
        # Recover observable accounting even when the front door failed. Any
        # missing final report remains unknown; no unlisted ledger is opened.
        try:
            if binding["arm"] == "gptgrep":
                attempt.update(product_accounting(parse(result["stdout"])))
            else:
                attempt.update(codex_accounting(result["stdout"]))
        except (ValueError, TypeError, KeyError, AttributeError):
            attempt["accounting_projection_complete"] = False
        if decisions == "invocation_bounded":
            # Host-side copy beside the claim, outside the Harbor job directory.
            # Evidence only: a failed write never changes the Ask's outcome.
            try:
                operation["decisions_receipt"] = record_invocation(binding, result["stdout"], attempt)
            except (OSError, ValueError, TypeError):
                operation["decisions_receipt"] = None
        require(result["failure"] is None and result["return_code"] == 0, "reader_process")
        stage = "answer_validation"
        answer, metadata = (product_result(result["stdout"], config["reader_mode"]) if binding["arm"] == "gptgrep"
                            else codex_result(result["stdout"], allow_missing_usage=True))
        attempt.update(metadata)
        if binding["arm"] == "gptgrep":
            # A completed intentional abstention crosses as an ordinary response
            # for judging; the flag stays in the private operation record.
            operation["insufficient_evidence"] = metadata["insufficient_evidence"]
        outcome.update(status="response", answer=answer)
        operation["answer_sha256"] = sha(canonical(outcome))
        remaining()
    except Exception:
        # Redacted code only: exception text may contain task/provider/private data.
        outcome.update(status="operational_failure", answer=None)
        operation["answer_sha256"] = sha(canonical(outcome))
        operation["failure"] = "reader_failed"
        operation["failure_stage"] = stage
    finally:
        if writer is not None:
            os.close(writer)
    operation["status"] = outcome["status"]
    operation["cumulative_query_wall_ms"] = round((time.monotonic() - started) * 1000)
    operation["query_wall_scope"] = "question_read_through_answer_envelope_hash; terminal_writes_excluded"
    # Only this bounded envelope crosses to the separate primary verifier.
    persistence_started = time.monotonic()
    put("/answer/answer.json", outcome)
    operation["answer_persistence_wall_ms"] = round((time.monotonic() - persistence_started) * 1000)
    put("/logs/agent/operational.json", operation)


if __name__ == "__main__":
    main()
