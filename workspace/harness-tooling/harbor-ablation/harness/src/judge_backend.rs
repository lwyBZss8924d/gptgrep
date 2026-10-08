//! Native verifier source foundation. Production execution authority is unavailable.
//! The private runner is exercised only with test-only mock permits. No rewards.
use super::*;
use std::os::fd::AsRawFd;

#[path = "judge_app_server.rs"]
mod app_server;

const CONTROL_MAX: usize = 65_536;
const BINARY_MAX: u64 = 1_073_741_824;
const STDOUT_CAP: usize = 1_048_576;
const STDERR_CAP: usize = 65_536;
const EVENT_CAP: usize = 65_536;
const EVENT_COUNT_CAP: usize = 4096;
const FINAL_CAP: usize = 65_536;
const RECEIPT_CAP: usize = 65_536;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema_version: String,
    role: String,
    invocation_id: String,
    request: PreparedControl,
    runtime: RuntimeControl,
    environment: Environment,
    phase: PhaseReferences,
    invocation_root: PathBuf,
    output_root: PathBuf,
    limits: Limits,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedControl {
    root: PathBuf,
    manifest_sha256: String,
    python: PathBuf,
    python_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeControl {
    binary: PathBuf,
    binary_sha256: String,
    binary_bytes: u64,
    source_revision: String,
    recipe_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Environment {
    path: String,
    home: PathBuf,
    codex_home: PathBuf,
    tmpdir: PathBuf,
    sqlite_home: PathBuf,
    log_dir: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseReferences {
    origin_receipt_sha256: String,
    execution_policy_sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    stdout_bytes: usize,
    stderr_bytes: usize,
    event_bytes: usize,
    event_count: usize,
    final_bytes: usize,
    receipt_bytes: usize,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        for (n, cap) in [
            (self.stdout_bytes, STDOUT_CAP),
            (self.stderr_bytes, STDERR_CAP),
            (self.event_bytes, EVENT_CAP),
            (self.event_count, EVENT_COUNT_CAP),
            (self.final_bytes, FINAL_CAP),
            (self.receipt_bytes, RECEIPT_CAP),
        ] {
            ensure!(n > 0 && n <= cap, "native_limit_contract");
        }
        Ok(())
    }
}
struct Executable {
    path: PathBuf,
    file: File,
    id: (u64, u64),
    bytes: u64,
    sha256: String,
}
impl Executable {
    fn load(control: &RuntimeControl) -> Result<Self> {
        ensure!(
            hex(&control.binary_sha256, 64)
                && control.binary_bytes > 0
                && control.binary_bytes <= BINARY_MAX,
            "native_binary_contract"
        );
        let path = absolute(&control.binary)?;
        let mut file = open_held(&path, false)?;
        let id = identity(&file)?;
        let bytes = file.metadata()?.len();
        ensure!(bytes == control.binary_bytes, "native_binary_bytes");
        let sha256 = stream_digest(&mut file, bytes)?;
        ensure!(sha256 == control.binary_sha256, "native_binary_digest");
        Ok(Self {
            path,
            file,
            id,
            bytes,
            sha256,
        })
    }
    fn fresh(&self) -> Result<()> {
        ensure!(
            identity(&open_held(&self.path, false)?)? == self.id,
            "native_binary_changed"
        );
        let mut held = self.file.try_clone()?;
        ensure!(
            held.metadata()?.len() == self.bytes
                && stream_digest(&mut held, self.bytes)? == self.sha256,
            "native_binary_changed"
        );
        Ok(())
    }
}
fn stream_digest(file: &mut File, expected: u64) -> Result<String> {
    ensure!(
        expected > 0 && expected <= BINARY_MAX,
        "native_binary_bytes"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut scratch = [0; 4096];
    let mut count = 0u64;
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).context("native_binary_bytes")?;
        ensure!(count <= expected, "native_binary_bytes");
        hash.update(&scratch[..n]);
    }
    ensure!(count == expected, "native_binary_bytes");
    Ok(format!("{:x}", hash.finalize()))
}
struct Admission {
    control_snapshot: Snapshot,
    control: Control,
    prepared: PreparedRequest,
    binary: Executable,
}
impl Admission {
    fn load(path: &Path, sha: &str) -> Result<Self> {
        let control_snapshot = Snapshot::load(path, sha, CONTROL_MAX)?;
        let control: Control = strict(&control_snapshot.bytes)?;
        ensure!(
            control.schema_version == "gptgrep.judge.native-control.v1"
                && control.role == "verifier_only"
                && intent(&control.invocation_id)
                && hex(&control.runtime.source_revision, 40)
                && hex(&control.runtime.recipe_sha256, 64)
                && hex(&control.phase.origin_receipt_sha256, 64)
                && hex(&control.phase.execution_policy_sha256, 64),
            "native_control_contract"
        );
        control.limits.validate()?;
        // These are portable path strings only. Do not inspect homes, claims or phase ledgers.
        for p in [
            &control.environment.home,
            &control.environment.codex_home,
            &control.environment.tmpdir,
            &control.environment.sqlite_home,
            &control.environment.log_dir,
            &control.invocation_root,
            &control.output_root,
        ] {
            ensure!(p.is_absolute(), "native_path_contract");
            absolute(p)?;
            ensure!(
                p.to_str()
                    .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control)),
                "native_path_contract"
            );
        }
        ensure!(
            !control.environment.path.is_empty()
                && control.environment.path.len() <= 16_384
                && !control.environment.path.chars().any(char::is_control)
                && control
                    .environment
                    .path
                    .split(':')
                    .all(|s| Path::new(s).is_absolute()),
            "native_environment_contract"
        );
        // The explicitly selected Control snapshot is necessarily decoded first.
        // Reject nested input/native-state overlap lexically before opening any nested input.
        for input in [
            &control.request.root,
            &control.request.python,
            &control.runtime.binary,
        ] {
            let input = absolute(input)?;
            for native_state in [
                &control.environment.home,
                &control.environment.codex_home,
                &control.environment.sqlite_home,
                &control.environment.log_dir,
            ] {
                let native_state = absolute(native_state)?;
                ensure!(
                    !input.starts_with(&native_state) && !native_state.starts_with(&input),
                    "native_input_native_state_overlap"
                );
            }
        }
        let python = Snapshot::load(
            &control.request.python,
            &control.request.python_sha256,
            SOURCE_MAX,
        )?;
        let prepared = PreparedRequest::load(
            &control.request.root,
            &control.request.manifest_sha256,
            python,
        )?;
        let binary = Executable::load(&control.runtime)?;
        ensure!(
            binary.id != control_snapshot.id
                && prepared
                    .snapshots()
                    .iter()
                    .all(|s| s.id != control_snapshot.id),
            "input_alias"
        );
        let mut inputs = vec![
            prepared.root.as_path(),
            control_snapshot.path.as_path(),
            binary.path.as_path(),
            prepared.python.path.as_path(),
        ];
        inputs.extend([
            control.invocation_root.as_path(),
            control.environment.home.as_path(),
            control.environment.codex_home.as_path(),
            control.environment.sqlite_home.as_path(),
            control.environment.log_dir.as_path(),
        ]);
        fresh_output(&control.output_root, &inputs)?;
        let invocation = absolute(&control.invocation_root)?;
        ensure!(
            inputs[..4]
                .iter()
                .all(|p| !p.starts_with(&invocation) && !invocation.starts_with(p)),
            "native_claim_input_overlap"
        );
        let value = Self {
            control_snapshot,
            control,
            prepared,
            binary,
        };
        value.fresh()?;
        receipt_admission(
            &value,
            &initial_report(&value, "native_standard_exec_source_unqualified"),
        )?;
        Ok(value)
    }
    fn fresh_output(&self) -> Result<()> {
        let c = &self.control;
        fresh_output(
            &c.output_root,
            &[
                &self.prepared.root,
                &self.control_snapshot.path,
                &self.binary.path,
                &self.prepared.python.path,
                &c.invocation_root,
                &c.environment.home,
                &c.environment.codex_home,
                &c.environment.sqlite_home,
                &c.environment.log_dir,
            ],
        )?;
        Ok(())
    }
    fn fresh(&self) -> Result<()> {
        self.control_snapshot.fresh()?;
        self.prepared.fresh()?;
        self.binary.fresh()
    }
}
fn intent(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub(super) fn dispatch_app_server(path: &Path, sha: &str, run: bool) -> Result<String> {
    app_server::dispatch(path, sha, run)
}
pub(super) fn dispatch(path: &Path, sha: &str, run: bool) -> Result<String> {
    let admitted = Admission::load(path, sha)?;
    if !run {
        return String::from_utf8(json_bytes(
            &serde_json::json!({
                "schema_version":"gptgrep.judge.native-preflight.v1",
                "status":"preflight_no_effects", "role":"verifier_only",
                "control_sha256":admitted.control_snapshot.sha256,
                "request_manifest_sha256":admitted.prepared.manifest.sha256,
                "request_sha256":admitted.prepared.saved.sha256,
                "execution_policy":"unavailable",
                "original_formatter_reconstruction":"not_replayed_by_native_preflight",
                "judge_status":"unavailable", "actual_judge_model":null,
                "actual_usage_presence":"unknown", "actual_usage":null,
                "actual_cost":null, "actual_provider_calls":null,
                "reward_emitted":false, "effect_count":0
            }),
            RECEIPT_CAP,
        )?)
        .map_err(Into::into);
    }
    // No caller boolean/hash/config/artifact grants a permit. Source/profile metadata
    // is not actual tool, original-clock, phase, home or response-identity qualification.
    let permit = authorize(&admitted)?;
    let outcome = execute(&admitted, permit)?;
    String::from_utf8(json_bytes(&outcome, admitted.control.limits.receipt_bytes)?)
        .map_err(Into::into)
}
struct ExecutionPermit {
    control_sha256: String,
    deadline: Instant,
    // Immutable direct argv set only by an authority constructor, never Control input.
    argv: Vec<String>,
    observation_origin: &'static str,
}
fn authorize(_: &Admission) -> Result<ExecutionPermit> {
    bail!("native_execution_policy_unavailable")
}
fn toml_path(path: &Path) -> Result<String> {
    let s = path.to_str().context("native_path_contract")?;
    ensure!(!s.chars().any(char::is_control), "native_path_contract");
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
fn native_argv(a: &Admission, schema: &Path, final_path: &Path) -> Result<Vec<String>> {
    let strings = [
        "-c".into(),
        "model_reasoning_effort=\"max\"".into(),
        "-c".into(),
        "service_tier=\"fast\"".into(),
        "-c".into(),
        "web_search=\"disabled\"".into(),
        "-c".into(),
        format!(
            "sqlite_home={}",
            toml_path(&a.control.environment.sqlite_home)?
        ),
        "-c".into(),
        format!("log_dir={}", toml_path(&a.control.environment.log_dir)?),
        "exec".into(),
        "--strict-config".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "--model".into(),
        "gpt-6-luna".into(),
        "--sandbox".into(),
        "read-only".into(),
        "--json".into(),
        "--output-schema".into(),
        schema.to_str().context("native_path_contract")?.into(),
        "--output-last-message".into(),
        final_path.to_str().context("native_path_contract")?.into(),
        "-".into(),
    ];
    Ok(strings.into())
}
fn private_file(parent: &File, name: &str) -> Result<File> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        !name.is_empty() && !name.contains('/') && name != "." && name != "..",
        "native_private_name"
    );
    let name = std::ffi::CString::new(name)?;
    // SAFETY: live held parent and terminated leaf; exclusive no-follow file creation.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "native_invocation_consumed");
    // SAFETY: successful openat returned one owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    ensure!(
        file.metadata()?.permissions().mode() & 0o777 == 0o600,
        "native_private_mode"
    );
    Ok(file)
}
fn consume(a: &Admission, permit: &ExecutionPermit) -> Result<File> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        permit.control_sha256 == a.control_snapshot.sha256 && Instant::now() < permit.deadline,
        "native_execution_policy_unavailable"
    );
    a.fresh()?;
    a.fresh_output()?;
    let root = open_held(&a.control.invocation_root, true)?;
    ensure!(
        root.metadata()?.permissions().mode() & 0o777 == 0o700,
        "native_claim_private_mode"
    );
    let claim_bytes = json_bytes(
        &serde_json::json!({
            "schema_version":"gptgrep.judge.native-claim.v1",
            "state":"consumed_no_spawn_yet", "control_sha256":a.control_snapshot.sha256,
            "request_manifest_sha256":a.prepared.manifest.sha256,
            "request_sha256":a.prepared.saved.sha256,
            "runtime_sha256":a.binary.sha256,
            "phase_origin_sha256":a.control.phase.origin_receipt_sha256,
            "execution_policy_sha256":a.control.phase.execution_policy_sha256,
            "actual_provider_calls":null, "actual_usage":null, "actual_cost":null
        }),
        RECEIPT_CAP,
    )?;
    // Synchronous held-source/binary freshness may use the remaining phase time.
    // Recheck the original permit deadline immediately before the durable effect.
    ensure!(
        Instant::now() < permit.deadline,
        "native_execution_policy_unavailable"
    );
    let mut claim = private_file(&root, &format!("{}.claim.json", a.control.invocation_id))?;
    claim.write_all(&claim_bytes)?;
    claim.sync_all()?;
    root.sync_all()?;
    // Never remove or refund a created claim, including fsync/spawn/limit/publication failures.
    Ok(root)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
    output_tokens: i64,
    reasoning_output_tokens: i64,
}
impl CliUsage {
    fn validate(&self) -> Result<()> {
        ensure!(
            [
                self.input_tokens,
                self.cached_input_tokens,
                self.output_tokens,
                self.reasoning_output_tokens
            ]
            .iter()
            .all(|n| *n >= 0)
                && self.cache_write_input_tokens.is_none_or(|n| n >= 0),
            "native_usage_shape"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Event {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "item.completed")]
    ItemCompleted { item: Message },
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CliUsage },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    text: String,
}
struct Events {
    state: u8,
    count: usize,
    thread_id: Option<String>,
    item_id: Option<String>,
    final_text: Option<String>,
    projected_usage: Option<CliUsage>,
}
impl Events {
    fn new() -> Self {
        Self {
            state: 0,
            count: 0,
            thread_id: None,
            item_id: None,
            final_text: None,
            projected_usage: None,
        }
    }
    fn line(&mut self, bytes: &[u8], limits: &Limits) -> Result<()> {
        self.count += 1;
        ensure!(self.count <= limits.event_count, "native_event_count_limit");
        let event: Event = strict(bytes)?;
        match (self.state, event) {
            (0, Event::ThreadStarted { thread_id }) => {
                ensure!(intent(&thread_id), "native_event_identity");
                self.thread_id = Some(thread_id);
                self.state = 1;
            }
            (1, Event::TurnStarted {}) => self.state = 2,
            (2, Event::ItemCompleted { item }) => {
                ensure!(
                    item.kind == "agent_message"
                        && intent(&item.id)
                        && !item.text.is_empty()
                        && item.text.len() <= limits.final_bytes,
                    "native_final_message_shape"
                );
                self.item_id = Some(item.id);
                self.final_text = Some(item.text);
                self.state = 3;
            }
            (3, Event::TurnCompleted { usage }) => {
                usage.validate()?;
                self.projected_usage = Some(usage);
                self.state = 4;
            }
            _ => bail!("native_terminal_sequence"),
        }
        Ok(())
    }
}
#[derive(Serialize)]
struct Observations {
    wrapper_spawned: bool,
    wrapper_leader_reaped: bool,
    pipe_eof_observed: bool,
    terminal_sequence_observed: bool,
    observation_origin: &'static str,
    transport_thread_id: Option<String>,
    native_turn_id: Option<String>,
    transport_final_item_id: Option<String>,
    actual_judge_model: Option<String>,
    actual_provider: Option<String>,
    actual_effort: Option<String>,
    actual_service_tier: Option<String>,
    actual_response_id: Option<String>,
    actual_provider_calls: Option<u64>,
    actual_usage_presence: &'static str,
    actual_usage: Option<Value>,
    actual_cost: Option<f64>,
    cli_projected_usage: Option<CliUsage>,
    stdout_bytes_observed: usize,
    stderr_bytes_observed: usize,
    stderr_sha256: String,
    remote_lifetime: &'static str,
}
#[derive(Serialize)]
struct Report {
    schema_version: &'static str,
    status: &'static str,
    judge_status: &'static str,
    qualification: &'static str,
    control_sha256: String,
    request_manifest_sha256: String,
    request_sha256: String,
    judge_source_sha256: String,
    reference_sha256: String,
    answer_sha256: String,
    verdict_schema_sha256: String,
    raw_final_sha256: Option<String>,
    verdict: Option<Verdict>,
    observations: Observations,
    failure_code: Option<&'static str>,
    reward_emitted: bool,
    requested: Requested,
}
#[derive(Serialize)]
struct Outcome {
    schema_version: &'static str,
    publication_sha256: Option<String>,
    publication_status: &'static str,
    output_may_exist: bool,
    no_retry: bool,
    report: Report,
}
fn nonblocking(fd: i32) -> Result<()> {
    // SAFETY: borrowed live pipe descriptor; fcntl changes only this owned pipe flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "native_pipe_nonblocking"
    );
    Ok(())
}
fn cleanup(child: &mut std::process::Child) -> bool {
    // SAFETY: child group was created by our pre_exec; signal its exact positive ID's group.
    let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let end = Instant::now() + Duration::from_millis(250);
    while Instant::now() < end {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}
fn initial_report(a: &Admission, origin: &'static str) -> Report {
    Report {
        schema_version: "gptgrep.judge.native-observation.v1",
        status: "failed_unavailable",
        judge_status: "unavailable",
        qualification: QUALIFICATION,
        control_sha256: a.control_snapshot.sha256.clone(),
        request_manifest_sha256: a.prepared.manifest.sha256.clone(),
        request_sha256: a.prepared.saved.sha256.clone(),
        judge_source_sha256: a.prepared.source.sha256.clone(),
        reference_sha256: a.prepared.reference.sha256.clone(),
        answer_sha256: a.prepared.answer.sha256.clone(),
        verdict_schema_sha256: a.prepared.decoded.formatted.verdict_schema_sha256.clone(),
        raw_final_sha256: None,
        verdict: None,
        failure_code: None,
        reward_emitted: false,
        requested: Requested {
            model: "gpt-6-luna".into(),
            reasoning_effort: "max".into(),
            service_tier: "fast".into(),
        },
        observations: Observations {
            wrapper_spawned: false,
            wrapper_leader_reaped: false,
            pipe_eof_observed: false,
            terminal_sequence_observed: false,
            observation_origin: origin,
            transport_thread_id: None,
            native_turn_id: None,
            transport_final_item_id: None,
            actual_judge_model: None,
            actual_provider: None,
            actual_effort: None,
            actual_service_tier: None,
            actual_response_id: None,
            actual_provider_calls: None,
            actual_usage_presence: "unknown",
            actual_usage: None,
            actual_cost: None,
            cli_projected_usage: None,
            stdout_bytes_observed: 0,
            stderr_bytes_observed: 0,
            stderr_sha256: digest(b""),
            remote_lifetime: "unknown",
        },
    }
}
fn receipt_admission(a: &Admission, report: &Report) -> Result<()> {
    // Reserve the exact serialized worst scalar-only failure receipt before a child/claim.
    // IDs are ASCII <=64, counts/usage are nonnegative numbers, no diagnostic text is retained.
    let mut probe = serde_json::to_value(report)?;
    probe["status"] = serde_json::json!("typed_sequence_unpublished_unavailable");
    probe["raw_final_sha256"] = serde_json::json!("f".repeat(64));
    probe["failure_code"] = serde_json::json!("native_publication_unknown_no_retry");
    for name in ["transport_thread_id", "transport_final_item_id"] {
        probe["observations"][name] = serde_json::json!("x".repeat(64));
    }
    probe["observations"]["stdout_bytes_observed"] = serde_json::json!(usize::MAX);
    probe["observations"]["stderr_bytes_observed"] = serde_json::json!(usize::MAX);
    probe["observations"]["cli_projected_usage"] = serde_json::json!({
        "input_tokens":i64::MAX,"cached_input_tokens":i64::MAX,"cache_write_input_tokens":i64::MAX,
        "output_tokens":i64::MAX,"reasoning_output_tokens":i64::MAX
    });
    outcome_allowance(&probe, a.control.limits.receipt_bytes)
        .context("native_receipt_allowance")?;
    Ok(())
}
fn outcome_allowance(report: &impl Serialize, cap: usize) -> Result<()> {
    // Reserve the outer public result envelope as well as the persisted report.
    let bounded = serde_json::json!({
        "schema_version":"gptgrep.judge.native-result.v1",
        "publication_sha256":"f".repeat(64),
        "publication_status":"unknown_after_rename_no_retry",
        "output_may_exist":false,"no_retry":false,"report":report
    });
    json_bytes(&bounded, cap)?;
    Ok(())
}
#[cfg(test)]
type LateCustodyTestHook = Box<dyn FnOnce() -> Result<()>>;
#[cfg(test)]
thread_local! {
    // One-shot test seam only; absent from the production binary and public control API.
    static LATE_CUSTODY_TEST_HOOK: std::cell::RefCell<Option<LateCustodyTestHook>> =
        const { std::cell::RefCell::new(None) };
}
fn execute(a: &Admission, permit: ExecutionPermit) -> Result<Outcome> {
    use std::os::unix::process::CommandExt;
    // Fixed trusted formatter parity is checked before consuming an invocation.
    // Production policy is closed before this pure child or any claim/spawn effects.
    ensure!(
        permit.control_sha256 == a.control_snapshot.sha256,
        "native_execution_policy_unavailable"
    );
    a.prepared.reconstruct(
        a.control.output_root.parent().context("output_parent")?,
        permit.deadline,
    )?;
    let claim_root = consume(a, &permit)?;
    let claim_root_id = identity(&claim_root)?;
    let mut report = initial_report(a, permit.observation_origin);
    let mut final_bytes = None;
    let result: Result<()> = (|| {
        let limits = &a.control.limits;
        // Named retained raw buffers: one event frame + one final file + one receipt.
        // This is not serde/allocator/native/total-RSS accounting or physical disk reservation.
        let _capture_bound = limits
            .event_bytes
            .checked_add(limits.final_bytes)
            .and_then(|n| n.checked_add(limits.receipt_bytes))
            .context("native_capture_bound")?;
        let parent = a.control.output_root.parent().context("output_parent")?;
        let staging = private_tempdir(parent, ".judge-native-")?;
        let held = open_held(staging.path(), true)?;
        let schema = staging.path().join("verdict.schema.json");
        let final_path = staging.path().join("native-final.json");
        let final_custody = private_file(&held, "native-final.json")?;
        let final_identity = identity(&final_custody)?;
        let schema_bytes = json_bytes(&a.prepared.decoded.formatted.verdict_schema, ARTIFACT_MAX)?;
        let mut file = private_file(&held, "verdict.schema.json")?;
        file.write_all(&schema_bytes)?;
        file.sync_all()?;
        held.sync_all()?;
        let fixed = native_argv(a, &schema, &final_path)?;
        let argv = if permit.argv.is_empty() {
            fixed
        } else {
            permit.argv.clone()
        };
        // Only the opaque authority sets argv. Production authorize() is closed;
        // test permits contain self-authored Python -I -S mock code, not caller commands.
        a.fresh()?;
        ensure!(Instant::now() < permit.deadline, "native_deadline");
        let mut command = Command::new(&a.binary.path);
        command
            .args(&argv)
            .env_clear()
            .env("PATH", &a.control.environment.path)
            .env("HOME", &a.control.environment.home)
            .env("CODEX_HOME", &a.control.environment.codex_home)
            .env("TMPDIR", &a.control.environment.tmpdir)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .current_dir(staging.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: async-signal-safe setpgid in the fork child; no allocation, locks or inherited code.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::umask(0o077);
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        report.observations.wrapper_spawned = true;
        let process_result: Result<()> = (|| {
            let mut input = Some(child.stdin.take().context("native_pipe")?);
            let mut stdout = child.stdout.take().context("native_pipe")?;
            let mut stderr = child.stderr.take().context("native_pipe")?;
            nonblocking(input.as_ref().context("native_pipe")?.as_raw_fd())?;
            nonblocking(stdout.as_raw_fd())?;
            nonblocking(stderr.as_raw_fd())?;
            let prompt = a.prepared.decoded.formatted.prompt.as_bytes();
            let mut sent = 0;
            let mut line = Vec::new();
            line.try_reserve_exact(limits.event_bytes)?;
            let mut scratch = [0; 4096];
            let mut stdout_eof = false;
            let mut stderr_eof = false;
            let mut events = Events::new();
            let mut stderr_hash = Sha256::new();
            let mut exit = None;
            let pumped: Result<()> = (|| {
                loop {
                    ensure!(Instant::now() < permit.deadline, "native_deadline");
                    if let Some(stdin) = input.as_mut() {
                        match stdin.write(&prompt[sent..prompt.len().min(sent + scratch.len())]) {
                            Ok(n) => sent += n,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => bail!("native_stdin_failure"),
                        }
                        if sent == prompt.len() {
                            input = None;
                        }
                    }
                    for (stream, eof, is_stdout) in [
                        (&mut stdout as &mut dyn Read, &mut stdout_eof, true),
                        (&mut stderr as &mut dyn Read, &mut stderr_eof, false),
                    ] {
                        if *eof {
                            continue;
                        }
                        match stream.read(&mut scratch) {
                            Ok(0) => *eof = true,
                            Ok(n) if is_stdout => {
                                report.observations.stdout_bytes_observed += n;
                                ensure!(
                                    report.observations.stdout_bytes_observed
                                        <= limits.stdout_bytes,
                                    "native_stdout_limit"
                                );
                                for b in &scratch[..n] {
                                    if *b == b'\n' {
                                        ensure!(!line.is_empty(), "native_event_shape");
                                        events.line(&line, limits)?;
                                        line.clear();
                                    } else {
                                        ensure!(
                                            line.len() < limits.event_bytes,
                                            "native_event_limit"
                                        );
                                        line.push(*b);
                                    }
                                }
                            }
                            Ok(n) => {
                                report.observations.stderr_bytes_observed += n;
                                stderr_hash.update(&scratch[..n]);
                                ensure!(
                                    report.observations.stderr_bytes_observed
                                        <= limits.stderr_bytes,
                                    "native_stderr_limit"
                                );
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => bail!("native_pipe_failure"),
                        }
                    }
                    if exit.is_none() {
                        exit = child.try_wait()?;
                    }
                    if stdout_eof && stderr_eof && exit.is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                ensure!(
                    sent == prompt.len() && line.is_empty() && events.state == 4,
                    "native_incomplete_terminal"
                );
                ensure!(exit.is_some_and(|s| s.success()), "native_exit_failure");
                Ok(())
            })();
            report.observations.stderr_sha256 = format!("{:x}", stderr_hash.finalize());
            report.observations.wrapper_leader_reaped = exit.is_some();
            report.observations.pipe_eof_observed = stdout_eof && stderr_eof;
            report.observations.terminal_sequence_observed = events.state == 4;
            report.observations.transport_thread_id = events.thread_id;
            report.observations.transport_final_item_id = events.item_id;
            report.observations.cli_projected_usage = events.projected_usage;
            pumped?;
            let mut held_final = open_held(&final_path, false)?;
            ensure!(
                identity(&held_final)? == final_identity,
                "native_final_identity"
            );
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    held_final.metadata()?.permissions().mode() & 0o777 == 0o600,
                    "native_final_private_mode"
                );
            }
            let bytes = read_bounded(&mut held_final, limits.final_bytes)?;
            ensure!(
                events
                    .final_text
                    .as_deref()
                    .is_some_and(|s| s.as_bytes() == bytes),
                "native_final_stream_binding"
            );
            let verdict: Verdict = strict(&bytes)?;
            ensure!(verdict.reason.len() <= STDERR_MAX, "native_reason_limit");
            report.raw_final_sha256 = Some(digest(&bytes));
            report.verdict = Some(verdict);
            final_bytes = Some(bytes);
            Ok(())
        })();
        if process_result.is_err() {
            report.observations.wrapper_leader_reaped |= cleanup(&mut child);
            // Leader cleanup never attests descendant or remote-provider termination.
        }
        process_result?;
        a.fresh()?;
        ensure!(
            identity(&open_held(&a.control.invocation_root, true)?)? == claim_root_id,
            "native_claim_root_changed"
        );
        ensure!(Instant::now() < permit.deadline, "native_deadline");
        Ok(())
    })();
    if result.is_ok() {
        report.status = "typed_structural_unqualified";
    } else {
        report.status = if report.observations.wrapper_spawned {
            "completion_unknown_quarantined"
        } else {
            "consumed_failed_no_spawn_observed"
        };
        report.failure_code = Some("native_unavailable_no_retry");
        report.verdict = None;
        final_bytes = None;
    }
    let receipt = match outcome_allowance(&report, a.control.limits.receipt_bytes)
        .and_then(|()| json_bytes(&report, a.control.limits.receipt_bytes))
    {
        Ok(bytes) => bytes,
        Err(_) => {
            // A large typed reason cannot discard charged/unknown observations or become a reward.
            report.verdict = None;
            final_bytes = None;
            report.status = "completion_unknown_quarantined";
            report.failure_code = Some("native_receipt_limit_no_retry");
            json_bytes(&report, a.control.limits.receipt_bytes)?
        }
    };
    let mut outcome = Outcome {
        schema_version: "gptgrep.judge.native-result.v1",
        publication_sha256: None,
        publication_status: "not_published",
        output_may_exist: false,
        no_retry: true,
        report,
    };
    #[cfg(test)]
    if let Some(hook) = LATE_CUSTODY_TEST_HOOK.with(|slot| slot.borrow_mut().take()) {
        // Exactly after the initial postprocess freshness and receipt serialization.
        hook()?;
    }
    // A source/claim mutation suppresses output; preserve returned sanitized observations and claim.
    if Instant::now() >= permit.deadline {
        outcome.report.status = "completion_unknown_quarantined";
        outcome.report.failure_code = Some("native_deadline_no_retry");
        outcome.report.verdict = None;
        return Ok(outcome);
    }
    if a.fresh().is_err()
        || !open_held(&a.control.invocation_root, true)
            .and_then(|f| identity(&f))
            .is_ok_and(|id| id == claim_root_id)
    {
        outcome.report.status = "completion_unknown_quarantined";
        outcome.report.failure_code = Some("native_stale_custody_no_retry");
        outcome.report.verdict = None;
        return Ok(outcome);
    }
    let mut snapshots = a.prepared.snapshots().to_vec();
    snapshots.push(&a.control_snapshot);
    let mut files = vec![("native-observation.json", receipt.as_slice())];
    if let Some(bytes) = final_bytes.as_ref() {
        files.push(("native-final.json", bytes.as_slice()));
    }
    match publish_checked(&a.control.output_root, &files, &snapshots, || {
        ensure!(Instant::now() < permit.deadline, "native_deadline");
        a.fresh()?;
        ensure!(
            identity(&open_held(&a.control.invocation_root, true)?)? == claim_root_id,
            "native_claim_root_changed"
        );
        ensure!(Instant::now() < permit.deadline, "native_deadline");
        Ok(())
    }) {
        Ok(sha) => {
            outcome.publication_sha256 = Some(sha);
            outcome.publication_status = "published";
        }
        Err(error) => {
            outcome.output_may_exist = error
                .chain()
                .any(|e| e.to_string() == "publication_sync_unknown_no_retry_output_may_exist");
            outcome.publication_status = if outcome.output_may_exist {
                "unknown_after_rename_no_retry"
            } else {
                "failed_before_rename_no_retry"
            };
            if outcome.report.verdict.is_some() {
                outcome.report.status = "typed_sequence_unpublished_unavailable";
                outcome.report.failure_code = Some("native_publication_unknown_no_retry");
            }
            outcome.report.verdict = None;
        }
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "judge_backend_tests.rs"]
mod tests;
