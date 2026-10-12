//! Focused source tests; fixtures do not launch any runtime or provider.
use super::*;
use crate::ocr_tests::Fixture;
use std::time::Duration;
const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const P: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture_crop(fixture: &Fixture) -> gptgrep_parse::NativeOcrCrop {
    gptgrep_parse::create_native_ocr_crop(
        &fixture.task,
        &fixture.task.images[0].artifact.id,
        File::open(&fixture.task.images[0].path).unwrap(),
        gptgrep_parse::BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        &fixture.task.budget.resources,
    )
    .unwrap()
}
fn crop_session(fixture: &Fixture) -> NativeOcrSession {
    let mut page = fixture.page();
    page.native_crops.push(fixture_crop(fixture));
    NativeOcrSession::prepare(
        &page,
        &fixture.dir.path().join("private-crop"),
        H,
        tokio::time::Instant::now() + Duration::from_secs(60),
        now_ms().unwrap() + 90_000,
    )
    .unwrap()
}

fn profile() -> OcrRuntimeProfile {
    OcrRuntimeProfile::unqualified_native("a".repeat(40), H.into(), H.into(), H.into()).unwrap()
}

#[test]
fn configured_page_admits_real_png_above_default_and_binds_profile_manifest_ready() {
    let mut fixture = Fixture::new();
    let png = crate::ocr_tests::configured_image_fixture_png();
    let resources = &mut fixture.task.budget.resources;
    resources.image_file_bytes = 4_194_304;
    resources.image_base64_bytes = 5_592_408;
    resources.image_file_bytes_per_page = 4_194_304;
    let image = &mut fixture.task.images[0];
    fs::write(&image.path, &png).unwrap();
    image.width = 1024;
    image.height = 1024;
    image.artifact.bytes = png.len() as u64;
    image.artifact.sha256 = hash(&png);
    image.artifact.id = gptgrep_parse::artifact_id(
        &fixture.task.source_sha256,
        &image.artifact.profile_sha256,
        &image.artifact.kind,
        Some(1),
        None,
        &image.artifact.sha256,
    )
    .unwrap();
    fixture.task.task_sha256 = gptgrep_parse::ocr_task_digest(&fixture.task).unwrap();
    fixture.task.id = format!("ocr-{}", fixture.task.task_sha256);
    let selected = OcrRuntimeProfile::unqualified_native_with_resources(
        "a".repeat(40),
        H.into(),
        H.into(),
        H.into(),
        &fixture.task.budget.resources,
    )
    .unwrap();
    validate_resource_profile(&fixture.task, &selected).unwrap();
    assert!(validate_resource_profile(&fixture.task, &profile()).is_err());
    assert_ne!(selected.sha256().unwrap(), profile().sha256().unwrap());
    assert!(!selected.image_qualification_accepted());
    let native = session(&fixture);
    assert_eq!(
        native.manifest["image_limits"],
        serde_json::to_value(&selected.native_viewer.unwrap().image_limits).unwrap()
    );
    assert_eq!(
        native.ready["image_limits"],
        native.manifest["image_limits"]
    );
    assert_eq!(native.grants[0].bytes, png.len() as u64);
    assert_eq!(
        fs::read(native.image_root.join("image-1.png")).unwrap(),
        png
    );
    let mut lower = fixture.task.clone();
    lower.budget.resources = ResourceBudget::default();
    lower.task_sha256 = gptgrep_parse::ocr_task_digest(&lower).unwrap();
    lower.id = format!("ocr-{}", lower.task_sha256);
    assert!(PreparedPage::new(lower, fixture.dir.path()).is_err());
}

