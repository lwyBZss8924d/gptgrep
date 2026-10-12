//! Count-admitted native stages. Opaque upstream rich getters remain explicit
//! capabilities until their length/count preflight can be implemented.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use gptgrep_pageindex::{Heading, PageInput, ParsedDocument, from_pages, locate_heading};
use liteparse::{
    stages,
    types::{Page, PdfInput, TextItem},
};
use serde::{Deserialize, Serialize};

use crate::artifact::{
    ArtifactBinding, ArtifactKind, ArtifactRef, FeatureState, artifact_id, hash_file, json_sha256,
    read_bounded, safe_path, sha256, verify_artifact, write_bytes, write_json,
};
use crate::geometry::{AffineTransform, BoundingBox, PageGeometry};
use crate::ocr::{
    OcrImage, OcrPageResult, OcrProfile, OcrRegion, OcrTask, OcrTaskBudget, ocr_task_digest,
    validate_ocr_result, validate_ocr_task,
};
use crate::resource::{LimitedWriter, ResourceBudget, encoded_size};

#[path = "native_memory.rs"]
mod native_memory;
#[path = "native_text_metadata.rs"]
pub(crate) mod native_text_metadata;

pub const RICH_PARSER: &str = "liteparse-2.15.1/rich-v1";
static PREPARATION_ID: AtomicU64 = AtomicU64::new(0);

const PNG_STREAM_CHUNK_BYTES: usize = 8192;
// The selected png 0.18.1 / flate2 1.1.10 / miniz_oxide 0.9.1
// backend uses fixed compressor arrays (under 320 KiB), including its
// nested boxed buffers. Keep separate slack for those arrays and constructor
// temporaries; the row and output buffers are charged explicitly below.
const PNG_COMPRESSOR_WORKING_UPPER: u64 = 4_194_304;
const PNG_DEFLATE_OUTPUT_BYTES: u64 = 32_768;

fn png_streaming_working_upper(width: u32) -> Result<u64> {
    // StreamWriter retains three RGBA rows. Charge four rows, its explicitly
    // selected chunk buffer, and flate2's fixed output buffer before render.
    u64::from(width)
        .checked_mul(4)
        .and_then(|row| row.checked_mul(4))
        .and_then(|rows| rows.checked_add(PNG_STREAM_CHUNK_BYTES as u64))
        .and_then(|bytes| bytes.checked_add(PNG_DEFLATE_OUTPUT_BYTES))
        .and_then(|bytes| bytes.checked_add(PNG_COMPRESSOR_WORKING_UPPER))
        .context("resource_limit: PNG encoder working admission overflow")
}

fn admit_raster_working(
    held_upper: u64,
    width: u32,
    pixels: u64,
    budget: &ResourceBudget,
) -> Result<()> {
    let scratch = png_streaming_working_upper(width)?;
    let raster_owned = pixels
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(budget.image_file_bytes))
        .and_then(|bytes| bytes.checked_add(scratch))
        .context("resource_limit: raster/PNG working admission overflow")?;
    ensure!(
        held_upper
            .checked_add(raster_owned)
            .is_some_and(|bytes| bytes <= budget.owned_working_bytes),
        "resource_limit: raster+metadata+PNG working admission"
    );
    Ok(())
}

fn encode_raster_png(rgba: &[u8], width: u32, height: u32, limit: u64) -> Result<Vec<u8>> {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .context("resource_limit: PNG input geometry overflow")?;
    ensure!(
        width > 0 && height > 0 && expected == rgba.len() as u64,
        "invalid PNG input geometry"
    );
    let mut output = LimitedWriter {
        inner: Vec::with_capacity(
            usize::try_from(limit).context("image file limit is not addressable")?,
        ),
        limit,
        written: 0,
    };
    let result = (|| -> Result<()> {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::High);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder.write_header()?;
        {
            let mut stream = writer.stream_writer_with_size(PNG_STREAM_CHUNK_BYTES)?;
            stream.write_all(rgba)?;
            stream.finish()?;
        }
        // Validate final chunks and propagate IEND byte-limit errors too.
        writer.finish()?;
        Ok(())
    })();
    result.with_context(|| {
        format!(
            "PNG rendition encoding failed: width={width}, height={height}, limit_bytes={limit}, emitted_prefix_bytes={}",
            output.written
        )
    })?;
    Ok(output.inner)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionIdentity {
    pub converter: String,
    pub version: String,
    pub invocations: u32,
}

/// Raster source coordinates and normalized page coordinates both start at the
/// top left. One source pixel occupies one PDF point; metadata never rotates or
/// rescales pixels implicitly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedImageGeometry {
    pub source_width_pixels: u32,
    pub source_height_pixels: u32,
    pub page_width_points: f64,
    pub page_height_points: f64,
    pub source_pixels_to_page: AffineTransform,
    pub source_orientation: FeatureState<String>,
    pub source_color_profile: FeatureState<String>,
    pub alpha_background: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSummary {
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub source_bytes: u64,
    pub prepared_bytes: u64,
    pub format: String,
    pub parser_identity: String,
    pub preparation_profile_sha256: String,
    pub conversion: FeatureState<ConversionIdentity>,
    #[serde(default)]
    pub image_geometry: FeatureState<PreparedImageGeometry>,
}

