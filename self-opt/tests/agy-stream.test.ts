/** Offline fake-native transport fixtures. They grant no native, account, confinement or release acceptance. */
import { expect, test } from "bun:test";
import { execFile } from "node:child_process";
import { chmod, lstat, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { canonical, digest, fileHash, sha256 } from "../../benchmark/src/io";
import { executeOwnerCommand } from "../../benchmark/src/process";
import { agyCommand, agyRequest, agyStreamTaskPacket, consumeAgyExecution, type PreparedAgy } from "../src/native-agy";
import { isAgyStreamRoute, type ActorPolicy, type AgyStreamRoute } from "../src/actor-contracts";
import { AGY_STREAM_PRODUCER, AGY_STREAM_SOURCES, agyDenyDefinition, runAgyStream, shellQuote } from "../src/agy-stream";
import { DENY_HELPER, DENY_ID, DENY_REASON, validateAgyStreamPacket, type AgyStreamJob, type AgyStreamReceipt } from "../src/agy-stream-contracts";
import type { Pin } from "../src/competition";

const external = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
if (!external || !process.env.HOME) throw new Error("Checked external output and original fixture HOME required");
const exec = promisify(execFile), H = "a".repeat(64), conversation = "12345678-1234-4234-8234-123456789abc";
async function pin(path: string): Promise<Pin> { return { path, sha256: await fileHash(path) }; }
async function save(path: string, value: unknown): Promise<Pin> { await writeFile(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: "wx" }); return pin(path); }
const fakeNative = String.raw`
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
const [mode, capture, expected, ...args] = process.argv.slice(2);
const get = key => args[args.indexOf(key) + 1];
const id = "12345678-1234-4234-8234-123456789abc";
const emit = event => process.stdout.write(JSON.stringify(event) + "\n");
const init = { event: "init", conversation_id: id, init: { model: mode === "wrong-init" ? "another-model" : get("--model"), cwd: process.cwd(), permission_mode: "request-review", tools: [] } };
emit(init);
if (mode === "duplicate-init") emit(init);
let raw = "";
for await (const part of process.stdin) raw += part.toString("utf8");
const lines = raw.trimEnd().split("\n");
if (lines.length !== 1) process.exit(91);
const frame = JSON.parse(lines[0]);
if (frame.event !== "user" || frame.message.content !== await readFile(expected, "utf8")) process.exit(92);
await writeFile(capture, JSON.stringify({ frame, inputBytes: Buffer.byteLength(raw), argv: args, pid: process.pid }) + "\n", { flag: "wx" });
if (mode === "hold") { process.on("SIGTERM", () => {}); setInterval(() => {}, 1000); await new Promise(() => {}); }
emit({ event: "step_update", step_update: { conversation_id: id, step_index: 0, step_type: "user_input", state: "DONE" } });
if (mode === "deny") {
  const hook = JSON.parse(await readFile(join(process.cwd(), ".agents", "hooks.json"), "utf8"));
  const command = hook["gptgrep-deny-all-v1"].PreToolUse[0].hooks[0].command;
  const proc = Bun.spawn(["/bin/sh", "-c", command], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  proc.stdin.end("{}\n");
  const denied = JSON.parse(await new Response(proc.stdout).text());
  if (await proc.exited !== 0 || denied.decision !== "deny") process.exit(93);
  emit({ event: "step_update", step_update: { conversation_id: id, step_index: 1, step_type: "tool", state: "ERROR", tool_name: "read_file", tool_info: { error: { message: denied.reason } } } });
}
const result = { conversation_id: id, status: mode === "error" ? "ERROR" : "SUCCESS", num_turns: 1,
  usage: { input_tokens: 17, output_tokens: 8, total_tokens: 25 },
  structured_output: { status: "completed", summary: "The complete offline packet reached its declared tail", findings: [], evidence: [], limitations: ["Offline fake native only"] } };
if (mode === "unproven-tool") {
  const tool = { event: "step_update", step_update: { conversation_id: id, step_index: 1, step_type: "tool", state: "ERROR", tool_name: "read_file", tool_info: { error: { message: "Fixture failure without the selected deny reason or marker" } } } };
  process.stdout.write(JSON.stringify(tool) + "\n" + JSON.stringify({ event: "result", result }) + "\n");
} else emit({ event: "result", result });
if (mode === "duplicate-result") emit({ event: "result", result });
`;
async function fixture(mode: string) {
  const root = join(external!, `agy-stream-${mode}-${crypto.randomUUID()}`); await mkdir(root);
  const repository = join(root, "candidate"); await mkdir(repository); await mkdir(join(repository, "runtime"));
  const git = async (...args: string[]) => (await exec("git", ["-c", "user.name=Offline Fixture", "-c", "user.email=fixture@example.invalid", "-C", repository, ...args], { timeout: 10000, maxBuffer: 1024 * 1024 })).stdout;
  await git("init", "--quiet");
  const before = `original\n${"source body before\n".repeat(1200)}END-BEFORE\n`, after = `revised\n${"source body after\n".repeat(1200)}END-AFTER-完整尾部\n`;
  await writeFile(join(repository, "runtime", "guide.md"), before); await git("add", "runtime/guide.md"); await git("commit", "--quiet", "-m", "offline parent");
  const parentRevision = (await git("rev-parse", "HEAD")).trim();
  await writeFile(join(repository, "runtime", "guide.md"), after); await git("add", "runtime/guide.md"); await git("commit", "--quiet", "-m", "offline candidate");
  const candidateRevision = (await git("rev-parse", "HEAD")).trim(), info = await lstat(repository);
  const binaryPath = join(root, "fake-native.sh"), fixturePath = join(root, "fake-native.ts"), capture = join(root, "consumed.json"), configPath = join(root, "empty-bunfig.toml"), schemaPath = join(root, "result-schema.json");
  await writeFile(fixturePath, fakeNative, { mode: 0o600 }); await writeFile(configPath, "", { mode: 0o600 });
  const bun = await pin(await realpath(process.execPath)), producer = await pin(AGY_STREAM_PRODUCER), resultSchema = await save(schemaPath, { type: "object" });
  const taskPath = join(root, "packet.json");
  await writeFile(binaryPath, `#!/bin/sh\nexec ${shellQuote(bun.path)} --no-env-file --no-install --config=${shellQuote(configPath)} ${shellQuote(fixturePath)} ${shellQuote(mode)} ${shellQuote(capture)} ${shellQuote(taskPath)} "$@"\n`, { mode: 0o700 });
  await chmod(binaryPath, 0o700);
  const instructions = await save(join(root, "instructions.json"), { instructions: "Offline fixture instruction binding" }), rootInstructions = await save(join(root, "root-instructions.json"), { rule: "Inspect only parent-supplied candidate bytes" });
  const reviewer: AgyStreamRoute = { schema: "gptgrep.opt-agy-stream-route.v1", binary: await pin(binaryPath), bun, bunConfig: await pin(configPath), producer,
    producerSources: await Promise.all(AGY_STREAM_SOURCES.map(pin)), resultSchema, model: "offline-selected-model", effort: "medium", permissionPolicy: "preserve", authorization: "Offline fixture only", instructions,
    rootInstructions: [rootInstructions], outputRoot: join(root, "reviews"), nativeHome: process.env.HOME!, timeoutSeconds: 2, outerTimeoutMs: 8000,
    maxOutputBytes: 65536, maxSources: 4, maxPacketBytes: 1024 * 1024, maxInputBytes: 2 * 1024 * 1024, maxTraceBytes: 256 * 1024 };
  await mkdir(reviewer.outputRoot);
  const reviewRoot = join(reviewer.outputRoot, "offline-candidate-a-audit"), cwd = join(reviewRoot, "workspace");
  await mkdir(join(cwd, ".agents"), { recursive: true }); await mkdir(join(reviewRoot, "receipts"));
  const policy = { competitionId: "offline", parentRevision, outputRoot: root, reviewer,
    engineers: [{ candidateId: "candidate-a", worktree: { path: repository, device: info.dev, inode: info.ino, gitDirectory: (await git("rev-parse", "--absolute-git-dir")).trim() } }], builds: [{ candidateId: "candidate-a", targetArch: "x64" }] } as ActorPolicy & { reviewer: AgyStreamRoute };
  const build: any = { candidateId: "candidate-a", candidateRevision, binary: { path: binaryPath, sha256: H }, checks: [{ argv: ["offline-check"], exitCode: 0 }] };
  const engineer: any = { diffSha256: digest(await git("diff", "--binary", "--no-ext-diff", "--no-renames", parentRevision, candidateRevision, "--")), changedPaths: ["runtime/guide.md"], hypothesis: { claim: "Offline generic source change" } };
  const packet = await agyStreamTaskPacket(policy, build, engineer, await readFile(instructions.path, "utf8")), task = await save(taskPath, packet);
  const spoolPath = join(reviewRoot, "receipts", "hook-invocations.txt"); await writeFile(spoolPath, "", { mode: 0o600 });
  const helperPath = join(cwd, ".agents", "deny-all.sh"), hookPath = join(cwd, ".agents", "hooks.json");
  await writeFile(helperPath, DENY_HELPER, { mode: 0o444 });
  const hook = await save(hookPath, agyDenyDefinition(helperPath, spoolPath));
  const deadlineAt = Date.now() + 30000, job: AgyStreamJob = { schema: "gptgrep.opt-agy-stream-job.v1", requestId: "offline-candidate-a-audit", candidateId: "candidate-a", parentRevision, candidateRevision,
    diffSha256: engineer.diffSha256, packet: task, binary: reviewer.binary, bun, producer, producerSources: reviewer.producerSources, resultSchema, cwd, nativeHome: reviewer.nativeHome, outputRoot: join(reviewRoot, "native"), model: reviewer.model, effort: reviewer.effort,
    deadlineAt, timeoutMs: 2000, maxPacketBytes: reviewer.maxPacketBytes, maxInputBytes: reviewer.maxInputBytes, maxTraceBytes: reviewer.maxTraceBytes, maxResultBytes: reviewer.maxOutputBytes, maxSources: reviewer.maxSources, denyPolicy: { hook, helper: await pin(helperPath), spoolPath } };
  const request = await save(join(root, "job.json"), job);
  const prepared: PreparedAgy = { schema: "gptgrep.opt-prepared-agy.v2", candidateId: "candidate-a", build: { path: join(root, "unused-build.json"), sha256: H }, request, task, expectedReceiptPath: join(job.outputRoot, "receipt.json"), command: agyCommand(policy, request, task) };
  return { root, packet, before, after, capture, policy, build, engineer, job, request, prepared, git };
}
async function value(pin: Pin): Promise<AgyStreamReceipt> { expect(await fileHash(pin.path)).toBe(pin.sha256); return JSON.parse(await readFile(pin.path, "utf8")); }
async function waitForFile(path: string) {
  for (let tries = 0; tries < 100; tries++) { try { await lstat(path); return; } catch { await Bun.sleep(10); } }
  throw new Error("Offline native did not acknowledge its consumed frame");
}

