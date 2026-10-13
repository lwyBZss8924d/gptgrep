//! Native PTC retrieval reader past the shared tool budget. Synthetic documents
//! and scripted RPC only; no provider, model or native runtime is contacted.
use super::*;
use crate::jev_accounting::Accounting;
use crate::tests::mandatory::{corpus, mock_jev};
use std::sync::atomic::Ordering;

fn tool_call(id: u64, tool: &str, arguments: Value) -> Value {
    json!({"id":id,"method":"item/tool/call","params":{"threadId":"ptc-thread",
        "turnId":"ptc-turn","callId":format!("budget-call-{id}"),"namespace":"gptgrep",
        "tool":tool,"arguments":arguments}})
}

/// Local lexical search: issues evidence without Jev work.
fn lexical(id: u64) -> Value {
    tool_call(
        id,
        "gptgrep_search",
        json!({"query":"Exact source words","mode":"lexical"}),
    )
}

/// Hybrid search requires Jev when admitted; past the budget it must not run.
fn excess(id: u64) -> Value {
    tool_call(
        id,
        "gptgrep_search",
        json!({"query":"private-query-sentinel","mode":"hybrid"}),
    )
}

fn finalize(answer: &Value) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"ptc-thread",
    "turn":{"id":"ptc-turn","status":"completed","error":null,"items":[
        {"type":"agentMessage","phase":"final_answer","id":"answer","text":answer.to_string()}
    ]}}})
}

fn failed_turn() -> Value {
    json!({"method":"turn/completed","params":{"threadId":"ptc-thread",
        "turn":{"id":"ptc-turn","status":"failed","items":[],
            "error":{"message":"private-error-sentinel","codexErrorInfo":"contextWindowExceeded"}}}})
}

fn budget(maximum: usize, admitted: usize, denied: usize) -> ToolBudgetDiagnostics {
    ToolBudgetDiagnostics {
        max_tool_calls: maximum,
        admitted_tool_calls: admitted,
        denied_tool_calls: denied,
        max_denied_tool_calls: maximum,
    }
}

fn turn_starts(frames: &[Value]) -> usize {
    frames
        .iter()
        .filter(|frame| frame["method"] == "turn/start")
        .count()
}

/// The host's reply to one scripted dynamic request.
fn reply(frames: &[Value], id: u64) -> &Value {
    frames
        .iter()
        .find(|frame| frame["id"] == id && frame.get("method").is_none())
        .expect("host reply")
}

