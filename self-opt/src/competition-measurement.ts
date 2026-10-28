/** Fresh C4 measurements reuse the actual C2 episode kernel; no paired-manifest emulation. */
import { lstat, open } from "node:fs/promises";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { EpisodeSlotError, runEpisodeSlot, validateEpisodeSlot, validateEnvelope, verifyRuntimeReadiness,
  verifyEpisodeDispatch, verifyNativeEpisodeCapture, verifyReaderEpisodeArtifacts,
  type CampaignEnvelope, type EpisodeExecutionOptions, type EpisodeSlot } from "../../benchmark/src/controller";
import { loadReaderEpisode, outcomeFromEvidence, writeEvaluationRun, type EpisodeEvidence } from "../../benchmark/src/episode";
import { campaignActionPath } from "../../benchmark/src/campaign";
import { canonical, digest, fileHash, physicalPath, requireStorage, within } from "../../benchmark/src/io";
import { immutable, observeActorStorage, pinned, validateActorStorageObservation, verifyActorGate } from "./actor-contracts";
import { verifyCandidateBuild } from "./candidate-build";
import { verifyPin, type CompetitionContract, type Pin } from "./competition";
import { scenarioFingerprint, summarize, validateRun, type EvaluationRun } from "./evaluate";
import { boundedText, closed, identity } from "./proposals";

export interface CandidateEpisodeSlot { candidateId: string; slot: EpisodeSlot }
export interface MeasurementPlan {
  schema: "gptgrep.opt-measurement-plan.v1";
  competition: Pin; policy: Pin; freezeReview: Pin;
  evaluations: Record<string, Pin>;
  slots: CandidateEpisodeSlot[];
  runtimeReadiness: Pin;
  /** Actual imported measurement code, with the broader C2 closure in runtimeReadiness. */
  sources: Pin[];
  outputRoot: string;
}
export interface MeasurementOptions {
  plan: Pin; review: Pin; qualifiedCandidatePins: Record<string, Pin>;
}
export interface MeasurementResult {
  status: "pending_parent_finalization" | "incomplete";
  report: Pin; runs: Record<string, Pin>;
}
const SOURCE_PATHS = ["./competition-measurement.ts", "../../benchmark/src/controller.ts", "../../benchmark/src/episode.ts",
  "../../benchmark/src/campaign.ts", "../../benchmark/src/process.ts", "./evaluate.ts"].map(path => fileURLToPath(new URL(path, import.meta.url)));
const same = (a: unknown, b: unknown) => canonical(a) === canonical(b);
const roles = (competition: CompetitionContract) => ["incumbent", ...competition.candidateIds];
export const measurementSlotKey = (candidateId: string, taskId: string) => digest({ candidateId, taskId }).slice(0, 24);
async function exists(path: string): Promise<boolean> {
  try { await lstat(path); return true; } catch (error: any) { if (error.code === "ENOENT") return false; throw error; }
}
async function syncArtifact(path: string): Promise<void> {
  const file = await open(path, "r"); try { await file.sync(); } finally { await file.close(); }
  const parent = await open(dirname(path), "r"); try { await parent.sync(); } finally { await parent.close(); }
}
function terminalPaths(slot: EpisodeSlot): string[] {
  return [slot.readerTerminal.storage, slot.readerTerminal.auth, slot.judgeTerminal.storage, slot.judgeTerminal.auth, slot.verifierTerminal.storage];
}
function resultPaths(slot: EpisodeSlot): string[] {
  return [slot.attemptPath, slot.bindingPath, slot.judgmentPath, slot.feedbackPath];
}
async function observedPin(path: string): Promise<Pin | null> {
  if (!await exists(path)) return null;
  if (path.split("/").at(-1) === "auth.json") throw new Error("Credential files cannot be measurement artifacts");
  await physicalPath(path); return { path, sha256: await fileHash(path) };
}
function episodeOptions(plan: MeasurementPlan, competition: CompetitionContract, candidateId: string, slot: EpisodeSlot,
  manifestPin: Pin, envelopePin: Pin, startedAt: string): EpisodeExecutionOptions {
  return { slot, evaluation: plan.evaluations[candidateId]!, envelopePin, experimentId: competition.competitionId,
    manifestPin, outputRoot: plan.outputRoot, key: measurementSlotKey(candidateId, slot.taskId), readerPurpose: "c4_reader",
    judgePurpose: "c4_judge", freshAfter: startedAt, deadlineAt: competition.deadlineAt };
}

