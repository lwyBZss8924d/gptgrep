//! Initialized-ledger and alias admission tests over same-process loopback only.
//! No model provider, credential or network beyond 127.0.0.1 is used.

use gptgrep_jev::{
    DecisionFailure, DecisionInput, DecisionsBudget, ExecutionContext, ImageEvidence, JevClient,
    MAX_RESPONSE_BYTES, SourceBinding, TextEvidence, UsagePresence,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const KEY: &str = "synthetic-budget-key-sentinel";

fn questions() -> Value {
    json!({"q":{"type":"noul","instructions":"Is this synthetic note internally consistent?"}})
}

fn deadline_ms(after: Duration) -> u64 {
    (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + after).as_millis() as u64
}

/// Loopback endpoint answering accepted connections with scripted bodies in
/// order; every accepted connection is counted, including unexpected ones.
async fn serve(bodies: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        let mut bodies = bodies.into_iter();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let Some(body) = bodies.next() else {
                continue;
            };
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if let Some(offset) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..offset]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    if request.len() >= offset + 4 + length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (endpoint, accepted)
}

fn answer(model: &str, id: &str) -> String {
    json!({"model":model,"id":id,"provider":"Synthetic","answers":{"q":{"type":"noul","noul":0.75}},
        "usage":{"input_tokens":19,"output_tokens":1,"cost":0.0000008}})
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_initialized_run_stops_at_max_submissions_without_another_post() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = directory.path().join("ledger.json");
    let control = directory.path().join("control.json");
    let budget = DecisionsBudget::initialize(
        "synthetic-exhaustion-run",
        &"d".repeat(64),
        deadline_ms(Duration::from_secs(60)),
        2,
        &ledger,
        &control,
    )
    .unwrap();
    let (endpoint, accepted) = serve(vec![
        answer("typesafe/jev-1.13-20260917", "gen-synthetic-a"),
        answer("typesafe/jev-1.13", "gen-synthetic-b"),
    ])
    .await;
    // Two separate joins model two independent CLI processes sharing the ledger.
    for _ in 0..2 {
        let client = JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint)
            .unwrap()
            .with_execution_context(ExecutionContext::from_budget_file(&control).unwrap());
        let response = client
            .decide(json!("synthetic note"), questions())
            .await
            .unwrap();
        assert!(response.model.starts_with("typesafe/jev-1.13"));
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    let before = std::fs::read(&ledger).unwrap();
    let exhausted = JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint)
        .unwrap()
        .with_execution_context(ExecutionContext::from_budget_file(&control).unwrap());
    let error = exhausted
        .decide(json!("synthetic note"), questions())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Decisions submission budget exhausted");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "no POST after exhaustion"
    );
    let status = budget.status().unwrap();
    assert_eq!(status.reservations.total, 2);
    assert_eq!(status.reservations.terminal, 2);
    assert_eq!(status.remaining_submissions, 0);
    assert_eq!(status.requested_models["~typesafe/jev-latest"], 2);
    assert!(!status.admission_open);
    // The refused attempt adds only a local observation, never a reservation.
    assert_eq!(status.submission_observations, 3);
    assert_ne!(std::fs::read(&ledger).unwrap(), before);
    // A second initialization can neither reset nor renew the consumed run.
    assert!(
        DecisionsBudget::initialize(
            "synthetic-exhaustion-run",
            &"d".repeat(64),
            deadline_ms(Duration::from_secs(120)),
            2,
            &ledger,
            &directory.path().join("renewed-control.json"),
        )
        .is_err()
    );
    assert_eq!(budget.status().unwrap().reservations.total, 2);
    let text = std::fs::read_to_string(&ledger).unwrap();
    assert!(!text.contains(KEY));
    assert!(!text.contains("synthetic note"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_rejects_other_author_or_family_after_charge_and_keeps_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.json");
    DecisionsBudget::initialize(
        "synthetic-alias-run",
        &"e".repeat(64),
        deadline_ms(Duration::from_secs(60)),
        3,
        &directory.path().join("ledger.json"),
        &control,
    )
    .unwrap();
    let (endpoint, accepted) = serve(vec![
        answer("other/jev-1.13-20260917", "gen-synthetic-other-author"),
        answer("typesafe/jevx-2", "gen-synthetic-other-family"),
        answer("typesafe/jev-2.0", "gen-synthetic-newer-build"),
    ])
    .await;
    let client = JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint)
        .unwrap()
        .with_execution_context(ExecutionContext::from_budget_file(&control).unwrap());
    for (served, id) in [
        ("other/jev-1.13-20260917", "gen-synthetic-other-author"),
        ("typesafe/jevx-2", "gen-synthetic-other-family"),
    ] {
        let error = client
            .decide(json!("synthetic note"), questions())
            .await
            .unwrap_err();
        let diagnostic = error
            .downcast_ref::<DecisionFailure>()
            .unwrap()
            .diagnostic();
        assert_eq!(diagnostic.validation_code, Some("model_mismatch"));
        let metadata = diagnostic.response_metadata.as_ref().unwrap();
        assert_eq!(metadata.model.value.as_deref(), Some(served));
        assert_eq!(metadata.id.value.as_deref(), Some(id));
        let usage = diagnostic.reported_usage.as_ref().unwrap();
        assert_eq!(usage.cost.presence, UsagePresence::Reported);
        assert_eq!(usage.cost.value, Some(0.0000008));
        assert!(!error.to_string().contains(served));
    }
    let admitted = client
        .decide(json!("synthetic note"), questions())
        .await
        .unwrap();
    assert_eq!(admitted.model, "typesafe/jev-2.0");
    assert_eq!(accepted.load(Ordering::SeqCst), 3);
    let snapshot = client.execution_context().snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 3);
    assert_eq!(snapshot.terminal_submissions, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_and_malformed_alias_reject_before_any_reservation_or_post() {
    let (endpoint, accepted) = serve(vec![]).await;
    for malformed in [
        "~typesafe/jev",
        "~typesafe",
        "~TypeSafe/jev-latest",
        "~a/b:free-latest",
    ] {
        let error = JevClient::with_endpoint(KEY, Some(malformed), &endpoint)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Unsupported Decisions model alias"));
        assert!(!error.contains(KEY));
    }
    let context = ExecutionContext::new(
        "synthetic-alias-images",
        &"f".repeat(64),
        SystemTime::now() + Duration::from_secs(30),
        2,
    )
    .unwrap();
    let client = JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint)
        .unwrap()
        .with_execution_context(context.clone());
    let mut png = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut png),
        &[255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0],
        2,
        2,
        image::ExtendedColorType::Rgb8,
    )
    .unwrap();
    let source = SourceBinding {
        source_id: "synthetic".into(),
        unit_id: "image".into(),
        source_sha256: "a".repeat(64),
    };
    let input = DecisionInput {
        text: vec![TextEvidence {
            source: SourceBinding {
                unit_id: "caption".into(),
                ..source.clone()
            },
            text: "Synthetic caption.".into(),
        }],
        images: vec![ImageEvidence::from_bytes(source, png).unwrap()],
    };
    let error = match client.prepare_input(input.clone(), questions(), MAX_RESPONSE_BYTES) {
        Ok(_) => panic!("alias images must be rejected"),
        Err(error) => error.to_string(),
    };
    assert_eq!(
        error,
        "Decisions images require the Luna Decisions model; no request was prepared or submitted"
    );
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 0);
    assert_eq!(snapshot.submission_observations, 0);
    // The same text without images uses the legacy evidence-object state.
    let mut text_only = input;
    text_only.images.clear();
    let prepared = client
        .prepare_input(text_only, questions(), MAX_RESPONSE_BYTES)
        .unwrap();
    let wire: Value = serde_json::from_slice(prepared.canonical_body()).unwrap();
    assert_eq!(wire["model"], "~typesafe/jev-latest");
    assert!(wire["state"]["evidence"].is_array());
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    assert!(!gptgrep_jev::model_accepts_images("~typesafe/jev-latest"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_state_image_parts_for_text_only_selectors_fail_before_any_reservation_or_post() {
    let (endpoint, accepted) = serve(vec![]).await;
    let data_url = "data:image/png;base64,iVBORw0KGgo=";
    let flat = json!(["Synthetic caption.", {"type":"image_url","image_url":{"url":data_url}}]);
    let nested = json!({"evidence":[{"text":"Synthetic caption."},
        {"part":{"type":"input_image","image_url":data_url}}]});
    for model in [
        "~typesafe/jev-latest",
        gptgrep_jev::LEGACY_MODEL,
        "explicit/other",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control.json");
        let budget = DecisionsBudget::initialize(
            "synthetic-raw-image-run",
            &"9".repeat(64),
            deadline_ms(Duration::from_secs(60)),
            1,
            &directory.path().join("ledger.json"),
            &control,
        )
        .unwrap();
        let client = JevClient::with_endpoint(KEY, Some(model), &endpoint)
            .unwrap()
            .with_execution_context(ExecutionContext::from_budget_file(&control).unwrap());
        for state in [&flat, &nested] {
            assert!(client.prepare_decision(state.clone(), questions()).is_err());
            let error = client.decide(state.clone(), questions()).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "Decisions images require the Luna Decisions model; no request was prepared or submitted",
                "{model}"
            );
        }
        let status = budget.status().unwrap();
        assert_eq!(status.reservations.total, 0, "{model}");
        assert_eq!(status.submission_observations, 0, "{model}");
        assert_eq!(status.remaining_submissions, 1, "{model}");
        let snapshot = client.execution_context().snapshot().unwrap();
        assert_eq!(snapshot.logical_reservations, 0);
        assert_eq!(snapshot.submission_observations, 0);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "no connection was opened"
    );
    // Luna keeps the raw structured-state API, including documented image parts.
    let luna = JevClient::with_endpoint(KEY, None, &endpoint).unwrap();
    let prepared = luna.prepare_decision(flat.clone(), questions()).unwrap();
    let wire: Value = serde_json::from_slice(prepared.canonical_body()).unwrap();
    assert_eq!(wire["state"], flat);
}
