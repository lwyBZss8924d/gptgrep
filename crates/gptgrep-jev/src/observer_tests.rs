use super::*;
use std::{collections::BTreeMap, sync::Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

#[derive(Default)]
struct Observed {
    sent: BTreeMap<String, (String, usize, usize)>,
    validated: BTreeMap<String, Value>,
    failed: Vec<String>,
}

#[derive(Default)]
struct Observer(Mutex<Observed>);

impl DecisionObserver for Observer {
    fn send_started(&self, request: &DecisionRequestObservation<'_>) -> Result<()> {
        let mut observed = self.0.lock().unwrap();
        ensure!(
            !observed.sent.contains_key(request.body_sha256),
            "duplicate_test_admission"
        );
        observed.sent.insert(
            request.body_sha256.into(),
            (
                request.requested_model.into(),
                request.body_bytes,
                request.question_count,
            ),
        );
        Ok(())
    }

    fn validated_response(
        &self,
        request: &DecisionRequestObservation<'_>,
        response: &DecisionResponse,
    ) -> Result<()> {
        let mut observed = self.0.lock().unwrap();
        ensure!(observed.sent.contains_key(request.body_sha256));
        ensure!(!observed.validated.contains_key(request.body_sha256));
        observed
            .validated
            .insert(request.body_sha256.into(), serde_json::to_value(response)?);
        Ok(())
    }

    fn failed(&self, request: &DecisionRequestObservation<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .failed
            .push(request.body_sha256.into());
        Ok(())
    }
}

fn questions() -> Value {
    json!({"support":{"type":"choice","instructions":"Is this supported?",
        "criteria":{"supported":"Supported","unsupported":"Unsupported"}}})
}

fn response() -> Value {
    json!({"model":"openai/gpt-6-luna-decisions-20261006",
        "answers":{"support":{"type":"choice","choice":"supported"}},
        "usage":{"cost":0.002,"input_tokens":9,"output_tokens":2},
        "id":"synthetic-response","provider":"synthetic-provider"})
}

async fn server(
    body: String,
    requests: usize,
    observer: Arc<Observer>,
) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/decisions", listener.local_addr().unwrap());
    let worker = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        let mut bodies = Vec::new();
        for _ in 0..requests {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let wire_body = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 128 * 1024);
                if let Some(offset) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..offset]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= offset + 4 + length {
                        break request[offset + 4..offset + 4 + length].to_vec();
                    }
                }
            };
            let digest = format!("{:x}", Sha256::digest(&wire_body));
            // Independent socket bytes must already have a matching admission.
            let expected = observer.0.lock().unwrap().sent[&digest].clone();
            assert_eq!(expected, (DEFAULT_MODEL.into(), wire_body.len(), 1));
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            bodies.push(wire_body);
        }
        bodies
        })
        .await
        .expect("synthetic observer transport exceeded its bounded deadline")
    });
    (endpoint, worker)
}

#[tokio::test]
async fn observer_binds_actual_wire_and_complete_validated_dto_before_projection() {
    let observer = Arc::new(Observer::default());
    let (endpoint, server) = server(response().to_string(), 1, Arc::clone(&observer)).await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
        .unwrap()
        .with_decision_observer(observer.clone());
    let prepared = client
        .prepare_decision(json!({"source":"ceramic"}), questions())
        .unwrap();
    let expected = prepared.canonical_body().to_vec();
    let digest = prepared.body_sha256().to_owned();
    let mut result = client.submit_prepared(prepared).await.unwrap();
    let complete = serde_json::to_value(&result).unwrap();
    result.usage = Value::Null;
    assert_eq!(server.await.unwrap(), vec![expected]);
    let observed = observer.0.lock().unwrap();
    assert_eq!(observed.validated[&digest], complete);
    assert_eq!(observed.validated[&digest]["usage"]["cost"], json!(0.002));
    assert_eq!(observed.validated[&digest]["id"], "synthetic-response");
    assert!(observed.failed.is_empty());
}

#[tokio::test]
async fn observer_never_promotes_failed_parse_or_invalid_answers_to_validated_cost() {
    for body in [
        "{invalid-json".to_owned(),
        json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":{},
            "usage":{"cost":0.002}})
        .to_string(),
    ] {
        let observer = Arc::new(Observer::default());
        let (endpoint, server) = server(body, 1, Arc::clone(&observer)).await;
        let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
            .unwrap()
            .with_decision_observer(observer.clone());
        assert!(
            client
                .decide(json!({"source":"ceramic"}), questions())
                .await
                .is_err()
        );
        assert_eq!(server.await.unwrap().len(), 1);
        let observed = observer.0.lock().unwrap();
        assert_eq!(observed.sent.len(), 1);
        assert_eq!(observed.failed.len(), 1);
        assert!(observed.validated.is_empty());
    }
}

#[tokio::test]
async fn cloned_clients_keep_concurrent_wire_and_response_identity_separate() {
    let observer = Arc::new(Observer::default());
    let (endpoint, server) = server(response().to_string(), 2, Arc::clone(&observer)).await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
        .unwrap()
        .with_decision_observer(observer.clone());
    let other = client.clone();
    let (first, second) = tokio::join!(
        client.decide(json!({"source":"ceramic"}), questions()),
        other.decide(json!({"source":"steel"}), questions())
    );
    assert!(first.is_ok() && second.is_ok());
    let bodies = server.await.unwrap();
    assert_eq!(bodies.len(), 2);
    assert_ne!(bodies[0], bodies[1]);
    let observed = observer.0.lock().unwrap();
    assert_eq!(observed.sent.len(), 2);
    assert_eq!(observed.validated.len(), 2);
    assert!(observed.sent.keys().eq(observed.validated.keys()));
    assert!(observed.failed.is_empty());
}

