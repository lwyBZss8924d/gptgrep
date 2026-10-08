use super::*;
use gptgrep_core::RichGenerationWriter;
use gptgrep_pageindex::{PageInput, from_pages};
use gptgrep_parse::*;
use std::{collections::BTreeMap, sync::Arc, time::SystemTime};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
// Copied ordinary synthetic DTO custody fixture. This is not native/OCR accuracy.
fn candidate(root: &Path, texts: &[&str]) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = b"synthetic source bytes for artifact custody tests";
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = digest(source);
    let profile = digest(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create =
        |kind: ArtifactKind, page: Option<u32>, bytes: &[u8], name: &str| -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &digest(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: digest(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".txt") {
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
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        });
    }
    let inputs = texts
        .iter()
        .enumerate()
        .map(|(i, text)| PageInput {
            number: i as u32 + 1,
            text: (*text).into(),
        })
        .collect::<Vec<_>>();
    let document = from_pages("synthetic", &inputs, &[], &[], RICH_PARSER)?;
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
                preparation_profile_sha256: digest(&serde_json::to_vec(&(
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

fn publish(root: &Path, texts: &[&str]) -> Result<GenerationPin> {
    let (staging, parsed) = candidate(root, texts)?;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    config_path: PathBuf,
    config_sha: String,
}
impl Fixture {
    fn new() -> Result<Self> {
        Self::with_lifetime(Duration::from_secs(30))
    }
    fn with_lifetime(lifetime: Duration) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let corpus = root.join("corpus");
        fs::create_dir(&corpus)?;
        let generation = publish(
            &corpus,
            &["The workshop green cabinet holds a blue linen pouch.\n"],
        )?;
        let query_path = root.join("query.txt");
        fs::write(&query_path, b"What material is the pouch?")?;
        let deadline = SystemTime::now() + lifetime;
        let context =
            ExecutionContext::new("ordinary-bootstrap-fixture", &"a".repeat(64), deadline, 4)?
                .with_ledger(root.join("ledger.json"))?;
        let anchor = context.ledger_clock_anchor()?.unwrap();
        let config = Config {
            schema_version: "gptgrep.harbor.rich_helper.v1".into(),
            corpus_root: corpus,
            generation,
            query: InputFile {
                path: query_path,
                bytes: 27,
                sha256: digest(b"What material is the pouch?"),
            },
            options: Options {
                document: None,
                window_bytes: 1024,
                max_output_bytes: EVIDENCE_CAP,
                max_metadata_bytes: 524288,
                min_score: 0.5,
            },
            expected_actual_model: "openai/gpt-6-luna-decisions-20261006".into(),
            context: ContextInput {
                ledger: root.join("ledger.json"),
                initialization_sha256: digest(&fs::read(root.join("ledger.json.lock"))?),
                original_clock_anchor_sha256: digest(&serde_json::to_vec(&anchor)?),
                run_id: "ordinary-bootstrap-fixture".into(),
                plan_digest: "a".repeat(64),
                absolute_deadline_unix_ms: anchor.deadline_unix_ms(),
                max_submissions: 4,
                expected_before: context.snapshot()?,
            },
            output_dir: root.join("helper-output"),
        };
        let mut fixture = Self {
            _dir: dir,
            config,
            config_path: root.join("config.json"),
            config_sha: String::new(),
        };
        fixture.refresh()?;
        Ok(fixture)
    }
    fn refresh(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.config)?;
        fs::write(&self.config_path, &bytes)?;
        self.config_sha = digest(&bytes);
        Ok(())
    }
    fn bound(&self) -> Result<Bound> {
        bind(&self.config_path, &self.config_sha)
    }
    fn read_output(&self, name: &str) -> Result<Value> {
        decode(&fs::read(self.config.output_dir.join(name))?)
    }
}
type MockAction = Arc<dyn Fn(usize) -> Result<()> + Send + Sync>;
async fn mock(
    count: usize,
    fail_at: Option<usize>,
    actual_model: &str,
    action: MockAction,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Value>>>)> {
    mock_with_cost(count, fail_at, actual_model, action, None).await
}
async fn mock_with_cost(
    count: usize,
    fail_at: Option<usize>,
    actual_model: &str,
    action: MockAction,
    cost: Option<String>,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Value>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/decisions", listener.local_addr()?);
    let actual_model = actual_model.to_owned();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for ordinal in 0..count {
            let (mut stream, _) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await??;
            let mut bytes = Vec::new();
            let (end, length) = loop {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).await?;
                ensure!(n > 0 && bytes.len() + n <= 131072, "mock_request");
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end])?;
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse::<usize>())
                        })
                        .transpose()?
                        .ok_or_else(|| anyhow::anyhow!("mock_length"))?;
                    break (end + 4, length);
                }
            };
            while bytes.len() < end + length {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).await?;
                ensure!(n > 0 && bytes.len() + n <= 131072, "mock_body");
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = decode(&bytes[end..end + length])?;
            action(ordinal)?;
            let answers = request["questions"]
                .as_object()
                .unwrap()
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        json!({"type":"score","score":3.0,"confidence":0.75}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let body=format!("{{\"model\":{},\"answers\":{},\"usage\":{{\"input_tokens\":100,\"output_tokens\":0,\"cost\":{},\"private_detail\":\"DO_NOT_RETAIN_PROVIDER_DETAIL\"}},\"id\":\"PRIVATE_ID\",\"provider\":\"PRIVATE_PROVIDER\"}}",serde_json::to_string(&actual_model)?,serde_json::to_string(&answers)?,cost.as_deref().unwrap_or("0.00001")).into_bytes();
            let status = if fail_at == Some(ordinal) { 500 } else { 200 };
            stream.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
            stream.write_all(&body).await?;
            requests.push(request);
        }
        Ok(requests)
    });
    Ok((endpoint, task))
}
async fn run(fixture: &Fixture, endpoint: &str) -> Result<Value> {
    let endpoint = endpoint.to_owned();
    execute_bound(fixture.bound()?, Instant::now(), move |context| {
        Ok(
            JevClient::with_endpoint("mock-key-not-a-secret", Some(MODEL), &endpoint)?
                .with_execution_context(context),
        )
    })
    .await
}

