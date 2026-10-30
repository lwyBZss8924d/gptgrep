import { runNativeEngineer } from "./native-engineer";
if (process.argv.length !== 6 || process.argv[2] !== "--launch" || process.argv[4] !== "--expect-sha256") throw new Error("Use --launch ABSOLUTE_JSON --expect-sha256 SHA256");
try {
  const receipt = await runNativeEngineer({ path: process.argv[3]!, sha256: process.argv[5]! });
  process.stdout.write(`${JSON.stringify({ receipt })}\n`);
} catch (error) {
  process.stderr.write(`${JSON.stringify({ error: error instanceof Error ? error.message : String(error) })}\n`);
  process.exitCode = 1;
}
