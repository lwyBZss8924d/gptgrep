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
  --output RUN_OUTPUT/frozen --seed EXPERIMENT_SEED --sample-size 4
bun benchmark/src/cli.ts export-harbor --release RELEASE_ROOT --corpus VERIFIED_RAW_ROOT \
  --cohort RUN_OUTPUT/frozen/cohort.json --arm bare --output RUN_OUTPUT/harbor-bare
bun benchmark/src/cli.ts reader --config PRIVATE_READER_CONFIG
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
