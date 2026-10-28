import { constants } from "node:fs";
import { chmod, copyFile, lstat, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { corpusDigest } from "./adapter";
import type { ReaderTask } from "./dataset";
import { canonical, digest, fileHash, physicalPath, readJson, requireStorage, sha256, within, writeJson, writeJsonExclusive } from "./io";
import { boundNativeResult, runNative, type NativeRunOptions, type NativeRunResult } from "./native";
import { assertTaskAdmitted, loadAdmission, verifyPin, verifyRequiredPreprocessing, type AdmissionReference, type Pin } from "./availability";

export const SUPERVISOR_MODEL = "gpt-6.1-sol";
export const WORKER_MODEL = "gpt-6-luna";
export const HELPER_MODELS = ["openai/gpt-6-luna-decisions", "typesafe/jev-1.14"] as const;
export class StartedReaderArtifactError extends Error {}
export type ReaderArm = "bare" | "gptgrep";
export interface WorkspaceDocument { documentId: string; relativePath: string; sha256: string; }
export interface ReaderWorkspace {
  cwd: string;
  /** These are the full task context, including non-evidence documents. */
  documents: WorkspaceDocument[];
  /** Additional context-derived products; GPTgrep arm only. Paths and bytes are pinned. */
  preparedArtifacts?: Array<{ relativePath: string; sha256: string }>;
  preparedContextDocumentIds?: string[];
  /** Optional discovery capability for an exact prepared generation; GPTgrep arm only. */
  preparedIndex?: { relativeRoot: string; generation: string; manifestSha256: string };
}
export interface PinnedRuntime {
  sourceRevision: string;
  buildReceipt: Pin;
  packageRoot: string;
  manifestSha256: string;
  binaryPath: string;
  binarySha256: string;
}
export interface ReaderPairOptions {
  admission: Record<ReaderArm, AdmissionReference>;
  task: ReaderTask;
  pairIndex: number;
  seed: string;
  outputDir: string;
  native: Omit<NativeRunOptions, "cwd" | "model" | "input" | "tracePath" | "outputSchema">;
  bare: ReaderWorkspace;
  gptgrep: ReaderWorkspace;
  runtime: PinnedRuntime;
  helperModel: typeof HELPER_MODELS[number];
  /** Must launch a separate owner storage + auth-supervisor child for each arm. */
  launchArm: (options: ReaderArmOptions) => Promise<ReaderArmResult>;
}
export interface ReaderAttempt {
  schema: "gptgrep.reader-attempt.v1";
  arm: ReaderArm;
  taskId: string;
  executionMode: "live";
  order: number;
  startedAt: string;
  completedAt: string;
  validStart: boolean;
  status: "completed" | "failed" | "timeout" | "setup_failed" | "answer_invalid";
  native: NativeRunResult | null;
  answerPath: string | null;
  answerSha256: string | null;
  error: string | null;
}
export interface ReaderArmOptions {
  admission: AdmissionReference;
  task: ReaderTask;
  arm: ReaderArm;
  order: number;
  workspace: ReaderWorkspace;
  outputDir: string;
  native: ReaderPairOptions["native"];
  runtime: PinnedRuntime;
  helperModel: typeof HELPER_MODELS[number];
}
export interface ReaderArmResult extends ReaderAttempt {
  productRevision: string;
  buildReceiptSha256: string;
  preparationSha256: string;
  readerTaskSha256: string;
  authRunId: string | null;
  storageBindingSha256: string;
  corpusSha256: string | null;
  contextSha256: string | null;
  commonPromptSha256: string;
  runtimeManifestSha256: string;
  binarySha256: string;
}
export interface ReaderPairResult {
  schema: "gptgrep.reader-pair.v1";
  executionMode: "live";
  taskId: string;
  pairIndex: number;
  order: ReaderArm[];
  corpusSha256: string;
  commonPromptSha256: string;
  contextSha256: string | null;
  requestedModels: { supervisor: string; worker: string; ocr: string; helper: string };
  runtimeManifestSha256: string;
  binarySha256: string;
  attempts: ReaderArmResult[];
}

const RUNTIME_PATHS: Record<string, string> = {
  "runtime/codex/AGENTS.md": "AGENTS.md",
  "runtime/codex/agents/gptgrep-document.toml": ".codex/agents/gptgrep-document.toml",
  "runtime/codex/agents/gptgrep-ocr.toml": ".codex/agents/gptgrep-ocr.toml",
  "runtime/codex/agents/gptgrep-retrieval.toml": ".codex/agents/gptgrep-retrieval.toml",
  "skills/gptgrep/SKILL.md": ".agents/skills/gptgrep/SKILL.md",
  "skills/gptgrep-document/SKILL.md": ".agents/skills/gptgrep-document/SKILL.md",
  "skills/gptgrep-ocr/SKILL.md": ".agents/skills/gptgrep-ocr/SKILL.md",
  "skills/gptgrep-retrieval/SKILL.md": ".agents/skills/gptgrep-retrieval/SKILL.md",
};
const WORKSPACE_DESCRIPTOR = "gptgrep-workspace.json";
const ANSWER_SCHEMA = {
  type: "object", properties: { answer: { type: "string" }, citations: { type: "array", items: {
    type: "object", properties: { document_id: { type: "string" }, page: { type: "integer", minimum: 1 } },
    required: ["document_id", "page"], additionalProperties: false,
  } }, explanation: { type: "string" } }, required: ["answer", "citations", "explanation"], additionalProperties: false,
};

function relativePath(path: string): string {
  if (!path || isAbsolute(path) || path.includes("\\") || /[\r\n\0]/.test(path) || path.split("/").some(part => !part || part === "." || part === "..")) throw new Error("Artifact path must be a safe relative path");
  return path;
}
async function regular(path: string, expected: string): Promise<void> {
  await physicalPath(path);
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || !/^[a-f0-9]{64}$/.test(expected) || await fileHash(path) !== expected) throw new Error(`Pinned regular file mismatch: ${path}`);
}
async function absent(path: string): Promise<void> {
  try { await lstat(path); } catch (error: any) { if (error.code === "ENOENT") return; throw error; }
  throw new Error(`Refusing to overwrite an existing artifact: ${path}`);
}
async function files(root: string, prefix = ""): Promise<string[]> {
  const result: string[] = [];
  for (const entry of await readdir(resolve(root, prefix), { withFileTypes: true })) {
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isSymbolicLink() || !entry.isFile() && !entry.isDirectory()) throw new Error("Reader workspaces require regular files and physical directories");
    if (entry.isDirectory()) result.push(...await files(root, path)); else result.push(path);
  }
  return result;
}

