import { constants } from "node:fs";
import { copyFile, lstat, mkdir, open, readFile, realpath, stat } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { assertTaskAdmitted, loadAdmission, verifyPin, verifyRequiredPreprocessing, type AdmissionReference, type Pin } from "./availability";
import { loadReaderEpisode, outcomeFromEvidence, sealReaderEpisode, writeEvaluationRun, type EpisodeEvidence } from "./episode";
import { canonical, digest, fileHash, physicalPath, readJson, requireStorage, within, writeJsonExclusive } from "./io";
import { judgeAnswer, type JudgeOptions } from "./judge";
import { indexTaskFiles, validImmutableImage } from "./harbor-launch";
import { preparationDigest, readerOrder, readerPrompt, runReaderArm, StartedReaderArtifactError, type ReaderArm, type ReaderArmOptions, type ReaderArmResult } from "./runner";
import { ATIF_SOURCE_REVISION, ATIF_SPEC_SHA256, collectNativeMedia, convertNativeCapture, writeNativeTrajectory } from "./trajectory";
import { campaignActionPath, reserveCampaignAction, validateCampaignEnvelope, verifyCampaignAction, type CampaignEnvelopeV2 } from "./campaign";
import { executeOwnerCommand, type ProcessCommand, type ProcessObservation } from "./process";
import type { NativeRunResult } from "./native";
import { expectedHarborEnvironment, runHarborOwnerWrapper, validateHarborWrapperJob, type HarborOwnerWrapperJob } from "./harbor-wrapper";
import { verifyContainerReaderChain } from "./container";
export { executeOwnerCommand, type ProcessCommand, type ProcessObservation } from "./process";

export type EpisodePurpose = "reader" | "judge" | "repair_reader" | "repair_judge" | "c4_reader" | "c4_judge" | "calibration" | "probe";
export interface CampaignEnvelope extends CampaignEnvelopeV2 {
  episodeDeadlineMs: number;
  maxTraceBytes: number;
  maxMediaBytes: number;
  maxTrajectoryBytes: number;
  nativeDeadlineMs: number;
  observationLimits: { toolCalls: number; childThreads: number; retryEvents: number };
  providerRequestLimit: null;
  dollarLimitUsd: null;
}
export interface EpisodeSlot {
  taskId: string;
  arm: ReaderArm;
  order: number;
  harborJob: Pin;
  wrapperJob: Pin;
  bridgeContract: Pin;
  readerJob: Pin;
  judgeJob: Pin;
  harborExport: Pin;
  reader: ProcessCommand;
  judge: ProcessCommand;
  verifier: ProcessCommand;
  attemptPath: string;
  expected: Pin;
  bindingPath: string;
  judgmentPath: string;
  feedbackPath: string;
  /** Exact owner-generated terminal receipts, consumed after each process. */
  readerTerminal: OwnerTerminalPaths;
  judgeTerminal: OwnerTerminalPaths;
  verifierTerminal: Pick<OwnerTerminalPaths, "storage" | "storagePlanDigest">;
}
export interface OwnerTerminalPaths { storage: string; auth: string; storagePlanDigest: string; authRunId: string }
export interface ControllerManifest {
  schema: "gptgrep.paired-controller.v2";
  experimentId: string;
  attemptKind: "primary" | "repair";
  envelope: Pin;
  evaluations: Record<ReaderArm, Pin>;
  seed: string;
  taskIds: string[];
  slots: EpisodeSlot[];
  outputRoot: string;
  cumulativeBatches: number[];
  /** Source-ready allows the first real batch; it never claims live qualification. */
  runtimeReadiness: Pin;
}

const id = (value: string) => /^[A-Za-z0-9][A-Za-z0-9_.-]{0,120}$/.test(value);
const integer = (value: number, min = 1) => Number.isSafeInteger(value) && value >= min;
async function immutable(path: string, value: unknown): Promise<Pin> {
  await requireStorage(path); await physicalPath(path); await mkdir(dirname(path), { recursive: true });
  const file = await open(path, "wx", 0o600);
  try { await file.writeFile(`${JSON.stringify(value, null, 2)}\n`); await file.sync(); } finally { await file.close(); }
  return { path: resolve(path), sha256: await fileHash(path) };
}
async function immutableOrEqual(path: string, value: unknown): Promise<Pin> {
  if (!await exists(path)) return immutable(path, value);
  if (canonical(await readJson(path)) !== canonical(value)) throw new Error("Retained immutable output differs from recomputed evidence");
  return { path, sha256: await fileHash(path) };
}
async function exists(path: string): Promise<boolean> {
  try { await stat(path); return true; } catch (error: any) { if (error.code === "ENOENT") return false; throw error; }
}
async function artifactPin(path: string): Promise<Pin> {
  if (basename(path) === "auth.json") throw new Error("Credential files cannot be episode artifacts");
  await physicalPath(path); return { path, sha256: await fileHash(path) };
}
export function validateEnvelope(value: CampaignEnvelope): void {
  validateCampaignEnvelope(value);
  if (!integer(value.episodeDeadlineMs) || value.episodeDeadlineMs > 3_600_000 ||
      !integer(value.maxTraceBytes) || value.maxTraceBytes > 64 * 1024 * 1024 || !integer(value.maxMediaBytes) ||
      !integer(value.maxTrajectoryBytes) || value.maxTrajectoryBytes > 128 * 1024 * 1024 || value.maxMediaBytes > 64 * 1024 * 1024 ||
      !integer(value.nativeDeadlineMs) || value.nativeDeadlineMs > value.episodeDeadlineMs ||
      !value.observationLimits || Object.keys(value.observationLimits).length !== 3 || Object.values(value.observationLimits).some(limit => !integer(limit, 0) || limit > 10_000) ||
      value.providerRequestLimit !== null || value.dollarLimitUsd !== null) throw new Error("Invalid finite runtime envelope or invented provider/USD limit");
  for (const purpose of ["reader", "judge", "repair_reader", "repair_judge", "c4_reader", "c4_judge", "calibration", "probe"] as const) {
    if (value.allowances[purpose].deadlineMs !== value.episodeDeadlineMs) throw new Error("Native episode deadline differs from its shared campaign allowance");
  }
}

/** Reader/judge dispatch uses the same durable ledger as every other campaign front door. */
export async function reserveEpisode(envelopePin: Pin, options: { experimentId: string; slotId: string; purpose: EpisodePurpose; commandSha256: string }): Promise<Pin> {
  await verifyPin(envelopePin); validateEnvelope(await readJson<CampaignEnvelope>(envelopePin.path));
  return reserveCampaignAction(envelopePin, { experimentId: options.experimentId, actionId: options.slotId,
    purpose: options.purpose, commandSha256: options.commandSha256 });
}

export async function verifyStorageTerminal(paths: Pick<OwnerTerminalPaths, "storage" | "storagePlanDigest">) {
  const storage = await readJson(paths.storage);
  if (storage.schema !== "environments-harness.storage-receipt.v1" || storage.status !== "success" || storage.plan_digest !== paths.storagePlanDigest ||
      storage.error !== null || storage.worker?.terminal !== true || storage.worker.returncode !== 0 || storage.worker.reason !== null ||
      storage.internal_metadata?.ceiling_met !== true || !storage.worker.startup_receipt) throw new Error("Storage native terminal/cleanup is unavailable");
  await verifyPin(storage.worker.startup_receipt);
  await verifyPin({ path: storage.binding_path, sha256: storage.binding_sha256 });
  return { storage: { path: paths.storage, sha256: await fileHash(paths.storage) },
    binding: { path: storage.binding_path, sha256: storage.binding_sha256 }, bindingSha256: storage.binding_sha256 };
}

