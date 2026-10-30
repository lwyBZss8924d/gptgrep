---
name: gptgrep-remote-devbox
description: Operate, selfcheck, configure, and administer the remote GCE DevBox DevOps harness, persistent ~/.agents environment, and AI Engineer CLI clients (Claude Code, OpenAI Codex, Google Antigravity).
---

# GPTgrep Remote DevBox DevOps Harness & Self-Governance Skill

This skill governs the operation, programmatic verification, and administration of the dedicated Google Compute Engine (GCE) DevBox (`gptgrep-devbox-01`) and its persistent AI Engineer environments (`~/.agents/*` and CLI clients `claude`, `codex`, `agy`).

## 1. Architectural Overview

### 1.1 Dual-Layer Architecture
The package enforces strict segregation between public portable contracts and private machine configurations:
- **Layer 1: Git-Tracked Portable Skill Package (`skills/gptgrep-remote-devbox/`)**: Sanitized of all host-specific machine paths, personal emails, OAuth Client IDs, credit IDs, or concrete secret UUIDs. Contains public instruction contracts, verification CLI, reference documentation, and configuration templates.
- **Layer 2: Market & Local Execution Layer (`~/ai-self-library/skills-market/gptgrep-remote-devbox/` and `workspace/exec-plans/.../skills-package/`)**: Fully populated local distribution bundling upstream developer skills, offline documentation snapshots, and git-ignored device-local configuration profiles.

### 1.2 Dedicated GCE Infrastructure (Pattern A)
- **Instance**: `gptgrep-devbox-01` (`n2-standard-16`, 16 vCPU, 64 GiB RAM, 50 GB boot disk) in `us-east4-a` (`<GCP_PROJECT_ID>`).
- **Dedicated Persistent Disk**: `gptgrep-dev-vol-01` (250 GB `pd-balanced`, `ext4`, `auto-delete=no`) mounted at `/mnt/gptgrep-dev`.
- **Cold Storage Tier**: Google Cloud Storage bucket `gs://gptgrep-xl-docbench-<GCP_PROJECT_ID>` (US-EAST4).
- **Cost Protection**: Automated 30-minute idle auto-suspend daemon (`gptgrep-idle-suspend.timer`) transitions the instance to `SUSPENDED` ($0/hour vCPU burn) when inactive.

### 1.3 Persistent SSD Storage Hierarchy
All long-lived state, repositories, datasets, runtimes, and agent configurations reside strictly on the 250 GB SSD mount:
```text
/mnt/gptgrep-dev/
├── agents-home/                       # Persistent root for ~/.agents symlink
│   ├── AGENTS.md                      # Root agent bootstrap & constraints
│   ├── llms.txt                       # Root navigation index
│   ├── skills/                        # Shared skills catalog
│   ├── references/                    # Offline documentation snapshots
│   ├── configs/                       # Device-local governance configuration
│   ├── bin/                           # DevBox executables (selfcheck, auth helper)
│   └── clients/                       # Persistent AI client states
│       ├── claude/                    # ~/.claude persistent target
│       ├── codex/                     # ~/.codex persistent target
│       └── gemini/                    # ~/.gemini persistent target
├── datasets/xl-docbench/              # Three-tier XL-DocBench corpus
├── development-artifacts/             # Isolated workspaces, caches, runtimes
│   ├── owners/gptgrep/worktrees/      # Admitted Git worktrees
│   ├── owner-private/codex-auth/      # Single-refresh lease broker home & lock
│   └── toolchains/                    # Pinned toolchains (Rust, Codex, etc.)
└── docker-data/                       # Relocated Docker daemon data-root
```

---

## 2. DevBox Lifecycle Workflows

### 2.1 Resuming the DevBox
To wake the suspended DevBox before running workloads:
```sh
# 1. Resume the instance
gcloud compute instances resume gptgrep-devbox-01 \
  --project=<GCP_PROJECT_ID> \
  --zone=us-east4-a

# 2. Wait for instance RUNNING status
gcloud compute instances describe gptgrep-devbox-01 \
  --project=<GCP_PROJECT_ID> \
  --zone=us-east4-a \
  --format="value(status)"

# 3. Connect via secure Identity-Aware Proxy (IAP) SSH tunnel
gcloud compute ssh gptgrep-devbox-01 \
  --project=<GCP_PROJECT_ID> \
  --zone=us-east4-a \
  --tunnel-through-iap
```

### 2.2 Suspending the DevBox (Cost Protection)
When evaluation runs, qualifications, or interactive sessions finish, suspend the instance immediately to prevent unnecessary billing:
```sh
gcloud compute instances suspend gptgrep-devbox-01 \
  --project=<GCP_PROJECT_ID> \
  --zone=us-east4-a
```

### 2.3 Running Jobs Under Keepalive Protection
To protect long-running batch evaluation or compile tasks from premature auto-suspend:
```sh
gptgrep-with-keepalive <command> [args...]
```
The wrapper traps signals (`EXIT`, `INT`, `TERM`, `HUP`), manages an active lock file in `/tmp/gptgrep-keepalive/job-$$.lock`, and inhibits the 30-minute idle suspend countdown for the duration of the task.

