import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type { DependencyRelease, SourceDocument, TaskDependency } from "./dataset";
import { canonical, digest, fileHash, physicalPath, readJson, requireStorage } from "./io";
import { verifiedSource } from "./source-verification";

export type RawState = "verified" | "recovery_pending" | "identity_hold" | "frozen_unavailable";
export type OcrState = "not_required" | "pending" | "partial" | "verified" | "failed" | "blocked_raw_dependency";
export interface Pin { path: string; sha256: string; }
export interface RecoveryRoute {
  kind: "original" | "official_equivalent" | "mirror" | "archive";
  outcome: "exhausted" | "not_applicable";
  reason: string;
  evidence: Pin[];
}
export interface FreezeDecision {
  releaseRevision: string; documentId: string; expectedIdentitySha256: string;
  reason: string; recordedAt: string; routes: RecoveryRoute[]; review: Pin;
}
export interface DocumentAvailability {
  documentId: string; expectedIdentitySha256: string; rawState: RawState;
  rawSha256: string | null; reason: string; evidence: Pin[];
  /** Acquisition's exact source verification record, not merely status=ready. */
  verification: Record<string, unknown> | null;
  freeze: FreezeDecision | null;
}
export interface ArmProfile {
  id: string; arm: string; implementation: "implemented" | "planned";
  requiresOcr: boolean; preprocessingProfileSha256: string | null;
}
export interface PreprocessingAvailability {
  documentId: string; rawSha256: string; profileSha256: string;
  state: OcrState; pages: number[]; artifacts: Pin[]; provenance: Pin[];
}
export interface AssessmentInput {
  revision: string; generation: number; recordedAt: string; supersedes: Pin | null;
  release: DependencyRelease; documents: DocumentAvailability[];
  profiles: ArmProfile[]; preprocessing: PreprocessingAvailability[];
}
export interface TaskAvailability {
  taskId: string; taskType: TaskDependency["taskType"]; contextId: string; groupId: string;
  requiredDocumentIds: string[]; rawState: RawState; blockingDocumentIds: string[];
  reasonCodes: string[]; documentStateReferences: Record<string, string>;
  assessmentRevision: string;
  armAvailability: Record<string, { admitted: boolean; profileSha256: string; reasons: string[]; ocr: Record<string, OcrState> }>;
}
export interface AvailabilityAssessment extends AssessmentInput {
  schema: "gptgrep.task-availability.v1"; dependencySha256: string; tasks: TaskAvailability[];
}
export interface ControllerAcceptance {
  schema: "gptgrep.availability-controller.v1"; status: "accepted";
  current: Pin; assessment: Pin; generation: number; assessmentRevision: string;
  taskIds: string[]; profileIds: string[]; sourcePins: Pin[];
  storage: { runId: string; workloadSha256: string; cwd: string };
  controllerArgv: string[]; childArgv: string[][];
  runtime: Pin | null; harborExport: Pin | null;
}
export interface AdmissionReference { acceptance: Pin; profileId: string; }
export interface Admission { assessment: AvailabilityAssessment; acceptance: ControllerAcceptance; reference: AdmissionReference; }

