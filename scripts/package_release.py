#!/usr/bin/env python3
"""Package a native GPTgrep binary or an offline Rust SDK with pinned PDFium."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import tomllib
import unittest

from collect_licenses import collect, supplement_files


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

RUNTIME_SKILLS = ("gptgrep", "gptgrep-document", "gptgrep-ocr", "gptgrep-retrieval")
RUNTIME_ROLES = ("gptgrep-document", "gptgrep-ocr", "gptgrep-retrieval")
RUNTIME_STAGED_FILES = (
    "runtime/codex/AGENTS.md",
    *(f"runtime/codex/agents/{name}.toml" for name in RUNTIME_ROLES),
    *(f"skills/{name}/SKILL.md" for name in RUNTIME_SKILLS),
)
RUNTIME_PACKAGE_FILES = (
    *RUNTIME_STAGED_FILES, "runtime/codex/DEPENDENCIES.md", "runtime/codex/STAGING.md",
    "runtime/codex/assets.sha256", "runtime/codex/manifest.json", "runtime/codex/stage.sh",
)

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


def copy_runtime_assets(repo: Path, destination: Path) -> dict:
    """Ship the selected portable runtime package without home configuration."""
    manifest = repo / "runtime/codex/manifest.json"
    regular(manifest)
    data = json.loads(manifest.read_text(encoding="utf-8"))
    if (data.get("schema") != "gptgrep.native-runtime-package.v1"
            or {entry["source_path"] for entry in data["files"]} != set(RUNTIME_STAGED_FILES)
            or len(data["files"]) != len(RUNTIME_STAGED_FILES)):
        raise ValueError("native runtime manifest does not select the expected portable assets")
    for entry in data["files"]:
        original = repo / entry["source_path"]
        regular(original)
        if original.stat().st_size != entry["bytes"] or sha256(original) != entry["sha256"]:
            raise ValueError("native runtime asset does not match its manifest")
    files = {}
    for relative in RUNTIME_PACKAGE_FILES:
        original = repo / relative
        if original.resolve() != original:
            raise ValueError("native runtime assets must not traverse symlinks")
        copy_file(original, destination / relative, executable=relative == "runtime/codex/stage.sh")
        files[relative] = sha256(destination / relative)
    return {"manifest": "runtime/codex/manifest.json", "manifest_sha256": files["runtime/codex/manifest.json"],
            "skills": list(RUNTIME_SKILLS), "roles": list(RUNTIME_ROLES), "files": files}


def copy_sdk_sources(repo: Path, destination: Path) -> dict[str, str]:
    """Copy the explicit public source closure; never copy repository state."""
    files = [repo / name for name in ("Cargo.toml", "Cargo.lock", "LICENSE", "THIRD_PARTY.md",
                                      "docs/rust-sdk.md")]
    for name in ("crates", "vendor/liteparse-pdfium", "licenses", "examples/rust-ptc-consumer"):
        base = repo / name
        if base.resolve() != base or not base.is_dir():
            raise ValueError(f"SDK source directory is missing or is a symlink: {name}")
        for item in sorted(base.rglob("*")):
            if item.is_symlink():
                raise ValueError("SDK source trees must not contain symlinks")
            relative = item.relative_to(repo)
            if any(part in {".git", ".local", "target", "node_modules", "__pycache__"}
                   or part == ".env" or part.startswith(".env.") for part in relative.parts):
                raise ValueError("private or generated files found in the SDK source tree")
            if item.is_file():
                files.append(item)
    for item in files:
        if item.resolve() != item:
            raise ValueError("SDK source paths must not traverse symlinks")
        copy_file(item, destination / item.relative_to(repo))
    runtime = copy_runtime_assets(repo, destination)
    return {**{item.relative_to(repo).as_posix(): sha256(destination / item.relative_to(repo))
               for item in sorted(files)}, **runtime["files"]}


def extract_sdk_pdfium(archive: Path, destination: Path, target: str) -> dict[str, str]:
    """Use the pinned archive so headers share the library's source identity."""
    regular(archive)
    if archive.stat().st_size > 64 * 1024 * 1024:
        raise ValueError("PDFium archive exceeds the SDK input limit")
    data = archive.read_bytes()
    if hashlib.sha256(data).hexdigest() != TARGETS[target]["archive_sha256"]:
        raise ValueError("PDFium archive differs from the pinned release")
    destination.mkdir()
    seen, total = set(), 0
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as packed:
        for member in packed:
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or path in seen or len(seen) >= 1024:
                raise ValueError("invalid PDFium archive member path")
            seen.add(path)
            if not (member.isdir() or member.isfile()) or member.sparse is not None:
                raise ValueError("PDFium archive must contain ordinary files and directories")
            total += member.size
            if total > 256 * 1024 * 1024:
                raise ValueError("PDFium archive exceeds the SDK expansion limit")
            output = destination / path
            if member.isdir():
                output.mkdir(parents=True, exist_ok=True)
            else:
                output.parent.mkdir(parents=True, exist_ok=True)
                with packed.extractfile(member) as source, output.open("xb") as stream:
                    shutil.copyfileobj(source, stream)
                output.chmod(0o755 if path.parts[0] == "lib" else 0o644)
    verify_pdfium(destination, target)
    regular(destination / "include" / "fpdfview.h")
    return {item.relative_to(destination).as_posix(): sha256(item)
            for item in sorted(destination.rglob("*")) if item.is_file()}


