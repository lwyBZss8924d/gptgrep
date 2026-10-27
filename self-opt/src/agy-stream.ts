/** One owner-admitted native AGY review. Full packet bytes travel on stdin, never argv. */
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { lstat, mkdir, open, readFile, realpath } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { canonical, requireStorage, sha256 } from "../../benchmark/src/io";
import { verifyReceiptPin } from "./actor-guest-contracts";
import { DENY_HELPER, DENY_ID, DENY_REASON, validateAgyReviewResult, validateAgyStreamJob, validateAgyStreamPacket,
  type AgyStreamJob, type AgyStreamReceipt } from "./agy-stream-contracts";
import type { ProcessObservation } from "../../benchmark/src/process";
import type { Pin } from "./competition";

export const AGY_STREAM_PRODUCER = fileURLToPath(import.meta.url);
export const AGY_STREAM_SOURCES = [AGY_STREAM_PRODUCER, ...["./agy-stream-contracts.ts", "./actor-guest-contracts.ts", "./proposals.ts", "./mutation-paths.ts", "../../benchmark/src/io.ts"].map(path => fileURLToPath(new URL(path, import.meta.url)))];
export const shellQuote = (value: string) => `'${value.replace(/'/g, `'"'"'`)}'`;
export function agyDenyDefinition(helper: string, spool: string) {
  return { [DENY_ID]: { enabled: true, PreToolUse: [{ matcher: "*", hooks: [{ type: "command",
    command: `/bin/sh ${shellQuote(helper)} ${shellQuote(spool)}`, timeout: 5 }] }] } };
}
export function agyStreamArgv(job: AgyStreamJob, schema: unknown): string[] {
  return ["--log-file", "/dev/null", "--model", job.model, "--effort", job.effort, "--sandbox", "--disable-slash-commands",
    "--input-format", "stream-json", "--output-format", "stream-json", "--print-timeout", `${job.timeoutMs / 1000}s`, "--json-schema", JSON.stringify(schema)];
}
async function bytes(pin: Pin, maximum: number): Promise<Buffer> {
  await verifyReceiptPin(pin);
  if ((await lstat(pin.path)).size > maximum) throw new Error("AGY input exceeds its exact declared byte bound");
  const raw = await readFile(pin.path);
  if (raw.length > maximum || sha256(raw) !== pin.sha256) throw new Error("AGY input changed while reading");
  return raw;
}
async function write(path: string, raw: Uint8Array): Promise<Pin> {
  const stream = await open(path, "wx", 0o600);
  try { await stream.writeFile(raw); await stream.sync(); } finally { await stream.close(); }
  return { path, sha256: sha256(raw) };
}
export async function agyHelperMarkers(path: string): Promise<number> {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || info.nlink !== 1 || info.size > 1048576) throw new Error("AGY helper receipt is not a bounded regular file");
  const raw = await readFile(path, "utf8");
  if (raw !== `${DENY_ID}\n`.repeat(raw.split("\n").length - 1)) throw new Error("Unexpected AGY helper marker");
  return raw.split("\n").length - 1;
}
/** Every attempted spawn retains its result/counters before any success check. No retry path exists. */
export async function runAgyStream(jobPin: Pin, options: { signal?: AbortSignal } = {}): Promise<Pin> {
  const admission = await requireStorage();
  if (admission.container) throw new Error("AGY stream producer requires its selected host owner route, not container adoption");
  const job = JSON.parse((await bytes(jobPin, 128 * 1024)).toString("utf8")) as AgyStreamJob;
  validateAgyStreamJob(job); await requireStorage(job.outputRoot, job.cwd, dirname(job.denyPolicy.spoolPath));
  await verifyReceiptPin(job.binary, true); await verifyReceiptPin(job.bun, true); await verifyReceiptPin(job.producer);
  if (canonical(job.producerSources.map(pin => pin.path).sort()) !== canonical([...AGY_STREAM_SOURCES].sort())) throw new Error("AGY stream producer source closure differs");
  for (const source of job.producerSources) await verifyReceiptPin(source);
  if (job.producer.path !== AGY_STREAM_PRODUCER || await realpath(process.execPath) !== job.bun.path || process.env.HOME !== job.nativeHome) throw new Error("AGY producer, Bun or original native HOME differs from the selected host route");
  for (const name of ["OP_SERVICE_ACCOUNT_TOKEN", "GPTGREP_AGY_KEYRING_MASTER", "OPENROUTER_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY", "ANTHROPIC_API_KEY"]) {
    if (process.env[name]) throw new Error("Ambient provider or vault credential is outside the selected native account route");
  }
  const packetRaw = await bytes(job.packet, job.maxPacketBytes), packetText = new TextDecoder("utf-8", { fatal: true }).decode(packetRaw);
  const packet = JSON.parse(packetText); validateAgyStreamPacket(packet);
  if (packet.candidateId !== job.candidateId || packet.parentRevision !== job.parentRevision || packet.revision !== job.candidateRevision || packet.diffSha256 !== job.diffSha256) throw new Error("AGY full packet belongs to another exact candidate");
  const schema = JSON.parse((await bytes(job.resultSchema, 64 * 1024)).toString("utf8"));
  if (job.denyPolicy.hook.path !== join(job.cwd, ".agents", "hooks.json") || job.denyPolicy.helper.path !== join(job.cwd, ".agents", "deny-all.sh") ||
      canonical(JSON.parse((await bytes(job.denyPolicy.hook, 65536)).toString("utf8"))) !== canonical(agyDenyDefinition(job.denyPolicy.helper.path, job.denyPolicy.spoolPath)) ||
      (await bytes(job.denyPolicy.helper, 65536)).toString("utf8") !== DENY_HELPER || await agyHelperMarkers(job.denyPolicy.spoolPath) !== 0) throw new Error("Fresh exact AGY deny files and empty helper receipt required");
  const frame = Buffer.from(`${JSON.stringify({ event: "user", message: { content: packetText } })}\n`);
  if (frame.length > job.maxInputBytes || Date.now() + job.timeoutMs + 2000 > job.deadlineAt) throw new Error("Complete AGY input or native lifetime cannot fit the frozen job");
  // A fresh evidence directory is the local once-attempt boundary, even after a failed submission.
  await mkdir(job.outputRoot, { mode: 0o700 });
  const preparedPrompt = await write(join(job.outputRoot, "prepared-prompt.txt"), packetRaw);
  const inputFrame = await write(join(job.outputRoot, "input.ndjson"), frame);
  const startedAt = new Date().toISOString(), started = Date.now(), argv = agyStreamArgv(job, schema);
  const stdoutHash = createHash("sha256"), stderrHash = createHash("sha256"), outputHash = createHash("sha256");
  let stdoutBytes = 0, stderrBytes = 0, outputBytes = 0, pending = Buffer.alloc(0), traceRaw = Buffer.alloc(0);
  let submittedUserEvents = 0, userInputAcknowledgements = 0, init: any = null, native: any = null;
  let error: string | null = null, timedOut = false, cancelled = false, outputExceeded = false, cleanup = true;
  let exitCode: number | null = null, exitSignal: string | null = null;
  const lifecycle: AgyStreamReceipt["lifecycle"] = { leaderClosed: false, groupAbsenceProven: false, signalErrorCount: 0, signalErrors: [], groupProbeError: null };
  const errorCode = (caught: any) => ({ code: typeof caught?.code === "string" && /^[A-Z_0-9]{1,64}$/.test(caught.code) ? caught.code : null,
    errno: Number.isSafeInteger(caught?.errno) ? caught.errno as number : null });
  const terminalCalls: unknown[] = [];
  const child = spawn(job.binary.path, argv, { cwd: job.cwd, env: process.env, detached: true, shell: false, stdio: ["pipe", "pipe", "pipe"] });
  const stop = (signal: NodeJS.Signals) => {
    if (child.pid) try { process.kill(-child.pid, signal); } catch (caught: any) {
      if (caught.code !== "ESRCH") {
        lifecycle.signalErrorCount++; if (lifecycle.signalErrors.length < 8) lifecycle.signalErrors.push({ signal, ...errorCode(caught) });
        cleanup = false; error ??= "native-group-signal-failed";
      }
    }
  };
  const rejectEvent = (caught: any) => {
    error ??= /^(native-|duplicate-|unrecognized-)/.test(caught?.message ?? "") ? caught.message : "native-stream-parse-or-write-failed";
    stop("SIGKILL");
  };
  let abortForce: ReturnType<typeof setTimeout> | undefined;
  child.stdin.on("error", () => { error ??= "native-input-write-failed"; stop("SIGKILL"); });
  const abort = () => { cancelled = true; error ??= "cancelled"; stop("SIGTERM"); abortForce ??= setTimeout(() => stop("SIGKILL"), 250); };
  options.signal?.addEventListener("abort", abort, { once: true });
  if (options.signal?.aborted) abort();
  const timer = setTimeout(() => { timedOut = true; error ??= "native-timeout"; stop("SIGTERM"); }, job.timeoutMs);
  const force = setTimeout(() => stop("SIGKILL"), job.timeoutMs + 1000);
  let watchdog: ReturnType<typeof setTimeout> | undefined;
  const closed = new Promise<void>(done => {
    child.once("error", () => { error ??= "native-spawn-failed"; if (!child.pid) done(); else stop("SIGKILL"); });
    child.once("exit", (code, signal) => { exitCode = code; exitSignal = signal; });
    child.once("close", (code, signal) => { lifecycle.leaderClosed = true; exitCode = code; exitSignal = signal; done(); });
    watchdog = setTimeout(() => { cleanup = false; timedOut = true; error ??= "native-pipes-unresolved"; stop("SIGKILL"); child.stdout.destroy(); child.stderr.destroy(); child.unref(); done(); }, job.timeoutMs + 2000);
  });
  const observe = (block: Buffer, stream: "stdout" | "stderr") => {
    outputHash.update(block); outputBytes += block.length;
    if (stream === "stdout") { stdoutHash.update(block); stdoutBytes += block.length; }
    else { stderrHash.update(block); stderrBytes += block.length; }
    if (outputBytes > job.maxTraceBytes) { outputExceeded = true; error ??= "native-output-bound"; stop("SIGKILL"); }
  };
  const parse = async (line: Buffer) => {
    const event = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(line));
    if (event.event === "init") {
      if (error || init || submittedUserEvents || native || cancelled || timedOut || typeof event.conversation_id !== "string" || !/^[0-9a-fA-F-]{36}$/.test(event.conversation_id) ||
          event.init?.model !== job.model || event.init?.cwd !== job.cwd || event.init?.permission_mode !== "request-review" || !Array.isArray(event.init?.tools) || event.init?.agent !== undefined) throw new Error("native-init-identity-mismatch");
      init = { conversationId: event.conversation_id, ...event.init };
      submittedUserEvents = 1; // A partial/failed pipe write is still a consumed submission attempt.
      await new Promise<void>((resolve, reject) => child.stdin.write(frame, caught => caught ? reject(caught) : resolve()));
      child.stdin.end();
    } else if (event.event === "step_update") {
      const step = event.step_update;
      if (!init || submittedUserEvents !== 1 || step?.conversation_id !== init.conversationId) throw new Error("native-step-identity-mismatch");
      if (step.step_type === "user_input" && step.state === "DONE" && ++userInputAcknowledgements !== 1) throw new Error("duplicate-native-user-acknowledgement");
      if (step.subagent_info?.subagents?.length) throw new Error("native-subagent-observed");
      if (["tool", "subagent"].includes(step.step_type) && ["DONE", "ERROR"].includes(step.state)) {
        const markers = await agyHelperMarkers(job.denyPolicy.spoolPath), message = step.tool_info?.error?.message;
        const intendedDeny = typeof message === "string" && message.includes(DENY_REASON) && markers > 0;
        terminalCalls.push({ stepIndex: step.step_index, stepType: step.step_type, state: step.state, name: step.tool_name,
          helperMarkers: markers, intendedDeny, subagentReasonExposed: step.step_type === "subagent" ? false : null });
        if (step.step_type === "tool" && !intendedDeny) throw new Error("native-tool-not-proven-denied");
      }
    } else if (event.event === "result") {
      if (!init || native || event.result?.conversation_id !== init.conversationId) throw new Error("native-result-identity-mismatch");
      native = event.result;
    } else throw new Error("unrecognized-native-event");
  };
  try {
    await Promise.all([closed, (async () => {
      try {
        for await (const value of child.stdout) {
          const block = Buffer.from(value); observe(block, "stdout");
          const room = job.maxTraceBytes - traceRaw.length; if (room > 0) traceRaw = Buffer.concat([traceRaw, block.subarray(0, room)]);
          if (outputExceeded) continue;
          pending = Buffer.concat([pending, block]);
          let boundary: number;
          while ((boundary = pending.indexOf(10)) >= 0) {
            const line = pending.subarray(0, boundary); pending = pending.subarray(boundary + 1);
            // Reject and stop immediately, but keep bounded terminal capture.
            // A later already-emitted result still owns its real turns/usage;
            // the error above prevents any fresh or repeated user submission.
            try { await parse(line); } catch (caught) { rejectEvent(caught); }
          }
        }
      } catch (caught) { rejectEvent(caught); }
    })(), (async () => { try { for await (const value of child.stderr) observe(Buffer.from(value), "stderr"); } catch { error ??= "native-stderr-read-failed"; stop("SIGKILL"); } })()]);
  } finally {
    clearTimeout(timer); clearTimeout(force); clearTimeout(watchdog); clearTimeout(abortForce); options.signal?.removeEventListener("abort", abort); stop("SIGKILL");
    // A close event can precede the kernel's final group disappearance. Observe
    // only this owned group, within the original two-second cleanup allowance.
    const cleanupUntil = Math.min(Date.now() + 250, started + job.timeoutMs + 2000, job.deadlineAt);
    if (child.pid) for (;;) {
      try { process.kill(-child.pid, 0); }
      catch (caught: any) {
        if (caught.code === "ESRCH") lifecycle.groupAbsenceProven = true;
        else { cleanup = false; lifecycle.groupProbeError = errorCode(caught); }
        break;
      }
      if (Date.now() >= cleanupUntil) { cleanup = false; break; }
      await new Promise(resolve => setTimeout(resolve, Math.min(10, cleanupUntil - Date.now())));
    }
  }
  const processObservation: ProcessObservation = { startedAt, completedAt: new Date().toISOString(), latencyMs: Date.now() - started,
    started: Boolean(child.pid), pid: child.pid ?? null, executionError: error, exitCode, signal: exitSignal, timedOut, cleanup, outputExceeded,
    outputBytes, outputSha256: outputHash.digest("hex"), stdoutBytes, stdoutSha256: stdoutHash.digest("hex"), stderrBytes, stderrSha256: stderrHash.digest("hex") };
  const trace = await write(join(job.outputRoot, "native-trace.ndjson"), traceRaw);
  const nativeRaw = native ? Buffer.from(`${JSON.stringify(native, null, 2)}\n`) : null;
  if (nativeRaw && nativeRaw.length > job.maxResultBytes) error ??= "native-result-bound";
  const nativePin = nativeRaw && nativeRaw.length <= job.maxResultBytes ? await write(join(job.outputRoot, "native-result.json"), nativeRaw) : null;
  let result: any = null, filesUnchanged = false, helperMarkers: number | null = null;
  try {
    if (pending.length || !native || native.status !== "SUCCESS" || !Number.isFinite(native.num_turns) || native.num_turns < 0 || exitCode !== 0 || !cleanup || error || userInputAcknowledgements !== 1) throw new Error("native-review-incomplete-or-failed");
    if (!nativePin || (await lstat(nativePin.path)).size > job.maxResultBytes) throw new Error("native-result-bound");
    result = native.structured_output ?? native.response; if (typeof result === "string") result = JSON.parse(result);
    validateAgyReviewResult(result, job.maxSources);
  } catch (caught: any) { error ??= /^(native-)/.test(caught?.message ?? "") ? caught.message : "native-review-result-invalid"; result = null; }
  try {
    await bytes(job.packet, job.maxPacketBytes); await bytes(job.denyPolicy.hook, 65536); await bytes(job.denyPolicy.helper, 65536);
    filesUnchanged = true; helperMarkers = await agyHelperMarkers(job.denyPolicy.spoolPath);
  } catch { error ??= "native-review-input-or-policy-changed"; }
  const receipt: AgyStreamReceipt = { schema: "gptgrep.opt-agy-stream-receipt.v1", job: jobPin,
    candidateId: job.candidateId, parentRevision: job.parentRevision, candidateRevision: job.candidateRevision, diffSha256: job.diffSha256,
    status: !cleanup ? "unknown" : error ? "failed" : "review_reported", error, packet: job.packet, preparedPrompt, inputFrame, trace, native: nativePin,
    inputBytes: frame.length, submittedUserEvents, userInputAcknowledgements, sessionId: init?.conversationId ?? null,
    nativeNumTurns: typeof native?.num_turns === "number" && Number.isFinite(native.num_turns) ? native.num_turns : null, usage: native?.usage ?? null,
    result, process: processObservation, lifecycle, policy: { filesUnchanged, helperMarkers, terminalCalls }, requestedModel: job.model, requestedEffort: job.effort,
    observedModel: null, observedEffort: null, providerRequestCount: null, currencyCost: null, originalDeadlineAt: job.deadlineAt,
    acceptance: "pending_parent_adjudication", descendantQuiescence: "not_established", releaseAccepted: false };
  return write(join(job.outputRoot, "receipt.json"), Buffer.from(`${JSON.stringify(receipt, null, 2)}\n`));
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  if (args.length !== 4 || args[0] !== "--job" || args[2] !== "--expect-sha256") throw new Error("Exact immutable AGY stream job required");
  const abort = new AbortController(); process.once("SIGTERM", () => abort.abort()); process.once("SIGINT", () => abort.abort());
  const receipt = await runAgyStream({ path: args[1]!, sha256: args[3]! }, { signal: abort.signal });
  console.log(JSON.stringify({ receipt }));
  const value = JSON.parse(await readFile(receipt.path, "utf8"));
  process.exitCode = value.status === "review_reported" ? 0 : 1;
}
