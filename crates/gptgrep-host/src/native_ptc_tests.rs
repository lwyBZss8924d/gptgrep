//! Prepared regression source. Uses synthetic documents and in-memory RPC only.
//! This fixture never establishes native pre-execution enforcement.
use super::*;
use crate::model_attempts::ModelAttemptTracker;
use crate::native_ptc::{ItemEvent, NativeCommandStatus, NativeToolKind, Tracker};

fn config(enabled: bool, limit: usize) -> HostConfig {
    HostConfig {
        native_ptc: enabled,
        permission_profile: enabled.then(|| "native_ptc_reader".into()),
        max_tool_calls: limit,
        ..HostConfig::default()
    }
}

fn command(id: &str, status: &str) -> Value {
    json!({"type":"commandExecution","id":id,"status":status,
        "command":"private-command-sentinel","cwd":"/private/path-sentinel",
        "aggregatedOutput":"private-output-sentinel","processId":"private-process-sentinel"})
}

#[test]
fn native_item_families_keep_their_actual_status_contracts() {
    let mut tracker = Tracker::new(128).unwrap();
    for (index, kind) in ["fileChange", "mcpToolCall", "collabAgentToolCall"]
        .into_iter()
        .enumerate()
    {
        let id = format!("tool-{index}");
        tracker
            .observe(
                &json!({"type":kind,"id":id,"status":"inProgress"}),
                ItemEvent::Started,
            )
            .unwrap();
        tracker
            .observe(
                &json!({"type":kind,"id":id,"status":"completed"}),
                ItemEvent::Completed,
            )
            .unwrap();
        tracker
            .observe(
                &json!({"type":kind,"id":id,"status":"completed"}),
                ItemEvent::FinalSnapshot,
            )
            .unwrap();
    }
    tracker
        .observe(
            &json!({"type":"collabAgentToolCall","id":"interrupted","status":"interrupted"}),
            ItemEvent::FinalSnapshot,
        )
        .unwrap();
    for kind in ["webSearch", "sleep", "imageGeneration"] {
        let item = json!({"type":kind,"id":kind,"status":"runtime-owned-value"});
        tracker.observe(&item, ItemEvent::Started).unwrap();
        tracker.observe(&item, ItemEvent::Completed).unwrap();
        tracker.observe(&item, ItemEvent::FinalSnapshot).unwrap();
    }
    let report = tracker.report();
    assert_eq!(report.native_tool_calls, 7);
    assert!(
        report
            .items
            .iter()
            .filter(|item| matches!(
                item.kind,
                NativeToolKind::WebSearch | NativeToolKind::Sleep | NativeToolKind::ImageGeneration
            ))
            .all(|item| item.status.is_none() && item.completed_observed)
    );
    for (kind, status) in [
        ("mcpToolCall", "declined"),
        ("commandExecution", "interrupted"),
        ("fileChange", "unknown"),
    ] {
        assert!(
            tracker
                .observe(
                    &json!({"type":kind,"id":"invalid","status":status}),
                    ItemEvent::FinalSnapshot
                )
                .is_err()
        );
    }
}

fn image(id: &str) -> Value {
    json!({"type":"imageView","id":id,"path":"/private/image-sentinel.png"})
}

fn event(method: &str, item: Value) -> Value {
    json!({"method":method,"params":{"threadId":"ptc-thread","turnId":"ptc-turn","item":item}})
}

fn completed(mut items: Vec<Value>) -> Value {
    items.push(json!({"type":"agentMessage","phase":"final_answer","id":"answer",
        "text":json!({"answer":"Uncertain","citations":[],"insufficient_evidence":true}).to_string()}));
    json!({"method":"turn/completed","params":{"threadId":"ptc-thread",
        "turn":{"id":"ptc-turn","status":"completed","error":null,"items":items}}})
}

fn dynamic(id: &str) -> Value {
    json!({"id":91,"method":"item/tool/call","params":{"threadId":"ptc-thread",
        "turnId":"ptc-turn","callId":id,"namespace":"gptgrep",
        "tool":"gptgrep_catalog","arguments":{}}})
}

