//! Bounded transient retry against a loopback fake provider. No network or
//! provider credentials are used; every response is a synthetic fixture.
use anyhow::Result;
use gptgrep_jev::{
    Candidate, DecisionFailure, DecisionFailureKind, DecisionObserver, DecisionRequestObservation,
    DecisionResponse, ExecutionContext, JevClient, RETRY_BACKOFF_MAX, RETRY_BACKOFF_MIN,
    TransientRetry,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const KEY: &str = "synthetic-retry-test-key";

/// One scripted reply: HTTP status line, extra header lines and body.
struct Reply {
    status: &'static str,
    headers: &'static str,
    body: String,
}

fn reply(status: &'static str, body: impl Into<String>) -> Reply {
    Reply {
        status,
        headers: "",
        body: body.into(),
    }
}

fn success() -> Reply {
    reply(
        "200 OK",
        json!({"model":"openai/gpt-6-luna-decisions-20261006",
            "answers":{"q":{"type":"noul","noul":0.75}},
            "usage":{"input_tokens":7,"output_tokens":1,"cost":0.0002},
            "id":"synthetic-final","provider":"synthetic-provider"})
        .to_string(),
    )
}

struct Served {
    bodies: Vec<Vec<u8>>,
    /// Connections accepted after the scripted replies were exhausted.
    unexpected: usize,
}

async fn read_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() < 256 * 1024);
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map(|value| value.trim().parse().unwrap())
                .unwrap_or(0);
            if bytes.len() >= offset + 4 + length {
                return bytes[offset + 4..offset + 4 + length].to_vec();
            }
        }
    }
}

/// Serve scripted replies on fresh connections, then count any further
/// connection during a short grace period. A retry beyond the script is visible.
async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Served>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .expect("scripted request never arrived")
                .unwrap();
            bodies.push(read_body(&mut socket).await);
            let wire = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                reply.status,
                reply.body.len(),
                reply.headers,
                reply.body
            );
            let _ = socket.write_all(wire.as_bytes()).await;
        }
        let mut unexpected = 0;
        while let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
        {
            unexpected += 1;
        }
        Served { bodies, unexpected }
    });
    (endpoint, server)
}

fn context(deadline: Duration, cap: u32) -> ExecutionContext {
    ExecutionContext::new(
        "synthetic-retry-run",
        &"b".repeat(64),
        SystemTime::now() + deadline,
        cap,
    )
    .unwrap()
}

fn retrying(endpoint: &str, context: &ExecutionContext) -> JevClient {
    JevClient::with_endpoint(KEY, None, endpoint)
        .unwrap()
        .with_execution_context(context.clone())
        .with_transient_retry(TransientRetry::with_jitter(|| 0.0))
}

async fn decide(client: &JevClient) -> Result<DecisionResponse> {
    client
        .decide(
            json!("Synthetic passage for a transient provider failure."),
            json!({"q":{"type":"noul","instructions":"Is the synthetic passage relevant?"}}),
        )
        .await
}

fn failure(error: &anyhow::Error) -> &DecisionFailure {
    error.downcast_ref::<DecisionFailure>().unwrap()
}

#[tokio::test]
async fn transient_502_then_success_accounts_both_attempts() {
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", "private-body"), success()]).await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let started = Instant::now();
    let response = decide(&client).await.unwrap();
    assert!(started.elapsed() >= RETRY_BACKOFF_MIN);
    assert_eq!(response.usage["cost"], json!(0.0002));
    assert_eq!(response.id.as_deref(), Some("synthetic-final"));
    assert_eq!(response.retried_attempts.len(), 1);
    let failed = &response.retried_attempts[0];
    assert_eq!(failed.attempt, 1);
    assert_eq!(failed.kind, DecisionFailureKind::HttpStatus);
    assert_eq!(failed.http_status, Some(502));
    // The error body is not read: the failed attempt's usage stays unknown.
    assert!(failed.reported_usage.is_none());
    assert_eq!(failed.retry_wait_ms, 250);
    assert_eq!(failed.retry_after_ms, None);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["retried_attempts"][0]["http_status"], 502);
    assert_eq!(
        serialized["retried_attempts"][0]["reported_usage"],
        Value::Null
    );
    assert!(!serialized.to_string().contains("private-body"));
    let served = server.await.unwrap();
    assert_eq!(served.bodies.len(), 2);
    assert_eq!(
        served.bodies[0], served.bodies[1],
        "identical request bytes"
    );
    assert_eq!(served.unexpected, 0);
    // Each attempt is its own reserved, sent and terminal submission.
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 2);
    assert_eq!(snapshot.send_start_observations, 2);
    assert_eq!(snapshot.submission_observations, 2);
    assert_eq!(snapshot.terminal_submissions, 2);
    assert_eq!(snapshot.active_or_quarantined_slots, 0);
}

