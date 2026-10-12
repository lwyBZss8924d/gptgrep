use super::*;
use std::collections::BTreeSet;

pub(crate) fn assert_only_prepared_and_valid_document_metadata(
    prepared: &PreparedDocument,
    budget: &ResourceBudget,
    physical_pages: u32,
) {
    let retained = std::fs::read_dir(prepared.staging_root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().any(|path| path == prepared.prepared_path()));
    let document_path = retained
        .iter()
        .find(|path| *path != prepared.prepared_path())
        .unwrap();
    assert!(
        document_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-document-native.json")
    );
    let length = std::fs::metadata(document_path).unwrap().len();
    assert!(length > 0 && length <= budget.document_metadata_bytes);
    assert_eq!(
        budget.staging_used(),
        prepared.summary().prepared_bytes + length
    );
    let bytes = std::fs::read(document_path).unwrap();
    let metadata: crate::NativeDocumentMetadata = serde_json::from_slice(&bytes).unwrap();
    crate::validate_native_document_metadata(&metadata, prepared.summary(), physical_pages, budget)
        .unwrap();
}

fn complete_result(task: &OcrTask, text: &str) -> OcrPageResult {
    OcrPageResult {
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
            .map(|(index, region)| OcrRegionResult {
                id: region.id.clone(),
                bbox: region.bbox,
                text: text.into(),
                reading_order: index as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            native_agent: None,
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile).unwrap(),
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(index, image)| OcrImageDelivery {
                    artifact_id: image.artifact.id.clone(),
                    artifact_sha256: image.artifact.sha256.clone(),
                    tool_ordinal: index as u32 + 1,
                    mime: image.artifact.mime.clone(),
                    width: image.width,
                    height: image.height,
                    tool_response_sha256: "4".repeat(64),
                    native: None,
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    }
}

pub(super) async fn fixture_candidate(
    dir: &tempfile::TempDir,
    render: bool,
    window: usize,
) -> (PreparedDocument, NativeCandidate, ResourceBudget) {
    let source = dir.path().join("source.pdf");
    std::fs::write(&source, crate::tests::pdf_fixture(true)).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &dir.path().join("staging"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let options = NativeOptions {
        render_pages: render,
        page_window: window,
        ..Default::default()
    };
    let candidate = extract_native(&prepared, &options, &budget).await.unwrap();
    (prepared, candidate, budget)
}

#[tokio::test]
async fn preparation_and_complete_projection_are_window_invariant() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (prepared, first, budget) = fixture_candidate(&a, false, 1).await;
    let (_, second, second_budget) = fixture_candidate(&b, false, 25).await;
    assert_eq!(
        first.native_profile_sha256(),
        second.native_profile_sha256()
    );
    assert_eq!(
        first.summary().source_sha256,
        first.summary().prepared_sha256
    );
    assert!(
        matches!(&prepared.summary().conversion,FeatureState::Present(identity) if identity.invocations == 0)
    );
    let ids = first
        .artifacts()
        .iter()
        .map(|artifact| &artifact.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), first.artifacts().len());
    for page in first.pages() {
        let source = (72.0, 720.0);
        let normalized = page.geometry.source_to_page.apply(source.0, source.1);
        let roundtrip = page
            .geometry
            .page_to_source
            .apply(normalized.0, normalized.1);
        assert!((roundtrip.0 - source.0).abs() < 0.001 && (roundtrip.1 - source.1).abs() < 0.001);
        assert!(matches!(
            page.capabilities["font_names"],
            FeatureState::Present(_) | FeatureState::Unsupported(_)
        ));
    }
    let first = finalize_document(first, vec![], OcrMode::Off, &budget).unwrap();
    let second = finalize_document(second, vec![], OcrMode::Off, &second_budget).unwrap();
    assert_eq!(
        serde_json::to_value(&first.document).unwrap(),
        serde_json::to_value(&second.document).unwrap()
    );
    assert!(
        first
            .document
            .text
            .contains("First page searchable evidence.")
    );
    assert!(
        first
            .document
            .text
            .contains("Second page distinct evidence.")
    );
    assert_eq!(first.unmapped_merged_lines, 0);
    let serialized = serde_json::to_string(&first).unwrap();
    assert!(!serialized.contains(a.path().to_str().unwrap()));
    assert_eq!(first.document.parser, RICH_PARSER);
}

#[tokio::test]
async fn offline_ocr_keeps_raw_region_text_and_rebuilds_complete_document_locators() {
    let directory = tempfile::tempdir().unwrap();
    let (prepared, candidate, budget) = fixture_candidate(&directory, true, 1).await;
    assert_eq!(candidate.plan().tasks.len(), 2);
    let outcomes = candidate
        .plan()
        .tasks
        .iter()
        .map(|task| {
            complete_result(
                task,
                if task.page == 1 {
                    "Revenue 2026\nAxis: quarterly units\nLegend: operating margin"
                } else {
                    "Appendix observations"
                },
            )
        })
        .collect();
    let rich = finalize_document(candidate, outcomes, OcrMode::Auto, &budget).unwrap();
    assert!(
        rich.coverage
            .iter()
            .all(|(_, state)| *state == CoverageState::ValidatedOcr)
    );
    assert_eq!(rich.ocr_tasks.len(), 2);
    let raw = rich
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == ArtifactKind::OcrText && artifact.page == Some(1))
        .unwrap();
    let raw = std::fs::read_to_string(prepared.staging_root().join(&raw.relative_path)).unwrap();
    assert_eq!(
        raw,
        "Revenue 2026\nAxis: quarterly units\nLegend: operating margin\n"
    );
    assert!(rich.document.text.contains("Axis: quarterly units"));
    assert!(rich.document.pages[1].line_start > rich.document.pages[0].line_start);
    assert_eq!(rich.unmapped_merged_lines, 0);
    for segment in &rich.segments {
        let artifact = rich
            .artifacts
            .iter()
            .find(|artifact| artifact.id == segment.artifact_id)
            .unwrap();
        let bytes = std::fs::read(prepared.staging_root().join(&artifact.relative_path)).unwrap();
        assert_eq!(
            &rich.document.text.as_bytes()
                [segment.merged_start as usize..segment.merged_end as usize],
            &bytes[segment.artifact_start as usize..segment.artifact_end as usize]
        );
    }
    for node in &rich.document.nodes {
        assert!(
            node.line_start <= node.line_end && node.line_end <= rich.document.text.lines().count()
        );
    }
}

