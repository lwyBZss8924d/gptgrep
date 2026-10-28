# TEST_READY.md — GPTgrep Remote DevBox & XL-DocBench REEF Readiness E2E Verification Suite

**Status**: Verified & Ready  
**Date**: 2026-10-22T14:55:00Z  
**Owner**: E2E Test Suite Architect (`test_writer_e2e`)  
**Scope**: GPTgrep Remote GCE DevBox Harness, `gptgrep-devbox-selfcheck` CLI, Skills Packaging, AI Clients & Google Skills Bundling, 1Password Reference Bridge, Provisioning Scripts (`01`–`06`), XL REEF Readiness  
**Receipt Path**: `workspace/handoff/e2e-verification-receipt.json`  
**Package Receipt Path**: `workspace/handoff/closeout/DEVBOX-SKILLS-PACKAGE-RECEIPT.json`  

This optional DevBox E2E suite requires separately supplied local provisioning fixtures referenced by `workspace/tests/e2e/common.py`. Those fixtures and workstation OCR/corpus verifiers are excluded from this source snapshot; source availability does not establish fresh C3/C4 or benchmark acceptance.

---

## 1. Test Suite Summary & Execution Results

All 4 tiers of the independent, requirement-driven, opaque-box E2E test suite are implemented under `workspace/tests/e2e/` and have passed all 36 verification checks (100% pass rate, 0 failures, 0 errors, 0 skipped):

| Tier | Module | Tests Run | Failures | Errors | Skipped | Status |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: |
| **Tier 1: Feature Coverage** | `test_tier1_features.py` | 12 | 0 | 0 | 0 | **PASS** |
| **Tier 2: Boundary & Corner Cases** | `test_tier2_boundaries.py` | 9 | 0 | 0 | 0 | **PASS** |
| **Tier 3: Cross-Feature Combinations** | `test_tier3_combinations.py` | 9 | 0 | 0 | 0 | **PASS** |
| **Tier 4: Real-World Scenarios** | `test_tier4_scenarios.py` | 6 | 0 | 0 | 0 | **PASS** |
| **Total Suite** | `runner.py --all` | **36** | **0** | **0** | **0** | **PASS** |

Execution time: **~16.45s** for all 36 test cases (including real network timeout checks, CLI invocations, and git bundle validations).

---

## 2. Test Execution Commands

### Unified CLI Runner
```bash
# Execute all 4 tiers with receipt output
python3 workspace/tests/e2e/runner.py --all

# Execute a specific tier (1, 2, 3, or 4)
python3 workspace/tests/e2e/runner.py --tier 1
python3 workspace/tests/e2e/runner.py --tier 2
python3 workspace/tests/e2e/runner.py --tier 3
python3 workspace/tests/e2e/runner.py --tier 4

# Execute all tiers and emit structured machine-readable JSON receipt
python3 workspace/tests/e2e/runner.py --all --json-report workspace/handoff/e2e-verification-receipt.json
```

### Standard Python Unittest Discovery
```bash
python3 -m unittest discover -s workspace/tests/e2e -p "test_*.py" -v
```

### Pytest Runner
```bash
pytest workspace/tests/e2e/ -v
```

---

## 3. Verified Assertions & Capabilities

