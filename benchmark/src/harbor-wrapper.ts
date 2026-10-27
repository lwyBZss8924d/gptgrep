import { lstat, readFile, realpath } from "node:fs/promises";
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { closed, readExactJson, verifyPin, type Pin } from "./availability";
import { canonical, digest, fileHash, physicalPath, processCommandIdentity, requireStorage, sha256, within, writeJsonExclusive } from "./io";
import { executeOwnerCommand, type ProcessCommand, type ProcessObservation } from "./process";

export interface HarborContainerOwner {
  schema: "gptgrep.harbor-container-owner.v1";
  interfaceContract: Pin; python: Pin; cli: Pin; template: Pin; derivedPlanRule: Pin;
  parentStorage: { runId: string; workloadSha256: string; cwd: string };
  harborSources: { docker: Pin; trial: Pin; base: Pin; prebuilt: Pin };
  hostEpisodeRoot: string; runtimeContextPath: string; authProjectionPath: string; networkEvidencePath: string;
  namespacePlanReviewPath: string; phaseEvidenceRoot: string; ownerCallTimeoutSeconds: number;
  procEnvironment?: { module: Pin; contract: Pin };
}
export interface HarborOwnerWrapperJob {
  schema: "gptgrep.harbor-owner-wrapper.v1";
  harborJob: Pin;
  bridgeContract: Pin;
  authHarbor: ProcessCommand;
  brokerFinalEvidence: { command: ProcessCommand; path: string };
  outputPath: string;
  timeoutMs: number;
}
type OwnerPhase = "admit" | "preflight" | "reader";
type FinalStage = "container" | "post-broker";
interface PhaseObservation {
  commandId: OwnerPhase;
  runtimeContext: Pin | null;
  request: Pin | null;
  namespacePlan: Pin | null;
  admission: Pin | null;
  result: Pin | null;
  commandPlan: Pin | null;
  startup: Pin | null;
  errors: string[];
}
interface LoadedWrapper { job: HarborOwnerWrapperJob; bridge: any; owner: HarborContainerOwner; template: any; definitions: Record<string, any> }
interface Operation<T> { value: T | null; error: string | null }
const RECORD_BYTES = 64 * 1024, OWNER_RECORD_BYTES = 1024 * 1024, MAX_OUTPUT_BYTES = 256 * 1024;
const hash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const integer = (value: unknown, min = 1): value is number => Number.isSafeInteger(value) && (value as number) >= min;
function need(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
function same(a: unknown, b: unknown, message: string) { need(canonical(a) === canonical(b), message); }
function reason(error: unknown): string { return (error instanceof Error ? error.message : String(error)).slice(0, 2000); }
function path(value: unknown): asserts value is string {
  need(typeof value === "string" && isAbsolute(value) && resolve(value) === value && value !== "/" && basename(value) !== "auth.json", "Exact noncredential artifact path required");
}
function pin(value: any): asserts value is Pin { closed(value, ["path", "sha256"], "wrapper pin"); path(value.path); need(hash(value.sha256), "Exact wrapper SHA-256 required"); }
async function exists(value: string): Promise<boolean> {
  await physicalPath(value);
  try { await lstat(value); return true; } catch (error: any) { if (error.code === "ENOENT") return false; throw error; }
}
async function pinnedJson(value: Pin, maxBytes = OWNER_RECORD_BYTES): Promise<any> {
  pin(value); await physicalPath(value.path);
  const info = await lstat(value.path);
  need(info.isFile() && !info.isSymbolicLink() && info.size <= maxBytes, "Wrapper requires a bounded regular JSON artifact");
  await verifyPin(value); const result = await readExactJson(value.path); await verifyPin(value); return result;
}
async function observedPin(value: string): Promise<Pin> {
  path(value); await physicalPath(value); const info = await lstat(value);
  need(info.isFile() && !info.isSymbolicLink() && info.size <= OWNER_RECORD_BYTES, "Observed owner artifact exceeds its bound");
  return { path: value, sha256: await fileHash(value) };
}

export function expectedHarborEnvironment(template: any, selected?: HarborContainerOwner["procEnvironment"]) {
  if (selected !== undefined) {
    closed(selected, ["module", "contract"], "selected proc environment"); pin(selected.module); pin(selected.contract);
  }
  return { type: selected ? null : "docker", import_path: selected ? "proc_environment:ProcOverlayDockerEnvironment" : null,
    force_build: false, delete: true, stream: false, extra_docker_compose: [template.compose_profile.path],
    mounts: selected ? [] : template.mounts.map((mount: any) => ({ type: "bind", source: mount.host_source, target: mount.guest_target,
      read_only: mount.access === "read", bind: { create_host_path: false } })),
    kwargs: selected ? { enable_environment_dir_upload: false, proc_contract_path: selected.contract.path,
      proc_contract_sha256: selected.contract.sha256 } : { enable_environment_dir_upload: false, auto_mounting: false } };
}

/** Match Docker CLI's JSON whitespace compaction without rewriting any token. */
export function compactProcProfile(raw: string): string {
  const value = JSON.parse(raw); need(value && typeof value === "object" && !Array.isArray(value), "Pinned seccomp source must be a JSON object");
  let quoted = false, escaped = false, result = "";
  for (const char of raw) {
    if (quoted || !" \t\r\n".includes(char)) result += char;
    if (quoted) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') quoted = false;
    } else if (char === '"') quoted = true;
  }
  return result;
}

