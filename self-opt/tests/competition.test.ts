import { expect, test } from "bun:test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { digest, fileHash, requireStorage } from "../../benchmark/src/io";
import { scenarioFingerprint, type EvaluationRun } from "../src/evaluate";
import { CompetitionLedger, validateCompetition, type CompetitionContract, type Fence, type Pin } from "../src/competition";

const exec = promisify(execFile);
const comparator = fileURLToPath(new URL("../src/evaluate.ts", import.meta.url));
const cohort = ["fixture-one", "fixture-two", "fixture-three", "fixture-four"];
function run(revision: string, parentRevision: string | null, successes: number): EvaluationRun {
  return { schema: "gptgrep.opt-evaluation.v1", runId: `fixture-${revision.slice(0, 8)}`, arm: "gptgrep", revision, parentRevision, cohort, split: "dev",
    scenario: { datasetRevision: "fixture-release", corpusSha256: "a".repeat(64), rubricSha256: "b".repeat(64), nativeScorerSha256: "c".repeat(64),
      readerModel: "fixture-reader", workerModel: "fixture-worker", ocrModel: "fixture-ocr", helperModel: "fixture-helper", reasoningEffort: "fixture", serviceTier: "fixture",
      judgeModel: "fixture-judge", judgeReasoningEffort: "fixture", judgeServiceTier: "fixture", environmentSha256: "d".repeat(64), preparationPolicySha256: "e".repeat(64), seed: "fixture" },
    outcomes: cohort.map((taskId, index) => ({ taskId, status: "complete", groundedSuccess: index < successes ? 1 : 0, nativeAccuracy: null, atomicCoverage: null, latencyMs: null, costUsd: null })) };
}
async function fixtureRoot(): Promise<string> {
  const root = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!root) throw new Error("Competition tests require an owner-admitted external output root; no temporary fallback");
  const path = join(root, "competition-fixtures", crypto.randomUUID()); await requireStorage(path); await mkdir(path, { recursive: true }); return path;
}
async function pin(root: string, name: string, value: unknown): Promise<Pin> {
  const path = join(root, name); await writeFile(path, JSON.stringify(value)); return { path, sha256: await fileHash(path) };
}
async function setup(deadlineAt = Date.now() + 60000) {
  const root = await fixtureRoot();
  const contract: CompetitionContract = { schema: "gptgrep.opt-competition.v1", competitionId: "fixture-competition", scope: "orchestration_qualification",
    parentRevision: "1".repeat(40), scenarioFingerprint: scenarioFingerprint(run("1".repeat(40), null, 1)), cohort, candidateIds: ["candidate-a", "candidate-b"],
    mutationPaths: ["prompts/reader.md"], comparator: { path: comparator, sha256: await fileHash(comparator) }, deadlineAt,
    reservationLimits: { engineer: 2, review: 2, reader: 12, judge: 12, probe: 0, repair: 0 }, generations: 1, compositionAllowed: false, paidExecutionQualified: false };
  const contractPin = await pin(root, "contract.json", contract), path = join(root, "state.sqlite");
  const ledger = await CompetitionLedger.open(path, contractPin);
  return { root, path, ledger, contract, contractPin };
}
function lease(ledger: CompetitionLedger, eventId = "lease-1"): Fence {
  const result = ledger.acquire(ledger.snapshot().version, eventId, "fixture-controller", Date.now() + 60000);
  return { holder: result.holder, token: result.token };
}

