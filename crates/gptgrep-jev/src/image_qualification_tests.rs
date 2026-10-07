//! Offline syntax/admission tests. None submit requests or initialize host clocks/ledgers.
use super::*;
use crate::{ExecutionContext, MAX_QUESTIONS};
use image::{
    ExtendedColorType, ImageEncoder,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

fn binding(id: &str) -> SourceBinding {
    SourceBinding {
        source_id: "fixture-source".into(),
        unit_id: id.into(),
        source_sha256: "a".repeat(64),
    }
}

fn encoded(format: ImageFormat, rgb: [u8; 3]) -> Vec<u8> {
    let pixels = rgb.repeat(4);
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Png => PngEncoder::new(&mut bytes)
            .write_image(&pixels, 2, 2, ExtendedColorType::Rgb8)
            .unwrap(),
        ImageFormat::Jpeg => JpegEncoder::new(&mut bytes)
            .write_image(&pixels, 2, 2, ExtendedColorType::Rgb8)
            .unwrap(),
        _ => unreachable!(),
    }
    bytes
}

fn image(id: &str) -> ImageEvidence {
    ImageEvidence::from_bytes(binding(id), encoded(ImageFormat::Png, [11, 27, 39])).unwrap()
}

fn input() -> DecisionInput {
    DecisionInput {
        text: vec![TextEvidence {
            source: binding("text"),
            text: "Inspect the attached image.".into(),
        }],
        images: vec![image("image")],
    }
}

fn questions() -> Value {
    json!({
        "decision": {"type":"choice", "instructions":"Select the visible color.",
            "criteria":{"a":"First color","b":"Second color"}},
        "predicate": {"type":"noul", "instructions":"Is a mark visible?"},
        "level": {"type":"score", "instructions":"Assess contrast.",
            "criteria":["No contrast","Some contrast","Strong contrast"]}
    })
}

fn client() -> JevClient {
    JevClient::new("offline-fixture-only", Some("openai/gpt-6-luna-decisions")).unwrap()
}

fn prepare(input: DecisionInput) -> PreparedImageQualification {
    client()
        .prepare_image_qualification(input, questions(), MAX_RESPONSE_BYTES)
        .unwrap()
}

fn assert_no_admission(client: &JevClient) {
    let snapshot = client.execution_context().snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 0);
    assert_eq!(snapshot.send_start_observations, 0);
    assert_eq!(snapshot.submission_observations, 0);
}

#[test]
fn png_and_jpeg_wire_is_flat_and_decodes_to_exact_admitted_bytes() {
    let png = encoded(ImageFormat::Png, [11, 27, 39]);
    let jpeg = encoded(ImageFormat::Jpeg, [71, 83, 97]);
    let mut input = input();
    input.text.push(TextEvidence {
        source: binding("second-text"),
        text: "literal \"quotes\"\n中".into(),
    });
    input.images = vec![
        ImageEvidence::from_bytes(binding("png"), png.clone()).unwrap(),
        ImageEvidence::from_bytes(binding("jpeg"), jpeg.clone()).unwrap(),
    ];
    let client = client();
    let prepared = client
        .prepare_image_qualification(input, questions(), MAX_RESPONSE_BYTES)
        .unwrap();
    let request = prepared.request();
    let wire: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(wire.as_object().unwrap().len(), 4);
    assert_eq!(wire["model"], "openai/gpt-6-luna-decisions");
    assert_eq!(wire["provider"], json!({"allow_fallbacks":false}));
    assert_eq!(wire["questions"], questions());
    assert_eq!(wire["state"][0], "Inspect the attached image.");
    assert_eq!(wire["state"][1], "literal \"quotes\"\n中");
    assert_eq!(wire["state"].as_array().unwrap().len(), 4);
    for (index, media, original) in [(2, "image/png", png), (3, "image/jpeg", jpeg)] {
        let part = &wire["state"][index];
        assert_eq!(part.as_object().unwrap().len(), 2);
        assert_eq!(part["type"], "image_url");
        assert_eq!(part["image_url"].as_object().unwrap().len(), 1);
        let prefix = format!("data:{media};base64,");
        let encoded = part["image_url"]["url"]
            .as_str()
            .unwrap()
            .strip_prefix(&prefix)
            .unwrap();
        assert_eq!(STANDARD.decode(encoded).unwrap(), original);
        let evidence = &prepared.source_input().images[index - 2];
        assert_eq!(evidence.bytes, original);
        assert_eq!(evidence.media_type(), media);
        assert_eq!(evidence.dimensions(), (2, 2));
        assert_eq!(
            evidence.body_sha256(),
            format!("{:x}", Sha256::digest(&original))
        );
        assert_eq!(evidence.source().source_sha256, "a".repeat(64));
    }
    assert_eq!(
        request.body_sha256(),
        format!("{:x}", Sha256::digest(&request.body))
    );
    assert_eq!(request.body_bytes(), request.body.len());
    assert_eq!(request.question_count(), 3);
    assert_no_admission(&client);
}

