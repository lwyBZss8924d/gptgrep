use super::*;
use crate::jev_accounting::Accounting;
use gptgrep_jev::JevClient;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::AsyncReadExt;

pub(crate) async fn corpus() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("pipeline-notes.md"),"# Token pipeline\n\n## Assembly\nEmpty records are removed before the remaining tokens are joined.\n").unwrap();
    std::fs::write(
        root.path().join("neighbor-guide.md"),
        "# Materials\n\n## Surface care\nApply a soft brush before storing the panels.\n",
    )
    .unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    root
}

pub(crate) async fn mock_jev(
    plan: Vec<(u16, f64)>,
) -> (
    JevClient,
    tokio::task::JoinHandle<Vec<Value>>,
    Arc<AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let client = JevClient::with_endpoint("synthetic-host-test-key", None, &endpoint).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let received = count.clone();
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for (ordinal, (status, score)) in plan.into_iter().enumerate() {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = vec![];
            let request = loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                assert!(bytes.len() < 128 * 1024);
                if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break serde_json::from_slice::<Value>(
                            &bytes[offset + 4..offset + 4 + length],
                        )
                        .unwrap();
                    }
                }
            };
            requests.push(request.clone());
            received.fetch_add(1, Ordering::SeqCst);
            if status == 0 {
                std::future::pending::<()>().await;
            }
            let body = if status == 200 {
                let answers: serde_json::Map<_, _> = request["questions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            json!({"type":"score","score":score,"confidence":0.9}),
                        )
                    })
                    .collect();
                json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,
                    "usage":{"input_tokens":100+ordinal,"output_tokens":2,"cost":0.001*(ordinal+1) as f64}}).to_string()
            } else {
                "private-provider-body-sentinel".into()
            };
            let wire = format!(
                "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(wire.as_bytes()).await.unwrap();
        }
        requests
    });
    (client, server, count)
}
fn ledger_events(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn first_excess_request_can_finalize_seed_evidence_without_more_jev_calls() {
    let root = corpus().await;
    // One scoped document: the seed needs only the rerank call.
    let (client, server, requests) = mock_jev(vec![(200, 3.0)]).await;
    let config = HostConfig {
        max_tool_calls: 1,
        document: Some("pipeline-notes.md".into()),
        ..HostConfig::default()
    };
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&config, accounting.clone(), Some(client))
        .unwrap();
    evidence
        .bootstrap("How are tokens assembled?")
        .await
        .unwrap();
    server.await.unwrap();
    let id = evidence.initial_payload.as_ref().unwrap()["hits"][0]["node_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (client, wire) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(wire);
    let rpc = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake(&mut reader, &mut server_writer).await;
        call(
            &mut reader,
            &mut server_writer,
            10,
            "gptgrep_catalog",
            json!({}),
        )
        .await;
        send(
            &mut server_writer,
            json!({"id":11,"method":"item/tool/call","params":{
                "threadId":"thread-native","turnId":"turn-native","callId":"call-11",
                "namespace":"gptgrep","tool":"gptgrep_search",
                "arguments":{"query":"How are tokens assembled?","mode":"hybrid"}
            }}),
        )
        .await;
        let response = receive(&mut reader).await;
        assert_eq!(response["id"], 11);
        assert_eq!(response["result"]["success"], false);
        let payload: Value = serde_json::from_str(
            response["result"]["contentItems"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"], "tool_budget_exhausted");
        assert_eq!(payload["tool_budget"]["admitted_tool_calls"], 1);
        let answer = json!({"answer":"Empty records are removed before the remaining tokens are joined.",
            "citations":[id],"insufficient_evidence":false});
        send(&mut server_writer, json!({"method":"turn/completed","params":{
            "threadId":"thread-native","turn":{"id":"turn-native","status":"completed",
                "items":[{"type":"agentMessage","phase":"final_answer","text":answer.to_string()}]}
        }})).await;
        let mut tail = String::new();
        assert_eq!(
            reader.read_line(&mut tail).await.unwrap(),
            0,
            "Unexpected client retry or new turn"
        );
    });
    let outcome = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "How are tokens assembled?",
        None,
        &config,
        &mut evidence,
    )
    .await
    .unwrap();
    rpc.await.unwrap();
    let (_, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert!(!insufficient);
    assert!(!citations.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(accounting.summary().requests, 1);
    assert_eq!(accounting.summary().attempted_calls, 1);
    assert_eq!(accounting.summary().searches.len(), 1);
    assert_eq!(evidence.receipts.len(), 3);
    assert!(evidence.receipts[0].required_initial);
    assert!(evidence.receipts[1].success);
    assert!(evidence.receipts[1].tool_budget.is_none());
    let denied = &evidence.receipts[2];
    assert!(!denied.success);
    assert!(denied.evidence.is_empty());
    assert!(denied.search.is_none());
    assert_eq!(denied.tool_budget.unwrap().denied_tool_calls, 1);
    let events = ledger_events(&accounting.path());
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "receipt")
            .count(),
        3
    );
    assert!(
        events
            .iter()
            .any(|event| event["receipt"]["tool_budget"]["denied_tool_calls"] == 1)
    );
    let forged =
        json!({"answer":"Claim", "citations":["never-issued"],"insufficient_evidence":false});
    assert!(evidence.finish(&forged.to_string()).is_err());
    std::fs::write(root.path().join("pipeline-notes.md"), "Changed source").unwrap();
    assert!(evidence.finish(&outcome.answer).is_err());
}

