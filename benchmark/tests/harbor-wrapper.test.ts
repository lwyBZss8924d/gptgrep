import { expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildHarborFinalizeCommand, harborWrapperBudget, makeHarborTransport, runHarborFinalizationSequence,
  compactProcProfile, expectedHarborEnvironment, validateHarborWrapperJob, verifyProcEnvironmentSelection,
  verifyReaderCommandSources, type HarborContainerOwner, type HarborOwnerWrapperJob } from "../src/harbor-wrapper";
import { fileHash, sha256 } from "../src/io";
import type { ProcessObservation } from "../src/process";

const pin = (name: string) => ({ path: `/fixture/${name}`, sha256: sha256(name) });
const owner: HarborContainerOwner = { schema: "gptgrep.harbor-container-owner.v1", interfaceContract: pin("interface.json"), python: pin("python"), cli: pin("owner-cli"),
  template: pin("template.json"), derivedPlanRule: pin("rule.json"), parentStorage: { runId: "parent", workloadSha256: sha256("workload"), cwd: "/fixture/work" },
  harborSources: { docker: pin("docker.py"), trial: pin("trial.py"), base: pin("base.py"), prebuilt: pin("prebuilt.yaml") }, hostEpisodeRoot: "/fixture/episode",
  runtimeContextPath: "/fixture/episode/runtime-context.json", authProjectionPath: "/fixture/episode/auth-projection.json", networkEvidencePath: "/fixture/episode/network.json",
  namespacePlanReviewPath: "/fixture/episode/namespace-review.json", phaseEvidenceRoot: "/fixture/episode/phases", ownerCallTimeoutSeconds: 10 };
const template = { outputs: { admission: "/fixture/episode/admission.json" }, host_source_closure: [owner.cli] };
const runtimeContext = { path: owner.runtimeContextPath, sha256: sha256("observed runtime context") };
const parentBinding = pin("actual-parent-binding.json"), transport = pin("actual-transport.json"), broker = pin("actual-broker-proof.json");
const command = { executable: pin("python"), inputs: [pin("source.py")], argv: ["-B", "/fixture/source.py"], cwd: "/fixture/work", timeoutMs: 600_000, maxOutputBytes: 1024 };
const job: HarborOwnerWrapperJob = { schema: "gptgrep.harbor-owner-wrapper.v1", harborJob: pin("job.json"), bridgeContract: pin("bridge.json"), authHarbor: command,
  brokerFinalEvidence: { command: { ...command, timeoutMs: 30_000 }, path: "/fixture/episode/broker-proof.json" }, outputPath: "/fixture/episode/wrapper.json", timeoutMs: 900_000 };
function observation(change: Partial<ProcessObservation> = {}): ProcessObservation {
  return { startedAt: "2026-01-01T00:00:00.000Z", completedAt: "2026-01-01T00:00:01.000Z", latencyMs: 1000, started: true, pid: 42, executionError: null,
    exitCode: 0, signal: null, timedOut: false, cleanup: true, outputExceeded: false, outputBytes: 7, outputSha256: sha256("stdout!"),
    stdoutBytes: 6, stdoutSha256: sha256("stdout"), stderrBytes: 1, stderrSha256: sha256("!"), ...change };
}
const transportOptions = { ownerTemplate: owner.template, runtimeContext, commandId: "reader" as const, expectedAdmissionPath: template.outputs.admission,
  commandPlan: pin("actual-reader-plan.json"), observerSourceSha256: sha256("wrapper source") };

async function sourceFixture() {
  const root = await realpath(await mkdtemp(join(tmpdir(), "reader-command-sources-"))), source = join(root, "source");
  await mkdir(source);
  for (const [name, bytes] of [["bun", "fixture runtime"], ["controller.ts", "fixture source"], ["empty.bunfig.toml", ""]]) await writeFile(join(source, name!), bytes!);
  const hostJob = join(root, "pre-materialized-job.json"); await writeFile(hostJob, "{}");
  const guest = async (name: string) => ({ path: `/guest/runtime/${name}`, sha256: await fileHash(join(source, name)) });
  const bridge = { bun: await guest("bun"), bunConfig: await guest("empty.bunfig.toml"), entrypoint: await guest("controller.ts"),
    readerJob: { path: "/guest/control/reader-job.json", sha256: await fileHash(hostJob) } };
  const mount = { host_source: source, guest_target: "/guest/runtime", kind: "directory", access: "read", propagation: "rprivate", create_host_path: false };
  const template = { mounts: [mount, { ...mount, host_source: join(root, "not-materialized"), guest_target: "/guest/control" }] };
  const command = { id: "reader", immutable_job_guest_path: bridge.readerJob.path, immutable_job: { path: hostJob, sha256: bridge.readerJob.sha256 },
    source_pins: [bridge.bun, bridge.bunConfig, bridge.entrypoint, bridge.readerJob],
    controller_argv: [bridge.bun.path, "--no-env-file", "--no-install", `--config=${bridge.bunConfig.path}`, bridge.entrypoint.path,
      "--mode", "reader", "--job", bridge.readerJob.path, "--expect-sha256", bridge.readerJob.sha256,
      "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"] };
  return { root, source, bridge, template, command, cleanup: () => rm(root, { recursive: true, force: true }) };
}

