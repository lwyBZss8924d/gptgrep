//! Owned image-to-PDF front door. Its container subset is deliberately narrower
//! than `image`: compressed metadata, animation and coefficient-heavy JPEG
//! modes are rejected before constructing an upstream decoder.

use std::fs::OpenOptions;
use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::{ColorType, ImageDecoder, ImageFormat, ImageReader, Limits};

use crate::artifact::{FeatureState, hash_file, json_sha256, read_bounded};
use crate::geometry::AffineTransform;
use crate::resource::{LimitedWriter, ResourceBudget};
use crate::rich::{ConversionIdentity, PreparedImageGeometry};

pub(crate) const CONVERTER: &str = "gptgrep-bounded-image-pdf";
pub(crate) const CONVERTER_VERSION: &str = "image-0.25.10/rgb8-pdf-v1";
const FIXED_WORKING_BYTES: u64 = 1_048_576;
const PDF_OVERHEAD_BYTES: u64 = 16_384;

pub(crate) struct ImagePreparation {
    bytes: Vec<u8>,
    header: Header,
    pub(crate) geometry: PreparedImageGeometry,
    pub(crate) output_upper: u64,
    pub(crate) profile_sha256: String,
}

#[derive(Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    format: ImageFormat,
    color_metadata: bool,
}

fn checked_sum(values: &[u64]) -> Result<u64> {
    values.iter().try_fold(0u64, |sum, value| {
        sum.checked_add(*value)
            .context("resource_limit: image working size overflow")
    })
}

impl ImagePreparation {
    pub(crate) fn admit(
        source: &Path,
        source_bytes: u64,
        source_sha256: &str,
        format: ImageFormat,
        budget: &ResourceBudget,
    ) -> Result<Self> {
        // The first owned buffer contains only encoded source bytes. File size
        // and this buffer's allowance are checked before read_bounded allocates.
        ensure!(
            source_bytes <= budget.image_file_bytes
                && checked_sum(&[source_bytes, FIXED_WORKING_BYTES])? <= budget.owned_working_bytes,
            "resource_limit: encoded image input admission failed"
        );
        let bytes = read_bounded(source, source_bytes, source_sha256)?;
        let header = match format {
            ImageFormat::Png => png_header(&bytes)?,
            ImageFormat::Jpeg => jpeg_header(&bytes)?,
            _ => bail!("capability_unavailable: bounded image format"),
        };
        let pixels = budget.admit_image(header.width, header.height, source_bytes)?;
        let rgba_upper = pixels.checked_mul(4).context("image byte overflow")?;
        let row_upper = u64::from(header.width) * 3;
        // PNG's explicit allocation quota is additional to its unfilter buffer
        // (not charged by png::Limits). At most twice the admitted raw frame,
        // row scratch and fixed inflate space cover that buffer's growth.
        // Baseline, one-scan JPEG has row coefficient/upsampling arrays only:
        // 1024*(width+16) dominates the pinned zune-jpeg arrays for <=3 components
        // with sampling <=2. No progressive/full-image coefficient path enters.
        let decoder_upper = match format {
            ImageFormat::Png => checked_sum(&[
                budget.image_pixel_buffer_bytes,
                // Filter bytes count even for one-pixel-wide rows. The Vec
                // growth allowance is twice the frame plus one 8KiB growth.
                (u64::from(header.width) * 4 + 1) * u64::from(header.height) * 2 + 16_384,
                u64::from(header.width) * 32,
            ])?,
            ImageFormat::Jpeg => (u64::from(header.width) + 16) * 1024,
            _ => unreachable!(),
        };
        // Both the original encoded buffer and JPEG's internal encoded copy
        // (including growth/metadata copies) are covered conservatively.
        let working_upper = checked_sum(&[
            source_bytes * 4,
            rgba_upper,
            row_upper,
            decoder_upper,
            FIXED_WORKING_BYTES,
        ])?;
        ensure!(
            working_upper <= budget.owned_working_bytes,
            "resource_limit: image decoder/color-conversion working admission failed"
        );
        let output_upper = checked_sum(&[pixels * 3, PDF_OVERHEAD_BYTES])?;
        ensure!(
            output_upper <= budget.document_staging_bytes,
            "resource_limit: prepared image PDF output admission failed"
        );
        budget.reserve_staging(output_upper)?;
        let geometry = PreparedImageGeometry {
            source_width_pixels: header.width,
            source_height_pixels: header.height,
            page_width_points: f64::from(header.width),
            page_height_points: f64::from(header.height),
            source_pixels_to_page: AffineTransform::IDENTITY,
            source_orientation: FeatureState::Absent,
            source_color_profile: if header.color_metadata {
                FeatureState::Unsupported(
                    "bounded color metadata retained in the original source; samples are not color managed".into(),
                )
            } else {
                FeatureState::Absent
            },
            alpha_background: "#ffffff".into(),
        };
        let profile_sha256 = image_preparation_profile_digest(
            if format == ImageFormat::Png {
                "png"
            } else {
                "jpeg"
            },
            &geometry,
        )?;
        Ok(Self {
            bytes,
            header,
            geometry,
            output_upper,
            profile_sha256,
        })
    }

