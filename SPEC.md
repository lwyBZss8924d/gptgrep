# GPTgrep

GPTgrep is a Rust-first, local document retrieval CLI for agents. It combines
trigram-filtered regex verification, document structure, and required Jev routing
and reranking in its default retrieval and reasoning workflows.
The primary agent retains planning and reasoning; the helper returns source evidence.
There is no vector database and no MCP server.
Local Codex workflows default to `gpt-5.6-luna`, reasoning effort `max`, and service
tier `fast`. Account selection belongs to the caller's private runtime environment;
workstation-specific account aliases and routing are not part of the public product.

## User contract

- Index an explicit document directory, preserving exact source hashes and page/line
  coordinates. Plaintext, Markdown and MDX retain source-line identity. Other
  supported formats use LiteParse extraction and page-based citations.
- Provide native grep-style commands, bounded JSON output, an inspectable command
  schema, document trees and selected-node reads for programmatic tool composition.
- Use a pinned tgrep-core package for lexical candidate filtering. Verify actual
  regex matches after filtering, including patterns that cannot use trigrams.
- Use Jev through its typed Decisions API for semantic document routing and
  candidate reranking. Default search, ask and summarize require this stage and
  fail explicitly if its credentials or service are unavailable. Explicit regex
  and lexical primitives remain available locally for exact inspection and
  controlled ablations; they do not constitute the complete GPTgrep workflow.
- Keep exact, lexical and model-assisted results distinguishable. Report budgets,
  truncated candidate coverage, actual provider identity and unavailable metrics.
- Treat documents as data. Do not execute document instructions, start services,
  silently ingest hidden credentials, or send document text to a provider unless
  the caller invokes a documented model-assisted command. Default search, ask and
  summarize are model-assisted; parse, index and explicit regex/lexical are local.

## Initial release acceptance

The first experimental publication requires the long-horizon comparison contract
below: a real, reproducible minimum advantage over PageIndex Flash plus GPT-5.6.
The engineering checks in this section are necessary and do not replace that gate.

1. A real mixed document fixture can be parsed, indexed, searched and cited end to
   end by the release build. Tree spans are valid and deterministic.
2. Indexed regex results agree with direct regex scanning on controlled cases;
   MatchAll, Unicode, literal and case-insensitive queries are covered.
3. A stale or deleted source cannot appear as fresh evidence. A failed rebuild
   cannot replace the current generation. Readers see a complete snapshot.
4. Live default search and local Codex workflows actually execute required Jev
   document routing and candidate reranking, record actual model, request count,
   latency, usage and cost when returned, and preserve failures. Missing data stays
   null. A standalone provider transport smoke cannot satisfy this workflow gate.
5. Offline evals report retrieval quality and citation fidelity on pinned fixtures.
   Source-reported benchmark numbers remain separate from GPTgrep measurements.
6. The optional incur interface is tested as a bounded native-binary wrapper with
   schema discovery and no MCP serving. The native CLI remains independently usable.
7. Source provenance/licenses, independent review, checks, Git/PoUW, local backup
   and GitHub source/release delivery are recorded separately.

## Scope and research boundary

The first release must satisfy its scoped comparative gate; it does not claim
complete PageIndex Flash parity or universally superior RAG. Flash contains a substantial
layout pipeline and optional generative optimization. The initial Rust crates use
LiteParse layout plus documented structural PageIndex rules. Parity and scale
claims require a future comparative corpus and measured acceptance.

The project may evolve autonomously within this direction. New global registrations,
automatic ingestion of private histories, changes to other projects, and broad
model-training/research experiments are outside this repository's routine authority.

## Long-horizon research goal

