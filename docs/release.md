# Native binary releases

The release package contains the native Rust CLI and the exact PDFium library needed for local PDF parsing. Local regex/lexical retrieval needs no Node, Bun, Python, vector database or MCP service. The optional `gptgrep-ai` incur wrapper is a separate source package. Office conversion needs an existing LibreOffice installation. Agentic parsing/OCR and reasoning additionally require their selected Codex runtime, independent authentication, qualified controls and generic tools; see [development setup](development.md). The package does not bundle those private runtime inputs.

The initial package targets are `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`. Development has used a macOS ARM64 host and, since 2026-10-12, a macOS Intel (x86_64) host. macOS Intel is not a release target, so local qualification there is development evidence, not a release artifact. A configured Linux job is not evidence of a successful Linux run; platform validation is established by that target's completed native CI and archive smoke receipt. macOS binaries are unsigned and not notarized. Linux compatibility is with the runner/build environment used for that release, not a promise of arbitrary older glibc support. The packaging source additionally supports `aarch64-unknown-linux-gnu` for the experimental Harbor workflow. Its current-source Linux build, complete license bundle and relocation/native smoke must be verified separately; support in the script is not an accepted release artifact. Windows, macOS Intel and musl remain outside this release set.

## Package an existing native build

Use the owned output paths and uv-managed Python from [development setup](development.md). Python 3.12 or later, Cargo and a native build environment are packaging tools. Pass every binary input explicitly. The script does not search a developer home directory or download native dependencies.

```sh
cargo build --release --locked -p gptgrep-cli
python3 scripts/package_release.py \
  --binary "$CARGO_TARGET_DIR/release/gptgrep" \
  --pdfium-dir /path/to/unmodified/extracted/pdfium \
  --target aarch64-apple-darwin \
  --output "$GPTGREP_DEV_ROOT/release" --require-clean --smoke-test
```

Use the corresponding native Linux target on Linux. `--require-clean` requires an actual committed source revision and a clean tree. Omit it only for local preparation; the metadata then records the actual dirty/uncommitted status and cannot pass publication checks. If Git is unavailable, local preparation records `source_revision: null` and `source_dirty: true`; `--require-clean` rejects that state. The script checks the native binary's version and architecture, rejects existing output filenames, and publishes its local files only after the requested checks pass.

