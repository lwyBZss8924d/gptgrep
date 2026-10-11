"""Local alias/build custody with synthetic engine responses; no Docker/model calls."""
from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from harbor.environments.definition import should_use_prebuilt_docker_image
from harbor.models.task.config import TaskConfig
from harbor.models.task.paths import TaskPaths
from harbor.models.task.verifier_mode import resolve_verifier_environment_definition

from gptgrep_harbor.common import canonical, sha
from gptgrep_harbor.local_images import LocalImageBuilder, build_dataset, chain_id, context_rows, file_pin
import test_harbor_runtime as fixtures


BASE = "sha256:" + "a" * 64
DERIVED = "sha256:" + "b" * 64
LAYERS = ["sha256:" + "c" * 64, "sha256:" + "e" * 64]


class FakeEngine:
    def __init__(self, fault=None):
        self.images = {BASE: {"Id": BASE, "Architecture": "amd64", "Os": "linux",
                              "GraphDriver": {"Name": "overlay2"},
                              "RootFS": {"Type": "layers", "Layers": LAYERS}}}
        self.fault = fault
        self.built = False
        self.calls = []

    def __call__(self, argv, **kwargs):
        args = argv[1:]
        self.calls.append(args)
        value, error, code = None, b"", 0
        if args[0] == "info":
            value = "changed-engine" if self.built and self.fault == "engine" else "synthetic-engine"
        elif args[:2] == ["image", "inspect"]:
            value = self.images.get(args[2])
            if value is None:
                code, error = 1, b"Error: No such image: synthetic\n"
        elif args[:2] == ["image", "tag"]:
            self.alias = args[3]
            self.images[self.alias] = self.images[args[2]]
        elif args[:2] == ["buildx", "build"]:
            self.built = True
            self.context = Path(args[-1])
            buildfile = Path(args[args.index("--file") + 1]).read_text()
            assert buildfile.startswith("FROM " + self.alias + "\n")
            self.images[DERIVED] = {"Id": DERIVED, "Architecture": "amd64", "Os": "linux",
                "RootFS": {"Type": "layers", "Layers": (LAYERS if self.fault != "layers" else []) +
                           ["sha256:" + "d" * 64]}}
            self.images[args[args.index("--tag") + 1]] = self.images[DERIVED]
            Path(args[args.index("--iidfile") + 1]).write_text(DERIVED + "\n")
            Path(args[args.index("--metadata-file") + 1]).write_bytes(canonical({
                "containerimage.config.digest": DERIVED,
                "buildx.build.provenance": {"materials": [{"digest": {
                    "sha256": (DERIVED if self.fault == "material" else chain_id(LAYERS))[7:]}}]}}))
            if self.fault == "alias":
                self.images[self.alias] = self.images[DERIVED]
            if self.fault == "context":
                (self.context / "payload").write_bytes(b"changed")
            if self.fault == "member":
                (self.context / "unlisted-private").write_bytes(b"do not read")
        else:
            raise AssertionError(args)
        return {"return_code": code, "failure": None, "elapsed_ms": 1,
                "stdout": canonical(value) if value is not None else b"", "stderr": error}


class LocalImageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.context = self.root / "context"
        self.context.mkdir()
        (self.context / "Dockerfile").write_text("FROM " + BASE + "\nLABEL synthetic=one\n")
        (self.context / "payload").write_bytes(b"authored generic bytes")
        self.rows = context_rows(self.context)
        metadata = self.root / "base-metadata.json"
        metadata.write_bytes(canonical({"containerimage.config.digest": BASE,
                                        "containerimage.digest": BASE}))
        self.provenance = {"build_metadata": file_pin(metadata), "material_kind": "moby-classic-chain-id-v1"}
        self.environment = {key: str(self.root) for key in ("HOME", "TMPDIR", "DOCKER_CONFIG", "BUILDX_CONFIG")}
        self.environment["DOCKER_HOST"] = "unix:///synthetic/docker.sock"

    def build(self, fake, directory="output"):
        with patch.dict(os.environ, self.environment), patch("gptgrep_harbor.local_images.run", fake):
            return LocalImageBuilder(sys.executable, self.root / directory, 30).build(
                self.context, self.rows, BASE, "linux/amd64", self.provenance)

    def test_alias_is_only_build_transport_and_result_joins_exact_parent(self):
        original = (self.context / "Dockerfile").read_bytes()
        fake = FakeEngine()
        result = self.build(fake)
        self.assertTrue(result["accepted"])
        self.assertEqual(result["base_config_id"], BASE)
        self.assertEqual(result["image_config_id"], DERIVED)
        self.assertEqual(result["parent_layers"], LAYERS)
        self.assertEqual(result["base_material_digest"], "sha256:" + sha((LAYERS[0] + " " + LAYERS[1]).encode()))
        self.assertNotEqual(result["base_material_digest"], BASE)
        self.assertEqual((self.context / "Dockerfile").read_bytes(), original)
        self.assertEqual(sum(call[:2] == ["buildx", "build"] for call in fake.calls), 1)
        self.assertFalse(any("push" in call or "prune" in call or "rm" in call for call in fake.calls))

    def test_engine_alias_material_layer_and_context_changes_fail_closed(self):
        for fault, expected in (("engine", "local_alias_after_build"), ("alias", "local_alias_after_build"),
                                ("material", "buildkit_selected_base_identity"), ("layers", "derived_parent_layers"),
                                ("context", "asset_size"), ("member", "image_context_membership")):
            with self.subTest(fault=fault):
                (self.context / "payload").write_bytes(b"authored generic bytes")
                extra = self.context / "unlisted-private"
                if extra.exists():
                    extra.unlink()
                fake = FakeEngine(fault)
                with self.assertRaisesRegex(ValueError, expected):
                    self.build(fake, "output-" + fault)
                self.assertFalse((self.root / ("output-" + fault) / "result.json").exists())
                self.assertFalse(json.loads((self.root / ("output-" + fault) / "failure.json").read_text())["accepted"])

    def test_unlisted_member_refuses_before_docker_or_body_read(self):
        (self.context / "unlisted-private").write_bytes(b"do not read")
        fake = FakeEngine()
        with patch("gptgrep_harbor.local_images.copy_asset") as read_member:
            with self.assertRaisesRegex(ValueError, "image_context_membership"):
                self.build(fake)
        read_member.assert_not_called()
        self.assertEqual(fake.calls, [])

    def test_wrong_config_metadata_pin_or_material_mode_rejects_before_docker(self):
        original = dict(self.provenance)
        for fault in ("config", "pin", "mode"):
            with self.subTest(fault=fault):
                self.provenance = dict(original)
                if fault == "mode":
                    self.provenance["material_kind"] = "accept-any-digest"
                else:
                    path = self.root / (fault + ".json")
                    path.write_bytes(canonical({"containerimage.config.digest": DERIVED,
                                                "containerimage.digest": DERIVED}))
                    self.provenance["build_metadata"] = file_pin(path)
                    if fault == "pin":
                        self.provenance["build_metadata"]["sha256"] = "0" * 64
                fake = FakeEngine()
                with self.assertRaises(ValueError):
                    self.build(fake, "reject-" + fault)
                self.assertEqual(fake.calls, [])

    def test_dataset_engine_rejected_before_tag_or_build(self):
        fake = FakeEngine()
        with patch.dict(os.environ, self.environment), patch("gptgrep_harbor.local_images.run", fake):
            with self.assertRaisesRegex(ValueError, "dataset_engine_identity"):
                LocalImageBuilder(sys.executable, self.root / "wrong-engine", 30).build(
                    self.context, self.rows, BASE, "linux/amd64", self.provenance,
                    expected_engine="another-engine")
        self.assertFalse(any(call[:2] in (["image", "tag"], ["buildx", "build"]) for call in fake.calls))

    def dataset_case(self, fault=None):
        fixture = fixtures.HarborMaterializationTests("test_local_image_ids_preserve_common_provenance_in_all_roles")
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.runtime["images"] = {"raw_only": BASE, "judge": BASE, "gptgrep": DERIVED}
        fixture.runtime["base_tool_environment"]["image"] = BASE
        product_path = Path(fixture.runtime["gptgrep_bundle"]["provenance"]["path"])
        product = json.loads(product_path.read_text())
        product["common_image"] = BASE
        fixture.runtime["gptgrep_bundle"]["provenance"] = fixtures.write_pin(product_path, product)
        dataset = fixture.root / "dataset"
        exported = fixture.materializer().run(dataset)
        export_path = fixture.root / "dataset.export.json"
        outputs = []
        inspected = []
        builds = {}
        for image_id in (BASE, DERIVED):
            metadata = fixture.root / (image_id[7:] + ".json")
            metadata.write_bytes(canonical({"containerimage.config.digest": image_id,
                                            "containerimage.digest": image_id}))
            builds[image_id] = {"build_metadata": file_pin(metadata), "material_kind": "moby-classic-chain-id-v1"}
        builds_path = fixture.root / "base-builds.json"
        builds_path.write_bytes(canonical(builds))

        class SyntheticBuilder:
            def __init__(self, *args):
                pass

            def engine(self):
                return "synthetic-engine"

            def inspect(self, image_id):
                # Inspection happens after every role was built, before enrollment.
                assert len(outputs) == 248
                first = dataset / exported["tasks"][0]["task_name"] / "task.toml"
                assert "docker_image" not in tomllib.loads(first.read_text())["environment"]
                inspected.append(image_id)
                if fault == "missing":
                    raise ValueError("local_image_docker_command")
                return {"Id": DERIVED if fault == "identity" else image_id,
                        "Architecture": "amd64", "Os": "linux", "RootFS": {"Type": "layers", "Layers": LAYERS}}

            def build(self, context, rows, base, platform, provenance, expected_engine=None):
                # No task may switch to prebuilt images until all 248 pass.
                first = dataset / exported["tasks"][0]["task_name"] / "task.toml"
                assert "docker_image" not in tomllib.loads(first.read_text())["environment"]
                value = "sha256:" + sha(str(context).encode())
                outputs.append(value)
                assert expected_engine == "synthetic-engine"
                return {"image_config_id": value, "derived_layers": LAYERS,
                        "engine_id": "changed-engine" if fault == "engine" and len(outputs) == 2 else "synthetic-engine"}

        with patch("gptgrep_harbor.local_images.LocalImageBuilder", SyntheticBuilder):
            if fault:
                expected = {"engine": "dataset_engine_identity", "missing": "local_image_docker_command",
                            "identity": "dataset_derived_image_identity"}[fault]
                with self.assertRaisesRegex(ValueError, expected):
                    build_dataset(export_path, sha(export_path.read_bytes()), dataset,
                                  fixture.root / "image-builds", sys.executable, 30,
                                  builds_path, sha(builds_path.read_bytes()))
                for row in exported["tasks"]:
                    config = tomllib.loads((dataset / row["task_name"] / "task.toml").read_text())
                    self.assertNotIn("docker_image", config["environment"])
                    self.assertNotIn("docker_image", config["verifier"]["environment"])
                self.assertFalse((fixture.root / "image-builds/result.json").exists())
                return
            result = build_dataset(export_path, sha(export_path.read_bytes()), dataset,
                                   fixture.root / "image-builds", sys.executable, 30,
                                   builds_path, sha(builds_path.read_bytes()))
        self.assertEqual(len(outputs), 248)
        self.assertEqual(inspected, outputs)
        self.assertEqual(result["role_image_count"], 248)
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            config = TaskConfig.model_validate(tomllib.loads((task / "task.toml").read_text()))
            self.assertEqual(config.environment.docker_image, record["images"]["environment"])
            verifier = resolve_verifier_environment_definition(config, TaskPaths(task))
            self.assertEqual(verifier.config.docker_image, record["images"]["tests"])
            for directory, value in ((task / "environment", config.environment.docker_image),
                                     (verifier.directory, verifier.config.docker_image)):
                self.assertTrue(should_use_prebuilt_docker_image(directory, docker_image=value, force_build=False))
                self.assertTrue((directory / "Dockerfile").is_file())

    def test_full_dataset_uses_harbor_prebuilt_ids_after_all_role_builds(self):
        self.dataset_case()

    def test_dataset_switch_between_roles_rejects_before_any_enrollment(self):
        self.dataset_case("engine")

    def test_disappeared_image_before_enrollment_rejects_every_task(self):
        self.dataset_case("missing")

    def test_wrong_derived_identity_before_enrollment_rejects_every_task(self):
        self.dataset_case("identity")


if __name__ == "__main__":
    unittest.main()
