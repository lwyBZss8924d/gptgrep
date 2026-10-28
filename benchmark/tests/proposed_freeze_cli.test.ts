import { describe, expect, test } from "bun:test";
import { assertDependencyReleaseMatches, buildSourceGroups, normalizeDependencyRelease, normalizeXlRelease, parseExposureLedger, parseSourceFamilies, splitBySourceGroups, type NormalizeReleaseInput } from "../src/dataset";

function document(id: string, url = `https://example.org/${id}.pdf`) {
  return { document_id: id, title: id, url, source_host: "example.org", page_count: 10,
    file_size_bytes: 1000, domain: "synthetic", doc_type: "report", pdf_type: "text", public_metadata: {} };
}

function task(id: string, ids: string[]) {
  const context = ids.map((document_id) => ({ document_id, evidence_pages: [1], evidence_items: [] }));
  return {
    question_id: id,
    task_type: ids.length === 1 ? "single_doc" : "cross_doc",
    question: "Test question",
    ...(ids.length === 1 ? { document: context[0] } : { documents: context }),
    answer: { value: "test", format: "Str" },
    metadata: { n_context_documents: ids.length, n_evidence_documents: ids.length, answerability: "test" }
  };
}

function fixture(): NormalizeReleaseInput {
  return {
    manifest: {
      release_version: "fixture",
      document_count: 6,
      qa_count: 6,
      single_doc_qa_count: 4,
      cross_doc_qa_count: 2,
      release_files: { documents: "docs.jsonl", single_doc_questions: "s.jsonl", cross_doc_questions: "x.jsonl" }
    },
    documents: [
      document("doc_A", "https://imf.org/gfsr/2018.pdf"),
      document("doc_B", "https://imf.org/gfsr/2019.pdf"),
      document("doc_C", "https://unctad.org/wir/2019.pdf"),
      document("doc_D", "https://unctad.org/wir/2020.pdf"),
      document("doc_E", "https://other.org/docE.pdf"),
      document("doc_F", "https://other.org/docF.pdf"),
    ],
    singleDocQuestions: [
      task("task_A", ["doc_A"]),
      task("task_B", ["doc_B"]),
      task("task_C", ["doc_C"]),
      task("task_D", ["doc_D"]),
    ],
    crossDocQuestions: [
      task("task_AB", ["doc_A", "doc_B"]),
      task("task_EF", ["doc_E", "doc_F"]),
    ]
  };
}

describe("zero family leakage and exposure ledger splitting", () => {
  test("family records and exposure events fail closed instead of silently dropping malformed entries", () => {
    expect(parseSourceFamilies({ schema: "gptgrep.source-families.v1", families: { doc_A: "series" } })).toEqual({ doc_A: "series" });
    expect(() => parseSourceFamilies({ families: { doc_A: 3 } })).toThrow();
    const ledger = { schema: "gptgrep.dev-exposure-ledger.v1", exposure_events: [{ event_id: "one", description: "Observed context", document_ids: ["doc_A", "doc_B"] }], exposed_document_ids: ["doc_A", "doc_B"] };
    expect(parseExposureLedger(ledger)).toEqual(["doc_A", "doc_B"]);
    expect(() => parseExposureLedger({ ...ledger, exposed_document_ids: ["doc_A"] })).toThrow("event union");
    expect(() => parseExposureLedger({ ...ledger, exposed_document_ids: ["doc_A", "doc_B", "doc_C"] })).toThrow("event union");
    expect(() => parseExposureLedger({ ...ledger, exposure_events: [...ledger.exposure_events, ...ledger.exposure_events] })).toThrow("duplicate");
  });
  test("all task groups must match the assessment even if only another task is selected", () => {
    const input = fixture();
    const expected = normalizeDependencyRelease(input), current = normalizeXlRelease(input);
    expect(() => assertDependencyReleaseMatches(expected, current)).not.toThrow();
    const changed = normalizeXlRelease({ ...input, sourceFamilies: { doc_C: "series", doc_D: "series" } });
    expect(() => assertDependencyReleaseMatches(expected, changed)).toThrow("Complete release dependencies");
  });
  test("sourceFamilies clusters disjoint single-doc series into the same group", () => {
    const input = fixture();
    // doc_C and doc_D have only single-doc tasks, but share unctad-wir family
    input.sourceFamilies = {
      doc_C: "unctad-wir",
      doc_D: "unctad-wir"
    };
    const release = normalizeXlRelease(input);
    const unctadGroup = release.groups.find(g => g.documentIds.includes("doc_C"));
    expect(unctadGroup).toBeDefined();
    expect(unctadGroup!.documentIds).toEqual(["doc_C", "doc_D"]);
    expect(unctadGroup!.taskIds).toEqual(["task_C", "task_D"]);
  });

  test("exposed document unconditionally pulls its entire family group into dev", () => {
    const input = fixture();
    input.sourceFamilies = {
      doc_C: "unctad-wir",
      doc_D: "unctad-wir"
    };
    const release = normalizeXlRelease(input);
    
    // doc_C is exposed
    const split = splitBySourceGroups(release, {
      seed: "fixed-seed",
      devGroupCount: 1,
      exposedDocumentIds: ["doc_C"]
    });

    // dev must contain unctadGroup (both doc_C and doc_D)
    expect(split.dev.documentIds).toContain("doc_C");
    expect(split.dev.documentIds).toContain("doc_D");
    expect(split.dev.taskIds).toContain("task_C");
    expect(split.dev.taskIds).toContain("task_D");

    // frozen must NOT contain doc_C or doc_D or their tasks
    expect(split.frozen.documentIds).not.toContain("doc_C");
    expect(split.frozen.documentIds).not.toContain("doc_D");
    expect(split.frozen.taskIds).not.toContain("task_C");
    expect(split.frozen.taskIds).not.toContain("task_D");

    // Zero cross-split leakage invariants
    const devDocSet = new Set(split.dev.documentIds);
    const devTaskSet = new Set(split.dev.taskIds);
    const devGroupSet = new Set(split.dev.groupIds);

    expect(split.frozen.documentIds.some(id => devDocSet.has(id))).toBe(false);
    expect(split.frozen.taskIds.some(id => devTaskSet.has(id))).toBe(false);
    expect(split.frozen.groupIds.some(id => devGroupSet.has(id))).toBe(false);
  });

  test("deterministic split with seed is reproducible", () => {
    const input = fixture();
    const release = normalizeXlRelease(input);
    const split1 = splitBySourceGroups(release, { seed: "seed-alpha", devGroupCount: 2 });
    const split2 = splitBySourceGroups(release, { seed: "seed-alpha", devGroupCount: 2 });
    expect(split1).toEqual(split2);
  });
});
