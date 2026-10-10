//! Parse once into a canonical document with explicit source/page locators.
//!
//! Native parsing uses a pinned LiteParse crate, with OCR disabled. It does not
//! launch a service or invoke a model. See README.md for native dependencies.

use anyhow::{Context, Result, bail, ensure};
use gptgrep_pageindex::{
    Bookmark, Heading, PageInput, ParsedDocument, from_pages, from_text, locate_heading,
};
use liteparse::{LiteParse, LiteParseConfig, OutputFormat, types::PdfInput};
use std::path::{Path, PathBuf};

mod artifact;
mod crop;
mod document_metadata;
#[cfg(test)]
mod document_metadata_tests;
mod geometry;
mod image_prepare;
#[cfg(test)]
mod image_prepare_tests;
pub mod native_graphics;
mod ocr;
mod ocr_compat;
mod office;
mod resource;
mod rich;
#[cfg(test)]
mod rich_tests;
mod rich_text;

pub use artifact::{ArtifactKind, ArtifactRef, FeatureState, artifact_id};
pub use crop::{CropAdmission, CropOutput, CropRecipe, PixelCrop, crop_rendition};
pub use document_metadata::{
    DOCUMENT_METADATA_NATIVE_RECIPE, DOCUMENT_METADATA_SCHEMA, MAX_NATIVE_BOOKMARK_DEPTH,
    MAX_NATIVE_BOOKMARKS, NativeBookmark, NativeBookmarkDestination, NativeDocumentMetadata,
    NativePageLabel, document_metadata_native_profile_digest, validate_native_document_metadata,
};
pub use geometry::{AffineTransform, BoundingBox, PageGeometry};
pub use image_prepare::image_preparation_profile_digest;
pub use liteparse::stages::PageComplexityStats as NativePageComplexity;
pub use liteparse::types::{Page as NativeLayoutPage, WordBox as NativeWordBox};
pub use native_graphics::{
    NATIVE_GRAPHICS_RECIPE, NativePageMetadata, native_graphics_profile_digest,
    native_metadata_working_upper, validate_native_graphics_metadata,
};
pub use ocr::{
    OcrExecutor, OcrImage, OcrImageDelivery, OcrPageResult, OcrProfile, OcrReceipt, OcrRegion,
    OcrRegionResult, OcrRegionStatus, OcrResultStatus, OcrTask, OcrTaskBudget, ocr_profile_digest,
    ocr_task_digest, validate_ocr_result, validate_ocr_result_metadata, validate_ocr_task,
    validate_ocr_task_metadata, validate_rendition, validate_rendition_metadata,
};
pub use ocr_compat::{
    COMPATIBILITY_MODE, CompatibilityOcrRegion, CompatibilityOcrResult, CompatibilityOcrTask,
    compatibility_task_digest, validate_compatibility_result,
};
pub use office::{is_office_extension, office_preparation_profile_digest};
pub use resource::ResourceBudget;
pub use rich::native_text_metadata::{
    NATIVE_FONT_WORD_RECIPE, NativeBounds, NativeCharacter, NativeFontInfo, NativeGeometry,
    NativeTextMetadata, native_font_word_profile_digest, validate_native_text_metadata,
};
pub use rich::{
    ConversionIdentity, CoverageState, NativeCandidate, NativeOptions, NativePage, OcrMode,
    OcrPlan, PreparationOptions, PreparedDocument, PreparedImageGeometry, PreparedSummary,
    RICH_PARSER, RichParsedDocument, SegmentMap, extract_native, finalize_document,
    prepare_document,
};
pub use rich_text::{
    PlaintextMetadata, plaintext_native_profile_digest, plaintext_preparation_profile_digest,
};

pub const NATIVE_PARSER: &str = "liteparse-2.15.1/layout-v1";
pub const LITEPARSE_REVISION: &str = "ab8879623ee45a0b7a775d12b42a9a7ae9dab16b";
/// PDFium represents page counts and indices with signed 32-bit integers.
/// This is a format/runtime representation bound, not a document resource policy.
pub const MAX_NATIVE_PAGES: u32 = i32::MAX as u32;

