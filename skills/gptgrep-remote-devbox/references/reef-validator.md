# REEF Candidate Mutation Validator Specification

## 1. Pinned Framework Version
- **REEF Git Revision**: `f773b6a6e769b5eb3884c1058e7da80b0b6862d1`
- **Modules**: `self-opt/src/evaluate.ts` (`validateMutationPaths`) and
  `self-opt/src/competition.ts` (actual Git diff and exact frozen-path checks).
- The former `self-opt/src/reef/` synthetic controller is retired. The real REEF
  bridge uses the distinct `gptgrep_reef` package described in `self-opt/README.md`.
  Source checks alone do not qualify native receipts or authorize live execution.

## 2. Allowed Mutation Surfaces
Candidate mutations proposed during the automated closed-loop optimization cycle are strictly validated against an allowlist. Only paths prefixed with one of the following directories are accepted:
1. `crates/` — Core Rust implementation and indexing modules
2. `skills/` — Agent skill packages and instruction prompts
3. `runtime/` — Codex staging and host launcher adapters
4. `prompts/` — System prompt templates and task directives

The competition contract additionally freezes exact allowed files. A prefix alone
does not authorize every file below it; evaluator, auth/storage and runtime-control
surfaces remain protected and independent review remains required.

## 3. Rejected Surfaces (Fail-Closed)
Modifications touching any of the following paths cause immediate candidate rejection:
- `tests/` or `benches/` (prevents cheating via test modifications)
- Root build files: `Cargo.toml`, `Cargo.lock`, `package.json`, `bun.lockb`
- Evaluator scripts: `benchmark/`, `workspace/tests/`
- Infrastructure: `scripts/`, `configs/`
