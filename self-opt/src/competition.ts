import { Database } from "bun:sqlite";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdir } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { canonical, digest, fileHash, physicalPath, readJson, requireStorage } from "../../benchmark/src/io";
import { compare, scenarioFingerprint, validateMutationPaths, validateRun, type EvaluationRun } from "./evaluate";
import { boundedText, closed, hash, identity, revision, validateHypothesis, type Hypothesis } from "./proposals";
import { validateNativeCandidate, validateNativePolicy, type NativeQualification, type NativeReceiptPolicy } from "./native-receipts";
import type { DirectCandidateQualification } from "./actor-qualification";

const exec = promisify(execFile);
const COMPARATOR_PATH = fileURLToPath(new URL("./evaluate.ts", import.meta.url));
function own<T>(map: Record<string, T>, key: string): T | undefined { return Object.hasOwn(map, key) ? map[key] : undefined; }
export interface Pin { path: string; sha256: string; }
export type Purpose = "engineer" | "review" | "reader" | "judge" | "probe" | "repair";
const PURPOSES: Purpose[] = ["engineer", "review", "reader", "judge", "probe", "repair"];
export interface CompetitionContract {
  schema: "gptgrep.opt-competition.v1" | "gptgrep.opt-competition.v2" | "gptgrep.opt-competition.v3";
  competitionId: string;
  scope: "orchestration_qualification";
  parentRevision: string;
  scenarioFingerprint: string;
  cohort: string[];
  candidateIds: string[];
  mutationPaths: string[];
  comparator: Pin;
  deadlineAt: number;
  reservationLimits: Record<Purpose, number>;
  generations: 1;
  compositionAllowed: false;
  paidExecutionQualified: false;
  /** v2 freezes legacy owner receipts; v3 freezes the explicit direct-local actor policy. */
  nativePolicy?: Pin;
}
export interface CandidateEvidence {
  schema: "gptgrep.opt-candidate-evidence.v1";
  candidateId: string;
  parentRevision: string;
  revision: string;
  worktree: string;
  hypothesis: Hypothesis;
  build: Pin;
  checks: Pin;
  review: Pin;
  engineer: Pin;
  storage: Pin;
}
interface Lease { holder: string; token: number; expiresAt: number; }
interface Operation {
  id: string; purpose: Purpose; commandSha256: string;
  state: "reserved" | "submitted" | "unknown" | "cancel_requested" | "complete" | "failed" | "cancelled";
  submissionAttempted: boolean; externalJobId: string | null; terminal: Pin | null;
}
interface CandidateRecord {
  evidence: Pin; revision: string; diffSha256: string; incumbent: Pin | null; measured: Pin | null;
  measurementReport?: Pin;
  nativeReceiptQualification: "unavailable" | "qualified";
  qualification?: NativeQualification | DirectCandidateQualification;
}
export interface Decision {
  schema: "gptgrep.opt-development-decision.v1";
  competitionId: string; candidateId: string; parentRevision: string; scenarioFingerprint: string;
  comparison: ReturnType<typeof compare>;
  outcome: "development_nominee" | "incumbent_retained" | "blocked";
  nativeReceiptQualification: "unavailable" | "qualified";
  qualificationReason: string | null;
  nativeQualificationSha256: string | null;
  c3ParentChanged: false; releaseAccepted: false;
}
interface State {
  schema: "gptgrep.opt-competition-state.v1"; contractSha256: string; contract: CompetitionContract;
  version: number; fence: number; lease: Lease | null;
  operations: Record<string, Operation>; candidates: Record<string, CandidateRecord>;
  staged: Record<string, Decision>; committed: Record<string, Decision>;
}
export interface Fence { holder: string; token: number; }

