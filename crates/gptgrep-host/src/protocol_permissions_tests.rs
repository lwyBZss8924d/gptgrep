//! In-memory ordinary-host protocol fixtures; no runtime or filesystem work.
use super::*;

fn thread_response() -> Value {
    json!({"thread":{"id":"profile-thread"},"model":crate::DEFAULT_MODEL,
        "modelProvider":"openai","reasoningEffort":"max","serviceTier":"priority",
        "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}})
}

fn named_workspace_projection() -> Value {
    // Actual 0.162.1 named-profile compatibility shape, not a legacy grant.
    json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
        "excludeTmpdirEnvVar":true,"excludeSlashTmp":true})
}

async fn exchange(selected: Option<&str>, response: Value) -> (Result<Outcome>, Vec<Value>) {
    let config = HostConfig {
        permission_profile: selected.map(str::to_owned),
        ..HostConfig::default()
    };
    let replies = [
        json!({"id":1,"result":{}}),
        json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        json!({"id":2,"result":{"config":{}}}),
        json!({"id":3,"result":response}),
        json!({"id":4,"result":{"turn":{"id":"profile-turn","status":"inProgress"}}}),
        json!({"method":"turn/completed","params":{"threadId":"profile-thread",
            "turn":{"id":"profile-turn","status":"completed","error":null,
                "items":[{"type":"agentMessage","phase":"final_answer","text":"{}"}]}}}),
    ];
    let input = replies
        .iter()
        .map(|value| format!("{value}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    let state = json!({});
    let schema = json!({"type":"object","properties":{},"additionalProperties":false});
    let result = run(
        BufReader::new(input.as_bytes()),
        &mut output,
        Path::new("/"),
        &config,
        Workflow::Completion {
            instructions: "Return an empty object.",
            state: &state,
            schema: &schema,
        },
        None,
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
    &sent
        .iter()
        .find(|message| message["method"] == method)
        .unwrap()["params"]
}

#[tokio::test]
async fn named_profile_uses_permissions_without_legacy_overrides() {
    for sandbox in [
        json!({"type":"readOnly","networkAccess":false}),
        named_workspace_projection(),
        json!({"type":"workspaceWrite","writableRoots":["/work/output","/data/derived"],
            "networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":true}),
        json!({"type":"workspaceWrite","writableRoots":["/work/output"],
            "networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}),
    ] {
        let mut response = thread_response();
        response["sandbox"] = sandbox.clone();
        response["activePermissionProfile"] = json!({"id":"gptgrep_reader","extends":null});
        let (result, sent) = exchange(Some("gptgrep_reader"), response).await;
        let outcome = result.unwrap();
        assert_eq!(
            outcome.permission_observation,
            Some(json!({
                "source":"native_thread_start","permission_profile":"gptgrep_reader",
                "sandbox":sandbox,"granular_enforcement_verified":false
            }))
        );
        assert_eq!(
            request(&sent, "initialize")["capabilities"]["experimentalApi"],
            true
        );
        for method in ["thread/start", "turn/start"] {
            let params = request(&sent, method);
            assert_eq!(params["permissions"], "gptgrep_reader");
            assert_eq!(params["approvalPolicy"], "never");
            assert!(params.get("sandbox").is_none());
            assert!(params.get("sandboxPolicy").is_none());
        }
        let thread = request(&sent, "thread/start");
        assert_eq!(thread["config"]["features.shell_tool"], false);
        assert_eq!(thread["config"]["features.view_image"], false);
        assert_eq!(thread["dynamicTools"], json!([]));
    }
}

#[tokio::test]
async fn missing_or_wrong_profile_acknowledgment_refuses_before_turn() {
    for active in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"id":"different"})),
        Some(json!({"id":17})),
    ] {
        let mut response = thread_response();
        if let Some(active) = active {
            response["activePermissionProfile"] = active;
        }
        let (result, sent) = exchange(Some("gptgrep_reader"), response).await;
        let error = result.err().expect("unacknowledged profile must fail");
        assert_eq!(
            error.to_string(),
            "host_codex_permission_profile_unacknowledged"
        );
        assert!(sent.iter().all(|message| message["method"] != "turn/start"));
    }
}

#[tokio::test]
async fn named_profile_still_requires_selected_approval_before_turn() {
    let mut response = thread_response();
    response["activePermissionProfile"] = json!({"id":"gptgrep_reader"});
    response["approvalPolicy"] = json!("on-request");
    let (result, sent) = exchange(Some("gptgrep_reader"), response).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_codex_approval_policy_rejected"
    );
    assert!(sent.iter().all(|message| message["method"] != "turn/start"));
}

#[tokio::test]
async fn default_profile_preserves_legacy_wire_and_needs_no_named_acknowledgment() {
    let (result, sent) = exchange(None, thread_response()).await;
    assert!(result.unwrap().permission_observation.is_none());
    let thread = request(&sent, "thread/start");
    assert_eq!(thread["sandbox"], "read-only");
    assert!(thread.get("permissions").is_none());
    assert!(thread.get("sandboxPolicy").is_none());
    let turn = request(&sent, "turn/start");
    assert_eq!(
        turn["sandboxPolicy"],
        json!({"type":"readOnly","networkAccess":false})
    );
    assert!(turn.get("permissions").is_none());
    assert!(turn.get("sandbox").is_none());
    assert_eq!(thread["approvalPolicy"], "never");
    assert_eq!(turn["approvalPolicy"], "never");

    for sandbox in [
        named_workspace_projection(),
        json!({"type":"readOnly","networkAccess":true}),
        json!({"type":"dangerFullAccess"}),
    ] {
        let mut projected = thread_response();
        projected["sandbox"] = sandbox;
        let (result, sent) = exchange(None, projected).await;
        assert_eq!(
            result
                .err()
                .expect("default must retain read-only network-disabled policy")
                .to_string(),
            "host_codex_sandbox_rejected"
        );
        assert!(sent.iter().all(|message| message["method"] != "turn/start"));
    }
}
