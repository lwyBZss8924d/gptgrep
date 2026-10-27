import { lstat, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { existsSync, lstatSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { canonical, fileHash, physicalPath, requireStorage, sha256, within } from "./io";
import { verifyPin, type Pin } from "./availability";
import type { NativeRunResult } from "./native";

export const ATIF_SCHEMA_VERSION = "ATIF-v1.8";
export const ATIF_SOURCE_REVISION = "6099b8cf2bfbbb3e7ff1eb2163bfc4524dbd9ba5";
export const ATIF_SPEC_SHA256 = "ab2dd5602187ac8271adba13bb9c89dd70961878faa1ab26324d98116bf37b34";
export const CAPTURE_VERSION = "gptgrep.native-capture.v1";
type Json = Record<string, any>;

/** Supported notification payloads only. Configuration and authentication RPCs never enter capture. */
export function captureNotification(message: Json, secrets: string[] = []): Json | null {
  const method = message.method;
  if (typeof method !== "string" || !/^(item\/|turn\/|thread\/tokenUsage\/|model\/rerouted$|error$)/.test(method) || /reasoning/i.test(method)) return null;
  const redaction = { fields: 0, strings: 0, omittedReasoningItems: 0 };
  const scrub = (value: any, key = ""): any => {
    if (/^(authorization|api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret|credentials?|encrypted_content|reasoning|reasoning_content)$/i.test(key)) { redaction.fields++; return "[redacted]"; }
    if (typeof value === "string") {
      const original = value;
      for (const secret of secrets) if (secret.length >= 4) value = value.replaceAll(secret, "[redacted]");
      value = value.replace(/(Bearer\s+)[A-Za-z0-9._~+\/-]+/gi, "$1[redacted]")
        .replace(/((?:[A-Za-z][A-Za-z0-9_]*_)?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret|credentials?)[\\"']*\s*[=:]\s*[\\"']*)[^\\\s"',}]+/gi, "$1[redacted]");
      if (value !== original) redaction.strings++;
      return value;
    }
    if (Array.isArray(value)) return value.filter(item => { if (item?.type === "reasoning") { redaction.omittedReasoningItems++; return false; } return true; }).map(item => scrub(item));
    if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, scrub(v, k)]));
    return value;
  };
  if (message.params?.item?.type === "reasoning") return null;
  const params = scrub(message.params ?? {});
  return { schema: CAPTURE_VERSION, method, params, redaction };
}

export interface CaptureRow extends Json { at?: string; direction: string }
export interface MediaBinding { sourcePath: string; artifact: Pin; mediaType: "image/jpeg" | "image/png" | "image/gif" | "image/webp"; sourceCallId?: string; sourceThreadId?: string }

function imageEncoding(content: Buffer): [MediaBinding["mediaType"], string] | null {
  return content.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])) ? ["image/png", "png"]
    : content[0] === 255 && content[1] === 216 && content[2] === 255 ? ["image/jpeg", "jpg"]
    : /^(GIF87a|GIF89a)$/.test(content.subarray(0, 6).toString("ascii")) ? ["image/gif", "gif"]
    : content.subarray(0, 4).toString("ascii") === "RIFF" && content.subarray(8, 12).toString("ascii") === "WEBP" ? ["image/webp", "webp"] : null;
}

