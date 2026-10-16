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
  --iteration 1 --paths '["runtime/codex/AGENTS.md"]' --output RUN_OUTPUT/loop.json
```

Use `bun self-opt/src/cli.ts --help` for the exact current command contract.
All output writes require the owner's checked external storage binding.

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

A loop closeout binds the implementation revision, tests, independent review,
paired live outcomes, failure categories, decision and next action. Preserve the
previous parent when evidence is incomplete or improvement is absent. A rejected
candidate still provides useful failure evidence. Confirmation, source delivery
and canary acceptance are separate decisions.
