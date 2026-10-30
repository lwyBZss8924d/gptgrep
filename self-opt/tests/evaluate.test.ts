import { expect, test } from "bun:test";
import { compare, normalizeOutcome, normalizeRun, planLoop, summarize, type EvaluationRun, type Outcome } from "../src/evaluate";
const hash = "a".repeat(64), rev = "b".repeat(40);
const result = (taskId: string, groundedSuccess: 0 | 1): Outcome => ({ taskId, status: "complete", groundedSuccess, nativeAccuracy: groundedSuccess, atomicCoverage: groundedSuccess, latencyMs: 10, costUsd: null });
function run(outcomes: Outcome[]): EvaluationRun {
  return { schema: "gptgrep.opt-evaluation.v1", runId: "unit", revision: rev, parentRevision: null, cohort: ["a", "b"], split: "dev",
    scenario: { datasetRevision: "test", corpusSha256: hash, rubricSha256: hash, nativeScorerSha256: hash, readerModel: "reader", workerModel: "worker", ocrModel: "vision", helperModel: "helper", reasoningEffort: "high", serviceTier: "default", judgeModel: "judge", judgeReasoningEffort: "high", judgeServiceTier: "default", environmentSha256: hash, preparationPolicySha256: hash, seed: "test" }, outcomes };
}
test("pending and judge unknown remain in fixed denominator and bounds", () => {
  const report = summarize(run([result("a", 1)]));
  expect(report.enrolled).toBe(2); expect(report.pending).toBe(1); expect(report.groundedSuccess).toEqual({ lowerBound: 0.5, upperBound: 1, final: null });
  const unknown = summarize(run([result("a", 1), { ...result("b", 0), status: "judge_unknown", groundedSuccess: null }]));
  expect(unknown.unknown).toBe(1); expect(unknown.pending).toBe(0); expect(unknown.comparable).toBe(false);
});
test("started failures remain observed zeros; absent cost stays unavailable", () => {
  const report = summarize(run([result("a", 1), { ...result("b", 0), status: "operational_failure" }]));
  expect(report.groundedSuccess.final).toBe(0.5); expect(report.failures).toBe(1); expect(report.costUsd.total).toBeNull();
});
test("foreign, duplicated, nonfinite and falsely numeric unknown results fail", () => {
  expect(() => summarize(run([result("x", 1)]))).toThrow();
  expect(() => summarize(run([result("a", 1), result("a", 0)]))).toThrow();
  expect(() => summarize(run([{ ...result("a", 1), nativeAccuracy: NaN }]))).toThrow();
  expect(() => summarize(run([{ ...result("a", 0), status: "judge_unknown" }]))).toThrow();
});
test("same scenario improvement can select a development parent only", () => {
  const a = run([result("a", 0), result("b", 1)]), b = { ...run([result("a", 1), result("b", 1)]), revision: "c".repeat(40), parentRevision: rev };
  expect(compare(a, b).eligibleNextParent).toBe(true); expect(compare(a, b).releaseAccepted).toBe(false);
  expect(compare(a, b, { minimumImprovement: 0.6 }).eligibleNextParent).toBe(false);
  expect(compare(a, b, { maxCostRatio: 2 }).reasons).toContain("cost_constraint_unobservable");
});
test("changed scenario or stale parent cannot promote historical scores", () => {
  const a = run([result("a", 0), result("b", 1)]), b = { ...run([result("a", 1), result("b", 1)]), revision: "c".repeat(40), parentRevision: rev };
  b.scenario.rubricSha256 = "f".repeat(64);
  expect(compare(a, b).reasons).toContain("scenario_or_cohort_changed_remeasure_both");
  b.parentRevision = null; expect(compare(a, b).reasons).toContain("candidate_parent_is_not_current_incumbent");
});
test("frozen confirmation cannot be consumed by Opt selection", () => {
  const a = { ...run([result("a", 0), result("b", 1)]), split: "frozen" as const }, b = { ...run([result("a", 1), result("b", 1)]), split: "frozen" as const, revision: "c".repeat(40), parentRevision: rev };
  expect(compare(a, b).reasons).toContain("frozen_split_cannot_select_next_opt_parent");
});
test("a changed judge cannot turn an adjudicator difference into product improvement", () => {
  for (const field of ["judgeModel", "judgeReasoningEffort", "judgeServiceTier"] as const) {
    const a = run([result("a", 0), result("b", 1)]), b = { ...run([result("a", 1), result("b", 1)]), revision: "c".repeat(40), parentRevision: rev };
    b.scenario[field] = "different";
    expect(compare(a, b).reasons).toContain("scenario_or_cohort_changed_remeasure_both");
    expect(compare(a, b).eligibleNextParent).toBe(false);
  }
});
test("loop planner isolates baseline and excludes evaluation/judge mutation", () => {
  const parent = run([result("a", 1), result("b", 0)]);
  const plan = planLoop({ baselineRevision: rev, parent, iteration: 1, mutationPaths: ["skills/gptgrep/SKILL.md"] });
  expect(plan.storageAdmissionRequired).toBe(true); expect(plan.executionAuthorizedByPlan).toBe(false);
  expect(() => planLoop({ baselineRevision: rev, parent, iteration: 1, mutationPaths: ["benchmark/src/judge.ts"] })).toThrow();
  expect(() => planLoop({ baselineRevision: rev, parent, iteration: 1, mutationPaths: ["crates/../benchmark/judge.ts"] })).toThrow();
});

