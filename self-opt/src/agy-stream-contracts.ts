/** Project-owned AGY stream data. This module imports no campaign, controller or judge. */
import { isAbsolute, resolve } from "node:path";
import { canonical, digest } from "../../benchmark/src/io";
import { boundedText, closed, hash, identity, revision } from "./proposals";
import type { ProcessObservation } from "../../benchmark/src/process";
import type { Pin } from "./competition";

const integer = (v: unknown, low = 1, high = Number.MAX_SAFE_INTEGER): v is number => Number.isSafeInteger(v) && (v as number) >= low && (v as number) <= high;
const path = (v: unknown): v is string => typeof v === "string" && isAbsolute(v) && resolve(v) === v && v !== "/" && !/[\0\r\n]/.test(v);
export const DENY_ID = "gptgrep-deny-all-v1";
export const DENY_REASON = `${DENY_ID}: all model-selected tool dispatch is denied; use only the parent-supplied packet.`;
/** Same portable helper body as the retained native deny-policy probe. */
export const DENY_HELPER = `#!/bin/sh
# Parent-owned policy helper. Tool input is never evaluated, inspected or retained.
# Argument 1 is an existing regular private receipt file pinned by the parent.
if [ "$#" -eq 1 ] && [ -f "$1" ] && [ ! -L "$1" ]; then
    printf '%s\\n' '${DENY_ID}' >> "$1" || :
fi
# Drain the hook JSON without retaining it. Never substitute it into a command.
/bin/cat >/dev/null || :
printf '%s\\n' '{"decision":"deny","reason":"${DENY_REASON}"}'
exit 0
`;