export async function verifyPin(pin: Pin): Promise<void> {
  closed(pin, ["path", "sha256"], "pin"); hash(pin.sha256, "pin hash");
  if (typeof pin.path !== "string" || !isAbsolute(pin.path)) throw new Error("Absolute evidence path required");
  await physicalPath(pin.path);
  if (await fileHash(pin.path) !== pin.sha256) throw new Error("Evidence bytes changed");
}
async function pinned<T>(pin: Pin): Promise<T> { await verifyPin(pin); return readJson<T>(pin.path); }
export function validateCompetition(contract: CompetitionContract): void {
  closed(contract, ["schema", "competitionId", "scope", "parentRevision", "scenarioFingerprint", "cohort", "candidateIds", "mutationPaths", "comparator", "deadlineAt", "reservationLimits", "generations", "compositionAllowed", "paidExecutionQualified", ...(contract.schema !== "gptgrep.opt-competition.v1" ? ["nativePolicy"] : [])], "competition contract");
  if (!["gptgrep.opt-competition.v1", "gptgrep.opt-competition.v2", "gptgrep.opt-competition.v3"].includes(contract.schema) || contract.scope !== "orchestration_qualification" || contract.generations !== 1 ||
      contract.compositionAllowed !== false || contract.paidExecutionQualified !== false) throw new Error("Only source-qualified bounded orchestration is implemented");
  identity(contract.competitionId, "competition ID"); revision(contract.parentRevision); hash(contract.scenarioFingerprint, "scenario fingerprint");
  if (!Array.isArray(contract.cohort) || contract.cohort.length !== 4 || new Set(contract.cohort).size !== 4) throw new Error("Exactly four unique dev tasks required");
  contract.cohort.forEach(id => boundedText(id, "task ID", 256));
  if (!Array.isArray(contract.candidateIds) || contract.candidateIds.length !== 2 || new Set(contract.candidateIds).size !== 2) throw new Error("Exactly two candidate slots required");
  contract.candidateIds.forEach(id => identity(id, "candidate ID")); validateMutationPaths(contract.mutationPaths);
  closed(contract.comparator, ["path", "sha256"], "comparator pin"); hash(contract.comparator.sha256, "comparator hash");
  if (contract.comparator.path !== COMPARATOR_PATH) throw new Error("Comparator pin must identify the actual imported comparator");
  if (contract.schema !== "gptgrep.opt-competition.v1") {
    closed(contract.nativePolicy, ["path", "sha256"], "frozen native policy"); hash(contract.nativePolicy.sha256, "native policy digest");
    if (!isAbsolute(contract.nativePolicy.path)) throw new Error("Absolute native policy required");
  }
  if (!Number.isSafeInteger(contract.deadlineAt) || contract.deadlineAt <= 0) throw new Error("Finite deadline required");
  closed(contract.reservationLimits, PURPOSES, "reservation limits");
  for (const value of Object.values(contract.reservationLimits)) if (!Number.isSafeInteger(value) || value < 0 || value > 10000) throw new Error("Finite nonnegative reservation limits required");
  if (contract.reservationLimits.reader !== 12 || contract.reservationLimits.judge !== 12 || contract.reservationLimits.engineer < 2 || contract.reservationLimits.review < 2) throw new Error("The first competition requires 12 reader, 12 judge and two engineer/review slots");
}
async function git(worktree: string, args: string[]): Promise<string> {
  const { stdout } = await exec("git", ["-C", worktree, ...args], { maxBuffer: 4 * 1024 * 1024 }); return stdout;
}
async function verifyCandidate(contract: CompetitionContract, pin: Pin): Promise<CandidateRecord> {
  if (contract.schema === "gptgrep.opt-competition.v3") {
    const { inspectDirectCandidate } = await import("./actor-qualification");
    const candidate = await inspectDirectCandidate(contract, pin);
    return { evidence: pin, revision: candidate.candidateRevision, diffSha256: candidate.diffSha256,
      incumbent: null, measured: null, nativeReceiptQualification: "unavailable" };
  }
  const evidence = await pinned<CandidateEvidence>(pin);
  closed(evidence, ["schema", "candidateId", "parentRevision", "revision", "worktree", "hypothesis", "build", "checks", "review", "engineer", "storage"], "candidate evidence");
  if (evidence.schema !== "gptgrep.opt-candidate-evidence.v1" || !contract.candidateIds.includes(evidence.candidateId) || evidence.parentRevision !== contract.parentRevision || evidence.revision === evidence.parentRevision) throw new Error("Candidate identity mismatch");
  revision(evidence.revision); validateHypothesis(evidence.hypothesis);
  if (!isAbsolute(evidence.worktree)) throw new Error("Absolute worktree required"); await physicalPath(evidence.worktree);
  if ((await git(evidence.worktree, ["rev-parse", "HEAD"])).trim() !== evidence.revision || (await git(evidence.worktree, ["status", "--porcelain=v1", "--untracked-files=all"])).length) throw new Error("Candidate must be an actual clean committed worktree");
  await git(evidence.worktree, ["merge-base", "--is-ancestor", evidence.parentRevision, evidence.revision]);
  const paths = (await git(evidence.worktree, ["diff", "--name-only", "-z", "--no-renames", evidence.parentRevision, evidence.revision, "--"])).split("\0").filter(Boolean).sort();
  validateMutationPaths(paths);
  if (paths.some(path => !contract.mutationPaths.includes(path)) || canonical(paths) !== canonical([...evidence.hypothesis.mutationPaths].sort())) throw new Error("Actual diff escapes the frozen candidate allowlist/hypothesis");
  const diffSha256 = digest(await git(evidence.worktree, ["diff", "--binary", "--no-ext-diff", "--no-renames", evidence.parentRevision, evidence.revision, "--"]));
  await verifyPin(evidence.build);
  const checks = await pinned<any>(evidence.checks), review = await pinned<any>(evidence.review), engineer = await pinned<any>(evidence.engineer), storage = await pinned<any>(evidence.storage);
  closed(checks, ["schema", "candidateRevision", "commands"], "candidate checks");
  if (checks.schema !== "gptgrep.opt-candidate-checks.v1" || checks.candidateRevision !== evidence.revision || !Array.isArray(checks.commands) || !checks.commands.length) throw new Error("Bound actual candidate checks required");
  for (const command of checks.commands) {
    closed(command, ["argv", "exitCode", "terminal"], "candidate check");
    if (!Array.isArray(command.argv) || !command.argv.length || command.argv.some((x: unknown) => typeof x !== "string") || command.exitCode !== 0) throw new Error("Candidate check did not pass");
    await verifyPin(command.terminal);
  }
  closed(engineer, ["schema", "candidateRevision", "actorId", "model", "effort", "terminal"], "engineer evidence");
  if (engineer.schema !== "gptgrep.opt-engineer-evidence.v1" || engineer.candidateRevision !== evidence.revision) throw new Error("Bound native engineer evidence required");
  for (const key of ["actorId", "model", "effort"]) boundedText(engineer[key], key); await verifyPin(engineer.terminal);
  closed(review, ["schema", "status", "candidateRevision", "diffSha256", "reviewerId", "role", "substantiveChange", "terminal"], "candidate review");
  if (review.schema !== "gptgrep.opt-candidate-review.v1" || review.status !== "accepted" || review.candidateRevision !== evidence.revision || review.diffSha256 !== diffSha256 || review.role !== "independent_agy" || review.substantiveChange !== true || review.reviewerId === engineer.actorId) throw new Error("Actual independent AGY review must bind the substantive diff");
  boundedText(review.reviewerId, "reviewer ID"); await verifyPin(review.terminal);
  closed(storage, ["schema", "status", "worktree", "ownerReceipt"], "candidate storage admission");
  if (storage.schema !== "gptgrep.opt-candidate-storage.v1" || storage.status !== "admitted" || resolve(storage.worktree) !== resolve(evidence.worktree)) throw new Error("Exact candidate storage admission required");
  await verifyPin(storage.ownerReceipt);
  // These are source checks and pinned declarations, not native terminal/admission
  // validation. The later owner adapter must validate real receipt schemas before
  // this controller may commit a business decision. Never infer that from a hash.
  return { evidence: pin, revision: evidence.revision, diffSha256, incumbent: null, measured: null, nativeReceiptQualification: "unavailable" };
}