export async function verifyAuthTerminal(paths: Pick<OwnerTerminalPaths, "auth" | "authRunId">) {
  if (basename(paths.auth) !== "receipt.json") throw new Error("Auth cleanup requires the selected native receipt.json");
  const auth = await readJson(paths.auth);
  if (auth.schema !== "gptgrep.codex-auth.run.v1" || auth.operation !== "run" || auth.run_id !== paths.authRunId || !id(auth.role ?? "") ||
      !["PASS", "FAILED", "TIMEOUT"].includes(auth.state) || auth.sweep?.inconclusive !== false ||
      !Array.isArray(auth.sweep.survivor_pids) || auth.sweep.survivor_pids.length !== 0 || auth.credential_links?.complete !== true ||
      auth.credential_links.found !== 0 || auth.auth_link_final?.released !== true || auth.auth_block || auth.child_exit_code === 77) throw new Error("Auth native terminal/cleanup is unavailable");
  const layout = auth.layout;
  if (!layout || !isAbsolute(layout.run_dir ?? "") || resolve(layout.run_dir) !== layout.run_dir ||
      basename(layout.run_dir) !== paths.authRunId || basename(dirname(layout.run_dir)) !== auth.role ||
      resolve(paths.auth) !== join(layout.run_dir, "receipt.json") || layout.codex_home !== join(layout.run_dir, "codex-home") ||
      layout.auth_link !== join(layout.codex_home, "auth.json")) throw new Error("Auth cleanup has no exact selected run layout");
  await physicalPath(layout.codex_home);
  const home = await lstat(layout.codex_home);
  if (!home.isDirectory() || home.isSymbolicLink() || home.uid !== process.getuid?.() || await realpath(layout.codex_home) !== layout.codex_home) throw new Error("Auth cleanup parent custody is unavailable");
  // Metadata only: never open, hash, follow or read the credential link.
  try { await lstat(layout.auth_link); throw new Error("Auth cleanup left the selected credential link present"); }
  catch (error: any) { if (error.code !== "ENOENT") throw error; }
  return { auth: { path: paths.auth, sha256: await fileHash(paths.auth) },
    authLinkAbsence: { path: layout.auth_link, observedAt: new Date().toISOString(), method: "lstat-ENOENT", parentDevice: home.dev, parentInode: home.ino } };
}
export async function verifyOwnerTerminal(paths: OwnerTerminalPaths) {
  const storage = await verifyStorageTerminal(paths);
  return { ...storage, ...await verifyAuthTerminal(paths) };
}

export function validateHarborJob(job: any, bridgePin: Pin, bridge: any, slot: Pick<EpisodeSlot, "taskId" | "arm" | "attemptPath">, template: any) {
  const agent = job.agents?.[0];
  if (job.n_attempts !== 1 || job.n_concurrent_trials !== 1 || job.retry?.max_retries !== 0 || job.timeout_multiplier !== 1 ||
      job.agents?.length !== 1 || job.tasks?.length !== 1 || job.datasets?.length || !isAbsolute(job.jobs_dir ?? "") ||
      job.verifier?.disable !== true || job.extra_instructions?.length || job.extra_instruction_paths?.length || job.user_agent ||
      !agent || agent.import_path !== "native_agent:NativeCodexAgent" || agent.name || agent.model_name !== "gpt-6.1-sol" || agent.n_concurrent !== 1 ||
      agent.base_url || agent.model_api_key_name || agent.model_api_format || agent.skills?.length || agent.load_trajectory || agent.resume_trajectory ||
      canonical(agent.kwargs) !== canonical({ bridge_contract_path: bridgePin.path, bridge_contract_sha256: bridgePin.sha256 })) throw new Error("Harbor job must freeze the custom native route, single attempt/concurrency and zero Harbor retries");
  if (bridge.schema !== "gptgrep.harbor-native-bridge.v2" || bridge.taskId !== slot.taskId || bridge.arm !== slot.arm ||
      resolve(bridge.hostOutputRoot, "attempt.json") !== slot.attemptPath || bridge.model !== "gpt-6.1-sol" ||
      !validImmutableImage(template?.image_pin) || !Array.isArray(template.mounts)) throw new Error("Harbor bridge differs from its paired controller slot");
  const environment = expectedHarborEnvironment(template, bridge.containerOwner?.procEnvironment);
  if (canonical(job.environment) !== canonical(environment) || Object.keys(job.tasks[0]).join(",") !== "path") throw new Error("Harbor requires the exact prebuilt image, no-upload configuration and reviewed owner binds");
}

/** Host relocation may change only file addresses; the original terminal record remains pinned. */
export async function verifyRelocatedAttempt(attempt: ReaderArmResult & { artifactRelocation?: any }, expected?: { taskId: string; arm: ReaderArm; readerJobSha256: string }) {
  const relocation = attempt.artifactRelocation;
  if (!relocation || !Array.isArray(relocation.files)) throw new Error("Harbor attempt lacks source-bound artifact relocation");
  await verifyPin(relocation.sourceAttempt); await verifyPin(relocation.sourceManifest);
  const source = await readJson(relocation.sourceAttempt.path), manifest = await readJson(relocation.sourceManifest.path);
  if (manifest.schema !== "gptgrep.harbor-native-artifacts.v1" || manifest.executionMode !== "live" ||
      !manifest.artifacts?.some((value: any) => value.role === "attempt" && value.sha256 === relocation.sourceAttempt.sha256) ||
      expected && (manifest.taskId !== expected.taskId || manifest.arm !== expected.arm || manifest.readerJobSha256 !== expected.readerJobSha256)) throw new Error("Relocated attempt is not the exact source bridge terminal");
  const mapping = new Map<string, Pin>();
  for (const item of relocation.files) {
    if (mapping.has(item.sourcePath)) throw new Error("Duplicate source artifact relocation");
    await verifyPin(item.artifact); mapping.set(item.sourcePath, item.artifact);
    if (!manifest.artifacts?.some((artifact: any) => artifact.path === item.sourcePath && artifact.sha256 === item.artifact.sha256)) throw new Error("Relocation is outside the source artifact inventory");
  }
  const projected = structuredClone(source);
  if (source.answerPath !== null) {
    const answer = mapping.get(source.answerPath);
    if (!answer || answer.sha256 !== source.answerSha256) throw new Error("Relocated answer differs from the reader terminal");
    projected.answerPath = answer.path;
  }
  if (source.native) {
    const trace = mapping.get(source.native.tracePath);
    if (!trace) throw new Error("Relocated raw capture is unavailable");
    projected.native.tracePath = trace.path;
  }
  projected.artifactRelocation = relocation;
  if (canonical(projected) !== canonical(attempt)) throw new Error("Relocation changed reader identity, status or measured content");
}

export async function verifyBatchAcceptance(manifestPin: Pin, reportPin: Pin, reviewPin: Pin) {
  await verifyPin(reportPin); await verifyPin(reviewPin);
  const report = await readJson(reportPin.path), review = await readJson(reviewPin.path);
  if (report.schema !== "gptgrep.controller-batch-report.v1" || canonical(report.manifest) !== canonical(manifestPin) || report.status !== "pending_independent_review" ||
      review.schema !== "gptgrep.controller-batch-review.v1" || review.status !== "accepted_for_next_batch" ||
      canonical(review.manifest) !== canonical(manifestPin) || canonical(review.report) !== canonical(reportPin) || !review.runtimeReview) throw new Error("Previous batch lacks review bound to its exact report and manifest");
  for (const pin of [...Object.values(report.runs) as Pin[], ...report.captures as Pin[]]) await verifyPin(pin);
  await verifyPin(review.runtimeReview); const runtime = await readJson(review.runtimeReview.path);
  if (runtime.schema !== "gptgrep.controller-runtime-review.v1" || !["qualification_in_progress", "qualified"].includes(runtime.status) ||
      canonical(runtime.manifest) !== canonical(manifestPin) || !runtime.reports?.some((pin: Pin) => canonical(pin) === canonical(reportPin)) ||
      !Array.isArray(runtime.observedCapabilities) || !Array.isArray(runtime.unavailableCapabilities)) throw new Error("Runtime review does not bind this exact real batch");
  return { report, review, runtime };
}