#[test]
fn native_structured_output_declares_scalar_types_and_preserves_exact_identities() {
    let fixture = Fixture::new();
    let schema = crate::ocr_protocol::native_output_schema(&fixture.task);
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let properties = schema["properties"].as_object().unwrap();
    for (field, value) in [
        ("schema_version", json!("gptgrep.ocr.result.v1")),
        ("task_id", json!(fixture.task.id)),
        ("task_sha256", json!(fixture.task.task_sha256)),
        ("source_sha256", json!(fixture.task.source_sha256)),
        ("prepared_sha256", json!(fixture.task.prepared_sha256)),
    ] {
        assert_eq!(properties[field]["type"], "string");
        assert_eq!(properties[field]["const"], value);
    }
    assert_eq!(properties["page"]["type"], "integer");
    assert_eq!(properties["page"]["const"], fixture.task.page);
    assert!(
        properties
            .values()
            .all(|property| property.get("type").is_some())
    );
    assert_eq!(
        schema["required"].as_array().unwrap().len(),
        properties.len()
    );
}

#[test]
fn native_output_rejects_invented_regions_and_crossed_source_geometry() {
    let fixture = Fixture::new();
    let mut task = fixture.task.clone();
    let mut second = task.regions[0].clone();
    second.id = "second-source-region".into();
    second.bbox.x += 0.25;
    task.regions.push(second);
    let schema = crate::ocr_protocol::native_output_schema(&task);
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut value = json!({
        "schema_version":"gptgrep.ocr.result.v1", "task_id":task.id,
        "task_sha256":task.task_sha256,"source_sha256":task.source_sha256,
        "prepared_sha256":task.prepared_sha256,"page":task.page,"status":"complete",
        "regions":task.regions.iter().enumerate().map(|(order,region)| json!({
            "id":region.id,"bbox":region.bbox,"text":"synthetic transcription",
            "reading_order":order,"status":"complete"
        })).collect::<Vec<_>>()
    });
    assert!(validator.is_valid(&value));
    let correct = value.clone();
    value["regions"][0]["id"] = json!("model-invented-subdivision");
    assert!(!validator.is_valid(&value));
    value = correct.clone();
    value["regions"][0]["bbox"] = correct["regions"][1]["bbox"].clone();
    assert!(!validator.is_valid(&value));
    value = correct.clone();
    value["regions"]
        .as_array_mut()
        .unwrap()
        .push(correct["regions"][0].clone());
    assert!(!validator.is_valid(&value));

    // A single source region uses the same exact constraint without anyOf.
    let single =
        jsonschema::validator_for(&crate::ocr_protocol::native_output_schema(&fixture.task))
            .unwrap();
    value = correct;
    value["regions"].as_array_mut().unwrap().truncate(1);
    assert!(single.is_valid(&value));
    value["regions"][0]["bbox"]["width"] = json!(0.5);
    assert!(!single.is_valid(&value));
}
fn session(fixture: &Fixture) -> NativeOcrSession {
    NativeOcrSession::prepare(
        &fixture.page(),
        &fixture.dir.path().join("private"),
        H,
        tokio::time::Instant::now() + Duration::from_secs(60),
        now_ms().unwrap() + 90_000,
    )
    .unwrap()
}

#[test]
fn zero_crop_page_charges_original_copies_admission_and_all_receipts_without_refund() {
    let fixture = Fixture::new();
    let page = fixture.page();
    assert!(page.native_crops.is_empty());
    let original = fixture.task.budget.resources.clone();
    let before = original.staging_used();
    let expected = fixture
        .task
        .images
        .iter()
        .map(|image| image.artifact.bytes)
        .sum::<u64>()
        + 65_536
        + (2 + 8 + 3 * 8) * 16_384;
    let native = NativeOcrSession::prepare(
        &page,
        &fixture.dir.path().join("zero-crop"),
        H,
        tokio::time::Instant::now() + Duration::from_secs(60),
        now_ms().unwrap() + 90_000,
    )
    .unwrap();
    assert_eq!(original.staging_used(), before + expected);
    assert_eq!(native.grants.len(), fixture.task.images.len());
    let charged = original.staging_used();
    drop(native);
    assert_eq!(original.staging_used(), charged);
}

