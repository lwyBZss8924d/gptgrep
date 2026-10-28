import { digest } from "../../benchmark/src/io";
import { validateMutationPaths } from "./mutation-paths";

export interface Hypothesis {
  schema: "gptgrep.opt-hypothesis.v1";
  id: string;
  kind: "invariant" | "workflow";
  capability: "agentic_parse" | "hybrid_index" | "decision_helper" | "hybrid_search";
  claim: string;
  expectedBehavior: string;
  falsification: string;
  mutationPaths: string[];
  feedbackIds: string[];
}
export interface DevFeedback {
  schema: "gptgrep.opt-dev-feedback.v1";
  id: string;
  split: "dev";
  scenarioFingerprint: string;
  candidateRevision: string;
  category: "success" | "abstention" | "operational_failure" | "unknown";
  observation: string;
  source: { path: string; sha256: string };
}
export interface BehavioralContrast {
  schema: "gptgrep.opt-contrast.v1";
  hypothesisId: string;
  incumbentFeedbackId: string;
  candidateFeedbackId: string;
  observedDifference: string;
  supportsHypothesis: boolean | null;
  compositionAllowed: false;
}

export function closed(value: unknown, keys: string[], label: string): asserts value is Record<string, any> {
  if (!value || typeof value !== "object" || Array.isArray(value) ||
      Object.keys(value).sort().join("\0") !== [...keys].sort().join("\0")) throw new Error(`${label} has missing or unknown fields`);
}
export function boundedText(value: unknown, label: string, max = 2000): asserts value is string {
  if (typeof value !== "string" || !value.trim() || value.length > max || value.includes("\0")) throw new Error(`Invalid ${label}`);
}
export function identity(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$/.test(value) || ["constructor", "prototype", "__proto__"].includes(value)) throw new Error(`Invalid ${label}`);
}
export function hash(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value)) throw new Error(`Invalid ${label}`);
}
export function revision(value: unknown): asserts value is string {
  if (typeof value !== "string" || !/^[a-f0-9]{40,64}$/.test(value)) throw new Error("Immutable source revision required");
}
function ids(value: unknown, label: string): asserts value is string[] {
  if (!Array.isArray(value) || value.length > 64 || new Set(value).size !== value.length) throw new Error(`Invalid ${label}`);
  value.forEach(v => identity(v, label));
}
export function validateHypothesis(value: Hypothesis): void {
  closed(value, ["schema", "id", "kind", "capability", "claim", "expectedBehavior", "falsification", "mutationPaths", "feedbackIds"], "hypothesis");
  if (value.schema !== "gptgrep.opt-hypothesis.v1" || !["invariant", "workflow"].includes(value.kind) ||
      !["agentic_parse", "hybrid_index", "decision_helper", "hybrid_search"].includes(value.capability)) throw new Error("Unknown hypothesis contract");
  identity(value.id, "hypothesis ID");
  for (const key of ["claim", "expectedBehavior", "falsification"] as const) boundedText(value[key], key);
  validateMutationPaths(value.mutationPaths); ids(value.feedbackIds, "feedback IDs");
}
export function validateDevFeedback(value: DevFeedback): void {
  closed(value, ["schema", "id", "split", "scenarioFingerprint", "candidateRevision", "category", "observation", "source"], "dev feedback");
  if (value.schema !== "gptgrep.opt-dev-feedback.v1" || value.split !== "dev" ||
      !["success", "abstention", "operational_failure", "unknown"].includes(value.category)) throw new Error("Only allowlisted development feedback is accepted");
  identity(value.id, "feedback ID"); hash(value.scenarioFingerprint, "scenario fingerprint"); revision(value.candidateRevision);
  boundedText(value.observation, "observation"); closed(value.source, ["path", "sha256"], "feedback source pin");
  boundedText(value.source.path, "feedback source path", 4096); hash(value.source.sha256, "feedback source hash");
}
/** The owner supplies an already reviewed dev projection, never a raw judge/ATIF object. */
export function proposalContext(hypothesis: Hypothesis, feedback: DevFeedback[], scenario: string) {
  validateHypothesis(hypothesis); hash(scenario, "scenario fingerprint");
  if (new Set(feedback.map(f => f.id)).size !== feedback.length) throw new Error("Duplicate feedback");
  feedback.forEach(f => { validateDevFeedback(f); if (f.scenarioFingerprint !== scenario) throw new Error("Feedback scenario mismatch"); });
  if (hypothesis.feedbackIds.some(id => !feedback.some(f => f.id === id)) || feedback.some(f => !hypothesis.feedbackIds.includes(f.id))) throw new Error("Feedback selection differs from hypothesis");
  return { schema: "gptgrep.opt-proposal-context.v1", hypothesis, feedback, scenarioFingerprint: scenario,
    compositionAllowed: false, contextSha256: digest({ hypothesis, feedback, scenario }) };
}
export function validateContrast(value: BehavioralContrast, hypothesis: Hypothesis, feedback: DevFeedback[]): void {
  closed(value, ["schema", "hypothesisId", "incumbentFeedbackId", "candidateFeedbackId", "observedDifference", "supportsHypothesis", "compositionAllowed"], "contrast");
  validateHypothesis(hypothesis); feedback.forEach(validateDevFeedback);
  if (value.schema !== "gptgrep.opt-contrast.v1" || value.hypothesisId !== hypothesis.id || value.compositionAllowed !== false ||
      ![true, false, null].includes(value.supportsHypothesis)) throw new Error("Invalid contrast contract");
  const before = feedback.find(f => f.id === value.incumbentFeedbackId), after = feedback.find(f => f.id === value.candidateFeedbackId);
  if (!before || !after || before.id === after.id || before.candidateRevision === after.candidateRevision || before.scenarioFingerprint !== after.scenarioFingerprint) throw new Error("Contrast requires distinct matched development artifacts");
  if ((before.category === "unknown" || after.category === "unknown") && value.supportsHypothesis !== null) throw new Error("Unknown evidence cannot support a contrast conclusion");
  boundedText(value.observedDifference, "observed difference");
}
