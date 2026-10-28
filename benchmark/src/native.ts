import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, open, realpath, stat, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { digest, fileHash, physicalPath, requireStorage, within, writeJson, type StorageAdmission } from "./io";
import { captureNotification, createNativeMediaCapture, type MediaBinding } from "./trajectory";

export type NativeStatus = "completed" | "failed" | "timeout" | "setup_failed";
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra";
export interface NativeObservationLimits {
  toolCalls: number;
  childThreads: number;
  retryEvents: number;
}
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
  /** Exact read-only file grants, including frozen inputs inside the writable workspace. */
  requiredReadOnlyFiles?: string[];
  permissionProfile: string;
  /** Optional actor-owned cache/tmp directory inside cwd; readers retain their existing default. */
  runtimeDirectory?: string;
  /** Tool egress is disabled unless this exact model-provider domain set is declared. */
  allowedToolNetworkHosts?: string[];
  /** Live treatment reader only; the current owner command must verify this ready attestation. */
  decisionsHelper?: {
    socketPath: "/run/gptgrep-decisions/decisions.sock";
    scope: "linux-container-fs-netns-v1";
    /** Optional readback assertion; immutable jobs must not embed a future ready-attestation hash. */
    attestationSha256?: string;
  };
  /** Existing, declared common font roots for the PDF tools; no font acquisition. */
  fontDirectories?: string[];
  input: string;
  outputSchema?: Record<string, unknown>;
  /** Only common model/runtime settings; cannot replace instructions, tools, hooks or permissions. */
  config?: Record<string, unknown>;
  /** Notifications arrive after dispatch. These interrupt on observation, not before a tool/provider request. */
  observationLimits?: NativeObservationLimits;
  /** Opt-in retained owner-home sessions permit read-only full-turn capture of explicit children. */
  retainedChildCapture?: boolean;
  mediaCapture?: { outputDir: string; maxBytes: number };
  maxTraceBytes?: number;
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
export interface NativeHelperEvidence {
  attestationSha256: string;
  commandBindingSha256: string;
  namespacePlanSha256: string;
  clientControlSha256: string;
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
  requestedEffort: ReasoningEffort;
  requestedServiceTier: string;
  runtimeBinarySha256: string | null;
  /** Thread/start readback, not a claim about the model served by the backend. */
  observedModel: string | null;
  servedModel: string | null;
  modelBoundaryValid: boolean;
  workerModels: Array<{ threadId: string; requestedModel: string | null; observedModel: string | null }>;
  activities: NativeActivity[];
  reroutes: Array<{ fromModel: string; toModel: string; reason: string | null; threadId?: string }>;
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
  observations?: NativeObservationLimits & { enforcement: "post_notification_interrupt"; exceeded: string | null };
  captureGaps?: string[];
  childSnapshots?: Array<{ threadId: string; parentThreadId: string; sessionId: string; model: string | null; turnIds: string[] }>;
  mediaBindings?: MediaBinding[];
  traceLimitBytes?: number;
  permissionEvidence?: { profileId: string; extends: string; filesystemSha256: string; exactReadOnlyFiles: string[] };
  /** Actual late owner authority consumed by a live run; private result/trace metadata only. */
  decisionsHelperEvidence?: NativeHelperEvidence;
  resultOmission?: { serializedBytes: number; sha256: string; serialization: "JSON.stringify-utf8"; originalStatus: NativeStatus; reason: "retained_result_byte_bound"; omittedBytesRetained: false };
}

