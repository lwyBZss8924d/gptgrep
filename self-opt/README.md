# GPTgrep Harness Opt Evaluation Kit

This kit uses the XL task/reader/judge contract in [benchmark](../benchmark/README.md)
to choose useful product and runtime changes. It implements fixed-cohort scoring,
scenario matching and a bounded next-loop plan. It does not execute a mutation,
create a worktree, or accept a release merely because it produced a plan.

Start from the verified 0.1 product integration, freeze the 0.2 foundation revision,
and create a distinct admitted experimental worktree for each loop. Apply the
selected parent delta explicitly. Product changes may touch `crates/`, `skills/`,
`runtime/` or `prompts/`; the optimizer cannot mutate tasks, gold, rubric, scorer,
frozen feedback or acquisition state to improve its score.

```sh
bun self-opt/src/cli.ts summarize --run PRIVATE_RUN_JSON --output RUN_OUTPUT/summary.json
bun self-opt/src/cli.ts compare --incumbent PRIVATE_PARENT_JSON --candidate PRIVATE_CANDIDATE_JSON \
  --output RUN_OUTPUT/comparison.json
bun self-opt/src/cli.ts plan-loop --parent PRIVATE_PARENT_JSON --baseline FROZEN_REVISION \
  --iteration 1 --paths '["runtime/codex/AGENTS.md"]' --output RUN_OUTPUT/loop.json \
  --profile PROFILE_ID --acceptance PRIVATE_ACCEPTANCE_JSON --acceptance-sha256 ACCEPTANCE_SHA256
bun self-opt/src/cli.ts coverage --assessment PRIVATE_ASSESSMENT_JSON \
  --assessment-sha256 ASSESSMENT_SHA256 --enrolled '["TASK_ID"]' --output RUN_OUTPUT/coverage.json
```

Use `bun self-opt/src/cli.ts --help` for the exact current command contract.
All output writes require an explicit storage binding. Local development runs
place dependencies, caches, temporary files and test output on external storage.

An evaluation run freezes the cohort, source revision, document bytes, rubric,
native scorer, all model roles, effort, service tier, environment and preparation
policy. Compare a candidate with its exact current parent under the same
scenario. A changed scenario requires measuring both again. Only complete
**development** measurements can choose the next parent. The candidate must
improve source-grounded success; an optional cost constraint requires observable
cost, so unknown cost cannot satisfy it.

Keep every enrolled task in the denominator. Started operational failures are
observed zeros. Setup failures, unknown judgments and pending runs remain null,
with lower/upper success bounds. Report native accuracy, atomic coverage,
preparation cost and search cost separately. Do not report root-thread usage as
the total usage of a multi-agent run.

Availability is a separate full-release view. `coverage` reports RAW recovery,
identity holds, reviewed terminal freezes, per-profile preprocessing readiness
and the explicitly enrolled cohort at the named immutable assessment. It does
not assert that a historical assessment is current launch authority. Pre-enrollment
exclusions remain visible in release coverage and never become invented failures.
An enrolled task that cannot start retains its unknown/pending measurement and
the original denominator. `summarize`, `compare`, scenario fingerprints and the
`EvaluationRun` schema retain their existing semantics.

