use super::*;
use crate::{HostConfig, QueryPlanConfig, retrieval::Evidence};
use gptgrep_core::{
    NavigationDocumentHints, NavigationHint, NavigationHintOrigin, NavigationJevIdentity,
    NavigationOverlay, NavigationOverlayProducer,
};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const QUESTION: &str = "Where are the orchard sorting instructions?";
const TARGET: &str = "Orchard sorting instructions for the amber baskets.";

async fn fixture(hints: Vec<String>) -> Result<(tempfile::TempDir, NavigationConfig)> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("notes.txt"),
        "Invented raw orchard notes and shelf inventory.\r\n".repeat(hints.len().max(1) * 6),
    )?;
    gptgrep_core::index(directory.path(), 8).await?;
    let binding = gptgrep_core::navigation_overlay_binding(directory.path())?;
    let mut cursor = gptgrep_core::open_navigation_document(directory.path(), "notes.txt")?;
    let identity = cursor.identity().clone();
    let mut windows = vec![];
    while let Some(window) = cursor.next_window(211)? {
        windows.push(window.anchor(format!("anchor-{}", windows.len())));
    }
    let hints = hints
        .into_iter()
        .enumerate()
        .map(|(index, hint)| {
            let id = windows[index % windows.len()].anchor_id.clone();
            NavigationHint {
                origin: NavigationHintOrigin::ModelDerivedNavigationOnly,
                target: NavigationHintTarget::Chunk {
                    anchor_id: id.clone(),
                },
                hint,
                anchor_ids: vec![id],
            }
        })
        .collect();
    let document = NavigationDocumentHints {
        document_id: identity.document_id,
        path: identity.path,
        source_sha256: identity.source_sha256,
        text_sha256: identity.text_sha256,
        text_bytes: identity.text_bytes,
        windows,
        hints,
    };
    let producer = NavigationOverlayProducer {
        model: "synthetic-builder".into(),
        reasoning_effort: "max".into(),
        service_tier: "fast".into(),
        prompt_sha256: "a".repeat(64),
        schema_sha256: "b".repeat(64),
        jev: NavigationJevIdentity {
            requested_model: Some(gptgrep_jev::DEFAULT_MODEL.into()),
            actual_models: vec![],
            logical_calls_attempted: 0,
            validated_responses: 0,
            prompt_sha256: "c".repeat(64),
            schema_sha256: "d".repeat(64),
        },
    };
    let overlay = NavigationOverlay::new(&binding, producer, vec![document])?;
    let publication = gptgrep_core::publish_navigation_overlay(directory.path(), &overlay)?;
    Ok((
        directory,
        NavigationConfig {
            expected_overlay_sha256: publication.artifact_sha256,
            ..Default::default()
        },
    ))
}

#[derive(Clone, Copy)]
enum Reply {
    Ok,
    AllPositive,
    Failure,
    UnknownId,
    Hang,
}
struct Server {
    client: JevClient,
    task: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<Value>>>,
    hashes: Arc<Mutex<Vec<String>>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(replies: Vec<Reply>, reverse_answers: bool) -> Result<Server> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = JevClient::with_endpoint(
        "synthetic-navigation",
        None,
        &format!("http://{}/decisions", listener.local_addr()?),
    )?;
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = Arc::clone(&requests);
    let hashes = Arc::new(Mutex::new(vec![]));
    let seen_hashes = Arc::clone(&hashes);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            let body = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
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
                        break bytes[offset + 4..offset + 4 + length].to_vec();
                    }
                }
            };
            assert!(body.len() <= gptgrep_jev::MAX_REQUEST_BYTES);
            let request: Value = serde_json::from_slice(&body).unwrap();
            let ordinal = {
                let mut seen = seen.lock().unwrap();
                let ordinal = seen.len();
                seen.push(request.clone());
                ordinal
            };
            seen_hashes.lock().unwrap().push(hash(&body));
            let reply = replies.get(ordinal).copied().unwrap_or(Reply::Ok);
            if matches!(reply, Reply::Hang) {
                std::future::pending::<()>().await;
            }
            let mut answers = request["questions"].as_object().unwrap().iter().map(|(id, question)| {
                let answer = match question["type"].as_str().unwrap() {
                    "score" => {
                        let score = if matches!(reply, Reply::AllPositive) { 2.0 } else if let Some(hint) = request["state"]["navigation_hints"][id]["hint"].as_str() {
                            if hint == TARGET { 3.0 } else { 0.0 }
                        } else { 3.0 };
                        json!({"type":"score","score":score})
                    },
                    "choice" => json!({"type":"choice","choice":question["criteria"].as_object().unwrap().keys().next().unwrap()}),
                    _ => json!({"type":"noul","noul":1.0}),
                };
                (id.clone(), answer)
            }).collect::<Vec<_>>();
            if reverse_answers {
                answers.reverse();
            }
            if matches!(reply, Reply::UnknownId) {
                answers[0].0 = "unrequested-hint-id".into();
            }
            let answers = answers
                .iter()
                .map(|(id, answer)| format!("{}:{}", serde_json::to_string(id).unwrap(), answer))
                .collect::<Vec<_>>()
                .join(",");
            let response = format!(
                "{{\"model\":\"openai/gpt-6-luna-decisions-20261006\",\"id\":\"navigation-response-{ordinal}\",\"provider\":\"synthetic\",\"usage\":{{\"input_tokens\":7,\"output_tokens\":2,\"cost\":0.001}},\"answers\":{{{answers}}}}}"
            );
            let status = if matches!(reply, Reply::Failure) {
                503
            } else {
                200
            };
            let raw = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            if socket.write_all(raw.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    Ok(Server {
        client,
        task,
        requests,
        hashes,
    })
}

