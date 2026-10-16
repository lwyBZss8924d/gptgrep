# Optional incur interface

`gptgrep-ai` uses pinned published `incur` 0.5.1 to expose command schemas and structured formatting around the native Rust `gptgrep` binary. Node 22 or later is required for this optional package. Native `gptgrep` remains independently usable without Node or Bun. The wrapper remains a thin execution/discovery layer; planning, parsing, retrieval and citation validation stay in the Rust implementation.

Install this package's locked dependencies with `pnpm install --frozen-lockfile`, then run `node src/cli.js`. Put the native binary on PATH or set `GPTGREP_BIN` to its executable path. The wrapper does not install or download the native binary.

Jev routing and reranking are a required core GPTgrep capability. Search defaults to `hybrid`, which invokes the native Jev Decisions path and requires `OPENROUTER_API_KEY` in the process environment. `regex` and `lexical` are explicit deterministic primitives; select one of those modes when that operation is intended. Schema discovery does not invoke a model.

```sh
GPTGREP_BIN=/path/to/gptgrep node src/cli.js index --request '{"root":"./docs"}' --json
GPTGREP_BIN=/path/to/gptgrep node src/cli.js search --request '{"query":"How does snapshot recovery work?","root":"./docs","document":"manual.pdf"}' --json
GPTGREP_BIN=/path/to/gptgrep node src/cli.js search --request '{"query":"--mcp","root":"./docs","mode":"lexical","limit":10}' --json
node src/cli.js search --schema --json
node src/cli.js request-schema search --json
node src/cli.js --llms --format json
node src/cli.js workflows --json
node src/cli.js native-schema --json
node src/cli.js parse-agent --help
node src/cli.js request-schema 'parse-agent run' --json
```

All forwarding commands accept `--request` with one JSON object. Nested names are space-separated: `gptgrep-ai parse-agent agent --request JSON`; quote the name for schema lookup: `gptgrep-ai request-schema "parse-agent agent" --json`. `--schema` and `request-schema` are local wrapper discovery; `native-schema` runs the selected binary to return its current versioned operation contracts. `request-schema COMMAND --json` describes that object's fields. `COMMAND --schema --json` describes the wrapper's environment, options and output. Shell callers should encode the complete JSON object as one argument; programmatic callers should pass an argument array. This keeps query text such as `--json`, `--schema`, and `--mcp` from being interpreted by incur as flags. For ordinary grep positional patterns, use native `gptgrep` directly.

| Command | JSON request fields | Native operation |
|---|---|---|
| `index` | `root` (default `.`), `optimizeMerge`, optional `maxFiles`, rich/OCR or pinned document-agent controls below | `gptgrep index ROOT [--optimize-merge] --json` |
| `search` | `query`, `root`, `mode` (`regex`, `lexical`, `hybrid`, `semantic`; default `hybrid`), optional `document` and Jev `model`, `limit` (1–1000; default 20), `context` (0–100; default 0), `minScore` (0–1; default 0.5), `ignoreCase`, `fixedStrings` | `gptgrep search QUERY ROOT --mode MODE [--document PATH] [--model JEV_MODEL] --limit N --context C --min-score S --json` |
| `tree` | `file`, `root` | `gptgrep tree FILE --root ROOT --json` |
| `read` | `nodeId`, `root`, `offset` (default 0), `maxBytes` (1–65536; default 8192) | `gptgrep read DOCUMENT_ID:NODE_ID --root ROOT --offset OFFSET --max-bytes BYTES --json` |
| `batch` | `root`, `rich` (default false), `batch` (native request object); optional `model` and `multimodalTimeout` (both require `rich: true`) and `decisionsBudget` (used only by paid rich operations) | `gptgrep batch --root ROOT --input - [--rich] --json`, with the request sent on stdin |
| `files` | `root` | `gptgrep files ROOT --json` |
| `parse` | `file` | `gptgrep parse FILE --json` |
| `status` | `root` | `gptgrep status ROOT --json` |
| `doctor` | none | `gptgrep doctor --json` |
| `ask` | `question`, `root`, optional `document`, `jevModel` and host settings below | `gptgrep ask QUESTION ROOT [--document PATH] [--jev-model JEV_MODEL] --json` |
| `summarize` | `nodeId`, `root`, optional `document`, `jevModel` and host settings below | `gptgrep summarize DOCUMENT_ID:NODE_ID --root ROOT [--document PATH] [--jev-model JEV_MODEL] --json` |

