use serde_json::{Value, json};
use std::{fs, process::Command};
fn native() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gptgrep"))
}
fn control() -> Value {
    json!({"schema_version":"gptgrep.ocr.serve.v1","source_root":"raw","prepared_root":"prepared",
    "spool_root":"spool","claim_root":"claims","raw_results_dir":"results","codex_home":"actor",
    "runtime_profile":"never-read-profile.json","compatibility_profile":"never-read-compat.json","phase":"never-read-phase.json"})
}
fn failed(output: std::process::Output, code: &str) -> Value {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], "gptgrep.error.v1");
    assert_eq!(report["code"], code);
    report
}
#[test]
fn serve_contract_is_native_only_and_pre_effect_errors_are_structured_without_json_flag() {
    let output = native().arg("--schema").output().unwrap();
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(schema["commands"]["ocr"]["native_cli_only"], true);
    assert_eq!(schema["commands"]["ocr"]["default_bind"], "127.0.0.1:8828");
    assert_eq!(
        schema["commands"]["ocr"]["defaults"]["native_model"],
        "gpt-6-luna"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    let mut invalid = control();
    invalid["bind"] = "0.0.0.0:8828".into();
    invalid["not_a_token_field"] = "private-fixture-not-echoed".into();
    fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
    let report = failed(
        native()
            .args(["ocr", "serve", "--config"])
            .arg(&path)
            .output()
            .unwrap(),
        "ocr_serve_control_invalid",
    );
    assert!(!report.to_string().contains("private-fixture"));
    assert!(!report.to_string().contains(dir.path().to_str().unwrap()));
    assert!(!dir.path().join("claims").exists());
    assert!(!dir.path().join("never-read-phase.json").exists());
}
#[test]
fn explicit_control_hash_and_token_preflight_prevent_phase_or_listener_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    fs::write(&path, serde_json::to_vec(&control()).unwrap()).unwrap();
    failed(
        native()
            .args(["ocr", "serve", "--config"])
            .arg(&path)
            .args(["--token-env", "HOME"])
            .output()
            .unwrap(),
        "ocr_serve_token_env_invalid",
    );
    failed(
        native()
            .args(["ocr", "serve", "--config"])
            .arg(&path)
            .args(["--expected-control-sha256", &"a".repeat(64)])
            .output()
            .unwrap(),
        "ocr_serve_control_digest_mismatch",
    );
    failed(
        native()
            .args(["ocr", "serve", "--config"])
            .arg(&path)
            .args(["--token-env", "OCR_SYNTHETIC_CONTROL_TOKEN"])
            .env_remove("OCR_SYNTHETIC_CONTROL_TOKEN")
            .output()
            .unwrap(),
        "ocr_serve_token_unavailable",
    );
    let secret = "synthetic-value-no-private-account";
    let output = native()
        .args(["ocr", "serve", "--config"])
        .arg(&path)
        .args(["--token-env", "OCR_SYNTHETIC_CONTROL_TOKEN"])
        .env("OCR_SYNTHETIC_CONTROL_TOKEN", secret)
        .output()
        .unwrap();
    let report = failed(output, "ocr_serve_prepare_failed");
    assert!(!report.to_string().contains(secret));
    assert!(!dir.path().join("claims").exists());
    assert!(!dir.path().join("spool").exists());
    assert!(!dir.path().join("never-read-phase.json").exists());
}
