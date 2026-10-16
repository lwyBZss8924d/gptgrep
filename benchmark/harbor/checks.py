"""RewardKit criteria over controller-validated native and EvalsAgent feedback."""

import json
from pathlib import Path

from rewardkit import criterion


@criterion(description="Validated XL feedback dimension {metric}")
def xl_metric(workspace: Path, metric: str, feedback_path: str) -> dict:
    del workspace
    feedback = json.loads(Path(feedback_path).read_text(encoding="utf-8"))
    if feedback.get("schema") != "gptgrep.xl-verifier-feedback.v1":
        raise ValueError("Unexpected verifier feedback schema")
    if metric in ("grounded_success", "atomic_coverage"):
        if feedback["grounding"]["status"] != "complete":
            raise ValueError("Unknown grounding cannot become a numeric reward")
        reason = feedback["grounding"].get("reason", "")
    elif metric in ("native_accuracy", "native_token_f1", "native_anls"):
        if feedback["native"]["status"] != "complete":
            raise ValueError("Unknown native score cannot become a numeric reward")
        reason = "Pinned upstream evaluator; enrolled task retained in denominator"
    else:
        raise ValueError("Unknown reward dimension")
    return {"score": feedback["metrics"][metric], "reasoning": reason}