#[tokio::test]
async fn incomplete_and_unbound_ocr_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let (_, candidate, budget) = fixture_candidate(&directory, true, 25).await;
    let task = &candidate.plan().tasks[0];
    let complete = complete_result(task, "visible evidence");
    validate_ocr_result(task, &complete).unwrap();
    for change in 0..8 {
        let mut invalid = complete.clone();
        match change {
            0 => invalid.status = OcrResultStatus::Partial,
            1 => invalid.regions.clear(),
            2 => invalid.task_sha256 = "0".repeat(64),
            3 => invalid.regions[0].bbox.x += 1.0,
            4 => invalid.receipt.model = "other-model".into(),
            5 => invalid.regions[0].status = OcrRegionStatus::Unknown,
            6 => invalid.receipt.image_deliveries.clear(),
            7 => invalid.receipt.image_deliveries[0].artifact_sha256 = "0".repeat(64),
            _ => unreachable!(),
        }
        assert!(
            validate_ocr_result_metadata(task, &invalid).is_err(),
            "mutation {change}"
        );
    }
    let error = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap_err();
    assert!(
        error.to_string().contains("coverage incomplete"),
        "{error:#}"
    );
}

#[tokio::test]
async fn crop_rotation_userunit_and_blank_coverage_are_explicit() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("geometry.pdf");
    let bytes = crate::tests::pdf_objects(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /CropBox [10 20 190 280] /Rotate 90 /UserUnit 2 /Contents 4 0 R >>".into(),
        "<< /Length 0 >>\nstream\n\nendstream".into(),
    ]);
    std::fs::write(&source, bytes).unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let candidate = extract_native(&prepared, &NativeOptions::default(), &budget)
        .await
        .unwrap();
    let page = &candidate.pages()[0];
    assert_eq!((page.width_points, page.height_points), (520.0, 360.0));
    assert_eq!(page.geometry.rotation_quarter_turns, 1);
    assert_eq!(page.geometry.user_unit, 2.0);
    assert_eq!(page.coverage, CoverageState::BlankVerified);
    assert!(candidate.plan().tasks.is_empty());
    validate_rendition(&page.renditions[0], &budget).unwrap();
    let normalized = page.geometry.source_to_page.apply(10.0, 20.0);
    assert!(normalized.0.abs() < 0.01 && normalized.1.abs() < 0.01);
    assert!(
        finalize_document(candidate, vec![], OcrMode::Off, &budget)
            .unwrap_err()
            .to_string()
            .contains("no searchable text")
    );
}

