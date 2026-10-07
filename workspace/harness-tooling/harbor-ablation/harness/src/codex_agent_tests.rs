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

const SUCCESS: &str = "prompt=sys.stdin.read()\nopen('received.json','w').write(json.dumps({'prompt':prompt,'argv':sys.argv[1:],'env_names':sorted(os.environ)}))\nprint(json.dumps({'type':'thread.started','thread_id':'MOCK-THREAD'}))\nprint(json.dumps({'type':'turn.started'}))\nfor k in ['item.started','item.updated','item.completed']:\n print(json.dumps({'type':k,'item':{'id':'MOCK-ITEM','type':'command_execution','command':'PRIVATE TOOL CONTENT'}}))\nprint(json.dumps({'type':'turn.completed','usage':{'input_tokens':11,'cached_input_tokens':2,'output_tokens':3}}))\nopen('answer.json','w').write(json.dumps({'answer':'Synthetic answer'}))\n";

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
