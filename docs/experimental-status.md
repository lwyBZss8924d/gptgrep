# Experimental status

Updated 2026-10-16. GPTgrep is in the 0.2 alpha development line. The active
comparative target is the same native Codex agent with GPTgrep versus bare Codex
on XL-DocBench. No full XL comparison, canary acceptance or benchmark advantage
is claimed by this source update.

## Retained product baseline

Commit `7fc95f4244e5635a7c5ed29974d63a33e10151f5` retains the reviewed 0.1
product baseline. It integrates native tool-budget accounting, stable reader
failure codes, usage retention, original-deadline handling and cancellation,
while preserving typed redacted runtime and observer diagnostics.

The exact baseline passed formatting, workspace tests and Clippy with denied
warnings: 1,150 unit/integration test results and five doctests passed; four
environment-dependent tests were explicitly ignored. One passing integration
test was an opt-in fixture preparation no-op. The optional wrapper passed 37
checks with one real native-composition fixture skipped. Packaging self-tests,
runtime staging and the then-retained offline retrieval oracle also passed.
Those are scoped engineering results, not XL results or complete native-runtime
acceptance. Exact private source, command and storage receipts remain local.

## Active 0.2 work

The fresh `0.2.0-alpha.1` macOS ARM64 build passed formatting, 1,150
unit/integration test results, five doctests and Clippy with denied warnings.
Four environment-dependent tests remain ignored, and the opt-in fixture no-op
is included in the passing count. The current wrapper passed 37 checks with one
native-composition fixture skipped. Packaging self-tests, corrected eight-asset
runtime staging and actual relocated text/PDF indexing and fresh retrieval passed.
The real locked RewardKit package passed nine synthetic verifier tests. These
checks establish the selected development build, not Linux packaging, native
model behavior or XL comparison acceptance. Private receipts retain earlier
failed attempts and the final source identities.

- [`benchmark/`](../benchmark/UPSTREAM.md) owns XL source/task projection,
  complete-context isolation, native reader/judge execution and Harbor scoring.
- [`self-opt/`](../self-opt/src/evaluate.ts) compares source-bound development
  outcomes, retains failures/unknowns and plans isolated optimization worktrees.
- Raw PDF acquisition and selected preprocessing are private data-engineering
  workflows. Download success, source identity, parsing coverage and usable
  preparation are separate states.
- Native reader/judge launches require qualified model/auth/storage/permission
  boundaries. Offline contract tests do not establish these live properties.
- Small development comparisons inform implementation. Frozen confirmation and
  release acceptance remain separately declared and evaluated.

The [evaluation protocol](evaluation.md), [SPEC](../SPEC.md) and
[goal contract](../workspace/harness-config/goal-contract.json) define the active
requirements. The former PageIndex/full62 runners and their first-tag gate have
been retired. Their detailed experiments remain in Git history rather than the
active navigation surface.
