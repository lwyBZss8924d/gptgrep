use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn invoke(root: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["batch", "--rich", "--root"])
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

fn error(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.len() <= 4096);
    assert_ne!(output.stdout.last(), Some(&b'\n'));
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["schema_version"], "gptgrep.batch.rich.response.v1");
    assert_eq!(response["execution"]["model_submissions"], 0);
    response
}

#[test]
fn stdin_rich_request_reaches_generation_open_without_model_context() {
    let root = tempfile::tempdir().unwrap();
    let input = serde_json::to_vec(&json!({
        "schema_version":"gptgrep.batch.rich.request.v1",
        "operations":[{"id":"catalog","op":"inspect_documents"}]
    }))
    .unwrap();
    let response = error(&invoke(root.path(), &input));
    assert_eq!(response["error"]["code"], "shared_integrity");
    assert!(response["generation"].is_null());
    assert!(response["read_metrics"].is_null());
}

#[test]
fn visual_operations_reach_the_same_local_pinned_generation_frontdoor() {
    let root = tempfile::tempdir().unwrap();
    for operation in [
        json!({"id":"visual","op":"inspect_visual","scope":{},"query":"generic","max_candidates":1}),
        json!({"id":"image","op":"read_visual","rendition_id":"source-bound-rendition"}),
    ] {
        let input = serde_json::to_vec(&json!({
            "schema_version":"gptgrep.batch.rich.request.v1", "operations":[operation]
        }))
        .unwrap();
        let response = error(&invoke(root.path(), &input));
        // Accepted operation schema, then refusal because no generation exists.
        assert_eq!(response["error"]["code"], "shared_integrity");
        assert!(response["generation"].is_null());
    }
    let input = serde_json::to_vec(&json!({
        "schema_version":"gptgrep.batch.rich.request.v1",
        "operations":[{"id":"image","op":"read_visual","rendition_id":"synthetic","cursor":"not-an-image-window"}]
    })).unwrap();
    assert_eq!(
        error(&invoke(root.path(), &input))["error"]["code"],
        "invalid_request"
    );
}

#[test]
fn rich_schema_and_output_admission_happen_before_generation_open() {
    let root = tempfile::tempdir().unwrap();
    for (input, expected) in [
        (
            json!({"schema_version":"gptgrep.batch.request.v1","operations":[{"id":"x","op":"catalog"}]}),
            "invalid_request",
        ),
        (
            json!({"schema_version":"gptgrep.batch.rich.request.v1","budget":{"max_output_bytes":5631},"operations":[{"id":"x","op":"inspect_documents"}]}),
            "output_budget_insufficient",
        ),
    ] {
        let response = error(&invoke(root.path(), &serde_json::to_vec(&input).unwrap()));
        assert_eq!(response["error"]["code"], expected);
        assert_eq!(response["items"], json!([]));
    }
}

#[test]
fn unreadable_rich_input_is_json_with_no_private_path() {
    let root = tempfile::tempdir().unwrap();
    for json_flag in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gptgrep"));
        command
            .args(["batch", "--rich", "--input"])
            .arg(root.path().join("absent-private-input"))
            .env_remove("OPENROUTER_API_KEY");
        if json_flag {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        let response = error(&output);
        assert_eq!(response["error"]["code"], "input_io_failure");
        assert!(response["generation"].is_null());
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("absent-private-input")
        );
    }
}

#[test]
fn discovery_describes_rich_schema_and_bounded_search_window() {
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("--schema")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rich = &schema["commands"]["batch"]["rich"];
    assert_eq!(rich["request_schema"], "gptgrep.batch.rich.request.v1");
    assert!(
        rich["network"]
            .as_str()
            .unwrap()
            .contains("search_multimodal")
    );
    assert!(
        rich["model_submissions"]
            .as_str()
            .unwrap()
            .contains("paid:null")
    );
    assert!(
        rich["operations"]
            .as_array()
            .unwrap()
            .contains(&json!("inspect_visual"))
    );
    assert!(
        rich["operations"]
            .as_array()
            .unwrap()
            .contains(&json!("read_visual"))
    );
    assert!(
        rich["visual_authority"]
            .as_str()
            .unwrap()
            .contains("actual_image_delivery=false")
    );
    assert!(
        rich["visual_admission"]
            .as_str()
            .unwrap()
            .contains("before rendition byte reads")
    );
    assert!(
        rich["search_continuation"]
            .as_str()
            .unwrap()
            .contains("max_hits")
    );
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("--llms")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("batch --rich"));
    assert!(help.contains("inspect_visual") && help.contains("read_visual"));
    assert!(help.contains("citation_authority=false"));
}

#[test]
fn paid_operation_uses_rich_schema_and_rejects_ambiguous_input_before_io() {
    let root = tempfile::tempdir().unwrap();
    let input = serde_json::to_vec(&json!({
        "schema_version":"gptgrep.batch.rich.request.v1",
        "operations":[{"id":"rank","op":"search_multimodal","query":"ordinary source evidence"}]
    }))
    .unwrap();
    assert_eq!(
        error(&invoke(root.path(), &input))["error"]["code"],
        "shared_integrity"
    );
    let input=br#"{"schema_version":"gptgrep.batch.rich.request.v1","operations":[{"id":"rank","op":"search_multimodal","query":"first","query":"second"}]}"#;
    assert_eq!(
        error(&invoke(root.path(), input))["error"]["code"],
        "invalid_request"
    );
    let input=serde_json::to_vec(&json!({
        "schema_version":"gptgrep.batch.rich.request.v1",
        "operations":[{"id":"rank","op":"search_multimodal","query":"ordinary","cursor":"forbidden"}]
    })).unwrap();
    assert_eq!(
        error(&invoke(root.path(), &input))["error"]["code"],
        "invalid_request"
    );
}
