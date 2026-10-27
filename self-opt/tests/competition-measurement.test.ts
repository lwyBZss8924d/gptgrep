/** Offline enrollment tests; these fixtures never qualify a live C3/O4 measurement. */
import { expect, test } from "bun:test";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { measurementSlotKey, runCompetitionMeasurements, validateMeasurementEnrollment, validateMeasurementEvaluation, verifyMeasurementReport, type MeasurementPlan } from "../src/competition-measurement";
import type { CompetitionContract } from "../src/competition";
import { scenarioFingerprint, type EvaluationRun } from "../src/evaluate";
import { EpisodeSlotError, runEpisodeSlot, verifyNativeEpisodeCapture, type CampaignEnvelope, type EpisodeSlot } from "../../benchmark/src/controller";
import { captureNotification, writeNativeTrajectory } from "../../benchmark/src/trajectory";
import type { NativeRunResult } from "../../benchmark/src/native";
import { CAMPAIGN_PURPOSES, verifyCampaignAction } from "../../benchmark/src/campaign";
import { digest, fileHash } from "../../benchmark/src/io";

const H = "a".repeat(64), pin = (path: string) => ({ path, sha256: H });
const competition = { cohort: ["task-1", "task-2", "task-3", "task-4"], candidateIds: ["candidate-a", "candidate-b"] } as CompetitionContract;
function fixture(): MeasurementPlan {
  const root = "/Volumes/fixture/measurement";
  const command = { executable: pin("/Volumes/fixture/bun"), inputs: [], argv: [], cwd: root, timeoutMs: 1000, maxOutputBytes: 1024 };
  const slots = competition.cohort.flatMap(taskId => ["incumbent", ...competition.candidateIds].map((candidateId, order) => {
    const path = `${root}/${candidateId}/${taskId}`, owner = (role: string) => ({ storage: `${path}/${role}/storage.json`, auth: `${path}/${role}/receipt.json`, storagePlanDigest: H, authRunId: `${candidateId}-${taskId}-${role}` });
    const slot: EpisodeSlot = { taskId, arm: "gptgrep", order, harborJob: pin(`${path}/harbor-job.json`), wrapperJob: pin(`${path}/wrapper-job.json`),
      bridgeContract: pin(`${path}/bridge.json`), readerJob: pin(`${path}/reader-job.json`), judgeJob: pin(`${path}/judge-job.json`), harborExport: pin(`${path}/export.json`),
      reader: command, judge: command, verifier: command, attemptPath: `${path}/attempt.json`, expected: pin(`${path}/expected.json`),
      bindingPath: `${path}/binding.json`, judgmentPath: `${path}/judgment.json`, feedbackPath: `${path}/feedback.json`,
      readerTerminal: owner("reader"), judgeTerminal: owner("judge"), verifierTerminal: { storage: `${path}/verifier/storage.json`, storagePlanDigest: H } };
    return { candidateId, slot };
  }));
  return { schema: "gptgrep.opt-measurement-plan.v1", competition: pin("/Volumes/fixture/competition.json"), policy: pin("/Volumes/fixture/policy.json"), freezeReview: pin("/Volumes/fixture/actor-review.json"),
    evaluations: Object.fromEntries(["incumbent", ...competition.candidateIds].map(candidate => [candidate, pin(`/Volumes/fixture/${candidate}.json`)])),
    slots, runtimeReadiness: pin("/Volumes/fixture/readiness.json"), outputRoot: root,
    sources: ["../src/competition-measurement.ts", "../../benchmark/src/controller.ts", "../../benchmark/src/episode.ts", "../../benchmark/src/campaign.ts", "../../benchmark/src/process.ts", "../src/evaluate.ts"].map(path => pin(fileURLToPath(new URL(path, import.meta.url)))) };
}