const TEXT_EXTENSIONS: &[&str] = &[
    "md", "mdx", "markdown", "txt", "rst", "adoc", "log", "json", "jsonl", "yaml", "yml", "toml",
    "csv", "tsv", "xml", "html", "htm", "tex", "org",
];
const NATIVE_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf", "pages", "ppt",
    "pptx", "pptm", "pot", "potm", "potx", "odp", "otp", "key", "xls", "xlsx", "xlsm", "xlsb",
    "ods", "ots", "numbers", "jpg", "jpeg", "png", "gif", "bmp", "tiff", "tif", "webp", "svg",
];

/// Whether this extension/name has a parser route. Native support also requires
/// PDFium, and Office conversion requires LibreOffice. Images/scans without a
/// usable text layer return an OCR-disabled error rather than an empty index.
pub fn supports_path(path: &Path) -> bool {
    is_plaintext(path)
        || extension(path).is_some_and(|ext| NATIVE_EXTENSIONS.contains(&ext.as_str()))
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

pub(crate) fn is_plaintext(path: &Path) -> bool {
    if let Some(ext) = extension(path) {
        return TEXT_EXTENSIONS.contains(&ext.as_str());
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ["README", "LICENSE", "NOTICE"]
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
        })
}

/// Ingest one local file. `parser == "plaintext"` preserves source bytes as
/// UTF-8 and source line numbers; every other parser uses canonical extracted
/// lines together with physical-page citations.
pub async fn parse_path(path: &Path) -> Result<ParsedDocument> {
    ensure!(
        supports_path(path),
        "unsupported document format: {}",
        path.display()
    );
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("cannot read document metadata: {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "document path is not a regular file: {}",
        path.display()
    );
    let title = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if is_plaintext(path) {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("cannot read UTF-8 plaintext: {}", path.display()))?;
        return Ok(from_text(&title, &text));
    }
    let owned = path.to_path_buf();
    // PDFium is synchronous and process-serialized. Keep its work off the
    // caller's async executor and turn an initialization panic into an error.
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot create native parser runtime")?;
        runtime.block_on(parse_native(owned, title))
    })
    .await
    .with_context(|| {
        format!(
            "native LiteParse worker failed for {}; verify the bundled PDFium library or PDFIUM_LIB_PATH (Office files also require LibreOffice)",
            path.display()
        )
    })?
}

