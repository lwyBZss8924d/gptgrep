//! Complete, count-admitted native object observations. These records describe
//! geometry; they are not rendered visibility, evidence boxes or citations.
use crate::rich::native_text_metadata::NativeTextMetadata;
use crate::{AffineTransform, FeatureState, PageGeometry, PreparedSummary, ResourceBudget};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of};

pub const NATIVE_GRAPHICS_RECIPE: &str = "gptgrep.native.count-admitted.v7-graphics";
pub const NATIVE_GRAPHICS_SCHEMA: &str = "gptgrep.native.graphics.v1";
pub const CONTENT_BOUNDS_SEMANTICS: &str = "native_reported_top_level_unclipped_geometric_union";
const PRECISION: &str = "raw_pdfium_f32; composition_and_derived_geometry_f64; viewport_coefficients_from_existing_f32_adapter";
const LIMITATIONS: &str = "geometric_metadata_only; rendered_visibility_clipping_soft_masks_blend_optional_content_paint_order_annotation_appearance_unsupported";
const OPTIONAL: &str = "pinned_binding_and_runtime_not_qualified";
pub const LAYOUT_GRAPHICS_RECIPE: &str = "gptgrep.native.layout-graphics.v1; complete-path-shapes; exact-axis-aligned-straight-segments-and-explicit-close; unmerged; curves-retained-in-native-metadata; no-pdfium-vector-parity; no-scalar-page-stroke-width";
const MAX_DEPTH: usize = 32;

pub type NativePoint = [f64; 2];
pub type NativeQuad = [NativePoint; 4];
pub type NativeMatrix = [f64; 6];

