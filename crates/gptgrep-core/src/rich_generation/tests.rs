use super::*;
use gptgrep_pageindex::{PageInput, from_pages};
use gptgrep_parse::{AffineTransform, FeatureState, PageGeometry, RICH_PARSER};
use serde_json::json;

fn baseline_gray_jpeg() -> Result<Vec<u8>> {
    use image::{ExtendedColorType, ImageEncoder};
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100).write_image(
        &[128; 13 * 9],
        13,
        9,
        ExtendedColorType::L8,
    )?;
    Ok(bytes)
}

async fn prepare_strict_jpeg(
    root: &Path,
    budget: &ResourceBudget,
) -> Result<gptgrep_parse::PreparedDocument> {
    fs::write(root.join("source.jpeg"), baseline_gray_jpeg()?)?;
    gptgrep_parse::prepare_document(
        &root.join("source.jpeg"),
        &root.join("jpeg-staging"),
        &gptgrep_parse::PreparationOptions,
        budget,
    )
    .await
}

async fn extract_strict_jpeg(
    prepared: &gptgrep_parse::PreparedDocument,
    budget: &ResourceBudget,
) -> Result<gptgrep_parse::NativeCandidate> {
    gptgrep_parse::extract_native(
        prepared,
        &gptgrep_parse::NativeOptions {
            dpi: 72.0,
            ..Default::default()
        },
        budget,
    )
    .await
}

