"""Answer artifact admission; judging remains unavailable in this offline candidate."""
import argparse
import json
import os
import stat
from pathlib import Path

MAX_RESPONSE_CHARS = 12000


def bounded_regular(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 64 * 1024:
            raise ValueError("artifact_type_or_size")
        with os.fdopen(os.dup(descriptor), "rb") as handle:
            data = handle.read(64 * 1024 + 1)
        if len(data) > 64 * 1024:
            raise ValueError("artifact_size")
        return data
    finally:
        os.close(descriptor)


def equivalence_reward(verdict):
    # Original answer equivalence only; there are no citation or preprocessing bonuses.
    if verdict == "correct":
        return 1.0
    if verdict == "incorrect":
        return 0.0
    if verdict == "unavailable":
        return None
    raise ValueError("unknown_equivalence_verdict")


def admit_answer(value):
    if not isinstance(value, dict) or set(value) != {"answer"} or not isinstance(value["answer"], str):
        raise ValueError("answer_schema")
    if not value["answer"].strip() or len(value["answer"]) > MAX_RESPONSE_CHARS:
        raise ValueError("answer_size_or_empty")
    return value["answer"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--answer", required=True)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    reference_path, answer_path = Path(args.reference), Path(args.answer)
    reference = json.loads(bounded_regular(reference_path))
    if reference.get("judge_status") != "unavailable" or reference.get("reward_rule") != "original_answer_equivalence_only":
        raise ValueError("judge_qualification_required")
    answer_status = "unavailable"
    try:
        admit_answer(json.loads(bounded_regular(answer_path)))
        answer_status = "admitted"
    except FileNotFoundError:
        pass
    except (ValueError, TypeError, OSError):
        answer_status = "invalid"
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("x") as handle:
        json.dump({"verification_status": "unavailable", "reason": "qualified_original_equivalence_judge_required",
                   "answer_artifact_status": answer_status, "reward_emitted": False}, handle, sort_keys=True)
    # Do not emit reward.txt/reward.json or turn unavailable into zero reward.
    raise SystemExit(75)


if __name__ == "__main__":
    main()