export function closed(value: unknown, fields: string[], label: string): asserts value is Record<string, any> {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).sort().join("\0") !== [...fields].sort().join("\0")) throw new Error(`${label}: missing or unknown fields`);
}
function nonempty(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !value.trim()) throw new Error(`${label}: nonempty string required`);
}
function hash(value: unknown): asserts value is string {
  if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value)) throw new Error("Exact SHA-256 required");
}
function unique(values: string[], label: string): void {
  if (!Array.isArray(values) || values.some(v => typeof v !== "string" || !v) || new Set(values).size !== values.length) throw new Error(`${label}: unique identifiers required`);
}
function pin(value: Pin): void { closed(value, ["path", "sha256"], "pin"); nonempty(value.path, "pin path"); hash(value.sha256); }
export async function verifyPin(value: Pin): Promise<void> {
  pin(value); await physicalPath(value.path);
  if (await fileHash(value.path) !== value.sha256) throw new Error("Pinned input changed");
}
/** JSON.parse alone silently accepts duplicate keys in launch/acceptance JSON. */
export async function readExactJson<T = any>(path: string): Promise<T> {
  await readJson(path); // Bounded regular-file check.
  const bytes = await readFile(path, "utf8"), tokens = bytes.match(/"(?:[^"\\]|\\.)*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null|[{}\[\],:]/g) ?? [];
  let cursor = 0;
  const visit = (): void => {
    const token = tokens[cursor++];
    if (token === "{") {
      const keys = new Set<string>();
      if (tokens[cursor] !== "}") do {
        const key = JSON.parse(tokens[cursor++]!);
        if (keys.has(key)) throw new Error("Duplicate JSON key"); keys.add(key);
        if (tokens[cursor++] !== ":") throw new Error("Invalid JSON object"); visit();
      } while (tokens[cursor] === "," && ++cursor);
      if (tokens[cursor++] !== "}") throw new Error("Invalid JSON object");
    } else if (token === "[") {
      if (tokens[cursor] !== "]") do { visit(); } while (tokens[cursor] === "," && ++cursor);
      if (tokens[cursor++] !== "]") throw new Error("Invalid JSON array");
    }
  };
  visit(); if (cursor !== tokens.length) throw new Error("Invalid JSON token stream");
  return JSON.parse(bytes) as T;
}
export function expectedIdentity(document: SourceDocument) {
  return { documentId: document.documentId, url: document.url, pageCount: document.pageCount,
    fileSizeBytes: document.fileSizeBytes, title: document.title, publicMetadata: document.publicMetadata };
}
export function dependencyProjection(task: TaskDependency): TaskDependency {
  return { id: task.id, taskType: task.taskType, contextDocumentIds: [...task.contextDocumentIds], contextId: task.contextId, groupId: task.groupId };
}

/** Select identity fields from an acquisition record; never retain its arbitrary payload. */
export function minimalSourceVerification(record: any): Record<string, unknown> {
  const proof = record?.sourceVerification;
  if (!proof || typeof proof !== "object") throw new Error("Acquired identity proof is missing");
  return { status: record.status, sha256: record.sha256, pageCountVerified: record.pageCountVerified, pageCount: record.pageCount,
    sourceVerification: { status: proof.status, sha256: proof.sha256, expectedPageCount: proof.expectedPageCount,
      observedPageCount: proof.observedPageCount, method: proof.method, identityEvidence: proof.identityEvidence } };
}

