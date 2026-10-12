use super::*;
use std::os::unix::fs::PermissionsExt;

const MOCK_HEADER: &str =
    "# OFFLINE MOCK ONLY; never Codex or a provider.\nimport os,sys,json,time\n";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    path: PathBuf,
    value: Value,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for dir in [
            "app",
            "app/raw",
            "out",
            "out/home",
            "out/home/codex",
            "out/tmp",
        ] {
            fs::create_dir(root.join(dir)).unwrap();
            fs::set_permissions(root.join(dir), fs::Permissions::from_mode(0o700)).unwrap();
        }
        private_new(
            &root.join("app/raw/document.pdf"),
            b"%PDF-1.4\nOFFLINE SYNTHETIC SOURCE ONLY\n%%EOF",
        )
        .unwrap();
        let binary = root.join("mock-native");
        // Explicit ordinary fixture interpreter only; no global Python fallback.
        let python =
            std::env::var("GPTGREP_MOCK_PYTHON").expect("set the uv-managed fixture interpreter");
        assert!(Path::new(&python).is_absolute() && !python.contains(['\n', ' ']));
        private_new(
            &binary,
            format!("#!{python} -B\n{MOCK_HEADER}{body}").as_bytes(),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let schema = json!({"type":"object","properties":{"answer":{"type":"string","maxLength":12000}},"required":["answer"],"additionalProperties":false});
        private_new(
            &root.join("schema.json"),
            &serde_json::to_vec(&schema).unwrap(),
        )
        .unwrap();
        let input =
            "Synthetic question\n$(touch SHOULD_NOT_EXIST) `echo SHOULD_NOT_RUN` ' \" --model evil";
        let at = |name: &str| json!({"path":root.join(name),"sha256":sha(&fs::read(root.join(name)).unwrap())});
        let now = monotonic_ns().unwrap();
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let value = json!({
            "schema_version":1,
            "phase":{"id":"synthetic-phase","origin_receipt_sha256":"a".repeat(64),"clock_namespace":"offline-fixture:same-process-host","issued_monotonic_ns":now,"deadline_monotonic_ns":now+10_000_000_000u64,"deadline_unix_ms":wall+10000},
            "profile":{"model":"gpt-6-luna","reasoning_effort":"max","service_tier":"fast"},
            "arm":"baseline","runtime_kind":"mock","runtime":at("mock-native"),"answer_schema":at("schema.json"),
            "prompt":{"path":root.join("out/prompt.txt"),"sha256":sha(input.as_bytes())},
            "cwd":root.join("app"),"raw_dir":root.join("app/raw"),"raw_documents":[at("app/raw/document.pdf")],
            "output_dir":root.join("out"),
            "environment":{"PATH":"/usr/bin:/bin","HOME":root.join("out/home"),"CODEX_HOME":root.join("out/home/codex"),"TMPDIR":root.join("out/tmp"),"LANG":"C.UTF-8"},
            "auth_channel_receipt_sha256":"b".repeat(64),"treatment_capabilities":null,
            "limits":{"stdout_bytes":16384,"stderr_bytes":4096,"event_bytes":4096}
        });
        let path = root.join("control.json");
        Self {
            _temp: temp,
            root,
            path,
            value,
        }
    }

    fn control(&self) -> String {
        let bytes = serde_json::to_vec(&self.value).unwrap();
        if self.path.exists() {
            fs::remove_file(&self.path).unwrap();
        }
        private_new(&self.path, &bytes).unwrap();
        sha(&bytes)
    }

    fn prompt(&self) {
        private_new(&self.root.join("out/prompt.txt"), b"Synthetic question\n$(touch SHOULD_NOT_EXIST) `echo SHOULD_NOT_RUN` ' \" --model evil").unwrap();
    }

    fn run(&self) -> Value {
        self.prompt();
        execute(admit(&self.path, &self.control(), true).unwrap()).unwrap()
    }
}

const SUCCESS: &str = "prompt=sys.stdin.read()\nopen('received.json','w').write(json.dumps({'prompt':prompt,'argv':sys.argv[1:],'env_names':sorted(os.environ),'refresh_override':os.environ.get('CODEX_REFRESH_TOKEN_URL_OVERRIDE'),'revoke_override':os.environ.get('CODEX_REVOKE_TOKEN_URL_OVERRIDE'),'no_proxy':[os.environ.get('NO_PROXY'),os.environ.get('no_proxy')]}))\nprint(json.dumps({'type':'thread.started','thread_id':'MOCK-THREAD'}))\nprint(json.dumps({'type':'turn.started'}))\nfor k in ['item.started','item.updated','item.completed']:\n print(json.dumps({'type':k,'item':{'id':'MOCK-ITEM','type':'command_execution','command':'PRIVATE TOOL CONTENT'}}))\nprint(json.dumps({'type':'turn.completed','usage':{'input_tokens':11,'cached_input_tokens':2,'output_tokens':3}}))\nopen('answer.json','w').write(json.dumps({'answer':'Synthetic answer'}))\n";

#[test]
fn check_does_not_launch_or_change_phase() {
    let f = Fixture::new(SUCCESS);
    let digest = f.control();
    let a = admit(&f.path, &digest, false).unwrap();
    assert_eq!(
        serde_json::to_value(a.control.phase).unwrap(),
        f.value["phase"]
    );
    assert!(!f.root.join("app/received.json").exists());
    assert_eq!(fs::read_dir(f.root.join("out")).unwrap().count(), 2);
}

