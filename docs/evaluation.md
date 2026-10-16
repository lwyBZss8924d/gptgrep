# XL-DocBench evaluation and optimization

The active comparison is the same native Codex agent with GPTgrep versus bare
Codex on XL-DocBench. The reusable benchmark code lives in [`benchmark/`](../benchmark/UPSTREAM.md);
[`self-opt/`](../self-opt/src/evaluate.ts) compares measured product iterations.
The product and its Skills remain separate from both kits. Raw acquisition and
OCR preparation are private data-engineering workflows, not benchmark answers.

The former PageIndex/full62 evaluators have been retired from the active tree.
Their source and measurements remain in Git history. They impose no current
acceptance requirement and are not inputs to a new optimization run.

## Source and task identity

The pinned XL release is `72954bd70ffffe230f08b57c57fa9274ec14d7ea`, whose
`xldocbench_strict_1345_v1` manifest contains 292 documents and 1,345 questions.
The [source contract](../benchmark/UPSTREAM.md) records the release/scorer pins,
document rights, Harbor adapter boundaries and exact local file layout.
Do not substitute a paper cohort or another leaderboard for this release.

Before enrolling a task, verify every original context PDF against its source
manifest, content hash, byte count, physical page count and edition evidence.
Cross-document tasks retain all public context documents, including documents
without annotated evidence. Both arms receive identical complete raw bytes.
Missing or unverified source material stays unavailable; a successful download
alone does not establish task identity. After enrollment, failures and missing
answers stay in the fixed denominator.

Group related documents before splitting development from frozen confirmation.
The grouping includes shared task context, canonical URLs, identical raw hashes
and reviewed edition/translation/near-duplicate families. Exposure moves an
entire connected source group into development. Index preparation receives raw
documents alone; questions, gold answers and evidence annotations never enter
indexes. Readers receive the enrolled question, but no gold answers, evidence
annotations or judge state.

## Matched reader conditions

The selected supervisor model is `gpt-6.1-sol`; native workers and OCR use
`gpt-6-luna`. Freeze the exact model identifiers, effort, service tier, native
runtime revision, permissions, common tools, deadlines and output limits before
each comparison. The helper profile explicitly selects Luna Decisions or Jev
1.14. Record the requested and observed identities; an unavailable model or a
backend substitution is a visible failure or a separately named experiment.

Both arms retain Codex's native base and receive the same question and output
instructions. Bare receives raw documents and ordinary permitted reading,
extraction and rendering tools. The GPTgrep arm additionally receives the pinned
GPTgrep binary, selected generic Skills/roles and complete context-scoped
preparation products. Declare this treatment profile and record which tools the
agent actually used. It is a product configuration comparison, not a claim that
retrieval alone caused a result.

Each arm and the independent judge use a distinct admitted storage run, auth
supervisor role, isolated runtime state and exact permission profile. Reader
profiles expose one task workspace and selected common runtime roots only;
they deny other tasks, the source checkout, controller output and evaluator
material. Shared corpus or parent-output grants are not a shortcut. A profile
readback or a synthetic transport test does not establish physical isolation.
See the [native launch contract](../benchmark/NATIVE-RUNNER.md) and the
[auth supervisor](../workspace/harness-tooling/codex-auth/README.md).

Raw PDF tools may run during a bare query. Give both arms the same selected
executables and libraries, and put temporary files and font caches inside that
arm's admitted writable workspace. Pre-query parsing, OCR, rendering, enrichment
and indexing are reported separately as preparation. All question-dependent
retrieval, routing, reading, rendering and answering belong to SEARCH. Judge
cost is separate. Missing usage, request counts or monetary cost remain null;
root-thread usage alone is not complete multi-agent usage.

## Results and promotion

Keep native relaxed accuracy, token F1, ANLS, grounded success and atomic coverage
as separate measures. The native XL scorer does not judge predicted citations.
The independent EvalsAgent binds its judgment to the exact task, collected
answer bytes, corpus and rubric. Freeze the answer only after the reader exits.
Neither arm may write the judgment or inherit a previous judgment.

A started operational reader failure is an observed zero for grounded success.
Setup failures, unavailable judging and pending tasks remain unknown, not zero
or omitted success. Report bounds and incomplete telemetry. `self-opt` refuses
candidate selection with unknown/pending outcomes or mismatched scenario/cohort
identity. An empty failed output is not a correct intentional abstention.

The default development endpoint is a strictly higher complete-cohort grounded
success rate. Any minimum improvement or cost constraint must be selected before
results. Do not switch metrics, choose per-task best answers or drop failures
after scoring. Retain native accuracy and cost/latency alongside this endpoint.