#[tokio::test]
async fn vectors_without_native_text_require_ocr_and_count_admission_precedes_materialization() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("vector.pdf");
    let drawing = "0 0 0 rg 10 10 50 50 re f";
    std::fs::write(
        &source,
        crate::tests::pdf_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>".into(),
            format!(
                "<< /Length {} >>\nstream\n{drawing}\nendstream",
                drawing.len()
            ),
        ]),
    )
    .unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let candidate = extract_native(&prepared, &NativeOptions::default(), &budget)
        .await
        .unwrap();
    assert_eq!(candidate.pages()[0].coverage, CoverageState::RequiredOcr);
    assert!(
        candidate.pages()[0]
            .complexity
            .reasons
            .iter()
            .any(|reason| reason.as_str() == "no-text")
    );
    let text = "Visible vector label\nAxis units";
    let result = complete_result(&candidate.plan().tasks[0], text);
    let rich = finalize_document(candidate, vec![result], OcrMode::Auto, &budget).unwrap();
    assert_eq!(rich.document.text, format!("{text}\n"));
    let mut small = ResourceBudget::default();
    small.page_layout_bytes = 18_431;
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &small,
    )
    .await
    .unwrap();
    let error = extract_native(&prepared, &NativeOptions::default(), &small)
        .await
        .unwrap_err();
    // The complete graphics collector reserves traversal storage before any
    // object materialization; this page budget rejects that earlier admission.
    assert!(
        error
            .to_string()
            .contains("graphics preallocation admission"),
        "{error:#}"
    );
    // Document metadata is independently admitted before page materialization.
    // A page-count refusal may retain that snapshot, never a page/raster output.
    assert_only_prepared_and_valid_document_metadata(&prepared, &small, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn retained_artifact_symlinks_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let (prepared, candidate, budget) = fixture_candidate(&directory, false, 25).await;
    let artifact = prepared
        .staging_root()
        .join(&candidate.pages()[0].native_text.relative_path);
    let copy = directory.path().join("external-copy");
    std::fs::copy(&artifact, &copy).unwrap();
    std::fs::remove_file(&artifact).unwrap();
    std::os::unix::fs::symlink(copy, &artifact).unwrap();
    assert!(
        finalize_document(candidate, vec![], OcrMode::Off, &budget)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[tokio::test]
async fn task_metadata_validation_is_path_free_and_headers_are_bound() {
    let directory = tempfile::tempdir().unwrap();
    let (_, candidate, _) = fixture_candidate(&directory, true, 25).await;
    let mut task = candidate.plan().tasks[0].clone();
    validate_ocr_task(&task).unwrap();
    let mut bad_header = task.clone();
    bad_header.images[0].width += 1;
    bad_header.task_sha256 = ocr_task_digest(&bad_header).unwrap();
    bad_header.id = format!("ocr-{}", bad_header.task_sha256);
    validate_ocr_task_metadata(&bad_header).unwrap();
    assert!(
        validate_ocr_task(&bad_header)
            .unwrap_err()
            .to_string()
            .contains("dimensions disagree")
    );
    std::fs::remove_file(&task.images[0].path).unwrap();
    task.source_path = directory.path().join("absent-source");
    validate_ocr_task_metadata(&task).unwrap();
    validate_ocr_result_metadata(&task, &complete_result(&task, "transcribed text")).unwrap();
    assert!(validate_ocr_task(&task).is_err());
}

#[tokio::test]
async fn source_and_prepared_mutations_invalidate_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let (prepared, candidate, budget) = fixture_candidate(&directory, false, 25).await;
    std::fs::write(prepared.source_path(), b"changed source").unwrap();
    assert!(
        finalize_document(candidate, vec![], OcrMode::Off, &budget)
            .unwrap_err()
            .to_string()
            .contains("source changed")
    );
    std::fs::write(prepared.prepared_path(), b"changed prepared").unwrap();
    assert!(
        prepared
            .verify(&budget)
            .unwrap_err()
            .to_string()
            .contains("prepared document changed")
    );
}

#[tokio::test]
async fn char_and_raster_limits_reject_before_their_output_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.pdf");
    std::fs::write(&source, crate::tests::pdf_fixture(true)).unwrap();
    let mut budget = ResourceBudget::default();
    budget.page_layout_bytes = 16_384;
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let error = extract_native(&prepared, &NativeOptions::default(), &budget)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("resource_limit: page character admission"),
        "actual character-limit refusal: {error:#}"
    );
    // Document metadata is independently admitted before page materialization.
    // A page-count refusal may retain that snapshot, never a page/raster output.
    assert_only_prepared_and_valid_document_metadata(&prepared, &budget, 2);
    let mut pixels = ResourceBudget::default();
    pixels.image_pixels = 10;
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &pixels,
    )
    .await
    .unwrap();
    assert!(
        extract_native(&prepared, &NativeOptions::default(), &pixels)
            .await
            .unwrap_err()
            .to_string()
            .contains("image exceeds admitted")
    );
    assert!(
        !std::fs::read_dir(prepared.staging_root())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "png"))
    );
}

