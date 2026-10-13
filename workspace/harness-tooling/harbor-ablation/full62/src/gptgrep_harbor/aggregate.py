"""Private fixed-denominator paired aggregation; never use Harbor mean alone."""
from __future__ import annotations

import math
import random
import statistics

from .common import (ARMS, MAX_ANSWER, binding_equal, canonical, finite_number, integer, parse, pinned, require, sha,
                     tool_cap_parity, validate_binding)
from .adapter import qualification, treatment_feature_profile
from .decisions import TRIAL_LIMIT_ENFORCER, selected_mode
from .reader import TREATMENT_IDENTITY


def trial_decisions_cost(attempts):
    """A trial's reported Decisions cost, or None unless every attempt's receipt is complete."""
    receipts = [attempt.get("decisions_cost") for attempt in attempts]
    complete = bool(receipts) and all(isinstance(receipt, dict) and receipt.get("cost_complete") is True
                                      and finite_number(receipt.get("reported_cost_usd")) for receipt in receipts)
    return sum(receipt["reported_cost_usd"] for receipt in receipts) if complete else None


def decisions_receipts(attempts):
    """The decisions-cost receipts that a trial's attempts carry."""
    return [attempt["decisions_cost"] for attempt in attempts if isinstance(attempt.get("decisions_cost"), dict)]


def decisions_costs(rows, runtime):
    """Aggregate treatment receipts; one unknown trial keeps the cohort total unknown."""
    known = [row["decisions_cost_usd"] for row in rows if row["decisions_cost_usd"] is not None]
    partial = [value for row in rows for value in row["retrieval_known"]]
    total = sum(known) if len(known) == len(rows) else None
    cap = runtime.get("decisions_cost_cap_usd")
    mode = selected_mode(runtime)
    limit = None
    if mode == "invocation_bounded":
        # The product enforces the per-trial submission limit; a failed Ask with
        # no remaining submission is limit-bound (an operational failure).
        budgets = [row["trial_budget"] if type(row["trial_budget"]) is dict else {} for row in rows]
        limit = {"max_submissions_per_trial": runtime.get("decisions_max_submissions_per_trial"),
                 "enforced_by": TRIAL_LIMIT_ENFORCER,
                 "bound_trials": sum(budget.get("limit_bound") is True for budget in budgets),
                 "reached_trials": sum(budget.get("limit_reached") is True for budget in budgets),
                 "unobserved_trials": sum(type(budget.get("limit_reached")) is not bool for budget in budgets)}
    return {"mode": mode, "treatment_trials": len(rows),
            "cost_complete_trials": len(known), "cost_unknown_trials": len(rows) - len(known),
            # Trials whose attempts carry a decisions-cost receipt, and the priced
            # Jev usage rows inside those receipts (one Ask has many rows).
            "trials_with_cost_receipt": sum(bool(row["decisions_receipts"]) for row in rows),
            "priced_usage_rows": sum(receipt["priced_rows"] for row in rows for receipt in row["decisions_receipts"]
                                     if type(receipt.get("priced_rows")) is int and receipt["priced_rows"] >= 0),
            "reported_cost_usd_known_subtotal": sum(partial) if partial else None,
            "reported_cost_usd_total": total, "cost_cap_usd": cap,
            "total_within_cap": total <= cap if total is not None and finite_number(cap) else None,
            "submission_limit": limit,
            "currency_verified": False, "included_in_query_cost": False}


def trial_claims(values, runtime):
    """invocation_bounded: every trial of both arms claims once; flag re-executed trials.

    A trial that found its claim already present (for example in a resumed job)
    is an operational failure, and its pair may mix two executions.
    """
    if selected_mode(runtime) != "invocation_bounded":
        return None
    arms, pairs = {}, set()
    for arm, rows in values.items():
        repeated = [row for row in rows if row["failure_code"] == "trial_already_claimed"]
        pairs.update(row["pair_id"] for row in repeated)
        arms[arm] = {"claimed": sum(row["trial_claimed"] for row in rows), "pre_existing_claim": len(repeated),
                     "unclaimed": sum(not row["trial_claimed"] for row in rows) - len(repeated)}
    return {"claimed_arms": list(ARMS), "arms": arms, "pairs_with_pre_existing_claim": len(pairs)}


