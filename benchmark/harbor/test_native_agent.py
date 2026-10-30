"""Stubbed bridge contract tests, never Harbor or owner runtime qualification.

The default Harbor API stub models the observed 0.24.0 constructor and its
closed options; a separate stub covers the explicit model-parsing parameter in
source commit 6099b8cf2bfbbb3e7ff1eb2163bfc4524dbd9ba5. Owner schemas below are
closed structural fixtures for exercised boundaries, not private owner policy.
No container, shell command, auth broker, native reader or provider is executed.
Runtime admission and comparative QA acceptance require independent evidence.
"""

import copy
import hashlib
import importlib.util
import json
import os
import stat
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch


class StubAgentOptions:
    def __init__(self, **kwargs):
        fields = set(type(self).__annotations__)
        unknown = set(kwargs) - fields
        if unknown:
            raise ValueError(f"Unknown option '{sorted(unknown)[0]}'.")
        if fields - set(kwargs):
            raise ValueError("Required agent options are missing")
        self.__dict__.update(kwargs)


class StubBaseAgent:
    """Observed 0.24.0 framework arguments, closed leftovers and model metadata."""

    MODEL_CONNECTION = None

    def __init__(self, logs_dir, model_name=None, logger=None, mcp_servers=None,
                 skills_dir=None, *, extra_env=None, load_trajectory=None,
                 environment_logs_dir=None, **kwargs):
        self.logs_dir = logs_dir
        self.model_name = model_name
        self.session_id = "unit-trial__agent"
        kwargs.pop("override_setup_timeout_sec", None)
        self.options = self.options_model(**kwargs)
        self._init_model_info()

    def _init_model_info(self):
        self._parsed_model_provider = None
        self._parsed_model_name = None
        if self.model_name is None:
            return
        if "/" in self.model_name:
            self._parsed_model_provider, self._parsed_model_name = self.model_name.split("/", 1)
        else:
            self._parsed_model_name = self.model_name


class StubDeclaredModelParsingAgent(StubBaseAgent):
    """The separate source API exposes the bypass before parsing user options."""

    def __init__(self, logs_dir, model_name=None, *, skip_model_parsing=False, **kwargs):
        self.skip_model_parsing = skip_model_parsing
        super().__init__(logs_dir, model_name=model_name, **kwargs)

    def _init_model_info(self):
        if not self.skip_model_parsing:
            raise AssertionError("The native bridge must bypass the declared provider parser")
        self._parsed_model_provider = "harbor-skip-parsing"
        self._parsed_model_name = self.model_name


class StubCapabilities:
    def __init__(self, **kwargs):
        self.__dict__.update(kwargs)


class StubContext:
    metadata = None
    n_input_tokens = None
    n_cache_tokens = None
    n_output_tokens = None
    model_usage = None
    cost_usd = None


def load_bridge_with_stubs(base_agent=StubBaseAgent):
    modules = {}
    for name in ("harbor", "harbor.agents", "harbor.environments", "harbor.models", "harbor.models.agent"):
        module = types.ModuleType(name)
        module.__path__ = []
        modules[name] = module
    for name, attributes in {
        "harbor.agents.base": {"BaseAgent": base_agent},
        "harbor.agents.options": {"AgentOptions": StubAgentOptions},
        "harbor.agents.capabilities": {"AgentCapabilities": StubCapabilities},
        "harbor.environments.base": {"BaseEnvironment": object},
        "harbor.models.agent.context": {"AgentContext": StubContext},
    }.items():
        module = types.ModuleType(name)
        module.__dict__.update(attributes)
        modules[name] = module
    spec = importlib.util.spec_from_file_location("gptgrep_native_agent_contract_unit", Path(__file__).with_name("native_agent.py"))
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, modules):
        spec.loader.exec_module(module)
    return module


bridge = load_bridge_with_stubs()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return (json.dumps(value, sort_keys=True, ensure_ascii=False) + "\n").encode()


def identity(path):
    info = path.lstat()
    return {"device": str(info.st_dev), "inode": str(info.st_ino), "uid": info.st_uid,
            "gid": info.st_gid, "mode": f"{stat.S_IMODE(info.st_mode):04o}"}


def closed_schema(fields, **properties):
    names = fields.split()
    return {"type": "object", "additionalProperties": False, "required": names,
            "properties": {name: properties.get(name, {}) for name in names}}


# Every listed field is required. These structural fixtures exercise bridge
# identity/link checks; the storage owner's full policy has separate validation.
OWNER_FIELDS = {
    "ownerTemplate": "schema id episode_id engine harbor_source compose_profile image_id image_pin reader_uid reader_gid guest_runtime mounts allowed_paths excluded_reader_roots sandbox_privacy_qualification commands outputs host_source_closure no_container_lifecycle cleanup admission_class auth_supervisor_source model_profile model_read_roots experiment_arm container_policy required_empty_directories network_policy filesystem_policy_name filesystem_policy_sha256 model_profile_guest_path",
    "runtimeContext": "schema episode_id compose_project compose_project_directory compose_files service host_episode_root harbor_source derivation auth_projection network_evidence",
    "namespacePlan": "schema owner_template runtime_context parent container mounts bootstrap_descriptor guest_startup_argv outputs source_closure_sha256 additional_reservation_required container_host_observation",
    "admission": "schema owner_template runtime_context namespace_plan parent_host_binding startup_receipt host_recheck_receipt container_binding container_binding_guest_path reservation container commands ready_for_command_materialization",
    "containerBinding": "schema binding_id owner_template_sha256 host_namespace_plan_sha256 parent_host_binding_sha256 startup_receipt_sha256 host_recheck_receipt_sha256 parent_deadline_monotonic_ns container mounts allowed_paths targets guest_runtime sandbox_privacy_qualification_sha256 custody_uid custody_mode mount_access digest_security command_binding_required admission_class collection_roots auth_projection_sha256 model_profile model_read_roots experiment_arm required_empty_directories network_evidence_sha256 filesystem_policy_name filesystem_policy_sha256",
    "commandPlan": "schema command_id owner_template admission container_binding command_binding command_binding_guest_path acceptance controller_argv child_argv environment environment_sha256 plan allowed_paths host_artifact_root guest_startup_path guest_terminal_path parent_deadline_monotonic_ns reservation one_use collection_seal_path workload guest_process_baseline_path guest_process_final_path guest_execution_identity_path",
    "commandBinding": "schema command_id owner_template_sha256 host_namespace_plan_sha256 parent_host_binding_sha256 container_binding container_binding_guest_path container_id acceptance immutable_job plan allowed_paths targets guest_runtime parent_deadline_monotonic_ns reservation custody_uid custody_mode mount_access exact_owner_injected_env_names admission_class",
    "collectionSeal": "schema owner_template_sha256 host_namespace_plan_sha256 parent_host_binding_sha256 container_binding_sha256 command_binding_sha256 command_plan_sha256 container_id container_generation guest_terminal host_recheck collection_roots artifacts source_originals_preserved projected_output_root collection_allowed container_may_remain_running reservation_status business_status guest_returncode",
    "runResult": "schema ok command_id owner_template_sha256 admission_sha256 transport collection_seal terminal_status business_status command_plan startup",
    "execStartup": "schema command_id command_plan_sha256 command_binding_sha256 container_binding_sha256 parent_host_binding_sha256 container_id boot_id namespaces uid gid supervisor_pid supervisor_start_ticks child_pid child_process_group command_sha256 started_monotonic_ns deadline_monotonic_ns timens_offsets_zero process_baseline child_role",
    "transport": "schema owner_template_sha256 runtime_context_sha256 command_id expected_admission_path command_plan_sha256 observed_returncode observed_signal observed_timeout observed_cancelled result stdout_bytes stdout_sha256 stderr_bytes stderr_sha256 observer_source_sha256 wrapper_argv_sha256 context_status",
    "guestTerminal": "schema command_id command_plan_sha256 command_binding_sha256 container_binding_sha256 startup_receipt_sha256 container_id boot_id namespaces result returncode child_reaped process_group_terminal output_bytes output_sha256 finished_monotonic_ns mount_recheck_sha256 timens_offsets_zero process_baseline process_final execution_child security_observation_sha256 execution_identity",
    "hostRecheck": "schema owner_template_sha256 host_namespace_plan_sha256 parent_host_binding_sha256 startup_sha256 container mounts compose_main_id_matches parent_lease_active volume_identity_matches guest_namespace_matches_host_process observed_monotonic_ns container_host_observation",
    "workload": "schema run_id cwd peak_bytes timeout_seconds output_limit_bytes internal_metadata_max_bytes allow_descendants allowed_paths memory_bytes pids_limit nano_cpus memory_swap_max_bytes collection_limit_bytes",
    "planResult": "schema ok plan container_id",
    "admissionResult": "schema ok admission container_binding container_id",
    "derivedPlanRule": "schema authority owner_template validator_source_closure_sha256 scenario_source_pins expected_episode_id harbor_derivation_source allowed_runtime_fields compose_file_roots max_compose_files fixed_mounts_paths_commands_security scope business_acceptance fixed_runtime_context",
    "derivedPlanReview": "schema rule owner_template runtime_context namespace_plan validator_source_closure_sha256 container validated_facts_sha256 result scope business_acceptance",
    "executionIdentity": "schema command_plan_sha256 command_binding_sha256 startup_receipt_sha256 guarded_worker_pid guarded_worker_start_ticks execution_child",
}
OWNER_DEFINITIONS = {name: closed_schema(fields) for name, fields in OWNER_FIELDS.items()}
OWNER_DEFINITIONS.update({
    "identity": closed_schema("device inode uid gid mode", device={"type": "string", "pattern": "^[0-9]+$"},
        inode={"type": "string", "pattern": "^[0-9]+$"}, uid={"type": "integer", "minimum": 0},
        gid={"type": "integer", "minimum": 0}, mode={"type": "string", "pattern": "^0[0-7]{3}$"}),
    "sealedArtifact": closed_schema("guest_path host_path host_identity sha256 bytes purpose",
        guest_path={"type": "string", "pattern": "^/"}, host_path={"type": "string", "pattern": "^/"},
        host_identity={"$ref": "#/$defs/identity"}, sha256={"type": "string", "pattern": "^[a-f0-9]{64}$"},
        bytes={"type": "string", "pattern": "^[0-9]+$"}, purpose={"type": "string"}),
    "collectionRoot": closed_schema("guest_root host_root host_identity purpose maximum_total_bytes",
        host_identity={"$ref": "#/$defs/identity"},
        purpose={"enum": ["private-output-originals", "answer-stage"]},
        maximum_total_bytes={"type": "string", "pattern": "^[0-9]+$"}),
})
OWNER_DEFINITIONS["collectionSeal"]["properties"].update({
    "artifacts": {"type": "array", "items": {"$ref": "#/$defs/sealedArtifact"}, "maxItems": 4096},
    "collection_roots": {"type": "array", "items": {"$ref": "#/$defs/collectionRoot"}, "minItems": 2, "maxItems": 2},
    "source_originals_preserved": {"const": True}, "collection_allowed": {"const": True},
    "container_may_remain_running": {"const": True}, "reservation_status": {"const": "held-until-outer-finalization"},
    "business_status": {"enum": ["passed", "failed"]},
})
OWNER_DEFINITIONS["guestTerminal"]["properties"].update({
    "child_reaped": {"const": True}, "process_group_terminal": {"const": True}, "timens_offsets_zero": {"const": True},
})
OWNER_DEFINITIONS["runResult"]["properties"].update({
    "ok": {"type": "boolean"}, "terminal_status": {"enum": ["verified-terminal", "unknown-retained"]},
    "business_status": {"enum": ["passed", "failed", "not-established"]},
})
OWNER_DEFINITIONS["derivedPlanReview"]["properties"].update({
    "result": {"const": "accepted_for_admission"}, "scope": {"const": "prior-reviewed-template-instance"},
    "business_acceptance": {"const": False},
})


