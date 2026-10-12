# Experimental development status

Updated **2026-10-12 UTC**. This is a public, aggregate development checkpoint for
the experimental source branch `codex/gptgrep-experimental-20261012` in
[`lwyBZss8924d/gptgrep`](https://github.com/lwyBZss8924d/gptgrep).
Use the checked-out commit and its validation results when comparing changes.
The branch is a collaboration snapshot, not an accepted first release.

## Goal and current cursor

The goal is a complete local agent search harness: whole-document parsing in
bounded batches, native multimodal OCR, source-bound document trees and tgrep,
Decisions routing/reranking, and actual Codex retrieval. The selected comparison
is **GPTgrep + Codex versus the same bare Codex reader**, over 62 tasks and 34
documents, producing 124 paired SEARCH trials. INDEX preparation is excluded
from the primary SEARCH reward and cost and reported separately.

The current cursor is **repair and validate native parse/OCR/index → complete the
frozen index → run paired SEARCH → Judge → analyze failures → next Opt**.
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
| D3 | Source-bound rich parsing and v2 artifact manifest | **Active.** Earlier parsing criteria remain accepted; complete native viewer/Document-agent/OCR/index integration and the 34-document generation still require acceptance |
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
| 2026-10-12, latest full34 attempt | Terminal failure after 750.688 seconds: ten complete OCR pages, one partial required region and eleven terminal OCR reservations; no generation published | Required content remains incomplete; a new 200-DPI single-page experiment tests a generic 100-DPI legibility hypothesis, not an accepted fix |
| 2026-10-12, current Python wrapper round | All 80 full62 wrapper tests passed; the wrapper generates and verifies all 124 tasks using the selected seed, randomized pair order and within-pair arm order | Source evidence only; the actual full62 export, SEARCH and Judge remain pending, and G5 remains HOLD |
| 2026-10-12, private Decisions integration | Per-trial preparation and request-ID isolation repair received scoped source acceptance; the actual private Linux build and 13 tests passed | Package qualification and actual Ask remain pending; source acceptance does not establish live integration or full62 acceptance |

The next Opt step is to test the image-legibility hypothesis and validate the
complete native document pipeline, publish the complete frozen index, then
execute all 124 SEARCH trials and their separate Judge stage.

## Recent checks and retained failures

The earlier integrated source round passed **986 Rust tests**, with **0 failed**
and **4 ignored**, plus formatting and Clippy with warnings denied. Its frozen CLI
release build passed **11 offline evaluation cases**. That round's full62 Python
wrapper suite passed **71 tests**, and its Node wrapper passed **25**, with
**1 skipped** because the optional real-native metadata fixture was absent.
An older integrated round passed **977 Rust tests** and **11 offline evaluation
cases**; its receipts remain historical.

The current Python wrapper round passed **all 80 tests**. Its paired-order
implementation retains all **124 tasks**, randomizing the 62 pair blocks and
each pair's arm order using a seed selected before outcomes, with one concurrent
trial and no retries. The actual full62 export and concrete task sequence still
need to be generated and verified before launch. The private
per-trial Decisions preparation and request-ID isolation repair received
independent acceptance within its source scope. Its actual private Linux build
and **13 tests** passed; package qualification and an actual Ask remain pending.
These results are separate from the earlier Rust, offline and Node checks and do
not establish complete SEARCH/Judge or live retrieval acceptance.

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
OCR reservations**. It published **no generation**. A new single-page experiment
at **200 DPI** is testing the generic hypothesis that **100-DPI** input limits
image legibility. This hypothesis is not an accepted fix, and the original
131-second region-ID failure remains in the retained history. The complete
full62 SEARCH/Judge comparison remains pending; **G5 remains HOLD**.

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
