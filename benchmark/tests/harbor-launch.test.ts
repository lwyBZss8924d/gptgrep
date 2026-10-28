import { describe, expect, test } from "bun:test";
import { cp, mkdtemp, mkdir, readFile, readdir, realpath, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { assessAvailability, expectedIdentity, loadAdmission, readExactJson, type ControllerAcceptance, type Pin } from "../src/availability";
import { normalizeDependencyRelease } from "../src/dataset";
import { harborPreflight, indexTaskFiles, runHarborForTest, validHarborAgent, validImmutableImage } from "../src/harbor-launch";
import { writeHarborTasks } from "../src/adapter";
import { digest, fileHash, processCommandIdentity, sha256 } from "../src/io";

async function fixture() {
  const root = await mkdtemp(join(await realpath(tmpdir()), "availability-harbor-"));
  const keys = ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"] as const;
  const previous = Object.fromEntries(keys.map(key => [key, process.env[key]]));
  const write = async (path: string, value: unknown) => { await mkdir(dirname(path), { recursive: true }); await writeFile(path, JSON.stringify(value) + "\n"); };
  const pin = async (path: string): Promise<Pin> => ({ path, sha256: await fileHash(path) });
  const restore = async () => {
    for (const key of keys) { if (previous[key] === undefined) delete process.env[key]; else process.env[key] = previous[key]; }
    await rm(root, { recursive: true, force: true });
  };
  try {
    const release = normalizeDependencyRelease({ manifest: { release_version: "synthetic", document_count: 2, qa_count: 1,
      single_doc_qa_count: 0, cross_doc_qa_count: 1, release_files: { documents: "documents", single_doc_questions: "single", cross_doc_questions: "cross" } },
      documents: ["d1", "d2"].map(id => ({ document_id: id, title: id, url: `https://example.invalid/${id}`, source_host: "example.invalid", page_count: 1, file_size_bytes: 2,
        domain: "synthetic", doc_type: "report", pdf_type: "text", public_metadata: {} })), singleDocQuestions: [],
      crossDocQuestions: [{ question_id: "series", task_type: "cross_doc", documents: [{ document_id: "d1" }, { document_id: "d2" }], metadata: { n_context_documents: 2 } }], fileHashes: { source: sha256("source") } });
    const evidence = join(root, "verification.json"); await write(evidence, { status: "synthetic_only" });
    const assessment = assessAvailability({ revision: "assessment-1", generation: 1, recordedAt: "2026-10-16T00:00:00Z", supersedes: null, release,
      documents: await Promise.all(release.documents.map(async doc => ({ documentId: doc.documentId, expectedIdentitySha256: digest(expectedIdentity(doc)), rawState: "verified" as const,
        rawSha256: sha256(doc.documentId), reason: "Synthetic exact identity", evidence: [await pin(evidence)], freeze: null,
        verification: { status: "ready", sha256: sha256(doc.documentId), pageCountVerified: true, pageCount: 1,
          sourceVerification: { status: "verified", sha256: sha256(doc.documentId), expectedPageCount: 1, observedPageCount: 1, method: "synthetic", identityEvidence: "synthetic",
            identity: { status: "verified", basis: "publisher_release_metadata", expectedIdentitySha256: digest(expectedIdentity(doc)), sourceUrl: doc.url, evidence: [await pin(evidence)], review: null } } } }))),
      profiles: [{ id: "bare-raw", arm: "bare", implementation: "implemented", requiresOcr: false, preprocessingProfileSha256: null }], preprocessing: [] });
    const assessmentPath = join(root, "assessment.json"); await write(assessmentPath, assessment);
    const assessmentPin = await pin(assessmentPath), currentPath = join(root, "current.json");
    await write(currentPath, { schema: "gptgrep.availability-current.v1", status: "current", generation: 1, assessmentRevision: assessment.revision, assessmentSha256: assessmentPin.sha256 });
    const task = release.tasks[0]!, taskDir = join(root, "export", "series"), outputDir = join(root, "output");
    const context = { schema: "gptgrep.xl-reader-context.v1", task_id: task.id, context_id: task.contextId,
      corpus_sha256: sha256(JSON.stringify(release.documents.map(doc => ({ document_id: doc.documentId, sha256: sha256(doc.documentId) })))),
      documents: release.documents.map(doc => ({ document_id: doc.documentId, path: `/app/documents/${doc.documentId}.pdf`, sha256: sha256(doc.documentId), page_count: 1 })) };
    await write(join(taskDir, "environment/context.json"), context);
    for (const doc of release.documents) { const path = join(taskDir, "environment/documents", `${doc.documentId}.pdf`); await mkdir(dirname(path), { recursive: true }); await writeFile(path, doc.documentId); }
    const manifest = { schema: "gptgrep.xl-harbor-task.v1", task_id: task.id, source_revision: release.revision, source_file_hashes: release.fileHashes,
      group_id: task.groupId, context_id: task.contextId, corpus_sha256: context.corpus_sha256, arm: "bare",
      availability: { assessmentSha256: assessmentPin.sha256, assessmentRevision: assessment.revision, generation: 1, dependencySha256: assessment.dependencySha256,
        profileId: "bare-raw", profileSha256: digest(assessment.profiles[0]), taskDependencySha256: digest(task) }, prepared_artifacts: [], prepared_context_document_ids: [] };
    await write(join(taskDir, "adapter-manifest.json"), manifest);
    for (const relative of ["instruction.md", "task.toml", "environment/Dockerfile", "tests/verify.py", "tests/checks.py", "tests/pyproject.toml", "tests/test.sh", "tests/Dockerfile", "tests/upstream_evaluate.py", "tests/gold.jsonl", "tests/expected.json"]) {
      await mkdir(dirname(join(taskDir, relative)), { recursive: true }); await writeFile(join(taskDir, relative), "synthetic only\n");
    }
    const generatedPath = join(root, "export/harbor-export.json"), exportAcceptancePath = join(root, "export-acceptance.json"), reviewPath = join(root, "review.json");
    const generated: any = { schema: "gptgrep.harbor-export.v1", status: "generated_pending_acceptance", assessmentSha256: assessmentPin.sha256,
      generation: 1, releaseRevision: release.revision, profileId: "bare-raw", arm: "bare", tasks: [{ taskId: task.id, directory: taskDir, files: await indexTaskFiles(taskDir) }] };
    await write(generatedPath, generated);
    // Well-typed mock evidence only. These temporary fixtures never qualify a live runtime.
    const exportReview: any = { schema: "gptgrep.harbor-export-review.v1", status: "accepted", generated: await pin(generatedPath) };
    await write(reviewPath, exportReview);
    const exported: any = { schema: "gptgrep.harbor-export-acceptance.v1", status: "accepted", generated: await pin(generatedPath), review: await pin(reviewPath) };
    await write(exportAcceptancePath, exported);
    const runtimePath = join(root, "runtime.json"), qualificationPath = join(root, "qualification.json"), executablePath = join(root, "never-executed"), authConfig = join(root, "nonsecret-auth-config.json");
    await writeFile(executablePath, "This fixture is never executable or used for authentication.\n"); await write(authConfig, { synthetic: true });
    const runtime: any = { schema: "gptgrep.harbor-runtime-acceptance.v1", status: "qualified", cliContract: "harbor-run-path-agent-job-name-v1",
      transport: "codex-auth-supervisor-direct-harbor", environment: "linux", python: await pin(executablePath), supervisor: await pin(executablePath), harbor: await pin(executablePath),
      authConfig: await pin(authConfig), qualification: await pin(reviewPath), agent: "synthetic-agent", runId: "synthetic-run", deadlineSeconds: 30 };
    const { qualification: placeholder, ...runtimeContract } = runtime;
    const qualification: any = { schema: "gptgrep.harbor-runtime-qualification.v1", status: "accepted", runtimeContractSha256: digest(runtimeContract) };
    await write(qualificationPath, qualification); runtime.qualification = await pin(qualificationPath);
    await write(runtimePath, runtime);
    const controllerPath = join(root, "controller.ts"); await writeFile(controllerPath, "// synthetic accepted controller closure\n");
    const sourcePaths = new Set([process.argv[0]!, controllerPath, resolve(import.meta.dir, "../harbor/native_agent.py"), resolve(import.meta.dir, "../../self-opt/src/evaluate.ts"), ...["availability.ts", "adapter.ts", "harbor-launch.ts", "dataset.ts", "runner.ts", "native.ts", "io.ts", "source-verification.ts", "episode.ts", "trajectory.ts", "judge.ts", "controller.ts", "campaign.ts", "container.ts", "process.ts", "harbor-wrapper.ts"].map(name => resolve(import.meta.dir, "../src", name))]);
    const actualArgv = (await processCommandIdentity()).argv;
    for (const arg of actualArgv.slice(1)) {
      if (/\.(?:py|[cm]?[jt]s|tsx|jsx)$/.test(arg)) sourcePaths.add(resolve(arg));
      if (arg.startsWith("--config=")) sourcePaths.add(resolve(arg.slice("--config=".length)));
    }
    const acceptance: ControllerAcceptance = { schema: "gptgrep.availability-controller.v1", status: "accepted", current: await pin(currentPath), assessment: assessmentPin,
      generation: 1, assessmentRevision: assessment.revision, taskIds: [task.id], profileIds: ["bare-raw"], sourcePins: await Promise.all([...sourcePaths].map(pin)),
      controllerArgv: [process.argv[0]!, controllerPath, "--acceptance", "@ACCEPTANCE_PATH@", "--acceptance-sha256", "@ACCEPTANCE_SHA256@"], childArgv: [actualArgv],
      storage: { runId: "synthetic-storage", workloadSha256: sha256("synthetic-workload"), cwd: process.cwd() }, runtime: await pin(runtimePath), harborExport: await pin(exportAcceptancePath) };
    const acceptancePath = join(root, "controller-acceptance.json"), bindingPath = join(root, "binding.json");
    const options = { admission: { acceptance: { path: acceptancePath, sha256: "" }, profileId: "bare-raw" }, outputDir };
    const seal = async () => {
      await write(acceptancePath, acceptance); options.admission.acceptance.sha256 = await fileHash(acceptancePath);
      const argv = acceptance.controllerArgv.map(v => v === "@ACCEPTANCE_PATH@" ? acceptancePath : v === "@ACCEPTANCE_SHA256@" ? options.admission.acceptance.sha256 : v);
      await write(bindingPath, { schema: "environments-harness.storage-binding.v1", targets: { output: root, test_data: root }, plan: {
        command: { executable: argv[0], executable_sha256: await fileHash(argv[0]!), argv_sha256: digest(argv) }, cwd: process.cwd(), run_id: acceptance.storage.runId, workload_sha256: acceptance.storage.workloadSha256 } });
      process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = bindingPath; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = await fileHash(bindingPath);
    };
    const repinExport = async () => {
      generated.tasks[0].files = await indexTaskFiles(taskDir); await write(generatedPath, generated);
      exported.generated = await pin(generatedPath); exportReview.generated = exported.generated; await write(reviewPath, exportReview);
      exported.review = await pin(reviewPath); await write(exportAcceptancePath, exported); acceptance.harborExport = await pin(exportAcceptancePath); await seal();
    };
    await seal();
    return { root, options, acceptance, assessment, release, task, taskDir, context, manifest, generated, exported, exportReview, reviewPath,
      runtime, runtimePath, qualification, qualificationPath, exportAcceptancePath, currentPath, bindingPath,
      write, pin, seal, repinExport, restore };
  } catch (error) { await restore(); throw error; }
}

async function refusesBeforeCallback(f: Awaited<ReturnType<typeof fixture>>) {
  let launches = 0;
  const before = new Set(await readdir(f.options.outputDir).catch((error: NodeJS.ErrnoException) => { if (error.code === "ENOENT") return []; throw error; }));
  await expect(runHarborForTest(f.options, async () => { launches++; return 0; })).rejects.toThrow();
  expect(launches).toBe(0);
  const denials = (await readdir(f.options.outputDir)).filter(name => name.startsWith("prelaunch-denial-") && !before.has(name));
  expect(denials).toHaveLength(1);
  const receipt = JSON.parse(await readFile(join(f.options.outputDir, denials[0]!), "utf8"));
  expect(receipt.processStarted).toBe(false); expect(receipt.measurement).toBeNull(); expect(receipt.executionMode).toBe("test");
}

describe("closed task-aware Harbor dispatch", () => {
  test("prebuilt images require immutable digest references without injected syntax", () => {
    const hash = sha256("synthetic image");
    for (const value of [`sha256:${hash}`, `registry.example.invalid:5000/team/reader@sha256:${hash}`, `reader:reviewed@sha256:${hash}`]) expect(validImmutableImage(value)).toBe(true);
    for (const value of ["reader:latest", `reader@sha256:${hash.slice(1)}`, `reader@sha256:${hash.toUpperCase()}`, `reader@other@sha256:${hash}`,
      `reader@sha256:${hash}\nextra = true`, `reader@sha256:${hash};touch /tmp/file`, null]) expect(validImmutableImage(value)).toBe(false);
  });
  test("custom agent import syntax is explicit and cannot inject command arguments", () => {
    expect(validHarborAgent("gptgrep_harbor.native:NativeCodexAgent")).toBe(true);
    expect(validHarborAgent("codex")).toBe(true);
    for (const value of ["module:Class --flag", "/tmp/agent.py:Agent", "module:", "module:Class:Other", "module;command"]) expect(validHarborAgent(value)).toBe(false);
  });
  test("synthetic accepted nested controller closure constructs the fixed direct-supervisor argv", async () => {
    const f = await fixture();
    try {
      const ready = await harborPreflight(f.options);
      expect(ready.taskIds).toEqual(["series"]); expect(ready.argv.slice(-8)).toEqual([f.runtime.harbor.path, "run", "--path", join(f.root, "export"), "--agent", "synthetic-agent", "--job-name", "synthetic-run"]);
      let launches = 0; const result = await runHarborForTest(f.options, async argv => { launches++; expect(argv).toEqual(ready.argv); return 0; });
      expect(launches).toBe(1); expect(result.executionMode).toBe("test");
    } finally { await f.restore(); }
  });
  test("prebuilt exports validate image and workspace bindings but cannot use the legacy launcher", async () => {
    const f = await fixture();
    try {
      const image = `reader@sha256:${sha256("synthetic prebuilt image")}`;
      const execution = { kind: "prebuilt-bind", image, workspace: "/app/task" };
      (f.manifest as any).execution = execution;
      for (const document of f.context.documents) document.path = document.path.replace("/app/documents/", "/app/task/documents/");
      const taskConfig = (selected: string) => `[environment]\ndocker_image = ${JSON.stringify(selected)}\n`;
      await f.write(join(f.taskDir, "adapter-manifest.json"), f.manifest);
      await f.write(join(f.taskDir, "environment/context.json"), f.context);
      await writeFile(join(f.taskDir, "task.toml"), taskConfig(image)); await f.repinExport();
      await expect(harborPreflight(f.options)).rejects.toThrow("paired controller v2");
      await refusesBeforeCallback(f);

      await writeFile(join(f.taskDir, "task.toml"), taskConfig(`reader@sha256:${sha256("foreign image")}`));
      await f.repinExport(); await expect(harborPreflight(f.options)).rejects.toThrow("image differs"); await refusesBeforeCallback(f);
      await writeFile(join(f.taskDir, "task.toml"), taskConfig(image));
      execution.workspace = "/app"; await f.write(join(f.taskDir, "adapter-manifest.json"), f.manifest);
      await f.repinExport(); await expect(harborPreflight(f.options)).rejects.toThrow("execution binding"); await refusesBeforeCallback(f);
      execution.workspace = "/app/task"; await f.write(join(f.taskDir, "adapter-manifest.json"), f.manifest);
      f.context.documents[0]!.path = f.context.documents[0]!.path.replace("/app/task/documents/", "/app/documents/");
      await f.write(join(f.taskDir, "environment/context.json"), f.context); await f.repinExport();
      await expect(harborPreflight(f.options)).rejects.toThrow("Unsafe generated RAW path"); await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("absent or unqualified runtime/export denies before any callback", async () => {
    const f = await fixture(); try {
      f.acceptance.runtime = null; await f.seal(); await refusesBeforeCallback(f);
      f.acceptance.runtime = await f.pin(f.runtimePath); f.runtime.status = "pending"; await f.write(f.runtimePath, f.runtime);
      f.acceptance.runtime = await f.pin(f.runtimePath); await f.seal(); await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("export review rejects an unrelated target, rejected status or synthetic placeholder", async () => {
    const f = await fixture(); try {
      for (const status of ["rejected", "synthetic"]) {
        f.exportReview.status = status; await f.repinExport(); await refusesBeforeCallback(f);
      }
      f.exportReview.status = "accepted"; await f.repinExport();
      f.exportReview.generated = { ...f.exported.generated, sha256: sha256("another export") };
      await f.write(f.reviewPath, f.exportReview); f.exported.review = await f.pin(f.reviewPath);
      await f.write(f.exportAcceptancePath, f.exported); f.acceptance.harborExport = await f.pin(f.exportAcceptancePath); await f.seal();
      await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("runtime qualification must accept the exact selected contract", async () => {
    const f = await fixture(); try {
      for (const change of [{ status: "rejected" }, { status: "synthetic" }, { status: "accepted", runtimeContractSha256: sha256("another runtime") }]) {
        Object.assign(f.qualification, change); await f.write(f.qualificationPath, f.qualification);
        f.runtime.qualification = await f.pin(f.qualificationPath); await f.write(f.runtimePath, f.runtime);
        f.acceptance.runtime = await f.pin(f.runtimePath); await f.seal(); await refusesBeforeCallback(f);
      }
    } finally { await f.restore(); }
  });
  test("a task cannot borrow required OCR artifact presence from a second task with the same context", async () => {
    const f = await fixture(); try {
      const { schema, tasks, dependencySha256, ...input } = f.assessment;
      const second = { ...input.release.tasks[0]!, id: "series-2" };
      input.release.tasks.push(second); input.release.groups[0]!.taskIds.push(second.id);
      input.profiles = [{ id: "gptgrep-ocr", arm: "gptgrep", implementation: "implemented", requiresOcr: true, preprocessingProfileSha256: sha256("ocr") }];
      const provenance = join(f.root, "ocr-provenance.json"); await writeFile(provenance, "synthetic parser");
      input.preprocessing = [];
      for (const doc of input.documents) {
        const artifact = join(f.root, `${doc.documentId}-ocr.md`), text = `synthetic OCR ${doc.documentId}`; await writeFile(artifact, text);
        const pageManifestPath = join(f.root, `${doc.documentId}-pages.json`);
        await f.write(pageManifestPath, { schema: "gptgrep.ocr-page-manifest.v1", documentId: doc.documentId, rawSha256: doc.rawSha256, profileSha256: sha256("ocr"),
          pageAssets: [{ page: 1, rawPage: 1, markdown: { ...await f.pin(artifact), relativePath: `ocr/${doc.documentId}/1.md`, bytes: Buffer.byteLength(text) },
            images: [], zeroByteDisposition: "not_empty", blankReview: null }] });
        const pageManifest = await f.pin(pageManifestPath);
        input.preprocessing.push({ documentId: doc.documentId, rawSha256: doc.rawSha256!, profileSha256: sha256("ocr"), state: "verified", pages: [1],
          pageManifest, artifacts: [pageManifest], provenance: [await f.pin(provenance)] });
      }
      const next = assessAvailability({ ...input, revision: "assessment-ocr", generation: 2, supersedes: f.acceptance.assessment });
      const path = join(f.root, "assessment-ocr.json"); await f.write(path, next); f.acceptance.assessment = await f.pin(path);
      f.acceptance.generation = 2; f.acceptance.assessmentRevision = next.revision; f.acceptance.taskIds = ["series", "series-2"];
      f.acceptance.profileIds = ["gptgrep-ocr"]; f.options.admission.profileId = "gptgrep-ocr";
      await f.write(f.currentPath, { schema: "gptgrep.availability-current.v1", status: "current", generation: 2, assessmentRevision: next.revision, assessmentSha256: f.acceptance.assessment.sha256 });
      f.acceptance.current = await f.pin(f.currentPath);
      f.manifest.arm = "gptgrep"; Object.assign(f.manifest.availability, { assessmentSha256: f.acceptance.assessment.sha256, assessmentRevision: next.revision,
        generation: 2, dependencySha256: next.dependencySha256, profileId: "gptgrep-ocr", profileSha256: digest(next.profiles[0]) });
      await f.write(join(f.taskDir, "adapter-manifest.json"), f.manifest);
      const secondDir = join(f.root, "export/series-2"); await cp(f.taskDir, secondDir, { recursive: true });
      const secondManifest: any = structuredClone(f.manifest); secondManifest.task_id = second.id;
      secondManifest.availability.taskDependencySha256 = digest(second); secondManifest.prepared_context_document_ids = [...second.contextDocumentIds];
      secondManifest.prepared_artifacts = [];
      for (const prep of input.preprocessing) {
        const pages = await readExactJson(prep.pageManifest!.path), markdown = pages.pageAssets[0].markdown;
        const relative = markdown.relativePath, target = join(secondDir, "environment/prepared", relative);
        await mkdir(dirname(target), { recursive: true }); await cp(markdown.path, target);
        secondManifest.prepared_artifacts.push({ relative_path: relative, sha256: markdown.sha256, scope: "context" });
      }
      await f.write(join(secondDir, "adapter-manifest.json"), secondManifest);
      await f.write(join(secondDir, "environment/context.json"), { ...f.context, task_id: second.id });
      Object.assign(f.generated, { assessmentSha256: f.acceptance.assessment.sha256, generation: 2, profileId: "gptgrep-ocr", arm: "gptgrep" });
      f.generated.tasks.push({ taskId: second.id, directory: secondDir, files: await indexTaskFiles(secondDir) }); await f.repinExport();
      let launches = 0;
      await expect(runHarborForTest(f.options, async () => { launches++; return 0; })).rejects.toThrow("Required OCR page artifact absent or at the wrong workspace path");
      expect(launches).toBe(0);
    } finally { await f.restore(); }
  });
  test("new current generation immediately invalidates older assessment and export selection", async () => {
    const f = await fixture(); try {
      await f.write(f.currentPath, { schema: "gptgrep.availability-current.v1", status: "current", generation: 2, assessmentRevision: "assessment-2", assessmentSha256: sha256("new") });
      await refusesBeforeCallback(f);
      f.acceptance.current = await f.pin(f.currentPath); await f.seal(); await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("a newly accepted recovery hold blocks the complete series even with retained exported bytes", async () => {
    const f = await fixture(); try {
      const { schema, tasks, dependencySha256, ...input } = f.assessment;
      input.documents[1]!.rawState = "recovery_pending";
      const next = assessAvailability({ ...input, revision: "assessment-2", generation: 2, supersedes: f.acceptance.assessment });
      const path = join(f.root, "assessment-2.json"); await f.write(path, next);
      f.acceptance.assessment = await f.pin(path); f.acceptance.assessmentRevision = next.revision; f.acceptance.generation = 2;
      await f.write(f.currentPath, { schema: "gptgrep.availability-current.v1", status: "current", generation: 2, assessmentRevision: next.revision, assessmentSha256: f.acceptance.assessment.sha256 });
      f.acceptance.current = await f.pin(f.currentPath);
      Object.assign(f.manifest.availability, { assessmentSha256: f.acceptance.assessment.sha256, assessmentRevision: next.revision, generation: 2 });
      await f.write(join(f.taskDir, "adapter-manifest.json"), f.manifest);
      f.generated.assessmentSha256 = f.acceptance.assessment.sha256; f.generated.generation = 2; await f.repinExport();
      await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("changed exported RAW bytes and repinned narrowed contexts deny", async () => {
    const f = await fixture(); try {
      await writeFile(join(f.taskDir, "environment/documents/d2.pdf"), "different edition"); await refusesBeforeCallback(f);
      await writeFile(join(f.taskDir, "environment/documents/d2.pdf"), "d2");
      f.context.documents = f.context.documents.slice(0, 1); await f.write(join(f.taskDir, "environment/context.json"), f.context);
      await f.repinExport(); await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("accepted task set cannot be narrowed, duplicated, expanded or overridden", async () => {
    const f = await fixture(); try {
      f.acceptance.taskIds.push("foreign"); await f.seal(); await refusesBeforeCallback(f);
      f.acceptance.taskIds = ["series"]; await f.seal();
      f.generated.tasks.push(f.generated.tasks[0]); await f.repinExport(); await refusesBeforeCallback(f);
      await expect(harborPreflight({ ...f.options, args: ["--path", "/other"] } as any)).rejects.toThrow("unknown fields");
    } finally { await f.restore(); }
  });
  test("unaccepted extra task directories and missing runnable material deny", async () => {
    const f = await fixture(); try {
      await mkdir(join(f.root, "export/extra-task")); await refusesBeforeCallback(f); await rm(join(f.root, "export/extra-task"), { recursive: true });
      await rm(join(f.taskDir, "task.toml")); await f.repinExport(); await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("binding to a different command or child closure is not acceptance", async () => {
    const f = await fixture(); try {
      f.acceptance.childArgv = []; await f.seal(); await refusesBeforeCallback(f);
      f.acceptance.childArgv = [(await processCommandIdentity()).argv]; await f.seal();
      const binding = JSON.parse(await readFile(f.bindingPath, "utf8")); binding.plan.command.argv_sha256 = sha256("different command");
      await f.write(f.bindingPath, binding); process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = await fileHash(f.bindingPath);
      await refusesBeforeCallback(f);
    } finally { await f.restore(); }
  });
  test("prospective output need not exist while selected JSON inputs remain pinned", async () => {
    const f = await fixture(); try {
      const configPath = join(f.root, "selected-config.json"), outputPath = join(f.root, "new-loop.json");
      await f.write(configPath, { synthetic: true });
      f.acceptance.controllerArgv.push("--config", configPath, "--output", outputPath); await f.seal();
      await expect(loadAdmission(f.options.admission, f.options.outputDir)).rejects.toThrow("source closure is not pinned");
      f.acceptance.sourcePins.push(await f.pin(configPath)); await f.seal();
      await loadAdmission(f.options.admission, f.options.outputDir);
      await expect(stat(outputPath)).rejects.toThrow();
      f.acceptance.controllerArgv.push("--parent"); await f.seal();
      await expect(loadAdmission(f.options.admission, f.options.outputDir)).rejects.toThrow("requires an exact path");
    } finally { await f.restore(); }
  });
  test("relative controller scripts resolve against checked cwd and require a source pin", async () => {
    const f = await fixture(); try {
      const controllerPath = f.acceptance.controllerArgv[1]!;
      f.acceptance.controllerArgv[1] = relative(process.cwd(), controllerPath); await f.seal();
      await loadAdmission(f.options.admission, f.options.outputDir);
      f.acceptance.sourcePins = f.acceptance.sourcePins.filter(p => p.path !== controllerPath); await f.seal();
      await expect(loadAdmission(f.options.admission, f.options.outputDir)).rejects.toThrow("source closure is not pinned");
    } finally { await f.restore(); }
  });
  test("episode, capture, judge and scoring code belong to the accepted source closure", async () => {
    const f = await fixture(); try {
      const original = [...f.acceptance.sourcePins];
      for (const path of ["../src/episode.ts", "../src/trajectory.ts", "../src/judge.ts", "../src/controller.ts", "../src/campaign.ts", "../src/container.ts", "../src/process.ts", "../src/harbor-wrapper.ts", "../harbor/native_agent.py", "../../self-opt/src/evaluate.ts"]) {
        f.acceptance.sourcePins = original.filter(pin => pin.path !== resolve(import.meta.dir, path)); await f.seal();
        await expect(loadAdmission(f.options.admission, f.options.outputDir)).rejects.toThrow("source closure is not pinned");
      }
    } finally { await f.restore(); }
  });
  test("runtime argv/environment override fields and duplicate JSON keys are rejected", async () => {
    const f = await fixture(); try {
      f.runtime.args = ["--path", "/other"]; await f.write(f.runtimePath, f.runtime); f.acceptance.runtime = await f.pin(f.runtimePath); await f.seal();
      await refusesBeforeCallback(f);
      const path = join(f.root, "duplicate.json"); await writeFile(path, '{"field":1,"field":2}'); await expect(readExactJson(path)).rejects.toThrow("Duplicate JSON key");
      await writeFile(path, '{"nested":{"field":1,"field":2}}'); await expect(readExactJson(path)).rejects.toThrow("Duplicate JSON key");
    } finally { await f.restore(); }
  });
  test("direct export API admission cannot be omitted before reading or creating task material", async () => {
    const f = await fixture(); try {
      const target = join(f.root, "denied-export");
      await expect(writeHarborTasks({ admission: undefined, release: { readerTasks: [{ ...f.task, question: "synthetic" }] },
        rawDocuments: [], outputDir: target, arm: "bare" } as any)).rejects.toThrow();
      await expect(stat(target)).rejects.toThrow();
      const admission = await loadAdmission(f.options.admission, f.options.outputDir); expect(admission.assessment.tasks).toHaveLength(1);
    } finally { await f.restore(); }
  });
  test("prebuilt export refuses mutable images and an ambiguous build base before materializing files", async () => {
    const f = await fixture(); try {
      const target = join(f.root, "denied-prebuilt-export"), options = { admission: f.options.admission,
        release: { readerTasks: [{ ...f.task, question: "synthetic" }] }, rawDocuments: [], outputDir: target, arm: "bare" };
      await expect(writeHarborTasks({ ...options, prebuiltImage: "reader:latest" } as any)).rejects.toThrow("immutable image");
      await expect(writeHarborTasks({ ...options, prebuiltImage: `reader@sha256:${sha256("image")}`, image: "ubuntu:24.04" } as any)).rejects.toThrow("build base");
      await expect(stat(target)).rejects.toThrow();
    } finally { await f.restore(); }
  });
});
