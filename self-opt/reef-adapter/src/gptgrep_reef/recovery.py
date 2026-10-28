"""Validate one real REEF commit before reconciling its staged GPTgrep decision.

The caller freezes the binding before the step and retains the returned receipt
beside the exact exported CommitRecord. This module reads upstream history; it
does not recover record consumption, append a REEF step, or run an evaluation.
"""

from __future__ import annotations

import hashlib
import json
import re
import time
from collections.abc import Mapping
from copy import deepcopy
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from reef.core.artifact_ref import ArtifactRef, encode_artifact_ref
from reef.storage.commit_history import CommitHistoryScenarioStore
from reef.storage.commits import CommitRecord

from gptgrep_reef.backend import _json_equal

if TYPE_CHECKING:
    from gptgrep_reef.backend import GptgrepCandidateBackend

_MAX_RECORD_BYTES = 1024 * 1024
_MAX_SAFE_INTEGER = 2**53 - 1


def _integer(value: object, *, minimum: int = 0) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and minimum <= value <= _MAX_SAFE_INTEGER


def _text(value: object, limit: int = 256) -> bool:
    return isinstance(value, str) and 0 < len(value) <= limit


@dataclass(frozen=True, kw_only=True)
class RecoveryBinding:
    """Owner-frozen identity and expected post-step cursor, never inferred on restart."""

    scenario: str
    step: int
    competition_id: str
    candidate_id: str
    parent_revision: str
    scenario_fingerprint: str
    decision_sha256: str
    artifact_ref: ArtifactRef
    high_water_sequence: int
    high_water_offset: int
    consumed_ids: frozenset[str]
    previous_fencing_token: int
    store_label: str

    def __post_init__(self) -> None:
        if not all(_text(value) for value in (self.scenario, self.competition_id, self.candidate_id)):
            raise ValueError("Bounded scenario, competition and candidate identities required")
        if not _text(self.store_label, 4096) or not isinstance(self.artifact_ref, ArtifactRef):
            raise ValueError("Actual store label and upstream ArtifactRef required")
        for value, size in ((self.parent_revision, 40), (self.scenario_fingerprint, 64), (self.decision_sha256, 64)):
            if not isinstance(value, str) or re.fullmatch(rf"[0-9a-f]{{{size}}}", value) is None:
                raise ValueError("Frozen revision and SHA-256 bindings required")
        if not _integer(self.step, minimum=1) or not all(_integer(value) for value in (
            self.high_water_sequence, self.high_water_offset, self.previous_fencing_token,
        )):
            raise ValueError("Finite frozen step, cursor and previous fence required")
        if not isinstance(self.consumed_ids, frozenset) or len(self.consumed_ids) > 10000 or not all(_text(value) for value in self.consumed_ids):
            raise ValueError("Bounded frozen consumed record IDs required")


@dataclass(frozen=True, kw_only=True)
class StartupRecovery:
    """Explicit input to build_trainer for a restored algorithm state."""

    store: CommitHistoryScenarioStore
    binding: RecoveryBinding
    record_pin: Mapping[str, str] | None
    fresh_lease: Mapping[str, Any]


def _canonical_record(value: Mapping[str, Any]) -> bytes:
    # This is a local record-content digest, not the TypeScript decision digest.
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate key in pinned REEF record")
        result[key] = value
    return result


def _invalid_constant(value: str) -> None:
    raise ValueError(f"Non-finite JSON value in pinned REEF record: {value}")


def _pinned_record(pin: Mapping[str, str]) -> CommitRecord:
    if set(pin) != {"path", "sha256"} or not isinstance(pin["sha256"], str) or re.fullmatch(r"[0-9a-f]{64}", pin["sha256"]) is None:
        raise ValueError("Closed record path/SHA-256 pin required")
    if not isinstance(pin["path"], str):
        raise ValueError("Pinned record path must be a string")
    path = Path(pin["path"])
    if not path.is_absolute() or path.is_symlink() or path.resolve() != path or not path.is_file():
        raise ValueError("Pinned record must be a physical regular file")
    with path.open("rb") as stream:
        raw = stream.read(_MAX_RECORD_BYTES + 1)
    if len(raw) > _MAX_RECORD_BYTES or hashlib.sha256(raw).hexdigest() != pin["sha256"]:
        raise ValueError("Pinned REEF record bytes changed or exceed the bound")
    value = json.loads(raw, object_pairs_hook=_unique_object, parse_constant=_invalid_constant)
    if not isinstance(value, dict):
        raise ValueError("Pinned REEF record must be an object")
    record = CommitRecord.from_dict(value)
    if not _json_equal(value, record.to_dict()):
        raise ValueError("Pinned export must be the exact current upstream CommitRecord format")
    return record


