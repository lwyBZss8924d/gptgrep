import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { canonical, fileHash, physicalPath, readJson, requireStorage } from "./io";
import { verifyPin, type Pin } from "./availability";
import type { ReaderArmResult } from "./runner";
import { normalizeOutcome, validateRun, type EvaluationRun, type Outcome } from "../../self-opt/src/evaluate";

/** Controller-owned post-reader binding. Never stage this in the reader image. */
export interface ReaderEpisode {
  schema: "gptgrep.verifier-binding.v2";
  execution_mode: "live";
  task_id: string;
  arm: "bare" | "gptgrep";
  corpus_sha256: string;
  rubric_sha256: string;
  native_scorer_sha256: string;
  reader_task_sha256: string;
  judge_reference_sha256: string;
  expected: Pin;
  evaluation: Pin;
  attempt: Pin;
  answer_sha256: string | null;
  reader_status: ReaderArmResult["status"];
  reader_valid_start: boolean;
  reader_thread_id: string | null;
  reader_session_id: string | null;
  reader_turn_id: string | null;
}
const validHash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);

async function constructEpisode(attemptPin: Pin, expectedPin: Pin, evaluationPin: Pin): Promise<ReaderEpisode> {
  await verifyPin(attemptPin); await verifyPin(expectedPin); await verifyPin(evaluationPin);
  const attempt = await readJson<ReaderArmResult>(attemptPin.path), expected = await readJson(expectedPin.path);
  const evaluation = await readJson(evaluationPin.path);
  if (evaluation.schema !== "gptgrep.evaluation-plan.v1") throw new Error("Frozen evaluation plan is required");
  validateRun({ ...evaluation.run, outcomes: [] });
  if (evaluation.run.arm !== attempt.arm || !evaluation.run.cohort.includes(attempt.taskId) || evaluation.run.scenario.rubricSha256 !== expected.rubric_sha256 ||
      evaluation.run.scenario.nativeScorerSha256 !== expected.native_scorer_sha256) throw new Error("Episode differs from frozen evaluation enrollment");
  if (expected.schema !== "gptgrep.verifier-binding.v1" || !validHash(expected.corpus_sha256) || !validHash(expected.rubric_sha256) || !validHash(expected.native_scorer_sha256) ||
      !validHash(expected.reader_task_sha256) || !validHash(expected.judge_reference_sha256) || attempt.readerTaskSha256 !== expected.reader_task_sha256) throw new Error("Exact exported task, reference, rubric and scorer bindings are required");
  const runtime = evaluation.runtime, task = evaluation.tasks?.[attempt.taskId];
  if (!runtime || !task || runtime.sourceRevision !== evaluation.run.revision || attempt.productRevision !== runtime.sourceRevision ||
      attempt.runtimeManifestSha256 !== runtime.runtimeManifestSha256 || attempt.binarySha256 !== runtime.binarySha256 ||
      attempt.buildReceiptSha256 !== runtime.buildReceipt?.sha256 || attempt.commonPromptSha256 !== task.commonPromptSha256 ||
      attempt.preparationSha256 !== task.preparationSha256 || (attempt.validStart || attempt.contextSha256 !== null) && attempt.contextSha256 !== task.contextSha256 ||
      (attempt.validStart || attempt.corpusSha256 !== null) && attempt.corpusSha256 !== task.corpusSha256 || task.corpusSha256 !== expected.corpus_sha256 ||
      ![runtime.runtimeManifestSha256, runtime.binarySha256, runtime.codexBinarySha256, task.commonPromptSha256, task.preparationSha256, task.contextSha256].every(validHash)) throw new Error("Reader product, runtime, prompt or preparation differs from frozen enrollment");
  await verifyPin(runtime.buildReceipt);
  const build = await readJson(runtime.buildReceipt.path);
  if (build.schema !== "gptgrep.product-build.v1" || build.sourceRevision !== runtime.sourceRevision || build.binarySha256 !== runtime.binarySha256 ||
      build.runtimeManifestSha256 !== runtime.runtimeManifestSha256) throw new Error("Frozen product build receipt differs from runtime");
  if (attempt.schema !== "gptgrep.reader-attempt.v1" || attempt.executionMode !== "live" ||
      attempt.taskId !== expected.task_id || !["bare", "gptgrep"].includes(attempt.arm) ||
      !["completed", "failed", "timeout", "setup_failed", "answer_invalid"].includes(attempt.status) ||
      typeof attempt.validStart !== "boolean" || !Number.isFinite(Date.parse(attempt.startedAt)) ||
      !Number.isFinite(Date.parse(attempt.completedAt)) || Date.parse(attempt.completedAt) < Date.parse(attempt.startedAt)) throw new Error("Reader attempt has no matching terminal identity");
  const native = attempt.native;
  if (native && (native.executionMode !== "live" || native.validStart !== attempt.validStart ||
      !["completed", "failed", "timeout", "setup_failed"].includes(native.status) || native.validStart && native.status === "setup_failed")) throw new Error("Synthetic or inconsistent native attempt");
  if (native && (native.requestedModel !== evaluation.run.scenario.readerModel || native.requestedEffort !== evaluation.run.scenario.reasoningEffort ||
      native.requestedServiceTier !== evaluation.run.scenario.serviceTier || native.runtimeBinarySha256 !== runtime.codexBinarySha256)) throw new Error("Reader configuration differs from the frozen scenario");
  if (attempt.validStart && (!native || !native.threadId || !native.turnId || attempt.corpusSha256 !== expected.corpus_sha256)) throw new Error("Started reader lacks exact native/context identity");
  if (attempt.validStart && (attempt.status === "setup_failed" || !native!.modelBoundaryValid || native!.observedModel !== native!.requestedModel)) throw new Error("Started reader has configuration drift or inconsistent status");
  if (!attempt.validStart && attempt.status !== "setup_failed") throw new Error("Unstarted reader cannot be a scored terminal episode");
  if (attempt.status === "completed" && (!native || native.status !== "completed" || !native.cleanup.completed || !native.modelBoundaryValid || native.traceTruncated || native.authFailure)) throw new Error("Successful reader has an invalid native terminal boundary");
  if (attempt.answerPath !== null) {
    if (!validHash(attempt.answerSha256) || await fileHash(attempt.answerPath) !== attempt.answerSha256) throw new Error("Frozen reader answer changed");
  } else if (attempt.answerSha256 !== null || attempt.status === "completed") throw new Error("Completed reader has no frozen answer");
  return { schema: "gptgrep.verifier-binding.v2", execution_mode: "live", task_id: attempt.taskId, arm: attempt.arm,
    corpus_sha256: expected.corpus_sha256, rubric_sha256: expected.rubric_sha256, native_scorer_sha256: expected.native_scorer_sha256,
    reader_task_sha256: expected.reader_task_sha256, judge_reference_sha256: expected.judge_reference_sha256,
    expected: expectedPin, evaluation: evaluationPin, attempt: attemptPin, answer_sha256: attempt.answerSha256, reader_status: attempt.status,
    reader_valid_start: attempt.validStart, reader_thread_id: native?.threadId ?? null,
    reader_session_id: native?.sessionId ?? null, reader_turn_id: native?.turnId ?? null };
}

