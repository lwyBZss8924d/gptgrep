# Harbor v0.24.0 Container Isolation Specification

## 1. Engine & Schema Version
- **Harbor CLI Version**: `>= 0.24.0` (pinned `0.24.0` / `0.24.1`).
- **Task Descriptor Schema**: `task.toml` enforces `schema_version = "1.4"`.
- **RewardKit Version**: `harbor-rewardkit == 0.2.3` with exact 4-place reward rounding (`round(score, 4)`).

## 2. Sandbox Security Boundaries
All benchmark task evaluation runs inside Docker container environments with strict zero-trust isolation:
- **Network Mode**: `--network=none` (strictly zero outbound network egress during verifier execution).
- **Filesystem Mode**: `--read-only` root filesystem.
- **Ephemeral Volumes**: `--tmpfs /tmp --tmpfs /logs` for temporary scratch and execution logging.
- **Artifact Isolation**: Task outputs are constrained strictly to `/app/answer.json` mapped to `answer.json`.
