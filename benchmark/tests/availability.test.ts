import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { normalizeDependencyRelease, projectReaderTask, XL_RELEASE_REVISION, type DependencyRelease } from "../src/dataset";
import { assessAvailability, assertAcquiredStates, assertTaskAdmitted, availabilityCoverage, expectedIdentity, validateAssessment, verifyPreprocessingPins, verifyRequiredPreprocessing,
  type Admission, type AssessmentInput, type DocumentAvailability, type FreezeDecision } from "../src/availability";
import { digest, sha256 } from "../src/io";
import { summarize, type EvaluationRun } from "../../self-opt/src/evaluate";

function fixture(): AssessmentInput {
  const context = (id: string) => ({ document_id: id });
  const privateFields = { question: "PRIVATE QUESTION", answer: "PRIVATE GOLD", evidence: [{ page: 777 }] };
  const single = ["d1", "d2"].map((id, i) => ({ question_id: `q${i + 1}`, task_type: "single_doc", document: context(id), metadata: { n_context_documents: 1 }, ...privateFields }));
  const release = normalizeDependencyRelease({ manifest: { release_version: "synthetic", document_count: 3, qa_count: 3,
    single_doc_qa_count: 2, cross_doc_qa_count: 1, release_files: { documents: "documents.jsonl", single_doc_questions: "single.jsonl", cross_doc_questions: "cross.jsonl" } },
    documents: ["d1", "d2", "d3"].map(id => ({ document_id: id, title: id, url: `https://example.invalid/${id}.pdf`, source_host: "example.invalid",
      page_count: 3, file_size_bytes: 100, domain: "synthetic", doc_type: "report", pdf_type: "text", public_metadata: {} })),
    singleDocQuestions: single, crossDocQuestions: [{ question_id: "series", task_type: "cross_doc", documents: [context("d1"), context("d2"), context("d3")], metadata: { n_context_documents: 3 }, ...privateFields }],
    fileHashes: { "documents.jsonl": sha256("metadata"), "single.jsonl": sha256("single"), "cross.jsonl": sha256("cross") } });
  const documents: DocumentAvailability[] = release.documents.map(d => ({ documentId: d.documentId, expectedIdentitySha256: digest(expectedIdentity(d)), rawState: "verified",
    rawSha256: sha256(d.documentId), reason: "Verified exact released identity", evidence: [{ path: `/synthetic/${d.documentId}.receipt`, sha256: sha256("receipt") }], freeze: null,
    verification: { status: "ready", sha256: sha256(d.documentId), pageCountVerified: true, pageCount: 3,
      sourceVerification: { status: "verified", sha256: sha256(d.documentId), expectedPageCount: 3, observedPageCount: 3, method: "synthetic", identityEvidence: "synthetic exact identity" } } }));
  return { revision: "assessment-1", generation: 1, recordedAt: "2026-10-16T00:00:00Z", supersedes: null, release, documents,
    profiles: [{ id: "bare-raw", arm: "bare", implementation: "implemented", requiresOcr: false, preprocessingProfileSha256: null },
      { id: "gptgrep-raw", arm: "gptgrep", implementation: "implemented", requiresOcr: false, preprocessingProfileSha256: null },
      { id: "ocr", arm: "gptgrep", implementation: "implemented", requiresOcr: true, preprocessingProfileSha256: sha256("ocr") },
      { id: "planned", arm: "DocAtlas", implementation: "planned", requiresOcr: false, preprocessingProfileSha256: null }], preprocessing: [] };
}
function admit(input: AssessmentInput, profileId = "bare-raw"): Admission {
  const assessment = assessAvailability(input);
  return { assessment, reference: { profileId, acceptance: { path: "/synthetic/acceptance", sha256: sha256("acceptance") } },
    acceptance: { schema: "gptgrep.availability-controller.v1", status: "accepted", current: { path: "/synthetic/current", sha256: sha256("current") },
      assessment: { path: "/synthetic/assessment", sha256: sha256("assessment") }, generation: 1, assessmentRevision: input.revision,
      taskIds: input.release.tasks.map(t => t.id), profileIds: input.profiles.map(p => p.id), sourcePins: [], controllerArgv: [], childArgv: [],
      storage: { runId: "synthetic", workloadSha256: sha256("workload"), cwd: "/synthetic" }, runtime: null, harborExport: null } };
}
function raw(input: AssessmentInput) { return input.documents.map(d => ({ documentId: d.documentId, sha256: d.rawSha256! })); }
function freeze(input: AssessmentInput, documentId: string): FreezeDecision {
  const doc = input.documents.find(d => d.documentId === documentId)!;
  return { releaseRevision: input.release.revision, documentId, expectedIdentitySha256: doc.expectedIdentitySha256,
    reason: "Reviewed reasonable exact-edition recovery is exhausted", recordedAt: input.recordedAt,
    routes: ["original", "official_equivalent", "mirror", "archive"].map(kind => ({ kind: kind as any, outcome: "exhausted", reason: "Exact edition unavailable after reviewed recovery",
      evidence: [{ path: `/synthetic/${kind}.json`, sha256: sha256(kind) }] })), review: { path: "/synthetic/review.json", sha256: sha256("review") } };
}

