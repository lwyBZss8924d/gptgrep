import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, open, realpath, stat, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { physicalPath, requireStorage, within, writeJson } from "./io";

export type NativeStatus = "completed" | "failed" | "timeout" | "setup_failed";
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra";
export interface NativeRunOptions {
  codexBinary: string;
  cwd: string;
  model: string;
  effort: ReasoningEffort;
  serviceTier: string;
  timeoutMs: number;
  tracePath: string;
  /** Exact task and runtime grants. Never include a parent holding other tasks or judges. */
  readableRoots: string[];
  permissionProfile: string;
  /** Tool egress is disabled unless this exact model-provider domain set is declared. */
  allowedToolNetworkHosts?: string[];
  /** Existing, declared common font roots for the PDF tools; no font acquisition. */
  fontDirectories?: string[];
  input: string;
  outputSchema?: Record<string, unknown>;
  /** Only common model/runtime settings; cannot replace instructions, tools, hooks or permissions. */
  config?: Record<string, unknown>;
}
export interface NativeUsage {
  totalTokens: number;
  inputTokens: number;
  cachedInputTokens: number;
  cacheWriteInputTokens: number | null;
  outputTokens: number;
  reasoningOutputTokens: number;
}
export interface NativeActivity {
  id: string;
  type: string;
  status: string | null;
  /** Private command provenance only. Command output is never retained here. */
  command?: string;
  redactedCommandSha256?: string;
  commandTruncated?: boolean;
  exitCode?: number | null;
  durationMs?: number | null;
  outputBytes?: number | null;
}
export interface NativeRunResult {
  schema: "gptgrep.native-run.v1";
  executionMode: "live" | "test";
  status: NativeStatus;
  answerText: string;
  threadId: string | null;
  sessionId: string | null;
  turnId: string | null;
  requestedModel: string;
  /** Thread/start readback, not a claim about the model served by the backend. */
  observedModel: string | null;
  servedModel: string | null;
  modelBoundaryValid: boolean;
  workerModels: Array<{ threadId: string; requestedModel: string | null; observedModel: string | null }>;
  activities: NativeActivity[];
  reroutes: Array<{ fromModel: string; toModel: string; reason: string | null }>;
  usage: NativeUsage | null;
  usageScope: "root_thread_only";
  costUsd: number | null;
  /** Not exposed by this protocol: null must never be reported as zero. */
  requestCount: number | null;
  turnRequests: number;
  latencyMs: number;
  validStart: boolean;
  instructionSources: string[];
  tracePath: string;
  traceTruncated: boolean;
  error?: string;
  authFailure?: "AUTH_UNAUTHORIZED" | "AUTH_NOT_LOGGED_IN" | "AUTH_REFRESH_REJECTED";
  cleanup: { completed: boolean; groupTerminated: boolean };
}

type Message = Record<string, any>;
export interface NativeTestTransport {
  send(message: Message): void;
  onMessage(callback: (message: Message) => void): void;
  onClose(callback: (error?: Error) => void): void;
  close(): Promise<{ completed: boolean; groupTerminated: boolean }>;
}
const SAFE_CONFIG_KEYS = new Set([
  "model_reasoning_effort", "model_reasoning_summary", "model_verbosity", "service_tier",
  "agents.max_threads", "agents.max_concurrent_threads_per_session", "agents.max_depth",
  "agents.default_subagent_model", "agents.default_subagent_reasoning_effort",
]);
const sha = (text: string) => createHash("sha256").update(text).digest("hex");
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const sleep = (ms: number) => new Promise<void>(done => setTimeout(done, ms));

function checkOptions(options: NativeRunOptions): void {
  if (!options.model || !options.input || !Number.isSafeInteger(options.timeoutMs) || options.timeoutMs < 1) throw new Error("Model, input and a positive bounded deadline are required");
  if (!options.permissionProfile || options.permissionProfile.startsWith(":")) throw new Error("A named owner permission profile is required");
  if (!isAbsolute(options.cwd) || !isAbsolute(options.tracePath) || !options.readableRoots.length || options.readableRoots.some(root => !isAbsolute(root) || root === "/")) throw new Error("Exact absolute workspace, trace and readable roots are required");
  if (!options.readableRoots.some(root => within(root, options.cwd))) throw new Error("Task cwd must be an explicit readable root");
  for (const path of options.fontDirectories ?? []) if (!isAbsolute(path) || !options.readableRoots.some(root => within(root, path))) throw new Error("Font directories must be inside declared runtime read roots");
  for (const key of Object.keys(options.config ?? {})) if (!SAFE_CONFIG_KEYS.has(key)) throw new Error(`Native config override is not admitted: ${key}`);
}

