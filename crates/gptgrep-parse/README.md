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
default 1,000-page limit with PDFium's signed 32-bit representation bound,
without a whole-document page-count policy. Complete, ordered source page coverage
is required. Page errors or truncation fail ingestion. Batching bounds parser intermediates; the final
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

The separate `liteparse-2.15.1/rich-v1` foundation exposes
`prepare_document`, `extract_native`, and `finalize_document`. It leaves
`parse_path` and its OCR-disabled `layout-v1` behavior intact. Preparation
admits PDF input and copies it once into immutable staging. Standalone PNG or admitted baseline JPEG
input uses one owned Rust conversion into a single-page prepared PDF.
Plaintext/Markdown use an identity UTF-8 preparation and the existing
`pageindex::from_text` source tree: original bytes, LF/CRLF, Unicode and line
numbers remain unchanged. `RichParsedDocument.pages` and coverage are empty
because these documents have no physical pages; `document.pages` retains one
logical source unit for nonempty files and zero for empty files. Native text
references use that logical unit, with no physical geometry or renditions.
Invalid UTF-8 and resource limits fail explicitly.

Office, OpenDocument, presentation and spreadsheet formats use an existing
LibreOffice executable, with no installation. One transaction copies the raw
input to immutable staging, launches the converter once with a fresh private
profile and macros disabled, and retains the normalized PDF. Extraction,
rendering and OCR all consume that same PDF, never reconvert the raw source.
The original raw digest and the retained PDF digest remain distinct. Geometry
is in retained normalized PDF coordinates; original Office page/cell mappings
are explicitly unavailable. Missing runtime, failed conversion, source changes,
extra output files and unsupported required coverage prevent whole-generation
publication and preserve the previous CURRENT.

The converter uses direct argv, closed stdin, an environment containing only
its private HOME/TMP/profile and PATH, 64 KiB per-pipe diagnostic counters (no
retained output text), and a 120-second timeout. Conversion staging is reserved
before launch and checked periodically and at completion. Successful scratch
is deleted with its conservative shared staging reservation still charged;
failed scratch is retained. These checks bound admission and observation, not
hard OS disk/RSS/CPU enforcement, descendant cleanup or external link/network
isolation. The adapter version identifies the pinned recipe, not an observed
LibreOffice version or layout fidelity. Other image formats and XFA/AcroForm
still fail with explicit capability errors.

These additional format recipes extend the existing v2 manifest capability
without changing PDF/PNG/JPEG fields or their validation. Older readers reject
the unfamiliar plaintext/Office format rather than reinterpret logical spans
as physical pages; rebuild or upgrade is required for those new formats.
All preparation and PDFium work runs on blocking workers; PDFium objects end
their synchronous scope before any host OCR future can be awaited.

The bounded image front door admits static 8-bit noninterlaced PNG (gray,
gray/alpha, RGB, RGBA or palette). The separate JPEG route admits only SOF0,
8-bit gray or canonical RGB/YCbCr, 1:1 sampling, one scan containing all frame
components, and non-transforming bounded JFIF/comment headers. Before pixel
decoding, an owned fixed-table validator requires actual coded entropy for every
block, bounded DC categories/predictions and AC runs, at most seven all-one pad
bits, and exact terminal EOI. It never supplies replacement bits. The pinned
`zune-jpeg` decoder also runs with strict mode enabled and unsafe routines
disabled, writing into one admitted caller buffer. Strict mode alone cannot
establish entropy completion. Progressive, restarts, subsampling, multiple
scans/pictures, CMYK/YCCK, metadata/color/orientation transforms and other JPEG
modes fail explicitly before the decoder or staging reservation. Animation,
16-bit or interlaced PNG,
compressed/unknown PNG metadata and unqualified container extensions also fail
before an upstream decoder is constructed. The original source bytes and format
remain bound by SHA-256. The prepared PDF contains uncompressed RGB8 samples. Alpha
is composited against white with explicit integer rounding. One source pixel
occupies one PDF point, with an identity affine between the top-left source
pixel frame and normalized page frame. No EXIF rotation or physical-resolution
rescaling is applied. Bounded PNG gamma/chromaticity metadata is recorded as an
unsupported color-management capability. `PreparedSummary.image_geometry`
records these dimensions, transform and capability states; existing PDF
summaries deserialize with this field absent. The converter receipt identifies
`gptgrep-bounded-image-pdf` and one invocation. PNG retains its existing
`image-0.25.10/png-rgb8-pdf-v2` recipe; JPEG binds the distinct
`zune-jpeg-0.5.15/baseline-strict-rgb8-pdf-v1` recipe. Earlier permissive JPEG
recipes are not accepted by this adapter.
`image_preparation_profile_digest` provides the pure canonical recipe binding,
including source format, for publication validation.

Encoded source size is checked before its input buffer is allocated. A local
container scan then admits dimensions, decoded output and the simultaneous
decoder/input/RGB-row working envelope before constructing the decoder. PNG's
explicit allocation limit is supplemented by its separately bounded unfilter
buffer and row scratch. JPEG's restricted single-scan 1:1 path admits the
borrowed encoded input, caller sample frame, padded component MCU rows and
fixed tables; it excludes upsampling and full-frame coefficient storage. Its
owned working bound does not establish full process RSS. The PDF writer retains
only one RGB row and uses `LimitedWriter`; its `3 * pixels + 16 KiB` output bound
is checked and reserved against shared staging before decoding or opening the
output. That conservative reservation remains charged even when actual output
is smaller or conversion fails. Actual output hashing verifies identity after
writing; it is not the admission check. Later native rendering uses the caller's
explicit DPI and can fail its separate raster budget without silently resizing.

