import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadReaderEpisode, outcomeFromEvidence, sealReaderEpisode, writeEvaluationRun } from "../src/episode";
import { fileHash, sha256 } from "../src/io";
import type { Pin } from "../src/availability";

/** Synthetic artifacts exercise contracts only; no runtime/provider qualification. */
async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "episode-contract-"));
  const old = { binding: process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING, digest: process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST };
  const write = async (name: string, value: unknown): Promise<Pin> => {
    const path = join(root, name); await writeFile(path, JSON.stringify(value) + "\n"); return { path, sha256: await fileHash(path) };
  };
  const storage = await write("storage.json", { plan: { targets: { output: root } } });
  process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = storage.path; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = storage.sha256;
  const answer = await write("answer.json", { answer: "fixture", citations: [], explanation: "contract only" });
  const native: any = { executionMode: "live", status: "completed", validStart: true, threadId: "synthetic-reader", sessionId: null, turnId: "reader-turn",
    cleanup: { completed: true }, modelBoundaryValid: true, traceTruncated: false, requestedModel: "gpt-6.1-sol", observedModel: "gpt-6.1-sol", requestedEffort: "high", requestedServiceTier: "default", runtimeBinarySha256: sha256("codex"), latencyMs: 25, costUsd: null };
  const build = await write("build.json", { schema: "gptgrep.product-build.v1", sourceRevision: "a".repeat(40), binarySha256: sha256("product"), runtimeManifestSha256: sha256("runtime") });
  const attempt: any = { schema: "gptgrep.reader-attempt.v1", executionMode: "live", taskId: "task", arm: "bare", validStart: true, status: "completed", native,
    startedAt: "2026-10-25T00:00:00Z", completedAt: "2026-10-25T00:00:01Z", answerPath: answer.path, answerSha256: answer.sha256, corpusSha256: sha256("corpus"),
    contextSha256: sha256("context"), readerTaskSha256: sha256("task"), preparationSha256: sha256("preparation"), commonPromptSha256: sha256("prompt"),
    productRevision: "a".repeat(40), binarySha256: sha256("product"), runtimeManifestSha256: sha256("runtime"), buildReceiptSha256: build.sha256 };
  const expected = await write("expected.json", { schema: "gptgrep.verifier-binding.v1", task_id: "task", corpus_sha256: attempt.corpusSha256,
    rubric_sha256: sha256("rubric"), native_scorer_sha256: sha256("scorer"), reader_task_sha256: sha256("task"), judge_reference_sha256: sha256("gold") });
  const run: any = { schema: "gptgrep.opt-evaluation.v1", runId: "fixture-run", arm: "bare", revision: "a".repeat(40), parentRevision: null, cohort: ["task", "pending"], split: "dev",
    scenario: { datasetRevision: "fixture", corpusSha256: attempt.corpusSha256, rubricSha256: sha256("rubric"), nativeScorerSha256: sha256("scorer"),
      readerModel: "gpt-6.1-sol", workerModel: "gpt-6-luna", ocrModel: "gpt-6-luna", helperModel: "helper", reasoningEffort: "high", serviceTier: "default",
      judgeModel: "gpt-6.1-sol", judgeReasoningEffort: "high", judgeServiceTier: "default", environmentSha256: sha256("env"), preparationPolicySha256: sha256("prep"), seed: "fixed" } };
  const evaluationValue = { schema: "gptgrep.evaluation-plan.v1", run,
    runtime: { sourceRevision: run.revision, runtimeManifestSha256: attempt.runtimeManifestSha256, binarySha256: attempt.binarySha256, buildReceipt: build, codexBinarySha256: native.runtimeBinarySha256 },
    tasks: { task: { commonPromptSha256: attempt.commonPromptSha256, preparationSha256: attempt.preparationSha256, contextSha256: attempt.contextSha256, corpusSha256: attempt.corpusSha256 } } };
  const evaluation = await write("evaluation.json", evaluationValue);
  let sequence = 0;
  const seal = async () => sealReaderEpisode({ attempt: await write("attempt.json", attempt), expected, evaluation, outputPath: join(root, `binding-${sequence++}.json`) });
  const restore = async () => {
    for (const [name, value] of [["ENVIRONMENTS_HARNESS_STORAGE_BINDING", old.binding], ["ENVIRONMENTS_HARNESS_STORAGE_DIGEST", old.digest]]) {
      if (value === undefined) delete process.env[name!]; else process.env[name!] = value;
    }
    await rm(root, { recursive: true, force: true });
  };
  return { root, answer, attempt, native, run, expected, evaluation, evaluationValue, write, seal, restore };
}

