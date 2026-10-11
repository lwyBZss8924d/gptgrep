"""Offline image-build provenance and generic-tool checks; no task/model inputs."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import stat
import subprocess
import sys


def file_pin(path):
    path = Path(path).resolve(strict=True)
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def check_pillow():
    import PIL
    from PIL import Image, ImageOps
    if PIL.__version__ != "12.3.0":
        raise ValueError("pillow_version")
    # Pure generated pixels exercise decode/crop/resize/contrast without task data.
    source = Image.new("RGB", (8, 8), (17, 31, 47))
    image = ImageOps.autocontrast(source.crop((1, 1, 7, 7))).resize((12, 12))
    encoded = io.BytesIO()
    image.save(encoded, format="PNG")
    encoded.seek(0)
    with Image.open(encoded) as decoded:
        decoded.load()
        if decoded.size != (12, 12) or decoded.getpixel((0, 0)) != (17, 31, 47):
            raise ValueError("pillow_operations")
    return PIL.__version__


def check_helper(platform):
    directory = Path("/usr/local/share/full62-sandbox-helper")
    metadata = directory / "helper-record.json"
    if metadata.stat().st_size > 65536:
        raise ValueError("native_helper_metadata_bound")
    value = json.loads(metadata.read_bytes())
    binary = Path("/usr/local/bin/codex-resources/bwrap")
    target, machine = {"linux/amd64": ("x86_64-unknown-linux-musl", 62),
                       "linux/arm64": ("aarch64-unknown-linux-musl", 183)}[platform]
    if (value["schema"] != "gptgrep.native-sandbox-helper.v1" or
            value["release_tag"] != "rust-v0.162.1" or
            value["source_commit"] != "092d3acd6bec3e3a14bdc7e7a2810ab628ab759d" or
            value["archive"]["name"] != "bwrap-" + target + ".tar.gz" or
            not 64 <= binary.stat().st_size <= 8 * 1024 * 1024 or
            file_pin(binary) != value["binary"] or stat.S_IMODE(binary.stat().st_mode) != 0o555):
        raise ValueError("native_helper_installed_pin")
    with binary.open("rb") as source:
        header = source.read(64)
    if header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != machine:
        raise ValueError("native_helper_linux_elf")
    if (len(value["licenses"]) != 2 or {pin["path"] for pin in value["licenses"]} !=
            {str(directory / name) for name in ("COPYING.bubblewrap", "LICENSE.codex")}):
        raise ValueError("native_helper_license_paths")
    for pin in value["licenses"]:
        if not 0 < Path(pin["path"]).stat().st_size <= 65536 or file_pin(pin["path"]) != pin:
            raise ValueError("native_helper_license_pin")
    return value


def main():
    parser = argparse.ArgumentParser()
    for name in ("python-image", "uv-image", "debian-snapshot", "poppler-version", "platform"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    if sys.version_info[:2] != (3, 13):
        raise ValueError("python_3_13_required")
    binaries = {}
    for name in ("python3", "sh", "uv", "codex", "pdftotext", "pdftoppm"):
        path = shutil.which(name)
        if path is None:
            raise ValueError("missing_common_tool")
        binaries[name] = file_pin(path)
    native = subprocess.check_output(["codex", "--version"], text=True).strip()
    if native != "codex-cli 0.162.1":
        raise ValueError("native_version")
    package = subprocess.check_output(["dpkg-query", "-W", "-f=${Version}", "poppler-utils"], text=True)
    if package != args.poppler_version:
        raise ValueError("poppler_package_version")
    versions = {name: subprocess.run([name, "-v"], check=True, capture_output=True, text=True).stderr.strip()
                for name in ("pdftotext", "pdftoppm")}
    result = {"schema": "gptgrep.harbor.base-tools.v1", "build_inputs": vars(args),
              "python_version": sys.version.split()[0], "pillow_version": check_pillow(),
              "poppler_version": package, "native_cli_version": "0.162.1",
              "native_source_commit": "092d3acd6bec3e3a14bdc7e7a2810ab628ab759d",
              "binaries": binaries, "pdf_tool_versions": versions,
              "native_sandbox_helpers": {"bwrap": check_helper(args.platform)},
              "native_helper_digest_compatibility_verified": False,
              "requirements": file_pin(Path(__file__).with_name("requirements.lock")),
              "capabilities": ["python3", "POSIX shell", "Pillow==12.3.0", "pdftotext", "pdftoppm"],
              "pillow_pixel_check": "passed", "pdf_document_check": "not_run",
              "model_tool_permissions_verified": False}
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
