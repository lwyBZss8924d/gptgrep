"""Real upstream lifecycle, with GPTgrep-owned durable decisions and no publication.

The bridge imports REEF normally and intentionally has no substitute classes.
Missing dependencies/import failures are startup failures, never qualification.
No provider, Harbor, engineer or reviewer is launched by this source scaffold.
"""

from __future__ import annotations

import hashlib
import importlib.metadata
import inspect
import json
import subprocess
from collections.abc import Mapping
from copy import deepcopy
from pathlib import Path
from typing import TYPE_CHECKING, Any

from reef.core.evaluation import CandidateEvaluationPlugin, EvaluationResult, SelectionDecision, UpdateCandidate
from reef.train.backend import CandidateBackend, PreparedStep
from reef.train.trainer import Trainer
from reef.train.types import TrainingBatch, TrainStepResult

if TYPE_CHECKING:
    from gptgrep_reef.recovery import StartupRecovery

REEF_REVISION = "f773b6a6e769b5eb3884c1058e7da80b0b6862d1"


def _json_equal(left: Any, right: Any) -> bool:
    """Compare finite JSON content without Python's bool/integer equivalence."""
    return json.dumps(left, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False) == json.dumps(
        right, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False,
    )


def installed_reef_identity() -> Mapping[str, str]:
    """Bind normal imports to the VCS-pinned installed distribution, not a local shadow."""
    distribution = importlib.metadata.distribution("reef-infra")
    origin = json.loads(distribution.read_text("direct_url.json") or "{}")
    observed = origin.get("vcs_info", {}).get("commit_id")
    module = Path(inspect.getfile(CandidateBackend)).resolve()
    expected_module = Path(distribution.locate_file("reef/train/backend.py")).resolve()
    if observed != REEF_REVISION or module != expected_module:
        raise RuntimeError("Imported REEF distribution does not match the pinned VCS revision/module origin")
    with module.open("rb") as stream:
        module_sha256 = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"expected_reef_revision": REEF_REVISION, "observed_reef_revision": observed,
            "candidate_backend_module_sha256": module_sha256}


class BunBridge:
    """Invoke only an explicitly pinned local executable and bridge module."""

    def __init__(self, *, bun: Mapping[str, str], cli: Mapping[str, str], ledger: str, timeout_seconds: int = 30):
        if not Path(ledger).is_absolute() or not isinstance(timeout_seconds, int) or not 1 <= timeout_seconds <= 300:
            raise ValueError("Absolute ledger and finite bridge timeout required")
        self.bun, self.cli, self.ledger, self.timeout_seconds = dict(bun), dict(cli), ledger, timeout_seconds
        self._verify(self.bun)
        self._verify(self.cli)

    @staticmethod
    def _verify(pin: Mapping[str, str]) -> None:
        if set(pin) != {"path", "sha256"}:
            raise ValueError("Closed executable/source pin required")
        path = Path(pin["path"])
        if not path.is_absolute() or path.is_symlink() or not path.is_file() or path.resolve() != path:
            raise ValueError("Pinned executable/source must be a physical regular file")
        with path.open("rb") as stream:
            observed = hashlib.file_digest(stream, "sha256").hexdigest()
        if observed != pin["sha256"]:
            raise ValueError("Pinned executable/source bytes changed")

    def request(self, action: str, payload: Mapping[str, Any]) -> Mapping[str, Any]:
        self._verify(self.bun)
        self._verify(self.cli)
        process = subprocess.run(
            [self.bun["path"], self.cli["path"], action, "--ledger", self.ledger],
            input=json.dumps(dict(payload)), text=True, capture_output=True, check=True,
            timeout=self.timeout_seconds,
        )
        if len(process.stdout.encode()) > 4 * 1024 * 1024:
            raise ValueError("Bridge response exceeds bounded contract")
        response = json.loads(process.stdout)
        if set(response) != {"schema", "action", "stateVersion", "result", "resultSha256"} or response["schema"] != "gptgrep.opt-bridge-response.v1" or response["action"] != action:
            raise ValueError("Unexpected bridge response")
        return response


