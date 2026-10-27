/** One owner-admitted Linux actor. The guest receives a single role, never the campaign or judge. */
import { constants } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { readExactJson } from "../../benchmark/src/availability";
import { validateMutationPaths } from "./mutation-paths";
import { actorGit, validateActorGitRoute, validateInitialActorGitConfig, verifyActorGitStore, type ActorGitRoute } from "./actor-git";
import { lstat, readFile, readlink, mkdir, open } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { canonical, digest, fileHash, physicalPath, requireStorage, sha256, validateContainerActorCapability,
  within, validateContainerBuildPdfium, verifyContainerBuildPdfiumFiles, type ContainerBuildPdfium, type ContainerActorCapability, type StorageAdmission } from "../../benchmark/src/io";
import { executeOwnerCommand, type ProcessCommand, type ProcessObservation } from "../../benchmark/src/process";
import { runNative, type ReasoningEffort } from "../../benchmark/src/native";
import { closed, boundedText, validateDevFeedback, type DevFeedback } from "./proposals";
import { immutable, validateBuildEnvironment, OUTPUT_SCHEMA, validateEngineerOutput, validateCodexCapture, verifyReceiptPin, inspectLinuxBinary, type EngineerOutput } from "./actor-guest-contracts";
import type { ActorPolicy } from "./actor-contracts";
import type { Pin } from "./competition";

