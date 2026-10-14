"""Native Codex CLI envelope. Profile requests are not served-model proof."""
from __future__ import annotations

import json

from .common import MAX_ANSWER, MAX_JSON, MAX_NATIVE_PROMPT_BYTES, PROFILE, integer, parse, require

# Materialized from the frozen contract's response_schema into the Judge's
# private /tests area; judge.json pins these exact bytes.
JUDGE_OUTPUT_SCHEMA = "/tests/judge-output-schema.json"
MAX_OUTPUT_SCHEMA_BYTES = 65536


def _key_sorted(value):
    if isinstance(value, dict):
        return {key: _key_sorted(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [_key_sorted(item) for item in value]
    return value


def output_schema_bytes(schema):
    """Exact bytes of the frozen response schema that the native Judge reads.

    Strict structured output generates keys in `properties` order, and Codex
    sends the file's key order unchanged. The frozen contract file is key-sorted,
    so `properties` follows the contract's `required` list, which keeps the
    frozen judge's order. Every other object is key-sorted, so the verifier can
    recompute these exact bytes from the contract.
    """
    required = schema.get("required") if type(schema) is dict else None
    properties = schema.get("properties") if type(schema) is dict else None
    require(type(required) is list and type(properties) is dict and
            all(isinstance(name, str) for name in required) and
            len(set(required)) == len(required) and set(required) == set(properties),
            "output_schema_property_order")
    ordered = _key_sorted(schema)
    # Replacing the value keeps the key's sorted position at the top level.
    ordered["properties"] = {name: _key_sorted(properties[name]) for name in required}
    data = json.dumps(ordered, separators=(",", ":"), ensure_ascii=False,
                      allow_nan=False).encode("utf-8") + b"\n"
    require(len(data) <= MAX_OUTPUT_SCHEMA_BYTES, "output_schema_bound")
    return data


def codex_argv(binary, *, profile=None, role="reader", output_schema=None):
    # The isolated sandbox supplies its own Dev/Test/Eval account environment.
    # No session resume, caller home copy, arbitrary shell command or retry.
    profile = PROFILE if profile is None else profile
    require(role in ("reader", "judge"), "native_role")
    argv = [binary, "exec", "--skip-git-repo-check", "--ephemeral",
            "--model", profile["model"], "--json",
            "-c", "default_permissions=" + json.dumps("full62_" + role),
            "-c", 'cli_auth_credentials_store="file"',
            "-c", 'forced_login_method="chatgpt"',
            "-c", 'model_provider="openai"',
            "-c", "model_reasoning_effort=" + json.dumps(profile["reasoning_effort"]),
            "-c", "service_tier=" + json.dumps(profile["service_tier"])]
    if output_schema is not None:
        # Only the Judge requests native structured output. The CLI reads this
        # file itself before the turn; reader argv stays unchanged.
        require(role == "judge" and isinstance(output_schema, str) and output_schema.startswith("/"),
                "output_schema_argument")
        argv += ["--output-schema", output_schema]
    return argv + ["--", "-"]


def prompt_bytes(prompt):
    require(isinstance(prompt, str), "native_prompt_type")
    data = prompt.encode("utf-8")
    require(0 < len(data) <= MAX_NATIVE_PROMPT_BYTES, "complete_native_prompt_bound")
    return data


def _projection(data):
    """Observe bounded events without requiring a usable final answer.

    Duplicate completions remain separate observations, never additive totals.
    A malformed suffix or failed process cannot erase earlier usage receipts.
    """
    require(isinstance(data, bytes) and len(data) <= MAX_JSON, "native_stream_bound")
    answer = None
    completions = 0
    tool_calls = 0
    events = malformed = missing_usage = invalid_usage = omitted_usage = 0
    failed = False
    observations = []
    for raw in data.splitlines():
        if not raw.strip():
            continue
        events += 1
        try:
            event = parse(raw)
            require(isinstance(event, dict), "native_event")
        except (ValueError, TypeError, RecursionError):
            malformed += 1
            continue
        kind = event.get("type")
        if kind in ("turn.failed", "error"):
            failed = True
        if kind == "item.completed":
            item = event.get("item", {})
            if not isinstance(item, dict):
                malformed += 1
                continue
            if item.get("type") == "agent_message":
                answer = item.get("text")
                if not isinstance(answer, str):
                    malformed += 1
            elif item.get("type") in ("command_execution", "mcp_tool_call", "web_search"):
                tool_calls += 1
        if kind == "turn.completed":
            completions += 1
            usage = event.get("usage")
            if usage is None:
                missing_usage += 1
                continue
            try:
                require(isinstance(usage, dict), "native_usage_missing")
                totals = {key: integer(usage.get(key)) for key in
                          ("input_tokens", "cached_input_tokens", "output_tokens")}
                require(totals["cached_input_tokens"] <= totals["input_tokens"], "cache_subset")
            except (ValueError, TypeError):
                invalid_usage += 1
                continue
            if len(observations) < 64:
                observations.append({"completion_ordinal": completions, "usage": totals})
            else:
                omitted_usage += 1
    try:
        answer_valid = isinstance(answer, str) and bool(answer.strip()) and len(answer.encode("utf-8")) <= MAX_ANSWER
    except UnicodeError:
        answer_valid = False
    complete = completions == 1 and not (failed or malformed or missing_usage or invalid_usage or omitted_usage) and answer_valid
    return answer, {"usage": observations[0]["usage"] if completions == 1 and len(observations) == 1 else None,
                    "native_completed_turn_usage": observations,
                    "native_usage_observations_omitted": omitted_usage,
                    "native_completed_turns": completions, "native_completion_ambiguous": completions > 1,
                    "native_missing_usage_events": missing_usage,
                    "native_malformed_events": malformed, "native_invalid_usage_events": invalid_usage,
                    "native_error_observed": failed, "native_answer_valid": answer_valid,
                    "native_stream_incomplete": not complete,
                    "accounting_projection_complete": not (malformed or missing_usage or invalid_usage or omitted_usage),
                    "usage_scope": "observed_completed_turns_not_total_provider_usage",
                    "tool_calls": tool_calls, "physical_provider_requests": None,
                    "cost_usd": None, "cost_complete": False,
                    "reported_profile": None, "served_profile_verified": False, "event_count": events}


def codex_accounting(data):
    return _projection(data)[1]


def codex_result(data, *, allow_missing_usage=False):
    answer, metadata = _projection(data)
    require(not metadata["native_completion_ambiguous"], "duplicate_native_completion")
    require(not metadata["native_error_observed"], "native_turn_failed")
    # Reader quality acceptance is independent of absent accounting, as for
    # product_result. Judge callers keep the strict default. Invalid reported
    # usage, malformed events and missing/ambiguous terminals remain failures.
    missing_accounting_only = (
        allow_missing_usage and metadata["native_completed_turns"] == 1
        and metadata["native_answer_valid"] and metadata["native_missing_usage_events"] == 1
        and not (metadata["native_malformed_events"] or metadata["native_invalid_usage_events"]
                 or metadata["native_usage_observations_omitted"]))
    require(not metadata["native_stream_incomplete"] or missing_accounting_only,
            "native_final_missing_or_invalid")
    return answer, metadata
