# GPTgrep

GPTgrep is a Rust-first, local document retrieval CLI for agents. It combines
trigram-filtered regex verification, document structure, and required Jev routing
and reranking in its default retrieval and reasoning workflows.
The primary agent retains planning and reasoning; the helper returns source evidence.
There is no vector database and no MCP server.
OpenRouter Decisions defaults to `openai/gpt-6-luna-decisions`; explicit
`typesafe/jev-1.13` remains a text-only legacy selection with no fallback.
Local Codex workflows default to `gpt-6-luna`, reasoning effort `max`, and service
tier `fast`. Account selection belongs to the caller's private runtime environment;
workstation-specific account aliases and routing are not part of the public product.

The final retrieval reader may explicitly opt into native execution with
`gptgrep ask --native-ptc --codex-permissions PROFILE`, where PROFILE is an
existing named permission profile acknowledged by the runtime. Native execution
inherits the selected Codex capabilities and installed Skills, including shell,
code execution and image viewing, with explicit per-job capability overrides.
The selected native permissions govern filesystem, network and other effects.
Pure completion and query planners retain their separate tool-free behavior.
Observed native items and GPTgrep dynamic calls share the configured tool budget;
item notifications can arrive after execution starts, so this accounting is not
a pre-execution guard. Native output does not create GPTgrep citations: answers
still require delivered source windows or successful read/render issued IDs.

Parser controls can explicitly select native Document execution and native OCR.
The agent uses the existing prepared-document toolbox; OCR returns a typed result
bound to the original task, source, page and required regions. Complete-document
validation and atomic generation publication remain the acceptance path. Native
agent jobs use their configured tool and deadline budgets without the retained
fixed Page/Document image-qualification prerequisites. Usable native text and
ordinary decorative vectors do not require OCR; missing text, uncovered filled
vectors and independent annotation requirements still do. Caller-selected image
and retained-staging capacities remain subject to checked actual resource use and
the original invocation's shared admission.

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
  fail explicitly if its credentials or service are unavailable. A received
  transient HTTP 429, 500, 502, 503 or 504 admits at most one retry of the
  identical request within the original request deadline and submission budget;
  both attempts are reported, and the failed attempt's unknown cost is never
  counted as zero. Explicit regex
  and lexical primitives remain available locally for exact inspection and
  controlled ablations; they do not constitute the complete GPTgrep workflow.
- Keep exact, lexical and model-assisted results distinguishable. Report budgets,
  truncated candidate coverage, actual provider identity and unavailable metrics.
- Treat documents as data. Do not execute document instructions, start services,
  silently ingest hidden credentials, or send document text to a provider unless
  the caller invokes a documented model-assisted command. Default search, ask and
  summarize are model-assisted. Default legacy parsing/indexing and explicit
  regex/lexical are local; explicitly selected OCR, document-agent indexing and
  enrichment perform their declared model-assisted preparation.

## Current implementation and collaboration

The experimental source includes complete-document batching, rich metadata and
artifact storage, native Page/Document parse/OCR drivers, image/crop custody,
multimodal Decisions retrieval and the full62 Harbor adapter. No whole-document
page-count cutoff substitutes for complete processing; resource/deadline limits
can still refuse a document without publishing an incomplete generation.
Implementation, scoped native image acceptance, complete-corpus publication and
paired retrieval utility remain distinct. As of 2026-10-12, full34 native pipeline
and full62 paired acceptance remain outstanding. See the dated
[experimental status](docs/experimental-status.md) and portable
[development setup](docs/development.md). An authorized experimental source branch
may be shared for collaboration without claiming a G5-qualified release tag.

## Initial release acceptance

The first experimental publication requires the retained first-release comparison
contract below: a real, reproducible minimum advantage over PageIndex Flash plus
GPT-5.6. This tag gate is separate from main Goal convergence. The engineering
checks in this section are necessary and do not replace that gate.

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