describe("full-release dependency availability", () => {
  test("verified states must match the currently hashed acquisition and its exact minimal proof", () => {
    const input = fixture(), acquired = raw(input);
    const records = input.documents.map(d => ({ documentId: d.documentId, ...structuredClone(d.verification) }));
    expect(() => assertAcquiredStates(input, acquired, records)).not.toThrow();
    expect(() => assertAcquiredStates(input, acquired.slice(1), records)).toThrow("physically verified acquisition");
    expect(() => assertAcquiredStates(input, acquired, records.slice(1))).toThrow("physically verified acquisition");
    expect(() => assertAcquiredStates(input, [{ ...acquired[0]!, sha256: sha256("stale bytes") }, ...acquired.slice(1)], records)).toThrow();
    const held: any[] = structuredClone(records); held[0].status = "held";
    expect(() => assertAcquiredStates(input, acquired, held)).toThrow();
    const differentProof: any[] = structuredClone(records); differentProof[0].sourceVerification.method = "different proof";
    expect(() => assertAcquiredStates(input, acquired, differentProof)).toThrow();
  });
  test("arbitrary nested verification payload and extra assessment fields cannot carry task or judge content", () => {
    for (const field of ["question", "answer", "evidence"]) {
      const input = fixture(); input.documents[0]!.verification![field] = "PRIVATE";
      expect(() => assessAvailability(input)).toThrow("minimal source verification");
      const nested = fixture(); (nested.documents[0]!.verification!.sourceVerification as any)[field] = "PRIVATE";
      expect(() => assessAvailability(nested)).toThrow("minimal identity proof");
    }
    expect(() => assessAvailability({ ...fixture(), answer: "PRIVATE" } as any)).toThrow("assessment input");
  });
  test("held and pending RAW retain only scalar proof fields", () => {
    for (const rawState of ["identity_hold", "recovery_pending"] as const) {
      const retained = fixture(); retained.documents[0]!.rawState = rawState;
      expect(() => assessAvailability(retained)).not.toThrow();
      for (const field of ["status", "sha256", "expectedPageCount", "observedPageCount", "method", "identityEvidence"]) {
        const input = fixture(); input.documents[0]!.rawState = rawState;
        (input.documents[0]!.verification!.sourceVerification as any)[field] = { question: "PRIVATE", answer: "PRIVATE", evidence: [777] };
        expect(() => assessAvailability(input)).toThrow();
      }
      for (const field of ["status", "sha256", "pageCountVerified", "pageCount"]) {
        const input = fixture(); input.documents[0]!.rawState = rawState;
        input.documents[0]!.verification![field] = { question: "PRIVATE", answer: "PRIVATE" };
        expect(() => assessAvailability(input)).toThrow();
      }
    }
  });
  test("enrollment/planning rehash required OCR bytes even without a staged workspace", async () => {
    const root = await mkdtemp(join(tmpdir(), "availability-ocr-"));
    try {
      const artifact = join(root, "pages.json"), provenance = join(root, "parser.json");
      await writeFile(artifact, "synthetic complete pages"); await writeFile(provenance, "synthetic exact parser");
      const input = fixture(); input.preprocessing = [{ documentId: "d1", rawSha256: sha256("d1"), profileSha256: sha256("ocr"), state: "verified", pages: [1, 2, 3],
        artifacts: [{ path: artifact, sha256: sha256("synthetic complete pages") }], provenance: [{ path: provenance, sha256: sha256("synthetic exact parser") }] }];
      const admission = admit(input, "ocr"), tasks = [input.release.tasks.find(t => t.id === "q1")!];
      await verifyPreprocessingPins(admission, tasks);
      await writeFile(artifact, "stale or partial replacement");
      await expect(verifyPreprocessingPins(admission, tasks)).rejects.toThrow("Pinned input changed");
      await expect(verifyRequiredPreprocessing(admission, tasks, [sha256("synthetic complete pages")])).rejects.toThrow("Pinned input changed");
      await rm(artifact); await expect(verifyPreprocessingPins(admission, tasks)).rejects.toThrow();
    } finally { await rm(root, { recursive: true, force: true }); }
  });
  test("dependency projection contains no questions, gold, evidence or scorer outputs", () => {
    const input = fixture(), serialized = JSON.stringify(input.release);
    expect(serialized).not.toContain("PRIVATE"); expect(serialized).not.toContain("777");
    expect(Object.keys(input.release.tasks[0]!)).toEqual(["id", "taskType", "contextDocumentIds", "contextId", "groupId"]);
    const row = { question_id: "q", task_type: "single_doc", document: { document_id: "d" }, metadata: { n_context_documents: 1 }, question: "public" };
    expect(projectReaderTask(row).question).toBe("public");
    expect(() => projectReaderTask({ ...row, metadata: { n_context_documents: 2 } })).toThrow("count mismatch");
  });
  test("any missing full-series member blocks the whole task without shrinking context or source-group spillover", () => {
    const input = fixture(); input.documents[2]!.rawState = "recovery_pending"; input.documents[2]!.rawSha256 = null; input.documents[2]!.verification = null;
    const assessment = assessAvailability(input), series = assessment.tasks.find(t => t.taskId === "series")!;
    expect(series.requiredDocumentIds).toEqual(["d1", "d2", "d3"]); expect(series.blockingDocumentIds).toEqual(["d3"]);
    expect(series.rawState).toBe("recovery_pending"); expect(series.armAvailability["bare-raw"]!.admitted).toBe(false);
    expect(assessment.tasks.find(t => t.taskId === "q1")!.armAvailability["bare-raw"]!.admitted).toBe(true);
    expect(input.release.groups).toHaveLength(1);
  });
  test("held identity and failed acquisition remain distinct from final freeze", () => {
    for (const state of ["identity_hold", "recovery_pending"] as const) {
      const input = fixture(); input.documents[1]!.rawState = state;
      const assessment = assessAvailability(input);
      expect(assessment.tasks.find(t => t.taskId === "q2")!.rawState).toBe(state);
      expect(availabilityCoverage(assessment).documents.frozen_unavailable).toBe(0);
    }
  });
  test("two attempts do not establish recovery exhaustion; freeze requires reviewed complete route rationale", () => {
    const input = fixture(), doc = input.documents[2]!; doc.rawState = "frozen_unavailable"; doc.rawSha256 = null; doc.verification = null;
    expect(() => assessAvailability(input)).toThrow("Reviewed final freeze");
    doc.freeze = freeze(input, doc.documentId); doc.freeze.routes = doc.freeze.routes.slice(0, 2);
    expect(() => assessAvailability(input)).toThrow("attempt count");
    doc.freeze = freeze(input, doc.documentId); doc.freeze.reason = "";
    expect(() => assessAvailability(input)).toThrow();
    doc.freeze = freeze(input, doc.documentId); doc.freeze.routes[2]!.outcome = "pending" as any;
    expect(() => assessAvailability(input)).toThrow("Unresolved");
    doc.freeze = freeze(input, doc.documentId);
    expect(assessAvailability(input).tasks.find(t => t.taskId === "series")!.rawState).toBe("frozen_unavailable");
    expect(doc.rawSha256).toBeNull();
  });
  test("changed edition/page identity cannot satisfy released dependencies", () => {
    const input = fixture(); (input.documents[0]!.verification!.sourceVerification as any).observedPageCount = 4;
    expect(() => assessAvailability(input)).toThrow("exact source identity");
    const different = fixture(); different.documents[0]!.expectedIdentitySha256 = sha256("new edition");
    expect(() => assessAvailability(different)).toThrow("identity changed");
  });
  test("RAW readiness is independent of partial OCR and planned-arm availability", () => {
    const input = fixture();
    input.preprocessing = input.documents.map(d => ({ documentId: d.documentId, rawSha256: d.rawSha256!, profileSha256: sha256("ocr"), state: "partial", pages: [1],
      artifacts: [{ path: "/synthetic/pages.json", sha256: sha256("pages") }], provenance: [{ path: "/synthetic/parser.json", sha256: sha256("parser") }] }));
    const task = assessAvailability(input).tasks.find(t => t.taskId === "series")!;
    expect(task.armAvailability["bare-raw"]!.admitted).toBe(true); expect(task.armAvailability["gptgrep-raw"]!.admitted).toBe(true);
    expect(task.armAvailability.ocr!.admitted).toBe(false); expect(task.armAvailability.planned!.admitted).toBe(false);
    input.preprocessing.forEach(p => { p.state = "verified"; p.pages = [1, 2, 3]; });
    expect(assessAvailability(input).tasks[0]!.armAvailability.ocr!.admitted).toBe(true);
    input.profiles[3]!.implementation = "implemented"; expect(() => assessAvailability(input)).toThrow("not implemented");
  });
  test("complete physical OCR coverage, exact RAW and profile are mandatory", () => {
    const input = fixture(); input.preprocessing = [{ documentId: "d1", rawSha256: sha256("d1"), profileSha256: sha256("ocr"), state: "verified", pages: [1, 2], artifacts: [], provenance: [] }];
    expect(() => assessAvailability(input)).toThrow("every physical page");
    input.preprocessing[0] = { ...input.preprocessing[0]!, pages: [1, 2, 3], artifacts: [{ path: "/synthetic/a", sha256: sha256("a") }], provenance: [{ path: "/synthetic/p", sha256: sha256("p") }], rawSha256: sha256("different") };
    expect(assessAvailability(input).tasks.find(t => t.taskId === "q1")!.armAvailability.ocr!.admitted).toBe(false);
  });
  test("duplicate, foreign, omitted and narrowed dependencies fail closed", () => {
    for (const mutate of [
      (r: DependencyRelease) => r.tasks.push(r.tasks[0]!),
      (r: DependencyRelease) => r.tasks[0]!.contextDocumentIds.push("foreign"),
      (r: DependencyRelease) => r.tasks[0]!.contextDocumentIds.push(r.tasks[0]!.contextDocumentIds[0]!),
      (r: DependencyRelease) => { (r.tasks[0] as any).question = "leak"; },
    ]) { const input = fixture(); mutate(input.release); expect(() => assessAvailability(input)).toThrow(); }
    const input = fixture(), admission = admit(input), task = input.release.tasks.find(t => t.id === "series")!;
    expect(() => assertTaskAdmitted(admission, { ...task, contextDocumentIds: ["d1", "d2"] }, "bare", raw(input))).toThrow("complete assessed context");
    expect(() => assertTaskAdmitted(admission, task, "bare", raw(input).slice(0, 2))).toThrow("assessed identity");
    expect(() => assertTaskAdmitted(admission, task, "gptgrep", raw(input))).toThrow("denied");
    expect(() => assertTaskAdmitted(admission, task, "bare", raw(input))).not.toThrow();
  });
  test("tampered derived admission and stale dependency digest are rejected", () => {
    const assessment = assessAvailability(fixture()); assessment.tasks[0]!.armAvailability["bare-raw"]!.admitted = false;
    expect(() => validateAssessment(assessment)).toThrow("projection");
    const other = assessAvailability(fixture()); other.dependencySha256 = sha256("stale"); expect(() => validateAssessment(other)).toThrow();
  });
  test("new recovery evidence creates a new immutable projection", () => {
    const input = fixture(); input.documents[2]!.rawState = "recovery_pending";
    const old = assessAvailability(input), before = JSON.stringify(old);
    input.documents[2]!.rawState = "verified"; input.generation = 2; input.revision = "assessment-2";
    input.supersedes = { path: "/synthetic/assessment-1.json", sha256: sha256(before) };
    const recovered = assessAvailability(input);
    expect(recovered.tasks.find(t => t.taskId === "series")!.rawState).toBe("verified"); expect(JSON.stringify(old)).toBe(before);
  });
  test("coverage retains released partitions and enrolled denominator without changing historical measurement", () => {
    const input = fixture(); input.documents[2]!.rawState = "identity_hold";
    const assessment = assessAvailability(input), report = availabilityCoverage(assessment, ["q1", "series"]);
    expect(report.releasedTasks).toBe(3); expect(report.tasks.verified).toBe(2); expect(report.tasks.identity_hold).toBe(1);
    expect(report.enrolled).toBe(2); expect(report.profiles["bare-raw"]!.enrolledEligibleAtAssessment).toBe(1);
    const scenario: any = Object.fromEntries(["corpusSha256", "rubricSha256", "nativeScorerSha256", "environmentSha256", "preparationPolicySha256"].map(k => [k, sha256(k)]));
    Object.assign(scenario, { datasetRevision: XL_RELEASE_REVISION, readerModel: "reader", workerModel: "worker", ocrModel: "ocr", helperModel: "helper", reasoningEffort: "high", serviceTier: "default", judgeModel: "judge", judgeReasoningEffort: "high", judgeServiceTier: "default", seed: "fixed" });
    const run: EvaluationRun = { schema: "gptgrep.opt-evaluation.v1", runId: "historical", revision: "a".repeat(40), parentRevision: null, cohort: ["q1", "series"], split: "dev", scenario,
      outcomes: [{ taskId: "q1", status: "complete", groundedSuccess: 1, nativeAccuracy: 1, atomicCoverage: 1, latencyMs: 1, costUsd: 0 },
        { taskId: "series", status: "pending", groundedSuccess: null, nativeAccuracy: null, atomicCoverage: null, latencyMs: null, costUsd: null }] };
    const before = JSON.stringify(run), summary = summarize(run); availabilityCoverage(assessment, run.cohort);
    expect(JSON.stringify(run)).toBe(before); expect(summarize(run)).toEqual(summary);
    expect(summary.enrolled).toBe(2); expect(summary.groundedSuccess).toEqual({ lowerBound: 0.5, upperBound: 1, final: null });
  });
});
