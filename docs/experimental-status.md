# Experimental development status

Updated **2026-10-12 UTC**. This is a public, aggregate development checkpoint for
the unified runtime source branch `codex/gptgrep-unified-runtime-20261012` in
[`lwyBZss8924d/gptgrep`](https://github.com/lwyBZss8924d/gptgrep).
Use the checked-out commit and its validation results when comparing changes.
The branch is a collaboration snapshot, not an accepted first release.

## Goal and current cursor

The core optimization goal is the actual **GPTgrep harness versus bare Codex
ablation study**: the same Codex reader over **62 tasks and 34 documents**,
producing **124 paired SEARCH trials**, followed by Judge and failure analysis.
INDEX preparation is excluded from the primary SEARCH reward and cost and
reported separately. Whole-document parsing, native PTC OCR, document trees,
tgrep and Decisions routing/reranking support this comparison; component
completion does not complete the goal.

The current cursor is **repair and validate native parse/OCR/index → complete the
frozen index → run paired SEARCH → Judge → analyze failures → next Opt**.
The required parsing/OCR workflow is full native PTC coding with native built-ins,
shell, `view_image`, CV and Skills through the available GPTgrep, tgrep and lit
CLIs/APIs, without requiring a fixed sequence of GPTgrep tool calls.
The native Document and OCR paths now use caller-selected native capabilities,
permissions and job budgets, with a shared runtime package staged into each
actual working directory. The retained fixed Page/Document guard checks and
qualifications cover scoped components only; new fixed-image qualifications are
not prerequisites for the mainline PTC work.
Job resource choices belong in configuration, while authentication, sandbox and
evaluator custody remain enforced.
The full comparison has not run. No measured retrieval advantage is claimed.
The independent PageIndex G5 first-release gate remains **HOLD**; its thresholds
are unchanged in [SPEC](../SPEC.md).

The original D1–D7 goal objects retain the following states. A completed component
keeps its accepted scope; it does not complete the broader document pipeline or
establish a retrieval advantage.

| Goal | Required outcome | State and evidence scope |
| --- | --- | --- |
| D1 | Pinned embedded engine with exact regex parity | **Done, scoped.** tgrep 1.1.0 source/license identity, indexed/direct parity and explicit rebuild boundary accepted |
| D2 | Typed concurrent Decisions with verified image transport | **Done, scoped.** Text/image transport, wire contract, shared accounting and failure handling accepted; provider failures and unknown usage retained |
| D3 | Source-bound rich parsing and v2 artifact manifest | **Active.** Earlier parsing criteria remain accepted; full native PTC coding for parsing/OCR, complete index integration and the 34-document generation still require acceptance |
| D4 | Explicit OCR completes all required content before publication | **Done, scoped.** OCR contract and component acceptance retained; every required task must still succeed before a new full generation can publish |
| D5 | Generation-pinned batch operations with shared reader | **Done, scoped.** Stable batch order, item/global errors, bounded output, cursors and shared ledger behavior accepted |
| D6 | Visual recall, reranking and delivered evidence citations | **Active.** Independent visual retrieval, source-bound crops and reader citation checks implemented; current live integration and utility remain to be established |
| D7 | Bounded controlled evaluation and independently reviewed source delivery | **Active.** Harbor adapter, common raw-document tools, separate verifier and paired aggregation implemented; complete 62-pair SEARCH/Judge, review and delivery still required |

The requested native reader profile is `gpt-6-luna` / `max` / `fast`; requested and
observed profiles remain distinct. Complete-document batching has no policy cap
on the document's total page count. Resource admission may refuse a build, and
any required content failure prevents publication of an incomplete generation.

## Public SELF-PM checkpoint log

Private checkpoints retain exact source revisions, run identities, receipts and
historical verdicts. This compact projection records their aggregate outcomes.

| Date (UTC) | Completed observation | Cursor and outstanding gate |
| --- | --- | --- |
| 2026-10-11 | PNG diagnostic observed 16 required OCR tasks and zero completed OCR; no generation published | Retain the failure; prepare and validate native image/parse controls |
| 2026-10-11, earlier source round | Integrated source checks passed: 977 Rust tests, 4 ignored; formatting, Clippy, 11 offline cases and 71 wrapper tests | Source delivery recorded; broader live acceptance remains separate |
| 2026-10-11, native components | Page image qualification and Document image contrast checks received scoped acceptance | Full document parse/OCR/index still required |
| 2026-10-11, historical full34 attempt | OCR response contained eight unplanned region IDs for one planned region; terminal failure after about 131 seconds | Original failure retained; later region-binding acceptance does not qualify a complete generation |
| 2026-10-12, earlier integrated source round | Integrated source checks passed: 986 Rust tests, 4 ignored; formatting, Clippy, 11 offline cases, 71 full62 wrapper tests and 25 Node wrapper tests, with 1 skipped. CLI release build completed | OCR and Decisions source changes received scoped independent review; complete native document integration remains active |
| 2026-10-12, native Page OCR | Fresh live run passed 2/2 synthetic image cases in about 81 seconds; original region IDs and bounding boxes matched exactly | Independent review accepted image delivery, region binding and paired image influence only; full34/full62 acceptance remains pending |
| 2026-10-12, latest full34 attempt | Terminal failure after 750.688 seconds: ten complete OCR pages, one partial required region and eleven terminal OCR reservations; no generation published | All 34 documents remain unpublished; the next evidence must exercise the full native PTC parsing/OCR workflow and configured image resources |
| 2026-10-12, earlier Python wrapper round | All 80 full62 wrapper tests passed; the wrapper generates and verifies all 124 tasks using the selected seed, randomized pair order and within-pair arm order | Source evidence only; the actual full62 export, SEARCH and Judge remain pending, and G5 remains HOLD |
| 2026-10-12, configured image resources | Workspace checks passed 994 Rust tests with 4 ignored, formatting, Clippy and 11 offline evaluation cases. The CLI and qualification probe built, and the patched native guard built with 23 native tests passing | Scoped fixed-guard source and synthetic validation only; full native PTC parsing/OCR remains active, and earlier live qualifications do not qualify the new tuple |
| 2026-10-12, direct native extraction | One complete PDF at 200 DPI produced all 16 PNG pages under the configured 4 MiB per-image limit: largest 2,976,169 bytes, total 30,425,938 bytes; no OCR invoked | Confirms parsing and encoded-image admission for this source; OCR legibility and complete-document acceptance remain pending |
| 2026-10-12, private Decisions integration | Per-trial preparation and request-ID isolation repair received scoped source acceptance; the private Linux build and 13 tests passed. A current-source Ask received two real known-settled Decisions responses, totaling $0.0001347 | Native reader permission projection failed before `turn/start`; Ask, complete SEARCH/Judge and full62 remain unaccepted |
| 2026-10-12, unified native runtime source | Formatting, Clippy, 1,024 Rust tests with 4 ignored, 142 evaluator tests and 11 offline cases passed. The repaired full62 wrapper passed all 83 tests; five staging scenarios and the direct Rust API example passed | Source and synthetic checks only; SDK package consumer validation, complete index, paired SEARCH/Judge and source delivery remain separate |

The current source round unifies native Document/OCR execution, configuration,
the [runtime package](../runtime/codex/STAGING.md), the
[incur CLI](../packages/cli/README.md), and the [public Rust APIs](rust-sdk.md).
The package contains four Skills and three native roles; the caller selects its
root and pins its manifest. GPTgrep stages it into each actual Document/OCR
working directory, checks exact enabled Skills through native discovery, and
binds the role configuration files explicitly. Skill availability and submitted
Skill inputs remain distinct from observed body use. The CLI exposes 21 native
operations plus its discovery helpers, including foreground parse-agent and OCR
commands. The Rust example composes index, search and node reading directly.
Source checks and package consumer evidence are recorded separately from the
pending complete 34-document index and all 124 SEARCH trials with their separate
Judge stage. The original experimental branch remains a historical source cursor.

## Recent checks and retained failures

The unified native runtime round passed **1,024 Rust tests**, with **0 failed**
and **4 ignored**, plus formatting and Clippy with warnings denied. Its frozen
native CLI passed **11 offline evaluation cases** with **zero provider requests**,
and the evaluator's own **142 tests** passed. The direct Rust API example built
and ran two composed local queries with verified citations. The runtime staging
regression passed fresh staging, existing-file refusal and three deterministic
late path-replacement cases. The incur wrapper's separate producer suite passed
**37 tests**, with **1 skipped** for its optional real-native metadata fixture.

The full62 Python wrapper suite passed **all 83 tests**. The subsequent runtime
package integration passed **10 focused bundle-reader tests** and **7 package
self-tests**. A retained replay confirms that the earlier reader rejected the
new producer's runtime metadata before creating output; the repaired reader
accepts the complete pinned assets and rejects detached metadata. These focused
checks do not constitute a new full-wrapper run.
An earlier
82-test run failed in two timeout/cancellation subcases when macOS rejected a
second process-group signal after termination. The repair requires bounded child
reaping even when signalling fails and reports cleanup failure explicitly; the
original failure remains retained. Earlier compilation and synthetic-fixture
failures in this source round remain in the private validation history.

The earlier integrated source round passed **986 Rust tests**, with **0 failed**
and **4 ignored**, plus formatting and Clippy with warnings denied. Its frozen CLI
release build passed **11 offline evaluation cases**. That round's full62 Python
wrapper suite passed **71 tests**, and its Node wrapper passed **25**, with
**1 skipped** because the optional real-native metadata fixture was absent.
An older integrated round passed **977 Rust tests** and **11 offline evaluation
cases**; its receipts remain historical.

An earlier Python wrapper round passed **all 80 tests**. Its paired-order
implementation retains all **124 tasks**, randomizing the 62 pair blocks and
each pair's arm order using a seed selected before outcomes, with one concurrent
trial and no retries. The actual full62 export and concrete task sequence still
need to be generated and verified before launch. The private
per-trial Decisions preparation and request-ID isolation repair received
independent acceptance within its source scope. Its actual private Linux build
and **13 tests** passed. A current-source Ask received **two real known-settled
Decisions responses**, totaling **$0.0001347**, but the native reader permission
projection failed before `turn/start`. This does not establish accepted Ask or
complete SEARCH/Judge. These results are separate from the earlier Rust, offline
and Node checks.

The configurable image-resource round passed **994 Rust tests**, with **0 failed**
and **4 ignored**, plus formatting and Clippy with warnings denied. Its frozen
CLI passed **11 offline evaluation cases** with **zero provider requests**.
The focused parser and host/CLI results are included in that workspace count;
they are not additional tests. The normal CLI and native Page qualification probe
built, and the patched native guard built and passed **23 native tests**. The
parser's encoded image-file, base64 and per-page capacities
now come from the existing parse-agent control; native Page and Document profiles
bind the selected limits through admission and readiness evidence. The previous
1 MiB value remains the default. Decisions and the HTTP `/ocr` compatibility
endpoint retain their separate 1 MiB image contracts. See the
[configuration example](architecture.md#prepared-document-agent). These focused
fixed-guard checks do not carry forward earlier live qualification to the new
binary/profile tuple or establish the required full native PTC parsing/OCR.

The first full-workspace run encountered a busy OCR ledger lock. A direct
filesystem experiment showed that an inherited descriptor could retain the
lock after the parent closed its descriptor. The repair explicitly unlocks the
ledger on scope exit; independent source review, a focused inherited-descriptor
regression and the subsequent full suite passed. Inheritance as the cause of the
original failure remains an inference. The initial failure is retained.

The historical full34 region-ID attempt failed during `execute_ocr` after about
**131 seconds**. The task planned **one region**, but the native response supplied
**eight unplanned region IDs**. A completed model response therefore did not
produce accepted OCR or an index generation. The repair binds the planned region
ID and bounding box in the output schema and preserves result validation and
bounded failure diagnostics. A fresh live Page OCR run observed **2/2 synthetic
cases passing**, with one native image and two tool calls per case, two charged
attempts, two terminal outcomes and no active attempts. Independent review
accepted original-region binding, native image delivery and paired image
influence for those cases. Each case planned one region; live multi-region
behavior remains unverified. This does not qualify the complete 34-document
index or the paired SEARCH experiment.

The latest full34 attempt failed after **750.688 seconds**. It completed OCR for
**ten pages**, left **one required region partial**, and recorded **eleven terminal
OCR reservations**. It published **no generation**; all **34 documents** remain
unpublished. A bounded higher-resolution extraction isolated the encoded PNG
writer's 1 MiB cap as its failure, after raster geometry admission. With the
configured 4 MiB capacity, direct native extraction of **one complete PDF at
200 DPI** produced **16/16 PNG pages**, with a largest image of **2,976,169 bytes**
and **30,425,938 total bytes**. No OCR or model ran in that extraction check.
The generic hypothesis that **100-DPI** input limits OCR legibility remains
unproven; actual native PTC OCR qualification and the selected 200-DPI experiment
remain pending. The original 131-second region-ID failure remains in the retained
history. Complete full62 SEARCH/Judge remains pending; **G5 remains HOLD**.

Earlier image, OCR timeout, parsing, source-review and build failures remain in
the private experiment history. This document publishes aggregate outcomes only;
private run identities, account fixtures, raw answers, benchmark questions/gold,
machine paths, financial ledgers and PoUW notes are not distributed.

## Working on a competing improvement

Start with [development and evaluation setup](development.md), then select a
general defect or falsifiable improvement. Preserve the original contracts and
report the exact commit, commands, profile, complete task denominator and observed
failures. Keep synthetic regressions separate from live evidence. Changes must
not recognize benchmark filenames, questions, answer formats or evidence pages.

The published source contains runtime contracts and templates. It does not ship
the owner's corpus, accepted native controls, device credentials or evaluation
admission. A collaborator supplies and validates their own authorized inputs.
Small diagnostics are useful for repair; they do not replace the complete paired
comparison or qualify a release tag.
