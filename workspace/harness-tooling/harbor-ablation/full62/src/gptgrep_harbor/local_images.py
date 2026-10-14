"""Build pinned local role images before Harbor runs their immutable config IDs."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time
import tomllib
import uuid

from .common import canonical, copy_asset, digest, integer, parse, pinned, put, read, relative, require, sha
from .process import run
from .runtime import PASS_ENV
from .trial_order import check_job_order


def config_id(value):
    require(isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value), "local_config_id")
    return value


def chain_id(layers):
    """OCI ChainID, as emitted for local images by Moby's classic layer store."""
    require(isinstance(layers, list) and 1 <= len(layers) <= 1024, "local_base_layers")
    current = config_id(layers[0])
    for layer in layers[1:]:
        current = "sha256:" + sha((current + " " + config_id(layer)).encode())
    return current


def base_provenance(value, base_id):
    require(isinstance(value, dict) and set(value) == {"build_metadata", "material_kind"},
            "base_build_provenance_fields")
    require(value["material_kind"] in ("moby-classic-chain-id-v1", "moby-verified-local-image-key-v1"),
            "local_material_kind")
    metadata = parse(pinned(value["build_metadata"]))
    require(metadata["containerimage.config.digest"] == config_id(base_id), "base_build_config_identity")
    config_id(metadata["containerimage.digest"])
    return metadata


def file_pin(path):
    path = Path(path).absolute()
    require(path.resolve() == path, "image_input_physical_path")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        before = os.fstat(source.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_size <= 2**40, "image_input_regular_file")
        value = hashlib.file_digest(source, "sha256").hexdigest()
        after = os.fstat(source.fileno())
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
            (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "image_input_changed")
    return {"path": str(path), "bytes": before.st_size, "sha256": value}


def context_members(context):
    """Metadata only within a newly generated task context; no source discovery."""
    context = Path(context).absolute()
    require(context.resolve() == context and context.is_dir(), "image_context_physical_path")
    members = []
    def walk_error(error):
        raise error
    for directory, dirs, files in os.walk(context, followlinks=False, onerror=walk_error):
        for name in dirs + files:
            require(not (Path(directory) / name).is_symlink(), "image_context_symlink")
        for name in sorted(files):
            path = Path(directory) / name
            require(path.is_file(), "image_context_regular_file")
            members.append(path.relative_to(context).as_posix())
            require(len(members) <= 8192, "finite_image_context")
    return sorted(members)


def context_rows(context):
    rows = []
    for name in context_members(context):
        pin = file_pin(Path(context) / name)
        rows.append({"relative_path": name, "bytes": pin["bytes"], "sha256": pin["sha256"]})
    return rows


def verify_context(context, rows):
    require(isinstance(rows, list) and 1 <= len(rows) <= 8192, "finite_image_context")
    require(context_members(context) == sorted(row["relative_path"] for row in rows),
            "image_context_membership")
    expected = set()
    for row in rows:
        require(set(row) == {"relative_path", "bytes", "sha256"}, "image_context_row")
        name = relative(row["relative_path"])
        require(name not in expected, "duplicate_image_context_input")
        expected.add(name)
        copy_asset({"path": str(context / name), "bytes": row["bytes"], "sha256": row["sha256"]}, 2**40)
    # Recheck names without reading any unlisted member's contents.
    require(context_members(context) == sorted(expected), "image_context_membership")


def task_contexts(task, images, platform):
    record = {"schema": "gptgrep.harbor.local-image-contexts.v1", "platform": platform,
              "images": images, "task_config": file_pin(task / "task.toml"),
              "contexts": {role: context_rows(task / role) for role in ("environment", "tests")}}
    path = task / "image-contexts.json"
    put(path, record)
    return file_pin(path)


