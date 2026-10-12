"""Small authored Linux bundle fixtures; no executable fixture is run."""
import io
import json
from pathlib import PurePosixPath
import struct
import tarfile

from gptgrep_harbor.common import canonical, sha


def pin(path):
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data), "sha256": sha(data)}


def sandbox_helper_fixture(root, platform="linux/amd64"):
    """Authored ELF-header and license bytes; no fixture helper is executed."""
    from gptgrep_harbor.runtime import CODEX_SOURCE_COMMIT, SANDBOX_HELPER_DATA, SANDBOX_HELPER_PATH
    root.mkdir(exist_ok=True)
    target, machine = {"linux/amd64": ("x86_64-unknown-linux-musl", 62),
                       "linux/arm64": ("aarch64-unknown-linux-musl", 183)}[platform]
    elf = bytearray(64)
    elf[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", elf, 18, machine)
    binary = root / "bwrap"
    binary.write_bytes(bytes(elf))
    binary.chmod(0o555)
    licenses = {}
    for name in ("COPYING.bubblewrap", "LICENSE.codex"):
        path = root / name
        path.write_bytes(("authored synthetic license " + name).encode())
        licenses[name] = pin(path)
    archive = "bwrap-" + target + ".tar.gz"
    record = {"schema": "gptgrep.native-sandbox-helper.v1", "release_tag": "rust-v0.162.1",
              "source_commit": CODEX_SOURCE_COMMIT,
              "binary": {**pin(binary), "path": SANDBOX_HELPER_PATH},
              "archive": {"name": archive, "bytes": 1, "sha256": "a" * 64,
                  "url": "https://github.com/openai/codex/releases/download/rust-v0.162.1/" + archive},
              "licenses": [{**value, "path": SANDBOX_HELPER_DATA + "/" + name}
                           for name, value in licenses.items()],
              "native_filesystem_qualification": "pending"}
    metadata = root / "helper-record.json"
    metadata.write_bytes(canonical(record))
    return {"binary": pin(binary), "provenance": pin(metadata), "licenses": licenses}, record


def code_mode_host_fixture(root, runtime_binary, platform="linux/amd64"):
    """Authored companion metadata and ELF header; no native host is executed."""
    from gptgrep_harbor.runtime import CODEX_SOURCE_COMMIT, CODE_MODE_HOST_DATA, CODE_MODE_HOST_PATH
    root.mkdir(exist_ok=True)
    target, machine = {"linux/amd64": ("x86_64-unknown-linux-musl", 62),
                       "linux/arm64": ("aarch64-unknown-linux-musl", 183)}[platform]
    elf = bytearray(64)
    elf[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", elf, 18, machine)
    binary = root / "codex-code-mode-host"
    binary.write_bytes(bytes(elf))
    binary.chmod(0o555)
    signature = root / "host.sigstore"
    signature.write_bytes(b"authored synthetic signature metadata; no cryptographic verification")
    archive = "codex-code-mode-host-" + target + ".tar.gz"
    record = {"schema": "gptgrep.native-code-mode-host.v1", "release_tag": "rust-v0.162.1",
              "source_commit": CODEX_SOURCE_COMMIT, "platform": platform,
              "runtime_binary": dict(runtime_binary),
              "binary": {**pin(binary), "path": CODE_MODE_HOST_PATH},
              "archive": {"name": archive, "bytes": 1, "sha256": "a" * 64,
                  "url": "https://github.com/openai/codex/releases/download/rust-v0.162.1/" + archive},
              "sigstore": {**pin(signature), "path": CODE_MODE_HOST_DATA + "/host.sigstore"},
              "attestation_verification": "not_performed", "source_and_digest_consistency_verified": True}
    metadata = root / "host-provenance.json"
    metadata.write_bytes(canonical(record))
    return {"binary": pin(binary), "provenance": pin(metadata), "sigstore": pin(signature)}, record


def bundle_fixture(root, *, change=None, omit=None, extra=None, member_change=None):
    target = "x86_64-unknown-linux-gnu"
    elf = bytearray(64)
    elf[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", elf, 18, 62)
    content = {"gptgrep": b'#!/bin/sh\nbundle_dir=$(dirname "$0")\n'
               b'export PDFIUM_LIB_PATH="$bundle_dir/lib"\nexec "$bundle_dir/gptgrep.bin" "$@"\n',
               "gptgrep.bin": bytes(elf), "lib/libpdfium.so": bytes(elf),
               "LICENSE": b"authored license fixture", "THIRD_PARTY.md": b"authored notice fixture",
               "third-party/rust/inventory.json": canonical({"schema_version": "gptgrep.licenses.v1",
                   "target": target, "complete": True, "missing_license_files": [], "packages": [{}]})}
    release = {"schema_version": "gptgrep.release.v1", "version": "1.2.3-synthetic", "target": target,
               "source_revision": None, "source_dirty": True,
               "binary": {"file": "gptgrep.bin", "sha256": sha(content["gptgrep.bin"])},
               "pdfium": {"library": "lib/libpdfium.so", "library_sha256": sha(content["lib/libpdfium.so"])},
               "rust_licenses": {"complete": True, "inventory": "third-party/rust/inventory.json", "packages": 1},
               "files": {name: sha(data) for name, data in content.items()}}
    if change:
        change(release, content)
    metadata = root / "release.json"
    metadata.write_bytes((json.dumps(release, sort_keys=True, indent=2) + "\n").encode())
    content["RELEASE.json"] = metadata.read_bytes()
    sums = {**release["files"], "RELEASE.json": sha(content["RELEASE.json"])}
    # Same line order as the packager's sorted pathlib walk (by "/"-separated parts).
    content["SHA256SUMS"] = "".join(sums[name] + "  " + name + "\n" for name in
                                    sorted(sums, key=lambda name: PurePosixPath(name).parts)).encode()
    if omit:
        del content[omit]
    top = "gptgrep-" + release["version"] + "-" + target
    archive = root / "bundle.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        directories = {top}
        for name in content:
            directories.update(top + "/" + p.as_posix() for p in PurePosixPath(name).parents
                               if p.as_posix() != ".")
        for name in sorted(directories):
            member = tarfile.TarInfo(name)
            member.type, member.mode = tarfile.DIRTYPE, 0o755
            tar.addfile(member)
        for name, data in sorted(content.items()):
            member = tarfile.TarInfo(top + "/" + name)
            member.size, member.mode = len(data), 0o755 if name in ("gptgrep", "gptgrep.bin") else 0o644
            if member_change:
                member_change(name, member)
            tar.addfile(member, io.BytesIO(data))
        if extra:
            member, data = extra(top)
            tar.addfile(member, io.BytesIO(data))
    return {"archive": pin(archive), "release_metadata": pin(metadata)}