#[tokio::test]
async fn rerank_reports_the_retried_attempt_beside_final_usage() {
    let body = json!({"model":"openai/gpt-6-luna-decisions-20261006",
        "answers":{"candidate_0":{"type":"score","score":3.0}},
        "usage":{"input_tokens":9,"output_tokens":1,"cost":0.0003}})
    .to_string();
    let (endpoint, server) = serve(vec![
        reply("429 Too Many Requests", ""),
        reply("200 OK", body),
    ])
    .await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let ranked = client
        .rerank(
            "synthetic query",
            &[Candidate {
                id: "synthetic".into(),
                text: "synthetic evidence".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(ranked.rankings[0].score, 1.0);
    assert_eq!(ranked.usage["cost"], json!(0.0003));
    assert_eq!(ranked.retried_attempts.len(), 1);
    assert_eq!(ranked.retried_attempts[0].http_status, Some(429));
    assert_eq!(server.await.unwrap().bodies.len(), 2);
}

#[tokio::test]
async fn two_transient_failures_fail_after_exactly_one_retry() {
    let (endpoint, server) = serve(vec![
        reply("502 Bad Gateway", "private-body"),
        reply("503 Service Unavailable", "private-body"),
    ])
    .await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let error = decide(&client).await.unwrap_err();
    let diagnostic = failure(&error).diagnostic();
    assert_eq!(diagnostic.kind, DecisionFailureKind::HttpStatus);
    assert_eq!(diagnostic.http_status, Some(503));
    assert_eq!(diagnostic.prior_attempts.len(), 1);
    assert_eq!(diagnostic.prior_attempts[0].http_status, Some(502));
    assert!(diagnostic.prior_attempts[0].reported_usage.is_none());
    let rendered = error.to_string();
    assert!(rendered.contains("after one bounded retry"), "{rendered}");
    assert!(rendered.contains("attempt 1: HTTP 502"), "{rendered}");
    assert!(rendered.contains("attempt 2: HTTP 503"), "{rendered}");
    assert!(rendered.contains("no further retry"), "{rendered}");
    assert!(!rendered.contains("private-body"));
    let served = server.await.unwrap();
    assert_eq!(served.bodies.len(), 2);
    assert_eq!(served.unexpected, 0, "never a second retry");
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 2);
    assert_eq!(snapshot.terminal_submissions, 2);
}

#[tokio::test]
async fn non_transient_statuses_are_sent_once() {
    for status in [
        "400 Bad Request",
        "401 Unauthorized",
        "403 Forbidden",
        "404 Not Found",
        "501 Not Implemented",
    ] {
        let (endpoint, server) = serve(vec![reply(status, "private-body")]).await;
        let context = context(Duration::from_secs(30), 5);
        let client = retrying(&endpoint, &context);
        let error = decide(&client).await.unwrap_err();
        let diagnostic = failure(&error).diagnostic();
        assert_eq!(diagnostic.kind, DecisionFailureKind::HttpStatus, "{status}");
        assert!(diagnostic.prior_attempts.is_empty(), "{status}");
        assert!(error.to_string().ends_with("request was not retried"));
        let served = server.await.unwrap();
        assert_eq!(served.bodies.len(), 1, "{status}");
        assert_eq!(served.unexpected, 0, "{status}");
        assert_eq!(context.snapshot().unwrap().logical_reservations, 1);
    }
}

#[tokio::test]
async fn clients_without_the_policy_keep_single_submission() {
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", "")]).await;
    let client = JevClient::with_endpoint(KEY, None, &endpoint).unwrap();
    assert!(client.transient_retry().is_none());
    let error = decide(&client).await.unwrap_err();
    assert!(failure(&error).diagnostic().prior_attempts.is_empty());
    assert_eq!(
        error.to_string(),
        "Jev provider returned HTTP 502; request was not retried"
    );
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    // An explicit opt-out restores the same behavior on a retrying client.
    let (endpoint, server) = serve(vec![reply("503 Service Unavailable", "")]).await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context).without_transient_retry();
    assert!(decide(&client).await.is_err());
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
}

#[tokio::test]
async fn numeric_retry_after_is_honoured_when_it_fits() {
    let (endpoint, server) = serve(vec![
        Reply {
            status: "503 Service Unavailable",
            headers: "Retry-After: 1\r\n",
            body: String::new(),
        },
        success(),
    ])
    .await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let started = Instant::now();
    let response = decide(&client).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    let failed = &response.retried_attempts[0];
    assert_eq!(failed.retry_after_ms, Some(1000));
    assert_eq!(failed.retry_wait_ms, 1000);
    assert_eq!(server.await.unwrap().bodies.len(), 2);
}

#[tokio::test]
async fn retry_after_beyond_the_original_deadline_refuses_the_retry() {
    let (endpoint, server) = serve(vec![Reply {
        status: "429 Too Many Requests",
        headers: "Retry-After: 30\r\n",
        body: String::new(),
    }])
    .await;
    // The 20-second request deadline cannot hold a 30-second wait plus 2 s.
    let context = context(Duration::from_secs(60), 5);
    let client = retrying(&endpoint, &context);
    let started = Instant::now();
    let error = decide(&client).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    let diagnostic = failure(&error).diagnostic();
    assert_eq!(diagnostic.http_status, Some(429));
    assert!(diagnostic.prior_attempts.is_empty());
    assert_eq!(failure(&error).retry_after(), Some(Duration::from_secs(30)));
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    assert_eq!(context.snapshot().unwrap().logical_reservations, 1);
}

#[tokio::test]
async fn short_original_deadline_refuses_the_retry() {
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", "")]).await;
    // Less than backoff + 2 s remains after the failed attempt.
    let context = context(Duration::from_millis(1500), 5);
    let client = retrying(&endpoint, &context);
    let error = decide(&client).await.unwrap_err();
    assert_eq!(failure(&error).diagnostic().http_status, Some(502));
    assert!(failure(&error).diagnostic().prior_attempts.is_empty());
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    assert_eq!(context.snapshot().unwrap().logical_reservations, 1);
}

#[tokio::test]
async fn exhausted_submission_budget_refuses_the_retry() {
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", "")]).await;
    let context = context(Duration::from_secs(30), 1);
    let client = retrying(&endpoint, &context);
    let error = decide(&client).await.unwrap_err();
    assert_eq!(failure(&error).diagnostic().http_status, Some(502));
    assert!(failure(&error).diagnostic().prior_attempts.is_empty());
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.max_submissions, 1);
}

