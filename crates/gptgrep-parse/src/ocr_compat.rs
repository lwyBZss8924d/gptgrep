//! Image-only OCR compatibility. These results never establish document lineage
//! or authorize generation publication. Confidence is an uncalibrated model report.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{OcrImage, OcrProfile, OcrReceipt, OcrResultStatus, OcrTaskBudget};

pub const COMPATIBILITY_MODE: &str = "gptgrep.ocr.compat.image-region-confidence.v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityOcrTask {
    pub schema_version: String,
    pub task_sha256: String,
    pub image: OcrImage,
    pub language: String,
    pub profile: OcrProfile,
    pub budget: OcrTaskBudget,
}

impl CompatibilityOcrTask {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == COMPATIBILITY_MODE,
            "ocr_compatibility_mode"
        );
        self.budget.resources.validate()?;
        crate::validate_rendition_metadata(&self.image, &self.budget.resources)?;
        ensure!(
            self.image.artifact.page == Some(1),
            "ocr_compatibility_image_page"
        );
        ensure!(
            !self.language.is_empty()
                && self.language.len() <= 64
                && self
                    .language
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "ocr_compatibility_language"
        );
        ensure!(
            self.profile.model == "gpt-6-luna"
                && self.profile.reasoning_effort == "max"
                && self.profile.service_tier == "fast",
            "ocr_compatibility_profile"
        );
        ensure!(
            self.budget.max_turns == 1
                && (1..=8).contains(&self.budget.max_tool_calls)
                && (1..=180).contains(&self.budget.max_seconds)
                && (1..=4_194_304).contains(&self.budget.frame_bytes)
                && (1..=16_777_216).contains(&self.budget.transport_bytes),
            "ocr_compatibility_budget"
        );
        ensure!(
            self.task_sha256 == compatibility_task_digest(self)?,
            "ocr_compatibility_task_binding"
        );
        Ok(())
    }
}