#[test]
fn zero_crop_page_refuses_one_byte_short_before_any_private_persistence() {
    let fixture = Fixture::new();
    let page = fixture.page();
    assert!(page.native_crops.is_empty());
    let original = fixture.task.budget.resources.clone();
    let expected = fixture
        .task
        .images
        .iter()
        .map(|image| image.artifact.bytes)
        .sum::<u64>()
        + 65_536
        + (2 + 8 + 3 * 8) * 16_384;
    original
        .reserve_retained_staging(original.run_staging_bytes - expected + 1)
        .unwrap();
    let before = original.staging_used();
    let raw = fixture.dir.path().join("zero-crop-refused");
    assert!(
        NativeOcrSession::prepare(
            &page,
            &raw,
            H,
            tokio::time::Instant::now() + Duration::from_secs(60),
            now_ms().unwrap() + 90_000
        )
        .is_err()
    );
    assert!(!raw.exists());
    assert_eq!(original.staging_used(), before);
}

fn put(map: &mut BTreeMap<String, (Value, String)>, leaf: &str, value: Value) {
    let digest = hash(&serde_json::to_vec(&value).unwrap());
    map.insert(leaf.into(), (value, digest));
}
fn receipt_set(session: &NativeOcrSession, task: &OcrTask) -> BTreeMap<String, (Value, String)> {
    let mut out = BTreeMap::new();
    put(&mut out, "ready.json", session.ready.clone());
    put(
        &mut out,
        "turn.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"turn_admitted","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"turn_id":"turn","max_turns":1,
            "permissions_profile_sha256":session.manifest["permissions_profile_sha256"]
        }),
    );
    put(
        &mut out,
        "call-1.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"tool_admitted","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"turn_id":"turn","call_id":"context","ordinal":1,
            "native_image":false,"handler_not_yet_executed":true,"argument_parser_not_yet_executed":true
        }),
    );
    put(
        &mut out,
        "call-2.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"tool_admitted","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"turn_id":"turn","call_id":"image","ordinal":2,
            "native_image":true,"handler_not_yet_executed":true,"argument_parser_not_yet_executed":true
        }),
    );
    let grant = &session.grants[0];
    put(
        &mut out,
        "source-2.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"native_source_buffer","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"call_id":"image","ordinal":2,"artifact_id":grant.artifact_id,
            "source_buffer_bytes":grant.bytes,"source_buffer_sha256":grant.sha256,
            "source_width":grant.width,"source_height":grant.height,"rendition_sha256":grant.rendition_sha256
        }),
    );
    put(
        &mut out,
        "prepared-2.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"native_prepared_image","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"call_id":"image","ordinal":2,"artifact_id":grant.artifact_id,
            "source_buffer_sha256":grant.sha256,"prepared_image_bytes":24,"prepared_image_sha256":P,
            "source_width":grant.width,"source_height":grant.height,"prepared_width":2,"prepared_height":3,
            "image_wire_bytes":256,"cumulative_image_wire_bytes":256,"provider_ingestion_verified":false
        }),
    );
    put(
        &mut out,
        "assembled-2.json",
        json!({
            "schema_version":RECEIPT_SCHEMA,"kind":"native_image_assembled","admission_sha256":session.manifest_sha256,
            "task_sha256":task.task_sha256,"call_id":"image","ordinal":2,"artifact_id":grant.artifact_id,
            "source_buffer_sha256":grant.sha256,"reference":{"type":"inline","bytes":24,"sha256":P},
            "prepared_image_sha256":P,"prepared_image_bytes":24,"prepared_width":2,"prepared_height":3,
            "image_content_wire_charge":256,"local_assembly_verified":true,"provider_ingestion_verified":false
        }),
    );
    out
}
fn check(
    session: &NativeOcrSession,
    task: &OcrTask,
    receipts: &BTreeMap<String, (Value, String)>,
) -> Result<Vec<OcrImageDelivery>> {
    validate_receipts(
        &session.manifest,
        &session.manifest_sha256,
        &session.ready,
        &session.grants,
        &session.image_root,
        receipts,
        &BTreeSet::from(["context".into()]),
        &BTreeMap::from([(
            "image".into(),
            session
                .image_root
                .join(&session.grants[0].relative_path)
                .to_str()
                .unwrap()
                .into(),
        )]),
        "turn",
        task,
    )
}

