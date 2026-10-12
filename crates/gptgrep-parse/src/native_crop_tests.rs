//! Authored, UNRUN, public synthetic tests. Generated PNGs exercise parser
//! custody/geometry only; synthetic receipts prove no paid/provider behavior.
//! A future test run must use separately admitted external temporary storage.
use super::*;
use crate::{
    AffineTransform, ArtifactKind, ArtifactRef, OcrImage, OcrImageDelivery, OcrNativeImageDelivery,
    OcrNativeImageReference, OcrPageResult, OcrProfile, OcrReceipt, OcrRegion, OcrRegionResult,
    OcrRegionStatus, OcrResultStatus, OcrTaskBudget, artifact_id, ocr_profile_digest,
    ocr_task_digest, validate_ocr_result, validate_ocr_result_metadata,
};
use image::ImageEncoder;
use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};
const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    _dir: tempfile::TempDir,
    task: OcrTask,
    rgba: Vec<u8>,
}
impl Fixture {
    fn new(transform: AffineTransform, page: (f64, f64)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = b"synthetic public crop source; no PDF/native/provider execution";
        fs::write(root.join("source.txt"), source).unwrap();
        let mut rgba = Vec::new();
        for y in 0u8..6 {
            for x in 0u8..8 {
                rgba.extend_from_slice(&[x * 17, y * 29, (x + y) * 11, 255 - x * 3 - y * 2]);
            }
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, 8, 6, image::ExtendedColorType::Rgba8)
            .unwrap();
        fs::create_dir(root.join("images")).unwrap();
        fs::write(root.join("images/parent.png"), &png).unwrap();
        let source_sha256 = sha256(source);
        let image_sha = sha256(&png);
        let id = artifact_id(
            &source_sha256,
            H,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &image_sha,
        )
        .unwrap();
        let mut task = OcrTask {
            schema_version: "gptgrep.ocr.task.v1".into(),
            id: String::new(),
            task_sha256: String::new(),
            source_sha256,
            prepared_sha256: H.into(),
            source_path: root.join("source.txt"),
            page: 1,
            page_width_points: page.0,
            page_height_points: page.1,
            regions: vec![OcrRegion {
                id: "region-original".into(),
                bbox: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: page.0,
                    height: page.1,
                },
                required: true,
                reason: "synthetic image".into(),
            }],
            images: vec![OcrImage {
                artifact: ArtifactRef {
                    id,
                    kind: ArtifactKind::Rendition,
                    relative_path: "images/parent.png".into(),
                    sha256: image_sha,
                    profile_sha256: H.into(),
                    bytes: png.len() as u64,
                    mime: "image/png".into(),
                    page: Some(1),
                    region_id: None,
                },
                path: root.join("images/parent.png"),
                width: 8,
                height: 6,
                page_to_raster: transform,
            }],
            native_text: String::new(),
            layout_context: "generated PNG".into(),
            profile: OcrProfile::default(),
            budget: OcrTaskBudget::default(),
        };
        rebind(&mut task);
        Self {
            _dir: dir,
            task,
            rgba,
        }
    }
    fn axis() -> Self {
        Self::new(
            AffineTransform {
                a: 2.0,
                d: 3.0,
                ..AffineTransform::IDENTITY
            },
            (4.0, 2.0),
        )
    }
    fn crop(&self, bbox: BoundingBox) -> NativeOcrCrop {
        create_native_ocr_crop(
            &self.task,
            &self.task.images[0].artifact.id,
            File::open(&self.task.images[0].path).unwrap(),
            bbox,
            &self.task.budget.resources,
        )
        .unwrap()
    }
}
fn rebind(task: &mut OcrTask) {
    task.task_sha256 = ocr_task_digest(task).unwrap();
    task.id = format!("ocr-{}", task.task_sha256);
}
fn selected() -> BoundingBox {
    BoundingBox {
        x: 1.0,
        y: 0.3,
        width: 1.5,
        height: 0.9,
    }
}
fn native_delivery(
    task: &OcrTask,
    crop: Option<NativeOcrCropProof>,
    ordinal: u32,
) -> OcrImageDelivery {
    let parent = &task.images[0];
    let rendition = crop
        .as_ref()
        .map_or(&parent.artifact.profile_sha256, |p| &p.recipe_sha256)
        .clone();
    OcrImageDelivery {
        artifact_id: parent.artifact.id.clone(),
        artifact_sha256: parent.artifact.sha256.clone(),
        tool_ordinal: ordinal,
        mime: parent.artifact.mime.clone(),
        width: parent.width,
        height: parent.height,
        tool_response_sha256: H.into(),
        native: Some(OcrNativeImageDelivery {
            schema_version: "gptgrep.ocr.native-image-delivery.v1".into(),
            admission_sha256: H.into(),
            task_sha256: task.task_sha256.clone(),
            source_sha256: task.source_sha256.clone(),
            prepared_sha256: task.prepared_sha256.clone(),
            runtime_profile_sha256: H.into(),
            call_id_sha256: format!("{ordinal:064x}"),
            source_receipt_sha256: H.into(),
            prepared_receipt_sha256: H.into(),
            assembled_receipt_sha256: H.into(),
            rendition_sha256: rendition,
            prepared_image_sha256: H.into(),
            prepared_image_bytes: 100,
            prepared_width: 2,
            prepared_height: 2,
            image_content_wire_charge: 256,
            reference: OcrNativeImageReference::Inline {
                bytes: 100,
                sha256: H.into(),
            },
            local_assembly_verified: true,
            provider_ingestion_verified: false,
            native_crop: crop,
        }),
    }
}
fn result(task: &OcrTask, image_deliveries: Vec<OcrImageDelivery>) -> OcrPageResult {
    OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: vec![OcrRegionResult {
            id: task.regions[0].id.clone(),
            bbox: task.regions[0].bbox,
            text: "synthetic transcription".into(),
            reading_order: 0,
            status: OcrRegionStatus::Complete,
        }],
        receipt: OcrReceipt {
            native_agent: None,
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile).unwrap(),
            runtime_profile_sha256: H.into(),
            image_deliveries,
            request_sha256: H.into(),
            response_sha256: H.into(),
            usage: None,
            latency_ms: None,
        },
    }
}

