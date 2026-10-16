//! Bounded, source-only PDF document metadata. These APIs never initialize a
//! form environment or execute document actions. Zero/empty getter ambiguity
//! is retained explicitly; native read failures fail the whole candidate.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use liteparse::stages::Document;
use serde::{Deserialize, Serialize};

use crate::{FeatureState, PreparedSummary, ResourceBudget};

pub const DOCUMENT_METADATA_SCHEMA: &str = "gptgrep.native.document_metadata.v1";
pub const MAX_NATIVE_BOOKMARKS: usize = 1024;
pub const MAX_NATIVE_BOOKMARK_DEPTH: u8 = 32;
pub const DOCUMENT_METADATA_NATIVE_RECIPE: &str =
    "gptgrep.native.count-admitted.v5-document-metadata";
const INFO_TAGS: [(&str, &std::ffi::CStr); 8] = [
    ("Title", c"Title"),
    ("Author", c"Author"),
    ("Subject", c"Subject"),
    ("Keywords", c"Keywords"),
    ("Creator", c"Creator"),
    ("Producer", c"Producer"),
    ("CreationDate", c"CreationDate"),
    ("ModDate", c"ModDate"),
];
const NO_VALUE: &str = "pdfium_zero_length_no_value_or_getter_failure";
const EMPTY_VALUE: &str = "pdfium_empty_or_missing_value";
const NO_DESTINATION: &str = "pdfium_action_present_or_no_direct_destination";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePageLabel {
    /// One-based physical page, independent of its display label.
    pub physical_page: u32,
    pub label: FeatureState<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBookmarkDestination {
    pub physical_page: u32,
    /// PDF user-space coordinate, never a text line or normalized viewport y.
    pub source_y: FeatureState<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBookmark {
    pub ordinal: u32,
    pub depth: u8,
    pub title: String,
    pub destination: FeatureState<NativeBookmarkDestination>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDocumentMetadata {
    pub schema_version: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub physical_page_count: u32,
    pub native_dpi: f32,
    pub native_render_pages: bool,
    /// Fixed PDF Info tags; not an enumeration of custom keys or XMP.
    pub info: BTreeMap<String, FeatureState<String>>,
    pub page_labels: Vec<NativePageLabel>,
    pub bookmarks: FeatureState<Vec<NativeBookmark>>,
    pub capabilities: BTreeMap<String, FeatureState<String>>,
}

fn value_state(value: Option<String>) -> FeatureState<String> {
    match value {
        None => FeatureState::Unsupported(NO_VALUE.into()),
        Some(text) if text.is_empty() => FeatureState::Unsupported(EMPTY_VALUE.into()),
        Some(text) => FeatureState::Present(text),
    }
}

pub(crate) fn extract(
    doc: &Document<'_>,
    summary: &PreparedSummary,
    total: u32,
    options: &crate::NativeOptions,
    budget: &ResourceBudget,
    admit: &mut impl FnMut(u64) -> Result<()>,
) -> Result<NativeDocumentMetadata> {
    ensure!(
        total > 0 && total <= crate::MAX_NATIVE_PAGES,
        "invalid physical metadata page count"
    );
    // Fixed tag map, complete page-label entries, DTO/capability scalars and
    // subsequent copies are admitted before allocating the collections.
    admit(
        u64::from(total)
            .checked_mul(512)
            .and_then(|n| n.checked_add(8192))
            .ok_or_else(|| anyhow::anyhow!("resource_limit: document metadata count overflow"))?,
    )?;
    let mut info = BTreeMap::new();
    for (tag, c_tag) in INFO_TAGS {
        let value = doc.meta_text_admitted(c_tag, budget.native_layout_input_bytes, admit)?;
        info.insert(tag.into(), value_state(value));
    }
    let mut page_labels = Vec::new();
    page_labels.try_reserve_exact(total as usize)?;
    for index in 0..total {
        let value =
            doc.page_label_admitted(index as i32, budget.native_layout_input_bytes, admit)?;
        page_labels.push(NativePageLabel {
            physical_page: index + 1,
            label: value_state(value),
        });
    }
    let outline = doc.outline_admitted(
        MAX_NATIVE_BOOKMARKS,
        MAX_NATIVE_BOOKMARK_DEPTH,
        budget.native_layout_input_bytes,
        admit,
    )?;
    // The raw outline and mapped DTO coexist during conversion. Reserve the
    // second vector/strings before cloning even one title.
    let mapped_upper = outline.iter().try_fold(0u64, |n, entry| {
        n.checked_add(512 + entry.title.len() as u64 * 6)
            .ok_or_else(|| anyhow::anyhow!("resource_limit: bookmark mapping overflow"))
    })?;
    admit(mapped_upper)?;
    let mut bookmarks = Vec::new();
    bookmarks.try_reserve_exact(outline.len())?;
    for (index, entry) in outline.into_iter().enumerate() {
        bookmarks.push(NativeBookmark {
            ordinal: index as u32 + 1,
            depth: entry.level,
            title: entry.title,
            destination: match entry.page_index {
                Some(page) => FeatureState::Present(NativeBookmarkDestination {
                    physical_page: page as u32 + 1,
                    source_y: match entry.y {
                        Some(y) => FeatureState::Present(y),
                        None => FeatureState::Unsupported(
                            "pdfium_non_xyz_or_missing_y_or_getter_failure".into(),
                        ),
                    },
                }),
                None => FeatureState::Unsupported(NO_DESTINATION.into()),
            },
        });
    }
    let metadata = NativeDocumentMetadata {
        schema_version: DOCUMENT_METADATA_SCHEMA.into(),
        source_sha256: summary.source_sha256.clone(),
        prepared_sha256: summary.prepared_sha256.clone(),
        physical_page_count: total,
        native_dpi: options.dpi,
        native_render_pages: options.render_pages,
        info,
        page_labels,
        bookmarks: if bookmarks.is_empty() { FeatureState::Absent } else { FeatureState::Present(bookmarks) },
        capabilities: BTreeMap::from([
            ("custom_info_keys".into(), FeatureState::Unsupported("bounded_dictionary_enumeration_unavailable".into())),
            ("xmp_metadata".into(), FeatureState::Unsupported("bounded_xmp_getter_unavailable".into())),
            ("getter_semantics".into(), FeatureState::Present("length_then_exact_write_utf16le; zero_and_empty_ambiguity_retained; null_outline_reports_no_tree; direct_local_destinations_only; destination_y_ambiguity_retained; no_actions".into())),
        ]),
    };
    validate_native_document_metadata(&metadata, summary, total, budget)?;
    Ok(metadata)
}

/// Validate a retained source-bound DTO. Callers must separately verify the
/// owning artifact digest/profile and admit its bytes before deserialization.
pub fn validate_native_document_metadata(
    metadata: &NativeDocumentMetadata,
    summary: &PreparedSummary,
    physical_pages: u32,
    budget: &ResourceBudget,
) -> Result<()> {
    budget.validate()?;
    ensure!(
        metadata.schema_version == DOCUMENT_METADATA_SCHEMA
            && metadata.source_sha256 == summary.source_sha256
            && metadata.prepared_sha256 == summary.prepared_sha256
            && metadata.physical_page_count == physical_pages
            && physical_pages > 0
            && physical_pages <= crate::MAX_NATIVE_PAGES
            && metadata.page_labels.len() == physical_pages as usize,
        "native document metadata source/physical page binding mismatch"
    );
    ensure!(
        metadata.native_dpi.is_finite()
            && metadata.native_dpi > 0.0
            && metadata.native_dpi <= 300.0,
        "native document metadata rendering recipe invalid"
    );
    ensure!(
        metadata.info.len() == INFO_TAGS.len()
            && INFO_TAGS
                .iter()
                .all(|(tag, _)| metadata.info.contains_key(*tag)),
        "native Info tag set mismatch"
    );
    let valid_value = |value: &FeatureState<String>| match value {
        FeatureState::Present(value) => {
            !value.is_empty()
                && value.len() as u64 <= budget.native_layout_input_bytes
                && !value.contains('\0')
        }
        FeatureState::Unsupported(reason) => reason == NO_VALUE || reason == EMPTY_VALUE,
        _ => false,
    };
    ensure!(
        metadata.info.values().all(valid_value),
        "native Info value/state invalid"
    );
    for (index, entry) in metadata.page_labels.iter().enumerate() {
        ensure!(
            entry.physical_page as usize == index + 1 && valid_value(&entry.label),
            "native display label/physical page mapping invalid"
        );
    }
    let entries = match &metadata.bookmarks {
        FeatureState::Absent => &[][..],
        FeatureState::Present(entries)
            if !entries.is_empty() && entries.len() <= MAX_NATIVE_BOOKMARKS =>
        {
            entries.as_slice()
        }
        _ => anyhow::bail!("native bookmark availability invalid"),
    };
    let mut previous_depth = 0u8;
    for (index, entry) in entries.iter().enumerate() {
        ensure!(
            entry.ordinal as usize == index + 1
                && entry.depth > 0
                && entry.depth <= MAX_NATIVE_BOOKMARK_DEPTH
                && entry.depth <= previous_depth + 1
                && entry.title.len() as u64 <= budget.native_layout_input_bytes
                && !entry.title.contains('\0'),
            "native bookmark hierarchy/title invalid"
        );
        match &entry.destination {
            FeatureState::Present(dest) => ensure!(
                dest.physical_page > 0
                    && dest.physical_page <= physical_pages
                    && match &dest.source_y {
                        FeatureState::Present(y) => y.is_finite(),
                        FeatureState::Unsupported(reason) =>
                            reason == "pdfium_non_xyz_or_missing_y_or_getter_failure",
                        _ => false,
                    },
                "native bookmark destination outside document"
            ),
            FeatureState::Unsupported(reason) if reason == NO_DESTINATION => {}
            _ => anyhow::bail!("native bookmark destination availability invalid"),
        }
        previous_depth = entry.depth;
    }
    let expected: BTreeMap<String, FeatureState<String>> = BTreeMap::from([
        ("custom_info_keys".into(), FeatureState::Unsupported("bounded_dictionary_enumeration_unavailable".into())),
        ("xmp_metadata".into(), FeatureState::Unsupported("bounded_xmp_getter_unavailable".into())),
        ("getter_semantics".into(), FeatureState::Present("length_then_exact_write_utf16le; zero_and_empty_ambiguity_retained; null_outline_reports_no_tree; direct_local_destinations_only; destination_y_ambiguity_retained; no_actions".into())),
    ]);
    ensure!(
        serde_json::to_value(&metadata.capabilities)? == serde_json::to_value(expected)?,
        "native document metadata capability provenance mismatch"
    );
    crate::resource::encoded_size(metadata, budget.document_metadata_bytes)?;
    Ok(())
}

/// Exact v5 extraction recipe. Page batching is intentionally excluded; all
/// metadata and page artifacts share the same source-bound profile.
pub fn document_metadata_native_profile_digest(
    summary: &PreparedSummary,
    dpi: f32,
    render_pages: bool,
) -> Result<String> {
    ensure!(
        dpi.is_finite() && dpi > 0.0 && dpi <= 300.0,
        "invalid native metadata DPI"
    );
    Ok(crate::artifact::sha256(&serde_json::to_vec(&(
        DOCUMENT_METADATA_NATIVE_RECIPE,
        crate::RICH_PARSER,
        crate::LITEPARSE_REVISION,
        &summary.preparation_profile_sha256,
        dpi,
        render_pages,
        "image-0.25.10/png-default-rgba8",
        "owned-liteparse-pdfium-1.11.0/annotation-count-v1; initial-render-cleared; display-render-into-flags=0x01; forms-off",
        "document-info-8-tags; complete-physical-page-labels; outline-1024-depth32; exact-length-utf16le-v1; zero-empty-destination-y-ambiguity-retained; direct-local-destinations-only",
    ))?))
}