/// Exactly the restricted reader's denial: same message, shape and diagnostics.
fn assert_restricted_denial(root: &Path, frame: &Value, expected: ToolBudgetDiagnostics) {
    // The restricted reader denies only after admitting max_tool_calls calls.
    assert_eq!(expected.admitted_tool_calls, expected.max_tool_calls);
    let mut restricted = Evidence::open(root, None).unwrap();
    let payload = restricted
        .deny_budget("restricted", "gptgrep_search", json!({}), expected)
        .unwrap();
    assert!(frame.get("error").is_none(), "{frame}");
    assert_eq!(frame["result"], payload);
    let text: Value =
        serde_json::from_str(frame["result"]["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["error"], "tool_budget_exhausted");
    assert!(text["message"].as_str().unwrap().contains("Finalize now"));
    assert!(!frame.to_string().contains("private-"));
}

fn assert_denial_receipt(receipt: &crate::retrieval::ToolReceipt, expected: ToolBudgetDiagnostics) {
    assert!(!receipt.success);
    assert!(!receipt.required_initial);
    assert!(receipt.evidence.is_empty(), "a denial issued evidence");
    assert!(receipt.search.is_none(), "a denial performed retrieval");
    assert_eq!(receipt.tool_budget, Some(expected));
}

/// Budget diagnostics as published in model_attempts[].native_ptc.
fn reported_budget(report: &crate::NativePtcReport) -> Value {
    serde_json::to_value(report).unwrap()["tool_budget"].clone()
}

async fn issued_node(root: &Path) -> String {
    let mut probe = Evidence::open(root, None).unwrap();
    let payload = probe
        .call(
            "probe",
            "gptgrep_search",
            json!({"query":"Exact source words","mode":"lexical"}),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(payload["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    body["hits"][0]["node_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn over_budget_request_gets_the_restricted_denial_and_the_reader_answers() {
    let (root, mut evidence) = fixture().await;
    let node = issued_node(root.path()).await;
    let answer =
        json!({"answer":"Exact source words.","citations":[node],"insufficient_evidence":false});
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![lexical(11), excess(12), finalize(&answer)],
    )
    .await;
    let outcome = result.expect("the denied reader finalizes in the same turn");
    assert_eq!(reply(&frames, 11)["result"]["success"], true);
    assert_restricted_denial(root.path(), reply(&frames, 12), budget(1, 1, 1));
    assert_eq!(turn_starts(&frames), 1, "no retry or new turn");
    assert_eq!(outcome.answer, answer.to_string());
    assert!(outcome.warnings.iter().any(|warning| warning.contains(
        "after 1 admitted GPTgrep calls and 0 observed native items; 1 additional GPTgrep requests were denied without retrieval"
    )));
    assert_eq!(evidence.receipts.len(), 2);
    assert!(evidence.receipts[0].success && !evidence.receipts[0].evidence.is_empty());
    assert_denial_receipt(&evidence.receipts[1], budget(1, 1, 1));
    let (_, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert!(!insufficient);
    assert_eq!(citations.len(), 1);
    assert_eq!(attempt.status, "completed");
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 1);
    assert_eq!(report.dynamic_requests_denied, 1);
    assert_eq!(report.native_tool_calls, 0);
    assert_eq!(report.combined_tool_calls, 1);
    assert!(!report.budget_exceeded);
    assert!(report.successful_turn_completed_observed);
    assert_eq!(
        reported_budget(&report),
        serde_json::to_value(budget(1, 1, 1)).unwrap()
    );
    assert!(!serde_json::to_string(&report).unwrap().contains("private-"));
}

#[tokio::test]
async fn over_budget_denial_lets_the_reader_return_insufficient_evidence() {
    let (root, mut evidence) = fixture().await;
    let answer = json!({"answer":"The issued evidence does not answer the question.",
        "citations":[],"insufficient_evidence":true});
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 2),
        vec![
            lexical(11),
            tool_call(12, "gptgrep_catalog", json!({})),
            excess(13),
            finalize(&answer),
        ],
    )
    .await;
    let outcome = result.expect("the denied reader abstains in the same turn");
    assert_restricted_denial(root.path(), reply(&frames, 13), budget(2, 2, 1));
    assert_eq!(turn_starts(&frames), 1, "no retry or new turn");
    let (_, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert!(insufficient);
    assert!(citations.is_empty());
    assert_eq!(evidence.receipts.len(), 3);
    assert_denial_receipt(&evidence.receipts[2], budget(2, 2, 1));
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 2);
    assert_eq!(report.combined_tool_calls, 2);
    assert!(!report.budget_exceeded);
    assert_eq!(
        reported_budget(&report),
        serde_json::to_value(budget(2, 2, 1)).unwrap()
    );
}

#[tokio::test]
async fn denial_bound_fails_with_the_existing_native_budget_code() {
    let (root, mut evidence) = fixture().await;
    let answer = json!({"answer":"Unreachable.","citations":[],"insufficient_evidence":true});
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 2),
        vec![
            lexical(11),
            tool_call(12, "gptgrep_catalog", json!({})),
            excess(13),
            excess(14),
            excess(15),
            finalize(&answer),
        ],
    )
    .await;
    // The same bound as the restricted reader: max_tool_calls denials, then
    // the next request fails the workflow with the unchanged native code.
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    assert_restricted_denial(root.path(), reply(&frames, 13), budget(2, 2, 1));
    assert_restricted_denial(root.path(), reply(&frames, 14), budget(2, 2, 2));
    let refused = reply(&frames, 15);
    assert_eq!(refused["error"]["code"], -32601);
    assert!(refused.get("result").is_none());
    assert_eq!(evidence.receipts.len(), 4);
    assert_denial_receipt(&evidence.receipts[2], budget(2, 2, 1));
    assert_denial_receipt(&evidence.receipts[3], budget(2, 2, 2));
    assert_eq!(attempt.status, "failed");
    assert_eq!(
        attempt.error.as_ref().unwrap()["code"],
        "host_native_ptc_tool_budget_exhausted"
    );
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 2);
    // Two recoverable denials plus the refused request.
    assert_eq!(report.dynamic_requests_denied, 3);
    assert_eq!(report.combined_tool_calls, 2);
    assert!(report.budget_exceeded);
    assert!(!report.successful_turn_completed_observed);
    assert_eq!(
        reported_budget(&report),
        serde_json::to_value(budget(2, 2, 2)).unwrap()
    );
}

