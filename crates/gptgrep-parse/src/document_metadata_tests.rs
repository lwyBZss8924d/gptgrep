use super::*;

pub(crate) fn metadata_pdf(title: &str, destination: &str) -> Vec<u8> {
    let body = format!(
        "BT /F1 8 Tf 20 180 Td {} ET",
        "(Native document metadata accompanies ordinary readable source text.) Tj 0 -12 Td "
            .repeat(12)
    );
    let title = title
        .encode_utf16()
        .map(|u| format!("{u:04X}"))
        .collect::<String>();
    let destination = if destination.starts_with('[') {
        format!("/Dest {destination}")
    } else {
        format!("/A {destination}")
    };
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R /Outlines 8 0 R /PageLabels 12 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Type /Outlines /First 9 0 R /Last 9 0 R /Count 2 >>".into(),
        format!("<< /Title (Opening section) /Parent 8 0 R /First 10 0 R /Last 10 0 R /Count 1 {destination} >>"),
        "<< /Title (Details section) /Parent 9 0 R /Dest [5 0 R /Fit] >>".into(),
        format!("<< /Title <FEFF{title}> /Author (Synthetic author) /Creator (Self-authored fixture) >>"),
        "<< /Nums [0 << /S /r >> 1 << /P (Sec-) /S /D /St 7 >>] >>".into(),
    ];
    let pdf = String::from_utf8(crate::tests::pdf_objects(&objects)).unwrap();
    pdf.replace("/Root 1 0 R >>", "/Root 1 0 R /Info 11 0 R >>")
        .into_bytes()
}

async fn prepare(
    dir: &tempfile::TempDir,
    bytes: &[u8],
    budget: &ResourceBudget,
) -> PreparedDocument {
    let path = dir.path().join("source.pdf");
    std::fs::write(&path, bytes).unwrap();
    prepare_document(
        &path,
        &dir.path().join("staging"),
        &PreparationOptions,
        budget,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn native_metadata_preserves_info_labels_bookmarks_and_final_tree_across_windows() {
    let mut previous = None;
    for window in [1, 25] {
        let dir = tempfile::tempdir().unwrap();
        let budget = ResourceBudget::default();
        let prepared = prepare(
            &dir,
            &metadata_pdf("Bounded metadata Ω", "[3 0 R /XYZ null 180 null]"),
            &budget,
        )
        .await;
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                page_window: window,
                render_pages: false,
                ..Default::default()
            },
            &budget,
        )
        .await
        .unwrap();
        assert!(
            candidate
                .pages()
                .iter()
                .all(|p| p.coverage == CoverageState::Native)
        );
        let metadata = candidate.document_metadata().unwrap();
        assert!(
            matches!(&metadata.info["Title"], FeatureState::Present(value) if value == "Bounded metadata Ω")
        );
        assert!(matches!(
            &metadata.info["Subject"],
            FeatureState::Unsupported(_)
        ));
        assert!(
            matches!(&metadata.page_labels[0].label, FeatureState::Present(value) if value == "i")
        );
        assert!(
            matches!(&metadata.page_labels[1].label, FeatureState::Present(value) if value == "Sec-7")
        );
        let FeatureState::Present(bookmarks) = &metadata.bookmarks else {
            panic!("retained complete outline")
        };
        assert_eq!(
            bookmarks.iter().map(|x| x.depth).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(
            matches!(&bookmarks[0].destination, FeatureState::Present(dest) if dest.physical_page == 1 && matches!(dest.source_y, FeatureState::Present(y) if y == 180.0))
        );
        assert!(
            matches!(&bookmarks[1].destination, FeatureState::Present(dest) if dest.physical_page == 2 && matches!(dest.source_y, FeatureState::Unsupported(_)))
        );
        let artifact = candidate
            .artifacts()
            .iter()
            .find(|a| a.kind == ArtifactKind::NativeMetadata && a.page.is_none())
            .unwrap();
        let identity = (
            artifact.id.clone(),
            artifact.sha256.clone(),
            candidate.native_profile_sha256().to_owned(),
        );
        if let Some(previous) = &previous {
            assert_eq!(previous, &identity)
        } else {
            previous = Some(identity)
        }
        let rich = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap();
        assert!(
            rich.document
                .nodes
                .iter()
                .any(|n| n.title == "Details section" && n.page_start == 2)
        );
        assert!(rich.ocr_tasks.is_empty());
        assert!(
            !serde_json::to_string(&rich)
                .unwrap()
                .contains(dir.path().to_str().unwrap())
        );
    }
}

#[tokio::test]
async fn native_metadata_refuses_oversized_values_invalid_destinations_and_document_budget() {
    for case in ["value", "destination", "budget"] {
        let dir = tempfile::tempdir().unwrap();
        let mut budget = ResourceBudget::default();
        if case == "budget" {
            budget.document_metadata_bytes = 8192;
        }
        let title = if case == "value" {
            "T".repeat(100_000)
        } else {
            "Title".into()
        };
        let prepared = prepare(
            &dir,
            &metadata_pdf(
                &title,
                if case == "destination" {
                    "[99 /Fit]"
                } else {
                    "[3 0 R /Fit]"
                },
            ),
            &budget,
        )
        .await;
        assert!(
            extract_native(
                &prepared,
                &NativeOptions {
                    render_pages: false,
                    ..Default::default()
                },
                &budget
            )
            .await
            .is_err(),
            "case {case}"
        );
        assert_eq!(budget.staging_used(), prepared.summary().prepared_bytes);
        assert!(
            !std::fs::read_dir(prepared.staging_root())
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with("document-native.json"))
        );
    }
}

