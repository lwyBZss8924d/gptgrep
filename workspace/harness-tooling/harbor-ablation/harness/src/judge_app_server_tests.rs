//! Generic self-authored protocol/child fixtures; no Codex, provider or original clock.
use super::super::super::tests::Fixture;
use super::*;

const RAW: &str = r#"{"equivalent":false,"abstained":false,"reason":""}"#;
#[test]
fn static_failure_diagnostic_never_copies_unknown_text_or_error_chains() -> Result<()> {
    for message in [
        "app_rpc_error: mock-private-provider-text".to_owned(),
        "mock-private-home/credential/MOCK_SECRET".to_owned(),
        "{\"error\":\"app_rpc_error\",\"private\":\"MOCK_SECRET\"}".to_owned(),
        "app_not_a_defined_internal_code".to_owned(),
        "app_rpc_error\n".to_owned(),
        "x".repeat(65_536),
    ] {
        let error = anyhow::anyhow!(message);
        let code = static_failure_code(&error);
        assert_eq!(code, "unknown_internal_failure");
        assert_eq!(
            serde_json::to_string(&code)?,
            "\"unknown_internal_failure\""
        );
    }
    let error = anyhow::anyhow!("app_rpc_error").context("mock-private-outer-context");
    assert_eq!(static_failure_code(&error), "unknown_internal_failure");
    let error = anyhow::anyhow!("mock-private-inner-provider-text").context("app_rpc_error");
    assert_eq!(static_failure_code(&error), "app_rpc_error");
    for code in STATIC_FAILURE_CODES {
        assert!(code.starts_with("app_") && code.len() <= STATIC_FAILURE_CODE_CAP);
        assert_eq!(static_failure_code(&anyhow::anyhow!(*code)), *code);
    }
    Ok(())
}
fn automatic(method: &str, params: Value) -> Value {
    json!({"method":method,"params":params})
}
#[test]
fn notification_failure_category_does_not_admit_prestarted_events_or_unknown_names() -> Result<()> {
    let sc = schema();
    for (method, expected) in [
        ("error", "error"),
        ("thread/tokenUsage/updated", "thread/tokenUsage/updated"),
        ("item/started", "item/started"),
        ("turn/completed", "turn/completed"),
        ("MOCK-private-unknown-method", "unknown_notification"),
    ] {
        let mut s = Session::new("generic", &sc, "/mock")?;
        s.stage = Stage::Running;
        let error = emit(&mut s, automatic(method, json!({}))).unwrap_err();
        assert_eq!(static_failure_code(&error), "app_turn_not_started");
        assert_eq!(s.protocol.failing_notification_category, Some(expected));
        assert!(!s.started && s.final_text.is_none());
        assert!(s.protocol.thread_usage.is_none() && s.protocol.raw_response_usage.is_none());
        assert!(!serde_json::to_string(&s.protocol)?.contains("MOCK-private"));
    }
    for method in NOTIFICATION_CATEGORIES {
        assert!(method.len() <= STATIC_FAILURE_CODE_CAP);
        assert_eq!(notification_category(Some(method)), *method);
    }
    assert_eq!(notification_category(None), "unknown_notification");
    Ok(())
}
#[test]
fn failed_error_notification_projects_only_closed_code_and_boolean() -> Result<()> {
    let sc = schema();
    for (info, expected) in [
        (json!("unauthorized"), "unauthorized"),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":400}}),
            "httpConnectionFailed",
        ),
        (
            json!({"responseStreamDisconnected":{"httpStatusCode":null}}),
            "responseStreamDisconnected",
        ),
        (
            json!({"responseTooManyFailedAttempts":{}}),
            "responseTooManyFailedAttempts",
        ),
        (
            json!({"responseStreamConnectionFailed":{"httpStatusCode":65535}}),
            "responseStreamConnectionFailed",
        ),
        (json!("MOCK-private-unknown-code"), "unknown_native_error"),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":65536}}),
            "unknown_native_error",
        ),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":true}}),
            "unknown_native_error",
        ),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":400,"MOCK-private-extra":"MOCK-private"}}),
            "unknown_native_error",
        ),
        (Value::Null, "unknown_native_error"),
    ] {
        for retry in [
            json!(true),
            json!(false),
            json!("MOCK-private-invalid-boolean"),
        ] {
            let mut s = Session::new("generic", &sc, "/mock")?;
            s.stage = Stage::Running;
            let event = automatic(
                "error",
                json!({"threadId":"MOCK-private-thread","turnId":"MOCK-private-turn","willRetry":retry,"error":{"message":"MOCK-private-provider-message","additionalDetails":"MOCK-private-details","codexErrorInfo":info}}),
            );
            let error = emit(&mut s, event).unwrap_err();
            assert_eq!(static_failure_code(&error), "app_turn_not_started");
            assert_eq!(s.protocol.failing_notification_category, Some("error"));
            assert_eq!(s.protocol.native_error_category, Some(expected));
            assert_eq!(s.protocol.native_error_will_retry, retry.as_bool());
            let receipt = serde_json::to_string(&s.protocol)?;
            assert!(!receipt.contains("MOCK-private") && !receipt.contains("httpStatusCode"));
            assert!(!s.started && s.final_text.is_none());
        }
    }
    Ok(())
}
#[test]
fn automatic_metadata_uses_one_discard_counter_across_all_phases() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    let account = automatic(
        "account/updated",
        json!({"authMode":"chatgpt","planType":"pro"}),
    );
    let rates = automatic(
        "account/rateLimits/updated",
        json!({"rateLimits":{"primary":{"usedPercent":0,"resetsAt":null},"credits":null}}),
    );
    let remote = automatic(
        "remoteControl/status/changed",
        json!({"installationId":"private-discarded","serverName":"private-discarded","status":"disabled","environmentId":null}),
    );
    for stage in [
        Stage::SendInit,
        Stage::Init,
        Stage::SendThread,
        Stage::Thread,
        Stage::ThreadStarted,
        Stage::SendTurn,
        Stage::Turn,
        Stage::Running,
        Stage::Complete,
    ] {
        s.stage = stage;
        for event in [&account, &rates, &remote] {
            emit(&mut s, event.clone())?;
        }
    }
    assert_eq!(s.metadata.notifications, 27);
    assert!(s.early.is_empty());
    for _ in 0..5 {
        emit(&mut s, account.clone())?;
    }
    assert_eq!(s.protocol.automatic_metadata_notifications, 32);
    assert!(emit(&mut s, account).is_err());
    let receipt = serde_json::to_string(&s.protocol)?;
    assert!(
        !receipt.contains("private-discarded")
            && !receipt.contains("planType")
            && !receipt.contains("usedPercent")
    );
    Ok(())
}
#[test]
fn notification_envelope_accepts_only_optional_i64_timestamp() -> Result<()> {
    let sc = schema();
    for timestamp in [None, Some(i64::MIN), Some(0), Some(i64::MAX)] {
        for stage in [
            Stage::SendInit,
            Stage::Init,
            Stage::SendThread,
            Stage::Thread,
            Stage::ThreadStarted,
            Stage::SendTurn,
            Stage::Turn,
            Stage::Running,
            Stage::Complete,
        ] {
            let mut s = Session::new("generic", &sc, "/mock")?;
            s.stage = stage;
            let mut event = automatic(
                "remoteControl/status/changed",
                json!({"installationId":"private-discarded","serverName":"private-discarded","status":"disabled","environmentId":null}),
            );
            if let Some(timestamp) = timestamp {
                event["emittedAtMs"] = json!(timestamp);
            }
            emit(&mut s, event)?;
            assert_eq!(s.metadata.notifications, 1);
            assert_eq!(s.protocol.automatic_metadata_notifications, 1);
            assert_eq!(s.protocol.outbound_frames.len(), 1);
            assert!(s.early.is_empty());
            assert!(s.thread_id.is_none() && s.turn_id.is_none());
            assert!(s.protocol.thread_usage.is_none() && s.protocol.raw_response_usage.is_none());
            let receipt = serde_json::to_string(&s.protocol)?;
            assert!(!receipt.contains("emittedAtMs") && !receipt.contains("private-discarded"));
        }
    }
    Ok(())
}
#[test]
fn notification_envelope_rejects_invalid_timestamps_before_metadata() -> Result<()> {
    let sc = schema();
    for timestamp in [
        Value::Null,
        json!(true),
        json!(false),
        json!("1"),
        json!(1.0),
        json!(i64::MAX as u64 + 1),
        serde_json::from_str("-9223372036854775809")?,
    ] {
        let mut s = Session::new("generic", &sc, "/mock")?;
        let mut event = automatic("account/updated", json!({"authMode":"chatgpt"}));
        event["emittedAtMs"] = timestamp;
        assert!(emit(&mut s, event).is_err());
        assert_eq!(s.metadata.notifications, 0);
        assert!(s.early.is_empty());
    }
    for raw in [
        br#"{"method":"account/updated","params":{"authMode":"chatgpt"},"emittedAtMs":0,"emittedAtMs":1}"#.as_slice(),
        br#"{"method":"account/updated","params":{"authMode":"chatgpt"},"emittedAtMs":0,"emitted\u0041tMs":1}"#,
    ] {
        let mut s = Session::new("generic", &sc, "/mock")?;
        assert!(s.line(raw, &limits()).is_err());
        assert_eq!(s.metadata.notifications, 0);
    }
    Ok(())
}
#[test]
fn notification_timestamp_does_not_admit_other_fields_or_configuration_warnings() -> Result<()> {
    let sc = schema();
    for (key, value) in [
        ("extra", json!(false)),
        ("jsonrpc", json!("2.0")),
        ("id", Value::Null),
    ] {
        let mut s = Session::new("generic", &sc, "/mock")?;
        let mut event = automatic("account/updated", json!({"authMode":"chatgpt"}));
        event["emittedAtMs"] = json!(0);
        event[key] = value;
        assert!(emit(&mut s, event).is_err());
        assert_eq!(s.metadata.notifications, 0);
    }
    let mut s = Session::new("generic", &sc, "/mock")?;
    let mut warning = automatic(
        "configWarning",
        json!({"summary":"generic warning","details":null}),
    );
    warning["emittedAtMs"] = json!(0);
    assert_eq!(
        emit(&mut s, warning).unwrap_err().to_string(),
        "app_notification_order"
    );
    assert_eq!(s.metadata.notifications, 0);
    let mut s = Session::new("generic", &sc, "/mock")?;
    sent(&mut s);
    assert!(emit(&mut s, json!({"id":1,"result":{},"emittedAtMs":0})).is_err());
    Ok(())
}
#[test]
fn automatic_metadata_invalid_shapes_requests_and_other_methods_stay_refused() -> Result<()> {
    let sc = schema();
    for event in [
        automatic("account/updated", json!({"authMode":null})),
        automatic("account/updated", json!({"authMode":"apiKey"})),
        automatic(
            "account/updated",
            json!({"authMode":"chatgpt","planType":"invented"}),
        ),
        automatic(
            "account/updated",
            json!({"authMode":"chatgpt","extra":true}),
        ),
        automatic("account/rateLimits/updated", json!({"rateLimits":null})),
        automatic(
            "account/rateLimits/updated",
            json!({"rateLimits":{"primary":{"usedPercent":null}}}),
        ),
        automatic(
            "account/rateLimits/updated",
            json!({"rateLimits":{"primary":{"usedPercent":2147483648u64}}}),
        ),
        automatic(
            "account/rateLimits/updated",
            json!({"rateLimits":{"credits":{"hasCredits":1,"unlimited":false}}}),
        ),
        automatic(
            "remoteControl/status/changed",
            json!({"installationId":"x","serverName":"x","status":"enabled"}),
        ),
        automatic(
            "remoteControl/status/changed",
            json!({"installationId":"x","serverName":"x","status":"disabled","environmentId":1}),
        ),
        json!({"id":9,"method":"account/updated","params":{"authMode":"chatgpt"}}),
        automatic("warning", json!({})),
        automatic("model/rerouted", json!({})),
        automatic("unknown", json!({})),
    ] {
        let mut s = Session::new("generic", &sc, "/mock")?;
        assert!(emit(&mut s, event).is_err());
        assert_eq!(s.metadata.notifications, 0);
    }
    Ok(())
}
#[test]
fn automatic_metadata_does_not_change_completed_native_ids_or_absent_usage() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    emit(
        &mut s,
        automatic("account/updated", json!({"authMode":"chatgpt"})),
    )?;
    start(&mut s)?;
    emit(
        &mut s,
        automatic(
            "remoteControl/status/changed",
            json!({"installationId":"discard","serverName":"discard","status":"disabled"}),
        ),
    )?;
    final_items(&mut s)?;
    emit(&mut s, terminal())?;
    emit(
        &mut s,
        automatic("account/rateLimits/updated", json!({"rateLimits":{}})),
    )?;
    assert_eq!(s.thread_id.as_deref(), Some("native-thread"));
    assert_eq!(s.turn_id.as_deref(), Some("native-turn"));
    assert!(s.protocol.thread_usage.is_none() && s.response_id.is_none());
    assert_eq!(s.metadata.notifications, 3);
    Ok(())
}
fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["equivalent","abstained","reason"],"properties":{"equivalent":{"type":"boolean"},"abstained":{"type":"boolean"},"reason":{"type":"string"}}})
}
fn limits() -> Limits {
    Limits {
        stdout_bytes: STDOUT_CAP,
        stderr_bytes: STDERR_CAP,
        event_bytes: EVENT_CAP,
        event_count: EVENT_COUNT_CAP,
        final_bytes: FINAL_CAP,
        receipt_bytes: RECEIPT_CAP,
    }
}
fn emit(s: &mut Session<'_>, v: Value) -> Result<()> {
    s.line(&serde_json::to_vec(&v)?, &limits())
}
fn sent(s: &mut Session<'_>) {
    s.sent = s.outgoing.len();
    s.written();
}
fn start(s: &mut Session<'_>) -> Result<()> {
    sent(s);
    emit(
        s,
        json!({"id":1,"result":{"userAgent":"mock","codexHome":"not-retained","platformFamily":"mock","platformOs":"mock"}}),
    )?;
    sent(s);
    emit(
        s,
        json!({"id":"judge-thread","result":{"thread":{"id":"native-thread"},"model":"gpt-6-luna","modelProvider":"configured-only","serviceTier":"fast"}}),
    )?;
    emit(
        s,
        json!({"method":"thread/started","params":{"thread":{"id":"native-thread"}}}),
    )?;
    sent(s);
    emit(
        s,
        json!({"id":"judge-turn","result":{"turn":{"id":"native-turn","status":"inProgress"}}}),
    )?;
    emit(
        s,
        json!({"method":"turn/started","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"inProgress"}}}),
    )
}
fn item(kind: &str, id: &str, text: &str) -> Value {
    if kind == "agentMessage" {
        json!({"type":kind,"id":id,"text":text,"phase":"final_answer"})
    } else {
        json!({"type":kind,"id":id})
    }
}
fn item_event(method: &str, item: Value) -> Value {
    json!({"method":method,"params":{"threadId":"native-thread","turnId":"native-turn","item":item}})
}
fn final_items(s: &mut Session<'_>) -> Result<()> {
    emit(
        s,
        item_event("item/started", item("agentMessage", "native-item", "")),
    )?;
    emit(
        s,
        item_event("item/completed", item("agentMessage", "native-item", RAW)),
    )
}
fn terminal() -> Value {
    json!({"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","error":null,"items":[],"itemsView":"notLoaded"}}})
}
fn zero() -> Value {
    json!({"totalTokens":0,"inputTokens":0,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0})
}

