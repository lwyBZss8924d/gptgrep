use super::*;
use crate::{jev_accounting::Accounting, model_attempts::ModelAttemptTracker};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn accounting_fixture(limit: usize) -> (tempfile::TempDir, Accounting) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".gptgrep")).unwrap();
    let accounting = Accounting::create(directory.path(), "synthetic-generation").unwrap();
    accounting.bind_workflow("Original question", None).unwrap();
    accounting.set_model_attempt_limit(limit).unwrap();
    (directory, accounting)
}

fn usage() -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":"thread-native","turnId":"turn-native","tokenUsage":{
            "total":{"inputTokens":7,"outputTokens":3,"totalTokens":10,
                "cachedInputTokens":"private-token-sentinel","unlisted":"private-token-sentinel"},
            "last":{"inputTokens":5,"outputTokens":2,"totalTokens":7},
            "private":"private-token-sentinel"}}})
}

fn completed(text: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread-native",
        "turn":{"id":"turn-native","status":"completed","items":[
            {"type":"agentMessage","phase":"final_answer","text":text}]}}})
}

fn retry() -> Value {
    json!({"method":"error","params":{"threadId":"thread-native","turnId":"turn-native",
        "willRetry":true,"error":{"message":"private-provider-error",
        "codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502}}}}})
}

async fn protocol_attempt(
    messages: Vec<Value>,
    output_limit: Option<usize>,
    expect_denial: bool,
) -> (Result<protocol::Outcome>, Vec<ModelAttempt>, String) {
    let (directory, accounting) = accounting_fixture(1);
    let config = HostConfig::default();
    let mut attempt = ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
    attempt.observer().running().unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (reader_server, mut writer_server) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(reader_server);
        for (expected, response) in [
            ("initialize", Some(json!({"id":1,"result":{}}))),
            ("initialized", None),
            (
                "account/read",
                Some(json!({"id":100,"result":{"account":{"type":"chatgpt"}}})),
            ),
            ("config/read", Some(json!({"id":2,"result":{"config":{}}}))),
            (
                "thread/start",
                Some(json!({"id":3,"result":{
                "thread":{"id":"thread-native"},"model":DEFAULT_MODEL,"modelProvider":"openai",
                "reasoningEffort":"max","serviceTier":"priority","approvalPolicy":"never",
                "sandbox":{"type":"readOnly","networkAccess":false}}})),
            ),
            (
                "turn/start",
                Some(json!({"id":4,"result":{"turn":{"id":"turn-native"}}})),
            ),
        ] {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let message: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(message["method"], expected);
            if expected == "thread/start" {
                assert_eq!(message["params"]["dynamicTools"], json!([]));
            }
            if let Some(response) = response {
                writer_server
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
        for message in messages {
            writer_server
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        if expect_denial {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let denial: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(denial["error"]["code"], -32601);
        }
    });
    let state = json!({});
    let schema = json!({});
    let result = protocol::run_observed(
        BufReader::new(reader),
        writer,
        directory.path(),
        &config,
        protocol::Workflow::Completion {
            instructions: "Return JSON.",
            state: &state,
            schema: &schema,
        },
        None,
        protocol::RunOptions {
            observer: Some(attempt.observer()),
            max_output_bytes: output_limit,
            ..Default::default()
        },
    )
    .await;
    attempt.finish(result.as_ref().err()).unwrap();
    server.await.unwrap();
    let raw = std::fs::read_to_string(accounting.path()).unwrap();
    (result, accounting.model_attempts(), raw)
}

#[tokio::test]
async fn foreign_usage_cannot_replace_trusted_observed_identity_or_tokens() {
    for field in ["threadId", "turnId"] {
        let mut foreign = usage();
        foreign["params"][field] = json!("foreign-identity");
        foreign["params"]["tokenUsage"]["total"]["inputTokens"] = json!(99999);
        let (result, attempts, raw) = protocol_attempt(vec![usage(), foreign], None, false).await;
        assert!(result.is_err());
        let attempt = &attempts[0];
        assert_eq!(attempt.status, "failed");
        assert_eq!(attempt.thread_id.as_deref(), Some("thread-native"));
        assert_eq!(attempt.turn_id.as_deref(), Some("turn-native"));
        assert_eq!(attempt.usage.as_ref().unwrap()["total"]["inputTokens"], 7);
        assert_eq!(attempt.model.as_deref(), Some(DEFAULT_MODEL));
        assert!(!raw.contains("foreign-identity"));
        assert!(!raw.contains("99999"));
        assert!(!raw.contains("private-token-sentinel"));
    }
}

#[tokio::test]
async fn planner_tool_call_is_denied_and_preserves_prior_usage() {
    let tool = json!({"id":9,"method":"item/tool/call","params":{
        "threadId":"thread-native","turnId":"turn-native","callId":"call-1",
        "namespace":"gptgrep","tool":"gptgrep_search","arguments":{"query":"x"}}});
    let (result, attempts, _) = protocol_attempt(vec![usage(), tool], None, true).await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("Pure completion")
    );
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(
        attempts[0].usage.as_ref().unwrap()["total"]["totalTokens"],
        10
    );
    assert!(!attempts[0].accounting_complete);
}

#[tokio::test]
async fn planner_raw_text_is_capped_before_json_parsing() {
    let text = format!("{}{{}}", " ".repeat(query_plan::MAX_PLAN_OUTPUT_BYTES));
    let (result, attempts, _) = protocol_attempt(
        vec![usage(), completed(&text)],
        Some(query_plan::MAX_PLAN_OUTPUT_BYTES),
        false,
    )
    .await;
    assert_eq!(
        result.err().unwrap().downcast_ref::<CompletionError>(),
        Some(&CompletionError::OutputLimit)
    );
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(
        attempts[0].error.as_ref().unwrap()["code"],
        "host_output_limit"
    );
}

#[test]
fn usage_totals_keep_unknown_fields_and_deduplicate_observed_turns() {
    let (_directory, accounting) = accounting_fixture(2);
    let config = HostConfig::default();
    let mut planner = ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
    planner
        .observer()
        .thread("t1", DEFAULT_MODEL, "openai", Some("max"), Some("priority"))
        .unwrap();
    planner.observer().turn("u1").unwrap();
    planner
        .observer()
        .usage(Some(
            &json!({"total":{"inputTokens":12,"outputTokens":4,"totalTokens":16},
        "last":{"inputTokens":9,"outputTokens":2,"totalTokens":11}}),
        ))
        .unwrap();
    // A later partial event must not erase observed input/output totals.
    planner
        .observer()
        .usage(Some(&json!({"total":{"totalTokens":16}})))
        .unwrap();
    planner.finish(None).unwrap();
    let mut reader = ModelAttemptTracker::reserve(&accounting, "final_reader", &config).unwrap();
    reader
        .observer()
        .thread("t2", DEFAULT_MODEL, "openai", None, None)
        .unwrap();
    reader.observer().turn("u2").unwrap();
    reader
        .observer()
        .usage(Some(&json!({"total":{"inputTokens":8}})))
        .unwrap();
    reader.finish(None).unwrap();
    let mut attempts = accounting.model_attempts();
    let summary = model_attempts::summarize(&attempts);
    assert_eq!(summary.observed_turns, 2);
    assert_eq!(summary.known_totals.input_tokens, Some(20));
    assert_eq!(summary.known_totals.output_tokens, Some(4));
    assert_eq!(summary.known_totals.cached_input_tokens, None);
    assert_eq!(summary.known_totals.total_tokens, Some(16));
    assert_eq!(summary.missing_totals.output_tokens, 1);
    assert_eq!(summary.missing_totals.cached_input_tokens, 2);
    assert!(!summary.accounting_complete);
    let mut duplicate = attempts[0].clone();
    duplicate.attempt_id = "duplicate-observation".into();
    duplicate.usage = None;
    attempts.push(duplicate);
    let summary = model_attempts::summarize(&attempts);
    assert_eq!(summary.attempted_calls, 3);
    assert_eq!(summary.observed_turns, 2);
    assert_eq!(summary.duplicate_turn_observations, 1);
    assert_eq!(summary.known_totals.input_tokens, Some(20));
    assert!(!summary.accounting_complete);
}

#[test]
fn dropping_reserved_or_running_attempts_retains_explicit_incomplete_records() {
    let (_directory, accounting) = accounting_fixture(2);
    {
        let planner =
            ModelAttemptTracker::reserve(&accounting, "query_planner", &HostConfig::default())
                .unwrap();
        planner
            .observer()
            .thread("thread", DEFAULT_MODEL, "openai", None, None)
            .unwrap();
        planner.observer().turn("turn").unwrap();
        planner
            .observer()
            .usage(Some(&json!({"total":{"inputTokens":4}})))
            .unwrap();
    }
    let attempts = accounting.model_attempts();
    assert_eq!(
        attempts.len(),
        1,
        "Unused reader budget must not fabricate an attempt"
    );
    assert_eq!(attempts[0].status, "interrupted");
    assert_eq!(
        attempts[0].usage.as_ref().unwrap()["total"]["inputTokens"],
        4
    );
    assert!(!attempts[0].accounting_complete);
    let raw = std::fs::read_to_string(accounting.path()).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let reserved = rows
        .iter()
        .position(|row| row["event"] == "model_attempt_reserved")
        .unwrap();
    assert!(
        rows[..reserved]
            .iter()
            .any(|row| row["event"] == "model_attempt_budget")
    );
    assert!(
        rows[..reserved]
            .iter()
            .any(|row| row["event"] == "workflow_bound")
    );
    assert_eq!(
        rows.last().unwrap()["model_attempts"][0]["status"],
        "interrupted"
    );
}

#[tokio::test]
async fn query_plan_is_rejected_before_nonask_operations_or_invalid_timeout() {
    let config = HostConfig {
        query_plan: Some(QueryPlanConfig::default()),
        ..HostConfig::default()
    };
    assert_eq!(
        complete_json("Return JSON.", json!({}), json!({}), &config)
            .await
            .unwrap_err()
            .to_string(),
        "host_query_plan_requires_ask"
    );
    assert_eq!(
        summarize(Path::new("/unavailable-fixture"), "node", &config)
            .await
            .unwrap_err()
            .to_string(),
        "host_query_plan_requires_ask"
    );
    for timeout in [0, 46, 901] {
        let config = HostConfig {
            query_plan: Some(QueryPlanConfig {
                planner_timeout_secs: timeout,
                ..QueryPlanConfig::default()
            }),
            ..HostConfig::default()
        };
        assert!(validate_config(&config, "question").is_err());
    }
}

#[test]
fn planner_model_override_is_bound_independently_from_reader() {
    let reader = HostConfig {
        model: "gpt-5.6-luna".into(),
        max_input_bytes: 64 * 1024,
        query_plan: Some(QueryPlanConfig {
            planner_model: "gpt-6-luna".into(),
            ..QueryPlanConfig::default()
        }),
        ..HostConfig::default()
    };
    let selected = planner_runtime_config(&reader, reader.query_plan.as_ref().unwrap());
    assert_eq!(reader.model, "gpt-5.6-luna");
    assert_eq!(selected.model, "gpt-6-luna");
    assert_eq!(selected.reasoning_effort, "max");
    assert_eq!(selected.service_tier, "fast");
    assert_eq!(selected.max_input_bytes, query_plan::MAX_PLAN_INPUT_BYTES);
    assert!(selected.query_plan.is_none());
    let control = QueryPlanConfig {
        planner_model: "gpt-5.6-luna".into(),
        ..QueryPlanConfig::default()
    };
    assert_eq!(
        planner_runtime_config(&reader, &control).model,
        "gpt-5.6-luna"
    );
    assert_eq!(QueryPlanConfig::default().planner_model, "gpt-6-luna");
    assert!(
        QueryPlanConfig {
            planner_model: " gpt-6-luna".into(),
            ..QueryPlanConfig::default()
        }
        .validate()
        .is_err()
    );
}

#[cfg(unix)]
fn mock_process(directory: &Path, answer: Option<&str>) -> HostConfig {
    use std::os::unix::fs::PermissionsExt;
    let launcher = directory.join("mock-lifecycle-runtime");
    let mut script = String::from("#!/bin/sh\nprintf '%s' \"$$\" > \"$CODEX_HOME/owned.pid\"\n");
    for response in [
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{}}}),
        json!({"id":3,"result":{"thread":{"id":"thread-native"},"model":DEFAULT_MODEL,
            "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
            "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}),
        json!({"id":4,"result":{"turn":{"id":"turn-native"}}}),
    ] {
        script.push_str("read -r request\n");
        if response["id"] == 100 {
            script.push_str("read -r request\n"); // initialized notification precedes account/read
        }
        script.push_str(&format!("printf '%s\\n' '{}'\n", response));
    }
    for event in [usage(), retry()] {
        script.push_str(&format!("printf '%s\\n' '{}'\n", event));
    }
    if let Some(answer) = answer {
        // Fixture text contains no shell quote; serialize before embedding into the mock.
        let final_event = completed(answer).to_string();
        assert!(!final_event.contains('\''));
        script.push_str(&format!("printf '%s\\n' '{final_event}'\n"));
    }
    script.push_str("while :; do sleep 1; done\n");
    std::fs::write(&launcher, script).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    HostConfig {
        codex_bin: launcher.to_string_lossy().into_owned(),
        codex_home: directory.into(),
        timeout_secs: 30,
        ..HostConfig::default()
    }
}

#[cfg(unix)]
fn assert_owned_process_stopped(directory: &Path) {
    let pid = std::fs::read_to_string(directory.join("owned.pid")).unwrap();
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .output()
        .unwrap();
    assert!(!status.status.success(), "Owned process survived: {pid}");
}

#[cfg(unix)]
#[tokio::test]
async fn schema_failure_keeps_process_metadata_usage_and_retry_events() {
    let (directory, accounting) = accounting_fixture(2);
    let config = mock_process(directory.path(), Some(r#"{"alternate_queries":"invalid"}"#));
    let mut planner = ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
    let result = completion::complete_json_until(
        "Return a query plan.",
        json!({}),
        json!({"type":"object","properties":{
            "alternate_queries":{"type":"array"}},"required":["alternate_queries"]}),
        &config,
        Some(tokio::time::Instant::now() + Duration::from_secs(10)),
        protocol::RunOptions {
            observer: Some(planner.observer()),
            max_output_bytes: Some(4096),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        result
            .as_ref()
            .unwrap_err()
            .downcast_ref::<CompletionError>(),
        Some(&CompletionError::InvalidOutput)
    );
    planner.finish(result.as_ref().err()).unwrap();
    let attempts = accounting.model_attempts();
    assert_eq!(attempts.len(), 1);
    let attempt = &attempts[0];
    assert_eq!(attempt.status, "failed");
    assert_eq!(attempt.model.as_deref(), Some(DEFAULT_MODEL));
    assert_eq!(attempt.model_provider.as_deref(), Some("openai"));
    assert_eq!(attempt.effective_reasoning_effort.as_deref(), Some("max"));
    assert_eq!(attempt.effective_service_tier.as_deref(), Some("priority"));
    assert_eq!(attempt.thread_id.as_deref(), Some("thread-native"));
    assert_eq!(attempt.turn_id.as_deref(), Some("turn-native"));
    assert_eq!(attempt.usage.as_ref().unwrap()["total"]["totalTokens"], 10);
    assert_eq!(attempt.server_retry_notifications, 1);
    assert!(attempt.elapsed_ms > 0);
    let summary = model_attempts::summarize(&attempts);
    assert_eq!(summary.known_totals.total_tokens, Some(10));
    assert!(!summary.accounting_complete);
    let raw = std::fs::read_to_string(accounting.path()).unwrap();
    assert!(!raw.contains("private-"));
    assert_owned_process_stopped(directory.path());
}

#[cfg(unix)]
#[tokio::test]
async fn supplied_absolute_deadline_is_not_refreshed_by_completion_or_retry() {
    let (directory, accounting) = accounting_fixture(1);
    let config = mock_process(directory.path(), None);
    let mut planner = ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(2);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let result = completion::complete_json_until(
        "Return JSON.",
        json!({}),
        json!({}),
        &config,
        Some(deadline),
        protocol::RunOptions {
            observer: Some(planner.observer()),
            max_output_bytes: Some(4096),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        result.as_ref().unwrap_err().to_string(),
        "host_model_attempt_timeout"
    );
    planner.finish(result.as_ref().err()).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let attempts = accounting.model_attempts();
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(attempts[0].server_retry_notifications, 1);
    assert_eq!(
        attempts[0].usage.as_ref().unwrap()["total"]["inputTokens"],
        7
    );
    assert_owned_process_stopped(directory.path());
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_completion_retains_observed_attempt_and_stops_owned_process() {
    let (directory, accounting) = accounting_fixture(2);
    let config = mock_process(directory.path(), None);
    let mut future = Box::pin(async {
        let mut planner =
            ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
        let result = completion::complete_json_until(
            "Return JSON.",
            json!({}),
            json!({}),
            &config,
            Some(tokio::time::Instant::now() + Duration::from_secs(10)),
            protocol::RunOptions {
                observer: Some(planner.observer()),
                max_output_bytes: Some(4096),
                ..Default::default()
            },
        )
        .await;
        planner.finish(result.as_ref().err()).unwrap();
        result
    });
    let observed = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if accounting
                    .model_attempts()
                    .first()
                    .is_some_and(|attempt| attempt.server_retry_notifications == 1)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    };
    tokio::select! {
        result = &mut future => panic!("Mock completed unexpectedly: {result:?}"),
        _ = observed => {},
    }
    drop(future);
    let attempts = accounting.model_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "interrupted");
    assert_eq!(
        attempts[0].usage.as_ref().unwrap()["total"]["inputTokens"],
        7
    );
    assert_eq!(attempts[0].thread_id.as_deref(), Some("thread-native"));
    assert_eq!(attempts[0].server_retry_notifications, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_owned_process_stopped(directory.path());
}

#[test]
fn model_budget_rejects_duplicate_reservations_and_a_third_turn() {
    let (_directory, accounting) = accounting_fixture(2);
    let config = HostConfig::default();
    let _planner = ModelAttemptTracker::reserve(&accounting, "query_planner", &config).unwrap();
    let duplicate = ModelAttemptTracker::reserve(&accounting, "query_planner", &config)
        .err()
        .unwrap();
    assert_eq!(duplicate.to_string(), "host_model_attempt_duplicate");
    assert_eq!(accounting.model_attempts().len(), 1);
    let _reader = ModelAttemptTracker::reserve(&accounting, "final_reader", &config).unwrap();
    let third = ModelAttemptTracker::reserve(&accounting, "query_planner", &config)
        .err()
        .unwrap();
    assert_eq!(third.to_string(), "host_model_attempt_limit");
    assert_eq!(accounting.model_attempts().len(), 2);
    let (_directory, default_accounting) = accounting_fixture(1);
    let _reader =
        ModelAttemptTracker::reserve(&default_accounting, "final_reader", &config).unwrap();
    assert_eq!(
        ModelAttemptTracker::reserve(&default_accounting, "query_planner", &config)
            .err()
            .unwrap()
            .to_string(),
        "host_model_attempt_limit"
    );
}

#[tokio::test]
async fn terminal_model_error_retains_only_sanitized_failure_and_trusted_usage() {
    let mut terminal = retry();
    terminal["params"]["willRetry"] = json!(false);
    let (result, attempts, raw) = protocol_attempt(vec![usage(), terminal], None, false).await;
    assert_eq!(
        result
            .err()
            .unwrap()
            .downcast_ref::<HostProtocolError>()
            .unwrap()
            .kind,
        HostProtocolErrorKind::TerminalError
    );
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(
        attempts[0].error.as_ref().unwrap()["protocol"]["http_status_code"],
        502
    );
    assert_eq!(
        attempts[0].usage.as_ref().unwrap()["total"]["inputTokens"],
        7
    );
    assert!(!raw.contains("private-"));
}

#[cfg(unix)]
fn two_process_mock(directory: &Path, reader_answer: &Value) -> HostConfig {
    two_process_mock_with_budget(directory, reader_answer, None)
}

#[cfg(unix)]
fn two_process_mock_with_budget(
    directory: &Path,
    reader_answer: &Value,
    ending: Option<&str>,
) -> HostConfig {
    use std::os::unix::fs::PermissionsExt;
    let launcher = directory.join("mock-two-stage-runtime");
    let mut script = String::from("#!/bin/sh\nif [ -f \"$CODEX_HOME/planner.started\" ]; then\n");
    for (role, output, input_tokens, output_tokens) in [
        ("reader", reader_answer.clone(), 19, 5),
        (
            "planner",
            json!({"alternate_queries":["thermal archive", "ember containment"]}),
            11,
            3,
        ),
    ] {
        if role == "planner" {
            script.push_str("else\nprintf '%s' 'started' > \"$CODEX_HOME/planner.started\"\n");
        }
        script.push_str(&format!(
            "printf '%s\\n' '{role}' >> \"$CODEX_HOME/process.starts\"\n"
        ));
        script.push_str(&format!(
            "printf '%s' \"$$\" > \"$CODEX_HOME/{role}.pid\"\n"
        ));
        let thread_id = format!("thread-{role}");
        let turn_id = format!("turn-{role}");
        for (request_name, response) in [
            ("initialize", json!({"id":1,"result":{}})),
            (
                "account",
                json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
            ),
            ("config", json!({"id":2,"result":{"config":{}}})),
            (
                "thread",
                json!({"id":3,"result":{"thread":{"id":thread_id},"model":if role == "planner" { "gpt-6-luna" } else { DEFAULT_MODEL },
                "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
                "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}),
            ),
            ("turn", json!({"id":4,"result":{"turn":{"id":turn_id}}})),
        ] {
            script.push_str("read -r request\n");
            if request_name == "account" {
                script.push_str("read -r request\n");
            }
            if matches!(request_name, "thread" | "turn") {
                script.push_str(&format!(
                    "printf '%s\\n' \"$request\" > \"$CODEX_HOME/{role}.{request_name}.json\"\n"
                ));
            }
            script.push_str(&format!("printf '%s\\n' '{response}'\n"));
        }
        if role == "reader"
            && let Some(ending) = ending
        {
            let emit = |script: &mut String, event: Value| {
                let event = event.to_string();
                assert!(!event.contains('\''));
                script.push_str(&format!("printf '%s\\n' '{event}'\n"));
            };
            emit(
                &mut script,
                json!({"method":"thread/tokenUsage/updated","params":{
                "threadId":thread_id,"turnId":turn_id,"tokenUsage":{"total":{"inputTokens":19,"outputTokens":5,"totalTokens":24}}}}),
            );
            for (id, tool, arguments) in [
                (10, "gptgrep_catalog", json!({})),
                (
                    11,
                    "gptgrep_search",
                    json!({"query":"invented refinement","mode":"hybrid"}),
                ),
            ] {
                emit(
                    &mut script,
                    json!({"id":id,"method":"item/tool/call","params":{
                    "threadId":thread_id,"turnId":turn_id,"callId":format!("budget-{id}"),
                    "namespace":"gptgrep","tool":tool,"arguments":arguments}}),
                );
                script.push_str("read -r response\n");
                if id == 11 {
                    script.push_str(
                        "printf '%s\\n' \"$response\" > \"$CODEX_HOME/reader.denial.json\"\n",
                    );
                }
            }
            emit(
                &mut script,
                json!({"method":"thread/tokenUsage/updated","params":{
                "threadId":thread_id,"turnId":turn_id,"tokenUsage":{"total":{"inputTokens":31,"outputTokens":9,"totalTokens":40}}}}),
            );
            match ending {
                "completed" => emit(
                    &mut script,
                    json!({"method":"turn/completed","params":{"threadId":thread_id,
                    "turn":{"id":turn_id,"status":"completed","items":[{"type":"agentMessage","phase":"final_answer","text":output.to_string()}]}}}),
                ),
                "failed" => {
                    emit(
                        &mut script,
                        json!({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,
                        "item":{"type":"agentMessage","phase":"final_answer","text":output.to_string()}}}),
                    );
                    emit(
                        &mut script,
                        json!({"method":"turn/completed","params":{"threadId":thread_id,
                        "turn":{"id":turn_id,"status":"failed","error":{"codexErrorInfo":"rateLimitExceeded"}}}}),
                    );
                }
                "foreign" => emit(
                    &mut script,
                    json!({"method":"thread/tokenUsage/updated","params":{
                    "threadId":thread_id,"turnId":"foreign-budget-turn","tokenUsage":{"total":{"totalTokens":99999}}}}),
                ),
                "stall" => {}
                _ => panic!("unknown synthetic ending"),
            }
            script.push_str("while :; do sleep 1; done\n");
            continue;
        }
        for event in [
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":thread_id,"turnId":turn_id,
                "tokenUsage":{"total":{"inputTokens":input_tokens,"outputTokens":output_tokens,
                    "totalTokens":input_tokens+output_tokens}}}}),
            json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":turn_id,
                "status":"completed","items":[{"type":"agentMessage","phase":"final_answer",
                    "text":output.to_string()}]}}}),
        ] {
            let event = event.to_string();
            assert!(!event.contains('\''));
            script.push_str(&format!("printf '%s\\n' '{event}'\n"));
        }
        script.push_str("while :; do sleep 1; done\n");
    }
    script.push_str("fi\n");
    std::fs::write(&launcher, script).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    HostConfig {
        codex_bin: launcher.to_string_lossy().into_owned(),
        codex_home: directory.into(),
        query_plan: Some(QueryPlanConfig::default()),
        trace_path: Some(directory.join("reader.trace.jsonl")),
        timeout_secs: 15,
        ..HostConfig::default()
    }
}

#[cfg(unix)]
async fn planner_corpus() -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("notebook.txt"),
        "Ember storage uses a ceramic chamber.\n",
    )
    .unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let tree = gptgrep_core::tree(root.path(), Path::new("notebook.txt")).unwrap();
    let citation = format!(
        "{}:{}",
        tree["document"]["id"].as_str().unwrap(),
        tree["document"]["nodes"][0]["id"].as_str().unwrap()
    );
    (root, citation)
}

#[cfg(unix)]
fn assert_two_processes_stopped(home: &Path) {
    assert_eq!(
        std::fs::read_to_string(home.join("process.starts")).unwrap(),
        "planner\nreader\n"
    );
    let mut pids = vec![];
    for role in ["planner", "reader"] {
        let pid = std::fs::read_to_string(home.join(format!("{role}.pid"))).unwrap();
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap();
        assert!(!status.status.success(), "Owned {role} process survived");
        pids.push(pid);
    }
    assert_ne!(pids[0], pids[1]);
}

#[cfg(unix)]
#[tokio::test]
async fn planned_ask_runs_two_distinct_turns_with_original_question_and_summed_usage() {
    let (root, citation) = planner_corpus().await;
    let home = tempfile::tempdir().unwrap();
    let config = two_process_mock(
        home.path(),
        &json!({"answer":"It uses a ceramic chamber.",
        "citations":[citation],"insufficient_evidence":false}),
    );
    let (client, jev_server) = crate::query_plan::tests::mock_jev(vec![200; 4]).await;
    let question = "  Explain ember storage.\n";
    let report = execute_with_client(root.path(), question, None, &config, Some(client))
        .await
        .unwrap();
    assert_eq!(jev_server.await.unwrap().len(), 4);
    assert_eq!(report.status, "completed");
    assert_eq!(report.query_plan.as_ref().unwrap().status, "completed");
    assert_eq!(
        report.query_plan.as_ref().unwrap().alternate_queries.len(),
        2
    );
    assert_eq!(report.model_attempts.len(), 2);
    assert_eq!(report.model_attempts[0].role, "query_planner");
    assert_eq!(report.model_attempts[1].role, "final_reader");
    assert!(
        report
            .model_attempts
            .iter()
            .all(|attempt| attempt.status == "completed")
    );
    assert_ne!(
        report.model_attempts[0].turn_id,
        report.model_attempts[1].turn_id
    );
    assert_eq!(report.model_usage.attempted_calls, 2);
    assert_eq!(report.model_usage.observed_turns, 2);
    assert_eq!(report.model_usage.known_totals.input_tokens, Some(30));
    assert_eq!(report.model_usage.known_totals.output_tokens, Some(8));
    assert_eq!(report.model_usage.known_totals.total_tokens, Some(38));
    assert!(report.model_usage.accounting_complete);
    assert_eq!(report.usage.as_ref().unwrap()["total"]["totalTokens"], 24);
    assert_eq!(report.usage_scope, "final_reader");
    assert_eq!(report.thread_id, "thread-reader");
    assert_eq!(report.turn_id, "turn-reader");
    assert_eq!(report.citations[0].node_id, citation);
    let mut legacy = serde_json::to_value(&report).unwrap();
    for field in ["usage_scope", "query_plan", "model_attempts", "model_usage"] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let legacy: HostReport = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.usage_scope, "final_reader");
    assert!(legacy.query_plan.is_none());
    assert!(legacy.model_attempts.is_empty());
    assert_eq!(legacy.model_usage.known_totals.total_tokens, None);
    assert!(!legacy.model_usage.accounting_complete);
    for role in ["planner", "reader"] {
        let turn: Value = serde_json::from_slice(
            &std::fs::read(home.path().join(format!("{role}.turn.json"))).unwrap(),
        )
        .unwrap();
        let state: Value =
            serde_json::from_str(turn["params"]["input"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(state["question"], question);
        let thread: Value = serde_json::from_slice(
            &std::fs::read(home.path().join(format!("{role}.thread.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            thread["params"]["model"],
            if role == "planner" {
                "gpt-6-luna"
            } else {
                DEFAULT_MODEL
            }
        );
        assert_eq!(thread["params"]["serviceTier"], "fast");
        if role == "planner" {
            assert_eq!(thread["params"]["dynamicTools"], json!([]));
        } else {
            assert!(
                !thread["params"]["dynamicTools"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }
    let trace_files = std::fs::read_dir(home.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("reader.trace.jsonl")
        })
        .count();
    assert_eq!(
        trace_files, 2,
        "Planner and reader must not collide at create_new trace paths"
    );
    assert_two_processes_stopped(home.path());
}

#[cfg(unix)]
#[tokio::test]
async fn reader_validation_failures_preserve_completed_planner_and_both_turn_usages() {
    for answer in [
        json!({"answer":12,"citations":[],"insufficient_evidence":false}),
        json!({"answer":"An unsupported claim.","citations":["never-issued"],"insufficient_evidence":false}),
    ] {
        let (root, _) = planner_corpus().await;
        let home = tempfile::tempdir().unwrap();
        let config = two_process_mock(home.path(), &answer);
        let (client, jev_server) = crate::query_plan::tests::mock_jev(vec![200; 4]).await;
        let error = execute_with_client(
            root.path(),
            "Explain ember storage.",
            None,
            &config,
            Some(client),
        )
        .await
        .unwrap_err();
        assert_eq!(jev_server.await.unwrap().len(), 4);
        let error = error.downcast_ref::<HostRetrievalError>().unwrap();
        assert_eq!(error.stage, "citation_validation");
        assert_eq!(error.query_plan.as_ref().unwrap().status, "completed");
        assert_eq!(error.model_attempts.len(), 2);
        assert_eq!(error.model_attempts[0].status, "completed");
        assert_eq!(error.model_attempts[1].status, "failed");
        assert_eq!(error.model_usage.known_totals.total_tokens, Some(38));
        assert_eq!(error.model_usage.observed_turns, 2);
        assert!(!error.model_usage.accounting_complete);
        assert_eq!(error.usage_scope, "final_reader");
        let rows: Vec<Value> = std::fs::read_to_string(&error.ledger_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows.last().unwrap()["model_attempts"][0]["status"],
            "completed"
        );
        assert_eq!(
            rows.last().unwrap()["model_attempts"][1]["status"],
            "failed"
        );
        assert_two_processes_stopped(home.path());
    }
}

/// Original deadline for integrated budget runs that must reach a stalled
/// reader. The planner turn, four durably accounted Jev calls and both process
/// startups take about 1.5 s on an idle host and under 7 s inside a fully
/// loaded parallel suite, so only the deliberately stalled reader reaches it.
#[cfg(unix)]
const INTEGRATED_DEADLINE_SECS: u64 = 30;

/// Original deadline for the integrated run that only a timeout can end. Its
/// last Decisions reply is held until [`STALLED_READER_WINDOW`] before this
/// deadline. That leaves the planner turn and the earlier Decisions calls
/// (under 7 s even in a loaded parallel suite) 15 s and keeps the hold below
/// the 20 s Decisions request timeout.
#[cfg(unix)]
const STALLED_READER_DEADLINE_SECS: u64 = 20;

/// The stalled reader starts with at most this much of the original deadline
/// left, on any host. Its startup, handshake and both tool calls need well under
/// that, while a reader deadline refreshed at its start, or set more than 8 s
/// after its start, would end the stall after the 3 s finalization allowance.
#[cfg(unix)]
const STALLED_READER_WINDOW: Duration = Duration::from_secs(5);

#[cfg(unix)]
fn budget_answer(citation: &str) -> Value {
    json!({"answer":"It uses a ceramic chamber.","citations":[citation],"insufficient_evidence":false})
}

#[cfg(unix)]
fn assert_budget_denied_without_new_turn(home: &Path) {
    let denial: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("reader.denial.json")).unwrap())
            .unwrap();
    assert_eq!(denial["result"]["success"], false);
    let body: Value = serde_json::from_str(
        denial["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"], "tool_budget_exhausted");
    let mut starts = 0;
    for file in std::fs::read_dir(home).unwrap().flatten() {
        if file.file_name().to_string_lossy().contains("trace.jsonl") {
            for line in std::fs::read_to_string(file.path()).unwrap().lines() {
                let event: Value = serde_json::from_str(line).unwrap();
                starts +=
                    usize::from(event["direction"] == "send" && event["method"] == "turn/start");
            }
        }
    }
    assert_eq!(starts, 2, "Planner and reader each start one turn");
    assert_two_processes_stopped(home);
}

#[cfg(unix)]
#[tokio::test]
async fn integrated_budget_finalization_counts_latest_reader_usage_once_and_preserves_coverage() {
    let (root, citation) = planner_corpus().await;
    let home = tempfile::tempdir().unwrap();
    let mut config =
        two_process_mock_with_budget(home.path(), &budget_answer(&citation), Some("completed"));
    config.max_tool_calls = 1;
    let (client, server) = crate::query_plan::tests::mock_jev(vec![200; 4]).await;
    let report = execute_with_client(
        root.path(),
        "Explain ember storage.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap();
    assert_eq!(
        server.await.unwrap().len(),
        4,
        "Denied search must not invoke Jev"
    );
    assert_eq!(
        (
            report.model_usage.attempted_calls,
            report.model_usage.observed_turns
        ),
        (2, 2)
    );
    assert_eq!(report.model_usage.known_totals.total_tokens, Some(54));
    assert_eq!(report.usage.as_ref().unwrap()["total"]["totalTokens"], 40);
    assert_eq!(
        report.model_attempts[1].turn_id.as_deref(),
        Some("turn-reader")
    );
    assert_eq!(
        report.model_attempts[1].usage.as_ref().unwrap()["total"]["totalTokens"],
        40
    );
    assert_eq!(report.jev.requests, 4);
    assert!(
        report
            .tool_calls
            .iter()
            .filter(|receipt| receipt.tool_budget.is_some())
            .all(|receipt| !receipt.success
                && receipt.evidence.is_empty()
                && receipt.search.is_none())
    );
    for item in &report.citations {
        let replay = gptgrep_core::read_node_window(
            root.path(),
            &item.node_id,
            item.byte_end - item.byte_start,
            item.node_offset,
        )
        .unwrap();
        assert_eq!(item.node_coverage, replay.node_coverage);
        assert!(!item.node_coverage.unwrap().complete);
        assert_eq!(item.node_coverage.unwrap().unread_after_bytes, 1);
        assert_eq!(item.excerpt_sha256, retrieval::hash(replay.text.as_bytes()));
    }
    assert_budget_denied_without_new_turn(home.path());
}

#[cfg(unix)]
#[tokio::test]
async fn integrated_budget_failures_keep_identity_usage_and_original_deadline() {
    for ending in ["failed", "foreign", "stall"] {
        let (root, citation) = planner_corpus().await;
        let home = tempfile::tempdir().unwrap();
        let mut config =
            two_process_mock_with_budget(home.path(), &budget_answer(&citation), Some(ending));
        config.max_tool_calls = 1;
        config.timeout_secs = INTEGRATED_DEADLINE_SECS;
        let started = Instant::now();
        let (client, server) = if ending == "stall" {
            // The reader starts only after the held last Decisions reply, i.e. with
            // at most STALLED_READER_WINDOW of the original deadline left.
            config.timeout_secs = STALLED_READER_DEADLINE_SECS;
            let release = tokio::time::Instant::from_std(started)
                + Duration::from_secs(config.timeout_secs)
                - STALLED_READER_WINDOW;
            crate::query_plan::tests::mock_jev_with_last_reply_at(vec![200; 4], release).await
        } else {
            crate::query_plan::tests::mock_jev(vec![200; 4]).await
        };
        let error = execute_with_client(
            root.path(),
            "Explain ember storage.",
            None,
            &config,
            Some(client),
        )
        .await
        .unwrap_err();
        assert_eq!(server.await.unwrap().len(), 4);
        let failure = error.downcast_ref::<HostRetrievalError>().unwrap();
        assert_eq!(failure.model_attempts.len(), 2);
        assert_eq!(failure.model_attempts[0].status, "completed");
        assert_eq!(failure.model_attempts[1].status, "failed");
        assert_eq!(
            failure.model_attempts[1].turn_id.as_deref(),
            Some("turn-reader")
        );
        assert_eq!(failure.model_usage.known_totals.total_tokens, Some(54));
        assert!(!failure.model_usage.accounting_complete);
        assert!(
            failure
                .receipts
                .iter()
                .any(|receipt| receipt.tool_budget.is_some())
        );
        let ledger = std::fs::read_to_string(&failure.ledger_path).unwrap();
        assert!(!ledger.contains("foreign-budget-turn") && !ledger.contains("99999"));
        if ending == "failed" {
            assert_eq!(
                failure.cause.as_ref().unwrap()["tool_budget"]["denied_tool_calls"],
                1
            );
        }
        if ending == "stall" {
            // Only the original deadline ends the stalled reader; finalization keeps
            // the same three-second allowance beyond it (formerly 2 s + 3 s = 5 s).
            let deadline = Duration::from_secs(config.timeout_secs);
            assert_eq!(failure.code, "host_model_attempt_timeout");
            assert!(started.elapsed() >= deadline);
            assert!(
                started.elapsed() < deadline + Duration::from_secs(3),
                "Finalization refreshed the original deadline"
            );
        }
        assert_budget_denied_without_new_turn(home.path());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn integrated_budget_cancellation_retains_bound_partial_observations() {
    let (root, citation) = planner_corpus().await;
    let home = tempfile::tempdir().unwrap();
    let mut config =
        two_process_mock_with_budget(home.path(), &budget_answer(&citation), Some("stall"));
    config.max_tool_calls = 1;
    config.timeout_secs = INTEGRATED_DEADLINE_SECS;
    let (client, server) = crate::query_plan::tests::mock_jev(vec![200; 4]).await;
    let mut running = Box::pin(execute_with_client(
        root.path(),
        "Explain ember storage.",
        None,
        &config,
        Some(client),
    ));
    // The attempt ledger is appended synchronously while the invocation is
    // polled, so checking the newly appended lines after every poll observes the
    // reader's usage as soon as it is durable. Only a run that never records it
    // reaches the original deadline and fails below.
    let attempts = root.path().join(".gptgrep/host-attempts");
    let mut scanned = std::collections::BTreeMap::<PathBuf, usize>::new();
    let ledger_path = std::future::poll_fn(|context| {
        if let std::task::Poll::Ready(result) = running.as_mut().poll(context) {
            panic!("Expected pending finalization: {result:?}");
        }
        for file in std::fs::read_dir(&attempts).into_iter().flatten().flatten() {
            let raw = std::fs::read_to_string(file.path()).unwrap();
            let seen = scanned.entry(file.path()).or_default();
            for line in raw.lines().skip(*seen) {
                *seen += 1;
                let event: Value = serde_json::from_str(line).unwrap();
                if event["model_attempts"][1]["usage"]["total"]["totalTokens"] == 40 {
                    return std::task::Poll::Ready(file.path());
                }
            }
        }
        std::task::Poll::Pending
    })
    .await;
    drop(running);
    assert_eq!(server.await.unwrap().len(), 4);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let raw = std::fs::read_to_string(ledger_path).unwrap();
    let last: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "interrupted");
    assert_eq!(last["model_attempts"][1]["status"], "interrupted");
    assert_eq!(
        last["model_attempts"][1]["usage"]["total"]["totalTokens"],
        40
    );
    assert_eq!(last["model_usage"]["known_totals"]["total_tokens"], 54);
    assert!(
        !last["workflow"]["query_sha256"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(raw.contains("tool_budget"));
    assert_budget_denied_without_new_turn(home.path());
}

#[cfg(unix)]
#[tokio::test]
async fn evidence_roles_preserve_two_turns_budget_finalization_and_treatment_only_guidance() {
    let (root, citation) = planner_corpus().await;
    let home = tempfile::tempdir().unwrap();
    let mut config =
        two_process_mock_with_budget(home.path(), &budget_answer(&citation), Some("completed"));
    config.query_plan.as_mut().unwrap().evidence_roles = true;
    config.max_tool_calls = 1;
    let (client, server) = crate::evidence_roles::tests::mock_roles(4).await;
    let report = execute_with_client(
        root.path(),
        "Explain ember storage.",
        None,
        &config,
        Some(client),
    )
    .await
    .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        (
            report.model_usage.attempted_calls,
            report.model_usage.observed_turns
        ),
        (2, 2)
    );
    assert_eq!(report.model_usage.known_totals.total_tokens, Some(54));
    assert_eq!(report.jev.requests, 4);
    let roles = report.jev.searches[0]
        .plan
        .as_ref()
        .unwrap()
        .evidence_roles
        .as_ref()
        .unwrap();
    assert_eq!(roles.strategy, "jev-evidence-role-v1");
    assert_eq!(
        roles.delivered_set_sufficiency,
        gptgrep_core::DeliveredSetSufficiency::Unassessed
    );
    let reader: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("reader.thread.json")).unwrap(),
    )
    .unwrap();
    assert!(
        reader["params"]["baseInstructions"]
            .as_str()
            .unwrap()
            .contains(crate::evidence_roles::READER_GUIDANCE)
    );
    let planner: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("planner.thread.json")).unwrap(),
    )
    .unwrap();
    assert!(
        !planner["params"]["baseInstructions"]
            .as_str()
            .unwrap()
            .contains(crate::evidence_roles::READER_GUIDANCE)
    );
    assert_budget_denied_without_new_turn(home.path());
}