fn request<'a>(sent: &'a [Value], method: &str) -> &'a Value {
    &sent.iter().find(|frame| frame["method"] == method).unwrap()["params"]
}

async fn fixture() -> (tempfile::TempDir, Evidence) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("source.md"),
        "# Neutral\nExact source words.\n",
    )
    .unwrap();
    gptgrep_core::index(root.path(), 4).await.unwrap();
    let evidence = Evidence::open(root.path(), None).unwrap();
    (root, evidence)
}

async fn exchange(
    root: &Path,
    evidence: &mut Evidence,
    config: &HostConfig,
    events: Vec<Value>,
) -> (Result<Outcome>, Vec<Value>, crate::ModelAttempt) {
    let accounting =
        crate::jev_accounting::Accounting::create(root, "synthetic-generation").unwrap();
    accounting.bind_workflow("neutral question", None).unwrap();
    let mut attempt = ModelAttemptTracker::reserve(&accounting, "final_reader", config).unwrap();
    let mut thread = json!({"thread":{"id":"ptc-thread"},"model":crate::DEFAULT_MODEL,
        "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
        "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}});
    if let Some(profile) = &config.permission_profile {
        thread["activePermissionProfile"] = json!({"id":profile});
    }
    let mut replies = vec![
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{"mcp_servers":{"inherited":{"command":"private-mcp-sentinel"}}}}}),
        json!({"id":3,"result":thread}),
        json!({"id":4,"result":{"turn":{"id":"ptc-turn","status":"inProgress"}}}),
    ];
    replies.extend(events);
    let input = replies
        .iter()
        .map(|frame| format!("{frame}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    let result = run_observed(
        BufReader::new(input.as_bytes()),
        &mut output,
        root,
        config,
        Workflow::Retrieval {
            question: "neutral question",
            node_id: None,
            evidence,
        },
        None,
        RunOptions {
            observer: Some(attempt.observer()),
            ..RunOptions::default()
        },
    )
    .await;
    attempt.finish(result.as_ref().err()).unwrap();
    let frames = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (result, frames, accounting.model_attempts().remove(0))
}

#[test]
fn default_config_keeps_native_ptc_disabled_for_retrieval_and_completion() {
    let selected = HostConfig::default();
    assert!(!selected.native_ptc);
    assert!(!crate::native_ptc::enabled(&selected, false).unwrap());
    assert!(!crate::native_ptc::enabled(&selected, true).unwrap());
}

