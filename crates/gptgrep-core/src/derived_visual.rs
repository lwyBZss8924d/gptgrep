//! Generation-external, process-local PNG derivatives. Cache residency grants
//! neither actual image delivery nor citation rights. No file is written.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail, ensure};
use gptgrep_parse::{
    BoundingBox, CropAdmission, CropOutput, CropRecipe, ResourceBudget, crop_rendition,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::read_cache::{CacheError, CacheKey, ReadCache, ReadMeter, ReadOrigin, ReadPin};
use crate::{
    EvidenceRef, GenerationPin, VerifiedRenditionFile, VisualCandidate, VisualGenerationReader,
};

const MAX_RESPONSE: usize = 1_048_576;
const MAX_METADATA: usize = 16_384;
// Serialized receipt, typed strings/geometry and bounded registry/key overhead.
// This is an owned-object admission envelope, not allocator/native RSS.
const METADATA_CHARGE: usize = 131_072;
const TRANSIENT_WORKING: u64 = 262_144;
const WIRE_OVERHEAD: usize = 512;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedCacheBudget {
    pub payload_bytes: usize,
    pub metadata_bytes: usize,
    pub entries: usize,
    pub working_bytes: u64,
}
impl Default for DerivedCacheBudget {
    fn default() -> Self {
        Self {
            payload_bytes: 16_777_216,
            metadata_bytes: 4_194_304,
            entries: 32,
            working_bytes: 268_435_456,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualSource {
    Manifest,
    Derived,
}

/// Portable identity of a derived image. This is not reader-issued evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedVisualReceipt {
    pub schema_version: String,
    pub origin: VisualSource,
    pub parent_origin: VisualSource,
    pub derivative_id: String,
    pub generation: GenerationPin,
    pub parent: VisualCandidate,
    pub parent_binding_sha256: String,
    pub resource_profile_sha256: String,
    pub recipe: CropRecipe,
    pub recipe_sha256: String,
    pub mime: String,
    pub encoded_bytes: u64,
    pub output_sha256: String,
    pub width: u32,
    pub height: u32,
    pub admission: CropAdmission,
    pub actual_image_delivery: bool,
}

struct MetadataCapacity {
    bytes: usize,
    entries: usize,
    retained: Mutex<(usize, usize)>,
}
struct MetadataCharge(Arc<MetadataCapacity>);
impl Drop for MetadataCharge {
    fn drop(&mut self) {
        let mut retained = self.0.retained.lock().unwrap_or_else(|e| e.into_inner());
        retained.0 -= METADATA_CHARGE;
        retained.1 -= 1;
    }
}
struct WorkingCapacity {
    maximum: u64,
    retained: Mutex<u64>,
}
struct WorkingCharge {
    capacity: Arc<WorkingCapacity>,
    bytes: u64,
}
impl Drop for WorkingCharge {
    fn drop(&mut self) {
        let mut retained = self
            .capacity
            .retained
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *retained -= self.bytes;
    }
}
struct Metadata {
    receipt: DerivedVisualReceipt,
    json: Vec<u8>,
    sha256: String,
    request_key: String,
    cache_key: CacheKey,
    envelope_upper: u64,
    _charge: MetadataCharge,
}
struct RegistryEntry {
    metadata: Arc<Metadata>,
    touched: u64,
}
#[derive(Default)]
struct Registry {
    entries: BTreeMap<String, RegistryEntry>,
    clock: u64,
}
impl Registry {
    fn tick(&mut self) -> Result<u64> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("derived_clock_overflow"))?;
        Ok(self.clock)
    }
    fn evict(&mut self) -> bool {
        let key = self
            .entries
            .iter()
            .min_by_key(|(key, value)| (value.touched, *key))
            .map(|(key, _)| key.clone());
        key.is_some_and(|key| self.entries.remove(&key).is_some())
    }
}

#[derive(Clone)]
pub struct DerivedVisualCache {
    payload: ReadCache,
    metadata: Arc<MetadataCapacity>,
    working: Arc<WorkingCapacity>,
    registry: Arc<Mutex<Registry>>,
    meter: ReadMeter,
}

/// An opaque, same-cache recipe token. Clones retain the original metadata
/// reservation. Public receipt reconstruction uses rebuild(), which validates
/// against an owned parent and a locally generated recipe/output instead.
#[derive(Clone)]
pub struct DerivedRecipeToken {
    metadata: Arc<Metadata>,
}

/// No Debug/Serialize and no unrestricted owned-byte-copy method. Each read
/// revalidates the held parent. Clones retain immutable PNG and metadata pins.
#[derive(Clone)]
pub struct DerivedVisualPin {
    bytes: ReadPin,
    metadata: Arc<Metadata>,
    custody: VisualGenerationReader,
    crop_executed: bool,
    cache_hit: bool,
}
impl DerivedVisualPin {
    pub fn receipt(&self) -> &DerivedVisualReceipt {
        &self.metadata.receipt
    }
    pub fn recipe_token(&self) -> DerivedRecipeToken {
        DerivedRecipeToken {
            metadata: self.metadata.clone(),
        }
    }
    pub fn crop_executed(&self) -> bool {
        self.crop_executed
    }
    pub fn cache_hit(&self) -> bool {
        self.cache_hit
    }
    pub fn envelope_upper_bytes(&self) -> u64 {
        self.metadata.envelope_upper
    }
    pub fn read(&self, resources: &ResourceBudget, max_response_bytes: usize) -> Result<&[u8]> {
        validate_metadata(&self.metadata, resources, max_response_bytes)?;
        ensure!(
            self.bytes.len() as u64 == self.metadata.receipt.encoded_bytes
                && hash(&self.bytes) == self.metadata.receipt.output_sha256,
            "derived_payload_changed"
        );
        verify_parent(&self.custody, &self.metadata.receipt, resources)?;
        Ok(&self.bytes)
    }
}

#[derive(Debug, Serialize)]
pub struct DerivedCacheStatus {
    pub payload_capacity_bytes: usize,
    pub payload_resident_bytes: usize,
    pub payload_retained_bytes: usize,
    pub payload_resident_entries: usize,
    pub payload_retained_entries: usize,
    pub metadata_reserved_bytes: usize,
    pub metadata_retained_entries: usize,
    pub registered_recipes: usize,
    pub working_reserved_bytes: u64,
}

impl DerivedVisualCache {
    pub fn new(budget: DerivedCacheBudget) -> Result<Self> {
        ensure!(
            (1..=67_108_864).contains(&budget.payload_bytes)
                && (METADATA_CHARGE..=67_108_864).contains(&budget.metadata_bytes)
                && (1..=256).contains(&budget.entries)
                && (1..=268_435_456).contains(&budget.working_bytes),
            "derived_cache_budget_rejected"
        );
        Ok(Self {
            payload: ReadCache::new(budget.payload_bytes, budget.entries).map_err(cache_error)?,
            metadata: Arc::new(MetadataCapacity {
                bytes: budget.metadata_bytes,
                entries: budget.entries,
                retained: Mutex::new((0, 0)),
            }),
            working: Arc::new(WorkingCapacity {
                maximum: budget.working_bytes,
                retained: Mutex::new(0),
            }),
            registry: Arc::new(Mutex::new(Registry::default())),
            meter: ReadMeter::default(),
        })
    }

    pub fn status(&self) -> Result<DerivedCacheStatus> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_registry_poisoned"))?;
        let meta = self
            .metadata
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_metadata_poisoned"))?;
        let working = self
            .working
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_working_poisoned"))?;
        let payload = self.payload.stats().map_err(cache_error)?;
        Ok(DerivedCacheStatus {
            payload_capacity_bytes: payload.capacity_bytes,
            payload_resident_bytes: payload.resident_bytes,
            payload_retained_bytes: payload.retained_bytes,
            payload_resident_entries: payload.resident_entries,
            payload_retained_entries: payload.retained_entries,
            metadata_reserved_bytes: meta.0,
            metadata_retained_entries: meta.1,
            registered_recipes: registry.entries.len(),
            working_reserved_bytes: *working,
        })
    }

    /// Removes resident entries; in-flight work is not cancelled, and existing
    /// pins/tokens keep their original reservations until their last clone drops.
    pub fn clear(&self) -> Result<()> {
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_registry_poisoned"))?
            .entries
            .clear();
        self.payload.clear().map_err(cache_error)
    }

    pub fn derive(
        &self,
        parent: VerifiedRenditionFile,
        selected: BoundingBox,
        resources: &ResourceBudget,
        max_response_bytes: usize,
    ) -> Result<DerivedVisualPin> {
        let _working = self.reserve_working(resources)?;
        let effective = crop_budget(resources, max_response_bytes)?;
        effective.admit_image(
            parent.candidate().width,
            parent.candidate().height,
            parent.candidate().encoded_bytes,
        )?;
        let parent_sha = hash_json(parent.candidate())?;
        selected.validate(
            parent.candidate().page_geometry.width_points,
            parent.candidate().page_geometry.height_points,
        )?;
        ensure!(
            parent.candidate().mime == "image/png",
            "capability_unavailable: derived cache requires qualified PNG parent"
        );
        let request_key = request_key(&parent_sha, selected, resources)?;
        if let Some(metadata) = self.lookup(&request_key)? {
            return self.resume_inner(parent, metadata, resources, &effective, max_response_bytes);
        }
        // Reserve typed/serialized metadata and an entry before cloning a
        // parent, constructing a recipe, or retaining/copying a crop payload.
        let charge = self.reserve_metadata()?;
        let fresh = fresh_parent(parent, resources, &parent_sha)?;
        let output = crop_rendition(
            &fresh.image,
            fresh.file,
            selected,
            (
                fresh.candidate.page_geometry.width_points,
                fresh.candidate.page_geometry.height_points,
            ),
            &effective,
        )
        .map_err(|_| anyhow::anyhow!("derived_qualified_crop_failed"))?;
        let receipt = receipt(&fresh.candidate, &output, resources)?;
        admit_copy(&receipt, resources)?;
        let json = json_bytes(&receipt)?;
        let sha256 = hash(&json);
        let cache_key = CacheKey::new(
            &generation_key(&receipt.generation),
            &receipt.derivative_id,
            &receipt.output_sha256,
        )
        .map_err(cache_error)?;
        let envelope_upper = envelope_upper(&receipt)?;
        ensure!(
            envelope_upper <= max_response_bytes as u64,
            "derived_response_limit_rejected"
        );
        let metadata = self.register(Arc::new(Metadata {
            receipt,
            json,
            sha256,
            request_key,
            cache_key,
            envelope_upper,
            _charge: charge,
        }))?;
        validate_metadata(&metadata, resources, max_response_bytes)?;
        let cached = self
            .payload
            .get_or_load(
                &metadata.cache_key,
                metadata.receipt.encoded_bytes as usize,
                &self.meter,
                |buffer, _| -> Result<usize> {
                    ensure!(
                        buffer.len() == output.png.len(),
                        "derived_payload_length_rejected"
                    );
                    buffer.copy_from_slice(&output.png);
                    Ok(buffer.len())
                },
            )
            .map_err(cache_error)?;
        verify_parent(&fresh.custody, &metadata.receipt, resources)?;
        let pin = DerivedVisualPin {
            bytes: cached.bytes,
            metadata,
            custody: fresh.custody,
            crop_executed: true,
            cache_hit: cached.origin == ReadOrigin::Cache,
        };
        pin.read(resources, max_response_bytes)?;
        Ok(pin)
    }

    pub fn resume(
        &self,
        parent: VerifiedRenditionFile,
        token: &DerivedRecipeToken,
        resources: &ResourceBudget,
        max_response_bytes: usize,
    ) -> Result<DerivedVisualPin> {
        let _working = self.reserve_working(resources)?;
        ensure!(
            Arc::ptr_eq(&self.metadata, &token.metadata._charge.0),
            "derived_recipe_cache_owner_mismatch"
        );
        let effective = crop_budget(resources, max_response_bytes)?;
        self.resume_inner(
            parent,
            token.metadata.clone(),
            resources,
            &effective,
            max_response_bytes,
        )
    }

    /// Portable receipts are untrusted inputs. Even a cache hit must match the
    /// entire independently constructed receipt for the supplied owned parent.
    /// A different generation is rejected rather than reinterpreted as CURRENT.
    pub fn rebuild(
        &self,
        parent: VerifiedRenditionFile,
        expected: &DerivedVisualReceipt,
        resources: &ResourceBudget,
        max_response_bytes: usize,
    ) -> Result<DerivedVisualPin> {
        ensure!(
            hash_json(parent.candidate())? == expected.parent_binding_sha256
                && candidate_pin(parent.candidate())? == expected.generation,
            "derived_parent_binding_rejected"
        );
        let expected_sha = hash_json(expected)?;
        let pin = self.derive(
            parent,
            expected.recipe.selected_page_bbox,
            resources,
            max_response_bytes,
        )?;
        ensure!(
            pin.metadata.sha256 == expected_sha,
            "derived_recipe_rebuild_mismatch"
        );
        Ok(pin)
    }

    fn resume_inner(
        &self,
        parent: VerifiedRenditionFile,
        metadata: Arc<Metadata>,
        resources: &ResourceBudget,
        effective: &ResourceBudget,
        max_response_bytes: usize,
    ) -> Result<DerivedVisualPin> {
        validate_metadata(&metadata, resources, max_response_bytes)?;
        let fresh = fresh_parent(parent, resources, &metadata.receipt.parent_binding_sha256)?;
        ensure!(
            candidate_pin(&fresh.candidate)? == metadata.receipt.generation,
            "derived_generation_rejected"
        );
        let metadata = self.register(metadata)?;
        let mut crop_executed = false;
        let mut admission_rejected = false;
        let cached = self
            .payload
            .get_or_load(
                &metadata.cache_key,
                metadata.receipt.encoded_bytes as usize,
                &self.meter,
                |buffer, _| -> Result<usize> {
                    // Only a miss has an admitted fill slice alive during
                    // decoding. A resident immutable PNG needs no such slice.
                    let Some(working) = effective
                        .owned_working_bytes
                        .checked_sub(metadata.receipt.encoded_bytes)
                        .filter(|working| {
                            metadata.receipt.admission.owned_working_upper_bytes <= *working
                        })
                    else {
                        admission_rejected = true;
                        bail!("derived_resume_working_rejected");
                    };
                    let mut effective = effective.clone();
                    effective.owned_working_bytes = working;
                    effective.validate()?;
                    crop_executed = true;
                    let output = crop_rendition(
                        &fresh.image,
                        fresh.file,
                        metadata.receipt.recipe.selected_page_bbox,
                        (
                            fresh.candidate.page_geometry.width_points,
                            fresh.candidate.page_geometry.height_points,
                        ),
                        &effective,
                    )
                    .map_err(|_| anyhow::anyhow!("derived_qualified_crop_failed"))?;
                    ensure!(
                        hash_json(&receipt(&fresh.candidate, &output, resources)?)?
                            == metadata.sha256
                            && output.png.len() == buffer.len(),
                        "derived_recipe_rebuild_mismatch"
                    );
                    buffer.copy_from_slice(&output.png);
                    Ok(buffer.len())
                },
            )
            .map_err(|error| {
                if admission_rejected {
                    anyhow::anyhow!("derived_resume_working_rejected")
                } else {
                    cache_error(error)
                }
            })?;
        verify_parent(&fresh.custody, &metadata.receipt, resources)?;
        let pin = DerivedVisualPin {
            bytes: cached.bytes,
            metadata,
            custody: fresh.custody,
            crop_executed,
            cache_hit: cached.origin == ReadOrigin::Cache,
        };
        pin.read(resources, max_response_bytes)?;
        Ok(pin)
    }

    fn lookup(&self, key: &str) -> Result<Option<Arc<Metadata>>> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_registry_poisoned"))?;
        let touched = registry.tick()?;
        Ok(registry.entries.get_mut(key).map(|entry| {
            entry.touched = touched;
            entry.metadata.clone()
        }))
    }
    fn register(&self, metadata: Arc<Metadata>) -> Result<Arc<Metadata>> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_registry_poisoned"))?;
        let touched = registry.tick()?;
        if let Some(existing) = registry.entries.get_mut(&metadata.request_key) {
            ensure!(
                existing.metadata.sha256 == metadata.sha256,
                "derived_recipe_collision"
            );
            existing.touched = touched;
            return Ok(existing.metadata.clone());
        }
        ensure!(
            registry.entries.len() < self.metadata.entries,
            "derived_registry_capacity_rejected"
        );
        registry.entries.insert(
            metadata.request_key.clone(),
            RegistryEntry {
                metadata: metadata.clone(),
                touched,
            },
        );
        Ok(metadata)
    }
    fn reserve_metadata(&self) -> Result<MetadataCharge> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_registry_poisoned"))?;
        loop {
            {
                let mut retained = self
                    .metadata
                    .retained
                    .lock()
                    .map_err(|_| anyhow::anyhow!("derived_metadata_poisoned"))?;
                let bytes = retained
                    .0
                    .checked_add(METADATA_CHARGE)
                    .ok_or_else(|| anyhow::anyhow!("derived_metadata_overflow"))?;
                let entries = retained
                    .1
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("derived_metadata_overflow"))?;
                if bytes <= self.metadata.bytes && entries <= self.metadata.entries {
                    *retained = (bytes, entries);
                    return Ok(MetadataCharge(self.metadata.clone()));
                }
            }
            if !registry.evict() {
                bail!("derived_metadata_capacity_retained");
            }
        }
    }
    fn reserve_working(&self, resources: &ResourceBudget) -> Result<WorkingCharge> {
        resources.validate()?;
        let mut retained = self
            .working
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("derived_working_poisoned"))?;
        let total = retained
            .checked_add(resources.owned_working_bytes)
            .ok_or_else(|| anyhow::anyhow!("derived_working_overflow"))?;
        ensure!(
            resources.owned_working_bytes > TRANSIENT_WORKING && total <= self.working.maximum,
            "derived_working_capacity_rejected"
        );
        *retained = total;
        Ok(WorkingCharge {
            capacity: self.working.clone(),
            bytes: resources.owned_working_bytes,
        })
    }
}