test("measurement enrollment is exactly twelve distinct gptgrep product/task slots", () => {
  expect(() => validateMeasurementEnrollment(fixture(), competition)).not.toThrow();
  for (const mutate of [
    (plan: MeasurementPlan) => { plan.slots.pop(); },
    (plan: MeasurementPlan) => { plan.slots[1] = structuredClone(plan.slots[0]!); },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.arm = "bare"; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.taskId = "replacement-task"; },
    (plan: MeasurementPlan) => { plan.slots[0]!.candidateId = "replacement-candidate"; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.order = 1; },
    (plan: MeasurementPlan) => { plan.evaluations["extra"] = pin("/Volumes/fixture/extra.json"); },
  ]) { const plan = fixture(); mutate(plan); expect(() => validateMeasurementEnrollment(plan, competition)).toThrow(); }
});

test("each frozen role/task gets a distinct shared-campaign action identity", () => {
  const keys = fixture().slots.map(({ candidateId, slot }) => measurementSlotKey(candidateId, slot.taskId));
  expect(new Set(keys).size).toBe(12);
  expect(measurementSlotKey("incumbent", "task-1")).toBe(measurementSlotKey("incumbent", "task-1"));
  expect(measurementSlotKey("incumbent", "task-1")).not.toBe(measurementSlotKey("candidate-a", "task-1"));
});

test("measurement freeze rejects source substitution and reused result/auth identities", () => {
  for (const mutate of [
    (plan: MeasurementPlan) => { plan.sources[0]!.path = "/Volumes/fixture/copied-measurement.ts"; },
    (plan: MeasurementPlan) => { plan.sources.pop(); },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.attemptPath = plan.slots[1]!.slot.attemptPath; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.bindingPath = `${plan.slots[0]!.slot.attemptPath}/binding.json`; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.bindingPath = "/Volumes/fixture/old-binding.json"; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.judgeTerminal.authRunId = plan.slots[1]!.slot.readerTerminal.authRunId; },
    (plan: MeasurementPlan) => { plan.slots[0]!.slot.verifierTerminal.storage = plan.slots[1]!.slot.readerTerminal.storage; },
  ]) { const plan = fixture(); mutate(plan); expect(() => validateMeasurementEnrollment(plan, competition)).toThrow(); }
});

function evaluationFixture() {
  const run: EvaluationRun = { schema: "gptgrep.opt-evaluation.v1", runId: "fresh-incumbent", arm: "gptgrep", revision: "1".repeat(40), parentRevision: null,
    cohort: competition.cohort, split: "dev", outcomes: [], scenario: { datasetRevision: "fixture", corpusSha256: H, rubricSha256: H, nativeScorerSha256: H,
      readerModel: "frozen-reader", workerModel: "frozen-worker", ocrModel: "frozen-ocr", helperModel: "frozen-helper", reasoningEffort: "high", serviceTier: "priority",
      judgeModel: "frozen-judge", judgeReasoningEffort: "high", judgeServiceTier: "priority", environmentSha256: H, preparationPolicySha256: H, seed: "frozen" } };
  const productPin = pin("/Volumes/fixture/incumbent-product.json"), runtime = { sourceRevision: run.revision, buildReceipt: productPin, binarySha256: H, runtimeManifestSha256: H, codexBinarySha256: H };
  const evaluation = { schema: "gptgrep.evaluation-plan.v1", run, runtime, tasks: Object.fromEntries(run.cohort.map(taskId => [taskId, { commonPromptSha256: H, preparationSha256: H, contextSha256: H, corpusSha256: H }])) };
  const expected = { competition: { ...competition, parentRevision: run.revision, scenarioFingerprint: scenarioFingerprint(run) }, candidateId: "incumbent", candidateRevision: run.revision,
    productPin, product: { schema: "gptgrep.product-build.v1", sourceRevision: run.revision, platform: "linux", architecture: "x64", binarySha256: H, runtimeManifestSha256: H },
    parentEvaluation: structuredClone(evaluation), baselineRun: { ...structuredClone(run), runId: "accepted-c3-parent" } };
  return { evaluation, expected };
}

