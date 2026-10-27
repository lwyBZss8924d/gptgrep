"""Offline restart fixtures using real REEF SQLite history and a labelled bridge double.

Run only in an owner-admitted storage environment. These fixtures do not qualify
native receipts or paid execution and do not restore upstream record consumption.
"""

from __future__ import annotations

import hashlib
import json
import os
import time
import unittest
import uuid
from copy import deepcopy
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from reef.core.artifact_ref import ArtifactRef
from reef.storage.commits import CommitRecord
from reef.storage.sqlite import SQLiteScenarioStorage

from gptgrep_reef.backend import GptgrepCandidateBackend, build_trainer
from gptgrep_reef.recovery import RecoveryBinding, StartupRecovery, reconcile_startup


class RecoveryFixtureBridge:
    """In-memory ledger fixture; never emitted as real qualification evidence."""

    def __init__(self):
        self.calls = []
        self.decision = {"schema": "gptgrep.opt-development-decision.v1", "fixture_only": True,
                         "competitionId": "fixture", "candidateId": "a", "parentRevision": "1" * 40,
                         "scenarioFingerprint": "2" * 64, "outcome": "development_nominee",
                         "c3ParentChanged": False, "releaseAccepted": False}
        self.decision_sha256 = hashlib.sha256(json.dumps(self.decision, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        self.lease = {"holder": "fixture-recovery", "token": 2, "expiresAt": int(time.time() * 1000) + 120000}
        self.state = {"version": 3, "fence": 2, "lease": dict(self.lease),
                      "contract": {"competitionId": "fixture", "candidateIds": ["a", "b"],
                                   "parentRevision": "1" * 40, "scenarioFingerprint": "2" * 64},
                      "operations": {"fixture-terminal": {"state": "complete"}},
                      "candidates": {"a": {"fixture_only": True}}, "staged": {"a": deepcopy(self.decision)}, "committed": {}}
        self.after_commit = None

    def request(self, action, payload):
        self.calls.append((action, deepcopy(payload)))
        if action == "snapshot":
            result = self.state
        elif action == "staged":
            result = self.state["staged"][payload["candidateId"]]
        elif action == "committed":
            result = self.state["committed"][payload["candidateId"]]
        elif action == "commit":
            assert payload["version"] == self.state["version"]
            assert payload["fence"] == {"holder": self.lease["holder"], "token": self.lease["token"]}
            assert payload["decisionSha256"] == self.decision_sha256
            assert "a" not in self.state["committed"]
            self.state["committed"]["a"] = deepcopy(self.state["staged"]["a"])
            self.state["version"] += 1
            if self.after_commit is not None:
                self.after_commit()
            result = self.decision
        else:
            raise AssertionError(f"Recovery attempted unexpected fixture operation: {action}")
        return {"result": deepcopy(result), "resultSha256": self.decision_sha256, "stateVersion": self.state["version"]}


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        output = os.environ.get("ENVIRONMENTS_HARNESS_OUTPUT")
        if not output:
            raise RuntimeError("Recovery fixtures require the owner-admitted ENVIRONMENTS_HARNESS_OUTPUT; no temporary fallback")
        root = Path(output)
        if not root.is_absolute() or root.resolve() != root or root.is_symlink() or not root.is_dir():
            raise RuntimeError("Recovery fixture output must be an existing physical admitted directory")
        self.directory = root / f"reef-recovery-fixture-{uuid.uuid4().hex}"
        self.directory.mkdir(mode=0o700)
        self.storage = SQLiteScenarioStorage(directory=self.directory)
        self.store = self.storage.open("fixture-scenario")
        self.bridge = RecoveryFixtureBridge()
        self.backend = self.new_backend()
        self.binding = RecoveryBinding(
            scenario="fixture-scenario", step=1, competition_id="fixture", candidate_id="a",
            parent_revision="1" * 40, scenario_fingerprint="2" * 64,
            decision_sha256=self.bridge.decision_sha256,
            artifact_ref=ArtifactRef(content_id="fixture-content", release_id="fixture-release", parent_release_id=None),
            high_water_sequence=7, high_water_offset=11, consumed_ids=frozenset({"row-1", "row-2"}),
            previous_fencing_token=1, store_label=self.store.commit_history.label,
        )
        self.state = {"competition_id": "fixture", "candidate_id": "a", "parent_revision": "1" * 40,
                      "scenario_step": 0, "decision": deepcopy(self.bridge.decision),
                      "decision_sha256": self.bridge.decision_sha256}

    def tearDown(self):
        self.store.close()
        self.storage.close()

    def new_backend(self):
        return GptgrepCandidateBackend(bridge=self.bridge, candidate_id="a", holder=self.bridge.lease["holder"], fencing_token=self.bridge.lease["token"])

    def record(self, **changes):
        value = CommitRecord(scenario=self.binding.scenario, step=1, artifact_ref=self.binding.artifact_ref,
                             checkpoint=False, algorithm_state=self.state,
                             high_water_sequence=7, high_water_offset=11, consumed_ids=self.binding.consumed_ids,
                             recorded_at=1700000000.0)
        return replace(value, **changes)

    def export(self, record):
        path = self.directory / f"record-{uuid.uuid4().hex}.json"
        raw = (json.dumps(record.to_dict(), indent=2) + "\n").encode()
        path.write_bytes(raw)
        return {"path": str(path), "sha256": hashlib.sha256(raw).hexdigest()}

    def persist(self, record=None):
        record = self.record() if record is None else record
        self.store.commit_step(expected_step=record.step - 1, commit=record)
        return self.export(record)

    def reconcile(self, pin, **changes):
        arguments = {"backend": self.backend, "store": self.store, "binding": self.binding,
                     "record_pin": pin, "fresh_lease": self.bridge.lease}
        arguments.update(changes)
        with patch.object(self.store, "commit_step", side_effect=AssertionError("Recovery must not append a step")), \
             patch.object(self.store, "recover", side_effect=AssertionError("Recovery must not mutate consumption")), \
             patch.object(self.backend, "evaluate", side_effect=AssertionError("Recovery must not evaluate")):
            return reconcile_startup(**arguments)

    def commits(self):
        return sum(action == "commit" for action, _ in self.bridge.calls)

    def test_real_reopened_record_commits_once_then_validates_noop(self):
        pin = self.persist()
        self.store.close()
        self.storage.close()
        self.storage = SQLiteScenarioStorage(directory=self.directory)
        self.store = self.storage.open(self.binding.scenario)
        operations = deepcopy(self.bridge.state["operations"])
        first = self.reconcile(pin)
        self.backend = self.new_backend()
        second = self.reconcile(pin)
        self.assertEqual((first["status"], second["status"]), ("committed", "already_committed"))
        self.assertEqual(self.commits(), 1)
        self.assertEqual(len(self.store.history()), 1)
        self.assertEqual(self.store.history()[0].artifact_ref, self.binding.artifact_ref)
        self.assertEqual(self.bridge.state["operations"], operations)
        self.assertEqual(first["recordPin"], pin)
        self.assertEqual(first["cursorValidation"], "matched")
        self.assertFalse(first["consumptionRestored"])
        self.assertFalse(first["newReefStep"])
        self.assertFalse(first["paidDispatch"])
        with self.assertRaisesRegex(ValueError, "already committed"):
            self.backend.prepare_step(None, self.state, 1)

    def test_no_record_keeps_staged_decision_uncommitted(self):
        before = deepcopy(self.bridge.state)
        receipt = self.reconcile(None)
        self.assertEqual(receipt["status"], "uncommitted")
        self.assertEqual(self.bridge.state, before)
        self.assertEqual(self.store.history(), ())
        self.assertEqual(self.commits(), 0)
        self.assertIsNone(self.backend._applied_decision_state)
        with self.assertRaisesRegex(ValueError, "exact durable REEF record"):
            self.reconcile(None, expected_algorithm_state=self.state)
        with self.assertRaisesRegex(ValueError, "frozen digest/content"):
            self.reconcile(None, binding=replace(self.binding, decision_sha256="f" * 64))
        with self.assertRaisesRegex(ValueError, "frozen recovery identity"):
            self.reconcile(None, expected_algorithm_state={**self.backend.initial_state(), "parent_revision": "f" * 40})

    def test_already_committed_record_is_a_validated_noop(self):
        pin = self.persist()
        self.bridge.state["committed"]["a"] = deepcopy(self.bridge.decision)
        before = deepcopy(self.bridge.state)
        receipt = self.reconcile(pin)
        self.assertEqual(receipt["status"], "already_committed")
        self.assertEqual(self.bridge.state, before)
        self.assertEqual(self.commits(), 0)

    def test_frozen_identity_artifact_cursor_and_digest_mismatches_block(self):
        pin = self.persist()
        cases = {"scenario": "foreign-scenario", "step": 2, "competition_id": "foreign",
                 "candidate_id": "b", "parent_revision": "3" * 40, "scenario_fingerprint": "4" * 64,
                 "decision_sha256": "5" * 64, "store_label": "foreign-store",
                 "artifact_ref": ArtifactRef(content_id="changed", release_id="changed", parent_release_id=None),
                 "high_water_sequence": 8, "high_water_offset": 12, "consumed_ids": frozenset({"other-row"})}
        for field, value in cases.items():
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.reconcile(pin, binding=replace(self.binding, **{field: value}))
        self.assertEqual(self.commits(), 0)

    def test_fresh_actual_bounded_fence_is_required(self):
        pin = self.persist()
        now = int(time.time() * 1000)
        cases = ({**self.bridge.lease, "holder": "foreign"}, {**self.bridge.lease, "token": 1},
                 {**self.bridge.lease, "expiresAt": now - 1}, {**self.bridge.lease, "expiresAt": now + 600000},
                 {**self.bridge.lease, "extra": True})
        for lease in cases:
            with self.subTest(lease=lease), self.assertRaises(ValueError):
                self.reconcile(pin, fresh_lease=lease)
        with self.assertRaisesRegex(ValueError, "fresh fence"):
            self.reconcile(pin, binding=replace(self.binding, previous_fencing_token=2))
        for expiry in (now - 1, now + 600000):
            lease = {**self.bridge.lease, "expiresAt": expiry}
            self.bridge.state["lease"] = lease
            with self.assertRaisesRegex(ValueError, "bounded to five minutes"):
                self.reconcile(pin, fresh_lease=lease)
        self.assertEqual(self.commits(), 0)

    def test_boolean_number_substitution_cannot_match_the_decision_digest(self):
        altered_state = {**self.state, "decision": {**self.bridge.decision, "fixture_only": 1}}
        pin = self.persist(self.record(algorithm_state=altered_state))
        with self.assertRaisesRegex(ValueError, "content differs"):
            self.reconcile(pin)
        self.assertEqual(self.commits(), 0)
        self.bridge.state["committed"]["a"] = deepcopy(self.bridge.decision)
        with self.assertRaisesRegex(ValueError, "Committed decision changed"):
            self.backend.commit_applied(altered_state)

    def test_export_must_match_exact_bytes_and_actual_durable_head(self):
        pin = self.persist()
        Path(pin["path"]).write_text("{}")
        with self.assertRaisesRegex(ValueError, "bytes changed"):
            self.reconcile(pin)
        altered = self.export(self.record(metrics={"foreign_export": True}))
        with self.assertRaisesRegex(ValueError, "actual selected durable history head"):
            self.reconcile(altered)
        with self.assertRaisesRegex(ValueError, "exact exported record pin"):
            self.reconcile(None)
        self.assertEqual(self.commits(), 0)

    def test_published_checkpoint_pending_or_weight_job_record_cannot_reconcile(self):
        for changes in ({"checkpoint": True}, {"pending": True}, {"training_job_id": "foreign-job"},
                        {"component": "model"}, {"components": {"model": "weights"}}, {"operation_verified": False}):
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "no-publication"):
                self.reconcile(self.export(self.record(**changes)))
        self.assertEqual(self.commits(), 0)

    def test_stale_history_head_and_altered_ledger_decision_block(self):
        pin = self.persist()
        self.bridge.state["staged"]["a"]["fixture_only"] = "altered"
        with self.assertRaisesRegex(ValueError, "content differs"):
            self.reconcile(pin)
        self.bridge.state["staged"]["a"] = deepcopy(self.bridge.decision)
        self.store.commit_step(expected_step=1, commit=self.record(step=2, algorithm_state=None))
        with self.assertRaisesRegex(ValueError, "actual selected durable history head"):
            self.reconcile(pin)
        self.assertEqual(self.commits(), 0)

    def test_missing_export_cannot_hide_an_already_committed_ledger(self):
        self.bridge.state["committed"]["a"] = deepcopy(self.bridge.decision)
        with self.assertRaisesRegex(ValueError, "exact exported record pin"):
            self.reconcile(None)

    def test_failed_final_validation_does_not_authorize_a_new_step(self):
        pin = self.persist()
        self.bridge.after_commit = lambda: self.bridge.state.update(lease=None)
        with self.assertRaisesRegex(ValueError, "actual current durable ledger fence"):
            self.reconcile(pin)
        self.assertEqual(self.commits(), 1)
        self.assertIsNone(self.backend._applied_decision_state)
        with self.assertRaisesRegex(ValueError, "startup reconciliation"):
            self.backend.prepare_step(None, self.state, 1)

    def test_factory_and_prepare_step_refuse_unreconciled_restored_decision(self):
        with patch("gptgrep_reef.backend.Trainer.build") as build:
            with self.assertRaisesRegex(ValueError, "startup reconciliation"):
                build_trainer(backend=self.backend, scenario=self.binding.scenario, algorithm_state=self.state)
            build.assert_not_called()
        with self.assertRaisesRegex(ValueError, "startup reconciliation"):
            self.backend.prepare_step(None, self.state, 1)
        self.bridge.state["committed"]["a"] = deepcopy(self.bridge.decision)
        with patch("gptgrep_reef.backend.Trainer.build") as build:
            with self.assertRaisesRegex(ValueError, "startup reconciliation"):
                build_trainer(backend=self.backend, scenario=self.binding.scenario)
            build.assert_not_called()

    def test_factory_binds_the_exact_store_scenario_and_restored_state(self):
        pin = self.persist()
        recovery = StartupRecovery(store=self.store, binding=self.binding, record_pin=pin, fresh_lease=self.bridge.lease)
        arguments = {"backend": self.backend, "startup_recovery": recovery, "scenario": self.binding.scenario,
                     "records": self.store.records, "algorithm_state": self.state}
        with patch("gptgrep_reef.backend.Trainer.build") as build:
            for change in ({"scenario": "foreign"}, {"records": object()},
                           {"algorithm_state": {**self.state, "scenario_step": 1}}):
                with self.subTest(change=change), self.assertRaises(ValueError):
                    build_trainer(**{**arguments, **change})
            build.assert_not_called()
            self.assertEqual(self.commits(), 0)
            build_trainer(**arguments)
            build.assert_called_once()
            self.assertIs(build.call_args.kwargs["records"], self.store.records)
            self.assertEqual(build.call_args.kwargs["algorithm_state"], self.state)
        self.assertEqual(self.commits(), 1)


if __name__ == "__main__":
    unittest.main()
