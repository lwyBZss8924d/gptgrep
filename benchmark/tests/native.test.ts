import { describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, readdir, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { runNativeForTest, validatePermissionProfile, type NativeRunOptions, type NativeTestTransport } from "../src/native";
import { parseReaderAnswer, readerOrder, readerPrompt, runReaderArm, runReaderPair, stagePreparedWorkspace, verifyReaderWorkspace, type ReaderWorkspace } from "../src/runner";
import { canonical, sha256 } from "../src/io";

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

async function preparedWorkspaceFixture() {
  const root = await mkdtemp(join(tmpdir(), "prepared-workspace-"));
  const bindingName = "ENVIRONMENTS_HARNESS_STORAGE_BINDING", digestName = "ENVIRONMENTS_HARNESS_STORAGE_DIGEST";
  const previous = { [bindingName]: process.env[bindingName], [digestName]: process.env[digestName] };
  const dispose = async () => {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key]; else process.env[key] = value;
    }
    await rm(root, { recursive: true, force: true });
  };
  try {
    const task = { id: "synthetic-task", question: "Synthetic document question", contextDocumentIds: ["d1", "d2"],
      contextId: "synthetic-context", groupId: "synthetic-group", taskType: "cross_doc" as const };
    const documents = task.contextDocumentIds.map(documentId => ({ documentId, relativePath: `documents/${documentId}.pdf`, sha256: sha256(documentId) }));
    const contextBytes = canonical({ documents: documents.map(doc => ({ document_id: doc.documentId, path: doc.relativePath, sha256: doc.sha256 })) }) + "\n";
    const workspace: ReaderWorkspace = { cwd: join(root, "treatment"), documents, preparedArtifacts: [], preparedContextDocumentIds: [...task.contextDocumentIds] };
    const bare: ReaderWorkspace = { cwd: join(root, "bare"), documents };
    for (const selected of [workspace, bare]) {
      await mkdir(join(selected.cwd, "documents"), { recursive: true });
      for (const doc of documents) await writeFile(join(selected.cwd, doc.relativePath), doc.documentId);
      await writeFile(join(selected.cwd, "context.json"), contextBytes);
    }
    const writePrepared = async (relativePath: string, bytes: string) => {
      await mkdir(dirname(join(workspace.cwd, relativePath)), { recursive: true });
      await writeFile(join(workspace.cwd, relativePath), bytes);
      workspace.preparedArtifacts = [...workspace.preparedArtifacts!.filter(pin => pin.relativePath !== relativePath), { relativePath, sha256: sha256(bytes) }];
    };
    for (const doc of documents) await writePrepared(`prepared/${doc.documentId}.pdf`, doc.documentId);
    const generation = "g1-1";
    const manifest = { schema_version: "gptgrep.v1", generation, root: join(workspace.cwd, "prepared"),
      documents: documents.map(doc => ({ path: `${doc.documentId}.pdf`, source_sha256: doc.sha256, source_bytes: doc.documentId.length })) };
    const publish = async (value: Record<string, unknown>) => {
      const bytes = canonical(value) + "\n", manifestSha256 = sha256(bytes);
      await writePrepared(`prepared/.gptgrep/generations/${generation}/manifest.json`, bytes);
      await writePrepared("prepared/.gptgrep/CURRENT.json", canonical({ schema_version: value.schema_version, generation, manifest_sha256: manifestSha256 }) + "\n");
      workspace.preparedIndex = { relativeRoot: "prepared", generation, manifestSha256 };
    };
    await publish(manifest);
    // Synthetic input to requireStorage's binding checks; owner admission belongs to the outer test run.
    const binding = join(root, "synthetic-binding.json"), bytes = canonical({ plan: { targets: { output: root } } }) + "\n";
    await writeFile(binding, bytes);
    process.env[bindingName] = binding;
    process.env[digestName] = sha256(bytes);
    return { workspace, bare, task, contextBytes, manifest, publish, writePrepared, dispose };
  } catch (error) { await dispose(); throw error; }
}