pub fn compatibility_task_digest(task: &CompatibilityOcrTask) -> Result<String> {
    crate::resource::encoded_size(task, 16_384)?;
    let mut binding = task.clone();
    binding.task_sha256.clear();
    let bytes = serde_json::to_vec(&binding)?;
    ensure!(bytes.len() <= 16_384, "ocr_compatibility_task_limit");
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// The rectangle is the submitted image extent, not detected word geometry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityOcrRegion {
    pub text: String,
    pub bbox: [f32; 4],
    pub confidence: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityOcrResult {
    pub schema_version: String,
    pub task_sha256: String,
    pub image_sha256: String,
    pub language: String,
    pub mode_sha256: String,
    pub status: OcrResultStatus,
    pub regions: Vec<CompatibilityOcrRegion>,
    pub receipt: OcrReceipt,
}

pub fn validate_compatibility_result(
    task: &CompatibilityOcrTask,
    result: &CompatibilityOcrResult,
    mode_sha256: &str,
) -> Result<()> {
    task.validate()?;
    ensure!(
        result.schema_version == "gptgrep.ocr.compat.result.v1"
            && result.task_sha256 == task.task_sha256
            && result.image_sha256 == task.image.artifact.sha256
            && result.language == task.language
            && result.mode_sha256 == mode_sha256,
        "ocr_compatibility_result_binding"
    );
    ensure!(
        result.status == OcrResultStatus::Complete && result.regions.len() == 1,
        "ocr_compatibility_incomplete"
    );
    let region = &result.regions[0];
    ensure!(
        region.text.len() as u64 <= task.budget.resources.ocr_region_text_bytes
            && region.confidence.is_finite()
            && (0.0..=1.0).contains(&region.confidence)
            && region.bbox == [0.0, 0.0, task.image.width as f32, task.image.height as f32],
        "ocr_compatibility_region"
    );
    let receipt = &result.receipt;
    ensure!(
        receipt.model == task.profile.model
            && receipt.profile_sha256 == crate::ocr_profile_digest(&task.profile)?
            && receipt.image_deliveries.len() == 1
            && receipt.image_deliveries[0].native.is_none()
            && receipt.image_deliveries[0].artifact_id == task.image.artifact.id
            && receipt.image_deliveries[0].artifact_sha256 == task.image.artifact.sha256
            && receipt.image_deliveries[0].mime == task.image.artifact.mime
            && receipt.image_deliveries[0].width == task.image.width
            && receipt.image_deliveries[0].height == task.image.height
            && (1..=task.budget.max_tool_calls).contains(&receipt.image_deliveries[0].tool_ordinal),
        "ocr_compatibility_delivery"
    );
    for digest in [
        &receipt.request_sha256,
        &receipt.response_sha256,
        &receipt.runtime_profile_sha256,
        &receipt.image_deliveries[0].tool_response_sha256,
        &result.mode_sha256,
    ] {
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "ocr_compatibility_receipt_digest"
        );
    }
    crate::resource::encoded_size(result, task.budget.resources.ocr_result_bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AffineTransform, ArtifactKind, ArtifactRef, OcrImageDelivery};
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn task() -> CompatibilityOcrTask {
        let mut task = CompatibilityOcrTask {
            schema_version: COMPATIBILITY_MODE.into(),
            task_sha256: String::new(),
            image: OcrImage {
                artifact: ArtifactRef {
                    id: "image".into(),
                    kind: ArtifactKind::Rendition,
                    relative_path: "image.png".into(),
                    sha256: HASH.into(),
                    profile_sha256: HASH.into(),
                    bytes: 80,
                    mime: "image/png".into(),
                    page: Some(1),
                    region_id: None,
                },
                path: "image.png".into(),
                width: 2,
                height: 3,
                page_to_raster: AffineTransform::IDENTITY,
            },
            language: "en".into(),
            profile: OcrProfile::default(),
            budget: OcrTaskBudget::default(),
        };
        task.task_sha256 = compatibility_task_digest(&task).unwrap();
        task
    }
    fn result(task: &CompatibilityOcrTask) -> CompatibilityOcrResult {
        CompatibilityOcrResult {
            schema_version: "gptgrep.ocr.compat.result.v1".into(),
            task_sha256: task.task_sha256.clone(),
            image_sha256: HASH.into(),
            language: task.language.clone(),
            mode_sha256: HASH.into(),
            status: OcrResultStatus::Complete,
            regions: vec![CompatibilityOcrRegion {
                text: "Synthetic text".into(),
                bbox: [0.0, 0.0, 2.0, 3.0],
                confidence: 0.73,
            }],
            receipt: OcrReceipt {
                model: task.profile.model.clone(),
                profile_sha256: crate::ocr_profile_digest(&task.profile).unwrap(),
                runtime_profile_sha256: HASH.into(),
                request_sha256: HASH.into(),
                response_sha256: HASH.into(),
                usage: None,
                latency_ms: None,
                image_deliveries: vec![OcrImageDelivery {
                    artifact_id: "image".into(),
                    artifact_sha256: HASH.into(),
                    tool_ordinal: 1,
                    mime: "image/png".into(),
                    width: 2,
                    height: 3,
                    tool_response_sha256: HASH.into(),
                    native: None,
                }],
            },
        }
    }
    #[test]
    fn compatibility_confidence_is_required_and_never_defaulted() {
        for raw in [
            r#"{"text":"x","bbox":[0,0,2,3]}"#,
            r#"{"text":"x","bbox":[0,0,2,3],"confidence":null}"#,
        ] {
            assert!(serde_json::from_str::<CompatibilityOcrRegion>(raw).is_err());
        }
        let task = task();
        let mut result = result(&task);
        validate_compatibility_result(&task, &result, HASH).unwrap();
        result.regions[0].confidence = 0.0;
        validate_compatibility_result(&task, &result, HASH).unwrap(); // actual zero is retained, not uplifted
        for value in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            result.regions[0].confidence = value;
            assert!(validate_compatibility_result(&task, &result, HASH).is_err());
        }
    }
    #[test]
    fn compatibility_language_extent_delivery_and_partial_are_bound() {
        let task = task();
        let result = result(&task);
        let mut changed = task.clone();
        changed.language = "fr".into();
        assert!(changed.validate().is_err());
        changed.task_sha256 = compatibility_task_digest(&changed).unwrap();
        changed.validate().unwrap();
        assert!(validate_compatibility_result(&changed, &result, HASH).is_err());
        for mutate in [0, 1, 2, 3] {
            let mut changed = result.clone();
            match mutate {
                0 => changed.status = OcrResultStatus::Partial,
                1 => changed.regions[0].bbox[2] = 1.0,
                2 => changed.receipt.image_deliveries.clear(),
                _ => changed.regions[0].text = "x".repeat(16_385),
            };
            assert!(validate_compatibility_result(&task, &changed, HASH).is_err());
        }
    }
    #[test]
    fn legacy_region_profile_and_shared_staging_remain_separate() {
        let region = crate::OcrRegionResult {
            id: "r".into(),
            reading_order: 0,
            text: "raw".into(),
            bbox: crate::BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            status: crate::OcrRegionStatus::Complete,
        };
        assert!(
            serde_json::to_value(region)
                .unwrap()
                .get("confidence")
                .is_none()
        );
        let original = crate::ResourceBudget::default();
        let mut restored = crate::ResourceBudget::default();
        let before = serde_json::to_vec(&restored).unwrap();
        restored.adopt_shared_staging(&original).unwrap();
        assert_eq!(before, serde_json::to_vec(&restored).unwrap());
        restored.reserve_retained_staging(123).unwrap();
        assert_eq!(original.staging_used(), 123);
    }
}
