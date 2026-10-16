# XL-DocBench source and adapter contract

The public benchmark kit contains reusable code and format documentation. Acquired
PDFs, QA rows, generated tasks, native gold, source download receipts, parsed/OCR
artifacts and run evidence belong in externally configured local storage. Dataset
acquisition and Colab/MinerU preparation are local data-engineering work and are
not part of this public kit.

## Pinned source

| Source | Revision / identity |
| --- | --- |
| [Microsoft XL-DocBench release](https://huggingface.co/datasets/microsoft/XL-DocBench/tree/72954bd70ffffe230f08b57c57fa9274ec14d7ea) | `72954bd70ffffe230f08b57c57fa9274ec14d7ea` |
| Release manifest | `xldocbench_strict_1345_v1`: 292 documents, 1,345 questions; 1,191 single-document and 154 cross-document |
| Native `code/evaluate.py` SHA-256 | `efc56cb25ba136e09ceaccb5d8006ea9461f71a68998f2987073387682a5e547` |
| [Harbor source](https://github.com/harbor-framework/harbor/tree/f527f8e447038fd242d08ecfa8aaa8029100da97) | `f527f8e447038fd242d08ecfa8aaa8029100da97`; task schema 1.4 |
| RewardKit dependency | `harbor-rewardkit==0.2.3`, managed with uv |

The [XL paper](https://arxiv.org/html/2608.00036v1) describes an earlier cohort.
The current conservative release excludes exact source URLs marked RAG Not
Approved and questions referencing them. Its manifest records 13 scored systems.
The [DocAtlas project's 27-system, three-benchmark table](https://officeintelligence.github.io/docatlas/)
covers MMLongBench-Doc, FinRAGBench-V and LongDocURL; it is not the current XL
leaderboard. Local GPTgrep ablations do not establish parity with either table.

The release card declares `license: other`. PDFs are referenced by URL, not
redistributed; original publishers retain their rights. The conservative filter
does not grant a general document redistribution license. Preserve acquisition
provenance and original terms with local source material.

## Required locally acquired layout

```text
release-root/
  manifest.json
  data/documents.jsonl
  data/qa_single_doc.jsonl
  data/qa_cross_doc.jsonl
  code/evaluate.py
```

The loader performs no network requests. It verifies cohort counts and the native
scorer digest, and records hashes of every loaded file. The caller must establish
the HF revision during acquisition; the loader explicitly labels that revision
as caller-asserted rather than claiming that a matching scorer proves QA identity.
Published score files and example gold are unnecessary runtime inputs.

## Reader context and verifier material

The official [`code/quickstart.py`](https://huggingface.co/datasets/microsoft/XL-DocBench/blob/main/code/quickstart.py)
defines `row_documents` and checks `metadata.n_context_documents` independently
from `metadata.n_evidence_documents`. For single-document tasks, project
`document.document_id`. For cross-document tasks, project **every**
`documents[].document_id`, including documents with no annotated evidence. Join
these IDs against `data/documents.jsonl` for source URL and file metadata.

`projectReaderTask` does not inspect answer, evidence, answerability or native
system scores. Its output contains only task ID, original question, task type,
complete context document IDs, context identity and source-group identity. Full
source rows are retained separately as `JudgeTask.originalRow`. Never serialize
the whole `XlRelease` into a reader workspace or an indexing input.

Original PDF files retain all pages. A page subset, a support-only document bundle
or an expanded corpus is a different task condition. Give such a variant its own
identity and revalidate answerability instead of reporting it as native XL.

## Development and frozen source groups

`buildSourceGroups` unites documents linked by any complete public task context,
the same canonical URL, the same acquired raw-file hash, or reviewed explicit
family membership. It takes connected components, so overlapping cross-document
series stay together even when their document lists differ. Revisions, editions,
translations and near-duplicates require reviewed family membership when exact
URLs and hashes do not capture their relationship. Publisher or domain equality
alone is insufficient to identify a family.

`splitBySourceGroups` uses a fixed seed and separates entire components. Explicit
exposure always moves a full component into development, even if that exceeds
the requested development size. All related single-document tasks move with it.
The small development selection is not a significance or release threshold.
Freeze task IDs before comparing arms; changing selection creates a new cohort.

## Harbor generation

`writeHarborTasks` provides output-directory, task-ID, limit and overwrite controls.
Task names combine a sanitized source ID with a deterministic hash. Overwrite
requires the existing directory's adapter ownership marker.

Generated tasks use `schema_version = "1.4"`, a stable `[task].name`, an explicit
answer artifact, a reader-only environment, and a separate verifier. Both arms
receive identical question and output instructions. Bare receives original PDFs
only; GPTgrep may receive generic tools and raw-document preparation scoped to
the same complete context. An index bundle declares its document IDs so a full
corpus index cannot accidentally widen a native context. Agent network defaults
to offline; the controller supplies the model/auth endpoint allowlist when the
selected provider supports that policy. Verifier computation is offline.

The [Harbor adapter guide](https://github.com/harbor-framework/harbor/blob/f527f8e447038fd242d08ecfa8aaa8029100da97/adapters/ADAPTER_CONTRIBUTING.md)
also specifies upstream contribution, oracle, parity, registration and publication
steps. This kit does not modify or submit to Harbor. Its generated metadata says
`oracle_status: not_provided` and `parity_status: not_run`; source compatibility
does not claim an oracle pass, published registry entry or upstream parity.
The guide retains older task-version examples; the current task model and
create-adapter Skill specify schema 1.4 separately from package version.

## Native score and independent grounding

The [released evaluator](https://huggingface.co/datasets/microsoft/XL-DocBench/blob/main/code/evaluate.py)
is loaded only in the verifier. Native relaxed accuracy, token F1 and ANLS remain
separate metrics. Missing and failed reader predictions stay in the enrolled
denominator; `ignore_missing` is always false. A broken scorer is unknown, not a
measured quality zero. The native scorer uses relaxed numeric/string matching
and does not grade predicted citations; the independent judgment covers that gap.

The controller runs the independent EvalsAgent once and provides
`gptgrep.grounding-judgment.v1`. The verifier checks task ID and exact answer,
corpus and rubric hashes. The answer hash covers the entire collected JSON file,
including citations. The reader must have exited before the controller freezes
its answer; the reader must never write the judgment or expected bindings.

RewardKit reads validated feedback as one criterion file per metric. There is no
default cross-metric weighted mean. Incomplete or missing grounding leaves
`grounded_success` and `atomic_coverage` absent from numeric rewards and records
`status: unknown`, with `promotion_allowed: false`. Completed grounding can
legitimately score zero. `promotion_allowed` only means evidence is complete;
the outer optimization policy still decides whether measured improvement exists.

Native scoring itself makes no model call. Synthetic contract tests verify
projection, grouping and judgment binding; they are not live benchmark evidence.
