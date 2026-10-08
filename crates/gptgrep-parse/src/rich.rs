//! Count-admitted native stages. Opaque upstream rich getters remain explicit
//! capabilities until their length/count preflight can be implemented.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use gptgrep_pageindex::{Heading, PageInput, ParsedDocument, from_pages, locate_heading};
use image::ImageEncoder;
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

pub const RICH_PARSER: &str = "liteparse-2.15.1/rich-v1";
static PREPARATION_ID: AtomicU64 = AtomicU64::new(0);

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NativeMetadata {
    page: Page,
    objects: Vec<serde_json::Value>,
    capabilities: std::collections::BTreeMap<String, FeatureState<String>>,
    complexity: stages::PageComplexityStats,
    image_rects: Vec<liteparse::types::Rect>,
    geometry: PageGeometry,
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

fn admit_native_estimate(
    metadata_used: &mut u64,
    owned_upper: &mut u64,
    bytes: u64,
    budget: &ResourceBudget,
) -> Result<()> {
    let metadata = metadata_used
        .checked_add(bytes)
        .context("resource_limit: native metadata sum overflow")?;
    let owned = owned_upper
        .checked_add(
            bytes
                .checked_mul(4)
                .context("resource_limit: native working bound overflow")?,
        )
        .context("resource_limit: native working sum overflow")?;
    ensure!(
        metadata <= budget.document_metadata_bytes && owned <= budget.owned_working_bytes,
        "resource_limit: complete document metadata/working admission"
    );
    *metadata_used = metadata;
    *owned_upper = owned;
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
    let native_profile_sha256 = crate::document_metadata_native_profile_digest(
        &prepared.summary,
        options.dpi,
        options.render_pages,
    )?;
    let mut raw_pages = Vec::new();
    let mut pages = Vec::new();
    let mut artifacts = Vec::new();
    let mut tasks = Vec::new();
    let mut annotation_counts = Vec::new();
    let mut staged = prepared.summary.prepared_bytes;
    let mut metadata_used = 0u64;
    let mut owned_upper = 0u64;
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
        ensure!(
            total > 0 && total as u32 <= crate::MAX_NATIVE_PAGES,
            "native page count is outside supported complete-document limits"
        );
        let metadata = crate::document_metadata::extract(
            &doc,
            &prepared.summary,
            total as u32,
            &options,
            &budget,
            &mut |bytes| {
                admit_native_estimate(&mut metadata_used, &mut owned_upper, bytes, &budget)
            },
        )?;
        (total, metadata)
    };
    ensure!(
        total > 0 && total as u32 <= crate::MAX_NATIVE_PAGES,
        "native page count is outside supported complete-document limits"
    );
    let document_metadata_size = encoded_size(&document_metadata, budget.document_metadata_bytes)?;
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
    for start in (0..total as usize).step_by(options.page_window) {
        // This scope ends before the async extraction function returns. No
        // Document, Page, TextPage, Bitmap or Library appears in a public DTO.
        let lib = stages::Library::init();
        let doc = stages::open(&lib, &input, None, &[])?;
        ensure!(doc.page_count() == total, "prepared page count changed");
        for index in start..(start + options.page_window).min(total as usize) {
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
            // Per-glyph numeric DTOs have a fixed serialized/owned bound. This
            // adapter deliberately omits opaque variable-length font getters.
            let glyph_bound = (count as u64)
                .checked_mul(2048)
                .context("resource_limit: native char bound overflow")?;
            ensure!(
                glyph_bound <= budget.page_layout_bytes,
                "resource_limit: native char count rejected before glyph allocation"
            );
            let mut objects = Vec::new();
            let mut image_rects = Vec::new();
            let mut vector_count = 0usize;
            let mut stack = Vec::new();
            let mut visited = HashSet::new();
            let object_count = page_obj.object_count();
            ensure!(
                object_count as u64 <= budget.page_layout_bytes / 2048,
                "resource_limit: native object count rejected before object allocation"
            );
            let mut object_bound = object_count as u64 * 2048;
            let label_bound = match &document_metadata.page_labels[index].label {
                FeatureState::Present(label) => label.len() as u64 * 12,
                _ => 1024,
            };
            ensure!(
                object_bound + glyph_bound + annotation_bound + label_bound + 16_384
                    <= budget.page_layout_bytes,
                "resource_limit: page char/object admission"
            );
            admit_native_estimate(
                &mut metadata_used,
                &mut owned_upper,
                object_bound + glyph_bound + annotation_bound + 16_384,
                &budget,
            )?;
            for object_index in 0..object_count {
                stack.push((
                    page_obj
                        .object(object_index)
                        .context("PDF object disappeared")?,
                    0u32,
                ));
            }
            while let Some((object, depth)) = stack.pop() {
                ensure!(
                    depth < 32 && visited.insert(object.id()),
                    "capability_unavailable: cyclic/repeated/deep Form object tree"
                );
                if let Some(children) = object.form_object_count() {
                    let addition = (children as u64)
                        .checked_mul(2048)
                        .context("resource_limit: nested object multiplication overflow")?;
                    object_bound = object_bound
                        .checked_add(children as u64 * 2048)
                        .context("resource_limit: nested object count overflow")?;
                    ensure!(
                        object_bound + glyph_bound + annotation_bound + label_bound + 16_384
                            <= budget.page_layout_bytes,
                        "resource_limit: nested object admission"
                    );
                    admit_native_estimate(&mut metadata_used, &mut owned_upper, addition, &budget)?;
                    for child in 0..children {
                        stack.push((
                            object
                                .form_object(child)
                                .context("Form child disappeared")?,
                            depth + 1,
                        ));
                    }
                }
                if let Some(segments) = object.path_segment_count() {
                    let addition = (segments as u64)
                        .checked_mul(256)
                        .context("resource_limit: segment multiplication overflow")?;
                    object_bound = object_bound
                        .checked_add(segments as u64 * 256)
                        .context("resource_limit: path segment count overflow")?;
                    ensure!(
                        object_bound + glyph_bound + annotation_bound + label_bound + 16_384
                            <= budget.page_layout_bytes,
                        "resource_limit: path segment count rejected before allocation"
                    );
                    admit_native_estimate(&mut metadata_used, &mut owned_upper, addition, &budget)?;
                    let mut points = Vec::with_capacity(segments);
                    for segment in 0..segments {
                        let segment = object
                            .path_segment(segment)
                            .context("path segment unavailable")?;
                        if let Some((x, y)) = segment.point {
                            ensure!(x.is_finite() && y.is_finite(), "invalid path geometry");
                        }
                        points.push(serde_json::json!({"kind":format!("{:?}",segment.kind),"point":segment.point,"close":segment.close}));
                    }
                    let matrix = object.matrix().map(|m| [m.a, m.b, m.c, m.d, m.e, m.f]);
                    ensure!(
                        matrix
                            .as_ref()
                            .is_none_or(|m| m.iter().all(|v| v.is_finite())),
                        "invalid path matrix"
                    );
                    objects.push(serde_json::json!({"kind":"path","segments":points,"matrix":matrix,"depth":depth}));
                    vector_count += 1;
                } else if let Some(image) = object.image_metadata() {
                    let bbox = object
                        .bounds()
                        .context("capability_unavailable: image bounds missing")?;
                    ensure!(
                        depth == 0,
                        "capability_unavailable: nested image geometry requires ancestor matrix adapter"
                    );
                    let corners = [
                        native_transform.transform_point(bbox.left, bbox.top),
                        native_transform.transform_point(bbox.right, bbox.top),
                        native_transform.transform_point(bbox.left, bbox.bottom),
                        native_transform.transform_point(bbox.right, bbox.bottom),
                    ];
                    ensure!(
                        corners.iter().all(|(x, y)| x.is_finite() && y.is_finite()),
                        "invalid image object geometry"
                    );
                    let minx = corners.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
                    let miny = corners.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
                    let maxx = corners
                        .iter()
                        .map(|p| p.0)
                        .fold(f32::NEG_INFINITY, f32::max);
                    let maxy = corners
                        .iter()
                        .map(|p| p.1)
                        .fold(f32::NEG_INFINITY, f32::max);
                    image_rects.push(liteparse::types::Rect {
                        x: minx,
                        y: miny,
                        width: maxx - minx,
                        height: maxy - miny,
                    });
                    objects.push(serde_json::json!({"kind":"image","width":image.width,"height":image.height,"bits_per_pixel":image.bits_per_pixel,"depth":depth}));
                }
            }
            let mut items: Vec<TextItem> = Vec::with_capacity(count as usize);
            for ordinal in 0..count {
                let glyph = text_page.char_at_unchecked(ordinal);
                let unicode = glyph.unicode();
                let character = char::from_u32(unicode).unwrap_or('\u{fffd}');
                if character == '\r' || character == '\n' {
                    continue;
                }
                let Some(box_) = glyph.char_box() else {
                    ensure!(character.is_whitespace(), "native glyph missing geometry");
                    if let Some(previous) = items.last_mut() {
                        previous.text.push(character);
                    }
                    continue;
                };
                let (x1, y1) = native_transform.transform_point(box_.left as f32, box_.top as f32);
                let (x2, y2) =
                    native_transform.transform_point(box_.right as f32, box_.bottom as f32);
                let x = x1.min(x2);
                let y = y1.min(y2);
                let w = (x2 - x1).abs();
                let h = (y2 - y1).abs();
                ensure!(
                    [x, y, w, h].iter().all(|value| value.is_finite())
                        && x >= -1.0
                        && y >= -1.0
                        && x + w <= width + 1.0
                        && y + h <= height + 1.0,
                    "capability_unavailable: native glyph geometry outside viewport"
                );
                if w == 0.0 || h == 0.0 {
                    ensure!(
                        character.is_whitespace(),
                        "native glyph has zero-area bounds"
                    );
                    if let Some(previous) = items.last_mut() {
                        previous.text.push(character);
                    }
                    continue;
                }
                let font = glyph.font_size() as f32;
                ensure!(font.is_finite() && font > 0.0, "invalid native font size");
                items.push(TextItem {
                    text: character.to_string(),
                    x,
                    y,
                    width: w,
                    height: h,
                    font_size: Some(font),
                    font_weight: Some(glyph.font_weight()),
                    has_unicode_map_error: glyph.has_unicode_map_error(),
                    char_codes: vec![glyph.char_code()],
                    trailing_space_generated: glyph.is_generated(),
                    ..Default::default()
                });
            }
            let raw = Page {
                page_number: number as usize,
                page_label: match &document_metadata.page_labels[index].label {
                    FeatureState::Present(label) => Some(label.clone()),
                    _ => None,
                },
                page_width: width,
                page_height: height,
                content_bounds: None,
                text_items: glyph_runs(items),
                graphics: Vec::new(),
                vector_graphics: None,
                struct_nodes: Vec::new(),
                image_refs: Vec::new(),
                annotations: None,
                form_fields: None,
                structure_tree: None,
            };
            admit_projection(std::slice::from_ref(&raw), &budget, owned_upper)?;
            let projected = stages::project(vec![raw.clone()]);
            // Avoid upstream image_bounds()/vector Vec getters. Inputs to this
            // pure classifier were admitted by the object-count walk above.
            let mut complexity =
                liteparse::ocr_merge::calculate_native_page_complexity(&raw, &image_rects, 0, 1);
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

            for feature in [
                "font_names",
                "word_segmentation",
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
                FeatureState::Present(crate::DOCUMENT_METADATA_NATIVE_RECIPE.into()),
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
                    "count-admitted raw path segments/object matrices; ancestor geometry is unsupported".into(),
                ),
            );
            let name = format!("{}-p{number}-native.txt", prepared.summary.source_sha256);
            admit_staging(&mut staged, text.len() as u64, &budget)?;
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
            // Bind the original source-only classifier inputs/output and frame
            // in retained metadata. Admit the added held/serialized copy before
            // cloning complexity/geometry or constructing the metadata DTO.
            let binding_bytes = encoded_size(
                &(&complexity, &image_rects, &geometry),
                budget.page_layout_bytes,
            )?;
            ensure!(
                object_bound
                    + glyph_bound
                    + annotation_bound
                    + label_bound
                    + 16_384
                    + binding_bytes
                    <= budget.page_layout_bytes,
                "resource_limit: native metadata classifier/frame binding admission"
            );
            admit_native_estimate(&mut metadata_used, &mut owned_upper, binding_bytes, &budget)?;
            let metadata = NativeMetadata {
                page: raw.clone(),
                objects,
                capabilities: capabilities.clone(),
                complexity: complexity.clone(),
                image_rects,
                geometry: geometry.clone(),
            };
            let metadata_size = encoded_size(&metadata, budget.page_layout_bytes)?;
            admit_staging(&mut staged, metadata_size, &budget)?;
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
                ensure!(
                    owned_upper + pixels * 8 + budget.image_file_bytes
                        <= budget.owned_working_bytes,
                    "resource_limit: raster+metadata working admission"
                );
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
                let mut encoded = LimitedWriter {
                    inner: Vec::with_capacity(
                        usize::try_from(budget.image_file_bytes)
                            .context("image file limit is not addressable")?,
                    ),
                    limit: budget.image_file_bytes,
                    written: 0,
                };
                image::codecs::png::PngEncoder::new(&mut encoded).write_image(
                    &rgba,
                    pixel_width,
                    pixel_height,
                    image::ExtendedColorType::Rgba8,
                )?;
                budget.admit_image(pixel_width, pixel_height, encoded.written)?;
                admit_staging(&mut staged, encoded.written, &budget)?;
                let artifact = write_bytes(
                    &prepared.staging_root,
                    &format!("{}-p{number}.png", prepared.summary.source_sha256),
                    ArtifactBinding {
                        source: &prepared.summary.source_sha256,
                        profile: &native_profile_sha256,
                        kind: ArtifactKind::Rendition,
                        page: Some(number),
                    },
                    &encoded.inner,
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
        }
    }
    prepared.verify(&budget)?;
    let blockers = pages
        .iter()
        .filter(|page| page.coverage == CoverageState::RequiredOcr && page.renditions.is_empty())
        .map(|page| {
            format!(
                "page {} requires OCR but no admitted rendition was requested",
                page.number
            )
        })
        .collect();
    let plan = OcrPlan {
        tasks,
        pages: pages
            .iter()
            .map(|page| (page.number, page.coverage.clone()))
            .collect(),
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
        native_owned_upper: owned_upper,
        metadata_admitted_upper: metadata_used,
        execution_budget: budget,
        frozen_integrity_sha256: String::new(),
        annotation_counts,
        document_metadata: Some(document_metadata),
        document_metadata_artifact: Some(document_metadata_artifact),
    };
    candidate.frozen_integrity_sha256 = candidate.integrity_digest()?;
    Ok(candidate)
}