Search defaults to hybrid with zero context. `minScore` is a relevance-rubric floor, not calibrated confidence. A search request's `model` selects the Jev Decisions model; omitting it preserves the native default. `document` restricts retrieval to one exact indexed source-relative path, including paths containing spaces.

`read` preserves the native bounded source window, typed evidence and `next_offset`. `batch` serializes its nested request once and sends at most 1 MiB of UTF-8 JSON to native stdin. Native validation owns the versioned batch schema, operation IDs, generation binding and per-item output reservations. Inspect every item's status, coverage and continuation; global and item failures preserve the structured native report with a nonzero exit status.

For a separately published rich v2 generation, opt in with `rich: true`. `inspect_visual` enumerates manifest-bound renditions independently of text matches; `read_visual` reads a complete verified rendition as base64. Copy rendition IDs and the generation pin from tool output. Enumeration cursors bind scope, query and options; image reads are never truncated or paginated. These local operations do not invoke OCR or a model, prove model image delivery, or issue visual citation rights. The stored generation resource bounds and the complete batch output budget also apply to image responses.

```sh
node src/cli.js read --request '{"nodeId":"DOCUMENT_ID:NODE_ID","root":"./docs","offset":0,"maxBytes":8192}' --json
node src/cli.js batch --request '{"root":"./docs","rich":true,"batch":{"schema_version":"gptgrep.batch.rich.request.v1","operations":[{"id":"visual","op":"inspect_visual","max_candidates":2}]}}' --json
```

`ask` and `summarize` invoke the native local Codex host, using the existing account environment. Their host settings are `codexBin` (default `codex`), optional `codexHome`, `model` (the Codex model, default `gpt-6-luna`), `reasoningEffort` (default `max`), `serviceTier` (default `fast`), `timeout` (default 180 seconds), and `maxToolCalls` (default 12). The retained host mode accepts at most 900 seconds and 64 tool calls. Explicit `ask` with `nativePtc: true` and an existing `codexPermissions` profile forwards larger caller budgets. The wrapper timer supports at most 2147473 seconds; native mode/resource validation remains authoritative. Additional host fields are `codexStateDir`, `codexPermissions`, `maxInputBytes` (at most 1048576), and private `protocolTrace`. Their separate `jevModel` field forwards `--jev-model`; omitting it preserves the native Jev default. The optional `document` scope forwards `--document`. For `summarize`, that path must match the selected node's source document; the native host rejects a conflict. The wrapper preserves the distinction between Codex and Jev model settings. Native citation verification and host receipts are preserved in its structured response.

```sh
node src/cli.js ask --request '{"question":"How is recovery verified?","root":"./docs","document":"manual.pdf","jevModel":"typesafe/jev-1.13"}' --json
node src/cli.js summarize --request '{"nodeId":"DOCUMENT_ID:NODE_ID","root":"./docs","document":"manual.pdf","model":"gpt-6-luna","jevModel":"typesafe/jev-1.13"}' --json
```

Use `--format json`, `--format jsonl`, `--format toon`, `--format yaml`, or `--format md` for incur presentation. `--full-output` requests its envelope. The native response is preserved as structured data; the wrapper does not infer an answer, alter rankings, or claim that a fallback used an index.

Native invocations use argument arrays, never a shell. Each request may include `execution: {"timeoutSeconds": 3600, "maxOutputBytes": 16777216}` for wrapper process limits. Captured stdout and stderr each default to 8 MiB; foreground streams share that byte allowance. Ordinary calls default to five minutes. Host, enrichment and multimodal calls derive their wrapper deadline from the declared native timeout plus ten seconds; `index` with `ocr: "auto"` derives it from `ocrTimeoutSeconds` (default 1800) plus ten seconds. Native-control-driven document operations, document-agent indexing and foreground serving use the native original deadline unless a wrapper deadline is selected. A wrapper limit does not renew any native model, resource or ledger allowance. Interrupt and termination signals are forwarded to the child. Exit 1 is treated as a no-match success only for search with a recognizable empty `hits` or `results` array and no error marker.

