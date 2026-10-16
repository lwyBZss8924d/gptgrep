# GPTgrep Reasoning-based Documents Benchmark Kit

This is the 0.2 evaluation foundation for GPTgrep. It uses the pinned public
XL-DocBench release, projects reader tasks without answers or evidence labels,
and compares native Codex readers with an independent source-grounded judge.
Acquisition and MinerU preprocessing are private data-engineering workflows;
this source tree contains the portable adapter, verifier and launch contracts.

The release pin and native scorer hash live in `src/dataset.ts`.
[Upstream provenance](UPSTREAM.md) distinguishes released files from paper counts
and distinguishes the DocAtlas site's historical three-benchmark table from
XL-DocBench results. A historical leaderboard is context, not a local baseline.

## Initial experiment

The initial executable arms are `bare` and `gptgrep`. Both receive the same full
public document context, original PDF bytes, native tool capabilities, prompt,
resource limits and model settings. The treatment additionally receives the
pinned GPTgrep binary, its packaged roles/Skills, and any declared raw-derived
prepared index. Preparation time and model usage are reported separately.

- Supervisor: `gpt-6.1-sol`.
- Delegated workers and visual/OCR work: `gpt-6-luna`.
- Treatment Decisions helper: `openai/gpt-6-luna-decisions` or the explicitly
  frozen `typesafe/jev-1.14` alternative.
- Judge: a fresh `gpt-6.1-sol` session with original PDFs, candidate, reference
  annotations and rubric; no reader session is resumed.

`bare+ocr`, DocAtlas, PageIndex Flash and OpenKB are planned comparison arms.
Their upstream pins and compatible OCR artifacts do not make them implemented
or measured here. MinerU 4.x reader-format compatibility is distinct from
reproducing DocAtlas's reported MinerU 2.5 results.

## Freeze and run

Use Bun and an owner-admitted external storage environment. Commands refuse
outputs outside that run's exact binding. No package installation is needed for
the TypeScript controller.

```sh
bun benchmark/src/cli.ts freeze --release RELEASE_ROOT --corpus VERIFIED_RAW_ROOT \
  --output RUN_OUTPUT/frozen --seed EXPERIMENT_SEED --sample-size 4 --profile bare-raw \
  --acceptance PRIVATE_ACCEPTANCE_JSON --acceptance-sha256 ACCEPTANCE_SHA256
bun benchmark/src/cli.ts export-harbor --release RELEASE_ROOT --corpus VERIFIED_RAW_ROOT \
  --cohort RUN_OUTPUT/frozen/cohort.json --arm bare --output RUN_OUTPUT/harbor-bare --profile bare-raw \
  --acceptance PRIVATE_ACCEPTANCE_JSON --acceptance-sha256 ACCEPTANCE_SHA256
bun benchmark/src/cli.ts reader --config PRIVATE_READER_CONFIG \
  --acceptance PRIVATE_ACCEPTANCE_JSON --acceptance-sha256 ACCEPTANCE_SHA256
bun benchmark/src/cli.ts judge --config PRIVATE_JUDGE_CONFIG
```

The freeze uses seeded source groups and chooses development tasks whose complete
context passed source/hash/physical-page verification before enrollment. It
records unavailable documents and never removes an enrolled task after failure.
The separate frozen split is for confirmation, not repeated optimizer feedback.
Do not tune prompts, indexes or selection rules using benchmark question patterns,
answers, evidence locations or confirmation feedback.

The controller configures one fresh owner storage run and auth-supervisor run
per reader and judge. See [native launch contract](NATIVE-RUNNER.md). Native host
execution and a Harbor container rollout are separate modes; generating tasks,
passing verifier tests or completing a host run does not claim container rollout
acceptance. A Linux Harbor image needs its own qualified tool/runtime artifacts.

## Current availability and launch admission

`assess-availability --release ROOT --corpus ROOT --states JSON --output JSON`
creates a new immutable assessment from a dependency-only release projection.
The state input contains `revision`, `generation`, `recordedAt`, `supersedes`,
`documents`, `profiles` and `preprocessing`; the controller derives the complete
task dependencies from the pinned source. The output retains task IDs, types,
complete context IDs and source-group IDs. It omits question text, answers,
evidence labels and scorer output. The controller parses source rows privately;
the metadata artifact must never be built by shrinking context to judged pages.
Each `verified` state must match the current physically hashed acquisition and
its minimal source-identity proof. Verification objects have closed field sets;
extra question, answer or evidence payloads are rejected rather than retained.

RAW states are `verified`, `recovery_pending`, `identity_hold` and
`frozen_unavailable`. A failed download remains pending recovery. A changed
edition remains held. Terminal freeze requires an explicit reason, frozen
expected identity, dated recovery assessment, pinned evidence and an accepted
independent review of the exact decision. Record the original route and the
reasonable official-equivalent, mirror and archive routes considered, including
why a route was inapplicable. There is no fixed URL-attempt threshold. Never
invent a content hash for an unacquired document; retain `rawSha256: null` and
the digest of its released URL/size/page/edition metadata.