#[tokio::test]
async fn native_items_keep_their_hard_limit_after_dynamic_denials() {
    let (root, mut evidence) = fixture().await;
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![
            tool_call(11, "gptgrep_catalog", json!({})),
            excess(12),
            event("item/started", command("native-after-denial", "inProgress")),
            completed(vec![]),
        ],
    )
    .await;
    // A native item cannot be denied before it runs: the first one past the
    // shared budget still stops the host, and denials never extend its bound.
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    assert_restricted_denial(root.path(), reply(&frames, 12), budget(1, 1, 1));
    assert_eq!(evidence.receipts.len(), 2);
    assert_eq!(attempt.status, "failed");
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 1);
    assert_eq!(report.dynamic_requests_denied, 1);
    assert_eq!(report.native_tool_calls, 1);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.combined_tool_calls, 2);
    assert!(report.budget_exceeded);
    assert_eq!(
        reported_budget(&report),
        serde_json::to_value(budget(1, 1, 1)).unwrap()
    );
}

#[tokio::test]
async fn a_native_item_counted_before_exhaustion_may_finish_after_denials() {
    let (root, mut evidence) = fixture().await;
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![
            event("item/started", command("native-first", "inProgress")),
            excess(11),
            event("item/completed", command("native-first", "completed")),
            completed(vec![command("native-first", "completed")]),
        ],
    )
    .await;
    let outcome = result.expect("the denied reader finalizes after its native item");
    // The native item was admitted to the shared budget, so the model reads
    // the restricted reader's diagnostics: the budget is used, not 0 of 1.
    assert_restricted_denial(root.path(), reply(&frames, 11), budget(1, 1, 1));
    assert_eq!(evidence.receipts.len(), 1);
    assert_denial_receipt(&evidence.receipts[0], budget(1, 1, 1));
    // The warning keeps the split: no GPTgrep call was charged.
    assert!(outcome.warnings.iter().any(|warning| warning.contains(
        "after 0 admitted GPTgrep calls and 1 observed native items; 1 additional GPTgrep requests were denied without retrieval"
    )));
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 0);
    assert_eq!(report.dynamic_requests_denied, 1);
    assert_eq!(report.native_tool_calls, 1);
    assert_eq!(report.combined_tool_calls, 1);
    assert!(!report.budget_exceeded);
    assert!(report.successful_turn_completed_observed);
    assert!(
        report.items[0].completed_observed
            && report.items[0].status == Some(NativeCommandStatus::Completed)
    );
    assert_eq!(report.tool_budget, Some(budget(1, 1, 1)));
}

