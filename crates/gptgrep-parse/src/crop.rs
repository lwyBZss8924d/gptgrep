//! In-memory derivative of a caller-validated rendition. This module grants no
//! generation/source identity, cache custody, publication or image-delivery rights.
//! PNG-only qualification: image 0.25.10 / png 0.18.1 / fdeflate 0.3.7.
//! JPEG crop is unavailable: the pinned image JPEG decoder tolerates entropy errors.

use std::{
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
};

use anyhow::{Context, Result, bail, ensure};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AffineTransform, BoundingBox, OcrImage, ResourceBudget, validate_rendition_metadata};

const FIXED_WORKING: u64 = 1_048_576;
const ENDPOINT_EPSILON: f64 = 1e-7;
const JPEG_UNAVAILABLE: &str =
    "capability_unavailable: JPEG crop decoder does not guarantee complete entropy validation";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PixelCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CropRecipe {
    pub schema_version: String,
    pub parent_artifact_id: String,
    pub parent_sha256: String,
    pub parent_profile_sha256: String,
    pub parent_page: u32,
    pub parent_width: u32,
    pub parent_height: u32,
    pub page_width_points: f64,
    pub page_height_points: f64,
    pub selected_page_bbox: BoundingBox,
    pub pixels: PixelCrop,
    pub page_to_raster: AffineTransform,
    pub raster_to_page: AffineTransform,
    pub raster_to_crop: AffineTransform,
    pub crop_to_raster: AffineTransform,
    pub page_to_crop: AffineTransform,
    pub crop_to_page: AffineTransform,
    pub decoder: String,
    pub encoder: String,
    pub pixel_rule: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CropAdmission {
    pub encoded_source_bytes: u64,
    pub source_decoded_upper_bytes: u64,
    pub decoder_aux_upper_bytes: u64,
    pub crop_rgba_bytes: u64,
    pub encoder_aux_upper_bytes: u64,
    pub encoded_output_upper_bytes: u64,
    pub owned_working_upper_bytes: u64,
}

#[derive(Debug)]
pub struct CropOutput {
    pub png: Vec<u8>,
    pub sha256: String,
    pub recipe: CropRecipe,
    pub recipe_sha256: String,
    pub admission: CropAdmission,
}

/// Select a tighter PNG decoder allocation limit from the complete original
/// raster geometry. crop.rs separately charges that entire selected limit plus
/// unfilter/scratch rows. Charging the frozen 32MiB maximum for a tiny raster
/// would exceed the toolbox's 32MiB operation allowance before any crop exists.
/// Limits only decrease; ancillary PNG decoding that cannot fit fails closed.
pub(crate) fn decoder_budget(
    parent: &crate::OcrImage,
    admitted: &ResourceBudget,
) -> Result<ResourceBudget> {
    let width = u64::from(parent.width);
    let height = u64::from(parent.height);
    let rgba = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("native_crop_decoder_bound_overflow"))?;
    let unfilter = width
        .checked_mul(4)
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(height))
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(16_384))
        .ok_or_else(|| anyhow::anyhow!("native_crop_decoder_bound_overflow"))?;
    let scratch = width
        .checked_mul(32)
        .ok_or_else(|| anyhow::anyhow!("native_crop_decoder_bound_overflow"))?;
    let chunks = parent
        .artifact
        .bytes
        .checked_mul(4)
        .and_then(|n| n.checked_add(65_536))
        .ok_or_else(|| anyhow::anyhow!("native_crop_decoder_bound_overflow"))?;
    let selected = rgba
        .max(unfilter)
        .max(scratch)
        .max(chunks)
        .min(admitted.image_pixel_buffer_bytes);
    ensure!(
        rgba <= selected && unfilter <= selected && scratch <= selected,
        "native_crop_decoder_pixel_limit"
    );
    let mut budget = admitted.clone();
    budget.image_pixel_buffer_bytes = selected;
    budget.ensure_same_invocation_within(admitted)?;
    Ok(budget)
}

