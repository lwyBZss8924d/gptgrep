# Guarded native image viewer

This experimental Codex source derivative supplies literal native `view_image`
for GPTgrep's page OCR and document parse agent. It applies to upstream
[`openai/codex`](https://github.com/openai/codex) commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`.
This source is based on release `rust-v0.162.1`; the derived build version is
`0.162.1-gptgrep.1`. Models-manager sends the upstream whole client version
`0.162.1`. `source-manifest.json` records before/after source and patch identities,
plus an observed build identity. That binary digest is provenance for that build,
not a promise that another platform/toolchain rebuild has identical bytes or
inherits a later live qualification. Preserve the accompanying Apache 2.0
`LICENSE` and upstream `NOTICE`.

This is the repository's selected experimental derivative, not a claim about the
latest upstream release. The standard official `codex` CLI still performs device
login and supplies the separately selected SEARCH reader. See
[development setup](../../docs/development.md) for those roles and the independent
account, state and Python/Pillow/Poppler environment.

Use a separate admitted source checkout, its selected toolchain and external
build/cache/temp paths from the project's development-storage workflow. Clone a
separate checkout rather than modifying an upstream reference checkout. Define
`GPTGREP_SOURCE` as this repository and `GPTGREP_CODEX_SOURCE` as the new checkout:

```sh
git clone https://github.com/openai/codex.git "$GPTGREP_CODEX_SOURCE"
git -C "$GPTGREP_CODEX_SOURCE" checkout --detach 092d3acd6bec3e3a14bdc7e7a2810ab628ab759d
git -C "$GPTGREP_CODEX_SOURCE" apply --check "$GPTGREP_SOURCE/patches/codex-native-viewer/codex-native-guard-v0.162.1.patch"
git -C "$GPTGREP_CODEX_SOURCE" apply "$GPTGREP_SOURCE/patches/codex-native-viewer/codex-native-guard-v0.162.1.patch"
cd "$GPTGREP_CODEX_SOURCE/codex-rs"
# Use this checkout's rust-toolchain.toml and its build prerequisites.
cargo build --locked --profile dev-small --package codex-app-server --bin codex-app-server --jobs 2
```

Keep this build's `CARGO_TARGET_DIR`, caches and temporary files separate from
the GPTgrep build. The artifact is `codex-app-server` under the selected target's
`dev-small` directory. Hash the actual artifact and verify its `--version` output
before binding controls. The source manifest does not install or qualify it.

The standalone binary accepts `--listen stdio:// --strict-config`. GPTgrep
supplies a private, source-bound admission and the selected named filesystem
profile. The Page guard enforces eight total tools, one turn and 180 seconds;
the Document guard defaults to 64 total tools, one turn and 900 seconds. An
explicit native Document `max_seconds` from 1 through 3,600 changes the bound
guard contract and requires matching caller controls and qualification. It does
not change Page's 180-second limit. The effective run deadline remains the
smallest frontend/session, document and original-phase deadline. Both guards
validate the final requested `gpt-6-luna`/`max`/`priority` profile before sending,
and keep native read, prepared image and assembled image receipts separate.
Fast is normalized to priority locally. A local model catalog preserves the
operator's requested tier; it is not evidence of the model or tier served.

Build success and guard unit tests do not qualify native image delivery. Bind
the exact binary, guard source, protocol schema, model descriptor and policy,
then independently accept an actual native delivery and paired image-influence
qualification before ordinary use. Page and Document qualifications are
separate. `parse-agent qualify` produces evidence for that review, not an accepted
record by itself. The public tree intentionally omits private admissions,
account routing, reader evidence and runtime identities; operators must prepare
their own controls from the published types and verify their exact runtime.

Scoped Page and Document image components have live evidence in the maintained
development environment. Complete-corpus OCR/indexing and the full62 paired
comparison remain open; [experimental status](../../docs/experimental-status.md)
records the distinction and the latest retained failure.

The GPTgrep entry points are `parse-agent serve` for an external PTC client,
`parse-agent run` for complete deterministic parsing, `parse-agent agent` for
one admitted document turn, and `parse-agent qualify` for private evidence.
`gptgrep --schema` gives the closed control, product and index-registry contracts.
Complete parser/native metadata coverage does not imply complete model inspection.
