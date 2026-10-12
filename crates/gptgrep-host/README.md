# gptgrep-host

Local Codex workflows over GPTgrep's mandatory Jev retrieval stage.

`ask(root, question, config)` runs a host-owned hybrid search with Jev document routing and evidence reranking **before starting Codex**. Its bounded source evidence is included in the initial model input. The model cannot skip that stage by choosing catalog, tree, or read tools. `summarize(root, node_id, config)` derives the document path and section title from the selected node and constrains its initial search to that document. A conflicting `HostConfig.document` is rejected.

The caller must export `OPENROUTER_API_KEY` for Jev. Missing credentials, provider failure, invalid provider output, missing candidates, or failed seed delivery stop the retrieval workflow. There is no local-only fallback. A real rerank that filters every candidate can continue with an explicit `filtered_all` status and empty evidence. An empty or entirely stale candidate scope cannot be reported as successful reranking.

Further `gptgrep_search` calls default to `hybrid`; `semantic` selects the semantic lane. Explicit `regex` and `lexical` calls are local refinements after the required initial pass. `HostConfig.document` scopes catalog, tree, read, and search access to an indexed relative source path. An additional search cannot widen that scope. Questions and scope come from runtime inputs; indexing receives no questions or expected answers.

The reader defaults to `gpt-6-luna`, reasoning effort `max`, service tier `fast`, with 180 seconds and 12 model-requested dynamic tool calls. The host-owned initial search has its own receipt and is outside that dynamic-call count. One deadline covers initial Jev work and the later Codex workflow; bounded process cleanup can follow the deadline. `HostReport.elapsed_ms` includes initial retrieval, Codex work, cleanup, and final citation checks. `HostConfig.jev_model` selects the Jev model; `None` uses the Jev client's default. Authentication stays with the selected existing `CODEX_HOME`; the host never reads credential files or initiates login. The OpenRouter key remains in the parent process and is removed from the Codex child.

After the dynamic-call budget is used, valid requests receive a recoverable `tool_budget_exhausted` result directing the model to finalize from previously issued evidence. These denials perform no retrieval or Jev work. At most `max_tool_calls` such responses are allowed, accommodating parallel requests without asserting that they belong to one batch; a further valid request fails with typed `host_tool_budget_exhausted`. The same turn and original deadline apply throughout. Identity, allowlist and argument-shape checks still reject malformed requests. Calls admitted before exhaustion consume the budget even when their handlers return recoverable argument errors. Denial receipts and typed protocol failures expose `tool_budget` with the configured limit, admitted calls, denied responses and denial limit. Final answers still require the normal issued-citation and source-freshness checks.

Retrieval and independent JSON completion share `HostConfig.service_tier`. Accepted values are `fast`, its Codex alias `priority`, `flex`, and explicit standard routing `default`. The host sends the exact requested value through the ephemeral thread configuration and the `serviceTier` fields on both `thread/start` and `turn/start`. For fast routing it also enables the thread's `features.fast_mode`; no global profile is changed. Codex normalizes `fast` to `priority`. Both report types expose `requested_service_tier` and nullable `effective_service_tier`, preserving the server's actual `thread/start` acknowledgement. A returned `priority` is equivalent to requested `fast`; an observed different tier aborts before inference, without fallback. An omitted or null acknowledgement remains null with a warning. The acknowledgement describes thread configuration, not independently verified provider routing or billing.

`gptgrep_read` accepts `offset_bytes`, default `0`, relative to the selected node's canonical UTF-8 text. Results contain `node_offset`, `next_offset`, and the evidence window. Continue with the same node ID and the returned `next_offset`; `null` means EOF. The byte budget remains 1–6144, default 4096. Offsets must be UTF-8 boundaries. An empty EOF response supplies no new citable evidence. On `tool_output_limit`, retry the same offset with fewer bytes.

Citations retain each delivered window's offset, continuation cursor, exact byte interval and excerpt hash. Final validation rereads that exact offset and length, including windows beyond 64 KiB, and checks current source/generation identity. Host search uses zero surrounding lines so citable evidence stays inside its node; native CLI context behavior remains separate. Snapshot identity validation does not establish semantic entailment.

## Jev accounting and private runtime records

`ToolReceipt.search` retains each search's mode, required-initial flag, query hash, document scope, generation, raw core metrics/coverage, delivered hit count and output truncation. `HostReport.jev` aggregates:

