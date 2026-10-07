use super::*;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    corpus: PathBuf,
    readers: PathBuf,
    options: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        fs::write(
            path.join("document.pdf"),
            b"%PDF-1.4\nsynthetic offline document: copper vessel\n%%EOF\n",
        )
        .unwrap();
        fs::write(
            path.join("native.txt"),
            b"synthetic offline document: copper vessel",
        )
        .unwrap();
        fs::write(
            path.join("tool"),
            b"#!/bin/sh\n# synthetic placeholder; never executed\nexit 75\n",
        )
        .unwrap();
        let raw = Asset {
            path: "document.pdf".into(),
            sha256: hash(&fs::read(path.join("document.pdf")).unwrap()),
        };
        let native = Asset {
            path: "native.txt".into(),
            sha256: hash(&fs::read(path.join("native.txt")).unwrap()),
        };
        let receipt = DerivationReceipt {
            schema_version: "1".into(),
            source_sha256: raw.sha256.clone(),
            artifact_sha256: native.sha256.clone(),
            derivation: "synthetic-source-only-v1".into(),
            query_agnostic: true,
            synthetic: true,
            preprocessing_latency_ms: 0,
            preprocessing_cost_usd: None,
        };
        fs::write(path.join("receipt.json"), json(&receipt).unwrap()).unwrap();
        let corpus = Corpus {
            schema_version: "1".into(),
            documents: vec![Document {
                id: "doc-a".into(),
                raw: raw.clone(),
                prebuilt: vec![Derived {
                    asset: native,
                    source_sha256: raw.sha256,
                    derivation: receipt.derivation,
                    query_agnostic: true,
                    receipt: Asset {
                        path: "receipt.json".into(),
                        sha256: hash(&fs::read(path.join("receipt.json")).unwrap()),
                    },
                }],
            }],
            tool: Tool {
                asset: Asset {
                    path: "tool".into(),
                    sha256: hash(&fs::read(path.join("tool")).unwrap()),
                },
                version: "synthetic".into(),
                source_revision: "synthetic".into(),
                synthetic: true,
            },
        };
        fs::write(path.join("corpus.json"), json(&corpus).unwrap()).unwrap();
        let question = "What material is named in the document?";
        let readers = Readers {
            schema_version: "1".into(),
            corpus_sha256: hash(&fs::read(path.join("corpus.json")).unwrap()),
            tasks: vec![Reader {
                id: "task-a".into(),
                question: question.into(),
                question_sha256: hash(question.as_bytes()),
                document_scope: vec!["doc-a".into()],
            }],
        };
        fs::write(path.join("readers.json"), json(&readers).unwrap()).unwrap();
        let options = Options {
            schema_version: "1".into(),
            reader_image: format!("example/reader@sha256:{}", "a".repeat(64)),
            verifier_image: format!("example/verifier@sha256:{}", "b".repeat(64)),
            codex_binary_sha256: "c".repeat(64),
            codex_argv: vec![
                "codex".into(),
                "exec".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
            ],
            model: "qualification-pending".into(),
            effort: "qualification-pending".into(),
            service_tier: "qualification-pending".into(),
            timeout_sec: 60,
            cpus: 1,
            memory_mb: 512,
            network_mode: "no-network".into(),
            repetitions: 2,
            seed: 0,
            synthetic: true,
        };
        fs::write(path.join("options.json"), json(&options).unwrap()).unwrap();
        Self {
            corpus: path.join("corpus.json"),
            readers: path.join("readers.json"),
            options: path.join("options.json"),
            _root: root,
        }
    }
    fn prepare(&self) -> Result<(BTreeMap<String, Vec<u8>>, Manifest)> {
        prepare(&self.corpus, &self.readers, &self.options, true, None, &[])
    }
    fn output(&self) -> PathBuf {
        self.corpus.parent().unwrap().join("output")
    }
    fn mutate(&self, path: &Path, change: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        change(&mut value);
        fs::write(path, json(&value).unwrap()).unwrap();
    }
}

