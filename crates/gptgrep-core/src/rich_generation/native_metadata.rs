//! Selected, uncached page metadata with lifetime-based aggregate admission.
//! None of these types constructs EvidenceRef or grants citation/delivery rights.
use super::*;
use gptgrep_parse::native_graphics::{NativeGraphicsMetadata, NativeObject};
use gptgrep_parse::{FeatureState, NativePageMetadata, NativeTextMetadata, PageGeometry};
use serde::ser::{SerializeMap, SerializeSeq};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_NATIVE_METADATA_BYTES: u64 = 4_194_304;
pub const MAX_NATIVE_METADATA_WORKING_BYTES: u64 = 268_435_456;
// Freshness hashes run sequentially through a 64 KiB stack buffer. Keep an
// additional 64 KiB for hash state, paths, identity/envelope checks and the
// already-streaming index validation. Never size this from manifest length.
pub(super) const NATIVE_METADATA_FRESHNESS_SCRATCH: u64 = 131_072;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeMetadataReadBudget {
    pub max_artifact_bytes: u64,
    pub max_working_bytes: u64,
}
impl Default for NativeMetadataReadBudget {
    fn default() -> Self {
        Self {
            max_artifact_bytes: MAX_NATIVE_METADATA_BYTES,
            max_working_bytes: MAX_NATIVE_METADATA_WORKING_BYTES,
        }
    }
}
impl NativeMetadataReadBudget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_artifact_bytes > 0
                && self.max_artifact_bytes <= MAX_NATIVE_METADATA_BYTES
                && self.max_working_bytes > 0
                && self.max_working_bytes <= MAX_NATIVE_METADATA_WORKING_BYTES,
            "resource_limit: native metadata reader budget"
        );
        Ok(())
    }
}
#[derive(Debug)]
struct LeaseReservation {
    used: Arc<AtomicU64>,
    bytes: u64,
}
impl Drop for LeaseReservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
/// Clones share one reservation. Private fields prevent forging admission.
#[derive(Clone, Debug)]
pub struct NativeMetadataLease(Arc<LeaseReservation>);
impl NativeMetadataLease {
    fn reserve(used: &Arc<AtomicU64>, bytes: u64, ceiling: u64) -> Result<Self> {
        used.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
            old.checked_add(bytes).filter(|n| *n <= ceiling)
        })
        .map_err(|_| anyhow::anyhow!("resource_limit: aggregate native metadata retention"))?;
        Ok(Self(Arc::new(LeaseReservation {
            used: used.clone(),
            bytes,
        })))
    }
    pub fn retained_bytes(&self) -> u64 {
        self.0.bytes
    }
    fn grow(&mut self, total: u64, ceiling: u64) -> Result<()> {
        let reservation =
            Arc::get_mut(&mut self.0).context("native admission lease already shared")?;
        let extra = total
            .checked_sub(reservation.bytes)
            .context("native admission cannot shrink")?;
        reservation
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                old.checked_add(extra).filter(|n| *n <= ceiling)
            })
            .map_err(|_| {
                anyhow::anyhow!("resource_limit: aggregate native metadata typed admission")
            })?;
        reservation.bytes = total;
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMetadataBinding {
    pub generation: GenerationPin,
    pub document_id: String,
    pub page: u32,
    pub artifact_id: String,
    pub artifact_sha256: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub native_profile_sha256: String,
    pub recipe: String,
}
/// Older font metadata retains only the separately qualified character/layout
/// fields. Unknown old raw object JSON is never upgraded into complete graphics.
#[derive(Debug, Deserialize)]
struct FontMetadata {
    page: liteparse_page::PageAlias,
    geometry: PageGeometry,
    native_text_metadata: NativeTextMetadata,
    capabilities: BTreeMap<String, FeatureState<String>>,
}
// The parse crate exports the accepted layout DTO without a new dependency.
mod liteparse_page {
    pub(super) type PageAlias = gptgrep_parse::NativeLayoutPage;
}
#[derive(Debug)]
enum Payload {
    Graphics(Box<NativePageMetadata>),
    Fonts(Box<FontMetadata>),
}
#[derive(Debug)]
struct SnapshotInner {
    binding: NativeMetadataBinding,
    payload: Payload,
    _lease: NativeMetadataLease,
    working_limit: u64,
}
#[derive(Clone, Debug)]
pub struct NativePageMetadataSnapshot {
    inner: Arc<SnapshotInner>,
}
impl NativePageMetadataSnapshot {
    pub fn binding(&self) -> &NativeMetadataBinding {
        &self.inner.binding
    }
    pub fn graphics(&self) -> Option<&NativeGraphicsMetadata> {
        match &self.inner.payload {
            Payload::Graphics(m) => Some(&m.graphics),
            _ => None,
        }
    }
    pub fn objects(&self) -> Option<&[NativeObject]> {
        match &self.inner.payload {
            Payload::Graphics(m) => Some(&m.objects),
            _ => None,
        }
    }
    pub fn native_text(&self) -> &NativeTextMetadata {
        match &self.inner.payload {
            Payload::Graphics(m) => &m.native_text_metadata,
            Payload::Fonts(m) => &m.native_text_metadata,
        }
    }
    pub fn geometry(&self) -> &PageGeometry {
        match &self.inner.payload {
            Payload::Graphics(m) => &m.geometry,
            Payload::Fonts(m) => &m.geometry,
        }
    }
    pub fn page(&self) -> &gptgrep_parse::NativeLayoutPage {
        match &self.inner.payload {
            Payload::Graphics(m) => &m.page,
            Payload::Fonts(m) => &m.page,
        }
    }
    fn capabilities(&self) -> &BTreeMap<String, FeatureState<String>> {
        match &self.inner.payload {
            Payload::Graphics(m) => &m.capabilities,
            Payload::Fonts(m) => &m.capabilities,
        }
    }
    pub fn retained_bytes(&self) -> u64 {
        self.inner._lease.retained_bytes()
    }
    pub fn collection_len(&self, selection: &NativeMetadataSelection) -> Result<usize> {
        selection.validate()?;
        if selection.fields.iter().any(|f| f == "content_bounds") {
            ensure!(
                self.graphics().is_some(),
                "rebuild_required: complete native graphics unavailable in this recipe"
            );
        }
        match selection.collection {
            NativeMetadataCollection::ObjectSummaries => Ok(self
                .objects()
                .context("rebuild_required: complete native objects unavailable")?
                .len()),
            NativeMetadataCollection::PathSegments => {
                Ok(self.selected_path(selection)?.segments.len())
            }
            NativeMetadataCollection::ImageGeometry => Ok(self
                .objects()
                .context("rebuild_required: native image geometry unavailable")?
                .iter()
                .filter(|o| o.image.is_some())
                .count()),
            NativeMetadataCollection::NativeCharacters => Ok(self.native_text().characters.len()),
            NativeMetadataCollection::NativeWords => {
                self.page().text_items.iter().try_fold(0usize, |n, item| {
                    n.checked_add(item.words.len())
                        .context("resource_limit: word count overflow")
                })
            }
        }
    }
    fn selected_path(
        &self,
        selection: &NativeMetadataSelection,
    ) -> Result<&gptgrep_parse::native_graphics::NativePathGeometry> {
        self.objects()
            .context("rebuild_required: native paths unavailable")?
            .iter()
            .find(|o| Some(o.id.as_str()) == selection.object_id.as_deref())
            .context("native_metadata_object_not_present")?
            .path
            .as_ref()
            .context("native_metadata_object_not_path")
    }
    pub(crate) fn projection<'a>(
        &'a self,
        selection: &'a NativeMetadataSelection,
        start: usize,
        end: usize,
    ) -> Result<NativeMetadataProjection<'a>> {
        ensure!(
            start <= end && end <= self.collection_len(selection)?,
            "native_metadata_cursor_range"
        );
        Ok(NativeMetadataProjection {
            snapshot: self,
            selection,
            start,
            end,
        })
    }
}
impl RichGenerationReader {
    pub fn native_metadata_retained_bytes(&self) -> u64 {
        self.metadata_used.load(Ordering::SeqCst)
    }
    /// Keep native metadata transport copies charged to the same reader as
    /// snapshots and projections. This cannot raise the stored working cap.
    pub fn reserve_native_metadata_transport(&self, bytes: u64) -> Result<NativeMetadataLease> {
        self.reserve_native_projection(bytes, MAX_NATIVE_METADATA_WORKING_BYTES)
    }
    /// Recheck the same pinned page after transport without allocating a second
    /// typed snapshot. Admission and identity checks are shared with its reader.
    pub fn recheck_page_native_metadata(&self, artifact_id: &str) -> Result<()> {
        let _freshness = self.reserve_native_projection(
            NATIVE_METADATA_FRESHNESS_SCRATCH,
            MAX_NATIVE_METADATA_WORKING_BYTES,
        )?;
        let (document, stored, _) = self.native_metadata_assignment(artifact_id)?;
        self.verify_native_metadata_current(document, stored)
    }
    pub(crate) fn reserve_native_projection(
        &self,
        bytes: u64,
        limit: u64,
    ) -> Result<NativeMetadataLease> {
        NativeMetadataLease::reserve(
            &self.metadata_used,
            bytes,
            limit.min(self.manifest.resource_budget.owned_working_bytes),
        )
    }
    fn native_metadata_assignment(
        &self,
        id: &str,
    ) -> Result<(&RichDocument, &StoredArtifact, &NativePage)> {
        let (doc, stored) = self.find_artifact(id)?;
        ensure!(
            stored.artifact.kind == ArtifactKind::NativeMetadata && stored.artifact.page.is_some(),
            "native_metadata_page_artifact_required"
        );
        let page = doc
            .pages
            .iter()
            .find(|p| Some(p.number) == stored.artifact.page)
            .context("native metadata physical page not present")?;
        ensure!(
            page.metadata_artifact.id == id
                && serde_json::to_vec(&page.metadata_artifact)?
                    == serde_json::to_vec(&stored.artifact)?
                && stored.artifact.profile_sha256 == doc.native_profile_sha256,
            "native page metadata assignment mismatch"
        );
        Ok((doc, stored, page))
    }
    fn verify_native_metadata_current(
        &self,
        doc: &RichDocument,
        stored: &StoredArtifact,
    ) -> Result<()> {
        self.validate()?;
        self.verify_source_metered(doc)?;
        ensure!(
            hash_reader_file(
                &owned_path(&self.dir, &doc.prepared_path)?,
                self.manifest.resource_budget.document_staging_bytes,
                &self.read_meter
            )? == (
                doc.prepared.prepared_bytes,
                doc.prepared.prepared_sha256.clone()
            ),
            "prepared bytes changed"
        );
        ensure!(
            hash_reader_file(
                &owned_path(&self.dir, &stored.artifact.relative_path)?,
                stored.artifact.bytes,
                &self.read_meter
            )? == (stored.artifact.bytes, stored.artifact.sha256.clone()),
            "native metadata artifact changed"
        );
        let document_metadata =
            document_metadata_artifact(doc)?.context("native document metadata not present")?;
        ensure!(
            hash_reader_file(
                &owned_path(&self.dir, &document_metadata.artifact.relative_path)?,
                document_metadata.artifact.bytes,
                &self.read_meter
            )? == (
                document_metadata.artifact.bytes,
                document_metadata.artifact.sha256.clone()
            ),
            "native document metadata artifact changed"
        );
        self.validate()?;
        Ok(())
    }
    pub fn read_page_native_metadata(
        &self,
        artifact_id: &str,
        budget: &NativeMetadataReadBudget,
    ) -> Result<NativePageMetadataSnapshot> {
        budget.validate()?;
        ensure!(
            !self.cache_budget.strict_cache_only,
            "read_cache_oversized: native metadata has no cache under strict cache-only policy"
        );
        // Reserve before the first manifest/path freshness read. This guard
        // remains live across every nested validation, including document
        // metadata validation, alongside the input/typed ownership lease.
        let _freshness = self.reserve_native_projection(
            NATIVE_METADATA_FRESHNESS_SCRATCH,
            budget.max_working_bytes,
        )?;
        self.validate()?;
        let (doc, stored, page) = self.native_metadata_assignment(artifact_id)?;
        let recipe = match page.capabilities.get("native_recipe") {
            Some(FeatureState::Present(r))
                if r == gptgrep_parse::NATIVE_GRAPHICS_RECIPE
                    || r == gptgrep_parse::NATIVE_FONT_WORD_RECIPE =>
            {
                r
            }
            _ => anyhow::bail!(
                "rebuild_required: native metadata recipe has no qualified typed snapshot"
            ),
        };
        let bytes = stored.artifact.bytes;
        ensure!(
            bytes <= budget.max_artifact_bytes
                && bytes <= self.manifest.resource_budget.page_layout_bytes,
            "resource_limit: native metadata artifact bytes"
        );
        // First admit only input plus parser scratch. A count-only scan derives
        // the typed/vector/map/validation overlap before any typed parse.
        let preliminary = bytes
            .checked_mul(4)
            .and_then(|v| v.checked_add(65_536))
            .context("resource_limit: metadata input overflow")?;
        let mut lease = self.reserve_native_projection(preliminary, budget.max_working_bytes)?;
        self.verify_native_metadata_current(doc, stored)?;
        {
            let document_metadata =
                document_metadata_artifact(doc)?.context("native document metadata not present")?;
            let document_working = document_metadata
                .artifact
                .bytes
                .checked_mul(32)
                .and_then(|v| v.checked_add(4096))
                .context("resource_limit: native document metadata validation overflow")?;
            let _document_lease =
                self.reserve_native_projection(document_working, budget.max_working_bytes)?;
            // Recomputes the recipe profile from accepted document DPI/render
            // settings and validates all physical-page label assignments.
            self.read_document_metadata(&doc.document.id)?;
        }
        let encoded = bounded_read_metered(
            &owned_path(&self.dir, &stored.artifact.relative_path)?,
            bytes,
            &self.read_meter,
        )?;
        ensure!(
            encoded.len() as u64 == bytes
                && digest_metered(&encoded, &self.read_meter)? == stored.artifact.sha256,
            "native metadata changed while reading"
        );
        let working = gptgrep_parse::native_metadata_working_upper(&encoded)?;
        lease.grow(
            working,
            budget
                .max_working_bytes
                .min(self.manifest.resource_budget.owned_working_bytes),
        )?;
        let payload = if recipe == gptgrep_parse::NATIVE_GRAPHICS_RECIPE {
            let metadata = gptgrep_parse::validate_native_graphics_metadata(
                &encoded,
                &doc.prepared,
                &doc.native_profile_sha256,
                page.number,
                &self.manifest.resource_budget,
            )?;
            validate_selected_page(
                &metadata.page,
                &metadata.geometry,
                &metadata.capabilities,
                &metadata.complexity,
                page,
            )?;
            Payload::Graphics(Box::new(metadata))
        } else {
            gptgrep_parse::validate_native_text_metadata(
                &encoded,
                &doc.prepared,
                page.number,
                &self.manifest.resource_budget,
            )?;
            let metadata: FontMetadata = serde_json::from_slice(&encoded)?;
            ensure!(
                metadata.page.page_number == page.number as usize
                    && serde_json::to_vec(&metadata.geometry)?
                        == serde_json::to_vec(&page.geometry)?
                    && serde_json::to_vec(&metadata.capabilities)?
                        == serde_json::to_vec(&page.capabilities)?,
                "native font snapshot page binding mismatch"
            );
            Payload::Fonts(Box::new(metadata))
        };
        self.verify_native_metadata_current(doc, stored)?;
        Ok(NativePageMetadataSnapshot {
            inner: Arc::new(SnapshotInner {
                binding: NativeMetadataBinding {
                    generation: self.pin(),
                    document_id: doc.document.id.clone(),
                    page: page.number,
                    artifact_id: artifact_id.into(),
                    artifact_sha256: stored.artifact.sha256.clone(),
                    source_sha256: doc.document.source_sha256.clone(),
                    prepared_sha256: doc.prepared.prepared_sha256.clone(),
                    native_profile_sha256: doc.native_profile_sha256.clone(),
                    recipe: recipe.clone(),
                },
                payload,
                _lease: lease,
                working_limit: budget.max_working_bytes,
            }),
        })
    }
    pub(crate) fn recheck_native_metadata(
        &self,
        snapshot: &NativePageMetadataSnapshot,
    ) -> Result<()> {
        // Snapshot clones may be rechecked concurrently. Each operation owns
        // its scratch reservation; sharing the retained snapshot is not a
        // reason to share a simultaneously active validation buffer.
        let _freshness = self.reserve_native_projection(
            NATIVE_METADATA_FRESHNESS_SCRATCH,
            snapshot.inner.working_limit,
        )?;
        let b = snapshot.binding();
        ensure!(
            b.generation == self.pin,
            "native snapshot generation differs"
        );
        let (doc, stored, page) = self.native_metadata_assignment(&b.artifact_id)?;
        ensure!(
            b.document_id == doc.document.id
                && b.page == page.number
                && b.artifact_sha256 == stored.artifact.sha256
                && b.source_sha256 == doc.document.source_sha256
                && b.prepared_sha256 == doc.prepared.prepared_sha256
                && b.native_profile_sha256 == doc.native_profile_sha256,
            "native snapshot identity differs"
        );
        self.verify_native_metadata_current(doc, stored)
    }
}
fn validate_selected_page(
    page: &gptgrep_parse::NativeLayoutPage,
    geometry: &PageGeometry,
    capabilities: &BTreeMap<String, FeatureState<String>>,
    complexity: &gptgrep_parse::NativePageComplexity,
    expected: &NativePage,
) -> Result<()> {
    ensure!(
        page.page_number == expected.number as usize
            && f64::from(page.page_width) == expected.width_points
            && f64::from(page.page_height) == expected.height_points
            && serde_json::to_vec(geometry)? == serde_json::to_vec(&expected.geometry)?
            && serde_json::to_vec(capabilities)? == serde_json::to_vec(&expected.capabilities)?
            && serde_json::to_vec(complexity)? == serde_json::to_vec(&expected.complexity)?,
        "native metadata page binding mismatch"
    );
    Ok(())
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeMetadataCollection {
    ObjectSummaries,
    PathSegments,
    ImageGeometry,
    NativeCharacters,
    NativeWords,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMetadataSelection {
    #[serde(default)]
    pub fields: Vec<String>,
    pub collection: NativeMetadataCollection,
    #[serde(default)]
    pub item_fields: Vec<String>,
    #[serde(default)]
    pub object_id: Option<String>,
}
pub const NATIVE_METADATA_SCALAR_FIELDS: &[&str] =
    &["geometry", "content_bounds", "availability", "counts"];
pub const NATIVE_OBJECT_FIELDS: &[&str] = &[
    "id",
    "ordinal",
    "ordinal_path",
    "kind",
    "raw_kind",
    "raw_matrix",
    "ancestor_matrix",
    "local_to_page",
    "native_bounds",
    "page_bounds_quad",
    "page_bounds",
    "child_count",
    "segment_count",
    "paint",
];
pub const NATIVE_SEGMENT_FIELDS: &[&str] = &[
    "ordinal",
    "kind",
    "raw_point",
    "page_point",
    "closes_subpath",
];
pub const NATIVE_IMAGE_FIELDS: &[&str] = &[
    "object_id",
    "pixel_width",
    "pixel_height",
    "bits_per_pixel",
    "colorspace",
    "marked_content_id",
    "horizontal_dpi",
    "vertical_dpi",
    "page_quad",
    "page_envelope",
    "placement_rule",
    "pixel_orientation",
];
pub const NATIVE_CHARACTER_FIELDS: &[&str] = &[
    "ordinal",
    "unicode",
    "char_code",
    "has_unicode_map_error",
    "generated",
    "geometry",
    "font_size",
    "font_weight",
    "font",
];
pub const NATIVE_WORD_FIELDS: &[&str] =
    &["ordinal", "item_ordinal", "word_ordinal", "text", "bounds"];
impl NativeMetadataSelection {
    pub fn allowed_item_fields(&self) -> &'static [&'static str] {
        match self.collection {
            NativeMetadataCollection::ObjectSummaries => NATIVE_OBJECT_FIELDS,
            NativeMetadataCollection::PathSegments => NATIVE_SEGMENT_FIELDS,
            NativeMetadataCollection::ImageGeometry => NATIVE_IMAGE_FIELDS,
            NativeMetadataCollection::NativeCharacters => NATIVE_CHARACTER_FIELDS,
            NativeMetadataCollection::NativeWords => NATIVE_WORD_FIELDS,
        }
    }
    pub fn validate(&self) -> Result<()> {
        for (fields, allowed) in [
            (&self.fields, NATIVE_METADATA_SCALAR_FIELDS),
            (&self.item_fields, self.allowed_item_fields()),
        ] {
            let mut seen = BTreeSet::new();
            ensure!(
                fields.len() <= allowed.len()
                    && fields
                        .iter()
                        .all(|f| allowed.contains(&f.as_str()) && seen.insert(f.as_str())),
                "native_metadata_invalid_fields"
            );
        }
        ensure!(
            self.object_id.is_some() == (self.collection == NativeMetadataCollection::PathSegments)
                && self.object_id.as_deref().is_none_or(valid_digest),
            "native_metadata_invalid_object_selector"
        );
        Ok(())
    }
    pub fn normalized(&self) -> Self {
        let mut value = self.clone();
        value.fields.sort();
        value.item_fields.sort();
        value
    }
}
fn selected(fields: &[String], name: &str) -> bool {
    fields.is_empty() || fields.iter().any(|f| f == name)
}
pub(crate) struct NativeMetadataProjection<'a> {
    snapshot: &'a NativePageMetadataSnapshot,
    selection: &'a NativeMetadataSelection,
    start: usize,
    end: usize,
}
impl Serialize for NativeMetadataProjection<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("schema_version", "gptgrep.native_metadata.inspection.v1")?;
        map.serialize_entry("binding", self.snapshot.binding())?;
        map.serialize_entry("metadata_only", &true)?;
        map.serialize_entry("citation_authority", &false)?;
        map.serialize_entry("source_status", "verified_current_source_prepared_artifact")?;
        if selected(&self.selection.fields, "geometry") {
            map.serialize_entry("geometry", self.snapshot.geometry())?;
        }
        if self.selection.fields.iter().any(|f| f == "content_bounds") {
            let g = self
                .snapshot
                .graphics()
                .ok_or_else(|| serde::ser::Error::custom("rebuild_required"))?;
            #[derive(Serialize)]
            struct Bounds<'a> {
                bounds: &'a FeatureState<gptgrep_parse::native_graphics::NativeRect>,
                quad: &'a FeatureState<gptgrep_parse::native_graphics::NativeQuad>,
                semantics: &'a str,
            }
            map.serialize_entry(
                "content_bounds",
                &Bounds {
                    bounds: &g.content_bounds,
                    quad: &g.content_bounds_quad,
                    semantics: &g.content_bounds_semantics,
                },
            )?;
        }
        if selected(&self.selection.fields, "availability") {
            map.serialize_entry("availability", self.snapshot.capabilities())?;
        }
        if selected(&self.selection.fields, "counts") {
            #[derive(Serialize)]
            struct Counts {
                objects: Option<u32>,
                top_level: Option<u32>,
                segments: Option<u32>,
                characters: u32,
                words: usize,
            }
            let g = self.snapshot.graphics();
            let words = self
                .snapshot
                .page()
                .text_items
                .iter()
                .map(|i| i.words.len())
                .sum();
            map.serialize_entry(
                "counts",
                &Counts {
                    objects: g.map(|v| v.object_count),
                    top_level: g.map(|v| v.top_level_count),
                    segments: g.map(|v| v.segment_count),
                    characters: self.snapshot.native_text().native_char_count,
                    words,
                },
            )?;
        }
        map.serialize_entry("collection", &self.selection.collection)?;
        map.serialize_entry("rows", &Rows { projection: self })?;
        map.end()
    }
}
struct Rows<'a, 'b> {
    projection: &'a NativeMetadataProjection<'b>,
}
impl Serialize for Rows<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let p = self.projection;
        let mut seq = s.serialize_seq(Some(p.end - p.start))?;
        match p.selection.collection {
            NativeMetadataCollection::ObjectSummaries => {
                for object in &p
                    .snapshot
                    .objects()
                    .ok_or_else(|| serde::ser::Error::custom("rebuild_required"))?
                    [p.start..p.end]
                {
                    seq.serialize_element(&ObjectRow {
                        object,
                        fields: &p.selection.item_fields,
                    })?;
                }
            }
            NativeMetadataCollection::PathSegments => {
                let path = p
                    .snapshot
                    .selected_path(p.selection)
                    .map_err(serde::ser::Error::custom)?;
                for row in &path.segments[p.start..p.end] {
                    seq.serialize_element(&SegmentRow {
                        row,
                        fields: &p.selection.item_fields,
                    })?;
                }
            }
            NativeMetadataCollection::ImageGeometry => {
                for object in p
                    .snapshot
                    .objects()
                    .ok_or_else(|| serde::ser::Error::custom("rebuild_required"))?
                    .iter()
                    .filter(|o| o.image.is_some())
                    .skip(p.start)
                    .take(p.end - p.start)
                {
                    seq.serialize_element(&ImageRow {
                        object,
                        fields: &p.selection.item_fields,
                    })?;
                }
            }
            NativeMetadataCollection::NativeCharacters => {
                for row in &p.snapshot.native_text().characters[p.start..p.end] {
                    seq.serialize_element(&CharacterRow {
                        row,
                        fields: &p.selection.item_fields,
                    })?;
                }
            }
            NativeMetadataCollection::NativeWords => {
                for (ordinal, (item, word, row)) in p
                    .snapshot
                    .page()
                    .text_items
                    .iter()
                    .enumerate()
                    .flat_map(|(i, item)| {
                        item.words
                            .iter()
                            .enumerate()
                            .map(move |(w, row)| (i, w, row))
                    })
                    .enumerate()
                    .skip(p.start)
                    .take(p.end - p.start)
                {
                    seq.serialize_element(&WordRow {
                        ordinal,
                        item,
                        word,
                        row,
                        fields: &p.selection.item_fields,
                    })?;
                }
            }
        }
        seq.end()
    }
}
macro_rules! field {
    ($map:ident,$fields:expr,$name:literal,$value:expr) => {
        if selected($fields, $name) {
            $map.serialize_entry($name, $value)?;
        }
    };
}
struct ObjectRow<'a> {
    object: &'a NativeObject,
    fields: &'a [String],
}
impl Serialize for ObjectRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let o = self.object;
        let mut m = s.serialize_map(None)?;
        let f = self.fields;
        field!(m, f, "id", &o.id);
        field!(m, f, "ordinal", &o.ordinal);
        field!(m, f, "ordinal_path", &o.ordinal_path);
        field!(m, f, "kind", &o.kind);
        field!(m, f, "raw_kind", &o.raw_kind);
        field!(m, f, "raw_matrix", &o.raw_matrix);
        field!(m, f, "ancestor_matrix", &o.ancestor_matrix);
        field!(m, f, "local_to_page", &o.local_to_page);
        field!(m, f, "native_bounds", &o.native_bounds);
        field!(m, f, "page_bounds_quad", &o.page_bounds_quad);
        field!(m, f, "page_bounds", &o.page_bounds);
        field!(m, f, "child_count", &o.child_count);
        field!(
            m,
            f,
            "segment_count",
            &o.path.as_ref().map(|p| p.native_segment_count)
        );
        field!(m, f, "paint", &o.path.as_ref().map(|p| &p.paint));
        m.end()
    }
}
struct SegmentRow<'a> {
    row: &'a gptgrep_parse::native_graphics::NativePathSegment,
    fields: &'a [String],
}
impl Serialize for SegmentRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let r = self.row;
        let f = self.fields;
        let mut m = s.serialize_map(None)?;
        field!(m, f, "ordinal", &r.ordinal);
        field!(m, f, "kind", &r.kind);
        field!(m, f, "raw_point", &r.raw_point);
        field!(m, f, "page_point", &r.page_point);
        field!(m, f, "closes_subpath", &r.closes_subpath);
        m.end()
    }
}
struct ImageRow<'a> {
    object: &'a NativeObject,
    fields: &'a [String],
}
impl Serialize for ImageRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let r = self.object.image.as_ref().expect("selected image");
        let f = self.fields;
        let mut m = s.serialize_map(None)?;
        field!(m, f, "object_id", &self.object.id);
        field!(m, f, "pixel_width", &r.pixel_width);
        field!(m, f, "pixel_height", &r.pixel_height);
        field!(m, f, "bits_per_pixel", &r.bits_per_pixel);
        field!(m, f, "colorspace", &r.colorspace);
        field!(m, f, "marked_content_id", &r.marked_content_id);
        field!(m, f, "horizontal_dpi", &r.horizontal_dpi);
        field!(m, f, "vertical_dpi", &r.vertical_dpi);
        field!(m, f, "page_quad", &r.page_quad);
        field!(m, f, "page_envelope", &r.page_envelope);
        field!(m, f, "placement_rule", &r.placement_rule);
        field!(m, f, "pixel_orientation", &r.pixel_orientation);
        m.end()
    }
}
struct CharacterRow<'a> {
    row: &'a gptgrep_parse::NativeCharacter,
    fields: &'a [String],
}
impl Serialize for CharacterRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let r = self.row;
        let f = self.fields;
        let mut m = s.serialize_map(None)?;
        field!(m, f, "ordinal", &r.ordinal);
        field!(m, f, "unicode", &r.unicode);
        field!(m, f, "char_code", &r.char_code);
        field!(m, f, "has_unicode_map_error", &r.has_unicode_map_error);
        field!(m, f, "generated", &r.generated);
        field!(m, f, "geometry", &r.geometry);
        field!(m, f, "font_size", &r.font_size);
        field!(m, f, "font_weight", &r.font_weight);
        field!(m, f, "font", &r.font);
        m.end()
    }
}
struct WordRow<'a> {
    ordinal: usize,
    item: usize,
    word: usize,
    row: &'a gptgrep_parse::NativeWordBox,
    fields: &'a [String],
}
impl Serialize for WordRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let f = self.fields;
        let mut m = s.serialize_map(None)?;
        let r = self.row;
        field!(m, f, "ordinal", &self.ordinal);
        field!(m, f, "item_ordinal", &self.item);
        field!(m, f, "word_ordinal", &self.word);
        field!(m, f, "text", &r.text);
        #[derive(Serialize)]
        struct Bounds {
            x: f32,
            y: f32,
            width: f32,
            height: f32,
        }
        field!(
            m,
            f,
            "bounds",
            &Bounds {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height
            }
        );
        m.end()
    }
}