class LocalImageBuilder:
    def __init__(self, docker, output, timeout):
        self.docker = str(Path(docker).absolute())
        require(Path(self.docker).is_file(), "docker_cli_file")
        self.output = Path(output).absolute()
        require(self.output.resolve() == self.output and not self.output.exists(), "fresh_local_image_output")
        self.output.mkdir(mode=0o700, parents=False)
        self.timeout = integer(timeout, 1, 3600)
        keys = PASS_ENV | {"HOME", "TMPDIR", "TMP", "TEMP", "XDG_CACHE_HOME", "XDG_CONFIG_HOME",
                           "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "DOCKER_HOST",
                           "DOCKER_CONFIG", "BUILDX_CONFIG", "DOCKER_BUILDKIT"}
        self.env = {key: value for key, value in os.environ.items() if key in keys}
        require(self.env.get("DOCKER_HOST", "").startswith("unix:///"), "explicit_local_docker_socket")
        for key in ("HOME", "TMPDIR", "DOCKER_CONFIG", "BUILDX_CONFIG"):
            path = Path(self.env.get(key, ""))
            require(path.is_absolute() and path.resolve() == path and path.is_dir(), "explicit_docker_client_storage")
        self.deadline = time.monotonic() + self.timeout
        self.sequence = 0

    def command(self, *args, absent_ok=False):
        self.sequence += 1
        stem = self.output / f"command-{self.sequence:02d}"
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "local_image_deadline")
        result = run([self.docker, *args], cwd=str(self.output), timeout=remaining, env=self.env)
        streams = {name: result.pop(name) for name in ("stdout", "stderr")}
        for name, data in streams.items():
            put(str(stem) + "." + name, data)
        put(str(stem) + ".json", {"argv": [self.docker, *args], "outcome": result,
                                  "streams": {name: {"bytes": len(data), "sha256": sha(data)}
                                              for name, data in streams.items()}})
        require(result["failure"] is None and (result["return_code"] == 0 or
                (absent_ok and result["return_code"] == 1 and
                 b"No such image" in streams["stderr"])), "local_image_docker_command")
        return None if result["return_code"] else streams["stdout"]

    def engine(self):
        value = parse(self.command("info", "--format", "{{json .ID}}"))
        require(isinstance(value, str) and 0 < len(value) <= 256, "docker_engine_identity")
        return value

    def inspect(self, reference, absent_ok=False):
        data = self.command("image", "inspect", reference, "--format", "{{json .}}", absent_ok=absent_ok)
        return None if data is None else parse(data)

    def build(self, context, rows, base_id, platform, provenance, expected_engine=None):
        context = Path(context).absolute()
        base_id = config_id(base_id)
        base_provenance(provenance, base_id)
        require(platform in ("linux/amd64", "linux/arm64"), "linux_image_platform")
        require(not self.output.is_relative_to(context), "build_output_outside_context")
        verify_context(context, rows)
        original = read(context / "Dockerfile")
        first, separator, rest = original.partition(b"\n")
        require(separator and first == ("FROM " + base_id).encode(), "pinned_local_image_from")
        nonce = uuid.uuid4().hex
        alias = "gptgrep-local-base-" + nonce + ":pinned"
        derived_tag = "gptgrep-local-role-" + nonce + ":built"
        try:
            engine = self.engine()
            require(expected_engine is None or engine == expected_engine, "dataset_engine_identity")
            base = self.inspect(base_id)
            require(base["Id"] == base_id and base["Os"] + "/" + base["Architecture"] == platform,
                    "local_base_identity")
            layers = base["RootFS"]["Layers"]
            require(base["RootFS"]["Type"] == "layers" and isinstance(layers, list) and layers,
                    "local_base_layers")
            require(base.get("GraphDriver", {}).get("Name") == "overlay2", "moby_classic_layer_store")
            material = chain_id(layers)
            if provenance["material_kind"] == "moby-verified-local-image-key-v1":
                material = "sha256:" + sha(("moby.buildkit.local-image.v1:" + material).encode())
            require(self.inspect(alias, absent_ok=True) is None and
                    self.inspect(derived_tag, absent_ok=True) is None, "fresh_local_build_alias")
            self.command("image", "tag", base_id, alias)
            require(self.inspect(alias)["Id"] == base_id and self.engine() == engine, "local_alias_before_build")
            dockerfile = self.output / "Dockerfile"
            put(dockerfile, ("FROM " + alias + "\n").encode() + rest)
            metadata_path, iid_path = self.output / "build-metadata.json", self.output / "image-id.txt"
            self.command("buildx", "build", "--load", "--platform", platform, "--pull=false",
                         "--network=none", "--progress", "plain", "--file", str(dockerfile),
                         "--tag", derived_tag, "--metadata-file", str(metadata_path),
                         "--iidfile", str(iid_path), str(context))
            image_id = config_id(read(iid_path, 128).decode().strip())
            metadata = parse(read(metadata_path))
            require(metadata["containerimage.config.digest"] == image_id, "derived_image_metadata")
            materials = metadata["buildx.build.provenance"]["materials"]
            material_uri = "pkg:docker/" + alias.replace(":", "@") + "?platform=" + platform.replace("/", "%2F")
            require(isinstance(materials, list) and len(materials) == 1 and isinstance(materials[0], dict) and
                    materials[0].get("uri") == material_uri and materials[0].get("digest") == {"sha256": material[7:]},
                    "buildkit_selected_base_identity")
            derived = self.inspect(image_id)
            require(derived["Id"] == image_id and derived["Os"] + "/" + derived["Architecture"] == platform and
                    derived["RootFS"]["Type"] == "layers" and
                    derived["RootFS"]["Layers"][:len(layers)] == layers, "derived_parent_layers")
            require(self.inspect(base_id)["Id"] == self.inspect(alias)["Id"] == base_id and
                    self.inspect(derived_tag)["Id"] == image_id and self.engine() == engine,
                    "local_alias_after_build")
            verify_context(context, rows)
            base_provenance(provenance, base_id)
            result = {"schema": "gptgrep.harbor.local-role-image.v1", "accepted": True,
                      "base_config_id": base_id, "image_config_id": image_id, "platform": platform,
                      "engine_id": engine, "build_alias": alias, "derived_tag": derived_tag,
                      "parent_layers": layers, "derived_layers": derived["RootFS"]["Layers"],
                      "base_build_provenance": provenance, "base_material_digest": material,
                      "context_rows_sha256": sha(canonical(rows)), "metadata": file_pin(metadata_path),
                      "aliases_retained": True, "registry_publication": False}
            put(self.output / "result.json", result)
            return result
        except Exception as error:
            put(self.output / "failure.json", {"accepted": False, "error": str(error),
                                               "base_config_id": base_id, "build_alias": alias,
                                               "derived_tag": derived_tag, "retry_count": 0})
            raise


