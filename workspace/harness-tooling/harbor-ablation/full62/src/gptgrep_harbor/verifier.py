"""Primary question/reference equivalence; no document or trace access."""
from __future__ import annotations

import os
from pathlib import Path

from .common import MAX_ANSWER, canonical, copy_asset, parse, put, read, require, sha, validate_binding
from .native import codex_accounting, codex_argv, codex_result, prompt_bytes
from .process import run
from .runtime import CODEX_VERSION, WORK_DIR, check_config, native_environment


def grade(config, envelope, invoke):
    validate_binding(config["binding"])
    require(set(envelope) == {"schema", "binding", "status", "answer"} and
            envelope.get("schema") == "gptgrep.harbor.answer.v1" and
            envelope["binding"] == config["binding"], "answer_binding")
    result = {"schema": "gptgrep.harbor.judgment.v1", "binding": config["binding"],
              "label": 0, "status": "operational_failure", "judge_attempts": [],
              "answer_envelope_sha256": sha(canonical(envelope)),
              "primary_documents_visible": False, "secondary_grounding": "unassessed"}
    if envelope["status"] == "operational_failure":
        require(envelope["answer"] is None, "failure_answer")
        return result
    require(envelope["status"] == "response" and isinstance(envelope.get("answer"), str)
            and envelope["answer"].strip(), "response_shape")
    answer = envelope["answer"]
    require(len(answer.encode()) <= MAX_ANSWER, "complete_answer_bound")
    # Entire answer, including an abstention, goes to the frozen primary rubric.
    # Oversize is a protocol failure, never a silently truncated judged answer.
    contract = config["contract"]
    prompt = contract["prompt"].format(question=" ".join(config["question"].split()),
                                       answer=config["reference"],
                                       answer_format=config["answer_format"], response=answer)
    result.update(label=None, status="judge_unavailable")
    try:
        verdict, metadata = invoke(prompt, contract["response_schema"])
        result["judge_attempts"].append(metadata)
        required = contract["response_schema"].get("required", [])
        properties = contract["response_schema"].get("properties", {})
        require(isinstance(verdict, dict) and set(required) <= set(verdict) <= set(properties),
                "judge_schema")
        for name, value in verdict.items():
            kind = properties[name].get("type")
            if kind == "boolean":
                require(type(value) is bool, "judge_boolean")
            elif kind == "string":
                require(isinstance(value, str), "judge_string")
            else:
                raise ValueError("unsupported_frozen_judge_schema")
        require(type(verdict.get("equivalent")) is bool, "judge_label")
        result.update(label=int(verdict["equivalent"]), status="judged",
                      verdict_sha256=sha(canonical(verdict)))
    except Exception:
        result["judge_error"] = "unavailable_or_invalid_verdict"
    return result


def transport_reward(result):
    label = result["label"]
    reward = {"label_available": int(label is not None),
              "answer_equivalence_lower": 0 if label is None else label,
              "answer_equivalence_upper": 1 if label is None else label}
    if label is not None:
        reward["answer_equivalence"] = label
    # Unknown has no primary scalar key. Harbor's per-key means are not the
    # project fixed-denominator metric; aggregate.py consumes every sidecar.
    return reward


def main():
    config = parse(read("/tests/judge.json"))
    check_config(config, "judge")
    require(config["reader_base_binary"]["path"] == config["codex_binary"], "judge_binary_path")
    copy_asset(config["reader_base_binary"], 2**30)
    require(not Path("/data/corpus").exists(), "primary_document_isolation")
    require(not Path("/logs/agent/operational.json").exists(), "primary_trace_isolation")
    attempt = {}

    def invoke(prompt, schema):
        # The frozen prompt carries the original rubric. Append no new rubric,
        # reference evidence or other-arm output. Strict JSON is already required
        # by the frozen contract selected during materialization.
        native_input = prompt_bytes(prompt)
        put("/logs/verifier/attempt-start.json", {"binding": config["binding"], "profile": config["judge_profile"]})
        import time
        started = time.monotonic()
        env = native_environment(os.environ)
        version = run([config["codex_binary"], "--version"], cwd=WORK_DIR,
                      timeout=min(5, config["judge_deadline_seconds"]), env=env)
        require(version["return_code"] == 0 and version["failure"] is None and
                version["stdout"].strip() == ("codex-cli " + CODEX_VERSION).encode(), "native_version_mismatch")
        remaining = config["judge_deadline_seconds"] - (time.monotonic() - started)
        require(remaining > 0, "judge_deadline")
        response = run(codex_argv(config["codex_binary"], profile=config["judge_profile"], role="judge"),
                       cwd=WORK_DIR, timeout=remaining, env=env, input_bytes=native_input)
        attempt.update(elapsed_ms=response["elapsed_ms"], return_code=response["return_code"],
                       observed_native_version=version["stdout"].decode().strip(),
                       native_config_sha256=config["native_config_sha256"],
                       base_tool_environment=config["base_tool_environment"],
                       failure=response["failure"], requested_profile=config["judge_profile"],
                       stdin_bytes=response["stdin_bytes"], stdin_bytes_sent=response["stdin_bytes_sent"],
                       stdin_complete=response["stdin_complete"],
                       native_process_incomplete=(response["failure"] is not None or response["return_code"] != 0
                                                  or not response["stdin_complete"]),
                       stdout_sha256=sha(response["stdout"]), stderr_sha256=sha(response["stderr"]),
                       cost_usd=None, cost_complete=False)
        attempt.update(codex_accounting(response["stdout"]))
        require(response["failure"] is None and response["return_code"] == 0, "judge_process")
        text, usage = codex_result(response["stdout"])
        attempt.update(usage)
        return parse(text.encode()), dict(attempt)

    try:
        envelope = parse(read("/answer/answer.json"))
        result = grade(config, envelope, invoke)
    except (OSError, ValueError, KeyError, TypeError):
        result = {"schema": "gptgrep.harbor.judgment.v1", "binding": config["binding"],
                  "label": 0, "status": "operational_failure", "judge_attempts": [],
                  "failure": "missing_or_invalid_answer", "secondary_grounding": "unassessed"}
    if attempt and not result["judge_attempts"]:
        result["judge_attempts"].append(attempt)
    put("/logs/verifier/judgment.json", result)
    put("/logs/verifier/reward.json", transport_reward(result))


if __name__ == "__main__":
    main()
