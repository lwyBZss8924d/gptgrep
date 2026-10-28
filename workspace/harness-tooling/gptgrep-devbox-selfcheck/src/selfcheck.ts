#!/usr/bin/env bun
/** Current-host observations, with an explicit separately qualified local probe interface. */
import { createHash, randomUUID } from "node:crypto";
import { constants, createReadStream, fstatSync, lstatSync, mkdirSync, openSync, closeSync, readSync, writeFileSync, realpathSync, statSync, statfsSync } from "node:fs";
import { hostname } from "node:os";
import { basename, dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";

export type Status = "PASSED" | "FAILED" | "UNKNOWN";
type Pin = { path: string; sha256: string };
type Observation = { name: string; status: Status; reason: string; observed?: unknown };
type Config = {
  schema: "gptgrep.devbox-selfcheck.config.v1";
  project_root: string;
  expected_host: { hostname: string; platform: string; arch: string };
  storage: { path: string; device: number; inode: number; reserve_bytes: number };
  tools: (Pin & { name: string })[];
  links: { name: string; path: string; target: string }[];
  qualifier?: { executable: Pin; source: Pin; evidence_root: string; timeout_ms: number; max_age_ms: number };
};
const HASH = /^[a-f0-9]{64}$/;
export const PROBES = ["storage.admission", "runtime.native_tools", "corpus.admission", "auth.isolation", "lifecycle.protection"] as const;
const NAMES = ["storage_and_mount_health", "toolchains_and_native_runtime", "corpus_admission", "auth_and_isolation", "governance_and_lifecycle"];
const MAX_JSON = 256 * 1024;

export function aggregate(statuses: Status[]): Status {
  return statuses.includes("FAILED") ? "FAILED" : statuses.length && statuses.every(s => s === "PASSED") ? "PASSED" : "UNKNOWN";
}
const digest = (value: unknown) => createHash("sha256").update(JSON.stringify(value)).digest("hex");
function object(value: unknown, keys: string[], required = keys): asserts value is Record<string, any> {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some(k => !keys.includes(k)) || required.some(k => !(k in value))) throw new Error("Unexpected or missing fields");
}
function nonempty(value: unknown): asserts value is string {
  if (typeof value !== "string" || !value.trim() || value.includes("\0")) throw new Error("Expected a nonempty string");
}
function integer(value: unknown, min = 0): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < min) throw new Error("Expected a bounded integer");
}
function pin(value: unknown): asserts value is Pin {
  object(value, ["path", "sha256"]); nonempty(value.path);
  if (!HASH.test(value.sha256)) throw new Error("Expected SHA-256 pin");
}
export function parseConfig(value: unknown): Config {
  object(value, ["schema", "project_root", "expected_host", "storage", "tools", "links", "qualifier"], ["schema", "project_root", "expected_host", "storage", "tools", "links"]);
  if (value.schema !== "gptgrep.devbox-selfcheck.config.v1") throw new Error("Unsupported config schema");
  nonempty(value.project_root); object(value.expected_host, ["hostname", "platform", "arch"]);
  for (const field of Object.values(value.expected_host)) nonempty(field);
  object(value.storage, ["path", "device", "inode", "reserve_bytes"]); nonempty(value.storage.path);
  for (const key of ["device", "inode", "reserve_bytes"]) integer(value.storage[key], 1);
  if (!Array.isArray(value.tools) || !value.tools.length || value.tools.length > 64 || !Array.isArray(value.links) || value.links.length > 64) throw new Error("Bounded tool/link lists required");
  for (const tool of value.tools) { object(tool, ["name", "path", "sha256"]); nonempty(tool.name); pin({ path: tool.path, sha256: tool.sha256 }); }
  for (const link of value.links) { object(link, ["name", "path", "target"]); for (const field of Object.values(link)) nonempty(field); }
  if (new Set(value.tools.map((t: any) => t.name)).size !== value.tools.length || new Set(value.links.map((t: any) => t.name)).size !== value.links.length) throw new Error("Duplicate observation names");
  if (value.qualifier !== undefined) {
    object(value.qualifier, ["executable", "source", "evidence_root", "timeout_ms", "max_age_ms"]);
    pin(value.qualifier.executable); pin(value.qualifier.source); nonempty(value.qualifier.evidence_root);
    integer(value.qualifier.timeout_ms, 1); integer(value.qualifier.max_age_ms, 1);
    if (value.qualifier.timeout_ms > 60_000 || value.qualifier.max_age_ms > 300_000) throw new Error("Qualifier bounds exceed the local protocol limits");
  }
  return value as Config;
}
function safePath(path: string): string {
  // Never open client credentials or dotenv contents, even through a symlink.
  for (const name of [basename(path), basename(realpathSync(path))]) {
    if (/^(auth\.json|credentials(?:\.json)?|\.env(?:\..*)?)$/i.test(name)) throw new Error("Credential files are not observation inputs");
  }
  return realpathSync(path);
}
async function hashFile(path: string): Promise<string> {
  const physical = safePath(path);
  if (!statSync(physical).isFile()) throw new Error("Expected a regular file");
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(physical)) hash.update(chunk);
  return hash.digest("hex");
}
/** One physical descriptor snapshot is the only source for parsing and hashing. */
export function readSnapshot(
  path: string,
  evidenceRoot?: string,
  read: (fd: number, buffer: Buffer, offset: number, length: number, position: number | null) => number = readSync,
) {
  const physical = evidenceRoot ? under(evidenceRoot, path) : safePath(path);
  const beforePath = lstatSync(physical, { bigint: true });
  const fields = ["dev", "ino", "mode", "size", "mtimeNs", "ctimeNs"] as const;
  const same = (a: typeof beforePath, b: typeof beforePath) => fields.every(key => a[key] === b[key]);
  if (!beforePath.isFile() || beforePath.size > BigInt(MAX_JSON)) throw new Error("Input is not a bounded physical regular file");
  const fd = openSync(physical, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const before = fstatSync(fd, { bigint: true });
    if (!before.isFile() || !same(beforePath, before)) throw new Error("Input identity changed before reading");
    const buffer = Buffer.alloc(MAX_JSON + 1);
    let size = 0;
    while (size < buffer.length) {
      const count = read(fd, buffer, size, buffer.length - size, null);
      if (count === 0) break;
      size += count;
    }
    const after = fstatSync(fd, { bigint: true });
    const afterPath = lstatSync(physical, { bigint: true });
    if (size > MAX_JSON || BigInt(size) !== before.size || !same(before, after) || !same(before, afterPath) || safePath(path) !== physical || (evidenceRoot && under(evidenceRoot, path) !== physical)) throw new Error("Input changed during snapshot acquisition");
    const bytes = buffer.subarray(0, size);
    return { path: physical, bytes, sha256: createHash("sha256").update(bytes).digest("hex") };
  } finally { closeSync(fd); }
}
async function verifyPin(value: Pin, base: string) {
  const path = resolve(base, value.path);
  const sha256 = await hashFile(path);
  if (sha256 !== value.sha256) throw new Error("Pinned file digest mismatch");
  return { path: realpathSync(path), sha256 };
}
function under(root: string, path: string): string {
  const physicalRoot = realpathSync(root), physical = safePath(path), rel = relative(physicalRoot, physical);
  if (!rel || rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel)) throw new Error("Evidence must remain inside its configured root");
  return physical;
}

