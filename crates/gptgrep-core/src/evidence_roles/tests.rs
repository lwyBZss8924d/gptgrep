use super::*;
use gptgrep_jev::{EvidenceRoleCandidate, RankedCandidate};

fn hit_window(index: usize, text: &str) -> Hit {
    Hit {
        path: "invented.txt".into(),
        node_id: "shared-node".into(),
        title: "Invented source".into(),
        line_start: index + 1,
        line_end: index + 1,
        page_start: 1,
        page_end: 1,
        match_line: None,
        match_column: None,
        byte_start: index * 100,
        byte_end: index * 100 + text.len(),
        node_offset: Some(index * 100),
        next_offset: None,
        node_coverage: None,
        column_start: 1,
        coordinate_system: "source_lines".into(),
        text: text.into(),
        text_truncated: false,
        score: 1.,
        confidence: None,
        literal_anchor: false,
        source_sha256: digest(b"invented original"),
        source_fresh: true,
        citation: "invented.txt:L1-L1".into(),
    }
}
fn response(roles: &[(EvidenceRole, f64)]) -> EvidenceRoleResponse {
    EvidenceRoleResponse {
        candidates: roles
            .iter()
            .enumerate()
            .map(|(index, (role, score))| EvidenceRoleCandidate {
                relevance: RankedCandidate {
                    id: format!("c{index}"),
                    score: *score,
                    confidence: None,
                },
                role: *role,
                role_confidence: None,
                role_probabilities: None,
            })
            .collect(),
        model: "typesafe/invented".into(),
        usage: Value::Null,
        provider_response_id: None,
        provider: None,
    }
}
fn prepare(hits: &[Hit]) -> Result<EvidenceRoleDiagnostics> {
    let client = JevClient::new("invented-key", None)?;
    Ok(
        EvidenceRoleDiagnostics::prepare(
            "generation",
            "What connection is stated?",
            hits,
            &client,
        )?
        .1,
    )
}

#[test]
fn role_order_preserves_anchor_precedence_floor_and_stable_span_ties() -> Result<()> {
    use EvidenceRole::*;
    let mut hits = vec![
        hit_window(0, "A related inventory mentions both devices."),
        hit_window(1, "An incomplete arrow points from this device..."),
        hit_window(2, "Cedar supplies Willow after a latch closes."),
        hit_window(3, "Atomic anchor."),
        hit_window(4, "Willow supplies Cedar if the latch stays open."),
    ];
    hits[3].literal_anchor = true;
    let mut diagnostic = prepare(&hits)?;
    assert!(
        diagnostic
            .candidates
            .iter()
            .all(|candidate| candidate.relevance_score.is_none() && candidate.role.is_none())
    );
    diagnostic.observe(&response(&[
        (Background, 0.99),
        (SourceLocalIncomplete, 0.70),
        (DirectSupport, 0.65),
        (NoSupport, 0.01),
        (DirectSupport, 0.49),
    ]))?;
    let mut batch = CandidateBatch {
        hits,
        coverage: Coverage::default(),
        warnings: vec![],
    };
    diagnostic.order(
        &mut batch,
        &SearchOptions {
            limit: 3,
            ..Default::default()
        },
    )?;
    assert_eq!(
        batch
            .hits
            .iter()
            .map(|hit| hit.byte_start)
            .collect::<Vec<_>>(),
        [300, 200, 100, 0]
    );
    assert_eq!(
        diagnostic.candidates[3].eligibility,
        Some(CandidateEligibility::OriginalLiteralAnchor)
    );
    assert_eq!(
        diagnostic.candidates[4].disposition,
        CandidateDisposition::ExcludedRelevanceFloor
    );
    assert_eq!(
        diagnostic.candidates[0].disposition,
        CandidateDisposition::EligibleBelowResultLimit
    );
    assert_eq!(batch.coverage.filtered_candidates, 1);
    assert_eq!(batch.coverage.retained_literal_anchors, 1);
    let hits = vec![hit_window(2, "Tie B"), hit_window(1, "Tie A")];
    let mut diagnostic = prepare(&hits)?;
    diagnostic.observe(&response(&[(DirectSupport, 0.8), (DirectSupport, 0.8)]))?;
    let mut batch = CandidateBatch {
        hits,
        coverage: Coverage::default(),
        warnings: vec![],
    };
    diagnostic.order(&mut batch, &SearchOptions::default())?;
    assert_eq!(batch.hits[0].byte_start, 100);
    Ok(())
}