#[tokio::test]
async fn default_native_items_are_rejected_in_notifications_and_final_snapshots() {
    for item in [command("command", "completed"), image("image")] {
        for method in ["item/started", "item/completed", "turn/completed"] {
            let (root, mut evidence) = fixture().await;
            let message = if method == "turn/completed" {
                completed(vec![item.clone()])
            } else {
                event(method, item.clone())
            };
            let (result, _, attempt) = exchange(
                root.path(),
                &mut evidence,
                &config(false, 12),
                vec![message],
            )
            .await;
            assert!(result.is_err(), "default accepted {method}");
            assert!(attempt.native_ptc.is_none());
            assert!(
                serde_json::to_value(attempt)
                    .unwrap()
                    .get("native_ptc")
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn opt_in_exposes_direct_tools_without_fallback_and_preserves_the_permission_envelope() {
    let (root, mut evidence) = fixture().await;
    let (_, restricted, _) = exchange(
        root.path(),
        &mut evidence,
        &config(false, 12),
        vec![completed(vec![])],
    )
    .await;
    let (result, opted, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 12),
        vec![completed(vec![])],
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
    let default_thread = request(&restricted, "thread/start");
    let native_thread = request(&opted, "thread/start");
    for key in [
        "environments",
        "runtimeWorkspaceRoots",
        "selectedCapabilityRoots",
    ] {
        assert!(native_thread.get(key).is_none());
        assert_eq!(default_thread[key], json!([]));
    }
    let mut expected_config = crate::native_ptc::runtime_config(&config(true, 12)).unwrap();
    expected_config["model_reasoning_effort"] = json!(crate::DEFAULT_REASONING_EFFORT);
    expected_config["service_tier"] = json!(crate::DEFAULT_SERVICE_TIER);
    expected_config["features.fast_mode"] = json!(true);
    assert_eq!(native_thread["config"], expected_config);
    assert_eq!(default_thread["config"]["features.shell_tool"], false);
    assert_eq!(default_thread["config"]["features.view_image"], false);
    assert_eq!(
        default_thread["config"]["features.code_mode.direct_only_tool_namespaces"],
        json!(["gptgrep"])
    );
    assert_eq!(
        default_thread["config"]["features.code_mode_host.disable_in_process_fallback"],
        false
    );
    assert_eq!(default_thread["sandbox"], "read-only");
    for key in [
        "mcp_servers",
        "features.code_mode.enabled",
        "features.code_mode_host.enabled",
        "web_search",
    ] {
        assert!(native_thread["config"].get(key).is_none());
    }
    for method in ["thread/start", "turn/start"] {
        let params = request(&opted, method);
        assert_eq!(params["permissions"], "native_ptc_reader");
        assert_eq!(params["approvalPolicy"], "never");
        assert!(params.get("sandbox").is_none());
        assert!(params.get("sandboxPolicy").is_none());
    }
    assert_eq!(
        native_thread["dynamicTools"],
        default_thread["dynamicTools"]
    );
    assert_eq!(
        request(&opted, "turn/start")["outputSchema"],
        request(&restricted, "turn/start")["outputSchema"]
    );
    let original = default_thread["baseInstructions"].as_str().unwrap();
    assert_eq!(
        crate::retrieval::hash(original.as_bytes()),
        "a65714ee9d695bdfab9ffbff94d7ae6747f1c6fe6575055dd8c0e978e2bb18e9"
    );
    assert!(original.contains("Do not run commands, access browsers"));
    assert!(!original.contains("Native PTC"));
    let instructions = native_thread["baseInstructions"].as_str().unwrap();
    assert!(instructions.contains("installed Skills"));
    assert!(instructions.contains("does not create a GPTgrep citation"));
    assert!(!instructions.contains("Do not run commands, access browsers"));
    // Both readers carry the generic answer rules exactly once, beside every
    // unchanged evidence and citation rule.
    for text in [original, instructions] {
        assert_eq!(
            text.matches(crate::protocol::READER_ANSWER_RULES).count(),
            1
        );
        for rule in [
            "Read evidence before citing it.",
            "citations (unique node IDs that gptgrep_search or gptgrep_read actually returned as evidence)",
            "A substantive answer requires citations.",
            "If evidence is missing, say so and set insufficient_evidence=true.",
            "Do not use external knowledge to fill evidence gaps.",
            "A node may be truncated; do not claim unseen content.",
        ] {
            assert!(text.contains(rule), "{rule}");
        }
    }
    assert!(instructions.contains("Never invent citations or infer unread evidence."));
    // The native abstention rule is unchanged and shared by every reader workflow.
    assert!(instructions.ends_with(
        "Return insufficient_evidence=true when the available evidence cannot support the answer."
    ));
    // The answer rules only say when to commit; they never narrow abstention.
    for text in [original, instructions] {
        assert!(!text.contains("answer rules above"));
        assert!(!text.contains("supports no reading"));
    }
    let native = attempt.native_ptc.unwrap();
    assert_eq!(native.combined_tool_calls, 0);
    assert!(!native.pre_execution_guard);
    assert_eq!(
        native.observation,
        "app_server_items_after_possible_execution"
    );
    let mut selected = config(true, 128);
    selected
        .native_capabilities
        .insert("features.code_mode.enabled".into(), json!(true));
    assert_eq!(
        crate::native_ptc::runtime_config(&selected).unwrap()["features.code_mode.enabled"],
        true
    );
    assert!(Tracker::new(128).is_ok());
}

#[tokio::test]
async fn opt_in_requires_a_named_profile_and_completion_fails_before_rpc() {
    for profile in [None, Some(""), Some("../profile"), Some("bad\nprofile")] {
        let mut selected = config(true, 12);
        selected.permission_profile = profile.map(str::to_owned);
        assert_eq!(
            crate::native_ptc::enabled(&selected, false)
                .unwrap_err()
                .to_string(),
            "host_native_ptc_requires_named_permissions"
        );
    }
    let selected = config(true, 12);
    let mut output = Vec::new();
    let state = json!({});
    let schema = json!({"type":"object"});
    let result = run(
        BufReader::new(&b""[..]),
        &mut output,
        Path::new("/"),
        &selected,
        Workflow::Completion {
            instructions: "No tools",
            state: &state,
            schema: &schema,
        },
        None,
    )
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_ptc_requires_retrieval"
    );
    assert!(output.is_empty());
    let planner = crate::planner_runtime_config(&selected, &crate::QueryPlanConfig::default());
    assert!(!planner.native_ptc);
    assert_eq!(planner.permission_profile, selected.permission_profile);
}

#[tokio::test]
async fn started_completed_and_snapshot_charge_one_id_and_retain_no_native_payload() {
    let (root, mut evidence) = fixture().await;
    let id = "private-native-id-sentinel";
    let events = vec![
        event("item/started", command(id, "inProgress")),
        event("item/completed", command(id, "completed")),
        event("item/started", image("private-image-id-sentinel")),
        event("item/completed", image("private-image-id-sentinel")),
        completed(vec![
            command(id, "completed"),
            image("private-image-id-sentinel"),
        ]),
    ];
    let (result, _, attempt) = exchange(root.path(), &mut evidence, &config(true, 2), events).await;
    assert!(result.is_ok(), "{:?}", result.err());
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.native_tool_calls, 2);
    assert_eq!(report.combined_tool_calls, 2);
    assert!(report.successful_turn_completed_observed);
    assert!(report.items.iter().all(|item| item.started_observed
        && item.completed_observed
        && item.final_snapshot_observed));
    assert!(
        report
            .items
            .iter()
            .any(|item| item.kind == NativeToolKind::CommandExecution
                && item.status == Some(NativeCommandStatus::Completed))
    );
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("private-"));
    assert!(!encoded.contains("/private/"));
    assert!(
        report
            .items
            .iter()
            .all(|item| item.item_id_sha256.len() == 64)
    );
    assert_eq!(report.items.len(), 2);
}

#[tokio::test]
async fn wrong_thread_or_turn_is_refused_before_charging_native_metadata() {
    for item in [command("command", "inProgress"), image("image")] {
        for field in ["threadId", "turnId"] {
            let (root, mut evidence) = fixture().await;
            let mut message = event("item/started", item.clone());
            message["params"][field] = json!("foreign");
            let (result, _, attempt) =
                exchange(root.path(), &mut evidence, &config(true, 2), vec![message]).await;
            assert!(result.is_err());
            assert_eq!(attempt.native_ptc.unwrap().native_tool_calls, 0);
        }
    }
}

#[tokio::test]
async fn native_first_exhausts_dynamic_admission_before_retrieval() {
    let (root, mut evidence) = fixture().await;
    let (result, frames, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![event("item/completed", image("image")), dynamic("dynamic")],
    )
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    assert!(
        evidence.receipts.is_empty(),
        "denied request performed retrieval"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame["id"] == 91 && frame["error"]["code"] == -32601)
    );
    assert_eq!(attempt.status, "failed");
    assert_eq!(
        attempt.error.unwrap()["code"],
        "host_native_ptc_tool_budget_exhausted"
    );
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 0);
    assert_eq!(report.dynamic_requests_denied, 1);
    assert_eq!(report.combined_tool_calls, 1);
    assert!(report.budget_exceeded);
}