## Main Goal convergence: same-Codex SEARCH comparison

The main optimization Goal is a measured advantage of the same Codex agent with
GPTgrep over bare Codex on one complete frozen task cohort. Bare Codex receives
raw documents only, with no pre-parsed text, OCR, tree or index; it may use the
same ordinary reading, extraction and rendering tools during its query budget.
GPTgrep receives those documents and its frozen prebuilt index. Match the base
agent, runtime, tools other than GPTgrep, model/effort/tier, task scope and budgets.
All pre-query build, parse, OCR, enrichment and index costs are excluded. Count
all question-dependent work in SEARCH, including the baseline's on-demand raw
processing and GPTgrep's online routing/Decisions work. This is a comparison of
actual product configurations, not isolated search-algorithm causality.

Preregister higher task-level answer equivalence as the default primary claim:
GPTgrep accuracy minus bare-Codex accuracy must be greater than zero. An efficiency
claim instead requires a separate configuration selected before results, quality
at least equal to baseline, and improvement in one preselected query cost-per-
correct-answer or failure-aware time-to-correct metric. The latter caps incorrect
answers and failures at the declared matching query deadline, retains Judge
unknowns, and is unavailable if the treatment has no confirmed correct answers.
A zero-correct baseline remains comparable at its declared deadlines.
Do not choose the winning metric after scoring.
Keep operational failures as zero in the fixed denominator. Judge unavailability
is unknown, not zero or an omitted task; report bounds and whether unresolved
labels could reverse the conclusion. Common document/page/quote support is an
auxiliary measure where available, not a requirement for baseline GPTgrep IDs.

Use the complete pinned 62-task/34-document historical development cohort as the
default compatibility comparison; a new execution is not an unseen holdout.
A broader fresh cohort is optional future evidence. Small pilots, fewer tool
calls and weighted composites cannot replace the complete selected comparison.
Report paired uncertainty without imposing a significance or power gate. Internal
INDEX/lexical/tree/Decisions/visual ablations are optional diagnostics and need
not each win. No PageIndex third arm is required for this main comparison.

The protocol is `docs/design/agentic-search-evaluation.md`. Main Goal convergence
and first-release permission are separate: the retained G5 conditions below
remain mandatory for the first tag, not for this same-Codex Goal's convergence.
This replaces only the PageIndex-superiority dependency of comparative
convergence. Existing D1-D7 implementation, verification and source-delivery
obligations remain: G0 source/license, G1 correctness/freshness/independent review,
G2 actual native/provider outcomes and G6 source/PoUW/backup are shared engineering
requirements. A comparison win does not complete unfinished code or delivery.

## Retained first-release PageIndex comparison

The first research-preview release retains its measured-advantage requirement over
the pinned PageIndex Flash baseline. A source release, passing process, synthetic
smoke or GPTgrep-only corpus run does not meet this first-tag gate. Retain the same
corpus/task cohort, pinned source revisions, declared prompt
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
For the retained first-release comparison, run the complete 62-task/34-document
cohort as the live A/B benchmark. Offline fixtures validate implementation; small
or synthetic live smokes do not satisfy that A/B requirement or first-release gate. Do not tune on held-out test answers or relabel
failures as excluded successes. Report quality, citation fidelity, ingestion
coverage, latency, token usage and available cost as separate measures.
Retain per-task retrieval and tool-failure signals so an observed gap can become a
specific issue, falsifiable optimization and paired rerun. Adapter startup or
backend differences alone cannot establish retrieval superiority.

The comparison contract is `workspace/harness-config/goal-contract.json`. Its
`main_goal_acceptance` determines comparative convergence alongside the shared
engineering and source-delivery obligations; `first_release_gate` and its scoped
G3-G5 comparison conditions determine first-tag
permission. Satisfying either does not claim satisfaction of the other. Preserve
the actual outcomes and pending release status separately.