fn accounting(root: &Path) -> Result<Accounting> {
    let binding = gptgrep_core::navigation_overlay_binding(root)?;
    let accounting = Accounting::create(root, &binding.generation)?;
    accounting.bind_workflow(QUESTION, Some("notes.txt"))?;
    Ok(accounting)
}
async fn navigate(
    root: &Path,
    config: &NavigationConfig,
    client: &JevClient,
    accounting: &Accounting,
    timeout: Duration,
) -> Result<BoundNavigation> {
    let binding = gptgrep_core::navigation_overlay_binding(root)?;
    run(
        NavigationRequest {
            root,
            question: QUESTION,
            generation: &binding.generation,
            document: "notes.txt",
            config,
            deadline: tokio::time::Instant::now() + timeout,
        },
        client,
        accounting,
    )
    .await
}

#[tokio::test]
async fn navigation_full_scan_finds_late_moved_hints_and_merges_unordered_answers() -> Result<()> {
    for target_index in [0, 83] {
        let mut hints = (0..84)
            .map(|index| format!("Shelf topic {index}: {}", "λ quoted \" detail ".repeat(35)))
            .collect::<Vec<_>>();
        hints[target_index] = TARGET.into();
        let (root, config) = fixture(hints).await?;
        let accounting = accounting(root.path())?;
        let server = server(vec![], true).await?;
        let bound = navigate(
            root.path(),
            &config,
            &server.client,
            &accounting,
            Duration::from_secs(10),
        )
        .await?;
        let report = accounting.navigation().unwrap();
        assert_eq!(report.schema_version, "gptgrep.navigation-query.v1");
        assert_eq!(report.status, "completed");
        assert!(report.hint_scan_complete);
        assert_eq!(report.unique_hints, 84);
        assert!(report.batches.len() > 1);
        assert_eq!(
            report
                .batches
                .iter()
                .map(|batch| batch.candidates)
                .sum::<usize>(),
            84
        );
        assert_eq!(bound.packet["hints"][0]["hint"], TARGET);
        assert_eq!(report.selected.len(), 1);
        assert_eq!(report.selected[0].score, 3.0);
        assert_eq!(
            report.attempted_calls,
            server.requests.lock().unwrap().len()
        );
        for (index, request) in server.requests.lock().unwrap().iter().enumerate() {
            assert_eq!(
                report.batches[index].request_sha256,
                server.hashes.lock().unwrap()[index]
            );
            for question in request["questions"].as_object().unwrap().values() {
                assert_eq!(question["criteria"], json!(CRITERIA));
            }
        }
        assert_eq!(accounting.summary().attempted_calls, report.attempted_calls);
        assert_eq!(accounting.summary().initial_status, "not_started");
        let ledger = fs::read_to_string(accounting.path())?;
        assert!(
            ledger.contains("navigation_admitted") && ledger.contains("navigation_after_reply")
        );
        assert!(!ledger.contains(TARGET)); // only hashes/scores, never hint bodies
    }
    Ok(())
}