#[tokio::test]
async fn native_metadata_keeps_form_refusal_and_source_custody() {
    let dir = tempfile::tempdir().unwrap();
    let budget = ResourceBudget::default();
    let bytes = metadata_pdf("Title", "[3 0 R /Fit]");
    let form = String::from_utf8(bytes.clone()).unwrap().replace(
        "/PageLabels 12 0 R",
        "/PageLabels 12 0 R /AcroForm << /Fields [] >>",
    );
    let prepared = prepare(&dir, form.as_bytes(), &budget).await;
    assert!(
        extract_native(&prepared, &NativeOptions::default(), &budget)
            .await
            .unwrap_err()
            .to_string()
            .contains("XFA/AcroForm")
    );
    let second = tempfile::tempdir().unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare(&second, &bytes, &budget).await;
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
    std::fs::write(prepared.source_path(), b"changed source").unwrap();
    assert!(finalize_document(candidate, vec![], OcrMode::Auto, &budget).is_err());
}

#[tokio::test]
async fn native_metadata_keeps_every_action_destination_unsupported() {
    // Direct destinations are positively exercised by the complete-projection
    // fixture. Even an in-document action, or both an action and a direct
    // destination, cannot establish a local target without action-type proof.
    for action in [
        "<< /S /GoToR /F (remote.pdf) /D [0 /Fit] >>",
        "<< /S /GoTo /D [3 0 R /Fit] >>",
        "<< /S /GoTo /D [3 0 R /Fit] >> /Dest [3 0 R /Fit]",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let budget = ResourceBudget::default();
        let bytes = metadata_pdf("Title", action);
        let prepared = prepare(&dir, &bytes, &budget).await;
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
        let FeatureState::Present(bookmarks) = &candidate.document_metadata().unwrap().bookmarks
        else {
            panic!("complete outline")
        };
        assert_eq!(bookmarks[0].title, "Opening section");
        assert!(
            matches!(&bookmarks[0].destination, FeatureState::Unsupported(reason) if reason == "pdfium_action_present_or_no_direct_destination")
        );
        assert!(
            matches!(&bookmarks[1].destination, FeatureState::Present(dest) if dest.physical_page == 2)
        );
        let rich = finalize_document(candidate, vec![], OcrMode::Auto, &budget).unwrap();
        assert!(
            !rich
                .document
                .nodes
                .iter()
                .any(|node| node.title == "Opening section")
        );
        assert!(
            rich.document
                .nodes
                .iter()
                .any(|node| node.title == "Details section")
        );
    }
}

#[tokio::test]
async fn retained_native_metadata_validator_rejects_source_page_hierarchy_and_availability_changes()
{
    let dir = tempfile::tempdir().unwrap();
    let budget = ResourceBudget::default();
    let prepared = prepare(&dir, &metadata_pdf("Title", "[3 0 R /Fit]"), &budget).await;
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
    for case in 0..6 {
        let mut metadata = candidate.document_metadata().unwrap().clone();
        match case {
            0 => metadata.source_sha256 = "0".repeat(64),
            1 => metadata.page_labels[0].physical_page = 2,
            2 => metadata
                .info
                .insert("Unknown".into(), FeatureState::Absent)
                .map(|_| ())
                .unwrap_or(()),
            3 => {
                if let FeatureState::Present(bookmarks) = &mut metadata.bookmarks {
                    bookmarks[0].depth = 2;
                }
            }
            4 => {
                if let FeatureState::Present(bookmarks) = &mut metadata.bookmarks {
                    bookmarks[0].destination = FeatureState::Present(NativeBookmarkDestination {
                        physical_page: 3,
                        source_y: FeatureState::Unsupported(
                            "pdfium_non_xyz_or_missing_y_or_getter_failure".into(),
                        ),
                    });
                }
            }
            5 => metadata
                .info
                .insert(
                    "Title".into(),
                    FeatureState::Failed("native read failed".into()),
                )
                .map(|_| ())
                .unwrap_or(()),
            _ => unreachable!(),
        }
        assert!(
            validate_native_document_metadata(&metadata, prepared.summary(), 2, &budget).is_err(),
            "case {case}"
        );
    }
}