Recognized `gptgrep.error.v1` failures retain their native structured report, including partial Jev request/usage accounting and ledger paths, with a nonzero process exit status. Exit-2 search reports with nonempty `coverage.stale_files` are also preserved. `--full-output` marks these envelopes `ok: false` and includes the native report under `data`; output filtering does not discard failure accounting. Invalid or unrecognized native failures remain actionable `NATIVE_ERROR` responses. Known provider-key values are redacted from forwarded diagnostics and failure reports.

MCP entry points are rejected before incur handles argv. The wrapper does not expose `fetch`, register MCP, sync generated skills, or perform wrapper updates. Incur's standalone binary builder embeds Bun; it does not build or bundle the native Rust executable. Native release packaging is managed by the repository's Rust workflow.

Run `pnpm test` for wrapper parsing, native forwarding, error and no-MCP tests. These tests use an explicit local fixture executable; combined native retrieval tests belong to the Rust workspace and a real-binary smoke test.


For ordinary source-bound v2 retrieval, set `multimodal:true` in `search` or `ask`. The wrapper forwards the native workflow and its typed reports; it does not provide a fallback or issue citations. Multimodal search admits only hybrid mode and rejects explicit legacy `limit`, `context`, `ignoreCase` and `fixedStrings` fields. Multimodal-only bounds require the flag: `multimodalTimeout`, `windowBytes`, `maxVisualCandidates`, `maxConcurrentBatches`, `maxImageBytes`, `maxImagePixels`, `maxMetadataBytes`, and `maxOutputBytes`. Ask also accepts `maxCitations` and `minScore`. Native preflight owns generation integrity, remaining resource budgets and actual reader delivery checks.

```sh
node src/cli.js search --request '{"query":"What does the chart show?","root":"./documents","multimodal":true,"maxVisualCandidates":8,"decisionsBudget":"/path/to/existing-budget.json"}' --json
node src/cli.js ask --request '{"question":"Explain this chart.","root":"./documents","multimodal":true,"maxCitations":4}' --json
```

The optional `decisionsBudget` on search, ask, summarize, rich batch, rerank, judge and enrich forwards the existing `--decisions-budget` contract; it never initializes a ledger. The native model default is now `gpt-6-luna/max/fast`, matching the native CLI. Multimodal ask rejects other native profiles. Model exposure in search is noncitable, while ask requires complete actual reader delivery and fresh source/coordinate bindings. Offline wrapper checks do not qualify real provider image influence or native model availability.


`rerank` forwards a nested `rerank` object to native bounded stdin. Its wrapper
request fields are `root`, `model`, `multimodalTimeout`, `decisionsBudget` and
`rerank`. Native validation owns `gptgrep.rich_rerank.request.v1`, including exact
source references and budgets; the wrapper never adds candidate content.
Structured native failures retain partial observations and a nonzero exit status.
`request-schema rerank --json` describes this forwarding boundary. The equivalent
rich batch `rerank` operation uses the same batch client and output/call shares.

The wrapper also forwards these native commands:

| Command | Request fields | Result/effect |
|---|---|---|
| `parse-agent serve` | `config`, `expectedControlSha256` | Supervised native NDJSON toolbox on stdin/stdout |
| `parse-agent run` | Same pins, `source`, `mode` (`off` or `auto`), `output` | Native product file; successful native execution is silent |
| `parse-agent agent` | Parser pins, `documentAgentControl`, `expectedDocumentAgentControlSha256`, `source`, `output` | Explicit native document job and its native report |
| `parse-agent qualify` | Document-job pins, `qualificationRequest`, `expectedQualificationRequestSha256`, `evidenceOutput` | Private evidence; acceptance/publication remains separate |
| `ocr serve` | `config`, optional `tokenEnv` (environment variable name), `initializeLedger`, `expectedControlSha256` | Native foreground loopback OCR lifecycle |
| `enrich` | `root`, `ledgerPath`, `maxBuilderCalls`, `maxJevCalls`, optional plan/resume/model/budget fields | Native navigation plan or enrichment report |
| `judge` | `input` (native JSON file), optional `model`, `decisionsBudget` | Explicit paid native Decisions request; native file bound is 64 KiB |
| `host-complete` | `completion: {instructions, state, schema}` and host fields | One bounded structured request on native stdin |
| `native-schema` | none | Current native versioned contract |

