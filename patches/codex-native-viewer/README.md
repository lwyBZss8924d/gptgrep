# Guarded native image viewer

This experimental Codex source derivative supplies literal native `view_image`
for GPTgrep's page OCR and document parse agent. It applies to upstream
[`openai/codex`](https://github.com/openai/codex) commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`.
This source is based on release `rust-v0.162.1`; the derived build version is
`0.162.1-gptgrep.1`. Models-manager sends the upstream whole client version
`0.162.1`. `source-manifest.json` records every before/after file identity. Preserve the
accompanying Apache 2.0 `LICENSE` and upstream `NOTICE`.

Use a separate admitted source checkout, its selected toolchain and external
build/cache/temp paths from the project's development-storage workflow. After
checking out the exact upstream revision, run from that source checkout:

```sh
git apply --check "$GPTGREP_SOURCE/patches/codex-native-viewer/codex-native-guard-v0.162.1.patch"
git apply "$GPTGREP_SOURCE/patches/codex-native-viewer/codex-native-guard-v0.162.1.patch"
cargo build --manifest-path codex-rs/Cargo.toml --locked --profile dev-small --package codex-app-server --bin codex-app-server --jobs 2
```

The standalone binary accepts `--listen stdio:// --strict-config`. GPTgrep
supplies a private, source-bound admission and a named image-only filesystem
profile. The Page guard enforces eight total tools, one turn and 180 seconds;
the Document guard enforces 64 total tools, one turn and 900 seconds. Both
validate the final requested `gpt-6-luna`/`max`/`priority` profile before sending,
and keep native read, prepared image and assembled image receipts separate.
Fast is normalized to priority locally. A local model catalog preserves the
operator's requested tier; it is not evidence of the model or tier served.

Build success and guard unit tests do not qualify native image delivery. Bind
the exact binary, guard source, protocol schema, model descriptor and policy,
then independently accept an actual native delivery and paired image-influence
qualification before ordinary use. Page and Document qualifications are
separate. Private admissions, account routing, reader evidence and runtime
identities belong in ignored local state.

The GPTgrep entry points are `parse-agent serve` for an external PTC client,
`parse-agent run` for complete deterministic parsing, `parse-agent agent` for
one admitted document turn, and `parse-agent qualify` for private evidence.
`gptgrep --schema` gives the closed control, product and index-registry contracts.
Complete parser/native metadata coverage does not imply complete model inspection.
