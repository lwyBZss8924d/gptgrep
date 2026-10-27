"""Source-only adapter tests with stubbed Harbor and an in-memory Engine.

These exercise selection, uncertainty, exact-ID transfer and cleanup. They do
not contact Docker, launch a process, load a profile, or qualify a live reader.
"""

import asyncio
import contextlib
import copy
import hashlib
import importlib
import importlib.util
import json
import os
import stat
import struct
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch


class StubResult:
    def __init__(self, return_code, stdout=None, stderr=None):
        self.return_code = return_code
        self.stdout = stdout
        self.stderr = stderr


class StubUnixOps:
    def __init__(self, env):
        self._env = env

    async def download_file(self, source, target, service=None):
        service = service or "main"
        try:
            await self._env._run_docker_compose_command(["cp", f"{service}:{source}", str(target)])
            return
        except RuntimeError:
            container = await self._resolve_service_container(service)
            await self._engine_cp(f"{container}:{source}", str(target))


class StubDockerEnvironment:
    def __init__(self, environment_dir, environment_name, session_id, trial_paths, task_env_config,
                 extra_docker_compose, mounts=None, persistent_env=None, **kwargs):
        self.environment_dir = environment_dir
        self.environment_name = environment_name
        self.session_id = session_id
        self.trial_paths = trial_paths
        self.task_env_config = task_env_config
        self.extra_docker_compose_paths = [Path(path) for path in extra_docker_compose]
        self._environment_docker_compose_path = environment_dir / "docker-compose.yaml"
        self._is_windows_container = False
        self._enable_egress_control = False
        self.stream_enabled = False
        self._mounts = copy.deepcopy(mounts or [])
        self._persistent_env = persistent_env or {}
        self._enable_environment_dir_upload = False
        self._keep_containers = False
        self._effective_gpus = 0
        self._use_prebuilt = False
        self._docker_compose_paths = self.extra_docker_compose_paths

    async def start(self, force_build):
        self._use_prebuilt = True
        self._validate_daemon_mode()
        await self._validate_image_os(self.task_env_config.docker_image)
        try:
            await self._run_docker_compose_command(["down", "--remove-orphans"])
        except RuntimeError:
            pass
        await self._run_docker_compose_command(["up", "--detach", "--wait"])
        await self._run_docker_compose_command(["exec", "-T", "-u", "root", "main", "mkdir", "-p", "/tmp"])

    async def stop(self, delete):
        # Match Harbor's best-effort swallowing: the adapter must still expose
        # a failed exact-ID cleanup to its caller.
        try:
            await self._run_docker_compose_command(
                ["down", "--rmi", "local", "--volumes", "--remove-orphans"] if delete else ["down"])
        except Exception as error:
            self.swallowed_stop_error = error

    def _compose_env_vars(self, include_os_env):
        return {"DOCKER_HOST": "tcp://foreign:2375", "DOCKER_CONTEXT": "foreign",
                "DOCKER_CONFIG": "/foreign", "DOCKER_API_VERSION": "1.0", "COMPOSE_FILE": "/foreign.yaml",
                "PATH": "/foreign", "KEEP_UNIT_VALUE": "retained"}


def load_adapter():
    modules = {}
    for name in ("harbor", "harbor.environments", "harbor.environments.docker"):
        module = types.ModuleType(name)
        module.__path__ = []
        modules[name] = module
    for name, attributes in {
        "harbor.environments.base": {"ExecResult": StubResult, "OutputCallback": object},
        "harbor.environments.docker.docker": {"DockerEnvironment": StubDockerEnvironment},
        "harbor.environments.docker.docker_unix": {"UnixOps": StubUnixOps},
    }.items():
        module = types.ModuleType(name)
        module.__dict__.update(attributes)
        modules[name] = module
    path = Path(__file__).with_name("proc_environment.py")
    spec = importlib.util.spec_from_file_location("unit_proc_environment", path)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, modules):
        spec.loader.exec_module(module)
    return module


adapter = load_adapter()
CID = "a" * 64
FOREIGN_CID = "b" * 64


def file_pin(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    if not isinstance(data, bytes):
        data = json.dumps(data).encode()
    path.write_bytes(data)
    return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest()}


