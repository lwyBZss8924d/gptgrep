import argparse
import json
import subprocess
import sys
import stat
import shutil
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

import format_bridge as bridge
import synthetic
from verify_answer import admit_answer, equivalence_reward

EXPORTER = None
SCRATCH = None
PRODUCTION_FIXTURE = None


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=SCRATCH)
        self.root = Path(self.temp.name)
        self.input = synthetic.fixture(self.root/"inputs")
        self.readers = self.root/"readers"
        self.graders = self.root/"graders"
        self.package = self.root/"package"
        self.command("export-readers", "--corpus", self.input/"corpus.json", "--readers", self.input/"readers.json",
                     "--options", self.input/"options.json", "--output-dir", self.readers)
        grader = synthetic.grader_projection(self.input, self.readers)
        self.command("export-verifier", "--graders", grader, "--reader-export", self.readers, "--output-dir", self.graders)

    def tearDown(self):
        self.temp.cleanup()

    def command(self, *args):
        result = subprocess.run([EXPORTER, *map(str,args)], capture_output=True, text=True, check=False, timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)

    def compose(self):
        return bridge.compose(self.readers,self.graders,self.package)

    def test_typed_harbor_tasks_datasets_and_native_digests(self):
        result=self.compose();self.assertEqual(result["task_count"],4);self.assertFalse(result["execution_qualified"])
        self.assertEqual(bridge.validate(self.package),result)
        for arm in ("control","gptgrep"):
            config=bridge.TaskConfig.model_validate_toml((self.package/arm/"task-1/task.toml").read_text())
            self.assertEqual(config.schema_version,"1.4")
            self.assertEqual(config.artifacts[0].source,"/app/answer.json")
            self.assertEqual(config.artifacts[0].destination,"answer.json")
            self.assertEqual(config.verifier.environment_mode.value,"separate")

    def test_gold_canary_only_verifier_not_reader_build_context(self):
        self.compose()
        for arm in ("control","gptgrep"):
            task=self.package/arm/"task-1"
            self.assertIn(b"SYNTHETIC_JUDGE_ONLY_CANARY",(task/"tests/reference.json").read_bytes())
            for file in (task/"environment").rglob("*"):
                if file.is_file():self.assertNotIn(b"SYNTHETIC_JUDGE_ONLY_CANARY",file.read_bytes())
            self.assertNotIn("tests",(task/"environment/Dockerfile").read_text())
            definition=bridge.resolve_verifier_environment_definition(bridge.Task(task).config,bridge.Task(task).paths)
            self.assertEqual(definition.directory,task/"tests")

    def test_baseline_raw_only_and_same_bytes_and_config(self):
        self.compose()
        control=self.package/"control/task-1/environment";treatment=self.package/"gptgrep/task-1/environment"
        self.assertFalse((control/"prebuilt").exists());self.assertFalse((control/"tool").exists())
        self.assertEqual((control/"raw/doc-a.pdf").read_bytes(),(treatment/"raw/doc-a.pdf").read_bytes())
        self.assertEqual((control/"codex-config.json").read_bytes(),(treatment/"codex-config.json").read_bytes())

    def test_symlink_source_export_refused_before_output(self):
        path=self.readers/"control/task-1/environment/raw/doc-a.pdf"
        path.unlink();path.symlink_to(self.input/"document.pdf")
        with self.assertRaises(ValueError):self.compose()
        self.assertFalse(self.package.exists())

    def test_digest_tamper_refused_before_output(self):
        (self.graders/"task-1/reference.json").write_text("{}")
        with self.assertRaisesRegex(ValueError,"inventory"):self.compose()
        self.assertFalse(self.package.exists())

    def test_cross_role_manifest_binding_refused(self):
        path=self.graders/"export-manifest.json";value=json.loads(path.read_text());value["corpus_sha256"]="e"*64
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError,"role_binding"):self.compose()

    def test_unknown_schema_and_traversal_refused(self):
        for path in ("../outside","/outside","a\\b","a/./b"):
            with self.assertRaises(ValueError):bridge.relative(path)
        manifest=self.readers/"export-manifest.json";value=json.loads(manifest.read_text());value["reference_answer"]="canary"
        manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError,"schema"):self.compose()

    def test_no_overwrite_and_added_nested_receipt_not_ignored(self):
        self.compose()
        with self.assertRaises(FileExistsError):self.compose()
        (self.package/"control/task-1/environment/package-receipt.json").write_text("extra")
        with self.assertRaisesRegex(ValueError,"inventory"):bridge.validate(self.package)

    def test_unavailable_judge_does_not_emit_reward(self):
        self.compose()
        answer=self.root/"answer.json";answer.write_text('{"answer":"copper"}')
        output=self.root/"status.json"
        result=subprocess.run([sys.executable,"-B",str(Path(bridge.__file__).parent/"verify_answer.py"),
            "--answer",str(answer),"--reference",str(self.package/"control/task-1/tests/reference.json"),"--output",str(output)],capture_output=True,timeout=30)
        self.assertEqual(result.returncode,75)
        status=json.loads(output.read_text());self.assertEqual(status["verification_status"],"unavailable")
        self.assertFalse(status["reward_emitted"]);self.assertFalse((self.root/"reward.txt").exists())
        self.assertIsNone(equivalence_reward("unavailable"));self.assertEqual(equivalence_reward("correct"),1.0)
        self.assertEqual(equivalence_reward("incorrect"),0.0)

    def test_answer_artifact_contract_bounded(self):
        self.assertEqual(admit_answer({"answer":"value"}),"value")
        for value in ({"answer":""},{"answer":3},{"answer":"a"*12001},{"answer":"value","reward":1}):
            with self.assertRaises(ValueError):admit_answer(value)

    def test_private_composed_modes_under_owned_subprocess_umasks(self):
        parent=self.root/"caller-parent";parent.mkdir(mode=0o755);parent.chmod(0o755)
        code="import os,sys;sys.path.insert(0,sys.argv[1]);import format_bridge;os.umask(int(sys.argv[2],8));format_bridge.compose(sys.argv[3],sys.argv[4],sys.argv[5])"
        for mask in (0o000,0o777):
            output=parent/f"package-{mask:o}"
            result=subprocess.run([sys.executable,"-B","-c",code,str(Path(bridge.__file__).parent),f"{mask:o}",
                str(self.readers),str(self.graders),str(output)],capture_output=True,text=True,timeout=30)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(stat.S_IMODE(parent.stat().st_mode),0o755)
            self.assertEqual(stat.S_IMODE(output.stat().st_mode),0o700)
            for path in output.rglob("*"):
                self.assertEqual(stat.S_IMODE(path.stat().st_mode),0o700 if path.is_dir() else 0o600,str(path.relative_to(output)))
            for name in ("control/task-1/tests/reference.json","control/task-1/task.toml","control/dataset.toml","package-receipt.json"):
                self.assertEqual(stat.S_IMODE((output/name).stat().st_mode),0o600)

    def test_changed_reader_inventory_cannot_reuse_prior_grader_binding(self):
        name="control/task-1/environment/Dockerfile";path=self.readers/name
        path.write_bytes(path.read_bytes()+b"\n# exact-binding discriminator\n")
        manifest=self.readers/"export-manifest.json";value=json.loads(manifest.read_text())
        grader=json.loads((self.graders/"export-manifest.json").read_text())
        for key in ("corpus_sha256","reader_projection_sha256","options_sha256","task_bindings"):
            self.assertEqual(value[key],grader[key])
        value["files"][name]=bridge.digest(path.read_bytes());manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError,"reader_export_digest_binding"):self.compose()
        self.assertFalse(self.package.exists())

    def test_manifest_swap_after_admission_cannot_reuse_prior_grader_binding(self):
        manifest=self.readers/"export-manifest.json";prior=manifest.read_bytes()
        name="control/task-1/environment/Dockerfile";path=self.readers/name
        path.write_bytes(path.read_bytes()+b"\n# snapshot-binding discriminator\n")
        value=json.loads(prior);value["files"][name]=bridge.digest(path.read_bytes())
        manifest.write_text(json.dumps(value))
        read_export=bridge.read_export;swapped=[]
        def admit_then_swap(root,role):
            snapshot=read_export(root,role)
            if role=="reader":
                manifest.write_bytes(prior);swapped.append(True)
            return snapshot
        with patch.object(bridge,"read_export",side_effect=admit_then_swap):
            with self.assertRaisesRegex(ValueError,"reader_export_digest_binding"):self.compose()
        self.assertEqual(swapped,[True]);self.assertFalse(self.package.exists())

    def test_receipt_keeps_manifest_snapshots_when_paths_change_after_admission(self):
        expected={role:bridge.digest((root/"export-manifest.json").read_bytes())
                  for role,root in (("reader",self.readers),("grader",self.graders))}
        read_export=bridge.read_export;swapped=[]
        def admit_then_swap(root,role):
            snapshot=read_export(root,role)
            if role=="grader":
                for directory in (self.readers,self.graders):
                    (directory/"export-manifest.json").write_bytes(b"{}")
                swapped.append(True)
            return snapshot
        with patch.object(bridge,"read_export",side_effect=admit_then_swap):
            result=self.compose()
        self.assertEqual(swapped,[True]);self.assertEqual(result["task_count"],4)
        receipt=json.loads((self.package/"package-receipt.json").read_bytes())
        for role in expected:self.assertEqual(receipt[f"{role}_manifest_sha256"],expected[role])


