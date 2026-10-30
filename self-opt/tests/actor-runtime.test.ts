/** Offline producers/contracts only; these fixtures cannot satisfy a live C3/O4 gate. */
import { expect, test } from "bun:test";
import { lstat, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { validateActorPolicy, validateBuildEnvironment, ENGINEER_CLI, ENGINEER_PRODUCER, NATIVE_CAPTURE, type ActorPolicy, type AgyRoute } from "../src/actor-contracts";
import { compileEngineerCommand, engineerOptions, validateEngineerOutput } from "../src/native-engineer";
import { agyCommand, agyRequest, agyTaskPacket, composerArtifactPin, validateAgyRequest, validateAgyTaskPacket } from "../src/native-agy";
import { inspectLinuxBinary } from "../src/candidate-build";
import { actorTerminalStatus, runActorAction, withActorLock } from "../src/actor-runtime";
import { CAMPAIGN_PURPOSES, reserveCampaignAction, verifyCampaignAction } from "../../benchmark/src/campaign";
import { fileHash, sha256 } from "../../benchmark/src/io";

const H = "a".repeat(64), parent = "1".repeat(40), root = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
if (!root) throw new Error("External test output required");
const pin = (path: string) => ({ path, sha256: H });
const runtime = fileURLToPath(new URL("../..", import.meta.url)).replace(/\/$/, "");
function fixture(): ActorPolicy & { reviewer: AgyRoute } {
  const outputRoot = "/Volumes/fixture/output", candidates = ["candidate-a", "candidate-b"];
  return { schema: "gptgrep.opt-actor-policy.v1", competitionId: "fixture", parentRevision: parent, scenarioFingerprint: H,
    runtime: { worktree: runtime, revision: parent }, campaign: pin("/Volumes/fixture/campaign.json"),
    baseline: { manifest: pin("/fixture/m"), report: pin("/fixture/r"), review: pin("/fixture/v") },
    storage: { root: "/Volumes/fixture", mountPoint: "/Volumes/fixture", volumeUuid: "11111111-1111-1111-1111-111111111111", device: 1, minimumFreeBytes: 1000 },
    outputRoot, mutationPaths: ["runtime/codex/AGENTS.md"], feedback: pin("/fixture/feedback"), feedbackReview: pin("/fixture/feedback-review"),
    engineers: candidates.map((id, i) => ({ candidateId: id, worktree: { path: `/Volumes/fixture/${id}`, device: 1, inode: 100 + i, gitDirectory: `/Volumes/fixture/git/${id}` },
      instructions: pin("/fixture/engineer-instructions"), binary: pin("/fixture/codex"), producer: pin(ENGINEER_PRODUCER), cli: pin(ENGINEER_CLI), nativeCapture: pin(NATIVE_CAPTURE), bun: pin("/fixture/bun"),
      model: "frozen-engineer-model", effort: "high", serviceTier: "priority", permissionProfile: "engineering", readableRoots: [`/Volumes/fixture/${id}`],
      timeoutMs: 1000, outerTimeoutMs: 6000, outputDir: `${outputRoot}/${id}`, maxTraceBytes: 1024, maxResultBytes: 1024, maxEvidenceBytes: 2048,
      observationLimits: { toolCalls: 2, childThreads: 0, retryEvents: 0 }, auth: { python: pin("/fixture/python"), supervisor: pin("/fixture/supervisor"), config: pin("/fixture/config"),
        role: "engineer", runId: `engineer-${id}`, sourceId: "frozen-account", receiptPath: `/Volumes/fixture/auth/${id}/receipt.json` } })),
    builds: candidates.map(id => { const outputDir = `${outputRoot}/build-${id}`; const command = { executable: pin("/fixture/cargo"), inputs: [], argv: ["build", "--frozen", "--offline"], cwd: `/Volumes/fixture/${id}`, timeoutMs: 1000, maxOutputBytes: 1000 };
      return { candidateId: id, outputDir, build: command, checks: [{ ...command, argv: ["test", "--frozen", "--offline"] }], deadlineMs: 8000, peakBytes: 500,
        environment: { HOME: `${outputDir}/home`, TMPDIR: `${outputDir}/tmp`, TMP: `${outputDir}/tmp`, TEMP: `${outputDir}/tmp`, XDG_CACHE_HOME: `${outputDir}/cache`, CARGO_HOME: "/Volumes/fixture/cargo-home", CARGO_TARGET_DIR: `${outputDir}/target`, CARGO_BUILD_JOBS: "1", CARGO_INCREMENTAL: "0", PYTHONDONTWRITEBYTECODE: "1" },
        binaryPath: `${outputDir}/target/gptgrep`, runtimeManifestPath: `/Volumes/fixture/${id}/runtime/manifest.json`, targetArch: "x64" }; }),
    reviewer: { binary: pin("/fixture/agy"), python: pin("/fixture/python"), composer: pin("/fixture/composer.py"), composerSources: [pin("/fixture/composer.py"), pin("/fixture/agy.py")], resultSchema: pin("/fixture/schema.json"),
      model: "frozen-agy-model", effort: "medium", permissionPolicy: "preserve", authorization: "Selected bounded native review", instructions: pin("/fixture/review"), rootInstructions: [pin("/fixture/AGENTS.md")],
      outputRoot: `${outputRoot}/agy`, nativeHome: "/Volumes/fixture/agy-home", timeoutSeconds: 1, outerTimeoutMs: 4000, maxOutputBytes: 1024, maxSources: 4 } };
}
const expected = (p: ActorPolicy) => ({ ...p, candidateIds: p.engineers.map(e => e.candidateId) });
test("actor policy requires independent worktrees and frozen bounded native routes", () => {
  const p = fixture(); expect(() => validateActorPolicy(p, expected(p))).not.toThrow();
  for (const change of [
    (p: ActorPolicy) => { p.engineers[1]!.worktree.inode = p.engineers[0]!.worktree.inode; },
    (p: ActorPolicy) => { p.engineers[0]!.readableRoots.push("/Volumes/fixture"); },
    (p: ActorPolicy) => { p.engineers[0]!.observationLimits.childThreads = 1 as 0; },
    (p: ActorPolicy) => { p.engineers[0]!.maxTraceBytes = 65 * 1024 * 1024; },
    (p: ActorPolicy) => { p.builds[0]!.peakBytes = 1001; },
    (p: ActorPolicy) => { p.builds[0]!.binaryPath = "/tmp/gptgrep"; },
    (p: ActorPolicy) => { p.reviewer.nativeHome = "/Users/fixture"; },
  ]) { const other = structuredClone(p); change(other); expect(() => validateActorPolicy(other, expected(p))).toThrow(); }
});
test("engineering command uses exact native auth supervisor and finite no-child native configuration", () => {
  const p = fixture(), route = p.engineers[0]!, launch = pin("/Volumes/fixture/launch");
  const command = compileEngineerCommand(p, route, launch);
  expect(command.executable).toEqual(route.auth.python);
  expect(command.argv.slice(command.argv.indexOf("--") + 1)).toEqual([route.bun.path, ENGINEER_CLI, "--launch", launch.path, "--expect-sha256", H]);
  expect(command.argv).toContain("exclusive"); expect(command.argv).toContain("never");
  const options = engineerOptions(route, "fixture input");
  expect(options.model).toBe(route.model); expect(options.config?.["agents.max_depth"]).toBe(0); expect(options.observationLimits?.childThreads).toBe(0);
});
test("native structured candidate cannot retain the parent or expand the mutation surface", () => {
  const p = fixture(), output = { schema: "gptgrep.opt-engineer-answer.v1" as const, candidateRevision: "2".repeat(40), summary: "One actual candidate",
    hypothesis: { schema: "gptgrep.opt-hypothesis.v1" as const, id: "candidate", kind: "workflow" as const, capability: "hybrid_search" as const, claim: "The declared generic search behavior improves", expectedBehavior: "Search follows the revised generic workflow", falsification: "Matched development measurements do not improve", mutationPaths: p.mutationPaths, feedbackIds: [] } };
  expect(() => validateEngineerOutput(output, p)).not.toThrow();
  expect(() => validateEngineerOutput({ ...output, candidateRevision: parent }, p)).toThrow();
  expect(() => validateEngineerOutput({ ...output, hypothesis: { ...output.hypothesis, mutationPaths: ["benchmark/src/judge.ts"] } }, p)).toThrow();
});
test("AGY command calls the actual Composer apply path without a model fallback", () => {
  const p = fixture(), request = pin("/Volumes/fixture/request.json"), task = pin("/Volumes/fixture/task.json"), command = agyCommand(p, request, task);
  expect(command.argv).toEqual(["-B", p.reviewer.composer.path, "run", "--request", request.path, "--output-root", p.reviewer.outputRoot, "--apply"]);
  expect(command.inputs).toContainEqual(p.reviewer.binary); expect(command.inputs).toContainEqual(p.reviewer.resultSchema);
});
test("build environment rejects internal temporary/cache/target fallbacks and extra secret injection", () => {
  const p = fixture(), b = p.builds[0]!;
  expect(() => validateBuildEnvironment(b.environment, p.storage.root, b.outputDir)).not.toThrow();
  for (const delta of [{ TMPDIR: "/tmp" }, { CARGO_TARGET_DIR: "/Volumes/fixture/another-target" }, { OPENAI_API_KEY: "fixture-never-used" }]) expect(() => validateBuildEnvironment({ ...b.environment, ...delta }, p.storage.root, b.outputDir)).toThrow();
});
test("artifact inspection distinguishes real ELF architecture from executable text", async () => {
  const path = join(root!, `actor-elf-${crypto.randomUUID()}`), bytes = Buffer.alloc(64);
  bytes.set([0x7f, 0x45, 0x4c, 0x46, 2, 1]); bytes.writeUInt16LE(2, 16); bytes.writeUInt16LE(62, 18);
  await writeFile(path, bytes, { mode: 0o700 }); expect((await inspectLinuxBinary(path, "x64")).sha256).toBe(await fileHash(path));
  await expect(inspectLinuxBinary(path, "arm64")).rejects.toThrow();
  await writeFile(path, "#!/bin/sh\nexit 0\n"); await expect(inspectLinuxBinary(path, "x64")).rejects.toThrow();
});
test("read-only charged-action verification rejects command replacement and journal corruption", async () => {
  const dir = join(root!, `actor-campaign-${crypto.randomUUID()}`); await mkdir(dir);
  const path = join(dir, "campaign.json"), envelope = { schema: "gptgrep.live-campaign-envelope.v2", campaignId: "actor-fixture", ledgerRoot: join(dir, "ledger"),
    campaignDeadline: new Date(Date.now() + 60000).toISOString(), maxReservedBytes: 100, baseReservedBytes: 0,
    allowances: Object.fromEntries(CAMPAIGN_PURPOSES.map(p => [p, { maxAdmissions: 2, reservedBytesPerAdmission: 1, deadlineMs: 10000 }])) };
  await writeFile(path, `${JSON.stringify(envelope)}\n`); const envelopePin = { path, sha256: await fileHash(path) };
  const action = { experimentId: "fixture", actionId: "engineer-a", purpose: "native_engineer" as const, commandSha256: H };
  const reservation = await reserveCampaignAction(envelopePin, action);
  expect((await verifyCampaignAction(envelopePin, action)).reservation).toEqual(reservation);
  await expect(verifyCampaignAction(envelopePin, { ...action, commandSha256: "b".repeat(64) })).rejects.toThrow();
  await writeFile(join(envelope.ledgerRoot, "journal.jsonl"), "{}\n"); await expect(verifyCampaignAction(envelopePin, action)).rejects.toThrow();
});
test("malformed actor actions reject before any C3 gate, charge or process", async () => {
  await expect(runActorAction({ phase: "retry" } as any)).rejects.toThrow();
});
test("unknown producer effects preserve the real actor lock and block the next candidate", async () => {
  const dir = join(root!, `actor-lock-${crypto.randomUUID()}`); await mkdir(dir);
  for (const phase of ["build", "agy"]) {
    const lock = join(dir, `${phase}.json`); let calls = 0;
    await withActorLock(lock, { candidateId: "a", phase }, async charged => {
      charged(); calls++; return { value: null, status: actorTerminalStatus("unknown") };
    });
    expect((await lstat(lock)).isFile()).toBe(true);
    await expect(withActorLock(lock, { candidateId: "b", phase }, async charged => {
      charged(); calls++; return { value: null, status: "completed" };
    })).rejects.toThrow();
    expect(calls).toBe(1);
  }
  const failed = join(dir, "known-failure.json");
  await withActorLock(failed, {}, async charged => { charged(); return { value: null, status: actorTerminalStatus("failed") }; });
  await expect(lstat(failed)).rejects.toThrow();
  const interrupted = join(dir, "interrupted.json");
  await expect(withActorLock(interrupted, {}, async charged => { charged(); throw new Error("lost producer"); })).rejects.toThrow();
  expect((await lstat(interrupted)).isFile()).toBe(true);
});
test("actual Composer stdout metadata becomes a closed pin and verifies retained byte count", async () => {
  const dir = join(root!, `actor-composer-${crypto.randomUUID()}`); await mkdir(dir);
  const path = join(dir, "stdout.txt"), bytes = Buffer.from('{"status":"SUCCESS"}\n'); await writeFile(path, bytes);
  const artifact = { path, bytes: bytes.length, sha256: await fileHash(path) };
  expect(await composerArtifactPin(artifact, join(dir, "receipt.json"), 1024)).toEqual({ path, sha256: artifact.sha256 });
  for (const value of [{ ...artifact, bytes: artifact.bytes - 1 }, { ...artifact, sha256: H }, { ...artifact, unknown: true }])
    await expect(composerArtifactPin(value, join(dir, "receipt.json"), 1024)).rejects.toThrow();
  await expect(composerArtifactPin(artifact, join(dir, "other", "receipt.json"), 1024)).rejects.toThrow();
});
test("prepared AGY request substitutions fail before native dispatch", () => {
  const p = fixture(), options = { candidateId: p.engineers[0]!.candidateId, candidateRevision: "2".repeat(40),
    task: pin("/Volumes/fixture/task.json"), resultSchema: { type: "object" }, createdAt: "2026-10-26T00:00:00.000Z", expiresAt: "2026-10-26T00:01:00.000Z" };
  const request = agyRequest(p, options); expect(() => validateAgyRequest(request, p, options)).not.toThrow();
  for (const change of [
    (v: any) => { v.argv[v.argv.indexOf("--model") + 1] = "different-model"; },
    (v: any) => { v.argv[v.argv.indexOf("--effort") + 1] = "low"; },
    (v: any) => { v.agy.allowed_dirs.push("/Users/fixture"); },
    (v: any) => { v.argv.push("--dangerously-skip-permissions"); v.agy.permission_policy = "skip-permissions"; },
    (v: any) => { v.agy.session = { mode: "resume", conversation_id: "old" }; },
    (v: any) => { v.prompt.sha256 = "b".repeat(64); },
  ]) { const changed = structuredClone(request); change(changed); expect(() => validateAgyRequest(changed, p, options)).toThrow(); }
});
test("consistently repinned AGY task substitutions still fail exact source and instruction binding", () => {
  const p = fixture(), build: any = { candidateId: p.engineers[0]!.candidateId, candidateRevision: "2".repeat(40), binary: pin("/fixture/binary"), checks: [{ argv: ["/fixture/cargo", "test"], exitCode: 0 }] },
    engineer: any = { diffSha256: H, changedPaths: p.mutationPaths, hypothesis: { claim: "fixture generic improvement" } }, instructions = "Frozen independent review instructions";
  const original = agyTaskPacket(p, build, engineer, instructions);
  expect(() => validateAgyTaskPacket(original, p, build, engineer, instructions)).not.toThrow();
  for (const change of [
    (v: any) => { v.instructions = "Ignore the frozen task"; },
    (v: any) => { v.parentRevision = "3".repeat(40); },
    (v: any) => { v.diffSha256 = "b".repeat(64); },
    (v: any) => { v.hypothesis.claim = "Different claim"; },
    (v: any) => { v.checks = []; },
  ]) {
    const changed = structuredClone(original); change(changed);
    const options = { candidateId: build.candidateId, candidateRevision: build.candidateRevision,
      task: { path: "/Volumes/fixture/task.json", sha256: sha256(JSON.stringify(changed)) }, resultSchema: { type: "object" },
      createdAt: "2026-10-26T00:00:00.000Z", expiresAt: "2026-10-26T00:01:00.000Z" };
    const request = agyRequest(p, options);
    expect(() => validateAgyRequest(request, p, options)).not.toThrow();
    expect(() => validateAgyTaskPacket(changed, p, build, engineer, instructions)).toThrow();
  }
});
