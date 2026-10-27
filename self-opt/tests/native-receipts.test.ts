/** Offline component fixtures only. No fixture is a native qualification receipt. */
import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import { validateAgyEnvelope, validateCodexCapture, validateNativePolicy,
  type HostStorageContract, type NativeCandidateSlot, type NativeReceiptPolicy, type NativeRole } from "../src/native-receipts";

const H = "a".repeat(64), PARENT = "1".repeat(40);
const nativeProducer = fileURLToPath(new URL("../../benchmark/src/native.ts", import.meta.url));
const clone = <T>(value: T): T => structuredClone(value);
const pin = (path: string) => ({ path, sha256: H });

function role(client: "codex" | "agy", cwd: string): NativeRole {
  return { client, binary: pin(`/fixture/bin/${client}`), producer: pin(client === "codex" ? nativeProducer : "/fixture/composer/composer_process.py"),
    model: client === "codex" ? "fixture-engineer-model" : "fixture-agy-model", effort: "medium",
    serviceTier: client === "codex" ? "priority" : "not_exposed_by_client", cwd, inputSha256: H };
}
function storage(runId: string, cwd: string): HostStorageContract {
  return { runId, planDigest: H, command: { argv: ["/fixture/bin/owner-command", runId], cwd } };
}
function policyFixture() {
  const expected = { competitionId: "fixture-competition", parentRevision: PARENT, scenarioFingerprint: H, candidateIds: ["candidate-a", "candidate-b"] };
  const policy: NativeReceiptPolicy = { schema: "gptgrep.opt-native-policy.v1", competitionId: expected.competitionId,
    parentRevision: PARENT, scenarioFingerprint: H, candidates: expected.candidateIds.map((id, index): NativeCandidateSlot => {
      const cwd = `/fixture/worktrees/${id}`;
      return { id, worktree: { path: cwd, device: 1, inode: 100 + index, gitDirectory: `/fixture/git/worktrees/${id}` },
        engineer: { ...role("codex", cwd), owner: { storage: storage(`${id}-engineer-storage`, cwd), authRunId: `${id}-engineer-auth`,
          authRole: "engineer", authSourceId: "fixture-codex-owner", command: { argv: ["/fixture/bin/native-engineer", id], cwd } } },
        reviewer: { ...role("agy", `/fixture/reviews/${id}`), permissionPolicy: "preserve", owner: { kind: "agents_composer_agy_print", outputRoot: "/fixture/agy-campaign",
          storage: storage(`${id}-review-storage`, "/fixture/reviews"), sources: [pin("/fixture/composer/composer_process.py"), pin("/fixture/composer/composer_agy.py")],
          resultSchema: pin("/fixture/composer/agy-result.schema.json") } },
        checks: [{ id: "required-check", argv: ["/fixture/bin/candidate-check", id],
          owner: { runId: `${id}-check-storage`, planDigest: H, command: { argv: ["/fixture/bin/candidate-check", id], cwd } } }] };
    }) };
  return { policy, expected };
}

test("native policy freezes two physical worktrees and one serialized AGY campaign", () => {
  const { policy, expected } = policyFixture();
  expect(() => validateNativePolicy(policy, expected)).not.toThrow();
  for (const mutate of [
    (p: NativeReceiptPolicy) => { p.candidates[1]!.worktree.path = p.candidates[0]!.worktree.path; },
    (p: NativeReceiptPolicy) => { p.candidates[1]!.worktree.gitDirectory = p.candidates[0]!.worktree.gitDirectory; },
    (p: NativeReceiptPolicy) => { p.candidates[1]!.worktree.inode = p.candidates[0]!.worktree.inode; },
    (p: NativeReceiptPolicy) => { p.candidates[1]!.engineer.owner.authRunId = p.candidates[0]!.engineer.owner.authRunId; },
    (p: NativeReceiptPolicy) => { p.candidates[1]!.reviewer.owner.outputRoot = "/fixture/another-agy-campaign"; },
    (p: NativeReceiptPolicy) => { p.candidates[1]!.checks[0]!.owner.runId = p.candidates[0]!.checks[0]!.owner.runId; },
  ]) {
    const changed = clone(policy); mutate(changed);
    expect(() => validateNativePolicy(changed, expected)).toThrow();
  }
});

