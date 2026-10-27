/** Read-only C4 preflight. Native job binding is unavailable, so this module cannot launch. */
import { isAbsolute } from "node:path";
import { validateCampaignEnvelope, type CampaignEnvelopeV2, type CampaignPurpose } from "../../benchmark/src/campaign";
import { verifyBatchAcceptance, type ProcessCommand } from "../../benchmark/src/controller";
import { canonical, digest, readJson } from "../../benchmark/src/io";
import { validateCompetition, verifyPin, type CompetitionContract, type Pin } from "./competition";
import { scenarioFingerprint, summarize, type EvaluationRun } from "./evaluate";
import { validateNativePolicy, verifyReceiptPin, type NativeReceiptPolicy } from "./native-receipts";
import { boundedText, closed, identity } from "./proposals";

type C4Purpose = Extract<CampaignPurpose, "native_engineer" | "agy" | "prepare_context" | "c4_reader" | "verifier" | "c4_judge">;
interface ActionSlot { id: string; purpose: C4Purpose; candidateId: string; taskId: string | null }
export interface CompetitionDispatchPlan {
  schema: "gptgrep.opt-dispatch-plan.v1";
  competition: Pin; campaign: Pin;
  baseline: { manifest: Pin; report: Pin; review: Pin };
  ownerRuntime: { python: Pin; cli: Pin; implementation: Pin };
  outputRoot: string; actions: ActionSlot[];
}
export interface CompetitionDispatchTicket {
  schema: "gptgrep.opt-dispatch-ticket.v1";
  plan: Pin; actionId: string; command: ProcessCommand; ownerPlan: Pin;
  workloadArgv: string[];
}
const PURPOSES: C4Purpose[] = ["native_engineer", "agy", "prepare_context", "c4_reader", "verifier", "c4_judge"];
async function pinned<T = any>(pin: Pin): Promise<T> { await verifyPin(pin); return readJson<T>(pin.path); }

export function validateActionSlots(plan: CompetitionDispatchPlan, competition: CompetitionContract): void {
  closed(plan, ["schema", "competition", "campaign", "baseline", "ownerRuntime", "outputRoot", "actions"], "C4 dispatch plan");
  closed(plan.baseline, ["manifest", "report", "review"], "C3 baseline evidence");
  closed(plan.ownerRuntime, ["python", "cli", "implementation"], "frozen owner runtime");
  if (plan.schema !== "gptgrep.opt-dispatch-plan.v1" || !isAbsolute(plan.outputRoot) || !Array.isArray(plan.actions) || plan.actions.length < 40 || plan.actions.length > 83 ||
      new Set(plan.actions.map(a => a.id)).size !== plan.actions.length) throw new Error("C4 dispatch requires a finite immutable slot set");
  const expected = new Set<string>();
  for (const candidate of competition.candidateIds) for (const purpose of ["native_engineer", "agy"]) expected.add(`${purpose}:${candidate}:`);
  for (const candidate of ["incumbent", ...competition.candidateIds]) for (const task of competition.cohort) for (const purpose of ["c4_reader", "verifier", "c4_judge"]) expected.add(`${purpose}:${candidate}:${task}`);
  const observed = new Set<string>();
  for (const action of plan.actions) {
    closed(action, ["id", "purpose", "candidateId", "taskId"], "C4 action slot"); identity(action.id, "C4 action ID");
    if (!PURPOSES.includes(action.purpose) || !["incumbent", ...competition.candidateIds].includes(action.candidateId)) throw new Error("Action is outside the frozen C4 roles");
    if (action.purpose === "prepare_context") {
      if (action.taskId !== null && !competition.cohort.includes(action.taskId)) throw new Error("PREPARE cannot replace a task");
      continue;
    }
    const key = `${action.purpose}:${action.candidateId}:${action.taskId ?? ""}`;
    if (!expected.has(key) || observed.has(key)) throw new Error("Duplicate, substituted or extra C4 action"); observed.add(key);
  }
  if (observed.size !== expected.size) throw new Error("Exactly two engineers, two AGY reviews, twelve readers, twelve verifiers and twelve judges are required");
}