fn declared_offline_ocr(task: &gptgrep_parse::OcrTask) -> Result<gptgrep_parse::OcrPageResult> {
    use gptgrep_parse::*;
    // This mirrors the existing offline OCR DTO fixture. No model, provider,
    // image delivery, usage, or OCR accuracy is qualified by declared receipts.
    Ok(OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: task
            .regions
            .iter()
            .enumerate()
            .map(|(i, region)| OcrRegionResult {
                id: region.id.clone(),
                bbox: region.bbox,
                text: "Declared offline JPEG OCR text".into(),
                reading_order: i as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(i, image)| OcrImageDelivery {
                    artifact_id: image.artifact.id.clone(),
                    artifact_sha256: image.artifact.sha256.clone(),
                    tool_ordinal: i as u32 + 1,
                    mime: image.artifact.mime.clone(),
                    width: image.width,
                    height: image.height,
                    tool_response_sha256: "4".repeat(64),
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    })
}

#[tokio::test]
async fn strict_jpeg_structural_publication_keeps_native_and_offline_ocr_distinct() -> Result<()> {
    use gptgrep_parse::*;
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let budget = ResourceBudget::default();
    let prepared = prepare_strict_jpeg(&root, &budget).await?;
    assert_eq!(prepared.summary().format, "jpeg");
    assert!(
        matches!(&prepared.summary().conversion, FeatureState::Present(conversion)
        if conversion.converter == "gptgrep-bounded-image-pdf"
        && conversion.version == "zune-jpeg-0.5.15/baseline-strict-rgb8-pdf-v1"
        && conversion.invocations == 1)
    );
    let FeatureState::Present(geometry) = &prepared.summary().image_geometry else {
        panic!("JPEG geometry must be retained")
    };
    assert_eq!(
        (geometry.source_width_pixels, geometry.source_height_pixels),
        (13, 9)
    );
    assert_eq!(geometry.source_pixels_to_page, AffineTransform::IDENTITY);
    assert!(matches!(geometry.source_orientation, FeatureState::Absent));
    assert!(matches!(
        geometry.source_color_profile,
        FeatureState::Absent
    ));
    assert_eq!(
        prepared.summary().preparation_profile_sha256,
        image_preparation_profile_digest("jpeg", geometry)?
    );
    let candidate = extract_strict_jpeg(&prepared, &budget).await?;
    assert_eq!(candidate.pages().len(), 1);
    assert_eq!(candidate.pages()[0].coverage, CoverageState::RequiredOcr);
    assert_eq!(candidate.pages()[0].native_text.bytes, 0);
    assert_eq!(candidate.plan().tasks.len(), 1);
    let result = declared_offline_ocr(&candidate.plan().tasks[0])?;
    let parsed = finalize_document(candidate, vec![result], OcrMode::Auto, &budget)?;
    assert_eq!(parsed.pages[0].coverage, CoverageState::ValidatedOcr);
    let staged = budget.staging_used();
    let mut writer = RichGenerationWriter::begin(&root, budget.clone())?;
    writer.add_document("source.jpeg", prepared.staging_root(), parsed)?;
    assert!(
        budget.staging_used() > staged,
        "retained copy shares the invocation counter"
    );
    let pin = writer.publish()?;
    let reader = RichGenerationReader::open_pinned(&root, &pin)?;
    reader.verify_all()?;
    assert_eq!(
        reader.manifest()?.documents[0].pages[0].native_text.bytes,
        0
    );
    let hits = reader.search_artifacts("Declared", false, true, 1, None, MAX_OUTPUT_BYTES)?;
    assert_eq!(hits.hits.len(), 1);
    assert!(matches!(hits.hits[0].evidence, EvidenceRef::OcrText { .. }));
    reader.validate_text_evidence(&hits.hits[0].evidence)?;
    Ok(())
}

#[tokio::test]
async fn strict_jpeg_without_required_ocr_preserves_prior_current() -> Result<()> {
    use gptgrep_parse::*;
    for mode in [OcrMode::Auto, OcrMode::Off] {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let prior = publish(&root, &["prior publication\n"])?;
        let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
        let budget = ResourceBudget::default();
        let prepared = prepare_strict_jpeg(&root, &budget).await?;
        let candidate = extract_strict_jpeg(&prepared, &budget).await?;
        assert_eq!(candidate.pages()[0].coverage, CoverageState::RequiredOcr);
        // A raster-only source has no native searchable text. Auto refuses
        // required coverage; Off refuses the no-content document earlier.
        // No completed DTO is fabricated to bypass either refusal.
        let error = finalize_document(candidate, vec![], mode, &budget)
            .expect_err("raster-only JPEG cannot finalize without OCR");
        let message = match mode {
            OcrMode::Auto => "required OCR coverage incomplete",
            OcrMode::Off => "no searchable text extracted",
        };
        assert!(error.to_string().contains(message), "{error:#}");
        assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
        assert_eq!(RichGenerationReader::open(&root)?.pin(), prior);
    }
    Ok(())
}

#[tokio::test]
async fn incomplete_jpeg_preparation_preserves_prior_current() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let prior = publish(&root, &["prior publication\n"])?;
    let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
    let encoded = baseline_gray_jpeg()?;
    let sos = encoded
        .windows(2)
        .position(|bytes| bytes == [0xff, 0xda])
        .unwrap();
    let length = u16::from_be_bytes([encoded[sos + 2], encoded[sos + 3]]) as usize;
    let mut incomplete = encoded[..sos + 2 + length].to_vec();
    incomplete.extend_from_slice(&[0xff, 0xd9]);
    fs::write(root.join("source.jpeg"), incomplete)?;
    let budget = ResourceBudget::default();
    let error = gptgrep_parse::prepare_document(
        &root.join("source.jpeg"),
        &root.join("jpeg-staging"),
        &gptgrep_parse::PreparationOptions,
        &budget,
    )
    .await
    .expect_err("missing coded entropy must fail preparation");
    assert!(error.to_string().contains("premature entropy marker"));
    assert_eq!(budget.staging_used(), 0);
    assert!(!root.join("jpeg-staging").exists());
    assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
    assert_eq!(RichGenerationReader::open(&root)?.pin(), prior);
    Ok(())
}

#[tokio::test]
async fn strict_jpeg_source_changes_refuse_prepare_import_and_reader_evidence() -> Result<()> {
    use gptgrep_parse::*;
    for boundary in ["after_prepare", "after_import", "after_publish"] {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let prior = publish(&root, &["prior publication\n"])?;
        let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
        let budget = ResourceBudget::default();
        let prepared = prepare_strict_jpeg(&root, &budget).await?;
        if boundary == "after_prepare" {
            fs::write(root.join("source.jpeg"), b"changed original source")?;
            assert!(extract_strict_jpeg(&prepared, &budget).await.is_err());
            assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
            assert_eq!(RichGenerationReader::open(&root)?.pin(), prior);
            continue;
        }
        let candidate = extract_strict_jpeg(&prepared, &budget).await?;
        let result = declared_offline_ocr(&candidate.plan().tasks[0])?;
        let parsed = finalize_document(candidate, vec![result], OcrMode::Auto, &budget)?;
        let mut writer = RichGenerationWriter::begin(&root, budget)?;
        writer.add_document("source.jpeg", prepared.staging_root(), parsed)?;
        if boundary == "after_import" {
            fs::write(root.join("source.jpeg"), b"changed original source")?;
            assert!(writer.publish().is_err());
            assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
            assert_eq!(RichGenerationReader::open(&root)?.pin(), prior);
            continue;
        }
        let pin = writer.publish()?;
        let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
        let reader = RichGenerationReader::open_pinned(&root, &pin)?;
        let hits = reader.search_artifacts("Declared", false, true, 1, None, MAX_OUTPUT_BYTES)?;
        let EvidenceRef::OcrText { binding, .. } = &hits.hits[0].evidence else {
            panic!("declared OCR artifact expected")
        };
        assert!(
            reader
                .read_text(&binding.artifact_id, 0, 128)?
                .text
                .contains("Declared")
        );
        fs::write(root.join("source.jpeg"), b"changed original source")?;
        assert!(reader.read_text(&binding.artifact_id, 0, 128).is_err());
        let stale = reader.search_artifacts("Declared", false, true, 1, None, MAX_OUTPUT_BYTES)?;
        assert!(stale.hits.is_empty());
        assert_eq!(stale.stale_documents, ["source.jpeg"]);
        assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
    }
    Ok(())
}

// These fixtures exercise persistence and lineage with synthetic parser DTOs.
// They do not claim native PDF parsing or OCR/model accuracy.
fn candidate(root: &Path, texts: &[&str]) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = b"synthetic source bytes for artifact custody tests";
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = digest(source);
    let profile = digest(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create =
        |kind: ArtifactKind, page: Option<u32>, bytes: &[u8], name: &str| -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &digest(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: digest(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".txt") {
                    "text/plain; charset=utf-8"
                } else {
                    "application/json"
                }
                .into(),
                page,
                region_id: None,
            };
            fs::write(staging_path.join(name), bytes)?;
            artifacts.push(artifact.clone());
            Ok(artifact)
        };
    for (index, text) in texts.iter().enumerate() {
        let number = index as u32 + 1;
        let native_text = create(
            ArtifactKind::NativeText,
            Some(number),
            text.as_bytes(),
            &format!("p{number}-native.txt"),
        )?;
        let geometry = PageGeometry {
            width_points: 100.0,
            height_points: 200.0,
            rotation_quarter_turns: 0,
            user_unit: 1.0,
            source_to_page: AffineTransform::IDENTITY,
            page_to_source: AffineTransform::IDENTITY,
        };
        let complexity = serde_json::from_value(
            json!({"page_number":number,"text_length":text.len(),"text_coverage":0.5,
            "has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,
            "full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":20000.0,
            "needs_ocr":false,"reasons":[],"layout":null}),
        )?;
        let capabilities =
            BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
        let metadata_artifact = create(
            ArtifactKind::NativeMetadata,
            Some(number),
            &serde_json::to_vec(
                &json!({"page":{"page_number":number,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
                "capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]}),
            )?,
            &format!("p{number}-native.json"),
        )?;
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        });
    }
    let inputs = texts
        .iter()
        .enumerate()
        .map(|(i, text)| PageInput {
            number: i as u32 + 1,
            text: (*text).into(),
        })
        .collect::<Vec<_>>();
    let document = from_pages("synthetic", &inputs, &[], &[], RICH_PARSER)?;
    create(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
    )?;
    create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
    Ok((
        staging,
        RichParsedDocument {
            summary: PreparedSummary {
                source_sha256: sha.clone(),
                prepared_sha256: sha,
                source_bytes: source.len() as u64,
                prepared_bytes: source.len() as u64,
                format: "pdf".into(),
                parser_identity: RICH_PARSER.into(),
                preparation_profile_sha256: digest(&serde_json::to_vec(&(
                    "gptgrep.prepare.identity-pdf.v1",
                    gptgrep_parse::LITEPARSE_REVISION,
                ))?),
                conversion: FeatureState::Present(gptgrep_parse::ConversionIdentity {
                    converter: "identity-pdf".into(),
                    version: gptgrep_parse::LITEPARSE_REVISION.into(),
                    invocations: 0,
                }),
                image_geometry: FeatureState::Absent,
            },
            document,
            pages,
            artifacts,
            native_profile_sha256: profile,
            segments: vec![],
            ocr_tasks: vec![],
            ocr_results: vec![],
            coverage: texts
                .iter()
                .enumerate()
                .map(|(i, _)| (i as u32 + 1, CoverageState::Native))
                .collect(),
            unmapped_merged_lines: texts.len(),
        },
    ))
}

fn publish(root: &Path, texts: &[&str]) -> Result<GenerationPin> {
    let (staging, parsed) = candidate(root, texts)?;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()
}

#[tokio::test]
async fn retained_legacy_generation_requests_explicit_rich_rebuild() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    fs::write(root.join("source.md"), "# Existing text\nlegacy evidence\n")?;
    crate::index(&root, 10).await?;
    let pointer: Pointer =
        serde_json::from_slice(&fs::read(root.join(STATE).join("CURRENT.json"))?)?;
    let pin = GenerationPin {
        generation: pointer.generation,
        manifest_sha256: pointer.manifest_sha256,
    };
    crate::GenerationReader::open_pinned(&root, &pin)?;
    for result in [
        RichGenerationReader::open(&root),
        RichGenerationReader::open_pinned(&root, &pin),
    ] {
        let error = result.err().expect("legacy rich reader requires rebuild");
        assert!(error.to_string().contains("rebuild_required"));
    }
    Ok(())
}

