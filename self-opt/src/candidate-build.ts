import { inspectLinuxBinary } from "./actor-guest-contracts";
export { inspectLinuxBinary } from "./actor-guest-contracts";
/** Deterministic candidate builds/checks consume their own finite campaign action. */
import { lstat, mkdir, open } from "node:fs/promises";
import { join } from "node:path";
import { canonical, digest, fileHash } from "../../benchmark/src/io";
import { executeOwnerCommand, type ProcessObservation } from "../../benchmark/src/process";
import { campaignActionPath, verifyCampaignAction } from "../../benchmark/src/campaign";
import type { Pin } from "./competition";
import type { EngineerReceipt } from "./native-engineer";
import { closed } from "./proposals";
import { immutable, observeActorStorage, pinned, verifyActorGate, verifyWorktree,
  type ActorLaunch, type ActorPolicy, type CandidateBuildRoute } from "./actor-contracts";

export interface CandidateBuildReceipt {
  schema: "gptgrep.opt-candidate-build.v1";
  launch: Pin; engineer: Pin; candidateId: string; parentRevision: string; candidateRevision: string;
  status: "passed" | "failed" | "unknown"; error: string | null;
  product: Pin | null; binary: Pin | null; runtimeManifest: Pin | null;
  checks: Array<{ argv: string[]; exitCode: number | null; terminal: Pin }>;
  buildTerminal: Pin | null; declaredPeakBytes: number;
  storageBefore: unknown; storageAfter: unknown; actualPeakBytes: null;
  sourceRuntimeRevision: string; releaseAccepted: false;
}
export function buildDescriptor(route: CandidateBuildRoute, engineer: Pin) {
  return { kind: "candidate_build_and_checks", route, engineer };
}
export async function verifyCandidateBuild(policy: ActorPolicy, pin: Pin): Promise<CandidateBuildReceipt> {
  const value = await pinned<CandidateBuildReceipt>(pin), route = policy.builds.find(b => b.candidateId === value.candidateId);
  closed(value, ["schema", "launch", "engineer", "candidateId", "parentRevision", "candidateRevision", "status", "error", "product", "binary", "runtimeManifest", "checks", "buildTerminal", "declaredPeakBytes", "storageBefore", "storageAfter", "actualPeakBytes", "sourceRuntimeRevision", "releaseAccepted"], "candidate build receipt");
  if (!route || value.schema !== "gptgrep.opt-candidate-build.v1" || value.status !== "passed" || value.error !== null || value.parentRevision !== policy.parentRevision ||
      value.sourceRuntimeRevision !== policy.runtime.revision || value.declaredPeakBytes !== route.peakBytes || !value.product || !value.binary || !value.runtimeManifest || !value.buildTerminal ||
      value.releaseAccepted !== false || value.checks.length !== route.checks.length || value.actualPeakBytes !== null) throw new Error("Actual complete candidate build/check evidence is unavailable");
  const launch = await pinned<ActorLaunch>(value.launch), engineer = await pinned<EngineerReceipt>(value.engineer);
  if (launch.purpose !== "prepare_context" || launch.candidateId !== value.candidateId || canonical(launch.preparedJob) !== canonical(value.engineer) ||
      engineer.schema !== "gptgrep.opt-engineer-result.v1" || engineer.executionMode !== "live" || engineer.status !== "candidate_source_ready" ||
      engineer.candidateRevision !== value.candidateRevision || engineer.candidateId !== value.candidateId) throw new Error("Build is not bound to the exact native engineering source");
  const { row: reservation } = await verifyCampaignAction(policy.campaign, { experimentId: policy.competitionId,
    actionId: `build-${value.candidateId}`, purpose: "prepare_context", commandSha256: digest(buildDescriptor(route, value.engineer)) });
  if (reservation.purpose !== "prepare_context" || reservation.actionId !== `build-${value.candidateId}` ||
      canonical(reservation.envelope) !== canonical(policy.campaign) || reservation.commandSha256 !== digest(buildDescriptor(route, value.engineer))) throw new Error("Candidate build lacks its own shared campaign charge");
  const terminals = [value.buildTerminal, ...value.checks.map(c => c.terminal)];
  for (const [index, terminal] of terminals.entries()) {
    const observed = await pinned(terminal), command = index === 0 ? route.build : route.checks[index - 1]!;
    if (observed.schema !== "gptgrep.opt-build-command.v1" || canonical(observed.command) !== canonical(command) || canonical(observed.environment) !== canonical(route.environment) ||
        observed.candidateRevision !== value.candidateRevision || !observed.process.started || observed.process.exitCode !== 0 || observed.process.executionError || observed.process.timedOut ||
        observed.process.outputExceeded || !observed.process.cleanup) throw new Error("Candidate build/check lacks a successful exact command terminal");
    if (index && (value.checks[index - 1]!.exitCode !== 0 || canonical(value.checks[index - 1]!.argv) !== canonical([command.executable.path, ...command.argv]))) throw new Error("Candidate check command differs from the frozen route");
  }
  if (canonical(value.binary) !== canonical(await inspectLinuxBinary(route.binaryPath, route.targetArch)) ||
      value.runtimeManifest.path !== route.runtimeManifestPath || value.runtimeManifest.sha256 !== await fileHash(route.runtimeManifestPath)) throw new Error("Candidate artifact or runtime manifest changed");
  const product = await pinned(value.product);
  if (product.schema !== "gptgrep.product-build.v1" || product.sourceRevision !== value.candidateRevision || product.binarySha256 !== value.binary.sha256 ||
      product.runtimeManifestSha256 !== value.runtimeManifest.sha256 || product.platform !== "linux" || product.architecture !== route.targetArch ||
      canonical(product.buildTerminal) !== canonical(value.buildTerminal)) throw new Error("Product build receipt differs from the measured Linux source artifact");
  return value;
}
export async function runCandidateBuild(launchPin: Pin): Promise<Pin> {
  const launch = await pinned<ActorLaunch>(launchPin);
  if (launch.schema !== "gptgrep.opt-actor-launch.v1" || launch.purpose !== "prepare_context" || !launch.preparedJob) throw new Error("A separately charged candidate build launch is required");
  const { policy, competition, campaign } = await verifyActorGate(launch.competition, launch.policy, launch.freezeReview);
  const route = policy.builds.find(b => b.candidateId === launch.candidateId), engineer = await pinned<EngineerReceipt>(launch.preparedJob);
  if (!route || engineer.schema !== "gptgrep.opt-engineer-result.v1" || engineer.executionMode !== "live" || engineer.status !== "candidate_source_ready" ||
      engineer.candidateId !== route.candidateId || !engineer.candidateRevision || canonical(engineer.policy) !== canonical(launch.policy) ||
      launch.actionId !== `build-${route.candidateId}` || launch.reservationPath !== campaignActionPath(campaign, { experimentId: competition.competitionId, actionId: launch.actionId })) throw new Error("Build route differs from the selected native candidate");
  const { row: reservation } = await verifyCampaignAction(policy.campaign, { experimentId: competition.competitionId,
    actionId: launch.actionId, purpose: "prepare_context", commandSha256: digest(buildDescriptor(route, launch.preparedJob)) });
  if (reservation.purpose !== "prepare_context" || reservation.commandSha256 !== digest(buildDescriptor(route, launch.preparedJob)) ||
      reservation.experimentId !== competition.competitionId || reservation.actionId !== launch.actionId || canonical(reservation.envelope) !== canonical(policy.campaign) ||
      Date.parse(launch.expiresAt) > Date.parse(reservation.deadline)) throw new Error("Candidate build has no exact shared campaign charge");
  const expiresAt = Math.min(Date.parse(launch.expiresAt), Date.parse(reservation.deadline));
  if (!Number.isFinite(expiresAt) || Date.now() >= expiresAt) throw new Error("Candidate build deadline expired");
  const worktree = policy.engineers.find(e => e.candidateId === route.candidateId)!.worktree;
  await verifyWorktree(worktree, engineer.candidateRevision);
  const storageBefore = await observeActorStorage(policy.storage);
  for (const [key, value] of Object.entries(route.environment)) if (!["CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "PYTHONDONTWRITEBYTECODE"].includes(key)) await mkdir(value, { recursive: true, mode: 0o700 });
  const checks: CandidateBuildReceipt["checks"] = [];
  let error: string | null = null, product: Pin | null = null, binary: Pin | null = null, runtimeManifest: Pin | null = null, buildTerminal: Pin | null = null;
  let failureStatus: "failed" | "unknown" = "failed";
  try {
    for (const [index, command] of [route.build, ...route.checks].entries()) {
      await verifyWorktree(worktree, engineer.candidateRevision);
      const remaining = expiresAt - Date.now();
      if (command.timeoutMs + 2000 > remaining) throw new Error("Frozen build/check cannot fit the remaining action deadline");
      const process: ProcessObservation = await executeOwnerCommand(command, remaining, { environment: route.environment });
      if (!process.cleanup) failureStatus = "unknown";
      const terminal = await immutable(join(route.outputDir, `command-${index}.json`), { schema: "gptgrep.opt-build-command.v1", command,
        environment: route.environment, candidateRevision: engineer.candidateRevision, process });
      if (index === 0) buildTerminal = terminal;
      else checks.push({ argv: [command.executable.path, ...command.argv], exitCode: process.exitCode, terminal });
      if (!process.started || process.exitCode !== 0 || process.executionError || process.timedOut || process.outputExceeded || !process.cleanup) throw new Error(`Candidate build/check command ${index} failed or has unresolved cleanup`);
      await observeActorStorage(policy.storage);
    }
    await verifyWorktree(worktree, engineer.candidateRevision);
    binary = await inspectLinuxBinary(route.binaryPath, route.targetArch);
    runtimeManifest = { path: route.runtimeManifestPath, sha256: await fileHash(route.runtimeManifestPath) };
    product = await immutable(join(route.outputDir, "product-build.json"), { schema: "gptgrep.product-build.v1", sourceRevision: engineer.candidateRevision,
      binarySha256: binary.sha256, runtimeManifestSha256: runtimeManifest.sha256, platform: "linux", architecture: route.targetArch, buildTerminal });
  } catch (caught) { error = caught instanceof Error ? caught.message : String(caught); }
  return immutable(join(route.outputDir, "candidate-build.json"), { schema: "gptgrep.opt-candidate-build.v1", launch: launchPin, engineer: launch.preparedJob,
    candidateId: route.candidateId, parentRevision: policy.parentRevision, candidateRevision: engineer.candidateRevision,
    status: error ? failureStatus : "passed", error, product, binary, runtimeManifest, checks, buildTerminal, declaredPeakBytes: route.peakBytes,
    storageBefore, storageAfter: await observeActorStorage(policy.storage), actualPeakBytes: null, sourceRuntimeRevision: policy.runtime.revision, releaseAccepted: false } satisfies CandidateBuildReceipt);
}