/** A pinned producer must execute the real owner probes; this validates their transport/bindings. */
export async function verifyProbe(
  proof: unknown,
  expected: { probe: string; request_id: string; binding_sha256: string; producer_sha256: string; started_ms: number; ended_ms: number; max_age_ms: number; evidence_root: string },
): Promise<Status> {
  object(proof, ["schema", "scope", "probe", "request_id", "binding_sha256", "producer_sha256", "run_id", "started_at", "completed_at", "expires_at", "status", "command_sha256", "evidence"]);
  if (proof.schema !== "gptgrep.devbox-selfcheck.probe.v1" || proof.scope !== "live_current_host") throw new Error("A static or fixture receipt is not a live probe");
  for (const key of ["probe", "request_id", "binding_sha256", "producer_sha256"] as const) if (proof[key] !== expected[key]) throw new Error("Probe identity mismatch");
  nonempty(proof.run_id);
  if (!HASH.test(proof.command_sha256) || !["PASSED", "FAILED", "UNKNOWN"].includes(proof.status)) throw new Error("Invalid probe result");
  const start = Date.parse(proof.started_at), end = Date.parse(proof.completed_at), expiry = Date.parse(proof.expires_at);
  const now = Date.now();
  if (![start, end, expiry].every(Number.isFinite) || start < expected.started_ms - 1000 || end < start || end > expected.ended_ms + 1000 || expiry <= now || expiry - end > expected.max_age_ms || now - end > expected.max_age_ms) throw new Error("Probe is stale or outside this invocation");
  pin(proof.evidence);
  const evidence = readSnapshot(resolve(expected.evidence_root, proof.evidence.path), expected.evidence_root);
  if (evidence.sha256 !== proof.evidence.sha256) throw new Error("Probe evidence digest mismatch");
  return proof.status;
}