#[tokio::test]
async fn navigation_declared_call_and_byte_caps_reject_entire_scan_before_send() -> Result<()> {
    let hints = (0..91)
        .map(|index| format!("Topic {index}: {}", "\"λ".repeat(550)))
        .collect();
    let (root, original) = fixture(hints).await?;
    for config in [
        NavigationConfig {
            max_jev_calls: 1,
            ..original.clone()
        },
        NavigationConfig {
            max_jev_request_bytes: 100,
            ..original.clone()
        },
    ] {
        let accounting = accounting(root.path())?;
        let server = server(vec![], false).await?;
        assert!(
            navigate(
                root.path(),
                &config,
                &server.client,
                &accounting,
                Duration::from_secs(10)
            )
            .await
            .is_err()
        );
        assert!(server.requests.lock().unwrap().is_empty());
        let report = accounting.navigation().unwrap();
        assert_eq!(report.status, "failed");
        assert!(!report.hint_scan_complete);
        assert_eq!(report.attempted_calls, 0);
        assert_eq!(accounting.summary().attempted_calls, 0);
    }
    Ok(())
}

#[tokio::test]
async fn navigation_partial_failure_unknown_ids_and_timeout_retain_observed_usage() -> Result<()> {
    let hints = (0..79)
        .map(|index| format!("Topic {index}: {}", "navigation metadata ".repeat(80)))
        .collect();
    let (root, config) = fixture(hints).await?;
    for failure in [Reply::Failure, Reply::UnknownId, Reply::Hang] {
        let accounting = accounting(root.path())?;
        let server = server(vec![Reply::Ok, failure], false).await?;
        let deadline = if matches!(failure, Reply::Hang) {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(10)
        };
        assert!(
            navigate(root.path(), &config, &server.client, &accounting, deadline)
                .await
                .is_err()
        );
        let report = accounting.navigation().unwrap();
        assert_eq!(report.status, "failed");
        assert_eq!(report.attempted_calls, 2);
        assert_eq!(report.requests, 1);
        assert_eq!(report.batches[0].usage.as_ref().unwrap()["input_tokens"], 7);
        assert!(report.batches[1].usage.is_none());
        assert!(report.batches[1].model.is_none());
        assert_eq!(accounting.summary().requests, 1);
        assert_eq!(accounting.summary().unobserved_attempts, 1);
        assert!(!report.hint_scan_complete);
        assert!(report.packet_sha256.is_none());
        assert!(fs::read_to_string(accounting.path())?.contains("navigation-response-0"));
    }
    Ok(())
}