class GptgrepCandidateBackend(CandidateBackend):
    """One already reviewed candidate per REEF step; evidence stays in the ledger."""

    def __init__(self, *, bridge: BunBridge, candidate_id: str, holder: str, fencing_token: int):
        self.bridge, self.candidate_id = bridge, candidate_id
        self.fence = {"holder": holder, "token": fencing_token}
        self.reef_identity = installed_reef_identity()
        self._applied_decision_state: Mapping[str, Any] | None = None

    def _snapshot(self) -> Mapping[str, Any]:
        return self.bridge.request("snapshot", {})["result"]

    def _event_id(self, action: str) -> str:
        return f"{action}-{hashlib.sha256(self.candidate_id.encode()).hexdigest()}"

    def initial_state(self) -> Mapping[str, Any]:
        state = self._snapshot()
        if self.candidate_id not in state["contract"]["candidateIds"]:
            raise ValueError("Candidate is outside the frozen competition")
        return {"competition_id": state["contract"]["competitionId"], "candidate_id": self.candidate_id,
                "parent_revision": state["contract"]["parentRevision"], "decision": None}

    def experiment_config(self) -> Mapping[str, Any]:
        return {**self.reef_identity, "candidate_id": self.candidate_id,
                "evaluation_policy": "gptgrep_strict_typescript", "publication": "none",
                "paid_execution_qualified": False}

    def prepare_step(self, batch: TrainingBatch, state: Mapping[str, Any], scenario_step: int) -> PreparedStep:
        del batch
        observed = self.initial_state()
        if any(state.get(key) != observed[key] for key in ("competition_id", "candidate_id", "parent_revision")):
            raise ValueError("REEF algorithm state differs from the frozen GPTgrep parent")
        ledger = self._snapshot()
        carries_decision = state.get("decision") is not None or state.get("decision_sha256") is not None
        if (carries_decision and not _json_equal(self._applied_decision_state, dict(state))) or (
            self.candidate_id in ledger["committed"] and self._applied_decision_state is None
        ):
            raise ValueError("Restored decision requires successful startup reconciliation before a new step")
        if self.candidate_id in ledger["committed"]:
            raise ValueError("Candidate is already committed; a further REEF step is not authorized")
        if self.candidate_id not in ledger["candidates"]:
            raise ValueError("Actual committed/tested/AGY-reviewed candidate evidence is required")
        if not isinstance(scenario_step, int) or scenario_step < 0:
            raise ValueError("Invalid REEF scenario step")
        candidate = UpdateCandidate(self.candidate_id, {"competition_id": observed["competition_id"], "scenario_step": scenario_step})
        return PreparedStep.with_candidate(candidate, state={**observed, "scenario_step": scenario_step})

    def evaluate(self, candidate: UpdateCandidate) -> EvaluationResult:
        if candidate.candidate_id != self.candidate_id:
            raise ValueError("Foreign candidate")
        response = self.bridge.request("evaluate", {"candidateId": self.candidate_id})
        decision = response["result"]
        return EvaluationResult("gptgrep_strict_typescript", "1", {"decision": decision},
                                {"decision_sha256": response["resultSha256"], "candidate_id": self.candidate_id})

    def settle_step(self, prepared: PreparedStep, decision: SelectionDecision) -> TrainStepResult:
        if prepared.candidate is None or prepared.candidate.candidate_id != self.candidate_id:
            raise ValueError("Unprepared candidate")
        expected = self.evaluate(prepared.candidate)
        if decision.policy != "gptgrep_strict_typescript" or decision.evaluation != expected or decision.selected != (expected.metrics["decision"]["outcome"] == "development_nominee"):
            raise ValueError("Foreign or altered selection policy/result")
        if expected.metrics["decision"]["outcome"] == "blocked":
            raise ValueError("Unknown measurements leave the REEF step unaccepted")
        snapshot = self._snapshot()
        if self.candidate_id in snapshot["staged"]:
            staged = snapshot["staged"][self.candidate_id]
            if not _json_equal(staged, expected.metrics["decision"]):
                raise ValueError("Persisted decision differs from current evidence")
        else:
            self.bridge.request("stage", {"version": snapshot["version"], "eventId": self._event_id("stage"),
                                         "fence": self.fence, "candidateId": self.candidate_id})
        # No artifact/runtime_load_id/training_job_id: those fields publish model bytes.
        # Only the later durable scenario-commit callback may commit this decision.
        return TrainStepResult(state={**prepared.state, "decision": expected.metrics["decision"],
                                      "decision_sha256": expected.metadata["decision_sha256"]},
                               metrics={"development_nominee": decision.selected, "publication": "none"})

    def abort_step(self, prepared: PreparedStep) -> None:
        del prepared
        # Durable results/reservations are retained. Aborting does not authorize a retry.

    def recover_pending_step(self, scenario_step: int, *, committed_training_job_id: str | None) -> None:
        del scenario_step
        if committed_training_job_id is not None:
            raise ValueError("External Harbor jobs are not REEF weight-training jobs")
        self._snapshot()  # Reconcile through explicit owner receipts; never dispatch on restart.

    def acknowledge_commit(self, scenario_step: int, training_job_id: str) -> None:
        del scenario_step, training_job_id
        raise ValueError("This backend has no weight-training job to acknowledge")

    def commit_applied(self, state: Mapping[str, Any], *, authorize_next_step: bool = True) -> None:
        initial = self.initial_state()
        if any(state.get(key) != initial[key] for key in ("competition_id", "candidate_id", "parent_revision")):
            raise ValueError("Durable REEF state does not bind this candidate decision")
        if state.get("decision") is None and state.get("decision_sha256") is None:
            return  # REEF also durably commits failed-instruction/no-candidate skips.
        if not state.get("decision_sha256"):
            raise ValueError("Durable REEF decision has no digest")
        snapshot = self._snapshot()
        if self.candidate_id in snapshot["committed"]:
            committed = self.bridge.request("committed", {"candidateId": self.candidate_id})
            if not _json_equal(committed["result"], state.get("decision")) or committed["resultSha256"] != state["decision_sha256"]:
                raise ValueError("Committed decision changed")
            if authorize_next_step:
                self._applied_decision_state = deepcopy(dict(state))
            return
        if not _json_equal(snapshot["staged"].get(self.candidate_id), state.get("decision")):
            raise ValueError("Durable REEF decision content differs from the staged ledger decision")
        self.bridge.request("commit", {"version": snapshot["version"], "eventId": self._event_id("commit"),
                                      "fence": self.fence, "candidateId": self.candidate_id,
                                      "decisionSha256": state["decision_sha256"]})
        committed = self.bridge.request("committed", {"candidateId": self.candidate_id})
        if not _json_equal(committed["result"], state.get("decision")) or committed["resultSha256"] != state["decision_sha256"]:
            raise ValueError("Applied decision changed")
        if authorize_next_step:
            self._applied_decision_state = deepcopy(dict(state))