/** Verify the pinned product package before staging exactly its eight native assets. */
export async function stageTreatmentRuntime(runtime: PinnedRuntime, cwd: string): Promise<string[]> {
  await requireStorage(cwd);
  await physicalPath(cwd);
  const manifestPath = resolve(runtime.packageRoot, "runtime/codex/manifest.json");
  await regular(manifestPath, runtime.manifestSha256);
  await regular(runtime.binaryPath, runtime.binarySha256);
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  if (manifest.schema !== "gptgrep.native-runtime-package.v1" || !Array.isArray(manifest.files) || manifest.files.length !== Object.keys(RUNTIME_PATHS).length) throw new Error("Unexpected native product package manifest");
  const seen = new Set<string>();
  const staged: Array<{ source: string; target: string; hash: string }> = [];
  for (const file of manifest.files) {
    if (RUNTIME_PATHS[file.source_path] !== file.target_path || seen.has(file.source_path)) throw new Error("Runtime manifest must contain the exact product-owned assets");
    seen.add(file.source_path);
    const source = resolve(runtime.packageRoot, relativePath(file.source_path));
    await regular(source, file.sha256);
    staged.push({ source, target: relativePath(file.target_path), hash: file.sha256 });
  }
  staged.push({ source: runtime.binaryPath, target: "bin/gptgrep", hash: runtime.binarySha256 });
  for (const file of staged) await absent(resolve(cwd, file.target));
  for (const file of staged) {
    const destination = resolve(cwd, file.target);
    await physicalPath(destination);
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(file.source, destination, constants.COPYFILE_EXCL | constants.COPYFILE_FICLONE);
    await chmod(destination, file.target === "bin/gptgrep" ? 0o755 : 0o444);
    await regular(destination, file.hash);
  }
  return staged.map(file => file.target);
}