async function batchReport(manifest: ControllerManifest, manifestPin: Pin, evidence: Record<ReaderArm, EpisodeEvidence[]>, label: string, cumulativeTasks: number, error: string | null) {
  const runs: Partial<Record<ReaderArm, Pin>> = {};
  for (const arm of ["bare", "gptgrep"] as const) {
    const outputPath = join(manifest.outputRoot, `${label}.${arm}.json`);
    if (await exists(outputPath)) {
      const plan = await readJson(manifest.evaluations[arm].path), byId = new Map(evidence[arm].map(value => [value.taskId, value]));
      const outcomes = await Promise.all(manifest.taskIds.map(taskId => outcomeFromEvidence(byId.get(taskId) ?? { taskId, binding: null, judgment: null, feedback: null })));
      runs[arm] = await immutableOrEqual(outputPath, { ...plan.run, outcomes });
    } else {
      await writeEvaluationRun({ evaluation: manifest.evaluations[arm], episodes: evidence[arm], outputPath });
      runs[arm] = { path: outputPath, sha256: await fileHash(outputPath) };
    }
  }
  const captures: Pin[] = [];
  const slotDispositions = [];
  const envelope = await readJson<CampaignEnvelope>(manifest.envelope.path);
  for (const slot of manifest.slots.filter(slot => manifest.taskIds.slice(0, cumulativeTasks).includes(slot.taskId))) {
    const bridge = await readJson(slot.bridgeContract.path), collection = join(bridge.hostOutputRoot, "collection.json");
    if (await exists(collection)) captures.push({ path: collection, sha256: await fileHash(collection) });
    if (await exists(`${slot.judgmentPath}.coverage.json`)) captures.push({ path: `${slot.judgmentPath}.coverage.json`, sha256: await fileHash(`${slot.judgmentPath}.coverage.json`) });
    const found = evidence[slot.arm].find(value => value.taskId === slot.taskId), key = digest({ taskId: slot.taskId, arm: slot.arm }).slice(0, 24);
    const reserved = campaignActionPath(envelope, { experimentId: manifest.experimentId, actionId: `${key}.reader` });
    slotDispositions.push({ taskId: slot.taskId, arm: slot.arm, disposition: found ? (await outcomeFromEvidence(found)).status : await exists(reserved) ? "dispatch_uncertain" : "not_started",
      reservation: await exists(reserved) ? { path: reserved, sha256: await fileHash(reserved) } : null });
  }
  return immutableOrEqual(join(manifest.outputRoot, `${label}.report.json`), { schema: "gptgrep.controller-batch-report.v1", manifest: manifestPin,
    status: error === null ? "pending_independent_review" : "incomplete", cumulativeTasks, enrolledTasks: 135, runs, captures, slotDispositions, error, liveQualified: false });
}

/** Shared actual Harbor readiness; C2 and C4 keep their own enrollment gates. */
export async function verifyRuntimeReadiness(runtimeReadiness: Pin) {
  await verifyPin(runtimeReadiness);
  const ready = await readJson(runtimeReadiness.path);
  if (ready.schema !== "gptgrep.controller-runtime-readiness.v1" || ready.status !== "contract_ready_for_live_qualification" || ready.liveQualified !== false || !Array.isArray(ready.pins) || !ready.pins.length) throw new Error("Explicit pre-live runtime readiness is required");
  for (const pin of ready.pins) await verifyPin(pin);
  await verifyPin(ready.agentModule); await verifyPin(ready.agentImport);
  const agentImport = await readJson(ready.agentImport.path);
  if (agentImport.schema !== "gptgrep.harbor-agent-import.v1" || agentImport.status !== "imported" || agentImport.classPath !== "native_agent:NativeCodexAgent" ||
      canonical(agentImport.module) !== canonical(ready.agentModule) || agentImport.harborSourceRevision !== "6099b8cf2bfbbb3e7ff1eb2163bfc4524dbd9ba5") throw new Error("Actual Harbor class import must bind the exact reviewed module and framework source");
  return ready;
}

