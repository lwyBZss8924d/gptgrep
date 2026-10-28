# Remote DevBox Agent Governance & Operational Rules

This document defines mandatory governance rules, security boundaries, and operational constraints for all agents operating on or administering `gptgrep-devbox-01`.

## 1. Storage & Filesystem Discipline

1. **Persistent SSD Boundary**:
   - All repositories, datasets, runtimes, toolchains, and agent states MUST reside under `/mnt/gptgrep-dev`.
   - The boot disk (`/`) is strictly for operating system binaries and ephemeral system logs. Never stage large corpora or toolchain caches on the boot disk.
   - Maintain a minimum free disk space floor of **30 GiB** on `/mnt/gptgrep-dev` at all times.

2. **Symlink Hierarchy**:
   - `~/.agents` MUST symlink to `/mnt/gptgrep-dev/agents-home`.
   - AI Engineer client homes MUST symlink to dedicated SSD persistent directories:
     - `~/.claude -> /mnt/gptgrep-dev/agents-home/clients/claude`
     - `~/.codex -> /mnt/gptgrep-dev/agents-home/clients/codex`
     - `~/.gemini -> /mnt/gptgrep-dev/agents-home/clients/gemini`
   - Client configurations MUST inherit shared bootstrap via autoload symlinks:
     - `~/.claude/CLAUDE.md -> ~/.agents/AGENTS.md` and `~/.claude/skills -> ~/.agents/skills`
     - `~/.codex/AGENTS.md -> ~/.agents/AGENTS.md` and `~/.codex/skills -> ~/.agents/skills`
     - `~/.gemini/AGENTS.md -> ~/.agents/AGENTS.md`, `~/.gemini/GEMINI.md -> ~/.agents/AGENTS.md`, and `~/.gemini/config/skills -> ~/.agents/skills`

3. **Docker Data-Root**:
   - Docker daemon storage MUST reside on `/mnt/gptgrep-dev/docker-data` with mode `0710 root:root`.

---

## 2. Security & Runtime Isolation

1. **Codex Auth Single-Refresh Lease Broker**:
   - All Codex CLI invocations MUST route through the single-refresh lease broker (`/mnt/gptgrep-dev/codex-auth`).
   - Exclusive kernel `flock` on `gptgrep-codex.lock` MUST be respected. Concurrent executions queue rather than refreshing tokens out of band.
   - Port 0 loopback blocker (`http://127.0.0.1:0/oauth/token`) MUST remain active to fail closed (exit code 77) if unmanaged token refresh is attempted.
   - Bubblewrap descendant pruning MUST be enforced to prevent EROFS mount collisions inside denied parent mounts.

2. **Harbor Verifier Isolation**:
   - Containerized verifiers MUST execute with `--network=none --read-only --tmpfs /tmp --tmpfs /logs`.
   - Zero outbound network egress is permitted during verification or scoring phases.

3. **REEF Mutation Boundaries**:
   - Candidate mutations during closed-loop optimization MUST be strictly confined to:
     - `crates/`
     - `skills/`
     - `runtime/`
     - `prompts/`
   - Any modification touching `tests/`, `benches/`, `Cargo.toml`, or root scripts MUST fail closed.

---

## 3. Privacy & Secret Governance

1. **Zero Secret Leakage**:
   - Never print, log, or commit secret values, bearer tokens, or `auth.json` content.
   - Tracked Git files MUST be completely free of host-specific machine paths, personal email addresses, OAuth Client IDs, or raw tokens.
2. **1Password Secret Reference Bridge**:
   - Credential references MUST follow the `op://AGENTS/<item_uuid>/<field>` template format.
   - Plaintext credentials must only exist in process memory via the `op-self` runner.
3. **Private Configuration Files**:
   - Host-specific configurations (such as `devbox-private-profile.local.json`) must remain strictly git-ignored with filesystem mode `0600`.

---

## 4. Cost Protection & Billing Guardrails

1. **Idle Auto-Suspend Daemon**:
   - The systemd unit `gptgrep-idle-suspend.timer` MUST remain active and enabled (polls every 5 minutes).
   - If the system remains idle across 6 consecutive checks (30 minutes), it automatically transitions to `SUSPENDED` ($0/hour vCPU burn).
2. **Process Keepalive Protection**:
   - All long-running batch evaluation, model compilation, or corpus verification tasks MUST be launched via:
     ```sh
     gptgrep-with-keepalive <command> [args...]
     ```
3. **Task Completion Re-Suspend**:
   - Upon finishing assigned evaluation, benchmarking, or maintenance tasks, agents MUST ensure the DevBox is returned to `SUSPENDED` state unless an active job is explicitly running.