#[test]
fn typed_crop_is_a_static_leaf_without_upstream_wire_or_directory_grant_extension() {
    let fixture = Fixture::new();
    let before = serde_json::to_vec(&fixture.task).unwrap();
    let staging = fixture.task.budget.resources.staging_used();
    let native = crop_session(&fixture);
    assert_eq!(native.grants.len(), 2);
    let rows = native.manifest["images"].as_array().unwrap();
    assert!(rows.iter().all(|row| row.as_object().unwrap().len() == 7
        && row.get("native_crop").is_none()
        && row.get("sealed_file").is_none()));
    let grant = &native.grants[1];
    let crop = grant.native_crop.as_ref().unwrap();
    assert_eq!(
        crop.recipe.parent_artifact_id,
        fixture.task.images[0].artifact.id
    );
    assert_eq!(
        fs::read(native.image_root.join(&grant.relative_path))
            .unwrap()
            .len() as u64,
        crop.png_bytes
    );
    gptgrep_parse::validate_native_crop_proof(&fixture.task, crop).unwrap();
    let permissions = permission_value(&native.image_root, &native.grants);
    let entries = permissions["file_system"]["entries"].as_array().unwrap();
    assert_eq!(
        entries.iter().filter(|row| row["access"] == "read").count(),
        2
    );
    assert!(
        entries
            .iter()
            .filter(|row| row["access"] == "read")
            .all(|row| row["path"]["path"] != json!(native.image_root))
    );
    assert!(
        native.prompt_images()[1]["supplemental_native_crop"]["recipe"]["crop_to_page"].is_object()
    );
    assert_eq!(serde_json::to_vec(&fixture.task).unwrap(), before);
    assert!(fixture.task.budget.resources.staging_used() > staging);
    let charged = fixture.task.budget.resources.staging_used();
    drop(native);
    assert_eq!(fixture.task.budget.resources.staging_used(), charged);
}

#[test]
fn crop_receipt_still_names_original_parent_and_does_not_mark_original_seen() {
    let fixture = Fixture::new();
    let mut native = crop_session(&fixture);
    native.grants.swap(0, 1); // Receipt fixture selects the crop's actual static row.
    let receipts = receipt_set(&native, &fixture.task);
    let deliveries = check(&native, &fixture.task, &receipts).unwrap();
    let delivery = &deliveries[0];
    assert_eq!(delivery.artifact_id, fixture.task.images[0].artifact.id);
    assert_eq!(
        delivery.artifact_sha256,
        fixture.task.images[0].artifact.sha256
    );
    assert!(delivery.native.as_ref().unwrap().native_crop.is_some());
    let mut page = fixture.page();
    page.record_native_delivery(delivery.clone()).unwrap();
    assert!(page.verify_delivery().is_err());
    native.grants.swap(0, 1);
    let original = check(&native, &fixture.task, &receipt_set(&native, &fixture.task)).unwrap();
    assert!(original[0].native.as_ref().unwrap().native_crop.is_none());
    page.record_native_delivery(original[0].clone()).unwrap();
    page.verify_delivery().unwrap();
}

#[test]
fn duplicate_crop_closure_and_logical_working_refuse_before_private_persistence() {
    let fixture = Fixture::new();
    let mut page = fixture.page();
    page.native_crops = vec![fixture_crop(&fixture), fixture_crop(&fixture)];
    let raw = fixture.dir.path().join("duplicate-must-not-exist");
    assert!(
        NativeOcrSession::prepare(
            &page,
            &raw,
            H,
            tokio::time::Instant::now() + Duration::from_secs(60),
            now_ms().unwrap() + 90_000
        )
        .is_err()
    );
    assert!(!raw.exists());
    let mut fixture = Fixture::new();
    fixture.task.budget.resources.owned_working_bytes = 35 * 1024 * 1024;
    fixture.task.task_sha256 = gptgrep_parse::ocr_task_digest(&fixture.task).unwrap();
    fixture.task.id = format!("ocr-{}", fixture.task.task_sha256);
    let mut page = fixture.page();
    page.native_crops.push(fixture_crop(&fixture));
    let raw = fixture.dir.path().join("working-must-not-exist");
    assert!(
        NativeOcrSession::prepare(
            &page,
            &raw,
            H,
            tokio::time::Instant::now() + Duration::from_secs(60),
            now_ms().unwrap() + 90_000
        )
        .is_err()
    );
    assert!(!raw.exists());
}

