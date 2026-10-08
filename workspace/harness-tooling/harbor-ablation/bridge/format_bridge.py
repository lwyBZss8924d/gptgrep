"""Offline Harbor 0.24.0 typed format bridge. No agent/runtime/provider imports."""
import argparse
import hashlib
import json
import math
import ipaddress
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


def regular_descriptor(path, limit=MAX_FILE):
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
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def regular(path, limit=MAX_FILE):
    descriptor = regular_descriptor(path, limit)
    try:
        with os.fdopen(os.dup(descriptor), "rb") as handle:
            data = handle.read(limit + 1)
        if len(data) > limit:
            raise ValueError("input_size_limit")
        return data
    finally:
        os.close(descriptor)


class HeldPayload:
    """An admitted regular file, copied in fixed chunks without buffering its payload."""
    def __init__(self, path):
        self.path = Path(os.path.abspath(path))
        self.descriptor = regular_descriptor(self.path)
        metadata = os.fstat(self.descriptor)
        self.identity = (metadata.st_dev, metadata.st_ino, metadata.st_size)
        self.sha256 = self.measure(self.descriptor)

    def __del__(self):
        if hasattr(self, "descriptor"):
            os.close(self.descriptor)
            del self.descriptor

    def __len__(self):
        return self.identity[2]

    def measure(self, descriptor, target=None):
        os.lseek(descriptor, 0, os.SEEK_SET)
        sha = hashlib.sha256()
        count = 0
        while chunk := os.read(descriptor, 65536):
            count += len(chunk)
            if count > len(self):
                raise ValueError("payload_growth")
            sha.update(chunk)
            if target is not None:
                target.write(chunk)
        if count != len(self):
            raise ValueError("payload_shortened")
        return sha.hexdigest()

    def fresh(self):
        descriptor = regular_descriptor(self.path)
        try:
            metadata = os.fstat(descriptor)
            if (metadata.st_dev, metadata.st_ino, metadata.st_size) != self.identity \
                    or self.measure(self.descriptor) != self.sha256 or self.measure(descriptor) != self.sha256:
                raise ValueError("payload_source_changed")
        finally:
            os.close(descriptor)

    def small_bytes(self):
        if len(self) > 1024 * 1024:
            raise ValueError("structured_input_limit")
        self.fresh()
        os.lseek(self.descriptor, 0, os.SEEK_SET)
        with os.fdopen(os.dup(self.descriptor), "rb") as handle:
            data = handle.read(1024 * 1024 + 1)
        if len(data) != len(self) or digest(data) != self.sha256:
            raise ValueError("payload_snapshot_changed")
        return data


def small_bytes(payload):
    return payload.small_bytes() if isinstance(payload, HeldPayload) else payload


def payload_digest(payload):
    return payload.sha256 if isinstance(payload, HeldPayload) else digest(payload)


def production_json(data):
    """Parse one bounded admitted buffer with unambiguous finite JSON semantics."""
    if len(data) > 1024 * 1024:
        raise ValueError("structured_input_limit")

    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("production_json_duplicate_key")
            result[key] = value
        return result

    def constant(_value):
        raise ValueError("production_json_nonfinite")

    def finite_float(value):
        number = float(value)
        if not math.isfinite(number):
            raise ValueError("production_json_nonfinite")
        return number

    return json.loads(data, object_pairs_hook=object_pairs, parse_constant=constant, parse_float=finite_float)


class ManifestProbe(dict):
    """Preserve root schema declarations for dispatch without changing legacy parsing."""
    def __init__(self, pairs):
        super().__init__(pairs)
        self.production_declared = any(key == "schema_version" and value == "gptgrep.harbor.export.v2"
                                       for key, value in pairs)


def production_qualification():
    return {"artifact_scope": "native_v2_source_structure_only", "historical_builder": "unattested",
            "runtime": "HOLD", "complete_multimodal_helper_treatment": "HOLD",
            "physical_isolation": "HOLD", "judge": "unavailable"}


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
    manifest = json.loads(manifest_bytes, object_pairs_hook=ManifestProbe)
    expected_keys = {"schema_version", "role", "corpus_sha256", "reader_projection_sha256",
                     "reader_export_sha256", "options_sha256", "synthetic", "executable", "task_bindings", "files"}
    production = isinstance(manifest, ManifestProbe) and manifest.production_declared
    if production:
        manifest = production_json(manifest_bytes)
        expected_keys = expected_keys - {"synthetic"} | {"source_mode", "qualification", "group_bindings"}
    if set(manifest) != expected_keys or manifest["role"] != role or manifest["executable"] is not False \
            or (not production and (manifest["schema_version"] != "1" or manifest["synthetic"] is not True)) \
            or (production and (manifest["schema_version"] != "gptgrep.harbor.export.v2"
                                or manifest["source_mode"] != "raw_only_native_v2_structure"
                                or manifest["qualification"] != production_qualification())):
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
        actual[name] = HeldPayload(path) if production else regular(path)
        if len(actual) > 4096 or sum(map(len, actual.values())) > MAX_TOTAL:
            raise ValueError("export_resource_limit")
    if {name: payload_digest(data) for name, data in actual.items()} != manifest["files"]:
        raise ValueError("export_inventory_mismatch")
    if production and role == "reader":
        production_reader_namespace(manifest, actual)
    return manifest, actual, manifest_sha256


