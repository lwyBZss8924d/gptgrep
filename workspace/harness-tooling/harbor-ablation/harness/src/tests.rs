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

// Genuine v2 Writer/Reader with generic MOCK parsed artifacts, never native extraction.
mod production {
    use super::*;
    use anyhow::{Result, ensure};
    use gptgrep_core::{RichGenerationReader, RichGenerationWriter};
    use gptgrep_parse::*;
    use serde_json::{Value, json};
    async fn parsed_fixture(
        root: &Path,
        texts: &[&str],
    ) -> Result<(tempfile::TempDir, RichParsedDocument)> {
        let staging = tempfile::tempdir()?;
        let staging_path = staging.path().canonicalize()?;
        let source = format!(
            "%PDF-1.4\nordinary MOCK source, never native parsed: {}\n%%EOF\n",
            texts.join("")
        );
        let source = source.as_bytes();
        fs::write(root.join("source.pdf"), source)?;
        fs::write(staging_path.join("prepared.pdf"), source)?;
        let sha = hash(source);
        let profile = hash(b"synthetic parser recipe");
        let mut artifacts = Vec::new();
        let mut pages = Vec::new();
        let mut create = |kind: ArtifactKind,
                          page: Option<u32>,
                          bytes: &[u8],
                          name: &str|
         -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &hash(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: hash(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".png") {
                    "image/png"
                } else if name.ends_with(".txt") {
                    "text/plain; charset=utf-8"
                } else {
                    "application/json"
                }
                .into(),
                page,
                region_id: None,
            };
            fs::write(staging_path.join(name), bytes)?;
            artifacts.push(artifact.clone());
            Ok(artifact)
        };
        for (index, text) in texts.iter().enumerate() {
            let number = index as u32 + 1;
            let native_text = create(
                ArtifactKind::NativeText,
                Some(number),
                text.as_bytes(),
                &format!("p{number}-native.txt"),
            )?;
            let geometry = PageGeometry {
                width_points: 100.0,
                height_points: 200.0,
                rotation_quarter_turns: 0,
                user_unit: 1.0,
                source_to_page: AffineTransform::IDENTITY,
                page_to_source: AffineTransform::IDENTITY,
            };
            let complexity = serde_json::from_value(
                json!({"page_number":number,"text_length":text.len(),"text_coverage":0.5,
            "has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,
            "full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":20000.0,
            "needs_ocr":false,"reasons":[],"layout":null}),
            )?;
            let capabilities =
                BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
            let metadata_artifact = create(
                ArtifactKind::NativeMetadata,
                Some(number),
                &serde_json::to_vec(
                    &json!({"page":{"page_number":number,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
                "capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]}),
                )?,
                &format!("p{number}-native.json"),
            )?;
            let mut pixels = Vec::new();
            use image::{ExtendedColorType, ImageEncoder};
            image::codecs::png::PngEncoder::new(&mut pixels).write_image(
                &[255; 8 * 16 * 4],
                8,
                16,
                ExtendedColorType::Rgba8,
            )?;
            let rendition = create(
                ArtifactKind::Rendition,
                Some(number),
                &pixels,
                &format!("p{number}.png"),
            )?;
            let image = OcrImage {
                artifact: rendition.clone(),
                path: staging_path.join(&rendition.relative_path),
                width: 8,
                height: 16,
                page_to_raster: AffineTransform {
                    a: 0.08,
                    d: 0.08,
                    ..AffineTransform::IDENTITY
                },
            };
            pages.push(NativePage {
                number,
                label: FeatureState::Absent,
                width_points: 100.0,
                height_points: 200.0,
                geometry,
                native_text,
                metadata_artifact,
                renditions: vec![image],
                complexity,
                coverage: CoverageState::Native,
                capabilities,
            });
        }
        ensure!(texts.len() == 1, "one-page ordinary host fixture");
        fs::write(staging_path.join("fixture.txt"), texts[0])?;
        let mut document = gptgrep_parse::parse_path(&staging_path.join("fixture.txt")).await?;
        document.title = "synthetic".into();
        document.parser = RICH_PARSER.into();
        create(
            ArtifactKind::MergedText,
            None,
            document.text.as_bytes(),
            "merged.txt",
        )?;
        create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
        let unmapped_merged_lines = document.text.lines().count().max(1);
        Ok((
            staging,
            RichParsedDocument {
                summary: PreparedSummary {
                    source_sha256: sha.clone(),
                    prepared_sha256: sha,
                    source_bytes: source.len() as u64,
                    prepared_bytes: source.len() as u64,
                    format: "pdf".into(),
                    parser_identity: RICH_PARSER.into(),
                    preparation_profile_sha256: hash(&serde_json::to_vec(&(
                        "gptgrep.prepare.identity-pdf.v1",
                        gptgrep_parse::LITEPARSE_REVISION,
                    ))?),
                    conversion: FeatureState::Present(gptgrep_parse::ConversionIdentity {
                        converter: "identity-pdf".into(),
                        version: gptgrep_parse::LITEPARSE_REVISION.into(),
                        invocations: 0,
                    }),
                    image_geometry: FeatureState::Absent,
                },
                document,
                pages,
                artifacts,
                native_profile_sha256: profile,
                segments: vec![],
                ocr_tasks: vec![],
                ocr_results: vec![],
                coverage: texts
                    .iter()
                    .enumerate()
                    .map(|(i, _)| (i as u32 + 1, CoverageState::Native))
                    .collect(),
                unmapped_merged_lines,
            },
        ))
    }

    struct ProductionFixture {
        _temp: TempDir,
        base: PathBuf,
        raw: PathBuf,
        corpus: PathBuf,
        readers: PathBuf,
        options: PathBuf,
    }
    fn save(path: &Path, value: &Value) -> Result<()> {
        fs::write(path, serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }
    fn external(path: &Path) -> Value {
        json!({"path":path,"sha256":hash(&fs::read(path).unwrap())})
    }
    fn files(root: &Path) -> Vec<Value> {
        fn walk(root: &Path, dir: &Path, result: &mut Vec<Value>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, result);
                } else {
                    let data = fs::read(&path).unwrap();
                    result.push(json!({"path":path.strip_prefix(root).unwrap().to_str().unwrap(),"bytes":data.len(),"sha256":hash(&data)}));
                }
            }
        }
        let mut result = Vec::new();
        walk(root, root, &mut result);
        result.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        result
    }
    impl ProductionFixture {
        async fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let base = temp.path().canonicalize()?;
            let raw = base.join("raw");
            fs::create_dir(&raw)?;
            let (stage, parsed) = parsed_fixture(&raw, &["ordinary MOCK copper fixture\n"]).await?;
            fs::create_dir(raw.join("nested"))?;
            fs::rename(raw.join("source.pdf"), raw.join("nested/source.pdf"))?;
            let budget = ResourceBudget::default();
            let mut writer = RichGenerationWriter::begin(&raw, budget.clone())?;
            writer.add_document("nested/source.pdf", &stage.path().canonicalize()?, parsed)?;
            let pin = writer.publish()?;
            let reader = RichGenerationReader::open_pinned(&raw, &pin)?;
            reader.verify_all()?;
            let manifest = reader.manifest()?;
            let generation = raw.join(".gptgrep/generations").join(&pin.generation);
            let inventory = files(&generation);
            let index: Value = serde_json::from_slice(&fs::read(
                generation.join("artifact-index/gptgrep-index.json"),
            )?)?;
            let source = fs::read(raw.join("nested/source.pdf"))?;
            let docs =
                json!([{"path":"nested/source.pdf","bytes":source.len(),"sha256":hash(&source)}]);
            let raw_manifest = base.join("raw-manifest.json");
            save(
                &raw_manifest,
                &json!({"schema_version":"gptgrep.harbor.raw-group.v1","group_id":"group-a","documents":docs}),
            )?;
            let controls = base.join("build-controls.json");
            save(
                &controls,
                &json!({"schema_version":"gptgrep.harbor.native-controls.v1","group_id":"group-a","observation_wall_seconds":60,"raw_group_bytes":1048576,"seal_inventory_bytes":16777216,"seal_inventory_files":100,"native":{"page_window":1,"dpi":72.0,"render_pages":true,"max_document_pages":1},"resource_budget":budget,"hard_limits":{"native_wall":false,"native_rss":false,"total_index_publication_bytes":false}}),
            )?;
            let pin_file = base.join("pin.json");
            fs::write(&pin_file, serde_json::to_vec(&pin)?)?;
            let coverage=manifest.documents.iter().map(|d|json!({"path":d.document.path,"source_sha256":d.document.source_sha256,"prepared_sha256":d.prepared.prepared_sha256,"preparation_profile_sha256":d.prepared.preparation_profile_sha256,"native_profile_sha256":d.native_profile_sha256,"pages":d.pages.iter().map(|p|(p.number,&p.coverage)).collect::<Vec<_>>() })).collect::<Vec<_>>();
            let receipt = base.join("receipt.json");
            save(
                &receipt,
                &json!({"schema_version":"gptgrep.harbor.prebuild.receipt.v1","qualification":"source_structure_only_not_production_exporter_admission","producer_observation":"MOCK_fixture_Writer_not_native_builder","canonical_root":raw,"group_id":"group-a","raw_manifest_sha256":hash(&fs::read(&raw_manifest)?),"controls_sha256":hash(&fs::read(&controls)?),"raw_documents":docs,"pin":pin,"pin_input_sha256":null,"current_snapshot_sha256":hash(&fs::read(raw.join(".gptgrep/CURRENT.json"))?),"inventory_scope":"exact_current_snapshot_and_selected_generation_only_sibling_history_unverified","generation_bytes":inventory.iter().map(|v|v["bytes"].as_u64().unwrap()).sum::<u64>(),"inventory":inventory,"coverage":coverage,"parser_identity":RICH_PARSER,"liteparse_revision":LITEPARSE_REVISION,"index_engine_revision":index["engine_revision"],"index_file_table_id":index["file_table_id"],"index_manifest_snapshot_sha256":hash(&fs::read(generation.join("artifact-index/gptgrep-index.json"))?),"builder_source_revision":null,"builder_binary_sha256":null,"selected_resource_budget":budget,"shared_staging_reservation_bytes_observed":null,"elapsed_ms":0,"native_library_identity":null,"native_peak_rss_bytes":null,"current_frontdoor_provider_calls":0,"hard_executor_qualification":"HOLD","wrapper_json_buffer_ceiling_bytes":8388608,"retained_raw_payload_bytes":0,"retained_raw_file_descriptors":1,"retained_generation_file_descriptors":inventory.len()+1,"wrapper_metadata_scope":"MOCK fixture"}),
            )?;
            let tool = base.join("tool");
            fs::write(&tool, b"MOCK unqualified binary bytes; never executed")?;
            let corpus = base.join("corpus.json");
            save(
                &corpus,
                &json!({"schema_version":"gptgrep.harbor.production-corpus.v1","groups":[{"id":"group-a","source_root":raw,"runtime_root":raw,"sealed_receipt":external(&receipt),"sealed_pin":external(&pin_file),"sealed_raw_manifest":external(&raw_manifest),"sealed_controls":external(&controls)}],"tool":{"asset_path":tool,"sha256":hash(&fs::read(&tool)?),"version":"MOCK","source_revision":"a".repeat(40),"os":"MOCK","architecture":"MOCK"}}),
            )?;
            let question = "What material is named?";
            let readers = base.join("readers.json");
            save(
                &readers,
                &json!({"schema_version":"gptgrep.harbor.production-readers.v1","corpus_sha256":hash(&fs::read(&corpus)?),"tasks":[{"id":"task-a","question":question,"question_sha256":hash(question.as_bytes()),"group_scope":["group-a"]}]}),
            )?;
            let options = base.join("options.json");
            save(
                &options,
                &json!({"schema_version":"gptgrep.harbor.production-options.v1","reader_image":format!("example/reader@sha256:{}","a".repeat(64)),"verifier_image":format!("example/verifier@sha256:{}","b".repeat(64)),"codex_binary_sha256":"c".repeat(64),"codex_argv":["codex","exec","-"],"model":"gpt-6-luna","effort":"max","service_tier":"fast","timeout_sec":60,"cpus":1,"memory_mb":512,"network_mode":"no-network","repetitions":2,"seed":u32::MAX,"reader_uid":1000,"reader_gid":1000}),
            )?;
            Ok(Self {
                _temp: temp,
                base,
                raw,
                corpus,
                readers,
                options,
            })
        }
        fn prepare(&self) -> super::super::Result<production_export::Plan> {
            production_export::prepare(&self.corpus, &self.readers, &self.options, true)
        }
        fn export(&self, out: &Path) {
            self.prepare().unwrap().publish(out).unwrap();
            production_export::validate(out).unwrap();
        }
        fn grades(&self, reader: &Path) -> PathBuf {
            let manifest: Value =
                serde_json::from_slice(&fs::read(reader.join("export-manifest.json")).unwrap())
                    .unwrap();
            let reference = self.base.join("reference.json");
            save(&reference,&json!({"question":"What material is named?","answer":{"value":"MOCK_JUDGE_ONLY_CANARY"},"answer_format":"original generic JSON format"})).unwrap();
            let grades = self.base.join("graders.json");
            save(&grades,&json!({"schema_version":"gptgrep.harbor.production-verifiers.v1","corpus_sha256":manifest["corpus_sha256"],"reader_projection_sha256":manifest["reader_projection_sha256"],"reader_export_sha256":hash(&fs::read(reader.join("export-manifest.json")).unwrap()),"rubric_sha256":"d".repeat(64),"judge_status":"unavailable","tasks":[{"id":"task-a","question_sha256":manifest["task_bindings"][0]["question_sha256"],"reference_asset":external(&reference)}]})).unwrap();
            grades
        }
        fn change_corpus(&self, update: impl FnOnce(&mut Value)) {
            let mut c: Value = serde_json::from_slice(&fs::read(&self.corpus).unwrap()).unwrap();
            update(&mut c);
            save(&self.corpus, &c).unwrap();
        }
    }
    fn runtime_asset(path: &Path) -> Value {
        json!({"path":path,"bytes":fs::metadata(path).unwrap().len(),"sha256":hash(&fs::read(path).unwrap())})
    }
    fn bind_runtime(f: &ProductionFixture, manifest: &Value) -> PathBuf {
        let path = f.base.join("runtime-packages.json");
        save(&path, manifest).unwrap();
        let mut o: Value = serde_json::from_slice(&fs::read(&f.options).unwrap()).unwrap();
        o["runtime_debian_packages"] = external(&path);
        save(&f.options, &o).unwrap();
        path
    }
    fn runtime_fixture(f: &ProductionFixture) -> Value {
        let deb = f.base.join("ordinary_2%3a1+2:3_arm64.deb");
        let license = f.base.join("ordinary.copyright");
        // Synthetic ar-header fixture only: never installed or executed.
        fs::write(&deb, b"!<arch>\nMOCK package bytes, not installable").unwrap();
        fs::write(&license, b"CC0-1.0 synthetic fixture license").unwrap();
        json!({"schema_version":"gptgrep.harbor.runtime-debian.v1","packages":[{"deb":runtime_asset(&deb),"license":runtime_asset(&license)}]})
    }
    #[tokio::test]
    async fn runtime_absent_and_null_preserve_legacy_dockerfiles() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let first = f.base.join("absent");
        f.export(&first);
        let mut o: Value = serde_json::from_slice(&fs::read(&f.options)?)?;
        o["runtime_debian_packages"] = Value::Null;
        save(&f.options, &o)?;
        let second = f.base.join("null");
        f.export(&second);
        for arm in ["control", "gptgrep"] {
            let name = format!("{arm}/task-a/environment/Dockerfile");
            let a = fs::read(first.join(&name))?;
            assert_eq!(a, fs::read(second.join(&name))?);
            assert!(!String::from_utf8(a)?.contains("runtime-debian"));
        }
        assert_eq!(
            fs::read_to_string(first.join("control/task-a/environment/Dockerfile"))?,
            format!(
                "FROM example/reader@sha256:{}\nWORKDIR /app\nCOPY --chown=1000:1000 reader-input.json codex-config.json /app/\nCOPY --chown=1000:1000 groups/group-a/raw/ {}/\nUSER 1000:1000\n",
                "a".repeat(64),
                f.raw.display()
            )
        );
        assert!(!second.join("frozen-runtime-debian-inputs.json").exists());
        Ok(())
    }
    #[tokio::test]
    async fn runtime_packages_are_symmetric_mapped_and_offline_before_user() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let manifest = runtime_fixture(&f);
        bind_runtime(&f, &manifest);
        let output = f.base.join("reader");
        f.export(&output);
        let control = output.join("control/task-a/environment/runtime-debian");
        let treatment = output.join("gptgrep/task-a/environment/runtime-debian");
        assert_eq!(files(&control), files(&treatment));
        let mapping: Value = serde_json::from_slice(&fs::read(control.join("manifest.json"))?)?;
        assert_eq!(mapping["files"][0]["path"], "debs/package-0000.deb");
        assert_eq!(
            mapping["files"][0]["source_filename"],
            "ordinary_2%3a1+2:3_arm64.deb"
        );
        let map_bytes = fs::read(control.join("manifest.json"))?;
        assert!(!String::from_utf8(map_bytes)?.contains(f.base.to_str().unwrap()));
        for arm in ["control", "gptgrep"] {
            let docker =
                fs::read_to_string(output.join(format!("{arm}/task-a/environment/Dockerfile")))?;
            assert!(
                docker.starts_with(&format!("FROM example/reader@sha256:{}\n", "a".repeat(64)))
            );
            assert!(docker.contains("sha256sum --check checksums.sha256 && dpkg -i debs/*.deb"));
            assert!(docker.find("dpkg -i").unwrap() < docker.find("USER 1000:1000").unwrap());
            assert!(!docker.contains("apt-get") && !docker.contains("ordinary_2"));
        }
        assert!(
            !output
                .join("control/task-a/environment/groups/group-a/raw/.gptgrep")
                .exists()
        );
        Ok(())
    }
    #[tokio::test]
    async fn runtime_digest_size_and_deb_header_are_required() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let original = runtime_fixture(&f);
        let mut m = original.clone();
        m["packages"][0]["deb"]["sha256"] = json!("0".repeat(64));
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("digest"));
        m = original.clone();
        m["packages"][0]["license"]["bytes"] = json!(1);
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("size_binding"));
        let deb = PathBuf::from(original["packages"][0]["deb"]["path"].as_str().unwrap());
        fs::write(&deb, b"not an ar archive")?;
        m["packages"][0]["deb"] = runtime_asset(&deb);
        m["packages"][0]["license"] = original["packages"][0]["license"].clone();
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("deb_header"));
        Ok(())
    }
    #[tokio::test]
    async fn runtime_count_declared_and_actual_sizes_are_bounded_before_reads() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let original = runtime_fixture(&f);
        let mut m = original.clone();
        m["packages"][0]["deb"]["path"] = json!(f.base.join("nonexistent.deb"));
        m["packages"][0]["deb"]["bytes"] = json!(16 * 1024 * 1024 + 1);
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("asset_size"));
        m = original.clone();
        m["packages"] = json!(vec![original["packages"][0].clone(); 33]);
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("package_count"));
        let mut packages = Vec::new();
        for i in 0..3 {
            let mut p = original["packages"][0].clone();
            p["deb"]["path"] = json!(f.base.join(format!("nonexistent-{i}.deb")));
            p["deb"]["bytes"] = json!(16 * 1024 * 1024);
            packages.push(p);
        }
        m["packages"] = json!(packages);
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("total_size"));
        m = original.clone();
        let deb = PathBuf::from(m["packages"][0]["deb"]["path"].as_str().unwrap());
        fs::OpenOptions::new()
            .write(true)
            .open(&deb)?
            .set_len(16 * 1024 * 1024 + 1)?;
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("type_or_size_limit"));
        Ok(())
    }
    #[tokio::test]
    async fn runtime_traversal_injection_leaf_and_ancestor_symlinks_are_rejected() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let original = runtime_fixture(&f);
        for path in [
            f.base.join("../ordinary.deb"),
            f.base.join("bad;touch-forbidden.deb"),
            PathBuf::from("relative.deb"),
        ] {
            let mut m = original.clone();
            m["packages"][0]["deb"]["path"] = json!(path);
            bind_runtime(&f, &m);
            assert!(f.prepare().err().unwrap().contains("filename"));
        }
        let deb = PathBuf::from(original["packages"][0]["deb"]["path"].as_str().unwrap());
        let link = f.base.join("linked.deb");
        std::os::unix::fs::symlink(&deb, &link)?;
        let mut m = original.clone();
        m["packages"][0]["deb"]["path"] = json!(link);
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("symlink"));
        let alias = f.base.join("alias");
        std::os::unix::fs::symlink(&f.base, &alias)?;
        m["packages"][0]["deb"]["path"] = json!(alias.join(deb.file_name().unwrap()));
        bind_runtime(&f, &m);
        assert!(f.prepare().err().unwrap().contains("symlink"));
        Ok(())
    }
    #[tokio::test]
    async fn runtime_assets_and_manifest_remain_bound_until_publication() -> Result<()> {
        for asset in ["deb", "license", "manifest"] {
            let f = ProductionFixture::new().await?;
            let m = runtime_fixture(&f);
            let manifest = bind_runtime(&f, &m);
            let prepared = f.prepare().unwrap();
            let path = if asset == "manifest" {
                manifest
            } else {
                PathBuf::from(m["packages"][0][asset]["path"].as_str().unwrap())
            };
            fs::write(path, b"changed after admission")?;
            let out = f.base.join("must-not-exist");
            assert!(prepared.publish(&out).is_err());
            assert!(!out.exists());
        }
        Ok(())
    }
    #[tokio::test]
    async fn runtime_rewritten_inventory_cannot_change_packages_or_install_command() -> Result<()> {
        for name in ["runtime-debian/debs/package-0000.deb", "Dockerfile"] {
            let f = ProductionFixture::new().await?;
            bind_runtime(&f, &runtime_fixture(&f));
            let out = f.base.join("reader");
            f.export(&out);
            let relative = format!("control/task-a/environment/{name}");
            fs::write(out.join(&relative), b"rewritten")?;
            let mp = out.join("export-manifest.json");
            let mut manifest: Value = serde_json::from_slice(&fs::read(&mp)?)?;
            manifest["files"][&relative] = json!(hash(b"rewritten"));
            save(&mp, &manifest)?;
            let error = production_export::validate(&out).err().unwrap();
            assert!(if name == "Dockerfile" {
                error == "production_reader_docker_binding"
            } else {
                error.contains("runtime_")
            });
        }
        Ok(())
    }
    #[tokio::test]
    async fn runtime_extra_run_and_root_user_cannot_refresh_inventory() -> Result<()> {
        for arm in ["control", "gptgrep"] {
            for addition in ["RUN touch /unrequested-command\n", "USER 0:0\n"] {
                let f = ProductionFixture::new().await?;
                bind_runtime(&f, &runtime_fixture(&f));
                let out = f.base.join("reader");
                f.export(&out);
                let relative = format!("{arm}/task-a/environment/Dockerfile");
                let mut data = fs::read(out.join(&relative))?;
                data.extend_from_slice(addition.as_bytes());
                fs::write(out.join(&relative), &data)?;
                let mp = out.join("export-manifest.json");
                let mut manifest: Value = serde_json::from_slice(&fs::read(&mp)?)?;
                manifest["files"][&relative] = json!(hash(&data));
                save(&mp, &manifest)?;
                assert_eq!(
                    production_export::validate(&out).err().unwrap(),
                    "production_reader_docker_binding"
                );
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn real_reader_group_topology_raw_only_baseline_and_role_reference() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let reader = f.base.join("reader-export");
        f.export(&reader);
        let manifest: Value =
            serde_json::from_slice(&fs::read(reader.join("export-manifest.json"))?)?;
        assert_eq!(manifest["executable"], false);
        assert_eq!(manifest["qualification"]["runtime"], "HOLD");
        let baseline = reader.join("control/task-a/environment/groups/group-a/raw");
        let treatment = reader.join("gptgrep/task-a/environment/groups/group-a/raw");
        assert_eq!(
            fs::read(baseline.join("nested/source.pdf"))?,
            fs::read(treatment.join("nested/source.pdf"))?
        );
        assert!(!baseline.join(".gptgrep").exists());
        let pin = &manifest["group_bindings"][0]["pin"];
        let generation = treatment
            .join(".gptgrep/generations")
            .join(pin["generation"].as_str().unwrap());
        for name in ["artifact-text", "artifact-index", "text", "artifacts"] {
            assert!(generation.join(name).is_dir());
        }
        assert!(
            fs::read_to_string(reader.join("control/task-a/environment/Dockerfile"))?
                .contains(f.raw.to_str().unwrap())
        );
        let grader = f.base.join("grader-export");
        production_export::verifier(&f.grades(&reader), &reader)
            .unwrap()
            .publish(&grader)
            .unwrap();
        assert!(
            fs::read_to_string(grader.join("task-a/reference.json"))?
                .contains("MOCK_JUDGE_ONLY_CANARY")
        );
        for entry in files(&reader) {
            let p = reader.join(entry["path"].as_str().unwrap());
            assert!(
                !fs::read(p)?
                    .windows(22)
                    .any(|w| w == b"MOCK_JUDGE_ONLY_CANARY")
            );
        }
        Ok(())
    }
    #[tokio::test]
    async fn root_relocation_extra_source_and_symlink_refused_before_questions() -> Result<()> {
        let f = ProductionFixture::new().await?;
        f.change_corpus(|c| c["groups"][0]["runtime_root"] = json!("/app/changed"));
        fs::write(&f.readers, b"not reader JSON")?;
        assert!(f.prepare().err().unwrap().contains("relocation"));
        let f = ProductionFixture::new().await?;
        fs::write(f.raw.join("extra.pdf"), b"%PDF-extra")?;
        assert!(f.prepare().err().unwrap().contains("raw_set"));
        let f = ProductionFixture::new().await?;
        fs::rename(f.raw.join("nested/source.pdf"), f.base.join("original.pdf"))?;
        std::os::unix::fs::symlink(f.base.join("original.pdf"), f.raw.join("nested/source.pdf"))?;
        assert!(f.prepare().is_err());
        Ok(())
    }
    #[tokio::test]
    async fn sealed_digest_current_and_extra_generation_never_become_authority() -> Result<()> {
        let f = ProductionFixture::new().await?;
        fs::write(f.base.join("receipt.json"), b"{}")?;
        assert!(f.prepare().err().unwrap().contains("digest"));
        let f = ProductionFixture::new().await?;
        fs::write(f.raw.join(".gptgrep/CURRENT.json"), b"{}")?;
        assert!(f.prepare().err().unwrap().contains("digest"));
        let f = ProductionFixture::new().await?;
        let pointer: Value =
            serde_json::from_slice(&fs::read(f.raw.join(".gptgrep/CURRENT.json"))?)?;
        fs::write(
            f.raw
                .join(".gptgrep/generations")
                .join(pointer["generation"].as_str().unwrap())
                .join("extra.json"),
            b"{}",
        )?;
        assert!(f.prepare().err().unwrap().contains("inventory"));
        Ok(())
    }
    #[tokio::test]
    async fn held_source_change_after_admission_refuses_fresh_publication() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let p = f.prepare().unwrap();
        fs::write(f.raw.join("nested/source.pdf"), b"%PDF-changed")?;
        let out = f.base.join("out");
        assert!(p.publish(&out).is_err());
        assert!(!out.exists());
        Ok(())
    }
    #[tokio::test]
    async fn reference_json_shape_and_exact_reader_bytes_are_required() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let reader = f.base.join("reader");
        f.export(&reader);
        let grades = f.grades(&reader);
        let reference = f.base.join("reference.json");
        save(
            &reference,
            &json!({"question":"What material is named?","answer":null,"answer_format":"JSON"}),
        )?;
        let mut g: Value = serde_json::from_slice(&fs::read(&grades)?)?;
        g["tasks"][0]["reference_asset"] = external(&reference);
        save(&grades, &g)?;
        assert!(
            production_export::verifier(&grades, &reader)
                .err()
                .unwrap()
                .contains("reference_contract")
        );
        let grades = f.grades(&reader);
        let path = reader.join("control/task-a/environment/Dockerfile");
        fs::write(&path, b"changed")?;
        let mut m: Value = serde_json::from_slice(&fs::read(reader.join("export-manifest.json"))?)?;
        m["files"]["control/task-a/environment/Dockerfile"] = json!(hash(b"changed"));
        save(&reader.join("export-manifest.json"), &m)?;
        assert!(
            production_export::verifier(&grades, &reader)
                .err()
                .unwrap()
                .contains("reader_docker_binding")
        );
        Ok(())
    }
    #[tokio::test]
    async fn rewritten_inventory_cannot_add_baseline_preparsed_files() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let reader = f.base.join("reader");
        f.export(&reader);
        let name = "control/task-a/environment/prebuilt.txt";
        fs::write(reader.join(name), b"extra parsed")?;
        let mut m: Value = serde_json::from_slice(&fs::read(reader.join("export-manifest.json"))?)?;
        m["files"][name] = json!(hash(b"extra parsed"));
        save(&reader.join("export-manifest.json"), &m)?;
        assert!(
            production_export::validate(&reader)
                .err()
                .unwrap()
                .contains("namespace")
        );
        Ok(())
    }
    #[tokio::test]
    async fn duplicate_group_scope_and_combined_output_budget_refused() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let mut r: Value = serde_json::from_slice(&fs::read(&f.readers)?)?;
        r["tasks"][0]["group_scope"] = json!(["group-a", "group-a"]);
        save(&f.readers, &r)?;
        assert!(f.prepare().err().unwrap().contains("duplicate_scope"));
        let f = ProductionFixture::new().await?;
        let tool = f.base.join("tool");
        fs::OpenOptions::new()
            .write(true)
            .open(&tool)?
            .set_len(48 * 1024 * 1024)?;
        f.change_corpus(|c| c["tool"]["sha256"] = json!(hash(&fs::read(&tool).unwrap())));
        let mut r: Value = serde_json::from_slice(&fs::read(&f.readers)?)?;
        r["corpus_sha256"] = json!(hash(&fs::read(&f.corpus)?));
        let task = r["tasks"][0].clone();
        r["tasks"] = json!([task.clone(), task.clone(), task]);
        for i in 0..3 {
            r["tasks"][i]["id"] = json!(format!("task-{i}"));
        }
        save(&f.readers, &r)?;
        assert!(f.prepare().err().unwrap().contains("output_budget"));
        Ok(())
    }
    #[test]
    fn production_private_modes_use_only_owned_child_umasks() -> Result<()> {
        for mask in ["000", "777"] {
            let status = std::process::Command::new(std::env::current_exe()?)
                .arg("--exact")
                .arg("tests::production::production_private_mode_child")
                .arg("--nocapture")
                .env("GPTGREP_PRODUCTION_MODE_FIXTURE", mask)
                .status()?;
            ensure!(status.success());
        }
        Ok(())
    }
    #[tokio::test]
    async fn production_private_mode_child() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let Some(mask) = std::env::var("GPTGREP_PRODUCTION_MODE_FIXTURE").ok() else {
            return Ok(());
        };
        let p = f.prepare().unwrap();
        // The child owns this process; the parent's mask is never changed.
        unsafe {
            libc::umask(libc::mode_t::from_str_radix(&mask, 8)?);
        }
        let out = f.base.join("mode-output");
        p.publish(&out).unwrap();
        fn verify(path: &Path) -> Result<()> {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::symlink_metadata(path)?;
            assert_eq!(
                m.permissions().mode() & 0o777,
                if m.is_dir() { 0o700 } else { 0o600 }
            );
            if m.is_dir() {
                for e in fs::read_dir(path)? {
                    verify(&e?.path())?;
                }
            }
            Ok(())
        }
        verify(&out)?;
        Ok(())
    }
    #[tokio::test]
    async fn production_treatment_guidance_is_bound_and_baseline_stays_raw_only() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let out = f.base.join("tool-guidance-readers");
        f.export(&out);
        let question = "What material is named?";
        let baseline = fs::read(out.join("control/task-a/instruction.md"))?;
        assert_eq!(baseline, format!("{question}\n\nRead the raw PDF groups named in /app/reader-input.json. Write only {{\"answer\":\"...\"}} to /app/answer.json.\n").into_bytes());
        let treatment = fs::read(out.join("gptgrep/task-a/instruction.md"))?;
        assert!(treatment.starts_with(&baseline));
        assert!(
            String::from_utf8(treatment)?
                .contains("/app/tool/gptgrep batch --rich --root ROOT --input FILE")
        );
        assert!(!out.join("control/task-a/environment/tool").exists());
        // Refresh editable inventory, so the instruction comparison is the discriminator.
        let name = "gptgrep/task-a/instruction.md";
        fs::write(out.join(name), &baseline)?;
        let manifest_path = out.join("export-manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["files"][name] = json!(hash(&baseline));
        save(&manifest_path, &manifest)?;
        assert_eq!(
            production_export::validate(&out).err().unwrap(),
            "production_instruction_binding"
        );
        Ok(())
    }

    #[tokio::test]
    async fn production_bridge_fixture_if_selected() -> Result<()> {
        let f = ProductionFixture::new().await?;
        let Some(output) = std::env::var_os("GPTGREP_PRODUCTION_BRIDGE_FIXTURE") else {
            return Ok(());
        };
        let output = PathBuf::from(output);
        if let Ok(image) = std::env::var("GPTGREP_PRODUCTION_BRIDGE_READER_IMAGE") {
            let mut options: Value = serde_json::from_slice(&fs::read(&f.options)?)?;
            options["reader_image"] = json!(image);
            save(&f.options, &options)?;
        }
        match std::env::var("GPTGREP_PRODUCTION_BRIDGE_VARIANT")
            .ok()
            .as_deref()
        {
            None => {}
            Some("runtime") => {
                bind_runtime(&f, &runtime_fixture(&f));
            }
            Some("null") => {
                let mut options: Value = serde_json::from_slice(&fs::read(&f.options)?)?;
                options["runtime_debian_packages"] = Value::Null;
                save(&f.options, &options)?;
            }
            Some(_) => anyhow::bail!("unknown explicit offline fixture variant"),
        }
        fs::create_dir(&output)?;
        let reader = output.join("readers");
        f.export(&reader);
        production_export::verifier(&f.grades(&reader), &reader)
            .unwrap()
            .publish(&output.join("graders"))
            .unwrap();
        // Generic MOCK artifacts only. Their source tempdir will be dropped, so this is
        // an offline composition fixture, never an adopted runtime corpus.
        Ok(())
    }
}
