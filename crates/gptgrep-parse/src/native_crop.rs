//! Trusted, source-derived supplemental native image. Never an OcrTask mutation.
use crate::artifact::{sha256, valid_digest};
use crate::resource::encoded_size;
use crate::{
    BoundingBox, CropRecipe, OcrTask, ResourceBudget, crop_rendition, validate_ocr_task,
    validate_ocr_task_metadata,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs::File, sync::Arc};

const SCHEMA: &str = "gptgrep.ocr.native-crop-proof.v1";
const PROOF_BYTES: u64 = 16_384;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOcrCropProof {
    pub schema_version: String,
    pub task_sha256: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub derived_artifact_id: String,
    pub png_sha256: String,
    pub png_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub recipe_sha256: String,
    pub recipe: CropRecipe,
}

/// No Deserialize, Serialize, public fields, mutable bytes or path registration.
/// Only the source-verified parser constructor can create this input.
pub struct NativeOcrCrop {
    png: Vec<u8>,
    proof: NativeOcrCropProof,
    // Buffer admission stays alive even if the caller times out while an owned
    // executor is cancelling/reaping. This owner grants no image identity.
    working_owner: Option<Arc<dyn Send + Sync>>,
    // Opaque toolbox lifetime binding; it grants no artifact or image identity.
    document_scope: Option<[u8; 32]>,
}
impl std::fmt::Debug for NativeOcrCrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeOcrCrop")
            .field("proof", &self.proof)
            .finish_non_exhaustive()
    }
}
impl NativeOcrCrop {
    pub fn png(&self) -> &[u8] {
        &self.png
    }
    pub fn proof(&self) -> &NativeOcrCropProof {
        &self.proof
    }
    /// Bind a source-verified crop to its originating typed document handle.
    /// The scope is not serialized and cannot replace the source proof.
    pub fn bind_document_scope(mut self, scope: [u8; 32]) -> Result<Self> {
        ensure!(
            self.document_scope.is_none() && scope.iter().any(|byte| *byte != 0),
            "native_crop_document_scope_already_bound_or_invalid"
        );
        self.document_scope = Some(scope);
        Ok(self)
    }
    pub fn verify_document_scope(&self, scope: &[u8; 32]) -> Result<()> {
        ensure!(
            self.document_scope.as_ref() == Some(scope),
            "native_crop_document_scope_changed"
        );
        Ok(())
    }
    /// Lifetime custody only. A lease must be reserved before construction;
    /// this cannot create or replace bytes, lineage, permissions or budgets.
    pub fn retain_working_owner<T: Send + Sync + 'static>(mut self, owner: Arc<T>) -> Result<Self> {
        ensure!(
            self.working_owner.is_none(),
            "native_crop_working_owner_already_bound"
        );
        self.working_owner = Some(owner);
        Ok(self)
    }
    pub fn validate_for_task(&self, task: &OcrTask) -> Result<()> {
        validate_native_crop_proof_metadata(task, &self.proof)?;
        ensure!(
            self.png.len() as u64 == self.proof.png_bytes
                && self.png.capacity() as u64 <= task.budget.resources.image_file_bytes
                && sha256(&self.png) == self.proof.png_sha256,
            "native_crop_owned_bytes_changed"
        );
        Ok(())
    }
}

fn derived_id(proof: &NativeOcrCropProof) -> Result<String> {
    let binding = (
        "gptgrep.native-crop-artifact.v1",
        &proof.task_sha256,
        &proof.source_sha256,
        &proof.prepared_sha256,
        &proof.recipe.parent_artifact_id,
        &proof.recipe_sha256,
        &proof.png_sha256,
        proof.png_bytes,
    );
    Ok(format!(
        "artifact-{}",
        sha256(&serde_json::to_vec(&binding)?)
    ))
}

