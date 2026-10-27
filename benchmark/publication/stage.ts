import { constants } from "node:fs";
import { copyFile, lstat, mkdir } from "node:fs/promises";
import { dirname, isAbsolute, join } from "node:path";
import { parseArgs } from "node:util";
import { verifyPin, type Pin } from "../src/availability";
import { fileHash, readJson, requireStorage, writeJson } from "../src/io";
import { createParquetDecoder, writeReaderParquet, type ParquetRuntime } from "../src/reader-parquet";
import { verifyPublicStaging, type PublicStagingManifest } from "../src/reader-publication";

/** Creates local, decoded publication staging; it performs no remote publication. */
async function main(): Promise<void> {
  const { values } = parseArgs({ options: {
    qualification: { type: "string" }, "qualification-sha256": { type: "string" },
    "source-root": { type: "string" }, "source-manifest": { type: "string" }, "source-manifest-sha256": { type: "string" },
    output: { type: "string" },
  }, strict: true });
  if (["qualification", "source-root", "source-manifest", "output"].some(key => !values[key] || !isAbsolute(values[key]!)) ||
      ["qualification-sha256", "source-manifest-sha256"].some(key => !/^[a-f0-9]{64}$/.test(values[key] ?? ""))) throw new Error("Explicit qualified runtime, source manifest pins and output required");
  const qualificationPin: Pin = { path: values.qualification!, sha256: values["qualification-sha256"]! };
  const manifestPin: Pin = { path: values["source-manifest"]!, sha256: values["source-manifest-sha256"]! };
  const selfPin: Pin = { path: import.meta.path, sha256: await fileHash(import.meta.path) };
  const storage = await requireStorage(values.output!);
  await verifyPin(qualificationPin); await verifyPin(manifestPin);
  const qualification = await readJson(qualificationPin.path);
  if (qualification.schema !== "gptgrep.parquet-qualification.v1" || qualification.status !== "passed" ||
      !Array.isArray(qualification.sourcePins)) throw new Error("Passed Parquet qualification required");
  const expectedPaths = ["qualify.ts", "reader_parquet.py", "test_reader_parquet.py", "pyproject.toml", "uv.lock",
    "../src/reader-parquet.ts", "../src/reader-publication.ts", "../src/io.ts", "../src/availability.ts", "../src/source-verification.ts"].map(path => join(import.meta.dir, path));
  const sourcePins: Pin[] = qualification.sourcePins;
  if (sourcePins.length !== expectedPaths.length || new Set(sourcePins.map(pin => pin.path)).size !== expectedPaths.length ||
      expectedPaths.some(path => !sourcePins.some(pin => pin.path === path))) throw new Error("Qualification must bind the executing source closure");
  for (const pin of sourcePins) await verifyPin(pin);
  await verifyPin(qualification.runtimeConfig);
  const runtime: ParquetRuntime = qualification.runtime;
  const source: PublicStagingManifest = await readJson(manifestPin.path);
  if (!Array.isArray(source.files) || source.files.length > 10 || source.files.some(file => !Number.isSafeInteger(file.bytes) || file.bytes < 0) ||
      source.files.reduce((bytes, file) => bytes + file.bytes, 0) > 64 * 1024 * 1024) throw new Error("Bounded public source staging required");
  await verifyPublicStaging(values["source-root"]!, source, null);
  if (source.files.some(file => file.format === "parquet") ||
      ["document", "single_doc", "cross_doc"].some(kind => source.files.filter(file => file.rowKind === kind && file.format === "jsonl").length !== 1)) throw new Error("Exactly three safe JSONL source partitions required");
  await mkdir(values.output!);
  const root = join(values.output!, "public");
  const manifest: PublicStagingManifest = { ...source, files: source.files.map(file => ({ ...file })) };
  for (const entry of source.files) {
    const target = join(root, entry.path);
    await requireStorage(target); await mkdir(dirname(target), { recursive: true });
    await copyFile(join(values["source-root"]!, entry.path), target, constants.COPYFILE_EXCL);
  }
  await verifyPublicStaging(root, manifest, null);
  for (const entry of source.files.filter(file => file.format === "jsonl")) {
    const relative = entry.path.replace(/\.jsonl$/, ".parquet"), target = join(root, relative);
    const written = await writeReaderParquet(runtime, join(root, entry.path), target, entry.rowKind!);
    manifest.files.push({ path: relative, sha256: written.sha256, bytes: (await lstat(target)).size,
      format: "parquet", rowKind: entry.rowKind });
  }
  const verified = await verifyPublicStaging(root, manifest, createParquetDecoder(runtime));
  await verifyPin(qualificationPin); await verifyPin(manifestPin); await verifyPin(selfPin);
  for (const pin of sourcePins) await verifyPin(pin);
  const manifestPath = join(values.output!, "public-manifest.json");
  await writeJson(manifestPath, manifest);
  const receipt = { schema: "gptgrep.parquet-staging.v1", status: "verified_local_staging", ...verified,
    releaseRevision: source.releaseRevision, qualification: qualificationPin, sourceManifest: manifestPin,
    publicManifest: { path: manifestPath, sha256: await fileHash(manifestPath) }, sourcePins: [selfPin, ...sourcePins], storage };
  await writeJson(join(values.output!, "staging.json"), receipt);
  console.log(JSON.stringify({ status: receipt.status, ...verified, manifestSha256: receipt.publicManifest.sha256,
    stagingSha256: await fileHash(join(values.output!, "staging.json")) }));
}

await main();
