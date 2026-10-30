import { constants } from "node:fs";
import { access, copyFile, mkdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { selectReaderTasks, sha256, XL_NATIVE_SCORER_SHA256, type ReaderTask, type XlRelease } from "./dataset";
import { assertTaskAdmitted, dependencyProjection, loadAdmission, verifyRequiredPreprocessing, type AdmissionReference } from "./availability";
import { digest, writeJson } from "./io";
import { indexTaskFiles, validImmutableImage } from "./harbor-launch";

export interface RawDocument {
  documentId: string;
  path: string;
  sha256: string;
  pageCount?: number;
}

export interface PreparedArtifact {
  path: string;
  /** Relative to /opt/gptgrep; source must be a regular file. */
  relativePath: string;
  sha256: string;
  /** Only tool binaries/scripts may declare tool; indexes remain context-scoped. */
  scope?: "tool" | "context";
}

export interface HarborAdapterOptions {
  admission: AdmissionReference;
  release: XlRelease;
  outputDir: string;
  rawDocuments: RawDocument[];
  taskIds?: string[];
  limit?: number;
  overwrite?: boolean;
  arm: "bare" | "gptgrep";
  image?: string;
  /** Existing immutable image plus separately reviewed binds; no task image build. */
  prebuiltImage?: string;
  agentTimeoutSec?: number;
  verifierTimeoutSec?: number;
  rubricSha256?: string;
  preparedArtifacts?: PreparedArtifact[];
  preparedContextDocumentIds?: string[];
  /** Model/auth endpoints only; undefined leaves the agent environment offline. */
  modelAllowedHosts?: string[];
  /** Portable verifier files; defaults to benchmark/harbor next to this module. */
  verifierRoot?: string;
}

export interface GeneratedHarborTask {
  id: string;
  name: string;
  directory: string;
  contextId: string;
  groupId: string;
  corpusSha256: string;
  arm: "bare" | "gptgrep";
}

export const DEFAULT_VERIFIER_ROOT = fileURLToPath(new URL("../harbor", import.meta.url));

function requireHash(value: string, label: string): string {
  if (!/^[a-f0-9]{64}$/i.test(value)) throw new Error(`${label}: expected SHA-256`);
  return value.toLowerCase();
}

export async function hashFile(path: string): Promise<string> {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  return digest.digest("hex");
}

/** Content identity shared by controller, reader manifest and verifier binding. */
export function corpusDigest(documents: Pick<RawDocument, "documentId" | "sha256">[]): string {
  const records = documents.map((d) => ({ document_id: d.documentId, sha256: requireHash(d.sha256, d.documentId) }))
    .sort((a, b) => a.document_id.localeCompare(b.document_id));
  if (new Set(records.map((r) => r.document_id)).size !== records.length) throw new Error("Duplicate corpus document ID");
  return sha256(JSON.stringify(records));
}

/** Stable and collision-resistant even when upstream IDs sanitize to the same slug. */
export function harborTaskName(taskId: string): string {
  const readable = taskId.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 80) || "task";
  return `gptgrep/xl-docbench-${readable}-${sha256(taskId).slice(0, 12)}`;
}

function pdfFilename(documentId: string): string {
  return `document-${sha256(documentId).slice(0, 24)}.pdf`;
}

function relativeDestination(value: string): string {
  if (!value || isAbsolute(value) || value.includes("\\") || value.split("/").some((v) => !v || v === "." || v === "..") || /[\r\n\0]/.test(value)) {
    throw new Error(`Unsafe relative artifact path: ${value}`);
  }
  return value;
}

