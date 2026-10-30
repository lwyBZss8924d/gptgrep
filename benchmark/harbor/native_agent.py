"""Pinned Harbor bridge to GPTgrep's native reader; owner admission precedes execution.

Contract: Harbor 0.24.0 BaseAgent API, retaining the explicit model-parsing
option of source revision 6099b8cf2bfbbb3e7ff1eb2163bfc4524dbd9ba5 when declared.
The owner binds the actual namespace and each phase after Harbor startup. This
bridge retains sealed host bind originals without a second Compose exec/copy.
``controller.ts`` owns the reader implementation and unconditional outer
finalization. This class neither chooses tasks nor reads judge or credential
contents. ATIF capability is separate from runtime/coverage acceptance.
"""

from __future__ import annotations

import asyncio
import hashlib
import inspect
import json
import os
import re
import signal
import stat
import sys
from pathlib import Path, PurePosixPath
from typing import Any

from harbor.agents.base import BaseAgent
from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.options import AgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


MIB = 1024 * 1024
SETUP_SECONDS = 60
TRANSFER_SECONDS = 120
HASH = re.compile(r"^[a-f0-9]{64}$")
OWNER_INTERFACE_STATUS = "interface-contract-v6.4-frozen-for-source-preparation"
OWNER_INTERFACE_STATUSES = {"interface-contract-v6.1-frozen-for-source-preparation",
                            "interface-contract-v6.2-frozen-for-source-preparation",
                            "interface-contract-v6.3-frozen-for-source-preparation", OWNER_INTERFACE_STATUS}
ROLES = {"attempt": 4 * MIB, "trace": 64 * MIB, "trajectory": 128 * MIB,
         "coverage": 4 * MIB, "answer": 4 * MIB, "media": 64 * MIB,
         "start": MIB, "denial": MIB, "preflight": 64 * 1024}
# Retained source artifacts and both projected ATIF copies are counted
# separately. This fixed portion is manifest + projected attempt + collection.
HOST_COLLECTION_FIXED_OVERHEAD = MIB + 8 * MIB + MIB


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def _hash(path: Path) -> str:
    _require(path.name != "auth.json", "Credential content is outside the bridge evidence boundary")
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(MIB), b""):
            value.update(chunk)
    return value.hexdigest()


def _physical(path: Path) -> Path:
    _require(path.is_absolute(), "An absolute physical path is required")
    _require(path.resolve() == path, "Symlink or noncanonical path is not admitted")
    return path


def _json(path: Path, limit: int = MIB, expected: str | None = None) -> Any:
    _require(path.name != "auth.json", "Credential content is outside the bridge evidence boundary")
    _physical(path)
    _require(path.is_file() and not path.is_symlink() and path.stat().st_size <= limit,
             "Expected a bounded regular JSON artifact")
    data = path.read_bytes()
    if expected is not None:
        _require(bool(HASH.fullmatch(expected)) and hashlib.sha256(data).hexdigest() == expected,
                 "Pinned JSON artifact changed")
    return _decode(data)


def _decode(data: bytes) -> Any:
    def pairs(items: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in items:
            _require(key not in result, "Duplicate JSON key")
            result[key] = value
        return result
    def nonfinite(_value: str) -> Any:
        raise ValueError("Nonfinite JSON number")
    return json.loads(data, object_pairs_hook=pairs, parse_constant=nonfinite)


def _container_path(value: Any) -> str:
    _require(isinstance(value, str) and value.startswith("/") and value != "/"
             and not any(c in value for c in "\n\r\0\\")
             and str(PurePosixPath(value)) == value and ".." not in value.split("/"),
             "An absolute canonical container path is required")
    return value


def _pin(value: Any) -> dict[str, str]:
    _require(isinstance(value, dict) and set(value) == {"path", "sha256"}
             and isinstance(value["sha256"], str) and bool(HASH.fullmatch(value["sha256"])),
             "An exact path and SHA-256 pin is required")
    _container_path(value["path"])
    return value


def _within(root: str, path: str) -> bool:
    return path == root or path.startswith(root + "/")


def _file_pin(path: Path) -> dict[str, str]:
    _physical(path)
    _require(path.is_file() and not path.is_symlink(), "A physical regular pinned file is required")
    return {"path": str(path), "sha256": _hash(path)}


def _verified_pin(value: Any) -> dict[str, str]:
    pin = _pin(value)
    _require(_file_pin(Path(pin["path"])) == pin, "Pinned host file changed")
    return pin


def _proc_environment_contract(owner: dict[str, Any], template: dict[str, Any], directory: str) -> dict[str, Any] | None:
    if "procEnvironment" not in owner:
        return None
    selected = owner["procEnvironment"]
    _require(isinstance(selected, dict) and set(selected) == {"module", "contract"},
             "Unknown selected proc environment")
    for value in selected.values():
        _verified_pin(value)
    contract = _json(Path(selected["contract"]["path"]), expected=selected["contract"]["sha256"])
    fields = {"schema", "module", "harborSources", "sessionDerivation", "environmentDirectory",
              "composeProfile", "engineSocket", "engineBinary", "composePlugin", "engineClientConfigRoot", "createRequest"}
    _require(isinstance(contract, dict) and set(contract) == fields
             and contract["schema"] == "gptgrep.harbor-proc-environment.v1"
             and contract["module"] == selected["module"], "Selected proc environment contract differs")
    sources = contract["harborSources"]
    _require(isinstance(sources, dict) and set(sources) == {"docker", "unix", "runtime", "factory", "config"}
             and sources["docker"] == owner["harborSources"]["docker"],
             "Selected proc environment lacks its exact Harbor source closure")
    _require(contract["sessionDerivation"] == {"rule": "harbor-trial-environment-v1", "source": owner["harborSources"]["trial"]}
             and contract["environmentDirectory"] == directory
             and contract["composeProfile"] == template["compose_profile"],
             "Selected proc environment changes its task directory, profile or session derivation")
    pins = [*selected.values(), *sources.values(), contract["sessionDerivation"]["source"]]
    for value in pins:
        _verified_pin(value)
        _require(value in template["host_source_closure"], "Owner source closure omits the selected proc environment")
    return contract


def _identity(path: Path, expected: dict[str, Any], *, directory: bool = False) -> None:
    _physical(path)
    info = path.lstat()
    _require((stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode))
             and not stat.S_ISLNK(info.st_mode)
             and (directory or info.st_nlink == 1), "Sealed original changed file type or link custody")
    actual = {"device": str(info.st_dev), "inode": str(info.st_ino), "uid": info.st_uid,
              "gid": info.st_gid, "mode": f"{stat.S_IMODE(info.st_mode):04o}"}
    _require(actual == expected, "Sealed original physical identity changed")


def _stable_container(value: dict[str, Any]) -> dict[str, Any]:
    # The owner permits only these two host-memory telemetry fields to vary
    # between observations of the same running container generation.
    result = json.loads(json.dumps(value))
    resources = result.get("security", {}).get("resources", {})
    resources.pop("host_mem_available_bytes", None)
    resources.pop("host_observation_sha256", None)
    return result


def _map_guest(base: dict[str, Any], guest_path: str, *, read_only: bool = False) -> str:
    """Resolve only the most-specific bind recorded in the admitted container binding."""
    _container_path(guest_path)
    rows = base.get("mounts")
    _require(isinstance(rows, list) and 1 <= len(rows) <= 32, "Container mount mapping is unavailable")
    matches = []
    for row in rows:
        mount = row.get("template") if isinstance(row, dict) else None
        _require(isinstance(mount, dict), "Container mount mapping is malformed")
        target = _container_path(mount.get("guest_target"))
        if (mount.get("kind") == "file" and target == guest_path or
                mount.get("kind") == "directory" and _within(target, guest_path)):
            matches.append(mount)
    matches.sort(key=lambda mount: len(mount["guest_target"]), reverse=True)
    _require(bool(matches) and (len(matches) == 1 or matches[0]["guest_target"] != matches[1]["guest_target"]),
             "Guest evidence has no unambiguous owner mount mapping")
    mount = matches[0]
    source = _container_path(mount["host_source"])
    _require(mount.get("propagation") == "rprivate" and mount.get("create_host_path") is False
             and (not read_only or mount.get("access") == "read"),
             "Guest evidence mapping is not the admitted bind")
    _physical(Path(source))
    host_path = source if mount["kind"] == "file" else source + guest_path[len(mount["guest_target"]):]
    return str(_physical(Path(host_path)))