/// Whatever mix of observed native items and charged GPTgrep calls used the
/// shared budget, the model-visible denial, its receipt and a later typed turn
/// failure are byte-identical to a restricted run that used the same budget.
#[tokio::test]
async fn native_denials_are_byte_identical_to_the_restricted_readers() {
    // (max_tool_calls, observed native items): native-first, a mix that charged
    // one GPTgrep call, and the default budget with four GPTgrep calls. Each
    // run takes the full denial bound.
    for (maximum, native_items) in [(1, 1), (3, 2), (12, 8)] {
        let (root, mut native_evidence) = fixture().await;
        let mut restricted_evidence = Evidence::open(root.path(), None).unwrap();
        let excess_ids: Vec<u64> = (200..).take(maximum).collect();
        // The restricted reader can use its budget only with GPTgrep calls.
        let mut restricted_script: Vec<Value> = (100..).take(maximum).map(lexical).collect();
        let mut native_script: Vec<Value> = (0..native_items)
            .map(|index| event("item/completed", image(&format!("native-{index}"))))
            .collect();
        native_script.extend((100..).take(maximum - native_items).map(lexical));
        for script in [&mut restricted_script, &mut native_script] {
            script.extend(excess_ids.iter().copied().map(excess));
            script.push(failed_turn());
        }
        let (restricted, restricted_frames, _) = exchange(
            root.path(),
            &mut restricted_evidence,
            &config(false, maximum),
            restricted_script,
        )
        .await;
        let (native, native_frames, attempt) = exchange(
            root.path(),
            &mut native_evidence,
            &config(true, maximum),
            native_script,
        )
        .await;
        for id in &excess_ids {
            let expected = reply(&restricted_frames, *id);
            let actual = reply(&native_frames, *id);
            // The exact text the model reads, then the complete reply frame.
            assert_eq!(
                actual["result"]["contentItems"][0]["text"].as_str(),
                expected["result"]["contentItems"][0]["text"].as_str(),
                "max {maximum}, native {native_items}, request {id}"
            );
            assert_eq!(actual, expected);
        }
        let restricted_denials = &restricted_evidence.receipts[maximum..];
        let native_denials = &native_evidence.receipts[maximum - native_items..];
        assert_eq!(restricted_denials.len(), maximum);
        assert_eq!(
            serde_json::to_string(native_denials).unwrap(),
            serde_json::to_string(restricted_denials).unwrap()
        );
        let restricted = restricted.err().expect("the restricted turn failed");
        let native = native.err().expect("the native turn failed");
        let restricted = restricted.downcast_ref::<HostProtocolError>().unwrap();
        let native = native.downcast_ref::<HostProtocolError>().unwrap();
        assert_eq!(native.kind, HostProtocolErrorKind::FailedTurn);
        assert_eq!(
            serde_json::to_string(native).unwrap(),
            serde_json::to_string(restricted).unwrap()
        );
        let expected = budget(maximum, maximum, maximum);
        assert_eq!(restricted.tool_budget, Some(expected));
        // The native report keeps the split behind the shared admitted count.
        let report = attempt.native_ptc.unwrap();
        assert_eq!(report.tool_budget, Some(expected));
        assert_eq!(report.dynamic_requests_charged, maximum - native_items);
        assert_eq!(report.native_tool_calls, native_items);
        assert_eq!(report.combined_tool_calls, maximum);
        assert_eq!(report.dynamic_requests_denied, maximum);
        assert!(!report.budget_exceeded);
    }
}

#[tokio::test]
async fn failed_turn_after_a_native_denial_keeps_typed_budget_diagnostics() {
    let (root, mut evidence) = fixture().await;
    let (result, _, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![
            tool_call(11, "gptgrep_catalog", json!({})),
            excess(12),
            failed_turn(),
        ],
    )
    .await;
    let error = result.err().unwrap();
    let typed = error
        .downcast_ref::<HostProtocolError>()
        .expect("the primary turn failure stays typed");
    assert_eq!(typed.kind, HostProtocolErrorKind::FailedTurn);
    assert_eq!(typed.tool_budget, Some(budget(1, 1, 1)));
    assert!(!serde_json::to_string(typed).unwrap().contains("private-"));
    assert_eq!(
        attempt.error.as_ref().unwrap()["code"],
        "host_codex_failed_turn"
    );
    let report = attempt.native_ptc.unwrap();
    assert!(!report.budget_exceeded);
    assert_eq!(
        reported_budget(&report),
        serde_json::to_value(budget(1, 1, 1)).unwrap()
    );
}