#[test]
fn native_custody_validation_rejects_forged_evidence_and_nonprogressing_utf8() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["证据 needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let id = &reader.manifest()?.documents[0].pages[0].native_text.id;
    assert!(reader.read_text(id, 0, 1).is_err());
    let window = reader.read_text(id, 0, 32)?;
    reader.validate_text_evidence(&window.evidence)?;
    let mut value = serde_json::to_value(&window.evidence)?;
    for field in ["page", "line", "byte_start"] {
        let mut bad = value.clone();
        bad["binding"][field] = json!(999);
        assert!(
            reader
                .validate_text_evidence(&serde_json::from_value(bad)?)
                .is_err()
        );
    }
    value["binding"]["source_sha256"] = json!("1".repeat(64));
    assert!(
        reader
            .validate_text_evidence(&serde_json::from_value(value)?)
            .is_err()
    );
    Ok(())
}

#[test]
fn whole_document_limits_coverage_and_affine_frames_are_bound() -> Result<()> {
    for variant in 0..10 {
        let root = tempfile::tempdir()?;
        let root = root.path().canonicalize()?;
        let (stage, mut parsed) = candidate(&root, &["same\n", "same\n"])?;
        let mut budget = ResourceBudget::default();
        match variant {
            0 => parsed.coverage[0].1 = CoverageState::BlankVerified,
            1 => parsed.pages[0].complexity.needs_ocr = true,
            2 => {
                parsed.pages[0].geometry.page_to_source = AffineTransform {
                    a: 3.0,
                    b: 0.0,
                    c: -1.0,
                    d: 1.0,
                    e: 0.0,
                    f: 0.0,
                }
            }
            3 => {
                parsed.artifacts[0].mime = "application/json".into();
                parsed.pages[0].native_text.mime = "application/json".into();
            }
            4 => budget.document_staging_bytes = parsed.artifacts.iter().map(|a| a.bytes).sum(),
            5 => parsed.segments.push(SegmentMap {
                page: 2,
                merged_start: 0,
                merged_end: 4,
                artifact_id: parsed.pages[1].native_text.id.clone(),
                artifact_start: 0,
                artifact_end: 4,
                kind: ArtifactKind::NativeText,
            }),
            6 => {
                let extra = parsed.artifacts[0].clone();
                parsed.artifacts.push(extra);
            }
            7 => parsed.unmapped_merged_lines = 0,
            8 => parsed.pages[0].complexity.text_length += 1,
            _ => {
                // A mutually consistent replacement frame still differs from
                // the original geometry saved by the native parser.
                parsed.pages[0].width_points = 300.0;
                parsed.pages[0].geometry.width_points = 300.0;
            }
        }
        let mut writer = RichGenerationWriter::begin(&root, budget)?;
        assert!(
            writer
                .add_document("source.pdf", &stage.path().canonicalize()?, parsed)
                .is_err(),
            "variant{variant}"
        );
        assert!(!root.join(STATE).join("CURRENT.json").exists());
    }
    Ok(())
}