    pub(crate) fn write(self, path: &Path, budget: &ResourceBudget) -> Result<ImageOutput> {
        // No decoder is created until the full simultaneous working envelope
        // and output staging have been admitted above.
        let mut limits = Limits::default();
        limits.max_image_width = Some(self.header.width);
        limits.max_image_height = Some(self.header.height);
        limits.max_alloc = Some(budget.image_pixel_buffer_bytes);
        let mut reader = ImageReader::with_format(Cursor::new(&self.bytes), self.header.format);
        reader.limits(limits);
        let decoder = reader.into_decoder()?;
        ensure!(
            decoder.dimensions() == (self.header.width, self.header.height),
            "image decoder/header geometry mismatch"
        );
        let color = decoder.color_type();
        ensure!(
            matches!(
                color,
                ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8
            ),
            "capability_unavailable: bounded image decoder requires 8-bit samples"
        );
        let decoded_bytes = decoder.total_bytes();
        ensure!(
            decoded_bytes <= budget.image_pixel_buffer_bytes
                && decoded_bytes
                    == u64::from(self.header.width)
                        * u64::from(self.header.height)
                        * u64::from(color.bytes_per_pixel()),
            "resource_limit: decoded image buffer admission failed"
        );
        let mut decoded = vec![0; usize::try_from(decoded_bytes)?];
        decoder.read_image(&mut decoded)?;
        let output = OpenOptions::new().create_new(true).write(true).open(path)?;
        let mut output = LimitedWriter {
            inner: output,
            limit: self.output_upper,
            written: 0,
        };
        write_pdf(&mut output, self.header, color, &decoded)?;
        output.inner.sync_all()?;
        let (bytes, sha256) = hash_file(path, self.output_upper)?;
        ensure!(bytes == output.written, "prepared PDF length changed");
        Ok(ImageOutput {
            bytes,
            sha256,
            geometry: self.geometry,
            profile_sha256: self.profile_sha256,
            conversion: ConversionIdentity {
                converter: CONVERTER.into(),
                version: CONVERTER_VERSION.into(),
                invocations: 1,
            },
        })
    }
}

/// Canonical recipe binding for the owned bounded image converter. This is a
/// pure metadata helper; it does not prove that untrusted source bytes were
/// converted, nor replace publication's source/artifact validation.
pub fn image_preparation_profile_digest(
    source_format: &str,
    geometry: &PreparedImageGeometry,
) -> Result<String> {
    ensure!(
        matches!(source_format, "png" | "jpeg"),
        "invalid image source format"
    );
    ensure!(
        geometry.source_width_pixels > 0
            && geometry.source_height_pixels > 0
            && geometry.page_width_points == f64::from(geometry.source_width_pixels)
            && geometry.page_height_points == f64::from(geometry.source_height_pixels)
            && geometry.source_pixels_to_page == AffineTransform::IDENTITY
            && geometry.alpha_background == "#ffffff",
        "invalid bounded image geometry/recipe"
    );
    ensure!(
        matches!(geometry.source_orientation, FeatureState::Absent),
        "unsupported image orientation recipe"
    );
    ensure!(
        matches!(
            &geometry.source_color_profile,
            FeatureState::Absent | FeatureState::Unsupported(_)
        ),
        "unsupported image color-profile recipe"
    );
    json_sha256(
        &(
            "gptgrep.prepare.bounded-image.v1",
            CONVERTER,
            CONVERTER_VERSION,
            source_format,
            "png-0.18.1/8bit-noninterlaced-static;zune-jpeg-0.5.15/baseline-single-scan-sampling<=2",
            "RGB8-uncompressed-PDF1.4;integer-white-alpha-round-nearest;one-pixel-one-point;raw-sample-order",
            geometry,
        ),
        PDF_OVERHEAD_BYTES,
    )
}

pub(crate) struct ImageOutput {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) geometry: PreparedImageGeometry,
    pub(crate) profile_sha256: String,
    pub(crate) conversion: ConversionIdentity,
}

