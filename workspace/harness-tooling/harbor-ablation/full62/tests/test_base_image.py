"""Finite base-image staging and installed Pillow checks; no Docker execution."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import shlex
import tempfile
import unittest
import sys
from unittest.mock import patch
from bundle_fixtures import code_mode_host_fixture, pin, sandbox_helper_fixture

from gptgrep_harbor.adapter import image
from gptgrep_harbor.common import canonical, sha
from gptgrep_harbor.runtime import BASE_CAPABILITIES, CODEX_SOURCE_COMMIT, base_tools_identity, check_base_tools


def load_script(name):
    path = Path(__file__).resolve().parents[1] / "base-image" / (name + ".py")
    spec = importlib.util.spec_from_file_location("base_image_" + name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CommonBaseImageTests(unittest.TestCase):
    def test_immutable_image_accepts_full_local_ids_and_registry_digests(self):
        for value in ("sha256:" + "a" * 64, "registry.example:5000/team/base:fixed@sha256:" + "b" * 64):
            self.assertEqual(image(value), value)
        for value in (None, "base:latest", "a" * 64, "sha256:" + "a" * 12,
                      "sha256:" + "a" * 63, "sha256:" + "a" * 65, "sha256:" + "A" * 64,
                      "SHA256:" + "a" * 64, "sha256:" + "a" * 64 + ":latest",
                      "sha256:" + "a" * 64 + "\n", "base@sha256:" + "B" * 64,
                      "base@sha256:" + "b" * 64 + "/extra"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "immutable_image"):
                image(value)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.prepare_module = load_script("prepare")
        self.prepare = self.prepare_module.prepare
        native = self.root / "synthetic-native"
        native.write_bytes(b"not an executable; authored synthetic bytes")
        helper, self.helper_record = sandbox_helper_fixture(self.root / "helper")
        self.config = {"platform": "linux/amd64", "python_image": "python@sha256:" + "a" * 64,
                       "uv_image": "synthetic/uv@sha256:" + "b" * 64,
                       "debian_snapshot": "20261001T000000Z", "poppler_version": "1.0-synthetic",
                       "codex_binary": {"path": str(native), "bytes": native.stat().st_size,
                                        "sha256": sha(native.read_bytes())}, "gptgrep_binary": None,
                       "native_sandbox_helper": helper}
        host, self.host_record = code_mode_host_fixture(self.root / "host",
            {**self.config["codex_binary"], "path": "/usr/local/bin/codex"})
        self.config["native_code_mode_host"] = host

    def test_base_from_rejects_local_config_ids_before_reading_binaries(self):
        for field in ("python_image", "uv_image"):
            with self.subTest(field=field):
                output = self.root / field
                with patch.object(self.prepare_module, "copy_asset") as copy:
                    with self.assertRaisesRegex(ValueError, "base_from_repository_digest_required"):
                        self.prepare({**self.config, field: "sha256:" + "a" * 64}, output)
                copy.assert_not_called()
                self.assertFalse(output.exists())

    def test_base_from_accepts_repository_digests_with_registry_ports_and_tags(self):
        config = {**self.config,
                  "python_image": "registry.example:5000/team/python:fixed@sha256:" + "a" * 64,
                  "uv_image": "registry.example:5000/team/uv:fixed@sha256:" + "b" * 64}
        result = self.prepare(config, self.root / "registry-context")
        for field in ("python_image", "uv_image"):
            self.assertIn(field.upper() + "=" + config[field], result["build_commands"]["common"])

    def test_finite_context_prints_build_without_execution_and_separates_product_target(self):
        (self.root / "auth.json").write_bytes(b"synthetic excluded sibling")
        output = self.root / "context"
        with patch("subprocess.run") as run, patch("subprocess.Popen") as spawn:
            result = self.prepare(self.config, output)
        run.assert_not_called()
        spawn.assert_not_called()
        self.assertFalse(result["actual_build_performed"])
        self.assertEqual(set(result["build_commands"]), {"common"})
        self.assertEqual({path.name for path in output.iterdir()},
                         {"Dockerfile", ".dockerignore", "requirements.lock", "probe.py", "codex",
                          "bwrap", "helper-record.json", "COPYING.bubblewrap", "LICENSE.codex",
                          "codex-code-mode-host", "host-provenance.json", "host.sigstore"})
        docker = (output / "Dockerfile").read_text()
        common, treatment = docker.split("FROM common AS gptgrep")
        self.assertNotIn("COPY gptgrep", common)
        self.assertIn("COPY gptgrep", treatment)
        self.assertIn("--require-hashes", common)
        self.assertIn('"poppler-utils=$POPPLER_VERSION"', common)
        self.assertIn("/usr/local/bin/codex-resources/bwrap", common)
        self.assertEqual(result["native_sandbox_helper"], self.helper_record)
        self.assertEqual(result["native_code_mode_host"], self.host_record)
        self.assertFalse(result["native_code_mode_host_protocol_executed"])
        self.assertIn("/usr/local/bin/codex-code-mode-host", common)
        self.assertIn("CODE_MODE_HOST_SHA256=", result["build_commands"]["common"])
        # The derived-image frontdoors pin this BuildKit metadata; it lands beside the context.
        argv = shlex.split(result["build_commands"]["common"])
        metadata = output.with_name("context.build-metadata.json")
        self.assertEqual(argv[argv.index("--metadata-file") + 1], str(metadata))
        self.assertEqual(argv[-1], str(output))
        self.assertEqual(result["build_metadata_file"], str(metadata))
        self.assertFalse(metadata.exists())
        self.assertFalse(result["native_helper_digest_compatibility_verified"])
        self.assertNotIn("auth.json", (output / ".dockerignore").read_text())
        product = self.root / "synthetic-product"
        product.write_bytes(b"synthetic current product")
        self.config["gptgrep_binary"] = {"path": str(product), "bytes": product.stat().st_size,
                                        "sha256": sha(product.read_bytes())}
        with self.assertRaisesRegex(ValueError, "raw_product_binary_unsupported_use_release_bundle"):
            self.prepare(self.config, self.root / "with-product")
        self.assertFalse((self.root / "with-product").exists())

    def test_build_metadata_path_is_fresh_outside_context_and_common_only(self):
        explicit = self.root / "metadata" / "common.json"
        explicit.parent.mkdir()
        result = self.prepare(self.config, self.root / "explicit-context", explicit)
        argv = shlex.split(result["build_commands"]["common"])
        self.assertEqual(argv[argv.index("--metadata-file") + 1], str(explicit))
        self.assertEqual(result["build_metadata_file"], str(explicit))
        (self.root / "existing.json").write_bytes(b"{}")
        (self.root / "dangling.json").symlink_to(self.root / "nowhere.json")
        cases = {"inside-context": lambda output: output / "build-metadata.json",
                 "existing": lambda output: self.root / "existing.json",
                 "dangling-link": lambda output: self.root / "dangling.json",
                 "missing-parent": lambda output: self.root / "absent-dir" / "metadata.json"}
        for name, metadata in cases.items():
            output = self.root / name
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "fresh_build_metadata_path"):
                self.prepare(self.config, output, metadata(output))
            self.assertFalse(output.exists())
        (self.root / "occupied.build-metadata.json").write_bytes(b"{}")
        with self.assertRaisesRegex(ValueError, "fresh_build_metadata_path"):
            self.prepare(self.config, self.root / "occupied")
        self.assertFalse((self.root / "occupied").exists())
        argv = ["prepare.py", "--treatment", "--inputs", str(self.root / "absent.json"),
                "--output-dir", str(self.root / "treatment"), "--metadata-file", str(explicit)]
        with patch.object(sys, "argv", argv), contextlib.redirect_stderr(io.StringIO()), \
                self.assertRaises(SystemExit) as raised:
            self.prepare_module.main()
        self.assertEqual(raised.exception.code, 2)
        self.assertFalse((self.root / "treatment").exists())

    def test_unresolved_package_or_bad_binary_pin_cannot_create_context(self):
        for field in ("python_image", "uv_image", "debian_snapshot", "poppler_version"):
            with self.subTest(field=field):
                with self.assertRaises(ValueError):
                    self.prepare({**self.config, field: None}, self.root / "absent")
                self.assertFalse((self.root / "absent").exists())
        self.config["codex_binary"]["sha256"] = "0" * 64
        with self.assertRaises(ValueError):
            self.prepare(self.config, self.root / "absent")
        self.assertFalse((self.root / "absent").exists())

    def test_helper_missing_tampered_or_wrong_architecture_fails_before_context(self):
        from copy import deepcopy
        missing = deepcopy(self.config)
        del missing["native_sandbox_helper"]
        with self.assertRaisesRegex(ValueError, "base_build_fields"):
            self.prepare(missing, self.root / "missing-helper")
        self.assertFalse((self.root / "missing-helper").exists())
        for field in ("binary", "provenance"):
            config = deepcopy(self.config)
            config["native_sandbox_helper"][field]["sha256"] = "0" * 64
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.prepare(config, self.root / field)
            self.assertFalse((self.root / field).exists())
        wrong_arch = deepcopy(self.config)
        wrong_arch["platform"] = "linux/arm64"
        with self.assertRaisesRegex(ValueError, "native_helper_official_asset"):
            self.prepare(wrong_arch, self.root / "wrong-arch")
        self.assertFalse((self.root / "wrong-arch").exists())
        license_pin = self.config["native_sandbox_helper"]["licenses"]["COPYING.bubblewrap"]
        license_path = Path(license_pin["path"])
        license_bytes = license_path.read_bytes()
        license_path.write_bytes(b"X" * len(license_bytes))
        with self.assertRaisesRegex(ValueError, "asset_digest"):
            self.prepare(self.config, self.root / "tampered-license")
        self.assertFalse((self.root / "tampered-license").exists())
        license_path.write_bytes(license_bytes)
        binary = Path(self.config["native_sandbox_helper"]["binary"]["path"])
        binary.chmod(0o644)
        binary.write_bytes(b"X" * binary.stat().st_size)
        with self.assertRaisesRegex(ValueError, "asset_digest"):
            self.prepare(self.config, self.root / "tampered-helper")
        self.assertFalse((self.root / "tampered-helper").exists())
        # A self-consistent caller pin still cannot disguise non-Linux bytes.
        self.config["native_sandbox_helper"]["binary"]["sha256"] = sha(binary.read_bytes())
        self.helper_record["binary"]["sha256"] = sha(binary.read_bytes())
        metadata = Path(self.config["native_sandbox_helper"]["provenance"]["path"])
        metadata.write_bytes(canonical(self.helper_record))
        self.config["native_sandbox_helper"]["provenance"] = {
            "path": str(metadata), "bytes": metadata.stat().st_size, "sha256": sha(metadata.read_bytes())}
        with self.assertRaisesRegex(ValueError, "native_helper_linux_elf"):
            self.prepare(self.config, self.root / "non-linux-helper")
        self.assertFalse((self.root / "non-linux-helper").exists())

    def test_host_missing_tampered_wrong_pair_or_architecture_prevents_context(self):
        from copy import deepcopy
        missing = deepcopy(self.config)
        del missing["native_code_mode_host"]
        with self.assertRaisesRegex(ValueError, "base_build_fields"):
            self.prepare(missing, self.root / "missing-host")
        self.assertFalse((self.root / "missing-host").exists())
        for field in ("binary", "provenance", "sigstore"):
            config = deepcopy(self.config)
            config["native_code_mode_host"][field]["sha256"] = "0" * 64
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.prepare(config, self.root / ("host-" + field))
            self.assertFalse((self.root / ("host-" + field)).exists())
        metadata = Path(self.config["native_code_mode_host"]["provenance"]["path"])
        record = deepcopy(self.host_record)
        record["runtime_binary"]["sha256"] = "0" * 64
        metadata.write_bytes(canonical(record))
        self.config["native_code_mode_host"]["provenance"] = pin(metadata)
        with self.assertRaisesRegex(ValueError, "native_code_mode_host_pair"):
            self.prepare(self.config, self.root / "wrong-pair")
        self.assertFalse((self.root / "wrong-pair").exists())
        binary = Path(self.config["native_code_mode_host"]["binary"]["path"])
        wrong_arch = bytearray(binary.read_bytes())
        wrong_arch[18:20] = (183).to_bytes(2, "little")
        binary.chmod(0o644)
        binary.write_bytes(wrong_arch)
        self.config["native_code_mode_host"]["binary"] = pin(binary)
        self.host_record["binary"]["sha256"] = sha(wrong_arch)
        metadata.write_bytes(canonical(self.host_record))
        self.config["native_code_mode_host"]["provenance"] = pin(metadata)
        with self.assertRaisesRegex(ValueError, "native_code_mode_host_linux_elf"):
            self.prepare(self.config, self.root / "wrong-host-arch")
        self.assertFalse((self.root / "wrong-host-arch").exists())

    def test_pinned_pillow_executes_generic_pixel_operations(self):
        self.assertEqual(load_script("probe").check_pillow(), "12.3.0")

    def test_startup_checks_installed_tool_bytes_and_executable_paths(self):
        binaries = {}
        for name in ("python3", "sh", "uv", "codex", "pdftotext", "pdftoppm"):
            path = self.root / name
            path.write_bytes(("synthetic " + name).encode())
            binaries[name] = {"path": str(path), "bytes": path.stat().st_size, "sha256": sha(path.read_bytes())}
        helper_dir = self.root / "installed-helper"
        helper_path = helper_dir / "bwrap"
        with patch("gptgrep_harbor.runtime.SANDBOX_HELPER_PATH", str(helper_path)), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_DATA", str(helper_dir)):
            _, helper = sandbox_helper_fixture(helper_dir)
        host_path = self.root / "codex-code-mode-host"
        with patch("gptgrep_harbor.runtime.CODE_MODE_HOST_PATH", str(host_path)), \
             patch("gptgrep_harbor.runtime.CODE_MODE_HOST_DATA", str(self.root)):
            _, host = code_mode_host_fixture(self.root, binaries["codex"])
        value = {"schema": "gptgrep.harbor.base-tools.v1", "pillow_version": "12.3.0",
                 "build_inputs": {"platform": "linux/amd64"}, "native_sandbox_helpers": {"bwrap": helper},
                 "native_code_mode_host": host,
                 "native_cli_version": "0.162.1", "native_source_commit": CODEX_SOURCE_COMMIT,
                 "python_version": sys.version.split()[0], "pillow_pixel_check": "passed",
                 "capabilities": list(BASE_CAPABILITIES), "binaries": binaries}
        path = self.root / "provenance.json"
        path.write_bytes(canonical(value))
        config = {"reader_base_binary": binaries["codex"], "base_tool_environment": {
            "provenance": {"path": str(path), "bytes": path.stat().st_size, "sha256": sha(path.read_bytes())}}}
        with patch("shutil.which", side_effect=lambda name: binaries[name]["path"]), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_PATH", str(helper_path)), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_DATA", str(helper_dir)), \
             patch("gptgrep_harbor.runtime.CODE_MODE_HOST_PATH", str(host_path)), \
             patch("gptgrep_harbor.runtime.CODE_MODE_HOST_DATA", str(self.root)):
            check_base_tools(config)
            original_host = host_path.read_bytes()
            host_path.chmod(0o644)
            host_path.write_bytes(b"X" * len(original_host))
            with self.assertRaisesRegex(ValueError, "asset_digest"):
                check_base_tools(config)
            host_path.write_bytes(original_host)
            with self.assertRaisesRegex(ValueError, "native_code_mode_host_executable_mode"):
                check_base_tools(config)
            host_path.chmod(0o555)
            original = helper_path.read_bytes()
            helper_path.chmod(0o644)
            helper_path.write_bytes(b"X" * len(original))
            with self.assertRaisesRegex(ValueError, "asset_digest"):
                check_base_tools(config)
            helper_path.write_bytes(original)
            helper_path.chmod(0o555)
            tool = self.root / "pdftoppm"
            tool.write_bytes(b"X" * tool.stat().st_size)
            with self.assertRaisesRegex(ValueError, "asset_digest"):
                check_base_tools(config)
        with patch("shutil.which", return_value="/synthetic/wrong-tool"), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_PATH", str(helper_path)), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_DATA", str(helper_dir)), \
             patch("gptgrep_harbor.runtime.CODE_MODE_HOST_PATH", str(host_path)), \
             patch("gptgrep_harbor.runtime.CODE_MODE_HOST_DATA", str(self.root)):
            with self.assertRaisesRegex(ValueError, "common_tool_path"):
                check_base_tools(config)
        del value["native_sandbox_helpers"]
        path.write_bytes(canonical(value))
        config["base_tool_environment"]["provenance"] = {"path": str(path), "bytes": path.stat().st_size,
                                                        "sha256": sha(path.read_bytes())}
        with self.assertRaisesRegex(ValueError, "native_sandbox_helper_required"):
            base_tools_identity(config["base_tool_environment"]["provenance"])
        value["native_sandbox_helpers"] = {"bwrap": helper}
        del value["native_code_mode_host"]
        path.write_bytes(canonical(value))
        config["base_tool_environment"]["provenance"] = pin(path)
        with patch("gptgrep_harbor.runtime.SANDBOX_HELPER_PATH", str(helper_path)), \
             patch("gptgrep_harbor.runtime.SANDBOX_HELPER_DATA", str(helper_dir)):
            with self.assertRaisesRegex(ValueError, "native_code_mode_host_required"):
                base_tools_identity(config["base_tool_environment"]["provenance"])


if __name__ == "__main__":
    unittest.main()
