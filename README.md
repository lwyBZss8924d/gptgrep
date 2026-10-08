# GPTgrep

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

Local document retrieval for agents: **Rust parsing and document trees, embedded
trigram grep, core Jev routing/reranking, and a local Codex reasoning host**.
No vector database, no search daemon, and no MCP server.

GPTgrep returns inspectable source evidence. Default `search` uses `hybrid` and
requires Jev routing/reranking. `ask` and `summarize` also execute this stage before
the final Codex reader; missing credentials or provider failures are explicit
errors. `semantic` and `judge` also use remote Jev inference. Explicit `regex` and
`lexical` commands are local retrieval primitives. The reasoning agent can compose
searches, inspect trees and read bounded nodes before answering.

This is development source. The first experimental release requires a verified
minimum live advantage over PageIndex Flash plus GPT-5.6 on PageIndex-OSS-Benchmark.
The original local PageIndex OSS `results.json` reports 60/62 with Luna/high;
our separately qualified live SDK/Codex control also reports 60/62 with Luna/max.
These runs have distinct model/backend and evidence provenance.
See [architecture](docs/architecture.md)
and the explicit
[Flash stage coverage](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md).
Maintainer research notes under `docs/research/` are local-only and untracked.

`gptgrep enrich` is an explicit, model-assisted step after local `index`. Its
`--plan-only` mode walks the full canonical document corpus and fixes a bounded
work plan without model calls. A complete live build can publish a separate,
source-bound navigation overlay using Codex (builder default `gpt-6-luna`) and
Jev support judgments; generated hints are never citations. Incomplete work
retains its ledger and leaves the current index intact. Scoped planned `ask`
can opt in with `--navigation-overlay-sha256` from the completed `enrich`
receipt. It scans the source-bound hints with Jev before the planner and reader;
the ordinary `ask` path remains unchanged. Hint quality and benchmark benefit
still require live measurement.

## Build and use

```sh
cargo build --release --locked
./target/release/gptgrep doctor --json
./target/release/gptgrep index ./documents --json
./target/release/gptgrep search 'retention|expiry' ./documents --mode regex --json
./target/release/gptgrep search 'signed snapshot recovery' ./documents --mode lexical --json
```

The native parser dependency downloads its pinned PDFium binary during the first
build if it is not already provided. Native release archives include a PDFium
runtime and notices; use their `gptgrep` launcher to select the bundled library.
Office conversion additionally needs LibreOffice. OCR is disabled in the initial
build: textless scans fail explicitly. Plaintext/Markdown work without loading
PDFium. [Release packaging](docs/release.md) describes the verified platforms and
relocation checks.

The index lives under `documents/.gptgrep/`. Hidden and ignored descendants,
credential files, symlinks and common build/runtime directories are excluded.
Each complete rebuild publishes a new immutable generation. Failed builds preserve
the previous generation. Old generations are retained until explicitly managed.

```sh
# Exact grep: no model or API key.
gptgrep search 'SNAP-[0-9]+' ./documents --mode regex -C 2 --json
gptgrep search --mode regex --fixed-strings --ignore-case --json -- '--flag-like text' ./documents

# Optional PageIndex scan-cost merge stage for native paginated documents.
gptgrep index ./documents --optimize-merge --json

# Discover the tree, then read an exact document_id:node_id.
gptgrep files ./documents --json
gptgrep tree manual.pdf --root ./documents --json
gptgrep read DOCUMENT_ID:NODE_ID --root ./documents --max-bytes 8192 --json
gptgrep status ./documents --json
gptgrep --schema
gptgrep --llms
```

`--limit` bounds returned results. `--context` defaults to zero. Exit status is 0
for success/matches, 1 for no matches, and 2 for an error or excluded stale
evidence. JSON is one object on stdout; diagnostics belong on stderr.
Node reads return `next_offset`; pass that value to `read --offset` to continue.
Offsets count UTF-8 bytes within the node, and each window retains exact source
coordinates. This lets agents inspect large sections without an unbounded reply.

