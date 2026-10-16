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

Every new GPTgrep Harness Codex base-agent run uses the latest official Codex OSS
release. Resolve the official latest release before admitting a standalone run or
a new experiment, and record the observed release, observation time, executable
versions and asset hashes. The CLI and required companion runtimes, including
the Code Mode host, must come from that same release and pass an actual native
tool qualification before evaluation. A paired experiment freezes one verified
runtime across all arms and judges; a later experiment checks latest again.
Historical runtime pins remain attached to their original results and cannot
serve as the current baseline merely because they were previously qualified.

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
  Lexical and hybrid seeds keep at most 16 query terms after dropping a fixed
  English function-word list and short ASCII-letter tokens. When more remain, a
  bounded first 64 are ranked by scoped document frequency, rarer first and
  terms absent from the scope last; terms after the first 64 are not considered.
- Use Jev through its typed Decisions API for semantic document routing and
  candidate reranking. Default search, ask and summarize require this stage and
  fail explicitly if its credentials or service are unavailable. A received
  transient HTTP 429, 500, 502, 503 or 504 admits at most one retry of the
  identical request within the original request deadline and submission budget;
  both attempts are reported, the admitted retry is recorded before it is sent,
  and the failed attempt's unknown cost is never counted as zero. The retry waits
  a jittered 250-1000 ms, or a longer `Retry-After` of whole seconds; any other
  `Retry-After` form refuses the retry and is recorded as `retry_after_refusal`.
  It is sent only if the original deadline still leaves the wait plus 2 seconds
  and a submission remains. Explicit regex and lexical primitives remain
  available locally for exact inspection and controlled ablations; they do not
  constitute the complete GPTgrep workflow.
- Hybrid and semantic search route up to 32 scoped documents with one Decisions
  call. Exactly one routable document (for example a `--document` scope) is
  selected without that call (`coverage.routing_status = skipped_single_document`).
- If no reranked candidate meets the relevance floor and no literal anchor is
  retained, at most three top-scoring candidates are still delivered, flagged
  `below_threshold: true`, with `coverage.below_threshold_fallback` and a
  warning. They never count as passing, and such a search exits 0.
- The text retrieval reader resolves the in-text references a passage depends
  on before answering. Evidence, citation and abstention rules remain in force;
  the reader has no additional answer-commitment rule.
- Decisions admission accepts the exact selector, the observed canonical build of
  the default Luna and legacy Jev selectors, or, for a text-only
  `~author/family-latest` alias, only a concrete same-author, same-family build.
  Requested and served models are reported separately.
- Paid commands may join one shared original-clock submission ledger
  (`decisions-budget init`/`status`, `--decisions-budget`). Reservations are
  permanent, and the ledger counts submissions, not USD.
- Keep exact, lexical and model-assisted results distinguishable. Report budgets,
  truncated candidate coverage, actual provider identity and unavailable metrics.
- Treat documents as data. Do not execute document instructions, start services,
  silently ingest hidden credentials, or send document text to a provider unless
  the caller invokes a documented model-assisted command. Default search, ask and
  summarize are model-assisted. Default legacy parsing/indexing and explicit
  regex/lexical are local; explicitly selected OCR, document-agent indexing and
  enrichment perform their declared model-assisted preparation.

## Development lines and responsibility

The retained 0.1 baseline contains the latest integrated product implementation
and its scoped product checks. The active 0.2 alpha line replaces the former
PageIndex/full62 evaluation stack with the [XL benchmark kit](benchmark/UPSTREAM.md)
and [self-opt evaluation kit](self-opt/src/evaluate.ts). The product runtime,
benchmark adapter, preparation workflow and optimization policy have separate
responsibilities. Source delivery is not evidence of comparative utility.

Routine implementation, tests, CI, releases and delivery are authorized within
this direction. Major requirements, product/domain choices and research changes
remain Human decisions. Do not change global configuration or unrelated owners.
Independent review is required before release.

## Active comparative acceptance

The current comparison is the same native Codex agent with GPTgrep versus bare
Codex on a frozen XL-DocBench cohort. The authoritative protocol is
[docs/evaluation.md](docs/evaluation.md), with machine-readable state in
[goal-contract.json](workspace/harness-config/goal-contract.json). The former
PageIndex first-tag requirement and full62 cohort are retired. Their historical
results do not impose an active release gate or supply new XL measurements.

