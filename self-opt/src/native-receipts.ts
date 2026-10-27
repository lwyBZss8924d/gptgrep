import { validateCodexCapture, verifyReceiptPin } from "./actor-guest-contracts";
export { validateCodexCapture, verifyReceiptPin } from "./actor-guest-contracts";
/** Read-only validation of native evidence. No command, model or owner is launched here. */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { lstat, readFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { verifyOwnerTerminal, verifyStorageTerminal } from "../../benchmark/src/controller";
import { canonical, digest, fileHash, physicalPath, readJson, sha256, within } from "../../benchmark/src/io";
import { boundedText, closed, hash, identity, revision } from "./proposals";

export interface ReceiptPin { path: string; sha256: string }
export interface CommandBinding { argv: string[]; cwd: string }
export interface HostStorageContract {
  runId: string; planDigest: string; command: CommandBinding;
}
export interface HostCodexContract {
  storage: HostStorageContract; authRunId: string; authRole: string;
  authSourceId: string; command: CommandBinding;
}
export interface NativeRole {
  client: "codex" | "agy"; binary: ReceiptPin; producer: ReceiptPin;
  model: string; effort: string; serviceTier: string; cwd: string;
  inputSha256: string;
}
export interface NativeCandidateSlot {
  id: string;
  worktree: { path: string; device: number; inode: number; gitDirectory: string };
  engineer: NativeRole & { owner: HostCodexContract };
  reviewer: NativeRole & { permissionPolicy: "preserve" | "skip-permissions"; owner: { kind: "agents_composer_agy_print"; outputRoot: string; storage: HostStorageContract; sources: ReceiptPin[]; resultSchema: ReceiptPin } };
  checks: Array<{ id: string; argv: string[]; owner: HostStorageContract }>;
}
export interface NativeReceiptPolicy {
  schema: "gptgrep.opt-native-policy.v1";
  competitionId: string; parentRevision: string; scenarioFingerprint: string;
  candidates: NativeCandidateSlot[];
}
export interface NativeCandidateEvidence {
  schema: "gptgrep.opt-native-evidence.v1";
  candidateId: string; candidateEvidence: ReceiptPin; policy: ReceiptPin;
  engineer: { result: ReceiptPin; trace: ReceiptPin; storage: ReceiptPin; auth: ReceiptPin };
  review: { request: ReceiptPin; receipt: ReceiptPin; native: ReceiptPin; task: ReceiptPin; preparedPrompt: ReceiptPin; storage: ReceiptPin };
  checks: Array<{ id: string; storage: ReceiptPin }>;
}
export interface NativeQualification {
  schema: "gptgrep.opt-native-qualification.v1";
  status: "qualified" | "unavailable";
  reasons: string[];
  policy: ReceiptPin; evidence: ReceiptPin; candidateEvidence: ReceiptPin;
  candidateId: string; candidateRevision: string; diffSha256: string;
  engineerSessionId: string; reviewerSessionId: string;
  engineerAuthRunId: string; worktree: NativeCandidateSlot["worktree"];
  reviewerConfiguration: { requestedModel: string; requestedEffort: string; observedModel: null; observedEffort: null; serviceTier: "not_exposed_by_client" };
  validatorSources: ReceiptPin[]; validatedPins: ReceiptPin[];
  authLinkAbsence: { path: string; absent: true };
  bindingSha256: string;
}

const exec = promisify(execFile);
const SOURCE_PATHS = [import.meta.url, new URL("./actor-guest-contracts.ts", import.meta.url).href, new URL("./mutation-paths.ts", import.meta.url).href, new URL("./proposals.ts", import.meta.url).href,
  new URL("../../benchmark/src/controller.ts", import.meta.url).href,
  new URL("../../benchmark/src/availability.ts", import.meta.url).href,
  new URL("../../benchmark/src/io.ts", import.meta.url).href,
  new URL("../../benchmark/src/native.ts", import.meta.url).href].map(url => fileURLToPath(url));
const NATIVE_PRODUCER = fileURLToPath(new URL("../../benchmark/src/native.ts", import.meta.url));

async function json<T = any>(pin: ReceiptPin): Promise<T> { await verifyReceiptPin(pin); return readJson<T>(pin.path); }
function command(value: CommandBinding): void {
  closed(value, ["argv", "cwd"], "owner command");
  if (!isAbsolute(value.cwd) || !Array.isArray(value.argv) || !value.argv.length || value.argv.length > 256) throw new Error("Exact owner argv/cwd required");
  value.argv.forEach(arg => boundedText(arg, "command argument", 8192));
  if (!isAbsolute(value.argv[0]!)) throw new Error("Owner executable must be absolute");
}
function storageContract(value: HostStorageContract): void {
  closed(value, ["runId", "planDigest", "command"], "host storage contract");
  identity(value.runId, "storage run ID"); hash(value.planDigest, "storage plan digest"); command(value.command);
}
function role(value: NativeRole, client: "codex" | "agy", extra: string[]): void {
  closed(value, ["client", "binary", "producer", "model", "effort", "serviceTier", "cwd", "inputSha256", ...extra], "native role");
  if (value.client !== client || !isAbsolute(value.cwd)) throw new Error("Native role/client/cwd differs from policy");
  for (const key of ["model", "effort", "serviceTier"] as const) boundedText(value[key], `required ${key}`, 256);
  hash(value.inputSha256, "native input digest");
  // AGY does not expose a service-tier selector. This is an explicit absence, not a default.
  if (client === "agy" && value.serviceTier !== "not_exposed_by_client") throw new Error("AGY service tier must remain explicitly unexposed");
}
export function validateNativePolicy(policy: NativeReceiptPolicy, expected: { competitionId: string; parentRevision: string; scenarioFingerprint: string; candidateIds: string[] }): void {
  closed(policy, ["schema", "competitionId", "parentRevision", "scenarioFingerprint", "candidates"], "native receipt policy");
  if (policy.schema !== "gptgrep.opt-native-policy.v1" || policy.competitionId !== expected.competitionId || policy.parentRevision !== expected.parentRevision || policy.scenarioFingerprint !== expected.scenarioFingerprint) throw new Error("Native policy does not bind the frozen competition");
  revision(policy.parentRevision); hash(policy.scenarioFingerprint, "scenario fingerprint");
  if (!Array.isArray(policy.candidates) || policy.candidates.length !== 2 || canonical(policy.candidates.map(x => x.id).sort()) !== canonical([...expected.candidateIds].sort())) throw new Error("Both independent candidate routes must be frozen");
  const paths = new Set<string>(), directories = new Set<string>(), inodes = new Set<string>(), runIds = new Set<string>();
  for (const slot of policy.candidates) {
    closed(slot, ["id", "worktree", "engineer", "reviewer", "checks"], "native candidate slot"); identity(slot.id, "candidate ID");
    closed(slot.worktree, ["path", "device", "inode", "gitDirectory"], "physical worktree");
    const w = slot.worktree;
    if (!isAbsolute(w.path) || !isAbsolute(w.gitDirectory) || !Number.isSafeInteger(w.device) || !Number.isSafeInteger(w.inode) || w.device < 0 || w.inode < 1 || paths.has(w.path) || directories.has(w.gitDirectory) || inodes.has(`${w.device}:${w.inode}`)) throw new Error("Candidate worktrees must have independent physical/Git identities");
    paths.add(w.path); directories.add(w.gitDirectory); inodes.add(`${w.device}:${w.inode}`);
    role(slot.engineer, "codex", ["owner"]); role(slot.reviewer, "agy", ["owner", "permissionPolicy"]);
    if (!["preserve", "skip-permissions"].includes(slot.reviewer.permissionPolicy)) throw new Error("AGY permission choice must be explicitly frozen");
    if (slot.engineer.cwd !== w.path || slot.engineer.producer.path !== NATIVE_PRODUCER) throw new Error("Engineer must use the actual native module and selected worktree");
    closed(slot.engineer.owner, ["storage", "authRunId", "authRole", "authSourceId", "command"], "Codex owner contract");
    const owner = slot.engineer.owner; storageContract(owner.storage); command(owner.command);
    for (const key of ["authRunId", "authRole", "authSourceId"] as const) boundedText(owner[key], key, 256);
    if (runIds.has(owner.authRunId) || runIds.has(owner.storage.runId)) throw new Error("Native owner run identities must be independent");
    runIds.add(owner.authRunId); runIds.add(owner.storage.runId);
    closed(slot.reviewer.owner, ["kind", "outputRoot", "storage", "sources", "resultSchema"], "AGY owner route");
    if (slot.reviewer.owner.kind !== "agents_composer_agy_print" || !isAbsolute(slot.reviewer.owner.outputRoot) ||
        !Array.isArray(slot.reviewer.owner.sources) || slot.reviewer.owner.sources.length < 2 || slot.reviewer.owner.sources.length > 8) throw new Error("The selected native AGY Composer route must be pinned");
    storageContract(slot.reviewer.owner.storage);
    if (runIds.has(slot.reviewer.owner.storage.runId)) throw new Error("Review storage run cannot be reused"); runIds.add(slot.reviewer.owner.storage.runId);
    if (!Array.isArray(slot.checks) || !slot.checks.length || slot.checks.length > 32 || new Set(slot.checks.map(c => c.id)).size !== slot.checks.length) throw new Error("Exact required candidate checks must be frozen");
    for (const check of slot.checks) {
      closed(check, ["id", "argv", "owner"], "required candidate check"); identity(check.id, "check ID");
      storageContract(check.owner); command({ argv: check.argv, cwd: w.path });
      if (canonical(check.argv) !== canonical(check.owner.command.argv) || check.owner.command.cwd !== w.path) throw new Error("Required check must be the exact owner command in the candidate worktree");
      if (runIds.has(check.owner.runId)) throw new Error("Check storage run cannot be reused"); runIds.add(check.owner.runId);
    }
  }
  if (new Set(policy.candidates.map(s => s.reviewer.owner.outputRoot)).size !== 1) throw new Error("Both AGY reviews must share one serialized campaign output root");
}


function flag(argv: string[], name: string): string {
  const positions = argv.flatMap((value, index) => value === name ? [index] : []);
  if (positions.length !== 1 || positions[0] === argv.length - 1) throw new Error(`AGY requires one exact ${name}`);
  return argv[positions[0]! + 1]!;
}
function composerResult(value: any, maxSources: number): void {
  closed(value, ["status", "summary", "findings", "evidence", "limitations"], "Composer AGY result");
  if (value.status !== "completed") throw new Error("Partial or blocked AGY output cannot qualify a review");
  boundedText(value.summary, "AGY summary", 1600);
  if (![value.findings, value.evidence, value.limitations].every(x => Array.isArray(x) && x.length <= 16) || value.evidence.length > maxSources) throw new Error("AGY result exceeds the selected bound");
  const ids = new Set<string>();
  for (const item of value.evidence) {
    closed(item, ["id", "locator", "note"], "AGY source evidence"); boundedText(item.id, "AGY evidence ID", 64); boundedText(item.locator, "AGY source locator", 1600);
    if (typeof item.note !== "string" || item.note.length > 600 || ids.has(item.id)) throw new Error("AGY evidence is invalid or duplicated"); ids.add(item.id);
  }
  for (const item of value.findings) {
    closed(item, ["claim", "evidence_ids"], "AGY finding"); boundedText(item.claim, "AGY finding", 1200);
    if (!Array.isArray(item.evidence_ids) || !item.evidence_ids.length || item.evidence_ids.length > 16 || item.evidence_ids.some((id: unknown) => !ids.has(id as string))) throw new Error("AGY finding has unavailable evidence");
  }
  value.limitations.forEach((v: unknown) => boundedText(v, "AGY limitation", 600));
}
/** Composer hashes Python's sorted, ASCII, spaced JSON; requests contain integer numbers. */
function composerRequestJson(value: unknown): string {
  if (typeof value === "string") return JSON.stringify(value).replace(/[\u007f-\uffff]/g, c => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);
  if (value === null || typeof value === "boolean" || typeof value === "number" && Number.isSafeInteger(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(composerRequestJson).join(", ")}]`;
  if (value && typeof value === "object") return `{${Object.entries(value).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([k, v]) => `${composerRequestJson(k)}: ${composerRequestJson(v)}`).join(", ")}}`;
  throw new Error("Composer request must use finite integer JSON values");
}
export function validateAgyEnvelope(raw: any, request: any, receipt: any, expected: NativeRole & { permissionPolicy: "preserve" | "skip-permissions" }, candidateRevision: string) {
  closed(request, ["schema", "request_id", "created_at", "backend", "argv", "cwd", "prompt", "timeout_sec", "max_output_bytes", "result_format", "agy"], "selected AGY request");
  const argv = request.argv;
  if (request.schema !== "agents_composer.process_request.v1" || request.backend !== "agy_print" || request.cwd !== expected.cwd || request.result_format !== "json" ||
      !Array.isArray(argv) || argv.some(arg => typeof arg !== "string") || argv[0] !== expected.binary.path || argv.includes("--disable-slash-commands") ||
      flag(argv, "--model") !== expected.model || flag(argv, "--effort") !== expected.effort || flag(argv, "--output-format") !== "json" || flag(argv, "--mode") !== "plan" ||
      flag(argv, "-p") !== "{prompt}" || !argv.includes("--sandbox") || flag(argv, "--print-timeout") !== `${request.timeout_sec}s`) throw new Error("AGY request differs from the selected native JSON adapter");
  const seen = new Set<string>(), added: string[] = [];
  const valued = new Set(["-p", "--mode", "--model", "--effort", "--output-format", "--json-schema", "--print-timeout", "--add-dir"]);
  for (let index = 1; index < argv.length; index++) {
    const key = argv[index]!;
    if (!["--sandbox", "--dangerously-skip-permissions"].includes(key) && !valued.has(key) || key !== "--add-dir" && seen.has(key)) throw new Error("Unsupported or duplicate AGY option");
    seen.add(key);
    if (valued.has(key)) {
      const value = argv[++index]; if (!value || value.startsWith("--")) throw new Error("AGY option has no exact value");
      if (key === "--add-dir") added.push(value);
    }
  }
  if (!seen.has("--json-schema") || !seen.has("--sandbox")) throw new Error("AGY schema and sandbox request are required");
  if (!Number.isSafeInteger(request.timeout_sec) || request.timeout_sec < 1 || request.timeout_sec > 1800 || !Number.isSafeInteger(request.max_output_bytes) || request.max_output_bytes < 1 || request.max_output_bytes > 1048576) throw new Error("AGY timeout/output limits are unavailable");
  const contract = request.agy;
  closed(contract, ["role", "allowed_dirs", "root_instructions", "deadline_utc", "max_in_flight", "max_native_subagents", "permission_policy", "authorization", "session", "max_sources", "source_revision"], "AGY task contract");
  if (contract?.role !== "audit" || contract.source_revision !== candidateRevision || contract.permission_policy !== expected.permissionPolicy ||
      seen.has("--dangerously-skip-permissions") !== (expected.permissionPolicy === "skip-permissions") || contract.max_in_flight !== 1 || contract.max_native_subagents !== 0 ||
      contract.session?.mode !== "fresh" || Object.keys(contract.session).length !== 1 || !Number.isSafeInteger(contract.max_sources) || contract.max_sources < 1 || contract.max_sources > 16 ||
      !Array.isArray(contract.allowed_dirs) || !contract.allowed_dirs.includes(expected.cwd) || contract.allowed_dirs.length > 8 || !Number.isFinite(Date.parse(contract.deadline_utc))) throw new Error("AGY task envelope exceeds the selected review route");
  if (new Set(contract.allowed_dirs).size !== contract.allowed_dirs.length || contract.allowed_dirs.some((p: unknown) => typeof p !== "string" || !isAbsolute(p as string) || p === "/") ||
      canonical(added) !== canonical(contract.allowed_dirs.filter((p: string) => p !== expected.cwd))) throw new Error("AGY directory flags differ from the native task envelope");
  boundedText(contract.authorization, "selected AGY task/permission authorization", 1000);
  if (receipt.schema !== "agents_composer.process_receipt.v1" || receipt.backend !== "agy_print" || receipt.request_id !== request.request_id ||
      receipt.request_sha256 !== sha256(composerRequestJson(request)) || receipt.cwd !== request.cwd || canonical(receipt.argv) !== canonical(argv) || canonical(receipt.agy) !== canonical(contract) ||
      receipt.transport !== "exited" || receipt.returncode !== 0 || receipt.worker_outcome !== "reported" || receipt.acceptance !== "not_evaluated" || receipt.output_complete !== true ||
      receipt.result_validation !== "valid" || receipt.leader_reaped !== true || receipt.argv_is_template !== true ||
      receipt.confinement !== "cooperative_allowlist; native_terminal_sandbox_requested; effective_policy_unverified" || receipt.descendant_quiescence !== "not_established") throw new Error("Actual AGY Composer terminal does not bind this successful attempt");
  boundedText(raw?.conversation_id, "AGY native conversation identity", 256);
  if (raw.status !== "SUCCESS" || raw.error || raw.conversation_id !== receipt.session_id) throw new Error("AGY native envelope is not a successful same-session response");
  let output = raw.structured_output ?? raw.response;
  if (typeof output === "string") output = JSON.parse(output);
  composerResult(output, contract.max_sources);
  if (canonical(output) !== canonical(receipt.result)) throw new Error("AGY native result differs from the adapter's validated result");
  return { sessionId: raw.conversation_id as string, requestedModel: expected.model, requestedEffort: expected.effort,
    observedModel: null, observedEffort: null, serviceTier: "not_exposed_by_client" as const };
}

async function stream(pin: ReceiptPin): Promise<any[]> {
  await verifyReceiptPin(pin);
  const lines = (await readFile(pin.path, "utf8")).split("\n").filter(line => line.trim());
  if (lines.length > 100_000) throw new Error("Native capture event count exceeds bound");
  return lines.map(line => JSON.parse(line));
}
async function verifyStorage(expected: HostStorageContract, pin: ReceiptPin, pins: ReceiptPin[]) {
  await verifyReceiptPin(pin);
  const verified = await verifyStorageTerminal({ storage: pin.path, storagePlanDigest: expected.planDigest });
  const receipt = await json(pin), binding = { path: receipt.binding_path, sha256: verified.bindingSha256 };
  const actual = await json(binding);
  if (actual.schema !== "environments-harness.storage-binding.v1" || actual.plan?.run_id !== expected.runId || actual.plan?.digest !== expected.planDigest ||
      actual.plan?.command?.argv_sha256 !== digest(expected.command.argv) || actual.plan?.command?.argument_count !== expected.command.argv.length ||
      actual.plan?.command?.executable !== expected.command.argv[0] || actual.plan?.command?.arguments !== "not_retained" || actual.plan?.cwd !== expected.command.cwd) throw new Error("Storage terminal does not bind the exact reviewed command/run");
  const executable = { path: expected.command.argv[0]!, sha256: actual.plan.command.executable_sha256 }; await verifyReceiptPin(executable, true);
  pins.push(pin, binding, receipt.worker.startup_receipt);
  return binding;
}

/** Evidence validation is additive to source checks; unsupported owner roles never become qualified. */
export async function validateNativeCandidate(options: {
  policyPin: ReceiptPin; evidencePin: ReceiptPin; candidateEvidencePin: ReceiptPin;
  competition: { competitionId: string; parentRevision: string; scenarioFingerprint: string; candidateIds: string[] };
  candidate: { candidateId: string; revision: string; diffSha256: string; worktree: string; engineerActorId: string; engineerModel: string; engineerEffort: string;
    engineerTerminal: ReceiptPin; reviewerActorId: string; reviewTerminal: ReceiptPin; storageReceipt: ReceiptPin; build: ReceiptPin; checks: ReceiptPin; checkArgv: string[][]; checkTerminals: ReceiptPin[] };
}): Promise<NativeQualification> {
  const policy = await json<NativeReceiptPolicy>(options.policyPin); validateNativePolicy(policy, options.competition);
  const evidence = await json<NativeCandidateEvidence>(options.evidencePin);
  closed(evidence, ["schema", "candidateId", "candidateEvidence", "policy", "engineer", "review", "checks"], "native candidate evidence");
  if (evidence.schema !== "gptgrep.opt-native-evidence.v1" || evidence.candidateId !== options.candidate.candidateId || canonical(evidence.policy) !== canonical(options.policyPin) ||
      canonical(evidence.candidateEvidence) !== canonical(options.candidateEvidencePin)) throw new Error("Native evidence does not bind the registered candidate and frozen policy");
  const slot = policy.candidates.find(s => s.id === evidence.candidateId)!;
  if (!slot || slot.worktree.path !== options.candidate.worktree) throw new Error("Native worktree differs from registered candidate");
  if (slot.engineer.model !== options.candidate.engineerModel || slot.engineer.effort !== options.candidate.engineerEffort ||
      canonical(options.candidate.engineerTerminal) !== canonical(evidence.engineer.result) || canonical(options.candidate.storageReceipt) !== canonical(evidence.engineer.storage)) throw new Error("Registered engineer/source declarations differ from native evidence");
  await physicalPath(slot.worktree.path); const info = await lstat(slot.worktree.path);
  const { stdout } = await exec("git", ["-C", slot.worktree.path, "rev-parse", "--absolute-git-dir"], { maxBuffer: 4096 });
  if (!info.isDirectory() || info.dev !== slot.worktree.device || info.ino !== slot.worktree.inode || stdout.trim() !== slot.worktree.gitDirectory) throw new Error("Native physical worktree/Git identity changed");
  const pins: ReceiptPin[] = [options.policyPin, options.evidencePin, options.candidateEvidencePin, options.candidate.build];
  const executablePaths = new Set([slot.engineer.binary.path, slot.reviewer.binary.path]);
  for (const role of [slot.engineer, slot.reviewer]) { await verifyReceiptPin(role.binary, true); await verifyReceiptPin(role.producer); pins.push(role.binary, role.producer); }
  closed(evidence.engineer, ["result", "trace", "storage", "auth"], "native engineer evidence");
  const native = await json(evidence.engineer.result), engineer = validateCodexCapture(native, await stream(evidence.engineer.trace), slot.engineer);
  if (native.tracePath !== evidence.engineer.trace.path || engineer.sessionId !== options.candidate.engineerActorId) throw new Error("Engineer identity/trace differs from registered source evidence");
  const owner = slot.engineer.owner;
  await verifyStorage(owner.storage, evidence.engineer.storage, pins); await verifyReceiptPin(evidence.engineer.auth);
  await verifyOwnerTerminal({ storage: evidence.engineer.storage.path, auth: evidence.engineer.auth.path, storagePlanDigest: owner.storage.planDigest, authRunId: owner.authRunId });
  const auth = await json(evidence.engineer.auth);
  if (auth.schema !== "gptgrep.codex-auth.run.v1" || auth.operation !== "run" || auth.state !== "PASS" || auth.child_exit_code !== 0 || auth.exit_code !== 0 ||
      auth.role !== owner.authRole || auth.auth_source_id !== owner.authSourceId || auth.cwd !== owner.command.cwd || canonical(auth.argv) !== canonical(owner.command.argv) ||
      !["shared", "exclusive"].includes(auth.lease?.mode) || auth.lease?.retained_by_guardian_pid) throw new Error("Native auth terminal is not the exact successful engineer command");
  const authLink = auth.layout?.auth_link;
  if (!isAbsolute(auth.codex_home ?? "") || authLink !== join(auth.codex_home, "auth.json")) throw new Error("Native auth link is not the exact per-run owner path");
  // Metadata only: never open, hash, follow or print a credential file.
  await physicalPath(auth.codex_home);
  try { await lstat(authLink); throw new Error("Native auth link still exists"); }
  catch (error: any) { if (error.code !== "ENOENT") throw error; }
  pins.push(evidence.engineer.result, evidence.engineer.trace, evidence.engineer.auth);
  closed(evidence.review, ["request", "receipt", "native", "task", "preparedPrompt", "storage"], "native review evidence");
  const request = await json(evidence.review.request), receipt = await json(evidence.review.receipt), raw = await json(evidence.review.native);
  const reviewer = validateAgyEnvelope(raw, request, receipt, slot.reviewer, options.candidate.revision);
  if (reviewer.sessionId !== options.candidate.reviewerActorId || reviewer.sessionId === engineer.sessionId) throw new Error("Independent AGY native identity differs from the registered review");
  if (canonical(options.candidate.reviewTerminal) !== canonical(evidence.review.receipt) || !within(slot.reviewer.owner.outputRoot, evidence.review.receipt.path) ||
      receipt.artifacts?.stdout?.path !== evidence.review.native.path || receipt.artifacts?.stdout?.sha256 !== evidence.review.native.sha256 ||
      receipt.prepared_prompt_sha256 !== evidence.review.preparedPrompt.sha256 || join(dirname(evidence.review.receipt.path), receipt.argv_prompt_ref ?? "") !== evidence.review.preparedPrompt.path) throw new Error("AGY raw output/prompt is not the actual owner-retained attempt");
  closed(request.prompt, ["ref", "sha256"], "AGY task prompt pin");
  if (typeof request.prompt.ref !== "string" || isAbsolute(request.prompt.ref) || !within(dirname(evidence.review.request.path), resolve(dirname(evidence.review.request.path), request.prompt.ref)) ||
      resolve(dirname(evidence.review.request.path), request.prompt.ref) !== evidence.review.task.path || request.prompt.sha256 !== evidence.review.task.sha256) throw new Error("AGY task file does not match the exact native request");
  const packet = await json(evidence.review.task);
  closed(packet, ["schema", "instructions", "candidateId", "parentRevision", "revision", "diffSha256", "build", "checks"], "AGY candidate review packet");
  if (packet.schema !== "gptgrep.opt-review-packet.v1" || packet.candidateId !== evidence.candidateId || packet.parentRevision !== policy.parentRevision || packet.revision !== options.candidate.revision ||
      packet.diffSha256 !== options.candidate.diffSha256 || canonical(packet.build) !== canonical(options.candidate.build) || canonical(packet.checks) !== canonical(options.candidate.checks) ||
      typeof packet.instructions !== "string" || sha256(packet.instructions) !== slot.reviewer.inputSha256) throw new Error("AGY prompt does not bind the exact diff/build/checks and frozen instructions");
  await verifyReceiptPin(evidence.review.preparedPrompt);
  const prompt = await readFile(evidence.review.preparedPrompt.path, "utf8"), task = await readFile(evidence.review.task.path, "utf8");
  if (!prompt.endsWith(`Task (selected by parent):\n${task}`)) throw new Error("Prepared native AGY prompt omits the selected candidate packet");
  const schema = await json(slot.reviewer.owner.resultSchema);
  if (canonical(JSON.parse(flag(request.argv, "--json-schema"))) !== canonical(schema)) throw new Error("AGY request replaced the selected Composer result schema");
  if (!Array.isArray(request.agy.root_instructions) || !request.agy.root_instructions.length || request.agy.root_instructions.length > 8) throw new Error("AGY root instruction pins are missing");
  for (const source of [...slot.reviewer.owner.sources, slot.reviewer.owner.resultSchema, ...request.agy.root_instructions]) { await verifyReceiptPin(source); pins.push(source); }
  if (!slot.reviewer.owner.sources.some(p => canonical(p) === canonical(slot.reviewer.producer)) || !slot.reviewer.owner.sources.some(p => basename(p.path) === "composer_agy.py")) throw new Error("Selected Composer producer sources are incomplete");
  await verifyStorage(slot.reviewer.owner.storage, evidence.review.storage, pins);
  pins.push(evidence.review.request, evidence.review.receipt, evidence.review.native, evidence.review.task, evidence.review.preparedPrompt);
  if (!Array.isArray(evidence.checks) || canonical(evidence.checks.map(c => c.id).sort()) !== canonical(slot.checks.map(c => c.id).sort()) ||
      canonical(options.candidate.checkArgv) !== canonical(slot.checks.map(c => c.argv))) throw new Error("Candidate checks differ from the exact frozen checks");
  for (const [index, check] of slot.checks.entries()) {
    const observed = evidence.checks.find(c => c.id === check.id)!; closed(observed, ["id", "storage"], "candidate check evidence");
    if (canonical(options.candidate.checkTerminals[index]) !== canonical(observed.storage)) throw new Error("Declared check terminal differs from the actual owner receipt");
    await verifyStorage(check.owner, observed.storage, pins);
  }
  // Existing native-account success is separate from protected evaluator visibility and audit acceptance.
  // Composer explicitly leaves effective all-tool confinement and descendant quiescence unverified.
  const reasons = ["native_execution_binding_not_yet_qualified", "agy_evaluator_data_isolation_not_yet_qualified",
    "agy_audit_acceptance_not_yet_independently_qualified", "agy_descendant_quiescence_unavailable"];
  const validatorSources = await Promise.all(SOURCE_PATHS.map(async path => ({ path, sha256: await fileHash(path) })));
  const uniquePins = [...new Map(pins.map(pin => [canonical(pin), pin])).values()].sort((a, b) => a.path.localeCompare(b.path));
  for (const pin of uniquePins) await verifyReceiptPin(pin, executablePaths.has(pin.path));
  const result = { schema: "gptgrep.opt-native-qualification.v1" as const, status: "unavailable" as const, reasons,
    policy: options.policyPin, evidence: options.evidencePin, candidateEvidence: options.candidateEvidencePin,
    candidateId: evidence.candidateId, candidateRevision: options.candidate.revision, diffSha256: options.candidate.diffSha256,
    engineerSessionId: engineer.sessionId, reviewerSessionId: reviewer.sessionId, engineerAuthRunId: owner.authRunId,
    worktree: slot.worktree, reviewerConfiguration: { requestedModel: reviewer.requestedModel, requestedEffort: reviewer.requestedEffort,
      observedModel: null, observedEffort: null, serviceTier: reviewer.serviceTier }, validatorSources, validatedPins: uniquePins, authLinkAbsence: { path: authLink, absent: true as const } };
  return { ...result, bindingSha256: digest(result) };
}
