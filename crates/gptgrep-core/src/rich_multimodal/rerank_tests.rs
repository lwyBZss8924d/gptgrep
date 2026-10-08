//! Pure source/custody tests. Injected scores exercise ordering, not provider utility.
use super::*;
use crate::rich_batch::tests::{publish, publish_visual};
use gptgrep_jev::ExecutionContext;
use serde_json::json;
use std::{
    fs,
    time::{Duration, SystemTime},
};

fn client(limit: u32) -> Result<JevClient> {
    Ok(
        JevClient::new("ordinary-dummy-no-network", None)?.with_execution_context(
            ExecutionContext::new(
                "ordinary-rerank-test",
                &"a".repeat(64),
                SystemTime::now() + Duration::from_secs(60),
                limit,
            )?,
        ),
    )
}
fn candidates(reader: &RichGenerationReader) -> Result<Vec<RichRerankCandidate>> {
    reader.manifest()?.documents[0]
        .pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let window = reader.read_text(&p.native_text.id, 0, 128)?;
            Ok(RichRerankCandidate {
                id: format!("output-label-{i}"),
                evidence: window.evidence,
            })
        })
        .collect()
}
fn request(reader: &RichGenerationReader) -> Result<RichRerankRequest> {
    Ok(RichRerankRequest {
        schema_version: REQUEST_SCHEMA.into(),
        generation: RichBatchGeneration::Exact(reader.pin()),
        query: "ordinary evidence question".into(),
        candidates: candidates(reader)?,
        budget: Default::default(),
    })
}
fn text_fixture() -> Result<(tempfile::TempDir, RichGenerationReader)> {
    let root = tempfile::tempdir()?;
    publish(
        root.path(),
        &["stored α evidence\n", "other stored evidence\n"],
    )?;
    let reader = RichGenerationReader::open(root.path())?;
    Ok((root, reader))
}
fn mutate_ref(candidate: &RichRerankCandidate, path: &[&str], value: Value) -> RichRerankCandidate {
    let mut json = serde_json::to_value(&candidate.evidence).unwrap();
    let mut cursor = &mut json;
    for key in &path[..path.len() - 1] {
        cursor = &mut cursor[*key];
    }
    cursor[*path.last().unwrap()] = value;
    RichRerankCandidate {
        id: candidate.id.clone(),
        evidence: serde_json::from_value(json).unwrap(),
    }
}
fn zero(c: &JevClient) -> Result<()> {
    let s = c.execution_context().snapshot()?;
    assert_eq!(s.logical_reservations, 0);
    assert_eq!(s.submission_observations, 0);
    assert_eq!(s.send_start_observations, 0);
    Ok(())
}
#[test]
fn rerank_actual_prepared_body_has_only_source_content_and_score_questions() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let c = client(8)?;
    let r = request(&reader)?;
    let p = prepare_rich_rerank(reader.clone(), c.clone(), r.clone())?;
    assert_eq!(p.generation(), &reader.pin());
    assert_eq!(p.reserved_calls(), 1);
    let wire = p.requests().next().unwrap();
    assert_eq!(wire.body_bytes(), wire.canonical_body().len());
    let body: Value = serde_json::from_slice(wire.canonical_body())?;
    assert_eq!(body["questions"].as_object().unwrap().len(), 2);
    assert!(
        body["questions"]
            .as_object()
            .unwrap()
            .values()
            .all(|q| q["type"] == "score")
    );
    let bytes = std::str::from_utf8(wire.canonical_body())?;
    assert!(bytes.contains("stored α evidence"));
    assert!(r.candidates.iter().all(|v| !bytes.contains(&v.id)));
    assert!(p.report.attempts.is_empty());
    assert!(!p.report.coverage.complete);
    assert_eq!(p.report.coverage.scope, "supplied_candidate_pool_only");
    assert_eq!(
        p.report.citation_status,
        "model_exposure_only_delivery_unqualified"
    );
    zero(&c)
}
#[test]
fn full_native_ref_equality_rejects_every_binding_field_before_submit() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let c = client(8)?;
    let original = request(&reader)?;
    for (path, value) in [
        (vec!["binding", "generation", "generation"], json!("forged")),
        (
            vec!["binding", "generation", "manifest_sha256"],
            json!("b".repeat(64)),
        ),
        (vec!["binding", "document_id"], json!("forged")),
        (vec!["binding", "source_path"], json!("other.pdf")),
        (vec!["binding", "source_sha256"], json!("b".repeat(64))),
        (vec!["binding", "artifact_id"], json!("absent")),
        (vec!["binding", "artifact_sha256"], json!("b".repeat(64))),
        (vec!["binding", "page"], json!(2)),
        (vec!["binding", "line"], json!(2)),
        (vec!["binding", "byte_end"], json!(2049)),
        (vec!["binding", "byte_start"], json!(8)), // middle of α
    ] {
        let mut r = original.clone();
        r.candidates[0] = mutate_ref(&r.candidates[0], &path, value);
        assert!(
            prepare_rich_rerank(reader.clone(), c.clone(), r).is_err(),
            "field {path:?}"
        );
        zero(&c)?;
    }
    let mut r = original;
    r.candidates[0].evidence = match &r.candidates[0].evidence {
        EvidenceRef::NativeText { binding } => EvidenceRef::OcrText {
            binding: binding.clone(),
            region_ids: vec!["forged".into()],
            ocr_profile_sha256: "a".repeat(64),
            ocr_receipt_sha256: "b".repeat(64),
        },
        _ => unreachable!(),
    };
    assert!(prepare_rich_rerank(reader, c.clone(), r).is_err());
    zero(&c)
}
#[test]
fn saved_ocr_lane_resolves_real_region_profile_receipt_and_rejects_lane_forgery() -> Result<()> {
    let root = tempfile::tempdir()?;
    super::super::tests::publish_saved_ocr(root.path())?;
    let reader = RichGenerationReader::open(root.path())?;
    let c = client(8)?;
    let artifact = &reader.manifest()?.documents[0]
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == gptgrep_parse::ArtifactKind::OcrText)
        .unwrap()
        .artifact;
    let evidence = reader.read_text(&artifact.id, 0, 128)?.evidence;
    assert!(matches!(evidence, EvidenceRef::OcrText { .. }));
    let mut r = RichRerankRequest {
        schema_version: REQUEST_SCHEMA.into(),
        generation: RichBatchGeneration::Exact(reader.pin()),
        query: "ordinary".into(),
        candidates: vec![RichRerankCandidate {
            id: "saved".into(),
            evidence,
        }],
        budget: Default::default(),
    };
    assert!(prepare_rich_rerank(reader.clone(), c.clone(), r.clone()).is_ok());
    for (key, value) in [
        ("region_ids", json!(["forged"])),
        ("ocr_profile_sha256", json!("c".repeat(64))),
        ("ocr_receipt_sha256", json!("c".repeat(64))),
    ] {
        let mut bad = r.clone();
        bad.candidates[0] = mutate_ref(&bad.candidates[0], &[key], value);
        assert!(prepare_rich_rerank(reader.clone(), c.clone(), bad).is_err());
    }
    r.candidates[0].evidence = match &r.candidates[0].evidence {
        EvidenceRef::OcrText { binding, .. } => EvidenceRef::NativeText {
            binding: binding.clone(),
        },
        _ => unreachable!(),
    };
    assert!(prepare_rich_rerank(reader, c.clone(), r).is_err());
    zero(&c)
}
#[test]
fn visual_refs_resolve_actual_inline_image_and_reject_geometry_digest_source() -> Result<()> {
    let root = tempfile::tempdir()?;
    publish_visual(root.path(), 1)?;
    let reader = RichGenerationReader::open(root.path())?;
    let visual = reader.visual_reader()?;
    let rendition = &reader.manifest()?.documents[0].pages[0].renditions[0]
        .artifact
        .id;
    let evidence = visual
        .open_rendition(rendition, &ResourceBudget::default())?
        .candidate()
        .evidence
        .clone();
    let c = client(8)?;
    let mut r = request(&reader)?;
    r.candidates.push(RichRerankCandidate {
        id: "opaque-visual-label".into(),
        evidence,
    });
    let p = prepare_rich_rerank(reader.clone(), c.clone(), r.clone())?;
    let body: Value = serde_json::from_slice(p.requests().next().unwrap().canonical_body())?;
    assert_eq!(
        body["state"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p.get("image_url").is_some())
            .count(),
        1
    );
    assert!(
        body["questions"]
            .as_object()
            .unwrap()
            .values()
            .all(|q| q["type"] == "score")
    );
    for (path, value) in [
        (vec!["page"], json!(2)),
        (vec!["region_id"], json!("forged")),
        (vec!["rendition_sha256"], json!("a".repeat(64))),
        (vec!["source_sha256"], json!("a".repeat(64))),
        (vec!["bbox", "x"], json!(1.0)),
        (vec!["page_to_raster", "a"], json!(2.0)),
    ] {
        let mut bad = r.clone();
        bad.candidates[1] = mutate_ref(&bad.candidates[1], &path, value);
        assert!(
            prepare_rich_rerank(reader.clone(), c.clone(), bad).is_err(),
            "field {path:?}"
        );
    }
    zero(&c)
}
#[test]
fn duplicate_ids_refs_and_injected_content_are_schema_refusals() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let original = request(&reader)?;
    for field in [
        "text",
        "title",
        "score",
        "image_url",
        "image_base64",
        "cursor",
        "depends_on",
    ] {
        let mut value = serde_json::to_value(&original)?;
        value["candidates"][0][field] = json!("untrusted");
        assert!(parse_rich_rerank_request(&serde_json::to_vec(&value)?).is_err());
    }
    let mut r = original.clone();
    r.candidates[1].id = r.candidates[0].id.clone();
    assert!(validate_request(&r).is_err());
    let mut r = original;
    r.candidates[1].evidence = r.candidates[0].evidence.clone();
    assert!(validate_request(&r).is_err());
    assert!(parse_rich_rerank_request(br#"{"schema_version":"x","schema_version":"y"}"#).is_err());
    Ok(())
}
#[test]
fn escaped_query_output_and_call_bounds_reject_before_submit() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let c = client(8)?;
    let original = request(&reader)?;
    for budget in [
        RichRerankBudget {
            max_output_bytes: 4096,
            ..Default::default()
        },
        RichRerankBudget {
            max_metadata_bytes: 1,
            ..Default::default()
        },
        RichRerankBudget {
            max_decisions_submissions: 0,
            ..Default::default()
        },
    ] {
        let mut r = original.clone();
        r.budget = budget;
        assert!(prepare_rich_rerank(reader.clone(), c.clone(), r).is_err());
    }
    let mut r = original;
    r.query = "\u{1}".repeat(8192);
    r.budget.max_output_bytes = 40_000;
    assert!(prepare_rich_rerank(reader, c.clone(), r).is_err());
    zero(&c)
}
#[test]
fn current_publish_movement_keeps_prepared_old_pin_and_body() -> Result<()> {
    let (root, reader) = text_fixture()?;
    let c = client(8)?;
    let mut r = request(&reader)?;
    r.generation = RichBatchGeneration::default();
    let opened = open_rich_rerank_reader(root.path(), &r)?;
    let p = prepare_rich_rerank(opened, c.clone(), r)?;
    let old_body = p.requests().next().unwrap().body_sha256().to_owned();
    let old = reader.pin();
    let new = publish(root.path(), &["new indexed source evidence"])?;
    assert_ne!(old, new);
    assert_eq!(p.generation(), &old);
    assert_eq!(p.requests().next().unwrap().body_sha256(), old_body);
    zero(&c)
}
#[tokio::test]
async fn stale_after_preparation_returns_partial_zero_attempt_report_without_http() -> Result<()> {
    let (root, reader) = text_fixture()?;
    let c = client(8)?;
    let r = request(&reader)?;
    let p = prepare_rich_rerank(reader, c.clone(), r)?;
    fs::write(root.path().join("source.pdf"), b"changed")?;
    let e = match p.execute().await {
        Ok(_) => panic!("stale source must fail"),
        Err(e) => e,
    };
    let report = e.report.unwrap();
    assert!(report.attempts.is_empty());
    assert!(report.ranked_candidates.is_empty());
    assert!(!report.coverage.complete);
    zero(&c)
}
#[test]
fn pure_score_application_orders_every_supplied_ref_without_initial_pass() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let c = client(8)?;
    let r = request(&reader)?;
    let mut p = prepare_rich_rerank(reader, c.clone(), r)?;
    let ids: Vec<_> = p.pool.iter().map(|e| e.id.clone()).collect();
    p.apply_scores(vec![
        (ids[1].clone(), 2.0 / 3.0, None),
        (ids[0].clone(), 2.0 / 3.0, Some(0.8)),
    ])?;
    assert_eq!(p.report.ranked_candidates.len(), 2);
    assert!(
        p.report.ranked_candidates[0].candidate_id < p.report.ranked_candidates[1].candidate_id
    );
    assert!(!p.report.coverage.complete);
    assert_eq!(p.report.logical_calls_attempted, 0);
    zero(&c)
}
#[test]
fn unsupported_model_unbound_context_and_expired_context_reject_preparation() -> Result<()> {
    let (_root, reader) = text_fixture()?;
    let r = request(&reader)?;
    let wrong = JevClient::new("ordinary-dummy", Some("typesafe/jev-1.13"))?
        .with_execution_context(client(8)?.execution_context().clone());
    assert!(prepare_rich_rerank(reader.clone(), wrong.clone(), r.clone()).is_err());
    zero(&wrong)?;
    let unbound = JevClient::new("ordinary-dummy", None)?;
    assert!(prepare_rich_rerank(reader.clone(), unbound.clone(), r.clone()).is_err());
    zero(&unbound)?;
    let expired = client(8)?.with_execution_context(
        client(8)?
            .execution_context()
            .clone()
            .with_deadline(Instant::now() + Duration::from_millis(5))?,
    );
    std::thread::sleep(Duration::from_millis(20));
    assert!(prepare_rich_rerank(reader, expired.clone(), r).is_err());
    zero(&expired)
}
