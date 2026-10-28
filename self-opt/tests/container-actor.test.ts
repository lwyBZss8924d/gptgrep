import { describe, expect, test } from "bun:test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdir, mkdtemp, readFile, writeFile, access, symlink, link, unlink } from "node:fs/promises";
import { join } from "node:path";
import { validateContainerActorCapability, validateContainerCapability, verifyContainerBuildPdfiumFiles, fileHash, type ContainerActorCapability, type StorageAdmission } from "../../benchmark/src/io";
import { executeOwnerCommand } from "../../benchmark/src/process";
import { actorGit, validateActorGitConfig, withActorGit } from "../src/actor-git";
import { validateActorAdmission, validateContainerActorJob, verifyActorSource, runContainerActor, compileContainerBuildCommand, containerBuildEnvironment, type ContainerActorJob } from "../src/container-actor";
const exec = promisify(execFile), hash = "a".repeat(64), pin = { path: "/runtime/pinned", sha256: hash };
const cap: ContainerActorCapability = { schema: "environments-harness.c4-actor-capability.v1", role: "engineer", candidate_id: "alpha", source_revision: "b".repeat(40),
  source_root: "/app/task", git_directory: "/app/task/.git", mutation_paths: ["crates/example/src/lib.rs"], source_manifest: { path: "/control/public-source.json", sha256: hash }, native_binary: pin, build_root: null };
function engineerJob(): ContainerActorJob { return { schema: "gptgrep.container-actor-job.v1", capability: structuredClone(cap), git: { binary: { path: "/usr/bin/git", sha256: hash } }, outputRoot: "/private/actor/output", runtimeRoot: "/app/task/.actor-runtime", deadlineAt: Date.now() + 120000, maxEvidenceBytes: 8 * 1024 * 1024,
  engineer: { binary: pin, nativeCapture: pin, instructions: pin, feedback: [], model: "selected-model", effort: "max", serviceTier: "priority", permissionProfile: "actor_engineer", timeoutMs: 30000, maxTraceBytes: 1024 * 1024, maxResultBytes: 65536, toolCalls: 16 }, build: null }; }
function buildJob(): ContainerActorJob {
  const j = engineerJob(); j.capability.role = "build"; j.capability.build_root = "/private/build"; j.engineer = null; j.runtimeRoot = null;
  const root = j.capability.build_root;
  j.build = { commands: [{ executable: { path: "/runtime/cargo", sha256: hash }, inputs: [], argv: ["test", "--offline", "--workspace"], cwd: cap.source_root, timeoutMs: 10000, maxOutputBytes: 4096 }],
    environment: { HOME: root + "/home", TMPDIR: root + "/tmp", TMP: root + "/tmp", TEMP: root + "/tmp", XDG_CACHE_HOME: root + "/cache", CARGO_HOME: root + "/cargo", CARGO_TARGET_DIR: root + "/target", CARGO_BUILD_JOBS: "1", CARGO_INCREMENTAL: "0", PYTHONDONTWRITEBYTECODE: "1" }, binaryPath: root + "/target/release/gptgrep", peakBytes: 1024 * 1024, targetArch: "x64" };
  return j;
}
function admission(job: ContainerActorJob): StorageAdmission { return { binding: "/control/command.json", sha256: hash, container: { commandId: `c4-${job.capability.role}` as any, admissionClass: "c4-actor", actorCapability: job.capability,
  containerBindingSha256: hash, parentHostBindingSha256: hash, ownerTemplateSha256: hash, reservationPeakBytes: String(16 * 1024 * 1024), acceptance: pin, immutableJob: pin, experimentArm: "treatment", modelProfile: { path: "/control/profile.toml", sha256: hash }, filesystemPolicyName: "actor_build", filesystemPolicySha256: hash, modelReadRoots: [cap.source_root, "/runtime"], ...(job.build?.pdfium ? { buildPdfium: job.build.pdfium } : {}) } }; }