---

## 3. Selfcheck observations and live qualification (`gptgrep-devbox-selfcheck`)

The Bun-based selfcheck reports current-host observations and separately validates
live qualification evidence. It does not certify readiness from installed files,
old PASS receipts, directory presence or declared configuration. The canonical
source and detailed protocol are in `workspace/harness-tooling/gptgrep-devbox-selfcheck/`.

### 3.1 Invocation syntax

```sh
gptgrep-devbox-selfcheck --config ./private-observation-spec.json [OPTIONS]

Options:
  --mode local|remote     Observe the current host; remote requires the Linux target
  --config PATH           Closed, non-secret private observation specification
  --json                  Output a v2 machine-readable receipt
  --strict                Retained compatibility flag; unknown always blocks success
  --output-file PATH      Exclusively create a private receipt; never overwrite
  --verbose, -v           Print observation progress to stderr
  --qualify               Explicitly invoke the configured pinned local qualifier
  --help, -h              Show help and exit
```

Neither mode connects over SSH, wakes a VM, starts services or invokes a provider.
Use an explicitly authorized connection before running on the intended host.
Missing config or unverified evidence is `UNKNOWN` with nonzero exit. Actual
contradictions are `FAILED`. A config template is not a usable private profile.

### 3.2 Five separately qualified requirements

1. `storage.admission`: actual registered volume/path, reserve, per-worker placement
   and workload-specific owner receipts; a directory is not Docker or storage readiness.
2. `runtime.native_tools`: current pinned tools and actual qualified behavior; no
   declared versions or fallback package values count as observations.
3. `corpus.admission`: current accepted RAW/context/preparation identities and source
   dispositions; historical corpus counts do not establish admission.
4. `auth.isolation`: the selected broker and real isolation controls, with actual
   run/source bindings; a config file or local imitation is insufficient.
5. `lifecycle.protection`: current governance, keepalive and lifecycle evidence;
   local mode never substitutes an active timer or successful suspend claim.

Default inventory leaves these requirements unqualified. An independently reviewed
local producer can supply fresh nonce/source/config/host/mode-bound probe receipts
through `--qualify`; proof and owner evidence hashes, identity, time and expiry are
validated. The producer must execute the actual owner checks. Static status JSON
or fixture receipts cannot qualify a gate. Every workload still needs its own
admission even after qualified observations pass.

Installation must include the canonical owner payload. Its copy-only `install.sh`
places the launcher, TypeScript source, README and template in an explicit directory;
an authorized installer can link the command to that payload. A source update does
not update installed Skills or prove current remote adoption.

---

## 4. AI Engineer CLI Invocation Contracts

All three AI Engineer CLI clients on the DevBox share global governance and skills via symlinks to `~/.agents`:

| Client | Invocation Contract | Autoload Symlinks |
| :--- | :--- | :--- |
| **Claude Code** | `claude --dangerously-skip-permissions --resume` | `~/.claude/CLAUDE.md -> ~/.agents/AGENTS.md`<br>`~/.claude/skills -> ~/.agents/skills` |
| **OpenAI Codex** | `codex --dangerously-bypass-approvals-and-sandbox resume` | `~/.codex/AGENTS.md -> ~/.agents/AGENTS.md`<br>`~/.codex/skills -> ~/.agents/skills` |
| **Google Antigravity** | `agy --dangerously-skip-permissions --conversation=<id>` | `~/.gemini/AGENTS.md -> ~/.agents/AGENTS.md`<br>`~/.gemini/GEMINI.md -> ~/.agents/AGENTS.md`<br>`~/.gemini/config/skills -> ~/.agents/skills` |

### 4.1 Credential & Secret Reference Management
- Never store plaintext credentials, tokens, or `auth.json` in tracked Git files.
- Secret injection uses the `1password-self` reference format: `op://AGENTS/<item_uuid>/<field>`.
- Private configurations reside in git-ignored files:
  - `workspace/exec-plans/.../configs/devbox-private-profile.local.json`
  - `/mnt/gptgrep-dev/agents-home/configs/devbox-private-profile.local.json` (`chmod 600`)

---

## 5. References & Linked Documentation

- `references/devbox-architecture.md`: In-depth specification of GCE DevBox infrastructure and storage policies.
- `references/verification-gates.md`: Mathematical definitions and thresholds for the 5 verification gates.
- `references/codex-auth-lease.md`: Single-refresh-owner flock broker contract and port 0 blocker specification.
- `references/harbor-isolation.md`: Harbor container sandbox isolation parameters.
- `references/reef-validator.md`: REEF mutation validator allowlist rules.
- `references/corpus-accounting.md`: Full accounting of the 291 RAW PDFs and 1,345-task denominator.
- `references/attribution.md`: Google Cloud SDK metric attribution standards.