test("incumbent uses the C3 product but must have a fresh unmeasured evaluation", () => {
  const f = evaluationFixture(); expect(validateMeasurementEvaluation(f.evaluation, f.expected).revision).toBe(f.expected.competition.parentRevision);
  for (const mutate of [
    (f: ReturnType<typeof evaluationFixture>) => { f.evaluation.run.runId = f.expected.baselineRun.runId; },
    (f: ReturnType<typeof evaluationFixture>) => { f.evaluation.run.outcomes = [{ taskId: "task-1" } as any]; },
    (f: ReturnType<typeof evaluationFixture>) => { f.evaluation.runtime.binarySha256 = "b".repeat(64); f.expected.product.binarySha256 = "b".repeat(64); },
    (f: ReturnType<typeof evaluationFixture>) => { f.evaluation.run.scenario.judgeModel = "different-judge"; },
    (f: ReturnType<typeof evaluationFixture>) => { f.evaluation.tasks["task-1"]!.preparationSha256 = "b".repeat(64); },
  ]) { const changed = evaluationFixture(); mutate(changed); expect(() => validateMeasurementEvaluation(changed.evaluation, changed.expected)).toThrow(); }
});

test("candidate enrollment binds the exact qualified Linux build without changing matched context", () => {
  const f = evaluationFixture(), revision = "2".repeat(40), product = pin("/Volumes/fixture/candidate-product.json");
  f.expected.candidateId = "candidate-a"; f.expected.candidateRevision = revision; f.expected.productPin = product;
  f.expected.product.sourceRevision = revision;
  f.evaluation.run.revision = revision; f.evaluation.run.parentRevision = f.expected.competition.parentRevision;
  f.evaluation.runtime.sourceRevision = revision; f.evaluation.runtime.buildReceipt = product;
  expect(validateMeasurementEvaluation(f.evaluation, f.expected).revision).toBe(revision);
  f.expected.product.platform = "darwin"; expect(() => validateMeasurementEvaluation(f.evaluation, f.expected)).toThrow("Linux");
  f.expected.product.platform = "linux"; f.evaluation.runtime.buildReceipt = pin("/Volumes/fixture/other-product.json");
  expect(() => validateMeasurementEvaluation(f.evaluation, f.expected)).toThrow("exact Linux product");
});

test("malformed executable requests cannot reach C3 gates, reservations or dispatch", async () => {
  await expect(runCompetitionMeasurements({ plan: pin("/Volumes/fixture/missing-plan.json") } as any)).rejects.toThrow();
});

