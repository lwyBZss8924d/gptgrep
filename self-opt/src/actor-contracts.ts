import { immutable, validateBuildEnvironment } from "./actor-guest-contracts";
export { immutable, validateBuildEnvironment } from "./actor-guest-contracts";
/** Frozen native actor routes. These records configure work; only the runtime gate can launch it. */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { lstat, mkdir, open, readFile, realpath, statfs } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { canonical, digest, fileHash, physicalPath, readJson, requireStorage, within } from "../../benchmark/src/io";
import { validateCampaignEnvelope, type CampaignEnvelopeV2 } from "../../benchmark/src/campaign";
import type { NativeRunOptions } from "../../benchmark/src/native";
import type { ProcessCommand } from "../../benchmark/src/process";
import { verifyC3Gate } from "./competition-dispatch";
import { validateMutationPaths } from "./evaluate";
import { boundedText, closed, hash, identity, revision, validateDevFeedback, type DevFeedback } from "./proposals";
import type { CompetitionContract, Pin } from "./competition";
import { verifyReceiptPin } from "./native-receipts";
import { AGY_STREAM_PRODUCER, AGY_STREAM_SOURCES } from "./agy-stream";

const exec = promisify(execFile);
export const ENGINEER_PRODUCER = fileURLToPath(new URL("./native-engineer.ts", import.meta.url));
export const ENGINEER_CLI = fileURLToPath(new URL("./native-engineer-cli.ts", import.meta.url));
export const NATIVE_CAPTURE = fileURLToPath(new URL("../../benchmark/src/native.ts", import.meta.url));
export interface ActorStorage {
  root: string; mountPoint: string; volumeUuid: string; device: number;
  /** These are runtime observations, not an owner storage-plan approval. */
  minimumFreeBytes: number;
}
export interface PhysicalWorktree { path: string; device: number; inode: number; gitDirectory: string }
export interface EngineerRoute {
  candidateId: string; worktree: PhysicalWorktree; instructions: Pin;
  binary: Pin; producer: Pin; cli: Pin; nativeCapture: Pin; bun: Pin;
  model: string; effort: NativeRunOptions["effort"]; serviceTier: string; permissionProfile: string;
  readableRoots: string[]; timeoutMs: number; outerTimeoutMs: number; outputDir: string;
  maxTraceBytes: number; maxResultBytes: number; maxEvidenceBytes: number;
  observationLimits: { toolCalls: number; childThreads: 0; retryEvents: 0 };
  auth: { python: Pin; supervisor: Pin; config: Pin; role: string; runId: string; sourceId: string; receiptPath: string };
}
export interface AgyRoute {
  binary: Pin; python: Pin; composer: Pin; composerSources: Pin[]; resultSchema: Pin;
  model: string; effort: string; permissionPolicy: "preserve" | "skip-permissions"; authorization: string;
  instructions: Pin; rootInstructions: Pin[]; outputRoot: string; nativeHome: string;
  timeoutSeconds: number; outerTimeoutMs: number; maxOutputBytes: number; maxSources: number;
}
export interface AgyStreamRoute {
  schema: "gptgrep.opt-agy-stream-route.v1";
  binary: Pin; bun: Pin; bunConfig: Pin; producer: Pin; producerSources: Pin[]; resultSchema: Pin;
  model: string; effort: string; permissionPolicy: "preserve"; authorization: string;
  instructions: Pin; rootInstructions: Pin[]; outputRoot: string; nativeHome: string;
  timeoutSeconds: number; outerTimeoutMs: number; maxOutputBytes: number; maxSources: number;
  maxPacketBytes: number; maxInputBytes: number; maxTraceBytes: number;
}
export function isAgyStreamRoute(value: AgyRoute | AgyStreamRoute): value is AgyStreamRoute {
  return "schema" in value && value.schema === "gptgrep.opt-agy-stream-route.v1";
}
export interface CandidateBuildRoute {
  candidateId: string; outputDir: string;
  build: ProcessCommand; checks: ProcessCommand[];
  environment: Record<string, string>; deadlineMs: number; peakBytes: number;
  binaryPath: string; runtimeManifestPath: string; targetArch: "x64" | "arm64";
}
export interface ActorPolicy {
  schema: "gptgrep.opt-actor-policy.v1";
  competitionId: string; parentRevision: string; scenarioFingerprint: string;
  runtime: { worktree: string; revision: string };
  campaign: Pin; baseline: { manifest: Pin; report: Pin; review: Pin };
  storage: ActorStorage; outputRoot: string; mutationPaths: string[];
  /** The complete pre-reviewed development projection; raw judge material is not an input. */
  feedback: Pin; feedbackReview: Pin;
  engineers: EngineerRoute[]; builds: CandidateBuildRoute[]; reviewer: AgyRoute | AgyStreamRoute;
}
export interface ActorLaunch {
  schema: "gptgrep.opt-actor-launch.v1";
  policy: Pin; competition: Pin; freezeReview: Pin;
  candidateId: string; purpose: "native_engineer" | "agy" | "prepare_context";
  actionId: string; reservationPath: string; preparedJob: Pin | null;
  createdAt: string; expiresAt: string;
}
const integer = (value: unknown, low = 1, high = Number.MAX_SAFE_INTEGER): value is number => Number.isSafeInteger(value) && (value as number) >= low && (value as number) <= high;
export async function pinned<T = any>(pin: Pin): Promise<T> {
  if (basename(pin?.path ?? "") === "auth.json") throw new Error("Credential files cannot be actor evidence");
  await verifyReceiptPin(pin); return readJson<T>(pin.path);
}
export function validateActorPolicy(policy: ActorPolicy, expected: Pick<CompetitionContract, "competitionId" | "parentRevision" | "scenarioFingerprint" | "candidateIds" | "mutationPaths">): void {
  closed(policy, ["schema", "competitionId", "parentRevision", "scenarioFingerprint", "runtime", "campaign", "baseline", "storage", "outputRoot", "mutationPaths", "feedback", "feedbackReview", "engineers", "builds", "reviewer"], "native actor policy");
  if (policy.schema !== "gptgrep.opt-actor-policy.v1" || policy.competitionId !== expected.competitionId || policy.parentRevision !== expected.parentRevision || policy.scenarioFingerprint !== expected.scenarioFingerprint) throw new Error("Actor policy differs from the exact competition");
  identity(policy.competitionId, "competition ID"); revision(policy.parentRevision); hash(policy.scenarioFingerprint, "scenario");
  closed(policy.runtime, ["worktree", "revision"], "frozen actor runtime source"); revision(policy.runtime.revision);
  if (!isAbsolute(policy.runtime.worktree) || !within(policy.runtime.worktree, ENGINEER_PRODUCER)) throw new Error("Actor runtime must identify the actual imported source checkout");
  validateMutationPaths(policy.mutationPaths);
  if (canonical([...policy.mutationPaths].sort()) !== canonical([...expected.mutationPaths].sort())) throw new Error("Actor mutation paths differ from the competition");
  closed(policy.baseline, ["manifest", "report", "review"], "accepted C3 baseline");
  closed(policy.storage, ["root", "mountPoint", "volumeUuid", "device", "minimumFreeBytes"], "local external storage");
  if (!isAbsolute(policy.storage.root) || !isAbsolute(policy.storage.mountPoint) || policy.storage.mountPoint === "/" ||
      !within(policy.storage.mountPoint, policy.storage.root) || !within(policy.storage.root, policy.outputRoot) ||
      !integer(policy.storage.device, 0) || !integer(policy.storage.minimumFreeBytes) || !/^[A-Fa-f0-9-]{36}$/.test(policy.storage.volumeUuid)) throw new Error("Exact external storage identity and free-space floor required");
  if (!Array.isArray(policy.engineers) || policy.engineers.length !== 2 || canonical(policy.engineers.map(e => e.candidateId).sort()) !== canonical([...expected.candidateIds].sort())) throw new Error("Two exact native engineer slots required");
  const paths = new Set<string>(), gitDirs = new Set<string>(), inodes = new Set<string>(), authRuns = new Set<string>();
  for (const engineer of policy.engineers) {
    closed(engineer, ["candidateId", "worktree", "instructions", "binary", "producer", "cli", "nativeCapture", "bun", "model", "effort", "serviceTier", "permissionProfile", "readableRoots", "timeoutMs", "outerTimeoutMs", "outputDir", "maxTraceBytes", "maxResultBytes", "maxEvidenceBytes", "observationLimits", "auth"], "native engineer route");
    identity(engineer.candidateId, "candidate ID");
    closed(engineer.worktree, ["path", "device", "inode", "gitDirectory"], "independent native worktree");
    const w = engineer.worktree;
    if (!isAbsolute(w.path) || !isAbsolute(w.gitDirectory) || !within(policy.storage.root, w.path) || !integer(w.device, 0) || !integer(w.inode) ||
        paths.has(w.path) || gitDirs.has(w.gitDirectory) || inodes.has(`${w.device}:${w.inode}`) || w.device !== policy.storage.device) throw new Error("Engineer worktrees must be independent physical external worktrees");
    paths.add(w.path); gitDirs.add(w.gitDirectory); inodes.add(`${w.device}:${w.inode}`);
    for (const key of ["model", "effort", "serviceTier", "permissionProfile"] as const) boundedText(engineer[key], key, 256);
    if (engineer.permissionProfile.startsWith(":")) throw new Error("A named writable engineering profile is required");
    if (engineer.producer.path !== ENGINEER_PRODUCER || engineer.cli.path !== ENGINEER_CLI || engineer.nativeCapture.path !== NATIVE_CAPTURE) throw new Error("Engineer must use the actual imported producer and native capture");
    if (!integer(engineer.timeoutMs, 1, 1_800_000) || !integer(engineer.outerTimeoutMs, engineer.timeoutMs + 4000, 2_100_000) ||
        !integer(engineer.maxTraceBytes, 1, 64 * 1024 * 1024) || !integer(engineer.maxResultBytes, 1, 2 * 1024 * 1024) ||
        !integer(engineer.maxEvidenceBytes, engineer.maxTraceBytes + engineer.maxResultBytes, 128 * 1024 * 1024) ||
        !within(policy.outputRoot, engineer.outputDir) || within(w.path, engineer.outputDir)) throw new Error("Engineer deadline or evidence bounds are not finite and independent");
    closed(engineer.observationLimits, ["toolCalls", "childThreads", "retryEvents"], "engineering observation limits");
    if (!integer(engineer.observationLimits.toolCalls, 1, 256) || engineer.observationLimits.childThreads !== 0 || engineer.observationLimits.retryEvents !== 0) throw new Error("Initial engineering slots prohibit nested workers and automatic retries");
    if (!Array.isArray(engineer.readableRoots) || !engineer.readableRoots.includes(w.path) || new Set(engineer.readableRoots).size !== engineer.readableRoots.length ||
        engineer.readableRoots.some(p => !isAbsolute(p) || p === "/" || within(p, policy.outputRoot))) throw new Error("Engineering roots must exclude controller/evaluator output");
    closed(engineer.auth, ["python", "supervisor", "config", "role", "runId", "sourceId", "receiptPath"], "native auth route");
    for (const key of ["role", "runId", "sourceId"] as const) identity(engineer.auth[key], key);
    if (authRuns.has(engineer.auth.runId) || !within(policy.storage.root, engineer.auth.receiptPath)) throw new Error("Native engineer auth runs must be independent and retained externally");
    authRuns.add(engineer.auth.runId);
  }
  for (const engineer of policy.engineers) for (const other of policy.engineers) {
    if (engineer.candidateId !== other.candidateId && (within(engineer.worktree.path, other.worktree.path) ||
        engineer.readableRoots.some(path => within(path, other.worktree.path) || within(other.worktree.path, path)))) throw new Error("Engineer routes cannot expose the other candidate worktree");
  }
  if (!Array.isArray(policy.builds) || policy.builds.length !== 2 || canonical(policy.builds.map(b => b.candidateId).sort()) !== canonical([...expected.candidateIds].sort())) throw new Error("Two separately bounded candidate build/check routes required");
  for (const build of policy.builds) {
    closed(build, ["candidateId", "outputDir", "build", "checks", "environment", "deadlineMs", "peakBytes", "binaryPath", "runtimeManifestPath", "targetArch"], "candidate artifact route");
    const worktree = policy.engineers.find(e => e.candidateId === build.candidateId)!.worktree.path;
    if (!within(policy.outputRoot, build.outputDir) || !within(build.outputDir, build.binaryPath) || !within(worktree, build.runtimeManifestPath) ||
        !["x64", "arm64"].includes(build.targetArch) || !integer(build.deadlineMs, 4001, 7_200_000) || !integer(build.peakBytes) ||
        build.peakBytes > policy.storage.minimumFreeBytes || !Array.isArray(build.checks) || !build.checks.length || build.checks.length > 16) throw new Error("Candidate build lacks an exact artifact, finite peak/deadline or checks");
    for (const command of [build.build, ...build.checks]) {
      closed(command, ["executable", "inputs", "argv", "cwd", "timeoutMs", "maxOutputBytes"], "candidate build/check command");
      if (command.cwd !== worktree || !integer(command.timeoutMs, 1, build.deadlineMs - 2000) || !integer(command.maxOutputBytes, 1, 8 * 1024 * 1024) ||
          !Array.isArray(command.argv) || command.argv.length > 128 || command.argv.some(a => typeof a !== "string" || a.includes("\0")) || !Array.isArray(command.inputs)) throw new Error("Candidate build/check command is outside its frozen route");
    }
    validateBuildEnvironment(build.environment, policy.storage.root, build.outputDir);
  }
  const reviewer = policy.reviewer;
  if (isAgyStreamRoute(reviewer)) {
    closed(reviewer, ["schema", "binary", "bun", "bunConfig", "producer", "producerSources", "resultSchema", "model", "effort", "permissionPolicy", "authorization", "instructions", "rootInstructions", "outputRoot", "nativeHome", "timeoutSeconds", "outerTimeoutMs", "maxOutputBytes", "maxSources", "maxPacketBytes", "maxInputBytes", "maxTraceBytes"], "project AGY stream route");
    for (const key of ["model", "effort", "authorization"] as const) boundedText(reviewer[key], key, 1000);
    if (reviewer.permissionPolicy !== "preserve" || reviewer.producer.path !== AGY_STREAM_PRODUCER ||
        !Array.isArray(reviewer.producerSources) || canonical(reviewer.producerSources.map(pin => pin.path).sort()) !== canonical([...AGY_STREAM_SOURCES].sort()) ||
        !integer(reviewer.timeoutSeconds, 1, 300) || !integer(reviewer.outerTimeoutMs, reviewer.timeoutSeconds * 1000 + 6000, 360000) ||
        !integer(reviewer.maxOutputBytes, 1, 2 * 1024 * 1024) || !integer(reviewer.maxSources, 1, 16) ||
        !integer(reviewer.maxPacketBytes, 1, 8 * 1024 * 1024) || !integer(reviewer.maxInputBytes, reviewer.maxPacketBytes, 16 * 1024 * 1024) ||
        !integer(reviewer.maxTraceBytes, 1, 8 * 1024 * 1024) || !within(policy.outputRoot, reviewer.outputRoot) || !isAbsolute(reviewer.nativeHome) || reviewer.nativeHome === "/" ||
        !Array.isArray(reviewer.rootInstructions) || !reviewer.rootInstructions.length || reviewer.rootInstructions.length > 8) throw new Error("AGY stream route lacks its exact source, original home or finite packet/native bounds");
    return;
  }
  closed(reviewer, ["binary", "python", "composer", "composerSources", "resultSchema", "model", "effort", "permissionPolicy", "authorization", "instructions", "rootInstructions", "outputRoot", "nativeHome", "timeoutSeconds", "outerTimeoutMs", "maxOutputBytes", "maxSources"], "native AGY route");
  for (const key of ["model", "effort", "authorization"] as const) boundedText(reviewer[key], key, 1000);
  if (!["preserve", "skip-permissions"].includes(reviewer.permissionPolicy) || !integer(reviewer.timeoutSeconds, 1, 300) ||
      !integer(reviewer.outerTimeoutMs, reviewer.timeoutSeconds * 1000 + 2000, 360000) || !integer(reviewer.maxOutputBytes, 1, 1048576) ||
      !integer(reviewer.maxSources, 3, 8) || !within(policy.outputRoot, reviewer.outputRoot) || !within(policy.storage.root, reviewer.nativeHome) ||
      !Array.isArray(reviewer.composerSources) || reviewer.composerSources.length < 2 || reviewer.composerSources.length > 8 ||
      !Array.isArray(reviewer.rootInstructions) || !reviewer.rootInstructions.length || reviewer.rootInstructions.length > 8) throw new Error("AGY uses one bounded native route, retained home and campaign root");
}
export async function verifyActorSources(policy: ActorPolicy): Promise<void> {
  await physicalPath(policy.runtime.worktree);
  const git = async (...args: string[]) => (await exec("git", ["-C", policy.runtime.worktree, ...args], { maxBuffer: 4 * 1024 * 1024, timeout: 10000 })).stdout;
  if ((await git("rev-parse", "HEAD")).trim() !== policy.runtime.revision || (await git("status", "--porcelain=v1", "--untracked-files=all")).length) throw new Error("Actor runtime must remain the exact clean frozen source revision");
  for (const route of policy.engineers) {
    for (const executable of [route.binary, route.bun, route.auth.python]) await verifyReceiptPin(executable, true);
    for (const input of [route.producer, route.cli, route.nativeCapture, route.instructions, route.auth.supervisor, route.auth.config]) await verifyReceiptPin(input);
  }
  const r = policy.reviewer;
  if (isAgyStreamRoute(r)) {
    for (const executable of [r.binary, r.bun]) await verifyReceiptPin(executable, true);
    for (const input of [r.bunConfig, r.producer, r.resultSchema, r.instructions, ...r.producerSources, ...r.rootInstructions]) await verifyReceiptPin(input);
    if ((await readFile(r.bunConfig.path)).length !== 0) throw new Error("AGY stream Bun configuration must be pinned and empty");
  } else {
    for (const executable of [r.binary, r.python]) await verifyReceiptPin(executable, true);
    for (const input of [r.composer, r.resultSchema, r.instructions, ...r.composerSources, ...r.rootInstructions]) await verifyReceiptPin(input);
    if (!r.composerSources.some(p => canonical(p) === canonical(r.composer))) throw new Error("AGY Composer source closure omits its entrypoint");
  }
  for (const build of policy.builds) for (const command of [build.build, ...build.checks]) {
    await verifyReceiptPin(command.executable, true); for (const input of command.inputs) await verifyReceiptPin(input);
  }
}
export async function observeActorStorage(storage: ActorStorage) {
  await physicalPath(storage.root); await physicalPath(storage.mountPoint);
  const info = await lstat(storage.root), mount = await lstat(storage.mountPoint);
  if (!info.isDirectory() || info.dev !== storage.device || mount.dev !== storage.device || await realpath(storage.root) !== storage.root) throw new Error("External actor storage moved or changed device");
  if (process.platform !== "darwin") throw new Error("This explicit local actor route requires its macOS external-volume observer");
  const { stdout } = await exec("diskutil", ["info", "-plist", storage.mountPoint], { maxBuffer: 65536, timeout: 10000 });
  const string = (key: string) => new RegExp(`<key>${key}</key>\\s*<string>([^<]+)</string>`).exec(stdout)?.[1];
  if (string("VolumeUUID")?.toUpperCase() !== storage.volumeUuid.toUpperCase() || string("MountPoint") !== storage.mountPoint ||
      !/<key>Internal<\/key>\s*<false\/>/.test(stdout) || !/<key>Writable<\/key>\s*<true\/>/.test(stdout)) throw new Error("Selected volume is not the same writable external volume");
  const fs = await statfs(storage.root), freeBytes = Number(fs.bavail) * Number(fs.bsize);
  if (!Number.isSafeInteger(freeBytes) || freeBytes < storage.minimumFreeBytes) throw new Error("External actor storage lacks its frozen free-space floor");
  return { schema: "gptgrep.local-actor-storage-observation.v1", root: storage.root, mountPoint: storage.mountPoint, volumeUuid: storage.volumeUuid,
    device: info.dev, inode: info.ino, freeBytes, observedAt: new Date().toISOString(), ownerPlanClaim: false };
}
/** A local free-space observation never establishes container custody or actor isolation. */
export function validateActorStorageObservation(value: any, storage: ActorStorage): void {
  if (value?.schema !== "gptgrep.local-actor-storage-observation.v1" || value.ownerPlanClaim !== false || value.root !== storage.root ||
      value.mountPoint !== storage.mountPoint || value.volumeUuid !== storage.volumeUuid || value.device !== storage.device ||
      !Number.isSafeInteger(value.freeBytes) || value.freeBytes < storage.minimumFreeBytes || !Number.isFinite(Date.parse(value.observedAt))) throw new Error("Actual external storage observation is unavailable");
}
export async function verifyWorktree(worktree: PhysicalWorktree, expectedRevision?: string) {
  await physicalPath(worktree.path); const info = await lstat(worktree.path);
  const git = async (...args: string[]) => (await exec("git", ["-C", worktree.path, ...args], { maxBuffer: 4 * 1024 * 1024, timeout: 10000 })).stdout;
  const head = (await git("rev-parse", "HEAD")).trim();
  if (!info.isDirectory() || info.dev !== worktree.device || info.ino !== worktree.inode || (await git("rev-parse", "--absolute-git-dir")).trim() !== worktree.gitDirectory ||
      expectedRevision && head !== expectedRevision || (await git("status", "--porcelain=v1", "--untracked-files=all")).length) throw new Error("Exact clean native worktree identity/revision is unavailable");
  return { revision: head, git };
}
export async function actorFeedback(policy: ActorPolicy): Promise<DevFeedback[]> {
  const value = await pinned(policy.feedback), review = await pinned(policy.feedbackReview);
  closed(value, ["schema", "scenarioFingerprint", "feedback"], "engineering feedback projection");
  if (value.schema !== "gptgrep.opt-engineering-feedback.v1" || value.scenarioFingerprint !== policy.scenarioFingerprint || !Array.isArray(value.feedback) || value.feedback.length > 64 ||
      review.schema !== "gptgrep.opt-feedback-review.v1" || review.status !== "accepted" || canonical(review.projection) !== canonical(policy.feedback)) throw new Error("Exact independently reviewed dev-only projection required");
  value.feedback.forEach((item: DevFeedback) => { validateDevFeedback(item); if (item.scenarioFingerprint !== policy.scenarioFingerprint) throw new Error("Feedback scenario differs"); });
  // Source pointers are provenance only. Do not dereference or expose the private source to the learner.
  return value.feedback;
}
export function validateActorDeadline(purpose: "launch" | "retained_evidence", deadlineAt: number, campaignDeadline: string): void {
  if (!["launch", "retained_evidence"].includes(purpose) || !Number.isSafeInteger(deadlineAt) || deadlineAt <= 0 ||
      !Number.isFinite(Date.parse(campaignDeadline))) throw new Error("Invalid actor validation purpose or original deadline");
  if (purpose === "launch" && Date.now() >= Math.min(deadlineAt, Date.parse(campaignDeadline))) throw new Error("Competition or shared campaign deadline expired");
}
export async function verifyActorGate(competitionPin: Pin, policyPin: Pin, reviewPin: Pin, purpose: "launch" | "retained_evidence" = "launch") {
  if (!["launch", "retained_evidence"].includes(purpose)) throw new Error("Unknown actor validation purpose");
  const competition = await pinned<CompetitionContract>(competitionPin), policy = await pinned<ActorPolicy>(policyPin);
  const { validateCompetition } = await import("./competition"); validateCompetition(competition);
  if (competition.schema !== "gptgrep.opt-competition.v3" || canonical(competition.nativePolicy) !== canonical(policyPin)) throw new Error("Direct-local native actors require the exact version 3 actor policy");
  validateActorPolicy(policy, competition); await verifyActorSources(policy);
  const campaign = await pinned<CampaignEnvelopeV2>(policy.campaign); validateCampaignEnvelope(campaign);
  await verifyC3Gate({ baseline: policy.baseline, campaign: policy.campaign } as any, competition);
  const review = await pinned(reviewPin);
  closed(review, ["schema", "status", "competition", "policy", "campaign", "reviewerSessionId"], "O4 native actor runtime review");
  if (review.schema !== "gptgrep.opt-actor-runtime-review.v1" || review.status !== "accepted_for_live_qualification" ||
      canonical(review.competition) !== canonical(competitionPin) || canonical(review.policy) !== canonical(policyPin) || canonical(review.campaign) !== canonical(policy.campaign)) throw new Error("Exact O4 actor/scenario/budget review required");
  boundedText(review.reviewerSessionId, "independent O4 reviewer", 256);
  validateActorDeadline(purpose, competition.deadlineAt, campaign.campaignDeadline);
  await actorFeedback(policy);
  return { competition, policy, campaign };
}
