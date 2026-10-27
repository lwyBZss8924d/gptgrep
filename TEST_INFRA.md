# TEST_INFRA.md — GPTgrep Remote DevBox & XL-DocBench REEF Readiness E2E Test Infrastructure

**Version**: 1.0.0  
**Date**: 2026-10-22  
**Owner**: E2E Testing Track Lead (`e2e_testing_orch_1`)  
**Scope**: GPTgrep Remote GCE DevBox (`0.1.x -> 0.2.x`) & XL-DocBench REEF Readiness (`D0`–`D6` ↔ `P1`–`P9`)  
**Integrity Mode**: Development / Independent Verification  

This optional DevBox E2E suite requires separately supplied local provisioning fixtures referenced by `workspace/tests/e2e/common.py`. Those fixtures and workstation OCR/corpus verifiers are excluded from this source snapshot; source availability does not establish fresh C3/C4 or benchmark acceptance.

---

## 1. Dual Track Principles & Architecture

The E2E Testing Track operates strictly under **Dual Track Principles**:
1. **Requirement-Driven & Opaque-Box**: Tests are derived solely from specifications (`ORIGINAL_REQUEST.md`, `PROJECT.md`, `PLAN.md`, `SPEC.md`) and authoritative interfaces, never from internal implementation quirks or white-box code artifacts.
2. **Implementation Independence**: The testing track modifies **test code only** (`workspace/tests/e2e/`). Test logic exercises observable CLI outputs, exit codes, file schemas, HTTP/socket protocol barriers, file locking semantics, and container contracts. Any defect in provisioners, sync scripts, broker supervisors, or evaluators is escalated to the implementing agent.
3. **Progressive Testability & Reproducibility**: Each test case is self-contained, sets up its own state in isolated temporary directories or mock POSIX environments, avoids test execution order dependencies, and cleans up completely upon completion.
4. **Authoritative Output Derivation**: Expected values are tied directly to documented architectural parameters, verified hashes, mathematical invariants (e.g. `[lowerBound, upperBound]` evaluation formulas), and audited corpus receipts (e.g. 1,066 zero-byte markdown pages, 62 exception tasks, 292 documents / 1,345 tasks).

---

## 2. 4-Tier Test Case Methodology

The test suite is structured into four distinct, progressive tiers:

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        Tier 4: Real-World Scenarios                    │
│   • End-to-End Paired Pilot Run (4-task dev cohort, Luna/Sol models)   │
│   • Closed-Loop REEF Candidate Loop (Opt -> Test -> Eval -> Next-Opt) │
│   • Verified PoUW Note & Git Bundle Backup Validation                 │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼────────────────────────────────────┐
│                    Tier 3: Cross-Feature Combinations                  │
│   • Concurrent Lease Contention & flock Serialization                  │
│   • Idle-Check Counter State Machine with Active Compilers/Agents      │
│   • Paired Pilot Execution with Alternating Seed & Session Isolation   │
│   • Candidate Worktree Isolation & Mutation Boundary Enforcement       │
│   • Harbor >= v0.24.0 Integration with RewardKit 0.2.3 Score Rounding   │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼────────────────────────────────────┐
│                    Tier 2: Boundary & Corner Cases                     │
│   • Unattached External Disk Error Handling (Refuse Internal Root)     │
│   • Port 0 Blocker Override (Block Child OAuth Refresh/Revocation)     │
│   • Zero-Byte Markdown Page Classification (1,066 Valid Pages)         │
│   • doc_000236 Frozen Missing Upstream Handling (Retain Denominator)   │
│   • Exception Document Page/Version Deltas (62 Tasks Gated/Tracked)    │
│   • Symlink & Unsafe Path Traversal Rejection in Adapter & Controller  │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼────────────────────────────────────┐
│                       Tier 1: Feature Coverage                         │
│   • DevBox GCE VM & Persistent Disk Provisioning Contract              │
│   • IAP SSH Tunneling Firewall Rule (35.235.240.0/20)                  │
│   • Disk Mount, Ext4 UUID, and /etc/fstab Persistence                  │
│   • Development Storage Hierarchy Scaffolding (/mnt/gptgrep-dev)       │
│   • Docker Engine Data-Root Relocation to External POSIX SSD           │
│   • Toolchain Version Integrity (Harbor >= v0.24.0, RewardKit 0.2.3,   │
│     Rust 1.96.0, Bun 1.3.14, uv, Python 3.12/3.13, Codex OSS CLI)       │
│   • AI Agent Keepalive Process Wrapper (/tmp/gptgrep-keepalive)        │
│   • 30-Minute Idle Check & Auto-Suspend Systemd Daemon                │
│   • Three-Tier Corpus Sync Manager (POSIX SSD ↔ HF ↔ GCS Cold Archive) │
│   • Gen 3 Task Availability Contract (292 Docs / 1,345 Tasks)         │
│   • Codex Auth Lease Broker Configuration & Ephemeral Runs             │
│   • Harbor Schema 1.4 Adapter Export Contract                          │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Detailed Test Matrix & Traceability