/** Pure enrollment checks are also used by the executable preflight, before any effects. */
export function validateMeasurementEnrollment(plan: MeasurementPlan, competition: CompetitionContract): void {
  closed(plan, ["schema", "competition", "policy", "freezeReview", "evaluations", "slots", "runtimeReadiness", "sources", "outputRoot"], "C4 measurement plan");
  const candidates = roles(competition);
  if (plan.schema !== "gptgrep.opt-measurement-plan.v1" || competition.cohort.length !== 4 || candidates.length !== 3 ||
      candidates.some(candidate => ["__proto__", "constructor", "prototype"].includes(candidate)) || new Set(candidates).size !== 3 ||
      !isAbsolute(plan.outputRoot) || resolve(plan.outputRoot) !== plan.outputRoot || plan.outputRoot === "/" ||
      !same(Object.keys(plan.evaluations).sort(), [...candidates].sort()) || !Array.isArray(plan.slots) || plan.slots.length !== 12) throw new Error("C4 requires exactly twelve fresh incumbent/A/B slots on four tasks");
  candidates.forEach(candidate => identity(candidate, "measurement candidate ID"));
  if (!Array.isArray(plan.sources) || !same(plan.sources.map(pin => pin.path).sort(), [...SOURCE_PATHS].sort())) throw new Error("Measurement source freeze must select the actual imported kernel and comparator");
  const keys = new Set<string>(), orders = new Set<string>(), outputs = new Set<string>(), authRuns = new Set<string>();
  for (const row of plan.slots) {
    closed(row, ["candidateId", "slot"], "candidate episode slot");
    const slot = row.slot, key = `${row.candidateId}:${slot.taskId}`, order = `${slot.taskId}:${slot.order}`;
    if (!candidates.includes(row.candidateId) || !competition.cohort.includes(slot.taskId) || slot.arm !== "gptgrep" ||
        !Number.isSafeInteger(slot.order) || slot.order < 0 || slot.order > 2 || keys.has(key) || orders.has(order)) throw new Error("Duplicate, substituted or misordered C4 measurement slot");
    keys.add(key); orders.add(order);
    for (const path of resultPaths(slot)) if (!within(plan.outputRoot, path)) throw new Error("C4 result must remain in its fresh measurement root");
    for (const path of [...resultPaths(slot), ...terminalPaths(slot)]) {
      if (!isAbsolute(path) || resolve(path) !== path || [...outputs].some(other => within(other, path) || within(path, other))) throw new Error("Measurement artifacts and owner terminals must be distinct non-overlapping physical files");
      outputs.add(path);
    }
    for (const run of [slot.readerTerminal.authRunId, slot.judgeTerminal.authRunId]) {
      if (!run || authRuns.has(run)) throw new Error("Each C4 reader and judge requires a separate native auth run");
      authRuns.add(run);
    }
  }
}