#[test]
fn direct_stdin_exact_argv_deduplicated_usage_and_null_actuals() {
    let f = Fixture::new(SUCCESS);
    let receipt = f.run();
    assert_eq!(receipt["status"], "answered_ungraded");
    assert_eq!(receipt["observations"]["unique_items"], 1);
    assert_eq!(receipt["observations"]["input_tokens"], 11);
    assert!(receipt["observations"]["reasoning_output_tokens"].is_null());
    assert!(
        receipt["actual"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    let captured: Value =
        serde_json::from_slice(&fs::read(f.root.join("app/received.json")).unwrap()).unwrap();
    assert_eq!(
        sha(captured["prompt"].as_str().unwrap().as_bytes()),
        f.value["prompt"]["sha256"]
    );
    assert_eq!(captured["argv"].as_array().unwrap().last().unwrap(), "-");
    // The runner, not the control, pins the refresh/revoke blockers and loopback NO_PROXY.
    assert_eq!(
        captured["refresh_override"],
        "http://127.0.0.1:0/oauth/token"
    );
    assert_eq!(
        captured["revoke_override"],
        "http://127.0.0.1:0/oauth/revoke"
    );
    assert_eq!(
        captured["no_proxy"],
        json!(["127.0.0.1,localhost,::1", "127.0.0.1,localhost,::1"])
    );
    assert!(
        !captured["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "agent")
    );
    assert!(!f.root.join("app/SHOULD_NOT_EXIST").exists());
    assert_eq!(receipt["phase"], f.value["phase"]);
    let safe = fs::read_to_string(f.root.join("out/native-events.jsonl")).unwrap();
    assert!(!safe.contains("PRIVATE TOOL CONTENT"));
    assert!(!safe.contains("MOCK-ITEM"));
    for name in [
        "native-run.claim",
        "answer.schema.json",
        "receipt.json",
        "native-events.jsonl",
        "prompt.txt",
    ] {
        assert_eq!(
            fs::metadata(f.root.join("out").join(name)).unwrap().mode() & 0o777,
            0o600
        );
    }
    assert!(admit(&f.path, &f.control(), true).is_err());
}

#[test]
fn baseline_refuses_preparsed_assets_unknown_options_and_profile_substitution() {
    let mut f = Fixture::new(SUCCESS);
    private_new(&f.root.join("app/raw/parsed.txt"), b"DERIVED").unwrap();
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("raw_inventory_mismatch")
    ));
    fs::remove_file(f.root.join("app/raw/parsed.txt")).unwrap();
    f.value["resume"] = json!(true);
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("control_schema")
    ));
    f.value.as_object_mut().unwrap().remove("resume");
    f.value["profile"]["model"] = json!("other");
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("profile_or_version")
    ));
}

#[test]
fn schema_and_prompt_snapshot_hashes_refuse_tampering() {
    let f = Fixture::new(SUCCESS);
    let d = f.control();
    fs::write(f.root.join("schema.json"), b"{}").unwrap();
    assert!(matches!(admit(&f.path, &d, false), Err("asset_digest")));
    let f = Fixture::new(SUCCESS);
    f.prompt();
    let d = f.control();
    fs::write(f.root.join("out/prompt.txt"), b"tampered").unwrap();
    assert!(matches!(admit(&f.path, &d, true), Err("asset_digest")));
}

#[test]
fn symlink_and_stale_answer_refused_before_claim() {
    let f = Fixture::new(SUCCESS);
    std::os::unix::fs::symlink(f.root.join("schema.json"), f.root.join("app/answer.json")).unwrap();
    f.prompt();
    assert!(matches!(
        admit(&f.path, &f.control(), true),
        Err("stale_answer")
    ));
    assert!(!f.root.join("out/native-run.claim").exists());
    let f = Fixture::new(SUCCESS);
    fs::remove_file(f.root.join("schema.json")).unwrap();
    std::os::unix::fs::symlink(f.root.join("control.json"), f.root.join("schema.json")).unwrap();
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("symlink_path")
    ));
}

#[test]
fn missing_answer_not_repaired_from_final_message() {
    let f = Fixture::new(
        "sys.stdin.read()\nprint('{\"type\":\"turn.started\"}')\nprint('{\"type\":\"turn.completed\"}')\np=sys.argv[sys.argv.index('--output-last-message')+1]\nopen(p,'w').write('{\"answer\":\"must not repair\"}')\n",
    );
    let receipt = f.run();
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["failure"], "path_missing");
    assert!(receipt["answer"].is_null());
    assert!(receipt["observations"]["input_tokens"].is_null());
    assert!(!f.root.join("app/answer.json").exists());
}

#[test]
fn native_nonzero_preserves_partial_usage_and_no_retry() {
    let f = Fixture::new(&format!("{SUCCESS}sys.exit(9)\n"));
    let receipt = f.run();
    assert_eq!(receipt["native_exit_code"], 9);
    assert_eq!(receipt["failure"], "native_nonzero");
    assert_eq!(receipt["observations"]["input_tokens"], 11);
    assert!(receipt["answer"].is_null());
    assert!(receipt["owned_leader_reaped"].as_bool().unwrap());
}

#[test]
fn deadline_is_not_renewed_after_check_or_cleanup() {
    let mut f = Fixture::new("sys.stdin.read()\ntime.sleep(30)\n");
    f.value["phase"]["deadline_monotonic_ns"] = json!(monotonic_ns().unwrap() + 150_000_000u64);
    let d = f.control();
    admit(&f.path, &d, false).unwrap();
    f.prompt();
    let admitted = admit(&f.path, &d, true).unwrap();
    let receipt = execute(admitted).unwrap();
    assert_eq!(receipt["failure"], "phase_deadline");
    assert_eq!(receipt["phase"], f.value["phase"]);
    assert!(receipt["owned_leader_reaped"].as_bool().unwrap());
    assert!(receipt["latency_ms"].as_u64().unwrap() < 3000);
}

#[test]
fn bounded_stdout_stderr_and_event_admission() {
    for (body, failure) in [
        (
            "sys.stdin.read()\nos.write(1,b'x'*20000)\ntime.sleep(30)\n",
            "event_limit",
        ),
        (
            "sys.stdin.read()\nos.write(2,b'x'*20000)\ntime.sleep(30)\n",
            "stderr_limit",
        ),
    ] {
        let f = Fixture::new(body);
        let receipt = f.run();
        assert_eq!(receipt["failure"], failure);
        assert!(receipt["stdout_admitted_bytes"].as_u64().unwrap() <= 16384);
        assert!(receipt["stderr_admitted_bytes"].as_u64().unwrap() <= 4096);
        assert!(receipt["owned_leader_reaped"].as_bool().unwrap());
    }
}

#[test]
fn descendant_group_is_stopped_without_claiming_descendant_reaping() {
    let f = Fixture::new(
        "sys.stdin.read()\npid=os.fork()\nif pid==0:\n time.sleep(30)\n sys.exit(0)\nopen('descendant.pid','w').write(str(pid))\nprint('{\"type\":\"turn.started\"}',flush=True)\nsys.exit(7)\n",
    );
    let receipt = f.run();
    assert_eq!(receipt["native_exit_code"], 7);
    assert_eq!(receipt["descendant_reaping_qualified"], false);
    assert!(receipt["latency_ms"].as_u64().unwrap() < 3000);
    assert!(f.root.join("app/descendant.pid").exists());
}

#[test]
fn symmetric_native_argv_and_full_treatment_references_required() {
    let f = Fixture::new(SUCCESS);
    let mut control: Control = serde_json::from_value(f.value.clone()).unwrap();
    let baseline = argv(&control);
    control.arm = "treatment".into();
    assert_eq!(argv(&control), baseline);
    let mut f = f;
    f.value["arm"] = json!("treatment");
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("arm_capabilities")
    ));
    f.value["arm"] = json!("baseline");
    f.value["environment"]["OPENAI_API_KEY"] = json!("SYNTHETIC_VALUE_NOT_A_KEY");
    assert!(matches!(
        admit(&f.path, &f.control(), false),
        Err("environment_allowlist")
    ));
}

