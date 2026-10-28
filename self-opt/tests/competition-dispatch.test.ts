/** Offline guard fixtures only: typed reports below are not real C3/O4 qualification. */
import { expect, test } from "bun:test";
import { chmod, mkdir, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { CAMPAIGN_PURPOSES, type CampaignEnvelopeV2 } from "../../benchmark/src/campaign";
import { digest, fileHash, requireStorage } from "../../benchmark/src/io";
import { type CompetitionContract, type Pin } from "../src/competition";
import { dispatchCompetitionAction, preflightCompetitionAction, validateActionSlots, verifyC3Gate,
  type CompetitionDispatchPlan, type CompetitionDispatchTicket } from "../src/competition-dispatch";
import { scenarioFingerprint, type EvaluationRun } from "../src/evaluate";
import { type NativeReceiptPolicy } from "../src/native-receipts";

const HASH = "a".repeat(64), PARENT = "1".repeat(40);
const comparator = fileURLToPath(new URL("../src/evaluate.ts", import.meta.url));
const tasks = Array.from({ length: 135 }, (_, index) => `fixture-task-${String(index).padStart(3, "0")}`);
const selected = tasks.slice(0, 4), candidates = ["candidate-a", "candidate-b"];
const placeholder = (name: string): Pin => ({ path: `/fixture/${name}.json`, sha256: HASH });
const clone = <T>(value: T): T => structuredClone(value);

function run(arm: "bare" | "gptgrep"): EvaluationRun {
  return { schema: "gptgrep.opt-evaluation.v1", runId: `offline-fixture-${arm}`, arm, revision: PARENT, parentRevision: null, split: "dev", cohort: [...tasks],
    scenario: { datasetRevision: "offline-fixture", corpusSha256: HASH, rubricSha256: HASH, nativeScorerSha256: HASH,
      readerModel: "fixture-reader", workerModel: "fixture-worker", ocrModel: "fixture-ocr", helperModel: "fixture-helper",
      reasoningEffort: "fixture", serviceTier: "fixture", judgeModel: "fixture-judge", judgeReasoningEffort: "fixture", judgeServiceTier: "fixture",
      environmentSha256: HASH, preparationPolicySha256: HASH, seed: "offline-fixture" },
    outcomes: tasks.map(taskId => ({ taskId, status: "complete", groundedSuccess: 1, nativeAccuracy: null, atomicCoverage: null, latencyMs: null, costUsd: null })) };
}
function competition(): CompetitionContract {
  return { schema: "gptgrep.opt-competition.v2", competitionId: "fixture-c4", scope: "orchestration_qualification", parentRevision: PARENT,
    scenarioFingerprint: scenarioFingerprint({ ...run("gptgrep"), cohort: selected }), cohort: [...selected], candidateIds: [...candidates],
    mutationPaths: ["prompts/reader.md"], comparator: { path: comparator, sha256: HASH }, deadlineAt: Date.now() + 60_000,
    reservationLimits: { engineer: 2, review: 2, reader: 12, judge: 12, probe: 0, repair: 0 }, generations: 1,
    compositionAllowed: false, paidExecutionQualified: false, nativePolicy: placeholder("native-policy") };
}
function actionSlots(): CompetitionDispatchPlan["actions"] {
  const actions: CompetitionDispatchPlan["actions"] = [];
  for (const candidateId of candidates) for (const purpose of ["native_engineer", "agy"] as const) {
    actions.push({ id: `${purpose}-${candidateId}`, purpose, candidateId, taskId: null });
  }
  for (const candidateId of ["incumbent", ...candidates]) for (const taskId of selected) for (const purpose of ["c4_reader", "verifier", "c4_judge"] as const) {
    actions.push({ id: `${purpose}-${candidateId}-${taskId}`, purpose, candidateId, taskId });
  }
  return actions;
}
function plan(): CompetitionDispatchPlan {
  return { schema: "gptgrep.opt-dispatch-plan.v1", competition: placeholder("competition"), campaign: placeholder("campaign"),
    baseline: { manifest: placeholder("manifest"), report: placeholder("report"), review: placeholder("review") },
    ownerRuntime: { python: placeholder("python"), cli: placeholder("cli"), implementation: placeholder("implementation") },
    outputRoot: "/fixture/output", actions: actionSlots() };
}

test("C4 action matrix contains two engineers, two reviews and twelve matched reader/verifier/judge slots", () => {
  const value = plan();
  expect(value.actions).toHaveLength(40);
  expect(() => validateActionSlots(value, competition())).not.toThrow();
  const withPrepare = clone(value);
  withPrepare.actions.push({ id: "prepare-incumbent", purpose: "prepare_context", candidateId: "incumbent", taskId: null });
  expect(() => validateActionSlots(withPrepare, competition())).not.toThrow();
});

test("C4 action slots reject missing, duplicated, renamed and foreign-task work", () => {
  for (const mutate of [
    (p: CompetitionDispatchPlan) => { p.actions.pop(); },
    (p: CompetitionDispatchPlan) => { p.actions.push(clone(p.actions[0]!)); },
    (p: CompetitionDispatchPlan) => { p.actions[1] = { ...p.actions[0]!, id: "different-id-same-actor-slot" }; },
    (p: CompetitionDispatchPlan) => { p.actions[4]!.taskId = tasks[4]!; },
    (p: CompetitionDispatchPlan) => { p.actions[4]!.candidateId = "unfrozen-candidate"; },
    (p: CompetitionDispatchPlan) => { p.actions[0]!.candidateId = "incumbent"; },
    (p: CompetitionDispatchPlan) => { p.actions.push({ id: "prepare-foreign-task", purpose: "prepare_context", candidateId: "incumbent", taskId: tasks[4]! }); },
  ]) {
    const value = plan(); mutate(value);
    expect(() => validateActionSlots(value, competition())).toThrow();
  }
});

async function fixtureRoot(): Promise<string> {
  const admitted = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!admitted) throw new Error("Dispatch fixtures require an explicitly bound external output; no temporary fallback");
  const root = join(admitted, "competition-dispatch-fixtures", crypto.randomUUID());
  await requireStorage(root); await mkdir(root, { recursive: true, mode: 0o700 }); return root;
}
async function pin(root: string, name: string, value: unknown): Promise<Pin> {
  const path = join(root, name); await requireStorage(path);
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx", mode: 0o600 });
  return { path, sha256: await fileHash(path) };
}
interface BaselineMutation { unknown?: boolean; incompleteBatch?: boolean; runtimeUnknown?: boolean; allFailures?: boolean; mismatchedScenario?: boolean }
async function baselineFixture(root: string, mutation: BaselineMutation = {}, envelope = placeholder("campaign")) {
  const manifest = await pin(root, "fixture-manifest.json", { schema: "gptgrep.paired-controller.v2", taskIds: tasks, envelope, fixtureOnly: true });
  const before = run("bare"), after = run("gptgrep");
  if (mutation.mismatchedScenario) before.scenario.environmentSha256 = "b".repeat(64);
  if (mutation.unknown) after.outcomes[0] = { ...after.outcomes[0]!, status: "judge_unknown", groundedSuccess: null };
  if (mutation.allFailures) for (const value of [before, after]) value.outcomes = value.outcomes.map(outcome => ({ ...outcome, status: "operational_failure", groundedSuccess: 0 }));
  const runs = { bare: await pin(root, "fixture-bare.json", before), gptgrep: await pin(root, "fixture-gptgrep.json", after) };
  const report = await pin(root, "fixture-report.json", { schema: "gptgrep.controller-batch-report.v1", manifest,
    status: "pending_independent_review", runs, captures: [], cumulativeTasks: mutation.incompleteBatch ? 134 : 135, enrolledTasks: 135, error: null, fixtureOnly: true });
  const runtimeReview = await pin(root, "fixture-runtime-review.json", { schema: "gptgrep.controller-runtime-review.v1", status: "qualified", manifest,
    reports: [report], observedCapabilities: ["offline_fixture_only"], unavailableCapabilities: mutation.runtimeUnknown ? ["fixture-missing-capability"] : [], fixtureOnly: true });
  const review = await pin(root, "fixture-baseline-review.json", { schema: "gptgrep.controller-batch-review.v1", status: "accepted_for_next_batch",
    manifest, report, runtimeReview, fixtureOnly: true });
  return { manifest, report, review };
}