/** The same product/scenario check is used for incumbent and both candidate evaluations. */
export function validateMeasurementEvaluation(evaluation: any, expected: {
  competition: CompetitionContract; candidateId: string; candidateRevision: string; productPin: Pin; product: any;
  parentEvaluation: any; baselineRun: EvaluationRun;
}): EvaluationRun {
  const { competition, candidateId, candidateRevision, productPin, product, parentEvaluation, baselineRun } = expected;
  const run = evaluation.run as EvaluationRun; validateRun({ ...run, outcomes: [] });
  if (evaluation.schema !== "gptgrep.evaluation-plan.v1" || run.arm !== "gptgrep" || run.split !== "dev" ||
      run.revision !== candidateRevision || run.parentRevision !== (candidateId === "incumbent" ? baselineRun.parentRevision : competition.parentRevision) ||
      !same(run.cohort, competition.cohort) || scenarioFingerprint(run) !== competition.scenarioFingerprint ||
      run.runId === baselineRun.runId || (run.outcomes !== undefined && run.outcomes.length !== 0)) throw new Error("C4 evaluation must be a fresh exact four-task product/scenario enrollment");
  const runtime = evaluation.runtime;
  if (!runtime || !same(runtime.buildReceipt, productPin) || runtime.sourceRevision !== run.revision ||
      product.schema !== "gptgrep.product-build.v1" || product.sourceRevision !== run.revision ||
      candidateId !== "incumbent" && (product.platform !== "linux" || !["x64", "arm64"].includes(product.architecture)) || product.binarySha256 !== runtime.binarySha256 ||
      product.runtimeManifestSha256 !== runtime.runtimeManifestSha256 || runtime.codexBinarySha256 !== parentEvaluation.runtime.codexBinarySha256 ||
      candidateId === "incumbent" && !same(runtime, parentEvaluation.runtime)) throw new Error("Measurement runtime must bind the exact Linux product build and frozen native binary");
  if (!same(Object.keys(evaluation.tasks).sort(), [...competition.cohort].sort())) throw new Error("Measurement task metadata differs from the complete four-task cohort");
  for (const taskId of competition.cohort) if (!same(evaluation.tasks[taskId], parentEvaluation.tasks[taskId])) throw new Error("Candidate changed the frozen task context, prompt or preparation");
  return run;
}

