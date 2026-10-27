import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, readdir, rm, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { reserveEpisode, validateEnvelope, validateHarborJob, verifyBatchAcceptance, verifyOwnerTerminal, verifyStorageTerminal, verifyRelocatedAttempt, type CampaignEnvelope } from "../src/controller";
import { createNativeMediaCapture } from "../src/trajectory";
import { CAMPAIGN_PURPOSES, type CampaignEnvelopeV2 } from "../src/campaign";
import { fileHash, sha256, writeJsonExclusive } from "../src/io";

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "controller-contract-"));
  const old = { binding: process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING, digest: process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST };
  const write = async (name: string, value: unknown) => {
    const path = join(root, name); await writeFile(path, JSON.stringify(value)); return { path, sha256: await fileHash(path) };
  };
  const binding = await write("test-binding.json", { targets: { output: root } });
  process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = binding.path; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = binding.sha256;
  const envelope: CampaignEnvelope = { schema: "gptgrep.live-campaign-envelope.v2", campaignId: "test-campaign", ledgerRoot: join(root, "ledger"),
    allowances: Object.fromEntries(CAMPAIGN_PURPOSES.map(purpose => [purpose, { maxAdmissions: purpose === "reader" || purpose === "judge" ? 2 : purpose === "probe" ? 0 : 1, reservedBytesPerAdmission: 1000, deadlineMs: 1000 }])) as CampaignEnvelopeV2["allowances"],
    episodeDeadlineMs: 1000, campaignDeadline: new Date(Date.now() + 60_000).toISOString(), maxReservedBytes: 7000, baseReservedBytes: 0,
    maxTraceBytes: 1024, maxMediaBytes: 1024, maxTrajectoryBytes: 1024, nativeDeadlineMs: 500, observationLimits: { toolCalls: 4, childThreads: 1, retryEvents: 0 }, providerRequestLimit: null, dollarLimitUsd: null };
  const envelopePin = await write("envelope.json", envelope);
  return { root, write, binding, envelope, envelopePin, cleanup: async () => {
    for (const [key, value] of [["ENVIRONMENTS_HARNESS_STORAGE_BINDING", old.binding], ["ENVIRONMENTS_HARNESS_STORAGE_DIGEST", old.digest]]) {
      if (value === undefined) delete process.env[key!]; else process.env[key!] = value;
    }
    await rm(root, { recursive: true, force: true });
  } };
}

test("campaign reservations charge failed/uncertain slots across experiments and reject duplicates", async () => {
  const f = await fixture();
  try {
    const commandSha256 = sha256("reviewed command");
    const first = await reserveEpisode(f.envelopePin, { experimentId: "experiment-a", slotId: "slot-a", purpose: "reader", commandSha256 });
    expect(JSON.parse(await readFile(first.path, "utf8")).disposition).toBe("charged_before_dispatch");
    await expect(reserveEpisode(f.envelopePin, { experimentId: "experiment-a", slotId: "slot-a", purpose: "reader", commandSha256 })).rejects.toThrow();
    await reserveEpisode(f.envelopePin, { experimentId: "experiment-b", slotId: "slot-b", purpose: "reader", commandSha256 });
    await expect(reserveEpisode(f.envelopePin, { experimentId: "experiment-c", slotId: "slot-c", purpose: "reader", commandSha256 })).rejects.toThrow("allowance exhausted");
    await expect(reserveEpisode(f.envelopePin, { experimentId: "experiment-c", slotId: "probe", purpose: "probe", commandSha256 })).rejects.toThrow("allowance exhausted");
    expect((await readdir(f.envelope.ledgerRoot)).filter(name => name.endsWith(".reservation.json"))).toHaveLength(2);
  } finally { await f.cleanup(); }
});

test("bounded answer serialization creates no oversized payload and refuses overwrite", async () => {
  const f = await fixture();
  try {
    const parent = join(f.root, "new-output"), path = join(parent, "answer.json");
    await expect(writeJsonExclusive(path, { answer: "x".repeat(4 * 1024 * 1024) }, 4 * 1024 * 1024)).rejects.toThrow("byte bound");
    await expect(readdir(parent)).rejects.toThrow();
    await writeJsonExclusive(path, { answer: "first" }, 4 * 1024 * 1024);
    const hash = await fileHash(path);
    await expect(writeJsonExclusive(path, { answer: "second" }, 4 * 1024 * 1024)).rejects.toThrow();
    expect(await fileHash(path)).toBe(hash); expect(await readdir(parent)).toEqual(["answer.json"]);
  } finally { await f.cleanup(); }
});