test("C3 gate mechanics require complete 135-task outcomes and meaningful runs under the same scenario", async () => {
  const root = await fixtureRoot(), baseline = await baselineFixture(root);
  const value = { ...plan(), baseline }, contract = competition();
  await expect(verifyC3Gate(value, contract)).resolves.toBeUndefined();
  await expect(verifyC3Gate(value, { ...contract, parentRevision: "2".repeat(40) })).rejects.toThrow("parent/scenario/tasks");
  await expect(verifyC3Gate(value, { ...contract, scenarioFingerprint: "b".repeat(64) })).rejects.toThrow("parent/scenario/tasks");
  await expect(verifyC3Gate({ ...value, campaign: placeholder("different-campaign") }, contract)).rejects.toThrow("same frozen campaign");
  for (const mutation of [{ unknown: true }, { incompleteBatch: true }, { runtimeUnknown: true }, { allFailures: true }, { mismatchedScenario: true }]) {
    const other = await fixtureRoot();
    await expect(verifyC3Gate({ ...plan(), baseline: await baselineFixture(other, mutation) }, contract)).rejects.toThrow();
  }
});

async function dispatchFixture() {
  const root = await fixtureRoot(), contract = competition();
  contract.deadlineAt = Date.now() + 300_000;
  contract.comparator.sha256 = await fileHash(comparator);
  // Files below model the exact wire shape, but none are executed or accepted as live evidence.
  const executable = await pin(root, "fixture-executable", { fixtureOnly: true }); await chmod(executable.path, 0o700);
  const cli = await pin(root, "fixture-owner-cli.py", { fixtureOnly: true });
  const implementation = await pin(root, "fixture-owner-implementation.py", { fixtureOnly: true });
  const producer = { path: fileURLToPath(new URL("../../benchmark/src/native.ts", import.meta.url)), sha256: "" };
  producer.sha256 = await fileHash(producer.path);
  const composer = await pin(root, "fixture-composer.py", { fixtureOnly: true });
  const agy = await pin(root, "fixture-agy.py", { fixtureOnly: true });
  const resultSchema = await pin(root, "fixture-result-schema.json", { fixtureOnly: true });
  const storage = (runId: string, cwd: string, argv = [executable.path, runId]) => ({ runId, planDigest: HASH, command: { argv, cwd } });
  const nativePolicy: NativeReceiptPolicy = { schema: "gptgrep.opt-native-policy.v1", competitionId: contract.competitionId,
    parentRevision: PARENT, scenarioFingerprint: contract.scenarioFingerprint, candidates: candidates.map((id, index) => {
      const cwd = join(root, "worktrees", id), reviewCwd = join(root, "reviews", id);
      return { id, worktree: { path: cwd, device: 1, inode: index + 1, gitDirectory: join(root, "git", id) },
        engineer: { client: "codex", binary: executable, producer, model: "fixture-engineer", effort: "medium", serviceTier: "priority", cwd, inputSha256: HASH,
          owner: { storage: storage(`${id}-engineer`, cwd), authRunId: `${id}-auth`, authRole: "engineer", authSourceId: "fixture-native-owner", command: { argv: [executable.path, id], cwd } } },
        reviewer: { client: "agy", binary: executable, producer: composer, model: "fixture-reviewer", effort: "medium", serviceTier: "not_exposed_by_client",
          cwd: reviewCwd, inputSha256: HASH, permissionPolicy: "preserve", owner: { kind: "agents_composer_agy_print", outputRoot: join(root, "reviews"),
            storage: storage(`${id}-review`, reviewCwd), sources: [composer, agy], resultSchema } },
        checks: [{ id: "candidate-check", argv: [executable.path, "check", id], owner: storage(`${id}-check`, cwd, [executable.path, "check", id]) }] };
    }) };
  contract.nativePolicy = await pin(root, "fixture-native-policy.json", nativePolicy);
  const competitionPin = await pin(root, "fixture-competition.json", contract);
  const campaign: CampaignEnvelopeV2 = { schema: "gptgrep.live-campaign-envelope.v2", campaignId: "offline-fixture-campaign", ledgerRoot: join(root, "ledger"),
    campaignDeadline: new Date(Date.now() + 300_000).toISOString(), maxReservedBytes: 65536, baseReservedBytes: 0,
    allowances: Object.fromEntries(CAMPAIGN_PURPOSES.map(purpose => [purpose, {
      maxAdmissions: purpose === "native_engineer" || purpose === "agy" ? 2 : ["c4_reader", "c4_judge", "verifier"].includes(purpose) ? 12 : 0,
      reservedBytesPerAdmission: 1024, deadlineMs: 5000,
    }])) as CampaignEnvelopeV2["allowances"] };
  const campaignPin = await pin(root, "fixture-campaign.json", campaign);
  const baseline = await baselineFixture(root, {}, campaignPin);
  const value = { ...plan(), competition: competitionPin, campaign: campaignPin, baseline, outputRoot: root, ownerRuntime: { python: executable, cli, implementation } };
  const planPin = await pin(root, "fixture-plan.json", value);
  const freezeReview = await pin(root, "fixture-freeze-review.json", { schema: "gptgrep.opt-dispatch-review.v1", status: "accepted_for_first_live_qualification",
    plan: planPin, competition: competitionPin, campaign: campaignPin, baselineReview: baseline.review, reviewerSessionId: "offline-fixture-reviewer" });
  return { root, campaign, contract, plan: planPin, freezeReview, executable, cli, implementation };
}
async function ticketFixture(f: Awaited<ReturnType<typeof dispatchFixture>>, actionId: string,
    change?: (ticket: CompetitionDispatchTicket) => void) {
  const policy = await pin(f.root, `${actionId}.policy.json`, { fixtureOnly: true });
  const workload = await pin(f.root, `${actionId}.workload.json`, { fixtureOnly: true });
  const workloadArgv = [f.executable.path, "fixture-workload", actionId];
  const identity = (argv: string[]) => ({ executable: f.executable.path, executable_sha256: f.executable.sha256,
    argv_sha256: digest(argv), argument_count: argv.length, arguments: "not_retained" });
  const body = { schema: "environments-harness.storage-plan.v1", implementation_sha256: f.implementation.sha256, cwd: f.root,
    command: identity(workloadArgv), bootstrap_python: identity([f.executable.path]), policy: policy.path, policy_sha256: policy.sha256,
    workload: workload.path, workload_sha256: workload.sha256, workload_declaration: { fixtureOnly: true } };
  const ownerPlan = await pin(f.root, `${actionId}.fixture-owner-plan.json`, { ...body, digest: digest(body) });
  const ticket: CompetitionDispatchTicket = { schema: "gptgrep.opt-dispatch-ticket.v1", plan: f.plan, actionId, ownerPlan, workloadArgv,
    command: { executable: f.executable, inputs: [f.cli, f.implementation, f.executable, ownerPlan],
      argv: ["-B", f.cli.path, "storage", "run", "--plan", ownerPlan.path, "--expect-digest", digest(body), "--yes", "--", ...workloadArgv],
      cwd: f.root, timeoutMs: 3000, maxOutputBytes: 4096 } };
  change?.(ticket);
  const ticketPin = await pin(f.root, `${actionId}.fixture-ticket.json`, ticket);
  const ticketReview = await pin(f.root, `${actionId}.fixture-ticket-review.json`, { schema: "gptgrep.opt-dispatch-ticket-review.v1", status: "accepted_for_owner_launch",
    ticket: ticketPin, freezeReview: f.freezeReview, reviewerSessionId: "offline-fixture-ticket-reviewer" });
  return { ticket, options: { plan: f.plan, freezeReview: f.freezeReview, ticket: ticketPin, ticketReview } };
}

