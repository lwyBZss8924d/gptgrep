# codex-auth: shared ChatGPT credential supervisor

`codex_auth_supervisor.py` lets several local Codex consumers share one persistent ChatGPT login cache
(the *canonical auth home*) without copying it, without letting consumers refresh it, and without
sharing any other Codex state. It is a single Python 3.12+ standard-library file. It never opens,
reads, hashes or decodes the credential: every check uses `lstat` metadata.

Status: implemented and qualified against fake credentials and loopback fakes (see
[Qualification](#qualification)). Live behaviour against a real login is not established by this
document. Follow the [live runbook](#live-runbook) and record its results separately.

## What it enforces

| Control | Mechanism |
| --- | --- |
| One serial group per auth source | `flock` on `lease_path`, a file outside every checkout. The parent directory must be 0700 and owned by the caller. The file must be 0600, opened with `O_NOFOLLOW`, never a symlink and never hardlinked. Exclusive by default. Shared mode exists only with `allow_shared_consumers=true`. The wait is bounded and ends in `AUTH_BUSY` (75). |
| Whole-lifecycle lease | The lease descriptor is passed to the child (`pass_fds`) and is never `LOCK_UN`ed, so descendants that keep it keep the lease. The supervisor tracks the process tree throughout the run. After the child exits it sweeps for survivors before releasing: pid tree, process group and session, `lsof` holders of a per-run marker descriptor, and processes with files or cwd under the run directory. Survivors give `UNKNOWN` (76). A detached lease keeper then holds the lease until they, and any labelled Harbor containers, are gone. |
| Per-run isolation | `RUNS_ROOT/<role>/<run-id>/{codex-home,state,state/logs,home,tmp,work,results}`, all 0700. Run ids are never reused. `codex-home/auth.json` is an absolute symlink to the canonical file. `codex-home/config.toml` is generated. The canonical `config.toml`, skills and state are never copied. |
| Pinned Codex config | `cli_auth_credentials_store="file"`, `forced_login_method="chatgpt"`, `features.use_agent_identity=false`, `features.{secret_auth_storage,memories,hooks,plugins,apps}=false`, `memories.{generate_memories,use_memories}=false`, `cloud.skills.enabled=false`, `notify=[]`, `analytics/feedback` off, `history.persistence="none"`, `check_for_update_on_startup=false`, `log_dir=<run>/state/logs`. Every key is checked against the 0.162.1 schema; the qualification starts app-servers with `--strict-config`. |
| Allowlisted environment | The child environment is built from scratch: fixed `PATH`, `LANG`/`LC_ALL`, per-run `HOME`, `TMPDIR` and `XDG_*`, `CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_REFRESH_TOKEN_URL_OVERRIDE=<blocker>`, `CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED=1`, loopback `NO_PROXY`/`no_proxy`, `GPTGREP_AUTH_RUN_ID` and `GPTGREP_AUTH_SOURCE_ID`, plus any names a role lists. `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` and `OP_SERVICE_ACCOUNT_TOKEN` can never pass. `OPENROUTER_API_KEY` passes only for roles with `allow_openrouter=true`. |
| Consumers cannot refresh | Codex reads `CODEX_REFRESH_TOKEN_URL_OVERRIDE` on every refresh. The default blocker `http://127.0.0.1:9/oauth/token` is a closed port, and every run first checks that nothing listens there. Proactive refresh (token within 5 minutes of expiry), 401 recovery step 2, and explicit `account/read {refreshToken:true}` all fail transiently, and the token is never sent anywhere. A guarded reload still adopts an owner's rewrite. |
| Proxy safety | reqwest/hyper-util read the macOS system HTTP(S) proxy but **ignore its ExceptionsList**, while honouring `NO_PROXY`. Without the loopback `NO_PROXY`, a request to the blocker (body included) can be relayed through a local or remote proxy. The supervisor always sets it. |
| Single refresh owner | `refresh` holds the **exclusive** lease, so no consumer is alive during Codex's non-atomic in-place save. It drives the official `codex app-server --listen stdio:// --strict-config` in its own per-run home without the blocker: `initialize`, `initialized`, `account/read {refreshToken:true}`, then `account/read {}`, which must report a `chatgpt` account. Success requires the canonical `mtime_ns`/`size` to change while the inode, regular-file type, mode 0600 and the run's symlink stay intact. Otherwise the result is `AUTH_REFRESH_FAILED`, and a quarantine marker blocks every `run`/`refresh` until a human clears it. |
| Owner stop | Close stdin and wait. If the owner is still running, SIGTERM its group with a long grace. SIGKILL is sent only after the grace, and is then recorded as `OWNER_SIGKILLED_AFTER_GRACE` with a quarantine, because the save may have been torn. |
| Canonical integrity | Every run records canonical metadata before and after. Any change during a consumer run is `CANONICAL_CHANGED_BY_CONSUMER`: state `QUARANTINED` (78) plus the quarantine marker. |
| Non-secret records | `manifest.json`, `receipt.json` and `state/supervisor-events.jsonl` in the run directory. Lease sidecars hold only pid, host id, process start time, run id, role, operation, mode and acquired/released times. The owner ledger `<lease_path>.ledger.jsonl` and the quarantine marker `<lease_path>.quarantine` hold no secrets. Child stdout/stderr go to `state/logs/` and are never parsed into receipts. |

## Configuration

Copy `config.example.json` to an ignored local file, fill in physical absolute paths and make it 0600.
The supervisor refuses group- or world-writable configs. Keys:

- `canonical_auth_home`: directory holding the one `auth.json`. It must be 0700, owned by you, a
  physical path, and contain a 0600 regular file.
- `lease_path`: the single lock identity for this auth source. Every checkout and worktree using
  the same login must point at the same file. Its sidecars, ledger and quarantine marker live next to it.
- `runs_root`: where per-run directories are created. It must not overlap the auth home.
- `codex_bin` and `codex_bin_sha256`: the official CLI and its pin. The pin is mandatory for
  `refresh` and `relogin`, and verified on every run. `binary_pins` records more pins.
- `expected_user` (optional): refuse unless the effective user has this name.
- `blocker_url` (loopback only) and `blocker_must_be_closed`.
- `allow_shared_consumers`: Phase 2 only (see below). Default `false`.
- `refresh_policy.max_age_seconds` (default 43200) and `refresh_policy.pre_run`
  (`never`, `if-stale` or `always`).
- `owner.*`: RPC timeout, exit wait, SIGTERM grace, `passthrough_env`, and the test-only
  `refresh_token_url_override` (loopback only).
- `codex_config`: extra allowlisted Codex keys, such as `chatgpt_base_url`, `openai_base_url`
  or `model`. The pinned auth keys cannot be overridden.
- `roles.<name>`: `passthrough_env`, `allow_openrouter`, `deadline_seconds`, `codex_config`
  and `path_env`.
- `docker.bin`, `docker.env` (`DOCKER_*`), `docker.wait_seconds`: used for `--harbor-job-label`.

## Commands

```text
codex_auth_supervisor.py --config CFG status
codex_auth_supervisor.py --config CFG run --role R --run-id ID [--lease exclusive|shared]
    [--deadline-seconds N] [--wait-seconds N] [--harbor-job-label KEY=VALUE]
    [--pre-refresh never|if-stale|always] [--live-request-attempted yes|no|unknown]
    [--manifest-field KEY=VALUE]... [--stdin null|inherit] [--dry-run] -- CMD...
codex_auth_supervisor.py --config CFG refresh --run-id ID [--force] [--wait-seconds N]
codex_auth_supervisor.py --config CFG relogin --run-id ID --confirm-relogin
codex_auth_supervisor.py --config CFG clear-quarantine --run-id ID --confirm --reason TEXT
codex_auth_supervisor.py account-probe --codex-bin BIN --out FILE   # inside `run` only
```

`CMD` may use the placeholders `{codex_home} {state_dir} {log_dir} {work_dir} {home} {tmp}
{results} {run_dir}`. The child's `PATH` is fixed, so pass absolute executables. For GPTgrep, pass
`--codex-bin <official binary> --codex-home {codex_home} --codex-state-dir {state_dir}` explicitly.
Do not use a wrapper that re-exports a shared `CODEX_HOME`.

| State | Exit | Meaning |
| --- | --- | --- |
| `PASS` | 0 | Child exited 0, the sweep was clean and the canonical file is unchanged (for `refresh`: refreshed, or skipped as fresh) |
| `FAILED` | child code (1 to 63), else 1 | Child failed; nothing retried |
| `CONFIG_ERROR` | 64 | Refused before launch (config, pin, lease safety, flags) |
| `INTERNAL_ERROR` | 70 | Unexpected supervisor error; only the exception type is reported |
| `RUN_ALREADY_EXISTS` | 73 | Run id reused; choose a new id |
| `AUTH_BUSY` | 75 | Lease not acquired within the wait bound |
| `UNKNOWN` | 76 | Processes or containers outlived the child, or the sweep was inconclusive. The lease is retained by the keeper. Never retried automatically |
| `BLOCKED_AUTH` | 77 | Canonical preflight failed, `AUTH_REFRESH_FAILED`, the blocker port has a listener, or re-login failed |
| `QUARANTINED` | 78 | The quarantine marker exists, or this run created it |
| `TIMEOUT` | 124 | Deadline: SIGTERM to the group, grace, then SIGKILL (consumers only) |
| `CANCELLED` | 130 | SIGINT/SIGTERM/SIGHUP forwarded to the child's group |

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
and once for the explicit request. Both writes are the owner's and both are recorded.

## Live runbook

Run each step only within the authorization and budget that apply to it. Keep the device code out of
files. Record receipts under your private evidence location.

1. **Preflight, no network.** Run `status`. Confirm that the lease probe reports `free` and that there
   is no quarantine, that canonical preflight is `ok`, and that `canonical_age_seconds` is present.
   Then run `run --dry-run ... -- CMD` and review the argv, the environment key names and the
   generated `config.toml`. Confirm no unmanaged consumer is running against the same cache.
2. **First live step: consumer probe with the blocker, no refresh, no model request.**
   ```text
   codex_auth_supervisor.py --config CFG run --role dev --run-id live-probe-001 \
     --live-request-attempted yes --deadline-seconds 300 -- \
     /abs/python3 -I -B /abs/codex_auth_supervisor.py account-probe \
     --codex-bin /abs/official/codex --out {results}/account-probe.json \
     --stderr-log {log_dir}/account-probe.stderr.log
   ```
   This calls `account/read` with `refreshToken=false` through the real backend. It sends no
   model request and can never refresh. Expect `PASS`, `account_ok=true` and
   `canonical_changed=false`. If the token is near expiry, the blocked refresh attempts appear only
   in the app-server log. An auth error here means "needs an owner refresh"; it never means
   "re-login".
3. **Owner refresh, by policy only.** Run `refresh --run-id live-own-001` without `--force`. If
   the canonical file is younger than `max_age_seconds`, the result is `SKIPPED_FRESH` and nothing
   is contacted. Use `--force` only when step 2 showed an expired token or a policy decision
   requires it. Expect `REFRESHED`, `inode_preserved=true`, `integrity_ok=true` and a ledger entry.
4. **Bounded consumer runs.** Repeat step 2's shape with the real command, a new run id, an explicit
   deadline and `--harbor-job-label` for Harbor jobs. One unit at a time in Phase 1.
5. **Afterwards.** Check `status`: no unreleased holders and no quarantine. Then archive the
   receipts.

## Failure recovery

- `AUTH_BUSY`: another managed run holds the lease. Use `status` to find it (holder records). Wait;
  do not delete the lock file. The kernel releases it when the last holder exits.
- `UNKNOWN`: inspect `receipt.json` → `sweep` and `docker`. Verify that the listed pids and
  containers belong to this run before stopping them. The keeper releases the lease once they are
  gone. It works from a config snapshot taken when it started, so later config edits cannot
  release the lease early. Never rerun automatically: check for duplicate side effects or
  charges first.
- `QUARANTINED` / `CANONICAL_CHANGED_BY_CONSUMER`: a consumer, or something outside the supervisor,
  wrote the credential. Find the writer: an unmanaged entry point, a missing blocker, or a tool
  bypassing the run home. After review, run `clear-quarantine --confirm --reason "..."`. The marker
  is archived, not deleted.
- `BLOCKED_AUTH` / `AUTH_REFRESH_FAILED`: the owner could not refresh. See
  `refresh.owner_log_failure_class` (for example `REFRESH_TOKEN_REUSED` or
  `REFRESH_TOKEN_EXPIRED`). Do not loop. A transport failure may be retried once, after the
  network is fixed and the quarantine is cleared. A permanent class needs a re-login.
- `OWNER_SIGKILLED_AFTER_GRACE`: the in-place save may be torn. Run `status`; canonical preflight
  must still pass. If `auth.json` is empty or invalid, re-login. Never restore an old copy.
- Consumer 401 or "unauthorized" during a run: stop the unit. Run the owner refresh (step 3), then
  repeat with a new run id. Never add an API key or switch accounts.

## Re-login (maintenance)

1. Stop every consumer, then run `status`: no holders, lease `free`.
2. Tell the user this re-logs in the existing ChatGPT login for this auth source, and get explicit
   confirmation for this one action.
3. Run `relogin --run-id relogin-001 --confirm-relogin` in an interactive terminal. It takes the
   exclusive lease and runs the pinned official
   `codex -c cli_auth_credentials_store="file" -c forced_login_method="chatgpt" -c log_dir=<run>/state/logs login --device-auth`
   with `CODEX_HOME` set to the canonical home. Terminal I/O is passed through and never captured.
4. Give the device code and the official page only in the user's private conversation. The user
   approves it personally. Then `login status` runs, and only exit codes are recorded.
5. Login replaces `auth.json` (logout first, so the inode changes); per-run symlinks follow the path.
   Clear any quarantine with a reason. Resume work with new run ids.
6. If the code expires or the user declines, the state stays `BLOCKED_AUTH`. A new code is a new,
   explicitly confirmed attempt. Never retry on a timer.

## Qualification

```text
python3 -I -B tests/run_qualification.py --results-dir /path/to/results \
    [--codex-bin /path/to/official/codex --codex-sha256 <pin>]
```

The runner writes `<results>/<UTC>/` containing `logs/unittest.log`, `summary.json` (counts,
per-test outcomes, non-secret observations), `runner-meta.json` and `sandbox.sb`. On macOS it
re-executes under `sandbox-exec`. Outbound network is allowed only to its reserved loopback ports
and the closed port 9, so system proxies, DNS and every real endpoint are unreachable. Any access
under the real `~/.gptgrep` and `~/.codex` is denied. Tests that run a real Codex binary refuse to
run without this guard. All credentials are unsigned fake JWTs under the run's own temporary
directory.

Covered:

- (a) Lease semantics.
- (b) Layout and environment.
- (c) Owner refresh through the symlink with a fake authority: in place, inode kept, one request.
- (d) Consumers with expiring tokens never reach the authority.
- (e) Consumers adopt the owner's write, both at startup and through the guarded reload of a
  running app-server.
- (f) A 401 fails closed.
- (g) Four shared consumers, then the owner waits for exclusive.
- (h) SIGTERM grace and SIGKILL anomaly, `UNKNOWN` with the keeper, deadline and cancel.
- (i) `CODEX_API_KEY` stripped. The unsupervised negative control shows `codex exec` would log out,
  and that this only unlinks the run's symlink.
- (j) Leak canaries in every supervisor- and Codex-written run file.
- Fake re-login, the fake Docker label sweep, and the guard's self-check.

Not covered: any real login, token authority, ChatGPT backend or model, and real Harbor containers.

## Verified Codex 0.162.1 behaviour this design depends on

Paths are relative to `codex-rs/` at source commit `092d3acd`.

- `login/src/auth/storage.rs` `FileAuthStorage::save`: truncates and rewrites `auth.json` in place
  and follows symlinks; there is no temp file, rename or fsync. `delete_file_if_exists` removes
  only a symlink.
- `login/src/auth/manager.rs`:
  - The refresh window is 5 minutes, with an 8-day `last_refresh` fallback.
  - `CODEX_REFRESH_TOKEN_URL_OVERRIDE` is read on each call.
  - `auth()` refreshes proactively and keeps the token on failure.
  - `refresh_token()` does a guarded reload, then contacts the authority.
  - `UnauthorizedRecovery` step 2 calls `refresh_token_from_authority` without comparing the file.
  - Each manager has an in-process `Semaphore(1)` and no cross-process lock.
- `app-server/src/request_processors/account_processor.rs`: `account/read {refreshToken:true}`
  calls `refresh_token()` and discards the outcome.
- `app-server/src/request_processors/account_processor/workspace_routing.rs`: `account/read`
  queries `<chatgpt_base_url>/…/accounts/check` and routes model traffic to the returned workspace
  origin.
- `login/src/auth/manager.rs` `enforce_login_restrictions`: with `forced_login_method="chatgpt"`
  and a visible `CODEX_API_KEY`, `codex exec` logs out and deletes `CODEX_HOME/auth.json`.
- `arg0/src/lib.rs`: every invocation writes `CODEX_HOME/tmp/arg0` and refuses a `CODEX_HOME`
  under `TMPDIR`.
- `http-client/src/outbound_proxy.rs`: the default `ReqwestDefault` policy (`respect_system_proxy`
  is off) uses hyper-util's system matcher. On macOS that matcher reads the HTTP/HTTPS proxy but
  not the ExceptionsList, and applies `NO_PROXY` first.
- The app-server `chatgptAuthTokens` mode is labelled "FOR OPENAI INTERNAL USE ONLY" and is not used.
