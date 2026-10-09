//! Exact native character retention, with source font bytes and glyph words.
//! Normalized positioned layout is a separate, explicitly bound projection.

use anyhow::{Context, Result, ensure};
use liteparse::types::{Page, TextItem, WordBox};
use serde::{Deserialize, Serialize};

use crate::{FeatureState, PreparedSummary, ResourceBudget};

pub const NATIVE_FONT_WORD_RECIPE: &str = "gptgrep.native.count-admitted.v6-font-words";
const TEXT_SCHEMA: &str = "gptgrep.native.character-font-words.v2";
const WORD_RECIPE: &str = "native-character-order-whitespace-and-run-boundaries-v2; actual-glyph-box-unions; no-invented-spaces; not-linguistic-segmentation";
const LAYOUT_RECIPE: &str = "positioned-nonwhitespace-glyphs; actual-noncontrol-whitespace-appends-to-preceding-run; prefix-and-all-whitespace-retained-only-in-stream; controls-retained-only-in-stream-and-break-runs; no-invented-boxes; invalid-unicode-displays-replacement-with-raw-codepoint-retained";
const STREAM_RECIPE: &str = "complete-contiguous-pdfium-native-character-ordinals; raw-unicode-charcode-maperror-generated-and-observed-geometry-retained; zero-unicode-may-be-NUL-or-getter-ambiguity";
const NO_FONT: &str = "pdfium_font_zero_length_unavailable_or_getter_failure";
const UNKNOWN_NAME: &str =
    "native_font_names_incomplete; character_states_retain_zero_length_or_encoding_ambiguity";
