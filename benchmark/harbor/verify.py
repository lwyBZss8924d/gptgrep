"""Controller-owned XL native scoring and independent, bound judge feedback.

This module never calls a judge/model. A separately executed EvalsAgent supplies
judgment.json. Unknown judgment is preserved, not converted into a numeric loss.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
import stat
import sys
from pathlib import Path
from typing import Any

sys.dont_write_bytecode = True

NATIVE_SCORER_SHA256 = "efc56cb25ba136e09ceaccb5d8006ea9461f71a68998f2987073387682a5e547"
NATIVE_KEYS = ("native_accuracy", "native_token_f1", "native_anls")
GROUNDING_KEYS = ("grounded_success", "atomic_coverage")
MAX_INPUT_BYTES = 4 * 1024 * 1024
MAX_JSON_OUTPUT_BYTES = 1024 * 1024
MAX_REASON_BYTES = 8 * 1024
MAX_CRITERION_SOURCE_BYTES = 16 * 1024


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_bounded(path: Path, max_bytes: int = MAX_INPUT_BYTES) -> bytes:
    """Read one bounded regular artifact; changed or symlinked inputs are unknown."""
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode) or before.st_size > max_bytes:
        raise ValueError("Verifier input is not a bounded regular file")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        current = os.fstat(stream.fileno())
        if (current.st_dev, current.st_ino, current.st_size) != (before.st_dev, before.st_ino, before.st_size):
            raise ValueError("Verifier input changed while opening")
        raw = stream.read(max_bytes + 1)
        if len(raw) != current.st_size or len(raw) > max_bytes:
            raise ValueError("Verifier input changed or exceeded its byte bound")
    return raw


def reason_record(text: str, field: str = "reason") -> dict[str, Any]:
    """Keep a UTF-8 preview and identity; the pinned judgment retains full reasons."""
    if not isinstance(text, str):
        raise ValueError("Verifier reason must be a string")
    raw = text.encode("utf-8")
    preview = raw[:MAX_REASON_BYTES].decode("utf-8", errors="ignore")
    return {field: preview, f"{field}_retention": {"sha256": digest(raw), "utf8_bytes": len(raw),
            "preview_utf8_bytes": len(preview.encode("utf-8")), "truncated": len(raw) > MAX_REASON_BYTES}}


def read_object(path: Path, max_bytes: int = MAX_INPUT_BYTES) -> dict[str, Any]:
    value = json.loads(read_bounded(path, max_bytes))
    if not isinstance(value, dict):
        raise ValueError(f"Expected JSON object: {path.name}")
    return value


def valid_hash(value: Any) -> bool:
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def bounded_score(value: Any) -> bool:
    return type(value) in (int, float) and math.isfinite(value) and 0 <= value <= 1


def read_answer(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {"status": "missing", "sha256": None, "answer": "", "reason": "answer artifact missing"}
    raw = read_bounded(path)
    result: dict[str, Any] = {"status": "invalid_output", "sha256": digest(raw), "answer": ""}
    try:
        value = json.loads(raw)
        if not isinstance(value, dict) or not isinstance(value.get("answer"), str) or not value["answer"].strip():
            raise ValueError("answer must be a nonempty string")
        result.update(status="success", answer=value["answer"])
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
        result.update(reason_record(str(exc)))
    return result


def validate_grounding(judgment_path: Path | None, expected: dict[str, Any], answer_hash: str | None) -> dict[str, Any]:
    unknown: dict[str, Any] = {"status": "unknown", "grounded_success": None, "atomic_coverage": None}
    if not judgment_path or not judgment_path.is_file():
        return {**unknown, "reason": "independent judgment unavailable"}
    try:
        raw = read_bounded(judgment_path)
        value = json.loads(raw)
        if not isinstance(value, dict) or value.get("schema") != "gptgrep.grounding-judgment.v1":
            raise ValueError("judgment schema mismatch")
        if value.get("task_id") != expected.get("task_id"):
            raise ValueError("judgment task binding mismatch")
        for field, expected_value in (
            ("answer_sha256", answer_hash),
            ("corpus_sha256", expected.get("corpus_sha256")),
            ("rubric_sha256", expected.get("rubric_sha256")),
            ("reader_binding_sha256", expected.get("reader_binding_sha256")),
        ):
            if not valid_hash(expected_value) or value.get(field) != expected_value:
                raise ValueError(f"judgment {field} binding mismatch or unconfigured")
        if value.get("status") != "complete":
            return {**unknown, **reason_record(value.get("reason") or "independent judge did not complete"),
                    "judgment_sha256": digest(raw)}
        success = value.get("grounded_success")
        coverage = value.get("atomic_coverage")
        if type(success) not in (int, float) or success not in (0, 1) or not bounded_score(coverage):
            raise ValueError("completed judgment has invalid or missing metric values")
        return {"status": "complete", "grounded_success": int(success), "atomic_coverage": coverage,
                **reason_record(value.get("reason") or ""), "judgment_sha256": digest(raw)}
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
        return {**unknown, **reason_record(str(exc))}


def native_score(scorer_path: Path, gold_path: Path, task_id: str, answer: dict[str, Any]) -> dict[str, Any]:
    try:
        if digest(read_bounded(scorer_path)) != NATIVE_SCORER_SHA256:
            raise ValueError("native scorer SHA-256 mismatch")
        module_name = "gptgrep_xl_native_evaluator"
        spec = importlib.util.spec_from_file_location(module_name, scorer_path)
        if spec is None or spec.loader is None:
            raise ValueError("cannot load pinned native scorer")
        module = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = module
        spec.loader.exec_module(module)
        gold = module.load_gold_records([gold_path])
        if set(gold) != {task_id}:
            raise ValueError("verifier gold must contain exactly the enrolled task ID")
        prediction = {task_id: answer["answer"]} if answer["status"] != "missing" else {}
        statuses = {task_id: answer["status"]} if prediction else {}
        report = module.evaluate(gold, prediction, statuses, ignore_missing=False)
        if report["gold_count"] != 1 or report["evaluated_count"] != 1:
            raise ValueError("native denominator changed")
        metrics = {"native_accuracy": report["overall"]["accuracy"],
                   "native_token_f1": report["overall"]["token_f1"],
                   "native_anls": report["overall"]["anls"]}
        if not all(bounded_score(v) for v in metrics.values()):
            raise ValueError("invalid native metric output")
        return {"status": "complete", "metrics": metrics, "enrolled_count": 1,
                "evaluated_count": 1, "reader_status": answer["status"], "scorer_sha256": NATIVE_SCORER_SHA256}
    except Exception as exc:
        # Operational/scorer failure is not an observed zero-quality answer.
        return {"status": "unknown", "metrics": {}, "enrolled_count": 1,
                "evaluated_count": 0, **reason_record(f"{type(exc).__name__}: {exc}")}


def write_json(path: Path, value: Any) -> None:
    raw = (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")
    if len(raw) > MAX_JSON_OUTPUT_BYTES:
        raise ValueError("Verifier JSON output exceeds its byte bound")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(raw)


def validate_rewardkit_scores(metrics: dict[str, float], rewards: dict[str, float], details: dict[str, Any]) -> None:
    """Bind each raw criterion to its source and its four-place presentation."""
    if not isinstance(rewards, dict) or not isinstance(details, dict) or set(rewards) != set(metrics) or set(details) != set(metrics):
        raise ValueError("RewardKit changed dimension membership")
    for key, source in metrics.items():
        if key not in (*NATIVE_KEYS, *GROUNDING_KEYS) or not bounded_score(source):
            raise ValueError("Invalid validated reward dimension")
        detail = details[key]
        if not isinstance(detail, dict) or detail.get("kind") != "programmatic" or detail.get("error") is not None:
            raise ValueError("RewardKit dimension detail is missing or invalid")
        criteria = detail.get("criteria")
        if not isinstance(criteria, list) or len(criteria) != 1 or not isinstance(criteria[0], dict):
            raise ValueError("RewardKit must retain exactly one criterion per metric")
        criterion = criteria[0]
        if (criterion.get("name") != f"xl_metric:{key}" or criterion.get("error") is not None
                or not bounded_score(criterion.get("weight")) or criterion["weight"] != 1):
            raise ValueError("RewardKit criterion identity or weight changed")
        raw = criterion.get("raw")
        if not bounded_score(raw) or raw != source:
            raise ValueError("RewardKit raw metric differs from validated source")
        # The pinned RewardKit 0.2.3 rounds both aggregate scores and serialized
        # criterion values to four places; raw retains the original score.
        rounded = round(source, 4)
        for value in (rewards[key], detail.get("score"), criterion.get("value")):
            if not bounded_score(value) or value != rounded:
                raise ValueError("RewardKit metric differs from its specified rounding")


def run_rewardkit(output_dir: Path, report_path: Path) -> dict[str, float]:
    """Each metric is its own dimension directory; no cross-metric mean exists."""
    import rewardkit as rk

    # RewardKit discovers generated criteria from a different directory and may
    # replace sys.path. Register our controller-owned module by exact file, once
    # per source digest, without relying on the launcher's working directory.
    checks_path = Path(__file__).resolve().with_name("checks.py")
    module_name = "gptgrep_rewardkit_checks_" + digest(read_bounded(checks_path))[:16]
    if module_name not in sys.modules:
        spec = importlib.util.spec_from_file_location(module_name, checks_path)
        if spec is None or spec.loader is None:
            raise ValueError("Cannot load the controller's RewardKit criteria")
        checks = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = checks
        try:
            spec.loader.exec_module(checks)
        except Exception:
            sys.modules.pop(module_name, None)
            raise

    report = read_object(report_path, MAX_JSON_OUTPUT_BYTES)
    metrics = report.get("metrics")
    if (not isinstance(metrics, dict) or not metrics
            or any(key not in (*NATIVE_KEYS, *GROUNDING_KEYS) or not bounded_score(value)
                   for key, value in metrics.items())):
        raise ValueError("RewardKit requires bounded validated metric dimensions")
    # RewardKit 0.2.3 aggregates a flat root into one `reward`. Child
    # directories expose independent dimensions, even when only one exists.
    # Exclusive creation prevents stale dimensions or repeated retained-directory
    # growth. A rerun needs a separately admitted output directory.
    criteria = output_dir / "rewardkit-criteria"
    criteria.mkdir()
    # The directory is controller-owned generated code, never an agent artifact.
    for key in metrics:
        dimension = criteria / key
        dimension.mkdir()
        content = ("import rewardkit as rk\n"
                   f"rk.xl_metric({key!r}, {str(report_path)!r})\n")
        if len(content.encode("utf-8")) > MAX_CRITERION_SOURCE_BYTES:
            raise ValueError("Generated verifier criterion exceeds its byte bound")
        (dimension / "score.py").write_text(content, encoding="utf-8")
    rewards = rk.run(criteria, workspace=output_dir, output=output_dir / "reward.json")
    # The pinned programmatic route writes only these two files. Criteria return
    # bounded reason text; this readback checks the library's actual output too.
    persisted_rewards = read_object(output_dir / "reward.json", MAX_JSON_OUTPUT_BYTES)
    if persisted_rewards != rewards:
        raise ValueError("RewardKit persisted rewards differ from its result")
    validate_rewardkit_scores(metrics, rewards, read_object(output_dir / "reward-details.json", MAX_JSON_OUTPUT_BYTES))
    return rewards


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    here = Path(__file__).resolve().parent
    parser.add_argument("--answer", type=Path, default=Path("/app/answer.json"))
    parser.add_argument("--judgment", type=Path)
    parser.add_argument("--expected", type=Path, default=here / "expected.json")
    parser.add_argument("--gold", type=Path, default=here / "gold.jsonl")
    parser.add_argument("--native-scorer", type=Path, default=here / "upstream_evaluate.py")
    parser.add_argument("--output-dir", type=Path, default=Path("/logs/verifier"))
    parser.add_argument("--rewardkit", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    report_path = args.output_dir / "feedback.json"
    pending = {"schema": "gptgrep.xl-verifier-feedback.v1", "status": "unknown",
               "promotion_allowed": False, "metrics": {}, "reason": "verification not complete"}
    # A failed rerun must not leave a previous successful report looking current.
    write_json(report_path, pending)
    write_json(args.output_dir / "reward.json", {})
    try:
        expected_bytes = read_bounded(args.expected)
        expected = json.loads(expected_bytes)
        if not isinstance(expected, dict):
            raise ValueError("Expected JSON binding object")
        if expected.get("schema") != "gptgrep.verifier-binding.v2" or expected.get("execution_mode") != "live":
            raise ValueError("Controller post-reader binding unavailable")
        expected["reader_binding_sha256"] = digest(expected_bytes)
        if (type(expected.get("reader_valid_start")) is not bool
                or expected.get("reader_status") not in ("completed", "failed", "timeout", "setup_failed", "answer_invalid")
                or "answer_sha256" not in expected):
            raise ValueError("Incomplete reader terminal binding")
        if expected["reader_valid_start"] and (not expected.get("reader_thread_id") or not expected.get("reader_turn_id")):
            raise ValueError("Started reader lacks native thread/turn identity")
        if not expected["reader_valid_start"] and expected["reader_status"] != "setup_failed":
            raise ValueError("Unstarted reader has a scored terminal status")
        if expected.get("native_scorer_sha256", NATIVE_SCORER_SHA256) != NATIVE_SCORER_SHA256:
            raise ValueError("Expected native scorer differs from the pinned release")
        task_id = expected.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            raise ValueError("Missing enrolled task ID")
        answer = read_answer(args.answer)
        if expected["answer_sha256"] != answer["sha256"]:
            raise ValueError("Collected answer differs from controller's frozen artifact")
        reader_status = expected.get("reader_status")
        if reader_status is not None and reader_status not in ("success", "ok", "completed", "complete"):
            # A partial artifact from a timed-out/crashed process is still a
            # failed native attempt, even when it happens to contain valid JSON.
            answer["status"] = "reader_failed"
            answer["reason"] = f"controller reader status: {reader_status}"
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        error = reason_record(f"Verifier input error: {exc}")
        write_json(report_path, {**pending, **error})
        print(error["reason"], file=sys.stderr)
        return 2
    native = native_score(args.native_scorer, args.gold, task_id, answer)
    independent_grounding = validate_grounding(args.judgment, expected, answer["sha256"])
    grounding = independent_grounding
    completed_states = ("success", "ok", "completed", "complete")
    failed_states = ("timeout", "failed", "crashed", "crash", "answer_invalid", "operational_failure")
    if expected.get("reader_valid_start") is False or (reader_status is not None and reader_status not in (*completed_states, *failed_states)):
        # There was no valid episode to score. Infrastructure absence is unknown.
        native = {"status": "unknown", "metrics": {}, "reason": "No verified terminal reader episode"}
        grounding = {"status": "unknown", "grounded_success": None, "atomic_coverage": None,
                     "reason": "No verified terminal reader episode"}
    elif answer["status"] != "success":
        # A partial answer may be useful for diagnosis, but cannot rescue a
        # timed-out, crashed, missing-answer or invalid-answer reader episode.
        grounding = {"status": "complete", "grounded_success": 0, "atomic_coverage": 0,
                     "reason": f"Reader episode failed: {answer['status']}"}
    metrics = dict(native["metrics"])
    if grounding["status"] == "complete":
        metrics.update({key: grounding[key] for key in GROUNDING_KEYS})
    feedback: dict[str, Any] = {
        "schema": "gptgrep.xl-verifier-feedback.v1", "task_id": task_id,
        "answer_sha256": answer["sha256"], "corpus_sha256": expected.get("corpus_sha256"),
        "rubric_sha256": expected.get("rubric_sha256"), "reader_status": answer["status"],
        "reader_binding_sha256": expected["reader_binding_sha256"],
        "native": native, "grounding": grounding, "independent_grounding": independent_grounding, "metrics": metrics,
        "status": "complete" if native["status"] == grounding["status"] == "complete" else "unknown",
        "promotion_allowed": False, "rewardkit_status": "not_run",
    }
    write_json(report_path, feedback)
    write_json(args.output_dir / "native-metrics.json", native)
    write_json(args.output_dir / "reward.json", metrics)
    if args.rewardkit:
        try:
            run_rewardkit(args.output_dir, report_path)
            feedback["rewardkit_status"] = "complete"
        except Exception as exc:
            feedback["rewardkit_status"] = "unknown"
            feedback.update(reason_record(f"{type(exc).__name__}: {exc}", "rewardkit_error"))
            feedback["status"] = "unknown"
            # Preserve actual native observations; unknown criteria are absent.
            write_json(args.output_dir / "reward.json", metrics)
    feedback["promotion_allowed"] = (answer["status"] == "success" and feedback["status"] == "complete"
                                     and feedback["rewardkit_status"] == "complete")
    write_json(report_path, feedback)
    print(json.dumps({"task_id": task_id, "status": feedback["status"],
                      "native_status": native["status"], "grounding_status": grounding["status"],
                      "rewardkit_status": feedback["rewardkit_status"], "metrics": metrics,
                      "promotion_allowed": feedback["promotion_allowed"]}))
    return 2 if native["status"] == "unknown" or feedback["rewardkit_status"] == "unknown" else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(reason_record(f"Verifier input error: {exc}")["reason"], file=sys.stderr)
        raise SystemExit(2)
