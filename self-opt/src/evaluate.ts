import { digest } from "../../benchmark/src/io";

export type OutcomeStatus = "complete" | "operational_failure" | "judge_unknown" | "setup_failed" | "pending";
export interface Outcome {
  taskId: string;
  status: OutcomeStatus;
  groundedSuccess: 0 | 1 | null;
  nativeAccuracy: number | null;
  atomicCoverage: number | null;
  latencyMs: number | null;
  costUsd: number | null;
  errorCategory?: string;
}
export interface EvaluationRun {
  schema: "gptgrep.opt-evaluation.v1";
  runId: string;
  /** Immutable product revision. Changed products are compared under the same scenario. */
  revision: string;
  parentRevision: string | null;
  cohort: string[];
  split: "dev" | "frozen";
  scenario: {
    datasetRevision: string;
    corpusSha256: string;
    rubricSha256: string;
    nativeScorerSha256: string;
    readerModel: string;
    workerModel: string;
    ocrModel: string;
    helperModel: string;
    reasoningEffort: string;
    serviceTier: string;
    judgeModel: string;
    judgeReasoningEffort: string;
    judgeServiceTier: string;
    environmentSha256: string;
    preparationPolicySha256: string;
    seed: string;
  };
  outcomes: Outcome[];
}

export function scenarioFingerprint(run: EvaluationRun): string {
  return digest({ scenario: run.scenario, split: run.split, cohort: [...run.cohort].sort() });
}

function score(value: number | null, label: string): void {
  if (value !== null && (!Number.isFinite(value) || value < 0 || value > 1)) throw new Error(`Invalid ${label}`);
}
function nonnegative(value: number | null, label: string): void {
  if (value !== null && (!Number.isFinite(value) || value < 0)) throw new Error(`Invalid ${label}`);
}
export function validateRun(run: EvaluationRun): void {
  if (run.schema !== "gptgrep.opt-evaluation.v1" || !run.runId || !/^[a-f0-9]{40,64}$/.test(run.revision)) throw new Error("Evaluation identity requires an immutable source revision");
  if (!run.cohort.length || new Set(run.cohort).size !== run.cohort.length) throw new Error("A nonempty unique frozen cohort is required");
  if (!["dev", "frozen"].includes(run.split)) throw new Error("Unknown evaluation split");
  const scenarioFields = ["datasetRevision", "corpusSha256", "rubricSha256", "nativeScorerSha256", "readerModel", "workerModel", "ocrModel", "helperModel", "reasoningEffort", "serviceTier", "judgeModel", "judgeReasoningEffort", "judgeServiceTier", "environmentSha256", "preparationPolicySha256", "seed"];
  if (!run.scenario || Object.keys(run.scenario).sort().join("\0") !== scenarioFields.sort().join("\0")) throw new Error("Scenario contract has missing or unknown fields");
  if (!Array.isArray(run.outcomes)) throw new Error("Evaluation outcomes must be an array");
  for (const [key, value] of Object.entries(run.scenario)) {
    if (typeof value !== "string" || !value) throw new Error(`Missing scenario field: ${key}`);
    if (key.endsWith("Sha256") && !/^[a-f0-9]{64}$/.test(value)) throw new Error(`Invalid scenario digest: ${key}`);
  }
  const ids = new Set(run.cohort), observed = new Set<string>();
  for (const result of run.outcomes) {
    if (!ids.has(result.taskId) || observed.has(result.taskId)) throw new Error("Outcome is duplicated or outside the frozen cohort");
    observed.add(result.taskId);
    if (!["complete", "operational_failure", "judge_unknown", "setup_failed", "pending"].includes(result.status)) throw new Error("Unknown outcome status");
    score(result.nativeAccuracy, "native accuracy"); score(result.atomicCoverage, "atomic coverage");
    nonnegative(result.latencyMs, "latency"); nonnegative(result.costUsd, "cost");
    if (result.status === "complete" && ![0, 1].includes(result.groundedSuccess as number)) throw new Error("Completed grounding requires a binary result");
    if (result.status === "operational_failure" && result.groundedSuccess !== 0) throw new Error("A started operational failure remains an observed zero");
    if (["judge_unknown", "setup_failed", "pending"].includes(result.status) && result.groundedSuccess !== null) throw new Error("Unavailable judgment or execution cannot become a numeric reward");
  }
}

