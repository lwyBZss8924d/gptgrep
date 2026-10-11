//! Logical owned-allocation bounds for the native candidate's retained data.
//! PDFium/native RSS and allocator bookkeeping are outside this parser budget.
//! Current-page extraction reservations and retained artifact bytes are separate.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use liteparse::types::{Page, TextItem};

use super::{CoverageState, NativeCandidate, NativePage, PreparedDocument, PreparedSummary};
use crate::ResourceBudget;
use crate::artifact::{ArtifactRef, FeatureState};
use crate::document_metadata::{NativeBookmark, NativeDocumentMetadata, NativePageLabel};
use crate::ocr::{OcrImage, OcrPageResult, OcrRegion, OcrRegionResult, OcrTask};

#[derive(Debug)]
pub(super) struct NativeAdmission {
    retained_owned: u64,
    active_owned: u64,
    metadata_bytes: u64,
}

impl NativeAdmission {
    pub(super) fn new(retained_owned: u64, budget: &ResourceBudget) -> Result<Self> {
        budget.validate()?;
        ensure!(
            retained_owned <= budget.owned_working_bytes,
            "resource_limit: native fixed owned admission"
        );
        Ok(Self {
            retained_owned,
            active_owned: 0,
            metadata_bytes: 0,
        })
    }

    pub(super) fn held_upper(&self) -> Result<u64> {
        self.retained_owned
            .checked_add(self.active_owned)
            .context("resource_limit: native retained/active sum overflow")
    }

    /// The existing character/font/graphics estimates include the current page's
    /// source DTO, layout and serialized representations. Keep their 4x allowance
    /// active through every use of those representations, then release it once.
    pub(super) fn reserve_native(&mut self, bytes: u64, budget: &ResourceBudget) -> Result<()> {
        self.reserve_owned(
            bytes
                .checked_mul(4)
                .context("resource_limit: native working bound overflow")?,
            budget,
        )
    }

    pub(super) fn reserve_owned(&mut self, bytes: u64, budget: &ResourceBudget) -> Result<()> {
        let active = self
            .active_owned
            .checked_add(bytes)
            .context("resource_limit: native active working sum overflow")?;
        let held = self
            .retained_owned
            .checked_add(active)
            .context("resource_limit: native complete working sum overflow")?;
        ensure!(
            held <= budget.owned_working_bytes,
            "resource_limit: native retained plus active working admission: retained_bytes={}, active_bytes={active}, limit_bytes={}",
            self.retained_owned,
            budget.owned_working_bytes
        );
        self.active_owned = active;
        Ok(())
    }

    pub(super) fn admit_metadata(&mut self, bytes: u64, budget: &ResourceBudget) -> Result<()> {
        let next = self
            .metadata_bytes
            .checked_add(bytes)
            .context("resource_limit: native retained metadata length overflow")?;
        ensure!(
            next <= budget.document_metadata_bytes,
            "resource_limit: native retained metadata admission: retained_bytes={next}, limit_bytes={}",
            budget.document_metadata_bytes
        );
        self.metadata_bytes = next;
        Ok(())
    }

    /// Call only after all nonretained products of this phase have been dropped.
    /// The replacement is a capacity-aware bound for the products that survive;
    /// it is not a refund of staged files or a reset of previous retained pages.
    pub(super) fn finish_phase(&mut self, retained: u64, budget: &ResourceBudget) -> Result<()> {
        let next = self
            .retained_owned
            .checked_add(retained)
            .context("resource_limit: native retained owned sum overflow")?;
        ensure!(
            next <= budget.owned_working_bytes,
            "resource_limit: native retained owned admission"
        );
        self.retained_owned = next;
        self.active_owned = 0;
        Ok(())
    }

    pub(super) fn retained_owned(&self) -> u64 {
        self.retained_owned
    }
    pub(super) fn metadata_bytes(&self) -> u64 {
        self.metadata_bytes
    }
}

#[derive(Default)]
struct Owned(u64);

impl Owned {
    fn add(&mut self, bytes: u64) -> Result<()> {
        self.0 = self
            .0
            .checked_add(bytes)
            .context("resource_limit: retained owned size overflow")?;
        Ok(())
    }

    fn slots<T>(&mut self, count: usize) -> Result<()> {
        self.add(
            (count as u64)
                .checked_mul(size_of::<T>() as u64)
                .context("resource_limit: retained typed capacity overflow")?,
        )
    }

    fn string(&mut self, value: &String) -> Result<()> {
        self.add(value.capacity() as u64)
    }