/** Best-effort immediate local snapshots at supported event receipt; never fetch remote media. */
export async function createNativeMediaCapture(options: { roots: string[]; outputDir: string; maxBytes: number }) {
  await requireStorage(options.outputDir);
  if (!Number.isSafeInteger(options.maxBytes) || options.maxBytes < 1 || options.roots.some(root => !isAbsolute(root) || root === "/")) throw new Error("Exact media roots and a finite byte bound are required");
  await mkdir(options.outputDir, { recursive: true });
  const bindings: MediaBinding[] = [], gaps: string[] = [], calls = new Map<string, string>();
  let bytes = 0, metadataBytes = 2;
  const capture = (notification: Json, origin: "live_notification" | "thread_read_snapshot" = "live_notification") => {
    const p = notification.params ?? {};
    if (!["item/completed", "turn/completed"].includes(notification.method)) return;
    for (const item of [p.item, ...(p.turn?.items ?? [])]) {
      if (item?.type !== "imageView" || typeof item.id !== "string" || typeof p.threadId !== "string") continue;
      const sourcePath = item.path ?? item.imagePath, key = `${p.threadId}:${item.id}`;
      if (calls.has(key)) continue;
      if (origin !== "live_notification" || notification.method !== "item/completed") { gaps.push(`historical_media_unverified:${key}`); continue; }
      try {
        if (typeof sourcePath !== "string" || !isAbsolute(sourcePath) || !options.roots.some(root => within(root, sourcePath)) || realpathSync(sourcePath) !== resolve(sourcePath)) throw new Error("media path is outside its physical roots");
        const info = lstatSync(sourcePath);
        if (!info.isFile() || info.isSymbolicLink() || info.size < 1 || info.size > 64 * 1024 * 1024) throw new Error("media file exceeds its bound");
        const content = readFileSync(sourcePath), encoding = imageEncoding(content), hash = sha256(content);
        if (content.length !== info.size || !encoding) throw new Error("media changed or has an unsupported encoding");
        const path = join(options.outputDir, `${hash}.${encoding[1]}`);
        const binding: MediaBinding = { sourcePath, artifact: { path, sha256: hash }, mediaType: encoding[0], sourceCallId: item.id, sourceThreadId: p.threadId };
        const bindingBytes = Buffer.byteLength(JSON.stringify(binding)) + 1;
        if (bindings.length >= 4096 || metadataBytes + bindingBytes > 1024 * 1024) throw new Error("media call-map retention bound exhausted");
        if (!existsSync(path)) {
          if (bytes + content.length > options.maxBytes) throw new Error("aggregate media retention bound exhausted");
          writeFileSync(path, content, { flag: "wx", mode: 0o600 }); bytes += content.length;
        }
        calls.set(key, hash);
        bindings.push(binding); metadataBytes += bindingBytes;
      } catch (error) { gaps.push(`event_media_unavailable:${key}:${error instanceof Error ? error.message : "unknown"}`); }
    }
  };
  return { capture, snapshot: () => ({ bindings, gaps: [...new Set(gaps)], bytes, timing: "supported_event_received" as const }) };
}

/** Copies only referenced, local image bytes. Missing/unsupported media remains an explicit gap. */
export async function collectNativeMedia(options: { trace: Pin; roots: string[]; outputDir: string; maxBytes: number }) {
  await verifyPin(options.trace); await requireStorage(options.outputDir);
  if (!Number.isSafeInteger(options.maxBytes) || options.maxBytes < 1 || options.roots.some(root => !isAbsolute(root) || root === "/")) throw new Error("Exact media roots and a finite byte bound are required");
  const raw = await readFile(options.trace.path);
  if (raw.length > 64 * 1024 * 1024) throw new Error("Media inventory capture exceeds its bound");
  const paths = new Set<string>();
  for (const line of raw.toString("utf8").split("\n").filter(Boolean)) {
    const row = JSON.parse(line);
    if (row.direction !== "capture" || row.capture?.schema !== CAPTURE_VERSION) continue;
    const p = row.capture.params;
    for (const item of [p.item, ...(p.turn?.items ?? [])]) if (item?.type === "imageView" && typeof (item.path ?? item.imagePath) === "string") paths.add(item.path ?? item.imagePath);
  }
  const bindings: MediaBinding[] = [], gaps: string[] = [];
  let bytes = 0;
  await mkdir(options.outputDir, { recursive: true });
  for (const sourcePath of paths) {
    try {
      if (!isAbsolute(sourcePath) || !options.roots.some(root => within(root, sourcePath)) || await realpath(sourcePath) !== resolve(sourcePath)) throw new Error("media path is outside its physical roots");
      const info = await lstat(sourcePath);
      if (!info.isFile() || info.isSymbolicLink() || info.size < 1 || info.size > 64 * 1024 * 1024 || bytes + info.size > options.maxBytes) throw new Error("media size exceeds the frozen bound");
      const content = await readFile(sourcePath);
      if (content.length !== info.size) throw new Error("media changed while being collected");
      const type = imageEncoding(content);
      if (!type) throw new Error("unsupported image encoding");
      const hash = sha256(content), path = join(options.outputDir, `${hash}.${type[1]}`);
      await physicalPath(path);
      if (!bindings.some(binding => binding.artifact.path === path)) { await writeFile(path, content, { flag: "wx", mode: 0o600 }); bytes += content.length; }
      bindings.push({ sourcePath, artifact: { path, sha256: hash }, mediaType: type[0] as MediaBinding["mediaType"] });
    } catch (error) { gaps.push(`media_collection_failed:${sourcePath}:${error instanceof Error ? error.message : "unknown"}`); }
  }
  return { bindings, gaps, bytes, timing: "post_terminal" as const };
}

