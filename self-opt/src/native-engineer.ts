import { OUTPUT_SCHEMA, validateEngineerOutput, type EngineerOutput } from "./actor-guest-contracts";
export { OUTPUT_SCHEMA, validateEngineerOutput, type EngineerOutput } from "./actor-guest-contracts";
/** Actual native engineering producer. No alternate client, synthetic revision or retry path. */
import { lstat, readFile } from "node:fs/promises";
import { join } from "node:path";
import { canonical, digest, fileHash, sha256 } from "../../benchmark/src/io";
import { campaignActionPath, verifyCampaignAction } from "../../benchmark/src/campaign";
import { runNative, type NativeRunOptions, type NativeRunResult } from "../../benchmark/src/native";
import type { ProcessCommand } from "../../benchmark/src/process";
import { boundedText, closed, identity, revision, validateHypothesis, type Hypothesis } from "./proposals";
import { validateCodexCapture } from "./native-receipts";
import type { Pin } from "./competition";
import { actorFeedback, immutable, observeActorStorage, pinned, verifyActorGate, verifyWorktree,
  type ActorLaunch, type ActorPolicy, type EngineerRoute } from "./actor-contracts";

export interface EngineerReceipt {
  schema: "gptgrep.opt-engineer-result.v1";
  executionMode: "live"; launch: Pin; policy: Pin; candidateId: string;
  parentRevision: string; candidateRevision: string | null; diffSha256: string | null;
  changedPaths: string[]; hypothesis: Hypothesis | null;
  status: "candidate_source_ready" | "failed"; error: string | null;
  native: Pin; trace: Pin; promptSha256: string;
  source: { producer: Pin; nativeCapture: Pin; runtimeRevision: string };
  storageBefore: unknown; storageAfter: unknown;
  process: { pid: number; parentPid: number; startedAt: string; completedAt: string };
  buildAcceptance: false; reviewAcceptance: false; releaseAcceptance: false;
}
export function compileEngineerCommand(policy: ActorPolicy, route: EngineerRoute, launch: Pin): ProcessCommand {
  const payload = [route.bun.path, route.cli.path, "--launch", launch.path, "--expect-sha256", launch.sha256];
  return { executable: route.auth.python, inputs: [route.auth.supervisor, route.auth.config, route.bun, route.binary, route.cli,
    route.producer, route.nativeCapture, route.instructions, launch], cwd: policy.outputRoot,
    argv: ["-B", route.auth.supervisor.path, "--config", route.auth.config.path, "run", "--role", route.auth.role,
      "--run-id", route.auth.runId, "--lease", "exclusive", "--deadline-seconds", String((route.outerTimeoutMs - 4000) / 1000),
      "--wait-seconds", "0", "--pre-refresh", "never", "--live-request-attempted", "yes", "--", ...payload],
    timeoutMs: route.outerTimeoutMs - 2000, maxOutputBytes: 1024 * 1024 };
}
export async function engineeringPrompt(policy: ActorPolicy, route: EngineerRoute): Promise<string> {
  const instructions = await readFile(route.instructions.path, "utf8"), feedback = await actorFeedback(policy);
  if (Buffer.byteLength(instructions) > 16384) throw new Error("Frozen engineering instructions exceed 16 KiB");
  const projection = feedback.map(({ id, category, candidateRevision, observation }) => ({ id, category, candidateRevision, observation }));
  const prompt = [instructions,
    "Create one substantive product or generic harness improvement from the frozen parent in this independent worktree. Commit only the exact permitted files. Do not access evaluation gold, private judge objects, frozen feedback, another candidate, credentials or host controls. Do not start a nested agent, provider, release, benchmark or long-running build. Required builds and independent review run after your commit. Return the requested structured hypothesis and the actual clean candidate commit. No automatic retry or second candidate is authorized.",
    JSON.stringify({ candidateId: route.candidateId, parentRevision: policy.parentRevision, scenarioFingerprint: policy.scenarioFingerprint,
      mutationPaths: policy.mutationPaths, developmentFeedback: projection, compositionAllowed: false })].join("\n\n");
  if (Buffer.byteLength(prompt) > 192 * 1024) throw new Error("Engineering input exceeds its finite bound");
  return prompt;
}
export function engineerOptions(route: EngineerRoute, input: string): NativeRunOptions {
  return { codexBinary: route.binary.path, cwd: route.worktree.path, model: route.model, effort: route.effort, serviceTier: route.serviceTier,
    timeoutMs: route.timeoutMs, tracePath: join(route.outputDir, "native-trace.jsonl"), readableRoots: route.readableRoots,
    runtimeDirectory: join(route.worktree.path, ".local", "native-runtime"), permissionProfile: route.permissionProfile,
    input, outputSchema: OUTPUT_SCHEMA, observationLimits: route.observationLimits, retainedChildCapture: false, maxTraceBytes: route.maxTraceBytes,
    config: { "agents.max_threads": 1, "agents.max_concurrent_threads_per_session": 1, "agents.max_depth": 0 } };
}
export async function validateEngineerCapture(native: NativeRunResult, route: EngineerRoute, inputSha256: string): Promise<void> {
  const info = await lstat(native.tracePath);
  if (!info.isFile() || info.size > route.maxTraceBytes) throw new Error("Native engineering capture exceeds its bound");
  const lines = (await readFile(native.tracePath, "utf8")).split("\n").filter(Boolean);
  if (lines.length > 100000) throw new Error("Native engineering event count exceeds its bound");
  validateCodexCapture(native, lines.map(line => JSON.parse(line)), { client: "codex", binary: route.binary, producer: route.nativeCapture,
    model: route.model, effort: route.effort, serviceTier: route.serviceTier, cwd: route.worktree.path, inputSha256 });
  if (native.permissionEvidence?.profileId !== route.permissionProfile || native.permissionEvidence.extends !== ":workspace" ||
      native.observations?.exceeded !== null || native.observations?.childThreads !== 0 || native.observations?.retryEvents !== 0 ||
      native.workerModels.length || (native.childSnapshots?.length ?? 0)) throw new Error("Engineering did not retain its writable profile and bounded independent worker scope");
}
export async function runNativeEngineer(launchPin: Pin): Promise<Pin> {
  const startedAt = new Date().toISOString(), launch = await pinned<ActorLaunch>(launchPin);
  closed(launch, ["schema", "policy", "competition", "freezeReview", "candidateId", "purpose", "actionId", "reservationPath", "preparedJob", "createdAt", "expiresAt"], "native actor launch");
  if (launch.schema !== "gptgrep.opt-actor-launch.v1" || launch.purpose !== "native_engineer" || launch.preparedJob !== null) throw new Error("Exact native engineer launch required");
  identity(launch.candidateId, "candidate ID");
  const { competition, policy, campaign } = await verifyActorGate(launch.competition, launch.policy, launch.freezeReview);
  const route = policy.engineers.find(e => e.candidateId === launch.candidateId);
  if (!route || launch.actionId !== `engineer-${route.candidateId}` || launch.reservationPath !== campaignActionPath(campaign, { experimentId: competition.competitionId, actionId: launch.actionId })) throw new Error("Native engineer is outside its frozen charged slot");
  const command = compileEngineerCommand(policy, route, launchPin);
  const { row: reservation } = await verifyCampaignAction(policy.campaign, { experimentId: competition.competitionId,
    actionId: launch.actionId, purpose: "native_engineer", commandSha256: digest(command) });
  if (reservation.schema !== "gptgrep.live-reservation.v2" || reservation.purpose !== "native_engineer" || reservation.actionId !== launch.actionId ||
      reservation.experimentId !== competition.competitionId || canonical(reservation.envelope) !== canonical(policy.campaign) || reservation.commandSha256 !== digest(command) ||
      reservation.disposition !== "charged_before_dispatch" || !Number.isFinite(Date.parse(launch.expiresAt)) ||
      Date.parse(launch.expiresAt) > Date.parse(reservation.deadline) ||
      Date.now() + route.timeoutMs + 2000 > Math.min(Date.parse(launch.expiresAt), Date.parse(reservation.deadline))) throw new Error("Native turn lacks the exact unexpired shared campaign charge");
  if (process.env.GPTGREP_AUTH_RUN_ID !== route.auth.runId || process.env.GPTGREP_AUTH_SOURCE_ID !== route.auth.sourceId) throw new Error("Engineer is not inside its frozen native auth supervisor");
  const storageBefore = await observeActorStorage(policy.storage);
  await verifyWorktree(route.worktree, policy.parentRevision);
  const input = await engineeringPrompt(policy, route), promptSha256 = sha256(input);
  const native = await runNative(engineerOptions(route, input));
  const nativePin = await immutable(join(route.outputDir, "native-result.json"), native, route.maxResultBytes);
  const trace = { path: native.tracePath, sha256: await fileHash(native.tracePath) };
  let output: EngineerOutput | null = null, error: string | null = null, diffSha256: string | null = null, changedPaths: string[] = [];
  try {
    await validateEngineerCapture(native, route, promptSha256);
    output = JSON.parse(native.answerText); validateEngineerOutput(output!, policy);
    const { git } = await verifyWorktree(route.worktree, output!.candidateRevision);
    await git("merge-base", "--is-ancestor", policy.parentRevision, output!.candidateRevision);
    changedPaths = (await git("diff", "--name-only", "-z", "--no-renames", policy.parentRevision, output!.candidateRevision, "--")).split("\0").filter(Boolean).sort();
    if (canonical(changedPaths) !== canonical([...output!.hypothesis.mutationPaths].sort()) || !changedPaths.length) throw new Error("Actual commit diff differs from the native hypothesis");
    for (const path of changedPaths) {
      const entry = await git("ls-tree", output!.candidateRevision, "--", path);
      if (entry && !/^(100644|100755) blob /.test(entry)) throw new Error("Candidate introduces a non-regular source object");
    }
    const diff = await git("diff", "--binary", "--no-ext-diff", "--no-renames", policy.parentRevision, output!.candidateRevision, "--");
    if (Buffer.byteLength(diff) > 4 * 1024 * 1024) throw new Error("Candidate diff exceeds the retained source bound");
    diffSha256 = digest(diff);
    const feedback = await actorFeedback(policy);
    if (output!.hypothesis.feedbackIds.some(id => !feedback.some(f => f.id === id))) throw new Error("Native hypothesis cites unprovided development feedback");
  } catch (caught) { error = caught instanceof Error ? caught.message : String(caught); }
  const receipt: EngineerReceipt = { schema: "gptgrep.opt-engineer-result.v1", executionMode: "live", launch: launchPin, policy: launch.policy,
    candidateId: route.candidateId, parentRevision: policy.parentRevision, candidateRevision: output?.candidateRevision ?? null, diffSha256, changedPaths,
    hypothesis: output?.hypothesis ?? null, status: error ? "failed" : "candidate_source_ready", error, native: nativePin, trace, promptSha256,
    source: { producer: route.producer, nativeCapture: route.nativeCapture, runtimeRevision: policy.runtime.revision }, storageBefore,
    storageAfter: await observeActorStorage(policy.storage), process: { pid: process.pid, parentPid: process.ppid, startedAt, completedAt: new Date().toISOString() },
    buildAcceptance: false, reviewAcceptance: false, releaseAcceptance: false };
  return immutable(join(route.outputDir, "engineer-result.json"), receipt);
}