/** Validate the effective owner profile; legacy sandbox overrides would discard auth denies. */
export function validatePermissionProfile(config: Message, options: NativeRunOptions): void {
  const profile = config.permissions?.[options.permissionProfile];
  if (!profile || ![":read-only", ":workspace"].includes(profile.extends)) throw new Error("Owner profile must have a supported base");
  const hosts = options.allowedToolNetworkHosts ?? [];
  if (hosts.length) {
    if (hosts.length !== 1 || hosts[0] !== "openrouter.ai") throw new Error("Only the frozen Decisions provider tool domain is admitted");
    const network = profile.network;
    if (config.features?.network_proxy !== true || config.web_search !== "disabled" ||
        network?.enabled !== true || network.mode !== "full" || network.proxy_url !== "http://127.0.0.1:3128" ||
        JSON.stringify(network.domains) !== JSON.stringify({ "openrouter.ai": "allow" }) ||
        ["enable_socks5", "enable_socks5_udp", "allow_upstream_proxy", "allow_local_binding", "dangerously_allow_non_loopback_proxy", "dangerously_allow_all_unix_sockets"].some(key => network[key] !== false) ||
        !network.unix_sockets || Object.keys(network.unix_sockets).length) throw new Error("Owner profile lacks the exact managed Decisions network boundary");
  } else if (profile.network?.enabled !== false) throw new Error("Tool network must be disabled for this run");
  const fs = profile.filesystem;
  if (!fs || fs["/"] !== "deny") throw new Error("Owner profile must deny / and explicitly grant the selected workspace/runtime roots");
  for (const [path, access] of Object.entries(fs)) {
    if (path === "glob_scan_max_depth") {
      if (access !== null) throw new Error("Unexpected filesystem glob_scan_max_depth metadata in owner permission profile");
      continue;
    }
    if (access === "deny") continue;
    if (path === ":minimal" && access === "read") continue;
    if (!["read", "write"].includes(String(access)) || !isAbsolute(path) || /[*?\[\]{}]/.test(path)) throw new Error("Unbounded filesystem grant in owner permission profile");
    if (!options.readableRoots.some(root => within(root, path))) throw new Error(`Owner profile grants an undeclared readable path: ${path}`);
    if (access === "write" && !within(options.cwd, path)) throw new Error("Only the exact task workspace may be writable");
  }
  for (const root of options.readableRoots) {
    const grants = Object.entries(fs).filter(([path]) => isAbsolute(path) && within(path, root)).sort(([a], [b]) => b.length - a.length);
    if (!grants.length || !["read", "write"].includes(String(grants[0][1]))) throw new Error(`Readable root lacks a positive owner grant: ${root}`);
  }
}