type Options = { mode: "local" | "remote"; json: boolean; strict: boolean; verbose: boolean; qualify: boolean; config?: string; output?: string; help: boolean };
function options(argv: string[]): Options {
  const result: Options = { mode: "local", json: false, strict: false, verbose: false, qualify: false, help: false };
  const seen = new Set<string>();
  const tokens = argv.flatMap(arg => /^--(?:mode|config|output-file)=/.test(arg) ? [arg.slice(0, arg.indexOf("=")), arg.slice(arg.indexOf("=") + 1)] : [arg]);
  for (let i = 0; i < tokens.length; i++) {
    const arg = tokens[i] === "-v" ? "--verbose" : tokens[i] === "-h" ? "--help" : tokens[i];
    if (seen.has(arg)) throw new Error("Duplicate option"); seen.add(arg);
    if (["--json", "--strict", "--verbose", "--qualify", "--help"].includes(arg)) (result as any)[arg.slice(2)] = true;
    else if (["--mode", "--config", "--output-file"].includes(arg)) {
      const value = tokens[++i]; if (!value || value.startsWith("--")) throw new Error("Option needs a value");
      if (arg === "--mode") { if (value !== "local" && value !== "remote") throw new Error("Mode must be local or remote"); result.mode = value; }
      else if (arg === "--config") result.config = value; else result.output = value;
    } else throw new Error("Unknown option");
  }
  return result;
}

