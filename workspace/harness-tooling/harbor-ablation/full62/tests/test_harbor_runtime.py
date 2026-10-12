"""Real Harbor schemas and synthetic runtime boundaries; no Docker or model calls."""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
import sys
from unittest.mock import AsyncMock, patch
from types import SimpleNamespace
from bundle_fixtures import bundle_fixture, code_mode_host_fixture, sandbox_helper_fixture

from harbor.models.agent.context import AgentContext
from harbor.models.job.config import JobConfig
from harbor.models.task.config import TaskConfig, VerifierEnvironmentMode
from harbor.models.task.paths import TaskPaths
from harbor.models.task.verifier_mode import resolve_verifier_environment_definition

from gptgrep_harbor import adapter as adapter_module, verifier
from gptgrep_harbor.adapter import MAX_TOOL_CALLS, TOOL_CALL_WARNING_FLOOR, Full62Adapter
from gptgrep_harbor.agent import PairedNativeAgent
from gptgrep_harbor.common import MAX_NATIVE_PROMPT_BYTES, MAX_READER_QUESTION_BYTES, PROFILE, canonical, parse, sha
from gptgrep_harbor.native import JUDGE_OUTPUT_SCHEMA, codex_argv, output_schema_bytes
from gptgrep_harbor.reader import reader_argv, product_result, product_accounting
from gptgrep_harbor.process import run
from gptgrep_harbor.product_bundle import PRODUCT_ENTRYPOINT, PRODUCT_PROVENANCE, release_bundle
from gptgrep_harbor.runtime import (
    BASE_CAPABILITIES, BASE_TOOLS_PATH, CODEX_HOME, CODEX_SOURCE_COMMIT, DECISIONS_BUDGET,
    base_tools_identity, model_config, native_environment, native_wrapper, REFRESH_BLOCKER_URL,
    REVOKE_BLOCKER_URL, AUTH_JOB_LABEL, auth_compose,
    validate_asset_path,
)


def write_pin(path, value):
    data = value if isinstance(value, bytes) else canonical(value)
    path.write_bytes(data)
    return {"path": str(path), "bytes": len(data), "sha256": sha(data)}


class HarborMaterializationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.cohort = {"schema": "gptgrep.harbor.cohort.v1", "source_cohort_sha256": "a" * 64,
            "tasks": [{"pair_id": f"synthetic-{i}", "document_id": f"synthetic-doc-{i % 34}",
                       "question": f"Read synthetic item {i}.", "reference": f"reference-{i}",
                       "answer_format": "text"} for i in range(62)]}
        documents = []
        for i in range(34):
            documents.append({"document_id": f"synthetic-doc-{i}", "relative_path": f"raw-{i}.txt",
                "pin": write_pin(self.root / f"raw-{i}.txt", f"authored generic content {i}".encode())})
        self.runtime = {"schema": "gptgrep.harbor.runtime.v1", "profile": PROFILE,
            "native_cli": {"version": "0.162.1", "release_tag": "rust-v0.162.1", "source_commit": CODEX_SOURCE_COMMIT},
            "auth": {"method": "chatgpt_device_file", "reader_file": "/synthetic-private/reader/auth.json",
                     "judge_file": "/synthetic-private/judge/auth.json"},
            "decisions_budget": {"path": "/synthetic-private/decisions-budget.json", "bytes": 2, "sha256": "e" * 64},
            "decisions_ledger_dir": "/synthetic-private/ledger",
            "admitted": True, "admission": None, "account_scope": "Dev/Test/Eval", "platform": "linux/amd64",
            "run_id": "synthetic-full62", "jobs_dir": str(self.root / "jobs"),
            "reader_base_binary": {"path": "/usr/local/bin/codex", "bytes": 1, "sha256": "b" * 64},
            "codex_binary": "/usr/local/bin/codex", "gptgrep_binary": "/opt/gptgrep", "codex_home": CODEX_HOME,
            "gptgrep_binary_pin": {"path": "/opt/gptgrep", "bytes": 1, "sha256": "c" * 64},
            "judge_profile": {"model": "synthetic-frozen-judge", "reasoning_effort": "high", "service_tier": "default"},
            "permitted_tools": {"gptgrep": ["native shell", "GPTgrep"], "raw_only": ["native shell"]},
            "generic_capabilities": list(BASE_CAPABILITIES), "qualification_mismatches": [],
            "images": {role: f"synthetic/{role}@sha256:" + "d" * 64 for role in ("gptgrep", "raw_only", "judge")},
            "reader_env": {"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"}, "judge_env": {},
            "jev_model": "openai/gpt-6-luna-decisions", "reader_mode": "rich_multimodal",
            "native_ptc": True, "codex_permissions": "full62_reader",
            "primary_metric": "answer_equivalence_delta", "bootstrap_seed": 17, "trial_order_seed": 23,
            "max_asset_bytes": 1048576, "max_assets_total_bytes": 1048576,
            "deadline_seconds": 20, "judge_deadline_seconds": 20, "max_tool_calls": 8, "max_input_bytes": 8192,
            "documents": documents, "index_files": [{"relative_path": ".gptgrep/index.bin",
                "pin": write_pin(self.root / "index.bin", b"generic synthetic index")}],
            "resources": {"cpus": 1, "memory_mb": 512, "storage_mb": 512}}
        self.runtime["images"]["judge"] = self.runtime["images"]["raw_only"]
        tools = {"schema": "gptgrep.harbor.base-tools.v1", "build_inputs": {"platform": "linux/amd64"},
                 "pillow_version": "12.3.0", "native_cli_version": "0.162.1",
                 "native_source_commit": CODEX_SOURCE_COMMIT, "python_version": "3.13.0",
                 "pillow_pixel_check": "passed", "capabilities": list(BASE_CAPABILITIES),
                 "binaries": {name: {"path": "/synthetic/" + name, "bytes": 1, "sha256": "f" * 64}
                              for name in ("python3", "sh", "uv", "codex", "pdftotext", "pdftoppm")}}
        tools["binaries"]["codex"] = self.runtime["reader_base_binary"]
        _, helper = sandbox_helper_fixture(self.root / "helper")
        tools["native_sandbox_helpers"] = {"bwrap": helper}
        _, host = code_mode_host_fixture(self.root / "host", self.runtime["reader_base_binary"])
        tools["native_code_mode_host"] = host
        self.runtime["base_tool_environment"] = {"image": self.runtime["images"]["raw_only"],
            "provenance": write_pin(self.root / "base-tools.json", tools)}
        product = self.root / "product"
        product.mkdir()
        provenance = release_bundle(bundle_fixture(product), "linux/amd64")
        provenance["common_image"] = self.runtime["images"]["raw_only"]
        self.runtime.update(gptgrep_binary=PRODUCT_ENTRYPOINT, gptgrep_binary_pin=provenance["entrypoint"],
                            gptgrep_bundle={"provenance": write_pin(product / "provenance.json", provenance)})
        self.judge = {"schema": "gptgrep.harbor.primary-judge.v1",
            "prompt": "Question:{question}\nReference:{answer}\nFormat:{answer_format}\nResponse:{response}",
            "response_schema": {"type": "object", "additionalProperties": False, "required": ["equivalent"],
                                "properties": {"equivalent": {"type": "boolean"}}}}
        # The synthetic admission attests the runtime-only external-tool qualification.
        self.external_tools_excluded = True

    def materializer(self):
        cohort = write_pin(self.root / "cohort.json", self.cohort)
        judge = write_pin(self.root / "judge-contract.json", self.judge)
        approval = {"accepted": True, "scope": "harbor_full62_paired_once", "run_id": self.runtime["run_id"],
            "runtime_subject_sha256": sha(canonical({k: v for k, v in self.runtime.items() if k not in ("admission", "admitted")})),
            "source_cohort_sha256": self.cohort["source_cohort_sha256"], "profile": PROFILE,
            "platform": self.runtime["platform"], "images": self.runtime["images"],
            "native_cli_contract_qualified": True, "reader_base_binary": self.runtime["reader_base_binary"],
            "bare_image_excludes_gptgrep_and_indexes": True, "raw_only_index_provenance_verified": True,
            "named_permissions_qualified": True, "reader_effective_permissions_equal": True,
            "reader_external_tools_excluded": self.external_tools_excluded,
            "native_cli": self.runtime["native_cli"],
            "assets_sha256": sha(canonical(self.runtime["documents"] + self.runtime["index_files"])),
            "judge_sha256": judge["sha256"]}
        self.runtime["admission"] = write_pin(self.root / "synthetic-admission.json", approval)
        runtime = write_pin(self.root / "runtime.json", self.runtime)
        manifest = write_pin(self.root / "manifest.json", {"schema": "gptgrep.harbor.materialization.v1",
                                                          "cohort": cohort, "runtime": runtime, "judge_contract": judge})
        return Full62Adapter(manifest["path"], manifest["sha256"])

    def test_full124_tasks_parse_in_real_harbor_and_keep_role_isolation(self):
        self.cohort["tasks"][0]["question"] = "é" * (MAX_READER_QUESTION_BYTES // 2)
        adapter = self.materializer()
        output = self.root / "dataset"
        result = adapter.run(output)
        self.assertEqual(len(result["tasks"]), 124)
        self.assertEqual(len({row["binding"]["document_id"] for row in result["tasks"]}), 34)
        job = JobConfig.model_validate_json((self.root / "dataset.job.json").read_text())
        self.assertEqual((job.n_attempts, job.n_concurrent_trials, job.retry.max_retries), (1, 1, 0))
        self.assertEqual(job.datasets, [])
        self.assertEqual([task.path for task in job.tasks],
                         [output / name for name in result["trial_order"]["task_names"]])
        self.assertTrue(all(task.source == output.name for task in job.tasks))
        self.assertEqual(job.agents[0].import_path, "gptgrep_harbor.agent:PairedNativeAgent")
        # The fixture's treatment cap (8) is below the parity floor: valid, but warned.
        self.assertEqual(result["preflight"]["tool_budget"]["treatment_max_tool_calls"], 8)
        self.assertEqual([warning["code"] for warning in result["preflight"]["warnings"]],
                         ["treatment_tool_cap_may_bind_before_deadline"])
        # Decisions run in the treatment's GPTgrep host process, outside the Codex
        # tool sandbox, so the job keeps the OpenRouter key reference (never a value).
        job_record = json.loads((self.root / "dataset.job.json").read_text())
        self.assertEqual(job_record["agents"][0]["env"], {"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"})
        self.assertEqual(job_record["verifier"]["env"], {})
        reader_policies, budget_scopes, pair_policies = set(), set(), {}
        for record in result["tasks"]:
            task = output / record["task_name"]
            config = TaskConfig.model_validate(tomllib.loads((task / "task.toml").read_text()))
            self.assertEqual(config.verifier.environment_mode, VerifierEnvironmentMode.SEPARATE)
            definition = resolve_verifier_environment_definition(config, TaskPaths(task))
            self.assertEqual(definition.directory, task / "tests")
            self.assertTrue(definition.bundled_tests)
            self.assertEqual(config.artifacts, ["/answer/answer.json"])
            reader = json.loads((task / "environment/reader.json").read_text())
            self.assertEqual(reader["gptgrep_binary"], PRODUCT_ENTRYPOINT)
            self.assertEqual(reader["gptgrep_bundle"]["provenance"]["path"], PRODUCT_PROVENANCE)
            self.assertNotIn("reference", reader)
            self.assertNotIn("auth", reader)
            self.assertEqual(reader["budget_scope"]["reader_question_max_bytes"], 8192)
            self.assertEqual(reader["budget_scope"]["tool_calls"], "treatment_enforced_raw_observed_only")
            self.assertEqual(reader["budget_scope"]["treatment_max_tool_calls"], 8)
            self.assertEqual(reader["budget_scope"]["raw_tool_call_cap"], "none")
            budget_scopes.add(canonical(reader["budget_scope"]))
            self.assertIs(reader["native_ptc"], True)
            self.assertEqual(reader["codex_permissions"], "full62_reader")
            self.assertEqual(len(list((task / "environment/corpus").glob("raw-*.txt"))), 34)
            treatment = record["binding"]["arm"] == "gptgrep"
            self.assertEqual((task / "environment/corpus/.gptgrep/index.bin").exists(), treatment)
            reader_policies.add((task / "environment/config.toml").read_bytes())
            pair_policies.setdefault(record["binding"]["pair_id"], {})[record["binding"]["arm"]] = (
                task / "environment/config.toml").read_bytes()
            # The container keeps network for the native CLI's model connection
            # and the product's host-side Decisions requests; tools have none.
            self.assertEqual(tomllib.loads((task / "task.toml").read_text())["environment"]["network_mode"],
                             "public")
            for directory, key in (("environment", "reader_file"), ("tests", "judge_file")):
                compose = json.loads((task / directory / "docker-compose.yaml").read_text())
                mounts = compose["services"]["main"]["volumes"]
                self.assertEqual(mounts[0]["source"], self.runtime["auth"][key])
                self.assertEqual(mounts[0]["target"], CODEX_HOME + "/auth.json")
                self.assertTrue(mounts[0]["read_only"])
                self.assertFalse(mounts[0]["bind"]["create_host_path"])
                # Every trial service carries the job-wide label the host supervisor waits on.
                self.assertEqual(compose["services"]["main"]["labels"], {AUTH_JOB_LABEL: self.runtime["run_id"]})
                self.assertFalse((task / directory / "auth.json").exists())
                self.assertNotIn("COPY .", (task / directory / "Dockerfile").read_text())
            self.assertEqual(len(json.loads((task / "environment/docker-compose.yaml").read_text())["services"]["main"]["volumes"]), 3 if treatment else 1)
            judge = json.loads((task / "tests/judge.json").read_text())
            # The frozen response schema is pinned in the Judge's private area only.
            schema_bytes = (task / "tests/judge-output-schema.json").read_bytes()
            self.assertEqual(json.loads(schema_bytes), self.judge["response_schema"])
            self.assertEqual(judge["output_schema"], {"path": JUDGE_OUTPUT_SCHEMA, "bytes": len(schema_bytes),
                                                      "sha256": sha(schema_bytes)})
            self.assertEqual(judge["contract"]["prompt"], self.judge["prompt"])
            self.assertIn("COPY judge.json judge-output-schema.json test.sh /tests/",
                          (task / "tests/Dockerfile").read_text())
            self.assertIn("!judge-output-schema.json", (task / "tests/.dockerignore").read_text().splitlines())
            self.assertFalse((task / "environment/judge-output-schema.json").exists())
            self.assertEqual(judge["judge_profile"], self.runtime["judge_profile"])
            self.assertEqual(judge["base_tool_environment"], reader["base_tool_environment"])
            self.assertEqual(reader["base_tool_environment"]["provenance"]["path"], BASE_TOOLS_PATH)
            self.assertFalse((task / "tests/corpus").exists())
        self.assertEqual(reader_policies, {model_config(PROFILE, "reader")})
        self.assertEqual(len(budget_scopes), 1)
        # Both arms of every pair receive identical reader configuration bytes,
        # with no hosted web search and no model-tool network.
        self.assertEqual(len(pair_policies), 62)
        for policies in pair_policies.values():
            self.assertEqual(set(policies), {"gptgrep", "raw_only"})
            self.assertEqual(policies["gptgrep"], policies["raw_only"])
        policy = tomllib.loads(next(iter(reader_policies)).decode())
        self.assertEqual(policy["web_search"], "disabled")
        self.assertIs(policy["permissions"]["full62_reader"]["network"]["enabled"], False)

    def test_treatment_tool_cap_accepts_adapter_maximum_and_warns_below_parity_floor(self):
        self.runtime["max_tool_calls"] = MAX_TOOL_CALLS
        adapter = self.materializer()
        self.assertEqual(adapter.preflight["tool_budget"],
                         {"treatment_max_tool_calls": 10000, "raw_tool_call_cap": "none",
                          "adapter_maximum": 10000, "warning_floor": 64})
        self.assertEqual(adapter.preflight["warnings"], [])
        argv = reader_argv({**self.runtime, "source_relative_path": "raw-0.txt"},
                           {"arm": "gptgrep"}, "generic question")
        self.assertEqual(argv[argv.index("--max-tool-calls") + 1], "10000")
        for value, warned in ((TOOL_CALL_WARNING_FLOOR, False), (TOOL_CALL_WARNING_FLOOR - 1, True), (1, True)):
            self.runtime["max_tool_calls"] = value
            with self.subTest(max_tool_calls=value):
                warnings = self.materializer().preflight["warnings"]
                self.assertEqual(warnings, [{"code": "treatment_tool_cap_may_bind_before_deadline",
                                             "max_tool_calls": value, "warning_floor": 64}] if warned else [])
        for value in (0, MAX_TOOL_CALLS + 1, True, 64.0):
            self.runtime["max_tool_calls"] = value
            with self.subTest(max_tool_calls=value), self.assertRaisesRegex(ValueError, "integer_range"):
                self.materializer()
        self.assertFalse((self.root / "dataset").exists())

    def test_prepare_prints_budget_preflight_before_any_task_is_created(self):
        from contextlib import redirect_stdout
        import io
        from gptgrep_harbor import main as cli
        stdout, seen = io.StringIO(), []
        adapter = self.materializer()
        with patch.object(cli, "Full62Adapter", return_value=adapter), \
             patch.object(adapter, "run", side_effect=lambda *args, **kwargs: seen.append(stdout.getvalue())), \
             patch("sys.argv", ["gptgrep-harbor-full62", "prepare", "--manifest", "/synthetic/manifest.json",
                                "--manifest-sha256", "a" * 64, "--output-dir", str(self.root / "dataset")]), \
             redirect_stdout(stdout):
            cli.main()
        self.assertEqual(json.loads(seen[0]), {"preflight": adapter.preflight})
        self.assertEqual(json.loads(seen[0])["preflight"]["warnings"][0]["code"],
                         "treatment_tool_cap_may_bind_before_deadline")

    def test_frozen_judge_schema_must_be_strict_structured_output(self):
        del self.judge["response_schema"]["additionalProperties"]
        with self.assertRaisesRegex(ValueError, "strict_output_schema"):
            self.materializer()
        self.judge["response_schema"]["additionalProperties"] = False
        self.judge["response_schema"]["properties"]["reason"] = {"type": "string"}
        with self.assertRaisesRegex(ValueError, "strict_output_schema"):
            self.materializer()
        # `required` must list every property exactly once: it fixes their order.
        self.judge["response_schema"]["required"] = ["equivalent", "reason", "reason"]
        with self.assertRaisesRegex(ValueError, "strict_output_schema"):
            self.materializer()
        self.assertFalse((self.root / "dataset").exists())

    def test_judge_schema_file_restores_the_frozen_property_order(self):
        # The frozen-contract builder writes a key-sorted file, so only `required`
        # keeps the upstream order that strict structured output generates in.
        self.judge["response_schema"] = parse(canonical({
            "type": "object", "additionalProperties": False, "required": ["equivalent", "abstained", "reason"],
            "properties": {"equivalent": {"type": "boolean"}, "abstained": {"type": "boolean"},
                           "reason": {"type": "string"}}}))
        self.assertEqual(list(self.judge["response_schema"]["properties"]), ["abstained", "equivalent", "reason"])
        adapter = self.materializer()
        self.assertEqual(adapter_module.judge_contract(self.judge),
                         output_schema_bytes(self.judge["response_schema"]))
        row = adapter.rows[0]
        document = next(d for d in self.runtime["documents"] if d["document_id"] == row["document_id"])
        task = self.root / "single-task"
        adapter._task(task, Path(adapter_module.__file__).parent, row, document,
                      {"pair_id": row["pair_id"], "document_id": row["document_id"], "arm": "gptgrep"})
        schema_file = task / "tests/judge-output-schema.json"
        self.assertEqual(list(json.loads(schema_file.read_bytes())["properties"]), ["equivalent", "abstained", "reason"])
        judge = json.loads((task / "tests/judge.json").read_text())
        self.assertEqual(list(judge["contract"]["response_schema"]["properties"]), ["abstained", "equivalent", "reason"])
        # The verifier recomputes the same bytes from its key-sorted judge.json.
        pin = {**judge["output_schema"], "path": str(schema_file)}
        with patch.object(verifier, "JUDGE_OUTPUT_SCHEMA", str(schema_file)):
            self.assertEqual(verifier.output_schema({**judge, "output_schema": pin},
                                                    judge["contract"]["response_schema"]), pin)

    def test_shared_native_reader_fields_are_required_before_materialization(self):
        for key, value in (("native_ptc", False), ("native_ptc", None), ("codex_permissions", "other")):
            original = self.runtime[key]
            self.runtime[key] = value
            with self.subTest(key=key, value=value), self.assertRaisesRegex(ValueError, "shared_native_reader_required"):
                self.materializer()
            self.runtime[key] = original
        self.assertFalse((self.root / "dataset").exists())

    def test_external_tool_exclusion_needs_runtime_qualification(self):
        # Disabled config features are requests; only the runtime review can
        # attest that no connector, plugin, MCP or web-search tool is exposed.
        for value in (False, None):
            self.external_tools_excluded = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "external_tools_admission"):
                self.materializer()
        self.assertFalse((self.root / "dataset").exists())

    def test_shared_base_and_pillow_identity_are_required(self):
        self.runtime["images"]["judge"] = "synthetic/other@sha256:" + "a" * 64
        with self.assertRaisesRegex(ValueError, "shared_raw_judge_base"):
            self.materializer()
        self.runtime["images"]["judge"] = self.runtime["images"]["raw_only"]
        tools = base_tools_identity(self.runtime["base_tool_environment"]["provenance"])
        tools["pillow_version"] = "0.0.0"
        self.runtime["base_tool_environment"]["provenance"] = write_pin(self.root / "base-tools.json", tools)
        with self.assertRaisesRegex(ValueError, "common_base_tool_versions"):
            self.materializer()

    def test_local_image_ids_preserve_common_provenance_in_all_roles(self):
        common = "sha256:" + "a" * 64
        treatment = "sha256:" + "b" * 64
        self.runtime["images"] = {"raw_only": common, "judge": common, "gptgrep": treatment}
        self.runtime["base_tool_environment"]["image"] = common
        product_path = Path(self.runtime["gptgrep_bundle"]["provenance"]["path"])
        product = json.loads(product_path.read_text())
        product["common_image"] = common
        self.runtime["gptgrep_bundle"]["provenance"] = write_pin(product_path, product)
        provenance = self.runtime["base_tool_environment"]["provenance"]
        output = self.root / "local-images"
        result = self.materializer().run(output)
        self.assertEqual(len(result["tasks"]), 124)
        for record in result["tasks"]:
            task = output / record["task_name"]
            arm = record["binding"]["arm"]
            self.assertEqual((task / "environment/Dockerfile").read_text().splitlines()[0],
                             "FROM " + self.runtime["images"][arm])
            self.assertEqual((task / "tests/Dockerfile").read_text().splitlines()[0], "FROM " + common)
            expected = {"image": common, "provenance": {**provenance, "path": BASE_TOOLS_PATH}}
            for path in (task / "environment/reader.json", task / "tests/judge.json"):
                self.assertEqual(json.loads(path.read_text())["base_tool_environment"], expected)
        self.runtime["images"]["judge"] = treatment
        with self.assertRaisesRegex(ValueError, "shared_raw_judge_base"):
            self.materializer()

    def test_oversize_selected_question_rejects_entire_manifest_before_materialization(self):
        self.runtime["max_input_bytes"] = 1048576
        for question in ("x" * 8193, "é" * 4097):
            self.cohort["tasks"][-1]["question"] = question
            with patch.object(Full62Adapter, "_task") as materialize, \
                 patch("gptgrep_harbor.adapter.copy_asset") as asset_read:
                with self.assertRaisesRegex(ValueError, "selected_cohort_reader_question_bound"):
                    self.materializer().run(self.root / "dataset")
            materialize.assert_not_called()
            asset_read.assert_not_called()
            self.assertFalse((self.root / "dataset").exists())
            self.assertFalse((self.root / "dataset.export.json").exists())
        self.runtime["max_input_bytes"] = 1024
        self.cohort["tasks"][-1]["question"] = "x" * 1025
        with self.assertRaisesRegex(ValueError, "selected_cohort_reader_question_bound"):
            self.materializer()

    def test_raw_elf_pin_or_missing_complete_bundle_cannot_be_enrolled(self):
        self.runtime["gptgrep_binary_pin"] = {**self.runtime["gptgrep_binary_pin"], "sha256": "0" * 64}
        with self.assertRaisesRegex(ValueError, "product_entrypoint_pin"):
            self.materializer()
        del self.runtime["gptgrep_bundle"]
        with self.assertRaisesRegex(ValueError, "product_bundle_required"):
            self.materializer()

    def test_partial_cohort_and_overwrite_are_rejected_before_output(self):
        adapter = self.materializer()
        for kwargs in ({"limit": 1}, {"task_ids": ["synthetic-0"]}, {"overwrite": True}):
            with self.assertRaises(ValueError):
                adapter.run(self.root / "never-created", **kwargs)
        self.assertFalse((self.root / "never-created").exists())

    def test_changed_version_or_credential_asset_is_rejected(self):
        self.runtime["native_cli"]["version"] = "0.100.0"
        with self.assertRaisesRegex(ValueError, "native_version_pin"):
            self.materializer()
        for path in ("credentials.toml", "/raw/.env.production", "judge/reference.json", "/home/auth.json"):
            with self.assertRaisesRegex(ValueError, "private_or_evaluator_asset"):
                validate_asset_path(path)