/** Pure conversion seam. Missing child/media/result evidence stays an explicit coverage gap. */
export function convertNativeCapture(rows: CaptureRow[], result: NativeRunResult, media: MediaBinding[] = []) {
  if (!result.threadId || !result.turnId) throw new Error("Native trajectory requires observed thread and turn identities");
  const groups = new Map<string, { items: Map<string, Json>; terminalTurns: Set<string>; usage: any; turnId: string | null; turnIds: Set<string>; input: string | null }>();
  const group = (id: string) => {
    let value = groups.get(id);
    if (!value) { value = { items: new Map(), terminalTurns: new Set(), usage: null, turnId: null,
      turnIds: new Set(result.childSnapshots?.find(snapshot => snapshot.threadId === id)?.turnIds ?? []), input: null }; groups.set(id, value); }
    return value;
  };
  group(result.threadId).turnId = result.turnId;
  group(result.threadId).turnIds.add(result.turnId);
  const gaps: string[] = [], sourceRefs = new Map<string, number[]>();
  // Only explicit delegation from the selected root/known descendants establishes lineage.
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows) {
      if (row.direction !== "capture" || row.capture?.schema !== CAPTURE_VERSION) continue;
      const p = row.capture.params, source = groups.get(p.threadId), turn = p.turnId ?? p.turn?.id;
      if (!source || typeof turn !== "string" || source.turnIds.size && !source.turnIds.has(turn)) continue;
      source.turnId ??= turn;
      source.turnIds.add(turn);
      for (const item of [p.item, ...(p.turn?.items ?? [])]) {
        if (item?.type !== "collabAgentToolCall") continue;
        for (const child of item.receiverThreadIds ?? []) {
          if (typeof child !== "string" || child === result.threadId) continue;
          if (!groups.has(child)) { group(child); changed = true; }
          if (typeof item.prompt === "string" && !group(child).input) group(child).input = item.prompt;
        }
      }
    }
  }
  rows.forEach((row, index) => {
    if (row.direction !== "capture" || row.capture?.schema !== CAPTURE_VERSION) return;
    const { method, params: p } = row.capture;
    const thread = typeof p.threadId === "string" ? p.threadId : null;
    if (!thread) { gaps.push(`unattributed_event:${index + 1}`); return; }
    const target = groups.get(thread), turn = p.turnId ?? p.turn?.id;
    if (!target) { gaps.push(`foreign_thread:${thread}`); return; }
    if (typeof turn !== "string" || !target.turnIds.has(turn)) { gaps.push(`foreign_or_unbound_turn:${thread}:${String(turn)}`); return; }
    const add = (item: any, completed: boolean) => {
      if (!item || item.type === "reasoning" || typeof item.id !== "string") return;
      const itemKey = `${turn}:${item.id}`, old = target.items.get(itemKey);
      const finished = completed && !["inProgress", "in_progress", "interrupted", "cancelled"].includes(item.status);
      const payloadSha256 = sha256(canonical(item));
      if (old?.completed && finished && old.payloadSha256 !== payloadSha256) gaps.push(`conflicting_item:${thread}:${item.id}`);
      target.items.set(itemKey, { ...old, ...item, turnId: turn, payloadSha256, capturedAt: old?.capturedAt ?? row.at, completed: finished });
      const key = `${thread}:${turn}:${item.id}`; sourceRefs.set(key, [...(sourceRefs.get(key) ?? []), index + 1]);
    };
    if (method === "item/started" || method === "item/completed") add(p.item, method === "item/completed");
    if (method === "turn/completed") { if (["completed", "failed", "interrupted"].includes(p.turn?.status)) target.terminalTurns.add(turn); for (const item of p.turn?.items ?? []) add(item, true); }
    if (method === "thread/tokenUsage/updated") target.usage = p.tokenUsage?.total ?? null;
  });
  const mediaBySource = new Map(media.filter(m => !m.sourceCallId).map(m => [m.sourcePath, m]));
  const mediaByCall = new Map(media.filter(m => m.sourceCallId && m.sourceThreadId).map(m => [`${m.sourceThreadId}:${m.sourceCallId}`, m]));
  const hasMedia = (value: any): boolean => Array.isArray(value) ? value.some(hasMedia) : value && typeof value === "object"
    ? /^(?:image|audio)/i.test(String(value.type ?? "")) || /^(?:image|audio)\//i.test(String(value.mimeType ?? value.mime_type ?? "")) || Object.values(value).some(hasMedia)
    : typeof value === "string" && /^data:(?:image|audio)\//.test(value);
  const trajectories = new Map<string, Json>();
  for (const [thread, source] of groups) {
    const steps: Json[] = [];
    const rootInput = thread === result.threadId ? rows.find(r => r.direction === "input" && typeof r.text === "string") : null;
    const input = rootInput?.text ?? source.input;
    if (typeof input === "string") steps.push({ step_id: 1, source: "user", message: input, ...(rootInput?.at ? { timestamp: rootInput.at } : {}) });
    else if (![...source.items.values()].some(item => item.type === "userMessage")) gaps.push(`missing_input:${thread}`);
    for (const item of source.items.values()) {
      const references = sourceRefs.get(`${thread}:${item.turnId}:${item.id}`) ?? [];
      const step: Json = { step_id: steps.length + 1, source: "agent", message: "", extra: { native_item_id: item.id, native_turn_id: item.turnId, native_type: item.type, source_lines: references, completed: item.completed } };
      if (item.capturedAt) step.timestamp = item.capturedAt;
      if (item.type === "agentMessage") step.message = item.text ?? "";
      else if (item.type === "userMessage") {
        step.source = "user"; step.message = JSON.stringify(item.content ?? []);
        if (hasMedia(item.content)) gaps.push(`unsupported_user_media:${thread}:${item.id}`);
      }
      else {
        if (!["commandExecution", "mcpToolCall", "dynamicToolCall", "collabAgentToolCall", "imageView"].includes(item.type)) gaps.push(`unsupported_item:${thread}:${item.id}:${item.type}`);
        const functionName = item.type === "commandExecution" ? "commandExecution" : item.type === "collabAgentToolCall" ? `collab.${item.tool ?? "unknown"}` : item.type;
        const argumentsValue = item.type === "commandExecution" ? { command: item.command ?? null, cwd: item.cwd ?? null } : item.type === "collabAgentToolCall"
          ? { prompt: item.prompt ?? null, model: item.model ?? null, receiverThreadIds: item.receiverThreadIds ?? [], tool: item.tool ?? null } : item.arguments ?? item.input ?? {};
        step.tool_calls = [{ tool_call_id: item.id, function_name: functionName, arguments: typeof argumentsValue === "object" && argumentsValue !== null && !Array.isArray(argumentsValue) ? argumentsValue : { value: argumentsValue } }];
        const output = item.aggregatedOutput ?? item.result ?? item.output ?? item.content ?? null;
        if (item.type !== "imageView" && hasMedia(output)) gaps.push(`unsupported_nested_media:${thread}:${item.id}`);
        const observation: Json = { source_call_id: item.id, content: typeof output === "string" ? output : output === null ? null : JSON.stringify(output), extra: { native_status: item.status ?? null, exit_code: item.exitCode ?? null } };
        if (!item.completed) { observation.extra.disposition = "interrupted_or_incomplete"; gaps.push(`incomplete_tool:${thread}:${item.id}`); }
        if (item.type === "imageView") {
          const path = item.path ?? item.imagePath, bound = mediaByCall.get(`${thread}:${item.id}`) ?? mediaBySource.get(path);
          if (bound) observation.content = [{ type: "image", source: { media_type: bound.mediaType, path: bound.artifact.path } }];
          else { observation.extra.unresolved_media_path = path ?? null; gaps.push(`unresolved_media:${thread}:${item.id}`); }
        } else if (output === null && item.type !== "collabAgentToolCall") gaps.push(`missing_tool_output:${thread}:${item.id}`);
        if (item.type === "collabAgentToolCall") {
          const children = (item.receiverThreadIds ?? []).filter((id: unknown): id is string => typeof id === "string");
          observation.subagent_trajectory_ref = children.filter((id: string) => groups.get(id)?.items.size).map((id: string) => ({ trajectory_id: `codex-thread:${id}` }));
          const missing = children.filter((id: string) => !groups.get(id)?.items.size);
          if (missing.length) { observation.extra.unresolved_child_thread_ids = missing; gaps.push(...missing.map((id: string) => `missing_child_trajectory:${id}`)); }
          if (!observation.subagent_trajectory_ref.length) delete observation.subagent_trajectory_ref;
        }
        step.observation = { results: [observation] };
      }
      steps.push(step);
    }
    if (!source.turnIds.size || [...source.turnIds].some(turn => !source.terminalTurns.has(turn))) gaps.push(`missing_terminal:${thread}`);
    if (!steps.length) { gaps.push(`missing_items:${thread}`); continue; }
    const usage = source.usage ?? (thread === result.threadId ? result.usage : null);
    const model = thread === result.threadId ? result.observedModel : result.workerModels.find(w => w.threadId === thread)?.observedModel;
    if (!model) gaps.push(`model_unavailable:${thread}`);
    const trajectory: Json = { schema_version: ATIF_SCHEMA_VERSION, trajectory_id: `codex-thread:${thread}`,
      agent: { name: "Codex", version: "runtime-bound-externally", ...(model ? { model_name: model } : {}) }, steps,
      extra: { native_thread_id: thread, native_turn_id: source.turnId, native_turn_ids: [...source.turnIds], converter: "gptgrep.native-capture-to-atif.v1", schema_source_revision: ATIF_SOURCE_REVISION, schema_spec_sha256: ATIF_SPEC_SHA256,
        usage_scope: "this_thread_only", request_count: null, cost_usd: null, hidden_reasoning: "not_captured" } };
    if (thread === result.threadId && result.sessionId) trajectory.session_id = result.sessionId;
    if (usage) trajectory.final_metrics = { total_prompt_tokens: usage.inputTokens, total_completion_tokens: usage.outputTokens,
      total_cached_tokens: usage.cachedInputTokens, total_steps: steps.length, extra: { includes_subagents: false } };
    else gaps.push(`usage_unavailable:${thread}`);
    trajectories.set(thread, trajectory);
  }
  const root = trajectories.get(result.threadId);
  if (!root) throw new Error("Native capture has no convertible root steps");
  const children = [...trajectories.entries()].filter(([id]) => id !== result.threadId).map(([, value]) => value);
  if (children.length) root.subagent_trajectories = children;
  if (result.traceTruncated) gaps.push("capture_truncated");
  gaps.push(...result.captureGaps ?? []);
  return { trajectory: root, coverage: { schema: "gptgrep.trajectory-coverage.v1", executionMode: result.executionMode,
    sourceRows: rows.length, trajectories: trajectories.size, mediaBindings: media.length, gaps: [...new Set(gaps)],
    complete: gaps.length === 0, liveQualified: false, providerRequestCount: null, costUsd: null } };
}