export interface EpisodeValidationOptions {
  slot: EpisodeSlot; evaluation: Pin; envelope: CampaignEnvelope; outputRoot: string;
  ready: Awaited<ReturnType<typeof verifyRuntimeReadiness>>;
  readerPurposes: Array<Extract<EpisodePurpose, "reader" | "judge" | "repair_reader" | "repair_judge" | "c4_reader" | "c4_judge">>;
}
/** One actual C2 reader/judge/container contract, without inventing a paired manifest. */
export async function validateEpisodeSlot(options: EpisodeValidationOptions): Promise<void> {
  const { slot, evaluation: evaluationPin, envelope, outputRoot, ready, readerPurposes } = options;
  if (!readerPurposes.length) throw new Error("Episode validation requires its exact campaign purposes");
  if (!within(outputRoot, slot.bindingPath)) throw new Error("Episode seal must remain in controller storage");
  for (const pin of [slot.harborJob, slot.wrapperJob, slot.bridgeContract, slot.readerJob, slot.judgeJob, slot.harborExport]) await verifyPin(pin);
  const harbor = await readJson(slot.harborJob.path), bridge = await readJson(slot.bridgeContract.path), readerJob = await readJson(slot.readerJob.path), judgeJob = await readJson(slot.judgeJob.path);
  const wrapper = await readJson<HarborOwnerWrapperJob>(slot.wrapperJob.path);
  await validateHarborWrapperJob(wrapper);
  if (canonical(wrapper.harborJob) !== canonical(slot.harborJob) || canonical(wrapper.bridgeContract) !== canonical(slot.bridgeContract) ||
      wrapper.timeoutMs + 2000 > envelope.episodeDeadlineMs) throw new Error("Harbor wrapper differs from its exact slot and total admission");
  await verifyPin(bridge.containerOwner.template);
  const ownerTemplate = await readJson(bridge.containerOwner.template.path);
  validateHarborJob(harbor, slot.bridgeContract, bridge, slot, ownerTemplate);
  if (canonical(bridge.agentModule) !== canonical(ready.agentModule) || bridge.readerJob.sha256 !== slot.readerJob.sha256 ||
      readerJob.options?.task?.id !== slot.taskId || readerJob.options?.arm !== slot.arm || readerJob.options.outputDir !== bridge.outputRoot ||
      readerJob.maxMediaBytes !== envelope.maxMediaBytes || readerJob.maxArtifactBytes !== bridge.maxArtifactBytes ||
      readerJob.maxTrajectoryBytes !== envelope.maxTrajectoryBytes || judgeJob.maxTrajectoryBytes !== envelope.maxTrajectoryBytes ||
      bridge.maxTraceBytes !== envelope.maxTraceBytes || bridge.maxTrajectoryBytes !== envelope.maxTrajectoryBytes || bridge.maxMediaBytes !== envelope.maxMediaBytes ||
      judgeJob.maxMediaBytes !== envelope.maxMediaBytes || judgeJob.options?.outputPath !== slot.judgmentPath || judgeJob.bindingPath !== slot.bindingPath ||
      judgeJob.options.documentSourceMode !== "pinned-read-only") throw new Error("Reader/judge bridge jobs differ from frozen identities, direct source access or media allowances");
  const logicalArtifactBound = envelope.maxTraceBytes + envelope.maxTrajectoryBytes + envelope.maxMediaBytes + 20 * 1024 * 1024;
  if (bridge.maxArtifactBytes < logicalArtifactBound || bridge.maxCollectedBytes < bridge.maxArtifactBytes + 10 * 1024 * 1024 + 2 * envelope.maxTrajectoryBytes ||
      readerPurposes.some(purpose => envelope.allowances[purpose].reservedBytesPerAdmission < bridge.maxArtifactBytes + bridge.maxCollectedBytes)) throw new Error("Episode reservation omits captured bytes or bridge retention copies");
  await verifyPin(evaluationPin); const evaluation = await readJson(evaluationPin.path);
  for (const [native, role] of [[readerJob.options.native, "reader"], [judgeJob.options.native, "judge"]] as const) {
    if (native.maxTraceBytes !== envelope.maxTraceBytes || native.mediaCapture?.maxBytes !== envelope.maxMediaBytes ||
        canonical(native.observationLimits) !== canonical(envelope.observationLimits) || native.timeoutMs !== envelope.nativeDeadlineMs || native.retainedChildCapture !== true ||
        native.effort !== evaluation.run.scenario[role === "reader" ? "reasoningEffort" : "judgeReasoningEffort"] ||
        native.serviceTier !== evaluation.run.scenario[role === "reader" ? "serviceTier" : "judgeServiceTier"] ||
        native.config?.["agents.default_subagent_model"] !== "gpt-6-luna" || native.config?.["agents.default_subagent_reasoning_effort"] !== "max" ||
        !integer(native.config?.["agents.max_concurrent_threads_per_session"]) || native.config["agents.max_concurrent_threads_per_session"] > envelope.observationLimits.childThreads) throw new Error("Native configuration or limits are not the frozen campaign values");
  }
  const exported = await readJson(slot.harborExport.path), entry = exported.tasks?.find((value: any) => value.taskId === slot.taskId);
  if (exported.schema !== "gptgrep.harbor-export.v1" || exported.arm !== slot.arm || !entry || harbor.tasks[0].path !== entry.directory ||
      canonical(await indexTaskFiles(entry.directory)) !== canonical(entry.files) || slot.expected.path !== join(entry.directory, "tests/expected.json")) throw new Error("Harbor job task source closure differs from the frozen export");
  await verifyPin(slot.expected); const expected = await readJson(slot.expected.path);
  if (digest(readerJob.options.task) !== expected.reader_task_sha256) throw new Error("Reader task bytes differ from their frozen exported hash");
  const taskManifest = await readJson(join(entry.directory, "adapter-manifest.json"));
  const taskConfig = Bun.TOML.parse(await readFile(join(entry.directory, "task.toml"), "utf8")) as any;
  if (canonical(taskManifest.execution) !== canonical({ kind: "prebuilt-bind", image: ownerTemplate.image_pin, workspace: "/app/task" }) ||
      taskConfig.environment?.docker_image !== ownerTemplate.image_pin || taskConfig.environment?.healthcheck || taskConfig.skills ||
      await exists(join(entry.directory, "environment/docker-compose.yaml")) || readerJob.options.workspace.cwd !== "/app/task") throw new Error("Live export lacks the frozen prebuilt task namespace");
  const args = slot.reader.argv;
  if (canonical(args.slice(-6)) !== canonical(["--mode", "harbor", "--job", slot.wrapperJob.path, "--expect-sha256", slot.wrapperJob.sha256]) ||
      !slot.reader.inputs.some(pin => canonical(pin) === canonical(slot.wrapperJob)) || !slot.reader.inputs.some(pin => canonical(pin) === canonical(slot.bridgeContract))) throw new Error("Reader owner command must select the exact finalizing Harbor wrapper");
  if (canonical(slot.judge.argv.slice(-6)) !== canonical(["--mode", "judge", "--job", slot.judgeJob.path, "--expect-sha256", slot.judgeJob.sha256]) ||
      !slot.judge.inputs.some(pin => canonical(pin) === canonical(slot.judgeJob))) throw new Error("Judge owner command must select the exact fresh independent judge job");
  if (slot.verifier.timeoutMs + 2000 > envelope.allowances.verifier.deadlineMs || !slot.verifierTerminal?.storage ||
      !/^[a-f0-9]{64}$/.test(slot.verifierTerminal.storagePlanDigest)) throw new Error("Verifier lacks its own fixed admission and storage terminal identity");
}

export async function validateControllerManifest(manifest: ControllerManifest): Promise<void> {
  if (manifest.schema !== "gptgrep.paired-controller.v2" || !id(manifest.experimentId) || !["primary", "repair"].includes(manifest.attemptKind) || manifest.taskIds.length !== 135 || new Set(manifest.taskIds).size !== 135 ||
      canonical(manifest.cumulativeBatches) !== canonical([2, 8, 25, 135]) || manifest.slots.length !== 270) throw new Error("Controller requires the complete frozen 135-task paired manifest");
  await verifyPin(manifest.envelope); validateEnvelope(await readJson(manifest.envelope.path));
  const ready = await verifyRuntimeReadiness(manifest.runtimeReadiness);
  const envelope = await readJson<CampaignEnvelope>(manifest.envelope.path);
  for (const arm of ["bare", "gptgrep"] as const) {
    await verifyPin(manifest.evaluations[arm]); const evaluation = await readJson(manifest.evaluations[arm].path);
    if (evaluation.run?.arm !== arm || canonical(evaluation.run?.cohort) !== canonical(manifest.taskIds) || evaluation.run?.scenario?.seed !== manifest.seed) throw new Error("Evaluation plan and controller enrollment differ");
  }
  const keys = new Set<string>();
  for (const slot of manifest.slots) {
    const key = `${slot.taskId}:${slot.arm}`, index = manifest.taskIds.indexOf(slot.taskId);
    if (index < 0 || keys.has(key) || !["bare", "gptgrep"].includes(slot.arm) || readerOrder(manifest.seed, index)[slot.order] !== slot.arm) throw new Error("Duplicate, foreign or misordered reader slot");
    keys.add(key);
    await validateEpisodeSlot({ slot, evaluation: manifest.evaluations[slot.arm], envelope, outputRoot: manifest.outputRoot, ready,
      readerPurposes: ["reader", "judge", "repair_reader", "repair_judge"] });
  }
}

export interface EpisodeExecutionOptions {
  slot: EpisodeSlot; evaluation: Pin; envelopePin: Pin; experimentId: string; manifestPin: Pin; outputRoot: string; key: string;
  readerPurpose: "reader" | "repair_reader" | "c4_reader";
  judgePurpose: "judge" | "repair_judge" | "c4_judge";
  /** C4's claim boundary prohibits adopting any historical terminal or dispatch. */
  freshAfter?: string;
  deadlineAt?: number;
}
export class EpisodeSlotError extends Error {
  constructor(error: unknown, readonly evidence: EpisodeEvidence) {
    super(error instanceof Error ? error.message : String(error));
    this.name = "EpisodeSlotError";
  }
}
export interface RetainedEpisodeOptions extends EpisodeExecutionOptions { completedBy?: string }
type EpisodeRole = "reader" | "judge" | "verifier";