/// `image` and `page_size` must come from the caller's pinned, source-validated
/// manifest. `source` is an owned regular-file handle opened under that caller's
/// path-custody policy. Its bytes are independently reverified here. No path is
/// reopened and no caller-supplied crop lineage or transform is accepted.
///
/// The file handle's cursor is reset; callers must transfer exclusive cursor
/// ownership. Only axis-aligned scaling/flips and quarter turns are qualified.
/// Samples retain raster order and alpha; no resize, EXIF rotation or color
/// management is performed. The owned working envelope is not a native RSS cap.
pub fn crop_rendition(
    image: &OcrImage,
    mut source: File,
    selected: BoundingBox,
    page_size: (f64, f64),
    budget: &ResourceBudget,
) -> Result<CropOutput> {
    ensure!(
        budget.owned_working_bytes >= FIXED_WORKING,
        "resource_limit: crop fixed working admission"
    );
    validate_rendition_metadata(image, budget)?;
    match image.artifact.mime.as_str() {
        "image/png" => {}
        "image/jpeg" => bail!(JPEG_UNAVAILABLE),
        _ => bail!("capability_unavailable: crop MIME"),
    }
    let selected_budget = decoder_budget(image, budget)?;
    let budget = &selected_budget;
    ensure!(image.artifact.id.len() <= 256, "crop_parent_id_limit");
    ensure!(
        page_size.0.is_finite()
            && page_size.1.is_finite()
            && page_size.0 > 0.0
            && page_size.1 > 0.0,
        "crop_invalid_page_size"
    );
    selected.validate(page_size.0, page_size.1)?;
    ensure!(
        selected.x + selected.width <= page_size.0 && selected.y + selected.height <= page_size.1,
        "crop_outside_page"
    );
    let pixels = pixel_crop(image, selected)?;
    let source_len = image.artifact.bytes;
    ensure!(
        sum(&[source_len, FIXED_WORKING])? <= budget.owned_working_bytes,
        "resource_limit: crop source read admission"
    );
    let metadata = source.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == source_len,
        "crop_source_file_changed"
    );
    let mut encoded = allocate(source_len)?;
    source.seek(SeekFrom::Start(0))?;
    source.read_exact(&mut encoded)?;
    ensure!(
        source.read(&mut [0u8; 1])? == 0 && sha(&encoded) == image.artifact.sha256,
        "crop_source_bytes_changed"
    );
    let dimensions = png_header(&encoded)?;
    ensure!(
        dimensions == (image.width, image.height),
        "crop_source_dimensions_changed"
    );
    let admission = admit(image, pixels, budget)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(image.width);
    limits.max_image_height = Some(image.height);
    limits.max_alloc = Some(budget.image_pixel_buffer_bytes);
    let mut reader = ImageReader::with_format(Cursor::new(&encoded), ImageFormat::Png);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    ensure!(
        decoder.dimensions() == dimensions,
        "crop_decoder_geometry_changed"
    );
    let color = decoder.color_type();
    ensure!(
        matches!(
            color,
            ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8
        ),
        "capability_unavailable: crop requires 8-bit decoder samples"
    );
    let expected = mul(&[
        u64::from(image.width),
        u64::from(image.height),
        u64::from(color.bytes_per_pixel()),
    ])?;
    ensure!(
        decoder.total_bytes() == expected && expected <= budget.image_pixel_buffer_bytes,
        "resource_limit: crop decoded buffer admission"
    );
    let mut decoded = allocate(expected)?;
    decoder.read_image(&mut decoded)?;
    // Decoder and encoded input are retained in the conservative pre-admission
    // envelope even though their lifetimes end before the encoder starts.
    let mut rgba = allocate(admission.crop_rgba_bytes)?;
    copy_pixels(&decoded, &mut rgba, image.width, pixels, color)?;
    let mut writer = CropWriter {
        bytes: Vec::new(),
        limit: admission.encoded_output_upper_bytes,
    };
    writer
        .bytes
        .try_reserve_exact(usize::try_from(writer.limit)?)
        .context("resource_limit: crop output allocation failed")?;
    PngEncoder::new_with_quality(
        &mut writer,
        CompressionType::Uncompressed,
        FilterType::NoFilter,
    )
    .write_image(
        &rgba,
        pixels.width,
        pixels.height,
        image::ExtendedColorType::Rgba8,
    )
    .context("capability_unavailable: qualified crop PNG encoder failed")?;
    // image's PNG Writer emits IEND on Drop. A Drop write error is not surfaced
    // by write_image, so require a complete container before returning success.
    ensure!(
        png_header(&writer.bytes)? == (pixels.width, pixels.height),
        "crop_output_geometry_changed"
    );
    ensure!(
        writer.bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82"),
        "crop_output_incomplete"
    );
    budget.admit_image(pixels.width, pixels.height, writer.bytes.len() as u64)?;
    let recipe = recipe_for(image, selected, page_size)?;
    let recipe_sha256 = sha(&serde_json::to_vec(&recipe)?);
    let sha256 = sha(&writer.bytes);
    Ok(CropOutput {
        png: writer.bytes,
        sha256,
        recipe,
        recipe_sha256,
        admission,
    })
}

