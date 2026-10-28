use super::*;
use crate::{HostConfig, QueryPlanConfig, jev_accounting::Accounting, retrieval::Evidence};
use gptgrep_core::{
    CandidateDisposition, CandidateEligibility, DeliveredSetSufficiency, EvidenceRole,
    EvidenceRoleCandidateDecision, EvidenceRoleDiagnostics, EvidenceRoleSource, Hit,
};
use gptgrep_jev::JevClient;
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) const QUESTION: &str = "Which material lines the copper container?";

pub(crate) async fn source_hit(text: &str) -> (tempfile::TempDir, String, Hit) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("evidence.txt"), text).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let tree = gptgrep_core::tree(root.path(), Path::new("evidence.txt")).unwrap();
    let id = format!(
        "{}:{}",
        tree["document"]["id"].as_str().unwrap(),
        tree["document"]["nodes"][0]["id"].as_str().unwrap()
    );
    let mut hit = gptgrep_core::read_node(root.path(), &id, 65536).unwrap();
    hit.score = 0.75;
    hit.confidence = Some(0.31);
    (root, tree["generation"].as_str().unwrap().into(), hit)
}

pub(crate) fn diagnostics(generation: &str, hits: &[Hit]) -> EvidenceRoleDiagnostics {
    let mut sources = vec![];
    let candidates = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let source = EvidenceRoleSource {
                path: hit.path.clone(),
                source_sha256: hit.source_sha256.clone(),
            };
            let source_index = sources
                .iter()
                .position(|prior| prior == &source)
                .unwrap_or_else(|| {
                    sources.push(source);
                    sources.len() - 1
                });
            EvidenceRoleCandidateDecision {
                candidate_id: format!("c{index}"),
                source_index,
                byte_start: hit.byte_start,
                byte_end: hit.byte_end,
                text_sha256: crate::retrieval::hash(hit.text.as_bytes()),
                literal_anchor: hit.literal_anchor,
                relevance_score: Some(hit.score),
                relevance_confidence: hit.confidence,
                role: Some(EvidenceRole::DirectSupport),
                role_confidence: Some(0.93),
                role_probabilities: None,
                eligibility: Some(CandidateEligibility::ScoreFloor),
                selection_rank: Some(index + 1),
                disposition: CandidateDisposition::AwaitingDelivery,
                delivered_span: None,
            }
        })
        .collect();
    EvidenceRoleDiagnostics {
        strategy: "jev-evidence-role-v1".into(),
        generation: generation.into(),
        original_question_sha256: crate::retrieval::hash(QUESTION.as_bytes()),
        decision_contract_sha256: "a".repeat(64),
        request_sha256: "b".repeat(64),
        request_bytes: 2048,
        question_count: hits.len() * 2,
        score_question_count: hits.len(),
        choice_question_count: hits.len(),
        reserved_metadata_bytes: 24 * 1024,
        delivered_set_sufficiency: DeliveredSetSufficiency::Unassessed,
        delivery_packet_sha256: None,
        sources,
        candidates,
    }
}

#[tokio::test]
async fn exact_window_hints_keep_confidences_separate_and_clips_unassessed() {
    let source = "甲🙂 copper container uses ceramic \"only when dry\".\r\n";
    let (root, generation, hit) = source_hit(source).await;
    let original = diagnostics(&generation, std::slice::from_ref(&hit));
    validate_policy(&original, &generation, QUESTION).unwrap();
    let exact = delivered_hit(&original, &generation, &hit, &hit).unwrap();
    assert_eq!(exact["evidence_role"], "direct_support");
    assert_eq!(exact["role_assessment_status"], "assessed");
    assert_eq!(exact["confidence"], 0.31);
    assert!(exact.get("role_confidence").is_none());
    assert!(!exact.to_string().contains("0.93"));
    for (start, end) in [
        (3, source.len()),
        (0, source.find("only").unwrap()),
        (source.len(), source.len()),
    ] {
        let clipped =
            gptgrep_core::read_node_window(root.path(), &hit.node_id, (end - start).max(1), start)
                .unwrap();
        let mut assessment = original.clone();
        assessment.record_delivery(&hit, Some(&clipped)).unwrap();
        let value = delivered_hit(&assessment, &generation, &hit, &clipped).unwrap();
        assert!(value.get("evidence_role").is_none());
        assert_eq!(
            value["role_assessment_status"],
            "unavailable_for_delivered_span"
        );
        assert!(value["confidence"].is_null() && value["score"].is_null());
        assert_eq!(
            assessment.candidates[0].disposition,
            CandidateDisposition::DeliveredScopeChangedUnassessed
        );
        assert_eq!(assessment.candidates[0].role_confidence, Some(0.93));
        let span = assessment.candidates[0].delivered_span.as_ref().unwrap();
        assert_eq!(
            (span.byte_start, span.byte_end),
            (clipped.byte_start, clipped.byte_end)
        );
        assert_eq!(
            span.text_sha256,
            crate::retrieval::hash(clipped.text.as_bytes())
        );
    }
    let partial = gptgrep_core::read_node_window(root.path(), &hit.node_id, 20, 3).unwrap();
    assert!(partial.text_truncated && !partial.node_coverage.unwrap().complete);
    let partial_assessment = diagnostics(&generation, std::slice::from_ref(&partial));
    assert_eq!(
        delivered_hit(&partial_assessment, &generation, &partial, &partial).unwrap()["evidence_role"],
        "direct_support"
    );
    let mut changed = hit.clone();
    changed.text = changed.text.replace("dry", "wet");
    assert!(
        delivered_hit(&original, &generation, &hit, &changed)
            .unwrap()
            .get("evidence_role")
            .is_none()
    );
    assert!(
        original
            .clone()
            .record_delivery(&hit, Some(&changed))
            .is_err()
    );
    assert!(
        delivered_hit(&original, "another-generation", &hit, &hit)
            .unwrap()
            .get("evidence_role")
            .is_none()
    );
}