/** The CLI supplies the RAW list only after hashing its physical files in acquiredInputs. */
export function assertAcquiredStates(input: AssessmentInput, acquired: { documentId: string; sha256: string }[], records: any[]): void {
  unique(acquired.map(d => d.documentId), "physically verified RAW"); unique(records.map(d => d.documentId), "acquisition records");
  for (const state of input.documents) if (state.rawState === "verified") {
    const physical = acquired.find(d => d.documentId === state.documentId), record = records.find(d => d.documentId === state.documentId);
    const source = input.release.documents.find(d => d.documentId === state.documentId);
    if (!source || !physical || !record || physical.sha256 !== state.rawSha256 || record.sha256 !== state.rawSha256 ||
        !verifiedSource(record, source.pageCount) || canonical(minimalSourceVerification(record)) !== canonical(state.verification)) {
      throw new Error("Verified state does not match the current physically verified acquisition and identity proof");
    }
  }
}
function validateFreeze(value: FreezeDecision, document: DocumentAvailability, release: DependencyRelease): void {
  closed(value, ["releaseRevision", "documentId", "expectedIdentitySha256", "reason", "recordedAt", "routes", "review"], "freeze decision");
  if (value.releaseRevision !== release.revision || value.documentId !== document.documentId || value.expectedIdentitySha256 !== document.expectedIdentitySha256) throw new Error("Freeze identity mismatch");
  nonempty(value.reason, "freeze reason"); if (!Number.isFinite(Date.parse(value.recordedAt))) throw new Error("Freeze timestamp required"); pin(value.review);
  if (!Array.isArray(value.routes) || value.routes.length !== 4 || new Set(value.routes.map(r => r.kind)).size !== 4) throw new Error("Reviewed recovery routes required; attempt count is not exhaustion");
  for (const route of value.routes) {
    closed(route, ["kind", "outcome", "reason", "evidence"], "recovery route");
    if (!["original", "official_equivalent", "mirror", "archive"].includes(route.kind) || !["exhausted", "not_applicable"].includes(route.outcome) || route.kind === "original" && route.outcome !== "exhausted") throw new Error("Unresolved recovery route");
    nonempty(route.reason, "recovery rationale"); if (!route.evidence.length) throw new Error("Recovery evidence required"); route.evidence.forEach(pin);
  }
}
function validateInput(input: AssessmentInput): void {
  closed(input, ["revision", "generation", "recordedAt", "supersedes", "release", "documents", "profiles", "preprocessing"], "assessment input");
  nonempty(input.revision, "assessment revision");
  if (!Number.isSafeInteger(input.generation) || input.generation < 1 || !Number.isFinite(Date.parse(input.recordedAt))) throw new Error("Assessment generation and timestamp required");
  if (input.supersedes) pin(input.supersedes);
  const release = input.release;
  closed(release, ["revision", "documents", "tasks", "groups", "fileHashes"], "dependency release");
  nonempty(release.revision, "release revision");
  if (!Object.keys(release.fileHashes).length) throw new Error("Release input hashes required"); Object.values(release.fileHashes).forEach(hash);
  unique(release.documents.map(d => d.documentId), "released documents"); unique(release.tasks.map(t => t.id), "released tasks");
  unique(input.documents.map(d => d.documentId), "document states"); unique(input.profiles.map(p => p.id), "profiles");
  const sources = new Map(release.documents.map(d => [d.documentId, d]));
  for (const document of release.documents) {
    closed(document, ["documentId", "title", "url", "sourceHost", "pageCount", "fileSizeBytes", "domain", "docType", "pdfType", "publicMetadata"], "released document");
    if (!Number.isSafeInteger(document.pageCount) || document.pageCount < 1 || !Number.isSafeInteger(document.fileSizeBytes) || document.fileSizeBytes < 0) throw new Error("Invalid released physical identity");
  }
  if (input.documents.length !== sources.size || input.documents.some(d => !sources.has(d.documentId))) throw new Error("Document states must cover the full release");
  const used = new Set<string>();
  for (const task of release.tasks) {
    closed(task, ["id", "taskType", "contextDocumentIds", "contextId", "groupId"], "dependency task");
    unique(task.contextDocumentIds, "task context"); nonempty(task.contextId, "context identity"); nonempty(task.groupId, "group identity");
    if (task.taskType !== "single_doc" && task.taskType !== "cross_doc" || task.contextDocumentIds.length < (task.taskType === "cross_doc" ? 2 : 1) || task.taskType === "single_doc" && task.contextDocumentIds.length !== 1 || task.contextDocumentIds.some(id => !sources.has(id))) throw new Error("Invalid full task context");
    task.contextDocumentIds.forEach(id => used.add(id));
  }
  if (used.size !== sources.size) throw new Error("Dependencies omit released documents");
  unique(release.groups.map(g => g.id), "source groups");
  const groupedDocuments: string[] = [], groupedTasks: string[] = [];
  for (const group of release.groups) {
    closed(group, ["id", "documentIds", "taskIds"], "source group");
    unique(group.documentIds, "group documents"); unique(group.taskIds, "group tasks");
    if (group.documentIds.some(id => !sources.has(id)) || group.taskIds.some(id => {
      const task = release.tasks.find(t => t.id === id); return !task || task.groupId !== group.id || task.contextDocumentIds.some(d => !group.documentIds.includes(d));
    })) throw new Error("Invalid source grouping");
    groupedDocuments.push(...group.documentIds); groupedTasks.push(...group.taskIds);
  }
  unique(groupedDocuments, "grouped documents"); unique(groupedTasks, "grouped tasks");
  if (groupedDocuments.length !== sources.size || groupedTasks.length !== release.tasks.length) throw new Error("Groups omit released dependencies");
  for (const doc of input.documents) {
    closed(doc, ["documentId", "expectedIdentitySha256", "rawState", "rawSha256", "reason", "evidence", "verification", "freeze"], "document availability");
    if (doc.expectedIdentitySha256 !== digest(expectedIdentity(sources.get(doc.documentId)!))) throw new Error("Expected document identity changed");
    if (!["verified", "recovery_pending", "identity_hold", "frozen_unavailable"].includes(doc.rawState)) throw new Error("Unknown RAW state");
    nonempty(doc.reason, "document reason"); doc.evidence.forEach(pin); if (doc.rawSha256 !== null) hash(doc.rawSha256);
    if (doc.verification !== null) {
      closed(doc.verification, ["status", "sha256", "pageCountVerified", "pageCount", "sourceVerification"], "minimal source verification");
      closed(doc.verification.sourceVerification, ["status", "sha256", "expectedPageCount", "observedPageCount", "method", "identityEvidence"], "minimal identity proof");
      const record = doc.verification, proof = record.sourceVerification as Record<string, unknown>;
      nonempty(record.status, "verification status"); hash(record.sha256);
      nonempty(proof.status, "identity proof status"); hash(proof.sha256);
      nonempty(proof.method, "identity proof method"); nonempty(proof.identityEvidence, "identity proof evidence");
      if (typeof record.pageCountVerified !== "boolean" || !Number.isSafeInteger(record.pageCount) || (record.pageCount as number) < 1 ||
          !Number.isSafeInteger(proof.expectedPageCount) || (proof.expectedPageCount as number) < 1 ||
          !Number.isSafeInteger(proof.observedPageCount) || (proof.observedPageCount as number) < 0) throw new Error("Identity proof requires scalar verification and physical page counts");
    }
    if (doc.rawState === "verified" && (!doc.rawSha256 || !verifiedSource(doc.verification, sources.get(doc.documentId)!.pageCount) || doc.verification!.sha256 !== doc.rawSha256 || !doc.evidence.length)) throw new Error("Verified RAW requires exact source identity proof");
    if (doc.rawState === "frozen_unavailable") { if (!doc.freeze) throw new Error("Reviewed final freeze required"); validateFreeze(doc.freeze, doc, release); }
    else if (doc.freeze !== null) throw new Error("Only terminal freeze may carry a freeze decision");
  }
  for (const profile of input.profiles) {
    closed(profile, ["id", "arm", "implementation", "requiresOcr", "preprocessingProfileSha256"], "arm profile");
    if (!["implemented", "planned"].includes(profile.implementation) || typeof profile.requiresOcr !== "boolean") throw new Error("Invalid profile policy");
    if (profile.implementation === "implemented" && !["bare", "gptgrep"].includes(profile.arm)) throw new Error("Arm is not implemented");
    if (profile.requiresOcr) hash(profile.preprocessingProfileSha256); else if (profile.preprocessingProfileSha256 !== null) throw new Error("Raw-only profile cannot declare required preprocessing");
  }
  const prepKeys = new Set<string>();
  for (const prep of input.preprocessing) {
    closed(prep, ["documentId", "rawSha256", "profileSha256", "state", "pages", "artifacts", "provenance"], "preprocessing");
    hash(prep.rawSha256); hash(prep.profileSha256);
    const key = `${prep.documentId}:${prep.rawSha256}:${prep.profileSha256}`;
    if (prepKeys.has(key) || !sources.has(prep.documentId)) throw new Error("Duplicate or foreign preprocessing"); prepKeys.add(key);
    if (!["not_required", "pending", "partial", "verified", "failed", "blocked_raw_dependency"].includes(prep.state)) throw new Error("Unknown preprocessing state");
    const pages = sources.get(prep.documentId)!.pageCount;
    if (!Array.isArray(prep.pages) || new Set(prep.pages).size !== prep.pages.length || prep.pages.some(p => !Number.isSafeInteger(p) || p < 1 || p > pages)) throw new Error("Invalid physical page coverage");
    prep.artifacts.forEach(pin); prep.provenance.forEach(pin);
    if (prep.state === "verified" && (prep.pages.length !== pages || !prep.artifacts.length || !prep.provenance.length)) throw new Error("Verified OCR requires every physical page and artifact/provenance pins");
  }
}

