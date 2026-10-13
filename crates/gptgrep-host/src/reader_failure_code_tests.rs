//! Stable failure codes for the Codex reader process.
//!
//! Each scripted fault ends the restricted retrieval reader with one fixed
//! code. The durable model attempt and the ask stage both report that code
//! instead of the generic `host_model_attempt_failed` and `host_codex_failed`,
//! and no fixture text reaches the error.
use super::*;
use crate::{jev_accounting::Accounting, model_attempts::ModelAttemptTracker};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const SENTINEL: &str = "private-fixture-sentinel";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    InitializeRejected,
    AccountRejected,
    ChatgptLoginRequired,
    ConfigRejected,
    ResponseWithoutResult,
    ConfigUnavailable,
    McpConfigUnsupported,
    McpServerLimit,
    ThreadRejected,
    ThreadIdentityMissing,
    ThreadIdentityInvalid,
    ModelProviderChanged,
    ModelChanged,
    ApprovalPolicyRejected,
    PermissionProfileUnacknowledged,
    SandboxRejected,
    ReasoningEffortChanged,
    ServiceTierInvalid,
    ServiceTierChanged,
    InputTransportClosed,
    TurnStartRejected,
    NoFinalAnswer,
    AgentMessageWithoutText,
    FinalMessageInvalid,
    FinalResponseTooLarge,
    ItemNotAllowed,
    ItemToolNotAllowed,
    EventThreadMismatch,
    TurnIdentityChanged,
    ToolCallThreadMismatch,
    ToolCallRepeated,
    ToolNameInvalid,
    ToolNotAllowed,
    ToolArgumentsTooLarge,
    InvalidJson,
    AppServerClosed,
    OutputTransportFailed,
    FrameTooLarge,
    MessageLimit,
}

const FAULTS: &[(Fault, &str)] = &[
    (Fault::InitializeRejected, "host_codex_initialize_rejected"),
    (Fault::AccountRejected, "host_codex_account_read_rejected"),
    (
        Fault::ChatgptLoginRequired,
        "host_codex_chatgpt_login_required",
    ),
    (Fault::ConfigRejected, "host_codex_config_read_rejected"),
    (
        Fault::ResponseWithoutResult,
        "host_codex_response_without_result",
    ),
    (Fault::ConfigUnavailable, "host_codex_config_unavailable"),
    (
        Fault::McpConfigUnsupported,
        "host_codex_mcp_config_unsupported",
    ),
    (Fault::McpServerLimit, "host_codex_mcp_server_limit"),
    (Fault::ThreadRejected, "host_codex_thread_start_rejected"),
    (Fault::ThreadIdentityMissing, "host_codex_identity_missing"),
    (Fault::ThreadIdentityInvalid, "host_codex_identity_invalid"),
    (
        Fault::ModelProviderChanged,
        "host_codex_model_provider_changed",
    ),
    (Fault::ModelChanged, "host_codex_model_changed"),
    (
        Fault::ApprovalPolicyRejected,
        "host_codex_approval_policy_rejected",
    ),
    (
        Fault::PermissionProfileUnacknowledged,
        "host_codex_permission_profile_unacknowledged",
    ),
    (Fault::SandboxRejected, "host_codex_sandbox_rejected"),
    (
        Fault::ReasoningEffortChanged,
        "host_codex_reasoning_effort_changed",
    ),
    (Fault::ServiceTierInvalid, "host_codex_service_tier_invalid"),
    (Fault::ServiceTierChanged, "host_codex_service_tier_changed"),
    (
        Fault::InputTransportClosed,
        "host_codex_input_transport_closed",
    ),
    (Fault::TurnStartRejected, "host_codex_turn_start_rejected"),
    (Fault::NoFinalAnswer, "host_codex_no_final_answer"),
    (
        Fault::AgentMessageWithoutText,
        "host_codex_agent_message_without_text",
    ),
    (
        Fault::FinalMessageInvalid,
        "host_codex_final_message_invalid",
    ),
    (
        Fault::FinalResponseTooLarge,
        "host_codex_final_response_too_large",
    ),
    (Fault::ItemNotAllowed, "host_codex_item_not_allowed"),
    (
        Fault::ItemToolNotAllowed,
        "host_codex_item_tool_not_allowed",
    ),
    (
        Fault::EventThreadMismatch,
        "host_codex_event_thread_mismatch",
    ),
    (
        Fault::TurnIdentityChanged,
        "host_codex_turn_identity_changed",
    ),
    (
        Fault::ToolCallThreadMismatch,
        "host_codex_tool_call_thread_mismatch",
    ),
    (Fault::ToolCallRepeated, "host_codex_tool_call_repeated"),
    (Fault::ToolNameInvalid, "host_codex_tool_name_invalid"),
    (Fault::ToolNotAllowed, "host_codex_tool_not_allowed"),
    (
        Fault::ToolArgumentsTooLarge,
        "host_codex_tool_arguments_too_large",
    ),
    (Fault::InvalidJson, "host_codex_invalid_json"),
    (Fault::AppServerClosed, "host_codex_app_server_closed"),
    (
        Fault::OutputTransportFailed,
        "host_codex_output_transport_failed",
    ),
    (Fault::FrameTooLarge, "host_codex_frame_too_large"),
    (Fault::MessageLimit, "host_codex_message_limit"),
];