def build_dataset(export_path, export_sha256, dataset, output, docker, timeout,
                  base_builds_path, base_builds_sha256):
    export_path, dataset, output = (Path(value).absolute() for value in (export_path, dataset, output))
    body = read(export_path)
    require(sha(body) == digest(export_sha256), "local_build_export_pin")
    export = parse(body)
    require(export["schema"] == "gptgrep.harbor.export.v1" and export["expected_pairs"] == 62 and
            export["expected_documents"] == 34 and len(export["tasks"]) == 124, "full124_local_build")
    require(dataset.resolve() == dataset and dataset.is_dir(), "local_build_dataset")
    require(output.resolve() == output and not output.exists() and not output.is_relative_to(dataset),
            "fresh_local_build_output")
    runtime = parse(pinned(export["runtime"]))
    check_job_order(export, runtime, dataset)
    images = runtime["images"]
    for role in ("gptgrep", "raw_only", "judge"):
        config_id(images[role])
    builds_body = read(Path(base_builds_path).absolute())
    require(sha(builds_body) == digest(base_builds_sha256), "base_builds_pin")
    base_builds = parse(builds_body)
    require(isinstance(base_builds, dict) and set(base_builds) == set(images.values()), "base_builds_membership")
    for image_id, provenance in base_builds.items():
        base_provenance(provenance, image_id)
    pending, names, pairs, documents = [], set(), {}, set()
    # Check the complete pinned materialization before any Docker action.
    for row in export["tasks"]:
        name = relative(row["task_name"])
        require("/" not in name and name not in names, "local_build_task_name")
        names.add(name)
        task = dataset / name
        binding = row["binding"]
        arm, pair = binding["arm"], binding["pair_id"]
        require(arm in ("gptgrep", "raw_only") and binding["runtime_sha256"] == export["runtime"]["sha256"],
                "local_build_task_binding")
        require(arm not in pairs.setdefault(pair, set()), "local_build_duplicate_arm")
        pairs[pair].add(arm)
        documents.add(binding["document_id"])
        pin = row["local_image_contexts"]
        require(pin["path"] == str(task / "image-contexts.json"), "local_build_context_location")
        context = parse(pinned(pin))
        require(context["schema"] == "gptgrep.harbor.local-image-contexts.v1" and
                context["images"] == {"environment": images[arm], "tests": images["judge"]} and
                context["platform"] == runtime["platform"], "local_build_base_join")
        require(context["task_config"]["path"] == str(task / "task.toml"), "local_build_task_config_location")
        config = pinned(context["task_config"])
        parsed = tomllib.loads(config.decode())
        require("docker_image" not in parsed["environment"] and
                "docker_image" not in parsed["verifier"]["environment"], "local_build_already_prepared")
        for role in ("environment", "tests"):
            verify_context(task / role, context["contexts"][role])
        pending.append((task, context, config))
    require(len(pairs) == 62 and all(arms == {"gptgrep", "raw_only"} for arms in pairs.values()) and
            len(documents) == 34, "complete_local_build_pairs")
    output.mkdir(mode=0o700, parents=False)
    initial = LocalImageBuilder(docker, output / "engine-before-builds", timeout)
    engine = initial.engine()
    built = []
    for task, context, original_config in pending:
        task_output = output / task.name
        task_output.mkdir(mode=0o700)
        roles = {}
        for role in ("environment", "tests"):
            roles[role] = LocalImageBuilder(docker, task_output / role, timeout).build(
                task / role, context["contexts"][role], context["images"][role], context["platform"],
                base_builds[context["images"][role]], expected_engine=engine)
            require(roles[role]["engine_id"] == engine, "dataset_engine_identity")
        text = original_config.decode()
        for section, role in (("environment", "environment"), ("verifier.environment", "tests")):
            anchor = "[" + section + "]\n"
            require(text.count(anchor) == 1, "local_build_task_section")
            text = text.replace(anchor, anchor + "docker_image = " + json.dumps(roles[role]["image_config_id"]) + "\n")
        built.append((task, context, original_config, text.encode(), roles))
    # Both role images for every pair must pass before enrolling any task.
    for task, context, original, updated, roles in built:
        require(read(task / "task.toml") == original, "local_build_task_changed")
        for role in ("environment", "tests"):
            verify_context(task / role, context["contexts"][role])
    # No task is enrolled until all completed images still exist in this engine.
    final = LocalImageBuilder(docker, output / "engine-before-enrollment", timeout)
    require(final.engine() == engine, "dataset_engine_identity")
    for task, context, original, updated, roles in built:
        for role, result in roles.items():
            image = final.inspect(result["image_config_id"])
            require(image["Id"] == result["image_config_id"] and
                    image["Os"] + "/" + image["Architecture"] == context["platform"] and
                    image["RootFS"]["Type"] == "layers" and
                    image["RootFS"]["Layers"] == result["derived_layers"], "dataset_derived_image_identity")
    require(final.engine() == engine, "dataset_engine_identity")
    require(read(Path(base_builds_path).absolute()) == builds_body, "base_builds_changed")
    for image_id, provenance in base_builds.items():
        base_provenance(provenance, image_id)
    records = []
    for task, context, original, updated, roles in built:
        require(read(task / "task.toml") == original, "local_build_task_changed")
        put(task / "task.toml.prebuilt", updated)
        os.replace(task / "task.toml.prebuilt", task / "task.toml")
        records.append({"task_name": task.name, "before_task_config": context["task_config"],
                        "after_task_config": file_pin(task / "task.toml"),
                        "images": {role: result["image_config_id"] for role, result in roles.items()}})
    result = {"schema": "gptgrep.harbor.local-images.v1", "accepted": True,
              "export": {"path": str(export_path), "bytes": len(body), "sha256": export_sha256},
              "task_count": 124, "role_image_count": 248, "tasks": records,
              "engine_id": engine, "base_builds": file_pin(Path(base_builds_path).absolute()),
              "mutable_runtime_image_selectors": False, "model_calls": 0, "full62_executed": False}
    put(output / "result.json", result)
    return result