/// Signed positions and zero area are allowed; this is not an EvidenceBoundingBox.
/// `bottom` and `top` name the numeric minimum and maximum Y respectively;
/// viewport coordinates remain Y-down, so these are not visual edge labels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRect {
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
    pub top: f64,
}
impl NativeRect {
    fn validate(self) -> Result<()> {
        ensure!(
            [self.left, self.bottom, self.right, self.top]
                .iter()
                .all(|x| x.is_finite())
                && self.left <= self.right
                && self.bottom <= self.top,
            "invalid native signed rectangle"
        );
        Ok(())
    }
    fn corners(self) -> NativeQuad {
        [
            [self.left, self.bottom],
            [self.right, self.bottom],
            [self.right, self.top],
            [self.left, self.top],
        ]
    }
    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            bottom: self.bottom.min(other.bottom),
            right: self.right.max(other.right),
            top: self.top.max(other.top),
        }
    }
}
fn envelope(q: &NativeQuad) -> NativeRect {
    NativeRect {
        left: q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min),
        right: q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max),
        bottom: q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min),
        top: q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max),
    }
}
const IDENTITY: NativeMatrix = [1., 0., 0., 1., 0., 0.];
fn finite_matrix(m: NativeMatrix) -> Result<NativeMatrix> {
    ensure!(m.iter().all(|n| n.is_finite()), "invalid native matrix");
    // A finite singular object matrix is a valid observed geometric transform.
    Ok(m)
}
fn matrix(m: [f32; 6]) -> Result<NativeMatrix> {
    finite_matrix(m.map(f64::from))
}
fn viewport(t: &AffineTransform) -> NativeMatrix {
    [t.a, t.b, t.c, t.d, t.e, t.f]
}
/// Column vectors: compose(outer,inner)(p) = outer(inner(p)).
fn compose(o: NativeMatrix, i: NativeMatrix) -> Result<NativeMatrix> {
    finite_matrix([
        o[0] * i[0] + o[2] * i[1],
        o[1] * i[0] + o[3] * i[1],
        o[0] * i[2] + o[2] * i[3],
        o[1] * i[2] + o[3] * i[3],
        o[0] * i[4] + o[2] * i[5] + o[4],
        o[1] * i[4] + o[3] * i[5] + o[5],
    ])
}
fn point(m: NativeMatrix, p: NativePoint) -> Result<NativePoint> {
    let p = [
        m[0] * p[0] + m[2] * p[1] + m[4],
        m[1] * p[0] + m[3] * p[1] + m[5],
    ];
    ensure!(
        p.iter().all(|n| n.is_finite()),
        "native transformed point overflow"
    );
    Ok(p)
}
fn quad(m: NativeMatrix, q: NativeQuad) -> Result<NativeQuad> {
    Ok([
        point(m, q[0])?,
        point(m, q[1])?,
        point(m, q[2])?,
        point(m, q[3])?,
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeObjectKind {
    Text,
    Path,
    Image,
    Shading,
    Form,
    Unknown,
}
impl NativeObjectKind {
    fn from_raw(raw: i32) -> Self {
        match raw {
            1 => Self::Text,
            2 => Self::Path,
            3 => Self::Image,
            4 => Self::Shading,
            5 => Self::Form,
            _ => Self::Unknown,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeSegmentKind {
    MoveTo,
    LineTo,
    BezierTo,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePathSegment {
    pub ordinal: u32,
    pub kind: NativeSegmentKind,
    pub raw_point: [f32; 2],
    pub page_point: NativePoint,
    pub closes_subpath: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePaint {
    pub raw_fill_rule: i32,
    pub fill_rule: String,
    pub stroked: bool,
    pub local_stroke_width: FeatureState<f32>,
    pub fill_rgba: FeatureState<[u8; 4]>,
    pub stroke_rgba: FeatureState<[u8; 4]>,
    pub active: FeatureState<bool>,
    pub line_join: FeatureState<i32>,
    pub line_cap: FeatureState<i32>,
    pub dash: FeatureState<Vec<f32>>,
    pub clip: FeatureState<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePathGeometry {
    pub native_segment_count: u32,
    pub segments: Vec<NativePathSegment>,
    pub paint: NativePaint,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeImageGeometry {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub bits_per_pixel: u32,
    pub colorspace: i32,
    pub marked_content_id: i32,
    pub horizontal_dpi: FeatureState<f32>,
    pub vertical_dpi: FeatureState<f32>,
    pub page_quad: NativeQuad,
    pub page_envelope: NativeRect,
    pub placement_rule: String,
    pub pixel_orientation: FeatureState<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObject {
    pub id: String,
    pub ordinal: u32,
    pub ordinal_path: Vec<u32>,
    pub kind: NativeObjectKind,
    pub raw_kind: i32,
    pub raw_matrix: FeatureState<[f32; 6]>,
    pub ancestor_matrix: NativeMatrix,
    pub local_to_page: FeatureState<NativeMatrix>,
    /// Already includes the object's own matrix, but not its ancestor forms.
    pub native_bounds: [f32; 4],
    pub page_bounds_quad: NativeQuad,
    pub page_bounds: NativeRect,
    pub child_count: Option<u32>,
    pub path: Option<NativePathGeometry>,
    pub image: Option<NativeImageGeometry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeGraphicsMetadata {
    pub schema_version: String,
    pub recipe: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub native_profile_sha256: String,
    pub page: u32,
    pub top_level_count: u32,
    pub object_count: u32,
    pub segment_count: u32,
    pub content_bounds: FeatureState<NativeRect>,
    pub content_bounds_quad: FeatureState<NativeQuad>,
    pub content_bounds_semantics: String,
    pub zero_count_provenance: String,
    pub precision: String,
    pub limitations: String,
}
/// Complete immutable artifact DTO; no PDFium handle or machine pointer crosses
/// this boundary. Words stay in the accepted v6 page layout and characters.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePageMetadata {
    pub page: liteparse::types::Page,
    pub objects: Vec<NativeObject>,
    pub capabilities: BTreeMap<String, FeatureState<String>>,
    pub complexity: liteparse::stages::PageComplexityStats,
    pub image_rects: Vec<liteparse::types::Rect>,
    pub geometry: PageGeometry,
    pub native_text_metadata: NativeTextMetadata,
    pub graphics: NativeGraphicsMetadata,
}

pub fn native_graphics_profile_digest(
    s: &PreparedSummary,
    dpi: f32,
    render: bool,
) -> Result<String> {
    crate::artifact::json_sha256(
        &(
            NATIVE_GRAPHICS_RECIPE,
            crate::native_font_word_profile_digest(s, dpi, render)?,
            NATIVE_GRAPHICS_SCHEMA,
            PRECISION,
            CONTENT_BOUNDS_SEMANTICS,
            LIMITATIONS,
            LAYOUT_GRAPHICS_RECIPE,
            crate::rich::native_text_metadata::UNCLIPPED_GEOMETRY,
            "depth_first_source_order; active_ancestor_cycle_check; depth_32; no_native_pointer_persistence; unit_square_image_placement; optional_paint_unsupported",
        ),
        8192,
    )
}
fn object_id(s: &PreparedSummary, profile: &str, page: u32, path: &[u32]) -> Result<String> {
    crate::artifact::json_sha256(
        &(
            NATIVE_GRAPHICS_RECIPE,
            &s.source_sha256,
            &s.prepared_sha256,
            profile,
            page,
            path,
        ),
        8192,
    )
}
fn unsupported<T>() -> FeatureState<T> {
    FeatureState::Unsupported(OPTIONAL.into())
}
fn observed<T>(value: Option<T>) -> FeatureState<T> {
    value
        .map(FeatureState::Present)
        .unwrap_or_else(|| FeatureState::Unsupported("native_getter_failed_or_unavailable".into()))
}
fn raw_rect(raw: [f32; 4]) -> Result<NativeRect> {
    let r = NativeRect {
        left: f64::from(raw[0]),
        bottom: f64::from(raw[1]),
        right: f64::from(raw[2]),
        top: f64::from(raw[3]),
    };
    r.validate()?;
    Ok(r)
}
fn fill_rule(raw: i32) -> Result<&'static str> {
    match raw {
        0 => Ok("none"),
        1 => Ok("alternate"),
        2 => Ok("winding"),
        _ => anyhow::bail!("unqualified native fill rule"),
    }
}
/// Allocation-free source seam. Production closures each perform only the
/// selected kind's native getters; fixtures can independently inject failures.
#[derive(Clone, Copy)]
pub(crate) struct ObjectObservation {
    pub identity: usize,
    pub raw_kind: i32,
    pub matrix: Option<[f32; 6]>,
    pub bounds: Option<[f32; 4]>,
    pub segments: Option<usize>,
    pub draw: Option<(i32, bool)>,
    pub stroke_width: Option<f32>,
    pub fill: Option<[u8; 4]>,
    pub stroke: Option<[u8; 4]>,
    pub image: Option<ImageObservation>,
}
#[derive(Clone, Copy)]
pub(crate) struct ImageObservation {
    pub width: u32,
    pub height: u32,
    pub bits: u32,
    pub colorspace: i32,
    pub marked: i32,
    pub horizontal_dpi: f32,
    pub vertical_dpi: f32,
}
#[derive(Clone, Copy)]
pub(crate) struct SegmentObservation {
    pub kind: Option<NativeSegmentKind>,
    pub point: Option<[f32; 2]>,
    pub close: bool,
}
#[derive(Debug)]
pub(crate) struct GraphicsExtraction {
    pub header: NativeGraphicsMetadata,
    pub objects: Vec<NativeObject>,
    pub image_rects: Vec<liteparse::types::Rect>,
    pub charged_bytes: u64,
    pub paths: usize,
}
/// One continuation frame per ancestor, never a stack of all siblings.
struct Frame<H> {
    handle: Option<H>,
    identity: Option<usize>,
    next: usize,
    count: usize,
    ancestor: NativeMatrix,
    path: Vec<u32>,
}

/// Count admission includes actual DTO sizes, vector capacity, paths, strings,
/// raw+transformed segments, retained classifier rectangles, and serialization.
/// The larger wire allowance is checked again by a counting serializer.
fn object_charge(depth: usize) -> Result<u64> {
    let owned = size_of::<NativeObject>()
        + size_of::<NativeImageGeometry>()
        + size_of::<NativePaint>()
        + size_of::<NativePathGeometry>()
        + size_of::<liteparse::types::Rect>()
        + size_of::<liteparse::types::VectorShape>()
        + 128
        + (depth + 1) * size_of::<u32>();
    u64::try_from(
        owned
            .checked_mul(4)
            .and_then(|n| n.checked_add(4096))
            .context("resource_limit: object sizing overflow")?,
    )
    .map_err(Into::into)
}
fn segment_charge() -> u64 {
    ((size_of::<NativePathSegment>() + 2 * size_of::<liteparse::types::VectorLine>() + 256) * 4
        + 768) as u64
}
fn charge(
    total: &mut u64,
    addition: u64,
    limit: u64,
    admit: &mut impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let next = total
        .checked_add(addition)
        .context("resource_limit: graphics admission overflow")?;
    ensure!(
        next <= limit,
        "resource_limit: graphics preallocation admission"
    );
    admit(addition)?;
    *total = next;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect<H>(
    s: &PreparedSummary,
    profile: &str,
    page: u32,
    geometry: &PageGeometry,
    limit: u64,
    mut top_object: impl FnMut(usize) -> Option<H>,
    mut count: impl FnMut(Option<&H>) -> Option<usize>,
    mut child_object: impl FnMut(&H, usize) -> Option<H>,
    mut observe: impl FnMut(&H) -> ObjectObservation,
    mut segment: impl FnMut(&H, usize) -> Option<SegmentObservation>,
    mut admit: impl FnMut(u64) -> Result<()>,
) -> Result<GraphicsExtraction> {
    geometry.validate_native_pdf()?;
    let top = count(None).context("native top-level object count unavailable")?;
    let top_u32 = u32::try_from(top).context("resource_limit: native object count overflow")?;
    let mut charged = 0;
    let frame_bytes = (size_of::<Frame<H>>() + MAX_DEPTH * size_of::<u32>()) as u64;
    charge(
        &mut charged,
        frame_bytes
            .checked_mul((MAX_DEPTH + 1) as u64)
            .context("resource_limit: frame sizing overflow")?
            + 4096,
        limit,
        &mut admit,
    )?;
    // Reject impossible counts before reserving objects or calling a getter.
    ensure!(
        (top as u64)
            .checked_mul(object_charge(0)?)
            .is_some_and(|n| n <= limit - charged),
        "resource_limit: top-level count admission"
    );
    let mut stack = Vec::new();
    stack.try_reserve_exact(MAX_DEPTH + 1)?;
    stack.push(Frame {
        handle: None,
        identity: None,
        next: 0,
        count: top,
        ancestor: IDENTITY,
        path: Vec::new(),
    });
    let mut objects = Vec::new();
    let mut images = Vec::new();
    let mut union: Option<NativeRect> = None;
    let mut paths = 0;
    let mut segments_total = 0u32;
    while !stack.is_empty() {
        let frame = stack.last_mut().expect("nonempty");
        if frame.next == frame.count {
            stack.pop();
            continue;
        }
        let index = frame.next;
        frame.next += 1;
        let depth = frame.path.len();
        ensure!(depth < MAX_DEPTH, "resource_limit: native Form depth");
        charge(&mut charged, object_charge(depth)?, limit, &mut admit)?;
        let object = match frame.handle.as_ref() {
            None => top_object(index),
            Some(parent) => child_object(parent, index),
        }
        .context("native object handle missing")?;
        let o = observe(&object);
        let kind = NativeObjectKind::from_raw(o.raw_kind);
        let ancestor = frame.ancestor;
        let mut path = Vec::new();
        path.try_reserve_exact(depth + 1)?;
        path.extend_from_slice(&frame.path);
        path.push(u32::try_from(index).context("resource_limit: native ordinal overflow")?);
        ensure!(
            !stack.iter().any(|f| f.identity == Some(o.identity)),
            "native active ancestor cycle"
        );
        let raw = o
            .bounds
            .context("required native object bounds unavailable")?;
        let bounds = raw_rect(raw)?;
        if depth == 0 {
            union = Some(union.map_or(bounds, |u| u.union(bounds)));
        }
        let ancestor_to_page = compose(viewport(&geometry.source_to_page), ancestor)?;
        let bounds_quad = quad(ancestor_to_page, bounds.corners())?;
        let m = o.matrix.map(matrix).transpose()?;
        ensure!(
            m.is_some()
                || !matches!(
                    kind,
                    NativeObjectKind::Path | NativeObjectKind::Image | NativeObjectKind::Form
                ),
            "required native object matrix unavailable"
        );
        let local = m.map(|m| compose(ancestor_to_page, m)).transpose()?;
        let mut path_geometry = None;
        let mut image_geometry = None;
        let mut children = None;
        if kind == NativeObjectKind::Path {
            paths += 1;
            let n = o
                .segments
                .context("required native segment count unavailable")?;
            let addition = (n as u64)
                .checked_mul(segment_charge())
                .context("resource_limit: segment count overflow")?;
            charge(&mut charged, addition, limit, &mut admit)?;
            let n = u32::try_from(n).context("resource_limit: segment count u32 overflow")?;
            segments_total = segments_total
                .checked_add(n)
                .context("resource_limit: complete segment count overflow")?;
            let (fill, stroked) = o.draw.context("required native draw mode unavailable")?;
            let fill_name = fill_rule(fill)?;
            ensure!(
                o.stroke_width.is_none_or(|w| w.is_finite() && w >= 0.),
                "invalid native stroke width"
            );
            let mut values = Vec::new();
            values.try_reserve_exact(n as usize)?;
            for i in 0..n {
                let seg = segment(&object, i as usize).context("native segment handle missing")?;
                let kind = seg.kind.context("native segment kind unavailable")?;
                let p = seg
                    .point
                    .context("required native segment point unavailable")?;
                ensure!(p.iter().all(|v| v.is_finite()), "invalid raw native point");
                values.push(NativePathSegment {
                    ordinal: i,
                    kind,
                    raw_point: p,
                    page_point: point(local.expect("path matrix"), p.map(f64::from))?,
                    closes_subpath: seg.close,
                });
            }
            validate_segments(&values)?;
            path_geometry = Some(NativePathGeometry {
                native_segment_count: n,
                segments: values,
                paint: NativePaint {
                    raw_fill_rule: fill,
                    fill_rule: fill_name.into(),
                    stroked,
                    local_stroke_width: observed(o.stroke_width),
                    fill_rgba: observed(o.fill),
                    stroke_rgba: observed(o.stroke),
                    active: unsupported(),
                    line_join: unsupported(),
                    line_cap: unsupported(),
                    dash: unsupported(),
                    clip: unsupported(),
                },
            });
        } else if kind == NativeObjectKind::Image {
            let img = o
                .image
                .context("required native image metadata unavailable")?;
            ensure!(
                img.width > 0 && img.height > 0,
                "invalid native image dimensions"
            );
            let q = quad(
                local.expect("image matrix"),
                [[0., 0.], [1., 0.], [1., 1.], [0., 1.]],
            )?;
            image_geometry = Some(NativeImageGeometry {
                pixel_width: img.width,
                pixel_height: img.height,
                bits_per_pixel: img.bits,
                colorspace: img.colorspace,
                marked_content_id: img.marked,
                horizontal_dpi: observed(
                    (img.horizontal_dpi.is_finite() && img.horizontal_dpi > 0.)
                        .then_some(img.horizontal_dpi),
                ),
                vertical_dpi: observed(
                    (img.vertical_dpi.is_finite() && img.vertical_dpi > 0.)
                        .then_some(img.vertical_dpi),
                ),
                page_quad: q,
                page_envelope: envelope(&q),
                placement_rule: "native_image_unit_square_through_object_ancestors_viewport".into(),
                pixel_orientation: unsupported(),
            });
            images.try_reserve_exact(1)?;
            let r = envelope(&bounds_quad);
            let values = [
                r.left as f32,
                r.bottom as f32,
                (r.right - r.left) as f32,
                (r.top - r.bottom) as f32,
            ];
            ensure!(
                values.iter().all(|v| v.is_finite()),
                "native classifier image rectangle overflow"
            );
            images.push(liteparse::types::Rect {
                x: values[0],
                y: values[1],
                width: values[2],
                height: values[3],
            });
        } else if kind == NativeObjectKind::Form {
            let n = count(Some(&object)).context("native Form child count unavailable")?;
            ensure!(
                (n as u64)
                    .checked_mul(object_charge(depth + 1)?)
                    .is_some_and(|n| n <= limit - charged),
                "resource_limit: Form count admission"
            );
            children = Some(u32::try_from(n).context("resource_limit: Form child count overflow")?);
        }
        objects.try_reserve_exact(1)?;
        let next_ancestor = m.map(|m| compose(ancestor, m)).transpose()?;
        objects.push(NativeObject {
            id: object_id(s, profile, page, &path)?,
            ordinal: u32::try_from(objects.len())
                .context("resource_limit: object ordinal overflow")?,
            ordinal_path: path.clone(),
            kind,
            raw_kind: o.raw_kind,
            raw_matrix: observed(o.matrix),
            ancestor_matrix: ancestor,
            local_to_page: observed(local),
            native_bounds: raw,
            page_bounds_quad: bounds_quad,
            page_bounds: envelope(&bounds_quad),
            child_count: children,
            path: path_geometry,
            image: image_geometry,
        });
        if let Some(n) = children {
            if n > 0 {
                ensure!(depth + 1 < MAX_DEPTH, "resource_limit: native Form depth");
            }
            stack.push(Frame {
                handle: Some(object),
                identity: Some(o.identity),
                next: 0,
                count: n as usize,
                ancestor: next_ancestor.expect("Form matrix"),
                path,
            });
        }
    }
    let content_quad = union
        .map(|r| quad(viewport(&geometry.source_to_page), r.corners()))
        .transpose()?;
    let header = NativeGraphicsMetadata {
        schema_version: NATIVE_GRAPHICS_SCHEMA.into(),
        recipe: NATIVE_GRAPHICS_RECIPE.into(),
        source_sha256: s.source_sha256.clone(),
        prepared_sha256: s.prepared_sha256.clone(),
        native_profile_sha256: profile.into(),
        page,
        top_level_count: top_u32,
        object_count: u32::try_from(objects.len())
            .context("resource_limit: object count overflow")?,
        segment_count: segments_total,
        content_bounds: content_quad
            .map(|q| FeatureState::Present(envelope(&q)))
            .unwrap_or(FeatureState::Absent),
        content_bounds_quad: content_quad
            .map(FeatureState::Present)
            .unwrap_or(FeatureState::Absent),
        content_bounds_semantics: CONTENT_BOUNDS_SEMANTICS.into(),
        zero_count_provenance:
            "FPDFPage_CountObjects_zero_can_mean_empty_or_error; not_blank_or_visibility_proof"
                .into(),
        precision: PRECISION.into(),
        limitations: LIMITATIONS.into(),
    };
    ensure!(
        crate::resource::encoded_size(&(&header, &objects, &images), limit)? <= charged,
        "resource_limit: graphics wire allowance exceeded"
    );
    Ok(GraphicsExtraction {
        header,
        objects,
        image_rects: images,
        charged_bytes: charged,
        paths,
    })
}
fn validate_segments(segments: &[NativePathSegment]) -> Result<()> {
    let mut open = false;
    let mut cubic = 0;
    for (i, s) in segments.iter().enumerate() {
        ensure!(s.ordinal as usize == i, "native segment ordinal mismatch");
        match s.kind {
            NativeSegmentKind::MoveTo => {
                ensure!(cubic == 0, "incomplete cubic path");
                open = true;
            }
            NativeSegmentKind::LineTo => ensure!(
                open && cubic == 0,
                "native line without subpath or incomplete cubic"
            ),
            NativeSegmentKind::BezierTo => {
                ensure!(open, "native curve without subpath");
                cubic = (cubic + 1) % 3;
            }
        }
        if s.closes_subpath {
            ensure!(open && cubic == 0, "invalid native close/cubic grouping");
            open = false;
        }
    }
    ensure!(cubic == 0, "incomplete native cubic path");
    Ok(())
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(serde_json::to_vec(a)? == serde_json::to_vec(b)?)
}
fn optional<T>(v: &FeatureState<T>) -> bool {
    matches!(v,FeatureState::Unsupported(s) if s==OPTIONAL)
}
fn observed_optional<T>(v: &FeatureState<T>) -> bool {
    matches!(v, FeatureState::Present(_))
        || matches!(v,FeatureState::Unsupported(s) if s=="native_getter_failed_or_unavailable")
}

/// Allocation-free structural prepass for page metadata admission. JSON parser
/// scratch and the already bounded input are admitted separately by the reader.
/// This counts containers/elements/keys and decoded string bytes, never creates
/// a Value tree or a typed record while deriving the upper bound.
#[derive(Default)]
struct AllocationCounts {
    nodes: u64,
    arrays: u64,
    elements: u64,
    numeric_arrays: u64,
    numeric_elements: u64,
    entries: u64,
    strings: u64,
}
impl AllocationCounts {
    fn add(target: &mut u64, amount: u64) -> Result<()> {
        *target = target
            .checked_add(amount)
            .context("resource_limit: native metadata shape overflow")?;
        Ok(())
    }
}
struct CountValue<'a>(&'a mut AllocationCounts);
impl<'de> serde::de::DeserializeSeed<'de> for CountValue<'_> {
    // Only an immediate numeric scalar returns true. A nested sequence must
    // remain a structured element in its parent's allocation accounting.
    type Value = bool;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<bool, D::Error> {
        AllocationCounts::add(&mut self.0.nodes, 1).map_err(serde::de::Error::custom)?;
        d.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for CountValue<'_> {
    type Value = bool;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded native metadata JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<bool, E> {
        Ok(false)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<bool, E> {
        Ok(true)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<bool, E> {
        Ok(true)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<bool, E> {
        Ok(true)
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<bool, E> {
        Ok(false)
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<bool, E> {
        AllocationCounts::add(&mut self.0.strings, s.len() as u64).map_err(E::custom)?;
        Ok(false)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<bool, A::Error> {
        let mut elements = 0;
        let mut numeric = true;
        while let Some(is_numeric) = seq.next_element_seed(CountValue(&mut *self.0))? {
            AllocationCounts::add(&mut elements, 1).map_err(serde::de::Error::custom)?;
            numeric &= is_numeric;
        }
        // Pinned serde_json supplies no size hint, so empty Vecs allocate no
        // element storage. Their inline headers remain covered by their DTO.
        if elements != 0 {
            let (arrays, total) = if numeric {
                (&mut self.0.numeric_arrays, &mut self.0.numeric_elements)
            } else {
                (&mut self.0.arrays, &mut self.0.elements)
            };
            AllocationCounts::add(arrays, 1).map_err(serde::de::Error::custom)?;
            AllocationCounts::add(total, elements).map_err(serde::de::Error::custom)?;
        }
        Ok(false)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<bool, A::Error> {
        while map.next_key_seed(CountValue(&mut *self.0))?.is_some() {
            AllocationCounts::add(&mut self.0.entries, 1).map_err(serde::de::Error::custom)?;
            map.next_value_seed(CountValue(&mut *self.0))?;
        }
        Ok(false)
    }
}
/// Concrete typed/capacity/overlap admission, intentionally conservative:
/// * serde Vec capacity <=2*elements +8 minimum slots per nonempty array;
///   numeric-only arrays use an eight-byte scalar stride, all others the
///   largest admitted DTO including inline members. The pinned v6/v7 closure
///   has only u8/u32/f32 numeric Vecs and inline numeric arrays, with no custom
///   numeric-to-DTO deserializers. Nested sequences always remain structured;
/// * three typed copies cover retained input, font/word reconstruction and the
///   derived layout/validation overlap;
/// * two canonical JSON trees charge each Value, full array backing capacities
///   and generously padded BTree entries independently; eight string/input
///   copies cover escaped output, parser scratch, typed ownership and canonical
///   serialization;
/// * fixed stack/Arc/envelope overhead is reserved too.
///
/// This is an owned Rust allocation bound, never a native/RSS quota.
pub fn native_metadata_working_upper(bytes: &[u8]) -> Result<u64> {
    use serde::de::DeserializeSeed;
    let mut counts = AllocationCounts::default();
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    CountValue(&mut counts).deserialize(&mut decoder)?;
    decoder.end()?;
    #[derive(Default, Deserialize)]
    struct UnsupportedPage {
        #[serde(default)]
        graphics: Vec<serde::de::IgnoredAny>,
        #[serde(default)]
        struct_nodes: Vec<serde::de::IgnoredAny>,
        #[serde(default)]
        image_refs: Vec<serde::de::IgnoredAny>,
        #[serde(default)]
        annotations: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        form_fields: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        structure_tree: Option<serde::de::IgnoredAny>,
    }
    #[derive(Deserialize)]
    struct Envelope {
        page: UnsupportedPage,
    }
    let page = serde_json::from_slice::<Envelope>(bytes)?.page;
    ensure!(
        page.graphics.is_empty()
            && page.struct_nodes.is_empty()
            && page.image_refs.is_empty()
            && page.annotations.is_none()
            && page.form_fields.is_none()
            && page.structure_tree.is_none(),
        "native metadata contains unsupported unadmitted page features"
    );
    let largest = [
        size_of::<NativeObject>(),
        size_of::<NativePageMetadata>(),
        size_of::<NativePathSegment>(),
        size_of::<NativeImageGeometry>(),
        size_of::<crate::NativeCharacter>(),
        size_of::<crate::NativeFontInfo>(),
        size_of::<liteparse::types::TextItem>(),
        size_of::<liteparse::types::WordBox>(),
        size_of::<liteparse::types::VectorLine>(),
        size_of::<liteparse::types::VectorShape>(),
    ]
    .into_iter()
    .max()
    .expect("typed sizes") as u64;
    let overflow = || anyhow::anyhow!("resource_limit: native metadata allocation upper overflow");
    let slots = counts
        .elements
        .checked_mul(2)
        .and_then(|n| counts.arrays.checked_mul(8).and_then(|a| n.checked_add(a)))
        .ok_or_else(overflow)?;
    let numeric_slots = counts
        .numeric_elements
        .checked_mul(2)
        .and_then(|n| {
            counts
                .numeric_arrays
                .checked_mul(8)
                .and_then(|a| n.checked_add(a))
        })
        .ok_or_else(overflow)?;
    let numeric = numeric_slots
        .checked_mul(size_of::<u64>() as u64)
        .ok_or_else(overflow)?;
    let typed = slots
        .checked_mul(largest)
        .and_then(|n| n.checked_add(numeric))
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(overflow)?;
    // Both canonical trees coexist. Their Vec capacity can exceed the number
    // of initialized Value nodes, including in numeric-only arrays. Charge
    // full backing capacities independently of the tighter typed scalar term.
    let json_arrays = slots
        .checked_add(numeric_slots)
        .and_then(|n| n.checked_mul((size_of::<serde_json::Value>() * 2) as u64))
        .ok_or_else(overflow)?;
    let json = counts
        .nodes
        .checked_mul((size_of::<serde_json::Value>() * 2) as u64)
        .and_then(|n| n.checked_add(json_arrays))
        .and_then(|n| {
            counts
                .entries
                .checked_mul(
                    (8 * (size_of::<String>()
                        + size_of::<serde_json::Value>()
                        + 3 * size_of::<usize>())) as u64,
                )
                .and_then(|e| n.checked_add(e))
        })
        .ok_or_else(overflow)?;
    let strings = counts.strings.checked_mul(8).ok_or_else(overflow)?;
    (bytes.len() as u64)
        .checked_mul(8)
        .and_then(|n| n.checked_add(typed))
        .and_then(|n| n.checked_add(json))
        .and_then(|n| n.checked_add(strings))
        .and_then(|n| n.checked_add(65_536 + largest * 4))
        .ok_or_else(overflow)
}
/// Shared publication and selected-read validator. Retained raw observations
/// are authoritative only after their source/artifact/profile owner verifies them.
pub fn validate_native_graphics_metadata(
    bytes: &[u8],
    s: &PreparedSummary,
    profile: &str,
    page: u32,
    budget: &ResourceBudget,
) -> Result<NativePageMetadata> {
    budget.validate()?;
    ensure!(
        bytes.len() as u64 <= budget.page_layout_bytes
            && (bytes.len() as u64)
                .checked_mul(64)
                .is_some_and(|n| n <= budget.owned_working_bytes),
        "resource_limit: native graphics validation admission"
    );
    ensure!(
        native_metadata_working_upper(bytes)? <= budget.owned_working_bytes,
        "resource_limit: native graphics typed validation upper"
    );
    crate::rich::native_text_metadata::validate_native_text_metadata_unclipped(
        bytes, s, page, budget,
    )?;
    let m: NativePageMetadata =
        serde_json::from_slice(bytes).context("native graphics typed artifact invalid")?;
    // Upstream layout and the shared FeatureState enum predate strict unknown
    // fields. Exact structural roundtrip closes those nested shapes too, without
    // changing the accepted v6 reader's deserialization contract.
    let canonical = serde_json::to_vec(&m)?;
    ensure!(
        serde_json::from_slice::<serde_json::Value>(bytes)?
            == serde_json::from_slice::<serde_json::Value>(&canonical)?,
        "native graphics contains unknown or noncanonical nested fields"
    );
    let h = &m.graphics;
    ensure!(
        h.schema_version == NATIVE_GRAPHICS_SCHEMA
            && h.recipe == NATIVE_GRAPHICS_RECIPE
            && h.source_sha256 == s.source_sha256
            && h.prepared_sha256 == s.prepared_sha256
            && h.native_profile_sha256 == profile
            && h.page == page
            && m.page.page_number == page as usize
            && h.object_count as usize == m.objects.len()
            && h.content_bounds_semantics == CONTENT_BOUNDS_SEMANTICS
            && h.precision == PRECISION
            && h.limitations == LIMITATIONS
            && h.zero_count_provenance
                == "FPDFPage_CountObjects_zero_can_mean_empty_or_error; not_blank_or_visibility_proof",
        "native graphics identity/count/provenance mismatch"
    );
    ensure!(
        matches!(m.capabilities.get("native_recipe"),Some(FeatureState::Present(r)) if r==NATIVE_GRAPHICS_RECIPE),
        "native graphics recipe capability mismatch"
    );
    m.geometry.validate_native_pdf()?;
    ensure!(
        m.geometry.width_points == f64::from(m.page.page_width)
            && m.geometry.height_points == f64::from(m.page.page_height),
        "native graphics viewport/page mismatch"
    );
    let mut frames = vec![(Vec::<u32>::new(), 0u32, h.top_level_count, IDENTITY)];
    let mut union: Option<NativeRect> = None;
    let mut segments = 0u32;
    let mut image_index = 0;
    for (i, o) in m.objects.iter().enumerate() {
        while frames.last().is_some_and(|f| f.1 == f.2) {
            frames.pop();
        }
        let f = frames.last_mut().context("native graphics extra object")?;
        let mut path = f.0.clone();
        path.push(f.1);
        f.1 += 1;
        let ancestor = f.3;
        ensure!(
            path.len() <= MAX_DEPTH
                && o.ordinal as usize == i
                && o.ordinal_path == path
                && o.kind == NativeObjectKind::from_raw(o.raw_kind)
                && o.id == object_id(s, profile, page, &path)?
                && o.ancestor_matrix == ancestor,
            "native graphics object identity/order/ancestor mismatch"
        );
        let bounds = raw_rect(o.native_bounds)?;
        if path.len() == 1 {
            union = Some(union.map_or(bounds, |u| u.union(bounds)));
        }
        let a = compose(viewport(&m.geometry.source_to_page), ancestor)?;
        let q = quad(a, bounds.corners())?;
        ensure!(
            q == o.page_bounds_quad && envelope(&q) == o.page_bounds,
            "native object bounds recomposition mismatch"
        );
        let raw_m = match &o.raw_matrix {
            FeatureState::Present(v) => Some(matrix(*v)?),
            v => {
                ensure!(observed_optional(v), "native matrix state invalid");
                None
            }
        };
        ensure!(
            raw_m.is_some()
                || !matches!(
                    o.kind,
                    NativeObjectKind::Form | NativeObjectKind::Path | NativeObjectKind::Image
                ),
            "required native matrix missing"
        );
        let local = raw_m.map(|r| compose(a, r)).transpose()?;
        ensure!(
            same(&observed(local), &o.local_to_page)?,
            "native local matrix recomposition mismatch"
        );
        ensure!(
            o.path.is_some() == (o.kind == NativeObjectKind::Path)
                && o.image.is_some() == (o.kind == NativeObjectKind::Image)
                && o.child_count.is_some() == (o.kind == NativeObjectKind::Form),
            "native object kind payload mismatch"
        );
        if let Some(p) = &o.path {
            ensure!(
                p.native_segment_count as usize == p.segments.len(),
                "native segment count mismatch"
            );
            segments = segments
                .checked_add(p.native_segment_count)
                .context("resource_limit: native segment count overflow")?;
            validate_segments(&p.segments)?;
            for v in &p.segments {
                ensure!(
                    v.raw_point.iter().all(|n| n.is_finite())
                        && point(local.expect("path matrix"), v.raw_point.map(f64::from))?
                            == v.page_point,
                    "native segment recomposition mismatch"
                );
            }
            let p = &p.paint;
            ensure!(
                p.fill_rule == fill_rule(p.raw_fill_rule)?
                    && optional(&p.active)
                    && optional(&p.line_join)
                    && optional(&p.line_cap)
                    && optional(&p.dash)
                    && optional(&p.clip)
                    && observed_optional(&p.fill_rgba)
                    && observed_optional(&p.stroke_rgba)
                    && observed_optional(&p.local_stroke_width),
                "native paint state mismatch"
            );
            if let FeatureState::Present(w) = p.local_stroke_width {
                ensure!(w.is_finite() && w >= 0., "invalid native stroke width");
            }
        }
        if let Some(img) = &o.image {
            let q = quad(
                local.expect("image matrix"),
                [[0., 0.], [1., 0.], [1., 1.], [0., 1.]],
            )?;
            ensure!(
                img.pixel_width > 0
                    && img.pixel_height > 0
                    && q == img.page_quad
                    && envelope(&q) == img.page_envelope
                    && img.placement_rule
                        == "native_image_unit_square_through_object_ancestors_viewport"
                    && optional(&img.pixel_orientation),
                "native image geometry mismatch"
            );
            for dpi in [&img.horizontal_dpi, &img.vertical_dpi] {
                ensure!(observed_optional(dpi), "native image DPI state invalid");
                if let FeatureState::Present(d) = dpi {
                    ensure!(d.is_finite() && *d > 0., "native image DPI invalid");
                }
            }
            let r = o.page_bounds;
            let rect = m
                .image_rects
                .get(image_index)
                .context("native image classifier rectangle missing")?;
            ensure!(
                [rect.x, rect.y, rect.width, rect.height]
                    == [
                        r.left as f32,
                        r.bottom as f32,
                        (r.right - r.left) as f32,
                        (r.top - r.bottom) as f32
                    ],
                "native image classifier geometry mismatch"
            );
            image_index += 1;
        }
        if let Some(n) = o.child_count {
            frames.push((path, 0, n, compose(ancestor, raw_m.expect("Form matrix"))?));
        }
    }
    ensure!(
        frames.iter().all(|f| f.1 == f.2)
            && segments == h.segment_count
            && image_index == m.image_rects.len(),
        "native graphics incomplete traversal/count mismatch"
    );
    let q = union
        .map(|r| quad(viewport(&m.geometry.source_to_page), r.corners()))
        .transpose()?;
    ensure!(
        same(
            &h.content_bounds_quad,
            &q.map(FeatureState::Present).unwrap_or(FeatureState::Absent)
        )? && same(
            &h.content_bounds,
            &q.map(|v| FeatureState::Present(envelope(&v)))
                .unwrap_or(FeatureState::Absent)
        )?,
        "native content union mismatch"
    );
    let (bounds, vectors) = derive_layout(h, &m.objects)?;
    ensure!(
        same(&m.page.content_bounds, &bounds)?
            && same(&m.page.vector_graphics, &Some(vectors))?
            && m.page.graphics.is_empty(),
        "native layout graphics derivative mismatch"
    );
    ensure!(
        matches!(m.capabilities.get("native_layout_graphics"),Some(FeatureState::Present(r)) if r==LAYOUT_GRAPHICS_RECIPE)
            && matches!(m.capabilities.get("native_content_bounds_semantics"),Some(FeatureState::Present(r)) if r==CONTENT_BOUNDS_SEMANTICS),
        "native layout/extent capability mismatch"
    );
    Ok(m)
}

/// A deliberately qualified layout derivative. It preserves each path shape
/// and exact H/V straight edges but does not approximate curves or merge lines.
pub(crate) fn derive_layout(
    header: &NativeGraphicsMetadata,
    objects: &[NativeObject],
) -> Result<(
    Option<liteparse::types::Rect>,
    liteparse::types::VectorGraphics,
)> {
    use liteparse::types::{Rect, VectorGraphics, VectorLine, VectorShape};
    fn rect(r: NativeRect) -> Result<Rect> {
        let values = [
            r.left as f32,
            r.bottom as f32,
            (r.right - r.left) as f32,
            (r.top - r.bottom) as f32,
        ];
        ensure!(
            values.iter().all(|v| v.is_finite()),
            "native layout rectangle overflow"
        );
        Ok(Rect {
            x: values[0],
            y: values[1],
            width: values[2],
            height: values[3],
        })
    }
    fn color(v: &FeatureState<[u8; 4]>) -> Option<String> {
        match v {
            FeatureState::Present([r, g, b, a]) => Some(format!("{a:02x}{r:02x}{g:02x}{b:02x}")),
            _ => None,
        }
    }
    let bounds = match header.content_bounds {
        FeatureState::Present(r) => Some(rect(r)?),
        FeatureState::Absent => None,
        _ => anyhow::bail!("invalid native extent state"),
    };
    let mut result = VectorGraphics::default();
    result
        .shapes
        .try_reserve_exact(objects.iter().filter(|o| o.path.is_some()).count())?;
    // At most one edge per native segment plus one explicit close per segment.
    let line_capacity = (header.segment_count as usize)
        .checked_mul(2)
        .context("resource_limit: layout line capacity overflow")?;
    result.lines.try_reserve_exact(line_capacity)?;
    for object in objects {
        let Some(path) = &object.path else { continue };
        let paint = &path.paint;
        result.shapes.push(VectorShape {
            bbox: rect(object.page_bounds)?,
            stroke: paint.stroked,
            stroke_color: color(&paint.stroke_rgba),
            fill: paint.raw_fill_rule != 0,
            fill_color: color(&paint.fill_rgba),
            has_curve: path
                .segments
                .iter()
                .any(|s| s.kind == NativeSegmentKind::BezierTo),
        });
        let mut previous = None;
        let mut first = None;
        let mut cubic = 0;
        for segment in &path.segments {
            let p = segment.page_point;
            let mut edge = |a: NativePoint, b: NativePoint| -> Result<()> {
                if a != b && (a[0] == b[0] || a[1] == b[1]) {
                    let v = [a[0] as f32, a[1] as f32, b[0] as f32, b[1] as f32];
                    ensure!(
                        v.iter().all(|x| x.is_finite()),
                        "native layout line overflow"
                    );
                    result.lines.push(VectorLine {
                        x1: v[0],
                        y1: v[1],
                        x2: v[2],
                        y2: v[3],
                        stroke: paint.stroked,
                        stroke_width: None,
                        stroke_color: color(&paint.stroke_rgba),
                        fill: paint.raw_fill_rule != 0,
                        fill_color: color(&paint.fill_rgba),
                    });
                }
                Ok(())
            };
            match segment.kind {
                NativeSegmentKind::MoveTo => {
                    first = Some(p);
                    previous = Some(p);
                    cubic = 0;
                }
                NativeSegmentKind::LineTo => {
                    if let Some(a) = previous {
                        edge(a, p)?;
                    }
                    previous = Some(p);
                }
                NativeSegmentKind::BezierTo => {
                    cubic = (cubic + 1) % 3;
                    if cubic == 0 {
                        previous = Some(p);
                    }
                }
            }
            if segment.closes_subpath {
                if let (Some(a), Some(b)) = (previous, first) {
                    edge(a, b)?;
                }
                previous = None;
                first = None;
            }
        }
    }
    Ok((bounds, result))
}

#[cfg(test)]
#[path = "native_graphics_tests.rs"]
mod tests;