#[test]
fn stdout_total_limit_is_distinct_from_one_event_limit() {
    let f = Fixture::new(
        "sys.stdin.read()\nfor n in range(2000):\n print(json.dumps({'type':'item.updated','item':{'id':'one','type':'reasoning'}}),flush=True)\ntime.sleep(30)\n",
    );
    let receipt = f.run();
    assert_eq!(receipt["failure"], "stdout_limit");
    assert!(receipt["stdout_admitted_bytes"].as_u64().unwrap() <= 16384);
    assert_eq!(receipt["observations"]["unique_items"], 1);
}

#[test]
fn answer_character_byte_and_symlink_contracts_are_enforced() {
    let f = Fixture::new(SUCCESS);
    let control: Control = serde_json::from_value(f.value).unwrap();
    let path = f.root.join("app/answer.json");
    private_new(
        &path,
        &serde_json::to_vec(&json!({"answer":"x".repeat(12001)})).unwrap(),
    )
    .unwrap();
    assert!(matches!(answer(&control), Err("answer_contract")));
    fs::remove_file(&path).unwrap();
    private_new(&path, &vec![b'x'; 65537]).unwrap();
    assert!(matches!(answer(&control), Err("input_size_or_kind")));
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(f.root.join("schema.json"), &path).unwrap();
    assert!(matches!(answer(&control), Err("symlink_path")));
}

#[test]
fn admitted_control_schema_prompt_buffers_are_not_reopened() {
    let f = Fixture::new(SUCCESS);
    f.prompt();
    let d = f.control();
    let admitted = admit(&f.path, &d, true).unwrap();
    fs::write(&f.path, b"{}").unwrap();
    fs::write(f.root.join("schema.json"), b"{}").unwrap();
    fs::write(f.root.join("out/prompt.txt"), b"changed").unwrap();
    let receipt = execute(admitted).unwrap();
    assert_eq!(receipt["status"], "answered_ungraded");
    let captured: Value =
        serde_json::from_slice(&fs::read(f.root.join("app/received.json")).unwrap()).unwrap();
    assert_eq!(
        sha(captured["prompt"].as_str().unwrap().as_bytes()),
        f.value["prompt"]["sha256"]
    );
    assert_eq!(
        sha(&fs::read(f.root.join("out/answer.schema.json")).unwrap()),
        f.value["answer_schema"]["sha256"]
    );
}

#[test]
fn expired_phase_before_launch_has_no_claim_or_child() {
    let mut f = Fixture::new(SUCCESS);
    f.value["phase"]["deadline_monotonic_ns"] = json!(monotonic_ns().unwrap() + 30_000_000);
    f.prompt();
    let d = f.control();
    let admitted = admit(&f.path, &d, true).unwrap();
    std::thread::sleep(Duration::from_millis(40));
    assert!(matches!(execute(admitted), Err("phase_deadline")));
    assert!(!f.root.join("out/native-run.claim").exists());
    assert!(!f.root.join("app/received.json").exists());
}

