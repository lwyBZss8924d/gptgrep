import { describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runNativeForTest, validatePermissionProfile, type NativeRunOptions, type NativeTestTransport } from "../src/native";
import { parseReaderAnswer, readerOrder, readerPrompt, verifyReaderWorkspace } from "../src/runner";
import { sha256 } from "../src/io";

type Message = Record<string, any>;
const options: NativeRunOptions = {
  codexBinary: "/runtime/codex", cwd: "/task", model: "gpt-6.1-sol", effort: "high", serviceTier: "default",
  timeoutMs: 1000, tracePath: "/private/run/trace.jsonl", readableRoots: ["/task"], permissionProfile: "test_reader",
  input: "Read the supplied document.",
};
const profile = { permissions: { test_reader: { extends: ":read-only", network: { enabled: false }, filesystem: { "/": "deny", "/task": "read" } } } };
const tokenUsage = { total: { totalTokens: 99, inputTokens: 70, cachedInputTokens: 10, cacheWriteInputTokens: 0, outputTokens: 29, reasoningOutputTokens: 17 } };

class FakeServer implements NativeTestTransport {
  requests: Message[] = [];
  messages: (message: Message) => void = () => {};
  closed: (error?: Error) => void = () => {};
  models = [{ model: "gpt-6.1-sol", supportedReasoningEfforts: [{ reasoningEffort: "high" }], serviceTiers: [{ id: "default" }] }];
  effectiveConfig: any = profile;
  threadModel = "gpt-6.1-sol";
  activeProfile = "test_reader";
  turnBody: (server: FakeServer, request: Message) => void = (server, request) => {
    server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
    server.finish();
  };
  cleanup = { completed: true, groupTerminated: true };
  closeCalled = false;
  onMessage(callback: (message: Message) => void) { this.messages = callback; }
  onClose(callback: (error?: Error) => void) { this.closed = callback; }
  emit(message: Message) { this.messages(message); }
  reply(id: number, result: unknown) { this.emit({ id, result }); }
  finish(status = "completed", error: unknown = null) {
    this.emit({ method: "turn/completed", params: { threadId: "thread-1", turn: { id: "turn-1", status, error,
      items: [{ type: "agentMessage", id: "message-1", phase: "final_answer", text: '{"answer":"yes","citations":[],"explanation":""}' }] } } });
  }
  send(request: Message) {
    this.requests.push(request);
    queueMicrotask(() => {
      switch (request.method) {
        case "initialize": this.reply(request.id, { userAgent: "fake" }); break;
        case "model/list": this.reply(request.id, { data: this.models, nextCursor: null }); break;
        case "config/read": this.reply(request.id, { config: this.effectiveConfig }); break;
        case "permissionProfile/list": this.reply(request.id, { data: [{ id: "test_reader", allowed: true }], nextCursor: null }); break;
        case "thread/start": this.reply(request.id, { thread: { id: "thread-1", sessionId: "session-distinct" }, model: this.threadModel,
          cwd: "/task", activePermissionProfile: { id: this.activeProfile }, instructionSources: [] }); break;
        case "turn/start": this.turnBody(this, request); break;
      }
    });
  }
  async close() { this.closeCalled = true; return this.cleanup; }
}