def production_reader_namespace(manifest, files):
    option_bytes = small_bytes(files["frozen-options.json"])
    production_json(option_bytes)
    if digest(option_bytes) != manifest["options_sha256"]:
        raise ValueError("options_digest_binding")
    groups = {group["id"]: group for group in manifest["group_bindings"]}
    if len(groups) != len(manifest["group_bindings"]) or not 1 <= len(groups) <= 16:
        raise ValueError("production_group_count")
    expected = {"frozen-options.json", "paired-schedule.json"}
    ids = set()
    for task in manifest["task_bindings"]:
        task_id = relative(task["id"])
        if "/" in task_id or task_id in ids or not task["group_scope"] or len(set(task["group_scope"])) != len(task["group_scope"]):
            raise ValueError("production_task_scope")
        ids.add(task_id)
        raw = {}
        for arm in ("control", "gptgrep"):
            prefix = f"{arm}/{task_id}"
            if small_bytes(files[f"{prefix}/environment/codex-config.json"]) != option_bytes:
                raise ValueError("production_config_difference")
            reader_input = production_json(small_bytes(files[f"{prefix}/environment/reader-input.json"]))
            expected_input_keys = {"schema_version", "task_id", "question", "question_sha256", "group_scope", "arm"}
            if not isinstance(reader_input, dict) or set(reader_input) != expected_input_keys \
                    or reader_input["schema_version"] != "gptgrep.harbor.production-reader-input.v1" \
                    or reader_input["task_id"] != task_id or reader_input["arm"] != arm \
                    or reader_input["group_scope"] != task["group_scope"] \
                    or reader_input["question_sha256"] != task["question_sha256"] \
                    or not isinstance(reader_input["question"], str) or not reader_input["question"] \
                    or digest(reader_input["question"].encode("utf-8")) != task["question_sha256"]:
                raise ValueError("production_reader_input_binding")
            expected_instruction = (
                reader_input["question"] + '\n\nRead the raw PDF groups named in /app/reader-input.json. '
                'Write only {"answer":"..."} to /app/answer.json.\n').encode("utf-8")
            if small_bytes(files[f"{prefix}/instruction.md"]) != expected_instruction:
                raise ValueError("production_instruction_binding")
            expected.update(f"{prefix}/{name}" for name in (
                "instruction.md", "task-descriptor.json", "environment/Dockerfile",
                "environment/reader-input.json", "environment/codex-config.json"))
            for group_id in task["group_scope"]:
                group = groups.get(group_id)
                if group is None or group["runtime_root"] != group["canonical_root"]:
                    raise ValueError("production_group_or_relocation")
                base = f"{prefix}/environment/groups/{group_id}"
                for document in group["raw_documents"]:
                    name = f"{base}/raw/{relative(document['path'])}"
                    expected.add(name)
                    raw[f"{group_id}/{document['path']}"] = document["sha256"]
                    if manifest["files"].get(name) != document["sha256"]:
                        raise ValueError("production_raw_digest")
                if arm == "gptgrep":
                    name = f"{base}/raw/.gptgrep/CURRENT.json"
                    expected.add(name)
                    if manifest["files"].get(name) != group["current_snapshot_sha256"]:
                        raise ValueError("production_current_digest")
                    for artifact in group["generation_inventory"]:
                        name = f"{base}/raw/.gptgrep/generations/{relative(group['pin']['generation'])}/{relative(artifact['path'])}"
                        expected.add(name)
                        if manifest["files"].get(name) != artifact["sha256"]:
                            raise ValueError("production_generation_digest")
                    expected.update(f"{base}/seal/{name}" for name in (
                        "receipt.json", "pin.json", "raw-manifest.json", "build-controls.json"))
            if arm == "gptgrep":
                expected.update(f"{prefix}/environment/tool/{name}" for name in ("gptgrep", "provenance.json"))
        if raw != task["raw_sha256"]:
            raise ValueError("production_raw_scope")
    if set(files) != expected:
        raise ValueError("production_exact_reader_namespace")


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
                if isinstance(data, HeldPayload):
                    data.fresh()
                    if data.measure(data.descriptor, handle) != data.sha256:
                        raise ValueError("payload_copy_changed")
                    data.fresh()
                else:
                    handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
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
    for data in files.values():
        if isinstance(data, HeldPayload):
            data.fresh()
    parent = directory_fd(root.parent)
    try:
        directory = new_private_directory(parent, root.name)
        os.close(directory)
    finally:
        os.close(parent)
    for name, data in files.items():
        private_write(root, name, data)
    for data in files.values():
        if isinstance(data, HeldPayload):
            data.fresh()