/** Keep oversized native output a started failure; the raw capture remains its source evidence. */
export function boundNativeResult(result: NativeRunResult): NativeRunResult {
  const serialized = JSON.stringify(result);
  if (Buffer.byteLength(serialized) <= 2 * 1024 * 1024) return result;
  const bounded: NativeRunResult = { ...result, status: result.validStart ? "failed" : "setup_failed", answerText: "",
    error: "Native result exceeds the retained result byte bound", resultOmission: { serializedBytes: Buffer.byteLength(serialized), sha256: sha(serialized), serialization: "JSON.stringify-utf8", originalStatus: result.status, reason: "retained_result_byte_bound", omittedBytesRetained: false },
    captureGaps: ["native_result_omitted_size"], activities: [], workerModels: [], reroutes: [], childSnapshots: [], instructionSources: [] };
  // Event-time capture independently bounds the media call map; preserve every retained mapping.
  return bounded;
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
const HELPER_PARENT = "/run/gptgrep-decisions";
const HELPER_SOCKET = `${HELPER_PARENT}/decisions.sock`;
const HELPER_CONTROL = `${HELPER_PARENT}/client.json`;
const HELPER_SCOPE = "linux-container-fs-netns-v1";
const HELPER_ENV = ["GPTGREP_DECISIONS_IPC_CONTROL", "GPTGREP_DECISIONS_IPC_SHA256"] as const;
const exactHash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const emptyMap = (value: unknown) => !!value && typeof value === "object" && !Array.isArray(value) && !Object.keys(value).length;

function checkOptions(options: NativeRunOptions): void {
  if (!options.model || !options.input || !Number.isSafeInteger(options.timeoutMs) || options.timeoutMs < 1) throw new Error("Model, input and a positive bounded deadline are required");
  if (!options.permissionProfile || options.permissionProfile.startsWith(":")) throw new Error("A named owner permission profile is required");
  if (!isAbsolute(options.cwd) || !isAbsolute(options.tracePath) || !options.readableRoots.length || options.readableRoots.some(root => !isAbsolute(root) || root === "/")) throw new Error("Exact absolute workspace, trace and readable roots are required");
  if (!options.readableRoots.some(root => within(root, options.cwd))) throw new Error("Task cwd must be an explicit readable root");
  if (options.decisionsHelper && (Object.keys(options.decisionsHelper).some(key => !["attestationSha256", "scope", "socketPath"].includes(key)) ||
      options.decisionsHelper.socketPath !== HELPER_SOCKET || options.decisionsHelper.scope !== HELPER_SCOPE ||
      (options.decisionsHelper.attestationSha256 !== undefined && !exactHash(options.decisionsHelper.attestationSha256)) || options.allowedToolNetworkHosts?.length ||
      !options.readableRoots.includes(options.cwd) || !options.readableRoots.includes(HELPER_PARENT) ||
      within(options.cwd, HELPER_PARENT) || within(HELPER_PARENT, options.cwd) ||
      options.readableRoots.some(root => root !== HELPER_PARENT && within(root, HELPER_PARENT)))) throw new Error("Exact owner-qualified Unix helper expectation is required");
  if (options.runtimeDirectory !== undefined && (!isAbsolute(options.runtimeDirectory) || resolve(options.runtimeDirectory) !== options.runtimeDirectory ||
      options.runtimeDirectory === options.cwd || !within(options.cwd, options.runtimeDirectory))) throw new Error("Native runtime directory must be an exact child of the writable workspace");
  if (options.requiredReadOnlyFiles && (!options.requiredReadOnlyFiles.length || options.requiredReadOnlyFiles.length > 1024 ||
      new Set(options.requiredReadOnlyFiles).size !== options.requiredReadOnlyFiles.length || options.requiredReadOnlyFiles.some(path =>
        !isAbsolute(path) || resolve(path) !== path || path === options.cwd ||
        !within(options.cwd, path) && (!options.readableRoots.includes(path) ||
          options.readableRoots.some(root => root !== path && within(root, path)))))) throw new Error("Read-only files require exact grants and external sources require separate file roots");
  for (const path of options.fontDirectories ?? []) if (!isAbsolute(path) || !options.readableRoots.some(root => within(root, path))) throw new Error("Font directories must be inside declared runtime read roots");
  for (const key of Object.keys(options.config ?? {})) if (!SAFE_CONFIG_KEYS.has(key)) throw new Error(`Native config override is not admitted: ${key}`);
  if (options.observationLimits && (Object.keys(options.observationLimits).length !== 3 || Object.entries(options.observationLimits).some(([key, value]) => !["toolCalls", "childThreads", "retryEvents"].includes(key) || !Number.isSafeInteger(value) || value < 0 || value > 10_000))) throw new Error("Invalid finite native observation limits");
  if (options.retainedChildCapture && (!options.observationLimits || options.observationLimits.childThreads > 32)) throw new Error("Retained child capture requires an explicit child bound of at most 32");
  if (options.maxTraceBytes !== undefined && (!Number.isSafeInteger(options.maxTraceBytes) || options.maxTraceBytes < 1 || options.maxTraceBytes > 64 * 1024 * 1024)) throw new Error("Native trace limit must be at most 64 MiB");
}

/** Validate the effective owner profile; legacy sandbox overrides would discard auth denies. */
export function validatePermissionProfile(config: Message, options: NativeRunOptions): void {
  checkOptions(options);
  const profile = config.permissions?.[options.permissionProfile];
  if (!profile || ![":read-only", ":workspace"].includes(profile.extends)) throw new Error("Owner profile must have a supported base");
  const hosts = options.allowedToolNetworkHosts ?? [];
  if (options.decisionsHelper) {
    const network = profile.network;
    if (profile.extends !== ":read-only" || config.features?.network_proxy !== true || config.features?.use_legacy_landlock !== false ||
        config.approval_policy !== "never" || config.web_search !== "disabled" ||
        network?.enabled !== true || network.mode !== "full" || network.proxy_url !== "http://127.0.0.1:3128" ||
        network.dangerously_allow_all_unix_sockets !== true ||
        ["enable_socks5", "enable_socks5_udp", "allow_upstream_proxy", "allow_local_binding", "dangerously_allow_non_loopback_proxy"].some(key => network[key] !== false) ||
        !emptyMap(network.domains) || !emptyMap(network.unix_sockets) ||
        profile.filesystem?.[HELPER_PARENT] !== "read" || profile.filesystem?.[":minimal"] !== "read") throw new Error("Owner profile lacks the exact Linux helper filesystem and ProxyOnly boundary");
  } else if (hosts.length) {
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
  if (options.requiredReadOnlyFiles && (profile.extends !== ":read-only" || options.requiredReadOnlyFiles.some(path => fs[path] !== "read"))) throw new Error("Exact source file read-only grants are missing from the effective owner profile");
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

/** The storage owner verifies ready bytes and current generation; options cannot manufacture that authority. */
export function validateNativeHelperAdmission(options: NativeRunOptions, admission: StorageAdmission,
    environment: NodeJS.ProcessEnv = process.env): NativeHelperEvidence | undefined {
  checkOptions(options);
  for (const name of ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN", "OPENROUTER_API_KEY"]) {
    if (environment[name] !== undefined) throw new Error(`Forbidden credential override is present: ${name}`);
  }
  const expected = options.decisionsHelper, command = admission.container, helper = command?.decisionsHelper;
  if (!expected) {
    if (helper || HELPER_ENV.some(name => environment[name] !== undefined)) throw new Error("Undeclared Decisions helper authority is present");
    return;
  }
  if (!command || command.admissionClass !== "reader" || command.commandId !== "reader" || command.experimentArm !== "treatment" ||
      !helper || !exactHash(helper.attestation?.sha256) || !exactHash(admission.sha256) ||
      (expected.attestationSha256 !== undefined && helper.attestation.sha256 !== expected.attestationSha256) ||
      helper.allowedUnixSocket !== expected.socketPath || helper.unixSocketScope !== expected.scope ||
      helper.nativeProfileId !== options.permissionProfile || helper.nativeBinary.path !== options.codexBinary ||
      helper.nativeProfile.path !== command.modelProfile.path || helper.nativeProfile.sha256 !== command.modelProfile.sha256 ||
      helper.nativeProfile.path !== resolve(environment.CODEX_HOME ?? "/", "config.toml") ||
      environment[HELPER_ENV[0]] !== HELPER_CONTROL || helper.clientControl.path !== HELPER_CONTROL ||
      environment[HELPER_ENV[1]] !== helper.clientControl.sha256 || !exactHash(helper.namespacePlanSha256) ||
      !exactHash(helper.nativeBinary.sha256) || !exactHash(helper.clientControl.sha256) ||
      JSON.stringify([...command.modelReadRoots].sort()) !== JSON.stringify(options.readableRoots.filter(root => root !== options.cwd).sort())) throw new Error("Current reader command does not bind the selected Decisions helper");
  const boundary = helper.linuxRawUnixBoundary;
  if (!boundary || boundary.schema !== "environments-harness.linux-native-unix-boundary.v1" ||
      boundary.native_mode !== "ProxyRouted-ProxyOnly" || boundary.allow_all_unix_syscalls !== true ||
      boundary.filesystem_socket_authority !== "one-selected-external-helper-endpoint" || boundary.selected_socket !== expected.socketPath ||
      (["tool_namespace_observation", "filesystem_socket_inventory", "network_denial_evidence", "parent_api_proxy_separation"] as const).some(key => !exactHash(boundary[key]?.sha256))) throw new Error("Reader helper lacks the qualified Linux raw Unix boundary");
  return { attestationSha256: helper.attestation.sha256, commandBindingSha256: admission.sha256,
    namespacePlanSha256: helper.namespacePlanSha256, clientControlSha256: helper.clientControl.sha256 };
}

async function verifyLiveEnvironment(options: NativeRunOptions): Promise<NativeHelperEvidence | undefined> {
  checkOptions(options);
  const admission = await requireStorage(options.tracePath, options.cwd);
  const helperEvidence = validateNativeHelperAdmission(options, admission);
  if (options.decisionsHelper && (process.platform !== "linux" ||
      await fileHash(options.codexBinary) !== admission.container!.decisionsHelper!.nativeBinary.sha256)) throw new Error("Qualified Linux helper native binary differs");
  if (options.mediaCapture) {
    await requireStorage(options.mediaCapture.outputDir);
    if (options.readableRoots.some(root => within(root, options.mediaCapture!.outputDir))) throw new Error("Private media snapshots must remain outside model-readable roots");
  }
  for (const name of ["GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID", "GPTGREP_AUTH_FAILURE_RECORD", "CODEX_HOME", "CODEX_SQLITE_HOME"]) {
    if (!process.env[name]) throw new Error(`Auth supervisor environment is required: ${name}`);
  }
  for (const name of ["CODEX_REFRESH_TOKEN_URL_OVERRIDE", "CODEX_REVOKE_TOKEN_URL_OVERRIDE"]) {
    const endpoint = new URL(process.env[name] ?? "");
    if (endpoint.protocol !== "http:" || endpoint.hostname !== "127.0.0.1" || endpoint.port !== "0") throw new Error("Consumer token refresh/revoke must remain blocked by the auth supervisor");
  }
  if (process.env.CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED !== "1") throw new Error("Auth supervisor remote-control isolation is missing");
  if (!(process.env.NO_PROXY ?? "").split(",").includes("127.0.0.1")) throw new Error("Auth supervisor loopback proxy exclusion is missing");
  await physicalPath(options.cwd);
  for (const path of options.readableRoots) if (await realpath(path) !== resolve(path)) throw new Error("Readable roots must be physical paths");
  if (!isAbsolute(options.codexBinary) || !(await stat(options.codexBinary)).isFile()) throw new Error("An exact pinned Codex binary path is required");
  return helperEvidence;
}

class StdioTransport implements NativeTestTransport {
  private child: ChildProcessWithoutNullStreams;
  private messageCallback: (value: Message) => void = () => {};
  private closeCallback: (error?: Error) => void = () => {};
  private ended = false;
  private closeError: Error | undefined;
  constructor(options: NativeRunOptions) {
    const runtime = options.runtimeDirectory ?? resolve(options.cwd, ".native-runtime");
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
    requestedModel: options.model, requestedEffort: options.effort, requestedServiceTier: options.serviceTier, runtimeBinarySha256: null, observedModel: null, servedModel: null, modelBoundaryValid: true, workerModels: [], activities: [], reroutes: [], usage: null, usageScope: "root_thread_only", costUsd: null, requestCount: null,
    turnRequests: 0, latencyMs: 0, validStart: false, instructionSources: [], tracePath: options.tracePath, traceTruncated: false, cleanup: { completed: false, groupTerminated: false } };
  let nextId = 1;
  const pending = new Map<number, { method: string; resolve: (value: any) => void; reject: (error: Error) => void }>();
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
  let fatalError: Error | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const observedTools = new Set<string>(), observedChildren = new Set<string>(), childParents = new Map<string, string>();
  const deferredChildEvents: Message[] = [];
  let replayingChildren = false;
  let observedRetries = 0;
  if (options.observationLimits) result.observations = { toolCalls: 0, childThreads: 0, retryEvents: 0, enforcement: "post_notification_interrupt", exceeded: null };
  const fail = (error: Error) => { fatalError ??= error; for (const p of pending.values()) p.reject(error); pending.clear(); terminalReject(error); };
  const send = (message: Message) => transport.send(message);
  const captureSecrets = Object.entries(process.env).filter(([key, value]) => /(?:API_KEY|ACCESS_TOKEN|REFRESH_TOKEN|PASSWORD|SECRET|SERVICE_ACCOUNT_TOKEN)$/.test(key) && value).map(([, value]) => value!);
  const rpc = (method: string, params: Message): Promise<any> => {
    const id = nextId++;
    trace({ direction: "request", id, method, ...(method === "turn/start" ? { inputSha256: sha(options.input) } : {}) });
    if (method === "turn/start") trace({ direction: "input", text: captureNotification({ method: "turn/input", params: { text: options.input } }, captureSecrets)!.params.text });
    return new Promise((resolve, reject) => { pending.set(id, { method, resolve, reject }); try { send({ id, method, params }); } catch (error) { pending.delete(id); reject(error); } });
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
    if (typeof p.threadId === "string" && p.threadId !== result.threadId && !observedChildren.has(p.threadId)) {
      if (deferredChildEvents.length >= 10_000) throw new Error("Too many unbound child notifications");
      deferredChildEvents.push(message); return;
    }
    const rootEvent = p.threadId === result.threadId && (p.turnId ?? p.turn?.id) === result.turnId;
    if (rootEvent || observedChildren.has(p.threadId)) {
      for (const value of [p.item, ...(p.turn?.items ?? [])]) {
        if (["commandExecution", "mcpToolCall", "dynamicToolCall", "collabAgentToolCall", "imageView"].includes(value?.type) && typeof value.id === "string") observedTools.add(`${p.threadId}:${value.id}`);
        if (value?.type === "collabAgentToolCall" && value.tool === "spawnAgent") for (const child of value.receiverThreadIds ?? []) if (typeof child === "string" && child !== result.threadId) {
          observedChildren.add(child);
          if (childParents.has(child) && childParents.get(child) !== p.threadId) throw new Error("Conflicting native child parent identities");
          childParents.set(child, p.threadId);
        }
      }
      if (message.method === "error" && p.willRetry === true) observedRetries++;
      if (message.method === "model/rerouted" && typeof p.fromModel === "string" && typeof p.toModel === "string") {
        result.reroutes.push({ fromModel: p.fromModel, toModel: p.toModel, reason: typeof p.reason === "string" ? p.reason : null, threadId: p.threadId });
        if (rootEvent) result.servedModel = p.toModel;
        const requested = rootEvent ? options.model : options.config?.["agents.default_subagent_model"];
        if (!requested || p.toModel !== requested) result.modelBoundaryValid = false;
      }
      if (message.method === "error") {
        result.authFailure ??= authClass(p.error);
        trace({ direction: "error", threadId: p.threadId, willRetry: p.willRetry === true, messageSha256: sha(String(p.error?.message ?? "")) });
      }
      if (message.method === "turn/completed" && p.turn?.error) result.authFailure ??= authClass(p.turn.error);
      if (result.observations) {
        Object.assign(result.observations, { toolCalls: observedTools.size, childThreads: observedChildren.size, retryEvents: observedRetries });
        for (const name of ["toolCalls", "childThreads", "retryEvents"] as const) if (result.observations[name] > options.observationLimits![name]) {
          result.observations.exceeded = name; throw new Error(`Native observed ${name} limit exceeded; already-dispatched work remains charged`);
        }
      }
      if (!replayingChildren) {
        replayingChildren = true;
        try {
          while (true) {
            const index = deferredChildEvents.findIndex(value => observedChildren.has(value.params?.threadId));
            if (index < 0) break;
            const [deferred] = deferredChildEvents.splice(index, 1); event(deferred!);
          }
        } finally { replayingChildren = false; }
      }
    }
    if (p.threadId !== result.threadId || (p.turnId ?? p.turn?.id) !== result.turnId) return;
    trace({ direction: "event", method: message.method, threadId: result.threadId, turnId: result.turnId });
    if (message.method === "thread/tokenUsage/updated") result.usage = usage(p.tokenUsage?.total) ?? result.usage;
    if (message.method === "item/completed") item(p.item);
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
      if (message.id === undefined) {
        const capture = captureNotification(message, captureSecrets);
        if (capture) trace({ direction: "capture", capture });
      }
      if (message.id !== undefined && !message.method) {
        const p = pending.get(message.id); if (!p) return;
        // Acknowledged start is observed before capture I/O can fail. Keep p pending
        // until the fallible trace completes so fail() can always reject its waiter.
        if (!message.error && p.method === "turn/start" && typeof message.result?.turn?.id === "string") {
          result.turnId = message.result.turn.id; result.validStart = true;
        }
        trace({ direction: "response", id: message.id, ok: !message.error });
        pending.delete(message.id);
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
    const selectedProfile = effective.config.permissions[options.permissionProfile];
    result.permissionEvidence = { profileId: options.permissionProfile, extends: selectedProfile.extends,
      filesystemSha256: digest({ name: options.permissionProfile, extends: selectedProfile.extends, filesystem: selectedProfile.filesystem }),
      exactReadOnlyFiles: [...(options.requiredReadOnlyFiles ?? [])].sort() };
    const permissions = await rpc("permissionProfile/list", { cwd: options.cwd, limit: 100 });
    if (!permissions.data?.some((value: any) => value.id === options.permissionProfile && value.allowed === true)) throw new Error("Exact owner permission profile is unavailable or disallowed");
    const thread = await rpc("thread/start", { model: options.model, allowProviderModelFallback: false, serviceTier: options.serviceTier,
      cwd: options.cwd, permissions: options.permissionProfile, approvalPolicy: "never", ephemeral: !options.retainedChildCapture,
      config: { ...options.config, web_search: "disabled" } });
    result.threadId = typeof thread.thread?.id === "string" ? thread.thread.id : null;
    result.sessionId = typeof thread.thread?.sessionId === "string" ? thread.thread.sessionId : null;
    result.observedModel = typeof thread.model === "string" ? thread.model : null;
    result.instructionSources = Array.isArray(thread.instructionSources) ? thread.instructionSources.filter((v: unknown) => typeof v === "string") : [];
    if (!result.threadId || result.observedModel !== options.model || thread.activePermissionProfile?.id !== options.permissionProfile || thread.cwd !== options.cwd) throw new Error("Thread identity, model, cwd or permission readback differs from the frozen request");
    if (options.retainedChildCapture && thread.thread?.ephemeral !== false) throw new Error("Retained capture requires an acknowledged non-ephemeral owner session");
    result.turnRequests++;
    const turn = await rpc("turn/start", { threadId: result.threadId, input: [{ type: "text", text: options.input, text_elements: [] }],
      model: options.model, effort: options.effort, serviceTier: options.serviceTier, ...(options.outputSchema ? { outputSchema: options.outputSchema } : {}) });
    result.turnId = typeof turn.turn?.id === "string" ? turn.turn.id : null;
    if (!result.turnId) throw new Error("Turn/start did not return a turn ID");
    result.validStart = true;
    for (const message of early.splice(0)) event(message);
    if (turn.turn?.status !== "inProgress") event({ method: "turn/completed", params: { threadId: result.threadId, turn: turn.turn } });
    await terminal;
    if (options.retainedChildCapture) {
      result.captureGaps = []; result.childSnapshots = [];
      const queue = [result.threadId], seen = new Set<string>();
      for (let index = 0; index < queue.length; index++) {
        const threadId = queue[index]!;
        if (seen.has(threadId)) continue;
        seen.add(threadId);
        if (seen.size > options.observationLimits!.childThreads + 1) throw new Error("Retained child capture exceeds its finite thread bound");
        const metadata = (await rpc("thread/read", { threadId, includeTurns: false })).thread;
        if (metadata?.id !== threadId || metadata.ephemeral !== false || typeof metadata.sessionId !== "string" ||
            threadId === result.threadId && metadata.sessionId !== result.sessionId ||
            threadId !== result.threadId && metadata.parentThreadId !== childParents.get(threadId)) throw new Error("Retained child identity/lineage differs from explicit delegation");
        const snapshot = (await rpc("thread/read", { threadId, includeTurns: true })).thread;
        if (snapshot?.id !== threadId || snapshot.sessionId !== metadata.sessionId || snapshot.ephemeral !== false ||
            snapshot.parentThreadId !== metadata.parentThreadId || !Array.isArray(snapshot.turns)) throw new Error("Full-turn snapshot identity changed");
        const turns = threadId === result.threadId ? snapshot.turns.filter((value: any) => value.id === result.turnId) : snapshot.turns;
        if (!turns.length || turns.length > 32) throw new Error("Retained thread has missing or excessive turns");
        if (threadId !== result.threadId) {
          const expected = options.config?.["agents.default_subagent_model"];
          if (expected && snapshot.model !== expected) result.modelBoundaryValid = false;
          const worker = result.workerModels.find(value => value.threadId === threadId);
          if (worker) worker.observedModel = typeof snapshot.model === "string" ? snapshot.model : null;
          else result.workerModels.push({ threadId, requestedModel: null, observedModel: typeof snapshot.model === "string" ? snapshot.model : null });
          result.childSnapshots.push({ threadId, parentThreadId: metadata.parentThreadId, sessionId: metadata.sessionId,
            model: typeof snapshot.model === "string" ? snapshot.model : null, turnIds: turns.map((value: any) => value.id) });
        }
        for (const value of turns) {
          if (typeof value.id !== "string" || value.itemsView !== "full" || !Array.isArray(value.items) || !["completed", "failed", "interrupted"].includes(value.status)) {
            result.captureGaps.push(`incomplete_snapshot:${threadId}:${String(value.id)}`); continue;
          }
          const captured = captureNotification({ method: "turn/completed", params: { threadId, turn: value } }, captureSecrets)!;
          // Snapshot-only child work is still charged by the same observation guard.
          event({ method: "turn/completed", params: { threadId, turn: value } });
          trace({ direction: "capture", capture: captured, source: "supported_thread_read", sourceThreadId: threadId });
          for (const item of value.items) if (item?.type === "collabAgentToolCall" && item.tool === "spawnAgent") {
            for (const child of item.receiverThreadIds ?? []) if (typeof child === "string" && child !== result.threadId) {
              if (childParents.has(child) && childParents.get(child) !== threadId) throw new Error("Conflicting captured child lineage");
              childParents.set(child, threadId); observedChildren.add(child); queue.push(child);
            }
          }
        }
        for (const child of observedChildren) if (!seen.has(child) && !queue.includes(child)) queue.push(child);
      }
    }
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
    if (fatalError && result.status === "completed") { result.status = result.validStart ? "failed" : "setup_failed"; result.error = errorText(fatalError); }
    if (!result.modelBoundaryValid && result.status === "completed") { result.status = "failed"; result.error = "Observed model routing differs from the frozen model boundary"; }
    // Authentication belongs to the supervisor lifecycle even if a retry produced text.
    if (result.authFailure && result.status === "completed") { result.status = "failed"; result.error = "Authentication failure requires supervisor recovery"; }
    const final = [...messages.values()].filter(value => value.phase === "final_answer");
    const unphased = [...messages.values()].filter(value => value.phase === null);
    result.answerText = (final.length ? final : unphased).map(value => value.text).join("\n");
    result.latencyMs = Math.round(performance.now() - started);
    try {
      trace({ direction: "result", status: result.status, threadId: result.threadId, sessionId: result.sessionId, turnId: result.turnId,
        requestedModel: result.requestedModel, observedModel: result.observedModel, servedModel: result.servedModel, usage: result.usage, cleanup: result.cleanup });
    } catch (error) {
      result.status = result.validStart ? "failed" : "setup_failed"; result.error = `Final capture failed: ${errorText(error)}`; result.traceTruncated = true;
    }
  }
  return result;
}

/** In-memory protocol seam. Always returns executionMode=test and cannot launch a process. */
export function runNativeForTest(options: NativeRunOptions, transport: NativeTestTransport, capture: (value: Message) => void = () => {}): Promise<NativeRunResult> {
  return execute(options, transport, "test", capture);
}

/** Live entrypoint: owner storage and auth-supervisor admission are mandatory. */
export async function runNative(options: NativeRunOptions): Promise<NativeRunResult> {
  const decisionsHelperEvidence = await verifyLiveEnvironment(options);
  const runtimeBinarySha256 = await fileHash(options.codexBinary);
  // Native tool writes remain in the task's sole writable root, not the auth owner's home.
  const runtime = options.runtimeDirectory ?? resolve(options.cwd, ".native-runtime");
  await physicalPath(runtime);
  await mkdir(resolve(runtime, "tmp"), { recursive: true });
  await mkdir(resolve(runtime, "cache"), { recursive: true });
  if (options.fontDirectories?.length) {
    const xml = (value: string) => value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
    await writeFile(resolve(runtime, "fonts.conf"), `<?xml version="1.0"?>\n<!DOCTYPE fontconfig SYSTEM "fonts.dtd">\n<fontconfig>${options.fontDirectories.map(path => `<dir>${xml(path)}</dir>`).join("")}<cachedir>${xml(resolve(runtime, "cache"))}</cachedir></fontconfig>\n`, { flag: "wx", mode: 0o600 });
  }
  await mkdir(dirname(options.tracePath), { recursive: true });
  const media = options.mediaCapture ? await createNativeMediaCapture({ ...options.mediaCapture, roots: options.readableRoots }) : null;
  const file = await open(options.tracePath, "wx", 0o600);
  const traceLimitBytes = options.maxTraceBytes ?? 64 * 1024 * 1024;
  let writes = Promise.resolve();
  let bytes = 0;
  let traceTruncated = false;
  let writeError: Error | null = null;
  let result: NativeRunResult | null = null;
  try {
    const capture = (value: Message) => {
      if (writeError) throw writeError;
      if (value.direction === "capture" && media) media.capture(value.capture, value.source === "supported_thread_read" ? "thread_read_snapshot" : "live_notification");
      const line = `${JSON.stringify({ at: new Date().toISOString(), ...value })}\n`;
      bytes += Buffer.byteLength(line);
      if (bytes > traceLimitBytes) {
        if (!traceTruncated) { traceTruncated = true; throw new Error("Native private capture exceeds its frozen byte bound"); }
        return;
      }
      writes = writes.then(async () => { if (!writeError) await file.writeFile(line); }).catch(error => { writeError ??= error instanceof Error ? error : new Error(String(error)); });
    };
    if (decisionsHelperEvidence) {
      capture({ direction: "owner_helper", ...decisionsHelperEvidence });
      await writes;
      if (writeError) throw writeError;
    }
    result = await execute(options, new StdioTransport(options), "live", capture);
    if (decisionsHelperEvidence) result.decisionsHelperEvidence = decisionsHelperEvidence;
    await writes;
    result.runtimeBinarySha256 = runtimeBinarySha256;
    result.traceLimitBytes = traceLimitBytes;
    if (media) {
      const snapshot = media.snapshot(); result.mediaBindings = snapshot.bindings;
      result.captureGaps = [...(result.captureGaps ?? []), ...snapshot.gaps];
    }
    if (await fileHash(options.codexBinary) !== runtimeBinarySha256) {
      result.status = result.validStart ? "failed" : "setup_failed"; result.modelBoundaryValid = false;
      result.error = "Native runtime binary changed during execution";
    }
    result.traceTruncated ||= traceTruncated || writeError !== null;
    if (result.traceTruncated) { result.status = result.validStart ? "failed" : "setup_failed"; result.error = writeError ? "Native private capture write failed" : "Native private capture exceeds its frozen byte bound"; }
    if (result.authFailure) await writeJson(process.env.GPTGREP_AUTH_FAILURE_RECORD!, { schema: "gptgrep.codex-auth.auth-failure.v1", class: result.authFailure });
    return result;
  } catch (error) {
    if (!result) throw error;
    result.status = result.validStart ? "failed" : "setup_failed"; result.error = `Native post-execution evidence failed: ${errorText(error)}`;
    result.runtimeBinarySha256 = runtimeBinarySha256; result.traceTruncated = true; return result;
  } finally {
    await writes;
    try { await file.close(); } catch (error) {
      if (!result) throw error;
      result.status = result.validStart ? "failed" : "setup_failed"; result.error = "Native capture close failed"; result.traceTruncated = true;
    }
  }
}
