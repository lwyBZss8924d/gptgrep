# 1Password CLI modes and secret references

Adapted from the OpenClaw `1password` Skill at revision `e5441c22d89`.
Use the installed CLI help for commands and flags; its version may change.

## Select the authentication path

| Mode | Use here | Interaction |
| --- | --- | --- |
| Configured AI-vault Service Account | Ordinary read-only AI work in the designated vault | `bin/op-self` loads its token from macOS Keychain and starts `op`; no per-command desktop authorization in the observed logged-in session. |
| Desktop app integration | Human-owned vaults, creating or changing 1Password items, or account administration | Run `op` directly. The app can request Touch ID. Do not assume the Service Account has these permissions. |
| Standalone `op signin` | Only where neither of the above applies | Interactive and session-bound; prefer an authorized user terminal. Never copy its session token into the model context. |

The OpenClaw base describes tmux as a way to retain a standalone login, while
warning that desktop app integration may not work from a different tmux IPC
context. That mode is not needed for the configured Service Account.

## Inject only selected fields

A secret reference identifies a vault, item, and field, preferably by stable
IDs: `op://<vault-id>/<item-id>/<field-id>`. Put references in a local file
ignored by Git, for example:

```dotenv
SERVICE_API_KEY="op://<configured-vault-id>/<item-id>/<field-id>"
```

From this package root, `bin/op-self run --env-file /path/to/project/.env.refs
-- <trusted-command>`. `op run` resolves references for its child process and
masks known secret values in output by default. The package's child wrapper
removes `OP_SERVICE_ACCOUNT_TOKEN` before starting the trusted command. The
injected `SERVICE_API_KEY` is still readable by that command and by same-user
processes with process-inspection access. The helper rejects an environment
file that tries to expand `OP_SERVICE_ACCOUNT_TOKEN` into another variable.

Never `source` a reference file to make the shell resolve its values, and do
not add a resolved `.env` to Git or logs. `op inject` can create plaintext
configuration; use it only when the target format requires it and the output
path, permissions, lifetime, and cleanup are defined. Do not use `op read` or
`op item get` as a way to display credentials to the agent. `bin/op-self items`
and `bin/op-self fields` return bounded metadata without values.

Official references: [CLI](https://www.1password.dev/cli),
[runtime environment injection](https://www.1password.dev/cli/secrets-environment-variables),
[Service Accounts](https://www.1password.dev/service-accounts/get-started).
