/** Direct-local qualification from actual native producers, never legacy owner declarations. */
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { canonical, digest, fileHash, within } from "../../benchmark/src/io";
import { verifyCampaignAction } from "../../benchmark/src/campaign";
import { closed, boundedText, validateHypothesis, type Hypothesis } from "./proposals";
import { immutable, isAgyStreamRoute, pinned, validateActorStorageObservation, verifyActorGate, verifyWorktree, type ActorLaunch, type ActorPolicy } from "./actor-contracts";
import { actorTerminalStatus, verifyEngineerAuth, type ActorAction, type ActorTerminal } from "./actor-runtime";
import { compileEngineerCommand, engineeringPrompt, validateEngineerCapture, validateEngineerOutput, type EngineerReceipt } from "./native-engineer";
import { buildDescriptor, verifyCandidateBuild } from "./candidate-build";
import { agyCommand, validateAgyAttemptEvidence, type AgyReceipt, type PreparedAgy } from "./native-agy";
import { verifyReceiptPin } from "./native-receipts";
import type { CompetitionContract, Pin } from "./competition";

export interface DirectCandidateEvidence {
  schema: "gptgrep.opt-direct-candidate-evidence.v1";
  candidateId: string; parentRevision: string; revision: string; worktree: string; hypothesis: Hypothesis;
  competition: Pin; policy: Pin; freezeReview: Pin;
  engineer: Pin; build: Pin; review: Pin; adjudication: Pin;
}
export interface AgyAdjudication {
  schema: "gptgrep.opt-agy-adjudication.v1"; status: "accepted" | "rejected";
  candidateId: string; candidateRevision: string; diffSha256: string; agy: Pin;
  parentSessionId: string; substantiveChange: boolean; rationale: string;
  findings: Array<{ findingSha256: string; disposition: "not_blocking" | "false_positive"; rationale: string; evidence: Pin[] }>;
}
interface QualificationRecord {
  schema: "gptgrep.opt-direct-native-qualification.v1"; status: "qualified" | "unavailable"; reasons: string[];
  policy: Pin; competition: Pin; freezeReview: Pin; candidateEvidence: Pin;
  candidateId: string; parentRevision: string; candidateRevision: string; diffSha256: string; build: Pin;
  engineerSessionId: string; reviewerSessionId: string; engineerAuthRunId: string; parentAdjudicatorSessionId: string;
  worktree: ActorPolicy["engineers"][number]["worktree"];
  requestedEngineer: { model: string; effort: string; serviceTier: string };
  reviewerConfiguration: { requestedModel: string; requestedEffort: string; observedModel: null; observedEffort: null; serviceTier: "not_exposed_by_client" };
  limitations: string[]; validatorSources: Pin[]; validatedPins: Pin[]; bindingSha256: string;
}
export type DirectCandidateQualification = QualificationRecord & { evidence: Pin };
const SOURCE_PATHS = [import.meta.url, new URL("./actor-runtime.ts", import.meta.url).href, new URL("./actor-contracts.ts", import.meta.url).href,
  new URL("./native-engineer.ts", import.meta.url).href, new URL("./native-agy.ts", import.meta.url).href, new URL("./candidate-build.ts", import.meta.url).href,
  new URL("./native-receipts.ts", import.meta.url).href, new URL("./agy-stream.ts", import.meta.url).href, new URL("./agy-stream-contracts.ts", import.meta.url).href, new URL("../../benchmark/src/controller.ts", import.meta.url).href,
  new URL("../../benchmark/src/native.ts", import.meta.url).href, new URL("../../benchmark/src/process.ts", import.meta.url).href,
  new URL("../../benchmark/src/campaign.ts", import.meta.url).href].map(path => fileURLToPath(path));