describe("explicit C4 role boundary", () => {
  test("reader, host and qualifier cannot become an actor", () => {
    const j = buildJob();
    expect(() => validateActorAdmission(j, { binding: "/host", sha256: hash })).toThrow("cannot execute");
    for (const cls of ["reader", "bounded-qualification", "c4-actor-qualification"]) expect(() => validateActorAdmission(j, { ...admission(j), container: { ...admission(j).container!, admissionClass: cls as any } })).toThrow("cannot execute");
    for (const cls of ["reader", "bounded-qualification"]) expect(() => validateContainerCapability(cls, cls === "reader" ? "reader" : "qualification", null, cap)).toThrow("cannot inherit");
  });
  test("class, command, role and selected source remain exact", () => {
    const j = buildJob(); expect(() => validateActorAdmission(j, admission(j))).not.toThrow();
    expect(() => validateActorAdmission(j, { ...admission(j), container: { ...admission(j).container!, commandId: "c4-engineer" } })).toThrow();
    expect(() => validateActorAdmission(j, { ...admission(j), container: { ...admission(j).container!, actorCapability: { ...j.capability, candidate_id: "beta" } } })).toThrow();
    expect(() => validateContainerActorCapability({ ...cap, git_directory: "/shared/git" })).toThrow();
    expect(() => validateContainerActorCapability({ ...cap, mutation_paths: ["crates/../gold"] })).toThrow();
    expect(() => validateContainerActorCapability({ ...cap, source_manifest: { path: "/native/auth.json", sha256: hash } })).toThrow();
  });
  test("authentication belongs only to the exact native engineering role", () => {
    expect(() => validateContainerCapability("c4-actor", "c4-engineer", hash, cap)).not.toThrow();
    expect(() => validateContainerCapability("c4-actor", "c4-engineer", null, cap)).toThrow();
    const b = { ...cap, role: "build" as const, build_root: "/private/build" };
    expect(() => validateContainerCapability("c4-actor", "c4-build", null, b)).not.toThrow();
    expect(() => validateContainerCapability("c4-actor", "c4-build", hash, b)).toThrow();
    expect(() => validateContainerCapability("c4-actor-qualification", "c4-qualification", hash, cap)).toThrow();
  });
  test("native AGY stays unavailable for live execution and no-model qualification", () => {
    const agy = { ...cap, role: "agy" as const };
    for (const cls of ["c4-actor", "c4-actor-qualification"]) expect(() => validateContainerCapability(cls, "c4-agy", null, agy)).toThrow("unavailable");
    expect(() => validateContainerActorJob({ ...engineerJob(), capability: agy })).toThrow("unavailable");
  });
  test("actual build commands use the same qualified sandbox and no account environment", () => {
    const job = buildJob(), owner = admission(job).container!, inner = job.build!.commands[0]!;
    const actual = compileContainerBuildCommand(job, owner, inner);
    expect(actual.executable).toEqual(job.capability.native_binary);
    expect(actual.argv).toEqual(["sandbox", "--permission-profile", owner.filesystemPolicyName!, "-C", job.capability.source_root, "--", inner.executable.path, ...inner.argv]);
    const environment = containerBuildEnvironment(job, inner);
    expect(environment.CODEX_HOME).toBe(job.outputRoot + "/.build-sandbox-home"); expect(environment.HOME.startsWith(job.capability.build_root!)).toBe(true);
    for (const key of ["GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID", "OPENAI_API_KEY", "OPENROUTER_API_KEY", "ENVIRONMENTS_HARNESS_STORAGE_BINDING"]) expect(environment[key]).toBeUndefined();
    expect(environment.CARGO_NET_OFFLINE).toBe("true");
  });
  test("one role, finite evidence, offline builds and the existing reservation are retained", () => {
    expect(() => validateContainerActorJob(engineerJob())).not.toThrow();
    expect(() => validateContainerActorJob(buildJob())).not.toThrow();
    const j = buildJob(); j.build!.commands[0]!.argv = ["test"];
    expect(() => validateContainerActorJob(j)).toThrow("offline");
    const mixed = engineerJob(); mixed.build = buildJob().build;
    expect(() => validateContainerActorJob(mixed)).toThrow("cannot inherit");
    const oversized = buildJob(); oversized.build!.peakBytes = 32 * 1024 * 1024;
    expect(() => validateActorAdmission(oversized, admission(oversized))).toThrow("reservation");
    const bad = engineerJob(); bad.runtimeRoot = "/app/task/.git/runtime";
    expect(() => validateContainerActorJob(bad)).toThrow();
  });
});