/// Owns only immutable local preparation paths. Paths are excluded from public
/// serializable summaries; this object never owns a PDFium handle.
#[derive(Clone, Debug)]
pub struct PreparedDocument {
    pub(crate) summary: PreparedSummary,
    pub(crate) source_path: PathBuf,
    pub(crate) prepared_path: PathBuf,
    pub(crate) staging_root: PathBuf,
    pub(crate) invocation_budget: ResourceBudget,
}
impl PreparedDocument {
    pub fn summary(&self) -> &PreparedSummary {
        &self.summary
    }
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }
    pub fn prepared_path(&self) -> &Path {
        &self.prepared_path
    }
    pub fn verify(&self, budget: &ResourceBudget) -> Result<()> {
        budget.ensure_same_invocation_within(&self.invocation_budget)?;
        ensure!(
            self.staging_root.canonicalize()? == self.staging_root,
            "prepared staging path changed or became a symlink"
        );
        let (bytes, digest) = hash_file(&self.prepared_path, budget.document_staging_bytes)?;
        ensure!(
            bytes == self.summary.prepared_bytes && digest == self.summary.prepared_sha256,
            "prepared document changed"
        );
        let (bytes, digest) = hash_file(&self.source_path, budget.document_staging_bytes)?;
        ensure!(
            bytes == self.summary.source_bytes && digest == self.summary.source_sha256,
            "original source changed during preparation"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct PreparationOptions;

#[derive(Clone, Debug)]
pub struct NativeOptions {
    pub page_window: usize,
    pub dpi: f32,
    pub render_pages: bool,
}
impl Default for NativeOptions {
    fn default() -> Self {
        Self {
            page_window: 25,
            dpi: 96.0,
            render_pages: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrMode {
    Off,
    Auto,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Native,
    BlankVerified,
    RequiredOcr,
    ValidatedOcr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePage {
    pub number: u32,
    pub label: FeatureState<String>,
    pub width_points: f64,
    pub height_points: f64,
    pub geometry: PageGeometry,
    pub native_text: ArtifactRef,
    pub metadata_artifact: ArtifactRef,
    pub renditions: Vec<OcrImage>,
    pub complexity: stages::PageComplexityStats,
    pub coverage: CoverageState,
    pub capabilities: std::collections::BTreeMap<String, FeatureState<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrPlan {
    pub tasks: Vec<OcrTask>,
    pub pages: Vec<(u32, CoverageState)>,
    pub blockers: Vec<String>,
}

/// An immutable extraction candidate. Callers can inspect its plan or clone
/// individual task DTOs, but only parser finalization can change candidate data.
///
/// ```compile_fail
/// use gptgrep_parse::{NativeCandidate, CoverageState};
/// fn forge(mut candidate: NativeCandidate) {
///     candidate.summary.source_sha256 = "0".repeat(64);
///     candidate.pages[0].coverage = CoverageState::Native;
///     candidate.artifacts.clear();
///     candidate.ocr_plan.tasks.clear();
///     candidate.native_profile_sha256 = "1".repeat(64);
/// }
/// ```
#[derive(Debug)]
pub struct NativeCandidate {
    pub(crate) summary: PreparedSummary,
    pub(crate) pages: Vec<NativePage>,
    pub(crate) artifacts: Vec<ArtifactRef>,
    pub(crate) ocr_plan: OcrPlan,
    pub(crate) native_profile_sha256: String,
    pub(crate) prepared: PreparedDocument,
    pub(crate) raw_pages: Vec<Page>,
    pub(crate) document_staging_used: u64,
    pub(crate) native_owned_upper: u64,
    pub(crate) metadata_admitted_upper: u64,
    pub(crate) execution_budget: ResourceBudget,
    pub(crate) frozen_integrity_sha256: String,
    pub(crate) annotation_counts: Vec<u32>,
    pub(crate) document_metadata: Option<crate::NativeDocumentMetadata>,
    pub(crate) document_metadata_artifact: Option<ArtifactRef>,
}

impl NativeCandidate {
    pub fn summary(&self) -> &PreparedSummary {
        &self.summary
    }
    pub fn pages(&self) -> &[NativePage] {
        &self.pages
    }
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }
    pub fn plan(&self) -> &OcrPlan {
        &self.ocr_plan
    }
    pub fn native_profile_sha256(&self) -> &str {
        &self.native_profile_sha256
    }
    pub fn document_metadata(&self) -> Option<&crate::NativeDocumentMetadata> {
        self.document_metadata.as_ref()
    }

    pub(crate) fn integrity_digest(&self) -> Result<String> {
        json_sha256(
            &(
                "gptgrep.native-candidate.private.v1",
                &self.prepared.summary,
                &self.prepared.source_path,
                &self.prepared.prepared_path,
                &self.prepared.staging_root,
                &self.summary,
                &self.pages,
                &self.artifacts,
                &self.ocr_plan,
                &self.native_profile_sha256,
                &self.raw_pages,
                self.document_staging_used,
                self.native_owned_upper,
                self.metadata_admitted_upper,
                &self.execution_budget,
                &(
                    &self.annotation_counts,
                    &self.document_metadata,
                    &self.document_metadata_artifact,
                ),
            ),
            self.execution_budget.owned_working_bytes,
        )
    }

    fn verify_integrity(&self, budget: &ResourceBudget) -> Result<()> {
        budget.ensure_same_invocation_within(&self.execution_budget)?;
        self.prepared.verify(budget)?;
        ensure!(
            self.frozen_integrity_sha256 == self.integrity_digest()?,
            "native candidate frozen integrity mismatch"
        );
        ensure!(
            json_sha256(&self.summary, 4096)? == json_sha256(&self.prepared.summary, 4096)?,
            "native candidate summary does not match immutable preparation"
        );
        if self.summary.format == "plaintext" {
            return crate::rich_text::verify_candidate(self, budget);
        }
        ensure!(
            !self.pages.is_empty()
                && self.pages.len() == self.raw_pages.len()
                && self.pages.len() == self.annotation_counts.len(),
            "native candidate page coverage mismatch"
        );
        ensure!(
            self.ocr_plan.pages
                == self
                    .pages
                    .iter()
                    .map(|page| (page.number, page.coverage.clone()))
                    .collect::<Vec<_>>(),
            "native candidate OCR coverage plan mismatch"
        );
        let mut registered = BTreeMap::new();
        for artifact in &self.artifacts {
            ensure!(
                artifact.profile_sha256 == self.native_profile_sha256
                    && artifact.id
                        == artifact_id(
                            &self.prepared.summary.source_sha256,
                            &self.native_profile_sha256,
                            &artifact.kind,
                            artifact.page,
                            artifact.region_id.as_deref(),
                            &artifact.sha256
                        )?,
                "native candidate artifact binding mismatch"
            );
            ensure!(
                registered.insert(&artifact.id, artifact).is_none(),
                "duplicate native candidate artifact ID"
            );
            verify_artifact(&self.prepared.staging_root, artifact)?;
        }
        let mut expected_artifacts = BTreeSet::new();
        let document_metadata = self
            .document_metadata
            .as_ref()
            .context("native document metadata missing")?;
        crate::validate_native_document_metadata(
            document_metadata,
            &self.summary,
            self.pages.len() as u32,
            budget,
        )?;
        ensure!(
            self.native_profile_sha256
                == crate::native_graphics_profile_digest(
                    &self.summary,
                    document_metadata.native_dpi,
                    document_metadata.native_render_pages
                )?,
            "native font/word profile mismatch"
        );
        let document_artifact = self
            .document_metadata_artifact
            .as_ref()
            .context("native document metadata artifact missing")?;
        ensure!(
            document_artifact.kind == ArtifactKind::NativeMetadata
                && document_artifact.page.is_none()
                && json_sha256(document_metadata, budget.document_metadata_bytes)?
                    == document_artifact.sha256,
            "native document metadata artifact/content mismatch"
        );
        verify_registered_artifact(document_artifact, &registered, &mut expected_artifacts)?;
        let mut required_tasks = BTreeSet::new();
        let mut expected_blockers = Vec::new();
        for (index, (page, raw)) in self.pages.iter().zip(&self.raw_pages).enumerate() {
            ensure!(
                serde_json::to_value(&page.label)?
                    == serde_json::to_value(&document_metadata.page_labels[index].label)?
                    && raw.page_label.as_deref()
                        == match &page.label {
                            FeatureState::Present(label) => Some(label.as_str()),
                            _ => None,
                        },
                "native document/page label binding mismatch"
            );
            ensure!(
                page.number as usize == index + 1
                    && raw.page_number == index + 1
                    && page.width_points == f64::from(raw.page_width)
                    && page.height_points == f64::from(raw.page_height)
                    && page.complexity.page_number == raw.page_number,
                "native candidate page/native geometry mismatch"
            );
            let annotation_count = self.annotation_counts[index];
            ensure!(
                page.coverage != CoverageState::ValidatedOcr
                    && (page.coverage == CoverageState::BlankVerified
                        || (page.coverage == CoverageState::RequiredOcr)
                            == (page.complexity.needs_ocr || annotation_count > 0)),
                "native candidate native OCR coverage mismatch"
            );
            ensure!(
                page.coverage != CoverageState::BlankVerified
                    || (raw.text_items.is_empty() && annotation_count == 0),
                "native candidate blank coverage has native text"
            );
            for (artifact, kind) in [
                (&page.native_text, ArtifactKind::NativeText),
                (&page.metadata_artifact, ArtifactKind::NativeMetadata),
            ] {
                ensure!(
                    artifact.kind == kind && artifact.page == Some(page.number),
                    "native candidate page artifact kind/page mismatch"
                );
                verify_registered_artifact(artifact, &registered, &mut expected_artifacts)?;
            }
            ensure!(
                matches!(page.capabilities.get("native_recipe"),
                Some(FeatureState::Present(recipe)) if recipe == crate::NATIVE_GRAPHICS_RECIPE),
                "native font/word recipe missing"
            );
            ensure!(
                self.native_owned_upper
                    .checked_add(
                        page.metadata_artifact
                            .bytes
                            .checked_mul(64)
                            .context("resource_limit: native font/word validation overflow")?
                    )
                    .is_some_and(|n| n <= budget.owned_working_bytes),
                "resource_limit: held native candidate plus font/word validation"
            );
            let bytes = read_bounded(
                &self
                    .prepared
                    .staging_root
                    .join(&page.metadata_artifact.relative_path),
                page.metadata_artifact.bytes,
                &page.metadata_artifact.sha256,
            )?;
            ensure!(
                self.native_owned_upper
                    .checked_add(crate::native_metadata_working_upper(&bytes)?)
                    .is_some_and(|n| n <= budget.owned_working_bytes),
                "resource_limit: retained native candidate and typed graphics validation"
            );
            // The strict v7 validator below also validates the complete native
            // character/font/word stream under its bound unclipped policy.
            let retained = crate::validate_native_graphics_metadata(
                &bytes,
                &self.summary,
                &self.native_profile_sha256,
                page.number,
                budget,
            )?;
            ensure!(
                serde_json::to_vec(&retained.page)? == serde_json::to_vec(raw)?
                    && serde_json::to_value(&retained.capabilities)?
                        == serde_json::to_value(&page.capabilities)?,
                "native font/word retained candidate mismatch"
            );
            for image in &page.renditions {
                ensure!(
                    image.artifact.kind == ArtifactKind::Rendition
                        && image.artifact.page == Some(page.number)
                        && image.path
                            == self
                                .prepared
                                .staging_root
                                .join(&image.artifact.relative_path),
                    "native candidate rendition path/page mismatch"
                );
                verify_registered_artifact(&image.artifact, &registered, &mut expected_artifacts)?;
            }
            if page.coverage == CoverageState::RequiredOcr {
                if page.renditions.is_empty() {
                    expected_blockers.push(format!(
                        "page {} requires OCR but no admitted rendition was requested",
                        page.number
                    ));
                } else {
                    required_tasks.insert(page.number);
                }
            }
        }
        ensure!(
            expected_artifacts.len() == registered.len(),
            "native candidate contains unassigned artifacts"
        );
        ensure!(
            expected_blockers == self.ocr_plan.blockers,
            "native candidate OCR blockers mismatch"
        );
        let mut tasks = BTreeSet::new();
        for task in &self.ocr_plan.tasks {
            ensure!(
                tasks.insert(task.page) && required_tasks.contains(&task.page),
                "native candidate unexpected/duplicate OCR task"
            );
            let page = &self.pages[task.page as usize - 1];
            ensure!(
                task.source_sha256 == self.prepared.summary.source_sha256
                    && task.prepared_sha256 == self.prepared.summary.prepared_sha256
                    && task.source_path == self.prepared.source_path
                    && task.page_width_points == page.width_points
                    && task.page_height_points == page.height_points
                    && json_sha256(&task.images, budget.page_layout_bytes)?
                        == json_sha256(&page.renditions, budget.page_layout_bytes)?,
                "native candidate OCR source/page/rendition mismatch"
            );
            task.budget
                .resources
                .ensure_same_invocation_within(&self.execution_budget)?;
            validate_ocr_task(task)?;
            ensure!(
                task.native_text.as_bytes()
                    == read_bounded(
                        &self
                            .prepared
                            .staging_root
                            .join(&page.native_text.relative_path),
                        page.native_text.bytes,
                        &page.native_text.sha256
                    )?,
                "native candidate OCR/native text mismatch"
            );
        }
        ensure!(
            tasks == required_tasks,
            "native candidate missing required OCR task"
        );
        Ok(())
    }
}

fn verify_registered_artifact<'a>(
    artifact: &ArtifactRef,
    registered: &BTreeMap<&'a String, &ArtifactRef>,
    expected: &mut BTreeSet<&'a String>,
) -> Result<()> {
    let (id, canonical) = registered
        .get_key_value(&artifact.id)
        .context("native candidate artifact missing from registry")?;
    ensure!(
        json_sha256(artifact, 4096)? == json_sha256(canonical, 4096)?,
        "native candidate artifact reference mismatch"
    );
    ensure!(
        expected.insert(id),
        "native candidate artifact assigned more than once"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentMap {
    pub page: u32,
    pub merged_start: u64,
    pub merged_end: u64,
    pub artifact_id: String,
    pub artifact_start: u64,
    pub artifact_end: u64,
    pub kind: ArtifactKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichParsedDocument {
    pub summary: PreparedSummary,
    pub document: ParsedDocument,
    pub pages: Vec<NativePage>,
    pub artifacts: Vec<ArtifactRef>,
    pub segments: Vec<SegmentMap>,
    pub ocr_results: Vec<OcrPageResult>,
    pub ocr_tasks: Vec<OcrTask>,
    pub native_profile_sha256: String,
    pub coverage: Vec<(u32, CoverageState)>,
    pub unmapped_merged_lines: usize,
}

pub async fn prepare_document(
    source: &Path,
    staging_root: &Path,
    _options: &PreparationOptions,
    budget: &ResourceBudget,
) -> Result<PreparedDocument> {
    let source = source.to_path_buf();
    let staging_root = staging_root.to_path_buf();
    let budget = budget.clone();
    tokio::task::spawn_blocking(move || prepare_sync(&source, &staging_root, &budget))
        .await
        .context("rich preparation worker failed")?
}

fn prepare_sync(
    source: &Path,
    staging_root: &Path,
    budget: &ResourceBudget,
) -> Result<PreparedDocument> {
    budget.validate()?;
    let extension = source
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_text = crate::is_plaintext(source);
    let is_office = crate::office::is_office_extension(&extension);
    ensure!(
        is_text || is_office || matches!(extension.as_str(), "pdf" | "png" | "jpeg" | "jpg"),
        "capability_unavailable: rich preparation supports plaintext, PDF, PNG, baseline JPEG and installed LibreOffice formats"
    );
    ensure!(
        fs::symlink_metadata(source)?.is_file()
            && !fs::symlink_metadata(source)?.file_type().is_symlink(),
        "prepared source must be a nonsymlink regular file"
    );
    let source_path = source.canonicalize()?;
    let is_pdf = extension == "pdf";
    let source_limit = if is_pdf || is_text || is_office {
        budget.document_staging_bytes
    } else {
        budget.image_file_bytes
    };
    let (source_bytes, source_sha256) = hash_file(&source_path, source_limit)?;
    let image = if is_pdf || is_text || is_office {
        budget.reserve_staging(source_bytes)?;
        None
    } else {
        Some(crate::image_prepare::ImagePreparation::admit(
            &source_path,
            source_bytes,
            &source_sha256,
            if extension == "png" {
                image::ImageFormat::Png
            } else {
                image::ImageFormat::Jpeg
            },
            budget,
        )?)
    };
    fs::create_dir_all(staging_root)?;
    ensure!(
        !fs::symlink_metadata(staging_root)?.file_type().is_symlink(),
        "staging root must not be a symlink"
    );
    let staging_root = staging_root.canonicalize()?.join(format!(
        "prepared-{}-{}",
        std::process::id(),
        PREPARATION_ID.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir(&staging_root)?;
    let prepared_path = staging_root.join(if is_text {
        "prepared.txt"
    } else {
        "prepared.pdf"
    });
    let (
        prepared_bytes,
        prepared_sha256,
        format,
        preparation_profile_sha256,
        conversion,
        image_geometry,
    ) = if is_office {
        let output = crate::office::prepare_once(
            &source_path,
            &staging_root,
            &prepared_path,
            &extension,
            (source_bytes, &source_sha256),
            budget,
        )?;
        (
            output.0,
            output.1,
            extension.as_str(),
            crate::office::office_preparation_profile_digest(&extension)?,
            FeatureState::Present(ConversionIdentity {
                converter: "gptgrep-bounded-libreoffice-pdf".into(),
                version: crate::office::OFFICE_ADAPTER.into(),
                invocations: 1,
            }),
            FeatureState::Absent,
        )
    } else if let Some(image) = image {
        let output = image.write(&prepared_path, budget)?;
        (
            output.bytes,
            output.sha256,
            if extension == "png" { "png" } else { "jpeg" },
            output.profile_sha256,
            FeatureState::Present(output.conversion),
            FeatureState::Present(output.geometry),
        )
    } else {
        let mut input = fs::File::open(&source_path)?;
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&prepared_path)?;
        let mut output = LimitedWriter {
            inner: output,
            limit: source_bytes,
            written: 0,
        };
        let mut buffer = [0u8; 65_536];
        loop {
            let bytes = input.read(&mut buffer)?;
            if bytes == 0 {
                break;
            }
            output.write_all(&buffer[..bytes])?;
        }
        output.inner.sync_all()?;
        let (prepared_bytes, prepared_sha256) = hash_file(&prepared_path, source_bytes)?;
        ensure!(
            source_bytes == prepared_bytes && source_sha256 == prepared_sha256,
            "source changed while copying immutable preparation"
        );
        (
            prepared_bytes,
            prepared_sha256,
            if is_text { "plaintext" } else { "pdf" },
            if is_text {
                crate::rich_text::plaintext_preparation_profile_digest()?
            } else {
                sha256(&serde_json::to_vec(&(
                    "gptgrep.prepare.identity-pdf.v1",
                    crate::LITEPARSE_REVISION,
                ))?)
            },
            FeatureState::Present(ConversionIdentity {
                converter: if is_text {
                    "identity-utf8"
                } else {
                    "identity-pdf"
                }
                .into(),
                version: crate::LITEPARSE_REVISION.into(),
                invocations: 0,
            }),
            FeatureState::Absent,
        )
    };
    let document = PreparedDocument {
        summary: PreparedSummary {
            source_sha256,
            prepared_sha256,
            source_bytes,
            prepared_bytes,
            format: format.into(),
            parser_identity: if is_text { "plaintext" } else { RICH_PARSER }.into(),
            preparation_profile_sha256,
            conversion,
            image_geometry,
        },
        source_path,
        prepared_path,
        staging_root,
        invocation_budget: budget.clone(),
    };
    document.verify(budget)?;
    Ok(document)
}

pub async fn extract_native(
    prepared: &PreparedDocument,
    options: &NativeOptions,
    budget: &ResourceBudget,
) -> Result<NativeCandidate> {
    ensure!(
        options.page_window > 0 && options.page_window <= 25,
        "native page window must be 1..=25"
    );
    ensure!(
        options.dpi.is_finite() && options.dpi > 0.0 && options.dpi <= 300.0,
        "invalid native DPI"
    );
    let prepared = prepared.clone();
    let options = options.clone();
    let budget = budget.clone();
    tokio::task::spawn_blocking(move || extract_sync(prepared, options, budget))
        .await
        .context("rich PDFium worker failed")?
}

pub(crate) fn admit_staging(used: &mut u64, bytes: u64, budget: &ResourceBudget) -> Result<()> {
    let next = used
        .checked_add(bytes)
        .context("resource_limit: staging overflow")?;
    ensure!(
        next <= budget.document_staging_bytes,
        "resource_limit: document staging admission"
    );
    budget.reserve_staging(bytes)?;
    *used = next;
    Ok(())
}

pub(crate) fn extract_sync(
    prepared: PreparedDocument,
    options: NativeOptions,
    budget: ResourceBudget,
) -> Result<NativeCandidate> {
    prepared.verify(&budget)?;
    if prepared.summary.format == "plaintext" {
        return crate::rich_text::extract(prepared, budget);
    }
    let native_profile_sha256 = crate::native_graphics_profile_digest(
        &prepared.summary,
        options.dpi,
        options.render_pages,
    )?;
    let mut raw_pages = Vec::new();
    let mut pages = Vec::new();
    let mut artifacts = Vec::new();
    let mut tasks = Vec::new();
    let mut annotation_counts = Vec::new();
    let mut coverage_plan = Vec::new();
    let mut blockers = Vec::new();
    let mut staged = prepared.summary.prepared_bytes;
    let mut admission = native_memory::NativeAdmission::new(
        native_memory::fixed_owned(&prepared, &native_profile_sha256)?,
        &budget,
    )?;
    let input = PdfInput::Path(
        prepared
            .prepared_path
            .to_str()
            .context("prepared path is not UTF-8")?
            .into(),
    );
    let (total, document_metadata) = {
        let lib = stages::Library::init();
        let doc = stages::open(&lib, &input, None, &[])?;
        ensure!(
            doc.xfa_packet_count() == 0 && doc.form_type() == 0,
            "capability_unavailable: XFA/AcroForm preparation needs a bounded packet/appearance adapter"
        );
        let total = doc.page_count();
        ensure!(total > 0, "native document contains no pages");
        let metadata = crate::document_metadata::extract(
            &doc,
            &prepared.summary,
            total as u32,
            &options,
            &budget,
            &mut |bytes| admission.reserve_native(bytes, &budget),
        )?;
        (total, metadata)
    };
    ensure!(total > 0, "native document contains no pages");
    let document_metadata_size = encoded_size(&document_metadata, budget.document_metadata_bytes)?;
    admission.admit_metadata(document_metadata_size, &budget)?;
    admit_staging(&mut staged, document_metadata_size, &budget)?;
    let document_metadata_artifact = write_json(
        &prepared.staging_root,
        &format!("{}-document-native.json", prepared.summary.source_sha256),
        ArtifactBinding {
            source: &prepared.summary.source_sha256,
            profile: &native_profile_sha256,
            kind: ArtifactKind::NativeMetadata,
            page: None,
        },
        &document_metadata,
        budget.document_metadata_bytes,
    )?;
    artifacts.push(document_metadata_artifact.clone());
    // Every outer candidate vector has a known page-count ceiling. Reserve its
    // storage once, while document extraction's temporary allocation allowance
    // is still active, so a later page cannot trigger uncharged growth overlap.
    let page_slots = total as usize;
    // Two native artifacts, optionally one raster and two OCR artifacts, plus
    // document metadata and the final layout/merged-text pair. Reserving final
    // slots here also prevents OCR from reallocating the whole artifact vector.
    let artifact_slots = page_slots
        .checked_mul(if options.render_pages { 5 } else { 2 })
        .and_then(|n| n.checked_add(3))
        .context("resource_limit: native artifact slot overflow")?;
    let slot_reservation = native_memory::candidate_slot_reservation(page_slots, artifact_slots)?;
    admission.reserve_owned(slot_reservation, &budget)?;
    raw_pages.try_reserve_exact(page_slots)?;
    pages.try_reserve_exact(page_slots)?;
    tasks.try_reserve_exact(page_slots)?;
    annotation_counts.try_reserve_exact(page_slots)?;
    coverage_plan.try_reserve_exact(page_slots)?;
    blockers.try_reserve_exact(page_slots)?;
    artifacts.try_reserve_exact(artifact_slots - artifacts.len())?;
    let slots_owned = native_memory::candidate_slots_owned(
        &raw_pages,
        &pages,
        &tasks,
        &annotation_counts,
        &coverage_plan,
        &blockers,
        &artifacts,
    )?;
    ensure!(
        slots_owned <= slot_reservation,
        "resource_limit: native vector capacity admission"
    );
    let document_owned = native_memory::document_owned(&document_metadata)?
        .checked_add(
            native_memory::artifact_owned(&document_metadata_artifact)?
                .checked_mul(2)
                .context("resource_limit: native document artifact copies overflow")?,
        )
        .and_then(|n| n.checked_add(slots_owned))
        .context("resource_limit: retained native document overflow")?;
    // document_metadata::extract has returned; its outline/getter temporaries
    // and PDFium document scope have ended. Only these retained products remain.
    admission.finish_phase(document_owned, &budget)?;
    for start in (0..total as usize).step_by(options.page_window) {
        // This scope ends before the async extraction function returns. No
        // Document, Page, TextPage, Bitmap or Library appears in a public DTO.
        let lib = stages::Library::init();
        let doc = stages::open(&lib, &input, None, &[])?;
        ensure!(doc.page_count() == total, "prepared page count changed");
        for index in start..(start + options.page_window).min(total as usize) {
            let first_artifact = artifacts.len();
            let first_task = tasks.len();
            let number = index as u32 + 1;
            let page_obj = doc.page(index as i32)?;
            let annotation_count = page_obj.annotation_count();
            ensure!(
                annotation_count >= 0,
                "capability_unavailable: PDFium annotation count failed"
            );
            let annotation_bound = u64::from(annotation_count as u32) * 2048;
            ensure!(
                annotation_bound <= budget.page_layout_bytes,
                "resource_limit: annotation count admission"
            );
            annotation_counts.push(annotation_count as u32);
            let view = page_obj
                .view_box()
                .context("capability_unavailable: missing PDF view box")?;
            let (width, height) = page_obj.viewport_size(&view);
            ensure!(
                width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
                "invalid PDF page geometry"
            );
            let native_transform = page_obj.viewport_transform(&view);
            let (e, f) = native_transform.transform_point(0.0, 0.0);
            let (x, y) = native_transform.transform_point(1.0, 0.0);
            let (u, v) = native_transform.transform_point(0.0, 1.0);
            let transform = AffineTransform {
                a: f64::from(x - e),
                b: f64::from(y - f),
                c: f64::from(u - e),
                d: f64::from(v - f),
                e: f64::from(e),
                f: f64::from(f),
            };
            let geometry = PageGeometry {
                width_points: width.into(),
                height_points: height.into(),
                rotation_quarter_turns: page_obj.rotation(),
                user_unit: page_obj.user_unit().into(),
                source_to_page: transform,
                page_to_source: transform.inverse()?,
            };
            let text_page = page_obj.text()?;
            let count = text_page.char_count();
            ensure!(count >= 0, "PDFium char count failed");
            // Reserve every native character/run/word before allocation.
            // This conservative estimate belongs to the active page. Retained
            // candidate storage and actual serialized metadata have other ledgers.
            let glyph_bound =
                native_text_metadata::character_allocation_bound(count as u32, &budget)?;
            let label_bound = match &document_metadata.page_labels[index].label {
                FeatureState::Present(label) => label.len() as u64 * 12,
                _ => 1024,
            };
            let base_bound = annotation_bound + label_bound + 16_384;
            ensure!(
                base_bound < budget.page_layout_bytes,
                "resource_limit: page character admission"
            );
            admission.reserve_native(glyph_bound + base_bound, &budget)?;
            let graphics = crate::native_graphics::collect(
                &prepared.summary,
                &native_profile_sha256,
                number,
                &geometry,
                budget.page_layout_bytes - base_bound,
                |ordinal| page_obj.object(ordinal),
                |parent| match parent {
                    None => page_obj.object_count_checked(),
                    Some(object) => object.form_object_count(),
                },
                |object, ordinal| object.form_object(ordinal),
                |object| {
                    use crate::native_graphics::{ImageObservation, ObjectObservation};
                    let raw_kind = object.raw_kind();
                    let is_path = raw_kind == 2;
                    ObjectObservation {
                        identity: object.id(),
                        raw_kind,
                        matrix: object.matrix().map(|m| [m.a, m.b, m.c, m.d, m.e, m.f]),
                        bounds: object.bounds().map(|r| [r.left, r.bottom, r.right, r.top]),
                        segments: if is_path {
                            object.path_segment_count()
                        } else {
                            None
                        },
                        draw: if is_path {
                            object.path_raw_draw_mode()
                        } else {
                            None
                        },
                        stroke_width: if is_path { object.stroke_width() } else { None },
                        fill: if is_path {
                            object.fill_color().map(|c| [c.r, c.g, c.b, c.a])
                        } else {
                            None
                        },
                        stroke: if is_path {
                            object.stroke_color().map(|c| [c.r, c.g, c.b, c.a])
                        } else {
                            None
                        },
                        image: if raw_kind == 3 {
                            object.image_metadata().map(|m| ImageObservation {
                                width: m.width,
                                height: m.height,
                                bits: m.bits_per_pixel,
                                colorspace: m.colorspace,
                                marked: m.marked_content_id,
                                horizontal_dpi: m.horizontal_dpi,
                                vertical_dpi: m.vertical_dpi,
                            })
                        } else {
                            None
                        },
                    }
                },
                |object, ordinal| {
                    object.path_segment(ordinal).map(|segment| {
                        use crate::native_graphics::{NativeSegmentKind, SegmentObservation};
                        SegmentObservation {
                            kind: match segment.kind_label() {
                                Some("move_to") => Some(NativeSegmentKind::MoveTo),
                                Some("line_to") => Some(NativeSegmentKind::LineTo),
                                Some("bezier_to") => Some(NativeSegmentKind::BezierTo),
                                _ => None,
                            },
                            point: segment.point.map(|(x, y)| [x, y]),
                            close: segment.close,
                        }
                    })
                },
                |bytes| admission.reserve_native(bytes, &budget),
            )?;
            let object_count = graphics.header.top_level_count;
            let object_bound = graphics.charged_bytes;
            let vector_count = graphics.paths;
            let image_rects = graphics.image_rects;
            let characters = native_text_metadata::collect_characters(
                count as u32,
                width,
                height,
                &budget,
                native_text_metadata::NativeBoundsPolicy::UnclippedV7,
                |ordinal| {
                    let glyph = text_page.char_at_unchecked(ordinal as i32);
                    let bounds = glyph.char_box().map(|box_| {
                        let (x1, y1) =
                            native_transform.transform_point(box_.left as f32, box_.top as f32);
                        let (x2, y2) =
                            native_transform.transform_point(box_.right as f32, box_.bottom as f32);
                        native_text_metadata::NativeBounds {
                            x: x1.min(x2),
                            y: y1.min(y2),
                            width: (x2 - x1).abs(),
                            height: (y2 - y1).abs(),
                        }
                    });
                    // Every admitted character, including geometryless spaces
                    // and actual/generated controls, reaches the same retained
                    // source stream. The font estimate includes active copies
                    // and worst-case JSON. Reserve its working allowance before
                    // the getter allocates; the bounded metadata writer measures
                    // final page bytes against the separate wire cap.
                    let font_info = glyph
                        .font_info_admitted(budget.native_layout_input_bytes, &mut |bytes| {
                            admission.reserve_native(bytes, &budget)
                        })?;
                    Ok(native_text_metadata::NativeCharacterObservation {
                        unicode: glyph.unicode(),
                        char_code: glyph.char_code(),
                        has_unicode_map_error: glyph.has_unicode_map_error(),
                        generated: glyph.is_generated(),
                        bounds,
                        font_size: glyph.font_size(),
                        font_weight: glyph.font_weight(),
                        font: native_text_metadata::font_state(font_info),
                    })
                },
            )?;
            let mut raw = Page {
                page_number: number as usize,
                page_label: match &document_metadata.page_labels[index].label {
                    FeatureState::Present(label) => Some(label.clone()),
                    _ => None,
                },
                page_width: width,
                page_height: height,
                content_bounds: None,
                text_items: native_text_metadata::unclipped_glyph_runs(&characters)?,
                graphics: Vec::new(),
                vector_graphics: None,
                struct_nodes: Vec::new(),
                image_refs: Vec::new(),
                annotations: None,
                form_fields: None,
                structure_tree: None,
            };
            // Avoid upstream image_bounds()/vector Vec getters. Inputs to this
            // pure classifier were admitted by the object-count walk above.
            // Preserve the pre-G3 classifier inputs; the layout derivative is
            // assigned only after that original source classification.
            let mut complexity =
                liteparse::ocr_merge::calculate_native_page_complexity(&raw, &image_rects, 0, 1);
            let (content_bounds, vector_graphics) =
                crate::native_graphics::derive_layout(&graphics.header, &graphics.objects)?;
            raw.content_bounds = content_bounds;
            raw.vector_graphics = Some(vector_graphics);
            let held_before_projection = admission.held_upper()?;
            let projection =
                projection_admission(std::slice::from_ref(&raw), &budget, held_before_projection)?;
            // Projected output remains live while native metadata is cloned,
            // rasterized and turned into OCR tasks. Keep its whole admitted
            // stage allowance until drop(projected), including conservative
            // scratch that the pure stage may have already released.
            admission.reserve_owned(
                projection
                    .working_upper
                    .checked_sub(held_before_projection)
                    .context("resource_limit: native projection allowance underflow")?,
                &budget,
            )?;
            let projected = stages::project(vec![raw.clone()]);
            if vector_count > 0
                && !complexity
                    .reasons
                    .contains(&liteparse::ocr_merge::ComplexityReason::VectorText)
            {
                complexity
                    .reasons
                    .push(liteparse::ocr_merge::ComplexityReason::VectorText);
                complexity.needs_ocr = true;
                complexity.uncovered_vector_area = None;
            }
            complexity.layout = Some(stages::layout_complexity(&projected[0]));
            let text = &projected[0].text;
            let empty = raw.text_items.is_empty();
            let coverage = if empty && object_count == 0 && annotation_count == 0 {
                CoverageState::BlankVerified
            } else if complexity.needs_ocr || annotation_count > 0 {
                CoverageState::RequiredOcr
            } else {
                CoverageState::Native
            };
            let mut capabilities = std::collections::BTreeMap::new();
            if crate::office::is_office_extension(&prepared.summary.format) {
                capabilities.insert(
                    "source_coordinate_system".into(),
                    FeatureState::Present("retained_normalized_pdf_points".into()),
                );
                capabilities.insert("original_office_page_cell_mapping".into(), FeatureState::Unsupported("Original Office page/cell mapping is unavailable; geometry refers only to the retained normalized PDF".into()));
            }

            let (font_capability, word_capability) =
                native_text_metadata::capabilities(&characters);
            capabilities.insert("font_names".into(), font_capability);
            capabilities.insert("word_segmentation".into(), word_capability);
            capabilities.insert(
                "native_stream".into(),
                FeatureState::Present(native_text_metadata::stream_recipe().into()),
            );
            capabilities.insert(
                "native_character_geometry".into(),
                FeatureState::Present(native_text_metadata::UNCLIPPED_GEOMETRY.into()),
            );
            for feature in [
                "tagged_structure",
                "annotations",
                "form_fields",
                "xfa_packets",
            ] {
                capabilities.insert(
                    feature.into(),
                    FeatureState::Unsupported("bounded length/count getter adapter pending".into()),
                );
            }
            capabilities.insert(
                "native_recipe".into(),
                FeatureState::Present(crate::NATIVE_GRAPHICS_RECIPE.into()),
            );
            capabilities.insert(
                "document_metadata".into(),
                FeatureState::Present(document_metadata_artifact.id.clone()),
            );
            capabilities.insert(
                "bookmarks".into(),
                match &document_metadata.bookmarks {
                    FeatureState::Absent => FeatureState::Absent,
                    FeatureState::Present(_) => {
                        FeatureState::Present(document_metadata_artifact.id.clone())
                    }
                    _ => unreachable!("validated native outline"),
                },
            );
            capabilities.insert(
                "annotation_count".into(),
                FeatureState::Present(annotation_count.to_string()),
            );
            capabilities.insert(
                "vector_objects".into(),
                FeatureState::Present(
                    "complete native geometry; ancestor composition; metadata only; visibility unsupported".into(),
                ),
            );
            capabilities.insert(
                "native_layout_graphics".into(),
                FeatureState::Present(crate::native_graphics::LAYOUT_GRAPHICS_RECIPE.into()),
            );
            capabilities.insert(
                "native_content_bounds_semantics".into(),
                FeatureState::Present(crate::native_graphics::CONTENT_BOUNDS_SEMANTICS.into()),
            );
            // Bind the original source-only classifier inputs/output and frame
            // in retained metadata. Admit the added held/serialized copy before
            // cloning complexity/geometry or constructing the metadata DTO.
            let binding_bytes = encoded_size(
                &(&complexity, &image_rects, &geometry),
                budget.page_layout_bytes,
            )?;
            ensure!(
                object_bound + annotation_bound + label_bound + 16_384 + binding_bytes
                    <= budget.page_layout_bytes,
                "resource_limit: native metadata classifier/frame binding admission"
            );
            admission.reserve_native(binding_bytes, &budget)?;
            let metadata = crate::NativePageMetadata {
                page: raw.clone(),
                objects: graphics.objects,
                graphics: graphics.header,
                capabilities: capabilities.clone(),
                complexity: complexity.clone(),
                image_rects,
                geometry: geometry.clone(),
                native_text_metadata: native_text_metadata::metadata(
                    &prepared.summary,
                    number,
                    count as u32,
                    characters,
                ),
            };
            let metadata_size = encoded_size(&metadata, budget.page_layout_bytes)?;
            admission.admit_metadata(metadata_size, &budget)?;
            // Admit the complete native page pair before either output exists.
            // A later document metadata refusal retains the independently
            // admitted document snapshot, without a partial page text artifact.
            let page_staging = metadata_size
                .checked_add(text.len() as u64)
                .context("resource_limit: native page staging overflow")?;
            admit_staging(&mut staged, page_staging, &budget)?;
            let name = format!("{}-p{number}-native.txt", prepared.summary.source_sha256);
            let native_text = write_bytes(
                &prepared.staging_root,
                &name,
                ArtifactBinding {
                    source: &prepared.summary.source_sha256,
                    profile: &native_profile_sha256,
                    kind: ArtifactKind::NativeText,
                    page: Some(number),
                },
                text.as_bytes(),
                "text/plain; charset=utf-8",
                budget.page_layout_bytes,
            )?;
            let metadata_artifact = write_json(
                &prepared.staging_root,
                &format!("{}-p{number}-native.json", prepared.summary.source_sha256),
                ArtifactBinding {
                    source: &prepared.summary.source_sha256,
                    profile: &native_profile_sha256,
                    kind: ArtifactKind::NativeMetadata,
                    page: Some(number),
                },
                &metadata,
                budget.page_layout_bytes,
            )?;
            let mut renditions = Vec::new();
            if options.render_pages {
                let pixel_scale = f64::from(options.dpi) / 72.0 * f64::from(page_obj.user_unit());
                let pixel_width = (f64::from(page_obj.width()) * pixel_scale).round();
                let pixel_height = (f64::from(page_obj.height()) * pixel_scale).round();
                ensure!(
                    pixel_width.is_finite()
                        && pixel_height.is_finite()
                        && pixel_width > 0.0
                        && pixel_height > 0.0
                        && pixel_width <= u32::MAX as f64
                        && pixel_height <= u32::MAX as f64,
                    "invalid raster geometry"
                );
                let (pixel_width, pixel_height) = (pixel_width as u32, pixel_height as u32);
                let pixels = budget.admit_image(pixel_width, pixel_height, 0)?;
                admit_raster_working(admission.held_upper()?, pixel_width, pixels, &budget)?;
                let bitmap = page_obj.render(options.dpi)?;
                ensure!(
                    bitmap.width() == pixel_width as i32 && bitmap.height() == pixel_height as i32,
                    "raster geometry disagrees with admitted dimensions"
                );
                // Reuse the admitted bitmap; the allocation helper's first
                // render uses printing flags. Clear it and draw the display
                // view, including non-print annotation appearances.
                bitmap.fill_rect(0, 0, pixel_width as i32, pixel_height as i32, 0xFFFFFFFF);
                page_obj.render_into(
                    &bitmap,
                    0,
                    0,
                    pixel_width as i32,
                    pixel_height as i32,
                    None,
                    0x01,
                );
                let rgba = bitmap.to_rgba();
                let encoded =
                    encode_raster_png(&rgba, pixel_width, pixel_height, budget.image_file_bytes)?;
                let encoded_bytes = encoded.len() as u64;
                budget.admit_image(pixel_width, pixel_height, encoded_bytes)?;
                admit_staging(&mut staged, encoded_bytes, &budget)?;
                let artifact = write_bytes(
                    &prepared.staging_root,
                    &format!("{}-p{number}.png", prepared.summary.source_sha256),
                    ArtifactBinding {
                        source: &prepared.summary.source_sha256,
                        profile: &native_profile_sha256,
                        kind: ArtifactKind::Rendition,
                        page: Some(number),
                    },
                    &encoded,
                    "image/png",
                    budget.image_file_bytes,
                )?;
                let image = OcrImage {
                    path: prepared.staging_root.join(&artifact.relative_path),
                    artifact: artifact.clone(),
                    width: pixel_width,
                    height: pixel_height,
                    page_to_raster: AffineTransform {
                        a: f64::from(pixel_width) / f64::from(width),
                        b: 0.0,
                        c: 0.0,
                        d: f64::from(pixel_height) / f64::from(height),
                        e: 0.0,
                        f: 0.0,
                    },
                };
                artifacts.push(artifact);
                renditions.push(image);
            }
            if coverage == CoverageState::RequiredOcr && !renditions.is_empty() {
                ensure!(
                    text.len() as u64 <= budget.native_layout_input_bytes,
                    "resource_limit: page native OCR input"
                );
                let region = OcrRegion {
                    id: format!("p{number}-required"),
                    bbox: BoundingBox {
                        x: 0.0,
                        y: 0.0,
                        width: width.into(),
                        height: height.into(),
                    },
                    required: true,
                    reason: format!(
                        "required visible coverage; native complexity={:?}; annotation count={annotation_count}",
                        complexity.reasons
                    ),
                };
                let mut task = OcrTask {
                    schema_version: "gptgrep.ocr.task.v1".into(),
                    id: String::new(),
                    task_sha256: String::new(),
                    source_sha256: prepared.summary.source_sha256.clone(),
                    prepared_sha256: prepared.summary.prepared_sha256.clone(),
                    source_path: prepared.source_path.clone(),
                    page: number,
                    page_width_points: width.into(),
                    page_height_points: height.into(),
                    regions: vec![region],
                    images: renditions.clone(),
                    native_text: text.clone(),
                    layout_context: String::new(),
                    profile: OcrProfile::default(),
                    budget: OcrTaskBudget {
                        transport_bytes: budget
                            .native_image_limits()?
                            .transport_bytes()?
                            .max(OcrTaskBudget::default().transport_bytes),
                        resources: budget.clone(),
                        ..Default::default()
                    },
                };
                task.task_sha256 = ocr_task_digest(&task)?;
                task.id = format!("ocr-{}", task.task_sha256);
                tasks.push(task);
            }
            artifacts.push(native_text.clone());
            artifacts.push(metadata_artifact.clone());
            pages.push(NativePage {
                number,
                label: document_metadata.page_labels[index].label.clone(),
                width_points: width.into(),
                height_points: height.into(),
                geometry,
                native_text,
                metadata_artifact,
                renditions,
                complexity,
                coverage,
                capabilities,
            });
            raw_pages.push(raw);
            let native_page = pages.last().expect("just retained native page");
            coverage_plan.push((number, native_page.coverage.clone()));
            let blocker = if native_page.coverage == CoverageState::RequiredOcr
                && native_page.renditions.is_empty()
            {
                blockers.push(format!(
                    "page {} requires OCR but no admitted rendition was requested",
                    number
                ));
                blockers.last()
            } else {
                None
            };
            let retained_owned = native_memory::retained_page_owned(
                raw_pages.last().expect("just retained raw page"),
                native_page,
                &artifacts[first_artifact..],
                &tasks[first_task..],
                blocker,
            )?;
            // The character/font/object trees and metadata's raw-page clone are
            // still live through rendering and task creation. Release their
            // reservation only after dropping those trees and projected copies.
            drop(metadata);
            drop(projected);
            drop(text_page);
            drop(page_obj);
            admission.finish_phase(retained_owned, &budget)?;
        }
    }
    prepared.verify(&budget)?;
    let plan = OcrPlan {
        tasks,
        pages: coverage_plan,
        blockers,
    };
    let mut candidate = NativeCandidate {
        summary: prepared.summary.clone(),
        pages,
        artifacts,
        ocr_plan: plan,
        native_profile_sha256,
        prepared,
        raw_pages,
        document_staging_used: staged,
        native_owned_upper: admission.retained_owned(),
        metadata_admitted_upper: admission.metadata_bytes(),
        execution_budget: budget,
        frozen_integrity_sha256: String::new(),
        annotation_counts,
        document_metadata: Some(document_metadata),
        document_metadata_artifact: Some(document_metadata_artifact),
    };
    candidate.frozen_integrity_sha256 = candidate.integrity_digest()?;
    Ok(candidate)
}

/// Logical owned scratch for the pinned liteparse XY stage, independent of
/// sparse-text padding. Its 2pt density and 5pt column buckets use absolute
/// spans, even when the characters themselves have very large advances.
fn projection_xy_scratch_upper(x_span: f64, y_span: f64, items: u64) -> Result<u64> {
    // The only admitted item rotation is zero. The pinned projection rounds
    // dimensions, then has two merging passes (at most 2N merges). Each merge
    // has at most three endpoint/difference operations per axis. Including
    // zero in the input extent bounds absolute coordinates as well as spans.
    // Two points cover dimension rounding; a fourfold enclosure dominates
    // accumulated f32 rounding with 8N*EPSILON <= 1/4. Reject a coarser case.
    ensure!(
        items as f64 * 8.0 * f64::from(f32::EPSILON) <= 0.25,
        "resource_limit: projection merge precision admission"
    );
    let buckets = |span: f64, width: f32| -> Result<u64> {
        ensure!(
            span.is_finite() && span >= 0.0 && (span as f32).is_finite(),
            "resource_limit: projection finite absolute span admission"
        );
        let enclosure = (span + 2.0) * 4.0;
        let rounded = enclosure as f32;
        ensure!(
            rounded.is_finite(),
            "resource_limit: projection rounded span overflow"
        );
        // next_up prevents the f64->f32 conversion rounding the enclosure
        // down. Use the upstream f32 division/ceil, then check before casting.
        let count = (rounded.next_up() / width).ceil().max(1.0);
        ensure!(
            count.is_finite()
                && f64::from(count) < u64::MAX as f64
                && f64::from(count) < usize::MAX as f64,
            "resource_limit: projection bucket count overflow"
        );
        Ok(count as u64)
    };
    let bx = buckets(x_span, 2.0)?;
    let by = buckets(y_span, 2.0)?;
    let histogram = buckets(x_span, 5.0)?;
    // Density: one exact Vec, growing nonzero Vec (including reallocation
    // overlap), and stable-sort scratch fit eight f32 slots per bucket.
    // Axis calls release their arrays before recursion; sum both axes anyway.
    let density = bx
        .checked_add(by)
        .and_then(|n| n.checked_mul(8 * size_of::<f32>() as u64))
        .context("resource_limit: projection density scratch overflow")?;
    // The column histogram is exact usize storage. At most N occupied peaks
    // and line starts exist. 64 usize slots per item cover their growable
    // vectors, peak clones, fill arrays and sort scratch. Charge this at every
    // one of the 13 retained levels (root plus XY_MAX_DEPTH=12), also covering
    // parent/sibling index vectors, region nodes, and collected leaf paths.
    let histogram = histogram
        .checked_mul(size_of::<usize>() as u64)
        .context("resource_limit: projection histogram scratch overflow")?;
    let indices = items
        .checked_mul(64 * size_of::<usize>() as u64)
        .and_then(|n| n.checked_mul(13))
        .context("resource_limit: projection index scratch overflow")?;
    density
        .checked_add(histogram)
        .and_then(|n| n.checked_add(indices))
        .and_then(|n| n.checked_add(4096))
        .context("resource_limit: projection XY scratch overflow")
}

struct ProjectionAdmission {
    text_upper: u64,
    working_upper: u64,
}

/// Bound sparse text, projected copies and absolute-span scratch before either
/// native classification or finalization calls the unchanged pure projector.
#[cfg(test)]
fn admit_projection(pages: &[Page], budget: &ResourceBudget, held_upper: u64) -> Result<u64> {
    Ok(projection_admission(pages, budget, held_upper)?.text_upper)
}

fn projection_admission(
    pages: &[Page],
    budget: &ResourceBudget,
    held_upper: u64,
) -> Result<ProjectionAdmission> {
    let mut total = 0u64;
    let mut scratch = 0u64;
    for page in pages {
        ensure!(
            [page.page_width, page.page_height]
                .iter()
                .all(|v| v.is_finite() && *v > 0.0),
            "native projection page geometry invalid"
        );
        let mut smallest = f32::INFINITY;
        let mut chars = 0u64;
        // Signed/out-of-CropBox native positions are retained, so page size
        // alone no longer bounds sparse layout padding. Admit the whole span
        // before the unchanged projection stage allocates any output.
        let mut left = 0.0_f64;
        let mut top = 0.0_f64;
        let mut right = f64::from(page.page_width);
        let mut bottom = f64::from(page.page_height);
        for item in &page.text_items {
            // Native glyph runs and OCR replacement items use TextItem's
            // derived Default rotation=0. PDF quarter-turns already belong to
            // the page viewport transform, not this virtual-canvas mechanism.
            ensure!(
                item.rotation == 0.0,
                "capability_unavailable: projection item rotation is not qualified"
            );
            ensure!(
                [
                    item.x,
                    item.y,
                    item.width,
                    item.height,
                    item.x + item.width,
                    item.y + item.height,
                    item.x + item.width.round(),
                    item.y + item.height.round()
                ]
                .iter()
                .all(|n| n.is_finite())
                    && item.width > 0.0
                    && item.height > 0.0,
                "native projection geometry or endpoint overflow"
            );
            left = left.min(f64::from(item.x));
            top = top.min(f64::from(item.y));
            right = right.max(f64::from(item.x) + f64::from(item.width));
            bottom = bottom.max(f64::from(item.y) + f64::from(item.height));
            right = right.max(f64::from(item.x + item.width.round()));
            bottom = bottom.max(f64::from(item.y + item.height.round()));
            let count = item.text.chars().count().max(1);
            smallest = smallest.min(item.width / count as f32);
            chars = chars
                .checked_add(count as u64)
                .context("projection char overflow")?;
        }
        if chars == 0 {
            continue;
        }
        ensure!(
            smallest.is_finite() && smallest >= 0.25,
            "capability_unavailable: projection glyph advance below qualified minimum"
        );
        let columns = ((right - left).max(bottom - top) / f64::from(smallest)).ceil();
        ensure!(
            columns.is_finite() && columns <= budget.page_layout_bytes as f64,
            "resource_limit: native projection coordinate span exceeds admission"
        );
        let columns = columns as u64;
        let upper = chars
            .checked_mul(
                columns
                    .checked_add(64)
                    .context("projection column overflow")?,
            )
            .and_then(|v| v.checked_mul(4))
            .context("projection size overflow")?;
        // This padding estimate belongs to working admission below. Actual
        // projected text and metadata retain their bounded wire writers.
        total = total
            .checked_add(upper)
            .context("projection document overflow")?;
        let items =
            u64::try_from(page.text_items.len()).context("projection item count overflow")?;
        let xy = projection_xy_scratch_upper(right - left, bottom - top, items)?;
        // Include dynamic strings/words and typed TextItem storage in copies
        // retained by projection, line formation and ParsedPage construction.
        // Counting serialization uses a sink; it does not materialize JSON.
        let copies = encoded_size(&page.text_items, budget.page_layout_bytes)?
            .checked_add(
                items
                    .checked_mul(size_of::<TextItem>() as u64)
                    .context("projection typed copy overflow")?,
            )
            .and_then(|n| n.checked_mul(16))
            .context("projection owned copy overflow")?;
        scratch = scratch
            .checked_add(xy)
            .and_then(|n| n.checked_add(copies))
            .context("resource_limit: projection document scratch overflow")?;
    }
    let working_upper = held_upper
        .checked_add(
            total
                .checked_mul(4)
                .context("projection working overflow")?,
        )
        .and_then(|n| n.checked_add(scratch))
        .context("projection held working overflow")?;
    ensure!(
        working_upper <= budget.owned_working_bytes,
        "resource_limit: complete projection working admission including XY scratch and copies: estimated_bytes={working_upper}, held_bytes={held_upper}, projected_text_bytes={total}, scratch_bytes={scratch}, limit_bytes={}",
        budget.owned_working_bytes
    );
    Ok(ProjectionAdmission {
        text_upper: total,
        working_upper,
    })
}

fn admit_final_projection(
    pages: &[Page],
    budget: &ResourceBudget,
    held_upper: u64,
    native_retained_upper: u64,
    outcome_bytes: u64,
) -> Result<()> {
    let projection = projection_admission(pages, budget, held_upper)?;
    // Preserve the conservative allowance for document signals, block/layout
    // copies, headings and canonical text. It is working memory, not JSON size.
    let extra_working_upper = native_retained_upper
        .checked_mul(2)
        .and_then(|native| {
            projection
                .text_upper
                .checked_mul(8)
                .and_then(|text| native.checked_add(text))
        })
        .and_then(|total| total.checked_add(outcome_bytes))
        .context("resource_limit: finalization working allowance overflow")?;
    let working_upper = projection
        .working_upper
        .checked_add(extra_working_upper)
        .context("resource_limit: complete final working bound overflow")?;
    ensure!(
        working_upper <= budget.owned_working_bytes,
        "resource_limit: complete final working admission: estimated_bytes={working_upper}, projection_bytes={}, extra_bytes={extra_working_upper}, native_retained_bytes={native_retained_upper}, projected_text_bytes={}, outcome_bytes={outcome_bytes}, limit_bytes={}",
        projection.working_upper,
        projection.text_upper,
        budget.owned_working_bytes
    );
    Ok(())
}

pub fn finalize_document(
    mut candidate: NativeCandidate,
    mut outcomes: Vec<OcrPageResult>,
    mode: OcrMode,
    budget: &ResourceBudget,
) -> Result<RichParsedDocument> {
    budget.validate()?;
    candidate.verify_integrity(budget)?;
    if candidate.summary.format == "plaintext" {
        ensure!(outcomes.is_empty(), "plaintext cannot accept OCR results");
        return crate::rich_text::finalize(candidate, mode, budget);
    }
    let mut seen = BTreeSet::new();
    ensure!(
        outcomes.len() <= candidate.pages.len(),
        "unexpected OCR result count"
    );
    let final_artifact_count = outcomes
        .len()
        .checked_mul(2)
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_add(candidate.artifacts.len()))
        .context("resource_limit: final artifact count overflow")?;
    ensure!(
        final_artifact_count <= candidate.artifacts.capacity(),
        "resource_limit: final artifact capacity was not admitted"
    );
    let outcome_bytes = encoded_size(&outcomes, budget.document_metadata_bytes)?;
    // A short OCR line still creates a full TextItem, independently of wire
    // length. Admit replacement slots and line/sort scratch before mutation.
    let ocr_replacement_upper = native_memory::ocr_replacement_working_upper(&outcomes)?;
    let final_owned_upper = candidate
        .native_owned_upper
        .checked_add(
            outcome_bytes
                .checked_mul(8)
                .context("resource_limit: OCR finalization working overflow")?,
        )
        .and_then(|n| n.checked_add(ocr_replacement_upper))
        .context("resource_limit: native/OCR working overflow")?;
    ensure!(
        final_owned_upper <= budget.owned_working_bytes,
        "resource_limit: native/OCR finalization admission"
    );
    outcomes.sort_by_key(|result| result.page);
    for result in &outcomes {
        ensure!(seen.insert(result.page), "duplicate OCR page result");
        let task = candidate
            .ocr_plan
            .tasks
            .iter()
            .find(|task| task.page == result.page)
            .context("unexpected OCR page result")?;
        validate_ocr_result(task, result)?;
    }
    if mode == OcrMode::Off {
        ensure!(outcomes.is_empty(), "OCR off cannot accept model results");
    } else {
        ensure!(
            candidate.ocr_plan.blockers.is_empty()
                && candidate
                    .pages
                    .iter()
                    .filter(|page| page.coverage == CoverageState::RequiredOcr)
                    .all(|page| seen.contains(&page.number)),
            "required OCR coverage incomplete; publication blocked"
        );
    }
    encoded_size(&outcomes, budget.document_metadata_bytes)?;
    let final_profile_sha256 = sha256(&serde_json::to_vec(&(
        "gptgrep.finalize.complete-document.v1",
        RICH_PARSER,
        crate::LITEPARSE_REVISION,
        &candidate.native_profile_sha256,
        mode,
        &outcomes,
    ))?);
    let mut raw_ocr = Vec::new();
    raw_ocr.try_reserve_exact(outcomes.len())?;
    for result in &outcomes {
        let page = candidate
            .raw_pages
            .iter_mut()
            .find(|page| page.page_number == result.page as usize)
            .context("OCR page absent")?;
        let metadata_size = encoded_size(result, budget.ocr_result_bytes)?;
        admit_staging(&mut candidate.document_staging_used, metadata_size, budget)?;
        let regions = write_json(
            &candidate.prepared.staging_root,
            &format!(
                "{}-p{}-ocr.json",
                candidate.summary.source_sha256, result.page
            ),
            ArtifactBinding {
                source: &candidate.summary.source_sha256,
                profile: &final_profile_sha256,
                kind: ArtifactKind::OcrRegions,
                page: Some(result.page),
            },
            result,
            budget.ocr_result_bytes,
        )?;
        candidate.artifacts.push(regions);
        let mut ordered = result.regions.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|region| region.reading_order);
        let text_bytes = ordered
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.text.len() + 1)
            })
            .context("resource_limit: raw OCR text length overflow")?;
        ensure!(
            text_bytes as u64 <= budget.ocr_result_bytes,
            "resource_limit: raw OCR text"
        );
        let mut text = String::with_capacity(text_bytes);
        for region in &ordered {
            text.push_str(&region.text);
            text.push('\n');
        }
        ensure!(
            text.len() as u64 <= budget.ocr_result_bytes,
            "resource_limit: raw OCR text"
        );
        admit_staging(
            &mut candidate.document_staging_used,
            text.len() as u64,
            budget,
        )?;
        let artifact = write_bytes(
            &candidate.prepared.staging_root,
            &format!(
                "{}-p{}-ocr.txt",
                candidate.summary.source_sha256, result.page
            ),
            ArtifactBinding {
                source: &candidate.summary.source_sha256,
                profile: &final_profile_sha256,
                kind: ArtifactKind::OcrText,
                page: Some(result.page),
            },
            text.as_bytes(),
            "text/plain; charset=utf-8",
            budget.ocr_result_bytes,
        )?;
        raw_ocr.push((result.page, text, artifact.clone()));
        candidate.artifacts.push(artifact);
        // This foundation admits full-page required tasks. Their complete
        // transcription replaces the reading view; native bytes stay retained.
        let native_page = candidate
            .pages
            .iter_mut()
            .find(|page| page.number == result.page)
            .expect("validated page");
        let replacement_lines = result
            .regions
            .iter()
            .try_fold(0usize, |n, region| {
                n.checked_add(region.text.lines().count())
            })
            .context("resource_limit: OCR replacement line count overflow")?;
        page.text_items.clear();
        // Old backing slots remain covered by the native retained bound; the
        // complete new array was admitted above, including overlap on growth.
        page.text_items.try_reserve_exact(replacement_lines)?;
        for region in ordered {
            let lines = region.text.lines().collect::<Vec<_>>();
            let height = region.bbox.height / lines.len().max(1) as f64;
            for (index, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                page.text_items.push(TextItem {
                    text: (*line).into(),
                    x: region.bbox.x as f32,
                    y: (region.bbox.y + index as f64 * height) as f32,
                    width: region.bbox.width as f32,
                    height: height as f32,
                    font_size: Some(height.min(12.0) as f32),
                    ..Default::default()
                });
            }
        }
        native_page.coverage = CoverageState::ValidatedOcr;
    }
    let native_metadata_bytes = candidate
        .artifacts
        .iter()
        .filter(|artifact| {
            matches!(
                artifact.kind,
                ArtifactKind::NativeMetadata | ArtifactKind::OcrRegions
            )
        })
        .try_fold(0u64, |total, artifact| total.checked_add(artifact.bytes))
        .context("resource_limit: retained native/OCR metadata length overflow")?;
    ensure!(
        native_metadata_bytes <= budget.document_metadata_bytes,
        "resource_limit: retained native/OCR metadata admission: retained_bytes={native_metadata_bytes}, limit_bytes={}",
        budget.document_metadata_bytes
    );
    admit_final_projection(
        &candidate.raw_pages,
        budget,
        final_owned_upper,
        candidate.native_owned_upper,
        outcome_bytes,
    )?;
    let mut projected = stages::project(candidate.raw_pages);
    let signals = stages::document_signals(&projected, true);
    let mut headings = Vec::new();
    for page in &mut projected {
        let blocks = stages::extract_blocks(
            page,
            &signals,
            &stages::BlockOptions {
                outline: &[],
                image_mode: liteparse::config::ImageMode::Off,
                keep_headers_footers: true,
            },
        );
        if let Some(blocks) = blocks.as_ref() {
            page.blocks = Some(stages::layout_blocks(page, blocks));
        }
        for block in page.blocks.as_deref().unwrap_or_default() {
            if block.kind == "heading"
                && let (Some(title), Some(level)) = (&block.text, block.level)
                && let Some(line) = locate_heading(&page.text, title)
            {
                headings.push(Heading {
                    title: title.clone(),
                    level: u32::from(level),
                    page: page.page_number as u32,
                    line,
                });
            }
        }
    }
    if projected.iter().all(|page| page.text.trim().is_empty()) {
        bail!("no searchable text extracted; required OCR or explicit no-content policy needed");
    }
    let inputs = projected
        .iter()
        .map(|page| PageInput {
            number: page.page_number as u32,
            text: page.text.clone(),
        })
        .collect::<Vec<_>>();
    let bookmarks = candidate
        .document_metadata
        .as_ref()
        .and_then(|metadata| match &metadata.bookmarks {
            FeatureState::Present(bookmarks) => Some(bookmarks.as_slice()),
            _ => None,
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|bookmark| match &bookmark.destination {
            FeatureState::Present(dest) if !bookmark.title.trim().is_empty() => {
                Some(gptgrep_pageindex::Bookmark {
                    title: bookmark.title.clone(),
                    level: u32::from(bookmark.depth),
                    page: dest.physical_page,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let document = from_pages(
        "prepared document",
        &inputs,
        &headings,
        &bookmarks,
        RICH_PARSER,
    )?;
    let mut segments = Vec::new();
    let mut unmapped = 0usize;
    for page in &document.pages {
        let source = candidate
            .pages
            .iter()
            .find(|native| native.number == page.number)
            .expect("complete native pages");
        let native = String::from_utf8(read_bounded(
            &safe_path(
                &candidate.prepared.staging_root,
                &source.native_text.relative_path,
            )?,
            source.native_text.bytes,
            &source.native_text.sha256,
        )?)?;
        let page_start = document
            .text
            .lines()
            .take(page.line_start - 1)
            .map(|line| line.len() + 1)
            .sum::<usize>();
        let mut offset = page_start;
        for line in page.text.split_inclusive('\n') {
            let line = line.trim_end_matches('\n');
            let mut matched = None;
            if !line.is_empty() {
                if let Some(start) = native.find(line) {
                    matched = Some((
                        source.native_text.id.clone(),
                        start,
                        ArtifactKind::NativeText,
                    ));
                }
                if matched.is_none() {
                    for (number, text, artifact) in &raw_ocr {
                        if *number == page.number
                            && let Some(start) = text.find(line)
                        {
                            matched = Some((artifact.id.clone(), start, ArtifactKind::OcrText));
                            break;
                        }
                    }
                }
            }
            if let Some((artifact_id, start, kind)) = matched {
                segments.push(SegmentMap {
                    page: page.number,
                    merged_start: offset as u64,
                    merged_end: (offset + line.len()) as u64,
                    artifact_id,
                    artifact_start: start as u64,
                    artifact_end: (start + line.len()) as u64,
                    kind,
                });
            } else if !line.is_empty() {
                unmapped += 1;
            }
            offset += line.len() + 1;
        }
    }
    let layout_size = encoded_size(&projected, budget.document_metadata_bytes)?;
    for page in &projected {
        encoded_size(page, budget.page_layout_bytes)?;
    }
    let retained_metadata = native_metadata_bytes
        .checked_add(layout_size)
        .context("resource_limit: retained metadata length overflow")?;
    ensure!(
        retained_metadata <= budget.document_metadata_bytes,
        "resource_limit: retained complete metadata: retained_bytes={retained_metadata}, native_ocr_bytes={native_metadata_bytes}, layout_bytes={layout_size}, limit_bytes={}",
        budget.document_metadata_bytes
    );
    admit_staging(&mut candidate.document_staging_used, layout_size, budget)?;
    candidate.artifacts.push(write_json(
        &candidate.prepared.staging_root,
        &format!("{}-layout.json", candidate.summary.source_sha256),
        ArtifactBinding {
            source: &candidate.summary.source_sha256,
            profile: &final_profile_sha256,
            kind: ArtifactKind::Layout,
            page: None,
        },
        &projected,
        budget.document_metadata_bytes,
    )?);
    admit_staging(
        &mut candidate.document_staging_used,
        document.text.len() as u64,
        budget,
    )?;
    candidate.artifacts.push(write_bytes(
        &candidate.prepared.staging_root,
        &format!("{}-merged.txt", candidate.summary.source_sha256),
        ArtifactBinding {
            source: &candidate.summary.source_sha256,
            profile: &final_profile_sha256,
            kind: ArtifactKind::MergedText,
            page: None,
        },
        document.text.as_bytes(),
        "text/plain; charset=utf-8",
        budget.document_metadata_bytes,
    )?);
    candidate.prepared.verify(budget)?;
    for artifact in &candidate.artifacts {
        verify_artifact(&candidate.prepared.staging_root, artifact)?;
    }
    let coverage = candidate
        .pages
        .iter()
        .map(|page| (page.number, page.coverage.clone()))
        .collect();
    Ok(RichParsedDocument {
        summary: candidate.summary,
        document,
        pages: candidate.pages,
        artifacts: candidate.artifacts,
        segments,
        ocr_results: outcomes,
        ocr_tasks: candidate.ocr_plan.tasks,
        native_profile_sha256: candidate.native_profile_sha256,
        coverage,
        unmapped_merged_lines: unmapped,
    })
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use image::ImageEncoder;

    #[test]
    fn png_streaming_respects_lossless_wire_and_working_limits() -> Result<()> {
        let (width, height) = (512u32, 1024u32);
        let row_bytes = width as usize * 4;
        let mut state = 0x1a2b_3c4d_5e6f_7081u64;
        let mut next_pixel = |pixel: &mut [u8]| {
            for channel in &mut pixel[..3] {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *channel = (state >> 32) as u8;
            }
            pixel[3] = 255;
        };
        // Nonadjacent repeated rows are still within the compressor's 32 KiB
        // dictionary. No document text, IDs or expected encoded size are used.
        let mut cycle = vec![0; row_bytes * 8];
        for pixel in cycle.chunks_exact_mut(4) {
            next_pixel(pixel);
        }
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let start = (row % 8) * row_bytes;
            rgba.extend_from_slice(&cycle[start..start + row_bytes]);
        }
        let mut legacy = Vec::new();
        image::codecs::png::PngEncoder::new(&mut legacy).write_image(
            &rgba,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;
        let mut budget = ResourceBudget::default();
        assert!(legacy.len() as u64 > budget.image_file_bytes);
        drop(legacy);
        let encoded = encode_raster_png(&rgba, width, height, budget.image_file_bytes)?;
        assert!(encoded.len() as u64 <= budget.image_file_bytes);
        let decoded =
            image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)?.into_rgba8();
        assert_eq!(decoded.dimensions(), (width, height));
        assert_eq!(decoded.as_raw(), &rgba);
        let wire = encoded.len() as u64;
        assert_eq!(encode_raster_png(&rgba, width, height, wire)?, encoded);
        // The last IEND CRC must propagate through writer.finish(), rather
        // than becoming an ignored Drop error after an otherwise valid stream.
        let error = encode_raster_png(&rgba, width, height, wire - 1).unwrap_err();
        assert!(format!("{error:#}").contains("bounded writer admission failed"));
        assert!(format!("{error:#}").contains("emitted_prefix_bytes="));

        let pixels = u64::from(width) * u64::from(height);
        assert_eq!(png_streaming_working_upper(width)?, 4_243_456);
        let held = 12_345;
        budget.owned_working_bytes = 9_498_681;
        admit_raster_working(held, width, pixels, &budget)?;
        budget.owned_working_bytes -= 1;
        assert!(admit_raster_working(held, width, pixels, &budget).is_err());
        assert!(admit_raster_working(u64::MAX, width, pixels, &budget).is_err());

        // Independent RGB pixels cannot use the repeated-row dictionary.
        // The byte cap still refuses them; no partial Vec escapes the helper.
        for pixel in rgba.chunks_exact_mut(4) {
            next_pixel(pixel);
        }
        let error = encode_raster_png(&rgba, width, height, budget.image_file_bytes).unwrap_err();
        assert!(format!("{error:#}").contains("bounded writer admission failed"));
        Ok(())
    }

    #[test]
    fn dense_projection_separates_actual_wire_bytes_from_working_admission() {
        let page = Page {
            page_number: 1,
            page_label: None,
            page_width: 1000.0,
            page_height: 1000.0,
            content_bounds: None,
            text_items: (0..40)
                .map(|row| TextItem {
                    text: "A".repeat(100),
                    x: 10.0,
                    y: 10.0 + row as f32 * 20.0,
                    width: 500.0,
                    height: 10.0,
                    ..Default::default()
                })
                .collect(),
            graphics: vec![],
            vector_graphics: None,
            struct_nodes: vec![],
            image_refs: vec![],
            annotations: None,
            form_fields: None,
            structure_tree: None,
        };
        let mut budget = ResourceBudget::default();
        let held = 1024;
        let admission = projection_admission(std::slice::from_ref(&page), &budget, held)
            .expect("dense text fits working admission");
        assert!(admission.text_upper > budget.page_layout_bytes);
        budget.owned_working_bytes = admission.working_upper;
        projection_admission(std::slice::from_ref(&page), &budget, held)
            .expect("exact working bound is admitted");
        let projected = stages::project(vec![page.clone()]);
        assert_eq!(projected.len(), 1);
        let output = &projected[0];
        assert_eq!(
            output
                .text
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>(),
            "A".repeat(4000)
        );
        assert!((output.text.len() as u64) < budget.page_layout_bytes);
        assert!(encoded_size(output, budget.page_layout_bytes).unwrap() < budget.page_layout_bytes);
        assert_eq!(output.text_items.len(), page.text_items.len());
        for (actual, expected) in output.text_items.iter().zip(&page.text_items) {
            assert_eq!(actual.text, expected.text);
            assert_eq!(actual.x, expected.x);
            assert_eq!(actual.y, expected.y);
            assert_eq!(actual.width, expected.width);
            assert_eq!(actual.height, expected.height);
            assert_eq!(actual.rotation, expected.rotation);
        }
        budget.owned_working_bytes -= 1;
        let mut projection_started = false;
        let rejected = projection_admission(std::slice::from_ref(&page), &budget, held).map(|_| {
            projection_started = true;
            stages::project(vec![page])
        });
        let error = rejected.expect_err("one byte below working bound must reject");
        assert!(
            error
                .to_string()
                .contains("complete projection working admission")
        );
        assert!(!projection_started);
    }

    #[test]
    fn native_projection_admits_signed_span_and_rejects_far_outside_before_allocation() {
        let mut page = Page {
            page_number: 1,
            page_label: None,
            page_width: 100.0,
            page_height: 100.0,
            content_bounds: None,
            text_items: vec![TextItem {
                text: "A".into(),
                x: 10.0,
                y: 10.0,
                width: 5.0,
                height: 10.0,
                ..Default::default()
            }],
            graphics: vec![],
            vector_graphics: None,
            struct_nodes: vec![],
            image_refs: vec![],
            annotations: None,
            form_fields: None,
            structure_tree: None,
        };
        let budget = ResourceBudget::default();
        let inside = admit_projection(std::slice::from_ref(&page), &budget, 0).unwrap();
        page.text_items[0].x = -20.0;
        page.text_items[0].y = -30.0;
        assert!(admit_projection(std::slice::from_ref(&page), &budget, 0).unwrap() > inside);
        let projected = stages::project(vec![page.clone()]);
        assert!(projected[0].text.contains('A'));
        page.text_items[0].x = 1.0e15;
        assert!(
            admit_projection(std::slice::from_ref(&page), &budget, 0)
                .unwrap_err()
                .to_string()
                .contains("coordinate span")
        );
        page.text_items[0].x = f32::MAX;
        page.text_items[0].width = f32::MAX;
        assert!(admit_projection(std::slice::from_ref(&page), &budget, 0).is_err());
    }

    #[test]
    fn native_projection_xy_scratch_rejects_wide_glyph_density_and_histogram_before_projection() {
        // Five items reach density; eight also qualify for column histograms.
        // Only small DTOs are constructed. Never invoke projection on them.
        for count in [5, 8] {
            let page = Page {
                page_number: 1,
                page_label: None,
                page_width: 100.0,
                page_height: 100.0,
                content_bounds: None,
                text_items: (0..count)
                    .map(|i| TextItem {
                        text: "A".into(),
                        x: 0.0,
                        y: i as f32 * 100.0,
                        width: 1_000_000.0,
                        height: 10.0,
                        ..Default::default()
                    })
                    .collect(),
                graphics: vec![],
                vector_graphics: None,
                struct_nodes: vec![],
                image_refs: vec![],
                annotations: None,
                form_fields: None,
                structure_tree: None,
            };
            let mut budget = ResourceBudget::default();
            budget.owned_working_bytes = 1_048_576;
            let old_text_upper = count as u64 * (1 + 64) * 4;
            let copies = (encoded_size(&page.text_items, budget.page_layout_bytes).unwrap()
                + count as u64 * size_of::<TextItem>() as u64)
                * 16;
            assert!(old_text_upper * 4 + copies < budget.owned_working_bytes);
            let error = admit_projection(std::slice::from_ref(&page), &budget, 0).unwrap_err();
            assert!(error.to_string().contains("including XY scratch"));
        }
    }

    #[test]
    fn native_projection_xy_scratch_joins_held_bytes_and_all_pages() {
        let page = Page {
            page_number: 1,
            page_label: None,
            page_width: 100.0,
            page_height: 100.0,
            content_bounds: None,
            text_items: vec![
                TextItem {
                    text: "A".into(),
                    x: -20.0,
                    y: -30.0,
                    width: 100.0,
                    height: 10.0,
                    ..Default::default()
                };
                8
            ],
            graphics: vec![],
            vector_graphics: None,
            struct_nodes: vec![],
            image_refs: vec![],
            annotations: None,
            form_fields: None,
            structure_tree: None,
        };
        let mut budget = ResourceBudget::default();
        let text = admit_projection(std::slice::from_ref(&page), &budget, 0).unwrap();
        let xy = projection_xy_scratch_upper(120.0, 130.0, 8).unwrap();
        // Fourfold rounded enclosure: ceil(next_up(488)/2)=245;
        // ceil(next_up(528)/2)=265; ceil(next_up(488)/5)=98.
        let word = size_of::<usize>() as u64;
        assert_eq!(xy, (245 + 265) * 32 + 98 * word + 8 * 64 * word * 13 + 4096);
        let copies = (encoded_size(&page.text_items, budget.page_layout_bytes).unwrap()
            + 8 * size_of::<TextItem>() as u64)
            * 16;
        let held = 1024;
        let one = text * 4 + xy + copies;
        budget.owned_working_bytes = held + one;
        assert_eq!(
            admit_projection(std::slice::from_ref(&page), &budget, held).unwrap(),
            text
        );
        budget.owned_working_bytes -= 1;
        assert!(admit_projection(std::slice::from_ref(&page), &budget, held).is_err());
        budget.owned_working_bytes = held + one * 2;
        assert_eq!(
            admit_projection(&[page.clone(), page.clone()], &budget, held).unwrap(),
            text * 2
        );
        budget.owned_working_bytes -= 1;
        assert!(admit_projection(&[page.clone(), page.clone()], &budget, held).is_err());
        let native = 128;
        let outcomes = 64;
        let final_allowance = native * 2 + text * 8 + outcomes;
        budget.owned_working_bytes = held + one + final_allowance;
        admit_final_projection(std::slice::from_ref(&page), &budget, held, native, outcomes)
            .unwrap();
        budget.owned_working_bytes -= 1;
        // Each allowance fits separately; only their simultaneous sum exceeds
        // this limit. No pure projection or finalization should have begun.
        assert!(held + one <= budget.owned_working_bytes);
        assert!(final_allowance <= budget.owned_working_bytes);
        let error =
            admit_final_projection(std::slice::from_ref(&page), &budget, held, native, outcomes)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("complete final working admission")
        );
        assert!(
            error
                .to_string()
                .contains(&format!("estimated_bytes={}", held + one + final_allowance))
        );
        assert!(
            error
                .to_string()
                .contains(&format!("limit_bytes={}", budget.owned_working_bytes))
        );
    }

    #[test]
    fn native_projection_xy_scratch_rejects_unqualified_numeric_ranges_and_item_rotation() {
        for span in [
            f64::NAN,
            f64::INFINITY,
            -1.0,
            f64::from(f32::MAX) * 2.0,
            f64::from(f32::MAX),
            u64::MAX as f64,
        ] {
            assert!(projection_xy_scratch_upper(span, 100.0, 8).is_err());
            assert!(projection_xy_scratch_upper(100.0, span, 8).is_err());
        }
        assert!(projection_xy_scratch_upper(100.0, 100.0, u64::MAX).is_err());
        let mut page = Page {
            page_number: 1,
            page_label: None,
            page_width: 100.0,
            page_height: 100.0,
            content_bounds: None,
            text_items: vec![TextItem {
                text: "A".into(),
                x: 0.0,
                y: 0.0,
                width: 5.0,
                height: 10.0,
                ..Default::default()
            }],
            graphics: vec![],
            vector_graphics: None,
            struct_nodes: vec![],
            image_refs: vec![],
            annotations: None,
            form_fields: None,
            structure_tree: None,
        };
        let budget = ResourceBudget::default();
        assert_eq!(page.text_items[0].rotation, 0.0);
        admit_projection(std::slice::from_ref(&page), &budget, 0).unwrap();
        for rotation in [90.0, 180.0, 270.0, f32::NAN] {
            page.text_items[0].rotation = rotation;
            assert!(
                admit_projection(std::slice::from_ref(&page), &budget, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("item rotation")
            );
        }
    }

    #[tokio::test]
    async fn finalizer_rejects_forged_coverage_source_recipe_and_artifact_snapshots() {
        for mutation in 0..6 {
            let directory = tempfile::tempdir().unwrap();
            let (prepared, mut candidate, budget) =
                crate::rich_tests::fixture_candidate(&directory, false, 25).await;
            assert!(
                candidate
                    .pages
                    .iter()
                    .any(|page| page.coverage == CoverageState::RequiredOcr)
            );
            let before = std::fs::read_dir(prepared.staging_root()).unwrap().count();
            match mutation {
                0 => {
                    for page in &mut candidate.pages {
                        page.coverage = CoverageState::Native;
                    }
                    candidate.ocr_plan.tasks.clear();
                    candidate.ocr_plan.blockers.clear();
                    for (_, state) in &mut candidate.ocr_plan.pages {
                        *state = CoverageState::Native;
                    }
                }
                1 => candidate.summary.source_sha256 = "0".repeat(64),
                2 => candidate.native_profile_sha256 = "1".repeat(64),
                3 => {
                    candidate.artifacts.pop();
                }
                4 => candidate.raw_pages[0].text_items.clear(),
                5 => candidate.ocr_plan.pages.clear(),
                _ => unreachable!(),
            }
            let error = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap_err();
            assert!(
                error.to_string().contains("frozen integrity mismatch"),
                "mutation {mutation}: {error}"
            );
            assert_eq!(
                std::fs::read_dir(prepared.staging_root()).unwrap().count(),
                before
            );
        }
    }
}
