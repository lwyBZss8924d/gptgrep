# Local configuration and the AI-owned vault

The vault name and UUID are host-local metadata in the ignored mode-`0600`
`.local/config.json`. `config.example.json` describes the format without
embedding this workstation's values. `bin/op-self status` reports the active
name and UUID after validating that the Keychain-backed Service Account can
read that exact vault. The account can read every item in the configured
vault; there is no per-item permission within that grant.

The package is an optional market source. It does not install itself into a
project, repository, or agent's Skill directory. A
trusted consumer invokes `bin/op-self` by path when its task needs a field.
The same Service Account then has one shared 1Password audit identity across
all consumers. Separate audit identities require separate Service Accounts.

The token is held in the macOS login Keychain under the `keychain.service`
and `keychain.account` specified in the local config. The helper's
`adopt-legacy` command can migrate a previously stored token from the
optional `legacy_keychain` identity, after verifying access to the exact
configured vault. It never prints the token and refuses to replace an
existing destination item. Remove `legacy_keychain` from the local config
after adoption; the old Keychain item remains for its previous consumer.

From the package root, or with its absolute path from elsewhere:

```bash
bin/op-self status
bin/op-self items --limit 20 --offset 0
bin/op-self fields --item <item-id>
bin/op-self run --env-file /absolute/project/.env.refs -- python3 app.py
```

`items` returns only IDs, titles, and categories with bounded pagination.
`fields` retrieves one item inside the short-lived helper process but emits
only field IDs, labels, and types; it never emits field values.
`run` starts a trusted process with resolved field values and removes the
Service Account token from that process's environment. It does not prove the
injected API key has permission at its downstream service; verify the actual
application outcome separately. The operation is unattended **after the
macOS login Keychain is available**. Operation before user login after a
reboot has not been verified.

Before a Service Account expires, create a replacement with the intended
vault grant, save a backup outside its accessible vaults, and perform an
explicit token rotation. Do not replace a working Keychain item with
unverified bytes. Official account behavior:
[Service Accounts](https://www.1password.dev/service-accounts/get-started).
