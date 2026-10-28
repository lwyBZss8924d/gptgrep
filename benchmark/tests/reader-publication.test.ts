import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { publicDocument, publicReaderTask, validatePublicRows, verifyPublicStaging, type PublicFile, type PublicStagingManifest } from "../src/reader-publication";
import { canonical, sha256 } from "../src/io";

const task = { id: "task", taskType: "single_doc" as const, contextDocumentIds: ["source"], contextId: "context", groupId: "group", question: "Public question" };
test("closed reader projection drops all nested evaluator payload and validation rejects it", () => {
  const input = { ...task, answer: "PRIVATE", metadata: { is_unanswerable: true, candidate_evidence_sources: ["PRIVATE"] } };
  expect(publicReaderTask(input)).toEqual(task);
  expect(() => validatePublicRows([input], "single_doc")).toThrow("nested judge/private");
  expect(() => validatePublicRows([{ ...task, contextDocumentIds: [{ documentId: "source", evidence: [1] }] }], "single_doc")).toThrow();
  const document = { documentId: "source", title: "Title", url: "https://example.invalid/source.pdf", sourceHost: "example.invalid", pageCount: 2,
    fileSizeBytes: 10, domain: "test", docType: "report", pdfType: "text", publicMetadata: { answer: "PRIVATE" } };
  expect(JSON.stringify(publicDocument(document))).not.toContain("PRIVATE");
  expect(() => validatePublicRows([document], "document")).toThrow("nested judge/private");
});

async function staging() {
  const root = await mkdtemp(join(tmpdir(), "public-staging-")); await mkdir(join(root, "data"));
  const files: PublicFile[] = [];
  const put = async (path: string, content: string, format: PublicFile["format"], rowKind: PublicFile["rowKind"]) => {
    await writeFile(join(root, path), content); files.push({ path, sha256: sha256(content), bytes: Buffer.byteLength(content), format, rowKind });
  };
  await put("data/qa_single_doc.jsonl", `${canonical(task)}\n`, "jsonl", "single_doc");
  const manifest: PublicStagingManifest = { schema: "gptgrep.public-reader-staging.v1", releaseRevision: "a".repeat(40), files };
  return { root, manifest, put };
}

test("upload verification rejects unlisted caches, tampering and private path payloads", async () => {
  const f = await staging();
  try {
    expect(await verifyPublicStaging(f.root, f.manifest, null)).toEqual({ files: 1, jsonlRows: 1, parquetRows: 0 });
    await writeFile(join(f.root, "extra.pyc"), "cache");
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow("closed file allowlist");
    await rm(join(f.root, "extra.pyc"));
    await f.put("README.md", "Private /Volumes/device/path", "text", null);
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow("private path");
    await writeFile(join(f.root, "data/qa_single_doc.jsonl"), "{}");
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow("changed");
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("Parquet requires actual decoding and logical parity; fixture decoder tests only the interface", async () => {
  const f = await staging();
  try {
    // This is a deliberately invalid format fixture. It never claims Parquet runtime qualification.
    await f.put("data/qa_single_doc.parquet", "PAR1fixture-PAR1", "parquet", "single_doc");
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow("Actual Parquet decoding");
    await expect(verifyPublicStaging(f.root, f.manifest, async () => { throw new Error("decoder rejects invalid fixture"); })).rejects.toThrow("decoder rejects");
    const decoded = (rows: unknown[]) => ({ rows, fields: { id: "string", taskType: "string", contextDocumentIds: "list<string>", contextId: "string", groupId: "string", question: "string" } as const,
      fileMetadata: {}, schemaMetadata: {}, fieldMetadata: {} });
    await expect(verifyPublicStaging(f.root, f.manifest, async () => decoded([{ ...task, question: "Changed question" }]))).rejects.toThrow("differs");
    await expect(verifyPublicStaging(f.root, f.manifest, async () => decoded([{ ...task, metadata: { evidence: [1] } }]))).rejects.toThrow("nested judge/private");
    await expect(verifyPublicStaging(f.root, f.manifest, async () => ({ ...decoded([task]), fileMetadata: { pandas: "private attrs" } }))).rejects.toThrow("metadata");
    await expect(verifyPublicStaging(f.root, f.manifest, async () => ({ ...decoded([]), fields: { ...decoded([]).fields, gold: "string" } }))).rejects.toThrow("column schema");
    await expect(verifyPublicStaging(f.root, f.manifest, async () => decoded([{ ...task, question: "/Users/private/source" }]))).rejects.toThrow("decoded private");
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("duplicate JSON keys and escaped private paths cannot evade publication checks", async () => {
  const f = await staging();
  try {
    const replace = async (bytes: string) => {
      await writeFile(join(f.root, f.manifest.files[0]!.path), bytes);
      Object.assign(f.manifest.files[0]!, { bytes: Buffer.byteLength(bytes), sha256: sha256(bytes) });
    };
    await replace(canonical(task).replace('"question":"Public question"', '"question":"hidden payload","question":"Public question"') + "\n");
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow("duplicate keys");
    await replace(canonical({ ...task, question: "/Users/private/source" }).replaceAll("/", "\\u002f") + "\n");
    await expect(verifyPublicStaging(f.root, f.manifest, null)).rejects.toThrow();
  } finally { await rm(f.root, { recursive: true, force: true }); }
});
