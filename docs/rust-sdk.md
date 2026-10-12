# Rust APIs for programmatic tool composition

GPTgrep's Rust crates can be used directly by a Rust program running in the
agent's selected task environment. The caller composes functions and consumes
typed results. The existing native CLI and optional incur wrapper provide other
entrypoints to the same product. JavaScript code can invoke a compiled Rust
program through the task environment's normal process tools; this source does
not define JavaScript imports or a Rust FFI.

The public API example has compiled and run in the development workspace. It
checks deterministic local indexing, composed regex searches and fresh source
citations. A relocated SDK consumer is a separate delivery check: record its
archive digest, selected toolchain, offline compilation and runtime result.
The presence of source files does not establish that a selected task image has
the required toolchain or a staged SDK. Follow [development setup](development.md)
and its storage admission before acquiring dependencies or compiling.

## Public crate entrypoints

| Crate | Selected public entrypoints | Behavior |
| --- | --- | --- |
| `gptgrep-core` | `index`, `search`, `SearchOptions`, `catalog`, `tree`, `read_node`, `read_node_window` | Build an immutable generation, retrieve fresh source windows, and read bounded source text |
| `gptgrep-core::batch` | `parse_batch_request`, `dispatch_batch`, `dispatch_batch_with_reader` | Compose bounded typed text operations against a selected generation |
| `gptgrep-core::rich_batch` | `parse_rich_batch_request`, `open_rich_batch_reader`, `dispatch_rich_batch_with_reader` | Read the rich generation through its existing bounded batch contract |
| `gptgrep-index` | `build`, `search`, `search_with_document` | Use pinned `tgrep-core` filtering with exact regex verification over an immutable text snapshot |
| `gptgrep-host` | `HostConfig`, `NativeTaskRequest`, `run_native_task` | Invoke the caller-selected native Codex runtime under an acknowledged named permission profile |
| `gptgrep-host::parse_agent_driver` | `document_native_execution_bindings` | Derive source-bound Document execution metadata without starting a model |

`SearchOptions::default()` selects model-assisted hybrid search. Explicit
`mode: "regex"` and `mode: "lexical"` expose local inspection primitives; the
default GPTgrep workflow still requires Decisions routing and reranking.
Exact retrieval does not qualify that full workflow. Source freshness and
citation validation remain part of the crate contracts regardless of entrypoint.

The [`rust_ptc_sdk` example](../crates/gptgrep-host/examples/rust_ptc_sdk.rs) is a
separate Cargo example target that links public library APIs. It creates a new
synthetic Markdown corpus under a caller-selected directory, calls `index`,
composes two `search` futures, and checks bounded `read_node` results. It does
not start Codex, read credentials, or contact a provider.

After the source and locked dependencies are available in the selected build
environment, run the example from the source root:

```sh
cargo run --offline --locked -p gptgrep-host --example rust_ptc_sdk -- \
  /absolute/owned/job/new-sdk-example
```

The final directory must not exist and its parent must exist. Cargo output and
temporary directories must already point to admitted job storage. The example's
JSON reports the composed query count and verified citations. It is a local
consumer check, not a retrieval-quality evaluation or native-runtime acceptance.

## Build and package boundary

The source closure consists of the root `Cargo.toml` and exact `Cargo.lock`,
all seven `crates/` packages, the patched `vendor/liteparse-pdfium` package, and
their license notices. Keep the root Cargo patch: downstream use that omits it
can resolve the upstream PDFium wrapper instead of GPTgrep's selected wrapper.
The repository uses edition 2024 and currently selects Rust/Cargo 1.96.0 for CI.
The source SDK must also supply its locked Cargo dependency closure for offline
use; an unverified preexisting package cache is insufficient.

The locked sources include `tgrep-core` at
`1120aca41dd192ae61bdd996e9886cb354a78a27` and LiteParse at
`ab8879623ee45a0b7a775d12b42a9a7ae9dab16b`. Their licenses and the package-local
notices are described in [third-party provenance](../THIRD_PARTY.md). Preserve
actual dependency license files when redistributing a vendored SDK. The existing
release tool packages the source SDK, its full locked Cargo source closure,
PDFium headers/library/notices and a complete dependency license inventory:

