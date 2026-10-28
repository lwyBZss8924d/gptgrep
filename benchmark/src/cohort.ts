import { corpusDigest, type RawDocument } from "./adapter";
import type { ReaderTask, XlRelease } from "./dataset";
import { digest } from "./io";

export interface FrozenCohort {
  schema: "gptgrep.xl-cohort.v1";
  revision: string;
  split: "dev" | "frozen";
  seed: string;
  sampleSize: number;
  taskIds: string[];
  readerTasks: ReaderTask[];
  documents: RawDocument[];
  corpusSha256: string;
  sourceFileHashes: Record<string, string>;
}

/** Check the frozen content before export or execution; relocation may not change bytes. */
export function validateFrozenCohort(value: unknown, release: XlRelease, available: RawDocument[]): FrozenCohort {
  const cohort = value as FrozenCohort;
  if (!cohort || cohort.schema !== "gptgrep.xl-cohort.v1" || cohort.revision !== release.revision ||
      !["dev", "frozen"].includes(cohort.split) || !cohort.seed || !Array.isArray(cohort.taskIds) ||
      !cohort.taskIds.length || cohort.sampleSize !== cohort.taskIds.length || new Set(cohort.taskIds).size !== cohort.taskIds.length ||
      !Array.isArray(cohort.readerTasks) || !Array.isArray(cohort.documents) || !cohort.sourceFileHashes) throw new Error("A complete frozen cohort is required");
  if (digest(cohort.sourceFileHashes) !== digest(release.fileHashes)) throw new Error("Frozen release file bindings changed");
  const byTask = new Map(release.readerTasks.map(task => [task.id, task]));
  const tasks = cohort.taskIds.map(id => { const task = byTask.get(id); if (!task) throw new Error("Frozen task is absent from the release"); return task; });
  if (digest(cohort.readerTasks) !== digest(tasks)) throw new Error("Frozen reader questions or public contexts changed");
  const ids = [...new Set(tasks.flatMap(task => task.contextDocumentIds))].sort();
  if (digest(cohort.documents.map(doc => doc.documentId).sort()) !== digest(ids)) throw new Error("Frozen document membership changed");
  const byDocument = new Map(available.map(document => [document.documentId, document]));
  const selected = ids.map(id => { const doc = byDocument.get(id); if (!doc) throw new Error("Frozen source bytes are unavailable"); return doc; });
  const identities = (documents: RawDocument[]) => documents.map(({ documentId, sha256, pageCount }) => ({ documentId, sha256, pageCount: pageCount ?? null }))
    .sort((a, b) => a.documentId.localeCompare(b.documentId));
  if (digest(identities(selected)) !== digest(identities(cohort.documents)) ||
      corpusDigest(selected) !== cohort.corpusSha256 || corpusDigest(cohort.documents) !== cohort.corpusSha256) throw new Error("Frozen corpus bytes or physical page counts changed");
  return cohort;
}
