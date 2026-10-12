//! Self-authored mock child fixtures only. No native Codex/model/provider/RunClock.
use super::super::tests::Fixture;
use super::*;
use serde_json::json;

const MOCK: &str = r#"
import json, os, sys, time
mode = sys.argv[1]
assert set(os.environ) <= {'PATH','HOME','CODEX_HOME','TMPDIR','LANG','LC_ALL','LC_CTYPE','__CF_USER_TEXT_ENCODING','CODEX_REFRESH_TOKEN_URL_OVERRIDE','CODEX_REVOKE_TOKEN_URL_OVERRIDE','NO_PROXY','no_proxy'}
assert os.environ['CODEX_REFRESH_TOKEN_URL_OVERRIDE'] == 'http://127.0.0.1:0/oauth/token'
assert os.environ['CODEX_REVOKE_TOKEN_URL_OVERRIDE'] == 'http://127.0.0.1:0/oauth/revoke'
assert os.environ['NO_PROXY'] == os.environ['no_proxy'] == '127.0.0.1,localhost,::1'
prompt = sys.stdin.buffer.read()
assert prompt
if mode == 'hang':
    time.sleep(10)
if mode == 'overflow':
    sys.stdout.write('x' * 8192)
    sys.stdout.flush()
    time.sleep(10)
verdict = {'equivalent':False,'abstained':False,'reason':''}
if mode == 'malformed':
    verdict['equivalent'] = 'false'
if mode in ('reason', 'receipt-overflow'):
    verdict['reason'] = 'x' * (16385 if mode == 'reason' else 16384)
raw = json.dumps(verdict,ensure_ascii=False,separators=(',',':'))
with open('native-final.json','wb') as f:
    f.write((raw if mode != 'wrong-file' else '{}').encode())
if mode == 'source-change':
    with open(sys.argv[2],'wb') as f: f.write(b'changed')
if mode == 'secret-stderr':
    sys.stderr.write('private_mock_diagnostic_DO_NOT_RETAIN')
    sys.stderr.flush()
events = [
    {'type':'thread.started','thread_id':'mock-thread'},
    {'type':'turn.started'},
    {'type':'item.completed','item':{'id':'mock-final','type':('command_execution' if mode == 'tool' else 'agent_message'),'text':raw}},
    {'type':'turn.completed','usage':{'input_tokens':0,'cached_input_tokens':0,'cache_write_input_tokens':0,'output_tokens':0,'reasoning_output_tokens':0}}
]
if mode == 'missing-terminal': events.pop()
if mode == 'repeat-turn': events.append({'type':'turn.started'})
for event in events:
    sys.stdout.write(json.dumps(event,ensure_ascii=False,separators=(',',':')) + '\n')
sys.stdout.flush()
"#;

