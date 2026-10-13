# Third-party provenance

GPTgrep's original code is MIT licensed. Dependencies retain their own licenses.
Cargo.lock records the actual resolved graph; the package-local pnpm lock records
the optional incur wrapper graph. Binary archives must carry the bundled dependency
license inventory and PDFium's full native notices, not only this summary.

| Source | Pinned revision/version | Use | License |
| --- | --- | --- | --- |
| [Microsoft tgrep](https://github.com/microsoft/tgrep/tree/1120aca41dd192ae61bdd996e9886cb354a78a27) | v1.1.0 / `1120aca41dd192ae61bdd996e9886cb354a78a27` | Embedded tgrep-core; no upstream server | MIT |
| [LiteParse](https://github.com/run-llama/liteparse/tree/ab8879623ee45a0b7a775d12b42a9a7ae9dab16b) | `ab8879623ee45a0b7a775d12b42a9a7ae9dab16b` | Native parser library, 2.15.1 | Apache-2.0 |
| Vendored `liteparse-pdfium` ([`vendor/liteparse-pdfium`](vendor/liteparse-pdfium/UPSTREAM.json)) | LiteParse `crates/pdfium` 1.11.0 at `ab8879623ee45a0b7a775d12b42a9a7ae9dab16b`, modified | Cargo `[patch]` replacement adding bounded admission helpers; modifications and per-file upstream source hashes in `UPSTREAM.json` | Apache-2.0 (LICENSE and NOTICE retained) |
| [PageIndex](https://github.com/VectifyAI/PageIndex/tree/9c4c3ff2ddd2cbd70501997635578064f2b7c7c3) | `9c4c3ff2ddd2cbd70501997635578064f2b7c7c3` | Studied structural semantics; custom Rust implementation, partial algorithm coverage | MIT |
| [incur](https://github.com/wevm/incur) | published `0.5.1` | Optional gptgrep-ai wrapper | MIT |
| [PDFium native binaries](https://github.com/run-llama/pdfium-binaries/releases/tag/chromium%2F8058) | `chromium/8058` | Runtime PDFium library | Per-component licenses bundled in release |
| [Codex](https://github.com/openai/codex) | Reader runtime selected by caller; native viewer derivative pins `rust-v0.162.1` / `092d3acd6bec3e3a14bdc7e7a2810ab628ab759d` | Local stdio app-server; derivative source patch and notices under `patches/codex-native-viewer`, no Codex binary bundled | Apache-2.0 source; hosted service terms separate |
| [Moby default seccomp profile](https://github.com/moby/moby/blob/daa0cb7f23594cdbcce5002e370027d3fd36ffd7/vendor/github.com/moby/profiles/seccomp/default.json) | Docker Engine `docker-v29.4.0` / `daa0cb7f23594cdbcce5002e370027d3fd36ffd7` (module `github.com/moby/profiles/seccomp` v0.1.0) | Vendored data file and the derived full62 reader seccomp profile; license, derivation and provenance under `workspace/harness-tooling/harbor-ablation/full62/seccomp` | Apache-2.0 |

The PageIndex and parser crate notices document their source mapping and limitations.
The [native viewer derivative](patches/codex-native-viewer/README.md) records its
upstream pin, source patch, notices and build/qualification boundaries.
The full62 reader seccomp profile changes the vendored Moby profile only as its
`provenance.json` records; the module's `LICENSE` is kept beside the vendored file,
and the module ships no `NOTICE` file.
JevGrep, TypeSafe SDK/skills and the example projects were studied for interface and
workflow patterns; GPTgrep implements its own Rust Decisions client. No Jev weights
are distributed. TypeSafe/OpenRouter are hosted providers selected explicitly by
the caller; API credentials and provider service access are not package assets.

The external PageIndex benchmark, jev-eval and open-jev reference checkouts had no
clear repository license declaration in the reviewed snapshots. Their code/data
are not vendored here. GPTgrep's committed eval corpus is project-authored. A local
reference manifest identifies external benchmark inputs without redistributing PDFs.

Research reports separate source/vendor claims from GPTgrep execution evidence.
The initial Rust layout profile does not claim the complete PageIndex Flash algorithm.

The tgrep v1.1.0 pin uses its own `vendor/ignore` package (`0.4.25`), rather than
the crates.io `ignore` used by GPTgrep's corpus discovery. The vendored package
retains Andrew Gallant's MIT and Unlicense texts and declares `Unlicense OR MIT`;
its source identity is the same tgrep Git revision above. Preserve those package
notices in addition to tgrep's repository MIT notice.

The embedded engine's resolved dependency additions retain these licenses:

| Dependency | Locked version | Declared license |
| --- | --- | --- |
| `fs2` | 0.4.3 | MIT OR Apache-2.0 |
| `same-file` | 1.0.6 | Unlicense OR MIT |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 |
| `cap-std`, `cap-tempfile`, `cap-primitives` | 4.0.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `ambient-authority` | 0.0.2 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `fs-set-times` | 0.20.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `io-extras` | 0.19.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `io-lifetimes` | 2.0.4, 3.0.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `maybe-owned` | 0.3.4 | MIT OR Apache-2.0 |
| `rustix-linux-procfs` | 0.1.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `libc` | 0.2.189 | MIT OR Apache-2.0 |
| `windows-sys` | 0.52.0, 0.61.2 | MIT OR Apache-2.0 |
| `winx` | 0.36.4 | Apache-2.0 WITH LLVM-exception |

This table includes conditional dependencies in Cargo.lock; it does not imply
every package is linked on every platform. In particular, the engine enables
`cap-std`/`cap-tempfile`/`libc` on Unix and its `windows-sys` filesystem API on
Windows. The lock also retains conditional dependencies of those packages.
`scripts/collect_licenses.py --target TARGET --output DIRECTORY --require-complete`
collects the actual normal/build graph for each release target, including the
Git-source `ignore` notices. License collection does not establish platform runtime
qualification; native build and relocation checks remain separate release gates.
