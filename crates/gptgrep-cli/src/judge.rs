//! `gptgrep judge`: one explicit Decisions submission with sanitized failure accounting.
//!
//! A failure keeps the requested selector, the served model and provider response
//! ID when a response was parsed, numeric usage/cost or an explicit reason for
//! their absence, whether the SDK reached its pre-send boundary, and latency.
//! Request/response bodies, headers, URLs and credentials are never retained.

use anyhow::Result;
use gptgrep_jev::{
    DecisionFailure, DecisionFailureDiagnostic, DecisionFailureKind, DecisionObserver,
    DecisionRequestObservation, DecisionResponse, ExecutionSnapshot, JevClient, UsagePresence,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub(crate) const JUDGE_FAILURE_SCHEMA: &str = "gptgrep.judge-failure.v1";
pub(crate) const JUDGE_FAILURE_CODE: &str = "decisions_judge_failed";

/// Records only that the SDK reached its pre-send boundary. It never borrows
/// the request body (the default body hook delegates to `send_started`).
#[derive(Default)]
struct SendProbe {
    send_started: AtomicBool,
}

impl DecisionObserver for SendProbe {
    fn send_started(&self, _request: &DecisionRequestObservation<'_>) -> Result<()> {
        self.send_started.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn validated_response(
        &self,
        _request: &DecisionRequestObservation<'_>,
        _response: &DecisionResponse,
    ) -> Result<()> {
        Ok(())
    }
    fn failed(&self, _request: &DecisionRequestObservation<'_>) -> Result<()> {
        Ok(())
    }
}

/// Numeric usage values; null means not reported (see `usage_presence`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct UsageValues {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct UsagePresenceView {
    pub input_tokens: UsagePresence,
    pub output_tokens: UsagePresence,
    pub cost: UsagePresence,
}

/// Sanitized `gptgrep judge` failure accounting.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JudgeFailureReport {
    pub schema_version: &'static str,
    /// `input`, `client`, `preparation` or `submission`.
    pub stage: &'static str,
    /// The requested selector; null if it was not a bounded identifier.
    pub requested_model: Option<String>,
    /// `not_submitted`, `sent_response_received` or `sent_outcome_unknown`.
    pub submission: &'static str,
    /// Served build returned by a parsed but rejected response.
    pub served_model: Option<String>,
    /// Provider response (generation) ID returned by a parsed response.
    pub provider_response_id: Option<String>,
    pub provider: Option<String>,
    pub usage: Option<UsageValues>,
    pub usage_presence: Option<UsagePresenceView>,
    pub usage_unavailable_reason: Option<&'static str>,
    /// Reported provider cost metadata in USD; never inferred, never zero-filled.
    pub cost_usd: Option<f64>,
    pub cost_unavailable_reason: Option<&'static str>,
    pub failure: Option<DecisionFailureDiagnostic>,
    /// Duration of the submission call (admission, HTTP and validation).
    pub latency_ms: Option<u64>,
    pub elapsed_ms: u64,
    /// Local admission observations of the shared or single-invocation context.
    pub execution: Option<ExecutionSnapshot>,
}

#[derive(Debug)]
pub(crate) struct JudgeFailure {
    message: String,
    report: JudgeFailureReport,
}

impl JudgeFailure {
    pub(crate) fn report(&self) -> &JudgeFailureReport {
        &self.report
    }

    /// A failure before any client could submit: nothing was sent.
    pub(crate) fn before_submission(
        stage: &'static str,
        requested: &str,
        error: &anyhow::Error,
        started: Instant,
        execution: Option<ExecutionSnapshot>,
    ) -> Self {
        Self::build(stage, requested, error, false, None, started, execution)
    }