/** Read existing dispatch and its complete common journal. This function cannot launch a process. */
export async function verifyEpisodeDispatch(options: RetainedEpisodeOptions, role: EpisodeRole) {
  const { slot, envelopePin, experimentId, manifestPin, outputRoot, key } = options;
  const command = slot[role], purpose = role === "reader" ? options.readerPurpose : role === "judge" ? options.judgePurpose : "verifier";
  const dispatchPin = await artifactPin(join(outputRoot, "dispatch", `${key}.${role}.json`));
  const dispatch = await readJson(dispatchPin.path);
  if (dispatch.schema !== (role === "verifier" ? "gptgrep.controller-verifier.v2" : "gptgrep.controller-dispatch.v1") ||
      canonical(dispatch.manifest) !== canonical(manifestPin) || role === "verifier" && dispatch.commandSha256 !== digest(command)) throw new Error("Retained dispatch does not bind the exact episode command");
  const charged = await verifyCampaignAction(envelopePin, { experimentId, actionId: `${key}.${role}`, purpose, commandSha256: digest(command) });
  if (canonical(dispatch.reservation) !== canonical(charged.reservation)) throw new Error("Dispatch points to another shared campaign reservation");
  await verifyPin(command.executable); for (const pin of command.inputs) await verifyPin(pin);
  const process = dispatch.process as ProcessObservation, start = Date.parse(process?.startedAt), end = Date.parse(process?.completedAt);
  const earliest = Math.max(Date.parse(charged.row.reservedAt), options.freshAfter ? Date.parse(options.freshAfter) : 0);
  const latest = Math.min(Date.parse(charged.row.deadline), options.completedBy ? Date.parse(options.completedBy) : Date.now());
  if (!process || process.started !== true || !integer(process.pid!) || process.executionError !== null || process.timedOut !== false ||
      process.outputExceeded !== false || process.cleanup !== true || !Number.isFinite(start) || !Number.isFinite(end) ||
      !Number.isFinite(earliest) || !Number.isFinite(latest) || start < earliest || end < start || end > latest ||
      options.deadlineAt !== undefined && Date.parse(charged.row.deadline) > options.deadlineAt) throw new Error("Retained dispatch has unresolved cleanup, stale times or an exceeded frozen deadline");
  const owner = role === "verifier" ? await verifyStorageTerminal(slot.verifierTerminal)
    : await verifyOwnerTerminal(role === "reader" ? slot.readerTerminal : slot.judgeTerminal);
  return { pin: dispatchPin, reservation: charged.reservation, process, owner };
}

/** Reconstruct the actual converter's output from retained raw capture; never write replacement sidecars. */
export async function verifyNativeEpisodeCapture(options: {
  native: NativeRunResult; sourceTrace: Pin; trace: Pin; coverage: Pin; trajectory: Pin;
  mediaRelocation?: Map<string, Pin>; process: Pick<ProcessObservation, "startedAt" | "completedAt">; requireComplete: boolean;
}) {
  const { native, sourceTrace, trace, coverage, trajectory } = options;
  for (const pin of [trace, coverage, trajectory]) {
    if (basename(pin.path) === "auth.json") throw new Error("Credential files cannot be capture evidence");
    await verifyPin(pin);
  }
  if (native.executionMode !== "live" || !native.validStart || !native.threadId || !native.turnId || native.tracePath !== sourceTrace.path ||
      sourceTrace.sha256 !== trace.sha256 || !Array.isArray(native.mediaBindings)) throw new Error("Exact live native capture identity and media map required");
  const info = await lstat(trace.path);
  if (!info.isFile() || info.size > 64 * 1024 * 1024) throw new Error("Native capture exceeds its retained bound");
  const bytes = await readFile(trace.path), rows = bytes.toString("utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
  const start = Date.parse(options.process.startedAt), end = Date.parse(options.process.completedAt);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start || rows.length === 0 ||
      rows.some(row => !Number.isFinite(Date.parse(row.at)) || Date.parse(row.at) < start || Date.parse(row.at) > end)) throw new Error("Capture timestamps are outside their actual fresh dispatch");
  for (const media of native.mediaBindings) {
    const retained = options.mediaRelocation ? options.mediaRelocation.get(media.artifact.path) : media.artifact;
    if (!retained || retained.sha256 !== media.artifact.sha256 || basename(retained.path) === "auth.json") throw new Error("Native media differs from retained original artifacts");
    await verifyPin(retained);
  }
  const resultRows = rows.filter(row => row.direction === "result");
  if (resultRows.length > 1 || options.requireComplete && resultRows.length !== 1 || resultRows.some(row => row.threadId !== native.threadId ||
      row.turnId !== native.turnId || row.sessionId !== native.sessionId || row.requestedModel !== native.requestedModel || row.observedModel !== native.observedModel ||
      row.servedModel !== native.servedModel || canonical(row.usage) !== canonical(native.usage) || canonical(row.cleanup) !== canonical(native.cleanup) ||
      options.requireComplete && row.status !== "completed")) throw new Error("Raw terminal capture differs from the selected native episode");
  if (native.requestCount !== null || native.costUsd !== null || native.usageScope !== "root_thread_only") throw new Error("Unavailable native provider counts/costs cannot be supplied as measured values");
  const converted = convertNativeCapture(rows, native, native.mediaBindings), actualCoverage = await readJson(coverage.path);
  const provenance = { trace: sourceTrace, converterSha256: await fileHash(fileURLToPath(new URL("./trajectory.ts", import.meta.url))), sourceSha256: trace.sha256,
    schemaSourceRevision: ATIF_SOURCE_REVISION, schemaSpecSha256: ATIF_SPEC_SHA256, runtimeBinarySha256: native.runtimeBinarySha256,
    requestedModel: native.requestedModel, requestedEffort: native.requestedEffort, requestedServiceTier: native.requestedServiceTier };
  if (canonical(actualCoverage) !== canonical({ ...converted.coverage, provenance }) ||
      canonical(await readJson(trajectory.path)) !== canonical(converted.trajectory)) throw new Error("Retained trajectory or coverage differs from actual raw-capture recomputation");
  if (options.requireComplete && (!converted.coverage.complete || converted.coverage.gaps.length)) throw new Error("Completed episode has incomplete native trajectory coverage");
  const threadIds = [converted.trajectory, ...(converted.trajectory.subagent_trajectories ?? [])].map((value: any) => value.extra.native_thread_id as string);
  return { ...converted, trace, coveragePin: coverage, trajectoryPin: trajectory, threadIds };
}

