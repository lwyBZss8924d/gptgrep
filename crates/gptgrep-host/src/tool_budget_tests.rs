use super::*;
use tokio::io::AsyncReadExt;

fn request(id: u64, tool: &str, arguments: Value) -> Value {
    json!({"id":id,"method":"item/tool/call","params":{
        "threadId":"thread-native","turnId":"turn-native","callId":format!("call-{id}"),
        "namespace":"gptgrep","tool":tool,"arguments":arguments
    }})
}

fn excess(id: u64) -> Value {
    request(
        id,
        "gptgrep_search",
        json!({"query":"private-query-sentinel","mode":"semantic"}),
    )
}

fn usage() -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":"thread-native","turnId":"turn-native",
        "tokenUsage":{"total":{"inputTokens":37,"outputTokens":5,"totalTokens":42},
            "private":"private-usage-sentinel"}
    }})
}

fn earlier_answer() -> Value {
    json!({"method":"item/completed","params":{
        "threadId":"thread-native","turnId":"turn-native",
        "item":{"type":"agentMessage","phase":"final_answer",
            "text":"{\"answer\":\"Uncertain\",\"citations\":[],\"insufficient_evidence\":true}"}
    }})
}

async fn after_budget(
    limit: usize,
    admitted_arguments: Value,
    messages: Vec<Value>,
) -> (Result<protocol::Outcome>, Evidence, Vec<Value>) {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake(&mut reader, &mut server_writer).await;
        for offset in 0..limit {
            let id = 10 + offset as u64;
            send(
                &mut server_writer,
                request(id, "gptgrep_catalog", admitted_arguments.clone()),
            )
            .await;
            assert_eq!(receive(&mut reader).await["id"], id);
        }
        // Queue requests before reading their responses, as parallel tool calls
        // may arrive before the model has seen any budget-exhausted response.
        for message in messages {
            send(&mut server_writer, message).await;
        }
        let mut remaining = String::new();
        reader.read_to_string(&mut remaining).await.unwrap();
        remaining
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    });
    let config = HostConfig {
        max_tool_calls: limit,
        ..HostConfig::default()
    };
    let result = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "query",
        None,
        &config,
        &mut evidence,
    )
    .await;
    (result, evidence, server.await.unwrap())
}

fn assert_denial(response: &Value, admitted: usize, denied: usize) {
    assert_eq!(response["result"]["success"], false);
    let payload: Value = serde_json::from_str(
        response["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload["error"], "tool_budget_exhausted");
    assert_eq!(payload["tool_budget"]["admitted_tool_calls"], admitted);
    assert_eq!(payload["tool_budget"]["denied_tool_calls"], denied);
    assert_eq!(payload["tool_budget"]["max_denied_tool_calls"], admitted);
    assert!(
        payload["message"]
            .as_str()
            .unwrap()
            .contains("Finalize now")
    );
    assert!(!payload.to_string().contains("private-query-sentinel"));
}

#[tokio::test]
async fn parallel_excess_requests_are_bounded_with_typed_failure_and_usage() {
    let (result, evidence, responses) = after_budget(
        2,
        json!({}),
        vec![
            excess(12),
            excess(13),
            usage(),
            earlier_answer(),
            excess(14),
        ],
    )
    .await;
    let error = result.err().unwrap();
    let typed = error.downcast_ref::<HostProtocolError>().unwrap();
    assert_eq!(typed.kind, HostProtocolErrorKind::ToolBudgetExhausted);
    assert_eq!(typed.code(), "host_tool_budget_exhausted");
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["totalTokens"], 42);
    assert!(!typed.accounting_complete);
    let budget = typed.tool_budget.unwrap();
    assert_eq!(budget.admitted_tool_calls, 2);
    assert_eq!(budget.max_tool_calls, 2);
    assert_eq!(budget.denied_tool_calls, 2);
    assert_eq!(
        responses.len(),
        2,
        "Unexpected retry, new turn or tool response"
    );
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(response["id"], 12 + index);
        assert_denial(response, 2, index + 1);
    }
    assert_eq!(evidence.receipts.len(), 4);
    assert!(evidence.receipts[..2].iter().all(|receipt| receipt.success));
    for receipt in &evidence.receipts[2..] {
        assert!(!receipt.success);
        assert!(receipt.evidence.is_empty());
        assert!(receipt.search.is_none());
        assert!(receipt.tool_budget.is_some());
    }
    for sanitized in [
        serde_json::to_string(typed).unwrap(),
        serde_json::to_string(
            &evidence
                .receipts
                .iter()
                .map(ReceiptSummary::from)
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        format!("{error:?}"),
    ] {
        assert!(!sanitized.contains("private-"));
    }
}

#[tokio::test]
async fn recoverable_argument_errors_still_consume_the_admitted_call_budget() {
    let (result, evidence, responses) =
        after_budget(1, json!({"limit":999}), vec![excess(11), excess(12)]).await;
    assert_eq!(
        result
            .err()
            .unwrap()
            .downcast_ref::<HostProtocolError>()
            .unwrap()
            .kind,
        HostProtocolErrorKind::ToolBudgetExhausted
    );
    assert_eq!(evidence.receipts.len(), 2);
    assert!(!evidence.receipts[0].success);
    assert!(evidence.receipts[0].tool_budget.is_none());
    assert_denial(&responses[0], 1, 1);
}

#[tokio::test]
async fn malformed_and_unsupported_excess_requests_remain_fatal() {
    let mut invalid = vec![];
    for (key, value) in [
        ("threadId", json!("foreign-thread")),
        ("turnId", json!("foreign-turn")),
        ("callId", json!("call-10")),
        ("callId", Value::Null),
        ("namespace", json!("functions")),
        ("tool", json!("exec")),
        ("tool", Value::Null),
    ] {
        let mut message = excess(11);
        message["params"][key] = value;
        invalid.push(message);
    }
    for arguments in [
        Value::Null,
        json!("private-arguments-sentinel"),
        json!([]),
        json!({}),
        json!({"query":7}),
        json!({"query":""}),
        json!({"query":"test","extra":"private-arguments-sentinel"}),
        json!({"query":"test","mode":"unsupported"}),
        json!({"query":"test","limit":1.0}),
        json!({"query":"test","limit":4}),
        json!({"query":"é".repeat(1025)}),
        json!({"query":"x".repeat(4096)}),
    ] {
        invalid.push(request(11, "gptgrep_search", arguments));
    }
    for message in invalid {
        let (result, evidence, responses) = after_budget(1, json!({}), vec![message]).await;
        let error = result
            .err()
            .expect("Malformed request received a recoverable denial");
        assert!(error.downcast_ref::<HostProtocolError>().is_none());
        assert!(!format!("{error:?}").contains("private-"));
        assert_eq!(evidence.receipts.len(), 1);
        assert!(
            responses
                .iter()
                .all(|response| response.get("error").is_some())
        );
    }
}

#[tokio::test]
async fn failed_turn_after_budget_cannot_promote_an_earlier_answer() {
    let failed = json!({"method":"turn/completed","params":{
        "threadId":"thread-native","turn":{"id":"turn-native","status":"failed",
            "error":{"message":"private-error-sentinel","codexErrorInfo":"contextWindowExceeded"},
            "items":[]}
    }});
    let (result, evidence, responses) = after_budget(
        1,
        json!({}),
        vec![excess(11), usage(), earlier_answer(), failed],
    )
    .await;
    let error = result.err().unwrap();
    let typed = error.downcast_ref::<HostProtocolError>().unwrap();
    assert_eq!(typed.kind, HostProtocolErrorKind::FailedTurn);
    assert_eq!(
        typed.codex_error_info,
        Some(CodexErrorInfo::ContextWindowExceeded)
    );
    assert_eq!(typed.tool_budget.unwrap().denied_tool_calls, 1);
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["totalTokens"], 42);
    assert_eq!(evidence.receipts.len(), 2);
    assert_eq!(responses.len(), 1);
    assert!(!serde_json::to_string(typed).unwrap().contains("private-"));
}