def reader_network_policy(opt, production):
    mode = opt["network_mode"]
    hosts = opt.get("allowed_hosts", [])
    if not isinstance(hosts, list) or len(hosts) > 32 or mode not in ("no-network", "allowlist") \
            or (mode == "no-network" and hosts) or (mode == "allowlist" and (not production or not hosts)):
        raise ValueError("reader_network_policy")
    seen = set()
    for host in hosts:
        if not isinstance(host, str) or not host or len(host) > 253 or not host.isascii() or "%" in host or host in seen:
            raise ValueError("reader_network_host")
        try:
            address = ipaddress.ip_address(host)
        except ValueError:
            labels = host.split(".")
            # Refuse legacy IPv4 interpretation instead of falling back to DNS.
            final_label = labels[-1]
            numeric = final_label.isdecimal() or (final_label.startswith("0x")
                       and all(c in "0123456789abcdefABCDEF" for c in final_label[2:]))
            if numeric or not all(label and len(label) <= 63 and label[0].isalnum() and label[-1].isalnum()
                       and all(c in "abcdefghijklmnopqrstuvwxyz0123456789-" for c in label) for label in labels):
                raise ValueError("reader_network_host") from None
        else:
            canonical = str(address)
            if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped:
                canonical = "::ffff:" + str(address.ipv4_mapped)
            if host != canonical:
                raise ValueError("reader_network_host")
        seen.add(host)
    policy = {"network_mode": mode}
    if hosts:
        policy["allowed_hosts"] = list(hosts)
    return policy