test("campaign replacement, unknown dollar ceiling and deadline exhaustion cannot broaden admission", async () => {
  const f = await fixture();
  try {
    const dispatch = { experimentId: "experiment", slotId: "first", purpose: "reader" as const, commandSha256: sha256("command") };
    await reserveEpisode(f.envelopePin, dispatch);
    const changed = await f.write("changed.json", { ...f.envelope, allowances: { ...f.envelope.allowances, reader: { ...f.envelope.allowances.reader, maxAdmissions: 3 } } });
    await expect(reserveEpisode(changed, { ...dispatch, slotId: "second" })).rejects.toThrow("cannot reset");
    expect(() => validateEnvelope({ ...f.envelope, dollarLimitUsd: 0 } as any)).toThrow("invented");
    const expired = await f.write("expired.json", { ...f.envelope, ledgerRoot: join(f.root, "expired-ledger"), campaignDeadline: new Date(0).toISOString() });
    await expect(reserveEpisode(expired, dispatch)).rejects.toThrow("deadline");
  } finally { await f.cleanup(); }
});

test("Harbor job refuses defaults, stock route, retries, extra tasks and automatic unbound judging", () => {
  const pin = { path: "/private/bridge.json", sha256: sha256("bridge") };
  const bridge = { schema: "gptgrep.harbor-native-bridge.v2", taskId: "task", arm: "bare", hostOutputRoot: "/private/collected", model: "gpt-6.1-sol" };
  const template = { image_pin: `sha256:${sha256("image")}`, compose_profile: { path: "/private/compose.yaml", sha256: sha256("compose") }, mounts: [] };
  const slot = { taskId: "task", arm: "bare" as const, attemptPath: "/private/collected/attempt.json" };
  const job = { n_attempts: 1, n_concurrent_trials: 1, retry: { max_retries: 0 }, timeout_multiplier: 1, jobs_dir: "/private/jobs", tasks: [{ path: "/task" }],
    environment: { type: "docker", import_path: null, force_build: false, delete: true, stream: false, extra_docker_compose: [template.compose_profile.path], mounts: [], kwargs: { enable_environment_dir_upload: false, auto_mounting: false } },
    verifier: { disable: true }, agents: [{ import_path: "native_agent:NativeCodexAgent", model_name: "gpt-6.1-sol", n_concurrent: 1,
      kwargs: { bridge_contract_path: pin.path, bridge_contract_sha256: pin.sha256 } }] };
  expect(() => validateHarborJob(job, pin, bridge, slot, template)).not.toThrow();
  for (const changed of [{ ...job, n_concurrent_trials: 4 }, { ...job, retry: { max_retries: 1 } }, { ...job, tasks: [...job.tasks, ...job.tasks] },
    { ...job, verifier: { disable: false } }, { ...job, environment: { ...job.environment, force_build: true } },
    { ...job, agents: [{ ...job.agents[0], import_path: "codex" }] }]) expect(() => validateHarborJob(changed, pin, bridge, slot, template)).toThrow();
});

