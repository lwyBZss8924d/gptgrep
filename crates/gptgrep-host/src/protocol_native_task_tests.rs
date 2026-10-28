//! Native structured-task protocol fixtures. In-memory RPC only; no model or process.
use super::*;

fn selected_config() -> HostConfig {
    HostConfig {
        native_ptc: true,
        permission_profile: Some("native_task_profile".into()),
        source_continuation: true,
        max_tool_calls: 128,
        native_capabilities: std::collections::BTreeMap::from([
            ("features.shell_tool".into(), json!(true)),
            ("features.view_image".into(), json!(true)),
        ]),
        ..HostConfig::default()
    }
}

fn command(status: &str) -> Value {
    json!({"type":"commandExecution","id":"task-command","status":status,
        "command":"synthetic-private-command","aggregatedOutput":"synthetic-private-output"})
}

fn event(method: &str, item: Value) -> Value {
    json!({"method":method,"params":{"threadId":"task-thread","turnId":"task-turn",
        "item":item}})
}

fn completed(answer: String, mut items: Vec<Value>) -> Value {
    items.push(
        json!({"type":"agentMessage","phase":"final_answer","id":"task-answer",
        "text":answer}),
    );
    json!({"method":"turn/completed","params":{"threadId":"task-thread",
        "turn":{"id":"task-turn","status":"completed","error":null,"items":items}}})
}

async fn exchange(
    config: &HostConfig,
    events: Vec<Value>,
    max_output_bytes: usize,
) -> (Result<Outcome>, Vec<Value>) {
    let mut replies = vec![
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{"mcp_servers":{"selected":{"enabled":true}}}}}),
        json!({"id":3,"result":{"thread":{"id":"task-thread"},"model":config.model,
            "modelProvider":"openai","reasoningEffort":config.reasoning_effort,
            "serviceTier":"priority","approvalPolicy":"never",
            "activePermissionProfile":{"id":"native_task_profile"},
            "sandbox":{"type":"workspaceWrite","writableRoots":["/selected/work"],
                "networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":true}}}),
        json!({"id":4,"result":{"turn":{"id":"task-turn","status":"inProgress"}}}),
    ];
    replies.extend(events);
    let input = replies
        .iter()
        .map(|frame| format!("{frame}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    let state = json!({"task":"synthetic structured native task"});
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"},
        "payload":{"type":"string"}},"required":["ok"],"additionalProperties":false});
    let result = run_observed(
        BufReader::new(input.as_bytes()),
        &mut output,
        Path::new("/selected/work"),
        config,
        Workflow::NativeTask {
            instructions: "Use the selected native capabilities and return the requested JSON.",
            state: &state,
            schema: &schema,
        },
        None,
        RunOptions {
            max_output_bytes: Some(max_output_bytes),
            ..RunOptions::default()
        },
    )
    .await;
    let sent = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (result, sent)
}

fn request<'a>(sent: &'a [Value], method: &str) -> &'a Value {
    &sent.iter().find(|frame| frame["method"] == method).unwrap()["params"]
}

#[tokio::test]
async fn native_task_preserves_caller_instructions_state_schema_cwd_and_capabilities() {
    let terminal = command("completed");
    let (result, sent) = exchange(
        &selected_config(),
        vec![
            event("item/started", command("inProgress")),
            event("item/completed", terminal.clone()),
            event("item/completed", json!({"type":"plan","id":"task-plan"})),
            completed(json!({"ok":true}).to_string(), vec![terminal]),
        ],
        4096,
    )
    .await;
    let outcome = result.unwrap();
    assert_eq!(outcome.answer, json!({"ok":true}).to_string());
    let observation = outcome.native_ptc.unwrap();
    assert_eq!(observation.native_tool_calls, 1);
    assert_eq!(observation.dynamic_requests_charged, 0);
    assert_eq!(observation.max_tool_calls, 128);
    assert!(observation.successful_turn_completed_observed);
    assert!(!observation.pre_execution_guard);
    let serialized = serde_json::to_string(&observation).unwrap();
    assert!(!serialized.contains("synthetic-private-command"));
    assert!(!serialized.contains("synthetic-private-output"));
    assert_eq!(
        outcome.permission_observation.as_ref().unwrap()["permission_profile"],
        "native_task_profile"
    );
    let thread = request(&sent, "thread/start");
    assert_eq!(thread["cwd"], "/selected/work");
    assert_eq!(
        thread["baseInstructions"],
        "Use the selected native capabilities and return the requested JSON."
    );
    assert_eq!(thread["dynamicTools"], json!([]));
    assert_eq!(thread["config"]["features.shell_tool"], true);
    assert_eq!(thread["config"]["features.view_image"], true);
    assert!(thread["config"].get("mcp_servers").is_none());
    let turn = request(&sent, "turn/start");
    assert_eq!(
        serde_json::from_str::<Value>(turn["input"][0]["text"].as_str().unwrap()).unwrap(),
        json!({"task":"synthetic structured native task"})
    );
    assert_eq!(turn["outputSchema"]["properties"]["ok"]["type"], "boolean");
    assert_eq!(turn["permissions"], "native_task_profile");
}

#[tokio::test]
async fn native_task_requires_explicit_native_selection_before_rpc() {
    let mut config = selected_config();
    config.native_ptc = false;
    let (result, sent) = exchange(&config, vec![], 4096).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_task_requires_native_ptc"
    );
    assert!(sent.is_empty());
}

#[tokio::test]
async fn native_task_does_not_register_or_dispatch_retrieval_dynamic_tools() {
    let call = json!({"id":91,"method":"item/tool/call","params":{
        "threadId":"task-thread","turnId":"task-turn","callId":"unregistered",
        "namespace":"gptgrep","tool":"gptgrep_catalog","arguments":{}}});
    let (result, sent) = exchange(&selected_config(), vec![call], 4096).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_native_task_unregistered_dynamic_tool"
    );
    assert!(
        sent.iter()
            .any(|frame| frame["id"] == 91 && frame.get("error").is_some())
    );
}

#[tokio::test]
async fn native_task_honors_the_caller_output_cap_beyond_completion_default() {
    let answer = json!({"ok":true,"payload":"x".repeat(130 * 1024)}).to_string();
    let (result, _) = exchange(
        &selected_config(),
        vec![completed(answer.clone(), vec![])],
        256 * 1024,
    )
    .await;
    assert_eq!(result.unwrap().answer, answer);
    let (result, _) = exchange(
        &selected_config(),
        vec![completed(answer, vec![])],
        64 * 1024,
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .downcast_ref::<crate::CompletionError>()
            .is_some()
    );
}

#[test]
fn native_neutral_metadata_items_require_native_mode() {
    for kind in [
        "hookPrompt",
        "functionCallOutput",
        "plan",
        "subAgentActivity",
        "enteredReviewMode",
        "exitedReviewMode",
        "contextCompaction",
    ] {
        let item = json!({"type":kind,"id":"neutral-item"});
        assert!(validate_workflow_item(&item, false, true).is_ok());
        assert!(validate_workflow_item(&item, false, false).is_err());
    }
}
