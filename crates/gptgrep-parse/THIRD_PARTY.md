# Source provenance and redistribution

Direct dependency:

- Source: [run-llama/liteparse](https://github.com/run-llama/liteparse/tree/ab8879623ee45a0b7a775d12b42a9a7ae9dab16b).
- Git revision: `ab8879623ee45a0b7a775d12b42a9a7ae9dab16b`.
- Core version: `2.15.1`; `liteparse-pdfium` / `liteparse-pdfium-sys`: `1.11.0`.
- License: Apache-2.0; full upstream text retained in
  [licenses/LiteParse-APACHE-2.0.txt](licenses/LiteParse-APACHE-2.0.txt).
- Build profile: `default-features = false`; no Tesseract or ONNX OCR feature.
- No changes to the upstream source were made for this adapter.

The adapter relies on the public APIs in `crates/liteparse/src/lib.rs`,
`parser.rs`, `config.rs`, `types.rs` and `layout.rs`. These exact source contracts
were inspected; older installed `lit` versions are not treated as the same API.
Cargo.lock retains Rust dependency resolution. A crates.io publication needs a
verified registry dependency/version or a different packaging plan; the current
git pin is intended for reproducible workspace and binary builds.

PDFium's native artifact is pinned by upstream build.rs to `chromium/8058` from
run-llama/pdfium-binaries. PDFium/Chromium and bundled third-party components have
their own notices. Release packaging must preserve those artifacts' notice files
alongside the library. The Apache license for the Rust wrapper alone does not
replace native dependency notices. LibreOffice is an external optional runtime
dependency and is not vendored here.

The inspected macOS ARM64 artifact is cached under
`~/Library/Caches/pdfium-rs/chromium_8058/pdfium-mac-arm64/`. The runtime payload
is `lib/libpdfium.dylib`; retain `VERSION`, the root `LICENSE`, and the **entire**
`licenses/` directory with it. That directory currently contains fourteen notices:
`abseil.txt`, `agg23.txt`, `fast_float.txt`, `freetype.txt`, `icu.txt`, `lcms.txt`,
`libjpeg_turbo.ijg`, `libjpeg_turbo.md`, `libopenjpeg.txt`, `libpng.txt`,
`llvm-libc.txt`, `pdfium.txt`, `simdutf.txt`, and `zlib.txt`. The root LICENSE is
the binary-packaging project's MIT notice and explicitly refers to this
directory; it is not the entire native license payload. Resolve the corresponding
artifact and notices for each release platform rather than copying macOS bytes
into a different platform package.

The synthetic PDF fixture generator in the Rust tests is original GPTgrep code;
no upstream benchmark PDF or dataset has been copied into this crate.
