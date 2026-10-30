import { expect, test } from "bun:test";
import { JUDGE_MODEL, JUDGE_SCHEMA, validateJudgeAnswer } from "../src/judge";
const docs = [{ documentId: "source", pageCount: 20 }];
const valid = () => ({ status: "complete", grounded_success: 1, required_atoms: 2, supported_atoms: 2, answerability: "answerable", reason: "Both requirements are supported.", source_checks: [{ document_id: "source", page: 9, claim: "A requirement", supported: true, reason: "Visible source supports the claim." }], contradictions: [], missing_atoms: [] });
test("accept alternative source evidence within task context", () => expect(validateJudgeAnswer(valid(), docs).grounded_success).toBe(1));
test("reject unverifiable or inconsistent success", () => {
  expect(() => validateJudgeAnswer({ ...valid(), source_checks: [] }, docs)).toThrow();
  expect(() => validateJudgeAnswer({ ...valid(), supported_atoms: 1 }, docs)).toThrow();
  expect(() => validateJudgeAnswer({ ...valid(), contradictions: ["Wrong total"] }, docs)).toThrow();
  expect(() => validateJudgeAnswer({ ...valid(), source_checks: [{ ...valid().source_checks[0], page: 21 }] }, docs)).toThrow();
  expect(() => validateJudgeAnswer({ ...valid(), source_checks: [{ ...valid().source_checks[0], document_id: "other" }] }, docs)).toThrow();
});
test("unknown verdict cannot retain numeric rewards", () => {
  const result = validateJudgeAnswer({ ...valid(), status: "unknown", answerability: "uncertain" }, docs);
  expect(result.grounded_success).toBeNull(); expect(result.required_atoms).toBeNull(); expect(result.supported_atoms).toBeNull();
});
test("judge model is calibrated and pinned to gpt-6.1-sol", () => {
  expect(JUDGE_MODEL).toBe("gpt-6.1-sol");
  expect(JUDGE_SCHEMA.properties.status.enum).toEqual(["complete", "unknown"]);
  expect(JUDGE_SCHEMA.properties.grounded_success.enum).toEqual([0, 1, null]);
});
test("calibrate primary PDF citation checks: valid page numbers and documents required", () => {
  const answer = valid();
  answer.source_checks.push({ document_id: "source", page: 15, claim: "Second claim", supported: true, reason: "Verified on page 15 of primary PDF." });
  const validated = validateJudgeAnswer(answer, docs);
  expect(validated.source_checks).toHaveLength(2);
  expect(validated.grounded_success).toBe(1);

  // Missing claim text throws
  expect(() => validateJudgeAnswer({ ...valid(), source_checks: [{ document_id: "source", page: 1, claim: "  ", supported: true, reason: "x" }] }, docs)).toThrow();
  // Negative or zero page throws
  expect(() => validateJudgeAnswer({ ...valid(), source_checks: [{ document_id: "source", page: 0, claim: "valid", supported: true, reason: "x" }] }, docs)).toThrow();
});

