#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn failed_completion_keeps_typed_metadata_without_server_error_text() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let input = root.join("input.json");
    fs::write(
        &input,
        r#"{"instructions":"Return an empty object.","state":{},"schema":{"type":"object"}}"#,
    )
    .unwrap();
    let launcher = root.join("mock-codex.sh");
    // This is an offline protocol fixture. Each response follows one actual CLI
    // request; no model, account, document or network is involved.
    // Keep the fixture alive after EOF until owned-group termination settles
    // cleanup, so exited-leader verification cannot mask the protocol assertion.
    fs::write(
        &launcher,
        r#"#!/bin/sh
trap '' TERM
read -r request
printf '%s\n' '{"id":1,"result":{}}'
read -r notification
read -r request
printf '%s\n' '{"id":100,"result":{"account":{"type":"chatgpt"}}}'
read -r request
printf '%s\n' '{"id":2,"result":{"config":{"mcp_servers":{}}}}'
read -r request
printf '%s\n' '{"id":3,"result":{"thread":{"id":"offline-thread"},"model":"gpt-6-luna","modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority","approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}'
read -r request
printf '%s\n' '{"id":4,"result":{"turn":{"id":"offline-turn"}}}'
printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"offline-thread","turnId":"offline-turn","tokenUsage":{"total":{"inputTokens":37,"outputTokens":5,"totalTokens":42},"last":{"inputTokens":37,"outputTokens":5,"totalTokens":42}}}}'
printf '%s\n' '{"method":"error","params":{"threadId":"offline-thread","turnId":"offline-turn","willRetry":true,"error":{"message":"private-error-sentinel","additionalDetails":"private-details-sentinel","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502}}}}}'
printf '%s\n' '{"method":"error","params":{"threadId":"offline-thread","turnId":"offline-turn","willRetry":false,"error":{"message":"private-error-sentinel","additionalDetails":"private-details-sentinel","codexErrorInfo":{"responseTooManyFailedAttempts":{"httpStatusCode":503}}}}}'
read -r request
exec /bin/sleep 30
"#,
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let trace = root.join("trace.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["host-complete", "--input"])
        .arg(&input)
        .arg("--codex-bin")
        .arg(&launcher)
        .arg("--codex-home")
        .arg(root)
        .arg("--protocol-trace")
        .arg(&trace)
        .args(["--timeout", "5", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["code"], "host_codex_terminal_error");
    assert_eq!(report["ok"], false);
    assert_eq!(report["host_protocol"]["kind"], "terminal_error");
    assert_eq!(
        report["host_protocol"]["codex_error_info"],
        "responseTooManyFailedAttempts"
    );
    assert_eq!(report["host_protocol"]["will_retry"], false);
    assert_eq!(report["host_protocol"]["http_status_code"], 503);
    assert_eq!(report["host_protocol"]["server_retry_notifications"], 1);
    assert_eq!(report["host_protocol"]["usage"]["total"]["totalTokens"], 42);
    assert_eq!(report["host_protocol"]["accounting_complete"], false);
    assert!(report.get("value").is_none());
    for content in [
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
        fs::read_to_string(trace).unwrap(),
    ] {
        assert!(!content.contains("private-error-sentinel"));
        assert!(!content.contains("private-details-sentinel"));
    }
}
