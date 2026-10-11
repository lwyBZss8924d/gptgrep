"""Discriminating generic bundle/custody checks; no Docker or native execution."""
import importlib.util
import gzip
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from bundle_fixtures import bundle_fixture, pin
from gptgrep_harbor import product_bundle as product
from gptgrep_harbor.common import canonical


class ProductBundleTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"])
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()

    def test_valid_bundle_preserves_every_payload_and_binds_launcher_not_elf(self):
        bundle = bundle_fixture(self.root)
        dest = self.root / "installed"
        result = product.release_bundle(bundle, "linux/amd64", dest)
        self.assertEqual(set(result["files"]), {p.relative_to(dest).as_posix() for p in dest.rglob("*") if p.is_file()})
        self.assertEqual(result["entrypoint"]["path"], product.PRODUCT_ENTRYPOINT)
        self.assertNotEqual(result["entrypoint"]["sha256"], result["binary"]["sha256"])
        self.assertEqual((dest / "lib/libpdfium.so").read_bytes()[:4], b"\x7fELF")

    def test_archive_digest_and_inner_metadata_mismatch_fail_before_output(self):
        bundle = bundle_fixture(self.root)
        original = bundle["archive"]["sha256"]
        bundle["archive"]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "pin_mismatch"):
            product.release_bundle(bundle, "linux/amd64", self.root / "absent")
        bundle["archive"]["sha256"] = original
        path = Path(bundle["release_metadata"]["path"])
        path.write_bytes(path.read_bytes() + b" ")
        bundle["release_metadata"] = pin(path)
        with self.assertRaisesRegex(ValueError, "bundle_member_digest"):
            product.release_bundle(bundle, "linux/amd64", self.root / "absent")
        self.assertFalse((self.root / "absent").exists())

    def test_compressed_extension_overflow_is_rejected_before_tar_parser_or_destination(self):
        bundle = bundle_fixture(self.root)
        extension = tarfile.TarInfo("PaxHeaders/oversized")
        extension.type, extension.mode, extension.size = tarfile.XHDTYPE, 0o644, 4096
        prefix = b"4096 comment="
        body = prefix + b"x" * (4096 - len(prefix) - 1) + b"\n"
        tar_bytes = extension.tobuf(format=tarfile.USTAR_FORMAT) + body + b"\0" * 1024
        archive = Path(bundle["archive"]["path"])
        archive.write_bytes(gzip.compress(tar_bytes, mtime=0))
        bundle["archive"] = pin(archive)
        self.assertLess(archive.stat().st_size, 1024)
        with patch.object(product, "MAX_TAR_BYTES", 1024), \
                patch.object(product.tarfile, "open", side_effect=AssertionError("tar parser reached")) as parser:
            with self.assertRaisesRegex(ValueError, "bundle_tar_expanded_bound"):
                product.release_bundle(bundle, "linux/amd64", self.root / "absent")
        parser.assert_not_called()
        self.assertFalse((self.root / "absent").exists())

    def test_missing_pdfium_and_license_tamper_fail_before_output(self):
        for name, kwargs in (("missing", {"omit": "lib/libpdfium.so"}),
                             ("tamper", {"change": lambda release, files: files.update(LICENSE=b"changed bytes")})):
            with self.subTest(name=name):
                root = self.root / name
                root.mkdir()
                bundle = bundle_fixture(root, **kwargs)
                with self.assertRaisesRegex(ValueError, "bundle_missing_manifest_file|bundle_member_digest"):
                    product.release_bundle(bundle, "linux/amd64", root / "absent")
                self.assertFalse((root / "absent").exists())

    def test_path_traversal_symlink_duplicate_and_unlisted_files_are_rejected(self):
        def extra(top, name):
            item = tarfile.TarInfo(name.replace("TOP", top))
            item.mode, item.size = 0o644, 1
            return item, b"x"
        variants = {"traversal": {"extra": lambda top: extra(top, "TOP/../escape")},
                    "absolute": {"extra": lambda top: extra(top, "/escape")},
                    "extra": {"extra": lambda top: extra(top, "TOP/unlisted")},
                    "duplicate": {"extra": lambda top: extra(top, "TOP/LICENSE")},
                    "symlink": {"member_change": lambda name, item: setattr(item, "type", tarfile.SYMTYPE)
                                if name == "lib/libpdfium.so" else None}}
        for name, kwargs in variants.items():
            with self.subTest(name=name):
                root = self.root / name
                root.mkdir()
                bundle = bundle_fixture(root, **kwargs)
                with self.assertRaises(ValueError):
                    product.release_bundle(bundle, "linux/amd64", root / "absent")
                self.assertFalse((root / "absent").exists())
        self.assertFalse((self.root / "escape").exists())

    def test_wrong_elf_and_raw_binary_launcher_are_rejected_even_with_matching_manifest(self):
        def wrong_elf(release, files):
            files["gptgrep.bin"] = b"not an ELF binary"
            from gptgrep_harbor.common import sha
            release["binary"]["sha256"] = release["files"]["gptgrep.bin"] = sha(files["gptgrep.bin"])
        def raw_launcher(release, files):
            files["gptgrep"] = files["gptgrep.bin"]
            release["files"]["gptgrep"] = release["files"]["gptgrep.bin"]
        for name, change in (("elf", wrong_elf), ("launcher", raw_launcher)):
            with self.subTest(name=name):
                root = self.root / name
                root.mkdir()
                with self.assertRaisesRegex(ValueError, "bundle_linux_elf|bundle_launcher_layout"):
                    product.release_bundle(bundle_fixture(root, change=change), "linux/amd64", root / "absent")
                self.assertFalse((root / "absent").exists())

    def test_installed_library_tamper_missing_and_extra_files_fail_startup(self):
        dest = self.root / "installed"
        value = product.release_bundle(bundle_fixture(self.root), "linux/amd64", dest)
        common = "sha256:" + "a" * 64
        value.update(root=str(dest), common_image=common)
        for field, name in (("entrypoint", "gptgrep"), ("binary", "gptgrep.bin"), ("pdfium", "lib/libpdfium.so")):
            value[field]["path"] = str(dest / name)
        provenance = self.root / "provenance.json"
        provenance.write_bytes(canonical(value))
        config = {"platform": "linux/amd64", "gptgrep_binary": str(dest / "gptgrep"),
                  "gptgrep_binary_pin": value["entrypoint"], "gptgrep_bundle": {"provenance": pin(provenance)},
                  "base_tool_environment": {"image": common}}
        with patch.object(product, "PRODUCT_ROOT", str(dest)), patch.object(product, "PRODUCT_ENTRYPOINT", str(dest / "gptgrep")), patch.object(product, "PRODUCT_PROVENANCE", str(provenance)):
            product.check_product_bundle(config)
            library = dest / "lib/libpdfium.so"
            original = library.read_bytes()
            library.chmod(0o600)
            library.write_bytes(b"x" * len(original))
            with self.assertRaisesRegex(ValueError, "asset_digest"):
                product.check_product_bundle(config)
            library.unlink()
            with self.assertRaisesRegex(ValueError, "installed_bundle_missing_file"):
                product.check_product_bundle(config)
            library.write_bytes(original)
            (dest / "unexpected").write_bytes(b"extra")
            with self.assertRaisesRegex(ValueError, "installed_bundle_extra_file"):
                product.check_product_bundle(config)

    def test_treatment_context_derives_from_common_without_install_or_docker(self):
        path = Path(__file__).resolve().parents[1] / "base-image/prepare.py"
        spec = importlib.util.spec_from_file_location("treatment_prepare", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        common = "sha256:" + "a" * 64
        metadata = self.root / "build-metadata.json"
        metadata.write_bytes(canonical({"containerimage.config.digest": common, "containerimage.digest": common}))
        config = {"platform": "linux/amd64", "common_image": common,
                  "common_build_provenance": {"build_metadata": pin(metadata), "material_kind": "moby-classic-chain-id-v1"},
                  "product_bundle": bundle_fixture(self.root)}
        with patch("subprocess.run") as run, patch("subprocess.Popen") as spawn:
            result = module.prepare_treatment(config, self.root / "context")
        run.assert_not_called()
        spawn.assert_not_called()
        self.assertFalse(result["actual_build_performed"])
        docker = (self.root / "context/Dockerfile").read_text()
        self.assertEqual(docker.splitlines()[0], "FROM " + common)
        self.assertIn("COPY product-provenance.json " + product.PRODUCT_PROVENANCE + "\n", docker)
        self.assertNotIn("apt", docker)
        self.assertNotIn("pip", docker)
        self.assertNotIn("codex", docker)
        provenance = json.loads((self.root / "context/product-provenance.json").read_text())
        self.assertEqual(result["runtime_product"]["gptgrep_binary_pin"], provenance["entrypoint"])
        self.assertEqual(provenance["common_image"], common)


if __name__ == "__main__":
    unittest.main()