#[test]
fn builders_preserve_original_prompt_schema_and_fixed_source_fields() -> Result<()> {
    let sc = schema();
    let prompt = "quote\" \n中🙂 $(inert) `inert`";
    let b = turn_frame("native-thread", prompt, &sc)?;
    let v = decode(&b, OUTGOING_MAX)?;
    assert_eq!(v["params"]["input"][0]["text"], prompt);
    assert_eq!(v["params"]["input"][0]["text_elements"], json!([]));
    assert!(v["params"]["input"][0].get("textElements").is_none());
    assert_eq!(v["params"]["outputSchema"], sc);
    assert_eq!(v["params"]["effort"], "max");
    assert_eq!(v["params"]["serviceTier"], "fast");
    assert_eq!(thread_options("/mock")["sandbox"], "read-only");
    assert!(v.get("jsonrpc").is_none());
    Ok(())
}

#[test]
fn actual_frame_digest_binds_escaped_prompt_native_id_and_partial_write_facts() -> Result<()> {
    let sc = schema();
    let prompt = "quoted\" 中🙂 \\ inert";
    let native = "returned-\"\\-id";
    let mut s = Session::new(prompt, &sc, "/mock")?;
    assert!(!s.protocol.outbound_frames[0].full_write_observed);
    s.sent = 3;
    s.written();
    assert_eq!(s.protocol.outbound_frames[0].bytes_written, 3);
    assert!(!s.protocol.outbound_frames[0].full_write_observed);
    sent(&mut s);
    emit(
        &mut s,
        json!({"id":1,"result":{"userAgent":"m","codexHome":"m","platformFamily":"m","platformOs":"m"}}),
    )?;
    s.sent = 1;
    s.written();
    assert_eq!(s.protocol.outbound_frames[1].bytes_written, 1);
    assert_eq!(s.protocol.outbound_frames[2].bytes_written, 0);
    sent(&mut s);
    emit(
        &mut s,
        json!({"id":"judge-thread","result":{"thread":{"id":native},"model":"gpt-6-luna","modelProvider":"m","serviceTier":"priority"}}),
    )?;
    emit(
        &mut s,
        json!({"method":"thread/started","params":{"thread":{"id":native}}}),
    )?;
    let wire = decode(&s.outgoing, OUTGOING_MAX)?;
    assert_eq!(wire["params"]["threadId"], native);
    assert_eq!(wire["params"]["input"][0]["text"], prompt);
    assert_eq!(s.protocol.outbound_frames.len(), 4);
    let turn = &s.protocol.outbound_frames[3];
    assert_eq!(turn.prepared_sha256, digest(&s.outgoing));
    assert_eq!(turn.prepared_bytes, s.outgoing.len());
    assert_eq!(turn.bytes_written, 0);
    assert!(!turn.full_write_observed);
    s.sent = s.outgoing.len() - 1;
    s.written();
    assert!(!s.protocol.outbound_frames[3].full_write_observed);
    sent(&mut s);
    assert!(
        s.protocol
            .outbound_frames
            .iter()
            .all(|f| f.full_write_observed)
    );
    Ok(())
}
#[test]
fn strict_parser_rejects_duplicates_depth_nodes_nonfinite_and_overflow() -> Result<()> {
    for b in [
        br#"{"id":1,"id":1}"#.as_slice(),
        br#"{"params":{"x":1,"x":2}}"#,
        br#"{"x":NaN}"#,
        br#"{"x":1e999}"#,
        br#"{"x":18446744073709551616}"#,
        br#"{"x":1.25}"#,
        br#"{"x":{"$serde_json::private::Number":"1e9999"}}"#,
    ] {
        assert!(decode(b, EVENT_CAP).is_err());
    }
    assert!(
        decode(
            format!("{}0{}", "[".repeat(33), "]".repeat(33)).as_bytes(),
            EVENT_CAP
        )
        .is_err()
    );
    assert!(
        decode(
            format!("[{}]", vec!["0"; 4097].join(",")).as_bytes(),
            EVENT_CAP
        )
        .is_err()
    );
    let u = json!({"totalTokens":18446744073709551615u64,"inputTokens":0,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0});
    assert!(TokenUsage::parse(&u).is_err());
    Ok(())
}
#[test]
fn normal_one_turn_binds_actual_ids_and_unknown_telemetry() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    start(&mut s)?;
    final_items(&mut s)?;
    emit(&mut s, terminal())?;
    assert!(s.stage == Stage::Complete);
    assert_eq!(s.thread_id.as_deref(), Some("native-thread"));
    assert_eq!(s.turn_id.as_deref(), Some("native-turn"));
    assert_eq!(s.final_item.as_deref(), Some("native-item"));
    assert_eq!(s.final_text.as_deref(), Some(RAW));
    assert!(s.response_id.is_none() && s.protocol.thread_usage.is_none());
    assert!(!s.protocol.served_profile_qualified);
    assert!(emit(&mut s,json!({"method":"thread/status/changed","params":{"threadId":"native-thread","status":{"type":"active","activeFlags":[]}}})).is_err());
    Ok(())
}
#[test]
fn bounded_early_events_wait_for_real_turn_response_and_never_reselect() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    sent(&mut s);
    emit(
        &mut s,
        json!({"id":1,"result":{"userAgent":"m","codexHome":"m","platformFamily":"m","platformOs":"m"}}),
    )?;
    sent(&mut s);
    emit(
        &mut s,
        json!({"id":"judge-thread","result":{"thread":{"id":"native-thread"},"model":"gpt-6-luna","modelProvider":"m","serviceTier":"fast"}}),
    )?;
    emit(
        &mut s,
        json!({"method":"thread/started","params":{"thread":{"id":"native-thread"}}}),
    )?;
    sent(&mut s);
    emit(
        &mut s,
        json!({"method":"turn/started","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"inProgress"}}}),
    )?;
    assert!(s.turn_id.is_none());
    assert_eq!(s.early.len(), 1);
    emit(
        &mut s,
        json!({"id":"judge-turn","result":{"turn":{"id":"native-turn","status":"inProgress"}}}),
    )?;
    assert!(s.started && s.early.is_empty());
    assert!(
        emit(
            &mut s,
            json!({"id":"judge-turn","result":{"turn":{"id":"changed","status":"inProgress"}}})
        )
        .is_err()
    );
    assert_eq!(s.turn_id.as_deref(), Some("native-turn"));
    Ok(())
}
#[test]
fn rpc_ids_do_not_coerce_or_replace_native_ids() -> Result<()> {
    for id in [json!("1"), json!(2), json!(1.0), json!(null)] {
        let sc = schema();
        let mut s = Session::new("generic", &sc, "/mock")?;
        sent(&mut s);
        assert!(emit(&mut s, json!({"id":id,"result":{}})).is_err());
        assert!(s.thread_id.is_none());
    }
    Ok(())
}

#[test]
fn fast_request_accepts_source_normalized_priority_ack_and_refuses_disabled_or_wrong_tier()
-> Result<()> {
    for tier in [
        json!("priority"),
        json!("fast"),
        json!(null),
        json!("flex"),
        json!("other"),
    ] {
        let sc = schema();
        let mut s = Session::new("generic", &sc, "/mock")?;
        sent(&mut s);
        emit(
            &mut s,
            json!({"id":1,"result":{"userAgent":"m","codexHome":"m","platformFamily":"m","platformOs":"m"}}),
        )?;
        sent(&mut s);
        let accepted = matches!(tier.as_str(), Some("priority" | "fast"));
        let result = emit(
            &mut s,
            json!({"id":"judge-thread","result":{"thread":{"id":"native-thread"},"model":"gpt-6-luna","modelProvider":"configured-only","serviceTier":tier}}),
        );
        assert_eq!(result.is_ok(), accepted);
        if accepted {
            assert_eq!(
                s.protocol
                    .configured_ack
                    .as_ref()
                    .unwrap()
                    .service_tier
                    .as_deref(),
                tier.as_str()
            );
            assert!(!s.protocol.served_profile_qualified);
        } else {
            assert!(s.thread_id.is_none());
        }
    }
    Ok(())
}
#[test]
fn wrong_lifecycle_tool_server_requests_and_fallback_fail() -> Result<()> {
    for mode in [
        "wrong-turn",
        "duplicate-turn",
        "tool",
        "server-request",
        "rerouted",
        "unknown",
    ] {
        let sc = schema();
        let mut s = Session::new("generic", &sc, "/mock")?;
        start(&mut s)?;
        let v = match mode {
            "wrong-turn" => {
                json!({"method":"item/started","params":{"threadId":"native-thread","turnId":"wrong","item":item("agentMessage","native-item","")}})
            }
            "duplicate-turn" => {
                json!({"method":"turn/started","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"inProgress"}}})
            }
            "tool" => item_event("item/started", item("commandExecution", "tool", "")),
            "server-request" => {
                json!({"id":7,"method":"item/tool/call","params":{"secret":"never-retained"}})
            }
            "rerouted" => {
                json!({"method":"model/rerouted","params":{"fromModel":"gpt-6-luna","toModel":"other"}})
            }
            _ => json!({"method":"unknown","params":{}}),
        };
        assert!(emit(&mut s, v).is_err(), "{mode}");
        assert!(s.final_text.is_none());
    }
    Ok(())
}
#[test]
fn failed_missing_final_async_question_and_duplicate_final_cannot_grade() -> Result<()> {
    for mode in [
        "missing",
        "failed",
        "phase",
        "async",
        "question",
        "duplicate",
    ] {
        let sc = schema();
        let mut s = Session::new("generic", &sc, "/mock")?;
        start(&mut s)?;
        if mode == "missing" {
            assert!(emit(&mut s, terminal()).is_err());
            continue;
        }
        emit(
            &mut s,
            item_event("item/started", item("agentMessage", "native-item", "")),
        )?;
        let mut i = item("agentMessage", "native-item", RAW);
        match mode {
            "phase" => i["phase"] = json!("finalAnswer"),
            "async" => i["delivery"] = json!("async"),
            "question" => i["questions"] = json!([{"title":"inert"}]),
            _ => {}
        }
        if matches!(mode, "phase" | "async" | "question") {
            assert!(emit(&mut s, item_event("item/completed", i)).is_err());
            continue;
        }
        emit(&mut s, item_event("item/completed", i))?;
        if mode == "failed" {
            let mut t = terminal();
            t["params"]["turn"]["status"] = json!("failed");
            assert!(emit(&mut s, t).is_err());
        } else {
            assert!(
                emit(
                    &mut s,
                    item_event("item/completed", item("agentMessage", "native-item", RAW))
                )
                .is_err()
            );
        }
    }
    Ok(())
}
#[test]
fn usage_presence_and_atomic_invalid_projection_preserve_previous() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    start(&mut s)?;
    let usage = json!({"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn","tokenUsage":{"total":zero(),"last":zero(),"modelContextWindow":null}}});
    emit(&mut s, usage.clone())?;
    assert!(
        s.protocol
            .thread_usage
            .as_ref()
            .unwrap()
            .last
            .cache_write_input_tokens
            .is_none()
    );
    let before = serde_json::to_value(&s.protocol.thread_usage)?;
    let mut bad = usage;
    bad["params"]["tokenUsage"]["last"]["outputTokens"] = json!(-1);
    assert!(emit(&mut s, bad).is_err());
    assert_eq!(serde_json::to_value(&s.protocol.thread_usage)?, before);
    let raw = json!({"method":"rawResponse/completed","params":{"threadId":"native-thread","turnId":"native-turn","responseId":"actual-raw-id","usage":null,"usageMetadata":{"amount":"999","metadata":null}}});
    emit(&mut s, raw.clone())?;
    assert_eq!(s.response_id.as_deref(), Some("actual-raw-id"));
    assert!(s.protocol.raw_response_usage.is_none());
    assert!(emit(&mut s, raw).is_err());
    Ok(())
}
#[test]
fn frame_event_early_and_final_limits_refuse_without_success() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    assert!(s.line(&vec![b'x'; EVENT_CAP + 1], &limits()).is_err());
    s.protocol.events_observed = EVENT_COUNT_CAP;
    assert!(emit(&mut s, json!({})).is_err());
    let mut s = Session::new("generic", &sc, "/mock")?;
    start(&mut s)?;
    emit(
        &mut s,
        item_event("item/started", item("agentMessage", "native-item", "")),
    )?;
    let mut l = limits();
    l.final_bytes = 4;
    assert!(
        s.line(
            &serde_json::to_vec(&item_event(
                "item/completed",
                item("agentMessage", "native-item", RAW)
            ))?,
            &l
        )
        .is_err()
    );
    assert!(s.final_text.is_none());
    Ok(())
}
#[test]
fn input_echo_and_reasoning_progress_do_not_supply_an_answer() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    start(&mut s)?;
    let u = json!({"type":"userMessage","id":"user","content":[{"type":"text","text":"generic","text_elements":[]}]});
    emit(&mut s, item_event("item/started", u.clone()))?;
    emit(&mut s, item_event("item/completed", u))?;
    let r = item("reasoning", "reason", "");
    emit(&mut s, item_event("item/started", r.clone()))?;
    emit(
        &mut s,
        json!({"method":"item/reasoning/textDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"reason","delta":"never retained"}}),
    )?;
    emit(&mut s, item_event("item/completed", r))?;
    assert!(s.final_text.is_none());
    assert!(emit(&mut s, terminal()).is_err());
    Ok(())
}

#[test]
fn early_queue_reservation_is_aggregate_and_precedes_copy() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    // Pure transport fixture parked only at the legitimate pending turn response.
    s.stage = Stage::Turn;
    s.sent = s.outgoing.len();
    s.thread_id = Some("native-thread".into());
    let b = serde_json::to_vec(
        &json!({"method":"turn/started","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"inProgress"}}}),
    )?;
    let mut l = limits();
    l.event_bytes = b.len() + 1;
    s.line(&b, &l)?;
    let retained = s.early_bytes;
    assert!(s.line(&b, &l).is_err());
    assert_eq!(s.early_bytes, retained);
    assert_eq!(s.early.len(), 1);
    assert!(s.turn_id.is_none());
    Ok(())
}

#[test]
fn borrowed_stream_pump_drains_stderr_and_eof_without_retaining_diagnostics() -> Result<()> {
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    start(&mut s)?;
    let mut stdout = Vec::new();
    for v in [
        item_event("item/started", item("agentMessage", "native-item", "")),
        item_event("item/completed", item("agentMessage", "native-item", RAW)),
        terminal(),
    ] {
        stdout.extend_from_slice(&frame(&v)?);
    }
    let mut io = Pipes {
        input: None::<Vec<u8>>,
        stdout: std::io::Cursor::new(&stdout),
        stderr: std::io::Cursor::new(b"generic-private-diagnostic"),
    };
    let mut facts = PumpFacts::new();
    pump(
        &mut io,
        &mut s,
        &limits(),
        Instant::now() + Duration::from_secs(1),
        &mut || Ok(Some(true)),
        &mut facts,
        None,
    )?;
    assert!(s.stage == Stage::Complete && facts.stdout_eof && facts.stderr_eof);
    assert_eq!(facts.stdout_bytes, stdout.len());
    assert_eq!(facts.stderr_bytes, 26);
    assert_eq!(
        format!("{:x}", facts.stderr_hash.finalize()),
        digest(b"generic-private-diagnostic")
    );
    Ok(())
}

#[test]
fn expired_original_deadline_and_unsent_response_fail_before_any_write() -> Result<()> {
    struct BlockWrite;
    impl Write for BlockWrite {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sc = schema();
    let mut s = Session::new("generic", &sc, "/mock")?;
    let mut input = Vec::new();
    let mut io = Pipes {
        input: Some(&mut input),
        stdout: std::io::Cursor::new([]),
        stderr: std::io::Cursor::new([]),
    };
    assert!(
        pump(
            &mut io,
            &mut s,
            &limits(),
            Instant::now(),
            &mut || Ok(None),
            &mut PumpFacts::new(),
            None,
        )
        .is_err()
    );
    assert!(input.is_empty());
    let response = frame(&json!({"id":1,"result":{}}))?;
    let mut io = Pipes {
        input: Some(BlockWrite),
        stdout: std::io::Cursor::new(response),
        stderr: std::io::Cursor::new([]),
    };
    assert!(
        pump(
            &mut io,
            &mut s,
            &limits(),
            Instant::now() + Duration::from_secs(1),
            &mut || Ok(None),
            &mut PumpFacts::new(),
            None,
        )
        .is_err()
    );
    assert_eq!(s.sent, 0);
    assert!(s.thread_id.is_none());
    Ok(())
}

const MOCK: &str = r#"
import json,sys,time
mode=sys.argv[1]
def send(x):
    sys.stdout.write(json.dumps(x,separators=(',',':'))+'\n');sys.stdout.flush()
def read():
    x=json.loads(sys.stdin.buffer.readline());assert 'jsonrpc' not in x;return x
init=read();assert init['method']=='initialize'
send({'id':init['id'],'result':{'userAgent':'mock','codexHome':'not-retained','platformFamily':'mock','platformOs':'mock'}})
assert read()['method']=='initialized'
t=read();assert t['params']['approvalPolicy']=='never' and t['params']['sandbox']=='read-only' and t['params']['ephemeral'] is True
send({'id':t['id'],'result':{'thread':{'id':'native-thread'},'model':'gpt-6-luna','modelProvider':'configured-only','serviceTier':'priority'}})
send({'method':'thread/started','params':{'thread':{'id':'native-thread'}}})
t=read();assert t['params']['threadId']=='native-thread' and t['params']['effort']=='max' and t['params']['serviceTier']=='fast'
assert t['params']['input'][0]['text_elements']==[] and t['params']['outputSchema']['additionalProperties'] is False
if mode=='hang':time.sleep(10)
if mode=='flood':sys.stdout.write('x'*8192);sys.stdout.flush();time.sleep(10)
if mode=='server':send({'id':9,'method':'account/chatgptAuthTokens/refresh','params':{'secret':'mock-secret'}});time.sleep(10)
if mode=='error':send({'id':t['id'],'error':{'code':-32001,'message':'mock-secret-home-prompt','data':{'private':'never-retain'}}});sys.exit(1)
if mode=='prestarted':send({'id':t['id'],'result':{'turn':{'id':'native-turn','status':'inProgress'}}});send({'method':'error','params':{'threadId':'native-thread','turnId':'native-turn','willRetry':False,'error':{'message':'mock-secret-native-error','additionalDetails':'mock-secret-details','codexErrorInfo':{'httpConnectionFailed':{'httpStatusCode':400}}}}});sys.exit(1)
send({'method':'turn/started','params':{'threadId':'native-thread','turn':{'id':'native-turn','status':'inProgress'}}})
send({'id':t['id'],'result':{'turn':{'id':'native-turn','status':'inProgress'}}})
i={'type':'agentMessage','id':'native-item','phase':'final_answer','text':''}
send({'method':'item/started','params':{'threadId':'native-thread','turnId':'native-turn','item':i}})
i['text']=json.dumps({'equivalent':False,'abstained':False,'reason':''},separators=(',',':'))
send({'method':'item/completed','params':{'threadId':'native-thread','turnId':'native-turn','item':i}})
send({'method':'turn/completed','params':{'threadId':'native-thread','turn':{'id':'native-turn','status':'completed','error':None,'items':[],'itemsView':'notLoaded'}}})
assert sys.stdin.buffer.read()==b''
sys.stderr.write('mock-private-stderr-never-retain');sys.stderr.flush()
"#;
struct MockFixture {
    f: Fixture,
    path: PathBuf,
    sha: String,
    claims: PathBuf,
    output: PathBuf,
}
impl MockFixture {
    fn new() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new()?;
        let manifest = f.prepare()?;
        let py = f.python()?;
        let claims = f.root.join("claims");
        fs::create_dir(&claims)?;
        fs::set_permissions(&claims, fs::Permissions::from_mode(0o700))?;
        let output = f.root.join("app-output");
        let path = f.root.join("app-control.json");
        let mut c = Control {
            schema_version: "gptgrep.judge.native-control.v1".into(),
            role: "verifier_only".into(),
            invocation_id: "mock-app".into(),
            request: PreparedControl {
                root: f.output.clone(),
                manifest_sha256: manifest,
                python: py.path.clone(),
                python_sha256: py.sha256.clone(),
            },
            runtime: RuntimeControl {
                binary: py.path.clone(),
                binary_sha256: py.sha256.clone(),
                binary_bytes: py.bytes.len() as u64,
                source_revision: "1".repeat(40),
                recipe_sha256: "2".repeat(64),
            },
            environment: Environment {
                path: "/usr/bin:/bin".into(),
                home: f.root.join("unread-home"),
                codex_home: f.root.join("unread-codex"),
                tmpdir: f.root.clone(),
                sqlite_home: f.root.join("unread-sqlite"),
                log_dir: f.root.join("unread-log"),
            },
            phase: PhaseReferences {
                origin_receipt_sha256: "3".repeat(64),
                execution_policy_sha256: "4".repeat(64),
                binding: None,
            },
            invocation_root: claims.clone(),
            output_root: output.clone(),
            limits: limits(),
        };
        let b = json_bytes(&c, CONTROL_MAX)?;
        fs::write(&path, &b)?;
        c.runtime.recipe_sha256 = recipe(&Admission::load(&path, &digest(&b))?)?;
        let b = json_bytes(&c, CONTROL_MAX)?;
        fs::write(&path, &b)?;
        Ok(Self {
            f,
            path,
            sha: digest(&b),
            claims,
            output,
        })
    }
    fn run(&self, mode: &str, wall: Duration) -> Result<AppOutcome> {
        let a = Admission::load(&self.path, &self.sha)?;
        execute(
            &a,
            ExecutionPermit {
                control_sha256: self.sha.clone(),
                deadline: Instant::now() + wall,
                argv: vec![
                    "-I".into(),
                    "-S".into(),
                    "-c".into(),
                    MOCK.into(),
                    mode.into(),
                ],
                observation_origin: "mock_app_server_child",
                original_clock: None,
            },
        )
    }
}
#[test]
fn app_preflight_and_run_refusal_do_not_create_claim_home_or_output() -> Result<()> {
    let f = MockFixture::new()?;
    let v: Value = strict(dispatch(&f.path, &f.sha, false)?.as_bytes())?;
    assert_eq!(v["recipe_matches"], true);
    assert_eq!(v["effect_count"], 0);
    assert!(dispatch(&f.path, &f.sha, true).is_err());
    let a = Admission::load(&f.path, &f.sha)?;
    let expired = ExecutionPermit {
        control_sha256: f.sha.clone(),
        deadline: Instant::now(),
        argv: Vec::new(),
        observation_origin: "pure_expired_mock_permit",
        original_clock: None,
    };
    assert!(consume(&a, &expired).is_err());
    assert!(!f.output.exists());
    assert!(fs::read_dir(&f.claims)?.next().is_none());
    for name in ["unread-home", "unread-codex", "unread-sqlite", "unread-log"] {
        assert!(!f.f.root.join(name).exists());
    }
    Ok(())
}
#[test]
fn actual_bidirectional_mock_child_captures_native_ids_not_served_profile() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = MockFixture::new()?;
    let r = f.run("good", Duration::from_secs(3))?;
    assert_eq!(
        r.report.base.status,
        "typed_structural_unqualified",
        "{}",
        serde_json::to_string(&r)?
    );
    assert!(r.publication_sha256.is_some() && r.report.base.verdict.is_some());
    assert!(r.report.internal_failure_code.is_none());
    assert!(r.report.protocol.failing_notification_category.is_none());
    assert!(r.report.protocol.native_error_category.is_none());
    assert!(r.report.protocol.native_error_will_retry.is_none());
    assert_eq!(
        r.report.base.observations.native_turn_id.as_deref(),
        Some("native-turn")
    );
    assert!(r.report.base.observations.actual_response_id.is_none());
    assert!(r.report.base.observations.actual_judge_model.is_none());
    assert!(r.report.base.observations.actual_cost.is_none());
    assert_eq!(r.report.protocol.outbound_frames.len(), 4);
    assert!(
        r.report
            .protocol
            .outbound_frames
            .iter()
            .all(|f| f.full_write_observed && f.bytes_written == f.prepared_bytes)
    );
    assert!(!r.report.base.reward_emitted);
    assert!(
        r.report.base.observations.wrapper_leader_reaped
            && r.report.base.observations.pipe_eof_observed
    );
    let b = fs::read(f.output.join("app-server-observation.json"))?;
    assert!(!String::from_utf8_lossy(&b).contains("mock-private-stderr"));
    assert_eq!(fs::metadata(&f.output)?.permissions().mode() & 0o777, 0o700);
    for p in fs::read_dir(&f.output)? {
        assert_eq!(p?.metadata()?.permissions().mode() & 0o777, 0o600);
    }
    assert!(f.run("good", Duration::from_secs(3)).is_err());
    Ok(())
}
#[test]
fn mock_rpc_error_retains_only_numeric_code_and_permanent_claim() -> Result<()> {
    let f = MockFixture::new()?;
    let r = f.run("error", Duration::from_secs(3))?;
    assert!(r.report.base.verdict.is_none());
    assert_eq!(r.report.protocol.rpc_error_code, Some(-32001));
    assert_eq!(r.report.internal_failure_code, Some("app_rpc_error"));
    assert_eq!(r.report.base.failure_code, Some("app_unavailable_no_retry"));
    assert_eq!(r.report.base.status, "completion_unknown_quarantined");
    assert!(r.no_retry && !r.report.base.reward_emitted);
    assert!(!serde_json::to_string(&r)?.contains("mock-secret"));
    let receipt = fs::read(f.output.join("app-server-observation.json"))?;
    assert!(!String::from_utf8_lossy(&receipt).contains("mock-secret"));
    let receipt: Value = strict(&receipt)?;
    assert_eq!(receipt["internal_failure_code"], "app_rpc_error");
    assert!(f.claims.join("mock-app.claim.json").exists());
    Ok(())
}
#[test]
fn mock_prestarted_error_retains_safe_category_without_admitting_or_retrying() -> Result<()> {
    let f = MockFixture::new()?;
    let r = f.run("prestarted", Duration::from_secs(3))?;
    assert_eq!(r.report.base.status, "completion_unknown_quarantined");
    assert_eq!(r.report.base.failure_code, Some("app_unavailable_no_retry"));
    assert_eq!(r.report.internal_failure_code, Some("app_turn_not_started"));
    assert_eq!(
        r.report.protocol.failing_notification_category,
        Some("error")
    );
    assert_eq!(
        r.report.protocol.native_error_category,
        Some("httpConnectionFailed")
    );
    assert_eq!(r.report.protocol.native_error_will_retry, Some(false));
    assert!(r.report.base.verdict.is_none() && !r.report.base.reward_emitted && r.no_retry);
    assert!(f.claims.join("mock-app.claim.json").exists());
    let receipt = fs::read(f.output.join("app-server-observation.json"))?;
    assert!(!String::from_utf8_lossy(&receipt).contains("mock-secret"));
    let receipt: Value = strict(&receipt)?;
    assert_eq!(
        receipt["protocol"]["failing_notification_category"],
        "error"
    );
    assert_eq!(
        receipt["protocol"]["native_error_category"],
        "httpConnectionFailed"
    );
    assert_eq!(receipt["protocol"]["native_error_will_retry"], false);
    assert!(f.run("prestarted", Duration::from_secs(3)).is_err());
    Ok(())
}
#[test]
fn mock_server_request_flood_and_stall_fail_closed_no_retry() -> Result<()> {
    for mode in ["server", "flood", "hang"] {
        let f = MockFixture::new()?;
        if mode == "flood" {
            let mut c: Control = strict(&fs::read(&f.path)?)?;
            c.limits.event_bytes = 1024;
            let b = json_bytes(&c, CONTROL_MAX)?;
            fs::write(&f.path, &b)?;
            let f = MockFixture {
                sha: digest(&b),
                ..f
            };
            let r = f.run(mode, Duration::from_secs(1))?;
            assert!(r.report.base.verdict.is_none() && r.no_retry);
            continue;
        }
        let r = f.run(mode, Duration::from_millis(300))?;
        assert!(r.report.base.verdict.is_none() && r.no_retry);
        assert!(f.claims.join("mock-app.claim.json").exists());
        if mode == "server" {
            assert!(r.report.protocol.tools_observed_and_rejected);
        }
    }
    Ok(())
}
