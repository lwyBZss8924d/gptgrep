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

from .common import canonical, digest, field_at, integer, parse, pinned, put, read, require, sha
from .process import run
from .runtime import DECISIONS_LEDGER_DIR, native_environment, validate_asset_path

CONTROL_ROOT = "/opt/full62-private/controls"
FAMILY_ROOT = "/opt/full62-private/financial-family"
TRIAL_ROOT = "/opt/full62-private/trial"
TRIAL_BUDGET = TRIAL_ROOT + "/budget.json"
WRITER_LOCK = "/opt/full62-private/sole-writer.lock"
# invocation_bounded: this trial's own claim directory, mounted from the host
# outside the Harbor job directory in both arms, so a re-executed trial of
# either arm fails closed on its claim. A treatment directory also holds the
# trial's own Decisions
# ledger, lock and control file: the product creates them right after the claim
# with the per-trial submission limit, and the Ask joins them, so the product
# itself refuses any submission beyond that limit.
CLAIM_ROOT = "/opt/full62-private/claim"
TRIAL_LEDGER_NAME = "decisions-ledger.json"
TRIAL_CONTROL_NAME = "decisions-budget.json"
ATTACH_PATH_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN"
ATTACH_SHA_ENV = "GPTGREP_DECISIONS_ATTACH_PLAN_SHA256"

# Exactly one Decisions context mode per runtime. Fields of every unselected
# mode stay null (as runtime.template.json ships them) or absent.
DECISIONS_MODES = ("existing_context", "preparation", "invocation_bounded")
MODE_FIELDS = {"existing_context": ("decisions_budget", "decisions_ledger_dir"),
               "preparation": ("decisions_preparation",),
               "invocation_bounded": ("decisions_cost_cap_usd", "decisions_prompt_price", "decisions_claims_dir",
                                      "decisions_max_submissions_per_trial")}
# Product limits MIRRORED from gptgrep-jev for the host-side admission bound,
# where the Linux product cannot run: a Decisions context admits at most
# MAX_SUBMISSIONS, which is also the range limit of `decisions-budget init
# --max-submissions`, and one serialized text request is at most
# MAX_REQUEST_BYTES. Before its claim, every invocation_bounded treatment trial
# checks both against the pinned product's own `--schema` report, so a product
# change that would invalidate the admitted bound stops the trial before any
# Decisions submission.
INVOCATION_MAX_SUBMISSIONS = 200
MAX_REQUEST_BYTES = 64 * 1024
PRODUCT_LIMITS = {
    "max_submissions": (("commands", "decisions-budget", "init_options", "max-submissions", "maximum"),
                        INVOCATION_MAX_SUBMISSIONS),
    "max_request_bytes": (("decisions", "limits", "text_envelope_bytes"), MAX_REQUEST_BYTES)}
# The product accepts a Decisions model selector of at most 256 bytes.
MAX_MODEL_BYTES = 256
# Sanity bound for every USD amount and rate; keeps each bound a finite JSON number.
MAX_USD = 10**9
# Plausible prices. A per-token price given where a per-million price is
# expected (0.0000001 for USD 0.10 per million tokens) falls far below the
# floor; the ceilings catch other unit slips. A real price outside these bounds
# needs a reviewed source change.
MIN_USD_PER_MILLION_TOKENS = Fraction(1, 1000)
MAX_USD_PER_MILLION_TOKENS = 1000
MAX_USD_PER_REQUEST = 1
BUDGET_CONTRACT_SCHEMA = "gptgrep.decisions-budget-contract.v1"
# Every Ask first runs the product's mandatory retrieval pass: one document-routing
# and at least one evidence-reranking submission, even with --document scope.
MIN_BOOTSTRAP_SUBMISSIONS = 2
PRICE_FIELDS = {"model", "usd_per_million_prompt_tokens", "usd_per_million_completion_tokens",
                "usd_per_request", "provenance"}