def fixture(root):
    environment = root / "task-environment"
    environment.mkdir()
    bind = root / "bind"
    bind.mkdir()
    config_root = root / "docker-client"
    config_root.mkdir()
    sources = {key: file_pin(root / "sources" / f"{key}.py", (key + "\n").encode()) for key in adapter.SOURCE_MODULES}
    profile_raw = b'{\n "defaultAction": "SCMP_ACT_ERRNO", "syscalls": [], "note": "keep  spaces"\n}\n'
    body = {**copy.deepcopy(adapter.CONFIG_DEFAULTS), "Image": "sha256:" + "c" * 64,
            "User": "1000:1000", "WorkingDir": "/work", "Env": ["PATH=/usr/bin:/bin"],
            "Labels": {"com.docker.compose.project": "@HARBOR_PROJECT@", "com.docker.compose.service": "main",
                       "com.docker.compose.container-number": "1", "com.docker.compose.oneoff": "False",
                       "com.docker.compose.config-hash": "d" * 64},
            "HostConfig": {**copy.deepcopy(adapter.HOST_DEFAULTS), "CapAdd": None, "CgroupParent": "",
                           "Memory": 4294967296, "MemorySwap": 4294967296, "NanoCpus": 2000000000,
                           "PidsLimit": 256, "ShmSize": 67108864,
                           "SecurityOpt": ["no-new-privileges:true", "seccomp=" + adapter.compact_json_profile(profile_raw),
                                           "apparmor=unit-native"],
                           "Mounts": [{"Type": "bind", "Source": str(bind), "Target": "/work",
                                       "BindOptions": {"Propagation": "rprivate"}}]}}
    contract = {"schema": adapter.SCHEMA, "module": file_pin(root / "module.py", b"unit module pin\n"),
                "harborSources": sources,
                "sessionDerivation": {"rule": "harbor-trial-environment-v1",
                                      "source": file_pin(root / "sources" / "trial.py", b"unit trial derivation\n")},
                "environmentDirectory": str(environment),
                "composeProfile": file_pin(root / "compose-profile.json", {"services": {"main": {}}}),
                "engineBinary": file_pin(root / "docker", b"unit engine executable\n"),
                "composePlugin": file_pin(root / "docker-compose", b"unit compose executable\n"),
                "engineClientConfigRoot": str(config_root),
                "engineSocket": {"path": str(root / "docker.sock"), "type": "socket", "caller_uid": 1000,
                                 "caller_gid": 1000, "required_group": 112,
                                 "identity": {"device": "1", "inode": "2", "uid": 0, "gid": 112, "mode": "0660"}},
                "createRequest": body}
    host = body["HostConfig"]
    policy = {"readonly_rootfs": True, "privileged": False, "no_new_privileges": True,
              "cap_add": [], "cap_drop": ["ALL"], "pid_mode": "private", "ipc_mode": "private",
              "userns_mode": "host-unmapped", "cgroupns_mode": "private", "restart_policy": "no",
              "network_mode": "none", "log_driver": "none", "seccomp_mode": "explicit-profile",
              "seccomp_profile": file_pin(root / "seccomp.json", profile_raw), "apparmor_profile": "unit-native",
              "security_options": copy.deepcopy(host["SecurityOpt"]), "memory_bytes": str(host["Memory"]),
              "docker_memory_swap_bytes": str(host["MemorySwap"]), "cgroup_memory_swap_max": "0",
              "nano_cpus": str(host["NanoCpus"]), "cpu_period": "100000", "cpu_quota": "200000", "pids_limit": "256",
              "masked_paths": copy.deepcopy(host["MaskedPaths"]), "readonly_paths": [],
              "tmpfs": [{"target": target, "maximum_bytes": "67108864", "options": ["rw", "nosuid", "size=65536k"],
                         "purpose": "explicit built-in limit"} for target in ("/dev", "/dev/shm")]}
    template = {"engine": {"binary": contract["engineBinary"], "compose_plugin": contract["composePlugin"],
                           "socket": contract["engineSocket"], "client_config_root": contract["engineClientConfigRoot"]},
                "harbor_source": sources["docker"], "harbor_derivation_source": contract["sessionDerivation"]["source"],
                "compose_profile": contract["composeProfile"], "image_id": body["Image"], "image_pin": body["Image"],
                "reader_uid": 1000, "reader_gid": 1000, "allowed_paths": [{"path": "/work", "kind": "directory"}],
                "mounts": [{"host_source": str(bind), "guest_target": "/work", "kind": "directory", "access": "write",
                            "propagation": "rprivate", "create_host_path": False}], "container_policy": policy}
    return contract, template, profile_raw


