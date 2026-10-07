"""Offline Harbor 0.24.0 typed format bridge. No agent/runtime/provider imports."""
import argparse
import hashlib
import json
import os
from importlib.metadata import version
from pathlib import Path, PurePosixPath

from harbor.models.dataset.manifest import DatasetManifest
from harbor.models.task.config import TaskConfig
from harbor.models.task.task import Task
from harbor.models.task.verifier_mode import resolve_verifier_environment_definition
from harbor.publisher.packager import Packager

MAX_FILE = 64 * 1024 * 1024
MAX_TOTAL = 128 * 1024 * 1024
HERE = Path(__file__).resolve().parent


def digest(data):
    return hashlib.sha256(data).hexdigest()


def regular(path, limit=MAX_FILE):
    """Open every component via dirfd/O_NOFOLLOW; refuse nonregular input."""
    path = Path(os.path.abspath(path))
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for i, component in enumerate(path.parts[1:]):
            flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            if i != len(path.parts) - 2:
                flags |= os.O_DIRECTORY
            child = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        import stat

        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
            raise ValueError("input_type_or_size_limit")
        with os.fdopen(os.dup(descriptor), "rb") as handle:
            data = handle.read(limit + 1)
        if len(data) > limit:
            raise ValueError("input_size_limit")
        return data
    finally:
        os.close(descriptor)


def relative(value):
    p = PurePosixPath(value)
    if not value or "\\" in value or "\0" in value or p.is_absolute() or any(
        x in (".", "..", "") for x in value.split("/")
    ):
        raise ValueError("unsafe_relative_path")
    return value


def read_export(root, role):
    root = Path(root)
    manifest_bytes = regular(root / "export-manifest.json", 1024 * 1024)
    manifest_sha256 = digest(manifest_bytes)
    manifest = json.loads(manifest_bytes)
    expected_keys = {"schema_version", "role", "corpus_sha256", "reader_projection_sha256",
                     "reader_export_sha256", "options_sha256", "synthetic", "executable", "task_bindings", "files"}
    if set(manifest) != expected_keys or manifest["schema_version"] != "1" or manifest["role"] != role \
            or manifest["executable"] or not manifest["synthetic"]:
        raise ValueError("export_role_or_schema")
    expected_reader = manifest["reader_export_sha256"]
    if role == "grader":
        if not isinstance(expected_reader, str) or len(expected_reader) != 64 or any(c not in "0123456789abcdef" for c in expected_reader):
            raise ValueError("grader_reader_export_sha256")
    elif expected_reader is not None:
        raise ValueError("reader_export_role_binding")
    actual = {}
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError("export_symlink")
        if path.is_dir():
            continue
        name = relative(path.relative_to(root).as_posix())
        if name == "export-manifest.json":
            continue
        actual[name] = regular(path)
        if len(actual) > 4096 or sum(map(len, actual.values())) > MAX_TOTAL:
            raise ValueError("export_resource_limit")
    if {name: digest(data) for name, data in actual.items()} != manifest["files"]:
        raise ValueError("export_inventory_mismatch")
    return manifest, actual, manifest_sha256


def directory_fd(path):
    path = Path(os.path.abspath(path))
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for component in path.parts[1:]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def new_private_directory(parent, name, allow_existing=False):
    created = False
    try:
        os.mkdir(name, mode=0o700, dir_fd=parent)
        created = True
    except FileExistsError:
        if not allow_existing:
            raise
    if created:
        # Only the newly created component is changed, never caller parents.
        # chmodat handles masks that removed owner directory permissions.
        os.chmod(name, 0o700, dir_fd=parent, follow_symlinks=False)
    descriptor = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
    return descriptor


def private_write(root, name, data):
    parts = relative(name).split("/")
    directory = directory_fd(root)
    try:
        for component in parts[:-1]:
            child = new_private_directory(directory, component, allow_existing=True)
            os.close(directory)
            directory = child
        descriptor = os.open(parts[-1], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                             0o600, dir_fd=directory)
        try:
            # This acts on the new owned fd and does not alter global umask.
            os.fchmod(descriptor, 0o600)
            with os.fdopen(os.dup(descriptor), "wb") as handle:
                handle.write(data)
        finally:
            os.close(descriptor)
    finally:
        os.close(directory)