/// Caller supplies the original registered task and its exclusively owned,
/// custody-checked regular parent handle. No caller crop proof is accepted.
pub fn create_native_ocr_crop(
    task: &OcrTask,
    parent_id: &str,
    source: File,
    selected: BoundingBox,
    budget: &ResourceBudget,
) -> Result<NativeOcrCrop> {
    validate_ocr_task_metadata(task)?;
    budget.ensure_same_invocation_within(&task.budget.resources)?;
    let image = task
        .images
        .iter()
        .find(|image| image.artifact.id == parent_id)
        .ok_or_else(|| anyhow::anyhow!("native_crop_unregistered_parent"))?;
    ensure!(
        task.images.len() < task.budget.resources.images_per_page,
        "native_crop_original_image_closure_full"
    );
    let output = crop_rendition(
        image,
        source,
        selected,
        (task.page_width_points, task.page_height_points),
        budget,
    )?;
    let mut proof = NativeOcrCropProof {
        schema_version: SCHEMA.into(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        derived_artifact_id: String::new(),
        png_sha256: output.sha256,
        png_bytes: output.png.len() as u64,
        width: output.recipe.pixels.width,
        height: output.recipe.pixels.height,
        recipe_sha256: output.recipe_sha256,
        recipe: output.recipe,
    };
    proof.derived_artifact_id = derived_id(&proof)?;
    validate_native_crop_proof_metadata(task, &proof)?;
    let crop = NativeOcrCrop {
        png: output.png,
        proof,
        working_owner: None,
        document_scope: None,
    };
    crop.validate_for_task(task)?;
    validate_native_crop_set(task, std::slice::from_ref(&crop))?;
    Ok(crop)
}

/// Portable lineage and exact source-derived transforms, without opening paths.
pub fn validate_native_crop_proof_metadata(
    task: &OcrTask,
    proof: &NativeOcrCropProof,
) -> Result<()> {
    validate_ocr_task_metadata(task)?;
    encoded_size(proof, PROOF_BYTES)?;
    ensure!(
        proof.schema_version == SCHEMA
            && proof.task_sha256 == task.task_sha256
            && proof.source_sha256 == task.source_sha256
            && proof.prepared_sha256 == task.prepared_sha256,
        "native_crop_original_task_binding"
    );
    ensure!(
        valid_digest(&proof.png_sha256) && valid_digest(&proof.recipe_sha256),
        "native_crop_digest"
    );
    let parent = task
        .images
        .iter()
        .find(|image| image.artifact.id == proof.recipe.parent_artifact_id)
        .ok_or_else(|| anyhow::anyhow!("native_crop_unregistered_parent"))?;
    let recipe = crate::crop::recipe_for(
        parent,
        proof.recipe.selected_page_bbox,
        (task.page_width_points, task.page_height_points),
    )?;
    ensure!(
        serde_json::to_value(&proof.recipe)? == serde_json::to_value(&recipe)?
            && proof.recipe_sha256 == sha256(&serde_json::to_vec(&recipe)?)
            && proof.recipe.parent_page == task.page
            && proof.width == recipe.pixels.width
            && proof.height == recipe.pixels.height,
        "native_crop_recipe_or_viewport_changed"
    );
    ensure!(proof.png_bytes > 0, "native_crop_empty_png");
    task.budget
        .resources
        .admit_image(proof.width, proof.height, proof.png_bytes)?;
    ensure!(
        proof.derived_artifact_id == derived_id(proof)?
            && !task
                .images
                .iter()
                .any(|image| image.artifact.id == proof.derived_artifact_id),
        "native_crop_artifact_spoof"
    );
    Ok(())
}

/// Full parser check re-encodes the exact crop of the original verified parent.
/// A JSON proof alone cannot establish that its digest represents derived pixels.
pub fn validate_native_crop_proof(task: &OcrTask, proof: &NativeOcrCropProof) -> Result<()> {
    validate_ocr_task(task)?;
    validate_native_crop_proof_metadata(task, proof)?;
    let parent = task
        .images
        .iter()
        .find(|image| image.artifact.id == proof.recipe.parent_artifact_id)
        .ok_or_else(|| anyhow::anyhow!("native_crop_unregistered_parent"))?;
    let budget = crate::crop::decoder_budget(parent, &task.budget.resources)?;
    let output = crop_rendition(
        parent,
        File::open(&parent.path)?,
        proof.recipe.selected_page_bbox,
        (task.page_width_points, task.page_height_points),
        &budget,
    )?;
    ensure!(
        output.sha256 == proof.png_sha256
            && output.png.len() as u64 == proof.png_bytes
            && output.recipe_sha256 == proof.recipe_sha256,
        "native_crop_derived_pixels_changed"
    );
    Ok(())
}

/// Original and supplemental images share the original frozen page caps.
pub fn validate_native_crop_set(task: &OcrTask, crops: &[NativeOcrCrop]) -> Result<()> {
    validate_ocr_task_metadata(task)?;
    let mut ids = BTreeSet::new();
    let mut parents = BTreeSet::new();
    let mut pixels = 0u64;
    let mut bytes = 0u64;
    for image in &task.images {
        pixels = pixels
            .checked_add(u64::from(image.width) * u64::from(image.height))
            .ok_or_else(|| anyhow::anyhow!("native_crop_page_overflow"))?;
        bytes = bytes
            .checked_add(image.artifact.bytes)
            .ok_or_else(|| anyhow::anyhow!("native_crop_page_overflow"))?;
    }
    for crop in crops {
        crop.validate_for_task(task)?;
        let proof = crop.proof();
        ensure!(
            ids.insert(&proof.derived_artifact_id)
                && parents.insert(&proof.recipe.parent_artifact_id),
            "native_crop_duplicate_supplement"
        );
        pixels = pixels
            .checked_add(u64::from(proof.width) * u64::from(proof.height))
            .ok_or_else(|| anyhow::anyhow!("native_crop_page_overflow"))?;
        bytes = bytes
            .checked_add(proof.png_bytes)
            .ok_or_else(|| anyhow::anyhow!("native_crop_page_overflow"))?;
    }
    ensure!(
        !task.images.is_empty()
            && task
                .images
                .len()
                .checked_add(crops.len())
                .is_some_and(|n| n <= task.budget.resources.images_per_page && n <= 8)
            && pixels <= task.budget.resources.image_pixels_per_page
            && bytes <= task.budget.resources.image_file_bytes_per_page,
        "native_crop_original_page_budget"
    );
    Ok(())
}

impl NativeOcrCropProof {
    /// Heap-only strings; embedded recipe/proof sizes belong to delivery slots.
    pub fn heap_working_bytes(&self) -> Result<u64> {
        let strings = [
            &self.schema_version,
            &self.task_sha256,
            &self.source_sha256,
            &self.prepared_sha256,
            &self.derived_artifact_id,
            &self.png_sha256,
            &self.recipe_sha256,
            &self.recipe.schema_version,
            &self.recipe.parent_artifact_id,
            &self.recipe.parent_sha256,
            &self.recipe.parent_profile_sha256,
            &self.recipe.decoder,
            &self.recipe.encoder,
            &self.recipe.pixel_rule,
        ];
        strings.iter().try_fold(0u64, |n, s| {
            n.checked_add(s.capacity() as u64)
                .ok_or_else(|| anyhow::anyhow!("native_crop_working_overflow"))
        })
    }
}

#[cfg(test)]
#[path = "native_crop_tests.rs"]
mod tests;