test("guarded reader source verification maps guest paths and retains the unmaterialized job host pin", async () => {
  const f = await sourceFixture();
  try { await verifyReaderCommandSources(f.template, f.bridge, f.command); } finally { await f.cleanup(); }
});

test("reader argv refuses missing loader guards and an unpinned configuration", async () => {
  const f = await sourceFixture();
  try {
    for (const flag of f.command.controller_argv.slice(1, 4)) {
      await expect(verifyReaderCommandSources(f.template, f.bridge, { ...f.command, controller_argv: f.command.controller_argv.filter(arg => arg !== flag) })).rejects.toThrow("guarded detached");
    }
    await expect(verifyReaderCommandSources(f.template, f.bridge, { ...f.command, source_pins: f.command.source_pins.filter(value => value !== f.bridge.bunConfig) })).rejects.toThrow("source closure");
    await expect(verifyReaderCommandSources(f.template, f.bridge, { ...f.command, immutable_job: { ...f.command.immutable_job, sha256: sha256("other job") } })).rejects.toThrow("host input");
  } finally { await f.cleanup(); }
});

test("reader source mapping rejects ambiguous, writable and changed configuration binds", async () => {
  const f = await sourceFixture();
  try {
    const mount = f.template.mounts[0]!;
    await expect(verifyReaderCommandSources({ mounts: [mount, mount] }, f.bridge, f.command)).rejects.toThrow("unambiguous");
    await expect(verifyReaderCommandSources({ mounts: [{ ...mount, access: "write" }] }, f.bridge, f.command)).rejects.toThrow("read-only");
    await expect(verifyReaderCommandSources({ mounts: [{ ...mount, kind: "file" }] }, f.bridge, f.command)).rejects.toThrow("unambiguous");
    await writeFile(join(f.source, "empty.bunfig.toml"), "[install]\nauto = true\n");
    await expect(verifyReaderCommandSources(f.template, f.bridge, f.command)).rejects.toThrow();
  } finally { await f.cleanup(); }
});

// Pure source-contract fixtures: no commands, owner leases, auth probes or containers run.
test("wrapper admission reserves auth, broker proof and both finalization calls including cleanup grace", () => {
  expect(harborWrapperBudget(job, owner)).toBe(658_000);
  expect(harborWrapperBudget({ ...job, authHarbor: { ...command, timeoutMs: 850_000 } }, owner)).toBeGreaterThan(job.timeoutMs);
});

test("container finalization uses the observed context and exact actual parent binding", () => {
  const result = buildHarborFinalizeCommand({ owner, template, parentBinding, runtimeContext, commandId: "reader", transport, stage: "container", brokerFinalEvidence: null });
  expect(result.executable).toEqual(owner.python);
  expect(result.argv).toEqual(["-I", "-B", owner.cli.path, "storage-container", "finalize", "--template", owner.template.path, "--expect-template-digest", owner.template.sha256,
    "--runtime-context", runtimeContext.path, "--expect-context-digest", runtimeContext.sha256, "--admission-path", template.outputs.admission,
    "--command-id", "reader", "--transport-result", transport.path, "--expect-transport-digest", transport.sha256, "--stage", "container",
    "--parent-binding", parentBinding.path, "--expect-parent-digest", parentBinding.sha256]);
  expect(result.inputs).toContainEqual(parentBinding); expect(result.inputs).toContainEqual(runtimeContext);
  expect(result.timeoutMs).toBe(10_000); expect(result.argv).not.toContain("--broker-final-evidence");
});

