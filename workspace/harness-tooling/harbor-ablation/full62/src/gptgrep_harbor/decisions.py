"""Caller-selected, treatment-only current Decisions preparation controls."""
from __future__ import annotations

import fcntl
import os
from pathlib import Path
import stat
import time

from .common import canonical, digest, integer, parse, pinned, put, require, sha
from .process import run
from .runtime import native_environment, validate_asset_path

CONTROL_ROOT = "/opt/full62-private/controls"
FAMILY_ROOT = "/opt/full62-private/financial-family"
TRIAL_ROOT = "/opt/full62-private/trial"
TRIAL_BUDGET = TRIAL_ROOT + "/budget.json"
WRITER_LOCK = "/opt/full62-private/sole-writer.lock"
ATTACH_PATH_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN"
ATTACH_SHA_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN_SHA256"


def directory(value):
    require(isinstance(value, str) and value.startswith("/") and value != "/" and
            Path(value).as_posix() == value and ".." not in Path(value).parts and
            not any(c in value for c in ("$", "\n", "\r", "\x00")), "decisions_directory")
    validate_asset_path(value)
    path = Path(value)
    require(path.is_dir() and path.resolve() == path, "decisions_physical_directory")
    return path


def control(value):
    require(type(value) is dict and set(value) == {"schema", "seed", "controls_directory",
            "financial_family_directory", "trial_state_directory", "max_concurrent_batches", "sole_writer"}
            and value["schema"] == "gptgrep.harbor.decisions-preparation.v1"
            and type(value["max_concurrent_batches"]) is int
            and value["max_concurrent_batches"] == 1 and value["sole_writer"] is True,
            "decisions_preparation_control")
    roots = [directory(value[key]) for key in
             ("controls_directory", "financial_family_directory", "trial_state_directory")]
    require(all(not a.is_relative_to(b) and not b.is_relative_to(a)
                for i, a in enumerate(roots) for b in roots[i + 1:]), "decisions_distinct_directories")
    seed_pin = value["seed"]
    require(seed_pin["path"] == str(roots[0] / "seed.json"), "decisions_seed_path")
    seed = parse(pinned(seed_pin))
    require(set(seed) == {"schema", "financial_family", "budget", "budget_sha256", "authority", "review",
                         "source_files", "max_sdk_submissions", "cumulative_call_limit", "sole_writer"}
            and seed["schema"] == "gptgrep.current_decisions.preparation_seed.v1"
            and seed["financial_family"] == FAMILY_ROOT and seed["sole_writer"] is True
            and seed["cumulative_call_limit"] == 2000, "decisions_seed_contract")
    integer(seed["max_sdk_submissions"], 1, 200)
    digest(seed["budget_sha256"])
    require(isinstance(seed["source_files"], list) and 0 < len(seed["source_files"]) <= 96,
            "decisions_source_pins")
    for pin in [seed["authority"], seed["review"], *seed["source_files"]]:
        require(type(pin) is dict and set(pin) == {"path", "bytes", "sha256"}
                and Path(pin["path"]).is_relative_to(CONTROL_ROOT)
                and ".." not in Path(pin["path"]).parts, "decisions_control_source_path")
        validate_asset_path(pin["path"])
        integer(pin["bytes"], 0, 8 * 1024 * 1024)
        digest(pin["sha256"])
    return value


