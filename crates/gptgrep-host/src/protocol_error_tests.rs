use super::*;

fn notification(retry: Value, info: Value) -> Value {
    json!({"method":"error","params":{
        "threadId":"thread-native","turnId":"turn-native","willRetry":retry,
        "error":{"message":"private-error-sentinel","additionalDetails":"private-details-sentinel",
            "source":"private-source-sentinel","codexErrorInfo":info}
    }})
}
fn completed(status: &str) -> Value {
    json!({"method":"turn/completed","params":{
        "threadId":"thread-native","turn":{"id":"turn-native","status":status,
            "items":[{"type":"agentMessage","phase":"final_answer","text":"{}"}]}
    }})
}
fn usage() -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":"thread-native","turnId":"turn-native",
        "tokenUsage":{"total":{"totalTokens":42,"inputTokens":37,"outputTokens":5,
            "source":"private-usage-sentinel"},"last":{"outputTokens":5},
            "modelContextWindow":1024,"additionalDetails":"private-usage-sentinel"}
    }})
}

async fn complete_with(
    messages: Vec<Value>,
    trace_path: Option<&Path>,
) -> Result<protocol::Outcome> {
    let cwd = tempfile::tempdir().unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake_mode(&mut reader, &mut server_writer, false).await;
        for message in messages {
            send(&mut server_writer, message).await;
        }
        let mut extra = String::new();
        assert_eq!(
            reader.read_line(&mut extra).await.unwrap(),
            0,
            "The client issued an independent retry or new turn: {extra}"
        );
    });
    let state = json!({});
    let schema = json!({});
    let result = protocol::run(
        BufReader::new(reader),
        writer,
        cwd.path(),
        &HostConfig::default(),
        protocol::Workflow::Completion {
            instructions: "Return an empty object.",
            state: &state,
            schema: &schema,
        },
        crate::trace::Trace::open(trace_path).unwrap(),
    )
    .await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn transient_error_continues_the_same_completion_without_client_retry() {
    let outcome = complete_with(
        vec![
            notification(
                json!(true),
                json!({"responseStreamDisconnected":{"httpStatusCode":502}}),
            ),
            notification(
                json!(true),
                json!({"responseStreamConnectionFailed":{"httpStatusCode":null}}),
            ),
            completed("completed"),
        ],
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.thread_id, "thread-native");
    assert_eq!(outcome.turn_id, "turn-native");
    assert_eq!(outcome.answer, "{}");
    assert_eq!(outcome.server_retry_notifications, 2);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("2 transient retry notifications"))
    );
    assert!(
        !outcome
            .warnings
            .join(" ")
            .contains("private-error-sentinel")
    );
}

#[tokio::test]
async fn terminal_error_keeps_only_typed_metadata_and_observed_partial_usage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let error = complete_with(
        vec![
            usage(),
            notification(
                json!(true),
                json!({"responseStreamDisconnected":{"httpStatusCode":502}}),
            ),
            notification(
                json!(false),
                json!({"responseTooManyFailedAttempts":{"httpStatusCode":503}}),
            ),
        ],
        Some(&path),
    )
    .await
    .err()
    .unwrap();
    let typed = error.downcast_ref::<HostProtocolError>().unwrap();
    assert_eq!(typed.kind, HostProtocolErrorKind::TerminalError);
    assert_eq!(
        typed.codex_error_info,
        Some(CodexErrorInfo::ResponseTooManyFailedAttempts)
    );
    assert_eq!(typed.will_retry, Some(false));
    assert_eq!(typed.http_status_code, Some(503));
    assert_eq!(typed.server_retry_notifications, 1);
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["totalTokens"], 42);
    assert!(!typed.accounting_complete);
    let trace = std::fs::read_to_string(&path).unwrap();
    let entries: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let retry = entries
        .iter()
        .find(|entry| entry["will_retry"] == true)
        .unwrap();
    assert_eq!(retry["codex_error_info"], "responseStreamDisconnected");
    assert_eq!(retry["http_status_code"], 502);
    assert!(
        entries
            .iter()
            .any(|entry| entry["will_retry"] == false && entry["http_status_code"] == 503)
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["direction"] == "send" && entry["method"] == "turn/start")
            .count(),
        1
    );
    for output in [
        trace,
        serde_json::to_string(typed).unwrap(),
        error.to_string(),
        format!("{error:?}"),
    ] {
        for sentinel in [
            "private-error-sentinel",
            "private-details-sentinel",
            "private-source-sentinel",
            "private-usage-sentinel",
        ] {
            assert!(!output.contains(sentinel), "Leaked {sentinel}");
        }
    }
}

