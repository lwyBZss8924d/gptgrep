import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, mkdir, readFile, readdir, realpath, rename, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, relative, resolve, sep } from "node:path";

export function canonical(value: unknown): string {
  if (value === null || typeof value === "string" || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "number" && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value === "object") return `{${Object.entries(value as object).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  throw new Error("Canonical JSON requires finite, defined JSON values");
}

export function digest(value: unknown): string { return sha256(canonical(value)); }
export function sha256(value: string | Uint8Array): string { return createHash("sha256").update(value).digest("hex"); }
export async function fileHash(path: string): Promise<string> {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}

/** Linux exposes the actual OS argv, including Bun guard flags omitted from process.argv. */
export async function processCommandIdentity(): Promise<{ argv: string[]; argvSha256: string; executable: string; method: "proc-self-cmdline" | "runtime-execArgv" }> {
  let argv: string[], method: "proc-self-cmdline" | "runtime-execArgv";
  if (process.platform === "linux") {
    const raw = await readFile("/proc/self/cmdline");
    if (!raw.length || raw.length > 64 * 1024 || raw.at(-1) !== 0) throw new Error("Actual process command is unavailable or exceeds its bound");
    argv = new TextDecoder("utf-8", { fatal: true }).decode(raw).split("\0").slice(0, -1);
    method = "proc-self-cmdline";
  } else {
    // The local engineering fixture route records this weaker runtime observation explicitly.
    argv = [process.argv[0]!, ...process.execArgv, ...process.argv.slice(1)];
    method = "runtime-execArgv";
  }
  if (!argv.length || argv.length > 256 || !isAbsolute(argv[0]!) || argv.some(arg => arg.includes("\0") || Buffer.byteLength(arg) > 16 * 1024)) throw new Error("Process command observation is not exact and bounded");
  return { argv, argvSha256: digest(argv), executable: argv[0]!, method };
}

export function within(root: string, path: string): boolean {
  const r = relative(resolve(root), resolve(path));
  return r === "" || (!r.startsWith(`..${sep}`) && r !== ".." && !isAbsolute(r));
}

export async function physicalPath(path: string): Promise<string> {
  path = resolve(path);
  let cursor = path;
  while (true) {
    try {
      const info = await lstat(cursor);
      if (info.isSymbolicLink()) throw new Error(`Symlink is not an admitted output path: ${cursor}`);
    } catch (error: any) { if (error.code !== "ENOENT") throw error; }
    if (dirname(cursor) === cursor) break;
    cursor = dirname(cursor);
  }
  return path;
}

export async function writeJson(path: string, value: unknown): Promise<void> {
  await physicalPath(path);
  await mkdir(dirname(path), { recursive: true });
  const temp = `${path}.${process.pid}.${crypto.randomUUID()}.tmp`;
  await writeFile(temp, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: "wx" });
  await rename(temp, path);
}

/** Create-once artifacts are sized before creating directories or files. */
export async function writeJsonExclusive(path: string, value: unknown, maxBytes: number): Promise<void> {
  const bytes = Buffer.from(`${JSON.stringify(value, null, 2)}\n`);
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || bytes.length > maxBytes) throw new Error("Serialized JSON exceeds its frozen byte bound");
  await physicalPath(path);
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, bytes, { mode: 0o600, flag: "wx" });
}

export async function readJson<T = any>(path: string): Promise<T> {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || info.size > 64 * 1024 * 1024) throw new Error("Expected a bounded regular JSON file");
  return JSON.parse(await readFile(path, "utf8")) as T;
}

export interface ContainerActorCapability {
  schema: "environments-harness.c4-actor-capability.v1";
  role: "engineer" | "build" | "agy"; candidate_id: string; source_revision: string;
  source_root: string; git_directory: string; mutation_paths: string[];
  source_manifest: { path: string; sha256: string }; native_binary: { path: string; sha256: string }; build_root: string | null;
}
export function validateContainerActorCapability(value: ContainerActorCapability): void {
  if (!value || Object.keys(value).sort().join(",") !== "build_root,candidate_id,git_directory,mutation_paths,native_binary,role,schema,source_manifest,source_revision,source_root" ||
      value.schema !== "environments-harness.c4-actor-capability.v1" || !["engineer", "build", "agy"].includes(value.role) ||
      !/^[a-z0-9][a-z0-9_-]{0,95}$/.test(value.candidate_id) || !/^[a-f0-9]{40}$/.test(value.source_revision) ||
      typeof value.source_root !== "string" || !isAbsolute(value.source_root) || resolve(value.source_root) !== value.source_root || value.source_root === "/" ||
      value.git_directory !== resolve(value.source_root, ".git") || !Array.isArray(value.mutation_paths) || !value.mutation_paths.length || value.mutation_paths.length > 64 ||
      new Set(value.mutation_paths).size !== value.mutation_paths.length || value.mutation_paths.some(p => typeof p !== "string" || p.length > 512 || !/^[A-Za-z0-9_][A-Za-z0-9_./-]*$/.test(p) || p.split("/").some(part => ["", ".", "..", ".git"].includes(part))) ||
      !value.source_manifest || Object.keys(value.source_manifest).sort().join(",") !== "path,sha256" || !isAbsolute(value.source_manifest.path) || within(value.source_root, value.source_manifest.path) || basename(value.source_manifest.path) === "auth.json" || !/^[a-f0-9]{64}$/.test(value.source_manifest.sha256)) throw new Error("Exact closed C4 actor capability required");
  if (!value.native_binary || Object.keys(value.native_binary).sort().join(",") !== "path,sha256" || !isAbsolute(value.native_binary.path) || within(value.source_root, value.native_binary.path) || basename(value.native_binary.path) === "auth.json" || !/^[a-f0-9]{64}$/.test(value.native_binary.sha256)) throw new Error("C4 native binary must retain its exact qualified bytes outside mutable source");
  if (value.role === "build" ? typeof value.build_root !== "string" || !isAbsolute(value.build_root) || resolve(value.build_root) !== value.build_root || value.build_root === "/" || within(value.source_root, value.build_root) || within(value.build_root, value.source_root) || within(value.build_root, value.native_binary.path) || within(value.build_root, value.source_manifest.path) : value.build_root !== null) throw new Error("Only build roles carry one distinct writable build root");
}
export function validateContainerCapability(admissionClass: string, commandId: string, authProjection: unknown, capability?: ContainerActorCapability): void {
  if (["c4-actor", "c4-actor-qualification"].includes(admissionClass)) {
    validateContainerActorCapability(capability!);
    if (capability!.role === "agy") throw new Error("Native AGY owner authentication and sandbox qualification is unavailable");
    const qualifier = admissionClass === "c4-actor-qualification";
    if (commandId !== (qualifier ? "c4-qualification" : `c4-${capability!.role}`) ||
        (qualifier || capability!.role === "build" ? authProjection !== null : typeof authProjection !== "string" || !/^[a-f0-9]{64}$/.test(authProjection))) throw new Error("C4 role, command and authentication boundary differs");
    return;
  }
  if (capability !== undefined || !(admissionClass === "bounded-qualification" ? commandId === "qualification" && authProjection === null :
      admissionClass === "reader" && ["preflight", "reader"].includes(commandId))) throw new Error("Reader and qualification cannot inherit actor authority");
}
type InputPin = { path: string; sha256: string };
export interface ContainerBuildPdfium {
  launcher: InputPin; library: InputPin; includeDirectory: string; headers: InputPin[];
}
/** These runtime inputs are carried by the immutable build job, outside source acceptance. */
export function validateContainerBuildPdfium(value: ContainerBuildPdfium, capability: ContainerActorCapability, writeRoots: string[] = []): void {
  const exact = (p: unknown): p is string => typeof p === "string" && isAbsolute(p) && resolve(p) === p && p !== "/" && !/[\0\r\n:]/.test(p);
  const pin = (p: InputPin): boolean => !!p && Object.keys(p).sort().join(",") === "path,sha256" && exact(p.path) && exactHash(p.sha256);
  if (!value || Object.keys(value).sort().join(",") !== "headers,includeDirectory,launcher,library" || capability.role !== "build" ||
      !pin(value.launcher) || basename(value.launcher.path) !== "env" || !pin(value.library) || basename(value.library.path) !== "libpdfium.so" ||
      !exact(dirname(value.library.path)) || !exact(value.includeDirectory) || !Array.isArray(value.headers) || !value.headers.length || value.headers.length > 128 ||
      value.headers.some(p => !pin(p) || dirname(p.path) !== value.includeDirectory || !/^[A-Za-z0-9_][A-Za-z0-9_.-]*\.h$/.test(basename(p.path))) ||
      new Set(value.headers.map(p => p.path)).size !== value.headers.length) throw new Error("Exact pinned build PDFium runtime inputs required");
  const directories = [dirname(value.library.path), value.includeDirectory];
  if (within(directories[0]!, directories[1]!) || within(directories[1]!, directories[0]!) ||
      [value.launcher.path, ...directories].some(p => [capability.source_root, capability.build_root!, ...writeRoots].some(root => within(root, p) || within(p, root)))) throw new Error("Build PDFium runtime must remain outside candidate source and writable roots");
}
export async function verifyContainerBuildPdfiumFiles(value: ContainerBuildPdfium): Promise<void> {
  for (const [directory, names] of [[dirname(value.library.path), [basename(value.library.path)]], [value.includeDirectory, value.headers.map(p => basename(p.path))]] as [string, string[]][]) {
    await physicalPath(directory);
    const info = await lstat(directory);
    if (!info.isDirectory() || canonical((await readdir(directory)).sort()) !== canonical([...names].sort())) throw new Error("Build PDFium directory inventory differs from its immutable pins");
  }
  for (const p of [value.launcher, value.library, ...value.headers]) {
    await physicalPath(p.path); const info = await lstat(p.path);
    const limit = p === value.library ? 1024 * 1024 * 1024 : p === value.launcher ? 16 * 1024 * 1024 : 1024 * 1024;
    if (!info.isFile() || info.size > limit || p === value.launcher && !(info.mode & 0o111) || await fileHash(p.path) !== p.sha256) throw new Error("Build PDFium runtime pin changed or exceeds its bound");
  }
}
export interface LinuxRawUnixBoundary {
  schema: "environments-harness.linux-native-unix-boundary.v1";
  native_mode: "ProxyRouted-ProxyOnly"; allow_all_unix_syscalls: true;
  filesystem_socket_authority: "one-selected-external-helper-endpoint";
  selected_socket: "/run/gptgrep-decisions/decisions.sock";
  tool_namespace_observation: InputPin; filesystem_socket_inventory: InputPin;
  network_denial_evidence: InputPin; parent_api_proxy_separation: InputPin;
}
export interface ContainerDecisionsHelper {
  attestation: InputPin; namespacePlanSha256: string; nativeProfile: InputPin;
  nativeProfileId: string; nativeBinary: InputPin; clientControl: InputPin;
  allowedUnixSocket: "/run/gptgrep-decisions/decisions.sock";
  unixSocketScope: "linux-container-fs-netns-v1"; linuxRawUnixBoundary: LinuxRawUnixBoundary;
}
export interface StorageAdmission {
  binding: string;
  sha256: string;
  container?: {
    commandId: "qualification" | "preflight" | "reader" | "c4-qualification" | "c4-engineer" | "c4-build" | "c4-agy";
    admissionClass: "bounded-qualification" | "reader" | "c4-actor-qualification" | "c4-actor";
    actorCapability?: ContainerActorCapability;
    buildPdfium?: ContainerBuildPdfium;
    containerBindingSha256: string;
    parentHostBindingSha256: string;
    ownerTemplateSha256: string;
    reservationPeakBytes?: string;
    filesystemPolicyName?: string;
    filesystemPolicySha256?: string;
    acceptance: { path: string; sha256: string };
    immutableJob: { path: string; sha256: string };
    experimentArm: "baseline" | "treatment";
    modelProfile: { path: string; sha256: string };
    modelReadRoots: string[];
    decisionsHelper?: ContainerDecisionsHelper;
  };
}
interface AllowedPath { path: string; kind: "file" | "directory"; access: "read" | "write"; purpose: string }
const exactHash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
function allowedPaths(value: unknown): AllowedPath[] {
  if (!Array.isArray(value) || !value.length || value.length > 128) throw new Error("Container command has no bounded exact allowed paths");
  const seen = new Set<string>();
  for (const row of value) {
    if (!row || Object.keys(row).sort().join(",") !== "access,kind,path,purpose" ||
        typeof row.path !== "string" || !isAbsolute(row.path) || resolve(row.path) !== row.path || row.path === "/" ||
        !["file", "directory"].includes(row.kind) || !["read", "write"].includes(row.access) ||
        typeof row.purpose !== "string" || !row.purpose || seen.has(row.path)) throw new Error("Malformed or ambiguous container allowed path");
    seen.add(row.path);
  }
  return value;
}
function accessAt(rows: AllowedPath[], path: string): "read" | "write" | null {
  const matches = rows.filter(row => row.kind === "file" ? path === row.path : within(row.path, path));
  matches.sort((a, b) => b.path.length - a.path.length);
  return matches[0]?.access ?? null;
}
async function guestPin(value: any, guestPath = value?.path): Promise<void> {
  if (!value || Object.keys(value).sort().join(",") !== "path,sha256" || typeof value.path !== "string" || !isAbsolute(value.path) ||
      typeof guestPath !== "string" || !isAbsolute(guestPath) || !exactHash(value.sha256)) throw new Error("Exact container input pin required");
  await physicalPath(guestPath);
  if (await fileHash(guestPath) !== value.sha256) throw new Error("Container input pin changed");
}
const HELPER_ENV = ["GPTGREP_DECISIONS_IPC_CONTROL", "GPTGREP_DECISIONS_IPC_SHA256"] as const;
function helperClosed(value: any, fields: string[], label: string): void {
  if (!value || typeof value !== "object" || Array.isArray(value) || canonical(Object.keys(value).sort()) !== canonical([...fields].sort())) throw new Error(`Closed decisions helper ${label} required`);
}
function helperPin(value: any): void {
  helperClosed(value, ["path", "sha256"], "pin");
  if (typeof value.path !== "string" || !isAbsolute(value.path) || resolve(value.path) !== value.path || !exactHash(value.sha256)) throw new Error("Exact decisions helper pin required");
}
/** Pure joins for contract fixtures. Host ownership and native route probes are separate evidence. */
export function validateDecisionsHelperContract(value: any, base: any, ready: any, client: any,
  environment: Record<string, string | undefined> = process.env): ContainerDecisionsHelper | undefined {
  const selected = base.decisions_helper, command = value.decisions_helper;
  const wanted = selected && (value.command_id === "reader" || value.command_id === "qualification");
  if (!wanted) {
    if (command !== undefined || HELPER_ENV.some(name => environment[name] !== undefined)) throw new Error("Decisions helper is outside its selected owner command");
    return undefined;
  }
  helperClosed(selected, ["schema", "mode", "namespace_plan_sha256", "startup_input", "client_control", "attestation", "native_binary", "native_profile", "native_profile_id", "allowed_unix_socket", "unix_socket_scope", "helper_generation", "parent_identity", "socket_identity", "route_qualification", "reused_shipped_cli_probe"], "binding");
  if (canonical(command) !== canonical(selected) || selected.schema !== "environments-harness.container-decisions-helper-binding.v1" ||
      selected.namespace_plan_sha256 !== value.host_namespace_plan_sha256 || selected.namespace_plan_sha256 !== base.host_namespace_plan_sha256 ||
      base.experiment_arm !== "treatment" || base.actor_capability !== undefined ||
      selected.allowed_unix_socket !== "/run/gptgrep-decisions/decisions.sock" || selected.unix_socket_scope !== "linux-container-fs-netns-v1" ||
      selected.client_control?.path !== "/run/gptgrep-decisions/client.json" ||
      canonical(selected.native_profile) !== canonical(base.model_profile) || selected.native_profile_id !== base.filesystem_policy_name ||
      environment[HELPER_ENV[0]] !== selected.client_control.path || environment[HELPER_ENV[1]] !== selected.client_control.sha256) throw new Error("Decisions helper differs from its current command, profile or paired environment");
  for (const pin of [selected.startup_input, selected.client_control, selected.native_binary, selected.native_profile]) helperPin(pin);
  helperClosed(client, ["schema_version", "instance", "task_binding_sha256", "socket_parent", "parent_identity", "socket_identity", "server", "server_uid", "server_gid", "model", "provider_disabled", "budget"], "client control");
  if (client.schema_version !== "gptgrep.decisions-helper-client.v1" || !/^[a-f0-9]{32}$/.test(client.instance) ||
      client.task_binding_sha256 !== selected.namespace_plan_sha256 || client.socket_parent !== "/run/gptgrep-decisions" ||
      canonical(client.server) !== canonical(selected.helper_generation) || canonical(client.parent_identity) !== canonical(selected.parent_identity) ||
      canonical(client.socket_identity) !== canonical(selected.socket_identity) || client.parent_identity.mode !== 0o700 ||
      client.parent_identity.uid !== base.custody_uid || client.socket_identity.uid !== base.custody_uid ||
      client.parent_identity.uid !== client.server_uid || client.socket_identity.uid !== client.server_uid ||
      client.parent_identity.gid !== client.server_gid || client.socket_identity.gid !== client.server_gid || (client.socket_identity.mode & 0o077) !== 0) throw new Error("Decisions helper client belongs to another socket or process generation");
  for (const node of [client.parent_identity, client.socket_identity]) {
    helperClosed(node, ["device", "inode", "uid", "gid", "mode"], "node identity");
    if (Object.values(node).some(n => !Number.isSafeInteger(n) || (n as number) < 0)) throw new Error("Decisions helper node identity is not exact");
  }
  helperClosed(client.server, ["pid", "start_ticks", "pid_namespace", "cgroup", "boot_id"], "server generation");
  helperClosed(client.budget, ["run_id", "plan_digest", "deadline_unix_ms", "max_submissions", "clock_anchor"], "public budget");
  if (!exactHash(client.budget.plan_digest) || !Number.isSafeInteger(client.budget.deadline_unix_ms) ||
      !Number.isSafeInteger(client.budget.max_submissions) || client.budget.max_submissions < 1 || client.budget.max_submissions > 200) throw new Error("Decisions helper public budget differs");
  if (selected.mode === "qualification") {
    if (value.admission_class !== "bounded-qualification" || value.command_id !== "qualification" || client.provider_disabled !== true ||
        selected.attestation !== null || selected.route_qualification !== null || selected.reused_shipped_cli_probe !== null || ready !== null ||
        selected.startup_input.sha256 !== selected.client_control.sha256) throw new Error("Keyless qualification intent cannot inherit live ready authority");
    return undefined;
  }
  if (selected.mode !== "reader" || value.admission_class !== "reader" || value.command_id !== "reader" || client.provider_disabled !== false) throw new Error("Live helper authority is reader-only");
  for (const pin of [selected.attestation, selected.route_qualification, selected.reused_shipped_cli_probe]) helperPin(pin);
  helperClosed(ready, ["schema", "namespace_plan", "helper", "endpoint", "caller", "owner", "budget", "native", "privacy", "lifecycle"], "ready attestation");
  helperClosed(ready.helper, ["binary", "source_manifest", "server_control", "readiness", "client_control", "generation"], "ready helper");
  helperClosed(ready.endpoint, ["host_parent", "guest_parent", "guest_client_control", "socket_name", "parent_identity", "socket_identity", "mount_access", "disjoint_collection_roots"], "ready endpoint");
  helperClosed(ready.caller, ["container_id", "container_generation", "init", "scope", "native_inventory", "shipped_cli_probe"], "ready caller");
  helperClosed(ready.owner, ["binding", "lease_id", "lease_owner_pid", "lease_owner_start_ticks", "deadline_monotonic_ns"], "ready owner");
  helperClosed(ready.budget, ["private_control", "run_id", "plan_digest", "deadline_unix_ms", "max_submissions"], "ready budget");
  helperClosed(ready.native, ["codex_binary", "profile", "profile_id", "allowed_unix_socket", "tool_tcp_egress", "qualification", "linux_raw_unix_boundary"], "ready native route");
  helperClosed(ready.privacy, ["key_delivery", "provider_key_in_argv_or_environments", "key_files_retained", "private_host_paths_not_mounted", "dumpable", "core_soft_and_hard", "no_new_privs", "private_fds_cloexec", "actual_pre_key_evidence"], "ready privacy");
  helperClosed(ready.lifecycle, ["phase", "original_deadline_monotonic_ns", "helper_terminal", "socket_cleanup"], "ready lifecycle");
  const generation = JSON.parse(JSON.stringify(base.container));
  if (!generation.security?.resources) throw new Error("Current helper container security generation is unavailable");
  delete generation.security.resources.host_mem_available_bytes; delete generation.security.resources.host_observation_sha256;
  if (ready.schema !== "environments-harness.container-decisions-helper-attestation.v1" || ready.namespace_plan?.sha256 !== selected.namespace_plan_sha256 ||
      selected.startup_input.sha256 !== selected.attestation.sha256 || ready.helper.client_control?.sha256 !== selected.client_control.sha256 ||
      canonical(ready.helper.generation) !== canonical(client.server) || ready.caller.container_id !== value.container_id ||
      ready.caller.container_generation !== digest(generation) || ready.caller.scope !== "selected-container-cgroup-descendants" || ready.caller.init?.pid !== base.container.init_host_pid ||
      String(ready.caller.init?.start_ticks) !== base.container.init_start_ticks || ready.caller.init?.boot_id !== base.container.boot_id ||
      ready.owner.binding?.sha256 !== base.parent_host_binding_sha256 || ready.owner.deadline_monotonic_ns !== base.parent_deadline_monotonic_ns ||
      ready.lifecycle?.phase !== "ready" || ready.lifecycle?.helper_terminal !== false ||
      ready.lifecycle?.original_deadline_monotonic_ns !== base.parent_deadline_monotonic_ns ||
      ready.lifecycle.socket_cleanup !== "defer-until-owner-proves-helper-terminal" ||
      ready.privacy.key_delivery !== "encrypted-stdin-only" || ready.privacy.provider_key_in_argv_or_environments !== false || ready.privacy.key_files_retained !== false ||
      ready.privacy.private_host_paths_not_mounted !== true || ready.privacy.dumpable !== 0 || ready.privacy.core_soft_and_hard !== 0 || ready.privacy.no_new_privs !== 1 || ready.privacy.private_fds_cloexec !== true ||
      ready.endpoint.guest_parent !== "/run/gptgrep-decisions" || ready.endpoint.guest_client_control !== selected.client_control.path ||
      ready.endpoint.socket_name !== "decisions.sock" || ready.endpoint.mount_access !== "read-only" || ready.endpoint.disjoint_collection_roots !== true ||
      canonical(ready.endpoint.parent_identity) !== canonical(client.parent_identity) || canonical(ready.endpoint.socket_identity) !== canonical(client.socket_identity) ||
      canonical(ready.native.codex_binary) !== canonical(selected.native_binary) || canonical(ready.native.profile) !== canonical(selected.native_profile) ||
      ready.native.profile_id !== selected.native_profile_id || ready.native.allowed_unix_socket !== selected.allowed_unix_socket || ready.native.tool_tcp_egress !== "deny" ||
      canonical(ready.native.qualification) !== canonical(selected.route_qualification) || canonical(ready.caller.shipped_cli_probe) !== canonical(selected.reused_shipped_cli_probe) ||
      ["run_id", "plan_digest", "deadline_unix_ms", "max_submissions"].some(key => ready.budget?.[key] !== client.budget[key])) throw new Error("Live ready attestation is not the current generation with its separately reused route qualification");
  const boundary = ready.native.linux_raw_unix_boundary;
  helperClosed(boundary, ["schema", "native_mode", "allow_all_unix_syscalls", "filesystem_socket_authority", "selected_socket", "tool_namespace_observation", "filesystem_socket_inventory", "network_denial_evidence", "parent_api_proxy_separation"], "Linux Unix boundary");
  if (boundary.schema !== "environments-harness.linux-native-unix-boundary.v1" || boundary.native_mode !== "ProxyRouted-ProxyOnly" || boundary.allow_all_unix_syscalls !== true ||
      boundary.filesystem_socket_authority !== "one-selected-external-helper-endpoint" || boundary.selected_socket !== selected.allowed_unix_socket) throw new Error("Qualified Linux Unix socket boundary differs");
  for (const key of ["tool_namespace_observation", "filesystem_socket_inventory", "network_denial_evidence", "parent_api_proxy_separation"]) helperPin(boundary[key]);
  return { attestation: selected.attestation, namespacePlanSha256: selected.namespace_plan_sha256, nativeProfile: selected.native_profile,
    nativeProfileId: selected.native_profile_id, nativeBinary: selected.native_binary, clientControl: selected.client_control,
    allowedUnixSocket: selected.allowed_unix_socket, unixSocketScope: selected.unix_socket_scope, linuxRawUnixBoundary: boundary };
}
async function containerDecisionsHelper(value: any, base: any): Promise<ContainerDecisionsHelper | undefined> {
  if (!value.decisions_helper) return validateDecisionsHelperContract(value, base, null, null);
  const helper = value.decisions_helper;
  for (const pin of [helper.client_control, ...(helper.attestation ? [helper.attestation] : [])]) {
    if ((await lstat(pin.path)).size > 262144) throw new Error("Decisions helper control exceeds its owner byte bound");
  }
  await guestPin(helper.client_control); await guestPin(helper.native_binary); await guestPin(helper.native_profile);
  let ready = null;
  if (helper.mode === "reader") { await guestPin(helper.attestation); ready = await readJson(helper.attestation.path); }
  const client = await readJson(helper.client_control.path);
  const parent = await lstat("/run/gptgrep-decisions"), socket = await lstat("/run/gptgrep-decisions/decisions.sock"), control = await lstat(helper.client_control.path);
  const node = (info: Awaited<ReturnType<typeof lstat>>) => ({ device: Number(info.dev), inode: Number(info.ino), uid: info.uid, gid: info.gid, mode: info.mode & 0o7777 });
  if (!parent.isDirectory() || parent.isSymbolicLink() || !socket.isSocket() || socket.isSymbolicLink() || !control.isFile() || control.isSymbolicLink() || control.nlink !== 1 ||
      (control.mode & 0o777) !== 0o600 || control.uid !== base.custody_uid || canonical(node(parent)) !== canonical(client.parent_identity) || canonical(node(socket)) !== canonical(client.socket_identity)) throw new Error("Actual guest helper socket/control custody changed");
  return validateDecisionsHelperContract(value, base, ready, client);
}
async function containerCommand(value: any, bindingPath: string, outputs: string[]): Promise<NonNullable<StorageAdmission["container"]>> {
  const plan = value.plan, command = plan?.command;
  const observed = await processCommandIdentity();
  if (!["qualification", "preflight", "reader", "c4-qualification", "c4-engineer", "c4-build", "c4-agy"].includes(value.command_id) ||
      !["bounded-qualification", "reader", "c4-actor-qualification", "c4-actor"].includes(value.admission_class) ||
      value.custody_mode !== "0600" || value.mount_access !== "read-only" || value.custody_uid !== process.getuid?.() ||
      canonical(value.exact_owner_injected_env_names) !== canonical(["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST", ...(value.decisions_helper ? HELPER_ENV : [])]) ||
      !command || command.executable !== observed.executable || command.argv_sha256 !== observed.argvSha256 ||
      !exactHash(command.executable_sha256) || !isAbsolute(plan.cwd ?? "") || resolve(plan.cwd) !== await realpath(process.cwd()) ||
      typeof plan.run_id !== "string" || !plan.run_id || !exactHash(plan.workload_sha256)) throw new Error("Process is outside the exact guest command binding");
  const info = await lstat(bindingPath);
  if ((info.mode & 0o777) !== 0o600 || info.uid !== value.custody_uid || await fileHash(command.executable) !== command.executable_sha256) throw new Error("Guest command custody or executable changed");
  await guestPin(value.container_binding, value.container_binding_guest_path);
  const base = await readJson(value.container_binding_guest_path);
  const baseInfo = await lstat(value.container_binding_guest_path);
  if (base.schema !== "environments-harness.container-storage-binding.v1" || base.command_binding_required !== true ||
      base.custody_mode !== "0600" || base.mount_access !== "read-only" || base.custody_uid !== value.custody_uid ||
      (baseInfo.mode & 0o777) !== 0o600 || baseInfo.uid !== value.custody_uid || base.admission_class !== value.admission_class ||
      base.container?.id !== value.container_id || !exactHash(value.container_id) ||
      ["owner_template_sha256", "host_namespace_plan_sha256", "parent_host_binding_sha256"].some(key => !exactHash(value[key]) || base[key] !== value[key]) ||
      !/^\d+$/.test(value.parent_deadline_monotonic_ns ?? "") || value.parent_deadline_monotonic_ns !== base.parent_deadline_monotonic_ns ||
      value.reservation?.mode !== "additional-container-reservation" || value.reservation.transport_exit_is_terminal !== false ||
      !exactHash(value.reservation.sha256) || !exactHash(value.reservation.parent_plan_digest) ||
      !["baseline", "treatment"].includes(base.experiment_arm) ||
      value.admission_class === "reader" && (!exactHash(base.sandbox_privacy_qualification_sha256) || !exactHash(base.auth_projection_sha256))) throw new Error("Guest command lacks its exact owner namespace and subreservation ancestry");
  if (canonical(base.actor_capability ?? null) !== canonical(value.actor_capability ?? null)) throw new Error("C4 command and container capability differ");
  validateContainerCapability(value.admission_class, value.command_id, base.auth_projection_sha256, base.actor_capability);
  await guestPin(value.acceptance); await guestPin(value.immutable_job); await guestPin(base.model_profile);
  if (canonical(base.build_pdfium ?? null) !== canonical(value.build_pdfium ?? null)) throw new Error("Build PDFium command and container inputs differ");
  if (base.build_pdfium !== undefined) {
    if (value.admission_class !== "c4-actor" || value.command_id !== "c4-build") throw new Error("PDFium inputs belong only to an actual build role");
    validateContainerBuildPdfium(base.build_pdfium, base.actor_capability, allowedPaths(base.allowed_paths).filter(row => row.access === "write").map(row => row.path));
    const job = await readJson(value.immutable_job.path);
    if (canonical(job.build?.pdfium ?? null) !== canonical(base.build_pdfium)) throw new Error("Build PDFium inputs differ from the immutable actor job");
    for (const target of [dirname(base.build_pdfium.library.path), base.build_pdfium.includeDirectory, base.build_pdfium.launcher.path, base.build_pdfium.library.path, ...base.build_pdfium.headers.map((pin: InputPin) => pin.path)]) {
      const choices = base.mounts.filter((row: any) => row.template.kind === "directory" ? within(row.template.guest_target, target) : row.template.guest_target === target)
        .sort((a: any, b: any) => b.template.guest_target.length - a.template.guest_target.length);
      if (choices.length ? choices[0].template.access !== "read" : target !== base.build_pdfium.launcher.path) throw new Error("Build PDFium inputs require read-only runtime storage");
    }
    await verifyContainerBuildPdfiumFiles(base.build_pdfium);
  } else if (value.admission_class === "c4-actor" && base.actor_capability?.role === "build" && (await readJson(value.immutable_job.path)).build?.pdfium !== undefined) throw new Error("Immutable build PDFium inputs lack owner authority");
  if (value.admission_class === "c4-actor" && !exactHash(base.sandbox_privacy_qualification_sha256)) throw new Error("C4 lacks its actual role privacy qualification");
  if (base.actor_capability) await guestPin(base.actor_capability.source_manifest);
  if ((value.admission_class === "reader" || value.admission_class === "c4-actor" && base.actor_capability?.role === "engineer") && (!process.env.CODEX_HOME || base.model_profile.path !== resolve(process.env.CODEX_HOME, "config.toml"))) throw new Error("Reader profile is not the actual broker-native home configuration");
  if (!Array.isArray(base.model_read_roots) || !base.model_read_roots.length || base.model_read_roots.some((path: unknown) => typeof path !== "string" || !isAbsolute(path) || path === "/")) throw new Error("Guest model read roots are not exact");
  const selected = allowedPaths(value.allowed_paths), admitted = allowedPaths(base.allowed_paths);
  for (const output of outputs) {
    await physicalPath(output);
    if (accessAt(selected, resolve(output)) !== "write" || accessAt(admitted, resolve(output)) !== "write") throw new Error("Output lies outside the narrowed guest write allowance");
  }
  const decisionsHelper = await containerDecisionsHelper(value, base);
  return { commandId: value.command_id, admissionClass: value.admission_class, containerBindingSha256: value.container_binding.sha256,
    parentHostBindingSha256: value.parent_host_binding_sha256, ownerTemplateSha256: value.owner_template_sha256,
    acceptance: value.acceptance, immutableJob: value.immutable_job, experimentArm: base.experiment_arm,
    modelProfile: base.model_profile, modelReadRoots: base.model_read_roots,
    ...(decisionsHelper ? { decisionsHelper } : {}),
    ...(base.build_pdfium ? { buildPdfium: base.build_pdfium } : {}),
    ...(base.filesystem_policy_name ? { filesystemPolicyName: base.filesystem_policy_name, filesystemPolicySha256: base.filesystem_policy_sha256 } : {}),
    ...(base.actor_capability ? { actorCapability: base.actor_capability, reservationPeakBytes: value.reservation.peak_bytes } : {}) };
}

/** The owner launcher is the authority. Guest commands never inherit broad host target grants. */
export async function requireStorage(...outputs: string[]): Promise<StorageAdmission> {
  const binding = process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING;
  const expected = process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST;
  if (!binding || !expected || await fileHash(binding) !== expected) throw new Error("An exact owner-admitted external storage run is required");
  const value = await readJson(binding);
  if (value.schema === "environments-harness.container-command-binding.v1") {
    const container = await containerCommand(value, binding, outputs);
    return { binding: await realpath(binding), sha256: expected, container };
  }
  if (value.allowed_paths !== undefined || typeof value.schema === "string" && value.schema.startsWith("environments-harness.container-")) throw new Error("A container requires a fresh per-command binding; host target fallback is forbidden");
  const targets = value.targets ?? value.plan?.targets;
  if (!targets) throw new Error("Storage binding has no verified targets");
  const roots = [targets.output, targets.downloads, targets.test_data].filter((v): v is string => typeof v === "string");
  for (const output of outputs) {
    await physicalPath(output);
    if (!roots.some(root => within(root, output))) throw new Error(`Output lies outside the admitted run: ${output}`);
  }
  return { binding: await realpath(binding), sha256: expected };
}

export function boundedInteger(value: number, min: number, max: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < min || value > max) throw new Error(`Invalid ${label}`);
  return value;
}