Native `gptgrep decisions-budget init` (creates a new ledger and its 0600 control file) and `gptgrep decisions-budget status --control FILE --json` are not forwarded by this wrapper. Run them with the native CLI and pass the control file as `decisionsBudget`.

`index` exposes `rich`, `ocr`, `planOnly`, `resume`, `ocrRunId`, `ocrRuntimeProfile`, `ocrCodexBin`, `ocrCodexHome`, `ocrCodexStateDir`, `ocrMaxPages`, `ocrTimeoutSeconds`, `pageWindow` and `expectedPlanSha256`. Document-agent indexing instead supplies all four fields `parseAgentConfig`, `expectedParseAgentConfigSha256`, `documentAgentControls` and `expectedDocumentAgentControlsSha256`. The native CLI rejects mixing that pinned path with legacy rich/OCR controls or `optimizeMerge`. The wrapper forwards no unsolicited OCR defaults on the pinned path.

`ask` additionally exposes the real native options `experimentalQueryPlan`, `plannerModel`, `experimentalSourceContinuation`, `experimentalEvidenceRoles`, `navigationOverlaySha256`, `navigationMaxJevCalls` and `navigationMaxRequestBytes`. Their existing native dependencies and source bindings apply. `search` exposes `maxCandidates` and `routingDocs` for legacy retrieval; multimodal requests reject these legacy candidate flags.

`enrich` supports `planOnly`, `planOutput`, `expectedPlanSha256`, `resume`, `rebuildFailedWindow`, `supportRetries`, `windowBytes`, `maxHintsPerWindow`, `maxLedgerBytes`, `maxWindowsPerRun` and `deadlineSeconds`, plus `builderModel`, `jevModel`, Codex settings and `decisionsBudget`. Inspect `request-schema enrich` for the complete shape. Incomplete/failed native reports retain their ledger and builder/Decisions accounting with a nonzero exit, including when output filters were requested.

Foreground commands stay attached until the native process exits. `parse-agent serve` inherits stdin and streams replies; `ocr serve` streams native lifecycle records. A prepared OCR record has `listener_ready: false` and is not completion. `parse-agent run` also uses this direct output path because its actual native contract writes a product file and may emit no stdout. Incur formatting, filters and `--full-output` do not alter these streams or append a success envelope. Provider/token values present in the process environment are redacted from diagnostics, including split chunks. Native nonzero exits are preserved; wrapper interruption reports 130/143, its elapsed deadline 124 and its output limit 125. On POSIX systems a live guardian anchors the owned process group. The supervisor forwards the signal to the native child, terminates that still-owned group after the native exit report (or after five seconds without one), and waits for the guardian and output pipes to close. An unexpected guardian exit is an explicit error and revokes group ownership; the wrapper never schedules a later signal against its reaped PID. Foreground supervision is unavailable on Windows. Native controls own service admission, authentication, budgets and their cleanup protocol.

```sh
node src/cli.js parse-agent serve --schema --json
node src/cli.js request-schema 'ocr serve' --json
node src/cli.js host-complete --request '{"completion":{"instructions":"Return the input count.","state":{"count":3},"schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}}}' --json
```

`workflows` exposes the four repository runtime Skills under `skills/`, their reusable native commands, and `runtime/codex/manifest.json`. These are source-relative references, not automatic Skill installation. The [Rust SDK guide](../../docs/rust-sdk.md) and [architecture](../../docs/architecture.md) describe direct public Rust crate APIs for PTC programs; the caller selects an available source SDK and Rust toolchain. This Node wrapper adds no JavaScript-to-Rust FFI and does not force programs to use CLI subprocesses.

LiteParse, tgrep-core and PageIndex structure are embedded Rust components in this package contract. Discovery points to their actual GPTgrep operations and public crates. It does not advertise standalone `lit` or `tgrep` binaries as bundled tools. Use native `doctor` for observed selected-runtime capabilities. Offline fixture tests validate forwarding and process supervision; they do not qualify native models, OCR listener readiness or a published generation.