class ProductionPackagingTests(unittest.TestCase):
    def setUp(self):
        if PRODUCTION_FIXTURE is None:
            self.skipTest("explicit generic Writer/Reader fixture not supplied")
        self.temp = tempfile.TemporaryDirectory(dir=SCRATCH)
        self.root = Path(self.temp.name)
        self.readers = self.root / "readers"
        self.graders = self.root / "graders"
        self.package = self.root / "package"
        shutil.copytree(Path(PRODUCTION_FIXTURE) / "readers", self.readers)
        shutil.copytree(Path(PRODUCTION_FIXTURE) / "graders", self.graders)

    def tearDown(self):
        if hasattr(self, "temp"):
            self.temp.cleanup()

    def compose(self):
        return bridge.compose(self.readers, self.graders, self.package)

    def refresh(self, root, name):
        path = root / "export-manifest.json"
        value = json.loads(path.read_bytes())
        value["files"][name] = bridge.digest((root / name).read_bytes())
        path.write_text(json.dumps(value))

    def fresh_case(self, label):
        directory = self.root / label
        directory.mkdir()
        self.readers = directory / "readers"
        self.graders = directory / "graders"
        self.package = directory / "package"
        shutil.copytree(Path(PRODUCTION_FIXTURE) / "readers", self.readers)
        shutil.copytree(Path(PRODUCTION_FIXTURE) / "graders", self.graders)

    def rebind_reader(self):
        """Refresh all role hashes so a prior expected-export digest cannot mask this seam."""
        reader = json.loads((self.readers / "export-manifest.json").read_bytes())
        path = self.graders / "export-manifest.json"
        grader = json.loads(path.read_bytes())
        for key in ("corpus_sha256", "reader_projection_sha256", "options_sha256", "task_bindings",
                    "source_mode", "qualification", "group_bindings"):
            grader[key] = reader[key]
        grader["reader_export_sha256"] = bridge.digest((self.readers / "export-manifest.json").read_bytes())
        path.write_text(json.dumps(grader))

    def tamper_reader_payload(self, name, data):
        (self.readers / name).write_bytes(data)
        self.refresh(self.readers, name)
        self.rebind_reader()

    def test_self_consistent_one_arm_config_change_is_denied_before_output(self):
        name = "control/task-a/environment/codex-config.json"
        config = json.loads((self.readers / name).read_bytes())
        config["model"] = "MOCK_CHANGED_READER_MODEL"
        self.tamper_reader_payload(name, json.dumps(config).encode())
        with self.assertRaisesRegex(ValueError, "production_config_difference"):
            self.compose()
        self.assertFalse(self.package.exists())

    def test_self_consistent_actual_instruction_change_is_denied_before_output(self):
        self.tamper_reader_payload("control/task-a/instruction.md",
                                   b"Changed ordinary prompt; do not use the bound question.\n")
        with self.assertRaisesRegex(ValueError, "production_instruction_binding"):
            self.compose()
        self.assertFalse(self.package.exists())

    def test_self_consistent_reader_input_changes_and_gold_extension_are_denied(self):
        mutations = {
            "question": lambda value: value.update(question="Changed ordinary question"),
            "question_and_hash": lambda value: value.update(question="Changed ordinary question",
                question_sha256=bridge.digest(b"Changed ordinary question")),
            "task": lambda value: value.update(task_id="different-task"),
            "scope": lambda value: value.update(group_scope=[]),
            "arm": lambda value: value.update(arm="gptgrep"),
            "schema": lambda value: value.update(schema_version="different-schema"),
            "gold_extension": lambda value: value.update(reference_answer="MOCK_JUDGE_ONLY_CANARY"),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label):
                self.fresh_case(f"input-{label}")
                name = "control/task-a/environment/reader-input.json"
                value = json.loads((self.readers / name).read_bytes())
                mutation(value)
                self.tamper_reader_payload(name, json.dumps(value).encode())
                with self.assertRaisesRegex(ValueError, "production_reader_input_binding"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_production_manifest_duplicate_and_nonfinite_values_are_denied(self):
        cases = [(role, token) for role in ("reader", "grader")
                 for token in ("duplicate", "NaN", "Infinity", "-Infinity", "1e999")]
        for role, token in cases:
            with self.subTest(role=role, token=token):
                self.fresh_case(f"manifest-{role}-{token}")
                path = (self.readers if role == "reader" else self.graders) / "export-manifest.json"
                data = path.read_text()
                if token == "duplicate":
                    data = data.rstrip()[:-1] + f',"role":"{role}"' + "}"
                else:
                    value = json.loads(data)
                    value["group_bindings"][0]["generation_bytes"] = "PLACEHOLDER"
                    data = json.dumps(value).replace('"PLACEHOLDER"', token)
                path.write_text(data)
                if role == "reader":
                    self.rebind_reader()
                with self.assertRaisesRegex(ValueError, "production_json_(duplicate_key|nonfinite)"):
                    self.compose()
                self.assertFalse(self.package.exists())
        self.fresh_case("manifest-schema-shadow")
        path = self.readers / "export-manifest.json"
        path.write_text(path.read_text().rstrip()[:-1] + ',"schema_version":"1"}')
        with self.assertRaisesRegex(ValueError, "production_json_duplicate_key"):
            self.compose()
        self.assertFalse(self.package.exists())

    def test_production_options_duplicate_and_nonfinite_values_are_denied(self):
        for token in ("duplicate", "NaN", "Infinity", "-Infinity", "1e999"):
            with self.subTest(token=token):
                self.fresh_case(f"options-{token}")
                data = (self.readers / "frozen-options.json").read_text()
                if token == "duplicate":
                    data = data.rstrip()[:-1] + ',"model":"gpt-6-luna"}'
                else:
                    value = json.loads(data)
                    value["codex_argv"].append("PLACEHOLDER")
                    data = json.dumps(value).replace('"PLACEHOLDER"', token)
                encoded = data.encode()
                # Make every payload/options/role binding self-consistent apart from invalid JSON.
                for name in ("frozen-options.json", "control/task-a/environment/codex-config.json",
                             "gptgrep/task-a/environment/codex-config.json"):
                    (self.readers / name).write_bytes(encoded)
                    self.refresh(self.readers, name)
                for arm in ("control", "gptgrep"):
                    name = f"{arm}/task-a/task-descriptor.json"
                    descriptor = json.loads((self.readers / name).read_bytes())
                    descriptor["options"] = json.loads(data)
                    (self.readers / name).write_text(json.dumps(descriptor))
                    self.refresh(self.readers, name)
                manifest = self.readers / "export-manifest.json"
                value = json.loads(manifest.read_bytes())
                value["options_sha256"] = bridge.digest(encoded)
                manifest.write_text(json.dumps(value))
                self.rebind_reader()
                with self.assertRaisesRegex(ValueError, "production_json_(duplicate_key|nonfinite)"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_production_reference_duplicate_and_nested_nonfinite_answer_denied(self):
        for token in ("duplicate", "NaN", "Infinity", "-Infinity", "1e999"):
            with self.subTest(token=token):
                self.fresh_case(f"reference-{token}")
                name = "task-a/reference.json"
                value = json.loads((self.graders / name).read_bytes())
                if token == "duplicate":
                    data = json.dumps(value)[:-1] + ',"answer":' + json.dumps(value["answer"]) + "}"
                else:
                    value["answer"] = {"nested": [{"number": "PLACEHOLDER"}]}
                    data = json.dumps(value).replace('"PLACEHOLDER"', token)
                (self.graders / name).write_text(data)
                self.refresh(self.graders, name)
                with self.assertRaisesRegex(ValueError, "production_json_(duplicate_key|nonfinite)"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_production_reader_input_duplicate_and_nonfinite_values_denied(self):
        for token in ("duplicate", "NaN", "Infinity", "-Infinity", "1e999"):
            with self.subTest(token=token):
                self.fresh_case(f"reader-json-{token}")
                name = "control/task-a/environment/reader-input.json"
                value = json.loads((self.readers / name).read_bytes())
                if token == "duplicate":
                    data = json.dumps(value)[:-1] + ',"question":' + json.dumps(value["question"]) + "}"
                else:
                    value["question"] = "PLACEHOLDER"
                    data = json.dumps(value).replace('"PLACEHOLDER"', token)
                self.tamper_reader_payload(name, data.encode())
                with self.assertRaisesRegex(ValueError, "production_json_(duplicate_key|nonfinite)"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_native_structure_preserved_and_baseline_has_only_whole_raw_group(self):
        self.assertEqual(self.compose()["task_count"], 2)
        reader = json.loads((self.readers / "export-manifest.json").read_bytes())
        group = reader["group_bindings"][0]
        for arm in ("control", "gptgrep"):
            task = self.package / arm / "task-a"
            cfg = bridge.TaskConfig.model_validate_toml((task / "task.toml").read_text())
            self.assertFalse(cfg.metadata["synthetic"])
            self.assertFalse(cfg.metadata["execution_qualified"])
            self.assertEqual(cfg.verifier.environment_mode.value, "separate")
            raw = task / "environment/groups/group-a/raw"
            self.assertEqual((raw / "nested/source.pdf").read_bytes(),
                             (self.readers / arm / "task-a/environment/groups/group-a/raw/nested/source.pdf").read_bytes())
            self.assertIn(group["canonical_root"], (task / "environment/Dockerfile").read_text())
            self.assertEqual((raw / ".gptgrep").exists(), arm == "gptgrep")
            if arm == "gptgrep":
                generation = raw / ".gptgrep/generations" / group["pin"]["generation"]
                for item in group["generation_inventory"]:
                    self.assertEqual(bridge.digest((generation / item["path"]).read_bytes()), item["sha256"])
            reference = (task / "tests/reference.json").read_bytes()
            self.assertIn(b"MOCK_JUDGE_ONLY_CANARY", reference)
            for path in (task / "environment").rglob("*"):
                if path.is_file():
                    self.assertNotIn(b"MOCK_JUDGE_ONLY_CANARY", path.read_bytes())
        self.assertEqual(bridge.validate(self.package)["task_count"], 2)

    def test_production_unknown_baseline_payload_refused_before_output(self):
        name = "control/task-a/environment/extra-preparsed.txt"
        (self.readers / name).write_bytes(b"unexpected parsed text")
        self.refresh(self.readers, name)
        with self.assertRaisesRegex(ValueError, "exact_reader_namespace"):
            self.compose()
        self.assertFalse(self.package.exists())

    def test_production_reference_null_and_schema_extension_refused(self):
        name = "task-a/reference.json"
        reference = json.loads((self.graders / name).read_bytes())
        reference["answer"] = None
        (self.graders / name).write_text(json.dumps(reference))
        self.refresh(self.graders, name)
        with self.assertRaisesRegex(ValueError, "reference_contract"):
            self.compose()
        self.assertFalse(self.package.exists())

    def test_held_source_change_before_publication_has_no_output(self):
        original = bridge.private_new
        def change_then_publish(output, files):
            raw = self.readers / "control/task-a/environment/groups/group-a/raw/nested/source.pdf"
            raw.write_bytes(b"%PDF-changed after admission")
            return original(output, files)
        with patch.object(bridge, "private_new", side_effect=change_then_publish):
            with self.assertRaises(ValueError):
                self.compose()
        self.assertFalse(self.package.exists())

    def test_production_private_modes_and_unavailable_judge(self):
        code = "import os,sys;sys.path.insert(0,sys.argv[1]);import format_bridge;os.umask(int(sys.argv[2],8));format_bridge.compose(sys.argv[3],sys.argv[4],sys.argv[5])"
        for mask in (0o000, 0o777):
            output = self.root / f"package-{mask:o}"
            result = subprocess.run([sys.executable, "-B", "-c", code, str(Path(bridge.__file__).parent),
                                     f"{mask:o}", str(self.readers), str(self.graders), str(output)],
                                    capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            for path in [output, *output.rglob("*")]:
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)
            status = json.loads((output / "format-status.json").read_bytes())
            self.assertEqual(status["judge_status"], "unavailable")
            self.assertIsNone(status["reward"])
        answer = self.root / "answer.json"
        answer.write_text('{"answer":"ordinary candidate"}')
        status_path = self.root / "verification-status.json"
        result = subprocess.run([sys.executable, "-B", str(Path(bridge.__file__).parent / "verify_answer.py"),
                                 "--answer", str(answer), "--reference",
                                 str(output / "control/task-a/tests/judge-status.json"),
                                 "--output", str(status_path)], capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertFalse(json.loads(status_path.read_bytes())["reward_emitted"])
        self.assertFalse((self.root / "reward.txt").exists())

    def runtime_enabled(self):
        options = json.loads((self.readers / "frozen-options.json").read_bytes())
        if options.get("runtime_debian_packages") is None:
            self.skipTest("explicit runtime-DEB production fixture not selected")
        return options

    def test_runtime_debs_compose_symmetrically_into_real_task_configs(self):
        self.runtime_enabled()
        self.assertEqual(self.compose()["task_count"], 2)
        contexts = []
        for arm in ("control", "gptgrep"):
            task = self.package / arm / "task-a"
            cfg = bridge.TaskConfig.model_validate_toml((task / "task.toml").read_text())
            self.assertFalse(cfg.metadata["execution_qualified"])
            self.assertEqual(cfg.environment.network_mode.value, "no-network")
            self.assertEqual(cfg.agent.network_mode.value, "no-network")
            self.assertEqual(cfg.verifier.environment_mode.value, "separate")
            runtime = task / "environment/runtime-debian"
            contexts.append({str(p.relative_to(runtime)): p.read_bytes()
                             for p in runtime.rglob("*") if p.is_file()})
            self.assertFalse((task / "environment/frozen-runtime-debian-inputs.json").exists())
        self.assertEqual(*contexts)
        self.assertFalse((self.package / "control/task-a/environment/groups/group-a/raw/.gptgrep").exists())
        self.assertEqual(bridge.validate(self.package)["task_count"], 2)

    def test_extra_run_and_root_user_cannot_refresh_inventory_and_role_hashes(self):
        for index, command in enumerate((b"RUN touch /unrequested-command\n", b"USER 0:0\n")):
            with self.subTest(command=command):
                self.fresh_case(f"extra-docker-{index}")
                name = "control/task-a/environment/Dockerfile"
                self.tamper_reader_payload(name, (self.readers / name).read_bytes() + command)
                with self.assertRaisesRegex(ValueError, "production_reader_docker_binding"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_runtime_package_license_and_sidecar_tamper_refuse_before_compose(self):
        self.runtime_enabled()
        names = ("debs/package-0000.deb", "licenses/package-0000.txt", "manifest.json", "checksums.sha256")
        for index, suffix in enumerate(names):
            with self.subTest(suffix=suffix):
                self.fresh_case(f"runtime-tamper-{index}")
                name = f"control/task-a/environment/runtime-debian/{suffix}"
                self.tamper_reader_payload(name, b"rewritten runtime input")
                with self.assertRaisesRegex(ValueError, "production_runtime_(asset|mapping)_binding"):
                    self.compose()
                self.assertFalse(self.package.exists())

    def test_runtime_frozen_input_hash_and_unknown_payload_remain_bound(self):
        self.runtime_enabled()
        self.tamper_reader_payload("frozen-runtime-debian-inputs.json", b"{}")
        with self.assertRaisesRegex(ValueError, "production_runtime_manifest_binding"):
            self.compose()
        self.assertFalse(self.package.exists())
        self.fresh_case("extra-runtime-file")
        name = "control/task-a/environment/runtime-debian/unrequested.txt"
        (self.readers / name).write_bytes(b"unexpected payload")
        self.refresh(self.readers, name)
        self.rebind_reader()
        with self.assertRaisesRegex(ValueError, "production_exact_reader_namespace"):
            self.compose()
        self.assertFalse(self.package.exists())


if __name__ == "__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--exporter",required=True);parser.add_argument("--scratch",required=True)
    parser.add_argument("--production-fixture")
    args=parser.parse_args();EXPORTER=str(Path(args.exporter).resolve());SCRATCH=str(Path(args.scratch).resolve())
    PRODUCTION_FIXTURE = args.production_fixture
    unittest.main(argv=[sys.argv[0]],verbosity=2)