export interface AgyReviewResult {
  status: "completed"; summary: string;
  findings: { claim: string; evidence_ids: string[] }[];
  evidence: { id: string; locator: string; note: string }[];
  limitations: string[];
}
export function validateAgyReviewResult(value: any, maxSources: number): asserts value is AgyReviewResult {
  closed(value, ["status", "summary", "findings", "evidence", "limitations"], "native AGY review result");
  if (value.status !== "completed") throw new Error("Partial or blocked AGY output cannot qualify a review");
  boundedText(value.summary, "AGY summary", 1600);
  if (![value.findings, value.evidence, value.limitations].every(x => Array.isArray(x) && x.length <= 16) || value.evidence.length > maxSources) throw new Error("AGY result exceeds its selected bound");
  const ids = new Set<string>();
  for (const item of value.evidence) {
    closed(item, ["id", "locator", "note"], "AGY source evidence");
    boundedText(item.id, "AGY evidence ID", 64); boundedText(item.locator, "AGY source locator", 1600);
    if (typeof item.note !== "string" || item.note.length > 600 || ids.has(item.id)) throw new Error("AGY evidence is invalid or duplicated");
    ids.add(item.id);
  }
  for (const item of value.findings) {
    closed(item, ["claim", "evidence_ids"], "AGY finding"); boundedText(item.claim, "AGY finding", 1200);
    if (!Array.isArray(item.evidence_ids) || !item.evidence_ids.length || item.evidence_ids.length > 16 || item.evidence_ids.some((id: unknown) => !ids.has(id as string))) throw new Error("AGY finding has unavailable evidence");
  }
  value.limitations.forEach((item: unknown) => boundedText(item, "AGY limitation", 600));
}
export interface AgyStreamPacket {
  schema: "gptgrep.opt-review-packet.v3"; instructions: string; task: string;
  candidateId: string; parentRevision: string; revision: string; diffSha256: string;
  changedPaths: string[]; hypothesis: unknown;
  build: { binarySha256: string; platform: "linux"; targetArch: "x64" | "arm64" };
  checks: { argv: string[]; exitCode: number | null }[];
  diff: string; sources: { path: string; before: string | null; after: string | null }[];
}
export function validateAgyStreamPacket(value: any): asserts value is AgyStreamPacket {
  closed(value, ["schema", "instructions", "task", "candidateId", "parentRevision", "revision", "diffSha256", "changedPaths", "hypothesis", "build", "checks", "diff", "sources"], "complete AGY review packet");
  if (value.schema !== "gptgrep.opt-review-packet.v3") throw new Error("Complete source-bearing AGY packet required");
  identity(value.candidateId, "candidate ID"); revision(value.parentRevision); revision(value.revision); hash(value.diffSha256, "candidate diff");
  if (value.parentRevision === value.revision || typeof value.instructions !== "string" || !value.instructions.trim() || typeof value.task !== "string" || !value.task.trim() ||
      typeof value.diff !== "string" || !value.diff || digest(value.diff) !== value.diffSha256 || !Array.isArray(value.sources) || !value.sources.length || value.sources.length > 256 ||
      !Array.isArray(value.changedPaths) || canonical(value.changedPaths) !== canonical(value.sources.map((s: any) => s.path).sort()) || new Set(value.changedPaths).size !== value.changedPaths.length) throw new Error("AGY packet omits or changes its complete candidate diff/source bindings");
  for (const source of value.sources) {
    closed(source, ["path", "before", "after"], "candidate source body");
    if (typeof source.path !== "string" || !/^[A-Za-z0-9_.][A-Za-z0-9_./-]*$/.test(source.path) || source.path.split("/").some((p: string) => ["", ".", "..", ".git"].includes(p)) ||
        source.before !== null && typeof source.before !== "string" || source.after !== null && typeof source.after !== "string" || source.before === null && source.after === null ||
        [source.before, source.after].some(text => typeof text === "string" && text.includes("\0"))) throw new Error("AGY source body is not an exact regular text-source projection");
  }
  closed(value.build, ["binarySha256", "platform", "targetArch"], "candidate build"); hash(value.build.binarySha256, "candidate binary");
  if (value.build.platform !== "linux" || !["x64", "arm64"].includes(value.build.targetArch) || !Array.isArray(value.checks) || !value.checks.length || value.checks.length > 16 ||
      value.checks.some((c: any) => !Array.isArray(c.argv) || !c.argv.length || c.argv.some((a: unknown) => typeof a !== "string") || c.exitCode !== 0)) throw new Error("AGY packet lacks its successful candidate build/check bindings");
}
export interface AgyStreamJob {
  schema: "gptgrep.opt-agy-stream-job.v1"; requestId: string;
  candidateId: string; parentRevision: string; candidateRevision: string; diffSha256: string;
  packet: Pin; binary: Pin; bun: Pin; producer: Pin; producerSources: Pin[]; resultSchema: Pin;
  cwd: string; nativeHome: string; outputRoot: string; model: string; effort: string;
  deadlineAt: number; timeoutMs: number; maxPacketBytes: number; maxInputBytes: number;
  maxTraceBytes: number; maxResultBytes: number; maxSources: number;
  denyPolicy: { hook: Pin; helper: Pin; spoolPath: string };
}
export function validateAgyStreamJob(value: AgyStreamJob): void {
  closed(value, ["schema", "requestId", "candidateId", "parentRevision", "candidateRevision", "diffSha256", "packet", "binary", "bun", "producer", "producerSources", "resultSchema", "cwd", "nativeHome", "outputRoot", "model", "effort", "deadlineAt", "timeoutMs", "maxPacketBytes", "maxInputBytes", "maxTraceBytes", "maxResultBytes", "maxSources", "denyPolicy"], "AGY stream job");
  if (value.schema !== "gptgrep.opt-agy-stream-job.v1") throw new Error("Explicit project AGY stream job required");
  identity(value.requestId, "AGY request ID"); identity(value.candidateId, "candidate ID"); revision(value.parentRevision); revision(value.candidateRevision); hash(value.diffSha256, "candidate diff");
  for (const key of ["model", "effort"] as const) boundedText(value[key], `AGY ${key}`, 256);
  if (![value.cwd, value.nativeHome, value.outputRoot].every(path) || !integer(value.deadlineAt) || !integer(value.timeoutMs, 1, 300_000) ||
      !integer(value.maxPacketBytes, 1, 8 * 1024 * 1024) || !integer(value.maxInputBytes, value.maxPacketBytes, 16 * 1024 * 1024) ||
      !integer(value.maxTraceBytes, 1, 8 * 1024 * 1024) || !integer(value.maxResultBytes, 1, 2 * 1024 * 1024) || !integer(value.maxSources, 1, 16) || !Array.isArray(value.producerSources)) throw new Error("AGY stream limits or physical paths are unavailable");
  closed(value.denyPolicy, ["hook", "helper", "spoolPath"], "selected AGY deny policy");
  if (!path(value.denyPolicy.spoolPath)) throw new Error("Exact helper receipt path required");
}
export interface AgyStreamReceipt {
  schema: "gptgrep.opt-agy-stream-receipt.v1"; job: Pin;
  candidateId: string; parentRevision: string; candidateRevision: string; diffSha256: string;
  status: "review_reported" | "failed" | "unknown"; error: string | null;
  packet: Pin; preparedPrompt: Pin; inputFrame: Pin; trace: Pin; native: Pin | null;
  inputBytes: number; submittedUserEvents: number; userInputAcknowledgements: number;
  sessionId: string | null; nativeNumTurns: number | null; usage: unknown;
  result: AgyReviewResult | null; process: ProcessObservation;
  lifecycle: { leaderClosed: boolean; groupAbsenceProven: boolean; signalErrorCount: number;
    signalErrors: { signal: string; code: string | null; errno: number | null }[];
    groupProbeError: { code: string | null; errno: number | null } | null };
  policy: { filesUnchanged: boolean; helperMarkers: number | null; terminalCalls: unknown[] };
  requestedModel: string; requestedEffort: string;
  observedModel: null; observedEffort: null; providerRequestCount: null; currencyCost: null;
  originalDeadlineAt: number; acceptance: "pending_parent_adjudication";
  descendantQuiescence: "not_established"; releaseAccepted: false;
}