#[tokio::test]
async fn dynamic_first_retains_the_already_observed_native_excess_on_failure() {
    let (root, mut evidence) = fixture().await;
    let (result, _, attempt) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![
            dynamic("dynamic"),
            event("item/started", command("command", "inProgress")),
        ],
    )
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_ptc_tool_budget_exhausted"
    );
    assert_eq!(evidence.receipts.len(), 1);
    assert_eq!(attempt.status, "failed");
    let report = attempt.native_ptc.unwrap();
    assert_eq!(report.dynamic_requests_charged, 1);
    assert_eq!(report.native_tool_calls, 1);
    assert_eq!(report.combined_tool_calls, 2);
    assert_eq!(report.items.len(), 1);
    assert!(report.budget_exceeded);
    assert!(!report.pre_execution_guard);
    assert!(!report.successful_turn_completed_observed);
}

#[test]
fn snapshot_only_completion_is_counted_but_replays_kind_changes_and_bad_ids_fail() {
    let mut tracker = Tracker::new(4).unwrap();
    tracker
        .observe(&command("one", "completed"), ItemEvent::FinalSnapshot)
        .unwrap();
    assert_eq!(tracker.report().native_tool_calls, 1);
    assert!(
        tracker
            .observe(&command("one", "completed"), ItemEvent::FinalSnapshot)
            .is_err()
    );
    assert!(
        tracker
            .observe(&image("one"), ItemEvent::Completed)
            .is_err()
    );
    assert!(
        tracker
            .check_non_native(&json!({"id":"one","type":"agentMessage"}))
            .is_err()
    );
    assert!(tracker.charge_dynamic(&json!("one")).is_err());
    tracker.charge_dynamic(&json!("dynamic")).unwrap();
    assert!(
        tracker
            .observe(&image("dynamic"), ItemEvent::Completed)
            .is_err()
    );
    assert!(tracker.charge_dynamic(&json!("dynamic")).is_err());
    for id in [
        json!(""),
        json!("x".repeat(257)),
        json!("bad\nid"),
        json!(17),
        Value::Null,
    ] {
        let mut item = image("valid");
        item["id"] = id;
        assert!(tracker.observe(&item, ItemEvent::Completed).is_err());
    }
    assert!(
        tracker
            .observe(&command("bad-status", "unknown"), ItemEvent::Completed)
            .is_err()
    );
    assert!(
        tracker
            .observe(&command("bad-start", "completed"), ItemEvent::Started)
            .is_err()
    );
    assert!(
        tracker
            .observe(&command("bad-end", "inProgress"), ItemEvent::Completed)
            .is_err()
    );
    assert!(
        tracker
            .observe(
                &json!({"id":"x","type":"futureNativeTool"}),
                ItemEvent::Completed
            )
            .is_err()
    );
    assert!(Tracker::new(0).is_err());
    assert!(Tracker::new(usize::MAX).is_err());
}