/** Gate checking is read-only; an accepted C3 plus exact actor/measurement reviews remain mandatory. */
export async function validateMeasurementPlan(plan: MeasurementPlan, qualifiedCandidatePins: Record<string, Pin>, purpose: "launch" | "retained_evidence" = "launch") {
  if (!["launch", "retained_evidence"].includes(purpose)) throw new Error("Unknown measurement validation purpose");
  closed(plan, ["schema", "competition", "policy", "freezeReview", "evaluations", "slots", "runtimeReadiness", "sources", "outputRoot"], "C4 measurement plan");
  const { competition, policy, campaign } = await verifyActorGate(plan.competition, plan.policy, plan.freezeReview, purpose);
  if (competition.schema !== "gptgrep.opt-competition.v3" || !same(competition.nativePolicy, plan.policy)) throw new Error("C4 measurements require the explicit direct-local actor policy contract");
  validateMeasurementEnrollment(plan, competition);
  if (!same(Object.keys(qualifiedCandidatePins).sort(), [...competition.candidateIds].sort())) throw new Error("Both exact qualified candidate pins are required");
  for (const pin of plan.sources) await verifyPin(pin);
  if (!plan.sources.some(pin => same(pin, competition.comparator))) throw new Error("Measurement comparator differs from the frozen competition");
  if (!within(policy.outputRoot, plan.outputRoot)) throw new Error("Measurement output is outside the frozen actor storage root");
  await physicalPath(plan.outputRoot);
  const envelope = campaign as CampaignEnvelope; validateEnvelope(envelope);
  for (const purpose of ["c4_reader", "c4_judge", "verifier"] as const) {
    if (envelope.allowances[purpose].maxAdmissions < 12) throw new Error("Shared campaign lacks the twelve frozen measurement admissions");
  }
  const baseline = await pinned(policy.baseline.manifest), baselineReport = await pinned(policy.baseline.report);
  if (!same(plan.runtimeReadiness, baseline.runtimeReadiness)) throw new Error("C4 must use the actual independently accepted C3 runtime readiness");
  const ready = await verifyRuntimeReadiness(plan.runtimeReadiness);
  const parentEvaluation = await pinned(baseline.evaluations.gptgrep), baselineRun = await pinned<EvaluationRun>(baselineReport.runs.gptgrep);
  const expectedBuilds: Record<string, Pin> = { incumbent: parentEvaluation.runtime.buildReceipt };
  const revisions: Record<string, string> = { incumbent: competition.parentRevision };
  const { verifyQualifiedCandidate } = await import("./actor-qualification");
  for (const candidateId of competition.candidateIds) {
    const qualified = await verifyQualifiedCandidate(policy, competition, candidateId, qualifiedCandidatePins[candidateId]!);
    const build = await verifyCandidateBuild(policy, qualified.build);
    if (build.candidateId !== candidateId || build.candidateRevision !== qualified.candidateRevision || !build.product) throw new Error("Qualified candidate and measured product differ");
    revisions[candidateId] = build.candidateRevision; expectedBuilds[candidateId] = build.product;
  }
  if (new Set(Object.values(revisions)).size !== 3) throw new Error("Incumbent and both candidates must identify distinct actual revisions");
  const evaluations: Record<string, any> = {}, runIds = new Set<string>();
  for (const candidateId of roles(competition)) {
    const evaluation = await pinned(plan.evaluations[candidateId]!);
    const run = validateMeasurementEvaluation(evaluation, { competition, candidateId, candidateRevision: revisions[candidateId]!, productPin: expectedBuilds[candidateId]!,
      product: await pinned(expectedBuilds[candidateId]!), parentEvaluation, baselineRun });
    if (runIds.has(run.runId)) throw new Error("Each C4 product requires a distinct fresh evaluation run identity");
    runIds.add(run.runId);
    evaluations[candidateId] = evaluation;
  }
  const baselineOutputs = new Set<string>(baseline.slots.flatMap((slot: EpisodeSlot) => [...resultPaths(slot), ...terminalPaths(slot)]));
  const baselineAuthRuns = new Set<string>(baseline.slots.flatMap((slot: EpisodeSlot) => [slot.readerTerminal.authRunId, slot.judgeTerminal.authRunId]));
  for (const { candidateId, slot } of plan.slots) {
    await validateEpisodeSlot({ slot, evaluation: plan.evaluations[candidateId]!, envelope, outputRoot: plan.outputRoot, ready,
      readerPurposes: ["c4_reader", "c4_judge"] });
    const job = await pinned(slot.readerJob), runtime = job.options.runtime, expected = evaluations[candidateId].runtime;
    if (runtime.sourceRevision !== expected.sourceRevision || runtime.buildReceipt.sha256 !== expected.buildReceipt.sha256 ||
        runtime.binarySha256 !== expected.binarySha256 || runtime.manifestSha256 !== expected.runtimeManifestSha256 || job.options.order !== slot.order) throw new Error("Reader job does not execute its exact enrolled candidate product and order");
    for (const path of [...resultPaths(slot), ...terminalPaths(slot)]) {
      if (!within(policy.storage.root, path) || baselineOutputs.has(path)) throw new Error("C4 cannot reuse C3 result or owner terminal paths");
      await physicalPath(path);
    }
    if (baselineAuthRuns.has(slot.readerTerminal.authRunId) || baselineAuthRuns.has(slot.judgeTerminal.authRunId)) throw new Error("C4 cannot reuse a C3 native auth run");
  }
  return { competition, policy, envelope, evaluations };
}

async function verifyMeasurementReview(options: MeasurementOptions, plan: MeasurementPlan, campaign: Pin) {
  const review = await pinned(options.review);
  closed(review, ["schema", "status", "plan", "competition", "policy", "actorFreezeReview", "campaign", "qualifiedCandidatePins", "reviewerSessionId"], "exact measurement review");
  if (review.schema !== "gptgrep.opt-measurement-review.v1" || review.status !== "accepted_for_live_qualification" ||
      !same(review.plan, options.plan) || !same(review.competition, plan.competition) || !same(review.policy, plan.policy) ||
      !same(review.actorFreezeReview, plan.freezeReview) || !same(review.campaign, campaign) || !same(review.qualifiedCandidatePins, options.qualifiedCandidatePins)) throw new Error("Exact O4 measurement review and qualified candidate pins required");
  boundedText(review.reviewerSessionId, "independent measurement reviewer", 256);
}