async function verifyProductRuntime(runtime: PinnedRuntime): Promise<void> {
  await verifyPin(runtime.buildReceipt);
  const build = await readJson(runtime.buildReceipt.path);
  if (build.schema !== "gptgrep.product-build.v1" || !/^[a-f0-9]{40,64}$/.test(runtime.sourceRevision) ||
      build.sourceRevision !== runtime.sourceRevision || build.binarySha256 !== runtime.binarySha256 ||
      build.runtimeManifestSha256 !== runtime.manifestSha256) throw new Error("Product source/build/runtime binding differs");
  await regular(runtime.binaryPath, runtime.binarySha256);
  await regular(resolve(runtime.packageRoot, "runtime/codex/manifest.json"), runtime.manifestSha256);
}

export function preparationDigest(workspace: ReaderWorkspace): string {
  return digest({ artifacts: [...(workspace.preparedArtifacts ?? [])].sort((a, b) => a.relativePath.localeCompare(b.relativePath)),
    contextDocumentIds: [...(workspace.preparedContextDocumentIds ?? [])].sort(), index: workspace.preparedIndex ?? null });
}

export function readerOrder(seed: string, pairIndex: number): ReaderArm[] {
  if (!Number.isSafeInteger(pairIndex) || pairIndex < 0 || !seed) throw new Error("A seed and nonnegative pair index are required");
  const offset = parseInt(digest(seed).slice(0, 8), 16) % 2;
  return (offset + pairIndex) % 2 ? ["gptgrep", "bare"] : ["bare", "gptgrep"];
}

/** This exact prompt is used by both arms. No product instructions are injected here. */
export function readerPrompt(task: ReaderTask, helperModel: typeof HELPER_MODELS[number] = HELPER_MODELS[0]): string {
  if (!HELPER_MODELS.includes(helperModel)) throw new Error("Unsupported frozen helper model");
  return `${task.question}\n\nUse the supplied original PDF documents listed in context.json. ` +
    "Document paths are relative to the current task workspace. Use only this supplied document context. " +
    "Do not search the internet for an answer. Native tools and delegation are available within the task boundary. " +
    `If you delegate work, select ${WORKER_MODEL} for each worker, including visual/OCR work. ` +
    `If you use a routing or ranking model, select ${helperModel}. ` +
    "Return a JSON object with answer (a short string), citations (an array of document_id and page), and explanation (a separate string). " +
    "Each page must be the one-based physical PDF page number. If the supplied documents do not determine an answer, use \"not answerable\" and explain what is missing.\n";
}