def _guest_pin(base: dict[str, Any], pin: dict[str, str]) -> dict[str, str]:
    _pin(pin)
    return {"path": _map_guest(base, pin["path"]), "sha256": pin["sha256"]}


def _validate_schema(value: Any, schema: dict[str, Any], definitions: dict[str, Any]) -> None:
    """Validate the small, pinned owner schema vocabulary without another runtime dependency."""
    _require(set(schema) <= {"$ref", "anyOf", "const", "enum", "type", "properties", "required",
                            "additionalProperties", "items", "minItems", "maxItems", "minLength", "maxLength",
                            "pattern", "minimum", "maximum"}, "Unsupported owner schema keyword")
    if "$ref" in schema:
        reference = schema["$ref"]
        _require(reference.startswith("#/$defs/") and reference[8:] in definitions,
                 "Unknown owner schema reference")
        _validate_schema(value, definitions[reference[8:]], definitions)
        return
    if "anyOf" in schema:
        for option in schema["anyOf"]:
            try:
                _validate_schema(value, option, definitions)
                return
            except ValueError:
                pass
        raise ValueError("Owner artifact does not match any admitted schema variant")
    if "const" in schema:
        _require(type(value) is type(schema["const"]) and value == schema["const"], "Owner artifact constant differs")
    if "enum" in schema:
        _require(any(type(value) is type(option) and value == option for option in schema["enum"]),
                 "Owner artifact enum differs")
    kind = schema.get("type")
    if kind is not None:
        admitted = {"object": dict, "array": list, "string": str, "integer": int,
                    "boolean": bool, "null": type(None)}
        _require(kind in admitted and type(value) is admitted[kind], "Owner artifact type differs")
    if kind == "object":
        properties = schema.get("properties", {})
        _require(set(schema.get("required", [])) <= set(value), "Owner artifact required fields are missing")
        if schema.get("additionalProperties") is False:
            _require(set(value) <= set(properties), "Owner artifact contains unknown fields")
        for key, item in value.items():
            if key in properties:
                _validate_schema(item, properties[key], definitions)
    elif kind == "array":
        _require(schema.get("minItems", 0) <= len(value) <= schema.get("maxItems", len(value)),
                 "Owner artifact array bound differs")
        for item in value:
            _validate_schema(item, schema["items"], definitions)
    elif kind == "string":
        _require(schema.get("minLength", 0) <= len(value) <= schema.get("maxLength", len(value)),
                 "Owner artifact string bound differs")
        if "pattern" in schema:
            _require(re.search(schema["pattern"], value) is not None, "Owner artifact string pattern differs")
    elif kind == "integer":
        _require(schema.get("minimum", value) <= value <= schema.get("maximum", value),
                 "Owner artifact numeric bound differs")


def _write_json(path: Path, value: Any, limit: int = 8 * MIB) -> dict[str, str]:
    _physical(path)
    encoder = json.JSONEncoder(ensure_ascii=False, allow_nan=False)
    size = 1  # Final newline is part of the bound.
    for chunk in encoder.iterencode(value):
        size += len(chunk.encode())
        _require(size <= limit, "Projected artifact exceeds its finite byte bound")
    # Count before exclusive creation: a rejected projection writes no partial.
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600),
                   "w", encoding="utf-8") as stream:
        for chunk in encoder.iterencode(value):
            stream.write(chunk)
        stream.write("\n")
    return {"path": str(path), "sha256": _hash(path)}


class NativeBridgeOptions(AgentOptions):
    bridge_contract_path: str
    bridge_contract_sha256: str


