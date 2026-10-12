"""Freeze a complete paired order using Harbor's existing explicit-task config."""
from __future__ import annotations

from pathlib import Path
import random

from .common import ARMS, canonical, integer, parse, pinned, require, sha


def paired_order(rows, seed):
    integer(seed)
    require(isinstance(rows, list) and len(rows) == 124, "trial_order_full124")
    pairs, names = {}, set()
    for row in rows:
        binding, name = row["binding"], row["task_name"]
        pair, arm = binding["pair_id"], binding["arm"]
        require(arm in ARMS and name not in names, "trial_order_unique_member")
        names.add(name)
        require(arm not in pairs.setdefault(pair, {}), "trial_order_unique_arm")
        pairs[pair][arm] = name
    require(len(pairs) == 62 and all(set(pair) == set(ARMS) for pair in pairs.values()),
            "trial_order_complete_pairs")
    blocks = [[pair[arm] for arm in ARMS] for pair in pairs.values()]
    # Independent deterministic streams keep arm selection independent of the
    # number of draws consumed by the pair shuffle. The explicit order is final.
    pair_rng = random.Random(sha(canonical(["pair-order", seed])))
    arm_rng = random.Random(sha(canonical(["arm-order", seed])))
    pair_rng.shuffle(blocks)
    for block in blocks:
        if arm_rng.getrandbits(1):
            block.reverse()
    order = [name for block in blocks for name in block]
    return {"schema": "gptgrep.harbor.paired-trial-order.v1", "seed": seed,
            "algorithm": "python-random-pair-blocks-v1", "task_names": order,
            "sha256": sha(canonical(order))}


def check_job_order(export, runtime, dataset):
    """Read only the pinned job control before any local image build is allowed."""
    dataset = Path(dataset)
    expected = paired_order(export["tasks"], runtime["trial_order_seed"])
    require(export.get("trial_order") == expected, "trial_order_frozen_binding")
    pin = export["job"]
    require(pin["path"] == str(dataset.parent / (dataset.name + ".job.json")), "trial_order_job_path")
    job = parse(pinned(pin))
    tasks = [{"path": str(dataset / name), "source": dataset.name} for name in expected["task_names"]]
    require(job.get("datasets") == [] and job.get("tasks") == tasks
            and job.get("n_attempts") == 1 and job.get("n_concurrent_trials") == 1
            and job.get("retry", {}).get("max_retries") == 0, "trial_order_job_join")
    return job