def owner_record(definition, schema, **values):
    """Fill the structural stub; explicit values below carry the tested links."""
    result = {field: None for field in OWNER_FIELDS[definition].split()}
    result.update(schema=schema, **values)
    return result


class StubEnvironment:
    """Any use of Harbor's mutable Compose transport is a test failure."""

    def __init__(self):
        self.commands = []
        self.downloads = []

    async def exec(self, command, **kwargs):
        self.commands.append((command, kwargs))
        raise AssertionError("The v2 bridge must not execute through mutable Compose main")

    async def download_file(self, source, target):
        self.downloads.append((source, str(target)))
        raise AssertionError("The v2 bridge must retain sealed physical host originals")


class NativeBridgeContractTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.output = self.root / "admitted"
        self.output.mkdir()
        self.parent_storage = {"runId": "unit-parent-run", "workloadSha256": sha(b"unit parent workload"), "cwd": str(self.output)}
        self.binding = self.write(self.root / "storage-binding.json", {
            "targets": {"output": str(self.output)}, "plan": {"run_id": self.parent_storage["runId"],
                "workload_sha256": self.parent_storage["workloadSha256"], "cwd": self.parent_storage["cwd"]}})
        self.environment = patch.dict(os.environ, {
            "ENVIRONMENTS_HARNESS_STORAGE_BINDING": self.binding["path"],
            "ENVIRONMENTS_HARNESS_STORAGE_DIGEST": self.binding["sha256"],
        })
        self.environment.start()
        self.number = 0
        self.instruction = "Unit fixture instruction; not a benchmark question."
        self.fixtures = {}

    def tearDown(self):
        self.environment.stop()
        self.temp.cleanup()

    def write(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(value if isinstance(value, bytes) else encoded(value))
        path.chmod(0o600)
        return {"path": str(path), "sha256": sha(path.read_bytes())}

    def agent(self, changes=None, model="gpt-6.1-sol", overrides=None, template_changes=None, owner_changes=None, template_mutator=None,
              interface_status=bridge.OWNER_INTERFACE_STATUS):
        self.number += 1
        episode = self.output / f"episode-{self.number}"
        episode.mkdir()
        logs = episode / "harbor-logs"
        logs.mkdir()
        sources = {name: self.write(episode / "control" / f"harbor-{name}.txt", f"unit {name}\n".encode())
                   for name in ("docker", "trial", "base", "prebuilt")}
        job = self.write(episode / "control" / "reader-job.json", {"schema": "unit-immutable-job", "id": self.number})
        bun_config = self.write(episode / "control" / "empty.bunfig.toml", b"")
        c = {
            "schema": "gptgrep.harbor-native-bridge.v2", "taskId": "unit-task", "arm": "bare",
            "instructionSha256": sha(self.instruction.encode()), "model": "gpt-6.1-sol", "effort": "max",
            "serviceTier": "priority", "timeoutSeconds": 60, "cwd": "/app/task", "outputRoot": "/private/reader",
            "hostOutputRoot": str(episode / "projected"), "authRunId": "unit-auth-run",
            "maxArtifactBytes": 16 * bridge.MIB, "maxTraceBytes": 2 * bridge.MIB,
            "maxTrajectoryBytes": 2 * bridge.MIB, "maxMediaBytes": bridge.MIB,
            "maxCollectedBytes": 16 * bridge.MIB + bridge.HOST_COLLECTION_FIXED_OVERHEAD + 4 * bridge.MIB,
            "agentModule": {"path": str(Path(bridge.__file__).resolve()),
                            "sha256": sha(Path(bridge.__file__).read_bytes())},
            "bun": {"path": "/eh-runtime/bun", "sha256": "a" * 64},
            "bunConfig": {"path": "/eh-runtime/empty.bunfig.toml", "sha256": bun_config["sha256"]},
            "entrypoint": {"path": "/trusted/controller.ts", "sha256": "b" * 64},
            "readerJob": {"path": "/eh-control/reader-job.json", "sha256": job["sha256"]},
        }
        c.update(changes or {})
        allowed_paths = [{"path": path, "kind": "directory", "access": "write", "purpose": "unit trusted workload"}
                         for path in (c["cwd"], c["outputRoot"])]
        commands = []
        for mode in ("preflight", "reader"):
            workload = owner_record("workload", "environments-harness.container-storage-workload.v1",
                run_id=f"unit-{mode}", cwd=c["cwd"], peak_bytes={name: "0" for name in (
                    "remaining_download", "simultaneous_unpack", "build_test_output", "cache_growth",
                    "migration_new_copy", "retained_rollback_copy", "safety_margin")},
                timeout_seconds=c["timeoutSeconds"] if mode == "reader" else bridge.SETUP_SECONDS,
                output_limit_bytes=str(bridge.MIB), internal_metadata_max_bytes=str(bridge.MIB),
                allow_descendants=False, allowed_paths=allowed_paths, memory_bytes=str(1024 * bridge.MIB),
                pids_limit=64, nano_cpus="1000000000", memory_swap_max_bytes=str(1024 * bridge.MIB),
                collection_limit_bytes=str(c["maxArtifactBytes"]))
            commands.append({
                "id": mode, "run_id": f"unit-{mode}", "cwd": c["cwd"],
                "workload": self.write(episode / "control" / f"{mode}-workload.json", workload),
                "immutable_job": job,
                "acceptance_template": self.write(episode / "control" / f"{mode}-acceptance-template.json", {"unit": mode}),
                "source_pins": [c[name] for name in ("bun", "bunConfig", "entrypoint", "readerJob")],
                "controller_argv": [c["bun"]["path"], "--no-env-file", "--no-install", "--config=" + c["bunConfig"]["path"],
                                    c["entrypoint"]["path"], "--mode", mode,
                                    "--job", c["readerJob"]["path"], "--expect-sha256", c["readerJob"]["sha256"],
                                    "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"],
                "child_argv": [], "environment": [], "allowed_paths": allowed_paths,
                "host_artifact_root": str(episode / "owner" / mode),
                "guest_command_plan_path": f"/eh-control/{mode}/command-plan.json",
                "guest_command_binding_path": f"/eh-control/{mode}/command-binding.json",
                "guest_acceptance_path": f"/eh-control/{mode}/acceptance.json",
                "guest_startup_path": f"/eh-control/{mode}/startup.json",
                "guest_terminal_path": f"/eh-control/{mode}/terminal.json",
                "workload_guest_path": f"/eh-control/{mode}/workload.json",
                "immutable_job_guest_path": c["readerJob"]["path"],
            })
        outputs = {name: str(episode / "owner" / f"{name}.json") for name in
                   ("namespace_plan", "bootstrap_descriptor", "startup_capture", "host_recheck",
                    "container_binding_host", "admission", "finalization")}
        outputs["container_binding_guest"] = "/eh-control/container-binding.json"
        model_profile = self.write(episode / "control" / "model-profile.json", {"unit": "root deny"})
        broker_home = episode / "broker" / "codex-home"
        guest_profile_path = str(broker_home / "config.toml")
        mounts = [{"id": name, "host_source": str(source), "guest_target": target, "kind": kind,
                   "access": access, "purpose": "unit distinct host/guest bind", "propagation": "rprivate",
                   "create_host_path": False} for name, source, target, kind, access in (
            ("unit-control", episode / "owner", "/eh-control", "directory", "read"),
            ("unit-bun-config", Path(bun_config["path"]), c["bunConfig"]["path"], "file", "read"),
            ("unit-profile", Path(model_profile["path"]), guest_profile_path, "file", "read"),
            ("unit-originals", episode / "originals", c["outputRoot"], "directory", "write"),
            ("unit-answer-stage", episode / "answer-stage", "/app", "directory", "write"))]
        template = {
            "schema": "environments-harness.container-storage-template.v1", "id": "unit-template",
            "episode_id": "unit-episode", "engine": {}, "harbor_source": sources["docker"],
            "compose_profile": self.write(episode / "control" / "compose-profile.json", {"unit": "profile"}),
            "image_id": "sha256:" + "c" * 64, "image_pin": "unit@sha256:" + "c" * 64,
            "reader_uid": os.getuid(), "reader_gid": os.getgid(), "guest_runtime": {}, "mounts": mounts,
            "allowed_paths": allowed_paths, "excluded_reader_roots": [c["outputRoot"]],
            "sandbox_privacy_qualification": self.write(episode / "control" / "privacy.json", {"unit": "structural fixture only"}),
            "commands": commands, "outputs": outputs, "host_source_closure": [],
            "no_container_lifecycle": True, "cleanup": False, "admission_class": "reader",
            "auth_supervisor_source": self.write(episode / "control" / "auth-supervisor.txt", b"unit source\n"),
            "model_profile": model_profile, "model_profile_guest_path": guest_profile_path,
            "model_read_roots": ["/usr/bin", c["cwd"]], "experiment_arm": "baseline",
            "container_policy": {}, "required_empty_directories": [], "network_policy": {},
            "filesystem_policy_name": "unit-reader", "filesystem_policy_sha256": sha(b"unit root deny policy"),
        }
        definitions = copy.deepcopy(OWNER_DEFINITIONS)
        if interface_status in {"interface-contract-v6.3-frozen-for-source-preparation", bridge.OWNER_INTERFACE_STATUS}:
            template["harbor_derivation_source"] = sources["trial"]
            definitions["ownerTemplate"]["required"].append("harbor_derivation_source")
            definitions["ownerTemplate"]["properties"]["harbor_derivation_source"] = {}
        template.update(template_changes or {})
        if template_mutator is not None:
            template_mutator(template)
        interface = {
            "schema": "environments-harness.container-storage-design-contract.v1",
            "status": interface_status,
            "schema_bundle": {"$defs": definitions},
        }
        auth_projection = self.write(episode / "control" / "auth-projection.json",
                                     {"unit": "metadata only", "layout": {"codex_home": str(broker_home)}})
        template_pin = self.write(episode / "control" / "owner-template.json", template)
        rule = owner_record("derivedPlanRule", "environments-harness.container-derived-plan-rule.v1",
            authority=self.write(episode / "control" / "derived-rule-authority.json", {"unit": "prior authority fixture"}),
            owner_template=template_pin, validator_source_closure_sha256=sha(b"[]"), scenario_source_pins=[job],
            expected_episode_id=template["episode_id"], harbor_derivation_source=sources["trial"],
            allowed_runtime_fields=["trial_name", "environment_session_id", "compose_project", "compose_files",
                "container_id", "container_generation", "namespace_observations"],
            compose_file_roots=["parent-output", "parent-temporary"], max_compose_files=8,
            fixed_mounts_paths_commands_security=True, scope="derived-plan-admission-only", business_acceptance=False,
            fixed_runtime_context={"host_episode_root": str(episode), "compose_project_directory": str(episode / "task-environment")})
        owner = {
            "schema": "gptgrep.harbor-container-owner.v1",
            "interfaceContract": self.write(episode / "control" / "interface-contract.json", interface),
            "python": self.write(episode / "control" / "owner-python", b"unit executable pin; never executed\n"),
            "cli": self.write(episode / "control" / "owner-cli.py", b"unit CLI pin; never executed\n"),
            "template": template_pin,
            "parentStorage": self.parent_storage, "harborSources": sources, "hostEpisodeRoot": str(episode),
            "runtimeContextPath": str(episode / "owner" / "runtime-context.json"),
            "authProjectionPath": auth_projection["path"],
            "networkEvidencePath": str(episode / "owner" / "network-evidence.json"),
            "namespacePlanReviewPath": str(episode / "owner" / "namespace-review.json"),
            "phaseEvidenceRoot": str(episode / "owner" / "phase-evidence"), "ownerCallTimeoutSeconds": 60,
            "derivedPlanRule": self.write(episode / "control" / "derived-rule.json", rule),
        }
        owner.update(owner_changes or {})
        c["containerOwner"] = owner
        contract_pin = self.write(episode / "control" / "bridge-contract.json", c)
        kwargs = {"logs_dir": logs, "model_name": model, "bridge_contract_path": contract_pin["path"],
                  "bridge_contract_sha256": contract_pin["sha256"]}
        kwargs.update(overrides or {})
        agent = bridge.NativeCodexAgent(**kwargs)
        self.fixtures[id(agent)] = {"episode": episode, "template": template, "owner": owner, "job": job, "rule": rule,
                                   "auth_projection": auth_projection}
        return agent

    def owner_harness(self, agent, attempt_changes=None, native_changes=None, model_profile=None):
        """Stage coherent owner observations; stub only the process and Harbor observation."""
        f = self.fixtures[id(agent)]
        c, owner, template = agent.contract, f["owner"], f["template"]
        episode = f["episode"]
        originals, answers = episode / "originals", episode / "answer-stage"
        originals.mkdir(mode=0o700)
        answers.mkdir(mode=0o700)
        mounts = []
        for index, mount in enumerate(template["mounts"]):
            source = Path(mount["host_source"])
            if mount["kind"] == "directory":
                source.mkdir(mode=0o700, parents=True, exist_ok=True)
            mounts.append({"template": mount, "host_identity": identity(source), "guest_identity": identity(source),
                           "mount_id": str(index + 1), "mount_root": mount["host_source"],
                           "major_minor": "unit:fixture", "filesystem": "ext4"})
        roots = [
            {"guest_root": c["outputRoot"], "host_root": str(originals), "host_identity": identity(originals),
             "purpose": "private-output-originals", "maximum_total_bytes": str(c["maxArtifactBytes"] + bridge.MIB)},
            {"guest_root": "/app", "host_root": str(answers), "host_identity": identity(answers),
             "purpose": "answer-stage", "maximum_total_bytes": str(4 * bridge.MIB)},
        ]
        container = {"id": "d" * 64, "image_id": template["image_id"], "init_host_pid": 123,
                     "init_start_ticks": "456", "boot_id": "11111111-1111-4111-8111-111111111111",
                     "namespaces": {"mount": "mnt:[10]", "pid": "pid:[11]", "user": "user:[12]",
                                    "time": "time:[13]", "network": "net:[14]"},
                     "uid": os.getuid(), "gid": os.getgid(), "security": {},
                     "ownership_projection_sha256": "e" * 64}
        network_evidence = self.write(Path(owner["networkEvidencePath"]),
                                      {"unit": "structural network observation; no protocol probe executed"})
        context = owner_record("runtimeContext", "environments-harness.container-storage-runtime-context.v1",
            episode_id=template["episode_id"], compose_project="unit-trial__env",
            compose_project_directory=str(episode / "task-environment"), compose_files=[template["compose_profile"]],
            service="main", host_episode_root=str(episode), harbor_source=template["harbor_source"],
            derivation={"trial_name": "unit-trial", "environment_session_id": "unit-trial__env",
                        "compose_project_rule": "Harbor pinned DockerEnvironment derivation",
                        "harbor_derivation_source": owner["harborSources"]["trial"],
                        "expected_episode_id": template["episode_id"]},
            auth_projection=f["auth_projection"], network_evidence=network_evidence)
        context_pin = self.write(Path(owner["runtimeContextPath"]), context)
        bootstrap = self.write(episode / "owner" / "bootstrap.json", {"unit": "startup observation only"})
        observation = self.write(episode / "owner" / "host-observation.json", {"unit": "observed fixture"})
        namespace = owner_record("namespacePlan", "environments-harness.container-storage-plan.v1",
            owner_template=owner["template"], runtime_context=context_pin, parent={"binding": self.binding},
            container=container, mounts=mounts, bootstrap_descriptor=bootstrap, guest_startup_argv=["unit-supervisor"],
            outputs=template["outputs"], source_closure_sha256=sha(b"[]"), additional_reservation_required=True,
            container_host_observation=observation)
        namespace_pin = self.write(Path(template["outputs"]["namespace_plan"]), namespace)
        plan_result = owner_record("planResult", "environments-harness.container-storage-plan-result.v1",
            ok=True, plan=namespace_pin, container_id=container["id"])
        review = owner_record("derivedPlanReview", "environments-harness.container-derived-plan-review.v1",
            rule=owner["derivedPlanRule"], owner_template=owner["template"], runtime_context=context_pin,
            namespace_plan=namespace_pin, validator_source_closure_sha256=f["rule"]["validator_source_closure_sha256"],
            container=container, validated_facts_sha256="a" * 64, result="accepted_for_admission",
            scope="prior-reviewed-template-instance", business_acceptance=False)
        review_pin = self.write(Path(owner["namespacePlanReviewPath"]), review)
        base = owner_record("containerBinding", "environments-harness.container-storage-binding.v1",
            binding_id="unit-binding", owner_template_sha256=owner["template"]["sha256"],
            host_namespace_plan_sha256=namespace_pin["sha256"], parent_host_binding_sha256=self.binding["sha256"],
            startup_receipt_sha256=bootstrap["sha256"], host_recheck_receipt_sha256=observation["sha256"],
            parent_deadline_monotonic_ns="999999999999999", container=container, mounts=mounts, allowed_paths=template["allowed_paths"],
            targets={"output": c["outputRoot"]}, guest_runtime={},
            sandbox_privacy_qualification_sha256=template["sandbox_privacy_qualification"]["sha256"],
            custody_uid=os.getuid(), custody_mode="0600", mount_access="read-only", digest_security="integrity-not-signature",
            command_binding_required=True, admission_class="reader", collection_roots=roots,
            auth_projection_sha256=f["auth_projection"]["sha256"],
            model_profile=model_profile if model_profile is not None else {
                "path": template["model_profile_guest_path"], "sha256": template["model_profile"]["sha256"]},
            model_read_roots=template["model_read_roots"], experiment_arm=template["experiment_arm"],
            required_empty_directories=[], network_evidence_sha256=network_evidence["sha256"],
            filesystem_policy_name=template["filesystem_policy_name"],
            filesystem_policy_sha256=template["filesystem_policy_sha256"])
        base_pin = self.write(Path(template["outputs"]["container_binding_host"]), base)
        admission = owner_record("admission", "environments-harness.container-storage-admission.v1",
            owner_template=owner["template"], runtime_context=context_pin, namespace_plan=namespace_pin,
            parent_host_binding=self.binding, startup_receipt=bootstrap, host_recheck_receipt=observation,
            container_binding=base_pin, container_binding_guest_path=template["outputs"]["container_binding_guest"],
            reservation={"unit": "held"}, container=container, commands=template["commands"], ready_for_command_materialization=True)
        admission_pin = self.write(Path(template["outputs"]["admission"]), admission)
        admission_result = owner_record("admissionResult", "environments-harness.container-storage-admission-result.v1",
            ok=True, admission=admission_pin, container_binding=base_pin, container_id=container["id"])
        phases = {}
        for mode in ("preflight", "reader"):
            command = next(value for value in template["commands"] if value["id"] == mode)
            phase_root = Path(command["host_artifact_root"])
            acceptance_host = self.write(phase_root / "acceptance.json", {"unit": mode})
            acceptance = {"path": command["guest_acceptance_path"], "sha256": acceptance_host["sha256"]}
            argv = [value.replace("@ACCEPTANCE_PATH@", acceptance["path"])
                         .replace("@ACCEPTANCE_SHA256@", acceptance["sha256"]) for value in command["controller_argv"]]
            argv_sha = sha(json.dumps(argv, ensure_ascii=False, separators=(",", ":")).encode())
            plan_core = {"command": {"argv_sha256": argv_sha, "executable": c["bun"]["path"],
                                      "executable_sha256": c["bun"]["sha256"]},
                         "cwd": c["cwd"], "run_id": command["run_id"], "workload_sha256": command["workload"]["sha256"]}
            binding = owner_record("commandBinding", "environments-harness.container-command-binding.v1",
                command_id=mode, owner_template_sha256=owner["template"]["sha256"],
                host_namespace_plan_sha256=namespace_pin["sha256"], parent_host_binding_sha256=self.binding["sha256"],
                container_binding=base_pin, container_binding_guest_path=template["outputs"]["container_binding_guest"],
                container_id=container["id"], acceptance=acceptance, immutable_job=c["readerJob"], plan=plan_core,
                allowed_paths=command["allowed_paths"], targets={"output": c["outputRoot"]}, guest_runtime={},
                parent_deadline_monotonic_ns="999999999999999", reservation={"unit": "held"}, custody_uid=os.getuid(),
                custody_mode="0600", mount_access="read-only", exact_owner_injected_env_names=[
                    "ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"], admission_class="reader")
            binding_pin = self.write(phase_root / "command-binding.json", binding)
            plan = owner_record("commandPlan", "environments-harness.container-command-plan.v1",
                command_id=mode, owner_template=owner["template"], admission=admission_pin, container_binding=base_pin,
                command_binding=binding_pin, command_binding_guest_path=command["guest_command_binding_path"],
                acceptance=acceptance, controller_argv=argv, child_argv=command["child_argv"], environment=command["environment"],
                environment_sha256=sha(b"[]"), plan=plan_core, allowed_paths=command["allowed_paths"],
                host_artifact_root=str(phase_root), guest_startup_path=command["guest_startup_path"],
                guest_terminal_path=command["guest_terminal_path"], parent_deadline_monotonic_ns="999999999999999",
                reservation={"unit": "held"}, one_use=True, collection_seal_path=str(phase_root / "collection-seal.json"),
                workload={"path": command["workload_guest_path"], "sha256": command["workload"]["sha256"]},
                guest_process_baseline_path=f"/eh-control/{mode}/baseline.json",
                guest_process_final_path=f"/eh-control/{mode}/final.json",
                guest_execution_identity_path=f"/eh-control/{mode}/execution-identity.json")
            plan_pin = self.write(phase_root / "command-plan.json", plan)
            baseline = self.write(phase_root / "baseline.json", {"unit": "process identities only"})
            startup = owner_record("execStartup", "environments-harness.container-command-startup.v1",
                command_id=mode, command_plan_sha256=plan_pin["sha256"], command_binding_sha256=binding_pin["sha256"],
                container_binding_sha256=base_pin["sha256"], parent_host_binding_sha256=self.binding["sha256"],
                container_id=container["id"], boot_id=container["boot_id"], namespaces=container["namespaces"],
                uid=os.getuid(), gid=os.getgid(), supervisor_pid=20, supervisor_start_ticks="201", child_pid=21,
                child_process_group=21, command_sha256=argv_sha, started_monotonic_ns="1000",
                deadline_monotonic_ns="999999999999999", timens_offsets_zero=True, process_baseline=baseline,
                child_role="guarded-worker")
            startup_pin = self.write(phase_root / "startup.json", startup)
            execution_child = {"pid": 22, "process_group": 22, "start_ticks": "202"}
            execution = owner_record("executionIdentity", "environments-harness.container-storage-execution-identity.v1",
                command_plan_sha256=plan_pin["sha256"], command_binding_sha256=binding_pin["sha256"],
                startup_receipt_sha256=startup_pin["sha256"], guarded_worker_pid=startup["child_pid"],
                guarded_worker_start_ticks="201", execution_child=execution_child)
            execution_pin = self.write(phase_root / "execution-identity.json", execution)
            execution_guest_pin = {"path": plan["guest_execution_identity_path"], "sha256": execution_pin["sha256"]}
            phases[mode] = {"root": phase_root, "command": command, "binding": binding, "binding_pin": binding_pin,
                            "plan": plan, "plan_pin": plan_pin, "startup": startup, "startup_pin": startup_pin,
                            "baseline": baseline, "argv_sha": argv_sha, "execution": execution,
                            "execution_pin": execution_pin, "execution_guest_pin": execution_guest_pin,
                            "execution_child": execution_child}
        preflight = phases["preflight"]
        readiness = {"schema": "gptgrep.harbor-native-readiness.v2", "status": "ready",
                     "jobSha256": c["readerJob"]["sha256"], "taskId": c["taskId"], "arm": c["arm"],
                     "model": c["model"], "effort": c["effort"], "serviceTier": c["serviceTier"],
                     "commandId": "preflight", "commandBinding": {"path": preflight["plan"]["command_binding_guest_path"],
                         "sha256": preflight["binding_pin"]["sha256"]}, "storageBindingSha256": preflight["binding_pin"]["sha256"],
                     "containerBindingSha256": base_pin["sha256"], "ownerTemplateSha256": owner["template"]["sha256"],
                     "parentHostBindingSha256": self.binding["sha256"], "acceptance": preflight["plan"]["acceptance"],
                     "authRunId": c["authRunId"]}
        trace = b'{"direction":"unit-contract-fixture"}\n'
        answer = encoded({"answer": "unit fixture", "citations": [], "explanation": "unit contract only"})
        root = c["outputRoot"]
        native = {"schema": "gptgrep.native-run.v1", "executionMode": "live", "validStart": True, "status": "completed",
                  "threadId": "unit-root-thread", "turnId": "unit-root-turn", "requestedModel": c["model"],
                  "observedModel": c["model"], "requestedEffort": c["effort"], "requestedServiceTier": c["serviceTier"],
                  "runtimeBinarySha256": "a" * 64, "tracePath": root + "/trace.jsonl", "modelBoundaryValid": True,
                  "cleanup": {"completed": True}, "traceTruncated": False, "authFailure": None,
                  "usageScope": "root_thread_only", "usage": {"inputTokens": 10, "cachedInputTokens": 2, "outputTokens": 4}}
        native.update(native_changes or {})
        attempt = {"schema": "gptgrep.reader-attempt.v1", "executionMode": "live", "taskId": c["taskId"], "arm": c["arm"],
                   "authRunId": c["authRunId"], "storageBindingSha256": phases["reader"]["binding_pin"]["sha256"],
                   "status": "completed", "validStart": True, "answerPath": root + "/answer.json", "answerSha256": sha(answer),
                   "native": native, "readerTaskSha256": "b" * 64, "productRevision": "c" * 40, "binarySha256": "d" * 64,
                   "order": 1, "startedAt": "2000-01-01T00:00:00Z", "completedAt": "2000-01-01T00:00:01Z"}
        attempt.update(attempt_changes or {})
        trajectory = {"schema_version": "ATIF-v1.8", "trajectory_id": "codex-thread:unit-root-thread",
                      "extra": {"native_turn_id": "unit-root-turn"}, "steps": [{"observation": {"results": [{"content": [
                          {"type": "image", "source": {"media_type": "image/png", "path": root + "/media.png"}}]}]}}]}
        coverage = {"schema": "gptgrep.trajectory-coverage.v1", "executionMode": "live", "complete": False,
                    "gaps": ["unit_fixture_is_not_qualified"], "liveQualified": False,
                    "provenance": {"trace": {"path": root + "/trace.jsonl", "sha256": sha(trace)},
                                   "sourceSha256": sha(trace), "runtimeBinarySha256": native["runtimeBinarySha256"]}}
        contents = [("attempt", "attempt.json", encoded(attempt)), ("trace", "trace.jsonl", trace),
                    ("trajectory", "trajectory.json", encoded(trajectory)), ("coverage", "coverage.json", encoded(coverage)),
                    ("answer", "answer.json", answer), ("media", "media.png", b"unit media bytes; not qualification"),
                    ("preflight", "preflight-readiness.json", encoded(readiness))]
        files = {}
        for _role, name, data in contents:
            self.write(originals / name, data)
            files[root + "/" + name] = originals / name
        self.write(answers / "answer.json", answer)
        files["/app/answer.json"] = answers / "answer.json"
        manifest = {"schema": "gptgrep.harbor-native-artifacts.v1", "executionMode": "live",
                    "readerJobSha256": c["readerJob"]["sha256"], "taskId": c["taskId"], "arm": c["arm"],
                    "instructionSha256": c["instructionSha256"], "artifacts": [
                        {"role": role, "path": root + "/" + name, "sha256": sha(data), "bytes": len(data)}
                        for role, name, data in contents], "totalBytes": sum(len(data) for _, _, data in contents)}
        files[root + "/bridge-manifest.json"] = originals / "bridge-manifest.json"
        self.write(originals / "bridge-manifest.json", manifest)
        for mode, phase in phases.items():
            failed = mode == "reader" and attempt["status"] != "completed"
            returncode = 2 if failed else 0
            terminal = owner_record("guestTerminal", "environments-harness.container-command-terminal.v1",
                command_id=mode, command_plan_sha256=phase["plan_pin"]["sha256"],
                command_binding_sha256=phase["binding_pin"]["sha256"], container_binding_sha256=base_pin["sha256"],
                startup_receipt_sha256=phase["startup_pin"]["sha256"], container_id=container["id"],
                boot_id=container["boot_id"], namespaces=container["namespaces"], result="failure" if failed else "success",
                returncode=returncode, child_reaped=True, process_group_terminal=True, output_bytes="0", output_sha256=sha(b""),
                finished_monotonic_ns="2000", mount_recheck_sha256="f" * 64, timens_offsets_zero=True,
                process_baseline=phase["baseline"], process_final=phase["baseline"], execution_child=phase["execution_child"],
                security_observation_sha256="f" * 64, execution_identity=phase["execution_guest_pin"])
            terminal_pin = self.write(phase["root"] / "terminal.json", terminal)
            recheck = owner_record("hostRecheck", "environments-harness.container-storage-host-recheck.v1",
                owner_template_sha256=owner["template"]["sha256"], host_namespace_plan_sha256=namespace_pin["sha256"],
                parent_host_binding_sha256=self.binding["sha256"], startup_sha256=phase["startup_pin"]["sha256"],
                container=container, mounts=mounts, compose_main_id_matches=True, parent_lease_active=True,
                volume_identity_matches=True, guest_namespace_matches_host_process=True, observed_monotonic_ns="2001",
                container_host_observation=observation)
            recheck_pin = self.write(phase["root"] / "host-recheck.json", recheck)
            transport = owner_record("transport", "environments-harness.container-storage-transport.v1",
                owner_template_sha256=owner["template"]["sha256"], runtime_context_sha256=context_pin["sha256"],
                command_id=mode, expected_admission_path=admission_pin["path"], command_plan_sha256=phase["plan_pin"]["sha256"],
                observed_returncode=returncode, observed_signal=None, observed_timeout=False, observed_cancelled=False,
                result="returned", stdout_bytes="0", stdout_sha256=sha(b""), stderr_bytes="0", stderr_sha256=sha(b""),
                observer_source_sha256=owner["cli"]["sha256"], wrapper_argv_sha256=phase["argv_sha"], context_status="observed")
            transport_pin = self.write(phase["root"] / "transport.json", transport)
            seal = owner_record("collectionSeal", "environments-harness.container-storage-collection-seal.v1",
                owner_template_sha256=owner["template"]["sha256"], host_namespace_plan_sha256=namespace_pin["sha256"],
                parent_host_binding_sha256=self.binding["sha256"], container_binding_sha256=base_pin["sha256"],
                command_binding_sha256=phase["binding_pin"]["sha256"], command_plan_sha256=phase["plan_pin"]["sha256"],
                container_id=container["id"], container_generation=container, guest_terminal=terminal_pin, host_recheck=recheck_pin,
                collection_roots=roots, artifacts=[], source_originals_preserved=True, projected_output_root=c["hostOutputRoot"],
                collection_allowed=True, container_may_remain_running=True, reservation_status="held-until-outer-finalization",
                business_status="failed" if failed else "passed", guest_returncode=returncode)
            phase.update(terminal=terminal, terminal_pin=terminal_pin, recheck=recheck, recheck_pin=recheck_pin,
                         transport=transport, transport_pin=transport_pin, seal=seal)
            phase["result"] = owner_record("runResult", "environments-harness.container-storage-run-result.v1",
                ok=not failed, command_id=mode, owner_template_sha256=owner["template"]["sha256"],
                admission_sha256=admission_pin["sha256"], transport=transport_pin, collection_seal=None,
                terminal_status="verified-terminal", business_status=seal["business_status"],
                command_plan=phase["plan_pin"], startup=phase["startup_pin"])
        f.update(context=context, context_pin=context_pin, namespace=namespace, namespace_pin=namespace_pin, plan_result=plan_result,
                 review=review, review_pin=review_pin, admission_result=admission_result,
                 admission=admission, admission_pin=admission_pin, base=base, base_pin=base_pin, phases=phases,
                 files=files, roots=roots, readiness=readiness, attempt=attempt, manifest=manifest, calls=[])
        self.refresh_seals(agent)

        async def invoke(args, timeout):
            f["calls"].append((args, timeout))
            if args[0] == "plan":
                return copy.deepcopy(f["plan_result"])
            if args[0] == "validate-derived-plan":
                return copy.deepcopy(f["review"])
            if args[0] == "admit":
                return copy.deepcopy(f["admission_result"])
            if args[0] == "run":
                return copy.deepcopy(f["phases"][args[-1]]["result"])
            raise AssertionError("Unexpected owner command in structural contract fixture")

        agent._invoke_owner = invoke
        agent._create_runtime_context = lambda _environment: context_pin
        return StubEnvironment(), f

    def refresh_seals(self, agent, modes=("preflight", "reader")):
        f = self.fixtures[id(agent)]
        for mode in modes:
            phase = f["phases"][mode]
            selected = f["files"] if mode == "reader" else {
                agent.contract["outputRoot"] + "/preflight-readiness.json":
                    f["files"][agent.contract["outputRoot"] + "/preflight-readiness.json"]}
            phase["seal"]["artifacts"] = [
                {"guest_path": guest, "host_path": str(path), "host_identity": identity(path),
                 "sha256": sha(path.read_bytes()), "bytes": str(path.stat().st_size), "purpose": "unit retained original"}
                for guest, path in selected.items()]
            phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])

    def update_manifest(self, agent):
        f = self.fixtures[id(agent)]
        self.write(f["files"][agent.contract["outputRoot"] + "/bridge-manifest.json"], f["manifest"])
        self.refresh_seals(agent, ("reader",))

    async def prepare_collection(self, agent, environment):
        await agent.setup(environment)
        await agent._owner_run("reader")

    def test_explicit_reader_model_is_required(self):
        for model in (None, "gpt-6-luna", "openai/gpt-6.1-sol"):
            with self.subTest(model=model), self.assertRaisesRegex(ValueError, "explicitly select"):
                self.agent(model=model)
        with self.assertRaisesRegex(ValueError, "explicitly select"):
            self.agent(changes={"model": "gpt-6-luna"}, model="gpt-6-luna")

    def test_harbor_024_constructor_preserves_native_model_without_extra_option(self):
        agent = self.agent()
        self.assertEqual(agent.model_name, "gpt-6.1-sol")
        self.assertEqual(agent._parsed_model_name, "gpt-6.1-sol")
        self.assertIsNone(agent._parsed_model_provider)
        self.assertIsNone(agent.MODEL_CONNECTION)
        self.assertEqual(agent.contract["effort"], "max")
        self.assertEqual(agent.contract["serviceTier"], "priority")
        self.assertEqual(set(vars(agent.options)), {"bridge_contract_path", "bridge_contract_sha256"})

    def test_harbor_024_options_reject_unknown_constructor_leftovers(self):
        for name in ("skip_model_parsing", "unknown_native_option"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "Unknown option"):
                self.agent(overrides={name: True})

    def test_declared_model_parsing_option_bypasses_provider_parser(self):
        source_bridge = load_bridge_with_stubs(StubDeclaredModelParsingAgent)
        contract = self.write(self.root / "source-api-invalid-contract.json", {})
        agent = source_bridge.NativeCodexAgent.__new__(source_bridge.NativeCodexAgent)
        with self.assertRaisesRegex(ValueError, "Unknown bridge contract"):
            agent.__init__(logs_dir=self.root / "source-api-logs", model_name="gpt-6.1-sol",
                           bridge_contract_path=contract["path"],
                           bridge_contract_sha256=contract["sha256"])
        self.assertTrue(agent.skip_model_parsing)
        self.assertEqual(agent.model_name, "gpt-6.1-sol")
        self.assertEqual(agent._parsed_model_name, "gpt-6.1-sol")
        self.assertEqual(agent._parsed_model_provider, "harbor-skip-parsing")
        self.assertNotIn("skip_model_parsing", vars(agent.options))

    def test_effort_tier_and_finite_bounds_have_no_default(self):
        for changes in ({"effort": None}, {"effort": "automatic"}, {"serviceTier": ""},
                        {"timeoutSeconds": 0}, {"timeoutSeconds": True}, {"maxArtifactBytes": 0},
                        {"maxCollectedBytes": 16 * bridge.MIB}, {"maxCollectedBytes": True},
                        {"maxTraceBytes": 0}, {"maxTrajectoryBytes": True}, {"maxMediaBytes": 0},
                        {"maxTraceBytes": 64 * bridge.MIB + 1}, {"maxTrajectoryBytes": 128 * bridge.MIB + 1},
                        {"maxMediaBytes": 64 * bridge.MIB + 1}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.agent(changes=changes)

    def test_actual_imported_module_requires_the_supplied_path_and_hash(self):
        actual = Path(bridge.__file__).resolve()
        for pin in ({"path": str(actual), "sha256": "0" * 64},
                    {"path": str(self.root / "different-module.py"), "sha256": sha(actual.read_bytes())}):
            with self.subTest(pin=pin), self.assertRaisesRegex(ValueError, "actual imported native agent module"):
                self.agent(changes={"agentModule": pin})

    def test_host_collection_bound_includes_both_trajectory_copies(self):
        self.assertEqual(bridge.HOST_COLLECTION_FIXED_OVERHEAD, 10 * bridge.MIB)
        agent = self.agent()
        expected = 10 * bridge.MIB + 2 * agent.contract["maxTrajectoryBytes"]
        with self.assertRaisesRegex(ValueError, "all source and projected artifact copies"):
            self.agent(changes={"maxCollectedBytes": 16 * bridge.MIB + expected - 1})

    def test_provider_auth_and_tool_overrides_are_rejected(self):
        values = {"base_url": "https://invalid.example", "model_api_key_name": "UNIT_KEY",
                  "model_api_format": "responses", "load_trajectory": "/unit/old.json",
                  "skills_dir": "/unit/skills", "mcp_servers": ["unit"], "extra_env": {"UNIT": "value"}}
        for key, value in values.items():
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "forbids"):
                self.agent(overrides={key: value})

    def test_owner_contract_version_is_explicit_and_preserves_previous_interfaces(self):
        self.agent(interface_status="interface-contract-v6.1-frozen-for-source-preparation")
        self.agent(interface_status="interface-contract-v6.2-frozen-for-source-preparation")
        self.agent(interface_status="interface-contract-v6.3-frozen-for-source-preparation")
        self.agent(interface_status="interface-contract-v6.4-frozen-for-source-preparation")
        with self.assertRaisesRegex(ValueError, "reviewed owner interface version"):
            self.agent(interface_status="interface-contract-v7-frozen-for-source-preparation")

    def test_recent_owner_templates_bind_the_distinct_harbor_trial_source(self):
        for version in ("6.3", "6.4"):
            with self.subTest(version=version), self.assertRaisesRegex(ValueError, "Harbor trial derivation source"):
                self.agent(interface_status=f"interface-contract-v{version}-frozen-for-source-preparation",
                           template_mutator=lambda template: template.update(
                               harbor_derivation_source=template["harbor_source"]))

    def test_owner_contract_rejects_old_schema_unknown_fields_and_foreign_arm(self):
        for changes in ({"schema": "gptgrep.harbor-native-bridge.v1"}, {"storageBinding": self.binding}):
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "Unknown bridge contract"):
                self.agent(changes=changes)
        with self.assertRaisesRegex(ValueError, "Unknown container owner contract"):
            self.agent(owner_changes={"unexpected": True})
        with self.assertRaisesRegex(ValueError, "exact arm"):
            self.agent(template_changes={"experiment_arm": "treatment"})

    def test_owner_commands_must_preserve_exact_job_and_detached_argv(self):
        probe = self.agent()
        template = copy.deepcopy(self.fixtures[id(probe)]["template"])
        template["commands"][0]["controller_argv"].append("--unexpected")
        with self.assertRaisesRegex(ValueError, "frozen detached reader invocation"):
            self.agent(template_changes={"commands": template["commands"]})

    def test_reader_bun_loader_guards_and_config_source_are_required(self):
        probe = self.agent()
        original = self.fixtures[id(probe)]["template"]
        self.assertEqual(original["commands"][0]["controller_argv"][1:4],
                         ["--no-env-file", "--no-install", "--config=" + probe.contract["bunConfig"]["path"]])
        for flag in ("--no-env-file", "--no-install", "--config=" + probe.contract["bunConfig"]["path"]):
            with self.subTest(flag=flag), self.assertRaisesRegex(ValueError, "frozen detached reader invocation"):
                self.agent(template_mutator=lambda value: value["commands"][0]["controller_argv"].remove(flag))
        def omit_config(value):
            value["commands"][0]["source_pins"] = [pin for pin in value["commands"][0]["source_pins"]
                                                      if pin["path"] != probe.contract["bunConfig"]["path"]]
        with self.assertRaisesRegex(ValueError, "source closure"):
            self.agent(template_mutator=omit_config)

    def test_reader_bun_config_requires_empty_bytes_and_read_only_mapping(self):
        with self.assertRaisesRegex(ValueError, "configuration must be empty"):
            self.agent(changes={"bunConfig": {"path": "/eh-runtime/empty.bunfig.toml", "sha256": sha(b"install = true")}})
        probe = self.agent()
        mounts = copy.deepcopy(self.fixtures[id(probe)]["template"]["mounts"])
        selected = next(row for row in mounts if row["id"] == "unit-bun-config")
        selected["access"] = "write"
        with self.assertRaisesRegex(ValueError, "admitted bind"):
            self.agent(template_changes={"mounts": mounts})
        selected["access"] = "read"
        Path(selected["host_source"]).write_bytes(b"[install]\nauto = true\n")
        with self.assertRaisesRegex(ValueError, "Pinned host file changed"):
            self.agent(template_changes={"mounts": mounts})

    def test_host_storage_pin_and_output_scope_are_required(self):
        agent = self.agent()
        with patch.dict(os.environ, {"ENVIRONMENTS_HARNESS_STORAGE_DIGEST": "0" * 64}):
            with self.assertRaisesRegex(ValueError, "Pinned JSON artifact changed"):
                agent._host_storage()
        outside = self.agent(changes={"hostOutputRoot": str(self.root / "outside")})
        with self.assertRaisesRegex(ValueError, "within admitted host storage"):
            outside._host_storage()
        foreign = self.write(self.root / "foreign-parent-binding.json", {
            "targets": {"output": str(self.output)}, "plan": {"run_id": "another-run",
                "workload_sha256": self.parent_storage["workloadSha256"], "cwd": self.parent_storage["cwd"]}})
        with patch.dict(os.environ, {"ENVIRONMENTS_HARNESS_STORAGE_BINDING": foreign["path"],
                                    "ENVIRONMENTS_HARNESS_STORAGE_DIGEST": foreign["sha256"]}):
            with self.assertRaises(ValueError):
                agent._host_storage()
        agent._host_storage()
        rebound = self.write(self.root / "rebound-parent-binding.json", json.loads(Path(self.binding["path"]).read_bytes()))
        with patch.dict(os.environ, {"ENVIRONMENTS_HARNESS_STORAGE_BINDING": rebound["path"],
                                    "ENVIRONMENTS_HARNESS_STORAGE_DIGEST": rebound["sha256"]}):
            with self.assertRaisesRegex(ValueError, "changed within one Harbor episode"):
                agent._host_storage()

    def test_duplicate_keys_nonfinite_numbers_and_owner_schema_types_are_rejected(self):
        for raw in (b'{"id":1,"id":2}', b'{"value":NaN}', b'{"value":Infinity}'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                bridge._decode(raw)
        schema = closed_schema("id count", id={"$ref": "#/$defs/id"}, count={"type": "integer", "minimum": 1})
        definitions = {"id": {"type": "string", "pattern": "^[a-z]+$"}}
        bridge._validate_schema({"id": "unit", "count": 1}, schema, definitions)
        for value in ({"id": "unit", "count": True}, {"id": "UNIT", "count": 1},
                      {"id": "unit", "count": 0}, {"id": "unit"},
                      {"id": "unit", "count": 1, "extra": True}):
            with self.subTest(value=value), self.assertRaises(ValueError):
                bridge._validate_schema(value, schema, definitions)

    def test_sealed_identity_rejects_replacement_hardlink_symlink_and_mode_change(self):
        path = self.output / "original.json"
        self.write(path, {"unit": True})
        expected = identity(path)
        bridge._identity(path, expected)
        replacement = self.output / "replacement.json"
        self.write(replacement, path.read_bytes())
        replacement.replace(path)
        with self.assertRaisesRegex(ValueError, "physical identity changed"):
            bridge._identity(path, expected)
        expected = identity(path)
        link = self.output / "hardlink.json"
        os.link(path, link)
        with self.assertRaisesRegex(ValueError, "link custody"):
            bridge._identity(path, expected)
        link.unlink()
        path.chmod(0o640)
        with self.assertRaisesRegex(ValueError, "physical identity changed"):
            bridge._identity(path, expected)
        symbolic = self.output / "symbolic.json"
        symbolic.symlink_to(path)
        with self.assertRaisesRegex(ValueError, "Symlink"):
            bridge._identity(symbolic, identity(path))

    def test_actual_runtime_context_derives_trial_identity_and_retains_prebuilt_denial(self):
        for failure in (None, "upload", "image", "trial"):
            agent = self.agent()
            f = self.fixtures[id(agent)]
            owner = agent.contract["containerOwner"]
            Path(owner["runtimeContextPath"]).parent.mkdir(parents=True)
            environment_dir = f["episode"] / "task-environment"
            environment_dir.mkdir()
            modules = {}
            for key, name in (("docker", "harbor.environments.docker.docker"),
                              ("base", "harbor.environments.base"), ("trial", "harbor.trial.trial")):
                module = types.ModuleType(name)
                module.__file__ = owner["harborSources"][key]["path"]
                modules[name] = module
            environment_type = type("DockerEnvironment", (), {"__module__": "harbor.environments.docker.docker"})
            environment = environment_type()
            environment.session_id = "Task.Name__A1B2C3D__env"
            agent.session_id = "Task.Name__A1B2C3D__agent" if failure != "trial" else "another-trial__agent"
            environment.environment_dir = environment_dir
            environment._docker_compose_paths = [Path(owner["harborSources"]["prebuilt"]["path"]),
                                                  Path(f["template"]["compose_profile"]["path"])]
            environment._use_prebuilt = True
            environment._enable_environment_dir_upload = failure == "upload"
            environment.auto_mounting = False
            environment._enable_egress_control = False
            environment.stream_enabled = False
            environment._is_windows_container = False
            environment.task_env_config = types.SimpleNamespace(
                docker_image="foreign-image" if failure == "image" else f["template"]["image_pin"])
            environment._environment_docker_compose_path = environment_dir / "docker-compose.yaml"
            environment.extra_docker_compose_paths = [Path(f["template"]["compose_profile"]["path"])]
            with self.subTest(failure=failure), patch.dict(sys.modules, modules):
                if failure is None:
                    pin = agent._create_runtime_context(environment)
                    self.assertEqual(pin["path"], owner["runtimeContextPath"])
                else:
                    with self.assertRaisesRegex(ValueError, "same Harbor trial|frozen prebuilt"):
                        agent._create_runtime_context(environment)
            context_path = Path(owner["runtimeContextPath"])
            if failure == "trial":
                self.assertFalse(context_path.exists())
                continue
            context = json.loads(context_path.read_bytes())
            self.assertEqual(context["compose_project"], "task-name__a1b2c3d__env")
            self.assertEqual(context["derivation"]["trial_name"], "Task.Name__A1B2C3D")
            self.assertEqual(context["auth_projection"], f["auth_projection"])
            self.assertEqual(context["harbor_source"], owner["harborSources"]["docker"])

    def proc_runtime_fixture(self):
        agent = self.agent()
        f = self.fixtures[id(agent)]
        owner, template, episode = agent.contract["containerOwner"], agent._owner_template, f["episode"]
        f.update(owner=owner, template=template)
        environment_dir = episode / "task-environment"
        environment_dir.mkdir()
        Path(owner["runtimeContextPath"]).parent.mkdir(parents=True)
        module_pin = self.write(episode / "control" / "proc_environment.py", b"unit selected adapter source; not imported\n")
        support = {"docker": owner["harborSources"]["docker"], **{
            key: self.write(episode / "control" / f"{key}.py", ("unit " + key).encode())
            for key in ("unix", "runtime", "factory", "config")}}
        contract = {"schema": "gptgrep.harbor-proc-environment.v1", "module": module_pin, "harborSources": support,
                    "sessionDerivation": {"rule": "harbor-trial-environment-v1", "source": owner["harborSources"]["trial"]},
                    "environmentDirectory": str(environment_dir), "composeProfile": template["compose_profile"],
                    "engineSocket": {}, "engineBinary": owner["cli"], "composePlugin": owner["cli"],
                    "engineClientConfigRoot": str(episode / "client-config"),
                    "createRequest": {"Labels": {"com.docker.compose.project": "@HARBOR_PROJECT@"}}}
        contract_pin = self.write(episode / "control" / "proc-contract.json", contract)
        owner["procEnvironment"] = {"module": module_pin, "contract": contract_pin}
        template["host_source_closure"] = [module_pin, contract_pin, *support.values(), owner["harborSources"]["trial"]]
        template["container_policy"]["seccomp_profile"] = self.write(episode / "control" / "seccomp.json", {"unit": "profile"})
        agent._proc_contract = bridge._proc_environment_contract(owner, template, str(environment_dir))
        modules = {}
        for key, name in (("docker", "harbor.environments.docker.docker"),
                          ("base", "harbor.environments.base"), ("trial", "harbor.trial.trial")):
            module = types.ModuleType(name)
            module.__file__ = owner["harborSources"][key]["path"]
            modules[name] = module
        module = types.ModuleType("proc_environment")
        module.__file__ = module_pin["path"]
        module.validate_owner_template = lambda c, t, raw: self.assertEqual((c, t, raw),
            (contract, template, Path(template["container_policy"]["seccomp_profile"]["path"]).read_bytes()))
        modules["proc_environment"] = module
        environment_type = type("ProcOverlayDockerEnvironment", (), {"__module__": "proc_environment"})
        environment = environment_type()
        environment.session_id = "Task.Name__A1B2C3D__env"
        agent.session_id = "Task.Name__A1B2C3D__agent"
        environment.environment_dir = environment_dir
        environment._docker_compose_paths = [Path(owner["harborSources"]["prebuilt"]["path"]), Path(template["compose_profile"]["path"])]
        environment._use_prebuilt = True
        environment._enable_environment_dir_upload = environment._enable_egress_control = environment.stream_enabled = environment._is_windows_container = False
        environment.proc_implicit_mounts_suppressed = True
        environment._mounts, environment._persistent_env = [], {}
        environment.task_env_config = types.SimpleNamespace(docker_image=template["image_pin"])
        environment._environment_docker_compose_path = environment_dir / "docker-compose.yaml"
        environment.extra_docker_compose_paths = [Path(template["compose_profile"]["path"])]
        resolved = {"Labels": {"com.docker.compose.project": "task-name__a1b2c3d__env"}}
        environment.proc_environment_identity = {"contract_pin": contract_pin, "container_id": "c" * 64,
            "session_id": environment.session_id,
            "request_sha256": sha(json.dumps(resolved, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode())}
        return agent, environment, modules, f

    def test_selected_proc_environment_requires_exact_class_source_input_and_created_identity(self):
        for failure in (None, "unselected", "identity", "request", "mounts", "source", "closure"):
            agent, environment, modules, f = self.proc_runtime_fixture()
            if failure == "unselected":
                del f["owner"]["procEnvironment"]
            elif failure == "identity":
                environment.proc_environment_identity["contract_pin"] = f["owner"]["template"]
            elif failure == "request":
                environment.proc_environment_identity["request_sha256"] = "f" * 64
            elif failure == "mounts":
                environment.proc_implicit_mounts_suppressed = False
            elif failure == "source":
                Path(f["owner"]["procEnvironment"]["module"]["path"]).write_bytes(b"changed adapter")
            elif failure == "closure":
                f["template"]["host_source_closure"].pop()
            with self.subTest(failure=failure), patch.dict(sys.modules, modules):
                if failure is None:
                    self.assertEqual(agent._create_runtime_context(environment)["path"], f["owner"]["runtimeContextPath"])
                    self.assertEqual(agent._adapter_created_id, "c" * 64)
                else:
                    with self.assertRaises(ValueError):
                        agent._create_runtime_context(environment)

    async def test_owner_plan_must_resolve_the_adapter_created_container_before_admission(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        agent._adapter_created_id = "e" * 64
        with self.assertRaisesRegex(ValueError, "Observed namespace plan"):
            await agent.setup(environment)
        self.assertEqual([args[0] for args, _timeout in f["calls"]], ["plan"])

    async def test_guest_control_pins_use_distinct_admitted_host_files(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        phase = f["phases"]["preflight"]
        reads = []
        owner_json = agent._owner_json

        def observed(pin, definition):
            reads.append((dict(pin), definition))
            return owner_json(pin, definition)

        agent._owner_json = observed
        self.assertNotEqual(phase["execution_pin"]["path"], phase["execution_guest_pin"]["path"])
        self.assertNotEqual(f["base"]["model_profile"]["path"], f["template"]["model_profile"]["path"])
        auth = json.loads(Path(f["owner"]["authProjectionPath"]).read_bytes())
        self.assertEqual(f["base"]["model_profile"]["path"], str(Path(auth["layout"]["codex_home"]) / "config.toml"))
        self.assertEqual(bridge._guest_pin(f["base"], f["base"]["model_profile"]), f["template"]["model_profile"])
        await agent.setup(environment)
        self.assertIn((phase["execution_pin"], "executionIdentity"), reads)
        self.assertNotIn((phase["execution_guest_pin"], "executionIdentity"), reads)
        self.assertEqual(environment.commands, [])
        self.assertEqual(environment.downloads, [])

    async def test_guest_pins_reject_existing_host_path_substitution(self):
        agent = self.agent()
        host_profile = self.fixtures[id(agent)]["template"]["model_profile"]
        environment, f = self.owner_harness(agent, model_profile=host_profile)
        with self.assertRaisesRegex(ValueError, "Actual container binding differs"):
            await agent.setup(environment)
        self.assertEqual([args[0] for args, _timeout in f["calls"]], ["plan", "validate-derived-plan", "admit"])

        agent = self.agent()
        environment, f = self.owner_harness(agent)
        phase = f["phases"]["preflight"]
        phase["terminal"]["execution_identity"] = phase["execution_pin"]
        phase["seal"]["guest_terminal"] = self.write(phase["root"] / "terminal.json", phase["terminal"])
        phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])
        with self.assertRaisesRegex(ValueError, "Execution identity is outside the exact command plan"):
            await agent.setup(environment)
        self.assertNotIn("preflight", agent._phase_seals)

    def test_guest_mapping_rejects_unmapped_ambiguous_and_file_descendant_paths(self):
        agent = self.agent()
        _environment, f = self.owner_harness(agent)
        phase = f["phases"]["preflight"]
        for path in (phase["execution_pin"]["path"], "/unmapped/identity.json",
                     f["base"]["model_profile"]["path"] + "/child", "/eh-control/preflight/../reader/identity.json"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                bridge._map_guest(f["base"], path)
        ambiguous = copy.deepcopy(f["base"])
        ambiguous["mounts"].append(copy.deepcopy(ambiguous["mounts"][0]))
        with self.assertRaisesRegex(ValueError, "unambiguous owner mount mapping"):
            bridge._map_guest(ambiguous, phase["execution_guest_pin"]["path"])

    async def test_setup_uses_exact_owner_plan_admit_and_run_without_compose_transport(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        await agent.setup(environment)
        owner = agent.contract["containerOwner"]
        calls = [args for args, _timeout in f["calls"]]
        self.assertEqual([args[0] for args in calls], ["plan", "validate-derived-plan", "admit", "run"])
        self.assertEqual(calls[0], ["plan", "--template", owner["template"]["path"],
            "--expect-template-digest", owner["template"]["sha256"], "--runtime-context", f["context_pin"]["path"],
            "--expect-context-digest", f["context_pin"]["sha256"], "--parent-binding", self.binding["path"],
            "--expect-parent-digest", self.binding["sha256"], "--out", f["namespace_pin"]["path"]])
        self.assertEqual(calls[1], ["validate-derived-plan", "--plan", f["namespace_pin"]["path"],
            "--expect-digest", f["namespace_pin"]["sha256"], "--rule", owner["derivedPlanRule"]["path"],
            "--expect-rule-digest", owner["derivedPlanRule"]["sha256"], "--out", owner["namespacePlanReviewPath"]])
        self.assertEqual(calls[2], ["admit", "--plan", f["namespace_pin"]["path"],
            "--expect-digest", f["namespace_pin"]["sha256"], "--review", f["review_pin"]["path"],
            "--expect-review-digest", f["review_pin"]["sha256"]])
        self.assertEqual(calls[3], ["run", "--admission", f["admission_pin"]["path"],
            "--expect-admission-digest", f["admission_pin"]["sha256"], "--command-id", "preflight"])
        self.assertTrue(agent._ready)
        self.assertEqual(environment.commands, [])
        self.assertEqual(environment.downloads, [])

    async def test_namespace_review_is_required_before_admit_and_failed_setup_cannot_replay(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        review_path = Path(agent.contract["containerOwner"]["namespacePlanReviewPath"])
        f["review"]["namespace_plan"] = {**f["review"]["namespace_plan"], "sha256": "0" * 64}
        self.write(review_path, f["review"])
        with self.assertRaisesRegex(ValueError, "review|derived-plan"):
            await agent.setup(environment)
        self.assertEqual([args[0] for args, _timeout in f["calls"]], ["plan", "validate-derived-plan"])
        self.assertFalse(agent._ready)
        with self.assertRaisesRegex(ValueError, "cannot be reused"):
            await agent.setup(environment)
        self.assertEqual(len(f["calls"]), 2)

    async def test_owner_result_wrappers_must_name_the_observed_plan_and_admission(self):
        for mutation in ("plan", "admission"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            if mutation == "plan":
                f["plan_result"]["container_id"] = "0" * 64
            else:
                f["admission_result"]["container_binding"] = {**f["base_pin"], "sha256": "0" * 64}
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, "namespace plan differs|admission identity differs"):
                await agent.setup(environment)
            self.assertNotIn("run", [args[0] for args, _timeout in f["calls"]])

    def test_container_comparison_ignores_only_the_allowed_host_telemetry(self):
        original = {"id": "a" * 64, "security": {"resources": {
            "host_mem_available_bytes": "1000", "host_observation_sha256": "b" * 64, "memory_max": "2048"}}}
        changed = copy.deepcopy(original)
        changed["security"]["resources"].update(host_mem_available_bytes="900", host_observation_sha256="c" * 64)
        self.assertEqual(bridge._stable_container(original), bridge._stable_container(changed))
        changed["security"]["resources"]["memory_max"] = "4096"
        self.assertNotEqual(bridge._stable_container(original), bridge._stable_container(changed))
        self.assertEqual(original["security"]["resources"]["host_mem_available_bytes"], "1000")

    async def test_readiness_binds_auth_storage_command_and_reader_identity(self):
        fields = ("authRunId", "storageBindingSha256", "containerBindingSha256", "parentHostBindingSha256",
                  "ownerTemplateSha256", "jobSha256", "model", "effort", "serviceTier", "taskId", "arm", "commandId",
                  "commandBinding", "acceptance")
        for field in fields:
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            f["readiness"][field] = ({**f["readiness"][field], "path": "/foreign/pin.json"}
                                     if field in {"commandBinding", "acceptance"} else "foreign")
            self.write(f["files"][agent.contract["outputRoot"] + "/preflight-readiness.json"], f["readiness"])
            self.refresh_seals(agent)
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "Sealed readiness differs"):
                await agent.setup(environment)
            self.assertFalse(agent._ready)
            self.assertNotIn("reader", [args[-1] for args, _timeout in f["calls"] if args[0] == "run"])

    async def test_preflight_readiness_is_bounded_and_must_be_in_owner_seal(self):
        for missing in (False, True):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            if missing:
                f["phases"]["preflight"]["seal"]["artifacts"] = []
                phase = f["phases"]["preflight"]
                phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])
            else:
                self.write(f["files"][agent.contract["outputRoot"] + "/preflight-readiness.json"], b"x" * (64 * 1024 + 1))
                self.refresh_seals(agent)
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                await agent.setup(environment)
            self.assertFalse(agent._ready)

    async def test_owner_phase_rejects_another_namespace_generation_and_unknown_terminal(self):
        for mutation in ("namespace", "terminal"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            phase = f["phases"]["preflight"]
            if mutation == "namespace":
                phase["startup"]["container_id"] = "0" * 64
                phase["result"]["startup"] = self.write(phase["root"] / "startup.json", phase["startup"])
            else:
                phase["result"].update(terminal_status="unknown-retained", collection_seal=None)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                await agent.setup(environment)
            self.assertFalse(agent._ready)
            self.assertNotIn("preflight", agent._phase_seals)

    async def test_materialized_owner_command_cannot_change_argv_or_immutable_job(self):
        for mutation in ("argv", "job"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            phase = f["phases"]["preflight"]
            if mutation == "argv":
                phase["plan"]["controller_argv"].append("--unexpected")
            else:
                phase["binding"]["immutable_job"] = {**phase["binding"]["immutable_job"], "sha256": "0" * 64}
                phase["plan"]["command_binding"] = self.write(phase["root"] / "command-binding.json", phase["binding"])
            phase["result"]["command_plan"] = self.write(phase["root"] / "command-plan.json", phase["plan"])
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, "Materialized owner command"):
                await agent.setup(environment)
            self.assertNotIn("preflight", agent._phase_seals)

    async def test_passed_phase_requires_the_bound_observed_controller_child(self):
        for mutation in ("guarded-worker", "actual-child"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            phase = f["phases"]["preflight"]
            if mutation == "guarded-worker":
                phase["execution"]["guarded_worker_pid"] = 999
            else:
                phase["execution"]["execution_child"] = None
                phase["terminal"]["execution_child"] = None
            execution = self.write(phase["root"] / "execution-identity.json", phase["execution"])
            phase["terminal"]["execution_identity"] = {"path": phase["plan"]["guest_execution_identity_path"],
                                                        "sha256": execution["sha256"]}
            phase["seal"]["guest_terminal"] = self.write(phase["root"] / "terminal.json", phase["terminal"])
            phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, "terminal chain differs|observed controller child"):
                await agent.setup(environment)
            self.assertNotIn("preflight", agent._phase_seals)

    async def test_seal_does_not_replace_terminal_proof_or_descriptor_custody(self):
        for mutation in ("collection-flag", "terminal-flag", "mode"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            phase = f["phases"]["preflight"]
            if mutation == "collection-flag":
                phase["seal"]["collection_allowed"] = False
                phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])
            elif mutation == "terminal-flag":
                phase["terminal"]["child_reaped"] = False
                phase["seal"]["guest_terminal"] = self.write(phase["root"] / "terminal.json", phase["terminal"])
                phase["result"]["collection_seal"] = self.write(phase["root"] / "collection-seal.json", phase["seal"])
            else:
                Path(f["admission_pin"]["path"]).chmod(0o640)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                await agent.setup(environment)
            self.assertNotIn("preflight", agent._phase_seals)

    async def test_narrow_original_caps_and_combined_media_bound_are_enforced(self):
        for field in ("maxTraceBytes", "maxTrajectoryBytes", "maxMediaBytes"):
            agent = self.agent(changes={field: 1})
            environment, _f = self.owner_harness(agent)
            await self.prepare_collection(agent, environment)
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "byte bound is invalid"):
                await agent._collect(environment, StubContext())
        agent = self.agent(changes={"maxMediaBytes": 64})
        environment, f = self.owner_harness(agent)
        media = next(item for item in f["manifest"]["artifacts"] if item["role"] == "media")
        second = {**media, "path": agent.contract["outputRoot"] + "/second-media.png"}
        f["manifest"]["artifacts"].append(second)
        f["manifest"]["totalBytes"] += second["bytes"]
        second_path = f["episode"] / "originals" / "second-media.png"
        self.write(second_path, f["files"][media["path"]].read_bytes())
        f["files"][second["path"]] = second_path
        self.update_manifest(agent)
        await self.prepare_collection(agent, environment)
        with self.assertRaisesRegex(ValueError, "Aggregate media retention"):
            await agent._collect(environment, StubContext())

    async def test_sealed_inventory_cannot_exceed_the_declared_source_budget(self):
        agent = self.agent(changes={"maxArtifactBytes": 64 * 1024})
        environment, f = self.owner_harness(agent)
        path = agent.contract["outputRoot"] + "/media.png"
        pin = self.write(f["files"][path], b"x" * (70 * 1024))
        item = next(item for item in f["manifest"]["artifacts"] if item["role"] == "media")
        old_size = item["bytes"]
        item.update(sha256=pin["sha256"], bytes=70 * 1024)
        f["manifest"]["totalBytes"] += item["bytes"] - old_size
        self.update_manifest(agent)
        await agent.setup(environment)
        with self.assertRaisesRegex(ValueError, "original/collection byte limits"):
            await agent._owner_run("reader")
        self.assertNotIn("reader", agent._phase_seals)

    async def test_manifest_rejects_foreign_identity_paths_roles_and_byte_bounds(self):
        mutations = [
            lambda m: m.update(executionMode="test"),
            lambda m: m.update(readerJobSha256="0" * 64),
            lambda m: m["artifacts"][0].update(path="/private/reader/../foreign.json"),
            lambda m: m["artifacts"][0].update(path="/private/foreign.json"),
            lambda m: m["artifacts"][0].update(role="judge"),
            lambda m: m["artifacts"][0].update(bytes=5 * bridge.MIB),
            lambda m: m["artifacts"].append(copy.deepcopy(m["artifacts"][0])),
            lambda m: m.update(totalBytes=m["totalBytes"] + 1),
            lambda m: m.update(artifacts=[item for item in m["artifacts"] if item["role"] != "preflight"],
                               totalBytes=m["totalBytes"] - sum(item["bytes"] for item in m["artifacts"] if item["role"] == "preflight")),
        ]
        for index, mutate in enumerate(mutations):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            mutate(f["manifest"])
            self.update_manifest(agent)
            await self.prepare_collection(agent, environment)
            with self.subTest(case=index), self.assertRaises(ValueError):
                await agent._collect(environment, StubContext())
            self.assertEqual(environment.commands, [])
            self.assertEqual(environment.downloads, [])

    async def test_preflight_start_and_denial_roles_retain_exact_source_pins(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        for role in ("start", "denial"):
            guest = agent.contract["outputRoot"] + f"/{role}.json"
            path = f["episode"] / "originals" / f"{role}.json"
            pin = self.write(path, {"schema": "unit-retained-metadata.v1", "role": role})
            f["files"][guest] = path
            f["manifest"]["artifacts"].append({"role": role, "path": guest, "sha256": pin["sha256"], "bytes": path.stat().st_size})
            f["manifest"]["totalBytes"] += path.stat().st_size
        self.update_manifest(agent)
        await self.prepare_collection(agent, environment)
        projected = await agent._collect(environment, StubContext())
        mapping = {item["sourcePath"]: item["artifact"] for item in projected["artifactRelocation"]["files"]}
        for role in ("start", "denial", "preflight"):
            item = next(item for item in f["manifest"]["artifacts"] if item["role"] == role)
            self.assertEqual(mapping[item["path"]], {"path": str(f["files"][item["path"]]), "sha256": item["sha256"]})
        self.assertEqual(bridge.ROLES["preflight"], 64 * 1024)
        self.assertEqual(bridge.ROLES["start"], bridge.MIB)
        self.assertEqual(bridge.ROLES["denial"], bridge.MIB)

    async def test_terminal_identity_and_status_cannot_be_forged_with_matching_hashes(self):
        for changes in ({"taskId": "foreign"}, {"arm": "gptgrep"}, {"authRunId": "foreign"},
                        {"storageBindingSha256": "0" * 64}, {"status": "success"},
                        {"executionMode": "test"}, {"validStart": "true"}):
            agent = self.agent()
            environment, _f = self.owner_harness(agent, attempt_changes=changes)
            await self.prepare_collection(agent, environment)
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "terminal artifact"):
                await agent._collect(environment, StubContext())

    async def test_completed_answer_stage_must_match_the_original_answer(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        self.write(f["files"]["/app/answer.json"], {"answer": "different stage bytes"})
        self.refresh_seals(agent, ("reader",))
        await self.prepare_collection(agent, environment)
        with self.assertRaisesRegex(ValueError, "/app/answer.json differs"):
            await agent._collect(environment, StubContext())
        self.assertFalse((Path(agent.contract["hostOutputRoot"]) / "collection.json").exists())

    async def test_reader_cannot_replace_the_original_preflight_receipt(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        await agent.setup(environment)
        path = agent.contract["outputRoot"] + "/preflight-readiness.json"
        f["readiness"]["serviceTier"] = "another-tier"
        replacement = self.write(f["files"][path], f["readiness"])
        item = next(item for item in f["manifest"]["artifacts"] if item["role"] == "preflight")
        old_size = item["bytes"]
        item.update(sha256=replacement["sha256"], bytes=f["files"][path].stat().st_size)
        f["manifest"]["totalBytes"] += item["bytes"] - old_size
        self.update_manifest(agent)
        await agent._owner_run("reader")
        with self.assertRaisesRegex(ValueError, "Sealed original bytes changed"):
            await agent._collect(environment, StubContext())

    async def test_original_bytes_and_bind_mapping_must_match_owner_seal(self):
        for mutation in ("bytes", "host-path", "owner-size"):
            agent = self.agent()
            environment, f = self.owner_harness(agent)
            await self.prepare_collection(agent, environment)
            path = agent.contract["outputRoot"] + "/trace.jsonl"
            original = f["files"][path]
            sealed = next(item for item in agent._phase_seals["reader"]["artifacts"] if item["guest_path"] == path)
            if mutation == "bytes":
                original.write_bytes(original.read_bytes() + b" ")
            elif mutation == "host-path":
                sealed["host_path"] = str(f["episode"] / "foreign.json")
            else:
                sealed["bytes"] = str(agent.contract["maxTraceBytes"] + 1)
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, "Sealed original"):
                await agent._collect(environment, StubContext())

    async def test_projection_reuses_unchanged_originals_and_records_owner_chain(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        before = {guest: (path.read_bytes(), identity(path)) for guest, path in f["files"].items()}
        await self.prepare_collection(agent, environment)
        context = StubContext()
        projected = await agent._collect(environment, context)
        relocation = projected["artifactRelocation"]
        mapping = {item["sourcePath"]: item["artifact"] for item in relocation["files"]}
        for guest, pin in mapping.items():
            self.assertEqual(pin["path"], str(f["files"][guest]))
            self.assertEqual(pin["sha256"], sha(before[guest][0]))
        for guest, path in f["files"].items():
            self.assertEqual((path.read_bytes(), identity(path)), before[guest])
        restored = copy.deepcopy(projected)
        restored.pop("artifactRelocation")
        restored["answerPath"] = f["attempt"]["answerPath"]
        restored["native"]["tracePath"] = f["attempt"]["native"]["tracePath"]
        self.assertEqual(restored, f["attempt"])
        host = Path(agent.contract["hostOutputRoot"])
        self.assertFalse((host / "source").exists())
        collection = json.loads((host / "collection.json").read_bytes())
        self.assertEqual(collection["schema"], "gptgrep.harbor-native-collection.v2")
        self.assertEqual(collection["sourceAttempt"]["path"], str(f["files"][agent.contract["outputRoot"] + "/attempt.json"]))
        self.assertEqual(collection["sourceManifest"]["path"], str(f["files"][agent.contract["outputRoot"] + "/bridge-manifest.json"]))
        self.assertEqual(set(collection["ownerEvidence"]), {"ownerTemplate", "parentHostBinding", "runtimeContext", "namespacePlanReview", "admission", "preflight", "reader"})
        self.assertNotIn("finalization", collection["ownerEvidence"])
        self.assertEqual(set(collection["ownerEvidence"]["reader"]), {"result", "commandPlan", "startup"})
        self.assertEqual((host / "trajectory.json").read_bytes(), (agent.logs_dir / "trajectory.json").read_bytes())
        self.assertFalse(context.metadata["runtime_accepted"])
        self.assertFalse(context.metadata["trajectory_coverage"]["complete"])
        self.assertIsNone(context.n_input_tokens)
        self.assertIsNone(context.model_usage)

    async def test_projected_atif_bound_rejects_without_partial_projection(self):
        probe = self.agent()
        _environment, f = self.owner_harness(probe)
        original_size = f["files"][probe.contract["outputRoot"] + "/trajectory.json"].stat().st_size
        agent = self.agent(changes={"maxTrajectoryBytes": original_size})
        environment, _f = self.owner_harness(agent)
        await self.prepare_collection(agent, environment)
        with self.assertRaisesRegex(ValueError, "Projected artifact exceeds its finite byte bound"):
            await agent._collect(environment, StubContext())
        self.assertFalse((Path(agent.contract["hostOutputRoot"]) / "trajectory.json").exists())
        self.assertFalse((agent.logs_dir / "trajectory.json").exists())

    async def test_failed_business_with_terminal_seal_is_collected_before_run_raises(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent, attempt_changes={"status": "failed"}, native_changes={"status": "failed"})
        await agent.setup(environment)
        context = StubContext()
        with self.assertRaisesRegex(ValueError, "Native reader did not complete"):
            await agent.run(self.instruction, environment, context)
        collection = json.loads((Path(agent.contract["hostOutputRoot"]) / "collection.json").read_bytes())
        self.assertEqual(collection["status"], "failed")
        self.assertTrue(collection["validStart"])
        self.assertEqual(context.metadata["bridge_exit_code"], 2)
        self.assertFalse(context.metadata["runtime_accepted"])
        self.assertEqual(f["phases"]["reader"]["result"]["terminal_status"], "verified-terminal")

    async def test_missing_reader_terminal_retains_result_without_inventing_collection(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        await agent.setup(environment)
        f["phases"]["reader"]["result"].update(terminal_status="unknown-retained", collection_seal=None)
        with self.assertRaisesRegex(ValueError, "terminal|custody"):
            await agent.run(self.instruction, environment, StubContext())
        self.assertTrue((Path(agent.contract["containerOwner"]["phaseEvidenceRoot"]) / "reader.result.json").exists())
        self.assertFalse((Path(agent.contract["hostOutputRoot"]) / "collection.json").exists())
        self.assertTrue(agent._started)

    async def test_duplicate_run_and_instruction_drift_do_not_dispatch(self):
        agent = self.agent()
        environment, f = self.owner_harness(agent)
        await agent.setup(environment)
        with self.assertRaisesRegex(ValueError, "instruction differs"):
            await agent.run(self.instruction + " changed", environment, StubContext())
        self.assertEqual([args[-1] for args, _timeout in f["calls"] if args[0] == "run"], ["preflight"])
        await agent.run(self.instruction, environment, StubContext())
        with self.assertRaisesRegex(ValueError, "pass setup once"):
            await agent.run(self.instruction, environment, StubContext())
        self.assertEqual([args[-1] for args, _timeout in f["calls"] if args[0] == "run"], ["preflight", "reader"])

    def test_exclusive_bounded_writes_preserve_existing_bytes(self):
        path = self.output / "exclusive.json"
        pin = bridge._write_json(path, {"unit": True})
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            bridge._write_json(path, {"unit": False})
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(pin["sha256"], sha(original))
        with self.assertRaisesRegex(ValueError, "finite byte bound"):
            bridge._write_json(self.output / "too-large.json", {"value": "abcdef"}, limit=8)
        self.assertFalse((self.output / "too-large.json").exists())


if __name__ == "__main__":
    unittest.main()
