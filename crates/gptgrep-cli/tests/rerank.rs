use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Output, Stdio},
};
fn invoke(input: &[u8]) -> Output {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["rerank", "--root"])
        .arg(root.path())
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn request() -> Value {
    json!({"schema_version":"gptgrep.rich_rerank.request.v1","query":"ordinary question",
    "candidates":[{"id":"selected","evidence":{"kind":"native_text","binding":{
        "generation":{"generation":"fixed","manifest_sha256":"a".repeat(64)},"document_id":"document",
        "source_path":"document.pdf","source_sha256":"b".repeat(64),"artifact_id":"artifact",
        "artifact_sha256":"c".repeat(64),"page":1,"byte_start":0,"byte_end":12,"line":1}}}]})
}
fn failure(output: Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.len() <= 4096);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], "gptgrep.rich_rerank.response.v1");
    assert!(value["report"].is_null());
    value
}
#[test]
fn valid_source_only_request_reaches_generation_open_without_provider() {
    assert_eq!(
        failure(invoke(&serde_json::to_vec(&request()).unwrap()))["code"],
        "shared_integrity"
    );
}
#[test]
fn rerank_refuses_injected_content_duplicates_and_cursor_before_io() {
    for field in ["text", "score", "image_url", "cursor", "depends_on"] {
        let mut r = request();
        r["candidates"][0][field] = json!("untrusted");
        assert_eq!(
            failure(invoke(&serde_json::to_vec(&r).unwrap()))["code"],
            "invalid_request"
        );
    }
    let mut r = request();
    let duplicate = r["candidates"][0].clone();
    r["candidates"].as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        failure(invoke(&serde_json::to_vec(&r).unwrap()))["code"],
        "invalid_request"
    );
    assert_eq!(
        failure(invoke(br#"{"schema_version":"a","schema_version":"b"}"#))["code"],
        "invalid_request"
    );
}
#[test]
fn rerank_input_output_bounds_are_admitted_before_generation_open() {
    let mut r = request();
    r["budget"] = json!({"max_output_bytes":4095});
    assert_eq!(
        failure(invoke(&serde_json::to_vec(&r).unwrap()))["code"],
        "invalid_request"
    );
    assert_eq!(
        failure(invoke(&vec![b' '; 1_048_577]))["code"],
        "invalid_request"
    );
}
#[test]
fn rerank_missing_input_is_bounded_json_without_path_or_json_flag() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["rerank", "--input"])
        .arg(root.path().join("private-absent-input"))
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-absent-input"));
    assert_eq!(failure(output)["code"], "input_io_failure");
}
#[test]
fn rerank_discovery_retains_search_routing_and_describes_source_boundary() {
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("--schema")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["commands"]["rerank"]["request_schema"],
        "gptgrep.rich_rerank.request.v1"
    );
    assert!(
        value["commands"]["batch"]["rich"]["operations"]
            .as_array()
            .unwrap()
            .contains(&json!("rerank"))
    );
    assert!(
        value["commands"]["batch"]["rich"]["search_multimodal"]["effect"]
            .as_str()
            .unwrap()
            .contains("mandatory document routing")
    );
}