#[tokio::test]
async fn attaching_observer_invalidates_an_already_prepared_client_identity() {
    let observer = Arc::new(Observer::default());
    let client =
        JevClient::with_endpoint("synthetic-test-key", None, "http://127.0.0.1:9/decisions")
            .unwrap();
    let prepared = client
        .prepare_decision(json!({"source":"ceramic"}), questions())
        .unwrap();
    let client = client.with_decision_observer(observer.clone());
    assert!(
        client
            .submit_prepared(prepared)
            .await
            .unwrap_err()
            .to_string()
            .contains("different client")
    );
    assert!(observer.0.lock().unwrap().sent.is_empty());
}

struct BodyObserver {
    metadata: Arc<Observer>,
    expected: Vec<u8>,
    reject: bool,
    delay: Duration,
}

impl DecisionObserver for BodyObserver {
    fn send_started(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
        anyhow::bail!("body_callback_required")
    }

    fn send_started_with_body(
        &self,
        request: &DecisionRequestObservation<'_>,
        canonical_body: &[u8],
    ) -> Result<()> {
        ensure!(canonical_body == self.expected, "body_mismatch");
        ensure!(canonical_body.len() == request.body_bytes);
        ensure!(format!("{:x}", Sha256::digest(canonical_body)) == request.body_sha256);
        self.metadata.send_started(request)?;
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        ensure!(!self.reject, "body_admission_rejected");
        Ok(())
    }

    fn validated_response(
        &self,
        request: &DecisionRequestObservation<'_>,
        response: &DecisionResponse,
    ) -> Result<()> {
        self.metadata.validated_response(request, response)
    }

    fn failed(&self, request: &DecisionRequestObservation<'_>) -> Result<()> {
        self.metadata.failed(request)
    }
}

#[tokio::test]
async fn borrowed_body_observer_receives_exact_wire_once_before_http() {
    let metadata = Arc::new(Observer::default());
    let (endpoint, server) = server(response().to_string(), 1, Arc::clone(&metadata)).await;
    let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint).unwrap();
    let expected = client
        .prepare_decision(json!({"source":"ceramic"}), questions())
        .unwrap()
        .canonical_body()
        .to_vec();
    let client = client.with_decision_observer(Arc::new(BodyObserver {
        metadata: Arc::clone(&metadata),
        expected: expected.clone(),
        reject: false,
        delay: Duration::ZERO,
    }));
    client
        .decide(json!({"source":"ceramic"}), questions())
        .await
        .unwrap();
    assert_eq!(server.await.unwrap(), vec![expected]);
    let observed = metadata.0.lock().unwrap();
    assert_eq!(observed.sent.len(), 1);
    assert_eq!(observed.validated.len(), 1);
    assert!(observed.failed.is_empty());
}

#[tokio::test]
async fn borrowed_body_rejection_or_elapsed_admission_prevents_http_and_keeps_attempt() {
    for elapsed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/decisions", listener.local_addr().unwrap());
        let metadata = Arc::new(Observer::default());
        let context = ExecutionContext::new(
            "borrowed-body-admission-test",
            &"a".repeat(64),
            std::time::SystemTime::now() + Duration::from_secs(10),
            1,
        )
        .unwrap();
        let client = JevClient::with_endpoint("synthetic-test-key", None, &endpoint)
            .unwrap()
            .with_execution_context(context.clone());
        let expected = client
            .prepare_decision(json!({"source":"ceramic"}), questions())
            .unwrap()
            .canonical_body()
            .to_vec();
        let client = client.with_decision_observer(Arc::new(BodyObserver {
            metadata: Arc::clone(&metadata),
            expected,
            reject: !elapsed,
            delay: if elapsed {
                Duration::from_millis(100)
            } else {
                Duration::ZERO
            },
        }));
        let client = if elapsed {
            client.with_execution_context(
                context
                    .with_deadline(std::time::Instant::now() + Duration::from_millis(50))
                    .unwrap(),
            )
        } else {
            client
        };
        let error = client
            .decide(json!({"source":"ceramic"}), questions())
            .await
            .unwrap_err();
        if elapsed {
            assert!(error.to_string().contains("expired during send admission"));
        } else {
            let diagnostic = error
                .downcast_ref::<DecisionFailure>()
                .unwrap()
                .diagnostic();
            assert_eq!(diagnostic.kind, DecisionFailureKind::ObserverCallback);
            let observer = diagnostic.observer_failure.as_ref().unwrap();
            assert_eq!(observer.callbacks, [DecisionObserverCallback::SendStarted]);
            assert!(!observer.send_started);
            assert!(!observer.response_validated);
            assert!(!error.to_string().contains("body_admission_rejected"));
            assert!(!format!("{error:?}").contains("body_admission_rejected"));
            assert!(
                !serde_json::to_string(diagnostic)
                    .unwrap()
                    .contains("body_admission_rejected")
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let observed = metadata.0.lock().unwrap();
        assert_eq!(observed.sent.len(), 1);
        assert_eq!(observed.failed.len(), 1);
        assert!(observed.validated.is_empty());
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.logical_reservations, 1);
        assert_eq!(snapshot.submission_observations, 1);
        assert_eq!(snapshot.send_start_observations, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
    }
}