#[test]
fn generated_crop_keeps_exact_samples_outward_bounds_and_original_task() {
    let fixture = Fixture::axis();
    let before = serde_json::to_vec(&fixture.task).unwrap();
    let crop = fixture.crop(selected());
    let proof = crop.proof();
    assert_eq!(
        proof.recipe.pixels,
        crate::PixelCrop {
            x: 2,
            y: 0,
            width: 3,
            height: 4
        }
    );
    assert_eq!(proof.recipe.crop_to_page.apply(0.0, 0.0), (1.0, 0.0));
    let decoded = image::load_from_memory(crop.png()).unwrap().to_rgba8();
    for y in 0..4 {
        for x in 0..3 {
            let offset = ((y * 8 + x + 2) * 4) as usize;
            assert_eq!(decoded.get_pixel(x, y).0, fixture.rgba[offset..offset + 4]);
        }
    }
    validate_native_crop_proof(&fixture.task, proof).unwrap();
    assert_eq!(serde_json::to_vec(&fixture.task).unwrap(), before);
    assert!(
        !fixture
            .task
            .images
            .iter()
            .any(|i| i.artifact.id == proof.derived_artifact_id)
    );
}

#[test]
fn quarter_turn_uses_explicit_transform_without_rotating_crop_samples() {
    let fixture = Fixture::new(
        AffineTransform {
            a: 0.0,
            b: 1.0,
            c: -1.0,
            d: 0.0,
            e: 8.0,
            f: 0.0,
        },
        (6.0, 8.0),
    );
    let crop = fixture.crop(BoundingBox {
        x: 1.0,
        y: 1.0,
        width: 2.0,
        height: 3.0,
    });
    assert_eq!(
        crop.proof().recipe.pixels,
        crate::PixelCrop {
            x: 4,
            y: 1,
            width: 3,
            height: 2
        }
    );
    assert_eq!(crop.proof().recipe.page_to_crop.apply(2.0, 2.0), (2.0, 1.0));
    assert_eq!(crop.proof().recipe.crop_to_page.apply(2.0, 1.0), (2.0, 2.0));
    let decoded = image::load_from_memory(crop.png()).unwrap().to_rgba8();
    for y in 0..2 {
        for x in 0..3 {
            let offset = (((y + 1) * 8 + x + 4) * 4) as usize;
            assert_eq!(decoded.get_pixel(x, y).0, fixture.rgba[offset..offset + 4]);
        }
    }
    validate_native_crop_proof(&fixture.task, crop.proof()).unwrap();
}

#[test]
fn crop_only_cannot_satisfy_unseen_original_and_result_boxes_stay_original() {
    let fixture = Fixture::axis();
    let crop = fixture.crop(selected());
    let only_crop = result(
        &fixture.task,
        vec![native_delivery(
            &fixture.task,
            Some(crop.proof().clone()),
            1,
        )],
    );
    assert!(validate_ocr_result_metadata(&fixture.task, &only_crop).is_err());
    let mut both = result(
        &fixture.task,
        vec![
            native_delivery(&fixture.task, None, 1),
            native_delivery(&fixture.task, Some(crop.proof().clone()), 2),
        ],
    );
    validate_ocr_result(&fixture.task, &both).unwrap();
    both.regions[0].bbox = selected();
    assert!(validate_ocr_result_metadata(&fixture.task, &both).is_err());
    both.regions[0].bbox = fixture.task.regions[0].bbox;
    both.regions[0].id = crop.proof().derived_artifact_id.clone();
    assert!(validate_ocr_result_metadata(&fixture.task, &both).is_err());
}