/** Serial, single-attempt execution. A retained lock is never automatically cleared or retried. */
export async function runCompetitionMeasurements(options: MeasurementOptions): Promise<MeasurementResult> {
  closed(options, ["plan", "review", "qualifiedCandidatePins"], "measurement options");
  const plan = await pinned<MeasurementPlan>(options.plan);
  const { competition, policy, envelope } = await validateMeasurementPlan(plan, options.qualifiedCandidatePins, "launch");
  await verifyMeasurementReview(options, plan, policy.campaign);
  await requireStorage(plan.outputRoot);
  const before = await observeActorStorage(policy.storage);
  const lockPath = join(plan.outputRoot, "measurement.lock");
  if (await exists(lockPath)) throw new Error("C4 measurement is already claimed; retained effects require external reconciliation, never an automatic retry");
  for (const { slot } of plan.slots) {
    const bridge = await pinned(slot.bridgeContract);
    for (const path of [...resultPaths(slot), ...terminalPaths(slot), join(bridge.hostOutputRoot, "collection.json")]) {
      if (await exists(path)) throw new Error("Fresh C4 measurements cannot adopt pre-existing reader, judge or owner evidence");
    }
  }
  const startedAt = new Date().toISOString();
  const lock = await immutable(lockPath, { schema: "gptgrep.opt-measurement-lock.v1", ...options, startedAt, pid: process.pid });
  const evidence: Record<string, EpisodeEvidence[]> = Object.fromEntries(roles(competition).map(candidate => [candidate, []]));
  let error: string | null = null;
  try {
    for (const taskId of competition.cohort) for (const { candidateId, slot } of plan.slots.filter(row => row.slot.taskId === taskId).sort((a, b) => a.slot.order - b.slot.order)) {
      try {
        const episode = await runEpisodeSlot(episodeOptions(plan, competition, candidateId, slot, options.plan, policy.campaign, startedAt));
        evidence[candidateId]!.push(episode);
      } catch (failure) {
        if (failure instanceof EpisodeSlotError && failure.evidence.binding) evidence[candidateId]!.push(failure.evidence);
        throw failure;
      }
    }
  } catch (failure) { error = failure instanceof Error ? failure.message : String(failure); }
  const runs: Record<string, Pin> = {};
  for (const candidateId of roles(competition)) {
    const outputPath = join(plan.outputRoot, `measured-${candidateId}.json`);
    await writeEvaluationRun({ evaluation: plan.evaluations[candidateId]!, episodes: evidence[candidateId]!, outputPath });
    await syncArtifact(outputPath);
    runs[candidateId] = { path: outputPath, sha256: await fileHash(outputPath) };
  }
  const slotDispositions = [];
  for (const { candidateId, slot } of plan.slots) {
    const found = evidence[candidateId]!.find(value => value.taskId === slot.taskId), reservations: Record<string, Pin | null> = {}, dispatches: Record<string, Pin | null> = {};
    const key = measurementSlotKey(candidateId, slot.taskId);
    for (const purpose of ["reader", "judge", "verifier"] as const) {
      const path = campaignActionPath(envelope, { experimentId: competition.competitionId, actionId: `${key}.${purpose}` });
      reservations[purpose] = await observedPin(path);
      dispatches[purpose] = await observedPin(join(plan.outputRoot, "dispatch", `${key}.${purpose}.json`));
    }
    const bridge = await pinned(slot.bridgeContract);
    slotDispositions.push({ candidateId, taskId: slot.taskId, disposition: found ? (await outcomeFromEvidence(found)).status : reservations.reader ? "dispatch_uncertain" : "not_started",
      evidence: found ?? { taskId: slot.taskId, binding: null, judgment: null, feedback: null }, reservations, dispatches,
      terminal: await observedPin(join(plan.outputRoot, "episodes", `${key}.json`)), collection: await observedPin(join(bridge.hostOutputRoot, "collection.json")),
      judgmentCoverage: await observedPin(`${slot.judgmentPath}.coverage.json`) });
  }
  for (const pin of Object.values(runs)) if (!summarize(await pinned<EvaluationRun>(pin)).comparable) error ??= "Fixed-denominator measurement remains incomplete or unknown";
  let after: unknown = null;
  try { after = await observeActorStorage(policy.storage); } catch (failure) { error ??= failure instanceof Error ? failure.message : String(failure); }
  const status = error === null ? "pending_parent_finalization" : "incomplete";
  const report = await immutable(join(plan.outputRoot, "measurement-report.json"), {
    schema: "gptgrep.opt-measurement-report.v1", status, ...options, competition: plan.competition, policy: plan.policy,
    campaign: policy.campaign, freezeReview: plan.freezeReview, runtimeReadiness: plan.runtimeReadiness, lock,
    startedAt, completedAt: new Date().toISOString(), enrolledTasksPerRun: 4, enrolledReaderSlots: 12, enrolledJudgeSlots: 12, enrolledVerifierSlots: 12,
    runs, slotDispositions, error, storageBefore: before, storageAfter: after, releaseAccepted: false, c3ParentChanged: false,
  });
  return { status, report, runs };
}