#[tokio::test]
async fn unsupported_conversion_fails_before_preparation_and_staging_limit_is_shared() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.gif");
    std::fs::write(&source, b"not an Office fixture").unwrap();
    let root = directory.path().join("staging");
    let budget = ResourceBudget::default();
    assert!(
        prepare_document(&source, &root, &PreparationOptions, &budget)
            .await
            .unwrap_err()
            .to_string()
            .contains("capability_unavailable")
    );
    assert!(!root.exists());
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 100;
    let copy = budget.clone();
    budget.reserve_retained_staging(60).unwrap();
    assert!(copy.reserve_retained_staging(41).is_err());
    assert_eq!(budget.staging_used(), 60);
    copy.reserve_retained_staging(40).unwrap();
    assert_eq!(budget.staging_used(), 100);
    let source = directory.path().join("source.pdf");
    std::fs::write(&source, crate::tests::pdf_fixture(true)).unwrap();
    assert!(
        prepare_document(&source, &root, &PreparationOptions, &budget)
            .await
            .is_err()
    );
    assert!(!root.exists());
}

#[tokio::test]
async fn preparation_extraction_and_finalization_cannot_reset_the_invocation_budget() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.pdf");
    std::fs::write(&source, crate::tests::pdf_fixture(true)).unwrap();
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 200_000;
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let foreign = ResourceBudget::default();
    assert!(
        extract_native(&prepared, &NativeOptions::default(), &foreign)
            .await
            .unwrap_err()
            .to_string()
            .contains("different invocation")
    );
    let mut increased = budget.clone();
    increased.run_staging_bytes += 1;
    assert!(
        extract_native(&prepared, &NativeOptions::default(), &increased)
            .await
            .unwrap_err()
            .to_string()
            .contains("original admission")
    );
    let options = NativeOptions {
        render_pages: false,
        ..Default::default()
    };
    let candidate = extract_native(&prepared, &options, &budget.clone())
        .await
        .unwrap();
    assert!(
        finalize_document(candidate, vec![], OcrMode::Off, &foreign)
            .unwrap_err()
            .to_string()
            .contains("different invocation")
    );
    let prepared = prepare_document(
        &source,
        &directory.path().join("stage"),
        &PreparationOptions,
        &budget,
    )
    .await
    .unwrap();
    let mut tighter = budget.clone();
    tighter.image_pixels -= 1;
    let candidate = extract_native(&prepared, &options, &tighter).await.unwrap();
    tighter.image_pixels -= 1;
    let rich = finalize_document(candidate, vec![], OcrMode::Off, &tighter).unwrap();
    assert!(!rich.document.text.is_empty());
    assert_eq!(tighter.staging_used(), budget.staging_used());
}

