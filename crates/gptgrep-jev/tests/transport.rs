use gptgrep_jev::{
    Candidate, DecisionFailure, DecisionFailureKind, ExecutionContext, JevClient,
    MAX_RESPONSE_BYTES, UsagePresence,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

async fn serve_once(
    status: &str,
    body: String,
    extra_headers: &str,
) -> (String, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let status = status.to_owned();
    let extra_headers = extra_headers.to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() < 128 * 1024);
            if let Some(offset) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..offset]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map(|length| length.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                if request.len() >= offset + 4 + length {
                    break;
                }
            }
        }
        let length = if extra_headers
            .to_ascii_lowercase()
            .contains("content-length:")
        {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{length}{extra_headers}Connection: close\r\n\r\n{body}"
        );
        // A bounded client may close immediately after reading oversized response headers.
        let _ = socket.write_all(response.as_bytes()).await;
        request
    });
    (endpoint, server)
}

#[tokio::test]
async fn prepared_decision_binds_exact_bytes_model_and_endpoint() {
    let response = json!({"model":"typesafe/jev-1.13-20260917",
        "answers":{"support":{"type":"choice","choice":"supported"}},
        "usage":{"input_tokens":9,"output_tokens":2}});
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let state = json!({"source":"A ceramic chamber stores the item."});
    let questions = json!({"support":{"type":"choice","instructions":"Does the source support this hint?",
        "criteria":{"supported":"Directly supported","unsupported":"Not supported"}}});
    let wrong = client
        .prepare_decision(state.clone(), questions.clone())
        .unwrap();
    let other =
        JevClient::with_endpoint("synthetic-test-key", Some("typesafe/other"), &endpoint).unwrap();
    assert!(
        other
            .submit_prepared(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("different client")
    );
    let wrong_client = client
        .prepare_decision(state.clone(), questions.clone())
        .unwrap();
    let other_same_model = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    assert!(
        other_same_model
            .submit_prepared(wrong_client)
            .await
            .unwrap_err()
            .to_string()
            .contains("different client")
    );
    let prepared = client.prepare_decision(state, questions).unwrap();
    let bytes = prepared.body_bytes();
    let sha = prepared.body_sha256().to_owned();
    assert_eq!(prepared.requested_model(), "typesafe/jev-1.13");
    let result = client.submit_prepared(prepared).await.unwrap();
    assert_eq!(
        result.answers["support"],
        gptgrep_jev::DecisionAnswer::Choice {
            choice: "supported".into(),
            confidence: None,
            probabilities: None
        }
    );
    let request = server.await.unwrap();
    let offset = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert_eq!(request.len() - offset, bytes);
    let actual: String = Sha256::digest(&request[offset..])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(actual, sha);
    let body: Value = serde_json::from_slice(&request[offset..]).unwrap();
    assert_eq!(body["provider"]["allow_fallbacks"], false);
}

fn diagnostic_question() -> Value {
    json!({"support":{"type":"choice","instructions":"Does the supplied passage support the claim?",
        "criteria":{"supported":"Supported","unsupported":"Unsupported"}}})
}

#[tokio::test]
async fn failed_typed_response_preserves_http_and_numeric_usage_without_payload() {
    let response = json!({"model":"vendor-build", "answers":{"support":{"type":"unsupported-secret-sentinel"}},
        "usage":{"input_tokens":17,"output_tokens":null,"cost":0.0000123,"private":"secret-sentinel"},
        "private":"secret-sentinel"}).to_string();
    let expected_bytes = response.len();
    let (endpoint, server) = serve_once("200 OK", response, "").await;
    let context = ExecutionContext::new(
        "schema-failure",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(30),
        1,
    )
    .unwrap();
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
        .unwrap()
        .with_execution_context(context.clone());
    let error = client
        .decide(json!("ordinary passage"), diagnostic_question())
        .await
        .unwrap_err();
    let failure = error.downcast_ref::<DecisionFailure>().unwrap();
    let observed = failure.diagnostic();
    assert_eq!(observed.kind, DecisionFailureKind::ResponseSchema);
    assert_eq!(observed.http_status, Some(200));
    assert_eq!(observed.response_bytes, Some(expected_bytes));
    assert_eq!(observed.validation_code, Some("schema"));
    let usage = observed.reported_usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens.presence, UsagePresence::Reported);
    assert_eq!(usage.input_tokens.value, Some(17));
    assert_eq!(usage.output_tokens.presence, UsagePresence::Null);
    assert_eq!(usage.output_tokens.value, None);
    assert_eq!(usage.cost.value, Some(0.0000123));
    for rendered in [
        format!("{failure}"),
        format!("{failure:?}"),
        serde_json::to_string(observed).unwrap(),
        format!("{error:#}"),
    ] {
        assert!(!rendered.contains("secret-sentinel"));
        assert!(!rendered.contains("ordinary passage"));
        assert!(!rendered.contains("synthetic-test-key"));
        assert!(!rendered.contains(&endpoint));
    }
    assert!(std::error::Error::source(failure).is_none());
    server.await.unwrap();
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.send_start_observations, 1);
    assert_eq!(snapshot.terminal_submissions, 1);
    assert_eq!(snapshot.active_or_quarantined_slots, 0);
}