test("failure before setup retains the expected context path without inventing its digest", () => {
  const result = buildHarborFinalizeCommand({ owner, template, parentBinding, runtimeContext: null, commandId: "admit", transport, stage: "container", brokerFinalEvidence: null });
  expect(result.argv).toContain("--context-unavailable"); expect(result.argv).not.toContain("--expect-context-digest");
  expect(result.argv[result.argv.indexOf("--runtime-context") + 1]).toBe(owner.runtimeContextPath);
  expect(result.argv[result.argv.indexOf("--command-id") + 1]).toBe("admit");
});

test("post-broker finalization requires a real evidence pin and cannot use a future placeholder", () => {
  const selected = { owner, template, parentBinding, runtimeContext, commandId: "reader" as const, transport, stage: "post-broker" as const };
  expect(() => buildHarborFinalizeCommand({ ...selected, brokerFinalEvidence: null })).toThrow("actual broker evidence");
  const result = buildHarborFinalizeCommand({ ...selected, brokerFinalEvidence: broker });
  expect(result.argv.slice(-4)).toEqual(["--broker-final-evidence", broker.path, "--expect-broker-digest", broker.sha256]);
  expect(result.inputs).toContainEqual(broker);
  expect(() => buildHarborFinalizeCommand({ ...selected, brokerFinalEvidence: { path: broker.path, sha256: "FUTURE_SHA" } })).toThrow();
});

test("finalizer inputs reject conflicting source bytes and another runtime-context address", () => {
  const selected = { owner, template, parentBinding, runtimeContext, commandId: "reader" as const, transport, stage: "container" as const, brokerFinalEvidence: null };
  expect(() => buildHarborFinalizeCommand({ ...selected, runtimeContext: pin("foreign-context.json") })).toThrow("context path differs");
  expect(() => buildHarborFinalizeCommand({ ...selected, template: { ...template, host_source_closure: [{ ...owner.cli, sha256: sha256("other source") }] } })).toThrow("Conflicting");
});

test("a returned failed business command keeps its actual nonzero return code and stream hashes", () => {
  const result = makeHarborTransport({ ...transportOptions, observation: observation({ exitCode: 2 }) });
  expect(result.result).toBe("returned"); expect(result.observed_returncode).toBe(2);
  expect(result.stdout_bytes).toBe("6"); expect(result.stdout_sha256).toBe(sha256("stdout")); expect(result.stderr_bytes).toBe("1");
  expect(result.stderr_sha256).toBe(sha256("!")); expect(result.context_status).toBe("observed"); expect(result.wrapper_argv_sha256).toBeNull();
});

test("prelaunch failure has no runtime context or command plan and is never a synthetic started run", () => {
  const result = makeHarborTransport({ ...transportOptions, runtimeContext: null, commandPlan: null, commandId: "admit", observation: null });
  expect(result.result).toBe("not-started"); expect(result.runtime_context_sha256).toBeNull(); expect(result.command_plan_sha256).toBeNull();
  expect(result.context_status).toBe("unavailable-before-setup"); expect(result.observed_returncode).toBeNull();
  expect(result.stdout_bytes).toBe("0"); expect(result.stdout_sha256).toBe(sha256(""));
});

test("started transport failures preserve observed bytes and distinguish timeout, error and lost cleanup", () => {
  for (const [change, status] of [[{ timedOut: true, signal: "SIGTERM", exitCode: null }, "timeout"],
    [{ executionError: "observed spawn or stream error" }, "error"], [{ outputExceeded: true }, "error"], [{ cleanup: false }, "lost"]] as const) {
    const result = makeHarborTransport({ ...transportOptions, observation: observation(change) });
    expect(result.result).toBe(status); expect(result.stdout_sha256).toBe(sha256("stdout")); expect(result.stderr_bytes).toBe("1");
  }
  expect(() => makeHarborTransport({ ...transportOptions, observation: observation({ stdoutSha256: "unknown" }) })).toThrow("Actual stream");
});

test("auth and container-finalizer errors still attempt proof production and the post-broker stage", async () => {
  const order: string[] = [];
  const stage = (name: string, fail: boolean) => async () => { order.push(name); if (fail) throw new Error(`${name} failed`); return name; };
  const result = await runHarborFinalizationSequence({ authHarbor: stage("auth", true), container: stage("container", true), brokerEvidence: stage("broker", true), postBroker: stage("post", false) });
  expect(order).toEqual(["auth", "container", "broker", "post"]);
  expect(result.authHarbor.error).toBe("auth failed"); expect(result.container.error).toBe("container failed");
  expect(result.brokerEvidence.error).toBe("broker failed"); expect(result.postBroker.value).toBe("post");
});

