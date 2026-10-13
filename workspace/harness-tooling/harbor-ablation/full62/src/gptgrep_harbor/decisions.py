"""Caller-selected, treatment-only Decisions context modes and their controls."""
from __future__ import annotations

import fcntl
from fractions import Fraction
import math
import os
from pathlib import Path
import stat
import time
import unicodedata

from .common import canonical, digest, integer, parse, pinned, put, require, sha
from .process import run
from .runtime import DECISIONS_LEDGER_DIR, native_environment, validate_asset_path

CONTROL_ROOT = "/opt/full62-private/controls"
FAMILY_ROOT = "/opt/full62-private/financial-family"
TRIAL_ROOT = "/opt/full62-private/trial"
TRIAL_BUDGET = TRIAL_ROOT + "/budget.json"
WRITER_LOCK = "/opt/full62-private/sole-writer.lock"
# invocation_bounded: this trial's own claim directory, mounted from the host
# outside the Harbor job directory.
CLAIM_ROOT = "/opt/full62-private/claim"
ATTACH_PATH_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN"
ATTACH_SHA_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN_SHA256"

# Exactly one Decisions context mode per runtime. Fields of every unselected
# mode stay null (as runtime.template.json ships them) or absent.
DECISIONS_MODES = ("existing_context", "preparation", "invocation_bounded")
MODE_FIELDS = {"existing_context": ("decisions_budget", "decisions_ledger_dir"),
               "preparation": ("decisions_preparation",),
               "invocation_bounded": ("decisions_cost_cap_usd", "decisions_prompt_price", "decisions_claims_dir")}
# Product limits mirrored from gptgrep-jev: a context admits at most
# MAX_SUBMISSIONS, and one serialized request is at most MAX_REQUEST_BYTES.
INVOCATION_MAX_SUBMISSIONS = 200
MAX_REQUEST_BYTES = 64 * 1024
# The product accepts a Decisions model selector of at most 256 bytes.
MAX_MODEL_BYTES = 256
# Sanity bound for every USD amount and rate; keeps each bound a finite JSON number.
MAX_USD = 10**9
BUDGET_CONTRACT_SCHEMA = "gptgrep.decisions-budget-contract.v1"
# Every Ask first runs the product's mandatory retrieval pass: one document-routing
# and at least one evidence-reranking submission, even with --document scope.
MIN_BOOTSTRAP_SUBMISSIONS = 2
PRICE_FIELDS = {"model", "usd_per_million_prompt_tokens", "usd_per_million_completion_tokens",
                "usd_per_request", "provenance"}


def selected_mode(runtime):
    """Resolve the one Decisions mode and reject any field of another mode."""
    mode = runtime.get("decisions_mode")
    if mode is None:
        # Original field-based selection; invocation_bounded is explicit only.
        mode = "preparation" if runtime.get("decisions_preparation") is not None else "existing_context"
    require(mode in DECISIONS_MODES, "decisions_mode")
    for other, fields in MODE_FIELDS.items():
        require(other == mode or all(runtime.get(field) is None for field in fields),
                "decisions_" + mode + "_mode_conflict")
    return mode


def shared_allowance(budget_pin, treatment_trials):
    """Existing-context preflight: the one shared ledger's allowance against the cohort's bootstraps.

    Every reservation in the shared ledger is permanent, and every treatment Ask
    spends at least two submissions on its mandatory retrieval pass before any
    model-driven search. Once the allowance is exhausted, each later treatment
    Ask fails at that pass and scores an operational zero, while the bare arm is
    unaffected. The ledger may already hold reservations; this adapter never
    reads it, so the record is an upper bound.
    """
    budget = parse(pinned(budget_pin, 16384))
    require(type(budget) is dict and budget.get("schema_version") == BUDGET_CONTRACT_SCHEMA and
            isinstance(budget.get("ledger_path"), str) and
            Path(budget["ledger_path"]).parent == Path(DECISIONS_LEDGER_DIR), "decisions_budget_contract")
    maximum = integer(budget.get("max_submissions"), 1, INVOCATION_MAX_SUBMISSIONS)
    integer(treatment_trials, 1)
    floor = treatment_trials * MIN_BOOTSTRAP_SUBMISSIONS
    record = {"mode": "existing_context", "treatment_trials": treatment_trials,
              "max_submissions_at_most": maximum, "min_bootstrap_submissions_per_ask": MIN_BOOTSTRAP_SUBMISSIONS,
              "bootstrap_submissions": floor, "submissions_after_bootstraps_at_most": max(0, maximum - floor),
              "reservations": "shared_and_permanent"}
    warning = {"code": ("decisions_shared_allowance_below_bootstrap_floor" if maximum < floor
                        else "decisions_shared_allowance_may_exhaust"),
               "max_submissions": maximum, "bootstrap_submissions": floor, "treatment_trials": treatment_trials}
    return record, [warning]


