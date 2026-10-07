# gptgrep-parse

`parse_path(&Path).await` ingests one supported local document and returns the
shared `gptgrep_pageindex::ParsedDocument`. `supports_path(&Path)` is the
extension/name capability matrix used by directory discovery.

Plaintext files are read as UTF-8 without changing their text. Extensions include
Markdown/MDX, txt, rst, adoc, log, JSON/JSONL, YAML, TOML, CSV/TSV, XML, HTML,
TeX and org; extensionless README, LICENSE and NOTICE are also supported. Their
`parser` is `plaintext`; line numbers refer to the original file. A file with
invalid UTF-8 fails explicitly, and an empty plaintext file is valid.

PDF, Office, OpenDocument, presentation and image formats use the direct Rust
LiteParse dependency, pinned to `ab8879623ee45a0b7a775d12b42a9a7ae9dab16b`
(`2.15.1`) with default features disabled. No `lit` subprocess is used. The
parser identifier is `liteparse-2.15.1/layout-v1`. Its canonical line numbers
address the stored extracted artifact, supplemented by one-based physical page
citations. For Office/images, these pages belong to the intermediate PDF and are
not source-file line numbers or spreadsheet cell coordinates.

PDFium extracts the PDF text; LiteParse's layout blocks and PDF bookmarks feed
the tree builder. Every heading used with a line locator must match actual
extracted lines. Unlocatable layout headings are omitted with a warning while
all page text remains searchable. The tree profile is LiteParse-derived; it is
not full PageIndex Flash algorithm parity.

Optional deterministic tree merging is exposed separately by
`gptgrep_pageindex::optimize_merge`; `parse_path` leaves the extracted hierarchy
unchanged. The pure crate's stage-coverage table records the verified merge
operators and the remaining Flash-specific layout work.

Native documents are parsed in 25-page batches. GPTgrep overrides LiteParse's
default 1,000-page limit, checks its explicit 100,000-page resource ceiling before
parsing, and requires complete, ordered source page coverage. Page errors or
truncation fail ingestion. Batching bounds parser intermediates; the final
canonical document still holds all extracted text in memory. PDFium is
process-serialized upstream; spawning async tasks does not imply parallel native
PDF extraction.

OCR is **disabled**. Textless scanned/image/vector-only documents fail with an
actionable message instead of becoming empty successful indexes. Native image
conversion uses Rust image/resvg; Office conversion requires an existing
LibreOffice installation. No tools or services are installed or started by the
adapter. PDFs with a usable text layer require no LLM or network inference.

The dependency's PDFium build helper can download its pinned `chromium/8058`
native artifact if a suitable cache, vendor directory, or explicit
`PDFIUM_LIB_PATH` and `PDFIUM_INCLUDE_PATH` is absent. Runtime PDFium is dynamically
loaded. Binary releases must bundle the appropriate library and its notices;
the developer's baked-in cache path is not a portable release dependency.
See [THIRD_PARTY.md](THIRD_PARTY.md).

The loader's actual order is runtime `PDFIUM_LIB_PATH`, compiled cache path,
native module directory, executable directory, then the system library name.
For a relocatable bundle containing `gptgrep.bin` and `lib/libpdfium.dylib` (or
the platform equivalent), a launcher should set `PDFIUM_LIB_PATH` to its own
absolute `lib` directory before executing `gptgrep.bin`. Copying a dylib next to
the executable alone does not establish independence from a builder cache.
Keep this override local to the launched process.

A relocation smoke check must exercise actual PDF parsing in a fresh process
and verify which library loaded, for example macOS `DYLD_PRINT_LIBRARIES=1` or
Linux loader diagnostics. Also run plaintext-only work and verify no PDFium
load occurred. Do not rename or delete the shared cache to simulate relocation.
On September 22, 2026, a relocated macOS ARM64 parser test executable passed the
synthetic two-page PDF test while dyld reported only the copied bundle library;
the plaintext test loaded no PDFium even with an absent override directory.
That validates this parser dependency path; the final CLI archive still needs
its own relocation check. The research sources manifest records the test and
library digests and the observed dyld path.

Run `cargo test -p gptgrep-parse`. Tests include real native PDFium parsing of a
small original synthetic PDF, bookmark/page/line citations, textless-document
failure, plaintext source preservation, format routing and incomplete-page
rejection. Native tests require the same PDFium artifact as the runtime. They do
not use a model, remote inference, or benchmark document downloads.