test("actual project CLI sends the full source packet once on stdin and shared consumer verifies it", async () => {
  const f = await fixture("deny"), command = f.prepared.command;
  expect(isAgyStreamRoute(f.policy.reviewer)).toBe(true);
  expect(command.executable).toEqual(f.policy.reviewer.bun);
  expect(command.argv).toEqual(["--no-env-file", "--no-install", `--config=${f.policy.reviewer.bunConfig.path}`, AGY_STREAM_PRODUCER, "--job", f.request.path, "--expect-sha256", f.request.sha256]);
  expect(Buffer.byteLength(JSON.stringify(command.argv))).toBeLessThan(4096);
  expect(Buffer.byteLength(await readFile(f.prepared.task.path, "utf8"))).toBeGreaterThan(16 * 1024);
  expect(f.packet.sources[0]).toEqual({ path: "runtime/guide.md", before: f.before, after: f.after });
  expect(sha256(DENY_HELPER)).toBe("5f89ef800073e7e2268af7fca62e97fe559ae6a1771692eb594d494cbe303e65");
  const observation = await executeOwnerCommand(command, 15000, { environment: { AGY_CLI_DISABLE_AUTO_UPDATE: "true" } });
  expect(observation.exitCode).toBe(0); expect(observation.cleanup).toBe(true);
  const receiptPin = await pin(f.prepared.expectedReceiptPath), receipt = await value(receiptPin), consumed = JSON.parse(await readFile(f.capture, "utf8"));
  expect(receipt.status).toBe("review_reported"); expect(receipt.submittedUserEvents).toBe(1); expect(receipt.userInputAcknowledgements).toBe(1);
  expect(consumed.frame.message.content).toBe(await readFile(f.prepared.task.path, "utf8"));
  expect(consumed.inputBytes).toBe(receipt.inputBytes); expect(consumed.argv).not.toContain(consumed.frame.message.content);
  expect(receipt.policy.helperMarkers).toBe(1); expect(receipt.policy.terminalCalls).toEqual([{ stepIndex: 1, stepType: "tool", state: "ERROR", name: "read_file", helperMarkers: 1, intendedDeny: true, subagentReasonExposed: null }]);
  const consumedEvidence = await consumeAgyExecution(f.policy, f.prepared, f.build, f.engineer, new Date(f.job.deadlineAt).toISOString(), observation);
  expect(consumedEvidence.error).toBeNull(); expect(consumedEvidence.unknown).toBe(false); expect(consumedEvidence.sessionId).toBe(conversation);
  expect(consumedEvidence.receipt).toEqual(receiptPin);
  expect(receipt.providerRequestCount).toBeNull(); expect(receipt.currencyCost).toBeNull(); expect(receipt.releaseAccepted).toBe(false);
}, 20000);