#[cfg(unix)]
#[test]
fn frozen_crop_rejects_namespace_replacement_same_hash_restore_and_extra_leaf() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let native = crop_session(&fixture);
    native.verify_grants().unwrap();
    let path = native.image_root.join(&native.grants[1].relative_path);
    let bytes = fs::read(&path).unwrap();
    fs::rename(&path, native.image_root.join("old-crop.png")).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(native.verify_grants().is_err());
    let fixture = Fixture::new();
    let native = crop_session(&fixture);
    let path = native.image_root.join(&native.grants[1].relative_path);
    let bytes = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, b"mutation").unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(native.verify_grants().is_err());
    let fixture = Fixture::new();
    let native = crop_session(&fixture);
    fs::write(native.image_root.join("ungranted.png"), b"ungranted").unwrap();
    assert!(native.verify_grants().is_err());
}

#[test]
fn native_contract_requires_fresh_crop_profile_qualification_while_guard_abi_stays_r10() {
    let candidate = profile();
    assert!(!candidate.image_qualification_accepted());
    assert!(INSTRUCTIONS.contains("never a substitute for an original image"));
    assert!(INSTRUCTIONS.contains("never crop coordinates"));
    assert_eq!(MANIFEST_SCHEMA, "gptgrep.native-ocr.admission.v1");
    assert_eq!((FRAME_BYTES, RPC_BYTES), (4 * 1024 * 1024, 8 * 1024 * 1024));
    let mut stale = candidate.clone();
    stale.native_viewer.as_mut().unwrap().guard_contract_sha256 = H.into();
    assert!(stale.validate().is_err());
}

#[test]
fn legacy_profile_wire_and_qualification_are_not_reused() {
    let legacy = OcrRuntimeProfile::unqualified("a".repeat(40), H.into(), H.into()).unwrap();
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("native_viewer")
            .is_none()
    );
    assert_eq!(
        legacy.sha256().unwrap(),
        hash(&serde_json::to_vec(&legacy).unwrap())
    );
    let mut candidate = profile();
    assert!(!candidate.image_qualification_accepted());
    candidate.accepted_image_qualification_sha256 = Some(H.into());
    assert!(candidate.validate().is_err());
    candidate.accepted_image_qualification_sha256 = None;
    let digest = candidate.sha256().unwrap();
    candidate
        .native_viewer
        .as_mut()
        .unwrap()
        .accepted_qualification = Some(NativeOcrQualification {
        schema_version: "gptgrep.ocr.native-qualification.v1".into(),
        runtime_profile_sha256: digest.clone(),
        qualification_sha256: H.into(),
        native_delivery_accepted: true,
        paired_semantic_influence_accepted: true,
        actual_served_profile_verified: false,
    });
    candidate.validate().unwrap();
    assert!(candidate.image_qualification_accepted());
    assert_eq!(candidate.sha256().unwrap(), digest);
    candidate.codex_binary_sha256 = P.into();
    assert!(candidate.validate().is_err());
}

#[test]
fn ready_is_required_and_bound_to_original_deadline_before_any_turn() {
    let fixture = Fixture::new();
    let mut native = session(&fixture);
    assert!(native.initialize(&json!({})).is_err());
    for key in [
        "admission_sha256",
        "runtime_profile_sha256",
        "permissions_profile_sha256",
        "deadline_unix_ms",
        "environment_ids",
        "environment_id_argument_admitted",
    ] {
        let mut wrong = native.ready.clone();
        wrong[key] = json!("changed");
        assert!(
            native
                .initialize(&json!({"gptgrepNativeOcr":wrong}))
                .is_err()
        );
    }
    for key in [
        "tool_counter_before_argument_parse",
        "normalized_request_profile_verified_before_send",
    ] {
        for replacement in [None, Some(json!(false)), Some(json!("changed"))] {
            let mut wrong = native.ready.clone();
            match replacement {
                Some(value) => {
                    wrong[key] = value;
                }
                None => {
                    wrong.as_object_mut().unwrap().remove(key);
                }
            }
            assert!(
                native
                    .initialize(&json!({"gptgrepNativeOcr":wrong}))
                    .is_err()
            );
        }
    }
    let ready = native.ready.clone();
    native
        .initialize(&json!({"gptgrepNativeOcr":ready}))
        .unwrap();
    assert!(
        native
            .initialize(&json!({"gptgrepNativeOcr":native.ready}))
            .is_err()
    );
    assert_eq!(bounded_deadline(1_000, 1_500, 180_000).unwrap(), 1_500);
    assert!(bounded_deadline(1_000, 1_000, 1).is_err());
    assert!(bounded_deadline(1_000, 999, 180_000).is_err());
    assert!(bounded_deadline(1_000, 999_999, 180_001).is_err());
}