describe("pinned build PDFium child environment", () => {
  function withPdfium(): ContainerActorJob {
    const job = buildJob(); job.build!.pdfium = { launcher: { path: "/usr/bin/env", sha256: hash }, library: { path: "/runtime/pdfium/lib/libpdfium.so", sha256: hash }, includeDirectory: "/runtime/pdfium/include", headers: [{ path: "/runtime/pdfium/include/fpdfview.h", sha256: hash }] }; return job;
  }
  test("owner inputs, readonly roots and exact directory names remain compulsory", () => {
    const job = withPdfium(); expect(() => validateActorAdmission(job, admission(job))).not.toThrow();
    expect(() => validateActorAdmission(job, { ...admission(job), container: { ...admission(job).container!, buildPdfium: undefined } })).toThrow("PDFium");
    for (const mutate of [
      (j: ContainerActorJob) => { j.build!.pdfium!.library.path = "/private/build/lib/libpdfium.so"; },
      (j: ContainerActorJob) => { j.build!.pdfium!.includeDirectory = j.outputRoot; },
      (j: ContainerActorJob) => { j.build!.pdfium!.library.path = "/runtime/pdfium:other/libpdfium.so"; },
      (j: ContainerActorJob) => { j.build!.pdfium!.headers.push({ ...j.build!.pdfium!.headers[0]! }); },
      (j: ContainerActorJob) => { j.build!.pdfium!.headers[0]!.path = "/runtime/pdfium/include/extra.so"; },
      (j: ContainerActorJob) => { j.build!.environment.LD_LIBRARY_PATH = "/runtime/pdfium/lib"; },
    ]) { const changed = structuredClone(job); mutate(changed); expect(() => validateContainerActorJob(changed)).toThrow(); }
    const outer = containerBuildEnvironment(job, job.build!.commands[0]!);
    for (const name of ["PDFIUM_LIB_PATH", "PDFIUM_INCLUDE_PATH", "LD_LIBRARY_PATH"]) expect(outer[name]).toBeUndefined();
  });
  async function fixture() {
    const out = process.env.ENVIRONMENTS_HARNESS_OUTPUT; if (!out) throw new Error("A fresh checked external test output is required");
    const root = await mkdtemp(join(out, "build-pdfium-")), source = join(root, "source"), lib = join(root, "runtime/lib"), include = join(root, "runtime/include");
    for (const directory of [source, lib, include]) await mkdir(directory, { recursive: true });
    const launcher = "/usr/bin/env", library = join(lib, "libpdfium.so"), header = join(include, "fpdfview.h"), native = join(root, "sandbox-native.sh"), child = join(root, "observe.ts"), result = join(root, "child-env.json"), profile = join(root, "profile.toml");
    await writeFile(library, "synthetic PDFium runtime fixture\n"); await writeFile(header, "synthetic header fixture\n"); await writeFile(profile, "# Fixture profile\n");
    await writeFile(native, '#!/bin/sh\nif [ "${PDFIUM_LIB_PATH+x}${PDFIUM_INCLUDE_PATH+x}${LD_LIBRARY_PATH+x}" != "" ]; then exit 77; fi\nwhile [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done\n[ "$1" = "--" ] || exit 78\nshift\nexec "$@"\n', { mode: 0o700 });
    await writeFile(child, 'await Bun.write(process.argv[2]!, JSON.stringify({ lib: process.env.PDFIUM_LIB_PATH, include: process.env.PDFIUM_INCLUDE_PATH, loader: process.env.LD_LIBRARY_PATH }));\n');
    const pinned = async (path: string) => ({ path, sha256: await fileHash(path) });
    const job = withPdfium(); job.capability = { ...job.capability, source_root: source, git_directory: join(source, ".git"), build_root: join(root, "build"), native_binary: await pinned(native) }; job.outputRoot = join(root, "output");
    job.build!.environment = Object.fromEntries(Object.entries(job.build!.environment).map(([name, value]) => [name, value.startsWith("/private/build") ? job.capability.build_root! + value.slice("/private/build".length) : value]));
    for (const directory of new Set(Object.values(job.build!.environment).filter(value => value.startsWith("/")))) await mkdir(directory, { recursive: true });
    job.build!.pdfium = { launcher: await pinned(launcher), library: await pinned(library), includeDirectory: include, headers: [await pinned(header)] };
    const command = { executable: await pinned(process.execPath), inputs: [await pinned(child)], argv: ["--no-env-file", "--no-install", child, result], cwd: source, timeoutMs: 1000, maxOutputBytes: 4096 };
    job.build!.commands = [command]; const owner = admission(job).container!; owner.modelProfile = await pinned(profile);
    return { job, owner, command, lib, include, library, result };
  }
  test("the outer launcher sees no loader variables, and changed runtime pins prevent a second child", async () => {
    const { job, owner, command, lib, include, library, result } = await fixture();
    await verifyContainerBuildPdfiumFiles(job.build!.pdfium!);
    const compiled = compileContainerBuildCommand(job, owner, command);
    const observed = await executeOwnerCommand(compiled, 10000, { environment: containerBuildEnvironment(job, command), inheritEnvironment: false });
    expect(observed.exitCode).toBe(0); expect(observed.cleanup).toBe(true); expect(observed.executionError).toBeNull();
    expect(JSON.parse(await readFile(result, "utf8"))).toEqual({ lib, include, loader: lib });
    await unlink(result); await writeFile(library, "changed fixture bytes\n");
    await expect(executeOwnerCommand(compiled, 10000, { environment: containerBuildEnvironment(job, command), inheritEnvironment: false })).rejects.toThrow();
    await expect(access(result)).rejects.toHaveProperty("code", "ENOENT");
  });
  test("unlisted library/header files and symlink headers fail the runtime inventory", async () => {
    const { job, lib, include } = await fixture(), pdfium = job.build!.pdfium!;
    const extra = join(lib, "unselected.so"); await writeFile(extra, "synthetic extra runtime");
    await expect(verifyContainerBuildPdfiumFiles(pdfium)).rejects.toThrow("inventory"); await unlink(extra);
    const header = pdfium.headers[0]!.path, copy = join(include, "other.h"); await writeFile(copy, "unlisted header");
    await expect(verifyContainerBuildPdfiumFiles(pdfium)).rejects.toThrow("inventory"); await unlink(copy);
    await unlink(header); await symlink(job.build!.pdfium!.library.path, header);
    await expect(verifyContainerBuildPdfiumFiles(pdfium)).rejects.toThrow("Symlink");
  });
});