Every task requires every released context member. One unresolved document
blocks the complete task, including full-series tasks. Source groups isolate
development splits; they do not expand a task's material requirements. Explicit
profiles determine whether preprocessing is required. RAW-only `bare` and
`gptgrep` profiles remain eligible when unrelated OCR is pending. A required OCR
profile needs every physical page, exact RAW and parser/profile bindings, and
verified artifact/provenance pins. A planned arm remains unavailable even when
its artifacts exist. A partial pilot is not complete-document OCR readiness.
Enrollment and prospective planning rehash required preprocessing files even
before a reader workspace exists. Export and launch additionally verify artifact
presence separately for every task; one task cannot borrow another's OCR files.

New freeze, export, native and prospective Opt work requires a project-owned
`gptgrep.availability-controller.v1` acceptance receipt. It binds the current
assessment selection, generation, task/profile allowlists, exact source pins,
storage run ID/workload digest/cwd, and optional accepted Harbor export/runtime.
Its `controllerArgv` uses exactly one `@ACCEPTANCE_PATH@` and
`@ACCEPTANCE_SHA256@`; after expansion it must match the existing storage plan's
exact command digest and executable identity. `childArgv` lists any exact
permitted descendant entrypoints, whose source closure is also pinned. This
supports an admitted Python controller spawning Bun without adding fields to
the storage owner's schema. Unknown or duplicate launch JSON fields are denied.

The root/controller selects the authoritative current-assessment record before
obtaining the checked plan. Updating that same current-selection file invalidates
old pinned acceptance receipts. A historical snapshot or a moving pointer by
itself grants no new launch. Hashes bind reviewed bytes; they do not authenticate
origin or protect against a hostile same-user replacement of the entire authority
chain. Each reader/judge still needs its own owner/auth run. A paired controller's
`launchArm` must supply the separately accepted child receipt when crossing to a
fresh storage run; the child cannot reuse the controller's run ID acceptance.

`freeze` writes full-release coverage and blocked-task reasons before rejecting
an insufficient development sample. Coverage counts every released task and
reports pre-enrollment eligibility separately from enrolled N. Historical cohorts,
run outcomes and scores remain unchanged when a later assessment blocks a task.

## Harbor preflight and dispatch

`harbor-preflight --config JSON --acceptance JSON --acceptance-sha256 SHA` checks
one already accepted export for one profile and its exact task set. `harbor-run`
uses the same config and repeats preflight immediately before dispatch. The
config has only `admission` and `outputDir`. No command, args, environment,
task-filter, path, agent or runtime override is accepted. A subset needs a new
accepted export. The adapter's `harbor-export.json` is generated evidence;
independent export acceptance is a separate receipt.
Its `gptgrep.harbor-export-review.v1` record must have `status: accepted` and
bind the exact generated export pin. The runtime qualification record uses
`gptgrep.harbor-runtime-qualification.v1`, `status: accepted` and
`runtimeContractSha256`, computed over the runtime contract excluding its own
qualification pin. An unrelated, rejected or synthetic-status receipt is denied.

Preflight hashes every generated task file, verifies complete RAW/context and
required preprocessing identities, rejects extra task directories, and verifies
the current assessment/profile and accepted runtime. The runtime receipt must
qualify the exact `harbor-run-path-agent-job-name-v1` CLI contract, Linux agent
environment, executable/config pins and auth-supervisor direct-child behavior.
The controller then constructs the fixed supervisor invocation with Harbor as
its direct child. Missing qualification blocks dispatch. This source change
supplies no qualified production runtime and performs no live Harbor acceptance.

Denied dispatches produce a bounded prelaunch receipt with no process start or
numeric measurement. Native admission denials likewise remain distinct from
started reader attempts. Synthetic callback tests establish this boundary;
they cannot establish container, authentication or provider operation.

## Scoring and artifacts

The primary rubric is source-grounded success, with atomic answer coverage as a
diagnostic. Native answer metrics remain separate. The judge must inspect
actual pages for material claims, can accept valid alternative evidence, and
returns unknown when source verification cannot finish. A protocol failure does
not become a successful zero or disappear from the denominator.

Started reader failures count as observed failures. Setup failures, unfinished
attempts and unavailable judgments remain explicit unknowns with score bounds.
Record raw and prepared corpus hashes, exact source revision, model readbacks,
worker observations, reroutes, root-thread usage, latency and all attempts. Missing
backend request counts, full multi-agent cost or served-model identity stay null.
Private run evidence includes exact native provenance; public reports contain
only aggregate results.

The Harbor adapter creates a separate verifier environment. `harbor/verify.py`
uses the pinned native scorer and independent judgment artifact; its RewardKit
integration emits separate metric criteria. It never supplies gold to a reader
or calls a separate LLM for every criterion.

## Checks

```sh
bun test benchmark/tests self-opt/tests
```

The Python verifier also has an isolated, pinned RewardKit integration check.
Run its tests under the project's admitted uv environment. Synthetic protocol
and leakage-boundary tests verify the harness; live paired evidence establishes
provider operation and measured behavior. A four-task development measurement
does not qualify a full benchmark result or canary release.