### Tier 1: Feature Coverage (12 Tests)
- [x] **Skills Package Structure (`test_01`)**: Portable Git-tracked `skills/gptgrep-remote-devbox/` directory structure, frontmatter in `SKILL.md`, `llms.txt`, and `AGENTS.md`.
- [x] **Selfcheck CLI Interface & Flags (`test_02`)**: `bin/gptgrep-devbox-selfcheck` executable permissions, responds to `--help`, `--mode local`, `--json`, and `--strict`.
- [x] **Manifest & Root llms.txt Registration (`test_03`)**: `gptgrep-remote-devbox` registered in `workspace/harness-config/resource-manifest.json` and linked from root `llms.txt`.
- [x] **Google Upstream Skills Bundle (`test_04`)**: All 8 bundled Google skills (`retrieving-developer-knowledge`, `finding-google-skills`, `gcloud`, `google-cloud-recipe-auth`, `google-cloud-recipe-onboarding`, `gdrive`, `gws`, `colab-operator`) and official Google developer documentation (`developers.google.com/knowledge`).
- [x] **AI Clients Skills & Docs Bundle (`test_05`)**: Bundled skills and documentation for Claude Code (`claude-code-operator`, 26 official docs), OpenAI Codex (`openai-docs`, config schema, reference docs), Antigravity (`antigravity-guide`, `agy-customizations`), and 1Password (`1password-self`).
- [x] **1Password Secret Reference Bridge (`test_06`)**: `.env.op.template` files define abstract `op://` references; asserts zero plaintext tokens or hardcoded secrets.
- [x] **Privacy Governance & Ignored Configs (`test_07`)**: `devbox-private-profile.local.json` strict permissions (0600), valid profile schema, and Git-ignore enforcement via `git check-ignore`.
- [x] **Idle Suspend Script Contract (`test_08`)**: `04-install-idle-suspend.sh` parameters, 4-layer inspection, keepalive logic, and 30-minute threshold.
- [x] **DevBox Agents Home Installer Contract (`test_09`)**: `05-install-devbox-agents-home.sh` directory layout, `~/.agents` symlink wiring, and selfcheck symlink.
- [x] **DevBox AI Clients Setup Contract (`test_10`)**: `06-setup-devbox-ai-clients.sh` client symlinks, lease broker, and authentication helper integration.
- [x] **DevBox Wake and Suspend Lifecycle (`test_11`)**: GCE instance wake (resume), suspend, and IAP tunneling contracts in scripts and skills.
- [x] **Five-Pillar Selfcheck Receipt Schema (`test_12`)**: Validates `gptgrep.devbox-selfcheck.receipt.v1` JSON receipt schema and required properties.

### Tier 2: Boundary & Corner Cases (9 Tests)
- [x] **Invalid CLI Flags & Missing Arguments (`test_01`)**: Rejects invalid flags (e.g. `--unknown-flag`, missing arguments) and exits gracefully with non-zero status.
- [x] **Unattached Disk Error Handling (`test_02`)**: Provisioning scripts refuse internal disk fallback and exit with status 1 when external SSD mount is missing.
- [x] **Insufficient Free Space Threshold (`test_03`)**: Storage threshold logic triggers failure when free space < 30 GiB.
- [x] **Malformed Manifest & JSON Rejection (`test_04`)**: Schema parser rejects malformed JSON or manifests missing required fields.
- [x] **Port 0 Blocker Token Isolation (`test_05`)**: Port 0 blocker URL overrides (`http://127.0.0.1:0/oauth/token`) and deterministic socket connection refusal.
- [x] **Zero-Task Splits & Leakage Audit (`test_06`)**: `split.json` task counts verify exact 1,345 tasks (366 dev vs 979 frozen), 0 duplicate tasks, 0 cross-family leakage, and anchor `doc_000236` strictly frozen.
- [x] **Network Timeouts & IAP Tunnel Failure (`test_07`)**: Remote selfcheck fails closed with exit code 1 when IAP SSH is unreachable.
- [x] **Symlink and Path Traversal Rejection (`test_08`)**: Rejection of relative paths with `..`, leading `/`, null bytes, or outside allowed surfaces (`crates/`, `skills/`, `runtime/`, `prompts/`).
- [x] **Tracked Files Privacy Scan (`test_09`)**: Full Git tree scan asserting zero private emails, OAuth client IDs, credit IDs, or host paths tracked in Git.

