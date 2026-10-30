"""Offline lifecycle fixtures using the actual installed REEF contracts."""

from __future__ import annotations

import unittest
from unittest.mock import patch

from reef.core.evaluation import CandidateEvaluationPlugin, EvaluationResult, UpdateCandidate
from reef.train.backend import CandidateBackend
from reef.train.types import NoArtifactPublication

from gptgrep_reef.backend import GptgrepCandidateBackend, GptgrepEvaluationPlugin, build_trainer


class FixtureBridge:
    """Explicit in-memory test double; never emitted as live qualification evidence."""

    def __init__(self, outcome="blocked"):
        self.calls = []
        self.decision = {"outcome": outcome, "fixture_only": True}
        self.state = {"version": 0, "contract": {"competitionId": "fixture", "candidateIds": ["a", "b"], "parentRevision": "1" * 40},
                      "candidates": {"a": {"fixture_only": True}}, "staged": {}, "committed": {}}

    def request(self, action, payload):
        self.calls.append((action, payload))
        if action == "snapshot":
            result = self.state
        elif action == "evaluate":
            result = self.decision
        elif action == "committed":
            result = self.state["committed"]["a"]
        elif action == "stage":
            self.state["staged"]["a"] = self.decision
            self.state["version"] += 1
            result = self.decision
        elif action == "commit":
            self.state["committed"]["a"] = self.decision
            self.state["version"] += 1
            result = self.decision
        else:
            raise AssertionError(f"Unexpected fixture operation: {action}")
        return {"result": result, "resultSha256": "a" * 64, "stateVersion": self.state["version"]}


class BackendTests(unittest.TestCase):
    def backend(self, outcome="blocked"):
        bridge = FixtureBridge(outcome)
        backend = GptgrepCandidateBackend(bridge=bridge, candidate_id="a", holder="fixture", fencing_token=1)
        return bridge, backend, GptgrepEvaluationPlugin(backend)

    def test_actual_upstream_inheritance_and_explicit_plugin_wiring(self):
        self.assertTrue(issubclass(GptgrepCandidateBackend, CandidateBackend))
        self.assertTrue(issubclass(GptgrepEvaluationPlugin, CandidateEvaluationPlugin))
        _, backend, _ = self.backend()
        self.assertEqual(backend.experiment_config()["observed_reef_revision"], backend.experiment_config()["expected_reef_revision"])
        with patch("gptgrep_reef.backend.Trainer.build") as build:
            build_trainer(backend=backend, scenario="fixture")
            self.assertIs(build.call_args.kwargs["candidate_backend"], backend)
            self.assertIsInstance(build.call_args.kwargs["candidate_evaluator"], GptgrepEvaluationPlugin)
        with self.assertRaises(ValueError):
            build_trainer(backend=backend, candidate_evaluator=None)

    def test_missing_candidate_and_unknown_evidence_cannot_settle(self):
        bridge, backend, plugin = self.backend()
        initial = backend.initial_state()
        prepared = backend.prepare_step(None, initial, 0)
        evaluation = plugin.evaluate(prepared.candidate)
        decision = plugin.decide(prepared.candidate, evaluation)
        self.assertFalse(decision.selected)
        with self.assertRaisesRegex(ValueError, "Unknown measurements"):
            backend.settle_step(prepared, decision)
        self.assertFalse(bridge.state["staged"])
        bridge.state["candidates"] = {}
        with self.assertRaisesRegex(ValueError, "candidate evidence"):
            backend.prepare_step(None, initial, 0)

    def test_altered_evaluation_and_parent_are_rejected(self):
        _, backend, plugin = self.backend()
        initial = backend.initial_state()
        with self.assertRaisesRegex(ValueError, "frozen GPTgrep parent"):
            backend.prepare_step(None, {**initial, "parent_revision": "2" * 40}, 0)
        with self.assertRaisesRegex(ValueError, "Foreign candidate"):
            backend.evaluate(UpdateCandidate("b"))
        with self.assertRaisesRegex(ValueError, "replaced"):
            plugin.decide(UpdateCandidate("a"), EvaluationResult("fixture", "1", {}))

    def test_fixture_settlement_never_publishes_and_commit_waits_for_durable_callback(self):
        bridge, backend, plugin = self.backend("development_nominee")
        prepared = backend.prepare_step(None, backend.initial_state(), 0)
        result = backend.settle_step(prepared, plugin.decide(prepared.candidate, plugin.evaluate(prepared.candidate)))
        self.assertIsInstance(result.publication, NoArtifactPublication)
        self.assertIsNone(result.training_job_id)
        self.assertTrue(bridge.state["staged"])
        self.assertFalse(bridge.state["committed"])
        with self.assertRaisesRegex(ValueError, "content differs"):
            backend.commit_applied({**result.state, "decision": {"outcome": "altered"}})
        backend.abort_step(prepared)
        backend.recover_pending_step(0, committed_training_job_id=None)
        self.assertFalse(bridge.state["committed"])
        backend.commit_applied(result.state)
        backend.commit_applied(result.state)
        self.assertEqual(sum(action == "commit" for action, _ in bridge.calls), 1)
        with self.assertRaisesRegex(ValueError, "already committed"):
            backend.prepare_step(None, result.state, 0)
        self.assertEqual(sum(action == "commit" for action, _ in bridge.calls), 1)

    def test_no_decision_skip_callback_is_a_noop(self):
        bridge, backend, _ = self.backend()
        backend.commit_applied(backend.initial_state())
        self.assertFalse(bridge.state["committed"])
        self.assertFalse(any(action == "commit" for action, _ in bridge.calls))

    def test_weight_job_identifiers_are_not_fabricated_for_external_jobs(self):
        _, backend, _ = self.backend()
        with self.assertRaisesRegex(ValueError, "weight-training"):
            backend.recover_pending_step(0, committed_training_job_id="external-harbor-job")
        with self.assertRaisesRegex(ValueError, "weight-training"):
            backend.acknowledge_commit(0, "external-harbor-job")


if __name__ == "__main__":
    unittest.main()
