"""Project-owned Harbor Docker seam for one pinned native sandbox container.

This opt-in environment uses Harbor's task lifecycle and Unix operations. Only
the inherited ``up --detach --wait`` is replaced by one local Engine create and
start. It never builds, pulls, adopts a container, or performs project cleanup.
The owner wrapper validates the pinned create contract against its template
before launching Harbor; the bridge binds the resulting full container ID.
"""

from __future__ import annotations

import asyncio
import copy
import hashlib
import http.client
import inspect
import json
import os
import re
import socket
import stat
import struct
import sys
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import urlencode

from harbor.environments.base import ExecResult, OutputCallback
from harbor.environments.docker.docker import DockerEnvironment
from harbor.environments.docker.docker_unix import UnixOps


MIB = 1024 * 1024
HASH = re.compile(r"^[0-9a-f]{64}$")
SCHEMA = "gptgrep.harbor-proc-environment.v1"
SOURCE_MODULES = {
    "docker": "harbor.environments.docker.docker",
    "unix": "harbor.environments.docker.docker_unix",
    "runtime": "harbor.environments.docker.runtime",
    "factory": "harbor.environments.factory",
    "config": "harbor.models.trial.config",
}
CONTRACT_FIELDS = {"schema", "module", "harborSources", "sessionDerivation",
                   "environmentDirectory", "composeProfile", "engineSocket", "engineBinary",
                   "composePlugin", "engineClientConfigRoot", "createRequest"}
CONFIG_DEFAULTS = {"Hostname": "", "Domainname": "", "AttachStdin": False,
                   "AttachStdout": True, "AttachStderr": True, "Tty": False,
                   "OpenStdin": False, "StdinOnce": False, "Volumes": None,
                   "Entrypoint": ["/usr/bin/sleep"], "Cmd": ["infinity"]}
HOST_DEFAULTS = {
    "AutoRemove": False, "Binds": None, "BlkioDeviceReadBps": [], "BlkioDeviceReadIOps": [],
    "BlkioDeviceWriteBps": [], "BlkioDeviceWriteIOps": [], "BlkioWeight": 0,
    "BlkioWeightDevice": [], "Cgroup": "", "ConsoleSize": [0, 0], "ContainerIDFile": "",
    "CpuCount": 0, "CpuPercent": 0, "CpuPeriod": 0, "CpuQuota": 0, "CpuRealtimePeriod": 0,
    "CpuRealtimeRuntime": 0, "CpuShares": 0, "CpusetCpus": "", "CpusetMems": "",
    "DeviceCgroupRules": None, "DeviceRequests": None, "Devices": [], "Dns": None,
    "DnsOptions": [], "DnsSearch": [], "ExtraHosts": None, "GroupAdd": None,
    "IOMaximumBandwidth": 0, "IOMaximumIOps": 0, "Isolation": "", "Links": None,
    "MemoryReservation": 0, "MemorySwappiness": None, "OomKillDisable": None,
    "OomScoreAdj": 0, "PortBindings": {}, "PublishAllPorts": False, "Runtime": "runc",
    "UTSMode": "", "Ulimits": [], "VolumeDriver": "", "VolumesFrom": None,
    "ReadonlyRootfs": True, "Privileged": False, "CapDrop": ["ALL"], "IpcMode": "private",
    "CgroupnsMode": "private", "UsernsMode": "host", "PidMode": "", "NetworkMode": "none",
    "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
    "LogConfig": {"Type": "none", "Config": {}},
    "MaskedPaths": ["/sys/devices/virtual/powercap", "/sys/firmware"], "ReadonlyPaths": [],
}
HOST_VARIABLES = {"CapAdd", "CgroupParent", "Memory", "MemorySwap", "Mounts", "NanoCpus",
                  "PidsLimit", "SecurityOpt", "ShmSize"}


class ProcEnvironmentError(ValueError):
    """A policy denial must not trigger Harbor's RuntimeError copy fallback."""


def _need(value: Any, message: str) -> None:
    if not value:
        raise ProcEnvironmentError(message)


def _canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode()


def _same(actual: Any, expected: Any, message: str) -> None:
    _need(_canonical(actual) == _canonical(expected), message)


def _path(value: Any) -> str:
    _need(isinstance(value, str) and value.startswith("/") and value != "/"
          and not any(c in value for c in "\0\r\n\\")
          and str(PurePosixPath(value)) == value and ".." not in value.split("/"),
          "An absolute canonical path is required")
    return value


def _physical(value: Any) -> Path:
    path = Path(_path(value))
    _need(path.resolve() == path, "A physical path without symlink components is required")
    return path


def _pin(value: Any) -> dict[str, str]:
    _need(isinstance(value, dict) and set(value) == {"path", "sha256"}
          and isinstance(value["sha256"], str) and bool(HASH.fullmatch(value["sha256"])),
          "An exact file pin is required")
    _path(value["path"])
    return value


