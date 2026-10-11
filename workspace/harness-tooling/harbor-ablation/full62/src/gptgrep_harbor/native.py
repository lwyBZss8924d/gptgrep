"""Native Codex CLI envelope. Profile requests are not served-model proof."""
from __future__ import annotations

import json

from .common import MAX_ANSWER, MAX_JSON, MAX_NATIVE_PROMPT_BYTES, PROFILE, integer, parse, require


def codex_argv(binary, *, profile=None, role="reader"):
    # The isolated sandbox supplies its own Dev/Test/Eval account environment.
    # No session resume, caller home copy, arbitrary shell command or retry.
    profile = PROFILE if profile is None else profile
    require(role in ("reader", "judge"), "native_role")
    return [binary, "exec", "--skip-git-repo-check", "--ephemeral",
            "--model", profile["model"], "--json",
            "-c", "default_permissions=" + json.dumps("full62_" + role),
            "-c", 'cli_auth_credentials_store="file"',
            "-c", 'forced_login_method="chatgpt"',
            "-c", 'model_provider="openai"',
            "-c", "model_reasoning_effort=" + json.dumps(profile["reasoning_effort"]),
            "-c", "service_tier=" + json.dumps(profile["service_tier"]),
            "--", "-"]


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
    events = malformed = invalid_usage = omitted_usage = 0
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
    complete = completions == 1 and not (failed or malformed or invalid_usage or omitted_usage) and answer_valid
    return answer, {"usage": observations[0]["usage"] if completions == 1 and len(observations) == 1 else None,
                    "native_completed_turn_usage": observations,
                    "native_usage_observations_omitted": omitted_usage,
                    "native_completed_turns": completions, "native_completion_ambiguous": completions > 1,
                    "native_malformed_events": malformed, "native_invalid_usage_events": invalid_usage,
                    "native_error_observed": failed, "native_answer_valid": answer_valid,
                    "native_stream_incomplete": not complete,
                    "accounting_projection_complete": not (malformed or invalid_usage or omitted_usage),
                    "usage_scope": "observed_completed_turns_not_total_provider_usage",
                    "tool_calls": tool_calls, "physical_provider_requests": None,
                    "cost_usd": None, "cost_complete": False,
                    "reported_profile": None, "served_profile_verified": False, "event_count": events}


def codex_accounting(data):
    return _projection(data)[1]


def codex_result(data):
    answer, metadata = _projection(data)
    require(not metadata["native_completion_ambiguous"], "duplicate_native_completion")
    require(not metadata["native_error_observed"], "native_turn_failed")
    require(not metadata["native_stream_incomplete"], "native_final_missing_or_invalid")
    return answer, metadata
