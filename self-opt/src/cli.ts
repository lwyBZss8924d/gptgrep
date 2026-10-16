import { readJson, requireStorage, writeJson } from "../../benchmark/src/io";
import { compare, planLoop, summarize, type EvaluationRun } from "./evaluate";
import { assertTaskAdmitted, availabilityCoverage, loadAdmission, readExactJson, verifyPin, verifyPreprocessingPins, type AvailabilityAssessment } from "../../benchmark/src/availability";

const [command, ...args] = process.argv.slice(2);
const values = new Map<string, string>();
for (let i = 0; i < args.length; i += 2) {
  if (!args[i]?.startsWith("--") || args[i + 1] === undefined) throw new Error("Expected --key value arguments");
  if (values.has(args[i]!.slice(2))) throw new Error("Duplicate option");
  values.set(args[i]!.slice(2), args[i + 1]!);
}
const get = (key: string) => { const value = values.get(key); if (!value) throw new Error(`Missing --${key}`); return value; };
if (command === "--help" || command === "help" || !command) {
  console.log("summarize --run JSON --output JSON | compare --incumbent JSON --candidate JSON --output JSON [--minimum-improvement N] | coverage --assessment JSON --assessment-sha256 SHA --enrolled JSON_ARRAY --output JSON | plan-loop --parent JSON --baseline REV --iteration N --paths JSON_ARRAY --output JSON --acceptance JSON --acceptance-sha256 SHA --profile ID");
} else {
  const output = get("output"); await requireStorage(output);
  let result: unknown;
  if (command === "summarize") result = summarize(await readJson<EvaluationRun>(get("run")));
  else if (command === "compare") result = compare(await readJson(get("incumbent")), await readJson(get("candidate")), {
    minimumImprovement: Number(values.get("minimum-improvement") ?? 0),
    ...(values.has("max-cost-ratio") ? { maxCostRatio: Number(get("max-cost-ratio")) } : {}),
  });
  else if (command === "coverage") {
    await verifyPin({ path: get("assessment"), sha256: get("assessment-sha256") });
    result = availabilityCoverage(await readExactJson<AvailabilityAssessment>(get("assessment")), JSON.parse(get("enrolled")));
  } else if (command === "plan-loop") {
    const parent = await readJson<EvaluationRun>(get("parent"));
    const plan = planLoop({ parent, baselineRevision: get("baseline"), iteration: Number(get("iteration")), mutationPaths: JSON.parse(get("paths")) });
    const admission = await loadAdmission({ acceptance: { path: get("acceptance"), sha256: get("acceptance-sha256") }, profileId: get("profile") }, output);
    if (parent.scenario.datasetRevision !== admission.assessment.release.revision) throw new Error("Prospective Opt release differs from historical parent");
    const profile = admission.assessment.profiles.find(p => p.id === get("profile"))!;
    for (const id of parent.cohort) {
      const task = admission.assessment.release.tasks.find(t => t.id === id);
      if (!task) throw new Error("Prospective Opt task absent from current release");
      assertTaskAdmitted(admission, task, profile.arm, admission.assessment.documents.filter(d => d.rawSha256 !== null).map(d => ({ documentId: d.documentId, sha256: d.rawSha256! })));
    }
    await verifyPreprocessingPins(admission, parent.cohort.map(id => admission.assessment.release.tasks.find(t => t.id === id)!));
    result = { ...plan, currentAvailability: { assessmentSha256: admission.acceptance.assessment.sha256,
      generation: admission.assessment.generation, profileId: get("profile"), coverage: availabilityCoverage(admission.assessment, parent.cohort) } };
  }
  else throw new Error(`Unknown command: ${command}`);
  await writeJson(output, result); console.log(JSON.stringify(result));
}