/** Read-only report validation. It cannot dispatch, rewrite captures or clear the claim lock. */
export async function verifyMeasurementReport(reportPin: Pin, expectedCompetition: CompetitionContract) {
  const report = await pinned(reportPin);
  closed(report, ["schema", "status", "plan", "review", "qualifiedCandidatePins", "competition", "policy", "campaign", "freezeReview", "runtimeReadiness", "lock",
    "startedAt", "completedAt", "enrolledTasksPerRun", "enrolledReaderSlots", "enrolledJudgeSlots", "enrolledVerifierSlots", "runs", "slotDispositions", "error",
    "storageBefore", "storageAfter", "releaseAccepted", "c3ParentChanged"], "fresh measurement report");
  if (report.schema !== "gptgrep.opt-measurement-report.v1" || report.status !== "pending_parent_finalization" || report.error !== null ||
      report.enrolledTasksPerRun !== 4 || report.enrolledReaderSlots !== 12 || report.enrolledJudgeSlots !== 12 || report.enrolledVerifierSlots !== 12 ||
      report.releaseAccepted !== false || report.c3ParentChanged !== false) throw new Error("Only a complete exact fresh measurement report can be attached");
  const plan = await pinned<MeasurementPlan>(report.plan);
  const { competition, policy, envelope, evaluations } = await validateMeasurementPlan(plan, report.qualifiedCandidatePins, "retained_evidence");
  if (!same(competition, expectedCompetition) || !same(report.competition, plan.competition) || !same(report.policy, plan.policy) ||
      !same(report.freezeReview, plan.freezeReview) || !same(report.campaign, policy.campaign) || !same(report.runtimeReadiness, plan.runtimeReadiness) ||
      reportPin.path !== join(plan.outputRoot, "measurement-report.json")) throw new Error("Measurement report belongs to another exact frozen competition");
  await verifyMeasurementReview({ plan: report.plan, review: report.review, qualifiedCandidatePins: report.qualifiedCandidatePins }, plan, policy.campaign);
  const start = Date.parse(report.startedAt), end = Date.parse(report.completedAt);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start || end > Math.min(competition.deadlineAt, Date.parse(envelope.campaignDeadline)) ||
      end > Date.now() || !same(Object.keys(report.runs).sort(), roles(competition).sort()) || !Array.isArray(report.slotDispositions) || report.slotDispositions.length !== 12) throw new Error("Measurement report has stale times or incomplete enrollment");
  const lock = await pinned(report.lock);
  closed(lock, ["schema", "plan", "review", "qualifiedCandidatePins", "startedAt", "pid"], "persistent measurement claim");
  if (report.lock.path !== join(plan.outputRoot, "measurement.lock") || lock.schema !== "gptgrep.opt-measurement-lock.v1" ||
      !same(lock.plan, report.plan) || !same(lock.review, report.review) || !same(lock.qualifiedCandidatePins, report.qualifiedCandidatePins) ||
      lock.startedAt !== report.startedAt || !Number.isSafeInteger(lock.pid) || lock.pid < 1) throw new Error("Measurement report lacks its exact original one-attempt claim");
  for (const observation of [report.storageBefore, report.storageAfter]) {
    validateActorStorageObservation(observation, policy.storage);
    const at = Date.parse(observation.observedAt);
    if (at > end || observation === report.storageBefore && at > start || observation === report.storageAfter && at < start) throw new Error("Measurement storage observation is outside the recorded phase");
  }
  const episodes: Record<string, EpisodeEvidence[]> = Object.fromEntries(roles(competition).map(candidate => [candidate, []]));
  const nativeSessions = new Set<string>(), nativeThreads = new Set<string>();
  const retainIdentity = (native: any, threads: string[]) => {
    if (typeof native.sessionId !== "string" || !native.sessionId || nativeSessions.has(native.sessionId) ||
        !threads.length || new Set(threads).size !== threads.length || threads.some(thread => !thread || nativeThreads.has(thread))) throw new Error("Fresh measurements reused or lack actual native session/thread identities");
    nativeSessions.add(native.sessionId); threads.forEach(thread => nativeThreads.add(thread));
  };
  for (const [index, { candidateId, slot }] of plan.slots.entries()) {
    const row = report.slotDispositions[index];
    closed(row, ["candidateId", "taskId", "disposition", "evidence", "reservations", "dispatches", "terminal", "collection", "judgmentCoverage"], "measured episode disposition");
    closed(row.evidence, ["taskId", "binding", "judgment", "feedback"], "measured episode evidence");
    closed(row.reservations, ["reader", "judge", "verifier"], "episode reservations");
    closed(row.dispatches, ["reader", "judge", "verifier"], "episode dispatches");
    if (row.candidateId !== candidateId || row.taskId !== slot.taskId || row.evidence.taskId !== slot.taskId ||
        row.evidence.binding?.path !== slot.bindingPath) throw new Error("Measured evidence substituted a frozen candidate/task slot");
    const options = { ...episodeOptions(plan, competition, candidateId, slot, report.plan, policy.campaign, report.startedAt), completedBy: report.completedAt };
    const readerDispatch = await verifyEpisodeDispatch(options, "reader");
    if (!same(row.reservations.reader, readerDispatch.reservation) || !same(row.dispatches.reader, readerDispatch.pin)) throw new Error("Reader report differs from its charged actual dispatch");
    const artifacts = await verifyReaderEpisodeArtifacts(slot, readerDispatch.owner, report.startedAt);
    if (!same(row.collection, artifacts.collectionPin)) throw new Error("Measurement collection differs from its actual owner chain");
    const sealed = await loadReaderEpisode(row.evidence.binding);
    if (!same(sealed.attempt, artifacts.attempt) || !same(sealed.expected, slot.expected) || !same(sealed.evaluation, plan.evaluations[candidateId])) throw new Error("Measurement seal belongs to another exact product/task episode");
    if (!row.terminal || row.terminal.path !== join(plan.outputRoot, "episodes", `${options.key}.json`)) throw new Error("Fresh measurement terminal is unavailable");
    const terminal = await pinned(row.terminal);
    if (!same(terminal, { schema: "gptgrep.controller-episode.v1", manifestSha256: report.plan.sha256, evidence: row.evidence })) throw new Error("Reported episode differs from its original terminal");
    const source = await pinned(artifacts.collection.sourceAttempt), inventory = await pinned(artifacts.collection.sourceManifest);
    const relocated = new Map<string, Pin>(artifacts.reader.artifactRelocation.files.map((item: any) => [item.sourcePath, item.artifact]));
    const capture = (role: string) => {
      const matches = inventory.artifacts.filter((item: any) => item.role === role);
      if (matches.length !== 1) throw new Error("Reader RAW/ATIF inventory is incomplete or duplicated");
      const item = matches[0], retained = relocated.get(item.path);
      if (!retained || retained.sha256 !== item.sha256) throw new Error("Reader RAW/ATIF relocation changed original bytes");
      return { original: { path: item.path, sha256: item.sha256 }, retained };
    };
    const trace = capture("trace"), coverage = capture("coverage"), trajectory = capture("trajectory");
    const readerCapture = await verifyNativeEpisodeCapture({ native: source.native, sourceTrace: trace.original, trace: trace.retained,
      coverage: coverage.retained, trajectory: trajectory.retained, mediaRelocation: relocated, process: readerDispatch.process, requireComplete: sealed.reader_status === "completed" });
    retainIdentity(source.native, readerCapture.threadIds);
    if (sealed.reader_status === "completed") {
      if (row.evidence.judgment?.path !== slot.judgmentPath || row.evidence.feedback?.path !== slot.feedbackPath ||
          row.judgmentCoverage?.path !== `${slot.judgmentPath}.coverage.json`) throw new Error("Completed reader lacks its own fresh judge/verifier artifacts");
      const judgeDispatch = await verifyEpisodeDispatch(options, "judge"), verifierDispatch = await verifyEpisodeDispatch(options, "verifier");
      for (const [role, dispatch] of [["judge", judgeDispatch], ["verifier", verifierDispatch]] as const) {
        if (!same(row.reservations[role], dispatch.reservation) || !same(row.dispatches[role], dispatch.pin)) throw new Error("Scoring report differs from its charged actual dispatch");
      }
      const judgment = await pinned(row.evidence.judgment), judge = judgment.judge_run;
      const judgeTrace = await observedPin(judge?.tracePath), judgeTrajectory = await observedPin(`${slot.judgmentPath}.trajectory.json`);
      if (!judgeTrace || !judgeTrajectory) throw new Error("Fresh judge RAW/ATIF artifacts are unavailable");
      const judgeCapture = await verifyNativeEpisodeCapture({ native: judge, sourceTrace: judgeTrace,
        trace: judgeTrace, coverage: row.judgmentCoverage, trajectory: judgeTrajectory, process: judgeDispatch.process, requireComplete: true });
      retainIdentity(judge, judgeCapture.threadIds);
      if (Date.parse(judgeDispatch.process.startedAt) < Date.parse(readerDispatch.process.completedAt) ||
          Date.parse(verifierDispatch.process.startedAt) < Date.parse(judgeDispatch.process.completedAt)) throw new Error("Judge/verifier did not follow their actual source episode");
    } else {
      if (row.evidence.judgment !== null || row.evidence.feedback !== null || row.judgmentCoverage !== null ||
          row.reservations.judge !== null || row.reservations.verifier !== null || row.dispatches.judge !== null || row.dispatches.verifier !== null) throw new Error("Failed reader cannot inherit another episode's scoring evidence");
      for (const role of ["judge", "verifier"] as const) {
        if (await exists(campaignActionPath(envelope, { experimentId: competition.competitionId, actionId: `${options.key}.${role}` })) ||
            await exists(join(plan.outputRoot, "dispatch", `${options.key}.${role}.json`))) throw new Error("Failed reader has unexplained scoring effects");
      }
    }
    const outcome = await outcomeFromEvidence(row.evidence);
    if (!["complete", "operational_failure"].includes(outcome.status) || row.disposition !== outcome.status) throw new Error("Unknown outcomes cannot complete fresh measurement attachment");
    episodes[candidateId]!.push(row.evidence);
  }
  for (const candidateId of roles(competition)) {
    const run = await pinned<EvaluationRun>(report.runs[candidateId]);
    const outcomes = await Promise.all(episodes[candidateId]!.map(outcomeFromEvidence));
    const ordered = competition.cohort.map(taskId => outcomes.find(value => value.taskId === taskId)!);
    if (report.runs[candidateId].path !== join(plan.outputRoot, `measured-${candidateId}.json`) ||
        !same(run, { ...evaluations[candidateId].run, outcomes: ordered }) || !summarize(run).comparable) throw new Error("Measured run differs from its complete original fixed-denominator evidence");
  }
  return report as { runs: Record<string, Pin>; qualifiedCandidatePins: Record<string, Pin> };
}