#[test]
fn lifecycle_replays_terminal_status_changes_and_incomplete_items_fail() {
    let mut tracker = Tracker::new(3).unwrap();
    tracker
        .observe(&command("command", "inProgress"), ItemEvent::Started)
        .unwrap();
    assert!(
        tracker
            .observe(&command("command", "inProgress"), ItemEvent::Started)
            .is_err()
    );
    assert!(tracker.finish().is_err());
    tracker
        .observe(&command("command", "completed"), ItemEvent::Completed)
        .unwrap();
    assert!(
        tracker
            .observe(&command("command", "completed"), ItemEvent::Completed)
            .is_err()
    );
    assert!(
        tracker
            .observe(&command("command", "failed"), ItemEvent::FinalSnapshot)
            .is_err()
    );
    tracker
        .observe(&command("command", "completed"), ItemEvent::FinalSnapshot)
        .unwrap();
    tracker.finish().unwrap();
    assert_eq!(tracker.report().native_tool_calls, 1);
    let mut maximum = Tracker::new(64).unwrap();
    for index in 0..64 {
        maximum
            .observe(&image(&format!("image-{index}")), ItemEvent::Completed)
            .unwrap();
    }
    assert!(
        maximum
            .observe(&image("excess"), ItemEvent::Completed)
            .is_err()
    );
    assert_eq!(maximum.report().items.len(), 65);
    assert!(
        maximum
            .observe(&image("second-excess"), ItemEvent::Completed)
            .is_err()
    );
    assert_eq!(maximum.report().items.len(), 65);
}