/// Client stdout that reports the app-server's end of output as a read error.
struct ClientRead {
    inner: ReadHalf<DuplexStream>,
    fail_at_eof: bool,
}
impl AsyncRead for ClientRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = output.filled().len();
        let fail_at_eof = self.fail_at_eof;
        match Pin::new(&mut self.inner).poll_read(cx, output) {
            Poll::Ready(Ok(()))
                if fail_at_eof && output.remaining() > 0 && output.filled().len() == before =>
            {
                Poll::Ready(Err(std::io::Error::other(SENTINEL)))
            }
            other => other,
        }
    }
}

/// Client stdin that fails after a fixed number of complete frames.
struct ClientWrite {
    inner: WriteHalf<DuplexStream>,
    frames_before_failure: Option<usize>,
    frames: usize,
}
impl AsyncWrite for ClientWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self
            .frames_before_failure
            .is_some_and(|limit| self.frames >= limit)
        {
            return Poll::Ready(Err(std::io::Error::other(SENTINEL)));
        }
        let written = std::task::ready!(Pin::new(&mut self.inner).poll_write(cx, bytes))?;
        self.frames += bytes[..written]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        Poll::Ready(Ok(written))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn expect(reader: &mut Reader, method: &str) {
    assert_eq!(receive(reader).await["method"], method);
}
/// Keep the scripted app-server open until the client drops its transport.
async fn until_closed(reader: &mut Reader) {
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .await
        .is_ok_and(|count| count > 0)
    {
        line.clear();
    }
}
fn rejected(id: u64) -> Value {
    json!({"id":id,"error":{"code":-32603,"message":SENTINEL}})
}
fn item_completed(item: Value) -> Value {
    json!({"method":"item/completed","params":{
        "threadId":"thread-native","turnId":"turn-native","item":item}})
}
fn turn_completed(items: Value) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread-native",
        "turn":{"id":"turn-native","status":"completed","items":items}}})
}
fn tool_call(id: u64, call_id: &str, tool: Value, arguments: Value) -> Value {
    json!({"id":id,"method":"item/tool/call","params":{
        "threadId":"thread-native","turnId":"turn-native","callId":call_id,
        "namespace":"gptgrep","tool":tool,"arguments":arguments}})
}