#[tokio::test]
async fn duplicate_json_cannot_supply_failure_usage_or_become_a_schema_failure() {
    let response =
        r#"{"model":"vendor-build","answers":{},"usage":{"cost":0},"usage":{"cost":999}}"#;
    let (endpoint, server) = serve_once("200 OK", response.into(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(json!("ordinary passage"), diagnostic_question())
        .await
        .unwrap_err();
    let observed = error
        .downcast_ref::<DecisionFailure>()
        .unwrap()
        .diagnostic();
    assert_eq!(observed.kind, DecisionFailureKind::InvalidJson);
    assert_eq!(observed.http_status, Some(200));
    assert!(observed.reported_usage.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_usage_values_are_marked_invalid_and_never_retained() {
    let response = json!({"model":"vendor-build","answers":{"support":{"type":"choice","choice":"supported"}},
        "usage":{"input_tokens":"secret-sentinel","output_tokens":-1,"cost":"secret-sentinel"}});
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(json!("ordinary passage"), diagnostic_question())
        .await
        .unwrap_err();
    let observed = error
        .downcast_ref::<DecisionFailure>()
        .unwrap()
        .diagnostic();
    assert_eq!(observed.kind, DecisionFailureKind::ResponseValidation);
    assert_eq!(observed.validation_code, Some("token_usage"));
    let usage = observed.reported_usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens.presence, UsagePresence::Invalid);
    assert_eq!(usage.output_tokens.presence, UsagePresence::Invalid);
    assert_eq!(usage.cost.presence, UsagePresence::Invalid);
    assert!(
        usage.input_tokens.value.is_none()
            && usage.output_tokens.value.is_none()
            && usage.cost.value.is_none()
    );
    assert!(
        !serde_json::to_string(observed)
            .unwrap()
            .contains("secret-sentinel")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn mathematical_rejection_retains_its_specific_validation_code() {
    let response = json!({"model":"vendor-build","answers":{"support":{"type":"choice","choice":"supported",
        "probabilities":{"supported":0.1,"unsupported":0.9}}},"usage":{"cost":null}});
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(json!("ordinary passage"), diagnostic_question())
        .await
        .unwrap_err();
    let observed = error
        .downcast_ref::<DecisionFailure>()
        .unwrap()
        .diagnostic();
    assert_eq!(observed.validation_code, Some("choice_distribution"));
    let usage = observed.reported_usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens.presence, UsagePresence::Absent);
    assert_eq!(usage.cost.presence, UsagePresence::Null);
    server.await.unwrap();
}

#[tokio::test]
async fn sends_decisions_contract_and_preserves_model_and_usage() {
    let response = json!({
        "model":"typesafe/jev-1.13-20260917",
        "id":"gen-dec-synthetic",
        "provider":"TypeSafe",
        "answers":{
            "candidate_0":{"type":"score","score":1.5},
            "candidate_1":{"type":"score","score":3.0,"confidence":0.9}
        },
        "usage":{"input_tokens":123,"output_tokens":12,"cost":0.000005166}
    });
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let result = client
        .rerank(
            "retention",
            &[
                Candidate {
                    id: "background".into(),
                    text: "General policy".into(),
                },
                Candidate {
                    id: "answer".into(),
                    text: "Keep records seven years".into(),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(result.model, "typesafe/jev-1.13-20260917");
    assert_eq!(result.rankings[0].id, "answer");
    assert_eq!(result.rankings[0].score, 1.0);
    assert_eq!(result.rankings[1].score, 0.5);
    assert_eq!(result.rankings[1].confidence, None);
    assert_eq!(result.usage["input_tokens"], 123);
    assert_eq!(
        result.provider_response_id.as_deref(),
        Some("gen-dec-synthetic")
    );
    assert_eq!(result.provider.as_deref(), Some("TypeSafe"));
    let request = server.await.unwrap();
    let offset = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let header = String::from_utf8_lossy(&request[..offset]).to_ascii_lowercase();
    assert!(header.starts_with("post /api/alpha/decisions http/1.1"));
    assert!(header.contains("authorization: bearer synthetic-test-key"));
    let body: Value = serde_json::from_slice(&request[offset + 4..]).unwrap();
    assert_eq!(body["provider"]["allow_fallbacks"], false);
    assert_eq!(body["questions"]["candidate_0"]["type"], "score");
    assert_eq!(body["questions"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn status_failure_is_redacted_and_not_retried() {
    let (endpoint, server) =
        serve_once("503 Service Unavailable", "secret-sentinel".into(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(
            json!("data"),
            json!({
                "q":{"type":"noul","instructions":"Relevant?"}
            }),
        )
        .await
        .unwrap_err();
    let failure = error
        .downcast_ref::<DecisionFailure>()
        .unwrap()
        .diagnostic();
    assert_eq!(failure.kind, DecisionFailureKind::HttpStatus);
    assert_eq!(failure.http_status, Some(503));
    assert!(failure.reported_usage.is_none());
    let rendered = error.to_string();
    assert!(rendered.contains("HTTP 503"));
    assert!(rendered.contains("not retried"));
    assert!(!rendered.contains("secret-sentinel"));
    server.await.unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (endpoint, server) = serve_once(
        "302 Found",
        String::new(),
        "Location: https://example.invalid/decisions\r\n",
    )
    .await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(
            json!("data"),
            json!({
                "q":{"type":"noul","instructions":"Relevant?"}
            }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 302"));
    server.await.unwrap();
}

#[tokio::test]
async fn response_body_is_bounded_before_decoding() {
    let headers = format!("Content-Length: {}\r\n", MAX_RESPONSE_BYTES + 1);
    let (endpoint, server) = serve_once("200 OK", String::new(), &headers).await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let error = client
        .decide(
            json!("data"),
            json!({
                "q":{"type":"noul","instructions":"Relevant?"}
            }),
        )
        .await
        .unwrap_err();
    let observed = error
        .downcast_ref::<DecisionFailure>()
        .unwrap()
        .diagnostic();
    assert_eq!(observed.kind, DecisionFailureKind::ResponseSize);
    assert_eq!(observed.http_status, Some(200));
    assert_eq!(observed.response_bytes, None);
    assert!(observed.reported_usage.is_none());
    assert!(error.to_string().contains("byte limit"));
    server.await.unwrap();
}

#[tokio::test]
async fn incomplete_body_preserves_status_and_quarantines_the_charged_slot() {
    let body = "secret-sentinel";
    let (endpoint, server) = serve_once("200 OK", body.into(), "Content-Length: 4096\r\n").await;
    let context = ExecutionContext::new(
        "partial-body",
        &"b".repeat(64),
        SystemTime::now() + Duration::from_secs(30),
        1,
    )
    .unwrap();
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
        .unwrap()
        .with_execution_context(context.clone());
    let error = client
        .decide(json!("ordinary passage"), diagnostic_question())
        .await
        .unwrap_err();
    let failure = error.downcast_ref::<DecisionFailure>().unwrap();
    let observed = failure.diagnostic();
    assert_eq!(observed.kind, DecisionFailureKind::ResponseBody);
    assert_eq!(observed.http_status, Some(200));
    assert!(
        observed
            .response_bytes
            .is_some_and(|bytes| bytes <= body.len())
    );
    assert!(observed.reported_usage.is_none());
    assert!(std::error::Error::source(failure).is_none());
    assert!(!format!("{error:#?}").contains("secret-sentinel"));
    assert!(!format!("{error:#?}").contains(&endpoint));
    server.await.unwrap();
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.send_start_observations, 1);
    assert_eq!(snapshot.terminal_submissions, 0);
    assert_eq!(snapshot.quarantined_slots, 1);
}

#[tokio::test]
async fn absent_provider_metadata_and_usage_remain_unavailable() {
    let response = json!({"model":"typesafe/jev-fixture", "answers":{"candidate_0":{"type":"score","score":3.0}}});
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let result = client
        .rerank(
            "query",
            &[Candidate {
                id: "id".into(),
                text: "invented evidence".into(),
            }],
        )
        .await
        .unwrap();
    assert!(result.provider_response_id.is_none());
    assert!(result.provider.is_none());
    assert!(result.usage.is_null());
    server.await.unwrap();
    let old_report: gptgrep_jev::RerankResponse = serde_json::from_value(
        json!({"model":"typesafe/jev-fixture", "rankings":[], "usage":null}),
    )
    .unwrap();
    assert!(old_report.provider_response_id.is_none());
    assert!(old_report.provider.is_none());
}

#[tokio::test]
async fn mixed_roles_use_one_prepared_request_and_preserve_optional_unknowns() {
    use gptgrep_jev::EvidenceRole;
    let response = json!({"model":"typesafe/invented-role-model","answers":{
        "relevance_0":{"type":"score","score":2.4},
        "role_0":{"type":"choice","choice":"source_local_incomplete"}
    }});
    let (endpoint, server) = serve_once("200 OK", response.to_string(), "").await;
    let client = JevClient::with_endpoint("invented-key", None, &endpoint).unwrap();
    let prepared = client
        .prepare_evidence_roles(
            "Which link is described?",
            &[Candidate {
                id: "window".into(),
                text: "The second device transmits to it...".into(),
            }],
        )
        .unwrap();
    let expected = prepared.encoded_request().to_vec();
    let result = client.decide_evidence_roles(prepared).await.unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(
        result.candidates[0].role,
        EvidenceRole::SourceLocalIncomplete
    );
    assert!(result.candidates[0].role_confidence.is_none());
    assert!(result.candidates[0].role_probabilities.is_none());
    assert!(result.candidates[0].relevance.confidence.is_none());
    assert!(
        result.provider_response_id.is_none()
            && result.provider.is_none()
            && result.usage.is_null()
    );
    let request = server.await.unwrap();
    let offset = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert_eq!(&request[offset..], expected);
}

#[tokio::test]
async fn missing_wrong_type_or_unknown_role_is_an_explicit_failure() {
    for answers in [
        json!({"relevance_0":{"type":"score","score":3}}),
        json!({"relevance_0":{"type":"score","score":3},"role_0":{"type":"score","score":3}}),
        json!({"relevance_0":{"type":"score","score":3},"role_0":{"type":"choice","choice":"invented_unknown"}}),
    ] {
        let (endpoint, server) = serve_once(
            "200 OK",
            json!({"model":"typesafe/invented","answers":answers}).to_string(),
            "",
        )
        .await;
        let client = JevClient::with_endpoint("invented-key", None, &endpoint).unwrap();
        let prepared = client
            .prepare_evidence_roles(
                "relation",
                &[Candidate {
                    id: "x".into(),
                    text: "maple leads to cedar".into(),
                }],
            )
            .unwrap();
        assert!(client.decide_evidence_roles(prepared).await.is_err());
        server.await.unwrap();
    }
}