test("native ERROR retains consumed submission, real terminal counters and usage before rejection", async () => {
  const f = await fixture("error"), observation = await executeOwnerCommand(f.prepared.command, 15000);
  expect(observation.exitCode).toBe(1);
  const receipt = await value(await pin(f.prepared.expectedReceiptPath));
  expect(receipt.status).toBe(receipt.process.cleanup ? "failed" : "unknown"); expect(receipt.nativeNumTurns).toBe(1); expect(receipt.usage).toEqual({ input_tokens: 17, output_tokens: 8, total_tokens: 25 });
  expect(receipt.submittedUserEvents).toBe(1); expect(receipt.userInputAcknowledgements).toBe(1); expect(receipt.result).toBeNull();
  expect(JSON.parse(await readFile(receipt.native!.path, "utf8")).status).toBe("ERROR");
  const retained = await consumeAgyExecution(f.policy, f.prepared, f.build, f.engineer, new Date(f.job.deadlineAt).toISOString(), observation);
  expect(retained.error).toBe("AGY outer process failed or cleanup is unresolved"); expect(retained.unknown).toBe(!receipt.process.cleanup);
  expect(retained.native).toEqual(receipt.native); expect(retained.trace).toEqual(receipt.trace); expect(retained.sessionId).toBe(conversation);
  const absent = await consumeAgyExecution(f.policy, { ...f.prepared, expectedReceiptPath: join(f.root, "absent.json") }, f.build, f.engineer, new Date(f.job.deadlineAt).toISOString(), observation);
  expect(absent.unknown).toBe(true); expect(absent.native).toBeNull();
  await expect(runAgyStream(f.request)).rejects.toThrow();
  expect(JSON.parse(await readFile(f.capture, "utf8")).inputBytes).toBe(receipt.inputBytes);
}, 10000);

