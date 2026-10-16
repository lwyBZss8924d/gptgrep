import { expect, test } from "bun:test";
import { validateJudgeAnswer } from "../src/judge";
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