async function exists(path: string): Promise<boolean> {
  try { await access(path); return true; } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

async function copyRegular(source: string, destination: string): Promise<void> {
  if (!(await stat(source)).isFile()) throw new Error(`Expected regular file: ${source}`);
  await mkdir(dirname(destination), { recursive: true });
  // APFS reflink where supported; copyFile falls back without requiring it.
  await copyFile(source, destination, constants.COPYFILE_FICLONE);
}

function taskInstruction(task: ReaderTask, workspace: "/app" | "/app/task"): string {
  return `${task.question}\n\n` +
    `Use the supplied original PDF documents in ${workspace}/documents. ` +
    `The mapping from document IDs to files is ${workspace}/context.json. ` +
    "Do not use the internet to obtain an answer.\n\n" +
    "Write /app/answer.json as a JSON object with a short string answer and a citations array. " +
    'Each citation must have document_id and page (one-based PDF page index). ' +
    'Put any explanation in a separate explanation field. If the supplied documents do not determine an answer, use "not answerable" as the answer and explain what is missing.\n';
}

function taskToml(options: HarborAdapterOptions, task: ReaderTask): string {
  const timeout = options.agentTimeoutSec ?? 900;
  const verifierTimeout = options.verifierTimeoutSec ?? 120;
  if (![timeout, verifierTimeout].every((n) => Number.isFinite(n) && n > 0)) throw new Error("Timeouts must be positive");
  const hosts = options.modelAllowedHosts ?? [];
  if (hosts.some((h) => !h || /[\s/\\]/.test(h) || h === "*")) throw new Error("Model allowlist requires explicit hostnames or IPs");
  return `schema_version = "1.4"\n` +
    `artifacts = [{ source = "/app/answer.json", destination = "answer.json" }]\n\n` +
    `[task]\nname = ${JSON.stringify(harborTaskName(task.id))}\nversion = "1.0.0"\n` +
    `description = "XL-DocBench document question answering"\nkeywords = ["xl-docbench", "document-search", "rewardkit"]\n\n` +
    `[metadata]\nsource_revision = ${JSON.stringify(options.release.revision)}\nupstream_question_id = ${JSON.stringify(task.id)}\n` +
    `context_id = ${JSON.stringify(task.contextId)}\ngroup_id = ${JSON.stringify(task.groupId)}\narm = ${JSON.stringify(options.arm)}\n\n` +
    `[agent]\ntimeout_sec = ${timeout}\n\n` +
    `[environment]\n` + (options.prebuiltImage ? `docker_image = ${JSON.stringify(options.prebuiltImage)}\n` : "") +
    `build_timeout_sec = 600\ncpus = 2\nmemory_mb = 4096\nstorage_mb = 20480\n` +
    `network_mode = ${JSON.stringify(hosts.length ? "allowlist" : "no-network")}\n` +
    (hosts.length ? `allowed_hosts = ${JSON.stringify(hosts)}\n` : "") + "\n" +
    `[verifier]\ntimeout_sec = ${verifierTimeout}\nenvironment_mode = "separate"\n\n` +
    `[verifier.environment]\nnetwork_mode = "no-network"\n`;
}

function agentDockerfile(options: HarborAdapterOptions): string {
  if (options.prebuiltImage) return "# This task requires the pinned prebuilt image and reviewed bind mounts.\nFROM scratch\nRUN [\"/prebuilt-bind-required\"]\n";
  const image = options.image ?? "ubuntu:24.04";
  if (!/^[a-zA-Z0-9][a-zA-Z0-9._/:@-]*$/.test(image)) throw new Error("Invalid container image reference");
  return `FROM ${image}\nWORKDIR /app\nCOPY documents/ /app/documents/\nCOPY context.json /app/context.json\n` +
    (options.arm === "gptgrep" && options.preparedArtifacts?.length
      ? 'COPY prepared/ /opt/gptgrep/\nENV PATH="/opt/gptgrep/bin:${PATH}"\n' : "") +
    "RUN chmod -R a-w /app/documents /app/context.json\n";
}

/**
 * Materializes Harbor tasks, not live runs. Reader image has only allowlisted public
 * context and optional generic GPTgrep preparation. Gold is in the separate verifier.
 */
export async function writeHarborTasks(options: HarborAdapterOptions): Promise<GeneratedHarborTask[]> {
  if (options.arm !== "bare" && options.arm !== "gptgrep") throw new Error("Unknown experiment arm");
  if (options.prebuiltImage !== undefined && (!validImmutableImage(options.prebuiltImage) || options.image !== undefined)) throw new Error("Prebuilt export requires one immutable image and cannot select a build base");
  const workspace = options.prebuiltImage ? "/app/task" : "/app";
  if (options.arm === "bare" && options.preparedArtifacts?.length) throw new Error("Bare arm cannot receive prepared artifacts");
  if (options.rubricSha256) requireHash(options.rubricSha256, "rubricSha256");
  const tasks = selectReaderTasks(options.release.readerTasks, options);
  if (!tasks.length) throw new Error("No tasks selected");
  const admission = await loadAdmission(options.admission, options.outputDir);
  if (admission.assessment.release.revision !== options.release.revision || digest(admission.assessment.release.fileHashes) !== digest(options.release.fileHashes)) throw new Error("Release differs from assessed source");
  for (const task of tasks) assertTaskAdmitted(admission, task, options.arm, options.rawDocuments);
  await verifyRequiredPreprocessing(admission, tasks, options.preparedArtifacts ?? []);
  const outputRoot = resolve(options.outputDir);
  const sourceRoot = options.release.releaseRoot && resolve(options.release.releaseRoot);
  if (sourceRoot && (outputRoot === sourceRoot || !relative(sourceRoot, outputRoot).startsWith(`..${sep}`) && !isAbsolute(relative(sourceRoot, outputRoot)))) {
    throw new Error("Output must not be placed inside the upstream release");
  }
  const scorerPath = options.release.nativeScorerPath;
  if (!scorerPath || await hashFile(scorerPath) !== XL_NATIVE_SCORER_SHA256) throw new Error("Pinned native scorer is required");
  const raw = new Map(options.rawDocuments.map((d) => [d.documentId, d]));
  if (raw.size !== options.rawDocuments.length) throw new Error("Duplicate raw document ID");
  const sources = new Map(options.release.documents.map((d) => [d.documentId, d]));
  const neededIds = [...new Set(tasks.flatMap((t) => t.contextDocumentIds))];
  for (const id of neededIds) {
    const artifact = raw.get(id);
    if (!artifact) throw new Error(`Raw document unavailable: ${id}`);
    requireHash(artifact.sha256, id);
    if (await hashFile(artifact.path) !== artifact.sha256.toLowerCase()) throw new Error(`Raw document digest mismatch: ${id}`);
    if (artifact.pageCount !== undefined && artifact.pageCount !== sources.get(id)?.pageCount) {
      throw new Error(`Raw PDF page count differs from release: ${id}`);
    }
  }
  const prepared = options.preparedArtifacts ?? [];
  if (prepared.some((p) => p.scope !== "tool")) {
    const declared = options.preparedContextDocumentIds;
    if (!declared || new Set(declared).size !== declared.length) throw new Error("Prepared indexes require unique declared context document IDs");
    const sorted = [...declared].sort();
    for (const task of tasks) if (JSON.stringify(sorted) !== JSON.stringify(task.contextDocumentIds)) {
      throw new Error("Prepared index scope differs from the native task context; generate each context separately");
    }
  }
  const destinations = prepared.map((p) => relativeDestination(p.relativePath));
  if (new Set(destinations).size !== destinations.length) throw new Error("Duplicate prepared artifact destination");
  for (const artifact of prepared) {
    if (await hashFile(artifact.path) !== requireHash(artifact.sha256, artifact.relativePath)) throw new Error("Prepared artifact digest mismatch");
  }
  const verifierRoot = resolve(options.verifierRoot ?? DEFAULT_VERIFIER_ROOT);
  const verifierFiles = ["verify.py", "checks.py", "pyproject.toml", "test.sh", "Dockerfile"];
  for (const filename of verifierFiles) await access(resolve(verifierRoot, filename));
  const judgeById = new Map(options.release.judgeTasks.map((t) => [t.id, t]));
  const targets = tasks.map((task) => resolve(outputRoot, harborTaskName(task.id).split("/")[1]!));
  // Preflight every overwrite before touching any existing generated task.
  for (const [i, target] of targets.entries()) {
    if (!await exists(target)) continue;
    if (!options.overwrite) throw new Error(`Task already exists: ${target}`);
    const marker = JSON.parse(await readFile(resolve(target, "adapter-manifest.json"), "utf8"));
    if (marker.schema !== "gptgrep.xl-harbor-task.v1" || marker.task_id !== tasks[i]!.id) {
      throw new Error(`Refusing to overwrite an unowned task directory: ${target}`);
    }
  }
  await mkdir(outputRoot, { recursive: true });
  const generated: GeneratedHarborTask[] = [];
  for (const [index, task] of tasks.entries()) {
    const judge = judgeById.get(task.id);
    if (!judge) throw new Error(`Verifier task unavailable: ${task.id}`);
    const target = targets[index]!;
    const staging = resolve(outputRoot, `.xl-task-${randomUUID()}`);
    const taskRaw = task.contextDocumentIds.map((id) => raw.get(id)!);
    const corpusSha256 = corpusDigest(taskRaw);
    try {
      await mkdir(resolve(staging, "environment/documents"), { recursive: true });
      await mkdir(resolve(staging, "tests"), { recursive: true });
      const context = {
        schema: "gptgrep.xl-reader-context.v1", task_id: task.id, context_id: task.contextId, corpus_sha256: corpusSha256,
        documents: taskRaw.map((d) => ({ document_id: d.documentId,
          path: `${workspace}/documents/${pdfFilename(d.documentId)}`, sha256: d.sha256.toLowerCase(),
          page_count: sources.get(d.documentId)!.pageCount })),
      };
      for (const document of taskRaw) await copyRegular(document.path, resolve(staging, "environment/documents", pdfFilename(document.documentId)));
      for (const artifact of prepared) await copyRegular(artifact.path, resolve(staging, "environment/prepared", artifact.relativePath));
      for (const filename of verifierFiles) await copyRegular(resolve(verifierRoot, filename), resolve(staging, "tests", filename));
      await copyRegular(scorerPath, resolve(staging, "tests/upstream_evaluate.py"));
      await Promise.all([
        writeFile(resolve(staging, "instruction.md"), taskInstruction(task, workspace)),
        writeFile(resolve(staging, "task.toml"), taskToml(options, task)),
        writeFile(resolve(staging, "environment/Dockerfile"), agentDockerfile(options)),
        writeFile(resolve(staging, "environment/context.json"), JSON.stringify(context, null, 2) + "\n"),
        writeFile(resolve(staging, "tests/gold.jsonl"), JSON.stringify(judge.originalRow) + "\n"),
        writeFile(resolve(staging, "tests/expected.json"), JSON.stringify({ schema: "gptgrep.verifier-binding.v1", status: "awaiting_reader_terminal",
          task_id: task.id, corpus_sha256: corpusSha256, rubric_sha256: options.rubricSha256 ?? null,
          reader_task_sha256: digest(task), judge_reference_sha256: digest(judge.originalRow),
          native_scorer_sha256: XL_NATIVE_SCORER_SHA256 }, null, 2) + "\n"),
        writeFile(resolve(staging, "adapter-manifest.json"), JSON.stringify({ schema: "gptgrep.xl-harbor-task.v1",
          task_id: task.id, source_revision: options.release.revision, source_file_hashes: options.release.fileHashes,
          group_id: task.groupId, context_id: task.contextId, corpus_sha256: corpusSha256, arm: options.arm,
          ...(options.prebuiltImage ? { execution: { kind: "prebuilt-bind", image: options.prebuiltImage, workspace } } : {}),
          availability: { assessmentSha256: admission.acceptance.assessment.sha256, assessmentRevision: admission.assessment.revision,
            generation: admission.assessment.generation, dependencySha256: admission.assessment.dependencySha256,
            profileId: options.admission.profileId, profileSha256: digest(admission.assessment.profiles.find(p => p.id === options.admission.profileId)),
            taskDependencySha256: digest(dependencyProjection(task)) },
          prepared_artifacts: prepared.map((p) => ({ relative_path: p.relativePath, sha256: p.sha256, scope: p.scope ?? "context" })),
          prepared_context_document_ids: options.preparedContextDocumentIds ?? [],
          oracle_status: "not_provided", parity_status: "not_run" }, null, 2) + "\n"),
      ]);
      if (await exists(target)) await rm(target, { recursive: true });
      await rename(staging, target);
    } catch (error) {
      await rm(staging, { recursive: true, force: true });
      throw error;
    }
    generated.push({ id: task.id, name: harborTaskName(task.id), directory: target,
      contextId: task.contextId, groupId: task.groupId, corpusSha256, arm: options.arm });
  }
  await writeJson(resolve(outputRoot, "harbor-export.json"), { schema: "gptgrep.harbor-export.v1", status: "generated_pending_acceptance",
    assessmentSha256: admission.acceptance.assessment.sha256, generation: admission.assessment.generation,
    releaseRevision: options.release.revision, profileId: options.admission.profileId, arm: options.arm,
    tasks: await Promise.all(generated.map(async task => ({ taskId: task.id, directory: task.directory,
      files: await indexTaskFiles(task.directory) }))) });
  return generated;
}

/** Controller-only invocation after reader exit and immutable answer collection. */
export function verifyCommand(options: {
  answerPath: string; judgmentPath?: string; expectedPath: string; outputDir: string;
  verifierRoot: string; goldPath?: string; nativeScorerPath?: string;
}): { command: string; args: string[] } {
  // expectedPath is the controller's sealed v2 binding, produced after reader exit.
  const root = resolve(options.verifierRoot);
  const args = ["run", "--project", root, "--no-sync", "--offline", "python", resolve(root, "verify.py"),
    "--answer", resolve(options.answerPath), "--expected", resolve(options.expectedPath),
    "--output-dir", resolve(options.outputDir), "--gold", resolve(options.goldPath ?? resolve(root, "gold.jsonl")),
    "--native-scorer", resolve(options.nativeScorerPath ?? resolve(root, "upstream_evaluate.py")), "--rewardkit"];
  if (options.judgmentPath) args.push("--judgment", resolve(options.judgmentPath));
  return { command: "uv", args };
}