#[test]
fn preflight_binds_inputs_without_clock_join_key_or_output_effects() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = fs::read(&fixture.config.context.ledger)?;
    let lock = fs::read(lock_path(&fixture.config.context.ledger)?)?;
    let bound = fixture.bound()?;
    assert_eq!(bound.query, "What material is the pouch?");
    assert_eq!(before, fs::read(&fixture.config.context.ledger)?);
    assert_eq!(lock, fs::read(lock_path(&fixture.config.context.ledger)?)?);
    assert!(!fixture.config.output_dir.exists());
    assert_eq!(discovery()["effects"], 0);
    Ok(())
}
#[test]
fn query_configuration_and_duplicate_json_pins_fail_closed() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(&fixture.config.query.path, b"changed query")?;
    assert!(fixture.bound().is_err());
    assert!(bind(&fixture.config_path, &"0".repeat(64)).is_err());
    assert!(decode::<Value>(br#"{"query":1,"qu\u0065ry":2}"#).is_err());
    let mut config = serde_json::to_value(&fixture.config)?;
    config["context"]["expected_before"]["unexpected"] = json!(true);
    let raw = serde_json::to_vec(&config)?;
    fs::write(&fixture.config_path, &raw)?;
    assert!(bind(&fixture.config_path, &digest(&raw)).is_err());
    assert!(!fixture.config.output_dir.exists());
    Ok(())
}
#[test]
fn missing_original_files_and_anchor_mismatch_never_initialize_or_renew() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.config.context.original_clock_anchor_sha256 = "0".repeat(64);
    fixture.refresh()?;
    assert!(fixture.bound().is_err());
    fs::remove_file(&fixture.config.context.ledger)?;
    fs::remove_file(lock_path(&fixture.config.context.ledger)?)?;
    assert!(fixture.bound().is_err());
    assert!(!fixture.config.context.ledger.exists());
    assert!(!lock_path(&fixture.config.context.ledger)?.exists());
    Ok(())
}
#[test]
fn symlinks_output_overlap_and_bounded_options_are_rejected() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let original = fixture.config.query.path.clone();
    let link = original.with_file_name("query-link");
    std::os::unix::fs::symlink(&original, &link)?;
    fixture.config.query.path = link;
    fixture.refresh()?;
    assert!(fixture.bound().is_err());
    fixture.config.query.path = original;
    fixture.config.output_dir = fixture.config.corpus_root.join("unsafe-output");
    fixture.refresh()?;
    assert!(fixture.bound().is_err());
    fixture.config.options.window_bytes = 2049;
    fixture.refresh()?;
    assert!(fixture.bound().is_err());
    Ok(())
}
#[tokio::test]
async fn genuine_two_stage_pipeline_preserves_sources_usage_coverage_and_original_anchor()
-> Result<()> {
    let fixture = Fixture::new()?;
    let model = fixture.config.expected_actual_model.clone();
    let (endpoint, server) = mock(2, None, &model, Arc::new(|_| Ok(()))).await?;
    let result = run(&fixture, &endpoint).await?;
    let requests = server.await??;
    assert_eq!(result["status"], "completed");
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request["model"], MODEL);
        assert_eq!(request["provider"]["allow_fallbacks"], false);
        assert!(
            request["questions"]
                .as_object()
                .unwrap()
                .values()
                .all(|q| q["type"] == "score")
        );
    }
    assert_eq!(result["execution_after"]["logical_reservations"], 2);
    assert_eq!(result["execution_after"]["send_start_observations"], 2);
    assert_eq!(result["product"]["logical_calls_attempted"], 2);
    assert_eq!(result["product"]["accounting_complete"], true);
    assert_eq!(result["helper_logical_accounting_complete"], true);
    assert_eq!(result["logical_reservations_delta"], 2);
    assert_eq!(result["send_start_observations_delta"], 2);
    assert_eq!(result["submission_observations_delta"], 2);
    assert_eq!(result["terminal_submissions_delta"], 2);
    assert_eq!(result["observed_validated_input_tokens_sum"], 200);
    assert_eq!(result["observed_validated_cost_sum"], 0.00002);
    assert_eq!(
        result["original_clock_anchor_sha256"],
        fixture.config.context.original_clock_anchor_sha256
    );
    assert_eq!(result["excluded_as_indexing"], false);
    assert_eq!(result["actual_provider_requests"], Value::Null);
    let evidence = fixture.read_output("reader-evidence.json")?;
    assert_eq!(
        evidence["generation"],
        serde_json::to_value(&fixture.config.generation)?
    );
    assert!(!evidence["hits"].as_array().unwrap().is_empty());
    assert_eq!(evidence["coverage"]["semantic_scope_complete"], false);
    let account = fs::read(fixture.config.output_dir.join("accounting.json"))?;
    assert!(!String::from_utf8(account)?.contains("DO_NOT_RETAIN_PROVIDER_DETAIL"));
    assert_eq!(
        result["validated_observations"].as_array().unwrap().len(),
        2
    );
    assert_eq!(result["validated_observations"][0]["input_tokens"], 100);
    assert_eq!(result["validated_observations"][0]["cost"], 0.00001);
    assert!(
        result["validated_observations"][0]["complete_validated_usage_sha256"]
            .as_str()
            .unwrap()
            .len()
            == 64
    );
    Ok(())
}
#[tokio::test]
async fn required_stage_failures_keep_actual_counts_unknown_cost_and_no_reader_evidence()
-> Result<()> {
    for ordinal in [0, 1] {
        let fixture = Fixture::new()?;
        let (endpoint, server) = mock(
            ordinal + 1,
            Some(ordinal),
            &fixture.config.expected_actual_model,
            Arc::new(|_| Ok(())),
        )
        .await?;
        let result = run(&fixture, &endpoint).await?;
        assert_eq!(server.await??.len(), ordinal + 1);
        assert_eq!(result["status"], "failed");
        assert_eq!(
            result["execution_after"]["logical_reservations"],
            ordinal + 1
        );
        assert_eq!(
            result["validated_observations"].as_array().unwrap().len(),
            ordinal
        );
        assert_eq!(result["reader_must_not_start"], true);
        assert_eq!(
            fixture.read_output("reader-evidence.json")?["status"],
            "unavailable"
        );
        assert_eq!(result["product"]["accounting_complete"], false);
        assert_eq!(result["helper_logical_accounting_complete"], false);
        assert_eq!(result["actual_provider_requests"], Value::Null);
    }
    Ok(())
}
#[tokio::test]
async fn original_deadline_expiry_at_publication_barriers_revokes_evidence_and_keeps_charges()
-> Result<()> {
    for selected in [
        FinalizationPoint::BeforeEvidence,
        FinalizationPoint::AfterAccounting,
    ] {
        let fixture = Fixture::with_lifetime(Duration::from_secs(3))?;
        let original_lock = fs::read(lock_path(&fixture.config.context.ledger)?)?;
        let (endpoint, server) = mock(
            2,
            None,
            &fixture.config.expected_actual_model,
            Arc::new(|_| Ok(())),
        )
        .await?;
        let held = Arc::new(Mutex::new(None::<ExecutionContext>));
        let for_client = Arc::clone(&held);
        let output = fixture.config.output_dir.clone();
        let reached = std::sync::atomic::AtomicBool::new(false);
        let result = execute_bound_with_finalization_hook(
            fixture.bound()?,
            Instant::now(),
            move |context| {
                *for_client.lock().unwrap() = Some(context.clone());
                Ok(
                    JevClient::with_endpoint("mock-key-not-a-secret", Some(MODEL), &endpoint)?
                        .with_execution_context(context),
                )
            },
            |context, point| {
                if point == selected {
                    context.check_deadline()?;
                    let account: Value = decode(&fs::read(output.join("accounting.json"))?)?;
                    let evidence: Value = decode(&fs::read(output.join("reader-evidence.json"))?)?;
                    assert_eq!(
                        account["validated_observations"].as_array().unwrap().len(),
                        2
                    );
                    if point == FinalizationPoint::BeforeEvidence {
                        assert_eq!(account["status"], "running");
                        assert_eq!(evidence["status"], "unavailable");
                    } else {
                        assert_eq!(account["status"], "completed");
                        assert_eq!(evidence["status"], "completed");
                    }
                    let remaining = context.original_run_remaining()?;
                    assert!(remaining <= Duration::from_secs(3));
                    // One bounded fixture wait expires the SAME original anchor.
                    // No new clock/view, changed ledger or simulated deadline error.
                    std::thread::sleep(remaining + Duration::from_millis(5));
                    assert!(context.check_deadline().is_err());
                    reached.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(())
            },
        )
        .await?;
        assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(server.await??.len(), 2);
        assert_eq!(result["status"], "failed");
        assert_eq!(result["reader_must_not_start"], true);
        assert_eq!(result["helper_logical_accounting_complete"], false);
        assert_eq!(
            result["prior_accounting"]["validated_observations"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            fixture.read_output("reader-evidence.json")?["status"],
            "unavailable"
        );
        assert_eq!(fixture.read_output("accounting.json")?, result);
        let context = held.lock().unwrap().take().unwrap();
        let snapshot = context.snapshot()?;
        assert_eq!(snapshot.logical_reservations, 2);
        assert_eq!(snapshot.send_start_observations, 2);
        assert_eq!(snapshot.terminal_submissions, 2);
        assert_eq!(
            digest(&serde_json::to_vec(
                &context.ledger_clock_anchor()?.unwrap()
            )?),
            fixture.config.context.original_clock_anchor_sha256
        );
        assert_eq!(
            original_lock,
            fs::read(lock_path(&fixture.config.context.ledger)?)?
        );
        assert!(fixture.config.output_dir.join("consumed.json").exists());
        assert!(
            execute_bound(fixture.bound()?, Instant::now(), |_| panic!(
                "consumed output must stop before client"
            ))
            .await
            .is_err()
        );
    }
    Ok(())
}
#[tokio::test]
async fn model_mismatch_stops_after_one_observation_without_fallback_or_freeform_retention()
-> Result<()> {
    let fixture = Fixture::new()?;
    let (endpoint, server) = mock(1, None, "PRIVATE_WRONG_MODEL", Arc::new(|_| Ok(()))).await?;
    let result = run(&fixture, &endpoint).await?;
    assert_eq!(server.await??.len(), 1);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["product"]["logical_calls_attempted"], 1);
    assert_eq!(result["product"]["accounting_complete"], true);
    assert_eq!(result["helper_logical_accounting_complete"], false);
    assert_eq!(
        result["validated_observations"][0]["actual_model"],
        Value::Null
    );
    assert_eq!(
        result["validated_observations"][0]["actual_model_matches_expected"],
        false
    );
    assert!(!result.to_string().contains("PRIVATE_WRONG_MODEL"));
    Ok(())
}
#[tokio::test]
async fn source_change_after_route_preserves_observation_and_stops_second_stage() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.config.corpus_root.join("source.pdf");
    let (endpoint, server) = mock(
        1,
        None,
        &fixture.config.expected_actual_model,
        Arc::new(move |_| {
            fs::write(&source, b"changed source")?;
            Ok(())
        }),
    )
    .await?;
    let result = run(&fixture, &endpoint).await?;
    assert_eq!(server.await??.len(), 1);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["product"]["logical_calls_attempted"], 1);
    assert_eq!(
        result["validated_observations"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        fixture.read_output("reader-evidence.json")?["status"],
        "unavailable"
    );
    Ok(())
}
#[tokio::test]
async fn preparation_output_bound_refuses_before_any_submission_and_consumed_output_cannot_replay()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.config.options.max_output_bytes = 1;
    fixture.refresh()?;
    let result = execute_bound(fixture.bound()?, Instant::now(), |context| {
        Ok(JevClient::new("dummy-not-a-secret", Some(MODEL))?.with_execution_context(context))
    })
    .await?;
    assert_eq!(result["status"], "failed");
    let joined = join_existing(&fixture.config.context)?;
    assert_eq!(joined.snapshot()?.logical_reservations, 0);
    assert!(fixture.config.output_dir.join("consumed.json").exists());
    assert!(
        execute_bound(fixture.bound()?, Instant::now(), |_| panic!(
            "must not construct client"
        ))
        .await
        .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn oversized_numeric_cost_projection_stops_after_one_and_keeps_bounded_accounting()
-> Result<()> {
    let fixture = Fixture::new()?;
    let cost = format!("0.{}1", "0".repeat(100000));
    let (endpoint, server) = mock_with_cost(
        1,
        None,
        &fixture.config.expected_actual_model,
        Arc::new(|_| Ok(())),
        Some(cost),
    )
    .await?;
    let result = run(&fixture, &endpoint).await?;
    assert_eq!(server.await??.len(), 1);
    assert_eq!(result["status"], "failed");
    assert_eq!(
        result["validated_observations"][0]["cost_projection_valid"],
        false
    );
    assert_eq!(result["validated_observations"][0]["cost"], Value::Null);
    assert_eq!(
        result["validated_observations"][0]["cost_decimal"],
        Value::Null
    );
    assert_eq!(result["execution_after"]["logical_reservations"], 1);
    assert_eq!(
        fixture.read_output("reader-evidence.json")?["status"],
        "unavailable"
    );
    assert!(fs::read(fixture.config.output_dir.join("accounting.json"))?.len() < ACCOUNT_CAP);
    assert!(
        result["validated_observations"][0]["complete_validated_usage_sha256"]
            .as_str()
            .unwrap()
            .len()
            == 64
    );
    Ok(())
}
#[tokio::test]
async fn full_pool_bounds_and_omitted_coverage_are_preserved_without_wrapper_truncation()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    let root = &fixture.config.corpus_root;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    for ordinal in 0..10 {
        let (staging, parsed) = candidate(
            root,
            &["ordinary one\n", "ordinary two\n", "ordinary three\n"],
        )?;
        let path = format!("d{ordinal:02}.pdf");
        fs::copy(root.join("source.pdf"), root.join(&path))?;
        writer.add_document(&path, &staging.path().canonicalize()?, parsed)?;
    }
    fixture.config.generation = writer.publish()?;
    fixture.refresh()?;
    let (endpoint, server) = mock(
        2,
        None,
        &fixture.config.expected_actual_model,
        Arc::new(|_| Ok(())),
    )
    .await?;
    let result = run(&fixture, &endpoint).await?;
    let requests = server.await??;
    assert_eq!(result["status"], "completed");
    assert!(requests[1]["questions"].as_object().unwrap().len() <= 24);
    let evidence = fixture.read_output("reader-evidence.json")?;
    assert_eq!(evidence["coverage"]["scoped_documents"], 10);
    assert_eq!(evidence["coverage"]["available_artifacts"], 30);
    assert_eq!(evidence["coverage"]["inspected_artifacts"], 24);
    assert_eq!(evidence["coverage"]["omitted_artifacts"], 6);
    assert_eq!(evidence["coverage"], result["product"]["coverage"]);
    assert!(evidence["hits"].as_array().unwrap().len() <= 24);
    assert!(
        result["full_pool_request_bytes"].as_u64().unwrap()
            >= serde_json::to_vec(&requests[1])?.len() as u64
    );
    Ok(())
}