For v2 generations published through the rich core API, `gptgrep batch --rich
--root ./documents --input rich-operations.json` accepts
`gptgrep.batch.rich.request.v1`. Its local operations are `inspect_documents`,
`inspect_pages`, `read_artifact` and `search_artifacts`. A batch resolves `CURRENT`
once or uses an explicit generation and manifest SHA. Input and output are bounded
to 1 MiB with at most 64 operations, and every item reserves output space before
execution. Inspection returns indexed metadata; text reads and searches retain
typed native/OCR evidence and source checks. Search cursors cover a bounded
`max_hits` result window, so callers must inspect truncation and exhaustion.
The response is always JSON, with exit status 2 for an item or global failure.
Reader byte counters describe partial API reads rather than complete physical I/O.
The ordinary `index` command currently publishes the original generation format.

The Rust core provides `DerivedVisualCache` for PNG crops obtained from a
`VisualGenerationReader` rendition. Each derivative binds its generation, source,
parent image, crop recipe, coordinate transforms, encoder and output digest.
Cache hits and reads recheck the parent. Payload, metadata and working capacities
are admitted separately; retained image pins and recipe tokens keep their capacity
charges after eviction or `clear`. This process-local cache preserves the sealed
generation and grants no reader image delivery or citation rights. Its bounds cover
owned cache payloads and crop admission, not total process memory.

## Core Jev retrieval

The required Decisions stage defaults to `openai/gpt-6-luna-decisions`.
Explicit `--model typesafe/jev-1.13` selects legacy text-only Jev. Default search performs
document routing and evidence reranking; there is no automatic local-only
fallback. Parsing and index publication are deterministic local preparation.
Use `--document manual.pdf` to constrain retrieval to one exact indexed path
before candidate budgets apply. Results expose `document_scope` and `coverage`.

Provide `OPENROUTER_API_KEY` in the process environment using your existing secret
manager. GPTgrep does not automatically load credential files. The optional
development helper `scripts/with_dev_key.py` reads only an explicitly selected
entry and does not source shell code, copy an env file, or echo its value.

```sh
gptgrep search 'how can a damaged journal be recovered?' ./documents \
  --model typesafe/jev-1.13 --min-score 0.5 --json

gptgrep judge --input evals/requests/decision-smoke.json \
  --model '~typesafe/jev-latest' --json
```

Jev uses OpenRouter's typed **Decisions API**, not chat completions. It supplies
dynamic Choice/Noul/Score judgments; application code retains parsing, offsets,
arithmetic and workflow execution. The response records the actual returned
model, available usage and cost. Unknown metrics remain null. Network/schema
errors are explicit; there is no silent fallback, redirect or automatic retry.

Hybrid search combines lexical candidates with semantically routed tree nodes.
The initial policy routes at most 32 document descriptions and reranks at most
24 evidence candidates. Check `coverage` before treating a result as exhaustive.
`--min-score 0.5` is a relevance-rubric floor, **not calibrated confidence**;
`--min-score 0` exposes low-score candidates for exploration. Large-corpus
hierarchical semantic routing remains an explicit scalability task.
For an exact single-token query, the hybrid lexical lane retains verified token
matches even if Jev assigns a low score. Results expose `literal_anchor` and the
count retained below the floor; their model scores are preserved unchanged.

## Local Codex host

The dedicated helper defaults in [config/codex-host.toml](config/codex-host.toml)
are:

```toml
model = "gpt-6-luna"
model_reasoning_effort = "max"
service_tier = "fast"
approval_policy = "never"
sandbox_mode = "read-only"
```

