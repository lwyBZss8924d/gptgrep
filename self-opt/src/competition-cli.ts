import { digest } from "../../benchmark/src/io";
import { CompetitionLedger } from "./competition";
import { closed } from "./proposals";

const FIELDS: Record<string, string[]> = {
  init: ["contract"], snapshot: [], acquire: ["version", "eventId", "holder", "expiresAt"],
  reserve: ["version", "eventId", "fence", "operationId", "purpose", "commandSha256"],
  mark: ["version", "eventId", "fence", "operationId", "status", "externalJobId"],
  reconcile: ["version", "eventId", "fence", "operationId", "terminal"],
  register: ["version", "eventId", "fence", "candidateId", "evidence"],
  qualify: ["version", "eventId", "fence", "candidateId", "evidence"],
  results: ["version", "eventId", "fence", "candidateId", "incumbent", "measured"],
  measurements: ["version", "eventId", "fence", "report"],
  evaluate: ["candidateId"], committed: ["candidateId"], staged: ["candidateId"], stage: ["version", "eventId", "fence", "candidateId"],
  commit: ["version", "eventId", "fence", "candidateId", "decisionSha256"], "assert-paid": [],
};

/** JSON-only local bridge. This command has no native-worker or model launch action. */
export async function competitionCommand(action: string, ledgerPath: string, request: unknown) {
  if (!Object.hasOwn(FIELDS, action)) throw new Error("Unknown competition action");
  closed(request, FIELDS[action]!, "competition request");
  const ledger = await CompetitionLedger.open(ledgerPath, action === "init" ? request.contract : undefined);
  try {
    let result: unknown;
    switch (action) {
      case "init": case "snapshot": result = ledger.snapshot(); break;
      case "acquire": result = ledger.acquire(request.version, request.eventId, request.holder, request.expiresAt); break;
      case "reserve": result = ledger.reserve(request.version, request.eventId, request.fence, request.operationId, request.purpose, request.commandSha256); break;
      case "mark": result = ledger.markOperation(request.version, request.eventId, request.fence, request.operationId, request.status, request.externalJobId); break;
      case "reconcile": result = await ledger.reconcile(request.version, request.eventId, request.fence, request.operationId, request.terminal); break;
      case "register": result = await ledger.register(request.version, request.eventId, request.fence, request.candidateId, request.evidence); break;
      case "qualify": result = await ledger.qualify(request.version, request.eventId, request.fence, request.candidateId, request.evidence); break;
      case "results": result = await ledger.attachResults(request.version, request.eventId, request.fence, request.candidateId, request.incumbent, request.measured); break;
      case "measurements": result = await ledger.attachMeasurements(request.version, request.eventId, request.fence, request.report); break;
      case "evaluate": result = await ledger.evaluate(request.candidateId); break;
      case "committed": result = ledger.committedDecision(request.candidateId); break;
      case "staged": result = ledger.stagedDecision(request.candidateId); break;
      case "stage": result = await ledger.stageDecision(request.version, request.eventId, request.fence, request.candidateId); break;
      case "commit": result = await ledger.commitDecision(request.version, request.eventId, request.fence, request.candidateId, request.decisionSha256); break;
      case "assert-paid": result = ledger.assertPaidExecutionQualified(); break;
    }
    return { schema: "gptgrep.opt-bridge-response.v1", action, stateVersion: ledger.snapshot().version, result, resultSha256: digest(result) };
  } finally { ledger.close(); }
}

if (import.meta.main) {
  const [action, flag, ledgerPath, ...extra] = process.argv.slice(2);
  if (!action || flag !== "--ledger" || !ledgerPath || extra.length) throw new Error("Usage: competition-cli.ts ACTION --ledger ABSOLUTE_SQLITE_PATH; request JSON on stdin");
  let input = "";
  for await (const chunk of process.stdin) { input += chunk.toString(); if (Buffer.byteLength(input) > 1024 * 1024) throw new Error("Competition request exceeds 1 MiB"); }
  console.log(JSON.stringify(await competitionCommand(action, ledgerPath, JSON.parse(input))));
}