#[tokio::test]
async fn malformed_retry_flags_and_error_shapes_fail_closed() {
    let mut messages = vec![];
    for flag in [Value::Null, json!("true"), json!(1), json!({})] {
        messages.push(notification(flag, json!("internalServerError")));
    }
    let mut missing = notification(json!(true), json!("internalServerError"));
    missing["params"]
        .as_object_mut()
        .unwrap()
        .remove("willRetry");
    messages.push(missing);
    let mut unexpected_id = notification(json!(true), json!("internalServerError"));
    unexpected_id["id"] = json!("unexpected-request");
    messages.push(unexpected_id);
    let mut invalid_error = notification(json!(true), json!("internalServerError"));
    invalid_error["params"]["error"] = json!({"message":7});
    messages.push(invalid_error);
    let mut missing_error = notification(json!(true), Value::Null);
    missing_error["params"]
        .as_object_mut()
        .unwrap()
        .remove("error");
    messages.push(missing_error);
    for info in [
        json!("private-unknown-variant"),
        json!({"unauthorized":{}}),
        json!({"responseStreamDisconnected":{"httpStatusCode":"503"}}),
        json!({"responseStreamDisconnected":{"httpStatusCode":600}}),
        json!({"responseStreamDisconnected":{"httpStatusCode":-1}}),
    ] {
        messages.push(notification(json!(true), info));
    }
    for message in messages {
        let error = complete_with(vec![message], None).await.err().unwrap();
        let typed = error.downcast_ref::<HostProtocolError>().unwrap();
        assert_eq!(typed.kind, HostProtocolErrorKind::MalformedError);
        assert_eq!(typed.server_retry_notifications, 0);
        assert!(typed.usage.is_none());
        assert!(
            !serde_json::to_string(typed)
                .unwrap()
                .contains("private-unknown-variant")
        );
    }
}

#[tokio::test]
async fn error_and_completion_identities_cannot_change_or_disappear() {
    for (key, value) in [
        ("threadId", json!("foreign-thread")),
        ("turnId", json!("foreign-turn")),
        ("threadId", Value::Null),
        ("turnId", Value::Null),
        ("turnId", json!("")),
    ] {
        let mut message = notification(json!(true), json!("serverOverloaded"));
        message["params"][key] = value;
        let error = complete_with(vec![message], None).await.err().unwrap();
        let typed = error.downcast_ref::<HostProtocolError>().unwrap();
        assert_eq!(typed.kind, HostProtocolErrorKind::IdentityMismatch);
        assert_eq!(typed.server_retry_notifications, 0);
    }
    let mut message = completed("failed");
    message["params"]["turn"]["id"] = json!("foreign-turn");
    let error = complete_with(vec![message], None).await.err().unwrap();
    assert_eq!(
        error.downcast_ref::<HostProtocolError>().unwrap().kind,
        HostProtocolErrorKind::IdentityMismatch
    );
}

#[tokio::test]
async fn failed_turn_completion_uses_the_same_safe_error_projection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let mut message = completed("failed");
    message["params"]["turn"]["error"] = notification(
        json!(false),
        json!({"httpConnectionFailed":{"httpStatusCode":429}}),
    )["params"]["error"]
        .clone();
    let plausible_answer = json!({"method":"item/completed","params":{
        "threadId":"thread-native","turnId":"turn-native",
        "item":{"type":"agentMessage","phase":"final_answer","text":"{}"}
    }});
    let error = complete_with(
        vec![
            usage(),
            notification(
                json!(true),
                json!({"responseStreamDisconnected":{"httpStatusCode":502}}),
            ),
            plausible_answer,
            message,
        ],
        Some(&path),
    )
    .await
    .err()
    .unwrap();
    let typed = error.downcast_ref::<HostProtocolError>().unwrap();
    assert_eq!(typed.kind, HostProtocolErrorKind::FailedTurn);
    assert_eq!(
        typed.codex_error_info,
        Some(CodexErrorInfo::HttpConnectionFailed)
    );
    assert_eq!(typed.http_status_code, Some(429));
    assert_eq!(typed.will_retry, None);
    assert_eq!(typed.server_retry_notifications, 1);
    assert_eq!(typed.usage.as_ref().unwrap()["last"]["outputTokens"], 5);
    let trace = std::fs::read_to_string(path).unwrap();
    assert!(trace.contains("httpConnectionFailed"));
    assert!(!trace.contains("private-error-sentinel"));
    let error = complete_with(vec![completed("interrupted")], None)
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.downcast_ref::<HostProtocolError>().unwrap().kind,
        HostProtocolErrorKind::InterruptedTurn
    );
    let mut contradictory = completed("completed");
    contradictory["params"]["turn"]["error"] =
        json!({"message":"private-error-sentinel","codexErrorInfo":"unauthorized"});
    let error = complete_with(vec![contradictory], None)
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.downcast_ref::<HostProtocolError>().unwrap().kind,
        HostProtocolErrorKind::InvalidTurnStatus
    );
}