#[test]
fn malformed_usage_and_duplicate_terminal_do_not_inflate_observations() {
    let mut events = Events::default();
    events.line(br#"{"type":"turn.started"}"#).unwrap();
    assert!(matches!(
        events.line(br#"{"type":"turn.completed","usage":{"input_tokens":-1}}"#),
        Err("native_usage_number")
    ));
    let mut events = Events::default();
    events.line(br#"{"type":"turn.started"}"#).unwrap();
    let terminal = br#"{"type":"turn.completed","usage":{"input_tokens":11}}"#;
    events.line(terminal).unwrap();
    assert!(matches!(
        events.line(terminal),
        Err("duplicate_or_unstarted_terminal")
    ));
    assert_eq!(events.observations.completed_turns, 1);
    assert_eq!(events.observations.input_tokens, Some(11));
}

struct FixtureInterruption(bool);
impl FixtureInterruption {
    fn new() -> Self {
        Self(INTERRUPTED.swap(true, Ordering::Relaxed))
    }
}
impl Drop for FixtureInterruption {
    fn drop(&mut self) {
        INTERRUPTED.store(self.0, Ordering::Relaxed);
    }
}

#[test]
fn preset_interruption_refuses_admission_and_preclaim_launch() {
    let f = Fixture::new(SUCCESS);
    f.prompt();
    let digest = f.control();
    {
        let _interruption = FixtureInterruption::new();
        assert!(matches!(
            admit(&f.path, &digest, true),
            Err("launcher_interrupted")
        ));
    }
    let admitted = admit(&f.path, &digest, true).unwrap();
    {
        let _interruption = FixtureInterruption::new();
        assert!(matches!(execute(admitted), Err("launcher_interrupted")));
    }
    assert!(!f.root.join("out/native-run.claim").exists());
    assert!(!f.root.join("out/answer.schema.json").exists());
    assert!(!f.root.join("app/received.json").exists());
}

#[test]
fn invalid_later_terminal_fields_preserve_complete_prior_state() {
    for field in [
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
    ] {
        let mut events = Events::default();
        events.line(br#"{"type":"turn.started"}"#).unwrap();
        events.line(br#"{"type":"turn.completed","usage":{"input_tokens":11,"cached_input_tokens":2,"cache_write_input_tokens":1,"output_tokens":3,"reasoning_output_tokens":4}}"#).unwrap();
        events.line(br#"{"type":"turn.started"}"#).unwrap();
        let prior = serde_json::to_value(&events.observations).unwrap();
        let prior_projection = events.projected.clone();
        let mut bad = json!({"type":"turn.completed","usage":{"input_tokens":7,"cached_input_tokens":1,"cache_write_input_tokens":1,"output_tokens":1,"reasoning_output_tokens":1}});
        bad["usage"][field] = json!(-1);
        assert!(matches!(
            events.line(&serde_json::to_vec(&bad).unwrap()),
            Err("native_usage_number")
        ));
        assert_eq!(serde_json::to_value(&events.observations).unwrap(), prior);
        assert_eq!(events.projected, prior_projection);
        assert!(events.active_turn);
    }
}

#[test]
fn later_terminal_sum_overflow_preserves_complete_prior_state() {
    let mut events = Events::default();
    events.line(br#"{"type":"turn.started"}"#).unwrap();
    events.line(&serde_json::to_vec(&json!({"type":"turn.completed","usage":{"input_tokens":11,"cached_input_tokens":u64::MAX,"output_tokens":3}})).unwrap()).unwrap();
    events.line(br#"{"type":"turn.started"}"#).unwrap();
    let prior = serde_json::to_value(&events.observations).unwrap();
    let prior_projection = events.projected.clone();
    assert!(matches!(events.line(br#"{"type":"turn.completed","usage":{"input_tokens":7,"cached_input_tokens":1,"output_tokens":1}}"#),Err("usage_overflow")));
    assert_eq!(serde_json::to_value(&events.observations).unwrap(), prior);
    assert_eq!(events.projected, prior_projection);
    assert!(events.active_turn);
}

#[test]
fn terminal_projection_limit_preserves_usage_and_active_state() {
    let mut events = Events::default();
    events.line(br#"{"type":"turn.started"}"#).unwrap();
    events.projected = vec![b'x'; 1_048_576];
    let prior = serde_json::to_value(&events.observations).unwrap();
    assert!(matches!(
        events.line(br#"{"type":"turn.completed","usage":{"input_tokens":11}}"#),
        Err("projection_limit")
    ));
    assert_eq!(serde_json::to_value(&events.observations).unwrap(), prior);
    assert!(events.active_turn);
}

#[test]
fn stale_pending_receipt_refuses_admission_and_preclaim_launch() {
    let f = Fixture::new(SUCCESS);
    f.prompt();
    let digest = f.control();
    let admitted = admit(&f.path, &digest, true).unwrap();
    private_new(
        &f.root.join("out/.receipt.pending"),
        b"EXISTING_SYNTHETIC_PENDING",
    )
    .unwrap();
    assert!(matches!(admit(&f.path, &digest, true), Err("stale_output")));
    assert!(matches!(execute(admitted), Err("stale_output")));
    assert!(!f.root.join("out/native-run.claim").exists());
    assert!(!f.root.join("app/received.json").exists());
    assert_eq!(
        fs::read(f.root.join("out/.receipt.pending")).unwrap(),
        b"EXISTING_SYNTHETIC_PENDING"
    );
}

// Actual v2 publication/index/Reader over generic MOCK parsed artifacts. No native parser.
mod generation {
    use super::*;
    use gptgrep_core::{RichGenerationReader, RichGenerationWriter};
    use gptgrep_parse::{
        AffineTransform, ArtifactKind, ArtifactRef, CoverageState, FeatureState, NativePage,
        OcrImage, PageGeometry, PreparedSummary, RICH_PARSER, ResourceBudget, RichParsedDocument,
    };
    async fn parsed_fixture(
        root: &Path,
        texts: &[&str],
    ) -> anyhow::Result<(tempfile::TempDir, RichParsedDocument)> {
        let staging = tempfile::tempdir()?;
        let staging_path = staging.path().canonicalize()?;
        let source_bytes = fs::read(root.join("document.pdf"))?;
        let source = source_bytes.as_slice();
        fs::write(staging_path.join("prepared.pdf"), source)?;
        let source_sha = sha(source);
        let profile = sha(b"synthetic parser recipe");
        let mut artifacts = Vec::new();
        let mut pages = Vec::new();
        let mut create = |kind: ArtifactKind,
                          page: Option<u32>,
                          bytes: &[u8],
                          name: &str|
         -> anyhow::Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(
                    &source_sha,
                    &profile,
                    &kind,
                    page,
                    None,
                    &sha(bytes),
                )?,
                kind,
                relative_path: name.into(),
                sha256: sha(bytes),
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
        fs::write(staging_path.join("fixture.txt"), texts.concat())?;
        let mut document = if texts.len() == 1 {
            gptgrep_parse::parse_path(&staging_path.join("fixture.txt")).await?
        } else {
            let inputs: Vec<_> = texts
                .iter()
                .enumerate()
                .map(|(index, text)| gptgrep_pageindex::PageInput {
                    number: index as u32 + 1,
                    text: (*text).into(),
                })
                .collect();
            gptgrep_pageindex::from_pages("synthetic", &inputs, &[], &[], RICH_PARSER)?
        };
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
                    source_sha256: source_sha.clone(),
                    prepared_sha256: source_sha,
                    source_bytes: source.len() as u64,
                    prepared_bytes: source.len() as u64,
                    format: "pdf".into(),
                    parser_identity: RICH_PARSER.into(),
                    preparation_profile_sha256: sha(&serde_json::to_vec(&(
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

    struct Generated {
        f: Fixture,
        root: PathBuf,
        capabilities: Value,
    }
    impl Generated {
        async fn new() -> Self {
            Self::with_text("ordinary MOCK copper fixture\n").await
        }
        async fn with_text(text: &str) -> Self {
            Self::with_pages(&[text]).await
        }
        async fn with_pages(texts: &[&str]) -> Self {
            Self::with_documents(texts, 1).await
        }
        async fn with_documents(texts: &[&str], count: usize) -> Self {
            let mut f = Fixture::new("open('child-marker','w').write('MOCK')\n");
            let raw = f.root.join("app/raw");
            let mut writer = RichGenerationWriter::begin(&raw, ResourceBudget::default()).unwrap();
            for index in 0..count {
                let name = if index == 0 {
                    "document.pdf".to_owned()
                } else {
                    format!("document-{index}.pdf")
                };
                if index != 0 {
                    let path = raw.join(&name);
                    fs::copy(raw.join("document.pdf"), &path).unwrap();
                    f.value["raw_documents"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"path":path,"sha256":sha(&fs::read(&path).unwrap())}));
                }
                let (stage, parsed) = parsed_fixture(&raw, texts).await.unwrap();
                writer
                    .add_document(&name, &stage.path().canonicalize().unwrap(), parsed)
                    .unwrap();
            }
            let pin = writer.publish().unwrap();
            RichGenerationReader::open_pinned(&raw, &pin)
                .unwrap()
                .verify_all()
                .unwrap();
            // Exported consumer topology contains CURRENT + the selected generation, not writer locks.
            fs::remove_file(raw.join(".gptgrep/write.lock")).unwrap();
            let root = raw.join(".gptgrep/generations").join(&pin.generation);
            let mut inventory = Vec::new();
            fn walk(root: &Path, dir: &Path, inventory: &mut Vec<Value>) {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(root, &path, inventory);
                    } else {
                        let bytes = fs::read(&path).unwrap();
                        inventory.push(json!({"path":path.strip_prefix(root).unwrap(),"bytes":bytes.len(),"sha256":sha(&bytes)}));
                    }
                }
            }
            walk(&root, &root, &mut inventory);
            inventory.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            let capabilities = json!({"schema_version":2,"generation_v2":pin.manifest_sha256,
                "regex":"a".repeat(64),"tree":"a".repeat(64),"visual":"a".repeat(64),
                "multimodal":"a".repeat(64),"decisions_helper":"a".repeat(64),
                "generation_binding":{"pin":pin,"current_sha256":sha(&fs::read(raw.join(".gptgrep/CURRENT.json")).unwrap()),"inventory":inventory}});
            f.value["arm"] = json!("treatment");
            // New synthetic fixture phase starts after offline index construction; production never reanchors.
            let now = monotonic_ns().unwrap();
            let wall = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            f.value["phase"]["issued_monotonic_ns"] = json!(now);
            f.value["phase"]["deadline_monotonic_ns"] = json!(now + 10_000_000_000u64);
            f.value["phase"]["deadline_unix_ms"] = json!(wall + 10000);
            let mut result = Self {
                f,
                root,
                capabilities,
            };
            result.save();
            result
        }
        fn save(&mut self) {
            let path = self.f.root.join("capabilities.json");
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            let bytes = serde_json::to_vec(&self.capabilities).unwrap();
            private_new(&path, &bytes).unwrap();
            self.f.value["treatment_capabilities"] = json!({"path":path,"sha256":sha(&bytes)});
        }
        fn rejected(&self) -> &'static str {
            let result = admit(&self.f.path, &self.f.control(), false);
            assert!(!self.f.root.join("app/child-marker").exists());
            assert!(!self.f.root.join("out/native-run.claim").exists());
            result.err().expect("must refuse before launch/claim")
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn genuine_pinned_generation_admits_check_only_and_baseline_stays_pdf_only() {
        let mut g = Generated::new().await;
        admit(&g.f.path, &g.f.control(), false).unwrap();
        assert!(!g.f.root.join("out/native-run.claim").exists());
        assert!(!g.f.root.join("app/child-marker").exists());
        g.f.value["arm"] = json!("baseline");
        g.f.value["treatment_capabilities"] = Value::Null;
        assert_eq!(g.rejected(), "raw_inventory_mismatch");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn real_foreign_engine_with_refreshed_inventory_refuses_before_claim_and_child() {
        let mut g = Generated::new().await;
        let foreign = Generated::with_text("different MOCK silver material\n").await;
        admit(&g.f.path, &g.f.control(), false).unwrap();
        for name in ["lookup.bin", "index.bin"] {
            fs::copy(
                foreign.root.join("artifact-index").join(name),
                g.root.join("artifact-index").join(name),
            )
            .unwrap();
        }
        let meta_path = g.root.join("artifact-index/meta.json");
        let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta["num_trigrams"] = json!(
            fs::metadata(g.root.join("artifact-index/lookup.bin"))
                .unwrap()
                .len()
                / 16
        );
        fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        // Preserve original normalized source and file table, while refreshing
        // every replaced byte's caller inventory. Reader.verify_all alone passes.
        RichGenerationReader::open_pinned(
            &g.f.root.join("app/raw"),
            &serde_json::from_value(g.capabilities["generation_binding"]["pin"].clone()).unwrap(),
        )
        .unwrap()
        .verify_all()
        .unwrap();
        for entry in g.capabilities["generation_binding"]["inventory"]
            .as_array_mut()
            .unwrap()
        {
            let bytes = fs::read(g.root.join(entry["path"].as_str().unwrap())).unwrap();
            entry["bytes"] = json!(bytes.len());
            entry["sha256"] = json!(sha(&bytes));
        }
        g.save();
        assert_eq!(g.rejected(), "generation_index_content");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn operator_version_one_is_not_a_generation_binding() {
        let mut g = Generated::new().await;
        g.capabilities["schema_version"] = json!(1);
        g.capabilities
            .as_object_mut()
            .unwrap()
            .remove("generation_binding");
        g.save();
        assert_eq!(g.rejected(), "capability_schema");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn changed_current_or_missing_generation_refuses() {
        let g = Generated::new().await;
        fs::write(g.f.root.join("app/raw/.gptgrep/CURRENT.json"), b"{}").unwrap();
        assert!(g.rejected().starts_with("generation_current"));
        let g = Generated::new().await;
        fs::remove_file(g.root.join("artifact-index/index.bin")).unwrap();
        assert_eq!(g.rejected(), "path_missing");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn tampered_artifact_and_self_consistent_inventory_cannot_bypass_reader() {
        let mut g = Generated::new().await;
        let entry = g.capabilities["generation_binding"]["inventory"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["path"].as_str().unwrap().ends_with("prepared.pdf"))
            .unwrap();
        let path = g.root.join(entry["path"].as_str().unwrap());
        let bytes = b"self consistent changed MOCK prepared bytes";
        fs::write(path, bytes).unwrap();
        entry["bytes"] = json!(bytes.len());
        entry["sha256"] = json!(sha(bytes));
        g.save();
        assert_eq!(g.rejected(), "generation_verification");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn undeclared_files_empty_history_and_symlinks_are_not_excluded() {
        for kind in ["raw", "artifact", "history", "hidden", "symlink"] {
            let g = Generated::new().await;
            match kind {
                "raw" => fs::write(g.f.root.join("app/raw/extra.pdf"), b"extra").unwrap(),
                "artifact" => fs::write(g.root.join("extra.txt"), b"extra").unwrap(),
                "history" => {
                    fs::create_dir(g.f.root.join("app/raw/.gptgrep/generations/extra")).unwrap()
                }
                "hidden" => fs::write(g.f.root.join("app/raw/.gptgrep/.extra"), b"extra").unwrap(),
                _ => std::os::unix::fs::symlink(
                    g.f.root.join("schema.json"),
                    g.root.join("extra-link"),
                )
                .unwrap(),
            }
            let code = g.rejected();
            assert!(
                matches!(
                    code,
                    "raw_inventory_mismatch"
                        | "generation_inventory_mismatch"
                        | "generation_symlink"
                ),
                "{kind}: {code}"
            );
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_escaping_inventory_and_noncanonical_root_refuse() {
        let mut g = Generated::new().await;
        let entry = g.capabilities["generation_binding"]["inventory"][0].clone();
        g.capabilities["generation_binding"]["inventory"]
            .as_array_mut()
            .unwrap()
            .push(entry);
        g.save();
        assert_eq!(g.rejected(), "generation_duplicate");
        let mut g = Generated::new().await;
        g.capabilities["generation_binding"]["inventory"][0]["path"] = json!("../outside");
        g.save();
        assert_eq!(g.rejected(), "generation_path");
        let mut g = Generated::new().await;
        g.f.value["raw_dir"] = json!(g.f.root.join("app/raw/.."));
        assert!(matches!(
            g.rejected(),
            "path_component" | "generation_binding"
        ));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn changed_generation_after_admission_refuses_before_claim_or_child() {
        let g = Generated::new().await;
        g.f.prompt();
        let admitted = admit(&g.f.path, &g.f.control(), true).unwrap();
        fs::write(g.root.join("artifact-index/index.bin"), b"changed index").unwrap();
        assert!(execute(admitted).is_err());
        assert!(!g.f.root.join("out/native-run.claim").exists());
        assert!(!g.f.root.join("app/child-marker").exists());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn changed_raw_source_is_rejected_even_with_refreshed_control() {
        let mut g = Generated::new().await;
        let path = g.f.root.join("app/raw/document.pdf");
        fs::write(&path, b"%PDF-1.4 changed MOCK source").unwrap();
        g.f.value["raw_documents"][0]["sha256"] = json!(sha(&fs::read(path).unwrap()));
        assert_eq!(g.rejected(), "generation_sources");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn self_consistent_extra_inventory_and_index_corruption_refuse() {
        let mut g = Generated::new().await;
        fs::write(g.root.join("extra.txt"), b"extra").unwrap();
        g.capabilities["generation_binding"]["inventory"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path":"extra.txt","bytes":5,"sha256":sha(b"extra")}));
        g.save();
        assert_eq!(g.rejected(), "generation_complete_inventory");
        let mut g = Generated::new().await;
        let path = g.root.join("artifact-index/index.bin");
        fs::write(path, b"bad").unwrap();
        let entry = g.capabilities["generation_binding"]["inventory"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["path"] == "artifact-index/index.bin")
            .unwrap();
        entry["bytes"] = json!(3);
        entry["sha256"] = json!(sha(b"bad"));
        g.save();
        assert_eq!(g.rejected(), "generation_index_content");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refreshed_manifest_pin_cannot_relocate_canonical_root() {
        let mut g = Generated::new().await;
        let path = g.root.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["root"] = json!(g.f.root.join("different-root"));
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(&path, &bytes).unwrap();
        let digest = sha(&bytes);
        let binding = &mut g.capabilities["generation_binding"];
        binding["pin"]["manifest_sha256"] = json!(digest);
        let entry = binding["inventory"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["path"] == "manifest.json")
            .unwrap();
        entry["bytes"] = json!(bytes.len());
        entry["sha256"] = json!(digest);
        let current = json!({"schema_version":"gptgrep.manifest.v2","generation":binding["pin"]["generation"],"manifest_sha256":digest});
        let bytes = serde_json::to_vec(&current).unwrap();
        fs::write(g.f.root.join("app/raw/.gptgrep/CURRENT.json"), &bytes).unwrap();
        binding["current_sha256"] = json!(sha(&bytes));
        g.save();
        assert_eq!(g.rejected(), "generation_reader");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generation_inventory_and_size_caps_are_preadmitted() {
        let mut g = Generated::new().await;
        let entry = g.capabilities["generation_binding"]["inventory"][0].clone();
        g.capabilities["generation_binding"]["inventory"] =
            json!(vec![entry; MAX_GENERATION_FILES + 1]);
        g.save();
        assert_eq!(g.rejected(), "generation_binding");
        let mut g = Generated::new().await;
        g.capabilities["generation_binding"]["inventory"][0]["bytes"] = json!(67_108_865u64);
        g.save();
        assert_eq!(g.rejected(), "generation_file_limit_or_digest");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_inventory_above_pilot_counts_admits_and_unlisted_entries_refuse() {
        // Tiny synthetic artifacts exercise the real writer/index/reader, without a large PDF.
        // Two 140-page documents yield >512 files, >256 artifacts per document,
        // and >256 actual indexed text files, while each document stays below 256 pages.
        let g = Generated::with_documents(&["small synthetic native text\n"; 140], 2).await;
        let entries = g.capabilities["generation_binding"]["inventory"]
            .as_array()
            .unwrap();
        assert!(entries.len() > 512 && entries.len() <= MAX_GENERATION_FILES);
        assert!(
            entries
                .iter()
                .filter(|entry| entry["path"]
                    .as_str()
                    .unwrap()
                    .starts_with("artifact-text/"))
                .count()
                > 256
        );
        let bytes = serde_json::to_vec(&g.capabilities).unwrap();
        assert!(bytes.len() > 65_536 && bytes.len() <= MAX_CAPABILITY_BYTES);
        admit(&g.f.path, &g.f.control(), false).unwrap();
        assert!(!g.f.root.join("out/native-run.claim").exists());
        assert!(!g.f.root.join("app/child-marker").exists());
        let extra = g.root.join("unlisted.txt");
        fs::write(&extra, b"unlisted").unwrap();
        assert_eq!(g.rejected(), "generation_inventory_mismatch");
        fs::remove_file(extra).unwrap();
        let extra_dir = g.f.root.join("app/raw/unlisted-dir");
        fs::create_dir(&extra_dir).unwrap();
        assert_eq!(g.rejected(), "raw_inventory_mismatch");
        fs::remove_dir(extra_dir).unwrap();
        admit(&g.f.path, &g.f.control(), false).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capability_metadata_budget_is_checked_before_deserialization() {
        let mut g = Generated::new().await;
        let path = g.f.root.join("capabilities.json");
        let bytes = vec![b' '; MAX_CAPABILITY_BYTES + 1];
        fs::write(&path, &bytes).unwrap();
        g.f.value["treatment_capabilities"] = json!({"path":path,"sha256":sha(&bytes)});
        assert_eq!(g.rejected(), "input_size_or_kind");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refreshed_index_manifest_cannot_select_merged_text_or_omit_artifacts() {
        for change in ["root", "files"] {
            let mut g = Generated::new().await;
            let path = g.root.join("artifact-index/gptgrep-index.json");
            let mut index: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if change == "root" {
                index["root_relative"] = json!("../text");
            } else {
                index["files"] = json!({});
            }
            let bytes = serde_json::to_vec(&index).unwrap();
            fs::write(path, &bytes).unwrap();
            let entry = g.capabilities["generation_binding"]["inventory"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["path"] == "artifact-index/gptgrep-index.json")
                .unwrap();
            entry["bytes"] = json!(bytes.len());
            entry["sha256"] = json!(sha(&bytes));
            g.save();
            assert_eq!(g.rejected(), "generation_index_sources");
        }
    }
}
#[test]
fn treatment_tool_events_project_pinned_search_and_read_without_raw_contents() {
    let pin = gptgrep_core::GenerationPin {
        generation: "generic-generation".into(),
        manifest_sha256: "a".repeat(64),
    };
    let context = ToolContext {
        root: "/app/groups/group-a/raw".into(),
        pin: pin.clone(),
    };
    let window = json!({"text":"PRIVATE_GENERIC_DOCUMENT_TEXT","next_offset":null,"evidence":{"kind":"native_text","binding":{"generation":pin,"source_sha256":"b".repeat(64),"artifact_sha256":"c".repeat(64)}}});
    let response = json!({"schema_version":"gptgrep.batch.rich.response.v1","generation":pin,"execution":{"scope":"local_deterministic_no_model_context","model_submissions":0},"items":[{"id":"search","status":"ok","error":null,"coverage":{"unit":"bounded_search_hits"},"result":{"hits":[window.clone()]}},{"id":"read","status":"ok","coverage":{"unit":"bytes"},"result":window}]});
    let command =
        "/app/tool/gptgrep batch --rich --root /app/groups/group-a/raw --input /app/request.json";
    let item = json!({"id":"private-native-item-id","type":"command_execution","status":"completed","exit_code":0,"command":command,"aggregated_output":response.to_string()});
    let mut events = Events {
        tool_context: Some(context),
        ..Events::default()
    };
    events
        .line(&serde_json::to_vec(&json!({"type":"item.completed","item":item})).unwrap())
        .unwrap();
    let projection: Value = serde_json::from_slice(&events.projected).unwrap();
    assert_eq!(
        projection["tool_use"],
        json!({
            "schema_version":"gptgrep.harbor.tool_use_item.v1",
            "scope":"native_command_event_and_typed_output_observation_only",
            "raw_root_sha256":sha(b"/app/groups/group-a/raw"), "generation":pin,
            "command_sha256":sha(command.as_bytes()),
            "output_sha256":sha(response.to_string().as_bytes()),
            "native_text_reads":1, "native_text_searches":1, "native_text_windows":2
        })
    );
    assert_eq!(projection["tool_use"]["native_text_searches"], 1);
    assert_eq!(projection["tool_use"]["native_text_reads"], 1);
    assert_eq!(projection["tool_use"]["native_text_windows"], 2);
    assert_eq!(
        projection["tool_use"]["output_sha256"],
        sha(response.to_string().as_bytes())
    );
    assert_eq!(projection["item_observation"]["lifecycle"], "completed");
    assert_eq!(
        projection["item_observation"]["command"]["classification"],
        "pinned_batch_direct"
    );
    assert!(projection["item_observation"]["command"]["rejection_reason"].is_null());
    assert_eq!(
        projection["item_observation"]["command"]["output_sha256"],
        projection["tool_use"]["output_sha256"]
    );
    let serialized = String::from_utf8(events.projected).unwrap();
    assert!(!serialized.contains(command));
    assert!(!serialized.contains("PRIVATE_GENERIC_DOCUMENT_TEXT"));
    assert!(!serialized.contains("private-native-item-id"));
}

#[test]
fn bounded_direct_tool_command_rejects_discovery_echo_chains_and_substitution() {
    let root = "/app/groups/group-a/raw";
    let command = format!("/app/tool/gptgrep batch --rich --root {root} --input /app/request.json");
    for valid in [
        command.clone(),
        format!("/bin/bash -lc '{command}'"),
        format!("/bin/zsh -lc \"{command}\""),
        format!("/app/tool/gptgrep batch --input /app/request.json --root {root} --rich"),
    ] {
        assert!(pinned_batch_command(&valid, root), "{valid}");
    }
    for invalid in [
        "/app/tool/gptgrep --schema".into(),
        format!("echo {command}"),
        format!("{command}; echo done"),
        command.replace("/app/request.json", "$(echo /app/request.json)"),
        command.replace("/app/request.json", "/app/../outside"),
        command.replace(root, "/app/groups/other/raw"),
        "x".repeat(8193),
    ] {
        assert!(!pinned_batch_command(&invalid, root), "{invalid}");
    }
}

#[test]
fn tool_observation_never_promotes_baseline_failure_foreign_pin_or_metadata() {
    let pin = gptgrep_core::GenerationPin {
        generation: "generic-generation".into(),
        manifest_sha256: "a".repeat(64),
    };
    let context = ToolContext {
        root: "/app/groups/group-a/raw".into(),
        pin: pin.clone(),
    };
    let response = json!({"schema_version":"gptgrep.batch.rich.response.v1","generation":pin,"execution":{"scope":"local_deterministic_no_model_context","model_submissions":0},"items":[{"id":"search","status":"ok","coverage":{"unit":"bounded_search_hits"},"result":{"hits":[]}}]});
    let item = json!({"id":"item","type":"command_execution","status":"completed","exit_code":0,"command":"/app/tool/gptgrep batch --rich --root /app/groups/group-a/raw --input /app/request.json","aggregated_output":response.to_string()});
    assert_eq!(
        tool_projection(&item, &context).unwrap()["native_text_windows"],
        0
    );
    let mut baseline = Events::default();
    baseline
        .line(&serde_json::to_vec(&json!({"type":"item.completed","item":item.clone()})).unwrap())
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&baseline.projected).unwrap()["tool_use"].is_null());
    for (key, value) in [
        ("exit_code", json!(2)),
        ("exit_code", json!(false)),
        ("status", json!("failed")),
        ("aggregated_output", json!("malformed")),
    ] {
        let mut bad = item.clone();
        bad[key] = value;
        assert!(tool_projection(&bad, &context).is_none());
    }
    let mut foreign = response.clone();
    foreign["generation"]["generation"] = json!("other");
    let mut bad = item.clone();
    bad["aggregated_output"] = json!(foreign.to_string());
    assert!(tool_projection(&bad, &context).is_none());
    let mut metadata = response;
    metadata["items"][0]["coverage"]["unit"] = json!("objects");
    bad["aggregated_output"] = json!(metadata.to_string());
    assert!(tool_projection(&bad, &context).is_none());
}

fn tool_diagnostic_fixture() -> (ToolContext, Value, Value) {
    let pin = gptgrep_core::GenerationPin {
        generation: "generic-generation".into(),
        manifest_sha256: "a".repeat(64),
    };
    let context = ToolContext {
        root: "/app/groups/group-a/raw".into(),
        pin: pin.clone(),
    };
    let response = json!({"schema_version":"gptgrep.batch.rich.response.v1","generation":pin,"execution":{"scope":"local_deterministic_no_model_context","model_submissions":0},"items":[{"status":"ok","coverage":{"unit":"bounded_search_hits"},"result":{"hits":[]}}]});
    let item = json!({"id":"private-item","type":"command_execution","status":"completed","exit_code":0,"command":"/app/tool/gptgrep batch --rich --root /app/groups/group-a/raw --input /app/request.json","aggregated_output":response.to_string()});
    (context, item, response)
}

fn tool_diagnostic_event(context: Option<ToolContext>, event: &str, item: Value) -> Value {
    let mut events = Events {
        tool_context: context,
        ..Events::default()
    };
    events
        .line(&serde_json::to_vec(&json!({"type":event,"item":item})).unwrap())
        .unwrap();
    serde_json::from_slice(&events.projected).unwrap()
}

#[test]
fn tool_diagnostics_classify_failed_unsupported_and_invalid_outputs_without_promoting() {
    let (context, item, response) = tool_diagnostic_fixture();
    let mut cases = vec![];
    for (key, value, reason) in [
        ("status", json!("failed"), "not_completed_status"),
        ("exit_code", json!(7), "nonzero_exit"),
        ("exit_code", json!(false), "exit_code_missing_or_invalid"),
        (
            "command",
            json!("echo SECRET_COMMAND"),
            "unsupported_command",
        ),
        (
            "aggregated_output",
            Value::Null,
            "output_missing_or_invalid",
        ),
        (
            "aggregated_output",
            json!("SECRET_MALFORMED"),
            "output_json_invalid",
        ),
        (
            "aggregated_output",
            json!("x".repeat(1_048_577)),
            "output_oversized",
        ),
    ] {
        let mut changed = item.clone();
        changed[key] = value;
        cases.push((changed, reason));
    }
    for (key, value, reason) in [
        (
            "schema_version",
            json!("unrecognized"),
            "output_schema_mismatch",
        ),
        ("generation", json!(null), "generation_mismatch"),
        (
            "execution",
            json!({"model_submissions":1}),
            "execution_scope_mismatch",
        ),
        ("items", json!(null), "items_missing_or_invalid"),
        ("items", json!([]), "item_count_invalid"),
        (
            "items",
            json!([{"status":"ok","coverage":{"unit":"objects"}}]),
            "no_valid_native_retrieval",
        ),
    ] {
        let mut changed_response = response.clone();
        changed_response[key] = value;
        let mut changed = item.clone();
        changed["aggregated_output"] = json!(changed_response.to_string());
        cases.push((changed, reason));
    }
    for (changed, reason) in cases {
        assert!(tool_projection(&changed, &context).is_none());
        let projected = tool_diagnostic_event(Some(context.clone()), "item.completed", changed);
        assert!(projected["tool_use"].is_null());
        assert_eq!(
            projected["item_observation"]["command"]["rejection_reason"],
            reason
        );
        let safe = projected.to_string();
        for secret in [
            "SECRET_COMMAND",
            "SECRET_MALFORMED",
            "private-item",
            "/app/request.json",
        ] {
            assert!(!safe.contains(secret));
        }
        if reason == "output_oversized" {
            assert_eq!(
                projected["item_observation"]["command"]["output_bytes"],
                1_048_577
            );
            assert!(projected["item_observation"]["command"]["output_sha256"].is_null());
        }
    }
    let baseline = tool_diagnostic_event(None, "item.completed", item);
    assert!(baseline["tool_use"].is_null());
    assert_eq!(
        baseline["item_observation"]["command"]["rejection_reason"],
        "context_unavailable"
    );
}

#[test]
fn tool_diagnostics_preserve_exact_shell_recognizer_and_lifecycle_gate() {
    let (context, item, _) = tool_diagnostic_fixture();
    let command = item["command"].as_str().unwrap();
    for (changed_command, classification, accepted) in [
        (command.to_owned(), "pinned_batch_direct", true),
        (
            format!("/bin/bash -lc '{command}'"),
            "pinned_batch_shell",
            true,
        ),
        (
            format!("/bin/bash -x -lc '{command}'"),
            "unsupported_wrapper",
            false,
        ),
        (
            format!("{command}; echo SECRET"),
            "unsupported_syntax",
            false,
        ),
        (format!("echo '{command}'"), "unrecognized_command", false),
        ("x".repeat(8193), "oversized_command", false),
    ] {
        let mut changed = item.clone();
        changed["command"] = json!(changed_command);
        let projected =
            tool_diagnostic_event(Some(context.clone()), "item.completed", changed.clone());
        assert_eq!(
            projected["item_observation"]["command"]["classification"],
            classification
        );
        assert_eq!(!projected["tool_use"].is_null(), accepted);
        if accepted {
            assert_eq!(
                projected["tool_use"],
                tool_projection(&changed, &context).unwrap()
            );
        }
        for (event, lifecycle) in [("item.started", "started"), ("item.updated", "updated")] {
            let projected = tool_diagnostic_event(Some(context.clone()), event, changed.clone());
            assert!(projected["tool_use"].is_null());
            assert_eq!(projected["item_observation"]["lifecycle"], lifecycle);
            assert_eq!(
                projected["item_observation"]["command"]["rejection_reason"],
                "not_completed_event"
            );
        }
    }
}

#[test]
fn tool_diagnostics_close_unknown_fields_and_preserve_absence_and_signed_exit() {
    let (context, item, _) = tool_diagnostic_fixture();
    for (exit, expected) in [
        (json!(-9), json!(-9)),
        (json!(false), Value::Null),
        (json!(i64::MAX), Value::Null),
        (Value::Null, Value::Null),
    ] {
        let mut changed = item.clone();
        changed["exit_code"] = exit;
        changed["aggregated_output"] = json!("SECRET_UTF8_字");
        let projected = tool_diagnostic_event(Some(context.clone()), "item.completed", changed);
        let observation = &projected["item_observation"]["command"];
        assert_eq!(observation["exit_code"], expected);
        assert_eq!(observation["output_bytes"], "SECRET_UTF8_字".len());
        assert_eq!(
            observation["output_sha256"],
            sha("SECRET_UTF8_字".as_bytes())
        );
        assert!(!projected.to_string().contains("SECRET_UTF8"));
    }
    for kind in [
        "agent_message",
        "reasoning",
        "file_change",
        "mcp_tool_call",
        "web_search",
        "todo_list",
        "error",
        "SECRET_UNKNOWN_KIND",
    ] {
        let projected = tool_diagnostic_event(
            Some(context.clone()),
            "item.completed",
            json!({"id":"private-item", "type":kind,"status":"SECRET_UNKNOWN_STATUS","text":"SECRET_TEXT","command":"SECRET_COMMAND"}),
        );
        assert_eq!(
            projected["item_observation"]["item_kind"],
            if kind == "SECRET_UNKNOWN_KIND" {
                "unknown"
            } else {
                kind
            }
        );
        assert_eq!(projected["item_observation"]["status"], "unknown");
        assert!(projected["item_observation"]["command"].is_null());
        assert!(projected["tool_use"].is_null());
        assert!(!projected.to_string().contains("SECRET"));
    }
}