describe("native protocol lifecycle", () => {
  test("direct reader and paired APIs deny absent admission before staging or model callbacks", async () => {
    const fixture = await preparedWorkspaceFixture();
    const outputDir = join(dirname(fixture.bare.cwd), "denied-reader");
    const runtime = { packageRoot: "/must-not-read", manifestSha256: sha256("manifest"), binaryPath: "/must-not-start", binarySha256: sha256("binary") };
    let launches = 0;
    try {
      await expect(runReaderPair({ admission: undefined, task: fixture.task, pairIndex: 0, seed: "fixed", outputDir,
        native: options, bare: fixture.bare, gptgrep: fixture.workspace, runtime, helperModel: "openai/gpt-6-luna-decisions",
        launchArm: async () => { launches++; throw new Error("must not run"); } } as any)).rejects.toThrow("admission reference");
      expect(launches).toBe(0);
      await expect(runReaderArm({ admission: undefined, task: fixture.task, arm: "bare", order: 0, workspace: fixture.bare,
        outputDir, native: options, runtime, helperModel: "openai/gpt-6-luna-decisions" } as any)).rejects.toThrow("admission reference");
      await expect(stat(join(outputDir, "attempt-start.json"))).rejects.toThrow();
      await expect(stat(join(outputDir, "pair.json"))).rejects.toThrow();
      const denials = (await readdir(outputDir)).filter(name => name.startsWith("prelaunch-denial-"));
      expect(denials).toHaveLength(2);
      for (const file of denials) {
        const denial = JSON.parse(await readFile(join(outputDir, file), "utf8"));
        expect(denial.readerStarted).toBe(false); expect(denial.measurement).toBeNull();
      }
      expect(await readFile(join(fixture.bare.cwd, "context.json"), "utf8")).toBe(fixture.contextBytes);
    } finally { await fixture.dispose(); }
  });
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
  test("accepts the null filesystem metadata returned by app-server config/read", async () => {
    // The pinned app-server adds this null field to its serialized filesystem map.
    const server = new FakeServer();
    server.effectiveConfig = { permissions: { test_reader: { ...profile.permissions.test_reader,
      filesystem: { ...profile.permissions.test_reader.filesystem, glob_scan_max_depth: null } } } };
    const result = await runNativeForTest(options, server);
    expect(result.status).toBe("completed");
    expect(result.validStart).toBe(true);
    expect(server.requests.some(request => request.method === "turn/start")).toBe(true);
  });

  test("rejects non-null filesystem metadata without relaxing path grants", () => {
    const check = (filesystem: Message) => validatePermissionProfile({ permissions: { test_reader: {
      ...profile.permissions.test_reader, filesystem: { ...profile.permissions.test_reader.filesystem, ...filesystem },
    } } }, options);
    for (const value of [0, 1, -1, false, "read", "deny", {}, []]) {
      expect(() => check({ glob_scan_max_depth: value })).toThrow("Unexpected filesystem glob_scan_max_depth metadata");
    }
    for (const mutation of [{ unknown_metadata: null }, { "/whole-corpus": "read" }, { "/private/judge": "write" }]) {
      expect(() => check({ glob_scan_max_depth: null, ...mutation })).toThrow();
    }
  });

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

  test("prepared capability stages an exact isolated descriptor without changing shared reader inputs", async () => {
    const fixture = await preparedWorkspaceFixture();
    const { workspace, bare, task, contextBytes } = fixture;
    const descriptor = join(workspace.cwd, "gptgrep-workspace.json");
    const noCapability = { ...workspace, preparedIndex: undefined };
    const promptBefore = readerPrompt(task);
    try {
      await stagePreparedWorkspace(noCapability, task, "gptgrep");
      await expect(stat(descriptor)).rejects.toThrow();
      await verifyReaderWorkspace(noCapability, task, "gptgrep");
      await expect(verifyReaderWorkspace(workspace, task, "gptgrep")).rejects.toThrow();
      await expect(stagePreparedWorkspace({ ...bare, preparedIndex: workspace.preparedIndex }, task, "bare")).rejects.toThrow();
      await expect(stagePreparedWorkspace(noCapability, task, "gptgrep", ["gptgrep-workspace.json"])).rejects.toThrow();
      await stagePreparedWorkspace(workspace, task, "gptgrep");
      const expectedBytes = canonical({ schema: "gptgrep.workspace.v1", prepared_index: { root: "prepared",
        snapshot: { generation: workspace.preparedIndex!.generation, manifest_sha256: workspace.preparedIndex!.manifestSha256 } } }) + "\n";
      expect(await readFile(descriptor, "utf8")).toBe(expectedBytes);
      expect((await stat(descriptor)).mode & 0o777).toBe(0o444);
      const verified = await verifyReaderWorkspace(workspace, task, "gptgrep");
      expect(verified).toEqual(await verifyReaderWorkspace(bare, task, "bare"));
      expect(await readFile(join(workspace.cwd, "context.json"), "utf8")).toBe(contextBytes);
      expect(await readFile(join(bare.cwd, "context.json"), "utf8")).toBe(contextBytes);
      expect(readerPrompt(task)).toBe(promptBefore);
      expect(promptBefore).not.toContain("gptgrep-workspace.json");
      expect(expectedBytes).not.toContain(task.id);
      expect(expectedBytes).not.toContain(task.question);
      await expect(stagePreparedWorkspace(workspace, task, "gptgrep")).rejects.toThrow();
      expect(await readFile(descriptor, "utf8")).toBe(expectedBytes);
      await expect(verifyReaderWorkspace(noCapability, task, "gptgrep")).rejects.toThrow();
      await expect(verifyReaderWorkspace(noCapability, task, "gptgrep", ["gptgrep-workspace.json"])).rejects.toThrow();
      await writeFile(join(bare.cwd, "gptgrep-workspace.json"), expectedBytes);
      await expect(verifyReaderWorkspace(bare, task, "bare")).rejects.toThrow();
    } finally { await fixture.dispose(); }
  });

  test("prepared snapshot validation rejects repinned semantic drift and accepts the nested manifest schema", async () => {
    const fixture = await preparedWorkspaceFixture();
    const { workspace, task, manifest, publish, writePrepared } = fixture;
    const descriptor = join(workspace.cwd, "gptgrep-workspace.json");
    const refusesBeforeWriting = async () => {
      await expect(stagePreparedWorkspace(workspace, task, "gptgrep")).rejects.toThrow();
      await expect(stat(descriptor)).rejects.toThrow();
    };
    try {
      await writeFile(join(workspace.cwd, "prepared/d2.pdf"), "unpinned-change");
      await refusesBeforeWriting();
      await writeFile(join(workspace.cwd, "prepared/d2.pdf"), "d2");
      workspace.preparedContextDocumentIds = ["d1"];
      await refusesBeforeWriting();
      workspace.preparedContextDocumentIds = [...task.contextDocumentIds];
      await writePrepared("prepared/.gptgrep/CURRENT.json", canonical({ schema_version: "gptgrep.v1", generation: "g2-2",
        manifest_sha256: workspace.preparedIndex!.manifestSha256 }) + "\n");
      await refusesBeforeWriting();
      await publish({ ...manifest, root: fixture.bare.cwd });
      await refusesBeforeWriting();
      await publish(manifest);
      workspace.preparedIndex = { ...workspace.preparedIndex!, relativeRoot: "../outside" };
      await refusesBeforeWriting();
      await writePrepared("prepared/d1.pdf", "replacement-source");
      await publish({ ...manifest, documents: manifest.documents.map((doc, index) => index === 0
        ? { ...doc, source_sha256: sha256("replacement-source"), source_bytes: "replacement-source".length } : doc) });
      await refusesBeforeWriting();
      await writePrepared("prepared/d1.pdf", "d1");
      await publish({ ...manifest, schema_version: "gptgrep.manifest.v2", documents: manifest.documents.map(document => ({ document })) });
      await stagePreparedWorkspace(workspace, task, "gptgrep");
      expect((await verifyReaderWorkspace(workspace, task, "gptgrep")).context).toBe(sha256(fixture.contextBytes));
      expect((await stat(descriptor)).mode & 0o777).toBe(0o444);
    } finally { await fixture.dispose(); }
  });
});