test("explicit prelaunch denials normalize to unknown without document-specific routing", () => {
  const denied1 = normalizeOutcome({ taskId: "a", status: "denied_before_dispatch", documentIds: ["arbitrary"], nativeAccuracy: 0.8 });
  expect(denied1.status).toBe("setup_failed");
  expect(denied1.groundedSuccess).toBeNull();
  expect(denied1.nativeAccuracy).toBe(0.8);

  const denied2 = normalizeOutcome({ taskId: "b", status: "denied_before_reader" });
  expect(denied2.status).toBe("setup_failed");
  expect(denied2.groundedSuccess).toBeNull();

  const setupFailure = normalizeOutcome({ taskId: "b", status: "setup_failed" });
  expect(setupFailure.status).toBe("setup_failed");
  expect(setupFailure.groundedSuccess).toBeNull();

  const r = run([
    result("a", 1),
    { ...denied2, taskId: "b" },
  ]);
  const s = summarize(r);
  expect(s.enrolled).toBe(2);
  expect(s.unknown).toBe(1);
  expect(s.successes).toBe(1);
  expect(s.bounds).toEqual([0.5, 1.0]);
  expect(s.groundedSuccess.lowerBound).toBe(0.5);
  expect(s.groundedSuccess.upperBound).toBe(1.0);
  expect(s.comparable).toBe(false);
});

test("normalizes raw statuses before validating and never classifies by document ID", () => {
  const raw = { ...run([]), outcomes: [{ taskId: "a", status: "timeout" as const },
    { taskId: "b", status: "complete" as const, documentIds: ["doc_000236", "doc_000165"], groundedSuccess: 1 as const }] };
  const normalized = normalizeRun(raw);
  expect(normalized.outcomes.map(o => o.status)).toEqual(["operational_failure", "complete"]);
  expect(normalizeOutcome({ taskId: "a", status: "timeout", validStart: false }).status).toBe("setup_failed");
  expect(() => normalizeRun({ ...raw, outcomes: [{ taskId: "a", status: "made_up" as any }] })).toThrow("Unknown raw outcome");
});

test("postlaunch operational failures receive numeric score 0", () => {
  const failure = normalizeOutcome({ taskId: "b", status: "timeout", nativeAccuracy: 0.0 });
  expect(failure.status).toBe("operational_failure");
  expect(failure.groundedSuccess).toBe(0);
  expect(failure.groundedAccuracy).toBe(0);

  const crash = normalizeOutcome({ taskId: "b", status: "crashed" });
  expect(crash.status).toBe("operational_failure");
  expect(crash.groundedSuccess).toBe(0);

  const r = run([result("a", 1), failure]);
  const s = summarize(r);
  expect(s.enrolled).toBe(2);
  expect(s.failures).toBe(1);
  expect(s.unknown).toBe(0);
  expect(s.successes).toBe(1);
  expect(s.bounds).toEqual([0.5, 0.5]);
  expect(s.groundedSuccess.final).toBe(0.5);
  expect(s.comparable).toBe(true);
});

test("native accuracy and grounded accuracy separation", () => {
  const o: Outcome = {
    taskId: "a",
    status: "complete",
    groundedSuccess: 0,
    groundedAccuracy: 0,
    nativeAccuracy: 0.85,
    accuracy: 0.85,
    atomicCoverage: 0.5,
    latencyMs: 120,
    costUsd: 0.01,
  };
  const s = summarize(run([o, result("b", 1)]));
  expect(s.nativeAccuracy.observed).toBe(2);
  expect(s.accuracy.observed).toBe(2);
  expect(s.nativeAccuracy.fullCohortMean).toBe((0.85 + 1.0) / 2);
  expect(s.groundedSuccess.final).toBe(0.5);
  expect(s.groundedAccuracy.final).toBe(0.5);
});

test("compare disables automated win when unknown counts differ or delta is within uncertainty bounds", () => {
  // unknown counts differ
  const r1 = run([result("a", 1), { ...result("b", 0), status: "judge_unknown", groundedSuccess: null }]);
  const r2 = { ...run([result("a", 1), result("b", 1)]), revision: "c".repeat(40), parentRevision: rev };
  const comp1 = compare(r1, r2);
  expect(comp1.eligibleNextParent).toBe(false);
  expect(comp1.reasons).toContain("unknown_count_differs");

  // delta within uncertainty bounds
  const rInc = run([result("a", 1), { ...result("b", 0), status: "judge_unknown", groundedSuccess: null }]);
  const rCand = {
    ...run([result("a", 0), { ...result("b", 0), status: "judge_unknown", groundedSuccess: null }]),
    revision: "c".repeat(40),
    parentRevision: rev,
  };
  const comp2 = compare(rInc, rCand);
  expect(comp2.eligibleNextParent).toBe(false);
  expect(comp2.reasons).toContain("delta_within_uncertainty_bounds");
});

