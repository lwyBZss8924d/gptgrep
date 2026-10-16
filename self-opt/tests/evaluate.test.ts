import { expect, test } from "bun:test";
import { compare, planLoop, summarize, type EvaluationRun, type Outcome } from "../src/evaluate";
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