The research-preview program must demonstrate a useful measured advantage over a
live PageIndex Flash baseline on the pinned PageIndex-OSS-Benchmark tasks. A source
release, passing process, synthetic smoke or GPTgrep-only corpus run does not meet
this goal. Retain the same corpus/task cohort, pinned source revisions, declared prompt
and tool budgets, judge and failure denominators for each comparison. The
benchmark repository's original local-OSS `results.json` is a separately labelled
native reference: PageIndex Flash plus GPT-5.6 Luna/high reports 60/62 and
$0.003607 estimated answering cost per question, excluding its shared one-off
indexing cost. The first research-preview G5 gate requires one predeclared,
independently confirmed complete-cohort GPTgrep configuration using GPT-6
Luna/Fast at xhigh or max effort to achieve at least 61/62 and
strictly less than $0.003607 per question for GPTgrep answering (Codex
reasoning plus Jev) over the same 62 tasks. Judge/evaluation cost is reported
separately. Future GPTgrep candidate runs use GPT-6 Luna/Fast at xhigh or max
for the builder, planner, reader **and judge**. The original PageIndex repository
retains only aggregate `results.json`, not per-task predictions, so its original
60/62 cannot be rescored with the new judge without rerunning the baseline;
disclose this cross-judge limitation. The previously adapted R8 PageIndex
per-task predictions can be independently rejudged as a separate reference,
without reindexing or re-answering PageIndex, but cannot replace the original
aggregate. Compute the G5 Codex dollar-equivalent cost from measured token
usage at the pinned **Standard** API price card, plus observed Jev cost; the
live experiment may request Fast for throughput. Also report an actual-tier
Fast API price-equivalent estimate separately. Neither is a ChatGPT subscription
bill. The original PageIndex run does not declare Fast, so treating its original
litellm estimate as Standard is a disclosed pricing-alignment inference, not
observed provider-tier evidence. Missing answering usage or Jev cost blocks cost
acceptance for chargeable comparison attempts. Exclude only attempts with a
bounded, observed communication/remote-service failure category from the
experimental cost numerator; retain their count, any observed usage, and
unknown actual billing separately. A generic legacy failure is not inferred to
be a communication failure, and task failures stay in the 62-task quality
denominator. Report GPTgrep cold indexing cost separately. The original
PageIndex benchmark's `documents.json` has known indexing cost for 28 of 34
PDFs; its $1.619953 is an incomplete subtotal, not a complete baseline index
cost or a zero for the remaining six. Our
qualified R8 live SDK/Codex adapter also reports 60/62 with Luna/max reader and
retained paired tool evidence. Equal aggregate counts do not merge their answer,
backend, effort, indexing-cost or timing provenance. The
Earlier GPTgrep 5.6-versus-6 Luna arms remain historical diagnostics; no new
GPTgrep 5.6 runs or recoveries are required for this preview. For the
user-selected first-release target, GPTgrep 6 Luna/Fast is compared with the
frozen PageIndex Flash plus GPT-5.6 Luna baseline; disclose that model difference.
Future exploratory xhigh/max configurations must be
labelled as such. Freeze a selected complete configuration before a fresh
full-cohort confirmation; neither per-task best-of answers nor post-result task
exclusions count. A cross-model whole-system gain cannot be attributed
to retrieval alone. Provider/backend substitutions and harness differences must
also be explicit.

A named, pinned conventional embedding-RAG comparator is deferred to the version
after the first accepted research-preview release. Its preparation, experiments
and benchmarks are pending and do not block that first release. The current
release comparison remains PageIndex Flash under G3-G5. When resumed, the separate
evaluation comparator must declare chunking, encoder, similarity metric and context
budget, with the same task scope, reader and judge profiles where controlled. It
does not become part of GPTgrep's vectorless runtime; a generic "standard RAG" label
is not a reproducible baseline specification.

Discover new issues from live failures, propose one falsifiable improvement, run
paired ablations, retain failed outcomes, and promote only verified improvements.
Never hardcode benchmark questions, answers, document IDs, annotated evidence
pages, or task-specific routing/response rules in product code or adapters. The
harness loads the pinned dataset at runtime. Retrieval receives only the question,
protocol-permitted scope and document evidence; gold answers and annotations are
reserved for separate judging/scoring. Improvements must use general algorithms
and synthetic regression cases, rather than recognizing evaluation tasks.
This also forbids specialization to benchmark document types, question templates,
answer formats and annotated answer-location distributions. Index construction
receives raw documents alone. Reader prompts and caches must not inherit task
labels, gold annotations or previous judge state. Synthetic metamorphic checks
should vary file names, page placement, section order, question wording and facts
to establish that improvements are general. Protocol-provided document scope is
permitted only when supplied consistently to both systems and clearly labelled.
Run the complete 62-task/34-document cohort as the live A/B benchmark. Offline
fixtures validate implementation; small or synthetic live smokes do not satisfy
the A/B requirement or first-release gate. Do not tune on held-out test answers or relabel
failures as excluded successes. Report quality, citation fidelity, ingestion
coverage, latency, token usage and available cost as separate measures.
Retain per-task retrieval and tool-failure signals so an observed gap can become a
specific issue, falsifiable optimization and paired rerun. Adapter startup or
backend differences alone cannot establish retrieval superiority.

The comparison contract is `workspace/harness-config/goal-contract.json`. The
native outer Goal remains active until that contract's actual acceptance is met.
