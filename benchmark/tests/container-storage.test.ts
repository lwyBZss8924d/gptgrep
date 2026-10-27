import { expect, test } from "bun:test";
import { chmod, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { digest, fileHash, processCommandIdentity, requireStorage, sha256, validateDecisionsHelperContract } from "../src/io";

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "guest-binding-contract-"));
  const old = [process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING, process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST, process.env.CODEX_HOME];
  const put = async (name: string, value: unknown) => {
    const path = join(root, name); await writeFile(path, JSON.stringify(value), { mode: 0o600 }); await chmod(path, 0o600);
    return { path, sha256: await fileHash(path) };
  };
  const job = await put("job.json", { schema: "test.immutable-job", options: {} });
  const acceptance = await put("acceptance.json", { schema: "test.acceptance" });
  const profile = await put("config.toml", { filesystem: { "/": "deny" } });
  process.env.CODEX_HOME = root;
  const paths = [
    { path: join(root, "task"), kind: "directory", access: "write", purpose: "workspace" },
    { path: join(root, "task/documents"), kind: "directory", access: "read", purpose: "raw-context" },
    { path: join(root, "output"), kind: "directory", access: "write", purpose: "private-capture" },
    { path: join(root, "app/answer.json"), kind: "file", access: "write", purpose: "answer-handoff" },
  ];
  const base: any = { schema: "environments-harness.container-storage-binding.v1", command_binding_required: true,
    custody_mode: "0600", mount_access: "read-only", custody_uid: process.getuid!(), admission_class: "reader",
    container: { id: sha256("container") }, owner_template_sha256: sha256("owner-template"), host_namespace_plan_sha256: sha256("namespace-plan"),
    parent_host_binding_sha256: sha256("host-binding"), parent_deadline_monotonic_ns: "99999999999999999",
    experiment_arm: "baseline", sandbox_privacy_qualification_sha256: sha256("synthetic-no-model-proof"), auth_projection_sha256: sha256("synthetic-auth-projection"),
    model_profile: profile, model_read_roots: [join(root, "task")], allowed_paths: paths };
  let basePin = await put("container-binding.json", base);
  const value: any = { schema: "environments-harness.container-command-binding.v1", command_id: "reader", admission_class: "reader",
    custody_mode: "0600", mount_access: "read-only", custody_uid: process.getuid!(),
    exact_owner_injected_env_names: ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"],
    plan: { command: { argv_sha256: (await processCommandIdentity()).argvSha256, executable: process.argv[0], executable_sha256: await fileHash(process.argv[0]!) },
      cwd: await realpath(process.cwd()), run_id: "test-reader", workload_sha256: sha256("static-workload") },
    container_binding: basePin, container_binding_guest_path: basePin.path, container_id: base.container.id,
    owner_template_sha256: base.owner_template_sha256, host_namespace_plan_sha256: base.host_namespace_plan_sha256,
    parent_host_binding_sha256: base.parent_host_binding_sha256, parent_deadline_monotonic_ns: base.parent_deadline_monotonic_ns,
    reservation: { path: "/host-only/reservation.json", sha256: sha256("reservation"), parent_plan_digest: sha256("parent-plan"), mode: "additional-container-reservation", transport_exit_is_terminal: false },
    acceptance, immutable_job: job, allowed_paths: paths,
    // Deliberately broad legacy hints must never widen the guest's typed grants.
    targets: { output: root, downloads: root, test_data: root } };
  const select = async () => {
    const pin = await put("command-binding.json", value);
    process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = pin.path; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = pin.sha256;
    return pin;
  };
  const repinBase = async () => { basePin = await put("container-binding.json", base); value.container_binding = basePin; return select(); };
  await select();
  return { root, value, base, job, acceptance, select, repinBase, put, dispose: async () => {
    for (const [i, key] of ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST", "CODEX_HOME"].entries()) {
      if (old[i] === undefined) delete process.env[key]; else process.env[key] = old[i];
    }
    await rm(root, { recursive: true, force: true });
  } };
}

// These are synthetic consumer-contract fixtures under the outer admitted test run,
// not observations of a Docker namespace, owner lease or model sandbox.
test("guest storage uses exact file grants and the most-specific intersection of command and base paths", async () => {
  const f = await fixture();
  try {
    const admitted = await requireStorage(join(f.root, "task/new.txt"), join(f.root, "output/attempt.json"), join(f.root, "app/answer.json"));
    expect(admitted.container?.commandId).toBe("reader"); expect(admitted.container?.immutableJob).toEqual(f.job);
    await expect(requireStorage(join(f.root, "app/other.json"))).rejects.toThrow("narrowed guest");
    await expect(requireStorage(join(f.root, "app/answer.json/child"))).rejects.toThrow("narrowed guest");
    await expect(requireStorage(join(f.root, "task/documents/replacement.pdf"))).rejects.toThrow("narrowed guest");
    f.value.allowed_paths = f.value.allowed_paths.filter((row: any) => row.access === "write"); await f.select();
    await expect(requireStorage(join(f.root, "task/documents/replacement.pdf"))).rejects.toThrow("narrowed guest");
    f.value.allowed_paths.push({ path: join(f.root, "elsewhere"), kind: "directory", access: "write", purpose: "unadmitted" }); await f.select();
    await expect(requireStorage(join(f.root, "elsewhere/file"))).rejects.toThrow("narrowed guest");
  } finally { await f.dispose(); }
});

