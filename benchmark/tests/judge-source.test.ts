import { expect, test } from "bun:test";
import { lstat, mkdir, mkdtemp, readFile, readdir, rename, rm, symlink, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { assertJudgeJobSelected, finalizeJudgeSourceCheck, observeJudgeSources, prepareJudgeDocuments, recheckJudgeSources, type GroundingResult, type JudgeInputPins } from "../src/judge";
import type { NativeRunResult } from "../src/native";
import { fileHash, sha256 } from "../src/io";

/** Synthetic filesystem contracts only; the outer test run supplies storage admission. */
async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "judge-source-contract-"));
  const keys = ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"] as const;
  const old = keys.map(key => process.env[key]);
  const binding = join(root, "storage.json"); await writeFile(binding, JSON.stringify({ targets: { output: root } }));
  process.env[keys[0]] = binding; process.env[keys[1]] = await fileHash(binding);
  const raw = join(root, "raw"); await mkdir(raw);
  const path = join(raw, "source.pdf"), bytes = "%PDF-synthetic-source";
  await writeFile(path, bytes);
  const documents = [{ documentId: "synthetic-source", path, sha256: sha256(bytes), pageCount: 1 }];
  return { root, raw, path, bytes, documents, cleanup: async () => {
    for (const [index, key] of keys.entries()) { if (old[index] === undefined) delete process.env[key]; else process.env[key] = old[index]; }
    await rm(root, { recursive: true, force: true });
  } };
}

test("direct judge staging retains original physical files and creates no PDF copies or links", async () => {
  const f = await fixture();
  try {
    const workspace = join(f.root, "judge"), before = await lstat(f.path, { bigint: true });
    const prepared = await prepareJudgeDocuments({ documents: f.documents, workspace, readableRoots: [workspace], mode: "pinned-read-only" });
    expect(prepared.contexts).toEqual([{ document_id: "synthetic-source", path: f.path, page_count: 1 }]);
    expect(prepared.readableRoots).toEqual([workspace, f.path]);
    expect(await readdir(workspace)).toEqual([]);
    expect(await recheckJudgeSources(prepared.sources!)).toEqual(prepared.sources);
    const after = await lstat(f.path, { bigint: true });
    expect(after.dev).toBe(before.dev); expect(after.ino).toBe(before.ino); expect(after.nlink).toBe(before.nlink);
    expect(await readFile(f.path, "utf8")).toBe(f.bytes);
    await expect(prepareJudgeDocuments({ documents: f.documents, workspace, readableRoots: [workspace], mode: "pinned-read-only" })).rejects.toThrow();
  } finally { await f.cleanup(); }
});

test("default judge staging preserves the historical independent PDF copy route", async () => {
  const f = await fixture();
  try {
    const workspace = join(f.root, "legacy"), prepared = await prepareJudgeDocuments({ documents: f.documents, workspace, readableRoots: [workspace] });
    const staged = join(workspace, prepared.contexts[0]!.path);
    expect(prepared.sources).toBeNull(); expect(prepared.readableRoots).toEqual([workspace]);
    expect(prepared.contexts[0]!.path.startsWith("documents/")).toBe(true);
    expect(await fileHash(staged)).toBe(f.documents[0]!.sha256);
    expect((await lstat(staged, { bigint: true })).ino).not.toBe((await lstat(f.path, { bigint: true })).ino);
  } finally { await f.cleanup(); }
});

test("direct judge staging rejects shared parent grants, writable sources and symlink paths", async () => {
  const f = await fixture();
  try {
    const workspace = join(f.root, "judge");
    await expect(prepareJudgeDocuments({ documents: f.documents, workspace, readableRoots: [workspace, f.raw], mode: "pinned-read-only" })).rejects.toThrow("separate exact grants");
    await expect(prepareJudgeDocuments({ documents: f.documents, workspace: f.root, readableRoots: [f.root], mode: "pinned-read-only" })).rejects.toThrow("separate exact grants");
    const link = join(f.root, "linked.pdf"); await symlink(f.path, link);
    await expect(observeJudgeSources([{ ...f.documents[0]!, path: link }])).rejects.toThrow("Symlink");
    const parentLink = join(f.root, "linked-directory"); await symlink(f.raw, parentLink);
    await expect(observeJudgeSources([{ ...f.documents[0]!, path: join(parentLink, "source.pdf") }])).rejects.toThrow("Symlink");
    await expect(observeJudgeSources([{ ...f.documents[0]!, path: join(f.root, "auth.json") }])).rejects.toThrow("exact physical file pin");
  } finally { await f.cleanup(); }
});

