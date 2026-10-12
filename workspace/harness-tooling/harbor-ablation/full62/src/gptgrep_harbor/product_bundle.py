"""Validate and stage the complete pinned Linux release; never execute its code."""
from __future__ import annotations

import hashlib
import gzip
import io
import os
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import tarfile

from .common import MAX_JSON, copy_asset, digest, integer, parse, pinned, relative, require, sha

PRODUCT_ROOT = "/opt/full62-product"
PRODUCT_ENTRYPOINT = PRODUCT_ROOT + "/gptgrep"
PRODUCT_PROVENANCE = "/usr/local/share/full62-product/provenance.json"
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_MEMBER = 256 * 1024 * 1024
MAX_EXPANDED = 1024 * 1024 * 1024
# Bound all decompressed bytes, including metadata that tarfile consumes before
# returning a member. Header/padding overhead is finite as well as payload size.
MAX_TAR_BYTES = MAX_EXPANDED + 16 * 1024 * 1024
TARGETS = {"linux/amd64": ("x86_64-unknown-linux-gnu", 62),
           "linux/arm64": ("aarch64-unknown-linux-gnu", 183)}
REQUIRED = {"gptgrep", "gptgrep.bin", "lib/libpdfium.so", "LICENSE", "THIRD_PARTY.md",
            "third-party/rust/inventory.json"}
RUNTIME_SKILLS = ["gptgrep", "gptgrep-document", "gptgrep-ocr", "gptgrep-retrieval"]
RUNTIME_ROLES = ["gptgrep-document", "gptgrep-ocr", "gptgrep-retrieval"]
RUNTIME_FILES = {"runtime/codex/AGENTS.md", "runtime/codex/DEPENDENCIES.md",
                 "runtime/codex/STAGING.md", "runtime/codex/assets.sha256",
                 "runtime/codex/manifest.json", "runtime/codex/stage.sh",
                 *("skills/" + name + "/SKILL.md" for name in RUNTIME_SKILLS),
                 *("runtime/codex/agents/" + name + ".toml" for name in RUNTIME_ROLES)}


def member_path(value):
    relative(value)
    require(len(value.encode()) <= 1024 and not any(c in value for c in "\x00\r\n\t"),
            "bundle_member_path")
    return value


def _elf(header, machine):
    require(len(header) >= 64 and header[:6] == b"\x7fELF\x02\x01" and
            struct.unpack("<H", header[18:20])[0] == machine, "bundle_linux_elf")


def _bounded_tar(archive):
    chunks, total = [], 0
    with gzip.GzipFile(fileobj=io.BytesIO(archive), mode="rb") as source:
        while True:
            block = source.read1(min(1024 * 1024, MAX_TAR_BYTES + 1 - total))
            if not block:
                return b"".join(chunks)
            total += len(block)
            require(total <= MAX_TAR_BYTES, "bundle_tar_expanded_bound")
            chunks.append(block)