```sh
python3 scripts/package_release.py --rust-sdk-only \
  --target aarch64-unknown-linux-gnu \
  --pdfium-archive /absolute/owned/downloads/pdfium-linux-arm64.tgz \
  --output /absolute/owned/sdk-packages
```

Packaging uses offline locked Cargo inputs. It does not install a toolchain or
download the PDFium archive. The PDFium input must match the pinned target hash.
The archive includes `SDK.json`, a complete file hash inventory and
`SHA256SUMS`. Its `consumer_validation` field records that the packager itself
does not execute a consumer; keep the separate consumer receipt with delivery.

The SDK's `source/examples/rust-ptc-consumer/Cargo.toml` is an independent Cargo
workspace that imports `gptgrep-core` and preserves the LiteParse patch. After
extracting the SDK and selecting an admitted writable job directory:

```sh
sdk_dir=/absolute/owned/extracted-sdk
job_dir=/absolute/owned/job
export CARGO_HOME="$job_dir/cargo-home"
export CARGO_TARGET_DIR="$job_dir/target"
export CARGO_NET_OFFLINE=true
export TMPDIR="$job_dir/tmp"
export PDFIUM_LIB_PATH="$sdk_dir/native/pdfium/lib"
export PDFIUM_INCLUDE_PATH="$sdk_dir/native/pdfium/include"
mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR" "$TMPDIR"
cd "$sdk_dir/source"
cargo run --offline --locked \
  --manifest-path examples/rust-ptc-consumer/Cargo.toml -- "$job_dir/new-sdk-example"
```

Running from the source root selects its portable `.cargo/config.toml` and
vendored dependencies. A consumer in another directory must use the SDK's Cargo
source configuration and preserve the same dependency patch and lockfile.

Native document parsing additionally needs the pinned PDFium library and build
headers. Office conversion needs an existing LibreOffice installation. Plaintext
retrieval does not load PDFium at runtime, but compiling the current core/host
dependency graph still includes the parser's native build dependencies. A Rust
compiler and Cargo alone do not establish that build closure. The selected
native build environment also supplies its C/C++ compiler, linker, development
headers and build utilities. The SDK contains product dependencies; general
toolchain provisioning remains the task environment's responsibility.

Both the native binary archive and source SDK carry `runtime/codex`, four
workflow Skills and three role configurations. Their package metadata pins the
runtime manifest and all shipped runtime files. For the native archive the
runtime package root is the extracted bundle; for the SDK it is `source/`.
Stage the selected runtime package with its existing `runtime/codex/stage.sh`
or the typed native runtime package API. Packaging these assets does not launch
Codex or activate a role.

| Execution option | Product payload | Environment prerequisites |
| --- | --- | --- |
| Native CLI or workflow Skills | Native binary bundle, PDFium library, pinned runtime guidance | Matching operating system/architecture; selected Codex runtime for model work; LibreOffice for Office conversion |
| Direct Rust API composition | Source SDK, vendored Cargo dependencies, PDFium headers/library, pinned runtime guidance | Compatible Rust/Cargo and native build toolchain; selected Codex runtime only when calling model-backed APIs |

The optional incur wrapper is a separate CLI frontend. Its JavaScript runtime
requirements apply when that frontend is selected; direct Rust composition uses
the public Rust crates above.

For a paired agent comparison, the general Rust toolchain belongs in the common
environment available to both arms. GPTgrep sources, its embedded tgrep
implementation, dependency payload, and GPTgrep-specific examples belong only
to the selected GPTgrep treatment. Record the exact source and dependency hashes,
toolchain image, consumer compilation and runtime results before claiming the
SDK is usable in that task environment.

## Native task integration

`run_native_task` is the Rust API for a typed native task. Its caller supplies
instructions, JSON state, an output schema, working directory, deadline and
output bound through `NativeTaskRequest`, plus the selected `HostConfig`.
Native task launch remains separate from compiling or running this offline
example. The selected native model, effort and tier remain caller-owned.

The existing `HostConfig::native_runtime_package` selection stages its pinned
agent instructions, workflow Skills and role configuration into the actual task
directory. An optional source SDK must be made available to that same isolated
environment through its package owner. It must report actual source/dependency
availability and a successful consumer run; instructions naming an SDK path do
not establish those facts. Record actual staging and native execution separately
from archive construction and deterministic offline consumer validation.