def config(descriptor, production=False):
    if set(descriptor) != {"task_id", "arm", "options"} or descriptor["arm"] not in ("control", "gptgrep"):
        raise ValueError("descriptor_schema")
    opt = descriptor["options"]
    policy = reader_network_policy(opt, production)
    common = {"cpus": opt["cpus"], "memory_mb": opt["memory_mb"],
              **policy, "os": "linux", "workdir": "/app"}
    verifier_environment = {"cpus": opt["cpus"], "memory_mb": opt["memory_mb"],
                            "network_mode": "no-network", "os": "linux", "workdir": "/app"}
    cfg = TaskConfig.model_validate({
        "schema_version": "1.4",
        "task": {"name": f"gptgrep/{descriptor['arm']}-{descriptor['task_id']}", "version": "0.0.0"},
        "metadata": {"synthetic": not production, "execution_qualified": False,
                     "estimand": "whole_prebuilt_search_harness", "preprocessing_in_reward": False},
        "environment": common,
        "agent": {"timeout_sec": opt["timeout_sec"], **policy},
        "verifier": {"timeout_sec": 60, "environment_mode": "separate",
                     "environment": verifier_environment, "network_mode": "no-network"},
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
    production = reader["schema_version"] == "gptgrep.harbor.export.v2"
    if reader["schema_version"] != grader["schema_version"]:
        raise ValueError("role_schema_mismatch")
    for key in ("corpus_sha256", "reader_projection_sha256", "options_sha256", "task_bindings"):
        if reader[key] != grader[key]:
            raise ValueError("role_binding_mismatch")
    if production and any(reader[key] != grader[key] for key in ("source_mode", "qualification", "group_bindings")):
        raise ValueError("production_role_binding_mismatch")
    if grader["reader_export_sha256"] != reader_manifest_sha256:
        raise ValueError("reader_export_digest_binding")
    output_path = Path(os.path.abspath(output))
    for source in (Path(os.path.abspath(readers)), Path(os.path.abspath(graders))):
        if output_path == source or output_path in source.parents or source in output_path.parents:
            raise ValueError("output_input_overlap")
    ids = [binding["id"] for binding in reader["task_bindings"]]
    expected_grader = {f"{task_id}/reference.json" for task_id in ids}
    if set(grader_files) != expected_grader or any("/tests/" in name for name in reader_files):
        raise ValueError("role_file_namespace")
    if production:
        for binding in reader["task_bindings"]:
            reference = production_json(small_bytes(grader_files[f"{binding['id']}/reference.json"]))
            if not isinstance(reference, dict) or set(reference) != {"question", "answer", "answer_format"} or reference["answer"] is None \
                    or not isinstance(reference["question"], str) or not reference["question"] \
                    or digest(reference["question"].encode()) != binding["question_sha256"] \
                    or not isinstance(reference["answer_format"], str) or not reference["answer_format"]:
                raise ValueError("production_reference_contract")
    options_bytes = small_bytes(reader_files["frozen-options.json"])
    options = production_json(options_bytes) if production else json.loads(options_bytes)
    if payload_digest(reader_files["frozen-options.json"]) != reader["options_sha256"]:
        raise ValueError("options_digest_binding")
    files = {}
    for arm in ("control", "gptgrep"):
        for task_id in ids:
            prefix = f"{arm}/{task_id}"
            matching = {name.removeprefix(prefix + "/"): data for name, data in reader_files.items()
                        if name.startswith(prefix + "/")}
            descriptor_bytes = small_bytes(matching.pop("task-descriptor.json"))
            descriptor = production_json(descriptor_bytes) if production else json.loads(descriptor_bytes)
            if descriptor != {"task_id": task_id, "arm": arm, "options": options}:
                raise ValueError("descriptor_binding")
            allowed = {"instruction.md"} | {name for name in matching if name.startswith("environment/")}
            if set(matching) != allowed:
                raise ValueError("reader_namespace")
            if arm == "control" and any(name.startswith(("environment/prebuilt/", "environment/tool/")) for name in matching):
                raise ValueError("baseline_preparsed_artifact")
            for name, data in matching.items():
                files[f"{prefix}/{name}"] = data
            cfg = config(descriptor, production)
            files[f"{prefix}/task.toml"] = cfg.model_dump_toml().encode()
            files[f"{prefix}/tests/reference.json"] = grader_files[f"{task_id}/reference.json"]
            files[f"{prefix}/tests/verify_answer.py"] = regular(HERE / "verify_answer.py")
            reference_name = "judge-status.json" if production else "reference.json"
            if production:
                files[f"{prefix}/tests/judge-status.json"] = json.dumps({
                    "judge_status": "unavailable", "reward_rule": "original_answer_equivalence_only"}, sort_keys=True).encode()
            files[f"{prefix}/tests/test.sh"] = (
                "#!/bin/sh\nset -eu\nexec python3 /tests/verify_answer.py --answer /app/answer.json "
                f"--reference /tests/{reference_name} --output /logs/verifier/status.json\n").encode()
            files[f"{prefix}/tests/Dockerfile"] = (
                f"FROM {options['verifier_image']}\nWORKDIR /app\nCOPY . /tests/\n".encode()
            )
    files["paired-schedule.json"] = reader_files["paired-schedule.json"]
    files["format-status.json"] = json.dumps({"harbor_version": "0.24.0", "schema_version": "1.4",
        "execution_qualified": False, "judge_status": "unavailable", "reward": None,
        "holds": ["linux_runtime", "sandbox_role_isolation", "original_judge_qualification", "tool_delivery", "trial_phase"]}, sort_keys=True).encode()
    # Reserve bounded space for both dataset manifests and the final inventory receipt.
    if len(files) > 4092 or sum(map(len, files.values())) + 2 * 1024 * 1024 > MAX_TOTAL:
        raise ValueError("complete_package_resource_limit")
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
            "dataset": {"name": f"gptgrep/{'source-structure' if production else 'synthetic'}-{arm}", "version": "0.0.0"},
            "tasks": task_refs[arm], "files": []})
        # Harbor 0.24's serializer omits its default schema root field.
        private_write(root, f"{arm}/dataset.toml", ('schema_version = "1.0"\n' + dataset.to_toml()).encode())
    inventory = {path.relative_to(root).as_posix(): digest(regular(path)) for path in root.rglob("*") if path.is_file()}
    receipt = {"schema_version": "1", "harbor_version": "0.24.0", "task_schema_version": "1.4",
        "dataset_schema_version": "1.0", "reader_manifest_sha256": reader_manifest_sha256,
        "grader_manifest_sha256": grader_manifest_sha256,
        "task_refs": task_refs, "files": inventory, "execution_qualified": False}
    receipt_bytes = json.dumps(receipt, indent=2, sort_keys=True).encode()
    if len(receipt_bytes) > 1024 * 1024 or sum(path.stat().st_size for path in root.rglob("*") if path.is_file()) + len(receipt_bytes) > MAX_TOTAL:
        raise ValueError("complete_package_resource_limit")
    private_write(root, "package-receipt.json", receipt_bytes)
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