def decisions_model(value):
    """A Decisions model selector that the product accepts and argv carries as one value."""
    require(isinstance(value, str) and value == value.strip() and
            0 < len(value.encode("utf-8")) <= MAX_MODEL_BYTES and not value.startswith("-") and
            not any(unicodedata.category(c) == "Cc" for c in value), "decisions_model")
    return value


def decimal(value, code, *, zero=False):
    """The exact decimal value of a finite JSON number in (0, MAX_USD], or [0, MAX_USD] with zero."""
    require(type(value) in (int, float) and (type(value) is int or math.isfinite(value)), code)
    # repr() of a float is its shortest round-trip decimal; integers are exact.
    exact = Fraction(value) if type(value) is int else Fraction(repr(value))
    require((exact >= 0 if zero else exact > 0) and exact <= MAX_USD, code)
    return exact


def invocation_cost_bound(runtime, treatment_trials):
    """Admission bound on the cohort's Decisions spend in invocation_bounded mode.

    A create-new claim lets each treatment trial launch at most one Ask, and an
    Ask without a budget file uses the product's own single-invocation context
    of at most 200 submissions of at most 64 KiB each. One submission's bound
    prices every request byte as one prompt token at the pinned rate and adds
    the pinned per-request fee. The pinned completion rate must be zero: the
    product sets no output-token limit per submission, so a priced completion
    has no bound. The cap is checked here, before any task exists; nothing
    stops spending at runtime.
    """
    cap = decimal(runtime.get("decisions_cost_cap_usd"), "decisions_cost_cap_usd")
    price = runtime.get("decisions_prompt_price")
    require(type(price) is dict and set(price) == PRICE_FIELDS and
            isinstance(price["model"], str) and price["model"] == decisions_model(runtime.get("jev_model")),
            "decisions_prompt_price")
    prompt_rate = decimal(price["usd_per_million_prompt_tokens"], "decisions_prompt_price")
    request_fee = decimal(price["usd_per_request"], "decisions_prompt_price", zero=True)
    completion_rate = decimal(price["usd_per_million_completion_tokens"], "decisions_prompt_price", zero=True)
    require(completion_rate == 0, "decisions_completion_price_unbounded")
    # The price evidence is a host-side pin; it never enters a task context.
    pinned(price["provenance"])
    integer(treatment_trials, 1)
    per_submission = MAX_REQUEST_BYTES * prompt_rate / 1000000 + request_fee
    per_trial = INVOCATION_MAX_SUBMISSIONS * per_submission
    bound = treatment_trials * per_trial
    require(bound <= cap, "decisions_cost_cap_preflight")
    return {"mode": "invocation_bounded", "treatment_trials": treatment_trials, "asks_per_trial": 1,
            "max_submissions_per_trial": INVOCATION_MAX_SUBMISSIONS, "max_request_bytes": MAX_REQUEST_BYTES,
            "token_bound": "one_prompt_token_per_request_byte", "price": price,
            "submission_bound_usd": float(per_submission), "trial_bound_usd": float(per_trial),
            "cohort_bound_usd": float(bound), "cost_cap_usd": runtime["decisions_cost_cap_usd"],
            "bound_covers": ["prompt_tokens_at_one_per_request_byte", "per_request_fee",
                             "completion_tokens_at_pinned_zero_rate"],
            "excluded": ["provider_prompt_tokens_beyond_one_per_request_byte",
                         "price_components_absent_from_the_pinned_price"],
            "limits_enforced_by": {"asks_per_trial": "adapter_create_new_claim",
                                   "submissions_per_ask": "product_single_invocation_context",
                                   "request_bytes": "product"},
            "cap_runtime_enforced": False}


def claim_invocation(binding, deadline_unix_ms):
    """Claim this trial's one Ask before it starts.

    The claim directory is a host mount outside the Harbor job directory.
    Create-new means a re-executed trial (for example in a resumed job) fails
    closed before any Decisions submission, so the per-trial bound holds across
    every execution of the materialized cohort.
    """
    root = Path(CLAIM_ROOT)
    require(root.is_dir() and root.resolve() == root, "decisions_claim_mount")
    claim = {"schema": "gptgrep.harbor.decisions-invocation-claim.v1",
             "binding_sha256": sha(canonical(binding)), "deadline_unix_ms": deadline_unix_ms}
    put(root / "claim.json", claim)
    data = canonical(claim) + b"\n"
    return {"path": str(root / "claim.json"), "bytes": len(data), "sha256": sha(data)}


def record_invocation(binding, stdout, attempt):
    """Keep the Ask's Decisions cost receipt beside its claim for host-side reconciliation."""
    receipt = {"schema": "gptgrep.harbor.decisions-invocation-receipt.v1",
               "binding_sha256": sha(canonical(binding)), "product_report_sha256": sha(stdout),
               "return_code": attempt["return_code"], "failure": attempt["failure"],
               "decisions_cost": attempt.get("decisions_cost")}
    path = Path(CLAIM_ROOT) / "decisions-cost.json"
    put(path, receipt)
    data = canonical(receipt) + b"\n"
    return {"path": str(path), "bytes": len(data), "sha256": sha(data)}


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
