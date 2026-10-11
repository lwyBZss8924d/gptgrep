"""Stage finite source/binary inputs and print exact Docker build commands.

This command does not download, execute Docker, run supplied binaries, or read
credentials. Root selects pinned Linux inputs and runs the reviewed build.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shlex
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from gptgrep_harbor.common import copy_asset, digest, integer, parse, pinned, put, read, require
from gptgrep_harbor.adapter import image
from gptgrep_harbor.runtime import SANDBOX_HELPER_MAX_BYTES, sandbox_helper_identity, validate_asset_path
from gptgrep_harbor.product_bundle import (PRODUCT_ENTRYPOINT, PRODUCT_PROVENANCE,
                                          PRODUCT_ROOT, release_bundle)


def prepare(config, output):
    require(set(config) == {"platform", "python_image", "uv_image", "debian_snapshot",
                           "poppler_version", "codex_binary", "native_sandbox_helper", "gptgrep_binary"}, "base_build_fields")
    require(config["gptgrep_binary"] is None, "raw_product_binary_unsupported_use_release_bundle")
    require(config["platform"] in ("linux/amd64", "linux/arm64"), "linux_platform")
    for key in ("python_image", "uv_image"):
        require(re.fullmatch(r"[^@]+@sha256:[0-9a-f]{64}", image(config[key])),
                "base_from_repository_digest_required")
    require(isinstance(config["debian_snapshot"], str) and
            re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", config["debian_snapshot"]), "debian_snapshot_required")
    require(isinstance(config["poppler_version"], str) and
            re.fullmatch(r"[0-9][A-Za-z0-9.+:~_-]{0,127}", config["poppler_version"]), "poppler_version_required")
    helper_input = config["native_sandbox_helper"]
    require(isinstance(helper_input, dict) and set(helper_input) == {"binary", "provenance", "licenses"},
            "native_helper_input")
    require(isinstance(helper_input["provenance"], dict) and
            set(helper_input["provenance"]) == {"path", "bytes", "sha256"}, "native_helper_provenance_pin")
    validate_asset_path(helper_input["provenance"]["path"])
    helper = sandbox_helper_identity(parse(pinned(helper_input["provenance"], 65536)), config["platform"])
    require(isinstance(helper_input["licenses"], dict) and
            set(helper_input["licenses"]) == {"COPYING.bubblewrap", "LICENSE.codex"}, "native_helper_license_inputs")
    require(isinstance(helper_input["binary"], dict) and
            all(helper_input["binary"].get(key) == helper["binary"][key] for key in ("bytes", "sha256")),
            "native_helper_binary_join")
    for expected in helper["licenses"]:
        pin = helper_input["licenses"][Path(expected["path"]).name]
        require(isinstance(pin, dict) and set(pin) == {"path", "bytes", "sha256"} and
                all(pin.get(key) == expected[key] for key in ("bytes", "sha256")),
                "native_helper_license_join")
        validate_asset_path(pin["path"])
        copy_asset(pin, 65536)
    binaries = [("codex", config["codex_binary"]), ("bwrap", helper_input["binary"])]
    # All inputs are checked before creating output. No recursive source copy.
    for name, pin in binaries:
        require(isinstance(pin, dict) and set(pin) == {"path", "bytes", "sha256"}, "binary_pin")
        validate_asset_path(pin["path"])
        cap = SANDBOX_HELPER_MAX_BYTES if name == "bwrap" else 2**30
        integer(pin["bytes"], 1, cap)
        digest(pin["sha256"])
        copy_asset(pin, cap)
    header = read(helper_input["binary"]["path"], SANDBOX_HELPER_MAX_BYTES)[:64]
    machine = {"linux/amd64": 62, "linux/arm64": 183}[config["platform"]]
    require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01" and
            int.from_bytes(header[18:20], "little") == machine, "native_helper_linux_elf")
    root = Path(output).absolute()
    require(root.resolve() == root and not root.exists(), "fresh_physical_build_context")
    root.mkdir(mode=0o700, parents=False)
    source = Path(__file__).resolve().parent
    for name in ("Dockerfile", ".dockerignore", "requirements.lock", "probe.py"):
        put(root / name, read(source / name))
    for name, pin in binaries:
        copy_asset(pin, SANDBOX_HELPER_MAX_BYTES if name == "bwrap" else 2**30, root / name)
    copy_asset(helper_input["provenance"], 65536, root / "helper-record.json")
    for name, pin in helper_input["licenses"].items():
        copy_asset(pin, 65536, root / name)
    build = ["docker", "buildx", "build", "--load", "--platform", config["platform"]]
    for key in ("python_image", "uv_image", "debian_snapshot", "poppler_version"):
        build += ["--build-arg", key.upper() + "=" + config[key]]
    build += ["--build-arg", "CODEX_SHA256=" + config["codex_binary"]["sha256"]]
    build += ["--build-arg", "BWRAP_SHA256=" + helper_input["binary"]["sha256"]]
    commands = {"common": build + ["--target", "common", "--tag", "gptgrep-full62-common:local", str(root)]}
    return {"build_commands": {role: shlex.join(argv) for role, argv in commands.items()},
            "common_image_for": ["raw_only", "judge"], "native_sandbox_helper": helper,
            "native_helper_digest_compatibility_verified": False, "actual_build_performed": False}


def prepare_treatment(config, output):
    """Stage a release-only derivative of an existing immutable common image."""
    from gptgrep_harbor.local_images import base_provenance, config_id, context_rows, file_pin
    require(set(config) == {"platform", "common_image", "common_build_provenance", "product_bundle"},
            "treatment_build_fields")
    common = config_id(config["common_image"])
    base_provenance(config["common_build_provenance"], common)
    for pin in config["product_bundle"].values():
        validate_asset_path(pin["path"])
    # Validate the entire archive before creating any context directory.
    release_bundle(config["product_bundle"], config["platform"])
    root = Path(output).absolute()
    require(root.resolve() == root and not root.exists(), "fresh_physical_build_context")
    root.mkdir(mode=0o700, parents=False)
    provenance = release_bundle(config["product_bundle"], config["platform"], root / "bundle")
    provenance["common_image"] = common
    put(root / "product-provenance.json", provenance)
    product_pin = file_pin(root / "product-provenance.json")
    check = {"platform": config["platform"], "gptgrep_binary": PRODUCT_ENTRYPOINT,
             "gptgrep_binary_pin": provenance["entrypoint"],
             "gptgrep_bundle": {"provenance": {**product_pin, "path": PRODUCT_PROVENANCE}},
             "base_tool_environment": {"image": common}}
    put(root / "product-check.json", check)
    source = Path(__file__).resolve().parents[1] / "src/gptgrep_harbor"
    for name in ("__init__.py", "common.py", "product_bundle.py"):
        put(root / "verifier/gptgrep_harbor" / name, read(source / name))
    docker = ("FROM " + common + "\n"
              "COPY bundle " + PRODUCT_ROOT + "\n"
              "COPY product-provenance.json " + PRODUCT_PROVENANCE + "\n"
              "COPY product-check.json /usr/local/share/full62-product/\n"
              "COPY verifier /usr/local/share/full62-product/verifier\n"
              "RUN PYTHONPATH=/usr/local/share/full62-product/verifier python3 -c "
              "'import json; from gptgrep_harbor.product_bundle import check_product_bundle; "
              "check_product_bundle(json.load(open(\"/usr/local/share/full62-product/product-check.json\")))' "
              "&& " + PRODUCT_ENTRYPOINT + " --version\n")
    put(root / "Dockerfile", docker.encode())
    put(root / ".dockerignore", b"**\n!Dockerfile\n!bundle\n!bundle/**\n!product-provenance.json\n"
                                  b"!product-check.json\n!verifier\n!verifier/**\n")
    return {"schema": "gptgrep.harbor.treatment-context.v1", "context": str(root),
            "platform": config["platform"], "common_image": common,
            "common_build_provenance": config["common_build_provenance"],
            "product_bundle": config["product_bundle"], "context_rows": context_rows(root),
            "runtime_product": {"gptgrep_binary": PRODUCT_ENTRYPOINT,
                                "gptgrep_binary_pin": provenance["entrypoint"],
                                "gptgrep_bundle": {"provenance": product_pin}},
            "build_frontdoor": "gptgrep_harbor.local_images.LocalImageBuilder.build",
            "actual_build_performed": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", required=True)
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--treatment", action="store_true", help="derive only from an existing common image")
    args = parser.parse_args()
    stage = prepare_treatment if args.treatment else prepare
    print(json.dumps(stage(parse(read(Path(args.inputs).absolute())), args.output_dir), indent=2))


if __name__ == "__main__":
    main()