/// Shared pure geometry for encoding and portable custody verification.
pub(crate) fn recipe_for(
    image: &OcrImage,
    selected: BoundingBox,
    page_size: (f64, f64),
) -> Result<CropRecipe> {
    ensure!(
        page_size.0.is_finite()
            && page_size.1.is_finite()
            && page_size.0 > 0.0
            && page_size.1 > 0.0,
        "crop_invalid_page_size"
    );
    selected.validate(page_size.0, page_size.1)?;
    ensure!(
        selected.x + selected.width <= page_size.0 && selected.y + selected.height <= page_size.1,
        "crop_outside_page"
    );
    let pixels = pixel_crop(image, selected)?;
    let raster_to_crop = AffineTransform {
        e: -f64::from(pixels.x),
        f: -f64::from(pixels.y),
        ..AffineTransform::IDENTITY
    };
    let page_to_crop = compose(raster_to_crop, image.page_to_raster)?;
    Ok(CropRecipe {
        schema_version: "gptgrep.crop.recipe.v1".into(),
        parent_artifact_id: image.artifact.id.clone(),
        parent_sha256: image.artifact.sha256.clone(),
        parent_profile_sha256: image.artifact.profile_sha256.clone(),
        parent_page: image.artifact.page.context("crop parent page missing")?,
        parent_width: image.width, parent_height: image.height,
        page_width_points: page_size.0, page_height_points: page_size.1,
        selected_page_bbox: selected, pixels,
        page_to_raster: image.page_to_raster,
        raster_to_page: image.page_to_raster.inverse()?,
        raster_to_crop, crop_to_raster: raster_to_crop.inverse()?,
        page_to_crop, crop_to_page: page_to_crop.inverse()?,
        decoder: "image-0.25.10/png-0.18.1/8bit-restricted-v2".into(),
        encoder: "image-0.25.10/png-0.18.1/fdeflate-0.3.7/rgba8-uncompressed-no-filter-v1".into(),
        pixel_rule: "outward floor/ceil; endpoint roundoff clamp at 1e-7 raster pixels; no resize, reorientation or color management".into(),
    })
}

fn pixel_crop(image: &OcrImage, bbox: BoundingBox) -> Result<PixelCrop> {
    let t = image.page_to_raster;
    t.validate()?;
    ensure!(
        (t.b == 0.0 && t.c == 0.0) || (t.a == 0.0 && t.d == 0.0),
        "capability_unavailable: crop transform requires axis-aligned quarter-turn geometry"
    );
    let corners = [
        (bbox.x, bbox.y),
        (bbox.x + bbox.width, bbox.y),
        (bbox.x, bbox.y + bbox.height),
        (bbox.x + bbox.width, bbox.y + bbox.height),
    ]
    .map(|(x, y)| t.apply(x, y));
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in corners {
        ensure!(
            x.is_finite()
                && y.is_finite()
                && x >= -ENDPOINT_EPSILON
                && y >= -ENDPOINT_EPSILON
                && x <= f64::from(image.width) + ENDPOINT_EPSILON
                && y <= f64::from(image.height) + ENDPOINT_EPSILON,
            "crop_outside_parent_pixels"
        );
        bounds[0] = bounds[0].min(x.max(0.0));
        bounds[1] = bounds[1].min(y.max(0.0));
        bounds[2] = bounds[2].max(x.min(f64::from(image.width)));
        bounds[3] = bounds[3].max(y.min(f64::from(image.height)));
    }
    ensure!(
        bounds[2] > bounds[0] && bounds[3] > bounds[1],
        "crop_empty_pixels"
    );
    let x = bounds[0].floor() as u32;
    let y = bounds[1].floor() as u32;
    let right = bounds[2].ceil() as u32;
    let bottom = bounds[3].ceil() as u32;
    let width = right.checked_sub(x).context("crop width underflow")?;
    let height = bottom.checked_sub(y).context("crop height underflow")?;
    ensure!(
        width > 0 && height > 0 && right <= image.width && bottom <= image.height,
        "crop_empty_or_overflow"
    );
    Ok(PixelCrop {
        x,
        y,
        width,
        height,
    })
}