#[tokio::test]
async fn deadline_or_timeout_is_never_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_body(&mut socket).await;
        let mut extra = 0;
        // Hold the first request open until the client gives up, then watch
        // for a replacement submission.
        while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(4), listener.accept()).await
        {
            extra += 1;
        }
        drop(socket);
        extra
    });
    let context = context(Duration::from_millis(2500), 5);
    let client = retrying(&endpoint, &context);
    let started = Instant::now();
    let error = decide(&client).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    let diagnostic = failure(&error).diagnostic();
    assert!(matches!(
        diagnostic.kind,
        DecisionFailureKind::Deadline | DecisionFailureKind::Timeout
    ));
    assert!(diagnostic.prior_attempts.is_empty());
    assert_eq!(server.await.unwrap(), 0);
    // The uncertain submission stays charged and quarantined.
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.quarantined_slots, 1);
}

#[tokio::test]
async fn truncated_success_body_is_never_retried() {
    // A response that ends before its declared length is a body failure: the
    // provider may have completed the request, so it is never repeated.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_body(&mut socket).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{\"partial\"")
            .await;
        drop(socket);
        let mut extra = 0;
        while let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
        {
            extra += 1;
        }
        extra
    });
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let error = decide(&client).await.unwrap_err();
    assert_eq!(
        failure(&error).diagnostic().kind,
        DecisionFailureKind::ResponseBody
    );
    assert!(failure(&error).diagnostic().prior_attempts.is_empty());
    assert_eq!(server.await.unwrap(), 0);
    assert_eq!(context.snapshot().unwrap().logical_reservations, 1);
}