test("an unproven tool followed by one same-write native result preserves counters while rejecting review", async () => {
  const f = await fixture("unproven-tool"), observation = await executeOwnerCommand(f.prepared.command, 15000);
  expect(observation.exitCode).toBe(1);
  const receipt = await value(await pin(f.prepared.expectedReceiptPath));
  expect(receipt.status).toBe(receipt.process.cleanup ? "failed" : "unknown"); expect(receipt.error).toBe("native-tool-not-proven-denied");
  expect(receipt.submittedUserEvents).toBe(1); expect(receipt.userInputAcknowledgements).toBe(1); expect(receipt.result).toBeNull();
  expect(receipt.nativeNumTurns).toBe(1); expect(receipt.usage).toEqual({ input_tokens: 17, output_tokens: 8, total_tokens: 25 });
  expect(receipt.policy.helperMarkers).toBe(0); expect(receipt.policy.terminalCalls).toEqual([{ stepIndex: 1, stepType: "tool", state: "ERROR", name: "read_file", helperMarkers: 0, intendedDeny: false, subagentReasonExposed: null }]);
  expect(JSON.parse(await readFile(receipt.native!.path, "utf8")).status).toBe("SUCCESS");
  const trace = (await readFile(receipt.trace.path, "utf8")).trimEnd().split("\n").map(line => JSON.parse(line));
  expect(trace.at(-2)!.step_update.step_type).toBe("tool"); expect(trace.at(-1)!.result.usage).toEqual(receipt.usage);
  const retained = await consumeAgyExecution(f.policy, f.prepared, f.build, f.engineer, new Date(f.job.deadlineAt).toISOString(), observation);
  expect(retained.error).toBe("AGY outer process failed or cleanup is unresolved"); expect(retained.native).toEqual(receipt.native);
  expect(retained.sessionId).toBe(conversation); expect(retained.unknown).toBe(!receipt.process.cleanup);
}, 10000);

