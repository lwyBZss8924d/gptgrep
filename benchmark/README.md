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

`assess-availability --release ROOT --corpus ROOT --states JSON --output JSON --source-families JSON`
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
New assessments use `gptgrep.task-availability.v2`. Pinned v1 assessments remain
verifiable historical predecessors, but cannot authorize live work. The identity
proof includes `identity.status`, `basis`, `expectedIdentitySha256`, `sourceUrl`,
evidence pins and a review pin. Equivalence-based acceptance requires that review;
page-count equality and nonempty prose cannot independently admit a source.
The source URL and expected identity digest must match the released metadata.
Evidence semantics and independent acceptance still require actual review.

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
Each v2 preprocessing record pins one per-document `pageManifest`; `artifacts`
contains exactly that pin. The shard uses `gptgrep.ocr-page-manifest.v1`, binds
document/RAW/profile identity, and contains `pageAssets`: benchmark page and RAW page,
Markdown source path/hash/bytes/relative workspace path, corresponding image fields, zero-byte disposition and optional
blank-page review pin. Shards keep the full release assessment within the bounded
JSON reader. Runtime expands and verifies the actual assets; the shard hash alone
cannot satisfy task workspace material. Every page asset must match its exact
relative destination and hash; one empty file cannot stand in for other blank
page destinations. A corpus inventory summary cannot replace
them. Empty Markdown remains unresolved
unless a reviewed blank-page disposition exists; unresolved pages block OCR
readiness. Current-file verification rechecks both hashes and byte counts. Review
receipts must explicitly accept the exact identity/page and name distinct proposer
and reviewer identities; a hash of a rejected or unrelated review cannot admit it.

Assessment and freeze must use the same frozen family map and physically verified
RAW hashes. Freeze compares every released dependency/group before sampling, and
records family/exposure input pins. Exposure events are authoritative: their exact
document union must match the ledger summary. Malformed family entries, duplicate
events and missing exposure context fail closed.

`reader-publication.ts` provides a closed source/task projection and an upload
verification front door. It strips arbitrary nested metadata, checks all staged
paths against an exact hash manifest, rejects caches/private path markers, and
requires canonical JSONL bytes and actual Parquet decoding followed by complete
JSONL row parity. Parquet footer/schema/field metadata must be empty; create files
without pandas attributes or stored Arrow schema metadata. Supply
a separately pinned decoder; the negative interface fixtures do not qualify a
Parquet runtime. The caller verifies this manifest immediately before an
authorized upload. This module performs no publication itself.

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

The task's exported `expected.json` is a pre-reader v1 binding and cannot finish
verification. Harbor restores the collected answer at `/app/answer.json` in the
separate verifier. After the reader has exited, the controller calls
`sealReaderEpisode` with exact attempt, exported-expected and evaluation-plan
pins. The resulting v2 binding is immutable and stays outside the reader.
`judgeAnswer` requires that binding, verifies the frozen question/reference and
copied PDFs, and records a fresh native judge identity. Pass the same v2 binding
as `verifyCommand.expectedPath`; a missing binding or judge remains unknown.
The runtime-specific controller must deliver these controller-owned artifacts
to the verifier after collection. Task export alone does not implement that route.

The evaluation plan has schema `gptgrep.evaluation-plan.v1` and contains `run`
(the exact `EvaluationRun` identity including `arm`, without outcomes), `runtime`
(`sourceRevision`, `binarySha256`, `runtimeManifestSha256`, `codexBinarySha256`,
`buildReceipt` pin), and a `tasks` map keyed by enrolled task ID. Each task binds
`commonPromptSha256`, `contextSha256`, `corpusSha256` and `preparationSha256`.
The product build receipt uses `gptgrep.product-build.v1` and binds
`sourceRevision`, `binarySha256` and `runtimeManifestSha256`; the build owner must
produce it from actual build evidence. `preparationDigest` computes the declared
task-local preparation identity. `writeEvaluationRun` derives scores from exact
episode/judgment/verifier artifacts, validates the frozen plan and retains every
enrolled slot. It refuses overwrite, foreign episodes and changed artifacts.

Native private capture now retains supported app-server item payloads and tool
outputs separately from the compact `NativeRunResult`. Authentication/config RPC
responses and reasoning items are excluded; known and labelled secrets are
redacted with counters. The finite capture ceiling is 64 MiB; exceeding it fails
the run. `writeNativeTrajectory` produces ATIF-v1.8 plus a separate coverage
record, pinning converter bytes, the upstream schema revision/spec digest, native
runtime bytes and source trace. Tool results point to their call IDs and source
lines. Only the selected root turn and explicitly delegated children are joined.
Missing child input, usage, terminal events, media, interrupted calls or unknown
item types stay visible gaps. Nested tool media currently requires further
mapping and therefore prevents complete coverage. A schema-shaped trajectory
or fixture conversion cannot establish live tool/page-reading qualification.