async function preparedDescriptor(workspace: ReaderWorkspace): Promise<string | null> {
  const capability = workspace.preparedIndex;
  if (capability === undefined) return null;
  if (!capability || typeof capability !== "object" || Object.keys(capability).some(key => !["relativeRoot", "generation", "manifestSha256"].includes(key)) ||
      typeof capability.relativeRoot !== "string" || typeof capability.generation !== "string" || typeof capability.manifestSha256 !== "string" ||
      !/^g[0-9]+-[0-9]+$/.test(capability.generation) || !/^[a-f0-9]{64}$/.test(capability.manifestSha256)) throw new Error("Invalid prepared index capability");
  const root = relativePath(capability.relativeRoot);
  if (root !== "prepared" && !root.startsWith("prepared/")) throw new Error("Prepared index root must remain under prepared/");
  const pins = new Map(workspace.preparedArtifacts?.map(artifact => [artifact.relativePath, artifact.sha256]));
  const currentPath = `${root}/.gptgrep/CURRENT.json`;
  const manifestPath = `${root}/.gptgrep/generations/${capability.generation}/manifest.json`;
  if (!pins.has(currentPath) || pins.get(manifestPath) !== capability.manifestSha256) throw new Error("Prepared index pointer and manifest must be pinned artifacts");
  if ((await lstat(resolve(workspace.cwd, currentPath))).size > 4096) throw new Error("Prepared index pointer exceeds its bound");
  const current = await readJson(resolve(workspace.cwd, currentPath));
  // Match the core's MAX_MANIFEST_BYTES for both supported generation schemas.
  const manifestFile = resolve(workspace.cwd, manifestPath);
  if ((await lstat(manifestFile)).size > 128 * 1024 * 1024) throw new Error("Prepared manifest exceeds its bound");
  const manifest = JSON.parse(await readFile(manifestFile, "utf8"));
  if (!current || !manifest || !["gptgrep.v1", "gptgrep.manifest.v2"].includes(current.schema_version) || manifest.schema_version !== current.schema_version ||
      current.generation !== capability.generation || current.manifest_sha256 !== capability.manifestSha256 ||
      manifest.generation !== capability.generation || manifest.root !== resolve(workspace.cwd, root) || !Array.isArray(manifest.documents)) throw new Error("Prepared index snapshot identity mismatch");
  const sourceHashes: string[] = [];
  const sourcePaths = new Set<string>();
  for (const entry of manifest.documents) {
    const doc = manifest.schema_version === "gptgrep.manifest.v2" ? entry?.document : entry;
    if (!doc || typeof doc.path !== "string" || typeof doc.source_sha256 !== "string" || !Number.isSafeInteger(doc.source_bytes) || doc.source_bytes < 0) throw new Error("Invalid prepared source identity");
    const path = `${root}/${relativePath(doc.path)}`;
    if (sourcePaths.has(path) || pins.get(path) !== doc.source_sha256 || (await lstat(resolve(workspace.cwd, path))).size !== doc.source_bytes) throw new Error("Prepared source must match a pinned raw document");
    sourcePaths.add(path);
    sourceHashes.push(doc.source_sha256);
  }
  if (JSON.stringify(sourceHashes.sort()) !== JSON.stringify(workspace.documents.map(doc => doc.sha256).sort())) throw new Error("Prepared index must cover the exact full raw document context");
  return `${canonical({ schema: "gptgrep.workspace.v1", prepared_index: { root, snapshot: { generation: capability.generation, manifest_sha256: capability.manifestSha256 } } })}\n`;
}