### Tier 3: Cross-Feature Combinations (9 Tests)
- [x] **Concurrent Lease Contention & Serialization (`test_01`)**: Real `flock` lease serialization across processes; non-blocking contender immediately gets `EWOULDBLOCK`, blocking contender queues cleanly until release.
- [x] **Idle-Check Counter with Active Compiler (`test_02`)**: Counter increments when idle; resets immediately to 0 when compiler/agent process or keepalive lock appears, preventing premature VM suspension.
- [x] **Paired Execution with Alternating Seed (`test_03`)**: Interleaves execution order (`[baseline, gptgrep]` vs `[gptgrep, baseline]`) across tasks; isolates reader from judge workspaces.
- [x] **Candidate Worktree Isolation Boundary (`test_04`)**: Worktree isolation under `/mnt/gptgrep-dev/owners/gptgrep/worktrees/opt-candidate-<round-id>`; rejects diffs modifying benchmarks or CI.
- [x] **Harbor `>= v0.24.0` with RewardKit 0.2.3 Precision (`test_05`)**: Asserts Harbor CLI `>= v0.24.0`; RewardKit 0.2.3 enforces exact 4-decimal place rounding (`0.181818` -> `0.1818`).
- [x] **Selfcheck CLI & 1Password Resolution (`test_06`)**: Tests cross-feature interaction between `.env.op.template` abstract reference syntax and environment resolution with 0600 file modes.
- [x] **Skills Packaging & Manifest Indexing Consistency (`test_07`)**: Bidirectional consistency between `skills/gptgrep-remote-devbox/` structure, `workspace/harness-config/resource-manifest.json`, and root `llms.txt`.
- [x] **Client Symlinks & Cost Protection Daemon (`test_08`)**: AI client wrappers touch `/run/gptgrep-keepalive/` lockfiles; idle-suspend check detects active locks and resets idle counter.
- [x] **Harbor Isolation & Bubblewrap Rules (`test_09`)**: Validates Bubblewrap read-only sandbox construction (`--ro-bind / /`, `--unshare-net`, `--tmpfs /tmp`) preventing out-of-worktree modifications.

### Tier 4: Real-World Scenarios (6 Tests)
- [x] **End-to-End Paired Pilot Run Simulation (`test_01`)**: Complete 4-task pilot; normalizes `EvaluationRun`; separates native accuracy from grounded score; retains `unknown` in denominator for `[lowerBound, upperBound]`.
- [x] **Closed-Loop REEF Candidate Evaluation Cycle (`test_02`)**: Full `Opt -> Test -> Eval -> Judge -> Next-Opt` cycle; rejects identical code, mismatch parent, unknown outcomes; accepts strictly improved candidate.
- [x] **PoUW Bundle Creation & Cloud Backup Verification (`test_03`)**: Validates `refs/notes/commits` note format; creates and verifies Git bundle; asserts zero credential, machine-path, or private session identity leakage.
- [x] **Local Selfcheck Execution Pipeline (`test_04`)**: End-to-end execution of `gptgrep-devbox-selfcheck --mode local --json`; verifies all 5 gates pass, all 5 pillars pass, 0 errors, and corpus invariants match.
- [x] **Simulated Remote Provisioning Lifecycle (`test_05`)**: Validates all 6 provisioning scripts (`01`–`06`) for bash syntax (`bash -n`), executable bits, and dry-run/help execution.
- [x] **Skills Package Receipt Generation (`test_06`)**: Generates and validates `DEVBOX-SKILLS-PACKAGE-RECEIPT.json` conforming to `gptgrep.devbox-skills-package.receipt.v1`.

---

## 4. Test Infrastructure Deliverables Index

```text
TEST_READY.md                                      # Test suite readiness summary & execution receipt (this file)
workspace/tests/e2e/
├── __init__.py                                    # E2E package entrypoint
├── runner.py                                      # Unified CLI & test runner supporting --all, --tier, and --json-report
├── common.py                                      # Authoritative directory anchors, JSON helpers, and subprocess executor
├── test_tier1_features.py                         # Tier 1: Feature Coverage (12 test cases)
├── test_tier2_boundaries.py                       # Tier 2: Boundary & Corner Cases (9 test cases)
├── test_tier3_combinations.py                     # Tier 3: Cross-Feature Combinations (9 test cases)
└── test_tier4_scenarios.py                        # Tier 4: Real-World Scenarios (6 test cases)
workspace/handoff/e2e-verification-receipt.json     # Machine-readable test execution receipt
workspace/handoff/closeout/DEVBOX-SKILLS-PACKAGE-RECEIPT.json # Skills package closeout verification receipt
```