// Original synthetic PDF: vector with a tiny caption needs OCR, dense text on the
// second stays native. No task/gold/benchmark source is reused here.
fn native_pdf_fixture() -> Vec<u8> {
    let body = format!("BT /F1 8 Tf 20 180 Td {} ET", "(Native evidence page with ordinary printable text for parsing and source custody.) Tj 0 -12 Td ".repeat(12));
    let vector = "0 0 1 rg 20 20 80 80 re f BT /F1 8 Tf 20 180 Td (Caption) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{vector}\nendstream", vector.len()),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()));
    for offset in offsets.iter().skip(1) {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len()
    ));
    pdf.into_bytes()
}

#[tokio::test]
async fn real_native_pdf_and_offline_ocr_preserve_raw_artifact_citations() -> Result<()> {
    use gptgrep_parse::*;
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    fs::write(root.join("source.pdf"), native_pdf_fixture())?;
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &root.join("source.pdf"),
        &root.join("staging"),
        &PreparationOptions,
        &budget,
    )
    .await?;
    let candidate = extract_native(&prepared, &NativeOptions::default(), &budget).await?;
    assert_eq!(
        candidate.plan().tasks.len(),
        1,
        "{:?}",
        candidate
            .pages()
            .iter()
            .map(|p| (
                p.number,
                &p.coverage,
                &p.complexity.reasons,
                p.complexity.text_length
            ))
            .collect::<Vec<_>>()
    );
    let task = &candidate.plan().tasks[0];
    let raw = "Raw\r\n\t图像 label  ";
    // Declared fake receipts test offline DTO custody only, not actual image delivery.
    let result = OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: task
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| OcrRegionResult {
                id: r.id.clone(),
                bbox: r.bbox,
                text: raw.into(),
                reading_order: i as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(i, img)| OcrImageDelivery {
                    artifact_id: img.artifact.id.clone(),
                    artifact_sha256: img.artifact.sha256.clone(),
                    tool_ordinal: i as u32 + 1,
                    mime: img.artifact.mime.clone(),
                    width: img.width,
                    height: img.height,
                    tool_response_sha256: "4".repeat(64),
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    };
    let parsed = finalize_document(candidate, vec![result], OcrMode::Auto, &budget)?;
    assert!(parsed.pages[0].native_text.bytes > 0);
    for variant in 0..5 {
        let mut incomplete = parsed.clone();
        let task = &mut incomplete.ocr_tasks[0];
        match variant {
            0 => task.regions[0].bbox.width /= 2.0,
            1 => task.regions[0].required = false,
            2 => {
                let mut extra = task.regions[0].clone();
                extra.id = "extra".into();
                task.regions.push(extra);
            }
            3 => task.layout_context = "unplanned context".into(),
            _ => task.regions[0].reason = "replacement reason".into(),
        }
        let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
        let error = writer
            .add_document("source.pdf", prepared.staging_root(), incomplete)
            .expect_err("original full-page OCR coverage must be retained");
        assert!(
            error
                .to_string()
                .contains("original required full-page coverage")
        );
        assert!(!root.join(STATE).join("CURRENT.json").exists());
    }
    let mut writer = RichGenerationWriter::begin(&root, budget)?;
    writer.add_document("source.pdf", prepared.staging_root(), parsed)?;
    let pin = writer.publish()?;
    let reader = RichGenerationReader::open_pinned(&root, &pin)?;
    reader.verify_all()?;
    let hits = reader.search_artifacts("图像", false, true, 1, None, MAX_OUTPUT_BYTES)?;
    assert_eq!(hits.hits.len(), 1);
    let EvidenceRef::OcrText {
        binding,
        region_ids,
        ..
    } = &hits.hits[0].evidence
    else {
        panic!("OCR artifact expected")
    };
    assert_eq!(binding.page, 1);
    assert!(!region_ids.is_empty());
    reader.validate_text_evidence(&hits.hits[0].evidence)?;
    let retained = reader.read_text(&binding.artifact_id, 0, 1024)?;
    assert_eq!(retained.text, format!("{raw}\n"));
    let mut forged = serde_json::to_value(&hits.hits[0].evidence)?;
    forged["ocr_receipt_sha256"] = json!("5".repeat(64));
    assert!(
        reader
            .validate_text_evidence(&serde_json::from_value(forged)?)
            .is_err()
    );
    assert!(
        reader
            .search_artifacts(
                "label(?s:.*)Native evidence",
                false,
                false,
                1,
                None,
                MAX_OUTPUT_BYTES
            )?
            .hits
            .is_empty()
    );
    Ok(())
}