async function verifyLiveEnvironment(options: NativeRunOptions): Promise<void> {
  checkOptions(options);
  await requireStorage(options.tracePath, options.cwd);
  for (const name of ["GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID", "GPTGREP_AUTH_FAILURE_RECORD", "CODEX_HOME", "CODEX_SQLITE_HOME"]) {
    if (!process.env[name]) throw new Error(`Auth supervisor environment is required: ${name}`);
  }
  for (const name of ["CODEX_REFRESH_TOKEN_URL_OVERRIDE", "CODEX_REVOKE_TOKEN_URL_OVERRIDE"]) {
    const endpoint = new URL(process.env[name] ?? "");
    if (endpoint.protocol !== "http:" || endpoint.hostname !== "127.0.0.1" || endpoint.port !== "0") throw new Error("Consumer token refresh/revoke must remain blocked by the auth supervisor");
  }
  if (process.env.CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED !== "1") throw new Error("Auth supervisor remote-control isolation is missing");
  for (const name of ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN"]) if (process.env[name]) throw new Error(`Forbidden credential override is present: ${name}`);
  if (!(process.env.NO_PROXY ?? "").split(",").includes("127.0.0.1")) throw new Error("Auth supervisor loopback proxy exclusion is missing");
  await physicalPath(options.cwd);
  for (const path of options.readableRoots) if (await realpath(path) !== resolve(path)) throw new Error("Readable roots must be physical paths");
  if (!isAbsolute(options.codexBinary) || !(await stat(options.codexBinary)).isFile()) throw new Error("An exact pinned Codex binary path is required");
}

class StdioTransport implements NativeTestTransport {
  private child: ChildProcessWithoutNullStreams;
  private messageCallback: (value: Message) => void = () => {};
  private closeCallback: (error?: Error) => void = () => {};
  private ended = false;
  private closeError: Error | undefined;
  constructor(options: NativeRunOptions) {
    const runtime = resolve(options.cwd, ".native-runtime");
    this.child = spawn(options.codexBinary, ["app-server", "--listen", "stdio://", "--strict-config"], {
      cwd: options.cwd, env: { ...process.env, TMPDIR: resolve(runtime, "tmp"), TMP: resolve(runtime, "tmp"), TEMP: resolve(runtime, "tmp"),
        XDG_CACHE_HOME: resolve(runtime, "cache"), ...(options.fontDirectories?.length ? { FONTCONFIG_FILE: resolve(runtime, "fonts.conf") } : {}) },
      detached: true, stdio: ["pipe", "pipe", "pipe"],
    });
    let pending = "";
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => {
      pending += chunk;
      if (pending.length > 32 * 1024 * 1024) { this.fail(new Error("App-server line exceeds bounded protocol size")); return; }
      while (pending.includes("\n")) {
        const newline = pending.indexOf("\n");
        const line = pending.slice(0, newline); pending = pending.slice(newline + 1);
        if (!line.trim()) continue;
        try { this.messageCallback(JSON.parse(line)); } catch { this.fail(new Error("Malformed app-server JSON message")); }
      }
    });
    // Never persist raw stderr: it may contain auth-provider diagnostics or secrets.
    this.child.stderr.resume();
    this.child.stdin.on("error", error => this.fail(error));
    this.child.on("error", error => this.fail(error));
    this.child.on("close", (code, signal) => this.fail(new Error(`App-server closed (${code ?? signal ?? "unknown"})`)));
  }
  private fail(error: Error) { this.ended = true; this.closeError = error; this.closeCallback(error); }
  send(message: Message) {
    if (this.ended) throw this.closeError ?? new Error("App-server is closed");
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }
  onMessage(callback: (message: Message) => void) { this.messageCallback = callback; }
  onClose(callback: (error?: Error) => void) { this.closeCallback = callback; if (this.ended) callback(this.closeError); }
  async close() {
    const pid = this.child.pid;
    this.child.stdin.end();
    if (!pid) return { completed: this.ended, groupTerminated: false };
    let signaled = false;
    const signal = (name: NodeJS.Signals) => {
      try { process.kill(-pid, name); signaled = true; } catch (error: any) { if (error.code !== "ESRCH") throw error; }
    };
    signal("SIGTERM");
    for (let i = 0; i < 20; i++) {
      try { process.kill(-pid, 0); } catch (error: any) { if (error.code === "ESRCH") return { completed: true, groupTerminated: signaled }; throw error; }
      await sleep(50);
    }
    signal("SIGKILL");
    for (let i = 0; i < 20; i++) {
      try { process.kill(-pid, 0); } catch (error: any) { if (error.code === "ESRCH") return { completed: true, groupTerminated: signaled }; throw error; }
      await sleep(50);
    }
    return { completed: false, groupTerminated: signaled };
  }
}

function usage(value: any): NativeUsage | null {
  if (!value || !["totalTokens", "inputTokens", "cachedInputTokens", "outputTokens", "reasoningOutputTokens"].every(key => Number.isSafeInteger(value[key]) && value[key] >= 0)) return null;
  return { totalTokens: value.totalTokens, inputTokens: value.inputTokens, cachedInputTokens: value.cachedInputTokens,
    cacheWriteInputTokens: Number.isSafeInteger(value.cacheWriteInputTokens) ? value.cacheWriteInputTokens : null,
    outputTokens: value.outputTokens, reasoningOutputTokens: value.reasoningOutputTokens };
}