test("Harbor proc environment is an exact selected import path with no implicit user binds", () => {
  const pin = { path: "/private/bridge.json", sha256: sha256("bridge") };
  const procEnvironment = { module: { path: "/private/proc_environment.py", sha256: sha256("module") },
    contract: { path: "/private/proc.json", sha256: sha256("contract") } };
  const bridge = { schema: "gptgrep.harbor-native-bridge.v2", taskId: "task", arm: "bare", hostOutputRoot: "/private/collected", model: "gpt-6.1-sol",
    containerOwner: { procEnvironment } };
  const template = { image_pin: `sha256:${sha256("image")}`, compose_profile: { path: "/private/compose.yaml", sha256: sha256("compose") }, mounts: [] };
  const slot = { taskId: "task", arm: "bare" as const, attemptPath: "/private/collected/attempt.json" };
  const job = { n_attempts: 1, n_concurrent_trials: 1, retry: { max_retries: 0 }, timeout_multiplier: 1, jobs_dir: "/private/jobs", tasks: [{ path: "/task" }],
    environment: { type: null, import_path: "proc_environment:ProcOverlayDockerEnvironment", force_build: false, delete: true, stream: false,
      extra_docker_compose: [template.compose_profile.path], mounts: [], kwargs: { enable_environment_dir_upload: false,
        proc_contract_path: procEnvironment.contract.path, proc_contract_sha256: procEnvironment.contract.sha256 } },
    verifier: { disable: true }, agents: [{ import_path: "native_agent:NativeCodexAgent", model_name: "gpt-6.1-sol", n_concurrent: 1,
      kwargs: { bridge_contract_path: pin.path, bridge_contract_sha256: pin.sha256 } }] };
  expect(() => validateHarborJob(job, pin, bridge, slot, template)).not.toThrow();
  for (const environment of [{ ...job.environment, type: "docker" }, { ...job.environment, import_path: "foreign:Environment" },
    { ...job.environment, mounts: [{ type: "bind", source: "/other", target: "/logs/agent" }] },
    { ...job.environment, kwargs: { ...job.environment.kwargs, auto_mounting: false } },
    { ...job.environment, kwargs: { ...job.environment.kwargs, proc_contract_sha256: sha256("other contract") } }]) {
    expect(() => validateHarborJob({ ...job, environment }, pin, bridge, slot, template)).toThrow("exact prebuilt");
  }
  expect(() => validateHarborJob(job, pin, { ...bridge, containerOwner: {} }, slot, template)).toThrow("exact prebuilt");
});

test("owner terminal checks consume actual native cleanup receipts rather than a synthetic acceptance wrapper", async () => {
  const f = await fixture();
  try {
    const startup = await f.write("startup.json", { child: "test" });
    const storage = await f.write("storage.json", { schema: "environments-harness.storage-receipt.v1", status: "success", plan_digest: sha256("plan"),
      error: null, worker: { terminal: true, returncode: 0, reason: null, startup_receipt: startup }, internal_metadata: { ceiling_met: true },
      binding_path: f.binding.path, binding_sha256: f.binding.sha256 });
    const runDir = join(f.root, "auth", "reader", "test-auth"), codexHome = join(runDir, "codex-home");
    await mkdir(codexHome, { recursive: true });
    const auth = { schema: "gptgrep.codex-auth.run.v1", operation: "run", role: "reader", run_id: "test-auth", state: "FAILED",
      layout: { run_dir: runDir, codex_home: codexHome, auth_link: join(codexHome, "auth.json") },
      sweep: { inconclusive: false, survivor_pids: [] }, credential_links: { complete: true, found: 0 },
      auth_link_final: { released: true }, auth_block: null, child_exit_code: 2 };
    const authName = "auth/reader/test-auth/receipt.json", authPin = await f.write(authName, auth);
    const paths = { storage: storage.path, auth: authPin.path, storagePlanDigest: sha256("plan"), authRunId: "test-auth" };
    expect((await verifyOwnerTerminal(paths)).bindingSha256).toBe(f.binding.sha256);
    expect((await verifyStorageTerminal(paths)).bindingSha256).toBe(f.binding.sha256);
    await symlink(join(f.root, "absent-secret-target"), auth.layout.auth_link);
    await expect(verifyOwnerTerminal(paths)).rejects.toThrow("link present");
    await unlink(auth.layout.auth_link);
    await f.write(authName, { ...auth, sweep: { inconclusive: false, survivor_pids: [42] } });
    await expect(verifyOwnerTerminal(paths)).rejects.toThrow("cleanup");
    await f.write("storage.json", { schema: "gptgrep.controller-storage-terminal.v1", status: "complete", cleanup: true });
    await expect(verifyOwnerTerminal(paths)).rejects.toThrow("Storage native");
  } finally { await f.cleanup(); }
});

