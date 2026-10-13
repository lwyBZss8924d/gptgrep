# Experimental development status

Updated **2026-10-13 UTC**. This is a public, aggregate development checkpoint for
the experimental source branch that contains this file in
[`lwyBZss8924d/gptgrep`](https://github.com/lwyBZss8924d/gptgrep); Claude's
experimental Opt-loop branch is `claude/exp-ablation-opt-loop-20261013`. The earlier
`codex/gptgrep-unified-runtime-20261012` branch is the previous source cursor.
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

The reader images are rebuilt, the zero-model Linux text index is built and pinned
inside the treatment image, and the adapter carries the approved reader-only sandbox
setting. The current cursor is **remove the bare-arm copy of GPTgrep's answer rules,
bound Decisions submissions per trial and apply the adapter's open review fixes →
freeze the inputs on a published commit → Judge-container check and paired smoke →
complete full62 paired SEARCH → Judge → analyze failures → next Opt**. The first paired run
is prepared in the adapter's `legacy_text` mode with a frozen zero-model text
index, built inside the Linux treatment image before question exposure. Native
PTC parsing/OCR (D3) continues in parallel for later rich generations.
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
| D2 | Typed concurrent Decisions with verified image transport | **Done, scoped.** Text/image transport, wire contract, shared accounting and failure handling accepted; provider failures and unknown usage retained. This round's additions (alias-aware admission and text-only image refusal, `decisions-budget init`/`status`, judge failure accounting, and the bounded transient retry for text routing/reranking) were reviewed and tested offline; the live retry has not yet been exercised |
| D3 | Source-bound rich parsing and v2 artifact manifest | **Active.** Earlier parsing criteria remain accepted; full native PTC coding for parsing/OCR, complete index integration and the 34-document generation still require acceptance |
| D4 | Explicit OCR completes all required content before publication | **Done, scoped.** OCR contract and component acceptance retained; every required task must still succeed before a new full generation can publish |
| D5 | Generation-pinned batch operations with shared reader | **Done, scoped.** Stable batch order, item/global errors, bounded output, cursors and shared ledger behavior accepted |
| D6 | Visual recall, reranking and delivered evidence citations | **Active.** Independent visual retrieval, source-bound crops and reader citation checks implemented; current live integration and utility remain to be established |
| D7 | Bounded controlled evaluation and independently reviewed source delivery | **Active.** Harbor adapter, common raw-document tools, separate verifier and paired aggregation implemented. A treatment-only host pilot and the Opt-1 measurements ran under a frozen judge as development evidence. Complete 62-pair SEARCH/Judge, review and delivery are still required |

The requested native reader profile is `gpt-6-luna` / `max` / `fast`; requested and
observed profiles remain distinct. Complete-document batching has no policy cap
on the document's total page count. Resource admission may refuse a build, and
any required content failure prevents publication of an incomplete generation.

## 2026-10-13 UTC round

This round added a second development host and a shared-credential supervisor.
It measured a treatment-only development pilot and two MMLongBench-Doc-V2
development cohorts, and merged one measured optimization (Opt-1). The paired
GPTgrep-versus-bare-Codex comparison has **not run**. No advantage is claimed and
**G5 remains HOLD**. The source version remains `0.1.0-alpha.1` (experimental);
no tag or binary release was created.

### Goal objects and progress

- Main Goal (same-Codex full62 paired SEARCH): **not run**; harness preparation
  is in progress.
- D1, D4 and D5 remain done (scoped). D2 is done (scoped) with this round's
  additions. D3, D6 and D7 remain active (see the goal table above).
- G5 first-release gate: **HOLD**.

### Current cursor

Done in this round: common and treatment images rebuilt, the zero-model text index
built and pinned inside the Linux treatment image before any question exposure, and
the approved reader sandbox setting integrated. Next: remove the raw-only
answer-rules copy, bound Decisions submissions per trial and apply the adapter's
open review fixes → freeze inputs on a published commit → check the Judge/verifier
containers → paired smoke → complete full62 paired SEARCH in the
adapter's `legacy_text` mode → Judge → failure analysis → next Opt. In parallel,
Opt-1b removes or conditions the answer-commitment rule and re-measures all three
cohorts, and native PTC parsing/OCR (D3) continues.

### Branch and evidence

This section describes the experimental branch at the commit that contains it.
The public contracts behind it are the [evaluation protocol](design/agentic-search-evaluation.md),
the [full62 adapter](../workspace/harness-tooling/harbor-ablation/full62/README.md)
and the [credential supervisor](../workspace/harness-tooling/codex-auth/README.md).
Run identities, logs, ledgers, per-task answers and judgments stay in private
records; only their aggregates appear here.

### Completed checkpoints

**Second development host.** An environment preflight on a macOS Intel (x86_64)
host recorded 20 pass, 2 partial and 1 blocked check. The host's Codex CLI
0.162.1 binary was verified against the official release. Two checks were
partial: container-level tool isolation (host-level sandbox denial probes passed)
and cross-host test parity (only this host's results count for this round). The
blocked check was the Harbor reader container sandbox, which needs namespace
system calls and rebuilt images. The project owner has since approved a
reader-only seccomp profile: Docker's default profile plus the namespace and
mount calls the Codex sandbox needs. Judge and verifier containers keep the
default profile. The first probes passed. Adapter integration, the
Judge/verifier check and a paired smoke are pending and are not part of this
source revision.

**Credential supervisor.** [`codex-auth`](../workspace/harness-tooling/codex-auth/README.md)
1.3.1 lets several local consumers share one ChatGPT login. It provides a lease,
one refresh owner, a per-run `CODEX_HOME`, a port-0 refresh/revoke blocker,
generated tool-command permission profiles, a consumer argv guard and
credential-link scans that now fail closed. With fake credentials and loopback
fakes it passed 125/125 guarded and 13/13 unguarded qualification tests, and 18
leak scans found nothing. The 1.3.1 source build ran a three-task live workload
smoke and the Opt-1 measurements below. The pilot used an earlier build, and one
live owner refresh was observed with 1.3.0.

**Host development pilot (treatment arm only).** GPTgrep ran all 62 full62 tasks
with `ask --native-ptc`, protocol-provided document scope and concurrency 3. The
treatment profile was:

- a zero-model text index built before question exposure, with no OCR or visual
  evidence;
- mandatory Decisions routing and reranking;
- the product's 12-call tool budget.

The reader requested `gpt-6-luna`/`max`/`fast`. The runtime reported
`gpt-6-luna`, `max` and tier `priority`, which is Fast's protocol normalization;
the served profile is not independently verified. Decisions requested
`openai/gpt-6-luna-decisions` and was served
`openai/gpt-6-luna-decisions-20261006`.

A frozen MMLongBench-Doc-V2 answer-equivalence judge prompt and schema, run
tool-free with a requested Codex `gpt-6-luna`/`max`/`fast` profile, scored **59/62**: 2 judged
incorrect, 1 failed run and 0 judge unknowns. The failed run was a provider HTTP
502 during initial evidence reranking, before the bounded retry existed. The run
made 182 Decisions requests with $0.0696 known Decisions cost. Median product
latency was about 21 seconds, and no native tool calls were observed.

This run has no bare-Codex arm. It is development evidence, not the paired
comparison or a G5 attempt. The PageIndex reference result (60/62) used a
different model, judge and harness.

**Development cohorts outside full62.** Two MMLongBench-Doc-V2 cohorts were
selected by metadata, excluding the 62 full62 questions: H-text (evidence only in
text or layout text) and H-unans (reference marked not answerable). The
zero-model text index can ingest 107 of the 134 documents; the other 27 have no
searchable native text and need OCR. On those 107 documents, H-text has 98 tasks
and H-unans has 152.

These are question-level holdouts, not grouped by document: 42 H-text tasks and
39 H-unans tasks use full62 documents. After this round's measurements they are
development material, not unseen confirmation. Pre-Opt-1 baselines:

- H-text: **60/98**, with 12 failed runs.
- H-unans: **97/152** correct declines, with 31 over-commitments and 22 failed
  runs (17 of them tool-budget exhaustion).

The source documents are research-use data and are not redistributed.

**Opt-1.** Opt-1 adds the bounded transient Decisions retry, the single-document
routing skip, flagged below-threshold seed candidates, rarity-ordered seed terms
and two reader answer rules; see [retrieval modes](architecture.md#retrieval-modes)
and [local reasoning](architecture.md#local-pageindex-style-reasoning). It was
measured on a pre-merge integration build with the same frozen indexes, tool
budget, concurrency and judge:

| Cohort | Before | Opt-1 | Change |
| --- | --- | --- | --- |
| full62 | 59/62; 1 failed run | 59/62; 0 failed runs | +0 |
| H-text | 60/98; 12 failed runs | 62/98; 7 failed runs | +2 |
| H-unans | 97/152; 31 over-commitments; 22 failed runs | 87/152; 46 over-commitments; 16 failed runs | −10 |

Failed runs fell in every cohort, and full62's known Decisions cost fell from
about $0.070 to $0.060. The net change outside full62 is −8. The leading
hypothesis for the unanswerable-question regression is the answer-commitment
rule, possibly combined with the flagged fallback seed; Opt-1b tests it.

No Decisions provider failure or retry occurred in these runs, so the live retry
path has not been exercised. The merged source adds later review fixes,
including a retry-accounting fix, that these runs did not exercise. Known
Decisions cost across this round's live runs is about $1.01; some failed runs'
partial costs are unknown.

**Merged-source qualification.** On the Intel host:

- Formatting, Clippy with warnings denied and the release build passed.
- **1,109 Rust tests** passed, with 0 failed and 4 ignored.
- The evaluator's 145 tests passed.
- 11 offline evaluation cases passed with zero provider requests.
- The runtime staging regression passed.
- The full62 Python wrapper passed **all 90 tests**.

Under heavy host load two suites were not green; their failures are retained.
The separate harbor-ablation harness Cargo workspace had 208 passed and 3 failed
timing/deadline-sensitive tests. The incur wrapper had 36 passed, 1 failed and
1 skipped: a 1,200 ms timing assertion took about 1.9 to 2.1 seconds. Rerun at
lower load on the final source, the incur wrapper passed 37 tests with 1 skipped
(the timing assertion took 1.18 seconds, a narrow margin), and the harness
workspace had 209 passed and 2 failed. Both remaining failures also fail on the
published `codex/gptgrep-unified-runtime-20261012` base on this host: one test
gives admission a 30 ms deadline, and the other's large generation fixture
outlasts its 10-second phase deadline. (The base's harness lock file is stale;
this branch updates it.) Earlier intermediate merges in this round recorded
1,050 Rust tests and 87 full62 wrapper tests.

**Paired comparison.** Not run. By owner direction, GPTgrep's reader contract is
part of the declared treatment. That contract covers its base instructions, its
citation and abstention rules, and its answer rules. The comparison therefore
measures the GPTgrep harness against bare Codex, not retrieval alone. In this
source, the full62 adapter still copies the two answer rules into the raw-only
prompt in `legacy_text` mode. Removing that copy before the paired run is pending,
together with the adapter's open review fixes. An optional
bare-Codex-plus-answer-rules arm can later separate prompt-contract effects.

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
| 2026-10-12, second development host | Environment preflight on macOS Intel x86_64: 20 pass, 2 partial, 1 blocked; Codex CLI 0.162.1 binary verified against the official release | Reader container sandbox needed owner approval and rebuilt images |
| 2026-10-12, credential supervisor | `codex-auth` 1.3.1 qualified with fake credentials: 125/125 guarded and 13/13 unguarded tests, no leaks in 18 scans; live supervised runs observed on 1.3.1, and one live owner refresh observed with 1.3.0 | Unmanaged entry points stay blocked for shared-login use |
| 2026-10-12, host development pilot | Treatment arm only: full62 59/62 under a frozen MMLongBench-Doc-V2 judge (2 incorrect; 1 failed run from a provider HTTP 502); 182 Decisions requests, $0.0696 known cost | Development evidence only: no bare arm, not the paired comparison or G5 |
| 2026-10-12, development cohorts | H-text 60/98 and H-unans 97/152 on 107 text-ingestible MMLongBench-Doc-V2 documents; 27 of 134 need OCR | Question-level holdouts that share some full62 documents; development material |
| 2026-10-13, Opt-1 measured | full62 59/62 (+0), H-text 62/98 (+2), H-unans 87/152 (−10; over-commitments 31 → 46); failed runs fell in every cohort | Regression retained; Opt-1b next |
| 2026-10-13, merged source | 1,109 Rust tests (0 failed, 4 ignored), 145 evaluator tests, 11 offline cases and 90 full62 wrapper tests passed; at lower load the incur wrapper passed and 2 harness-workspace deadline tests failed, as they also do on the published base on this host | Paired full62 SEARCH not run; images, Linux index and reader sandbox integration pending; G5 HOLD |
| 2026-10-13, I0 enablement merged | Common and treatment images rebuilt credential-free (no Codex state in any layer; product binary pinned); the 34-document zero-model text index built inside the Linux treatment image, identical in pages and text to the earlier dry build; the reader-only seccomp profile (Docker 29.4.0 default plus the seven namespace calls bubblewrap needs) vendored, derived and pinned for both reader roles; acceptance probes 1–3 accepted, probe 1 in the owner-accepted form without `--proc`; full62 adapter 141 tests passed | Probes 4 (Judge/verifier default profile) and 5 (paired smoke) pending; the Decisions cost bound needs a per-trial submission limit; paired SEARCH not run |

The 2026-10-12 unified runtime round unified native Document/OCR execution, configuration,
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

## Earlier checks and retained failures

This round's checks are in the [2026-10-13 UTC round](#2026-10-13-utc-round)
section above. The unified native runtime round passed **1,024 Rust tests**, with **0 failed**
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