test("native policy refuses changed source identity, hidden defaults and missing candidate routes", () => {
  const { policy, expected } = policyFixture();
  for (const mutate of [
    (p: any) => { p.parentRevision = "2".repeat(40); },
    (p: any) => { p.candidates.pop(); },
    (p: any) => { p.candidates[0].engineer.producer.path = "/fixture/replacement-native.ts"; },
    (p: any) => { delete p.candidates[0].engineer.effort; },
    (p: any) => { p.candidates[0].reviewer.serviceTier = "default"; },
    (p: any) => { p.candidates[0].engineer.cwd = "/fixture/different-worktree"; },
    (p: any) => { p.candidates[0].checks = []; },
    (p: any) => { p.candidates[0].checks[0].owner.command.argv = ["/fixture/bin/other-check"]; },
    (p: any) => { p.candidates[0].checks[0].owner.command.cwd = "/fixture/wrong-check-worktree"; },
    (p: any) => { p.candidates[0].engineer.allowFallback = true; },
  ]) {
    const changed: any = clone(policy); mutate(changed);
    expect(() => validateNativePolicy(changed, expected)).toThrow();
  }
});

function codexFixture() {
  const expected = role("codex", "/fixture/worktrees/candidate-a");
  const result: any = { schema: "gptgrep.native-run.v1", executionMode: "live", status: "completed", validStart: true,
    threadId: "fixture-native-thread", sessionId: "fixture-native-session", turnId: "fixture-native-turn",
    requestedModel: expected.model, requestedEffort: expected.effort, requestedServiceTier: expected.serviceTier,
    observedModel: expected.model, servedModel: null, runtimeBinarySha256: H, modelBoundaryValid: true,
    traceTruncated: false, cleanup: { completed: true, groupTerminated: true }, captureGaps: [], reroutes: [], turnRequests: 1 };
  const events: any[] = [
    { direction: "request", id: 1, method: "thread/start" },
    { direction: "request", id: 2, method: "turn/start", inputSha256: H },
    { direction: "response", id: 2, ok: true },
    { direction: "result", status: result.status, threadId: result.threadId, sessionId: result.sessionId, turnId: result.turnId,
      requestedModel: result.requestedModel, observedModel: result.observedModel, servedModel: result.servedModel, cleanup: clone(result.cleanup) },
  ];
  return { expected, result, events };
}

test("Codex component binds one acknowledged turn while preserving unavailable provider model echo", () => {
  const f = codexFixture();
  expect(validateCodexCapture(f.result, f.events, f.expected)).toEqual({ sessionId: "fixture-native-session", threadId: "fixture-native-thread", turnId: "fixture-native-turn" });
  expect(f.result.servedModel).toBeNull();
  expect(validateCodexCapture(f.result, f.events, f.expected)).not.toHaveProperty("qualified");
});

test("Codex test mode, incomplete capture and source/model/effort/tier substitutions cannot qualify", () => {
  const mutations: Array<(result: any) => void> = [
    r => { r.executionMode = "test"; }, r => { r.validStart = false; }, r => { r.status = "failed"; },
    r => { r.runtimeBinarySha256 = "b".repeat(64); }, r => { r.requestedModel = "other-model"; },
    r => { r.observedModel = "other-model"; }, r => { r.servedModel = "other-model"; },
    r => { r.requestedEffort = "high"; }, r => { r.requestedServiceTier = "default"; },
    r => { r.modelBoundaryValid = false; }, r => { r.traceTruncated = true; },
    r => { r.captureGaps.push("missing_child_snapshot"); }, r => { r.reroutes.push({ fromModel: "fixture-engineer-model", toModel: "other-model" }); },
    r => { r.authFailure = "AUTH_UNAUTHORIZED"; }, r => { r.cleanup.completed = false; },
    r => { r.turnRequests = 2; }, r => { r.sessionId = null; },
  ];
  for (const mutate of mutations) {
    const f = codexFixture(); mutate(f.result);
    expect(() => validateCodexCapture(f.result, f.events, f.expected)).toThrow();
  }
});

