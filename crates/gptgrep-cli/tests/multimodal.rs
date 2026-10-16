//! Normal CLI preflight checks; no native runtime or provider is launched.
use serde_json::Value;
use std::{fs, process::Command};

fn invoke(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(arguments)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CODEX_HOME")
        .output()
        .unwrap()
}
fn error(output: &std::process::Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn multimodal_rejects_v1_before_budget_key_or_native_work_and_preserves_legacy_search() {
    let corpus = tempfile::tempdir().unwrap();
    fs::write(corpus.path().join("manual.txt"), "retention is 37 days\n").unwrap();
    let root = corpus.path().to_str().unwrap();
    assert!(invoke(&["index", root, "--json"]).status.success());
    for command in ["search", "ask"] {
        let output = invoke(&[
            command,
            "retention",
            root,
            "--multimodal",
            "--decisions-budget",
            "missing-budget.json",
            "--json",
        ]);
        let failure = error(&output);
        assert!(failure["error"].as_str().unwrap().contains("manifest v2"));
        assert!(
            !failure["error"]
                .as_str()
                .unwrap()
                .contains("OPENROUTER_API_KEY")
        );
        assert!(failure.get("answer").is_none() && failure.get("hits").is_none());
    }
    let legacy = invoke(&["search", "retention", root, "--mode", "regex", "--json"]);
    assert!(legacy.status.success());
    let report: Value = serde_json::from_slice(&legacy.stdout).unwrap();
    assert_eq!(report["schema_version"], "gptgrep.v1");
    assert_eq!(report["hits"].as_array().unwrap().len(), 1);
    assert_eq!(report["metrics"]["jev_calls_attempted"], 0);
}

#[test]
fn incompatible_modes_bounds_and_host_profiles_fail_before_generation_or_account_reads() {
    for options in [
        vec!["--mode", "regex"],
        vec!["--mode", "lexical"],
        vec!["--mode", "semantic"],
        vec!["--window-bytes", "3"],
        vec!["--max-visual-candidates", "17"],
        vec!["--max-concurrent-batches", "5"],
        vec!["--max-output-bytes", "1048577"],
        vec!["--max-metadata-bytes", "524289"],
        vec!["--max-image-bytes", "1048577"],
        vec!["--max-image-pixels", "8000001"],
    ] {
        let mut args = vec![
            "search",
            "chart",
            "missing-corpus",
            "--multimodal",
            "--json",
        ];
        args.extend(options);
        let failure = error(&invoke(&args));
        let message = failure["error"].as_str().unwrap();
        assert!(!message.contains("No such file"), "{message}");
        assert!(!message.contains("OPENROUTER_API_KEY"), "{message}");
    }
    for options in [
        vec!["--experimental-query-plan"],
        vec!["--model", "gpt-5.6-luna"],
        vec!["--reasoning-effort", "high"],
        vec!["--service-tier", "priority"],
        vec!["--max-citations", "9"],
    ] {
        let mut args = vec![
            "ask",
            "chart",
            "missing-corpus",
            "--multimodal",
            "--json",
            "--codex-home",
            "missing-account",
            "--codex-bin",
            "missing-native",
            "--decisions-budget",
            "missing-budget",
        ];
        args.extend(options);
        let failure = error(&invoke(&args));
        let message = failure["error"].as_str().unwrap();
        assert!(message.contains("multimodal"), "{message}");
        assert!(!message.contains("No such file"), "{message}");
    }
}

#[test]
fn local_schema_and_manifest_describe_distinct_exposure_and_delivery_without_budget_work() {
    let schema = invoke(&["--schema", "--decisions-budget", "missing-budget", "--json"]);
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(
        schema["commands"]["search"]["options"]["multimodal"]["citation_authority"],
        false
    );
    assert_eq!(
        schema["commands"]["ask"]["options"]["multimodal"]["ask_schema"],
        "gptgrep.rich_multimodal.host.v1"
    );
    assert_eq!(
        schema["commands"]["ask"]["options"]["max-image-pixels"]["maximum"],
        8_000_000
    );
    let manifest = invoke(&["--llms", "--decisions-budget", "missing-budget"]);
    assert!(manifest.status.success());
    let text = String::from_utf8(manifest.stdout).unwrap();
    assert!(text.contains("search QUERY ROOT --multimodal"));
    assert!(text.contains("model-exposure-only"));
    assert!(text.contains("actual complete reader delivery"));
}

#[test]
fn explicit_multimodal_failures_keep_structured_output_without_json_and_legacy_stays_text() {
    for args in [
        vec!["search", "chart", "missing-corpus", "--multimodal"],
        vec!["ask", "chart", "missing-corpus", "--multimodal"],
        vec![
            "search",
            "chart",
            "missing-corpus",
            "--multimodal",
            "--mode",
            "semantic",
        ],
        vec![
            "ask",
            "chart",
            "missing-corpus",
            "--multimodal",
            "--reasoning-effort",
            "high",
        ],
    ] {
        let implicit = invoke(&args);
        let failure = error(&implicit);
        assert_eq!(failure["schema_version"], "gptgrep.error.v1");
        assert_eq!(failure["ok"], false);
        assert!(failure["code"].is_string());
        assert!(failure["error"].is_string());
        assert!(implicit.stderr.is_empty());
        let mut explicit = args.clone();
        explicit.push("--json");
        assert_eq!(failure, error(&invoke(&explicit)));
    }
    let legacy = invoke(&["search", "chart", "missing-corpus", "--mode", "regex"]);
    assert_eq!(legacy.status.code(), Some(2));
    assert!(legacy.stdout.is_empty());
    assert!(String::from_utf8_lossy(&legacy.stderr).contains("gptgrep:"));
}