fn v2_fixture() -> Result<MockFixture> {
    use std::os::unix::fs::PermissionsExt;
    let mut f = MockFixture::new()?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    // OS HOME is a namespace, not a ban on all input/output beneath it.
    c.environment.home = f.fixture.root.clone();
    c.limits.final_bytes = 8192;
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    c.runtime.recipe_sha256 = app_server::recipe(&Admission::load(&f.control, &digest(&bytes))?)?;
    let clock: gptgrep_jev::RunClockAnchor = serde_json::from_value(json!({
        "schema":"gptgrep.run-clock-anchor.v1","deadline_unix_ms":9999999999999u64,
        "clock_kind":"synthetic-test-only","boot_identity":"invented-test-boot",
        "clock_namespace":null,"monotonic_started_ns":1,"monotonic_deadline_ns":2
    }))?;
    let mut ids = Vec::new();
    for dir in [
        &c.environment.codex_home,
        &c.environment.sqlite_home,
        &c.environment.log_dir,
    ] {
        fs::create_dir(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        ids.push(identity(&open_held(dir, true)?)?);
    }
    let origin = f.fixture.root.join("origin.json");
    let value = PhaseOrigin {
        schema_version: "gptgrep.judge.native-origin.v1".into(),
        run_id: "test-fresh-run".into(),
        original_clock_sha256: digest(&serde_json::to_vec(&clock)?),
        deadline_unix_ms: clock.deadline_unix_ms(),
        original_clock: clock,
        codex_identity: ids[0],
        sqlite_identity: ids[1],
        log_identity: ids[2],
    };
    let bytes = json_bytes(&value, CONTROL_MAX)?;
    fs::write(&origin, &bytes)?;
    fs::set_permissions(&origin, fs::Permissions::from_mode(0o600))?;
    c.schema_version = "gptgrep.judge.native-control.v2".into();
    let prepared = PreparedRequest::load(
        &c.request.root,
        &c.request.manifest_sha256,
        f.fixture.python()?,
    )?;
    let m: Manifest = strict(&prepared.manifest.bytes)?;
    let phase = NativePhase {
        schema_version: "gptgrep.judge.native-phase.v1".into(),
        role: "verifier_only".into(),
        run_id: "test-fresh-run".into(),
        invocation_id: c.invocation_id.clone(),
        trial_id: "test-trial".into(),
        task_id: "test-task".into(),
        arm: "qualification".into(),
        control_projection_sha256: control_projection(&c)?,
        request_manifest_sha256: prepared.manifest.sha256.clone(),
        request_sha256: prepared.saved.sha256.clone(),
        judge_source_sha256: prepared.source.sha256.clone(),
        reference_sha256: prepared.reference.sha256.clone(),
        answer_sha256: prepared.answer.sha256.clone(),
        schema_sha256: prepared.decoded.formatted.verdict_schema_sha256.clone(),
        runtime_sha256: c.runtime.binary_sha256.clone(),
        runtime_schema_sha256: "8".repeat(64),
        formatter_sha256: digest(FORMATTER.as_bytes()),
        original_revision: m.original_revision,
        recipe_sha256: c.runtime.recipe_sha256.clone(),
        origin: PhaseFile {
            path: origin,
            sha256: digest(&bytes),
        },
        child_timeout_ms: 120000,
        child_count: 1,
        turn_count: 1,
        retry_count: 0,
        fallback_count: 0,
    };
    let phase_path = f.fixture.root.join("phase.json");
    let bytes = json_bytes(&phase, CONTROL_MAX)?;
    fs::write(&phase_path, &bytes)?;
    fs::set_permissions(&phase_path, fs::Permissions::from_mode(0o600))?;
    c.phase.origin_receipt_sha256 = phase.origin.sha256;
    c.phase.execution_policy_sha256 = digest(&bytes);
    c.phase.binding = Some(PhaseFile {
        path: phase_path,
        sha256: digest(&bytes),
    });
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    fs::set_permissions(&f.control, fs::Permissions::from_mode(0o600))?;
    f.control_sha = digest(&bytes);
    Ok(f)
}

#[test]
fn explicit_v2_phase_preserves_original_anchor_without_clock_capture_or_exec_permission()
-> Result<()> {
    let f = v2_fixture()?;
    let a = f.admit()?;
    let preflight: Value =
        strict(dispatch_app_server(&f.control, &f.control_sha, false)?.as_bytes())?;
    assert_eq!(preflight["recipe_matches"], true);
    assert_eq!(preflight["effect_count"], 0);
    let phase = a.phase.as_ref().unwrap();
    let permit = app_permit(&a, |clock| {
        assert!(clock == &phase.clock.original_clock);
        Ok(Duration::from_secs(5))
    })?;
    assert!(permit.deadline <= Instant::now() + Duration::from_secs(5));
    assert!(permit.original_clock.as_ref() == Some(&phase.clock.original_clock));
    assert!(permit.argv.is_empty());
    assert!(authorize(&a).is_err()); // Legacy standard exec stays closed even for v2.
    assert!(!f.claim_exists() && !f.output.exists());
    Ok(())
}

#[test]
fn v2_wrong_binding_role_count_or_tampered_inputs_refuse_before_claim() -> Result<()> {
    let mut f = v2_fixture()?;
    let original: Control = strict(&fs::read(&f.control)?)?;
    let phase_path = &original.phase.binding.as_ref().unwrap().path;
    let phase: Value = strict(&fs::read(phase_path)?)?;
    for (key, value) in [
        ("role", json!("reader")),
        ("run_id", json!("")),
        ("invocation_id", json!("other")),
        ("request_manifest_sha256", json!("0".repeat(64))),
        ("runtime_sha256", json!("0".repeat(64))),
        ("control_projection_sha256", json!("0".repeat(64))),
        ("answer_sha256", json!("0".repeat(64))),
        ("reference_sha256", json!("0".repeat(64))),
        ("judge_source_sha256", json!("0".repeat(64))),
        ("schema_sha256", json!("0".repeat(64))),
        ("formatter_sha256", json!("0".repeat(64))),
        ("original_revision", json!("0".repeat(40))),
        ("child_count", json!(2)),
        ("turn_count", json!(2)),
        ("retry_count", json!(1)),
        ("fallback_count", json!(1)),
        ("child_timeout_ms", json!(120001)),
    ] {
        let mut bad = phase.clone();
        bad[key] = value;
        let bytes = json_bytes(&bad, CONTROL_MAX)?;
        fs::write(phase_path, &bytes)?;
        let mut c: Control = strict(&fs::read(&f.control)?)?;
        c.phase.execution_policy_sha256 = digest(&bytes);
        c.phase.binding.as_mut().unwrap().sha256 = digest(&bytes);
        let bytes = json_bytes(&c, CONTROL_MAX)?;
        fs::write(&f.control, &bytes)?;
        f.control_sha = digest(&bytes);
        assert!(f.admit().is_err(), "{key}");
        assert!(!f.claim_exists() && !f.output.exists());
    }
    Ok(())
}

#[test]
fn v2_expired_or_changed_clock_and_home_namespace_fail_without_effects() -> Result<()> {
    let f = v2_fixture()?;
    let a = f.admit()?;
    assert!(app_permit(&a, |_| bail!("synthetic_expired_original_clock")).is_err());
    assert!(app_permit(&a, |_| bail!("synthetic_different_boot_or_namespace")).is_err());
    assert!(app_permit(&a, |_| Ok(Duration::ZERO)).is_err());
    fs::rename(
        &a.control.environment.codex_home,
        f.fixture.root.join("old-codex"),
    )?;
    fs::create_dir(&a.control.environment.codex_home)?;
    assert!(app_permit(&a, |_| Ok(Duration::from_secs(5))).is_err());
    assert!(!f.claim_exists() && !f.output.exists());
    Ok(())
}

#[test]
fn os_home_namespace_allows_regular_inputs_and_output_but_selected_state_is_excluded() -> Result<()>
{
    let f = MockFixture::new()?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    c.environment.home = f.fixture.root.clone();
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    let a = Admission::load(&f.control, &digest(&bytes))?;
    a.fresh_output()?;
    c.output_root = c.environment.codex_home.join("output");
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    assert!(Admission::load(&f.control, &digest(&bytes)).is_err());
    assert!(!f.claim_exists() && !f.output.exists());
    Ok(())
}
struct MockFixture {
    fixture: Fixture,
    control: PathBuf,
    control_sha: String,
    claim_root: PathBuf,
    output: PathBuf,
}
impl MockFixture {
    fn new() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new()?;
        let manifest_sha = fixture.prepare()?;
        let python = fixture.python()?;
        let claim_root = fixture.root.join("mock-claims");
        fs::create_dir(&claim_root)?;
        fs::set_permissions(&claim_root, fs::Permissions::from_mode(0o700))?;
        let output = fixture.root.join("native-output");
        let control = fixture.root.join("control.json");
        let value = Control {
            schema_version: "gptgrep.judge.native-control.v1".into(),
            role: "verifier_only".into(),
            invocation_id: "mock-intent".into(),
            request: PreparedControl {
                root: fixture.output.clone(),
                manifest_sha256: manifest_sha,
                python: python.path.clone(),
                python_sha256: python.sha256.clone(),
            },
            runtime: RuntimeControl {
                binary: python.path.clone(),
                binary_sha256: python.sha256.clone(),
                binary_bytes: python.bytes.len() as u64,
                source_revision: "1".repeat(40),
                recipe_sha256: "2".repeat(64),
            },
            environment: Environment {
                path: "/usr/bin:/bin".into(),
                home: fixture.root.join("unread-home"),
                codex_home: fixture.root.join("unread-codex-home"),
                tmpdir: fixture.root.clone(),
                sqlite_home: fixture.root.join("unread-sqlite"),
                log_dir: fixture.root.join("unread-log"),
            },
            phase: PhaseReferences {
                origin_receipt_sha256: "3".repeat(64),
                execution_policy_sha256: "4".repeat(64),
                binding: None,
            },
            invocation_root: claim_root.clone(),
            output_root: output.clone(),
            limits: Limits {
                stdout_bytes: STDOUT_CAP,
                stderr_bytes: STDERR_CAP,
                event_bytes: EVENT_CAP,
                event_count: EVENT_COUNT_CAP,
                final_bytes: FINAL_CAP,
                receipt_bytes: RECEIPT_CAP,
            },
        };
        let bytes = json_bytes(&value, CONTROL_MAX)?;
        fs::write(&control, &bytes)?;
        Ok(Self {
            fixture,
            control,
            control_sha: digest(&bytes),
            claim_root,
            output,
        })
    }
    fn admit(&self) -> Result<Admission> {
        Admission::load(&self.control, &self.control_sha)
    }
    fn permit(admitted: &Admission, mode: &str, wall: Duration) -> ExecutionPermit {
        // Only cfg(test) code constructs this opaque permit; synthetic monotonic test timer,
        // not a real original phase clock, ledger or backend qualification.
        ExecutionPermit {
            control_sha256: admitted.control_snapshot.sha256.clone(),
            deadline: Instant::now() + wall,
            observation_origin: "mock_child_fixture",
            original_clock: None,
            argv: vec![
                "-I".into(),
                "-S".into(),
                "-c".into(),
                MOCK.into(),
                mode.into(),
                admitted.prepared.answer.path.to_str().unwrap().into(),
            ],
        }
    }
    fn execute(&self, mode: &str) -> Result<Outcome> {
        let a = self.admit()?;
        execute(&a, Self::permit(&a, mode, Duration::from_secs(3)))
    }
    fn claim_exists(&self) -> bool {
        self.claim_root.join("mock-intent.claim.json").exists()
    }
}

