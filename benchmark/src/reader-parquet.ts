import { execFile } from "node:child_process";
import { lstat, readFile, realpath } from "node:fs/promises";
import { basename, isAbsolute } from "node:path";
import { promisify } from "node:util";
import { canonical, fileHash, physicalPath, readJson, requireStorage } from "./io";
import { verifyPin, type Pin } from "./availability";
import { validatePublicRows, type DecodedParquet, type ParquetDecoder, type PublicRowKind } from "./reader-publication";

export interface ParquetRuntime {
  pythonExecutable: string;
  pythonRealPath: string;
  pythonSha256: string;
  runtimeReceipt: Pin;
  adapter: Pin;
  project: Pin;
  lock: Pin;
}

const execute = promisify(execFile);
const MAX_BYTES = 64 * 1024 * 1024;

async function checked(runtime: ParquetRuntime): Promise<void> {
  await requireStorage();
  for (const pin of [runtime.runtimeReceipt, runtime.adapter, runtime.project, runtime.lock]) await verifyPin(pin);
  // A venv executable may be a symlink, but its exact lexical and physical paths are frozen.
  if (!isAbsolute(runtime.pythonExecutable) || !isAbsolute(runtime.pythonRealPath) ||
      await realpath(runtime.pythonExecutable) !== runtime.pythonRealPath || await fileHash(runtime.pythonRealPath) !== runtime.pythonSha256) throw new Error("Parquet interpreter identity changed");
  const receipt = await readJson(runtime.runtimeReceipt.path);
  if (receipt.schema !== "gptgrep.pyarrow-runtime.v1" || receipt.pyarrowVersion !== "26.0.0" ||
      receipt.pythonExecutable !== runtime.pythonExecutable || receipt.pythonRealPath !== runtime.pythonRealPath || receipt.pythonSha256 !== runtime.pythonSha256) throw new Error("Parquet runtime receipt does not bind this interpreter");
}

async function invoke(runtime: ParquetRuntime, command: "write" | "decode", kind: PublicRowKind, input: string, output?: string): Promise<any> {
  await checked(runtime); await physicalPath(input);
  const info = await lstat(input);
  if (!info.isFile() || info.size > MAX_BYTES) throw new Error("Bounded regular Parquet input required");
  const argv = ["-I", "-B", runtime.adapter.path, command, "--runtime-receipt", runtime.runtimeReceipt.path, "--kind", kind, "--input", input];
  if (output) { await requireStorage(output); await physicalPath(output); argv.push("--output", output); }
  let stdout: string;
  try {
    ({ stdout } = await execute(runtime.pythonExecutable, argv, { encoding: "utf8", timeout: 120_000, maxBuffer: MAX_BYTES,
      env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" } }));
  } catch (error: any) {
    // The decoder's structured code contains no rows, metadata values or private paths.
    let code = "process_failed";
    try { const value = JSON.parse(String(error.stderr)); if (value.status === "failed" && /^[A-Za-z0-9_]{1,100}$/.test(value.code)) code = value.code; } catch { /* retain generic code */ }
    throw new Error(`Parquet adapter failed: ${code}`);
  }
  const value = JSON.parse(stdout);
  await checked(runtime);
  return value;
}

export async function writeReaderParquet(runtime: ParquetRuntime, jsonl: string, output: string, kind: PublicRowKind): Promise<{ rows: number; sha256: string; bytes: number }> {
  await physicalPath(jsonl);
  const info = await lstat(jsonl);
  if (!info.isFile() || info.size > MAX_BYTES) throw new Error("Bounded regular JSONL input required");
  const source = await readFile(jsonl, "utf8");
  if (Buffer.byteLength(source) > MAX_BYTES) throw new Error("Bounded JSONL input required");
  const rows = source ? source.trimEnd().split("\n").map(line => JSON.parse(line)) : [];
  const projected = validatePublicRows(rows, kind);
  if (source !== projected.map(row => canonical(row)).join("\n") + (projected.length ? "\n" : "")) throw new Error("Canonical public JSONL required before Parquet write");
  const result = await invoke(runtime, "write", kind, jsonl, output);
  if (result.schema !== "gptgrep.parquet-write.v1" || result.status !== "written_and_decoded" || result.rows !== projected.length ||
      result.sha256 !== await fileHash(output) || result.bytes !== (await lstat(output)).size) throw new Error("Parquet write receipt mismatch");
  return { rows: result.rows, sha256: result.sha256, bytes: result.bytes };
}

export async function decodeReaderParquet(runtime: ParquetRuntime, path: string, kind: PublicRowKind): Promise<DecodedParquet> {
  return await invoke(runtime, "decode", kind, path) as DecodedParquet;
}

export function createParquetDecoder(runtime: ParquetRuntime): ParquetDecoder {
  return async path => {
    const names: Record<string, PublicRowKind> = { "documents.parquet": "document", "qa_single_doc.parquet": "single_doc", "qa_cross_doc.parquet": "cross_doc" };
    const kind = names[basename(path)];
    if (!kind) throw new Error("Unknown public Parquet partition");
    return await decodeReaderParquet(runtime, path, kind);
  };
}