const integer = (v: unknown, low = 1, high = Number.MAX_SAFE_INTEGER): v is number => Number.isSafeInteger(v) && (v as number) >= low && (v as number) <= high;
const exactPath = (p: unknown): p is string => typeof p === "string" && isAbsolute(p) && resolve(p) === p && p !== "/" && !/[\0\r\n]/.test(p);
export interface ContainerActorJob {
  schema: "gptgrep.container-actor-job.v1";
  capability: ContainerActorCapability; git: ActorGitRoute;
  outputRoot: string; runtimeRoot: string | null; deadlineAt: number; maxEvidenceBytes: number;
  engineer: null | { binary: Pin; nativeCapture: Pin; instructions: Pin; feedback: DevFeedback[];
    model: string; effort: ReasoningEffort; serviceTier: string; permissionProfile: string;
    timeoutMs: number; maxTraceBytes: number; maxResultBytes: number; toolCalls: number };
  build: null | { commands: ProcessCommand[]; environment: Record<string, string>; binaryPath: string; peakBytes: number; targetArch: "x64" | "arm64"; pdfium?: ContainerBuildPdfium };
}
export interface ContainerActorReceipt {
  schema: "gptgrep.container-actor-result.v1"; job: Pin; capability: ContainerActorCapability;
  commandBinding: Pin; ownerTemplateSha256: string; parentHostBindingSha256: string;
  status: "candidate_source_ready" | "artifact_built" | "failed"; error: string | null;
  candidateRevision: string; changedPaths: string[]; diffSha256: string | null;
  hypothesis: EngineerOutput["hypothesis"] | null; native: Pin | null; trace: Pin | null;
  checks: ProcessObservation[]; sandboxCommands: ProcessCommand[]; binary: Pin | null; startedAt: string; finishedAt: string;
  runtimeAcceptance: false; releaseAcceptance: false;
}
export function validateContainerActorJob(job: ContainerActorJob): void {
  closed(job, ["schema", "capability", "git", "outputRoot", "runtimeRoot", "deadlineAt", "maxEvidenceBytes", "engineer", "build"], "single container actor job");
  validateContainerActorCapability(job.capability);
  validateMutationPaths(job.capability.mutation_paths);
  validateActorGitRoute(job);
  if (job.schema !== "gptgrep.container-actor-job.v1" || job.capability.role === "agy") throw new Error("Native AGY owner authentication and sandbox qualification is unavailable");
  if (!exactPath(job.outputRoot) || within(job.capability.source_root, job.outputRoot) || !integer(job.deadlineAt) || !integer(job.maxEvidenceBytes, 65536, 512 * 1024 * 1024)) throw new Error("Actor output, runtime, evidence or original deadline differs");
  if (job.capability.role === "engineer") {
    if (job.build !== null || !job.engineer || !exactPath(job.runtimeRoot) || !within(job.capability.source_root, job.runtimeRoot) || job.runtimeRoot === job.capability.source_root || within(job.capability.git_directory, job.runtimeRoot)) throw new Error("An engineer cannot inherit build commands");
    const e = job.engineer;
    if (canonical(e.binary) !== canonical(job.capability.native_binary)) throw new Error("Engineer binary differs from its exact no-model role qualification");
    closed(e, ["binary", "nativeCapture", "instructions", "feedback", "model", "effort", "serviceTier", "permissionProfile", "timeoutMs", "maxTraceBytes", "maxResultBytes", "toolCalls"], "container engineer route");
    for (const value of [e.model, e.effort, e.serviceTier, e.permissionProfile]) boundedText(value, "explicit native choice", 256);
    if (!["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].includes(e.effort) || e.permissionProfile.startsWith(":") ||
        !integer(e.timeoutMs, 1, 1800000) || !integer(e.maxTraceBytes, 1024, 64 * 1024 * 1024) || !integer(e.maxResultBytes, 1024, 2 * 1024 * 1024) ||
        e.maxTraceBytes + e.maxResultBytes + 2 * 1024 * 1024 > job.maxEvidenceBytes || !integer(e.toolCalls, 1, 256) || !Array.isArray(e.feedback) || e.feedback.length > 64) throw new Error("Engineer limits are outside the frozen job");
    e.feedback.forEach(validateDevFeedback);
  } else {
    if (job.engineer !== null || !job.build || job.runtimeRoot !== null) throw new Error("A build cannot inherit native model or authentication routes");
    const b = job.build;
    closed(b, ["commands", "environment", "binaryPath", "peakBytes", "targetArch", ...(b.pdfium === undefined ? [] : ["pdfium"])], "container candidate build");
    if (!integer(b.peakBytes) || !exactPath(b.binaryPath) || !within(job.capability.build_root!, b.binaryPath) || !["x64", "arm64"].includes(b.targetArch) || basename(b.binaryPath) !== "gptgrep" || within(job.capability.build_root!, job.outputRoot) || within(job.outputRoot, job.capability.build_root!) || !Array.isArray(b.commands) || !b.commands.length || b.commands.length > 16) throw new Error("Exact candidate build artifact, measured peak and checks required");
    validateBuildEnvironment(b.environment, job.capability.build_root!, job.capability.build_root!);
    if (b.pdfium !== undefined) validateContainerBuildPdfium(b.pdfium, job.capability, [job.outputRoot]);
    for (const c of b.commands) {
      closed(c, ["executable", "inputs", "argv", "cwd", "timeoutMs", "maxOutputBytes"], "container build command");
      if (c.cwd !== job.capability.source_root || !integer(c.timeoutMs, 1, 7200000) || !integer(c.maxOutputBytes, 1, 8 * 1024 * 1024) ||
          !Array.isArray(c.argv) || c.argv.length > (b.pdfium ? 115 : 128) || c.argv.some(a => typeof a !== "string" || /[\0\r\n]/.test(a)) || !Array.isArray(c.inputs)) throw new Error("Build command exceeds its exact source or finite limits");
      if (basename(c.executable.path) === "cargo" && !c.argv.includes("--offline")) throw new Error("Candidate Cargo commands must retain offline dependency behavior");
    }
  }
}
/** The owner separately verifies namespace, mount, terminal and reservation generations. */
export function validateActorAdmission(job: ContainerActorJob, admission: StorageAdmission): NonNullable<StorageAdmission["container"]> {
  validateContainerActorJob(job);
  const c = admission.container;
  if (!c || c.admissionClass !== "c4-actor" || c.commandId !== `c4-${job.capability.role}` || canonical(c.actorCapability) !== canonical(job.capability)) throw new Error("Host storage or reader authority cannot execute a C4 actor");
  if (canonical(c.buildPdfium ?? null) !== canonical(job.build?.pdfium ?? null)) throw new Error("Build PDFium runtime differs from its exact owner inputs");
  if (job.capability.role === "engineer" && (c.modelProfile.path !== resolve(process.env.CODEX_HOME ?? "/", "config.toml") || job.engineer!.permissionProfile !== c.filesystemPolicyName)) throw new Error("Engineer profile must be the actual outer native home config");
  if (job.capability.role === "build" && (typeof c.reservationPeakBytes !== "string" || !/^\d+$/.test(c.reservationPeakBytes) || BigInt(job.build!.peakBytes) > BigInt(c.reservationPeakBytes))) throw new Error("Candidate build peak exceeds its actual owner reservation");
  return c;
}
const git = actorGit;
export async function verifyActorSource(job: ContainerActorJob, expectedRevision: string, before: boolean): Promise<void> {
  const c = job.capability;
  await physicalPath(c.source_root); await physicalPath(c.git_directory); await verifyReceiptPin(c.source_manifest);
  const info = await lstat(c.git_directory);
  await verifyActorGitStore(job);
  if (before) await validateInitialActorGitConfig(job);
  if (!info.isDirectory() || info.isSymbolicLink() || (await git(job, "rev-parse", "HEAD")).trim() !== expectedRevision) throw new Error("Actor requires an independent exact Git store and revision");
  if (before && (await git(job, "rev-list", "--count", "HEAD")).trim() !== "1" || (await git(job, "fsck", "--full", "--no-reflogs", "--unreachable")).trim()) throw new Error("Actor Git store must contain only the selected public commit before dispatch");
  const refs = (await git(job, "for-each-ref", "--format=%(refname)")).trim().split("\n").filter(Boolean);
  if (refs.length !== 1 || !refs[0]!.startsWith("refs/heads/") || (await git(job, "remote")).trim()) throw new Error("Actor Git store contains extra refs, Notes or remote routing");
  const statusArgs = ["status", "--porcelain=v1", "--untracked-files=all", "--ignored=matching", "--", "."];
  if (!before && c.role === "engineer") statusArgs.push(`:(exclude)${relative(c.source_root, job.runtimeRoot!)}`);
  if ((await git(job, ...statusArgs)).length) throw new Error("Actor source contains changed, ignored or untracked input material");
  const manifest = await readExactJson<any>(c.source_manifest.path);
  closed(manifest, ["schema", "revision", "files"], "public source manifest");
  if (manifest.schema !== "gptgrep.actor-public-source.v1" || manifest.revision !== c.source_revision || !Array.isArray(manifest.files) || !manifest.files.length || manifest.files.length > 10000) throw new Error("Exact public source-only inventory required");
  const names: string[] = [];
  for (const row of manifest.files) {
    closed(row, ["path", "sha256"], "public source file");
    if (typeof row.path !== "string" || !/^[A-Za-z0-9_.][A-Za-z0-9_./-]*$/.test(row.path) || row.path.split("/").some((p: string) => ["", ".", "..", ".git", "auth.json", "credentials.json", ".netrc", ".npmrc", ".pypirc"].includes(p) || /^\.env(?:\..*)?$|\.env$/i.test(p)) || !/^[a-f0-9]{64}$/.test(row.sha256)) throw new Error("Public source inventory contains an unsafe path");
    if (job.runtimeRoot !== null && within(job.runtimeRoot, join(c.source_root, row.path))) throw new Error("Native runtime cannot shadow a tracked public source path");
    names.push(row.path);
    if (before || c.role !== "engineer" || !c.mutation_paths.includes(row.path)) {
      const p = join(c.source_root, row.path); await physicalPath(p);
      const source = await lstat(p);
      if (!source.isFile() || source.isSymbolicLink() || await fileHash(p) !== row.sha256) throw new Error("Public source bytes changed outside the allowed candidate delta");
    }
  }
  if (new Set(names).size !== names.length || canonical([...names].sort()) !== canonical((await git(job, "ls-tree", "-r", "--name-only", c.source_revision)).trim().split("\n").sort())) throw new Error("Public inventory differs from the exact source tree");
}
async function engineer(job: ContainerActorJob, admission: NonNullable<StorageAdmission["container"]>, receipt: ContainerActorReceipt): Promise<Partial<ContainerActorReceipt>> {
  const e = job.engineer!, cap = job.capability;
  if (e.nativeCapture.path !== fileURLToPath(new URL("../../benchmark/src/native.ts", import.meta.url))) throw new Error("Engineer must use the actual imported native capture source");
  for (const pin of [e.binary, e.nativeCapture, e.instructions]) await verifyReceiptPin(pin, pin === e.binary);
  const instructions = await readFile(e.instructions.path, "utf8");
  if (Buffer.byteLength(instructions) > 16384) throw new Error("Engineering instructions exceed their bound");
  const prompt = [instructions,
    "Create one substantive general improvement from this parent. Commit only the permitted source paths in this independent Git store. Do not read judge, gold, other candidates, credentials or host controls. Do not start nested agents, providers, builds, benchmarks or releases. Return the actual clean commit and requested hypothesis. Automatic retries and second candidates are unavailable.",
    JSON.stringify({ candidateId: cap.candidate_id, parentRevision: cap.source_revision, mutationPaths: cap.mutation_paths,
      developmentFeedback: e.feedback.map(({ id, category, candidateRevision, observation }) => ({ id, category, candidateRevision, observation })), compositionAllowed: false })].join("\n\n");
  if (Buffer.byteLength(prompt) > 192 * 1024) throw new Error("Engineering projection exceeds its bound");
  const options = { codexBinary: e.binary.path, cwd: cap.source_root, model: e.model, effort: e.effort, serviceTier: e.serviceTier,
    timeoutMs: e.timeoutMs, tracePath: join(job.outputRoot, "native-trace.jsonl"), readableRoots: admission.modelReadRoots,
    runtimeDirectory: job.runtimeRoot!, permissionProfile: e.permissionProfile, input: prompt, outputSchema: OUTPUT_SCHEMA,
    observationLimits: { toolCalls: e.toolCalls, childThreads: 0, retryEvents: 0 }, retainedChildCapture: false, maxTraceBytes: e.maxTraceBytes,
    config: { "agents.max_threads": 1, "agents.max_concurrent_threads_per_session": 1, "agents.max_depth": 0 } };
  if (Date.now() + e.timeoutMs + 4000 > job.deadlineAt) throw new Error("Native engineer cannot fit the original campaign deadline");
  const result = await runNative(options), native = await immutable(join(job.outputRoot, "native-result.json"), result, e.maxResultBytes);
  const trace = { path: result.tracePath, sha256: await fileHash(result.tracePath) };
  receipt.native = native; receipt.trace = trace;
  const traceInfo = await lstat(trace.path);
  if (traceInfo.size > e.maxTraceBytes) throw new Error("Engineering capture exceeds its bound");
  const events = (await readFile(trace.path, "utf8")).split("\n").filter(Boolean);
  if (events.length > 100000) throw new Error("Engineering trace event count exceeds its bound");
  validateCodexCapture(result, events.map(line => JSON.parse(line)), { client: "codex", binary: e.binary, producer: e.nativeCapture,
    model: e.model, effort: e.effort, serviceTier: e.serviceTier, cwd: cap.source_root, inputSha256: sha256(prompt) });
  if (result.permissionEvidence?.profileId !== e.permissionProfile || result.permissionEvidence.extends !== ":read-only" || result.permissionEvidence.filesystemSha256 !== admission.filesystemPolicySha256 ||
      result.observations?.exceeded !== null || result.observations.childThreads !== 0 || result.observations.retryEvents !== 0 ||
      result.workerModels.length || (result.childSnapshots?.length ?? 0)) throw new Error("Native engineer role or observed independent scope differs");
  const output = JSON.parse(result.answerText) as EngineerOutput;
  validateEngineerOutput(output, { parentRevision: cap.source_revision, mutationPaths: cap.mutation_paths } as ActorPolicy);
  if (output.hypothesis.feedbackIds.some(id => !e.feedback.some(f => f.id === id))) throw new Error("Engineer cites feedback outside its projection");
  await verifyActorSource(job, output.candidateRevision, false);
  await git(job, "merge-base", "--is-ancestor", cap.source_revision, output.candidateRevision);
  if ((await git(job, "rev-list", "--count", `${cap.source_revision}..${output.candidateRevision}`)).trim() !== "1") throw new Error("Each native engineer produces exactly one committed candidate");
  const changedPaths = (await git(job, "diff", "--name-only", "-z", "--no-renames", cap.source_revision, output.candidateRevision, "--")).split("\0").filter(Boolean).sort();
  if (!changedPaths.length || changedPaths.some(p => !cap.mutation_paths.includes(p)) || canonical(changedPaths) !== canonical([...output.hypothesis.mutationPaths].sort())) throw new Error("Actual candidate commit differs from the frozen mutation paths and hypothesis");
  for (const path of changedPaths) {
    const entry = await git(job, "ls-tree", output.candidateRevision, "--", path);
    if (entry && !/^(100644|100755) blob /.test(entry)) throw new Error("Candidate source must retain regular Git objects");
  }
  const diff = await git(job, "diff", "--binary", "--no-ext-diff", "--no-renames", cap.source_revision, output.candidateRevision, "--");
  return { native, trace, candidateRevision: output.candidateRevision, changedPaths, diffSha256: digest(diff), hypothesis: output.hypothesis, status: "candidate_source_ready" };
}
export async function verifyActorAcceptance(job: ContainerActorJob, admission: StorageAdmission, container: NonNullable<StorageAdmission["container"]>): Promise<void> {
  const binding = await readExactJson<any>(admission.binding), acceptance = await readExactJson<any>(container.acceptance.path);
  closed(acceptance, ["schema", "status", "current", "assessment", "generation", "assessmentRevision", "taskIds", "profileIds", "sourcePins", "storage", "controllerArgv", "childArgv", "runtime", "harborExport"], "actor detached acceptance");
  if (acceptance.schema !== "gptgrep.availability-controller.v1" || acceptance.status !== "accepted" ||
      canonical(acceptance.storage) !== canonical({ runId: binding.plan.run_id, workloadSha256: binding.plan.workload_sha256, cwd: binding.plan.cwd }) ||
      digest(acceptance.controllerArgv) !== binding.plan.command.argv_sha256 || !Array.isArray(acceptance.sourcePins) || acceptance.sourcePins.length > 128 ||
      !acceptance.sourcePins.some((p: Pin) => canonical(p) === canonical(container.immutableJob))) throw new Error("Actor acceptance differs from its immutable owner command");
  if (["current", "assessment", "runtime", "harborExport"].some(key => acceptance[key] !== null) || canonical(acceptance.taskIds) !== canonical([job.capability.candidate_id])) throw new Error("Actor acceptance cannot mount reader, evaluator or campaign controls");
  const paths = acceptance.sourcePins.map((p: Pin) => p.path);
  if (new Set(paths).size !== paths.length) throw new Error("Actor source acceptance has duplicate pins");
  const required = ["./container-actor.ts", "./container-actor-cli.ts", "./actor-git.ts", "./actor-guest-contracts.ts", "./proposals.ts", "./mutation-paths.ts",
    "../../benchmark/src/io.ts", "../../benchmark/src/process.ts", "../../benchmark/src/native.ts", "../../benchmark/src/trajectory.ts", "../../benchmark/src/availability.ts", "../../benchmark/src/source-verification.ts"];
  for (const url of required) if (!paths.includes(fileURLToPath(new URL(url, import.meta.url)))) throw new Error("Actor acceptance omits an actual imported guest source");
  const expected = new Map<string, Pin>();
  for (const rel of required) { const path = fileURLToPath(new URL(rel, import.meta.url)); expected.set(path, { path, sha256: await fileHash(path) }); }
  const emptyConfig = { path: join(dirname(binding.guest_runtime.supervisor_guest_path), "guest-bunfig.toml"), sha256: sha256("") };
  for (const pin of [container.immutableJob, job.capability.source_manifest, container.modelProfile, emptyConfig,
    ...(job.engineer ? [job.engineer.nativeCapture, job.engineer.instructions] : job.build!.commands.flatMap(command => command.inputs))]) {
    const prior = expected.get(pin.path); if (prior && canonical(prior) !== canonical(pin)) throw new Error("Conflicting actor source pins"); expected.set(pin.path, pin);
  }
  if (canonical([...acceptance.sourcePins].sort((a: Pin, b: Pin) => a.path.localeCompare(b.path))) !== canonical([...expected.values()].sort((a, b) => a.path.localeCompare(b.path)))) throw new Error("Actor acceptance includes omitted or unrelated source inputs");
  const children = job.engineer ? [[job.engineer.binary.path, "app-server", "--listen", "stdio://", "--strict-config"]] : job.build!.commands.map(command => { const c = compileContainerBuildCommand(job, container, command); return [c.executable.path, ...c.argv]; });
  if (canonical(acceptance.childArgv) !== canonical(children)) throw new Error("Actor child commands differ from the exact immutable role job");
  for (const pin of acceptance.sourcePins) await verifyReceiptPin(pin);
  if (job.engineer && !acceptance.profileIds.includes(job.engineer.permissionProfile)) throw new Error("Engineer profile is absent from the exact acceptance");
}
export async function verifyActorNamespace(admission: StorageAdmission): Promise<void> {
  if (process.platform !== "linux") throw new Error("C4 execution requires the actual Linux owner container namespace");
  const binding = await readExactJson<any>(admission.binding);
  const base = await readExactJson<any>(binding.container_binding_guest_path);
  if (await fileHash(binding.container_binding_guest_path) !== binding.container_binding.sha256) throw new Error("Actor container binding changed during namespace observation");
  const observed: Record<string, string> = {};
  for (const [name, file] of [["mount", "mnt"], ["pid", "pid"], ["user", "user"], ["time", "time"], ["network", "net"]]) observed[name!] = await readlink(`/proc/self/ns/${file}`);
  const boot = (await readFile("/proc/sys/kernel/random/boot_id", "utf8")).trim();
  const init = await readFile("/proc/1/stat", "utf8"), offset = init.lastIndexOf(")");
  const initStart = offset >= 0 ? init.slice(offset + 2).trim().split(/ +/)[19] : undefined;
  if (canonical(observed) !== canonical(base.container.namespaces) || boot !== base.container.boot_id ||
      process.getuid?.() !== base.container.uid || process.getgid?.() !== base.container.gid || initStart !== base.container.init_start_ticks) throw new Error("Actor actual namespace, boot, UID or container init generation differs");
}
export function compileContainerBuildCommand(job: ContainerActorJob, admission: NonNullable<StorageAdmission["container"]>, command: ProcessCommand): ProcessCommand {
  if (job.capability.role !== "build" || !admission.filesystemPolicyName || admission.filesystemPolicySha256 === undefined) throw new Error("Actual build requires its exact qualified owner sandbox profile");
  const pdfium = job.build!.pdfium;
  const child = pdfium ? [pdfium.launcher.path, "--", `PDFIUM_LIB_PATH=${dirname(pdfium.library.path)}`, `PDFIUM_INCLUDE_PATH=${pdfium.includeDirectory}`,
    `LD_LIBRARY_PATH=${dirname(pdfium.library.path)}`, command.executable.path, ...command.argv] : [command.executable.path, ...command.argv];
  return { executable: job.capability.native_binary, inputs: [admission.modelProfile, command.executable, ...command.inputs, ...(pdfium ? [pdfium.launcher, pdfium.library, ...pdfium.headers] : [])],
    argv: ["sandbox", "--permission-profile", admission.filesystemPolicyName, "-C", job.capability.source_root, "--", ...child],
    cwd: job.capability.source_root, timeoutMs: command.timeoutMs, maxOutputBytes: command.maxOutputBytes };
}
export function containerBuildEnvironment(job: ContainerActorJob, command: ProcessCommand): Record<string, string> {
  return { ...job.build!.environment, PATH: `${dirname(command.executable.path)}:/usr/bin:/bin`, LANG: "C.UTF-8", LC_ALL: "C.UTF-8",
    CODEX_HOME: join(job.outputRoot, ".build-sandbox-home"), CODEX_SQLITE_HOME: join(job.outputRoot, ".build-sandbox-state"),
    CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED: "1", CARGO_NET_OFFLINE: "true" };
}
async function preserveBuildArtifact(job: ContainerActorJob): Promise<Pin> {
  const source = job.build!.binaryPath, before = await inspectLinuxBinary(source, job.build!.targetArch);
  const target = join(job.outputRoot, "candidate-artifact.bin"), input = await open(source, constants.O_RDONLY | constants.O_NOFOLLOW);
  const output = await open(target, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o700);
  const hash = createHash("sha256"); let bytes = 0;
  try {
    const info = await input.stat(); if (!info.isFile() || info.size > job.maxEvidenceBytes) throw new Error("Candidate binary exceeds its sealed output allowance");
    const block = Buffer.alloc(1024 * 1024);
    while (true) { const read = await input.read(block, 0, block.length, null); if (!read.bytesRead) break; bytes += read.bytesRead; if (bytes > info.size || bytes > job.maxEvidenceBytes) throw new Error("Candidate artifact grew during collection"); const chunk = block.subarray(0, read.bytesRead); hash.update(chunk); await output.writeFile(chunk); }
    const after = await input.stat();
    if (bytes !== info.size || after.ino !== info.ino || after.dev !== info.dev || after.size !== info.size || after.mtimeMs !== info.mtimeMs || hash.digest("hex") !== before.sha256) throw new Error("Candidate artifact changed while preserving its original bytes");
    await output.sync();
  } finally { await input.close(); await output.close(); }
  const preserved = await inspectLinuxBinary(target, job.build!.targetArch); if (preserved.sha256 !== before.sha256) throw new Error("Preserved candidate artifact differs from the original build bytes"); return preserved;
}
export async function runContainerActor(jobPin: Pin): Promise<Pin> {
  await verifyReceiptPin(jobPin);
  const job = await readExactJson<ContainerActorJob>(jobPin.path);
  validateContainerActorJob(job);
  const admission = await requireStorage(job.outputRoot, ...(job.capability.role === "engineer" ? [job.capability.source_root] : [job.capability.build_root!]));
  const container = validateActorAdmission(job, admission);
  await verifyActorNamespace(admission);
  await verifyActorAcceptance(job, admission, container);
  if (canonical(container.immutableJob) !== canonical(jobPin)) throw new Error("Actor job is outside the exact owner command binding");
  if (Date.now() + 4000 >= job.deadlineAt) throw new Error("Original campaign deadline expired");
  if (job.capability.role === "build" && ["CODEX_HOME", "GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID"].some(k => process.env[k])) throw new Error("Candidate builds cannot inherit native authentication");
  await verifyActorSource(job, job.capability.source_revision, true);
  const receipt: ContainerActorReceipt = { schema: "gptgrep.container-actor-result.v1", job: jobPin, capability: job.capability,
    commandBinding: { path: admission.binding, sha256: admission.sha256 }, ownerTemplateSha256: container.ownerTemplateSha256,
    parentHostBindingSha256: container.parentHostBindingSha256, status: "failed", error: null,
    candidateRevision: job.capability.source_revision, changedPaths: [], diffSha256: null, hypothesis: null, native: null, trace: null,
    checks: [], sandboxCommands: [], binary: null, startedAt: new Date().toISOString(), finishedAt: "", runtimeAcceptance: false, releaseAcceptance: false };
  try {
    if (job.capability.role === "engineer") Object.assign(receipt, await engineer(job, container, receipt));
    else {
      const b = job.build!;
      if (b.pdfium) await verifyContainerBuildPdfiumFiles(b.pdfium);
      for (const value of Object.values(b.environment).filter(v => v.startsWith("/"))) { await requireStorage(value); await physicalPath(value); await mkdir(value, { recursive: true, mode: 0o700 }); }
      const home = join(job.outputRoot, ".build-sandbox-home"), state = join(job.outputRoot, ".build-sandbox-state");
      await mkdir(home, { mode: 0o700 }); await mkdir(state, { mode: 0o700 });
      const profile = await readFile(container.modelProfile.path); if (profile.length > 65536 || await fileHash(container.modelProfile.path) !== container.modelProfile.sha256) throw new Error("Build sandbox profile changed");
      const file = await open(join(home, "config.toml"), "wx", 0o600); try { await file.writeFile(profile); await file.sync(); } finally { await file.close(); }
      for (const command of b.commands) {
        const sandbox = compileContainerBuildCommand(job, container, command); receipt.sandboxCommands.push(sandbox);
        const result = await executeOwnerCommand(sandbox, job.deadlineAt - Date.now(), { environment: containerBuildEnvironment(job, command), inheritEnvironment: false }); receipt.checks.push(result);
        if (!result.started || result.executionError || result.exitCode !== 0 || result.timedOut || result.outputExceeded || !result.cleanup) throw new Error("Candidate build/check lacks successful bounded sandbox terminal evidence");
      }
      try { await lstat(join(home, "auth.json")); throw new Error("Build sandbox home unexpectedly acquired authentication state"); } catch (error: any) { if (error.code !== "ENOENT") throw error; }
      await verifyActorSource(job, job.capability.source_revision, false);
      receipt.binary = await preserveBuildArtifact(job); receipt.status = "artifact_built";
    }
  } catch (error) { receipt.error = error instanceof Error ? error.message : String(error); receipt.status = "failed"; }
  receipt.finishedAt = new Date().toISOString();
  return immutable(join(job.outputRoot, "actor-result.json"), receipt, Math.min(job.maxEvidenceBytes, 2 * 1024 * 1024));
}