async function inspectReaderWorkspace(workspace: ReaderWorkspace, task: ReaderTask, arm: ReaderArm, runtimeFiles: string[], phase: "before_descriptor" | "complete"): Promise<{ corpus: string; context: string; descriptor: string | null }> {
  await physicalPath(workspace.cwd);
  const allowedRuntime = new Set([...Object.values(RUNTIME_PATHS), "bin/gptgrep"]);
  if (new Set(runtimeFiles).size !== runtimeFiles.length || runtimeFiles.some(path => !allowedRuntime.has(path)) || arm === "bare" && runtimeFiles.length) throw new Error("Reader runtime files must be unique product-owned assets");
  const ids = workspace.documents.map(doc => doc.documentId).sort();
  if (new Set(ids).size !== ids.length || JSON.stringify(ids) !== JSON.stringify([...task.contextDocumentIds].sort())) throw new Error("Workspace must contain the full exact task context");
  const expected = new Set(["context.json", ...runtimeFiles]);
  for (const doc of workspace.documents) {
    const path = relativePath(doc.relativePath);
    if (!path.startsWith("documents/") || expected.has(path)) throw new Error("Document paths must be unique under documents/");
    expected.add(path); await regular(resolve(workspace.cwd, path), doc.sha256);
  }
  const prepared = workspace.preparedArtifacts ?? [];
  if (arm === "bare" && (prepared.length || workspace.preparedIndex !== undefined)) throw new Error("Bare reader cannot receive prepared products");
  if (prepared.length && JSON.stringify([...(workspace.preparedContextDocumentIds ?? [])].sort()) !== JSON.stringify(ids)) throw new Error("Prepared products must bind the exact full task context");
  for (const artifact of prepared) {
    const path = relativePath(artifact.relativePath);
    if (!path.startsWith("prepared/") || expected.has(path)) throw new Error("Prepared products require unique paths under prepared/");
    expected.add(path); await regular(resolve(workspace.cwd, path), artifact.sha256);
  }
  const descriptor = await preparedDescriptor(workspace);
  const descriptorPath = resolve(workspace.cwd, WORKSPACE_DESCRIPTOR);
  if (phase === "complete" && descriptor !== null) {
    expected.add(WORKSPACE_DESCRIPTOR);
    await regular(descriptorPath, sha256(descriptor));
    if (((await lstat(descriptorPath)).mode & 0o777) !== 0o444) throw new Error("Prepared workspace descriptor must be read-only");
  } else await absent(descriptorPath);
  const contextPath = resolve(workspace.cwd, "context.json");
  await physicalPath(contextPath);
  const context = JSON.parse(await readFile(contextPath, "utf8"));
  const wanted = workspace.documents.map(doc => ({ document_id: doc.documentId, path: doc.relativePath, sha256: doc.sha256 })).sort((a, b) => a.document_id.localeCompare(b.document_id));
  if (!context || !Array.isArray(context.documents) || Object.keys(context).some(key => key !== "documents") || digest(context.documents.slice().sort((a: any, b: any) => a.document_id.localeCompare(b.document_id))) !== digest(wanted)) throw new Error("context.json must contain only the exact public document mapping");
  for (const path of await files(workspace.cwd)) if (!expected.has(path)) throw new Error(`Unadmitted reader workspace file: ${path}`);
  if (arm === "bare") for (const path of ["AGENTS.md", ".agents", ".codex", "bin"]) await absent(resolve(workspace.cwd, path));
  return { corpus: corpusDigest(workspace.documents), context: await fileHash(contextPath), descriptor };
}

/** Stage only the validated task-local discovery capability, outside the indexed source root. */
export async function stagePreparedWorkspace(workspace: ReaderWorkspace, task: ReaderTask, arm: ReaderArm, runtimeFiles: string[] = []): Promise<void> {
  await requireStorage(workspace.cwd);
  const { descriptor } = await inspectReaderWorkspace(workspace, task, arm, runtimeFiles, "before_descriptor");
  if (descriptor === null) return;
  const path = resolve(workspace.cwd, WORKSPACE_DESCRIPTOR);
  await writeFile(path, descriptor, { flag: "wx", mode: 0o444 });
  await chmod(path, 0o444);
  await regular(path, sha256(descriptor));
}

export async function verifyReaderWorkspace(workspace: ReaderWorkspace, task: ReaderTask, arm: ReaderArm, runtimeFiles: string[] = []): Promise<{ corpus: string; context: string }> {
  const { corpus, context } = await inspectReaderWorkspace(workspace, task, arm, runtimeFiles, "complete");
  return { corpus, context };
}

export function parseReaderAnswer(text: string, task: ReaderTask): unknown {
  const value = JSON.parse(text);
  if (!value || typeof value.answer !== "string" || !value.answer.trim() || !Array.isArray(value.citations) || typeof value.explanation !== "string" || Object.keys(value).some(key => !["answer", "citations", "explanation"].includes(key))) throw new Error("Native final message is not a nonempty answer object");
  for (const citation of value.citations) if (!citation || !task.contextDocumentIds.includes(citation.document_id) || !Number.isSafeInteger(citation.page) || citation.page < 1 || Object.keys(citation).some(key => !["document_id", "page"].includes(key))) throw new Error("Native answer has an invalid document/page citation");
  return value;
}