test("returned business failures do not suppress ordered owner reconciliation", async () => {
  const order: string[] = [];
  const stage = (name: string, value: number) => async () => { order.push(name); return value; };
  const result = await runHarborFinalizationSequence({ authHarbor: stage("auth", 2), container: stage("container", 0), brokerEvidence: stage("broker", 0), postBroker: stage("post", 0) });
  expect(order).toEqual(["auth", "container", "broker", "post"]); expect(result.authHarbor).toEqual({ value: 2, error: null });
});

test("wrapper job rejects an unknown contract or an unbounded total timeout before reading pins", async () => {
  await expect(validateHarborWrapperJob({ ...job, schema: "other" } as any)).rejects.toThrow("Invalid bounded");
  await expect(validateHarborWrapperJob({ ...job, timeoutMs: 900_001 })).rejects.toThrow("Invalid bounded");
  await expect(validateHarborWrapperJob({ ...job, futureBindingSha256: sha256("future") } as any)).rejects.toThrow("unknown fields");
});

async function procFixture() {
  const root = await realpath(await mkdtemp(join(tmpdir(), "proc-owner-selection-")));
  const write = async (name: string, value: string | object) => {
    const target = join(root, name); await writeFile(target, typeof value === "string" ? value : JSON.stringify(value));
    return { path: target, sha256: await fileHash(target) };
  };
  const sources = Object.fromEntries(await Promise.all(["docker", "unix", "runtime", "factory", "config", "trial", "base", "prebuilt"]
    .map(async name => [name, await write(name + ".source", name)])));
  const module = await write("proc_environment.py", "fixture source, never imported"), compose = await write("compose.json", {});
  const binary = await write("docker", "fixture executable, never run"), plugin = await write("compose", "fixture plugin, never run");
  const profileRaw = '{\n "defaultAction": "SCMP_ACT_ERRNO", "comment": "keep  two spaces and \\u00e9"\n}\n';
  const profile = await write("seccomp.json", profileRaw);
  const socket = { path: "/fixture/docker.sock", identity: { device: "1", inode: "2", uid: 0, gid: 1000, mode: "0660" },
    caller_uid: 1000, caller_gid: 1000, required_group: 1000, type: "socket" };
  const security = ["no-new-privileges:true", "seccomp=" + compactProcProfile(profileRaw), "apparmor=unit-profile"];
  const policy = { readonly_rootfs: true, privileged: false, cap_drop: ["ALL"], ipc_mode: "private", cgroupns_mode: "private", network_mode: "none",
    security_options: security, masked_paths: ["/sys/devices/virtual/powercap", "/sys/firmware"], readonly_paths: [],
    memory_bytes: "4294967296", docker_memory_swap_bytes: "4294967296", nano_cpus: "2000000000", pids_limit: "256",
    seccomp_mode: "explicit-profile", seccomp_profile: profile, no_new_privileges: true, restart_policy: "no", log_driver: "none",
    cgroup_memory_swap_max: "0", cpu_period: "100000", cpu_quota: "200000", apparmor_profile: "unit-profile",
    tmpfs: [{ target: "/dev", maximum_bytes: "67108864", options: [] }, { target: "/dev/shm", maximum_bytes: "67108864", options: [] }] };
  const image = `sha256:${sha256("unit image")}`;
  const selectedOwner = structuredClone(owner);
  selectedOwner.harborSources = { docker: sources.docker, trial: sources.trial, base: sources.base, prebuilt: sources.prebuilt };
  const selectedTemplate: any = { image_id: image, image_pin: image, reader_uid: 1000, reader_gid: 1000,
    engine: { socket, binary, compose_plugin: plugin, client_config_root: "/fixture/client-config" }, compose_profile: compose,
    mounts: [{ host_source: "/fixture/task", guest_target: "/app/task", access: "read", propagation: "rprivate", create_host_path: false }],
    allowed_paths: [{ path: "/app/task", kind: "directory" }], container_policy: policy, host_source_closure: [] };
  const createRequest = { Image: image, User: "1000:1000", WorkingDir: "/app/task", Labels: {
    "com.docker.compose.project": "@HARBOR_PROJECT@", "com.docker.compose.service": "main", "com.docker.compose.container-number": "1",
    "com.docker.compose.oneoff": "False", "com.docker.compose.config-hash": sha256("unit create source") }, HostConfig: {
      ReadonlyRootfs: true, Privileged: false, CapDrop: ["ALL"], IpcMode: "private", CgroupnsMode: "private", NetworkMode: "none",
      SecurityOpt: security, MaskedPaths: policy.masked_paths, ReadonlyPaths: [], Memory: 4294967296, MemorySwap: 4294967296,
      NanoCpus: 2000000000, PidsLimit: 256, ShmSize: 67108864,
      Mounts: [{ Type: "bind", Source: "/fixture/task", Target: "/app/task", ReadOnly: true, BindOptions: { Propagation: "rprivate" } }] } };
  const contract: any = { schema: "gptgrep.harbor-proc-environment.v1", module,
    harborSources: Object.fromEntries(["docker", "unix", "runtime", "factory", "config"].map(key => [key, sources[key]])),
    sessionDerivation: { rule: "harbor-trial-environment-v1", source: sources.trial }, environmentDirectory: "/fixture/task-environment",
    composeProfile: compose, engineSocket: socket, engineBinary: binary, composePlugin: plugin, engineClientConfigRoot: "/fixture/client-config", createRequest };
  const refresh = async () => {
    selectedOwner.procEnvironment = { module, contract: await write("contract.json", contract) };
    selectedTemplate.host_source_closure = [module, selectedOwner.procEnvironment.contract, ...Object.values(contract.harborSources), sources.trial];
  };
  await refresh();
  return { root, owner: selectedOwner, template: selectedTemplate, contract, profileRaw, refresh,
    verify: () => verifyProcEnvironmentSelection(selectedOwner, selectedTemplate, "/fixture/task-environment"),
    cleanup: () => rm(root, { recursive: true, force: true }) };
}