def private_new(root, files):
    root = Path(os.path.abspath(root))
    if root.parent.resolve() != root.parent:
        raise ValueError("output_parent_not_canonical")
    if sum(map(len, files.values())) > MAX_TOTAL:
        raise ValueError("output_resource_limit")
    parent = directory_fd(root.parent)
    try:
        directory = new_private_directory(parent, root.name)
        os.close(directory)
    finally:
        os.close(parent)
    for name, data in files.items():
        private_write(root, name, data)


def config(descriptor):
    if set(descriptor) != {"task_id", "arm", "options"} or descriptor["arm"] not in ("control", "gptgrep"):
        raise ValueError("descriptor_schema")
    opt = descriptor["options"]
    common = {"cpus": opt["cpus"], "memory_mb": opt["memory_mb"],
              "network_mode": opt["network_mode"], "os": "linux", "workdir": "/app"}
    cfg = TaskConfig.model_validate({
        "schema_version": "1.4",
        "task": {"name": f"gptgrep/{descriptor['arm']}-{descriptor['task_id']}", "version": "0.0.0"},
        "metadata": {"synthetic": True, "execution_qualified": False,
                     "estimand": "whole_prebuilt_search_harness", "preprocessing_in_reward": False},
        "environment": common,
        "agent": {"timeout_sec": opt["timeout_sec"], "network_mode": opt["network_mode"]},
        "verifier": {"timeout_sec": 60, "environment_mode": "separate",
                     "environment": dict(common), "network_mode": "no-network"},
        "artifacts": [{"source": "/app/answer.json", "destination": "answer.json"}],
    })
    # The verifier Dockerfile is intentionally different and lives only in tests/.
    if cfg.verifier.environment is None or cfg.verifier.environment_mode.value != "separate":
        raise ValueError("separate_verifier_required")
    return cfg


def compose(readers, graders, output):
    if version("harbor") != "0.24.0":
        raise ValueError("harbor_version_mismatch")
    reader, reader_files, reader_manifest_sha256 = read_export(readers, "reader")
    grader, grader_files, grader_manifest_sha256 = read_export(graders, "grader")
    for key in ("corpus_sha256", "reader_projection_sha256", "options_sha256", "task_bindings"):
        if reader[key] != grader[key]:
            raise ValueError("role_binding_mismatch")
    if grader["reader_export_sha256"] != reader_manifest_sha256:
        raise ValueError("reader_export_digest_binding")
    ids = [binding["id"] for binding in reader["task_bindings"]]
    expected_grader = {f"{task_id}/reference.json" for task_id in ids}
    if set(grader_files) != expected_grader or any("/tests/" in name for name in reader_files):
        raise ValueError("role_file_namespace")
    options = json.loads(reader_files["frozen-options.json"])
    files = {}
    for arm in ("control", "gptgrep"):
        for task_id in ids:
            prefix = f"{arm}/{task_id}"
            matching = {name.removeprefix(prefix + "/"): data for name, data in reader_files.items()
                        if name.startswith(prefix + "/")}
            descriptor = json.loads(matching.pop("task-descriptor.json"))
            if descriptor != {"task_id": task_id, "arm": arm, "options": options}:
                raise ValueError("descriptor_binding")
            allowed = {"instruction.md"} | {name for name in matching if name.startswith("environment/")}
            if set(matching) != allowed:
                raise ValueError("reader_namespace")
            if arm == "control" and any(name.startswith(("environment/prebuilt/", "environment/tool/")) for name in matching):
                raise ValueError("baseline_preparsed_artifact")
            for name, data in matching.items():
                files[f"{prefix}/{name}"] = data
            cfg = config(descriptor)
            files[f"{prefix}/task.toml"] = cfg.model_dump_toml().encode()
            files[f"{prefix}/tests/reference.json"] = grader_files[f"{task_id}/reference.json"]
            files[f"{prefix}/tests/verify_answer.py"] = regular(HERE / "verify_answer.py")
            files[f"{prefix}/tests/test.sh"] = b"#!/bin/sh\nset -eu\nexec python3 /tests/verify_answer.py --answer /app/answer.json --reference /tests/reference.json --output /logs/verifier/status.json\n"
            files[f"{prefix}/tests/Dockerfile"] = (
                f"FROM {options['verifier_image']}\nWORKDIR /app\nCOPY . /tests/\n".encode()
            )
    files["paired-schedule.json"] = reader_files["paired-schedule.json"]
    files["format-status.json"] = json.dumps({"harbor_version": "0.24.0", "schema_version": "1.4",
        "execution_qualified": False, "judge_status": "unavailable", "reward": None,
        "holds": ["linux_runtime", "sandbox_role_isolation", "original_judge_qualification", "tool_delivery", "trial_phase"]}, sort_keys=True).encode()
    private_new(output, files)
    root = Path(output)
    task_refs = {arm: [] for arm in ("control", "gptgrep")}
    for arm in task_refs:
        for task_id in ids:
            task = Task(root / arm / task_id)
            definition = resolve_verifier_environment_definition(task.config, task.paths)
            if definition is None or definition.directory != task.paths.tests_dir:
                raise ValueError("verifier_context_fallback")
            content_hash, _ = Packager.compute_content_hash(task.task_dir)
            task_refs[arm].append({"name": task.name, "digest": f"sha256:{content_hash}"})
        dataset = DatasetManifest.model_validate({"schema_version": "1.0",
            "dataset": {"name": f"gptgrep/synthetic-{arm}", "version": "0.0.0"},
            "tasks": task_refs[arm], "files": []})
        # Harbor 0.24's serializer omits its default schema root field.
        private_write(root, f"{arm}/dataset.toml", ('schema_version = "1.0"\n' + dataset.to_toml()).encode())
    inventory = {path.relative_to(root).as_posix(): digest(regular(path)) for path in root.rglob("*") if path.is_file()}
    receipt = {"schema_version": "1", "harbor_version": "0.24.0", "task_schema_version": "1.4",
        "dataset_schema_version": "1.0", "reader_manifest_sha256": reader_manifest_sha256,
        "grader_manifest_sha256": grader_manifest_sha256,
        "task_refs": task_refs, "files": inventory, "execution_qualified": False}
    private_write(root, "package-receipt.json", json.dumps(receipt, indent=2, sort_keys=True).encode())
    return validate(root)


