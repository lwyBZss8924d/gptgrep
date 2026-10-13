"""Sandbox entrypoint: one query, one original deadline, no gold input."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
from .product_bundle import PRODUCT_PROVENANCE, PRODUCT_ROOT, check_product_bundle
import time

from .common import (MAX_ANSWER, MAX_READER_QUESTION_BYTES, PROFILE, canonical, copy_asset, field_at, finite_number,
                     parse, pinned, put, read, require, sha, validate_binding)
from .native import codex_accounting, codex_argv, codex_result, prompt_bytes
from .process import run
from .runtime import CODEX_VERSION, DECISIONS_BUDGET, DECISIONS_LEDGER_DIR, check_config, native_environment
from .decisions import (TRIAL_BUDGET, acquire_writer, claim_invocation, init_trial_budget,
                        prepare as prepare_decisions, product_limits, record_invocation, selected_mode,
                        trial_budget_status, trial_paths, trial_submission_limit)


# A completed legacy report is a response. An insufficient_evidence report with
# a non-empty answer is a completed intentional abstention, judged under the
# frozen answerability rubric. Every other status is an operational failure.
LEGACY_ANSWER_STATUSES = ("completed", "insufficient_evidence")


def treatment_command(config):
    """The product arguments for the selected reader and Decisions modes, before the per-task options.

    Every treatment reader launches GPTgrep with exactly these arguments, and
    the frozen treatment feature profile declares them.
    """
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
        # invocation_bounded: no shared ledger. The Ask joins this trial's own
        # product ledger, created right after its claim with the per-trial
        # submission limit, so the product enforces that limit.
        require(config["reader_mode"] == "legacy_text", "decisions_invocation_bounded_mode")
        trial_submission_limit(config.get("decisions_max_submissions_per_trial"))
        budget = ["--decisions-budget", str(trial_paths()[2])]
    return [*budget, "ask", "--native-ptc", *mode_flags]


def reader_argv(config, binding, question):
    require(len(question.encode()) <= min(MAX_READER_QUESTION_BYTES, config["max_input_bytes"]),
            "instruction_bytes_limit")
    if binding["arm"] == "gptgrep":
        require(config.get("native_ptc") is True and config.get("codex_permissions") == "full62_reader",
                "shared_native_reader_required")
        return [config["gptgrep_binary"], *treatment_command(config),
                "--document=" + config["source_relative_path"],
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


# GPTgrep's reader contract (its base instructions, answer/citation/
# insufficient_evidence schema, abstention rule and, in legacy_text, its generic
# answer rules) is part of the treatment under test (owner direction,
# 2026-10-13). The raw-only arm receives only this common task prompt with
# Codex's default base instructions, in every reader mode, and no GPTgrep rule text.
def raw_prompt(config, question):
    # States the enforced reader policy: web search and model-tool network are disabled.
    return prompt_bytes("Answer the question using only the protocol-scoped raw document at "
              + config["raw_document"] + ". You may inspect that document with native tools. "
              "No external search or network access is available. Give the complete answer and source locations "
              "when available. Question:\n" + question)


READER_CONTRACT_SCHEMA = "gptgrep.harbor.reader-contract.v1"
# The treatment's identity is the digest of its verified bundle provenance,
# which pins the product binary and so the contract text compiled into it: a
# product change to the rules changes it with no adapter edit. The pinned
# product's offline `--schema` report also describes the answer rules its
# legacy_text reader receives. That field is the product's own prose
# description, written separately from the rule text, so its digest identifies
# only the description: it is recorded beside the identity, never as it.
TREATMENT_IDENTITY = "product_provenance_sha256"
ANSWER_RULES_DESCRIPTION_FIELD = ("commands", "ask", "reader_answer_rules")
# A local report: Harbor's agent timeout leaves only ten seconds beyond the
# query deadline for everything outside it.
PRODUCT_SCHEMA_SECONDS = 5
# Adapter-defined failure codes that an operation record may carry verbatim.
# Every other exception stays the redacted generic failure.
RECORDED_FAILURE_CODES = ("product_reader_contract_unavailable", "decisions_max_submissions_per_trial",
                          "decisions_trial_budget_unavailable", "decisions_submission_limit_reached",
                          "decisions_product_limits_mismatch", "decisions_claim_mount", "trial_already_claimed")


def product_schema(config):
    """Run the pinned product's offline `--schema`; return its report bytes and parsed contract.

    Called only after the installed bundle matches its pins. No model, network or
    credential is involved, and the call precedes the query timer.
    """
    result = run([config["gptgrep_binary"], "--schema"], cwd="/data/corpus",
                 timeout=PRODUCT_SCHEMA_SECONDS, env=native_environment(os.environ))
    require(result["failure"] is None and result["return_code"] == 0, "product_reader_contract_unavailable")
    contract = parse(result["stdout"])
    require(type(contract) is dict, "product_reader_contract_unavailable")
    return result["stdout"], contract


def reader_contract(config, binding, schema=None):
    """This arm's reader-contract record for its operation record.

    The raw-only arm records that it received none of GPTgrep's contract. The
    treatment records its contract and its identity, the verified bundle
    provenance digest. In legacy_text it also records the digest of the pinned
    product's own description of its answer rules, which is not the rule text;
    rich_multimodal readers receive no answer rules, so that digest stays null.
    """
    if binding["arm"] == "raw_only":
        return {"schema": READER_CONTRACT_SCHEMA, "arm": "raw_only",
                "base_instructions": "codex_default", "prompt": "common_task_prompt",
                "gptgrep_contract_received": False, "answer_rules_received": False,
                "answer_rules_description_sha256": None}
    require(binding["arm"] == "gptgrep", "arm")
    mode = config["reader_mode"]
    require(mode in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
    record = {"schema": READER_CONTRACT_SCHEMA, "arm": "gptgrep",
              "base_instructions": "gptgrep_reader_contract", "reader_mode": mode,
              "answer_rules_received": mode == "legacy_text", "answer_rules_description_sha256": None,
              "identity": TREATMENT_IDENTITY,
              TREATMENT_IDENTITY: config["gptgrep_bundle"]["provenance"]["sha256"]}
    if mode == "legacy_text":
        require(schema is not None, "product_reader_contract_unavailable")
        data, contract = schema
        description = field_at(contract, ANSWER_RULES_DESCRIPTION_FIELD)
        require(isinstance(description, str) and description.strip(), "product_reader_contract_unavailable")
        record.update(answer_rules_description_source="product_schema:" + ".".join(ANSWER_RULES_DESCRIPTION_FIELD),
                      answer_rules_description_sha256=sha(description.encode()), product_schema_sha256=sha(data))
    return record


def observe_reader_contract(config, binding):
    """(record, schema report, None), or (None, None, failure code): a treatment that
    cannot report its contract stops before its Ask."""
    try:
        schema = None
        if binding["arm"] == "gptgrep" and config["reader_mode"] == "legacy_text":
            schema = product_schema(config)
        return reader_contract(config, binding, schema), schema, None
    except Exception:
        # Redacted code only: product output is never copied into the record.
        return None, None, "product_reader_contract_unavailable"


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
    # Before the query timer: the treatment's identity comes from the verified
    # product's own report; a failure is recorded and stops the trial below.
    contract, schema, contract_failure = observe_reader_contract(config, binding)
    claimed = config.get("decisions_mode") == "invocation_bounded"
    limits = None
    if claimed and binding["arm"] == "gptgrep" and contract_failure is None:
        # The admitted cost bound uses mirrored product limits: the pinned
        # product's own report must confirm them before the claim.
        limits = product_limits(schema[1]) if schema is not None else None
        if limits is None or not limits["match"]:
            contract_failure = "decisions_product_limits_mismatch"
    put("/logs/agent/attempt-start.json", {"binding": binding, "profile": PROFILE})
    started = time.monotonic()
    deadline_unix_ms = time.time_ns() // 1000000 + config["deadline_seconds"] * 1000
    outcome = {"schema": "gptgrep.harbor.answer.v1", "binding": binding,
               "status": "operational_failure", "answer": None}
    operation = {"schema": "gptgrep.harbor.operation.v1", "binding": binding,
                 "requested_profile": PROFILE, "attempts": [], "deadline_ms": config["deadline_seconds"] * 1000,
                 "reader_base_binary": config["reader_base_binary"],
                 "native_cli": config["native_cli"], "native_config_sha256": config["native_config_sha256"],
                 "base_tool_environment": config["base_tool_environment"],
                 "reader_mode": config["reader_mode"] if binding["arm"] == "gptgrep" else "raw_only",
                 "reader_contract": contract,
                 "effective_permissions_verified": False,
                 "permitted_tools": config["permitted_tools"],
                 "generic_capabilities": config["generic_capabilities"],
                 "qualification_mismatches": config["qualification_mismatches"],
                 "declared_treatment_differences": config.get("declared_treatment_differences", []),
                 "budget_scope": config["budget_scope"],
                 "outer_retry_count": 0, "provider_internal_retry_count": None,
                 "index_excluded": True, "grounding": "unassessed"}
    if limits is not None:
        operation["decisions_product_limits"] = limits
    writer = None
    try:
        if contract_failure is not None:
            raise ValueError(contract_failure)
        env = native_environment(os.environ, product=binding["arm"] == "gptgrep")
        version = run([config["codex_binary"], "--version"], cwd="/data/corpus",
                      timeout=min(5, config["deadline_seconds"]), env=native_environment(os.environ))
        require(version["return_code"] == 0 and version["failure"] is None and
                version["stdout"].strip() == ("codex-cli " + CODEX_VERSION).encode(), "native_version_mismatch")
        operation["observed_native_version"] = version["stdout"].decode().strip()
        decisions = None
        if binding["arm"] == "gptgrep":
            decisions = operation["decisions_mode"] = selected_mode(config)
        argv = reader_argv(config, binding, question)
        if decisions == "preparation":
            writer = acquire_writer()
            env, operation["decisions_preparation"] = prepare_decisions(
                config, binding, question, argv, started=started,
                deadline_unix_ms=deadline_unix_ms, environ=os.environ)
        elif decisions == "existing_context":
            budget = parse(pinned(config["decisions_budget"], 16384))
            require(Path(budget["ledger_path"]).parent == Path(DECISIONS_LEDGER_DIR),
                    "decisions_ledger_mount_contract")
        # invocation_bounded joins no shared ledger: the trial's own ledger is
        # created after its claim, and its cost receipt is kept per trial below.
        native_input = raw_prompt(config, question) if binding["arm"] == "raw_only" else None
        remaining = config["deadline_seconds"] - (time.monotonic() - started)
        require(remaining > 0, "reader_deadline")
        if claimed:
            # Create-new, immediately before this trial's one reader process,
            # in both arms: a re-executed trial stops here as an operational
            # failure (trial_already_claimed), the treatment with no Decisions spend.
            claim = operation["trial_claim"] = claim_invocation(binding, deadline_unix_ms)
        if decisions == "invocation_bounded":
            # The product creates this trial's own ledger with the per-trial
            # submission limit; the Ask joins it and the product enforces it.
            operation["decisions_trial_budget"] = init_trial_budget(config, binding, claim, deadline_unix_ms,
                                                                    remaining, os.environ)
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
        if decisions == "invocation_bounded":
            # Host-side copy beside the claim, outside the Harbor job directory.
            # Evidence only: a failed write never changes the Ask's outcome.
            try:
                operation["decisions_receipt"] = record_invocation(binding, result["stdout"], attempt)
            except (OSError, ValueError, TypeError):
                operation["decisions_receipt"] = None
            # The product's own ledger shows whether the Ask used its whole allowance.
            trial_budget = operation["decisions_trial_budget"]
            status = trial_budget["status_after_ask"] = trial_budget_status(config, trial_budget)
            if status is not None:
                trial_budget["remaining_submissions"] = status["remaining_submissions"]
                trial_budget["limit_reached"] = status["remaining_submissions"] == 0
        require(result["failure"] is None and result["return_code"] == 0, "reader_process")
        answer, metadata = (product_result(result["stdout"], config["reader_mode"]) if binding["arm"] == "gptgrep"
                            else codex_result(result["stdout"]))
        attempt.update(metadata)
        if binding["arm"] == "gptgrep":
            # A completed intentional abstention crosses as an ordinary response
            # for judging; the flag stays in the private operation record.
            operation["insufficient_evidence"] = metadata["insufficient_evidence"]
        outcome.update(status="response", answer=answer)
    except Exception as error:
        # Redacted code only: exception text may contain task/provider/private data.
        operation["failure"] = "reader_failed"
        code = error.args[0] if type(error) is ValueError and len(error.args) == 1 else None
        if code in RECORDED_FAILURE_CODES:
            operation["failure_code"] = code
    finally:
        if writer is not None:
            os.close(writer)
    trial_budget = operation.get("decisions_trial_budget")
    if trial_budget is not None and trial_budget["limit_reached"] is not None:
        # The product refuses a submission beyond the limit and that fails the
        # whole legacy Ask: a failed Ask with no remaining submission is bound
        # by the per-trial limit. It stays an operational failure, never retried.
        trial_budget["limit_bound"] = outcome["status"] != "response" and trial_budget["limit_reached"]
        if trial_budget["limit_bound"]:
            operation["failure_code"] = "decisions_submission_limit_reached"
    operation["cumulative_query_wall_ms"] = round((time.monotonic() - started) * 1000)
    operation["status"] = outcome["status"]
    operation["answer_sha256"] = sha(canonical(outcome))
    put("/logs/agent/operational.json", operation)
    # Only this bounded envelope crosses to the separate primary verifier.
    put("/answer/answer.json", outcome)


if __name__ == "__main__":
    main()