| Tier | Test ID | Target Component | Requirement / Milestone | Key Assertions & Authoritative Expected Outputs |
| :--- | :--- | :--- | :--- | :--- |
| **T1** | `test_devbox_provisioning_contract` | `01-provision-gce-devbox.sh` | R1, D1, M1 | Project `gws-personal-20261016`, zone `us-east4-a`, machine `n2-standard-16`, volume `gptgrep-dev-vol-01` 250GB `pd-balanced`, `auto-delete=no`, nested virt enabled. |
| **T1** | `test_iap_firewall_rule_contract` | `01-provision-gce-devbox.sh` | R1, D1, M1 | Rule `allow-ssh-ingress-from-iap`, direction `INGRESS`, allow `tcp:22`, source CIDR `35.235.240.0/20`. |
| **T1** | `test_devbox_mount_and_fstab` | `02-remote-devbox-init.sh` | R1, D1, M1 | Block device `/dev/disk/by-id/google-gptgrep-dev-vol-01`, `mkfs.ext4` discard flags, UUID entry in `/etc/fstab` with `discard,defaults,nofail 0 2`. |
| **T1** | `test_directory_hierarchy_scaffolding` | `02-remote-devbox-init.sh` | R1, D1, M1 | Creates all 19 required directories under `/mnt/gptgrep-dev`, enforces mode `700` on private auth and lease paths. |
| **T1** | `test_docker_daemon_relocation` | `02-remote-devbox-init.sh` | R1, D1, M1 | Generates `/etc/docker/daemon.json` with `data-root: "/mnt/gptgrep-dev/docker-data"`, `storage-driver: "overlay2"`, `live-restore: true`. |
| **T1** | `test_toolchain_versions_and_harbor_pin` | Toolchains & `02-remote-devbox-init.sh` | R1, D1, M1 | Mandatory assertion: `harbor >= v0.24.0`, `harbor-rewardkit == 0.2.3`, Rust `1.96.0`, Bun `1.3.14`, uv installed, Python 3.12 & 3.13. |
| **T1** | `test_keepalive_wrapper_contract` | `02-remote-devbox-init.sh` | R1, D1, M1 | Script `/usr/local/bin/gptgrep-with-keepalive` creates `/tmp/gptgrep-keepalive/job-$$.lock`, traps `EXIT INT TERM HUP` for cleanup. |
| **T1** | `test_idle_suspend_daemon_contract` | `02-remote-devbox-init.sh` | R1, D1, M1 | 4 activity checks (locks, pids, compilers/agents via pgrep, who sessions), 6-count threshold (30 min), systemd timer every 5 min. |
| **T1** | `test_three_tier_corpus_sync_interface` | `03-three-tier-corpus-sync.sh` | R2, D2, M2 | Commands `archive-to-gcs`, `pull-from-gcs`, `publish-hf-private`; multi-threaded `zstd -T0 -3` compression with `.sha256` digest verification. |
| **T1** | `test_gen3_task_availability_contract` | Dataset & Intake | R2, P1, P2, M2 | 292 documents, 1,345 tasks; registers `mineru-4.0.12-pro-2605` (`requiresOcr: true`) while `raw-pdf-only` is default. |
| **T1** | `test_codex_auth_lease_broker_config` | `codex-auth.devbox.template.json` | R3, D4, P4, M3 | `canonical_auth_home` mode 700, `flock` lease path, port 0 blocker URL, `passthrough_env` isolation, ephemeral run paths. |
| **T1** | `test_harbor_schema_1_4_export_contract` | `benchmark/src/adapter.ts` | R3, D3, P7, M3 | Generates `task.toml` with `schema_version = "1.4"`, separate verifier environment, network mode `no-network` or explicit allowlist. |
| **T2** | `test_unattached_disk_error_handling` | `02-remote-devbox-init.sh` | R1, D1, M1 | When disk ID does not exist, script exits with status 1 and error `External disk ... not attached. Refusing internal fallback.` |
| **T2** | `test_port_0_blocker_token_isolation` | `codex_auth_supervisor.py` | R3, D4, P4, M3 | Overrides `CODEX_REFRESH_TOKEN_URL_OVERRIDE="http://127.0.0.1:0/oauth/token"`; connections to port 0 are refused, blocking child refresh/revoke. |
| **T2** | `test_zero_byte_markdown_classification` | `zero-byte-markdown-audit.json` | R2, P2, M2 | Exactly 1,066 pages audited with 0-byte markdown: 674 blank/separator + 392 header/footer/page-number; 0 truncated/corrupt. |
| **T2** | `test_doc_000236_frozen_missing_upstream` | Dataset Admission | R2, P1, M2 | `doc_000236` status `frozen_unavailable` with 4-route exhaustion record, 3 tasks blocked (`formal_eval_admitted: false`), kept in denominator. |
| **T2** | `test_exception_document_page_deltas` | Dataset Admission | R2, P1, M2 | `doc_000165` quarantined (+160 page delta, 12 tasks), 3 cover trims (13 tasks), 5 edition updates (38 tasks); total 62 tasks tracked without silent substitution. |
| **T2** | `test_symlink_and_path_traversal_rejection` | Adapter & Evaluator | R3, R4, P7, P8 | Rejects paths with `..`, leading `/`, null bytes, or outside `crates/`, `skills/`, `runtime/`, `prompts/`. |
| **T3** | `test_concurrent_lease_contention` | `codex-auth` broker | R3, D4, P4, M3 | Two workers contend for `flock` lease: non-blocking contender immediately gets `EWOULDBLOCK`, blocking contender queues cleanly until release. |
| **T3** | `test_idle_check_counter_with_active_compiler` | `gptgrep-idle-check` | R1, D1, M1 | Counter increments while idle; when active compiler (`cargo`, `bun`) or agent appears mid-cycle, counter resets to 0, inhibiting suspend. |
| **T3** | `test_paired_execution_with_alternating_seed` | `benchmark/src/runner.ts` | R4, D5, P6, M4 | Interleaves execution order (`[baseline, gptgrep]` vs `[gptgrep, baseline]`) across tasks; isolates reader from judge workspaces. |
| **T3** | `test_candidate_worktree_isolation_boundary` | `self-opt/src/evaluate.ts` | R4, D5, P8, M4 | Enforces isolated candidate worktrees in `/mnt/gptgrep-dev/owners/gptgrep/worktrees/opt-candidate-<round-id>`; rejects diffs outside allowed surfaces. |
| **T3** | `test_harbor_cli_and_rewardkit_precision` | Harbor & RewardKit | R3, P7, M3 | Asserts Harbor CLI `>= v0.24.0`; RewardKit 0.2.3 enforces exact 4-decimal place rounding (`0.181818` -> `0.1818`, `0.666667` -> `0.6667`). |
| **T4** | `test_e2e_paired_pilot_run_simulation` | Paired Native Pilot | R4, D5, P6, M4 | Complete 4-task pilot; normalizes `EvaluationRun`; separates native accuracy from grounded score; retains `unknown` in denominator for `[lowerBound, upperBound]`. |
| **T4** | `test_closed_loop_reef_evaluation_cycle` | REEF Controller Loop | R4, D5, P8, P9 | Full `Opt -> Test -> Eval -> Judge -> Next-Opt` cycle; rejects identical code, mismatch parent, unknown outcomes; accepts strictly improved candidate. |
| **T4** | `test_pouw_bundle_creation_and_backup` | PoUW & Backup | R4, D6, M4 | Validates `refs/notes/commits` note format; creates and verifies Git bundle; asserts zero credential, machine-path, or private session identity leakage. |