/** Before Harbor starts, bind the selected create body to the existing owner policy. */
export async function verifyProcEnvironmentSelection(owner: HarborContainerOwner, template: any, environmentDirectory: string): Promise<Pin[]> {
  const selected = owner.procEnvironment;
  if (selected === undefined) return [];
  closed(selected, ["module", "contract"], "selected proc environment"); pin(selected.module); pin(selected.contract);
  await verifyPin(selected.module);
  const value = await pinnedJson(selected.contract);
  closed(value, ["schema", "module", "harborSources", "sessionDerivation", "environmentDirectory", "composeProfile",
    "engineSocket", "engineBinary", "composePlugin", "engineClientConfigRoot", "createRequest"], "proc environment contract");
  need(value.schema === "gptgrep.harbor-proc-environment.v1", "Unknown proc environment contract");
  same(value.module, selected.module, "Selected proc environment module differs");
  closed(value.harborSources, ["docker", "unix", "runtime", "factory", "config"], "proc environment Harbor sources");
  same(value.harborSources.docker, owner.harborSources.docker, "Proc environment selects another Harbor implementation");
  same(value.sessionDerivation, { rule: "harbor-trial-environment-v1", source: owner.harborSources.trial }, "Proc environment changes Harbor session derivation");
  need(value.environmentDirectory === environmentDirectory, "Proc environment selects another task directory");
  same(value.composeProfile, template.compose_profile, "Proc environment selects another Compose profile");
  for (const [field, key] of [["engineSocket", "socket"], ["engineBinary", "binary"], ["composePlugin", "compose_plugin"], ["engineClientConfigRoot", "client_config_root"]]) {
    same(value[field!], template.engine[key!], "Proc environment selects another Engine endpoint or executable");
  }
  const pins: Pin[] = [selected.module, selected.contract, ...Object.values(value.harborSources) as Pin[], value.sessionDerivation.source];
  for (const source of pins) {
    pin(source); await verifyPin(source);
    need(template.host_source_closure.some((entry: Pin) => canonical(entry) === canonical(source)), "Owner source closure omits the selected proc environment");
  }
  const body = value.createRequest, host = body?.HostConfig, policy = template.container_policy;
  need(body?.Image === template.image_id && template.image_pin === template.image_id && body.User === `${template.reader_uid}:${template.reader_gid}`,
    "Proc environment image or user differs from its owner");
  need(template.allowed_paths.some((entry: any) => entry.path === body.WorkingDir && entry.kind === "directory"), "Proc environment cwd is absent from the owner allowlist");
  need(body.Labels?.["com.docker.compose.project"] === "@HARBOR_PROJECT@" && body.Labels?.["com.docker.compose.service"] === "main" &&
    body.Labels?.["com.docker.compose.container-number"] === "1" && body.Labels?.["com.docker.compose.oneoff"] === "False" &&
    hash(body.Labels?.["com.docker.compose.config-hash"]), "Proc environment lacks its bounded Compose selection labels");
  const expectedMounts = template.mounts.map((entry: any) => {
    need(entry.propagation === "rprivate" && entry.create_host_path === false, "Proc environment mount permits undeclared host effects");
    return { Type: "bind", Source: entry.host_source, Target: entry.guest_target, ReadOnly: entry.access === "read", BindOptions: { Propagation: "rprivate" } };
  });
  need(Array.isArray(host?.Mounts), "Proc environment lacks its complete bind set");
  const actualMounts = host.Mounts.map((entry: any) => ({ ...entry, ReadOnly: entry.ReadOnly ?? false }));
  const mountOrder = (a: any, b: any) => a.Target.localeCompare(b.Target);
  same(actualMounts.sort(mountOrder), expectedMounts.sort(mountOrder), "Proc environment bind set differs from its owner");
  for (const [field, key] of Object.entries({ ReadonlyRootfs: "readonly_rootfs", Privileged: "privileged", CapDrop: "cap_drop",
    IpcMode: "ipc_mode", CgroupnsMode: "cgroupns_mode", NetworkMode: "network_mode", SecurityOpt: "security_options",
    MaskedPaths: "masked_paths", ReadonlyPaths: "readonly_paths" })) same(host[field], policy[key], "Proc environment security differs from its owner");
  same(host.MaskedPaths, ["/sys/devices/virtual/powercap", "/sys/firmware"], "Proc environment mask shape differs");
  same(host.ReadonlyPaths, [], "Proc environment readonly-path shape differs");
  for (const [field, key] of Object.entries({ Memory: "memory_bytes", MemorySwap: "docker_memory_swap_bytes", NanoCpus: "nano_cpus", PidsLimit: "pids_limit" })) {
    need(integer(host[field]) && String(host[field]) === policy[key], "Proc environment resources differ from its owner");
  }
  need(policy.seccomp_mode === "explicit-profile" && policy.seccomp_profile && policy.no_new_privileges === true &&
    policy.restart_policy === "no" && policy.log_driver === "none" && policy.cgroup_memory_swap_max === "0" &&
    policy.cpu_period === "100000" && policy.cpu_quota === String(Math.floor(host.NanoCpus / 10_000)), "Proc environment policy differs from its fixed resource controls");
  await pinnedJson(policy.seccomp_profile); // Reject duplicate keys before comparing the original token bytes.
  const rawProfile = await readFile(policy.seccomp_profile.path);
  await verifyPin(policy.seccomp_profile);
  same(host.SecurityOpt, ["no-new-privileges:true", "seccomp=" + compactProcProfile(new TextDecoder("utf-8", { fatal: true }).decode(rawProfile)),
    "apparmor=" + policy.apparmor_profile], "Proc environment inline seccomp differs from its exact pinned source");
  need(Array.isArray(policy.tmpfs) && new Set(policy.tmpfs.map((entry: any) => entry.target)).size === policy.tmpfs.length,
    "Proc environment tmpfs declarations are ambiguous");
  same(host.Tmpfs ?? {}, Object.fromEntries(policy.tmpfs.filter((entry: any) => !["/dev", "/dev/shm"].includes(entry.target))
    .map((entry: any) => [entry.target, entry.options.join(",")])), "Proc environment tmpfs differs from its owner");
  need(policy.tmpfs.find((entry: any) => entry.target === "/dev")?.maximum_bytes === "67108864" &&
    policy.tmpfs.find((entry: any) => entry.target === "/dev/shm")?.maximum_bytes === String(host.ShmSize), "Proc environment shared-memory limit differs");
  return pins;
}

