import { readFile } from "node:fs/promises";
import { runContainerActor } from "./container-actor";
const args = process.argv.slice(2);
if (args.length !== 4 || args[0] !== "--config" || args[2] !== "--expect-config-sha256" || !/^[a-f0-9]{64}$/.test(args[3]!)) throw new Error("Supply the exact owner-bound single-role config");
try {
  const result = await runContainerActor({ path: args[1]!, sha256: args[3]! });
  const receipt = JSON.parse(await readFile(result.path, "utf8"));
  process.stdout.write(`${JSON.stringify({ result, status: receipt.status, acceptance: "not_evaluated" })}\n`);
  if (receipt.status === "failed") process.exitCode = 1;
} catch (error) {
  process.stderr.write(`${JSON.stringify({ error: error instanceof Error ? error.message : String(error) })}\n`);
  process.exitCode = 1;
}