def _verify_pin(value: Any) -> Path:
    pin = _pin(value)
    path = _physical(pin["path"])
    _need(path.name != "auth.json" and path.name != ".env" and not path.name.startswith(".env.")
          and path.is_file() and not path.is_symlink()
          and path.stat().st_size <= 256 * MIB, "A bounded noncredential pinned file is required")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(MIB), b""):
            digest.update(chunk)
    _need(digest.hexdigest() == pin["sha256"], "A pinned source or executable changed")
    return path


def _decode(raw: bytes) -> Any:
    def pairs(items: list[tuple[str, Any]]) -> dict[str, Any]:
        result = {}
        for key, value in items:
            _need(key not in result, "Duplicate JSON key")
            result[key] = value
        return result

    def nonfinite(_value: str) -> Any:
        raise ProcEnvironmentError("Nonfinite JSON number")

    return json.loads(raw, object_pairs_hook=pairs, parse_constant=nonfinite)


def _project(session: str) -> str:
    value = session.lower()
    if not re.match(r"^[a-z0-9]", value):
        value = "0" + value
    return re.sub(r"[^a-z0-9_-]", "-", value)


def validate_contract(contract: Any) -> None:
    """Validate the closed create vocabulary without reading files or using Docker."""
    _need(isinstance(contract, dict) and set(contract) == CONTRACT_FIELDS
          and contract["schema"] == SCHEMA, "Unknown proc environment contract")
    for key in ("module", "composeProfile", "engineBinary", "composePlugin"):
        _pin(contract[key])
    sources = contract["harborSources"]
    _need(isinstance(sources, dict) and set(sources) == set(SOURCE_MODULES),
          "Exact supporting Harbor source pins are required")
    for value in sources.values():
        _pin(value)
    derivation = contract["sessionDerivation"]
    _need(isinstance(derivation, dict) and set(derivation) == {"rule", "source"}
          and derivation["rule"] == "harbor-trial-environment-v1", "The pinned Harbor session derivation is required")
    _pin(derivation["source"])
    _path(contract["environmentDirectory"])
    _path(contract["engineClientConfigRoot"])
    endpoint = contract["engineSocket"]
    _need(isinstance(endpoint, dict) and set(endpoint) == {
        "path", "identity", "caller_uid", "caller_gid", "required_group", "type"}
        and endpoint["type"] == "socket", "An exact local Engine socket is required")
    _path(endpoint["path"])
    identity = endpoint["identity"]
    _need(isinstance(identity, dict) and set(identity) == {"device", "inode", "uid", "gid", "mode"}
          and identity["uid"] == 0 and type(identity["uid"]) is int
          and type(identity["gid"]) is int and 0 <= identity["gid"] <= 2147483647
          and isinstance(identity["mode"], str) and re.fullmatch(r"0[0-7]{3}", identity["mode"])
          and all(isinstance(identity[key], str) and re.fullmatch(r"0|[1-9][0-9]{0,19}", identity[key])
                  for key in ("device", "inode"))
          and all(type(endpoint[key]) is int and 1 <= endpoint[key] <= 2147483647
                  for key in ("caller_uid", "caller_gid", "required_group")),
          "The rootful Engine socket identity or caller declaration differs")
    body = contract["createRequest"]
    _need(isinstance(body, dict) and set(body) == set(CONFIG_DEFAULTS) | {
        "Image", "User", "WorkingDir", "Env", "Labels", "HostConfig"}
        and len(_canonical(body)) <= 64 * 1024, "An exact bounded Engine create body is required")
    for key, expected in CONFIG_DEFAULTS.items():
        _same(body[key], expected, "The fixed keepalive container configuration differs")
    _need(isinstance(body["Image"], str) and re.fullmatch(r"sha256:[0-9a-f]{64}", body["Image"])
          and isinstance(body["User"], str) and re.fullmatch(r"[1-9][0-9]{0,9}:[1-9][0-9]{0,9}", body["User"]),
          "An immutable image ID and nonroot numeric user are required")
    _path(body["WorkingDir"])
    _need(isinstance(body["Env"], list) and len(body["Env"]) <= 64
          and all(isinstance(item, str) and len(item) <= 4096 and "=" in item
                  and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", item.split("=", 1)[0])
                  and not any(c in item for c in "\0\r\n") for item in body["Env"])
          and len({item.split("=", 1)[0] for item in body["Env"]}) == len(body["Env"]),
          "The create environment must be a bounded unique explicit list")
    labels = body["Labels"]
    _need(isinstance(labels, dict) and len(labels) <= 32
          and all(isinstance(key, str) and isinstance(value, str) and 0 < len(key) <= 256
                  and len(value) <= 4096 and not any(c in key + value for c in "\0\r\n")
                  for key, value in labels.items()), "The create labels are malformed")
    for key, expected in {"project": "@HARBOR_PROJECT@", "service": "main", "container-number": "1",
                          "oneoff": "False"}.items():
        _need(labels.get("com.docker.compose." + key) == expected, "The Compose selection labels differ")
    _need(bool(HASH.fullmatch(labels.get("com.docker.compose.config-hash", ""))),
          "Compose requires the exact pinned configuration-hash label")
    _need(_canonical(body).count(b"@HARBOR_PROJECT@") == 1, "Only the runtime project label may use a substitution")
    host = body["HostConfig"]
    _need(isinstance(host, dict) and set(host) in (set(HOST_DEFAULTS) | HOST_VARIABLES,
          set(HOST_DEFAULTS) | HOST_VARIABLES | {"Tmpfs"}), "Unknown Engine HostConfig field")
    for key, expected in HOST_DEFAULTS.items():
        _same(host[key], expected, "A fixed Engine namespace or security control differs")
    _need(host["CapAdd"] in (None, []) and isinstance(host["CgroupParent"], str)
          and (host["CgroupParent"] == "" or re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,90}\.slice", host["CgroupParent"])),
          "Unreviewed capability or cgroup parent override")
    for key, maximum in (("Memory", 1 << 50), ("MemorySwap", 1 << 50),
                         ("NanoCpus", 64_000_000_000), ("PidsLimit", 4096), ("ShmSize", 1 << 30)):
        _need(type(host[key]) is int and 1 <= host[key] <= maximum, "A finite container resource limit is required")
    _need(host["Memory"] == host["MemorySwap"], "This environment requires disabled container swap")
    options = host["SecurityOpt"]
    _need(isinstance(options, list) and len(options) == 3 and options[0] == "no-new-privileges:true"
          and isinstance(options[1], str) and options[1].startswith("seccomp={")
          and isinstance(_decode(options[1][8:].encode()), dict)
          and isinstance(options[2], str) and re.fullmatch(r"apparmor=[a-zA-Z0-9_.-]{1,128}", options[2]),
          "An explicit finite seccomp and AppArmor profile is required")
    mounts = host["Mounts"]
    _need(isinstance(mounts, list) and 1 <= len(mounts) <= 32, "An exact explicit bind set is required")
    targets = set()
    for row in mounts:
        _need(isinstance(row, dict) and set(row) in ({"Type", "Source", "Target", "BindOptions"},
              {"Type", "Source", "Target", "BindOptions", "ReadOnly"})
              and row["Type"] == "bind" and row["BindOptions"] == {"Propagation": "rprivate"}
              and type(row.get("ReadOnly", False)) is bool, "Only explicit private binds are admitted")
        _path(row["Source"])
        _path(row["Target"])
        _need(row["Target"] not in targets, "A duplicate mount target is forbidden")
        targets.add(row["Target"])
    tmpfs = host.get("Tmpfs", {})
    _need(isinstance(tmpfs, dict) and len(tmpfs) <= 14
          and all(_path(target) and isinstance(options, str) and len(options) <= 1024
                  and not any(c in options for c in "\0\r\n") for target, options in tmpfs.items()),
          "A bounded explicit tmpfs set is required")


