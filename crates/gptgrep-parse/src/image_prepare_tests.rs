use super::*;
use image::{ExtendedColorType, ImageEncoder};

fn serialized_value(value: &impl serde::Serialize) -> serde_json::Value {
    serde_json::from_slice(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn rgba_fixture() -> (u32, u32, Vec<u8>) {
    let (width, height) = (24, 16);
    let mut pixels = Vec::new();
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&if y < height / 2 {
                if x < width / 2 {
                    [240, 20, 10, 255]
                } else {
                    [10, 180, 30, 128]
                }
            } else if x < width / 2 {
                [20, 30, 220, 0]
            } else {
                [200, 100, 40, 255]
            });
        }
    }
    (width, height, pixels)
}

fn png_fixture() -> Vec<u8> {
    let (width, height, pixels) = rgba_fixture();
    let mut output = Vec::new();
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
        .unwrap();
    output
}

fn jpeg_fixture() -> Vec<u8> {
    let (width, height, pixels) = rgba_fixture();
    let rgb = pixels
        .chunks_exact(4)
        .flat_map(|pixel| pixel[..3].iter().copied())
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 90)
        .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    output
}

fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(payload);
    let mut crc = !0u32;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    chunk
}

fn embedded_rgb(pdf: &[u8], width: u32, height: u32) -> &[u8] {
    let marker = b"4 0 obj\n";
    let object = pdf
        .windows(marker.len())
        .position(|bytes| bytes == marker)
        .unwrap();
    let marker = b"stream\n";
    let offset = object
        + pdf[object..]
            .windows(marker.len())
            .position(|bytes| bytes == marker)
            .unwrap()
        + marker.len();
    &pdf[offset..offset + width as usize * height as usize * 3]
}