test("selected proc adapter binds source closure and exact owner request controls before Harbor starts", async () => {
  const f = await procFixture();
  try {
    expect(await f.verify()).toContainEqual(f.owner.procEnvironment!.contract);
    const environment = expectedHarborEnvironment(f.template, f.owner.procEnvironment);
    expect(environment.import_path).toBe("proc_environment:ProcOverlayDockerEnvironment");
    expect(environment.mounts).toEqual([]); expect(environment.kwargs).not.toHaveProperty("auto_mounting");
    expect(environment.kwargs).toHaveProperty("proc_contract_sha256", f.owner.procEnvironment!.contract.sha256);
  } finally { await f.cleanup(); }
});

test("selected proc adapter refuses missing source closure, input drift and another runtime directory", async () => {
  for (const change of ["closure", "source", "directory", "derivation"] as const) {
    const f = await procFixture();
    try {
      if (change === "closure") f.template.host_source_closure = f.template.host_source_closure.slice(0, -1);
      if (change === "source") await writeFile(f.owner.procEnvironment!.module.path, "changed source");
      if (change === "directory") { f.contract.environmentDirectory = "/other/task"; await f.refresh(); }
      if (change === "derivation") { f.contract.sessionDerivation.rule = "guessed-name"; await f.refresh(); }
      await expect(f.verify()).rejects.toThrow();
    } finally { await f.cleanup(); }
  }
});

test("selected proc adapter rejects broadened masks, mismatched binds and inline seccomp token drift", async () => {
  for (const change of ["mask", "mount", "seccomp", "profile-digest", "resource"] as const) {
    const f = await procFixture();
    try {
      const host = f.contract.createRequest.HostConfig;
      if (change === "mask") { host.MaskedPaths = []; f.template.container_policy.masked_paths = []; }
      if (change === "mount") host.Mounts.push({ ...host.Mounts[0], Target: "/private" });
      if (change === "seccomp") {
        host.SecurityOpt = host.SecurityOpt.map((value: string) => value.replace("\\u00e9", "é"));
        f.template.container_policy.security_options = host.SecurityOpt;
      }
      if (change === "profile-digest") await writeFile(f.template.container_policy.seccomp_profile.path, f.profileRaw + " ");
      if (change === "resource") host.Memory = 8589934592;
      await f.refresh(); await expect(f.verify()).rejects.toThrow();
    } finally { await f.cleanup(); }
  }
});