def _history(store: CommitHistoryScenarioStore, binding: RecoveryBinding) -> tuple[CommitRecord, ...]:
    if not isinstance(store, CommitHistoryScenarioStore) or store.durable is not True or store.commit_history.label != binding.store_label:
        raise ValueError("Recovery requires the frozen actual durable upstream scenario store")
    history = store.history()
    if len(history) > 10000 or any(not isinstance(record, CommitRecord) or record.scenario != binding.scenario for record in history):
        raise ValueError("Foreign or unbounded upstream commit history")
    if any(right.step <= left.step for left, right in zip(history, history[1:])):
        raise ValueError("Upstream commit history is not strictly ordered")
    return history


def _history_head(history: tuple[CommitRecord, ...]) -> tuple[int, str | None]:
    return len(history), hashlib.sha256(_canonical_record(history[-1].to_dict())).hexdigest() if history else None


def _snapshot(backend: GptgrepCandidateBackend, binding: RecoveryBinding, fresh_lease: Mapping[str, Any]) -> Mapping[str, Any]:
    snapshot = backend._snapshot()
    contract = snapshot["contract"]
    if backend.candidate_id != binding.candidate_id or any((
        contract["competitionId"] != binding.competition_id,
        contract["parentRevision"] != binding.parent_revision,
        contract["scenarioFingerprint"] != binding.scenario_fingerprint,
        binding.candidate_id not in contract["candidateIds"],
    )):
        raise ValueError("Recovery differs from the frozen competition, candidate, parent or scenario")
    now = int(time.time() * 1000)
    if set(fresh_lease) != {"holder", "token", "expiresAt"} or not _text(fresh_lease.get("holder")) or not _integer(fresh_lease.get("token"), minimum=1) or not _integer(fresh_lease.get("expiresAt")):
        raise ValueError("Closed fresh recovery lease required")
    if not _json_equal(snapshot.get("lease"), dict(fresh_lease)) or snapshot.get("fence") != fresh_lease["token"] or not _json_equal(backend.fence, {"holder": fresh_lease["holder"], "token": fresh_lease["token"]}):
        raise ValueError("Recovery lease is not the actual current durable ledger fence")
    if fresh_lease["token"] <= binding.previous_fencing_token or not now < fresh_lease["expiresAt"] <= now + 300000:
        raise ValueError("Recovery requires a fresh fence and an unexpired lease bounded to five minutes")
    return snapshot


def _decision_identity(decision: Any, binding: RecoveryBinding) -> None:
    if not isinstance(decision, Mapping) or any(decision.get(key) != value for key, value in (
        ("competitionId", binding.competition_id), ("candidateId", binding.candidate_id),
        ("parentRevision", binding.parent_revision), ("scenarioFingerprint", binding.scenario_fingerprint),
    )):
        raise ValueError("Persisted decision differs from frozen recovery identity")
    if decision.get("outcome") not in {"development_nominee", "incumbent_retained"} or decision.get("c3ParentChanged") is not False or decision.get("releaseAccepted") is not False:
        raise ValueError("Recovery cannot accept a blocked decision or change parent/release acceptance")


def _record_binding(record: CommitRecord, binding: RecoveryBinding) -> Mapping[str, Any]:
    if record.scenario != binding.scenario or record.step != binding.step or record.artifact_ref != binding.artifact_ref:
        raise ValueError("REEF record scenario, step or unchanged artifact head differs")
    if record.checkpoint is not False or record.pending or record.operation != "training" or not record.operation_verified or record.training_job_id is not None or record.component is not None or record.components is not None:
        raise ValueError("Recovery requires an activated flat no-publication training record")
    if (record.high_water_sequence, record.high_water_offset, record.consumed_ids) != (binding.high_water_sequence, binding.high_water_offset, binding.consumed_ids):
        raise ValueError("REEF record cursor or consumed IDs differ from the frozen step")
    state = record.algorithm_state
    if not isinstance(state, Mapping) or set(state) != {"competition_id", "candidate_id", "parent_revision", "scenario_step", "decision", "decision_sha256"}:
        raise ValueError("REEF record must contain the closed GPTgrep decision state")
    if any(state[key] != value for key, value in (
        ("competition_id", binding.competition_id), ("candidate_id", binding.candidate_id),
        ("parent_revision", binding.parent_revision), ("scenario_step", binding.step - 1),
        ("decision_sha256", binding.decision_sha256),
    )) or not _integer(state["scenario_step"]):
        raise ValueError("REEF algorithm state differs from the frozen step/decision")
    _decision_identity(state["decision"], binding)
    return state


