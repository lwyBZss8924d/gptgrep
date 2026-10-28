import { expect, test } from "bun:test";
import { captureNotification, convertNativeCapture } from "../src/trajectory";
import type { NativeRunResult } from "../src/native";

const result = { executionMode: "test", threadId: "root", turnId: "turn", sessionId: "native-session", observedModel: "gpt-6.1-sol", traceTruncated: false,
  workerModels: [{ threadId: "child", observedModel: "gpt-6-luna" }], usage: null } as NativeRunResult;
const event = (method: string, params: any) => ({ direction: "capture", at: "2026-10-25T00:00:00Z", capture: captureNotification({ method, params }) });
const completed = (threadId: string, item: any) => event("item/completed", { threadId, turnId: threadId === "child" ? "child-turn" : "turn", item });

test("private capture retains tool output, excludes auth/config and hidden reasoning, and redacts known secrets", () => {
  expect(captureNotification({ id: 1, result: { config: { secret: "secret-value" } } })).toBeNull();
  expect(captureNotification({ method: "account/updated", params: { token: "secret-value" } })).toBeNull();
  expect(captureNotification({ method: "item/reasoning/textDelta", params: { delta: "hidden" } })).toBeNull();
  const captured = captureNotification({ method: "item/completed", params: { item: { type: "commandExecution", command: "tool secret-value", aggregatedOutput: "source page text", authorization: "Bearer credential" } } }, ["secret-value"]);
  expect(JSON.stringify(captured)).toContain("source page text");
  expect(JSON.stringify(captured)).not.toContain("secret-value");
  expect(JSON.stringify(captured)).not.toContain("credential");
  const labels = captureNotification({ method: "item/completed", params: { item: { type: "commandExecution", aggregatedOutput: '{"access_token":"opaque-one"}\nPASSWORD=opaque-two\nOPENAI_API_KEY=opaque-three\n{\\"access_token\\":\\"opaque-four\\"}' } } });
  expect(JSON.stringify(labels)).not.toContain("opaque-");
  expect(labels!.redaction.strings).toBeGreaterThan(0);
});

test("ATIF binds tool observations, actual child trajectories, media pins and source lines", () => {
  const rows = [{ direction: "input", text: "Read the source" },
    completed("root", { id: "cmd", type: "commandExecution", command: "read source.pdf", aggregatedOutput: "source text", exitCode: 0 }),
    completed("root", { id: "spawn", type: "collabAgentToolCall", tool: "spawnAgent", receiverThreadIds: ["child"], prompt: "Read the source detail", model: "gpt-6-luna" }),
    completed("root", { id: "image", type: "imageView", path: "/run/page.png" }),
    completed("child", { id: "child-answer", type: "agentMessage", text: "source detail" }),
    event("turn/completed", { threadId: "child", turn: { id: "child-turn", status: "completed", items: [] } }),
    event("turn/completed", { threadId: "root", turn: { id: "turn", status: "completed", items: [{ id: "answer", type: "agentMessage", text: "answer" }] } })];
  const converted = convertNativeCapture(rows, result, [{ sourcePath: "/run/page.png", artifact: { path: "/private/media/page.png", sha256: "a".repeat(64) }, mediaType: "image/png" }]);
  const steps = converted.trajectory.steps;
  expect(steps[1].tool_calls[0].tool_call_id).toBe(steps[1].observation.results[0].source_call_id);
  expect(steps[1].observation.results[0].content).toBe("source text");
  expect(steps[1].extra.source_lines).toEqual([2]);
  expect(steps[2].observation.results[0].subagent_trajectory_ref[0].trajectory_id).toBe(converted.trajectory.subagent_trajectories[0].trajectory_id);
  expect(steps[3].observation.results[0].content[0].source.path).toBe("/private/media/page.png");
  expect(converted.trajectory.session_id).toBe("native-session");
  expect(converted.coverage.liveQualified).toBe(false);
  expect(converted.coverage.costUsd).toBeNull();
});

test("interrupted tools and unavailable child/media remain gaps rather than fabricated results", () => {
  const converted = convertNativeCapture([{ direction: "input", text: "question" },
    event("item/started", { threadId: "root", turnId: "turn", item: { id: "unfinished", type: "commandExecution", command: "read" } }),
    completed("root", { id: "spawn", type: "collabAgentToolCall", receiverThreadIds: ["missing-child"] }),
    completed("root", { id: "image", type: "imageView", path: "/unretained/image.png" })], result);
  expect(converted.coverage.complete).toBe(false);
  expect(converted.coverage.gaps).toContain("missing_child_trajectory:missing-child");
  expect(converted.coverage.gaps).toContain("incomplete_tool:root:unfinished");
  expect(converted.coverage.gaps).toContain("unresolved_media:root:image");
  expect(converted.coverage.gaps).toContain("missing_terminal:root");
  expect(converted.trajectory).not.toHaveProperty("subagent_trajectories");
});

test("foreign turns/threads, conflicting completions and nested media cannot certify complete conversion", () => {
  const rows = [{ direction: "input", text: "question" },
    completed("root", { id: "tool", type: "mcpToolCall", arguments: {}, result: { content: [{ type: "image", data: "encoded" }] } }),
    completed("root", { id: "user-media", type: "userMessage", content: [{ type: "resource_link", mimeType: "image/png", uri: "/unretained/page.png" }] }),
    completed("root", { id: "answer", type: "agentMessage", text: "first" }),
    completed("root", { id: "answer", type: "agentMessage", text: "changed" }),
    completed("foreign", { id: "message", type: "agentMessage", text: "unrelated" }),
    event("turn/completed", { threadId: "root", turn: { id: "old-turn", status: "completed", items: [] } }),
    event("turn/completed", { threadId: "root", turn: { id: "turn", status: "interrupted", items: [
      { id: "interrupted", type: "commandExecution", status: "inProgress", command: "long read" }] } })];
  const converted = convertNativeCapture(rows, result);
  expect(converted.coverage.gaps).toContain("unsupported_nested_media:root:tool");
  expect(converted.coverage.gaps).toContain("unsupported_user_media:root:user-media");
  expect(converted.coverage.gaps).toContain("foreign_thread:foreign");
  expect(converted.coverage.gaps).toContain("foreign_or_unbound_turn:root:old-turn");
  expect(converted.coverage.gaps).toContain("conflicting_item:root:answer");
  expect(converted.coverage.gaps).toContain("incomplete_tool:root:interrupted");
  expect(converted.coverage.complete).toBe(false);
  expect(converted.trajectory).not.toHaveProperty("subagent_trajectories");
});