#[test]
fn clipping_equal_to_another_assessment_cannot_borrow_its_role() -> Result<()> {
    let full = hit_window(
        0,
        "文 Cedar supplies Willow\r\nonly after the latch closes.",
    );
    let mut smaller = full.clone();
    smaller.text = "文 Cedar supplies Willow".into();
    smaller.byte_end = smaller.text.len();
    let mut diagnostic = prepare(&[full.clone(), smaller.clone()])?;
    diagnostic.observe(&response(&[
        (EvidenceRole::SourceLocalIncomplete, 0.9),
        (EvidenceRole::DirectSupport, 0.9),
    ]))?;
    let mut batch = CandidateBatch {
        hits: vec![full.clone(), smaller.clone()],
        coverage: Coverage::default(),
        warnings: vec![],
    };
    diagnostic.order(&mut batch, &SearchOptions::default())?;
    assert!(
        diagnostic
            .hint_for_delivery("generation", &full, &full)
            .is_some()
    );
    assert!(
        diagnostic
            .hint_for_delivery("changed", &full, &full)
            .is_none()
    );
    diagnostic.record_delivery(&full, Some(&smaller))?;
    assert_eq!(
        diagnostic.candidates[0].disposition,
        CandidateDisposition::DeliveredScopeChangedUnassessed
    );
    assert!(
        diagnostic
            .hint_for_delivery("generation", &full, &smaller)
            .is_none()
    );
    assert!(
        diagnostic
            .hint_for_delivery("generation", &smaller, &smaller)
            .is_some()
    );
    let mut changed = smaller.clone();
    changed.text = "changed".into();
    assert!(diagnostic.record_delivery(&full, Some(&changed)).is_err());
    diagnostic.record_delivery(&smaller, None)?;
    assert_eq!(
        diagnostic.candidates[1].disposition,
        CandidateDisposition::SelectedRemovedByEnvelope
    );
    diagnostic.delivery_packet_sha256 = Some(digest(b"escaped packet"));
    diagnostic.mark_delivery_failure();
    assert!(diagnostic.delivery_packet_sha256.is_none());
    assert!(diagnostic.candidates[0].delivered_span.is_none());
    assert_eq!(
        diagnostic.candidates[0].disposition,
        CandidateDisposition::DeliveryFailed
    );
    assert_eq!(
        diagnostic.candidates[1].disposition,
        CandidateDisposition::SelectedRemovedByEnvelope
    );
    diagnostic.validate_bound()?;
    Ok(())
}

#[test]
fn metadata_is_text_free_unknown_until_reply_and_reserved_for_maximum_results() -> Result<()> {
    let hits: Vec<_> = (0..24)
        .map(|index| hit_window(index, "PRIVATE-INVENTED-SOURCE-TEXT not metadata"))
        .collect();
    let mut diagnostic = prepare(&hits)?;
    let encoded = serde_json::to_string(&diagnostic)?;
    assert!(!encoded.contains("PRIVATE-INVENTED-SOURCE-TEXT"));
    assert_eq!(diagnostic.question_count, 48);
    assert_eq!(diagnostic.score_question_count, 24);
    assert_eq!(diagnostic.choice_question_count, 24);
    assert!(diagnostic.reserved_metadata_bytes <= MAX_EVIDENCE_ROLE_METADATA_BYTES);
    assert_eq!(
        serde_json::to_value(&diagnostic)?["candidates"][0]["role_confidence"],
        Value::Null
    );
    let mut observed = response(&vec![(EvidenceRole::SourceLocalIncomplete, 0.9); 24]);
    for candidate in &mut observed.candidates {
        candidate.relevance.confidence = Some(f64::MIN_POSITIVE);
        candidate.role_confidence = Some(f64::MIN_POSITIVE);
        candidate.role_probabilities = Some(
            EvidenceRole::ALL
                .into_iter()
                .map(|role| (role, f64::MIN_POSITIVE))
                .collect(),
        );
    }
    diagnostic.observe(&observed)?;
    let mut batch = CandidateBatch {
        hits: hits.clone(),
        coverage: Coverage::default(),
        warnings: vec![],
    };
    diagnostic.order(
        &mut batch,
        &SearchOptions {
            limit: 24,
            ..Default::default()
        },
    )?;
    for hit in &hits {
        diagnostic.record_delivery(hit, Some(hit))?;
    }
    diagnostic.delivery_packet_sha256 = Some(digest(b"packet"));
    diagnostic.validate_bound()?;
    assert_eq!(
        diagnostic.delivered_set_sufficiency,
        DeliveredSetSufficiency::Unassessed
    );
    let mut oversized = hits;
    for (index, hit) in oversized.iter_mut().enumerate() {
        hit.path = format!("{}-{index}", "path-label".repeat(120));
    }
    let client = JevClient::new("invented-key", None)?;
    let result = EvidenceRoleDiagnostics::prepare("generation", "query", &oversized, &client);
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("metadata exceeded")
    );
    Ok(())
}