/** This consumes existing C2 review formats, never a declared synthetic baseline success. */
export async function verifyC3Gate(plan: CompetitionDispatchPlan, competition: CompetitionContract): Promise<void> {
  await verifyPin(plan.baseline.manifest);
  const { report, runtime } = await verifyBatchAcceptance(plan.baseline.manifest, plan.baseline.report, plan.baseline.review);
  const manifest = await pinned(plan.baseline.manifest);
  if (canonical(manifest.envelope) !== canonical(plan.campaign)) throw new Error("C3 and C4 must use the same frozen campaign envelope");
  if (manifest.schema !== "gptgrep.paired-controller.v2" || !Array.isArray(manifest.taskIds) || manifest.taskIds.length !== 135 || new Set(manifest.taskIds).size !== 135 ||
      report.cumulativeTasks !== 135 || report.enrolledTasks !== 135 || report.error !== null || runtime.status !== "qualified" || runtime.unavailableCapabilities.length !== 0 ||
      canonical(Object.keys(report.runs).sort()) !== canonical(["bare", "gptgrep"])) throw new Error("C3 requires the final independently accepted real 135-task baseline");
  let incumbent: EvaluationRun | undefined, baselineScenario: string | undefined;
  for (const arm of ["bare", "gptgrep"] as const) {
    const run = await pinned<EvaluationRun>(report.runs[arm]), score = summarize(run);
    if (run.arm !== arm || run.split !== "dev" || score.enrolled !== 135 || !score.comparable || score.complete < 1 ||
        canonical([...run.cohort].sort()) !== canonical([...manifest.taskIds].sort())) throw new Error("C3 baseline has unknown, missing or nonmeaningful measurements");
    if (arm === "bare") baselineScenario = scenarioFingerprint(run);
    else {
      if (scenarioFingerprint(run) !== baselineScenario) throw new Error("C3 paired arms have different frozen scenarios");
      incumbent = run;
    }
  }
  if (!incumbent || incumbent.revision !== competition.parentRevision || competition.cohort.some(task => !incumbent!.cohort.includes(task)) ||
      scenarioFingerprint({ ...incumbent, cohort: competition.cohort }) !== competition.scenarioFingerprint) throw new Error("C4 parent/scenario/tasks differ from the accepted C3 baseline");
}

async function gates(planPin: Pin, reviewPin: Pin) {
  const plan = await pinned<CompetitionDispatchPlan>(planPin), competition = await pinned<CompetitionContract>(plan.competition);
  validateCompetition(competition);
  if (competition.schema !== "gptgrep.opt-competition.v2") throw new Error("Legacy source-only competitions cannot dispatch");
  validateActionSlots(plan, competition);
  const campaign = await pinned<CampaignEnvelopeV2>(plan.campaign); validateCampaignEnvelope(campaign);
  if (Date.now() >= Math.min(competition.deadlineAt, Date.parse(campaign.campaignDeadline))) throw new Error("C4 competition or campaign deadline has expired");
  const policy = await pinned<NativeReceiptPolicy>(competition.nativePolicy!); validateNativePolicy(policy, competition);
  for (const candidate of policy.candidates) {
    for (const role of [candidate.engineer, candidate.reviewer]) {
      await verifyReceiptPin(role.binary, true); await verifyReceiptPin(role.producer);
    }
    for (const pin of [...candidate.reviewer.owner.sources, candidate.reviewer.owner.resultSchema]) await verifyReceiptPin(pin);
  }
  await verifyReceiptPin(plan.ownerRuntime.python, true);
  await verifyReceiptPin(plan.ownerRuntime.cli); await verifyReceiptPin(plan.ownerRuntime.implementation);
  const review = await pinned(reviewPin);
  closed(review, ["schema", "status", "plan", "competition", "campaign", "baselineReview", "reviewerSessionId"], "O4 dispatch freeze review");
  if (review.schema !== "gptgrep.opt-dispatch-review.v1" || review.status !== "accepted_for_first_live_qualification" ||
      canonical(review.plan) !== canonical(planPin) || canonical(review.competition) !== canonical(plan.competition) || canonical(review.campaign) !== canonical(plan.campaign) ||
      canonical(review.baselineReview) !== canonical(plan.baseline.review)) throw new Error("C4 requires an independent review of the exact O4 plan and common campaign");
  boundedText(review.reviewerSessionId, "O4 reviewer identity", 256);
  for (const purpose of PURPOSES) {
    const count = plan.actions.filter(a => a.purpose === purpose).length;
    if (count > campaign.allowances[purpose].maxAdmissions) throw new Error("C4 slots exceed the common campaign allowance");
  }
  await verifyC3Gate(plan, competition);
  return { plan, competition, campaign };
}

export interface DispatchOptions { plan: Pin; freezeReview: Pin; ticket: Pin; ticketReview: Pin }