export async function inspect(opts: Options) {
  const observedAt = new Date().toISOString(), requestId = randomUUID();
  const host = { hostname: hostname(), platform: process.platform, arch: process.arch };
  const implementation = { path: realpathSync(import.meta.filename), sha256: await hashFile(import.meta.filename) };
  const runtime = { path: realpathSync(process.execPath), sha256: await hashFile(process.execPath), version: Bun.version };
  const observations: Observation[][] = [[], [], [], [], []];
  const add = (gate: number, name: string, status: Status, reason: string, observed?: unknown) => observations[gate].push({ name, status, reason, ...(observed === undefined ? {} : { observed }) });
  let config: Config | undefined, configPin: Pin | null = null, base = process.cwd(), project: any = null;
  if (!opts.config) add(4, "config", "UNKNOWN", "A private non-secret --config observation specification is required");
  else {
    try { const snapshot = readSnapshot(resolve(opts.config)); config = parseConfig(JSON.parse(snapshot.bytes.toString("utf8"))); configPin = { path: snapshot.path, sha256: snapshot.sha256 }; base = dirname(snapshot.path); }
    catch { add(4, "config", "UNKNOWN", "Configuration is missing, unreadable, unsupported or invalid; no probes executed"); }
  }
  if (config) {
    add(4, "host", (Object.keys(host) as (keyof typeof host)[]).every(key => host[key] === config.expected_host[key]) ? "PASSED" : "FAILED", "Current process host compared with the explicit target", host);
    if (opts.mode === "remote" && host.platform !== "linux") add(4, "mode", "UNKNOWN", "Remote mode only observes the current Linux target; it never connects or wakes a host");
    try {
      const path = realpathSync(resolve(base, config.storage.path)), st = statSync(path), fs = statfsSync(path);
      const available = fs.bavail * fs.bsize;
      const matches = st.isDirectory() && st.dev === config.storage.device && st.ino === config.storage.inode && available >= config.storage.reserve_bytes;
      add(0, "filesystem", matches ? "PASSED" : "FAILED", "Current physical directory identity and available bytes only; no admission or Docker state inferred", { path, device: st.dev, inode: st.ino, available_bytes: available, reserve_bytes: config.storage.reserve_bytes });
    } catch { add(0, "filesystem", "UNKNOWN", "Configured storage could not be observed"); }
    for (const tool of config.tools) {
      try { const observed = await verifyPin(tool, base); if (!(statSync(observed.path).mode & 0o111)) throw new Error("Not executable"); add(1, tool.name, "PASSED", "Executable bytes match the pin; no version or native tool execution inferred", { ...observed, version: null }); }
      catch { add(1, tool.name, "UNKNOWN", "Executable is missing, inaccessible, non-executable or does not match its pin"); }
    }
    for (const link of config.links) {
      try { const path = resolve(base, link.path), target = realpathSync(resolve(base, link.target)), actual = realpathSync(path); add(4, link.name, lstatSync(path).isSymbolicLink() && actual === target ? "PASSED" : "FAILED", "Observed symbolic link target; no autoload/runtime adoption inferred", { path, target: actual }); }
      catch { add(4, link.name, "UNKNOWN", "Configured symbolic link target is unavailable"); }
    }
    try {
      const root = realpathSync(resolve(base, config.project_root));
      const git = spawnSync("git", ["--no-optional-locks", "-C", root, "rev-parse", "--show-toplevel", "HEAD"], { encoding: "utf8", timeout: 5000, maxBuffer: 8192, env: { ...process.env, GIT_OPTIONAL_LOCKS: "0" } });
      const [top, revision, ...extra] = (git.stdout ?? "").trim().split("\n");
      if (git.status !== 0 || extra.length || realpathSync(top) !== root || !/^[a-f0-9]{40,64}$/.test(revision)) throw new Error("No exact Git identity");
      project = { path: root, revision }; add(4, "source", "PASSED", "Observed Git revision; implementation digest separately binds uncommitted selfcheck bytes", project);
    } catch { add(4, "source", "UNKNOWN", "Exact project Git identity unavailable"); }
  }
  const binding = { implementation, runtime, config: configPin, project, host, mode: opts.mode, execution_scope: "current_host" };
  const bindingHash = digest(binding);
  const qualification: any = { status: "UNKNOWN", attempted: false, reason: "No live local qualifier invoked", producer: null, proof_refs: [] };
  const statuses: Status[] = PROBES.map(() => "UNKNOWN");
  const observed = observations.flat();
  if (opts.qualify && config?.qualifier && project && aggregate(observed.map(o => o.status)) === "PASSED") {
    qualification.attempted = true;
    try {
      const q = config.qualifier, executable = await verifyPin(q.executable, base), source = await verifyPin(q.source, base);
      const producer = { executable, source }, producerHash = digest(producer), evidenceRoot = realpathSync(resolve(base, q.evidence_root));
      qualification.producer = producer;
      const request = { schema: "gptgrep.devbox-selfcheck.qualification-request.v1", request_id: requestId, binding, binding_sha256: bindingHash, producer_sha256: producerHash, required_probes: PROBES, evidence_root: evidenceRoot };
      const start = Date.now();
      const argv = executable.path === source.path ? ["--selfcheck-qualification-v1"] : [source.path, "--selfcheck-qualification-v1"];
      const child = spawnSync(executable.path, argv, { input: JSON.stringify(request), encoding: "utf8", timeout: q.timeout_ms, maxBuffer: MAX_JSON, env: process.env });
      const end = Date.now();
      if (child.status !== 0 || child.error) throw new Error("Qualifier failed or exceeded bounds");
      const result = JSON.parse(child.stdout); object(result, ["schema", "request_id", "binding_sha256", "proofs"]);
      if (result.schema !== "gptgrep.devbox-selfcheck.qualification-result.v1" || result.request_id !== requestId || result.binding_sha256 !== bindingHash || !Array.isArray(result.proofs) || result.proofs.length !== PROBES.length) throw new Error("Unbound qualifier response");
      for (let i = 0; i < PROBES.length; i++) {
        pin(result.proofs[i]); const snapshot = readSnapshot(resolve(evidenceRoot, result.proofs[i].path), evidenceRoot);
        if (snapshot.sha256 !== result.proofs[i].sha256) throw new Error("Probe receipt digest mismatch");
        statuses[i] = await verifyProbe(JSON.parse(snapshot.bytes.toString("utf8")), { probe: PROBES[i], request_id: requestId, binding_sha256: bindingHash, producer_sha256: producerHash, started_ms: start, ended_ms: end, max_age_ms: q.max_age_ms, evidence_root: evidenceRoot });
        qualification.proof_refs.push({ probe: PROBES[i], path: snapshot.path, sha256: snapshot.sha256 });
      }
      qualification.status = aggregate(statuses); qualification.reason = "Fresh local producer and owner probe bindings verified; workload-specific admission remains separate";
    } catch {
      statuses.fill("UNKNOWN"); qualification.status = "UNKNOWN"; qualification.reason = "Qualifier evidence unavailable, stale, unbound, invalid or unsuccessful; no readiness inferred";
    }
  } else if (opts.qualify) qualification.reason = "Qualification requires a pinned producer and complete matching current-host observations";
  const gates = PROBES.map((probe, i) => ({ gate: i + 1, name: NAMES[i], status: aggregate([...observations[i].map(o => o.status), statuses[i]]), required_probe: probe, observations: observations[i] }));
  const overall = aggregate(gates.map(g => g.status));
  return {
    schema: "gptgrep.devbox-selfcheck.receipt.v2", receipt_id: requestId, timestamp_utc: observedAt, mode: opts.mode, strict: opts.strict,
    execution_scope: "current_host", overall_status: overall, binding, binding_sha256: bindingHash,
    observations_status: aggregate(observed.map(o => o.status)), qualification,
    workload_admission: { status: "not_granted", reason: "Each workload still requires the selected owner's exact plan, admission and startup/phase/terminal receipts" },
    summary: { total_gates: gates.length, passed_gates: gates.filter(g => g.status === "PASSED").length, failed_gates: gates.filter(g => g.status === "FAILED").length, unknown_gates: gates.filter(g => g.status === "UNKNOWN").length }, gates,
  };
}