test("guest binding rejects foreign argv, ancestry, phase, changed inputs and broad fallback", async () => {
  const f = await fixture();
  try {
    const original = structuredClone(f.value);
    for (const change of [
      (v: any) => { v.plan.command.argv_sha256 = sha256("other-command"); },
      (v: any) => { v.plan.cwd = "/different-cwd"; },
      (v: any) => { v.container_id = sha256("replacement-container"); },
      (v: any) => { v.parent_host_binding_sha256 = sha256("foreign-host"); },
      (v: any) => { v.command_id = "qualification"; },
      (v: any) => { v.reservation.transport_exit_is_terminal = true; },
      (v: any) => { v.schema = "environments-harness.container-storage-binding.v1"; },
      (v: any) => { v.schema = "host-looking-schema"; },
    ]) {
      Object.assign(f.value, structuredClone(original)); change(f.value); await f.select();
      await expect(requireStorage(join(f.root, "output/result.json"))).rejects.toThrow();
    }
    Object.assign(f.value, structuredClone(original)); await f.select();
    await f.put("job.json", { changed: true });
    await expect(requireStorage(join(f.root, "output/result.json"))).rejects.toThrow("pin changed");
  } finally { await f.dispose(); }
});

function helperJoinFixture() {
  const pin = (name: string) => ({ path: "/fixture/" + name, sha256: sha256(name) });
  const plan = sha256("current-B-plan"), profile = pin("config.toml"), binary = pin("codex");
  const processIdentity = { pid: 800, start_ticks: 10, pid_namespace: 30, cgroup: "/fixture/host-helper-B", boot_id: "fixture-boot" };
  const parent = { device: 1, inode: 2, uid: 1000, gid: 1000, mode: 0o700 }, socket = { ...parent, inode: 3, mode: 0o600 };
  const client: any = { schema_version: "gptgrep.decisions-helper-client.v1", instance: "1".repeat(32), task_binding_sha256: plan,
    socket_parent: "/run/gptgrep-decisions", parent_identity: parent, socket_identity: socket, server: processIdentity,
    server_uid: 1000, server_gid: 1000, model: "fixture-model", provider_disabled: false,
    budget: { run_id: "fixture-run", plan_digest: sha256("original-budget"), deadline_unix_ms: 1000000000, max_submissions: 1,
      clock_anchor: { schema: "gptgrep.run-clock-anchor.v1", deadline_unix_ms: 1000000000, clock_kind: "linux.clock_boottime",
        boot_identity: "fixture-boot", clock_namespace: "time:[1]", monotonic_started_ns: 1, monotonic_deadline_ns: 900 } } };
  const route = pin("A-qualified-route.json"), shipped = pin("A-shipped-cli-probe.json"), attestation = pin("current-B-ready.json");
  const selected: any = { schema: "environments-harness.container-decisions-helper-binding.v1", mode: "reader", namespace_plan_sha256: plan,
    startup_input: attestation, client_control: { path: "/run/gptgrep-decisions/client.json", sha256: sha256("current-B-client") },
    attestation: { path: "/control/current-ready.json", sha256: attestation.sha256 }, native_binary: binary, native_profile: profile,
    native_profile_id: "fixture-profile", allowed_unix_socket: "/run/gptgrep-decisions/decisions.sock", unix_socket_scope: "linux-container-fs-netns-v1",
    helper_generation: processIdentity, parent_identity: parent, socket_identity: socket, route_qualification: route, reused_shipped_cli_probe: shipped };
  const base: any = { decisions_helper: selected, custody_uid: 1000, experiment_arm: "treatment", admission_class: "reader",
    model_profile: profile, filesystem_policy_name: "fixture-profile", host_namespace_plan_sha256: plan,
    parent_host_binding_sha256: sha256("parent-owner"), parent_deadline_monotonic_ns: "1000",
    container: { id: sha256("current-B"), init_host_pid: 900, init_start_ticks: "20", boot_id: "fixture-boot",
      security: { resources: { host_mem_available_bytes: "100", host_observation_sha256: sha256("telemetry"), memory_max: "100" } } } };
  const value: any = { decisions_helper: selected, command_id: "reader", admission_class: "reader", host_namespace_plan_sha256: plan, container_id: base.container.id };
  const stable = structuredClone(base.container); delete stable.security.resources.host_mem_available_bytes; delete stable.security.resources.host_observation_sha256;
  const ready: any = { schema: "environments-harness.container-decisions-helper-attestation.v1", namespace_plan: { path: "/owner/plan.json", sha256: plan },
    helper: { binary: pin("host-helper"), source_manifest: pin("source.json"), server_control: pin("server.json"), readiness: pin("ready-line.json"), client_control: pin("host-client.json"), generation: processIdentity },
    endpoint: { host_parent: "/host/private-endpoint", guest_parent: "/run/gptgrep-decisions", guest_client_control: selected.client_control.path,
      socket_name: "decisions.sock", parent_identity: parent, socket_identity: socket, mount_access: "read-only", disjoint_collection_roots: true },
    caller: { container_id: base.container.id, container_generation: digest(stable), init: { pid: 900, start_ticks: 20, pid_namespace: 40, cgroup: "/fixture/container-B", boot_id: "fixture-boot" },
      scope: "selected-container-cgroup-descendants", native_inventory: pin("B-inventory.json"), shipped_cli_probe: shipped },
    owner: { binding: { path: "/owner/binding.json", sha256: base.parent_host_binding_sha256 }, lease_id: "fixture", lease_owner_pid: 901, lease_owner_start_ticks: 21, deadline_monotonic_ns: "1000" },
    budget: { private_control: pin("private-budget.json"), run_id: client.budget.run_id, plan_digest: client.budget.plan_digest, deadline_unix_ms: client.budget.deadline_unix_ms, max_submissions: 1 },
    native: { codex_binary: { ...binary }, profile: { ...profile }, profile_id: "fixture-profile", allowed_unix_socket: selected.allowed_unix_socket, tool_tcp_egress: "deny", qualification: route,
      linux_raw_unix_boundary: { schema: "environments-harness.linux-native-unix-boundary.v1", native_mode: "ProxyRouted-ProxyOnly", allow_all_unix_syscalls: true,
        filesystem_socket_authority: "one-selected-external-helper-endpoint", selected_socket: selected.allowed_unix_socket,
        tool_namespace_observation: pin("A-tool-namespaces.json"), filesystem_socket_inventory: pin("A-socket-inventory.json"),
        network_denial_evidence: pin("A-network-denials.json"), parent_api_proxy_separation: pin("A-parent-proxy-separation.json") } },
    privacy: { key_delivery: "encrypted-stdin-only", provider_key_in_argv_or_environments: false, key_files_retained: false, private_host_paths_not_mounted: true,
      dumpable: 0, core_soft_and_hard: 0, no_new_privs: 1, private_fds_cloexec: true, actual_pre_key_evidence: pin("B-pre-key.json") },
    lifecycle: { phase: "ready", original_deadline_monotonic_ns: "1000", helper_terminal: false, socket_cleanup: "defer-until-owner-proves-helper-terminal" } };
  ready.helper.client_control.sha256 = selected.client_control.sha256;
  const environment = { GPTGREP_DECISIONS_IPC_CONTROL: selected.client_control.path, GPTGREP_DECISIONS_IPC_SHA256: selected.client_control.sha256 };
  return { selected, base, value, ready, client, environment };
}