def reader_contracts(contracts, runtime):
    """Check the operation records' reader contracts against the frozen declaration.

    The raw-only arm must report that it received none of GPTgrep's contract.
    Treatment trials report their identity, the verified bundle provenance
    digest, which binds the contract text compiled into the product; one cohort
    is expected to share one build. legacy_text treatment trials also report the
    digest of the product's own description of its answer rules: it is not the
    rule text and is never counted as the identity.
    """
    def recorded(contract, key):
        value = contract.get(key) if type(contract) is dict else None
        return value if isinstance(value, str) else None
    described = runtime.get("reader_mode") == "legacy_text"
    treatment, raw = contracts["gptgrep"], contracts["raw_only"]
    builds = sorted({recorded(c, TREATMENT_IDENTITY) for c in treatment} - {None})
    descriptions = sorted({recorded(c, "answer_rules_description_sha256") for c in treatment} - {None})
    return {"declared_treatment": True, "treatment_identity": TREATMENT_IDENTITY,
            "treatment_product_provenance_sha256": builds,
            "treatment_identity_consistent": len(builds) <= 1,
            "treatment_records_without_identity": sum(recorded(c, TREATMENT_IDENTITY) is None for c in treatment),
            "treatment_answer_rules_expected": described,
            "treatment_answer_rules_description_sha256": descriptions,
            "treatment_records_without_answer_rules_description": (
                sum(recorded(c, "answer_rules_description_sha256") is None for c in treatment) if described else 0),
            "raw_only_records_without_gptgrep_contract": sum(type(c) is dict and c.get("gptgrep_contract_received")
                                                             is False for c in raw),
            "raw_only_records_reporting_gptgrep_contract": sum(type(c) is dict and c.get("gptgrep_contract_received")
                                                               is not False for c in raw),
            "raw_only_records_unobserved": sum(type(c) is not dict for c in raw)}


def judge_abstained(judgment, status):
    """The retained verdict's `abstained` flag for a judged task, or None when it is absent.

    A retained verdict must match its digest. Older judgments without one stay
    unobserved, never counted as non-abstentions.
    """
    if judgment is None or status != "judged" or "verdict" not in judgment:
        return None
    verdict = judgment["verdict"]
    require(type(verdict) is dict and len(verdict) <= 64 and
            all(isinstance(key, str) and type(value) in (bool, str) for key, value in verdict.items()) and
            type(verdict.get("equivalent")) is bool and verdict["equivalent"] == bool(judgment["label"]) and
            judgment.get("verdict_sha256") == sha(canonical(verdict)), "judge_verdict_binding")
    return verdict["abstained"] if type(verdict.get("abstained")) is bool else None


def abstentions(rows):
    """Per-arm intentional-abstention counts; answerability slices need evaluator metadata."""
    return {"product_insufficient_evidence": sum(r["insufficient_evidence"] for r in rows),
            "judge_abstained": sum(r["judge_abstained"] is True for r in rows),
            "judged_abstention_unobserved": sum(r["status"] == "judged" and r["judge_abstained"] is None
                                                for r in rows),
            "answerability_slices": None}


def quantile(values, q):
    if not values:
        return None
    ordered = sorted(values)
    point = (len(ordered) - 1) * q
    low = math.floor(point)
    high = math.ceil(point)
    return ordered[low] + (ordered[high] - ordered[low]) * (point - low)


def paired(rows, seed, replicates=10000):
    require(len(rows) == 62 and len({r["pair_id"] for r in rows}) == 62 and
            len({r["document_id"] for r in rows}) == 34, "paired_denominator")
    cells = {"both_correct": 0, "treatment_only_correct": 0,
             "baseline_only_correct": 0, "both_incorrect": 0, "unknown_pairs": 0}
    low, high = 0, 0
    clusters = {}
    for row in rows:
        t, b = row["gptgrep"], row["raw_only"]
        require(t in (0, 1, None) and b in (0, 1, None), "label_domain")
        low += (0 if t is None else t) - (1 if b is None else b)
        high += (1 if t is None else t) - (0 if b is None else b)
        if t is None or b is None:
            cells["unknown_pairs"] += 1
        else:
            key = {(1, 1): "both_correct", (1, 0): "treatment_only_correct",
                   (0, 1): "baseline_only_correct", (0, 0): "both_incorrect"}[(t, b)]
            cells[key] += 1
            clusters.setdefault(row["document_id"], []).append(t - b)
    result = {**cells, "pairs_expected": 62, "exact_delta": None,
              "delta_bounds": [low / 62, high / 62], "cluster_interval": None,
              "mcnemar_exact_two_sided_p": None, "significance_is_gate": False,
              "mcnemar_cluster_limitation": "task-level diagnostic ignores document dependence",
              "bootstrap": {"seed": integer(seed), "replicates": replicates,
                            "clusters": 34, "quantile": "linear_type7", "unknown_imputation": False}}
    if cells["unknown_pairs"]:
        return result
    b, c = cells["treatment_only_correct"], cells["baseline_only_correct"]
    result["exact_delta"] = (b - c) / 62
    n = b + c
    result["mcnemar_exact_two_sided_p"] = (min(1.0, 2 * sum(math.comb(n, k) for k in range(min(b, c) + 1)) / 2**n)
                                           if n else 1.0)
    rng = random.Random(seed)
    ids = sorted(clusters)
    values = []
    for _ in range(replicates):
        sample = [clusters[rng.choice(ids)] for _ in ids]
        values.append(sum(sum(cluster) for cluster in sample) / sum(map(len, sample)))
    result["cluster_interval"] = [quantile(values, 0.025), quantile(values, 0.975)]
    return result