#[tokio::test]
async fn bounded_png_pdf_roundtrip_preserves_geometry_pixels_and_one_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("raster.PNG");
    let encoded = png_fixture();
    std::fs::write(&source, &encoded).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("staging"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let summary = prepared.summary();
    assert_eq!(summary.format, "png");
    assert_eq!(summary.source_sha256, crate::artifact::sha256(&encoded));
    assert_ne!(summary.source_sha256, summary.prepared_sha256);
    assert!(
        matches!(&summary.conversion, FeatureState::Present(identity)
        if identity.converter == "gptgrep-bounded-image-pdf"
        && identity.version == "image-0.25.10/png-rgb8-pdf-v2" && identity.invocations == 1)
    );
    let FeatureState::Present(geometry) = &summary.image_geometry else {
        panic!("missing geometry")
    };
    assert_eq!(
        (geometry.source_width_pixels, geometry.source_height_pixels),
        (24, 16)
    );
    assert_eq!(
        (geometry.page_width_points, geometry.page_height_points),
        (24.0, 16.0)
    );
    assert_eq!(geometry.source_pixels_to_page, AffineTransform::IDENTITY);
    assert_eq!(geometry.source_pixels_to_page.apply(4.0, 8.0), (4.0, 8.0));
    assert_eq!(geometry.alpha_background, "#ffffff");
    assert!(matches!(geometry.source_orientation, FeatureState::Absent));
    let pdf = std::fs::read(prepared.prepared_path()).unwrap();
    let (_, _, original) = rgba_fixture();
    let expected = original
        .chunks_exact(4)
        .flat_map(|pixel| {
            (0..3).map(move |channel| {
                ((u32::from(pixel[channel]) * u32::from(pixel[3])
                    + 255 * (255 - u32::from(pixel[3]))
                    + 127)
                    / 255) as u8
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(embedded_rgb(&pdf, 24, 16), expected);
    assert_eq!(budget.staging_used(), 24 * 16 * 3 + 16_384);
    let options = NativeOptions {
        dpi: 72.0,
        ..Default::default()
    };
    let candidate = extract_native(&prepared, &options, &budget).await.unwrap();
    assert_eq!(candidate.pages().len(), 1);
    assert_eq!(candidate.pages()[0].coverage, CoverageState::RequiredOcr);
    assert_eq!(candidate.pages()[0].width_points, 24.0);
    assert_eq!(candidate.pages()[0].height_points, 16.0);
    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            prepared
                .staging_root()
                .join(&candidate.pages()[0].metadata_artifact.relative_path),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        metadata["complexity"],
        serialized_value(&candidate.pages()[0].complexity)
    );
    assert_eq!(
        metadata["geometry"],
        serde_json::to_value(&candidate.pages()[0].geometry).unwrap()
    );
    assert_eq!(
        metadata["image_rects"],
        serde_json::json!([{"x":0.0,"y":0.0,"width":24.0,"height":16.0}])
    );
    let rendered = image::ImageReader::open(&candidate.pages()[0].renditions[0].path)
        .unwrap()
        .decode()
        .unwrap()
        .to_rgb8();
    assert_eq!(rendered.dimensions(), (24, 16));
    assert_eq!(rendered.as_raw(), &expected);
    assert_eq!(candidate.plan().tasks.len(), 1);
    validate_ocr_task(&candidate.plan().tasks[0]).unwrap();
    prepared.verify(&budget).unwrap();
    assert_eq!(std::fs::read(prepared.prepared_path()).unwrap(), pdf);
    assert!(
        matches!(&candidate.summary().conversion, FeatureState::Present(identity) if identity.invocations == 1)
    );
}

#[tokio::test]
async fn native_metadata_binds_vector_classifier_and_original_cropped_rotated_frame() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("native.pdf");
    let mut content = String::from("0 0 0 rg 20 30 50 50 re f BT /F1 8 Tf 30 250 Td ");
    for row in 0..18 {
        if row > 0 {
            content.push_str("0 -9 Td ");
        }
        content.push_str(&format!("(Native text row {row} remains searchable) Tj "));
    }
    content.push_str("ET");
    let bytes = crate::tests::pdf_objects(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /CropBox [10 20 190 280] /Rotate 90 /UserUnit 2 /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ]);
    std::fs::write(&source, bytes).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let candidate = extract_native(
        &prepared,
        &NativeOptions {
            render_pages: false,
            ..Default::default()
        },
        &budget,
    )
    .await
    .unwrap();
    let page = &candidate.pages()[0];
    assert_eq!(page.coverage, CoverageState::RequiredOcr);
    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            prepared
                .staging_root()
                .join(&page.metadata_artifact.relative_path),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["complexity"], serialized_value(&page.complexity));
    assert_eq!(metadata["complexity"]["needs_ocr"], true);
    assert!(page.complexity.text_coverage >= 0.15);
    assert!(
        page.complexity
            .uncovered_vector_area
            .is_some_and(|area| area >= 400.0)
    );
    assert!(
        metadata["complexity"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("vector-text")),
        "{:?}",
        page.complexity
    );
    assert!(
        metadata["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["kind"] == "path")
    );
    assert_eq!(metadata["image_rects"], serde_json::json!([]));
    assert_eq!(
        metadata["geometry"],
        serde_json::to_value(&page.geometry).unwrap()
    );
    assert_eq!(
        metadata["page"]["page_width"].as_f64().unwrap(),
        page.width_points
    );
    assert_eq!(
        metadata["page"]["page_height"].as_f64().unwrap(),
        page.height_points
    );
    assert_eq!(page.geometry.rotation_quarter_turns, 1);
    assert_eq!(page.geometry.user_unit, 2.0);
}

#[tokio::test]
async fn strict_jpeg_incomplete_entropy_fails_before_decoder_or_staging() {
    let encoded = crate::image_prepare::jpeg_strict::tests::fixture(8, 8, &[0]);
    let sos = encoded.windows(2).position(|b| b == [0xff, 0xda]).unwrap();
    let length = usize::from(u16::from_be_bytes([encoded[sos + 2], encoded[sos + 3]]));
    let mut incomplete = encoded[..sos + 2 + length].to_vec();
    incomplete.extend_from_slice(&[0xff, 0xd9]);
    let budget = ResourceBudget::default();
    let (error, staging) = prepare_error(&incomplete, "jpeg", &budget).await;
    assert!(error.contains("premature entropy marker"), "{error}");
    assert!(!staging);
    assert_eq!(budget.staging_used(), 0);
}

#[tokio::test]
async fn native_dpi_has_its_own_pixel_admission_without_resizing_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.png");
    std::fs::write(&source, png_fixture()).unwrap();
    let mut budget = ResourceBudget::default();
    budget.image_pixels = 24 * 16;
    let prepared = prepare_document(
        &source,
        &dir.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let pdf = std::fs::read(prepared.prepared_path()).unwrap();
    let error = extract_native(&prepared, &NativeOptions::default(), &budget)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("resource_limit"), "{error}");
    assert_eq!(std::fs::read(prepared.prepared_path()).unwrap(), pdf);
    assert!(
        !prepared
            .staging_root()
            .join(format!("{}-p1.png", prepared.summary().source_sha256))
            .exists()
    );
    prepared.verify(&budget).unwrap();
}

