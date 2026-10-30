/** Inspect untrusted candidate Git data without loading its executable configuration. */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { constants } from "node:fs";
import { lstat, mkdir, mkdtemp, open, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import { createHash } from "node:crypto";
import { closed } from "./proposals";
import { verifyReceiptPin } from "./actor-guest-contracts";
import { physicalPath, requireStorage, within, type ContainerActorCapability } from "../../benchmark/src/io";
import type { Pin } from "./competition";

const exec = promisify(execFile);
export interface ActorGitRoute { binary: Pin }
export interface ActorGitContext { capability: ContainerActorCapability; git: ActorGitRoute; outputRoot: string; maxEvidenceBytes: number }
const SAFE_CONFIG = "[core]\nrepositoryformatversion = 0\nbare = false\nlogallrefupdates = false\nfsmonitor = false\nhooksPath = /dev/null\nattributesFile = /dev/null\n[gc]\nauto = 0\n[maintenance]\nauto = false\n";
const FORBIDDEN = new Set(["commondir", "gitdir", "config.worktree", "info/grafts", "objects/info/alternates", "objects/info/http-alternates"]);
function safeRelative(path: string): boolean { return /^[A-Za-z0-9_.][A-Za-z0-9_./-]*$/.test(path) && path.split("/").every(part => !["", ".", ".."].includes(part)); }
export function validateActorGitRoute(job: ActorGitContext): void {
  closed(job.git, ["binary"], "pinned actor Git route");
  closed(job.git.binary, ["path", "sha256"], "actor Git binary");
  if (within(job.capability.source_root, job.git.binary.path) || within(job.outputRoot, job.git.binary.path) || job.capability.build_root !== null && within(job.capability.build_root, job.git.binary.path)) throw new Error("Git executable must be pinned outside mutable actor storage");
}
export function validateActorGitConfig(raw: string): void {
  if (Buffer.byteLength(raw) > 65536 || /[\0\r]/.test(raw)) throw new Error("Unsafe actor Git configuration");
  let section = ""; const values = new Map<string, string>();
  for (const line of raw.split("\n")) {
    const text = line.trim(); if (!text || /^[#;]/.test(text)) continue;
    const header = /^\[(core|user|gc|maintenance)\]$/i.exec(text);
    if (header) { section = header[1]!.toLowerCase(); continue; }
    const pair = /^([a-z][a-z0-9]*)\s*=\s*(.+)$/i.exec(text);
    if (!pair || !section) throw new Error("Unsafe actor Git configuration section or key");
    const key = `${section}.${pair[1]!.toLowerCase()}`, value = pair[2]!;
    const bool = new Set(["core.filemode", "core.logallrefupdates", "core.ignorecase", "core.precomposeunicode"]);
    if (values.has(key) || !(bool.has(key) ? ["true", "false"].includes(value) :
      key === "core.repositoryformatversion" ? value === "0" : key === "core.bare" ? value === "false" :
      key === "gc.auto" ? value === "0" : key === "maintenance.auto" ? value === "false" :
      ["user.name", "user.email"].includes(key) && /^[A-Za-z0-9 ._@+<>-]{1,128}$/.test(value))) throw new Error("Unsafe actor Git configuration value");
    values.set(key, value);
  }
  if (values.get("core.repositoryformatversion") !== "0" || values.get("core.bare") !== "false") throw new Error("Actor Git store must declare its ordinary independent repository format");
}
async function boundedFile(path: string, maximum: number): Promise<Buffer> {
  await physicalPath(path); const before = await lstat(path);
  if (!before.isFile() || before.isSymbolicLink() || before.nlink !== 1 || before.size > maximum) throw new Error("Actor Git metadata must be bounded regular independent files");
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const info = await file.stat();
    if (info.dev !== before.dev || info.ino !== before.ino || info.size !== before.size || info.nlink !== 1) throw new Error("Actor Git file generation changed");
    const data = await file.readFile();
    const after = await file.stat();
    if (data.length !== before.size || after.size !== before.size || after.mtimeMs !== before.mtimeMs || after.ctimeMs !== before.ctimeMs) throw new Error("Actor Git data changed while being read");
    return data;
  } finally { await file.close(); }
}
export async function validateInitialActorGitConfig(job: ActorGitContext): Promise<void> {
  const bytes = await boundedFile(join(job.capability.git_directory, "config"), 65536);
  validateActorGitConfig(bytes.toString("utf8"));
}
export async function verifyActorGitStore(job: ActorGitContext): Promise<void> {
  validateActorGitRoute(job); await physicalPath(job.capability.git_directory);
  if (!(await lstat(job.capability.git_directory)).isDirectory()) throw new Error("Actor requires an independent in-checkout Git directory");
  await verifyReceiptPin(job.git.binary, true);
}
/** Only raw Git data enters a new private directory; source config/hooks/attributes never do. */
export async function withActorGit<T>(job: ActorGitContext,
  work: (run: (...args: string[]) => Promise<string>, privateGitDirectory: string) => Promise<T>): Promise<T> {
  await verifyActorGitStore(job); await requireStorage(job.outputRoot); await mkdir(job.outputRoot, { recursive: true, mode: 0o700 });
  const scratch = await mkdtemp(join(job.outputRoot, ".git-inspection-")), rootInfo = await lstat(scratch);
  const budget = Math.min(32 * 1024 * 1024, Math.floor(job.maxEvidenceBytes / 4)); let copied = 0, entries = 0;
  async function walk(root: string): Promise<void> {
    for (const entry of await readdir(root, { withFileTypes: true })) {
      if (++entries > 32768) throw new Error("Actor Git metadata inventory exceeds its bound");
      const source = join(root, entry.name), name = relative(job.capability.git_directory, source);
      if (!safeRelative(name) || FORBIDDEN.has(name) || ["worktrees", "modules"].includes(name.split("/")[0]!) || name.endsWith(".lock")) throw new Error("Actor Git alternates, linked stores or in-flight metadata are forbidden");
      const info = await lstat(source);
      if (info.isSymbolicLink() || !info.isDirectory() && (!info.isFile() || info.nlink !== 1)) throw new Error("Actor Git metadata links or special files are forbidden");
      if (info.isDirectory()) { await walk(source); continue; }
      const selected = name === "HEAD" || name === "index" || name === "shallow" || name === "packed-refs" || name.startsWith("refs/") ||
        /^objects\/[a-f0-9]{2}\/[a-f0-9]{38}$/.test(name) || /^objects\/pack\/pack-[a-f0-9]{40}\.(pack|idx|rev)$/.test(name);
      if (!selected) continue;
      if (copied + info.size > budget) throw new Error("Actor private Git snapshot exceeds its frozen evidence fraction");
      const data = await boundedFile(source, budget - copied); copied += data.length;
      if (["HEAD", "shallow", "packed-refs"].includes(name) || name.startsWith("refs/")) {
        if (data.length > 1024 * 1024 || /[^\x20-\x7e\n\t]/.test(data.toString("utf8"))) throw new Error("Actor Git references must be bounded ASCII data");
      }
      const target = join(scratch, name); await mkdir(dirname(target), { recursive: true, mode: 0o700 }); await writeFile(target, data, { flag: "wx", mode: 0o600 });
    }
  }
  try {
    await walk(job.capability.git_directory);
    await mkdir(join(scratch, "objects"), { recursive: true }); await mkdir(join(scratch, "refs"), { recursive: true });
    await writeFile(join(scratch, "config"), SAFE_CONFIG, { flag: "wx", mode: 0o600 });
    const run = async (...args: string[]): Promise<string> => {
      await verifyReceiptPin(job.git.binary, true);
      const command = [...args];
      if (command[0] === "diff") command.splice(1, 0, "--no-ext-diff", "--no-textconv");
      const result = await exec(job.git.binary.path, ["--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
        "-c", "core.attributesFile=/dev/null", `--git-dir=${scratch}`, `--work-tree=${job.capability.source_root}`, ...command], {
        timeout: 10000, maxBuffer: 8 * 1024 * 1024,
        env: { PATH: "/usr/bin:/bin", LANG: "C", LC_ALL: "C", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_SYSTEM: "/dev/null", GIT_CONFIG_GLOBAL: "/dev/null",
          GIT_CONFIG_COUNT: "0", GIT_TERMINAL_PROMPT: "0", GIT_OPTIONAL_LOCKS: "0", GIT_ATTR_NOSYSTEM: "1", GIT_NO_REPLACE_OBJECTS: "1", GIT_NO_LAZY_FETCH: "1" },
      });
      return result.stdout;
    };
    return await work(run, scratch);
  } finally {
    const current = await lstat(scratch);
    if (current.dev !== rootInfo.dev || current.ino !== rootInfo.ino || !current.isDirectory() || current.isSymbolicLink()) throw new Error("Private Git scratch generation changed; retained for reconciliation");
    await rm(scratch, { recursive: true }); // Only this call's exclusively created private scratch data.
  }
}
export async function actorGit(job: ActorGitContext, ...args: string[]): Promise<string> { return withActorGit(job, run => run(...args)); }
