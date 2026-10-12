# gptgrep-jev

Trusted callers that already have an initialized same-host budget can use
`ExecutionContext::with_existing_ledger`. It adopts the original clock anchor
and reservations and refuses missing journal or lock files without initializing
a replacement. The existing `with_ledger` initializer remains available for the
owner of a new run; joining a ledger does not renew its deadline.

A Rust client for Jev's structured Decisions endpoint on OpenRouter. It uses the process's `OPENROUTER_API_KEY` and does not read credential files. The default selector is `openai/gpt-6-luna-decisions`. Explicit `typesafe/jev-1.13` remains text-only. Responses preserve the actual model identifier and must match the requested identifier or its supported observed canonical mapping: Luna `openai/gpt-6-luna-decisions-20261006`, legacy Jev `typesafe/jev-1.13-20260917`. Other explicit models require exact equality; unknown aliases fail validation, with no fallback.

```rust,no_run
use gptgrep_jev::{Candidate, JevClient};

# async fn example() -> anyhow::Result<()> {
let client = JevClient::from_env(None)?;
let result = client.rerank(
    "What is the retention period?",
    &[Candidate {
        id: "handbook:page-8:block-3".into(),
        text: "Records are retained for seven years after termination.".into(),
    }],
).await?;
assert_eq!(result.rankings[0].id, "handbook:page-8:block-3");
# Ok(())
# }
```

`rerank` asks one Score question per candidate, using the same four descriptive relevance levels. Its output score is the expected level divided by three, in 0..1. It is a comparable relevance measure, not a probability of answer correctness. A missing `confidence` remains `None`; missing usage remains JSON null. Results sort by descending score, then candidate ID. Empty input, duplicate candidate IDs, and oversized batches are errors.

`decide(state, questions)` accepts Noul, Choice and Score questions as `serde_json::Value`. State and instructions must be text, objects or arrays. Noul criteria, when supplied, need both true/false descriptions. Choice accepts 2–255 options; Score accepts 2–10 levels. These local constraints follow the useful vendor contract even where the gateway schema is looser. Responses use the public `DecisionAnswer` enum.

`prepare_decision(state, questions)` validates and serializes the complete request without network I/O or charging a submission. Its opaque `PreparedDecision` exposes the encoded byte count, SHA-256, question count and requested model. Both it and `PreparedRerank` offer an explicit `canonical_body()` borrow for trusted request-budget inspection. These bytes include document content, exclude authorization headers, and must not be written to public telemetry. Borrowing bytes does not clone the submission capability. `submit_prepared(prepared)` admits and sends those exact bytes once. A prepared request is bound to its originating client, model and endpoint; neither API silently splits or retries an oversized request.

An optional `DecisionObserver::send_started_with_body` callback borrows those exact
wire bytes after SDK admission and immediately before HTTP send. Trusted callers
can inspect internally prepared requests using the same admission boundary. The
default delegates to the existing metadata callback. A callback failure prevents
the HTTP send while preserving the charged reservation; a send-start observation
does not prove provider receipt. Keep source-bearing bytes out of generic logs
and retain unresolved liabilities if a request future is cancelled.

Submission has a 20-second admission/request timeout, a 64-question cap, a 64 KiB serialized input cap and a 1 MiB response cap. Client clones share four slots and a maximum of 200 charged submissions. These are local application bounds, not token counts or a billing guarantee. The client makes no application retries, follows no redirects and explicitly disables provider fallback. Callers own shortlisting, further batches, result-output reservation and any explicit local fallback.

For an experiment, create `ExecutionContext::new(run_id, plan_digest, absolute_deadline, max_submissions)` and attach it with `JevClient::with_execution_context`. The cap must be 1–200. All clones share the context; time limits use the earlier of the 20-second request limit, the wall deadline and a monotonic deadline captured at context creation. Legacy constructors retain their per-request deadline behavior. Without an explicit ledger the accounting scope is `single_invocation`; separate processes have independent contexts.

`context.with_ledger(path)` adds optional same-host coordination before the context is cloned. The ledger parent must exist. Every joining process must supply exactly the same run ID, plan digest, absolute deadline and limits. An OS file lock protects a bounded private JSON ledger; reservations and local send-start observations are synced before HTTP. Logical reservations permanently consume the submission allowance, including failures. Completed HTTP failures and fully received schema failures release their slots. Cancellation and uncertain transport outcomes retain their slots through run end. A crashed process leaves a charged reserved/in-flight slot that is also unavailable; no automatic recovery or slot reuse occurs. A new run uses a new ledger. There are no source bodies, image bytes, keys or provider response bodies in the ledger. `snapshot()` separates charged reservations, local send-start observations, terminal submissions and occupied/quarantined slots. These observations do not prove physical provider requests or billing.