#[test]
fn same_text_different_images_keep_literal_text_identical_and_change_body_hash() {
    let first = prepare(input());
    let mut second_input = input();
    second_input.images = vec![
        ImageEvidence::from_bytes(
            binding("other-image"),
            encoded(ImageFormat::Png, [97, 83, 71]),
        )
        .unwrap(),
    ];
    let second = prepare(second_input);
    let a: Value = serde_json::from_slice(&first.request().body).unwrap();
    let b: Value = serde_json::from_slice(&second.request().body).unwrap();
    assert_eq!(a["state"][0], b["state"][0]);
    assert_eq!(a["questions"], b["questions"]);
    assert_ne!(a["state"][1], b["state"][1]);
    assert_ne!(
        first.request().body_sha256(),
        second.request().body_sha256()
    );
}

#[test]
fn changing_only_provenance_changes_sidecar_without_changing_wire() {
    let first = prepare(input());
    let mut other = input();
    for text in &mut other.text {
        text.source.source_id = "other-source".into();
        text.source.source_sha256 = "b".repeat(64);
    }
    other.images[0].source.source_id = "other-source".into();
    other.images[0].source.source_sha256 = "b".repeat(64);
    let second = prepare(other);
    assert_eq!(first.request().body, second.request().body);
    assert_ne!(
        first.source_input().text[0].source.source_sha256,
        second.source_input().text[0].source.source_sha256
    );
    assert_eq!(
        first.text_content_sha256(0),
        Some(format!(
            "{:x}",
            Sha256::digest(b"Inspect the attached image.")
        ))
    );
    assert!(first.text_content_sha256(1).is_none());
    let text = String::from_utf8(first.request().body.clone()).unwrap();
    for field in [
        "source_id",
        "unit_id",
        "source_sha256",
        "content_sha256",
        "fixture-source",
    ] {
        assert!(!text.contains(field));
    }
}

#[test]
fn exact_endpoint_and_requested_model_are_required() {
    for model in [
        None,
        Some("typesafe/jev-1.13"),
        Some("openai/gpt-6-luna-decisions-20261006"),
    ] {
        let client = JevClient::new("offline-fixture-only", model).unwrap();
        assert!(
            client
                .prepare_image_qualification(input(), questions(), MAX_RESPONSE_BYTES)
                .is_err()
        );
        assert_no_admission(&client);
    }
    for endpoint in [
        "https://example.com/api/alpha/decisions",
        "https://openrouter.ai/api/v1/decisions",
        "http://127.0.0.1:9/api/alpha/decisions",
    ] {
        let client = JevClient::with_endpoint(
            "offline-fixture-only",
            Some("openai/gpt-6-luna-decisions"),
            endpoint,
        )
        .unwrap();
        assert!(
            client
                .prepare_image_qualification(input(), questions(), MAX_RESPONSE_BYTES)
                .is_err()
        );
        assert_no_admission(&client);
    }
}

#[test]
fn output_capacity_and_default_image_gate_reject_before_admission() {
    let client = client();
    assert!(
        client
            .prepare_image_qualification(input(), questions(), MAX_RESPONSE_BYTES - 1)
            .is_err()
    );
    let error = client
        .prepare_input(input(), questions(), MAX_RESPONSE_BYTES)
        .err()
        .unwrap();
    assert!(error.to_string().contains("unqualified"));
    assert_no_admission(&client);
}