    fn optional_string(&mut self, value: &Option<String>) -> Result<()> {
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn path(&mut self, value: &PathBuf) -> Result<()> {
        // Paths also undergo bounded UTF-8/native-path conversions. Preserve a
        // conservative four-byte allowance per reserved native storage unit.
        self.add(
            (value.capacity() as u64)
                .checked_mul(4)
                .context("resource_limit: retained path capacity overflow")?,
        )
    }

    fn state<T>(
        &mut self,
        value: &FeatureState<T>,
        present: impl FnOnce(&T) -> Result<u64>,
    ) -> Result<()> {
        match value {
            FeatureState::Present(value) => self.add(present(value)?),
            FeatureState::Absent => Ok(()),
            FeatureState::Unsupported(reason) | FeatureState::Failed(reason) => self.string(reason),
        }
    }

    fn string_state(&mut self, value: &FeatureState<String>) -> Result<()> {
        self.state(value, |value| Ok(value.capacity() as u64))
    }

    fn strings_map(&mut self, values: &BTreeMap<String, FeatureState<String>>) -> Result<()> {
        // A nonempty BTree node owns at least one entry. Per entry, admit sixteen
        // key/value slots plus 256 bytes for node headers/edges, exceeding the
        // pinned std BTree node's eleven slots and twelve edges. Heap strings
        // are counted separately at their actual capacities below.
        let node = (size_of::<String>() + size_of::<FeatureState<String>>()) as u64;
        let node = node
            .checked_mul(16)
            .and_then(|n| n.checked_add(256))
            .context("resource_limit: retained map node overflow")?;
        self.add(
            (values.len() as u64)
                .checked_mul(node)
                .context("resource_limit: retained map capacity overflow")?,
        )?;
        for (key, value) in values {
            self.string(key)?;
            self.string_state(value)?;
        }
        Ok(())
    }
}

fn summary_heap(value: &PreparedSummary) -> Result<u64> {
    let mut n = Owned::default();
    for s in [
        &value.source_sha256,
        &value.prepared_sha256,
        &value.format,
        &value.parser_identity,
        &value.preparation_profile_sha256,
    ] {
        n.string(s)?;
    }
    n.state(&value.conversion, |conversion| {
        (conversion.converter.capacity() as u64)
            .checked_add(conversion.version.capacity() as u64)
            .context("resource_limit: retained conversion identity overflow")
    })?;
    n.state(&value.image_geometry, |geometry| {
        let mut n = Owned::default();
        n.string_state(&geometry.source_orientation)?;
        n.string_state(&geometry.source_color_profile)?;
        n.string(&geometry.alpha_background)?;
        Ok(n.0)
    })?;
    Ok(n.0)
}

pub(super) fn fixed_owned(prepared: &PreparedDocument, profile: &String) -> Result<u64> {
    let mut n = Owned(size_of::<NativeCandidate>() as u64 + 256);
    // Both candidate.summary and prepared.summary survive; cloning a string
    // needs no more than its existing capacity. Include the extraction input
    // path copy and the future frozen digest, although the input is temporary.
    n.add(
        summary_heap(&prepared.summary)?
            .checked_mul(2)
            .context("resource_limit: retained summary copies overflow")?,
    )?;
    n.path(&prepared.source_path)?;
    n.path(&prepared.prepared_path)?;
    n.path(&prepared.staging_root)?;
    n.path(&prepared.prepared_path)?;
    n.string(profile)?;
    n.add(64)?;
    Ok(n.0)
}

pub(super) fn artifact_owned(value: &ArtifactRef) -> Result<u64> {
    let mut n = Owned(size_of::<ArtifactRef>() as u64);
    for s in [&value.id, &value.sha256, &value.profile_sha256, &value.mime] {
        n.string(s)?;
    }
    n.path(&value.relative_path)?;
    n.optional_string(&value.region_id)?;
    Ok(n.0)
}

pub(super) fn document_owned(value: &NativeDocumentMetadata) -> Result<u64> {
    let mut n = Owned(size_of::<NativeDocumentMetadata>() as u64);
    for s in [
        &value.schema_version,
        &value.source_sha256,
        &value.prepared_sha256,
    ] {
        n.string(s)?;
    }
    n.strings_map(&value.info)?;
    n.strings_map(&value.capabilities)?;
    n.slots::<NativePageLabel>(value.page_labels.capacity())?;
    for label in &value.page_labels {
        n.string_state(&label.label)?;
    }
    n.state(&value.bookmarks, |bookmarks| {
        let mut n = Owned::default();
        n.slots::<NativeBookmark>(bookmarks.capacity())?;
        for bookmark in bookmarks {
            n.string(&bookmark.title)?;
            n.state(&bookmark.destination, |destination| {
                let mut n = Owned::default();
                n.state(&destination.source_y, |_| Ok(0))?;
                Ok(n.0)
            })?;
        }
        Ok(n.0)
    })?;
    Ok(n.0)
}

/// Preallocate all top-level candidate vectors once, while the document phase
/// holds the reservation. This avoids uncharged old/new vector overlap later.
pub(super) fn candidate_slot_reservation(total: usize, artifacts: usize) -> Result<u64> {
    let mut n = Owned::default();
    n.slots::<Page>(total)?;
    n.slots::<NativePage>(total)?;
    n.slots::<OcrTask>(total)?;
    n.slots::<u32>(total)?;
    n.slots::<(u32, CoverageState)>(total)?;
    n.slots::<String>(total)?;
    n.slots::<ArtifactRef>(artifacts)?;
    // Includes the old small artifact vector during exact reallocation and
    // permits allocator-reported capacity above the requested slot count.
    n.0.checked_mul(2)
        .and_then(|n| n.checked_add(4096))
        .context("resource_limit: candidate vector reservation overflow")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn candidate_slots_owned(
    raw: &Vec<Page>,
    pages: &Vec<NativePage>,
    tasks: &Vec<OcrTask>,
    annotations: &Vec<u32>,
    coverage: &Vec<(u32, CoverageState)>,
    blockers: &Vec<String>,
    artifacts: &Vec<ArtifactRef>,
) -> Result<u64> {
    // Vec references are required here: slice lengths omit unused capacity.
    let mut n = Owned::default();
    n.slots::<Page>(raw.capacity())?;
    n.slots::<NativePage>(pages.capacity())?;
    n.slots::<OcrTask>(tasks.capacity())?;
    n.slots::<u32>(annotations.capacity())?;
    n.slots::<(u32, CoverageState)>(coverage.capacity())?;
    n.slots::<String>(blockers.capacity())?;
    n.slots::<ArtifactRef>(artifacts.capacity())?;
    Ok(n.0)
}

pub(super) fn raw_page_owned(page: &Page) -> Result<u64> {
    let Page {
        page_number: _,
        page_width: _,
        page_height: _,
        content_bounds: _,
        page_label,
        text_items,
        graphics,
        vector_graphics,
        struct_nodes,
        image_refs,
        annotations,
        form_fields,
        structure_tree,
    } = page;
    // These unsupported native extraction products must be deliberately added
    // to this accounting seam when their existing capability gate is lifted.
    ensure!(
        graphics.is_empty()
            && struct_nodes.is_empty()
            && image_refs.is_empty()
            && annotations.is_none()
            && form_fields.is_none()
            && structure_tree.is_none(),
        "capability_unavailable: unaccounted native retained page feature"
    );
    let mut n = Owned(size_of::<Page>() as u64);
    n.optional_string(page_label)?;
    n.slots::<TextItem>(text_items.capacity())?;
    n.slots::<liteparse::types::GraphicPrimitive>(graphics.capacity())?;
    n.slots::<liteparse::types::StructNode>(struct_nodes.capacity())?;
    n.slots::<liteparse::types::ImageRef>(image_refs.capacity())?;
    for item in text_items {
        let TextItem {
            text,
            font_name,
            fill_color,
            stroke_color,
            link,
            char_codes,
            words,
            x: _,
            y: _,
            width: _,
            height: _,
            rotation: _,
            font_size: _,
            font_height: _,
            font_ascent: _,
            font_descent: _,
            font_weight: _,
            font_flags: _,
            text_width: _,
            font_is_buggy: _,
            has_unicode_map_error: _,
            mcid: _,
            trailing_space_generated: _,
            confidence: _,
            strike: _,
        } = item;
        n.string(text)?;
        for value in [font_name, fill_color, stroke_color, link] {
            n.optional_string(value)?;
        }
        n.slots::<u32>(char_codes.capacity())?;
        n.slots::<liteparse::types::WordBox>(words.capacity())?;
        for word in words {
            n.string(&word.text)?;
        }
    }
    if let Some(vector) = vector_graphics {
        n.slots::<liteparse::types::VectorShape>(vector.shapes.capacity())?;
        n.slots::<liteparse::types::VectorLine>(vector.lines.capacity())?;
        for shape in &vector.shapes {
            n.optional_string(&shape.stroke_color)?;
            n.optional_string(&shape.fill_color)?;
        }
        for line in &vector.lines {
            n.optional_string(&line.stroke_color)?;
            n.optional_string(&line.fill_color)?;
        }
    }
    Ok(n.0)
}

fn image_heap(image: &OcrImage) -> Result<u64> {
    let mut n = Owned::default();
    n.add(artifact_owned(&image.artifact)?)?;
    n.path(&image.path)?;
    Ok(n.0)
}

fn page_owned(page: &NativePage) -> Result<u64> {
    let mut n = Owned(size_of::<NativePage>() as u64);
    n.string_state(&page.label)?;
    n.add(artifact_owned(&page.native_text)?)?;
    n.add(artifact_owned(&page.metadata_artifact)?)?;
    n.slots::<OcrImage>(page.renditions.capacity())?;
    for image in &page.renditions {
        n.add(image_heap(image)?)?;
    }
    n.strings_map(&page.capabilities)?;
    n.slots::<liteparse::ocr_merge::ComplexityReason>(page.complexity.reasons.capacity())?;
    if let Some(layout) = &page.complexity.layout {
        n.slots::<liteparse::ocr_merge::LayoutComplexityReason>(layout.reasons.capacity())?;
    }
    Ok(n.0)
}

fn task_owned(task: &OcrTask) -> Result<u64> {
    let mut n = Owned(size_of::<OcrTask>() as u64);
    for s in [
        &task.schema_version,
        &task.id,
        &task.task_sha256,
        &task.source_sha256,
        &task.prepared_sha256,
        &task.native_text,
        &task.layout_context,
        &task.profile.model,
        &task.profile.reasoning_effort,
        &task.profile.service_tier,
    ] {
        n.string(s)?;
    }
    n.path(&task.source_path)?;
    n.slots::<OcrRegion>(task.regions.capacity())?;
    for region in &task.regions {
        n.string(&region.id)?;
        n.string(&region.reason)?;
    }
    n.slots::<OcrImage>(task.images.capacity())?;
    for image in &task.images {
        n.add(image_heap(image)?)?;
    }
    Ok(n.0)
}

pub(super) fn retained_page_owned(
    raw: &Page,
    page: &NativePage,
    artifacts: &[ArtifactRef],
    tasks: &[OcrTask],
    blocker: Option<&String>,
) -> Result<u64> {
    let mut n = Owned(raw_page_owned(raw)?);
    n.add(page_owned(page)?)?;
    for artifact in artifacts {
        n.add(artifact_owned(artifact)?)?;
    }
    for task in tasks {
        n.add(task_owned(task)?)?;
    }
    if let Some(blocker) = blocker {
        n.string(blocker)?;
    }
    Ok(n.0)
}

pub(super) fn ocr_replacement_working_upper(outcomes: &[OcrPageResult]) -> Result<u64> {
    let mut n = Owned::default();
    n.slots::<(u32, String, ArtifactRef)>(outcomes.len())?;
    for result in outcomes {
        // Ordered region references plus stable-sort scratch.
        n.slots::<&OcrRegionResult>(
            result
                .regions
                .len()
                .checked_mul(2)
                .context("resource_limit: OCR region sort capacity overflow")?,
        )?;
        for region in &result.regions {
            let lines = region.text.lines().count();
            // The replacement Vec reserves the complete count exactly before
            // push. The old backing allocation remains in native_owned_upper.
            n.slots::<TextItem>(lines)?;
            // collect::<Vec<&str>>() can grow; four slots per line cover its
            // retained capacity plus old/new allocation overlap on growth.
            n.slots::<&str>(
                lines
                    .checked_mul(4)
                    .context("resource_limit: OCR line scratch capacity overflow")?,
            )?;
        }
    }
    Ok(n.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_page_estimates_do_not_become_document_character_limits() -> Result<()> {
        let budget = ResourceBudget::default();
        let mut ledger = NativeAdmission::new(4096, &budget)?;
        for _ in 0..24 {
            ledger.reserve_native(3_000 * 2_048, &budget)?;
            assert_eq!(ledger.active_owned, 3_000 * 2_048 * 4);
            ledger.admit_metadata(1_048_576, &budget)?;
            ledger.finish_phase(1_048_576, &budget)?;
        }
        assert_eq!(ledger.held_upper()?, 4096 + 24 * 1_048_576);
        assert_eq!(ledger.metadata_bytes(), 24 * 1_048_576);
        assert!(24 * 3_000 * 2_048 > budget.document_metadata_bytes);
        Ok(())
    }

    #[test]
    fn simultaneous_retained_and_active_memory_and_wire_limits_still_reject() -> Result<()> {
        let mut budget = ResourceBudget::default();
        budget.owned_working_bytes = 16_384;
        budget.document_metadata_bytes = 8192;
        let mut ledger = NativeAdmission::new(8192, &budget)?;
        ledger.reserve_native(2048, &budget)?;
        assert!(ledger.reserve_native(1, &budget).is_err());
        assert_eq!(ledger.held_upper()?, 16_384);
        ledger.admit_metadata(8192, &budget)?;
        assert!(ledger.admit_metadata(1, &budget).is_err());
        assert_eq!(ledger.metadata_bytes(), 8192);
        ledger.finish_phase(8192, &budget)?;
        assert!(ledger.reserve_native(1, &budget).is_err());
        assert!(ledger.finish_phase(1, &budget).is_err());
        Ok(())
    }

    #[test]
    fn native_overflow_is_atomic() -> Result<()> {
        let budget = ResourceBudget::default();
        let mut ledger = NativeAdmission::new(0, &budget)?;
        assert!(ledger.reserve_native(u64::MAX, &budget).is_err());
        assert_eq!(ledger.held_upper()?, 0);
        ledger.admit_metadata(1, &budget)?;
        assert!(ledger.admit_metadata(u64::MAX, &budget).is_err());
        assert_eq!(ledger.metadata_bytes(), 1);
        Ok(())
    }

    fn raw(items: Vec<TextItem>) -> Page {
        Page {
            page_number: 1,
            page_label: None,
            page_width: 612.0,
            page_height: 792.0,
            content_bounds: None,
            text_items: items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations: None,
            form_fields: None,
            structure_tree: None,
        }
    }

    #[test]
    fn retained_raw_counts_unused_glyph_run_and_nested_capacities() -> Result<()> {
        let compact = raw(vec![TextItem {
            text: "ordinary text".into(),
            ..Default::default()
        }]);
        let mut runs = Vec::with_capacity(3000);
        let mut item = TextItem {
            text: "ordinary text".into(),
            ..Default::default()
        };
        item.text.reserve_exact(4096);
        item.char_codes.reserve_exact(3000);
        item.words.reserve_exact(100);
        runs.push(item);
        let reserved = raw(runs);
        assert_eq!(compact.text_items.len(), reserved.text_items.len());
        let extra = raw_page_owned(&reserved)? - raw_page_owned(&compact)?;
        assert!(
            extra
                >= (2999 * size_of::<TextItem>()
                    + 3000 * size_of::<u32>()
                    + 100 * size_of::<liteparse::types::WordBox>()
                    + 4096) as u64
        );
        Ok(())
    }

    #[test]
    fn unsupported_native_retention_cannot_silently_escape_accounting() {
        let mut page = raw(Vec::new());
        page.annotations = Some(Vec::new());
        assert!(raw_page_owned(&page).is_err());
    }

    #[test]
    fn short_ocr_lines_admit_typed_replacements_before_wire_sized_work() -> Result<()> {
        use crate::ocr::{OcrReceipt, OcrRegionStatus, OcrResultStatus};
        let result = OcrPageResult {
            schema_version: "gptgrep.ocr.result.v1".into(),
            task_id: "task".into(),
            task_sha256: "0".repeat(64),
            source_sha256: "0".repeat(64),
            prepared_sha256: "0".repeat(64),
            page: 1,
            status: OcrResultStatus::Complete,
            regions: vec![OcrRegionResult {
                id: "region".into(),
                bbox: crate::geometry::BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 612.0,
                    height: 792.0,
                },
                text: "a\n".repeat(2048),
                reading_order: 0,
                status: OcrRegionStatus::Complete,
            }],
            receipt: OcrReceipt {
                model: "model".into(),
                profile_sha256: "0".repeat(64),
                request_sha256: "0".repeat(64),
                response_sha256: "0".repeat(64),
                runtime_profile_sha256: "0".repeat(64),
                image_deliveries: Vec::new(),
                usage: None,
                latency_ms: None,
            },
        };
        let encoded = crate::resource::encoded_size(&result, 262_144)?;
        let typed = ocr_replacement_working_upper(std::slice::from_ref(&result))?;
        assert!(typed > encoded * 8);
        assert!(typed >= (2048 * size_of::<TextItem>()) as u64);
        Ok(())
    }

    // Self-contained ordinary text fixture. No external document, benchmark
    // content, questions, answers or source-location priors enter this test.
    fn generated_text_pdf(
        pages: usize,
        lines: usize,
        text: &str,
        font: u32,
        leading: u32,
    ) -> Vec<u8> {
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Count {pages} /Kids [{}] >>",
                (0..pages)
                    .map(|page| format!("{} 0 R", 4 + page * 2))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_string(),
        ];
        for page in 0..pages {
            let stream = 5 + page * 2;
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {stream} 0 R >>"));
            let mut content = format!("BT /F1 {font} Tf {leading} TL 20 770 Td\n");
            for line in 0..lines {
                if line > 0 {
                    content.push_str("T*\n");
                }
                content.push_str(&format!("({text}) Tj\n"));
            }
            content.push_str("ET\n");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{}endstream",
                content.len(),
                content
            ));
        }
        let mut pdf = "%PDF-1.4\n".to_string();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
        }
        let start = pdf.len();
        pdf.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
            objects.len() + 1
        ));
        pdf.into_bytes()
    }

    fn observed_native_characters(candidate: &NativeCandidate) -> Result<u64> {
        candidate.pages.iter().try_fold(0u64, |total, page| {
            let bytes = crate::artifact::read_bounded(
                &candidate
                    .prepared
                    .staging_root
                    .join(&page.metadata_artifact.relative_path),
                page.metadata_artifact.bytes,
                &page.metadata_artifact.sha256,
            )?;
            let metadata: crate::native_graphics::NativePageMetadata =
                serde_json::from_slice(&bytes)?;
            total
                .checked_add(u64::from(metadata.native_text_metadata.native_char_count))
                .context("test native count overflow")
        })
    }

    #[tokio::test]
    async fn generated_dense_pages_cross_old_total_character_cap_with_retained_limits() -> Result<()>
    {
        use crate::{NativeOptions, PreparationOptions, extract_native, prepare_document};
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("ordinary-dense.pdf");
        let line = "ordinary repeated content ordinary repeated content";
        // 14 * 50 * 51 visible characters already exceed 32768, independent of
        // PDFium's generated line-control characters. A4 coordinates and font.
        std::fs::write(&source, generated_text_pdf(14, 50, line, 12, 14))?;
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &directory.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await?;
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                page_window: 1,
                dpi: 96.0,
                render_pages: false,
            },
            &budget,
        )
        .await?;
        let characters = observed_native_characters(&candidate)?;
        assert!(characters > 32_768);
        assert!(characters * 2048 > budget.document_metadata_bytes);
        assert!(candidate.metadata_admitted_upper < budget.document_metadata_bytes);
        assert!(candidate.native_owned_upper < budget.owned_working_bytes);
        candidate.verify_integrity(&budget)?;
        let mut wire_budget = ResourceBudget::default();
        wire_budget.document_metadata_bytes = candidate.metadata_admitted_upper - 1;
        let limited = prepare_document(
            &source,
            &directory.path().join("wire-limit"),
            &PreparationOptions,
            &wire_budget,
        )
        .await?;
        let error = extract_native(
            &limited,
            &NativeOptions {
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
            &wire_budget,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("native retained metadata admission"),
            "{error:#}"
        );
        let mut working_budget = ResourceBudget::default();
        working_budget.owned_working_bytes = 1_048_576;
        let limited = prepare_document(
            &source,
            &directory.path().join("working-limit"),
            &PreparationOptions,
            &working_budget,
        )
        .await?;
        let error = extract_native(
            &limited,
            &NativeOptions {
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
            &working_budget,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("native character allocation admission"),
            "{error:#}"
        );
        // This fixture asserts native extraction and validation only. Dense
        // final projection has its separate current working-space admission.
        Ok(())
    }

    #[tokio::test]
    async fn generated_long_text_finalizes_identically_across_native_page_windows() -> Result<()> {
        use crate::{
            NativeOptions, OcrMode, PreparationOptions, extract_native, finalize_document,
            prepare_document,
        };
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("ordinary-large-print.pdf");
        // 60 * 22 * 25 visible characters. The large-print font gives a modest
        // projection column bound while still exceeding the old native cap.
        std::fs::write(
            &source,
            generated_text_pdf(60, 22, "ordinary repeated content", 36, 34),
        )?;
        let mut previous = None;
        for window in [1, 25] {
            let budget = ResourceBudget::default();
            let prepared = prepare_document(
                &source,
                &directory.path().join(format!("stage-{window}")),
                &PreparationOptions,
                &budget,
            )
            .await?;
            let candidate = extract_native(
                &prepared,
                &NativeOptions {
                    page_window: window,
                    dpi: 96.0,
                    render_pages: false,
                },
                &budget,
            )
            .await?;
            assert!(observed_native_characters(&candidate)? > 32_768);
            let document = finalize_document(candidate, Vec::new(), OcrMode::Off, &budget)?;
            let fingerprint = (
                document.document.text,
                document.native_profile_sha256,
                document
                    .artifacts
                    .iter()
                    .map(|a| (a.id.clone(), a.sha256.clone()))
                    .collect::<Vec<_>>(),
            );
            if let Some(previous) = previous {
                assert_eq!(previous, fingerprint);
            }
            previous = Some(fingerprint);
        }
        Ok(())
    }

    const FONT_TEST_LINE: &str = "WMWMWMWMWMWMWMWMWMWMWMWMWMWMWMWMWMWMWMWM";
    const FONT_TEST_ROWS: usize = 6;
    const FONT_TEST_NAMES: [&str; 2] = ["Courier-BoldOblique", "Courier-Bold"];

    // Ordinary authored text and standard PDF fonts only. The wider page keeps
    // all forty large-print glyphs inside its frame; no corpus input is used.
    fn generated_font_metadata_pdf(mixed: bool) -> Vec<u8> {
        let mut content = String::from("BT 50 TL 20 740 Td\n");
        for row in 0..FONT_TEST_ROWS {
            if row > 0 {
                content.push_str("T*\n");
            }
            let font = if mixed { row % 2 + 1 } else { 1 };
            content.push_str(&format!("/F{font} 48 Tf ({FONT_TEST_LINE}) Tj\n"));
        }
        content.push_str("ET\n");
        crate::tests::pdf_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1224 792] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /{} >>", FONT_TEST_NAMES[0]),
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /{} >>", FONT_TEST_NAMES[1]),
        ])
    }

    fn page_metadata_bytes(candidate: &NativeCandidate) -> Result<Vec<u8>> {
        assert_eq!(candidate.pages.len(), 1);
        let artifact = &candidate.pages[0].metadata_artifact;
        crate::artifact::read_bounded(
            &candidate
                .prepared
                .staging_root
                .join(&artifact.relative_path),
            artifact.bytes,
            &artifact.sha256,
        )
    }

    async fn generated_font_page_wire_boundary(mixed: bool) -> Result<()> {
        use crate::{NativeOptions, PreparationOptions, extract_native, prepare_document};
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("ordinary-fonts.pdf");
        std::fs::write(&source, generated_font_metadata_pdf(mixed))?;
        let options = NativeOptions {
            render_pages: false,
            ..Default::default()
        };
        // Match the ordinary CLI single-document share of the working budget.
        let mut baseline_budget = ResourceBudget::default();
        baseline_budget.owned_working_bytes /= 2;
        let prepared = prepare_document(
            &source,
            &directory.path().join("baseline"),
            &PreparationOptions,
            &baseline_budget,
        )
        .await?;
        let baseline = extract_native(&prepared, &options, &baseline_budget).await?;
        let bytes = page_metadata_bytes(&baseline)?;
        let metadata: crate::NativePageMetadata = serde_json::from_slice(&bytes)?;
        let wire = bytes.len() as u64;
        assert_eq!(crate::resource::encoded_size(&metadata, wire)?, wire);
        let stream = &metadata.native_text_metadata;
        assert_eq!(stream.characters.len(), stream.native_char_count as usize);
        let mut authored = String::new();
        let mut old_font_upper = 0u64;
        for (ordinal, character) in stream.characters.iter().enumerate() {
            assert_eq!(character.ordinal as usize, ordinal);
            if let FeatureState::Present(info) = &character.font {
                // The pinned getter reserves 24*length+256 before allocation;
                // its native length includes the trailing NUL removed on fill.
                old_font_upper += (info.raw_name.len() as u64 + 1) * 24 + 256;
            }
            let value = char::from_u32(character.unicode).context("fixture Unicode scalar")?;
            if value.is_control() {
                continue;
            }
            assert!(!character.generated);
            assert!(!character.has_unicode_map_error);
            assert_eq!(character.char_code, character.unicode);
            let row = authored.len() / FONT_TEST_LINE.len();
            let expected = FONT_TEST_NAMES[if mixed { row % 2 } else { 0 }];
            let FeatureState::Present(info) = &character.font else {
                panic!("authored character lost native font information");
            };
            assert_eq!(info.raw_name, expected.as_bytes());
            assert!(matches!(&info.name, FeatureState::Present(name) if name == expected));
            authored.push(value);
        }
        assert_eq!(authored, FONT_TEST_LINE.repeat(FONT_TEST_ROWS));
        assert!(
            old_font_upper > wire,
            "fixture must distinguish the old font estimate: font={old_font_upper}, wire={wire}"
        );
        assert!(baseline.native_owned_upper < baseline_budget.owned_working_bytes);
        baseline.verify_integrity(&baseline_budget)?;

        let mut exact_budget = ResourceBudget::default();
        exact_budget.owned_working_bytes /= 2;
        exact_budget.page_layout_bytes = wire;
        let prepared = prepare_document(
            &source,
            &directory.path().join("exact"),
            &PreparationOptions,
            &exact_budget,
        )
        .await?;
        let exact = extract_native(&prepared, &options, &exact_budget).await?;
        assert_eq!(exact.pages[0].metadata_artifact.bytes, wire);
        assert_eq!(page_metadata_bytes(&exact)?, bytes);
        exact.verify_integrity(&exact_budget)?;

        let mut short_budget = ResourceBudget::default();
        short_budget.owned_working_bytes /= 2;
        short_budget.page_layout_bytes = wire - 1;
        // Prove the earlier projection and its counting serializer fit the
        // same cap. The complete metadata serializer must be the failing stage.
        let projection = super::super::projection_admission(
            std::slice::from_ref(&metadata.page),
            &short_budget,
            0,
        )?;
        assert!(projection.text_upper <= short_budget.page_layout_bytes);
        crate::resource::encoded_size(&metadata.page.text_items, wire - 1)?;
        crate::resource::encoded_size(
            &(
                &metadata.complexity,
                &metadata.image_rects,
                &metadata.geometry,
            ),
            wire - 1,
        )?;
        assert!(baseline.pages[0].native_text.bytes < wire - 1);
        let prepared = prepare_document(
            &source,
            &directory.path().join("one-byte-short"),
            &PreparationOptions,
            &short_budget,
        )
        .await?;
        let error = extract_native(&prepared, &options, &short_budget)
            .await
            .expect_err("complete page metadata exceeds the wire cap by one byte");
        assert!(
            error
                .to_string()
                .contains("resource_limit: bounded writer admission failed"),
            "{error:#}"
        );
        // Projection fits, while complete page metadata refuses by one byte.
        // Both native page outputs now share pre-write admission.
        let native_text = &baseline.pages[0].native_text;
        assert!(
            !prepared
                .staging_root
                .join(&native_text.relative_path)
                .exists()
        );
        assert!(
            !prepared
                .staging_root
                .join(&baseline.pages[0].metadata_artifact.relative_path)
                .exists()
        );
        crate::rich_tests::assert_only_prepared_and_valid_document_metadata(
            &prepared,
            &short_budget,
            1,
        );
        Ok(())
    }

    #[tokio::test]
    async fn generated_repeated_font_page_uses_exact_metadata_wire_limit() -> Result<()> {
        generated_font_page_wire_boundary(false).await
    }

    #[tokio::test]
    async fn generated_mixed_font_page_uses_exact_metadata_wire_limit() -> Result<()> {
        generated_font_page_wire_boundary(true).await
    }

    #[test]
    fn prospective_font_working_reservation_is_atomic_at_exact_and_minus_one() -> Result<()> {
        // This is the parser-ledger half of the preallocation contract. The
        // vendor's font_length_refusal_never_calls_write_getter separately
        // observes that a rejected admission callback never reaches FFI fill.
        let retained = 4096;
        let previous_native = 24_576;
        let font_upper = (FONT_TEST_NAMES[0].len() as u64 + 1) * 24 + 256;
        let exact = retained + (previous_native + font_upper) * 4;
        let mut budget = ResourceBudget::default();
        budget.owned_working_bytes = exact;
        let mut admitted = NativeAdmission::new(retained, &budget)?;
        admitted.reserve_native(previous_native, &budget)?;
        admitted.reserve_native(font_upper, &budget)?;
        assert_eq!(admitted.held_upper()?, exact);
        assert_eq!(admitted.retained_owned(), retained);

        budget.owned_working_bytes = exact - 1;
        let mut rejected = NativeAdmission::new(retained, &budget)?;
        rejected.reserve_native(previous_native, &budget)?;
        let before = rejected.held_upper()?;
        let error = rejected.reserve_native(font_upper, &budget).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resource_limit: native retained plus active working admission"),
            "{error:#}"
        );
        assert_eq!(rejected.held_upper()?, before);
        assert_eq!(rejected.retained_owned(), retained);
        assert_eq!(rejected.metadata_bytes(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn generated_projection_estimate_can_exceed_exact_page_wire_limit() -> Result<()> {
        use crate::{NativeOptions, PreparationOptions, extract_native, prepare_document};
        const ROWS: usize = 12;
        const STAGING_BYTES: u64 = 8 * 1024 * 1024;
        let line = "A".repeat(100);
        // Ordinary authored text inside a square page. The small glyph advance
        // makes the conservative projection estimate exceed the actual output.
        let mut content = String::from("BT /F1 1.5 Tf 20 TL 20 960 Td\n");
        for row in 0..ROWS {
            if row > 0 {
                content.push_str("T*\n");
            }
            content.push_str(&format!("({line}) Tj\n"));
        }
        content.push_str("ET\n");
        let pdf = crate::tests::pdf_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1000 1000] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".into(),
        ]);
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("ordinary-small-print.pdf");
        assert!(pdf.len() <= 8192);
        std::fs::write(&source, pdf)?;
        let options = NativeOptions {
            render_pages: false,
            ..Default::default()
        };
        let mut baseline_budget = ResourceBudget::default();
        baseline_budget.owned_working_bytes /= 2;
        baseline_budget.document_staging_bytes = STAGING_BYTES;
        let prepared = prepare_document(
            &source,
            &directory.path().join("baseline"),
            &PreparationOptions,
            &baseline_budget,
        )
        .await?;
        let baseline = extract_native(&prepared, &options, &baseline_budget).await?;
        let bytes = page_metadata_bytes(&baseline)?;
        let metadata: crate::NativePageMetadata = serde_json::from_slice(&bytes)?;
        let wire = bytes.len() as u64;
        assert!(wire < baseline_budget.page_layout_bytes);
        assert_eq!(crate::resource::encoded_size(&metadata, wire)?, wire);
        let projection = super::super::projection_admission(
            std::slice::from_ref(&metadata.page),
            &baseline_budget,
            0,
        )?;
        assert!(
            projection.text_upper > baseline_budget.page_layout_bytes,
            "fixture must cross the old projection estimate cap: upper={}, cap={}",
            projection.text_upper,
            baseline_budget.page_layout_bytes
        );
        let stream = &metadata.native_text_metadata;
        assert_eq!(stream.characters.len(), stream.native_char_count as usize);
        let mut authored = String::new();
        for (ordinal, character) in stream.characters.iter().enumerate() {
            assert_eq!(character.ordinal as usize, ordinal);
            let value = char::from_u32(character.unicode).context("fixture Unicode scalar")?;
            if value.is_control() {
                continue;
            }
            assert_eq!(value, 'A');
            assert!(!character.generated);
            assert!(!character.has_unicode_map_error);
            assert_eq!(character.char_code, character.unicode);
            let FeatureState::Present(info) = &character.font else {
                panic!("authored character lost native font information");
            };
            assert_eq!(info.raw_name.as_slice(), b"Courier");
            assert!(matches!(&info.name, FeatureState::Present(name) if name == "Courier"));
            authored.push(value);
        }
        assert_eq!(authored, line.repeat(ROWS));
        let native_text = &baseline.pages[0].native_text;
        let text_bytes = crate::artifact::read_bounded(
            &baseline
                .prepared
                .staging_root
                .join(&native_text.relative_path),
            native_text.bytes,
            &native_text.sha256,
        )?;
        assert!(native_text.bytes < wire - 1);
        assert!(native_text.bytes < baseline_budget.page_layout_bytes);
        let text = std::str::from_utf8(&text_bytes)?;
        assert_eq!(text.matches(line.as_str()).count(), ROWS);
        assert_eq!(
            text.chars()
                .filter(|value| !value.is_whitespace())
                .collect::<String>(),
            line.repeat(ROWS)
        );
        baseline.verify_integrity(&baseline_budget)?;

        let mut exact_budget = ResourceBudget::default();
        exact_budget.owned_working_bytes /= 2;
        exact_budget.document_staging_bytes = STAGING_BYTES;
        exact_budget.page_layout_bytes = wire;
        let prepared = prepare_document(
            &source,
            &directory.path().join("exact"),
            &PreparationOptions,
            &exact_budget,
        )
        .await?;
        let exact = extract_native(&prepared, &options, &exact_budget).await?;
        assert_eq!(exact.pages[0].metadata_artifact.bytes, wire);
        assert_eq!(page_metadata_bytes(&exact)?, bytes);
        assert_eq!(exact.pages[0].native_text.bytes, native_text.bytes);
        assert_eq!(exact.pages[0].native_text.sha256, native_text.sha256);
        assert_eq!(
            crate::artifact::read_bounded(
                &exact.prepared.staging_root.join(&native_text.relative_path),
                native_text.bytes,
                &native_text.sha256,
            )?,
            text_bytes
        );
        exact.verify_integrity(&exact_budget)?;

        let mut short_budget = ResourceBudget::default();
        short_budget.owned_working_bytes /= 2;
        short_budget.document_staging_bytes = STAGING_BYTES;
        short_budget.page_layout_bytes = wire - 1;
        // Smaller counting serializations must pass before the complete
        // metadata serializer rejects and before either page output is written.
        super::super::projection_admission(std::slice::from_ref(&metadata.page), &short_budget, 0)?;
        crate::resource::encoded_size(&metadata.page.text_items, wire - 1)?;
        crate::resource::encoded_size(
            &(
                &metadata.complexity,
                &metadata.image_rects,
                &metadata.geometry,
            ),
            wire - 1,
        )?;
        let prepared = prepare_document(
            &source,
            &directory.path().join("one-byte-short"),
            &PreparationOptions,
            &short_budget,
        )
        .await?;
        let error = extract_native(&prepared, &options, &short_budget)
            .await
            .expect_err("complete page metadata exceeds the wire cap by one byte");
        assert!(
            error
                .to_string()
                .contains("resource_limit: bounded writer admission failed"),
            "{error:#}"
        );
        assert!(
            !prepared
                .staging_root
                .join(&native_text.relative_path)
                .exists()
        );
        assert!(
            !prepared
                .staging_root
                .join(&baseline.pages[0].metadata_artifact.relative_path)
                .exists()
        );
        crate::rich_tests::assert_only_prepared_and_valid_document_metadata(
            &prepared,
            &short_budget,
            1,
        );
        Ok(())
    }
}