#[tokio::test]
async fn malformed_excess_requests_remain_fatal_without_a_denial() {
    for arguments in [
        json!({}),
        json!({"query":"test","mode":"unsupported"}),
        json!({"query":"test","extra":"private-arguments-sentinel"}),
    ] {
        let (root, mut evidence) = fixture().await;
        let (result, frames, attempt) = exchange(
            root.path(),
            &mut evidence,
            &config(true, 1),
            vec![
                tool_call(11, "gptgrep_catalog", json!({})),
                tool_call(12, "gptgrep_search", arguments),
                completed(vec![]),
            ],
        )
        .await;
        let error = result.err().expect("a malformed excess request is fatal");
        // The restricted reader's shape check fails before any denial is counted.
        assert_eq!(
            error.to_string(),
            "Invalid dynamic tool arguments after tool-budget exhaustion"
        );
        assert!(!format!("{error:?}").contains("private-"));
        assert!(
            frames
                .iter()
                .all(|frame| frame["id"] != 12 || frame.get("result").is_none())
        );
        assert_eq!(evidence.receipts.len(), 1);
        let report = attempt.native_ptc.unwrap();
        assert_eq!(report.dynamic_requests_charged, 1);
        assert_eq!(report.dynamic_requests_denied, 0);
        assert!(reported_budget(&report).is_null());
    }
}

