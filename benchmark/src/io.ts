import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, mkdir, readFile, realpath, rename, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";

export function canonical(value: unknown): string {
  if (value === null || typeof value === "string" || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "number" && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value === "object") return `{${Object.entries(value as object).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  throw new Error("Canonical JSON requires finite, defined JSON values");
}

export function digest(value: unknown): string { return sha256(canonical(value)); }
export function sha256(value: string | Uint8Array): string { return createHash("sha256").update(value).digest("hex"); }
export async function fileHash(path: string): Promise<string> {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}

export function within(root: string, path: string): boolean {
  const r = relative(resolve(root), resolve(path));
  return r === "" || (!r.startsWith(`..${sep}`) && r !== ".." && !isAbsolute(r));
}

export async function physicalPath(path: string): Promise<string> {
  path = resolve(path);
  let cursor = path;
  while (true) {
    try {
      const info = await lstat(cursor);
      if (info.isSymbolicLink()) throw new Error(`Symlink is not an admitted output path: ${cursor}`);
    } catch (error: any) { if (error.code !== "ENOENT") throw error; }
    if (dirname(cursor) === cursor) break;
    cursor = dirname(cursor);
  }
  return path;
}

export async function writeJson(path: string, value: unknown): Promise<void> {
  await physicalPath(path);
  await mkdir(dirname(path), { recursive: true });
  const temp = `${path}.${process.pid}.${crypto.randomUUID()}.tmp`;
  await writeFile(temp, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: "wx" });
  await rename(temp, path);
}

export async function readJson<T = any>(path: string): Promise<T> {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || info.size > 64 * 1024 * 1024) throw new Error("Expected a bounded regular JSON file");
  return JSON.parse(await readFile(path, "utf8")) as T;
}

/** The owner launcher is the authority. This verifies the inherited bytes and placement. */
export async function requireStorage(...outputs: string[]): Promise<{ binding: string; sha256: string }> {
  const binding = process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING;
  const expected = process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST;
  if (!binding || !expected || await fileHash(binding) !== expected) throw new Error("An exact owner-admitted external storage run is required");
  const value = await readJson(binding);
  const targets = value.targets ?? value.plan?.targets;
  if (!targets) throw new Error("Storage binding has no verified targets");
  const roots = [targets.output, targets.downloads, targets.test_data].filter((v): v is string => typeof v === "string");
  for (const output of outputs) {
    await physicalPath(output);
    if (!roots.some(root => within(root, output))) throw new Error(`Output lies outside the admitted run: ${output}`);
  }
  return { binding: await realpath(binding), sha256: expected };
}

export function boundedInteger(value: number, min: number, max: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < min || value > max) throw new Error(`Invalid ${label}`);
  return value;
}