fn compose(left: AffineTransform, right: AffineTransform) -> Result<AffineTransform> {
    let result = AffineTransform {
        a: left.a * right.a + left.c * right.b,
        b: left.b * right.a + left.d * right.b,
        c: left.a * right.c + left.c * right.d,
        d: left.b * right.c + left.d * right.d,
        e: left.a * right.e + left.c * right.f + left.e,
        f: left.b * right.e + left.d * right.f + left.f,
    };
    result.validate()?;
    Ok(result)
}

fn admit(image: &OcrImage, crop: PixelCrop, budget: &ResourceBudget) -> Result<CropAdmission> {
    let source_decoded = mul(&[u64::from(image.width), u64::from(image.height), 4])?;
    let crop_rgba = mul(&[u64::from(crop.width), u64::from(crop.height), 4])?;
    ensure!(
        crop_rgba <= budget.image_pixel_buffer_bytes,
        "resource_limit: crop pixel buffer admission"
    );
    let filtered = mul(&[
        sum(&[mul(&[u64::from(crop.width), 4])?, 1])?,
        u64::from(crop.height),
    ])?;
    let blocks = filtered
        .checked_add(65_534)
        .context("crop encoder size overflow")?
        / 65_535;
    // Stored DEFLATE: raw filter bytes + five bytes per block + six zlib bytes.
    // 4KiB covers the pinned encoder's critical chunks and fixed metadata.
    let output_upper = sum(&[filtered, mul(&[blocks, 5])?, 6, 4096])?;
    let base64_upper = output_upper
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .context("crop base64 overflow")?;
    ensure!(
        output_upper <= budget.image_file_bytes && base64_upper <= budget.image_base64_bytes,
        "resource_limit: crop PNG output admission"
    );
    let decoder_aux = {
        // png's UnfilteringBuffer is not charged to png::Limits. Bound its
        // geometric Vec growth and scratch rows separately before decoder
        // construction; a nominally <=8MP parent can be unqualified here.
        let unfilter = sum(&[
            mul(&[
                sum(&[mul(&[u64::from(image.width), 4])?, 1])?,
                u64::from(image.height),
                2,
            ])?,
            16_384,
        ])?;
        let scratch = mul(&[u64::from(image.width), 32])?;
        ensure!(
            unfilter <= budget.image_pixel_buffer_bytes
                && scratch <= budget.image_pixel_buffer_bytes,
            "capability_unavailable: crop PNG decoder pixel-buffer proof exceeds selected limit"
        );
        sum(&[budget.image_pixel_buffer_bytes, unfilter, scratch])?
    };
    let encoder_aux = sum(&[
        mul(&[output_upper, 2])?,
        mul(&[u64::from(crop.width), 4])?,
        16_384,
    ])?;
    let working = sum(&[
        mul(&[image.artifact.bytes, 4])?,
        source_decoded,
        decoder_aux,
        crop_rgba,
        encoder_aux,
        output_upper,
        FIXED_WORKING,
    ])?;
    ensure!(
        working <= budget.owned_working_bytes,
        "resource_limit: crop decoder/encoder working admission"
    );
    Ok(CropAdmission {
        encoded_source_bytes: image.artifact.bytes,
        source_decoded_upper_bytes: source_decoded,
        decoder_aux_upper_bytes: decoder_aux,
        crop_rgba_bytes: crop_rgba,
        encoder_aux_upper_bytes: encoder_aux,
        encoded_output_upper_bytes: output_upper,
        owned_working_upper_bytes: working,
    })
}