async fn parse_native(path: PathBuf, title: String) -> Result<ParsedDocument> {
    let path_text = path
        .to_str()
        .context("LiteParse requires a UTF-8 document path")?;
    let config = LiteParseConfig {
        max_pages: MAX_NATIVE_PAGES as usize,
        ocr_enabled: false,
        quiet: true,
        output_format: OutputFormat::Text,
        extract_blocks: true,
        extract_text_metadata: true,
        continue_on_page_error: false,
        ..Default::default()
    };
    let parser = LiteParse::new(config);
    let mut session = parser
        .open_batch_session(PdfInput::Path(path_text.into()), 25)
        .await
        .with_context(|| format!("cannot open {} with LiteParse; native parsing needs PDFium and Office conversion needs LibreOffice", path.display()))?;
    let total = session.total_pages();
    ensure!(total > 0, "document contains no pages: {}", path.display());
    ensure!(
        total <= MAX_NATIVE_PAGES,
        "document page count cannot be represented by PDFium; no partial index was accepted"
    );
    let mut pages = Vec::new();
    let mut headings = Vec::new();
    let mut bookmarks = Vec::new();
    let mut omitted_headings = 0;
    let mut captured_outline = false;
    while let Some(batch) = session
        .next_batch()
        .await
        .with_context(|| format!("native document extraction failed for {}", path.display()))?
    {
        ensure!(
            batch.result.total_pages == total,
            "source page count changed during parsing; no index was accepted"
        );
        ensure!(
            batch.result.page_errors.is_empty(),
            "LiteParse reported {} failed pages; refusing an incomplete index",
            batch.result.page_errors.len()
        );
        if !captured_outline {
            bookmarks = batch
                .result
                .outline
                .iter()
                .map(|entry| Bookmark {
                    title: entry.title.clone(),
                    level: u32::from(entry.level),
                    page: u32::try_from(entry.page_index)
                        .ok()
                        .and_then(|page| page.checked_add(1))
                        .unwrap_or(0),
                })
                .collect();
            captured_outline = true;
        }
        for page in batch.result.pages {
            let number = u32::try_from(page.page_number).context("invalid native page ordinal")?;
            ensure!(
                number as usize == pages.len() + 1,
                "native parser omitted or reordered a page; refusing an incomplete index"
            );
            // Match headings to actual canonical text. If a layout block cannot
            // be located (e.g. column-order disagreement), retain all page text
            // but omit its unproven line-level heading location.
            let mut offsets = vec![0];
            offsets.extend(page.text.match_indices('\n').map(|(offset, _)| offset + 1));
            let mut next_line = 1usize;
            for block in page.blocks.as_deref().unwrap_or_default() {
                if block.kind != "heading" {
                    continue;
                }
                let (Some(heading_title), Some(level)) = (&block.text, block.level) else {
                    omitted_headings += 1;
                    continue;
                };
                let located = offsets.get(next_line - 1).and_then(|offset| {
                    locate_heading(&page.text[*offset..], heading_title)
                        .map(|relative| next_line + relative - 1)
                });
                if let Some(line) = located {
                    headings.push(Heading {
                        title: heading_title.clone(),
                        level: u32::from(level),
                        page: number,
                        line,
                    });
                    next_line = line + 1;
                } else {
                    omitted_headings += 1;
                }
            }
            pages.push(PageInput {
                number,
                text: page.text,
            });
        }
    }
    ensure_complete(total, &pages)?;
    if pages.iter().all(|page| page.text.trim().is_empty()) {
        bail!(
            "no searchable text extracted from {}; OCR is disabled in this build (images, scans, or vector-only pages need an explicit OCR preprocessing step)",
            path.display()
        );
    }
    let mut document = from_pages(&title, &pages, &headings, &bookmarks, NATIVE_PARSER)?;
    if omitted_headings > 0 {
        document.warnings.push(format!(
            "omitted_layout_headings_without_verified_line_location:{omitted_headings}"
        ));
    }
    Ok(document)
}

