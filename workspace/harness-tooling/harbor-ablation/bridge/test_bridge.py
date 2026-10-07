import argparse
import json
import subprocess
import sys
import stat
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

import format_bridge as bridge
import synthetic
from verify_answer import admit_answer, equivalence_reward

EXPORTER = None
SCRATCH = None


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


if __name__ == "__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--exporter",required=True);parser.add_argument("--scratch",required=True)
    args=parser.parse_args();EXPORTER=str(Path(args.exporter).resolve());SCRATCH=str(Path(args.scratch).resolve())
    unittest.main(argv=[sys.argv[0]],verbosity=2)