TRIAL_BUDGET_SCHEMA = "gptgrep.harbor.decisions-trial-budget.v1"
# The component that enforces the per-trial submission limit at runtime.
TRIAL_LIMIT_ENFORCER = "product_decisions_budget_ledger"
BUDGET_INIT_SCHEMA = "gptgrep.decisions-budget-init.v1"
BUDGET_STATUS_SCHEMA = "gptgrep.decisions-budget-status.v1"
RESERVATION_COUNTS = ("total", "reserved", "in_flight", "unknown", "terminal")
# Product ledger commands are local file operations. Harbor's agent timeout
# leaves ten seconds beyond the query deadline for the reader's own bookkeeping.
BUDGET_COMMAND_SECONDS = 5
BOUND_FORMULA = ("treatment_trials * max_submissions_per_trial * "
                 "(max_request_bytes * usd_per_million_prompt_tokens / 1000000 + usd_per_request)")


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


def trial_submission_limit(value):
    """The per-trial Decisions submission limit: an explicit integer in 1..200, never defaulted."""
    require(type(value) is int and 1 <= value <= INVOCATION_MAX_SUBMISSIONS, "decisions_max_submissions_per_trial")
    return value


def trial_paths():
    """This trial's claim, Decisions ledger and control file in its own claim directory."""
    root = Path(CLAIM_ROOT)
    return root / "claim.json", root / TRIAL_LEDGER_NAME, root / TRIAL_CONTROL_NAME


# Options of the trial ledger's `decisions-budget init` that take a value per trial.
TRIAL_BUDGET_INIT_PER_TRIAL = ("--run-id", "--deadline-seconds")


def trial_budget_init_options(limit):
    """The fixed `decisions-budget init` options that create a trial's own ledger.

    The frozen treatment profile declares them, and every invocation_bounded
    treatment trial passes them with its per-trial --run-id and --deadline-seconds.
    """
    claim, ledger, control = trial_paths()
    return ["--plan-file", str(claim), "--max-submissions", str(trial_submission_limit(limit)),
            "--ledger", str(ledger), "--output", str(control)]


def invocation_cost_bound(runtime, treatment_trials):
    """Admission bound on the cohort's Decisions spend in invocation_bounded mode.

    A create-new claim lets each treatment trial launch at most one Ask, and
    that Ask joins the trial's own product ledger, which admits at most
    decisions_max_submissions_per_trial submissions of at most 64 KiB each. One
    submission's bound prices every request byte as one prompt token at the
    pinned rate and adds the pinned per-request fee. The pinned completion rate
    must be zero: the product sets no output-token limit per submission, so a
    priced completion has no bound. The USD cap is checked here, before any
    task exists; at runtime the product enforces the per-trial submission limit.
    """
    limit = trial_submission_limit(runtime.get("decisions_max_submissions_per_trial"))
    cap = decimal(runtime.get("decisions_cost_cap_usd"), "decisions_cost_cap_usd")
    price = runtime.get("decisions_prompt_price")
    require(type(price) is dict and set(price) == PRICE_FIELDS and
            isinstance(price["model"], str) and price["model"] == decisions_model(runtime.get("jev_model")),
            "decisions_prompt_price")
    prompt_rate = decimal(price["usd_per_million_prompt_tokens"], "decisions_prompt_price")
    request_fee = decimal(price["usd_per_request"], "decisions_prompt_price", zero=True)
    completion_rate = decimal(price["usd_per_million_completion_tokens"], "decisions_prompt_price", zero=True)
    require(completion_rate == 0, "decisions_completion_price_unbounded")
    require(MIN_USD_PER_MILLION_TOKENS <= prompt_rate <= MAX_USD_PER_MILLION_TOKENS and
            request_fee <= MAX_USD_PER_REQUEST, "decisions_price_unit_implausible")
    # The price evidence is a host-side pin; it never enters a task context.
    pinned(price["provenance"])
    integer(treatment_trials, 1)
    per_submission = MAX_REQUEST_BYTES * prompt_rate / 1000000 + request_fee
    per_trial = limit * per_submission
    bound = treatment_trials * per_trial
    require(bound <= cap, "decisions_cost_cap_preflight")
    return {"mode": "invocation_bounded", "treatment_trials": treatment_trials, "asks_per_trial": 1,
            "max_submissions_per_trial": limit, "max_request_bytes": MAX_REQUEST_BYTES,
            "bound_formula": BOUND_FORMULA,
            "product_limits": {**{name: value for name, (_, value) in PRODUCT_LIMITS.items()},
                               "source": "mirrored_from_gptgrep_jev",
                               "checked_against": "pinned_product_schema_in_every_treatment_trial_before_its_claim"},
            "price_plausibility": {"usd_per_million_prompt_tokens": [float(MIN_USD_PER_MILLION_TOKENS),
                                                                     MAX_USD_PER_MILLION_TOKENS],
                                   "usd_per_request_at_most": MAX_USD_PER_REQUEST},
            "token_bound": "one_prompt_token_per_request_byte", "price": price,
            "submission_bound_usd": float(per_submission), "trial_bound_usd": float(per_trial),
            "cohort_bound_usd": float(bound), "cost_cap_usd": runtime["decisions_cost_cap_usd"],
            "bound_covers": ["prompt_tokens_at_one_per_request_byte", "per_request_fee",
                             "completion_tokens_at_pinned_zero_rate"],
            "excluded": ["provider_prompt_tokens_beyond_one_per_request_byte",
                         "price_components_absent_from_the_pinned_price"],
            "limits_enforced_by": {"asks_per_trial": "adapter_create_new_claim",
                                   "submissions_per_trial": "product_trial_decisions_budget",
                                   "request_bytes": "product"},
            "limit_binding": "refused_submission_fails_the_ask_as_operational_failure_no_retry",
            "submission_limit_runtime_enforced": True, "cap_runtime_enforced": False}