#[test]
fn artifact_boundaries_and_retained_pin() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["left needle\n", "right needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    assert_eq!(reader.pin(), pin);
    reader.verify_all()?;
    let result = reader.search_artifacts("needle", false, true, 2, None, MAX_OUTPUT_BYTES)?;
    assert_eq!(result.hits.len(), 2);
    let cross =
        reader.search_artifacts("left(?s:.*)right", false, false, 2, None, MAX_OUTPUT_BYTES)?;
    assert!(
        cross.hits.is_empty(),
        "regex must not span physical-page artifacts"
    );
    for hit in &result.hits {
        let EvidenceRef::NativeText { binding } = &hit.evidence else {
            panic!("expected native evidence")
        };
        assert_eq!(binding.generation, pin);
        let text = reader.read_text(&binding.artifact_id, binding.byte_start, 100)?;
        assert!(text.text.starts_with(&hit.text));
    }
    let newer = publish(&root, &["new view\n"])?;
    assert_ne!(pin, newer);
    assert_eq!(
        reader
            .search_artifacts("needle", false, true, 2, None, MAX_OUTPUT_BYTES)?
            .hits
            .len(),
        2
    );
    fs::write(root.join(STATE).join("CURRENT.json"), b"broken")?;
    assert_eq!(RichGenerationReader::open_pinned(&root, &pin)?.pin(), pin);
    Ok(())
}