#[tokio::test]
async fn gray_alpha_palette_and_rgb_samples_use_the_admitted_scanline_conversion() {
    for (color, samples, expected) in [
        (
            ExtendedColorType::L8,
            vec![10, 200],
            vec![10, 10, 10, 200, 200, 200],
        ),
        (
            ExtendedColorType::La8,
            vec![10, 0, 200, 255],
            vec![255, 255, 255, 200, 200, 200],
        ),
        (
            ExtendedColorType::Rgb8,
            vec![10, 20, 30, 200, 210, 220],
            vec![10, 20, 30, 200, 210, 220],
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.png");
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&samples, 2, 1, color)
            .unwrap();
        std::fs::write(&source, encoded).unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &dir.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(
            embedded_rgb(&std::fs::read(prepared.prepared_path()).unwrap(), 2, 1),
            expected
        );
    }
    // Original palette fixture: gray deflate samples are valid palette indices;
    // replace IHDR and add bounded PLTE/tRNS chunks with their actual CRCs.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("palette.png");
    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded)
        .write_image(&[0, 1], 2, 1, ExtendedColorType::L8)
        .unwrap();
    let mut ihdr = encoded[16..29].to_vec();
    ihdr[9] = 3;
    encoded.splice(8..33, png_chunk(b"IHDR", &ihdr));
    let mut palette = png_chunk(b"PLTE", &[10, 20, 30, 200, 210, 220]);
    palette.extend_from_slice(&png_chunk(b"tRNS", &[0, 255]));
    encoded.splice(33..33, palette);
    std::fs::write(&source, encoded).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    assert_eq!(
        embedded_rgb(&std::fs::read(prepared.prepared_path()).unwrap(), 2, 1),
        &[255, 255, 255, 200, 210, 220]
    );
}

#[tokio::test]
async fn bounded_color_metadata_is_explicit_and_source_format_binds_the_recipe() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("gamma.png");
    let mut encoded = png_fixture();
    encoded.splice(33..33, png_chunk(b"gAMA", &45_455u32.to_be_bytes()));
    std::fs::write(&source, encoded).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let FeatureState::Present(geometry) = &prepared.summary().image_geometry else {
        panic!()
    };
    assert!(matches!(
        geometry.source_color_profile,
        FeatureState::Unsupported(_)
    ));
    assert_eq!(
        image_preparation_profile_digest("png", geometry).unwrap(),
        prepared.summary().preparation_profile_sha256
    );
    assert!(image_preparation_profile_digest("jpeg", geometry).is_err());
    let mut forged = geometry.clone();
    forged.source_pixels_to_page.e = 1.0;
    assert!(image_preparation_profile_digest("png", &forged).is_err());
}

