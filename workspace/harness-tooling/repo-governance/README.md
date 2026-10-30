# Repository Governance Helper (`gptgrep-repo-gov`)

`workspace/harness-tooling/repo-governance/` provides the portable, zero-dependency
repository governance CLI (`bin/gptgrep-repo-gov`) enforcing public-safety
sanitization, branch naming rules, organization push boundaries, and
worktree-aware `pre-push` hooks.

## Commands

### 1. Public Safety Scan (`check-public-safety`)

Scans Git-tracked files (working tree or any `--ref`), plain directories
(`--dir`), and optional Git commit history ranges (`--history-range <base..head>`)
for private LAN IPs, workstation user/volume paths, private homelab hostnames,
concrete 26-character 1Password item UUIDs, OAuth client IDs, Private Relay
emails, and raw secret prefixes.

```sh
bin/gptgrep-repo-gov check-public-safety --repo .
bin/gptgrep-repo-gov check-public-safety --repo . --ref dev/0.2.x-alpha.x --history-range main..dev/0.2.x-alpha.x
```

Optional untracked workstation-specific denylist extensions are loaded
automatically when present from:
- `<repo>/.local/repo-gov-denylist.txt`
- `<git-common-dir>/info/repo-gov-denylist.txt`
- `$GPTGREP_REPO_GOV_DENYLIST` or `--denylist-file <path>`

### 2. Branch Name Validation (`check-branch-name`)

Validates public branch names against repository governance:
- `main`
- `dev/<major>.<minor>.x-alpha.<x|n>`
- `dev/opt-evo/<goal-slug>/<checkpoint-slug>`
- `delivery/<slug>`
- `release/<semver>`

```sh
bin/gptgrep-repo-gov check-branch-name dev/0.2.x-alpha.x
bin/gptgrep-repo-gov check-branch-name dev/opt-evo/xl-c3-baseline-135/lan-preflight-ready-r1
```

### 3. Organization & Notes Boundary (`check-org-boundary`)

Enforces remote push boundaries across repository roles (`core`,
`homelab-private`, `kwb`):
- Blocks `homelab-private` repositories from pushing to any `CelHive` /
  `CelHive-AI` remote.
- Blocks direct pushes to remotes configured with
  `pushurl = DISABLED_MANUAL_RELEASE_SYNC_ONLY`.
- Blocks `refs/notes/*` (private PoUW commit notes) from pushing to any GitHub
  or Hugging Face remote while permitting LAN bare-hub synchronization.

```sh
bin/gptgrep-repo-gov check-org-boundary --role core --remote-url https://github.com/lwyBZss8924d/gptgrep.git
```

### 4. Worktree-Aware Hook Installation (`install-hooks`)

Installs a worktree-aware `pre-push` hook into
`$(git rev-parse --git-common-dir)/hooks/pre-push`:

```sh
bin/gptgrep-repo-gov install-hooks --repo . --role core
```
