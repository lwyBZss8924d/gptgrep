# GPTgrep Runbooks Index

This directory provides operational entry points and runbooks for GPTgrep development, evaluation, and remote DevBox harnesses.

---

## 1. Remote DevBox Operations Runbook

- **Primary Runbook**: `workspace/exec-plans/remote-devbox-harness-20261022-r1/REMOTE_PROVISIONING_RUNBOOK.md`
- **Target Instance**: `gptgrep-devbox-01` (`<GCP_PROJECT_ID>`, `us-east4-a`)
- **Automation Scripts**:
  - `scripts/01-provision-gce-devbox.sh`: Idempotent GCE instance and 250 GB SSD creation.
  - `scripts/02-remote-bootstrap.sh`: Base toolchain bootstrap (Rust, Bun, uv, Docker).
  - `scripts/03-three-tier-corpus-sync.sh`: XL-DocBench three-tier corpus sync.
  - `scripts/04-install-idle-suspend.sh`: 30-minute idle cost protection daemon and keepalive wrappers.
  - `scripts/05-install-devbox-agents-home.sh`: `/mnt/gptgrep-dev/agents-home` layout and `~/.agents` root bootstrap.
  - `scripts/06-setup-devbox-ai-clients.sh`: AI engineer CLI client installations (`claude`, `codex`, `agy`), lease broker, and autoload links.
- **Verification Tool**: `bin/gptgrep-devbox-selfcheck` (`--mode local|remote --strict --json`)

---

## 2. DevBox Operational Scope & Single-User Permission Architecture

### Single-User Scope Contract
The remote DevBox client state directories located at `/mnt/gptgrep-dev/agents-home/clients/` (including persistent state for `claude`, `codex`, and `gemini`) and their configuration files are configured specifically for the primary developer login user (`devbox_operator`) with mode `0700` (`rwx------`).

### Multi-User Architecture Guidance
In the current single-tenant deployment:
- Autoload symlinks in each user's home (`~/.claude -> /mnt/gptgrep-dev/agents-home/clients/claude`, `~/.codex -> /mnt/gptgrep-dev/agents-home/clients/codex`, `~/.gemini -> /mnt/gptgrep-dev/agents-home/clients/gemini`) point to this shared SSD path.
- Because permissions are set to `0700` owned by `devbox_operator:devbox_operator`, secondary or non-primary users attempting to run `claude`, `codex`, or `agy` will encounter permission denial (`Permission denied (os error 13)`).

For environments requiring multi-user or team concurrency:
1. **Per-User Partitioning**: Partition client state roots by username (e.g. `/mnt/gptgrep-dev/agents-home/clients/${USER}/claude`, `.../codex`, `.../gemini`).
2. **Dedicated Shared Group**: Introduce a devbox developer group (e.g. `devbox-users`) with `0770` permissions and `setgid` bit on client state roots, ensuring group read/write accessibility across authorized human accounts.