test("shared episode kernel charges C4 before a harmless process and never retries missing owner evidence", async () => {
  const external = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!external) throw new Error("Explicit external SSD test output required");
  const root = join(external, `measurement-kernel-${crypto.randomUUID()}`); await mkdir(root);
  const write = async (name: string, value: unknown) => {
    const path = join(root, name); await writeFile(path, `${JSON.stringify(value)}\n`); return { path, sha256: await fileHash(path) };
  };
  const old = { binding: process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING, digest: process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST };
  try {
    const binding = await write("test-storage-binding.json", { targets: { output: root } });
    process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = binding.path; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = binding.sha256;
    const envelope: CampaignEnvelope = { schema: "gptgrep.live-campaign-envelope.v2", campaignId: "measurement-offline", ledgerRoot: join(root, "ledger"),
      campaignDeadline: new Date(Date.now() + 60000).toISOString(), maxReservedBytes: 10000, baseReservedBytes: 0,
      allowances: Object.fromEntries(CAMPAIGN_PURPOSES.map(purpose => [purpose, { maxAdmissions: 12, reservedBytesPerAdmission: 1, deadlineMs: 5000 }])) as CampaignEnvelope["allowances"],
      episodeDeadlineMs: 5000, maxTraceBytes: 1024, maxMediaBytes: 1024, maxTrajectoryBytes: 1024, nativeDeadlineMs: 1000,
      observationLimits: { toolCalls: 1, childThreads: 0, retryEvents: 0 }, providerRequestLimit: null, dollarLimitUsd: null };
    const envelopePin = await write("envelope.json", envelope), manifestPin = await write("offline-plan.json", { fixtureOnly: true });
    const script = join(root, "harmless.js"), marker = join(root, "launch-count.txt");
    await writeFile(script, 'require("node:fs").appendFileSync(process.argv[2], "launched\\n");\n');
    const slot = structuredClone(fixture().slots[0]!.slot);
    slot.reader = { executable: { path: process.execPath, sha256: await fileHash(process.execPath) }, inputs: [{ path: script, sha256: await fileHash(script) }],
      argv: [script, marker], cwd: root, timeoutMs: 1000, maxOutputBytes: 1024 };
    slot.readerTerminal.storage = join(root, "intentionally-unavailable-owner-terminal.json");
    const options = { slot, evaluation: manifestPin, envelopePin, experimentId: "offline-competition", manifestPin, outputRoot: root,
      key: "fresh-slot", readerPurpose: "c4_reader" as const, judgePurpose: "c4_judge" as const, freshAfter: new Date().toISOString() };
    let failure: unknown;
    try { await runEpisodeSlot(options); } catch (error) { failure = error; }
    expect(failure).toBeInstanceOf(EpisodeSlotError);
    expect((failure as EpisodeSlotError).evidence.binding).toBeNull();
    const charged = await verifyCampaignAction(envelopePin, { experimentId: options.experimentId, actionId: "fresh-slot.reader", purpose: "c4_reader", commandSha256: digest(slot.reader) });
    expect(charged.row.disposition).toBe("charged_before_dispatch");
    expect(await readFile(marker, "utf8")).toBe("launched\n");
    await expect(runEpisodeSlot(options)).rejects.toThrow();
    expect(await readFile(marker, "utf8")).toBe("launched\n");
    await expect(verifyCampaignAction(envelopePin, { experimentId: options.experimentId, actionId: "fresh-slot.judge", purpose: "c4_judge", commandSha256: digest(slot.judge) })).rejects.toThrow();
    await expect(runEpisodeSlot({ ...options, key: "expired-slot", deadlineAt: Date.now() - 1 })).rejects.toThrow("deadline");
    await expect(verifyCampaignAction(envelopePin, { experimentId: options.experimentId, actionId: "expired-slot.reader", purpose: "c4_reader", commandSha256: digest(slot.reader) })).rejects.toThrow();
    expect(await readFile(marker, "utf8")).toBe("launched\n");
  } finally {
    if (old.binding === undefined) delete process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING; else process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = old.binding;
    if (old.digest === undefined) delete process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST; else process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = old.digest;
  }
});

