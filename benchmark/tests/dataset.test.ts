import { describe, expect, test } from "bun:test";
import { buildSourceGroups, normalizeXlRelease, projectReaderTask, selectReaderTasks, sha256, splitBySourceGroups, type NormalizeReleaseInput } from "../src/dataset";
import { corpusDigest, harborTaskName, verifyCommand } from "../src/adapter";
import { validateFrozenCohort } from "../src/cohort";

function document(id: string, url = `https://example.org/${id}.pdf`) {
  return { document_id: id, title: id, url, source_host: "example.org", page_count: 10,
    file_size_bytes: 1000, domain: "synthetic", doc_type: "report", pdf_type: "text", public_metadata: {} };
}

function task(id: string, ids: string[]) {
  const context = ids.map((document_id) => ({ document_id, evidence_pages: [4], evidence_items: [{ locator: "PRIVATE_EVIDENCE" }] }));
  return { question_id: id, task_type: ids.length === 1 ? "single_doc" : "cross_doc",
    question: "Find the requested value in the supplied documents.",
    ...(ids.length === 1 ? { document: context[0] } : { documents: context }),
    answer: { value: "PRIVATE_GOLD", format: "Str" },
    metadata: { n_context_documents: ids.length, n_evidence_documents: ids.length, answerability: "PRIVATE_LABEL" } };
}

function fixture(): NormalizeReleaseInput {
  return {
    manifest: { release_version: "fixture", document_count: 5, qa_count: 5, single_doc_qa_count: 3, cross_doc_qa_count: 2,
      release_files: { documents: "data/documents.jsonl", single_doc_questions: "data/single.jsonl", cross_doc_questions: "data/cross.jsonl" } },
    documents: ["alpha", "beta", "gamma", "delta", "epsilon"].map((id) => document(id)),
    singleDocQuestions: [task("s-one", ["alpha"]), task("s-two", ["delta"]), task("s-three", ["epsilon"])],
    crossDocQuestions: [task("x-one", ["alpha", "beta"]), task("x-two", ["beta", "gamma"])],
  };
}

describe("reader and judge separation", () => {
  test("preserves unavailable public titles without inventing metadata", () => {
    const input = fixture();
    (input.documents[0] as Record<string, unknown>).title = null;
    expect(normalizeXlRelease(input).documents[0]!.title).toBeNull();
    (input.documents[0] as Record<string, unknown>).title = 12;
    expect(() => normalizeXlRelease(input)).toThrow("title");
  });
  test("context projection is invariant to changed, absent or contradictory gold evidence", () => {
    const original = task("x-one", ["alpha", "beta"]);
    const changed = structuredClone(original);
    changed.documents![0]!.evidence_pages = [];
    changed.documents![1]!.evidence_items = [];
    changed.answer.value = "DIFFERENT_PRIVATE_GOLD";
    changed.metadata.n_evidence_documents = 0;
    changed.metadata.answerability = "different";
    expect(projectReaderTask(changed)).toEqual(projectReaderTask(original));
    expect(projectReaderTask(changed).contextDocumentIds).toEqual(["alpha", "beta"]);
    const text = JSON.stringify(projectReaderTask(changed));
    for (const forbidden of ["PRIVATE", "evidence", "answerability", "answer"]) expect(text).not.toContain(forbidden);
  });

  test("an unanswerable cross-doc task retains the full context", () => {
    const row = task("no-answer", ["alpha", "beta", "gamma"]);
    row.answer = { value: "None", format: "None" };
    for (const doc of row.documents!) { doc.evidence_pages = []; doc.evidence_items = []; }
    row.metadata.n_evidence_documents = 0;
    expect(projectReaderTask(row).contextDocumentIds.length).toBe(3);
  });

  test("counts, duplicate task IDs and missing source IDs fail before staging", () => {
    const wrongCount = fixture();
    (wrongCount.manifest as Record<string, unknown>).qa_count = 99;
    expect(() => normalizeXlRelease(wrongCount)).toThrow("counts");
    const duplicate = fixture();
    duplicate.singleDocQuestions[1] = duplicate.singleDocQuestions[0];
    expect(() => normalizeXlRelease(duplicate)).toThrow("duplicate");
    const missing = fixture();
    missing.crossDocQuestions[0] = task("x-one", ["alpha", "absent"]);
    expect(() => normalizeXlRelease(missing)).toThrow("released document set");
    const wrongContext = task("x-one", ["alpha", "beta"]);
    wrongContext.metadata.n_context_documents = 1;
    expect(() => projectReaderTask(wrongContext)).toThrow("context document count");
  });

  test("normalized reader records omit gold while verifier preserves it", () => {
    const release = normalizeXlRelease(fixture());
    expect(JSON.stringify(release.readerTasks)).not.toContain("PRIVATE");
    expect(JSON.stringify(release.judgeTasks)).toContain("PRIVATE_GOLD");
  });
});