#[tokio::test]
async fn clipping_cannot_borrow_another_assessed_window_or_mutate_private_identity() {
    let (root, generation, hit) = source_hit("Copper container: ceramic only when dry.\n").await;
    let clipped = gptgrep_core::read_node_window(root.path(), &hit.node_id, 18, 18).unwrap();
    let mut assessment = diagnostics(&generation, &[hit.clone(), clipped.clone()]);
    assert!(assessment.hint_for_hit(&generation, &clipped).is_some());
    assessment.record_delivery(&hit, Some(&clipped)).unwrap();
    assert!(
        delivered_hit(&assessment, &generation, &hit, &clipped)
            .unwrap()
            .get("evidence_role")
            .is_none()
    );
    assert_eq!(
        assessment.candidates[1].disposition,
        CandidateDisposition::AwaitingDelivery
    );
    for field in ["path", "source"] {
        let mut changed = clipped.clone();
        if field == "path" {
            changed.path = "other.txt".into();
        } else {
            changed.source_sha256 = "f".repeat(64);
        }
        assert!(
            assessment
                .clone()
                .record_delivery(&hit, Some(&changed))
                .is_err()
        );
    }
}

pub(crate) async fn mock_roles(count: usize) -> (JevClient, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = JevClient::with_endpoint(
        "synthetic-role-test",
        None,
        &format!(
            "http://{}/api/alpha/decisions",
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for ordinal in 0..count {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = vec![];
            let request: Value = loop {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
                assert!(bytes.len() < 128 * 1024);
                if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break serde_json::from_slice(&bytes[offset + 4..offset + 4 + length])
                            .unwrap();
                    }
                }
            };
            let answers: BTreeMap<_,_> = request["questions"].as_object().unwrap().iter().map(|(id, question)| {
                let answer = if question["type"] == "choice" {
                    json!({"type":"choice","choice":"direct_support","confidence":0.93,
                        "probabilities":{"direct_support":0.7,"source_local_incomplete":0.1,"background":0.1,"no_support":0.1}})
                } else {json!({"type":"score","score":3.0,"confidence":0.31})};
                (id.clone(), answer)
            }).collect();
            requests.push(request);
            let body = json!({"model":"openai/gpt-6-luna-decisions-20261006","provider":"fixture","id":format!("reply-{ordinal}"),"answers":answers,
                "usage":{"input_tokens":19,"output_tokens":7,"cost":0.001}}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (client, server)
}

#[tokio::test]
async fn role_packet_keeps_private_omissions_out_of_context_and_preserves_exact_citations() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..5 {
        std::fs::write(
            root.path().join(format!("record-{index}.txt")),
            format!(
                "Copper container material {} \"qualifier-{index}\".\n",
                "\"".repeat(1800)
            ),
        )
        .unwrap();
    }
    gptgrep_core::index(root.path(), 10).await.unwrap();
    for enabled in [false, true] {
        let (client, server) = mock_roles(2).await;
        let mut evidence = Evidence::open(root.path(), None).unwrap();
        let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
        let config = HostConfig {
            query_plan: Some(QueryPlanConfig {
                evidence_roles: enabled,
                ..Default::default()
            }),
            ..Default::default()
        };
        evidence
            .configure(&config, accounting.clone(), Some(client))
            .unwrap();
        accounting.bind_workflow(QUESTION, None).unwrap();
        evidence.bootstrap_planned(QUESTION, &[]).await.unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        let packet = evidence.initial_payload.as_ref().unwrap();
        assert!(packet.get("evidence_roles").is_none() && packet.get("operations").is_none());
        let envelope =
            json!({"contentItems":[{"type":"inputText","text":packet.to_string()}],"success":true});
        assert!(serde_json::to_vec(&envelope).unwrap().len() <= crate::retrieval::MAX_TOOL_BYTES);
        let search = &accounting.summary().searches[0];
        assert_eq!(search.metrics.jev_requests, 2);
        let roles = search.plan.as_ref().unwrap().evidence_roles.as_ref();
        assert_eq!(roles.is_some(), enabled);
        if enabled {
            let roles = roles.unwrap();
            assert_eq!(roles.candidates.len(), 5);
            assert_eq!(
                (
                    roles.score_question_count,
                    roles.choice_question_count,
                    roles.question_count
                ),
                (5, 5, 10)
            );
            assert_eq!(
                roles.request_sha256,
                crate::retrieval::hash(&serde_json::to_vec(&requests[1]).unwrap())
            );
            assert_eq!(
                roles.request_bytes,
                serde_json::to_vec(&requests[1]).unwrap().len()
            );
            assert_eq!(
                roles.delivery_packet_sha256.as_ref().unwrap(),
                &evidence.receipts[0].output_sha256
            );
            assert_eq!(
                packet["host_delivery"]["delivered_set_sufficiency"],
                "unassessed"
            );
            assert!(
                roles
                    .candidates
                    .iter()
                    .any(|candidate| candidate.disposition
                        == CandidateDisposition::EligibleBelowResultLimit)
            );
            assert!(
                roles
                    .candidates
                    .iter()
                    .any(|candidate| candidate.disposition
                        == CandidateDisposition::SelectedRemovedByEnvelope)
            );
            assert!(
                search
                    .plan
                    .as_ref()
                    .unwrap()
                    .operations
                    .values()
                    .all(|operation| operation.evidence_roles.is_none())
            );
            for hit in packet["hits"].as_array().unwrap() {
                assert_eq!(hit["evidence_role"], "direct_support");
                assert_eq!(hit["confidence"], 0.31);
                assert!(
                    hit.get("role_confidence").is_none() && hit.get("role_probabilities").is_none()
                );
            }
            let private = serde_json::to_value(roles).unwrap();
            assert!(
                private["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|record| record.get("text").is_none() && record.get("title").is_none())
            );
            assert!(serde_json::to_vec(roles).unwrap().len() <= 24 * 1024);
        } else {
            assert!(
                packet["host_delivery"]
                    .get("delivered_set_sufficiency")
                    .is_none()
            );
            assert!(
                packet["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|hit| hit.get("evidence_role").is_none())
            );
            assert!(
                requests[1]["questions"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|question| question["type"] == "score")
            );
        }
        let issued = evidence.receipts[0]
            .evidence
            .iter()
            .map(|citation| citation.node_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for index in 0..5 {
            let tree =
                gptgrep_core::tree(root.path(), Path::new(&format!("record-{index}.txt"))).unwrap();
            let id = format!(
                "{}:{}",
                tree["document"]["id"].as_str().unwrap(),
                tree["document"]["nodes"][0]["id"].as_str().unwrap()
            );
            if !issued.contains(&id) {
                let answer=json!({"answer":"An omitted window is not evidence.","citations":[id],"insufficient_evidence":false}).to_string();
                assert!(evidence.finish(&answer).is_err());
                assert_eq!(
                    evidence
                        .call("omitted", "gptgrep_read", json!({"node_id":id}))
                        .await
                        .unwrap()["success"],
                    false
                );
            }
        }
        let answer=json!({"answer":"The returned windows are available.","citations":issued,"insufficient_evidence":false}).to_string();
        let (_, citations, _) = evidence.finish(&answer).unwrap();
        for citation in citations {
            let replay = gptgrep_core::read_node_window(
                root.path(),
                &citation.node_id,
                citation.byte_end - citation.byte_start,
                citation.node_offset,
            )
            .unwrap();
            assert_eq!(
                citation.excerpt_sha256,
                crate::retrieval::hash(replay.text.as_bytes())
            );
            assert_eq!(citation.node_coverage, replay.node_coverage);
        }
    }
}