/** Validates the frozen route without creating storage, reservations, leases or processes. */
export async function preflightCompetitionAction(options: DispatchOptions) {
  const { plan, competition, campaign } = await gates(options.plan, options.freezeReview);
  const ticket = await pinned<CompetitionDispatchTicket>(options.ticket);
  closed(ticket, ["schema", "plan", "actionId", "command", "ownerPlan", "workloadArgv"], "C4 owner dispatch ticket");
  if (ticket.schema !== "gptgrep.opt-dispatch-ticket.v1" || canonical(ticket.plan) !== canonical(options.plan)) throw new Error("Dispatch ticket belongs to another frozen plan");
  const action = plan.actions.find(a => a.id === ticket.actionId);
  if (!action) throw new Error("Dispatch ticket is outside the immutable action slots");
  const review = await pinned(options.ticketReview);
  closed(review, ["schema", "status", "ticket", "freezeReview", "reviewerSessionId"], "exact owner ticket review");
  if (review.schema !== "gptgrep.opt-dispatch-ticket-review.v1" || review.status !== "accepted_for_owner_launch" || canonical(review.ticket) !== canonical(options.ticket) ||
      canonical(review.freezeReview) !== canonical(options.freezeReview)) throw new Error("The exact current owner ticket has not been reviewed");
  boundedText(review.reviewerSessionId, "owner ticket reviewer identity", 256);
  const command = ticket.command;
  closed(command, ["executable", "inputs", "argv", "cwd", "timeoutMs", "maxOutputBytes"], "bounded owner command");
  const allowance = campaign.allowances[action.purpose];
  if (!Number.isSafeInteger(command.timeoutMs) || command.timeoutMs < 1 || command.timeoutMs + 2000 > allowance.deadlineMs ||
      !Number.isSafeInteger(command.maxOutputBytes) || command.maxOutputBytes < 1 || command.maxOutputBytes > 8 * 1024 * 1024 ||
      !isAbsolute(command.cwd) || !Array.isArray(command.argv) || command.argv.length > 280 || command.argv.some(arg => typeof arg !== "string" || arg.includes("\0")) ||
      !Array.isArray(command.inputs) || !command.inputs.some(pin => canonical(pin) === canonical(ticket.ownerPlan))) throw new Error("Owner command exceeds its frozen admission or omits its exact owner plan");
  if (Date.now() + allowance.deadlineMs > Math.min(competition.deadlineAt, Date.parse(campaign.campaignDeadline))) throw new Error("C4 deadline cannot fit the complete action and cleanup allowance");
  await verifyReceiptPin(command.executable, true); for (const pin of command.inputs) await verifyPin(pin);
  const owner = await pinned(ticket.ownerPlan);
  if (owner.schema !== "environments-harness.storage-plan.v1" || owner.digest !== digest(Object.fromEntries(Object.entries(owner).filter(([key]) => key !== "digest"))) ||
      owner.implementation_sha256 !== plan.ownerRuntime.implementation.sha256 || owner.cwd !== command.cwd) throw new Error("Actual unchanged owner storage plan required");
  const argv = ticket.workloadArgv;
  if (!Array.isArray(argv) || !argv.length || argv.length > 256 || argv.some(arg => typeof arg !== "string" || arg.includes("\0")) ||
      argv.reduce((bytes, arg) => bytes + Buffer.byteLength(arg), 0) > 65536 || !isAbsolute(argv[0]!)) throw new Error("Exact bounded workload argv is required");
  const payload: Pin = { path: owner.command?.executable, sha256: owner.command?.executable_sha256 };
  await verifyReceiptPin(payload, true);
  const identity = (executable: Pin, argv: string[]) => ({ executable: executable.path, executable_sha256: executable.sha256,
    argv_sha256: digest(argv), argument_count: argv.length, arguments: "not_retained" });
  if (canonical(owner.command) !== canonical(identity(payload, argv)) || payload.path !== argv[0] ||
      canonical(owner.bootstrap_python) !== canonical(identity(plan.ownerRuntime.python, [plan.ownerRuntime.python.path]))) throw new Error("Owner command identity differs from the exact workload or frozen interpreter");
  const expectedArgv = ["-B", plan.ownerRuntime.cli.path, "storage", "run", "--plan", ticket.ownerPlan.path, "--expect-digest", owner.digest, "--yes", "--", ...argv];
  if (canonical(command.executable) !== canonical(plan.ownerRuntime.python) || canonical(command.argv) !== canonical(expectedArgv) ||
      ![plan.ownerRuntime.cli, plan.ownerRuntime.implementation, payload].every(expected => command.inputs.some(pin => canonical(pin) === canonical(expected)))) throw new Error("Only the exact frozen owner front door can dispatch the workload");
  await verifyPin({ path: owner.policy, sha256: owner.policy_sha256 });
  const workload = await pinned({ path: owner.workload, sha256: owner.workload_sha256 });
  if (canonical(workload) !== canonical(owner.workload_declaration)) throw new Error("Owner workload declaration changed");
  // Pins and review do not bind an engineer job, review isolation or reader/verifier/judge episode.
  // No caller-controlled flag or accepted wrapper can enable these absent native adapters.
  return { sourcePreflight: "ready" as const, actorAdmission: "unavailable" as const, measurementAdmission: "unavailable" as const,
    action, reasons: ["native_job_binding_unavailable", "phase_dependency_binding_unavailable"], charged: false as const, launched: false as const };
}

/** A future runnable adapter must add phase dependencies, one-in-flight exclusion and charge before launch together. */
export async function dispatchCompetitionAction(options: DispatchOptions): Promise<never> {
  const result = await preflightCompetitionAction(options);
  throw new Error(`C4 native dispatch unavailable before charge or launch: ${result.reasons.join(", ")}`);
}