#[tokio::test]
async fn initial_jev_pass_is_query_bound_scoped_and_delivered_to_codex() {
    let root = corpus().await;
    let (client, server, _) = mock_jev(vec![(200, 3.0)]).await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    let config = HostConfig {
        document: Some("pipeline-notes.md".into()),
        ..HostConfig::default()
    };
    evidence
        .configure(&config, accounting.clone(), Some(client))
        .unwrap();
    let question = "Which transformation removes blank records before assembly?";
    evidence.bootstrap(question).await.unwrap();
    // The scope leaves one routing candidate, selected without a routing call.
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["state"]["query"], question);
    let candidates = requests[0]["questions"].as_object().unwrap();
    assert!(!candidates.is_empty());
    assert!(candidates.values().all(|question| {
        question["instructions"]["candidate"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Document: pipeline-notes.md\n")
    }));
    let seed = evidence.initial_payload.as_ref().unwrap();
    assert_eq!(seed["query"], question);
    assert_eq!(seed["document_scope"], "pipeline-notes.md");
    assert!(
        seed["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["path"] == "pipeline-notes.md")
    );
    let id = seed["hits"][0]["node_id"].as_str().unwrap().to_owned();
    let (client, wire) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(wire);
    let rpc = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        let turn = handshake_mode(&mut reader, &mut server_writer, true).await;
        let input: Value =
            serde_json::from_str(turn["params"]["input"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(input["initial_retrieval"]["query"], question);
        assert_eq!(
            input["initial_retrieval"]["hits"][0]["path"],
            "pipeline-notes.md"
        );
        finish(&mut server_writer, vec![id]).await;
    });
    let outcome = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        question,
        None,
        &config,
        &mut evidence,
    )
    .await
    .unwrap();
    rpc.await.unwrap();
    assert!(evidence.finish(&outcome.answer).is_ok());
    assert_eq!(evidence.receipts.len(), 1);
    assert!(evidence.receipts[0].required_initial);
    let summary = accounting.summary();
    assert_eq!(summary.requests, 1);
    assert_eq!(summary.attempted_calls, 1);
    assert_eq!(summary.unobserved_attempts, 0);
    assert_eq!(summary.usage.len(), 1);
    assert_eq!(summary.initial_status, "reranked");
    assert!(summary.accounting_complete);
    let coverage = summary.searches[0].coverage.as_ref().unwrap();
    assert_eq!(coverage.scoped_files, 1);
    assert_eq!(
        coverage.routing_status.as_deref(),
        Some("skipped_single_document")
    );
    let catalog = evidence
        .call("catalog", "gptgrep_catalog", json!({}))
        .await
        .unwrap();
    let catalog: Value =
        serde_json::from_str(catalog["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(catalog["documents"].as_array().unwrap().len(), 1);
    assert_eq!(
        evidence
            .call(
                "outside",
                "gptgrep_tree",
                json!({"path":"neighbor-guide.md"})
            )
            .await
            .unwrap()["success"],
        false
    );
}

#[tokio::test]
async fn initial_pass_retries_one_transient_status_and_keeps_its_unknown_cost() {
    let root = corpus().await;
    // Unscoped two-document corpus: routing, a 502 rerank, then its retry.
    let (client, server, requests) = mock_jev(vec![(200, 3.0), (502, 0.0), (200, 3.0)]).await;
    let client = client.with_transient_retry(gptgrep_jev::TransientRetry::with_jitter(|| 0.0));
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&HostConfig::default(), accounting.clone(), Some(client))
        .unwrap();
    evidence
        .bootstrap("Which transformation removes blank records before assembly?")
        .await
        .unwrap();
    let bodies = server.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(
        bodies[1], bodies[2],
        "the retry repeats the identical rerank"
    );
    assert!(
        !evidence.initial_payload.as_ref().unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let summary = accounting.summary();
    assert_eq!(summary.initial_status, "reranked");
    assert_eq!(summary.requests, 2);
    assert_eq!(summary.attempted_calls, 3);
    assert_eq!(summary.unobserved_attempts, 1);
    // Only validated replies carry cost; the failed attempt keeps unknown cost.
    assert_eq!(summary.usage.len(), 2);
    assert!(!summary.accounting_complete);
    let retried = &summary.searches[0].metrics.jev_retried_attempts;
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].stage, "evidence_reranking");
    assert_eq!(retried[0].attempt.http_status, Some(502));
    assert!(retried[0].attempt.reported_usage.is_none());
    let ledger = std::fs::read_to_string(accounting.path()).unwrap();
    assert!(ledger.contains("jev_retried_attempts"));
    assert!(!ledger.contains("private-provider-body-sentinel"));
}