#[test]
fn serialized_crop_digest_cannot_authenticate_fabricated_pixels() {
    let fixture = Fixture::axis();
    let crop = fixture.crop(selected());
    let mut forged = crop.proof().clone();
    forged.png_sha256 = H.into();
    forged.derived_artifact_id = derived_id(&forged).unwrap();
    // Portable metadata has no source pixels. The full parser must reject this
    // internally consistent but fabricated image digest by real regeneration.
    validate_native_crop_proof_metadata(&fixture.task, &forged).unwrap();
    assert!(validate_native_crop_proof(&fixture.task, &forged).is_err());
}

#[test]
fn parent_task_viewport_and_recipe_spoofs_are_rejected() {
    let fixture = Fixture::axis();
    let crop = fixture.crop(selected());
    let original = crop.proof();
    for kind in 0..5 {
        let mut proof = original.clone();
        match kind {
            0 => proof.recipe.parent_sha256 = H.into(),
            1 => proof.recipe.crop_to_page.e += 1.0,
            2 => proof.width += 1,
            3 => proof.recipe.parent_artifact_id = format!("artifact-{H}"),
            _ => proof.task_sha256 = H.into(),
        }
        assert!(validate_native_crop_proof_metadata(&fixture.task, &proof).is_err());
    }
    let mut next = fixture.task.clone();
    next.prepared_sha256 = "b".repeat(64);
    rebind(&mut next);
    assert!(validate_native_crop_proof_metadata(&next, original).is_err());
}

#[test]
fn changed_original_source_and_parent_fail_full_validation() {
    let fixture = Fixture::axis();
    let crop = fixture.crop(selected());
    fs::write(&fixture.task.source_path, b"changed original source").unwrap();
    assert!(validate_native_crop_proof(&fixture.task, crop.proof()).is_err());
    let fixture = Fixture::axis();
    let crop = fixture.crop(selected());
    fs::write(&fixture.task.images[0].path, b"changed parent").unwrap();
    assert!(validate_native_crop_proof(&fixture.task, crop.proof()).is_err());
}

#[test]
fn duplicate_supplements_and_child_artifact_as_original_are_refused() {
    let fixture = Fixture::axis();
    let one = fixture.crop(selected());
    let two = fixture.crop(BoundingBox {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    });
    assert!(validate_native_crop_set(&fixture.task, &[one, two]).is_err());
    let crop = fixture.crop(selected());
    let proof = crop.proof().clone();
    let duplicate = result(
        &fixture.task,
        vec![
            native_delivery(&fixture.task, None, 1),
            native_delivery(&fixture.task, Some(proof.clone()), 2),
            native_delivery(&fixture.task, Some(proof.clone()), 3),
        ],
    );
    assert!(validate_ocr_result_metadata(&fixture.task, &duplicate).is_err());
    let mut wrong = native_delivery(&fixture.task, Some(proof.clone()), 2);
    wrong.artifact_id = proof.derived_artifact_id;
    assert!(
        validate_ocr_result_metadata(
            &fixture.task,
            &result(
                &fixture.task,
                vec![native_delivery(&fixture.task, None, 1), wrong]
            )
        )
        .is_err()
    );
}

#[test]
fn original_caps_and_invocation_counter_cannot_be_reset_for_crop() {
    let mut fixture = Fixture::axis();
    fixture.task.budget.resources.images_per_page = 1;
    rebind(&mut fixture.task);
    assert!(
        create_native_ocr_crop(
            &fixture.task,
            &fixture.task.images[0].artifact.id,
            File::open(&fixture.task.images[0].path).unwrap(),
            selected(),
            &fixture.task.budget.resources
        )
        .is_err()
    );
    let fixture = Fixture::axis();
    let foreign = ResourceBudget::default();
    assert!(
        create_native_ocr_crop(
            &fixture.task,
            &fixture.task.images[0].artifact.id,
            File::open(&fixture.task.images[0].path).unwrap(),
            selected(),
            &foreign
        )
        .is_err()
    );
    let mut tight = fixture.task.budget.resources.clone();
    tight.owned_working_bytes = 1_048_575;
    assert!(
        create_native_ocr_crop(
            &fixture.task,
            &fixture.task.images[0].artifact.id,
            File::open(&fixture.task.images[0].path).unwrap(),
            selected(),
            &tight
        )
        .is_err()
    );
    let before = fixture.task.budget.resources.staging_used();
    let crop = fixture.crop(selected());
    fixture
        .task
        .budget
        .resources
        .reserve_retained_staging(crop.proof().png_bytes)
        .unwrap();
    let charged = fixture.task.budget.resources.staging_used();
    drop(crop);
    assert!(charged > before);
    assert_eq!(fixture.task.budget.resources.staging_used(), charged);
}

