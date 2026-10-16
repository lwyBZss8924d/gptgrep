import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve, relative, isAbsolute } from "node:path";

export const XL_RELEASE_REVISION = "72954bd70ffffe230f08b57c57fa9274ec14d7ea";
export const XL_RELEASE_VERSION = "xldocbench_strict_1345_v1";
export const XL_NATIVE_SCORER_SHA256 = "efc56cb25ba136e09ceaccb5d8006ea9461f71a68998f2987073387682a5e547";
export const XL_REPOSITORY = "https://huggingface.co/datasets/microsoft/XL-DocBench";

export type JsonObject = Record<string, unknown>;

/** Public source metadata; never contains task answers or evidence annotations. */
export interface SourceDocument {
  documentId: string;
  title: string | null;
  url: string;
  sourceHost: string;
  pageCount: number;
  fileSizeBytes: number;
  domain: string;
  docType: string;
  pdfType: string;
  publicMetadata: JsonObject;
}

/** The only task shape allowed in a reader session or a preparation manifest. */
export interface TaskDependency {
  id: string;
  taskType: "single_doc" | "cross_doc";
  contextDocumentIds: string[];
  contextId: string;
  groupId: string;
}

export interface ReaderTask extends TaskDependency { question: string; }

/** Controller metadata only. No question, answer, evidence or scorer output. */
export interface DependencyRelease {
  revision: string;
  documents: SourceDocument[];
  tasks: TaskDependency[];
  groups: SourceGroup[];
  fileHashes: Record<string, string>;
}

/** Verifier-only. Do not log or serialize XlRelease into an agent workspace. */
export interface JudgeTask {
  id: string;
  originalRow: JsonObject;
}

export interface ReleaseManifest extends JsonObject {
  release_version: string;
  document_count: number;
  qa_count: number;
  single_doc_qa_count: number;
  cross_doc_qa_count: number;
  release_files: {
    documents: string;
    single_doc_questions: string;
    cross_doc_questions: string;
    [key: string]: unknown;
  };
}

export interface SourceGroup {
  id: string;
  documentIds: string[];
  taskIds: string[];
}

export interface GroupingOptions {
  /** Explicit, reviewed family membership; not inferred from answer locations. */
  sourceFamilies?: Record<string, string>;
  /** SHA-256 of acquired raw files, used to unite identical content. */
  rawSha256?: Record<string, string>;
}

export interface XlRelease {
  revision: string;
  manifest: ReleaseManifest;
  documents: SourceDocument[];
  readerTasks: ReaderTask[];
  judgeTasks: JudgeTask[];
  groups: SourceGroup[];
  fileHashes: Record<string, string>;
  releaseRoot?: string;
  nativeScorerPath?: string;
  /** Git/HF acquisition must independently establish the asserted revision. */
  revisionVerification: "caller-asserted-with-file-hashes";
}

export interface NormalizeReleaseInput extends GroupingOptions {
  revision?: string;
  manifest: unknown;
  documents: unknown[];
  singleDocQuestions: unknown[];
  crossDocQuestions: unknown[];
  fileHashes?: Record<string, string>;
}

export function sha256(value: string | Uint8Array): string {
  return createHash("sha256").update(value).digest("hex");
}

