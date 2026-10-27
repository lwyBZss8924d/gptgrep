import { runActorAction } from "./actor-runtime";
import { readFile } from "node:fs/promises";
if (process.argv.length !== 2) throw new Error("Supply one closed native actor action JSON object on stdin");
try {
  const input = await readFile(0, "utf8");
  if (Buffer.byteLength(input) > 16384) throw new Error("Native actor request exceeds 16 KiB");
  process.stdout.write(`${JSON.stringify({ terminal: await runActorAction(JSON.parse(input)) })}\n`);
} catch (error) {
  process.stderr.write(`${JSON.stringify({ error: error instanceof Error ? error.message : String(error) })}\n`);
  process.exitCode = 1;
}
