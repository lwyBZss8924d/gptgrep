/** One serialized, durable native actor action. Lost handles are reconciled, never retried. */
import { lstat, mkdir, open, unlink } from "node:fs/promises";
import { dirname, join } from "node:path";
import { canonical, digest, fileHash, requireStorage } from "../../benchmark/src/io";
import { campaignActionPath, reserveCampaignAction } from "../../benchmark/src/campaign";
import { executeOwnerCommand, type ProcessObservation } from "../../benchmark/src/process";
import { verifyAuthTerminal } from "../../benchmark/src/controller";
import { closed, identity } from "./proposals";
import { immutable, observeActorStorage, pinned, verifyActorGate, type ActorLaunch, type ActorPolicy } from "./actor-contracts";
import { compileEngineerCommand, type EngineerReceipt } from "./native-engineer";
import { buildDescriptor, runCandidateBuild, type CandidateBuildReceipt } from "./candidate-build";
import { prepareAgy, runNativeAgy, type AgyReceipt, type PreparedAgy } from "./native-agy";
import type { Pin } from "./competition";

export interface ActorAction {
  policy: Pin; competition: Pin; freezeReview: Pin; candidateId: string;
  phase: "engineer" | "build" | "agy";
}
export interface ActorTerminal {
  schema: "gptgrep.opt-actor-terminal.v1"; action: ActorAction; actionId: string; launch: Pin;
  reservation: Pin; status: "completed" | "failed" | "unknown"; error: string | null;
  producer: Pin | null; auth: Pin | null; process: ProcessObservation | null;
  completedAt: string; acceptance: "not_evaluated";
}
export function actorTerminalStatus(status: "passed" | "review_reported" | "failed" | "unknown"): ActorTerminal["status"] {
  return status === "unknown" ? "unknown" : status === "failed" ? "failed" : "completed";
}
/** A charged exception or unknown effect keeps this exact lock for reconciliation. */
export async function withActorLock<T>(lockPath: string, record: unknown,
  work: (charged: () => void) => Promise<{ value: T; status: ActorTerminal["status"] }>): Promise<T> {
  await requireStorage(lockPath); await mkdir(dirname(lockPath), { recursive: true, mode: 0o700 });
  const lock = await open(lockPath, "wx", 0o600); let charged = false, status: ActorTerminal["status"] | null = null;
  try {
    await lock.writeFile(`${JSON.stringify(record)}\n`); await lock.sync();
    const directory = await open(dirname(lockPath), "r"); try { await directory.sync(); } finally { await directory.close(); }
    const result = await work(() => { charged = true; }); status = result.status; return result.value;
  } finally {
    await lock.close();
    if (!charged || status !== null && status !== "unknown") await unlink(lockPath);
  }
}
function paths(policy: ActorPolicy, candidate: string, phase: ActorAction["phase"]) {
  return { actionId: `${phase}-${candidate}`, launch: join(policy.outputRoot, "actions", `${phase}-${candidate}-launch.json`),
    terminal: join(policy.outputRoot, "actions", `${phase}-${candidate}-terminal.json`) };
}
async function previous(policy: ActorPolicy, action: ActorAction, phase: ActorAction["phase"]): Promise<ActorTerminal> {
  const path = paths(policy, action.candidateId, phase).terminal, terminal = await pinned<ActorTerminal>({ path, sha256: await fileHash(path) });
  if (terminal.schema !== "gptgrep.opt-actor-terminal.v1" || terminal.status !== "completed" || !terminal.producer ||
      canonical(terminal.action) !== canonical({ ...action, phase })) throw new Error("Exact completed prior actor phase is unavailable");
  return terminal;
}
export async function verifyEngineerAuth(policy: ActorPolicy, candidateId: string, launchPin: Pin): Promise<Pin> {
  const route = policy.engineers.find(e => e.candidateId === candidateId)!, path = route.auth.receiptPath;
  await verifyAuthTerminal({ auth: path, authRunId: route.auth.runId });
  const pin = { path, sha256: await fileHash(path) }, auth = await pinned(pin), command = compileEngineerCommand(policy, route, launchPin);
  const separator = command.argv.indexOf("--"), payload = command.argv.slice(separator + 1);
  if (auth.schema !== "gptgrep.codex-auth.run.v1" || auth.operation !== "run" || auth.state !== "PASS" || auth.child_exit_code !== 0 || auth.exit_code !== 0 ||
      auth.run_id !== route.auth.runId || auth.role !== route.auth.role || auth.auth_source_id !== route.auth.sourceId ||
      auth.cwd !== join(dirname(path), "work") || canonical(auth.argv) !== canonical(payload) || auth.lease?.mode !== "exclusive" || auth.lease?.retained_by_guardian_pid) throw new Error("Native engineer auth terminal does not bind the exact successful producer");
  if (auth.codex_home !== join(dirname(path), "codex-home") || auth.layout?.auth_link !== join(auth.codex_home, "auth.json")) throw new Error("Native engineer auth cleanup path is unavailable");
  // Check metadata only, without reading or following any credential link.
  try { await lstat(auth.layout.auth_link); throw new Error("Native engineer credential link remains mounted"); }
  catch (error: any) { if (error.code !== "ENOENT") throw error; }
  return pin;
}
export async function runActorAction(action: ActorAction): Promise<Pin> {
  closed(action, ["policy", "competition", "freezeReview", "candidateId", "phase"], "native actor action");
  identity(action.candidateId, "candidate ID");
  if (!["engineer", "build", "agy"].includes(action.phase)) throw new Error("Unknown native actor phase");
  const { policy, competition, campaign } = await verifyActorGate(action.competition, action.policy, action.freezeReview);
  const route = policy.engineers.find(e => e.candidateId === action.candidateId);
  if (!route) throw new Error("Candidate is outside the two frozen native slots");
  const path = paths(policy, action.candidateId, action.phase), lockPath = join(policy.outputRoot, "actor-in-flight.json");
  await observeActorStorage(policy.storage);
  await requireStorage(lockPath); await mkdir(policy.outputRoot, { recursive: true, mode: 0o700 });
  // Completed actions are read-only on repeat. A charged action without a terminal
  // cannot reach this code again: its launch and persistent lock already exist.
  try {
    const existing = await pinned<ActorTerminal>({ path: path.terminal, sha256: await fileHash(path.terminal) });
    if (existing.schema !== "gptgrep.opt-actor-terminal.v1" || canonical(existing.action) !== canonical(action) || existing.status === "unknown") throw new Error("Existing actor action requires exact reconciliation");
    return { path: path.terminal, sha256: await fileHash(path.terminal) };
  } catch (error: any) { if (error.code !== "ENOENT") throw error; }
  const purpose = action.phase === "engineer" ? "native_engineer" : action.phase === "build" ? "prepare_context" : "agy";
  const allowance = campaign.allowances[purpose];
  const planned = action.phase === "engineer" ? route.outerTimeoutMs : action.phase === "build" ? policy.builds.find(b => b.candidateId === route.candidateId)!.deadlineMs : policy.reviewer.outerTimeoutMs;
  if (planned + 1000 > allowance.deadlineMs || Date.now() + allowance.deadlineMs > Math.min(competition.deadlineAt, Date.parse(campaign.campaignDeadline))) throw new Error("Complete actor phase cannot fit the frozen shared campaign allowance");
  return withActorLock(lockPath, { action, pid: process.pid, createdAt: new Date().toISOString() }, async charged => {
    const createdAt = new Date().toISOString(), expiresAt = new Date(Date.now() + allowance.deadlineMs - 500).toISOString();
    let preparedJob: Pin | null = null;
    if (action.phase === "build") preparedJob = (await previous(policy, action, "engineer")).producer;
    if (action.phase === "agy") preparedJob = await prepareAgy(policy, (await previous(policy, action, "build")).producer!, expiresAt);
    const launch = await immutable(path.launch, { schema: "gptgrep.opt-actor-launch.v1", policy: action.policy, competition: action.competition,
      freezeReview: action.freezeReview, candidateId: route.candidateId, purpose, actionId: path.actionId,
      reservationPath: campaignActionPath(campaign, { experimentId: competition.competitionId, actionId: path.actionId }), preparedJob, createdAt, expiresAt } satisfies ActorLaunch);
    const descriptor = action.phase === "engineer" ? compileEngineerCommand(policy, route, launch) : action.phase === "build"
      ? buildDescriptor(policy.builds.find(b => b.candidateId === route.candidateId)!, preparedJob!) : (await pinned<PreparedAgy>(preparedJob!)).command;
    const reservation = await reserveCampaignAction(policy.campaign, { experimentId: competition.competitionId,
      actionId: path.actionId, purpose, commandSha256: digest(descriptor) }); charged();
    let producer: Pin | null = null, auth: Pin | null = null, processObservation: ProcessObservation | null = null, error: string | null = null;
    let status: ActorTerminal["status"] = "unknown";
    try {
      if (action.phase === "engineer") {
        processObservation = await executeOwnerCommand(compileEngineerCommand(policy, route, launch), Date.parse(expiresAt) - Date.now());
        const receiptPath = join(route.outputDir, "engineer-result.json"); producer = { path: receiptPath, sha256: await fileHash(receiptPath) };
        const value = await pinned<EngineerReceipt>(producer);
        if (canonical(value.launch) !== canonical(launch) || value.candidateId !== route.candidateId || canonical(value.policy) !== canonical(action.policy)) throw new Error("Native engineer producer belongs to another action");
        if (!processObservation.started || processObservation.exitCode !== 0 || processObservation.executionError || processObservation.timedOut || processObservation.outputExceeded || !processObservation.cleanup) throw new Error("Native engineer outer transport failed or cleanup is unresolved");
        auth = await verifyEngineerAuth(policy, route.candidateId, launch);
        status = value.status === "candidate_source_ready" ? "completed" : "failed"; error = value.error;
      } else if (action.phase === "build") {
        producer = await runCandidateBuild(launch); const value = await pinned<CandidateBuildReceipt>(producer);
        status = actorTerminalStatus(value.status); error = value.error;
      } else {
        producer = await runNativeAgy(launch); const value = await pinned<AgyReceipt>(producer);
        status = actorTerminalStatus(value.status); error = value.error;
      }
    } catch (caught) { error = caught instanceof Error ? caught.message : String(caught); }
    const terminal = await immutable(path.terminal, { schema: "gptgrep.opt-actor-terminal.v1", action, actionId: path.actionId, launch,
      reservation: { path: reservation.path, sha256: reservation.sha256 }, status, error, producer, auth, process: processObservation,
      completedAt: new Date().toISOString(), acceptance: "not_evaluated" } satisfies ActorTerminal);
    return { value: terminal, status };
  });
}