test("after-episode checks reject changed bytes and same-byte inode replacement", async () => {
  const f = await fixture();
  try {
    const before = await observeJudgeSources(f.documents);
    await writeFile(f.path, "changed-source-bytes");
    await expect(recheckJudgeSources(before)).rejects.toThrow("hash changed");
    await writeFile(f.path, f.bytes);
    await utimes(f.path, new Date(0), new Date(1000));
    await expect(recheckJudgeSources(before)).rejects.toThrow("identity changed");
    const replacementBefore = await observeJudgeSources(f.documents);
    await rename(f.path, join(f.raw, "retained-original.pdf"));
    await writeFile(f.path, f.bytes);
    await expect(recheckJudgeSources(replacementBefore)).rejects.toThrow("identity changed");
    expect(await fileHash(join(f.raw, "retained-original.pdf"))).toBe(f.documents[0]!.sha256);
  } finally { await f.cleanup(); }
});

test("source finalization clears a completed score and still checks a failed native episode", async () => {
  const f = await fixture();
  try {
    const inputPins = {} as JudgeInputPins;
    for (const name of ["context", "task", "candidate", "reference", "rubric"] as const) {
      const path = join(f.root, `${name}.json`); await writeFile(path, JSON.stringify({ input: name }));
      inputPins[name] = { path, sha256: await fileHash(path) };
    }
    const before = await observeJudgeSources(f.documents);
    const permission = { profileId: "judge", extends: ":read-only", filesystemSha256: sha256("synthetic profile"),
      exactReadOnlyFiles: [f.path, ...Object.values(inputPins).map(pin => pin.path)].sort() };
    const result = { schema: "gptgrep.grounding-judgment.v1", task_id: "synthetic", answer_sha256: sha256("answer"),
      corpus_sha256: sha256("corpus"), rubric_sha256: sha256("rubric"), reader_binding_sha256: sha256("reader"),
      status: "complete", grounded_success: 1, atomic_coverage: 1, reason: "Synthetic completed score", source_checks: [],
      judge_configuration: { model: "gpt-6.1-sol", effort: "high", service_tier: "default" },
      judge_run: { executionMode: "test", validStart: true, permissionEvidence: permission } as NativeRunResult } as GroundingResult;
    expect((await finalizeJudgeSourceCheck(result, before, inputPins, "judge")).sourceError).toBeNull();
    await writeFile(f.path, "changed");
    const failed = await finalizeJudgeSourceCheck(result, before, inputPins, "judge");
    expect(failed.sourceError).toContain("hash changed"); expect(result.status).toBe("unknown");
    expect(result.grounded_success).toBeNull(); expect(result.atomic_coverage).toBeNull();
    result.judge_run = { ...result.judge_run!, status: "failed" };
    expect((await finalizeJudgeSourceCheck(result, before, inputPins, "judge")).sourceError).toContain("hash changed");
    await writeFile(f.path, f.bytes);
    const current = await observeJudgeSources(f.documents); result.judge_run!.permissionEvidence = undefined;
    expect((await finalizeJudgeSourceCheck(result, current, inputPins, "judge")).sourceError).toContain("permission evidence");
    result.judge_run!.permissionEvidence = permission;
    for (const name of ["candidate", "reference"] as const) {
      await writeFile(inputPins[name].path, JSON.stringify({ input: "changed" }));
      result.status = "complete"; result.grounded_success = 1; result.atomic_coverage = 1;
      const changed = await finalizeJudgeSourceCheck(result, current, inputPins, "judge");
      expect(changed.inputStatus).toBe("changed_or_unavailable"); expect(changed.sourceError).toContain("Frozen judge input changed");
      expect(result.status).toBe("unknown"); expect(result.grounded_success).toBeNull(); expect(result.atomic_coverage).toBeNull();
      await writeFile(inputPins[name].path, JSON.stringify({ input: name }));
    }
  } finally { await f.cleanup(); }
});

test("direct source receipt binds the job selected by the actual judge command", () => {
  const pin = { path: "/private/judge-job.json", sha256: sha256("job") };
  const argv = ["/runtime/bun", "/source/controller.ts", "--mode", "judge", "--job", pin.path, "--expect-sha256", pin.sha256];
  expect(() => assertJudgeJobSelected(argv, pin)).not.toThrow();
  for (const changed of [argv.slice(0, -2), [...argv.slice(0, -1), sha256("different")], [...argv, "--mode", "judge"],
    [...argv.slice(0, 4), "--job", "/other.json", ...argv.slice(6)]]) expect(() => assertJudgeJobSelected(changed, pin)).toThrow();
});