fn allocate(bytes: u64) -> Result<Vec<u8>> {
    let bytes = usize::try_from(bytes).context("crop allocation exceeds address space")?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes)
        .context("resource_limit: crop allocation failed")?;
    output.resize(bytes, 0);
    Ok(output)
}
fn sum(values: &[u64]) -> Result<u64> {
    values
        .iter()
        .try_fold(0u64, |a, b| a.checked_add(*b).context("crop size overflow"))
}
fn mul(values: &[u64]) -> Result<u64> {
    values
        .iter()
        .try_fold(1u64, |a, b| a.checked_mul(*b).context("crop size overflow"))
}
fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct CropWriter {
    bytes: Vec<u8>,
    limit: u64,
}
impl Write for CropWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(data.len())
            .ok_or_else(|| io::Error::other("crop output overflow"))?;
        if next as u64 > self.limit {
            return Err(io::Error::other(
                "resource_limit: crop PNG writer admission",
            ));
        }
        self.bytes
            .try_reserve_exact(data.len())
            .map_err(|_| io::Error::other("crop writer allocation failed"))?;
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn copy_pixels(
    source: &[u8],
    output: &mut [u8],
    width: u32,
    crop: PixelCrop,
    color: ColorType,
) -> Result<()> {
    let bpp = usize::from(color.bytes_per_pixel());
    for row in 0..crop.height {
        for column in 0..crop.width {
            let index = usize::try_from(
                (u64::from(crop.y + row) * u64::from(width) + u64::from(crop.x + column))
                    * bpp as u64,
            )?;
            let pixel = source
                .get(index..index + bpp)
                .context("crop decoded pixel outside buffer")?;
            let target =
                usize::try_from((u64::from(row) * u64::from(crop.width) + u64::from(column)) * 4)?;
            let rgba = match color {
                ColorType::L8 => [pixel[0], pixel[0], pixel[0], 255],
                ColorType::La8 => [pixel[0], pixel[0], pixel[0], pixel[1]],
                ColorType::Rgb8 => [pixel[0], pixel[1], pixel[2], 255],
                ColorType::Rgba8 => [pixel[0], pixel[1], pixel[2], pixel[3]],
                _ => bail!("capability_unavailable: crop sample conversion"),
            };
            output[target..target + 4].copy_from_slice(&rgba);
        }
    }
    Ok(())
}

