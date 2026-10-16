import { constants } from "node:fs";
import { chmod, copyFile, lstat, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";
import { corpusDigest } from "./adapter";
import type { ReaderTask } from "./dataset";
import { digest, fileHash, physicalPath, requireStorage, within, writeJson } from "./io";
import { runNative, type NativeRunOptions, type NativeRunResult } from "./native";

export const SUPERVISOR_MODEL = "gpt-6.1-sol";
export const WORKER_MODEL = "gpt-6-luna";
export const HELPER_MODELS = ["openai/gpt-6-luna-decisions", "typesafe/jev-1.14"] as const;
export type ReaderArm = "bare" | "gptgrep";
export interface WorkspaceDocument { documentId: string; relativePath: string; sha256: string; }
export interface ReaderWorkspace {
  cwd: string;
  /** These are the full task context, including non-evidence documents. */
  documents: WorkspaceDocument[];
  /** Additional context-derived products; GPTgrep arm only. Paths and bytes are pinned. */
  preparedArtifacts?: Array<{ relativePath: string; sha256: string }>;
  preparedContextDocumentIds?: string[];
}
export interface PinnedRuntime {
  packageRoot: string;
  manifestSha256: string;
  binaryPath: string;
  binarySha256: string;
}
export interface ReaderPairOptions {
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

export async function verifyReaderWorkspace(workspace: ReaderWorkspace, task: ReaderTask, arm: ReaderArm, runtimeFiles: string[] = []): Promise<{ corpus: string; context: string }> {
  await physicalPath(workspace.cwd);
  const ids = workspace.documents.map(doc => doc.documentId).sort();
  if (new Set(ids).size !== ids.length || JSON.stringify(ids) !== JSON.stringify([...task.contextDocumentIds].sort())) throw new Error("Workspace must contain the full exact task context");
  const expected = new Set(["context.json", ...runtimeFiles]);
  for (const doc of workspace.documents) {
    const path = relativePath(doc.relativePath);
    if (!path.startsWith("documents/") || expected.has(path)) throw new Error("Document paths must be unique under documents/");
    expected.add(path); await regular(resolve(workspace.cwd, path), doc.sha256);
  }
  const prepared = workspace.preparedArtifacts ?? [];
  if (arm === "bare" && prepared.length) throw new Error("Bare reader cannot receive prepared products");
  if (prepared.length && JSON.stringify([...(workspace.preparedContextDocumentIds ?? [])].sort()) !== JSON.stringify(ids)) throw new Error("Prepared products must bind the exact full task context");
  for (const artifact of prepared) {
    const path = relativePath(artifact.relativePath);
    if (!path.startsWith("prepared/") || expected.has(path)) throw new Error("Prepared products require unique paths under prepared/");
    expected.add(path); await regular(resolve(workspace.cwd, path), artifact.sha256);
  }
  const contextPath = resolve(workspace.cwd, "context.json");
  await physicalPath(contextPath);
  const context = JSON.parse(await readFile(contextPath, "utf8"));
  const wanted = workspace.documents.map(doc => ({ document_id: doc.documentId, path: doc.relativePath, sha256: doc.sha256 })).sort((a, b) => a.document_id.localeCompare(b.document_id));
  if (!context || !Array.isArray(context.documents) || Object.keys(context).some(key => key !== "documents") || digest(context.documents.slice().sort((a: any, b: any) => a.document_id.localeCompare(b.document_id))) !== digest(wanted)) throw new Error("context.json must contain only the exact public document mapping");
  for (const path of await files(workspace.cwd)) if (!expected.has(path)) throw new Error(`Unadmitted reader workspace file: ${path}`);
  if (arm === "bare") for (const path of ["AGENTS.md", ".agents", ".codex", "bin"]) await absent(resolve(workspace.cwd, path));
  return { corpus: corpusDigest(workspace.documents), context: await fileHash(contextPath) };
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
    commonPromptSha256: digest(prompt), runtimeManifestSha256: options.runtime.manifestSha256, binarySha256: options.runtime.binarySha256 };
  await writeJson(resolve(directory, "attempt-start.json"), { ...attempt, completedAt: null, status: "starting" });
  try {
      const runtimeFiles = options.arm === "gptgrep" ? await stageTreatmentRuntime(options.runtime, options.workspace.cwd) : [];
      const verified = await verifyReaderWorkspace(options.workspace, options.task, options.arm, runtimeFiles);
      attempt.corpusSha256 = verified.corpus;
      attempt.contextSha256 = verified.context;
      attempt.native = await runNative({ ...options.native, cwd: options.workspace.cwd, model: SUPERVISOR_MODEL, input: prompt,
        tracePath: resolve(directory, "native-trace.jsonl"), readableRoots: [...options.native.readableRoots, options.workspace.cwd],
        config: { ...options.native.config, "agents.default_subagent_model": WORKER_MODEL }, outputSchema: ANSWER_SCHEMA });
      if (attempt.native.executionMode !== "live") throw new Error("Test execution cannot enter a live pair");
      attempt.validStart = attempt.native.validStart;
      attempt.status = attempt.native.status;
      attempt.error = attempt.native.error ?? null;
      if (attempt.native.status === "completed") {
        try {
          const answer = parseReaderAnswer(attempt.native.answerText, options.task);
          attempt.answerPath = resolve(directory, "answer.json");
          await writeJson(attempt.answerPath, answer);
          attempt.answerSha256 = await fileHash(attempt.answerPath);
        } catch (error) { attempt.status = "answer_invalid"; attempt.error = error instanceof Error ? error.message : String(error); }
      }
      // Raw final text stays private and is not silently repaired or retried.
      await writeFile(resolve(directory, "answer.txt"), attempt.native.answerText, { flag: "wx", mode: 0o600 });
  } catch (error) {
    if (attempt.status === "completed") attempt.status = "failed";
    attempt.error = error instanceof Error ? error.message : String(error);
  }
  attempt.completedAt = new Date().toISOString();
  await writeJson(resolve(directory, "attempt.json"), attempt);
  return attempt;
}

/** Serial controller. launchArm crosses the separate owner-auth/storage boundary. */
export async function runReaderPair(options: ReaderPairOptions): Promise<ReaderPairResult> {
  await requireStorage(options.outputDir);
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
    const attempt = await options.launchArm({ task: options.task, arm, order: index, workspace: options[arm], outputDir: resolve(options.outputDir, arm),
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
