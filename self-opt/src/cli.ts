import { readJson, requireStorage, writeJson } from "../../benchmark/src/io";
import { compare, planLoop, summarize, type EvaluationRun } from "./evaluate";

const [command, ...args] = process.argv.slice(2);
const values = new Map<string, string>();
for (let i = 0; i < args.length; i += 2) {
  if (!args[i]?.startsWith("--") || args[i + 1] === undefined) throw new Error("Expected --key value arguments");
  if (values.has(args[i]!.slice(2))) throw new Error("Duplicate option");
  values.set(args[i]!.slice(2), args[i + 1]!);
}
const get = (key: string) => { const value = values.get(key); if (!value) throw new Error(`Missing --${key}`); return value; };
if (command === "--help" || command === "help" || !command) {
  console.log("summarize --run JSON --output JSON | compare --incumbent JSON --candidate JSON --output JSON [--minimum-improvement N] | plan-loop --parent JSON --baseline REV --iteration N --paths JSON_ARRAY --output JSON");
} else {
  const output = get("output"); await requireStorage(output);
  let result: unknown;
  if (command === "summarize") result = summarize(await readJson<EvaluationRun>(get("run")));
  else if (command === "compare") result = compare(await readJson(get("incumbent")), await readJson(get("candidate")), {
    minimumImprovement: Number(values.get("minimum-improvement") ?? 0),
    ...(values.has("max-cost-ratio") ? { maxCostRatio: Number(get("max-cost-ratio")) } : {}),
  });
  else if (command === "plan-loop") result = planLoop({ parent: await readJson(get("parent")), baselineRevision: get("baseline"), iteration: Number(get("iteration")), mutationPaths: JSON.parse(get("paths")) });
  else throw new Error(`Unknown command: ${command}`);
  await writeJson(output, result); console.log(JSON.stringify(result));
}