export async function sealReaderEpisode(options: { attempt: Pin; expected: Pin; evaluation: Pin; outputPath: string }): Promise<Pin> {
  await requireStorage(options.outputPath);
  const value = await constructEpisode(options.attempt, options.expected, options.evaluation);
  await physicalPath(options.outputPath); await mkdir(dirname(options.outputPath), { recursive: true });
  await writeFile(options.outputPath, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx", mode: 0o600 });
  return { path: resolve(options.outputPath), sha256: await fileHash(options.outputPath) };
}

export async function loadReaderEpisode(pin: Pin): Promise<ReaderEpisode> {
  await verifyPin(pin);
  const value = await readJson<ReaderEpisode>(pin.path);
  if (value.schema !== "gptgrep.verifier-binding.v2" || canonical(value) !== canonical(await constructEpisode(value.attempt, value.expected, value.evaluation))) throw new Error("Post-reader binding differs from its exact terminal evidence");
  return value;
}

export interface EpisodeEvidence { taskId: string; binding: Pin | null; feedback: Pin | null; judgment: Pin | null }

/** A measured outcome is derived from immutable artifacts, never caller-supplied scores. */
export async function outcomeFromEvidence(evidence: EpisodeEvidence): Promise<Outcome> {
  if (!evidence.binding) return normalizeOutcome({ taskId: evidence.taskId, status: "pending" });
  const binding = await loadReaderEpisode(evidence.binding);
  if (binding.task_id !== evidence.taskId) throw new Error("Episode is outside its enrolled task slot");
  const attempt = await readJson<ReaderArmResult>(binding.attempt.path);
  const telemetry = { latencyMs: attempt.native?.latencyMs ?? null, costUsd: attempt.native?.costUsd ?? null };
  if (!binding.reader_valid_start) return normalizeOutcome({ taskId: evidence.taskId, status: "setup_failed", ...telemetry });
  if (binding.reader_status !== "completed") return normalizeOutcome({ taskId: evidence.taskId, status: "operational_failure", nativeAccuracy: 0, atomicCoverage: 0, ...telemetry });
  const unknown = () => normalizeOutcome({ taskId: evidence.taskId, status: "judge_unknown", ...telemetry });
  if (!evidence.feedback || !evidence.judgment) return unknown();
  await verifyPin(evidence.feedback); await verifyPin(evidence.judgment);
  const feedback = await readJson(evidence.feedback.path), judgment = await readJson(evidence.judgment.path);
  for (const value of [feedback, judgment]) {
    if (value.task_id !== binding.task_id || value.answer_sha256 !== binding.answer_sha256 || value.corpus_sha256 !== binding.corpus_sha256 ||
        value.rubric_sha256 !== binding.rubric_sha256 || value.reader_binding_sha256 !== evidence.binding.sha256) throw new Error("Scoring artifact belongs to a different reader episode");
  }
  const judge = judgment.judge_run;
  const evaluation = await readJson(binding.evaluation.path);
  if (judgment.schema !== "gptgrep.grounding-judgment.v1" || feedback.schema !== "gptgrep.xl-verifier-feedback.v1") throw new Error("Unknown scoring artifact schema");
  if (judgment.status !== "complete" || feedback.status !== "complete" || feedback.rewardkit_status !== "complete" ||
      feedback.native?.status !== "complete" || feedback.grounding?.status !== "complete") return unknown();
  if (!judge || judge.executionMode !== "live" || judge.status !== "completed" || !judge.validStart || !judge.cleanup?.completed ||
      !judge.threadId || judge.threadId === binding.reader_thread_id || !judge.turnId || !judge.modelBoundaryValid || judge.traceTruncated ||
      judge.authFailure || feedback.independent_grounding?.judgment_sha256 !== evidence.judgment.sha256) throw new Error("Judgment lacks a fresh completed native episode");
  if (judge.requestedModel !== evaluation.run.scenario.judgeModel || judge.requestedEffort !== evaluation.run.scenario.judgeReasoningEffort ||
      judge.requestedServiceTier !== evaluation.run.scenario.judgeServiceTier || judge.runtimeBinarySha256 !== evaluation.runtime.codexBinarySha256) throw new Error("Judge configuration differs from frozen scenario");
  const metrics = feedback.metrics;
  if (feedback.native.scorer_sha256 !== binding.native_scorer_sha256 || feedback.native.enrolled_count !== 1 || feedback.native.evaluated_count !== 1 ||
      ["native_accuracy", "native_token_f1", "native_anls"].some(key => typeof metrics?.[key] !== "number" || !Number.isFinite(metrics[key]) || metrics[key] < 0 || metrics[key] > 1 || metrics[key] !== feedback.native.metrics?.[key])) throw new Error("Native metrics lack the pinned complete task denominator");
  if (!metrics || metrics.grounded_success !== judgment.grounded_success || metrics.atomic_coverage !== judgment.atomic_coverage) throw new Error("Verifier metrics differ from the bound judgment");
  return normalizeOutcome({ taskId: evidence.taskId, status: "complete", groundedSuccess: metrics.grounded_success,
    nativeAccuracy: metrics.native_accuracy ?? null, atomicCoverage: metrics.atomic_coverage, ...telemetry });
}

export async function writeEvaluationRun(options: {
  evaluation: Pin; episodes: EpisodeEvidence[]; outputPath: string;
}): Promise<EvaluationRun> {
  await requireStorage(options.outputPath);
  await verifyPin(options.evaluation);
  const plan = await readJson(options.evaluation.path);
  if (plan.schema !== "gptgrep.evaluation-plan.v1") throw new Error("Frozen evaluation plan is required");
  const run = plan.run as Omit<EvaluationRun, "outcomes">;
  validateRun({ ...run, outcomes: [] });
  if (new Set(options.episodes.map(e => e.taskId)).size !== options.episodes.length || options.episodes.some(e => !run.cohort.includes(e.taskId))) throw new Error("Duplicate or foreign episode slot");
  const byId = new Map(options.episodes.map(e => [e.taskId, e]));
  const outcomes = [];
  for (const taskId of run.cohort) {
    const evidence = byId.get(taskId) ?? { taskId, binding: null, feedback: null, judgment: null };
    if (evidence.binding) {
      const binding = await loadReaderEpisode(evidence.binding);
      if (canonical(binding.evaluation) !== canonical(options.evaluation)) throw new Error("Cannot mix episodes from different frozen evaluation plans");
    }
    outcomes.push(await outcomeFromEvidence(evidence));
  }
  const result: EvaluationRun = { ...run, outcomes }; validateRun(result);
  await physicalPath(options.outputPath); await mkdir(dirname(options.outputPath), { recursive: true });
  await writeFile(options.outputPath, `${JSON.stringify(result, null, 2)}\n`, { flag: "wx", mode: 0o600 });
  return result;
}
