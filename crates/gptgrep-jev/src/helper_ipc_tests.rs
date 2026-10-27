// Included only by the Linux transport. All credentials and providers are synthetic.
mod tests {
    use super::*;
    use crate::{DecisionFailure, DecisionFailureKind, DecisionObserverCallback, SourceBinding};
    use serde_json::json;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::AtomicUsize,
    };

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
    fn private_json(path: &Path, value: &impl Serialize) -> FilePin {
        let bytes = serde_json::to_vec(value).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        FilePin {
            path: path.to_owned(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        }
    }
    struct Fixture {
        _root: tempfile::TempDir,
        client: JevClient,
        control: ClientControl,
        execution: ExecutionContext,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<Result<Value>>,
    }
    impl Fixture {
        async fn start(provider: Option<JevClient>, key: Option<&str>) -> Self {
            Self::limited(provider, key, 20).await
        }
        async fn limited(
            provider: Option<JevClient>,
            key: Option<&str>,
            max_submissions: u32,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            // Exercise Linux descriptor addressing beyond sockaddr_un capacity.
            let endpoint = root
                .path()
                .join("long-physical-component-".repeat(4))
                .join("endpoint");
            std::fs::create_dir_all(&endpoint).unwrap();
            std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(endpoint.as_os_str().as_bytes().len() > 108);
            let deadline = SystemTime::now() + Duration::from_secs(60);
            let deadline_ms =
                u64::try_from(deadline.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
            let execution = ExecutionContext::new(
                "helper-synthetic",
                &"a".repeat(64),
                deadline,
                max_submissions,
            )
            .unwrap()
            .with_new_ledger(root.path().join("ledger.jsonl"))
            .unwrap();
            let budget = DecisionsBudget {
                schema_version: crate::DECISIONS_BUDGET_CONTRACT_SCHEMA.into(),
                run_id: "helper-synthetic".into(),
                plan_digest: "a".repeat(64),
                deadline_unix_ms: deadline_ms,
                max_submissions,
                ledger_path: execution.ledger_path().unwrap().to_owned(),
            };
            let control = ServerControl {
                schema_version: SERVER_SCHEMA.into(),
                task_binding_sha256: "b".repeat(64),
                socket_parent: endpoint.clone(),
                parent_identity: node(&std::fs::metadata(&endpoint).unwrap()),
                client_visible_parent: endpoint.clone(),
                client_control_path: endpoint.join("client.json"),
                budget: private_json(&root.path().join("budget.json"), &budget),
                owner_binding: FilePin {
                    path: root.path().join("unused-owner.json"),
                    sha256: "c".repeat(64),
                },
                peer: PeerScope::EngineeringDescendants {
                    ancestor: process(std::process::id()).unwrap(),
                },
                model: crate::DEFAULT_MODEL.into(),
                provider_disabled: true,
            };
            let control_path = control.client_control_path.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let (ready, announced) = tokio::sync::oneshot::channel();
            let key = key.map(str::to_owned);
            let task = tokio::spawn(serve_inner(
                control,
                key,
                None,
                provider,
                async move {
                    let _ = stopped.await;
                },
                move |value| {
                    let _ = ready.send(value);
                    Ok(())
                },
                true,
            ));
            let ready = tokio::time::timeout(Duration::from_secs(3), announced)
                .await
                .unwrap()
                .unwrap();
            let pin = FilePin {
                path: control_path,
                sha256: ready["client_control_sha256"].as_str().unwrap().to_owned(),
            };
            let control = read_client_control(&pin).unwrap();
            let client = HelperClient {
                remote: RemoteContext::new(control.clone()),
            }
            .into_client()
            .unwrap();
            Self {
                _root: root,
                client,
                control,
                execution,
                stop: Some(stop),
                task,
            }
        }
        async fn finish(mut self) -> Value {
            let _ = self.stop.take().unwrap().send(());
            tokio::time::timeout(Duration::from_secs(3), self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        }
    }

    async fn mock(replies: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/decisions", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for (status, reply) in replies {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(6), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut part = [0u8; 4096];
                    let count = socket.read(&mut part).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&part[..count]);
                    assert!(request.len() <= MAX_MULTIMODAL_REQUEST_BYTES + 8192);
                    if let Some(offset) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..offset]).to_ascii_lowercase();
                        let length: usize = head
                            .lines()
                            .find_map(|v| v.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if request.len() >= offset + 4 + length {
                            bodies.push(request[offset + 4..offset + 4 + length].to_vec());
                            break;
                        }
                    }
                }
                let wire = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                socket.write_all(wire.as_bytes()).await.unwrap();
            }
            bodies
        });
        (endpoint, task)
    }
    fn provider(endpoint: &str) -> JevClient {
        JevClient::with_endpoint("synthetic-private-key", None, endpoint).unwrap()
    }

    struct Observe {
        context: ExecutionContext,
        events: Mutex<Vec<String>>,
        refuse: Option<DecisionObserverCallback>,
        delay: Duration,
    }
    impl Observe {
        fn event(&self, name: &str, callback: DecisionObserverCallback) -> Result<()> {
            let snapshot = self.context.snapshot()?;
            ensure!(
                snapshot.logical_reservations >= 1 && snapshot.send_start_observations >= 1,
                "Callback lost the authoritative server state"
            );
            ensure!(
                snapshot.scope.starts_with("helper_server_event:"),
                "Snapshot is not as-of-event"
            );
            self.events.lock().unwrap().push(name.to_owned());
            if !self.delay.is_zero() && name == "send" {
                std::thread::sleep(self.delay);
            }
            ensure!(
                self.refuse != Some(callback),
                "synthetic original callback refusal"
            );
            Ok(())
        }
    }
    impl DecisionObserver for Observe {
        fn send_started(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
            self.event("send", DecisionObserverCallback::SendStarted)
        }
        fn validated_response(
            &self,
            _: &DecisionRequestObservation<'_>,
            response: &DecisionResponse,
        ) -> Result<()> {
            assert_eq!(response.usage["cost"], json!(0.002));
            self.event("validated", DecisionObserverCallback::ValidatedResponse)
        }
        fn failed(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
            self.event("failed", DecisionObserverCallback::Failed)
        }
    }
    fn observer(
        context: ExecutionContext,
        refuse: Option<DecisionObserverCallback>,
        delay: Duration,
    ) -> Arc<Observe> {
        Arc::new(Observe {
            context,
            events: Mutex::new(Vec::new()),
            refuse,
            delay,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_success_uses_single_server_ledger_and_original_clones() {
        let (endpoint, http) = mock(vec![(200, response().to_string())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let budget = &fixture.control.budget;
        let context = ExecutionContext::new(
            &budget.run_id,
            &budget.plan_digest,
            UNIX_EPOCH + Duration::from_millis(budget.deadline_unix_ms),
            budget.max_submissions,
        )
        .unwrap();
        let before = context.clone();
        let client = fixture.client.clone().with_execution_context(context);
        let observed = observer(before.clone(), None, Duration::ZERO);
        let client = client.with_decision_observer(observed.clone());
        let result = client
            .decide(json!({"source":"ceramic"}), questions())
            .await
            .unwrap();
        assert_eq!(result.usage["cost"], json!(0.002));
        assert_eq!(*observed.events.lock().unwrap(), ["send", "validated"]);
        assert_eq!(http.await.unwrap().len(), 1);
        assert_eq!(before.snapshot().unwrap().logical_reservations, 1);
        assert_eq!(
            fixture.execution.snapshot().unwrap().terminal_submissions,
            1
        );
        assert!(
            before
                .reserve(&"e".repeat(64), crate::DEFAULT_MODEL)
                .await
                .is_err()
        );
        assert_eq!(fixture.finish().await["terminal"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_retry_keeps_exact_body_order_and_trusted_prior() {
        let (endpoint, http) = mock(vec![(429, "{}".into()), (200, response().to_string())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let observed = observer(
            fixture.client.execution_context().clone(),
            None,
            Duration::ZERO,
        );
        let client = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone());
        let notice_observed = observed.clone();
        let notice = move |prior: &DecisionRetriedAttempt| {
            assert_eq!(prior.http_status, Some(429));
            notice_observed.events.lock().unwrap().push("retry".into());
            assert_eq!(notice_observed.context.snapshot()?.logical_reservations, 1);
            Ok(())
        };
        let prepared = client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let result = client
            .submit_prepared_with(prepared, Some(&TransientRetry::standard()), Some(&notice))
            .await
            .unwrap();
        assert_eq!(result.retried_attempts.len(), 1);
        assert_eq!(
            *observed.events.lock().unwrap(),
            ["send", "failed", "retry", "send", "validated"]
        );
        let bodies = http.await.unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
        let snapshot = fixture.execution.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.send_start_observations,
                snapshot.terminal_submissions
            ),
            (2, 2, 2)
        );
        fixture.finish().await;
    }

    #[derive(Debug)]
    struct NoticeRefusal;
    impl std::fmt::Display for NoticeRefusal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("synthetic original notice refusal")
        }
    }
    impl std::error::Error for NoticeRefusal {}
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_retry_notice_refusal_preserves_original_error_and_stops_send() {
        let (endpoint, http) = mock(vec![(503, "{}".into())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let prepared = fixture
            .client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let notice = |_: &DecisionRetriedAttempt| Err(NoticeRefusal.into());
        let error = fixture
            .client
            .submit_prepared_with(prepared, Some(&TransientRetry::standard()), Some(&notice))
            .await
            .unwrap_err();
        assert!(error.is::<NoticeRefusal>());
        assert_eq!(http.await.unwrap().len(), 1);
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            1
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_failed_observer_refusal_keeps_transient_http_failure_without_retry_send() {
        let (endpoint, http) = mock(vec![(503, "{}".into())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let observed = observer(
            fixture.client.execution_context().clone(),
            Some(DecisionObserverCallback::Failed),
            Duration::ZERO,
        );
        let client = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone())
            .with_transient_retry(TransientRetry::standard());
        let retry_calls = AtomicUsize::new(0);
        let notice = |_: &DecisionRetriedAttempt| {
            retry_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let prepared = client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let error = client
            .submit_prepared_with(prepared, client.retry.as_ref(), Some(&notice))
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<DecisionFailure>().unwrap();
        assert_eq!(failure.diagnostic().kind, DecisionFailureKind::HttpStatus);
        assert_eq!(failure.diagnostic().http_status, Some(503));
        assert_eq!(
            failure
                .diagnostic()
                .observer_failure
                .as_ref()
                .unwrap()
                .callbacks,
            [DecisionObserverCallback::Failed]
        );
        assert_eq!(retry_calls.load(Ordering::SeqCst), 0);
        assert_eq!(*observed.events.lock().unwrap(), ["send", "failed"]);
        assert_eq!(http.await.unwrap().len(), 1);
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            1
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_observer_refusal_suppresses_http_and_retains_charged_unknown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let observed = observer(
            fixture.client.execution_context().clone(),
            Some(DecisionObserverCallback::SendStarted),
            Duration::ZERO,
        );
        let error = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone())
            .decide(json!("source"), questions())
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<DecisionFailure>().unwrap();
        assert_eq!(
            failure.diagnostic().kind,
            DecisionFailureKind::ObserverCallback
        );
        assert_eq!(
            failure
                .diagnostic()
                .observer_failure
                .as_ref()
                .unwrap()
                .callbacks,
            [DecisionObserverCallback::SendStarted]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        let snapshot = fixture.execution.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.send_start_observations,
                snapshot.quarantined_slots
            ),
            (1, 1, 1)
        );
        assert_eq!(*observed.events.lock().unwrap(), ["send", "failed"]);
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_validated_callback_failure_preserves_numeric_usage() {
        let (endpoint, http) = mock(vec![(200, response().to_string())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let observed = observer(
            fixture.client.execution_context().clone(),
            Some(DecisionObserverCallback::ValidatedResponse),
            Duration::ZERO,
        );
        let error = fixture
            .client
            .clone()
            .with_decision_observer(observed)
            .decide(json!("source"), questions())
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<DecisionFailure>().unwrap();
        assert_eq!(
            failure
                .diagnostic()
                .reported_usage
                .as_ref()
                .unwrap()
                .cost
                .value,
            Some(0.002)
        );
        assert!(
            failure
                .diagnostic()
                .observer_failure
                .as_ref()
                .unwrap()
                .response_validated
        );
        http.await.unwrap();
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_callback_over_one_second_keeps_original_twenty_second_budget() {
        let (endpoint, http) = mock(vec![(200, response().to_string())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let observed = observer(
            fixture.client.execution_context().clone(),
            None,
            Duration::from_millis(1100),
        );
        let start = Instant::now();
        fixture
            .client
            .clone()
            .with_decision_observer(observed)
            .decide(json!("source"), questions())
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(1100));
        http.await.unwrap();
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn helper_four_parallel_callbacks_are_reentrant_and_status_is_readonly() {
        struct Parallel {
            entered: std::sync::Barrier,
            release: std::sync::Barrier,
            context: ExecutionContext,
            calls: AtomicUsize,
            snapshots: Mutex<std::collections::BTreeMap<String, ExecutionSnapshot>>,
        }
        impl DecisionObserver for Parallel {
            fn send_started(&self, request: &DecisionRequestObservation<'_>) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let before = self.context.snapshot()?;
                ensure!(
                    before.scope.starts_with("helper_server_event:"),
                    "Missing callback event identity"
                );
                self.entered.wait();
                self.release.wait();
                assert_eq!(
                    self.context.snapshot()?,
                    before,
                    "Another concurrent call replaced this callback's snapshot"
                );
                assert!(
                    self.snapshots
                        .lock()
                        .unwrap()
                        .insert(request.body_sha256.into(), before)
                        .is_none()
                );
                Ok(())
            }
            fn validated_response(
                &self,
                _: &DecisionRequestObservation<'_>,
                _: &DecisionResponse,
            ) -> Result<()> {
                Ok(())
            }
            fn failed(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
                Ok(())
            }
        }
        let (endpoint, http) = mock((0..4).map(|_| (200, response().to_string())).collect()).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let observed = Arc::new(Parallel {
            entered: std::sync::Barrier::new(5),
            release: std::sync::Barrier::new(5),
            context: fixture.client.execution_context().clone(),
            calls: AtomicUsize::new(0),
            snapshots: Mutex::new(std::collections::BTreeMap::new()),
        });
        let client = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone());
        let mut calls = tokio::task::JoinSet::new();
        for i in 0..4 {
            let client = client.clone();
            calls.spawn(async move { client.decide(json!({"source":i}), questions()).await });
        }
        tokio::task::block_in_place(|| observed.entered.wait());
        // Call the actual readonly transport directly, without a client snapshot cache.
        let snapshot = tokio::task::block_in_place(|| status(&fixture.control)).unwrap();
        assert_eq!(snapshot.logical_reservations, 4);
        assert_eq!(snapshot.submission_observations, 4);
        tokio::task::block_in_place(|| observed.release.wait());
        while let Some(call) = calls.join_next().await {
            call.unwrap().unwrap();
        }
        assert_eq!(observed.calls.load(Ordering::SeqCst), 4);
        let scopes: std::collections::BTreeSet<_> = observed
            .snapshots
            .lock()
            .unwrap()
            .values()
            .map(|value| value.scope.clone())
            .collect();
        assert_eq!(
            scopes.len(),
            4,
            "Every callback must retain its own server event"
        );
        assert_eq!(
            fixture
                .client
                .execution_context()
                .snapshot()
                .unwrap()
                .logical_reservations,
            4
        );
        assert_eq!(http.await.unwrap().len(), 4);
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_privacy_gate_rejects_unicode_reflection_before_response_write() {
        let key = "synthetic-private-key";
        let mut answer = response();
        answer["usage"]["untrusted"] = json!({"nested":key});
        let escaped = answer.to_string().replace(
            key,
            &key.chars()
                .map(|v| format!("\\u{:04x}", v as u32))
                .collect::<String>(),
        );
        let (endpoint, http) = mock(vec![(200, escaped)]).await;
        let fixture = Fixture::start(Some(provider(&endpoint)), Some(key)).await;
        let error = fixture
            .client
            .decide(json!("source"), questions())
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<DecisionFailure>().unwrap();
        assert_eq!(
            failure
                .diagnostic()
                .reported_usage
                .as_ref()
                .unwrap()
                .cost
                .value,
            Some(0.002)
        );
        assert!(failure.diagnostic().response_metadata.is_none());
        assert!(!error.to_string().contains(key));
        http.await.unwrap();
        fixture.finish().await;
    }

    #[test]
    fn helper_guard_screens_raw_and_decoded_keys_and_values() {
        let guard = SecretGuard(Some("synthetic-private-key".into()));
        for value in [
            json!({"usage":"synthetic-private-key"}),
            json!({"synthetic-private-key":7}),
            json!({"prior":[{"nested":"synthetic-private-key"}]}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(!guard.safe(&bytes));
            let escaped = String::from_utf8(bytes)
                .unwrap()
                .replace("synthetic-private-key", "\\u0073ynthetic-private-key");
            assert!(!guard.safe(escaped.as_bytes()));
        }
        assert!(guard.safe(br#"{"usage":{"cost":0.25}}"#));
    }

    #[test]
    fn helper_error_frame_roundtrips_float_usage_prior_and_closed_enums() {
        let usage = crate::FailedResponseUsage::from_response(
            &json!({"usage":{"cost":0.002,"input_tokens":9}}),
        );
        let metadata = crate::FailedResponseMetadata::from_response(&response());
        let direct = DecisionFailure::local_post_send(Some((usage.clone(), metadata.clone())));
        let restored_direct = DecisionFailure::from_helper_wire(direct.helper_wire()).unwrap();
        assert_eq!(direct.to_string(), restored_direct.to_string());
        assert!(
            restored_direct
                .to_string()
                .contains("reported usage is retained")
        );
        let prior = DecisionRetriedAttempt {
            attempt: 1,
            kind: DecisionFailureKind::HttpStatus,
            http_status: Some(429),
            reported_usage: Some(usage.clone()),
            retry_wait_ms: 250,
            retry_after_ms: None,
        };
        let failure = DecisionFailure::local_post_send(Some((usage, metadata)))
            .with_prior_attempts(vec![prior]);
        let context = ExecutionContext::compatibility();
        let frame = ServerFrame {
            request: "a".repeat(64),
            sequence: 1,
            snapshot: context.snapshot().unwrap(),
            event: Event::Error {
                failure: failure.helper_wire(),
            },
        };
        let bytes = serde_json::to_vec(&frame).unwrap();
        let restored: ServerFrame = unique(&bytes).unwrap();
        let Event::Error { failure } = restored.event else {
            panic!("error frame lost")
        };
        let failure = DecisionFailure::from_helper_wire(failure).unwrap();
        assert_eq!(
            failure
                .diagnostic()
                .reported_usage
                .as_ref()
                .unwrap()
                .cost
                .value,
            Some(0.002)
        );
        assert_eq!(
            failure.diagnostic().prior_attempts[0]
                .reported_usage
                .as_ref()
                .unwrap()
                .cost
                .value,
            Some(0.002)
        );
        let malformed = String::from_utf8(bytes)
            .unwrap()
            .replace("local_post_send", "invented_variant");
        assert!(unique::<ServerFrame>(malformed.as_bytes()).is_err());
    }

    #[test]
    fn helper_response_bound_retains_limit_escapes_numbers_and_trusted_retry() {
        let mut value = response();
        value["usage"]["metadata"] = json!("\u{0}\u{1}\"\\".repeat(40_000));
        value["usage"]["tiny"] = serde_json::from_str("1.234567890123456e-300").unwrap();
        value["usage"]["padding"] = json!("");
        let overhead = serde_json::to_vec(&value).unwrap().len();
        value["usage"]["padding"] = json!("x".repeat(MAX_RESPONSE_BYTES - overhead));
        let raw = serde_json::to_vec(&value).unwrap();
        assert_eq!(raw.len(), MAX_RESPONSE_BYTES);
        let validated = crate::validation::validate_response(
            crate::parse_unique_json(&raw).unwrap(),
            &questions(),
        )
        .unwrap();
        let (bytes, prior) = response_body(&validated).unwrap();
        assert!(bytes.len() <= RESPONSE_WIRE_MAX);
        let decoded = decode_response(&bytes, &questions(), crate::DEFAULT_MODEL, prior).unwrap();
        assert_eq!(decoded.usage, value["usage"]);
        let injected = bytes.strip_suffix(b"}").unwrap();
        let mut injected = injected.to_vec();
        injected.extend_from_slice(br#", "retried_attempts":[{"attempt":999}]}"#);
        let decoded =
            decode_response(&injected, &questions(), crate::DEFAULT_MODEL, Vec::new()).unwrap();
        assert!(decoded.retried_attempts.is_empty());
    }

    #[test]
    fn helper_private_control_rejects_symlinks_hardlinks_and_changed_pin() {
        let root = tempfile::tempdir().unwrap();
        let pin = private_json(&root.path().join("control.json"), &json!({"value":1}));
        assert_eq!(read_pin::<Value>(&pin, 4096).unwrap(), json!({"value":1}));
        let alias = root.path().join("alias.json");
        symlink(&pin.path, &alias).unwrap();
        assert!(
            read_pin::<Value>(
                &FilePin {
                    path: alias,
                    sha256: pin.sha256.clone()
                },
                4096
            )
            .is_err()
        );
        std::fs::hard_link(&pin.path, root.path().join("linked.json")).unwrap();
        assert!(read_pin::<Value>(&pin, 4096).is_err());
        let other = private_json(&root.path().join("other.json"), &json!({"value":2}));
        assert!(
            read_pin::<Value>(
                &FilePin {
                    path: other.path,
                    sha256: "f".repeat(64)
                },
                4096
            )
            .is_err()
        );
    }

    #[test]
    fn helper_partial_frames_never_renew_absolute_deadline() {
        let (mut reader, mut writer) = StdStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            for value in [0, 0, 0, 1, b'x'] {
                if writer.write_all(&[value]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let start = Instant::now();
        let deadline = deadline_after(Duration::from_millis(70)).unwrap();
        assert!(read_frame(&mut reader, 64, deadline).is_err());
        assert!(start.elapsed() < Duration::from_millis(180));
        drop(reader);
        worker.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_bad_headers_and_provider_disabled_probe_never_reserve() {
        let fixture = Fixture::start(None, None).await;
        assert_eq!(
            probe(&fixture.control).await.unwrap().logical_reservations,
            0
        );
        for bytes in [
            br#"{"version":1,"version":1}"#.to_vec(),
            serde_json::to_vec(&WireHeader {
                version: 1,
                instance: "f".repeat(32),
                task_binding_sha256: "b".repeat(64),
                request: "a".repeat(64),
                operation: Operation::Status,
            })
            .unwrap(),
        ] {
            let deadline = initial_deadline(&fixture.control).unwrap();
            let (mut stream, _) = connect_async(&fixture.control, deadline).await.unwrap();
            write_frame_async(&mut stream, &bytes, METADATA_MAX, deadline)
                .await
                .unwrap();
            assert!(
                read_frame_async(&mut stream, METADATA_MAX, deadline)
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            0
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_wrong_explicit_budget_fails_before_any_ipc_reservation() {
        let fixture = Fixture::start(None, None).await;
        let context = ExecutionContext::new(
            "another-run",
            &"a".repeat(64),
            SystemTime::now() + Duration::from_secs(20),
            20,
        )
        .unwrap();
        let client = fixture.client.clone().with_execution_context(context);
        assert!(
            client
                .prepare_decision(json!("source"), questions())
                .is_err()
        );
        assert_eq!(
            fixture
                .execution
                .snapshot()
                .unwrap()
                .submission_observations,
            0
        );
        fixture.finish().await;
    }

    #[test]
    fn helper_held_init_pidfd_refuses_terminal_generation() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let init = process(child.id()).unwrap();
        let init_fd = pidfd(init.pid).unwrap();
        alive(&init_fd).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(alive(&init_fd).is_err());
        let own = process(std::process::id()).unwrap();
        let lease = PeerLease {
            fd: pidfd(own.pid).unwrap(),
            scope_fd: init_fd,
            identity: own,
            scope: PeerScope::Container { init },
            uid: unsafe { libc::getuid() },
        };
        assert!(lease.check().unwrap_err().to_string().contains("terminal"));
    }

    #[test]
    fn helper_typed_flat_codec_matches_source_bound_image_preparation() {
        let client = JevClient::new("synthetic-test-key", None).unwrap();
        let mut image_bytes = Vec::new();
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut image_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let source = SourceBinding {
            source_id: "synthetic-source".into(),
            unit_id: "page-1".into(),
            source_sha256: format!("{:x}", Sha256::digest(&image_bytes)),
        };
        let image = crate::ImageEvidence::from_bytes(source, image_bytes).unwrap();
        let input = crate::DecisionInput {
            text: Vec::new(),
            images: vec![image],
        };
        let prepared = client
            .prepare_input(input, questions(), MAX_RESPONSE_BYTES)
            .unwrap();
        let restored =
            prepare_server(&client, PreparedBodyMode::TypedInput, prepared.body.clone()).unwrap();
        assert_eq!(prepared.body, restored.body);
        let mut invalid = crate::parse_unique_json(&restored.body).unwrap();
        invalid["state"][0]["image_url"]["url"] = json!("https://unadmitted.invalid/image.png");
        assert!(
            prepare_server(
                &client,
                PreparedBodyMode::TypedInput,
                serde_json::to_vec(&invalid).unwrap()
            )
            .is_err()
        );
    }

    fn png(width: u32, height: u32, noise: bool) -> Vec<u8> {
        use image::ImageEncoder;
        let mut seed = 0x5978_2da1u32;
        let pixels: Vec<u8> = (0..width as usize * height as usize * 3)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if noise { seed as u8 } else { 0 }
            })
            .collect();
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }
    fn image_input(bytes: Vec<u8>) -> crate::DecisionInput {
        let source = SourceBinding {
            source_id: "synthetic-source".into(),
            unit_id: "image".into(),
            source_sha256: format!("{:x}", Sha256::digest(&bytes)),
        };
        crate::DecisionInput {
            text: Vec::new(),
            images: vec![crate::ImageEvidence::from_bytes(source, bytes).unwrap()],
        }
    }
    async fn raw_submit(
        fixture: &Fixture,
        bytes: &[u8],
        mode: PreparedBodyMode,
        duration: Duration,
    ) -> (tokio::net::UnixStream, String) {
        let deadline = deadline_after(duration).unwrap();
        let header = header(
            &fixture.control,
            Operation::Submit {
                mode,
                body_sha256: format!("{:x}", Sha256::digest(bytes)),
                body_bytes: bytes.len(),
                deadline_monotonic_ns: deadline,
                retry: false,
            },
        );
        let (mut stream, _) = connect_async(&fixture.control, deadline).await.unwrap();
        write_frame_async(
            &mut stream,
            &serde_json::to_vec(&header).unwrap(),
            METADATA_MAX,
            deadline,
        )
        .await
        .unwrap();
        write_frame_async(&mut stream, bytes, MAX_MULTIMODAL_REQUEST_BYTES, deadline)
            .await
            .unwrap();
        (stream, header.request)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_both_image_apis_send_beyond_ordinary_body_limit_without_reencoding() {
        let (endpoint, http) = mock(vec![
            (200, response().to_string()),
            (200, response().to_string()),
        ])
        .await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let input = image_input(png(256, 256, true));
        let ordinary = fixture
            .client
            .prepare_input(input.clone(), questions(), MAX_RESPONSE_BYTES)
            .unwrap();
        let qualification = fixture
            .client
            .prepare_image_qualification(input, questions(), MAX_RESPONSE_BYTES)
            .unwrap();
        assert!(
            ordinary.body_bytes() > MAX_REQUEST_BYTES
                && qualification.request().body_bytes() > MAX_REQUEST_BYTES
        );
        let expected = ordinary.canonical_body().to_vec();
        assert_eq!(qualification.request().canonical_body(), expected);
        fixture.client.submit_prepared(ordinary).await.unwrap();
        fixture
            .client
            .submit_prepared(qualification.into_request())
            .await
            .unwrap();
        let bodies = http.await.unwrap();
        assert_eq!(bodies, [expected.clone(), expected]);
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            2
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_server_rechecks_image_count_bytes_pixels_total_envelope_and_model_before_reserve()
     {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let prepared = fixture
            .client
            .prepare_input(
                image_input(png(2, 2, false)),
                questions(),
                MAX_RESPONSE_BYTES,
            )
            .unwrap();
        let base = crate::parse_unique_json(prepared.canonical_body()).unwrap();
        let image = base["state"][0].clone();
        let mut cases = Vec::new();
        let mut count = base.clone();
        count["state"] = Value::Array(vec![image.clone(); crate::MAX_IMAGES + 1]);
        cases.push(count);
        let mut oversized = base.clone();
        oversized["state"][0]["image_url"]["url"] = json!(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(vec![1; crate::MAX_IMAGE_BYTES + 1])
        ));
        let error = crate::input::validate_helper_flat_body(
            &fixture.client,
            &serde_json::to_vec(&oversized).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Decisions image exceeded the file byte limit"
        );
        cases.push(oversized);
        let mut encoded_limit = base.clone();
        encoded_limit["state"][0]["image_url"]["url"] = json!(format!(
            "data:image/png;base64,{}",
            "A".repeat(crate::MAX_IMAGE_BASE64_BYTES + 1)
        ));
        let error = crate::input::validate_helper_flat_body(
            &fixture.client,
            &serde_json::to_vec(&encoded_limit).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Decisions image exceeded the base64 byte limit"
        );
        cases.push(encoded_limit);
        let mut pixels = base.clone();
        pixels["state"][0]["image_url"]["url"] = json!(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(3000, 3000, false))
        ));
        cases.push(pixels);
        let total_image = json!({"image_url":{"url":format!("data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(2800,2800,false)))},"type":"image_url"});
        let mut total = base.clone();
        total["state"] = Value::Array(vec![total_image; 4]);
        cases.push(total);
        let mut envelope = base.clone();
        envelope["state"] = json!(["\u{0}".repeat(12_000), image]);
        cases.push(envelope);
        let mut model = base.clone();
        model["model"] = json!(crate::LEGACY_MODEL);
        cases.push(model);
        for value in cases {
            let bytes = serde_json::to_vec(&value).unwrap();
            let (mut stream, _) = raw_submit(
                &fixture,
                &bytes,
                PreparedBodyMode::TypedInput,
                Duration::from_secs(10),
            )
            .await;
            assert!(
                read_frame_async(
                    &mut stream,
                    METADATA_MAX,
                    deadline_after(Duration::from_secs(10)).unwrap()
                )
                .await
                .is_err()
            );
            assert_eq!(
                fixture.execution.snapshot().unwrap().logical_reservations,
                0
            );
        }
        let legacy = JevClient::new("synthetic-private-key", Some(crate::LEGACY_MODEL)).unwrap();
        assert!(prepare_server(&legacy, PreparedBodyMode::TypedInput, prepared.body).is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_accepts_exact_full_ordinary_request_limit() {
        let (endpoint, http) = mock(vec![(200, response().to_string())]).await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let base = fixture
            .client
            .prepare_decision(json!("x"), questions())
            .unwrap()
            .body_bytes();
        let prepared = fixture
            .client
            .prepare_decision(json!("x".repeat(MAX_REQUEST_BYTES - base + 1)), questions())
            .unwrap();
        assert_eq!(prepared.body_bytes(), MAX_REQUEST_BYTES);
        fixture.client.submit_prepared(prepared).await.unwrap();
        assert_eq!(http.await.unwrap()[0].len(), MAX_REQUEST_BYTES);
        fixture.finish().await;
    }

    fn full_response() -> (Value, Vec<u8>) {
        let mut questions = serde_json::Map::new();
        let mut answers = serde_json::Map::new();
        for index in 0..crate::MAX_QUESTIONS {
            let id = format!("q{index}");
            questions.insert(id.clone(),json!({"type":"score","instructions":"Assess support","criteria":["none","direct"]}));
            answers.insert(
                id,
                json!({"type":"score","score":0,"confidence":1,
                "probabilities":{"0":1,"1":0},"legend":{"0":"none","1":"direct"}}),
            );
        }
        let mut value = json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,
            "usage":{"cost":0.002,"input_tokens":9007199254740993u64,"output_tokens":2,
                "padding":"","tiny":1.234567890123456e-300},"id":"synthetic-response","provider":"synthetic-provider"});
        let overhead = serde_json::to_vec(&value).unwrap().len();
        value["usage"]["padding"] = json!("x".repeat(MAX_RESPONSE_BYTES - overhead));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(bytes.len(), MAX_RESPONSE_BYTES);
        (Value::Object(questions), bytes)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_full_response_limit_numeric_expansion_sixty_four_answers_and_retry_cross_ipc() {
        let (questions, body) = full_response();
        let validated = crate::validation::validate_response(
            crate::parse_unique_json(&body).unwrap(),
            &questions,
        )
        .unwrap();
        let (expanded, _) = response_body(&validated).unwrap();
        assert!(expanded.len() > MAX_RESPONSE_BYTES && expanded.len() <= RESPONSE_WIRE_MAX);
        let (endpoint, http) = mock(vec![
            (429, "{}".into()),
            (200, String::from_utf8(body).unwrap()),
        ])
        .await;
        let fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let client = fixture
            .client
            .clone()
            .with_transient_retry(TransientRetry::standard());
        let result = client
            .decide(json!("synthetic source"), questions)
            .await
            .unwrap();
        assert_eq!(result.answers.len(), crate::MAX_QUESTIONS);
        assert_eq!(result.usage["input_tokens"], json!(9007199254740993u64));
        assert_eq!(result.retried_attempts.len(), 1);
        assert_eq!(result.retried_attempts[0].http_status, Some(429));
        assert_eq!(http.await.unwrap().len(), 2);
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            2
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_stalled_ack_expires_original_deadline_without_http_or_replay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let prepared = fixture
            .client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let start = Instant::now();
        let (mut stream, _) = raw_submit(
            &fixture,
            prepared.canonical_body(),
            PreparedBodyMode::Ordinary,
            Duration::from_millis(120),
        )
        .await;
        let frame: ServerFrame = unique(
            &read_frame_async(
                &mut stream,
                METADATA_MAX,
                deadline_after(Duration::from_millis(120)).unwrap(),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(frame.event, Event::BeforeSend { .. }));
        assert!(
            read_frame_async(
                &mut stream,
                METADATA_MAX,
                deadline_after(Duration::from_millis(500)).unwrap()
            )
            .await
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(700));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let snapshot = fixture.execution.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.send_start_observations,
                snapshot.quarantined_slots
            ),
            (1, 1, 1)
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_disconnect_before_header_and_after_reservation_preserves_counts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let (mut empty, _) = connect_async(
            &fixture.control,
            initial_deadline(&fixture.control).unwrap(),
        )
        .await
        .unwrap();
        empty.write_all(&[0, 0]).await.unwrap();
        drop(empty);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            fixture
                .execution
                .snapshot()
                .unwrap()
                .submission_observations,
            0
        );
        let prepared = fixture
            .client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let (mut stream, _) = raw_submit(
            &fixture,
            prepared.canonical_body(),
            PreparedBodyMode::Ordinary,
            Duration::from_secs(2),
        )
        .await;
        let frame: ServerFrame = unique(
            &read_frame_async(
                &mut stream,
                METADATA_MAX,
                deadline_after(Duration::from_secs(2)).unwrap(),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(frame.event, Event::BeforeSend { .. }));
        drop(stream);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let snapshot = fixture.execution.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.submission_observations,
                snapshot.quarantined_slots
            ),
            (1, 1, 1)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn helper_bounded_waiters_expire_and_ninth_submit_is_refused_while_status_works() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let prepared = fixture
            .client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let mut active = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = raw_submit(
                &fixture,
                prepared.canonical_body(),
                PreparedBodyMode::Ordinary,
                Duration::from_secs(5),
            )
            .await;
            let frame: ServerFrame = unique(
                &read_frame_async(
                    &mut stream,
                    METADATA_MAX,
                    deadline_after(Duration::from_secs(2)).unwrap(),
                )
                .await
                .unwrap(),
            )
            .unwrap();
            assert!(matches!(frame.event, Event::BeforeSend { .. }));
            active.push(stream);
        }
        let mut waiting = Vec::new();
        for _ in 0..4 {
            waiting.push(
                raw_submit(
                    &fixture,
                    prepared.canonical_body(),
                    PreparedBodyMode::Ordinary,
                    Duration::from_millis(350),
                )
                .await
                .0,
            );
        }
        let snapshot = tokio::task::block_in_place(|| status(&fixture.control)).unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.submission_observations
            ),
            (4, 4)
        );
        let (mut overflow, _) = raw_submit(
            &fixture,
            prepared.canonical_body(),
            PreparedBodyMode::Ordinary,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            read_frame_async(
                &mut overflow,
                METADATA_MAX,
                deadline_after(Duration::from_secs(1)).unwrap()
            )
            .await
            .is_err()
        );
        for mut stream in waiting {
            assert!(
                read_frame_async(
                    &mut stream,
                    METADATA_MAX,
                    deadline_after(Duration::from_secs(1)).unwrap()
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            4
        );
        drop(active);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(fixture.execution.snapshot().unwrap().quarantined_slots, 4);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_parent_and_socket_leaf_replacement_refuse_before_connect_or_reserve() {
        let fixture = Fixture::start(None, None).await;
        let parent = fixture.control.socket_parent.clone();
        let retained = parent.with_file_name("retained-original-parent");
        std::fs::rename(&parent, &retained).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(status(&fixture.control).is_err());
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            0
        );
        fixture.finish().await;

        let fixture = Fixture::start(None, None).await;
        let endpoint = Endpoint::open(
            &fixture.control.socket_parent,
            &fixture.control.parent_identity,
        )
        .unwrap();
        let address = endpoint.address().unwrap();
        std::fs::remove_file(&address).unwrap();
        let foreign = StdListener::bind(&address).unwrap();
        std::fs::set_permissions(&address, std::fs::Permissions::from_mode(0o600)).unwrap();
        foreign.set_nonblocking(true).unwrap();
        assert!(status(&fixture.control).is_err());
        assert!(foreign.accept().is_err());
        assert_eq!(
            fixture.execution.snapshot().unwrap().logical_reservations,
            0
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_exhausted_budget_is_admission_failure_without_extra_observer_or_post() {
        let (endpoint, http) = mock(vec![(200, response().to_string())]).await;
        let fixture =
            Fixture::limited(Some(provider(&endpoint)), Some("synthetic-private-key"), 1).await;
        fixture
            .client
            .decide(json!("first"), questions())
            .await
            .unwrap();
        assert_eq!(http.await.unwrap().len(), 1);
        let observed = observer(
            fixture.client.execution_context().clone(),
            None,
            Duration::ZERO,
        );
        let error = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone())
            .decide(json!("second"), questions())
            .await
            .unwrap_err();
        assert!(
            error.downcast_ref::<DecisionFailure>().is_none(),
            "Admission failure invented provider/observer progress"
        );
        assert_eq!(error.to_string(), "Decisions submission budget exhausted");
        assert!(observed.events.lock().unwrap().is_empty());
        let snapshot = fixture.execution.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.logical_reservations,
                snapshot.send_start_observations,
                snapshot.terminal_submissions
            ),
            (1, 1, 1)
        );
        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_unavailable_pre_observer_ledger_never_fabricates_observer_or_snapshot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut fixture = Fixture::start(
            Some(provider(&format!(
                "http://{}/decisions",
                listener.local_addr().unwrap()
            ))),
            Some("synthetic-private-key"),
        )
        .await;
        let path = fixture.execution.ledger_path().unwrap().to_owned();
        std::fs::rename(&path, path.with_extension("retained")).unwrap();
        let observed = observer(
            fixture.client.execution_context().clone(),
            None,
            Duration::ZERO,
        );
        let error = fixture
            .client
            .clone()
            .with_decision_observer(observed.clone())
            .decide(json!("source"), questions())
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<DecisionFailure>().is_none());
        assert!(observed.events.lock().unwrap().is_empty());
        assert!(fixture.client.execution_context().snapshot().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let _ = fixture.stop.take().unwrap().send(());
        assert!(
            fixture.task.await.unwrap().is_err(),
            "Missing ledger produced a fabricated terminal snapshot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_cancel_after_observed_http_send_keeps_unknown_without_replay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/decisions", listener.local_addr().unwrap());
        let (sent, observed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let http = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut part = [0; 4096];
                let count = socket.read(&mut part).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&part[..count]);
                assert!(bytes.len() < MAX_REQUEST_BYTES + 8192);
                if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let len: usize = header
                        .lines()
                        .find_map(|value| value.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + len {
                        let _ = sent.send(len);
                        break;
                    }
                }
            }
            let _ = released.await;
            let body = response().to_string();
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(wire.as_bytes()).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(80), listener.accept())
                    .await
                    .is_err()
            );
            1usize
        });
        let mut fixture =
            Fixture::start(Some(provider(&endpoint)), Some("synthetic-private-key")).await;
        let prepared = fixture
            .client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let (mut stream, request) = raw_submit(
            &fixture,
            prepared.canonical_body(),
            PreparedBodyMode::Ordinary,
            Duration::from_secs(3),
        )
        .await;
        let frame: ServerFrame = unique(
            &read_frame_async(
                &mut stream,
                METADATA_MAX,
                deadline_after(Duration::from_secs(2)).unwrap(),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(frame.event, Event::BeforeSend { .. }));
        let ack = Ack {
            request,
            sequence: frame.sequence,
            proceed: true,
        };
        write_frame_async(
            &mut stream,
            &serde_json::to_vec(&ack).unwrap(),
            METADATA_MAX,
            deadline_after(Duration::from_secs(2)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), observed)
                .await
                .unwrap()
                .unwrap(),
            prepared.body_bytes()
        );
        drop(stream);
        let _ = fixture.stop.take().unwrap().send(());
        let terminal = tokio::time::timeout(Duration::from_secs(3), fixture.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(terminal["snapshot"]["logical_reservations"], 1);
        assert_eq!(terminal["snapshot"]["send_start_observations"], 1);
        assert_eq!(terminal["snapshot"]["quarantined_slots"], 1);
        assert_eq!(terminal["snapshot"]["terminal_submissions"], 0);
        let _ = release.send(());
        assert_eq!(http.await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn helper_retry_notice_original_error_survives_lost_ack_channel() {
        // Isolated client-wire fixture: the real SDK retry ordering/accounting is
        // tested separately above. Here the peer deliberately closes at the notice.
        let mut fixture = Fixture::start(None, None).await;
        let _ = fixture.stop.take().unwrap().send(());
        fixture.task.await.unwrap().unwrap();
        let mut control = fixture.control.clone();
        let endpoint = Endpoint::open(&control.socket_parent, &control.parent_identity).unwrap();
        let address = endpoint.address().unwrap();
        std::fs::remove_file(&address).unwrap();
        let listener = StdListener::bind(&address).unwrap();
        std::fs::set_permissions(&address, std::fs::Permissions::from_mode(0o600)).unwrap();
        control.socket_identity = endpoint.leaf().unwrap();
        let mut snapshot = fixture.execution.snapshot().unwrap();
        snapshot.logical_reservations = 1;
        snapshot.send_start_observations = 1;
        snapshot.submission_observations = 1;
        snapshot.terminal_submissions = 1;
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let deadline = deadline_after(Duration::from_secs(3)).unwrap();
            let header: WireHeader =
                unique(&read_frame(&mut stream, METADATA_MAX, deadline).unwrap()).unwrap();
            read_frame(&mut stream, MAX_REQUEST_BYTES, deadline).unwrap();
            let prior = DecisionRetriedAttempt {
                attempt: 1,
                kind: DecisionFailureKind::HttpStatus,
                http_status: Some(503),
                reported_usage: None,
                retry_wait_ms: 250,
                retry_after_ms: None,
            };
            let frame = ServerFrame {
                request: header.request,
                sequence: 1,
                snapshot,
                event: Event::BeforeRetry { prior },
            };
            write_frame(
                &mut stream,
                &serde_json::to_vec(&frame).unwrap(),
                METADATA_MAX,
                deadline,
            )
            .unwrap();
            stream.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let client = HelperClient {
            remote: RemoteContext::new(control),
        }
        .into_client()
        .unwrap();
        let notice = |_: &DecisionRetriedAttempt| {
            std::thread::sleep(Duration::from_millis(30));
            Err(NoticeRefusal.into())
        };
        let prepared = client
            .prepare_decision(json!("source"), questions())
            .unwrap();
        let error = client
            .submit_prepared_with(prepared, Some(&TransientRetry::standard()), Some(&notice))
            .await
            .unwrap_err();
        assert!(error.is::<NoticeRefusal>());
        peer.join().unwrap();
    }
}