export async function verifyReaderEpisodeArtifacts(slot: EpisodeSlot, readerTerminal: { bindingSha256: string }, freshAfter?: string) {
  const taskId = slot.taskId, attempt = await artifactPin(slot.attemptPath);
  const reader = await readJson<ReaderArmResult & { artifactRelocation: any }>(attempt.path);
  if (freshAfter && Date.parse(reader.startedAt) < Date.parse(freshAfter)) throw new Error("Reader attempt predates this fresh measurement claim");
  await verifyRelocatedAttempt(reader, { taskId, arm: slot.arm, readerJobSha256: slot.readerJob.sha256 });
  await verifyPin(slot.bridgeContract);
  const bridge = await readJson(slot.bridgeContract.path); await verifyPin(bridge.containerOwner.template);
  const template = await readJson(bridge.containerOwner.template.path), collectionPin = await artifactPin(join(bridge.hostOutputRoot, "collection.json"));
  const collection = await readJson(collectionPin.path);
  if (collection.schema !== "gptgrep.harbor-native-collection.v2" || collection.taskId !== taskId || collection.arm !== slot.arm ||
      canonical(collection.readerJob) !== canonical(slot.readerJob) || canonical(collection.bridgeContract) !== canonical(slot.bridgeContract) ||
      canonical(collection.attempt) !== canonical(attempt)) throw new Error("Reader collection differs from the frozen slot");
  const chain = await verifyContainerReaderChain({ ...collection.ownerEvidence, derivedPlanRule: bridge.containerOwner.derivedPlanRule,
    finalization: await artifactPin(template.outputs.post_broker_finalization), parentHostBindingSha256: readerTerminal.bindingSha256,
    readerJob: slot.readerJob, taskId, arm: slot.arm, sourceAttempt: collection.sourceAttempt, sourceManifest: collection.sourceManifest, projectedOutputRoot: bridge.hostOutputRoot });
  if (reader.authRunId !== slot.readerTerminal.authRunId || reader.storageBindingSha256 !== chain.readerBindingSha256) throw new Error("Reader attempt differs from its actual guest command or owner auth terminal");
  return { attempt, reader, bridge, collection, collectionPin };
}
/** The existing C2 dispatch/sealing/scoring kernel, also used by fresh C4 slots. */
export async function runEpisodeSlot(options: EpisodeExecutionOptions): Promise<EpisodeEvidence> {
  const { slot, evaluation, envelopePin, experimentId, manifestPin, outputRoot, key, readerPurpose, judgePurpose, freshAfter, deadlineAt } = options;
  const taskId = slot.taskId, terminalPath = join(outputRoot, "episodes", `${key}.json`);
  let evidence: EpisodeEvidence = { taskId, binding: null, judgment: null, feedback: null };
  try {
  if (!id(key) || !id(experimentId) || !["reader", "repair_reader", "c4_reader"].includes(readerPurpose) ||
      judgePurpose !== readerPurpose.replace("reader", "judge") || freshAfter !== undefined && !Number.isFinite(Date.parse(freshAfter)) ||
      deadlineAt !== undefined && !Number.isSafeInteger(deadlineAt)) throw new Error("Invalid bounded episode execution identity");
  await verifyPin(envelopePin); const envelope = await readJson<CampaignEnvelope>(envelopePin.path); validateEnvelope(envelope);
  const assertDeadline = (allowanceMs: number) => {
    if (deadlineAt !== undefined && Date.now() + allowanceMs > deadlineAt) throw new Error("Competition deadline cannot fit this complete action and cleanup allowance");
  };
  if (await exists(terminalPath)) {
    if (freshAfter) throw new Error("Fresh measurement cannot reuse a retained episode terminal");
    const terminal = await readJson(terminalPath);
    if (terminal.manifestSha256 !== manifestPin.sha256) throw new Error("Cannot reuse a terminal under changed scenario bytes");
    const outcome = await outcomeFromEvidence(terminal.evidence);
    evidence = terminal.evidence;
    if (["setup_failed", "judge_unknown", "pending"].includes(outcome.status)) throw new Error("Retained episode remains unknown; repair/reconciliation required");
    return evidence;
  }
  const dispatch = async (purpose: "reader" | "judge", command: ProcessCommand) => {
    const dispatchPath = join(outputRoot, "dispatch", `${key}.${purpose}.json`);
    let process;
    if (await exists(dispatchPath)) {
      if (freshAfter) throw new Error("Fresh measurement cannot reuse an earlier dispatch; reconciliation required");
      const retained = await readJson(dispatchPath); await verifyPin(retained.reservation);
      const reservation = await readJson(retained.reservation.path);
      if (canonical(retained.manifest) !== canonical(manifestPin) || reservation.commandSha256 !== digest(command) ||
          reservation.purpose !== (purpose === "reader" ? readerPurpose : judgePurpose) || canonical(reservation.envelope) !== canonical(envelopePin)) throw new Error("Retained dispatch differs from the frozen command");
      process = retained.process;
    } else {
      assertDeadline(envelope.episodeDeadlineMs);
      const reservation = await reserveEpisode(envelopePin, { experimentId: experimentId, slotId: `${key}.${purpose}`, purpose: purpose === "reader" ? readerPurpose : judgePurpose, commandSha256: digest(command) });
      process = await executeOwnerCommand(command, envelope.episodeDeadlineMs);
      await immutable(dispatchPath, { schema: "gptgrep.controller-dispatch.v1", reservation, manifest: manifestPin, process });
    }
    if (!process.started || process.executionError || process.timedOut || process.outputExceeded || !process.cleanup) throw new Error("Dispatch has uncertain cleanup; reconciliation required");
    return verifyOwnerTerminal(purpose === "reader" ? slot.readerTerminal : slot.judgeTerminal);
  };
  const readerTerminal = await dispatch("reader", slot.reader);
  const { attempt } = await verifyReaderEpisodeArtifacts(slot, readerTerminal, freshAfter);
  const binding = await exists(slot.bindingPath) ? { path: slot.bindingPath, sha256: await fileHash(slot.bindingPath) }
    : await sealReaderEpisode({ attempt, expected: slot.expected, evaluation: evaluation, outputPath: slot.bindingPath });
  const sealed = await loadReaderEpisode(binding);
  if (canonical(sealed.attempt) !== canonical(attempt) || canonical(sealed.evaluation) !== canonical(evaluation)) throw new Error("Recovered reader seal differs from current frozen artifacts");
  evidence = { taskId, binding, judgment: null, feedback: null };
  let judgment: Pin | null = null, feedback: Pin | null = null;
  if (sealed.reader_status === "completed") {
    await dispatch("judge", slot.judge);
    judgment = { path: slot.judgmentPath, sha256: await fileHash(slot.judgmentPath) };
    const verifierPath = join(outputRoot, "dispatch", `${key}.verifier.json`);
    let process;
    if (await exists(verifierPath)) {
      if (freshAfter) throw new Error("Fresh measurement cannot reuse an earlier verifier dispatch");
      const retained = await readJson(verifierPath);
      if (canonical(retained.manifest) !== canonical(manifestPin) || retained.commandSha256 !== digest(slot.verifier)) throw new Error("Retained verifier belongs to another frozen command");
      await verifyPin(retained.reservation);
      const reservation = await readJson(retained.reservation.path);
      if (reservation.purpose !== "verifier" || reservation.commandSha256 !== digest(slot.verifier) ||
          canonical(reservation.envelope) !== canonical(envelopePin)) throw new Error("Retained verifier lacks its shared campaign charge");
      process = retained.process;
    } else {
      assertDeadline(envelope.allowances.verifier.deadlineMs);
      const reservation = await reserveCampaignAction(envelopePin, { experimentId: experimentId,
        actionId: `${key}.verifier`, purpose: "verifier", commandSha256: digest(slot.verifier) });
      process = await executeOwnerCommand(slot.verifier, envelope.allowances.verifier.deadlineMs);
      await immutable(verifierPath, { schema: "gptgrep.controller-verifier.v2", manifest: manifestPin, commandSha256: digest(slot.verifier), reservation, process });
    }
    if (!process.started || process.executionError || process.timedOut || process.outputExceeded || !process.cleanup) throw new Error("Verifier cleanup is unavailable");
    await verifyStorageTerminal(slot.verifierTerminal);
    feedback = { path: slot.feedbackPath, sha256: await fileHash(slot.feedbackPath) };
  }
  const episode = { taskId, binding, judgment, feedback };
  await immutable(terminalPath, { schema: "gptgrep.controller-episode.v1", manifestSha256: manifestPin.sha256, evidence: episode });
  const outcome = await outcomeFromEvidence(episode);
  evidence = episode;
  if (["setup_failed", "judge_unknown", "pending"].includes(outcome.status)) throw new Error("Episode remains unknown; batch stopped with reservation retained");
  return evidence;
  } catch (error) { throw new EpisodeSlotError(error, evidence); }
}