fn png_header(bytes: &[u8]) -> Result<(u32, u32)> {
    ensure!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "crop_invalid_png_signature"
    );
    let mut offset = 8usize;
    let mut dimensions = None;
    let mut data = false;
    let mut ended = false;
    let mut chunks = 0;
    while offset < bytes.len() {
        chunks += 1;
        ensure!(
            chunks <= 8192,
            "capability_unavailable: crop PNG chunk count"
        );
        let prefix = bytes
            .get(offset..offset + 8)
            .context("crop PNG truncated chunk")?;
        let len = u32::from_be_bytes(prefix[..4].try_into()?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(len))
            .context("crop PNG chunk overflow")?;
        let payload = bytes
            .get(offset + 8..end - 4)
            .context("crop PNG truncated payload")?;
        ensure!(end <= bytes.len(), "crop PNG missing CRC");
        let kind = &prefix[4..8];
        ensure!(
            dimensions.is_some() || kind == b"IHDR",
            "crop PNG IHDR order"
        );
        match kind {
            b"IHDR" => {
                ensure!(
                    dimensions.is_none() && len == 13,
                    "crop PNG duplicate/invalid IHDR"
                );
                ensure!(
                    payload[8] == 8
                        && matches!(payload[9], 0 | 2 | 4 | 6)
                        && payload[10..13] == [0, 0, 0],
                    "capability_unavailable: crop PNG requires 8-bit noninterlaced gray/RGB samples without metadata"
                );
                dimensions = Some((
                    u32::from_be_bytes(payload[..4].try_into()?),
                    u32::from_be_bytes(payload[4..8].try_into()?),
                ));
            }
            b"IDAT" => {
                ensure!(!ended && len > 0, "crop PNG invalid IDAT");
                data = true;
            }
            b"IEND" => {
                ensure!(
                    len == 0 && data && end == bytes.len(),
                    "crop PNG invalid IEND"
                );
                ended = true;
            }
            _ => bail!(
                "capability_unavailable: crop PNG excludes palette, animation, orientation/color/text metadata and unknown chunks"
            ),
        }
        offset = end;
    }
    ensure!(ended, "crop PNG missing IEND");
    dimensions.context("crop PNG missing IHDR")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactKind, ArtifactRef, artifact_id};
    use std::path::PathBuf;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn fixture(width: u32, height: u32, jpeg: bool) -> (OcrImage, tempfile::NamedTempFile) {
        let samples = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    [
                        (x as u8).wrapping_mul(31),
                        (y as u8).wrapping_mul(47),
                        17,
                        255,
                    ]
                })
            })
            .collect::<Vec<_>>();
        let mut encoded = Vec::new();
        if jpeg {
            let rgb = samples
                .chunks_exact(4)
                .flat_map(|p| p[..3].iter().copied())
                .collect::<Vec<_>>();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 90)
                .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .unwrap();
        } else {
            PngEncoder::new(&mut encoded)
                .write_image(&samples, width, height, image::ExtendedColorType::Rgba8)
                .unwrap();
        }
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&encoded).unwrap();
        let content = sha(&encoded);
        let image = OcrImage {
            artifact: ArtifactRef {
                id: artifact_id(
                    HASH,
                    HASH,
                    &ArtifactKind::Rendition,
                    Some(1),
                    None,
                    &content,
                )
                .unwrap(),
                kind: ArtifactKind::Rendition,
                relative_path: PathBuf::from("page.png"),
                sha256: content,
                profile_sha256: HASH.into(),
                bytes: encoded.len() as u64,
                mime: if jpeg { "image/jpeg" } else { "image/png" }.into(),
                page: Some(1),
                region_id: None,
            },
            path: file.path().into(),
            width,
            height,
            page_to_raster: AffineTransform::IDENTITY,
        };
        (image, file)
    }
    fn crop(
        image: &OcrImage,
        file: &tempfile::NamedTempFile,
        bbox: BoundingBox,
        page: (f64, f64),
        budget: &ResourceBudget,
    ) -> Result<CropOutput> {
        crop_rendition(image, file.reopen().unwrap(), bbox, page, budget)
    }
    fn bbox(x: f64, y: f64, width: f64, height: f64) -> BoundingBox {
        BoundingBox {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn pixels_scale_outward_rounding_and_recipe_are_deterministic() {
        let (mut image, file) = fixture(4, 3, false);
        image.page_to_raster.a = 0.5;
        image.page_to_raster.d = 0.5;
        let a = crop(
            &image,
            &file,
            bbox(2.1, 2.1, 3.7, 1.7),
            (8.0, 6.0),
            &ResourceBudget::default(),
        )
        .unwrap();
        assert_eq!(
            a.recipe.pixels,
            PixelCrop {
                x: 1,
                y: 1,
                width: 2,
                height: 1
            }
        );
        let decoded = image::load_from_memory(&a.png).unwrap().to_rgba8();
        assert_eq!(decoded.as_raw(), &vec![31, 47, 17, 255, 62, 47, 17, 255]);
        for point in [(0.0, 0.0), (2.0, 1.0), (1.3, 0.4)] {
            let p = a.recipe.crop_to_page.apply(point.0, point.1);
            let back = a.recipe.page_to_crop.apply(p.0, p.1);
            assert!((back.0 - point.0).abs() < 1e-10 && (back.1 - point.1).abs() < 1e-10);
        }
        let b = crop(
            &image,
            &file,
            bbox(2.1, 2.1, 3.7, 1.7),
            (8.0, 6.0),
            &ResourceBudget::default(),
        )
        .unwrap();
        assert_eq!(a.png, b.png);
        assert_eq!(a.recipe_sha256, b.recipe_sha256);
        assert_eq!(a.sha256, sha(&a.png));
    }
    #[test]
    fn quarter_turn_and_flip_keep_raster_sample_orientation() {
        let (mut image, file) = fixture(4, 3, false);
        image.page_to_raster = AffineTransform {
            a: 0.0,
            b: 1.0,
            c: -1.0,
            d: 0.0,
            e: 4.0,
            f: 0.0,
        };
        let result = crop(
            &image,
            &file,
            bbox(0.0, 0.0, 1.0, 1.0),
            (3.0, 4.0),
            &ResourceBudget::default(),
        )
        .unwrap();
        assert_eq!(
            result.recipe.pixels,
            PixelCrop {
                x: 3,
                y: 0,
                width: 1,
                height: 1
            }
        );
        assert_eq!(
            image::load_from_memory(&result.png)
                .unwrap()
                .to_rgba8()
                .as_raw(),
            &vec![93, 0, 17, 255]
        );
        image.page_to_raster = AffineTransform {
            a: -1.0,
            e: 4.0,
            ..AffineTransform::IDENTITY
        };
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 1.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap()
            .recipe
            .pixels
            .x,
            3
        );
    }
    #[test]
    fn jpeg_crop_is_explicitly_unavailable_without_decoder() {
        let (image, file) = fixture(4, 3, true);
        let budget = ResourceBudget::default();
        assert_eq!(
            crop(&image, &file, bbox(1.0, 1.0, 2.0, 1.0), (4.0, 3.0), &budget)
                .unwrap_err()
                .to_string(),
            JPEG_UNAVAILABLE
        );
        assert_eq!(budget.staging_used(), 0);
    }
    #[test]
    fn invalid_empty_outside_and_unqualified_geometry_fail() {
        let (mut image, file) = fixture(4, 3, false);
        for selected in [
            bbox(0.0, 0.0, 0.0, 1.0),
            bbox(-1.0, 0.0, 1.0, 1.0),
            bbox(3.9, 0.0, 0.2, 1.0),
            bbox(f64::NAN, 0.0, 1.0, 1.0),
        ] {
            assert!(
                crop(
                    &image,
                    &file,
                    selected,
                    (4.0, 3.0),
                    &ResourceBudget::default()
                )
                .is_err()
            );
        }
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(1.0, 0.0, f64::EPSILON / 4.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string(),
            "crop_empty_pixels"
        );
        image.page_to_raster.e = 5.0;
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string(),
            "crop_outside_parent_pixels"
        );
        image.page_to_raster = AffineTransform {
            c: 0.2,
            ..AffineTransform::IDENTITY
        };
        assert!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string()
            .starts_with("capability_unavailable:")
        );
        image.page_to_raster = AffineTransform {
            a: 1e308,
            d: 1e308,
            ..AffineTransform::IDENTITY
        };
        assert!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .is_err()
        );
    }
    #[test]
    fn source_digest_dimension_and_metadata_changes_are_rejected() {
        let (mut image, mut file) = fixture(4, 3, false);
        image.width = 5;
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string(),
            "crop_source_dimensions_changed"
        );
        image.width = 4;
        file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"bad").unwrap();
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string(),
            "crop_source_bytes_changed"
        );
        let (mut image, file) = fixture(4, 3, false);
        image.artifact.mime = "image/jpeg".into();
        assert!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .is_err()
        );
    }
    #[test]
    fn tighter_admissions_and_arithmetic_overflow_fail_before_decode() {
        let (image, file) = fixture(4, 3, false);
        let mut working = ResourceBudget::default();
        working.owned_working_bytes = 1_048_576;
        let mut base64 = ResourceBudget::default();
        base64.image_base64_bytes = 500;
        let mut decoded = ResourceBudget::default();
        decoded.image_pixel_buffer_bytes = 47;
        for budget in [working, base64, decoded] {
            assert!(crop(&image, &file, bbox(0.0, 0.0, 1.0, 1.0), (4.0, 3.0), &budget).is_err());
        }
        let mut over_pixels = image.clone();
        over_pixels.width = u32::MAX;
        over_pixels.height = u32::MAX;
        assert!(
            crop(
                &over_pixels,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string()
            .starts_with("resource_limit:")
        );
        assert!(sum(&[u64::MAX, 1]).is_err());
        assert!(mul(&[u64::MAX, 4]).is_err());
        let oversized = PixelCrop {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        assert_eq!(
            admit(&image, oversized, &ResourceBudget::default())
                .unwrap_err()
                .to_string(),
            "resource_limit: crop PNG output admission"
        );
        let mut large_parent = image.clone();
        large_parent.width = 2500;
        large_parent.height = 2000;
        let budget = ResourceBudget::default();
        assert!(
            budget
                .admit_image(
                    large_parent.width,
                    large_parent.height,
                    large_parent.artifact.bytes
                )
                .is_ok()
        );
        assert_eq!(
            admit(
                &large_parent,
                PixelCrop {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                },
                &budget
            )
            .unwrap_err()
            .to_string(),
            "capability_unavailable: crop PNG decoder pixel-buffer proof exceeds selected limit"
        );
    }
    #[test]
    fn output_writer_refuses_before_vec_exceeds_cap_and_subset_rejects_metadata() {
        let mut writer = CropWriter {
            bytes: Vec::new(),
            limit: 16,
        };
        writer.write_all(&[1; 16]).unwrap();
        assert!(writer.write_all(&[2]).is_err());
        assert_eq!(writer.bytes.len(), 16);
        let (image, file) = fixture(4, 3, false);
        let mut encoded = std::fs::read(file.path()).unwrap();
        encoded[28] = 1;
        assert!(
            png_header(&encoded)
                .unwrap_err()
                .to_string()
                .starts_with("capability_unavailable:")
        );
        let mut tight = ResourceBudget::default();
        tight.image_file_bytes = image.artifact.bytes.max(100);
        assert_eq!(
            crop(&image, &file, bbox(0.0, 0.0, 1.0, 1.0), (4.0, 3.0), &tight)
                .unwrap_err()
                .to_string(),
            "resource_limit: crop PNG output admission"
        );
    }

    #[test]
    fn actual_png_output_near_cap_is_admitted_and_larger_crop_is_rejected() {
        let (image, file) = fixture(512, 512, false);
        let budget = ResourceBudget::default();
        let admitted = crop(
            &image,
            &file,
            bbox(0.0, 0.0, 510.0, 510.0),
            (512.0, 512.0),
            &budget,
        )
        .unwrap();
        assert!(
            admitted.png.len() > 1_000_000 && admitted.png.len() as u64 <= budget.image_file_bytes
        );
        assert!(admitted.png.len() as u64 <= admitted.admission.encoded_output_upper_bytes);
        assert_eq!(
            image::load_from_memory(&admitted.png)
                .unwrap()
                .to_rgba8()
                .dimensions(),
            (510, 510)
        );
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 512.0, 512.0),
                (512.0, 512.0),
                &budget
            )
            .unwrap_err()
            .to_string(),
            "resource_limit: crop PNG output admission"
        );
        assert_eq!(budget.staging_used(), 0);
    }

    #[test]
    fn gray_alpha_samples_and_inverse_coordinates_are_preserved() {
        let (mut image, mut file) = fixture(2, 2, false);
        let samples = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&samples, 2, 2, image::ExtendedColorType::La8)
            .unwrap();
        file.as_file_mut().set_len(0).unwrap();
        file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&encoded).unwrap();
        image.artifact.bytes = encoded.len() as u64;
        image.artifact.sha256 = sha(&encoded);
        image.artifact.id = artifact_id(
            HASH,
            HASH,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &image.artifact.sha256,
        )
        .unwrap();
        let output = crop(
            &image,
            &file,
            bbox(1.0, 0.0, 1.0, 2.0),
            (2.0, 2.0),
            &ResourceBudget::default(),
        )
        .unwrap();
        assert_eq!(
            image::load_from_memory(&output.png)
                .unwrap()
                .to_rgba8()
                .as_raw(),
            &vec![30, 30, 30, 40, 70, 70, 70, 80]
        );
        assert_eq!(output.recipe.raster_to_crop.apply(1.0, 1.0), (0.0, 1.0));
        assert_eq!(output.recipe.crop_to_raster.apply(0.0, 1.0), (1.0, 1.0));
    }

    #[test]
    fn source_size_and_png_crc_fail_closed() {
        let (image, file) = fixture(4, 3, false);
        file.as_file().set_len(image.artifact.bytes - 1).unwrap();
        assert_eq!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .unwrap_err()
            .to_string(),
            "crop_source_file_changed"
        );
        let (mut image, mut file) = fixture(4, 3, false);
        let mut encoded = std::fs::read(file.path()).unwrap();
        encoded[32] ^= 1;
        file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&encoded).unwrap();
        image.artifact.sha256 = sha(&encoded);
        assert!(
            crop(
                &image,
                &file,
                bbox(0.0, 0.0, 1.0, 1.0),
                (4.0, 3.0),
                &ResourceBudget::default()
            )
            .is_err()
        );
    }
}