test("contract rejects wrong comparator identity, hidden defaults and wrong task scope", async () => {
  const f = await setup();
  try {
    expect(() => validateCompetition({ ...f.contract, comparator: { ...f.contract.comparator, path: "/unrelated/comparator.ts" } })).toThrow("actual imported");
    expect(() => validateCompetition({ ...f.contract, paidExecutionQualified: true } as any)).toThrow("source-qualified");
    expect(() => validateCompetition({ ...f.contract, cohort: cohort.slice(0, 3) })).toThrow("four");
    expect(() => validateCompetition({ ...f.contract, reservationLimits: { ...f.contract.reservationLimits, probe: Infinity } })).toThrow("Finite");
  } finally { f.ledger.close(); }
});
test("durable CAS, event idempotency and fencing survive independent ledger handles", async () => {
  const f = await setup(), other = await CompetitionLedger.open(f.path);
  try {
    const fence = lease(f.ledger), v = f.ledger.snapshot().version;
    const op = f.ledger.reserve(v, "reserve-one", fence, "operation-one", "reader", "a".repeat(64));
    expect(other.snapshot().operations[op.id]).toEqual(op);
    expect(other.reserve(v, "reserve-one", fence, "operation-one", "reader", "a".repeat(64))).toEqual(op);
    expect(() => other.reserve(v, "reserve-one", fence, "operation-one", "reader", "b".repeat(64))).toThrow("different content");
    expect(() => other.reserve(v, "stale-version", fence, "operation-two", "reader", "a".repeat(64))).toThrow("compare-and-swap");
    const renewed = lease(other, "lease-2");
    expect(renewed.token).toBeGreaterThan(fence.token);
    expect(() => f.ledger.reserve(f.ledger.snapshot().version, "stale-fence", fence, "operation-two", "reader", "a".repeat(64))).toThrow("fenced");
    expect(() => f.ledger.markOperation(f.ledger.snapshot().version, "prototype", renewed, "__proto__", "unknown", null)).toThrow("Invalid");
    expect(Object.prototype).not.toHaveProperty("state");
  } finally { other.close(); f.ledger.close(); }
});
test("unknown submissions cannot be resent, cancelled as never dispatched, or reset for another budget", async () => {
  const f = await setup();
  try {
    const fence = lease(f.ledger);
    f.ledger.reserve(f.ledger.snapshot().version, "reserve-reader", fence, "reader-one", "reader", "a".repeat(64));
    f.ledger.markOperation(f.ledger.snapshot().version, "unknown-reader", fence, "reader-one", "unknown", null);
    expect(() => f.ledger.markOperation(f.ledger.snapshot().version, "retry-reader", fence, "reader-one", "submitted", "invented-job")).toThrow("cannot be submitted");
    expect(() => f.ledger.reserve(f.ledger.snapshot().version, "duplicate-reader", fence, "reader-one", "reader", "a".repeat(64))).toThrow("reconciliation");
    const leaf = await pin(f.root, "fixture-owner-leaf.json", { fixtureOnly: true });
    const terminal = await pin(f.root, "false-never-dispatched.json", { schema: "gptgrep.opt-operation-terminal.v1", operationId: "reader-one", commandSha256: "a".repeat(64), externalJobId: null,
      submission: "never_dispatched", disposition: "cancelled", ownerTerminal: leaf });
    await expect(f.ledger.reconcile(f.ledger.snapshot().version, "false-terminal", fence, "reader-one", terminal)).rejects.toThrow("attempted/uncertain");
    expect(() => f.ledger.reserve(f.ledger.snapshot().version, "unbudgeted", fence, "probe-one", "probe", "a".repeat(64))).toThrow("exhausted");
  } finally { f.ledger.close(); }
});
test("recovery leases remain available after workload deadline while new work is denied", async () => {
  const f = await setup(Date.now() - 1000);
  try {
    const fence = lease(f.ledger);
    expect(fence.token).toBe(1);
    expect(() => f.ledger.reserve(f.ledger.snapshot().version, "late-work", fence, "reader-one", "reader", "a".repeat(64))).toThrow("finite budget");
    expect(() => f.ledger.acquire(f.ledger.snapshot().version, "long-lease", fence.holder, Date.now() + 3600000)).toThrow("five minutes");
  } finally { f.ledger.close(); }
});
test("a fresh process reads reserved state without dispatch and paid execution stays unavailable", async () => {
  const f = await setup();
  try {
    const fence = lease(f.ledger); f.ledger.reserve(f.ledger.snapshot().version, "reserve-durable", fence, "reader-one", "reader", "a".repeat(64));
    const cli = fileURLToPath(new URL("../src/competition-cli.ts", import.meta.url));
    const child = Bun.spawn([process.execPath, cli, "snapshot", "--ledger", f.path], { stdin: new Blob(["{}"]), stdout: "pipe", stderr: "pipe" });
    const response = JSON.parse(await new Response(child.stdout).text());
    expect(await child.exited).toBe(0); expect(response.result.operations["reader-one"].state).toBe("reserved");
    expect(() => f.ledger.assertPaidExecutionQualified()).toThrow("unqualified");
  } finally { f.ledger.close(); }
});