#[tokio::test]
async fn native_denial_performs_no_jev_work_and_the_reader_cites_seed_evidence() {
    let root = corpus().await;
    // One scoped document: the seed needs only the rerank call.
    let (client, server, requests) = mock_jev(vec![(200, 3.0)]).await;
    let config = HostConfig {
        document: Some("pipeline-notes.md".into()),
        ..config(true, 1)
    };
    let question = "How are tokens assembled?";
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting = Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&config, accounting.clone(), Some(client))
        .unwrap();
    accounting
        .bind_workflow(question, evidence.document_scope())
        .unwrap();
    evidence.bootstrap(question).await.unwrap();
    server.await.unwrap();
    let seed = evidence.initial_payload.as_ref().unwrap()["hits"][0]["node_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let answer = json!({"answer":"Empty records are removed before the remaining tokens are joined.",
        "citations":[seed],"insufficient_evidence":false});
    let mut attempt = ModelAttemptTracker::reserve(&accounting, "final_reader", &config).unwrap();
    let mut replies = vec![
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{}}}),
        json!({"id":3,"result":{"thread":{"id":"ptc-thread"},"model":crate::DEFAULT_MODEL,
            "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
            "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false},
            "activePermissionProfile":{"id":"native_ptc_reader"}}}),
        json!({"id":4,"result":{"turn":{"id":"ptc-turn","status":"inProgress"}}}),
    ];
    replies.extend([
        tool_call(11, "gptgrep_catalog", json!({})),
        tool_call(
            12,
            "gptgrep_search",
            json!({"query":"How are tokens assembled?","mode":"hybrid"}),
        ),
        finalize(&answer),
    ]);
    let input = replies
        .iter()
        .map(|frame| format!("{frame}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    let result = run_observed(
        BufReader::new(input.as_bytes()),
        &mut output,
        root.path(),
        &config,
        Workflow::Retrieval {
            question,
            node_id: None,
            evidence: &mut evidence,
        },
        None,
        RunOptions {
            observer: Some(attempt.observer()),
            ..RunOptions::default()
        },
    )
    .await;
    attempt.finish(result.as_ref().err()).unwrap();
    let outcome = result.expect("the denied reader finalizes from the seed");
    let frames: Vec<Value> = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_restricted_denial(root.path(), reply(&frames, 12), budget(1, 1, 1));
    let (_, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert!(!insufficient);
    assert_eq!(citations.len(), 1);
    // Only the mandatory seed reached Jev; the denied hybrid search did not.
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let summary = accounting.summary();
    assert_eq!(summary.requests, 1);
    assert_eq!(summary.attempted_calls, 1);
    assert_eq!(summary.searches.len(), 1);
    assert_eq!(evidence.receipts.len(), 3);
    assert!(evidence.receipts[0].required_initial);
    assert!(evidence.receipts[1].success);
    assert_denial_receipt(&evidence.receipts[2], budget(1, 1, 1));
    let attempts = accounting.model_attempts();
    assert_eq!(attempts.len(), 1);
    let report = attempts[0].native_ptc.as_ref().unwrap();
    assert_eq!(report.combined_tool_calls, 1);
    assert_eq!(
        reported_budget(report),
        serde_json::to_value(budget(1, 1, 1)).unwrap()
    );
}

#[test]
fn retrieval_denials_are_bounded_never_charged_and_keep_identities() {
    use crate::native_ptc::DynamicCharge;
    let mut tracker = Tracker::new(2).unwrap();
    assert!(tracker.deny_dynamic(&json!("early")).is_err());
    assert_eq!(
        tracker.charge_retrieval(&json!("dynamic")).unwrap(),
        DynamicCharge::Admit
    );
    tracker
        .observe(&image("native"), ItemEvent::Completed)
        .unwrap();
    for (index, id) in ["denied-one", "denied-two"].into_iter().enumerate() {
        assert_eq!(
            tracker.charge_retrieval(&json!(id)).unwrap(),
            DynamicCharge::Deny
        );
        assert_eq!(
            tracker.deny_dynamic(&json!(id)).unwrap(),
            budget(2, 2, index + 1)
        );
        let report = tracker.report();
        assert_eq!(report.dynamic_requests_charged, 1);
        assert_eq!(report.native_tool_calls, 1);
        assert_eq!(report.combined_tool_calls, 2);
        assert_eq!(report.dynamic_requests_denied, index + 1);
        assert!(!report.budget_exceeded);
        assert_eq!(report.tool_budget, Some(budget(2, 2, index + 1)));
    }
    // Denied identities stay bound: replays and kind changes remain fatal
    // without changing any count.
    assert!(tracker.charge_retrieval(&json!("denied-one")).is_err());
    assert!(
        tracker
            .observe(&image("denied-two"), ItemEvent::Completed)
            .is_err()
    );
    assert_eq!(tracker.report().dynamic_requests_denied, 2);
    // The restricted reader's bound: the next excess request fails.
    assert_eq!(
        tracker
            .charge_retrieval(&json!("refused"))
            .unwrap_err()
            .to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    let report = tracker.report();
    assert_eq!(report.dynamic_requests_denied, 3);
    assert!(report.budget_exceeded);
    assert_eq!(report.tool_budget, Some(budget(2, 2, 2)));
    assert!(tracker.charge_retrieval(&json!("after")).is_err());
    assert!(tracker.deny_dynamic(&json!("after")).is_err());
}

#[test]
fn denials_never_widen_the_native_bound_and_hard_limit_callers_are_unchanged() {
    use crate::native_ptc::DynamicCharge;
    let mut tracker = Tracker::new(1).unwrap();
    assert_eq!(
        tracker.charge_retrieval(&json!("dynamic")).unwrap(),
        DynamicCharge::Admit
    );
    assert_eq!(
        tracker.charge_retrieval(&json!("denied")).unwrap(),
        DynamicCharge::Deny
    );
    tracker.deny_dynamic(&json!("denied")).unwrap();
    assert!(
        tracker
            .observe(&image("native"), ItemEvent::Completed)
            .is_err()
    );
    let report = tracker.report();
    assert_eq!(report.native_tool_calls, 1);
    assert_eq!(report.combined_tool_calls, 2);
    assert_eq!(report.items.len(), 1);
    assert!(report.budget_exceeded);
    // The native item that stopped the host is counted, never admitted: the
    // diagnostics keep admitted at the limit, as at the denial.
    assert_eq!(report.tool_budget, Some(budget(1, 1, 1)));
    // Rich readers and the Document driver keep failing on the first excess.
    let mut hard = Tracker::new(1).unwrap();
    hard.charge_dynamic(&json!("first")).unwrap();
    assert_eq!(
        hard.charge_dynamic(&json!("second"))
            .unwrap_err()
            .to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    let report = hard.report();
    assert_eq!(report.dynamic_requests_charged, 1);
    assert_eq!(report.dynamic_requests_denied, 1);
    assert!(report.budget_exceeded);
    assert!(report.tool_budget.is_none());
    assert!(
        serde_json::to_value(&report)
            .unwrap()
            .get("tool_budget")
            .is_none()
    );
}