function authClass(value: any): NativeRunResult["authFailure"] {
  const info = value?.codexErrorInfo;
  if (info?.httpConnectionFailed?.httpStatusCode === 401 || info?.responseStreamConnectionFailed?.httpStatusCode === 401) return "AUTH_UNAUTHORIZED";
  const message = String(value?.message ?? "").toLowerCase();
  if (/\bnot logged in\b/.test(message)) return "AUTH_NOT_LOGGED_IN";
  if (/\bunauthorized\b|\b401\b/.test(message)) return "AUTH_UNAUTHORIZED";
  if (/refresh token (?:has been |was )?(?:revoked|expired|reused|rejected)|refresh_token_(?:reused|expired|revoked)/.test(message)) return "AUTH_REFRESH_REJECTED";
  return undefined;
}

async function execute(options: NativeRunOptions, transport: NativeTestTransport, executionMode: "live" | "test", trace: (value: Message) => void): Promise<NativeRunResult> {
  const started = performance.now();
  const result: NativeRunResult = { schema: "gptgrep.native-run.v1", executionMode, status: "setup_failed", answerText: "", threadId: null, sessionId: null, turnId: null,
    requestedModel: options.model, observedModel: null, servedModel: null, modelBoundaryValid: true, workerModels: [], activities: [], reroutes: [], usage: null, usageScope: "root_thread_only", costUsd: null, requestCount: null,
    turnRequests: 0, latencyMs: 0, validStart: false, instructionSources: [], tracePath: options.tracePath, traceTruncated: false, cleanup: { completed: false, groupTerminated: false } };
  let nextId = 1;
  const pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void }>();
  const early: Message[] = [];
  const messages = new Map<string, { text: string; phase: string | null }>();
  let terminalResolve: () => void = () => {};
  let terminalReject: (error: Error) => void = () => {};
  const terminal = new Promise<void>((resolve, reject) => { terminalResolve = resolve; terminalReject = reject; });
  // A setup failure may occur before the turn promise is awaited.
  void terminal.catch(() => {});
  let terminalSeen = false;
  let timedOut = false;
  let closing = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const fail = (error: Error) => { for (const p of pending.values()) p.reject(error); pending.clear(); terminalReject(error); };
  const send = (message: Message) => transport.send(message);
  const rpc = (method: string, params: Message): Promise<any> => {
    const id = nextId++;
    trace({ direction: "request", id, method, ...(method === "turn/start" ? { inputSha256: sha(options.input) } : {}) });
    return new Promise((resolve, reject) => { pending.set(id, { resolve, reject }); try { send({ id, method, params }); } catch (error) { pending.delete(id); reject(error); } });
  };
  const item = (value: any) => {
    if (value?.type === "agentMessage" && typeof value.id === "string" && typeof value.text === "string") messages.set(value.id, { text: value.text, phase: value.phase ?? null });
    if (typeof value?.id === "string" && typeof value?.type === "string" && value.type !== "agentMessage" && value.type !== "reasoning" &&
        !result.activities.some(activity => activity.id === value.id)) {
      if (result.activities.length >= 10_000) throw new Error("Native activity count exceeds the bounded trace contract");
      const activity: NativeActivity = { id: value.id, type: value.type, status: typeof value.status === "string" ? value.status : null };
      if (value.type === "commandExecution") {
        let command = typeof value.command === "string" ? value.command : "";
        for (const name of ["OPENROUTER_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN"]) {
          const secret = process.env[name]; if (secret) command = command.replaceAll(secret, "[redacted]");
        }
        activity.redactedCommandSha256 = sha(command);
        activity.command = command.slice(0, 8192); activity.commandTruncated = command.length > 8192;
        activity.exitCode = Number.isSafeInteger(value.exitCode) ? value.exitCode : null;
        activity.durationMs = Number.isFinite(value.durationMs) && value.durationMs >= 0 ? value.durationMs : null;
        activity.outputBytes = typeof value.aggregatedOutput === "string" ? Buffer.byteLength(value.aggregatedOutput) : null;
      }
      result.activities.push(activity);
      trace({ direction: "activity", ...activity });
    }
    const expectedWorker = options.config?.["agents.default_subagent_model"];
    if (value?.type === "collabAgentToolCall" && value.tool === "spawnAgent") {
      for (const id of value.receiverThreadIds ?? []) {
        if (typeof id !== "string") continue;
        let worker = result.workerModels.find(worker => worker.threadId === id);
        if (!worker) { worker = { threadId: id, requestedModel: null, observedModel: null }; result.workerModels.push(worker); }
        worker.requestedModel = typeof value.model === "string" ? value.model : null;
        if (expectedWorker && worker.requestedModel && worker.requestedModel !== expectedWorker) result.modelBoundaryValid = false;
      }
    }
    if (value?.type === "subAgentActivity" && typeof value.agentThreadId === "string" && typeof value.model === "string") {
      let worker = result.workerModels.find(worker => worker.threadId === value.agentThreadId);
      if (!worker) { worker = { threadId: value.agentThreadId, requestedModel: null, observedModel: null }; result.workerModels.push(worker); }
      worker.observedModel = value.model;
      if (expectedWorker && value.model !== expectedWorker) result.modelBoundaryValid = false;
    }
  };
  const event = (message: Message) => {
    const p = message.params ?? {};
    if (!result.threadId || !result.turnId) { if (early.length >= 10_000) throw new Error("Too many pre-turn protocol events"); early.push(message); return; }
    if (p.threadId !== result.threadId || (p.turnId ?? p.turn?.id) !== result.turnId) return;
    trace({ direction: "event", method: message.method, threadId: result.threadId, turnId: result.turnId });
    if (message.method === "thread/tokenUsage/updated") result.usage = usage(p.tokenUsage?.total) ?? result.usage;
    if (message.method === "item/completed") item(p.item);
    if (message.method === "model/rerouted" && typeof p.fromModel === "string" && typeof p.toModel === "string") {
      result.reroutes.push({ fromModel: p.fromModel, toModel: p.toModel, reason: typeof p.reason === "string" ? p.reason : null });
      result.servedModel = p.toModel;
      if (p.toModel !== options.model) result.modelBoundaryValid = false;
    }
    if (message.method === "error") {
      result.authFailure ??= authClass(p.error);
      // Even a non-retryable error is not a terminal turn; usage can still arrive.
      trace({ direction: "error", willRetry: p.willRetry === true, messageSha256: sha(String(p.error?.message ?? "")) });
    }
    if (message.method === "turn/completed") {
      if (terminalSeen) return;
      for (const value of p.turn?.items ?? []) item(value);
      if (!["completed", "failed", "interrupted"].includes(p.turn?.status)) return;
      if (!timedOut) result.status = p.turn.status === "completed" ? "completed" : "failed";
      if (p.turn.error) { result.error = `Turn failed; message SHA256 ${sha(String(p.turn.error.message ?? ""))}`; result.authFailure ??= authClass(p.turn.error); }
      terminalSeen = true; terminalResolve();
    }
  };
  transport.onMessage(message => {
    try {
      if (message.id !== undefined && !message.method) {
        const p = pending.get(message.id); if (!p) return;
        pending.delete(message.id);
        trace({ direction: "response", id: message.id, ok: !message.error });
        if (message.error) { result.authFailure ??= authClass(message.error); p.reject(new Error(`RPC failed (${Number.isInteger(message.error.code) ? message.error.code : "unknown"}); message SHA256 ${sha(String(message.error.message ?? ""))}`)); } else p.resolve(message.result);
      } else if (message.id !== undefined && message.method) {
        // No approval can widen the frozen reader boundary.
        if (["item/commandExecution/requestApproval", "item/fileChange/requestApproval"].includes(message.method)) send({ id: message.id, result: { decision: "decline" } });
        else if (message.method === "item/permissions/requestApproval") send({ id: message.id, result: { permissions: {}, scope: "turn" } });
        else if (message.method === "item/tool/requestUserInput") send({ id: message.id, result: { answers: {} } });
        else send({ id: message.id, error: { code: -32601, message: "No client tool is registered" } });
        trace({ direction: "server_request", method: message.method, granted: false });
      } else if (message.method) event(message);
    } catch (error) { fail(error instanceof Error ? error : new Error(String(error))); }
  });
  transport.onClose(error => { if (!closing && !terminalSeen) fail(error ?? new Error("App-server closed before the terminal turn")); });
  timer = setTimeout(() => { timedOut = true; result.status = "timeout"; fail(new Error("Native run deadline exceeded")); }, options.timeoutMs);
  try {
    checkOptions(options);
    await rpc("initialize", { clientInfo: { name: "gptgrep_benchmark", title: "Document benchmark", version: "0.2.0-alpha.1" }, capabilities: { experimentalApi: true } });
    send({ method: "initialized", params: {} });
    let cursor: string | null = null;
    const requiredModels = new Set([options.model, ...(typeof options.config?.["agents.default_subagent_model"] === "string" ? [options.config["agents.default_subagent_model"] as string] : [])]);
    const models = new Map<string, any>();
    const cursors = new Set<string>();
    do {
      const response = await rpc("model/list", { includeHidden: true, limit: 100, ...(cursor ? { cursor } : {}) });
      for (const value of response.data ?? []) if (requiredModels.has(value.model)) models.set(value.model, value);
      cursor = response.nextCursor ?? null;
      if (cursor && cursors.has(cursor)) throw new Error("Repeated model catalog cursor");
      if (cursor) cursors.add(cursor);
    } while (cursor && models.size < requiredModels.size);
    for (const required of requiredModels) if (!models.has(required)) throw new Error(`Requested model is unavailable: ${required}`);
    const model = models.get(options.model);
    if (!model.supportedReasoningEfforts?.some((value: any) => value.reasoningEffort === options.effort)) throw new Error("Requested reasoning effort is unavailable for the exact model");
    const tiers = model.serviceTiers?.map((value: any) => value.id) ?? [];
    if (options.serviceTier !== "default" && !tiers.includes(options.serviceTier) && !model.additionalSpeedTiers?.includes(options.serviceTier)) throw new Error("Requested service tier is unavailable for the exact model");
    const effective = await rpc("config/read", { cwd: options.cwd, includeLayers: false });
    validatePermissionProfile(effective.config ?? {}, options);
    const permissions = await rpc("permissionProfile/list", { cwd: options.cwd, limit: 100 });
    if (!permissions.data?.some((value: any) => value.id === options.permissionProfile && value.allowed === true)) throw new Error("Exact owner permission profile is unavailable or disallowed");
    const thread = await rpc("thread/start", { model: options.model, allowProviderModelFallback: false, serviceTier: options.serviceTier,
      cwd: options.cwd, permissions: options.permissionProfile, approvalPolicy: "never", ephemeral: true,
      config: { ...options.config, web_search: "disabled" } });
    result.threadId = typeof thread.thread?.id === "string" ? thread.thread.id : null;
    result.sessionId = typeof thread.thread?.sessionId === "string" ? thread.thread.sessionId : null;
    result.observedModel = typeof thread.model === "string" ? thread.model : null;
    result.instructionSources = Array.isArray(thread.instructionSources) ? thread.instructionSources.filter((v: unknown) => typeof v === "string") : [];
    if (!result.threadId || result.observedModel !== options.model || thread.activePermissionProfile?.id !== options.permissionProfile || thread.cwd !== options.cwd) throw new Error("Thread identity, model, cwd or permission readback differs from the frozen request");
    result.turnRequests++;
    const turn = await rpc("turn/start", { threadId: result.threadId, input: [{ type: "text", text: options.input, text_elements: [] }],
      model: options.model, effort: options.effort, serviceTier: options.serviceTier, ...(options.outputSchema ? { outputSchema: options.outputSchema } : {}) });
    result.turnId = typeof turn.turn?.id === "string" ? turn.turn.id : null;
    if (!result.turnId) throw new Error("Turn/start did not return a turn ID");
    result.validStart = true;
    for (const message of early.splice(0)) event(message);
    if (turn.turn?.status !== "inProgress") event({ method: "turn/completed", params: { threadId: result.threadId, turn: turn.turn } });
    await terminal;
    if (timer) clearTimeout(timer);
    // Drain notifications already queued behind turn/completed, including final usage.
    await sleep(25);
  } catch (error) {
    if (result.status !== "timeout") result.status = result.validStart ? "failed" : "setup_failed";
    result.error = errorText(error);
    if (result.threadId && result.turnId && !terminalSeen) {
      try { send({ id: nextId++, method: "turn/interrupt", params: { threadId: result.threadId, turnId: result.turnId } }); } catch { /* process may already be gone */ }
      await sleep(25);
    }
  } finally {
    if (timer) clearTimeout(timer);
    closing = true;
    try { result.cleanup = await transport.close(); } catch { result.cleanup = { completed: false, groupTerminated: false }; }
    if (!result.cleanup.completed) { result.status = result.validStart ? "failed" : "setup_failed"; result.error = "App-server process-group cleanup could not be confirmed"; }
    if (!result.modelBoundaryValid && result.status === "completed") { result.status = "failed"; result.error = "Observed model routing differs from the frozen model boundary"; }
    // Authentication belongs to the supervisor lifecycle even if a retry produced text.
    if (result.authFailure && result.status === "completed") { result.status = "failed"; result.error = "Authentication failure requires supervisor recovery"; }
    const final = [...messages.values()].filter(value => value.phase === "final_answer");
    const unphased = [...messages.values()].filter(value => value.phase === null);
    result.answerText = (final.length ? final : unphased).map(value => value.text).join("\n");
    result.latencyMs = Math.round(performance.now() - started);
    trace({ direction: "result", status: result.status, threadId: result.threadId, sessionId: result.sessionId, turnId: result.turnId,
      requestedModel: result.requestedModel, observedModel: result.observedModel, servedModel: result.servedModel, usage: result.usage, cleanup: result.cleanup });
  }
  return result;
}