#[test]
fn required_ocr_and_failed_import_cannot_publish() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["prior\n"])?;
    let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
    let (staging, mut parsed) = candidate(&root, &["candidate\n"])?;
    parsed.pages[0].coverage = CoverageState::RequiredOcr;
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    assert!(
        writer
            .add_document("source.pdf", &staging.path().canonicalize()?, parsed)
            .is_err()
    );
    assert!(writer.publish().is_err());
    assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
    assert_eq!(RichGenerationReader::open(&root)?.pin(), pin);
    Ok(())
}

#[test]
fn exact_segments_and_duplicate_ids_are_enforced() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (staging, mut parsed) = candidate(&root, &["abc\n"])?;
    parsed.segments.push(SegmentMap {
        page: 1,
        merged_start: 0,
        merged_end: 3,
        artifact_id: parsed.pages[0].native_text.id.clone(),
        artifact_start: 0,
        artifact_end: 2,
        kind: ArtifactKind::NativeText,
    });
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    assert!(
        writer
            .add_document("source.pdf", &staging.path().canonicalize()?, parsed)
            .is_err()
    );
    drop(writer);
    let (staging, mut parsed) = candidate(&root, &["abc\n"])?;
    parsed.artifacts.push(parsed.artifacts[0].clone());
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    assert!(
        writer
            .add_document("source.pdf", &staging.path().canonicalize()?, parsed)
            .is_err()
    );
    assert!(writer.publish().is_err());
    Ok(())
}

#[test]
fn repeated_page_content_has_distinct_artifact_identity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["same\n", "same\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let doc = &reader.manifest()?.documents[0];
    assert_ne!(doc.pages[0].native_text.id, doc.pages[1].native_text.id);
    assert_eq!(
        doc.pages[0].native_text.sha256,
        doc.pages[1].native_text.sha256
    );
    Ok(())
}

