# Source-bound rerank

This requires an existing complete rich generation and the independently supplied
Decisions credential; [development setup](development.md) covers both prerequisites.

`gptgrep rerank --root ROOT --input FILE_OR_DASH --json` scores only explicitly
supplied evidence from one published rich generation. It is a paid Decisions
operation, using the supported Luna profile and the same relevance rubric as
multimodal search. Ordinary search still requires document routing. This separate
frontdoor skips routing because the caller has supplied the complete pool.

Input uses `gptgrep.rich_rerank.request.v1` with `query`, optional `generation`
(`"CURRENT"` or an exact generation/manifest pin), `candidates` and optional
`budget`. Each candidate is exactly `{id,evidence}`. Copy the full native/OCR text
or visual `EvidenceRef` from a reader result. Labels are output-side identifiers;
they are never used as model content or routing hints. No caller text, titles,
scores, image bytes, image URLs, dependencies or cursors are accepted.

The reader resolves CURRENT once, verifies the full reference against regenerated
metadata and retrieves stored content. Native and OCR lanes remain distinct; OCR
region/profile/receipt fields, source/artifact hashes, exact UTF-8 byte ranges and
visual bounding box/affine fields must all match. The same source and generation
are checked before submissions and after responses. A reference from another
pin, stale source, corrupt artifact or altered geometry fails explicitly.

Limits are 1 MiB input and output, 1–64 unique nonempty IDs of at most128 UTF-8
bytes, query at most8192bytes, nonempty text windows of at most2048bytes and at
most16 visual candidates. The metadata budget defaults to524288bytes. Each image
must satisfy the existing 1MiB/8Mpixel bounds; groups admit at most4 images and
24M total pixels under the existing8MiB request cap. Visual reading additionally
requires its complete local rendition/base64 envelope to fit the existing1MiB
visual-output cap; large admissible source images can therefore be refused.
There is no partial image or implicit text truncation. Cache-only visual rerank
is unavailable because the reader has no rendition cache.

`budget` supports `max_output_bytes` (4096..1048576), `max_metadata_bytes`
(1..524288), `max_decisions_submissions` (0..200, default8),
`max_concurrent_batches` (1..4, default4) and the existing reader `cache` policy.
The complete output and worst-case calls must fit before any submission. Owned
buffer admission is bounded by the stored resource budget and256MiB; it is not a
physical RSS guarantee. `--multimodal-timeout` (1..900seconds, default180) narrows
the invocation. `--decisions-budget FILE` joins an existing original-clock context
without initializing/resetting it. Requests use one preparing client and no
retry, fallback or renewed deadline.

Successful output is `gptgrep.rich_rerank.response.v1` with `ranked_candidates`,
exact evidence, score normalized as score/3 and optional confidence. All supplied
candidates are scored; ties use the intrinsic source-reference identity. Literal
matching does not boost scores on this path. Coverage is explicitly
`supplied_candidate_pool_only`; it does not claim document/corpus completeness.
No initial routing-pass capability, inference continuation, actual reader delivery
or citation authority is created.

A required failure returns the same schema with a static `code`/`message` and,
after execution starts, available partial report/attempts. In-flight owned
requests are drained. Actual prepared-body hashes and validated response metadata
are retained; scheduled attempts are distinct from SDK reservation/send-start
observations and physical provider counts remain unknown. Usage projects bounded
numeric fields and retains the complete genuine usage SHA/accessor; finite-f64
cost is lossy, not an invoice. Missing/null/invalid observations are distinct.
Shared context before/after counters can include unrelated callers.

The rich batch `rerank` operation accepts `id`, `query`, `candidates` and optional
`max_concurrent_batches`. It prepares all paid members in input order on one
reader/client and fixes every complete output/call share before the first send.
It cannot borrow unused later shares. A preparation refusal is an item error;
other admitted items may continue. An execution failure in an admitted paid
operation stops later paid operations, while independent local operations may
continue. Item results remain
the distinct rerank schema with batch-response-local attempt identities. The
standalone and batch source tests establish custody/admission behavior; actual
provider relevance, image influence and reader citation qualification remain
separate live evidence.

The optional Node wrapper accepts a nested `rerank` request and forwards refs
through bounded native stdin. Its `--request` JSON argument is additionally
subject to the platform's command-line argument limit; use the native file/stdin
frontdoor for large inputs. No wrapper process is started if that OS limit is
exceeded.