fn png_header(bytes: &[u8]) -> Result<Header> {
    ensure!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "invalid PNG signature"
    );
    let mut offset = 8usize;
    let mut header = None;
    let mut image_data = false;
    let mut ended = false;
    let mut color_metadata = false;
    while offset < bytes.len() {
        let prefix = bytes
            .get(offset..offset + 8)
            .context("truncated PNG chunk")?;
        let len = u32::from_be_bytes(prefix[..4].try_into()?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(len))
            .context("PNG chunk overflow")?;
        let payload = bytes
            .get(offset + 8..end - 4)
            .context("truncated PNG chunk payload")?;
        ensure!(end <= bytes.len(), "truncated PNG chunk CRC");
        let kind = &prefix[4..8];
        ensure!(
            header.is_some() || kind == b"IHDR",
            "PNG IHDR must be first"
        );
        match kind {
            b"IHDR" => {
                ensure!(header.is_none() && len == 13, "invalid/duplicate PNG IHDR");
                ensure!(
                    payload[8] == 8
                        && matches!(payload[9], 0 | 2 | 3 | 4 | 6)
                        && payload[10..13] == [0, 0, 0],
                    "capability_unavailable: bounded PNG requires 8-bit noninterlaced samples"
                );
                header = Some(Header {
                    width: u32::from_be_bytes(payload[..4].try_into()?),
                    height: u32::from_be_bytes(payload[4..8].try_into()?),
                    format: ImageFormat::Png,
                    color_metadata: false,
                });
            }
            b"IDAT" => image_data = true,
            b"IEND" => {
                ensure!(
                    len == 0 && image_data && end == bytes.len(),
                    "invalid PNG end/image data"
                );
                ended = true;
            }
            b"PLTE" => ensure!(len <= 768 && len.is_multiple_of(3), "invalid PNG palette"),
            b"tRNS" => ensure!(len <= 256, "invalid PNG transparency"),
            b"gAMA" | b"cHRM" | b"sRGB" => {
                let expected = if kind == b"gAMA" {
                    4
                } else if kind == b"cHRM" {
                    32
                } else {
                    1
                };
                ensure!(len == expected, "invalid PNG color metadata");
                color_metadata = true;
            }
            b"pHYs" => ensure!(len == 9, "invalid PNG physical geometry"),
            _ => bail!(
                "capability_unavailable: bounded PNG excludes animation, orientation, compressed metadata and unknown chunks"
            ),
        }
        offset = end;
    }
    ensure!(ended, "PNG missing IEND");
    let mut header = header.context("PNG missing IHDR")?;
    header.color_metadata = color_metadata;
    Ok(header)
}