#[tokio::test]
async fn visible_annotation_appearances_cannot_be_verified_blank_or_bypass_ocr() {
    for (nested, annotation_flags) in [(false, 4), (true, 4), (false, 0), (true, 0)] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("annotations.pdf");
        let drawing = "1 0 0 rg 0 0 60 60 re f";
        let appearance = if nested { "/Nested Do" } else { drawing };
        let resources = if nested {
            "<< /XObject << /Nested 10 0 R >> >>"
        } else {
            "<< >>"
        };
        let mut body = String::from("BT /F1 12 Tf 20 180 Td ");
        for row in 0..10 {
            if row > 0 {
                body.push_str("0 -15 Td ");
            }
            body.push_str(&format!("(Synthetic row {row} has evidence) Tj "));
        }
        body.push_str("ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 7 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Annots [5 0 R] /Contents 4 0 R >>".into(),
            "<< /Length 0 >>\nstream\n\nendstream".into(),
            format!("<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /F {annotation_flags} /AP << /N 6 0 R >> >>"),
            format!("<< /Type /XObject /Subtype /Form /BBox [0 0 60 60] /Resources {resources} /Length {} >>\nstream\n{appearance}\nendstream",appearance.len()),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 9 0 R >> >> /Contents 8 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{body}\nendstream",body.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            format!("<< /Type /XObject /Subtype /Form /BBox [0 0 60 60] /Resources << >> /Length {} >>\nstream\n{drawing}\nendstream",drawing.len()),
        ];
        std::fs::write(&source, crate::tests::pdf_objects(&objects)).unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &directory.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        let candidate = extract_native(&prepared, &NativeOptions::default(), &budget)
            .await
            .unwrap();
        let page = &candidate.pages()[0];
        assert_eq!(page.coverage, CoverageState::RequiredOcr);
        assert!(
            matches!(&page.capabilities["annotation_count"],FeatureState::Present(count) if count == "1")
        );
        assert_eq!(candidate.pages()[1].coverage, CoverageState::Native);
        let pixels = image::ImageReader::open(&page.renditions[0].path)
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert!(
            pixels
                .pixels()
                .filter(|pixel| pixel[0] < 250 || pixel[1] < 250 || pixel[2] < 250)
                .count()
                > 500
        );
        assert!(candidate.plan().tasks.iter().any(|task| task.page == 1));
        let error = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap_err();
        assert!(
            error.to_string().contains("coverage incomplete"),
            "{error:#}"
        );
        let prepared = prepare_document(
            &source,
            &directory.path().join("stage"),
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
        assert_eq!(candidate.pages()[0].coverage, CoverageState::RequiredOcr);
        assert!(!candidate.plan().blockers.is_empty());
        let mut clean_objects = objects;
        clean_objects[2] =
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R >>".into();
        let clean_source = directory.path().join("unannotated.pdf");
        std::fs::write(&clean_source, crate::tests::pdf_objects(&clean_objects)).unwrap();
        let prepared = prepare_document(
            &clean_source,
            &directory.path().join("stage"),
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
        assert_eq!(candidate.pages()[0].coverage, CoverageState::BlankVerified);
        assert_eq!(candidate.pages()[1].coverage, CoverageState::Native);
        assert!(candidate.plan().tasks.is_empty() && candidate.plan().blockers.is_empty());
        let rich = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap();
        assert!(rich.document.text.contains("Synthetic row 9 has evidence"));
        assert_eq!(rich.unmapped_merged_lines, 0);
    }
}

fn vector_classifier_pdf(drawing: &str, native_text: bool) -> Vec<u8> {
    let mut content = drawing.to_string();
    if native_text {
        content.push_str(" BT /F1 12 Tf 20 180 Td ");
        for row in 0..10 {
            if row > 0 {
                content.push_str("0 -15 Td ");
            }
            content.push_str(&format!(
                "(Native content row {row} keeps evidence searchable) Tj "
            ));
        }
        content.push_str("ET");
    }
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 5 0 R >> /XObject << /Im 6 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /ASCIIHexDecode /Length 9 >>\nstream\n00808000>\nendstream".into(),
    ];
    crate::tests::pdf_objects(&objects)
}