#[tokio::test]
async fn navigation_empty_scan_still_requires_and_executes_initial_jev_search() -> Result<()> {
    let (root, navigation) = fixture(vec![]).await?;
    let server = server(vec![], false).await?;
    let accounting = accounting(root.path())?;
    let mut evidence = Evidence::open(root.path(), None)?;
    let config = HostConfig {
        document: Some("notes.txt".into()),
        query_plan: Some(QueryPlanConfig::default()),
        navigation: Some(navigation.clone()),
        ..Default::default()
    };
    evidence.configure(&config, accounting.clone(), Some(server.client.clone()))?;
    evidence
        .prepare_navigation(
            QUESTION,
            &navigation,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await?;
    let report = accounting.navigation().unwrap();
    assert_eq!(report.status, "completed_empty");
    assert!(report.hint_scan_complete);
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(evidence.initial_payload.is_none());
    assert_eq!(accounting.summary().initial_status, "not_started");
    evidence.bootstrap_planned(QUESTION, &[]).await?;
    assert!(evidence.initial_payload.is_some());
    assert!(!server.requests.lock().unwrap().is_empty());
    assert!(matches!(
        accounting.summary().initial_status.as_str(),
        "reranked" | "filtered_all"
    ));
    Ok(())
}

#[tokio::test]
async fn navigation_packet_is_shared_by_planner_reader_and_has_no_citation_authority() -> Result<()>
{
    let (root, mut navigation) = fixture(vec![TARGET.into()]).await?;
    let tree = gptgrep_core::tree(root.path(), Path::new("notes.txt"))?;
    let id = format!(
        "{}:{}",
        tree["document"]["id"].as_str().unwrap(),
        tree["document"]["nodes"][0]["id"].as_str().unwrap()
    );
    let overlay = gptgrep_core::read_navigation_overlay(root.path())?.unwrap();
    let mut document = overlay
        .selected_document_hints(root.path(), "notes.txt")?
        .unwrap();
    document.hints[0].target = NavigationHintTarget::Node {
        node_id: id.clone(),
    };
    let changed = NavigationOverlay::new(
        overlay.binding(),
        overlay.producer().clone(),
        vec![document],
    )?;
    navigation.expected_overlay_sha256 =
        gptgrep_core::publish_navigation_overlay(root.path(), &changed)?.artifact_sha256;
    let server = server(vec![], false).await?;
    let accounting = accounting(root.path())?;
    let mut evidence = Evidence::open(root.path(), None)?;
    let config = HostConfig {
        document: Some("notes.txt".into()),
        query_plan: Some(QueryPlanConfig::default()),
        navigation: Some(navigation.clone()),
        ..Default::default()
    };
    evidence.configure(&config, accounting.clone(), Some(server.client.clone()))?;
    evidence
        .prepare_navigation(
            QUESTION,
            &navigation,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await?;
    let planner = evidence.prepare_query_plan(QUESTION)?;
    let reader = evidence.reader_state(QUESTION, None)?;
    assert_eq!(planner.state["navigation"], reader["navigation"]);
    assert_eq!(reader["navigation"]["citable"], false);
    assert_eq!(reader["navigation"]["hints"][0]["target"]["node_id"], id);
    assert!(planner.instructions.contains(GUIDANCE));
    assert_eq!(evidence.navigation_guidance(), Some(GUIDANCE));
    assert!(evidence.receipts.is_empty());
    let denied = evidence
        .call(
            "unknown-from-navigation",
            "gptgrep_read",
            json!({"node_id":id}),
        )
        .await?;
    assert_eq!(denied["success"], false);
    assert!(evidence.receipts.last().unwrap().evidence.is_empty());
    assert!(
        evidence
            .finish(
                &json!({"answer":"A claim.","citations":[id],"insufficient_evidence":false})
                    .to_string()
            )
            .is_err()
    );
    let hint_id = reader["navigation"]["hints"][0]["hint_id"]
        .as_str()
        .unwrap();
    assert!(
        evidence
            .finish(
                &json!({"answer":"A claim.","citations":[hint_id],"insufficient_evidence":false})
                    .to_string()
            )
            .is_err()
    );
    fs::write(
        root.path().join("notes.txt"),
        "Source changed after navigation.",
    )?;
    assert!(evidence.reader_state(QUESTION, None).is_err());
    Ok(())
}

#[tokio::test]
async fn navigation_stale_overlay_wrong_sha_and_post_scan_pointer_change_fail_closed() -> Result<()>
{
    let (root, config) = fixture(vec![TARGET.into()]).await?;
    let server = server(vec![], false).await?;
    let wrong = NavigationConfig {
        expected_overlay_sha256: "f".repeat(64),
        ..config.clone()
    };
    assert!(
        navigate(
            root.path(),
            &wrong,
            &server.client,
            &accounting(root.path())?,
            Duration::from_secs(10)
        )
        .await
        .is_err()
    );
    assert!(server.requests.lock().unwrap().is_empty());
    let bound = navigate(
        root.path(),
        &config,
        &server.client,
        &accounting(root.path())?,
        Duration::from_secs(10),
    )
    .await?;
    let pointer = root.path().join(".gptgrep/NAVIGATION.json");
    let original = fs::read(&pointer)?;
    fs::write(&pointer, "{}")?;
    assert!(bound.verify(root.path()).is_err());
    fs::write(&pointer, original)?;
    fs::write(root.path().join("notes.txt"), "Replacement raw notes.")?;
    gptgrep_core::index(root.path(), 8).await?;
    let observed = server.requests.lock().unwrap().len();
    assert!(
        navigate(
            root.path(),
            &config,
            &server.client,
            &accounting(root.path())?,
            Duration::from_secs(10)
        )
        .await
        .is_err()
    );
    assert_eq!(server.requests.lock().unwrap().len(), observed);
    Ok(())
}

#[tokio::test]
async fn navigation_packet_budget_keeps_whole_hints_and_stable_global_ties() -> Result<()> {
    let mut hints = (0..12)
        .map(|index| format!("{index}: {}", "\"".repeat(3900)))
        .collect::<Vec<_>>();
    hints.push("A short, complete navigation hint.".into());
    let (root, config) = fixture(hints).await?;
    let overlay = gptgrep_core::read_navigation_overlay(root.path())?.unwrap();
    let document = overlay
        .selected_document_hints(root.path(), "notes.txt")?
        .unwrap();
    let (candidates, _) = candidates(&document)?;
    let scores: BTreeMap<_, _> = candidates
        .iter()
        .rev()
        .map(|candidate| (candidate.hint_id.clone(), 2.0))
        .collect();
    let accounting = accounting(root.path())?;
    let server = server(vec![], false).await?;
    navigate(
        root.path(),
        &config,
        &server.client,
        &accounting,
        Duration::from_secs(10),
    )
    .await?;
    let mut report = accounting.navigation().unwrap();
    report.selected.clear();
    let first = packet(&mut report, &candidates, &scores)?;
    assert!(serde_json::to_vec(&first)?.len() <= MAX_PACKET_BYTES);
    assert!(report.omitted_by_packet > 0);
    assert!(!first["hints"].as_array().unwrap().is_empty());
    let mut reversed = candidates.clone();
    reversed.reverse();
    let mut second_report = report.clone();
    second_report.selected.clear();
    second_report.omitted_by_packet = 0;
    assert_eq!(first, packet(&mut second_report, &reversed, &scores)?);
    for item in first["hints"].as_array().unwrap() {
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.hint_id == item["hint_id"])
            .unwrap();
        assert_eq!(item["hint"], candidate.hint);
    }
    Ok(())
}

#[test]
fn navigation_is_opt_in_ask_scope_bound_and_planner_does_not_reenter_navigation() {
    let defaults = HostConfig::default();
    assert!(defaults.navigation.is_none());
    let navigation = NavigationConfig {
        expected_overlay_sha256: "a".repeat(64),
        ..Default::default()
    };
    let mut config = HostConfig {
        navigation: Some(navigation),
        ..Default::default()
    };
    assert!(crate::validate_config(&config, QUESTION).is_err());
    config.query_plan = Some(QueryPlanConfig::default());
    assert!(crate::validate_config(&config, QUESTION).is_err());
    config.document = Some("notes.txt".into());
    assert!(crate::validate_config(&config, QUESTION).is_ok());
    let planner = crate::planner_runtime_config(&config, config.query_plan.as_ref().unwrap());
    assert!(planner.navigation.is_none());
    assert!(planner.query_plan.is_none());
}

#[tokio::test]
async fn navigation_eight_hint_cap_counts_all_positive_omissions_without_reselection() -> Result<()>
{
    for count in [10, 70] {
        let hints = (0..count)
            .map(|index| format!("A brief navigation topic {index}."))
            .collect();
        let (root, config) = fixture(hints).await?;
        let server = server(vec![Reply::AllPositive; MAX_BATCHES], true).await?;
        let accounting = accounting(root.path())?;
        let bound = navigate(
            root.path(),
            &config,
            &server.client,
            &accounting,
            Duration::from_secs(10),
        )
        .await?;
        let report = accounting.navigation().unwrap();
        assert_eq!(report.selected.len(), MAX_SELECTED);
        assert_eq!(report.omitted_by_packet, count - MAX_SELECTED);
        assert_eq!(report.unique_hints, count);
        let calls = server.requests.lock().unwrap().len();
        assert_eq!(calls, report.batches.len());
        assert_eq!(report.attempted_calls, calls);
        assert_eq!(
            report
                .batches
                .iter()
                .map(|batch| batch.candidates)
                .sum::<usize>(),
            count
        );
        let overlay = gptgrep_core::read_navigation_overlay(root.path())?.unwrap();
        let document = overlay
            .selected_document_hints(root.path(), "notes.txt")?
            .unwrap();
        let (candidates, _) = candidates(&document)?;
        let expected: Vec<_> = candidates
            .iter()
            .take(MAX_SELECTED)
            .map(|candidate| candidate.hint_id.clone())
            .collect();
        assert_eq!(
            report
                .selected
                .iter()
                .map(|selected| selected.hint_id.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            bound.packet["hints"],
            serde_json::to_value(&candidates[..MAX_SELECTED])?
        );

        // Non-positive hints are excluded by relevance, not by packet capacity.
        // Changing only the last two scores leaves the selected packet identical.
        let scores = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                (
                    candidate.hint_id.clone(),
                    if index < count - 2 { 2.0 } else { 0.0 },
                )
            })
            .collect();
        let mut mixed = report.clone();
        mixed.selected.clear();
        mixed.omitted_by_packet = 0;
        assert_eq!(packet(&mut mixed, &candidates, &scores)?, bound.packet);
        assert_eq!(mixed.omitted_by_packet, count - 2 - MAX_SELECTED);
        assert_eq!(mixed.packet_sha256, report.packet_sha256);
        assert_eq!(server.requests.lock().unwrap().len(), calls);
    }
    Ok(())
}

