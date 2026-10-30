# Historical Five-Gate Deployment Checklist

The following checklist retains the October 22 deployment assumptions. Its version
strings, corpus counts, split assignments and thresholds are historical targets,
not current admission or evidence that these checks ran. Current project and
owner contracts determine the required runtime, storage policy and data generation.

The [current selfcheck contract](../SKILL.md#3-selfcheck-observations-and-live-qualification-gptgrep-devbox-selfcheck)
separates current-host observations from live qualification. Default inventory
does not run these substantive owner checks and leaves unverified requirements
`UNKNOWN` with a nonzero exit. Actual storage/runtime/corpus/auth/lifecycle
qualification needs a reviewed producer, fresh bound probe evidence and exact
source/config/host/expiry checks. Even qualified observations do not grant the
next workload admission. The canonical source contract is the README under
`workspace/harness-tooling/gptgrep-devbox-selfcheck/` in the selected repository.

## Gate 1: Storage & Mount Health
- **Mount Verification**: Checks that `/mnt/gptgrep-dev` (or local development volume in local mode) is mounted with valid filesystem semantics.
- **Free Space Threshold**: Requires `free_gib >= 30.0` GiB reserve floor.
- **Docker Data-Root**: Verifies Docker daemon data-root is positioned on persistent SSD (`/mnt/gptgrep-dev/docker-data`).

## Gate 2: Toolchains & AI Engineer CLIs
- **Rust Toolchain**: `rustc >= 1.85.0` (target `1.96.0`), `cargo`.
- **Bun Runtime**: `bun >= 1.2.0` (target `1.3.14`).
- **Python & uv**: `uv >= 0.9.0`, Python `3.12` / `3.13`.
- **Harbor Evaluation Engine**: `harbor >= 0.24.0`, schema version `1.4`.
- **Harbor RewardKit**: `harbor-rewardkit == 0.2.3` with 4-place rounding (`round(score, 4)`).
- **Cloud & Model CLIs**: `gcloud`, `hf` (`huggingface_hub`).
- **AI Engineer CLI Clients**:
  - Claude Code: `claude` CLI
  - OpenAI Codex OSS: `codex` CLI (`0.163.0`)
  - Google Antigravity: `agy` CLI

## Gate 3: Three-Tier XL-DocBench Corpus Integrity
- **RAW PDFs**: Exactly 291 files, exactly `3,549,154,931` bytes.
- **Prepared MinerU Pages**: Exactly 82,121 Markdown files (`1,066` zero-byte pages, 0 missing).
- **Prepared Images**: Exactly 61,589 images.
- **Evaluation Denominator**: Exactly 1,345 tasks (366 `dev` vs 979 `frozen`).
- **Split Invariants**: `doc_000236` strictly frozen; zero cross-split family leakage between `dev` and `frozen`.

## Gate 4: Auth & Runtime Isolation Security
- **Codex Auth Lease Broker**: Exclusive kernel `flock` on `gptgrep-codex.lock`.
- **Port 0 Blocker**: Loopback port 0 (`http://127.0.0.1:0/oauth/token`) fails closed.
- **Bubblewrap Pruning**: Redundant descendant deny paths pruned to prevent Linux EROFS crash.
- **Harbor Container Sandbox**: Verifier execution `--network=none --read-only`.
- **REEF Mutation Validator**: Mutation surface restricted strictly to `crates/`, `skills/`, `runtime/`, `prompts/`.

## Gate 5: Root Agents & Cost Protection Daemon
- **Root Bootstrap**: `~/.agents` root contains `AGENTS.md`, `llms.txt`, and `skills/`.
- **Autoload Symlinks**:
  - `~/.claude/CLAUDE.md -> ~/.agents/AGENTS.md` and `~/.claude/skills -> ~/.agents/skills`
  - `~/.codex/AGENTS.md -> ~/.agents/AGENTS.md` and `~/.codex/skills -> ~/.agents/skills`
  - `~/.gemini/AGENTS.md -> ~/.agents/AGENTS.md`
- **Cost Protection Daemon**: `gptgrep-idle-suspend.timer` active and `gptgrep-with-keepalive` executable.
