# Choose the 1Password developer integration

Checked against the official pages on 2026-09-26. Recheck product support
before changing an integration.

| Need | Surface | Decision for this workstation |
| --- | --- | --- |
| Give a trusted local CLI one or more item fields | `op run` with field references | Use `bin/op-self run` and an ignored reference file. It reads the locally configured AI vault without per-run Touch ID. |
| A long-running trusted app needs dynamic item lookup | 1Password SDK | Use the official Go, JavaScript, or Python SDK with a service account scoped to that app's vaults and permissions. An SDK process must receive its token; do not pass a shared AI-vault token to untrusted or model-generated code. |
| An infrastructure service needs a private REST API and local cache | 1Password Connect Server | This is a separately deployed server with its own access token and cached data. It is not the existing Service Account or the local Environments MCP server. Design its host, vault grant, storage, and network boundary before deployment. |
| Manage named 1Password Environments from an MCP client | Environments MCP server (`1password-mcp`) | It lists variable names and manages Environments but does not return values or read ordinary vault items. It uses desktop approvals until 1Password locks. Configure it only if an Environment workflow is selected. |
| Validate locally mounted Environments `.env` files | 1Password agent hook | The published hook currently supports Claude Code, Cursor, GitHub Copilot, and Windsurf. Do not describe it as an installed gate for an unsupported client. |
| Sign in to a website without exposing password to the agent | Agentic Autofill | Current early access targets Browserbase Director and requests human approval for each autofill. `op-self run` is CLI credential injection, not browser Autofill. |

## Project integration contract

1. Name the target command or app, the exact item fields, and the required
   external destination. Prefer a project-specific vault and Service Account
   when code needs broad SDK access. The shared AI-vault account is read-only
   across the whole AI vault; it cannot isolate one project inside that vault.
2. Keep only field references in source or local ignored files. Launch a
   bounded trusted process with `bin/op-self run`; its child gets selected
   secrets, never the Service Account token. Do not log its environment or
   replace masking with `--no-masking`.
3. Verify a real service operation and its cleanup. A successful `op` lookup
   proves 1Password access, not downstream authorization or project success.
4. For SDK or Connect, use the official SDK/Connect docs, pin versions, and
   verify exact token scope before writing code. Do not launch a Connect
   server or change an agent's MCP configuration just because a project needs one
   API key.

Official pages: [Security for AI](https://www.1password.dev/security-for-ai),
[SDKs](https://www.1password.dev/sdks),
[Connect](https://www.1password.dev/connect),
[Environments MCP](https://www.1password.dev/environments/mcp-server),
[agent hook](https://www.1password.dev/environments/agent-hook-validate),
[Agentic Autofill](https://www.1password.dev/agentic-autofill).
