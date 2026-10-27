import { lstat, readFile, readdir } from "node:fs/promises";
import { isAbsolute, join, relative, resolve } from "node:path";
import type { ReaderTask, SourceDocument, XlRelease } from "./dataset";
import { canonical, fileHash, physicalPath } from "./io";

export type PublicDocument = Omit<SourceDocument, "publicMetadata">;
export type PublicRowKind = "document" | "single_doc" | "cross_doc";
export interface PublicFile {
  path: string; sha256: string; bytes: number;
  format: "jsonl" | "parquet" | "text";
  rowKind: PublicRowKind | null;
}
export interface PublicStagingManifest {
  schema: "gptgrep.public-reader-staging.v1";
  releaseRevision: string;
  files: PublicFile[];
}

function object(value: unknown): Record<string, any> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Public row must be an object");
  return value as Record<string, any>;
}
function text(value: unknown): string {
  if (typeof value !== "string") throw new Error("Public scalar string required");
  return value;
}
function integer(value: unknown, minimum: number): number {
  if (!Number.isSafeInteger(value) || (value as number) < minimum) throw new Error("Public integer required");
  return value as number;
}

/** Closed projection: arbitrary nested public_metadata is deliberately not copied. */
export function publicDocument(document: SourceDocument): PublicDocument {
  return { documentId: text(document.documentId), title: document.title === null ? null : text(document.title),
    url: text(document.url), sourceHost: text(document.sourceHost), pageCount: integer(document.pageCount, 1),
    fileSizeBytes: integer(document.fileSizeBytes, 0), domain: text(document.domain), docType: text(document.docType), pdfType: text(document.pdfType) };
}

/** No answer, answerability, evidence, arbitrary metadata or private provenance. */
export function publicReaderTask(task: ReaderTask): ReaderTask {
  if (task.taskType !== "single_doc" && task.taskType !== "cross_doc" || !Array.isArray(task.contextDocumentIds)) throw new Error("Invalid public task context");
  const contextDocumentIds = task.contextDocumentIds.map(text);
  if (new Set(contextDocumentIds).size !== contextDocumentIds.length || contextDocumentIds.some(id => !id) ||
      task.taskType === "single_doc" && contextDocumentIds.length !== 1 || task.taskType === "cross_doc" && contextDocumentIds.length < 2) throw new Error("Invalid complete public task context");
  return { id: text(task.id), taskType: task.taskType, contextDocumentIds,
    contextId: text(task.contextId), groupId: text(task.groupId), question: text(task.question) };
}

export function publicReaderRows(release: XlRelease) {
  return { documents: release.documents.map(publicDocument),
    single: release.readerTasks.filter(task => task.taskType === "single_doc").map(publicReaderTask),
    cross: release.readerTasks.filter(task => task.taskType === "cross_doc").map(publicReaderTask) };
}

/** Reject unexpected fields at every retained object level, including nested arrays. */
export function validatePublicRows(rows: unknown[], kind: PublicRowKind): (PublicDocument | ReaderTask)[] {
  if (!Array.isArray(rows)) throw new Error("Decoded public rows must be an array");
  const projected = rows.map(row => {
    const value = object(row);
    const result = kind === "document" ? publicDocument(value as SourceDocument) : publicReaderTask(value as ReaderTask);
    if (kind !== "document" && (result as ReaderTask).taskType !== kind) throw new Error("Public task is in the wrong partition");
    if (canonical(result) !== canonical(row)) throw new Error("Public row contains unknown or nested judge/private metadata");
    return result;
  });
  const ids = projected.map(row => "documentId" in row ? row.documentId : row.id);
  if (ids.some(id => !id) || new Set(ids).size !== ids.length) throw new Error("Duplicate or empty public row ID");
  return projected;
}

function relativeFile(path: string): void {
  if (!path || isAbsolute(path) || path.includes("\\") || path.split("/").some(part => !part || part === "." || part === ".." || part.startsWith(".")) ||
      /(?:^|\/)(?:__pycache__|node_modules|target)(?:\/|$)/.test(path) || /\.(?:pyc|pyo|env|db|sqlite|pdf)$/i.test(path)) throw new Error("Unsafe public file path");
}

/** The pinned decoder reports every footer/schema/field metadata entry, including nested fields. */
export interface DecodedParquet {
  rows: unknown[];
  fields: Record<string, "string" | "integer" | "list<string>">;
  fileMetadata: Record<string, string>;
  schemaMetadata: Record<string, string>;
  fieldMetadata: Record<string, Record<string, string>>;
}
export type ParquetDecoder = (path: string) => Promise<DecodedParquet>;

function rejectPrivateStrings(value: unknown, markers: string[]): void {
  if (typeof value === "string" && markers.some(marker => value.includes(marker))) throw new Error("Public staging contains a decoded private path or identity marker");
  if (Array.isArray(value)) value.forEach(item => rejectPrivateStrings(item, markers));
  else if (value !== null && typeof value === "object") for (const [key, item] of Object.entries(value)) { rejectPrivateStrings(key, markers); rejectPrivateStrings(item, markers); }
}