#[tokio::test]
async fn seed_below_floor_delivers_flagged_low_confidence_candidates() {
    let root = corpus().await;
    // Every reranked candidate scores 0 of 3, below the 0.5 relevance floor.
    let (client, server, _) = mock_jev(vec![(200, 0.0)]).await;
    let config = HostConfig {
        document: Some("pipeline-notes.md".into()),
        ..HostConfig::default()
    };
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&config, accounting.clone(), Some(client))
        .unwrap();
    evidence
        .bootstrap("Which transformation removes blank records before assembly?")
        .await
        .unwrap();
    server.await.unwrap();
    let seed = evidence.initial_payload.as_ref().unwrap();
    let hits = seed["hits"].as_array().unwrap();
    assert!(!hits.is_empty() && hits.len() <= 3);
    assert!(hits.iter().all(|hit| hit["below_threshold"] == true));
    assert_eq!(
        seed["coverage"]["below_threshold_fallback"]
            .as_u64()
            .unwrap() as usize,
        hits.len()
    );
    assert!(seed["warnings"].as_array().unwrap().iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("low-confidence candidates")
    }));
    // Delivered fallback windows are real issued evidence, never a passing status.
    assert_eq!(evidence.receipts[0].evidence.len(), hits.len());
    let summary = accounting.summary();
    assert_eq!(summary.initial_status, "filtered_all");
    assert_eq!(summary.searches[0].delivered_hits, hits.len());
}

