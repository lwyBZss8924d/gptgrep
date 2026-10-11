#!/usr/bin/env python3
"""Package a native GPTgrep binary with a verified local PDFium release payload."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import tomllib
import unittest

from collect_licenses import collect


PDFIUM_TAG = "chromium/8058"
PDFIUM_REVISION = "f15f55200f695a13c2ca3aab35f8aa40a596b2f7"
# Verified from the upstream release assets and their extracted license/library
# payloads. Refresh both hashes together only after reviewing the pinned source.
TARGETS = {
    "aarch64-apple-darwin": {
        "system": "Darwin", "machine": "arm64", "library": "libpdfium.dylib",
        "asset": "pdfium-mac-arm64.tgz",
        "archive_sha256": "df8d1016fc36c2651b368ca13b992ae3bb6b6020d12f2d24443bf13cf76ea593",
        "payload_sha256": "e7ea012bbb01f137ff113f16502bea5aacb9d4bf97aa9031ce326e8d3182f02a",
    },
    "aarch64-unknown-linux-gnu": {
        "system": "Linux", "machine": "aarch64", "library": "libpdfium.so",
        "asset": "pdfium-linux-arm64.tgz",
        "archive_sha256": "4915b5351640c1eea59e26023e0edcad8433df96dc8f4cb3d645a4d64a178b7c",
        "payload_sha256": "68b413b7935a014292c6f3d4e0999cd9be2a9131a14e26d22bb573451a01a6c2",
    },
    "x86_64-unknown-linux-gnu": {
        "system": "Linux", "machine": "x86_64", "library": "libpdfium.so",
        "asset": "pdfium-linux-x64.tgz",
        "archive_sha256": "2dd418a8974dbb373ad281f99b6f95ed02e01b1a2bb2e05e772c80b7587577b5",
        "payload_sha256": "626965d480a636087b3337a544e83e2de340d0f139fc4a157812ea98daaf2812",
    },
}

LAUNCHER = '''#!/bin/sh
set -eu
case "$0" in
  */*) launcher=$0 ;;
  *) launcher=$(command -v "$0") ;;
esac
links=0
while [ -L "$launcher" ]; do
  links=$((links + 1))
  if [ "$links" -gt 40 ]; then
    echo "gptgrep: launcher symlink chain is too long" >&2
    exit 2
  fi
  link=$(readlink "$launcher")
  case "$link" in
    /*) launcher=$link ;;
    *) launcher=$(dirname -- "$launcher")/$link ;;
  esac
done
bundle_dir=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd -P)
export PDFIUM_LIB_PATH="$bundle_dir/lib"
if [ "${GPTGREP_TRACE_LIBRARIES:-}" = 1 ]; then
  case $(uname -s) in
    Darwin) export DYLD_PRINT_LIBRARIES=1 ;;
    Linux) export LD_DEBUG=libs ;;
  esac
fi
exec "$bundle_dir/gptgrep.bin" "$@"
'''


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def regular(path: Path) -> None:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"required regular file is missing or is a symlink: {path.name}")


def copy_file(source: Path, destination: Path, executable: bool = False) -> None:
    regular(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def verify_architecture(path: Path, target: str) -> None:
    with path.open("rb") as stream:
        head = stream.read(32)
    if target == "aarch64-apple-darwin":
        valid = head[:4] == b"\xcf\xfa\xed\xfe" and int.from_bytes(head[4:8], "little") == 0x100000C
    elif target == "aarch64-unknown-linux-gnu":
        valid = head[:6] == b"\x7fELF\x02\x01" and int.from_bytes(head[18:20], "little") == 183
    else:
        valid = head[:6] == b"\x7fELF\x02\x01" and int.from_bytes(head[18:20], "little") == 62
    if not valid:
        raise ValueError(f"binary architecture does not match {target}: {path.name}")


def verify_pdfium(directory: Path, target: str) -> dict[str, str]:
    expected = TARGETS[target]
    license_dir = directory / "licenses"
    if not license_dir.is_dir() or license_dir.is_symlink():
        raise ValueError("PDFium licenses directory is missing or is a symlink")
    paths = [directory / "LICENSE", directory / "VERSION", directory / "lib" / expected["library"]]
    paths.extend(item for item in license_dir.rglob("*") if not item.is_dir())
    payload = {}
    for item in paths:
        regular(item)
        payload[item.relative_to(directory).as_posix()] = sha256(item)
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    if hashlib.sha256(encoded).hexdigest() != expected["payload_sha256"]:
        raise ValueError("PDFium library/notices differ from the pinned release payload; supply an unmodified extracted archive")
    verify_architecture(directory / "lib" / expected["library"], target)
    return payload


def source_state(repo: Path) -> tuple[str | None, bool]:
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True)
    if head.returncode:
        return None, True
    status = subprocess.run(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=repo, check=True, capture_output=True, text=True)
    return head.stdout.strip(), bool(status.stdout)


def archive_bundle(bundle: Path, output: Path) -> None:
    with output.open("xb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as archive:
            for item in [bundle, *sorted(bundle.rglob("*"))]:
                if item.is_symlink():
                    raise ValueError("release bundles must not contain symlinks")
                info = archive.gettarinfo(str(item), arcname=item.relative_to(bundle.parent).as_posix())
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = 0
                if item.is_file():
                    with item.open("rb") as stream:
                        archive.addfile(info, stream)
                else:
                    archive.addfile(info)


def synthetic_pdf() -> bytes:
    stream = b"BT /F1 14 Tf 72 720 Td (GPTgrepPackageRelocationMarker) Tj ET\n"
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"endstream",
    ]
    data = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for number, body in enumerate(objects, 1):
        offsets.append(len(data))
        data.extend(f"{number} 0 obj\n".encode() + body + b"\nendobj\n")
    xref = len(data)
    data.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        data.extend(f"{offset:010} 00000 n \n".encode())
    data.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
    return bytes(data)


def smoke_archive(archive: Path, target: str) -> dict:
    expected = TARGETS[target]
    if (platform.system(), platform.machine()) != (expected["system"], expected["machine"]):
        raise ValueError("archive smoke tests must run on the declared native target")
    with tempfile.TemporaryDirectory(prefix="gptgrep archive smoke ") as temporary:
        root = Path(temporary).resolve()
        with tarfile.open(archive) as packed:
            packed.extractall(root, filter="data")
        bundles = [item for item in root.iterdir() if item.is_dir()]
        if len(bundles) != 1:
            raise ValueError("archive must contain exactly one bundle directory")
        bundle = bundles[0]
        launcher = bundle / "gptgrep"
        environment = {**os.environ, "PDFIUM_LIB_PATH": str(root / "absent library"), "GPTGREP_TRACE_LIBRARIES": "1"}
        # Never let provider credentials turn an offline smoke test into live work.
        for name in ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "OPENAI_API_KEY"):
            environment.pop(name, None)
        reports = {}
        for kind in ("plain", "pdf"):
            corpus = root / f"{kind} documents"
            corpus.mkdir()
            if kind == "pdf":
                (corpus / "sample.pdf").write_bytes(synthetic_pdf())
            else:
                (corpus / "sample.txt").write_text("GPTgrepPackageRelocationMarker\n", encoding="utf-8")
            indexed = subprocess.run([str(launcher), "index", str(corpus), "--json"], env=environment, cwd=root, capture_output=True, text=True, timeout=120)
            if indexed.returncode:
                raise ValueError(f"relocated {kind} indexing failed: {indexed.stderr[-1500:]}")
            found = subprocess.run([str(launcher), "search", "GPTgrepPackageRelocationMarker", str(corpus), "--mode", "regex", "--json"], env=environment, cwd=root, capture_output=True, text=True, timeout=30)
            hits = json.loads(found.stdout).get("hits", []) if found.returncode == 0 else []
            if not hits or not all(hit.get("source_fresh") is True for hit in hits):
                raise ValueError(f"relocated {kind} search did not return fresh fixture evidence")
            lines = [line for line in indexed.stderr.splitlines() if "libpdfium" in line]
            if kind == "plain" and lines:
                raise ValueError("plaintext-only indexing unexpectedly loaded PDFium")
            if kind == "pdf":
                wanted = str((bundle / "lib" / expected["library"]).resolve())
                loaded = [line for line in lines if platform.system() == "Darwin" or "calling init:" in line]
                if not loaded or not all(wanted in line for line in loaded):
                    raise ValueError("loader evidence does not establish the bundled PDFium library")
                if not all(hit.get("page_start") == 1 for hit in hits):
                    raise ValueError("PDF smoke evidence lacks its physical page citation")
            reports[kind] = {"indexed": True, "fresh_hits": len(hits), "pdfium_loaded": bool(lines)}
        return {"schema_version": "gptgrep.release-smoke.v1", "target": target, "archive_sha256": sha256(archive), "relocated_spaced_path": True, "bundled_pdfium_verified": True, "checks": reports}


def package(args: argparse.Namespace) -> dict:
    repo, binary, pdfium = args.repo_root.resolve(), args.binary.resolve(), args.pdfium_dir.resolve()
    target = TARGETS[args.target]
    if (platform.system(), platform.machine()) != (target["system"], target["machine"]):
        raise ValueError("package on the native target; cross-built releases are not validated")
    version = tomllib.loads((repo / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("workspace version is not a supported semantic version")
    regular(binary)
    verify_architecture(binary, args.target)
    actual = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
    if actual != f"gptgrep {version}":
        raise ValueError("native binary version does not match Cargo workspace version")
    payload = verify_pdfium(pdfium, args.target)
    for name in ("LICENSE", "THIRD_PARTY.md"):
        regular(repo / name)
    revision, dirty = source_state(repo)
    if args.require_clean and (not revision or dirty):
        raise ValueError("release packaging requires a committed, clean source tree")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    name = f"gptgrep-{version}-{args.target}"
    paths = {suffix: output / f"{name}{suffix}" for suffix in (".tar.gz", ".tar.gz.sha256", ".release.json", ".smoke.json")}
    if any(item.exists() for item in paths.values()):
        raise ValueError("release output already exists; choose a new output directory")
    with tempfile.TemporaryDirectory(prefix=".gptgrep-package-", dir=output) as temporary:
        bundle = Path(temporary) / name
        bundle.mkdir()
        copy_file(binary, bundle / "gptgrep.bin", executable=True)
        (bundle / "gptgrep").write_text(LAUNCHER, encoding="utf-8")
        (bundle / "gptgrep").chmod(0o755)
        copy_file(pdfium / "lib" / target["library"], bundle / "lib" / target["library"], executable=True)
        for relative in payload:
            if not relative.startswith("lib/"):
                copy_file(pdfium / relative, bundle / "third-party" / "pdfium" / relative)
        copy_file(repo / "LICENSE", bundle / "LICENSE")
        copy_file(repo / "THIRD_PARTY.md", bundle / "THIRD_PARTY.md")
        if (repo / "docs" / "release.md").is_file():
            copy_file(repo / "docs" / "release.md", bundle / "docs" / "release.md")
        # Retain project-owned upstream notices at their source-relative paths.
        for base in [repo / "licenses", *sorted((repo / "crates").glob("*/licenses"))]:
            if base.is_dir():
                for original in sorted(base.rglob("*")):
                    if original.is_file():
                        copy_file(original, bundle / original.relative_to(repo))
        for original in sorted((repo / "crates").glob("*/LICENSE*")):
            copy_file(original, bundle / original.relative_to(repo))
        inventory = collect(repo, args.target, bundle / "third-party" / "rust", require_complete=True)
        metadata = {
            "schema_version": "gptgrep.release.v1", "version": version, "target": args.target,
            "source_revision": revision, "source_dirty": dirty,
            "binary": {"file": "gptgrep.bin", "sha256": sha256(bundle / "gptgrep.bin")},
            "pdfium": {"repository": "https://github.com/run-llama/pdfium-binaries", "tag": PDFIUM_TAG,
                "revision": PDFIUM_REVISION, "asset": target["asset"], "archive_sha256": target["archive_sha256"],
                "payload_sha256": target["payload_sha256"], "version": (pdfium / "VERSION").read_text().strip(),
                "library": f"lib/{target['library']}", "library_sha256": payload[f"lib/{target['library']}"]},
            "rust_licenses": {"complete": inventory["complete"], "packages": len(inventory["packages"]), "inventory": "third-party/rust/inventory.json"},
            "files": {item.relative_to(bundle).as_posix(): sha256(item) for item in sorted(bundle.rglob("*")) if item.is_file()},
        }
        encoded = json.dumps(metadata, indent=2, sort_keys=True) + "\n"
        (bundle / "RELEASE.json").write_text(encoded, encoding="utf-8")
        checksums = [f"{sha256(item)}  {item.relative_to(bundle).as_posix()}" for item in sorted(bundle.rglob("*")) if item.is_file()]
        (bundle / "SHA256SUMS").write_text("\n".join(checksums) + "\n", encoding="utf-8")
        staged_archive = Path(temporary) / paths[".tar.gz"].name
        archive_bundle(bundle, staged_archive)
        smoke = smoke_archive(staged_archive, args.target) if args.smoke_test else None
        # Publish outputs only after every requested check has succeeded.
        shutil.move(staged_archive, paths[".tar.gz"])
        paths[".release.json"].write_text(encoded, encoding="utf-8")
        paths[".tar.gz.sha256"].write_text(f"{sha256(paths['.tar.gz'])}  {paths['.tar.gz'].name}\n", encoding="utf-8")
        if smoke:
            paths[".smoke.json"].write_text(json.dumps(smoke, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {"archive": paths[".tar.gz"].name, "sha256": sha256(paths[".tar.gz"]), "version": version, "target": args.target, "rust_license_packages": len(inventory["packages"]), "smoke_passed": smoke is not None, "source_dirty": dirty}


def self_test() -> None:
    class PackagingTests(unittest.TestCase):
        def test_launcher_spaces_symlink_and_arguments(self):
            with tempfile.TemporaryDirectory(prefix="gptgrep launcher test ") as directory:
                root = Path(directory).resolve()
                launcher = root / "gptgrep"
                launcher.write_text(LAUNCHER)
                launcher.chmod(0o755)
                native = root / "gptgrep.bin"
                native.write_text('#!/usr/bin/env python3\nimport json,os,sys\nprint(json.dumps({"args":sys.argv[1:],"pdfium":os.environ["PDFIUM_LIB_PATH"]}))\n')
                native.chmod(0o755)
                alias = root / "symlink with spaces"
                alias.symlink_to("gptgrep")
                result = subprocess.run([str(alias), "--", "a b", "$(false)", ""], check=True, capture_output=True, text=True, env={**os.environ, "PDFIUM_LIB_PATH": "wrong"})
                self.assertEqual(json.loads(result.stdout), {"args": ["--", "a b", "$(false)", ""], "pdfium": str(root / "lib")})

        def test_archive_determinism_and_modes(self):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                bundle = root / "bundle"
                bundle.mkdir()
                executable = bundle / "gptgrep"
                executable.write_text("payload")
                executable.chmod(0o755)
                first, second = root / "one.tar.gz", root / "two.tar.gz"
                archive_bundle(bundle, first)
                os.utime(executable, (200, 200))
                archive_bundle(bundle, second)
                self.assertEqual(sha256(first), sha256(second))
                with tarfile.open(first) as archive:
                    self.assertEqual(stat.S_IMODE(archive.getmember("bundle/gptgrep").mode), 0o755)

        def test_linux_arm64_and_amd64_architectures_are_distinct(self):
            cases = (
                ("aarch64-unknown-linux-gnu", 183, "x86_64-unknown-linux-gnu"),
                ("x86_64-unknown-linux-gnu", 62, "aarch64-unknown-linux-gnu"),
            )
            for target, machine, other_target in cases:
                with self.subTest(target=target), tempfile.NamedTemporaryFile() as file:
                    head = bytearray(32)
                    head[:6] = b"\x7fELF\x02\x01"
                    head[18:20] = machine.to_bytes(2, "little")
                    file.write(head)
                    file.flush()
                    verify_architecture(Path(file.name), target)
                    with self.assertRaisesRegex(ValueError, "binary architecture does not match"):
                        verify_architecture(Path(file.name), other_target)

        def test_wrong_architecture_rejected(self):
            with tempfile.NamedTemporaryFile() as file:
                file.write(b"not a native binary")
                file.flush()
                with self.assertRaises(ValueError):
                    verify_architecture(Path(file.name), "aarch64-apple-darwin")

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PackagingTests))
    if not result.wasSuccessful():
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--pdfium-dir", type=Path)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-clean", action="store_true")
    parser.add_argument("--smoke-test", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if any(value is None for value in (args.binary, args.pdfium_dir, args.target, args.output)):
        parser.error("--binary, --pdfium-dir, --target and --output are required")
    print(json.dumps(package(args), sort_keys=True))


if __name__ == "__main__":
    main()
