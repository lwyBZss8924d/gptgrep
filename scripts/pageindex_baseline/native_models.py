"""Bounded accounting for native planner/reader turns, independent of host calls.

This module cannot invoke a model. Retained usage is an observation; a failed or
interrupted turn with partial usage never becomes a known complete total.
"""
from __future__ import annotations

import copy
import math

TOKEN_FIELDS = {
    "total_tokens": "totalTokens",
    "input_tokens": "inputTokens",
    "cached_input_tokens": "cachedInputTokens",
    "cache_write_input_tokens": "cacheWriteInputTokens",
    "output_tokens": "outputTokens",
    "reasoning_output_tokens": "reasoningOutputTokens",
}
STATUSES = {"reserved", "running", "process_completed", "completed", "failed", "interrupted"}
ROLES = {"query_planner", "final_reader"}
PLANNER_PROFILE = {"model": "gpt-5.6-luna", "reasoning_effort": "max", "service_tier": "fast"}


def _label(value, *, optional=False):
    if value is None and optional:
        return None
    if not isinstance(value, str) or not value or len(value.encode()) > 256 or any(ord(c) < 32 for c in value):
        raise ValueError("Native model identity/profile label is invalid")
    return value


def _tier(value):
    return "priority" if value in ("priority", "fast") else value


def attempts_from_report(report, reader_profile, *, required=False):
    """Validate explicit native records; do not infer an absent planner receipt."""
    if not isinstance(report, dict):
        raise ValueError("Native model accounting envelope must be an object")
    envelope = report
    if "model_attempts" not in envelope and isinstance(report.get("host_retrieval"), dict):
        envelope = report["host_retrieval"]
    if "model_attempts" not in envelope:
        if required:
            raise ValueError("Planned native workflow lacks model-attempt accounting")
        return None
    records = envelope["model_attempts"]
    if not isinstance(records, list) or len(records) > (2 if required else 1):
        raise ValueError("Native model-attempt bound differs from the declared workflow")
    identities, roles, result = set(), set(), []
    for record in records:
        if not isinstance(record, dict):
            raise ValueError("Native model attempt must be an object")
        role = record.get("role")
        if role not in ROLES or role in roles or (role == "query_planner" and not required):
            raise ValueError("Unexpected or duplicate native model role")
        roles.add(role)
        identifier = _label(record.get("attempt_id"))
        if identifier in identities:
            raise ValueError("Duplicate native model-attempt identity")
        identities.add(identifier)
        expected = PLANNER_PROFILE if role == "query_planner" else reader_profile
        for key in ("model", "reasoning_effort", "service_tier"):
            requested = _label(record.get("requested_" + key))
            if requested != expected[key]:
                raise ValueError("Native model-attempt requested profile differs")
        state = record.get("status")
        if state not in STATUSES:
            raise ValueError("Native model-attempt status is invalid")
        for key in ("model", "model_provider", "effective_reasoning_effort", "effective_service_tier", "thread_id", "turn_id"):
            _label(record.get(key), optional=True)
        if record.get("model") is not None and record["model"] != expected["model"]:
            raise ValueError("Native model-attempt observed model differs")
        if record.get("model_provider") not in (None, "openai"):
            raise ValueError("Native model-attempt observed provider differs")
        if record.get("effective_reasoning_effort") not in (None, expected["reasoning_effort"]):
            raise ValueError("Native model-attempt observed effort differs")
        if record.get("effective_service_tier") is not None and _tier(record["effective_service_tier"]) != _tier(expected["service_tier"]):
            raise ValueError("Native model-attempt observed service tier differs")
        if state in ("completed", "process_completed") and not all(record.get(key) for key in ("thread_id", "turn_id", "model", "model_provider")):
            raise ValueError("Completed native model step lacks its observed identity")
        elapsed = record.get("elapsed_ms")
        if type(elapsed) not in (int, float) or not math.isfinite(elapsed) or elapsed < 0:
            raise ValueError("Native model-attempt elapsed time is unavailable or invalid")
        if type(record.get("server_retry_notifications")) is not int or record["server_retry_notifications"] < 0:
            raise ValueError("Native retry notification count is invalid")
        if type(record.get("accounting_complete")) is not bool:
            raise ValueError("Native model accounting completeness must be explicit")
        usage = record.get("usage")
        if usage is not None:
            if not isinstance(usage, dict):
                raise ValueError("Native model usage must be an object or null")
            total = usage.get("total")
            if total is not None and not isinstance(total, dict):
                raise ValueError("Native total usage must be an object or null")
            for key in TOKEN_FIELDS.values():
                value = (total or {}).get(key)
                if value is not None and (type(value) is not int or not 0 <= value <= 2**64 - 1):
                    raise ValueError("Native token observation is invalid")
        # Keep bounded accounting fields only. No arbitrary model text/error detail.
        fields = ("attempt_id", "role", "status", "requested_model", "requested_reasoning_effort",
                  "requested_service_tier", "model", "model_provider", "effective_reasoning_effort",
                  "effective_service_tier", "thread_id", "turn_id", "elapsed_ms", "server_retry_notifications",
                  "accounting_complete")
        item = {key: copy.deepcopy(record.get(key)) for key in fields}
        item["usage"] = {"total": {key: (usage.get("total") or {}).get(key) for key in TOKEN_FIELDS.values()}} if usage else None
        result.append(item)
    if required and report.get("status") == "completed":
        if roles != ROLES or any(item["status"] != "completed" for item in result):
            raise ValueError("Completed planned workflow lacks a completed planner/reader pair")
        if len({(item["thread_id"], item["turn_id"]) for item in result}) != 2:
            raise ValueError("Planner and reader unexpectedly share a model turn")
    return result