#[tokio::test]
async fn invalid_success_body_is_never_retried() {
    let (endpoint, server) = serve(vec![reply("200 OK", "{not-json")]).await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let error = decide(&client).await.unwrap_err();
    assert_eq!(
        failure(&error).diagnostic().kind,
        DecisionFailureKind::InvalidJson
    );
    assert!(failure(&error).diagnostic().prior_attempts.is_empty());
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
}

#[derive(Default)]
struct Recorded {
    events: Mutex<Vec<&'static str>>,
    reject_second_send: bool,
    reject_validated: bool,
}

impl DecisionObserver for Recorded {
    fn send_started(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
        let mut events = self.events.lock().unwrap();
        let sends = events.iter().filter(|event| **event == "send").count();
        events.push("send");
        anyhow::ensure!(
            !(self.reject_second_send && sends == 1),
            "synthetic-observer-refusal"
        );
        Ok(())
    }
    fn validated_response(
        &self,
        _: &DecisionRequestObservation<'_>,
        _: &DecisionResponse,
    ) -> Result<()> {
        self.events.lock().unwrap().push("validated");
        anyhow::ensure!(!self.reject_validated, "synthetic-observer-refusal");
        Ok(())
    }
    fn failed(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
        self.events.lock().unwrap().push("failed");
        Ok(())
    }
}

#[tokio::test]
async fn observers_see_each_attempt_with_its_own_outcome() {
    let (endpoint, server) = serve(vec![reply("504 Gateway Timeout", ""), success()]).await;
    let observer = Arc::new(Recorded::default());
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context).with_decision_observer(observer.clone());
    decide(&client).await.unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["send", "failed", "send", "validated"]
    );
    assert_eq!(server.await.unwrap().bodies.len(), 2);
}

#[tokio::test]
async fn refused_retry_admission_keeps_the_first_attempt_and_its_charge() {
    let (endpoint, server) = serve(vec![reply("500 Internal Server Error", "")]).await;
    let observer = Arc::new(Recorded {
        reject_second_send: true,
        ..Recorded::default()
    });
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context).with_decision_observer(observer.clone());
    let error = decide(&client).await.unwrap_err();
    let diagnostic = failure(&error).diagnostic();
    assert_eq!(diagnostic.kind, DecisionFailureKind::RetryAdmission);
    assert_eq!(diagnostic.prior_attempts.len(), 1);
    assert_eq!(diagnostic.prior_attempts[0].http_status, Some(500));
    let rendered = error.to_string();
    assert!(rendered.contains("attempt 1: HTTP 500"), "{rendered}");
    assert!(
        rendered.contains("attempt 2: not admitted or sent"),
        "{rendered}"
    );
    assert!(!rendered.contains("synthetic-observer-refusal"));
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["send", "failed", "send", "failed"]
    );
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    // The refused retry still holds its own charged reservation.
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 2);
    assert_eq!(snapshot.send_start_observations, 2);
}

#[tokio::test]
async fn local_failure_after_the_retry_was_sent_is_not_reported_as_unsent() {
    // The retry reaches the provider and validates; a local step then fails.
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", ""), success()]).await;
    let observer = Arc::new(Recorded {
        reject_validated: true,
        ..Recorded::default()
    });
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context).with_decision_observer(observer.clone());
    let error = decide(&client).await.unwrap_err();
    let diagnostic = failure(&error).diagnostic();
    assert_eq!(diagnostic.kind, DecisionFailureKind::RetryPostSend);
    assert_eq!(diagnostic.prior_attempts.len(), 1);
    assert_eq!(diagnostic.prior_attempts[0].http_status, Some(502));
    // The validated retry's reported usage and identity are kept, not dropped.
    let usage = diagnostic.reported_usage.as_ref().unwrap();
    assert_eq!(usage.cost.value, Some(0.0002));
    assert_eq!(usage.input_tokens.value, Some(7));
    let metadata = diagnostic.response_metadata.as_ref().unwrap();
    assert_eq!(metadata.id.value.as_deref(), Some("synthetic-final"));
    let rendered = error.to_string();
    assert!(rendered.contains("attempt 1: HTTP 502"), "{rendered}");
    assert!(
        rendered.contains("attempt 2: sent and validated"),
        "{rendered}"
    );
    assert!(!rendered.contains("not admitted or sent"), "{rendered}");
    assert!(!rendered.contains("synthetic-observer-refusal"));
    assert_eq!(
        serde_json::to_value(diagnostic).unwrap()["kind"],
        "retry_post_send"
    );
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (2, 0));
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 2);
    assert_eq!(snapshot.terminal_submissions, 2);
}

