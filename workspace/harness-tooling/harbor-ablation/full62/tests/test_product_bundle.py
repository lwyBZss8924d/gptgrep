"""Discriminating generic bundle/custody checks; no Docker or native execution."""
import importlib.util
import gzip
import json
import os
from pathlib import Path
import shlex
import shutil
import stat
import subprocess
import tarfile
import tempfile
import sys
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

    def packager(self):
        repo = Path(__file__).resolve().parents[5]
        scripts = repo / "scripts"
        spec = importlib.util.spec_from_file_location("package_release_runtime_fixture", scripts / "package_release.py")
        module = importlib.util.module_from_spec(spec)
        sys.path.insert(0, str(scripts))
        try:
            spec.loader.exec_module(module)
        finally:
            sys.path.pop(0)
        return module, repo

    def runtime_bundle(self, root, change=None):
        module, repo = self.packager()

        def attach(release, files):
            copied = root / "runtime-copy"
            runtime = module.copy_runtime_assets(repo, copied)
            release["native_runtime_package"] = {"root": ".", **runtime}
            for name, digest in runtime["files"].items():
                files[name] = (copied / name).read_bytes()
                release["files"][name] = digest
            if change:
                change(release, files)
        return bundle_fixture(root, change=attach)

    def test_actual_packager_runtime_assets_join_and_stage_with_the_bundle(self):
        dest = self.root / "installed"
        bundle = self.runtime_bundle(self.root)
        result = product.release_bundle(bundle, "linux/amd64", dest)
        self.assertTrue(product.RUNTIME_FILES <= result["files"].keys())
        for name in product.RUNTIME_FILES:
            self.assertEqual((dest / name).read_bytes(), (self.root / "runtime-copy" / name).read_bytes())

    def test_packager_checksum_walk_and_archive_writer_are_accepted(self):
        """Private staging modes produce a portable archive without source chmods."""
        previous_umask = os.umask(0o077)
        self.addCleanup(os.umask, previous_umask)
        module, repo = self.packager()
        target = "x86_64-unknown-linux-gnu"
        bundle = self.root / "staging" / ("gptgrep-1.2.3-synthetic-" + target)
        bundle.mkdir(parents=True)
        elf = bytearray(64)
        elf[:6] = b"\x7fELF\x02\x01"
        elf[18:20] = (62).to_bytes(2, "little")
        authored = {"gptgrep": module.LAUNCHER.encode(), "gptgrep.bin": bytes(elf),
                    "lib/libpdfium.so": bytes(elf), "LICENSE": b"authored license fixture",
                    "THIRD_PARTY.md": b"authored notice fixture",
                    "third-party/rust/inventory.json": canonical({
                        "schema_version": "gptgrep.licenses.v1", "target": target, "complete": True,
                        "missing_license_files": [], "packages": [{}]})}
        for name, data in authored.items():
            path = bundle / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(0o755 if name in ("gptgrep", "gptgrep.bin") else 0o644)
        runtime = module.copy_runtime_assets(repo, bundle)
        # Mirrors scripts/package_release.py: manifest, RELEASE.json, then the sorted walk.
        files = {item.relative_to(bundle).as_posix(): module.sha256(item)
                 for item in sorted(bundle.rglob("*")) if item.is_file()}
        release = {"schema_version": "gptgrep.release.v1", "version": "1.2.3-synthetic", "target": target,
                   "source_revision": None, "source_dirty": True,
                   "binary": {"file": "gptgrep.bin", "sha256": files["gptgrep.bin"]},
                   "pdfium": {"library": "lib/libpdfium.so", "library_sha256": files["lib/libpdfium.so"]},
                   "rust_licenses": {"complete": True, "packages": 1, "inventory": "third-party/rust/inventory.json"},
                   "native_runtime_package": {"root": ".", **runtime}, "files": files}
        encoded = json.dumps(release, indent=2, sort_keys=True) + "\n"
        (bundle / "RELEASE.json").write_text(encoded, encoding="utf-8")
        checksums = [f"{module.sha256(item)}  {item.relative_to(bundle).as_posix()}"
                     for item in sorted(bundle.rglob("*")) if item.is_file()]
        (bundle / "SHA256SUMS").write_text("\n".join(checksums) + "\n", encoding="utf-8")
        listed = [line.split("  ", 1)[1] for line in checksums]
        # Pathlib order differs from a plain string sort for these real runtime paths.
        self.assertLess(listed.index("skills/gptgrep/SKILL.md"), listed.index("skills/gptgrep-document/SKILL.md"))
        self.assertNotEqual(listed, sorted(listed))
        before_modes = {item.relative_to(bundle).as_posix(): stat.S_IMODE(item.stat().st_mode)
                        for item in [bundle, *bundle.rglob("*")]}
        self.assertEqual(before_modes["."], 0o700)
        self.assertEqual(before_modes["RELEASE.json"], 0o600)
        self.assertEqual(before_modes["SHA256SUMS"], 0o600)
        archive = self.root / "packaged.tar.gz"
        module.archive_bundle(bundle, archive)
        self.assertEqual({item.relative_to(bundle).as_posix(): stat.S_IMODE(item.stat().st_mode)
                          for item in [bundle, *bundle.rglob("*")]}, before_modes)
        self.assertEqual(stat.S_IMODE(archive.stat().st_mode), 0o600)
        with tarfile.open(archive) as packed:
            self.assertEqual(packed.getmember(bundle.name).mode, 0o755)
            for member in packed:
                self.assertTrue(member.isdir() or member.isfile())
                relative = Path(member.name).relative_to(bundle.name).as_posix()
                expected = 0o755 if member.isdir() or before_modes[relative] & 0o111 else 0o644
                self.assertEqual(member.mode, expected)
        sidecar = self.root / "packaged.release.json"
        sidecar.write_text(encoded, encoding="utf-8")
        dest = self.root / "installed"
        result = product.release_bundle({"archive": pin(archive), "release_metadata": pin(sidecar)},
                                        "linux/amd64", dest)
        self.assertEqual((dest / "SHA256SUMS").read_bytes(), (bundle / "SHA256SUMS").read_bytes())
        self.assertEqual(result["files"]["SHA256SUMS"]["sha256"], module.sha256(bundle / "SHA256SUMS"))

    def test_runtime_declarations_cannot_detach_from_pinned_bundle_files(self):
        variants = {
            "manifest": lambda value: value.update(manifest_sha256="0" * 64),
            "file": lambda value: value["files"].update({"skills/gptgrep/SKILL.md": "0" * 64}),
            "missing": lambda value: value["files"].pop("runtime/codex/stage.sh"),
            "root": lambda value: value.update(root="/another/root"),
            "skills": lambda value: value.update(skills=[]),
        }
        for name, change in variants.items():
            with self.subTest(name=name):
                root = self.root / name
                root.mkdir()
                bundle = self.runtime_bundle(root, lambda release, files: change(release["native_runtime_package"]))
                with self.assertRaisesRegex(ValueError, "bundle_runtime_"):
                    product.release_bundle(bundle, "linux/amd64", root / "absent")
                self.assertFalse((root / "absent").exists())

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

        # Replay only the generated mode step on synthetic COPY destinations.
        # Root can read mode0600 inputs, so the build-time verifier alone cannot
        # establish that a different runtime UID can read this public payload.
        context = self.root / "context"
        staging_modes = {p: stat.S_IMODE(p.stat().st_mode) for p in context.rglob("*")}
        image = self.root / "image"
        bundle = image / product.PRODUCT_ROOT.lstrip("/")
        public = image / "usr/local/share/full62-product"
        shutil.copytree(context / "bundle", bundle)
        public.mkdir(parents=True)
        for name in ("product-provenance.json", "product-check.json"):
            shutil.copy2(context / name, public / name)
        shutil.copytree(context / "verifier", public / "verifier")
        private = image / "private-marker"
        private.write_bytes(b"private staging is outside the copied public roots")
        private.chmod(0o600)
        payload = {}
        executable = {bundle / "gptgrep", bundle / "gptgrep.bin"}
        for root in (bundle, public):
            for path in (root, *root.rglob("*")):
                path.chmod(0o700 if path.is_dir() else 0o500 if path in executable else 0o600)
                if path.is_file():
                    payload[path] = path.read_bytes()
        mode_step = shlex.split(next(line for line in docker.splitlines()
                                     if line.startswith("RUN ")).split(" && ", 1)[0])
        self.assertEqual(mode_step, ["RUN", "chmod", "-R", "a=rX", product.PRODUCT_ROOT,
                                     "/usr/local/share/full62-product"])
        subprocess.run([*mode_step[1:-2], str(bundle), str(public)], check=True)
        for root in (bundle, public):
            for path in (root, *root.rglob("*")):
                self.assertEqual(stat.S_IMODE(path.stat().st_mode),
                                 0o555 if path.is_dir() or path in executable else 0o444)
        self.assertEqual({p: p.read_bytes() for p in payload}, payload)
        self.assertEqual(stat.S_IMODE(private.stat().st_mode), 0o600)
        self.assertEqual({p: stat.S_IMODE(p.stat().st_mode) for p in staging_modes}, staging_modes)


if __name__ == "__main__":
    unittest.main()
