//! Manifest-bound visual candidates and owned verified renditions. Enumeration,
//! reading and reranking grant no actual image-delivery or citation rights.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail, ensure};
use gptgrep_parse::{
    AffineTransform, ArtifactKind, BoundingBox, OcrImage, PageGeometry, ResourceBudget,
};
use serde::{Deserialize, Serialize};

use super::{
    EvidenceRef, RICH_MANIFEST_SCHEMA, RichDocument, RichGenerationReader, RichManifest,
    StoredArtifact, namespaced_id, same_artifact, storage_path, valid_digest,
};
use crate::custody::{HeldDirectory, VerifiedFile};
use crate::read_cache::ReadMeter;
use crate::{GenerationPin, MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES, STATE, digest};

pub const MAX_VISUAL_OUTPUT_BYTES: usize = 1_048_576;
const MAX_CANDIDATES: usize = 256;
const MAX_CURSOR_BYTES: usize = 4096;
const LANE: &str = "manifest_visual_v1";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VisualScope {
    /// Empty selects all documents in the fixed pin, not an unknown sample.
    pub document_ids: Vec<String>,
    pub first_page: Option<u32>,
    pub last_page: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualCandidateRequest {
    pub scope: VisualScope,
    /// Bound into the cursor; this lane does not score or interpret the query.
    pub query: String,
    pub max_candidates: usize,
    pub max_output_bytes: usize,
    pub resources: ResourceBudget,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualCandidate {
    pub evidence: EvidenceRef,
    pub mime: String,
    pub encoded_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub profile_sha256: String,
    pub prepared_sha256: String,
    pub page_geometry: PageGeometry,
    pub raster_to_page: AffineTransform,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualCoverage {
    pub documents_in_scope: usize,
    pub pages_in_scope: usize,
    pub pages_without_rendition: usize,
    pub available_candidates: usize,
    pub returned_source_documents_verified: usize,
    pub enumeration_complete: bool,
    pub artifact_bytes_verified: bool,
    pub actual_image_delivery: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualCandidateReport {
    pub schema_version: String,
    pub generation: GenerationPin,
    pub lane: String,
    pub projection: String,
    pub scope: VisualScope,
    pub query_sha256: String,
    pub options_sha256: String,
    pub offset: usize,
    pub candidates: Vec<VisualCandidate>,
    pub next_cursor: Option<String>,
    pub coverage: VisualCoverage,
}

#[derive(Clone)]
pub struct VisualGenerationReader {
    manifest: Arc<RichManifest>,
    pin: GenerationPin,
    source_root: HeldDirectory,
    generation_dir: HeldDirectory,
    meter: ReadMeter,
}

/// Opaque owned bytes. Access does not certify delivery to a model or reader.
/// No Debug/Serialize implementation can accidentally emit image bytes.
pub struct VerifiedVisualPayload {
    candidate: VisualCandidate,
    bytes: Vec<u8>,
    envelope_upper_bytes: u64,
}
impl VerifiedVisualPayload {
    pub fn candidate(&self) -> &VisualCandidate {
        &self.candidate
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Bound for the declared base64 envelope, not arbitrary host wrappers.
    pub fn envelope_upper_bytes(&self) -> u64 {
        self.envelope_upper_bytes
    }
}

/// Exclusive file plus manifest-derived crop input and retained custody.
/// Passing it to crop still grants no image delivery or citation rights.
pub struct VerifiedRenditionFile {
    candidate: VisualCandidate,
    image: OcrImage,
    file: VerifiedFile,
    custody: VisualGenerationReader,
    document_id: String,
}
impl VerifiedRenditionFile {
    pub fn candidate(&self) -> &VisualCandidate {
        &self.candidate
    }
    pub fn image(&self) -> &OcrImage {
        &self.image
    }
    /// Keep the returned guard until crop/cache validation completes, and
    /// recheck its source before registering any downstream result.
    pub fn into_crop_parts(self) -> Result<(OcrImage, File, VisualGenerationReader)> {
        self.custody.validate()?;
        self.custody.verify_source(&self.document_id)?;
        Ok((self.image, self.file.into_file()?, self.custody))
    }
}

impl RichGenerationReader {
    /// Opt-in visual foundation; it never resolves CURRENT again or starts OCR.
    pub fn visual_reader(&self) -> Result<VisualGenerationReader> {
        ensure!(
            self.manifest.schema_version == RICH_MANIFEST_SCHEMA
                && self.manifest.generation == self.pin.generation
                && valid_digest(&self.pin.manifest_sha256),
            "visual_generation_binding_rejected"
        );
        let relative = Path::new(STATE)
            .join("generations")
            .join(&self.pin.generation);
        ensure!(
            self.dir == self.manifest.root.join(&relative),
            "visual_generation_binding_rejected"
        );
        let source_root = HeldDirectory::open_absolute(&self.manifest.root)?;
        let generation_dir = source_root.directory(&relative)?;
        let reader = VisualGenerationReader {
            manifest: self.manifest.clone(),
            pin: self.pin.clone(),
            source_root,
            generation_dir,
            meter: self.read_meter.clone(),
        };
        reader.validate()?;
        Ok(reader)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorBody {
    schema_version: String,
    generation: GenerationPin,
    scope_sha256: String,
    query_sha256: String,
    options_sha256: String,
    lane: String,
    projection: String,
    position: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    body: CursorBody,
    sha256: String,
}

impl VisualGenerationReader {
    pub fn pin(&self) -> GenerationPin {
        self.pin.clone()
    }
    pub fn validate(&self) -> Result<()> {
        self.source_root.validate()?;
        self.generation_dir.validate()?;
        self.generation_dir.open_verified(
            Path::new("manifest.json"),
            None,
            &self.pin.manifest_sha256,
            MAX_MANIFEST_BYTES,
            &self.meter,
        )?;
        Ok(())
    }
    pub fn verify_source(&self, document_id: &str) -> Result<()> {
        let doc = self
            .manifest
            .documents
            .iter()
            .find(|d| d.document.id == document_id)
            .ok_or_else(|| anyhow::anyhow!("visual_document_not_in_pin"))?;
        self.source_root.open_verified(
            Path::new(&doc.document.path),
            Some(doc.document.source_bytes),
            &doc.document.source_sha256,
            MAX_SOURCE_BYTES,
            &self.meter,
        )?;
        Ok(())
    }

    /// Fixed-pin page/rendition/document round robin. Every cursor page is a
    /// complete bounded object; missing renditions remain explicit coverage.
    pub fn enumerate(&self, request: &VisualCandidateRequest) -> Result<VisualCandidateReport> {
        ensure!(
            (1..=MAX_CANDIDATES).contains(&request.max_candidates)
                && (1..=MAX_VISUAL_OUTPUT_BYTES).contains(&request.max_output_bytes)
                && request.query.len() <= 65_536
                && request
                    .cursor
                    .as_ref()
                    .is_none_or(|c| c.len() <= MAX_CURSOR_BYTES),
            "visual_request_limit_rejected"
        );
        bounded_len(&request.scope, 65_536)?;
        selected_budget(&request.resources, &self.manifest.resource_budget)?;
        let heap_upper = self
            .manifest
            .documents
            .len()
            .checked_mul(128)
            .and_then(|n| n.checked_add(request.max_output_bytes * 4 + 1_048_576))
            .ok_or_else(|| anyhow::anyhow!("visual_working_overflow"))?;
        ensure!(
            heap_upper as u64 <= request.resources.owned_working_bytes,
            "visual_working_limit_rejected"
        );
        self.validate()?;
        let mut scope = request.scope.clone();
        scope.document_ids.sort();
        ensure!(
            scope
                .document_ids
                .iter()
                .all(|id| !id.is_empty() && id.len() <= 256)
                && scope.document_ids.windows(2).all(|w| w[0] != w[1])
                && scope.first_page.is_none_or(|p| p > 0)
                && scope.last_page.is_none_or(|p| p > 0)
                && scope
                    .last_page
                    .is_none_or(|end| end >= scope.first_page.unwrap_or(1)),
            "visual_scope_rejected"
        );
        let mut docs = self
            .manifest
            .documents
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                scope.document_ids.is_empty()
                    || scope.document_ids.binary_search(&d.document.id).is_ok()
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if !scope.document_ids.is_empty() {
            ensure!(
                docs.len() == scope.document_ids.len(),
                "visual_document_not_in_pin"
            );
        }
        docs.sort_by(|a, b| {
            self.manifest.documents[*a]
                .document
                .path
                .cmp(&self.manifest.documents[*b].document.path)
        });
        let scope_sha256 = digest(&serde_json::to_vec(&scope)?);
        let query_sha256 = digest(request.query.as_bytes());
        let options_sha256 = digest(&serde_json::to_vec(&(
            request.max_candidates,
            request.max_output_bytes,
            &request.resources,
            "visual_candidate_v1",
        ))?);
        let mut pages = 0usize;
        let mut missing = 0usize;
        let mut available = 0usize;
        let mut heap = BinaryHeap::new();
        for (order, &i) in docs.iter().enumerate() {
            let doc = &self.manifest.documents[i];
            for page in doc.pages.iter().filter(|p| in_scope(p.number, &scope)) {
                page_rendition_budget(page, &request.resources)?;
                pages = pages
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("visual_count_overflow"))?;
                missing += usize::from(page.renditions.is_empty());
                available = available
                    .checked_add(page.renditions.len())
                    .ok_or_else(|| anyhow::anyhow!("visual_count_overflow"))?;
            }
            if let Some(page) = next_page(doc, 0, &scope) {
                heap.push(Reverse((page, 0usize, order)));
            }
        }
        let offset = match &request.cursor {
            None => 0,
            Some(token) => {
                let cursor: Cursor = serde_json::from_str(token)
                    .map_err(|_| anyhow::anyhow!("visual_cursor_rejected"))?;
                let body = &cursor.body;
                ensure!(
                    cursor.sha256 == digest(&serde_json::to_vec(body)?)
                        && body.schema_version == "gptgrep.visual.cursor.v1"
                        && body.generation == self.pin
                        && body.scope_sha256 == scope_sha256
                        && body.query_sha256 == query_sha256
                        && body.options_sha256 == options_sha256
                        && body.lane == LANE
                        && body.projection == "visual_candidate_v1"
                        && body.position <= available,
                    "visual_cursor_rejected"
                );
                body.position
            }
        };
        let mut report = VisualCandidateReport {
            schema_version: "gptgrep.visual.candidates.v1".into(),
            generation: self.pin.clone(),
            lane: LANE.into(),
            projection: "visual_candidate_v1".into(),
            scope: scope.clone(),
            query_sha256: query_sha256.clone(),
            options_sha256: options_sha256.clone(),
            offset,
            candidates: Vec::new(),
            next_cursor: None,
            coverage: VisualCoverage {
                documents_in_scope: docs.len(),
                pages_in_scope: pages,
                pages_without_rendition: missing,
                available_candidates: available,
                returned_source_documents_verified: 0,
                enumeration_complete: offset == available,
                artifact_bytes_verified: false,
                actual_image_delivery: false,
            },
        };
        let cursor_at = |position| -> Result<Option<String>> {
            if position >= available {
                return Ok(None);
            }
            let body = CursorBody {
                schema_version: "gptgrep.visual.cursor.v1".into(),
                generation: self.pin.clone(),
                scope_sha256: scope_sha256.clone(),
                query_sha256: query_sha256.clone(),
                options_sha256: options_sha256.clone(),
                lane: LANE.into(),
                projection: "visual_candidate_v1".into(),
                position,
            };
            let sha256 = digest(&serde_json::to_vec(&body)?);
            let cursor = serde_json::to_string(&Cursor { body, sha256 })?;
            ensure!(
                cursor.len() <= MAX_CURSOR_BYTES,
                "visual_cursor_limit_rejected"
            );
            Ok(Some(cursor))
        };
        report.next_cursor = cursor_at(offset)?;
        bounded_len(&report, request.max_output_bytes as u64)?;
        let mut position = 0usize;
        let mut returned_documents = BTreeSet::new();
        while let Some(Reverse((page_index, rendition_rank, order))) = heap.pop() {
            let doc_index = docs[order];
            let doc = &self.manifest.documents[doc_index];
            let page = &doc.pages[page_index];
            let image_index = rendition_order(page)[rendition_rank];
            if position >= offset {
                let candidate =
                    candidate_for(&self.pin, doc, page_index, image_index, &request.resources)?;
                report.candidates.push(candidate);
                report.next_cursor = cursor_at(position + 1)?;
                report.coverage.enumeration_complete = position + 1 == available;
                let new_doc = !returned_documents.contains(&doc_index);
                report.coverage.returned_source_documents_verified =
                    returned_documents.len() + usize::from(new_doc);
                if bounded_len(&report, request.max_output_bytes as u64).is_err() {
                    report.candidates.pop();
                    report.next_cursor = cursor_at(position)?;
                    report.coverage.enumeration_complete = false;
                    report.coverage.returned_source_documents_verified = returned_documents.len();
                    ensure!(
                        !report.candidates.is_empty(),
                        "visual_response_limit_rejected"
                    );
                    break;
                }
                if new_doc {
                    self.verify_source(&doc.document.id)?;
                    returned_documents.insert(doc_index);
                }
                if report.candidates.len() == request.max_candidates {
                    break;
                }
            }
            position += 1;
            if rendition_rank + 1 < page.renditions.len() {
                heap.push(Reverse((page_index, rendition_rank + 1, order)));
            } else if let Some(next) = next_page(doc, page_index + 1, &scope) {
                heap.push(Reverse((next, 0, order)));
            }
        }
        bounded_len(&report, request.max_output_bytes as u64)?;
        self.validate()?;
        for index in returned_documents {
            self.verify_source(&self.manifest.documents[index].document.id)?;
        }
        Ok(report)
    }

    pub fn open_rendition(
        &self,
        rendition_id: &str,
        budget: &ResourceBudget,
    ) -> Result<VerifiedRenditionFile> {
        selected_budget(budget, &self.manifest.resource_budget)?;
        ensure!(
            budget.owned_working_bytes >= 1_048_576,
            "visual_working_limit_rejected"
        );
        self.validate()?;
        let (doc, page, image) = self.lookup(rendition_id)?;
        let candidate = candidate_for(&self.pin, doc, page, image, budget)?;
        self.verify_source(&doc.document.id)?;
        let image = &doc.pages[page].renditions[image];
        let mut file = self.generation_dir.open_verified(
            &image.artifact.relative_path,
            Some(image.artifact.bytes),
            &image.artifact.sha256,
            budget.image_file_bytes,
            &self.meter,
        )?;
        let dimensions = header_dimensions(&mut file, &image.artifact.mime, &self.meter)?;
        ensure!(
            dimensions == (image.width, image.height),
            "visual_image_dimensions_rejected"
        );
        file.check()?;
        self.validate()?;
        self.verify_source(&doc.document.id)?;
        let mut image = image.clone();
        image.path = PathBuf::new();
        Ok(VerifiedRenditionFile {
            candidate,
            image,
            file,
            custody: self.clone(),
            document_id: doc.document.id.clone(),
        })
    }

    /// Exact declared wire shape: {schema_version,candidate,image_base64}.
    /// It does not serialize or claim delivery; callers must retain this bound
    /// and separately admit any extra host/protocol envelope before delivery.
    pub fn read_rendition(
        &self,
        rendition_id: &str,
        budget: &ResourceBudget,
        max_response_bytes: usize,
    ) -> Result<VerifiedVisualPayload> {
        ensure!(
            (1..=MAX_VISUAL_OUTPUT_BYTES).contains(&max_response_bytes),
            "visual_response_limit_rejected"
        );
        selected_budget(budget, &self.manifest.resource_budget)?;
        ensure!(
            budget.owned_working_bytes >= 1_048_576,
            "visual_working_limit_rejected"
        );
        let (doc, page, image) = self.lookup(rendition_id)?;
        let candidate = candidate_for(&self.pin, doc, page, image, budget)?;
        #[derive(Serialize)]
        struct Envelope<'a> {
            schema_version: &'static str,
            candidate: &'a VisualCandidate,
            image_base64: &'static str,
        }
        let metadata = bounded_len(
            &Envelope {
                schema_version: "gptgrep.visual.payload.v1",
                candidate: &candidate,
                image_base64: "",
            },
            max_response_bytes as u64,
        )?;
        let base64 = candidate
            .encoded_bytes
            .checked_add(2)
            .and_then(|n| n.checked_div(3))
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("visual_base64_overflow"))?;
        let upper = metadata
            .checked_add(base64)
            .ok_or_else(|| anyhow::anyhow!("visual_response_overflow"))?;
        ensure!(
            upper <= max_response_bytes as u64
                && base64 <= budget.image_base64_bytes
                && candidate
                    .encoded_bytes
                    .checked_add(upper)
                    .and_then(|n| n.checked_add(1_048_576))
                    .is_some_and(|n| n <= budget.owned_working_bytes),
            "visual_response_admission_rejected"
        );
        // No artifact byte read before full metadata/base64 response admission.
        let mut owned = self.open_rendition(rendition_id, budget)?;
        let bytes = owned
            .file
            .read_bounded(budget.image_file_bytes, &self.meter)?;
        self.validate()?;
        self.verify_source(&owned.document_id)?;
        Ok(VerifiedVisualPayload {
            candidate: owned.candidate,
            bytes,
            envelope_upper_bytes: upper,
        })
    }

    fn lookup(&self, id: &str) -> Result<(&RichDocument, usize, usize)> {
        ensure!(
            !id.is_empty() && id.len() <= 256,
            "visual_rendition_id_rejected"
        );
        self.manifest
            .documents
            .iter()
            .find_map(|doc| {
                doc.pages.iter().enumerate().find_map(|(p, page)| {
                    page.renditions
                        .iter()
                        .position(|image| image.artifact.id == id)
                        .map(|r| (doc, p, r))
                })
            })
            .ok_or_else(|| anyhow::anyhow!("visual_rendition_not_in_pin"))
    }
}

fn selected_budget(selected: &ResourceBudget, stored: &ResourceBudget) -> Result<()> {
    selected.validate()?;
    stored.validate()?;
    let selected = serde_json::to_value(selected)?;
    let stored = serde_json::to_value(stored)?;
    ensure!(
        selected
            .as_object()
            .expect("resource object")
            .iter()
            .all(|(key, value)| value.as_u64() <= stored[key].as_u64()),
        "visual_budget_exceeds_generation"
    );
    Ok(())
}
fn in_scope(page: u32, scope: &VisualScope) -> bool {
    page >= scope.first_page.unwrap_or(1) && scope.last_page.is_none_or(|end| page <= end)
}
fn next_page(doc: &RichDocument, start: usize, scope: &VisualScope) -> Option<usize> {
    doc.pages
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, p)| in_scope(p.number, scope) && !p.renditions.is_empty())
        .map(|(i, _)| i)
}
fn rendition_order(page: &gptgrep_parse::NativePage) -> [usize; 4] {
    let mut order = [0, 1, 2, 3];
    order[..page.renditions.len()].sort_by(|a, b| {
        page.renditions[*a]
            .artifact
            .id
            .cmp(&page.renditions[*b].artifact.id)
    });
    order
}

fn page_rendition_budget(page: &gptgrep_parse::NativePage, budget: &ResourceBudget) -> Result<()> {
    ensure!(
        page.renditions.len() <= 4 && page.renditions.len() <= budget.images_per_page,
        "visual_page_rendition_limit_rejected"
    );
    let (pixels, bytes) = page.renditions.iter().try_fold(
        (0u64, 0u64),
        |(pixels, bytes), image| -> Result<(u64, u64)> {
            let image_pixels = u64::from(image.width)
                .checked_mul(u64::from(image.height))
                .ok_or_else(|| anyhow::anyhow!("visual_page_rendition_overflow"))?;
            Ok((
                pixels
                    .checked_add(image_pixels)
                    .ok_or_else(|| anyhow::anyhow!("visual_page_rendition_overflow"))?,
                bytes
                    .checked_add(image.artifact.bytes)
                    .ok_or_else(|| anyhow::anyhow!("visual_page_rendition_overflow"))?,
            ))
        },
    )?;
    ensure!(
        pixels <= budget.image_pixels_per_page && bytes <= budget.image_file_bytes_per_page,
        "visual_page_rendition_limit_rejected"
    );
    Ok(())
}

fn candidate_for(
    pin: &GenerationPin,
    doc: &RichDocument,
    page_index: usize,
    image_index: usize,
    budget: &ResourceBudget,
) -> Result<VisualCandidate> {
    let page = &doc.pages[page_index];
    page_rendition_budget(page, budget)?;
    let image = &page.renditions[image_index];
    let artifact = &image.artifact;
    gptgrep_parse::validate_rendition_metadata(image, budget)?;
    ensure!(
        artifact.region_id.is_none()
            && artifact.page == Some(page.number)
            && artifact.profile_sha256 == doc.native_profile_sha256
            && valid_digest(&doc.document.source_sha256)
            && valid_digest(&doc.prepared.prepared_sha256)
            && doc.prepared.source_sha256 == doc.document.source_sha256
            && doc.prepared.source_bytes == doc.document.source_bytes,
        "visual_manifest_binding_rejected"
    );
    let stored: &StoredArtifact = doc
        .artifacts
        .iter()
        .find(|a| a.artifact.id == artifact.id)
        .ok_or_else(|| anyhow::anyhow!("visual_artifact_not_in_pin"))?;
    let parser_id = gptgrep_parse::artifact_id(
        &doc.document.source_sha256,
        &artifact.profile_sha256,
        &ArtifactKind::Rendition,
        artifact.page,
        None,
        &artifact.sha256,
    )?;
    ensure!(
        stored.document_id == doc.document.id
            && stored.parser_artifact_id == parser_id
            && artifact.id == namespaced_id(&doc.document.id, &parser_id)
            && artifact.relative_path == storage_path(&doc.document.id, artifact)
            && same_artifact(&stored.artifact, artifact),
        "visual_artifact_binding_rejected"
    );
    let bbox = BoundingBox {
        x: 0.0,
        y: 0.0,
        width: page.width_points,
        height: page.height_points,
    };
    bbox.validate(page.width_points, page.height_points)?;
    ensure!(
        page.number as usize == page_index + 1
            && page.geometry.width_points == page.width_points
            && page.geometry.height_points == page.height_points
            && (0..=3).contains(&page.geometry.rotation_quarter_turns)
            && page.geometry.user_unit.is_finite()
            && page.geometry.user_unit > 0.0,
        "visual_page_geometry_rejected"
    );
    page.geometry.source_to_page.validate()?;
    page.geometry.page_to_source.validate()?;
    for (x, y) in [
        (0.0, 0.0),
        (page.width_points, 0.0),
        (0.0, page.height_points),
    ] {
        let s = page.geometry.page_to_source.apply(x, y);
        let p = page.geometry.source_to_page.apply(s.0, s.1);
        ensure!(
            (p.0 - x).abs() <= 0.001 && (p.1 - y).abs() <= 0.001,
            "visual_page_geometry_rejected"
        );
    }
    for (x, y, rx, ry) in [
        (0.0, 0.0, 0.0, 0.0),
        (page.width_points, 0.0, f64::from(image.width), 0.0),
        (0.0, page.height_points, 0.0, f64::from(image.height)),
    ] {
        let actual = image.page_to_raster.apply(x, y);
        ensure!(
            (actual.0 - rx).abs() <= 0.001 && (actual.1 - ry).abs() <= 0.001,
            "visual_page_raster_binding_rejected"
        );
    }
    Ok(VisualCandidate {
        evidence: EvidenceRef::VisualRegion {
            generation: pin.clone(),
            document_id: doc.document.id.clone(),
            source_path: doc.document.path.clone(),
            source_sha256: doc.document.source_sha256.clone(),
            page: page.number,
            region_id: format!("full-page-{}", page.number),
            rendition_id: artifact.id.clone(),
            rendition_sha256: artifact.sha256.clone(),
            bbox,
            page_to_raster: image.page_to_raster,
        },
        mime: artifact.mime.clone(),
        encoded_bytes: artifact.bytes,
        width: image.width,
        height: image.height,
        profile_sha256: artifact.profile_sha256.clone(),
        prepared_sha256: doc.prepared.prepared_sha256.clone(),
        page_geometry: page.geometry.clone(),
        raster_to_page: image.page_to_raster.inverse()?,
    })
}

fn bounded_len(value: &impl Serialize, limit: u64) -> Result<u64> {
    struct Count {
        bytes: u64,
        limit: u64,
    }
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let next = self
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("visual_size_overflow"))?;
            if next > self.limit {
                return Err(std::io::Error::other("visual_response_limit_rejected"));
            }
            self.bytes = next;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, limit };
    serde_json::to_writer(&mut count, value)
        .map_err(|_| anyhow::anyhow!("visual_response_limit_rejected"))?;
    Ok(count.bytes)
}