/// Preserve adjacent glyph runs before word-oriented projection. Font changes,
/// separate rows and large horizontal gaps retain independent layout items.
fn glyph_runs(items: Vec<TextItem>) -> Vec<TextItem> {
    let mut runs: Vec<TextItem> = Vec::with_capacity(items.len());
    for item in items {
        if let Some(last) = runs.last_mut() {
            let font = last.font_size.unwrap_or(0.0);
            let gap = item.x - (last.x + last.width);
            if last.font_size == item.font_size
                && last.font_weight == item.font_weight
                && last.y < item.y + item.height
                && item.y < last.y + last.height
                && item.x >= last.x
                && gap >= -font * 0.5
                && gap <= font
            {
                last.text.push_str(&item.text);
                last.char_codes.extend(item.char_codes);
                last.has_unicode_map_error |= item.has_unicode_map_error;
                last.trailing_space_generated = item.trailing_space_generated;
                let right = (last.x + last.width).max(item.x + item.width);
                let bottom = (last.y + last.height).max(item.y + item.height);
                last.y = last.y.min(item.y);
                last.width = right - last.x;
                last.height = bottom - last.y;
                continue;
            }
        }
        runs.push(item);
    }
    runs
}

/// Conservative bound for the pinned pure projection's sparse-space output.
/// Extremely thin text and huge coordinate spans need a limited projection
/// adapter rather than invoking an unbounded upstream space expansion.
fn admit_projection(pages: &[Page], budget: &ResourceBudget, held_upper: u64) -> Result<u64> {
    let mut total = 0u64;
    for page in pages {
        let mut smallest = f32::INFINITY;
        let mut chars = 0u64;
        for item in &page.text_items {
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
        let columns = (page.page_width.max(page.page_height) / smallest).ceil() as u64;
        let upper = chars
            .checked_mul(
                columns
                    .checked_add(64)
                    .context("projection column overflow")?,
            )
            .and_then(|v| v.checked_mul(4))
            .context("projection size overflow")?;
        ensure!(
            upper <= budget.page_layout_bytes,
            "resource_limit: projection upper bound rejected before pure stage allocation"
        );
        total = total
            .checked_add(upper)
            .context("projection document overflow")?;
    }
    ensure!(
        held_upper
            .checked_add(
                total
                    .checked_mul(4)
                    .context("projection working overflow")?
            )
            .context("projection held working overflow")?
            <= budget.owned_working_bytes,
        "resource_limit: complete projection working admission"
    );
    Ok(total)
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
    let outcome_bytes = encoded_size(&outcomes, budget.document_metadata_bytes)?;
    let final_owned_upper = candidate
        .native_owned_upper
        .checked_add(
            outcome_bytes
                .checked_mul(8)
                .context("resource_limit: OCR finalization working overflow")?,
        )
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
        page.text_items.clear();
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
    let projected_text_upper = admit_projection(&candidate.raw_pages, budget, final_owned_upper)?;
    // Reserve native records plus escaped text, line/block copies and OCR
    // records before the complete pure metadata stage. Keep the bound stricter
    // than the sink checks for persisted JSON below.
    let final_metadata_upper = candidate
        .metadata_admitted_upper
        .checked_mul(2)
        .and_then(|native| {
            projected_text_upper
                .checked_mul(8)
                .and_then(|projection| native.checked_add(projection))
        })
        .and_then(|total| total.checked_add(outcome_bytes))
        .context("resource_limit: final metadata bound overflow")?;
    ensure!(
        final_metadata_upper <= budget.document_metadata_bytes,
        "resource_limit: complete final metadata admission"
    );
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
    let retained_metadata = candidate
        .artifacts
        .iter()
        .filter(|artifact| {
            matches!(
                artifact.kind,
                ArtifactKind::NativeMetadata | ArtifactKind::OcrRegions
            )
        })
        .try_fold(layout_size, |total, artifact| {
            total.checked_add(artifact.bytes)
        })
        .context("resource_limit: retained metadata length overflow")?;
    ensure!(
        retained_metadata <= budget.document_metadata_bytes,
        "resource_limit: retained complete metadata"
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