class HarnessEnvironment(adapter.ProcOverlayDockerEnvironment):
    """No process or daemon access; preserve adapter dispatch and inspect logic."""

    def _verify_sources(self):
        self.source_checks = getattr(self, "source_checks", 0) + 1

    def __init__(self, *args, **kwargs):
        self.calls = []
        self.delegated = []
        self.fallback = []
        self.server_created = False
        self.server_running = False
        self.removed = False
        self.foreign_project = False
        self.foreign_name = False
        self.fail_create = False
        self.fail_start = False
        self.fail_stop = False
        self.fail_cp = False
        self.host_mutation = None
        self.server_pid = 1234
        self.started_at = "2026-01-01T00:00:00.000000000Z"
        super().__init__(*args, **kwargs)

    def _request_sync(self, method, endpoint, body=None):
        self.calls.append((method, endpoint, copy.deepcopy(body)))
        if method == "GET" and endpoint == "/containers/" + self._container_name + "/json":
            return (200 if self.foreign_name else 404), b"{}"
        if method == "GET" and endpoint == "/containers/" + CID + "/json" and self.removed:
            return 404, b"{}"
        raise AssertionError("Unexpected direct Engine endpoint")

    def row(self):
        body = copy.deepcopy(self._create_request)
        config = {key: value for key, value in body.items() if key != "HostConfig"}
        config["Hostname"] = CID[:12]
        host = body["HostConfig"]
        if self.host_mutation is not None:
            host.update(self.host_mutation)
        return {"Id": CID, "Name": "/" + self._container_name, "Image": body["Image"], "RestartCount": 0,
                "Config": config, "HostConfig": host, "AppArmorProfile": "unit-native",
                "State": {"Running": self.server_running, "Paused": False, "Restarting": False, "Dead": False,
                          "Error": "", "Pid": self.server_pid if self.server_running else 0,
                          "StartedAt": self.started_at if self._start_attempted else "0001-01-01T00:00:00Z"},
                "Mounts": [{"Type": "bind", "Source": item["Source"], "Destination": item["Target"],
                            "RW": not item.get("ReadOnly", False), "Propagation": "rprivate"} for item in host["Mounts"]]}

    async def _engine(self, method, endpoint, body=None, statuses=(200,)):
        self.calls.append((method, endpoint, copy.deepcopy(body)))
        if method == "GET" and endpoint == "/info":
            return {"OSType": "linux", "SecurityOptions": ["name=seccomp", "name=apparmor"]}
        if method == "GET" and endpoint.startswith("/images/"):
            return {"Id": self._create_request["Image"], "Os": "linux"}
        if method == "GET" and endpoint.startswith("/containers/json?"):
            ids = [CID] if self.server_created and not self.removed else []
            return [{"Id": value} for value in ids + ([FOREIGN_CID] if self.foreign_project else [])]
        if method == "POST" and endpoint.startswith("/containers/create?"):
            self.server_created = True
            if self.fail_create:
                raise ConnectionError("lost create reply; identity is unknown")
            return {"Id": CID, "Warnings": []}
        if method == "GET" and endpoint == "/containers/" + CID + "/json":
            return self.row()
        if method == "POST" and endpoint == "/containers/" + CID + "/start":
            self.server_running = True
            if self.fail_start:
                raise ConnectionError("lost start reply; generation is uncertain")
            return None
        if method == "POST" and endpoint == "/containers/" + CID + "/stop?t=10":
            if self.fail_stop:
                raise ConnectionError("lost stop reply; terminal state is uncertain")
            self.server_running = False
            return None
        if method == "DELETE" and endpoint == "/containers/" + CID + "?force=false&v=false":
            self.removed = True
            return None
        raise AssertionError("Unexpected Engine endpoint")

    async def _delegate_compose(self, command, check, timeout_sec, stdin_data, on_output):
        self.delegated.append(copy.deepcopy(command))
        if command[0] == "cp" and self.fail_cp:
            raise RuntimeError("expected copy failure")
        return StubResult(return_code=0)

    async def _delegate_engine_cp(self, source, destination):
        self.fallback.append((source, destination))


class ContractTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.contract, self.template, self.profile = fixture(self.root)

    def test_owner_matches_only_exact_local_image_and_literal_profile(self):
        adapter.validate_owner_template(self.contract, self.template, self.profile)
        self.template["image_pin"] = "registry.example/unit@" + self.template["image_id"]
        with self.assertRaises(adapter.ProcEnvironmentError):
            adapter.validate_owner_template(self.contract, self.template, self.profile)

    def test_reordered_equivalent_seccomp_source_is_rejected(self):
        equivalent = b'{"note":"keep  spaces","syscalls":[],"defaultAction":"SCMP_ACT_ERRNO"}'
        self.assertEqual(json.loads(equivalent), json.loads(self.profile))
        with self.assertRaises(adapter.ProcEnvironmentError):
            adapter.validate_owner_template(self.contract, self.template, equivalent)

    def test_compact_profile_preserves_string_escapes_and_spaces(self):
        raw = b' { "text": "a  \\\" b", "value": [ 1, 2 ] }\n'
        self.assertEqual(adapter.compact_json_profile(raw), '{"text":"a  \\\" b","value":[1,2]}')
        for raw in (b'{"a":1,"a":2}', b'{"a":NaN}'):
            with self.subTest(raw=raw), self.assertRaises(adapter.ProcEnvironmentError):
                adapter.compact_json_profile(raw)

    def test_runtime_substitution_has_one_closed_location(self):
        self.contract["createRequest"]["Env"].append("UNIT=@HARBOR_PROJECT@")
        with self.assertRaises(adapter.ProcEnvironmentError):
            adapter.validate_contract(self.contract)

    def test_unknown_fields_and_weakened_security_fail_closed(self):
        mutations = [lambda c: c.update(extra=True),
                     lambda c: c["createRequest"].update(NetworkingConfig={}),
                     lambda c: c["createRequest"]["HostConfig"].update(Privileged=True),
                     lambda c: c["createRequest"]["HostConfig"].update(MaskedPaths=[]),
                     lambda c: c["createRequest"]["HostConfig"].update(Devices=[{"PathOnHost": "/dev/sda"}]),
                     lambda c: c["createRequest"]["HostConfig"].update(AutoRemove=True)]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                contract = copy.deepcopy(self.contract)
                mutate(contract)
                with self.assertRaises(adapter.ProcEnvironmentError):
                    adapter.validate_contract(contract)

    def test_owner_security_mount_and_engine_mismatches_are_rejected(self):
        mutations = [lambda t: t["engine"].update(client_config_root="/foreign"),
                     lambda t: t["mounts"][0].update(access="read"),
                     lambda t: t["container_policy"].update(memory_bytes="2147483648"),
                     lambda t: t["container_policy"].update(readonly_paths=["/proc/sys"]),
                     lambda t: t["container_policy"].update(security_options=["no-new-privileges:true"])]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                template = copy.deepcopy(self.template)
                mutate(template)
                with self.assertRaises(adapter.ProcEnvironmentError):
                    adapter.validate_owner_template(self.contract, template, self.profile)

    def test_source_hash_drift_and_credential_pin_are_denied(self):
        pin = file_pin(self.root / "source.py", b"first\n")
        adapter._verify_pin(pin)
        Path(pin["path"]).write_bytes(b"other\n")
        with self.assertRaises(adapter.ProcEnvironmentError):
            adapter._verify_pin(pin)
        for name in ("auth.json", ".env", ".env.unit"):
            pin = file_pin(self.root / name, b"unit bytes; not credential material")
            with self.subTest(name=name), self.assertRaises(adapter.ProcEnvironmentError):
                adapter._verify_pin(pin)

    def test_engine_socket_requires_exact_identity_and_root_peer(self):
        endpoint = self.contract["engineSocket"]
        node = types.SimpleNamespace(st_dev=1, st_ino=2, st_uid=0, st_gid=112,
                                     st_mode=stat.S_IFSOCK | 0o660)
        path = Mock()
        path.lstat.return_value = node
        client = Mock()
        client.getsockopt.return_value = struct.pack("3i", 555, 0, 0)
        with patch.object(adapter, "_physical", return_value=path), \
                patch.object(adapter.os, "geteuid", return_value=1000), \
                patch.object(adapter.os, "getegid", return_value=1000), \
                patch.object(adapter.os, "getgroups", return_value=[112]), \
                patch.object(adapter.socket, "SO_PEERCRED", 17, create=True), \
                patch.object(adapter.socket, "socket", return_value=client):
            connection = adapter._ExactEngineConnection(endpoint)
            connection.connect()
            self.assertIs(connection.sock, client)
            client.getsockopt.return_value = struct.pack("3i", 555, 1000, 1000)
            with self.assertRaises(adapter.ProcEnvironmentError):
                connection.connect()
            self.assertIsNone(connection.sock)
            client.close.assert_called_once()
            path.lstat.return_value = types.SimpleNamespace(**{**vars(node), "st_ino": 3})
            with self.assertRaises(adapter.ProcEnvironmentError):
                connection.connect()


class LifecycleTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.contract, self.template, self.profile = fixture(self.root)

    def environment(self, **kwargs):
        pin = file_pin(self.root / "proc-contract.json", self.contract)
        trial = self.root / "trial"
        paths = types.SimpleNamespace(verifier_dir=trial / "verifier", agent_dir=trial / "agent", artifacts_dir=trial / "artifacts")
        mounts = []
        for source, target in ((paths.verifier_dir, "/logs/verifier"), (paths.agent_dir, "/logs/agent"),
                               (paths.artifacts_dir / "logs" / "artifacts", "/logs/artifacts")):
            source.mkdir(parents=True, exist_ok=True)
            mounts.append({"type": "bind", "source": str(source), "target": target, "bind": {"selinux": "z"}})
        mounts = kwargs.pop("mounts", mounts)
        return HarnessEnvironment(environment_dir=Path(self.contract["environmentDirectory"]), environment_name="unit",
                                  session_id="Unit-Trial__Ab12Cd3__env", trial_paths=paths, mounts=mounts,
                                  task_env_config=types.SimpleNamespace(docker_image=self.contract["createRequest"]["Image"], env={}),
                                  extra_docker_compose=[self.contract["composeProfile"]["path"]],
                                  proc_contract_path=pin["path"], proc_contract_sha256=pin["sha256"], **kwargs)

    async def test_harbor_lifecycle_creates_once_and_binds_actual_session(self):
        env = self.environment()
        self.assertTrue(env.proc_implicit_mounts_suppressed)
        self.assertEqual(env._mounts, [])
        with self.assertRaises(adapter.ProcEnvironmentError):
            _ = env.proc_environment_identity
        await env.start(False)
        creates = [(endpoint, body) for method, endpoint, body in env.calls if method == "POST" and "/create?" in endpoint]
        self.assertEqual(len(creates), 1)
        self.assertEqual(creates[0][1]["Labels"]["com.docker.compose.project"], "unit-trial__ab12cd3__env")
        self.assertIn("name=unit-trial__ab12cd3__env-main-1", creates[0][0])
        identity = env.proc_environment_identity
        self.assertEqual(identity["container_id"], CID)
        self.assertEqual(identity["session_id"], env.session_id)
        self.assertEqual(identity["request_sha256"], hashlib.sha256(adapter._canonical(creates[0][1])).hexdigest())
        identity["container_id"] = FOREIGN_CID
        self.assertEqual(env.proc_environment_identity["container_id"], CID)
        with self.assertRaises(AttributeError):
            env.proc_environment_identity = {}
        with self.assertRaises(AttributeError):
            env.proc_implicit_mounts_suppressed = False
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)

    async def test_force_build_and_foreign_fresh_project_have_no_mutation(self):
        env = self.environment()
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(True)
        self.assertEqual(env.calls, [])
        env.foreign_project = True
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)
        self.assertFalse(any(method in {"POST", "DELETE"} for method, _endpoint, _body in env.calls))

    async def test_missing_or_additional_harbor_mounts_are_rejected(self):
        with self.assertRaises(adapter.ProcEnvironmentError):
            self.environment(mounts=[])
        env = self.environment()
        extra = [{"type": "bind", "source": str(self.root), "target": "/unreviewed"}]
        with self.assertRaises(adapter.ProcEnvironmentError):
            self.environment(mounts=extra)
        env._mounts.append(extra[0])
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)
        self.assertEqual(env.calls, [])

    async def test_existing_exact_name_is_not_adopted_or_removed(self):
        env = self.environment()
        env.foreign_name = True
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)
        self.assertFalse(any(method in {"POST", "DELETE"} for method, _endpoint, _body in env.calls))

    async def test_lost_create_reply_is_not_retried_adopted_or_cleaned_by_name(self):
        env = self.environment()
        env.fail_create = True
        with self.assertRaises(ConnectionError):
            await env.start(False)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.stop(True)
        self.assertIsNone(env._container_id)
        self.assertEqual(sum(method == "POST" and "/create?" in endpoint for method, endpoint, _body in env.calls), 1)
        self.assertFalse(any(method == "DELETE" or "/stop?" in endpoint for method, endpoint, _body in env.calls))

    async def test_lost_start_reply_is_not_replayed(self):
        env = self.environment()
        env.fail_start = True
        with self.assertRaises(ConnectionError):
            await env.start(False)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._run_docker_compose_command(["up", "--detach", "--wait"])
        self.assertEqual(sum(endpoint.endswith("/start") for _method, endpoint, _body in env.calls), 1)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._run_docker_compose_command(["exec", "main", "true"])
        self.assertEqual(env.delegated, [])
        with self.assertRaises(adapter.ProcEnvironmentError):
            _ = env.proc_environment_identity

    async def test_full_host_config_drift_denies_start_and_exec(self):
        env = self.environment()
        env.host_mutation = {"Memory": 2147483648}
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.start(False)
        self.assertFalse(any(endpoint.endswith("/start") for _method, endpoint, _body in env.calls))
        self.assertEqual(env.delegated, [])

    async def test_ambiguous_compose_selection_denies_copy_without_fallback(self):
        env = self.environment()
        await env.start(False)
        env.foreign_project = True
        count = len(env.delegated)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._platform.download_file("/work/file", self.root / "download")
        self.assertEqual(len(env.delegated), count)
        self.assertEqual(env.fallback, [])

    async def test_compose_copy_failure_uses_only_verified_full_id(self):
        env = self.environment()
        await env.start(False)
        env.fail_cp = True
        await env._platform.download_file("/work/file", self.root / "download")
        self.assertEqual(env.fallback, [(CID + ":/work/file", str(self.root / "download"))])
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._platform._resolve_service_container("sidecar")
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._platform._engine_cp(FOREIGN_CID + ":/work/file", str(self.root / "foreign"))

    async def test_init_generation_change_denies_delegated_operation(self):
        env = self.environment()
        await env.start(False)
        env.server_pid += 1
        count = len(env.delegated)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env._run_docker_compose_command(["exec", "main", "true"])
        self.assertEqual(len(env.delegated), count)

    async def test_unsafe_service_options_and_broad_commands_are_denied(self):
        env = self.environment()
        await env.start(False)
        for command in (["build"], ["pull"], ["up", "--detach"], ["exec", "--index", "2", "main", "true"],
                        ["exec", "sidecar", "true"], ["cp", "sidecar:/file", str(self.root / "file")]):
            with self.subTest(command=command), self.assertRaises(adapter.ProcEnvironmentError):
                await env._run_docker_compose_command(command)

    async def test_cleanup_removes_only_created_id_and_never_images_or_volumes(self):
        env = self.environment()
        await env.start(False)
        await env.stop(True)
        mutations = [(method, endpoint) for method, endpoint, _body in env.calls if method in {"POST", "DELETE"}]
        self.assertEqual(mutations[-2:], [("POST", "/containers/" + CID + "/stop?t=10"),
                                         ("DELETE", "/containers/" + CID + "?force=false&v=false")])
        self.assertTrue(env._cleanup_complete)
        self.assertFalse(any(command[0] in {"down", "stop"} for command in env.delegated))
        self.assertFalse(any(endpoint.startswith(("/images/", "/volumes", "/networks")) for _method, endpoint in mutations))

    async def test_failed_stop_is_exposed_and_retained_without_retry(self):
        env = self.environment()
        await env.start(False)
        env.fail_stop = True
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.stop(True)
        with self.assertRaises(adapter.ProcEnvironmentError):
            await env.stop(True)
        self.assertTrue(env.server_running)
        self.assertFalse(env.removed)
        self.assertFalse(env._cleanup_complete)
        self.assertEqual(sum("/stop?" in endpoint for _method, endpoint, _body in env.calls), 1)
        self.assertFalse(any(method == "DELETE" for method, _endpoint, _body in env.calls))

    async def test_client_endpoint_is_fixed_independently_of_parent_context(self):
        env = self.environment()
        client = env._client_env()
        self.assertEqual(client["DOCKER_HOST"], "unix://" + self.contract["engineSocket"]["path"])
        self.assertEqual(client["DOCKER_CONFIG"], self.contract["engineClientConfigRoot"])
        self.assertEqual(client["KEEP_UNIT_VALUE"], "retained")
        self.assertNotIn("DOCKER_CONTEXT", client)
        self.assertNotIn("DOCKER_API_VERSION", client)
        self.assertNotIn("COMPOSE_FILE", client)

    async def test_client_config_root_must_remain_a_physical_existing_directory(self):
        config = Path(self.contract["engineClientConfigRoot"])
        config.rmdir()
        with self.assertRaises(adapter.ProcEnvironmentError):
            self.environment()
        config.write_bytes(b"unit regular file\n")
        with self.assertRaises(adapter.ProcEnvironmentError):
            self.environment()
        config.unlink()
        config.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(adapter.ProcEnvironmentError):
            self.environment()
        config.unlink()
        config.mkdir()
        env = self.environment()
        config.rmdir()
        with self.assertRaises(adapter.ProcEnvironmentError):
            env._client_env()

    async def test_delegates_use_absolute_pinned_binaries_and_explicit_socket(self):
        env = self.environment()
        collect = AsyncMock(return_value=StubResult(return_code=0))
        with patch.object(adapter.asyncio, "create_subprocess_exec", new_callable=AsyncMock) as spawn, \
                patch.object(env, "_collect_buffered_output", collect, create=True):
            await adapter.ProcOverlayDockerEnvironment._delegate_compose(
                env, ["exec", "-T", "main", "true"], True, 5, None, None)
            args, options = spawn.call_args
            self.assertEqual(args[:3], (self.contract["composePlugin"]["path"], "--host",
                                        "unix://" + self.contract["engineSocket"]["path"]))
            self.assertEqual(options["env"]["DOCKER_CONFIG"], self.contract["engineClientConfigRoot"])
            self.assertEqual(args[-4:], ("exec", "-T", "main", "true"))
            await adapter.ProcOverlayDockerEnvironment._delegate_engine_cp(
                env, CID + ":/work/file", str(self.root / "download"))
            args, options = spawn.call_args
            self.assertEqual(args[:5], (self.contract["engineBinary"]["path"], "--host",
                                        "unix://" + self.contract["engineSocket"]["path"],
                                        "--config", self.contract["engineClientConfigRoot"]))
            self.assertEqual(args[5:], ("cp", CID + ":/work/file", str(self.root / "download")))

    async def test_delegated_collection_reaps_its_process_on_base_exception(self):
        env = self.environment()
        for mode in ("compose", "engine-copy"):
            for error_type in (asyncio.CancelledError, ValueError):
                process = object()
                with self.subTest(mode=mode, error=error_type), \
                        patch.object(adapter.asyncio, "create_subprocess_exec", new=AsyncMock(return_value=process)), \
                        patch.object(env, "_collect_buffered_output", new=AsyncMock(side_effect=error_type("unit interruption")), create=True), \
                        patch.object(env, "_terminate_process", new=AsyncMock(), create=True) as terminate:
                    with self.assertRaises(error_type):
                        if mode == "compose":
                            await adapter.ProcOverlayDockerEnvironment._delegate_compose(
                                env, ["exec", "-T", "main", "true"], True, 5, None, None)
                        else:
                            await adapter.ProcOverlayDockerEnvironment._delegate_engine_cp(
                                env, CID + ":/work/file", str(self.root / "download"))
                    terminate.assert_awaited_once_with(process)


