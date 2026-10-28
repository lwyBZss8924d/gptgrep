use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn fixture(root: &Path, name: &str) {
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
        root.join(name),
    )
    .unwrap();
}
fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("index")
        .arg(root)
        .args(args)
        .arg("--json")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CODEX_HOME")
        .output()
        .unwrap()
}
fn decoded(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
#[test]
fn fixed_public_pdf_image_plan_is_zero_model_and_unpublished() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path(), "rich-mixed.pdf");
    fixture(tmp.path(), "rich-scan.png");
    let output = invoke(tmp.path(), &["--ocr", "auto", "--plan-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = decoded(&output);
    assert_eq!(plan["schema_version"], "gptgrep.rich-ingest.v1");
    assert_eq!(plan["status"], "planned_unpublished");
    assert_eq!(plan["model_calls_in_plan"], 0);
    assert_eq!(plan["documents"].as_array().unwrap().len(), 2);
    assert_eq!(plan["required_pages"], 2);
    assert!(!tmp.path().join(".gptgrep/CURRENT.json").exists());
    assert!(!tmp.path().join(".gptgrep/ingest-runs").exists());
}
#[test]
fn fixed_public_native_pdf_indexes_through_normal_rich_cli_and_batch() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path(), "rich-native.pdf");
    let output = invoke(tmp.path(), &["--ocr", "off"]);
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = decoded(&output);
    assert_eq!(report["status"], "published");
    assert_eq!(report["new_ocr_executions"], 0);
    assert_eq!(report["documents"][0]["pages"].as_array().unwrap().len(), 2);
    let input = tmp.path().join("batch.json");
    fs::write(&input,serde_json::to_vec(&json!({"schema_version":"gptgrep.batch.rich.request.v1","operations":[{"id":"documents","op":"inspect_documents"}]})).unwrap()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["batch", "--rich", "--root"])
        .arg(tmp.path())
        .arg("--input")
        .arg(input)
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(decoded(&result)["items"][0]["status"], "ok");
}
#[test]
fn explicit_ocr_off_blocks_required_pages_without_publication() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path(), "rich-scan.png");
    let output = invoke(tmp.path(), &["--ocr", "off"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(decoded(&output)["status"], "blocked_unpublished");
    assert!(!tmp.path().join(".gptgrep/CURRENT.json").exists());
}
#[test]
#[cfg(unix)]
fn failed_office_conversion_has_structured_error_and_preserves_current() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path(), "rich-native.pdf");
    let baseline = invoke(tmp.path(), &["--ocr", "off"]);
    assert!(
        baseline.status.success(),
        "status={} stdout={} stderr={}",
        baseline.status,
        String::from_utf8_lossy(&baseline.stdout),
        String::from_utf8_lossy(&baseline.stderr)
    );
    let current = tmp.path().join(".gptgrep/CURRENT.json");
    let before = fs::read(&current).unwrap();
    fs::write(tmp.path().join("failure.docx"), b"mock Office source").unwrap();
    let tools = tempfile::tempdir().unwrap();
    let converter = tools.path().join("libreoffice");
    // Exercise normal converter discovery with a deterministic local failure.
    fs::write(
        &converter,
        b"#!/bin/sh\nprintf 'private child diagnostic\\n' >&2\nexit 23\n",
    )
    .unwrap();
    fs::set_permissions(&converter, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("index")
        .arg(tmp.path())
        .args(["--ocr", "auto", "--plan-only", "--json"])
        .env("PATH", tools.path())
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report = decoded(&output);
    assert_eq!(report["schema_version"], "gptgrep.error.v1");
    assert_eq!(report["ok"], false);
    assert_eq!(report["code"], "gptgrep_error");
    assert_eq!(report["error"], "Office conversion failed");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private child diagnostic"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private child diagnostic"));
    assert_eq!(fs::read(current).unwrap(), before);
    assert!(!tmp.path().join(".gptgrep/ingest-runs").exists());
}
#[test]
fn legacy_index_stays_local_and_live_auto_refuses_missing_runtime_before_clock() {
    let plain = tempfile::tempdir().unwrap();
    fs::write(plain.path().join("document.md"), "Plain source text\n").unwrap();
    assert!(invoke(plain.path(), &[]).status.success());
    let current: Value =
        serde_json::from_slice(&fs::read(plain.path().join(".gptgrep/CURRENT.json")).unwrap())
            .unwrap();
    assert_eq!(current["schema_version"], "gptgrep.v1");
    let rich = tempfile::tempdir().unwrap();
    fixture(rich.path(), "rich-scan.png");
    let output = invoke(
        rich.path(),
        &["--ocr", "auto", "--ocr-run-id", "unqualified"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!rich.path().join(".gptgrep/ingest-runs").exists());
    assert!(!rich.path().join(".gptgrep/CURRENT.json").exists());
}