#[tokio::test]
async fn budget_finalization_remains_bounded_by_the_original_transport_deadline() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
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
        send(&mut server_writer, excess(11)).await;
        assert_denial(&receive(&mut reader).await, 1, 1);
        let mut extra = String::new();
        assert_eq!(
            reader.read_line(&mut extra).await.unwrap(),
            0,
            "The host retried or started a new turn during finalization"
        );
    });
    let config = HostConfig {
        max_tool_calls: 1,
        ..HostConfig::default()
    };
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout_at(
        started + Duration::from_millis(150),
        protocol::drive(
            BufReader::new(reader),
            writer,
            root.path(),
            "query",
            None,
            &config,
            &mut evidence,
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "Finalization exited before the unchanged deadline"
    );
    server.await.unwrap();
    assert_eq!(evidence.receipts.len(), 2);
    assert_eq!(
        evidence.receipts[1].tool_budget.unwrap().denied_tool_calls,
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn budget_finalization_keeps_the_original_process_deadline() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("mock-runtime");
    let trace = directory.path().join("trace.jsonl");
    std::fs::write(&launcher, r#"#!/bin/sh
read -r request
printf '%s\n' '{"id":1,"result":{}}'
read -r notification
read -r request
printf '%s\n' '{"id":100,"result":{"account":{"type":"chatgpt"}}}'
read -r request
printf '%s\n' '{"id":2,"result":{"config":{}}}'
read -r request
printf '%s\n' '{"id":3,"result":{"thread":{"id":"thread-native"},"model":"gpt-6-luna","modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority","approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}'
read -r request
printf '%s\n' '{"id":4,"result":{"turn":{"id":"turn-native"}}}'
printf '%s\n' '{"id":10,"method":"item/tool/call","params":{"threadId":"thread-native","turnId":"turn-native","callId":"call-10","namespace":"gptgrep","tool":"gptgrep_catalog","arguments":{}}}'
read -r response
printf '%s\n' '{"id":11,"method":"item/tool/call","params":{"threadId":"thread-native","turnId":"turn-native","callId":"call-11","namespace":"gptgrep","tool":"gptgrep_search","arguments":{"query":"synthetic query","mode":"hybrid"}}}'
read -r response
printf '%s' "$$" > "$CODEX_HOME/denial-received.pid"
sleep 30
"#).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = HostConfig {
        codex_bin: launcher.to_string_lossy().into_owned(),
        codex_home: directory.path().to_owned(),
        max_tool_calls: 1,
        timeout_secs: 30,
        trace_path: Some(trace.clone()),
        ..HostConfig::default()
    };
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_process_until(
            &config,
            protocol::Workflow::Retrieval {
                question: "query",
                node_id: None,
                evidence: &mut evidence,
            },
            Some(started + Duration::from_secs(2)),
            protocol::RunOptions::default(),
        ),
    )
    .await
    .expect("The original deadline was extended during finalization");
    let error = result.err().unwrap();
    assert!(error.to_string().contains("time limit"), "{error}");
    assert_eq!(evidence.receipts.len(), 2);
    assert_eq!(
        evidence.receipts[1].tool_budget.unwrap().denied_tool_calls,
        1
    );
    let pid = std::fs::read_to_string(directory.path().join("denial-received.pid")).unwrap();
    assert!(
        !std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap()
            .status
            .success()
    );
    let rows: Vec<Value> = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|row| row["direction"] == "send" && row["method"] == "turn/start")
            .count(),
        1
    );
}
