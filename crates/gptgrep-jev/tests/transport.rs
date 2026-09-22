use gptgrep_jev::{Candidate, JevClient, MAX_RESPONSE_BYTES};
use serde_json::{Value, json};
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
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 503"));
    assert!(error.contains("not retried"));
    assert!(!error.contains("secret-sentinel"));
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
        .unwrap_err()
        .to_string();
    assert!(error.contains("byte limit"));
    server.await.unwrap();
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