async fn assert_packed_fallback_coverage_matches_ledger(planned: bool) {
    let root = tempfile::tempdir().unwrap();
    // Large escaped headings force the host to omit whole hits from the seed.
    // The question has no literal anchor; every synthetic score is below floor.
    for ordinal in 0..3 {
        std::fs::write(
            root.path().join(format!("material-{ordinal}.md")),
            format!(
                "# Section {ordinal} {}\nGeneric material stored on a shelf.\n",
                "\"".repeat(1000)
            ),
        )
        .unwrap();
    }
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let (client, server, _) = mock_jev(vec![(200, 0.0); 2]).await;
    let config = HostConfig {
        query_plan: planned.then(crate::QueryPlanConfig::default),
        ..HostConfig::default()
    };
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&config, accounting.clone(), Some(client))
        .unwrap();
    let question = "How are supplies organized?";
    accounting.bind_workflow(question, None).unwrap();
    if planned {
        evidence.bootstrap_planned(question, &[]).await.unwrap();
    } else {
        evidence.bootstrap(question).await.unwrap();
    }
    assert_eq!(server.await.unwrap().len(), 2);
    let seed = evidence.initial_payload.as_ref().unwrap();
    let hits = seed["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty() && hits.len() < 3,
        "packing fixture must omit fallback hits"
    );
    assert!(
        hits.iter()
            .all(|hit| hit["below_threshold"] == true && hit["literal_anchor"] == false)
    );
    assert_eq!(seed["coverage"]["below_threshold_fallback"], hits.len());
    assert_eq!(seed["host_delivery"]["omitted_hits"], 3 - hits.len());
    let envelope =
        json!({"contentItems":[{"type":"inputText","text":seed.to_string()}],"success":true});
    assert!(serde_json::to_vec(&envelope).unwrap().len() <= crate::retrieval::MAX_TOOL_BYTES);
    let receipt = &evidence.receipts[0];
    assert_eq!(receipt.evidence.len(), hits.len());
    let summary = accounting.summary();
    assert_eq!(summary.initial_status, "filtered_all");
    assert_eq!((summary.attempted_calls, summary.requests), (2, 2));
    let events = ledger_events(&accounting.path());
    let before_event = if planned {
        "plan_fused"
    } else {
        "search_finished"
    };
    let before = &events
        .iter()
        .find(|event| event["event"] == before_event)
        .unwrap()["search"];
    let delivery = &events
        .iter()
        .find(|event| event["event"] == "delivery")
        .unwrap()["search"];
    let pointer = if planned {
        "/plan/coverage"
    } else {
        "/coverage"
    };
    let mut expected_coverage = before.pointer(pointer).unwrap().clone();
    assert_eq!(expected_coverage["below_threshold_fallback"], 3);
    assert_eq!(expected_coverage["reranked_candidates"], 3);
    assert_eq!(expected_coverage["filtered_candidates"], 3);
    expected_coverage["below_threshold_fallback"] = json!(hits.len());
    let receipt_search = serde_json::to_value(receipt.search.as_ref().unwrap()).unwrap();
    let summary_search = serde_json::to_value(&summary.searches[0]).unwrap();
    for search in [&receipt_search, &summary_search, delivery] {
        assert_eq!(search["delivered_hits"], hits.len());
        assert_eq!(search["output_truncated"], true);
        assert_eq!(search.pointer(pointer).unwrap(), &expected_coverage);
        // Packing changes only the delivered fallback count; candidate totals
        // and observed provider metrics retain their pre-delivery values.
        assert_eq!(search["metrics"], before["metrics"]);
    }
}

#[tokio::test]
async fn packed_seed_fallback_coverage_matches_receipt_and_ledger() {
    assert_packed_fallback_coverage_matches_ledger(false).await;
}

#[tokio::test]
async fn packed_planned_seed_fallback_coverage_matches_receipt_and_ledger() {
    assert_packed_fallback_coverage_matches_ledger(true).await;
}

#[tokio::test]
async fn partial_rerank_failure_preserves_routing_usage_in_typed_error_and_ledger() {
    let root = corpus().await;
    let (client, server, _) = mock_jev(vec![(200, 3.0), (503, 0.0)]).await;
    let home = tempfile::tempdir().unwrap();
    // Unscoped two-document corpus: routing replies before the rerank fails.
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        codex_bin: "/missing/mock-codex".into(),
        ..HostConfig::default()
    };
    let error = execute_with_client(
        root.path(),
        "Describe the transformation.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    let error = error.downcast_ref::<HostRetrievalError>().unwrap();
    assert_eq!(error.stage, "initial_search");
    assert_eq!(error.jev.requests, 1);
    assert_eq!(error.jev.attempted_calls, 2);
    assert_eq!(error.jev.unobserved_attempts, 1);
    assert_eq!(error.jev.usage.len(), 1);
    assert!(!error.jev.accounting_complete);
    assert_eq!(error.cause.as_ref().unwrap()["stage"], "evidence_reranking");
    let raw = std::fs::read_to_string(&error.ledger_path).unwrap();
    assert!(!raw.contains("private-provider-body-sentinel"));
    assert!(!raw.contains("synthetic-host-test-key"));
    assert!(!raw.contains("Describe the transformation."));
    let events = ledger_events(&error.ledger_path);
    assert_eq!(events.last().unwrap()["requests"], 1);
    assert_eq!(events.last().unwrap()["attempted_calls"], 2);
    assert_eq!(events.last().unwrap()["event"], "failed");
}