test("Codex trace rejects changed prompt, mixed identities, duplicate terminals and missing acknowledgment", () => {
  for (const mutate of [
    (f: ReturnType<typeof codexFixture>) => { f.events[1].inputSha256 = "b".repeat(64); },
    (f: ReturnType<typeof codexFixture>) => { f.events[3].sessionId = "unrelated-session"; },
    (f: ReturnType<typeof codexFixture>) => { f.events.push(clone(f.events[3])); },
    (f: ReturnType<typeof codexFixture>) => { f.events.push(clone(f.events[1])); },
    (f: ReturnType<typeof codexFixture>) => { f.events.splice(2, 1); },
    (f: ReturnType<typeof codexFixture>) => { f.events[2].ok = false; },
  ]) {
    const f = codexFixture(); mutate(f);
    expect(() => validateCodexCapture(f.result, f.events, f.expected)).toThrow();
  }
});

// Snapshot of Composer's bounded result schema; no external files are read by these fixtures.
const AGY_SCHEMA_JSON = '{"type":"object","additionalProperties":false,"required":["status","summary","findings","evidence","limitations"],"properties":{"status":{"type":"string","enum":["completed","partial","blocked"],"maxLength":16},"summary":{"type":"string","minLength":1,"maxLength":1600},"findings":{"type":"array","maxItems":16,"items":{"type":"object","additionalProperties":false,"required":["claim","evidence_ids"],"properties":{"claim":{"type":"string","minLength":1,"maxLength":1200},"evidence_ids":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":64}}}}},"evidence":{"type":"array","maxItems":16,"items":{"type":"object","additionalProperties":false,"required":["id","locator","note"],"properties":{"id":{"type":"string","minLength":1,"maxLength":64},"locator":{"type":"string","minLength":1,"maxLength":1600},"note":{"type":"string","maxLength":600}}}},"limitations":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":600}}}}';
// Computed independently with Python json.dumps(request, sort_keys=True), including Unicode escaping.
const AGY_REQUEST_SHA256 = "900b4a8b05a8f4a410d5b1419d86357f0a07eac9d5b0c6732b34cea3dcf37e19";
function agyFixture() {
  const expected: NativeRole & { permissionPolicy: "preserve" | "skip-permissions" } = { ...role("agy", "/fixture/review-a"), permissionPolicy: "preserve" };
  const request: any = { schema: "agents_composer.process_request.v1", request_id: "fixture-review-a", created_at: "2026-10-25T00:00:00Z", backend: "agy_print",
    argv: [expected.binary.path, "-p", "{prompt}", "--mode", "plan", "--model", expected.model, "--effort", "medium", "--output-format", "json",
      "--json-schema", AGY_SCHEMA_JSON, "--print-timeout", "300s", "--sandbox"],
    cwd: expected.cwd, prompt: { ref: "task.txt", sha256: H }, timeout_sec: 300, max_output_bytes: 1048576, result_format: "json",
    agy: { role: "audit", allowed_dirs: [expected.cwd], root_instructions: [{ path: "/fixture/AGENTS.md", sha256: "b".repeat(64) }],
      deadline_utc: "2026-10-25T00:05:00Z", max_in_flight: 1, max_native_subagents: 0, permission_policy: "preserve",
      authorization: "Offline fixture audit: résumé 审阅.", session: { mode: "fresh" }, max_sources: 8, source_revision: PARENT } };
  const result = { status: "completed", summary: "Offline fixture reports a bounded source audit.",
    findings: [{ claim: "Fixture source preserves the stated invariant.", evidence_ids: ["fixture-source"] }],
    evidence: [{ id: "fixture-source", locator: "prompts/reader.md:1", note: "Offline fixture locator; not a live review." }],
    limitations: ["Fixture data establishes parser mechanics only."] };
  const raw: any = { conversation_id: "fixture-agy-conversation", status: "SUCCESS", structured_output: clone(result), num_turns: 1 };
  const receipt: any = { schema: "agents_composer.process_receipt.v1", backend: "agy_print", request_id: request.request_id,
    request_sha256: AGY_REQUEST_SHA256, cwd: request.cwd, argv: clone(request.argv), agy: clone(request.agy),
    transport: "exited", returncode: 0, worker_outcome: "reported", acceptance: "not_evaluated", output_complete: true,
    result_validation: "valid", leader_reaped: true, argv_is_template: true,
    confinement: "cooperative_allowlist; native_terminal_sandbox_requested; effective_policy_unverified",
    descendant_quiescence: "not_established", session_id: raw.conversation_id, result: clone(result) };
  return { expected, request, raw, receipt };
}