#[test]
fn source_and_artifact_tampering_refuse_evidence() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open_pinned(&root, &pin)?;
    let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
    fs::write(
        root.join(STATE)
            .join("generations")
            .join(&pin.generation)
            .join(&artifact.relative_path),
        "tamper\n",
    )?;
    assert!(reader.read_text(&artifact.id, 0, 100).is_err());
    assert!(
        reader
            .search_artifacts("needle", false, true, 1, None, MAX_OUTPUT_BYTES)
            .is_err()
    );
    let pin = publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open_pinned(&root, &pin)?;
    let id = reader.manifest()?.documents[0].pages[0]
        .native_text
        .id
        .clone();
    fs::write(root.join("source.pdf"), "changed source")?;
    assert!(reader.read_text(&id, 0, 100).is_err());
    let search = reader.search_artifacts("needle", false, true, 1, None, MAX_OUTPUT_BYTES)?;
    assert!(search.hits.is_empty());
    assert_eq!(search.stale_documents, ["source.pdf"]);
    Ok(())
}

#[test]
fn source_changes_after_import_preserve_current() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["old\n"])?;
    let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
    let (staging, parsed) = candidate(&root, &["new\n"])?;
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    fs::write(root.join("source.pdf"), "mutation after OCR/import")?;
    assert!(writer.publish().is_err());
    assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
    Ok(())
}

#[test]
fn serialization_and_staging_caps_are_admission_limits() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (staging, parsed) = candidate(&root, &["needle\n"])?;
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 1;
    let observed = budget.clone();
    let mut writer = RichGenerationWriter::begin(&root, budget)?;
    assert!(
        writer
            .add_document("source.pdf", &staging.path().canonicalize()?, parsed)
            .is_err()
    );
    assert_eq!(observed.staging_used(), 0);
    drop(writer);
    publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    assert!(
        reader
            .search_artifacts("needle", false, true, 1, None, 100)
            .is_err()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn generation_and_artifact_ancestor_symlinks_are_rejected() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let id = reader.manifest()?.documents[0].pages[0]
        .native_text
        .id
        .clone();
    let dir = root.join(STATE).join("generations").join(&pin.generation);
    fs::rename(dir.join("artifact-text"), dir.join("text-moved"))?;
    std::os::unix::fs::symlink("text-moved", dir.join("artifact-text"))?;
    assert!(reader.read_text(&id, 0, 100).is_err());
    assert!(reader.verify_all().is_err());
    Ok(())
}

#[test]
fn rich_cache_warm_clone_revalidates_all_bound_bytes_and_counts_exact_windows() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["alpha β needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = &reader.manifest.documents[0].pages[0].native_text;
    let manifest_bytes = fs::metadata(reader.dir.join("manifest.json"))?.len();
    let source_bytes = reader.manifest.documents[0].prepared.source_bytes;
    let before = reader.read_metrics()?;
    let first = reader.read_text(&artifact.id, 0, 5)?;
    let cold = reader.read_metrics()?.since(&before)?;
    assert_eq!(first.text, "alpha");
    assert_eq!(cold.logical_reader_bytes, 5);
    assert_eq!(cold.cached_logical_bytes, 0);
    assert_eq!(cold.cache_misses, 1);
    let expected = 2 * manifest_bytes + 2 * source_bytes + 2 * artifact.bytes;
    assert_eq!(cold.observed_rust_read_bytes, expected);
    assert_eq!(cold.observed_hash_bytes, expected);
    let clone = reader.clone();
    assert!(Arc::ptr_eq(&reader.manifest, &clone.manifest));
    assert!(Arc::ptr_eq(&reader.index, &clone.index));
    let before = reader.read_metrics()?;
    let second = clone.read_text(&artifact.id, 0, 5)?;
    let warm = reader.read_metrics()?.since(&before)?;
    assert_eq!(serde_json::to_value(first)?, serde_json::to_value(second)?);
    assert_eq!(warm.logical_reader_bytes, 5);
    assert_eq!(warm.cached_logical_bytes, 5);
    assert_eq!(warm.cache_hits, 1);
    assert_eq!(warm.observed_rust_read_bytes, expected);
    assert_eq!(warm.observed_hash_bytes, expected);
    let before = reader.read_metrics()?;
    reader.validate_text_evidence(&clone.read_text(&artifact.id, 0, 5)?.evidence)?;
    assert_eq!(
        reader.read_metrics()?.since(&before)?.logical_reader_bytes,
        5
    );
    Ok(())
}