function object(value: unknown, label: string): JsonObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label}: expected object`);
  }
  return value as JsonObject;
}

function string(value: unknown, label: string, allowEmpty = false): string {
  if (typeof value !== "string" || (!allowEmpty && !value.trim())) {
    throw new Error(`${label}: expected ${allowEmpty ? "string" : "nonempty string"}`);
  }
  return value;
}

function integer(value: unknown, label: string, min = 0): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min) {
    throw new Error(`${label}: expected integer >= ${min}`);
  }
  return value;
}

function unique(values: string[], label: string): void {
  if (new Set(values).size !== values.length) throw new Error(`${label}: duplicate identifier`);
}

function canonicalUrl(value: string): string {
  const url = new URL(value);
  if (!["https:", "http:"].includes(url.protocol)) throw new Error("Source URL must use HTTP(S)");
  // Preserve path and query: distinct versions can be encoded in either.
  url.hash = "";
  return url.href;
}

function normalizeDocument(value: unknown): SourceDocument {
  const row = object(value, "document");
  const id = string(row.document_id, "document.document_id");
  const url = string(row.url, `${id}.url`);
  canonicalUrl(url);
  return {
    documentId: id,
    title: row.title === null ? null : string(row.title, `${id}.title`, true),
    url,
    sourceHost: string(row.source_host, `${id}.source_host`),
    pageCount: integer(row.page_count, `${id}.page_count`, 1),
    fileSizeBytes: integer(row.file_size_bytes, `${id}.file_size_bytes`),
    domain: string(row.domain, `${id}.domain`),
    docType: string(row.doc_type, `${id}.doc_type`),
    pdfType: string(row.pdf_type, `${id}.pdf_type`),
    publicMetadata: { ...object(row.public_metadata, `${id}.public_metadata`) },
  };
}

export function normalizeManifest(value: unknown): ReleaseManifest {
  const row = object(value, "manifest");
  const files = object(row.release_files, "manifest.release_files");
  return {
    ...row,
    release_version: string(row.release_version, "manifest.release_version"),
    document_count: integer(row.document_count, "manifest.document_count", 1),
    qa_count: integer(row.qa_count, "manifest.qa_count", 1),
    single_doc_qa_count: integer(row.single_doc_qa_count, "manifest.single_doc_qa_count"),
    cross_doc_qa_count: integer(row.cross_doc_qa_count, "manifest.cross_doc_qa_count"),
    release_files: {
      ...files,
      documents: string(files.documents, "manifest.release_files.documents"),
      single_doc_questions: string(files.single_doc_questions, "manifest.release_files.single_doc_questions"),
      cross_doc_questions: string(files.cross_doc_questions, "manifest.release_files.cross_doc_questions"),
    },
  };
}

/** Projection only: evidence, answer and answerability are not inspected here. */
export function projectTaskDependency(value: unknown, expectedType?: TaskDependency["taskType"]): TaskDependency {
  const row = object(value, "task");
  const id = string(row.question_id, "task.question_id");
  const type = row.task_type;
  if (type !== "single_doc" && type !== "cross_doc") throw new Error(`${id}: unknown task_type`);
  if (expectedType && type !== expectedType) throw new Error(`${id}: task_type disagrees with release file`);
  const contexts = type === "single_doc" ? [row.document] : row.documents;
  if (!Array.isArray(contexts) || !contexts.length) throw new Error(`${id}: missing public document context`);
  const ids = contexts.map((entry) => string(object(entry, `${id}.context`).document_id, `${id}.context.document_id`));
  unique(ids, `${id}.context`);
  if (type === "cross_doc" && ids.length < 2) throw new Error(`${id}: cross_doc needs >= 2 context documents`);
  const metadata = object(row.metadata, `${id}.metadata`);
  if (integer(metadata.n_context_documents, `${id}.n_context_documents`, 1) !== ids.length) {
    throw new Error(`${id}: context document count mismatch`);
  }
  const sorted = [...ids].sort();
  return {
    id,
    taskType: type,
    contextDocumentIds: sorted,
    contextId: `context-${sha256(JSON.stringify(sorted)).slice(0, 20)}`,
    groupId: "",
  };
}

export function projectReaderTask(value: unknown, expectedType?: ReaderTask["taskType"]): ReaderTask {
  const dependency = projectTaskDependency(value, expectedType);
  return { ...dependency, question: string(object(value, "task").question, `${dependency.id}.question`) };
}

export function normalizeDependencyRelease(input: NormalizeReleaseInput): DependencyRelease {
  const revision = input.revision ?? XL_RELEASE_REVISION;
  if (revision !== XL_RELEASE_REVISION) throw new Error("Unsupported dependency release revision");
  const manifest = normalizeManifest(input.manifest);
  const documents = input.documents.map(normalizeDocument).sort((a, b) => a.documentId.localeCompare(b.documentId));
  const single = input.singleDocQuestions.map(row => projectTaskDependency(row, "single_doc"));
  const cross = input.crossDocQuestions.map(row => projectTaskDependency(row, "cross_doc"));
  const tasks = [...single, ...cross].sort((a, b) => a.id.localeCompare(b.id));
  unique(documents.map(d => d.documentId), "documents"); unique(tasks.map(t => t.id), "tasks");
  if (documents.length !== manifest.document_count || tasks.length !== manifest.qa_count ||
      single.length !== manifest.single_doc_qa_count || cross.length !== manifest.cross_doc_qa_count) throw new Error("Dependency counts disagree with release");
  const known = new Set(documents.map(d => d.documentId)), used = new Set(tasks.flatMap(t => t.contextDocumentIds));
  if (known.size !== used.size || [...used].some(id => !known.has(id))) throw new Error("Dependency contexts must cover exactly the released documents");
  const groups = buildSourceGroups(documents, tasks, input);
  const taskGroups = new Map(groups.flatMap(g => g.taskIds.map(id => [id, g.id] as const)));
  for (const task of tasks) task.groupId = taskGroups.get(task.id)!;
  return { revision, documents, tasks, groups, fileHashes: { ...input.fileHashes } };
}

/** Connected components of public context membership, source identity and reviewed families. */
export function buildSourceGroups(documents: SourceDocument[], tasks: TaskDependency[], options: GroupingOptions = {}): SourceGroup[] {
  const parent = new Map(documents.map((d) => [d.documentId, d.documentId]));
  const find = (id: string): string => {
    let root = parent.get(id);
    if (root === undefined) throw new Error(`Unknown context document: ${id}`);
    while (root !== parent.get(root)) root = parent.get(root)!;
    let cursor = id;
    while (parent.get(cursor) !== root) {
      const next = parent.get(cursor)!;
      parent.set(cursor, root);
      cursor = next;
    }
    return root;
  };
  const union = (a: string, b: string) => {
    const roots = [find(a), find(b)].sort();
    parent.set(roots[1]!, roots[0]!);
  };
  for (const task of tasks) {
    const [first, ...rest] = task.contextDocumentIds;
    if (!first) throw new Error(`${task.id}: empty context`);
    find(first);
    for (const id of rest) union(first, id);
  }
  const identities = new Map<string, string>();
  for (const source of documents) {
    const keys = [`url:${canonicalUrl(source.url)}`];
    const family = options.sourceFamilies?.[source.documentId];
    if (family) keys.push(`family:${family}`);
    const hash = options.rawSha256?.[source.documentId];
    if (hash) {
      if (!/^[a-f0-9]{64}$/i.test(hash)) throw new Error(`Invalid raw SHA-256: ${source.documentId}`);
      keys.push(`sha256:${hash.toLowerCase()}`);
    }
    for (const key of keys) {
      const existing = identities.get(key);
      if (existing) union(source.documentId, existing);
      else identities.set(key, source.documentId);
    }
  }
  for (const map of [options.sourceFamilies, options.rawSha256]) {
    for (const id of Object.keys(map ?? {})) find(id);
  }
  const components = new Map<string, string[]>();
  for (const source of documents) {
    const root = find(source.documentId);
    components.set(root, [...(components.get(root) ?? []), source.documentId]);
  }
  return [...components.values()].map((documentIds) => {
    documentIds.sort();
    const memberIds = new Set(documentIds);
    return {
      id: `source-${sha256(JSON.stringify(documentIds)).slice(0, 20)}`,
      documentIds,
      taskIds: tasks.filter((t) => memberIds.has(t.contextDocumentIds[0]!)).map((t) => t.id).sort(),
    };
  }).sort((a, b) => a.id.localeCompare(b.id));
}

export function normalizeXlRelease(input: NormalizeReleaseInput): XlRelease {
  const revision = input.revision ?? XL_RELEASE_REVISION;
  if (revision !== XL_RELEASE_REVISION) throw new Error(`Unsupported XL release revision: ${revision}`);
  const manifest = normalizeManifest(input.manifest);
  const documents = input.documents.map(normalizeDocument).sort((a, b) => a.documentId.localeCompare(b.documentId));
  unique(documents.map((d) => d.documentId), "documents");
  const single = input.singleDocQuestions.map((r) => projectReaderTask(r, "single_doc"));
  const cross = input.crossDocQuestions.map((r) => projectReaderTask(r, "cross_doc"));
  const readerTasks = [...single, ...cross].sort((a, b) => a.id.localeCompare(b.id));
  unique(readerTasks.map((t) => t.id), "questions");
  if (documents.length !== manifest.document_count || single.length !== manifest.single_doc_qa_count ||
      cross.length !== manifest.cross_doc_qa_count || readerTasks.length !== manifest.qa_count) {
    throw new Error("Release counts disagree with manifest");
  }
  const known = new Set(documents.map((d) => d.documentId));
  const referenced = new Set(readerTasks.flatMap((t) => t.contextDocumentIds));
  if (known.size !== referenced.size || [...referenced].some((id) => !known.has(id))) {
    throw new Error("Task context IDs must reference exactly the released document set");
  }
  const groups = buildSourceGroups(documents, readerTasks, input);
  const taskGroups = new Map(groups.flatMap((g) => g.taskIds.map((id) => [id, g.id] as const)));
  for (const task of readerTasks) task.groupId = taskGroups.get(task.id)!;
  const judgeTasks = [...input.singleDocQuestions, ...input.crossDocQuestions].map((value) => {
    const row = object(value, "judge task");
    // Validation occurs in the verifier. This preserves source bytes semantically.
    return { id: string(row.question_id, "judge.question_id"), originalRow: row };
  }).sort((a, b) => a.id.localeCompare(b.id));
  return { revision, manifest, documents, readerTasks, judgeTasks, groups,
    fileHashes: { ...input.fileHashes }, revisionVerification: "caller-asserted-with-file-hashes" };
}

function releasePath(root: string, filename: string): string {
  const target = resolve(root, filename);
  const rel = relative(root, target);
  if (isAbsolute(filename) || rel === ".." || rel.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`)) {
    throw new Error(`Release file escapes source root: ${filename}`);
  }
  return target;
}