#[tokio::test]
async fn decorative_vectors_do_not_replace_usable_native_text_with_required_ocr() {
    for drawing in [
        "0 0 0 RG 1 w 10 15 m 290 15 l S",
        "0.9 g 10 10 280 25 re f 0 g",
        "0 g 10 10 4 4 re f",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("native-with-decoration.pdf");
        std::fs::write(&source, vector_classifier_pdf(drawing, true)).unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &directory.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                dpi: 72.,
                ..Default::default()
            },
            &budget,
        )
        .await
        .unwrap();
        candidate.prepared.verify(&budget).unwrap();
        let page = &candidate.pages()[0];
        assert_eq!(page.coverage, CoverageState::Native);
        assert!(!page.complexity.needs_ocr);
        assert!(page.complexity.uncovered_vector_area.unwrap() < 400.);
        assert!(candidate.plan().tasks.is_empty());
        let document = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap();
        assert!(document.document.text.contains("Native content row 9"));
        assert_eq!(document.coverage[0].1, CoverageState::Native);
    }
}

#[tokio::test]
async fn uncovered_filled_vectors_and_missing_native_text_still_require_ocr() {
    use liteparse::ocr_merge::ComplexityReason;
    let outlines = "0 g 15 10 12 12 re f 35 10 12 12 re f 55 10 12 12 re f 75 10 12 12 re f";
    for (drawing, native_text, reason) in [
        (outlines, true, ComplexityReason::VectorText),
        (outlines, false, ComplexityReason::NoText),
        (
            "q 300 0 0 200 0 0 cm /Im Do Q",
            false,
            ComplexityReason::Scanned,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("missing-native-content.pdf");
        std::fs::write(&source, vector_classifier_pdf(drawing, native_text)).unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &directory.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                dpi: 72.,
                ..Default::default()
            },
            &budget,
        )
        .await
        .unwrap();
        candidate.prepared.verify(&budget).unwrap();
        let page = &candidate.pages()[0];
        assert_eq!(page.coverage, CoverageState::RequiredOcr);
        assert!(page.complexity.reasons.contains(&reason));
        assert_eq!(candidate.plan().tasks.len(), 1);
        if native_text {
            assert!(page.complexity.uncovered_vector_area.unwrap() >= 400.);
        }
        assert!(
            finalize_document(candidate, vec![], OcrMode::Auto, &budget)
                .unwrap_err()
                .to_string()
                .contains("coverage incomplete")
        );
    }
}