#[test]
fn rich_cache_tighter_caps_fallback_and_live_pins_never_reset_capacity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["alpha needle\n", "bravo needle\n"])?;
    let budget = ReaderCacheBudget {
        max_bytes: 13,
        max_entries: 1,
        strict_cache_only: false,
    };
    let reader = RichGenerationReader::open_with_cache_budget(&root, &budget)?;
    let pages = &reader.manifest.documents[0].pages;
    let pin = reader.text_cached(&pages[0].native_text)?;
    reader.clone().clear_cache()?;
    assert_eq!(reader.cache_status()?["resident_bytes"], 0);
    assert_eq!(reader.cache_status()?["retained_bytes"], 13);
    let before = reader.read_metrics()?;
    let window = reader.clone().read_text(&pages[1].native_text.id, 0, 5)?;
    assert_eq!(window.text, "bravo");
    let fallback = reader.read_metrics()?.since(&before)?;
    assert_eq!(fallback.logical_reader_bytes, 5);
    assert_eq!(fallback.uncached_logical_bytes, 5);
    assert_eq!(fallback.cache_fallbacks, 1);
    assert_eq!(fallback.uncached_load_bytes, 13);
    assert_eq!(reader.cache_status()?["retained_entries"], 1);
    drop(pin);
    assert_eq!(
        reader.text_cached(&pages[1].native_text)?.origin,
        ReadOrigin::Loaded
    );
    let strict = RichGenerationReader::open_with_cache_budget(
        &root,
        &ReaderCacheBudget {
            max_bytes: 4,
            max_entries: 1,
            strict_cache_only: true,
        },
    )?;
    let before = strict.read_metrics()?;
    assert!(strict.text_cached(&pages[0].native_text).is_err());
    let failed = strict.read_metrics()?.since(&before)?;
    // Preserved pre-load digest validation is streaming, not an owned load.
    assert_eq!(failed.observed_rust_read_bytes, 13);
    assert_eq!(failed.observed_hash_bytes, 13);
    assert_eq!(failed.uncached_load_bytes, 0);
    assert_eq!(strict.cache_status()?["retained_bytes"], 0);
    Ok(())
}

#[test]
fn rich_cache_warm_pin_does_not_follow_current_or_hide_mutation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["needle original\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = &reader.manifest.documents[0].pages[0].native_text;
    reader.read_text(&artifact.id, 0, 5)?;
    let newer = publish(&root, &["newer contents\n"])?;
    assert_ne!(newer, pin);
    let before = reader.read_metrics()?;
    assert_eq!(reader.clone().read_text(&artifact.id, 0, 5)?.text, "needl");
    assert_eq!(reader.read_metrics()?.since(&before)?.cache_hits, 1);
    assert_eq!(reader.pin(), pin);
    let path = reader.dir.join(&artifact.relative_path);
    fs::write(&path, "needle tampered\n")?;
    let before = reader.read_metrics()?;
    assert!(reader.read_text(&artifact.id, 0, 5).is_err());
    let failed = reader.read_metrics()?.since(&before)?;
    assert_eq!(failed.logical_reader_bytes, 0);
    assert_eq!(failed.cache_hits, 0); // Current bytes fail before cached delivery.
    assert!(failed.observed_rust_read_bytes >= 16);
    assert!(failed.observed_hash_bytes >= 16);
    fs::write(path, "needle original\n")?;
    fs::write(root.join("source.pdf"), "changed source")?;
    let before = reader.read_metrics()?;
    assert!(reader.read_text(&artifact.id, 0, 5).is_err());
    let failed = reader.read_metrics()?.since(&before)?;
    assert_eq!(failed.logical_reader_bytes, 0);
    assert_eq!(failed.cache_hits, 0);
    assert!(failed.observed_rust_read_bytes >= 14);
    Ok(())
}