def release_bundle(bundle, platform, destination=None):
    """Verify every member before creating a destination; copy only verified bytes.

    Compressed input is pinned, then fully decompressed through a read ceiling
    before tar parsing. Both member passes use the same bounded immutable tar
    bytes, so extraction cannot race its source or inflate hidden metadata.
    """
    require(type(bundle) is dict and set(bundle) == {"archive", "release_metadata"}, "bundle_input_fields")
    require(platform in TARGETS, "bundle_linux_platform")
    archive = pinned(bundle["archive"], MAX_ARCHIVE)
    metadata = pinned(bundle["release_metadata"])
    release = parse(metadata)
    fields = {"schema_version", "version", "target", "source_revision", "source_dirty",
              "binary", "pdfium", "rust_licenses", "files"}
    require(set(release) in (fields, fields | {"native_runtime_package"}) and
            release["schema_version"] == "gptgrep.release.v1" and
            release["target"] == TARGETS[platform][0], "bundle_release_schema")
    require(isinstance(release["version"], str) and
            re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,79}", release["version"]), "bundle_version")
    require(type(release["source_dirty"]) is bool and (release["source_revision"] is None or
            isinstance(release["source_revision"], str) and
            re.fullmatch(r"[0-9a-f]{40}", release["source_revision"])), "bundle_source_metadata")
    files = release["files"]
    require(type(files) is dict and REQUIRED <= files.keys() and len(files) <= 4096 and
            not {"RELEASE.json", "SHA256SUMS"} & files.keys(), "bundle_manifest_files")
    for name, value in files.items():
        member_path(name)
        digest(value)
    if "native_runtime_package" in release:
        runtime = release["native_runtime_package"]
        require(type(runtime) is dict and set(runtime) == {
            "root", "manifest", "manifest_sha256", "skills", "roles", "files"}
            and runtime["root"] == "." and runtime["manifest"] == "runtime/codex/manifest.json"
            and runtime["skills"] == RUNTIME_SKILLS and runtime["roles"] == RUNTIME_ROLES,
            "bundle_runtime_schema")
        require(type(runtime["files"]) is dict and set(runtime["files"]) == RUNTIME_FILES
                and RUNTIME_FILES <= files.keys()
                and all(runtime["files"][name] == files[name] for name in RUNTIME_FILES)
                and runtime["manifest_sha256"] == files[runtime["manifest"]],
                "bundle_runtime_manifest_join")
    require(release["binary"] == {"file": "gptgrep.bin", "sha256": files["gptgrep.bin"]} and
            release["pdfium"].get("library") == "lib/libpdfium.so" and
            release["pdfium"].get("library_sha256") == files["lib/libpdfium.so"], "bundle_binary_library_join")
    licenses = release["rust_licenses"]
    require(type(licenses) is dict and set(licenses) == {"complete", "inventory", "packages"} and
            licenses["complete"] is True and licenses["inventory"] == "third-party/rust/inventory.json",
            "bundle_license_inventory")
    integer(licenses["packages"], 1, 4096)
    top = "gptgrep-" + release["version"] + "-" + release["target"]
    expected = {**files, "RELEASE.json": sha(metadata)}
    sums = "".join(value + "  " + name + "\n" for name, value in sorted(expected.items())).encode()
    expected["SHA256SUMS"] = sha(sums)
    rows, members, names, directory_names, total = {}, {}, set(), set(), 0
    tar_bytes = _bounded_tar(archive)
    with tarfile.open(fileobj=io.BytesIO(tar_bytes), mode="r:") as tar:
        for member in tar:
            name = member_path(member.name)
            require(name not in names and len(names) < 8192, "bundle_member_duplicate_or_count")
            names.add(name)
            path = PurePosixPath(name)
            require(path.parts[0] == top and member.type in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE) and
                    member.mode in (0o644, 0o755) and member.sparse is None, "bundle_member_type_or_root")
            if member.isdir():
                require(member.size == 0, "bundle_directory_size")
                directory_names.add(name)
                continue
            require(len(path.parts) > 1, "bundle_root_file")
            rel = "/".join(path.parts[1:])
            require(rel in expected, "bundle_unmanifested_file")
            integer(member.size, 0, MAX_MEMBER)
            total += member.size
            require(total <= MAX_EXPANDED, "bundle_expanded_bound")
            stream = tar.extractfile(member)
            require(stream is not None, "bundle_member_stream")
            hasher, size, prefix = hashlib.sha256(), 0, b""
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                size += len(block)
                hasher.update(block)
                if len(prefix) < 4096:
                    prefix += block[:4096 - len(prefix)]
            require(size == member.size and hasher.hexdigest() == expected[rel], "bundle_member_digest")
            if rel in ("gptgrep.bin", "lib/libpdfium.so"):
                _elf(prefix, TARGETS[platform][1])
            if rel == "gptgrep":
                require(member.mode == 0o755 and prefix.startswith(b"#!/bin/sh\n") and
                        b'export PDFIUM_LIB_PATH="$bundle_dir/lib"\n' in prefix and
                        prefix.endswith(b'exec "$bundle_dir/gptgrep.bin" "$@"\n'), "bundle_launcher_layout")
            if rel == "gptgrep.bin":
                require(member.mode == 0o755, "bundle_binary_executable")
            if rel == "third-party/rust/inventory.json":
                require(member.size <= MAX_JSON, "bundle_license_inventory_size")
                inventory = parse(tar.extractfile(member).read(MAX_JSON + 1))
                require(inventory.get("schema_version") == "gptgrep.licenses.v1" and
                        inventory.get("target") == release["target"] and inventory.get("complete") is True and
                        inventory.get("missing_license_files") == [] and
                        isinstance(inventory.get("packages"), list) and
                        len(inventory["packages"]) == licenses["packages"], "bundle_license_inventory_join")
            rows[rel] = {"bytes": size, "sha256": expected[rel]}
            members[rel] = member
        require(set(rows) == set(expected), "bundle_missing_manifest_file")
        # A directory may only be an ancestor of a declared file. No hidden empty
        # subtree or file/directory collision is accepted.
        directories = {top}
        for name in rows:
            directories.update(top + "/" + p.as_posix() for p in PurePosixPath(name).parents
                               if p.as_posix() != ".")
        require(directory_names == directories and
                names == directories | {top + "/" + n for n in rows}, "bundle_directory_membership")
        if destination is not None:
            destination = Path(destination).absolute()
            require(destination.resolve() == destination and not destination.exists(), "fresh_bundle_destination")
            destination.mkdir(mode=0o755, parents=False)
            for name, member in members.items():
                path = destination / name
                path.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                             0o555 if name in ("gptgrep", "gptgrep.bin") else 0o444)
                with os.fdopen(fd, "wb") as target:
                    stream = tar.extractfile(member)
                    for block in iter(lambda: stream.read(1024 * 1024), b""):
                        target.write(block)
                copy_asset({"path": str(path), **rows[name]}, MAX_MEMBER)
    return {"schema": "gptgrep.harbor.product-bundle.v1", "platform": platform,
            "root": PRODUCT_ROOT, "release_version": release["version"],
            "archive": {k: bundle["archive"][k] for k in ("bytes", "sha256")},
            "release_metadata": {k: bundle["release_metadata"][k] for k in ("bytes", "sha256")},
            "rust_license_packages": licenses["packages"], "files": rows,
            "entrypoint": {"path": PRODUCT_ENTRYPOINT, **rows["gptgrep"]},
            "binary": {"path": PRODUCT_ROOT + "/gptgrep.bin", **rows["gptgrep.bin"]},
            "pdfium": {"path": PRODUCT_ROOT + "/lib/libpdfium.so", **rows["lib/libpdfium.so"]}}


