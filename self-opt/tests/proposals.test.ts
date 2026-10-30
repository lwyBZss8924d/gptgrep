import { expect, test } from "bun:test";
import { proposalContext, validateContrast, validateDevFeedback, validateHypothesis, type DevFeedback, type Hypothesis } from "../src/proposals";

const scenario = "a".repeat(64);
const hypothesis: Hypothesis = { schema: "gptgrep.opt-hypothesis.v1", id: "fixture-hypothesis", kind: "invariant", capability: "hybrid_search",
  claim: "Preserve document identity during context lookup", expectedBehavior: "References retain their originating document", falsification: "A reference changes origin",
  mutationPaths: ["prompts/reader.md"], feedbackIds: ["before", "after"] };
const feedback: DevFeedback[] = ["before", "after"].map((id, index) => ({ schema: "gptgrep.opt-dev-feedback.v1", id, split: "dev", scenarioFingerprint: scenario,
  candidateRevision: String(index + 1).repeat(40), category: "success", observation: "A fixture observation with no question, gold or judge payload",
  source: { path: `/fixture/${id}.json`, sha256: String(index + 1).repeat(64) } }));

test("proposal context preserves an exact matched dev projection and disables composition", () => {
  const context = proposalContext(hypothesis, feedback, scenario);
  expect(context.compositionAllowed).toBe(false); expect(context.feedback).toEqual(feedback);
  expect(context.contextSha256).toHaveLength(64);
});
test("closed schemas reject judge/gold extensions, frozen feedback and protected mutations", () => {
  expect(() => validateDevFeedback({ ...feedback[0]!, judge: {} } as any)).toThrow("unknown fields");
  expect(() => validateDevFeedback({ ...feedback[0]!, split: "frozen" } as any)).toThrow("development");
  expect(() => validateHypothesis({ ...hypothesis, mutationPaths: ["benchmark/src/rubric.ts"] })).toThrow("outside");
  expect(() => validateHypothesis({ ...hypothesis, mutationPaths: ["prompts/../benchmark/task.json"] })).toThrow("outside");
  expect(() => validateHypothesis({ ...hypothesis, id: "constructor" })).toThrow("Invalid");
});
test("context rejects missing, extra, duplicated and foreign-scenario feedback", () => {
  expect(() => proposalContext(hypothesis, feedback.slice(0, 1), scenario)).toThrow("selection");
  expect(() => proposalContext(hypothesis, [...feedback, feedback[0]!], scenario)).toThrow("Duplicate");
  expect(() => proposalContext(hypothesis, [{ ...feedback[0]!, scenarioFingerprint: "b".repeat(64) }, feedback[1]!], scenario)).toThrow("scenario");
});
test("behavioral contrast cannot convert unknown evidence to a supported conclusion", () => {
  const contrast = { schema: "gptgrep.opt-contrast.v1", hypothesisId: hypothesis.id, incumbentFeedbackId: "before", candidateFeedbackId: "after",
    observedDifference: "No established difference", supportsHypothesis: null, compositionAllowed: false } as const;
  validateContrast(contrast, hypothesis, feedback);
  const unknown = [{ ...feedback[0]!, category: "unknown" as const }, feedback[1]!];
  validateContrast(contrast, hypothesis, unknown);
  expect(() => validateContrast({ ...contrast, supportsHypothesis: true }, hypothesis, unknown)).toThrow("Unknown");
  expect(() => validateContrast({ ...contrast, candidateFeedbackId: "before" }, hypothesis, feedback)).toThrow("distinct");
});