/** Serial batch execution. Resuming an uncertain dispatch requires external reconciliation. */
export async function runControllerBatch(manifestPin: Pin, cumulativeTasks: number, reviewPin: Pin) {
  await verifyPin(manifestPin); const manifest = await readJson<ControllerManifest>(manifestPin.path);
  await validateControllerManifest(manifest); await requireStorage(manifest.outputRoot);
  await verifyPin(reviewPin); const review = await readJson(reviewPin.path);
  if (review.schema !== "gptgrep.controller-manifest-review.v1" || review.status !== "accepted" || canonical(review.manifest) !== canonical(manifestPin) || canonical(review.envelope) !== canonical(manifest.envelope)) throw new Error("Exact controller and campaign envelope require independent pre-live review");
  if (!manifest.cumulativeBatches.includes(cumulativeTasks)) throw new Error("Unfrozen batch boundary");
  const index = manifest.cumulativeBatches.indexOf(cumulativeTasks);
  if (index > 0) {
    const previous = manifest.cumulativeBatches[index - 1]!, accepted = await readJson(join(manifest.outputRoot, `batch-${previous}.acceptance.json`));
    if (accepted.schema !== "gptgrep.controller-batch-acceptance.v1" || !accepted.review) throw new Error("Previous real batch lacks independent acceptance");
    const reportPath = join(manifest.outputRoot, `batch-${previous}.report.json`);
    await verifyBatchAcceptance(manifestPin, { path: reportPath, sha256: await fileHash(reportPath) }, accepted.review);
  }
  const evidence: Record<ReaderArm, EpisodeEvidence[]> = { bare: [], gptgrep: [] };
  try {
  for (const taskId of manifest.taskIds.slice(0, cumulativeTasks)) {
    const slots = manifest.slots.filter(slot => slot.taskId === taskId).sort((a, b) => a.order - b.order);
    for (const slot of slots) {
      const key = digest({ taskId, arm: slot.arm }).slice(0, 24);
      try {
        const episode = await runEpisodeSlot({ slot, evaluation: manifest.evaluations[slot.arm], envelopePin: manifest.envelope,
          experimentId: manifest.experimentId, manifestPin, outputRoot: manifest.outputRoot, key,
          readerPurpose: manifest.attemptKind === "repair" ? "repair_reader" : "reader",
          judgePurpose: manifest.attemptKind === "repair" ? "repair_judge" : "judge" });
        evidence[slot.arm].push(episode);
      } catch (error) {
        if (error instanceof EpisodeSlotError && error.evidence.binding) evidence[slot.arm].push(error.evidence);
        throw error;
      }
    }
  }
  const report = await batchReport(manifest, manifestPin, evidence, `batch-${cumulativeTasks}`, cumulativeTasks, null);
  return { status: "pending_independent_review", cumulativeTasks, manifest: manifestPin, report, liveQualified: false };
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    const report = await batchReport(manifest, manifestPin, evidence, `partial-${cumulativeTasks}-${crypto.randomUUID()}`, cumulativeTasks, reason);
    throw new Error(`${reason}; fixed-denominator partial report: ${report.path}`);
  }
}

export interface ReaderBridgeJob {
  schema: "gptgrep.harbor-reader-job.v2";
  instructionSha256: string;
  /** Immutable job bytes cannot embed the later acceptance hash that pins this job. */
  options: Omit<ReaderArmOptions, "admission">;
  profileId: string;
  readinessPath: string;
  answerPath: string;
  maxMediaBytes: number;
  maxTrajectoryBytes: number;
  maxArtifactBytes: number;
}
async function readerPreflight(job: ReaderBridgeJob, jobPin: Pin, reference: AdmissionReference, mode: "preflight" | "reader") {
  if (job.schema !== "gptgrep.harbor-reader-job.v2" || "admission" in job.options || !job.profileId || reference.profileId !== job.profileId ||
      job.readinessPath !== join(job.options.outputDir, "preflight-readiness.json") || job.answerPath !== "/app/answer.json" ||
      !integer(job.maxMediaBytes) || !integer(job.maxArtifactBytes) || !integer(job.maxTrajectoryBytes) || job.maxTrajectoryBytes > 128 * 1024 * 1024) throw new Error("Invalid detached Harbor reader bridge job");
  const binding = await requireStorage(job.options.workspace.cwd, job.options.outputDir, job.answerPath, job.readinessPath);
  const guest = binding.container;
  if (!guest || guest.commandId !== mode || guest.admissionClass !== "reader" || canonical(guest.immutableJob) !== canonical(jobPin) ||
      canonical(guest.acceptance) !== canonical(reference.acceptance) || guest.experimentArm !== (job.options.arm === "bare" ? "baseline" : "treatment") ||
      canonical([...guest.modelReadRoots].sort()) !== canonical(job.options.native.readableRoots.filter(path => path !== job.options.workspace.cwd).sort())) throw new Error("Reader requires its actual phase-specific guest command and read-only model-root binding");
  const admission = await loadAdmission(reference, job.options.outputDir);
  assertTaskAdmitted(admission, job.options.task, job.options.arm, job.options.workspace.documents);
  await verifyRequiredPreprocessing(admission, [job.options.task], (job.options.workspace.preparedArtifacts ?? []).map(item => ({
    relativePath: item.relativePath.startsWith("prepared/") ? item.relativePath.slice("prepared/".length) : item.relativePath, sha256: item.sha256,
  })));
  for (const name of ["GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID", "CODEX_HOME", "CODEX_SQLITE_HOME"]) if (!process.env[name]) throw new Error(`Missing owner auth environment: ${name}`);
  await verifyPin(job.options.runtime.buildReceipt);
  if (job.options.helperModel !== "openai/gpt-6-luna-decisions") throw new Error("Initial controller helper selection changed");
  if (job.options.native.retainedChildCapture !== true || !job.options.native.observationLimits || !job.options.native.maxTraceBytes ||
      job.options.native.mediaCapture?.maxBytes !== job.maxMediaBytes || job.options.native.mediaCapture.outputDir !== join(job.options.outputDir, "media")) throw new Error("Harbor reader requires explicit bounded native/child/media capture");
  if (job.maxMediaBytes > 64 * 1024 * 1024 || job.options.native.maxTraceBytes > 64 * 1024 * 1024 ||
      job.maxArtifactBytes < job.options.native.maxTraceBytes + job.maxTrajectoryBytes + job.maxMediaBytes + 20 * 1024 * 1024) throw new Error("Harbor reader omits original artifacts or answer handoff copies from its reservation");
  return { schema: "gptgrep.harbor-native-readiness.v2", status: "ready", jobSha256: jobPin.sha256, taskId: job.options.task.id, arm: job.options.arm,
    model: "gpt-6.1-sol", effort: job.options.native.effort, serviceTier: job.options.native.serviceTier, commandId: mode,
    commandBinding: { path: binding.binding, sha256: binding.sha256 }, storageBindingSha256: binding.sha256,
    containerBindingSha256: guest.containerBindingSha256, ownerTemplateSha256: guest.ownerTemplateSha256,
    parentHostBindingSha256: guest.parentHostBindingSha256, acceptance: reference.acceptance, authRunId: process.env.GPTGREP_AUTH_RUN_ID };
}
export async function runHarborReaderJob(jobPin: Pin, acceptance: Pin, preflightOnly = false) {
  await verifyPin(jobPin); const job = await readJson<ReaderBridgeJob>(jobPin.path), reference = { acceptance, profileId: job.profileId };
  const ready = await readerPreflight(job, jobPin, reference, preflightOnly ? "preflight" : "reader");
  if (preflightOnly) { await writeJsonExclusive(job.readinessPath, ready, 64 * 1024); return { status: "ready", readinessPath: job.readinessPath }; }
  const prior = await readJson(job.readinessPath);
  await verifyPin(prior.commandBinding);
  const priorBinding = await readJson(prior.commandBinding.path);
  if (prior.schema !== ready.schema || prior.status !== "ready" || prior.commandId !== "preflight" || prior.jobSha256 !== ready.jobSha256 ||
      prior.taskId !== ready.taskId || prior.arm !== ready.arm || prior.containerBindingSha256 !== ready.containerBindingSha256 ||
      prior.ownerTemplateSha256 !== ready.ownerTemplateSha256 || prior.parentHostBindingSha256 !== ready.parentHostBindingSha256 ||
      prior.authRunId !== ready.authRunId || priorBinding.command_id !== "preflight" ||
      priorBinding.container_binding.sha256 !== ready.containerBindingSha256 || priorBinding.immutable_job.sha256 !== jobPin.sha256) throw new Error("Reader lacks the exact preceding guest preflight artifact");
  const root = job.options.outputDir;
  let attempt: ReaderArmResult;
  try { attempt = await runReaderArm({ ...job.options, admission: reference }); }
  catch (error) {
    if (error instanceof StartedReaderArtifactError) throw error;
    const now = new Date().toISOString(), options = job.options;
    attempt = { schema: "gptgrep.reader-attempt.v1", arm: options.arm, taskId: options.task.id, executionMode: "live", order: options.order,
      startedAt: now, completedAt: now, validStart: false, status: "setup_failed", native: null, answerPath: null, answerSha256: null,
      error: error instanceof Error ? error.message : String(error), authRunId: process.env.GPTGREP_AUTH_RUN_ID!, storageBindingSha256: ready.storageBindingSha256,
      corpusSha256: null, contextSha256: null, productRevision: options.runtime.sourceRevision, buildReceiptSha256: options.runtime.buildReceipt.sha256,
      preparationSha256: preparationDigest(options.workspace), readerTaskSha256: digest(options.task), commonPromptSha256: digest(readerPrompt(options.task, options.helperModel)),
      runtimeManifestSha256: options.runtime.manifestSha256, binarySha256: options.runtime.binarySha256 };
    await writeJsonExclusive(join(root, "attempt.json"), attempt, 4 * 1024 * 1024);
  }
  const artifacts: Array<{ role: string; path: string; sha256: string; bytes: number }> = [];
  const add = async (role: string, path: string) => { if (artifacts.some(value => value.path === path)) return; const info = await stat(path); artifacts.push({ role, path, sha256: await fileHash(path), bytes: info.size }); };
  await add("attempt", join(root, "attempt.json"));
  await add("preflight", job.readinessPath);
  for (const [role, name] of [["start", "attempt-start.json"], ["denial", "prelaunch-denial.json"]]) if (await exists(join(root, name!))) await add(role!, join(root, name!));
  if (attempt.native) {
    const trace = { path: attempt.native.tracePath, sha256: await fileHash(attempt.native.tracePath) };
    await add("trace", trace.path);
    if (attempt.native.threadId && attempt.native.turnId) {
      const media = attempt.native.mediaBindings ?? (await collectNativeMedia({ trace, roots: [job.options.workspace.cwd], outputDir: join(root, "media"), maxBytes: job.maxMediaBytes })).bindings;
      await writeNativeTrajectory({ trace, result: attempt.native, media, outputPath: join(root, "trajectory.json"), coveragePath: join(root, "coverage.json"), maxBytes: job.maxTrajectoryBytes });
      await add("trajectory", join(root, "trajectory.json")); await add("coverage", join(root, "coverage.json"));
      for (const binding of media) await add("media", binding.artifact.path);
    }
  }
  if (attempt.answerPath) { await copyFile(attempt.answerPath, job.answerPath, constants.COPYFILE_EXCL); if (await fileHash(job.answerPath) !== attempt.answerSha256) throw new Error("Harbor answer transfer changed bytes"); await add("answer", attempt.answerPath); }
  const totalBytes = artifacts.reduce((sum, artifact) => sum + artifact.bytes, 0);
  const manifest = { schema: "gptgrep.harbor-native-artifacts.v1", executionMode: "live", readerJobSha256: jobPin.sha256,
    taskId: attempt.taskId, arm: attempt.arm, instructionSha256: job.instructionSha256, artifacts, totalBytes };
  const manifestBytes = Buffer.byteLength(`${JSON.stringify(manifest, null, 2)}\n`), answerCopyBytes = attempt.answerPath ? (await stat(job.answerPath)).size : 0;
  if (totalBytes + manifestBytes + answerCopyBytes > job.maxArtifactBytes) throw new Error("Harbor original artifacts and answer handoff copy exceed the frozen bound");
  await writeJsonExclusive(join(root, "bridge-manifest.json"), manifest, 1024 * 1024);
  return { status: attempt.status, taskId: attempt.taskId, arm: attempt.arm, artifactsPath: join(root, "bridge-manifest.json") };
}