#[test]
fn native_item_shape_cannot_be_treated_as_dynamic_delivery_or_grant_extra_paths() {
    let fixture = Fixture::new();
    let mut native = session(&fixture);
    let ready = native.ready.clone();
    native
        .initialize(&json!({"gptgrepNativeOcr":ready}))
        .unwrap();
    let mut calls = BTreeMap::new();
    let path = native.image_root.join(&native.grants[0].relative_path);
    let item = json!({"type":"imageView","id":"image","path":path});
    native.check_item(&item, &mut calls).unwrap();
    native.check_item(&item, &mut calls).unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        native
            .check_item(
                &json!({"type":"dynamicToolCall","id":"image","path":path}),
                &mut calls
            )
            .is_err()
    );
    assert!(
        native
            .check_item(
                &json!({"type":"imageView","id":"image","path":native.receipt_root}),
                &mut calls
            )
            .is_err()
    );
    for n in 2..=8 {
        native
            .check_item(
                &json!({"type":"imageView","id":format!("image-{n}"),"path":path}),
                &mut calls,
            )
            .unwrap();
    }
    assert!(
        native
            .check_item(
                &json!({"type":"imageView","id":"ninth","path":path}),
                &mut calls
            )
            .is_err()
    );
    assert!(
        context_only(&json!({"task_sha256":H,"resource":"image","artifact_id":"a","sha256":H}))
            .is_err()
    );
    assert!(
        context_only(&json!({"task_sha256":H,"resource":"context","artifact_id":null})).is_err()
    );
    context_only(&json!({"task_sha256":H,"resource":"context"})).unwrap();
}

#[test]
fn transformed_payload_is_separate_from_source_and_file_reference_proves_only_assembly() {
    let fixture = Fixture::new();
    let native = session(&fixture);
    let mut receipts = receipt_set(&native, &fixture.task);
    let deliveries = check(&native, &fixture.task, &receipts).unwrap();
    assert_eq!(
        deliveries[0].artifact_sha256,
        fixture.task.images[0].artifact.sha256
    );
    let delivery = deliveries[0].native.as_ref().unwrap();
    assert_ne!(
        delivery.prepared_image_sha256,
        deliveries[0].artifact_sha256
    );
    assert_eq!((delivery.prepared_width, delivery.prepared_height), (2, 3));
    assert_eq!((deliveries[0].width, deliveries[0].height), (1, 1));
    assert!(delivery.local_assembly_verified && !delivery.provider_ingestion_verified);
    let mut assembled = receipts["assembled-2.json"].0.clone();
    assembled["reference"] = json!({"type":"file","reference_sha256":P});
    put(&mut receipts, "assembled-2.json", assembled.clone());
    assert!(check(&native, &fixture.task, &receipts).is_ok());
    assembled["provider_ingestion_verified"] = json!(true);
    put(&mut receipts, "assembled-2.json", assembled);
    assert!(check(&native, &fixture.task, &receipts).is_err());
}