describe("source-family partitions", () => {
  test("frozen exports reject changed bytes, page counts, questions and release files with unchanged IDs", () => {
    const release = normalizeXlRelease(fixture());
    const task = release.readerTasks[0]!;
    const documents = task.contextDocumentIds.map(documentId => ({ documentId, path: `/source/${documentId}.pdf`, sha256: sha256(documentId), pageCount: 10 }));
    const cohort = { schema: "gptgrep.xl-cohort.v1", revision: release.revision, split: "dev", seed: "test", sampleSize: 1,
      taskIds: [task.id], readerTasks: [task], documents, corpusSha256: corpusDigest(documents), sourceFileHashes: release.fileHashes };
    expect(validateFrozenCohort(cohort, release, documents).taskIds).toEqual([task.id]);
    expect(() => validateFrozenCohort(cohort, release, documents.map(d => ({ ...d, sha256: sha256("changed bytes") })))).toThrow("corpus");
    expect(() => validateFrozenCohort(cohort, release, documents.map(d => ({ ...d, pageCount: 11 })))).toThrow("corpus");
    const changedQuestion = structuredClone(release); changedQuestion.readerTasks[0]!.question = "changed question";
    expect(() => validateFrozenCohort(cohort, changedQuestion, documents)).toThrow("questions");
    expect(() => validateFrozenCohort(cohort, { ...release, fileHashes: { ...release.fileHashes, added: sha256("new") } }, documents)).toThrow("release");
  });
  test("overlapping multi-document contexts form transitive components", () => {
    const release = normalizeXlRelease(fixture());
    const group = release.groups.find((g) => g.documentIds.includes("alpha"))!;
    expect(group.documentIds).toEqual(["alpha", "beta", "gamma"]);
    expect(group.taskIds).toEqual(["s-one", "x-one", "x-two"]);
  });

  test("exact URLs, acquired file hashes and reviewed families unite related sources", () => {
    const release = normalizeXlRelease(fixture());
    const docs = structuredClone(release.documents);
    docs.find((d) => d.documentId === "delta")!.url = docs.find((d) => d.documentId === "epsilon")!.url;
    expect(buildSourceGroups(docs, release.readerTasks).length).toBe(2);
    expect(buildSourceGroups(release.documents, release.readerTasks, {
      rawSha256: { delta: sha256("same PDF"), epsilon: sha256("same PDF") },
    }).length).toBe(2);
    expect(buildSourceGroups(release.documents, release.readerTasks, {
      sourceFamilies: { alpha: "reviewed-family", epsilon: "reviewed-family" },
    }).length).toBe(2);
  });

  test("frozen never overlaps dev and exposure takes precedence over requested size", () => {
    const release = normalizeXlRelease(fixture());
    const split = splitBySourceGroups(release, { seed: "fixed", devGroupCount: 0, exposedDocumentIds: ["beta"] });
    expect(split.dev.documentIds).toEqual(["alpha", "beta", "gamma"]);
    expect(split.dev.taskIds).toEqual(["s-one", "x-one", "x-two"]);
    expect(split.frozen.documentIds).toEqual(["delta", "epsilon"]);
    expect(split.dev.documentIds.some((id) => split.frozen.documentIds.includes(id))).toBe(false);
    expect(splitBySourceGroups(release, { seed: "fixed", devGroupCount: 1 }))
      .toEqual(splitBySourceGroups(release, { seed: "fixed", devGroupCount: 1 }));
  });

  test("selection validates IDs before limiting, never silently substitutes tasks", () => {
    const tasks = normalizeXlRelease(fixture()).readerTasks;
    expect(() => selectReaderTasks(tasks, { taskIds: ["missing"], limit: 1 })).toThrow("Unknown selected task");
    expect(() => selectReaderTasks(tasks, { taskIds: ["s-one", "s-one"] })).toThrow("duplicate");
    expect(selectReaderTasks(tasks, { taskIds: ["x-two", "x-one"], limit: 1 }).map((t) => t.id)).toEqual(["x-one"]);
  });
});

describe("portable adapter contracts", () => {
  test("registry names cannot collide after upstream ID sanitization", () => {
    expect(harborTaskName("A/B")).not.toEqual(harborTaskName("A_B"));
    expect(harborTaskName("A/B")).toMatch(/^gptgrep\/[a-z0-9-]+$/);
    expect(harborTaskName("A/B")).toEqual(harborTaskName("A/B"));
  });

  test("corpus binding is independent of input order and changes with actual bytes", () => {
    const a = { documentId: "alpha", sha256: sha256("PDF one") };
    const b = { documentId: "beta", sha256: sha256("PDF two") };
    expect(corpusDigest([a, b])).toEqual(corpusDigest([b, a]));
    expect(corpusDigest([a, b])).not.toEqual(corpusDigest([a, { ...b, sha256: sha256("changed PDF") }]));
  });

  test("verifier command uses offline prepared uv environment and a controller judgment", () => {
    const command = verifyCommand({ verifierRoot: "/verifier", answerPath: "/frozen/answer.json",
      expectedPath: "/verifier/expected.json", judgmentPath: "/controller/judgment.json", outputDir: "/output" });
    expect(command.command).toBe("uv");
    expect(command.args).toContain("--offline");
    expect(command.args).toContain("--rewardkit");
    expect(command.args).toContain("/controller/judgment.json");
  });
});