`benchmark/src/controller.ts` supplies the paired execution entrypoint. Its
`batch` mode consumes a pinned v2 manifest, finite v2 campaign envelope and independent
review pin. The shared ledger charges preparation, reader, judge, repair,
calibration, optimization and offline verifier admissions before dispatch. Its
per-purpose counts, deadlines and cumulative byte reservations cannot reset with
a new experiment ID; failed or uncertain submissions are not refunded. These
logical reservations are separate from physical storage measurements. The custom Harbor
agent is `native_agent:NativeCodexAgent`; its bridge contract pins the imported
module and runs the controller's `preflight` and `reader` modes inside the task
environment. The controller then seals the reader, dispatches a fresh judge,
runs the offline verifier and derives fixed-denominator reports. Actual Harbor
import resolution, container storage/auth isolation and live capability coverage
must be qualified separately.

The v2 reader route uses the adapter's explicit `prebuiltImage` digest pin,
`/app/task` workspace and reviewed owner binds. Harbor environment uploads are
disabled, and the actual environment must preserve that exact mount graph.
The outer storage worker runs the controller's
`harbor` wrapper, which launches the selected auth supervisor and Harbor, then
performs container and post-broker finalization before the storage worker exits.
The bridge uses exact-CID owner commands and sealed host originals; collection
does not copy those originals or establish final cleanup. Parent, container and
per-command bindings remain distinct. Guest acceptance is supplied separately
from the immutable reader job, avoiding a self-referential job hash.
The bridge also pins `bunConfig` to the empty `benchmark/empty.bunfig.toml`,
mounted read-only. Both phases include `--no-env-file --no-install --config=PATH`
before the controller entrypoint. The source closure includes those runtime
inputs; host validation resolves guest pins through the frozen bind map and
uses the explicit host job pin until the owner materializes its guest copy.

An owner may explicitly select `proc_environment:ProcOverlayDockerEnvironment`
through `containerOwner.procEnvironment`, containing separate `module` and
`contract` pins. This project-owned Harbor environment supports the native
sandbox's proc overlay while retaining the two `/sys` masks: its exact Engine
request uses `MaskedPaths=["/sys/devices/virtual/powercap","/sys/firmware"]` and
`ReadonlyPaths=[]`. All other security, resource and bind settings must match
the existing owner template. The selection uses a pre-existing local immutable
image ID, finite seccomp/AppArmor profiles and a pinned local Engine endpoint
and Compose executable. It does not patch the installed Harbor or Compose source.

The selected import route verifies and suppresses only Harbor's three generated
log binds; additional user binds are refused. Reader outputs continue through
the explicit owner mounts and sealed host projection. The contract pins Harbor's
trial derivation instead of predicting its random session suffix; only the
Compose project label and corresponding container name derive from the actual
session. One fixed create/start sequence supplies the full container ID, which
the bridge must join to the owner-observed namespace before admission. Later
exec/copy selection requires that singleton ID. Teardown addresses only that
created container and retains failed or uncertain outcomes, with no image,
volume, network or orphan cleanup. Actual Harbor startup, owner admission,
native capability and finalization still require fresh runtime qualification.

Judges may select `documentSourceMode: "pinned-read-only"` to use exact original
RAW files with a fresh workspace and native session. Actual permission readback
must grant those files and the five staged judge inputs read-only access. Source
identity and input hashes are checked after success or failure; changes invalidate
the judgment. The default judge route still copies PDFs. The source-access receipt
records zero harness PDF copies and does not claim that native tools created no
additional files. Live storage accounting must verify that distinction.

Capture ceilings are explicit in the envelope and bridge jobs. Source outputs
are sized before create-once writes; oversized native results remain started
failures with a bounded omission record. The reservation includes both answer
copies, startup/denial metadata, source and host artifacts, and both projected
ATIF copies. Image bytes are captured on direct item completion; aggregate
terminal/readback records cannot establish a new image binding. Retained child
capture is opt-in and uses read-only full-turn queries for explicitly delegated
threads. These controls do not establish OS-wide disk quotas or provider/USD
request ceilings.

Offline verification has its own shared admission and storage terminal. Authored
JSON writes are bounded before writing, and feedback keeps bounded reason previews
with original hashes and byte counts. RewardKit output accounting depends on the
pinned, finite programmatic criteria route; its readback check is not a filesystem
quota. Exact Linux command checks use `/proc/self/cmdline`, retaining Bun runtime
guard flags that are omitted from script `process.argv`. Use `--config=PATH` for
the explicit Bun configuration.

## Checks

```sh
bun test benchmark/tests self-opt/tests
```

The Python verifier also has an isolated, pinned RewardKit integration check.
Run its tests under the project's admitted uv environment. Synthetic protocol
and leakage-boundary tests verify the harness; live paired evidence establishes
provider operation and measured behavior. A four-task development measurement
does not qualify a full benchmark result or canary release.