---

## 4. Authoritative Sources of Expected Output

Every test case derives its expected values from explicit authoritative artifacts:
1. **Infrastructure Specifications**:
   - `workspace/exec-plans/remote-devbox-harness-20261022-r1/PLAN.md`
   - `scripts/01-provision-gce-devbox.sh`
   - `scripts/02-remote-devbox-init.sh`
   - `scripts/03-three-tier-corpus-sync.sh`
2. **Governance Templates**:
   - `configs/devbox-inventory.template.json`
   - `configs/storage-policy.devbox.template.json`
   - `configs/codex-auth.devbox.template.json`
3. **Audited Dataset & Intake Manifests**:
   - `workspace/handoff/google-agy-xl-docbench-ocr-20261018-r1/zero-byte-markdown-audit.json` (1,066 zero-byte pages)
   - `workspace/handoff/google-agy-xl-docbench-ocr-20261018-r1/remaining-failures-and-frozen-tasks.json` (`doc_000236` frozen, `doc_000165` quarantined)
   - `workspace/exec-plans/xl-reef-readiness-20261021-r1/source-impact.json` (62 exception tasks)
4. **Harbor & RewardKit Specifications**:
   - `benchmark/src/adapter.ts` (Harbor 1.4 schema export)
   - `benchmark/harbor/verify.py` and `test_verify.py` (RewardKit 0.2.3 4-place rounding)
   - User mandate (2026-10-22T03:22:49Z): Harbor version `>= v0.24.0`