def product_limits(contract):
    """Compare the mirrored product limits with the pinned product's own `--schema` report."""
    reported = {name: field_at(contract, path) for name, (path, _) in PRODUCT_LIMITS.items()}
    mirrored = {name: value for name, (_, value) in PRODUCT_LIMITS.items()}
    return {"schema": "gptgrep.harbor.decisions-product-limits.v1", "source": "product_schema",
            "mirrored": mirrored,
            "product_reported": {name: value if type(value) is int else None for name, value in reported.items()},
            "match": all(type(reported[name]) is int and reported[name] == value for name, value in mirrored.items())}


def claim_invocation(binding, deadline_unix_ms):
    """Claim this trial's one reader process before it starts, in either arm.

    The claim directory is a host mount outside the Harbor job directory.
    Create-new means a re-executed trial (for example in a resumed job) fails
    closed before its reader process, and for the treatment before any
    Decisions submission, so no pair silently mixes executions and the
    per-trial bound holds across every execution of the materialized cohort.
    """
    root = Path(CLAIM_ROOT)
    require(root.is_dir() and root.resolve() == root, "decisions_claim_mount")
    claim = {"schema": "gptgrep.harbor.trial-claim.v1",
             "binding_sha256": sha(canonical(binding)), "deadline_unix_ms": deadline_unix_ms}
    try:
        put(root / "claim.json", claim)
    except FileExistsError:
        raise ValueError("trial_already_claimed") from None
    data = canonical(claim) + b"\n"
    return {"path": str(root / "claim.json"), "bytes": len(data), "sha256": sha(data)}