describe("public source-only independent Git store", () => {
  async function fixture() {
    const out = process.env.ENVIRONMENTS_HARNESS_OUTPUT;
    if (!out) throw new Error("A fresh checked external test output is required");
    const root = await mkdtemp(join(out, "actor-source-")), source = join(root, "source"); await mkdir(join(source, "crates"), { recursive: true });
    const env = { ...process.env, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_SYSTEM: "/dev/null", GIT_CONFIG_GLOBAL: "/dev/null", GIT_AUTHOR_NAME: "Fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "Fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid" };
    const g = async (...args: string[]) => (await exec("git", ["-c", "core.hooksPath=/dev/null", "-C", source, ...args], { env })).stdout;
    await g("init", "--initial-branch=candidate"); await writeFile(join(source, "crates/lib.rs"), "pub fn answer() {}\n"); await writeFile(join(source, ".gitignore"), "# Public fixture\n"); await writeFile(join(source, ".gitattributes"), "* text=auto\n"); await g("add", "crates/lib.rs", ".gitignore", ".gitattributes"); await g("commit", "-m", "Public fixture");
    const revision = (await g("rev-parse", "HEAD")).trim(), manifest = join(root, "source-manifest.json");
    await writeFile(manifest, JSON.stringify({ schema: "gptgrep.actor-public-source.v1", revision, files: await Promise.all([".gitattributes", ".gitignore", "crates/lib.rs"].map(async path => ({ path, sha256: await fileHash(join(source, path)) }))) }));
    const j = engineerJob(); j.capability = { ...cap, source_revision: revision, source_root: source, git_directory: join(source, ".git"), source_manifest: { path: manifest, sha256: await fileHash(manifest) }, mutation_paths: ["crates/lib.rs"] }; j.runtimeRoot = join(source, ".actor-runtime"); j.outputRoot = join(root, "output"); j.git = { binary: { path: "/usr/bin/git", sha256: await fileHash("/usr/bin/git") } };
    return { j, g, source, root };
  }
  test("a pinned clean independent public source store is accepted", async () => {
    const { j } = await fixture(); await expect(verifyActorSource(j, j.capability.source_revision, true)).resolves.toBeUndefined();
  });
  test("ignored evaluator material is rejected even when ordinary Git status is clean", async () => {
    const { j, source, g } = await fixture(); await writeFile(join(source, ".git/info/exclude"), "secret-fixture\n"); await writeFile(join(source, "secret-fixture"), "synthetic forbidden fixture\n");
    expect((await g("status", "--porcelain")).trim()).toBe("");
    await expect(verifyActorSource(j, j.capability.source_revision, true)).rejects.toThrow("ignored or untracked");
  });
  test("unrelated refs, remote routing and unreachable Git objects are rejected", async () => {
    const a = await fixture(); await a.g("notes", "add", "-m", "Synthetic private fixture");
    await expect(verifyActorSource(a.j, a.j.capability.source_revision, true)).rejects.toThrow("extra refs");
    const b = await fixture(); await b.g("remote", "add", "origin", "https://example.invalid/repo");
    await expect(verifyActorSource(b.j, b.j.capability.source_revision, true)).rejects.toThrow("Unsafe actor Git configuration");
    const c = await fixture(); const garbage = join(c.root, "unreachable.txt"); await writeFile(garbage, "synthetic unreachable object\n"); await c.g("hash-object", "-w", garbage);
    await expect(verifyActorSource(c.j, c.j.capability.source_revision, true)).rejects.toThrow("selected public commit");
  });
  test("Git fsmonitor, textconv, clean filters and includes cannot execute in the controller", async () => {
    for (const field of ["core.fsmonitor", "diff.evil.textconv", "filter.evil.clean", "include.path"]) {
      const { j, g, source, root } = await fixture(), marker = join(root, "escaped-marker"), script = join(source, ".git", "payload.sh");
      await writeFile(script, `#!/bin/sh\nprintf escaped > '${marker}'\n`, { mode: 0o700 });
      await g("config", field, script);
      expect((await actorGit(j, "rev-parse", "HEAD")).trim()).toBe(j.capability.source_revision);
      await actorGit(j, "status", "--porcelain"); await actorGit(j, "diff", "--binary");
      await expect(verifyActorSource(j, j.capability.source_revision, true)).rejects.toThrow("Unsafe actor Git configuration");
      await expect(access(marker)).rejects.toHaveProperty("code", "ENOENT");
    }
  });
  test("controller Git uses the pinned executable and a curated environment", async () => {
    const { j, source, root } = await fixture(), marker = join(root, "path-marker"), bin = join(source, "bin"); await mkdir(bin);
    await writeFile(join(bin, "git"), `#!/bin/sh\nprintf escaped > '${marker}'\n`, { mode: 0o700 });
    const previous = { PATH: process.env.PATH, GIT_CONFIG_COUNT: process.env.GIT_CONFIG_COUNT, GIT_CONFIG_KEY_0: process.env.GIT_CONFIG_KEY_0, GIT_CONFIG_VALUE_0: process.env.GIT_CONFIG_VALUE_0 };
    process.env.PATH = bin + ":" + process.env.PATH; process.env.GIT_CONFIG_COUNT = "1"; process.env.GIT_CONFIG_KEY_0 = "core.fsmonitor"; process.env.GIT_CONFIG_VALUE_0 = join(bin, "git");
    try {
      expect((await actorGit(j, "rev-parse", "HEAD")).trim()).toBe(j.capability.source_revision);
      expect((await actorGit(j, "config", "--get", "core.fsmonitor")).trim()).toBe("false");
      await expect(access(marker)).rejects.toHaveProperty("code", "ENOENT");
    } finally { for (const [key, value] of Object.entries(previous)) if (value === undefined) delete process.env[key]; else process.env[key] = value; }
  });
  test("a late repository-config replacement cannot affect the private Git view", async () => {
    const { j, source, root } = await fixture(), marker = join(root, "raced-hook-marker"), payload = join(root, "payload.sh"), evil = join(root, "evil-config"), wrapper = join(root, "pinned-git-wrapper");
    await writeFile(payload, `#!/bin/sh\nprintf escaped > '${marker}'\n`, { mode: 0o700 });
    await writeFile(evil, (await readFile(join(source, ".git/config"), "utf8")) + `\n[filter "evil"]\nclean = ${payload}\n`);
    await writeFile(join(source, ".gitattributes"), "*.rs filter=evil\n");
    await writeFile(wrapper, `#!${process.execPath}\nimport { copyFileSync } from "node:fs"; import { spawnSync } from "node:child_process"; copyFileSync(${JSON.stringify(evil)}, ${JSON.stringify(join(source, ".git/config"))}); const p=spawnSync("/usr/bin/git", process.argv.slice(2), { env: process.env, stdio: ["ignore", "pipe", "pipe"] }); process.stdout.write(p.stdout); process.stderr.write(p.stderr); process.exit(p.status ?? 125);\n`, { mode: 0o700 });
    j.git.binary = { path: wrapper, sha256: await fileHash(wrapper) };
    await actorGit(j, "status", "--porcelain");
    await expect(access(marker)).rejects.toHaveProperty("code", "ENOENT");
  });
  test("alternate object stores and Git metadata links are rejected without loading them", async () => {
    const a = await fixture(); await writeFile(join(a.source, ".git/objects/info/alternates"), a.root + "/unrelated\n");
    await expect(actorGit(a.j, "status")).rejects.toThrow("alternates");
    const b = await fixture(); await symlink(join(b.root, "unrelated"), join(b.source, ".git", "linked"));
    await expect(actorGit(b.j, "status")).rejects.toThrow("links");
    const c = await fixture(); await link(join(c.source, ".git/HEAD"), join(c.source, ".git/HEAD.copy"));
    await expect(actorGit(c.j, "status")).rejects.toThrow("links");
  });
  test("the actual candidate tip is preserved in a fresh one-commit build snapshot", async () => {
    const { j, g, source, root } = await fixture(); await writeFile(join(source, "crates/lib.rs"), "pub fn answer() { let _ = 1; }\n"); await g("add", "crates/lib.rs"); await g("commit", "-m", "One native candidate fixture");
    const candidate = (await g("rev-parse", "HEAD")).trim(); await mkdir(j.runtimeRoot!); await writeFile(join(j.runtimeRoot!, "native-cache"), "synthetic private native state\n");
    await expect(verifyActorSource(j, candidate, false)).resolves.toBeUndefined();
    const buildSource = join(root, "build-source"); await mkdir(buildSource);
    const clean = { PATH: "/usr/bin:/bin", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_SYSTEM: "/dev/null", GIT_CONFIG_COUNT: "0" };
    const targetGit = (...args: string[]) => exec(j.git.binary.path, ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-C", buildSource, ...args], { env: clean, timeout: 10000 });
    await targetGit("init", "--initial-branch=candidate");
    await withActorGit(j, async (_run, publisher) => {
      await targetGit("fetch", "--depth=1", "--no-tags", "file://" + publisher, candidate);
      await targetGit("checkout", "-B", "candidate", "FETCH_HEAD");
    });
    const manifest = join(root, "candidate-source.json"), files = await Promise.all([".gitattributes", ".gitignore", "crates/lib.rs"].map(async path => ({ path, sha256: await fileHash(join(buildSource, path)) })));
    await writeFile(manifest, JSON.stringify({ schema: "gptgrep.actor-public-source.v1", revision: candidate, files }));
    const built = buildJob(); built.capability = { ...j.capability, role: "build", source_revision: candidate, source_root: buildSource, git_directory: join(buildSource, ".git"), source_manifest: { path: manifest, sha256: await fileHash(manifest) }, build_root: join(root, "build-output") }; built.outputRoot = join(root, "build-controller"); built.git = { binary: j.git.binary };
    expect((await targetGit("rev-parse", "HEAD")).stdout.trim()).toBe(candidate); expect((await targetGit("rev-list", "--count", "HEAD")).stdout.trim()).toBe("1");
    await expect(verifyActorSource(built, candidate, true)).resolves.toBeUndefined();
    await expect(access(join(buildSource, ".actor-runtime"))).rejects.toHaveProperty("code", "ENOENT");
    const reused = { ...built, capability: { ...built.capability, source_root: source, git_directory: j.capability.git_directory }, git: j.git };
    await expect(verifyActorSource(reused, candidate, true)).rejects.toThrow("selected public commit");
  });
  test("model and build work cannot run under the checked host-test binding", async () => {
    const { j, root } = await fixture(), config = join(root, "job.json"); await writeFile(config, JSON.stringify(j));
    await expect(runContainerActor({ path: config, sha256: await fileHash(config) })).rejects.toThrow("cannot execute a C4 actor");
  });
});


describe("no-model qualification payloads", () => {
  test("the qualifier compiles and rejects an unbound invocation before any sandbox call", async () => {
    const out = process.env.ENVIRONMENTS_HARNESS_OUTPUT!;
    try {
      await exec(process.execPath, ["--no-env-file", "--no-install", "--config=" + join(out, "empty-bunfig.toml"), new URL("../src/container-actor-qualifier.ts", import.meta.url).pathname], { timeout: 10000 });
      throw new Error("An unbound qualifier unexpectedly ran");
    } catch (error: any) {
      expect(error.code).toBe(1); const record = JSON.parse(error.stderr.trim());
      expect(record.status).toBe("setup_failed"); expect(record.receiptWritten).toBe(false); expect(record.errorCode).toBe(process.platform === "linux" ? "exact_config_cli_required" : "linux_bun_guest_required");
    }
  });
  test("the pinned probe executes actual writes and detects a requested denial that remains writable", async () => {
    const out = process.env.ENVIRONMENTS_HARNESS_OUTPUT!, root = await mkdtemp(join(out, "actor-probe-"));
    const file = join(root, "read.txt"); await writeFile(file, "synthetic\n");
    const source = await readFile(new URL("../src/container-actor-probe.js", import.meta.url), "utf8");
    const run = (allow: boolean, suffix: string) => exec(process.execPath, ["--no-env-file", "--no-install", "--config=" + join(out, "empty-bunfig.toml"), "--eval", source + `\nactorIsolationProbe(${JSON.stringify({ allow: [file, file, file], deny: Array.from({ length: 7 }, (_, i) => join(root, "missing", String(i))), write: [{ path: join(root, suffix), allow }, { path: join(root, "missing", "w1"), allow: false }, { path: join(root, "missing", "w2"), allow: false }, { path: join(root, "missing", "w3"), allow: false }] })});`], { timeout: 10000 });
    const result = await run(true, "allowed.txt"); const events = result.stdout.trim().split("\n").map(line => JSON.parse(line));
    expect(events.at(-1)).toEqual({ schema: "gptgrep.c4-actor-isolation.probe.v1", positiveReads: 3, deniedReads: 7, writeProbes: 4 });
    expect(await readFile(join(root, "allowed.txt"), "utf8")).toBe("synthetic actor sandbox write\n");
    try { await run(false, "forbidden.txt"); throw new Error("Writable negative fixture was accepted"); }
    catch (error: any) { expect(error.code).toBe(43); expect(JSON.parse(error.stdout.trim().split("\n").at(-1)).writable).toBe(true); }
  });
});