def package_rust_sdk(args: argparse.Namespace) -> dict:
    repo, output = args.repo_root.resolve(), args.output.resolve()
    version = tomllib.loads((repo / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("workspace version is not a supported semantic version")
    revision, dirty = source_state(repo)
    if args.require_clean and (not revision or dirty):
        raise ValueError("SDK packaging requires a committed, clean source tree")
    output.mkdir(parents=True, exist_ok=True)
    name = f"gptgrep-{version}-{args.target}-rust-sdk"
    paths = {suffix: output / f"{name}{suffix}" for suffix in (".tar.gz", ".tar.gz.sha256", ".sdk.json")}
    if any(item.exists() for item in paths.values()):
        raise ValueError("SDK output already exists; choose a new output directory")
    environment = {**os.environ, "CARGO_NET_OFFLINE": "true"}
    with tempfile.TemporaryDirectory(prefix=".gptgrep-sdk-", dir=output) as temporary:
        bundle = Path(temporary) / name
        source = bundle / "source"
        source.mkdir(parents=True)
        sources = copy_sdk_sources(repo, source)
        copy_file(repo / "LICENSE", bundle / "LICENSE")
        copy_file(repo / "THIRD_PARTY.md", bundle / "THIRD_PARTY.md")
        # Collect before vendoring so Git repository-level notices remain visible.
        inventory = collect(repo, args.target, bundle / "third-party" / "rust",
                            require_complete=True, all_dependencies=True)
        consumer = source / "examples" / "rust-ptc-consumer"
        # Git source replacement needs a preexisting lock. Preserve workspace
        # versions while resolving the independent consumer before enabling it.
        copy_file(source / "Cargo.lock", consumer / "Cargo.lock")
        subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1",
                        "--manifest-path", str(consumer / "Cargo.toml")], cwd=source,
                       env=environment, stdout=subprocess.DEVNULL, check=True)
        vendored = subprocess.run(["cargo", "vendor", "--offline", "--locked", "--versioned-dirs",
                                   "vendor/cargo"], cwd=source, env=environment,
                                  check=True, capture_output=True, text=True)
        config = source / ".cargo" / "config.toml"
        config.parent.mkdir()
        # Cargo emits relative vendor paths because the invocation used one.
        configuration = tomllib.loads(vendored.stdout)
        if configuration.get("source", {}).get("vendored-sources", {}).get("directory") != "vendor/cargo":
            raise ValueError("Cargo did not emit the portable SDK vendor path")
        config.write_text(vendored.stdout, encoding="utf-8")
        subprocess.run(["cargo", "metadata", "--offline", "--locked", "--format-version", "1",
                        "--manifest-path", str(consumer / "Cargo.toml")], cwd=source,
                       env=environment, stdout=subprocess.DEVNULL, check=True)
        (bundle / "native").mkdir()
        pdfium = extract_sdk_pdfium(args.pdfium_archive.resolve(), bundle / "native" / "pdfium", args.target)
        metadata = {
            "schema_version": "gptgrep.rust-sdk.v1", "version": version, "target": args.target,
            "source_revision": revision, "source_dirty": dirty,
            "source_files": sources,
            "cargo": {"workspace": "source/Cargo.toml", "lockfile": "source/Cargo.lock",
                      "config": "source/.cargo/config.toml", "vendor": "source/vendor/cargo",
                      "consumer_manifest": "source/examples/rust-ptc-consumer/Cargo.toml",
                      "consumer_lockfile": "source/examples/rust-ptc-consumer/Cargo.lock"},
            "pdfium": {"tag": PDFIUM_TAG, "revision": PDFIUM_REVISION,
                       "archive_sha256": TARGETS[args.target]["archive_sha256"],
                       "library": f"native/pdfium/lib/{TARGETS[args.target]['library']}",
                       "include": "native/pdfium/include", "files": pdfium},
            "rust_licenses": {"complete": inventory["complete"], "packages": len(inventory["packages"]),
                              "inventory": "third-party/rust/inventory.json"},
            "native_runtime_package": {"root": "source", "manifest": "runtime/codex/manifest.json",
                                       "manifest_sha256": sources["runtime/codex/manifest.json"],
                                       "files": {name: sources[name] for name in RUNTIME_PACKAGE_FILES},
                                       "skills": list(RUNTIME_SKILLS), "roles": list(RUNTIME_ROLES)},
            "toolchain_bundled": False, "consumer_validation": "not_run_by_packager",
            "files": {item.relative_to(bundle).as_posix(): sha256(item)
                      for item in sorted(bundle.rglob("*")) if item.is_file()},
        }
        encoded = json.dumps(metadata, indent=2, sort_keys=True) + "\n"
        (bundle / "SDK.json").write_text(encoded, encoding="utf-8")
        sums = [f"{sha256(item)}  {item.relative_to(bundle).as_posix()}"
                for item in sorted(bundle.rglob("*")) if item.is_file()]
        (bundle / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8")
        staged = Path(temporary) / paths[".tar.gz"].name
        archive_bundle(bundle, staged)
        shutil.move(staged, paths[".tar.gz"])
        paths[".sdk.json"].write_text(encoded, encoding="utf-8")
        paths[".tar.gz.sha256"].write_text(f"{sha256(paths['.tar.gz'])}  {paths['.tar.gz'].name}\n", encoding="utf-8")
    return {"archive": paths[".tar.gz"].name, "sha256": sha256(paths[".tar.gz"]),
            "manifest": paths[".sdk.json"].name, "version": version, "target": args.target,
            "rust_license_packages": len(inventory["packages"]), "source_dirty": dirty,
            "toolchain_bundled": False, "consumer_validation": "not_run_by_packager"}


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
        runtime = copy_runtime_assets(repo, bundle)
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
            "native_runtime_package": {"root": ".", **runtime},
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

        def test_sdk_source_allowlist_and_symlink_refusal(self):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                repo = root / "repo"
                for name in ("crates", "vendor/liteparse-pdfium", "licenses",
                             "examples/rust-ptc-consumer", "docs", ".local"):
                    (repo / name).mkdir(parents=True)
                for name in ("Cargo.toml", "Cargo.lock", "LICENSE", "THIRD_PARTY.md",
                             "docs/rust-sdk.md", "crates/public.rs", ".local/private.txt"):
                    (repo / name).write_text(name)
                for name in RUNTIME_PACKAGE_FILES:
                    (repo / name).parent.mkdir(parents=True, exist_ok=True)
                    (repo / name).write_text(name)
                manifest = {"schema": "gptgrep.native-runtime-package.v1", "files": [
                    {"source_path": name, "bytes": (repo / name).stat().st_size,
                     "sha256": sha256(repo / name)} for name in RUNTIME_STAGED_FILES]}
                (repo / "runtime/codex/manifest.json").write_text(json.dumps(manifest))
                copied = copy_sdk_sources(repo, root / "copied")
                self.assertIn("crates/public.rs", copied)
                self.assertIn("skills/gptgrep/SKILL.md", copied)
                self.assertIn("runtime/codex/manifest.json", copied)
                self.assertEqual(stat.S_IMODE((root / "copied/runtime/codex/stage.sh").stat().st_mode), 0o755)
                self.assertFalse((root / "copied/.local").exists())
                (repo / "skills/gptgrep/SKILL.md").write_text("changed asset")
                with self.assertRaisesRegex(ValueError, "does not match"):
                    copy_runtime_assets(repo, root / "changed")
                (repo / "crates/escape.rs").symlink_to(repo / ".local/private.txt")
                with self.assertRaisesRegex(ValueError, "symlink"):
                    copy_sdk_sources(repo, root / "rejected")

        def test_sdk_rejects_unpinned_pdfium_before_extraction(self):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive = root / "wrong.tgz"
                archive.write_bytes(b"untrusted archive")
                with self.assertRaisesRegex(ValueError, "pinned release"):
                    extract_sdk_pdfium(archive, root / "payload", "aarch64-unknown-linux-gnu")
                self.assertFalse((root / "payload").exists())

        def test_license_without_vcs_requires_locked_archive_and_complete_source(self):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                package_root = root / "cached-package"
                (package_root / "src").mkdir(parents=True)
                (package_root / "Cargo.toml").write_text("normalized manifest")
                (package_root / "Cargo.toml.orig").write_text("original manifest")
                (package_root / "src/lib.rs").write_text("pub fn example() {}")
                package = {"name": "fixture", "version": "1.0.0",
                           "manifest_path": str(package_root / "Cargo.toml"),
                           "repository": "https://github.com/example/fixture",
                           "source": "registry+https://github.com/rust-lang/crates.io-index"}
                (root / "Cargo.lock").write_text(
                    '[[package]]\nname = "fixture"\nversion = "1.0.0"\n'
                    'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                    f'checksum = "{"a" * 64}"\n')
                folder = root / "licenses/rust-supplements/fixture-1.0.0"
                folder.mkdir(parents=True)
                (folder / "LICENSE").write_text("fixture permission notice")
                tracked = {}
                for name in ("Cargo.toml.orig", "src/lib.rs"):
                    data = (package_root / name).read_bytes()
                    tracked[name] = {"git_blob_sha1": hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()}
                provenance = {"package": "fixture", "version": "1.0.0", "package_vcs_revision": None,
                              "license_revision": "b" * 40, "package_archive_sha256": "a" * 64,
                              "source_comparison": {"repository": package["repository"], "revision": "b" * 40,
                                                    "tracked_files": tracked, "generated_files": {}},
                              "files": [{"file": "LICENSE", "sha256": sha256(folder / "LICENSE")}]}
                manifest = folder / "provenance.json"
                manifest.write_text(json.dumps(provenance))
                self.assertEqual(len(supplement_files(package, root)[0]), 1)
                provenance["package_archive_sha256"] = "c" * 64
                manifest.write_text(json.dumps(provenance))
                with self.assertRaisesRegex(ValueError, "archive checksum"):
                    supplement_files(package, root)
                provenance["package_archive_sha256"] = "a" * 64
                manifest.write_text(json.dumps(provenance))
                (package_root / "src/extra.rs").write_text("uncompared source")
                with self.assertRaisesRegex(ValueError, "omits packaged files"):
                    supplement_files(package, root)
                (package_root / "src/extra.rs").unlink()
                (package_root / "src/lib.rs").write_text("changed source")
                with self.assertRaisesRegex(ValueError, "tracked source no longer matches"):
                    supplement_files(package, root)

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PackagingTests))
    if not result.wasSuccessful():
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--pdfium-dir", type=Path)
    parser.add_argument("--pdfium-archive", type=Path,
                        help="Pinned upstream PDFium archive, including headers, for --rust-sdk-only")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-clean", action="store_true")
    parser.add_argument("--smoke-test", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--rust-sdk-only", action="store_true",
                        help="Package public Rust sources, offline Cargo dependencies and PDFium")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.rust_sdk_only:
        if any(value is None for value in (args.pdfium_archive, args.target, args.output)):
            parser.error("--rust-sdk-only requires --pdfium-archive, --target and --output")
        if args.binary or args.pdfium_dir or args.smoke_test:
            parser.error("binary packaging flags cannot be combined with --rust-sdk-only")
        print(json.dumps(package_rust_sdk(args), sort_keys=True))
        return
    if any(value is None for value in (args.binary, args.pdfium_dir, args.target, args.output)):
        parser.error("--binary, --pdfium-dir, --target and --output are required")
    print(json.dumps(package(args), sort_keys=True))


if __name__ == "__main__":
    main()
