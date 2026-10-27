import { expect, test } from "bun:test";
import { lstat, mkdir, mkdtemp, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { verifyContainerReaderChain, type ContainerReaderChainOptions } from "../src/container";
import { digest, fileHash, sha256 } from "../src/io";
import type { Pin } from "../src/availability";

// Synthetic evidence-consumer fixtures. They do not qualify Docker, an owner
// lease, authentication, or any native model/tool execution.
async function fixture(business: "passed" | "failed" = "passed", finalProcessRows: any[] = []) {
  const root = await realpath(await mkdtemp(join(tmpdir(), "container-chain-")));
  const control = join(root, "control"), output = join(root, "originals"), answerRoot = join(root, "answer");
  for (const path of [control, output, answerRoot]) await mkdir(path);
  const guestControl = "/private/control", guestOutput = "/private/output";
  const pin = async (path: string): Promise<Pin> => ({ path, sha256: await fileHash(path) });
  const write = async (path: string, value: unknown): Promise<Pin> => {
    await mkdir(dirname(path), { recursive: true }); await writeFile(path, JSON.stringify(value), { mode: 0o600 }); return pin(path);
  };
  const put = (name: string, value: unknown) => write(join(control, name), value);
  const hostIdentity = async (path: string) => {
    const info = await lstat(path, { bigint: true });
    return { device: String(info.dev), inode: String(info.ino), uid: Number(info.uid), gid: Number(info.gid), mode: `0${Number(info.mode & 0o777n).toString(8).padStart(3, "0")}` };
  };
  const mapped = (path: string) => path.startsWith(`${guestControl}/`) ? join(control, path.slice(guestControl.length + 1))
    : path.startsWith(`${guestOutput}/`) ? join(output, path.slice(guestOutput.length + 1)) : join(answerRoot, path.slice("/app/".length));
  const job = { schema: "gptgrep.harbor-reader-job.v2", options: { task: { id: "synthetic-task" }, arm: "bare", outputDir: guestOutput, native: { permissionProfile: "reader" } },
    readinessPath: `${guestOutput}/preflight-readiness.json`, answerPath: "/app/answer.json" };
  const readerJob = await put("job.json", job), binary = await put("bun", { fixture: "unexecuted" }), controller = await put("controller.ts", { fixture: "unexecuted" });
  const sourcePins = [readerJob, binary, controller].map(value => ({ path: `${guestControl}/${value.path.split("/").at(-1)}`, sha256: value.sha256 }));
  const commands: any[] = [];
  for (const id of ["preflight", "reader"]) {
    const workload = await put(`${id}.workload.json`, { schema: "environments-harness.container-storage-workload.v1", fixture: true });
    const argv = [`${guestControl}/bun`, `${guestControl}/controller.ts`, "--mode", id, "--job", `${guestControl}/job.json`, "--expect-sha256", readerJob.sha256,
      "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"];
    const acceptance = await put(`${id}.acceptance.json`, { schema: "gptgrep.availability-controller.v1", status: "accepted", sourcePins,
      storage: { runId: `run-${id}`, cwd: "/app/task", workloadSha256: workload.sha256 }, controllerArgv: argv, childArgv: [] });
    commands.push({ id, run_id: `run-${id}`, cwd: "/app/task", workload, immutable_job: readerJob, acceptance_template: acceptance,
      source_pins: sourcePins, controller_argv: argv, child_argv: [], environment: [], allowed_paths: [], host_artifact_root: join(root, id),
      guest_command_plan_path: `${guestControl}/${id}.plan.json`, guest_command_binding_path: `${guestControl}/${id}.binding.json`,
      guest_acceptance_path: `${guestControl}/${id}.acceptance.json`, guest_startup_path: `${guestControl}/${id}.startup.json`,
      guest_terminal_path: `${guestControl}/${id}.terminal.json`, workload_guest_path: `${guestControl}/${id}.workload.json`, immutable_job_guest_path: `${guestControl}/job.json` });
  }
  const parent = await put("parent-binding.json", { schema: "fixture.parent" }), harbor = await put("harbor.json", { source: "fixture" }),
    brokerSource = await put("broker-source.py", { fixture: "unexecuted source" });
  const authLink = join(control, "auth.json"), authManifest = await put("auth-manifest.json", { schema: "gptgrep.codex-auth.run.v1", run_id: "synthetic-auth" });
  const authProjection = await put("auth-projection.json", { schema: "environments-harness.container-auth-projection.v1", source_manifest: authManifest, source_schema: "gptgrep.codex-auth.run.v1",
    lease_acquired: true, lease_binding_schema: "gptgrep.codex-auth.lease-binding.v1", absolute_paths_preserved: true, credential_contents_read: false, new_lease_or_refresh: false,
    uid: 1000, gid: 1000, layout: { codex_home: control, auth_link: authLink }, lease_path: join(control, "lease"), lease_device: "1", lease_inode: "2", docker_job_label: "synthetic-job" });
  const container = { id: sha256("container"), image_id: `sha256:${sha256("image")}`, boot_id: "00000000-0000-4000-8000-000000000001", uid: 1000, gid: 1000,
    namespaces: { mount: "mnt:[1]", pid: "pid:[2]", user: "user:[3]", time: "time:[4]", network: "net:[5]" },
    security: { readonly_rootfs: true, privileged: false, capabilities_dropped_all: true, no_new_privileges: true, pid_mode: "private", restart_policy: "no",
      profile: { failure_never_relaxes: true, cap_add: [] }, resources: { host_mem_available_bytes: "99999999", host_observation_sha256: sha256("memory-observation") } } };
  const containerPolicy = { fixture: "frozen-security", network_mode: "bridge" };
  const filesystemPolicy = { name: "reader", extends: ":read-only", filesystem: { "/": "deny", "/app": "deny", "/app/task": "write" } };
  await writeFile(join(control, "profile-source.toml"), 'default_permissions = "reader"\n[permissions.reader]\nextends = ":read-only"\n[permissions.reader.filesystem]\n"/" = "deny"\n"/app" = "deny"\n"/app/task" = "write"\n');
  const modelProfile = await pin(join(control, "profile-source.toml"));
  const qualificationTemplate = await put("qualification-template.json", { schema: "environments-harness.container-storage-template.v1", admission_class: "bounded-qualification", experiment_arm: "baseline",
    image_id: container.image_id, container_policy: { ...containerPolicy, network_mode: "none" }, model_profile: modelProfile });
  const privacy = await put("privacy.json", { schema: "environments-harness.sandbox-privacy-qualification.v1", status: "accepted", scope: "E4-no-model-sandbox-isolation",
    experiment_arm: "baseline", image_id: container.image_id, sandbox_security_policy_sha256: digest({ fixture: "frozen-security" }), actual_reader_coverage_claimed: false, authentication_qualified: false,
    profile: modelProfile, qualifier_profile: modelProfile, qualification_owner_template: qualificationTemplate, filesystem_policy_sha256: digest(filesystemPolicy),
    native_binary: binary, probe_source: controller, qualifier_source: controller, qualification_result: harbor, qualification_command_binding: harbor, qualification_collection_seal: harbor, qualification_outer_finalization: harbor });
  const outputs = { runtime_context: join(control, "runtime.json"), namespace_plan_review: join(control, "namespace-review.json"), admission: join(control, "admission.json"),
    post_broker_finalization: join(control, "finalization.json"), namespace_plan: join(control, "namespace.json"), container_binding_host: join(control, "base.json"),
    startup_capture: join(control, "bootstrap.json"), host_recheck: join(control, "bootstrap-recheck.json") };
  const ownerTemplate = await put("template.json", { schema: "environments-harness.container-storage-template.v1", episode_id: "episode", admission_class: "reader", experiment_arm: "baseline",
    image_id: container.image_id, no_container_lifecycle: true, cleanup: false, harbor_source: harbor, sandbox_privacy_qualification: privacy, container_policy: containerPolicy, model_profile: modelProfile, model_profile_guest_path: join(control, "config.toml"), auth_supervisor_source: brokerSource, filesystem_policy_name: "reader", filesystem_policy_sha256: digest(filesystemPolicy), outputs, commands });
  const runtimeContext = await put("runtime.json", { schema: "environments-harness.container-storage-runtime-context.v1", episode_id: "episode", service: "main", harbor_source: harbor,
    derivation: { expected_episode_id: "episode", harbor_derivation_source: harbor }, auth_projection: authProjection, host_episode_root: root, compose_project_directory: join(root, "export/environment") });
  const namespace = await put("namespace.json", { schema: "environments-harness.container-storage-plan.v1", owner_template: ownerTemplate, runtime_context: runtimeContext, container,
    outputs, parent: { binding: parent, plan_digest: sha256("parent-plan"), deadline_monotonic_ns: "999999999999" }, additional_reservation_required: true });
  const derivedPlanRule = await put("derived-rule.json", { schema: "environments-harness.container-derived-plan-rule.v1", owner_template: ownerTemplate, expected_episode_id: "episode",
    validator_source_closure_sha256: sha256("validator-source"), fixed_mounts_paths_commands_security: true, scope: "derived-plan-admission-only", business_acceptance: false,
    allowed_runtime_fields: ["trial_name", "environment_session_id", "compose_project", "compose_files", "container_id", "container_generation", "namespace_observations"],
    fixed_runtime_context: { host_episode_root: root, compose_project_directory: join(root, "export/environment") }, compose_file_roots: ["parent-output", "parent-temporary"], max_compose_files: 8, scenario_source_pins: [readerJob], authority: harbor, harbor_derivation_source: harbor });
  const namespacePlanReview = await put("namespace-review.json", { schema: "environments-harness.container-derived-plan-review.v1", rule: derivedPlanRule,
    owner_template: ownerTemplate, runtime_context: runtimeContext, namespace_plan: namespace, container, validator_source_closure_sha256: sha256("validator-source"),
    result: "accepted_for_admission", scope: "prior-reviewed-template-instance", business_acceptance: false, validated_facts_sha256: sha256("validated-facts") });
  const ancestry = { owner_template_sha256: ownerTemplate.sha256, host_namespace_plan_sha256: namespace.sha256, parent_host_binding_sha256: parent.sha256 };
  const mounts = [[control, guestControl], [control, control], [output, guestOutput], [answerRoot, "/app"]].map(([host_source, guest_target]) => ({ template: {
    host_source, guest_target, kind: "directory", access: "write", propagation: "rprivate", create_host_path: false } }));
  mounts.push({ template: { host_source: modelProfile.path, guest_target: join(control, "config.toml"), kind: "file", access: "read", propagation: "rprivate", create_host_path: false } });
  const collectionRoots = await Promise.all([[output, guestOutput, "private-output-originals"], [answerRoot, "/app", "answer-stage"]].map(async ([host_root, guest_root, purpose]) => ({
    host_root, guest_root, purpose, host_identity: await hostIdentity(host_root!), maximum_total_bytes: "1048576" })));
  const hostObservation = await put("host-observation.json", { schema: "environments-harness.container-host-storage-observation.v1", parent_host_binding_sha256: parent.sha256,
    parent_lease_active: true, artifact_roots_disjoint_from_engine_mounts: true, unlisted_nested_mounts: [] });
  const recheck = (name: string, startup: Pin) => put(name, { schema: "environments-harness.container-storage-host-recheck.v1", ...ancestry,
    startup_sha256: startup.sha256, container, mounts, compose_main_id_matches: true, parent_lease_active: true, volume_identity_matches: true,
    guest_namespace_matches_host_process: true, container_host_observation: hostObservation });
  const bootstrap = await put("bootstrap.json", { schema: "environments-harness.container-storage-startup.v1", owner_template_sha256: ownerTemplate.sha256,
    container_id: container.id, boot_id: container.boot_id, namespaces: container.namespaces, execution_admitted: false, timens_offsets_zero: true });
  const bootstrapRecheck = await recheck("bootstrap-recheck.json", bootstrap);
  const base = { schema: "environments-harness.container-storage-binding.v1", ...ancestry, container, admission_class: "reader", experiment_arm: "baseline", command_binding_required: true,
    custody_uid: 1000, custody_mode: "0600", mount_access: "read-only", digest_security: "integrity-not-signature", mounts, collection_roots: collectionRoots,
    parent_deadline_monotonic_ns: "999999999999", sandbox_privacy_qualification_sha256: privacy.sha256, auth_projection_sha256: authProjection.sha256,
    startup_receipt_sha256: bootstrap.sha256, host_recheck_receipt_sha256: bootstrapRecheck.sha256, model_profile: { path: join(control, "config.toml"), sha256: modelProfile.sha256 }, filesystem_policy_name: "reader", filesystem_policy_sha256: digest(filesystemPolicy) };
  const basePin = await put("base.json", base), reservationPin = await put("reservation.json", { fixture: "additional-reservation" });
  const reservation = { ...reservationPin, mode: "additional-container-reservation", transport_exit_is_terminal: false, parent_plan_digest: sha256("parent-plan") };
  const admission = await put("admission.json", { schema: "environments-harness.container-storage-admission.v1", owner_template: ownerTemplate, runtime_context: runtimeContext,
    namespace_plan: namespace, parent_host_binding: parent, startup_receipt: bootstrap, host_recheck_receipt: bootstrapRecheck, container_binding: basePin, reservation,
    container, commands, ready_for_command_materialization: true });
  const originals = async () => {
    const names = ["preflight-readiness.json", "attempt.json", "bridge-manifest.json", "answer.json", "trace.json"];
    const rows: any[] = [];
    for (const name of names) {
      const path = join(output, name);
      try { const info = await lstat(path); rows.push({ guest_path: `${guestOutput}/${name}`, host_path: path, host_identity: await hostIdentity(path), sha256: await fileHash(path), bytes: String(info.size), purpose: "private-output-originals" }); }
      catch (error: any) { if (error.code !== "ENOENT") throw error; }
    }
    try {
      const path = join(answerRoot, "answer.json"), info = await lstat(path);
      rows.push({ guest_path: "/app/answer.json", host_path: path, host_identity: await hostIdentity(path), sha256: await fileHash(path), bytes: String(info.size), purpose: "answer-stage" });
    } catch (error: any) { if (error.code !== "ENOENT") throw error; }
    return rows;
  };
  let sourceAttempt!: Pin, sourceManifest!: Pin;
  const phase = async (id: "preflight" | "reader") => {
    const command = commands.find(value => value.id === id), acceptance = { path: command.guest_acceptance_path, sha256: command.acceptance_template.sha256 };
    const argv = command.controller_argv.map((arg: string) => arg === "@ACCEPTANCE_PATH@" ? acceptance.path : arg === "@ACCEPTANCE_SHA256@" ? acceptance.sha256 : arg);
    const core = { command: { argv_sha256: digest(argv), executable: argv[0], executable_sha256: binary.sha256 }, cwd: command.cwd, run_id: command.run_id, workload_sha256: command.workload.sha256 };
    const binding = await put(`${id}.binding.json`, { schema: "environments-harness.container-command-binding.v1", ...ancestry, command_id: id, plan: core, container_binding: basePin,
      reservation, allowed_paths: [], container_id: container.id, admission_class: "reader", custody_mode: "0600", mount_access: "read-only", custody_uid: 1000,
      immutable_job: { path: command.immutable_job_guest_path, sha256: readerJob.sha256 }, acceptance,
      exact_owner_injected_env_names: ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"] });
    const planValue = { schema: "environments-harness.container-command-plan.v1", command_id: id, owner_template: ownerTemplate, admission, container_binding: basePin, command_binding: binding,
      command_binding_guest_path: command.guest_command_binding_path, acceptance, controller_argv: argv, child_argv: [], plan: core, allowed_paths: [], parent_deadline_monotonic_ns: base.parent_deadline_monotonic_ns,
      reservation, one_use: true, workload: { path: command.workload_guest_path, sha256: command.workload.sha256 }, guest_startup_path: command.guest_startup_path, guest_terminal_path: command.guest_terminal_path,
      guest_process_baseline_path: `${guestControl}/${id}.before.json`, guest_process_final_path: `${guestControl}/${id}.after.json`, guest_execution_identity_path: `${guestControl}/${id}.execution.json` };
    const commandPlan = await put(`${id}.plan.json`, planValue);
    const inventory = { schema: "environments-harness.container-process-inventory.v1", namespaces: container.namespaces, boot_id: container.boot_id,
      observed_monotonic_ns: "100", argv_or_environment_retained: false, processes: [{ pid: 1, start_ticks: "1", state: "S", parent: "0", group: "1", session: "1" }] };
    const before = await put(`${id}.before.json`, inventory), after = await put(`${id}.after.json`, { ...inventory, observed_monotonic_ns: "200", processes: [...inventory.processes, ...finalProcessRows] });
    const baseline = { path: planValue.guest_process_baseline_path, sha256: before.sha256 }, final = { path: planValue.guest_process_final_path, sha256: after.sha256 };
    const common = { command_id: id, command_plan_sha256: commandPlan.sha256, command_binding_sha256: binding.sha256, container_binding_sha256: basePin.sha256,
      container_id: container.id, boot_id: container.boot_id, namespaces: container.namespaces, timens_offsets_zero: true };
    const startup = await put(`${id}.startup.json`, { schema: "environments-harness.container-command-startup.v1", ...common, parent_host_binding_sha256: parent.sha256,
      command_sha256: core.command.argv_sha256, child_role: "guarded-worker", child_pid: 123, supervisor_pid: 2, supervisor_start_ticks: "2", uid: 1000, gid: 1000, deadline_monotonic_ns: "999999999", process_baseline: baseline });
    const executionChild = { pid: 124, process_group: 124, start_ticks: "101" };
    const executionIdentity = await put(`${id}.execution.json`, { schema: "environments-harness.container-storage-execution-identity.v1", command_plan_sha256: commandPlan.sha256,
      command_binding_sha256: binding.sha256, startup_receipt_sha256: startup.sha256, guarded_worker_pid: 123, guarded_worker_start_ticks: "100", execution_child: executionChild });
    const status = id === "preflight" ? "passed" : business, returncode = status === "passed" ? 0 : 2;
    const terminal = await put(`${id}.terminal.json`, { schema: "environments-harness.container-command-terminal.v1", ...common, startup_receipt_sha256: startup.sha256,
      execution_identity: { path: planValue.guest_execution_identity_path, sha256: executionIdentity.sha256 }, execution_child: executionChild,
      child_reaped: true, process_group_terminal: true, process_baseline: baseline, process_final: final, result: status === "passed" ? "success" : "failure", returncode });
    if (id === "preflight") {
      await write(join(output, "preflight-readiness.json"), { schema: "gptgrep.harbor-native-readiness.v2", status: "ready", commandId: id, jobSha256: readerJob.sha256,
        taskId: "synthetic-task", arm: "bare", storageBindingSha256: binding.sha256, containerBindingSha256: basePin.sha256, ownerTemplateSha256: ownerTemplate.sha256,
        parentHostBindingSha256: parent.sha256, commandBinding: { path: command.guest_command_binding_path, sha256: binding.sha256 }, acceptance, authRunId: "synthetic-auth" });
    } else {
      const answer = await write(join(output, "answer.json"), { answer: "synthetic fixture" }); await writeFile(join(answerRoot, "answer.json"), await readFile(answer.path));
      const trace = await write(join(output, "trace.json"), { fixture: "no provider" });
      sourceAttempt = await write(join(output, "attempt.json"), { schema: "gptgrep.reader-attempt.v1", taskId: "synthetic-task", arm: "bare", storageBindingSha256: binding.sha256,
        authRunId: "synthetic-auth", status: business === "passed" ? "completed" : "failed", answerPath: `${guestOutput}/answer.json`, answerSha256: answer.sha256,
        native: { tracePath: `${guestOutput}/trace.json` } });
      const artifacts = await Promise.all([["attempt", sourceAttempt], ["answer", answer], ["trace", trace], ["preflight", await pin(join(output, "preflight-readiness.json"))]].map(async ([role, value]) => {
        const p = value as Pin; return { role, path: `${guestOutput}/${p.path.split("/").at(-1)}`, sha256: p.sha256, bytes: (await lstat(p.path)).size };
      }));
      sourceManifest = await write(join(output, "bridge-manifest.json"), { schema: "gptgrep.harbor-native-artifacts.v1", taskId: "synthetic-task", arm: "bare", readerJobSha256: readerJob.sha256,
        executionMode: "live", artifacts });
    }
    const hostRecheck = await recheck(`${id}.host-recheck.json`, startup);
    const sealValue = { schema: "environments-harness.container-storage-collection-seal.v1", ...ancestry, command_binding_sha256: binding.sha256, command_plan_sha256: commandPlan.sha256,
      container_binding_sha256: basePin.sha256, container_id: container.id, container_generation: container, source_originals_preserved: true, collection_allowed: true,
      container_may_remain_running: true, reservation_status: "held-until-outer-finalization", business_status: status, guest_returncode: returncode,
      projected_output_root: join(command.host_artifact_root, "projected"), guest_terminal: terminal, host_recheck: hostRecheck, collection_roots: collectionRoots, artifacts: await originals() };
    const seal = await put(`${id}.seal.json`, sealValue);
    const transportValue = { schema: "environments-harness.container-storage-transport.v1", command_id: id, owner_template_sha256: ownerTemplate.sha256, runtime_context_sha256: runtimeContext.sha256,
      expected_admission_path: admission.path, command_plan_sha256: commandPlan.sha256 };
    const transport = await put(`${id}.transport.json`, transportValue);
    const result = await put(`${id}.result.json`, { schema: "environments-harness.container-storage-run-result.v1", command_id: id, owner_template_sha256: ownerTemplate.sha256,
      admission_sha256: admission.sha256, command_plan: commandPlan, startup, ok: true, terminal_status: "verified-terminal", business_status: status, transport, collection_seal: seal });
    return { result, commandPlan, startup, binding, terminal, seal, hostRecheck, transport, sealValue };
  };
  const preflight = await phase("preflight"), reader = await phase("reader");
  const normalProbe = await put("normal-probe.json", { schema: "environments-harness.storage-receipt.v1", status: "checkpoint", plan_digest: sha256("parent-plan"), error: null, cleanup_performed: false });
  const brokerStatus = await put("broker-status.json", { fixture: "status" });
  const brokerRun = await put("broker-run.json", { run_id: "synthetic-auth", state: "PASS", layout: { auth_link: authLink }, auth_link_final: { released: true },
    sweep: { inconclusive: false, survivor_pids: [] }, credential_links: { complete: true, found: 0 }, child_exit_code: 0, auth_block: null });
  const brokerFinal = await put("broker-final.json", { schema: "environments-harness.container-broker-final-evidence.v1", broker_source: brokerSource, run_receipt: brokerRun, status_receipt: brokerStatus,
    lease_path: join(control, "lease"), lease_identity: { device: "1", inode: "2" }, job_label: "synthetic-job",
    kernel_lease_probe: "free", binding_matches: true, own_auth_link: authLink, own_auth_link_absent: true, auth_link_final_released: true, job_label_positively_observed: true,
    job_label_remaining: 0, guardian_retained_lease: false, sweep_inconclusive: false, credential_link_scan_complete: true, canonical_changed: false, credential_contents_read: false,
    unreleased_holders: [], sweep_survivor_pids: [], credential_link_scan_errors: [] });
  const finalValue = { schema: "environments-harness.container-storage-finalization.v1", ...ancestry, runtime_context_sha256: runtimeContext.sha256,
    stage: "post-broker", broker_final_evidence: brokerFinal, context_status: "observed", command_id: "reader", expected_admission_path: admission.path, container_binding_sha256: basePin.sha256, command_binding_sha256: reader.binding.sha256, command_plan_sha256: reader.commandPlan.sha256,
    container_id: container.id, terminal_status: "verified-terminal", business_status: business, reservation_status: "released-after-terminal-proof", retention: "retain-all-no-cleanup",
    owned_overlay_unmounted: true, auth_link_released: true, own_auth_link_absent: true, auth_docker_label_seen: true, auth_docker_label_remaining: 0,
    guest_startup: reader.startup, guest_terminal: reader.terminal, collection_seal: reader.seal, host_recheck: reader.hostRecheck, reservation, transport: reader.transport, normal_host_probe_receipt: normalProbe };
  const finalization = await put("finalization.json", finalValue);
  const options: ContainerReaderChainOptions = { ownerTemplate, runtimeContext, namespacePlanReview, derivedPlanRule, parentHostBinding: parent, admission, preflight, reader, finalization, parentHostBindingSha256: parent.sha256,
    readerJob, taskId: "synthetic-task", arm: "bare", sourceAttempt, sourceManifest, projectedOutputRoot: join(root, "reader/projected") };
  return { root, options, put, write, pin, finalValue, reader, output, cleanup: () => rm(root, { recursive: true, force: true }) };
}

test("container evidence preserves same host originals and separates business result from cleanup", async () => {
  for (const business of ["passed", "failed"] as const) {
    const f = await fixture(business);
    try {
      const result = await verifyContainerReaderChain(f.options);
      expect(result.businessStatus).toBe(business); expect(result.liveQualified).toBe(false);
      expect(result.readerBindingSha256).toBe(f.reader.binding.sha256);
      expect(result.artifactMap.get("/private/output/attempt.json")?.host_path).toBe(f.options.sourceAttempt.path);
    } finally { await f.cleanup(); }
  }
});

test("transport completion and collection never replace final cleanup proof", async () => {
  const f = await fixture();
  try {
    for (const change of [{ stage: "container" }, { context_status: "unavailable-before-setup" }, { terminal_status: "unknown-retained" }, { owned_overlay_unmounted: false }, { own_auth_link_absent: null }, { auth_docker_label_remaining: 1 },
      { reservation_status: "held-for-owner-reconciliation" }, { container_id: sha256("replacement") }, { business_status: "failed" }]) {
      const finalization = await f.write(f.options.finalization.path, { ...f.finalValue, ...change });
      await expect(verifyContainerReaderChain({ ...f.options, finalization })).rejects.toThrow();
    }
  } finally { await f.cleanup(); }
});

test("changed original bytes, copied addresses and replaced host inodes are rejected", async () => {
  const f = await fixture();
  try {
    const copy = await f.write(join(f.root, "copied-attempt.json"), JSON.parse(await readFile(f.options.sourceAttempt.path, "utf8")));
    await expect(verifyContainerReaderChain({ ...f.options, sourceAttempt: copy })).rejects.toThrow("copied");
    const path = join(f.output, "trace.json"), original = await readFile(path);
    await writeFile(path, "changed"); await expect(verifyContainerReaderChain(f.options)).rejects.toThrow("bytes changed");
    const replacement = join(f.root, "replacement-trace.json"); await writeFile(replacement, original); await rename(replacement, path);
    await expect(verifyContainerReaderChain(f.options)).rejects.toThrow("identity changed");
  } finally { await f.cleanup(); }
});

test("a foreign phase binding, task or arm cannot reuse a successful container chain", async () => {
  const f = await fixture();
  try {
    await expect(verifyContainerReaderChain({ ...f.options, reader: f.options.preflight })).rejects.toThrow("exact verified owner terminal");
    await expect(verifyContainerReaderChain({ ...f.options, parentHostBindingSha256: sha256("other-parent") })).rejects.toThrow("parent-bound");
    await expect(verifyContainerReaderChain({ ...f.options, taskId: "different-task" })).rejects.toThrow("task differs");
    await expect(verifyContainerReaderChain({ ...f.options, arm: "gptgrep" })).rejects.toThrow("arm");
  } finally { await f.cleanup(); }
});

test("reader source pins cannot nominate credential content as evidence", async () => {
  const f = await fixture();
  try {
    await expect(verifyContainerReaderChain({ ...f.options, sourceAttempt: { path: join(f.root, "auth.json"), sha256: sha256("not-read") } })).rejects.toThrow("Credential files");
  } finally { await f.cleanup(); }
});

test("a rehashed terminal with surviving new processes still fails cleanup verification", async () => {
  const f = await fixture();
  try {
    const plan = JSON.parse(await readFile(f.reader.commandPlan.path, "utf8"));
    const afterPath = join(f.root, "control", plan.guest_process_final_path.split("/").at(-1));
    const after = JSON.parse(await readFile(afterPath, "utf8"));
    after.processes.push({ pid: 55, start_ticks: "99", state: "S", parent: "1", group: "55", session: "55" }); const afterPin = await f.write(afterPath, after);
    const terminal = JSON.parse(await readFile(f.reader.terminal.path, "utf8"));
    terminal.process_final.sha256 = afterPin.sha256; const terminalPin = await f.write(f.reader.terminal.path, terminal);
    const seal = await f.put("reader.rehashed-seal.json", { ...f.reader.sealValue, guest_terminal: terminalPin });
    const result = JSON.parse(await readFile(f.reader.result.path, "utf8"));
    result.collection_seal = seal; const resultPin = await f.put("reader.rehashed-result.json", result);
    const finalization = await f.write(f.options.finalization.path, { ...f.finalValue, guest_terminal: terminalPin, collection_seal: seal });
    await expect(verifyContainerReaderChain({ ...f.options, reader: { ...f.options.reader, result: resultPin }, finalization })).rejects.toThrow("New processes remain");
  } finally { await f.cleanup(); }
});

test("terminal inventories accept the exact supervisor and terminal zombie or dead rows", async () => {
  const f = await fixture("passed", [
    { pid: 2, start_ticks: "2", state: "R", parent: "1", group: "2", session: "2" },
    { pid: 55, start_ticks: "99", state: "Z", parent: "1", group: "55", session: "55" },
    { pid: 56, start_ticks: "100", state: "X", parent: "1", group: "56", session: "56" },
  ]);
  try { await verifyContainerReaderChain(f.options); } finally { await f.cleanup(); }
});

test("a reused supervisor PID does not admit a new live process generation", async () => {
  const f = await fixture("passed", [{ pid: 2, start_ticks: "3", state: "S", parent: "1", group: "2", session: "2" }]);
  try { await expect(verifyContainerReaderChain(f.options)).rejects.toThrow("New processes remain"); } finally { await f.cleanup(); }
});

test("a terminal guarded-worker row cannot be hidden by the zombie exception", async () => {
  const f = await fixture("passed", [{ pid: 123, start_ticks: "100", state: "Z", parent: "1", group: "123", session: "123" }]);
  try { await expect(verifyContainerReaderChain(f.options)).rejects.toThrow("Guarded worker remains"); } finally { await f.cleanup(); }
});