if (import.meta.main) {
  const args = process.argv.slice(2), flags = new Map<string, string>();
  if (args.length % 2) throw new Error("Expected unique --flag value arguments");
  for (let i = 0; i < args.length; i += 2) { if (!args[i]!.startsWith("--") || flags.has(args[i]!)) throw new Error("Invalid/duplicate controller flag"); flags.set(args[i]!, args[i + 1]!); }
  const path = flags.get("--job"), sha256 = flags.get("--expect-sha256"), mode = flags.get("--mode");
  if (!path || !sha256 || !mode) throw new Error("Controller requires --mode, --job and --expect-sha256");
  const allowed = new Set(["--mode", "--job", "--expect-sha256", ...(mode === "reader" || mode === "preflight" ? ["--acceptance", "--acceptance-sha256"] :
    mode === "batch" ? ["--review", "--review-sha256", "--cumulative-tasks"] : [])]);
  if ([...flags.keys()].some(flag => !allowed.has(flag))) throw new Error("Unknown controller flag for selected mode");
  const pin = { path, sha256 }; await verifyPin(pin);
  let result: unknown;
  if (mode === "preflight" || mode === "reader") {
    const path = flags.get("--acceptance"), sha256 = flags.get("--acceptance-sha256");
    if (!path || !sha256) throw new Error("Reader requires detached exact project acceptance");
    result = await runHarborReaderJob(pin, { path, sha256 }, mode === "preflight");
  }
  else if (mode === "judge") {
    const job = await readJson<{ schema: string; options: Omit<JudgeOptions, "readerBinding">; bindingPath: string; maxMediaBytes: number; maxTrajectoryBytes: number }>(path);
    if (job.schema !== "gptgrep.controller-judge-job.v1") throw new Error("Invalid independent judge job");
    if ((await requireStorage(job.options.outputPath)).container) throw new Error("The current container owner contract does not admit a judge phase");
    if (!integer(job.maxMediaBytes) || job.options.native.mediaCapture?.maxBytes !== job.maxMediaBytes) throw new Error("Independent judge media allowance is not frozen");
    const readerBinding = { path: job.bindingPath, sha256: await fileHash(job.bindingPath) };
    const judgment = await judgeAnswer({ ...job.options, readerBinding }, { job: pin });
    if (judgment.judge_run?.threadId && judgment.judge_run.turnId) {
      const trace = { path: judgment.judge_run.tracePath, sha256: await fileHash(judgment.judge_run.tracePath) };
      const media = judgment.judge_run.mediaBindings ?? (await collectNativeMedia({ trace, roots: [job.options.workspace], outputDir: `${job.options.outputPath}.media`, maxBytes: job.maxMediaBytes })).bindings;
      await writeNativeTrajectory({ trace, result: judgment.judge_run, media, outputPath: `${job.options.outputPath}.trajectory.json`, coveragePath: `${job.options.outputPath}.coverage.json`, maxBytes: job.maxTrajectoryBytes });
    }
    result = { taskId: judgment.task_id, status: judgment.status, outputPath: job.options.outputPath };
  } else if (mode === "harbor") {
    result = await runHarborOwnerWrapper(pin);
  } else if (mode === "batch") {
    const path = flags.get("--review"), sha256 = flags.get("--review-sha256");
    if (!path || !sha256) throw new Error("Batch dispatch requires the exact independent review pin");
    result = await runControllerBatch(pin, Number(flags.get("--cumulative-tasks")), { path, sha256 });
  }
  else throw new Error("Unknown controller mode");
  process.stdout.write(`${JSON.stringify(result)}\n`);
}