async fn prepare_error(bytes: &[u8], extension: &str, budget: &ResourceBudget) -> (String, bool) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join(format!("source.{extension}"));
    std::fs::write(&source, bytes).unwrap();
    let staging = dir.path().join("staging");
    let error = prepare_document(&source, &staging, &PreparationOptions, budget)
        .await
        .unwrap_err()
        .to_string();
    (error, staging.exists())
}

#[tokio::test]
async fn image_headers_are_admitted_before_decoder_and_staging() {
    let mut png = png_fixture();
    // Deliberately invalid IHDR CRC: pixel admission must win over decoding.
    png[16..20].copy_from_slice(&8_000_001u32.to_be_bytes());
    png[20..24].copy_from_slice(&1u32.to_be_bytes());
    let budget = ResourceBudget::default();
    let (error, staging) = prepare_error(&png, "png", &budget).await;
    assert!(error.contains("resource_limit: image exceeds"), "{error}");
    assert!(!staging);
    assert_eq!(budget.staging_used(), 0);
    let mut jpeg = jpeg_fixture();
    let sof = jpeg
        .windows(2)
        .position(|bytes| bytes == [0xff, 0xc0])
        .unwrap();
    jpeg[sof + 5..sof + 9].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    let (error, staging) = prepare_error(&jpeg, "jpeg", &budget).await;
    assert!(error.contains("resource_limit: image exceeds"), "{error}");
    assert!(!staging);

    let encoded = vec![0u8; 1_048_577];
    let (error, staging) = prepare_error(&encoded, "png", &budget).await;
    assert!(error.contains("resource_limit"), "{error}");
    assert!(!staging);
}