PDFium is pinned to [`chromium/8058`](https://github.com/run-llama/pdfium-binaries/releases/tag/chromium/8058), whose release-source commit is [`f15f55200f695a13c2ca3aab35f8aa40a596b2f7`](https://github.com/run-llama/pdfium-binaries/tree/f15f55200f695a13c2ca3aab35f8aa40a596b2f7). The package script validates the exact library plus `LICENSE`, `VERSION` and every file in `licenses/` against a recorded payload digest. A modified or incomplete payload fails. In particular, a build helper may rewrite a cached macOS dylib's install name; use the original extracted release artifact for packaging. Release workflows separately verify the downloaded archive before extracting it.

| Target | Upstream asset | SHA-256 |
|---|---|---|
| macOS ARM64 | `pdfium-mac-arm64.tgz` | `df8d1016fc36c2651b368ca13b992ae3bb6b6020d12f2d24443bf13cf76ea593` |
| Linux x86_64 | `pdfium-linux-x64.tgz` | `2dd418a8974dbb373ad281f99b6f95ed02e01b1a2bb2e05e772c80b7587577b5` |
| Linux ARM64 experimental | `pdfium-linux-arm64.tgz` | `4915b5351640c1eea59e26023e0edcad8433df96dc8f4cb3d645a4d64a178b7c` |

The `.tar.gz` has one top-level directory:

```text
gptgrep-VERSION-TARGET/
  gptgrep                 shell launcher; preserves arguments and exit status
  gptgrep.bin             native executable
  lib/libpdfium.*         native PDFium library
  LICENSE
  THIRD_PARTY.md
  licenses/               retained project notices and source supplements
  third-party/pdfium/     original LICENSE, VERSION and complete licenses/
  third-party/rust/       actual Rust dependency license/notice files and inventory
  RELEASE.json            source revision, target, version and payload digests
  SHA256SUMS              checksums of all bundle files except this checksum file
```

The launcher resolves its own location, including a bounded symlink chain, and sets `PDFIUM_LIB_PATH` to the bundle's absolute `lib` directory for the child process. Paths with spaces and original CLI arguments are preserved. Move the whole bundle; moving only `gptgrep.bin` removes the launcher guarantee. `GPTGREP_TRACE_LIBRARIES=1` enables loader diagnostics for relocation verification. It is not necessary for normal use.

Each archive also has `.tar.gz.sha256`, `.release.json`, and, when requested, `.smoke.json` sidecars. `--smoke-test` extracts the real archive into a separate path containing spaces, exercises original synthetic PDF and plaintext documents, verifies fresh citations, and checks that only PDF parsing loads the bundled PDFium library. The smoke process makes no model calls. Shared caches are never renamed or deleted. Archive file ownership and timestamps are normalized; this does not by itself establish byte-identical Rust builds across toolchains.

## Actual dependency license files

`scripts/collect_licenses.py` reads `cargo metadata --locked --format-version 1 --filter-platform TARGET` and follows normal/build dependency edges from `gptgrep-cli`. Development-only edges are excluded. It copies actual package/repository `LICENSE`, `COPYING`, `COPYRIGHT` and `NOTICE` files and license directories. SPDX expressions remain declarations in the inventory; they are not substituted for license texts. Packaging fails if an actual license payload is unavailable.

```sh
python3 scripts/collect_licenses.py --target aarch64-apple-darwin \
  --output "$GPTGREP_DEV_ROOT/license-audit" --require-complete
```

Some published crates omit their upstream license files. The version-bound files under `licenses/rust-supplements/` retain exact source text, SHA-256, Git blob identity, the package's `.cargo_vcs_info.json` revision and primary source URL. They are admitted only for the matching installed package revision. For `simd_helpers` 0.1.0, the original package commit omitted `LICENSE`; the author's next commit only adds that file, and the compiled source blob is unchanged. Both revisions and the one-file comparison are recorded explicitly. Zune's full Zlib text is retained alongside its upstream alternative-license notice; the notice is not presented as the full MIT or Apache text.

No local dependency-cache paths, private session identities or credentials are written into release metadata or license inventories.

An authorized experimental source branch is a collaborator handoff, not a binary
release or a successful benchmark. Its current development limits are recorded in
[experimental status](experimental-status.md).

The Cargo workspace and the optional wrapper are version `0.2.0-alpha.1`
(experimental). Experimental builds keep `0.x.y-alpha.N` versions. The version
moves to a canary `x.y.z` only after the paired same-Codex benchmark criterion in
[SPEC](../SPEC.md) passes. A version change alone creates no tag or release. Tags
need the applicable project authorization and independent acceptance.

## CI and publication

Release/canary acceptance follows the predeclared matched native Codex comparison
on XL-DocBench in [SPEC](../SPEC.md), [evaluation](evaluation.md) and the
[goal contract](../workspace/harness-config/goal-contract.json). Freeze the exact
cohort, source/runtime identities and success criterion before results; preserve
failures, unknown outcomes, citation checks and separate preparation/SEARCH/Judge
accounting. A small live pilot, offline check or source delivery does not establish
full-benchmark acceptance. The self-opt report cannot grant release acceptance.
The former PageIndex/full62 gate is retired.

CI runs Rust formatting, workspace tests, Clippy with denied warnings, packaging
self-tests, runtime staging and complete license collection on both native targets.
Separate jobs check the Bun benchmark/self-opt contracts, the uv-locked Harbor
verifier and the locked optional incur wrapper. The PDFium archive is checksum-verified before use. Native platform
acceptance requires that target's completed CI and archive smoke receipt.

The release workflow runs on an existing `vVERSION` tag or a manual dispatch naming that tag. It binds the tag to the Cargo workspace version and exact source revision, runs CI against that revision, builds each native artifact, collects licenses, packages and runs relocation tests, then publishes only after both targets succeed. Only the final publish job has `contents: write`. Checkout credentials are not persisted. The workflow does not publish to crates.io or npm, create a source tag, replace existing release assets, sign binaries or notarize macOS executables. Prerelease version tags produce GitHub prereleases.

Independent source review and project closeout precede the tag push. Preparing these workflow files or passing local checks does not establish that a hosted workflow or public release has run. Triggering the release workflow is a publication action; the project owner performs it after checking the concrete source and retained results.

Existing action references were resolved from the primary GitHub tag APIs and pinned to actual commit objects on September 22, 2026. The Bun and uv actions were additionally resolved on October 16, 2026. The annotated pnpm tag was dereferenced to its commit. The Rust action is pinned to the observed `stable` action commit while the compiler itself is explicitly 1.96.0.

| Action | Verified ref | Commit |
|---|---|---|
| [checkout](https://github.com/actions/checkout/releases/tag/v7.0.1) | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| [setup-python](https://github.com/actions/setup-python/releases/tag/v7.0.0) | v7.0.0 | `5fda3b95a4ea91299a34e894583c3862153e4b97` |
| [setup-node](https://github.com/actions/setup-node/releases/tag/v7.0.0) | v7.0.0 | `820762786026740c76f36085b0efc47a31fe5020` |
| [upload-artifact](https://github.com/actions/upload-artifact/releases/tag/v7.0.1) | v7.0.1 | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| [download-artifact](https://github.com/actions/download-artifact/releases/tag/v8.0.1) | v8.0.1 | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` |
| [pnpm/action-setup](https://github.com/pnpm/action-setup/releases/tag/v6.1.0) | v6.1.0 | `ea17c68df8912ef543352723c149a84f56e3d413` |
| [rust-toolchain](https://github.com/dtolnay/rust-toolchain/tree/6bed0761d98439e5a578e2877258200ad565ba87) | stable action revision | `6bed0761d98439e5a578e2877258200ad565ba87` |
| [setup-bun](https://github.com/oven-sh/setup-bun/tree/0c5077e51419868618aeaa5fe8019c62421857d6) | v2 action revision; Bun 1.3.14 | `0c5077e51419868618aeaa5fe8019c62421857d6` |
| [setup-uv](https://github.com/astral-sh/setup-uv/tree/37802adc94f370d6bfd71619e3f0bf239e1f3b78) | v7 action revision; uv 0.13.1 | `37802adc94f370d6bfd71619e3f0bf239e1f3b78` |