#[test]
fn complete_receipt_join_rejects_missing_duplicate_spurious_and_changed_records() {
    let fixture = Fixture::new();
    let native = session(&fixture);
    let receipts = receipt_set(&native, &fixture.task);
    for leaf in ["call-1.json", "call-2.json"] {
        for replacement in [None, Some(json!(false)), Some(json!("true"))] {
            let mut changed = receipts.clone();
            let mut call = changed[leaf].0.clone();
            match replacement {
                Some(value) => {
                    call["argument_parser_not_yet_executed"] = value;
                }
                None => {
                    call.as_object_mut()
                        .unwrap()
                        .remove("argument_parser_not_yet_executed");
                }
            }
            put(&mut changed, leaf, call);
            assert_eq!(
                check(&native, &fixture.task, &changed)
                    .unwrap_err()
                    .to_string(),
                "ocr_native_call_receipt_changed"
            );
        }
    }
    for leaf in [
        "ready.json",
        "turn.json",
        "call-1.json",
        "call-2.json",
        "source-2.json",
        "prepared-2.json",
        "assembled-2.json",
    ] {
        let mut missing = receipts.clone();
        missing.remove(leaf);
        assert!(check(&native, &fixture.task, &missing).is_err(), "{leaf}");
    }
    let mut duplicate = receipts.clone();
    duplicate.get_mut("call-2.json").unwrap().0["call_id"] = json!("context");
    assert!(check(&native, &fixture.task, &duplicate).is_err());
    for leaf in ["source-1.json", "assembled-8.json", "violation.json"] {
        let mut spurious = receipts.clone();
        put(&mut spurious, leaf, json!({}));
        assert!(check(&native, &fixture.task, &spurious).is_err());
    }
    for (leaf, key) in [
        ("source-2.json", "source_buffer_sha256"),
        ("prepared-2.json", "source_buffer_sha256"),
        ("assembled-2.json", "prepared_image_sha256"),
        ("assembled-2.json", "call_id"),
    ] {
        let mut changed = receipts.clone();
        changed.get_mut(leaf).unwrap().0[key] = json!("changed");
        assert!(
            check(&native, &fixture.task, &changed).is_err(),
            "{leaf}:{key}"
        );
    }
    let mut malformed = receipts;
    malformed.get_mut("prepared-2.json").unwrap().0["cumulative_image_wire_bytes"] = json!(257);
    assert!(check(&native, &fixture.task, &malformed).is_err());
}

