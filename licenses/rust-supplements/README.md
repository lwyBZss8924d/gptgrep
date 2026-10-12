# Version-bound source license supplements

These files fill missing license payloads in published crate archives. They are
original upstream bytes, not regenerated text from an SPDX identifier. Each
package directory contains `provenance.json` with its version, packaged Git
revision, actual license revision, primary URL, Git blob ID and SHA-256.
`scripts/collect_licenses.py` verifies that the installed crate's
`.cargo_vcs_info.json` revision and the retained license digests match before
including a supplement in a release inventory.

For archives without VCS metadata, the supplement instead binds the exact
`Cargo.lock` archive checksum, repository identity, original manifest and every
packaged source file to a pinned upstream Git tree. Release-generated payloads
are listed separately with SHA-256 values; they are not described as matching
tracked upstream files. No package revision is invented when the archive does
not supply one.

| Published package | Packaged revision | Retained license source |
|---|---|---|
| profiling 1.0.18 | `8271551172eb6fa4cba47369aedd93790c623df9` | Same revision, Apache-2.0 and MIT texts |
| profiling-procmacros 1.0.18 | `8271551172eb6fa4cba47369aedd93790c623df9` | Same revision, Apache-2.0 and MIT texts |
| pulp-wasm-simd-flag 0.1.1 | `5eb07fd7b68edf0a5e19f71737d315f72a510295` | Same revision, upstream LICENSE |
| simd_helpers 0.1.0 | `ca1a2f84aa386d758e98f8a609d990263932fb85` | Next upstream commit `82040194cd05affb060bf94d6f19f82a771d07fb`, only adds LICENSE; compiled source unchanged |
| uuid-simd 0.8.0 | `d74c030d9dc4f3cae02146d1f497ff62726ef09a` | Same revision, upstream MIT LICENSE |
| vsimd 0.8.0 | `d74c030d9dc4f3cae02146d1f497ff62726ef09a` | Same revision, upstream MIT LICENSE |
| zune-core 0.4.12 | `f8fbb123d5ed04441e8324a555bfcda0cb1bd28f` | Same revision, full Zlib text and upstream alternative-license notice |
| zune-inflate 0.2.54 | `69502ce83fdfecdd0beefd677e2abb3781b29d98` | Same revision, full Zlib text and upstream alternative-license notice |
| zune-jpeg 0.4.21 | `fa2c767a01d7d9373911d0bf63e0588553d67e0e` | Same revision, full Zlib text and upstream alternative-license notice |
| jni 0.22.4 | `5ae9458a4ec44c5318f37ddc7569c1d4ae8a69e7` | Same revision, Apache-2.0 and MIT texts |
| jni-macros 0.22.4 | `33045a124105c939d1e2cbdcb5a39e5d868ffa03` | Same revision, Apache-2.0 and MIT texts |
| jni-sys-macros 0.4.1 | `64d77b7a5f119d7b55b4e2c169a4668067ff59e6` | Same revision, Apache-2.0 and MIT texts |
| r-efi 5.3.0 | `97b55bed1c2c91dcbf787674849f05337ff80b33` | Same revision, AUTHORS contains full MIT grant and copyright notices |
| r-efi 6.0.0 | `7e1b0322d31d625f81a5656096330934f9cd835d` | Same revision, AUTHORS contains full MIT grant and copyright notices |
| rustls-platform-verifier-android 0.1.1 | Archive omits VCS metadata | Manifest and Rust source match `996b1c903491641b17b3c9afb65d1352f6fc6b76`; five generated Maven files separately hashed; same revision's Apache-2.0 and MIT texts |
| winapi-i686-pc-windows-gnu 0.4.0 | Archive omits VCS metadata | All 1,390 packaged source files match `9497609ef44cc9bcd16cd2411c0ee6ccaf5483aa`; same revision's Apache-2.0 and MIT texts |
| winapi-x86_64-pc-windows-gnu 0.4.0 | Archive omits VCS metadata | All 1,419 packaged source files match `9497609ef44cc9bcd16cd2411c0ee6ccaf5483aa`; same revision's Apache-2.0 and MIT texts |

The simd_helpers exception is supported by the author's
[one-file comparison](https://github.com/lu-zero/simd_helpers/compare/ca1a2f84aa386d758e98f8a609d990263932fb85...82040194cd05affb060bf94d6f19f82a771d07fb).
The source blob is verified against the installed package when collecting it.
Zune's short alternative-license notice is retained verbatim and is not labelled
as the full MIT or Apache-2.0 text; its full Zlib terms are the retained complete
license alternative.