#[tokio::test]
async fn navigation_cancellation_keeps_durable_partial_observations() -> Result<()> {
    let hints = (0..77)
        .map(|index| format!("Topic {index}: {}", "local descriptor ".repeat(70)))
        .collect();
    let (root, config) = fixture(hints).await?;
    let server = server(vec![Reply::Ok, Reply::Hang], false).await?;
    let accounting = accounting(root.path())?;
    let guard = crate::jev_accounting::AttemptGuard(accounting.clone());
    {
        let future = navigate(
            root.path(),
            &config,
            &server.client,
            &accounting,
            Duration::from_secs(10),
        );
        tokio::pin!(future);
        let observe_second = async {
            while server.requests.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            result = &mut future => panic!("expected hanging second batch, got {}", result.is_ok()),
            observed = tokio::time::timeout(Duration::from_secs(3), observe_second) => observed?,
        }
    }
    drop(guard);
    let report = accounting.navigation().unwrap();
    assert_eq!(report.status, "interrupted");
    assert_eq!(report.attempted_calls, 2);
    assert_eq!(report.requests, 1);
    assert!(report.batches[0].usage.is_some());
    assert!(report.batches[1].usage.is_none());
    assert_eq!(report.batches[1].elapsed_ms, None);
    let lines = fs::read_to_string(accounting.path())?;
    let terminal: Value = serde_json::from_str(lines.lines().last().unwrap())?;
    assert_eq!(terminal["event"], "interrupted");
    assert_eq!(terminal["navigation"]["status"], "interrupted");
    assert_eq!(terminal["attempted_calls"], 2);
    assert_eq!(terminal["requests"], 1);
    assert!(lines.contains("navigation-response-0"));
    Ok(())
}

