# DevBox selfcheck

The canonical implementation is `src/selfcheck.ts`, run by Bun without external
dependencies. The three repository entrypoints are thin launchers:

- `bin/gptgrep-devbox-selfcheck`
- `workspace/harness-tooling/gptgrep-devbox-selfcheck/bin/gptgrep-devbox-selfcheck`
- `skills/gptgrep-remote-devbox/bin/gptgrep-devbox-selfcheck`

They must be delivered with the canonical owner directory. `install.sh` copies
the exact source closure into an explicit payload destination; it creates no
global links and runs no probes. An authorized deployment may link its public
command to that payload's `bin/gptgrep-devbox-selfcheck`; the canonical launcher
resolves symlink chains before locating `src/selfcheck.ts`. A copied standalone
repository launcher has no supported installed layout. Existing installed Skills
are separate consumers and are not updated by a source edit. The older Python
copies and their always-ready assumptions are superseded; legacy selfcheck E2E
calls now use the native launcher and expect unknown v2 evidence without config.

```sh
bin/gptgrep-devbox-selfcheck --config ./private-observation-spec.json --mode local --json
```

The original `--mode`, `--json`, `--strict`, `--output-file`, `--verbose`/`-v`
and `--help`/`-h` options remain; value options also accept `--option=value`.
`--config` is required for observations. Default
mode is `local`; `remote` means the current process is already on the configured
Linux target. Neither mode performs SSH, wakes a machine, starts a service,
runs a model, invokes version probes or reads client authentication/config files.
Only a bounded read-only Git identity command runs during default inventory.

Use a private, non-secret JSON observation specification with the closed
`gptgrep.devbox-selfcheck.config.v1` schema. Relative paths resolve against its
directory. It contains `project_root`, exact `expected_host` (hostname, platform,
arch), `storage` (physical path, device, inode, reserve_bytes), explicitly pinned
`tools` (name, path, sha256), and `links` (name, path, target). Empty tool lists,
unknown fields and missing requirements are rejected. This is not a place for
tokens, provider arguments, authentication files or private client config.
`config.example.json` is an intentionally unfilled template, not an admission.

Configuration and proof metadata use one bounded physical regular-file snapshot
for both parsing and hashing. Descriptor/path identity and change timestamps are
checked before and after reading; replacement or in-place changes reject the
snapshot. Path containment and credential-name protections apply at acquisition.

The v2 receipt binds the actual canonical implementation digest, Bun executable
digest/version, configuration digest, Git revision, current hostname/platform/
architecture and requested mode. Its separate implementation digest covers
uncommitted selfcheck changes. Filesystem identity, free bytes, executable bytes
and actual symlink targets are observations, not volume/worker admission,
Docker health, provider readiness, corpus validity or autoload adoption. Missing
or unverified facts are `UNKNOWN`; observed contradictions are `FAILED`.
Neither is silently converted to PASS by `--strict` or a historical receipt.

Readiness uses five live probe requirements: `storage.admission`,
`runtime.native_tools`, `corpus.admission`, `auth.isolation`, and
`lifecycle.protection`. Default inventory leaves them UNKNOWN and exits 1 even
when its available observations pass. Invalid CLI arguments/output failure exit
2. `--output-file` exclusively creates a new mode-0600 receipt; it never
overwrites an earlier result. A PASSED qualification still grants no workload
admission: the selected owner must admit every exact workload independently.

## Explicit local qualification interface

E4 can implement an independently reviewed owner producer for these live probes.
E3 supplies the transport/binding validator; it does not implement or claim the
actual remote/storage/auth/corpus/lifecycle acceptance. Do not enroll a producer
that echoes requested statuses, reads static PASS fields, or runs fixture logic.

Only explicit `--qualify` executes a producer. Its optional config block is:

```json
{
  "qualifier": {
    "executable": {"path": "./runtime", "sha256": "<exact executable sha256>"},
    "source": {"path": "./reviewed-local-producer", "sha256": "<exact source sha256>"},
    "evidence_root": "./admitted-run-evidence",
    "timeout_ms": 30000,
    "max_age_ms": 60000
  }
}
```

The executable and producer source are both hashed before execution. When they
are the same file it is invoked with `--selfcheck-qualification-v1`; otherwise
the invocation is `executable source --selfcheck-qualification-v1`. There are
no arbitrary shell command or remote options. The operator must separately
authorize/admit that producer and its possible effects. A reviewed producer may
use the existing storage/auth/runtime front doors; this interface does not bypass
their policies. Source delivery alone does not enroll a production producer.

Stdin is a `gptgrep.devbox-selfcheck.qualification-request.v1` JSON object with a
fresh `request_id`, full `binding`, `binding_sha256`, `producer_sha256`, the five
`required_probes`, and physical `evidence_root`. The producer receives the
existing process environment, so it must preserve the selected broker and
secret boundaries and never echo credentials. Source pins cover only the selected
entry source/executable: the independent producer review must bind any additional
modules, subprocesses, policies and fixtures it uses in the owner evidence.

Stdout must be `gptgrep.devbox-selfcheck.qualification-result.v1` with the same
request/binding and exactly five ordered `proofs` (`path`, `sha256`). Output is
bounded to 256 KiB; timeout and malformed/failed output stay UNKNOWN without
printing raw stderr. Each proof is a bounded JSON object:

```text
schema = gptgrep.devbox-selfcheck.probe.v1
scope = live_current_host
probe = one required probe ID, in request order
request_id, binding_sha256, producer_sha256 = exact request fields
run_id = actual producer/owner run identity
started_at, completed_at = within this producer invocation
expires_at = still fresh, within configured max_age_ms
status = PASSED | FAILED | UNKNOWN
command_sha256 = actual owner probe command digest
evidence = {path, sha256} of bounded non-secret owner receipt metadata
```

Proofs and evidence must resolve inside the admitted evidence root, without
symlink escape; both hashes are verified. Fixture/static scope, replayed nonce,
different source/config/host/mode bindings, stale timestamps, missing evidence
or malformed proof cannot qualify a gate. The producer's separately reviewed
implementation is responsible for real probe semantics, expected identities,
current owner receipt validation and interpreting failures. This transport
validator cannot make an unreviewed producer trustworthy, and a command digest
alone does not prove a probe's substantive correctness.

## Focused tests

Run only inside a distinct reviewed owner storage run, with its startup and
terminal receipts. Test temporary files use the admitted TMPDIR; no dependency
installation, network, provider, SSH, Docker or shared Git writes are needed.

```sh
bun test workspace/harness-tooling/gptgrep-devbox-selfcheck/tests/selfcheck.test.ts
```

Fixtures exercise protocol rejection and state semantics. They do not certify
actual storage, authentication, native tools, corpus or VM lifecycle behavior.