class NativeCodexAgent(BaseAgent):
    """Use ``native_agent:NativeCodexAgent`` on the pinned module search path."""

    capabilities = AgentCapabilities(atif=True)
    options_model = NativeBridgeOptions
    DEFAULT_SETUP_TIMEOUT_SEC = SETUP_SECONDS

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        for name in ("base_url", "model_api_key_name", "model_api_format", "load_trajectory",
                     "skills_dir", "mcp_servers", "extra_env"):
            _require(not kwargs.get(name), f"The frozen native bridge forbids {name} overrides")
        # Harbor 0.24.0 keeps the raw model name without this constructor option;
        # later source revisions declare it to bypass their provider parser.
        if "skip_model_parsing" in inspect.signature(BaseAgent.__init__).parameters:
            kwargs["skip_model_parsing"] = True
        super().__init__(*args, **kwargs)
        assert isinstance(self.options, NativeBridgeOptions)
        self.contract = _json(Path(self.options.bridge_contract_path),
                              expected=self.options.bridge_contract_sha256)
        c = self.contract
        required = {"schema", "taskId", "arm", "instructionSha256", "model", "effort",
                    "serviceTier", "timeoutSeconds", "cwd", "outputRoot", "bun", "bunConfig", "entrypoint",
                    "readerJob", "containerOwner", "authRunId", "maxArtifactBytes",
                    "hostOutputRoot", "agentModule", "maxCollectedBytes",
                    "maxTraceBytes", "maxTrajectoryBytes", "maxMediaBytes"}
        _require(isinstance(c, dict) and set(c) == required
                 and c["schema"] == "gptgrep.harbor-native-bridge.v2", "Unknown bridge contract")
        module_pin = _pin(c["agentModule"])
        module_path = _physical(Path(module_pin["path"]))
        _require(module_path == Path(__file__).resolve() and module_path.is_file()
                 and _hash(module_path) == module_pin["sha256"],
                 "The actual imported native agent module differs from its supplied pin")
        _require(c["model"] == "gpt-6.1-sol" and self.model_name == c["model"],
                 "Harbor must explicitly select the frozen native reader model")
        _require(c["effort"] in {"none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"}
                 and isinstance(c["serviceTier"], str) and bool(c["serviceTier"]),
                 "Exact effort and tier are required; no defaults are selected")
        _require(isinstance(c["taskId"], str) and bool(c["taskId"]) and c["arm"] in {"bare", "gptgrep"}
                 and isinstance(c["authRunId"], str) and bool(c["authRunId"])
                 and isinstance(c["instructionSha256"], str) and bool(HASH.fullmatch(c["instructionSha256"])),
                 "Frozen task, instruction and auth-run identities are required")
        _require(type(c["timeoutSeconds"]) is int and 1 <= c["timeoutSeconds"] <= 86400
                 and type(c["maxArtifactBytes"]) is int and 1 <= c["maxArtifactBytes"] <= 1024 * MIB,
                 "Finite reader time and artifact byte limits are required")
        for field, role in (("maxTraceBytes", "trace"), ("maxTrajectoryBytes", "trajectory"),
                            ("maxMediaBytes", "media")):
            _require(type(c[field]) is int and 1 <= c[field] <= ROLES[role],
                     f"Exact {field} must remain inside the absolute artifact limit")
        self.artifact_limits = {**ROLES, "trace": c["maxTraceBytes"],
                                "trajectory": c["maxTrajectoryBytes"], "media": c["maxMediaBytes"]}
        _require(type(c["maxCollectedBytes"]) is int
                 and c["maxArtifactBytes"] + HOST_COLLECTION_FIXED_OVERHEAD
                 + 2 * c["maxTrajectoryBytes"] <= c["maxCollectedBytes"]
                 <= 1024 * MIB + HOST_COLLECTION_FIXED_OVERHEAD + 2 * ROLES["trajectory"],
                 "Host collection bound must include all source and projected artifact copies")
        for name in ("bun", "bunConfig", "entrypoint", "readerJob"):
            _pin(c[name])
        _container_path(c["cwd"])
        _container_path(c["outputRoot"])
        _require(isinstance(c["hostOutputRoot"], str), "A deterministic host output root is required")
        _physical(Path(c["hostOutputRoot"]))
        _require(not _within(c["cwd"], c["outputRoot"]) and not _within(c["outputRoot"], c["cwd"]),
                 "Reader workspace and private controller outputs must be disjoint")
        self._ready = False
        self._started = False
        self._setup_started = False
        self._runtime_context: dict[str, str] | None = None
        self._admission: dict[str, str] | None = None
        self._base_binding: dict[str, Any] | None = None
        self._parent_binding: dict[str, str] | None = None
        self._phase_evidence: dict[str, dict[str, dict[str, str]]] = {}
        self._phase_seals: dict[str, dict[str, Any]] = {}
        self._namespace_plan: dict[str, str] | None = None
        self._proc_contract: dict[str, Any] | None = None
        self._adapter_created_id: str | None = None
        self._initialize_owner()

    def _initialize_owner(self) -> None:
        c = self.contract
        owner = c["containerOwner"]
        fields = {"schema", "interfaceContract", "python", "cli", "template", "parentStorage",
                  "harborSources", "hostEpisodeRoot", "runtimeContextPath", "authProjectionPath",
                  "networkEvidencePath", "namespacePlanReviewPath", "phaseEvidenceRoot",
                  "ownerCallTimeoutSeconds", "derivedPlanRule"}
        if isinstance(owner, dict) and "procEnvironment" in owner:
            fields.add("procEnvironment")
        _require(isinstance(owner, dict) and set(owner) == fields
                 and owner["schema"] == "gptgrep.harbor-container-owner.v1", "Unknown container owner contract")
        for name in ("interfaceContract", "python", "cli", "template", "derivedPlanRule"):
            _verified_pin(owner[name])
        parent = owner["parentStorage"]
        _require(isinstance(parent, dict) and set(parent) == {"runId", "workloadSha256", "cwd"}
                 and isinstance(parent["runId"], str) and bool(parent["runId"])
                 and isinstance(parent["workloadSha256"], str) and bool(HASH.fullmatch(parent["workloadSha256"])),
                 "Exact immutable parent run/workload/cwd identity is required")
        _physical(Path(parent["cwd"]))
        interface = _json(Path(owner["interfaceContract"]["path"]), expected=owner["interfaceContract"]["sha256"])
        _require(interface.get("schema") == "environments-harness.container-storage-design-contract.v1"
                 and interface.get("status") in OWNER_INTERFACE_STATUSES,
                 "The reviewed owner interface version is required")
        self._owner_definitions = interface["schema_bundle"]["$defs"]
        self._owner_template = self._owner_json(owner["template"], "ownerTemplate")
        template = self._owner_template
        self._derived_rule = self._owner_json(owner["derivedPlanRule"], "derivedPlanRule")
        rule = self._derived_rule
        _require(rule["owner_template"] == owner["template"] and rule["expected_episode_id"] == template["episode_id"]
                 and rule["fixed_runtime_context"]["host_episode_root"] == owner["hostEpisodeRoot"],
                 "Derived-plan authority does not bind the immutable template and episode")
        for pin in (rule["authority"], *rule["scenario_source_pins"]):
            _verified_pin(pin)
        _require(template["admission_class"] == "reader" and template["auth_supervisor_source"] is not None
                 and template["sandbox_privacy_qualification"] is not None
                 and template["experiment_arm"] == {"bare": "baseline", "gptgrep": "treatment"}[c["arm"]],
                 "Reader owner template lacks exact arm, privacy qualification or authentication source")
        sources = owner["harborSources"]
        _require(isinstance(sources, dict) and set(sources) == {"docker", "trial", "base", "prebuilt"},
                 "Exact pinned Harbor source interfaces are required")
        for pin in sources.values():
            _verified_pin(pin)
        _require(template["harbor_source"] == sources["docker"], "Owner template names another Harbor implementation")
        if interface["status"] in {"interface-contract-v6.3-frozen-for-source-preparation", OWNER_INTERFACE_STATUS}:
            _require(template["harbor_derivation_source"] == sources["trial"],
                     "Owner template names another Harbor trial derivation source")
        _require(rule["harbor_derivation_source"] == sources["trial"], "Derived rule names another Harbor derivation source")
        self._proc_contract = _proc_environment_contract(owner, template,
            rule["fixed_runtime_context"]["compose_project_directory"])
        for name in ("hostEpisodeRoot", "runtimeContextPath", "authProjectionPath", "networkEvidencePath", "namespacePlanReviewPath", "phaseEvidenceRoot"):
            _physical(Path(owner[name]))
        _require(type(owner["ownerCallTimeoutSeconds"]) is int and 1 <= owner["ownerCallTimeoutSeconds"] <= 300,
                 "Owner setup/metadata transport requires a finite timeout")
        _require(_within(owner["hostEpisodeRoot"], owner["runtimeContextPath"])
                 and _within(owner["hostEpisodeRoot"], owner["phaseEvidenceRoot"])
                 and not _within(c["hostOutputRoot"], owner["phaseEvidenceRoot"]),
                 "Owner evidence needs its frozen private episode scope, separate from projected artifacts")
        self._commands = {value["id"]: value for value in template["commands"]}
        _require(len(self._commands) == 2 and set(self._commands) == {"preflight", "reader"},
                 "The reader bridge admits exactly preflight and reader owner commands")
        _require(c["bunConfig"]["sha256"] == hashlib.sha256(b"").hexdigest(),
                 "Reader Bun configuration must be empty")
        config_path = Path(_map_guest({"mounts": [{"template": row} for row in template["mounts"]]},
                                      c["bunConfig"]["path"], read_only=True))
        _verified_pin({"path": str(config_path), "sha256": c["bunConfig"]["sha256"]})
        _require(config_path.stat().st_size == 0, "Reader Bun configuration must be empty")
        for mode, command in self._commands.items():
            expected = [c["bun"]["path"], "--no-env-file", "--no-install", "--config=" + c["bunConfig"]["path"],
                        c["entrypoint"]["path"], "--mode", mode,
                        "--job", c["readerJob"]["path"], "--expect-sha256", c["readerJob"]["sha256"],
                        "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"]
            _require(command["controller_argv"] == expected and command["cwd"] == c["cwd"]
                     and command["immutable_job_guest_path"] == c["readerJob"]["path"]
                     and command["immutable_job"]["sha256"] == c["readerJob"]["sha256"],
                     "Owner command differs from the exact frozen detached reader invocation")
            _require(all(c[name] in command["source_pins"] for name in ("bun", "bunConfig", "entrypoint", "readerJob")),
                     "Reader source closure omits a guarded runtime input")
            _verified_pin(command["immutable_job"])
        _require(self._commands["preflight"]["immutable_job"] == self._commands["reader"]["immutable_job"],
                 "Preflight and reader must share one immutable reader job")

    def _owner_json(self, pin: dict[str, str], definition: str) -> Any:
        _pin(pin)
        info = Path(pin["path"]).lstat()
        _require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_uid == os.getuid()
                 and stat.S_IMODE(info.st_mode) == 0o600, "Owner descriptor lost private single-link custody")
        value = _json(Path(pin["path"]), expected=pin["sha256"])
        _validate_schema(value, self._owner_definitions[definition], self._owner_definitions)
        return value

    @staticmethod
    def name() -> str:
        return "gptgrep-native-codex"

    def version(self) -> str:
        return "0.2.0-alpha.1"

    def _host_storage(self) -> None:
        binding = os.environ.get("ENVIRONMENTS_HARNESS_STORAGE_BINDING")
        expected = os.environ.get("ENVIRONMENTS_HARNESS_STORAGE_DIGEST")
        _require(bool(binding) and bool(expected), "Harbor requires an owner-admitted host storage run")
        value = _json(Path(binding), expected=expected)
        owner = self.contract["containerOwner"]
        parent = owner["parentStorage"]
        plan = value.get("plan", {})
        _require(plan.get("run_id") == parent["runId"] and plan.get("workload_sha256") == parent["workloadSha256"]
                 and plan.get("cwd") == parent["cwd"], "Harbor inherited another owner host storage run")
        actual_pin = {"path": binding, "sha256": expected}
        _require(self._parent_binding is None or actual_pin == self._parent_binding,
                 "The actual parent binding changed within one Harbor episode")
        self._parent_binding = actual_pin
        targets = value.get("targets") or value.get("plan", {}).get("targets", {})
        for output in (self.logs_dir, Path(self.contract["hostOutputRoot"]), Path(owner["hostEpisodeRoot"])):
            path = str(_physical(output))
            _require(any(isinstance(targets.get(key), str) and _within(targets[key], path)
                         for key in ("output", "downloads", "test_data")),
                     "Harbor logs and deterministic outputs must remain within admitted host storage")
        _require(self.logs_dir != Path(self.contract["hostOutputRoot"]),
                 "The deterministic host output root must differ from Harbor's trial logs")

    async def _invoke_owner(self, args: list[str], timeout: int) -> dict[str, Any]:
        """Only the pinned owner executes Docker; this bridge never uses a shell or Compose exec."""
        owner = self.contract["containerOwner"]
        self._host_storage()
        for pin in (owner["interfaceContract"], owner["python"], owner["cli"], *self._owner_template["host_source_closure"]):
            _verified_pin(pin)
        argv = [owner["python"]["path"], "-I", "-B", owner["cli"]["path"], "storage-container", *args]
        process = await asyncio.create_subprocess_exec(*argv, stdout=asyncio.subprocess.PIPE,
                                                       stderr=asyncio.subprocess.PIPE, start_new_session=True)
        async def bounded(stream: asyncio.StreamReader) -> bytes:
            chunks: list[bytes] = []
            size = 0
            while chunk := await stream.read(64 * 1024):
                size += len(chunk)
                _require(size <= MIB, "Owner transport exceeded its finite observation bound")
                chunks.append(chunk)
            return b"".join(chunks)
        assert process.stdout is not None and process.stderr is not None
        try:
            async with asyncio.timeout(timeout):
                stdout, _stderr, returncode = await asyncio.gather(bounded(process.stdout), bounded(process.stderr), process.wait())
        except BaseException:
            if process.returncode is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(process.wait(), 5)
                except TimeoutError:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    await process.wait()
            raise
        # A business failure may be a proven terminal with retained artifacts.
        value = _decode(stdout)
        _require(isinstance(value, dict), "Owner returned no bounded JSON result")
        if args[0] != "run":
            _require(returncode == 0, "Owner metadata/admission operation failed")
        elif value.get("business_status") == "passed":
            _require(returncode == 0, "Owner transport failed after reporting a passed business result")
        return value

    def _create_runtime_context(self, environment: BaseEnvironment) -> dict[str, str]:
        owner = self.contract["containerOwner"]
        sources = owner["harborSources"]
        selected = owner.get("procEnvironment")
        expected_module = "proc_environment" if selected else "harbor.environments.docker.docker"
        expected_class = "ProcOverlayDockerEnvironment" if selected else "DockerEnvironment"
        expected_source = selected["module"] if selected else sources["docker"]
        _require(type(environment).__module__ == expected_module
                 and type(environment).__name__ == expected_class
                 and Path(inspect.getfile(type(environment))).resolve() == Path(expected_source["path"]),
                 "Only the exact selected and pinned Harbor environment implementation is admitted")
        _verified_pin(expected_source)
        for name, module_name in (("docker", "harbor.environments.docker.docker"),
                                  ("base", "harbor.environments.base"), ("trial", "harbor.trial.trial")):
            module = sys.modules.get(module_name)
            _require(module is not None and Path(module.__file__).resolve() == Path(sources[name]["path"]),
                     "The running Harbor module does not match its source pin")
            _verified_pin(sources[name])
        session = environment.session_id
        _require(isinstance(session, str) and session.endswith("__env") and len(session) > 5,
                 "Actual Harbor environment session identity is unavailable")
        trial = session[:-5]
        _require(self.session_id == trial + "__agent", "Agent and environment do not belong to the same Harbor trial")
        project = session.lower()
        if not re.match(r"^[a-z0-9]", project):
            project = "0" + project
        project = re.sub(r"[^a-z0-9_-]", "-", project)
        if selected:
            contract = _proc_environment_contract(owner, self._owner_template,
                self._derived_rule["fixed_runtime_context"]["compose_project_directory"])
            _require(contract == self._proc_contract, "Selected proc environment input changed within the episode")
            module = sys.modules.get("proc_environment")
            _require(module is not None and Path(module.__file__).resolve() == Path(selected["module"]["path"]),
                     "The loaded proc environment does not match its exact source pin")
            profile = self._owner_template["container_policy"]["seccomp_profile"]
            _verified_pin(profile)
            _require(Path(profile["path"]).stat().st_size <= MIB, "Pinned proc seccomp profile exceeds its bound")
            module.validate_owner_template(contract, self._owner_template, Path(profile["path"]).read_bytes())
            identity = environment.proc_environment_identity
            request = json.loads(json.dumps(contract["createRequest"]))
            request["Labels"]["com.docker.compose.project"] = project
            request_sha = hashlib.sha256(json.dumps(request, sort_keys=True, separators=(",", ":"),
                                                   ensure_ascii=False, allow_nan=False).encode()).hexdigest()
            _require(isinstance(identity, dict) and set(identity) == {"contract_pin", "container_id", "session_id", "request_sha256"}
                     and identity["contract_pin"] == selected["contract"] and identity["session_id"] == session
                     and isinstance(identity["container_id"], str) and bool(HASH.fullmatch(identity["container_id"]))
                     and identity["request_sha256"] == request_sha,
                     "The selected proc environment lacks its exact created-container observation")
            self._adapter_created_id = identity["container_id"]
        compose_files = [_file_pin(_physical(Path(path))) for path in environment._docker_compose_paths]
        network = Path(owner["networkEvidencePath"])
        auth = Path(owner["authProjectionPath"])
        context = {"schema": "environments-harness.container-storage-runtime-context.v1",
                   "episode_id": self._owner_template["episode_id"], "compose_project": project,
                   "compose_project_directory": str(_physical(Path(environment.environment_dir))),
                   "compose_files": compose_files, "service": "main", "host_episode_root": owner["hostEpisodeRoot"],
                   "harbor_source": sources["docker"], "derivation": {"trial_name": trial,
                       "environment_session_id": session, "compose_project_rule": "Harbor pinned DockerEnvironment derivation",
                       "harbor_derivation_source": sources["trial"], "expected_episode_id": self._owner_template["episode_id"]},
                   "auth_projection": _file_pin(auth) if auth.is_file() else None,
                   "network_evidence": _file_pin(network) if network.is_file() else None}
        _validate_schema(context, self._owner_definitions["runtimeContext"], self._owner_definitions)
        # Persist actual identity before the later checks can fail. Outer finalize
        # consumes this pin; failures before setup use its explicit absent-context mode.
        Path(owner["runtimeContextPath"]).parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        pin = _write_json(Path(owner["runtimeContextPath"]), context, MIB)
        self._runtime_context = pin
        _require(context["compose_project_directory"] == self._derived_rule["fixed_runtime_context"]["compose_project_directory"]
                 and len(compose_files) <= self._derived_rule["max_compose_files"]
                 and environment._use_prebuilt is True and environment._enable_environment_dir_upload is False
                 and (environment.proc_implicit_mounts_suppressed is True and not environment._mounts and not environment._persistent_env
                      if selected else environment.auto_mounting is False)
                 and environment._enable_egress_control is False
                 and not environment.stream_enabled and not environment._is_windows_container
                 and environment.task_env_config.docker_image == self._owner_template["image_pin"]
                 and not environment._environment_docker_compose_path.exists()
                 and [str(Path(path)) for path in environment.extra_docker_compose_paths]
                 == [self._owner_template["compose_profile"]["path"]]
                 and sources["prebuilt"] in compose_files
                 and self._owner_template["compose_profile"] in compose_files,
                 "Harbor did not use the frozen prebuilt, no-upload, explicit-mount route")
        return pin

    async def _admit(self, environment: BaseEnvironment) -> None:
        self._host_storage()
        owner, template = self.contract["containerOwner"], self._owner_template
        assert self._parent_binding is not None
        self._runtime_context = self._create_runtime_context(environment)
        context = self._runtime_context
        result = await self._invoke_owner(["plan", "--template", owner["template"]["path"],
            "--expect-template-digest", owner["template"]["sha256"], "--runtime-context", context["path"],
            "--expect-context-digest", context["sha256"], "--parent-binding", self._parent_binding["path"],
            "--expect-parent-digest", self._parent_binding["sha256"], "--out", template["outputs"]["namespace_plan"]],
            owner["ownerCallTimeoutSeconds"])
        _validate_schema(result, self._owner_definitions["planResult"], self._owner_definitions)
        plan_pin = _file_pin(Path(template["outputs"]["namespace_plan"]))
        plan = self._owner_json(plan_pin, "namespacePlan")
        _require(result["ok"] is True and result["plan"] == plan_pin and result["container_id"] == plan["container"]["id"]
                 and (self._adapter_created_id is None or plan["container"]["id"] == self._adapter_created_id)
                 and plan["owner_template"] == owner["template"] and plan["runtime_context"] == context
                 and plan["parent"]["binding"] == self._parent_binding
                 and plan["outputs"] == template["outputs"]
                 and plan["source_closure_sha256"] == self._derived_rule["validator_source_closure_sha256"],
                 "Observed namespace plan differs from its frozen template or actual runtime context")
        result = await self._invoke_owner(["validate-derived-plan", "--plan", plan_pin["path"],
            "--expect-digest", plan_pin["sha256"], "--rule", owner["derivedPlanRule"]["path"],
            "--expect-rule-digest", owner["derivedPlanRule"]["sha256"], "--out", owner["namespacePlanReviewPath"]],
            owner["ownerCallTimeoutSeconds"])
        review_pin = _file_pin(Path(owner["namespacePlanReviewPath"]))
        review = self._owner_json(review_pin, "derivedPlanReview")
        _require(result == review and review["rule"] == owner["derivedPlanRule"]
                 and review["owner_template"] == owner["template"] and review["runtime_context"] == context
                 and review["namespace_plan"] == plan_pin
                 and review["validator_source_closure_sha256"] == self._derived_rule["validator_source_closure_sha256"]
                 and _stable_container(review["container"]) == _stable_container(plan["container"]),
                 "The actual namespace plan lacks authorized exact-plan review evidence")
        self._namespace_review = review_pin
        self._namespace_plan = plan_pin
        self._phase_request("admit")
        result = await self._invoke_owner(["admit", "--plan", plan_pin["path"], "--expect-digest", plan_pin["sha256"],
                                          "--review", review_pin["path"], "--expect-review-digest", review_pin["sha256"]],
                                          owner["ownerCallTimeoutSeconds"])
        _validate_schema(result, self._owner_definitions["admissionResult"], self._owner_definitions)
        self._admission = _file_pin(Path(template["outputs"]["admission"]))
        admission = self._owner_json(self._admission, "admission")
        _require(result["ok"] is True and result["admission"] == self._admission
                 and result["container_binding"] == admission["container_binding"]
                 and result["container_id"] == admission["container"]["id"]
                 and admission["owner_template"] == owner["template"]
                 and admission["runtime_context"] == context and admission["namespace_plan"] == plan_pin
                 and admission["parent_host_binding"] == self._parent_binding
                 and admission["commands"] == template["commands"], "Owner admission identity differs")
        self._base_binding = self._owner_json(admission["container_binding"], "containerBinding")
        binding = self._base_binding
        runtime = self._owner_json(context, "runtimeContext")
        _require(binding["owner_template_sha256"] == owner["template"]["sha256"]
                 and binding["host_namespace_plan_sha256"] == plan_pin["sha256"]
                 and binding["parent_host_binding_sha256"] == self._parent_binding["sha256"]
                 and _stable_container(binding["container"]) == _stable_container(admission["container"])
                 and binding["model_profile"] == {"path": template["model_profile_guest_path"],
                                                   "sha256": template["model_profile"]["sha256"]}
                 and binding["model_read_roots"] == template["model_read_roots"]
                 and binding["experiment_arm"] == template["experiment_arm"] and binding["admission_class"] == "reader"
                 and runtime["auth_projection"] is not None and runtime["network_evidence"] is not None
                 and binding["auth_projection_sha256"] == runtime["auth_projection"]["sha256"]
                 and binding["network_evidence_sha256"] == runtime["network_evidence"]["sha256"]
                 and binding["sandbox_privacy_qualification_sha256"] == template["sandbox_privacy_qualification"]["sha256"],
                 "Actual container binding differs")
        _verified_pin(_guest_pin(binding, binding["model_profile"]))

    def _phase_request(self, mode: str) -> None:
        root = _physical(Path(self.contract["containerOwner"]["phaseEvidenceRoot"]))
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        _write_json(root / f"{mode}.requested.json", {
            "schema": "gptgrep.harbor-owner-phase-request.v1", "commandId": mode,
            "runtimeContext": self._runtime_context, "namespacePlan": self._namespace_plan,
            "admission": self._admission}, MIB)

    async def _owner_run(self, mode: str) -> dict[str, Any]:
        _require(mode in {"preflight", "reader"} and self._admission is not None
                 and self._base_binding is not None, "Owner admission must precede any guest command")
        owner, c = self.contract["containerOwner"], self.contract
        self._phase_request(mode)
        result = await self._invoke_owner(["run", "--admission", self._admission["path"],
            "--expect-admission-digest", self._admission["sha256"], "--command-id", mode],
            (c["timeoutSeconds"] if mode == "reader" else SETUP_SECONDS) + owner["ownerCallTimeoutSeconds"])
        result_pin = _write_json(Path(owner["phaseEvidenceRoot"]) / f"{mode}.result.json", result, MIB)
        _validate_schema(result, self._owner_definitions["runResult"], self._owner_definitions)
        _require(result["command_id"] == mode and result["owner_template_sha256"] == owner["template"]["sha256"]
                 and result["admission_sha256"] == self._admission["sha256"], "Owner phase result is foreign")
        _require(result["terminal_status"] == "verified-terminal" and result["collection_seal"] is not None
                 and result["startup"] is not None and result["command_plan"] is not None,
                 "Owner terminal/custody proof is unavailable; reservation remains held")
        plan = self._owner_json(result["command_plan"], "commandPlan")
        binding = self._owner_json(plan["command_binding"], "commandBinding")
        admission = self._owner_json(self._admission, "admission")
        command = self._commands[mode]
        workload = self._owner_json(command["workload"], "workload")
        _require(workload["run_id"] == command["run_id"] and workload["cwd"] == c["cwd"]
                 and (workload["timeout_seconds"] == c["timeoutSeconds"] if mode == "reader"
                      else workload["timeout_seconds"] <= SETUP_SECONDS),
                 "Owner phase workload changed its immutable run, cwd or timeout")
        argv = [item.replace("@ACCEPTANCE_PATH@", plan["acceptance"]["path"])
                    .replace("@ACCEPTANCE_SHA256@", plan["acceptance"]["sha256"]) for item in command["controller_argv"]]
        argv_sha = hashlib.sha256(json.dumps(argv, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
        _require(plan["command_id"] == mode and plan["owner_template"] == owner["template"]
                 and plan["admission"] == self._admission and plan["container_binding"] == admission["container_binding"]
                 and plan["controller_argv"] == argv and plan["child_argv"] == command["child_argv"]
                 and plan["environment"] == command["environment"] and plan["allowed_paths"] == command["allowed_paths"]
                 and plan["host_artifact_root"] == command["host_artifact_root"]
                 and plan["guest_startup_path"] == command["guest_startup_path"]
                 and plan["guest_terminal_path"] == command["guest_terminal_path"]
                 and plan["workload"] == {"path": command["workload_guest_path"], "sha256": command["workload"]["sha256"]}
                 and plan["command_binding_guest_path"] == command["guest_command_binding_path"]
                 and plan["acceptance"]["path"] == command["guest_acceptance_path"]
                 and plan["plan"] == binding["plan"] and plan["acceptance"] == binding["acceptance"]
                 and plan["plan"]["cwd"] == c["cwd"] and plan["plan"]["run_id"] == command["run_id"]
                 and plan["plan"]["workload_sha256"] == command["workload"]["sha256"]
                 and plan["plan"]["command"] == {"argv_sha256": argv_sha, "executable": c["bun"]["path"],
                                                     "executable_sha256": c["bun"]["sha256"]}
                 and binding["command_id"] == mode and binding["owner_template_sha256"] == owner["template"]["sha256"]
                 and binding["host_namespace_plan_sha256"] == admission["namespace_plan"]["sha256"]
                 and binding["parent_host_binding_sha256"] == self._parent_binding["sha256"]
                 and binding["container_binding"] == admission["container_binding"]
                 and binding["container_id"] == admission["container"]["id"]
                 and binding["allowed_paths"] == command["allowed_paths"] and binding["admission_class"] == "reader"
                 and binding["immutable_job"] == c["readerJob"], "Materialized owner command differs from its immutable invocation")
        startup = self._owner_json(result["startup"], "execStartup")
        transport = self._owner_json(result["transport"], "transport")
        seal = self._owner_json(result["collection_seal"], "collectionSeal")
        terminal = self._owner_json(seal["guest_terminal"], "guestTerminal")
        _require(terminal["execution_identity"]["path"] == plan["guest_execution_identity_path"],
                 "Execution identity is outside the exact command plan")
        execution = self._owner_json(_guest_pin(self._base_binding, terminal["execution_identity"]), "executionIdentity")
        recheck = self._owner_json(seal["host_recheck"], "hostRecheck")
        base = self._base_binding
        for value in (startup, terminal):
            _require(value["command_id"] == mode and value["command_plan_sha256"] == result["command_plan"]["sha256"]
                     and value["command_binding_sha256"] == plan["command_binding"]["sha256"]
                     and value["container_binding_sha256"] == admission["container_binding"]["sha256"]
                     and value["container_id"] == base["container"]["id"] and value["boot_id"] == base["container"]["boot_id"]
                     and value["namespaces"] == base["container"]["namespaces"], "Guest phase observed another namespace generation")
        _require(startup["parent_host_binding_sha256"] == self._parent_binding["sha256"]
                 and startup["command_sha256"] == argv_sha and startup["uid"] == self._owner_template["reader_uid"]
                 and startup["gid"] == self._owner_template["reader_gid"]
                 and terminal["startup_receipt_sha256"] == result["startup"]["sha256"]
                 and terminal["process_baseline"] == startup["process_baseline"]
                 and execution["command_plan_sha256"] == result["command_plan"]["sha256"]
                 and execution["command_binding_sha256"] == plan["command_binding"]["sha256"]
                 and execution["startup_receipt_sha256"] == result["startup"]["sha256"]
                 and execution["guarded_worker_pid"] == startup["child_pid"]
                 and execution["execution_child"] == terminal["execution_child"]
                 and transport["owner_template_sha256"] == owner["template"]["sha256"]
                 and transport["runtime_context_sha256"] == self._runtime_context["sha256"]
                 and transport["context_status"] == "observed"
                 and transport["command_id"] == mode and transport["command_plan_sha256"] == result["command_plan"]["sha256"]
                 and seal["owner_template_sha256"] == owner["template"]["sha256"]
                 and seal["host_namespace_plan_sha256"] == admission["namespace_plan"]["sha256"]
                 and seal["parent_host_binding_sha256"] == self._parent_binding["sha256"]
                 and seal["container_binding_sha256"] == admission["container_binding"]["sha256"]
                 and seal["command_binding_sha256"] == plan["command_binding"]["sha256"]
                 and seal["command_plan_sha256"] == result["command_plan"]["sha256"]
                 and _stable_container(seal["container_generation"]) == _stable_container(base["container"])
                 and seal["container_id"] == base["container"]["id"]
                 and seal["collection_roots"] == base["collection_roots"] and seal["projected_output_root"] == c["hostOutputRoot"]
                 and seal["business_status"] == result["business_status"] and seal["guest_returncode"] == terminal["returncode"]
                 and _stable_container(recheck["container"]) == _stable_container(base["container"])
                 and recheck["owner_template_sha256"] == owner["template"]["sha256"]
                 and recheck["host_namespace_plan_sha256"] == admission["namespace_plan"]["sha256"]
                 and recheck["parent_host_binding_sha256"] == self._parent_binding["sha256"],
                 "Owner collection seal or terminal chain differs from the actual admitted phase")
        if result["business_status"] == "passed":
            _require(terminal["result"] == "success" and terminal["returncode"] == 0
                     and execution["execution_child"] is not None,
                     "Passed phase lacks an observed controller child and successful terminal")
        paths = [item["guest_path"] for item in seal["artifacts"]]
        hosts = [item["host_path"] for item in seal["artifacts"]]
        _require(len(set(paths)) == len(paths) and len(set(hosts)) == len(hosts), "Owner seal has duplicate original mappings")
        roots = {root["purpose"]: root for root in seal["collection_roots"]}
        _require(set(roots) == {"private-output-originals", "answer-stage"}
                 and roots["private-output-originals"]["guest_root"] == c["outputRoot"]
                 and roots["answer-stage"]["guest_root"] == "/app", "Owner collection roots are not the frozen output and answer stage")
        for root in roots.values():
            _identity(Path(root["host_root"]), root["host_identity"], directory=True)
            total = sum(int(item["bytes"]) for item in seal["artifacts"] if _within(root["guest_root"], item["guest_path"]))
            _require(total <= int(root["maximum_total_bytes"]), "Sealed originals exceed the owner collection root bound")
        _require(sum(int(item["bytes"]) for item in seal["artifacts"])
                 <= min(c["maxArtifactBytes"], int(workload["collection_limit_bytes"])),
                 "Owner sealed originals exceed the frozen original/collection byte limits")
        self._phase_evidence[mode] = {"result": result_pin, "commandPlan": result["command_plan"], "startup": result["startup"]}
        self._phase_seals[mode] = seal
        return result

    def _sealed_original(self, mode: str, guest_path: str, limit: int) -> Path:
        seal = self._phase_seals[mode]
        matches = [item for item in seal["artifacts"] if item["guest_path"] == guest_path]
        _require(len(matches) == 1, "Required original is absent from the owner collection seal")
        item = matches[0]
        roots = [root for root in seal["collection_roots"] if _within(root["guest_root"], guest_path)]
        _require(bool(roots), "Sealed original is outside verified bind roots")
        root = max(roots, key=lambda value: len(value["guest_root"]))
        expected = root["host_root"] + guest_path[len(root["guest_root"]):]
        _require(item["host_path"] == expected and 0 < int(item["bytes"]) <= limit,
                 "Sealed original path or finite byte bound differs")
        path = Path(item["host_path"])
        _identity(Path(root["host_root"]), root["host_identity"], directory=True)
        _identity(path, item["host_identity"])
        _require(path.stat().st_size == int(item["bytes"]) and _hash(path) == item["sha256"],
                 "Sealed original bytes changed")
        return path

    def _sealed_pin(self, mode: str, guest_path: str, limit: int) -> dict[str, str]:
        path = self._sealed_original(mode, guest_path, limit)
        item = next(item for item in self._phase_seals[mode]["artifacts"] if item["guest_path"] == guest_path)
        return {"path": str(path), "sha256": item["sha256"]}

    def _verify_readiness(self, ready: Any) -> None:
        c = self.contract
        evidence = self._phase_evidence["preflight"]
        plan = self._owner_json(evidence["commandPlan"], "commandPlan")
        admission = self._owner_json(self._admission, "admission")
        expected = {"schema": "gptgrep.harbor-native-readiness.v2", "status": "ready",
                    "jobSha256": c["readerJob"]["sha256"], "taskId": c["taskId"], "arm": c["arm"],
                    "model": c["model"], "effort": c["effort"], "serviceTier": c["serviceTier"],
                    "commandId": "preflight", "commandBinding": {"path": plan["command_binding_guest_path"],
                        "sha256": plan["command_binding"]["sha256"]}, "storageBindingSha256": plan["command_binding"]["sha256"],
                    "containerBindingSha256": admission["container_binding"]["sha256"],
                    "ownerTemplateSha256": c["containerOwner"]["template"]["sha256"],
                    "parentHostBindingSha256": self._parent_binding["sha256"],
                    "acceptance": plan["acceptance"], "authRunId": c["authRunId"]}
        _require(ready == expected, "Sealed readiness differs from the exact admitted preflight")

    async def setup(self, environment: BaseEnvironment) -> None:
        _require(not self._setup_started and not self._started and not self._ready,
                 "A reader bridge instance cannot be reused")
        self._setup_started = True
        self._host_storage()
        async with asyncio.timeout(SETUP_SECONDS):
            await self._admit(environment)
            result = await self._owner_run("preflight")
        _require(result["ok"] is True and result["business_status"] == "passed", "Owner preflight business result failed")
        ready_pin = self._sealed_pin("preflight", self.contract["outputRoot"] + "/preflight-readiness.json", 64 * 1024)
        ready = _json(Path(ready_pin["path"]), 64 * 1024, ready_pin["sha256"])
        self._verify_readiness(ready)
        self._ready = True

    async def _collect(self, environment: BaseEnvironment, context: AgentContext) -> dict[str, Any]:
        c = self.contract
        self._host_storage()
        host_root = Path(c["hostOutputRoot"])
        host_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        manifest_path = c["outputRoot"] + "/bridge-manifest.json"
        manifest_pin = self._sealed_pin("reader", manifest_path, MIB)
        manifest_original = Path(manifest_pin["path"])
        manifest = _json(manifest_original, expected=manifest_pin["sha256"])
        _require(isinstance(manifest, dict) and set(manifest) == {
            "schema", "executionMode", "readerJobSha256", "taskId", "arm", "instructionSha256", "artifacts", "totalBytes"
        } and manifest["schema"] == "gptgrep.harbor-native-artifacts.v1" and manifest["executionMode"] == "live"
            and manifest["readerJobSha256"] == c["readerJob"]["sha256"]
            and manifest["taskId"] == c["taskId"] and manifest["arm"] == c["arm"]
            and manifest["instructionSha256"] == c["instructionSha256"], "Bridge manifest identity mismatch")
        artifacts = manifest["artifacts"]
        _require(isinstance(artifacts, list) and 1 <= len(artifacts) <= 4096, "Invalid artifact inventory")
        paths: set[str] = set()
        roles: dict[str, dict[str, Any]] = {}
        for item in artifacts:
            _require(isinstance(item, dict) and set(item) == {"role", "path", "sha256", "bytes"}
                     and item["role"] in ROLES, "Unknown native artifact")
            _pin({"path": item["path"], "sha256": item["sha256"]})
            _require(item["path"] not in paths and (item["role"] == "media" or item["role"] not in roles)
                     and (_within(c["outputRoot"], item["path"]) or
                          item["role"] == "answer" and item["path"] == "/app/answer.json")
                     and type(item["bytes"]) is int and 0 < item["bytes"] <= self.artifact_limits[item["role"]],
                     "Artifact path, role or byte bound is invalid")
            paths.add(item["path"])
            roles[item["role"]] = item
        _require(type(manifest["totalBytes"]) is int and manifest["totalBytes"] == sum(i["bytes"] for i in artifacts)
                 and manifest["totalBytes"] <= c["maxArtifactBytes"] and "attempt" in roles and "preflight" in roles,
                 "Aggregate artifact byte limit or required terminal attempt is missing")
        _require(sum(item["bytes"] for item in artifacts if item["role"] == "media") <= c["maxMediaBytes"],
                 "Aggregate media retention exceeds the frozen media byte limit")
        collection_peak = manifest["totalBytes"] + manifest_original.stat().st_size + 8 * MIB + MIB
        if "trajectory" in roles:
            collection_peak += 2 * c["maxTrajectoryBytes"]
        _require(collection_peak <= c["maxCollectedBytes"],
                 "Retained source and projected artifacts exceed the frozen host collection bound")
        saved: dict[str, Path] = {}
        for item in artifacts:
            destination = self._sealed_original("reader", item["path"], self.artifact_limits[item["role"]])
            _require(destination.is_file() and destination.stat().st_size == item["bytes"]
                     and _hash(destination) == item["sha256"], "Manifested original differs from its owner-sealed bytes")
            saved[item["path"]] = destination
        _require(roles["preflight"]["path"] == c["outputRoot"] + "/preflight-readiness.json",
                 "Manifest preflight path differs from the frozen readiness artifact")
        original_ready = self._sealed_original("preflight", roles["preflight"]["path"], 64 * 1024)
        _require(_hash(original_ready) == roles["preflight"]["sha256"], "Reader changed the exact sealed preflight bytes")
        self._verify_readiness(_json(saved[roles["preflight"]["path"]], 64 * 1024, roles["preflight"]["sha256"]))
        reader_plan = self._owner_json(self._phase_evidence["reader"]["commandPlan"], "commandPlan")
        attempt = _json(saved[roles["attempt"]["path"]], 4 * MIB, roles["attempt"]["sha256"])
        _require(attempt.get("schema") == "gptgrep.reader-attempt.v1" and attempt.get("executionMode") == "live"
                 and attempt.get("taskId") == c["taskId"] and attempt.get("arm") == c["arm"]
                 and attempt.get("authRunId") == c["authRunId"]
                 and attempt.get("storageBindingSha256") == reader_plan["command_binding"]["sha256"]
                 and attempt.get("status") in {"completed", "failed", "timeout", "setup_failed", "answer_invalid"}
                 and type(attempt.get("validStart")) is bool, "Reader terminal artifact is missing or foreign")
        native = attempt.get("native")
        context.metadata = {"bridge_contract_sha256": self.options.bridge_contract_sha256,
                            "reader_status": attempt["status"], "reader_valid_start": attempt["validStart"],
                            "artifact_manifest_sha256": manifest_pin["sha256"],
                            "provider_request_count": None, "cost_usd": None, "runtime_accepted": False}
        if native is not None:
            _require(isinstance(native, dict) and native.get("schema") == "gptgrep.native-run.v1"
                     and native.get("executionMode") == "live" and native.get("validStart") == attempt["validStart"]
                     and native.get("requestedModel") == c["model"]
                     and native.get("requestedEffort") == c["effort"]
                     and native.get("requestedServiceTier") == c["serviceTier"], "Native model configuration changed")
        if attempt["validStart"]:
            _require(isinstance(native, dict) and bool(native.get("threadId")) and bool(native.get("turnId"))
                     and all(role in roles for role in ("trace", "trajectory", "coverage")),
                     "Started reader lacks raw capture or trajectory artifacts")
        if "trajectory" in roles:
            _require(isinstance(native, dict) and "coverage" in roles and "trace" in roles,
                     "Trajectory requires native identity, raw capture and coverage")
            trajectory = _json(saved[roles["trajectory"]["path"]], c["maxTrajectoryBytes"], roles["trajectory"]["sha256"])
            coverage = _json(saved[roles["coverage"]["path"]], ROLES["coverage"], roles["coverage"]["sha256"])
            provenance = coverage.get("provenance", {})
            _require(coverage.get("schema") == "gptgrep.trajectory-coverage.v1"
                     and coverage.get("executionMode") == "live"
                     and trajectory.get("schema_version") == "ATIF-v1.8"
                     and provenance.get("trace") == {k: roles["trace"][k] for k in ("path", "sha256")}
                     and native.get("tracePath") == roles["trace"]["path"]
                     and provenance.get("sourceSha256") == roles["trace"]["sha256"]
                     and isinstance(native.get("runtimeBinarySha256"), str)
                     and bool(HASH.fullmatch(native["runtimeBinarySha256"]))
                     and provenance.get("runtimeBinarySha256") == native.get("runtimeBinarySha256")
                     and trajectory.get("trajectory_id") == "codex-thread:" + native.get("threadId", "")
                     and trajectory.get("extra", {}).get("native_turn_id") == native.get("turnId"),
                     "ATIF provenance differs from the captured native episode")
            media = {item["path"]: str(saved[item["path"]])
                     for item in artifacts if item["role"] == "media"}
            def relocate(value: Any) -> None:
                if isinstance(value, dict):
                    source = value.get("source")
                    if isinstance(source, dict) and source.get("path") in media:
                        source["path"] = media[source["path"]]
                    for child in value.values():
                        relocate(child)
                elif isinstance(value, list):
                    for child in value:
                        relocate(child)
            relocate(trajectory)
            trajectory.setdefault("extra", {})["harbor_bridge"] = {
                "source_trajectory_sha256": roles["trajectory"]["sha256"],
                "media_relocation": media, "coverage_complete": coverage.get("complete"),
                "runtime_accepted": False}
            _write_json(self.logs_dir / "trajectory.json", trajectory, c["maxTrajectoryBytes"])
            _write_json(host_root / "trajectory.json", trajectory, c["maxTrajectoryBytes"])
            context.metadata["trajectory_coverage"] = {k: coverage.get(k) for k in ("complete", "gaps", "liveQualified")}
        if isinstance(native, dict):
            context.metadata["root_native_usage"] = native.get("usage")
            context.metadata["usage_scope"] = native.get("usageScope")
            # Root-thread totals cannot honestly represent workers/helper traffic.
            context.n_input_tokens = context.n_cache_tokens = context.n_output_tokens = None
            context.cost_usd = None
            context.model_usage = None
        if attempt["status"] == "completed":
            _require(attempt["validStart"] and "answer" in roles
                     and attempt.get("answerSha256") == roles["answer"]["sha256"]
                     and native.get("status") == "completed" and native.get("cleanup", {}).get("completed") is True
                     and native.get("modelBoundaryValid") is True and native.get("observedModel") == c["model"]
                     and not native.get("traceTruncated") and not native.get("authFailure"),
                     "Completed reader lacks a bound answer or valid native terminal boundary")
            answer_stage = self._sealed_original("reader", "/app/answer.json", ROLES["answer"])
            _require(answer_stage.stat().st_size == roles["answer"]["bytes"]
                     and _hash(answer_stage) == roles["answer"]["sha256"],
                     "The exact owner-sealed /app/answer.json differs from the completed answer")
        _require("artifactRelocation" not in attempt, "A container attempt cannot supply host relocation authority")
        projected = json.loads(json.dumps(attempt))
        if attempt.get("answerPath") is not None:
            _require("answer" in roles and attempt["answerPath"] == roles["answer"]["path"]
                     and attempt.get("answerSha256") == roles["answer"]["sha256"],
                     "Source answer path is not the exact manifested answer")
            projected["answerPath"] = str(saved[attempt["answerPath"]])
        else:
            _require(attempt.get("answerSha256") is None, "Missing source answer must not carry a digest")
        if isinstance(native, dict):
            _require("trace" in roles and native.get("tracePath") == roles["trace"]["path"],
                     "Source native trace path is not the exact manifested capture")
            projected["native"]["tracePath"] = str(saved[native["tracePath"]])
        source_attempt = {"path": str(saved[roles["attempt"]["path"]]), "sha256": roles["attempt"]["sha256"]}
        source_manifest = manifest_pin
        projected["artifactRelocation"] = {
            "sourceAttempt": source_attempt, "sourceManifest": source_manifest,
            "files": [{"sourcePath": item["path"],
                       "artifact": {"path": str(saved[item["path"]]), "sha256": item["sha256"]}}
                      for item in artifacts]}
        projected_pin = _write_json(host_root / "attempt.json", projected)
        collection = {
            "schema": "gptgrep.harbor-native-collection.v2", "taskId": c["taskId"], "arm": c["arm"],
            "readerJob": c["readerJob"], "bridgeContract": {"path": self.options.bridge_contract_path,
                                                             "sha256": self.options.bridge_contract_sha256},
            "sourceManifest": source_manifest, "sourceAttempt": source_attempt, "attempt": projected_pin,
            "status": attempt["status"], "validStart": attempt["validStart"],
            "trajectory": {"path": str(host_root / "trajectory.json"),
                           "sha256": _hash(host_root / "trajectory.json")} if "trajectory" in roles else None,
            "ownerEvidence": {"ownerTemplate": c["containerOwner"]["template"], "runtimeContext": self._runtime_context,
                              "parentHostBinding": self._parent_binding,
                              "namespacePlanReview": self._namespace_review, "admission": self._admission,
                              "preflight": self._phase_evidence["preflight"], "reader": self._phase_evidence["reader"]}}
        # These are the retained originals, not copies. Recheck custody/bytes
        # after projection and let the outer controller recheck them after finalization.
        _require(_file_pin(self._sealed_original("reader", manifest_path, MIB)) == manifest_pin,
                 "Manifest changed during host projection")
        for item in artifacts:
            self._sealed_original("reader", item["path"], self.artifact_limits[item["role"]])
        _write_json(host_root / "collection.json", collection, MIB)
        context.metadata["host_collection"] = {"path": str(host_root / "collection.json"),
                                                "sha256": _hash(host_root / "collection.json")}
        return projected

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        _require(self._ready and not self._started, "The exact container bridge must pass setup once before one reader")
        c = self.contract
        _require(hashlib.sha256(instruction.encode()).hexdigest() == c["instructionSha256"],
                 "Harbor instruction differs from the frozen reader job")
        # A failed/uncertain submission is not replayed by this agent instance.
        self._started = True
        execution_error: str | None = None
        code: int | None = None
        owner_passed = False
        try:
            result = await self._owner_run("reader")
            code = self._phase_seals["reader"]["guest_returncode"]
            owner_passed = result["ok"] is True and result["business_status"] == "passed"
        except Exception as error:
            # Never persist raw stderr/exception text from an auth-bearing process.
            execution_error = type(error).__name__
        _require("reader" in self._phase_seals,
                 "Reader terminal/custody proof is unavailable; owner request/result evidence remains retained")
        async with asyncio.timeout(TRANSFER_SECONDS):
            attempt = await self._collect(environment, context)
        assert context.metadata is not None
        context.metadata["bridge_exit_code"] = code
        context.metadata["bridge_exception_class"] = execution_error
        _require(execution_error is None and owner_passed and code == 0 and attempt["status"] == "completed",
                 "Native reader did not complete; retained artifacts carry the terminal disposition")
