import { mkdir, readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { corpusDigest, writeHarborTasks, type RawDocument } from "./adapter";
import { loadDependencyRelease, loadXlRelease, splitBySourceGroups, XL_RELEASE_REVISION } from "./dataset";
import { digest, fileHash, physicalPath, readJson, requireStorage, sha256, within, writeJson } from "./io";
import { judgeAnswer, type JudgeOptions } from "./judge";
import { runReaderArm, type ReaderArmOptions } from "./runner";
import { verifiedSource } from "./source-verification";
import { validateFrozenCohort } from "./cohort";
import { assessAvailability, assertAcquiredStates, assertTaskAdmitted, availabilityCoverage, loadAdmission, readExactJson, verifyPreprocessingPins, writeAssessment, type AdmissionReference } from "./availability";
import { harborPreflight, runHarbor, type HarborLaunchOptions } from "./harbor-launch";

function options(args: string[]) {
  const values = new Map<string, string>();
  for (let i = 0; i < args.length; i += 2) {
    if (!args[i]?.startsWith("--") || args[i + 1] === undefined || values.has(args[i]!.slice(2))) throw new Error("Expected unique --key value arguments");
    values.set(args[i]!.slice(2), args[i + 1]!);
  }
  return { values, get: (key: string) => { const value = values.get(key); if (!value) throw new Error(`Missing --${key}`); return value; } };
}

/** Controller-only: never expose the combined release/gold directory to a reader. */
async function acquiredInputs(releaseRoot: string, corpusRoot: string) {
  const acquisition = await readJson(join(releaseRoot, "acquisition.json"));
  if (acquisition.schema !== "gptgrep.xl-release-acquisition.v1" || acquisition.revision !== XL_RELEASE_REVISION) throw new Error("Pinned release acquisition receipt required");
  for (const [file, record] of Object.entries(acquisition.files) as [string, { sha256: string }][]) {
    const path = resolve(releaseRoot, file);
    if (!within(releaseRoot, path) || await fileHash(path) !== record.sha256) throw new Error("Release file binding changed");
  }
  const raw = await readJson(join(corpusRoot, "raw-manifest.json"));
  if (raw.schema !== "gptgrep.raw-corpus.v1" || raw.revision !== XL_RELEASE_REVISION || raw.metadataSha256 !== acquisition.files["data/documents.jsonl"].sha256) throw new Error("Raw corpus source binding mismatch");
  const metadata = new Map<string, number>((await readFile(join(releaseRoot, "data/documents.jsonl"), "utf8"))
    .trim().split("\n").map(line => { const value = JSON.parse(line); return [value.document_id, value.page_count]; }));
  const documents: RawDocument[] = [];
  for (const record of raw.documents) if (verifiedSource(record, metadata.get(record.documentId) ?? 0)) {
    const path = resolve(corpusRoot, record.path);
    await physicalPath(path);
    if (!within(corpusRoot, path) || await fileHash(path) !== record.sha256) throw new Error("RAW file changed or escaped its corpus root");
    documents.push({ documentId: record.documentId, path, sha256: record.sha256,
      pageCount: record.pageCount });
  }
  const rawSha256 = Object.fromEntries(documents.map(d => [d.documentId, d.sha256]));
  const known = new Set(metadata.keys());
  if (new Set(raw.documents.map((d: any) => d.documentId)).size !== raw.documents.length || raw.documents.some((d: any) => !known.has(d.documentId))) throw new Error("RAW manifest has duplicate or foreign documents");
  return { acquisition, rawSha256, raw, documents };
}
async function sourceInputs(releaseRoot: string, corpusRoot: string) {
  const inputs = await acquiredInputs(releaseRoot, corpusRoot);
  return { ...inputs, release: await loadXlRelease({ releaseRoot, revision: inputs.acquisition.revision, rawSha256: inputs.rawSha256 }) };
}

const [command, ...args] = process.argv.slice(2);
if (!command || command === "help" || command === "--help") {
  console.log("GPTgrep XL Benchmark Kit\nassess-availability --release ROOT --corpus ROOT --states JSON --output JSON\nfreeze --release ROOT --corpus ROOT --output DIR --seed VALUE --profile ID --acceptance JSON --acceptance-sha256 SHA [--sample-size N] [--dev-groups N]\nexport-harbor --release ROOT --corpus ROOT --cohort JSON --arm bare|gptgrep --output DIR --profile ID --acceptance JSON --acceptance-sha256 SHA [--runtime-artifacts JSON]\nreader --config JSON --acceptance JSON --acceptance-sha256 SHA\nharbor-preflight|harbor-run --config JSON --acceptance JSON --acceptance-sha256 SHA\njudge --config JSON\nAcceptance must be selected by the exact owner-admitted command or its pinned child closure. Raw/OCR acquisition remains a separate workflow.");
} else {
  const { values, get } = options(args);
  const reference = (): AdmissionReference => ({ acceptance: { path: resolve(get("acceptance")), sha256: get("acceptance-sha256") }, profileId: get("profile") });
  const allowed: Record<string, string[]> = {
    "assess-availability": ["release", "corpus", "states", "output"],
    freeze: ["release", "corpus", "output", "seed", "profile", "acceptance", "acceptance-sha256", "sample-size", "dev-groups"],
    "export-harbor": ["release", "corpus", "cohort", "arm", "output", "runtime-artifacts", "profile", "acceptance", "acceptance-sha256"],
    reader: ["config", "acceptance", "acceptance-sha256"], judge: ["config"],
    "harbor-preflight": ["config", "acceptance", "acceptance-sha256"], "harbor-run": ["config", "acceptance", "acceptance-sha256"],
  };
  if (!allowed[command] || [...values.keys()].some(key => !allowed[command]!.includes(key))) throw new Error("Unknown command or option");
  if (command === "assess-availability") {
    const output = resolve(get("output")); await requireStorage(output);
    const releaseRoot = resolve(get("release")), inputs = await acquiredInputs(releaseRoot, resolve(get("corpus")));
    const release = await loadDependencyRelease({ releaseRoot, revision: inputs.acquisition.revision, rawSha256: inputs.rawSha256 });
    const state = await readExactJson(get("states"));
    if (Object.keys(state).sort().join("\0") !== ["revision", "generation", "recordedAt", "supersedes", "documents", "profiles", "preprocessing"].sort().join("\0")) throw new Error("Unknown or missing assessment input fields");
    const input = { ...state, release };
    assertAcquiredStates(input, inputs.documents, inputs.raw.documents);
    const assessment = assessAvailability(input);
    await writeAssessment(output, assessment);
    console.log(JSON.stringify({ assessment: output, sha256: await fileHash(output), coverage: availabilityCoverage(assessment) }));
  } else if (command === "freeze") {
    const output = resolve(get("output")); await requireStorage(output);
    const admission = await loadAdmission(reference(), output);
    const { release, raw, documents } = await sourceInputs(resolve(get("release")), resolve(get("corpus")));
    const seed = get("seed"), count = Number(values.get("sample-size") ?? 4);
    if (!Number.isSafeInteger(count) || count < 1) throw new Error("Sample size must be a positive integer");
    const split = splitBySourceGroups(release, { seed, ...(values.has("dev-groups") ? { devGroupCount: Number(get("dev-groups")) } : {}) });
    if (digest(release.fileHashes) !== digest(admission.assessment.release.fileHashes)) throw new Error("Freeze release differs from assessment");
    const dev = new Set(split.dev.taskIds), eligibleIds = new Set(admission.assessment.tasks.filter(t => t.armAvailability[reference().profileId]?.admitted).map(t => t.taskId));
    const eligible = release.readerTasks.filter(t => dev.has(t.id) && eligibleIds.has(t.id) && admission.acceptance.taskIds.includes(t.id))
      .sort((a, b) => sha256(`${seed}\0${a.id}`).localeCompare(sha256(`${seed}\0${b.id}`)));
    await mkdir(output, { recursive: false });
    await writeJson(join(output, "availability-coverage.json"), availabilityCoverage(admission.assessment));
    await writeJson(join(output, "split.json"), split);
    if (eligible.length < count) throw new Error("Insufficient acquired development tasks for the declared frozen sample");
    const tasks = eligible.slice(0, count);
    const profile = admission.assessment.profiles.find(p => p.id === reference().profileId)!;
    for (const task of tasks) assertTaskAdmitted(admission, task, profile.arm, documents);
    await verifyPreprocessingPins(admission, tasks);
    const sourceIds = new Set(tasks.flatMap(t => t.contextDocumentIds));
    const selected = documents.filter(d => sourceIds.has(d.documentId));
    await writeJson(join(output, "cohort.json"), { schema: "gptgrep.xl-cohort.v1", revision: release.revision, split: "dev", seed,
      selectionRule: "seeded task order within source-group development split; all public context documents must have verified acquired bytes before enrollment",
      sampleSize: tasks.length, taskIds: tasks.map(t => t.id), readerTasks: tasks,
      documents: selected, corpusSha256: corpusDigest(selected),
      sourceAvailabilityAtFreeze: { released: release.documents.length, verified: documents.length,
        acquired: raw.documents.filter((d: any) => d.status === "ready").length,
        held: raw.documents.filter((d: any) => d.status === "held").length, failed: raw.failed, pending: raw.pending },
      enrolledTasksNeverDropped: true, fullBenchmarkAccepted: false, sourceFileHashes: release.fileHashes,
      availabilityAtEnrollment: { assessmentSha256: admission.acceptance.assessment.sha256, revision: admission.assessment.revision,
        generation: admission.assessment.generation, profileId: reference().profileId },
      splitSha256: await fileHash(join(output, "split.json")) });
    console.log(JSON.stringify({ tasks: tasks.length, documents: selected.length, cohort: join(output, "cohort.json"), cohortSha256: await fileHash(join(output, "cohort.json")) }));
  } else if (command === "export-harbor") {
    const output = resolve(get("output")); await requireStorage(output);
    const { release, documents } = await sourceInputs(resolve(get("release")), resolve(get("corpus")));
    const cohort = validateFrozenCohort(await readJson(get("cohort")), release, documents);
    const arm = get("arm"); if (arm !== "bare" && arm !== "gptgrep") throw new Error("Unsupported initial benchmark arm");
    const rubricPath = resolve(import.meta.dir, "../rubric.json");
    const prepared = values.has("runtime-artifacts") ? await readJson(get("runtime-artifacts")) : {};
    const tasks = await writeHarborTasks({ admission: reference(), release, rawDocuments: documents, taskIds: cohort.taskIds, arm, outputDir: output,
      rubricSha256: await fileHash(rubricPath), preparedArtifacts: prepared.artifacts ?? [], preparedContextDocumentIds: prepared.contextDocumentIds });
    console.log(JSON.stringify({ tasks: tasks.length, arm, output }));
  } else if (command === "reader") {
    const config = await readExactJson<ReaderArmOptions>(get("config"));
    if (config.admission?.acceptance.path !== resolve(get("acceptance")) || config.admission.acceptance.sha256 !== get("acceptance-sha256")) throw new Error("Reader acceptance override denied");
    const result = await runReaderArm(config);
    console.log(JSON.stringify({ status: result.status, taskId: result.taskId, arm: result.arm, answerPath: result.answerPath }));
    if (result.native?.authFailure) process.exitCode = 77;
    else if (result.status !== "completed") process.exitCode = 2;
  } else if (command === "harbor-preflight" || command === "harbor-run") {
    const config = await readExactJson<HarborLaunchOptions>(get("config"));
    if (config.admission?.acceptance.path !== resolve(get("acceptance")) || config.admission.acceptance.sha256 !== get("acceptance-sha256")) throw new Error("Harbor acceptance override denied");
    const result = command === "harbor-run" ? await runHarbor(config) : await harborPreflight(config);
    console.log(JSON.stringify(result));
    if ("exitCode" in result && result.exitCode !== 0) process.exitCode = result.exitCode;
  } else if (command === "judge") {
    const result = await judgeAnswer(await readJson<JudgeOptions>(get("config")));
    console.log(JSON.stringify({ taskId: result.task_id, status: result.status, groundedSuccess: result.grounded_success }));
    if (result.judge_run?.authFailure) process.exitCode = 77;
    else if (result.status !== "complete") process.exitCode = 2;
  } else throw new Error(`Unknown benchmark command: ${command}`);
}