function parseJsonl(text: string, filename: string): unknown[] {
  return text.split(/\r?\n/).filter((line) => line.trim()).map((line, index) => {
    try { return JSON.parse(line); } catch { throw new Error(`${filename}: invalid JSON at nonempty row ${index + 1}`); }
  });
}

/** Reads a locally acquired release. No network, model calls or implicit downloads. */
export async function loadXlRelease(options: { releaseRoot: string; revision?: string } & GroupingOptions): Promise<XlRelease> {
  const { root, scorerPath, input } = await loadReleaseInput(options);
  return { ...normalizeXlRelease(input), releaseRoot: root, nativeScorerPath: scorerPath };
}

/** Parses the pinned local rows but selects only their complete public dependencies. */
export async function loadDependencyRelease(options: { releaseRoot: string; revision?: string } & GroupingOptions): Promise<DependencyRelease> {
  return normalizeDependencyRelease((await loadReleaseInput(options)).input);
}

async function loadReleaseInput(options: { releaseRoot: string; revision?: string } & GroupingOptions) {
  const root = resolve(options.releaseRoot);
  const manifestBytes = await readFile(resolve(root, "manifest.json"));
  const manifest = normalizeManifest(JSON.parse(manifestBytes.toString("utf8")));
  if (manifest.release_version !== XL_RELEASE_VERSION || manifest.document_count !== 292 || manifest.qa_count !== 1345) {
    throw new Error("Source is not the pinned conservative XL-DocBench release");
  }
  const names = [manifest.release_files.documents, manifest.release_files.single_doc_questions, manifest.release_files.cross_doc_questions];
  const bytes = await Promise.all(names.map((name) => readFile(releasePath(root, name))));
  const scorerPath = resolve(root, "code/evaluate.py");
  const scorerBytes = await readFile(scorerPath);
  const scorerHash = sha256(scorerBytes);
  if (scorerHash !== XL_NATIVE_SCORER_SHA256 || manifest.score_evaluator_sha256 !== XL_NATIVE_SCORER_SHA256) {
    throw new Error("Native evaluator hash differs from the pinned release");
  }
  const fileHashes = Object.fromEntries(names.map((name, i) => [name, sha256(bytes[i]!)]));
  fileHashes["manifest.json"] = sha256(manifestBytes);
  fileHashes["code/evaluate.py"] = scorerHash;
  const input: NormalizeReleaseInput = { ...options, manifest,
    documents: parseJsonl(bytes[0]!.toString("utf8"), names[0]!),
    singleDocQuestions: parseJsonl(bytes[1]!.toString("utf8"), names[1]!),
    crossDocQuestions: parseJsonl(bytes[2]!.toString("utf8"), names[2]!), fileHashes };
  return { root, scorerPath, input };
}