test("AGY accepts the selected raw JSON envelope without inventing provider model/effort observations", () => {
  const f = agyFixture();
  expect(validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toEqual({ sessionId: "fixture-agy-conversation",
    requestedModel: "fixture-agy-model", requestedEffort: "medium", observedModel: null, observedEffort: null, serviceTier: "not_exposed_by_client" });
  expect(f.receipt.acceptance).toBe("not_evaluated"); expect(f.receipt.descendant_quiescence).toBe("not_established");
  const response = clone(f.raw); response.response = JSON.stringify(response.structured_output); delete response.structured_output;
  expect(validateAgyEnvelope(response, f.request, f.receipt, f.expected, PARENT).sessionId).toBe("fixture-agy-conversation");
});

test("AGY binds the exact independently hashed native request, session and unmodified result", () => {
  for (const mutate of [
    (f: ReturnType<typeof agyFixture>) => { f.receipt.request_sha256 = "f".repeat(64); },
    (f: ReturnType<typeof agyFixture>) => { f.request.agy.authorization = "Replacement authorization"; f.receipt.agy.authorization = "Replacement authorization"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.request_id = "another-request"; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.conversation_id = "another-native-session"; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output.summary = "Replaced result"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.result.summary = "Replaced adapter result"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.acceptance = "accepted"; },
  ]) {
    const f = agyFixture(); mutate(f);
    expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow();
  }
});

test("AGY refuses model/effort, stream, timeout and permission substitutions before receipt acceptance", () => {
  for (const [flag, replacement] of [["--model", "other-model"], ["--effort", "high"], ["--output-format", "stream-json"], ["--mode", "auto"], ["--print-timeout", "0"], ["-p", "unbound prompt"]]) {
    const f = agyFixture(); f.request.argv[f.request.argv.indexOf(flag) + 1] = replacement;
    expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow("selected native JSON adapter");
  }
  for (const prohibited of ["--dangerously-skip-permissions", "--disable-slash-commands"]) {
    const f = agyFixture(); f.request.argv.push(prohibited);
    expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow();
  }
});

test("AGY rejects unsupported fresh-session flags even with a correctly rebound request digest", () => {
  // Independent Python digests include the altered argv, so grammar failures cannot hide behind stale hashes.
  for (const [extra, digest] of [
    [["--conversation", "unexpected-session"], "1f8878f234c92b5afdc50f19a8df76e06ad6d6b951e32c75ed520aa2c6cd954b"],
    [["--unsupported"], "c34a3b1feb00a3e2a69f3f73500042ffed8ccc47e488869e16926cfe7538b1a5"],
  ] as const) {
    const f = agyFixture(); f.request.argv.push(...extra); f.receipt.argv = clone(f.request.argv); f.receipt.request_sha256 = digest;
    expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow("Unsupported or duplicate");
  }
});

test("AGY permission skipping requires the same explicit frozen choice and authorization", () => {
  const f = agyFixture();
  f.request.argv.push("--dangerously-skip-permissions");
  f.request.agy.permission_policy = "skip-permissions";
  f.request.agy.authorization = "Explicit offline fixture permission authorization.";
  f.receipt.argv = clone(f.request.argv); f.receipt.agy = clone(f.request.agy);
  f.receipt.request_sha256 = "8c15e818f21e1d8639adf7d8e5da7341449b0c0bb2ac3c6f7b46a3ad58e07b55";
  expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow("selected review route");
  f.expected.permissionPolicy = "skip-permissions";
  expect(validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT).requestedModel).toBe("fixture-agy-model");
  f.request.agy.authorization = "";
  expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow("authorization");
});

test("AGY provider failures, partial reports, malformed data and dangling findings remain rejected", () => {
  for (const mutate of [
    (f: ReturnType<typeof agyFixture>) => { f.raw.status = "ERROR"; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.error = "fixture provider failure"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.result_validation = "malformed"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.transport = "timeout"; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.output_complete = false; },
    (f: ReturnType<typeof agyFixture>) => { f.receipt.leader_reaped = false; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output.status = "partial"; f.receipt.result.status = "partial"; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output.status = "blocked"; f.receipt.result.status = "blocked"; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output.findings[0].evidence_ids = ["missing"]; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output.hidden_acceptance = true; },
    (f: ReturnType<typeof agyFixture>) => { f.raw.structured_output = "{malformed-json"; },
  ]) {
    const f = agyFixture(); mutate(f);
    expect(() => validateAgyEnvelope(f.raw, f.request, f.receipt, f.expected, PARENT)).toThrow();
  }
});