/// A scripted app-server that answers the startup sequence until `fault`.
async fn serve(fault: Fault, mut reader: Reader, mut writer: Writer) {
    use Fault::*;
    expect(&mut reader, "initialize").await;
    if fault == InitializeRejected {
        send(&mut writer, rejected(1)).await;
        return until_closed(&mut reader).await;
    }
    send(&mut writer, json!({"id":1,"result":{}})).await;
    expect(&mut reader, "initialized").await;
    expect(&mut reader, "account/read").await;
    let account = match fault {
        AccountRejected => rejected(100),
        ChatgptLoginRequired => json!({"id":100,"result":{"account":{"type":"apiKey"}}}),
        _ => json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
    };
    send(&mut writer, account).await;
    if matches!(fault, AccountRejected | ChatgptLoginRequired) {
        return until_closed(&mut reader).await;
    }
    expect(&mut reader, "config/read").await;
    let servers: serde_json::Map<String, Value> = (0..129)
        .map(|index| (format!("server-{index}"), json!({})))
        .collect();
    let config = match fault {
        ConfigRejected => rejected(2),
        ResponseWithoutResult => json!({"id":2}),
        ConfigUnavailable => json!({"id":2,"result":{"config":[SENTINEL]}}),
        McpConfigUnsupported => json!({"id":2,"result":{"config":{"mcp_servers":[SENTINEL]}}}),
        McpServerLimit => json!({"id":2,"result":{"config":{"mcp_servers":servers}}}),
        _ => json!({"id":2,"result":{"config":{}}}),
    };
    send(&mut writer, config).await;
    if matches!(
        fault,
        ConfigRejected
            | ResponseWithoutResult
            | ConfigUnavailable
            | McpConfigUnsupported
            | McpServerLimit
    ) {
        return until_closed(&mut reader).await;
    }
    expect(&mut reader, "thread/start").await;
    let mut thread = json!({"thread":{"id":"thread-native"},"model":DEFAULT_MODEL,
        "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
        "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}});
    match fault {
        ThreadIdentityMissing => thread["thread"] = json!({}),
        ThreadIdentityInvalid => thread["thread"]["id"] = json!(""),
        ModelProviderChanged => thread["modelProvider"] = json!(SENTINEL),
        ModelChanged => thread["model"] = json!(SENTINEL),
        ApprovalPolicyRejected => thread["approvalPolicy"] = json!("on-request"),
        SandboxRejected => thread["sandbox"] = json!({"type":"dangerFullAccess"}),
        ReasoningEffortChanged => thread["reasoningEffort"] = json!("low"),
        ServiceTierInvalid => thread["serviceTier"] = json!(true),
        ServiceTierChanged => thread["serviceTier"] = json!("flex"),
        _ => {}
    }
    let thread = if fault == ThreadRejected {
        rejected(3)
    } else {
        json!({"id":3,"result":thread})
    };
    send(&mut writer, thread).await;
    if matches!(
        fault,
        ThreadRejected
            | ThreadIdentityMissing
            | ThreadIdentityInvalid
            | ModelProviderChanged
            | ModelChanged
            | ApprovalPolicyRejected
            | PermissionProfileUnacknowledged
            | SandboxRejected
            | ReasoningEffortChanged
            | ServiceTierInvalid
            | ServiceTierChanged
            | InputTransportClosed
    ) {
        return until_closed(&mut reader).await;
    }
    expect(&mut reader, "turn/start").await;
    if fault == TurnStartRejected {
        send(&mut writer, rejected(4)).await;
        return until_closed(&mut reader).await;
    }
    send(
        &mut writer,
        json!({"id":4,"result":{"turn":{"id":"turn-native","status":"inProgress"}}}),
    )
    .await;
    let message = match fault {
        NoFinalAnswer => Some(turn_completed(json!([]))),
        AgentMessageWithoutText => Some(item_completed(
            json!({"type":"agentMessage","phase":"final_answer","text":7}),
        )),
        FinalMessageInvalid => Some(turn_completed(
            json!([{"type":"agentMessage","phase":"final_answer","text":null}]),
        )),
        FinalResponseTooLarge => Some(item_completed(
            json!({"type":"agentMessage","phase":"final_answer","text":"x".repeat(16 * 1024 + 1)}),
        )),
        ItemNotAllowed => Some(item_completed(
            json!({"type":"commandExecution","command":SENTINEL}),
        )),
        ItemToolNotAllowed => Some(item_completed(
            json!({"type":"dynamicToolCall","namespace":"gptgrep","tool":SENTINEL}),
        )),
        EventThreadMismatch => {
            let mut message = item_completed(json!({"type":"reasoning"}));
            message["params"]["threadId"] = json!(SENTINEL);
            Some(message)
        }
        TurnIdentityChanged => {
            let mut message = item_completed(json!({"type":"reasoning"}));
            message["params"]["turnId"] = json!(SENTINEL);
            Some(message)
        }
        ToolCallThreadMismatch => {
            let mut message = tool_call(10, "call-10", json!("gptgrep_catalog"), json!({}));
            message["params"]["threadId"] = json!(SENTINEL);
            Some(message)
        }
        ToolCallRepeated => {
            call(&mut reader, &mut writer, 10, "gptgrep_catalog", json!({})).await;
            Some(tool_call(
                11,
                "call-10",
                json!("gptgrep_catalog"),
                json!({}),
            ))
        }
        ToolNameInvalid => Some(tool_call(10, "call-10", json!(7), json!({}))),
        ToolNotAllowed => {
            send(
                &mut writer,
                tool_call(10, "call-10", json!(SENTINEL), json!({})),
            )
            .await;
            assert_eq!(receive(&mut reader).await["error"]["code"], -32601);
            None
        }
        ToolArgumentsTooLarge => Some(tool_call(
            10,
            "call-10",
            json!("gptgrep_search"),
            json!({"query":SENTINEL.repeat(256)}),
        )),
        InvalidJson => {
            writer
                .write_all(format!("{SENTINEL}\n").as_bytes())
                .await
                .unwrap();
            None
        }
        AppServerClosed | OutputTransportFailed => {
            writer.shutdown().await.unwrap();
            None
        }
        FrameTooLarge => {
            // The client stops reading at the limit; the rest of the write fails.
            let _ = writer.write_all(&vec![b' '; FRAME_LIMIT + 1]).await;
            None
        }
        MessageLimit => {
            let event = format!("{}\n", json!({"method":"synthetic/event","params":{}}));
            for _ in 0..4096 {
                if writer.write_all(event.as_bytes()).await.is_err() {
                    break;
                }
            }
            None
        }
        _ => unreachable!("{fault:?} ends before the turn"),
    };
    if let Some(message) = message {
        send(&mut writer, message).await;
    }
    until_closed(&mut reader).await;
}

async fn reader_failure(root: &Path, fault: Fault) -> (anyhow::Error, ModelAttempt) {
    let config = HostConfig {
        permission_profile: (fault == Fault::PermissionProfileUnacknowledged)
            .then(|| "gptgrep_reader".to_owned()),
        ..HostConfig::default()
    };
    let mut evidence = Evidence::open(root, None).unwrap();
    let accounting = Accounting::create(root, &evidence.generation).unwrap();
    accounting.bind_workflow("query", None).unwrap();
    accounting.set_model_attempt_limit(1).unwrap();
    let mut attempt = ModelAttemptTracker::reserve(&accounting, "final_reader", &config).unwrap();
    attempt.observer().running().unwrap();
    let (client, wire) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, server_writer) = tokio::io::split(wire);
    let server = tokio::spawn(serve(fault, BufReader::new(server_reader), server_writer));
    let reader = ClientRead {
        inner: reader,
        fail_at_eof: fault == Fault::OutputTransportFailed,
    };
    // initialize, initialized, account/read, config/read and thread/start.
    let writer = ClientWrite {
        inner: writer,
        frames_before_failure: (fault == Fault::InputTransportClosed).then_some(5),
        frames: 0,
    };
    let result = protocol::run_observed(
        BufReader::new(reader),
        writer,
        root,
        &config,
        protocol::Workflow::Retrieval {
            question: "query",
            node_id: None,
            evidence: &mut evidence,
        },
        None,
        protocol::RunOptions {
            observer: Some(attempt.observer()),
            ..Default::default()
        },
    )
    .await;
    let Err(error) = result else {
        panic!("{fault:?} completed the reader turn");
    };
    attempt.finish(Some(&error)).unwrap();
    server.await.unwrap();
    let attempts = accounting.model_attempts();
    assert_eq!(attempts.len(), 1);
    (error, attempts[0].clone())
}

fn assert_coded(error: &anyhow::Error, attempt: &ModelAttempt, stage: &str, code: &str) {
    // The code is the complete error: no fixture or transport text is retained.
    assert_eq!(format!("{error:#}"), code);
    assert_eq!(attempt.status, "failed", "{code}");
    assert_eq!(attempt.error.as_ref().unwrap()["code"], code);
    assert_eq!(failure_code(error, stage), code);
    assert!(code.starts_with("host_") && code.len() <= 128);
    assert!(
        code.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    );
    assert_ne!(code, "host_model_attempt_failed");
    assert_ne!(code, format!("host_{stage}_failed"));
}

#[tokio::test]
async fn every_scripted_reader_fault_reports_its_stable_code() {
    let root = fixture().await;
    let mut codes = std::collections::BTreeSet::new();
    for &(fault, code) in FAULTS {
        let (error, attempt) = reader_failure(root.path(), fault).await;
        assert_eq!(error.to_string(), code, "{fault:?}");
        assert_coded(&error, &attempt, "codex", code);
        assert!(codes.insert(code), "{code} is reused");
    }
    let ledgers = std::fs::read_dir(root.path().join(".gptgrep/host-attempts")).unwrap();
    for ledger in ledgers {
        let raw = std::fs::read_to_string(ledger.unwrap().path()).unwrap();
        assert!(!raw.contains("private-"));
        assert!(!raw.contains("host_model_attempt_failed"));
    }
}

async fn process_failure(root: &Path, config: &HostConfig) -> (anyhow::Error, ModelAttempt) {
    let mut evidence = Evidence::open(root, None).unwrap();
    let accounting = Accounting::create(root, &evidence.generation).unwrap();
    accounting.bind_workflow("query", None).unwrap();
    accounting.set_model_attempt_limit(1).unwrap();
    let mut attempt = ModelAttemptTracker::reserve(&accounting, "final_reader", config).unwrap();
    let result = run_process_until(
        config,
        protocol::Workflow::Retrieval {
            question: "query",
            node_id: None,
            evidence: &mut evidence,
        },
        None,
        protocol::RunOptions {
            observer: Some(attempt.observer()),
            ..Default::default()
        },
    )
    .await;
    let Err(error) = result else {
        panic!("the synthetic process startup succeeded");
    };
    attempt.finish(Some(&error)).unwrap();
    (error, accounting.model_attempts()[0].clone())
}

#[tokio::test]
async fn reader_process_startup_failures_report_stable_codes() {
    let root = fixture().await;
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let file = home_path.join("not-a-directory");
    std::fs::write(&file, b"fixture").unwrap();
    let trace = home_path.join("existing.trace.jsonl");
    std::fs::write(&trace, b"").unwrap();
    let base = HostConfig {
        codex_home: home_path.clone(),
        // One path component: resolved through PATH at spawn time.
        codex_bin: "gptgrep-synthetic-missing-codex".into(),
        timeout_secs: 30,
        ..HostConfig::default()
    };
    for (config, code) in [
        (
            HostConfig {
                trace_path: Some(trace.clone()),
                ..base.clone()
            },
            "host_trace_file_unavailable",
        ),
        (
            HostConfig {
                codex_home: home_path.join("missing"),
                ..base.clone()
            },
            "host_codex_home_unavailable",
        ),
        (
            HostConfig {
                codex_bin: home_path
                    .join("missing-codex")
                    .to_string_lossy()
                    .into_owned(),
                ..base.clone()
            },
            "host_codex_binary_unavailable",
        ),
        (
            HostConfig {
                codex_home: file.clone(),
                ..base.clone()
            },
            "host_codex_home_not_directory",
        ),
        (
            HostConfig {
                codex_state_dir: Some("relative-state".into()),
                ..base.clone()
            },
            "host_codex_state_dir_not_absolute",
        ),
        (base.clone(), "host_codex_spawn_failed"),
    ] {
        let (error, attempt) = process_failure(root.path(), &config).await;
        assert_coded(&error, &attempt, "codex", code);
    }
}

#[tokio::test]
async fn final_answer_failures_report_stable_codes_and_unchanged_causes() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let check = |result: Result<(String, Vec<Citation>, bool)>, code: &str, cause: &str| {
        let Err(error) = result else {
            panic!("{code}: the final answer was accepted");
        };
        assert_eq!(format!("{error:#}"), code);
        assert_eq!(failure_code(&error, "citation_validation"), code);
        assert_eq!(retrieval::citation_validation_code(&error), cause);
    };
    let answer = |citations: Value, insufficient: bool| {
        json!({"answer":"Claim","citations":citations,"insufficient_evidence":insufficient})
            .to_string()
    };
    for (raw, code, cause) in [
        (
            " ".repeat(16 * 1024 + 1),
            "host_answer_too_large",
            "final_response_byte_limit",
        ),
        (
            format!("{SENTINEL} prose"),
            "host_answer_invalid_json",
            "final_response_schema",
        ),
        (
            r#"{"answer":"Claim","citations":["#.into(),
            "host_answer_invalid_json",
            "final_response_schema",
        ),
        (
            r#"{"answer":12,"citations":[],"insufficient_evidence":true}"#.into(),
            "host_answer_schema_mismatch",
            "final_response_schema",
        ),
        (
            r#"{"answer":"Claim","citations":[],"insufficient_evidence":true,"private":1}"#.into(),
            "host_answer_schema_mismatch",
            "final_response_schema",
        ),
        (
            r#"{"answer":"Claim","answer":"Claim","citations":[],"insufficient_evidence":true}"#
                .into(),
            "host_answer_schema_mismatch",
            "final_response_schema",
        ),
        (
            r#"{"answer":" ","citations":[],"insufficient_evidence":true}"#.into(),
            "host_answer_length_invalid",
            "answer_length",
        ),
        (
            answer(json!(vec!["a:b"; 25]), false),
            "host_answer_citation_limit",
            "citation_count",
        ),
        (
            answer(json!([]), false),
            "host_answer_citations_missing",
            "missing_citations",
        ),
        (
            answer(json!([]), true),
            "host_no_evidence_tools",
            "no_evidence_tools",
        ),
    ] {
        check(evidence.finish(&raw), code, cause);
    }
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"handbook.md"}))
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = body["nodes"][0]["node_id"].as_str().unwrap().to_owned();
    evidence
        .call("read", "gptgrep_read", json!({"node_id":id}))
        .await
        .unwrap();
    check(
        evidence.finish(&answer(json!([id, id]), false)),
        "host_answer_citation_repeated",
        "duplicate_citation",
    );
    check(
        evidence.finish(&answer(json!([SENTINEL]), false)),
        "host_answer_citation_unissued",
        "unissued_citation",
    );
    let valid = answer(json!([id]), false);
    assert!(evidence.finish(&valid).is_ok());
    // The core refuses to reread a changed source before freshness is compared.
    std::fs::write(root.path().join("handbook.md"), "# Changed\nnew evidence").unwrap();
    check(
        evidence.finish(&valid),
        "host_answer_citation_window_unreadable",
        "unreadable_cited_window",
    );
    gptgrep_core::index(root.path(), 10).await.unwrap();
    check(
        evidence.finish(&valid),
        "host_generation_changed",
        "generation_changed",
    );
    std::fs::remove_dir_all(root.path().join(".gptgrep")).unwrap();
    check(
        evidence.finish(&valid),
        "host_index_unavailable",
        "index_unavailable",
    );
}

