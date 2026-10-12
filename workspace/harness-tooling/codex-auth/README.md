# codex-auth: shared ChatGPT credential supervisor

`codex_auth_supervisor.py` lets several local Codex consumers share one persistent ChatGPT login cache
(the *canonical auth home*) without copying it, without letting consumers refresh or revoke it, and
without sharing any other Codex state. It is a single Python 3.12+ standard-library file. It never
opens, reads, hashes or decodes the credential: every check uses `lstat` metadata.

Status: implemented and qualified against fake credentials and loopback fakes (see
[Qualification](#qualification)). Live behaviour against a real login is not established by this
document. Follow the [live runbook](#live-runbook) and record its results separately. The
[limitations](#limitations-and-unmanaged-entry-points) are part of the contract: the related
acceptance items stay `BLOCKED` until each is qualified.

## What it enforces

| Control | Mechanism |
| --- | --- |
| One serial group per auth source | `flock` on `lease_path`, a file outside every checkout. The parent directory must be 0700 and owned by the caller. The file must be 0600, opened with `O_NOFOLLOW`, never a symlink and never hardlinked. Exclusive by default. Shared mode exists only with `allow_shared_consumers=true`. The wait is bounded and ends in `AUTH_BUSY` (75). |
| Lease bound to the auth source | The first managed command writes a non-secret marker, `.gptgrep-codex-auth-binding.json`, into the canonical auth home. It records the lease path and the lock file's device and inode. Later commands refuse another lease path (`lease_binding_mismatch`), and refuse a deleted or recreated lock file (`lease_file_missing`, `lease_file_replaced`), which would otherwise split the lock. `rebind-lease` repairs the binding after human review, and only while no holder is alive. The marker is created whole (a temporary file hard-linked into place), so concurrent first commands never see it half-written; one written in place by an older supervisor is awaited for 2 seconds before it counts as `lease_binding_unreadable`. Deleting the marker itself is not detected; see [limitations](#limitations-and-unmanaged-entry-points). |
| Whole-lifecycle lease | The lease is never `LOCK_UN`ed. Its descriptor is held by the supervisor and by a **lease guardian** process, which inherits it before anything that can use the credential is launched. **Consumers never receive it** (no `pass_fds`, no `GPTGREP_AUTH_LEASE_FD`): every holder of that open file description, a model tool command included, could release the lock for all holders with `flock(LOCK_UN)`. The pinned official owner app-server and re-login CLI, which run no tool commands, still inherit it. The supervisor tracks the process tree throughout the run. After the child exits it sweeps for survivors: pid tree, process group and session, `lsof` holders of a per-run marker descriptor, and processes with files or cwd under the run directory. Only a clean sweep releases the guardian. Survivors, an inconclusive sweep, a signal or any error after launch give `UNKNOWN` (76), and the guardian then holds the lease until the processes and any labelled Harbor containers are gone. If the supervisor itself is killed, the guardian takes over the same way and writes the receipt. Before it releases, it re-checks the canonical credential (a change quarantines), scans the run directory for credential links (see below) and removes the run's auth link. |
| Signals | `SIGINT`, `SIGTERM` and `SIGHUP` are recorded, never fatal, for the whole command. During a run a signal cancels the child. After the child exits, a further signal only stops the waiting (sweep settle, Harbor container wait): the run ends `UNKNOWN` and the guardian keeps the lease. During an owner refresh the signal is honoured after the in-flight refresh and its in-place save complete, so the refresh is always judged and recorded. |
| Per-run isolation | `RUNS_ROOT/<role>/<run-id>/{codex-home,state,state/logs,home,tmp,work,results}`, all 0700. Run ids are never reused. `codex-home/config.toml` is generated. The canonical `config.toml`, skills and state are never copied. |
| Auth link lifetime | `codex-home/auth.json`, an absolute symlink to the canonical file, is created only after the quarantine check, the lease and the blocker and canonical preflights. It is removed on every exit path once the sweep is clean; after `UNKNOWN`, the guardian removes it at release. Refusals (`AUTH_BUSY`, `QUARANTINED`, `BLOCKED_AUTH`, configuration errors) never create it. The `_relogin` and `_maintenance` layouts never have one. A link a consumer *replaced* with a regular file or a different symlink (a per-run login or copy) is `RUN_AUTH_LINK_REPLACED`: the run is `QUARANTINED` and the file is left for review, not deleted. |
| Pinned Codex config | `cli_auth_credentials_store="file"`, `forced_login_method="chatgpt"`, `features.use_agent_identity=false`, `features.{secret_auth_storage,memories,hooks,plugins,apps}=false`, `memories.{generate_memories,use_memories}=false`, `cloud.skills.enabled=false`, `notify=[]`, `analytics/feedback` off, `history.persistence="none"`, `check_for_update_on_startup=false`, `log_dir=<run>/state/logs`. Every key is checked against the 0.162.1 schema; the qualification starts app-servers with `--strict-config`. |
| Allowlisted environment | The child environment is built from scratch: fixed `PATH` (or the role's `path_env`), `LANG`/`LC_ALL`, per-run `HOME`, `TMPDIR` and `XDG_*`, `CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_REFRESH_TOKEN_URL_OVERRIDE` and `CODEX_REVOKE_TOKEN_URL_OVERRIDE` (the blocker), `CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED=1`, loopback `NO_PROXY`/`no_proxy`, `GPTGREP_AUTH_RUN_ID`, `GPTGREP_AUTH_SOURCE_ID` and `GPTGREP_AUTH_FAILURE_RECORD` (`{results}/auth-failure.json`), plus any names a role lists. `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` and `OP_SERVICE_ACCOUNT_TOKEN` can never pass. `OPENROUTER_API_KEY` passes only for roles with `allow_openrouter=true`. |
| Consumers cannot refresh or revoke | Codex reads both override variables on every call. The default blocker is `http://127.0.0.1:0/oauth/token` (revoke: `/oauth/revoke` on the same address). **Port 0 can never have a listener**: binding port 0 assigns an ephemeral port, and a connect to it fails before a byte is sent (`EADDRNOTAVAIL` on macOS, `ECONNREFUSED` on Linux). A fixed low port such as 9 is not safe, because any local process, a consumer's own tool commands included, can bind it on macOS and would receive the refresh token. Proactive refresh (token within 5 minutes of expiry), 401 recovery step 2, and explicit `account/read {refreshToken:true}` all fail transiently. A guarded reload still adopts an owner's rewrite. For a configured non-zero blocker port, the port is probed at launch, every 2 seconds while the child runs, and after it exits. A listener stops the child and gives `QUARANTINED` (`BLOCKER_LISTENER_APPEARED`), because the token may have been harvested. |
| Proxy safety | reqwest/hyper-util read the macOS system HTTP(S) proxy but **ignore its ExceptionsList**, while honouring `NO_PROXY`. Without the loopback `NO_PROXY`, a request to the blocker (body included) can be relayed through a local or remote proxy. The supervisor always sets it. |
| Single refresh owner | `refresh` holds the **exclusive** lease, so no consumer is alive during Codex's non-atomic in-place save. It drives the official `codex app-server --listen stdio:// --strict-config` in its own per-run home without the consumer blocker: `initialize`, `initialized`, `account/read {refreshToken:true}`, then `account/read {}`, which must report a `chatgpt` account. Success (`REFRESHED`) requires the canonical `mtime_ns`/`size` to change while the inode, regular-file type, mode 0600 and the run's symlink stay intact. |
| Owner failure classes | The owner runs with `RUST_LOG=error,codex_login=warn,codex_app_server=warn`, because by default the app-server logs only errors and hides transient refresh failures. Its log is classified by the 0.162.1 error codes and messages. **Permanent** (`REFRESH_TOKEN_REUSED`, `_EXPIRED`, `_REVOKED`, `INVALID_GRANT`, `ACCOUNT_MISMATCH`, `REFRESH_REJECTED` for any other 401) gives `BLOCKED_AUTH` (77) and a quarantine whose details carry `failure_kind=permanent`. **Transient** (`TRANSPORT`, `RATE_LIMITED`, `AUTHORITY_SERVER_ERROR`) with an unchanged credential that still reads as a chatgpt account gives `REFRESH_UNAVAILABLE` (69): no quarantine, retry later. An unclassified failure is `BLOCKED_AUTH` with a quarantine, but is never treated as permanent. |
| Owner stop | Close stdin and wait. If the owner is still running, SIGTERM its group with a long grace. SIGKILL is sent only after the grace, and is then recorded as `OWNER_SIGKILLED_AFTER_GRACE` with a quarantine, because the save may have been torn. If the supervisor is killed during a refresh, the guardian waits for the owner and quarantines any credential change it could not judge (`OWNER_REFRESH_UNRECORDED`, ledger `OWNER_INTERRUPTED`). |
| Consumer auth failures | Consumers report an explicit auth failure (unauthorized, not logged in, refresh rejected) by **writing the auth-failure record and exiting 77**. The record is `$GPTGREP_AUTH_FAILURE_RECORD` (`{results}/auth-failure.json`): `{"schema": "gptgrep.codex-auth.auth-failure.v1", "class": "AUTH_UNAUTHORIZED"}`, where the class is `AUTH_UNAUTHORIZED`, `AUTH_NOT_LOGGED_IN`, `AUTH_UNEXPECTED_ACCOUNT_TYPE` or `AUTH_REFRESH_REJECTED`. It counts only as this uid's regular file with one link that is not the credential, at most 4 KiB (read with `O_NOFOLLOW`). The run is then `BLOCKED_AUTH`, and an auth-block marker refuses new consumer runs until an owner refresh succeeds (it forces the refresh even when the file is fresh), a re-login succeeds, or a human runs `clear-quarantine --auth-block`. Exit 77 alone is also sysexits `EX_NOPERM`: it gives `FAILED` with the anomaly `CONSUMER_EXIT_77_WITHOUT_AUTH_FAILURE_RECORD` and blocks nothing. Network, rate-limit and server failures must use other codes and never block. `account-probe` implements the contract: 0 OK, 77 with the record for an auth failure, 69 service unavailable. |
| Canonical integrity | Every run records canonical metadata before and after, and the guardian re-checks it before a late release. Any change during or after a consumer run is `CANONICAL_CHANGED_BY_CONSUMER` or `CANONICAL_CHANGED_AFTER_SUPERVISOR`: state `QUARANTINED` (78) plus the quarantine marker. |
| Credential links | Codex saves in place and a re-login installs by rename, so the canonical file always has one link. The canonical preflight of `run` and `refresh` refuses a second link (`canonical_auth_hardlinked`, `QUARANTINED`, marker `CANONICAL_HARDLINKED`): a hard link elsewhere is a credential copy that survives rotation. After every consumer run, and again in the guardian before a late release, the supervisor scans the run directory with `lstat`/`stat`/`realpath` only (never opening a file, never following a symlink into a directory). It looks for a hard link to the credential, or a symlink that resolves to it or to a directory that contains it, other than the run's own `codex-home/auth.json`. Each one is removed by name within its own directory descriptor, so the removal can never reach the credential. A hard link is removed only while the canonical path still names that file. If the consumer replaced the canonical path, the link may be the only copy of the original, and it is kept for review (`kept`). Any finding gives `QUARANTINED` with `CREDENTIAL_LINK_IN_RUN_DIR`, listed in the receipt's `credential_links` (run-relative paths). A second link that remains outside the run directory gives `CANONICAL_HARDLINKED`. A directory the scan cannot read is recorded as `CREDENTIAL_LINK_SCAN_INCOMPLETE`, without a quarantine. |
| Re-login | Only for an unusable credential, in an empty staging home, installed by an atomic rename after success. See [Re-login](#re-login-maintenance). |
| Non-secret records | `manifest.json`, `receipt.json` and `state/supervisor-events.jsonl` in the run directory, plus `state/lease-guardian-*.json`. Lease sidecars hold only pid, guardian pid, host id, process start time, run id, role, operation, mode and acquired/released times. The owner ledger `<lease_path>.ledger.jsonl`, the quarantine marker `<lease_path>.quarantine`, the auth-block marker `<lease_path>.auth-blocked` and the binding marker hold no secrets. Child stdout/stderr go to `state/logs/` and are never parsed into receipts. |

## Configuration

Copy `config.example.json` to an ignored local file, fill in physical absolute paths and make it 0600.
The supervisor refuses group- or world-writable configs. Keys:

- `canonical_auth_home`: directory holding the one `auth.json`. It must be 0700, owned by you, a
  physical path, and contain a 0600 regular file. The first managed command adds the non-secret
  lease binding file next to `auth.json`.
- `lease_path`: the single lock identity for this auth source. Every checkout and worktree using
  the same login must point at the same file; the binding refuses any other. Its sidecars, ledger
  and markers live next to it. Its directory must not contain `runs_root`.
- `runs_root`: where per-run directories are created. It must not overlap the auth home. A re-login
  installs the new login by `rename` from a run directory, so it must be on the same filesystem as
  the auth home.
- `codex_bin` and `codex_bin_sha256`: the official CLI and its pin. The pin is mandatory for
  `refresh` and `relogin`, and verified on every run. `binary_pins` records more pins.
- `expected_user` (optional): refuse unless the effective user has this name.
- `blocker_url` (loopback only; keep the default port 0) and `blocker_must_be_closed`.
- `allow_shared_consumers`: Phase 2 only (see below). Default `false`.
- `refresh_policy.max_age_seconds` (default 43200) and `refresh_policy.pre_run`
  (`never`, `if-stale` or `always`). With `if-stale`, a shared consumer upgrades to the exclusive
  lease only when a refresh is actually due.
- `owner.*`: RPC timeout, exit wait, SIGTERM grace, `passthrough_env`, and the test-only
  `refresh_token_url_override` (loopback only).
- `relogin.experimental_issuer`: test-only loopback device-code issuer for qualification.
- `codex_config`: extra allowlisted Codex keys, such as `chatgpt_base_url`, `openai_base_url`
  or `model`. The pinned auth keys and the permission keys cannot be overridden.
- `extra_denied_paths` (optional): up to 64 absolute, normalized paths that model tool commands
  are denied in every role whose profile the supervisor generates, on top of the credential paths.
  Use physical paths (no symlinks). A leading `//` counts as not normalized.
- `roles.<name>`:
  - `tool_permissions` (**required**; see [Tool commands](#tool-commands-and-the-credential)):
    `{"extends": ":workspace"|":read-only", "network_enabled": false, "deny_paths": [],
    "read_paths": []}` for a generated profile, `"consumer-managed"`, or `"unrestricted"`.
    `deny_paths` and `read_paths` are optional, each up to 64 absolute, normalized paths.
  - `passthrough_env`, `allow_openrouter`, `deadline_seconds` and `codex_config`.
  - `path_env`: Harbor needs the directory that holds `docker`.
  - `stop_scope` (`tree` or `root-first`) and `term_grace_seconds`. With `root-first`, cancel and
    deadline send SIGTERM to the root alone and wait its grace, so Harbor can run its own cancel
    path and remove its containers. Only then is the rest of the tree signalled.
- `docker.bin`, `docker.env` (`DOCKER_*`), `docker.wait_seconds`, `docker.poll_seconds`: used for
  `--harbor-job-label`.

Supervisors before 1.2.0 ignore unknown top-level keys, so they silently drop
`extra_denied_paths`, and they refuse role `deny_paths` and `read_paths`. Do not run a config that
uses these keys with an older supervisor. The config schema stays
`gptgrep.codex-auth.config.v1`.

Compatibility of 1.3.0 with earlier consumers:

- A consumer that reports an auth failure by exiting 77 without the auth-failure record is now
  `FAILED` (exit 1). Earlier supervisors blocked every later consumer on exit 77 alone. No
  credential is at risk: the next consumer meets the same auth failure. Consumers that use the
  contract must write `$GPTGREP_AUTH_FAILURE_RECORD` first; `account-probe` does. Earlier
  supervisors ignore the record.
- `GPTGREP_AUTH_LEASE_FD` is no longer set and no lease descriptor is inherited. Nothing in this
  repository used it; full62 uses only its container label.
- A canonical file with a second hard link is refused at preflight and quarantined.

## Commands

```text
codex_auth_supervisor.py --config CFG status
codex_auth_supervisor.py --config CFG run --role R --run-id ID [--lease exclusive|shared]
    [--deadline-seconds N] [--wait-seconds N] [--harbor-job-label KEY=VALUE]
    [--pre-refresh never|if-stale|always] [--live-request-attempted yes|no|unknown]
    [--manifest-field KEY=VALUE]... [--stdin null|inherit] [--dry-run] -- CMD...
codex_auth_supervisor.py --config CFG refresh --run-id ID [--force] [--wait-seconds N]
codex_auth_supervisor.py --config CFG relogin --run-id ID --confirm-relogin
codex_auth_supervisor.py --config CFG clear-quarantine --run-id ID --confirm [--auth-block] --reason TEXT
codex_auth_supervisor.py --config CFG rebind-lease --run-id ID --confirm --reason TEXT
codex_auth_supervisor.py account-probe --codex-bin BIN --out FILE [--stderr-log FILE]   # inside `run` only
```

`CMD` may use the placeholders `{codex_home} {state_dir} {log_dir} {work_dir} {home} {tmp}
{results} {run_dir}`. The child's `PATH` is fixed, so pass absolute executables. Do not use a wrapper
that re-exports a shared `CODEX_HOME`. For GPTgrep the intended shape is `--codex-bin <official
binary> --codex-home {codex_home} --codex-state-dir {state_dir}`; it is designed but has not been
run under the supervisor (see [limitations](#limitations-and-unmanaged-entry-points)).

`account-probe` never writes into the credential paths:

- It refuses an `--out`, `--stderr-log` or record path equal to or inside the canonical auth home,
  the lease directory or any run's `codex-home`. It compares in every spelling, with symlinks
  resolved, and reads those paths from the run's `manifest.json`
  (`account_probe_output_in_protected_path`).
- It opens `--stderr-log` with `O_NOFOLLOW|O_APPEND|O_CREAT` (0600). It refuses a symlink as the
  last component, and a file with more than one link, which an append would write through
  (`account_probe_stderr_log_refused`).

Use `{log_dir}/account-probe.stderr.log`.

| State | Exit | Meaning |
| --- | --- | --- |
| `PASS` | 0 | Child exited 0, the sweep was clean, the canonical file is unchanged and the link was removed (for `refresh`: refreshed, or skipped as fresh) |
| `FAILED` | child code (1 to 63), else 1 | Child failed for a non-auth reason, including exit 77 without the auth-failure record; nothing retried |
| `CONFIG_ERROR` | 64 | Refused before launch (config, pin, lease safety, lease binding, flags, consumer argv guard) |
| `RELOGIN_NOT_REQUIRED` | 64 | `relogin` refused: the credential is still usable |
| `REFRESH_UNAVAILABLE` | 69 | Owner refresh failed transiently (network, 429, 5xx); credential unchanged and usable; retry later |
| `INTERNAL_ERROR` | 70 | Unexpected supervisor error before launch; only the exception type is reported |
| `RUN_ALREADY_EXISTS` | 73 | Run id reused; choose a new id |
| `AUTH_BUSY` | 75 | Lease not acquired within the wait bound |
| `UNKNOWN` | 76 | Processes or containers outlived the child, the Harbor label never matched, the sweep was inconclusive, a signal interrupted the post-run waits, or an error followed the launch. The guardian keeps the lease. Never retried automatically |
| `BLOCKED_AUTH` | 77 | Canonical preflight failed, a permanent or unclassified owner refresh failure, the blocker port has a listener at launch, a consumer exited 77 with its auth-failure record, an auth block is pending, or re-login failed or was declined |
| `QUARANTINED` | 78 | The quarantine marker exists, or this run created it (also for a second hard link to the credential at preflight, or a credential link in the run directory) |
| `TIMEOUT` | 124 | Deadline: SIGTERM (root-first or tree), grace, then SIGKILL (consumers only) |
| `CANCELLED` | 130 | SIGINT/SIGTERM/SIGHUP during the run, forwarded per the role's `stop_scope` |

## Concurrency phases

- **Phase 1 (default):** every consumer takes the exclusive lease, so at most one managed consumer
  uses the credential at a time. In-job concurrency must be 1 where a tool hard-codes several
  app-servers.
- **Phase 2 (`allow_shared_consumers=true`):** consumers take shared leases and the owner still
  needs exclusive. The qualification shows four shared consumers with expiring tokens never reach
  the authority, and the owner waits until all have finished. Enable it only after reviewing that
  evidence for your build.

## Refresh cadence

The owner cannot see token expiry, because the credential is never decoded. Choose one of these:

- (a) `--pre-refresh always` before every unit. This rotates the refresh token each time.
- (b) `if-stale` with an approved `max_age_seconds`.
- (c) Run units shorter than the remaining access-token life and refresh between them.

A consumer whose access token expires mid-run fails closed with an auth error. It does not refresh.
If the token is within five minutes of expiry when the owner starts, Codex refreshes once at startup
and once for the explicit request, presenting each rotated token once. Both writes are the owner's
and both are recorded.

## Harbor

- full62 stamps the label `gptgrep.auth-job=<runtime run_id>` on every trial service it generates
  (reader and judge compose files). Harbor gives each trial its own compose project, so that label
  is the only job-wide handle.
- Launch the Harbor executable directly under `run --role harbor --harbor-job-label
  gptgrep.auth-job=<run_id>`. Never launch through a local wrapper that
  re-exports the canonical `CODEX_HOME`.
- The supervisor polls the label while Harbor runs and requires at least one matching container
  before it accepts "cleared". A label that never matched gives `UNKNOWN`, so a wrong label cannot
  release the lease early. After Harbor exits, the supervisor waits for the label to clear. The
  guardian keeps watching it after an `UNKNOWN` result, even when the first check was clear.
- The `harbor` role uses `stop_scope=root-first` with a long `term_grace_seconds`. Harbor turns
  SIGTERM into its own cancel path, which removes the trial containers.
- **Qualified with a real Docker engine, not with Harbor:** the optional container pass ran plain
  containers from a local image with `--network none`, a fake credential and the full62 mount. It
  showed four things: the label gate holds the lease until the container is gone (and the guardian
  keeps it when the container outlives the wait); the read-only single-file bind refuses writes
  (`EROFS`); a host in-place save is visible inside a running container on OrbStack; and Linux
  refuses a connect to port 0.
- **Not yet qualified:** a real Harbor trial under the supervisor, and the Linux Codex binary's
  refresh, 401 and guarded-reload behaviour inside a trial container. Keep Harbor shared-login runs
  `BLOCKED` until one minimal non-live Harbor trial with fake auth and `network: none` passes.

## Tool commands and the credential

Model tool commands normally inherit `CODEX_HOME`, so without a permission profile they can read the
credential through the run's link (and, before the supervisor removes it, write to it). Each role
must state who prevents that:

- `{"extends": ":workspace"|":read-only", "network_enabled": false, "deny_paths": [...],
  "read_paths": [...]}`: the supervisor generates `default_permissions = "gptgrep_codex_auth"`.
  The profile extends the chosen built-in. Its filesystem table holds:
  1. `deny` for the canonical auth home, the run's `codex-home/auth.json` and the lease directory
     (markers and ledger);
  2. `deny` for every `extra_denied_paths` entry;
  3. `deny` for the role's `deny_paths`;
  4. `read` for the role's `read_paths`.

  The official 0.162.1 `command/exec` path was qualified with the official binary: the deny holds
  through the link, the workspace stays writable, and an `unrestricted` negative control could read
  the file. This needs `--unguarded-tool-sandbox`: Codex's seatbelt sandbox cannot be applied inside
  another sandbox, so a consumer that itself runs inside a sandbox cannot use this mode.
- `"consumer-managed"`: the consumer selects its own permission profile. GPTgrep's native OCR and
  Document roles use deny-root profiles, and Harbor full62 denies `/opt/full62-home`. The
  supervisor cannot verify this; receipts record `verified_by_supervisor=false`.
- `"unrestricted"`: tool commands can read the credential. Use this only for consumers that run no
  model tools, such as `account-probe`.

Codex 0.162.1 applies the most specific entry, and of two equally specific entries `deny` wins.
A read path therefore opens a directory below a deny, which is what a carve-out is for. It can
never reopen a supervisor deny:

- At config load, a read path equal to or inside the canonical auth home, any run's
  `codex-home/auth.json` or the lease directory is refused (`config_role_read_path_reopens_credential`).
  The same applies to an `extra_denied_paths` entry (`config_role_read_path_reopens_extra_deny`).
  A path listed in both `deny_paths` and `read_paths` is refused
  (`config_role_read_path_equals_deny_path`).
- Before launch, read paths are checked again against the run's actual denies with symlinks
  resolved (`role_read_path_resolves_into_denied_path`). Codex resolves read roots again for every
  sandboxed command, so keep read paths on directories that no other process can replace with a
  symlink while a run is live.

Receipts and dry runs list every deny in `tool_credential_access.denied_paths` and the carve-outs in
`tool_credential_access.read_paths`.

### Consumer argv guard

A consumer command line can replace the generated profile. In 0.162.1, a `--sandbox` override
switches config resolution to the legacy syntax and silently ignores `default_permissions`
(`core/src/config/mod.rs`, `resolve_permission_config_syntax`), so tool commands would run without
the credential denies. For a role with a generated profile, the supervisor therefore inspects the
consumer's argv when its executable is Codex. The executable is resolved as the launch will resolve
it: a bare name through the role's `PATH`, a relative path against the work directory. It counts as
Codex in any of these cases:

- it is the pinned binary (the same file, also through a symlink or hard link);
- its name, or its symlink target's name, is `codex` or starts with `codex-` or `codex.`, in any
  letter case (release binaries such as `codex-aarch64-apple-darwin`, shims such as `codex.js`);
- it cannot be resolved (then it cannot launch either).

The run is refused before anything is created (`CONFIG_ERROR`,
`consumer_argv_overrides_permission_profile`, with the argument as the detail) when the argv, up to
`--`, contains any of these:

- `--sandbox` or `-s` in any form (`-s VALUE`, `-sVALUE`, `--sandbox=VALUE`);
- `--dangerously-bypass-approvals-and-sandbox` or its alias `--yolo`;
- `--approve-for-me` or its alias `--not-so-yolo`, which set `sandbox_mode`;
- `--full-auto` (not in 0.162.1; refused for other builds);
- a `-c`/`--config` override whose key starts with one of these:
  - `sandbox_mode`, `default_permissions`, `permissions` or `sandbox_workspace_write`;
  - `projects`: project trust loads a repository's `.codex/config.toml` above the generated
    config, and that layer may set the keys above;
  - `mcp_servers`: MCP servers run outside the sandbox;
  - `approvals_reviewer`: a model reviewer could approve permission escalations;
  - a key the supervisor pins in the generated `config.toml` (`cli_auth_credentials_store`,
    `forced_login_method`, `notify`, `memories`, `cloud`, `analytics`, `feedback`, `history`,
    `check_for_update_on_startup`, `log_dir`), `hooks`, `plugins` or `marketplaces` (hooks,
    notify and plugin servers run outside the sandbox), and `features` itself or a pinned
    feature (`use_agent_identity`, `secret_auth_storage`, `memories`, `hooks`, `plugins`,
    `apps`);
- `--enable` with a pinned feature;
- `--profile` or `-p`, which layers `$CODEX_HOME/<name>.config.toml`;
- `--ignore-user-config`, which skips the generated `config.toml`;
- `--remote`, which runs threads on another app-server under its own config;
- for `codex sandbox`: `--sandbox-state-json`, `--sandbox-state-readable-root`,
  `--allow-unix-socket`, and `-P`/`--permission-profile` (alias `--permissions-profile`) naming
  any profile other than `gptgrep_codex_auth`.

Codex makes the working directory and every added directory writable project roots. Such a root is
more specific than any deny above it: with `-C` set to a directory below a denied directory, tool
commands can read and write it (qualified in the unguarded pass). The guard therefore checks every
`-C`/`--cd` and `--add-dir` directory:

1. This run's own `{work_dir}`, `{results}`, `{tmp}` and `{home}`, and paths inside them, are
   allowed first. Both the value as written and its resolved form (symlinks resolved) must lie in
   one of them. They are the run's intended writable roots: Codex makes the working directory one
   even without `-C`. This also holds when a role deny such as `/Users` contains `runs_root`. The
   production Judge shape `codex exec ... -C {work_dir} ... -o {results}/verdict.json -`, with the
   role denies `/Users`, `/Volumes` and the agent scratch directory, passes the guard (qualified with
   a fake Codex and, unguarded, with the official `codex sandbox -C {work_dir}`).
2. Any other directory equal to or inside any deny of the profile is refused
   (`consumer_argv_reopens_denied_path`).
3. Any other directory inside `runs_root`, or containing it, is refused
   (`consumer_argv_root_overlaps_run_state`). Tool commands could otherwise rewrite a generated
   `config.toml` or the supervisor's records of this or another run. `--add-dir /` is refused for
   this reason.

The detail is `FLAG PATH`. Values are resolved as Codex resolves them: a leading `~` or `~/` is the
run's home, relative values are resolved against the run's work directory, and an added directory
also against each `-C` directory. Directories outside every deny and outside `runs_root`, or above
a deny, stay allowed. Keep `runs_root` out of `/tmp`: the `:workspace` built-in makes `/tmp`
writable without any argument.

A root below an own directory may not exist at launch, so a tool command could create it as a
symlink. On macOS, Codex 0.162.1 does not follow it. It refuses a writable root with a symlink
component (outside the `allow_symlinked_codex_home` opt-out, which the generated config never
sets), keeps writable roots uncanonicalized, and denies unlinking a root directory
(`sandboxing/src/seatbelt.rs`). This comes from the source and is not qualified here. Prefer the run's
own directories themselves as roots.

Paths are compared in every spelling that names the same file on macOS: letter case and Unicode
normalization are folded, a leading `//` counts as `/`, and symlinks are resolved. The same rules
apply to `read_paths`. On a case-sensitive volume this can refuse a path that is in fact different.

Arguments after a positional argument are still inspected, because Codex accepts options there. A
`codex sandbox` command whose own arguments look like these flags must follow `--`. The receipt
records the outcome in `tool_credential_access.consumer_argv_guard`: `checked`,
`not_a_codex_command` or `not_applicable` (no generated profile).

The guard reads only the consumer's own argv. It does not cover:

- non-Codex consumers that launch Codex themselves, such as a GPTgrep binary, Harbor, a shell or an
  `env` wrapper: their Codex command lines are invisible to the supervisor;
- app-server and `exec-server` clients, which choose a sandbox, a profile or a working directory
  per request over JSON-RPC (`thread/start` `sandbox`, `permissions` or `cwd`; `command/exec`
  `sandboxPolicy`, `permissionProfile` or `cwd`);
- permission escalations approved by a person or an app-server client, and feature switches such
  as `--enable` with `request_permissions_tool` or `exec_permission_approvals`.

Such consumers must keep the generated profile themselves. Until every consumer chain has been
qualified, the ISO-02 and SEC acceptance items stay `BLOCKED`.

## Limitations and unmanaged entry points

The lease covers only processes the supervisor launches. These entry points use the canonical
`CODEX_HOME` with no lease and must not be used with the shared login. Treat them as `BLOCKED` for
shared-login use until each is routed through `run`:

- Local workstation wrappers outside this repository that force or export the canonical
  `CODEX_HOME`, or do not unset `CODEX_API_KEY`.
- Local metadata front doors that keep their own lock file.
- The harbor-ablation Rust harness (`judge_app_server`, `judge_backend`, `codex_agent`) sets the
  refresh and revoke blockers and loopback `NO_PROXY` in its children. It takes no lease. Use it with
  the shared login only under `run`.
- Scripts with built-in concurrency (system eval, PageIndex) and OCR's two app-servers need
  in-job concurrency 1 under the exclusive lease.
- No GPTgrep product, OCR, Document or Judge call chain, and no Harbor trial (only plain containers
  from a local image), has been run under the supervisor yet. CRED-11 and CTR-05 stay open.
- The owner attributes every canonical change made while it holds the exclusive lease to itself. An
  unmanaged writer active at the same time cannot be told apart.
- Process tracking cannot see a daemon that escapes between samples, closes inherited descriptors
  and leaves the run directory.
- **Lease binding marker (residual risk).** The marker is a plain file in the canonical auth home;
  tool commands are denied it, but the owner's account can delete it. After a deletion, the next
  command whose config names another lease path binds the auth source to that lease. From then on
  the original config is refused (`lease_binding_mismatch`). A run that was already in progress
  under the original lease can still overlap with runs under the new one, including a refresh
  owner. A second copy of the binding in the lease directory would not prevent this: the new config
  never looks at the old lease directory. Only a lock on the auth home itself would, and that is
  not implemented. Never delete the marker; use `rebind-lease`.
- Consumers do not hold the lease descriptor. If the supervisor and its guardian are both killed
  with `SIGKILL` while a consumer runs, the lease is free although the consumer still runs. The
  guardian ignores `SIGINT` and `SIGHUP`, and the supervisor replaces a guardian that dies while it
  lives.
- The credential-link scan covers this run's directory only. A symlink to the credential elsewhere
  is not detected. A hard link elsewhere is detected through the link count, but cannot be located
  or removed.

## Live runbook

Run each step only within the authorization and budget that apply to it. Keep the device code out of
files. Record receipts under your private evidence location.

1. **Preflight, no network.** Run `status`. Check all of these:
   - the lease probe reports `free`, and the binding, if present, matches your config;
   - there is no quarantine and no auth block;
   - the canonical preflight is `ok`;
   - the blocker probe is `closed`.

   Then run `run --dry-run ... -- CMD` and review the argv, the environment key names, the
   generated `config.toml` and `tool_credential_access`. Confirm that no
   [unmanaged entry point](#limitations-and-unmanaged-entry-points) is running against the same cache.
2. **First live step: consumer probe with the blocker, no refresh, no model request.**
   ```text
   codex_auth_supervisor.py --config CFG run --role dev --run-id live-probe-001 \
     --live-request-attempted yes --deadline-seconds 300 -- \
     /abs/python3 -I -B /abs/codex_auth_supervisor.py account-probe \
     --codex-bin /abs/official/codex --out {results}/account-probe.json \
     --stderr-log {log_dir}/account-probe.stderr.log
   ```
   This calls `account/read` with `refreshToken=false` through the real backend. It sends no
   model request and can never refresh.

   Expect `PASS`, `outcome=OK` and `canonical_changed=false`. `BLOCKED_AUTH` (`AUTH_*`) means
   "needs an owner refresh" and sets the auth block; it never means "re-login". A child exit of 69
   (`SERVICE_UNAVAILABLE`) is a network, rate-limit or server problem and blocks nothing.
3. **Owner refresh, by policy only.** Run `refresh --run-id live-own-001` without `--force`. If
   the canonical file is younger than `max_age_seconds` and no auth block is pending, the result is
   `SKIPPED_FRESH` and nothing is contacted. Use `--force` only when a policy decision requires it.
   Expect `REFRESHED`, `inode_preserved=true`, `integrity_ok=true` and a ledger entry.
4. **Bounded consumer runs.** Repeat step 2's shape with the real command, a new run id and an
   explicit deadline. For Harbor jobs, also pass `--harbor-job-label`. One unit at a time in Phase 1.
5. **Afterwards.** Check `status`: no unreleased holders, no quarantine, no auth block, and the
   canonical preflight `ok` (`canonical_auth_hardlinked` means a second hard link exists).
   - Archive only `receipt.json`, `manifest.json` and `results/` from each run directory, never the
     whole directory, and never from a `QUARANTINED` run before its review.
   - Copy symlinks as symlinks: `cp -RP`, `rsync -a` without `-L`, `--copy-links` or
     `--copy-unsafe-links`, or `shutil.copytree(..., symlinks=True)`. A copy that follows symlinks
     copies whatever a link names, the credential included.
   - The post-run scan removes hard links and symlinks to the credential below the run directory
     and lists them in the receipt's `credential_links`. Such a run is `QUARANTINED`
     (`CREDENTIAL_LINK_IN_RUN_DIR`). An entry marked `kept` is the only copy of an original
     credential whose canonical path the consumer replaced: review it, never archive it.
   - A finished run has no `codex-home/auth.json`. A file found there after a `QUARANTINED`
     `RUN_AUTH_LINK_REPLACED` run is a credential: remove it after review, and do not archive it.

## Failure recovery

- `AUTH_BUSY`: another managed run or a guardian holds the lease. Use `status` to find it (holder
  records, including `guardian_pid`). Wait; do not delete the lock file. Deleting it is refused
  afterwards (`lease_file_missing`) until `rebind-lease` is run with no live holders.
- `UNKNOWN`: inspect `receipt.json` → `sweep` and `docker`, and `state/lease-guardian-*.json`.
  - Verify that the listed pids and containers belong to this run before stopping them.
  - The guardian releases the lease once they are gone. It works from a config snapshot taken when
    it started, so later config edits cannot release the lease early.
  - Never rerun automatically: check for duplicate side effects or charges first.
- `QUARANTINED`: something changed the credential, replaced a run's link, linked the credential,
  listened on the blocker, or an owner was interrupted or killed. Find the cause, such as an
  unmanaged entry point, a missing blocker, a tool bypassing the run home or a harvester. After
  review, run `clear-quarantine --confirm --reason "..."`. The marker is archived, not deleted.
  - `CREDENTIAL_LINK_IN_RUN_DIR`: the receipt's (or `lease-guardian-result.json`'s)
    `credential_links` lists what was removed and what was `kept`.
  - `CANONICAL_HARDLINKED`: a second hard link to the credential exists outside every scanned run
    directory. Find it by inode, with metadata only: `find DIR -xdev -samefile
    <canonical auth home>/auth.json` compares `stat` results and never opens a file. Remove that
    link after review, never the canonical file, then clear the quarantine.
- `BLOCKED_AUTH` after `refresh`: read `refresh.owner_log_failure_class` and
  `owner_log_failure_kind`. Do not loop.
  - A **permanent** class means the refresh token is dead: re-login (below).
  - An unclassified failure needs investigation; it does not allow a re-login on its own.
- `REFRESH_UNAVAILABLE`: a transient failure (network, 429, 5xx). The credential is unchanged.
  Retry later, after the network is fixed.
- `BLOCKED_AUTH` from a consumer (exit 77 with its auth-failure record, or a pending auth block): run
  the owner refresh, then repeat with a new run id. Never add an API key or switch accounts. A
  consumer that exited 77 without the record is `FAILED` with
  `CONSUMER_EXIT_77_WITHOUT_AUTH_FAILURE_RECORD`; it blocks nothing.
- `OWNER_SIGKILLED_AFTER_GRACE` / `OWNER_REFRESH_UNRECORDED`: the in-place save may be torn, or
  nobody judged the write. Run `status`; the canonical preflight must still pass. If the credential
  is empty or no longer loads, the re-login gate allows a re-login. Never restore an old copy.

## Re-login (maintenance)

The official `codex login --device-auth` first **revokes the refresh token in the `auth.json` of the
`CODEX_HOME` it runs in and deletes that file**, before it shows a code (`clear_existing_auth_before_login`
in 0.162.1). Run directly against the canonical home, a declined or expired code would destroy the
only shared login. The supervisor therefore:

1. Allows a re-login only when the credential is unusable. Any one of these is enough:
   - the canonical `auth.json` is missing or empty;
   - a quarantine records a permanent owner failure;
   - the pinned official `login status`, run in a separate linked home, reports not logged in.

   Otherwise it ends `RELOGIN_NOT_REQUIRED`.
2. Runs the pinned `codex -c cli_auth_credentials_store="file" -c forced_login_method="chatgpt" -c
   log_dir=<run>/state/logs login --device-auth` with `CODEX_HOME` set to the run's empty
   `codex-home` (a staging home on the auth home's filesystem). Both the refresh and revoke
   endpoints point at the blocker. Nothing is revoked or deleted. Terminal I/O is passed through
   and never captured, and a notice explaining this is printed first.
3. Runs `login status` against the staging home. Only when both succeed does it rename the staged
   `auth.json` over the canonical file. Per-run symlinks follow the path. A staged login that cannot
   be verified is discarded.

Procedure:

1. Stop every consumer, then run `status`: no holders, lease `free`.
2. Tell the user this re-logs in the existing ChatGPT login for this auth source, and get explicit
   confirmation for this one action.
3. Run `relogin --run-id relogin-001 --confirm-relogin` in an interactive terminal.
4. Give the device code and the official page only in the user's private conversation. The user
   approves it personally. Only exit codes are recorded.
5. After `PASS`, a pending auth block is cleared. Clear any quarantine with a reason, then resume
   with new run ids.
6. If the code expires or the user declines, the state stays `BLOCKED_AUTH` and the existing login
   is untouched. A new code is a new, explicitly confirmed attempt. Never retry on a timer.

## Qualification

```text
python3 -I -B tests/run_qualification.py --results-dir /path/to/results \
    [--codex-bin /path/to/official/codex --codex-sha256 <pin>] [--unguarded-tool-sandbox \
    [--docker-bin /path/to/docker --docker-host unix:///path/to/docker.sock --docker-image <local image>]]
```

The runner writes `<results>/<UTC>/` containing `logs/unittest.log`, `summary.json` (counts,
per-test outcomes, non-secret observations), `runner-meta.json` and `sandbox.sb`.

- On macOS it re-executes under `sandbox-exec`. Outbound network is allowed only to its reserved
  loopback ports and port 9, so system proxies, DNS and every real endpoint are unreachable.
- Any access under the real `~/.gptgrep` and `~/.codex` is denied.
- Tests that run a real Codex binary refuse to run without this guard. A declared guard is not
  enough: they require a loopback connect to a port the runner did not reserve to be denied.
  Otherwise they fail in `setUpClass`, never as a skip, before any binary starts. The only
  exception is the explicit `--unguarded-tool-sandbox` pass below.
- All credentials are unsigned fake JWTs under the run's own temporary directory.

`--unguarded-tool-sandbox` (official binary only) adds one more pass for `unguarded_test_*.py`. That
pass needs Codex's own tool sandbox, which cannot nest inside the guard:

- every endpoint is a loopback fake;
- the blocker is port 0;
- `HTTP(S)_PROXY`/`ALL_PROXY` point at a loopback trap that must record zero connections. A
  failure reports each connection's request line (method and host only).
- The fake ChatGPT backend answers `accounts/check` with `workspace_backend_origin =
  "https://127.0.0.1:0"`: an https origin, as Codex requires, on loopback port 0, where nothing
  can listen and `NO_PROXY` applies. Codex's routed backend requests therefore fail at connect,
  with no proxy and no DNS lookup.
- Earlier rounds advertised `https://fake-backend.invalid`. After a thread and a sandboxed
  `command/exec`, Codex then intermittently routed a backend request through the proxy trap.
  One run captured `CONNECT fake-backend.invalid:443`; in others the connection closed before
  sending any byte.

Covered:

- (a) Lease semantics, the lease binding, and deleting the lock file while it is held.
- (b) Layout and environment; the auth-link lifetime on every exit path; a replaced link.
- (c) Owner refresh through the symlink with a fake authority: in place, inode kept, one request.
  Owner failure classes from the official log: reused, expired, revoked, `invalid_grant`, other
  401, 5xx, 429 and connection refused, each with valid and near-expiry tokens.
- (d) Consumers with expiring tokens never reach the authority. The port-0 blocker, guarded and
  unguarded.
- (e) Consumers adopt the owner's write, both at startup and through the guarded reload of a
  running app-server.
- (f) A 401 fails closed. The exit-77 contract (with the auth-failure record) blocks the next
  consumer until an owner refresh; 5xx and 429 do not block.
- (g) Four shared consumers, then the owner waits for exclusive. A shared `if-stale` consumer
  overlaps while the file is fresh.
- (h) Lifecycle:
  - SIGTERM grace and the SIGKILL anomaly; deadline and cancel; a root-first stop;
  - SIGINT/SIGTERM during the sweep and during the Harbor container wait;
  - a killed supervisor, after the child exits and during an owner refresh;
  - the post-launch exception path;
  - the guardian's canonical re-check and its continued label watch;
  - the guardian does not adopt a reused root pid;
  - a listener appearing on the blocker port.
- (i) `CODEX_API_KEY` stripped. The unsupervised negative control shows `codex exec` would log out,
  and that this only unlinks the run's symlink.
- (j) Leak canaries in every supervisor- and Codex-written run file, after every official-binary
  test.
- (k) Staged re-login with the official device flow against a fake issuer:
  - decline: no revoke and no change;
  - approve: installed by rename, and a consumer uses the new login;
  - with the fake CLI: gating, discarding an unverified login, and the clear-before-login model.
- Tool-command denial through `command/exec` (unguarded pass only), the fake Docker label gate, and
  the guard's self-check.
- (l) Configurable denies and read carve-outs: config limits, normalization and every reopen
  refusal, the generated `config.toml` and receipts, and the official app-server starting on the
  profile with `--strict-config`. In the unguarded pass, `codex sandbox --permission-profile
  gptgrep_codex_auth` runs as a supervised consumer without any model:
  - a role deny and an `extra_denied_paths` entry give `Operation not permitted`;
  - a read carve-out below a denied parent is readable;
  - the canonical credential stays denied below a read carve-out;
  - a role without that deny can read the file, while the extra deny still applies to it.

  The app-server starts a thread on the same profile, and its `command/exec` enforces the same
  denies and carve-out.
- (m) The consumer argv guard:
  - every refused form, including pinned keys and `--enable`, before any run directory or lease
    file exists;
  - executables found through the role's `PATH`, named `CODEX` or `codex-latest`, or not found;
  - `-C`/`--add-dir` directories inside a deny (relative, `~/…`, through `-C`, through a symlink,
    in another letter case, with a leading `//`), and directories inside `runs_root` outside this
    run's own directories (`~/../codex-home`, another run, `_owner`) or containing it;
  - allowed command lines: the supervisor's own `-P`, arguments after `--`, roots outside the
    denies, non-Codex consumers, and roles without a generated profile.

  In the unguarded pass, the official `codex sandbox` with `-C` below a role deny reads and writes
  there outside the supervisor, and the supervisor refuses the same command line.
- (n) Verification findings of 1.2.0:
  - the run's own directories as roots below a role deny that contains `runs_root`: the exact
    production Judge command line (dry run, and a fake-Codex run with `--stdin inherit` and
    `--lease shared`), `-C .`, `--cd={work_dir}`, `--add-dir {results}` and paths inside them
    pass, while other directories below the deny, other runs and the run's own `codex-home`,
    `state` and run directory stay refused. Unguarded, the official `codex sandbox -C
    {work_dir}` writes in the work directory while a sibling below the deny and the credential
    stay denied;
  - no lease descriptor reaches a consumer: `flock(LOCK_UN)` on everything it inherited, also
    from a sandboxed tool command of the official binary (unguarded), leaves the lease held;
  - hard links and symlinks to the credential in `results/` (to the file, to the run's link and to
    the auth home) are removed and quarantined; one outside the run quarantines after the run and
    at every later preflight; the only copy of a replaced original is kept; the guardian rescans
    after a survivor links the credential late;
  - `account-probe` refuses outputs in the protected paths, a symlink as the last component and a
    hard-linked log;
  - exit 77 without the auth-failure record, and malformed, oversized or symlinked records, block
    nothing;
  - real-binary tests refuse to run without a verified guard (guarded pass), also when the guard
    is only declared (unguarded pass);
  - eight concurrent first commands bind the auth source once (three rounds); a marker still
    being written is awaited, and one that stays empty is refused.
- With `--docker-bin/--docker-host/--docker-image` (unguarded pass): the label gate against plain
  containers on a real Docker engine. Also the read-only bind (`EROFS`), adoption of a host in-place
  save inside a running container, and Linux port-0 refusal.

`--docker-bin`, `--docker-host` and `--docker-image` name a local image with `python3`, which is
never pulled.

Not covered: any real login, token authority, ChatGPT backend or model; Harbor trials and the
Linux Codex binary inside a container; GPTgrep product, OCR, Document and Judge chains under the
supervisor.

## Verified Codex 0.162.1 behaviour this design depends on

Paths are relative to `codex-rs/` at source commit `092d3acd`.

- `login/src/auth/storage.rs` `FileAuthStorage::save`: truncates and rewrites `auth.json` in place
  and follows symlinks; there is no temp file, rename or fsync. `delete_file_if_exists` removes
  only a symlink.
- `login/src/auth/manager.rs`:
  - The refresh window is 5 minutes, with an 8-day `last_refresh` fallback.
  - `CODEX_REFRESH_TOKEN_URL_OVERRIDE` and `CODEX_REVOKE_TOKEN_URL_OVERRIDE` are read on each
    call; without the latter, the revoke endpoint is derived from the former.
  - `auth()` refreshes proactively and keeps the token on failure.
  - `refresh_token()` does a guarded reload, then contacts the authority.
  - `UnauthorizedRecovery` step 2 calls `refresh_token_from_authority` without comparing the file.
  - Each manager has an in-process `Semaphore(1)` and no cross-process lock.
  - `request_chatgpt_token_refresh` logs `Failed to refresh token status=… detail=TokenErrorDetail
    { error_code … }` at ERROR. It classifies 401, `invalid_grant` and the `refresh_token_*` codes
    as permanent and other statuses as transient.
- `app-server/src/request_processors/account_processor.rs`:
  - `account/read {refreshToken:true}` calls `refresh_token()` and discards the outcome.
  - It logs transient failures only at WARN.
  - After a permanent failure the account reads as `null`.
- `app-server/src/request_processors/account_processor/workspace_routing.rs`: `account/read`
  queries `<chatgpt_base_url>/…/accounts/check`. A 401 surfaces as `workspace routing discovery
  unauthorized (401)`; 429, 5xx and connection errors surface as `workspace routing discovery
  failed`.
- `cli/src/login.rs` `run_login_with_device_code`: `clear_existing_auth_before_login` →
  `logout_with_revoke` revokes and deletes the current `auth.json` before the user code is requested.
- `login/src/auth/manager.rs` `enforce_login_restrictions`: with `forced_login_method="chatgpt"`
  and a visible `CODEX_API_KEY`, `codex exec` logs out and deletes `CODEX_HOME/auth.json`.
- `arg0/src/lib.rs`: every invocation writes `CODEX_HOME/tmp/arg0` and refuses a `CODEX_HOME`
  under `TMPDIR`.
- `http-client/src/outbound_proxy.rs`: the default `ReqwestDefault` policy (`respect_system_proxy`
  is off) uses hyper-util's system matcher. On macOS that matcher reads the HTTP/HTTPS proxy but
  not the ExceptionsList, and applies `NO_PROXY` first.
- `app-server` `command/exec`: runs a command under the configured permission profile without a
  model request; `extends = ":workspace"` and `":read-only"` are the extensible built-ins.
- `protocol/src/permissions.rs`: filesystem entries resolve to the most specific path; of two
  equally specific entries `deny` beats `write`, and `write` beats `read`.
  `materialize_project_roots_with_workspace_roots` turns the working directory and every
  `--add-dir` into concrete `write` entries (`core/src/config/mod.rs` workspace roots; `exec`
  does not check that an added path is a directory). Observed with the official binary: `codex
  sandbox -C` set to a directory below a denied directory can read and write it.
- `core/src/config/mod.rs` `resolve_permission_config_syntax`: a `sandbox_mode` override selects
  the legacy syntax, and `default_permissions` is then ignored. A `sandbox_mode` set by `-c` in
  the session flags does the same.
- `utils/cli/src/shared_options.rs`: `--yolo` is an alias of
  `--dangerously-bypass-approvals-and-sandbox`; `--approve-for-me` (alias `--not-so-yolo`) adds
  `sandbox_mode="workspace-write"`; `-p/--profile` layers `$CODEX_HOME/<name>.config.toml`.
  `exec/src/cli.rs` `--ignore-user-config` skips `$CODEX_HOME/config.toml`. There is no
  `--full-auto`.
- `cli/src/debug_sandbox.rs`: `codex sandbox -P NAME` adds `default_permissions=NAME`. With `-P`
  and without `--include-managed-config` it fetches no cloud config, so it needs no network.
- The app-server `chatgptAuthTokens` mode is labelled "FOR OPENAI INTERNAL USE ONLY" and is not used.