- `required: true` and `initial_status`;
- `requests`: successful validated Jev responses;
- `attempted_calls`: logical client calls, not proof that HTTP was transmitted;
- `unobserved_attempts`: attempted calls without validated responses;
- actual returned `models`, `usage`, `accounting_complete`, and per-search `searches`.

Unknown usage and cost remain unavailable. Coverage is null until the core supplies an observed snapshot. Progress is cumulative within a search: accounting replaces the prior snapshot and aggregates once across distinct searches.

Before provider work, the host creates a private, create-new ledger under `.gptgrep/host-attempts/`. It rejects symlinks and path escapes. On Unix the directory is 0700 when created and files are 0600. This is a local metadata write; read-only mode describes source access and the Codex child, not an absence of runtime receipts. The JSONL ledger is limited to 4 MiB, 1024 events, and 64 KiB per event. Entries are synced before/after client calls and retain stage, hashes, scope, model/usage metadata and bounded receipt summaries—not source text, questions, credentials, or child stderr.

The ledger preserves completed routing usage if reranking fails or is cancelled. Interrupted workflows leave incomplete records. `HostRetrievalError` is serializable and contains `code`, `stage`, `generation`, `ledger_path`, `jev`, projected `receipts`, optional sanitized core `cause`, and `elapsed_ms`. Later Codex or citation failures retain already incurred Jev observations and remain errors. These records and native identities are private runtime evidence, not public release artifacts.

`HostConfig.trace_path` optionally adds a separate bounded protocol trace containing method, ID, and tool-name metadata. Error events add only a whitelisted Codex error variant, boolean `will_retry`, and a valid numeric HTTP status when present. Error messages, additional details, source payloads, and credentials are never copied into traces or failures. On Unix, cleanup targets a freshly created owned process group, including launcher descendants, and reaps its leader.

An identity-bound `error` notification with explicit `willRetry: true` keeps the same turn running under the original host deadline and protocol limits. The app-server owns that recovery; GPTgrep starts no replacement turn, performs no independent retry, and does not reset the timeout. Missing or malformed retry flags, foreign or missing identities, terminal errors, and failed turn completion remain failures. `HostProtocolError` exposes a typed `kind`, optional whitelisted `codex_error_info`, `will_retry`, valid `http_status_code`, and already-observed numeric token `usage`. Failed workflows retain `accounting_complete: false`; missing usage stays null. Retrieval preserves this object in `HostRetrievalError.cause`. Success reports and typed failures expose `server_retry_notifications`, which counts server events, not physical or billed provider requests.

## Native parsing and OCR roles

`parse_agent_driver` coordinates one prepared-document turn and delegates required
OCR to the separately qualified Page executor. The source-bound region schema
and validation reject missing, duplicate or unplanned region identities and
mismatched bounding boxes; a complete model response alone is not an accepted
OCR product. The default Document duration is 900 seconds, with explicit bound
profiles up to 3,600 seconds; Page limits remain eight tools, one turn and 180
seconds. Parent frontend/session and phase deadlines remain effective.

The selected standalone app-server derivative and private runtime qualification
are separate from the stock CLI reader/login route. Follow [setup](../../docs/development.md)
and the [derivative source guide](../../patches/codex-native-viewer/README.md).
Complete native page/metadata extraction and actual model inspection are reported
separately; complete-corpus OCR/index acceptance is still a live gate.

## Independent JSON completion

`complete_json(instructions, state, schema, config)` is a separate no-native-tools completion primitive for source pipeline adapters. It needs no index, adds no Jev stage or attempt ledger, and returns `CompletionReport.value` plus native provenance/usage. Its input budget defaults to 256 KiB with a 1 MiB hard cap; output is 128 KiB. Caller schemas are preserved and validated without external resolution or type coercion. Caller-schema tool selections or action proposals may be returned as JSON data for an outer executor; no native tool or external resource is invoked. This primitive makes no retrieval, citation, or semantic acceptance claim.

See the public [architecture](../../docs/architecture.md) and [portable reader profile](../../config/codex-host.toml). The profile is not automatically installed globally.

```sh
cargo test -p gptgrep-host
cargo clippy -p gptgrep-host --all-targets -- -D warnings
```

Tests use synthetic documents, in-memory Codex protocol transports, and loopback Jev HTTP fixtures. They do not call a paid provider.