#[test]
fn typed_search_and_uncoded_errors_map_to_bounded_codes() {
    let jev = || {
        anyhow::Error::new(gptgrep_core::JevSearchError {
            stage: "evidence_reranking".into(),
            metrics: Default::default(),
            coverage: Default::default(),
            document_scope: None,
            generation: None,
            cause: SENTINEL.into(),
            accounting_complete: false,
        })
    };
    let config = HostConfig::default();
    for (error, ask, attempt_code) in [
        (jev(), "host_jev_search_failed", "host_jev_search_failed"),
        (
            anyhow!("host_codex_no_final_answer"),
            "host_codex_no_final_answer",
            "host_codex_no_final_answer",
        ),
        // A message that is not a code still falls back without its text.
        (
            anyhow!("{SENTINEL} detail"),
            "host_codex_failed",
            "host_model_attempt_failed",
        ),
    ] {
        assert_eq!(failure_code(&error, "codex"), ask);
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".gptgrep")).unwrap();
        let accounting = Accounting::create(directory.path(), "synthetic-generation").unwrap();
        accounting.bind_workflow("query", None).unwrap();
        accounting.set_model_attempt_limit(1).unwrap();
        let mut attempt =
            ModelAttemptTracker::reserve(&accounting, "final_reader", &config).unwrap();
        attempt.finish(Some(&error)).unwrap();
        let recorded = accounting.model_attempts()[0].error.clone();
        assert_eq!(recorded, Some(json!({"code":attempt_code})));
        let ledger = std::fs::read_to_string(accounting.path()).unwrap();
        assert!(!ledger.contains("private-"));
    }
}