test("real Git checks and strict comparison cannot turn pinned fixture declarations into accepted live evidence", async () => {
  const root = await fixtureRoot(), worktree = join(root, "repository"); await mkdir(join(worktree, "prompts"), { recursive: true });
  const git = async (...args: string[]) => (await exec("git", ["-C", worktree, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", ...args])).stdout.trim();
  await git("init", "--initial-branch=fixture"); await writeFile(join(worktree, "prompts/reader.md"), "Read the supplied context.\n"); await git("add", "."); await git("commit", "-m", "fixture parent");
  const parent = await git("rev-parse", "HEAD"); await writeFile(join(worktree, "prompts/reader.md"), "Read the complete supplied context and preserve source identity.\n"); await git("commit", "-am", "fixture candidate");
  const candidate = await git("rev-parse", "HEAD");
  const rawDiff = (await exec("git", ["-C", worktree, "diff", "--binary", "--no-ext-diff", "--no-renames", parent, candidate, "--"])).stdout;
  const baseRun = run(parent, null, 1), candidateRun = run(candidate, parent, 2);
  const contract: CompetitionContract = { schema: "gptgrep.opt-competition.v1", competitionId: "artifact-fixture", scope: "orchestration_qualification", parentRevision: parent,
    scenarioFingerprint: scenarioFingerprint(baseRun), cohort, candidateIds: ["candidate-a", "candidate-b"], mutationPaths: ["prompts/reader.md"],
    comparator: { path: comparator, sha256: await fileHash(comparator) }, deadlineAt: Date.now() + 60000,
    reservationLimits: { engineer: 2, review: 2, reader: 12, judge: 12, probe: 0, repair: 0 }, generations: 1, compositionAllowed: false, paidExecutionQualified: false };
  const ledger = await CompetitionLedger.open(join(root, "state.sqlite"), await pin(root, "contract.json", contract));
  try {
    const fence = lease(ledger), terminal = await pin(root, "unqualified-terminal.json", { fixtureOnly: true });
    const evidence = { schema: "gptgrep.opt-candidate-evidence.v1", candidateId: "candidate-a", parentRevision: parent, revision: candidate, worktree,
      hypothesis: { schema: "gptgrep.opt-hypothesis.v1", id: "fixture-hypothesis", kind: "invariant", capability: "hybrid_search", claim: "Preserve source identity",
        expectedBehavior: "References retain source identity", falsification: "Identity changes", mutationPaths: ["prompts/reader.md"], feedbackIds: [] },
      build: await pin(root, "build.json", { fixtureBinary: true }),
      checks: await pin(root, "checks.json", { schema: "gptgrep.opt-candidate-checks.v1", candidateRevision: candidate, commands: [{ argv: ["fixture-check"], exitCode: 0, terminal }] }),
      engineer: await pin(root, "engineer.json", { schema: "gptgrep.opt-engineer-evidence.v1", candidateRevision: candidate, actorId: "fixture-engineer", model: "fixture", effort: "fixture", terminal }),
      review: await pin(root, "review.json", { schema: "gptgrep.opt-candidate-review.v1", status: "accepted", candidateRevision: candidate, diffSha256: digest(rawDiff), reviewerId: "fixture-reviewer", role: "independent_agy", substantiveChange: true, terminal }),
      storage: await pin(root, "storage.json", { schema: "gptgrep.opt-candidate-storage.v1", status: "admitted", worktree: resolve(worktree), ownerReceipt: terminal }) };
    const evidencePin = await pin(root, "candidate.json", evidence);
    await ledger.register(ledger.snapshot().version, "register-a", fence, "candidate-a", evidencePin);
    await ledger.attachResults(ledger.snapshot().version, "results-a", fence, "candidate-a", await pin(root, "parent-run.json", baseRun), await pin(root, "candidate-run.json", candidateRun));
    const decision = await ledger.evaluate("candidate-a");
    expect(decision.comparison.eligibleNextParent).toBe(true); expect(decision.outcome).toBe("blocked"); expect(decision.nativeReceiptQualification).toBe("unavailable");
    await ledger.stageDecision(ledger.snapshot().version, "stage-a", fence, "candidate-a");
    await expect(ledger.qualify(ledger.snapshot().version, "qualify-v1", fence, "candidate-a", evidencePin)).rejects.toThrow("Legacy v1");
    await expect(ledger.commitDecision(ledger.snapshot().version, "commit-a", fence, "candidate-a", digest(decision))).rejects.toThrow();
    expect(ledger.snapshot().committed).toEqual({});
    await writeFile(join(worktree, "prompts/reader.md"), "Uncommitted mutation\n");
    await expect(ledger.evaluate("candidate-a")).rejects.toThrow("clean committed");
  } finally { ledger.close(); }
});
