/** Shared guest data and capture checks. This module imports no campaign, controller or judge. */
import { lstat, mkdir, open } from "node:fs/promises";
import { basename, dirname, isAbsolute } from "node:path";
import { fileHash, physicalPath, requireStorage, within } from "../../benchmark/src/io";
import { boundedText, closed, hash, revision, validateHypothesis, type Hypothesis } from "./proposals";
import type { ActorPolicy } from "./actor-contracts";
import type { NativeRole, ReceiptPin } from "./native-receipts";
import type { Pin } from "./competition";
export interface EngineerOutput {
  schema: "gptgrep.opt-engineer-answer.v1";
  candidateRevision: string; hypothesis: Hypothesis; summary: string;
}
export const OUTPUT_SCHEMA = {
  type: "object", additionalProperties: false, required: ["schema", "candidateRevision", "hypothesis", "summary"],
  properties: { schema: { type: "string", enum: ["gptgrep.opt-engineer-answer.v1"] }, candidateRevision: { type: "string" }, summary: { type: "string" },
    hypothesis: { type: "object", additionalProperties: false,
      required: ["schema", "id", "kind", "capability", "claim", "expectedBehavior", "falsification", "mutationPaths", "feedbackIds"],
      properties: { schema: { type: "string", enum: ["gptgrep.opt-hypothesis.v1"] }, id: { type: "string" },
        kind: { type: "string", enum: ["invariant", "workflow"] }, capability: { type: "string", enum: ["agentic_parse", "hybrid_index", "decision_helper", "hybrid_search"] },
        claim: { type: "string" }, expectedBehavior: { type: "string" }, falsification: { type: "string" },
        mutationPaths: { type: "array", items: { type: "string" } }, feedbackIds: { type: "array", items: { type: "string" } } } } },
};
export function validateEngineerOutput(value: EngineerOutput, policy: ActorPolicy): void {
  closed(value, ["schema", "candidateRevision", "hypothesis", "summary"], "native engineering output");
  if (value.schema !== "gptgrep.opt-engineer-answer.v1") throw new Error("Native engineering output schema differs");
  revision(value.candidateRevision); validateHypothesis(value.hypothesis); boundedText(value.summary, "engineering summary", 2000);
  if (value.candidateRevision === policy.parentRevision || value.hypothesis.mutationPaths.some(p => !policy.mutationPaths.includes(p))) throw new Error("Native candidate is unchanged or outside the frozen mutation surface");
}
export async function verifyReceiptPin(pin: ReceiptPin, executable = false): Promise<void> {
  closed(pin, ["path", "sha256"], "native evidence pin"); hash(pin.sha256, "native evidence digest");
  if (/^(?:auth\.json|credentials(?:\.json)?|\.netrc|\.npmrc|\.pypirc|\.env(?:\..*)?)$/i.test(basename(pin.path ?? "")) || /\.env$/i.test(basename(pin.path ?? ""))) throw new Error("Credential files cannot be actor evidence");
  if (!isAbsolute(pin.path)) throw new Error("Native evidence requires a physical absolute path");
  await physicalPath(pin.path);
  const info = await lstat(pin.path);
  if (!info.isFile() || info.size > (executable ? 1024 : 64) * 1024 * 1024 || executable && !(info.mode & 0o111) || await fileHash(pin.path) !== pin.sha256) throw new Error("Native evidence pin changed or exceeds its bound");
}
/** Parse bounded captures only. Raw provider request counts and served-model absence stay absent. */
export function validateCodexCapture(result: any, events: any[], expected: NativeRole) {
  if (result?.schema !== "gptgrep.native-run.v1" || result.executionMode !== "live" || result.status !== "completed" || result.validStart !== true ||
      result.requestedModel !== expected.model || result.requestedEffort !== expected.effort || result.requestedServiceTier !== expected.serviceTier ||
      result.observedModel !== expected.model || result.runtimeBinarySha256 !== expected.binary.sha256 || result.modelBoundaryValid !== true ||
      result.traceTruncated !== false || result.cleanup?.completed !== true || result.authFailure || result.resultOmission ||
      !Array.isArray(result.captureGaps) || result.captureGaps.length || !Array.isArray(result.reroutes) || result.reroutes.length ||
      result.servedModel !== null && result.servedModel !== expected.model) throw new Error("Actual successful native Codex evidence is unavailable");
  for (const key of ["threadId", "sessionId", "turnId"]) boundedText(result[key], `native ${key}`, 256);
  if (result.turnRequests !== 1 || !Array.isArray(events) || !events.length) throw new Error("Exactly one observed native turn is required");
  const starts = events.filter(e => e.direction === "request" && e.method === "turn/start");
  const terminals = events.filter(e => e.direction === "result");
  if (starts.length !== 1 || starts[0].inputSha256 !== expected.inputSha256 || terminals.length !== 1) throw new Error("Native input/terminal trace does not match the frozen request");
  const last = terminals[0];
  if (last.status !== "completed" || ["threadId", "sessionId", "turnId", "requestedModel", "observedModel", "servedModel"].some(k => last[k] !== result[k]) || last.cleanup?.completed !== true) throw new Error("Native terminal and result disagree");
  if (!events.some(e => e.direction === "request" && e.method === "thread/start") || !events.some(e => e.direction === "response" && e.id === starts[0].id && e.ok === true)) throw new Error("Native start acknowledgment is missing");
  return { sessionId: result.sessionId as string, threadId: result.threadId as string, turnId: result.turnId as string };
}
export async function immutable(path: string, value: unknown, maxBytes = 2 * 1024 * 1024): Promise<Pin> {
  await requireStorage(path); await physicalPath(path);
  const bytes = Buffer.from(`${JSON.stringify(value, null, 2)}\n`);
  if (bytes.length > maxBytes) throw new Error("Actor artifact exceeds its byte bound");
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const file = await open(path, "wx", 0o600);
  try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
  const directory = await open(dirname(path), "r"); try { await directory.sync(); } finally { await directory.close(); }
  return { path, sha256: await fileHash(path) };
}
export function validateBuildEnvironment(environment: Record<string, string>, root: string, outputDir: string): void {
  const paths = ["HOME", "TMPDIR", "TMP", "TEMP", "XDG_CACHE_HOME", "XDG_STATE_HOME", "CARGO_HOME", "CARGO_TARGET_DIR", "UV_CACHE_DIR", "BUN_INSTALL_CACHE_DIR"];
  if (!environment || Object.keys(environment).some(key => ![...paths, "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "PYTHONDONTWRITEBYTECODE"].includes(key)) ||
      ["HOME", "TMPDIR", "TMP", "TEMP", "XDG_CACHE_HOME", "CARGO_HOME", "CARGO_TARGET_DIR"].some(key => !environment[key]) ||
      paths.some(key => environment[key] !== undefined && (!isAbsolute(environment[key]!) || !within(root, environment[key]!))) ||
      !within(outputDir, environment.CARGO_TARGET_DIR!) || environment.CARGO_BUILD_JOBS !== "1" || environment.CARGO_INCREMENTAL !== "0" ||
      environment.PYTHONDONTWRITEBYTECODE !== "1") throw new Error("Build dependencies, cache, temporary files and targets must use explicit external storage");
}
export async function inspectLinuxBinary(path: string, arch: "x64" | "arm64") {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || !(info.mode & 0o111) || info.size < 64 || info.size > 1024 * 1024 * 1024) throw new Error("Candidate artifact is not a bounded executable regular file");
  const file = await open(path, "r"), head = Buffer.alloc(64);
  try { await file.read(head, 0, 64, 0); } finally { await file.close(); }
  if (head.subarray(0, 4).toString("hex") !== "7f454c46" || head[4] !== 2 || head[5] !== 1 ||
      ![2, 3].includes(head.readUInt16LE(16)) || head.readUInt16LE(18) !== (arch === "x64" ? 62 : 183)) throw new Error("Candidate artifact is not the frozen Linux ELF architecture");
  return { path, sha256: await fileHash(path) };
}