#[test]
fn preflight_and_run_refusal_have_no_claim_native_home_or_output_effects() -> Result<()> {
    let f = MockFixture::new()?;
    let value: Value = strict(dispatch(&f.control, &f.control_sha, false)?.as_bytes())?;
    assert_eq!(value["status"], "preflight_no_effects");
    assert_eq!(value["execution_policy"], "unavailable");
    assert_eq!(value["effect_count"], 0);
    assert!(dispatch(&f.control, &f.control_sha, true).is_err());
    assert!(!f.claim_exists());
    assert!(!f.output.exists());
    for name in [
        "unread-home",
        "unread-codex-home",
        "unread-sqlite",
        "unread-log",
    ] {
        assert!(!f.fixture.root.join(name).exists());
    }
    Ok(())
}
#[test]
fn caller_qualified_boolean_and_oversized_limits_cannot_grant_permit() -> Result<()> {
    let f = MockFixture::new()?;
    let original: Value = strict(&fs::read(&f.control)?)?;
    let mut value = original;
    value["qualified"] = json!(true);
    let b = serde_json::to_vec(&value)?;
    fs::write(&f.control, &b)?;
    assert!(Admission::load(&f.control, &digest(&b)).is_err());
    let mut value: Value = strict(&fs::read(&f.control)?)?;
    value.as_object_mut().unwrap().remove("qualified");
    value["limits"]["stdout_bytes"] = json!(STDOUT_CAP + 1);
    let b = serde_json::to_vec(&value)?;
    fs::write(&f.control, &b)?;
    assert!(Admission::load(&f.control, &digest(&b)).is_err());
    assert!(!f.claim_exists());
    assert!(!f.output.exists());
    Ok(())
}
#[test]
fn valid_mock_terminal_and_final_bytes_remain_structural_unqualified() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = MockFixture::new()?;
    let result = f.execute("good")?;
    assert_eq!(
        result.report.status,
        "typed_structural_unqualified",
        "{}",
        serde_json::to_string(&result)?
    );
    assert!(result.publication_sha256.is_some());
    assert!(f.claim_exists());
    assert_eq!(
        result.report.observations.observation_origin,
        "mock_child_fixture"
    );
    assert!(result.report.observations.native_turn_id.is_none());
    assert!(result.report.observations.actual_judge_model.is_none());
    assert_eq!(result.report.observations.actual_usage_presence, "unknown");
    assert!(result.report.observations.actual_usage.is_none());
    assert!(result.report.observations.actual_cost.is_none());
    assert!(result.report.observations.actual_provider_calls.is_none());
    assert_eq!(
        result
            .report
            .observations
            .cli_projected_usage
            .as_ref()
            .unwrap()
            .input_tokens,
        0
    );
    assert!(!result.report.reward_emitted);
    assert!(!f.output.join("reward.txt").exists());
    assert!(!f.output.join("reward.json").exists());
    assert_eq!(fs::metadata(&f.output)?.permissions().mode() & 0o777, 0o700);
    for p in fs::read_dir(&f.output)? {
        assert_eq!(p?.metadata()?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}
#[test]
fn wrong_final_malformed_verdict_tool_and_incomplete_sequence_never_admit_verdict() -> Result<()> {
    for mode in [
        "wrong-file",
        "malformed",
        "tool",
        "missing-terminal",
        "reason",
    ] {
        let f = MockFixture::new()?;
        let result = f.execute(mode)?;
        assert_eq!(result.report.status, "completion_unknown_quarantined");
        assert!(result.report.verdict.is_none());
        assert!(!result.report.reward_emitted);
        assert!(f.claim_exists());
        assert!(result.no_retry);
    }
    Ok(())
}
#[test]
fn failure_after_terminal_retains_projected_numbers_but_no_actual_zero_usage() -> Result<()> {
    let f = MockFixture::new()?;
    let result = f.execute("repeat-turn")?;
    assert_eq!(result.report.status, "completion_unknown_quarantined");
    assert!(result.report.verdict.is_none());
    assert_eq!(
        result
            .report
            .observations
            .cli_projected_usage
            .as_ref()
            .unwrap()
            .output_tokens,
        0
    );
    assert!(result.report.observations.actual_usage.is_none());
    assert_eq!(result.report.observations.actual_usage_presence, "unknown");
    Ok(())
}
#[test]
fn stderr_is_counted_hashed_and_never_retained_as_raw_diagnostics() -> Result<()> {
    let f = MockFixture::new()?;
    let result = f.execute("secret-stderr")?;
    let bytes = json_bytes(&result, RECEIPT_CAP)?;
    assert_eq!(
        result.report.status,
        "typed_structural_unqualified",
        "{}",
        serde_json::to_string(&result)?
    );
    assert!(result.report.observations.stderr_bytes_observed > 0);
    assert!(!String::from_utf8(bytes)?.contains("private_mock_diagnostic_DO_NOT_RETAIN"));
    for entry in fs::read_dir(&f.output)? {
        assert!(
            !String::from_utf8(fs::read(entry?.path())?)?
                .contains("private_mock_diagnostic_DO_NOT_RETAIN")
        );
    }
    Ok(())
}
#[test]
fn durable_claim_rejects_replay_even_with_another_output_namespace() -> Result<()> {
    let f = MockFixture::new()?;
    f.execute("good")?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    c.output_root = f.fixture.root.join("second-output");
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    let a = Admission::load(&f.control, &digest(&bytes))?;
    assert!(execute(&a, MockFixture::permit(&a, "good", Duration::from_secs(3))).is_err());
    assert!(!c.output_root.exists());
    assert!(f.claim_exists());
    Ok(())
}
#[test]
fn source_change_before_claim_has_no_effects_and_after_spawn_suppresses_publication() -> Result<()>
{
    let f = MockFixture::new()?;
    let a = f.admit()?;
    fs::write(f.fixture.output.join("answer.json"), b"changed")?;
    assert!(execute(&a, MockFixture::permit(&a, "good", Duration::from_secs(3))).is_err());
    assert!(!f.claim_exists());
    assert!(!f.output.exists());
    let f = MockFixture::new()?;
    let result = f.execute("source-change")?;
    assert_eq!(result.report.status, "completion_unknown_quarantined");
    assert!(result.report.verdict.is_none());
    assert!(result.publication_sha256.is_none());
    assert!(!f.output.exists());
    assert!(f.claim_exists());
    Ok(())
}
#[test]
fn output_collision_is_refused_before_consuming_invocation() -> Result<()> {
    let f = MockFixture::new()?;
    fs::create_dir(&f.output)?;
    fs::write(f.output.join("keep"), b"retain")?;
    assert!(f.admit().is_err());
    assert_eq!(fs::read(f.output.join("keep"))?, b"retain");
    assert!(!f.claim_exists());
    let f = MockFixture::new()?;
    let a = f.admit()?;
    fs::create_dir(&f.output)?;
    fs::write(f.output.join("keep"), b"retain")?;
    assert!(execute(&a, MockFixture::permit(&a, "good", Duration::from_secs(3))).is_err());
    assert_eq!(fs::read(f.output.join("keep"))?, b"retain");
    assert!(!f.claim_exists());
    Ok(())
}
#[test]
fn deadline_and_capture_overflow_preserve_claim_and_unknown_lifetime() -> Result<()> {
    for mode in ["hang", "overflow"] {
        let f = MockFixture::new()?;
        let a = if mode == "overflow" {
            let mut c: Control = strict(&fs::read(&f.control)?)?;
            c.limits.stdout_bytes = 32;
            let bytes = json_bytes(&c, CONTROL_MAX)?;
            fs::write(&f.control, &bytes)?;
            Admission::load(&f.control, &digest(&bytes))?
        } else {
            f.admit()?
        };
        let began = Instant::now();
        let result = execute(
            &a,
            MockFixture::permit(&a, mode, Duration::from_millis(500)),
        )?;
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(result.report.status, "completion_unknown_quarantined");
        assert_eq!(result.report.observations.remote_lifetime, "unknown");
        assert!(result.report.verdict.is_none());
        assert!(f.claim_exists());
    }
    Ok(())
}
#[test]
fn concurrent_mock_attempts_share_one_durable_claim() -> Result<()> {
    let f = MockFixture::new()?;
    let a = f.admit()?;
    let b = f.admit()?;
    let pa = MockFixture::permit(&a, "good", Duration::from_secs(3));
    let pb = MockFixture::permit(&b, "good", Duration::from_secs(3));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = barrier.clone();
    let first = thread::spawn(move || {
        barrier.wait();
        execute(&a, pa)
    });
    let second = thread::spawn(move || {
        other.wait();
        execute(&b, pb)
    });
    let results = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(f.claim_exists());
    Ok(())
}
#[test]
fn native_recipe_uses_supported_flags_and_never_tool_off_or_bypass() -> Result<()> {
    let f = MockFixture::new()?;
    let a = f.admit()?;
    let args = native_argv(
        &a,
        &f.fixture.root.join("schema.json"),
        &f.fixture.root.join("final.json"),
    )?;
    assert!(args.iter().any(|s| s == "--sandbox"));
    assert!(args.iter().any(|s| s == "read-only"));
    assert!(args.iter().any(|s| s == "model_reasoning_effort=\"max\""));
    assert!(args.iter().any(|s| s == "service_tier=\"fast\""));
    assert!(
        !args
            .iter()
            .any(|s| s.contains("bypass") || s == "--no-tools")
    );
    assert_eq!(args.last().unwrap(), "-");
    Ok(())
}

#[test]
fn failure_receipt_allowance_is_checked_before_effects_and_preserves_unknown_on_large_reason()
-> Result<()> {
    let f = MockFixture::new()?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    c.limits.receipt_bytes = 1;
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    assert!(Admission::load(&f.control, &digest(&bytes)).is_err());
    assert!(!f.claim_exists());
    assert!(!f.output.exists());
    let f = MockFixture::new()?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    c.limits.receipt_bytes = 4096;
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    let a = Admission::load(&f.control, &digest(&bytes))?;
    let result = execute(
        &a,
        MockFixture::permit(&a, "receipt-overflow", Duration::from_secs(3)),
    )?;
    assert_eq!(result.report.status, "completion_unknown_quarantined");
    assert!(result.report.verdict.is_none());
    assert!(result.report.observations.terminal_sequence_observed);
    assert!(result.report.observations.actual_usage.is_none());
    assert!(result.publication_sha256.is_some());
    assert!(f.claim_exists());
    assert!(!f.output.join("native-final.json").exists());
    assert!(fs::metadata(f.output.join("native-observation.json"))?.len() <= 4096);
    Ok(())
}

#[test]
fn consistently_rewritten_request_is_structural_only_and_reconstruction_refuses_before_claim()
-> Result<()> {
    let f = MockFixture::new()?;
    let path = f.fixture.output.join("request.json");
    let mut r: Value = strict(&fs::read(&path)?)?;
    r["formatted"]["prompt"] = json!("changed synthetic prompt");
    r["formatted"]["prompt_sha256"] = json!(digest(b"changed synthetic prompt"));
    let bytes = json_bytes(&r, REQUEST_MAX)?;
    fs::write(&path, &bytes)?;
    let path = f.fixture.output.join("request-manifest.json");
    let mut manifest: Manifest = strict(&fs::read(&path)?)?;
    manifest.request = Binding {
        sha256: digest(&bytes),
        bytes: bytes.len(),
    };
    let bytes = json_bytes(&manifest, ARTIFACT_MAX)?;
    fs::write(&path, &bytes)?;
    let mut c: Control = strict(&fs::read(&f.control)?)?;
    c.request.manifest_sha256 = digest(&bytes);
    let bytes = json_bytes(&c, CONTROL_MAX)?;
    fs::write(&f.control, &bytes)?;
    let a = Admission::load(&f.control, &digest(&bytes))?;
    assert!(execute(&a, MockFixture::permit(&a, "good", Duration::from_secs(3))).is_err());
    assert!(!f.claim_exists());
    assert!(!f.output.exists());
    Ok(())
}

#[test]
fn nested_native_home_state_overlap_is_rejected_before_open_and_tmpdir_remains_allowed()
-> Result<()> {
    let f = MockFixture::new()?;
    let original: Value = strict(&fs::read(&f.control)?)?;
    assert!(f.admit().is_ok()); // All selected inputs may share the explicitly admitted TMPDIR.
    for (state, input) in [
        ("codex_home", "python"),
        ("sqlite_home", "binary"),
        ("log_dir", "binary"),
    ] {
        let mut c = original.clone();
        let unread = f.fixture.root.join(format!("absent-{state}"));
        c["environment"][state] = json!(unread);
        let path = unread.join("absent-input");
        if input == "binary" {
            c["runtime"][input] = json!(path);
        } else {
            c["request"][input] = json!(path);
        }
        let bytes = json_bytes(&c, CONTROL_MAX)?;
        fs::write(&f.control, &bytes)?;
        let error = match Admission::load(&f.control, &digest(&bytes)) {
            Ok(_) => panic!("overlap must fail before nested input admission"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "native_input_native_state_overlap");
        assert!(!unread.exists());
        assert!(!f.claim_exists());
        assert!(!f.output.exists());
    }
    Ok(())
}

#[test]
fn late_source_control_and_claim_root_changes_clear_verdict_preserve_observations_without_publication()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for kind in ["source", "control", "claim-root"] {
        let f = MockFixture::new()?;
        let a = f.admit()?;
        let input = if kind == "source" {
            a.prepared.answer.path.clone()
        } else {
            a.control_snapshot.path.clone()
        };
        let root = f.claim_root.clone();
        let retained = f.fixture.root.join("retained-consumed-claims");
        let moved = retained.clone();
        LATE_CUSTODY_TEST_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move || {
                if kind == "claim-root" {
                    fs::rename(&root, &moved)?;
                    fs::create_dir(&root)?;
                    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
                } else {
                    fs::write(&input, b"changed only after receipt serialization")?;
                }
                Ok(())
            }));
        });
        let result = execute(&a, MockFixture::permit(&a, "good", Duration::from_secs(3)))?;
        assert_eq!(result.report.status, "completion_unknown_quarantined");
        assert_eq!(
            result.report.failure_code,
            Some("native_stale_custody_no_retry")
        );
        assert!(result.report.verdict.is_none());
        assert!(result.report.raw_final_sha256.is_some());
        assert!(result.report.observations.terminal_sequence_observed);
        assert!(result.report.observations.transport_thread_id.is_some());
        assert!(result.report.observations.transport_final_item_id.is_some());
        assert_eq!(
            result
                .report
                .observations
                .cli_projected_usage
                .as_ref()
                .unwrap()
                .input_tokens,
            0
        );
        assert!(result.report.observations.actual_usage.is_none());
        assert_eq!(result.report.observations.actual_usage_presence, "unknown");
        assert!(result.report.observations.native_turn_id.is_none());
        assert!(result.publication_sha256.is_none());
        assert_eq!(result.publication_status, "not_published");
        assert!(result.no_retry);
        assert!(!result.output_may_exist);
        assert!(!result.report.reward_emitted);
        assert!(!f.output.exists());
        let consumed = if kind == "claim-root" {
            retained
        } else {
            f.claim_root.clone()
        };
        assert!(consumed.join("mock-intent.claim.json").exists());
        assert!(LATE_CUSTODY_TEST_HOOK.with(|slot| slot.borrow().is_none()));
    }
    Ok(())
}