def acquire_writer():
    """One active paid reader in this selected cohort; other writers need exclusion by the owner."""
    path = Path(WRITER_LOCK)
    require(path.resolve() == path, "decisions_writer_physical_path")
    descriptor = os.open(path, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        info = os.fstat(descriptor)
        require(stat.S_ISREG(info.st_mode) and info.st_size == 0, "decisions_writer_lock")
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def prepare(config, binding, question, argv, *, started, deadline_unix_ms, environ):
    selected = config["decisions_preparation"]
    require(binding["arm"] == "gptgrep" and config["reader_mode"] == "rich_multimodal"
            and selected["max_concurrent_batches"] == 1, "decisions_treatment_preparation")
    seed_pin = selected["seed"]
    require(seed_pin["path"] == CONTROL_ROOT + "/seed.json", "decisions_seed_mount")
    pinned(seed_pin)
    require(Path(TRIAL_ROOT).is_dir() and Path(TRIAL_ROOT).resolve() == Path(TRIAL_ROOT),
            "decisions_trial_mount")
    binding_sha256 = sha(canonical(binding))
    query_sha256 = sha(question.encode())
    require(query_sha256 == binding["question_sha256"], "decisions_question_binding")
    trial_input = {"schema": "gptgrep.current_decisions.trial_input.v1", "seed": seed_pin,
        "binding_sha256": binding_sha256, "query_sha256": query_sha256,
        "run_id": selected["run_id"], "phase_id": "current-ask",
        "deadline_unix_ms": deadline_unix_ms, "trial_directory": TRIAL_ROOT}
    input_path = TRIAL_ROOT + "/input.json"
    payload = canonical(trial_input) + b"\n"
    # create-new binds a failed preparation to this trial; retries cannot replace it.
    put(input_path, payload)
    remaining = config["deadline_seconds"] - (time.monotonic() - started)
    require(remaining > 0, "reader_deadline")
    result = run([argv[0], "--harbor-prepare-input", input_path,
                  "--harbor-prepare-input-sha256", sha(payload), *argv[1:]],
                 cwd="/data/corpus", timeout=remaining, env=native_environment(environ))
    put(TRIAL_ROOT + "/preparer-process.json", {"input_sha256": sha(payload),
        "elapsed_ms": result["elapsed_ms"], "return_code": result["return_code"],
        "failure": result["failure"], "stdout_sha256": sha(result["stdout"]),
        "stderr_sha256": sha(result["stderr"]), "models_invoked": 0})
    require(result["failure"] is None and result["return_code"] == 0, "decisions_preparation_process")
    value = parse(result["stdout"])
    require(value.get("schema") == "gptgrep.current_decisions.trial_prepared.v1"
            and value.get("binding_sha256") == binding_sha256 and value.get("query_sha256") == query_sha256
            and value.get("run_id") == selected["run_id"] and value.get("deadline_unix_ms") == deadline_unix_ms
            and value.get("models_invoked") == 0 and value.get("financial_reservations_created") == 0
            and value.get("retry") == "forbidden", "decisions_preparation_binding")
    integer(value["max_sdk_submissions"], 1, 200)
    integer(value["max_requests"], 1, value["max_sdk_submissions"])
    require(value["budget"]["path"] == TRIAL_BUDGET and value["plan"]["path"] == TRIAL_ROOT + "/plan.json"
            and value["receipt_directory"] == TRIAL_ROOT + "/receipts", "decisions_preparation_output_paths")
    budget = parse(pinned(value["budget"], 16384))
    plan = parse(pinned(value["plan"]))
    require(budget["schema_version"] == "gptgrep.decisions-budget-contract.v1"
            and budget["run_id"] == selected["run_id"] and budget["deadline_unix_ms"] == deadline_unix_ms
            and budget["max_submissions"] == value["max_sdk_submissions"]
            and budget["plan_digest"] == sha(payload)
            and budget["ledger_path"] == TRIAL_ROOT + "/ledger/decisions.json"
            and plan["schema"] == "gptgrep.current_decisions.attachment.v1"
            and plan["run_id"] == selected["run_id"] and plan["query_sha256"] == query_sha256
            and plan["financial_family"] == FAMILY_ROOT and plan["max_requests"] == value["max_requests"]
            and plan["receipt_directory"] == value["receipt_directory"]
            and plan["expected_sdk_snapshot"]["deadline_unix_ms"] == deadline_unix_ms,
            "decisions_exported_control_join")
    require(config["deadline_seconds"] - (time.monotonic() - started) > 0, "reader_deadline")
    # These are the only additional variables after the existing sanitizer.
    env = native_environment(environ, product=True)
    env.update({ATTACH_PATH_ENV: value["plan"]["path"], ATTACH_SHA_ENV: value["plan"]["sha256"]})
    return env, {"schema": value["schema"], "budget": value["budget"], "plan": value["plan"],
        "elapsed_ms": result["elapsed_ms"], "deadline_unix_ms": deadline_unix_ms,
        "max_requests": value["max_requests"], "original_clock_anchor_sha256": value["original_clock_anchor_sha256"],
        "stdout_sha256": sha(result["stdout"]), "stderr_sha256": sha(result["stderr"]),
        "cohort_writer_lock_held": True, "models_invoked": 0}