#[test]
fn png_output_refuses_preallocation_above_original_file_cap() {
    let mut fixture = Fixture::axis();
    fixture.task.budget.resources.image_file_bytes = fixture.task.images[0].artifact.bytes;
    rebind(&mut fixture.task);
    let error = create_native_ocr_crop(
        &fixture.task,
        &fixture.task.images[0].artifact.id,
        File::open(&fixture.task.images[0].path).unwrap(),
        selected(),
        &fixture.task.budget.resources,
    )
    .unwrap_err();
    assert!(error.to_string().contains("crop PNG output admission"));
}

#[test]
fn tiny_crop_fits_tighter_original_operation_allowance_without_pixel_or_wire_change() {
    let fixture = Fixture::axis();
    let original = fixture.crop(selected());
    let mut lower = fixture.task.budget.resources.clone();
    lower.owned_working_bytes = 32 * 1024 * 1024;
    let crop = create_native_ocr_crop(
        &fixture.task,
        &fixture.task.images[0].artifact.id,
        File::open(&fixture.task.images[0].path).unwrap(),
        selected(),
        &lower,
    )
    .unwrap();
    assert_eq!(crop.png(), original.png());
    assert_eq!(
        serde_json::to_vec(crop.proof()).unwrap(),
        serde_json::to_vec(original.proof()).unwrap()
    );
    let selected = crate::crop::decoder_budget(&fixture.task.images[0], &lower).unwrap();
    assert!(selected.image_pixel_buffer_bytes < lower.image_pixel_buffer_bytes);
    selected.ensure_same_invocation_within(&lower).unwrap();
    assert_eq!(selected.staging_used(), lower.staging_used());
}

#[test]
fn original_and_crop_share_the_page_byte_and_pixel_caps() {
    let mut fixture = Fixture::axis();
    let proof = fixture.crop(selected()).proof().clone();
    fixture.task.budget.resources.image_file_bytes_per_page =
        fixture.task.images[0].artifact.bytes + proof.png_bytes - 1;
    rebind(&mut fixture.task);
    assert!(
        create_native_ocr_crop(
            &fixture.task,
            &fixture.task.images[0].artifact.id,
            File::open(&fixture.task.images[0].path).unwrap(),
            selected(),
            &fixture.task.budget.resources
        )
        .is_err()
    );
    let mut fixture = Fixture::axis();
    fixture.task.budget.resources.image_pixels_per_page = 48 + 12 - 1;
    rebind(&mut fixture.task);
    assert!(
        create_native_ocr_crop(
            &fixture.task,
            &fixture.task.images[0].artifact.id,
            File::open(&fixture.task.images[0].path).unwrap(),
            selected(),
            &fixture.task.budget.resources
        )
        .is_err()
    );
}

#[test]
fn buffer_owner_survives_caller_drop_and_cannot_be_replaced() {
    struct Notice(Arc<AtomicUsize>);
    impl Drop for Notice {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let fixture = Fixture::axis();
    let count = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Notice(count.clone()));
    let crop = fixture
        .crop(selected())
        .retain_working_owner(owner.clone())
        .unwrap();
    drop(owner);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // Ownership can move to the executor; the lease ends only with the bytes.
    let owned_child_input = crop;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        owned_child_input
            .retain_working_owner(Arc::new(()))
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn dynamic_delivery_none_serializes_byte_identically_and_crop_is_optional() {
    let legacy = OcrImageDelivery {
        artifact_id: "parent".into(),
        artifact_sha256: H.into(),
        tool_ordinal: 1,
        mime: "image/png".into(),
        width: 8,
        height: 6,
        tool_response_sha256: H.into(),
        native: None,
    };
    let expected = format!(
        "{{\"artifact_id\":\"parent\",\"artifact_sha256\":\"{H}\",\"tool_ordinal\":1,\"mime\":\"image/png\",\"width\":8,\"height\":6,\"tool_response_sha256\":\"{H}\"}}"
    );
    assert_eq!(serde_json::to_vec(&legacy).unwrap(), expected.as_bytes());
    let fixture = Fixture::axis();
    let native = native_delivery(&fixture.task, None, 1);
    assert!(
        serde_json::to_value(&native).unwrap()["native"]
            .get("native_crop")
            .is_none()
    );
    let old = serde_json::to_vec(&native).unwrap();
    assert_eq!(
        serde_json::to_vec(&serde_json::from_slice::<OcrImageDelivery>(&old).unwrap()).unwrap(),
        old
    );
}