test("RAW/ATIF revalidation rejects altered usage, provider counts, stale captures and rewritten sidecars", async () => {
  const external = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!external) throw new Error("Explicit external test output required");
  const root = join(external, `capture-validation-${crypto.randomUUID()}`); await mkdir(root);
  const now = Date.now(), at = new Date(now).toISOString(), tracePath = join(root, "trace.jsonl");
  const native = { schema: "gptgrep.native-run.v1", executionMode: "live", status: "completed", validStart: true,
    threadId: "synthetic-root", turnId: "synthetic-turn", sessionId: "synthetic-session", requestedModel: "fixture-reader", observedModel: "fixture-reader",
    servedModel: null, requestedEffort: "high", requestedServiceTier: "priority", runtimeBinarySha256: H,
    usage: { totalTokens: 12, inputTokens: 10, cachedInputTokens: 0, cacheWriteInputTokens: null, outputTokens: 2, reasoningOutputTokens: 0 },
    usageScope: "root_thread_only", requestCount: null, costUsd: null, cleanup: { completed: true, groupTerminated: true },
    workerModels: [], mediaBindings: [], tracePath, traceTruncated: false } as unknown as NativeRunResult;
  const rows = [{ direction: "input", at, text: "Synthetic converter fixture only" },
    { direction: "capture", at, capture: captureNotification({ method: "turn/completed", params: { threadId: native.threadId,
      turn: { id: native.turnId, status: "completed", items: [{ id: "answer", type: "agentMessage", text: "fixture", status: "completed" }] } } }) },
    { direction: "result", at, status: native.status, threadId: native.threadId, turnId: native.turnId, sessionId: native.sessionId,
      requestedModel: native.requestedModel, observedModel: native.observedModel, servedModel: native.servedModel, usage: native.usage, cleanup: native.cleanup }];
  await writeFile(tracePath, rows.map(row => JSON.stringify(row)).join("\n") + "\n");
  const trace = { path: tracePath, sha256: await fileHash(tracePath) }, coveragePath = join(root, "coverage.json"), trajectoryPath = join(root, "trajectory.json");
  await writeNativeTrajectory({ trace, result: native, media: [], outputPath: trajectoryPath, coveragePath });
  const options = { native, sourceTrace: trace, trace, coverage: { path: coveragePath, sha256: await fileHash(coveragePath) },
    trajectory: { path: trajectoryPath, sha256: await fileHash(trajectoryPath) },
    process: { startedAt: new Date(now - 1000).toISOString(), completedAt: new Date(now + 1000).toISOString() }, requireComplete: true };
  const result = await verifyNativeEpisodeCapture(options);
  expect(result.coverage.complete).toBe(true); expect(result.coverage.liveQualified).toBe(false);
  await expect(verifyNativeEpisodeCapture({ ...options, native: { ...native, usage: { ...native.usage!, inputTokens: 999 } } })).rejects.toThrow("Raw terminal");
  await expect(verifyNativeEpisodeCapture({ ...options, native: { ...native, requestCount: 0 } })).rejects.toThrow("Unavailable native provider");
  await expect(verifyNativeEpisodeCapture({ ...options, native: { ...native, costUsd: 0 } })).rejects.toThrow("Unavailable native provider");
  await expect(verifyNativeEpisodeCapture({ ...options, sourceTrace: { ...trace, sha256: H } })).rejects.toThrow("capture identity");
  await expect(verifyNativeEpisodeCapture({ ...options, process: { startedAt: new Date(now + 1).toISOString(), completedAt: new Date(now + 1000).toISOString() } })).rejects.toThrow("timestamps");
  await expect(verifyNativeEpisodeCapture({ ...options, native: { ...native, executionMode: "test" } })).rejects.toThrow("live native capture");
  const coverage = JSON.parse(await readFile(coveragePath, "utf8")); coverage.providerRequestCount = 0;
  await writeFile(coveragePath, JSON.stringify(coverage));
  await expect(verifyNativeEpisodeCapture({ ...options, coverage: { path: coveragePath, sha256: await fileHash(coveragePath) } })).rejects.toThrow("recomputation");
});

test("an incomplete or acceptance-decorated report cannot enter measurement gates", async () => {
  const external = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!external) throw new Error("Explicit external test output required");
  const root = join(external, `report-refusal-${crypto.randomUUID()}`); await mkdir(root);
  const plan = fixture(), missing = pin(join(root, "plan-must-not-be-opened.json"));
  const base = { schema: "gptgrep.opt-measurement-report.v1", status: "incomplete", plan: missing, review: missing, qualifiedCandidatePins: {},
    competition: missing, policy: missing, campaign: missing, freezeReview: missing, runtimeReadiness: plan.runtimeReadiness, lock: missing,
    startedAt: "2026-01-01T00:00:00Z", completedAt: "2026-01-01T00:00:01Z", enrolledTasksPerRun: 4, enrolledReaderSlots: 12,
    enrolledJudgeSlots: 12, enrolledVerifierSlots: 12, runs: {}, slotDispositions: [], error: "missing actual owner terminal", storageBefore: null,
    storageAfter: null, releaseAccepted: false, c3ParentChanged: false };
  for (const value of [base, { ...base, status: "pending_parent_finalization", error: null, enrolledReaderSlots: 11 },
    { ...base, status: "pending_parent_finalization", error: null, releaseAccepted: true }, { ...base, accepted: true }]) {
    const path = join(root, `${crypto.randomUUID()}.json`); await writeFile(path, JSON.stringify(value));
    await expect(verifyMeasurementReport({ path, sha256: await fileHash(path) }, competition)).rejects.toThrow();
  }
  expect(await readFile(join(root, "plan-must-not-be-opened.json")).catch(() => null)).toBeNull();
});