/** Validate the bounded owner records against the supplied frozen interface bundle. */
function ownerSchema(value: any, schema: any, definitions: Record<string, any>, depth = 0): void {
  need(schema && depth < 64, "Invalid owner schema reference");
  if (schema.$ref) {
    need(typeof schema.$ref === "string" && schema.$ref.startsWith("#/$defs/"), "Foreign owner schema reference");
    return ownerSchema(value, definitions[schema.$ref.slice(8)], definitions, depth + 1);
  }
  if (schema.anyOf) {
    need(Array.isArray(schema.anyOf) && schema.anyOf.some((option: any) => { try { ownerSchema(value, option, definitions, depth + 1); return true; } catch { return false; } }), "Owner record does not match its nullable schema");
    return;
  }
  if ("const" in schema) same(value, schema.const, "Owner schema constant differs");
  if (schema.enum) need(schema.enum.some((item: unknown) => canonical(item) === canonical(value)), "Owner schema enum differs");
  if (schema.type === "object") {
    need(value && typeof value === "object" && !Array.isArray(value), "Owner record must be an object");
    for (const key of schema.required ?? []) need(Object.hasOwn(value, key), "Owner record is missing a required field");
    for (const key of Object.keys(value)) {
      if (schema.properties?.[key]) ownerSchema(value[key], schema.properties[key], definitions, depth + 1);
      else need(schema.additionalProperties !== false, "Owner record has an unknown field");
    }
  } else if (schema.type === "array") {
    need(Array.isArray(value) && value.length >= (schema.minItems ?? 0) && value.length <= (schema.maxItems ?? Number.MAX_SAFE_INTEGER), "Owner array exceeds its bound");
    for (const item of value) if (schema.items) ownerSchema(item, schema.items, definitions, depth + 1);
  } else if (schema.type === "string") {
    need(typeof value === "string" && value.length >= (schema.minLength ?? 0) && value.length <= (schema.maxLength ?? Number.MAX_SAFE_INTEGER) &&
      (!schema.pattern || new RegExp(schema.pattern).test(value)), "Owner text differs from its schema");
  } else if (schema.type === "integer" || schema.type === "number") {
    need(typeof value === "number" && Number.isFinite(value) && (schema.type !== "integer" || Number.isSafeInteger(value)) &&
      value >= (schema.minimum ?? -Infinity) && value <= (schema.maximum ?? Infinity), "Owner number differs from its schema");
  } else if (schema.type === "boolean") need(typeof value === "boolean", "Owner boolean required");
  else if (schema.type === "null") need(value === null, "Owner null required");
}
async function ownerRecord(value: Pin, definition: string, definitions: Record<string, any>): Promise<any> {
  const result = await pinnedJson(value); ownerSchema(result, definitions[definition], definitions); return result;
}
function commandShape(command: ProcessCommand): void {
  closed(command, ["executable", "inputs", "argv", "cwd", "timeoutMs", "maxOutputBytes"], "wrapper process command"); pin(command.executable); path(command.cwd);
  need(Array.isArray(command.inputs) && command.inputs.length > 0 && command.inputs.length <= 128 &&
    Array.isArray(command.argv) && command.argv.length > 0 && command.argv.length <= 256 && command.argv.every(arg => typeof arg === "string" && !arg.includes("\0")) &&
    integer(command.timeoutMs) && integer(command.maxOutputBytes) && command.maxOutputBytes <= 8 * 1024 * 1024, "Unbounded wrapper process command");
  for (const value of command.inputs) pin(value);
  need(new Set(command.inputs.map(value => value.path)).size === command.inputs.length, "Duplicate wrapper command input pin");
}
function containsPin(command: ProcessCommand, value: Pin) { return command.inputs.some(item => canonical(item) === canonical(value)); }

function mappedSource(template: any, value: Pin, readOnly = false): Pin {
  pin(value);
  need(Array.isArray(template.mounts) && template.mounts.length > 0 && template.mounts.length <= 32, "Reader source bind mapping is unavailable");
  const mounts = template.mounts.filter((mount: any) => mount.kind === "file" ? mount.guest_target === value.path :
    mount.kind === "directory" && within(mount.guest_target, value.path)).sort((a: any, b: any) => b.guest_target.length - a.guest_target.length);
  need(mounts.length && (mounts.length === 1 || mounts[0].guest_target !== mounts[1].guest_target), "Reader source has no unambiguous bind mapping");
  const mount = mounts[0]; path(mount.host_source); path(mount.guest_target);
  need(mount.propagation === "rprivate" && mount.create_host_path === false && (!readOnly || mount.access === "read"), "Reader source lacks its required read-only bind");
  return { path: mount.kind === "file" ? mount.host_source : resolve(mount.host_source, relative(mount.guest_target, value.path)), sha256: value.sha256 };
}

/** Guest source pins resolve through frozen binds; the job is materialized later by the owner. */
export async function verifyReaderCommandSources(template: any, bridge: any, command: any): Promise<void> {
  for (const value of [bridge.bun, bridge.bunConfig, bridge.entrypoint, bridge.readerJob]) pin(value);
  need(bridge.bunConfig.sha256 === sha256(""), "Reader Bun configuration must be empty");
  const config = mappedSource(template, bridge.bunConfig, true); await physicalPath(config.path); await verifyPin(config);
  need((await lstat(config.path)).isFile() && (await lstat(config.path)).size === 0, "Reader Bun configuration must be an empty regular file");
  same(command.controller_argv, [bridge.bun.path, "--no-env-file", "--no-install", `--config=${bridge.bunConfig.path}`, bridge.entrypoint.path,
    "--mode", command.id, "--job", bridge.readerJob.path, "--expect-sha256", bridge.readerJob.sha256,
    "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"], "Owner reader argv differs from the guarded detached immutable job");
  need(Array.isArray(command.source_pins) && [bridge.bun, bridge.bunConfig, bridge.entrypoint, bridge.readerJob]
    .every(value => command.source_pins.some((source: Pin) => canonical(source) === canonical(value))), "Reader source closure omits a guarded runtime input");
  for (const value of command.source_pins) {
    pin(value);
    const host = value.path === command.immutable_job_guest_path ? command.immutable_job : mappedSource(template, value);
    need(host.sha256 === value.sha256, "Materialized reader source differs from its host input");
    await physicalPath(host.path); await verifyPin(host);
  }
}