#[tokio::test]
async fn validated_retry_reply_keeps_its_usage_in_the_typed_error_and_ledger() {
    let root = corpus().await;
    // One scoped document: the rerank gets HTTP 502; its retry is sent and
    // validated, then a local observer step rejects the reply.
    let (client, server, requests) = mock_jev(vec![(502, 0.0), (200, 3.0)]).await;
    let client = client
        .with_transient_retry(gptgrep_jev::TransientRetry::with_jitter(|| 0.0))
        .with_decision_observer(Arc::new(crate::query_plan::tests::RejectValidated));
    let home = tempfile::tempdir().unwrap();
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        codex_bin: "/missing/mock-codex".into(),
        document: Some("pipeline-notes.md".into()),
        ..HostConfig::default()
    };
    let error = execute_with_client(
        root.path(),
        "Describe the transformation.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let error = error.downcast_ref::<HostRetrievalError>().unwrap();
    assert_eq!(error.stage, "initial_search");
    assert_eq!((error.jev.attempted_calls, error.jev.requests), (2, 1));
    assert_eq!(error.jev.unobserved_attempts, 1);
    // The validated retry keeps its usage; the failed attempt stays unknown.
    assert_eq!(
        error.jev.usage,
        [json!({"input_tokens":101,"output_tokens":2,"cost":0.002})]
    );
    assert!(!error.jev.accounting_complete);
    let retried = &error.jev.searches[0].metrics.jev_retried_attempts;
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].attempt.http_status, Some(502));
    assert!(retried[0].attempt.reported_usage.is_none());
    let cause = error.cause.as_ref().unwrap();
    assert_eq!(cause["stage"], "evidence_reranking");
    assert!(
        cause["cause"]
            .as_str()
            .unwrap()
            .contains("attempt 2: sent and validated")
    );
    let events = ledger_events(&error.ledger_path);
    let last = events.last().unwrap();
    assert_eq!(
        (last["attempted_calls"].as_u64(), last["requests"].as_u64()),
        (Some(2), Some(1))
    );
    assert!(events.iter().any(|event| {
        event["search"]["metrics"]["jev_usage"]
            .as_array()
            .is_some_and(|usage| usage.len() == 1)
    }));
    let raw = std::fs::read_to_string(&error.ledger_path).unwrap();
    assert!(!raw.contains("synthetic-local-ledger-failure"));
    assert!(!raw.contains("private-provider-body-sentinel"));
}