#[tokio::test]
async fn decoded_working_pdf_and_shared_staging_limits_precede_decoder() {
    let mut png = png_fixture();
    let idat = png.windows(4).position(|bytes| bytes == b"IDAT").unwrap();
    png[idat + 4] ^= 0xff; // corrupt deflate, leaving a valid container header
    for (field, expected) in [
        ("decoded", "image exceeds"),
        ("working", "working admission"),
        ("output", "PDF output admission"),
        ("run", "shared run staging admission"),
    ] {
        let mut budget = ResourceBudget::default();
        match field {
            "decoded" => budget.image_pixel_buffer_bytes = 24 * 16 * 4 - 1,
            "working" => budget.owned_working_bytes = png.len() as u64 + 1_048_576,
            "output" => budget.document_staging_bytes = 24 * 16 * 3 + 16_384 - 1,
            "run" => budget.run_staging_bytes = 24 * 16 * 3 + 16_384 - 1,
            _ => unreachable!(),
        }
        let (error, staging) = prepare_error(&png, "png", &budget).await;
        assert!(error.contains(expected), "{field}: {error}");
        assert!(!staging, "{field}");
        assert_eq!(budget.staging_used(), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.png");
    std::fs::write(&source, png_fixture()).unwrap();
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 2 * (24 * 16 * 3 + 16_384) - 1;
    let prepared = prepare_document(
        &source,
        &dir.path().join("first"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let error = prepare_document(
        &source,
        &dir.path().join("second"),
        &PreparationOptions,
        &budget.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("shared run staging admission"));
    assert_eq!(budget.staging_used(), 24 * 16 * 3 + 16_384);
    prepared.verify(&budget).unwrap();
}

#[tokio::test]
async fn unsafe_image_container_modes_fail_as_capabilities_before_decode() {
    let mut png = png_fixture();
    png[24] = 16;
    assert!(
        prepare_error(&png, "png", &ResourceBudget::default())
            .await
            .0
            .contains("capability_unavailable")
    );
    for kind in [b"iCCP", b"acTL", b"eXIf"] {
        let mut png = png_fixture();
        let mut chunk = vec![0, 0, 0, 0];
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(&[0, 0, 0, 0]);
        png.splice(33..33, chunk);
        let (error, staging) = prepare_error(&png, "png", &ResourceBudget::default()).await;
        assert!(error.contains("capability_unavailable"), "{error}");
        assert!(!staging);
    }
    let mut jpeg = jpeg_fixture();
    let sof = jpeg
        .windows(2)
        .position(|bytes| bytes == [0xff, 0xc0])
        .unwrap();
    jpeg[sof + 1] = 0xc2;
    assert!(
        prepare_error(&jpeg, "jpeg", &ResourceBudget::default())
            .await
            .0
            .contains("capability_unavailable")
    );
    let mut jpeg = jpeg_fixture();
    let len = jpeg.len();
    jpeg.splice(len - 2..len - 2, [0xff, 0xda, 0, 2]);
    assert!(
        prepare_error(&jpeg, "jpeg", &ResourceBudget::default())
            .await
            .0
            .contains("capability_unavailable")
    );
}

#[tokio::test]
async fn image_source_preparation_and_invocation_remain_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.png");
    std::fs::write(&source, png_fixture()).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("staging"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    assert!(
        prepared
            .verify(&ResourceBudget::default())
            .unwrap_err()
            .to_string()
            .contains("different invocation")
    );
    let original = std::fs::read(&source).unwrap();
    let mut changed = original.clone();
    changed[40] ^= 1;
    std::fs::write(&source, changed).unwrap();
    assert!(
        prepared
            .verify(&budget)
            .unwrap_err()
            .to_string()
            .contains("original source changed")
    );
    std::fs::write(&source, original).unwrap();
    let mut pdf = std::fs::read(prepared.prepared_path()).unwrap();
    pdf[40] ^= 1;
    std::fs::write(prepared.prepared_path(), pdf).unwrap();
    assert!(
        prepared
            .verify(&budget)
            .unwrap_err()
            .to_string()
            .contains("prepared document changed")
    );
}

#[test]
fn existing_pdf_summaries_default_image_geometry_to_absent() {
    let old = serde_json::json!({
        "source_sha256":"1".repeat(64), "prepared_sha256":"1".repeat(64),
        "source_bytes":1, "prepared_bytes":1, "format":"pdf", "parser_identity":RICH_PARSER,
        "preparation_profile_sha256":"2".repeat(64), "conversion":{"state":"absent"}
    });
    let summary: PreparedSummary = serde_json::from_value(old).unwrap();
    assert!(matches!(summary.image_geometry, FeatureState::Absent));
}

#[tokio::test]
async fn bounded_strict_jpeg_pdf_roundtrip_preserves_known_samples_and_profile() {
    for (values, expected) in [
        (&[0][..], &[128, 128, 128][..]),
        (&[-1024, 0, 1016][..], &[0, 128, 255][..]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("constant.jpeg");
        let encoded = crate::image_prepare::jpeg_strict::tests::fixture(13, 9, values);
        std::fs::write(&source, &encoded).unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &dir.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        let summary = prepared.summary();
        assert_eq!(summary.format, "jpeg");
        assert_eq!(summary.source_sha256, crate::artifact::sha256(&encoded));
        assert!(
            matches!(&summary.conversion, FeatureState::Present(identity) if identity.version == "zune-jpeg-0.5.15/baseline-strict-rgb8-pdf-v1" && identity.invocations == 1)
        );
        let FeatureState::Present(geometry) = &summary.image_geometry else {
            panic!("missing image geometry")
        };
        assert_eq!(
            image_preparation_profile_digest("jpeg", geometry).unwrap(),
            summary.preparation_profile_sha256
        );
        let pdf = std::fs::read(prepared.prepared_path()).unwrap();
        assert!(
            embedded_rgb(&pdf, 13, 9)
                .chunks_exact(3)
                .all(|pixel| pixel == expected)
        );
        assert_eq!(budget.staging_used(), 13 * 9 * 3 + 16_384);
        prepared.verify(&budget).unwrap();
        std::fs::write(&source, b"changed").unwrap();
        assert!(prepared.verify(&budget).is_err());
    }
}

#[tokio::test]
async fn strict_jpeg_buffer_output_and_invocation_limits_fail_before_entropy_or_staging() {
    let mut encoded = crate::image_prepare::jpeg_strict::tests::fixture(13, 9, &[-1024, 0, 1016]);
    let sos = encoded.windows(2).position(|b| b == [0xff, 0xda]).unwrap();
    let length = usize::from(u16::from_be_bytes([encoded[sos + 2], encoded[sos + 3]]));
    encoded.truncate(sos + 2 + length);
    encoded.extend_from_slice(&[0xff, 0xd9]);
    for (field, expected) in [
        ("decoded", "image exceeds"),
        ("working", "working admission"),
        ("output", "PDF output admission"),
        ("run", "shared run staging admission"),
    ] {
        let mut budget = ResourceBudget::default();
        match field {
            "decoded" => budget.image_pixel_buffer_bytes = 13 * 9 * 4 - 1,
            "working" => {
                budget.owned_working_bytes =
                    encoded.len() as u64 + 13 * 9 * 3 + 16 * 3 * 16 + 3 * 13 + 1_048_576 - 1
            }
            "output" => budget.document_staging_bytes = 13 * 9 * 3 + 16_384 - 1,
            "run" => budget.run_staging_bytes = 13 * 9 * 3 + 16_384 - 1,
            _ => unreachable!(),
        }
        // Entropy rejection intentionally precedes reservation. Use valid
        // entropy for the shared-counter case; it must fail before decode/output.
        let valid = crate::image_prepare::jpeg_strict::tests::fixture(13, 9, &[-1024, 0, 1016]);
        let input = if field == "run" { &valid } else { &encoded };
        let (error, staging) = prepare_error(input, "jpeg", &budget).await;
        assert!(error.contains(expected), "{field}: {error}");
        assert!(!staging, "{field}");
        assert_eq!(budget.staging_used(), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("shared.jpg");
    let encoded = crate::image_prepare::jpeg_strict::tests::fixture(13, 9, &[-1024, 0, 1016]);
    std::fs::write(&source, &encoded).unwrap();
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 2 * (13 * 9 * 3 + 16_384) - 1;
    let first = prepare_document(
        &source,
        &dir.path().join("one"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let error = prepare_document(
        &source,
        &dir.path().join("two"),
        &PreparationOptions,
        &budget.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("shared run staging admission"));
    assert!(!dir.path().join("two").exists());
    assert_eq!(budget.staging_used(), 13 * 9 * 3 + 16_384);
    first.verify(&budget).unwrap();
    assert!(first.verify(&ResourceBudget::default()).is_err());
}

#[tokio::test]
async fn strict_jpeg_recipe_rejects_unsupported_source_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.jpeg");
    std::fs::write(
        &source,
        crate::image_prepare::jpeg_strict::tests::fixture(8, 8, &[0]),
    )
    .unwrap();
    let prepared = prepare_document(
        &source,
        &dir.path().join("stage"),
        &PreparationOptions,
        &ResourceBudget::default(),
    )
    .await
    .unwrap();
    let FeatureState::Present(geometry) = &prepared.summary().image_geometry else {
        panic!("missing geometry")
    };
    let png_profile = image_preparation_profile_digest("png", geometry).unwrap();
    assert_ne!(png_profile, prepared.summary().preparation_profile_sha256);
    let mut forged = geometry.clone();
    forged.source_color_profile = FeatureState::Unsupported("not admitted".into());
    assert!(image_preparation_profile_digest("jpeg", &forged).is_err());
    forged = geometry.clone();
    forged.source_orientation = FeatureState::Unsupported("not admitted".into());
    assert!(image_preparation_profile_digest("jpeg", &forged).is_err());
    assert!(image_preparation_profile_digest("jpg", geometry).is_err());
}
