import { execFile } from "node:child_process";
import { lstat, mkdir, realpath, writeFile } from "node:fs/promises";
import { isAbsolute, join } from "node:path";
import { parseArgs, promisify } from "node:util";
import { canonical, fileHash, physicalPath, readJson, requireStorage, writeJson } from "../src/io";
import { verifyPin } from "../src/availability";
import { createParquetDecoder, writeReaderParquet, type ParquetRuntime } from "../src/reader-parquet";
import { verifyPublicStaging, type PublicRowKind, type PublicStagingManifest } from "../src/reader-publication";

const execute = promisify(execFile);

/** Engineering format qualification only; these fixtures are not evaluation tasks. */
async function main(): Promise<void> {
  const { values } = parseArgs({ options: { python: { type: "string" }, output: { type: "string" } }, strict: true });
  if (!values.python || !values.output || !isAbsolute(values.python) || !isAbsolute(values.output)) throw new Error("Explicit absolute interpreter and output required");
  const output = values.output;
  const storage = await requireStorage(output);
  await physicalPath(output);
  await mkdir(output);
  const sourceRoot = import.meta.dir;
  const adapter = join(sourceRoot, "reader_parquet.py"), runtimePath = join(output, "runtime.json");
  const pin = async (path: string) => ({ path, sha256: await fileHash(path) });
  const sourcePins = await Promise.all(["qualify.ts", "reader_parquet.py", "test_reader_parquet.py", "pyproject.toml", "uv.lock",
    "../src/reader-parquet.ts", "../src/reader-publication.ts", "../src/io.ts", "../src/availability.ts", "../src/source-verification.ts"].map(path => pin(join(sourceRoot, path))));
  const interpreter = await pin(await realpath(values.python));
  const commandOptions = { encoding: "utf8" as const, timeout: 120_000, maxBuffer: 1024 * 1024,
    env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" } };
  const runtimeResult = await execute(values.python, ["-I", "-B", adapter, "runtime", "--output", runtimePath], commandOptions);
  const identity = await readJson(runtimePath), summary = JSON.parse(runtimeResult.stdout);
  if (summary.schema !== "gptgrep.pyarrow-runtime-written.v1" || summary.sha256 !== await fileHash(runtimePath) ||
      identity.pythonExecutable !== values.python || identity.pythonRealPath !== interpreter.path || identity.pythonSha256 !== interpreter.sha256) throw new Error("Runtime qualification identity mismatch");
  const runtimeReceipt = await pin(runtimePath);
  let tests;
  try {
    const result = await execute(values.python, ["-I", "-B", join(sourceRoot, "test_reader_parquet.py")], commandOptions);
    tests = { status: "passed", stdout: result.stdout, stderr: result.stderr };
  } catch (error: any) {
    await writeJson(join(output, "tests.json"), { status: "failed", stdout: String(error.stdout ?? ""), stderr: String(error.stderr ?? "") });
    throw new Error("Parquet qualification tests failed");
  }
  await writeJson(join(output, "tests.json"), tests);
  const sourcePin = (name: string) => sourcePins.find(item => item.path === join(sourceRoot, name))!;
  const runtime: ParquetRuntime = { pythonExecutable: values.python, pythonRealPath: identity.pythonRealPath,
    pythonSha256: identity.pythonSha256, runtimeReceipt, adapter: sourcePin("reader_parquet.py"),
    project: sourcePin("pyproject.toml"), lock: sourcePin("uv.lock") };
  await writeJson(join(output, "runtime-config.json"), runtime);
  const runtimeConfig = await pin(join(output, "runtime-config.json"));
  const root = join(output, "public");
  await mkdir(join(root, "data"), { recursive: true });
  const document = (id: string) => ({ documentId: id, title: null, url: `https://example.invalid/${id}.pdf`, sourceHost: "example.invalid",
    pageCount: 1, fileSizeBytes: 10, domain: "synthetic", docType: "report", pdfType: "text" });
  const common = { contextId: "synthetic-context", groupId: "synthetic-group", question: "Unicode 漢字 😀 e\u0301 separator\u2028 / slash" };
  const partitions: { stem: string; kind: PublicRowKind; rows: unknown[] }[] = [
    { stem: "documents", kind: "document", rows: [document("synthetic-one"), document("synthetic-two")] },
    { stem: "qa_single_doc", kind: "single_doc", rows: [{ ...common, id: "synthetic-single", taskType: "single_doc", contextDocumentIds: ["synthetic-one"] }] },
    { stem: "qa_cross_doc", kind: "cross_doc", rows: [{ ...common, id: "synthetic-cross", taskType: "cross_doc", contextDocumentIds: ["synthetic-one", "synthetic-two"] }] },
  ];
  const manifest: PublicStagingManifest = { schema: "gptgrep.public-reader-staging.v1", releaseRevision: "0".repeat(40), files: [] };
  for (const partition of partitions) {
    const jsonl = join(root, "data", `${partition.stem}.jsonl`), parquet = join(root, "data", `${partition.stem}.parquet`);
    await writeFile(jsonl, partition.rows.map(canonical).join("\n") + "\n", { flag: "wx", mode: 0o600 });
    await writeReaderParquet(runtime, jsonl, parquet, partition.kind);
    for (const [format, path] of [["jsonl", jsonl], ["parquet", parquet]] as const) {
      manifest.files.push({ path: `data/${partition.stem}.${format}`, format, rowKind: partition.kind,
        sha256: await fileHash(path), bytes: (await lstat(path)).size });
    }
  }
  const verified = await verifyPublicStaging(root, manifest, createParquetDecoder(runtime));
  await writeJson(join(output, "public-manifest.json"), manifest);
  for (const item of [...sourcePins, interpreter, runtimeReceipt, runtimeConfig]) await verifyPin(item);
  if (await realpath(values.python) !== interpreter.path) throw new Error("Interpreter link changed during qualification");
  const result = { schema: "gptgrep.parquet-qualification.v1", status: "passed", syntheticFormatFixturesOnly: true,
    ...verified, runtime, runtimeConfig, storage, tests: await pin(join(output, "tests.json")),
    publicManifest: await pin(join(output, "public-manifest.json")),
    sourcePins };
  await writeJson(join(output, "qualification.json"), result);
  console.log(JSON.stringify({ status: result.status, files: verified.files, jsonlRows: verified.jsonlRows, parquetRows: verified.parquetRows,
    pyarrowVersion: identity.pyarrowVersion, packageFiles: identity.packageFiles, qualificationSha256: await fileHash(join(output, "qualification.json")) }));
}

await main();