export function harborWrapperBudget(job: HarborOwnerWrapperJob, owner: HarborContainerOwner) {
  return job.authHarbor.timeoutMs + 2000 + job.brokerFinalEvidence.command.timeoutMs + 2000 + 2 * (owner.ownerCallTimeoutSeconds * 1000 + 2000);
}

/** Static validation does not stand in for the actual admitted host process check. */
export async function validateHarborWrapperJob(job: HarborOwnerWrapperJob): Promise<LoadedWrapper> {
  closed(job, ["schema", "harborJob", "bridgeContract", "authHarbor", "brokerFinalEvidence", "outputPath", "timeoutMs"], "Harbor owner wrapper job");
  need(job.schema === "gptgrep.harbor-owner-wrapper.v1" && integer(job.timeoutMs) && job.timeoutMs <= 900_000, "Invalid bounded Harbor wrapper job");
  pin(job.harborJob); pin(job.bridgeContract); path(job.outputPath); commandShape(job.authHarbor);
  closed(job.brokerFinalEvidence, ["command", "path"], "broker final evidence producer"); path(job.brokerFinalEvidence.path); commandShape(job.brokerFinalEvidence.command);
  const bridge = await pinnedJson(job.bridgeContract), harbor = await pinnedJson(job.harborJob), owner: HarborContainerOwner = bridge.containerOwner;
  closed(bridge, ["schema", "taskId", "arm", "instructionSha256", "model", "effort", "serviceTier", "timeoutSeconds", "cwd", "outputRoot", "bun", "bunConfig", "entrypoint", "readerJob",
    "containerOwner", "authRunId", "maxArtifactBytes", "hostOutputRoot", "agentModule", "maxCollectedBytes", "maxTraceBytes", "maxTrajectoryBytes", "maxMediaBytes"], "native Harbor bridge");
  need(bridge.schema === "gptgrep.harbor-native-bridge.v2" && ["bare", "gptgrep"].includes(bridge.arm), "Wrapper requires the reviewed native container bridge");
  closed(owner, ["schema", "interfaceContract", "python", "cli", "template", "parentStorage", "harborSources", "hostEpisodeRoot", "runtimeContextPath", "authProjectionPath",
    "networkEvidencePath", "namespacePlanReviewPath", "phaseEvidenceRoot", "ownerCallTimeoutSeconds", "derivedPlanRule",
    ...(owner && Object.hasOwn(owner, "procEnvironment") ? ["procEnvironment"] : [])], "Harbor container owner");
  need(owner.schema === "gptgrep.harbor-container-owner.v1" && integer(owner.ownerCallTimeoutSeconds) && owner.ownerCallTimeoutSeconds <= 300, "Invalid container owner route");
  closed(owner.parentStorage, ["runId", "workloadSha256", "cwd"], "static parent storage identity");
  need(typeof owner.parentStorage.runId === "string" && owner.parentStorage.runId.length > 0 && hash(owner.parentStorage.workloadSha256), "Invalid static parent storage identity"); path(owner.parentStorage.cwd);
  closed(owner.harborSources, ["docker", "trial", "base", "prebuilt"], "Harbor source closure");
  for (const key of ["hostEpisodeRoot", "runtimeContextPath", "authProjectionPath", "networkEvidencePath", "namespacePlanReviewPath", "phaseEvidenceRoot"] as const) path(owner[key]);
  for (const value of [job.outputPath, job.brokerFinalEvidence.path, owner.runtimeContextPath, owner.phaseEvidenceRoot]) need(within(owner.hostEpisodeRoot, value), "Wrapper outputs escaped their exact episode root");
  need(job.outputPath !== job.brokerFinalEvidence.path && harborWrapperBudget(job, owner) <= job.timeoutMs, "Wrapper deadline omits finalization or output paths overlap");
  for (const value of [owner.interfaceContract, owner.python, owner.cli, owner.template, owner.derivedPlanRule, bridge.agentModule, ...Object.values(owner.harborSources),
    job.authHarbor.executable, ...job.authHarbor.inputs, job.brokerFinalEvidence.command.executable, ...job.brokerFinalEvidence.command.inputs]) { pin(value); await verifyPin(value); }
  const contract = await pinnedJson(owner.interfaceContract);
  need(contract.schema === "environments-harness.container-storage-design-contract.v1" &&
    ["interface-contract-v6.1-frozen-for-source-preparation", "interface-contract-v6.2-frozen-for-source-preparation",
      "interface-contract-v6.3-frozen-for-source-preparation",
      "interface-contract-v6.4-frozen-for-source-preparation"].includes(contract.status) && contract.schema_bundle?.$defs,
    "Exact reviewed v6.1, v6.2, v6.3 or v6.4 owner interface is required");
  const definitions = contract.schema_bundle.$defs, template = await ownerRecord(owner.template, "ownerTemplate", definitions);
  need(template.admission_class === "reader" && template.auth_supervisor_source && template.sandbox_privacy_qualification &&
    template.experiment_arm === (bridge.arm === "bare" ? "baseline" : "treatment"), "Owner template does not admit this reader arm");
  same(template.harbor_source, owner.harborSources.docker, "Owner template names another Harbor source");
  if (["interface-contract-v6.3-frozen-for-source-preparation",
    "interface-contract-v6.4-frozen-for-source-preparation"].includes(contract.status)) {
    same(template.harbor_derivation_source, owner.harborSources.trial, "Owner template names another Harbor trial derivation source");
  }
  need(template.outputs.runtime_context === owner.runtimeContextPath && template.outputs.namespace_plan_review === owner.namespacePlanReviewPath, "Owner output paths differ from the bridge");
  for (const value of [template.auth_supervisor_source, template.sandbox_privacy_qualification, ...template.host_source_closure]) { pin(value); await verifyPin(value); }
  need(Array.isArray(template.commands) && template.commands.length === 2 && canonical(template.commands.map((value: any) => value.id).sort()) === canonical(["preflight", "reader"]), "Owner template must freeze exactly preflight and reader");
  for (const command of template.commands) {
    need(command.immutable_job_guest_path === bridge.readerJob.path && command.immutable_job.sha256 === bridge.readerJob.sha256 && command.cwd === bridge.cwd, "Owner reader job mapping differs");
    need(within(owner.hostEpisodeRoot, command.host_artifact_root), "Owner phase artifacts escaped the episode root");
    for (const value of [command.workload, command.immutable_job, command.acceptance_template]) { pin(value); await verifyPin(value); }
    await verifyReaderCommandSources(template, bridge, command);
  }
  const rule = await ownerRecord(owner.derivedPlanRule, "derivedPlanRule", definitions);
  same(rule.owner_template, owner.template, "Derived plan authority selects another owner template");
  const procPins = await verifyProcEnvironmentSelection(owner, template, rule.fixed_runtime_context.compose_project_directory);
  for (const value of procPins) need(containsPin(job.authHarbor, value), "Auth Harbor command omits the selected proc environment source");
  same(harbor.environment, expectedHarborEnvironment(template, owner.procEnvironment), "Harbor environment differs from its exact selected owner route");
  for (const value of [rule.authority, ...rule.scenario_source_pins]) { pin(value); await verifyPin(value); }
  need(containsPin(job.authHarbor, job.harborJob) && containsPin(job.authHarbor, job.bridgeContract) && containsPin(job.authHarbor, template.auth_supervisor_source) &&
    (job.authHarbor.argv.includes(template.auth_supervisor_source.path) || job.authHarbor.executable.path === template.auth_supervisor_source.path) &&
    canonical(job.authHarbor.argv.slice(-3)) === canonical(["run", "--config", job.harborJob.path]), "Auth Harbor command lacks its exact frozen job, bridge or existing broker source");
  const separator = job.authHarbor.argv.indexOf("--"), brokerArgs = job.authHarbor.argv.slice(0, separator);
  const selected = (flag: string) => brokerArgs.filter(value => value === flag).length === 1 ? brokerArgs[brokerArgs.indexOf(flag) + 1] : null;
  need(separator > 0 && job.authHarbor.argv.lastIndexOf("--") === separator && selected("--run-id") === bridge.authRunId && selected("--lease") === "exclusive" &&
    selected("--pre-refresh") === "never" && selected("--wait-seconds") === "0", "Wrapper must retain the selected exclusive native auth broker route");
  const brokerConfig = selected("--config"); need(brokerConfig && job.authHarbor.inputs.some(value => value.path === brokerConfig), "Auth broker configuration is not pinned");
  need(harbor.n_attempts === 1 && harbor.n_concurrent_trials === 1 && harbor.retry?.max_retries === 0 && harbor.agents?.length === 1 &&
    harbor.agents[0].import_path === "native_agent:NativeCodexAgent" && canonical(harbor.agents[0].kwargs) === canonical({ bridge_contract_path: job.bridgeContract.path, bridge_contract_sha256: job.bridgeContract.sha256 }),
    "Harbor job is outside the frozen native bridge route");
  return { job, bridge, owner, template, definitions };
}