#[tokio::test]
async fn retry_notice_runs_after_admission_and_before_the_retry_is_reserved() {
    let body = json!({"model":"openai/gpt-6-luna-decisions-20261006",
        "answers":{"candidate_0":{"type":"score","score":3.0}},
        "usage":{"input_tokens":9,"output_tokens":1,"cost":0.0003}})
    .to_string();
    let (endpoint, server) = serve(vec![
        reply("503 Service Unavailable", ""),
        reply("200 OK", body),
    ])
    .await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let noticed = Mutex::new(Vec::new());
    let notice = |failed: &gptgrep_jev::DecisionRetriedAttempt| -> Result<()> {
        // Only the failed first submission is reserved when the notice runs.
        let reserved = context.snapshot().unwrap().logical_reservations;
        noticed
            .lock()
            .unwrap()
            .push((failed.attempt, failed.http_status, reserved));
        Ok(())
    };
    let candidates = [Candidate {
        id: "synthetic".into(),
        text: "synthetic evidence".into(),
    }];
    let ranked = client
        .rerank_with_retry_notice("synthetic query", &candidates, &notice)
        .await
        .unwrap();
    assert_eq!(*noticed.lock().unwrap(), [(1, Some(503), 1)]);
    assert_eq!(ranked.retried_attempts.len(), 1);
    assert_eq!(server.await.unwrap().bodies.len(), 2);
    assert_eq!(context.snapshot().unwrap().logical_reservations, 2);
    // Without an admitted retry the notice is never called.
    let (endpoint, server) = serve(vec![reply("400 Bad Request", "")]).await;
    let context = self::context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let calls = Mutex::new(0);
    let counting = |_: &gptgrep_jev::DecisionRetriedAttempt| -> Result<()> {
        *calls.lock().unwrap() += 1;
        Ok(())
    };
    assert!(
        client
            .rerank_with_retry_notice("synthetic query", &candidates, &counting)
            .await
            .is_err()
    );
    assert_eq!(*calls.lock().unwrap(), 0);
    assert_eq!(server.await.unwrap().bodies.len(), 1);
}

#[tokio::test]
async fn refused_retry_notice_stops_the_retry_before_any_reservation() {
    let (endpoint, server) = serve(vec![reply("502 Bad Gateway", "")]).await;
    let context = context(Duration::from_secs(30), 5);
    let client = retrying(&endpoint, &context);
    let refuse = |_: &gptgrep_jev::DecisionRetriedAttempt| -> Result<()> {
        anyhow::bail!("synthetic-notice-refusal")
    };
    let error = client
        .rerank_with_retry_notice(
            "synthetic query",
            &[Candidate {
                id: "synthetic".into(),
                text: "synthetic evidence".into(),
            }],
            &refuse,
        )
        .await
        .unwrap_err();
    // The caller's own error is returned unchanged; nothing further is sent.
    assert_eq!(error.to_string(), "synthetic-notice-refusal");
    assert!(error.downcast_ref::<DecisionFailure>().is_none());
    let served = server.await.unwrap();
    assert_eq!((served.bodies.len(), served.unexpected), (1, 0));
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.terminal_submissions, 1);
}

#[test]
fn jitter_is_clamped_to_the_backoff_range() {
    for (unit, expected) in [
        (0.0, 250),
        (0.5, 625),
        (1.0, 1000),
        (-3.0, 250),
        (7.0, 1000),
        (f64::NAN, 250),
        (f64::INFINITY, 250),
    ] {
        let backoff = TransientRetry::with_jitter(move || unit).backoff();
        assert_eq!(backoff, Duration::from_millis(expected), "{unit}");
    }
    let standard = TransientRetry::standard();
    for _ in 0..64 {
        let backoff = standard.backoff();
        assert!((RETRY_BACKOFF_MIN..=RETRY_BACKOFF_MAX).contains(&backoff));
    }
    assert!(format!("{standard:?}").contains("max_retries"));
}
