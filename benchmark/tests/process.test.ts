import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { executeOwnerCommand, type ProcessCommand } from "../src/process";
import { digest, fileHash, sha256 } from "../src/io";

async function fixture(source: string) {
  const root = await mkdtemp(join(tmpdir(), "owner-command-contract-")), script = join(root, "child.ts");
  await writeFile(script, source);
  const command: ProcessCommand = { executable: { path: process.argv[0]!, sha256: await fileHash(process.argv[0]!) },
    inputs: [{ path: script, sha256: await fileHash(script) }], argv: [script], cwd: root, timeoutMs: 1000, maxOutputBytes: 1024 };
  return { root, command, dispose: () => rm(root, { recursive: true, force: true }) };
}

test("owner command records independent stream hashes and a business failure without claiming acceptance", async () => {
  const f = await fixture('process.stdout.write("hello"); process.stderr.write("failure detail"); process.exitCode = 2;');
  try {
    const result = await executeOwnerCommand(f.command, 3000);
    expect(result.started).toBe(true); expect(result.executionError).toBeNull(); expect(result.exitCode).toBe(2);
    expect(result.cleanup).toBe(true); expect(result.stdoutBytes).toBe(5); expect(result.stdoutSha256).toBe(sha256("hello"));
    expect(result.stderrSha256).toBe(sha256("failure detail")); expect(result.outputBytes).toBe(19);
  } finally { await f.dispose(); }
});

test("failed spawn retains a factual not-started observation and no fabricated child output", async () => {
  const f = await fixture('throw new Error("must not execute");');
  try {
    const result = await executeOwnerCommand({ ...f.command, cwd: join(f.root, "missing") }, 3000);
    expect(result.started).toBe(false); expect(result.pid).toBeNull(); expect(result.executionError).not.toBeNull();
    expect(result.outputBytes).toBe(0); expect(result.stdoutSha256).toBe(sha256("")); expect(result.cleanup).toBe(true);
  } finally { await f.dispose(); }
});

test("owner command includes termination grace in admission and retains a timed-out process observation", async () => {
  const f = await fixture('setInterval(() => {}, 1000);');
  try {
    await expect(executeOwnerCommand(f.command, 1000)).rejects.toThrow("cleanup grace");
    const result = await executeOwnerCommand({ ...f.command, timeoutMs: 30 }, 2030);
    expect(result.started).toBe(true); expect(result.timedOut).toBe(true); expect(result.cleanup).toBe(true);
    expect(result.exitCode).not.toBe(0); expect(result.latencyMs).toBeLessThan(2030);
  } finally { await f.dispose(); }
});

test("exact command identity retains Bun guard flags omitted from script argv", async () => {
  const ioPath = resolve(import.meta.dir, "../src/io.ts");
  const f = await fixture(`import { processCommandIdentity } from ${JSON.stringify(ioPath)}; process.stdout.write(JSON.stringify(await processCommandIdentity()));`);
  try {
    const config = join(f.root, "empty.bunfig.toml"); await writeFile(config, "# empty fixture configuration\n");
    const argv = ["--no-env-file", "--no-install", `--config=${config}`, ...f.command.argv];
    const result = await executeOwnerCommand({ ...f.command, argv, inputs: [...f.command.inputs,
      { path: config, sha256: await fileHash(config) }, { path: ioPath, sha256: await fileHash(ioPath) }] }, 3000);
    const observed = { argv: [f.command.executable.path, ...argv], argvSha256: digest([f.command.executable.path, ...argv]),
      executable: f.command.executable.path, method: process.platform === "linux" ? "proc-self-cmdline" : "runtime-execArgv" };
    expect(result.exitCode).toBe(0); expect(result.executionError).toBeNull();
    expect(result.stdoutSha256).toBe(sha256(JSON.stringify(observed)));
  } finally { await f.dispose(); }
});

test("owner command stops waiting when an escaped descendant retains its pipes", async () => {
  const f = await fixture(`import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
if (process.argv.includes("--escaped")) setTimeout(() => process.exit(0), 20000);
else {
  const child = spawn(process.execPath, [process.argv[1], "--escaped"], { detached: true, stdio: ["ignore", "inherit", "inherit"] });
  writeFileSync("escaped.pid", String(child.pid));
  setInterval(() => {}, 1000);
}`);
  try {
    const result = await executeOwnerCommand({ ...f.command, timeoutMs: 300 }, 2300);
    expect(result.started).toBe(true); expect(result.timedOut).toBe(true);
    expect(result.cleanup).toBe(false); expect(result.executionError).toContain("descendant cleanup is unresolved");
    expect(result.latencyMs).toBeLessThan(2800);
    const escapedPid = Number(await readFile(join(f.root, "escaped.pid"), "utf8"));
    expect(escapedPid).toBeGreaterThan(0);
    expect(() => process.kill(escapedPid, 0)).not.toThrow();
  } finally {
    try { process.kill(-Number(await readFile(join(f.root, "escaped.pid"), "utf8")), "SIGKILL"); } catch {}
    await f.dispose();
  }
});


test("an explicit isolated environment does not inherit auth, loader or control values", async () => {
  const f = await fixture('process.stdout.write(JSON.stringify({ allowed: process.env.ONLY_ALLOWED, auth: process.env.GPTGREP_AUTH_RUN_ID ?? null, control: process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING ?? null }));');
  const previous = process.env.GPTGREP_AUTH_RUN_ID; process.env.GPTGREP_AUTH_RUN_ID = "synthetic-private-canary";
  try {
    const result = await executeOwnerCommand(f.command, 3000, { environment: { ONLY_ALLOWED: "explicit" }, inheritEnvironment: false });
    expect(result.exitCode).toBe(0); expect(result.stdoutSha256).toBe(sha256(JSON.stringify({ allowed: "explicit", auth: null, control: null })));
  } finally { if (previous === undefined) delete process.env.GPTGREP_AUTH_RUN_ID; else process.env.GPTGREP_AUTH_RUN_ID = previous; await f.dispose(); }
});
