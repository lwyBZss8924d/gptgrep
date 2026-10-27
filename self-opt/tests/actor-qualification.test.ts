/** Offline evidence-boundary tests. No accepted live evidence or model calls are emitted. */
import { expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { validateAdjudication, inspectDirectCandidate, verifyQualifiedCandidate, type AgyAdjudication } from "../src/actor-qualification";
import { pinned, validateActorDeadline, verifyActorGate } from "../src/actor-contracts";
import { validateCompetition, type CompetitionContract } from "../src/competition";
import { digest, fileHash } from "../../benchmark/src/io";

const H = "a".repeat(64), revision = "2".repeat(40), agy = { path: "/fixture/agy-result.json", sha256: H };
const expected = { candidateId: "candidate-a", revision, diffSha256: H, agy, findings: [] as any[], engineerSessionId: "native-engineer", reviewerSessionId: "native-agy" };
function adjudication(): AgyAdjudication {
  return { schema: "gptgrep.opt-agy-adjudication.v1", status: "accepted", candidateId: expected.candidateId, candidateRevision: revision,
    diffSha256: H, agy, parentSessionId: "independent-parent", substantiveChange: true, rationale: "The actual source review supports this substantive candidate", findings: [] };
}
function contract(): CompetitionContract {
  return { schema: "gptgrep.opt-competition.v3", competitionId: "fixture-direct", scope: "orchestration_qualification", parentRevision: "1".repeat(40),
    scenarioFingerprint: H, cohort: ["task-a", "task-b", "task-c", "task-d"], candidateIds: ["candidate-a", "candidate-b"], mutationPaths: ["runtime/codex/AGENTS.md"],
    comparator: { path: fileURLToPath(new URL("../src/evaluate.ts", import.meta.url)), sha256: H }, nativePolicy: { path: "/fixture/actor-policy.json", sha256: H },
    deadlineAt: 1, reservationLimits: { engineer: 2, review: 2, reader: 12, judge: 12, probe: 0, repair: 0 }, generations: 1, compositionAllowed: false, paidExecutionQualified: false };
}
test("version 3 explicitly requires a direct actor policy while preserving closed legacy contracts", () => {
  const value = contract(); expect(() => validateCompetition(value)).not.toThrow();
  const missing = { ...value }; delete missing.nativePolicy; expect(() => validateCompetition(missing)).toThrow();
  expect(() => validateCompetition({ ...value, schema: "gptgrep.opt-competition.v2" })).not.toThrow();
  const legacy: any = { ...value, schema: "gptgrep.opt-competition.v1" }; delete legacy.nativePolicy;
  expect(() => validateCompetition(legacy)).not.toThrow();
  expect(() => validateCompetition({ ...legacy, nativePolicy: value.nativePolicy })).toThrow();
});
test("native AGY transport and same-session self-approval cannot satisfy parent adjudication", () => {
  expect(() => validateAdjudication(adjudication(), expected)).not.toThrow();
  for (const update of [{ status: "rejected" }, { parentSessionId: expected.engineerSessionId }, { parentSessionId: expected.reviewerSessionId },
    { substantiveChange: false }, { candidateRevision: "3".repeat(40) }, { diffSha256: "b".repeat(64) }, { acceptance: "accepted" }]) {
    expect(() => validateAdjudication({ ...adjudication(), ...update } as any, expected)).toThrow();
  }
});
test("every actual AGY finding binds an ordered explicit disposition and evidence", () => {
  const findings = [{ claim: "A possible regression", evidence_ids: ["source-1"] }], withFinding = { ...expected, findings };
  expect(() => validateAdjudication(adjudication(), withFinding)).toThrow();
  const value = { ...adjudication(), findings: [{ findingSha256: digest(findings[0]), disposition: "false_positive" as const,
    rationale: "The pinned check demonstrates the reported path is covered", evidence: [{ path: "/fixture/check.json", sha256: H }] }] };
  expect(() => validateAdjudication(value, withFinding)).not.toThrow();
  expect(() => validateAdjudication(value, { ...withFinding, findings: [{ claim: "Different actual finding", evidence_ids: ["source-1"] }] })).toThrow();
  expect(() => validateAdjudication({ ...value, findings: [{ ...value.findings[0]!, evidence: [] }] }, withFinding)).toThrow();
});
test("legacy owner receipt policy cannot enter the direct-local qualification route", async () => {
  await expect(inspectDirectCandidate({ ...contract(), schema: "gptgrep.opt-competition.v2" }, { path: "/fixture/never-opened.json", sha256: H })).rejects.toThrow("version 3");
});
test("expired evidence remains inspectable without granting a new actor launch", async () => {
  const past = Date.now() - 1000, future = new Date(Date.now() + 60000).toISOString();
  expect(() => validateActorDeadline("retained_evidence", past, future)).not.toThrow();
  expect(() => validateActorDeadline("launch", past, future)).toThrow("deadline expired");
  expect(() => validateActorDeadline("launch", Date.now() + 60000, new Date(past).toISOString())).toThrow("deadline expired");
  expect(() => validateActorDeadline("retained_evidence", past, "unknown")).toThrow();
  const pin = { path: "/fixture/not-opened.json", sha256: H };
  await expect(verifyActorGate(pin, pin, pin, "caller_override" as any)).rejects.toThrow("purpose");
});
test("purported qualified records cannot enable an actor route without its actual owner validator", async () => {
  const external = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
  if (!external) throw new Error("Explicit external test output required");
  const root = join(external, `qualification-boundary-${crypto.randomUUID()}`); await mkdir(root);
  for (const extra of [{}, { ownerWitness: { containerId: "foreign-cid", initPid: 42, processStart: "foreign-start", job: "foreign-job" } },
    { terminal: null, finalization: null }, { isolationAccepted: true, cleanupAccepted: true }]) {
    const path = join(root, `${crypto.randomUUID()}.json`);
    await writeFile(path, JSON.stringify({ schema: "gptgrep.opt-direct-native-qualification.v1", status: "qualified", reasons: [], ...extra }));
    await expect(verifyQualifiedCandidate({} as any, contract(), "candidate-a", { path, sha256: await fileHash(path) })).rejects.toThrow("no selected owner validator");
  }
  // This refusal also precedes any attempt to open a credential-bearing filename.
  await expect(verifyQualifiedCandidate({} as any, contract(), "candidate-a", { path: join(root, "auth.json"), sha256: H })).rejects.toThrow("unavailable");
  await expect(pinned({ path: join(root, "auth.json"), sha256: H })).rejects.toThrow("Credential files");
});