`plan-loop` additionally requires current task/profile admission through the
[benchmark controller contract](../benchmark/README.md#current-availability-and-launch-admission).
Every parent-cohort dependency must be eligible before a new loop is proposed.
Required OCR artifact and provenance bytes are rehashed during planning; declared
hashes alone cannot stand in for the currently available files.
A past successful measurement remains historical evidence even if its task is
now blocked. The new availability sidecar neither changes that result nor grants
execution or expands the optimizer's allowed mutation surfaces.

A loop closeout binds the implementation revision, tests, independent review,
paired live outcomes, failure categories, decision and next action. Preserve the
previous parent when evidence is incomplete or improvement is absent. A rejected
candidate still provides useful failure evidence. Confirmation, source delivery
and canary acceptance are separate decisions.

## Competitive controller and native evidence

`src/competition.ts` owns a bounded four-task, two-candidate development
qualification contract. Its SQLite ledger uses full synchronous commits, explicit
version compare-and-swap, idempotency keys and fenced five-minute controller
leases. The workload deadline denies new reservations while bounded recovery
leases remain available. A reservation is never erased or refunded by a failure,
cancellation or uncertain submission. Known cancellation before dispatch retains
that disposition without inventing a remote job ID.

`src/competition-cli.ts` accepts one closed JSON request on stdin and returns a
JSON result. Its explicit actions are `init`, `snapshot`, `acquire`, `reserve`,
`mark`, `reconcile`, `register`, `qualify`, `results`, `evaluate`, `stage`, `staged`,
`commit` and `committed`.
Supply `ACTION --ledger ABSOLUTE_SQLITE_PATH`; the ledger, WAL and shared-memory
files must be inside the current owner's admitted output. There is no worker,
provider, Harbor or DevBox launch action. `assert-paid` always refuses until the
later execution adapter is independently qualified.

Candidate registration checks real clean Git revisions, ancestry, exact diff
paths and hashes, declared checks/review identities, and pinned build/evidence
bytes. Version 2 additionally freezes the two independent worktrees, native
client binaries and producers, requested model configuration, exact owner
commands and required checks. Version 1 remains unavailable for native qualification.

`src/native-receipts.ts` validates bounded Codex result/trace pairs, the actual
AGY Composer JSON request and result format, host storage terminals, and Codex
authentication cleanup through the evaluator's owner validators. AGY's requested
model and effort are retained separately from its unavailable provider observations.
Composer's cooperative directory restrictions and reaped leader do not establish
evaluator-data isolation or descendant quiescence. The persisted qualification
therefore remains unavailable until an execution adapter binds these components
to the same native job and supplies the missing isolation, audit acceptance and
cleanup evidence. Business commitment revalidates the current pinned evidence;
blocked comparisons cannot nominate a parent.

`src/competition-dispatch.ts` provides read-only preflight for a frozen C4 route.
It checks the independently accepted 135-task C3 baseline, the same campaign
envelope, the four-task parent/scenario, two engineer and two AGY slots, and twelve
matched reader/verifier/judge slots. For a selected owner-managed route it verifies
the exact owner interpreter, CLI, implementation, storage plan and workload argv.
This interface creates no reservation, lease or process. Native actor binding and
measurement dependencies are still unavailable, so dispatch refuses before charge
or launch. A runnable adapter must add those bindings, serialized execution and
charge-before-launch together; these preflight checks do not satisfy C4 execution.

Evaluation calls the existing `evaluate.ts` comparator and requires its pin to
identify the imported source file. Measured runs must match the frozen cohort,
scenario and actual parent/candidate revisions. Unknown outcomes block acceptance.
Results and staged decisions are durable before a distinct commit operation.
The four-task scope can only produce a development nominee after qualification;
it never replaces the 135-task parent, publishes a release or starts another
generation. The native 135-task baseline controller remains a separate interface.

`src/proposals.ts` supplies closed invariant/workflow hypotheses, matched
behavioral contrasts and restricted development feedback. Raw judge objects,
gold-bearing fields and frozen feedback are rejected as extra/invalid fields.
The owner must first produce and review the allowlisted feedback projection;
these structural validators do not classify arbitrary prose as free of gold.
Composition is disabled for the initial round.

The thin Python package lives in `reef-adapter/src/gptgrep_reef`, which cannot
shadow the upstream `reef` import. It imports the actual `CandidateBackend`,
`CandidateEvaluationPlugin` and `Trainer` contracts. `build_trainer` always
supplies the strict GPTgrep plugin. A settled step has no artifact publication;
the separate `commit_applied` callback follows REEF's durable scenario commit.
External job identities are not placed in REEF's weight-training job fields.
Missing upstream dependencies fail startup; no fallback contract classes exist.
Backend initialization checks the installed distribution's VCS commit and the
actual imported module origin. Expected and observed revisions remain separate
fields. The current backend is inline. `recovery.py` uses REEF's actual
`CommitHistoryScenarioStore` to match the exact recorded commit, scenario label,
cursor and artifact references to the durable controller decision. Startup
reconciliation requires a fresh fenced lease: a staged decision without a REEF
commit stays uncommitted, a matching recorded commit is acknowledged once, and an
already acknowledged commit is a no-op. The caller must restore the actual REEF
cursor; the adapter does not claim to reconstruct it. Offline tests reopen the
real scenario store and exercise these boundaries. Live O7 recovery qualification
still requires the real recorded-commit restart path after measured work.

The isolated uv project uses Python 3.13 and pins
[`reef-infra` at `f773b6a6e769b5eb3884c1058e7da80b0b6862d1`](https://github.com/Human-Agent-Society/reef/tree/f773b6a6e769b5eb3884c1058e7da80b0b6862d1).
No GPU/model extras are selected. REEF is Apache-2.0; this adapter imports its
interfaces and does not vendor its implementation. Harness² is method provenance
only, using the invariant/workflow/contrast concepts from
[`1cfe254e8d25b15f37048c3730134686abe60335`](https://github.com/google-research/harness2/tree/1cfe254e8d25b15f37048c3730134686abe60335);
its package, stock proposer, solver, selector and composition loop are not used.

With dependencies and output bound to the selected external development storage,
run the TypeScript contract tests with
`bun test self-opt/tests` and the Python bridge tests through the locked isolated
uv environment with `python -m unittest discover -s self-opt/reef-adapter/tests`.
All fixture paths require `ENVIRONMENTS_HARNESS_OUTPUT` and its exact storage
binding; tests have no temporary fallback and do not emit accepted live evidence.
The retired local `src/reef/`
synthetic pilot/controller implementation remains available in Git history.

## Native actor producers

`src/actor-cli.ts` accepts one closed JSON action on stdin: `policy`, `competition`,
`freezeReview`, `candidateId`, and `phase` (`engineer`, `build`, or `agy`). The first
three values are exact `{path, sha256}` pins. This separate direct local route
uses the user's external storage policy; it does not synthesize an owner storage
approval. Every launch revalidates the exact accepted C3 baseline and O4 actor
review, the clean source runtime, two independent physical candidate worktrees,
current volume identity, finite deadlines, and the shared campaign budget.

The runtime holds one persistent actor lock and charges the campaign before each
phase. A repeated terminal action is read-only. A lost handle, missing terminal or
unknown effect retains the charge and lock for reconciliation; there is no retry,
refund, fallback model or next generation. Engineers use the actual native Codex
capture through the selected authentication supervisor, one bounded native turn,
a named writable profile, no nested workers, and committed source limited to the
frozen mutation paths. The per-run authentication link must be absent after the
supervisor exits. No credential contents are read by this adapter.

Candidate build/check work has its own `prepare_context` admission and finite
command deadline. The configured commands execute against the exact native
candidate commit with dependencies, temporary files, caches and output on the
selected external storage. The producer checks the resulting executable's Linux
ELF architecture and retains its hash, runtime manifest and actual command
terminals. Its peak-byte reservation remains a declaration; before/after free
space observations do not claim a measured full build peak.

The AGY phase requires that exact successful build and an explicitly selected
reviewer route. The legacy route invokes pinned Composer `agy_print` with its
version 2 metadata packet and existing prompt bound. It remains a separate
interface; it cannot silently fall back to the project stream route.

`gptgrep.opt-agy-stream-route.v1` selects the project-owned Bun producer. Its
version 3 packet includes the entire committed diff, complete before/after text
for every changed regular source file, frozen review/root instructions and the
successful build/check bindings. Evaluation tasks and judge material are excluded.
Raw source blobs must round-trip as exact UTF-8 text; valid BOM bytes are retained
and invalid UTF-8 or NUL-bearing blobs are rejected before packet acceptance.
The prepared version 2 record invokes `src/agy-stream.ts` with a pinned job path
and digest in small argv. After a matching native init, the producer sends the
complete packet as exactly one `stream-json` user event on stdin. Packet, frame,
trace and result each have explicit byte bounds; no prompt truncation or alternate
transport is selected when a bound fails.

Each stream job owns one fresh evidence directory, selected model/effort,
original deadline and detached native process group. The fixed deny hook uses
absolute quoted helper paths, and a tool denial requires its complete reason plus
an actual helper marker. Native terminal status, turn count and exposed usage are
retained before success checks, including ERROR/nonzero exits. Cancellation stops
and reaps the owned group; a missing producer receipt or unresolved cleanup keeps
custody unknown and preserves the actor lock. `runNativeAgy` uses the same evidence
consumer exercised by the offline fake-native transport fixtures. Parent
adjudication remains required after any reported review.
Policy/protocol rejection stops the group and prevents further submissions while
continuing bounded capture of already emitted terminal turn/usage accounting.
Bounded lifecycle evidence retains leader closure, the final group-absence probe
and fixed signal/probe error codes and errno values; raw exception text is omitted.

This producer consumes an admitted host run and preserves its original HOME;
the selected host wrapper owns native account/keyring custody and installed hook
qualification. The stream producer does not admit a container AGY role. Effective
all-tool confinement, descendants outside the owned group, provider model/effort
echoes, HTTP request count and monetary cost remain unknown when unexposed by the
client. Offline transport fixtures establish no native or release acceptance.

These producers supply actual actor execution and retained evidence. Their
source tests do not satisfy C3, O4, the fresh matched reader/verifier/judge runs,
business qualification, REEF recovery or release acceptance. The legacy
owner-ticket preflight and native-receipt policy remain separate interfaces.

Version 3 competitions explicitly pin this actor policy. Version 2 retains the
legacy owner-receipt contract; neither version can be silently upgraded by adding
an acceptance flag. `actor-qualification.ts` revalidates the actual native source,
diff, build/check terminals, AGY packet and parent finding dispositions. Its
persisted inspection is currently **unavailable** for runtime qualification:
the direct actor route has no selected owner validator for native job custody,
isolation, collection/finalization or descendant quiescence. A purported qualified
record is refused before it is opened. Local macOS volume observations establish
storage identity and free space only. Linux reader/helper receipts do not admit
engineering, builds or AGY. The Linux route below has explicit role storage and guest execution contracts.
Final candidate qualification still remains unavailable until its native AGY,
full process custody and complete owner evidence have been accepted.

`competition-measurement.ts` defines the fresh four-task incumbent/A/B plan. It
uses the shared `benchmark/src/controller.ts` episode functions for twelve distinct
reader slots, with separate judge/verifier admissions where a reader completes.
A started reader failure stays a measured zero; setup, missing evidence and unknown
judging remain unknown in the fixed denominator. Plans freeze the same scenario,
context, preparation and native runtime, plus each candidate's exact Linux product
build. They cannot reuse C3 results or authentication runs. Each phase is charged
in the common campaign before dispatch. One persistent measurement claim prevents
automatic reruns after lost or uncertain effects.

The ledger's `measurements` JSON action attaches one complete report for both
candidates. Read-only report validation follows its original campaign charges,
owner/container collection and finalization, reader seals, independent judge and
verifier artifacts, and recomputed RAW/ATIF captures. It rejects substituted slots,
stale source pins, repeated native identities and supplied scores that differ from
the original episode evidence. Evaluation, staging and decision commitment repeat
that validation while preserving the existing parent/scenario compare-and-swap.
Retained evidence may be inspected after a deadline; a new launch still checks
the original competition and campaign deadlines. Actual actor qualification,
fresh live measurements and observed recovery remain required. A four-task
development nominee cannot replace the 135-task parent or accept a release.

The Linux `container-actor-cli.ts` entrypoint consumes one exact immutable
`gptgrep.container-actor-job.v1` file. The owner selects Bun with environment
file loading disabled, dependency installation disabled, and its pinned empty
Bun configuration; the guest CLI accepts only the exact config and digest. `c4-actor` admission selects exactly one
`c4-engineer` or `c4-build` command and copies a closed actor capability through
the original owner container and command bindings. The job supplies only that
role's public source, qualified native binary, inputs, original deadline and finite limits. Its independent
Git store has one public source commit before dispatch, no Notes or remote refs,
no unreachable objects, and no ignored or untracked input material. Engineering
uses the existing outer native Codex lease and capture; builds receive no native
authentication, use read-only candidate source and keep dependencies offline.
The shared guest data checks import no controller, campaign, judge or evaluator.

`container-actor-qualifier.ts` and the pinned `container-actor-probe.js` provide
a distinct `c4-actor-qualification` route. It runs no model. It tests same-UID
read baselines, private and other-candidate denials, and the exact allowed or
denied candidate writes under the requested native sandbox. Its report remains
pending owner collection and finalization. Reader E4 evidence cannot be relabeled
as actor evidence. `c4-agy` is recognized but refused until a concrete native
AGY account and isolation contract is implemented. Guest success alone does not
qualify a candidate, accept a campaign decision, or authorize live execution.

The engineer-to-build handoff creates a fresh independent shallow snapshot of
the actual candidate tip. It preserves that commit SHA, regenerates the public
file manifest for the candidate, and carries no engineer runtime/cache or prior
refs. The engineer's two-commit working store is therefore rejected as a build
input. Controller Git uses an exact executable pin and its own closed configuration.
It checks the initial public input configuration, then copies only bounded
regular Git data into a private inspection
directory with its own inert config; it never executes the candidate's hooks,
fsmonitor, filters, textconv, includes or PATH binaries. This scratch copy is
removed after the query while original candidate data remains retained.

Every actual build/check command runs through the same pinned native Codex
sandbox and exact profile tested by no-model qualification. Candidate source is
read-only, a distinct `build_root` is writable, and private controller, evidence
and authentication roots remain denied. The sandbox gets a fresh config-only
home and a curated environment with no account/control inheritance. The actual
build job may include optional `build.pdfium` inputs: a pinned `env` launcher,
the pinned `libpdfium.so`, an exact `includeDirectory`, and pinned `headers`.
The owner copies the identical `build_pdfium` object into the build's container
and command bindings. Both directories stay read-only and outside candidate,
build and evidence roots; their complete library/header inventories and hashes
are verified before execution. The launcher sets `PDFIUM_LIB_PATH`,
`PDFIUM_INCLUDE_PATH`, and `LD_LIBRARY_PATH` only inside the qualified native
sandbox. The outer native process keeps its existing curated environment.
Runtime binaries and headers remain pinned in the immutable job; detached source
acceptance continues to carry source/config inputs. The actual
ELF artifact is copied by the controller into its sealed output after successful
checks. These contracts still require actual Linux qualification and owner
collection/finalization before runtime acceptance.
