"""Synthetic per-trial preparation, one deadline, and private mount custody."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from gptgrep_harbor import decisions
from gptgrep_harbor.common import canonical, sha
from gptgrep_harbor.reader import reader_argv
from gptgrep_harbor.runtime import native_environment
import test_harbor_runtime as fixtures

write_pin = fixtures.write_pin


class PreparationMaterializationTests(unittest.TestCase):
    def setUp(self):
        fixtures.HarborMaterializationTests.setUp(self)
        roots = [self.root / name for name in ("controls", "financial-family", "trial-state")]
        for root in roots:
            root.mkdir()
        source = {"path": decisions.CONTROL_ROOT + "/source.rs", "bytes": 1, "sha256": "a" * 64}
        seed = {"schema": "gptgrep.current_decisions.preparation_seed.v1",
                "financial_family": decisions.FAMILY_ROOT, "budget": {}, "budget_sha256": "b" * 64,
                "authority": source, "review": source, "source_files": [source],
                "max_sdk_submissions": 200, "cumulative_call_limit": 2000, "sole_writer": True}
        self.runtime.pop("decisions_budget")
        self.runtime.pop("decisions_ledger_dir")
        self.runtime["decisions_preparation"] = {
            "schema": "gptgrep.harbor.decisions-preparation.v1",
            "seed": write_pin(roots[0] / "seed.json", seed), "controls_directory": str(roots[0]),
            "financial_family_directory": str(roots[1]), "trial_state_directory": str(roots[2]),
            "max_concurrent_batches": 1, "sole_writer": True}

    def test_complete_materialization_has_unique_treatment_state_and_whole_family_mount(self):
        self.runtime["run_id"] = "a" * 80
        adapter = fixtures.HarborMaterializationTests.materializer(self)
        dataset = self.root / "prepared-dataset"
        result = adapter.run(dataset)
        states, run_ids, locks = set(), set(), set()
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            config = json.loads((task / "environment/reader.json").read_text())
            mounts = json.loads((task / "environment/docker-compose.yaml").read_text())["services"]["main"]["volumes"]
            judge = json.loads((task / "tests/docker-compose.yaml").read_text())["services"]["main"]["volumes"]
            self.assertEqual(len(judge), 1)
            if record["binding"]["arm"] == "raw_only":
                self.assertEqual(len(mounts), 1)
                self.assertNotIn("decisions_preparation", config)
                continue
            targets = {mount["target"]: mount for mount in mounts}
            self.assertEqual(len(targets), 5)
            self.assertTrue(targets[decisions.CONTROL_ROOT]["read_only"])
            self.assertFalse(targets[decisions.FAMILY_ROOT]["read_only"])
            self.assertEqual(targets[decisions.FAMILY_ROOT]["source"], self.runtime["decisions_preparation"]["financial_family_directory"])
            states.add(targets[decisions.TRIAL_ROOT]["source"])
            locks.add(targets[decisions.WRITER_LOCK]["source"])
            run_ids.add(config["decisions_preparation"]["run_id"])
            self.assertLessEqual(len(config["decisions_preparation"]["run_id"]), 96)
            argv = reader_argv(config, record["binding"], "synthetic request")
            self.assertEqual(argv[2], decisions.TRIAL_BUDGET)
            self.assertEqual(argv[argv.index("--max-concurrent-batches") + 1], "1")
            self.assertFalse(any("reference" in key for key in config["decisions_preparation"]))
        self.assertEqual((len(states), len(run_ids), len(locks)), (62, 62, 1))
        self.assertTrue(all(Path(path).is_dir() for path in states))
        self.assertEqual(Path(next(iter(locks))).read_bytes(), b"")
        with self.assertRaises(FileExistsError):
            adapter.run(self.root / "repeat-dataset")
        self.assertFalse((self.root / "repeat-dataset").exists())

    def test_template_null_existing_context_fields_mean_unselected(self):
        template = json.loads((Path(fixtures.__file__).parents[1] / "runtime.template.json").read_text())
        unselected = {key: template[key] for key in ("decisions_budget", "decisions_ledger_dir")}
        self.assertEqual(unselected, {"decisions_budget": None, "decisions_ledger_dir": None})
        self.runtime.update(unselected)
        adapter = fixtures.HarborMaterializationTests.materializer(self)
        self.assertEqual(adapter.runtime["decisions_preparation"]["schema"],
                         "gptgrep.harbor.decisions-preparation.v1")
        self.runtime["decisions_ledger_dir"] = "/synthetic/unselected-ledger"
        with self.assertRaisesRegex(ValueError, "decisions_preparation_mode_conflict"):
            fixtures.HarborMaterializationTests.materializer(self)

    def test_ambiguous_context_mode_and_parallel_preparation_are_rejected(self):
        self.runtime["decisions_budget"] = {"path": "/synthetic/unselected.json", "bytes": 1, "sha256": "d" * 64}
        with self.assertRaisesRegex(ValueError, "decisions_preparation_mode_conflict"):
            fixtures.HarborMaterializationTests.materializer(self)
        self.runtime.pop("decisions_budget")
        self.runtime["decisions_preparation"]["max_concurrent_batches"] = 2
        with self.assertRaisesRegex(ValueError, "decisions_preparation_control"):
            fixtures.HarborMaterializationTests.materializer(self)


class PreparationProcessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.controls, self.state = self.root / "controls", self.root / "state"
        self.controls.mkdir()
        self.state.mkdir()
        self.seed = write_pin(self.controls / "seed.json", {"synthetic": True})
        self.question = "Where is the ceramic kettle?"
        self.binding = {"arm": "gptgrep", "question_sha256": sha(self.question.encode())}
        self.task_input = ("Document: /data/corpus/generic.txt\nQuestion: " + self.question).encode()
        self.argv = ["/synthetic/pinned-product", "--decisions-budget", str(self.state / "budget.json"),
                     "ask", "--", self.task_input.decode(), "/data/corpus"]
        self.config = {"reader_mode": "rich_multimodal", "deadline_seconds": 20,
                       "decisions_preparation": {"seed": self.seed, "run_id": "ordinary-trial", "max_concurrent_batches": 1}}
        for name, value in (("CONTROL_ROOT", str(self.controls)), ("TRIAL_ROOT", str(self.state)),
                            ("TRIAL_BUDGET", str(self.state / "budget.json"))):
            selected = patch.object(decisions, name, value)
            selected.start()
            self.addCleanup(selected.stop)
        self.environment = {"PATH": "/usr/bin", "OPENROUTER_API_KEY": "synthetic-only",
            decisions.ATTACH_PATH_ENV: "/unselected/plan.json", decisions.ATTACH_SHA_ENV: "unselected",
            "GPTGREP_DECISIONS_PREPARATION_ONLY": "unselected", "BASH_ENV": "/unselected/startup"}

    def completed(self, argv, **kwargs):
        self.invocation = (argv, kwargs)
        request = json.loads((self.state / "input.json").read_bytes())
        budget = {"schema_version": "gptgrep.decisions-budget-contract.v1", "run_id": request["run_id"],
                  "deadline_unix_ms": request["deadline_unix_ms"], "max_submissions": 200,
                  "plan_digest": sha((self.state / "input.json").read_bytes()),
                  "ledger_path": str(self.state / "ledger/decisions.json")}
        plan = {"schema": "gptgrep.current_decisions.attachment.v1", "run_id": request["run_id"],
                "query_sha256": request["query_sha256"], "financial_family": decisions.FAMILY_ROOT,
                "max_requests": 2, "receipt_directory": str(self.state / "receipts"),
                "expected_sdk_snapshot": {"deadline_unix_ms": request["deadline_unix_ms"]}}
        value = {"schema": "gptgrep.current_decisions.trial_prepared.v1", "binding_sha256": sha(canonical(self.binding)),
                 "query_sha256": request["query_sha256"], "run_id": request["run_id"],
                 "deadline_unix_ms": request["deadline_unix_ms"], "models_invoked": 0, "financial_reservations_created": 0,
                 "retry": "forbidden", "max_requests": 2, "max_sdk_submissions": 200,
                 "receipt_directory": str(self.state / "receipts"), "original_clock_anchor_sha256": "c" * 64,
                 "budget": write_pin(self.state / "budget.json", budget),
                 "plan": write_pin(self.state / "plan.json", plan)}
        return {"return_code": 0, "failure": None, "stdout": canonical(value), "stderr": b"", "elapsed_ms": 3000}

    def prepare(self):
        return decisions.prepare(self.config, self.binding, self.question, self.task_input, self.argv,
            started=100, deadline_unix_ms=123456, environ=self.environment)

    def test_preparation_is_once_and_uses_remaining_deadline_and_verified_environment(self):
        with patch.object(decisions, "run", side_effect=self.completed) as execute, \
             patch.object(decisions.time, "monotonic", side_effect=[104, 107, 108]):
            env, receipt = self.prepare()
            argv, kwargs = self.invocation
            self.assertEqual(kwargs["timeout"], 16)
            self.assertIn("--harbor-prepare-input", argv)
            request = json.loads((self.state / "input.json").read_bytes())
            self.assertEqual(request["query_sha256"], sha(self.task_input))
            self.assertNotEqual(request["query_sha256"], self.binding["question_sha256"])
            self.assertEqual(argv[-2].encode(), self.task_input)
            self.assertNotIn("OPENROUTER_API_KEY", kwargs["env"])
            self.assertNotIn(decisions.ATTACH_PATH_ENV, kwargs["env"])
            self.assertEqual(env["OPENROUTER_API_KEY"], "synthetic-only")
            self.assertEqual(env[decisions.ATTACH_PATH_ENV], str(self.state / "plan.json"))
            self.assertEqual(env[decisions.ATTACH_SHA_ENV], sha((self.state / "plan.json").read_bytes()))
            self.assertNotIn("GPTGREP_DECISIONS_PREPARATION_ONLY", env)
            self.assertNotIn("BASH_ENV", env)
            self.assertEqual(receipt["deadline_unix_ms"], 123456)
            native = native_environment(env)
            for key in ("OPENROUTER_API_KEY", decisions.ATTACH_PATH_ENV, decisions.ATTACH_SHA_ENV):
                self.assertNotIn(key, native)
            with self.assertRaises(FileExistsError):
                self.prepare()
            self.assertEqual(execute.call_count, 1)

    def test_preparation_cannot_extend_the_original_deadline(self):
        with patch.object(decisions, "run", side_effect=self.completed), \
             patch.object(decisions.time, "monotonic", side_effect=[115, 121]):
            with self.assertRaisesRegex(ValueError, "reader_deadline"):
                self.prepare()
            self.assertEqual(self.invocation[1]["timeout"], 5)
            self.assertTrue((self.state / "preparer-process.json").exists())

    def test_changed_question_is_rejected_before_process_launch(self):
        self.binding["question_sha256"] = "e" * 64
        with patch.object(decisions, "run") as execute:
            with self.assertRaisesRegex(ValueError, "decisions_question_binding"):
                self.prepare()
            execute.assert_not_called()

    def test_changed_product_query_is_rejected_before_process_launch(self):
        self.argv[-2] = self.question
        with patch.object(decisions, "run") as execute:
            with self.assertRaisesRegex(ValueError, "decisions_query_binding"):
                self.prepare()
            execute.assert_not_called()
        self.assertFalse((self.state / "input.json").exists())

    def test_cohort_writer_lock_is_exclusive_until_the_reader_closes_it(self):
        lock = self.root / "writer.lock"
        lock.write_bytes(b"")
        with patch.object(decisions, "WRITER_LOCK", str(lock)):
            first = decisions.acquire_writer()
            try:
                with self.assertRaises(BlockingIOError):
                    decisions.acquire_writer()
            finally:
                os.close(first)
            second = decisions.acquire_writer()
            os.close(second)
        self.assertEqual(lock.read_bytes(), b"")