export function validateAdjudication(value: AgyAdjudication, expected: {
  candidateId: string; revision: string; diffSha256: string; agy: Pin; findings: any[]; engineerSessionId: string; reviewerSessionId: string;
}): void {
  closed(value, ["schema", "status", "candidateId", "candidateRevision", "diffSha256", "agy", "parentSessionId", "substantiveChange", "rationale", "findings"], "AGY parent adjudication");
  if (value.schema !== "gptgrep.opt-agy-adjudication.v1" || value.status !== "accepted" || value.candidateId !== expected.candidateId ||
      value.candidateRevision !== expected.revision || value.diffSha256 !== expected.diffSha256 || canonical(value.agy) !== canonical(expected.agy) ||
      value.substantiveChange !== true || [expected.engineerSessionId, expected.reviewerSessionId].includes(value.parentSessionId)) throw new Error("Exact independent parent adjudication of the substantive native candidate is required");
  boundedText(value.parentSessionId, "parent adjudicator session", 256); boundedText(value.rationale, "parent adjudication rationale", 2000);
  if (!Array.isArray(value.findings) || value.findings.length !== expected.findings.length || value.findings.length > 16) throw new Error("Every actual AGY finding requires an explicit parent disposition");
  for (const [index, finding] of value.findings.entries()) {
    closed(finding, ["findingSha256", "disposition", "rationale", "evidence"], "AGY finding disposition");
    if (finding.findingSha256 !== digest(expected.findings[index]) || !["not_blocking", "false_positive"].includes(finding.disposition) ||
        !Array.isArray(finding.evidence) || !finding.evidence.length || finding.evidence.length > 8) throw new Error("AGY finding disposition differs from its exact evidence");
    boundedText(finding.rationale, "finding disposition rationale", 2000);
  }
}
async function actorTerminal(policy: ActorPolicy, evidence: DirectCandidateEvidence, pin: Pin, phase: ActorAction["phase"]) {
  const terminal = await pinned<ActorTerminal>(pin);
  closed(terminal, ["schema", "action", "actionId", "launch", "reservation", "status", "error", "producer", "auth", "process", "completedAt", "acceptance"], "actual actor terminal");
  const action: ActorAction = { policy: evidence.policy, competition: evidence.competition, freezeReview: evidence.freezeReview, candidateId: evidence.candidateId, phase };
  if (pin.path !== join(policy.outputRoot, "actions", `${phase}-${evidence.candidateId}-terminal.json`) || terminal.schema !== "gptgrep.opt-actor-terminal.v1" ||
      terminal.status !== "completed" || terminal.error !== null || !terminal.producer || terminal.acceptance !== "not_evaluated" ||
      terminal.actionId !== `${phase}-${evidence.candidateId}` || canonical(terminal.action) !== canonical(action)) throw new Error("Direct-local candidate lacks its exact completed native actor phase");
  const launch = await pinned<ActorLaunch>(terminal.launch), route = policy.engineers.find(e => e.candidateId === evidence.candidateId)!;
  const purpose = phase === "engineer" ? "native_engineer" : phase === "build" ? "prepare_context" : "agy";
  if (terminal.launch.path !== join(policy.outputRoot, "actions", `${phase}-${evidence.candidateId}-launch.json`) || launch.schema !== "gptgrep.opt-actor-launch.v1" ||
      launch.purpose !== purpose || launch.candidateId !== evidence.candidateId || launch.actionId !== terminal.actionId ||
      canonical(launch.policy) !== canonical(evidence.policy) || canonical(launch.competition) !== canonical(evidence.competition) || canonical(launch.freezeReview) !== canonical(evidence.freezeReview)) throw new Error("Actor terminal belongs to another exact launch");
  const descriptor = phase === "engineer" ? compileEngineerCommand(policy, route, terminal.launch) : phase === "build"
    ? buildDescriptor(policy.builds.find(b => b.candidateId === evidence.candidateId)!, launch.preparedJob!) : (await pinned<PreparedAgy>(launch.preparedJob!)).command;
  const charge = await verifyCampaignAction(policy.campaign, { experimentId: policy.competitionId, actionId: terminal.actionId, purpose, commandSha256: digest(descriptor) });
  if (canonical(charge.reservation) !== canonical(terminal.reservation) || launch.reservationPath !== charge.reservation.path ||
      !Number.isFinite(Date.parse(terminal.completedAt)) || !Number.isFinite(Date.parse(launch.createdAt)) || !Number.isFinite(Date.parse(launch.expiresAt)) ||
      Date.parse(launch.createdAt) > Date.parse(charge.row.reservedAt) || Date.parse(launch.expiresAt) > Date.parse(charge.row.deadline) ||
      Date.parse(terminal.completedAt) < Date.parse(charge.row.reservedAt)) throw new Error("Actor terminal lacks the same charged-before-launch action");
  return { terminal, launch };
}
/** The retained producer has no owner validator for engineering/build/AGY custody.
 * No receipt pin, review status or caller-supplied witness can enable that route.
 * Add a supported route only against its actual selected owner contract. */