test("current live helper custody reuses only the immutable A route proof", () => {
  const f = helperJoinFixture();
  const current = validateDecisionsHelperContract(f.value, f.base, f.ready, f.client, f.environment);
  expect(current?.attestation).toEqual(f.selected.attestation);
  expect(current?.linuxRawUnixBoundary.tool_namespace_observation.path).toContain("A-tool");
  for (const mutate of [
    (v: any) => { v.ready.helper.generation = { ...v.client.server, start_ticks: 9 }; },
    (v: any) => { v.ready.caller.container_generation = sha256("old-container"); },
    (v: any) => { v.client.task_binding_sha256 = sha256("A-old-plan"); },
    (v: any) => { v.ready.owner.deadline_monotonic_ns = "1001"; },
    (v: any) => { v.ready.native.profile.sha256 = sha256("other-profile"); },
    (v: any) => { v.ready.caller.shipped_cli_probe = { path: "/fixture/B-fake-probe.json", sha256: sha256("B") }; },
    (v: any) => { v.ready.privacy.private_fds_cloexec = false; },
    (v: any) => { v.base.experiment_arm = "baseline"; },
  ]) {
    const changed = structuredClone(f); mutate(changed);
    expect(() => validateDecisionsHelperContract(changed.value, changed.base, changed.ready, changed.client, changed.environment)).toThrow();
  }
});

test("keyless helper qualification cannot become live admission or consume an enabled sentinel", () => {
  const f = helperJoinFixture();
  f.selected.mode = "qualification"; f.selected.attestation = null; f.selected.route_qualification = null; f.selected.reused_shipped_cli_probe = null;
  f.selected.startup_input = { path: "/fixture/A-client.json", sha256: f.selected.client_control.sha256 };
  f.value.command_id = "qualification"; f.value.admission_class = "bounded-qualification"; f.base.admission_class = "bounded-qualification";
  f.client.provider_disabled = true;
  expect(validateDecisionsHelperContract(f.value, f.base, null, f.client, f.environment)).toBeUndefined();
  expect(() => validateDecisionsHelperContract(f.value, f.base, f.ready, f.client, f.environment)).toThrow();
  f.client.provider_disabled = false;
  expect(() => validateDecisionsHelperContract(f.value, f.base, null, f.client, f.environment)).toThrow();
});