export function makeHarborTransport(options: { ownerTemplate: Pin; runtimeContext: Pin | null; commandId: OwnerPhase; expectedAdmissionPath: string;
  commandPlan: Pin | null; observation: ProcessObservation | null; observerSourceSha256: string }) {
  pin(options.ownerTemplate); path(options.expectedAdmissionPath); need(hash(options.observerSourceSha256) && ["admit", "preflight", "reader"].includes(options.commandId), "Invalid wrapper transport identity");
  if (options.runtimeContext) pin(options.runtimeContext); if (options.commandPlan) pin(options.commandPlan);
  const observed = options.observation;
  if (observed) need(integer(observed.stdoutBytes, 0) && integer(observed.stderrBytes, 0) && hash(observed.stdoutSha256) && hash(observed.stderrSha256), "Actual stream observations are required");
  return { schema: "environments-harness.container-storage-transport.v1", owner_template_sha256: options.ownerTemplate.sha256,
    runtime_context_sha256: options.runtimeContext?.sha256 ?? null, command_id: options.commandId, expected_admission_path: options.expectedAdmissionPath,
    command_plan_sha256: options.commandPlan?.sha256 ?? null, observed_returncode: observed?.exitCode ?? null, observed_signal: observed?.signal ?? null,
    observed_timeout: observed?.timedOut ?? false, observed_cancelled: false,
    result: !observed ? "not-started" : observed.timedOut ? "timeout" : observed.executionError || observed.outputExceeded ? "error" :
      !observed.cleanup || observed.exitCode === null && observed.signal === null ? "lost" : "returned",
    stdout_bytes: String(observed?.stdoutBytes ?? 0), stdout_sha256: observed?.stdoutSha256 ?? sha256(""),
    stderr_bytes: String(observed?.stderrBytes ?? 0), stderr_sha256: observed?.stderrSha256 ?? sha256(""), observer_source_sha256: options.observerSourceSha256,
    wrapper_argv_sha256: null, context_status: options.runtimeContext ? "observed" : "unavailable-before-setup" };
}