Rich extraction preflights glyph, object, nested Form and path-segment counts
before constructing owned metadata. It retains numeric native glyph data,
raw vector paths/object matrices, normalized page geometry, native text and
metadata artifacts, complexity signals and admitted PNG page renditions.
Native metadata also binds the original source-only complexity output, admitted
image rectangles and original page geometry. The native extraction recipe is
`gptgrep.native.count-admitted.v4-metadata-classifier-geometry-binding`; completed
public DTO consumers must validate these retained facts before accepting mutable
coverage or page-frame fields.
Crop, rotation and UserUnit transformations are explicit invertible affines.
Variable-length font names, tagged structure, annotations, bookmarks and
document metadata are represented as unsupported capabilities. Nested image
geometry fails explicitly because ancestor matrix composition is not yet
qualified. The rich path uses a conservative source-only OCR policy; vector
paths and sparse text can require OCR without a claim that they are unreadable.
An owned, licensed patch of the pinned PDFium wrapper exposes an allocation-free
annotation count. A positive count conservatively requires OCR; zero preserves
the native classifier. Blank coverage requires zero native text, zero page
objects and zero annotations. A white finite-DPI raster is not blank-source
proof. Every rich rendition clears the admitted bitmap and uses the public
display rendering entry with `FPDF_ANNOT` only, so visible non-print annotation
appearances remain in the delivered image. This currently renders twice into
one bitmap; its preprocessing latency must be measured as actual work. The
legacy renderer and its printing flags remain unchanged.

`ResourceBudget::default()` freezes the development contract's admission
maxima. Callers can lower limits. Glyph/object bounds, projected sparse-space
output bounds and image pixel/RGBA dimensions are checked before owned
materialization, projection or raster allocation. Serialized metadata and PNG
output use bounded writers. Image validation reads only the admitted encoded
bytes and checks actual PNG/JPEG headers without pixel decoding. Cloned budgets
share one atomic retained-staging allowance; `reserve_retained_staging` also
lets publication reserve copies before retaining them. Failed candidates keep
their reservation. These controls bound GPTgrep-owned admitted buffers and
staging; they do not establish a process RSS ceiling for PDFium's internal
document/font allocations, or an interruptible timeout for blocking native IO.
Preparation, extraction and finalization must use the same budget or a clone
sharing its original counter. Later limits may only decrease from the admitted
values. A new or deserialized budget cannot reset the invocation's accounting.

`NativeCandidate` exposes immutable `summary()`, `pages()`, `artifacts()`,
`plan()` and `native_profile_sha256()` accessors. It retains a private snapshot
of source, raw pages, coverage, artifacts, task plan and extraction recipe.
Finalization verifies that snapshot and its source/page/artifact/task
relationships before accepting results. Callers may clone task DTOs for a host
executor; they cannot replace candidate fields or remove required OCR coverage.
The completed `RichParsedDocument` is a portable public DTO and remains
untrusted input to publication, which must perform its own validation.

`OcrExecutor` is an object-safe host trait accepting an owned `OcrTask` and
returning an owned future. The parser never launches a model. Tasks bind source,
preparation, artifact recipe and exact image digests; local paths are omitted
from JSON. The selected profile is `gpt-6-luna`, `max`, `fast`, with no fallback.
Complete results must match every required region and retain a receipt for each
declared image's successful mediated delivery. Metadata-only validators perform
no IO so a host can use held directory/file descriptors for path custody;
full validators additionally recheck source and image bytes. Parser path
checks reject observed symlinks but are not a substitute for such descriptor
custody against concurrent ancestor replacement.

Finalization retains every raw OCR region string and the portable result and
delivery receipt. Full-page required OCR replaces the reading view while
preserving the native artifact. Projection, document signals, layout, hierarchy
and line offsets are rebuilt over the complete merged document. Exact byte
segments link native/OCR substrings to the merged view; unmatched lines are
reported explicitly. OCR tasks remain in `RichParsedDocument` so publication
can revalidate original required coverage. `OcrMode::Off` may retain
`RequiredOcr` coverage for inspection; the publication layer must reject that
coverage. Blank-only documents require a future explicit no-content policy.

Artifact IDs bind source SHA-256, parser/options or final OCR recipe SHA-256,
kind, page, region and content SHA-256. Native recipe identity includes the
pinned revision, DPI and rendition encoding and excludes page-window size.
Final recipe identity also binds OCR mode and the complete retained results.
No public serialized summary contains source paths or runtime session IDs.
Synthetic tests exercise actual PNG-to-PDFium pixel roundtrips, gray/alpha
and palette conversion, source and prepared-file mutation, header/decode/owned
working/output/shared-staging failure ordering, strict JPEG gray/RGB known
samples, actual incomplete entropy at EOI, invalid Huffman/run/padding/terminal
content, and excluded modes. These tests
do not qualify all image formats, all Office layouts/converters, orientation/color fidelity, complete
rich metadata, process RSS, native cancellation, live OCR or release packaging.
This foundation is not complete multimodal or full rich-metadata acceptance.