def time_bounds(label, status, elapsed, deadline):
    require(finite_number(deadline) and deadline > 0, "deadline")
    if label == 0 or status in ("operational_failure", "missing_task"):
        return deadline, deadline
    if not finite_number(elapsed):
        return 0, deadline
    observed = min(elapsed, deadline)
    return (observed, deadline) if label is None else (observed, observed)


def summarize(export, records, runtime):
    expected = export["tasks"]
    require(export["expected_pairs"] == 62 and export["expected_documents"] == 34 and
            len(expected) == 124, "export_size")
    names = {row["task_name"] for row in expected}
    require(len(names) == 124 and len(records) == 124 and
            len({row["task_name"] for row in records}) == 124 and
            {row["task_name"] for row in records} == names, "missing_or_duplicate_result_binding")
    by_name = {row["task_name"]: row for row in records}
    values, pairs = {arm: [] for arm in ARMS}, {}
    contracts = {arm: [] for arm in ARMS}
    judge_attempts = []
    for item in expected:
        binding = validate_binding(item["binding"])
        require(binding["cohort_sha256"] == export["cohort"]["sha256"] and
                binding["runtime_sha256"] == export["runtime"]["sha256"] and
                binding["judge_sha256"] == export["judge_contract"]["sha256"], "export_subject_join")
        arm, pair = binding["arm"], binding["pair_id"]
        require(arm in ARMS, "arm")
        record = by_name[item["task_name"]]
        require(set(record) == {"task_name", "answer", "judgment", "operation", "grounding"}, "result_fields")
        answer = parse(pinned(record["answer"])) if record["answer"] else None
        judgment = parse(pinned(record["judgment"])) if record["judgment"] else None
        operation = parse(pinned(record["operation"])) if record["operation"] else None
        grounding = parse(pinned(record["grounding"], 65536)) if record["grounding"] else None
        for body in (answer, judgment, operation, grounding):
            if body is not None:
                require(body["binding"] == binding,
                        "result_subject_mismatch")
        if answer is not None:
            require(set(answer) == {"schema", "binding", "status", "answer"} and
                    answer["schema"] == "gptgrep.harbor.answer.v1", "answer_shape")
        if judgment is not None and judgment.get("status") in ("judged", "judge_unavailable"):
            require(answer is not None and answer.get("status") == "response" and
                    isinstance(answer.get("answer"), str) and bool(answer["answer"].strip()) and
                    len(answer["answer"].encode()) <= MAX_ANSWER and
                    judgment.get("answer_envelope_sha256") == sha(canonical(answer)), "judged_answer_join")
        if judgment is not None:
            observed_judge = judgment.get("judge_attempts", [])
            require(isinstance(observed_judge, list) and len(observed_judge) <= 100, "judge_attempts_bound")
            judge_attempts.extend(observed_judge)
        if judgment is None:
            # A verifier startup failure still leaves a valid response unknown.
            valid = (answer is not None and answer.get("schema") == "gptgrep.harbor.answer.v1" and
                     answer.get("status") == "response" and isinstance(answer.get("answer"), str) and
                     bool(answer["answer"].strip()) and len(answer["answer"].encode()) <= MAX_ANSWER)
            label, status = (None, "judge_unavailable") if valid else (0, "missing_task")
        else:
            label, status = judgment["label"], judgment["status"]
            require((status == "judged" and type(label) is int and label in (0, 1)) or
                    (status == "operational_failure" and type(label) is int and label == 0) or
                    (status == "judge_unavailable" and label is None), "label_status")
        abstained = judge_abstained(judgment, status)
        if operation is not None:
            require(operation["schema"] == "gptgrep.harbor.operation.v1", "operation_schema")
            require(operation["deadline_ms"] == runtime["deadline_seconds"] * 1000, "deadline_join")
            if status in ("judged", "judge_unavailable"):
                require(answer is not None and operation["status"] == "response" and
                        operation["answer_sha256"] == sha(canonical(answer)), "answer_operation_join")
                if judgment is not None:
                    require(operation["answer_sha256"] == judgment["answer_envelope_sha256"], "judge_answer_join")
            attempts = operation["attempts"]
            require(isinstance(attempts, list) and len(attempts) <= 100 and
                    len({a["ordinal"] for a in attempts}) == len(attempts), "unique_outer_attempts")
        else:
            attempts = []
        contracts[arm].append(operation.get("reader_contract") if operation is not None else None)
        if grounding is not None:
            require(grounding["schema"] == "gptgrep.harbor.grounding.v1" and
                    grounding["primary_label_modified"] is False and
                    len(grounding["metrics"]) <= 64, "grounding_sidecar")
        elapsed = operation.get("cumulative_query_wall_ms") if operation else None
        low, high = time_bounds(label, status, elapsed, runtime["deadline_seconds"] * 1000)
        known_cost = sum(a["cost_usd"] for a in attempts if finite_number(a.get("cost_usd")))
        complete_cost = bool(attempts) and all(a.get("cost_complete") is True and
                                               finite_number(a.get("cost_usd")) for a in attempts)
        values[arm].append({"pair_id": pair, "label": label, "status": status, "elapsed": elapsed,
                            "trial_claimed": operation is not None and type(operation.get("trial_claim")) is dict,
                            "time_lower": low, "time_upper": high, "known_cost_usd": known_cost,
                            "cost_complete": complete_cost, "attempt_count": len(attempts),
                            "grounding_available": grounding is not None,
                            "retrieval_known": [a["reported_retrieval_cost_subtotal"] for a in attempts
                                                if finite_number(a.get("reported_retrieval_cost_subtotal"))],
                            "decisions_receipts": decisions_receipts(attempts),
                            "decisions_cost_usd": trial_decisions_cost(attempts),
                            "trial_budget": operation.get("decisions_trial_budget") if operation else None,
                            "failure_code": (operation.get("failure_code") if operation is not None and
                                             isinstance(operation.get("failure_code"), str) else None),
                            "judge_abstained": abstained,
                            # A completed legacy abstention keeps this flag in the private operation record.
                            "insufficient_evidence": operation is not None and status in ("judged", "judge_unavailable")
                                                     and operation.get("insufficient_evidence") is True,
                            "final_reader_usage": [a["usage"] for a in attempts if isinstance(a.get("usage"), dict)]})
        if pair in pairs:
            require(binding_equal(pairs[pair]["binding"], binding) and arm not in pairs[pair], "paired_binding")
        else:
            pairs[pair] = {"binding": binding, "pair_id": pair, "document_id": binding["document_id"]}
        pairs[pair][arm] = label
    require(len(pairs) == 62 and all(set(ARMS) <= set(r) for r in pairs.values()), "paired_complete")
    summary = {}
    for arm, rows in values.items():
        require(len(rows) == 62, "arm_denominator")
        correct = sum(r["label"] == 1 for r in rows)
        unknown = sum(r["label"] is None for r in rows)
        failures = sum(r["status"] in ("operational_failure", "missing_task") for r in rows)
        cost_complete = all(r["cost_complete"] for r in rows)
        known = sum(r["known_cost_usd"] for r in rows)
        elapsed = [r["elapsed"] for r in rows if finite_number(r["elapsed"])]
        retrieval_costs = [v for r in rows for v in r["retrieval_known"]]
        usage_rows = [v for r in rows for v in r["final_reader_usage"]]
        usage_subtotals = {}
        for key in ("input_tokens", "cached_input_tokens", "output_tokens"):
            seen = [r[key] for r in usage_rows if type(r.get(key)) is int and r[key] >= 0]
            usage_subtotals[key] = {"known_subtotal": sum(seen) if seen else None, "observed_records": len(seen)}
        summary[arm] = {"denominator": 62, "correct": correct, "judge_unknown": unknown,
                        "operational_failures": failures, "judged_incorrect": 62 - correct - unknown - failures,
                        "missing_tasks": sum(r["status"] == "missing_task" for r in rows),
                        "accuracy": correct / 62 if not unknown else None,
                        "accuracy_bounds": [correct / 62, (correct + unknown) / 62],
                        "failure_aware_mean_ms_bounds": [sum(r["time_lower"] for r in rows) / 62,
                                                        sum(r["time_upper"] for r in rows) / 62],
                        "query_wall_ms_median_observed_subset": quantile(elapsed, 0.5),
                        "query_wall_ms_p95_observed_subset": quantile(elapsed, 0.95),
                        "query_wall_unknown": 62 - len(elapsed),
                        "known_query_cost_usd_subtotal": known, "query_cost_usd": known if cost_complete else None,
                        "query_cost_unknown_tasks": sum(not r["cost_complete"] for r in rows),
                        "cost_per_correct_usd": known / correct if cost_complete and correct else None,
                        "outer_attempts_observed": sum(r["attempt_count"] for r in rows),
                        "grounding_unassessed": sum(not r["grounding_available"] for r in rows)}
        codes = {}
        for row in rows:
            if row["failure_code"] is not None and row["status"] in ("operational_failure", "missing_task"):
                codes[row["failure_code"]] = codes.get(row["failure_code"], 0) + 1
        summary[arm].update(intentional_abstentions=abstentions(rows),
                            operational_failure_codes=dict(sorted(codes.items())),
                            final_reader_usage_diagnostic=usage_subtotals,
                            all_model_turn_usage_complete=False,
                            reported_retrieval_cost_known_subtotal=sum(retrieval_costs) if retrieval_costs else None,
                            retrieval_cost_denomination_verified=False,
                            usage_subsets_not_added_twice=True)
    comparison = paired(list(pairs.values()), runtime["bootstrap_seed"])
    checked = qualification(runtime.get("qualification_mismatches", []))
    profile = treatment_feature_profile(runtime)
    frozen = export.get("preflight", {}).get("treatment_feature_profile")
    require(frozen is None or frozen == profile, "treatment_profile_join")
    claims = trial_claims(values, runtime)
    reexecuted = []
    if claims is not None and claims["pairs_with_pre_existing_claim"]:
        reexecuted = [str(claims["pairs_with_pre_existing_claim"]) + " pairs include a trial that found its claim "
                      "already present (a re-execution); that trial is an operational failure and its pair may "
                      "mix two executions."]
    contract = reader_contracts(contracts, runtime)
    builds = len(contract["treatment_product_provenance_sha256"])
    mixed_builds = []
    if builds > 1:
        mixed_builds = [str(builds) + " distinct product provenance digests among the treatment records: the "
                        "treatment trials did not all run one product build, so their reader contracts may differ."]
    return {"schema": "gptgrep.harbor.paired-summary.v1", "search": summary, "paired": comparison,
            "decisions": decisions_costs(values["gptgrep"], runtime),
            "reader_contract": contract,
            "trial_claims": claims,
            "judge": {"outer_attempts_observed": len(judge_attempts),
                      "observed_attempt_wall_ms_subtotal": sum(a["elapsed_ms"] for a in judge_attempts
                                                               if finite_number(a.get("elapsed_ms"))),
                      "wall_subtotal_is_query_wall": False,
                      "usage_records_observed": sum(isinstance(a.get("usage"), dict) for a in judge_attempts),
                      "cost_usd": None, "complete_accounting": False, "included_in_primary_search": False},
            "index": {"included_in_primary_reward": False, "comprehensive_index_evidence": False,
                      "status": "separate_ledger_required"},
            "profile": {"requested": runtime["profile"], "requested_tier_normalized": "priority",
                        "actually_served_verified": False, "reader_base_binary": runtime.get("reader_base_binary"),
                        "permitted_tools": runtime.get("permitted_tools"),
                        "generic_capabilities": runtime.get("generic_capabilities"),
                        "qualification_mismatches": checked["mismatches"],
                        "declared_treatment_differences": checked["declared_treatment_differences"],
                        "treatment_feature_profile": profile,
                        "budget_scope": {"query_deadline": "shared", "instruction_bytes": "shared_local_cap",
                            "tool_calls": "treatment_enforced_raw_observed_only",
                            "treatment_max_tool_calls": runtime.get("max_tool_calls"), "raw_tool_call_cap": "none",
                            "treatment_tool_cap_parity": tool_cap_parity(runtime.get("max_tool_calls")),
                            "total_provider_input": "not_enforced_by_raw_cli"}},
            "preregistered_primary": runtime["primary_metric"],
            "accuracy_improvement_logically_supported": comparison["delta_bounds"][0] > 0,
            "acceptance": False, "g5": "HOLD_SEPARATE_COST_PROFILE_AND_INDEPENDENT_REVIEW",
            "limitations": ["No served-profile proof is inferred from requested CLI flags.",
              "Costs missing native or physical-provider coverage remain unknown.",
              "No full62 result establishes comprehensive INDEX quality.",
              "Answerable and unanswerable slices need evaluator-side answerability metadata, "
              "which the cohort projection does not carry; abstention counts are not those slices.",
              "Source preparation and aggregation do not grant release or Goal acceptance."] + reexecuted + mixed_builds}