async function main() {
  let opts: Options;
  try { opts = options(process.argv.slice(2)); }
  catch (error) { console.error(`gptgrep-devbox-selfcheck: ${(error as Error).message}`); process.exitCode = 2; return; }
  if (opts.help) {
    console.log("Usage: gptgrep-devbox-selfcheck --config <private non-secret spec.json> [--mode local|remote] [--json] [--strict] [--output-file path] [--verbose|-v] [--qualify] [--help|-h]\nChecks the current host only. Remote mode never connects or wakes a host.\n--qualify explicitly invokes the configured pinned local producer; default inventory never runs qualification probes.\nMissing or unverified evidence exits nonzero. No receipt grants workload admission."); return;
  }
  const receipt = await inspect(opts);
  if (opts.verbose) console.error(`Observed current host; qualification ${receipt.qualification.attempted ? "invoked" : "not invoked"}`);
  if (opts.output) {
    try { const path = resolve(opts.output); mkdirSync(dirname(path), { recursive: true, mode: 0o700 }); const fd = openSync(path, "wx", 0o600); try { writeFileSync(fd, `${JSON.stringify(receipt, null, 2)}\n`); } finally { closeSync(fd); } }
    catch { console.error("Cannot create a new private output receipt; existing receipts are not overwritten"); process.exitCode = 2; return; }
  }
  if (opts.json) console.log(JSON.stringify(receipt, null, 2));
  else {
    console.log(`GPTgrep DevBox selfcheck: ${receipt.overall_status} (current host, ${opts.mode})`);
    for (const gate of receipt.gates) console.log(`Gate ${gate.gate} ${gate.name}: ${gate.status}`);
    console.log("Observation and qualification evidence do not grant workload admission.");
  }
  process.exitCode = receipt.overall_status === "PASSED" ? 0 : 1;
}
if (import.meta.main) await main();