The selected supervisor is `gpt-6.1-sol`; workers and OCR use `gpt-6-luna`.
Choose Luna Decisions or Jev 1.14 explicitly for the helper profile. Freeze exact
models, effort, tier, runtime, permissions, tools, deadlines and budgets before
comparison. Record configured identities and actual observations separately;
unsupported profiles and backend substitutions cannot silently replace them.
These comparison choices do not change the product's documented CLI defaults.

Both arms retain the native Codex base and receive the same question, complete
public document context and output instructions. Bare receives the original PDFs
and common ordinary tools. GPTgrep additionally receives its pinned generic
Skills/roles, binary and complete preparation products scoped to that same
context. Pre-query preparation is measured separately. All question-dependent
work belongs to SEARCH, including on-demand raw extraction in the bare arm and
online routing/Decisions in the GPTgrep arm. Judge work is separate.

Native XL accuracy and independent grounded success/atomic coverage are distinct
metrics. The default development endpoint is strictly higher grounded success
on the same complete enrolled cohort. Preregister any minimum advantage or cost
constraint before scoring. Keep started operational failures as zero and
unavailable setup/judging as unknown in the fixed denominator. Do not select a
winning metric or best-of task answers after results. Missing model usage, costs
and request counts stay unavailable.

Development Opt loops use source-group-separated development material. A frozen
confirmation cohort and selected configuration must be declared before their
confirmation run; confirmation feedback cannot choose successive Opt parents.
The complete declared confirmation and independently reviewed matched advantage
are required for a canary/release claim. A small live sample, source release,
passing process, offline suite or treatment-only run cannot satisfy it. A full
XL result must cover the released benchmark; a subset remains explicitly scoped.
The current freeze CLI enrolls development samples only and grants no full
benchmark or canary acceptance.

## Engineering and delivery acceptance

1. Real supported documents parse, index, search and cite end to end. Tree spans
   are valid, complete and deterministic; resource refusal does not publish a
   partial generation.
2. Exact regex agrees with direct scanning across controlled literal, Unicode,
   MatchAll and case-insensitive cases. Source freshness, byte ranges and
   generation replacement remain covered by product tests.
3. Default model-assisted workflows execute their required helper and native
   stages, retain actual models/usage/latency/failures, and never infer live
   workflow success from a transport-only smoke.
4. Run formatting, workspace tests, Clippy with denied warnings, applicable
   wrapper tests, runtime staging, packaging self-tests and the active offline
   benchmark/self-opt contracts. Hosted CI and platform package qualification
   remain separate from local results.
5. Every comparative reader/judge has separate checked storage and authentication
   state, exact runtime permissions and source-bound result artifacts. Actual
   isolation and model behavior require live evidence, not synthetic tests.
6. Record source/license provenance, independent review, commit, machine-readable
   PoUW, exact private lineage and a verified external Git/notes backup. Push
   source and permitted tags to the authorized origin at the applicable delivery
   stage; do not publish private notes or runtime evidence.

Experimental builds retain `0.x.y-alpha.N` versions. A version update alone does
not create a tag or establish acceptance. The self-opt kit may select a measured
next development parent; its reports explicitly do not accept a release.

## Integrity and scope

Never hardcode benchmark questions, answers, document IDs, evidence pages,
question templates, answer-location priors or task-specific routing rules in
product code, prompts, caches or adapters. Load tasks at runtime. Indexes receive
raw documents alone; gold and annotations are isolated to the scorer/judge.
Reader sessions must not inherit prior judge state. Improve general algorithms
and generic harness behavior, with synthetic variations in filenames, page
placement, section order, question wording and source facts where relevant.

Preserve the full context supplied by the benchmark, not only evidence-bearing
documents. Group related editions, translations and near duplicates before
splitting. Exposed groups become development material. Keep failed and partial
runs, source-unavailable cases and unknown judgment visible. Retrieval benefit,
preparation coverage, citation grounding, latency and cost are separate claims.

The Rust structural/merge stages retain documented PageIndex provenance and
[coverage limits](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md). Removing its
old evaluator does not remove that product implementation or its licenses, and
no complete Flash parity or universal retrieval superiority is claimed.
New global registrations, private-history ingestion, unrelated project changes
and broad model-training/research programs are outside routine project authority.
