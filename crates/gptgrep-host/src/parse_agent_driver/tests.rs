//! Synthetic protocol tests, authored only. These are not native/model proof.
use super::*;
use std::sync::{Arc, Mutex};
const PRIVATE_BACKEND_ERROR: &str = "MOCK-private-backend-error /synthetic/private/source";
struct Captured(Arc<Mutex<Vec<u8>>>);
impl AsyncWrite for Captured {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0
            .lock()
            .expect("synthetic writer")
            .extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
struct Backend {
    prepared: DocumentPreparedState,
    calls: usize,
    finalized: bool,
    refuse: bool,
    application_refusal: bool,
    images: Vec<DocumentImageGrant>,
}
impl Backend {
    fn new() -> Self {
        Self {
            prepared: DocumentPreparedState {
                schema: "gptgrep.parse-agent.prepared-document.v1",
                handle: "a".repeat(64),
                task_sha256: "d".repeat(64),
                source_sha256: "b".repeat(64),
                prepared_sha256: "c".repeat(64),
                original_pages: 137,
                artifact_count: 300,
                native_metadata_artifacts: 138,
                required_ocr_tasks: 0,
                original_rendition_artifacts: 0,
            },
            calls: 0,
            finalized: false,
            refuse: false,
            application_refusal: false,
            images: Vec::new(),
        }
    }
}
impl DocumentBackend for Backend {
    fn prepared(&self) -> &DocumentPreparedState {
        &self.prepared
    }
    fn images(&self) -> &[DocumentImageGrant] {
        &self.images
    }
    fn reserve_retained(&self, _: u64) -> Result<()> {
        Ok(())
    }
    fn call<'a>(
        &'a mut self,
        operation: DocumentToolOperation,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentToolReply>> + Send + 'a>> {
        self.calls += 1;
        Box::pin(async move {
            if self.refuse {
                return Err(anyhow!(PRIVATE_BACKEND_ERROR)
                    .context("MOCK-private-backend-context")
                    .context("parse_agent_operation_failed"));
            }
            if self.application_refusal
                && matches!(
                    operation,
                    DocumentToolOperation::ReadArtifact { .. }
                        | DocumentToolOperation::ReadMetadataWindow { .. }
                )
            {
                let bytes = b"{\"ok\":false,\"error\":\"parse_toolbox_unknown_artifact\",\"guidance\":\"inspect_artifacts; use issued IDs\"}\n";
                return Ok(DocumentToolReply {
                    text: String::from_utf8(bytes.to_vec())?,
                    image_url: None,
                    source_frame_sha256: sha(bytes),
                });
            }
            if matches!(operation, DocumentToolOperation::Finalize {}) {
                self.finalized = true;
            }
            DocumentToolReply::from_core_frame(
                b"{\"ok\":true,\"data\":{\"kind\":\"synthetic\"}}\n".to_vec(),
                4096,
            )
        })
    }
    fn verify_complete(&self) -> Result<DocumentVerifiedProduct> {
        ensure!(self.finalized, "synthetic not finalized");
        Ok(DocumentVerifiedProduct {
            handle: self.prepared.handle.clone(),
            source_sha256: self.prepared.source_sha256.clone(),
            prepared_sha256: self.prepared.prepared_sha256.clone(),
            original_pages: 137,
            delivered_pages: 0,
            native_metadata_artifacts: 138,
            native_metadata_complete: 0,
            registered_ocr_tasks: 0,
            inspected_ocr_tasks: 0,
            required_ocr_complete: true,
            parser_complete_original_pages: true,
            all_source_artifacts_verified: true,
            model_inspection_complete: false,
        })
    }
}
fn limits() -> DocumentAgentLimits {
    DocumentAgentLimits {
        timeout_ms: 900_000,
        max_tool_calls: 64,
        max_messages: 100,
        incoming_frame_bytes: 65_536,
        outgoing_frame_bytes: 4_194_304,
        tool_argument_bytes: 4096,
        tool_reply_bytes: 262_144,
        total_rpc_bytes: 33_554_432,
        working_bytes: 67_108_864,
        tracking_bytes: 8_388_608,
    }
}
fn control() -> Result<DocumentAgentControl> {
    let clock = serde_json::from_value(
        json!({"schema":"gptgrep.run-clock-anchor.v1","deadline_unix_ms":999999,
        "clock_kind":"synthetic","boot_identity":"synthetic","clock_namespace":null,"monotonic_started_ns":1,"monotonic_deadline_ns":2}),
    )?;
    let file = DocumentFilePin {
        path: "/synthetic/never-read".into(),
        bytes: 1,
        sha256: "f".repeat(64),
    };
    Ok(DocumentAgentControl {
        schema: "gptgrep.parse-agent.document-control.v1".into(),
        invocation_id: "synthetic".into(),
        codex_bin: file.clone(),
        runtime_profile: file.clone(),
        model_descriptor: file.clone(),
        permissions_config: file,
        auth_home: "/synthetic/auth".into(),
        state_dir: "/synthetic/state".into(),
        cwd: "/synthetic/cwd".into(),
        environment_id: "local".into(),
        auth_mode: "chatgpt".into(),
        requested_model: DOCUMENT_MODEL.into(),
        reasoning_effort: "max".into(),
        requested_service_tier: "fast".into(),
        original_clock: clock,
        deadline_unix_ms: 999999,
        limits: limits(),
        native_viewer: None,
        document_image_qualification: None,
    })
}
fn line(value: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).expect("synthetic frame");
    bytes.push(b'\n');
    bytes
}
fn startup(auth: &str) -> Vec<u8> {
    startup_with_thread(auth, thread_response())
}
fn thread_response() -> Value {
    json!({"thread":{"id":"synthetic-thread"},"model":DOCUMENT_MODEL,"modelProvider":"openai",
        "approvalPolicy":"never","activePermissionProfile":{"id":PERMISSIONS,"extends":null},
        "sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"priority"})
}
fn startup_with_thread(auth: &str, response: Value) -> Vec<u8> {
    let mut script = Vec::new();
    for value in [
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":{"account":{"type":auth}}}),
        json!({"id":3,"result":{"config":{"mcp_servers":{},"tools":{}}}}),
        json!({"id":4,"result":response}),
        json!({"id":5,"result":{"turn":{"id":"synthetic-turn"}}}),
    ] {
        script.extend(line(value));
    }
    script
}
fn tool(call: &str, op: &str) -> Value {
    json!({"id":101,"method":"item/tool/call","params":{"threadId":"synthetic-thread",
    "turnId":"synthetic-turn","callId":call,"namespace":NAMESPACE,"tool":TOOL,"arguments":{"op":op}}})
}
fn finish() -> Vec<u8> {
    let mut script = Vec::new();
    script.extend(line(json!({"method":"item/completed","params":{"threadId":"synthetic-thread",
    "turnId":"synthetic-turn","item":{"type":"agentMessage","id":"synthetic-answer","phase":"final_answer",
    "text":json!({"status":"complete","handle":"a".repeat(64)}).to_string()}}})));
    script.extend(line(
        json!({"method":"turn/completed","params":{"threadId":"synthetic-thread",
        "turn":{"id":"synthetic-turn","status":"completed","error":null,"items":[]}}}),
    ));
    script
}
async fn replay(script: &[u8], backend: &mut Backend) -> Result<DocumentAgentOutcome> {
    replay_captured(script, backend, Arc::new(Mutex::new(Vec::new()))).await
}
async fn replay_captured(
    script: &[u8],
    backend: &mut Backend,
    captured: Arc<Mutex<Vec<u8>>>,
) -> Result<DocumentAgentOutcome> {
    let config = control()?;
    let budget = TransportBudget::new(config.limits.total_rpc_bytes);
    let writer = Captured(captured);
    run_protocol(
        script,
        writer,
        &config,
        backend,
        DocumentRunBudget {
            working: DocumentWorking::new(config.limits.working_bytes),
            transport: budget,
            deadline: Instant::now() + Duration::from_secs(30),
        },
        None,
        None,
    )
    .await
}

fn named_workspace_projection() -> Value {
    json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
        "excludeTmpdirEnvVar":true,"excludeSlashTmp":true})
}
fn sent_frames(captured: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
    std::str::from_utf8(&captured.lock().unwrap())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn current_named_acknowledgments_and_typed_schemas_reach_one_document_turn() -> Result<()> {
    for sandbox in [
        json!({"type":"readOnly","networkAccess":false}),
        named_workspace_projection(),
    ] {
        let mut response = thread_response();
        response["sandbox"] = sandbox;
        assert!(response.get("permissions").is_none());
        let mut script = startup_with_thread("chatgpt", response);
        script.extend(line(tool("finalize-one", "finalize")));
        script.extend(finish());
        let mut backend = Backend::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        replay_captured(&script, &mut backend, captured.clone()).await?;
        assert_eq!(backend.calls, 1);
        let sent = sent_frames(&captured);
        for method in ["thread/start", "turn/start"] {
            let messages = sent
                .iter()
                .filter(|value| value["method"] == method)
                .collect::<Vec<_>>();
            assert_eq!(messages.len(), 1);
            let params = &messages[0]["params"];
            assert_eq!(params["permissions"], PERMISSIONS);
            assert_eq!(params["approvalPolicy"], "never");
            assert!(params.get("sandbox").is_none());
            assert!(params.get("sandboxPolicy").is_none());
        }
        let thread = sent
            .iter()
            .find(|value| value["method"] == "thread/start")
            .unwrap();
        let branches = thread["params"]["dynamicTools"][0]["tools"][0]["inputSchema"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(branches.len(), 10);
        let mut operations = BTreeSet::new();
        for branch in branches {
            assert_eq!(branch["type"], "object");
            assert_eq!(branch["additionalProperties"], false);
            let properties = &branch["properties"];
            assert_eq!(properties["op"]["type"], "string");
            operations.insert(properties["op"]["const"].as_str().unwrap());
            if properties.get("page_size").is_some() {
                assert_eq!(properties["page_size"]["minimum"], 1);
                assert_eq!(properties["page_size"]["maximum"], 64);
            }
            if properties.get("max_bytes").is_some() {
                assert_eq!(properties["max_bytes"]["maximum"], 32768);
            }
            if properties.get("section").is_some() {
                assert_eq!(properties["section"]["type"], "string");
                assert_eq!(
                    properties["section"]["enum"],
                    json!(["tasks", "pages", "blockers"])
                );
            }
            if properties.get("bbox").is_some() {
                assert_eq!(properties["bbox"]["additionalProperties"], false);
                for coordinate in ["x", "y", "width", "height"] {
                    assert_eq!(
                        properties["bbox"]["properties"][coordinate]["type"],
                        "number"
                    );
                }
            }
        }
        assert_eq!(
            operations,
            [
                "inspect_document",
                "finalize",
                "inspect_pages",
                "inspect_artifacts",
                "read_metadata_window",
                "read_artifact",
                "inspect_images",
                "inspect_ocr_plan",
                "execute_ocr",
                "crop"
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
        let turn = sent
            .iter()
            .find(|value| value["method"] == "turn/start")
            .unwrap();
        assert_eq!(
            turn["params"]["outputSchema"]["properties"]["status"],
            json!({"type":"string","enum":["complete","blocked"]})
        );
    }
    Ok(())
}

#[tokio::test]
async fn incompatible_document_profile_or_projection_refuses_before_turn() -> Result<()> {
    let mut cases = vec![
        (
            "activePermissionProfile",
            None,
            "document_permission_profile",
        ),
        (
            "activePermissionProfile",
            Some(Value::Null),
            "document_permission_profile",
        ),
        (
            "activePermissionProfile",
            Some(json!({})),
            "document_permission_profile",
        ),
        (
            "activePermissionProfile",
            Some(json!({"id":"different"})),
            "document_permission_profile",
        ),
        (
            "activePermissionProfile",
            Some(json!({"id":17})),
            "document_permission_profile",
        ),
        ("sandbox", None, "document_effective_permissions"),
        (
            "sandbox",
            Some(json!({"type":"dangerFullAccess","networkAccess":false})),
            "document_effective_permissions",
        ),
        (
            "sandbox",
            Some(json!({"type":"externalSandbox","networkAccess":false})),
            "document_effective_permissions",
        ),
        (
            "sandbox",
            Some(json!({"type":"readOnly","networkAccess":true})),
            "document_effective_permissions",
        ),
        (
            "sandbox",
            Some(json!({"type":"readOnly"})),
            "document_effective_permissions",
        ),
        (
            "approvalPolicy",
            Some(json!("on-request")),
            "document_thread_profile",
        ),
        ("model", Some(json!("different")), "document_thread_profile"),
        (
            "modelProvider",
            Some(json!("different")),
            "document_thread_profile",
        ),
        ("reasoningEffort", Some(json!("low")), "document_effort"),
        ("serviceTier", Some(json!("flex")), "document_tier"),
    ];
    for (field, value) in [
        ("writableRoots", None),
        ("writableRoots", Some(Value::Null)),
        ("writableRoots", Some(json!(["/corpus"]))),
        ("excludeTmpdirEnvVar", None),
        ("excludeTmpdirEnvVar", Some(json!(false))),
        ("excludeSlashTmp", None),
        ("excludeSlashTmp", Some(json!(false))),
        ("networkAccess", None),
        ("networkAccess", Some(json!(true))),
    ] {
        let mut sandbox = named_workspace_projection();
        if let Some(value) = value {
            sandbox[field] = value;
        } else {
            sandbox.as_object_mut().unwrap().remove(field);
        }
        cases.push(("sandbox", Some(sandbox), "document_effective_permissions"));
    }
    for (field, value, code) in cases {
        let mut response = thread_response();
        // The obsolete managed permissions object cannot substitute for a
        // missing named acknowledgment or a rejected compatibility envelope.
        response["permissions"] = native::permission_value(None);
        if let Some(value) = value {
            response[field] = value;
        } else {
            response.as_object_mut().unwrap().remove(field);
        }
        let mut script = startup_with_thread("chatgpt", response);
        script.extend(line(tool("never-dispatch", "finalize")));
        script.extend(finish());
        let mut backend = Backend::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let error = replay_captured(&script, &mut backend, captured.clone())
            .await
            .err()
            .expect("invalid acknowledgment");
        assert_eq!(error.to_string(), code);
        assert_eq!(backend.calls, 0);
        assert!(
            sent_frames(&captured)
                .iter()
                .all(|value| value["method"] != "turn/start")
        );
    }
    Ok(())
}

#[tokio::test]
async fn typed_finalization_can_cover_all_pages_with_partial_model_inspection() -> Result<()> {
    let mut script = startup("chatgpt");
    // Native app-server may publish item/started BEFORE forwarding tool/call.
    script.extend(line(json!({"method":"item/started","params":{"threadId":"synthetic-thread","turnId":"synthetic-turn",
        "item":{"type":"dynamicToolCall","id":"call-one","namespace":NAMESPACE,"tool":TOOL}}})));
    script.extend(line(tool("call-one", "finalize")));
    script.extend(finish());
    let mut backend = Backend::new();
    let result = replay(&script, &mut backend).await?;
    assert_eq!(result.verified_product.original_pages, 137);
    assert_eq!(result.verified_product.delivered_pages, 0);
    assert!(result.verified_product.parser_complete_original_pages);
    assert!(!result.verified_product.model_inspection_complete);
    assert_eq!(result.tool_submissions, 1);
    assert_eq!(backend.calls, 1);
    assert_eq!(result.deliveries.len(), 1);
    assert!(!result.actual_served_profile_verified);
    assert!(!result.provider_ingestion_verified);
    Ok(())
}
#[tokio::test]
async fn agent_prose_cannot_replace_typed_product_finalization() -> Result<()> {
    let mut script = startup("chatgpt");
    script.extend(finish());
    let mut backend = Backend::new();
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 0);
    Ok(())
}
#[tokio::test]
async fn wrong_auth_and_typed_backend_refusal_do_not_fall_back_or_retry() -> Result<()> {
    let mut script = startup("apiKey");
    script.extend(line(tool("one", "finalize")));
    let mut backend = Backend::new();
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 0);
    let mut script = startup("chatgpt");
    script.extend(line(tool("one", "finalize")));
    script.extend(line(tool("two", "finalize")));
    backend.refuse = true;
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 1);
    Ok(())
}

fn refused_selector_tool(call: &str) -> Value {
    let mut request = tool(call, "read_metadata_window");
    request["params"]["arguments"] = json!({"op":"read_metadata_window",
        "artifact_id":format!("artifact-{}","b".repeat(64)),"cursor":null,"max_bytes":8192});
    request
}

#[tokio::test]
async fn application_refusal_is_delivered_then_a_different_call_can_complete() -> Result<()> {
    let mut script = startup("chatgpt");
    script.extend(line(refused_selector_tool("selector-one")));
    script.extend(line(tool("inspect-two", "inspect_document")));
    script.extend(line(tool("finalize-three", "finalize")));
    script.extend(finish());
    let mut backend = Backend::new();
    backend.application_refusal = true;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let outcome = replay_captured(&script, &mut backend, captured.clone()).await?;
    assert_eq!(outcome.tool_submissions, 3);
    assert_eq!(backend.calls, 3);
    assert_eq!(outcome.deliveries.len(), 3);
    let sent = sent_frames(&captured);
    assert_eq!(
        sent.iter()
            .filter(|value| value["method"] == "turn/start")
            .count(),
        1
    );
    let replies: Vec<_> = sent.iter().filter(|value| value["id"] == 101).collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["result"]["success"], true); // Delivery, not selector acceptance.
    let refused: Value = serde_json::from_str(
        replies[0]["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )?;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"], "parse_toolbox_unknown_artifact");
    assert!(refused.get("data").is_none());
    assert!(!outcome.deliveries[0].mediated_image);
    // The generic core-frame converter still rejects backend refusal frames.
    assert!(DocumentToolReply::from_core_frame(serde_json::to_vec(&refused)?, 4096).is_err());
    Ok(())
}

