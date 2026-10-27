/** Explicit legacy Composer and project stream routes. Preparation is never a native review. */
import { chmod, lstat, mkdir, open, readFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { canonical, digest, fileHash, requireStorage, sha256 } from "../../benchmark/src/io";
import { campaignActionPath, verifyCampaignAction } from "../../benchmark/src/campaign";
import { executeOwnerCommand, type ProcessCommand, type ProcessObservation } from "../../benchmark/src/process";
import { validateAgyEnvelope, verifyReceiptPin } from "./native-receipts";
import { verifyCandidateBuild, type CandidateBuildReceipt } from "./candidate-build";
import { immutable, isAgyStreamRoute, observeActorStorage, pinned, verifyActorGate, verifyWorktree, type ActorLaunch, type ActorPolicy } from "./actor-contracts";
import { agyDenyDefinition, agyHelperMarkers } from "./agy-stream";
import { DENY_HELPER, validateAgyReviewResult, validateAgyStreamJob, validateAgyStreamPacket,
  type AgyStreamJob, type AgyStreamPacket, type AgyStreamReceipt } from "./agy-stream-contracts";
import type { EngineerReceipt } from "./native-engineer";
import type { Pin } from "./competition";

export interface PreparedAgy {
  schema: "gptgrep.opt-prepared-agy.v1" | "gptgrep.opt-prepared-agy.v2"; candidateId: string; build: Pin;
  request: Pin; task: Pin; expectedReceiptPath: string; command: ProcessCommand;
}
export interface AgyReceipt {
  schema: "gptgrep.opt-agy-result.v1" | "gptgrep.opt-agy-result.v2"; executionMode: "live"; launch: Pin; build: Pin; candidateId: string;
  candidateRevision: string; status: "review_reported" | "failed" | "unknown"; error: string | null;
  request: Pin; task: Pin; receipt: Pin | null; native: Pin | null; preparedPrompt: Pin | null;
  sessionId: string | null; process: ProcessObservation; storageBefore: unknown; storageAfter: unknown;
  requestedModel: string; requestedEffort: string; observedModel: null; observedEffort: null;
  serviceTier: "not_exposed_by_client"; auditAcceptance: "pending_parent_adjudication";
  confinement: "cooperative_allowlist; native_terminal_sandbox_requested; effective_policy_unverified";
  descendantQuiescence: "not_established"; releaseAccepted: false;
  trace?: Pin | null;
}
export function agyCommand(policy: ActorPolicy, request: Pin, task: Pin): ProcessCommand {
  const r = policy.reviewer;
  if (isAgyStreamRoute(r)) return { executable: r.bun,
    inputs: [request, task, r.binary, r.producer, r.bunConfig, r.resultSchema, ...r.producerSources, ...r.rootInstructions],
    argv: ["--no-env-file", "--no-install", `--config=${r.bunConfig.path}`, r.producer.path, "--job", request.path, "--expect-sha256", request.sha256],
    cwd: policy.outputRoot, timeoutMs: r.outerTimeoutMs - 2000, maxOutputBytes: 2 * 1024 * 1024 };
  return { executable: r.python, inputs: [request, task, r.binary, r.composer, r.resultSchema, ...r.composerSources, ...r.rootInstructions],
    argv: ["-B", r.composer.path, "run", "--request", request.path, "--output-root", r.outputRoot, "--apply"],
    cwd: policy.outputRoot, timeoutMs: r.outerTimeoutMs - 2000, maxOutputBytes: 2 * 1024 * 1024 };
}
export function agyTaskPacket(policy: ActorPolicy, build: CandidateBuildReceipt, engineer: EngineerReceipt, instructions: string) {
  return { schema: "gptgrep.opt-review-packet.v2", instructions,
    task: "Audit the exact committed candidate against the frozen parent. Inspect the declared source diff and checks for correctness, benchmark-specific routing or contamination, regressions and violated boundaries. Do not edit, start workers, execute builds, contact providers, read credentials or inspect evaluation tasks, gold, judge objects or another candidate. Return evidence-grounded findings and explicit limitations; the parent adjudicates acceptance.",
    candidateId: build.candidateId, parentRevision: policy.parentRevision, revision: build.candidateRevision,
    diffSha256: engineer.diffSha256, changedPaths: engineer.changedPaths, hypothesis: engineer.hypothesis,
    build: { binarySha256: build.binary!.sha256, platform: "linux", targetArch: policy.builds.find(b => b.candidateId === build.candidateId)!.targetArch },
    checks: build.checks.map(c => ({ argv: c.argv, exitCode: c.exitCode })) };
}
export function validateAgyTaskPacket(task: any, policy: ActorPolicy, build: CandidateBuildReceipt, engineer: EngineerReceipt, instructions: string) {
  if (canonical(task) !== canonical(agyTaskPacket(policy, build, engineer, instructions))) throw new Error("Prepared AGY task differs from the exact frozen instructions, source diff or build/check evidence");
}
export function agyRequest(policy: ActorPolicy, options: { candidateId: string; candidateRevision: string; task: Pin; resultSchema: unknown; createdAt: string; expiresAt: string }) {
  const reviewer = policy.reviewer, route = policy.engineers.find(e => e.candidateId === options.candidateId);
  if (isAgyStreamRoute(reviewer)) throw new Error("Stream review cannot use a legacy Composer request");
  const requestId = `${policy.competitionId}-${options.candidateId}-audit`;
  if (!route || !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,120}$/.test(requestId) || !Number.isFinite(Date.parse(options.createdAt)) ||
      !Number.isFinite(Date.parse(options.expiresAt)) || Date.parse(options.createdAt) >= Date.parse(options.expiresAt)) throw new Error("Exact bounded AGY request identity and times required");
  const argv = [reviewer.binary.path, "-p", "{prompt}", "--mode", "plan", "--model", reviewer.model, "--effort", reviewer.effort,
    "--sandbox", "--print-timeout", `${reviewer.timeoutSeconds}s`, "--output-format", "json", "--json-schema", JSON.stringify(options.resultSchema)];
  if (reviewer.permissionPolicy === "skip-permissions") argv.push("--dangerously-skip-permissions");
  return { schema: "agents_composer.process_request.v1", request_id: requestId, created_at: options.createdAt, backend: "agy_print",
    argv, cwd: route.worktree.path, prompt: { ref: "task.json", sha256: options.task.sha256 }, timeout_sec: reviewer.timeoutSeconds,
    max_output_bytes: reviewer.maxOutputBytes, result_format: "json",
    agy: { role: "audit", allowed_dirs: [route.worktree.path], root_instructions: reviewer.rootInstructions, deadline_utc: options.expiresAt,
      max_in_flight: 1, max_native_subagents: 0, permission_policy: reviewer.permissionPolicy, authorization: reviewer.authorization,
      session: { mode: "fresh" }, max_sources: reviewer.maxSources, source_revision: options.candidateRevision } };
}
export function validateAgyRequest(request: any, policy: ActorPolicy, options: Omit<Parameters<typeof agyRequest>[1], "createdAt">) {
  if (canonical(request) !== canonical(agyRequest(policy, { ...options, createdAt: request.created_at }))) throw new Error("Prepared AGY request differs from its frozen native model, permission or input policy");
}
export async function composerArtifactPin(artifact: any, receiptPath: string, maxBytes: number): Promise<Pin> {
  if (!artifact || canonical(Object.keys(artifact).sort()) !== canonical(["bytes", "path", "sha256"]) || dirname(artifact.path) !== dirname(receiptPath) ||
      !Number.isSafeInteger(artifact.bytes) || artifact.bytes < 1 || artifact.bytes > maxBytes) throw new Error("AGY stdout lacks the exact bounded Composer artifact metadata");
  const pin = { path: artifact.path, sha256: artifact.sha256 }; await verifyReceiptPin(pin);
  if ((await lstat(pin.path)).size !== artifact.bytes) throw new Error("AGY stdout byte count differs from its actual retained artifact");
  return pin;
}
export async function agyStreamTaskPacket(policy: ActorPolicy, build: CandidateBuildReceipt, engineer: EngineerReceipt, instructions: string): Promise<AgyStreamPacket> {
  const reviewer = policy.reviewer;
  if (!isAgyStreamRoute(reviewer)) throw new Error("Explicit stream reviewer route required for a complete packet");
  const route = policy.engineers.find(e => e.candidateId === build.candidateId)!;
  const { git } = await verifyWorktree(route.worktree, build.candidateRevision);
  const diff = await git("diff", "--binary", "--no-ext-diff", "--no-renames", policy.parentRevision, build.candidateRevision, "--");
  if (digest(diff) !== engineer.diffSha256) throw new Error("Full AGY source diff differs from the actual engineer receipt");
  const sources: AgyStreamPacket["sources"] = [];
  for (const path of [...engineer.changedPaths].sort()) {
    const body = async (revision: string) => {
      const entry = await git("ls-tree", revision, "--", path);
      if (!entry) return null;
      if (!/^(100644|100755) blob /.test(entry)) throw new Error("AGY source packet cannot read a non-regular source object");
      const { stdout } = await promisify(execFile)("git", ["-C", route.worktree.path, "show", `${revision}:${path}`],
        { encoding: "buffer", maxBuffer: reviewer.maxPacketBytes, timeout: 10000 });
      const text = stdout.toString("utf8");
      // Exact round-trip preserves a valid UTF-8 BOM while rejecting lossy
      // replacement decoding and unsupported NUL-bearing source blobs.
      if (!Buffer.from(text, "utf8").equals(stdout) || text.includes("\0")) throw new Error("AGY source body is not exact UTF-8 text");
      return text;
    };
    sources.push({ path, before: await body(policy.parentRevision), after: await body(build.candidateRevision) });
  }
  const roots = await Promise.all(reviewer.rootInstructions.map(pin => readFile(pin.path, "utf8")));
  const packet: AgyStreamPacket = { ...agyTaskPacket(policy, build, engineer, [instructions, ...roots].join("\n\n")),
    schema: "gptgrep.opt-review-packet.v3", diff, sources };
  validateAgyStreamPacket(packet); return packet;
}
function streamJob(policy: ActorPolicy, build: CandidateBuildReceipt, engineer: EngineerReceipt, task: Pin, expiresAt: string, denyPolicy: AgyStreamJob["denyPolicy"]): AgyStreamJob {
  const r = policy.reviewer;
  if (!isAgyStreamRoute(r) || !engineer.diffSha256) throw new Error("Exact source-bearing stream route required");
  const requestId = `${policy.competitionId}-${build.candidateId}-audit`, root = join(r.outputRoot, requestId);
  return { schema: "gptgrep.opt-agy-stream-job.v1", requestId, candidateId: build.candidateId, parentRevision: policy.parentRevision,
    candidateRevision: build.candidateRevision, diffSha256: engineer.diffSha256, packet: task, binary: r.binary, bun: r.bun,
    producer: r.producer, producerSources: r.producerSources, resultSchema: r.resultSchema, cwd: join(root, "workspace"), nativeHome: r.nativeHome,
    outputRoot: join(root, "native"), model: r.model, effort: r.effort, deadlineAt: Date.parse(expiresAt), timeoutMs: r.timeoutSeconds * 1000,
    maxPacketBytes: r.maxPacketBytes, maxInputBytes: r.maxInputBytes, maxTraceBytes: r.maxTraceBytes, maxResultBytes: r.maxOutputBytes,
    maxSources: r.maxSources, denyPolicy };
}
async function textArtifact(path: string, text: string, mode: number): Promise<Pin> {
  await requireStorage(path); await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const file = await open(path, "wx", mode);
  try { await file.writeFile(text); await file.chmod(mode); await file.sync(); } finally { await file.close(); }
  return { path, sha256: sha256(text) };
}
export async function validatePreparedAgy(policy: ActorPolicy, prepared: PreparedAgy, build: CandidateBuildReceipt, engineer: EngineerReceipt, expiresAt: string): Promise<any> {
  const request = await pinned(prepared.request), task = await pinned(prepared.task), r = policy.reviewer;
  const instructions = await readFile(r.instructions.path, "utf8");
  if (isAgyStreamRoute(r)) {
    if (prepared.schema !== "gptgrep.opt-prepared-agy.v2") throw new Error("Stream reviewer cannot consume a legacy prepared job");
    validateAgyStreamJob(request); validateAgyStreamPacket(task);
    if (canonical(task) !== canonical(await agyStreamTaskPacket(policy, build, engineer, instructions)) ||
        canonical(request) !== canonical(streamJob(policy, build, engineer, prepared.task, expiresAt, request.denyPolicy)) ||
        prepared.expectedReceiptPath !== join(request.outputRoot, "receipt.json")) throw new Error("Prepared stream review differs from its exact full source packet or route");
    const helperPath = join(request.cwd, ".agents", "deny-all.sh"), hookPath = join(request.cwd, ".agents", "hooks.json"), spoolPath = join(dirname(request.cwd), "receipts", "hook-invocations.txt");
    if (request.denyPolicy.helper.path !== helperPath || request.denyPolicy.hook.path !== hookPath || request.denyPolicy.spoolPath !== spoolPath ||
        request.denyPolicy.helper.sha256 !== sha256(DENY_HELPER) ||
        request.denyPolicy.hook.sha256 !== sha256(`${JSON.stringify(agyDenyDefinition(helperPath, spoolPath), null, 2)}\n`)) throw new Error("Prepared stream review changed its selected deny policy");
  } else {
    if (prepared.schema !== "gptgrep.opt-prepared-agy.v1") throw new Error("Legacy reviewer cannot consume a stream job");
    validateAgyTaskPacket(task, policy, build, engineer, instructions);
    validateAgyRequest(request, policy, { candidateId: build.candidateId, candidateRevision: build.candidateRevision, task: prepared.task, resultSchema: await pinned(r.resultSchema), expiresAt });
  }
  return request;
}
/** Read-only validation shared by actual dispatch and later parent qualification. */
export async function validateAgyAttemptEvidence(policy: ActorPolicy, prepared: PreparedAgy, build: CandidateBuildReceipt, engineer: EngineerReceipt, expiresAt: string, receiptPin: Pin) {
  const request = await validatePreparedAgy(policy, prepared, build, engineer, expiresAt), r = policy.reviewer;
  if (receiptPin.path !== prepared.expectedReceiptPath) throw new Error("AGY receipt belongs to another prepared attempt");
  const receipt = await pinned(receiptPin);
  if (!isAgyStreamRoute(r)) {
    const native = await composerArtifactPin(receipt.artifacts?.stdout, receiptPin.path, r.maxOutputBytes), raw = await pinned(native);
    const identity = validateAgyEnvelope(raw, request, receipt, { client: "agy", binary: r.binary, producer: r.composer, model: r.model, effort: r.effort,
      serviceTier: "not_exposed_by_client", cwd: request.cwd, inputSha256: sha256((await pinned(prepared.task)).instructions), permissionPolicy: r.permissionPolicy }, build.candidateRevision);
    const prompt = { path: join(dirname(receiptPin.path), "prepared-prompt.txt"), sha256: receipt.prepared_prompt_sha256 };
    await verifyReceiptPin(prompt);
    if (!(await readFile(prompt.path, "utf8")).endsWith(`Task (selected by parent):\n${await readFile(prepared.task.path, "utf8")}`)) throw new Error("Legacy AGY prompt differs from its exact task");
    return { sessionId: identity.sessionId, native, preparedPrompt: prompt, trace: null as Pin | null, result: receipt.result };
  }
  const value = receipt as AgyStreamReceipt, job = request as AgyStreamJob;
  if (value.schema !== "gptgrep.opt-agy-stream-receipt.v1" || value.status !== "review_reported" || value.error !== null ||
      canonical(value.job) !== canonical(prepared.request) || canonical(value.packet) !== canonical(prepared.task) || value.candidateId !== build.candidateId ||
      value.parentRevision !== policy.parentRevision || value.candidateRevision !== build.candidateRevision || value.diffSha256 !== engineer.diffSha256 ||
      value.requestedModel !== r.model || value.requestedEffort !== r.effort || value.originalDeadlineAt !== job.deadlineAt ||
      value.submittedUserEvents !== 1 || value.userInputAcknowledgements !== 1 || !value.native || !value.policy?.filesUnchanged ||
      !value.lifecycle?.leaderClosed || !value.lifecycle.groupAbsenceProven || value.lifecycle.signalErrorCount !== 0 || value.lifecycle.groupProbeError !== null ||
      value.acceptance !== "pending_parent_adjudication" || value.releaseAccepted !== false || value.descendantQuiescence !== "not_established" ||
      !value.process.started || value.process.exitCode !== 0 || !value.process.cleanup || value.process.executionError || value.process.timedOut || value.process.outputExceeded) throw new Error("Actual successful stream review evidence is unavailable");
  for (const pin of [value.native, value.trace, value.preparedPrompt, value.inputFrame, job.denyPolicy.hook, job.denyPolicy.helper]) await verifyReceiptPin(pin);
  if ((await lstat(value.native.path)).size > job.maxResultBytes) throw new Error("AGY native result exceeds its selected bound");
  const packetRaw = await readFile(prepared.task.path), promptRaw = await readFile(value.preparedPrompt.path), frameRaw = await readFile(value.inputFrame.path), traceRaw = await readFile(value.trace.path);
  const expectedFrame = Buffer.from(`${JSON.stringify({ event: "user", message: { content: packetRaw.toString("utf8") } })}\n`);
  if (!promptRaw.equals(packetRaw) || !frameRaw.equals(expectedFrame) || frameRaw.length !== value.inputBytes || frameRaw.length > job.maxInputBytes ||
      traceRaw.length !== value.process.stdoutBytes || sha256(traceRaw) !== value.process.stdoutSha256 || traceRaw.length > job.maxTraceBytes ||
      await agyHelperMarkers(job.denyPolicy.spoolPath) !== value.policy.helperMarkers) throw new Error("Retained AGY stream input, trace or policy counters differ");
  const lines = traceRaw.toString("utf8").split("\n").filter(Boolean);
  if (lines.length > 100000) throw new Error("AGY native trace event count exceeds its bound");
  const events = lines.map(line => JSON.parse(line)), inits = events.filter(e => e.event === "init"), results = events.filter(e => e.event === "result");
  const acknowledgements = events.filter(e => e.event === "step_update" && e.step_update?.step_type === "user_input" && e.step_update.state === "DONE");
  const raw = await pinned(value.native);
  if (inits.length !== 1 || results.length !== 1 || acknowledgements.length !== 1 || inits[0].conversation_id !== value.sessionId ||
      inits[0].init?.model !== job.model || inits[0].init?.cwd !== job.cwd || inits[0].init?.permission_mode !== "request-review" ||
      raw.conversation_id !== value.sessionId || raw.status !== "SUCCESS" || canonical(raw) !== canonical(results[0].result) ||
      raw.num_turns !== value.nativeNumTurns || canonical(raw.usage ?? null) !== canonical(value.usage) ||
      events.some(e => !["init", "step_update", "result"].includes(e.event) || e.event === "step_update" &&
        (e.step_update?.conversation_id !== value.sessionId || e.step_update?.subagent_info?.subagents?.length))) throw new Error("AGY native identity, acknowledgement or terminal result differs");
  let output = raw.structured_output ?? raw.response; if (typeof output === "string") output = JSON.parse(output);
  validateAgyReviewResult(output, job.maxSources);
  if (canonical(output) !== canonical(value.result)) throw new Error("AGY structured review differs from the native response");
  return { sessionId: value.sessionId!, native: value.native, preparedPrompt: value.preparedPrompt, trace: value.trace, result: output };
}
export async function prepareAgy(policy: ActorPolicy, buildPin: Pin, expiresAt: string): Promise<Pin> {
  const build = await verifyCandidateBuild(policy, buildPin), engineer = await pinned<EngineerReceipt>(build.engineer);
  const route = policy.engineers.find(e => e.candidateId === build.candidateId)!;
  await verifyWorktree(route.worktree, build.candidateRevision);
  const reviewer = policy.reviewer, instructions = await readFile(reviewer.instructions.path, "utf8");
  const requestId = `${policy.competitionId}-${build.candidateId}-audit`;
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]{0,120}$/.test(requestId)) throw new Error("Bounded native AGY request ID required");
  const root = join(policy.outputRoot, "agy-requests", requestId);
  if (isAgyStreamRoute(reviewer)) {
    const packet = await agyStreamTaskPacket(policy, build, engineer, instructions), packetRaw = `${JSON.stringify(packet, null, 2)}\n`;
    if (Buffer.byteLength(`${JSON.stringify({ event: "user", message: { content: packetRaw } })}\n`) > reviewer.maxInputBytes) throw new Error("Complete AGY stream frame exceeds its declared bound");
    const task = await immutable(join(root, "packet.json"), packet, reviewer.maxPacketBytes), reviewRoot = join(reviewer.outputRoot, requestId), workspace = join(reviewRoot, "workspace");
    await requireStorage(reviewRoot); await mkdir(reviewer.outputRoot, { recursive: true, mode: 0o700 }); await mkdir(reviewRoot, { mode: 0o700 });
    await mkdir(join(workspace, ".agents"), { recursive: true, mode: 0o700 }); await mkdir(join(reviewRoot, "receipts"), { mode: 0o700 });
    const spoolPath = join(reviewRoot, "receipts", "hook-invocations.txt"); await textArtifact(spoolPath, "", 0o600);
    const helper = await textArtifact(join(workspace, ".agents", "deny-all.sh"), DENY_HELPER, 0o444);
    const hook = await immutable(join(workspace, ".agents", "hooks.json"), agyDenyDefinition(helper.path, spoolPath));
    await chmod(hook.path, 0o444); await chmod(join(workspace, ".agents"), 0o555);
    const job = streamJob(policy, build, engineer, task, expiresAt, { hook, helper, spoolPath }); validateAgyStreamJob(job);
    const request = await immutable(join(root, "stream-job.json"), job);
    return immutable(join(root, "prepared.json"), { schema: "gptgrep.opt-prepared-agy.v2", candidateId: build.candidateId, build: buildPin,
      request, task, expectedReceiptPath: join(job.outputRoot, "receipt.json"), command: agyCommand(policy, request, task) } satisfies PreparedAgy);
  }
  const task = await immutable(join(root, "task.json"), agyTaskPacket(policy, build, engineer, instructions), 16 * 1024);
  const request = await immutable(join(root, "request.json"), agyRequest(policy, { candidateId: build.candidateId,
    candidateRevision: build.candidateRevision, task, resultSchema: await pinned(reviewer.resultSchema), createdAt: new Date().toISOString(), expiresAt }));
  return immutable(join(root, "prepared.json"), { schema: "gptgrep.opt-prepared-agy.v1", candidateId: build.candidateId, build: buildPin,
    request, task, expectedReceiptPath: join(reviewer.outputRoot, requestId, "receipt.json"), command: agyCommand(policy, request, task) } satisfies PreparedAgy);
}
/** Actual dispatch consumes this same read-only path, including nonzero producer terminals. */
export async function consumeAgyExecution(policy: ActorPolicy, prepared: PreparedAgy, build: CandidateBuildReceipt, engineer: EngineerReceipt,
  expiresAt: string, observation: ProcessObservation) {
  const stream = isAgyStreamRoute(policy.reviewer), route = policy.engineers.find(e => e.candidateId === prepared.candidateId);
  let error: string | null = null, receipt: Pin | null = null, native: Pin | null = null, preparedPrompt: Pin | null = null, trace: Pin | null = null, sessionId: string | null = null;
  let streamReceiptObserved = false, nativeCustodyUnknown = stream;
  try {
    // Stream failures retain their real terminal/counters even when the outer CLI exits nonzero.
    if (stream) {
      receipt = { path: prepared.expectedReceiptPath, sha256: await fileHash(prepared.expectedReceiptPath) };
      const value = await pinned<AgyStreamReceipt>(receipt);
      if (value.schema !== "gptgrep.opt-agy-stream-receipt.v1" || canonical(value.job) !== canonical(prepared.request)) throw new Error("Stream producer receipt belongs to another attempt");
      streamReceiptObserved = true; nativeCustodyUnknown = value.status === "unknown" || value.process?.cleanup !== true;
      native = value.native; preparedPrompt = value.preparedPrompt; trace = value.trace; sessionId = value.sessionId;
    }
    if (!observation.started || observation.exitCode !== 0 || observation.executionError || observation.timedOut || observation.outputExceeded || !observation.cleanup) throw new Error("AGY outer process failed or cleanup is unresolved");
    receipt ??= { path: prepared.expectedReceiptPath, sha256: await fileHash(prepared.expectedReceiptPath) };
    const verified = await validateAgyAttemptEvidence(policy, prepared, build, engineer, expiresAt, receipt);
    native = verified.native; preparedPrompt = verified.preparedPrompt; trace = verified.trace; sessionId = verified.sessionId;
    if (!route) throw new Error("AGY attempt has no selected candidate worktree");
    await verifyWorktree(route.worktree, build.candidateRevision);
  } catch (caught) { error = caught instanceof Error ? caught.message : String(caught); }
  const unknown = !observation.cleanup || stream && observation.started && (!streamReceiptObserved || nativeCustodyUnknown);
  return { error, receipt, native, preparedPrompt, trace, sessionId, unknown };
}
export async function runNativeAgy(launchPin: Pin): Promise<Pin> {
  const launch = await pinned<ActorLaunch>(launchPin);
  if (launch.schema !== "gptgrep.opt-actor-launch.v1" || launch.purpose !== "agy" || !launch.preparedJob) throw new Error("Exact charged native AGY launch required");
  const { competition, policy, campaign } = await verifyActorGate(launch.competition, launch.policy, launch.freezeReview);
  const prepared = await pinned<PreparedAgy>(launch.preparedJob), build = await verifyCandidateBuild(policy, prepared.build);
  const route = policy.engineers.find(e => e.candidateId === launch.candidateId), reviewer = policy.reviewer, stream = isAgyStreamRoute(reviewer);
  if (!route || prepared.schema !== (stream ? "gptgrep.opt-prepared-agy.v2" : "gptgrep.opt-prepared-agy.v1") ||
      prepared.candidateId !== route.candidateId || build.candidateId !== route.candidateId || launch.actionId !== `agy-${route.candidateId}` ||
      launch.reservationPath !== campaignActionPath(campaign, { experimentId: competition.competitionId, actionId: launch.actionId }) ||
      canonical(prepared.command) !== canonical(agyCommand(policy, prepared.request, prepared.task))) throw new Error("AGY launch differs from the exact selected candidate/job/transport");
  const engineer = await pinned<EngineerReceipt>(build.engineer);
  const request = await validatePreparedAgy(policy, prepared, build, engineer, launch.expiresAt);
  const { row: reservation } = await verifyCampaignAction(policy.campaign, { experimentId: competition.competitionId,
    actionId: launch.actionId, purpose: "agy", commandSha256: digest(prepared.command) });
  const deadline = Math.min(Date.parse(launch.expiresAt), Date.parse(reservation.deadline));
  const jobDeadline = stream ? request.deadlineAt : Date.parse(request.agy.deadline_utc);
  if (!Number.isFinite(deadline) || jobDeadline !== Date.parse(launch.expiresAt) || Date.now() + reviewer.outerTimeoutMs > deadline) throw new Error("AGY review cannot fit its exact original action deadline");
  await verifyWorktree(route.worktree, build.candidateRevision);
  const storageBefore = await observeActorStorage(policy.storage), temporary = join(policy.outputRoot, "agy-temporary");
  await mkdir(temporary, { recursive: true, mode: 0o700 });
  const environment: Record<string, string> = { TMPDIR: temporary, TMP: temporary, TEMP: temporary, PYTHONDONTWRITEBYTECODE: "1" };
  if (stream) environment.AGY_CLI_DISABLE_AUTO_UPDATE = "true";
  else Object.assign(environment, { HOME: reviewer.nativeHome, XDG_CACHE_HOME: join(reviewer.nativeHome, ".cache"), XDG_STATE_HOME: join(reviewer.nativeHome, ".local", "state") });
  const observation = await executeOwnerCommand(prepared.command, deadline - Date.now(), { environment });
  const { error, receipt, native, preparedPrompt, trace, sessionId, unknown } = await consumeAgyExecution(policy, prepared, build, engineer, launch.expiresAt, observation);
  return immutable(join(policy.outputRoot, `agy-${route.candidateId}-result.json`), {
    schema: stream ? "gptgrep.opt-agy-result.v2" : "gptgrep.opt-agy-result.v1", executionMode: "live", launch: launchPin, build: prepared.build,
    candidateId: route.candidateId, candidateRevision: build.candidateRevision, status: unknown ? "unknown" : error ? "failed" : "review_reported", error,
    request: prepared.request, task: prepared.task, receipt, native, preparedPrompt, ...(stream ? { trace } : {}), sessionId, process: observation,
    storageBefore, storageAfter: await observeActorStorage(policy.storage), requestedModel: reviewer.model, requestedEffort: reviewer.effort,
    observedModel: null, observedEffort: null, serviceTier: "not_exposed_by_client", auditAcceptance: "pending_parent_adjudication",
    confinement: "cooperative_allowlist; native_terminal_sandbox_requested; effective_policy_unverified", descendantQuiescence: "not_established", releaseAccepted: false } satisfies AgyReceipt);
}
