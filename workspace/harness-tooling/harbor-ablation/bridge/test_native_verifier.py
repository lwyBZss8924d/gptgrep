"""Finite evaluator transfer/reward fixtures only; no native process or account."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from harbor.models.task.paths import TaskPaths
from harbor.models.trial.paths import TrialPaths
from harbor.verifier.factory import VerifierFactory
import gptgrep_harbor_verifier as verifier
import judge_format


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


class NativeVerifierTests(unittest.IsolatedAsyncioTestCase):
    def test_fifo_snapshot_refuses_without_waiting_for_writer(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary).resolve() / "selected-input.json"
            os.mkfifo(path, 0o600)
            code = """
import sys
from gptgrep_harbor_verifier import Snapshot
try:
    Snapshot(sys.argv[1])
except ValueError as error:
    assert str(error) == "input_bound"
else:
    raise AssertionError("FIFO accepted")
"""
            result = subprocess.run(
                [sys.executable, "-B", "-c", code, str(path)],
                cwd=Path(verifier.__file__).parent, capture_output=True, timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.task_paths = TaskPaths(self.root / "task")
        self.trial_paths = TrialPaths(self.root / "trial")
        self.task = SimpleNamespace(paths=self.task_paths)
        self.environment = SimpleNamespace(calls=[])
        self.calls = 0
        for path in [self.task_paths.tests_dir, self.trial_paths.artifacts_dir,
                     self.root / "prepared", self.root / "native-output"]:
            path.mkdir(parents=True, mode=0o700)
        self.answer = encoded({"answer": "synthetic response"})
        self.reference = encoded({"question": "generic question", "answer": [True, "generic"],
                                  "answer_format": "generic"})
        self.converted = encoded({"schema_version": "gptgrep.judge.reference.v1",
                                  **json.loads(self.reference)})
        self.write(self.trial_paths.artifacts_dir / "answer.json", self.answer)
        self.write(self.task_paths.tests_dir / "reference.json", self.reference)
        self.write(self.trial_paths.artifacts_dir / "manifest.json", encoded([
            {"source": "/app/answer.json", "destination": "answer.json", "type": "file",
             "status": "ok", "service": "main", "exclude": None}]))
        self.source = b"synthetic source bytes, not original rubric or executable"
        self.request = b"synthetic request bytes"
        for name, data in [("answer.json", self.answer), ("reference.json", self.converted),
                           ("request-manifest.json", b"{}"), ("judge-source.py", self.source),
                           ("request.json", self.request)]:
            self.write(self.root / "prepared" / name, data)
        self.phase = {"trial_id": "trial", "task_id": "task",
                      "answer_sha256": verifier.digest(self.answer),
                      "reference_sha256": verifier.digest(self.converted),
                      "judge_source_sha256": verifier.digest(self.source),
                      "request_sha256": verifier.digest(self.request)}
        self.write(self.root / "phase.json", encoded(self.phase))
        self.control = {"schema_version": "gptgrep.judge.native-control.v2", "role": "verifier_only",
                        "phase": {"binding": {"path": str(self.root / "phase.json"),
                                                "sha256": verifier.digest(encoded(self.phase))}},
                        "request": {"root": str(self.root / "prepared"),
                                    "manifest_sha256": verifier.digest(b"{}")},
                        "output_root": str(self.root / "native-output")}
        self.write(self.root / "control.json", encoded(self.control))
        self.write(self.root / "broker", b"never executed fake broker")
        self.entry = {"trial_dir": str(self.trial_paths.trial_dir), "task_dir": str(self.task_paths.task_dir),
                      "step_name": None, "control": str(self.root / "control.json"),
                      "control_sha256": verifier.digest(encoded(self.control)),
                      "answer_sha256": verifier.digest(self.answer),
                      "reference_sha256": verifier.digest(self.reference)}
        self.mapping = {"schema_version": "gptgrep.harbor.native-judge-map.v1",
                        "broker": {"path": str(self.root / "broker"),
                                   "sha256": verifier.digest(b"never executed fake broker"), "bytes": 26},
                        "entries": [self.entry]}

    def write(self, path, data):
        path.write_bytes(data)
        path.chmod(0o600)

    def configured(self, step=None):
        data = encoded(self.mapping)
        self.write(self.root / "map.json", data)
        return VerifierFactory.create_verifier_from_import_path(
            "gptgrep_harbor_verifier:OriginalNativeJudgeVerifier", task=self.task,
            trial_paths=self.trial_paths, environment=self.environment, step_name=step,
            phase_map=str(self.root / "map.json"), phase_map_sha256=verifier.digest(data))

    def result(self, equivalent, abstained):
        raw = encoded({"equivalent": equivalent, "abstained": abstained,
                       "reason": "private fixture reason stays evaluator-only"})
        report = {"status": "completed_actual", "judge_status": "completed",
                  "control_sha256": self.entry["control_sha256"],
                  "answer_sha256": verifier.digest(self.answer),
                  "reference_sha256": verifier.digest(self.converted),
                  "judge_source_sha256": verifier.digest(self.source),
                  "request_sha256": verifier.digest(self.request),
                  "request_manifest_sha256": verifier.digest(b"{}"),
                  "raw_final_sha256": verifier.digest(raw),
                  "verdict": {"equivalent": equivalent, "abstained": abstained, "reason": ""},
                  "observations": {"observation_origin": "native_app_server_v2", "wrapper_spawned": True,
                                   "wrapper_leader_reaped": True, "pipe_eof_observed": True,
                                   "terminal_sequence_observed": True, "transport_thread_id": "native-thread",
                                   "native_turn_id": "native-turn", "transport_final_item_id": "native-item",
                                   "actual_usage": None, "actual_cost": None, "actual_judge_model": None}}
        observed = encoded(report)
        self.write(self.root / "native-output" / "app-server-observation.json", observed)
        self.write(self.root / "native-output" / "native-final.json", raw)
        return {"schema_version": "gptgrep.judge.app-server-result.v1", "publication_status": "published",
                "publication_sha256": verifier.digest(observed), "no_retry": True, "report": report}

    async def invoke(self, _self, broker, control, sha):
        self.calls += 1
        self.assertEqual(control, str(self.root / "control.json"))
        self.assertEqual(sha, self.entry["control_sha256"])
        return self.reply

    async def run_fake(self, configured):
        async def fake(actor, broker, control, sha):
            return await self.invoke(actor, broker, control, sha)
        with patch.object(verifier.OriginalNativeJudgeVerifier, "_invoke", fake):
            return await configured.verify()

    async def test_installed_factory_exact_transfer_and_all_reward_combinations(self):
        for equivalent in (False, True):
            for abstained in (False, True):
                self.reply = self.result(equivalent, abstained)
                result = await self.run_fake(self.configured())
                self.assertEqual(result.rewards, {"reward": 1.0 if equivalent else 0.0})
                self.assertNotIn("reason", result.model_dump_json())
        self.assertEqual(self.calls, 4)
        self.assertEqual(self.environment.calls, [])  # No gold upload or reader command.

    async def test_missing_malformed_or_changed_answer_and_reference_never_invoke(self):
        for data in [b"{}", b"{bad}", b'{"answer":" "}', encoded({"answer": "changed"})]:
            self.write(self.trial_paths.artifacts_dir / "answer.json", data)
            self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.write(self.trial_paths.artifacts_dir / "answer.json", self.answer)
        self.write(self.task_paths.tests_dir / "reference.json", encoded({"answer": "changed"}))
        self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.assertEqual(self.calls, 0)

    async def test_missing_phase_or_wrong_artifact_provenance_has_no_reward_or_invoke(self):
        self.mapping["entries"][0]["trial_dir"] += "-other"
        self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.entry["trial_dir"] = str(self.trial_paths.trial_dir)
        for key, bad in [("source", "/app/final-message"), ("status", "skipped"), ("service", "sidecar")]:
            item = {"source": "/app/answer.json", "destination": "answer.json", "type": "file", "status": "ok"}
            item[key] = bad
            self.write(self.trial_paths.artifacts_dir / "manifest.json", encoded([item]))
            self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.assertEqual(self.calls, 0)

    async def test_step_specific_transfer_uses_only_active_step_artifact(self):
        step = "reader-step"
        self.entry["step_name"] = step
        target = self.trial_paths.step_artifacts_dir(step)
        target.mkdir(parents=True)
        for name in ["answer.json", "manifest.json"]:
            self.write(target / name, (self.trial_paths.artifacts_dir / name).read_bytes())
        tests = self.task_paths.step_tests_dir(step)
        tests.mkdir(parents=True)
        self.write(tests / "reference.json", self.reference)
        self.write(self.trial_paths.artifacts_dir / "answer.json", b"not the active step")
        self.reply = self.result(True, True)
        self.assertEqual((await self.run_fake(self.configured(step))).rewards, {"reward": 1.0})
        self.assertEqual(self.calls, 1)

    async def test_changed_source_and_symlink_transfer_refuse_before_invoke(self):
        self.write(self.root / "prepared" / "judge-source.py", b"changed source")
        self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.write(self.root / "prepared" / "judge-source.py", self.source)
        answer = self.trial_paths.artifacts_dir / "answer.json"
        answer.unlink()
        answer.symlink_to(self.root / "prepared" / "answer.json")
        self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.assertEqual(self.calls, 0)

    async def test_incomplete_unknown_wrong_binding_or_unpublished_result_has_no_reward(self):
        for key, value in [("status", "completion_unknown_quarantined"), ("judge_status", "unavailable"),
                           ("answer_sha256", "0" * 64)]:
            self.reply = self.result(True, True)
            self.reply["report"][key] = value
            self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.reply = self.result(True, True)
        self.reply["publication_status"] = "unknown_after_rename_no_retry"
        self.assertIsNone((await self.run_fake(self.configured())).rewards)

    async def test_published_final_tampering_and_late_source_change_clear_reward(self):
        self.reply = self.result(True, False)
        self.write(self.root / "native-output" / "native-final.json", encoded({"equivalent": False}))
        self.assertIsNone((await self.run_fake(self.configured())).rewards)
        self.reply = self.result(True, False)
        original = self.invoke
        async def changed(actor, broker, control, sha):
            reply = await original(actor, broker, control, sha)
            self.write(self.root / "prepared" / "judge-source.py", b"late change")
            return reply
        self.invoke = changed
        self.assertIsNone((await self.run_fake(self.configured())).rewards)

    async def test_unavailable_or_cancelled_invoke_is_not_retried(self):
        for error in (ValueError("private provider diagnostic"), asyncio.CancelledError()):
            async def failed(*_):
                self.calls += 1
                raise error
            with patch.object(verifier.OriginalNativeJudgeVerifier, "_invoke", failed):
                self.assertIsNone((await self.configured().verify()).rewards)
        self.assertEqual(self.calls, 2)


class OriginalLiteralFormatterTests(unittest.TestCase):
    def test_exact_retained_original_literals_unicode_structured_answer_and_character_cap(self):
        source = Path(os.environ["GPTGREP_JUDGE_TEST_ORIGINAL_SOURCE"]).read_bytes()
        self.assertEqual(verifier.digest(source),
                         "40d3c4f423fa4130c3c33bfdeae7eda009a142712a30ba35a14bc7c8fb0401eb")
        literals = judge_format.constants(source)  # AST only; never import upstream module.
        reference = {"schema_version": "gptgrep.judge.reference.v1", "question": "  generic\t\u2003 question ",
                     "answer": [True, None, {"generic": 1.25}], "answer_format": "generic"}
        response = "中🙂" * 6001
        formatted = verifier.decode(judge_format.format_buffers(
            source, encoded(reference), encoded({"answer": response})))
        self.assertEqual(formatted["prompt"], literals["PROMPT"].format(
            question="generic question", answer=reference["answer"],
            answer_format="generic", response=response[:12000]))
        self.assertEqual(formatted["verdict_schema"], literals["SCHEMA"])
        self.assertEqual(formatted["used_response_characters"], 12000)
        self.assertTrue(formatted["response_truncated"])
        self.assertEqual(formatted["answer_sha256"], verifier.digest(encoded({"answer": response})))


if __name__ == "__main__":
    unittest.main(verbosity=2)