def compact_json_profile(raw: bytes) -> str:
    """Remove only JSON whitespace outside strings, preserving all source tokens."""
    _need(isinstance(raw, bytes) and len(raw) <= MIB and isinstance(_decode(raw), dict),
          "A bounded pinned JSON security profile is required")
    quoted = escaped = False
    result = bytearray()
    for value in raw:
        if quoted or value not in b" \t\n\r":
            result.append(value)
        if quoted:
            if escaped:
                escaped = False
            elif value == 92:
                escaped = True
            elif value == 34:
                quoted = False
        elif value == 34:
            quoted = True
    return result.decode("utf-8")


def validate_owner_template(contract: Any, template: Any, seccomp_profile: bytes | None = None) -> None:
    """Pure comparison; callers supply the separately pinned raw seccomp bytes."""
    validate_contract(contract)
    body, engine, policy = contract["createRequest"], template["engine"], template["container_policy"]
    for field, source in (("engineSocket", "socket"), ("engineBinary", "binary"),
                          ("composePlugin", "compose_plugin"), ("engineClientConfigRoot", "client_config_root")):
        _same(contract[field], engine[source], "The create contract selects another Engine")
    _same(contract["composeProfile"], template["compose_profile"], "The Compose profile differs from its owner")
    _same(contract["harborSources"]["docker"], template["harbor_source"], "The Harbor source differs from its owner")
    _same(contract["sessionDerivation"]["source"], template["harbor_derivation_source"], "The Harbor session derivation differs")
    _need(body["Image"] == template["image_id"] == template["image_pin"]
          and body["User"] == f'{template["reader_uid"]}:{template["reader_gid"]}',
          "The create image or user differs from the owner template")
    _need(any(row["path"] == body["WorkingDir"] and row["kind"] == "directory" for row in template["allowed_paths"]),
          "The container working directory is absent from the owner allowlist")
    expected_mounts = []
    for row in template["mounts"]:
        _need(row["propagation"] == "rprivate" and row["create_host_path"] is False,
              "The owner mount permits unreviewed propagation or host path creation")
        expected_mounts.append({"Type": "bind", "Source": row["host_source"], "Target": row["guest_target"],
                                "ReadOnly": row["access"] == "read", "BindOptions": {"Propagation": "rprivate"}})
    actual_mounts = [{**row, "ReadOnly": row.get("ReadOnly", False)} for row in body["HostConfig"]["Mounts"]]
    _same(sorted(actual_mounts, key=lambda row: row["Target"]),
          sorted(expected_mounts, key=lambda row: row["Target"]), "The create bind set differs from the owner template")
    host = body["HostConfig"]
    mapping = {"ReadonlyRootfs": "readonly_rootfs", "Privileged": "privileged", "CapDrop": "cap_drop",
               "IpcMode": "ipc_mode", "CgroupnsMode": "cgroupns_mode", "NetworkMode": "network_mode",
               "SecurityOpt": "security_options", "MaskedPaths": "masked_paths", "ReadonlyPaths": "readonly_paths"}
    for key, field in mapping.items():
        _same(host[key], policy[field], "The create security controls differ from the owner policy")
    for key, field in (("Memory", "memory_bytes"), ("MemorySwap", "docker_memory_swap_bytes"),
                       ("NanoCpus", "nano_cpus"), ("PidsLimit", "pids_limit")):
        _need(str(host[key]) == policy[field], "The create resource limits differ from the owner policy")
    _need(policy["cap_add"] == [] and policy["no_new_privileges"] is True
          and policy["pid_mode"] == "private" and policy["userns_mode"] == "host-unmapped"
          and policy["restart_policy"] == "no" and policy["log_driver"] == "none"
          and policy["cgroup_memory_swap_max"] == "0" and policy["cpu_period"] == "100000"
          and policy["cpu_quota"] == str(host["NanoCpus"] // 10_000)
          and policy["seccomp_mode"] == "explicit-profile" and policy["seccomp_profile"] is not None,
          "The owner policy differs from the fixed Engine resource and namespace contract")
    _pin(policy["seccomp_profile"])
    _need(seccomp_profile is not None and host["SecurityOpt"][1] == "seccomp=" + compact_json_profile(seccomp_profile),
          "The inline Engine seccomp bytes differ from the separately pinned owner source")
    _need(host["SecurityOpt"][2] == "apparmor=" + policy["apparmor_profile"], "The AppArmor owner profile differs")
    tmpfs = policy["tmpfs"]
    _need(len({row["target"] for row in tmpfs}) == len(tmpfs)
          and {"/dev", "/dev/shm"} <= {row["target"] for row in tmpfs}, "The owner must declare built-in tmpfs bounds")
    _same(host.get("Tmpfs", {}), {row["target"]: ",".join(row["options"]) for row in tmpfs
                                  if row["target"] not in {"/dev", "/dev/shm"}}, "The create tmpfs set differs")
    _need(next(row["maximum_bytes"] for row in tmpfs if row["target"] == "/dev") == "67108864"
          and next(row["maximum_bytes"] for row in tmpfs if row["target"] == "/dev/shm") == str(host["ShmSize"]),
          "The built-in tmpfs limits differ from the create request")


class _ExactEngineConnection(http.client.HTTPConnection):
    def __init__(self, endpoint: dict[str, Any]) -> None:
        super().__init__("localhost", timeout=10)
        self.endpoint = endpoint

    def connect(self) -> None:
        _need(os.geteuid() == self.endpoint["caller_uid"] and os.getegid() == self.endpoint["caller_gid"]
              and self.endpoint["required_group"] in {os.getegid(), *os.getgroups()},
              "The Engine caller or selected socket group differs")
        path = _physical(self.endpoint["path"])
        before = path.lstat()
        actual = {"device": str(before.st_dev), "inode": str(before.st_ino), "uid": before.st_uid,
                  "gid": before.st_gid, "mode": f"{stat.S_IMODE(before.st_mode):04o}"}
        _same(actual, self.endpoint["identity"], "The local Engine socket identity changed")
        _need(stat.S_ISSOCK(before.st_mode), "The selected Engine endpoint is not a socket")
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        try:
            self.sock.connect(str(path))
            pid, uid, gid = struct.unpack("3i", self.sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            after = path.lstat()
            _need(pid > 0 and uid == gid == 0 and (before.st_dev, before.st_ino) == (after.st_dev, after.st_ino),
                  "The connected Engine peer or socket generation differs")
        except BaseException:
            self.sock.close()
            self.sock = None
            raise


class _OwnedUnixOps(UnixOps):
    async def _resolve_service_container(self, service: str) -> str:
        _need(service == "main", "Only the exact owned main service is admitted")
        await self._env._assert_owned()
        return self._env._container_id

    async def _engine_cp(self, source: str, destination: str) -> None:
        await self._env._assert_owned()
        prefix = self._env._container_id + ":"
        _need(sum(value.startswith(prefix) for value in (source, destination)) == 1,
              "The copy fallback must select the exact created container ID")
        await self._env._delegate_engine_cp(source, destination)


class ProcOverlayDockerEnvironment(DockerEnvironment):
    """Select with ``proc_environment:ProcOverlayDockerEnvironment`` explicitly."""

    @classmethod
    def preflight(cls) -> None:
        """The contract-bound Engine checks run in start, without a PATH-based CLI probe."""

    @classmethod
    def _egress_control_kernel_support(cls) -> bool:
        raise ProcEnvironmentError("This selected environment forbids Harbor sidecar or kernel-probe acquisition")

    def __init__(self, *args: Any, proc_contract_path: str, proc_contract_sha256: str, **kwargs: Any) -> None:
        self._contract_pin = {"path": proc_contract_path, "sha256": proc_contract_sha256}
        path = _verify_pin(self._contract_pin)
        _need(path.stat().st_size <= MIB, "The proc environment contract exceeds its byte bound")
        self._contract = _decode(path.read_bytes())
        validate_contract(self._contract)
        self._container_id: str | None = None
        self._creation_attempted = False
        self._start_called = False
        self._start_attempted = False
        self._running_verified = False
        self._daemon_checked = False
        self._image_labels: dict[str, str] | None = None
        self._generation: tuple[int, str] | None = None
        self._identity: dict[str, Any] | None = None
        self._cleanup_attempted = False
        self._cleanup_complete = False
        self._implicit_mounts_suppressed = False
        self._verify_sources()
        super().__init__(*args, **kwargs)
        self._platform = _OwnedUnixOps(self)
        self._suppress_implicit_mounts()
        self._validate_selection()
        self._create_request = copy.deepcopy(self._contract["createRequest"])
        self._create_request["Labels"]["com.docker.compose.project"] = _project(self.session_id)

    @property
    def proc_environment_identity(self) -> dict[str, Any]:
        _need(self._identity is not None, "The exact proc environment has not completed verified startup")
        return copy.deepcopy(self._identity)

    @property
    def proc_implicit_mounts_suppressed(self) -> bool:
        return self._implicit_mounts_suppressed

    def _suppress_implicit_mounts(self) -> None:
        # The pinned Trial API always supplies these three log binds. Its
        # public config has no auto_mounting switch. Their random trial paths
        # cannot enter the frozen reader mount set; bridge outputs are already
        # projected from explicit owner binds. Reject every additional mount.
        expected = []
        for source, target in ((self.trial_paths.verifier_dir, "/logs/verifier"),
                               (self.trial_paths.agent_dir, "/logs/agent"),
                               (self.trial_paths.artifacts_dir / "logs" / "artifacts", "/logs/artifacts")):
            path = _physical(str(source))
            _need(path.is_dir(), "A generated Harbor log directory is absent")
            expected.append({"type": "bind", "source": str(path), "target": target, "bind": {"selinux": "z"}})
        _same(self._mounts, expected, "Only the three exact generated Harbor log binds may be suppressed")
        self._mounts = []
        self._implicit_mounts_suppressed = True

    def _verify_sources(self) -> None:
        contract = self._contract
        _need(_verify_pin(contract["module"]) == Path(__file__).resolve(), "The imported proc environment module differs")
        _verify_pin(self._contract_pin)
        for key, name in SOURCE_MODULES.items():
            source = _verify_pin(contract["harborSources"][key])
            module = sys.modules.get(name)
            if module is not None:
                _need(Path(module.__file__).resolve() == source, "A loaded supporting Harbor module differs")
        source = _verify_pin(contract["sessionDerivation"]["source"])
        module = sys.modules.get("harbor.trial.trial")
        if module is not None:
            _need(Path(module.__file__).resolve() == source, "The running Harbor session derivation source differs")
        _need(Path(inspect.getfile(DockerEnvironment)).resolve() == Path(contract["harborSources"]["docker"]["path"])
              and Path(inspect.getfile(UnixOps)).resolve() == Path(contract["harborSources"]["unix"]["path"]),
              "The inherited Harbor implementation differs from its selected source")
        for key in ("composeProfile", "engineBinary", "composePlugin"):
            _verify_pin(contract[key])

    def _validate_selection(self) -> None:
        c = self._contract
        _need(_physical(c["engineClientConfigRoot"]).is_dir(), "The selected Engine client configuration root is absent or nonphysical")
        _need(isinstance(self.session_id, str) and self.session_id.endswith("__env") and 5 < len(self.session_id) <= 128
              and not any(value in self.session_id for value in "\0\r\n/\\")
              and str(_physical(str(self.environment_dir))) == c["environmentDirectory"]
              and [str(path) for path in self.extra_docker_compose_paths] == [c["composeProfile"]["path"]]
              and not self._environment_docker_compose_path.exists()
              and self.task_env_config.docker_image == c["createRequest"]["Image"]
              and not self._is_windows_container and not self._enable_egress_control
              and not self.stream_enabled and not self._mounts and not self._persistent_env
              and self.proc_implicit_mounts_suppressed is True
              and not self._enable_environment_dir_upload and not self._keep_containers
              and not self.task_env_config.env and self._effective_gpus == 0,
              "Only the pinned prebuilt, explicit-bind, no-upload Harbor route is admitted")

    def _request_sync(self, method: str, endpoint: str, body: Any = None) -> tuple[int, bytes]:
        connection = _ExactEngineConnection(self._contract["engineSocket"])
        try:
            raw = None if body is None else _canonical(body)
            connection.request(method, "/v1.52" + endpoint, body=raw,
                               headers={"Content-Type": "application/json"} if raw is not None else {})
            response = connection.getresponse()
            result = response.read(MIB + 1)
            _need(len(result) <= MIB, "The Engine metadata response exceeded its finite byte bound")
            return response.status, result
        finally:
            connection.close()

    async def _engine(self, method: str, endpoint: str, body: Any = None,
                      statuses: tuple[int, ...] = (200,)) -> Any:
        status, raw = await asyncio.to_thread(self._request_sync, method, endpoint, body)
        _need(status in statuses, "The exact local Engine operation failed or remains uncertain")
        return _decode(raw) if raw else None

    async def _project_ids(self) -> list[str]:
        query = urlencode({"all": "1", "filters": json.dumps({"label": [
            "com.docker.compose.project=" + _project(self.session_id)]}, separators=(",", ":"))})
        rows = await self._engine("GET", "/containers/json?" + query)
        _need(isinstance(rows, list) and len(rows) <= 1024
              and all(isinstance(row, dict) and isinstance(row.get("Id"), str)
                      and bool(HASH.fullmatch(row["Id"])) for row in rows), "Malformed project container inventory")
        return sorted(row["Id"] for row in rows)

    async def _fresh_project(self) -> None:
        _need(await self._project_ids() == [], "An existing Compose project cannot be adopted or removed")
        status, raw = await asyncio.to_thread(self._request_sync, "GET", "/containers/" + self._container_name + "/json")
        _need(status == 404, "The selected fresh container name is already present or uncertain")

    @property
    def _container_name(self) -> str:
        return _project(self.session_id) + "-main-1"

    async def _inspect_owned(self, *, running: bool | None = True) -> dict[str, Any]:
        _need(self._container_id is not None, "No complete exact container create identity is available")
        row = await self._engine("GET", "/containers/" + self._container_id + "/json")
        body = self._create_request
        expected = {key: value for key, value in body.items() if key != "HostConfig"}
        expected["Hostname"] = self._container_id[:12]
        _need(self._image_labels is not None, "The pinned image labels have not been inspected")
        expected["Labels"] = {**self._image_labels, **body["Labels"]}
        _need(isinstance(row, dict) and row.get("Id") == self._container_id
              and row.get("Name") == "/" + self._container_name and row.get("Image") == body["Image"]
              and type(row.get("RestartCount")) is int and row["RestartCount"] == 0,
              "The exact container identity or restart generation differs")
        _same(row.get("Config"), expected, "The inspected full container Config differs from its create request")
        host = copy.deepcopy(row.get("HostConfig"))
        _need(isinstance(host, dict) and isinstance(host.get("Mounts"), list)
              and all(isinstance(mount, dict) for mount in host["Mounts"]), "Malformed inspected HostConfig mounts")
        expected_host = copy.deepcopy(body["HostConfig"])
        # Engine omits false bind flags and serializes the default OOM flag as
        # false. Admit only these field-specific representations.
        for mount in [*host["Mounts"], *expected_host["Mounts"]]:
            mount.setdefault("ReadOnly", False)
        if expected_host["OomKillDisable"] is None and host.get("OomKillDisable") is False:
            host["OomKillDisable"] = None
        _same(host, expected_host, "The inspected full HostConfig differs from its create request")
        _need(row.get("AppArmorProfile") == body["HostConfig"]["SecurityOpt"][2][9:], "The active AppArmor profile differs")
        state = row.get("State")
        _need(isinstance(state, dict) and type(state.get("Running")) is bool
              and state.get("Paused") is False and state.get("Restarting") is False
              and state.get("Dead") is False and state.get("Error") == ""
              and (running is None or state["Running"] is running), "The owned container state differs or is uncertain")
        if state["Running"]:
            _need(type(state.get("Pid")) is int and state["Pid"] > 0
                  and isinstance(state.get("StartedAt"), str) and bool(state["StartedAt"]), "A running init generation is required")
            if self._generation is not None:
                _same([state["Pid"], state["StartedAt"]], list(self._generation), "The created init generation changed")
        elif self._generation is not None:
            _need(state.get("Pid") == 0 and state.get("StartedAt") == self._generation[1], "The terminal container generation differs")
        binds = []
        mounts = row.get("Mounts")
        _need(isinstance(mounts, list), "The actual mount inventory is unavailable")
        for item in mounts:
            _need(isinstance(item, dict), "Malformed actual container mount")
            if item.get("Type") == "bind":
                _need(type(item.get("RW")) is bool, "The actual bind access flag is unavailable")
                binds.append({"Source": item.get("Source"), "Target": item.get("Destination"),
                              "ReadOnly": item.get("RW") is False, "Propagation": item.get("Propagation")})
            else:
                _need(item.get("Type") == "tmpfs" and item.get("Destination") in body["HostConfig"].get("Tmpfs", {}),
                      "An unreviewed non-bind mount is present")
        expected_binds = [{"Source": item["Source"], "Target": item["Target"], "ReadOnly": item.get("ReadOnly", False),
                           "Propagation": "rprivate"} for item in body["HostConfig"]["Mounts"]]
        _same(sorted(binds, key=lambda item: item["Target"]), sorted(expected_binds, key=lambda item: item["Target"]),
              "The actual bind inventory differs from the exact create request")
        return row

    async def _assert_owned(self) -> dict[str, Any]:
        _need(self._running_verified and not self._cleanup_attempted, "The created container is not available for delegated operations")
        _need(await self._project_ids() == [self._container_id], "Compose selection is missing, foreign or ambiguous")
        return await self._inspect_owned()

    async def start(self, force_build: bool) -> None:
        _need(force_build is False and not self._start_called, "Only one prebuilt startup is admitted; no build or retry")
        self._start_called = True
        self._validate_selection()
        self._verify_sources()
        for mount in self._contract["createRequest"]["HostConfig"]["Mounts"]:
            _need(_physical(mount["Source"]).exists(), "An explicit bind source is absent")
        info = await self._engine("GET", "/info")
        _need(isinstance(info, dict) and info.get("OSType") == "linux"
              and isinstance(info.get("SecurityOptions"), list)
              and not any("rootless" in option for option in info["SecurityOptions"]), "The fixed Linux rootful Engine is required")
        self._daemon_checked = True
        await super().start(force_build=False)
        await self._assert_owned()
        self._identity = {"contract_pin": copy.deepcopy(self._contract_pin), "container_id": self._container_id,
                          "session_id": self.session_id,
                          "request_sha256": hashlib.sha256(_canonical(self._create_request)).hexdigest()}

    def _validate_daemon_mode(self) -> None:
        _need(self._daemon_checked, "The pinned local Engine has not passed its Linux mode check")

    async def _validate_image_os(self, image_name: str) -> None:
        _need(image_name == self._contract["createRequest"]["Image"], "Another image is not admitted")
        row = await self._engine("GET", "/images/" + image_name + "/json")
        _need(isinstance(row, dict) and row.get("Id") == image_name and row.get("Os") == "linux",
              "The pinned local prebuilt Linux image is unavailable")
        config = row.get("Config")
        _need(isinstance(config, dict), "The pinned image configuration is unavailable")
        labels = config.get("Labels")
        _need(labels is None or (isinstance(labels, dict) and len(labels) <= 32
              and all(isinstance(key, str) and isinstance(value, str) and 0 < len(key) <= 256
                      and len(value) <= 4096 and not any(c in key + value for c in "\0\r\n")
                      for key, value in labels.items())), "The pinned image labels are malformed")
        self._image_labels = copy.deepcopy(labels or {})

    async def _is_rootless_docker(self) -> bool:
        _need(self._daemon_checked, "The pinned Engine mode is not known")
        return False

    async def _create_once(self) -> None:
        _need(self._start_called and self._daemon_checked and self._use_prebuilt is True
              and not self._creation_attempted and self._container_id is None, "An uncertain create cannot be retried or adopted")
        await self._fresh_project()
        self._verify_sources()
        self._creation_attempted = True
        result = await self._engine("POST", "/containers/create?" + urlencode({"name": self._container_name}),
                                    self._create_request, statuses=(201,))
        _need(isinstance(result, dict) and set(result) == {"Id", "Warnings"}
              and isinstance(result["Id"], str) and bool(HASH.fullmatch(result["Id"]))
              and result["Warnings"] in (None, []), "The complete created container identity is unavailable")
        self._container_id = result["Id"]
        await self._inspect_owned(running=False)
        _need(await self._project_ids() == [self._container_id], "The new Compose project is not a singleton")
        self._start_attempted = True
        await self._engine("POST", "/containers/" + self._container_id + "/start", statuses=(204,))
        row = await self._inspect_owned()
        self._generation = row["State"]["Pid"], row["State"]["StartedAt"]
        self._running_verified = True

    def _client_env(self) -> dict[str, str]:
        _need(_physical(self._contract["engineClientConfigRoot"]).is_dir(),
              "The selected Engine client configuration root is absent or nonphysical")
        env = self._compose_env_vars(include_os_env=True)
        for key in ("DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH", "DOCKER_API_VERSION",
                    "DOCKER_CLI_PLUGIN_EXTRA_DIRS", "COMPOSE_FILE", "COMPOSE_PROJECT_NAME", "COMPOSE_PROFILES"):
            env.pop(key, None)
        env["DOCKER_HOST"] = "unix://" + self._contract["engineSocket"]["path"]
        env["DOCKER_CONFIG"] = self._contract["engineClientConfigRoot"]
        return env

    @staticmethod
    def _check_delegated(command: list[str]) -> None:
        _need(bool(command) and all(isinstance(value, str) and "\0" not in value for value in command), "Malformed Compose operation")
        if command[0] == "cp":
            _need(len(command) == 3 and all(not value.startswith("-") for value in command[1:])
                  and sum(value.startswith("main:") for value in command[1:]) == 1
                  and all(":" not in value or value.startswith("main:/") for value in command[1:]),
                  "Only one main-service copy is admitted")
            return
        _need(command[0] == "exec", "Only inherited main exec and copy operations are admitted")
        position = 1
        while position < len(command) and command[position].startswith("-"):
            option = command[position]
            _need(option in {"-T", "--no-TTY", "-w", "-e", "-u"}, "Unreviewed Compose exec selection option")
            position += 1 if option in {"-T", "--no-TTY"} else 2
        _need(position < len(command) - 1 and command[position] == "main", "Only the exact main exec service is admitted")

    async def _delegate_compose(self, command: list[str], check: bool, timeout_sec: int | None,
                                stdin_data: bytes | None, on_output: OutputCallback | None) -> ExecResult:
        self._verify_sources()
        argv = [self._contract["composePlugin"]["path"], "--host", "unix://" + self._contract["engineSocket"]["path"],
                "--project-name", _project(self.session_id),
                "--project-directory", self._contract["environmentDirectory"]]
        for path in self._docker_compose_paths:
            argv.extend(["-f", str(_physical(str(path)))])
        process = await asyncio.create_subprocess_exec(
            *argv, *command, env=self._client_env(), cwd=self._contract["environmentDirectory"],
            stdin=asyncio.subprocess.PIPE if stdin_data is not None else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
        try:
            if on_output is None:
                result = await self._collect_buffered_output(process, timeout_sec=timeout_sec, stdin_data=stdin_data)
            else:
                result = await self._collect_streamed_output(process, timeout_sec=timeout_sec,
                                                            stdin_data=stdin_data, on_output=on_output)
        except BaseException:
            await self._terminate_process(process)
            raise
        if check and result.return_code != 0:
            raise RuntimeError("The pinned Compose operation failed")
        return result

    async def _delegate_engine_cp(self, source: str, destination: str) -> None:
        self._verify_sources()
        process = await asyncio.create_subprocess_exec(
            self._contract["engineBinary"]["path"], "--host", "unix://" + self._contract["engineSocket"]["path"],
            "--config", self._contract["engineClientConfigRoot"], "cp", source, destination,
            env=self._client_env(), stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
        try:
            result = await self._collect_buffered_output(process, timeout_sec=120, stdin_data=None)
        except BaseException:
            await self._terminate_process(process)
            raise
        if result.return_code != 0:
            raise RuntimeError("The exact-ID Engine copy failed")

    async def _cleanup_owned(self) -> None:
        _need(not self._cleanup_attempted, "An uncertain cleanup cannot be retried")
        if not self._creation_attempted:
            self._cleanup_complete = True
            return
        _need(self._container_id is not None, "Create identity remains uncertain; retain the project for owner reconciliation")
        _need(await self._project_ids() == [self._container_id], "Cleanup cannot select an ambiguous or foreign project")
        row = await self._inspect_owned(running=None)
        self._cleanup_attempted = True
        if row["State"]["Running"]:
            await self._engine("POST", "/containers/" + self._container_id + "/stop?t=10", statuses=(204, 304))
        await self._inspect_owned(running=False)
        await self._engine("DELETE", "/containers/" + self._container_id + "?force=false&v=false", statuses=(204,))
        status, _raw = await asyncio.to_thread(self._request_sync, "GET", "/containers/" + self._container_id + "/json")
        _need(status == 404 and await self._project_ids() == [], "Exact-ID container absence is not proved")
        self._cleanup_complete = True

    async def _run_docker_compose_command(self, command: list[str], check: bool = True,
                                          timeout_sec: int | None = None, stdin_data: bytes | None = None,
                                          on_output: OutputCallback | None = None) -> ExecResult:
        if command == ["down", "--remove-orphans"] and not self._creation_attempted:
            await self._fresh_project()
            return ExecResult(return_code=0)
        if command == ["up", "--detach", "--wait"]:
            _need(stdin_data is None and on_output is None, "Startup does not accept alternate streams")
            await self._create_once()
            return ExecResult(return_code=0)
        if command in (["down"], ["down", "--rmi", "local", "--volumes", "--remove-orphans"]):
            await self._cleanup_owned()
            return ExecResult(return_code=0)
        self._check_delegated(command)
        await self._assert_owned()
        return await self._delegate_compose(command, check, timeout_sec, stdin_data, on_output)

    async def stop(self, delete: bool) -> None:
        await super().stop(delete)
        _need(self._cleanup_complete, "Harbor cleanup did not prove exact-ID terminal removal; owner reconciliation is required")
