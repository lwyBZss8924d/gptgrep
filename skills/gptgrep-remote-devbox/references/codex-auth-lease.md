# Codex Auth Single-Refresh Lease Broker Contract

## 1. Single-Refresh Invariant
When multiple agents or parallel processes share a single OpenAI subscription login, concurrent refresh token rotations cause invalidation of peer tokens.
The `codex-auth` architecture enforces a single-refresh-owner pattern:
- **Canonical Auth Home**: `/mnt/gptgrep-dev/development-artifacts/owner-private/codex-auth/canonical-home` holds the sole authoritative `auth.json`.
- **Exclusive Lease Lock**: `/mnt/gptgrep-dev/development-artifacts/owner-private/codex-auth/lease/gptgrep-codex.lock`.
- **Flock Concurrency Control**: Any process refreshing a token must hold an exclusive non-blocking `flock(fd, LOCK_EX | LOCK_NB)` on the lease lock. Concurrent workers wait and copy the refreshed token.

## 2. Port 0 Refresh Blocker
To guarantee no rogue or unmanaged child process can independently contact the auth authority:
- `CODEX_REFRESH_TOKEN_URL_OVERRIDE="http://127.0.0.1:0/oauth/token"`
- `CODEX_REVOKE_TOKEN_URL_OVERRIDE="http://127.0.0.1:0/oauth/revoke"`
Attempting to connect to loopback port 0 is rejected by the kernel, immediately failing closed with exit code 77 (`BLOCKED_AUTH`).

## 3. Bubblewrap Descendant Pruning
Under Linux user namespaces, mounting a read-only parent directory followed by a deny rule on a child path within that mount produces `EROFS` collision errors in Bubblewrap (`bwrap`). The supervisor automatically detects ancestor deny relationships and prunes descendant deny entries before constructing the `bwrap` command line.