class NativePolicyTests(unittest.TestCase):
    def test_complete_large_stdin_is_multiplexed_with_stdout_without_prompt_argv(self):
        payload = ("synthetic é prompt\n" * 20000).encode()
        self.assertGreater(len(payload), 131072)
        script = ('import sys,hashlib; sys.stdout.buffer.write(b"x"*262144); sys.stdout.buffer.flush(); '
                  'data=sys.stdin.buffer.read(); '
                  'print(str(len(data))+":"+hashlib.sha256(data).hexdigest())')
        argv = [sys.executable, "-I", "-B", "-c", script]
        self.assertLess(max(len(arg.encode()) for arg in argv), 131072)
        response = run(argv, cwd=os.environ["TMPDIR"], timeout=3.5,
                       env={"PATH": os.environ["PATH"]}, input_bytes=payload)
        self.assertIsNone(response["failure"])
        self.assertEqual(response["return_code"], 0)
        self.assertEqual(response["stdout"], b"x" * 262144 + f"{len(payload)}:{sha(payload)}\n".encode())
        self.assertEqual(response["stdin_bytes_sent"], len(payload))
        self.assertTrue(response["stdin_complete"])

    def test_blocked_stdin_obeys_original_deadline_and_cancellation(self):
        scripts = [("import time; time.sleep(10)", 0.15, "child_failed_or_limit"),
                   ("import os,signal,time; os.kill(os.getppid(),signal.SIGTERM); time.sleep(10)",
                    3.5, "cancelled")]
        for script, deadline, failure in scripts:
            with self.subTest(failure=failure):
                response = run([sys.executable, "-I", "-B", "-c", script],
                               cwd=os.environ["TMPDIR"], timeout=deadline,
                               env={"PATH": os.environ["PATH"]}, input_bytes=b"x" * MAX_NATIVE_PROMPT_BYTES)
                self.assertEqual(response["failure"], failure)
                self.assertFalse(response["stdin_complete"])
                self.assertLess(response["stdin_bytes_sent"], response["stdin_bytes"])
                self.assertIsNotNone(response["return_code"])
                self.assertLess(response["elapsed_ms"], 2000)

    def test_early_stdin_close_retains_output_and_marks_transport_incomplete(self):
        response = run([sys.executable, "-I", "-B", "-c", 'import os; os.close(0); print("receipt")'],
                       cwd=os.environ["TMPDIR"], timeout=2.5,
                       env={"PATH": os.environ["PATH"]}, input_bytes=b"x" * MAX_NATIVE_PROMPT_BYTES)
        self.assertEqual(response["failure"], "stdin_incomplete")
        self.assertFalse(response["stdin_complete"])
        self.assertEqual(response["stdout"], b"receipt\n")

    def test_bounded_child_accepts_fractional_remaining_deadline(self):
        response = run([sys.executable, "-I", "-B", "-c", 'print("synthetic child")'],
                       cwd=os.environ["TMPDIR"], timeout=2.5,
                       env={"PATH": os.environ["PATH"], "TMPDIR": os.environ["TMPDIR"]})
        self.assertEqual(response["return_code"], 0)
        self.assertIsNone(response["failure"])
        self.assertEqual(response["stdout"], b"synthetic child\n")

    def test_only_the_judge_requests_native_structured_output(self):
        judge = codex_argv("/opt/codex", profile=PROFILE, role="judge", output_schema=JUDGE_OUTPUT_SCHEMA)
        self.assertEqual(judge[-4:], ["--output-schema", JUDGE_OUTPUT_SCHEMA, "--", "-"])
        self.assertEqual(codex_argv("/opt/codex", profile=PROFILE, role="judge")[:-2], judge[:-4])
        with self.assertRaisesRegex(ValueError, "output_schema_argument"):
            codex_argv("/opt/codex", output_schema=JUDGE_OUTPUT_SCHEMA)

    def test_named_permissions_have_no_legacy_sandbox_and_deny_private_paths(self):
        argv = codex_argv("/opt/codex")
        self.assertEqual(argv[-2:], ["--", "-"])
        self.assertNotIn("--sandbox", argv)
        self.assertNotIn("--output-schema", argv)
        self.assertIn('default_permissions="full62_reader"', argv)
        self.assertIn('forced_login_method="chatgpt"', argv)
        config = tomllib.loads(model_config(PROFILE, "reader").decode())
        self.assertNotIn("sandbox_mode", config)
        self.assertEqual(config["cli_auth_credentials_store"], "file")
        fs = config["permissions"]["full62_reader"]["filesystem"]
        self.assertEqual(fs["/data/corpus"], "read")
        for path in (CODEX_HOME, "/tests", "/solution", "/logs", "/opt/full62-private"):
            self.assertEqual(fs[path], "deny")
        self.assertFalse(config["permissions"]["full62_reader"]["network"]["enabled"])
        self.assertEqual(config["web_search"], "disabled")
        self.assertEqual(config["features"], {
            "shell_tool": True, "view_image": True, "unified_exec": True,
            "multi_agent": True, "plugins": False, "apps": False,
            "tool_suggest": False, "code_mode": {"enabled": True}, "code_mode_host": {"enabled": True}})
        judge = tomllib.loads(model_config(PROFILE, "judge").decode())
        self.assertEqual(judge["web_search"], "disabled")
        self.assertFalse(judge["permissions"]["full62_judge"]["network"]["enabled"])
        self.assertEqual(judge["features"], {
            "shell_tool": False, "view_image": False, "code_mode_only": False, "code_mode_prewarm": False,
            "apps": False, "plugins": False, "tool_suggest": False,
            "code_mode": {"enabled": False, "direct_only_tool_namespaces": []},
            "code_mode_host": {"enabled": False, "disable_in_process_fallback": True}})

    def test_product_credential_never_reaches_native_environment(self):
        source = {"PATH": "/usr/bin", "OPENROUTER_API_KEY": "synthetic-only", "HTTPS_PROXY": "http://synthetic-proxy",
                  "BASH_ENV": "/unsafe/startup", "CODEX_HOME": "/wrong-home",
                  "GPTGREP_DECISIONS_ATTACH_PLAN": "/private/plan.json",
                  "GPTGREP_DECISIONS_ATTACH_PLAN_SHA256": "synthetic-pin"}
        native = native_environment(source)
        self.assertNotIn("OPENROUTER_API_KEY", native)
        self.assertNotIn("BASH_ENV", native)
        self.assertNotIn("GPTGREP_DECISIONS_ATTACH_PLAN", native)
        self.assertNotIn("GPTGREP_DECISIONS_ATTACH_PLAN_SHA256", native)
        self.assertEqual(native["CODEX_HOME"], CODEX_HOME)
        self.assertEqual(native["HTTPS_PROXY"], source["HTTPS_PROXY"])
        self.assertEqual(native_environment(source, product=True)["OPENROUTER_API_KEY"], "synthetic-only")
        self.assertEqual(native["CODEX_REFRESH_TOKEN_URL_OVERRIDE"], REFRESH_BLOCKER_URL)
        for key in ("CODEX_API_KEY", "OPENAI_API_KEY", "OPENAI_BASE_URL"):
            with self.assertRaisesRegex(ValueError, "api_auth_environment_forbidden"):
                native_environment({key: "synthetic-only"})

    def test_every_native_child_receives_the_refresh_blocker_and_loopback_no_proxy(self):
        source = {"PATH": "/usr/bin", "NO_PROXY": "corp.example.invalid", "no_proxy": "lower.example.invalid",
                  "HTTPS_PROXY": "http://synthetic-proxy:3128",
                  "CODEX_REFRESH_TOKEN_URL_OVERRIDE": "https://synthetic-elsewhere.invalid/oauth/token"}
        # Port 0: no process in the container (tool commands share its network namespace) can listen there.
        self.assertEqual(REFRESH_BLOCKER_URL, "http://127.0.0.1:0/oauth/token")
        self.assertEqual(REVOKE_BLOCKER_URL, "http://127.0.0.1:0/oauth/revoke")
        for product in (False, True):
            native = native_environment(source, product=product)
            self.assertEqual(native["CODEX_REFRESH_TOKEN_URL_OVERRIDE"], REFRESH_BLOCKER_URL)
            self.assertEqual(native["CODEX_REVOKE_TOKEN_URL_OVERRIDE"], REVOKE_BLOCKER_URL)
            self.assertEqual(native["NO_PROXY"], native["no_proxy"])
            entries = native["NO_PROXY"].split(",")
            for entry in ("corp.example.invalid", "lower.example.invalid", "127.0.0.1", "localhost", "::1"):
                self.assertIn(entry, entries)
            self.assertEqual(native["HTTPS_PROXY"], source["HTTPS_PROXY"])
        bare = native_environment({"PATH": "/usr/bin"})
        self.assertEqual(bare["NO_PROXY"], "127.0.0.1,localhost,::1")
        self.assertEqual(bare["CODEX_REFRESH_TOKEN_URL_OVERRIDE"], REFRESH_BLOCKER_URL)

    def test_auth_compose_labels_every_trial_with_the_job_id(self):
        compose = auth_compose("/synthetic/accounts/dev/auth.json", "linux/amd64", "synthetic-full62")
        self.assertEqual(compose["services"]["main"]["labels"], {"gptgrep.auth-job": "synthetic-full62"})
        for bad in ("", "x" * 81, "has space", "label=x", None):
            with self.assertRaisesRegex(ValueError, "auth_job_label"):
                auth_compose("/synthetic/accounts/dev/auth.json", "linux/amd64", bad)

    def test_product_wrapper_strips_openrouter_and_rejects_legacy_sandbox(self):
        config = {"codex_binary": "/opt/codex", "reader_base_binary": {"path": "/opt/codex"}}
        with patch("gptgrep_harbor.runtime.read", return_value=canonical(config)), \
             patch("gptgrep_harbor.runtime.check_config"), \
             patch("gptgrep_harbor.common.copy_asset"), \
             patch.dict(os.environ, {"OPENROUTER_API_KEY": "synthetic-only",
                                    "GPTGREP_DECISIONS_ATTACH_PLAN": "/private/plan.json",
                                    "GPTGREP_DECISIONS_ATTACH_PLAN_SHA256": "synthetic-pin"}, clear=True), \
             patch("sys.argv", ["codex-native", "app-server"]), \
             patch("os.execve") as execute:
            native_wrapper()
            binary, argv, env = execute.call_args.args
            self.assertEqual(binary, "/opt/codex")
            self.assertEqual(argv[-1], "app-server")
            self.assertNotIn("OPENROUTER_API_KEY", env)
            self.assertNotIn("GPTGREP_DECISIONS_ATTACH_PLAN", env)
            self.assertNotIn("GPTGREP_DECISIONS_ATTACH_PLAN_SHA256", env)
            self.assertEqual(env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"], REFRESH_BLOCKER_URL)
            execute.reset_mock()
            with patch("sys.argv", ["codex-native", "exec", "--sandbox", "read-only"]):
                with self.assertRaisesRegex(ValueError, "legacy_sandbox_override_forbidden"):
                    native_wrapper()
            execute.assert_not_called()
            with patch("sys.argv", ["codex-native", "app-server", "-c", 'default_permissions="unsafe"']):
                with self.assertRaisesRegex(ValueError, "native_policy_override_forbidden"):
                    native_wrapper()
            execute.assert_not_called()

    def test_decisions_budget_is_global_and_never_added_to_bare_reader(self):
        config = {"gptgrep_binary": "/opt/gptgrep", "codex_binary": "/opt/codex", "codex_home": CODEX_HOME,
                  "raw_document": "/data/corpus/generic.txt", "jev_model": "synthetic", "deadline_seconds": 20,
                  "max_tool_calls": 8, "max_input_bytes": 8192, "reader_mode": "rich_multimodal",
                  "native_ptc": True, "codex_permissions": "full62_reader",
                  "source_relative_path": "generic-source.txt"}
        argv = reader_argv(config, {"arm": "gptgrep", "document_id": "generic"}, "generic question")
        self.assertEqual(argv[:4], ["/opt/gptgrep", "--decisions-budget", DECISIONS_BUDGET, "ask"])
        self.assertIn("--multimodal", argv)
        self.assertEqual(argv.count("--native-ptc"), 1)
        self.assertIn("--document=generic-source.txt", argv)
        self.assertNotIn("--document=generic", argv)
        profile = tomllib.loads(model_config(PROFILE, "reader").decode())["default_permissions"]
        self.assertEqual(argv[argv.index("--codex-permissions") + 1], profile)
        bare = reader_argv(config, {"arm": "raw_only"}, "generic question")
        self.assertEqual(bare, codex_argv(config["codex_binary"]))
        self.assertNotIn("--native-ptc", bare)
        self.assertNotIn("--decisions-budget", bare)
        self.assertNotIn("--codex-permissions", bare)
        self.assertIn("default_permissions=" + json.dumps(profile), bare)
        for key, value in (("native_ptc", False), ("native_ptc", None), ("codex_permissions", "other")):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "shared_native_reader_required"):
                reader_argv({**config, key: value}, {"arm": "gptgrep"}, "generic question")

    def test_full_instruction_cap_is_checked_before_argv_for_both_arms(self):
        for arm in ("gptgrep", "raw_only"):
            with self.assertRaisesRegex(ValueError, "instruction_bytes_limit"):
                reader_argv({"max_input_bytes": 3}, {"arm": arm}, "four")
            with self.assertRaisesRegex(ValueError, "instruction_bytes_limit"):
                reader_argv({"max_input_bytes": 1048576}, {"arm": arm}, "é" * 4097)

    def test_rich_success_preserves_typed_citations_and_is_never_parsed_as_legacy(self):
        report = {"schema_version": "gptgrep.rich_multimodal.host.v1", "answer": "complete synthetic answer",
                  "insufficient_evidence": False, "citations": [{"type": "visual", "artifact_sha256": "a" * 64,
                      "page": 1, "geometry": [0, 0, 10, 10]}],
                  "initial_retrieval": {"generation": {"manifest_sha256": "b" * 64},
                      "attempts": [{"usage": {"cost": 0.1}}, {"usage": {"cost": 0.2}}]},
                  "model_attempts": [{"requested_model": PROFILE["model"],
                      "requested_reasoning_effort": "max", "requested_service_tier": "priority",
                      "model": PROFILE["model"], "effective_service_tier": "priority",
                      "native_ptc": {"enabled": True, "command_executions": 2, "command_failures": 1,
                                     "view_image_calls": 1, "payloads_retained": False}}],
                  "model_usage": {"attempted_calls": 1}, "tool_calls": 3}
        answer, accounting = product_result(canonical(report), "rich_multimodal")
        self.assertEqual(answer, report["answer"])
        self.assertEqual(accounting["citations"], report["citations"])
        self.assertEqual(accounting["model_attempts"], report["model_attempts"])
        self.assertEqual(accounting["tool_calls"], 3)
        self.assertEqual(accounting["initial_retrieval"], report["initial_retrieval"])
        self.assertAlmostEqual(accounting["reported_retrieval_cost_subtotal"], 0.3)
        self.assertFalse(accounting["cost_complete"])
        with self.assertRaisesRegex(ValueError, "product_not_completed"):
            product_result(canonical(report), "legacy_text")
        with self.assertRaisesRegex(ValueError, "rich_product_schema"):
            product_result(canonical({"status": "completed", "answer": "legacy"}), "rich_multimodal")
        partial = {"schema_version": "gptgrep.error.v1", "multimodal_host": report}
        self.assertEqual(product_accounting(partial)["model_attempts"], report["model_attempts"])
        self.assertEqual(product_accounting(partial)["model_attempts"][0]["native_ptc"]["command_failures"], 1)


class AgentInterfaceTests(unittest.IsolatedAsyncioTestCase):
    async def test_real_baseagent_setup_and_run_use_harbor_interfaces(self):
        with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as directory:
            agent = PairedNativeAgent(logs_dir=Path(directory).resolve(), model_name=PROFILE["model"])
            environment = SimpleNamespace(exec=AsyncMock(return_value=SimpleNamespace(return_code=0)),
                                          upload_file=AsyncMock())
            await agent.setup(environment)
            context = AgentContext()
            await agent.run("generic synthetic question", environment, context)
            self.assertEqual(context.metadata["reader_exit"], 0)
            self.assertEqual(environment.upload_file.call_count, 1)
            self.assertEqual(environment.exec.call_args.kwargs["cwd"], "/data/corpus")

    async def test_agent_refuses_unmatched_extensions_before_container_command(self):
        agent = PairedNativeAgent(logs_dir=Path(os.environ["TMPDIR"]), model_name=PROFILE["model"], skills_dir="/extra")
        environment = SimpleNamespace(exec=AsyncMock())
        with self.assertRaisesRegex(ValueError, "unmatched_agent_extension"):
            await agent.setup(environment)
        environment.exec.assert_not_called()


if __name__ == "__main__":
    unittest.main()