#[test]
fn receipt_reader_uses_unique_json_and_rejects_symlinks_and_unlisted_files() {
    let fixture = Fixture::new();
    let native = session(&fixture);
    write_private(&native.receipt_root.join("ready.json"), br#"{"x":1,"x":2}"#).unwrap();
    assert!(native.read_receipts().is_err());
    fs::remove_file(native.receipt_root.join("ready.json")).unwrap();
    write_private(&native.receipt_root.join("violation.json"), b"{}").unwrap();
    assert!(native.read_receipts().is_err());
    #[cfg(unix)]
    {
        fs::remove_file(native.receipt_root.join("violation.json")).unwrap();
        std::os::unix::fs::symlink(
            &native.manifest_path,
            native.receipt_root.join("ready.json"),
        )
        .unwrap();
        assert!(native.read_receipts().is_err());
    }
}

#[test]
fn standalone_command_uses_exact_leaf_permissions_and_separate_native_partition() {
    let fixture = Fixture::new();
    let native = session(&fixture);
    let command = native
        .command(
            Path::new("/selected/app-server"),
            fixture.dir.path(),
            fixture.dir.path(),
            None,
            &fixture.dir.path().join("catalog.json"),
        )
        .unwrap();
    let args = command
        .as_std()
        .get_args()
        .map(|s| s.to_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(&args[..3], &["--listen", "stdio://", "--strict-config"]);
    assert!(!args.iter().any(|arg| arg == "app-server"));
    assert!(
        args.iter()
            .any(|arg| arg == "default_permissions=\"gptgrep_native_ocr\"")
    );
    assert!(args.iter().any(
        |arg| arg.starts_with("permissions.gptgrep_native_ocr.filesystem={ ")
            && arg.contains("\":root\" = \"deny\"")
            && arg.contains("image-1.png\" = \"read\"")
    ));
    assert_eq!(
        native.manifest["image_transport_reserved_bytes"],
        json!(
            fixture
                .task
                .budget
                .resources
                .native_image_limits()
                .unwrap()
                .image_transport_bytes
        )
    );
    assert!(
        RPC_BYTES
            + native.manifest["image_transport_reserved_bytes"]
                .as_u64()
                .unwrap()
            <= fixture.task.budget.transport_bytes
    );
    let mut changed = fixture.task.clone();
    changed.budget.max_tool_calls = 9;
    assert!(validate_task_budget(&changed).is_err());
    changed = fixture.task.clone();
    changed.budget.transport_bytes += 1;
    assert!(validate_task_budget(&changed).is_err());
    let permissions = permission_value(&native.image_root, &native.grants);
    assert_eq!(
        native.environments(),
        json!([{"environmentId":"local","cwd":native.image_root,"runtimeWorkspaceRoots":[]}])
    );
    let entries = permissions["file_system"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e["access"] == "deny"
        || e["path"]["path"] == json!(native.image_root.join(&native.grants[0].relative_path))));
}

#[test]
fn portable_native_metadata_preserves_artifact_binding_and_rejects_mixed_transports() {
    use gptgrep_parse::{
        OcrPageResult, OcrReceipt, OcrRegionResult, OcrRegionStatus, OcrResultStatus,
        ocr_profile_digest, validate_ocr_result_metadata,
    };
    let fixture = Fixture::new();
    let native = session(&fixture);
    let deliveries = check(&native, &fixture.task, &receipt_set(&native, &fixture.task)).unwrap();
    let task = &fixture.task;
    let mut result = OcrPageResult {
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
            text: "fixture".into(),
            reading_order: 0,
            status: OcrRegionStatus::Complete,
        }],
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile).unwrap(),
            request_sha256: H.into(),
            response_sha256: H.into(),
            runtime_profile_sha256: H.into(),
            image_deliveries: deliveries,
            usage: None,
            latency_ms: None,
        },
    };
    validate_ocr_result_metadata(task, &result).unwrap();
    let mut legacy = result.receipt.image_deliveries[0].clone();
    legacy.native = None;
    legacy.tool_ordinal = 3;
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("native")
            .is_none()
    );
    result.receipt.image_deliveries.push(legacy);
    assert!(validate_ocr_result_metadata(task, &result).is_err());
    result.receipt.image_deliveries.pop();
    result.receipt.image_deliveries[0]
        .native
        .as_mut()
        .unwrap()
        .rendition_sha256 = P.into();
    assert!(validate_ocr_result_metadata(task, &result).is_err());
}

#[test]
fn native_and_mediated_events_share_eight_slots_and_cannot_share_ids() {
    let fixture = Fixture::new();
    let mut native = session(&fixture);
    native
        .initialize(&json!({"gptgrepNativeOcr":native.ready.clone()}))
        .unwrap();
    let mediated = (1..=7)
        .map(|n| format!("context-{n}"))
        .collect::<BTreeSet<_>>();
    let mut events = BTreeSet::new();
    let mut native_calls = BTreeMap::new();
    let path = native.image_root.join(&native.grants[0].relative_path);
    let item = json!({"type":"imageView","id":"image","path":path});
    assert!(
        crate::ocr_protocol::check_item_mode(
            &item,
            &mut events,
            &mut native_calls,
            &mediated,
            None,
            8
        )
        .is_err()
    );
    crate::ocr_protocol::check_item_mode(
        &item,
        &mut events,
        &mut native_calls,
        &mediated,
        Some(&native),
        8,
    )
    .unwrap();
    crate::ocr_protocol::check_item_mode(
        &item,
        &mut events,
        &mut native_calls,
        &mediated,
        Some(&native),
        8,
    )
    .unwrap();
    assert_eq!(native_calls.len() + mediated.len(), 8);
    assert!(
        crate::ocr_protocol::check_item_mode(
            &json!({"type":"imageView","id":"ninth","path":path}),
            &mut events,
            &mut native_calls,
            &mediated,
            Some(&native),
            8
        )
        .is_err()
    );
    native_calls.clear();
    assert!(
        crate::ocr_protocol::check_item_mode(
            &json!({"type":"imageView","id":"context-1","path":path}),
            &mut events,
            &mut native_calls,
            &mediated,
            Some(&native),
            8
        )
        .is_err()
    );
}
