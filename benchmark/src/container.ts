import { lstat, readFile, realpath } from "node:fs/promises";
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { readExactJson, verifyPin, type Pin } from "./availability";
import { canonical, digest, fileHash, physicalPath, within } from "./io";

export interface ContainerPhaseEvidence { result: Pin; commandPlan: Pin; startup: Pin }
export interface ContainerReaderChainOptions {
  ownerTemplate: Pin;
  runtimeContext: Pin;
  namespacePlanReview: Pin;
  derivedPlanRule: Pin;
  parentHostBinding: Pin;
  admission: Pin;
  preflight: ContainerPhaseEvidence;
  reader: ContainerPhaseEvidence;
  finalization: Pin;
  parentHostBindingSha256: string;
  readerJob: Pin;
  taskId: string;
  arm: "bare" | "gptgrep";
  sourceAttempt: Pin;
  sourceManifest: Pin;
  projectedOutputRoot: string;
}
export interface SealedContainerArtifact {
  guest_path: string;
  host_path: string;
  host_identity: { device: string; inode: string; uid: number; gid: number; mode: string };
  sha256: string;
  bytes: string;
  purpose: string;
}

const hash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const decimal = (value: unknown): value is string => typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value);
function need(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
function same(actual: unknown, expected: unknown, message: string): void { need(canonical(actual) === canonical(expected), message); }
function exactPath(value: unknown): asserts value is string {
  need(typeof value === "string" && isAbsolute(value) && resolve(value) === value && value !== "/", "Exact bounded physical path required");
}
function safePin(pin: Pin): void {
  need(pin && Object.keys(pin).sort().join(",") === "path,sha256" && hash(pin.sha256), "Exact container evidence pin required");
  exactPath(pin.path);
  // Credential contents are never part of this evidence graph.
  need(basename(pin.path) !== "auth.json", "Credential files cannot be container evidence");
}
async function read(pin: Pin, schema?: string): Promise<any> {
  safePin(pin); await verifyPin(pin);
  const info = await lstat(pin.path);
  need(info.isFile() && info.size <= 4 * 1024 * 1024, "Container evidence record exceeds its bound");
  const value = await readExactJson(pin.path); await verifyPin(pin);
  if (schema) need(value.schema === schema, `Unexpected container evidence schema: ${schema}`);
  return value;
}
async function identity(path: string, expected: any, directory: boolean): Promise<void> {
  exactPath(path); await physicalPath(path);
  need(await realpath(path) === path, "Sealed original is not its physical host path");
  const info = await lstat(path, { bigint: true });
  need(directory ? info.isDirectory() : info.isFile() && info.nlink === 1n, "Sealed original changed file kind or gained a hard link");
  same({ device: String(info.dev), inode: String(info.ino), uid: Number(info.uid), gid: Number(info.gid), mode: `0${Number(info.mode & 0o777n).toString(8).padStart(3, "0")}` }, expected,
    "Sealed original host identity changed");
}
function mapGuest(base: any, path: string): string {
  exactPath(path);
  need(Array.isArray(base.mounts) && base.mounts.length > 0 && base.mounts.length <= 32, "Container mount mapping is unavailable");
  const mounts = base.mounts.map((row: any) => row.template).filter((row: any) => row && (row.kind === "file" ? row.guest_target === path : row.kind === "directory" && within(row.guest_target, path)))
    .sort((a: any, b: any) => b.guest_target.length - a.guest_target.length);
  need(mounts.length && (mounts.length === 1 || mounts[0].guest_target !== mounts[1].guest_target), "Guest evidence has no unambiguous owner mount mapping");
  const mount = mounts[0]; exactPath(mount.host_source); exactPath(mount.guest_target);
  need(mount.propagation === "rprivate" && mount.create_host_path === false, "Guest evidence mapping is not the admitted bind");
  return mount.kind === "file" ? mount.host_source : resolve(mount.host_source, relative(mount.guest_target, path));
}
function guestPin(base: any, pin: Pin): Pin { safePin(pin); return { path: mapGuest(base, pin.path), sha256: pin.sha256 }; }
function ancestry(value: any, context: any): void {
  for (const key of ["owner_template_sha256", "host_namespace_plan_sha256", "parent_host_binding_sha256"]) same(value[key], context[key], "Container evidence ancestry differs");
}
function stableContainer(value: any): any {
  const stable = structuredClone(value), resources = stable.security?.resources;
  need(resources && decimal(resources.host_mem_available_bytes) && hash(resources.host_observation_sha256), "Container capacity telemetry is unavailable");
  // The owner permits only these current host-capacity observations to vary.
  delete resources.host_mem_available_bytes; delete resources.host_observation_sha256;
  return stable;
}
function sameContainer(actual: any, expected: any, message: string): void { same(stableContainer(actual), stableContainer(expected), message); }
async function filesystemPolicy(pin: Pin, name: string): Promise<unknown> {
  safePin(pin); await verifyPin(pin); need((await lstat(pin.path)).size <= 1024 * 1024, "Permission configuration is unbounded");
  const config: any = Bun.TOML.parse(await readFile(pin.path, "utf8")), selected = config.permissions?.[name];
  need(config.default_permissions === name && selected?.extends === ":read-only" && selected.filesystem?.["/"] === "deny" && selected.filesystem?.["/app"] === "deny",
    "Permission configuration lacks its explicit root-deny policy");
  await verifyPin(pin); return { name, extends: selected.extends, filesystem: selected.filesystem };
}
function sandboxPolicy(value: any): unknown { const policy = structuredClone(value); delete policy.network_mode; return policy; }
function containerIdentity(value: any, container: any): void {
  same(value.container_id, container.id, "Container identity differs");
  same(value.boot_id, container.boot_id, "Container boot identity differs");
  same(value.namespaces, container.namespaces, "Container namespace identity differs");
}
async function verifyRecheck(pin: Pin, startup: Pin, context: any, base: any): Promise<void> {
  const check = await read(pin, "environments-harness.container-storage-host-recheck.v1");
  ancestry(check, context); same(check.startup_sha256, startup.sha256, "Host recheck refers to a different startup");
  sameContainer(check.container, base.container, "Host recheck observed another container generation");
  same(check.mounts, base.mounts, "Host recheck mount identity differs");
  need(check.compose_main_id_matches === true && check.parent_lease_active === true && check.volume_identity_matches === true && check.guest_namespace_matches_host_process === true,
    "Host recheck is incomplete");
  const observed = await read(check.container_host_observation, "environments-harness.container-host-storage-observation.v1");
  need(observed.parent_host_binding_sha256 === context.parent_host_binding_sha256 && observed.parent_lease_active === true &&
    observed.artifact_roots_disjoint_from_engine_mounts === true && Array.isArray(observed.unlisted_nested_mounts) && observed.unlisted_nested_mounts.length === 0,
    "Host recheck has no strict topology observation");
}
async function verifyInventory(pin: Pin, base: any): Promise<any> {
  const inventory = await read(guestPin(base, pin), "environments-harness.container-process-inventory.v1");
  same(inventory.boot_id, base.container.boot_id, "Process inventory boot identity differs");
  same(inventory.namespaces, base.container.namespaces, "Process inventory namespace differs");
  need(inventory.argv_or_environment_retained === false && Array.isArray(inventory.processes) && inventory.processes.length > 0 && inventory.processes.length <= 4096 && decimal(inventory.observed_monotonic_ns),
    "Bounded metadata-only process inventory required");
  const keys = new Set<string>(), pids = new Set<number>();
  for (const process of inventory.processes) {
    need(Number.isSafeInteger(process.pid) && process.pid > 0 && decimal(process.start_ticks) &&
      typeof process.state === "string" && process.state.length > 0 && process.state.length <= 4096, "Malformed process identity");
    const key = `${process.pid}:${process.start_ticks}`; need(!pids.has(process.pid), "Duplicate process inventory identity"); keys.add(key); pids.add(process.pid);
  }
  return { value: inventory, keys };
}
async function verifyOriginals(seal: any, base: any): Promise<Map<string, SealedContainerArtifact>> {
  same(seal.collection_roots, base.collection_roots, "Collection roots differ from owner admission");
  need(Array.isArray(seal.collection_roots) && seal.collection_roots.length === 2, "Both original-output and answer-stage collection roots are required");
  const purposes = new Set(seal.collection_roots.map((root: any) => root.purpose));
  need(purposes.has("private-output-originals") && purposes.has("answer-stage"), "Collection root roles are incomplete");
  const totals = new Map<string, bigint>();
  for (const root of seal.collection_roots) {
    exactPath(root.guest_root); need(decimal(root.maximum_total_bytes), "Collection root has no finite byte bound");
    await identity(root.host_root, root.host_identity, true); totals.set(root.host_root, 0n);
    need(mapGuest(base, root.guest_root) === root.host_root, "Collection root is not the original owner bind");
  }
  need(Array.isArray(seal.artifacts) && seal.artifacts.length <= 4096, "Collection inventory is unbounded");
  const artifacts = new Map<string, SealedContainerArtifact>(), hostPaths = new Set<string>();
  for (const artifact of seal.artifacts as SealedContainerArtifact[]) {
    safePin({ path: artifact.host_path, sha256: artifact.sha256 }); exactPath(artifact.guest_path);
    need(decimal(artifact.bytes) && !artifacts.has(artifact.guest_path) && !hostPaths.has(artifact.host_path), "Duplicate or malformed sealed artifact");
    const roots = seal.collection_roots.filter((root: any) => within(root.guest_root, artifact.guest_path));
    need(roots.length === 1 && mapGuest(base, artifact.guest_path) === artifact.host_path && within(roots[0].host_root, artifact.host_path), "Artifact is not an original file in its sealed bind");
    await identity(artifact.host_path, artifact.host_identity, false);
    need(String((await lstat(artifact.host_path, { bigint: true })).size) === artifact.bytes && await fileHash(artifact.host_path) === artifact.sha256, "Sealed original bytes changed");
    const total = totals.get(roots[0].host_root)! + BigInt(artifact.bytes); totals.set(roots[0].host_root, total);
    need(total <= BigInt(roots[0].maximum_total_bytes), "Sealed original collection exceeds its owner byte ceiling");
    artifacts.set(artifact.guest_path, artifact); hostPaths.add(artifact.host_path);
  }
  return artifacts;
}

/** Verifies recorded owner evidence; it does not create container or model qualification. */
export async function verifyContainerReaderChain(options: ContainerReaderChainOptions) {
  need(hash(options.parentHostBindingSha256) && ["bare", "gptgrep"].includes(options.arm), "Reader parent identity and arm are required");
  exactPath(options.projectedOutputRoot);
  const template = await read(options.ownerTemplate, "environments-harness.container-storage-template.v1");
  const runtime = await read(options.runtimeContext, "environments-harness.container-storage-runtime-context.v1");
  const admission = await read(options.admission, "environments-harness.container-storage-admission.v1");
  const job = await read(options.readerJob, "gptgrep.harbor-reader-job.v2");
  need(template.outputs?.runtime_context === options.runtimeContext.path && template.outputs.namespace_plan_review === options.namespacePlanReview.path &&
    template.outputs.admission === options.admission.path && template.outputs.post_broker_finalization === options.finalization.path &&
    template.outputs.namespace_plan === admission.namespace_plan?.path && template.outputs.container_binding_host === admission.container_binding?.path &&
    template.outputs.startup_capture === admission.startup_receipt?.path && template.outputs.host_recheck === admission.host_recheck_receipt?.path,
    "Container evidence addresses differ from the frozen owner outputs");
  same(admission.owner_template, options.ownerTemplate, "Admission owner template differs");
  same(admission.runtime_context, options.runtimeContext, "Admission runtime context differs");
  need(admission.parent_host_binding.sha256 === options.parentHostBindingSha256 && admission.ready_for_command_materialization === true, "Actual parent-bound container admission is unavailable");
  same(admission.parent_host_binding, options.parentHostBinding, "Admission parent binding differs from the actual inherited owner binding");
  safePin(admission.parent_host_binding); await verifyPin(admission.parent_host_binding);
  const namespace = await read(admission.namespace_plan, "environments-harness.container-storage-plan.v1");
  same(namespace.outputs, template.outputs, "Namespace output addresses differ");
  same(namespace.owner_template, options.ownerTemplate, "Namespace owner template differs"); same(namespace.runtime_context, options.runtimeContext, "Namespace runtime context differs");
  const rule = await read(options.derivedPlanRule, "environments-harness.container-derived-plan-rule.v1");
  const review = await read(options.namespacePlanReview, "environments-harness.container-derived-plan-review.v1");
  same(rule.owner_template, options.ownerTemplate, "Derived-plan rule selects another template"); same(review.rule, options.derivedPlanRule, "Derived-plan review uses another rule");
  same(review.owner_template, options.ownerTemplate, "Derived-plan review selects another template"); same(review.runtime_context, options.runtimeContext, "Derived-plan review selects another runtime context");
  same(review.namespace_plan, admission.namespace_plan, "Derived-plan review selects another actual plan"); sameContainer(review.container, namespace.container, "Derived-plan review observed another container");
  need(rule.expected_episode_id === template.episode_id && hash(rule.validator_source_closure_sha256) && review.validator_source_closure_sha256 === rule.validator_source_closure_sha256 &&
    rule.fixed_mounts_paths_commands_security === true && rule.scope === "derived-plan-admission-only" && rule.business_acceptance === false &&
    review.result === "accepted_for_admission" && review.scope === "prior-reviewed-template-instance" && review.business_acceptance === false && hash(review.validated_facts_sha256),
    "Derived-plan review is outside the prior reviewed admission scope");
  same(rule.allowed_runtime_fields, ["trial_name", "environment_session_id", "compose_project", "compose_files", "container_id", "container_generation", "namespace_observations"], "Derived-plan runtime exceptions were broadened");
  same(rule.compose_file_roots, ["parent-output", "parent-temporary"], "Derived-plan Compose roots were broadened");
  need(Number.isSafeInteger(rule.max_compose_files) && rule.max_compose_files >= 1 && rule.max_compose_files <= 8 &&
    Array.isArray(rule.scenario_source_pins) && rule.scenario_source_pins.length > 0 && rule.scenario_source_pins.length <= 64, "Derived-plan source scope is unbounded");
  for (const pin of [rule.authority, rule.harbor_derivation_source, ...rule.scenario_source_pins]) { safePin(pin); await verifyPin(pin); }
  same(runtime.derivation.harbor_derivation_source, rule.harbor_derivation_source, "Runtime derivation source differs from the reviewed rule");
  same({ host_episode_root: runtime.host_episode_root, compose_project_directory: runtime.compose_project_directory }, rule.fixed_runtime_context, "Derived runtime changed a fixed episode or Compose directory");
  same(namespace.parent.binding, admission.parent_host_binding, "Namespace parent binding differs");
  need(hash(namespace.parent.plan_digest) && decimal(namespace.parent.deadline_monotonic_ns) && namespace.additional_reservation_required === true, "Namespace parent reservation evidence is incomplete");
  const base = await read(admission.container_binding, "environments-harness.container-storage-binding.v1");
  const context = { owner_template_sha256: options.ownerTemplate.sha256, host_namespace_plan_sha256: admission.namespace_plan.sha256, parent_host_binding_sha256: options.parentHostBindingSha256 };
  ancestry(base, context); sameContainer(base.container, admission.container, "Admission container generation differs"); sameContainer(base.container, namespace.container, "Namespace container generation differs");
  const arm = options.arm === "bare" ? "baseline" : "treatment";
  need(template.admission_class === "reader" && base.admission_class === "reader" && template.experiment_arm === arm && base.experiment_arm === arm &&
    base.command_binding_required === true && base.custody_mode === "0600" && base.mount_access === "read-only" && base.digest_security === "integrity-not-signature",
    "Reader admission class, arm or command custody differs");
  need(runtime.episode_id === template.episode_id && runtime.derivation?.expected_episode_id === template.episode_id && runtime.service === "main" &&
    template.no_container_lifecycle === true && template.cleanup === false && template.image_id === base.container.image_id,
    "Runtime context does not bind the frozen episode and image");
  same(runtime.harbor_source, template.harbor_source, "Runtime Harbor source differs");
  same(base.parent_deadline_monotonic_ns, namespace.parent.deadline_monotonic_ns, "Container deadline differs from parent");
  need(admission.reservation.mode === "additional-container-reservation" && admission.reservation.transport_exit_is_terminal === false &&
    admission.reservation.parent_plan_digest === namespace.parent.plan_digest, "Additional container reservation is unavailable");
  safePin({ path: admission.reservation.path, sha256: admission.reservation.sha256 });
  await verifyPin({ path: admission.reservation.path, sha256: admission.reservation.sha256 });
  const security = base.container.security;
  need(security?.readonly_rootfs === true && security.privileged === false && security.capabilities_dropped_all === true && security.no_new_privileges === true &&
    security.pid_mode === "private" && security.restart_policy === "no" && security.profile?.failure_never_relaxes === true && security.profile.cap_add?.length === 0,
    "Container security was relaxed or is unavailable");
  safePin(template.model_profile); await verifyPin(template.model_profile);
  same(base.model_profile, { path: template.model_profile_guest_path, sha256: template.model_profile.sha256 }, "Guest model profile is not the exact frozen full config");
  await verifyPin(guestPin(base, base.model_profile));
  const privacy = await read(template.sandbox_privacy_qualification, "environments-harness.sandbox-privacy-qualification.v1");
  need(base.sandbox_privacy_qualification_sha256 === template.sandbox_privacy_qualification.sha256 && privacy.status === "accepted" && privacy.scope === "E4-no-model-sandbox-isolation" &&
    privacy.experiment_arm === arm && privacy.image_id === template.image_id && privacy.sandbox_security_policy_sha256 === digest(sandboxPolicy(template.container_policy)) &&
    privacy.actual_reader_coverage_claimed === false && privacy.authentication_qualified === false && hash(base.auth_projection_sha256), "Reader prerequisite sandbox evidence differs");
  same(privacy.profile, template.model_profile, "Privacy wrapper selected another live profile");
  need(template.filesystem_policy_name === job.options.native?.permissionProfile && base.filesystem_policy_name === template.filesystem_policy_name &&
    base.filesystem_policy_sha256 === template.filesystem_policy_sha256 && privacy.filesystem_policy_sha256 === template.filesystem_policy_sha256 &&
    digest(await filesystemPolicy(template.model_profile, template.filesystem_policy_name)) === template.filesystem_policy_sha256 &&
    digest(await filesystemPolicy(privacy.qualifier_profile, template.filesystem_policy_name)) === template.filesystem_policy_sha256,
    "Live and qualified filesystem policy semantics differ");
  const qualificationTemplate = await read(privacy.qualification_owner_template, "environments-harness.container-storage-template.v1");
  need(qualificationTemplate.admission_class === "bounded-qualification" && qualificationTemplate.experiment_arm === arm && qualificationTemplate.image_id === template.image_id &&
    digest(sandboxPolicy(qualificationTemplate.container_policy)) === privacy.sandbox_security_policy_sha256, "Privacy proof used another sandbox security policy");
  same(qualificationTemplate.model_profile, privacy.qualifier_profile, "Qualification profile pin differs");
  for (const pin of [privacy.native_binary, privacy.probe_source, privacy.qualifier_source, privacy.qualification_result,
    privacy.qualification_command_binding, privacy.qualification_collection_seal, privacy.qualification_outer_finalization]) { safePin(pin); await verifyPin(pin); }
  const authProjection = await read(runtime.auth_projection, "environments-harness.container-auth-projection.v1");
  need(runtime.auth_projection.sha256 === base.auth_projection_sha256 && authProjection.source_schema === "gptgrep.codex-auth.run.v1" && authProjection.lease_acquired === true &&
    authProjection.lease_binding_schema === "gptgrep.codex-auth.lease-binding.v1" && authProjection.absolute_paths_preserved === true && authProjection.credential_contents_read === false &&
    authProjection.new_lease_or_refresh === false && authProjection.uid === base.container.uid && authProjection.gid === base.container.gid &&
    base.model_profile.path === join(authProjection.layout.codex_home, "config.toml"), "Container auth projection does not preserve the actual broker namespace");
  const authManifest = await read(authProjection.source_manifest, "gptgrep.codex-auth.run.v1");
  same(job.options?.task?.id, options.taskId, "Reader job task differs"); same(job.options?.arm, options.arm, "Reader job arm differs");
  const bootstrap = await read(admission.startup_receipt, "environments-harness.container-storage-startup.v1");
  containerIdentity(bootstrap, base.container);
  need(bootstrap.owner_template_sha256 === options.ownerTemplate.sha256 && bootstrap.execution_admitted === false && bootstrap.timens_offsets_zero === true &&
    base.startup_receipt_sha256 === admission.startup_receipt.sha256 && base.host_recheck_receipt_sha256 === admission.host_recheck_receipt.sha256,
    "Container bootstrap evidence differs");
  await verifyRecheck(admission.host_recheck_receipt, admission.startup_receipt, context, base);

  const verifyPhase = async (phase: "preflight" | "reader", evidence: ContainerPhaseEvidence) => {
    const result = await read(evidence.result, "environments-harness.container-storage-run-result.v1");
    const plan = await read(evidence.commandPlan, "environments-harness.container-command-plan.v1");
    const command = template.commands?.find((value: any) => value.id === phase);
    need(command && template.commands.filter((value: any) => value.id === phase).length === 1 && result.command_id === phase && plan.command_id === phase &&
      result.owner_template_sha256 === options.ownerTemplate.sha256 && result.admission_sha256 === options.admission.sha256 && result.ok === true &&
      result.terminal_status === "verified-terminal" && ["passed", "failed"].includes(result.business_status), "Phase has no exact verified owner terminal");
    same(result.command_plan, evidence.commandPlan, "Run result command-plan pin differs"); same(result.startup, evidence.startup, "Run result startup pin differs");
    same(plan.owner_template, options.ownerTemplate, "Command owner template differs"); same(plan.admission, options.admission, "Command admission differs"); same(plan.container_binding, admission.container_binding, "Command base binding differs");
    same(plan.reservation, admission.reservation, "Command reservation differs"); same(plan.parent_deadline_monotonic_ns, base.parent_deadline_monotonic_ns, "Command parent deadline differs");
    need(plan.one_use === true && plan.plan.run_id === command.run_id && plan.plan.cwd === command.cwd && plan.plan.workload_sha256 === command.workload.sha256 &&
      plan.workload.sha256 === command.workload.sha256 && plan.workload.path === command.workload_guest_path && plan.command_binding_guest_path === command.guest_command_binding_path,
      "Command plan differs from its static workload/template");
    need(evidence.commandPlan.path === mapGuest(base, command.guest_command_plan_path) && plan.command_binding.path === mapGuest(base, command.guest_command_binding_path), "Command control records are outside their admitted physical mapping");
    const binding = await read(plan.command_binding, "environments-harness.container-command-binding.v1");
    ancestry(binding, context); same(binding.plan, plan.plan, "Per-command core binding differs"); same(binding.container_binding, admission.container_binding, "Per-command base binding differs");
    same(binding.reservation, admission.reservation, "Per-command reservation differs"); same(binding.allowed_paths, plan.allowed_paths, "Command write allowances differ");
    need(binding.command_id === phase && binding.container_id === base.container.id && binding.admission_class === "reader" && binding.custody_mode === "0600" &&
      binding.mount_access === "read-only" && binding.custody_uid === base.custody_uid && binding.immutable_job.path === command.immutable_job_guest_path &&
      binding.immutable_job.sha256 === options.readerJob.sha256 && command.immutable_job.sha256 === options.readerJob.sha256, "Exact phase or immutable reader-job binding differs");
    same(binding.exact_owner_injected_env_names, ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"], "Owner-injected environment exception differs");
    same(binding.acceptance, plan.acceptance, "Command acceptance differs");
    need(plan.acceptance.path === command.guest_acceptance_path && plan.acceptance.sha256 === command.acceptance_template.sha256, "Detached acceptance is not the pinned template bytes");
    const acceptance = await read(command.acceptance_template, "gptgrep.availability-controller.v1");
    const expand = (args: string[]) => args.map(arg => arg === "@ACCEPTANCE_PATH@" ? plan.acceptance.path : arg === "@ACCEPTANCE_SHA256@" ? plan.acceptance.sha256 : arg);
    need(Array.isArray(command.controller_argv) && command.controller_argv.filter((arg: string) => arg === "@ACCEPTANCE_PATH@").length === 1 && command.controller_argv.filter((arg: string) => arg === "@ACCEPTANCE_SHA256@").length === 1,
      "Command acceptance expansion is not exact");
    same(plan.controller_argv, expand(command.controller_argv), "Materialized controller argv differs");
    same(acceptance.controllerArgv, command.controller_argv, "Acceptance argv template differs");
    same(plan.plan.command.argv_sha256, digest(plan.controller_argv), "Materialized argv hash differs");
    need(plan.controller_argv[0] === plan.plan.command.executable && acceptance.storage.runId === command.run_id && acceptance.storage.cwd === command.cwd &&
      acceptance.storage.workloadSha256 === command.workload.sha256 && acceptance.status === "accepted", "Acceptance does not bind the actual checked workload");
    const executable = { path: mapGuest(base, plan.plan.command.executable), sha256: plan.plan.command.executable_sha256 }; safePin(executable); await verifyPin(executable);
    const jobIndex = plan.controller_argv.indexOf("--job"), jobHashIndex = plan.controller_argv.indexOf("--expect-sha256"), modeIndex = plan.controller_argv.indexOf("--mode");
    need(jobIndex > 0 && jobHashIndex > 0 && modeIndex > 0 && plan.controller_argv[jobIndex + 1] === binding.immutable_job.path &&
      plan.controller_argv[jobHashIndex + 1] === options.readerJob.sha256 && plan.controller_argv[modeIndex + 1] === phase, "Guest command selected a different job or phase");
    same(command.source_pins, acceptance.sourcePins, "Acceptance source closure differs");
    same(plan.child_argv, command.child_argv, "Child argv closure differs"); same(acceptance.childArgv, command.child_argv, "Accepted child argv closure differs");
    for (const pin of command.source_pins ?? []) await verifyPin(guestPin(base, pin));
    await verifyPin(guestPin(base, binding.immutable_job)); await verifyPin(guestPin(base, plan.acceptance));
    await verifyPin(guestPin(base, plan.workload));
    const transport = await read(result.transport, "environments-harness.container-storage-transport.v1");
    need(transport.command_id === phase && transport.owner_template_sha256 === options.ownerTemplate.sha256 && transport.runtime_context_sha256 === options.runtimeContext.sha256 &&
      transport.expected_admission_path === options.admission.path && transport.command_plan_sha256 === evidence.commandPlan.sha256, "Transport observed another command");
    const seal = await read(result.collection_seal, "environments-harness.container-storage-collection-seal.v1");
    ancestry(seal, context);
    need(seal.command_binding_sha256 === plan.command_binding.sha256 && seal.command_plan_sha256 === evidence.commandPlan.sha256 && seal.container_binding_sha256 === admission.container_binding.sha256 &&
      seal.container_id === base.container.id && seal.source_originals_preserved === true && seal.collection_allowed === true && seal.container_may_remain_running === true &&
      seal.reservation_status === "held-until-outer-finalization" && seal.business_status === result.business_status, "Collection seal lacks exact custody/termination evidence");
    sameContainer(seal.container_generation, base.container, "Collection observed another container generation");
    if (phase === "reader") same(seal.projected_output_root, options.projectedOutputRoot, "Projected output root differs from owner collection");
    const startup = await read(evidence.startup, "environments-harness.container-command-startup.v1");
    const terminal = await read(seal.guest_terminal, "environments-harness.container-command-terminal.v1");
    need(evidence.startup.path === mapGuest(base, plan.guest_startup_path) && seal.guest_terminal.path === mapGuest(base, plan.guest_terminal_path), "Phase lifecycle pins are outside their owner-mapped paths");
    for (const record of [startup, terminal]) {
      containerIdentity(record, base.container);
      need(record.command_id === phase && record.command_plan_sha256 === evidence.commandPlan.sha256 && record.command_binding_sha256 === plan.command_binding.sha256 &&
        record.container_binding_sha256 === admission.container_binding.sha256 && record.timens_offsets_zero === true, "Guest process terminal belongs to another command");
    }
    need(startup.parent_host_binding_sha256 === options.parentHostBindingSha256 && startup.command_sha256 === plan.plan.command.argv_sha256 && startup.child_role === "guarded-worker" &&
      startup.uid === base.container.uid && startup.gid === base.container.gid && decimal(startup.deadline_monotonic_ns) && BigInt(startup.deadline_monotonic_ns) <= BigInt(base.parent_deadline_monotonic_ns) &&
      terminal.startup_receipt_sha256 === evidence.startup.sha256 && terminal.child_reaped === true && terminal.process_group_terminal === true, "Guest startup or teardown proof is incomplete");
    need(terminal.execution_identity?.path === plan.guest_execution_identity_path, "Execution identity is outside the exact command plan");
    const execution = await read(guestPin(base, terminal.execution_identity), "environments-harness.container-storage-execution-identity.v1");
    need(execution.command_plan_sha256 === evidence.commandPlan.sha256 && execution.command_binding_sha256 === plan.command_binding.sha256 &&
      execution.startup_receipt_sha256 === evidence.startup.sha256 && execution.guarded_worker_pid === startup.child_pid && decimal(execution.guarded_worker_start_ticks),
      "Actual execution identity is not bound to the guarded worker startup");
    same(execution.execution_child, terminal.execution_child, "Terminal changed the actual execution child generation");
    if (execution.execution_child !== null) need(Number.isSafeInteger(execution.execution_child.pid) && execution.execution_child.pid > 0 &&
      Number.isSafeInteger(execution.execution_child.process_group) && execution.execution_child.process_group > 0 && decimal(execution.execution_child.start_ticks), "Actual execution child identity is malformed");
    same(terminal.process_baseline, startup.process_baseline, "Terminal process baseline changed");
    need(terminal.process_baseline.path === plan.guest_process_baseline_path && terminal.process_final.path === plan.guest_process_final_path,
      "Process inventory paths are outside the command plan");
    const before = await verifyInventory(terminal.process_baseline, base), after = await verifyInventory(terminal.process_final, base);
    need(!after.keys.has(`${execution.guarded_worker_pid}:${execution.guarded_worker_start_ticks}`), "Guarded worker remains in the terminal inventory");
    need(Number.isSafeInteger(startup.supervisor_pid) && startup.supervisor_pid > 0 && decimal(startup.supervisor_start_ticks), "Guest supervisor generation is unavailable");
    const supervisor = `${startup.supervisor_pid}:${startup.supervisor_start_ticks}`;
    need(after.value.processes.every((row: any) => before.keys.has(`${row.pid}:${row.start_ticks}`) ||
      `${row.pid}:${row.start_ticks}` === supervisor || row.state === "Z" || row.state === "X"), "New processes remain after guest terminal");
    need(BigInt(after.value.observed_monotonic_ns) >= BigInt(before.value.observed_monotonic_ns), "Process inventory order is invalid");
    const business = terminal.result === "success" && terminal.returncode === 0 ? "passed" : "failed";
    if (business === "passed") need(execution.execution_child !== null, "Successful guest command has no actual execution child");
    need(result.business_status === business && seal.guest_returncode === terminal.returncode, "Business result was relabelled during collection");
    await verifyRecheck(seal.host_recheck, evidence.startup, context, base);
    return { result, plan, binding, startup, terminal, seal, artifacts: await verifyOriginals(seal, base) };
  };
  const preflight = await verifyPhase("preflight", options.preflight), reader = await verifyPhase("reader", options.reader);
  need(preflight.result.business_status === "passed", "Reader cannot follow failed preflight");
  const readyArtifact = preflight.artifacts.get(job.readinessPath), readerReadyArtifact = reader.artifacts.get(job.readinessPath);
  need(readyArtifact && readerReadyArtifact, "Sealed preflight-readiness artifact is unavailable"); same(readerReadyArtifact, readyArtifact, "Reader changed its sealed preflight-readiness original");
  const readiness = { path: readyArtifact.host_path, sha256: readyArtifact.sha256 };
  const ready = await read(readiness, "gptgrep.harbor-native-readiness.v2");
  need(ready.status === "ready" && ready.commandId === "preflight" && ready.jobSha256 === options.readerJob.sha256 && ready.taskId === options.taskId && ready.arm === options.arm &&
    ready.storageBindingSha256 === preflight.plan.command_binding.sha256 && ready.containerBindingSha256 === admission.container_binding.sha256 &&
    ready.ownerTemplateSha256 === options.ownerTemplate.sha256 && ready.parentHostBindingSha256 === options.parentHostBindingSha256,
    "Readiness does not bind the actual preflight phase");
  same(ready.commandBinding, { path: preflight.plan.command_binding_guest_path, sha256: preflight.plan.command_binding.sha256 }, "Readiness command pin differs");
  same(ready.acceptance, preflight.plan.acceptance, "Readiness acceptance differs");
  need(authManifest.run_id === ready.authRunId, "Readiness belongs to another broker run");
  const attempt = await read(options.sourceAttempt, "gptgrep.reader-attempt.v1"), manifest = await read(options.sourceManifest, "gptgrep.harbor-native-artifacts.v1");
  need(attempt.taskId === options.taskId && attempt.arm === options.arm && attempt.storageBindingSha256 === reader.plan.command_binding.sha256 && attempt.authRunId === ready.authRunId &&
    manifest.taskId === options.taskId && manifest.arm === options.arm && manifest.readerJobSha256 === options.readerJob.sha256 && manifest.executionMode === "live" &&
    Array.isArray(manifest.artifacts), "Source reader evidence differs from actual command identity");
  const original = (pin: Pin, guestPath: string) => {
    const row = reader.artifacts.get(guestPath);
    need(row && row.host_path === pin.path && row.sha256 === pin.sha256, "Reader source evidence was copied or is outside the sealed originals"); return row;
  };
  original(options.sourceAttempt, join(job.options.outputDir, "attempt.json")); original(options.sourceManifest, join(job.options.outputDir, "bridge-manifest.json"));
  for (const entry of manifest.artifacts) {
    const row = reader.artifacts.get(entry.path);
    need(row && row.sha256 === entry.sha256 && BigInt(row.bytes) === BigInt(entry.bytes), "Reader manifest differs from sealed original bytes");
  }
  if (attempt.answerPath) {
    const answer = reader.artifacts.get(attempt.answerPath), staged = reader.artifacts.get(job.answerPath);
    need(answer && staged && answer.sha256 === attempt.answerSha256 && staged.sha256 === answer.sha256 && staged.bytes === answer.bytes, "Answer handoff is absent or differs from its original");
  }
  const final = await read(options.finalization, "environments-harness.container-storage-finalization.v1");
  ancestry(final, context);
  need(final.stage === "post-broker" && final.context_status === "observed" && final.runtime_context_sha256 === options.runtimeContext.sha256 && final.command_id === "reader" && final.expected_admission_path === options.admission.path &&
    final.container_binding_sha256 === admission.container_binding.sha256 && final.command_binding_sha256 === reader.plan.command_binding.sha256 && final.command_plan_sha256 === options.reader.commandPlan.sha256 &&
    final.container_id === base.container.id && final.terminal_status === "verified-terminal" && final.business_status === reader.result.business_status &&
    final.reservation_status === "released-after-terminal-proof" && final.retention === "retain-all-no-cleanup" && final.owned_overlay_unmounted === true &&
    final.auth_link_released === true && final.own_auth_link_absent === true && final.auth_docker_label_seen === true && final.auth_docker_label_remaining === 0,
    "Outer finalization lacks verified cleanup, auth absence or reservation release");
  same(final.guest_startup, options.reader.startup, "Finalization startup differs"); same(final.guest_terminal, reader.seal.guest_terminal, "Finalization terminal differs");
  same(final.collection_seal, reader.result.collection_seal, "Finalization collection seal differs"); same(final.host_recheck, reader.seal.host_recheck, "Finalization host recheck differs"); same(final.reservation, admission.reservation, "Finalization reservation differs");
  const finalTransport = await read(final.transport, "environments-harness.container-storage-transport.v1");
  need(finalTransport.owner_template_sha256 === options.ownerTemplate.sha256 && finalTransport.runtime_context_sha256 === options.runtimeContext.sha256 &&
    finalTransport.expected_admission_path === options.admission.path && finalTransport.command_id === "reader" && finalTransport.command_plan_sha256 === options.reader.commandPlan.sha256,
    "Outer finalization transport identity differs");
  const broker = await read(final.broker_final_evidence, "environments-harness.container-broker-final-evidence.v1");
  need(broker.kernel_lease_probe === "free" && broker.binding_matches === true && broker.own_auth_link_absent === true && broker.auth_link_final_released === true &&
    broker.job_label_positively_observed === true && broker.job_label_remaining === 0 && broker.guardian_retained_lease === false && broker.sweep_inconclusive === false &&
    broker.credential_link_scan_complete === true && broker.canonical_changed === false && broker.credential_contents_read === false &&
    [broker.unreleased_holders, broker.sweep_survivor_pids, broker.credential_link_scan_errors].every(value => Array.isArray(value) && value.length === 0),
    "Post-broker final evidence has incomplete lease/auth cleanup");
  same(broker.broker_source, template.auth_supervisor_source, "Broker final source differs from owner admission");
  need(broker.lease_path === authProjection.lease_path && broker.lease_identity.device === authProjection.lease_device && broker.lease_identity.inode === authProjection.lease_inode &&
    broker.job_label === authProjection.docker_job_label && broker.own_auth_link === authProjection.layout.auth_link, "Broker final evidence belongs to another lease or auth link");
  safePin(broker.broker_source); await verifyPin(broker.broker_source);
  safePin(broker.status_receipt); await verifyPin(broker.status_receipt);
  const auth = await read(broker.run_receipt);
  need(auth.run_id === ready.authRunId && ["PASS", "FAILED", "TIMEOUT"].includes(auth.state) && auth.auth_link_final?.released === true &&
    auth.layout?.auth_link === broker.own_auth_link && auth.sweep?.inconclusive === false && Array.isArray(auth.sweep.survivor_pids) && auth.sweep.survivor_pids.length === 0 &&
    auth.credential_links?.complete === true && auth.credential_links.found === 0 && !auth.auth_block && auth.child_exit_code !== 77,
    "Broker final evidence does not bind this reader's actual auth receipt");
  const probe = await read(final.normal_host_probe_receipt, "environments-harness.storage-receipt.v1");
  need(probe.status === "checkpoint" && probe.plan_digest === namespace.parent.plan_digest && probe.error === null && probe.cleanup_performed === false,
    "Post-container strict host checkpoint is unavailable");
  return { readerBindingSha256: reader.plan.command_binding.sha256, containerBindingSha256: admission.container_binding.sha256,
    parentHostBindingSha256: options.parentHostBindingSha256, artifactMap: reader.artifacts, readiness,
    businessStatus: reader.result.business_status as "passed" | "failed", finalization: options.finalization, liveQualified: false as const };
}
