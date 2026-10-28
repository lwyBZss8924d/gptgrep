"""RewardKit criteria over controller-validated native and EvalsAgent feedback."""

import json
import hashlib
import os
import stat
from pathlib import Path

from rewardkit import criterion

MAX_FEEDBACK_BYTES = 1024 * 1024
MAX_REASON_PREVIEW_BYTES = 2048


def load_feedback(path: Path) -> dict:
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_FEEDBACK_BYTES:
        raise ValueError("Expected bounded regular verifier feedback")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        current = os.fstat(stream.fileno())
        if (current.st_dev, current.st_ino, current.st_size) != (before.st_dev, before.st_ino, before.st_size):
            raise ValueError("Verifier feedback changed while opening")
        raw = stream.read(MAX_FEEDBACK_BYTES + 1)
    if len(raw) != current.st_size or len(raw) > MAX_FEEDBACK_BYTES:
        raise ValueError("Verifier feedback changed or exceeded its byte bound")
    feedback = json.loads(raw)
    if not isinstance(feedback, dict) or feedback.get("schema") != "gptgrep.xl-verifier-feedback.v1":
        raise ValueError("Unexpected verifier feedback schema")
    return feedback


def grounding_reason(grounding: dict) -> str:
    reason = grounding.get("reason", "")
    if not isinstance(reason, str):
        raise ValueError("Grounding reason must be a string")
    raw = reason.encode("utf-8")
    retention = grounding.get("reason_retention", {})
    original_hash = retention.get("sha256", hashlib.sha256(raw).hexdigest())
    judgment_hash = grounding.get("judgment_sha256", "unavailable")
    for value in (original_hash, judgment_hash):
        if value != "unavailable" and (not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdef" for c in value)):
            raise ValueError("Malformed retained reason or judgment hash")
    preview = raw[:MAX_REASON_PREVIEW_BYTES].decode("utf-8", errors="ignore")
    # Complete text remains in the source-pinned judgment; RewardKit need not
    # duplicate it in each grounding dimension's serialized detail.
    return f"Judgment SHA-256: {judgment_hash}; reason SHA-256: {original_hash}; preview: {preview}"


@criterion(description="Validated XL feedback dimension {metric}")
def xl_metric(workspace: Path, metric: str, feedback_path: str) -> dict:
    del workspace
    feedback = load_feedback(Path(feedback_path))
    if metric in ("grounded_success", "atomic_coverage"):
        if feedback["grounding"]["status"] != "complete":
            raise ValueError("Unknown grounding cannot become a numeric reward")
        reason = grounding_reason(feedback["grounding"])
    elif metric in ("native_accuracy", "native_token_f1", "native_anls"):
        if feedback["native"]["status"] != "complete":
            raise ValueError("Unknown native score cannot become a numeric reward")
        reason = "Pinned upstream evaluator; enrolled task retained in denominator"
    else:
        raise ValueError("Unknown reward dimension")
    return {"score": feedback["metrics"][metric], "reasoning": reason}
