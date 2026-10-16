# Rich batch composition

For source build, rich generation creation, native runtime setup and provider
credentials, start with [development setup](development.md). A batch consumes an
existing pinned generation; it does not start ingestion or OCR.

`gptgrep batch --rich --root DOCUMENTS --input operations.json --json` accepts
`gptgrep.batch.rich.request.v1`. Local operations keep their existing behavior
and cursors. `inspect_native_metadata` adds bounded page-object, path, image,
character and word inspection; see [native page metadata](native-metadata.md)
for discovery and pagination. Explicit `search_multimodal` performs mandatory document
routing followed by source-bound text and independent visual reranking. It uses
the normal product search, with no local fallback or inference cursor.

```json
{
  "schema_version": "gptgrep.batch.rich.request.v1",
  "generation": "CURRENT",
  "budget": {"max_output_bytes": 1048576, "max_decisions_submissions": 8},
  "operations": [
    {"id": "catalog", "op": "inspect_documents", "page_size": 10},
    {"id": "rank", "op": "search_multimodal", "query": "How are service limits described?", "window_bytes": 1024, "max_visual_candidates": 8, "max_concurrent_batches": 4, "min_score": 0.5}
  ]
}
```

Paid operations require `OPENROUTER_API_KEY`; `--model` defaults to
`openai/gpt-6-luna-decisions`. `--decisions-budget FILE` joins an existing finite
context with its original clock and consumed allowance. Without that option,
the CLI creates one finite invocation context. `--multimodal-timeout` defaults
to 180 seconds, can be 1..900, and only narrows an existing deadline. Local-only
batches do not look up credentials or open a Decisions context.

Input/output remain at most 1 MiB with 1..64 unique operation IDs. Complete
output shares are fixed in input order before execution. Before the first submit,
every paid operation prepares its complete request bounds: routing and full-pool
reranking for search, or supplied-pool scoring for explicit rerank;
insufficient result space or a complete call share is an explicit item error
with no submission. `max_decisions_submissions` defaults to 8 and can be 0..200;
its deterministic worst-case allocations also fit the parent's remaining
allowance. Failed/unsent allocations are never borrowed by another item. The
parent SDK still performs its durable admission and enforces aggregate slots
and submissions. The preflight remaining check is not an atomic reservation
against unrelated concurrent callers.

Operations execute serially; each normal search uses up to four concurrent
request groups and drains its owned in-flight requests on failure. Prepared
operations share one pinned reader/cache/client/context. Conservative retained
preparation admission is at most 256 MiB; active per-search response/decoder
work, reader cache, native allocation and process RSS have separate coverage.
CURRENT changes cannot redirect the reader. Sources are rechecked by normal
search before/after model exposure and before result return. Evidence is model
exposure only and grants no reader citation rights.

Results stay in input order. Invalid items are errors; required paid failure
retains partial observations, stops later paid items, and lets independent local
items continue. A deadline stops remaining work. Shared integrity failure returns
the accumulated ordered items and available accounting. Missing rows are
`not_executed`, not omitted successes. The `rerank` operation below accepts only
source-bound references; raw caller-provided text or images remain unsupported.
The `search_multimodal` operation above performs routing plus reranking.

Local `model_submissions` remains `0`. Paid exact batch-attributable
`model_submissions` is `null`; `scheduled_logical_attempts` includes futures that
can fail before SDK submission. One shared before/after snapshot reports SDK
logical reservations, submission/send-start observations, and quarantined slots.
Other users of that context may contribute to those cumulative snapshots. No
counter proves physical provider requests.

`batch_attempt_id` binds operation ID, stage and group ordinal within this batch.
Its explicit scope is this batch response; repeating an identical batch may
produce the same IDs. These are not SDK/native or cross-batch unique identities.
Equal request hashes remain separate attempts. Batch cost totals deduplicate
those identities; allocations are labelled separately and never counted as
observed billing. Returned token counts are bounded integers. Cost is a labelled
lossy finite-f64 projection, with original complete-usage SHA retained when
observed; arbitrary extra usage metadata is omitted. Failure diagnostics keep
the SDK's safe typed usage observations. Missing/invalid cost stays null, and
the subtotal is not an invoice or evidence of complete billing. A non-finite
sum remains unavailable. Required hit identities are never clipped after a paid
call to fit another item's share.

Source preparation and offline tests do not qualify provider image semantics,
actual native reader delivery, full harness utility, or release acceptance.

Library callers can retain `PreparedRichBatch` and inspect its borrowed
`prepared_searches()` routing/full-pool requests before execution. Inspection
does not submit a request or grant delivery/citation rights.


Explicit source-bound rerank is also a paid operation:

```json
{"id":"rank","op":"rerank","query":"What evidence addresses the question?","candidates":[{"id":"chosen-window","evidence":{"kind":"native_text","binding":"COPY_COMPLETE_BINDING_FROM_READ_RESULT"}}],"max_concurrent_batches":4}
```

The placeholder represents the complete typed `EvidenceRef` from a retained
read/search/visual result; it is not a valid binding by itself. `rerank` accepts
1–64 unique labels/references and resolves every exact artifact, range, lane and
geometry on the same reader. It scores only this supplied pool and has no routing
stage, inference cursor or implicit dependency on an earlier operation. Its
result is `gptgrep.rich_rerank.response.v1`, distinct from search coverage. The
normal `search_multimodal` member retains mandatory routing. Both use fixed item
output/call shares before any submission. Preparation refusals are isolated item
errors; an execution failure in an admitted paid member stops later paid members
and leaves independent local operations available. See
[source-bound rerank](rerank.md) for budgets and evidence validation.