#[test]
fn empty_image_route_duplicate_sources_and_invalid_text_are_rejected() {
    let client = client();
    let mut no_images = input();
    no_images.images.clear();
    assert!(
        client
            .prepare_image_qualification(no_images, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut duplicate = input();
    duplicate.images[0].source.unit_id = "text".into();
    assert!(
        client
            .prepare_image_qualification(duplicate, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut conflict = input();
    conflict.images[0].source.source_sha256 = "b".repeat(64);
    assert!(
        client
            .prepare_image_qualification(conflict, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut empty = input();
    empty.text[0].text = " \n\t".into();
    assert!(
        client
            .prepare_image_qualification(empty, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut bad_binding = input();
    bad_binding.text[0].source.source_sha256 = "not-a-digest".into();
    assert!(
        client
            .prepare_image_qualification(bad_binding, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert_no_admission(&client);
}

#[test]
fn image_file_base64_count_pixels_and_media_caps_are_unchanged() {
    assert_eq!(MAX_IMAGES, 4);
    assert_eq!(MAX_IMAGE_BYTES, 1_048_576);
    assert_eq!(MAX_IMAGE_BASE64_BYTES, 1_398_104);
    assert_eq!(MAX_IMAGE_PIXELS, 8_000_000);
    assert_eq!(MAX_TOTAL_IMAGE_PIXELS, 24_000_000);
    assert_eq!(MAX_IMAGE_PIXEL_BUFFER_BYTES, 33_554_432);
    assert!(ImageEvidence::from_bytes(binding("oversize"), vec![0; MAX_IMAGE_BYTES + 1]).is_err());
    assert!(ImageEvidence::from_bytes(binding("invalid"), vec![0; 16]).is_err());
    assert!(
        ImageEvidence::from_bytes(binding("webp"), b"RIFF\x04\x00\x00\x00WEBP".to_vec()).is_err()
    );
    let client = client();
    let mut count = input();
    count.images = (0..MAX_IMAGES + 1)
        .map(|i| image(&format!("image-{i}")))
        .collect();
    assert!(
        client
            .prepare_image_qualification(count, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut pixels = input();
    pixels.images[0].width = MAX_IMAGE_PIXELS as u32;
    pixels.images[0].height = 2;
    // Immutable fields cannot be forged by external callers. Aggregate admission
    // is nevertheless checked even for a malformed internal fixture.
    pixels
        .images
        .extend((0..3).map(|i| image(&format!("extra-{i}"))));
    pixels.images[1].width = MAX_IMAGE_PIXELS as u32;
    pixels.images[1].height = 2;
    assert!(
        client
            .prepare_image_qualification(pixels, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert_no_admission(&client);
}

#[test]
fn escaped_text_and_question_envelope_rejects_before_base64_body_allocation() {
    let client = client();
    let mut escaped = input();
    escaped.text[0].text = "\"".repeat(MAX_REQUEST_BYTES / 2);
    assert!(escaped.validate().is_ok());
    let error = client
        .prepare_image_qualification(escaped, questions(), MAX_RESPONSE_BYTES)
        .err()
        .unwrap();
    assert!(error.to_string().contains("escaped text/questions"));
    let mut raw = input();
    raw.text[0].text = "x".repeat(MAX_REQUEST_BYTES + 1);
    assert!(
        client
            .prepare_image_qualification(raw, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let huge_question =
        json!({"predicate":{"type":"noul","instructions":"\n".repeat(MAX_REQUEST_BYTES/6 + 1)}});
    // Newline is two JSON bytes; choose an aggregate text/description boundary.
    let mut aggregate = input();
    aggregate.text[0].text = "x".repeat(MAX_REQUEST_BYTES - 256);
    assert!(
        client
            .prepare_image_qualification(aggregate, huge_question, MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert_no_admission(&client);
}

#[test]
fn question_schema_and_64_question_limit_remain_enforced() {
    let client = client();
    for invalid in [
        json!({}),
        json!({"x":{"type":"predicate","instructions":"x"}}),
        json!({"x":{"type":"noul","instructions":"x","extra":true}}),
    ] {
        assert!(
            client
                .prepare_image_qualification(input(), invalid, MAX_RESPONSE_BYTES)
                .is_err()
        );
    }
    let make = |count| {
        Value::Object(
            (0..count)
                .map(|i| (format!("q-{i}"), json!({"type":"noul","instructions":"x"})))
                .collect(),
        )
    };
    assert_eq!(
        client
            .prepare_image_qualification(input(), make(MAX_QUESTIONS), MAX_RESPONSE_BYTES)
            .unwrap()
            .request()
            .question_count(),
        MAX_QUESTIONS
    );
    assert!(
        client
            .prepare_image_qualification(input(), make(MAX_QUESTIONS + 1), MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert_no_admission(&client);
}

#[test]
fn whole_body_writer_rejects_exactly_before_crossing_its_limit() {
    let mut writer = BoundedWriter::buffered(8);
    assert_eq!(writer.write(b"12345678").unwrap(), 8);
    assert!(writer.write(b"9").is_err());
    assert_eq!(writer.bytes, b"12345678");
    assert_eq!(writer.len, 8);
    let mut counter = BoundedWriter::counting(3);
    assert!(counter.write(b"1234").is_err());
    assert_eq!(counter.len, 0);
    assert!(counter.bytes.is_empty());
}

#[test]
fn complete_serializer_small_cap_rejects_without_partial_admission() {
    let client = client();
    let input = input();
    let questions = questions();
    let mut small = BoundedWriter::buffered(32);
    assert!(serialize_qualification(&client, &input, &questions, true, &mut small).is_err());
    assert!(small.len <= 32);
    assert!(small.bytes.len() <= 32);
    assert_no_admission(&client);
}

#[test]
fn maximum_file_and_image_envelope_stays_below_full_body_limit() {
    let mut bytes = encoded(ImageFormat::Png, [11, 27, 39]);
    bytes.resize(MAX_IMAGE_BYTES, 0);
    let mut input = input();
    input.images = (0..MAX_IMAGES)
        .map(|i| ImageEvidence::from_bytes(binding(&format!("image-{i}")), bytes.clone()).unwrap())
        .collect();
    input.text[0].text = "x".repeat(MAX_REQUEST_BYTES - 2048);
    let prepared = prepare(input);
    assert!(prepared.request().body_bytes() > MAX_REQUEST_BYTES);
    assert!(prepared.request().body_bytes() < MAX_MULTIMODAL_REQUEST_BYTES);
    assert!(
        prepared.source_input().images.iter().all(
            |i| i.file_bytes() == MAX_IMAGE_BYTES && i.base64_bytes() == MAX_IMAGE_BASE64_BYTES
        )
    );
}

#[test]
fn prepared_identity_and_shared_execution_are_preserved_without_submission() {
    let context = ExecutionContext::new(
        "offline-codec",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(60),
        2,
    )
    .unwrap();
    let client = client().with_execution_context(context.clone());
    let clone = client.clone();
    let prepared = clone
        .prepare_image_qualification(input(), questions(), MAX_RESPONSE_BYTES)
        .unwrap();
    assert!(Arc::ptr_eq(&prepared.request().identity, &client.identity));
    assert_eq!(prepared.request().endpoint.as_str(), DEFAULT_ENDPOINT);
    let request = prepared.into_request();
    let other = client.clone().with_execution_context(context);
    assert!(!Arc::ptr_eq(&request.identity, &other.identity));
    assert_no_admission(&client);
    assert_no_admission(&other);
}

#[test]
fn explicit_expired_deadline_view_rejects_without_admission() {
    let context = ExecutionContext::new(
        "offline-codec",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(60),
        2,
    )
    .unwrap();
    let expired = context
        .with_deadline(std::time::Instant::now() + Duration::from_millis(100))
        .unwrap();
    let client = client().with_execution_context(expired);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        client
            .prepare_image_qualification(input(), questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert_no_admission(&client);
}

#[test]
fn png_header_pixel_limit_precedes_decoder_and_aggregate_bound_is_preserved() {
    // Encode genuine bounded fixtures; over-limit rejection names header admission,
    // rather than treating a failed full decode as the pixel-limit test oracle.
    let png = |width| {
        let pixels = vec![23; width as usize * 2000 * 3];
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, 2000, ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    };
    let exact = ImageEvidence::from_bytes(binding("eight-mp"), png(4000)).unwrap();
    assert_eq!(exact.dimensions(), (4000, 2000));
    let error = ImageEvidence::from_bytes(binding("over-eight-mp"), png(4001))
        .err()
        .unwrap();
    assert!(error.to_string().contains("pixel limit"));
    let mut input = DecisionInput {
        text: vec![],
        images: vec![],
    };
    for i in 0..3 {
        let mut image = exact.clone();
        image.source.unit_id = format!("exact-{i}");
        input.images.push(image);
    }
    assert!(input.validate().is_ok());
    let mut fourth = exact;
    fourth.source.unit_id = "exact-fourth".into();
    input.images.push(fourth);
    assert!(
        input
            .validate()
            .err()
            .unwrap()
            .to_string()
            .contains("aggregate pixel")
    );
}

#[test]
fn ordinary_typed_text_wire_and_direct_preparation_are_unchanged() {
    let client = client();
    let mut text = input();
    text.images.clear();
    let expected = json!({"evidence":[{"source":text.text[0].source,"text":text.text[0].text,
        "content_sha256":format!("{:x}",Sha256::digest(text.text[0].text.as_bytes()))}]});
    let direct = client.prepare_decision(expected, questions()).unwrap();
    let typed = client
        .prepare_input(text, questions(), MAX_RESPONSE_BYTES)
        .unwrap();
    assert_eq!(direct.body, typed.body);
    assert_eq!(direct.body_sha256(), typed.body_sha256());
    assert!(Arc::ptr_eq(&direct.identity, &typed.identity));
    assert_no_admission(&client);
}

#[test]
fn refusal_unknown_and_wrong_answer_types_stay_fail_closed() {
    for response in [
        json!({"model":"actual","answers":{"predicate":{"type":"refusal","reason":"unknown"}}}),
        json!({"model":"actual","answers":{"predicate":{"type":"noul","noul":"UNKNOWN"}}}),
        json!({"model":"actual","answers":{"predicate":{"type":"score","score":0}}}),
    ] {
        let questions = json!({"predicate":{"type":"noul","instructions":"x"}});
        assert!(
            crate::validation::response(&serde_json::to_vec(&response).unwrap(), &questions)
                .is_err()
        );
    }
}