fn header_dimensions(file: &mut VerifiedFile, mime: &str, meter: &ReadMeter) -> Result<(u32, u32)> {
    file.check()?;
    let length = file.bytes();
    let inner = file.file_mut();
    inner
        .seek(SeekFrom::Start(0))
        .map_err(|_| anyhow::anyhow!("visual_image_header_rejected"))?;
    fn read(file: &mut File, bytes: &mut [u8], meter: &ReadMeter) -> Result<()> {
        let mut at = 0;
        while at < bytes.len() {
            let n = file
                .read(&mut bytes[at..])
                .map_err(|_| anyhow::anyhow!("visual_image_header_rejected"))?;
            meter.record_physical_read(n)?;
            ensure!(n > 0, "visual_image_header_rejected");
            at += n;
        }
        Ok(())
    }
    let result = match mime {
        "image/png" => {
            let mut header = [0u8; 33];
            read(inner, &mut header, meter)?;
            ensure!(
                header[..8] == *b"\x89PNG\r\n\x1a\n"
                    && header[8..12] == 13u32.to_be_bytes()
                    && header[12..16] == *b"IHDR",
                "visual_image_header_rejected"
            );
            (
                u32::from_be_bytes(header[16..20].try_into()?),
                u32::from_be_bytes(header[20..24].try_into()?),
            )
        }
        "image/jpeg" => {
            let mut soi = [0u8; 2];
            read(inner, &mut soi, meter)?;
            ensure!(soi == [0xff, 0xd8], "visual_image_header_rejected");
            loop {
                let mut marker = [0u8; 1];
                read(inner, &mut marker, meter)?;
                ensure!(marker[0] == 0xff, "visual_image_header_rejected");
                while marker[0] == 0xff {
                    read(inner, &mut marker, meter)?;
                }
                ensure!(
                    !matches!(marker[0], 0 | 0xd8 | 0xd9 | 0xda | 0xd0..=0xd7),
                    "visual_image_header_rejected"
                );
                let mut len = [0u8; 2];
                read(inner, &mut len, meter)?;
                let len = u64::from(u16::from_be_bytes(len));
                ensure!(len >= 2, "visual_image_header_rejected");
                let at = inner
                    .stream_position()
                    .map_err(|_| anyhow::anyhow!("visual_image_header_rejected"))?;
                let end = at
                    .checked_add(len - 2)
                    .ok_or_else(|| anyhow::anyhow!("visual_image_header_rejected"))?;
                ensure!(end <= length, "visual_image_header_rejected");
                if matches!(marker[0],0xc0..=0xc3|0xc5..=0xc7|0xc9..=0xcb|0xcd..=0xcf) {
                    ensure!(len >= 8, "visual_image_header_rejected");
                    let mut frame = [0u8; 6];
                    read(inner, &mut frame, meter)?;
                    break (
                        u32::from(u16::from_be_bytes([frame[3], frame[4]])),
                        u32::from(u16::from_be_bytes([frame[1], frame[2]])),
                    );
                }
                inner
                    .seek(SeekFrom::Start(end))
                    .map_err(|_| anyhow::anyhow!("visual_image_header_rejected"))?;
            }
        }
        _ => bail!("visual_image_mime_rejected"),
    };
    file.check()?;
    ensure!(result.0 > 0 && result.1 > 0, "visual_image_header_rejected");
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::Document;
    use gptgrep_parse::{ArtifactRef, CoverageState, FeatureState, NativePage, PreparedSummary};
    use serde_json::json;
    use std::fs;

    // Synthetic pinned DTOs test custody/lineage, not publication/parser/image
    // utility acceptance. No native engine, provider or model is involved.
    fn png(value: u8) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut crc = u32::MAX;
            for b in kind.iter().chain(data) {
                crc ^= u32::from(*b);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
                }
            }
            out.extend_from_slice(&(!crc).to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = vec![];
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &header);
        let raw = [0, value, 60, 78, 255];
        let mut z = vec![0x78, 0x01, 1, 5, 0, 250, 255];
        z.extend_from_slice(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        dir: PathBuf,
        reader: VisualGenerationReader,
    }
    fn fixture(counts: &[usize], missing: bool) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let dir = root
            .join(STATE)
            .join("generations")
            .join("synthetic-visual");
        fs::create_dir_all(&dir).unwrap();
        let mut docs = vec![];
        for (index, &count) in counts.iter().enumerate() {
            let source = format!("synthetic source {index}").into_bytes();
            let source_sha = digest(&source);
            let profile = digest(b"synthetic visual recipe");
            let id = format!("document-{index}");
            let path = format!("source-{index}.pdf");
            fs::write(root.join(&path), &source).unwrap();
            let mut pages = vec![];
            let mut artifacts = vec![];
            for number in 1..=count as u32 {
                let data = png(number as u8);
                let content = digest(&data);
                let parser_id = gptgrep_parse::artifact_id(
                    &source_sha,
                    &profile,
                    &ArtifactKind::Rendition,
                    Some(number),
                    None,
                    &content,
                )
                .unwrap();
                let mut artifact = ArtifactRef {
                    id: namespaced_id(&id, &parser_id),
                    kind: ArtifactKind::Rendition,
                    relative_path: PathBuf::new(),
                    sha256: content,
                    profile_sha256: profile.clone(),
                    bytes: data.len() as u64,
                    mime: "image/png".into(),
                    page: Some(number),
                    region_id: None,
                };
                artifact.relative_path = storage_path(&id, &artifact);
                fs::create_dir_all(dir.join(&artifact.relative_path).parent().unwrap()).unwrap();
                fs::write(dir.join(&artifact.relative_path), &data).unwrap();
                let image = OcrImage {
                    artifact: artifact.clone(),
                    path: PathBuf::new(),
                    width: 1,
                    height: 1,
                    page_to_raster: AffineTransform {
                        a: 1.0 / 64.0,
                        d: 1.0 / 32.0,
                        ..AffineTransform::IDENTITY
                    },
                };
                artifacts.push(StoredArtifact {
                    document_id: id.clone(),
                    parser_artifact_id: parser_id,
                    artifact: artifact.clone(),
                });
                let complexity=serde_json::from_value(json!({"page_number":number,"text_length":0,"text_coverage":0.0,"has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,"full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":2048.0,"needs_ocr":false,"reasons":[],"layout":null})).unwrap();
                pages.push(NativePage {
                    number,
                    label: FeatureState::Absent,
                    width_points: 64.0,
                    height_points: 32.0,
                    geometry: PageGeometry {
                        width_points: 64.0,
                        height_points: 32.0,
                        rotation_quarter_turns: 0,
                        user_unit: 1.0,
                        source_to_page: AffineTransform::IDENTITY,
                        page_to_source: AffineTransform::IDENTITY,
                    },
                    native_text: artifact.clone(),
                    metadata_artifact: artifact,
                    renditions: if missing && number == count as u32 {
                        vec![]
                    } else {
                        vec![image]
                    },
                    complexity,
                    coverage: CoverageState::BlankVerified,
                    capabilities: Default::default(),
                });
            }
            let document = Document {
                id: id.clone(),
                path,
                source_sha256: source_sha.clone(),
                source_bytes: source.len() as u64,
                text_sha256: digest(b""),
                parser: "synthetic visual DTO fixture".into(),
                title: "synthetic".into(),
                pages: vec![],
                nodes: vec![],
                warnings: vec![],
            };
            let prepared = PreparedSummary {
                source_sha256: source_sha.clone(),
                prepared_sha256: source_sha,
                source_bytes: source.len() as u64,
                prepared_bytes: source.len() as u64,
                format: "pdf".into(),
                parser_identity: document.parser.clone(),
                preparation_profile_sha256: profile.clone(),
                conversion: FeatureState::Absent,
                image_geometry: FeatureState::Absent,
            };
            docs.push(RichDocument {
                document,
                prepared,
                prepared_path: "prepared.pdf".into(),
                native_profile_sha256: profile,
                pages,
                artifacts,
                segments: vec![],
                ocr_tasks: vec![],
                ocr_results: vec![],
                unmapped_merged_lines: 0,
            });
        }
        let manifest = Arc::new(RichManifest {
            schema_version: RICH_MANIFEST_SCHEMA.into(),
            generation: "synthetic-visual".into(),
            root: root.clone(),
            created_unix_ms: 0,
            resource_budget: ResourceBudget::default(),
            documents: docs,
        });
        let bytes = serde_json::to_vec(manifest.as_ref()).unwrap();
        fs::write(dir.join("manifest.json"), &bytes).unwrap();
        let pin = GenerationPin {
            generation: manifest.generation.clone(),
            manifest_sha256: digest(&bytes),
        };
        let source_root = HeldDirectory::open_absolute(&root).unwrap();
        let generation_dir = source_root
            .directory(&Path::new(STATE).join("generations").join(&pin.generation))
            .unwrap();
        Fixture {
            _temp: temp,
            root,
            dir,
            reader: VisualGenerationReader {
                manifest,
                pin,
                source_root,
                generation_dir,
                meter: ReadMeter::default(),
            },
        }
    }
    fn request() -> VisualCandidateRequest {
        VisualCandidateRequest {
            scope: VisualScope::default(),
            query: "visual traversal".into(),
            max_candidates: 256,
            max_output_bytes: MAX_VISUAL_OUTPUT_BYTES,
            resources: ResourceBudget::default(),
            cursor: None,
        }
    }
    fn identity(c: &VisualCandidate) -> (String, u32, String) {
        match &c.evidence {
            EvidenceRef::VisualRegion {
                document_id,
                page,
                rendition_id,
                ..
            } => (document_id.clone(), *page, rendition_id.clone()),
            _ => panic!("fabricated text evidence"),
        }
    }
    fn change_manifest(f: &mut Fixture, change: impl FnOnce(&mut RichManifest)) {
        change(Arc::make_mut(&mut f.reader.manifest));
        let bytes = serde_json::to_vec(f.reader.manifest.as_ref()).unwrap();
        fs::write(f.dir.join("manifest.json"), &bytes).unwrap();
        f.reader.pin.manifest_sha256 = digest(&bytes);
    }
    fn add_rendition(f: &mut Fixture, document: usize, page: usize) {
        let doc = &f.reader.manifest.documents[document];
        let data = png(77);
        let mut image = doc.pages[page].renditions[0].clone();
        image.artifact.sha256 = digest(&data);
        image.artifact.bytes = data.len() as u64;
        let parser_id = gptgrep_parse::artifact_id(
            &doc.document.source_sha256,
            &image.artifact.profile_sha256,
            &image.artifact.kind,
            image.artifact.page,
            None,
            &image.artifact.sha256,
        )
        .unwrap();
        image.artifact.id = namespaced_id(&doc.document.id, &parser_id);
        image.artifact.relative_path = storage_path(&doc.document.id, &image.artifact);
        fs::write(f.dir.join(&image.artifact.relative_path), data).unwrap();
        change_manifest(f, |manifest| {
            let doc = &mut manifest.documents[document];
            doc.artifacts.push(StoredArtifact {
                document_id: doc.document.id.clone(),
                parser_artifact_id: parser_id,
                artifact: image.artifact.clone(),
            });
            doc.pages[page].renditions.push(image);
        });
    }
    #[test]
    fn multiple_renditions_are_fair_and_selected_page_totals_precede_payload_reads() {
        let mut f = fixture(&[2, 1], false);
        add_rendition(&mut f, 0, 0);
        let report = f.reader.enumerate(&request()).unwrap();
        assert_eq!(
            report
                .candidates
                .iter()
                .map(|c| {
                    let (d, p, _) = identity(c);
                    (d, p)
                })
                .collect::<Vec<_>>(),
            vec![
                ("document-0".into(), 1),
                ("document-1".into(), 1),
                ("document-0".into(), 1),
                ("document-0".into(), 2),
            ]
        );
        let page = &f.reader.manifest.documents[0].pages[0];
        let id = page.renditions[0].artifact.id.clone();
        let one_image_bytes = page.renditions[0].artifact.bytes;
        for kind in 0..3 {
            let mut budget = ResourceBudget::default();
            match kind {
                0 => budget.images_per_page = 1,
                1 => budget.image_pixels_per_page = 1,
                _ => {
                    budget.image_file_bytes = one_image_bytes;
                    budget.image_file_bytes_per_page = one_image_bytes;
                }
            }
            let before = f.reader.meter.snapshot().unwrap();
            let error = f
                .reader
                .read_rendition(&id, &budget, MAX_VISUAL_OUTPUT_BYTES)
                .err()
                .expect("page aggregate was not admitted")
                .to_string();
            assert_eq!(error, "visual_page_rendition_limit_rejected");
            assert_eq!(
                f.reader.meter.snapshot().unwrap().physical_read_bytes,
                before.physical_read_bytes
            );
        }
    }
    #[test]
    fn fixed_pin_round_robin_cursor_equals_unpaged_and_grants_no_delivery() {
        let f = fixture(&[3, 1], false);
        let full = f.reader.enumerate(&request()).unwrap();
        assert_eq!(
            full.candidates
                .iter()
                .map(|c| {
                    let (d, p, _) = identity(c);
                    (d, p)
                })
                .collect::<Vec<_>>(),
            vec![
                ("document-0".into(), 1),
                ("document-1".into(), 1),
                ("document-0".into(), 2),
                ("document-0".into(), 3)
            ]
        );
        let mut req = request();
        req.max_candidates = 2;
        let first = f.reader.enumerate(&req).unwrap();
        req.cursor = first.next_cursor.clone();
        let second = f.reader.enumerate(&req).unwrap();
        assert_eq!(
            first
                .candidates
                .iter()
                .chain(&second.candidates)
                .map(identity)
                .collect::<Vec<_>>(),
            full.candidates.iter().map(identity).collect::<Vec<_>>()
        );
        assert!(second.coverage.enumeration_complete && second.next_cursor.is_none());
        assert!(!full.coverage.actual_image_delivery && !full.coverage.artifact_bytes_verified);
        let encoded = serde_json::to_value(&full.candidates[0].evidence).unwrap();
        assert_eq!(encoded["kind"], "visual_region");
        assert!(encoded.get("line").is_none() && encoded.get("byte_start").is_none());
        for kind in 0..4 {
            let mut bad = req.clone();
            match kind {
                0 => bad.query.push('!'),
                1 => bad.max_candidates += 1,
                2 => bad.max_output_bytes -= 1,
                _ => bad.resources.image_file_bytes /= 2,
            };
            assert_eq!(
                f.reader.enumerate(&bad).unwrap_err().to_string(),
                "visual_cursor_rejected"
            );
        }
        let mut cursor: Cursor = serde_json::from_str(req.cursor.as_ref().unwrap()).unwrap();
        cursor.body.generation.generation = "another-generation".into();
        cursor.sha256 = digest(&serde_json::to_vec(&cursor.body).unwrap());
        req.cursor = Some(serde_json::to_string(&cursor).unwrap());
        assert_eq!(
            f.reader.enumerate(&req).unwrap_err().to_string(),
            "visual_cursor_rejected"
        );
    }
    #[test]
    fn response_bound_returns_complete_objects_and_truthful_missing_page_scope() {
        let f = fixture(&[3, 2], true);
        let mut small = request();
        small.max_candidates = 1;
        let first = f.reader.enumerate(&small).unwrap();
        let cap = serde_json::to_vec(&first).unwrap().len() + 16;
        let mut req = request();
        req.max_output_bytes = cap;
        let mut all = vec![];
        loop {
            let report = f.reader.enumerate(&req).unwrap();
            assert!(serde_json::to_vec(&report).unwrap().len() <= cap);
            assert_eq!(report.coverage.pages_without_rendition, 2);
            all.extend(report.candidates.iter().map(identity));
            match report.next_cursor {
                Some(c) => req.cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(
            all,
            f.reader
                .enumerate(&request())
                .unwrap()
                .candidates
                .iter()
                .map(identity)
                .collect::<Vec<_>>()
        );
        assert_eq!(all.len(), 3);
        let mut scoped = request();
        scoped.scope = VisualScope {
            document_ids: vec!["document-1".into()],
            first_page: Some(2),
            last_page: Some(2),
        };
        let r = f.reader.enumerate(&scoped).unwrap();
        assert!(r.candidates.is_empty() && r.coverage.enumeration_complete);
        assert_eq!(r.coverage.pages_in_scope, 1);
        assert_eq!(r.coverage.pages_without_rendition, 1);
    }
    #[test]
    fn response_and_base64_admission_precede_all_artifact_reads() {
        let mut f = fixture(&[1], false);
        let id = f.reader.manifest.documents[0].pages[0].renditions[0]
            .artifact
            .id
            .clone();
        let before = f.reader.meter.snapshot().unwrap();
        assert!(
            f.reader
                .read_rendition(&id, &ResourceBudget::default(), 1)
                .is_err()
        );
        assert_eq!(
            f.reader.meter.snapshot().unwrap().physical_read_bytes,
            before.physical_read_bytes
        );
        let result = f
            .reader
            .read_rendition(&id, &ResourceBudget::default(), MAX_VISUAL_OUTPUT_BYTES)
            .unwrap();
        assert_eq!(result.bytes(), png(1));
        assert!(result.envelope_upper_bytes() <= MAX_VISUAL_OUTPUT_BYTES as u64);
        change_manifest(&mut f, |m| {
            m.documents[0].pages[0].renditions[0].artifact.bytes = 1_048_576;
            m.documents[0].artifacts[0].artifact.bytes = 1_048_576
        });
        let before = f.reader.meter.snapshot().unwrap();
        assert_eq!(
            f.reader
                .read_rendition(&id, &ResourceBudget::default(), MAX_VISUAL_OUTPUT_BYTES)
                .err()
                .unwrap()
                .to_string(),
            "visual_response_admission_rejected"
        );
        assert_eq!(
            f.reader.meter.snapshot().unwrap().physical_read_bytes,
            before.physical_read_bytes
        );
    }
    #[test]
    fn source_and_manifest_mutations_reject_without_partial_or_private_errors() {
        let f = fixture(&[1], false);
        fs::write(f.root.join("source-0.pdf"), b"changed bytes").unwrap();
        let e = f.reader.enumerate(&request()).unwrap_err().to_string();
        assert!(!e.contains(f.root.to_str().unwrap()));
        assert!(e.starts_with("custody_"));
        let f = fixture(&[1], false);
        fs::write(f.dir.join("manifest.json"), b"{}").unwrap();
        assert!(f.reader.enumerate(&request()).is_err());
    }
    #[test]
    fn profile_geometry_and_actual_header_dimensions_are_checked() {
        let mut f = fixture(&[1], false);
        change_manifest(&mut f, |m| {
            m.documents[0].pages[0].renditions[0]
                .artifact
                .profile_sha256 = digest(b"forged")
        });
        assert!(f.reader.enumerate(&request()).is_err());
        let mut f = fixture(&[1], false);
        change_manifest(&mut f, |m| {
            m.documents[0].pages[0].renditions[0].page_to_raster.e = 5.0
        });
        assert!(f.reader.enumerate(&request()).is_err());
        let mut f = fixture(&[1], false);
        change_manifest(&mut f, |m| {
            m.documents[0].pages[0].geometry.rotation_quarter_turns = -1
        });
        assert_eq!(
            f.reader.enumerate(&request()).unwrap_err().to_string(),
            "visual_page_geometry_rejected"
        );
        let mut f = fixture(&[1], false);
        let id = f.reader.manifest.documents[0].pages[0].renditions[0]
            .artifact
            .id
            .clone();
        change_manifest(&mut f, |m| {
            let i = &mut m.documents[0].pages[0].renditions[0];
            i.width = 2;
            i.page_to_raster.a = 2.0 / 64.0
        });
        assert_eq!(
            f.reader
                .open_rendition(&id, &ResourceBudget::default())
                .err()
                .unwrap()
                .to_string(),
            "visual_image_dimensions_rejected"
        );
        let mut tight = ResourceBudget::default();
        tight.image_file_bytes = 1;
        assert!(f.reader.open_rendition(&id, &tight).is_err());
    }
    #[test]
    fn ancestor_and_final_symlink_replacements_reject_held_handles() {
        use std::os::unix::fs::symlink;
        let f = fixture(&[1], false);
        let i = &f.reader.manifest.documents[0].pages[0].renditions[0];
        let file = f.dir.join(&i.artifact.relative_path);
        fs::rename(&file, file.with_extension("retained")).unwrap();
        symlink(file.with_extension("retained"), &file).unwrap();
        assert!(
            f.reader
                .open_rendition(&i.artifact.id, &ResourceBudget::default())
                .is_err()
        );
        let f = fixture(&[1], false);
        fs::rename(&f.dir, f.dir.with_extension("retained")).unwrap();
        fs::create_dir(&f.dir).unwrap();
        assert!(f.reader.validate().is_err());
    }
    #[test]
    fn exclusive_file_crop_handoff_retains_source_guard_without_delivery_rights() {
        let f = fixture(&[1], false);
        let id = f.reader.manifest.documents[0].pages[0].renditions[0]
            .artifact
            .id
            .clone();
        let owned = f
            .reader
            .open_rendition(&id, &ResourceBudget::default())
            .unwrap();
        let (image, mut file, guard) = owned.into_crop_parts().unwrap();
        assert!(image.path.as_os_str().is_empty());
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, png(1));
        guard.verify_source("document-0").unwrap();
        fs::write(f.root.join("source-0.pdf"), b"changed source").unwrap();
        assert!(guard.verify_source("document-0").is_err());
    }
}