#[test]
fn artifact_identity_binds_recipe_kind_page_and_source() {
    let source = "1".repeat(64);
    let profile = "2".repeat(64);
    let content = "3".repeat(64);
    let baseline = artifact_id(
        &source,
        &profile,
        &ArtifactKind::NativeText,
        Some(1),
        None,
        &content,
    )
    .unwrap();
    let other = [
        artifact_id(
            &"4".repeat(64),
            &profile,
            &ArtifactKind::NativeText,
            Some(1),
            None,
            &content,
        )
        .unwrap(),
        artifact_id(
            &source,
            &"4".repeat(64),
            &ArtifactKind::NativeText,
            Some(1),
            None,
            &content,
        )
        .unwrap(),
        artifact_id(
            &source,
            &profile,
            &ArtifactKind::OcrText,
            Some(1),
            None,
            &content,
        )
        .unwrap(),
        artifact_id(
            &source,
            &profile,
            &ArtifactKind::NativeText,
            Some(2),
            None,
            &content,
        )
        .unwrap(),
        artifact_id(
            &source,
            &profile,
            &ArtifactKind::NativeText,
            Some(1),
            Some("region"),
            &content,
        )
        .unwrap(),
    ];
    assert!(other.iter().all(|id| id != &baseline));
    assert!(
        artifact_id(
            "invalid",
            &profile,
            &ArtifactKind::NativeText,
            None,
            None,
            &content
        )
        .is_err()
    );
}

#[tokio::test]
async fn native_graphics_complete_26_page_windows_and_after_ocr_metadata_are_invariant() {
    let mut outputs = Vec::new();
    for window in [1, 25] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("fresh-whole-document.pdf");
        std::fs::write(
            &source,
            include_bytes!("../../../tests/fixtures/native-graphics-26-pages.pdf"),
        )
        .unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &dir.path().join("staging"),
            &PreparationOptions,
            &budget,
        )
        .await
        .unwrap();
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                dpi: 72.,
                render_pages: true,
                page_window: window,
            },
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(candidate.pages().len(), 26);
        assert_eq!(candidate.plan().tasks.len(), 26);
        let raw: Vec<_> = candidate
            .pages()
            .iter()
            .map(|p| {
                std::fs::read(
                    prepared
                        .staging_root()
                        .join(&p.metadata_artifact.relative_path),
                )
                .unwrap()
            })
            .collect();
        let results = candidate
            .plan()
            .tasks
            .iter()
            .map(|t| {
                complete_result(
                    t,
                    &format!("Authored offline geometry transcription page {}.", t.page),
                )
            })
            .collect();
        let final_document = finalize_document(candidate, results, OcrMode::Auto, &budget).unwrap();
        assert_eq!(final_document.coverage.len(), 26);
        assert!(
            final_document
                .coverage
                .iter()
                .all(|(_, c)| *c == CoverageState::ValidatedOcr)
        );
        assert!(
            final_document
                .document
                .text
                .contains("transcription page 26")
        );
        assert_eq!(final_document.unmapped_merged_lines, 0);
        for (p, bytes) in final_document.pages.iter().zip(&raw) {
            assert_eq!(
                &std::fs::read(
                    prepared
                        .staging_root()
                        .join(&p.metadata_artifact.relative_path)
                )
                .unwrap(),
                bytes
            );
            let metadata = validate_native_graphics_metadata(
                bytes,
                &final_document.summary,
                &final_document.native_profile_sha256,
                p.number,
                &budget,
            )
            .unwrap();
            assert_eq!(metadata.graphics.object_count, 7);
            assert!(!metadata.native_text_metadata.characters.is_empty());
            assert!(metadata.page.text_items.iter().any(|i| !i.words.is_empty()));
            assert_eq!(
                metadata.page.vector_graphics.as_ref().unwrap().shapes.len(),
                2
            );
        }
        outputs.push((raw, serde_json::to_value(&final_document.document).unwrap()));
    }
    assert_eq!(outputs[0], outputs[1]);
}