/** In-memory protocol seam. Always returns executionMode=test and cannot launch a process. */
export function runNativeForTest(options: NativeRunOptions, transport: NativeTestTransport): Promise<NativeRunResult> {
  return execute(options, transport, "test", () => {});
}

/** Live entrypoint: owner storage and auth-supervisor admission are mandatory. */
export async function runNative(options: NativeRunOptions): Promise<NativeRunResult> {
  await verifyLiveEnvironment(options);
  // Native tool writes remain in the task's sole writable root, not the auth owner's home.
  const runtime = resolve(options.cwd, ".native-runtime");
  await physicalPath(runtime);
  await mkdir(resolve(runtime, "tmp"), { recursive: true });
  await mkdir(resolve(runtime, "cache"), { recursive: true });
  if (options.fontDirectories?.length) {
    const xml = (value: string) => value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
    await writeFile(resolve(runtime, "fonts.conf"), `<?xml version="1.0"?>\n<!DOCTYPE fontconfig SYSTEM "fonts.dtd">\n<fontconfig>${options.fontDirectories.map(path => `<dir>${xml(path)}</dir>`).join("")}<cachedir>${xml(resolve(runtime, "cache"))}</cachedir></fontconfig>\n`, { flag: "wx", mode: 0o600 });
  }
  await mkdir(dirname(options.tracePath), { recursive: true });
  const file = await open(options.tracePath, "wx", 0o600);
  let writes = Promise.resolve();
  let bytes = 0;
  let traceTruncated = false;
  try {
    const result = await execute(options, new StdioTransport(options), "live", value => {
      const line = `${JSON.stringify({ at: new Date().toISOString(), ...value })}\n`;
      bytes += Buffer.byteLength(line);
      if (bytes > 16 * 1024 * 1024) { traceTruncated = true; return; }
      writes = writes.then(async () => { await file.write(line); });
    });
    await writes;
    result.traceTruncated = traceTruncated;
    if (traceTruncated) { result.status = result.validStart ? "failed" : "setup_failed"; result.error = "Native metadata trace exceeds its 16 MiB bound"; }
    if (result.authFailure) await writeJson(process.env.GPTGREP_AUTH_FAILURE_RECORD!, { schema: "gptgrep.codex-auth.auth-failure.v1", class: result.authFailure });
    return result;
  } finally { await writes; await file.close(); }
}