export function buildHarborFinalizeCommand(options: { owner: HarborContainerOwner; template: any; parentBinding: Pin; runtimeContext: Pin | null;
  commandId: OwnerPhase; transport: Pin; stage: FinalStage; brokerFinalEvidence: Pin | null }): ProcessCommand {
  const { owner, template } = options;
  for (const value of [owner.python, owner.cli, owner.template, options.parentBinding, options.transport]) pin(value);
  need(options.stage === "container" ? options.brokerFinalEvidence === null : options.brokerFinalEvidence !== null, "Post-broker finalization requires actual broker evidence");
  if (options.runtimeContext) { pin(options.runtimeContext); need(options.runtimeContext.path === owner.runtimeContextPath, "Finalization context path differs"); }
  if (options.brokerFinalEvidence) pin(options.brokerFinalEvidence);
  const argv = ["-I", "-B", owner.cli.path, "storage-container", "finalize", "--template", owner.template.path, "--expect-template-digest", owner.template.sha256,
    "--runtime-context", owner.runtimeContextPath, ...(options.runtimeContext ? ["--expect-context-digest", options.runtimeContext.sha256] : ["--context-unavailable"]),
    "--admission-path", template.outputs.admission, "--command-id", options.commandId, "--transport-result", options.transport.path, "--expect-transport-digest", options.transport.sha256,
    "--stage", options.stage, "--parent-binding", options.parentBinding.path, "--expect-parent-digest", options.parentBinding.sha256,
    ...(options.brokerFinalEvidence ? ["--broker-final-evidence", options.brokerFinalEvidence.path, "--expect-broker-digest", options.brokerFinalEvidence.sha256] : [])];
  const inputs = [owner.interfaceContract, owner.cli, owner.template, options.parentBinding, options.transport, ...template.host_source_closure,
    ...(options.runtimeContext ? [options.runtimeContext] : []), ...(options.brokerFinalEvidence ? [options.brokerFinalEvidence] : [])];
  const unique = new Map<string, Pin>();
  for (const value of inputs) { pin(value); const prior = unique.get(value.path); need(!prior || prior.sha256 === value.sha256, "Conflicting finalizer source pins"); unique.set(value.path, value); }
  return { executable: owner.python, inputs: [...unique.values()], argv, cwd: owner.parentStorage.cwd, timeoutMs: owner.ownerCallTimeoutSeconds * 1000, maxOutputBytes: OWNER_RECORD_BYTES };
}

/** The callbacks are also a source-level seam for checking finally ordering without launching commands. */
export async function runHarborFinalizationSequence<T>(operations: { authHarbor(): Promise<T>; container(): Promise<T>; brokerEvidence(): Promise<T>; postBroker(): Promise<T> }) {
  const settle = async (operation: () => Promise<T>): Promise<Operation<T>> => { try { return { value: await operation(), error: null }; } catch (error) { return { value: null, error: reason(error) }; } };
  let authHarbor: Operation<T> = { value: null, error: null }, container = authHarbor, brokerEvidence = authHarbor, postBroker = authHarbor;
  try { authHarbor = await settle(operations.authHarbor); }
  finally {
    try { container = await settle(operations.container); }
    finally { try { brokerEvidence = await settle(operations.brokerEvidence); } finally { postBroker = await settle(operations.postBroker); } }
  }
  return { authHarbor, container, brokerEvidence, postBroker };
}