#[cfg(unix)]
#[tokio::test]
async fn retry_notifications_cannot_extend_the_real_process_deadline() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("mock-runtime");
    let trace_path = directory.path().join("trace.jsonl");
    std::fs::write(&launcher, r#"#!/bin/sh
printf '%s' "$$" > "$CODEX_HOME/owned.pid"
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
printf '%s' "$$" > "$CODEX_HOME/turn-ready.pid"
while :; do
printf '%s\n' '{"method":"error","params":{"threadId":"thread-native","turnId":"turn-native","willRetry":true,"error":{"message":"private-transient-sentinel","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502}}}}}'
sleep 0.05
done
"#).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = HostConfig {
        codex_bin: launcher.to_string_lossy().into_owned(),
        codex_home: directory.path().to_owned(),
        timeout_secs: 5,
        trace_path: Some(trace_path.clone()),
        ..HostConfig::default()
    };
    let ready_pid_file = directory.path().join("turn-ready.pid");
    let started = Instant::now();
    let (result, ready) = tokio::join!(
        complete_json("Return an empty object.", json!({}), json!({}), &config),
        wait_for_live_mock_pid(&ready_pid_file),
    );
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("time limit") || error.to_string() == "host_deadline_exceeded",
        "{error}"
    );
    let ready_pid = ready.expect("mock runtime did not reach turn/start before the startup limit");
    assert!(started.elapsed() < Duration::from_secs(10));
    let raw = std::fs::read_to_string(trace_path).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(rows.iter().filter(|row| row["will_retry"] == true).count() >= 2);
    assert_eq!(
        rows.iter()
            .filter(|row| row["direction"] == "send" && row["method"] == "turn/start")
            .count(),
        1
    );
    assert!(!raw.contains("private-transient-sentinel"));
    let pid = std::fs::read_to_string(directory.path().join("owned.pid")).unwrap();
    assert_eq!(pid, ready_pid);
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .output()
        .unwrap();
    assert!(
        !status.status.success(),
        "Owned runtime survived the original deadline"
    );
}

#[tokio::test]
async fn repeated_server_retries_remain_bounded_by_the_original_deadline() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let directory = tempfile::tempdir().unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let sent = Arc::new(AtomicUsize::new(0));
    let count = sent.clone();
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake_mode(&mut reader, &mut server_writer, false).await;
        loop {
            let message = notification(
                json!(true),
                json!({"responseStreamDisconnected":{"httpStatusCode":502}}),
            );
            if server_writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
            count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let state = json!({});
    let schema = json!({});
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(80);
    let result = tokio::time::timeout_at(
        deadline,
        protocol::run(
            BufReader::new(reader),
            writer,
            directory.path(),
            &HostConfig::default(),
            protocol::Workflow::Completion {
                instructions: "Return an empty object.",
                state: &state,
                schema: &schema,
            },
            None,
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "Retry processing ended or reset the enclosing deadline"
    );
    assert!(sent.load(Ordering::SeqCst) >= 2);
    assert!(started.elapsed() < Duration::from_secs(2));
    server.await.unwrap();
}

#[test]
fn trace_rejects_unlisted_error_variants_and_invalid_http_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let mut trace = crate::trace::Trace::open(Some(&path)).unwrap().unwrap();
    for info in [
        json!("private-unlisted-variant"),
        json!({"private-unlisted-variant":{"httpStatusCode":503}}),
        json!({"responseStreamDisconnected":{"httpStatusCode":"private-http-sentinel"}}),
        json!({"responseStreamDisconnected":{"httpStatusCode":700}}),
    ] {
        trace
            .record(
                "receive",
                &notification(json!("private-flag-sentinel"), info),
            )
            .unwrap();
    }
    drop(trace);
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(!raw.contains("private-"));
    for line in raw.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["http_status_code"], Value::Null);
        assert_eq!(row["will_retry"], Value::Null);
    }
}