test("fully shaped preflight remains unavailable and creates no campaign ledger or process", async () => {
  const f = await dispatchFixture(), t = await ticketFixture(f, "native_engineer-candidate-a");
  const result = await preflightCompetitionAction(t.options);
  expect(result.sourcePreflight).toBe("ready"); expect(result.actorAdmission).toBe("unavailable");
  expect(result.measurementAdmission).toBe("unavailable"); expect(result.charged).toBe(false); expect(result.launched).toBe(false);
  await expect(dispatchCompetitionAction(t.options)).rejects.toThrow("unavailable before charge or launch");
  await expect(stat(f.campaign.ledgerRoot)).rejects.toThrow();
  const measured = await ticketFixture(f, `c4_reader-incumbent-${selected[0]}`);
  expect((await preflightCompetitionAction(measured.options)).measurementAdmission).toBe("unavailable");
  await expect(stat(f.campaign.ledgerRoot)).rejects.toThrow();
});

test("decorative owner plans, replaced workload argv and missing source inputs are rejected before charge", async () => {
  for (const change of [
    (t: CompetitionDispatchTicket) => { t.command.argv = ["fixture-direct-launch"]; },
    (t: CompetitionDispatchTicket) => { t.workloadArgv[1] = "changed-payload"; },
    (t: CompetitionDispatchTicket) => { t.command.inputs = [t.ownerPlan]; },
    (t: CompetitionDispatchTicket) => { t.command.timeoutMs = 3001; },
  ]) {
    const f = await dispatchFixture(), t = await ticketFixture(f, "native_engineer-candidate-a", change);
    await expect(preflightCompetitionAction(t.options)).rejects.toThrow();
    await expect(stat(f.campaign.ledgerRoot)).rejects.toThrow();
  }
});

test("an unreviewed owner ticket cannot reach native admission", async () => {
  const f = await dispatchFixture(), t = await ticketFixture(f, "native_engineer-candidate-a");
  const wrongReview = await pin(f.root, "wrong-ticket-review.json", { schema: "gptgrep.opt-dispatch-ticket-review.v1", status: "accepted_for_owner_launch",
    ticket: { ...t.options.ticket, sha256: "b".repeat(64) }, freezeReview: f.freezeReview, reviewerSessionId: "offline-fixture-ticket-reviewer" });
  await expect(preflightCompetitionAction({ ...t.options, ticketReview: wrongReview })).rejects.toThrow("not been reviewed");
  await expect(stat(f.campaign.ledgerRoot)).rejects.toThrow();
});