async function observePhase(loaded: LoadedWrapper, parentBinding: Pin): Promise<PhaseObservation> {
  const { owner, template, definitions } = loaded;
  const phase: PhaseObservation = { commandId: "admit", runtimeContext: null, request: null, namespacePlan: null, admission: null, result: null, commandPlan: null, startup: null, errors: [] };
  try {
    if (await exists(owner.runtimeContextPath)) {
      phase.runtimeContext = await observedPin(owner.runtimeContextPath);
      const runtime = await ownerRecord(phase.runtimeContext, "runtimeContext", definitions);
      need(runtime.episode_id === template.episode_id && runtime.host_episode_root === owner.hostEpisodeRoot && canonical(runtime.harbor_source) === canonical(owner.harborSources.docker), "Runtime context belongs to another Harbor episode");
    }
  } catch (error) { phase.errors.push(reason(error)); }
  for (const commandId of ["admit", "preflight", "reader"] as const) {
    try {
      const requestPath = join(owner.phaseEvidenceRoot, `${commandId}.requested.json`);
      if (!await exists(requestPath)) continue;
      // Filename scope records which phase was requested; malformed contents never supply input pins.
      phase.commandId = commandId; phase.request = await observedPin(requestPath); phase.commandPlan = null; phase.startup = null; phase.result = null;
      const request = await pinnedJson(phase.request);
      closed(request, ["schema", "commandId", "runtimeContext", "namespacePlan", "admission"], "owner phase request");
      need(request.schema === "gptgrep.harbor-owner-phase-request.v1" && request.commandId === commandId && phase.runtimeContext !== null, "Malformed owner phase request");
      same(request.runtimeContext, phase.runtimeContext, "Phase request names another runtime context");
      need(request.namespacePlan.path === template.outputs.namespace_plan, "Phase namespace path differs from its owner output");
      const namespace = await ownerRecord(request.namespacePlan, "namespacePlan", definitions);
      same(namespace.owner_template, owner.template, "Phase namespace template differs"); same(namespace.runtime_context, phase.runtimeContext, "Phase namespace context differs");
      phase.namespacePlan = request.namespacePlan;
      if (commandId === "admit") { need(request.admission === null, "Admission request cannot contain its future admission"); continue; }
      need(request.admission?.path === template.outputs.admission, "Guest phase lacks its exact admission path");
      const admission = await ownerRecord(request.admission, "admission", definitions);
      same(admission.owner_template, owner.template, "Guest admission template differs"); same(admission.runtime_context, phase.runtimeContext, "Guest admission context differs");
      same(admission.parent_host_binding, parentBinding, "Guest admission parent differs"); same(admission.namespace_plan, request.namespacePlan, "Guest admission namespace differs"); phase.admission = request.admission;
      const resultPath = join(owner.phaseEvidenceRoot, `${commandId}.result.json`);
      if (!await exists(resultPath)) continue;
      const resultPin = await observedPin(resultPath), result = await ownerRecord(resultPin, "runResult", definitions);
      need(result.command_id === commandId && result.owner_template_sha256 === owner.template.sha256 && result.admission_sha256 === request.admission.sha256, "Owner phase result belongs to another dispatch");
      phase.result = resultPin;
      if (result.command_plan) {
        const frozen = template.commands.find((value: any) => value.id === commandId);
        need(frozen && within(frozen.host_artifact_root, result.command_plan.path), "Owner command plan escaped its phase artifact root");
        const plan = await ownerRecord(result.command_plan, "commandPlan", definitions);
        need(plan.command_id === commandId, "Owner command plan selects another phase"); same(plan.owner_template, owner.template, "Owner command plan template differs");
        same(plan.admission, request.admission, "Owner command plan admission differs"); same(plan.container_binding, admission.container_binding, "Owner command plan namespace differs");
        phase.commandPlan = result.command_plan;
        if (result.startup) {
          need(within(frozen.host_artifact_root, result.startup.path), "Owner startup escaped its phase artifact root");
          const startup = await ownerRecord(result.startup, "execStartup", definitions);
          need(startup.command_id === commandId && startup.command_plan_sha256 === result.command_plan.sha256 && startup.parent_host_binding_sha256 === parentBinding.sha256 &&
            startup.command_binding_sha256 === plan.command_binding.sha256 && startup.container_binding_sha256 === admission.container_binding.sha256 && startup.container_id === admission.container.id,
            "Owner startup belongs to another command or namespace"); phase.startup = result.startup;
        }
      } else need(result.startup === null, "Owner startup has no command plan identity");
    } catch (error) { phase.errors.push(reason(error)); }
  }
  return phase;
}