fn ensure_complete(total: u32, pages: &[PageInput]) -> Result<()> {
    ensure!(
        pages.len() == total as usize,
        "incomplete extraction: expected {total} source pages, received {}; no index was accepted",
        pages.len()
    );
    ensure!(
        pages
            .iter()
            .enumerate()
            .all(|(index, page)| page.number as usize == index + 1),
        "page coverage is not complete and ordered; no index was accepted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_matrix_matches_actual_routes() {
        for name in [
            "AGENTS.md",
            "notes.llms.txt",
            "page.MDX",
            "README",
            "LICENSE",
            "NOTICE",
            "items.csv",
            "report.PDF",
            "deck.pptx",
            "photo.webp",
        ] {
            assert!(supports_path(Path::new(name)), "{name}");
        }
        for name in ["program.exe", "archive.zip", ".env", "unknown"] {
            assert!(!supports_path(Path::new(name)), "{name}");
        }
        assert!(is_plaintext(Path::new("items.csv")));
    }

    #[tokio::test]
    async fn plaintext_preserves_source_lines_and_accepts_empty_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("document.md");
        std::fs::write(&path, "# 标题\r\nsource\r\n").unwrap();
        let parsed = parse_path(&path).await.unwrap();
        assert_eq!(parsed.parser, "plaintext");
        assert_eq!(parsed.text, "# 标题\r\nsource\r\n");
        assert_eq!(parsed.nodes[0].line_end, 2);
        std::fs::write(&path, "").unwrap();
        assert!(parse_path(&path).await.unwrap().nodes.is_empty());
    }

    #[test]
    fn source_page_count_cannot_be_mistaken_for_emitted_page_count() {
        let truncated: Vec<_> = (1..=1000)
            .map(|number| PageInput {
                number,
                text: "x".into(),
            })
            .collect();
        assert!(ensure_complete(1001, &truncated).is_err());
        let mut complete = truncated;
        complete.push(PageInput {
            number: 1001,
            text: "x".into(),
        });
        assert!(ensure_complete(1001, &complete).is_ok());
        complete[500].number = 500;
        assert!(ensure_complete(1001, &complete).is_err());
    }

    // A tiny original fixture, generated here rather than copying a benchmark
    // document. Real PDFium parsing exercises the native dependency and page
    // citation contract; this is not a mocked parser acceptance test.
    pub(super) fn pdf_fixture(with_text: bool) -> Vec<u8> {
        let content = |heading: &str, body: &str| {
            if with_text {
                format!("BT /F1 22 Tf 72 720 Td ({heading}) Tj 0 -35 Td /F1 11 Tf ({body}) Tj ET")
            } else {
                String::new()
            }
        };
        let first = content("Heading One", "First page searchable evidence.");
        let second = content("Heading Two", "Second page distinct evidence.");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R /Outlines 8 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", first.len(), first),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", second.len(), second),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            "<< /Type /Outlines /First 9 0 R /Last 10 0 R /Count 2 >>".into(),
            "<< /Title (Heading One) /Parent 8 0 R /Next 10 0 R /Dest [3 0 R /Fit] >>".into(),
            "<< /Title (Heading Two) /Parent 8 0 R /Prev 9 0 R /Dest [5 0 R /Fit] >>".into(),
        ];
        pdf_objects(&objects)
    }

    pub(super) fn pdf_objects(objects: &[String]) -> Vec<u8> {
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = vec![0];
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
        }
        let xref = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()));
        for offset in offsets.iter().skip(1) {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        ));
        pdf.into_bytes()
    }

    fn many_page_fixture(count: usize) -> Vec<u8> {
        let kids = (0..count)
            .map(|index| format!("{} 0 R", 4 + index * 2))
            .collect::<Vec<_>>()
            .join(" ");
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>"),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        for index in 0..count {
            let content_id = 5 + index * 2;
            let content = format!(
                "BT /F1 11 Tf 72 700 Td (Document evidence on physical page {}) Tj ET",
                index + 1
            );
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        pdf_objects(&objects)
    }

    #[tokio::test]
    async fn native_pdf_extracts_real_text_pages_and_bookmark_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.pdf");
        std::fs::write(&path, pdf_fixture(true)).unwrap();
        let document = parse_path(&path).await.unwrap();
        assert_eq!(document.parser, NATIVE_PARSER);
        assert_eq!(document.pages.len(), 2);
        assert!(document.text.contains("First page searchable evidence"));
        assert!(document.text.contains("Second page distinct evidence"));
        assert!(
            document
                .nodes
                .iter()
                .any(|node| node.title == "Heading Two" && node.page_start == 2)
        );
        for page in &document.pages {
            assert!(
                document
                    .nodes
                    .iter()
                    .any(|node| node.page_start <= page.number && page.number <= node.page_end)
            );
            let selected = document
                .text
                .lines()
                .skip(page.line_start - 1)
                .take(page.line_end - page.line_start + 1)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(selected, page.text);
        }
    }

    #[tokio::test]
    async fn native_textless_pdf_fails_instead_of_accepting_an_empty_index() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blank.pdf");
        std::fs::write(&path, pdf_fixture(false)).unwrap();
        let error = parse_path(&path).await.unwrap_err().to_string();
        assert!(error.contains("OCR is disabled"), "{error}");
    }

    #[tokio::test]
    async fn native_batches_cover_beyond_the_upstream_default_thousand_page_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("1001-pages.pdf");
        std::fs::write(&path, many_page_fixture(1001)).unwrap();
        let document = parse_path(&path).await.unwrap();
        assert_eq!(document.pages.len(), 1001);
        let last = document.pages.last().unwrap();
        assert_eq!(last.number, 1001);
        assert!(last.text.contains("physical page 1001"));
        assert_eq!(last.line_end, document.text.lines().count());
        assert!(document.nodes.iter().any(|node| node.page_end == 1001));
    }
}