const NO_SIZE: &str = "pdfium_font_size_nonpositive_or_nonfinite_or_getter_failure";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeFontInfo {
    pub raw_name: Vec<u8>,
    pub name: FeatureState<String>,
    pub flags: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The observed native box transformed into the existing f32 viewport frame.
/// Zero-area observations remain zero-area; MissingBox carries no rectangle.
pub(super) struct NativeBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "bounds",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum NativeGeometry {
    MissingBox,
    ZeroArea(NativeBounds),
    Positioned(NativeBounds),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeCharacter {
    pub ordinal: u32,
    /// Exact native u32, even when it is not a Unicode scalar value.
    pub unicode: u32,
    pub char_code: u32,
    pub has_unicode_map_error: bool,
    pub generated: bool,
    pub geometry: NativeGeometry,
    pub font_size: FeatureState<f64>,
    pub font_weight: i32,
    pub font: FeatureState<NativeFontInfo>,
}

/// Scalar observations from the actual extraction loop. The shared collector
/// has no filtering step, so source indices cannot disappear before retention.
pub(super) struct NativeCharacterObservation {
    pub unicode: u32,
    pub char_code: u32,
    pub has_unicode_map_error: bool,
    pub generated: bool,
    pub bounds: Option<NativeBounds>,
    pub font_size: f64,
    pub font_weight: i32,
    pub font: FeatureState<NativeFontInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeTextMetadata {
    pub schema_version: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub page: u32,
    pub native_char_count: u32,
    pub stream_recipe: String,
    pub layout_recipe: String,
    pub word_recipe: String,
    pub characters: Vec<NativeCharacter>,
}

pub(super) fn font_state(value: Option<(Vec<u8>, i32)>) -> FeatureState<NativeFontInfo> {
    match value {
        Some((raw_name, flags)) => {
            let name = decoded_name(&raw_name);
            FeatureState::Present(NativeFontInfo {
                raw_name,
                name,
                flags,
            })
        }
        None => FeatureState::Unsupported(NO_FONT.into()),
    }
}

fn decoded_name(raw: &[u8]) -> FeatureState<String> {
    match std::str::from_utf8(raw) {
        Ok("") => FeatureState::Unsupported("pdfium_font_empty_name".into()),
        Ok(name) => FeatureState::Present(name.into()),
        Err(_) => FeatureState::Unsupported("pdfium_font_name_non_utf8".into()),
    }
}

fn name(font: &FeatureState<NativeFontInfo>) -> Option<&str> {
    match font {
        FeatureState::Present(NativeFontInfo {
            name: FeatureState::Present(name),
            ..
        }) => Some(name),
        _ => None,
    }
}

fn character(value: &NativeCharacter) -> char {
    char::from_u32(value.unicode).unwrap_or('\u{fffd}')
}

fn validate_character(
    value: &NativeCharacter,
    width: f32,
    height: f32,
    budget: &ResourceBudget,
) -> Result<()> {
    let c = character(value);
    let bounds = match &value.geometry {
        NativeGeometry::MissingBox => None,
        NativeGeometry::Positioned(bounds) => {
            ensure!(
                bounds.width > 0.0 && bounds.height > 0.0,
                "native positioned character has no area"
            );
            Some(bounds)
        }
        NativeGeometry::ZeroArea(bounds) => {
            ensure!(
                bounds.width == 0.0 || bounds.height == 0.0,
                "native zero-area classification invalid"
            );
            Some(bounds)
        }
    };
    if let Some(b) = bounds {
        ensure!(
            [b.x, b.y, b.width, b.height].iter().all(|n| n.is_finite())
                && b.width >= 0.0
                && b.height >= 0.0
                && b.x >= -1.0
                && b.y >= -1.0
                && b.x + b.width <= width + 1.0
                && b.y + b.height <= height + 1.0,
            "native character geometry invalid"
        );
    }
    match &value.font_size {
        FeatureState::Present(size) => {
            ensure!(size.is_finite() && *size > 0.0, "native font size invalid")
        }
        FeatureState::Unsupported(reason) => ensure!(
            reason == NO_SIZE,
            "native font size unavailable state invalid"
        ),
        _ => anyhow::bail!("native font size availability invalid"),
    }
    if !c.is_whitespace() && !c.is_control() {
        ensure!(
            matches!(value.geometry, NativeGeometry::Positioned(_)),
            "native non-whitespace character missing or degenerate geometry"
        );
        ensure!(
            matches!(&value.font_size,FeatureState::Present(size) if (*size as f32).is_finite() && (*size as f32) > 0.0),
            "native positioned font size is unavailable or outside layout range"
        );
    }
    match &value.font {
        FeatureState::Present(info) => ensure!(
            info.raw_name.len() as u64 <= budget.native_layout_input_bytes
                && !info.raw_name.contains(&0)
                && serde_json::to_value(&info.name)?
                    == serde_json::to_value(decoded_name(&info.raw_name))?,
            "native font bytes/name mismatch"
        ),
        FeatureState::Unsupported(reason) => {
            ensure!(reason == NO_FONT, "native font unavailable state invalid")
        }
        _ => anyhow::bail!("native font availability invalid"),
    }
    Ok(())
}

/// The count/2048-byte per-character bound is admitted by extract_sync's
/// document/working preflight before this vector, including every control and
/// geometryless character. Per-font bytes have their own pre-allocation getter
/// callback. Tests use this same retention seam with scalar authored sources.
pub(super) fn collect_characters(
    count: u32,
    width: f32,
    height: f32,
    budget: &ResourceBudget,
    mut observe: impl FnMut(u32) -> Result<NativeCharacterObservation>,
) -> Result<Vec<NativeCharacter>> {
    budget.validate()?;
    ensure!(
        [width, height].iter().all(|n| n.is_finite() && *n > 0.0),
        "native page geometry invalid"
    );
    ensure!(
        u64::from(count)
            .checked_mul(2048)
            .is_some_and(|n| n <= budget.page_layout_bytes),
        "resource_limit: complete native character count before allocation"
    );
    let mut characters = Vec::new();
    characters.try_reserve_exact(count as usize)?;
    for ordinal in 0..count {
        let observation = observe(ordinal)?;
        let geometry = match observation.bounds {
            None => NativeGeometry::MissingBox,
            Some(b) if b.width == 0.0 || b.height == 0.0 => NativeGeometry::ZeroArea(b),
            Some(b) => NativeGeometry::Positioned(b),
        };
        let value = NativeCharacter {
            ordinal,
            unicode: observation.unicode,
            char_code: observation.char_code,
            has_unicode_map_error: observation.has_unicode_map_error,
            generated: observation.generated,
            geometry,
            font_size: if observation.font_size.is_finite() && observation.font_size > 0.0 {
                FeatureState::Present(observation.font_size)
            } else {
                FeatureState::Unsupported(NO_SIZE.into())
            },
            font_weight: observation.font_weight,
            font: observation.font,
        };
        validate_character(&value, width, height, budget)?;
        characters.push(value);
    }
    Ok(characters)
}

fn same_font(a: &FeatureState<NativeFontInfo>, b: &FeatureState<NativeFontInfo>) -> bool {
    matches!((a,b),(FeatureState::Present(a),FeatureState::Present(b))
        if a.raw_name == b.raw_name && a.flags == b.flags)
}

fn mergeable(a: &TextItem, b: &TextItem) -> bool {
    let font = a.font_size.unwrap_or(0.0);
    let gap = b.x - (a.x + a.width);
    a.font_size == b.font_size
        && a.font_weight == b.font_weight
        && a.y < b.y + b.height
        && b.y < a.y + a.height
        && b.x >= a.x
        && gap >= -font * 0.5
        && gap <= font
}

fn positioned_item(value: &NativeCharacter) -> TextItem {
    let NativeGeometry::Positioned(b) = &value.geometry else {
        unreachable!("validated positioned character")
    };
    let FeatureState::Present(size) = &value.font_size else {
        unreachable!("validated positioned font size")
    };
    let text = character(value).to_string();
    TextItem {
        text: text.clone(),
        x: b.x,
        y: b.y,
        width: b.width,
        height: b.height,
        font_name: name(&value.font).map(str::to_owned),
        font_size: Some(*size as f32),
        font_weight: Some(value.font_weight),
        font_flags: match &value.font {
            FeatureState::Present(info) => Some(info.flags),
            _ => None,
        },
        has_unicode_map_error: value.has_unicode_map_error,
        char_codes: vec![value.char_code],
        // Only an actual trailing whitespace character can set this flag.
        trailing_space_generated: false,
        words: vec![WordBox {
            text,
            x: b.x,
            y: b.y,
            width: b.width,
            height: b.height,
        }],
        ..Default::default()
    }
}

pub(super) fn glyph_runs(characters: &[NativeCharacter]) -> Vec<TextItem> {
    let mut runs: Vec<TextItem> = Vec::with_capacity(characters.len());
    let mut run_font: Option<&FeatureState<NativeFontInfo>> = None;
    for value in characters {
        let c = character(value);
        if c.is_control() {
            // The full stream retains the code point and generated flag.
            // Control normalization does not create a space or a glyph box.
            run_font = None;
            continue;
        }
        if c.is_whitespace() {
            if run_font.is_some()
                && let Some(last) = runs.last_mut()
            {
                last.text.push(c);
                last.char_codes.push(value.char_code);
                last.has_unicode_map_error |= value.has_unicode_map_error;
                last.trailing_space_generated = value.generated;
            }
            continue;
        }
        let mut item = positioned_item(value);
        if let Some(last) = runs.last_mut()
            && run_font.is_some_and(|font| same_font(font, &value.font))
            && mergeable(last, &item)
        {
            if !last.text.ends_with(char::is_whitespace) {
                let word = last.words.last_mut().expect("positioned word");
                let first = item.words.remove(0);
                word.text.push_str(&first.text);
                let right = (word.x + word.width).max(first.x + first.width);
                let bottom = (word.y + word.height).max(first.y + first.height);
                word.x = word.x.min(first.x);
                word.y = word.y.min(first.y);
                word.width = right - word.x;
                word.height = bottom - word.y;
            }
            last.words.extend(item.words);
            last.text.push_str(&item.text);
            last.char_codes.extend(item.char_codes);
            last.has_unicode_map_error |= item.has_unicode_map_error;
            last.trailing_space_generated = false;
            let right = (last.x + last.width).max(item.x + item.width);
            let bottom = (last.y + last.height).max(item.y + item.height);
            last.y = last.y.min(item.y);
            last.width = right - last.x;
            last.height = bottom - last.y;
        } else {
            runs.push(item);
            run_font = Some(&value.font);
        }
    }
    runs
}

pub(super) fn capabilities(
    characters: &[NativeCharacter],
) -> (FeatureState<String>, FeatureState<String>) {
    if characters.is_empty() {
        return (FeatureState::Absent, FeatureState::Absent);
    }
    // Font availability describes the full retained source stream. A control
    // with an unavailable getter remains explicit rather than disappearing.
    let fonts = if characters.iter().all(|value| name(&value.font).is_some()) {
        FeatureState::Present(
            "exact-native-font-info-utf8-v1; every-character-raw-state-retained".into(),
        )
    } else {
        FeatureState::Unsupported(UNKNOWN_NAME.into())
    };
    let words = if characters.iter().any(|value| {
        let c = character(value);
        !c.is_whitespace() && !c.is_control()
    }) {
        FeatureState::Present(WORD_RECIPE.into())
    } else {
        FeatureState::Absent
    };
    (fonts, words)
}

pub(super) fn metadata(
    summary: &PreparedSummary,
    page: u32,
    count: u32,
    characters: Vec<NativeCharacter>,
) -> NativeTextMetadata {
    NativeTextMetadata {
        schema_version: TEXT_SCHEMA.into(),
        source_sha256: summary.source_sha256.clone(),
        prepared_sha256: summary.prepared_sha256.clone(),
        page,
        native_char_count: count,
        stream_recipe: STREAM_RECIPE.into(),
        layout_recipe: LAYOUT_RECIPE.into(),
        word_recipe: WORD_RECIPE.into(),
        characters,
    }
}

pub fn native_font_word_profile_digest(
    summary: &PreparedSummary,
    dpi: f32,
    render_pages: bool,
) -> Result<String> {
    crate::artifact::json_sha256(
        &(
            NATIVE_FONT_WORD_RECIPE,
            crate::document_metadata_native_profile_digest(summary, dpi, render_pages)?,
            TEXT_SCHEMA,
            STREAM_RECIPE,
            LAYOUT_RECIPE,
            WORD_RECIPE,
            "font-info-length/exact-nul-write-v1; strict-utf8-or-typed-unavailable; no-lossy-names; complete-character-snapshot; font-capability-scope-all-native-characters; word-scope-positioned-layout; native-font-size-f64-layout-font-size-f32; existing-viewport-transform-f32",
        ),
        8192,
    )
}

/// Structural validation after owning artifact/source/profile verification.
/// This reconstructs layout separately from complete character retention; it
/// never opens a PDF or invents missing old-recipe source data.
pub fn validate_native_text_metadata(
    bytes: &[u8],
    summary: &PreparedSummary,
    physical_page: u32,
    budget: &ResourceBudget,
) -> Result<()> {
    budget.validate()?;
    ensure!(
        physical_page > 0 && physical_page <= crate::MAX_NATIVE_PAGES,
        "native physical page invalid"
    );
    ensure!(
        bytes.len() as u64 <= budget.page_layout_bytes
            && (bytes.len() as u64)
                .checked_mul(64)
                .is_some_and(|n| n <= budget.owned_working_bytes),
        "resource_limit: native font/word validation admission"
    );
    #[derive(Deserialize)]
    struct Envelope {
        page: Page,
        native_text_metadata: NativeTextMetadata,
        capabilities: std::collections::BTreeMap<String, FeatureState<String>>,
    }
    let envelope: Envelope = serde_json::from_slice(bytes)
        .context("native character/font/word metadata missing or invalid")?;
    let text = &envelope.native_text_metadata;
    ensure!(
        text.schema_version == TEXT_SCHEMA
            && text.stream_recipe == STREAM_RECIPE
            && text.layout_recipe == LAYOUT_RECIPE
            && text.word_recipe == WORD_RECIPE
            && text.source_sha256 == summary.source_sha256
            && text.prepared_sha256 == summary.prepared_sha256
            && text.page == physical_page
            && envelope.page.page_number == physical_page as usize,
        "native character/font/word source/page/recipe mismatch"
    );
    ensure!(
        [envelope.page.page_width, envelope.page.page_height]
            .iter()
            .all(|n| n.is_finite() && *n > 0.0),
        "native page geometry invalid"
    );
    ensure!(
        u64::from(text.native_char_count)
            .checked_mul(2048)
            .is_some_and(|n| n <= budget.page_layout_bytes)
            && text.characters.len() == text.native_char_count as usize,
        "native character count or complete coverage invalid"
    );
    for (ordinal, value) in text.characters.iter().enumerate() {
        ensure!(
            value.ordinal as usize == ordinal,
            "native character ordinal missing, duplicate or reordered"
        );
        validate_character(
            value,
            envelope.page.page_width,
            envelope.page.page_height,
            budget,
        )?;
    }
    let expected = glyph_runs(&text.characters);
    crate::resource::encoded_size(&expected, budget.page_layout_bytes)?;
    crate::resource::encoded_size(&envelope.page.text_items, budget.page_layout_bytes)?;
    ensure!(
        serde_json::to_vec(&expected)? == serde_json::to_vec(&envelope.page.text_items)?,
        "native layout/font/word items differ from complete characters"
    );
    let (fonts, words) = capabilities(&text.characters);
    ensure!(
        serde_json::to_value(envelope.capabilities.get("font_names"))?
            == serde_json::to_value(Some(fonts))?
            && serde_json::to_value(envelope.capabilities.get("word_segmentation"))?
                == serde_json::to_value(Some(words))?,
        "native font/word availability mismatch"
    );
    ensure!(
        matches!(envelope.capabilities.get("native_stream"),Some(FeatureState::Present(recipe)) if recipe==STREAM_RECIPE),
        "native complete character stream availability mismatch"
    );
    Ok(())
}

pub(super) fn stream_recipe() -> &'static str {
    STREAM_RECIPE
}

#[cfg(test)]
#[path = "native_text_metadata_tests.rs"]
mod tests;