#[tokio::test]
async fn application_refusal_does_not_admit_duplicate_calls_or_fatal_backend_errors() -> Result<()>
{
    let mut script = startup("chatgpt");
    script.extend(line(refused_selector_tool("same")));
    script.extend(line(tool("same", "finalize")));
    let mut backend = Backend::new();
    backend.application_refusal = true;
    let error = replay(&script, &mut backend)
        .await
        .err()
        .expect("duplicate refusal");
    assert_eq!(error.to_string(), "document_duplicate_tool_submission");
    assert_eq!(backend.calls, 1);
    assert!(!backend.finalized);
    let mut script = startup("chatgpt");
    script.extend(line(refused_selector_tool("one")));
    script.extend(line(tool("two", "finalize")));
    let mut backend = Backend::new();
    backend.application_refusal = true;
    backend.refuse = true;
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 1);
    assert!(!backend.finalized);
    Ok(())
}

#[tokio::test]
async fn application_refusals_consume_the_original_64_call_envelope() -> Result<()> {
    for refused in [63, 64] {
        let mut script = startup("chatgpt");
        for number in 0..refused {
            script.extend(line(refused_selector_tool(&format!("refused-{number}"))));
        }
        script.extend(line(tool("finalize", "finalize")));
        script.extend(finish());
        let mut backend = Backend::new();
        backend.application_refusal = true;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let result = replay_captured(&script, &mut backend, captured.clone()).await;
        if refused == 63 {
            assert_eq!(result?.tool_submissions, 64);
            assert!(backend.finalized);
        } else {
            assert_eq!(
                result.err().expect("64-call cap").to_string(),
                "document_tool_count"
            );
            assert!(!backend.finalized);
        }
        assert_eq!(backend.calls, 64);
        assert_eq!(
            sent_frames(&captured)
                .iter()
                .filter(|value| value["method"] == "turn/start")
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn backend_refusal_reports_only_opcode_and_bounded_error_fingerprint() -> Result<()> {
    let mut script = startup("chatgpt");
    script.extend(line(tool("one", "inspect_document")));
    script.extend(line(tool("must-not-dispatch", "finalize")));
    let mut backend = Backend::new();
    backend.refuse = true;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let error = replay_captured(&script, &mut backend, captured.clone())
        .await
        .err()
        .expect("backend refusal");
    let underlying = format!(
        "parse_agent_operation_failed: MOCK-private-backend-context: {PRIVATE_BACKEND_ERROR}"
    );
    let expected = format!(
        "document_backend_refused_or_timeout kind=backend_error operation=inspect_document error_bytes_hashed={} error_sha256={} error_truncated=false",
        underlying.len(),
        sha(underlying.as_bytes())
    );
    assert_eq!(error.to_string(), expected);
    assert!(!format!("{error:?}").contains("MOCK-private"));
    assert!(!format!("{error:#}").contains("/synthetic/private"));
    assert_eq!(error.chain().count(), 1);
    assert_eq!(backend.calls, 1);
    let outgoing = String::from_utf8(captured.lock().unwrap().clone())?;
    assert!(!outgoing.contains(PRIVATE_BACKEND_ERROR));
    assert!(!outgoing.contains("MOCK-private"));
    assert!(!outgoing.contains("/synthetic/private"));
    let sent = sent_frames(&captured);
    assert_eq!(sent.last().unwrap()["id"], 101);
    assert_eq!(sent.last().unwrap()["error"]["code"], -32601);

    // The cap is in bytes, including when it falls inside a UTF-8 character.
    let oversized = format!("{PRIVATE_BACKEND_ERROR}x{}", "中".repeat(4096));
    let fingerprint = backend_failure("read_artifact", Some(&anyhow!(oversized.clone())), None);
    assert_eq!(
        fingerprint.to_string(),
        format!(
            "document_backend_refused_or_timeout kind=backend_error operation=read_artifact error_bytes_hashed=4096 error_sha256={} error_truncated=true",
            sha(&oversized.as_bytes()[..4096])
        )
    );
    assert!(!format!("{fingerprint:?}").contains("MOCK-private"));
    assert_eq!(
        backend_failure("read_artifact", None, None).to_string(),
        format!(
            "document_backend_refused_or_timeout kind=timeout operation=read_artifact error_bytes_hashed=0 error_sha256={} error_truncated=false",
            sha(b"")
        )
    );
    Ok(())
}

#[tokio::test]
async fn read_backend_refusal_reports_numeric_shape_without_artifact_or_cursor_identity()
-> Result<()> {
    let artifact_id = format!("artifact-{}", "f".repeat(64));
    let cursor_id = "e".repeat(64);
    for op in ["read_artifact", "read_metadata_window"] {
        for (max_bytes, cursor_present) in [(0, false), (32_769, true), (usize::MAX, false)] {
            let mut request = tool("private-call-not-retained", op);
            request["params"]["arguments"] = json!({"op":op,"artifact_id":artifact_id,
                "cursor":if cursor_present {Some(&cursor_id)} else {None},"max_bytes":max_bytes});
            let mut script = startup("chatgpt");
            script.extend(line(request));
            script.extend(line(tool("must-not-dispatch", "finalize")));
            let captured = Arc::new(Mutex::new(Vec::new()));
            let mut backend = Backend::new();
            backend.refuse = true;
            let error = replay_captured(&script, &mut backend, captured.clone())
                .await
                .err()
                .expect("backend refusal");
            let diagnostic = error.to_string();
            assert!(diagnostic.contains(&format!("operation={op} ")));
            assert!(diagnostic.ends_with(&format!(
                " max_bytes={max_bytes} cursor_present={cursor_present}"
            )));
            assert_eq!(backend.calls, 1);
            let outgoing = String::from_utf8(captured.lock().unwrap().clone())?;
            for value in [&diagnostic, &format!("{error:?}"), &outgoing] {
                assert!(!value.contains(&artifact_id));
                assert!(!value.contains(&cursor_id));
                assert!(!value.contains("private-call-not-retained"));
                assert!(!value.contains("MOCK-private"));
                assert!(!value.contains("/synthetic/private"));
            }
            let sent = sent_frames(&captured);
            assert_eq!(sent.last().unwrap()["id"], 101);
            assert_eq!(sent.last().unwrap()["error"]["code"], -32601);
        }
    }
    Ok(())
}

#[tokio::test]
async fn startup_tool_events_and_auth_mode_changes_are_refused_before_backend_dispatch()
-> Result<()> {
    let mut script = line(
        json!({"method":"item/started","params":{"item":{"type":"commandExecution","id":"unadmitted"}}}),
    );
    script.extend(startup("chatgpt"));
    let mut backend = Backend::new();
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 0);
    let mut script = startup("chatgpt");
    script.extend(line(
        json!({"method":"account/updated","params":{"authMode":"apiKey"}}),
    ));
    script.extend(line(tool("one", "finalize")));
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 0);
    Ok(())
}

fn remote_control_notice(environment: Option<Value>) -> Value {
    let mut notice = json!({"method":"remoteControl/status/changed","params":{
        "installationId":"MOCK-private-installation","serverName":"MOCK-private-server",
        "status":"disabled"}});
    if let Some(environment) = environment {
        notice["params"]["environmentId"] = environment;
    }
    notice
}

#[tokio::test]
async fn disabled_remote_control_metadata_is_discarded_during_startup_and_active_turn() -> Result<()>
{
    for environment in [
        None,
        Some(Value::Null),
        Some(json!("MOCK-private-environment")),
    ] {
        let notice = remote_control_notice(environment);
        let mut script = Vec::new();
        // Interleave all startup RPC waits, including the pending turn reply.
        for frame in startup("chatgpt").split_inclusive(|byte| *byte == b'\n') {
            script.extend(line(notice.clone()));
            script.extend_from_slice(frame);
        }
        script.extend(line(notice));
        script.extend(line(tool("finalize-one", "finalize")));
        script.extend(finish());
        let mut backend = Backend::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let outcome = replay_captured(&script, &mut backend, captured.clone()).await?;
        assert_eq!(backend.calls, 1);
        assert_eq!(outcome.tool_submissions, 1);
        let outgoing = String::from_utf8(captured.lock().unwrap().clone())?;
        let outcome_json = serde_json::to_string(&outcome)?;
        for value in [&outgoing, &outcome_json] {
            assert!(!value.contains("MOCK-private-"));
            assert!(!value.contains("remoteControl/status/changed"));
            assert!(!value.contains("installationId"));
            assert!(!value.contains("serverName"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_remote_control_metadata_and_requests_stop_before_backend_dispatch() -> Result<()>
{
    let valid = remote_control_notice(Some(Value::Null));
    let mut refused = Vec::new();
    for key in ["installationId", "serverName", "status"] {
        let mut message = valid.clone();
        message["params"].as_object_mut().unwrap().remove(key);
        refused.push(message);
    }
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("params");
    refused.push(missing);
    for params in [Value::Null, json!(false), json!([]), json!("MOCK-private")] {
        let mut message = valid.clone();
        message["params"] = params;
        refused.push(message);
    }
    for key in ["installationId", "serverName"] {
        for value in [Value::Null, json!(false), json!(7), json!([]), json!({})] {
            let mut message = valid.clone();
            message["params"][key] = value;
            refused.push(message);
        }
    }
    for value in [json!(false), json!(7), json!([]), json!({})] {
        let mut message = valid.clone();
        message["params"]["environmentId"] = value;
        refused.push(message);
    }
    for status in [
        Value::Null,
        json!(false),
        json!(7),
        json!([]),
        json!({}),
        json!("enabled"),
        json!("connected"),
        json!("connecting"),
        json!("errored"),
        json!("MOCK-unknown"),
    ] {
        let mut message = valid.clone();
        message["params"]["status"] = status;
        refused.push(message);
    }
    let mut unknown_field = valid.clone();
    unknown_field["params"]["extra"] = json!("MOCK-private");
    refused.push(unknown_field);
    for id in [json!(101), json!("request"), Value::Null] {
        let mut message = valid.clone();
        message["id"] = id;
        assert!(automatic(&message).is_err());
        refused.push(message);
    }
    let mut unknown_method = valid;
    unknown_method["method"] = json!("remoteControl/status/unknown");
    refused.push(unknown_method);

    for (case, message) in refused.into_iter().enumerate() {
        for active in [false, true] {
            let mut script = if active {
                startup("chatgpt")
            } else {
                Vec::new()
            };
            script.extend(line(message.clone()));
            if !active {
                script.extend(startup("chatgpt"));
            }
            script.extend(line(tool("must-not-dispatch", "finalize")));
            script.extend(finish());
            let mut backend = Backend::new();
            let captured = Arc::new(Mutex::new(Vec::new()));
            assert!(
                replay_captured(&script, &mut backend, captured.clone())
                    .await
                    .is_err(),
                "case={case} active={active}"
            );
            assert_eq!(backend.calls, 0, "case={case} active={active}");
            assert_eq!(
                sent_frames(&captured)
                    .iter()
                    .filter(|value| value["method"] == "turn/start")
                    .count(),
                usize::from(active),
                "case={case} active={active}"
            );
        }
    }
    Ok(())
}

#[test]
fn malformed_and_duplicate_submissions_consume_the_same_finite_counter() -> Result<()> {
    let working = DocumentWorking::new(16384);
    let mut calls = Calls {
        submissions: 0,
        ids: BTreeSet::new(),
        events: BTreeSet::new(),
        retained: working.reserve(0)?,
    };
    assert!(calls.submit("", 3).is_err());
    assert_eq!(calls.submissions, 1);
    assert_eq!(calls.submit("same", 3)?, 2);
    assert!(calls.submit("same", 3).is_err());
    assert_eq!(calls.submissions, 3);
    assert!(calls.submit("next", 3).is_err());
    assert_eq!(calls.submissions, 4);
    assert_eq!(calls.ids.len(), 1);
    Ok(())
}
#[tokio::test]
async fn duplicate_tool_call_never_reinvokes_the_typed_backend() -> Result<()> {
    let mut script = startup("chatgpt");
    script.extend(line(tool("same", "inspect_document")));
    script.extend(line(tool("same", "finalize")));
    let mut backend = Backend::new();
    assert!(replay(&script, &mut backend).await.is_err());
    assert_eq!(backend.calls, 1);
    Ok(())
}
#[test]
fn closed_tool_json_cannot_supply_paths_budgets_tasks_or_paid_results() {
    for value in [
        json!({"op":"execute_ocr","task_sha256":"a".repeat(64),"result":{}}),
        json!({"op":"inspect_document","source":"/private/source"}),
        json!({"op":"finalize","resources":{}}),
        json!({"op":"prepare","path":"/private/source"}),
    ] {
        assert!(serde_json::from_value::<DocumentToolOperation>(value).is_err());
    }
    for value in [json!({"op":"inspect_document"}), json!({"op":"finalize"})] {
        let parsed = serde_json::from_value::<DocumentToolOperation>(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
}
#[test]
fn complete_mediated_images_and_escaped_frames_are_distinct_from_native_receipts() -> Result<()> {
    let core = line(
        json!({"ok":true,"data":{"kind":"complete_image","data_url":"data:image/png;base64,YWJj",
        "artifact":{"id":"artifact-synthetic","sha256":"synthetic"},"extra":"quoted\\\"\u{0}中"}}),
    );
    let reply = DocumentToolReply::from_core_frame(core.clone(), 4096)?;
    assert_eq!(reply.source_frame_sha256, sha(&core));
    assert_eq!(
        reply.image_url.as_deref(),
        Some("data:image/png;base64,YWJj")
    );
    let text: Value = serde_json::from_str(&reply.text)?;
    assert!(text["data"].get("data_url").is_none());
    assert_eq!(text["data"]["extra"], "quoted\\\"\u{0}中");
    let outer = json!({"id":"quoted\\\"\u{0}中","result":{"success":true,"contentItems":[{"type":"inputText","text":reply.text}]}});
    let bytes = crate::transport::encode_frame(&outer, 4096)?;
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert!(crate::transport::encode_frame(&outer, bytes.len() - 1).is_err());
    Ok(())
}
#[test]
fn named_permissions_expose_only_the_private_image_directory() -> Result<()> {
    let native = DocumentNativeControl {
        image_root: "/synthetic/images".into(),
        receipt_root: "/synthetic/receipts".into(),
        admission_path: "/synthetic/admission.json".into(),
        image_catalog_path: "/synthetic/catalog.ndjson".into(),
        owner_admission_sha256: "a".repeat(64),
        image_wire_bytes: 33_554_432,
    };
    let policy = document_permissions_config(Some(&native));
    let text = policy.to_string();
    assert!(text.contains("/synthetic/images"));
    assert!(!text.contains("/synthetic/receipts"));
    assert!(!text.contains("/synthetic/catalog.ndjson"));
    assert!(!text.contains("/synthetic/admission.json"));
    assert_eq!(
        policy["permissions.gptgrep_native_document.network.enabled"],
        false
    );
    assert_eq!(
        policy["permissions.gptgrep_native_document.filesystem"][":root"],
        "deny"
    );
    Ok(())
}
#[test]
fn native_events_are_refused_without_the_separate_document_guard() -> Result<()> {
    let working = DocumentWorking::new(16384);
    let mut calls = Calls {
        submissions: 0,
        ids: BTreeSet::new(),
        events: BTreeSet::new(),
        retained: working.reserve(0)?,
    };
    assert!(
        check_item(
            &json!({"type":"imageView","id":"one","path":"/synthetic/images/one.png"}),
            &mut calls,
            None,
            64
        )
        .is_err()
    );
    assert!(
        check_item(
            &json!({"type":"commandExecution","id":"shell"}),
            &mut calls,
            None,
            64
        )
        .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn incoming_overflow_and_total_wire_failure_precede_tree_or_backend_allocation() -> Result<()>
{
    let mut limits = limits();
    limits.incoming_frame_bytes = 1024;
    let bytes = vec![b'a'; 1025];
    let transport = TransportBudget::new(8192);
    let mut rpc = Rpc {
        reader: BufReader::new(BudgetedReader::new(bytes.as_slice(), transport.clone())),
        writer: Vec::new(),
        limits: limits.clone(),
        working: DocumentWorking::new(67_108_864),
        transport,
        deadline: Instant::now() + Duration::from_secs(1),
        messages: 0,
    };
    assert!(rpc.receive().await.is_err());
    assert_eq!(rpc.messages, 0);
    let transport = TransportBudget::new(4);
    let mut rpc = Rpc {
        reader: BufReader::new(&b""[..]),
        writer: Vec::new(),
        limits,
        working: DocumentWorking::new(67_108_864),
        transport,
        deadline: Instant::now() + Duration::from_secs(1),
        messages: 0,
    };
    assert!(rpc.send(json!({"id":1})).await.is_err());
    assert!(rpc.writer.is_empty());
    Ok(())
}
#[cfg(unix)]
#[test]
fn legacy_document_ready_cannot_authorize_a_turn_or_weaken_the_manifest_abi() -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    for leaf in ["images", "receipts"] {
        fs::DirBuilder::new().mode(0o700).create(root.join(leaf))?;
    }
    let payload = b"synthetic source bytes; no runtime image ingestion";
    let source = root.join("images/image.png");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    options.open(&source)?.write_all(payload)?;
    let mut backend = Backend::new();
    backend.prepared.original_rendition_artifacts = 1;
    backend.images.push(DocumentImageGrant {
        artifact: ArtifactRef {
            id: format!("artifact-{}", "1".repeat(64)),
            kind: gptgrep_parse::ArtifactKind::Rendition,
            relative_path: "image.png".into(),
            bytes: payload.len() as u64,
            sha256: sha(payload),
            profile_sha256: "2".repeat(64),
            mime: "image/png".into(),
            page: Some(1),
            region_id: None,
        },
        copied_path: source,
        source_sha256: backend.prepared.source_sha256.clone(),
        prepared_sha256: backend.prepared.prepared_sha256.clone(),
        width: 2,
        height: 2,
    });
    let mut config = control()?;
    config.deadline_unix_ms = u64::MAX;
    let native = DocumentNativeControl {
        image_root: root.join("images"),
        receipt_root: root.join("receipts"),
        admission_path: root.join("admission.json"),
        image_catalog_path: root.join("catalog.ndjson"),
        owner_admission_sha256: "3".repeat(64),
        image_wire_bytes: 33_554_432,
    };
    let mut session = native::NativeSession::prepare(
        &native,
        &config,
        &backend,
        &DocumentWorking::new(67_108_864),
        Instant::now() + Duration::from_secs(30),
    )?;
    let manifest_bytes = fs::read(&native.admission_path)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
    assert_eq!(manifest.as_object().expect("manifest object").len(), 15);
    assert!(manifest.get("max_seconds").is_none());
    let ready = json!({"schema_version":"gptgrep.native-document.ready.v1","required":true,
        "admission_sha256":sha(&manifest_bytes),"task_sha256":backend.prepared.task_sha256,
        "source_sha256":backend.prepared.source_sha256,"prepared_sha256":backend.prepared.prepared_sha256,
        "runtime_profile_sha256":config.runtime_profile.sha256,"permissions_profile_id":"gptgrep_native_document",
        "permissions_profile_sha256":manifest["permissions_profile_sha256"],"environment_ids":["local"],
        "environment_id_argument_admitted":false,"max_tool_calls":64,"max_turns":1,"max_seconds":900,
        "deadline_unix_ms":manifest["deadline_unix_ms"],"frame_bytes":4_194_304,"image_transport_reserved_bytes":33_554_432,
        "tool_counter_before_lookup_hooks_handler":true,"tool_counter_before_argument_parse":true,
        "normalized_request_profile_verified_before_send":true,"native_image_read_bounded":true,
        "assembled_image_receipts_required":true,"requested_model":"gpt-6-luna","requested_effort":"max",
        "normalized_request_service_tier":"priority","actual_served_profile_verified":false,
        "image_catalog_sha256":manifest["image_catalog"]["sha256"],"image_catalog_count":1});
    for field in [
        "tool_counter_before_argument_parse",
        "normalized_request_profile_verified_before_send",
    ] {
        let mut older = ready.clone();
        older.as_object_mut().expect("ready object").remove(field);
        assert!(
            session
                .initialize(&json!({"gptgrepNativeDocument":older}))
                .is_err()
        );
    }
    assert!(
        session
            .initialize(&json!({"gptgrepNativeOcr":ready.clone()}))
            .is_err()
    );
    session.initialize(&json!({"gptgrepNativeDocument":ready}))?;
    Ok(())
}

#[cfg(unix)]
struct RetainedNativeBackend {
    inner: Backend,
    resources: gptgrep_parse::ResourceBudget,
    requests: Mutex<Vec<u64>>,
}
#[cfg(unix)]
impl DocumentBackend for RetainedNativeBackend {
    fn prepared(&self) -> &DocumentPreparedState {
        self.inner.prepared()
    }
    fn images(&self) -> &[DocumentImageGrant] {
        self.inner.images()
    }
    fn reserve_retained(&self, bytes: u64) -> Result<()> {
        self.requests
            .lock()
            .expect("synthetic retained requests")
            .push(bytes);
        self.resources.reserve_retained_staging(bytes)
    }
    fn call<'a>(
        &'a mut self,
        operation: DocumentToolOperation,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentToolReply>> + Send + 'a>> {
        self.inner.call(operation)
    }
    fn verify_complete(&self) -> Result<DocumentVerifiedProduct> {
        self.inner.verify_complete()
    }
}

#[cfg(unix)]
fn retained_native_fixture() -> Result<(
    tempfile::TempDir,
    DocumentAgentControl,
    DocumentNativeControl,
    RetainedNativeBackend,
)> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    for leaf in ["images", "receipts"] {
        fs::DirBuilder::new().mode(0o700).create(root.join(leaf))?;
    }
    // The backend fixture owns this already-prepared source; no native/runtime
    // execution or provider ingestion is claimed by these accounting tests.
    let payload = b"synthetic retained image source";
    let source = root.join("images/image.png");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&source)?
        .write_all(payload)?;
    let mut inner = Backend::new();
    inner.prepared.original_rendition_artifacts = 1;
    inner.images.push(DocumentImageGrant {
        artifact: ArtifactRef {
            id: format!("artifact-{}", "1".repeat(64)),
            kind: gptgrep_parse::ArtifactKind::Rendition,
            relative_path: "image.png".into(),
            bytes: payload.len() as u64,
            sha256: sha(payload),
            profile_sha256: "2".repeat(64),
            mime: "image/png".into(),
            page: Some(1),
            region_id: None,
        },
        copied_path: source,
        source_sha256: inner.prepared.source_sha256.clone(),
        prepared_sha256: inner.prepared.prepared_sha256.clone(),
        width: 2,
        height: 2,
    });
    let mut config = control()?;
    config.deadline_unix_ms = u64::MAX;
    let native = DocumentNativeControl {
        image_root: root.join("images"),
        receipt_root: root.join("receipts"),
        admission_path: root.join("admission.json"),
        image_catalog_path: root.join("catalog.ndjson"),
        owner_admission_sha256: "3".repeat(64),
        image_wire_bytes: 33_554_432,
    };
    Ok((
        temporary,
        config,
        native,
        RetainedNativeBackend {
            inner,
            resources: gptgrep_parse::ResourceBudget::default(),
            requests: Mutex::new(Vec::new()),
        },
    ))
}

#[cfg(unix)]
#[test]
fn native_document_reserves_all_258_receipts_on_the_original_counter_before_metadata() -> Result<()>
{
    let (_temporary, config, native, backend) = retained_native_fixture()?;
    let original = backend.resources.clone();
    let receipt_bound = (2 + 64 + 3 * 64) * 16_384u64;
    assert_eq!(receipt_bound, 4_227_072);
    let session = native::NativeSession::prepare(
        &native,
        &config,
        &backend,
        &DocumentWorking::new(67_108_864),
        Instant::now() + Duration::from_secs(30),
    )?;
    assert_eq!(
        backend.requests.lock().expect("requests").first(),
        Some(&receipt_bound)
    );
    let metadata = fs::metadata(&native.image_catalog_path)?.len()
        + fs::metadata(&native.admission_path)?.len();
    assert_eq!(original.staging_used(), receipt_bound + metadata);
    let charged = original.staging_used();
    drop(session);
    assert_eq!(original.staging_used(), charged);
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_document_receipt_reservation_refuses_before_writes_and_keeps_later_failure_charge()
-> Result<()> {
    let (_temporary, config, native, backend) = retained_native_fixture()?;
    let original = backend.resources.clone();
    let receipt_bound = 4_227_072;
    original.reserve_retained_staging(original.run_staging_bytes - receipt_bound + 1)?;
    let before = original.staging_used();
    assert!(
        native::NativeSession::prepare(
            &native,
            &config,
            &backend,
            &DocumentWorking::new(67_108_864),
            Instant::now() + Duration::from_secs(30)
        )
        .is_err()
    );
    assert_eq!(original.staging_used(), before);
    assert_eq!(
        *backend.requests.lock().expect("requests"),
        vec![receipt_bound]
    );
    assert!(!native.image_catalog_path.exists() && !native.admission_path.exists());
    assert!(fs::read_dir(&native.receipt_root)?.next().is_none());

    // Exactly enough headroom for receipts still cannot fund the catalog. The
    // successful receipt reservation is retained when that next write refuses.
    let (_temporary, config, native, backend) = retained_native_fixture()?;
    let original = backend.resources.clone();
    original.reserve_retained_staging(original.run_staging_bytes - receipt_bound)?;
    assert!(
        native::NativeSession::prepare(
            &native,
            &config,
            &backend,
            &DocumentWorking::new(67_108_864),
            Instant::now() + Duration::from_secs(30)
        )
        .is_err()
    );
    assert_eq!(original.staging_used(), original.run_staging_bytes);
    assert!(!native.image_catalog_path.exists() && !native.admission_path.exists());
    assert!(fs::read_dir(&native.receipt_root)?.next().is_none());
    Ok(())
}

#[test]
fn native_document_qualification_rejects_page_scope_and_different_binary_bindings() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let mut config = control()?;
    config.native_viewer = Some(DocumentNativeControl {
        image_root: root.join("images"),
        receipt_root: root.join("receipts"),
        admission_path: root.join("admission"),
        image_catalog_path: root.join("catalog"),
        owner_admission_sha256: "a".repeat(64),
        image_wire_bytes: 33_554_432,
    });
    let profile = RuntimeProfile {
        schema: "gptgrep.parse-agent.document-runtime-profile.v1".into(),
        source_revision: "1".repeat(40),
        protocol_schema_sha256: "2".repeat(64),
        codex_binary_sha256: config.codex_bin.sha256.clone(),
        descriptor_sha256: config.model_descriptor.sha256.clone(),
        permissions_config_sha256: config.permissions_config.sha256.clone(),
        permissions_template_sha256: sha(&serde_json::to_vec(&native::canonical(
            document_permissions_template(true),
        ))?),
        tools_sha256: document_tool_policy_sha256()?,
        instructions_sha256: document_instructions_sha256(),
        standalone_app_server: true,
        max_seconds: None,
        native_document_guard: Some(NativeDocumentGuardProfile {
            schema: "gptgrep.native-document.guard-profile.v1".into(),
            source_sha256: "3".repeat(64),
            contract_sha256: document_guard_contract_sha256()?,
        }),
    };
    assert!(qualification::accepted(&config, &profile).is_err());
    let bindings = serde_json::to_value(qualification::expected(&config, &profile)?)?;
    let assertions = json!({"native_view_image_executed":true,"native_source_prepared_assembled_verified":true,
        "common_counter_before_argument_parse":true,"failed_duplicate_submissions_charged":true,
        "unknown_or_missing_guard_state_refused":true,"normalized_request_profile_verified_before_send":true,
        "complete_source_image_catalog_verified":true,"image_contrast_observed":true,"image_absence_refused":true,
        "metadata_only_not_accepted_as_image_delivery":true,"provider_ingestion_verified":false,"actual_served_profile_verified":false});
    fn store(root: &Path, name: &str, value: &Value) -> Result<DocumentFilePin> {
        let path = root.join(name);
        let bytes = serde_json::to_vec(value)?;
        fs::write(&path, &bytes)?;
        Ok(DocumentFilePin {
            path,
            bytes: bytes.len() as u64,
            sha256: sha(&bytes),
        })
    }
    for (schema, mutated) in [
        (
            "gptgrep.native-document.image-qualification.v1",
            bindings.clone(),
        ),
        ("gptgrep.ocr.image-qualification.v1", bindings.clone()),
        ("gptgrep.native-document.image-qualification.v1", {
            let mut b = bindings.clone();
            b["standalone_binary_sha256"] = json!("0".repeat(64));
            b
        }),
        ("gptgrep.native-document.image-qualification.v1", {
            let mut b = bindings.clone();
            b["max_tool_calls"] = json!(8);
            b
        }),
    ] {
        let record = store(
            &root,
            "record.json",
            &json!({"schema":schema,"accepted":true,"bindings":mutated,"assertions":assertions}),
        )?;
        let review = store(
            &root,
            "review.json",
            &json!({"schema":"gptgrep.native-document.image-qualification-independent-review.v1",
            "verdict":"ACCEPT_NATIVE_DOCUMENT_IMAGE_QUALIFICATION","accepted":true,"subjects":{"qualification_record":record},
            "bindings":bindings,"assertions":assertions,"findings":[]}),
        )?;
        let selected = store(
            &root,
            "control.json",
            &json!({"schema":"gptgrep.native-document.image-qualification-control.v1",
            "qualification_record":record,"independent_review":review}),
        )?;
        config.document_image_qualification = Some(DocumentImageQualification {
            path: selected.path,
            bytes: selected.bytes,
            sha256: selected.sha256,
        });
        assert_eq!(
            qualification::accepted(&config, &profile).is_ok(),
            schema == "gptgrep.native-document.image-qualification.v1" && mutated == bindings
        );
    }
    // Equal record bytes/hash at another path cannot satisfy the three-field subject join.
    let record = store(
        &root,
        "record.json",
        &json!({"schema":"gptgrep.native-document.image-qualification.v1",
        "accepted":true,"bindings":bindings,"assertions":assertions}),
    )?;
    let mut other = record.clone();
    other.path = root.join("different-record.json");
    fs::copy(&record.path, &other.path)?;
    let review = store(
        &root,
        "review.json",
        &json!({"schema":"gptgrep.native-document.image-qualification-independent-review.v1",
        "verdict":"ACCEPT_NATIVE_DOCUMENT_IMAGE_QUALIFICATION","accepted":true,"subjects":{"qualification_record":other},
        "bindings":bindings,"assertions":assertions,"findings":[]}),
    )?;
    let selected = store(
        &root,
        "control.json",
        &json!({"schema":"gptgrep.native-document.image-qualification-control.v1",
        "qualification_record":record,"independent_review":review}),
    )?;
    config.document_image_qualification = Some(DocumentImageQualification {
        path: selected.path,
        bytes: selected.bytes,
        sha256: selected.sha256,
    });
    assert!(qualification::accepted(&config, &profile).is_err());
    Ok(())
}
#[test]
fn qualification_image_observations_are_bounded_private_evidence_not_product_fields() -> Result<()>
{
    let value = json!({"status":"complete","handle":"a".repeat(64),"image_observations":[{
        "artifact_id":format!("artifact-{}","b".repeat(64)),"description":"synthetic red square; quoted \\\" 中"}]});
    let item = json!({"text":value.to_string()});
    assert!(final_signal(&item, false).is_err());
    let signal = final_signal(&item, true)?;
    assert_eq!(
        signal
            .image_observations
            .as_ref()
            .expect("observations")
            .len(),
        1
    );
    let mut oversized = value;
    oversized["image_observations"][0]["description"] = json!("x".repeat(513));
    assert!(final_signal(&json!({"text":oversized.to_string()}), true).is_err());
    assert!(
        final_signal(
            &json!({"text":json!({"status":"complete","handle":"a".repeat(64)}).to_string()}),
            true
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn emitted_document_qualification_requires_direct_native_view_without_broad_path_authority()
-> Result<()> {
    // Exercise emitted RPC instructions only. No native guard, provider or image
    // delivery is simulated or accepted; the stream ends just after turn/start.
    for qualification in [None, Some("synthetic-private-qualification")] {
        let config = control()?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut backend = Backend::new();
        // More catalog entries than the existing observation-output limit:
        // qualification may choose any admitted original, without truncation.
        for page in 1..=73 {
            let payload = format!("synthetic schema source image {page}");
            let digest = sha(payload.as_bytes());
            backend.images.push(DocumentImageGrant {
                artifact: ArtifactRef {
                    id: format!("artifact-{digest}"),
                    kind: gptgrep_parse::ArtifactKind::Rendition,
                    relative_path: format!("image-{page}.png").into(),
                    bytes: payload.len() as u64,
                    sha256: digest,
                    profile_sha256: sha(b"synthetic image profile"),
                    mime: "image/png".into(),
                    page: Some(page),
                    region_id: None,
                },
                copied_path: format!("/synthetic/never-read/image-{page}.png").into(),
                source_sha256: backend.prepared.source_sha256.clone(),
                prepared_sha256: backend.prepared.prepared_sha256.clone(),
                width: 2,
                height: 2,
            });
        }
        let expected_ids = backend
            .images()
            .iter()
            .map(|image| image.artifact.id.clone())
            .collect::<Vec<_>>();
        assert!(
            run_protocol(
                startup("chatgpt").as_slice(),
                Captured(captured.clone()),
                &config,
                &mut backend,
                DocumentRunBudget {
                    working: DocumentWorking::new(config.limits.working_bytes),
                    transport: TransportBudget::new(config.limits.total_rpc_bytes),
                    deadline: Instant::now() + Duration::from_secs(30),
                },
                None,
                qualification,
            )
            .await
            .is_err()
        );
        assert_eq!(backend.calls, 0);
        let frames = sent_frames(&captured);
        let thread = frames
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .expect("emitted thread/start");
        let instructions = thread["params"]["baseInstructions"].as_str().unwrap();
        assert!(instructions.contains("When native_images is exposed"));
        assert!(instructions.contains(
            "literal native view_image using only the exact issued original-image paths"
        ));
        assert!(instructions.contains("Do not invent or substitute paths or access other files"));
        assert!(instructions.contains("Read windows require max_bytes from 1 through 32768"));
        assert!(
            instructions.contains("For an image read_artifact, use cursor=null and max_bytes=1")
        );
        assert!(
            instructions.contains("complete image under the independent image and wire budgets")
        );
        assert!(instructions.contains(
            "For toolbox operations, use only issued opaque artifact/cursor/task identities"
        ));
        assert!(instructions.contains(
            "do not infer unread fields or claim complete model inspection from a partial window"
        ));
        assert!(!instructions.contains("using only gptgrep_parse.toolbox"));
        assert!(!instructions.contains("transcription of the actual visible text"));
        let turn = frames
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .expect("emitted turn/start");
        let prompt: Value =
            serde_json::from_str(turn["params"]["input"][0]["text"].as_str().unwrap())?;
        let mut product_schema = turn["params"]["outputSchema"].clone();
        assert_eq!(prompt["document_tool_budget"], 64);
        if let Some(purpose) = qualification {
            let observations = &product_schema["properties"]["image_observations"];
            let artifact_schema = &observations["items"]["properties"]["artifact_id"];
            assert_eq!(
                artifact_schema,
                &json!({"type":"string","enum":expected_ids})
            );
            assert_eq!(artifact_schema["enum"].as_array().unwrap().len(), 73);
            assert_eq!(observations["minItems"], 1);
            assert_eq!(observations["maxItems"], 64);
            assert!(!artifact_schema.to_string().contains("/synthetic/"));
            product_schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("image_observations");
            assert_eq!(
                product_schema["required"].as_array_mut().unwrap().pop(),
                Some(json!("image_observations"))
            );
            let observation = &prompt["private_qualification_observation"];
            assert_eq!(observation["purpose_sha256"], purpose);
            assert_eq!(observation["additional_authority"], false);
            assert_eq!(
                observation
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                ["purpose_sha256", "task", "additional_authority"]
                    .into_iter()
                    .collect::<BTreeSet<_>>()
            );
            let task = observation["task"].as_str().unwrap();
            assert!(task.contains("this Document agent must directly call literal native view_image on at least one admitted original image"));
            assert!(task.contains(
                "before describing each observed image and before finalize or finishing"
            ));
            assert!(task.contains("OCR-child text or OCR-child viewing, source metadata, and mediated images/crops do not satisfy"));
            assert!(task.contains("does not require viewing every original page or grant access to new paths or other files"));
            assert!(task.contains("When text is present, include a transcription of the actual visible text in each image description"));
            assert!(
                task.contains(
                    "For nontext content, describe distinctive visible shapes and colors"
                )
            );
            assert!(task.contains("State uncertainty for unclear content and do not guess"));
            assert!(task.contains("Generic layout-only prose does not replace observed content"));
            assert!(!task.contains(purpose));
        } else {
            assert!(prompt.get("private_qualification_observation").is_none());
        }
        assert_eq!(
            product_schema,
            json!({"type":"object","additionalProperties":false,"properties":{"status":{"type":"string","enum":["complete","blocked"]},"handle":{"type":"string","pattern":"^[a-f0-9]{64}$"}},"required":["status","handle"]})
        );
    }
    Ok(())
}

#[cfg(unix)]
fn duration_profile_value(config: &DocumentAgentControl, duration: Option<Value>) -> Result<Value> {
    let mut value = json!({
        "schema":"gptgrep.parse-agent.document-runtime-profile.v1",
        "source_revision":"1".repeat(40), "protocol_schema_sha256":"2".repeat(64),
        "codex_binary_sha256":config.codex_bin.sha256,
        "descriptor_sha256":config.model_descriptor.sha256,
        "permissions_config_sha256":config.permissions_config.sha256,
        "permissions_template_sha256":sha(&serde_json::to_vec(&native::canonical(document_permissions_template(true)))?),
        "tools_sha256":document_tool_policy_sha256()?,
        "instructions_sha256":document_instructions_sha256(),
        "standalone_app_server":true,
        "native_document_guard":{
            "schema":"gptgrep.native-document.guard-profile.v1",
            "source_sha256":"3".repeat(64),
            "contract_sha256":document_guard_contract_sha256()?
        }
    });
    if let Some(duration) = duration {
        value["max_seconds"] = duration;
    }
    Ok(value)
}
#[cfg(unix)]
fn duration_profile(
    config: &DocumentAgentControl,
    duration: Option<Value>,
) -> Result<RuntimeProfile> {
    Ok(serde_json::from_value(duration_profile_value(
        config, duration,
    )?)?)
}

#[cfg(unix)]
#[test]
fn explicit_document_duration_rejects_mismatch_non_native_and_nonfinite_inputs() -> Result<()> {
    let (_temporary, mut config, native, _) = retained_native_fixture()?;
    config.native_viewer = Some(native);
    let legacy = duration_profile(&config, None)?;
    assert_eq!(legacy.selected_max_seconds(&config)?, 900);
    config.limits.timeout_ms = 3_600_000;
    assert!(legacy.selected_max_seconds(&config).is_err());
    let selected = duration_profile(&config, Some(json!(3600)))?;
    assert_eq!(selected.selected_max_seconds(&config)?, 3600);
    config.limits.timeout_ms = 3_600_001;
    assert!(selected.selected_max_seconds(&config).is_err());
    for seconds in [0, 3601, u64::MAX] {
        let profile = duration_profile(&config, Some(json!(seconds)))?;
        assert!(profile.selected_max_seconds(&config).is_err());
        assert!(document_guard_contract_sha256_for(seconds).is_err());
        assert!(native::duration_millis(seconds).is_err());
    }
    for value in [Value::Null, json!(-1), json!(1.5), json!("3600")] {
        assert!(duration_profile(&config, Some(value)).is_err());
    }
    assert_eq!(native::duration_millis(1)?, 1000);
    assert_eq!(native::duration_millis(3600)?, 3_600_000);
    config.native_viewer = None;
    config.limits.timeout_ms = 3_600_000;
    assert!(selected.selected_max_seconds(&config).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn document_duration_binds_all_18_fields_and_preserves_absent_900_digest() -> Result<()> {
    let (_temporary, mut config, native, _) = retained_native_fixture()?;
    config.native_viewer = Some(native);
    let mut profile = duration_profile(&config, None)?;
    let legacy = qualification::expected(&config, &profile)?;
    let guard = profile.native_document_guard.as_ref().expect("guard");
    // Freeze the complete legacy generic descriptor: no max_seconds field.
    let legacy_generic = native::canonical(json!({
        "schema":"gptgrep.parse-agent.document-generic-runtime-profile.v1",
        "source_revision":profile.source_revision,"protocol_schema_sha256":profile.protocol_schema_sha256,
        "standalone_app_server":profile.standalone_app_server,"codex_binary_sha256":profile.codex_binary_sha256,
        "descriptor_sha256":profile.descriptor_sha256,"permissions_template_sha256":profile.permissions_template_sha256,
        "tools_sha256":profile.tools_sha256,"instructions_sha256":profile.instructions_sha256,
        "native_document_guard":{"schema":guard.schema,"source_sha256":guard.source_sha256,"contract_sha256":guard.contract_sha256}
    }));
    assert!(legacy_generic.get("max_seconds").is_none());
    assert_eq!(
        legacy.generic_runtime_profile_sha256,
        sha(&serde_json::to_vec(&legacy_generic)?)
    );
    assert_eq!(legacy.max_seconds, 900);
    assert_eq!(
        document_guard_contract_sha256()?,
        document_guard_contract_sha256_for(900)?
    );
    assert_eq!(
        serde_json::to_value(&legacy)?
            .as_object()
            .expect("18 bindings")
            .len(),
        18
    );

    profile.max_seconds = Some(3600);
    config.limits.timeout_ms = 3_600_000;
    profile
        .native_document_guard
        .as_mut()
        .expect("guard")
        .contract_sha256 = document_guard_contract_sha256_for(3600)?;
    let selected = qualification::expected(&config, &profile)?;
    assert_eq!(selected.max_seconds, 3600);
    assert_ne!(selected.guard_contract_sha256, legacy.guard_contract_sha256);
    assert_ne!(
        selected.generic_runtime_profile_sha256,
        legacy.generic_runtime_profile_sha256
    );
    let mut selected_fields = serde_json::to_value(&selected)?;
    let mut legacy_fields = serde_json::to_value(&legacy)?;
    for field in [
        "max_seconds",
        "guard_contract_sha256",
        "generic_runtime_profile_sha256",
    ] {
        selected_fields.as_object_mut().unwrap().remove(field);
        legacy_fields.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(selected_fields, legacy_fields);
    // The other source/binary fields above are held synthetic and identical only
    // for this unit check. A real duration build must rebind its actual new pins.
    Ok(())
}

#[cfg(unix)]
#[test]
fn selected_document_duration_is_emitted_in_admission_and_ready_only_when_explicit() -> Result<()> {
    for duration in [None, Some(3600)] {
        let (_temporary, mut config, native, backend) = retained_native_fixture()?;
        let selected = duration.unwrap_or(900);
        config.limits.timeout_ms = selected * 1000;
        let mut session = native::NativeSession::prepare_with_duration(
            &native,
            &config,
            &backend,
            &DocumentWorking::new(67_108_864),
            Instant::now() + Duration::from_secs(selected),
            duration,
        )?;
        let bytes = fs::read(&native.admission_path)?;
        let manifest: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(
            manifest.get("max_seconds"),
            duration.map(|value| json!(value)).as_ref()
        );
        assert_eq!(
            manifest.as_object().unwrap().len(),
            if duration.is_some() { 16 } else { 15 }
        );
        let ready = json!({"schema_version":"gptgrep.native-document.ready.v1","required":true,
            "admission_sha256":sha(&bytes),"task_sha256":backend.prepared().task_sha256,
            "source_sha256":backend.prepared().source_sha256,"prepared_sha256":backend.prepared().prepared_sha256,
            "runtime_profile_sha256":config.runtime_profile.sha256,"permissions_profile_id":"gptgrep_native_document",
            "permissions_profile_sha256":manifest["permissions_profile_sha256"],"environment_ids":["local"],
            "environment_id_argument_admitted":false,"max_tool_calls":64,"max_turns":1,"max_seconds":selected,
            "deadline_unix_ms":manifest["deadline_unix_ms"],"frame_bytes":4_194_304,"image_transport_reserved_bytes":33_554_432,
            "tool_counter_before_lookup_hooks_handler":true,"tool_counter_before_argument_parse":true,
            "normalized_request_profile_verified_before_send":true,"native_image_read_bounded":true,
            "assembled_image_receipts_required":true,"requested_model":"gpt-6-luna","requested_effort":"max",
            "normalized_request_service_tier":"priority","actual_served_profile_verified":false,
            "image_catalog_sha256":manifest["image_catalog"]["sha256"],"image_catalog_count":1});
        let mut wrong = ready.clone();
        wrong["max_seconds"] = json!(selected + 1);
        assert!(
            session
                .initialize(&json!({"gptgrepNativeDocument":wrong}))
                .is_err()
        );
        session.initialize(&json!({"gptgrepNativeDocument":ready}))?;
    }
    Ok(())
}

struct CleanupBackend {
    inner: Backend,
    calls: usize,
    fail: bool,
    completed: Arc<std::sync::atomic::AtomicBool>,
    ocr: Option<crate::OcrCleanup>,
}
impl DocumentBackend for CleanupBackend {
    fn prepared(&self) -> &DocumentPreparedState {
        self.inner.prepared()
    }
    fn images(&self) -> &[DocumentImageGrant] {
        self.inner.images()
    }
    fn reserve_retained(&self, bytes: u64) -> Result<()> {
        self.inner.reserve_retained(bytes)
    }
    fn call<'a>(
        &'a mut self,
        operation: DocumentToolOperation,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentToolReply>> + Send + 'a>> {
        self.inner.call(operation)
    }
    fn verify_complete(&self) -> Result<DocumentVerifiedProduct> {
        self.inner.verify_complete()
    }
    fn cancel_and_drain_ocr<'a>(
        &'a mut self,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        self.calls += 1;
        Box::pin(async move {
            if let Some(ocr) = &self.ocr {
                ocr.cancel_and_drain(deadline).await?;
            }
            tokio::task::yield_now().await;
            self.completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ensure!(!self.fail, "synthetic_cleanup_failure");
            Ok(())
        })
    }
}
#[tokio::test]
async fn document_cleanup_is_awaited_and_primary_timeout_survives_cleanup_failure() -> Result<()> {
    for fail in [false, true] {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut backend = CleanupBackend {
            inner: Backend::new(),
            calls: 0,
            fail,
            completed: completed.clone(),
            ocr: None,
        };
        let primary = backend_failure("execute_ocr", None, None);
        let original = primary.to_string();
        let error = finish_document_cleanup::<()>(&mut backend, Err(primary), Some(Instant::now()))
            .await
            .expect_err("primary timeout");
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(backend.calls, 1);
        assert!(error.to_string().starts_with(&original));
        assert_eq!(
            error.to_string().contains("document_ocr_cleanup_unknown"),
            fail
        );
        assert!(error.chain().any(|cause| cause.to_string() == original));
    }
    Ok(())
}
#[tokio::test]
async fn document_validation_return_also_waits_for_backend_cleanup() -> Result<()> {
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut backend = CleanupBackend {
        inner: Backend::new(),
        calls: 0,
        fail: false,
        completed: completed.clone(),
        ocr: None,
    };
    let mut invalid = control()?;
    invalid.schema = "invalid-before-any-runtime".into();
    let error = drive_document_agent(
        &invalid,
        &mut backend,
        DocumentWorking::new(67_108_864),
        Instant::now(),
    )
    .await
    .err()
    .expect("validation refusal");
    assert_eq!(error.to_string(), "document_control_schema");
    assert_eq!(backend.calls, 1);
    assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
    Ok(())
}

#[cfg(unix)]
#[test]
fn document_profile_validation_requires_the_selected_duration_contract_and_exact_caller()
-> Result<()> {
    fn pin_json(root: &Path, name: &str, value: &Value) -> Result<DocumentFilePin> {
        let bytes = serde_json::to_vec(value)?;
        let path = root.join(name);
        fs::write(&path, &bytes)?;
        Ok(DocumentFilePin {
            path,
            bytes: bytes.len() as u64,
            sha256: sha(&bytes),
        })
    }
    let (temporary, mut config, native, _) = retained_native_fixture()?;
    config.native_viewer = Some(native);
    config.model_descriptor = pin_json(
        temporary.path(),
        "descriptor.json",
        &document_model_descriptor(),
    )?;
    config.permissions_config = pin_json(
        temporary.path(),
        "permissions.json",
        &document_permissions_config(config.native_viewer.as_ref()),
    )?;
    config.limits.timeout_ms = 3_600_000;
    let mut selected = duration_profile_value(&config, Some(json!(3600)))?;
    config.runtime_profile = pin_json(temporary.path(), "profile.json", &selected)?;
    assert!(validate_profile(&config).is_err()); // Legacy 900 contract cannot authorize 3600.
    selected["native_document_guard"]["contract_sha256"] =
        json!(document_guard_contract_sha256_for(3600)?);
    config.runtime_profile = pin_json(temporary.path(), "profile.json", &selected)?;
    assert_eq!(
        validate_profile(&config)?.selected_max_seconds(&config)?,
        3600
    );
    config.limits.timeout_ms = 900_000;
    assert!(validate_profile(&config).is_err());
    let legacy = duration_profile_value(&config, None)?;
    assert!(legacy.get("max_seconds").is_none());
    config.runtime_profile = pin_json(temporary.path(), "profile.json", &legacy)?;
    assert_eq!(
        validate_profile(&config)?.selected_max_seconds(&config)?,
        900
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn document_return_barrier_waits_for_real_owned_mock_after_submitted_timeout() -> Result<()> {
    let fixture = crate::ocr_tests::Fixture::new();
    let (executor, context, pid_file) = crate::ocr_tests::mock_executor(&fixture, "hang", true);
    let cleanup = crate::OcrCleanup::new(64)?;
    let executor = executor.with_cleanup(cleanup.clone());
    let cancel = crate::OcrCancellation::default();
    let mut operation = Box::pin(executor.execute_cancellable(fixture.task.clone(), cancel));
    tokio::select! {
        result = &mut operation => panic!("mock ended before submission: {result:?}"),
        () = crate::ocr_tests::wait_for_submitted_mock(&pid_file) => {}
    }
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(&pid_file)?)?;
    assert!(
        timeout_at(Instant::now() + Duration::from_millis(1), operation)
            .await
            .is_err()
    );
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut backend = CleanupBackend {
        inner: Backend::new(),
        calls: 0,
        fail: false,
        completed: completed.clone(),
        ocr: Some(cleanup),
    };
    let original = backend_failure("execute_ocr", None, None).to_string();
    let error = finish_document_cleanup::<()>(
        &mut backend,
        Err(backend_failure("execute_ocr", None, None)),
        Some(Instant::now()),
    )
    .await
    .expect_err("original timeout");
    assert_eq!(error.to_string(), original);
    assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(backend.calls, 1);
    let leader = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pids[0].to_string()])
        .output()?;
    assert!(
        leader.stdout.is_empty(),
        "Document returned before OCR leader reap"
    );
    let snapshot = context.snapshot()?;
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.terminal_admissions, 0);
    assert_eq!(snapshot.quarantined_slots, 1);
    Ok(())
}