struct FreshParent {
    candidate: VisualCandidate,
    image: gptgrep_parse::OcrImage,
    file: std::fs::File,
    custody: VisualGenerationReader,
}
fn fresh_parent(
    parent: VerifiedRenditionFile,
    resources: &ResourceBudget,
    expected: &str,
) -> Result<FreshParent> {
    ensure!(
        hash_json(parent.candidate())? == expected,
        "derived_parent_binding_rejected"
    );
    let candidate = parent.candidate().clone();
    let (_, original_file, custody) = parent.into_crop_parts()?;
    drop(original_file);
    let id = parent_id(&candidate)?;
    let current = custody.open_rendition(id, resources)?;
    ensure!(
        hash_json(current.candidate())? == expected,
        "derived_parent_binding_rejected"
    );
    let (image, file, custody) = current.into_crop_parts()?;
    Ok(FreshParent {
        candidate,
        image,
        file,
        custody,
    })
}
fn verify_parent(
    custody: &VisualGenerationReader,
    receipt: &DerivedVisualReceipt,
    resources: &ResourceBudget,
) -> Result<()> {
    ensure!(
        custody.pin() == receipt.generation,
        "derived_generation_rejected"
    );
    let parent = custody.open_rendition(parent_id(&receipt.parent)?, resources)?;
    ensure!(
        hash_json(parent.candidate())? == receipt.parent_binding_sha256,
        "derived_parent_binding_rejected"
    );
    Ok(())
}
fn parent_id(parent: &VisualCandidate) -> Result<&str> {
    match &parent.evidence {
        EvidenceRef::VisualRegion { rendition_id, .. } => Ok(rendition_id),
        _ => bail!("derived_visual_parent_required"),
    }
}
fn candidate_pin(parent: &VisualCandidate) -> Result<GenerationPin> {
    match &parent.evidence {
        EvidenceRef::VisualRegion { generation, .. } => Ok(generation.clone()),
        _ => bail!("derived_visual_parent_required"),
    }
}
fn receipt(
    parent: &VisualCandidate,
    output: &CropOutput,
    resources: &ResourceBudget,
) -> Result<DerivedVisualReceipt> {
    let generation = candidate_pin(parent)?;
    let parent_binding_sha256 = hash_json(parent)?;
    let resource_profile_sha256 = hash_json(resources)?;
    ensure!(
        output.recipe.parent_artifact_id == parent_id(parent)?
            && output.recipe_sha256 == hash_json(&output.recipe)?
            && output.sha256 == hash(&output.png),
        "derived_crop_binding_rejected"
    );
    let derivative_id = derivative_id(
        &generation,
        &parent_binding_sha256,
        &resource_profile_sha256,
        &output.recipe_sha256,
        &output.sha256,
    )?;
    Ok(DerivedVisualReceipt {
        schema_version: "gptgrep.visual.derived.v1".into(),
        origin: VisualSource::Derived,
        parent_origin: VisualSource::Manifest,
        derivative_id,
        generation,
        parent: parent.clone(),
        parent_binding_sha256,
        resource_profile_sha256,
        recipe: output.recipe.clone(),
        recipe_sha256: output.recipe_sha256.clone(),
        mime: "image/png".into(),
        encoded_bytes: output.png.len() as u64,
        output_sha256: output.sha256.clone(),
        width: output.recipe.pixels.width,
        height: output.recipe.pixels.height,
        admission: output.admission.clone(),
        actual_image_delivery: false,
    })
}
fn validate_metadata(
    meta: &Metadata,
    resources: &ResourceBudget,
    max_response: usize,
) -> Result<()> {
    let effective = crop_budget(resources, max_response)?;
    let r = &meta.receipt;
    ensure!(
        meta.sha256 == hash_json(r)?
            && meta.sha256 == hash(&meta.json)
            && r.schema_version == "gptgrep.visual.derived.v1"
            && r.origin == VisualSource::Derived
            && r.parent_origin == VisualSource::Manifest
            && !r.actual_image_delivery
            && r.mime == "image/png"
            && r.parent_binding_sha256 == hash_json(&r.parent)?
            && r.generation == candidate_pin(&r.parent)?
            && r.resource_profile_sha256 == hash_json(resources)?
            && r.recipe_sha256 == hash_json(&r.recipe)?
            && r.derivative_id
                == derivative_id(
                    &r.generation,
                    &r.parent_binding_sha256,
                    &r.resource_profile_sha256,
                    &r.recipe_sha256,
                    &r.output_sha256
                )?
            && r.encoded_bytes > 0
            && r.encoded_bytes <= effective.image_file_bytes
            && r.admission.encoded_output_upper_bytes <= effective.image_file_bytes
            && r.admission.owned_working_upper_bytes <= effective.owned_working_bytes
            && r.width == r.recipe.pixels.width
            && r.height == r.recipe.pixels.height
            && meta.envelope_upper == envelope_upper(r)?
            && meta.envelope_upper <= max_response as u64,
        "derived_receipt_changed"
    );
    resources.admit_image(r.width, r.height, r.encoded_bytes)?;
    effective.admit_image(r.parent.width, r.parent.height, r.parent.encoded_bytes)?;
    admit_copy(r, resources)?;
    Ok(())
}
fn admit_copy(receipt: &DerivedVisualReceipt, resources: &ResourceBudget) -> Result<()> {
    let working = receipt
        .admission
        .encoded_output_upper_bytes
        .checked_add(receipt.encoded_bytes)
        .and_then(|n| n.checked_add(TRANSIENT_WORKING))
        .ok_or_else(|| anyhow::anyhow!("derived_copy_working_overflow"))?;
    ensure!(
        working <= resources.owned_working_bytes,
        "derived_copy_working_rejected"
    );
    Ok(())
}
fn crop_budget(resources: &ResourceBudget, max_response: usize) -> Result<ResourceBudget> {
    resources.validate()?;
    ensure!(
        (MAX_METADATA + WIRE_OVERHEAD + 4..=MAX_RESPONSE).contains(&max_response),
        "derived_response_limit_rejected"
    );
    let base64 = (max_response - MAX_METADATA - WIRE_OVERHEAD) as u64;
    let encoded = (base64 / 4)
        .checked_mul(3)
        .ok_or_else(|| anyhow::anyhow!("derived_response_overflow"))?;
    let mut effective = resources.clone();
    effective.image_file_bytes = effective.image_file_bytes.min(encoded);
    effective.image_base64_bytes = effective.image_base64_bytes.min(base64);
    effective.owned_working_bytes = effective
        .owned_working_bytes
        .checked_sub(TRANSIENT_WORKING)
        .ok_or_else(|| anyhow::anyhow!("derived_working_capacity_rejected"))?;
    effective.validate()?;
    Ok(effective)
}
fn generation_key(pin: &GenerationPin) -> String {
    format!("{}:{}", pin.generation, pin.manifest_sha256)
}
fn request_key(parent: &str, bbox: BoundingBox, resources: &ResourceBudget) -> Result<String> {
    hash_json(&(
        "gptgrep.derived.request.v1",
        parent,
        bbox,
        hash_json(resources)?,
    ))
}
fn derivative_id(
    pin: &GenerationPin,
    parent: &str,
    resources: &str,
    recipe: &str,
    output: &str,
) -> Result<String> {
    Ok(format!(
        "derived-{}",
        hash_json(&(
            "gptgrep.derived.identity.v1",
            pin,
            parent,
            resources,
            recipe,
            output
        ))?
    ))
}
fn envelope_upper(receipt: &DerivedVisualReceipt) -> Result<u64> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        schema_version: &'static str,
        receipt: &'a DerivedVisualReceipt,
        png_base64: &'static str,
    }
    let (json, _) = count_hash(
        &Envelope {
            schema_version: "gptgrep.visual.derived-payload.v1",
            receipt,
            png_base64: "",
        },
        MAX_METADATA + WIRE_OVERHEAD,
    )?;
    let base64 = receipt
        .encoded_bytes
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("derived_response_overflow"))?;
    json.checked_add(base64)
        .ok_or_else(|| anyhow::anyhow!("derived_response_overflow"))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hash_json(value: &impl Serialize) -> Result<String> {
    Ok(count_hash(value, MAX_METADATA)?.1)
}
fn count_hash(value: &impl Serialize, limit: usize) -> Result<(u64, String)> {
    struct Counter {
        count: usize,
        limit: usize,
        hash: Sha256,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let total = self
                .count
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("derived_metadata_overflow"))?;
            if total > self.limit {
                return Err(std::io::Error::other("derived_metadata_limit_rejected"));
            }
            self.count = total;
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Counter {
        count: 0,
        limit,
        hash: Sha256::new(),
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow::anyhow!("derived_metadata_limit_rejected"))?;
    Ok((writer.count as u64, format!("{:x}", writer.hash.finalize())))
}
fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let (length, _) = count_hash(value, MAX_METADATA)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length as usize)
        .map_err(|_| anyhow::anyhow!("derived_metadata_allocation_rejected"))?;
    serde_json::to_writer(&mut bytes, value)
        .map_err(|_| anyhow::anyhow!("derived_metadata_serialization_rejected"))?;
    ensure!(
        bytes.len() as u64 == length,
        "derived_metadata_length_changed"
    );
    Ok(bytes)
}
fn cache_error(error: CacheError) -> anyhow::Error {
    anyhow::anyhow!(match error {
        CacheError::CapacityRetained => "derived_payload_capacity_retained",
        CacheError::Oversized { .. } => "derived_payload_capacity_rejected",
        CacheError::DigestMismatch => "derived_payload_changed",
        CacheError::Loader(_) => "derived_cache_loader_rejected",
        _ => "derived_cache_error",
    })
}

#[cfg(test)]
#[path = "derived_visual/tests.rs"]
mod tests;