/** Actual outer wrapper: the parent storage worker remains alive through both owner finalization stages. */
export async function runHarborOwnerWrapper(jobPin: Pin) {
  const job = await pinnedJson(jobPin), loaded = await validateHarborWrapperJob(job), { owner, template, definitions } = loaded;
  const files = { start: `${job.outputPath}.start.json`, auth: `${job.outputPath}.auth-process.json`, transport: `${job.outputPath}.transport.json`,
    broker: `${job.outputPath}.broker-process.json`, container: `${job.outputPath}.container-finalize-process.json`, postBroker: `${job.outputPath}.post-broker-finalize-process.json` };
  const admission = await requireStorage(job.outputPath, ...Object.values(files), job.brokerFinalEvidence.path, owner.runtimeContextPath, owner.phaseEvidenceRoot,
    template.outputs.finalization, template.outputs.post_broker_finalization);
  need(!admission.container, "Harbor wrapper must run in the actual admitted host worker");
  const parentBinding = { path: process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING!, sha256: process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST! };
  need(parentBinding.path === admission.binding && parentBinding.sha256 === admission.sha256, "Parent binding must retain its exact physical environment pin");
  const binding = await pinnedJson(parentBinding), plan = binding.plan;
  const observed = await processCommandIdentity();
  need(plan?.run_id === owner.parentStorage.runId && plan.workload_sha256 === owner.parentStorage.workloadSha256 && plan.cwd === owner.parentStorage.cwd &&
    plan.cwd === await realpath(process.cwd()) && plan.command?.argv_sha256 === observed.argvSha256 && plan.command.executable === observed.executable &&
    plan.command.executable_sha256 === await fileHash(observed.executable), "Wrapper is outside its exact host workload, argv or executable binding");
  const selected = (flag: string) => process.argv.filter(value => value === flag).length === 1 ? process.argv[process.argv.indexOf(flag) + 1] : null;
  need(selected("--mode") === "harbor" && selected("--job") === jobPin.path && selected("--expect-sha256") === jobPin.sha256, "Actual wrapper argv does not select this immutable job");
  for (const value of [job.outputPath, ...Object.values(files), job.brokerFinalEvidence.path, owner.runtimeContextPath, template.outputs.namespace_plan, template.outputs.admission,
    template.outputs.finalization, template.outputs.post_broker_finalization, ...["admit", "preflight", "reader"].flatMap(mode => [join(owner.phaseEvidenceRoot, `${mode}.requested.json`), join(owner.phaseEvidenceRoot, `${mode}.result.json`)])])
    need(!await exists(value), "Wrapper refuses retained artifacts or an automatic retry");
  const startedAt = new Date().toISOString(), started = Date.now(), observerSourceSha256 = await fileHash(import.meta.path);
  const write = async (output: string, value: unknown, limit = RECORD_BYTES): Promise<Pin> => {
    await requireStorage(output); await writeJsonExclusive(output, value, limit); return { path: output, sha256: await fileHash(output) };
  };
  await write(files.start, { schema: "gptgrep.harbor-owner-wrapper-start.v1", job: jobPin, parentHostBinding: parentBinding, startedAt,
    observerSourceSha256, plannedTimeoutMs: harborWrapperBudget(job, owner), timeoutMs: job.timeoutMs });
  let authObservation: ProcessObservation | null = null, authPin: Pin | null = null, transport: Pin | null = null, phase: PhaseObservation | null = null;
  let brokerObservation: ProcessObservation | null = null, brokerProcess: Pin | null = null, brokerEvidence: Pin | null = null;
  const finals: Record<FinalStage, { process: Pin | null; receipt: Pin | null; observation: ProcessObservation | null }> = {
    container: { process: null, receipt: null, observation: null }, "post-broker": { process: null, receipt: null, observation: null } };
  const finalizer = async (stage: FinalStage) => {
    need(phase && transport, "Finalization transport could not be retained");
    const command = buildHarborFinalizeCommand({ owner, template, parentBinding, runtimeContext: phase.runtimeContext, commandId: phase.commandId, transport, stage, brokerFinalEvidence: stage === "post-broker" ? brokerEvidence : null });
    const observed = await executeOwnerCommand(command, command.timeoutMs + 2000); finals[stage].observation = observed;
    finals[stage].process = await write(stage === "container" ? files.container : files.postBroker, { schema: "gptgrep.harbor-owner-process.v1", stage, commandSha256: digest(command), observation: observed });
    const output = stage === "container" ? template.outputs.finalization : template.outputs.post_broker_finalization;
    const finalPin = await observedPin(output), final = await ownerRecord(finalPin, "finalization", definitions);
    need(final.stage === stage && final.owner_template_sha256 === owner.template.sha256 && final.runtime_context_sha256 === (phase.runtimeContext?.sha256 ?? null) &&
      final.context_status === (phase.runtimeContext ? "observed" : "unavailable-before-setup") && final.command_id === phase.commandId && final.expected_admission_path === template.outputs.admission &&
      final.parent_host_binding_sha256 === parentBinding.sha256 && final.command_plan_sha256 === (phase.commandPlan?.sha256 ?? null), "Owner finalization belongs to another wrapper dispatch");
    same(final.transport, transport, "Owner finalization transport differs");
    if (stage === "post-broker") same(final.broker_final_evidence, brokerEvidence, "Post-broker finalization evidence differs");
    finals[stage].receipt = finalPin; return finalPin;
  };
  const sequence = await runHarborFinalizationSequence<Pin | null>({
    authHarbor: async () => {
      authObservation = await executeOwnerCommand(job.authHarbor, job.authHarbor.timeoutMs + 2000);
      authPin = await write(files.auth, { schema: "gptgrep.harbor-owner-process.v1", stage: "auth-harbor", commandSha256: digest(job.authHarbor), observation: authObservation }); return authPin;
    },
    container: async () => {
      phase = await observePhase(loaded, parentBinding);
      const value = makeHarborTransport({ ownerTemplate: owner.template, runtimeContext: phase.runtimeContext, commandId: phase.commandId, expectedAdmissionPath: template.outputs.admission,
        commandPlan: phase.commandPlan, observation: authObservation, observerSourceSha256 });
      ownerSchema(value, definitions.transport, definitions); transport = await write(files.transport, value); return finalizer("container");
    },
    brokerEvidence: async () => {
      brokerObservation = await executeOwnerCommand(job.brokerFinalEvidence.command, job.brokerFinalEvidence.command.timeoutMs + 2000);
      brokerProcess = await write(files.broker, { schema: "gptgrep.harbor-owner-process.v1", stage: "broker-evidence", commandSha256: digest(job.brokerFinalEvidence.command), observation: brokerObservation });
      need(brokerObservation.started && brokerObservation.exitCode === 0 && !brokerObservation.executionError && !brokerObservation.timedOut && brokerObservation.cleanup && !brokerObservation.outputExceeded,
        "Broker evidence producer failed; actual cleanup remains unknown");
      const evidencePin = await observedPin(job.brokerFinalEvidence.path), evidence = await ownerRecord(evidencePin, "brokerFinalEvidence", definitions);
      same(evidence.broker_source, template.auth_supervisor_source, "Broker final evidence belongs to another auth source");
      const receipt = await pinnedJson(evidence.run_receipt); need(receipt.run_id === loaded.bridge.authRunId, "Broker final receipt belongs to another auth run");
      pin(evidence.status_receipt); await verifyPin(evidence.status_receipt); brokerEvidence = evidencePin; return brokerEvidence;
    },
    postBroker: () => finalizer("post-broker"),
  });
  const selectedPhase = phase as PhaseObservation | null;
  const errors = [...Object.values(sequence).flatMap(value => value.error ? [value.error] : []), ...(selectedPhase?.errors ?? [])];
  const post = finals["post-broker"].receipt ? await pinnedJson(finals["post-broker"].receipt!) : null;
  const clean = (value: ProcessObservation | null) => value && !value.executionError && !value.timedOut && value.cleanup && !value.outputExceeded && value.exitCode !== null && value.signal === null;
  const elapsedMs = Date.now() - started;
  if (elapsedMs > job.timeoutMs) errors.push("Wrapper exceeded its frozen total deadline");
  const reconciled = errors.length === 0 && clean(authObservation) && clean(brokerObservation) && clean(finals.container.observation) && clean(finals["post-broker"].observation) &&
    post?.terminal_status === "verified-terminal" && post.reservation_status === "released-after-terminal-proof";
  const result = { schema: "gptgrep.harbor-owner-wrapper-result.v1", job: jobPin, bridgeContract: job.bridgeContract, parentHostBinding: parentBinding,
    startedAt, completedAt: new Date().toISOString(), elapsedMs, timeoutMs: job.timeoutMs, status: reconciled ? "reconciled" : "unknown",
    authHarborProcess: authPin, phase: selectedPhase, transport, containerFinalization: finals.container, brokerEvidenceProcess: brokerProcess, brokerFinalEvidence: brokerEvidence,
    postBrokerFinalization: finals["post-broker"], businessStatus: post?.business_status ?? "not-established", errors, liveQualified: false };
  await write(job.outputPath, result, MAX_OUTPUT_BYTES);
  return { ...result, result: { path: job.outputPath, sha256: await fileHash(job.outputPath) } };
}