Development selection, frozen confirmation and release acceptance are distinct:

1. A small live cohort qualifies the execution path and supplies development
   evidence. It does not establish a full XL benchmark result or canary readiness.
2. Each optimization iteration starts from the frozen 0.2 baseline plus the
   explicit accepted parent delta in an isolated worktree. Change product or
   generic harness surfaces, measure both revisions under the same scenario,
   retain regressions, and promote a parent only from complete development data.
3. Freeze the selected configuration and the complete confirmation cohort before
   confirmation. Keep it separate from iterative development feedback. State
   exactly whether a result covers the released benchmark or a declared subset;
   source availability does not silently narrow a full-benchmark claim.
4. A release/canary claim requires the predeclared matched comparison advantage,
   complete required outcomes, independent review and the engineering/delivery
   checks in [SPEC](../SPEC.md). The self-opt report always has
   `releaseAccepted: false`; it cannot grant that acceptance by itself.

No fixed small pilot size or source count is a release threshold. A partial run
ends with its actual scope and unresolved work, without promoting it to a full
comparison. Report paired uncertainty and source-group dependence where
estimable; do not impose a significance claim on a tiny development sample.

## Entry points

Use an owner-checked external storage run before downloads, dependency staging,
tests or live execution. The examples use caller-selected external paths; they
do not authorize acquisition or provider calls by themselves.

```sh
# Offline synthetic contract tests; no npm dependencies are required.
bun run --cwd benchmark test

# Controller-only development selection from already verified local material.
bun benchmark/src/cli.ts freeze \
  --release "$XL_RELEASE_ROOT" --corpus "$XL_CORPUS_ROOT" \
  --output "$XL_RUN_ROOT/cohort" --seed "$XL_SEED" \
  --sample-size "$DEV_SAMPLE_SIZE" --profile "$PROFILE_ID" \
  --acceptance "$CONTROLLER_ACCEPTANCE" --acceptance-sha256 "$CONTROLLER_ACCEPTANCE_SHA256"

# One independently supervised child for each invocation.
bun benchmark/src/cli.ts reader --config "$READER_CONFIG" \
  --acceptance "$CONTROLLER_ACCEPTANCE" --acceptance-sha256 "$CONTROLLER_ACCEPTANCE_SHA256"
bun benchmark/src/cli.ts judge --config "$JUDGE_CONFIG"

# Compare retained measurements; this command does not execute an experiment.
bun self-opt/src/cli.ts compare \
  --incumbent "$INCUMBENT_RUN" --candidate "$CANDIDATE_RUN" \
  --output "$XL_RUN_ROOT/opt-comparison.json"
```

`freeze` currently creates a development cohort. It does not enroll a frozen
release confirmation. `export-harbor` writes compatible tasks from a supplied
cohort; see the [Harbor verifier](../benchmark/harbor/README.md) for its uv-managed
RewardKit dependency and scorer isolation. Compatibility is not an upstream
oracle, parity, registry or publication result.

The [self-opt CLI](../self-opt/src/cli.ts) also supports `summarize` and
`plan-loop`. A loop plan is a request to the checked worktree/storage launcher,
not execution authority. Keep model outcomes, process status, storage receipts,
auth cleanup, source revision, independent review and release state separate in
the final private checkpoint. Public reports contain no private corpus, native
session identifiers, account routing or workstation paths.

Prospective execution also binds a current immutable task-availability assessment.
The dependency projection includes every released public context document, including
non-evidence members of a full-series task. Missing one member blocks that task;
source-group membership does not freeze unrelated tasks with complete contexts.
Acquisition failures remain recovery pending while reasonable exact-source routes
remain unresolved. Final unavailability requires reviewed recovery evidence and
the frozen expected document identity. A newer or similar edition cannot replace
it. RAW eligibility and profile-specific complete OCR preparation are separate.

Report the full released document/task partitions, eligible-before-enrollment
tasks, explicitly enrolled N and historical measurements separately. Prelaunch
denials have no fabricated numeric reward. Keep already enrolled tasks in their
fixed denominator and preserve prior results when a new assessment changes current
launchability. No current release-wide eligible/frozen task count can be inferred
from document counts alone; it needs the verified complete dependency projection.

The [benchmark admission contract](../benchmark/README.md#current-availability-and-launch-admission)
guards direct export/native APIs and the task-aware Harbor launch front door.
Harbor requires a separately accepted complete export and qualified exact runtime;
synthetic no-launch tests, generated tasks and metadata pins cannot qualify a live
Linux/container rollout. Existing frozen code snapshots and consumed runs retain
their historical contracts until explicitly re-admitted for prospective work.