#[tokio::test]
async fn codex_startup_failure_retains_successful_initial_accounting() {
    let root = corpus().await;
    let (client, server, _) = mock_jev(vec![(200, 3.0)]).await;
    let home = tempfile::tempdir().unwrap();
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        codex_bin: "/missing/mock-codex".into(),
        document: Some("pipeline-notes.md".into()),
        ..HostConfig::default()
    };
    let error = execute_with_client(
        root.path(),
        "Describe the transformation.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    let error = error.downcast_ref::<HostRetrievalError>().unwrap();
    assert_eq!(error.stage, "codex");
    assert_eq!(error.code, "host_codex_binary_unavailable");
    assert_eq!(
        error.model_attempts[0].error,
        Some(json!({"code":"host_codex_binary_unavailable"}))
    );
    assert_eq!(error.jev.requests, 1);
    assert_eq!(error.jev.attempted_calls, 1);
    assert_eq!(error.jev.usage.len(), 1);
    assert_eq!(error.receipts.len(), 1);
    assert!(error.receipts[0].required_initial);
    assert_eq!(
        ledger_events(&error.ledger_path).last().unwrap()["event"],
        "failed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reader_turn_failure_code_reaches_the_ask_error_and_attempt_ledger() {
    use std::os::unix::fs::PermissionsExt;
    let root = corpus().await;
    let (client, server, _) = mock_jev(vec![(200, 3.0)]).await;
    let home = tempfile::tempdir().unwrap();
    let launcher = home.path().join("mock-reader-runtime");
    // A completed turn that never delivers a final agent message.
    let mut script = String::from("#!/bin/sh\n");
    for response in [
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{}}}),
        json!({"id":3,"result":{"thread":{"id":"thread-native"},"model":DEFAULT_MODEL,
            "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
            "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}),
        json!({"id":4,"result":{"turn":{"id":"turn-native"}}}),
        json!({"method":"turn/completed","params":{"threadId":"thread-native",
            "turn":{"id":"turn-native","status":"completed","items":[]}}}),
    ] {
        if response.get("id").is_some() {
            script.push_str("read -r request\n");
        }
        if response["id"] == 100 {
            script.push_str("read -r request\n"); // initialized precedes account/read
        }
        script.push_str(&format!("printf '%s\\n' '{response}'\n"));
    }
    script.push_str("while :; do sleep 1; done\n");
    std::fs::write(&launcher, script).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        codex_bin: launcher.to_string_lossy().into_owned(),
        document: Some("pipeline-notes.md".into()),
        timeout_secs: 30,
        ..HostConfig::default()
    };
    let error = execute_with_client(
        root.path(),
        "Describe the transformation.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    let error = error.downcast_ref::<HostRetrievalError>().unwrap();
    assert_eq!(
        (error.code.as_str(), error.stage.as_str()),
        ("host_codex_no_final_answer", "codex")
    );
    assert!(error.cause.is_none());
    let reader = &error.model_attempts[0];
    assert_eq!(reader.role, "final_reader");
    assert_eq!(reader.status, "failed");
    assert_eq!(reader.turn_id.as_deref(), Some("turn-native"));
    assert_eq!(
        reader.error,
        Some(json!({"code":"host_codex_no_final_answer"}))
    );
    let last = ledger_events(&error.ledger_path).pop().unwrap();
    assert_eq!(last["event"], "failed");
    assert_eq!(
        last["model_attempts"][0]["error"]["code"],
        "host_codex_no_final_answer"
    );
}

#[tokio::test]
async fn cancellation_retains_prior_reply_and_unobserved_attempt() {
    let root = corpus().await;
    let (client, server, received) = mock_jev(vec![(200, 3.0), (0, 0.0)]).await;
    let root_path = root.path().to_owned();
    let home = tempfile::tempdir().unwrap();
    // Unscoped: routing replies, then the rerank stays pending.
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        ..HostConfig::default()
    };
    let task = tokio::spawn(async move {
        execute_with_client(
            &root_path,
            "Describe the transformation.",
            None,
            &config,
            Some(client),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.abort();
    let path = std::fs::read_dir(root.path().join(".gptgrep/host-attempts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let events = ledger_events(&path);
    let last = events.last().unwrap();
    assert_eq!(last["event"], "interrupted");
    assert_eq!(last["requests"], 1);
    assert_eq!(last["attempted_calls"], 2);
    assert_eq!(last["unobserved_attempts"], 1);
    assert_eq!(last["accounting_complete"], false);
    assert!(events.iter().any(|event| {
        event["search"]["metrics"]["jev_usage"]
            .as_array()
            .is_some_and(|usage| usage.len() == 1)
    }));
}

#[tokio::test]
async fn cancellation_during_a_bounded_retry_keeps_every_submission_in_the_ledger() {
    let root = corpus().await;
    // Unscoped: routing replies, the rerank gets HTTP 502, its retry hangs.
    let (client, server, received) = mock_jev(vec![(200, 3.0), (502, 0.0), (0, 0.0)]).await;
    let client = client.with_transient_retry(gptgrep_jev::TransientRetry::with_jitter(|| 0.0));
    let root_path = root.path().to_owned();
    let home = tempfile::tempdir().unwrap();
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        ..HostConfig::default()
    };
    let task = tokio::spawn(async move {
        execute_with_client(
            &root_path,
            "Describe the transformation.",
            None,
            &config,
            Some(client),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while received.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.abort();
    let path = std::fs::read_dir(root.path().join(".gptgrep/host-attempts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let events = ledger_events(&path);
    assert!(events.iter().any(|event| event["event"] == "before_retry"));
    let last = events.last().unwrap();
    assert_eq!(last["event"], "interrupted");
    // Three submissions: one validated reply and two with unknown cost.
    assert_eq!(last["requests"], 1);
    assert_eq!(last["attempted_calls"], 3);
    assert_eq!(last["unobserved_attempts"], 2);
    assert_eq!(last["accounting_complete"], false);
    let retried = events
        .iter()
        .rev()
        .find_map(|event| event["search"]["metrics"]["jev_retried_attempts"].as_array())
        .unwrap();
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["stage"], "evidence_reranking");
    assert_eq!(retried[0]["http_status"], 502);
    assert!(retried[0]["reported_usage"].is_null());
}

#[tokio::test]
async fn all_stale_candidates_are_fatal_without_claiming_reranking() {
    let root = corpus().await;
    std::fs::remove_file(root.path().join("pipeline-notes.md")).unwrap();
    std::fs::remove_file(root.path().join("neighbor-guide.md")).unwrap();
    let (client, server, received) = mock_jev(vec![]).await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&HostConfig::default(), accounting.clone(), Some(client))
        .unwrap();
    let error = evidence
        .bootstrap("Find relevant evidence.")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("host_jev_no_candidates"));
    server.await.unwrap();
    assert_eq!(received.load(Ordering::SeqCst), 0);
    assert_eq!(accounting.summary().requests, 0);
    assert_eq!(accounting.summary().initial_status, "no_candidates");
    assert!(evidence.initial_payload.is_none());
}

#[tokio::test]
async fn summary_scope_is_derived_and_conflicts_are_rejected() {
    let root = corpus().await;
    let tree = gptgrep_core::tree(root.path(), Path::new("pipeline-notes.md")).unwrap();
    let id = format!(
        "{}:{}",
        tree["document"]["id"].as_str().unwrap(),
        tree["document"]["nodes"][0]["id"].as_str().unwrap()
    );
    let (client, server, _) = mock_jev(vec![(200, 3.0)]).await;
    let mut evidence = Evidence::open(root.path(), Some(&id)).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&HostConfig::default(), accounting, Some(client))
        .unwrap();
    evidence
        .bootstrap("Summarize the selected node.")
        .await
        .unwrap();
    let requests = server.await.unwrap();
    let query = requests[0]["state"]["query"].as_str().unwrap();
    assert!(query.contains("pipeline-notes.md"));
    assert!(query.contains("Token pipeline"));
    assert_eq!(evidence.document_scope(), Some("pipeline-notes.md"));
    let mut other = Evidence::open(root.path(), Some(&id)).unwrap();
    let accounting = Accounting::create(root.path(), &other.generation).unwrap();
    let config = HostConfig {
        document: Some("neighbor-guide.md".into()),
        ..HostConfig::default()
    };
    assert!(
        other
            .configure(&config, accounting, None)
            .unwrap_err()
            .to_string()
            .contains("conflict")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ledger_symlink_is_rejected_before_inference() {
    let root = corpus().await;
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join(".gptgrep/host-attempts")).unwrap();
    assert!(Accounting::create(root.path(), "generation").is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn missing_key_fails_before_codex_in_a_key_free_process() {
    const FLAG: &str = "GPTGREP_HOST_TEST_KEY_FREE";
    if std::env::var_os(FLAG).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::mandatory::missing_key_fails_before_codex_in_a_key_free_process",
                "--nocapture",
            ])
            .env(FLAG, "1")
            .env_remove("OPENROUTER_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = corpus().await;
    let home = tempfile::tempdir().unwrap();
    let config = HostConfig {
        codex_home: home.path().to_owned(),
        codex_bin: "/missing/mock-codex".into(),
        ..HostConfig::default()
    };
    let error = ask(root.path(), "Find processing evidence.", &config)
        .await
        .unwrap_err();
    let error = error.downcast_ref::<HostRetrievalError>().unwrap();
    assert_eq!(error.stage, "initial_search");
    assert_eq!(error.jev.requests, 0);
    assert_eq!(error.code, "host_jev_initialization_failed");
    assert!(
        error.cause.as_ref().unwrap()["cause"]
            .as_str()
            .unwrap()
            .contains("OPENROUTER_API_KEY")
    );
    assert_eq!(error.jev.attempted_calls, 0);
    assert!(error.jev.searches[0].coverage.is_none());
}
