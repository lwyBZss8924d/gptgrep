use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::artifact::{
    ArtifactKind, ArtifactRef, artifact_id, hash_file, read_bounded, sha256, valid_digest,
    validate_relative,
};
use crate::geometry::{AffineTransform, BoundingBox};
use crate::resource::{ResourceBudget, encoded_size};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrProfile {
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}
impl Default for OcrProfile {
    fn default() -> Self {
        Self {
            model: "gpt-6-luna".into(),
            reasoning_effort: "max".into(),
            service_tier: "fast".into(),
        }
    }
}
pub fn ocr_profile_digest(profile: &OcrProfile) -> Result<String> {
    encoded_size(profile, 2048)?;
    Ok(sha256(&serde_json::to_vec(profile)?))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrTaskBudget {
    pub max_tool_calls: u32,
    pub max_seconds: u32,
    pub max_turns: u32,
    pub frame_bytes: u64,
    pub transport_bytes: u64,
    pub resources: ResourceBudget,
}
impl Default for OcrTaskBudget {
    fn default() -> Self {
        Self {
            max_tool_calls: 8,
            max_seconds: 180,
            max_turns: 1,
            frame_bytes: 4_194_304,
            transport_bytes: 16_777_216,
            resources: ResourceBudget::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrRegion {
    pub id: String,
    pub bbox: BoundingBox,
    pub required: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrImage {
    pub artifact: ArtifactRef,
    #[serde(skip)]
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub page_to_raster: AffineTransform,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrTask {
    pub schema_version: String,
    pub id: String,
    pub task_sha256: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    #[serde(skip)]
    pub source_path: PathBuf,
    pub page: u32,
    pub page_width_points: f64,
    pub page_height_points: f64,
    pub regions: Vec<OcrRegion>,
    pub images: Vec<OcrImage>,
    pub native_text: String,
    pub layout_context: String,
    pub profile: OcrProfile,
    pub budget: OcrTaskBudget,
}

pub fn ocr_task_digest(task: &OcrTask) -> Result<String> {
    let limit = task.budget.resources.native_layout_input_bytes + 65_536;
    encoded_size(task, limit)?;
    let mut binding = task.clone();
    binding.id.clear();
    binding.task_sha256.clear();
    encoded_size(&binding, limit)?;
    Ok(sha256(&serde_json::to_vec(&binding)?))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrResultStatus {
    Complete,
    Partial,
    Unreadable,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrRegionStatus {
    Complete,
    Unknown,
    Partial,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrRegionResult {
    pub id: String,
    pub bbox: BoundingBox,
    pub text: String,
    pub reading_order: u32,
    pub status: OcrRegionStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrImageDelivery {
    pub artifact_id: String,
    pub artifact_sha256: String,
    pub tool_ordinal: u32,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub tool_response_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrReceipt {
    pub model: String,
    pub profile_sha256: String,
    pub request_sha256: String,
    pub response_sha256: String,
    pub runtime_profile_sha256: String,
    pub image_deliveries: Vec<OcrImageDelivery>,
    pub usage: Option<serde_json::Value>,
    pub latency_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrPageResult {
    pub schema_version: String,
    pub task_id: String,
    pub task_sha256: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub page: u32,
    pub status: OcrResultStatus,
    pub regions: Vec<OcrRegionResult>,
    pub receipt: OcrReceipt,
}

/// Host implementations receive owned, admitted page resources. No PDFium handle crosses await.
pub trait OcrExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        task: OcrTask,
    ) -> Pin<Box<dyn Future<Output = Result<OcrPageResult>> + Send + 'a>>;
}

pub fn validate_ocr_task(task: &OcrTask) -> Result<()> {
    validate_ocr_task_metadata(task)?;
    for image in &task.images {
        validate_rendition(image, &task.budget.resources)?;
    }
    let (_, source) = hash_file(
        &task.source_path,
        task.budget.resources.document_staging_bytes,
    )?;
    ensure!(source == task.source_sha256, "OCR source changed");
    Ok(())
}

pub fn validate_rendition_metadata(image: &OcrImage, budget: &ResourceBudget) -> Result<()> {
    budget.admit_image(image.width, image.height, image.artifact.bytes)?;
    ensure!(
        image.artifact.kind == ArtifactKind::Rendition
            && image.artifact.page.is_some_and(|page| page > 0),
        "invalid rendition kind/page"
    );
    ensure!(
        valid_digest(&image.artifact.sha256) && valid_digest(&image.artifact.profile_sha256),
        "invalid rendition digest"
    );
    ensure!(
        image.artifact.mime == "image/png" || image.artifact.mime == "image/jpeg",
        "unsupported OCR image MIME"
    );
    validate_relative(&image.artifact.relative_path)?;
    image.page_to_raster.validate()?;
    Ok(())
}

/// Validate admitted image bytes and headers without decoding pixel content.
/// Hosts requiring race-resistant path custody should use the metadata helper
/// and read through their held directory descriptors instead.
pub fn validate_rendition(image: &OcrImage, budget: &ResourceBudget) -> Result<()> {
    validate_rendition_metadata(image, budget)?;
    let encoded = read_bounded(&image.path, image.artifact.bytes, &image.artifact.sha256)?;
    let reader = image::ImageReader::new(std::io::Cursor::new(&encoded)).with_guessed_format()?;
    let expected_format = if image.artifact.mime == "image/png" {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    ensure!(
        reader.format() == Some(expected_format),
        "OCR image MIME disagrees with bytes"
    );
    ensure!(
        reader.into_dimensions()? == (image.width, image.height),
        "OCR image dimensions disagree with bytes"
    );
    Ok(())
}

/// Validate portable task structure and identity without opening any path.
/// A transport must independently validate source and image bytes before use.
pub fn validate_ocr_task_metadata(task: &OcrTask) -> Result<()> {
    let budget = &task.budget.resources;
    budget.validate()?;
    encoded_size(task, budget.native_layout_input_bytes + 65_536)?;
    for digest in [
        &task.source_sha256,
        &task.prepared_sha256,
        &task.task_sha256,
    ] {
        ensure!(valid_digest(digest), "invalid OCR digest");
    }
    ensure!(
        task.schema_version == "gptgrep.ocr.task.v1" && task.page > 0,
        "invalid OCR task schema/page"
    );
    ensure!(
        task.profile.model == "gpt-6-luna"
            && task.profile.reasoning_effort == "max"
            && task.profile.service_tier == "fast",
        "OCR profile does not match selected model/effort/tier"
    );
    ensure!(
        task.budget.max_tool_calls > 0
            && task.budget.max_tool_calls <= 8
            && task.budget.max_seconds > 0
            && task.budget.max_seconds <= 180
            && task.budget.max_turns == 1
            && task.budget.frame_bytes > 0
            && task.budget.frame_bytes <= 4_194_304
            && task.budget.transport_bytes > 0
            && task.budget.transport_bytes <= 16_777_216,
        "OCR host budget exceeds frozen M0 limits"
    );
    ensure!(
        task.native_text.len() as u64 + task.layout_context.len() as u64
            <= budget.native_layout_input_bytes,
        "resource_limit: OCR native/layout input"
    );
    ensure!(
        !task.regions.is_empty() && task.regions.len() <= budget.ocr_regions,
        "resource_limit: OCR task regions"
    );
    let mut ids = BTreeSet::new();
    for region in &task.regions {
        ensure!(
            !region.id.is_empty() && ids.insert(&region.id),
            "duplicate/empty OCR region ID"
        );
        region
            .bbox
            .validate(task.page_width_points, task.page_height_points)?;
    }
    ensure!(
        !task.images.is_empty() && task.images.len() <= budget.images_per_page,
        "resource_limit: OCR page images"
    );
    let mut pixels = 0u64;
    let mut bytes = 0u64;
    let mut images = BTreeSet::new();
    for image in &task.images {
        validate_rendition_metadata(image, budget)?;
        ensure!(images.insert(&image.artifact.id), "duplicate OCR image ID");
        ensure!(
            image.artifact.mime == "image/png" || image.artifact.mime == "image/jpeg",
            "unsupported OCR image MIME"
        );
        ensure!(
            image.artifact.page == Some(task.page)
                && image.artifact.kind == ArtifactKind::Rendition
                && image.artifact.region_id.is_none(),
            "OCR image page mismatch"
        );
        ensure!(
            image.artifact.id
                == artifact_id(
                    &task.source_sha256,
                    &image.artifact.profile_sha256,
                    &image.artifact.kind,
                    image.artifact.page,
                    image.artifact.region_id.as_deref(),
                    &image.artifact.sha256
                )?,
            "OCR image artifact identity mismatch"
        );
        pixels = pixels
            .checked_add(budget.admit_image(image.width, image.height, image.artifact.bytes)?)
            .ok_or_else(|| anyhow::anyhow!("image pixel sum overflow"))?;
        bytes = bytes
            .checked_add(image.artifact.bytes)
            .ok_or_else(|| anyhow::anyhow!("image byte sum overflow"))?;
        image.page_to_raster.validate()?;
    }
    ensure!(
        pixels <= budget.image_pixels_per_page && bytes <= budget.image_file_bytes_per_page,
        "resource_limit: OCR page image totals"
    );
    let digest = ocr_task_digest(task)?;
    ensure!(
        digest == task.task_sha256 && task.id == format!("ocr-{digest}"),
        "OCR task digest mismatch"
    );
    Ok(())
}

pub fn validate_ocr_result(task: &OcrTask, result: &OcrPageResult) -> Result<()> {
    validate_ocr_task(task)?;
    validate_ocr_result_metadata(task, result)
}

/// Validate result lineage and required regions without opening task paths.
pub fn validate_ocr_result_metadata(task: &OcrTask, result: &OcrPageResult) -> Result<()> {
    validate_ocr_task_metadata(task)?;
    encoded_size(result, task.budget.resources.ocr_result_bytes)?;
    for digest in [
        &result.receipt.request_sha256,
        &result.receipt.response_sha256,
        &result.receipt.runtime_profile_sha256,
    ] {
        ensure!(valid_digest(digest), "invalid OCR receipt digest");
    }
    ensure!(
        result.schema_version == "gptgrep.ocr.result.v1"
            && result.task_id == task.id
            && result.task_sha256 == task.task_sha256
            && result.source_sha256 == task.source_sha256
            && result.prepared_sha256 == task.prepared_sha256
            && result.page == task.page,
        "OCR result lineage mismatch"
    );
    ensure!(
        result.receipt.model == task.profile.model
            && result.receipt.profile_sha256 == ocr_profile_digest(&task.profile)?,
        "OCR result model/profile mismatch"
    );
    ensure!(
        result.receipt.image_deliveries.len() <= task.budget.max_tool_calls as usize,
        "OCR image delivery count exceeds tool budget"
    );
    let mut previous_ordinal = 0;
    let mut delivered = BTreeSet::new();
    for delivery in &result.receipt.image_deliveries {
        ensure!(
            delivery.tool_ordinal > previous_ordinal
                && delivery.tool_ordinal <= task.budget.max_tool_calls,
            "OCR image delivery ordinals must be unique and ascending within tool budget"
        );
        previous_ordinal = delivery.tool_ordinal;
        ensure!(
            valid_digest(&delivery.tool_response_sha256),
            "invalid OCR delivery digest"
        );
        let image = task
            .images
            .iter()
            .find(|image| image.artifact.id == delivery.artifact_id)
            .ok_or_else(|| anyhow::anyhow!("unplanned OCR image delivery"))?;
        ensure!(
            delivery.artifact_sha256 == image.artifact.sha256
                && delivery.mime == image.artifact.mime
                && delivery.width == image.width
                && delivery.height == image.height,
            "OCR delivered image metadata mismatch"
        );
        delivered.insert(&delivery.artifact_id);
    }
    ensure!(
        task.images
            .iter()
            .all(|image| delivered.contains(&image.artifact.id)),
        "OCR required image delivery incomplete"
    );
    ensure!(
        result.regions.len() <= task.budget.resources.ocr_regions,
        "resource_limit: OCR result regions"
    );
    let mut ids = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for region in &result.regions {
        ensure!(
            ids.insert(&region.id) && orders.insert(region.reading_order),
            "duplicate OCR region/order"
        );
        let planned = task
            .regions
            .iter()
            .find(|planned| planned.id == region.id)
            .ok_or_else(|| anyhow::anyhow!("unplanned OCR region"))?;
        region
            .bbox
            .validate(task.page_width_points, task.page_height_points)?;
        ensure!(
            region.bbox == planned.bbox,
            "OCR region coordinates mismatch"
        );
        ensure!(
            region.text.len() as u64 <= task.budget.resources.ocr_region_text_bytes,
            "resource_limit: OCR region text"
        );
        if planned.required {
            ensure!(
                region.status == OcrRegionStatus::Complete && !region.text.trim().is_empty(),
                "required OCR region not complete"
            );
        }
    }
    ensure!(
        result.status == OcrResultStatus::Complete
            && task
                .regions
                .iter()
                .filter(|region| region.required)
                .all(|required| ids.contains(&required.id)),
        "required OCR page incomplete"
    );
    Ok(())
}
