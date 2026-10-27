/** Owner-launched synthetic qualification only. No model, auth setup, Docker call or retry. */
import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { lstat, mkdir, open, opendir, readlink, realpath, symlink, unlink } from "node:fs/promises";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

type Obj = Record<string, any>;
type Pin = { path: string; bytes: number; sha256: string };
const HASH = /^[a-f0-9]{64}$/;
const START = performance.now();
const MAX_WALL_MS = 90_000, REPORT_GRACE_MS = 7_000;
let reporting = false, failureStage = "bootstrap", sandboxAttempted = false;
let sourceReadTotal = 0, sourceReadLimit = 4_294_967_296;
const FIXED = {
  task: "/app/task/.isolation-probe/readable.txt",
  documents: "/app/task/documents/.isolation-probe.txt",
  prepared: "/app/task/prepared/.isolation-probe.txt",
  capture: "/private/episode/output/.isolation-probe.txt",
  "native-home-analogue": "/private/episode/native-home/.isolation-probe.txt",
  "native-state-analogue": "/private/episode/native-state/.isolation-probe.txt",
  judge: "/private/episode/judge-probe/.isolation-probe.txt",
  "other-candidate": "/private/episode/other-candidate/.isolation-probe.txt",
} as const;
const TASK_PROBE = "/app/task/.isolation-probe";
const HOME_ROOT = "/private/episode/native-home/read-isolation-home";
const STATE_ROOT = "/private/episode/native-state/read-isolation-state";
const ESCAPE = `${TASK_PROBE}/private-link`;
const INJECTED = ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"];
const hash = (b: string | Uint8Array) => createHash("sha256").update(b).digest("hex");
function must(ok: unknown, code: string): asserts ok { if (!ok) throw new Error(code); }
function canonical(v: any): string {
  if (v === null || typeof v === "string" || typeof v === "boolean") return JSON.stringify(v);
  if (typeof v === "number" && Number.isFinite(v)) return JSON.stringify(v);
  if (Array.isArray(v)) return `[${v.map(canonical).join(",")}]`;
  must(v && typeof v === "object", "canonical_type");
  return `{${Object.keys(v).sort().map(k => `${JSON.stringify(k)}:${canonical(v[k])}`).join(",")}}`;
}
function closed(v: any, keys: string[], code: string): asserts v is Obj {
  must(v && typeof v === "object" && !Array.isArray(v) && canonical(Object.keys(v).sort()) === canonical([...keys].sort()), code);
}
function number(v: any, lo: number, hi: number, code: string): number {
  must(Number.isSafeInteger(v) && v >= lo && v <= hi, code); return v;
}
function path(v: any): string {
  must(typeof v === "string" && v.length <= 4096 && isAbsolute(v) && resolve(v) === v && !/[\0\r\n*?\[\]{}]/.test(v), "physical_absolute_path_required"); return v;
}
function within(root: string, p: string): boolean {
  const r = relative(root, p); return r === "" || (!r.startsWith(`..${sep}`) && r !== ".." && !isAbsolute(r));
}
function deadline(): void { must(performance.now() - START < MAX_WALL_MS - (reporting ? 0 : REPORT_GRACE_MS), "qualifier_wall_deadline"); }
function counted(n: number): void { sourceReadTotal += n; must(sourceReadTotal <= sourceReadLimit, "cumulative_source_read_limit"); }

