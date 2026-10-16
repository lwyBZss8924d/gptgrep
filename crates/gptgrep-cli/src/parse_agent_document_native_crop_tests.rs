//! Authored, UNRUN source-custody tests. No provider, app-server, paid OCR,
//! authentication or owner activity. Temp storage needs independent admission.
use super::*;
use gptgrep_parse::{
    AffineTransform, OcrImage, OcrProfile, OcrRegion, OcrTaskBudget, artifact_id, ocr_task_digest,
};
use image::ImageEncoder;

#[test]
fn document_working_lease_follows_crop_through_pending_and_owned_child_move() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    fs::write(root.join("source.txt"), b"synthetic source")?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        &[10, 20, 30, 255],
        1,
        1,
        image::ExtendedColorType::Rgba8,
    )?;
    fs::write(root.join("parent.png"), &png)?;
    let source_sha = digest(b"synthetic source");
    let profile_sha = "a".repeat(64);
    let image_sha = digest(&png);
    let artifact = ArtifactRef {
        id: artifact_id(
            &source_sha,
            &profile_sha,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &image_sha,
        )?,
        kind: ArtifactKind::Rendition,
        relative_path: "parent.png".into(),
        sha256: image_sha,
        profile_sha256: profile_sha,
        bytes: png.len() as u64,
        mime: "image/png".into(),
        page: Some(1),
        region_id: None,
    };
    let bbox = BoundingBox {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    let mut task = OcrTask {
        schema_version: "gptgrep.ocr.task.v1".into(),
        id: String::new(),
        task_sha256: String::new(),
        source_sha256: source_sha,
        prepared_sha256: "b".repeat(64),
        source_path: root.join("source.txt"),
        page: 1,
        page_width_points: 1.0,
        page_height_points: 1.0,
        regions: vec![OcrRegion {
            id: "original".into(),
            bbox,
            required: true,
            reason: "generated".into(),
        }],
        images: vec![OcrImage {
            artifact: artifact.clone(),
            path: root.join("parent.png"),
            width: 1,
            height: 1,
            page_to_raster: AffineTransform::IDENTITY,
        }],
        native_text: String::new(),
        layout_context: String::new(),
        profile: OcrProfile::default(),
        budget: OcrTaskBudget::default(),
    };
    task.task_sha256 = ocr_task_digest(&task)?;
    task.id = format!("ocr-{}", task.task_sha256);
    let retained = task.budget.resources.image_file_bytes + 16_384 * 42 * 4 + 4096;
    let working = DocumentWorking::new(retained);
    let lease = working.reserve(retained)?;
    let crop = gptgrep_parse::create_native_ocr_crop(
        &task,
        &artifact.id,
        File::open(&task.images[0].path)?,
        bbox,
        &task.budget.resources,
    )?
    .retain_working_owner(std::sync::Arc::new(lease))?;
    let mut pending = BTreeMap::<String, Vec<PendingCrop>>::new();
    pending.insert(task.task_sha256.clone(), vec![PendingCrop { crop }]);
    assert!(working.reserve(1).is_err());
    let owned_child = pending
        .remove(&task.task_sha256)
        .expect("pending task")
        .pop()
        .expect("crop")
        .crop;
    assert!(pending.is_empty());
    assert!(working.reserve(1).is_err());
    let before = task.budget.resources.staging_used();
    task.budget
        .resources
        .reserve_retained_staging(owned_child.proof().png_bytes)?;
    let charged = task.budget.resources.staging_used();
    drop(owned_child);
    working.reserve(retained)?;
    assert!(charged > before);
    assert_eq!(task.budget.resources.staging_used(), charged);
    Ok(())
}

#[test]
fn overflow_lease_refuses_before_source_crop_can_be_constructed() -> Result<()> {
    let required = ResourceBudget::default().image_file_bytes + 16_384 * 42 * 4 + 4096;
    let working = DocumentWorking::new(required - 1);
    assert!(working.reserve(required).is_err());
    // Failed reservations neither grow capacity nor consume an unrelated lease.
    let held = working.reserve(required - 1)?;
    assert!(working.reserve(1).is_err());
    drop(held);
    working.reserve(required - 1)?;
    Ok(())
}

#[test]
fn crop_tool_json_cannot_register_paths_tasks_recipes_or_paid_results() {
    for extra in [
        "native_path",
        "task_sha256",
        "native_crop",
        "receipt",
        "executor",
    ] {
        let mut value = serde_json::json!({"op":"crop","artifact_id":format!("artifact-{}","a".repeat(64)),
            "bbox":{"x":0,"y":0,"width":1,"height":1}});
        value[extra] = serde_json::json!({"untrusted":true});
        assert!(serde_json::from_value::<DocumentToolOperation>(value).is_err());
    }
}