describe("native protocol lifecycle", () => {
  test("empty completed text is an invalid answer while explicit abstention is valid", () => {
    const task = { id: "test", question: "test question", contextDocumentIds: ["doc"] } as any;
    expect(() => parseReaderAnswer(JSON.stringify({ answer: "   ", citations: [], explanation: "" }), task)).toThrow("nonempty");
    expect(parseReaderAnswer(JSON.stringify({ answer: "not answerable", citations: [], explanation: "source insufficient" }), task)).toBeTruthy();
  });
  test("uses the native base with experimental permission provenance and no injected tool/instruction surface", async () => {
    const server = new FakeServer();
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("completed");
    expect(result.executionMode).toBe("test");
    expect(result.validStart).toBe(true);
    expect(result.sessionId).toBe("session-distinct");
    expect(result.observedModel).toBe(options.model);
    expect(result.servedModel).toBeNull();
    expect(result.requestCount).toBeNull();
    expect(result.costUsd).toBeNull();
    expect(result.usage).toBeNull();
    const start = server.requests.find(request => request.method === "thread/start")!;
    expect(start.params).not.toHaveProperty("baseInstructions");
    expect(start.params).not.toHaveProperty("developerInstructions");
    expect(start.params).not.toHaveProperty("dynamicTools");
    expect(start.params).not.toHaveProperty("sandbox");
    expect(start.params.permissions).toBe("test_reader");
    expect(start.params.allowProviderModelFallback).toBe(false);
    expect(start.params.config.web_search).toBe("disabled");
    expect(server.requests[0].params.capabilities.experimentalApi).toBe(true);
    expect(server.closeCalled).toBe(true);
  });

  test("fails setup when the exact model is unavailable instead of silently substituting", async () => {
    const server = new FakeServer();
    server.models[0].model = "other-model";
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("setup_failed");
    expect(result.validStart).toBe(false);
    expect(server.requests.some(request => request.method === "thread/start")).toBe(false);
  });

  test("rejects thread model substitution and permission readback drift before a turn starts", async () => {
    for (const field of ["threadModel", "activeProfile"] as const) {
      const server = new FakeServer(); server[field] = "unexpected";
      const result = await runNativeForTest(options, server);
      expect(result.status).toBe("setup_failed");
      expect(result.validStart).toBe(false);
      expect(server.requests.some(request => request.method === "turn/start")).toBe(false);
    }
  });

  test("does not terminate on retryable errors, warnings or another thread's terminal event", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.emit({ method: "error", params: { threadId: "thread-1", turnId: "turn-1", willRetry: true, error: { message: "Temporary connection loss" } } });
      server.emit({ method: "warning", params: { threadId: "thread-1", turnId: "turn-1", message: "Retry underway" } });
      server.emit({ method: "turn/completed", params: { threadId: "other-thread", turn: { id: "turn-1", status: "failed" } } });
      server.emit({ method: "turn/completed", params: { threadId: "thread-1", turn: { id: "other-turn", status: "failed" } } });
      server.emit({ method: "thread/tokenUsage/updated", params: { threadId: "thread-1", turnId: "turn-1", tokenUsage } });
      server.finish();
    };
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("completed");
    expect(result.usage?.totalTokens).toBe(99);
    expect(result.authFailure).toBeUndefined();
  });

  test("buffers matching terminal/usage events that arrive before the turn/start response", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.emit({ method: "thread/tokenUsage/updated", params: { threadId: "thread-1", turnId: "turn-1", tokenUsage } });
      server.finish();
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
    };
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("completed");
    expect(result.usage?.inputTokens).toBe(70);
  });

  test("retains explicit authentication failure even if a retry later returns an answer", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.emit({ method: "error", params: { threadId: "thread-1", turnId: "turn-1", willRetry: true,
        error: { message: "Unauthorized", codexErrorInfo: { httpConnectionFailed: { httpStatusCode: 401 } } } } });
      server.finish();
    };
    const result = await runNativeForTest(options, server);
    expect(result.authFailure).toBe("AUTH_UNAUTHORIZED");
    expect(result.status).toBe("failed");
    expect(result.validStart).toBe(true);
  });

  test("retains command provenance without tool output or injected secret values", async () => {
    const old = process.env.OPENROUTER_API_KEY;
    process.env.OPENROUTER_API_KEY = "synthetic-secret-for-redaction";
    try {
      const server = new FakeServer();
      server.turnBody = (server, request) => {
        server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
        for (let i = 0; i < 2; i++) server.emit({ method: "item/completed", params: { threadId: "thread-1", turnId: "turn-1", item: {
          type: "commandExecution", id: "command-1", status: "completed", command: "gptgrep search synthetic-secret-for-redaction documents",
          exitCode: 0, durationMs: 21, aggregatedOutput: "PRIVATE_TOOL_OUTPUT" } } });
        server.finish();
      };
      const result = await runNativeForTest(options, server);
      expect(result.activities).toHaveLength(1);
      expect(result.activities[0]!.command).toBe("gptgrep search [redacted] documents");
      expect(result.activities[0]!.exitCode).toBe(0);
      expect(JSON.stringify(result)).not.toContain("synthetic-secret-for-redaction");
      expect(JSON.stringify(result)).not.toContain("PRIVATE_TOOL_OUTPUT");
    } finally { if (old === undefined) delete process.env.OPENROUTER_API_KEY; else process.env.OPENROUTER_API_KEY = old; }
  });

  test("keeps failed-turn usage including updates queued after the terminal event", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.finish("failed", { message: "Provider unavailable" });
      server.emit({ method: "thread/tokenUsage/updated", params: { threadId: "thread-1", turnId: "turn-1", tokenUsage } });
    };
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("failed");
    expect(result.usage?.outputTokens).toBe(29);
    expect(result.validStart).toBe(true);
  });

  test("timeout retains observed usage, interrupts the exact turn and closes the process group", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.emit({ method: "thread/tokenUsage/updated", params: { threadId: "thread-1", turnId: "turn-1", tokenUsage } });
    };
    const result = await runNativeForTest({ ...options, timeoutMs: 40 }, server);
    expect(result.status).toBe("timeout");
    expect(result.usage?.totalTokens).toBe(99);
    expect(server.requests.find(request => request.method === "turn/interrupt")?.params).toEqual({ threadId: "thread-1", turnId: "turn-1" });
    expect(result.cleanup.completed).toBe(true);
  });

  test("missing process cleanup cannot be promoted to a successful run", async () => {
    const server = new FakeServer(); server.cleanup.completed = false;
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("failed");
    expect(result.cleanup.completed).toBe(false);
  });

  test("records observed reroutes separately from the requested/readback model", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.emit({ method: "model/rerouted", params: { threadId: "thread-1", turnId: "turn-1", fromModel: options.model, toModel: "fallback-model", reason: "capacity" } });
      server.finish();
    };
    const result = await runNativeForTest(options, server);
    expect(result.requestedModel).toBe(options.model);
    expect(result.observedModel).toBe(options.model);
    expect(result.servedModel).toBe("fallback-model");
    expect(result.reroutes).toHaveLength(1);
    expect(result.modelBoundaryValid).toBe(false);
    expect(result.status).toBe("failed");
  });

  test("denies approval widening without replacing native tools", async () => {
    const server = new FakeServer();
    server.turnBody = (server, request) => {
      server.reply(request.id, { turn: { id: "turn-1", status: "inProgress" } });
      server.emit({ id: 800, method: "item/commandExecution/requestApproval", params: { threadId: "thread-1", turnId: "turn-1" } });
      server.emit({ id: 801, method: "item/permissions/requestApproval", params: { threadId: "thread-1", turnId: "turn-1" } });
      server.finish();
    };
    await runNativeForTest(options, server);
    expect(server.requests.find(request => request.id === 800)?.result).toEqual({ decision: "decline" });
    expect(server.requests.find(request => request.id === 801)?.result).toEqual({ permissions: {}, scope: "turn" });
  });
});