def reconcile_startup(
    *, backend: GptgrepCandidateBackend, store: CommitHistoryScenarioStore, binding: RecoveryBinding,
    record_pin: Mapping[str, str] | None, fresh_lease: Mapping[str, Any],
    expected_algorithm_state: Mapping[str, Any] | None = None,
) -> Mapping[str, Any]:
    """Read actual durable history and reconcile only the selected decision.

    A successful receipt validates the cursor; upstream scenario restoration is
    still responsible for replaying record consumption. Exceptions are blocking.
    """
    backend._applied_decision_state = None
    snapshot = _snapshot(backend, binding, fresh_lease)
    if expected_algorithm_state is not None and any(expected_algorithm_state.get(key) != value for key, value in (
        ("competition_id", binding.competition_id), ("candidate_id", binding.candidate_id),
        ("parent_revision", binding.parent_revision),
    )):
        raise ValueError("Restored algorithm state differs from the frozen recovery identity")
    history = _history(store, binding)
    head = _history_head(history)
    staged = snapshot["staged"].get(binding.candidate_id)
    committed = snapshot["committed"].get(binding.candidate_id)
    if staged is None and committed is None:
        raise ValueError("No persisted candidate decision to reconcile")
    for decision in (staged, committed):
        if decision is not None:
            _decision_identity(decision, binding)
    if staged is not None and committed is not None and not _json_equal(staged, committed):
        raise ValueError("Staged and committed decisions contradict each other")
    if staged is not None:
        observed = backend.bridge.request("staged", {"candidateId": binding.candidate_id})
        if not _json_equal(observed["result"], staged) or observed["resultSha256"] != binding.decision_sha256:
            raise ValueError("Staged decision differs from the frozen digest/content")

    record = None if record_pin is None else _pinned_record(record_pin)
    if record is None:
        if committed is not None or any(item.step >= binding.step or (
            item.algorithm_state and item.algorithm_state.get("candidate_id") == binding.candidate_id and item.algorithm_state.get("decision") is not None
        ) for item in history):
            raise ValueError("A durable or committed decision requires its exact exported record pin")
        if (history[-1].step if history else 0) != binding.step - 1 or (history and history[-1].artifact_ref != binding.artifact_ref):
            raise ValueError("Uncommitted recovery differs from the frozen preceding step/artifact")
        if expected_algorithm_state is not None and (expected_algorithm_state.get("decision") is not None or expected_algorithm_state.get("decision_sha256") is not None):
            raise ValueError("A restored decision requires an exact durable REEF record")
        status = "uncommitted"
    else:
        state = _record_binding(record, binding)
        if not history or not _json_equal(history[-1].to_dict(), record.to_dict()):
            raise ValueError("Pinned export does not equal the actual selected durable history head")
        if any(item.algorithm_state and item.algorithm_state.get("candidate_id") == binding.candidate_id and item.algorithm_state.get("decision") is not None for item in history[:-1]):
            raise ValueError("Another durable record already binds this candidate decision")
        if expected_algorithm_state is not None and not _json_equal(dict(expected_algorithm_state), dict(state)):
            raise ValueError("Restored algorithm state differs from the selected durable record")
        if not _json_equal(committed if committed is not None else staged, state["decision"]):
            raise ValueError("Durable REEF decision content differs from the persisted ledger decision")
        # Recheck the fence/history immediately before the existing callback. The
        # TypeScript commit performs its own version, lease and decision-digest CAS.
        current = _snapshot(backend, binding, fresh_lease)
        if current["version"] != snapshot["version"] or _history_head(_history(store, binding)) != head:
            raise ValueError("Recovery inputs changed before ledger reconciliation")
        backend.commit_applied(state, authorize_next_step=False)
        status = "already_committed" if committed is not None else "committed"

    final = _snapshot(backend, binding, fresh_lease)
    if _history_head(_history(store, binding)) != head:
        raise ValueError("REEF history changed during startup reconciliation")
    if record is None:
        if final["version"] != snapshot["version"] or final["committed"].get(binding.candidate_id) is not None:
            raise ValueError("Uncommitted recovery unexpectedly changed the ledger")
    else:
        verified = backend.bridge.request("committed", {"candidateId": binding.candidate_id})
        if not _json_equal(verified["result"], record.algorithm_state["decision"]) or verified["resultSha256"] != binding.decision_sha256:
            raise ValueError("Reconciled decision does not match the frozen digest/content")
    receipt = {
        "schema": "gptgrep.reef-startup-reconciliation.v1", "status": status,
        "competitionId": binding.competition_id, "candidateId": binding.candidate_id,
        "parentRevision": binding.parent_revision, "scenarioFingerprint": binding.scenario_fingerprint,
        "scenario": binding.scenario, "step": binding.step, "storeLabel": store.commit_history.label,
        "recordPin": None if record_pin is None else dict(record_pin),
        "recordContentSha256": None if record is None else hashlib.sha256(_canonical_record(record.to_dict())).hexdigest(),
        "decisionSha256": binding.decision_sha256, "artifactRef": encode_artifact_ref(binding.artifact_ref),
        "cursorValidation": "no_record" if record is None else "matched",
        "consumptionRestored": False, "reefRecordCountBefore": head[0], "reefRecordCountAfter": head[0],
        "ledgerVersionBefore": snapshot["version"], "ledgerVersionAfter": final["version"],
        "freshLease": dict(fresh_lease), "reefIdentity": dict(backend.reef_identity),
        "newReefStep": False, "paidDispatch": False, "c3ParentChanged": False, "releaseAccepted": False,
    }
    if record is not None:
        backend._applied_decision_state = deepcopy(dict(record.algorithm_state))
    return receipt