def bundle_identity(binding, platform, entrypoint, common_image):
    require(type(binding) is dict and set(binding) == {"provenance"}, "product_bundle_required")
    value = parse(pinned(binding["provenance"]))
    require(set(value) == {"schema", "platform", "root", "release_version", "archive", "release_metadata",
                           "rust_license_packages", "files", "entrypoint", "binary", "pdfium", "common_image"}
            and value.get("schema") == "gptgrep.harbor.product-bundle.v1" and platform in TARGETS and
            value.get("platform") == platform and value.get("common_image") == common_image and
            value.get("root") == PRODUCT_ROOT, "product_bundle_identity")
    files = value["files"]
    require(type(files) is dict and REQUIRED | {"RELEASE.json", "SHA256SUMS"} <= files.keys()
            and len(files) <= 4098, "product_bundle_files")
    for name, pin in files.items():
        member_path(name)
        require(type(pin) is dict and set(pin) == {"bytes", "sha256"}, "product_bundle_file_pin")
        integer(pin["bytes"], 0, MAX_MEMBER)
        digest(pin["sha256"])
    require(sum(p["bytes"] for p in files.values()) <= MAX_EXPANDED, "product_bundle_total")
    for name, cap in (("archive", MAX_ARCHIVE), ("release_metadata", MAX_JSON)):
        require(type(value[name]) is dict and set(value[name]) == {"bytes", "sha256"}, "product_bundle_source_pin")
        integer(value[name]["bytes"], 1, cap)
        digest(value[name]["sha256"])
    require(value["release_metadata"] == files["RELEASE.json"], "product_bundle_release_join")
    integer(value["rust_license_packages"], 1, 4096)
    for field, name in (("entrypoint", "gptgrep"), ("binary", "gptgrep.bin"), ("pdfium", "lib/libpdfium.so")):
        require(value[field] == {"path": PRODUCT_ROOT + "/" + name, **files[name]}, "product_bundle_layout")
    require(entrypoint == value["entrypoint"], "product_entrypoint_pin")
    return value


def check_product_bundle(config):
    require(config["gptgrep_binary"] == PRODUCT_ENTRYPOINT, "product_entrypoint_path")
    require(config.get("gptgrep_bundle", {}).get("provenance", {}).get("path") == PRODUCT_PROVENANCE,
            "product_provenance_path")
    value = bundle_identity(config.get("gptgrep_bundle"), config["platform"],
                            config["gptgrep_binary_pin"], config["base_tool_environment"]["image"])
    root = Path(PRODUCT_ROOT)
    require(root.resolve() == root and root.is_dir(), "installed_bundle_root")
    found = set()
    directories = {p.as_posix() for name in value["files"] for p in PurePosixPath(name).parents}
    def walk_error(error):
        raise error
    for directory, dirs, files in os.walk(root, followlinks=False, onerror=walk_error):
        for name in dirs + files:
            path = Path(directory) / name
            info = path.lstat()
            require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "installed_bundle_member_type")
            if stat.S_ISDIR(info.st_mode):
                require(path.relative_to(root).as_posix() in directories, "installed_bundle_extra_directory")
        for name in files:
            relative_path = (Path(directory) / name).relative_to(root).as_posix()
            require(relative_path in value["files"], "installed_bundle_extra_file")
            found.add(relative_path)
    require(found == set(value["files"]), "installed_bundle_missing_file")
    for name, pin in value["files"].items():
        copy_asset({"path": str(root / name), **pin}, MAX_MEMBER)
    require(os.access(PRODUCT_ENTRYPOINT, os.X_OK) and os.access(root / "gptgrep.bin", os.X_OK),
            "installed_bundle_executable")
    return value