#[test]
fn source_freshness_and_failure_preserve_prior_exclusions() -> Result<()> {
    let hits = vec![
        hit_window(0, "direct"),
        hit_window(1, "background"),
        hit_window(2, "below floor"),
    ];
    let mut diagnostic = prepare(&hits)?;
    diagnostic.observe(&response(&[
        (EvidenceRole::DirectSupport, 0.9),
        (EvidenceRole::Background, 0.8),
        (EvidenceRole::DirectSupport, 0.1),
    ]))?;
    let mut batch = CandidateBatch {
        hits,
        coverage: Coverage::default(),
        warnings: vec![],
    };
    diagnostic.order(
        &mut batch,
        &SearchOptions {
            limit: 1,
            ..Default::default()
        },
    )?;
    diagnostic.finish_freshness(&[])?;
    assert_eq!(
        diagnostic.candidates[0].disposition,
        CandidateDisposition::StaleBeforeDelivery
    );
    diagnostic.mark_delivery_failure();
    assert_eq!(
        diagnostic.candidates[1].disposition,
        CandidateDisposition::EligibleBelowResultLimit
    );
    assert_eq!(
        diagnostic.candidates[2].disposition,
        CandidateDisposition::ExcludedRelevanceFloor
    );
    Ok(())
}

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
struct Server {
    client: JevClient,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(direct_fact: String, status: u16) -> Result<Server> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = JevClient::with_endpoint(
        "invented-key",
        None,
        &format!("http://{}/decisions", listener.local_addr()?),
    )?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut buf = [0; 4096];
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
                if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..start]).to_ascii_lowercase();
                    let size = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    if bytes.len() >= start + 4 + size {
                        break bytes[start + 4..start + 4 + size].to_vec();
                    }
                }
            };
            recorded.lock().unwrap().push(body.clone());
            let body: Value = serde_json::from_slice(&body).unwrap();
            let mixed = body["state"].get("role_definitions").is_some();
            if mixed && status == 0 {
                std::future::pending::<()>().await;
            }
            let http_status = if mixed { status } else { 200 };
            let answers:serde_json::Map<_,_>=body["questions"].as_object().unwrap().iter().map(|(id,question)| {
                let index=id.rsplit('_').next().unwrap().parse::<usize>().unwrap();
                let candidate=body["state"]["candidates"][index]["text"].as_str().unwrap_or("");
                let role=if candidate.contains(&direct_fact){"direct_support"}else if candidate.contains("This connector feeds it"){"source_local_incomplete"}else{"background"};
                let value=if question["type"]=="choice" {json!({"type":"choice","choice":role})}
                    else {json!({"type":"score","score":if mixed&&role=="direct_support"{2.4}else{3.0}})};
                (id.clone(),value)
            }).collect();
            let response = if http_status == 200 {
                json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,"usage":{"input_tokens":123,"output_tokens":4},"id":"invented-response","provider":"invented-provider"}).to_string()
            } else {
                "private-error-body".into()
            };
            let wire = format!(
                "HTTP/1.1 {http_status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            let _ = socket.write_all(wire.as_bytes()).await;
        }
    });
    Ok(Server {
        client,
        requests,
        task,
    })
}
async fn source_fixture(
    name: &str,
    eol: &str,
    invert: bool,
) -> Result<(tempfile::TempDir, String, String)> {
    let temp = tempfile::tempdir()?;
    let relation = if invert {
        "Willow supplies Cedar while the latch stays open."
    } else {
        "Cedar supplies Willow after the latch closes."
    };
    let sections = [
        "# Context\nA workshop contains assorted connectors and decorative tiles.",
        "# Inventory\nSeveral devices appear in the inventory near the café.",
        "# Fragment\nThis connector feeds it under the condition described next...",
        relation,
    ];
    let text = format!(
        "{}\n{}\n{}\n# Connection\n{}\n",
        sections[0], sections[1], sections[2], sections[3]
    )
    .replace('\n', eol);
    fs::write(temp.path().join(name), text)?;
    fs::write(
        temp.path().join("unrelated.txt"),
        "outside the selected scope",
    )?;
    let generation = index(temp.path(), 10).await?.generation;
    Ok((temp, generation, relation.into()))
}