export async function writeNativeTrajectory(options: { trace: Pin; result: NativeRunResult; media?: MediaBinding[]; outputPath: string; coveragePath: string; maxBytes?: number }) {
  await requireStorage(options.outputPath, options.coveragePath); await verifyPin(options.trace);
  if (options.result.tracePath !== options.trace.path) throw new Error("Capture/result trace identity mismatch");
  for (const item of options.media ?? []) { await requireStorage(item.artifact.path); await verifyPin(item.artifact); }
  const bytes = await readFile(options.trace.path);
  if (bytes.length > 64 * 1024 * 1024) throw new Error("Native capture exceeds the 64 MiB conversion bound");
  const maxBytes = options.maxBytes ?? 128 * 1024 * 1024;
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 128 * 1024 * 1024) throw new Error("ATIF output needs a finite declared bound at most 128 MiB");
  const rows = bytes.toString("utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
  const converted = convertNativeCapture(rows, options.result, options.media);
  const provenance = { trace: options.trace, converterSha256: await fileHash(import.meta.filename), sourceSha256: sha256(bytes),
    schemaSourceRevision: ATIF_SOURCE_REVISION, schemaSpecSha256: ATIF_SPEC_SHA256, runtimeBinarySha256: options.result.runtimeBinarySha256,
    requestedModel: options.result.requestedModel, requestedEffort: options.result.requestedEffort, requestedServiceTier: options.result.requestedServiceTier };
  for (const [path, value] of [[options.outputPath, converted.trajectory], [options.coveragePath, { ...converted.coverage, provenance }]] as const) {
    const bytes = `${JSON.stringify(value, null, 2)}\n`;
    if (Buffer.byteLength(bytes) > (path === options.outputPath ? maxBytes : 4 * 1024 * 1024)) throw new Error("Converted trajectory artifact exceeds its bound");
    await mkdir(dirname(path), { recursive: true }); await writeFile(path, bytes, { mode: 0o600, flag: "wx" });
  }
  return converted.coverage;
}