/** One admitted child only. Pair orchestration must use a fresh auth/storage run per arm. */
export async function runReaderArm(options: ReaderArmOptions): Promise<ReaderArmResult> {
  const storage = await requireStorage(options.outputDir, options.workspace.cwd);
  await admitReader(options.admission, options.task, options.arm, options.workspace, options.outputDir);
  await verifyProductRuntime(options.runtime);
  if (!HELPER_MODELS.includes(options.helperModel)) throw new Error("Unsupported frozen helper model");
  if (options.arm !== "bare" && options.arm !== "gptgrep") throw new Error("Unknown reader arm");
  for (const root of options.native.readableRoots) {
    if (within(root, options.outputDir) || within(root, options.workspace.cwd)) throw new Error("Common readable roots must not include the reader workspace or controller outputs");
  }
  const prompt = readerPrompt(options.task, options.helperModel);
  const directory = resolve(options.outputDir);
  await absent(resolve(directory, "attempt-start.json"));
  await mkdir(directory, { recursive: true });
  const startedAt = new Date().toISOString();
  const attempt: ReaderArmResult = { schema: "gptgrep.reader-attempt.v1", arm: options.arm, taskId: options.task.id, executionMode: "live", order: options.order,
    startedAt, completedAt: startedAt, validStart: false, status: "setup_failed", native: null, answerPath: null, answerSha256: null, error: null,
    authRunId: process.env.GPTGREP_AUTH_RUN_ID ?? null, storageBindingSha256: storage.sha256, corpusSha256: null, contextSha256: null,
    commonPromptSha256: digest(prompt), runtimeManifestSha256: options.runtime.manifestSha256, binarySha256: options.runtime.binarySha256,
    productRevision: options.runtime.sourceRevision, buildReceiptSha256: options.runtime.buildReceipt.sha256,
    preparationSha256: preparationDigest(options.workspace), readerTaskSha256: digest(options.task) };
  await writeJsonExclusive(resolve(directory, "attempt-start.json"), { ...attempt, completedAt: null, status: "starting" }, 1024 * 1024);
  try {
      const runtimeFiles = options.arm === "gptgrep" ? await stageTreatmentRuntime(options.runtime, options.workspace.cwd) : [];
      await stagePreparedWorkspace(options.workspace, options.task, options.arm, runtimeFiles);
      const verified = await verifyReaderWorkspace(options.workspace, options.task, options.arm, runtimeFiles);
      attempt.corpusSha256 = verified.corpus;
      attempt.contextSha256 = verified.context;
      await admitReader(options.admission, options.task, options.arm, options.workspace, options.outputDir);
      attempt.native = boundNativeResult(await runNative({ ...options.native, cwd: options.workspace.cwd, model: SUPERVISOR_MODEL, input: prompt,
        tracePath: resolve(directory, "native-trace.jsonl"), readableRoots: [...options.native.readableRoots, options.workspace.cwd],
        config: { ...options.native.config, "agents.default_subagent_model": WORKER_MODEL }, outputSchema: ANSWER_SCHEMA }));
      if (attempt.native.executionMode !== "live") throw new Error("Test execution cannot enter a live pair");
      attempt.validStart = attempt.native.validStart;
      attempt.status = attempt.native.status;
      attempt.error = attempt.native.error ?? null;
      if (attempt.native.status === "completed") {
        try {
          const answer = parseReaderAnswer(attempt.native.answerText, options.task);
          const answerPath = resolve(directory, "answer.json");
          await writeJsonExclusive(answerPath, answer, 4 * 1024 * 1024);
          const answerSha256 = await fileHash(answerPath);
          attempt.answerPath = answerPath; attempt.answerSha256 = answerSha256;
        } catch (error) { attempt.status = "answer_invalid"; attempt.error = error instanceof Error ? error.message : String(error); }
      }
      // Raw final text is already retained by the native capture and bounded attempt.
  } catch (error) {
    if (attempt.status === "completed") attempt.status = "failed";
    attempt.error = error instanceof Error ? error.message : String(error);
  }
  attempt.completedAt = new Date().toISOString();
  try { await writeJsonExclusive(resolve(directory, "attempt.json"), attempt, 4 * 1024 * 1024); }
  catch (error) { if (attempt.validStart) throw new StartedReaderArtifactError(`Started reader artifact could not be retained: ${error instanceof Error ? error.message : String(error)}`); throw error; }
  return attempt;
}