    fn build(
        stage: &'static str,
        requested: &str,
        error: &anyhow::Error,
        send_started: bool,
        latency_ms: Option<u64>,
        started: Instant,
        execution: Option<ExecutionSnapshot>,
    ) -> Self {
        let diagnostic = error
            .downcast_ref::<DecisionFailure>()
            .map(|failure| failure.diagnostic().clone());
        let response_received = diagnostic.as_ref().is_some_and(|d| {
            matches!(
                d.kind,
                DecisionFailureKind::HttpStatus
                    | DecisionFailureKind::ResponseSize
                    | DecisionFailureKind::InvalidJson
                    | DecisionFailureKind::ResponseSchema
                    | DecisionFailureKind::ResponseValidation
            )
        });
        let submission = if !send_started {
            "not_submitted"
        } else if response_received {
            "sent_response_received"
        } else {
            "sent_outcome_unknown"
        };
        let metadata = diagnostic
            .as_ref()
            .and_then(|d| d.response_metadata.as_ref());
        let reported = diagnostic.as_ref().and_then(|d| d.reported_usage.as_ref());
        let unavailable = if !send_started {
            "not_submitted"
        } else {
            match diagnostic.as_ref().map(|d| d.kind) {
                Some(DecisionFailureKind::HttpStatus) => "http_status_body_not_read",
                Some(DecisionFailureKind::ResponseSize) => "response_exceeded_byte_limit",
                Some(DecisionFailureKind::InvalidJson) => "response_invalid_json",
                Some(DecisionFailureKind::ResponseSchema)
                | Some(DecisionFailureKind::ResponseValidation) => "usage_not_reported",
                _ => "outcome_unknown_no_response",
            }
        };
        let usage = reported.map(|usage| UsageValues {
            input_tokens: usage.input_tokens.value,
            output_tokens: usage.output_tokens.value,
            cost: usage.cost.value,
        });
        let usage_presence = reported.map(|usage| UsagePresenceView {
            input_tokens: usage.input_tokens.presence,
            output_tokens: usage.output_tokens.presence,
            cost: usage.cost.presence,
        });
        let cost_usd = reported.and_then(|usage| usage.cost.value);
        let cost_unavailable_reason = match reported {
            Some(usage) if usage.cost.value.is_some() => None,
            Some(usage) => Some(match usage.cost.presence {
                UsagePresence::Absent => "cost_absent",
                UsagePresence::Null => "cost_null",
                UsagePresence::Invalid => "cost_invalid",
                UsagePresence::Reported => "cost_invalid",
            }),
            None => Some(unavailable),
        };
        let reported_value = |field: Option<&gptgrep_jev::UsageObservation<String>>| {
            field.and_then(|observation| observation.value.clone())
        };
        Self {
            message: format!("{error:#}"),
            report: JudgeFailureReport {
                schema_version: JUDGE_FAILURE_SCHEMA,
                stage,
                requested_model: bounded_identifier(requested),
                submission,
                served_model: reported_value(metadata.map(|m| &m.model)),
                provider_response_id: reported_value(metadata.map(|m| &m.id)),
                provider: reported_value(metadata.map(|m| &m.provider)),
                usage_unavailable_reason: usage.is_none().then_some(unavailable),
                usage,
                usage_presence,
                cost_usd,
                cost_unavailable_reason,
                failure: diagnostic,
                latency_ms,
                elapsed_ms: elapsed_ms(started),
                execution,
            },
        }
    }
}

impl std::fmt::Display for JudgeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JudgeFailure {}

