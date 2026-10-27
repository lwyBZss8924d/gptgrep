---
name: 1password-self
description: Use for local AI-owned 1Password vault access, project secret-reference injection, or building 1Password CLI, SDK, Connect, Environments, and agent credential workflows. Works with any trusted agent or project; do not use for unrelated authentication.
metadata:
  aicatlog_manifest: aicatlog-manifest.json
  source_owner: 1password-self
---

# 1Password Self

Use the AI-owned vault declared in this package's ignored `.local/config.json`
through its existing read-only Service Account when a task needs its items.
Any trusted local agent or project can invoke the same helper. The user has
preauthorized this vault for AI project work; do not request a fresh Touch ID
approval for ordinary reads. Vault access does not authorize sending a secret
to an unrelated destination or changing 1Password items.

## Route the request

1. For a local command needing a vault field, use `bin/op-self status` to read
   the configured vault name and ID. Use `bin/op-self items` for bounded item
   metadata and `bin/op-self fields --item <id>` for field IDs if needed.
   Put only `op://<vault>/<item>/<field>` references in a project-local ignored
   environment file and run `bin/op-self run --env-file <file> -- <command>`.
   The command gets resolved field values, but not the Service Account token.
2. For local configuration, authentication setup, renewal, or a failed
   Keychain lookup, read [local configuration](references/local-configuration.md).
   Do not reveal a token, read an agent's auth file, copy a token into a shell command, or
   replace an existing Keychain item speculatively.
3. For `op` syntax and interactive fallbacks, read [CLI modes](references/cli.md).
   Use installed `op --help` for exact flags. Ordinary vault reads use the
   Service Account; desktop integration is for operations it cannot perform.
4. For an SDK, Connect server, 1Password Environment, MCP server, hook, or
   website login, read [developer integrations](references/developer-integrations.md).
   Choose the surface by where the secret must go. Browser Agentic Autofill
   currently has its own approval flow and is not replaced by `op-self run`.

## Boundaries

For work with live fields, read and apply [RULES.md](RULES.md). The immediate
boundaries are:

- Keep secret values out of chat, model context, tool arguments, source files,
  Git, logs, notes, and receipts. Do not use `op read`, `op item get`,
  `--no-masking`, `printenv`, or shell tracing to display a secret. Return item
  and field IDs, names, validation status, and artifact evidence instead.
- `read_items` exposes the **whole configured vault** to this Service Account.
  Select the minimum fields needed for each child process. A project command
  can inspect its own injected secret variables; run only a trusted target.
  Other same-user processes may inspect process environments.
- A Service Account cannot read the built-in `Personal` vault; a backup token
  may be stored there. Do not assume that the Environments MCP server can
  read ordinary vault items or that a Service Account can silently approve
  browser autofill.
- Agent identity and 1Password authorization are separate. This package is
  optional market source and does not install itself into a project, repo,
  agent profile, or other client. Consumers share the account's 1Password audit
  identity unless they provision separate accounts explicitly.

Source provenance and validation live in [AGENTS.md](AGENTS.md). Official
behavior can change; recheck the linked 1Password documentation when changing
an integration.