class GptgrepEvaluationPlugin(CandidateEvaluationPlugin):
    """Explicitly installed; REEF's BackendAlwaysSelectPlugin is never selected."""

    def __init__(self, backend: GptgrepCandidateBackend):
        self.backend = backend

    def evaluate(self, candidate: UpdateCandidate) -> EvaluationResult:
        return self.backend.evaluate(candidate)

    def decide(self, candidate: UpdateCandidate, evaluation: EvaluationResult) -> SelectionDecision:
        observed = self.backend.evaluate(candidate)
        if evaluation != observed:
            raise ValueError("Evaluation was replaced or its evidence changed")
        decision = evaluation.metrics["decision"]
        return SelectionDecision(decision["outcome"] == "development_nominee", "gptgrep_strict_typescript",
                                 decision["outcome"], evaluation, {"release_accepted": False, "c3_parent_changed": False})


def build_trainer(*, backend: GptgrepCandidateBackend, startup_recovery: StartupRecovery | None = None, **kwargs: Any) -> Trainer:
    """Explicit upstream factory: callers cannot replace or omit the strict plugin."""
    if {"candidate_backend", "candidate_evaluator"} & kwargs.keys():
        raise ValueError("The GPTgrep factory owns backend and selection policy")
    state = kwargs.get("algorithm_state")
    if state is not None and not isinstance(state, Mapping):
        raise ValueError("Restored algorithm state must be a mapping")
    carries_decision = state is not None and (state.get("decision") is not None or state.get("decision_sha256") is not None)
    if startup_recovery is None:
        if carries_decision or backend.candidate_id in backend._snapshot()["committed"]:
            raise ValueError("Restored decision requires explicit successful startup reconciliation")
    else:
        from gptgrep_reef.recovery import StartupRecovery, reconcile_startup

        if not isinstance(startup_recovery, StartupRecovery):
            raise ValueError("Explicit upstream startup recovery inputs required")
        if kwargs.get("scenario") != startup_recovery.binding.scenario or kwargs.get("records") is not startup_recovery.store.records:
            raise ValueError("Trainer scenario/record store differs from startup recovery")
        if startup_recovery.record_pin is not None and not carries_decision:
            raise ValueError("Pinned startup recovery requires the exact restored decision state")
        reconcile_startup(backend=backend, store=startup_recovery.store, binding=startup_recovery.binding,
                          record_pin=startup_recovery.record_pin, fresh_lease=startup_recovery.fresh_lease,
                          expected_algorithm_state=state)
    return Trainer.build(candidate_backend=backend, candidate_evaluator=GptgrepEvaluationPlugin(backend), **kwargs)