test("Harbor relocation can change only verified artifact addresses", async () => {
  const f = await fixture();
  try {
    const answer = await f.write("answer.json", { answer: "source" }), trace = await f.write("trace.json", { event: "tool" });
    const source = { schema: "gptgrep.reader-attempt.v1", status: "completed", answerPath: "/container/answer.json", answerSha256: answer.sha256, native: { tracePath: "/container/trace.json", requestedModel: "gpt-6.1-sol" } };
    const sourceAttempt = await f.write("source-attempt.json", source), sourceManifest = await f.write("manifest.json", { schema: "gptgrep.harbor-native-artifacts.v1", executionMode: "live", artifacts: [
      { role: "attempt", path: "/container/attempt.json", sha256: sourceAttempt.sha256 },
      { path: "/container/answer.json", sha256: answer.sha256 }, { path: "/container/trace.json", sha256: trace.sha256 }] });
    const artifactRelocation = { sourceAttempt, sourceManifest, files: [{ sourcePath: "/container/answer.json", artifact: answer }, { sourcePath: "/container/trace.json", artifact: trace }] };
    const projected: any = { ...source, answerPath: answer.path, native: { ...source.native, tracePath: trace.path }, artifactRelocation };
    await verifyRelocatedAttempt(projected);
    projected.native.requestedModel = "other-model";
    await expect(verifyRelocatedAttempt(projected)).rejects.toThrow("Relocation changed");
  } finally { await f.cleanup(); }
});

test("batch advancement reads the actual review and binds exact report and runtime evidence", async () => {
  const f = await fixture();
  try {
    const manifest = await f.write("controller.json", { source: "frozen" }), run = await f.write("run.json", { source: "real outputs required at runtime" });
    const report = await f.write("report.json", { schema: "gptgrep.controller-batch-report.v1", manifest, status: "pending_independent_review", runs: { bare: run, gptgrep: run }, captures: [] });
    const unrelated = await f.write("unrelated.json", { status: "accepted" });
    await expect(verifyBatchAcceptance(manifest, report, unrelated)).rejects.toThrow("exact report");
    const runtimeReview = await f.write("runtime-review.json", { schema: "gptgrep.controller-runtime-review.v1", status: "qualification_in_progress", manifest, reports: [report], observedCapabilities: [], unavailableCapabilities: ["real tools"] });
    const review = await f.write("review.json", { schema: "gptgrep.controller-batch-review.v1", status: "accepted_for_next_batch", manifest, report, runtimeReview });
    expect((await verifyBatchAcceptance(manifest, report, review)).runtime.status).toBe("qualification_in_progress");
    await f.write("runtime-review.json", { status: "qualified" });
    await expect(verifyBatchAcceptance(manifest, report, review)).rejects.toThrow();
  } finally { await f.cleanup(); }
});

test("event-time image snapshots survive source deletion and bind reused paths to distinct calls", async () => {
  const f = await fixture();
  try {
    const source = join(f.root, "view.png"), png = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jPioAAAAASUVORK5CYII=", "base64");
    await writeFile(source, png);
    const capture = await createNativeMediaCapture({ roots: [f.root], outputDir: join(f.root, "media"), maxBytes: 1024 });
    const notification = (id: string) => ({ method: "item/completed", params: { threadId: "thread", item: { id, type: "imageView", path: source } } });
    capture.capture(notification("first")); await rm(source); capture.capture(notification("first"));
    expect(capture.snapshot().gaps).toEqual([]); expect(capture.snapshot().bindings).toHaveLength(1);
    expect(await fileHash(capture.snapshot().bindings[0]!.artifact.path)).toBe(sha256(png));
    await writeFile(source, Buffer.concat([png, Buffer.from("changed source bytes")])); capture.capture(notification("second"));
    expect(capture.snapshot().bindings).toHaveLength(2);
    expect(capture.snapshot().bindings[0]!.artifact.sha256).not.toBe(capture.snapshot().bindings[1]!.artifact.sha256);
    capture.capture(notification("historical"), "thread_read_snapshot");
    expect(capture.snapshot().bindings).toHaveLength(2);
    expect(capture.snapshot().gaps).toContain("historical_media_unverified:thread:historical");
    capture.capture({ method: "turn/completed", params: { threadId: "thread", turn: { items: [
      { id: "first", type: "imageView", path: source }, { id: "terminal-only", type: "imageView", path: source },
    ] } } });
    expect(capture.snapshot().bindings).toHaveLength(2);
    expect(capture.snapshot().bindings[0]!.artifact.sha256).toBe(sha256(png));
    expect(capture.snapshot().gaps).toContain("historical_media_unverified:thread:terminal-only");
    expect(capture.snapshot().gaps).not.toContain("historical_media_unverified:thread:first");
  } finally { await f.cleanup(); }
});