/** Pure full-release projection. No acquisition, model call or scoring. */
export function assessAvailability(input: AssessmentInput): AvailabilityAssessment {
  validateInput(input);
  const docs = new Map(input.documents.map(d => [d.documentId, d]));
  const tasks = input.release.tasks.map(task => {
    const required = task.contextDocumentIds.map(id => docs.get(id)!);
    const rawState: RawState = required.some(d => d.rawState === "frozen_unavailable") ? "frozen_unavailable"
      : required.some(d => d.rawState === "identity_hold") ? "identity_hold"
      : required.some(d => d.rawState === "recovery_pending") ? "recovery_pending" : "verified";
    const armAvailability = Object.fromEntries(input.profiles.map(profile => {
      const reasons: string[] = [], ocr: Record<string, OcrState> = {};
      if (profile.implementation !== "implemented") reasons.push("arm_not_implemented");
      for (const doc of required) {
        if (doc.rawState !== "verified") reasons.push(`raw:${doc.documentId}:${doc.rawState}`);
        const prep = input.preprocessing.find(p => p.documentId === doc.documentId && p.rawSha256 === doc.rawSha256 && p.profileSha256 === profile.preprocessingProfileSha256);
        ocr[doc.documentId] = !profile.requiresOcr ? "not_required" : doc.rawState !== "verified" ? "blocked_raw_dependency" : prep?.state ?? "pending";
        if (profile.requiresOcr && ocr[doc.documentId] !== "verified") reasons.push(`ocr:${doc.documentId}:${ocr[doc.documentId]}`);
      }
      return [profile.id, { admitted: reasons.length === 0, profileSha256: digest(profile), reasons, ocr }];
    }));
    return { taskId: task.id, taskType: task.taskType, contextId: task.contextId, groupId: task.groupId,
      requiredDocumentIds: [...task.contextDocumentIds], rawState, blockingDocumentIds: required.filter(d => d.rawState !== "verified").map(d => d.documentId),
      reasonCodes: required.filter(d => d.rawState !== "verified").map(d => `${d.documentId}:${d.rawState}`),
      documentStateReferences: Object.fromEntries(required.map(d => [d.documentId, digest(d)])), assessmentRevision: input.revision, armAvailability };
  });
  // Round-trip disconnects the immutable revision from caller-owned mutable objects.
  return JSON.parse(JSON.stringify({ ...input, schema: "gptgrep.task-availability.v1", dependencySha256: digest(input.release), tasks }));
}
export function validateAssessment(value: AvailabilityAssessment): void {
  closed(value, ["schema", "revision", "generation", "recordedAt", "supersedes", "release", "documents", "profiles", "preprocessing", "dependencySha256", "tasks"], "assessment");
  const { schema, dependencySha256, tasks, ...input } = value;
  if (schema !== "gptgrep.task-availability.v1" || digest(assessAvailability(input)) !== digest(value)) throw new Error("Assessment projection or digest mismatch");
}
export function availabilityCoverage(assessment: AvailabilityAssessment, enrolled: string[] = []) {
  validateAssessment(assessment); unique(enrolled, "enrolled cohort");
  const known = new Set(assessment.tasks.map(t => t.taskId));
  if (enrolled.some(id => !known.has(id))) throw new Error("Enrolled task outside assessment release");
  const states: RawState[] = ["verified", "recovery_pending", "identity_hold", "frozen_unavailable"];
  return { schema: "gptgrep.availability-coverage.v1", assessmentRevision: assessment.revision, generation: assessment.generation,
    releasedDocuments: assessment.documents.length, releasedTasks: assessment.tasks.length,
    documents: Object.fromEntries(states.map(s => [s, assessment.documents.filter(d => d.rawState === s).length])),
    tasks: Object.fromEntries(states.map(s => [s, assessment.tasks.filter(t => t.rawState === s).length])),
    profiles: Object.fromEntries(assessment.profiles.map(p => [p.id, { eligibleBeforeEnrollment: assessment.tasks.filter(t => t.armAvailability[p.id]!.admitted).length,
      blockedBeforeEnrollment: assessment.tasks.filter(t => !t.armAvailability[p.id]!.admitted).length,
      enrolledEligibleAtAssessment: enrolled.filter(id => assessment.tasks.find(t => t.taskId === id)!.armAvailability[p.id]!.admitted).length }])),
    enrolled: enrolled.length, enrolledTaskIds: [...enrolled], taskAvailability: assessment.tasks,
    currentLaunchability: "requires fresh controller admission and physical input checks",
    scoring: "separate historical fixed enrolled denominator; availability does not fabricate or rewrite outcomes" };
}