#[test]
fn raw_only_baseline_and_identical_codex_settings() {
    let f = Fixture::new();
    let (files, manifest) = f.prepare().unwrap();
    assert_eq!(
        files["control/task-a/environment/raw/doc-a.pdf"],
        files["gptgrep/task-a/environment/raw/doc-a.pdf"]
    );
    assert_eq!(
        files["control/task-a/environment/codex-config.json"],
        files["gptgrep/task-a/environment/codex-config.json"]
    );
    assert!(
        !files
            .keys()
            .any(|p| p.starts_with("control/") && (p.contains("prebuilt/") || p.contains("tool/")))
    );
    assert!(
        files
            .keys()
            .any(|p| p.starts_with("gptgrep/task-a/environment/prebuilt/"))
    );
    assert!(!manifest.executable);
}
#[test]
fn reader_schema_refuses_reference_answer_field() {
    let f = Fixture::new();
    f.mutate(&f.readers, |v| {
        v["tasks"][0]["reference_answer"] = "DO_NOT_MOUNT".into()
    });
    assert!(f.prepare().unwrap_err().starts_with("schema:"));
}
#[test]
fn question_digest_mismatch_refused() {
    let f = Fixture::new();
    f.mutate(&f.readers, |v| v["tasks"][0]["question"] = "changed".into());
    assert_eq!(f.prepare().unwrap_err(), "reader_question_binding");
}
#[test]
fn source_digest_tampering_refused_before_output() {
    let f = Fixture::new();
    fs::write(
        f.corpus.parent().unwrap().join("document.pdf"),
        b"%PDF-1.4 modified",
    )
    .unwrap();
    assert_eq!(f.prepare().unwrap_err(), "asset_digest_mismatch");
    assert!(!f.output().exists());
}
#[test]
fn path_traversal_and_symlink_refused() {
    assert!(check_rel("../document.pdf").is_err());
    assert!(check_rel("/document.pdf").is_err());
    assert!(check_rel("a\\b").is_err());
    let f = Fixture::new();
    let parent = f.corpus.parent().unwrap();
    fs::remove_file(parent.join("native.txt")).unwrap();
    std::os::unix::fs::symlink("document.pdf", parent.join("native.txt")).unwrap();
    assert_eq!(f.prepare().unwrap_err(), "input_not_regular_or_symlink");
}
#[test]
fn intermediate_symlink_refused() {
    let f = Fixture::new();
    let parent = f.corpus.parent().unwrap();
    std::os::unix::fs::symlink(parent, parent.join("alias")).unwrap();
    assert_eq!(
        regular_bytes(&parent.join("alias/document.pdf"), MAX_FILE).unwrap_err(),
        "input_not_regular_or_symlink"
    );
}
#[test]
fn derivation_receipt_query_conditioning_refused() {
    let f = Fixture::new();
    f.mutate(&f.corpus, |v| {
        v["documents"][0]["prebuilt"][0]["query_agnostic"] = false.into()
    });
    f.mutate(&f.readers, |v| {
        v["corpus_sha256"] = hash(&fs::read(&f.corpus).unwrap()).into()
    });
    assert_eq!(f.prepare().unwrap_err(), "prebuilt_source_binding");
}
#[test]
fn duplicate_task_scope_and_unknown_document_refused() {
    let f = Fixture::new();
    f.mutate(&f.readers, |v| {
        v["tasks"][0]["document_scope"] = serde_json::json!(["doc-a", "doc-a"])
    });
    assert_eq!(f.prepare().unwrap_err(), "duplicate_scope");
    f.mutate(&f.readers, |v| {
        v["tasks"][0]["document_scope"] = serde_json::json!(["missing"])
    });
    assert_eq!(f.prepare().unwrap_err(), "unknown_document_scope");
}
#[test]
fn counterbalanced_repetitions_preserve_denominator() {
    let f = Fixture::new();
    let (files, _) = f.prepare().unwrap();
    let s: Vec<serde_json::Value> = serde_json::from_slice(&files["paired-schedule.json"]).unwrap();
    assert_eq!(s.len(), 4);
    assert_eq!(s[0]["arm"], "control");
    assert_eq!(s[2]["arm"], "gptgrep");
    assert!(s.iter().all(|v| v["executed"] == false));
}
#[test]
fn overwrite_refused_and_inventory_detects_added_file() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let (files, m) = f.prepare().unwrap();
    create_output(&f.output(), &files, &m).unwrap();
    assert_eq!(
        fs::metadata(f.output()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(f.output().join("export-manifest.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    validate(&f.output()).unwrap();
    assert_eq!(
        create_output(&f.output(), &files, &m).unwrap_err(),
        "output_must_be_new"
    );
    fs::write(f.output().join("extra.txt"), b"added").unwrap();
    assert_eq!(
        validate(&f.output()).unwrap_err(),
        "export_inventory_mismatch"
    );
}
#[test]
fn grader_export_is_separate_and_exact_reader_digest_bound() {
    let f = Fixture::new();
    let (files, m) = f.prepare().unwrap();
    create_output(&f.output(), &files, &m).unwrap();
    let graders = Graders {
        schema_version: "1".into(),
        corpus_sha256: m.corpus_sha256,
        reader_projection_sha256: m.reader_projection_sha256,
        reader_export_sha256: hash(&fs::read(f.output().join("export-manifest.json")).unwrap()),
        rubric_sha256: "d".repeat(64),
        judge_status: "unavailable".into(),
        tasks: vec![Grade {
            id: "task-a".into(),
            question_sha256: m.task_bindings[0].question_sha256.clone(),
            reference_answer: "JUDGE_ONLY_CANARY".into(),
        }],
    };
    let path = f.corpus.parent().unwrap().join("graders.json");
    fs::write(&path, json(&graders).unwrap()).unwrap();
    let (g, gm) = verifier(&path, &f.output()).unwrap();
    assert_eq!(gm.role, "grader");
    assert_eq!(
        gm.reader_export_sha256.as_deref(),
        Some(graders.reader_export_sha256.as_str())
    );
    let grader_output = f.corpus.parent().unwrap().join("grader-output");
    create_output(&grader_output, &g, &gm).unwrap();
    validate(&grader_output).unwrap();
    f.mutate(&grader_output.join("export-manifest.json"), |v| {
        v["reader_export_sha256"] = serde_json::Value::Null
    });
    assert_eq!(
        validate(&grader_output).unwrap_err(),
        "export_reader_binding"
    );
    assert!(
        String::from_utf8(g["task-a/reference.json"].clone())
            .unwrap()
            .contains("JUDGE_ONLY_CANARY")
    );
    assert!(
        !files
            .values()
            .any(|b| String::from_utf8_lossy(b).contains("JUDGE_ONLY_CANARY"))
    );
    f.mutate(&path, |v| v["reader_export_sha256"] = "e".repeat(64).into());
    assert_eq!(
        verifier(&path, &f.output()).unwrap_err(),
        "grader_binding_or_qualification"
    );
}
#[test]
fn docker_image_newline_injection_refused() {
    let f = Fixture::new();
    f.mutate(&f.options, |v| {
        v["reader_image"] = format!("example\nRUN false@sha256:{}", "a".repeat(64)).into()
    });
    assert_eq!(f.prepare().unwrap_err(), "image_name_injection");
}

#[test]
fn subset_selection_and_noncanonical_options_bytes_remain_bound() {
    let f = Fixture::new();
    let options: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.options).unwrap()).unwrap();
    fs::write(&f.options, serde_json::to_vec(&options).unwrap()).unwrap();
    let (files, m) = prepare(
        &f.corpus,
        &f.readers,
        &f.options,
        true,
        Some(1),
        &["task-a".into()],
    )
    .unwrap();
    assert_eq!(m.task_bindings.len(), 1);
    assert_eq!(hash(&files["frozen-options.json"]), m.options_sha256);
    create_output(&f.output(), &files, &m).unwrap();
    validate(&f.output()).unwrap();
    assert_eq!(
        prepare(
            &f.corpus,
            &f.readers,
            &f.options,
            true,
            None,
            &["unknown".into()]
        )
        .unwrap_err(),
        "selection_task_ids"
    );
    assert_eq!(
        prepare(&f.corpus, &f.readers, &f.options, true, Some(0), &[]).unwrap_err(),
        "selection_limit"
    );
}
