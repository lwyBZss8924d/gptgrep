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
See [architecture](docs/architecture.md)
and the explicit
[Flash stage coverage](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md).
Maintainer research notes under `docs/research/` are local-only and untracked.

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

## Core Jev retrieval

Jev is a required part of the GPTgrep search system. Default search performs
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
model = "gpt-5.6-luna"
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
  --model gpt-5.6-luna --reasoning-effort max --service-tier fast --json
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
`gpt-5.6-luna` / `max` / `fast` planner. It retains the original question and
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
```

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