#[tokio::test]
async fn navigation_full_ledger_reservation_preserves_existing_hard_cap() -> Result<()> {
    let (root, config) = fixture(vec![TARGET.into()]).await?;
    let server = server(vec![], false).await?;
    let observed = accounting(root.path())?;
    navigate(
        root.path(),
        &config,
        &server.client,
        &observed,
        Duration::from_secs(10),
    )
    .await?;
    let mut report = observed.navigation().unwrap();
    report.status = "running".into();
    report.hint_scan_complete = false;
    report.selected.clear();
    report.packet_sha256 = None;
    report.packet_bytes = None;
    report.attempted_calls = 0;
    report.requests = 0;
    report.declared_max_jev_calls = MAX_BATCHES;
    report.declared_max_jev_request_bytes = 8 * 1024 * 1024;
    let mut template = report.batches[0].clone();
    template.status = "prepared".into();
    template.candidates = 64;
    template.model = None;
    template.provider = None;
    template.provider_response_id = None;
    template.usage = None;
    template.elapsed_ms = None;
    template.scores_sha256 = None;
    report.batches = (0..MAX_BATCHES)
        .map(|index| {
            let mut batch = template.clone();
            batch.batch_id = index;
            batch
        })
        .collect();
    report.unique_hints = MAX_BATCHES * 64;
    report.eligible_hints = report.unique_hints;
    report.hints_total = report.unique_hints;
    report.planned_request_bytes = report.batches.iter().map(|batch| batch.request_bytes).sum();
    let unspent = accounting(root.path())?;
    assert!(unspent.navigation_admit(&report).is_err());
    assert_eq!(unspent.summary().attempted_calls, 0);
    assert!(unspent.navigation().is_none());
    Ok(())
}

#[tokio::test]
async fn navigation_disabled_reader_state_and_planner_prompt_remain_unchanged() -> Result<()> {
    let (root, _) = fixture(vec![TARGET.into()]).await?;
    let mut evidence = Evidence::open(root.path(), None)?;
    let config = HostConfig {
        document: Some("notes.txt".into()),
        ..Default::default()
    };
    evidence.configure(
        &config,
        accounting(root.path())?,
        Some(JevClient::with_endpoint(
            "synthetic",
            None,
            "http://127.0.0.1:9/unused",
        )?),
    )?;
    let reader = evidence.reader_state(QUESTION, None)?;
    assert!(reader.get("navigation").is_none());
    assert!(evidence.navigation_guidance().is_none());
    let planner = evidence.prepare_query_plan(QUESTION)?;
    assert!(planner.state.get("navigation").is_none());
    assert!(!planner.instructions.contains(GUIDANCE));
    Ok(())
}