An existing Codex installation and authenticated local account are required.
Select the account home explicitly for isolated development. Authentication is
handled by Codex; GPTgrep does not read or copy its auth cache or install a global
profile. [Codex authentication](https://learn.chatgpt.com/docs/auth?surface=cli)
documents headless device-code setup when no login exists.

```sh
gptgrep ask 'What is the recovery procedure, and how long are snapshots kept?' \
  ./documents --json

gptgrep summarize DOCUMENT_ID:NODE_ID --root ./documents \
  --model gpt-6-luna --reasoning-effort max --service-tier fast --json
```

By default, before starting the reader, the host executes required Jev hybrid retrieval and
supplies the bounded evidence to the reasoning model. `--jev-model` selects its
Decisions model independently of the Codex `--model`. `--document` scopes an ask;
a summary is scoped to its selected node's document. Follow-up searches default
to hybrid; tree reads and explicit exact refinements build on the initial result.
Reports retain Jev coverage, actual model and usage alongside Codex usage.

The host creates an ephemeral stdio app-server thread with an empty execution
environment and bounded GPTgrep catalog/tree/search/read tools. It verifies the
effective sandbox/approval settings, bounds time and tool calls, denies unexpected
server requests, and kills/reaps its owned child. Model-written citations must
refer to evidence actually issued in the run and still match the current source.
The report separates answer, evidence, tool receipts, actual model/effort and usage.
The host also writes a private, bounded metadata ledger under
`.gptgrep/host-attempts/` so completed Jev calls remain observable if a later stage
fails or is interrupted. Source documents stay unchanged; the Codex child has no
access to the Jev credential.
Citation identity checks do not independently prove semantic entailment.


The ask-only `--experimental-query-plan` option first runs a separate no-tools
`gpt-6-luna` / caller-selected effort / `fast` planner by default (the normal
effort default is `max`). Use `--planner-model` to select
another planner model explicitly, including `gpt-5.6-luna` for a controlled run.
It retains the original question and
proposes at most two alternate retrieval phrases. Up to two routing operations
run concurrently; exact source spans are combined within the existing candidate
budget and Jev reranks them against the original question before the reader starts.
The option is off by default and shares the caller's overall deadline. Planning or
branch failure is explicit. More diverse candidates can displace original
candidates and add latency; no quality gain is guaranteed. `model_attempts` and
`model_usage` include the planner separately; legacy `usage` remains reader-only.

```sh
gptgrep ask 'How are offline exports recovered?' ./documents \
  --experimental-query-plan --json

# After a completed enrich run, pass its artifact_sha256 and exact document path.
gptgrep ask 'How are offline exports recovered?' ./documents \
  --document manual.pdf --experimental-query-plan \
  --navigation-overlay-sha256 "$OVERLAY_SHA" --json
```

Add `--experimental-evidence-roles` to that command to test the default-off
`jev-evidence-role-v1` selection policy. Jev keeps the relevance Score and adds one
Choice per candidate: direct support, incomplete local evidence, background, or
no support. The original anchor precedence, score floor and three-hit limit stay
fixed. Role ordering and exact-window hints help the reader choose evidence and
source-local continuation; delivered-set sufficiency remains unassessed. A role
can support one requested fact without covering the whole question. If packing
changes the assessed bytes, the delivered role hint is unavailable. Discarded
candidate diagnostics carry no rejected text or citation authority.

At most 24 candidates produce 48 questions in the same union Decisions request,
whose complete encoded body must fit 64 KiB. Extra judgments can increase usage
and latency; subsequent reader work remains budgeted and measured. This option
adds no initial Jev operation or Codex step by itself. It requires
`--experimental-query-plan` and never enables a fallback on failure.

At the retrieval tool budget, the reader finalizes within its existing Codex turn and original deadline. Denied requests issue no new evidence and perform no Jev work; their diagnostics remain separate from model-turn counts. `node_coverage` describes each delivered excerpt, not cumulative reading: EOF can still leave a prefix outside that excerpt.

`host-complete --input FILE_OR_DASH` supplies the same isolated local model as a
typed workflow primitive. Its input is `{instructions, state, schema}` and its
output includes schema-validated `value`, model/effort, usage and input digests.
The default combined input budget is 256 KiB, explicitly adjustable up to 1 MiB;
prompts are never silently truncated. This completion mode does not claim citation
verification. It supports the declared provider bridge used to run original
PageIndex summary/optimization prompts during comparative evaluation.

The app-server API is experimental. Supported boundaries and configuration are
recorded in [the host crate documentation](crates/gptgrep-host/README.md).

## Optional incur interface

The native binary is the grep interface. `packages/cli` adds the actual incur
schema/discovery surface as `gptgrep-ai`, forwarding typed request objects to the
native executable without shell interpolation. The wrapper disables MCP serving,
skill synchronization and update entry points.

```sh
cd packages/cli
pnpm install --frozen-lockfile
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js --schema
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js search \
  --request '{"query":"retention","root":"/absolute/path/to/documents","mode":"lexical"}'
```

See [wrapper documentation](packages/cli/README.md) for exact request keys. The
wrapper pins published incur 0.5.1; the studied local 0.6.0 checkout was not a
published registry version during this investigation.

## Verification and evidence

Python checks use Python 3.12 or newer and a local environment with locked dependencies.

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 -m venv .local/eval-venv
.local/eval-venv/bin/python -m pip install --only-binary=:all: -r evals/requirements.lock
.local/eval-venv/bin/python -B -m unittest discover -s evals -p 'test_*.py'
.local/eval-venv/bin/python -B scripts/eval.py --binary target/release/gptgrep --output /tmp/gptgrep-eval.json
```

The fixture suite covers regex correctness, lexical retrieval, Unicode, exact
source digests/byte ranges, stale replacement/deletion and reindexing. Native
parser tests cover real generated PDFs, bookmarks, textless-document rejection and
more than 1,000 pages. The merge-stage fixtures execute the pinned upstream pure
functions for comparison. Mock Codex/HTTP tests establish transport invariants;
live host/provider receipts are separate evidence.

External PageIndex data is referenced by a pinned manifest and is not redistributed.
Running GPTgrep on that subset is distinct from executing the original PageIndex
SDK baseline. Historical upstream benchmark numbers are not GPTgrep measurements.
See [the baseline runner](scripts/pageindex_baseline/README.md) for denominators,
ablation boundaries, failed cases and cost accounting. Detailed local research
and experimental interpretation remain under the ignored `docs/research/` path.

## Limits of this release

- LiteParse supplies the layout front end. Full Flash character/font repair,
  multilingual heading classification and generative expansion parity are not
  claimed by the Rust structural/merge stages.
- OCR and universal Office/image acceptance are not claimed. Missing extraction
  capability is an explicit failure.
- Semantic candidate budgets can miss relevant documents/nodes. Jev cannot rescue
  evidence excluded upstream; its scores are not a truth guarantee.
- Corpus freshness is a snapshot contract. Rerun indexing for new files; a search
  only rechecks sources relevant to that retrieval, while `status` checks all
  existing indexed documents.
- Current indexing rebuilds the generation; incremental parse reuse and a broad
  performance/quality benchmark remain follow-up work.

Original code is [MIT licensed](LICENSE). Dependency and native-runtime notices
are described in [THIRD_PARTY.md](THIRD_PARTY.md).


The default OpenRouter Decisions selector is `openai/gpt-6-luna-decisions`.
`--model typesafe/jev-1.13` explicitly selects the legacy text-only route. Native
Codex reasoning uses `gpt-6-luna`, `max`, and `fast` independently. Responses retain
actual returned model metadata; admission requires the requested identifier or the
observed canonical mapping (`openai/gpt-6-luna-decisions-20261006` for Luna and
`typesafe/jev-1.13-20260917` for legacy Jev). Other explicit models require exact
identifier equality. No guessed dated aliases or fallback models are accepted.

`gptgrep judge --input evidence.json --json` also accepts the normal typed image
contract: `input.text` contains `{source, text}` objects and `input.images` contains
`{source, path}` objects for local PNG/JPEG files, alongside `questions`. Each
`source` contains `source_id`, `unit_id`, and `source_sha256`. The source digest is
caller provenance, not an independent freshness claim. Text is literal; image
bytes are admitted as bounded snapshots and sent in order as flat `image_url`
data URLs. Source bindings and local paths stay out of the provider body. The
64 KiB control/text envelope, four-image cap, per-image 1 MiB/8 million pixel cap,
24 million total pixel cap, 8 MiB request cap and 1 MiB response cap remain explicit.
Legacy Jev rejects typed images before submission. Refusals, unknown answers,
partial bodies and malformed responses do not become successful decisions.

For a shared experiment, add `--decisions-budget budget.json` to hybrid/semantic
search, judge, ask, summarize, or non-plan enrich. The portable control file is at
most 16 KiB and contains `schema_version: "gptgrep.decisions-budget-contract.v1"`,
`run_id`, `plan_digest`, `deadline_unix_ms`, `max_submissions` and an absolute
`ledger_path`. A trusted owner must already have initialized that matching ledger.
The CLI joins its original clock and consumed reservations; missing files or a
changed run, deadline, plan or cap are errors. It never creates or resets a ledger.
Routing, reranking and follow-up host clients share the same context. Without the
option, calls use a bounded single-invocation context. Regex/lexical, schema,
doctor and enrich `--plan-only` do not open the budget file. Numeric usage and cost
remain reported provider metadata; absent fields remain unavailable.


`search --multimodal` and `ask --multimodal` connect the v2 rich generation to the normal Decisions and native reader workflows:

```sh
gptgrep search 'What does the chart show?' ./documents --multimodal --max-visual-candidates 8 --json
gptgrep ask 'Explain the chart and supporting text.' ./documents --multimodal --document report.pdf --max-citations 4 --json
```

Both commands resolve `CURRENT` once, retain that source-bound generation and one finite Decisions execution context, and perform mandatory document routing followed by reranking of text and independent visual candidates. The visual lane can include pages without a literal query match. The default Decisions model is `openai/gpt-6-luna-decisions`; multimodal retrieval also accepts its observed canonical `openai/gpt-6-luna-decisions-20261006` identifier. Multimodal ask requires the separate native `gpt-6-luna/max/fast` profile. Explicit legacy Jev remains available on the text workflow.

Multimodal output is always one typed JSON report, including when `--json` is omitted. Search returns `gptgrep.rich_multimodal_search.v1`; its image exposure grants no citation rights. Ask returns `gptgrep.rich_multimodal.host.v1` and retains the initial retrieval report. Text and image citations require complete actual native reader delivery followed by source, hash and coordinate checks. Partial failures preserve observed retrieval/accounting in the existing `gptgrep.error.v1` envelope and exit 2.

Bounds are configurable with `--window-bytes` (4–2048), `--max-visual-candidates` (1–16), `--max-concurrent-batches` (1–4), `--max-image-bytes` (up to 1 MiB), `--max-image-pixels` (up to 8 million), `--max-metadata-bytes` (up to 512 KiB), and `--max-output-bytes` (up to 1 MiB). Ask additionally admits 1–8 `--max-citations` and applies the host's 32 KiB candidate metadata and 512 KiB initial-report caps. Tight budgets may refuse preparation before a model call. Reports distinguish inspected/omitted candidates, truncation and incomplete semantic coverage. These limits describe owned buffers and transport; they are not whole-process RSS or exhaustive recall guarantees.

`--multimodal-timeout` admits 1–900 seconds, default 180 for search; ask uses the smaller of this value and its existing `--timeout`. `--decisions-budget FILE` joins the same existing original-clock shared context for routing, reranking and host Decisions tools, without initializing or resetting its ledger. Without that file, the command creates one finite invocation context whose plan identity binds the retained generation manifest; it is not a portable campaign claim.

A v1 generation, regex/lexical/semantic mode selection, legacy grep/window/candidate options, or ask's experimental planning/navigation/continuation options are rejected for this workflow. There is no text or regex fallback. Omitting `--multimodal` preserves the legacy paths and output. The rich ingestion frontdoor supports lossless UTF-8 text/Markdown, PDF, PNG and restricted strict baseline JPEG. Plaintext preserves original bytes and line spans without physical-page or OCR claims. Office inputs use one bounded LibreOffice conversion retained for parsing, rendering and OCR; a working converter is required and real Office runtime qualification remains pending. The wider JPEG family remains unsupported. CLI composition and offline mock checks establish implementation behavior. Actual provider image influence, native runtime availability and the live comparison/release gates remain pending.