/// Error literals in the reader process modules must be stable codes:
/// `anyhow!`/`bail!` carry the message first and `ensure!` second. A
/// non-literal message is checked by the runtime tests above.
fn uncoded_error_literals(name: &str, source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut uncoded = vec![];
    let mut index = 0;
    while index < bytes.len() {
        let Some((macro_name, message_argument)) =
            [("anyhow!(", 0), ("bail!(", 0), ("ensure!(", 1)]
                .into_iter()
                .find(|(name, _)| bytes[index..].starts_with(name.as_bytes()))
        else {
            index += 1;
            continue;
        };
        let line = source[..index].matches('\n').count() + 1;
        index += macro_name.len();
        let (mut depth, mut start, mut arguments) = (0usize, index, vec![]);
        while index < bytes.len() {
            match bytes[index] {
                b'"' => {
                    index += 1;
                    while bytes[index] != b'"' {
                        index += if bytes[index] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\'' if bytes.get(index + 2) == Some(&b'\'') => index += 2,
                b'(' | b'[' | b'{' => depth += 1,
                b')' if depth == 0 => {
                    arguments.push(source[start..index].trim());
                    break;
                }
                b')' | b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    arguments.push(source[start..index].trim());
                    start = index + 1;
                }
                _ => {}
            }
            index += 1;
        }
        let arguments: Vec<_> = arguments
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        let Some(message) = arguments.get(message_argument) else {
            continue;
        };
        let Some(text) = message
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
        else {
            continue;
        };
        let coded = text.starts_with("host_")
            && text.len() <= 128
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            && arguments.len() == message_argument + 1;
        if !coded {
            uncoded.push(format!("{name}:{line}: {text}"));
        }
    }
    uncoded
}

#[test]
fn reader_process_modules_use_only_stable_error_codes() {
    let mut uncoded = vec![];
    for (name, source) in [
        ("protocol.rs", include_str!("protocol.rs")),
        ("process_group.rs", include_str!("process_group.rs")),
        ("trace.rs", include_str!("trace.rs")),
        ("native_ptc.rs", include_str!("native_ptc.rs")),
        ("native_skills.rs", include_str!("native_skills.rs")),
        ("native_runtime.rs", include_str!("native_runtime.rs")),
    ] {
        uncoded.extend(uncoded_error_literals(name, source));
    }
    assert!(uncoded.is_empty(), "uncoded reader errors: {uncoded:#?}");
    // The scanner itself recognizes both forms of an uncoded message.
    assert_eq!(
        uncoded_error_literals(
            "fixture",
            r#"ensure!(a == "b", "Some text"); anyhow!("host_ok"); bail!("host_{x}", x)"#
        )
        .len(),
        2
    );
}
