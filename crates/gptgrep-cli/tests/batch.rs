use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn invoke(root: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("batch")
        .arg("--root")
        .arg(root)
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn index(root: &std::path::Path) {
    let result = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("index")
        .arg(root)
        .arg("--json")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
}

fn request(operations: Value) -> Value {
    json!({"schema_version":"gptgrep.batch.request.v1","operations":operations})
}

#[test]
fn stdin_batch_keeps_order_and_isolates_item_errors() {
    let corpus = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("guide.txt"), "needle evidence\n").unwrap();
    index(corpus.path());
    let input = request(json!([
        {"id":"first","op":"search","query":"needle","mode":"regex"},
        {"id":"bad","op":"search","query":"[","mode":"regex"},
        {"id":"last","op":"catalog","fields":["path"]}
    ]));
    let result = invoke(corpus.path(), &serde_json::to_vec(&input).unwrap());
    assert_eq!(result.status.code(), Some(2));
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(response["schema_version"], "gptgrep.batch.response.v1");
    assert_eq!(response["items"][0]["id"], "first");
    assert_eq!(response["items"][0]["status"], "ok");
    assert_eq!(
        response["items"][0]["result"]["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(response["items"][1]["id"], "bad");
    assert_eq!(response["items"][1]["error"]["code"], "invalid_regex");
    assert_eq!(response["items"][2]["id"], "last");
    assert_eq!(response["items"][2]["status"], "ok");
    assert_eq!(response["metrics"]["model_submissions"], 0);
    assert!(result.stdout.len() <= 1_048_576);
}

#[test]
fn exact_minimum_output_budget_is_not_changed_by_cli_framing() {
    let corpus = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("guide.txt"), "needle\n").unwrap();
    index(corpus.path());
    let mut input = request(json!([{"id":"catalog","op":"catalog","fields":["path"]}]));
    input["budget"] = json!({"max_output_bytes":5120});
    let result = invoke(corpus.path(), &serde_json::to_vec(&input).unwrap());
    assert!(result.stdout.len() <= 5120);
    assert_ne!(result.stdout.last(), Some(&b'\n'));
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    // Minimum admission reserves an item envelope, not an arbitrary result.
    // Complete reader metrics also count against its serialized output share.
    assert_eq!(response["items"].as_array().unwrap().len(), 1);
    assert!(response.get("error").is_none());
    if result.status.success() {
        assert_eq!(response["items"][0]["status"], "ok");
    } else {
        assert_eq!(result.status.code(), Some(2));
        assert_eq!(response["items"][0]["status"], "error");
        assert!(response["items"][0].get("result").is_none());
        assert_eq!(
            response["items"][0]["error"]["code"],
            "output_budget_insufficient"
        );
    }
    input["budget"]["max_output_bytes"] = json!(6144);
    let complete = invoke(corpus.path(), &serde_json::to_vec(&input).unwrap());
    assert!(complete.status.success(), "{:?}", complete);
    assert!(complete.stdout.len() <= 6144);
    assert_ne!(complete.stdout.last(), Some(&b'\n'));
    let response: Value = serde_json::from_slice(&complete.stdout).unwrap();
    assert_eq!(response["items"][0]["status"], "ok");
    input["budget"]["max_output_bytes"] = json!(5119);
    let rejected = invoke(corpus.path(), &serde_json::to_vec(&input).unwrap());
    assert_eq!(rejected.status.code(), Some(2));
    assert!(rejected.stdout.len() <= 4096);
    let error: Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(error["error"]["code"], "output_budget_insufficient");
}

#[test]
fn explicit_pin_survives_new_current_and_model_work_refuses_preflight() {
    let corpus = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("guide.txt"), "needle\n").unwrap();
    index(corpus.path());
    let basic = request(json!([{"id":"catalog","op":"catalog"}]));
    let first = invoke(corpus.path(), &serde_json::to_vec(&basic).unwrap());
    assert!(first.status.success());
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    index(corpus.path());
    let mut retained = basic;
    retained["generation"] = first["generation"].clone();
    let second = invoke(corpus.path(), &serde_json::to_vec(&retained).unwrap());
    assert!(second.status.success());
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["generation"], first["generation"]);
    let model = request(json!([{"id":"model","op":"search","query":"needle","mode":"hybrid"}]));
    let result = invoke(corpus.path(), &serde_json::to_vec(&model).unwrap());
    assert_eq!(result.status.code(), Some(2));
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        response["items"][0]["error"]["code"],
        "output_budget_insufficient"
    );
    assert_eq!(response["metrics"]["model_submissions"], 0);
}

#[test]
fn malformed_and_oversized_input_produce_bounded_global_json_errors() {
    let root = tempfile::tempdir().unwrap();
    let duplicate = request(json!([{"id":"same","op":"catalog"},{"id":"same","op":"catalog"}]));
    for (input, code) in [
        (
            serde_json::to_vec(&duplicate).unwrap(),
            "duplicate_operation_id",
        ),
        (vec![b' '; 1_048_577], "input_too_large"),
    ] {
        let result = invoke(root.path(), &input);
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.len() <= 4096);
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["error"]["code"], code);
        assert_eq!(response["items"], json!([]));
    }
}

#[test]
fn unreadable_input_always_uses_batch_json_schema() {
    let root = tempfile::tempdir().unwrap();
    for json_flag in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gptgrep"));
        command
            .arg("batch")
            .arg("--input")
            .arg(root.path().join("absent-private-path"));
        if json_flag {
            command.arg("--json");
        }
        let output = command.env_remove("OPENROUTER_API_KEY").output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["schema_version"], "gptgrep.batch.response.v1");
        assert_eq!(response["error"]["code"], "input_io_failure");
        assert!(output.stdout.len() <= 4096);
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("absent-private-path")
        );
    }
}

#[test]
fn stale_search_is_an_item_failure_and_later_items_continue() {
    let corpus = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("guide.txt"), "needle\n").unwrap();
    index(corpus.path());
    std::fs::write(corpus.path().join("guide.txt"), "changed original\n").unwrap();
    for mode in ["regex", "lexical"] {
        let input = request(json!([
            {"id":"stale","op":"search","query":"needle","mode":mode,"document":"guide.txt"},
            {"id":"later","op":"catalog"}
        ]));
        let result = invoke(corpus.path(), &serde_json::to_vec(&input).unwrap());
        assert_eq!(result.status.code(), Some(2));
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["items"][0]["error"]["code"], "source_stale");
        assert!(
            !response["items"][0]["result"]["coverage"]["stale_files"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(response["items"][1]["status"], "ok");
        assert_eq!(response["metrics"]["model_submissions"], 0);
    }
}