export interface SplitManifest {
  schema: "gptgrep.xl-source-split.v1";
  revision: string;
  seed: string;
  groups: SourceGroup[];
  dev: { groupIds: string[]; taskIds: string[]; documentIds: string[] };
  frozen: { groupIds: string[]; taskIds: string[]; documentIds: string[] };
  exposedDocumentIds: string[];
  groupingRule: string;
}

export function splitBySourceGroups(release: XlRelease, options: {
  seed: string; devGroupCount?: number; devFraction?: number; exposedDocumentIds?: string[];
}): SplitManifest {
  if (!options.seed) throw new Error("A deterministic split seed is required");
  if (options.devGroupCount !== undefined && options.devFraction !== undefined) throw new Error("Choose devGroupCount or devFraction");
  const groups = [...release.groups].sort((a, b) => sha256(`${options.seed}\0${a.id}`).localeCompare(sha256(`${options.seed}\0${b.id}`)));
  const exposed = [...new Set(options.exposedDocumentIds ?? [])].sort();
  const known = new Set(release.documents.map((d) => d.documentId));
  if (exposed.some((id) => !known.has(id))) throw new Error("Unknown exposed document ID");
  const fraction = options.devFraction ?? 0.2;
  if (!Number.isFinite(fraction) || fraction < 0 || fraction > 1) throw new Error("devFraction must be between 0 and 1");
  const count = options.devGroupCount ?? Math.ceil(groups.length * fraction);
  integer(count, "devGroupCount");
  if (count > groups.length) throw new Error("devGroupCount exceeds available groups");
  const exposedSet = new Set(exposed);
  const devIds = new Set(groups.filter((g) => g.documentIds.some((id) => exposedSet.has(id))).map((g) => g.id));
  // Exposure overrides target size; exposed material is never left in frozen.
  for (const group of groups) if (devIds.size < count) devIds.add(group.id);
  const partition = (selected: SourceGroup[]) => ({
    groupIds: selected.map((g) => g.id).sort(),
    taskIds: selected.flatMap((g) => g.taskIds).sort(),
    documentIds: selected.flatMap((g) => g.documentIds).sort(),
  });
  return {
    schema: "gptgrep.xl-source-split.v1", revision: release.revision, seed: options.seed,
    groups: [...release.groups], dev: partition(groups.filter((g) => devIds.has(g.id))),
    frozen: partition(groups.filter((g) => !devIds.has(g.id))), exposedDocumentIds: exposed,
    groupingRule: "connected public context membership + exact canonical source URL + supplied raw SHA-256 + reviewed source families",
  };
}

export function selectReaderTasks(tasks: ReaderTask[], options: { taskIds?: string[]; limit?: number } = {}): ReaderTask[] {
  unique(tasks.map((t) => t.id), "input tasks");
  let selected = [...tasks].sort((a, b) => a.id.localeCompare(b.id));
  if (options.taskIds) {
    unique(options.taskIds, "selected task IDs");
    const known = new Set(tasks.map((t) => t.id));
    for (const id of options.taskIds) if (!known.has(id)) throw new Error(`Unknown selected task: ${id}`);
    const ids = new Set(options.taskIds);
    selected = selected.filter((t) => ids.has(t.id));
  }
  if (options.limit !== undefined) {
    integer(options.limit, "limit", 1);
    selected = selected.slice(0, options.limit);
  }
  return selected;
}