# An explicit pinned installation opt-in keeps ordinary portable tests free of
# Harbor dependencies. A supplied but incomplete pin input must fail, not skip.
INSTALLED_MODULES = {
    **adapter.SOURCE_MODULES,
    "trial": "harbor.trial.trial", "base": "harbor.environments.base",
    "paths": "harbor.models.trial.paths", "task": "harbor.models.task.config",
    "definition": "harbor.environments.definition", "compose": "harbor.environments.docker",
    "artifacts": "harbor.trial.artifact_handler", "singleStep": "harbor.trial.single_step",
}


@unittest.skipUnless(os.environ.get("GPTGREP_HARBOR_PROC_TEST_PINS")
                     or os.environ.get("GPTGREP_HARBOR_PROC_TEST_PINS_SHA256"),
                     "Exact installed Harbor source pins were not selected")
class InstalledHarborTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        manifest_pin = {"path": os.environ.get("GPTGREP_HARBOR_PROC_TEST_PINS"),
                        "sha256": os.environ.get("GPTGREP_HARBOR_PROC_TEST_PINS_SHA256")}
        manifest_path = adapter._verify_pin(manifest_pin)
        adapter._need(manifest_path.stat().st_size <= adapter.MIB, "Installed test pin input is too large")
        selected = adapter._decode(manifest_path.read_bytes())
        adapter._need(isinstance(selected, dict) and set(selected) == {"schema", "harborRoot", "sources"}
                      and selected["schema"] == "gptgrep.harbor-proc-installed-tests.v1", "Unknown installed test pin input")
        root = adapter._physical(selected["harborRoot"])
        sources = selected["sources"]
        adapter._need(isinstance(sources, dict) and set(sources) == set(INSTALLED_MODULES) | {"prebuilt"},
                      "The complete installed Harbor source set must be pinned")
        for key, name in INSTALLED_MODULES.items():
            relative = name.removeprefix("harbor.").replace(".", "/")
            relative += "/__init__.py" if key == "compose" else ".py"
            adapter._need(adapter._verify_pin(sources[key]) == root / relative, "An installed source escaped its exact package role")
        adapter._need(adapter._verify_pin(sources["prebuilt"]) == root / "environments/docker/docker-compose-prebuilt.yaml",
                      "The installed prebuilt Compose source differs")
        guards = contextlib.ExitStack()
        cls.addClassCleanup(guards.close)
        cls.forbidden_processes = []
        for owner, name in ((subprocess, "Popen"), (subprocess, "run"), (os, "system"),
                            *((os, key) for key in ("fork", "forkpty", "posix_spawn", "posix_spawnp",
                                                    "execv", "execve", "execvp", "execvpe") if hasattr(os, key))):
            mock = guards.enter_context(patch.object(owner, name, side_effect=AssertionError("Unmocked process edge is forbidden")))
            cls.forbidden_processes.append(mock)
        for name in ("create_subprocess_exec", "create_subprocess_shell"):
            mock = guards.enter_context(patch.object(asyncio, name, new=AsyncMock(side_effect=AssertionError("Unmocked async process edge is forbidden"))))
            cls.forbidden_processes.append(mock)
        with patch.object(sys, "path", [str(root.parent), *sys.path]):
            cls.harbor = {key: importlib.import_module(name) for key, name in INSTALLED_MODULES.items()}
            for key, module in cls.harbor.items():
                adapter._need(Path(module.__file__).resolve() == Path(sources[key]["path"]), "An actual imported Harbor module differs")
            path = Path(__file__).with_name("proc_environment.py").resolve()
            spec = importlib.util.spec_from_file_location("unit_proc_environment_installed", path)
            actual = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(actual)
        cls.actual = actual
        cls.sources = sources
        cls.forbidden_processes.append(guards.enter_context(
            patch.object(actual._ExactEngineConnection, "connect", side_effect=AssertionError("Unmocked Engine socket edge is forbidden"))))

        class RealHarness(actual.ProcOverlayDockerEnvironment):
            # Reuse the in-memory wire simulator, retaining actual constructor,
            # inherited lifecycle, UnixOps and source-pin checks unchanged.
            _request_sync = HarnessEnvironment._request_sync
            _engine = HarnessEnvironment._engine
            row = HarnessEnvironment.row
            _delegate_compose = HarnessEnvironment._delegate_compose
            _delegate_engine_cp = HarnessEnvironment._delegate_engine_cp

            def __init__(self, *args, **kwargs):
                self.calls = []
                self.delegated = []
                self.fallback = []
                self.server_created = self.server_running = self.removed = False
                self.foreign_project = self.foreign_name = False
                self.fail_create = self.fail_start = self.fail_stop = self.fail_cp = False
                self.host_mutation = None
                self.server_pid = 1234
                self.started_at = "2026-01-01T00:00:00.000000000Z"
                super().__init__(*args, **kwargs)

        cls.environment_class = RealHarness
        adapter._need(all(mock.call_count == 0 for mock in cls.forbidden_processes), "Import attempted an unmocked process")

    def setUp(self):
        self.edge_counts = [mock.call_count for mock in self.forbidden_processes]
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.contract, self.template, self.profile = fixture(self.root)
        self.contract["module"] = {"path": self.actual.__file__, "sha256": hashlib.sha256(Path(self.actual.__file__).read_bytes()).hexdigest()}
        self.contract["harborSources"] = {key: self.sources[key] for key in adapter.SOURCE_MODULES}
        self.contract["sessionDerivation"]["source"] = self.sources["trial"]
        paths_module, task_module, trial_module = self.harbor["paths"], self.harbor["task"], self.harbor["trial"]
        self.paths = paths_module.TrialPaths(self.root / "trial")
        self.paths.mkdir()
        self.guest_paths = paths_module.EnvironmentPaths.for_os(task_module.TaskOS.LINUX)
        self.task_config = task_module.EnvironmentConfig(docker_image=self.contract["createRequest"]["Image"],
                                                        cpus=2, memory_mb=4096)
        # Execute the actual property function using the same runtime values
        # supplied by Trial; no copied implementation of its mount derivation.
        self.trial = types.SimpleNamespace(task=types.SimpleNamespace(config=types.SimpleNamespace(environment=self.task_config)),
                                           paths=self.paths, agent_env_paths=self.guest_paths, user_agent=None,
                                           config=types.SimpleNamespace(environment=types.SimpleNamespace(mounts=[]),
                                                                        verifier=types.SimpleNamespace(disable=True)))
        self.trial._main_artifacts_mount_dir = trial_module.Trial._main_artifacts_mount_dir.fget(self.trial)
        self.trial._main_artifacts_mount_dir.mkdir(parents=True)
        self.generated_mounts = trial_module.Trial._agent_env_mounts.fget(self.trial)
        self.environment = None

    def tearDown(self):
        # Detect a forbidden edge even if a Harbor best-effort path swallowed
        # its raised exception. No process call is accepted by this layer.
        self.assertEqual([mock.call_count for mock in self.forbidden_processes], self.edge_counts)

    def make_environment(self, mounts=None):
        pin = file_pin(self.root / "actual-proc-contract.json", self.contract)
        env = self.environment_class(environment_dir=Path(self.contract["environmentDirectory"]), environment_name="unit",
                                     session_id="Unit-Trial__Ab12Cd3__env", trial_paths=self.paths, task_env_config=self.task_config,
                                     extra_docker_compose=[self.contract["composeProfile"]["path"]],
                                     enable_environment_dir_upload=False, mounts=self.generated_mounts if mounts is None else mounts,
                                     proc_contract_path=pin["path"], proc_contract_sha256=pin["sha256"])
        self.environment = env
        return env

    async def test_real_start_uses_actual_generated_mounts_and_cleans_generated_files(self):
        env = self.make_environment()
        self.assertIsInstance(env, self.harbor["docker"].DockerEnvironment)
        self.assertIsInstance(env._platform, self.harbor["unix"].UnixOps)
        self.assertEqual(len(self.generated_mounts), 3)
        self.assertTrue(env.proc_implicit_mounts_suppressed)
        self.assertEqual(env._mounts, [])
        await env.start(False)
        generated = env._mounts_compose_path
        self.assertEqual(json.loads(generated.read_bytes()), {"services": {"main": {"volumes": []}}})
        self.assertTrue(env._env_compose_path.is_file())
        self.assertTrue(env._resources_compose_path.is_file())
        self.assertTrue(env._use_prebuilt)
        self.assertEqual(env.proc_environment_identity["container_id"], CID)
        self.assertEqual(sum(method == "POST" and "/create?" in endpoint for method, endpoint, _body in env.calls), 1)
        self.assertEqual(env.delegated, [])  # Empty mounts skip inherited mkdir/chmod.
        await env.stop(True)
        self.assertTrue(env._cleanup_complete)
        self.assertFalse(generated.exists())

    async def test_real_unix_download_fallback_binds_full_id_and_denial_never_retries(self):
        env = self.make_environment()
        await env.start(False)
        env.fail_cp = True
        await env.download_file("/work/file", self.root / "download")
        self.assertEqual(env.fallback, [(CID + ":/work/file", str(self.root / "download"))])
        env.foreign_project = True
        count = len(env.fallback)
        with self.assertRaises(self.actual.ProcEnvironmentError):
            await env.download_file("/work/file", self.root / "other")
        self.assertEqual(len(env.fallback), count)
        env.foreign_project = False
        await env.stop(True)

    async def test_real_unix_upload_fallback_keeps_main_service_and_tar_dispatch(self):
        env = self.make_environment()
        await env.start(False)
        env.fail_cp = True
        source = self.root / "unit-upload.txt"
        source.write_bytes(b"synthetic upload\n")
        await env.upload_file(source, "/work/upload")
        self.assertEqual(env.delegated[0], ["cp", str(source), "main:/work/upload"])
        self.assertTrue(any(command[:5] == ["exec", "-T", "-u", "root", "main"]
                            and "tar" in command for command in env.delegated))
        await env.stop(True)

    async def test_real_mounted_logs_artifacts_and_disabled_verifier_need_no_guest_logs(self):
        env = self.make_environment()
        await env.start(False)
        self.trial.agent_environment = env
        self.trial._are_agent_logs_downloaded = False
        before = len(env.calls), len(env.delegated), len(env.fallback)
        await self.harbor["trial"].Trial._download_agent_logs(self.trial)
        await self.harbor["trial"].Trial._upload_agent_logs(self.trial)
        await self.harbor["singleStep"].SingleStepTrial._run_verifier(self.trial)
        self.assertTrue(self.trial._are_agent_logs_downloaded)
        artifact_handler = self.harbor["artifacts"].ArtifactHandler(artifacts=[], logger=Mock())
        await artifact_handler.download_artifacts(env, self.paths.artifacts_dir, source_artifacts_dir=self.guest_paths.artifacts_dir)
        self.assertTrue(self.paths.artifacts_manifest_path.is_file())
        self.assertEqual((len(env.calls), len(env.delegated), len(env.fallback)), before)
        await env.stop(True)

    async def test_real_trial_additional_or_user_agent_mounts_are_rejected(self):
        self.trial.config.environment.mounts = [{"type": "bind", "source": str(self.root), "target": "/extra"}]
        rows = self.harbor["trial"].Trial._agent_env_mounts.fget(self.trial)
        with self.assertRaises(self.actual.ProcEnvironmentError):
            self.make_environment(rows)
        self.trial.config.environment.mounts = []
        self.trial.user_agent = object()
        rows = self.harbor["trial"].Trial._agent_env_mounts.fget(self.trial)
        with self.assertRaises(self.actual.ProcEnvironmentError):
            self.make_environment(rows)

    async def test_real_inherited_termination_reaps_delegates_on_base_exception(self):
        env = self.make_environment()
        for mode in ("compose", "engine-copy"):
            for error_type in (asyncio.CancelledError, ValueError):
                process = types.SimpleNamespace(returncode=None, terminate=Mock(), kill=Mock())

                async def wait():
                    process.returncode = -15
                    return -15

                process.wait = AsyncMock(side_effect=wait)
                with self.subTest(mode=mode, error=error_type), \
                        patch.object(self.actual.asyncio, "create_subprocess_exec", new=AsyncMock(return_value=process)), \
                        patch.object(env, "_collect_buffered_output", new=AsyncMock(side_effect=error_type("unit interruption"))):
                    with self.assertRaises(error_type):
                        if mode == "compose":
                            await self.actual.ProcOverlayDockerEnvironment._delegate_compose(
                                env, ["exec", "-T", "main", "true"], True, 5, None, None)
                        else:
                            await self.actual.ProcOverlayDockerEnvironment._delegate_engine_cp(
                                env, CID + ":/work/file", str(self.root / "download"))
                    process.terminate.assert_called_once_with()
                    process.wait.assert_awaited_once_with()
                    process.kill.assert_not_called()
                    self.assertEqual(process.returncode, -15)


if __name__ == "__main__":
    unittest.main()
