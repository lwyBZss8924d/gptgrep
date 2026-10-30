import { spawn, type ChildProcessByStdio } from "node:child_process";
import { createHash } from "node:crypto";
import { isAbsolute } from "node:path";
import type { Readable } from "node:stream";
import { verifyPin, type Pin } from "./availability";

export interface ProcessCommand {
  executable: Pin;
  /** All scripts, configs and owner storage plans used by argv must be pinned. */
  inputs: Pin[];
  argv: string[];
  cwd: string;
  timeoutMs: number;
  maxOutputBytes: number;
}
export interface ProcessObservation {
  startedAt: string; completedAt: string; latencyMs: number;
  started: boolean; pid: number | null; executionError: string | null;
  exitCode: number | null; signal: string | null; timedOut: boolean; cleanup: boolean; outputExceeded: boolean;
  outputBytes: number; outputSha256: string; stdoutBytes: number; stdoutSha256: string; stderrBytes: number; stderrSha256: string;
}
const integer = (value: number, min = 1) => Number.isSafeInteger(value) && value >= min;

/** Prelaunch pin failures throw; every attempted spawn returns its observed transport, including errors. */
export async function executeOwnerCommand(command: ProcessCommand, deadlineMs: number, options: { environment?: Record<string, string>; inheritEnvironment?: boolean } = {}): Promise<ProcessObservation> {
  await verifyPin(command.executable); for (const pin of command.inputs) await verifyPin(pin);
  if (!isAbsolute(command.cwd) || !integer(command.timeoutMs) || command.timeoutMs + 2000 > deadlineMs || !integer(command.maxOutputBytes) || command.maxOutputBytes > 8 * 1024 * 1024 || command.argv.some(arg => typeof arg !== "string" || arg.includes("\0"))) throw new Error("Unbounded child command or cleanup grace outside its admission");
  const startedAt = new Date().toISOString(), started = Date.now();
  const output = createHash("sha256"), stdout = createHash("sha256"), stderr = createHash("sha256");
  let outputBytes = 0, stdoutBytes = 0, stderrBytes = 0, timedOut = false, outputExceeded = false;
  let executionError: string | null = null, exitCode: number | null = null, signal: string | null = null;
  let child: ChildProcessByStdio<null, Readable, Readable> | null = null, cleanup = true;
  const failure = (error: unknown) => { executionError ??= error instanceof Error ? error.message : String(error); };
  const kill = (value: NodeJS.Signals) => {
    if (child?.pid) try { process.kill(-child.pid, value); } catch (error: any) { if (error.code !== "ESRCH") { failure(error); cleanup = false; } }
  };
  if (options.environment && Object.entries(options.environment).some(([key, value]) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(key) || typeof value !== "string" || value.includes("\0"))) throw new Error("Invalid explicit child environment");
  try { child = spawn(command.executable.path, command.argv, { cwd: command.cwd, env: options.inheritEnvironment === false ? { ...options.environment } : options.environment ? { ...process.env, ...options.environment } : undefined,
    shell: false, detached: true, stdio: ["ignore", "pipe", "pipe"] }); }
  catch (error) { failure(error); }
  if (child) {
    const active = child;
    active.stdout.on("data", bytes => { stdout.update(bytes); stdoutBytes += bytes.length; });
    active.stderr.on("data", bytes => { stderr.update(bytes); stderrBytes += bytes.length; });
    for (const stream of [active.stdout, active.stderr]) stream.on("data", bytes => {
      output.update(bytes); outputBytes += bytes.length;
      if (outputBytes > command.maxOutputBytes) { outputExceeded = true; kill("SIGKILL"); }
    });
    const timer = setTimeout(() => { timedOut = true; kill("SIGTERM"); }, command.timeoutMs);
    // Reserve the second half of the cleanup grace for reaping and pipe closure.
    const force = setTimeout(() => kill("SIGKILL"), command.timeoutMs + 1000);
    let watchdog: ReturnType<typeof setTimeout> | undefined;
    try {
      await new Promise<void>(done => {
        watchdog = setTimeout(() => {
          timedOut = true; cleanup = false;
          failure(new Error("Owner command cleanup deadline expired before stdio closure; descendant cleanup is unresolved"));
          kill("SIGKILL");
          // A descendant can leave the original process group while retaining
          // these pipes. Stop waiting without claiming that descendant is dead.
          active.stdout.destroy(); active.stderr.destroy(); active.unref(); done();
        }, command.timeoutMs + 2000);
        active.once("error", error => { failure(error); if (active.pid) kill("SIGKILL"); else done(); });
        active.once("exit", (code, sig) => { exitCode = code; signal = sig; });
        active.once("close", (code, sig) => { exitCode = code; signal = sig; done(); });
      });
    } finally { clearTimeout(timer); clearTimeout(force); clearTimeout(watchdog); kill("SIGKILL"); }
    if (active.pid) try { process.kill(-active.pid, 0); cleanup = false; }
    catch (error: any) { if (error.code !== "ESRCH") { failure(error); cleanup = false; } }
  }
  return { startedAt, completedAt: new Date().toISOString(), latencyMs: Date.now() - started,
    started: Boolean(child?.pid), pid: child?.pid ?? null, executionError, exitCode, signal, timedOut, cleanup, outputExceeded,
    outputBytes, outputSha256: output.digest("hex"), stdoutBytes, stdoutSha256: stdout.digest("hex"), stderrBytes, stderrSha256: stderr.digest("hex") };
}