fn jpeg_header(bytes: &[u8]) -> Result<Header> {
    ensure!(bytes.starts_with(&[0xff, 0xd8]), "invalid JPEG signature");
    let mut offset = 2usize;
    let mut header = None;
    let mut components = [0u8; 3];
    let mut count = 0usize;
    loop {
        ensure!(bytes.get(offset) == Some(&0xff), "invalid JPEG marker");
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset).context("truncated JPEG marker")?;
        offset += 1;
        let length = bytes
            .get(offset..offset + 2)
            .context("truncated JPEG segment")?;
        let length = u16::from_be_bytes(length.try_into()?) as usize;
        ensure!(length >= 2, "invalid JPEG segment length");
        let end = offset
            .checked_add(length)
            .context("JPEG segment overflow")?;
        let payload = bytes
            .get(offset + 2..end)
            .context("truncated JPEG segment payload")?;
        match marker {
            0xc0 => {
                ensure!(
                    header.is_none() && payload.len() >= 6 && payload[0] == 8,
                    "capability_unavailable: bounded JPEG requires one 8-bit baseline frame"
                );
                count = usize::from(payload[5]);
                ensure!(
                    matches!(count, 1 | 3) && payload.len() == 6 + count * 3,
                    "capability_unavailable: bounded JPEG requires gray or three-component samples"
                );
                for index in 0..count {
                    components[index] = payload[6 + index * 3];
                    ensure!(
                        !components[..index].contains(&components[index]),
                        "duplicate JPEG component"
                    );
                    let sampling = payload[7 + index * 3];
                    ensure!(
                        (1..=2).contains(&(sampling >> 4)) && (1..=2).contains(&(sampling & 15)),
                        "capability_unavailable: bounded JPEG sampling factors exceed two"
                    );
                }
                header = Some(Header {
                    width: u32::from(u16::from_be_bytes(payload[3..5].try_into()?)),
                    height: u32::from(u16::from_be_bytes(payload[1..3].try_into()?)),
                    format: ImageFormat::Jpeg,
                    color_metadata: false,
                });
            }
            0xda => {
                ensure!(
                    header.is_some()
                        && payload.len() == 1 + count * 2 + 3
                        && usize::from(payload[0]) == count
                        && payload[payload.len() - 3..] == [0, 63, 0],
                    "capability_unavailable: bounded JPEG requires one complete baseline scan"
                );
                let mut seen = [false; 3];
                for index in 0..count {
                    let component = payload[1 + index * 2];
                    let position = components[..count]
                        .iter()
                        .position(|id| *id == component)
                        .context("unknown JPEG scan component")?;
                    ensure!(!seen[position], "duplicate JPEG scan component");
                    seen[position] = true;
                }
                // Scan entropy syntax without decoding. Stuffed bytes and
                // restart markers are bounded; another scan/frame is excluded.
                offset = end;
                while offset < bytes.len() {
                    if bytes[offset] != 0xff {
                        offset += 1;
                        continue;
                    }
                    offset += 1;
                    while bytes.get(offset) == Some(&0xff) {
                        offset += 1;
                    }
                    let marker = *bytes.get(offset).context("truncated JPEG entropy marker")?;
                    offset += 1;
                    match marker {
                        0x00 | 0xd0..=0xd7 => {}
                        0xd9 => {
                            ensure!(offset == bytes.len(), "JPEG trailing bytes unsupported");
                            return header.context("JPEG missing frame");
                        }
                        _ => bail!(
                            "capability_unavailable: bounded JPEG excludes additional scans and entropy extensions"
                        ),
                    }
                }
                bail!("JPEG missing EOI");
            }
            0xc4 | 0xdb | 0xdd => {}
            0xe0 => ensure!(
                payload.starts_with(b"JFIF\0"),
                "capability_unavailable: bounded JPEG APP0 metadata is not JFIF"
            ),
            _ => bail!(
                "capability_unavailable: bounded JPEG excludes progressive, arithmetic, orientation/color metadata and unknown segments"
            ),
        }
        offset = end;
    }
}

fn write_pdf<W: Write>(
    writer: &mut LimitedWriter<W>,
    header: Header,
    color: ColorType,
    decoded: &[u8],
) -> Result<()> {
    let (width, height) = (header.width, header.height);
    let rgb_bytes = u64::from(width) * u64::from(height) * 3;
    let content = format!("q\n{width} 0 0 {height} 0 0 cm\n/Im0 Do\nQ\n");
    let mut offsets = [0u64; 6];
    writer.write_all(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
        ),
    ];
    for (index, object) in objects.iter().enumerate() {
        offsets[index + 1] = writer.written;
        writeln!(writer, "{} 0 obj\n{object}\nendobj", index + 1)?;
    }
    offsets[4] = writer.written;
    writeln!(
        writer,
        "4 0 obj\n<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Interpolate false /Length {rgb_bytes} >>\nstream"
    )?;
    // Only one admitted RGB row exists; the full decoded buffer stays in its
    // original color representation. Alpha composites against white exactly.
    let mut rgb_row = vec![0u8; width as usize * 3];
    let input_row = width as usize * color.bytes_per_pixel() as usize;
    for row in decoded.chunks_exact(input_row) {
        for (index, pixel) in row
            .chunks_exact(color.bytes_per_pixel() as usize)
            .enumerate()
        {
            let (samples, alpha) = match color {
                ColorType::L8 => ([pixel[0]; 3], 255),
                ColorType::La8 => ([pixel[0]; 3], pixel[1]),
                ColorType::Rgb8 => ([pixel[0], pixel[1], pixel[2]], 255),
                ColorType::Rgba8 => ([pixel[0], pixel[1], pixel[2]], pixel[3]),
                _ => unreachable!(),
            };
            for channel in 0..3 {
                rgb_row[index * 3 + channel] = ((u32::from(samples[channel]) * u32::from(alpha)
                    + 255 * (255 - u32::from(alpha))
                    + 127)
                    / 255) as u8;
            }
        }
        writer.write_all(&rgb_row)?;
    }
    writer.write_all(b"\nendstream\nendobj\n")?;
    offsets[5] = writer.written;
    writeln!(
        writer,
        "5 0 obj\n<< /Length {} >>\nstream\n{content}endstream\nendobj",
        content.len()
    )?;
    let xref = writer.written;
    writer.write_all(b"xref\n0 6\n0000000000 65535 f \n")?;
    for offset in offsets.iter().skip(1) {
        writeln!(writer, "{offset:010} 00000 n ")?;
    }
    writeln!(
        writer,
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )?;
    Ok(())
}