test("post-reader seal rejects synthetic mode, missing terminal identity and changed answer", async () => {
  const f = await fixture();
  try {
    f.native.executionMode = "test"; await expect(f.seal()).rejects.toThrow("Synthetic"); f.native.executionMode = "live";
    f.native.turnId = null; await expect(f.seal()).rejects.toThrow("native/context"); f.native.turnId = "reader-turn";
    const binding = await f.seal(); expect((await loadReaderEpisode(binding)).answer_sha256).toBe(f.answer.sha256);
    await writeFile(f.answer.path, "changed"); await expect(loadReaderEpisode(binding)).rejects.toThrow("answer changed");
  } finally { await f.restore(); }
});

test("old product, prompt, preparation or reference identities cannot be relabelled under new enrollment", async () => {
  const f = await fixture();
  try {
    for (const field of ["productRevision", "binarySha256", "runtimeManifestSha256", "commonPromptSha256", "preparationSha256", "readerTaskSha256"]) {
      const before = f.attempt[field]; f.attempt[field] = sha256("changed");
      await expect(f.seal()).rejects.toThrow(); f.attempt[field] = before;
    }
    f.evaluationValue.run.revision = "b".repeat(40);
    Object.assign(f.evaluation, await f.write("evaluation.json", f.evaluationValue));
    await expect(f.seal()).rejects.toThrow("frozen enrollment");
  } finally { await f.restore(); }
});

test("scoring requires fresh judge identity and exact answer/terminal/feedback binding", async () => {
  const f = await fixture();
  try {
    const binding = await f.seal(), episode = await loadReaderEpisode(binding);
    const identity = { task_id: "task", answer_sha256: episode.answer_sha256, corpus_sha256: episode.corpus_sha256, rubric_sha256: episode.rubric_sha256, reader_binding_sha256: binding.sha256 };
    const judgment: any = { schema: "gptgrep.grounding-judgment.v1", ...identity, status: "complete", grounded_success: 1, atomic_coverage: 1,
      judge_run: { ...f.native, threadId: "fresh-judge", turnId: "judge-turn" } };
    const judgmentPin = await f.write("judgment.json", judgment);
    const feedback: any = { schema: "gptgrep.xl-verifier-feedback.v1", ...identity, status: "complete", rewardkit_status: "complete",
      native: { status: "complete", scorer_sha256: episode.native_scorer_sha256, enrolled_count: 1, evaluated_count: 1,
        metrics: { native_accuracy: 0.5, native_token_f1: 0.4, native_anls: 0.3 } }, grounding: { status: "complete" },
      independent_grounding: { judgment_sha256: judgmentPin.sha256 }, metrics: { grounded_success: 1, atomic_coverage: 1, native_accuracy: 0.5, native_token_f1: 0.4, native_anls: 0.3 } };
    const evidence = { taskId: "task", binding, judgment: judgmentPin, feedback: await f.write("feedback.json", feedback) };
    expect((await outcomeFromEvidence(evidence)).groundedSuccess).toBe(1);
    judgment.judge_run.threadId = "synthetic-reader"; evidence.judgment = await f.write("judgment.json", judgment);
    feedback.independent_grounding.judgment_sha256 = evidence.judgment.sha256; evidence.feedback = await f.write("feedback.json", feedback);
    await expect(outcomeFromEvidence(evidence)).rejects.toThrow("fresh");
    expect((await outcomeFromEvidence({ ...evidence, judgment: null })).status).toBe("judge_unknown");
  } finally { await f.restore(); }
});

test("writer retains every frozen slot, separates started failures/setup, refuses scenario mixing and overwrite", async () => {
  const f = await fixture();
  try {
    f.attempt.status = "timeout"; f.native.status = "timeout";
    const binding = await f.seal(), evidence = { taskId: "task", binding, feedback: null, judgment: null };
    const path = join(f.root, "run.json");
    const run = await writeEvaluationRun({ evaluation: f.evaluation, episodes: [evidence], outputPath: path });
    expect(run.outcomes.map(o => o.status)).toEqual(["operational_failure", "pending"]);
    expect(run.outcomes[0]!.groundedSuccess).toBe(0); expect(run.outcomes[0]!.costUsd).toBeNull();
    const otherPlan = await f.write("other-plan.json", { ...f.evaluationValue, runtime: { ...f.evaluationValue.runtime, binarySha256: sha256("other product") } });
    await expect(writeEvaluationRun({ evaluation: otherPlan, episodes: [evidence], outputPath: join(f.root, "wrong.json") })).rejects.toThrow("different frozen");
    await expect(writeEvaluationRun({ evaluation: f.evaluation, episodes: [evidence], outputPath: path })).rejects.toThrow();
    f.attempt.status = "setup_failed"; f.attempt.validStart = false; f.native.validStart = false; f.native.status = "setup_failed";
    const failed = await f.seal(); expect((await outcomeFromEvidence({ ...evidence, binding: failed })).groundedSuccess).toBeNull();
  } finally { await f.restore(); }
});