test("complete source projection preserves a UTF-8 BOM and rejects an invalid UTF-8 Git blob", async () => {
  const f = await fixture("text-projection"), source = join(f.policy.engineers[0]!.worktree.path, "runtime", "guide.md");
  const project = async () => {
    f.build.candidateRevision = (await f.git("rev-parse", "HEAD")).trim();
    f.engineer.diffSha256 = digest(await f.git("diff", "--binary", "--no-ext-diff", "--no-renames", f.policy.parentRevision, f.build.candidateRevision, "--"));
    return agyStreamTaskPacket(f.policy, f.build, f.engineer, await readFile(f.policy.reviewer.instructions.path, "utf8"));
  };
  const bom = Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from("Exact valid text including its BOM\n")]);
  await writeFile(source, bom); await f.git("add", "runtime/guide.md"); await f.git("commit", "--quiet", "-m", "offline BOM source");
  const packet = await project(); expect(Buffer.from(packet.sources[0]!.after!, "utf8").equals(bom)).toBe(true);
  await writeFile(source, Buffer.from([0x61, 0xff, 0x62, 0x0a])); await f.git("add", "runtime/guide.md"); await f.git("commit", "--quiet", "-m", "offline invalid UTF-8 source");
  await expect(project()).rejects.toThrow("not exact UTF-8 text");
  await expect(lstat(f.capture)).rejects.toThrow(); await expect(lstat(f.job.outputRoot)).rejects.toThrow();
}, 10000);

test("cancellation kills and reaps the owned native group while retaining one consumed attempt", async () => {
  const f = await fixture("hold"), abort = new AbortController(), pending = runAgyStream(f.request, { signal: abort.signal });
  await waitForFile(f.capture); abort.abort();
  const receipt = await value(await pending), consumed = JSON.parse(await readFile(f.capture, "utf8"));
  expect(receipt.status).toBe("failed"); expect(receipt.error).toBe("cancelled"); expect(receipt.process.cleanup).toBe(true);
  expect(receipt.lifecycle.leaderClosed).toBe(true); expect(receipt.lifecycle.groupAbsenceProven).toBe(true);
  expect(receipt.process.pid).toBe(consumed.pid); expect(receipt.submittedUserEvents).toBe(1); expect(receipt.userInputAcknowledgements).toBe(0);
  expect(() => process.kill(-consumed.pid, 0)).toThrow();
  await expect(runAgyStream(f.request)).rejects.toThrow();
}, 10000);

test("duplicate init and result never submit a second user event or qualify a review", async () => {
  for (const mode of ["duplicate-init", "duplicate-result"]) {
    const f = await fixture(mode), receipt = await value(await runAgyStream(f.request));
    expect(receipt.status).toBe(receipt.process.cleanup ? "failed" : "unknown"); expect(receipt.result).toBeNull(); expect(receipt.submittedUserEvents).toBe(1);
    expect(receipt.lifecycle.signalErrors.length).toBeLessThanOrEqual(8);
    if (receipt.process.cleanup) { expect(receipt.lifecycle.leaderClosed).toBe(true); expect(receipt.lifecycle.groupAbsenceProven).toBe(true); }
    expect(receipt.error).toBe(mode === "duplicate-init" ? "native-init-identity-mismatch" : "native-result-identity-mismatch");
  }
}, 10000);

test("wrong native identity and consistently repinned candidate substitution fail before any user submission", async () => {
  const wrong = await fixture("wrong-init"), receipt = await value(await runAgyStream(wrong.request));
  expect(receipt.status).toBe(receipt.process.cleanup ? "failed" : "unknown"); expect(receipt.error).toBe("native-init-identity-mismatch"); expect(receipt.submittedUserEvents).toBe(0);
  await expect(lstat(wrong.capture)).rejects.toThrow();
  const changed = await fixture("success"), packet = { ...changed.packet, candidateId: "candidate-b" };
  validateAgyStreamPacket(packet);
  const task = await save(join(changed.root, "different-packet.json"), packet), request = await save(join(changed.root, "different-job.json"), { ...changed.job, packet: task });
  await expect(runAgyStream(request)).rejects.toThrow("another exact candidate");
  await expect(lstat(changed.capture)).rejects.toThrow(); await expect(lstat(changed.job.outputRoot)).rejects.toThrow();
  const incomplete = structuredClone(changed.packet); incomplete.diff += "changed"; expect(() => validateAgyStreamPacket(incomplete)).toThrow();
  expect(() => agyRequest(changed.policy, {} as any)).toThrow("legacy Composer");
  expect(canonical(JSON.parse(await readFile(changed.job.denyPolicy.hook.path, "utf8")))).toBe(canonical(agyDenyDefinition(changed.job.denyPolicy.helper.path, changed.job.denyPolicy.spoolPath)));
  expect(DENY_REASON).toContain(DENY_ID);
}, 10000);