describe("native experiment boundary", () => {
  test("accepts only the declared managed Decisions domain and rejects wider proxy access", () => {
    const network = { enabled: true, proxy_url: "http://127.0.0.1:3128", mode: "full", domains: { "openrouter.ai": "allow" },
      enable_socks5: false, enable_socks5_udp: false, allow_upstream_proxy: false, allow_local_binding: false,
      dangerously_allow_non_loopback_proxy: false, dangerously_allow_all_unix_sockets: false, unix_sockets: {} };
    const config = { features: { network_proxy: true }, web_search: "disabled",
      permissions: { test_reader: { ...profile.permissions.test_reader, network } } };
    const selected = { ...options, allowedToolNetworkHosts: ["openrouter.ai"] };
    expect(() => validatePermissionProfile(config, selected)).not.toThrow();
    expect(() => validatePermissionProfile(config, options)).toThrow();
    for (const change of [{ domains: { "*": "allow" } }, { allow_upstream_proxy: true }, { mode: "read-only" }, { enable_socks5: true }, { unix_sockets: { "/run/service": "allow" } }]) {
      expect(() => validatePermissionProfile({ ...config, permissions: { test_reader: { ...config.permissions.test_reader, network: { ...network, ...change } } } }, selected)).toThrow();
    }
  });
  test("rejects broad or network-enabled profiles", () => {
    expect(() => validatePermissionProfile(profile, options)).not.toThrow();
    for (const mutation of [
      { filesystem: { "/": "read", "/task": "read" } },
      { filesystem: { "/": "deny", "/task": "read", "/whole-corpus": "read" } },
      { filesystem: { "/": "deny", "/task": "read", "/private/judge": "write" } },
      { network: { enabled: true } },
    ]) expect(() => validatePermissionProfile({ permissions: { test_reader: { ...profile.permissions.test_reader, ...mutation } } }, options)).toThrow();
  });

  test("forbids config replacing tools, hooks, instructions or permissions", async () => {
    for (const key of ["base_instructions", "features.hooks", "sandbox_mode", "permissions", "tools", "developer_instructions"]) {
      const server = new FakeServer();
      const result = await runNativeForTest({ ...options, config: { [key]: false } }, server);
      expect(result.status).toBe("setup_failed");
      expect(server.requests).toHaveLength(0);
    }
  });

  test("order is deterministic and alternates within each seed", () => {
    expect(readerOrder("frozen-seed", 0)).toEqual(readerOrder("frozen-seed", 0));
    expect(readerOrder("frozen-seed", 1)).toEqual([...readerOrder("frozen-seed", 0)].reverse());
    expect(readerOrder("frozen-seed", 2)).toEqual(readerOrder("frozen-seed", 0));
  });

  test("common prompt carries no treatment name or alternate context routing", () => {
    const task = { id: "q1", question: "What happened?", contextDocumentIds: ["d1"], contextId: "c1", groupId: "g1", taskType: "single_doc" as const };
    const prompt = readerPrompt(task);
    expect(prompt).toContain(task.question);
    expect(prompt).toContain("context.json");
    expect(prompt).not.toContain("gptgrep");
    expect(prompt).not.toContain("prepared/");
    expect(prompt).toContain("gpt-6-luna");
  });

  test("reader workspace retains complete context and rejects unadmitted files, maps and links", async () => {
    const cwd = await mkdtemp(join(tmpdir(), "native-reader-"));
    const task = { id: "q1", question: "What happened?", contextDocumentIds: ["d1", "d2"], contextId: "c1", groupId: "g1", taskType: "cross_doc" as const };
    const documents = task.contextDocumentIds.map(documentId => ({ documentId, relativePath: `documents/${documentId}.pdf`, sha256: sha256(documentId) }));
    const context = { documents: documents.map(doc => ({ document_id: doc.documentId, path: doc.relativePath, sha256: doc.sha256 })) };
    try {
      await mkdir(join(cwd, "documents"));
      for (const doc of documents) await writeFile(join(cwd, doc.relativePath), doc.documentId);
      await writeFile(join(cwd, "context.json"), JSON.stringify(context));
      expect((await verifyReaderWorkspace({ cwd, documents }, task, "bare")).corpus).toHaveLength(64);
      await expect(verifyReaderWorkspace({ cwd, documents: documents.slice(0, 1) }, task, "bare")).rejects.toThrow("full exact task context");
      await writeFile(join(cwd, "gold.json"), "{}");
      await expect(verifyReaderWorkspace({ cwd, documents }, task, "bare")).rejects.toThrow("Unadmitted");
      await rm(join(cwd, "gold.json"));
      await writeFile(join(cwd, "context.json"), JSON.stringify({ ...context, answer: "leaked" }));
      await expect(verifyReaderWorkspace({ cwd, documents }, task, "bare")).rejects.toThrow("public document mapping");
      await writeFile(join(cwd, "context.json"), JSON.stringify(context));
      await symlink(join(cwd, "documents", "d1.pdf"), join(cwd, "alias.pdf"));
      await expect(verifyReaderWorkspace({ cwd, documents }, task, "bare")).rejects.toThrow("physical directories");
    } finally { await rm(cwd, { recursive: true, force: true }); }
  });
});