def validate(root):
    if version("harbor") != "0.24.0":
        raise ValueError("harbor_version_mismatch")
    root = Path(root)
    receipt = json.loads(regular(root / "package-receipt.json", 1024 * 1024))
    actual = {}
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError("package_symlink")
        if path.is_file() and path != root / "package-receipt.json":
            actual[path.relative_to(root).as_posix()] = digest(regular(path))
    if actual != receipt["files"] or receipt["execution_qualified"]:
        raise ValueError("package_inventory_mismatch")
    if set(receipt["task_refs"]) != {"control", "gptgrep"}:
        raise ValueError("arm_namespace")
    for arm, task_refs in receipt["task_refs"].items():
        dataset = DatasetManifest.from_toml_file(root / arm / "dataset.toml")
        if dataset.schema_version != "1.0":
            raise ValueError("dataset_schema_version")
        if [ref.model_dump() for ref in dataset.tasks] != task_refs:
            raise ValueError("dataset_refs_mismatch")
        for ref in task_refs:
            prefix = f"{arm}-"
            task_id = ref["name"].split("/", 1)[1].removeprefix(prefix)
            task = Task(root / arm / task_id)
            if task.name != ref["name"] or task.config.schema_version != "1.4" or task.config.artifacts[0].source != "/app/answer.json" \
                    or task.config.artifacts[0].destination != "answer.json":
                raise ValueError("task_schema_or_answer_artifact")
            definition = resolve_verifier_environment_definition(task.config, task.paths)
            if definition is None or definition.directory != task.paths.tests_dir:
                raise ValueError("verifier_context_fallback")
            if "sha256:" + Packager.compute_content_hash(task.task_dir)[0] != ref["digest"]:
                raise ValueError("native_task_digest_mismatch")
    return {"valid": True, "execution_qualified": False, "task_count": sum(map(len, receipt["task_refs"].values()))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    command = parser.add_subparsers(dest="command", required=True)
    writer = command.add_parser("compose")
    writer.add_argument("--readers", required=True)
    writer.add_argument("--graders", required=True)
    writer.add_argument("--output-dir", required=True)
    checker = command.add_parser("validate")
    checker.add_argument("--export-dir", required=True)
    args = parser.parse_args()
    result = compose(args.readers, args.graders, args.output_dir) if args.command == "compose" else validate(args.export_dir)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