/** Small bounded JSON parser: rejects duplicate keys, non-finite numbers and deep inputs. */
function json(text: string): any {
  let at = 0, nodes = 0;
  const ws = () => { while (/[ \t\r\n]/.test(text[at] ?? "x")) at++; };
  const str = () => {
    must(text[at] === '"', "json_string"); const start = at++;
    while (at < text.length) { const ch = text[at++]; if (ch === "\\") at++; else if (ch === '"') return JSON.parse(text.slice(start, at)); }
    throw new Error("json_unterminated_string");
  };
  const value = (depth: number): any => {
    must(depth <= 32 && ++nodes <= 20_000, "json_structure_limit"); ws();
    if (text[at] === '"') return str();
    if (text[at] === "{") {
      at++; ws(); const out: Obj = Object.create(null), seen = new Set<string>();
      if (text[at] === "}") { at++; return out; }
      while (true) {
        ws(); const key = str(); must(!seen.has(key), "json_duplicate_key"); seen.add(key); ws(); must(text[at++] === ":", "json_colon");
        out[key] = value(depth + 1); ws(); const ch = text[at++]; if (ch === "}") return out; must(ch === ",", "json_object_separator");
      }
    }
    if (text[at] === "[") {
      at++; ws(); const out = []; if (text[at] === "]") { at++; return out; }
      while (true) { out.push(value(depth + 1)); ws(); const ch = text[at++]; if (ch === "]") return out; must(ch === ",", "json_array_separator"); }
    }
    const token = /^(?:true|false|null|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)/.exec(text.slice(at));
    must(token, "json_value"); at += token[0].length; const out = JSON.parse(token[0]); must(typeof out !== "number" || Number.isFinite(out), "json_nonfinite"); return out;
  };
  const out = value(0); ws(); must(at === text.length, "json_trailing_data"); return out;
}
async function physical(p: string, missingLeaf = false): Promise<void> {
  path(p); deadline();
  if (missingLeaf) { must(await realpath(dirname(p)) === dirname(p), "parent_symlink"); return; }
  must(await realpath(p) === p, "symlink_path");
}
async function bytes(p: string, cap: number, retain = true, custody = false): Promise<{ bytes: number; sha256: string; data: Buffer; uid: number }> {
  await physical(p); const f = await open(p, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = await f.stat(); must(before.isFile() && before.size <= cap, "regular_file_byte_limit");
    if (custody) must(before.nlink === 1 && (before.mode & 0o7777) === 0o600 && before.uid === process.getuid!(), "control_file_custody");
    const h = createHash("sha256"), chunks: Buffer[] = []; let total = 0; const buffer = Buffer.alloc(65_536);
    while (true) {
      deadline(); const n = (await f.read(buffer, 0, buffer.length, null)).bytesRead; if (!n) break;
      total += n; counted(n); must(total <= cap, "read_byte_limit"); h.update(buffer.subarray(0, n)); if (retain) chunks.push(Buffer.from(buffer.subarray(0, n)));
    }
    const after = await f.stat(); must(total === before.size && after.size === before.size && after.mtimeMs === before.mtimeMs && after.ctimeMs === before.ctimeMs, "file_changed_while_reading");
    return { bytes: total, sha256: h.digest("hex"), data: retain ? Buffer.concat(chunks) : Buffer.alloc(0), uid: before.uid };
  } finally { await f.close(); }
}
async function kernelBytes(p: "/proc/self/cmdline" | "/proc/sys/kernel/random/boot_id", cap: number): Promise<Buffer> {
  // These exact proc files report stat.size=0; ordinary pinned files use bytes().
  const f = await open(p, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const chunks: Buffer[] = []; let total = 0; const buffer = Buffer.alloc(Math.min(cap + 1, 4096));
    while (true) { deadline(); const n = (await f.read(buffer, 0, buffer.length, null)).bytesRead; if (!n) break; counted(n); total += n; must(total <= cap, "kernel_read_limit"); chunks.push(Buffer.from(buffer.subarray(0, n))); }
    return Buffer.concat(chunks);
  } finally { await f.close(); }
}
function pin(v: any, cap: number): Pin {
  closed(v, ["path", "bytes", "sha256"], "pin_fields"); path(v.path); number(v.bytes, 1, cap, "pin_bytes"); must(HASH.test(v.sha256), "pin_sha256"); return v as Pin;
}
async function pinned(p: Pin, cap: number, retain = false): Promise<Buffer> {
  const b = await bytes(p.path, cap, retain); must(b.bytes === p.bytes && b.sha256 === p.sha256, "source_pin_mismatch"); return b.data;
}
function allowed(rules: any, p: string, access: "read" | "write"): void {
  must(Array.isArray(rules) && rules.length > 0 && rules.length <= 128, "allowed_paths_required");
  const seen = new Set<string>(); let selected: Obj | null = null;
  for (const rule of rules) {
    closed(rule, ["path", "kind", "access", "purpose"], "allowed_path_fields"); path(rule.path);
    must(["file", "directory"].includes(rule.kind) && ["read", "write"].includes(rule.access) && typeof rule.purpose === "string", "allowed_path_type");
    must(!seen.has(rule.path), "duplicate_allowed_path"); seen.add(rule.path);
    if ((rule.kind === "file" ? p === rule.path : within(rule.path, p)) && (!selected || rule.path.length > selected.path.length)) selected = rule;
  }
  must(selected && (access === "read" || selected.access === "write"), "path_not_owner_admitted");
}
async function exclusive(p: string, data: Uint8Array, rules: any): Promise<void> {
  allowed(rules, p, "write"); await physical(p, true); const f = await open(p, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
  try { await f.writeFile(data); await f.sync(); } finally { await f.close(); }
}
async function newDir(p: string, rules: any): Promise<void> { allowed(rules, p, "write"); await physical(p, true); await mkdir(p, { mode: 0o700 }); }
function fixtureText(runId: string, role: string): string { return JSON.stringify({ schema: "gptgrep.read-isolation.synthetic.v1", runId, role }) + "\n"; }
function decodeProbe(stdout: string, allows: Obj[], denies: Obj[], writes: Obj[]): boolean {
  must(stdout === "" || stdout.endsWith("\n"), "probe_partial_line");
  const rows = stdout === "" ? [] : stdout.slice(0, -1).split("\n");
  const expected = [...allows.map((row, index) => ({ kind: "allow", index, row })), ...denies.map((row, index) => ({ kind: "deny", index, row })), ...writes.map((row, index) => ({ kind: "write", index, row }))];
  must(rows.length <= expected.length + 1, "probe_excess_lines"); let summary = false;
  for (let n = 0; n < rows.length; n++) {
    const result = json(rows[n]);
    if (n === expected.length) {
      closed(result, ["schema", "positiveReads", "deniedReads", "writeProbes"], "probe_summary_fields");
      must(result.schema === "gptgrep.c4-actor-isolation.probe.v1" && result.positiveReads === allows.length && result.deniedReads === denies.length && result.writeProbes === writes.length, "probe_summary_counts"); summary = true; continue;
    }
    const e = expected[n];
    if (e.kind === "write") {
      closed(result, ["kind", "index", "writable", "error"], "probe_write_fields");
      must(result.kind === "write" && result.index === e.index && typeof result.writable === "boolean", "probe_write_order");
      const denied = !result.writable && ["EACCES", "EPERM", "EROFS", "ENOENT", "ENOTDIR"].includes(result.error);
      e.row.observed = result.writable ? "writable" : denied ? "denied" : "error"; e.row.error = result.error;
      must(result.writable ? result.error === null : denied, "probe_write_operational_failure");
      if (e.row.observed !== e.row.expected) must(n === rows.length - 1, "probe_write_continued_after_failure");
      continue;
    }
    closed(result, ["kind", "index", "readable", "error"], "probe_event_fields");
    must(result.kind === e.kind && result.index === e.index && typeof result.readable === "boolean" && (result.error === null || typeof result.error === "string"), "probe_event_order");
    const expectedDenial = e.kind === "deny" && !result.readable && ["EACCES", "EPERM", "ENOENT", "ENOTDIR"].includes(result.error);
    e.row.observed = result.readable ? "readable" : expectedDenial ? "denied" : "error"; e.row.error = result.error;
    must(result.readable ? result.error === null : expectedDenial || e.kind === "allow", "probe_operational_failure");
    if (result.readable !== (e.kind === "allow")) must(n === rows.length - 1, "probe_continued_after_failure");
  }
  return summary && [...allows, ...denies, ...writes].every(row => row.observed === row.expected);
}
function policy(c: Obj): Obj {
  const fs: Obj = { "/": "deny", "/app": "deny", ":minimal": "read", "/app/task": c.actorCapability.role === "engineer" ? "write" : "read", "/app/task/documents": "read", "/private": "deny" };
  if (c.arm === "gptgrep") fs["/app/task/prepared"] = "read";
  for (const root of c.runtimeReadRoots) fs[root] = "read";
  for (const root of c.privateDenyRoots) fs[root] = "deny";
  if (c.actorCapability.role === "build") fs[c.actorCapability.build_root] = "write";
  return { name: c.profileName, extends: ":read-only", filesystem: fs };
}
function profile(c: Obj): string {
  const p = policy(c);
  return `approval_policy = "never"\ndefault_permissions = ${JSON.stringify(c.profileName)}\nweb_search = "disabled"\n\n[features]\nuse_legacy_landlock = false\n\n[permissions.${c.profileName}]\nextends = ":read-only"\n\n[permissions.${c.profileName}.filesystem]\n` +
    Object.keys(p.filesystem).sort().map(k => `${JSON.stringify(k)} = ${JSON.stringify(p.filesystem[k])}\n`).join("") +
    `\n[permissions.${c.profileName}.network]\nenabled = false\n`;
}
async function observeNamespace(): Promise<Obj> {
  const ns: Obj = {}; for (const [name, leaf] of [["mount", "mnt"], ["pid", "pid"], ["user", "user"], ["time", "time"], ["network", "net"]]) ns[name] = await readlink(`/proc/self/ns/${leaf}`);
  const boot = (await kernelBytes("/proc/sys/kernel/random/boot_id", 1024)).toString().trim();
  return { namespaces: ns, bootId: boot, uid: process.getuid!(), gid: process.getgid!() };
}
async function fixedDaemonDirectory(base: Obj, rules: any): Promise<Obj> {
  const p = `/tmp/codex-daemon-${process.getuid!()}`;
  must(Array.isArray(base.required_empty_directories), "daemon_directory_admission");
  const rows = base.required_empty_directories.filter((r: Obj) => r.guest_path === p);
  must(rows.length === 1, "daemon_directory_exact_admission"); const row = rows[0];
  must(row.uid === process.getuid!() && row.gid === process.getgid!() && row.mode === "0700" && row.mount_access === "read" && row.required_empty === true && row.purpose === "codex-daemon-staging-no-socket", "daemon_directory_owner_contract");
  allowed(rules, p, "read"); await physical(p); const s = await lstat(p);
  must(s.isDirectory() && !s.isSymbolicLink() && s.uid === row.uid && s.gid === row.gid && (s.mode & 0o7777) === 0o700, "daemon_directory_identity");
  const dir = await opendir(p); for await (const _entry of dir) throw new Error("daemon_directory_not_empty");
  return { path: p, uid: s.uid, gid: s.gid, mode: "0700", dev: String(s.dev), ino: String(s.ino), empty: true, mountAccessDeclared: "read", hostSocketExposed: false };
}
async function ownedSize(roots: string[], files: string[], cap: number): Promise<number> {
  let total = 0, entries = 0;
  async function walk(p: string, depth: number): Promise<void> {
    deadline();
    must(depth <= 12 && ++entries <= 2048, "owned_tree_limit"); const s = await lstat(p); total += s.isDirectory() ? 0 : s.size; must(total <= cap, "owned_byte_limit");
    if (s.isDirectory()) { const dir = await opendir(p); for await (const entry of dir) await walk(`${p}/${entry.name}`, depth + 1); }
  }
  for (const root of roots) await walk(root, 0); for (const file of files) await walk(file, 0); return total;
}

async function command(argv: string[], c: Obj, environment: Record<string, string>, monitor: () => Promise<number>): Promise<Obj> {
  const began = performance.now(); let child: ChildProcessWithoutNullStreams | undefined;
  let reason: string | null = null, exitCode: number | null = null, signal: string | null = null, closedChild = false, killSent = false;
  let total = 0, stdoutBytes = 0, stderrBytes = 0, ownedPeak = 0, outRetained = 0, errRetained = 0; const stdout: Buffer[] = [], stderr: Buffer[] = [];
  const outHash = createHash("sha256"), errHash = createHash("sha256");
  let killTimer: ReturnType<typeof setTimeout> | undefined, watchdog: ReturnType<typeof setTimeout> | undefined, inFlight: Promise<void> | undefined;
  const kill = () => { if (child?.pid) { try { process.kill(-child.pid, "SIGKILL"); killSent = true; } catch (e: any) { if (e.code !== "ESRCH") reason = "process_group_signal_failed"; } } };
  const stop = (why: string) => {
    reason ??= why;
    if (child?.pid) { try { process.kill(-child.pid, "SIGTERM"); } catch (e: any) { if (e.code !== "ESRCH") reason = "process_group_signal_failed"; } }
    if (!killTimer) killTimer = setTimeout(kill, c.limits.cleanupMs);
  };
  const timer = setTimeout(() => stop("timeout"), c.limits.commandMs);
  const interval = setInterval(() => {
    if (!inFlight) inFlight = monitor().then(n => { ownedPeak = Math.max(ownedPeak, n); }).catch(() => stop("owned_byte_limit")).finally(() => { inFlight = undefined; });
  }, 50);
  const abort = () => stop("cancelled"); process.once("SIGTERM", abort); process.once("SIGINT", abort);
  try {
    child = spawn(argv[0], argv.slice(1), { cwd: "/app/task", env: environment, detached: true, shell: false, stdio: ["ignore", "pipe", "pipe"] }) as ChildProcessWithoutNullStreams;
    const receive = (kind: "stdout" | "stderr", data: Buffer) => {
      total += data.length; if (kind === "stdout") { stdoutBytes += data.length; outHash.update(data); } else { stderrBytes += data.length; errHash.update(data); }
      if (total > c.limits.outputBytes) { stop("output_limit"); return; }
      const retained = kind === "stdout" ? outRetained : errRetained;
      const prefix = data.subarray(0, Math.max(0, 4096 - retained));
      (kind === "stdout" ? stdout : stderr).push(Buffer.from(prefix));
      if (kind === "stdout") outRetained += prefix.length; else errRetained += prefix.length;
    };
    child.stdout.on("data", data => receive("stdout", data)); child.stderr.on("data", data => receive("stderr", data));
    await new Promise<void>((done) => {
      child!.once("error", () => { reason ??= "spawn_failed"; done(); });
      child!.once("close", (code, sig) => { closedChild = true; exitCode = code; signal = sig; done(); });
      watchdog = setTimeout(() => { stop("child_terminal_unknown"); kill(); done(); }, c.limits.commandMs + c.limits.cleanupMs + 500);
    });
  } catch { reason ??= "spawn_failed"; }
  finally {
    clearTimeout(timer); clearInterval(interval); if (watchdog) clearTimeout(watchdog); if (inFlight) await inFlight;
    process.removeListener("SIGTERM", abort); process.removeListener("SIGINT", abort);
    // A returned parent with a surviving owned group is failure, never a successful probe.
    if (child?.pid) { try { process.kill(-child.pid, 0); stop("surviving_process_group"); kill(); } catch (e: any) { if (e.code !== "ESRCH") reason ??= "process_group_terminal_unknown"; } }
    if (killTimer) clearTimeout(killTimer);
  }
  let groupTerminal = !child?.pid;
  if (child?.pid) for (let n = 0; n < Math.ceil(c.limits.cleanupMs / 20); n++) {
    try { process.kill(-child.pid, 0); } catch (e: any) { if (e.code === "ESRCH") { groupTerminal = true; break; } }
    await new Promise(r => setTimeout(r, 20));
  }
  if (!groupTerminal || !closedChild) reason ??= "process_group_terminal_unknown";
  if (!closedChild && child) { child.stdout.destroy(); child.stderr.destroy(); child.unref(); }
  return { argvSha256: hash(canonical(argv)), exitCode, signal, failure: reason, childReaped: closedChild, processGroupTerminal: groupTerminal, killSent,
    durationMs: Math.ceil(performance.now() - began), stdoutBytes, stderrBytes, stdoutSha256: outHash.digest("hex"), stderrSha256: errHash.digest("hex"),
    retainedPrefixBytesMax: 4096, stdoutTruncated: stdoutBytes > outRetained, stderrTruncated: stderrBytes > errRetained,
    stdout: Buffer.concat(stdout).toString("utf8"), stderr: Buffer.concat(stderr).toString("utf8"), ownedPeakBytes: ownedPeak, weakerSettingsRetries: 0 };
}

async function verifyPublicSource(actor: Obj): Promise<void> {
  const raw = await bytes(actor.source_manifest.path, 1_048_576, true);
  must(raw.sha256 === actor.source_manifest.sha256, "actor_public_manifest_changed");
  const manifest = json(raw.data.toString()); closed(manifest, ["schema", "revision", "files"], "actor_public_manifest_fields");
  must(manifest.schema === "gptgrep.actor-public-source.v1" && manifest.revision === actor.source_revision && Array.isArray(manifest.files) && manifest.files.length >= 1 && manifest.files.length <= 10000, "actor_public_manifest_identity");
  const names = new Set<string>();
  for (const row of manifest.files) {
    closed(row, ["path", "sha256"], "actor_public_file");
    must(typeof row.path === "string" && /^[A-Za-z0-9_.][A-Za-z0-9_./-]*$/.test(row.path) && row.path.split("/").every((p: string) => !["", ".", "..", ".git", "auth.json", "credentials.json", ".netrc", ".npmrc", ".pypirc"].includes(p) && !/^\.env(?:\..*)?$|\.env$/i.test(p)) && HASH.test(row.sha256) && !names.has(row.path), "actor_public_file_identity"); names.add(row.path);
    must((await bytes(`${actor.source_root}/${row.path}`, 1_048_576)).sha256 === row.sha256, "actor_public_source_changed");
  }
}

async function main(): Promise<void> {
  must(process.platform === "linux" && typeof Bun !== "undefined", "linux_bun_guest_required");
  const args = process.argv.slice(2); must(args.length === 4 && args[0] === "--config" && args[2] === "--expect-config-sha256" && HASH.test(args[3]), "exact_config_cli_required");
  const configPath = path(args[1]), raw = await bytes(configPath, 65_536, true, true); must(raw.sha256 === args[3], "config_sha256"); const c = json(raw.data.toString());
  closed(c, ["schema", "runId", "arm", "profileName", "runtimeTag", "sourcePins", "runtimes", "runtimeReadRoots", "privateDenyRoots", "runtimeReadProbes", "filesystemPolicySha256", "ownerSourceClosureSha256", "imageId", "securityProfileSha256", "outputPath", "pathEnvironment", "limits", "actorCapability"], "config_fields");
  must(c.schema === "gptgrep.c4-actor-isolation.qualifier-config.v1" && /^[a-z0-9][a-z0-9_-]{0,95}$/.test(c.runId) && ["bare", "gptgrep"].includes(c.arm), "config_identity");
  const actor = c.actorCapability;
  closed(actor, ["schema", "role", "candidate_id", "source_revision", "source_root", "git_directory", "mutation_paths", "source_manifest", "native_binary", "build_root"], "actor_capability_fields");
  must(actor.schema === "environments-harness.c4-actor-capability.v1" && ["engineer", "build"].includes(actor.role) && /^[a-z0-9][a-z0-9_-]{0,95}$/.test(actor.candidate_id) && /^[a-f0-9]{40}$/.test(actor.source_revision), "actor_capability_identity");
  must(actor.source_root === "/app/task" && actor.git_directory === "/app/task/.git" && Array.isArray(actor.mutation_paths) && actor.mutation_paths.length >= 1 && actor.mutation_paths.length <= 64 && new Set(actor.mutation_paths).size === actor.mutation_paths.length, "actor_public_source_route");
  for (const name of actor.mutation_paths) must(typeof name === "string" && /^[A-Za-z0-9_][A-Za-z0-9_./-]*$/.test(name) && name.split("/").every((part: string) => !["", ".", "..", ".git"].includes(part)), "actor_mutation_paths");
  must(actor.role === "build" ? typeof actor.build_root === "string" && actor.build_root !== "/" && !within(actor.source_root, actor.build_root) && !within(actor.build_root, actor.source_root) : actor.build_root === null, "actor_build_root");
  if (actor.build_root !== null) path(actor.build_root);
  closed(actor.native_binary, ["path", "sha256"], "actor_native_pin"); path(actor.native_binary.path); must(HASH.test(actor.native_binary.sha256) && !within(actor.source_root, actor.native_binary.path), "actor_native_identity");
  closed(actor.source_manifest, ["path", "sha256"], "actor_manifest_pin"); path(actor.source_manifest.path); must(HASH.test(actor.source_manifest.sha256) && !actor.source_manifest.path.endsWith("/auth.json"), "actor_manifest_identity");
  must(/^[a-z][a-z0-9_]{0,63}$/.test(c.profileName) && c.runtimeTag === "rust-v0.164.0", "profile_or_tag");
  must(/^sha256:[a-f0-9]{64}$/.test(c.imageId) && HASH.test(c.filesystemPolicySha256) && HASH.test(c.securityProfileSha256) && HASH.test(c.ownerSourceClosureSha256), "config_hashes");
  closed(c.sourcePins, ["qualifier", "probe", "profile", "fixtures", "ownerGuestSupervisor"], "source_pin_fields");
  for (const p of Object.values(c.sourcePins)) pin(p, 1_048_576);
  closed(c.runtimes, ["bun", "codex", "probeBun"], "runtime_fields"); for (const p of Object.values(c.runtimes)) pin(p, 2_147_483_648);
  must(c.runtimes.codex.path === actor.native_binary.path && c.runtimes.codex.sha256 === actor.native_binary.sha256, "qualified_actor_native_bytes");
  must(c.runtimes.bun.sha256 === c.runtimes.probeBun.sha256 && c.runtimes.bun.bytes === c.runtimes.probeBun.bytes, "probe_bun_same_pinned_bytes");
  must(Array.isArray(c.runtimeReadRoots) && c.runtimeReadRoots.length >= 1 && c.runtimeReadRoots.length <= 16 && new Set(c.runtimeReadRoots).size === c.runtimeReadRoots.length, "runtime_roots");
  for (const root of c.runtimeReadRoots) { path(root); must(root !== "/" && ["/private", "/app", "/eh-control", "/eh-runtime", "/proc", "/sys", "/dev"].every(p => !within(root, p) && !within(p, root)), "broad_runtime_root"); if (c.arm === "bare") must(!within(root, "/opt/gptgrep") && !within("/opt/gptgrep", root), "bare_product_grant"); }
  must(c.runtimeReadRoots.some((root: string) => within(root, c.runtimes.probeBun.path)), "probe_bun_common_read_grant");
  must(Array.isArray(c.privateDenyRoots) && c.privateDenyRoots.length <= 16 && new Set(c.privateDenyRoots).size === c.privateDenyRoots.length, "private_deny_roots");
  for (const root of c.privateDenyRoots) { path(root); must(root !== "/" && !within(root, "/app/task") && !within("/app/task", root) && !c.runtimeReadRoots.some((r: string) => within(root, r) || within(r, root)), "private_grant_overlap"); }
  must(Array.isArray(c.runtimeReadProbes) && c.runtimeReadProbes.length >= 1 && c.runtimeReadProbes.length <= 8, "runtime_probe_count");
  for (const p of c.runtimeReadProbes) { pin(p, 2_147_483_648); must(c.runtimeReadRoots.some((r: string) => within(r, p.path)), "runtime_probe_not_granted"); }
  must(c.runtimeReadProbes.filter((p: Pin) => canonical(p) === canonical(c.runtimes.codex)).length === 1 && new Set(c.runtimeReadProbes.map((p: Pin) => p.path)).size === c.runtimeReadProbes.length, "codex_read_probe_required");
  must(hash(canonical(policy(c))) === c.filesystemPolicySha256, "filesystem_policy_pin");
  path(c.outputPath); must(c.outputPath === `/private/episode/output/read-isolation-${c.runId}.json`, "output_path");
  must(typeof c.pathEnvironment === "string" && c.pathEnvironment.length <= 4096 && c.pathEnvironment.split(":").every((p: string) => isAbsolute(p) && resolve(p) === p && !/[\0\r\n]/.test(p)), "explicit_path_environment");
  closed(c.limits, ["commandMs", "cleanupMs", "outputBytes", "ownedBytes", "reportBytes", "sourceReadBytes"], "limit_fields");
  number(c.limits.commandMs, 1000, 60_000, "command_time"); number(c.limits.cleanupMs, 100, 2000, "cleanup_time");
  number(c.limits.outputBytes, 1024, 65_536, "output_bytes"); number(c.limits.ownedBytes, 65_536, 16_777_216, "owned_bytes");
  number(c.limits.reportBytes, 4096, 65_536, "report_bytes"); number(c.limits.sourceReadBytes, 1, 4_294_967_296, "source_read_bytes");
  sourceReadLimit = c.limits.sourceReadBytes; must(sourceReadTotal <= sourceReadLimit, "cumulative_source_read_limit");

  const bindingPath = path(process.env[INJECTED[0]]), bindingSha = process.env[INJECTED[1]];
  must(HASH.test(bindingSha ?? ""), "command_binding_environment"); const bindingRaw = await bytes(bindingPath, 1_048_576, true, true); must(bindingRaw.sha256 === bindingSha, "command_binding_digest");
  const b = json(bindingRaw.data.toString());
  must(b.schema === "environments-harness.container-command-binding.v1" && b.command_id === "c4-qualification" && b.admission_class === "c4-actor-qualification" && canonical(b.actor_capability) === canonical(actor), "qualification_admission_required");
  must(canonical(b.exact_owner_injected_env_names) === canonical(INJECTED) && b.custody_uid === process.getuid!() && b.custody_mode === "0600" && b.mount_access === "read-only", "command_binding_custody");
  must(b.immutable_job?.sha256 === raw.sha256 && b.immutable_job.path === configPath && b.plan?.cwd === "/app/task" && process.cwd() === "/app/task" && b.plan.run_id === c.runId && HASH.test(b.plan.workload_sha256), "exact_immutable_job");
  const cmdline = (await kernelBytes("/proc/self/cmdline", 131_072)).toString(); must(cmdline.endsWith("\0"), "proc_argv_termination");
  const actualArgv = cmdline.slice(0, -1).split("\0");
  must(canonical(actualArgv) === canonical([c.runtimes.bun.path, "--no-env-file", "--no-install", "--config=" + resolve(dirname(b.guest_runtime.supervisor_guest_path), "guest-bunfig.toml"), c.sourcePins.qualifier.path, "--config", configPath, "--expect-config-sha256", raw.sha256]) && await realpath(process.execPath) === c.runtimes.bun.path, "exact_qualifier_process_argv");
  must(b.plan.command?.argv_sha256 === hash(canonical(actualArgv)) && b.plan.command.executable === actualArgv[0] && b.plan.command.executable_sha256 === c.runtimes.bun.sha256, "owner_exact_argv");
  must(c.sourcePins.qualifier.path === fileURLToPath(import.meta.url) && c.runtimes.bun.path === actualArgv[0], "running_payload_identity");
  const basePath = path(b.container_binding_guest_path), baseRaw = await bytes(basePath, 1_048_576, true, true);
  must(baseRaw.sha256 === b.container_binding?.sha256, "base_container_binding_pin"); const base = json(baseRaw.data.toString());
  must(base.schema === "environments-harness.container-storage-binding.v1" && base.admission_class === "c4-actor-qualification" && canonical(base.actor_capability) === canonical(actor) && base.experiment_arm === (c.arm === "bare" ? "baseline" : "treatment") && base.auth_projection_sha256 === null && base.command_binding_required === true, "synthetic_container_admission");
  for (const key of ["owner_template_sha256", "host_namespace_plan_sha256", "parent_host_binding_sha256", "parent_deadline_monotonic_ns"]) must(base[key] === b[key], "owner_lineage_mismatch");
  must(base.container?.id === b.container_id && HASH.test(base.container.id) && base.container.image_id === c.imageId && base.model_profile?.sha256 === c.sourcePins.profile.sha256, "container_image_profile_pin");
  must(canonical([...base.model_read_roots].sort()) === canonical(["/app/task", ...c.runtimeReadRoots, ...(actor.role === "build" ? [actor.build_root] : [])].sort()), "owner_model_roots");
  must(base.sandbox_privacy_qualification_sha256 === null, "qualification_must_not_claim_existing_privacy_acceptance");
  const security = base.container.security;
  must(security?.readonly_rootfs === true && security.privileged === false && security.capabilities_dropped_all === true && security.no_new_privileges === true && security.pid_mode === "private" && security.userns_mode === "host-unmapped" && security.restart_policy === "no", "container_security_boundary");
  must(hash(canonical(security.profile)) === c.securityProfileSha256 && security.profile.failure_never_relaxes === true && security.profile.cap_add.length === 0, "frozen_seccomp_capability_profile");
  must(b.reservation?.mode === "additional-container-reservation" && b.reservation.transport_exit_is_terminal === false && HASH.test(b.reservation.sha256) && HASH.test(b.reservation.parent_plan_digest) && typeof b.reservation.lease_id === "string" && BigInt(b.reservation.peak_bytes) >= BigInt(c.limits.ownedBytes + c.limits.reportBytes + c.limits.outputBytes), "subreservation_required");
  const remaining = BigInt(b.parent_deadline_monotonic_ns) - process.hrtime.bigint();
  must(remaining > BigInt(MAX_WALL_MS) * 1_000_000n, "original_parent_deadline");
  must(canonical(b.guest_runtime) === canonical(base.guest_runtime) && b.guest_runtime.bun_guest_path === c.runtimes.bun.path && b.guest_runtime.bun_sha256 === c.runtimes.bun.sha256 && b.guest_runtime.supervisor_guest_path === c.sourcePins.ownerGuestSupervisor.path && b.guest_runtime.supervisor_sha256 === c.sourcePins.ownerGuestSupervisor.sha256 && b.guest_runtime.source_closure_sha256 === c.ownerSourceClosureSha256, "owner_supervisor_source_pin");
  await verifyPublicSource(actor);
  const observed = await observeNamespace();
  must(canonical(observed.namespaces) === canonical(base.container.namespaces) && observed.bootId === base.container.boot_id && observed.uid === base.container.uid && observed.gid === base.container.gid, "actual_guest_namespace");
  for (const p of [configPath, bindingPath, basePath]) allowed(b.allowed_paths, p, "read");
  const daemonDirectory = await fixedDaemonDirectory(base, b.allowed_paths);
  const allPins: Pin[] = [...Object.values(c.sourcePins), ...Object.values(c.runtimes), ...c.runtimeReadProbes] as Pin[];
  const uniquePins = new Map<string, Pin>(); for (const p of allPins) { const prior = uniquePins.get(p.path); must(!prior || canonical(prior) === canonical(p), "conflicting_source_pin"); uniquePins.set(p.path, p); }
  // Includes extra retained profile/probe/manifest reads, fixture baselines and control/kernel rereads.
  const extraReadBound = 2 * 1_048_576 + 3 * 65_536 + 2 * 16_384 + 16 * 4096 + 4096;
  must([...uniquePins.values()].reduce((sum, p) => sum + 2 * p.bytes, sourceReadTotal + extraReadBound) <= c.limits.sourceReadBytes, "source_read_envelope");
  for (const p of uniquePins.values()) { allowed(b.allowed_paths, p.path, "read"); await pinned(p, 2_147_483_648); }
  const profileBytes = await pinned(c.sourcePins.profile, 65_536, true); must(profileBytes.toString() === profile(c), "profile_exact_contract");
  const probeBytes = await pinned(c.sourcePins.probe, 16_384, true);
  const fixtures = json((await pinned(c.sourcePins.fixtures, 16_384, true)).toString());
  closed(fixtures, ["schema", "runId", "readonly", "create", "escape"], "fixture_manifest_fields");
  must(fixtures.schema === "gptgrep.read-isolation.fixture-manifest.v1" && fixtures.runId === c.runId, "fixture_manifest_identity");
  const roRoles = c.arm === "gptgrep" ? ["documents", "prepared"] : ["documents"];
  const createRoles = ["task", "capture", "native-home-analogue", "native-state-analogue", "judge", "other-candidate"];
  const checkRows = (rows: any, roles: string[]) => {
    must(Array.isArray(rows) && canonical(rows.map(r => r.role).sort()) === canonical([...roles].sort()), "exact_fixture_roles");
    for (const row of rows) { closed(row, ["role", "path", "sha256", "bytes"], "fixture_fields"); const content = fixtureText(c.runId, row.role); must(row.path === FIXED[row.role as keyof typeof FIXED] && row.bytes === Buffer.byteLength(content) && row.sha256 === hash(content), "typed_fixture_pin"); }
  };
  checkRows(fixtures.readonly, roRoles); checkRows(fixtures.create, createRoles);
  closed(fixtures.escape, ["path", "target"], "escape_fields"); must(fixtures.escape.path === ESCAPE && fixtures.escape.target === FIXED.judge, "fixed_escape_probe");
  for (const row of fixtures.readonly) { allowed(b.allowed_paths, row.path, "read"); await pinned(row, 4096); }
  for (const p of [TASK_PROBE, HOME_ROOT, STATE_ROOT, `${TASK_PROBE}/tmp`, `${TASK_PROBE}/cache`]) allowed(b.allowed_paths, p, "write");
  for (const row of fixtures.create) allowed(b.allowed_paths, row.path, "write"); allowed(b.allowed_paths, ESCAPE, "write"); allowed(b.allowed_paths, c.outputPath, "write");
  await physical(c.outputPath, true); try { await lstat(c.outputPath); throw new Error("receipt_already_exists"); } catch (e: any) { if (e.code !== "ENOENT") throw e; }

  const allowRows = [{ role: "task", path: FIXED.task }, ...fixtures.readonly.map((r: Obj) => ({ role: r.role, path: r.path })), ...c.runtimeReadProbes.map((p: Pin, index: number) => ({ role: `runtime-${index}`, path: p.path }))].map(r => ({ ...r, expected: "readable", observed: "unobserved", error: null }));
  const denyRows = ["capture", "native-home-analogue", "native-state-analogue", "judge", "other-candidate"].map(role => ({ role, path: FIXED[role as keyof typeof FIXED], expected: "denied", observed: "unobserved", error: null }));
  denyRows.push({ role: "task-symlink-private-escape", path: ESCAPE, expected: "denied", observed: "unobserved", error: null });
  denyRows.push({ role: "owner-command-config", path: configPath, expected: "denied", observed: "unobserved", error: null });
  const writeRows = [
    { path: `${TASK_PROBE}/model-write.txt`, expected: actor.role === "engineer" ? "writable" : "denied" },
    { path: "/private/episode/output/.model-write.txt", expected: "denied" },
    { path: "/private/episode/other-candidate/.model-write.txt", expected: "denied" },
    { path: actor.role === "build" ? `${actor.build_root}/.model-write.txt` : "/private/episode/native-home/.model-write.txt", expected: actor.role === "build" ? "writable" : "denied" },
  ].map(row => ({ ...row, observed: "unobserved", error: null }));
  const receipt: Obj = { schema: "gptgrep.c4-actor-isolation.qualification.v1", actorCapability: actor, writeProbes: writeRows, writeBaselineVerified: false, status: "setup_failed", runId: c.runId, arm: c.arm, modelRequests: 0,
    runtimeTagDeclared: c.runtimeTag, sourcePins: c.sourcePins, runtimes: c.runtimes, configSha256: raw.sha256,
    commandBindingSha256: bindingSha, containerBindingSha256: baseRaw.sha256, workloadSha256: b.plan.workload_sha256, containerId: base.container.id,
    imageId: c.imageId, securityProfileSha256: c.securityProfileSha256, security, observedNamespace: observed, fixedDaemonDirectory: daemonDirectory,
    filesystemPolicySha256: c.filesystemPolicySha256, profileSha256: c.sourcePins.profile.sha256, nativeBinarySha256: c.runtimes.codex.sha256, probeSourceSha256: c.sourcePins.probe.sha256,
    modelReadRoots: base.model_read_roots, inheritedMinimalReadRoots: ["/bin", "/sbin", "/usr", "/etc", "/lib", "/lib64", "/nix/store", "/run/current-system/sw"], sandboxProbeReadBytesMax: allowRows.length,
    ownerLineage: { ownerTemplateSha256: b.owner_template_sha256, hostNamespacePlanSha256: b.host_namespace_plan_sha256,
      parentHostBindingSha256: b.parent_host_binding_sha256, parentDeadlineMonotonicNs: b.parent_deadline_monotonic_ns, reservationSha256: b.reservation.sha256,
      reservationLeaseId: b.reservation.lease_id, parentPlanDigest: b.reservation.parent_plan_digest, ownerSourceClosureSha256: c.ownerSourceClosureSha256 },
    baseline: [], baselineVerifiedPost: false, allowedReads: allowRows, privateDenyReads: denyRows, sourcePinsVerifiedPre: true, sourcePinsVerifiedPost: false,
    probe: null, failureCode: null, limits: c.limits, actualAuthenticationQualified: false,
    lifecycle: { ownerFinalizationRequired: true, ownerCleanupPending: true, weakerSettingsRetries: 0, reservationReleased: false },
    byteAccounting: "payload/fixture/report writes are size-checked; owned runtime growth is periodically observed, not a filesystem quota", startedAt: new Date().toISOString() };
  // Reserve room before any mutation; pathological long path/policy metadata cannot consume the terminal report budget.
  must(Buffer.byteLength(JSON.stringify(receipt)) + 36_864 <= c.limits.reportBytes, "receipt_metadata_budget");
  let stage = "fixture_creation"; failureStage = stage;
  try {
    await newDir(TASK_PROBE, b.allowed_paths); await newDir(HOME_ROOT, b.allowed_paths); await newDir(STATE_ROOT, b.allowed_paths);
    await newDir(`${TASK_PROBE}/tmp`, b.allowed_paths); await newDir(`${TASK_PROBE}/cache`, b.allowed_paths);
    await exclusive(`${HOME_ROOT}/config.toml`, profileBytes, b.allowed_paths);
    await exclusive(`${TASK_PROBE}/empty-bunfig.toml`, Buffer.alloc(0), b.allowed_paths);
    for (const row of fixtures.create) await exclusive(row.path, Buffer.from(fixtureText(c.runId, row.role)), b.allowed_paths);
    await symlink(FIXED.judge, ESCAPE);
    stage = "same_uid_unsandboxed_baseline"; failureStage = stage;
    for (const row of [...fixtures.readonly, ...fixtures.create]) {
      const read = await bytes(row.path, 4096); must(read.sha256 === row.sha256 && read.bytes === row.bytes && read.uid === observed.uid, "synthetic_fixture_baseline");
      receipt.baseline.push({ role: row.role, path: row.path, bytes: read.bytes, sha256: read.sha256, uid: read.uid, readByUid: observed.uid, result: "readable" });
    }
    const control = await bytes(configPath, 65_536); must(control.sha256 === raw.sha256 && control.uid === observed.uid, "owner_control_baseline");
    receipt.baseline.push({ role: "owner-command-config", path: configPath, bytes: control.bytes, sha256: control.sha256, uid: control.uid, readByUid: observed.uid, result: "readable" });
    must(await readlink(ESCAPE) === FIXED.judge && (await realpath(ESCAPE)) === FIXED.judge, "escape_baseline_target");
    for (const row of writeRows) {
      // The same UID can create each fresh target before the native sandbox.
      // Its later denial therefore cannot be explained by the fixture mode.
      await exclusive(row.path, Buffer.from("synthetic actor write baseline\n"), b.allowed_paths);
      await unlink(row.path);
    }
    receipt.writeBaselineVerified = true;
    stage = "sandbox_command"; failureStage = stage;
    const positives = allowRows.map(row => row.path), negatives = denyRows.map(row => row.path);
    const probeLiteral = probeBytes.toString() + `\nactorIsolationProbe(${JSON.stringify({ allow: positives, deny: negatives, write: writeRows.map(row => ({ path: row.path, allow: row.expected === "writable" })) })});\n`;
    const argv = [c.runtimes.codex.path, "sandbox", "--permission-profile", c.profileName, "-C", "/app/task", "--", c.runtimes.probeBun.path, "--no-env-file", "--no-install", "--config=" + `${TASK_PROBE}/empty-bunfig.toml`, "--eval", probeLiteral];
    const env = { PATH: c.pathEnvironment, LANG: "C.UTF-8", LC_ALL: "C.UTF-8", CODEX_HOME: HOME_ROOT, CODEX_SQLITE_HOME: STATE_ROOT,
      TMPDIR: `${TASK_PROBE}/tmp`, TMP: `${TASK_PROBE}/tmp`, TEMP: `${TASK_PROBE}/tmp`, XDG_CACHE_HOME: `${TASK_PROBE}/cache`,
      CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED: "1" };
    const monitor = async () => {
      let total = await ownedSize([TASK_PROBE, HOME_ROOT, STATE_ROOT], fixtures.create.filter((r: Obj) => r.role !== "task").map((r: Obj) => r.path), c.limits.ownedBytes);
      for (const row of writeRows.filter(row => !within(TASK_PROBE, row.path))) {
        try { const info = await lstat(row.path); must(info.isFile() && !info.isSymbolicLink() && info.size <= 4096, "write_probe_shape"); total += info.size; }
        catch (error: any) { if (error.code !== "ENOENT") throw error; }
      }
      must(total <= c.limits.ownedBytes, "owned_byte_limit"); return total;
    };
    sandboxAttempted = true; receipt.probe = await command(argv, c, env, monitor);
    receipt.ownedBytesAtEnd = await monitor();
    const probePassed = receipt.probe.stdoutBytes <= 4096 && decodeProbe(receipt.probe.stdout, allowRows, denyRows, writeRows);
    must(receipt.probe.failure === null && receipt.probe.exitCode === 0 && receipt.probe.signal === null && receipt.probe.childReaped && receipt.probe.processGroupTerminal && probePassed, "sandbox_probe_not_passed");
    stage = "post_pin_verification"; failureStage = stage;
    for (const p of uniquePins.values()) await pinned(p, 2_147_483_648);
    await verifyPublicSource(actor);
    for (const row of [...fixtures.readonly, ...fixtures.create]) {
      const read = await bytes(row.path, 4096); must(read.sha256 === row.sha256 && read.bytes === row.bytes && read.uid === observed.uid, "synthetic_fixture_changed");
    }
    must(await readlink(ESCAPE) === FIXED.judge && (await realpath(ESCAPE)) === FIXED.judge, "escape_changed");
    must((await bytes(`${HOME_ROOT}/config.toml`, 65_536)).sha256 === c.sourcePins.profile.sha256, "applied_profile_changed");
    must((await bytes(`${TASK_PROBE}/empty-bunfig.toml`, 1)).sha256 === hash(""), "sandbox_bun_config_changed");
    receipt.baselineVerifiedPost = true;
    must((await bytes(configPath, 65_536, false, true)).sha256 === raw.sha256 && (await bytes(bindingPath, 1_048_576, false, true)).sha256 === bindingSha && (await bytes(basePath, 1_048_576, false, true)).sha256 === baseRaw.sha256, "admission_changed");
    must(canonical(await observeNamespace()) === canonical(observed), "namespace_changed");
    must(canonical(await fixedDaemonDirectory(base, b.allowed_paths)) === canonical(daemonDirectory), "daemon_directory_changed");
    receipt.sourcePinsVerifiedPost = true;
    receipt.status = "passed";
  } catch (e: any) { receipt.status = stage === "fixture_creation" || stage === "same_uid_unsandboxed_baseline" ? "setup_failed" : "failed"; receipt.failureCode = `${stage}:${String(e?.message ?? "unknown").slice(0,256)}`; }
  reporting = true; failureStage = "receipt_write";
  receipt.finishedAt = new Date().toISOString(); receipt.durationMs = Math.ceil(performance.now() - START);
  receipt.cumulativeSourceReadBytes = sourceReadTotal;
  // Compact encoding keeps the pre-reserved metadata/output allowance meaningful.
  let out = Buffer.from(JSON.stringify(receipt) + "\n");
  if (out.length > c.limits.reportBytes) {
    out = Buffer.from(JSON.stringify({ schema: "gptgrep.c4-actor-isolation.failure.v1", status: "failed", runId: c.runId, configSha256: raw.sha256,
      commandBindingSha256: bindingSha, containerBindingSha256: baseRaw.sha256, failureStage, failureCode: "receipt_byte_limit", sandboxAttempted,
      modelRequests: 0, ownerFinalizationRequired: true, reservationReleased: false, receiptKind: "minimal-failure" }) + "\n");
    receipt.status = "failed";
  }
  must(out.length <= c.limits.reportBytes, "minimal_receipt_byte_limit");
  await exclusive(c.outputPath, out, b.allowed_paths);
  process.stdout.write(JSON.stringify({ schema: json(out.toString()).schema, status: receipt.status, path: c.outputPath, sha256: hash(out), bytes: out.length }) + "\n");
  if (receipt.status !== "passed") process.exitCode = 1;
}

// The owner supervisor supplies the hard outer deadline and process-group custody.
main().catch((error: any) => { process.stderr.write(JSON.stringify({ status: sandboxAttempted ? "failed" : "setup_failed", failureStage, errorCode: String(error?.message ?? "unknown").slice(0,256), receiptWritten: false }) + "\n"); process.exitCode = 1; });