`prepare_input(DecisionInput, questions, output_capacity)` is an additive typed evidence seam. `TextEvidence` binds its text and content hash to a `SourceBinding` (source ID, unit ID, raw-source digest). `ImageEvidence::from_bytes` and `from_file` admit an exact PNG/JPEG snapshot with a 1 MiB compressed/base64-decoded file cap, a 1,398,104-byte padded-base64 cap, 8 million pixels, and a 32 MiB decoded pixel-buffer limit. Input admission allows at most four images and 24 million total pixels; the multimodal envelope cap is 8 MiB. Source/unit pairs must be unique and one source ID cannot have conflicting raw-source digests. Typed preparation requires capacity for the entire 1 MiB response before submission.

Normal `prepare_input` uses the flat Luna Decisions codec: literal text strings,
then PNG/JPEG `image_url` data URL objects. Object keys match canonical JSON Value
encoding and the provider body excludes source metadata. The escaped text/questions
envelope stays within 64 KiB before image encoding; the complete body stays within
8 MiB. Explicit legacy or other models retain text-only evidence objects and reject
images before reservation. Raw `decide` remains the JSON structured-state API and
does not interpret arbitrary objects as images. `prepare_image_qualification`
remains a compatible explicit wrapper around the same codec. Supported syntax and
observed simple image controls do not establish general image or refusal semantics.
Unsupported answer types, including refusal/UNKNOWN wrappers, remain failures.

`ExecutionContext::from_budget_file` admits a portable, bounded 16 KiB
`gptgrep.decisions-budget-contract.v1` file with `run_id`, `plan_digest`,
`deadline_unix_ms`, `max_submissions`, and absolute `ledger_path`. It joins only an
existing matching ledger and retains the original clock and charged reservations.
It never initializes missing files or renews a deadline. Share the context through
`with_execution_context` before preparing requests; clones retain one allowance.
The CLI exposes this as `--decisions-budget FILE` for paid paths. Native Codex
execution and its budget remain separate. Without an explicit context, one client
invocation uses the bounded compatibility context.

Response validation rejects missing, unexpected or duplicate answer IDs, duplicate JSON object keys, wrong answer types, non-finite/out-of-range numbers, invalid choices, inconsistent probability keys and malformed usage. Optional gateway confidence/distributions/legend stay optional; supplied values are validated. No provider body, query, excerpt or credential is included in error text. After a transport failure, usage may be unknown.

An unsuccessful submission can be downcast to `DecisionFailure` for a serializable `diagnostic()`: the failure kind, observed HTTP status, retained response byte count, a fixed validation code and independently checked numeric usage fields. Usage distinguishes absent, null, invalid and reported values. Invalid answers remain rejected even when numeric usage can be retained; a reported cost is provider metadata, not a verified bill. Invalid JSON or duplicate keys cannot supply usage. The diagnostic stores no request or response contents, URL, headers or underlying exception, and does not authorize retrying a charged submission.

Rejecting oversized success headers or a partial response body happens before EOF, so it also quarantines the slot. A fully received invalid response releases its slot while remaining charged. The stable lock filename appends `.lock` to the complete selected filename; journal staging uses a uniquely created, owned temporary file. The lock stores and syncs the immutable initialization header before journal creation. Only the access that atomically creates the lock inode can initialize a journal; an existing empty header or a missing, malformed or mismatched journal fails closed. Bounded lock-contention waits retry only an unperformed journal action, never an ambiguous storage update or HTTP request.

Persistent ledgers retain the original run's uncapped time anchor. Fresh processes adopt it rather than deriving a new lifetime from their current wall clock. Every access checks the same boot and clock identity; Linux also binds the time namespace. The deadline uses the earlier of wall time and elapsed time from this original anchor, with the independent 20-second limit still applied to each request. Persistent clock support is implemented for macOS (`CLOCK_MONOTONIC_RAW`) and Linux (`CLOCK_BOOTTIME`) using source-verified clocks; other platforms explicitly reject persistent-ledger attachment. These clocks include time spent suspended. Changing the boot or namespace does not grant a fresh budget.

`JevClient::new(api_key, model)` supports an explicitly supplied key. `with_endpoint(api_key, model, endpoint)` supports compatible HTTPS endpoints and loopback HTTP test servers; it rejects URL credentials, query strings and fragments.

An explicitly loopback endpoint connects directly at the application proxy layer. Internet endpoints retain the configured proxy and OS-managed routing policy. Tests bind same-process `127.0.0.1` only and never call a model provider.

The wire contract was checked against [OpenRouter's Decisions schema](https://openrouter.ai/openapi.json), the [Jev model page](https://openrouter.ai/typesafe/jev-1.13), and [TypeSafe's API documentation](https://docs.typesafe.ai/api.md). Jev is a text-only decision model; it does not generate text. Unit and loopback transport tests require no API key and make no paid model calls.

```sh
cargo test -p gptgrep-jev
```