5. **Evaluation Contract**:
   - `self-opt/src/evaluate.ts` (`gptgrep.opt-evaluation.v1`, `compare()`, `summarize()`, `planLoop()`)

---

## 5. Test Runner Architecture (`workspace/tests/e2e/`)

The test suite is implemented under `workspace/tests/e2e/`:

```text
workspace/tests/e2e/
├── __init__.py
├── runner.py                    # Unified CLI runner with --tier, --all, and --json-report
├── common.py                    # Opaque-box fixtures, test harness, and assertion utilities
├── test_tier1_features.py       # Tier 1: Feature Coverage (12 test cases)
├── test_tier2_boundaries.py     # Tier 2: Boundary & Corner Cases (6 test cases)
├── test_tier3_combinations.py   # Tier 3: Cross-Feature Combinations (5 test cases)
└── test_tier4_scenarios.py      # Tier 4: Real-World Scenarios (3 test cases)
```

### Running the Tests

To execute all tiers:
```bash
python3 workspace/tests/e2e/runner.py --all
```

To execute a specific tier:
```bash
python3 workspace/tests/e2e/runner.py --tier 1
python3 workspace/tests/e2e/runner.py --tier 2
python3 workspace/tests/e2e/runner.py --tier 3
python3 workspace/tests/e2e/runner.py --tier 4
```

To generate a machine-readable JSON execution receipt:
```bash
python3 workspace/tests/e2e/runner.py --all --json-report workspace/handoff/e2e-verification-receipt.json
```

Or via standard Python `unittest`:
```bash
python3 -m unittest discover -s workspace/tests/e2e -p "test_*.py" -v
```

---

## 6. Verification and Exit Criteria

The test infrastructure is declared ready when:
1. All 26 test cases across Tiers 1–4 are fully implemented and pass with 0 failures and 0 errors.
2. `harbor --version` satisfies `>= v0.24.0` in both Tier 1 and Tier 3 suites.
3. Every test is verified to run cleanly in the local workspace environment without mocking out the real verification logic.
4. `TEST_READY.md` is published at the project root summarizing test commands and coverage.