/** Serial controller. launchArm crosses the separate owner-auth/storage boundary. */
export async function runReaderPair(options: ReaderPairOptions): Promise<ReaderPairResult> {
  await requireStorage(options.outputDir);
  for (const arm of ["bare", "gptgrep"] as const) await admitReader(options.admission?.[arm], options.task, arm, options[arm], options.outputDir);
  if (within(options.bare.cwd, options.gptgrep.cwd) || within(options.gptgrep.cwd, options.bare.cwd)) throw new Error("Reader workspaces must be disjoint");
  const bareCorpus = corpusDigest(options.bare.documents);
  if (bareCorpus !== corpusDigest(options.gptgrep.documents)) throw new Error("Declared paired source identities differ");
  const order = readerOrder(options.seed, options.pairIndex);
  const result: ReaderPairResult = { schema: "gptgrep.reader-pair.v1", executionMode: "live", taskId: options.task.id, pairIndex: options.pairIndex, order,
    corpusSha256: bareCorpus, commonPromptSha256: digest(readerPrompt(options.task, options.helperModel)), contextSha256: null,
    requestedModels: { supervisor: SUPERVISOR_MODEL, worker: WORKER_MODEL, ocr: WORKER_MODEL, helper: options.helperModel },
    runtimeManifestSha256: options.runtime.manifestSha256, binarySha256: options.runtime.binarySha256, attempts: [] };
  await absent(resolve(options.outputDir, "pair.json"));
  for (const [index, arm] of order.entries()) {
    await admitReader(options.admission[arm], options.task, arm, options[arm], options.outputDir);
    const attempt = await options.launchArm({ admission: options.admission[arm], task: options.task, arm, order: index, workspace: options[arm], outputDir: resolve(options.outputDir, arm),
      native: options.native, runtime: options.runtime, helperModel: options.helperModel });
    if (attempt.executionMode !== "live" || attempt.arm !== arm || attempt.taskId !== options.task.id || attempt.order !== index) throw new Error("Arm result has the wrong execution or task identity");
    if (attempt.native && attempt.native.executionMode !== "live") throw new Error("A test run cannot enter live evidence");
    if (attempt.corpusSha256 !== null && attempt.corpusSha256 !== bareCorpus || attempt.commonPromptSha256 !== result.commonPromptSha256) throw new Error("Arm result does not bind the frozen context and common prompt");
    if (attempt.contextSha256) {
      if (result.contextSha256 && result.contextSha256 !== attempt.contextSha256) throw new Error("Paired context mapping bytes differ");
      result.contextSha256 = attempt.contextSha256;
    }
    for (const previous of result.attempts) {
      if (!attempt.authRunId || attempt.authRunId === previous.authRunId || attempt.storageBindingSha256 === previous.storageBindingSha256) throw new Error("Each arm requires a distinct auth-supervisor and storage run");
    }
    result.attempts.push(attempt);
    await writeJson(resolve(options.outputDir, "pair.json"), result);
    // The supervisor must consume explicit auth failures before any next reader starts.
    if (attempt.native?.authFailure || attempt.native && !attempt.native.cleanup.completed) break;
  }
  return result;
}

async function admitReader(reference: AdmissionReference, task: ReaderTask, arm: ReaderArm, workspace: ReaderWorkspace, output: string): Promise<void> {
  try {
    const admission = await loadAdmission(reference, output);
    assertTaskAdmitted(admission, task, arm, workspace.documents);
    await verifyRequiredPreprocessing(admission, [task], (workspace.preparedArtifacts ?? []).map(p => ({
      relativePath: p.relativePath.startsWith("prepared/") ? p.relativePath.slice("prepared/".length) : p.relativePath,
      sha256: p.sha256,
    })));
  } catch (error) {
    await writeJsonExclusive(resolve(output, "prelaunch-denial.json"), {
      schema: "gptgrep.reader-prelaunch-denial.v1", taskId: task.id, arm, status: "denied_before_reader",
      readerStarted: false, measurement: null, reason: error instanceof Error ? error.message : "Admission failed",
    }, 1024 * 1024);
    throw error;
  }
}