/** Local durable control state only. No method starts a worker/provider/Harbor process. */
export class CompetitionLedger {
  private constructor(private db: Database) {}
  static async open(path: string, contractPin?: Pin): Promise<CompetitionLedger> {
    await requireStorage(path, `${path}-wal`, `${path}-shm`); await physicalPath(path);
    await mkdir(dirname(path), { recursive: true });
    const db = new Database(path, { create: true });
    try {
      db.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS control (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events (id TEXT PRIMARY KEY, request_sha256 TEXT NOT NULL, result TEXT NOT NULL);");
      const ledger = new CompetitionLedger(db);
      if (!db.query("SELECT value FROM control WHERE id=1").get()) {
        if (!contractPin) throw new Error("A pinned frozen contract is required to initialize a ledger");
        const contract = await pinned<CompetitionContract>(contractPin); validateCompetition(contract); await verifyPin(contract.comparator);
        if (contract.schema === "gptgrep.opt-competition.v3") {
          const { validateActorPolicy } = await import("./actor-contracts"); validateActorPolicy(await pinned(contract.nativePolicy!), contract);
        } else if (contract.nativePolicy) validateNativePolicy(await pinned<NativeReceiptPolicy>(contract.nativePolicy), contract);
        const state: State = { schema: "gptgrep.opt-competition-state.v1", contractSha256: contractPin.sha256, contract,
          version: 0, fence: 0, lease: null, operations: {}, candidates: {}, staged: {}, committed: {} };
        db.query("INSERT INTO control(id,value) VALUES (1,?)").run(JSON.stringify(state));
      } else if (contractPin && (await pinned<CompetitionContract>(contractPin), ledger.snapshot().contractSha256 !== contractPin.sha256)) throw new Error("Ledger contract cannot be replaced");
      return ledger;
    } catch (error) { db.close(); throw error; }
  }
  close(): void { this.db.close(); }
  snapshot(): State { return JSON.parse((this.db.query("SELECT value FROM control WHERE id=1").get() as { value: string }).value); }
  committedDecision(candidateId: string): Decision {
    identity(candidateId, "candidate ID"); const decision = own(this.snapshot().committed, candidateId);
    if (!decision) throw new Error("No durable decision for this candidate"); return decision;
  }
  stagedDecision(candidateId: string): Decision {
    identity(candidateId, "candidate ID"); const decision = own(this.snapshot().staged, candidateId);
    if (!decision) throw new Error("No staged decision for this candidate"); return decision;
  }
  private change<T>(version: number, eventId: string, request: unknown, mutate: (state: State) => T): T {
    identity(eventId, "event ID"); const requestSha256 = digest(request);
    return this.db.transaction(() => {
      const previous = this.db.query("SELECT request_sha256,result FROM events WHERE id=?").get(eventId) as { request_sha256: string; result: string } | null;
      if (previous) { if (previous.request_sha256 !== requestSha256) throw new Error("Idempotency key reused with different content"); return JSON.parse(previous.result) as T; }
      const state = this.snapshot();
      if (state.version !== version) throw new Error("State compare-and-swap conflict");
      const result = mutate(state); state.version++;
      this.db.query("UPDATE control SET value=? WHERE id=1").run(JSON.stringify(state));
      this.db.query("INSERT INTO events(id,request_sha256,result) VALUES (?,?,?)").run(eventId, requestSha256, JSON.stringify(result));
      return result;
    }).immediate();
  }
  private assertLease(state: State, fence: Fence): void {
    closed(fence, ["holder", "token"], "lease fence"); identity(fence.holder, "lease holder");
    if (!Number.isSafeInteger(fence.token) || fence.token < 1) throw new Error("Invalid lease fencing token");
    if (!state.lease || state.lease.holder !== fence.holder || state.lease.token !== fence.token || state.lease.expiresAt <= Date.now()) throw new Error("Missing, expired or fenced controller lease");
  }
  acquire(version: number, eventId: string, holder: string, expiresAt: number): Lease {
    identity(holder, "lease holder");
    return this.change(version, eventId, { holder, expiresAt }, state => {
      if (!Number.isSafeInteger(expiresAt) || expiresAt <= Date.now() || expiresAt > Date.now() + 300000) throw new Error("Controller/recovery leases are bounded to five minutes");
      if (state.lease && state.lease.expiresAt > Date.now() && state.lease.holder !== holder) throw new Error("Controller lease held by another owner");
      state.lease = { holder, token: ++state.fence, expiresAt }; return state.lease;
    });
  }
  reserve(version: number, eventId: string, fence: Fence, operationId: string, purpose: Purpose, commandSha256: string): Operation {
    identity(operationId, "operation ID"); hash(commandSha256, "command hash");
    return this.change(version, eventId, { fence, operationId, purpose, commandSha256 }, state => {
      this.assertLease(state, fence);
      if (Date.now() >= state.contract.deadlineAt || !PURPOSES.includes(purpose)) throw new Error("Operation outside finite budget");
      if (own(state.operations, operationId)) throw new Error("Existing reservation requires reconciliation, never resubmission");
      if (Object.values(state.operations).filter(o => o.purpose === purpose).length >= state.contract.reservationLimits[purpose]) throw new Error("Reservation budget exhausted");
      return state.operations[operationId] = { id: operationId, purpose, commandSha256, state: "reserved", submissionAttempted: false, externalJobId: null, terminal: null };
    });
  }
  markOperation(version: number, eventId: string, fence: Fence, operationId: string, status: "submitted" | "unknown" | "cancel_requested", externalJobId: string | null): Operation {
    identity(operationId, "operation ID");
    return this.change(version, eventId, { fence, operationId, status, externalJobId }, state => {
      this.assertLease(state, fence); const op = own(state.operations, operationId);
      if (!op || ["complete", "failed", "cancelled"].includes(op.state)) throw new Error("No open operation to transition");
      if (!["submitted", "unknown", "cancel_requested"].includes(status)) throw new Error("Unknown operation transition");
      if (status === "submitted" && op.state !== "reserved") throw new Error("An uncertain operation cannot be submitted again");
      if (externalJobId !== null) boundedText(externalJobId, "external job ID", 256);
      if (status === "submitted" && externalJobId === null) throw new Error("Submitted operation requires actual job identity");
      if (op.externalJobId && externalJobId !== op.externalJobId) throw new Error("External job identity changed");
      op.state = status; op.externalJobId = externalJobId;
      if (status !== "cancel_requested") op.submissionAttempted = true;
      return op;
    });
  }
  async reconcile(version: number, eventId: string, fence: Fence, operationId: string, terminalPin: Pin): Promise<Operation> {
    identity(operationId, "operation ID");
    const terminal = await pinned<any>(terminalPin);
    closed(terminal, ["schema", "operationId", "commandSha256", "externalJobId", "submission", "disposition", "ownerTerminal"], "operation terminal");
    if (terminal.schema !== "gptgrep.opt-operation-terminal.v1" || terminal.operationId !== operationId || !["complete", "failed", "cancelled"].includes(terminal.disposition)) throw new Error("Invalid terminal reconciliation");
    await verifyPin(terminal.ownerTerminal);
    return this.change(version, eventId, { fence, operationId, terminalPin }, state => {
      this.assertLease(state, fence); const op = own(state.operations, operationId);
      if (!op || op.terminal || op.commandSha256 !== terminal.commandSha256) throw new Error("Terminal does not match the reserved operation");
      if (terminal.submission === "never_dispatched") {
        if (op.submissionAttempted || op.externalJobId !== null || terminal.externalJobId !== null || terminal.disposition !== "cancelled") throw new Error("An attempted/uncertain dispatch cannot be claimed never dispatched");
      } else if (terminal.submission !== "observed" || !terminal.externalJobId || (op.externalJobId && op.externalJobId !== terminal.externalJobId)) throw new Error("Observed terminal requires the same real external job ID");
      op.state = terminal.disposition; op.externalJobId = terminal.externalJobId; op.terminal = terminalPin; return op;
    });
  }
  async register(version: number, eventId: string, fence: Fence, candidateId: string, evidence: Pin): Promise<CandidateRecord> {
    identity(candidateId, "candidate ID");
    const snapshot = this.snapshot(), candidate = await verifyCandidate(snapshot.contract, evidence);
    const body = await pinned<CandidateEvidence>(evidence);
    if (body.candidateId !== candidateId) throw new Error("Candidate slot differs from evidence");
    return this.change(version, eventId, { fence, candidateId, evidence }, state => {
      this.assertLease(state, fence);
      if (own(state.candidates, candidateId)) throw new Error("Candidate slot is immutable");
      return state.candidates[candidateId] = candidate;
    });
  }
  private async nativeQualification(state: State, candidateId: string, evidence: Pin): Promise<NativeQualification | DirectCandidateQualification> {
    if (state.contract.schema === "gptgrep.opt-competition.v1" || !state.contract.nativePolicy) throw new Error("Legacy v1 evidence cannot be promoted to native qualification");
    const record = own(state.candidates, candidateId);
    if (!record) throw new Error("Registered immutable candidate required");
    await verifyCandidate(state.contract, record.evidence);
    if (state.contract.schema === "gptgrep.opt-competition.v3") {
      const { verifyQualifiedCandidate } = await import("./actor-qualification");
      const qualification = await verifyQualifiedCandidate(await pinned(state.contract.nativePolicy), state.contract, candidateId, evidence);
      if (canonical(qualification.candidateEvidence) !== canonical(record.evidence)) throw new Error("Qualification differs from the registered direct-local candidate");
      return qualification;
    }
    const candidate = await pinned<CandidateEvidence>(record.evidence), engineer = await pinned<any>(candidate.engineer), review = await pinned<any>(candidate.review), checks = await pinned<any>(candidate.checks), storage = await pinned<any>(candidate.storage);
    return validateNativeCandidate({ policyPin: state.contract.nativePolicy, evidencePin: evidence, candidateEvidencePin: record.evidence,
      competition: state.contract, candidate: { candidateId, revision: record.revision, diffSha256: record.diffSha256, worktree: candidate.worktree,
        engineerActorId: engineer.actorId, engineerModel: engineer.model, engineerEffort: engineer.effort, engineerTerminal: engineer.terminal,
        reviewerActorId: review.reviewerId, reviewTerminal: review.terminal, storageReceipt: storage.ownerReceipt, build: candidate.build, checks: candidate.checks,
        checkArgv: checks.commands.map((c: any) => c.argv), checkTerminals: checks.commands.map((c: any) => c.terminal) } });
  }
  async qualify(version: number, eventId: string, fence: Fence, candidateId: string, evidence: Pin): Promise<NativeQualification | DirectCandidateQualification> {
    identity(candidateId, "candidate ID");
    const qualification = await this.nativeQualification(this.snapshot(), candidateId, evidence);
    return this.change(version, eventId, { fence, candidateId, evidence, qualification }, state => {
      this.assertLease(state, fence); const candidate = own(state.candidates, candidateId);
      if (!candidate || candidate.qualification || own(state.staged, candidateId) || own(state.committed, candidateId)) throw new Error("Native qualification is immutable and must precede decisions");
      for (const other of Object.values(state.candidates)) if (other.qualification &&
          (other.qualification.engineerSessionId === qualification.engineerSessionId || other.qualification.engineerAuthRunId === qualification.engineerAuthRunId ||
           other.qualification.reviewerSessionId === qualification.engineerSessionId || other.qualification.engineerSessionId === qualification.reviewerSessionId ||
           other.qualification.reviewerSessionId === qualification.reviewerSessionId)) throw new Error("Candidate native engineer/reviewer identities are not independent");
      candidate.qualification = qualification; candidate.nativeReceiptQualification = qualification.status; return qualification;
    });
  }
  async attachResults(version: number, eventId: string, fence: Fence, candidateId: string, incumbent: Pin, measured: Pin): Promise<CandidateRecord> {
    identity(candidateId, "candidate ID");
    const state = this.snapshot(), record = own(state.candidates, candidateId);
    if (state.contract.schema === "gptgrep.opt-competition.v3") throw new Error("Direct-local candidates require the verified complete fresh measurement report");
    if (!record) throw new Error("Reviewed candidate required before measurement");
    await verifyCandidate(state.contract, record.evidence);
    const before = await pinned<EvaluationRun>(incumbent), after = await pinned<EvaluationRun>(measured);
    this.checkRuns(state, candidateId, before, after);
    return this.change(version, eventId, { fence, candidateId, incumbent, measured }, current => {
      this.assertLease(current, fence); const candidate = current.candidates[candidateId];
      if (candidate.incumbent || candidate.measured) throw new Error("Measured results are immutable");
      for (const other of Object.values(current.candidates)) if (other.incumbent && canonical(other.incumbent) !== canonical(incumbent)) throw new Error("Both comparisons must use the same fresh incumbent artifact");
      candidate.incumbent = incumbent; candidate.measured = measured; return candidate;
    });
  }
  async attachMeasurements(version: number, eventId: string, fence: Fence, reportPin: Pin): Promise<Record<string, CandidateRecord>> {
    const state = this.snapshot();
    if (state.contract.schema !== "gptgrep.opt-competition.v3") throw new Error("Fresh direct-local measurements require a version 3 competition");
    const { verifyMeasurementReport } = await import("./competition-measurement");
    const measured = await verifyMeasurementReport(reportPin, state.contract);
    for (const candidateId of state.contract.candidateIds) {
      const candidate = own(state.candidates, candidateId);
      if (!candidate?.qualification || candidate.nativeReceiptQualification !== "qualified" ||
          canonical(candidate.qualification.evidence) !== canonical(measured.qualifiedCandidatePins[candidateId])) throw new Error("Both actual qualified candidates must precede measured result attachment");
      this.checkRuns(state, candidateId, await pinned(measured.runs.incumbent!), await pinned(measured.runs[candidateId]!));
    }
    return this.change(version, eventId, { fence, reportPin, runs: measured.runs }, current => {
      this.assertLease(current, fence);
      for (const candidateId of current.contract.candidateIds) {
        const candidate = current.candidates[candidateId]!;
        if (candidate.incumbent || candidate.measured || candidate.measurementReport) throw new Error("Fresh measured results are immutable");
        candidate.incumbent = measured.runs.incumbent!; candidate.measured = measured.runs[candidateId]!; candidate.measurementReport = reportPin;
      }
      return current.candidates;
    });
  }
  private checkRuns(state: State, candidateId: string, before: EvaluationRun, after: EvaluationRun): void {
    validateRun(before); validateRun(after);
    const candidate = own(state.candidates, candidateId);
    if (!candidate || before.revision !== state.contract.parentRevision || after.revision !== candidate.revision || after.parentRevision !== before.revision || before.split !== "dev" || after.split !== "dev" ||
        scenarioFingerprint(before) !== state.contract.scenarioFingerprint || scenarioFingerprint(after) !== state.contract.scenarioFingerprint ||
        canonical([...before.cohort].sort()) !== canonical([...state.contract.cohort].sort()) || canonical([...after.cohort].sort()) !== canonical([...state.contract.cohort].sort())) throw new Error("Measured source/scenario/cohort differs from the frozen contract");
  }
  async evaluate(candidateId: string): Promise<Decision> {
    identity(candidateId, "candidate ID");
    const state = this.snapshot(), candidate = own(state.candidates, candidateId);
    if (!candidate?.incumbent || !candidate.measured) throw new Error("Actual pinned incumbent and candidate results are required; no live fallback exists");
    await verifyPin(state.contract.comparator); await verifyCandidate(state.contract, candidate.evidence);
    let qualification: NativeQualification | DirectCandidateQualification | null = null;
    if (candidate.qualification) {
      qualification = await this.nativeQualification(state, candidateId, candidate.qualification.evidence);
      if (canonical(qualification) !== canonical(candidate.qualification)) throw new Error("Native qualification evidence or validator changed");
    }
    if (state.contract.schema === "gptgrep.opt-competition.v3") {
      if (!candidate.measurementReport) throw new Error("The direct-local decision has no actual fresh measurement report");
      const { verifyMeasurementReport } = await import("./competition-measurement");
      const measured = await verifyMeasurementReport(candidate.measurementReport, state.contract);
      if (canonical(measured.runs.incumbent) !== canonical(candidate.incumbent) || canonical(measured.runs[candidateId]) !== canonical(candidate.measured) ||
          canonical(measured.qualifiedCandidatePins[candidateId]) !== canonical(candidate.qualification?.evidence)) throw new Error("Fresh measurement or qualified candidate changed before comparison");
    }
    const before = await pinned<EvaluationRun>(candidate.incumbent), after = await pinned<EvaluationRun>(candidate.measured);
    this.checkRuns(state, candidateId, before, after); const comparison = compare(before, after);
    return { schema: "gptgrep.opt-development-decision.v1", competitionId: state.contract.competitionId, candidateId,
      parentRevision: state.contract.parentRevision, scenarioFingerprint: state.contract.scenarioFingerprint, comparison,
      outcome: qualification?.status !== "qualified" || !comparison.incumbent.comparable || !comparison.candidate.comparable ? "blocked" : comparison.eligibleNextParent ? "development_nominee" : "incumbent_retained",
      nativeReceiptQualification: qualification?.status ?? "unavailable",
      qualificationReason: qualification?.status === "qualified" ? null : qualification?.reasons.join(",") ?? "native_engineer_review_storage_terminals_not_yet_validated",
      nativeQualificationSha256: qualification?.bindingSha256 ?? null, c3ParentChanged: false, releaseAccepted: false };
  }
  async stageDecision(version: number, eventId: string, fence: Fence, candidateId: string): Promise<Decision> {
    identity(candidateId, "candidate ID");
    const decision = await this.evaluate(candidateId);
    return this.change(version, eventId, { fence, candidateId, decision }, state => {
      this.assertLease(state, fence);
      if (own(state.staged, candidateId) || own(state.committed, candidateId)) throw new Error("Decision already persisted");
      state.staged[candidateId] = decision; return decision;
    });
  }
  async commitDecision(version: number, eventId: string, fence: Fence, candidateId: string, expectedDecisionSha256: string): Promise<Decision> {
    identity(candidateId, "candidate ID");
    hash(expectedDecisionSha256, "decision digest");
    const observed = await this.evaluate(candidateId);
    if (digest(observed) !== expectedDecisionSha256) throw new Error("Decision/evidence changed before durable commitment");
    return this.change(version, eventId, { fence, candidateId, expectedDecisionSha256 }, state => {
      this.assertLease(state, fence); const decision = own(state.staged, candidateId);
      if (!decision || own(state.committed, candidateId) || digest(decision) !== expectedDecisionSha256 || decision.parentRevision !== state.contract.parentRevision || decision.scenarioFingerprint !== state.contract.scenarioFingerprint) throw new Error("Decision compare-and-swap failed");
      if (decision.outcome === "blocked" || !decision.comparison.incumbent.comparable || !decision.comparison.candidate.comparable) throw new Error("Unknown measurements cannot complete a decision");
      if (decision.nativeReceiptQualification === "unavailable") throw new Error("Actual native receipt validation remains unavailable; source checks cannot commit a business decision");
      if (Object.values(state.operations).some(op => !["complete", "failed", "cancelled"].includes(op.state))) throw new Error("Unreconciled operation blocks decision commit");
      state.committed[candidateId] = decision; return decision;
    });
  }
  assertPaidExecutionQualified(): never { throw new Error("Paid C4 execution remains unqualified: use the later independently accepted native engineer/AGY/Harbor adapter"); }
}