fn bounded_identifier(value: &str) -> Option<String> {
    (!value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Prepare and submit once. Success returns the validated response JSON, whose
/// `model` is the served build, plus `requested_model`.
pub(crate) async fn run(
    client: JevClient,
    data: &Value,
    started: Instant,
) -> std::result::Result<Value, JudgeFailure> {
    let requested = client.requested_model().to_owned();
    let probe = Arc::new(SendProbe::default());
    let client = client.with_decision_observer(probe.clone());
    let snapshot = |client: &JevClient| client.execution_context().snapshot().ok();
    let prepared = match crate::prepare_judge(&client, data) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(JudgeFailure::before_submission(
                "preparation",
                &requested,
                &error,
                started,
                snapshot(&client),
            ));
        }
    };
    let submitted = Instant::now();
    let result = client.submit_prepared(prepared).await;
    let latency_ms = Some(elapsed_ms(submitted));
    match result {
        Ok(response) => {
            let mut value = serde_json::to_value(response).map_err(|error| {
                JudgeFailure::build(
                    "submission",
                    &requested,
                    &error.into(),
                    true,
                    latency_ms,
                    started,
                    snapshot(&client),
                )
            })?;
            value["requested_model"] = json!(requested);
            Ok(value)
        }
        Err(error) => Err(JudgeFailure::build(
            "submission",
            &requested,
            &error,
            probe.send_started.load(Ordering::SeqCst),
            latency_ms,
            started,
            snapshot(&client),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::AtomicUsize,
        time::Duration,
    };

    const KEY: &str = "synthetic-judge-key-sentinel";

    /// Same-process loopback fixture. Every accepted connection is counted; the
    /// scripted responses answer the first connections and later ones are closed.
    fn serve(responses: Vec<(&'static str, String)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/api/alpha/decisions",
            listener.local_addr().unwrap()
        );
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else {
                    continue;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let Some((status, body)) = responses.next() else {
                    continue;
                };
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).unwrap_or(0);
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
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (endpoint, accepted)
    }

    /// Fixed 1x1 PNG; generic synthetic content only.
    const ONE_PIXEL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn request() -> Value {
        json!({"state":{"note":"Synthetic offline judge fixture."},
            "questions":{"q":{"type":"noul","instructions":"Is the note synthetic?"}}})
    }

    fn assert_sanitized(value: &Value) {
        let text = value.to_string();
        assert!(!text.contains(KEY));
        assert!(!text.contains("private-body-sentinel"));
        assert!(!text.contains("Synthetic offline judge fixture"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alias_success_reports_requested_and_served_models_separately() {
        let body = json!({"model":"typesafe/jev-1.13-20260917","id":"gen-synthetic-1",
            "provider":"Synthetic","answers":{"q":{"type":"noul","noul":0.9}},
            "usage":{"input_tokens":21,"output_tokens":1,"cost":0.000001}})
        .to_string();
        let (endpoint, accepted) = serve(vec![("200 OK", body)]);
        let client =
            JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint).unwrap();
        let value = run(client, &request(), Instant::now()).await.unwrap();
        assert_eq!(value["requested_model"], "~typesafe/jev-latest");
        assert_eq!(value["model"], "typesafe/jev-1.13-20260917");
        assert_eq!(value["usage"]["cost"], 0.000001);
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        assert_sanitized(&value);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alias_family_mismatch_keeps_served_model_usage_id_and_latency() {
        let body = json!({"model":"other/jev-1.13-20260917","id":"gen-synthetic-2",
            "provider":"Synthetic","answers":{"q":{"type":"noul","noul":0.9}},
            "usage":{"input_tokens":21,"output_tokens":1,"cost":0.000002},
            "private":"private-body-sentinel"})
        .to_string();
        let (endpoint, _) = serve(vec![("200 OK", body)]);
        let client =
            JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint).unwrap();
        let failure = run(client, &request(), Instant::now()).await.unwrap_err();
        let report = serde_json::to_value(failure.report()).unwrap();
        assert_eq!(report["schema_version"], JUDGE_FAILURE_SCHEMA);
        assert_eq!(report["stage"], "submission");
        assert_eq!(report["submission"], "sent_response_received");
        assert_eq!(report["requested_model"], "~typesafe/jev-latest");
        assert_eq!(report["served_model"], "other/jev-1.13-20260917");
        assert_eq!(report["provider_response_id"], "gen-synthetic-2");
        assert_eq!(report["provider"], "Synthetic");
        assert_eq!(report["cost_usd"], 0.000002);
        assert!(report["cost_unavailable_reason"].is_null());
        assert_eq!(report["usage"]["input_tokens"], 21);
        assert_eq!(report["usage_presence"]["cost"], "reported");
        assert!(report["usage_unavailable_reason"].is_null());
        assert_eq!(report["failure"]["validation_code"], "model_mismatch");
        assert!(report["latency_ms"].as_u64().is_some());
        assert_eq!(report["execution"]["logical_reservations"], 1);
        assert_eq!(report["execution"]["terminal_submissions"], 1);
        assert_eq!(
            failure.to_string(),
            "Decisions response model does not match the requested model"
        );
        assert_sanitized(&report);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn null_cost_and_http_failures_are_explicit_never_zero() {
        let body = json!({"model":"typesafe/jev-1.13-20260917",
            "answers":{"q":{"type":"noul"}},
            "usage":{"input_tokens":5,"output_tokens":null,"cost":null}})
        .to_string();
        let (endpoint, _) = serve(vec![
            ("200 OK", body),
            ("402 Payment Required", "private-body-sentinel".into()),
        ]);
        let client = JevClient::with_endpoint(KEY, Some(gptgrep_jev::LEGACY_MODEL), &endpoint)
            .unwrap()
            .with_execution_context(
                gptgrep_jev::ExecutionContext::new(
                    "synthetic-judge-run",
                    &"b".repeat(64),
                    std::time::SystemTime::now() + Duration::from_secs(10),
                    2,
                )
                .unwrap(),
            );
        let schema = run(client.clone(), &request(), Instant::now())
            .await
            .unwrap_err();
        let report = serde_json::to_value(schema.report()).unwrap();
        assert_eq!(report["failure"]["validation_code"], "schema");
        assert!(report["cost_usd"].is_null());
        assert_eq!(report["cost_unavailable_reason"], "cost_null");
        assert_eq!(report["usage"]["input_tokens"], 5);
        assert!(report["usage"]["cost"].is_null());
        assert_eq!(report["served_model"], "typesafe/jev-1.13-20260917");
        assert!(report["provider_response_id"].is_null());
        assert_sanitized(&report);
        let http = run(client, &request(), Instant::now()).await.unwrap_err();
        let report = serde_json::to_value(http.report()).unwrap();
        assert_eq!(report["submission"], "sent_response_received");
        assert_eq!(report["failure"]["http_status"], 402);
        assert!(report["usage"].is_null());
        assert_eq!(
            report["usage_unavailable_reason"],
            "http_status_body_not_read"
        );
        assert_eq!(
            report["cost_unavailable_reason"],
            "http_status_body_not_read"
        );
        assert!(report["served_model"].is_null());
        assert_eq!(report["execution"]["logical_reservations"], 2);
        assert_sanitized(&report);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exhausted_budget_and_alias_images_fail_before_any_post() {
        let body = json!({"model":"openai/gpt-6-luna-decisions-20261006",
            "answers":{"q":{"type":"noul","noul":0.5}}})
        .to_string();
        let (endpoint, accepted) = serve(vec![("200 OK", body)]);
        let context = gptgrep_jev::ExecutionContext::new(
            "synthetic-judge-exhaustion",
            &"c".repeat(64),
            std::time::SystemTime::now() + Duration::from_secs(10),
            1,
        )
        .unwrap();
        let client = JevClient::with_endpoint(KEY, None, &endpoint)
            .unwrap()
            .with_execution_context(context.clone());
        assert!(
            run(client.clone(), &request(), Instant::now())
                .await
                .is_ok()
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        let failure = run(client, &request(), Instant::now()).await.unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("Decisions submission budget exhausted")
        );
        let report = serde_json::to_value(failure.report()).unwrap();
        assert_eq!(report["submission"], "not_submitted");
        assert_eq!(report["cost_unavailable_reason"], "not_submitted");
        assert!(report["failure"].is_null());
        assert!(report["latency_ms"].as_u64().is_some());
        assert_eq!(context.snapshot().unwrap().logical_reservations, 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("synthetic.png");
        std::fs::write(&image, ONE_PIXEL_PNG).unwrap();
        let typed = json!({"input":{"images":[{"source":{"source_id":"synthetic","unit_id":"image",
            "source_sha256":"a".repeat(64)},"path":image}]},
            "questions":{"q":{"type":"noul","instructions":"Is it visible?"}}});
        let client =
            JevClient::with_endpoint(KEY, Some("~typesafe/jev-latest"), &endpoint).unwrap();
        let failure = run(client, &typed, Instant::now()).await.unwrap_err();
        assert_eq!(
            failure.to_string(),
            "Decisions images require the Luna Decisions model; no request was prepared or submitted"
        );
        let report = serde_json::to_value(failure.report()).unwrap();
        assert_eq!(report["stage"], "preparation");
        assert_eq!(report["submission"], "not_submitted");
        assert_eq!(report["execution"]["logical_reservations"], 0);
        assert_eq!(report["execution"]["submission_observations"], 0);
        assert!(report["latency_ms"].is_null());
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unbounded_requested_identifiers_are_not_echoed() {
        let error = anyhow::anyhow!("Invalid Jev model identifier");
        let failure =
            JudgeFailure::before_submission("client", "bad\nmodel", &error, Instant::now(), None);
        assert!(failure.report().requested_model.is_none());
        let failure = JudgeFailure::before_submission(
            "client",
            &"m".repeat(257),
            &error,
            Instant::now(),
            None,
        );
        assert!(failure.report().requested_model.is_none());
        assert_eq!(failure.report().submission, "not_submitted");
    }
}