def usage_summary(records):
    """Summarize disjoint steps; never add last-turn subsets or context capacity."""
    if records is None:
        return {"available": False, "attempted_calls": None, "observed_turns": None,
                "accounting_complete": False, "token_totals": None}
    seen, selected, duplicates = {}, [], 0
    for record in records:
        identity = (record.get("thread_id"), record.get("turn_id"))
        if all(identity):
            if identity in seen:
                prior = seen[identity]
                if (prior.get("role"), prior.get("usage"), prior.get("accounting_complete")) != (record.get("role"), record.get("usage"), record.get("accounting_complete")):
                    raise ValueError("Conflicting observations for one native model turn")
                duplicates += 1
                continue
            seen[identity] = record
        selected.append(record)
    complete_flags = all(record.get("accounting_complete") is True for record in selected)
    totals = {}
    for name, key in TOKEN_FIELDS.items():
        values = [((record.get("usage") or {}).get("total") or {}).get(key) for record in selected]
        known = [value for value in values if type(value) is int and 0 <= value <= 2**64 - 1]
        missing = len(values) - len(known)
        totals[name] = {"known_subtotal": sum(known), "missing_contributions": missing,
                        "total": sum(known) if missing == 0 and complete_flags else None}
    return {"available": True, "attempted_calls": len(records), "observed_turns": len(seen),
            "duplicate_turn_observations": duplicates, "distinct_accounting_contributions": len(selected),
            "missing_usage_attempts": sum(
                not any(type(((record.get("usage") or {}).get("total") or {}).get(key)) is int
                        for key in TOKEN_FIELDS.values())
                for record in selected
            ),
            "accounting_complete": complete_flags and all(value["missing_contributions"] == 0 for value in totals.values()),
            "token_totals": totals,
            "roles": {role: {"attempted_calls": sum(record.get("role") == role for record in selected),
                             "elapsed_ms_known_subtotal": sum(record.get("elapsed_ms", 0) for record in selected if record.get("role") == role)}
                      for role in sorted(ROLES)},
            "physical_provider_request_count": None, "provider_billing_usd": None,
            "usage_scope": "Per observed model turn; partial failed observations are known subtotals, not complete totals"}
