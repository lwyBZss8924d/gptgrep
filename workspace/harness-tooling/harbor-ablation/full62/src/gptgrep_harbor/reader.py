"""Sandbox entrypoint: one query, one original deadline, no gold input."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
from .product_bundle import PRODUCT_PROVENANCE, PRODUCT_ROOT, check_product_bundle
import time

from .common import MAX_ANSWER, MAX_READER_QUESTION_BYTES, PROFILE, canonical, copy_asset, finite_number, parse, pinned, put, read, require, sha, validate_binding
from .native import codex_accounting, codex_argv, codex_result, prompt_bytes
from .process import run
from .runtime import CODEX_VERSION, DECISIONS_BUDGET, DECISIONS_LEDGER_DIR, check_config, native_environment


def reader_argv(config, binding, question):
    require(len(question.encode()) <= min(MAX_READER_QUESTION_BYTES, config["max_input_bytes"]),
            "instruction_bytes_limit")
    if binding["arm"] == "gptgrep":
        require(config.get("native_ptc") is True and config.get("codex_permissions") == "full62_reader",
                "shared_native_reader_required")
        require(config["reader_mode"] in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
        mode_flags = ["--multimodal"] if config["reader_mode"] == "rich_multimodal" else []
        return [config["gptgrep_binary"], "--decisions-budget", DECISIONS_BUDGET,
                "ask", "--native-ptc", *mode_flags, "--document=" + config["source_relative_path"],
                "--jev-model", config["jev_model"], "--codex-bin", "/opt/full62/codex-native",
                "--codex-home", config["codex_home"], "--codex-permissions", config["codex_permissions"],
                "--model", PROFILE["model"],
                "--reasoning-effort", "max", "--service-tier", "fast",
                "--timeout", str(config["deadline_seconds"]),
                "--max-tool-calls", str(config["max_tool_calls"]),
                "--max-input-bytes", str(config["max_input_bytes"]),
                "--json", "--", question, "/data/corpus"]
    require(binding["arm"] == "raw_only", "arm")
    return codex_argv(config["codex_binary"])


def raw_prompt(config, question):
    return prompt_bytes("Answer the question using only the protocol-scoped raw document at "
              + config["raw_document"] + ". You may inspect that document with native tools. "
              "Do not use external search. Give the complete answer and source locations "
              "when available. Question:\n" + question)


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
            "cost_usd": None, "cost_complete": False,
            "physical_provider_requests": None,
            "usage_scope": "final_reader_summary_not_additive_to_model_attempts"}


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
                report.get("status") == "completed", "product_not_completed")
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
    question = read(args.instruction, min(MAX_READER_QUESTION_BYTES, config["max_input_bytes"])).decode("utf-8")
    require(sha(question.encode()) == binding["question_sha256"], "question_binding")
    require(config["profile"] == PROFILE, "profile")
    check_config(config, "reader")
    require(config["reader_base_binary"]["path"] == config["codex_binary"], "reader_binary_path")
    copy_asset(config["reader_base_binary"], 2**30)
    if binding["arm"] == "raw_only":
        require(not any(Path(path).exists() for path in
                        (config["gptgrep_binary"], PRODUCT_ROOT, PRODUCT_PROVENANCE)),
                "bare_image_has_treatment_binary")
    else:
        check_product_bundle(config)
    require(not Path("/answer/answer.json").exists() and not Path("/logs/agent/operational.json").exists(),
            "reader_outputs_already_exist")
    put("/logs/agent/attempt-start.json", {"binding": binding, "profile": PROFILE})
    started = time.monotonic()
    outcome = {"schema": "gptgrep.harbor.answer.v1", "binding": binding,
               "status": "operational_failure", "answer": None}
    operation = {"schema": "gptgrep.harbor.operation.v1", "binding": binding,
                 "requested_profile": PROFILE, "attempts": [], "deadline_ms": config["deadline_seconds"] * 1000,
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
    try:
        env = native_environment(os.environ, product=binding["arm"] == "gptgrep")
        version = run([config["codex_binary"], "--version"], cwd="/data/corpus",
                      timeout=min(5, config["deadline_seconds"]), env=native_environment(os.environ))
        require(version["return_code"] == 0 and version["failure"] is None and
                version["stdout"].strip() == ("codex-cli " + CODEX_VERSION).encode(), "native_version_mismatch")
        operation["observed_native_version"] = version["stdout"].decode().strip()
        if binding["arm"] == "gptgrep":
            budget = parse(pinned(config["decisions_budget"], 16384))
            require(Path(budget["ledger_path"]).parent == Path(DECISIONS_LEDGER_DIR),
                    "decisions_ledger_mount_contract")
        argv = reader_argv(config, binding, question)
        native_input = raw_prompt(config, question) if binding["arm"] == "raw_only" else None
        remaining = config["deadline_seconds"] - (time.monotonic() - started)
        require(remaining > 0, "reader_deadline")
        result = run(argv, cwd="/data/corpus", timeout=remaining, env=env, input_bytes=native_input)
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
        require(result["failure"] is None and result["return_code"] == 0, "reader_process")
        answer, metadata = (product_result(result["stdout"], config["reader_mode"]) if binding["arm"] == "gptgrep"
                            else codex_result(result["stdout"]))
        attempt.update(metadata)
        outcome.update(status="response", answer=answer)
    except Exception:
        # Redacted code only: exception text may contain task/provider/private data.
        operation["failure"] = "reader_failed"
    operation["cumulative_query_wall_ms"] = round((time.monotonic() - started) * 1000)
    operation["status"] = outcome["status"]
    operation["answer_sha256"] = sha(canonical(outcome))
    put("/logs/agent/operational.json", operation)
    # Only this bounded envelope crosses to the separate primary verifier.
    put("/answer/answer.json", outcome)


if __name__ == "__main__":
    main()