/** Creates only a new revision; never overwrites an assessment, run or result. */
export async function writeAssessment(path: string, value: AvailabilityAssessment): Promise<void> {
  await requireStorage(path); validateAssessment(value); await physicalPath(path);
  if (value.supersedes) { await verifyPin(value.supersedes); const prior = await readJson<AvailabilityAssessment>(value.supersedes.path); validateAssessment(prior);
    if (prior.generation >= value.generation || prior.revision === value.revision || prior.release.revision !== value.release.revision) throw new Error("New assessment must advance the prior revision"); }
  await mkdir(dirname(path), { recursive: true }); await writeFile(path, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx", mode: 0o600 });
}

/** Exact project acceptance selected by the already owner-admitted argv. */
export async function loadAdmission(reference: AdmissionReference, output: string): Promise<Admission> {
  closed(reference, ["acceptance", "profileId"], "admission reference");
  const storage = await requireStorage(output), binding = await readJson(storage.binding), plan = binding.plan;
  await verifyPin(reference.acceptance);
  const acceptance = await readExactJson<ControllerAcceptance>(reference.acceptance.path);
  closed(acceptance, ["schema", "status", "current", "assessment", "generation", "assessmentRevision", "taskIds", "profileIds", "sourcePins", "storage", "controllerArgv", "childArgv", "runtime", "harborExport"], "controller acceptance");
  const expand = (template: string[]) => {
    if (!Array.isArray(template) || !template.length || template.length > 256 || template.some(v => typeof v !== "string" || v.includes("\0"))) throw new Error("Exact bounded argv template required");
    return template.map(v => v === "@ACCEPTANCE_PATH@" ? reference.acceptance.path : v === "@ACCEPTANCE_SHA256@" ? reference.acceptance.sha256 : v);
  };
  if (acceptance.controllerArgv.filter(v => v === "@ACCEPTANCE_PATH@").length !== 1 || acceptance.controllerArgv.filter(v => v === "@ACCEPTANCE_SHA256@").length !== 1) throw new Error("Controller command must select exact acceptance bytes");
  const controller = expand(acceptance.controllerArgv), allowed = [controller, ...acceptance.childArgv.map(expand)];
  if (!plan || digest(controller) !== plan.command?.argv_sha256 || controller[0] !== plan.command.executable ||
      await fileHash(controller[0]!) !== plan.command.executable_sha256 || resolve(process.cwd()) !== plan.cwd ||
      !allowed.some(argv => canonical(argv) === canonical(process.argv))) throw new Error("Process is outside the exact owner-admitted controller/child closure");
  closed(acceptance.storage, ["runId", "workloadSha256", "cwd"], "controller storage binding");
  if (acceptance.schema !== "gptgrep.availability-controller.v1" || acceptance.status !== "accepted" || acceptance.storage.runId !== plan.run_id ||
      acceptance.storage.workloadSha256 !== plan.workload_sha256 || resolve(acceptance.storage.cwd) !== plan.cwd) throw new Error("Controller acceptance does not bind this checked workload");
  unique(acceptance.taskIds, "accepted tasks"); unique(acceptance.profileIds, "accepted profiles");
  if (!acceptance.profileIds.includes(reference.profileId) || !acceptance.taskIds.length || !acceptance.sourcePins.length) throw new Error("Profile or source pins not accepted");
  unique(acceptance.sourcePins.map(p => resolve(p.path)), "controller source pins");
  const pinned = new Set(acceptance.sourcePins.map(p => resolve(p.path)));
  const requiredSources = ["availability.ts", "adapter.ts", "harbor-launch.ts", "dataset.ts", "runner.ts", "native.ts", "io.ts", "source-verification.ts"];
  // These project inputs and script entrypoints supplement the reviewed exact command;
  // this is not discovery of arbitrary custom-controller input semantics.
  const inputFlags = new Set(["--config", "--cohort", "--parent", "--runtime-artifacts"]);
  const commandInputs = allowed.flatMap(argv => {
    const paths = [resolve(plan.cwd, argv[0]!)];
    for (let i = 1; i < argv.length; i++) {
      const arg = argv[i]!;
      if (inputFlags.has(arg) || arg === "--output" || arg === "--acceptance") {
        const value = argv[++i];
        if (!value || value.startsWith("--")) throw new Error("Selected input/output flag requires an exact path");
        if (inputFlags.has(arg)) paths.push(resolve(plan.cwd, value));
      } else if (/\.(?:py|[cm]?[jt]s|tsx|jsx)$/.test(arg)) paths.push(resolve(plan.cwd, arg));
    }
    return paths;
  });
  if (requiredSources.some(name => !pinned.has(resolve(import.meta.dir, name))) ||
      commandInputs.some(path => !pinned.has(path))) throw new Error("Controller/child source closure is not pinned");
  for (const input of [acceptance.current, acceptance.assessment, ...acceptance.sourcePins]) await verifyPin(input);
  const current = await readExactJson(acceptance.current.path);
  closed(current, ["schema", "status", "generation", "assessmentRevision", "assessmentSha256"], "current assessment selection");
  if (current.schema !== "gptgrep.availability-current.v1" || current.status !== "current" || current.generation !== acceptance.generation ||
      current.assessmentRevision !== acceptance.assessmentRevision || current.assessmentSha256 !== acceptance.assessment.sha256) throw new Error("Assessment is absent, stale or superseded");
  const assessment = await readExactJson<AvailabilityAssessment>(acceptance.assessment.path); validateAssessment(assessment);
  if (assessment.generation !== acceptance.generation || assessment.revision !== acceptance.assessmentRevision) throw new Error("Assessment generation mismatch");
  if (acceptance.taskIds.some(id => !assessment.tasks.some(task => task.taskId === id)) || acceptance.profileIds.some(id => !assessment.profiles.some(profile => profile.id === id))) throw new Error("Accepted task/profile outside assessment");
  for (const doc of assessment.documents) {
    for (const evidence of doc.evidence) await verifyPin(evidence);
    if (doc.freeze) {
      await verifyPin(doc.freeze.review);
      const review = await readExactJson(doc.freeze.review.path);
      closed(review, ["schema", "status", "releaseRevision", "documentId", "expectedIdentitySha256", "decisionSha256"], "freeze review");
      const { review: reviewPin, ...decision } = doc.freeze;
      if (review.schema !== "gptgrep.source-freeze-review.v1" || review.status !== "accepted" || review.releaseRevision !== assessment.release.revision ||
          review.documentId !== doc.documentId || review.expectedIdentitySha256 !== doc.expectedIdentitySha256 || review.decisionSha256 !== digest(decision)) throw new Error("Terminal freeze review is absent or does not accept this exact decision");
      for (const route of doc.freeze.routes) for (const evidence of route.evidence) await verifyPin(evidence);
    }
  }
  return { assessment, acceptance, reference };
}

export function assertTaskAdmitted(admission: Admission, task: TaskDependency, arm: string, raw: { documentId: string; sha256: string }[]): TaskAvailability {
  validateAssessment(admission.assessment);
  const dependency = admission.assessment.release.tasks.find(t => t.id === task.id);
  if (!dependency || canonical(dependency) !== canonical(dependencyProjection(task))) throw new Error("Task dependencies differ from the complete assessed context");
  const record = admission.assessment.tasks.find(t => t.taskId === task.id)!;
  const profile = admission.assessment.profiles.find(p => p.id === admission.reference.profileId);
  if (!admission.acceptance.taskIds.includes(task.id) || !profile || profile.arm !== arm || !record.armAvailability[profile.id]?.admitted) throw new Error("Task/arm denied by current dependency availability");
  unique(raw.map(d => d.documentId), "RAW inputs");
  for (const id of task.contextDocumentIds) if (raw.find(d => d.documentId === id)?.sha256 !== admission.assessment.documents.find(d => d.documentId === id)?.rawSha256) throw new Error("RAW input differs from assessed identity");
  return record;
}

/** Artifact-only current check for enrollment/planning, which have no staged workspace yet. */
export async function verifyPreprocessingPins(admission: Admission, tasks: TaskDependency[]): Promise<void> {
  const profile = admission.assessment.profiles.find(p => p.id === admission.reference.profileId)!;
  if (!profile.requiresOcr) return;
  for (const id of new Set(tasks.flatMap(t => t.contextDocumentIds))) {
    const raw = admission.assessment.documents.find(d => d.documentId === id)!;
    const prep = admission.assessment.preprocessing.find(p => p.documentId === id && p.rawSha256 === raw.rawSha256 && p.profileSha256 === profile.preprocessingProfileSha256);
    if (!prep || prep.state !== "verified") throw new Error("Required preprocessing unavailable");
    for (const artifact of [...prep.artifacts, ...prep.provenance]) await verifyPin(artifact);
  }
}

/** In addition to current source pins, every required artifact must be in this task workspace. */
export async function verifyRequiredPreprocessing(admission: Admission, tasks: TaskDependency[], suppliedHashes: string[]): Promise<void> {
  await verifyPreprocessingPins(admission, tasks);
  const profile = admission.assessment.profiles.find(p => p.id === admission.reference.profileId)!;
  if (!profile.requiresOcr) return;
  for (const id of new Set(tasks.flatMap(t => t.contextDocumentIds))) {
    const raw = admission.assessment.documents.find(d => d.documentId === id)!;
    const prep = admission.assessment.preprocessing.find(p => p.documentId === id && p.rawSha256 === raw.rawSha256 && p.profileSha256 === profile.preprocessingProfileSha256)!;
    if (prep.artifacts.some(p => !suppliedHashes.includes(p.sha256))) throw new Error("Required OCR artifacts absent from selected workspace");
  }
}