#[tokio::test]
async fn opt_in_uses_one_mixed_union_request_with_original_query_and_private_dispositions()
-> Result<()> {
    // Routing-call mechanics: keep the one-document routing request.
    let _route = crate::RouteSingleDocument::enable();
    for (name, eol, invert) in [("alpha.md", "\n", false), ("变式.md", "\r\n", true)] {
        let (temp, generation, relation) = source_fixture(name, eol, invert).await?;
        let server = server(relation.clone(), 200).await?;
        let events = Mutex::new(Vec::<PlannedSearchEvent>::new());
        let query = "  Describe the guarded connection. 文  ";
        let report = search_planned_with_policy_and_observer(
            temp.path(),
            query,
            &["contextual view".into(), "connection constraint".into()],
            &SearchOptions {
                document: Some(name.into()),
                limit: 3,
                ..Default::default()
            },
            &generation,
            &server.client,
            PlannedScoringPolicy::EvidenceRoles,
            &|event| {
                events.lock().unwrap().push(event.clone());
                Ok(())
            },
        )
        .await?;
        assert_eq!(
            (
                report.metrics.jev_calls_attempted,
                report.metrics.jev_requests
            ),
            (4, 4)
        );
        assert!(report.hits[0].text.contains(&relation));
        assert!(report.hits[1].text.contains("This connector feeds it"));
        assert_eq!(report.hits.len(), 3);
        let diagnostic = report.evidence_roles.as_ref().unwrap();
        assert_eq!(diagnostic.question_count, 8);
        assert_eq!(
            (
                diagnostic.score_question_count,
                diagnostic.choice_question_count
            ),
            (4, 4)
        );
        assert_eq!(
            diagnostic
                .candidates
                .iter()
                .filter(|candidate| candidate.disposition
                    == CandidateDisposition::EligibleBelowResultLimit)
                .count(),
            1
        );
        assert_eq!(
            diagnostic.delivered_set_sufficiency,
            DeliveredSetSufficiency::Unassessed
        );
        assert!(
            diagnostic
                .candidates
                .iter()
                .all(|candidate| candidate.role_confidence.is_none())
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let request: Value = serde_json::from_slice(&requests[3])?;
        assert_eq!(request["state"]["query"], query);
        assert_eq!(diagnostic.request_sha256, digest(&requests[3]));
        assert_eq!(diagnostic.request_bytes, requests[3].len());
        assert!(!String::from_utf8_lossy(&requests[3]).contains("unrelated.txt"));
        assert!(!serde_json::to_string(diagnostic)?.contains(&relation));
        let events = events.lock().unwrap();
        let before = events
            .iter()
            .find(|event| event.operation_id == "union.rerank" && event.event == "before_call")
            .unwrap();
        assert!(
            before
                .evidence_roles
                .as_ref()
                .unwrap()
                .candidates
                .iter()
                .all(|candidate| candidate.relevance_score.is_none())
        );
        let after = events
            .iter()
            .find(|event| event.operation_id == "union.rerank" && event.event == "after_reply")
            .unwrap();
        assert_eq!(after.metrics.jev_requests, 1);
        assert!(
            after
                .evidence_roles
                .as_ref()
                .unwrap()
                .candidates
                .iter()
                .all(|candidate| candidate.role.is_some()
                    && candidate.disposition == CandidateDisposition::JudgedAwaitingSelection)
        );
    }
    Ok(())
}

#[tokio::test]
async fn disabled_policy_omits_new_json_and_keeps_score_only_requests() -> Result<()> {
    // Routing-call mechanics: keep the one-document routing request.
    let _route = crate::RouteSingleDocument::enable();
    let (temp, generation, relation) = source_fixture("notes.md", "\n", false).await?;
    let server = server(relation, 200).await?;
    let report = search_planned_with_client_and_observer(
        temp.path(),
        "question",
        &[],
        &SearchOptions {
            document: Some("notes.md".into()),
            ..Default::default()
        },
        &generation,
        &server.client,
        &|_| Ok(()),
    )
    .await?;
    let encoded = serde_json::to_value(report)?;
    assert!(encoded.get("evidence_roles").is_none());
    for event in encoded["operations"].as_array().unwrap() {
        assert!(event.get("evidence_roles").is_none());
    }
    for bytes in server.requests.lock().unwrap().iter() {
        let request: Value = serde_json::from_slice(bytes)?;
        assert!(request["state"].get("role_definitions").is_none());
        assert!(
            request["questions"]
                .as_object()
                .unwrap()
                .values()
                .all(|question| question["type"] == "score")
        );
    }
    Ok(())
}

#[tokio::test]
async fn union_failure_and_reply_observer_failure_retain_disjoint_accounting() -> Result<()> {
    // Routing-call mechanics: keep the one-document routing request.
    let _route = crate::RouteSingleDocument::enable();
    for status in [503, 200] {
        let (temp, generation, relation) = source_fixture("notes.md", "\n", false).await?;
        let server = server(relation, status).await?;
        let error = search_planned_with_policy_and_observer(
            temp.path(),
            "question",
            &[],
            &SearchOptions {
                document: Some("notes.md".into()),
                ..Default::default()
            },
            &generation,
            &server.client,
            PlannedScoringPolicy::EvidenceRoles,
            &|event| {
                if status == 200
                    && event.operation_id == "union.rerank"
                    && event.event == "after_reply"
                {
                    bail!("private-observer-cause");
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
        assert_eq!(failure.metrics.jev_calls_attempted, 2);
        assert_eq!(
            failure.metrics.jev_requests,
            if status == 200 { 2 } else { 1 }
        );
        let union = failure
            .operations
            .iter()
            .find(|event| event.operation_id == "union.rerank")
            .unwrap();
        assert_eq!(union.event, "failed");
        let diagnostic = union.evidence_roles.as_ref().unwrap();
        assert!(
            diagnostic
                .candidates
                .iter()
                .all(|candidate| candidate.role.is_some() == (status == 200))
        );
        assert!(!serde_json::to_string(failure)?.contains("private-observer-cause"));
        assert!(diagnostic.delivery_packet_sha256.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn dropped_mixed_request_retains_attempt_metadata_with_unknown_roles() -> Result<()> {
    // Routing-call mechanics: keep the one-document routing request.
    let _route = crate::RouteSingleDocument::enable();
    let (temp, generation, relation) = source_fixture("notes.md", "\n", false).await?;
    let server = server(relation, 0).await?;
    let events = Mutex::new(Vec::<PlannedSearchEvent>::new());
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let observer = |event: &PlannedSearchEvent| {
        events.lock().unwrap().push(event.clone());
        if event.operation_id == "union.rerank"
            && event.event == "before_call"
            && let Some(sender) = sender.lock().unwrap().take()
        {
            let _ = sender.send(());
        }
        Ok(())
    };
    let options = SearchOptions {
        document: Some("notes.md".into()),
        ..Default::default()
    };
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        tokio::select! {
            result=search_planned_with_policy_and_observer(temp.path(),"question",&[],&options,&generation,&server.client,PlannedScoringPolicy::EvidenceRoles,&observer)=>panic!("unexpected completion: {result:?}"),
            result=receiver=>result.unwrap(),
        }
    }).await?;
    let events = events.lock().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.event, "interrupted");
    assert_eq!(
        (last.metrics.jev_calls_attempted, last.metrics.jev_requests),
        (1, 0)
    );
    assert!(
        last.evidence_roles
            .as_ref()
            .unwrap()
            .candidates
            .iter()
            .all(|candidate| candidate.role.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn source_change_after_mixed_reply_excludes_hits_but_preserves_judgments() -> Result<()> {
    // Routing-call mechanics: keep the one-document routing request.
    let _route = crate::RouteSingleDocument::enable();
    let (temp, generation, relation) = source_fixture("notes.md", "\r\n", false).await?;
    let server = server(relation, 200).await?;
    let report = search_planned_with_policy_and_observer(
        temp.path(),
        "question",
        &[],
        &SearchOptions {
            document: Some("notes.md".into()),
            ..Default::default()
        },
        &generation,
        &server.client,
        PlannedScoringPolicy::EvidenceRoles,
        &|event| {
            if event.operation_id == "union.rerank" && event.event == "after_reply" {
                fs::write(temp.path().join("notes.md"), "Changed local source facts")?;
            }
            Ok(())
        },
    )
    .await?;
    assert!(report.hits.is_empty());
    assert_eq!(report.source_fresh, None);
    assert_eq!(report.metrics.jev_requests, 2);
    assert!(
        report
            .evidence_roles
            .as_ref()
            .unwrap()
            .candidates
            .iter()
            .all(|candidate| candidate.role.is_some()
                && candidate.disposition == CandidateDisposition::StaleBeforeDelivery)
    );
    assert_eq!(report.coverage.stale_files, ["notes.md"]);
    Ok(())
}