def init_trial_budget(config, binding, claim, deadline_unix_ms, remaining, environ):
    """Create this trial's own Decisions ledger through the product, right after its claim.

    `decisions-budget init` runs offline and refuses any existing ledger, lock or
    control path, so like the claim it is create-new. Its plan file is the claim,
    which binds the ledger to this trial, and its deadline is the reader's
    original query deadline. The Ask joins it with --decisions-budget, and the
    product refuses every submission beyond the limit before sending it. Any
    failure stops the trial before its Ask, with no Decisions submission.
    """
    try:
        limit = trial_submission_limit(config.get("decisions_max_submissions_per_trial"))
        _, ledger, control = trial_paths()
        run_id = "gptgrep-full62-trial-" + sha(canonical(binding))[:32]
        seconds = math.ceil((deadline_unix_ms - time.time_ns() // 1000000) / 1000)
        require(0 < seconds <= 86400 and remaining > 0, "decisions_trial_budget_unavailable")
        # The per-trial options (TRIAL_BUDGET_INIT_PER_TRIAL), then the declared fixed ones.
        result = run([config["gptgrep_binary"], "decisions-budget", "init", "--run-id", run_id,
                      "--deadline-seconds", str(seconds), *trial_budget_init_options(limit)],
                     cwd="/data/corpus", timeout=min(BUDGET_COMMAND_SECONDS, remaining),
                     env=native_environment(environ))
        require(result["failure"] is None and result["return_code"] == 0, "decisions_trial_budget_unavailable")
        report = parse(result["stdout"])
        data = read(str(control), 16384)
        budget = parse(data)
        require(type(report) is dict and type(budget) is dict and
                report.get("schema_version") == BUDGET_INIT_SCHEMA and report.get("status") == "initialized" and
                report.get("joins_existing") is False and report.get("control_path") == str(control) and
                report.get("control_sha256") == sha(data) and
                budget.get("schema_version") == BUDGET_CONTRACT_SCHEMA and budget.get("run_id") == run_id and
                budget.get("plan_digest") == claim["sha256"] and budget.get("max_submissions") == limit and
                budget.get("ledger_path") == str(ledger) and type(budget.get("deadline_unix_ms")) is int and
                all(report.get(key) == budget[key] for key in
                    ("run_id", "plan_digest", "deadline_unix_ms", "max_submissions", "ledger_path")),
                "decisions_trial_budget_unavailable")
    except Exception:
        # Redacted code only: product output is never copied into the record.
        raise ValueError("decisions_trial_budget_unavailable") from None
    return {"schema": TRIAL_BUDGET_SCHEMA, "max_submissions": limit,
            "enforced_by": TRIAL_LIMIT_ENFORCER, "run_id": run_id, "plan_sha256": claim["sha256"],
            "deadline_unix_ms": budget["deadline_unix_ms"], "ledger_path": str(ledger),
            "control": {"path": str(control), "bytes": len(data), "sha256": sha(data)},
            "init_elapsed_ms": result["elapsed_ms"], "status_after_ask": None,
            "remaining_submissions": None, "limit_reached": None, "limit_bound": None}


def trial_budget_status(config, record):
    """Read this trial's ledger through the product's read-only status after the Ask.

    Evidence only: it never changes the Ask's outcome, and any failure leaves the
    observation null (unknown). Status never creates, locks, joins or writes.
    """
    _, _, control = trial_paths()
    try:
        result = run([config["gptgrep_binary"], "decisions-budget", "status", "--control", str(control), "--json"],
                     cwd="/data/corpus", timeout=BUDGET_COMMAND_SECONDS, env=native_environment(os.environ))
        require(result["failure"] is None and result["return_code"] == 0, "status")
        value = parse(result["stdout"])
        counts = value.get("reservations") if type(value) is dict else None
        require(type(value) is dict and value.get("schema_version") == BUDGET_STATUS_SCHEMA and
                value.get("run_id") == record["run_id"] and
                value.get("max_submissions") == record["max_submissions"] and type(counts) is dict and
                all(type(counts.get(key)) is int and counts[key] >= 0 for key in RESERVATION_COUNTS) and
                type(value.get("remaining_submissions")) is int and
                value["remaining_submissions"] == record["max_submissions"] - counts["total"] >= 0, "status")
    except Exception:
        return None
    return {"remaining_submissions": value["remaining_submissions"],
            "reservations": {key: counts[key] for key in RESERVATION_COUNTS},
            "send_started": value.get("send_started") if type(value.get("send_started")) is int else None,
            "submission_observations": (value.get("submission_observations")
                                        if type(value.get("submission_observations")) is int else None),
            "status_sha256": sha(result["stdout"])}


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