export function requireActorRuntimeQualification(): never {
  throw new Error("Actor runtime qualification unavailable: no selected owner validator binds native jobs, isolation, collection/finalization and descendant quiescence");
}
export async function inspectDirectCandidate(competition: CompetitionContract, evidencePin: Pin): Promise<Omit<QualificationRecord, "bindingSha256">> {
  if (competition.schema !== "gptgrep.opt-competition.v3" || !competition.nativePolicy) throw new Error("Direct-local qualification requires an explicit version 3 competition");
  const evidence = await pinned<DirectCandidateEvidence>(evidencePin);
  closed(evidence, ["schema", "candidateId", "parentRevision", "revision", "worktree", "hypothesis", "competition", "policy", "freezeReview", "engineer", "build", "review", "adjudication"], "direct-local candidate evidence");
  if (evidence.schema !== "gptgrep.opt-direct-candidate-evidence.v1" || !competition.candidateIds.includes(evidence.candidateId) ||
      evidence.parentRevision !== competition.parentRevision || canonical(evidence.policy) !== canonical(competition.nativePolicy)) throw new Error("Direct-local candidate belongs to another frozen competition");
  const gate = await verifyActorGate(evidence.competition, evidence.policy, evidence.freezeReview, "retained_evidence"), policy = gate.policy;
  if (canonical(gate.competition) !== canonical(competition)) throw new Error("Candidate evidence replaced the selected competition contract");
  const route = policy.engineers.find(e => e.candidateId === evidence.candidateId)!;
  if (evidence.worktree !== route.worktree.path || evidence.revision === policy.parentRevision) throw new Error("Direct-local candidate has no distinct frozen worktree revision");
  validateHypothesis(evidence.hypothesis);
  const { terminal: engineering, launch: engineerLaunch } = await actorTerminal(policy, evidence, evidence.engineer, "engineer");
  const source = await pinned<EngineerReceipt>(engineering.producer!);
  if (engineering.producer!.path !== join(route.outputDir, "engineer-result.json") || source.schema !== "gptgrep.opt-engineer-result.v1" || source.executionMode !== "live" ||
      source.status !== "candidate_source_ready" || source.error !== null || source.candidateId !== evidence.candidateId || source.parentRevision !== policy.parentRevision || source.candidateRevision !== evidence.revision ||
      canonical(source.launch) !== canonical(engineering.launch) || canonical(source.policy) !== canonical(evidence.policy) || canonical(source.hypothesis) !== canonical(evidence.hypothesis) ||
      canonical(source.source) !== canonical({ producer: route.producer, nativeCapture: route.nativeCapture, runtimeRevision: policy.runtime.revision }) || engineerLaunch.preparedJob !== null) throw new Error("Candidate source is not the exact successful native engineer output");
  const native = await pinned(source.native);
  if (source.native.path !== join(route.outputDir, "native-result.json") || source.trace.path !== join(route.outputDir, "native-trace.jsonl") || native.tracePath !== source.trace.path) throw new Error("Native engineering artifact addresses differ from their frozen producer");
  await verifyReceiptPin(source.trace);
  const input = await engineeringPrompt(policy, route);
  const { sha256 } = await import("../../benchmark/src/io");
  if (source.promptSha256 !== sha256(input)) throw new Error("Native engineer did not receive the exact frozen development projection");
  await validateEngineerCapture(native, route, source.promptSha256);
  const output = JSON.parse(native.answerText); validateEngineerOutput(output, policy);
  if (output.candidateRevision !== evidence.revision || canonical(output.hypothesis) !== canonical(evidence.hypothesis)) throw new Error("Registered source differs from the actual native answer");
  if (!engineering.process?.started || engineering.process.exitCode !== 0 || !engineering.process.cleanup || engineering.process.executionError || engineering.process.timedOut || engineering.process.outputExceeded) throw new Error("Native engineering outer process is not terminal");
  const authPin = await verifyEngineerAuth(policy, evidence.candidateId, engineering.launch);
  if (canonical(authPin) !== canonical(engineering.auth)) throw new Error("Engineer auth evidence differs from the actual charged terminal");
  const { git } = await verifyWorktree(route.worktree, evidence.revision);
  await git("merge-base", "--is-ancestor", policy.parentRevision, evidence.revision);
  const changed = (await git("diff", "--name-only", "-z", "--no-renames", policy.parentRevision, evidence.revision, "--")).split("\0").filter(Boolean).sort();
  const diffSha256 = digest(await git("diff", "--binary", "--no-ext-diff", "--no-renames", policy.parentRevision, evidence.revision, "--"));
  if (!changed.length || changed.some(p => !policy.mutationPaths.includes(p)) || canonical(changed) !== canonical([...evidence.hypothesis.mutationPaths].sort()) ||
      canonical(changed) !== canonical(source.changedPaths) || diffSha256 !== source.diffSha256) throw new Error("Actual candidate source diff changed since native engineering");
  const { terminal: building, launch: buildLaunch } = await actorTerminal(policy, evidence, evidence.build, "build");
  const build = await verifyCandidateBuild(policy, building.producer!);
  if (canonical(build.engineer) !== canonical(engineering.producer) || canonical(build.launch) !== canonical(building.launch) ||
      canonical(buildLaunch.preparedJob) !== canonical(engineering.producer) || build.candidateRevision !== evidence.revision) throw new Error("Candidate build/checks do not follow the actual engineer source");
  const { terminal: reviewing, launch: reviewLaunch } = await actorTerminal(policy, evidence, evidence.review, "agy");
  const audit = await pinned<AgyReceipt>(reviewing.producer!), prepared = await pinned<PreparedAgy>(reviewLaunch.preparedJob!);
  if (audit.schema !== (isAgyStreamRoute(policy.reviewer) ? "gptgrep.opt-agy-result.v2" : "gptgrep.opt-agy-result.v1") || audit.executionMode !== "live" || actorTerminalStatus(audit.status) !== "completed" || audit.error !== null ||
      audit.candidateId !== evidence.candidateId || audit.candidateRevision !== evidence.revision || canonical(audit.launch) !== canonical(reviewing.launch) ||
      canonical(audit.build) !== canonical(building.producer) || canonical(prepared.build) !== canonical(building.producer) ||
      canonical(prepared.command) !== canonical(agyCommand(policy, prepared.request, prepared.task)) || !audit.receipt || !audit.native || !audit.preparedPrompt ||
      canonical(audit.request) !== canonical(prepared.request) || canonical(audit.task) !== canonical(prepared.task) ||
      audit.receipt.path !== prepared.expectedReceiptPath || audit.auditAcceptance !== "pending_parent_adjudication") throw new Error("Actual independent AGY audit is unavailable");
  const reviewerIdentity = await validateAgyAttemptEvidence(policy, prepared, build, source, reviewLaunch.expiresAt, audit.receipt);
  if (reviewerIdentity.sessionId !== audit.sessionId || reviewerIdentity.sessionId === native.sessionId ||
      canonical(reviewerIdentity.native) !== canonical(audit.native) || canonical(reviewerIdentity.preparedPrompt) !== canonical(audit.preparedPrompt) ||
      isAgyStreamRoute(policy.reviewer) && canonical(reviewerIdentity.trace) !== canonical(audit.trace ?? null) ||
      !audit.process.started || audit.process.exitCode !== 0 || !audit.process.cleanup || audit.process.executionError || audit.process.timedOut || audit.process.outputExceeded) throw new Error("AGY audit identity, capture or cleanup differs from the exact live attempt");
  const adjudication = await pinned<AgyAdjudication>(evidence.adjudication);
  validateAdjudication(adjudication, { candidateId: evidence.candidateId, revision: evidence.revision, diffSha256, agy: reviewing.producer!,
    findings: reviewerIdentity.result.findings, engineerSessionId: native.sessionId, reviewerSessionId: reviewerIdentity.sessionId });
  for (const observation of [source.storageBefore, source.storageAfter, build.storageBefore, build.storageAfter, audit.storageBefore, audit.storageAfter]) validateActorStorageObservation(observation, policy.storage);
  const pins = [evidencePin, evidence.competition, evidence.policy, evidence.freezeReview, evidence.engineer, evidence.build, evidence.review, evidence.adjudication,
    engineering.launch, engineering.reservation, engineering.producer!, authPin, source.native, source.trace,
    building.launch, building.reservation, building.producer!, build.product!, build.binary!, build.runtimeManifest!, build.buildTerminal!, ...build.checks.map(c => c.terminal),
    reviewing.launch, reviewing.reservation, reviewing.producer!, reviewLaunch.preparedJob!, audit.request, audit.task, audit.receipt, audit.native, audit.preparedPrompt, ...(audit.trace ? [audit.trace] : []),
    ...adjudication.findings.flatMap(f => f.evidence)];
  const validatedPins = [...new Map(pins.map(p => [canonical(p), p])).values()].sort((a, b) => a.path.localeCompare(b.path));
  for (const pin of validatedPins) await verifyReceiptPin(pin, canonical(pin) === canonical(build.binary));
  const validatorSources = await Promise.all(SOURCE_PATHS.map(async path => ({ path, sha256: await fileHash(path) })));
  return { schema: "gptgrep.opt-direct-native-qualification.v1", status: "unavailable",
    reasons: ["actor_owner_job_and_isolation_validator_unavailable", "agy_descendant_quiescence_unavailable", "candidate_build_peak_unmeasured"],
    policy: evidence.policy, competition: evidence.competition,
    freezeReview: evidence.freezeReview, candidateEvidence: evidencePin, candidateId: evidence.candidateId, parentRevision: policy.parentRevision,
    candidateRevision: evidence.revision, diffSha256, build: building.producer!, engineerSessionId: native.sessionId, reviewerSessionId: reviewerIdentity.sessionId,
    engineerAuthRunId: route.auth.runId, parentAdjudicatorSessionId: adjudication.parentSessionId, worktree: route.worktree,
    requestedEngineer: { model: route.model, effort: route.effort, serviceTier: route.serviceTier },
    reviewerConfiguration: { requestedModel: reviewer.model, requestedEffort: reviewer.effort, observedModel: null, observedEffort: null, serviceTier: "not_exposed_by_client" },
    limitations: ["AGY effective all-tool confinement is unverified", "AGY descendant quiescence is not established", "AGY provider model/effort echoes are unavailable", "Candidate build full peak bytes are unmeasured", "Development source qualification does not accept a release or replace the 135-task parent"],
    validatorSources, validatedPins };
}
export async function writeDirectQualification(competition: CompetitionContract, evidence: Pin, outputPath: string): Promise<Pin> {
  const result = await inspectDirectCandidate(competition, evidence), policy = await pinned<ActorPolicy>(result.policy);
  if (!within(policy.outputRoot, outputPath)) throw new Error("Qualification must remain in selected external actor output");
  return immutable(outputPath, { ...result, bindingSha256: digest(result) } satisfies QualificationRecord);
}
export async function verifyQualifiedCandidate(_policy: ActorPolicy, _competition: CompetitionContract, _candidateId: string, _pin: Pin): Promise<DirectCandidateQualification> {
  // Refuse before opening a purported qualification. The current actor route has
  // no supported owner witness; a fabricated qualified record cannot bypass it.
  requireActorRuntimeQualification();
}