#[tokio::test]
async fn native_command_ids_never_issue_legacy_citations() {
    let (root, mut evidence) = fixture().await;
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"source.md"}))
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = body["nodes"][0]["node_id"].as_str().unwrap().to_owned();
    evidence
        .call("read", "gptgrep_read", json!({"node_id":id}))
        .await
        .unwrap();
    let (result, _, _) = exchange(
        root.path(),
        &mut evidence,
        &config(true, 1),
        vec![
            event("item/completed", command("shell-id", "completed")),
            completed(vec![]),
        ],
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
    let answer = |citation: &str| {
        json!({"answer":"Exact source words.",
        "citations":[citation],"insufficient_evidence":false})
        .to_string()
    };
    assert!(evidence.finish(&answer(&id)).is_ok());
    assert!(evidence.finish(&answer("shell-id")).is_err());
    assert!(
        evidence
            .finish(&answer("/private/image-sentinel.png"))
            .is_err()
    );
}

#[tokio::test]
async fn terminal_failure_snapshots_keep_primary_error_and_bounded_native_accounting() {
    let cmd = command("private-terminal-command", "completed");
    let img = image("private-terminal-image");
    let normal = json!([cmd.clone(), img.clone(), {
        "type":"agentMessage","phase":"final_answer",
        "text":"private-final-answer-must-not-be-accepted"
    }]);
    let mut truncated = vec![json!({"type":"reasoning"}); 127];
    truncated.push(cmd.clone());
    truncated.push(img.clone());
    // Each row is a distinct failed receipt. Projection errors and excess must
    // preserve its original diagnostic, including when some counts are known.
    let cases = vec![
        (
            "failed",
            "failed",
            normal.clone(),
            vec![],
            2,
            2,
            0,
            Some("complete"),
            None,
        ),
        (
            "interrupted",
            "interrupted",
            normal.clone(),
            vec![],
            2,
            2,
            0,
            Some("complete"),
            None,
        ),
        (
            "invalid-status",
            "future-status",
            normal.clone(),
            vec![],
            2,
            2,
            0,
            Some("complete"),
            None,
        ),
        (
            "completed-with-error",
            "completed",
            normal.clone(),
            vec![],
            2,
            2,
            0,
            Some("complete"),
            None,
        ),
        (
            "already-notified",
            "failed",
            normal.clone(),
            vec![
                event(
                    "item/started",
                    command("private-terminal-command", "inProgress"),
                ),
                event("item/completed", cmd.clone()),
                event("item/completed", img.clone()),
            ],
            2,
            2,
            0,
            Some("complete"),
            None,
        ),
        (
            "native-excess",
            "failed",
            json!([cmd.clone(), img.clone(), image("uninspected-third")]),
            vec![],
            1,
            2,
            0,
            Some("partial"),
            None,
        ),
        (
            "dynamic-already-charged",
            "failed",
            json!([img.clone()]),
            vec![dynamic("earlier-dynamic")],
            1,
            1,
            1,
            Some("partial"),
            None,
        ),
        (
            "malformed-item",
            "failed",
            json!([cmd.clone(), {"type":"imageView","id":null}, img.clone()]),
            vec![],
            2,
            1,
            0,
            Some("partial"),
            None,
        ),
        (
            "unknown-kind",
            "failed",
            json!([cmd.clone(), {"type":"futureNativeTool","id":"unknown"}, img.clone()]),
            vec![],
            2,
            1,
            0,
            Some("partial"),
            None,
        ),
        (
            "replayed-snapshot",
            "failed",
            json!([cmd.clone(), cmd.clone(), img.clone()]),
            vec![],
            2,
            1,
            0,
            Some("partial"),
            None,
        ),
        (
            "missing-items",
            "failed",
            Value::Null,
            vec![],
            2,
            0,
            0,
            Some("unknown"),
            None,
        ),
        (
            "invalid-items",
            "failed",
            json!("private-invalid-snapshot"),
            vec![],
            2,
            0,
            0,
            Some("unknown"),
            None,
        ),
        (
            "bounded-inspection",
            "failed",
            json!(truncated),
            vec![],
            2,
            1,
            0,
            Some("partial"),
            None,
        ),
        (
            "foreign-thread",
            "failed",
            normal.clone(),
            vec![],
            2,
            0,
            0,
            None,
            Some("thread"),
        ),
        (
            "foreign-turn",
            "failed",
            normal,
            vec![],
            2,
            0,
            0,
            None,
            Some("turn"),
        ),
    ];
    for (
        label,
        status,
        items,
        mut events,
        limit,
        expected_native,
        expected_dynamic,
        projection,
        foreign,
    ) in cases
    {
        let (root, mut evidence) = fixture().await;
        let mut terminal = json!({"method":"turn/completed","params":{"threadId":"ptc-thread",
            "turn":{"id":"ptc-turn","status":status,"items":items,
                "error":{"message":"private-primary-error","codexErrorInfo":"contextWindowExceeded"}}}});
        match foreign {
            Some("thread") => terminal["params"]["threadId"] = json!("foreign-thread"),
            Some("turn") => terminal["params"]["turn"]["id"] = json!("foreign-turn"),
            _ => {}
        }
        let expected_code = if foreign.is_some() {
            "host_codex_error_identity_mismatch"
        } else {
            match status {
                "failed" => "host_codex_failed_turn",
                "interrupted" => "host_codex_interrupted_turn",
                _ => "host_codex_invalid_turn_status",
            }
        };
        events.push(terminal);
        let (result, _, attempt) =
            exchange(root.path(), &mut evidence, &config(true, limit), events).await;
        let error = result
            .err()
            .expect("failed snapshot must never produce an accepted answer");
        let primary = error
            .downcast_ref::<HostProtocolError>()
            .expect("original typed failure");
        assert_eq!(primary.code(), expected_code, "{label}");
        assert_eq!(
            primary.codex_error_info,
            Some(crate::CodexErrorInfo::ContextWindowExceeded),
            "{label}"
        );
        assert_eq!(attempt.status, "failed", "{label}");
        assert_eq!(
            attempt.error.as_ref().unwrap()["code"],
            expected_code,
            "{label}"
        );
        assert!(!attempt.accounting_complete, "{label}");
        let report = attempt.native_ptc.unwrap();
        assert_eq!(
            report.failure_snapshot_accounting.as_deref(),
            projection,
            "{label}"
        );
        assert_eq!(report.native_tool_calls, expected_native, "{label}");
        assert_eq!(report.dynamic_requests_charged, expected_dynamic, "{label}");
        assert_eq!(
            report.combined_tool_calls,
            expected_native + expected_dynamic,
            "{label}"
        );
        assert_eq!(report.max_tool_calls, limit, "{label}");
        assert_eq!(report.dynamic_requests_denied, 0, "{label}");
        assert_eq!(
            report.budget_exceeded,
            expected_native + expected_dynamic > limit,
            "{label}"
        );
        assert_eq!(report.items.len(), expected_native, "{label}");
        assert!(report.items.len() <= limit + 1, "{label}");
        assert!(!report.successful_turn_completed_observed, "{label}");
        assert!(!report.pre_execution_guard, "{label}");
        assert!(
            report.items.iter().all(|item| item.final_snapshot_observed),
            "{label}"
        );
        assert_eq!(evidence.receipts.len(), expected_dynamic, "{label}");
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("private-"), "{label}");
        assert!(!encoded.contains("uninspected-third"), "{label}");
    }
}
