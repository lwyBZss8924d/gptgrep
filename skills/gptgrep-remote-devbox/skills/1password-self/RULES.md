# Local 1Password security rules

These rules apply when a consumer chooses this optional package and acts on
the locally configured AI-owned vault. They do not install themselves into
another agent's global instructions.

1. Read `.local/config.json` through `bin/op-self status`; use the reported
   vault name and UUID. Treat that configuration as host-local metadata, not
   as permission to access any other vault.
2. Use the existing Keychain-backed Service Account for authorized read-only
   work in the configured vault. Its grant covers all items in that vault;
   select the smallest set of fields for each project process.
3. Keep the Service Account token in Keychain and its backup outside every
   vault it can read. Never put a token or resolved item value in a prompt,
   tool argument, shell history, source file, tracked config, log, test
   fixture, receipt, Git note, or agent memory.
4. Resolve fields at process launch with `bin/op-self run` and a local
   reference file. Do not print environment variables or disable `op run`
   masking. The helper strips its bootstrap token from the target process;
   it cannot hide selected field values from that process or from all
   same-user process inspection.
5. Treat vault metadata lookup, field injection, SDK access, Connect Server,
   Environments MCP, and browser Agentic Autofill as distinct capabilities.
   The MCP server cannot return Environment secrets or access ordinary vault
   items, and browser Autofill has its own approval flow.
6. Account creation, permission changes, item writes or deletion, sharing,
   server deployment, and client configuration changes require task-specific
   authorization and a reviewable target. Do not infer these effects from
   authorization for a read-only secret lookup.
7. Verify the downstream project's actual operation. Successful 1Password
   authentication proves neither the API key's downstream scope nor the
   project's result. Stop on ambiguous mutating outcomes and retain exact
   cleanup evidence.

Sources: [1Password Service Accounts](https://www.1password.dev/service-accounts/get-started),
[runtime injection](https://www.1password.dev/cli/secrets-environment-variables),
[Security for AI](https://www.1password.dev/security-for-ai).