export function summarize(run: EvaluationRun) {
  validateRun(run);
  const enrolled = run.cohort.length;
  const explicitPending = run.outcomes.filter(o => o.status === "pending").length;
  const pending = enrolled - run.outcomes.length + explicitPending;
  const unknown = run.outcomes.filter(o => ["judge_unknown", "setup_failed"].includes(o.status)).length;
  const complete = run.outcomes.filter(o => o.status === "complete").length;
  const failures = run.outcomes.filter(o => o.status === "operational_failure").length;
  const successes = run.outcomes.reduce((n, o) => n + (o.groundedSuccess === 1 ? 1 : 0), 0);
  const numeric = (key: "nativeAccuracy" | "atomicCoverage") => {
    const values = run.outcomes.map(o => o[key]).filter((v): v is number => v !== null);
    return { observed: values.length, unavailable: enrolled - values.length,
      fullCohortMean: values.length === enrolled ? values.reduce((a, b) => a + b, 0) / enrolled : null };
  };
  const telemetry = (key: "costUsd" | "latencyMs") => {
    const values = run.outcomes.map(o => o[key]).filter((v): v is number => v !== null);
    return { observedTotal: values.reduce((a, b) => a + b, 0), observed: values.length, unavailable: enrolled - values.length,
      total: values.length === enrolled ? values.reduce((a, b) => a + b, 0) : null };
  };
  return {
    schema: "gptgrep.opt-summary.v1", runId: run.runId, revision: run.revision,
    fingerprint: scenarioFingerprint(run), split: run.split, enrolled, complete, failures, unknown, pending, successes,
    groundedSuccess: { lowerBound: successes / enrolled, upperBound: (successes + unknown + pending) / enrolled,
      final: unknown === 0 && pending === 0 ? successes / enrolled : null },
    nativeAccuracy: numeric("nativeAccuracy"), atomicCoverage: numeric("atomicCoverage"),
    latencyMs: telemetry("latencyMs"), costUsd: telemetry("costUsd"),
    comparable: unknown === 0 && pending === 0,
  };
}

export function compare(incumbent: EvaluationRun, candidate: EvaluationRun, options: { minimumImprovement?: number; maxCostRatio?: number } = {}) {
  const before = summarize(incumbent), after = summarize(candidate);
  const minimumImprovement = options.minimumImprovement ?? 0;
  if (!Number.isFinite(minimumImprovement) || minimumImprovement < 0 || minimumImprovement > 1) throw new Error("Invalid improvement threshold");
  if (options.maxCostRatio !== undefined && (!Number.isFinite(options.maxCostRatio) || options.maxCostRatio <= 0)) throw new Error("Invalid cost constraint");
  const reasons: string[] = [];
  if (before.fingerprint !== after.fingerprint) reasons.push("scenario_or_cohort_changed_remeasure_both");
  if (candidate.parentRevision !== incumbent.revision) reasons.push("candidate_parent_is_not_current_incumbent");
  if (candidate.revision === incumbent.revision) reasons.push("candidate_has_no_source_change");
  if (!before.comparable || !after.comparable) reasons.push("incomplete_or_unknown_outcomes");
  // Repeated Opt search only uses development material; frozen runs are confirmation evidence.
  if (candidate.split !== "dev" || incumbent.split !== "dev") reasons.push("frozen_split_cannot_select_next_opt_parent");
  const improvement = before.groundedSuccess.final !== null && after.groundedSuccess.final !== null
    ? after.groundedSuccess.final - before.groundedSuccess.final : null;
  if (improvement === null || improvement <= 0 || improvement < minimumImprovement) reasons.push("no_required_grounded_success_improvement");
  if (options.maxCostRatio !== undefined) {
    if (before.costUsd.total === null || after.costUsd.total === null) reasons.push("cost_constraint_unobservable");
    else if (after.costUsd.total > before.costUsd.total * options.maxCostRatio) reasons.push("cost_constraint_exceeded");
  }
  return { schema: "gptgrep.opt-comparison.v1", incumbent: before, candidate: after, improvement,
    minimumImprovement, eligibleNextParent: reasons.length === 0, reasons,
    releaseAccepted: false, acceptanceScope: "development_candidate_selection_only" };
}

const ALLOWED_SURFACES = ["crates/", "skills/", "runtime/", "prompts/"];
export function validateMutationPaths(paths: string[]): void {
  if (!paths.length || new Set(paths).size !== paths.length) throw new Error("Explicit unique mutation paths are required");
  for (const path of paths) {
    if (path.startsWith("/") || path.includes("\\") || path.split("/").some(p => !p || p === "." || p === "..") ||
        !ALLOWED_SURFACES.some(prefix => path.startsWith(prefix))) throw new Error(`Mutation outside product/harness surfaces: ${path}`);
  }
}

/** A loop plan is a request to the checked worktree/storage launcher, never execution authority. */
export function planLoop(options: { baselineRevision: string; parent: EvaluationRun; iteration: number; mutationPaths: string[] }) {
  validateRun(options.parent); validateMutationPaths(options.mutationPaths);
  if (!/^[a-f0-9]{40,64}$/.test(options.baselineRevision) || !Number.isSafeInteger(options.iteration) || options.iteration < 1) throw new Error("Frozen baseline and positive iteration required");
  if (options.parent.split !== "dev" || !summarize(options.parent).comparable) throw new Error("Opt parent must have complete development measurements");
  return { schema: "gptgrep.opt-loop-plan.v1", iteration: options.iteration,
    baselineRevision: options.baselineRevision, parentRevision: options.parent.revision,
    scenarioFingerprint: scenarioFingerprint(options.parent), cohort: [...options.parent.cohort],
    mutationPaths: [...options.mutationPaths], branch: `experiment/0.2-opt-${options.iteration}`,
    isolation: "new_worktree_from_frozen_0.2_baseline_then_explicit_parent_delta",
    storageAdmissionRequired: true, executionAuthorizedByPlan: false,
    forbiddenInputs: ["gold answers", "evidence annotations", "frozen confirmation feedback", "credential files"],
  };
}