/** Publication front doors call this immediately before upload with the exact manifest. */
export async function verifyPublicStaging(root: string, manifest: PublicStagingManifest, decodeParquet: ParquetDecoder | null,
  privateMarkers: string[] = []): Promise<{ files: number; jsonlRows: number; parquetRows: number }> {
  await physicalPath(root);
  if (canonical(Object.keys(manifest).sort()) !== canonical(["schema", "releaseRevision", "files"].sort()) ||
      manifest.schema !== "gptgrep.public-reader-staging.v1" || !/^[a-f0-9]{40}$/.test(manifest.releaseRevision) || !Array.isArray(manifest.files) || !manifest.files.length) throw new Error("Exact public staging manifest required");
  const expected = new Set<string>(), rowsByFile = new Map<string, unknown[]>();
  let jsonlRows = 0, parquetRows = 0;
  for (const entry of manifest.files) {
    if (canonical(Object.keys(entry).sort()) !== canonical(["path", "sha256", "bytes", "format", "rowKind"].sort())) throw new Error("Unknown public manifest fields");
    relativeFile(entry.path);
    if (expected.has(entry.path) || !/^[a-f0-9]{64}$/.test(entry.sha256) || !Number.isSafeInteger(entry.bytes) || entry.bytes < 0 ||
        !["jsonl", "parquet", "text"].includes(entry.format)) throw new Error("Invalid public manifest entry");
    expected.add(entry.path);
    const path = resolve(root, entry.path); await physicalPath(path);
    const info = await lstat(path);
    if (!info.isFile() || info.isSymbolicLink() || info.size !== entry.bytes || await fileHash(path) !== entry.sha256) throw new Error("Public staging file changed or is not regular");
    if (entry.format === "text") {
      if (entry.rowKind !== null || !["README.md", "LICENSE", "harbor/verify.py", "harbor/checks.py", "harbor/pyproject.toml", "harbor/test.sh", "harbor/Dockerfile"].includes(entry.path)) throw new Error("Unsupported public text asset");
    } else {
      if (!["document", "single_doc", "cross_doc"].includes(entry.rowKind ?? "")) throw new Error("Public row kind required");
      const stem = entry.rowKind === "document" ? "documents" : entry.rowKind === "single_doc" ? "qa_single_doc" : "qa_cross_doc";
      if (entry.path !== `data/${stem}.${entry.format}`) throw new Error("Public data path or partition mismatch");
    }
    const bytes = await readFile(path);
    const markers = ["/Users/", "/Volumes/", ".codex/sessions/", ...privateMarkers];
    if (markers.some(marker => !marker || bytes.includes(Buffer.from(marker)))) throw new Error("Public staging contains a private path or identity marker");
    if (entry.format === "jsonl") {
      if (!entry.path.endsWith(".jsonl")) throw new Error("JSONL extension mismatch");
      const source = bytes.toString("utf8");
      const rows = source.trim() ? source.trimEnd().split("\n").map(line => JSON.parse(line)) : [];
      const projected = validatePublicRows(rows, entry.rowKind!);
      // Requiring canonical bytes also rejects duplicate keys and hidden escaped payloads.
      if (source !== projected.map(row => canonical(row)).join("\n") + (projected.length ? "\n" : "")) throw new Error("Public JSONL must contain canonical rows without duplicate keys");
      rejectPrivateStrings(projected, markers);
      rowsByFile.set(entry.path, projected); jsonlRows += rows.length;
    } else if (entry.format === "parquet") {
      if (!entry.path.endsWith(".parquet") || bytes.length < 12 || bytes.subarray(0, 4).toString() !== "PAR1" || bytes.subarray(-4).toString() !== "PAR1" || !decodeParquet) throw new Error("Actual Parquet decoding is required");
      const decoded = await decodeParquet(path);
      const fields = entry.rowKind === "document" ? { documentId: "string", title: "string", url: "string", sourceHost: "string", pageCount: "integer",
        fileSizeBytes: "integer", domain: "string", docType: "string", pdfType: "string" } :
        { id: "string", taskType: "string", contextDocumentIds: "list<string>", contextId: "string", groupId: "string", question: "string" };
      const fieldPaths = new Set([...Object.keys(fields), "contextDocumentIds.element", "contextDocumentIds.item"]);
      if (canonical(Object.keys(decoded).sort()) !== canonical(["rows", "fields", "fileMetadata", "schemaMetadata", "fieldMetadata"].sort()) ||
          canonical(decoded.fields) !== canonical(fields) ||
          Object.keys(object(decoded.fileMetadata)).length || Object.keys(object(decoded.schemaMetadata)).length ||
          Object.entries(object(decoded.fieldMetadata)).some(([path, metadata]) => !fieldPaths.has(path) || Object.keys(object(metadata)).length)) throw new Error("Public Parquet requires the closed column schema and no footer, schema or field metadata");
      const rows = validatePublicRows(decoded.rows, entry.rowKind!); rejectPrivateStrings(rows, markers);
      if (await fileHash(path) !== entry.sha256) throw new Error("Parquet changed while decoding");
      rowsByFile.set(entry.path, rows); parquetRows += rows.length;
    }
  }
  for (const entry of manifest.files.filter(file => file.format === "parquet")) {
    const jsonlPath = entry.path.replace(/\.parquet$/, ".jsonl");
    const jsonl = manifest.files.find(file => file.path === jsonlPath && file.format === "jsonl" && file.rowKind === entry.rowKind);
    if (!jsonl || canonical(rowsByFile.get(jsonlPath)) !== canonical(rowsByFile.get(entry.path))) throw new Error("Decoded Parquet differs from its safe JSONL projection");
  }
  const actual: string[] = [];
  const visit = async (directory: string): Promise<void> => {
    for (const item of await readdir(directory, { withFileTypes: true })) {
      const path = join(directory, item.name);
      if (item.isSymbolicLink()) throw new Error("Public staging contains a symlink");
      if (item.isDirectory()) await visit(path);
      else if (item.isFile()) actual.push(relative(root, path));
      else throw new Error("Public staging contains a non-regular entry");
    }
  };
  await visit(root);
  if (canonical(actual.sort()) !== canonical([...expected].sort())) throw new Error("Public staging differs from the closed file allowlist");
  return { files: expected.size, jsonlRows, parquetRows };
}
