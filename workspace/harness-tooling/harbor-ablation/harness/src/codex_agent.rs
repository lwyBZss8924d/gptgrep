//! One fresh `codex exec` process. Sandbox, auth and capability adoption are external gates.
//! Check mode never launches the native binary. No installer, shell prompt or credential reader.
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, &'static str>;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control: PathBuf,
    #[arg(long)]
    control_sha256: String,
    /// Explicit one-shot launch; omission is admission-only.
    #[arg(long)]
    run: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Phase {
    id: String,
    origin_receipt_sha256: String,
    clock_namespace: String,
    issued_monotonic_ns: u64,
    deadline_monotonic_ns: u64,
    deadline_unix_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    model: String,
    reasoning_effort: String,
    service_tier: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    stdout_bytes: usize,
    stderr_bytes: usize,
    event_bytes: usize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityReferences {
    schema_version: u32,
    /// Operator references only; this launcher cannot certify their underlying evidence.
    generation_v2: String,
    regex: String,
    tree: String,
    visual: String,
    multimodal: String,
    decisions_helper: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema_version: u32,
    phase: Phase,
    profile: Profile,
    arm: String,
    runtime_kind: String,
    runtime: Asset,
    answer_schema: Asset,
    prompt: Asset,
    cwd: PathBuf,
    raw_dir: PathBuf,
    raw_documents: Vec<Asset>,
    output_dir: PathBuf,
    environment: BTreeMap<String, String>,
    /// Secret-free identity of the externally provisioned channel; no credential path/value.
    auth_channel_receipt_sha256: String,
    treatment_capabilities: Option<Asset>,
    limits: Limits,
}

fn sha(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn digest_shape(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn path_custody(path: &Path, missing_leaf: bool) -> Result<()> {
    if !path.is_absolute() {
        return Err("path_not_absolute");
    }
    let mut prefix = PathBuf::new();
    for part in path.components() {
        if !matches!(part, Component::RootDir | Component::Normal(_)) {
            return Err("path_component");
        }
        prefix.push(part);
        match fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => return Err("symlink_path"),
            Ok(_) => {}
            Err(error)
                if missing_leaf
                    && prefix == path
                    && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("path_missing"),
        }
    }
    Ok(())
}

fn snapshot(path: &Path, cap: usize, private: bool) -> Result<Vec<u8>> {
    path_custody(path, false)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "open_input")?;
    let meta = file.metadata().map_err(|_| "input_metadata")?;
    if !meta.is_file() || meta.len() > cap as u64 {
        return Err("input_size_or_kind");
    }
    if private && (meta.mode() & 0o777 != 0o600 || meta.uid() != unsafe { libc::geteuid() }) {
        return Err("input_private_mode");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input_read")?;
    if bytes.len() > cap || file.metadata().map_err(|_| "input_metadata")?.len() != meta.len() {
        return Err("input_changed_or_oversize");
    }
    Ok(bytes)
}

fn asset(asset: &Asset, cap: usize, private: bool) -> Result<Vec<u8>> {
    if !digest_shape(&asset.sha256) {
        return Err("digest_shape");
    }
    let bytes = snapshot(&asset.path, cap, private)?;
    if sha(&bytes) != asset.sha256 {
        return Err("asset_digest");
    }
    Ok(bytes)
}

fn executable(asset: &Asset) -> Result<()> {
    if !digest_shape(&asset.sha256) {
        return Err("digest_shape");
    }
    path_custody(&asset.path, false)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&asset.path)
        .map_err(|_| "runtime_open")?;
    let before = file.metadata().map_err(|_| "runtime_metadata")?;
    if !before.is_file()
        || before.len() == 0
        || before.len() > 1_073_741_824
        || before.mode() & 0o111 == 0
    {
        return Err("runtime_executable");
    }
    let mut total = 0u64;
    let mut digest = Sha256::new();
    loop {
        let mut bytes = [0u8; 8192];
        let count = file.read(&mut bytes).map_err(|_| "runtime_read")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > 1_073_741_824 {
            return Err("runtime_size");
        }
        digest.update(&bytes[..count]);
    }
    let after = file.metadata().map_err(|_| "runtime_metadata")?;
    if total != before.len()
        || after.len() != before.len()
        || after.mtime() != before.mtime()
        || after.mtime_nsec() != before.mtime_nsec()
        || format!("{:x}", digest.finalize()) != asset.sha256
    {
        return Err("runtime_changed_or_digest");
    }
    Ok(())
}

fn private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    path_custody(path, true)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "fresh_output_required")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "output_write")
}

fn monotonic_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0
        || time.tv_sec < 0
        || time.tv_nsec < 0
    {
        return Err("monotonic_clock");
    }
    (time.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(time.tv_nsec as u64))
        .ok_or("clock_overflow")
}

impl Phase {
    fn check(&self) -> Result<()> {
        let now = monotonic_ns()?;
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "wall_clock")?
            .as_millis();
        if now < self.issued_monotonic_ns
            || now >= self.deadline_monotonic_ns
            || wall >= self.deadline_unix_ms as u128
        {
            return Err("phase_deadline");
        }
        Ok(())
    }
}

fn prelaunch_guard(phase: &Phase) -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err("launcher_interrupted");
    }
    phase.check()?;
    // Check again after clock work. This is not an atomic OS cancellation guarantee.
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err("launcher_interrupted");
    }
    Ok(())
}

struct Admitted {
    control: Control,
    control_sha256: String,
    schema: Vec<u8>,
    prompt: Option<Vec<u8>>,
}

fn fresh_outputs(control: &Control, phase_reserved: bool) -> Result<()> {
    for name in [
        "native-run.claim",
        "receipt.json",
        "native-events.jsonl",
        "answer.schema.json",
        "final-message.json",
        ".receipt.pending",
    ] {
        if phase_reserved && matches!(name, "native-run.claim" | "answer.schema.json") {
            continue;
        }
        if fs::symlink_metadata(control.output_dir.join(name)).is_ok() {
            return Err("stale_output");
        }
    }
    if fs::symlink_metadata(control.cwd.join("answer.json")).is_ok() {
        return Err("stale_answer");
    }
    Ok(())
}

fn raw_inventory(
    root: &Path,
    result: &mut BTreeSet<PathBuf>,
    depth: usize,
    visited: &mut usize,
) -> Result<()> {
    if depth > 8 || result.len() > 256 {
        return Err("raw_inventory_limit");
    }
    for entry in fs::read_dir(root).map_err(|_| "raw_directory")? {
        *visited += 1;
        if *visited > 512 {
            return Err("raw_inventory_limit");
        }
        let entry = entry.map_err(|_| "raw_entry")?;
        let kind = entry.file_type().map_err(|_| "raw_kind")?;
        if kind.is_symlink() {
            return Err("raw_symlink");
        }
        if kind.is_dir() {
            raw_inventory(&entry.path(), result, depth + 1, visited)?;
        } else if kind.is_file() {
            result.insert(entry.path());
        } else {
            return Err("raw_kind");
        }
    }
    Ok(())
}

fn admit(path: &Path, expected: &str, run: bool) -> Result<Admitted> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err("launcher_interrupted");
    }
    if !digest_shape(expected) {
        return Err("digest_shape");
    }
    let bytes = snapshot(path, 1_048_576, true)?;
    if sha(&bytes) != expected {
        return Err("control_digest");
    }
    let control: Control = serde_json::from_slice(&bytes).map_err(|_| "control_schema")?;
    if control.schema_version != 1
        || control.profile.model != "gpt-6-luna"
        || control.profile.reasoning_effort != "max"
        || control.profile.service_tier != "fast"
    {
        return Err("profile_or_version");
    }
    if !matches!(control.runtime_kind.as_str(), "native" | "mock")
        || !matches!(control.arm.as_str(), "baseline" | "treatment")
    {
        return Err("role_or_runtime");
    }
    if control.phase.id.is_empty()
        || control.phase.id.len() > 64
        || !control
            .phase
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        || !digest_shape(&control.phase.origin_receipt_sha256)
        || control.phase.clock_namespace.is_empty()
        || control.phase.clock_namespace.len() > 128
        || !control
            .phase
            .clock_namespace
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':'))
        || control.phase.deadline_monotonic_ns <= control.phase.issued_monotonic_ns
        || control.phase.deadline_monotonic_ns - control.phase.issued_monotonic_ns
            > 86_400_000_000_000
    {
        return Err("phase_schema");
    }
    prelaunch_guard(&control.phase)?;
    if !digest_shape(&control.auth_channel_receipt_sha256) {
        return Err("channel_reference");
    }
    let allowed: BTreeSet<&str> = ["PATH", "HOME", "CODEX_HOME", "TMPDIR", "LANG", "LC_ALL"]
        .into_iter()
        .collect();
    if control
        .environment
        .keys()
        .any(|key| !allowed.contains(key.as_str()))
        || ["PATH", "HOME", "CODEX_HOME", "TMPDIR"]
            .iter()
            .any(|key| !control.environment.contains_key(*key))
        || control
            .environment
            .values()
            .any(|v| v.len() > 4096 || v.contains('\0'))
    {
        return Err("environment_allowlist");
    }
    for key in ["HOME", "CODEX_HOME", "TMPDIR"] {
        let directory = Path::new(&control.environment[key]);
        path_custody(directory, false)?;
        if !directory.is_dir() || !directory.starts_with(&control.output_dir) {
            return Err("environment_directory");
        }
    }
    if control.limits.stdout_bytes == 0
        || control.limits.stdout_bytes > 16_777_216
        || control.limits.stderr_bytes == 0
        || control.limits.stderr_bytes > 1_048_576
        || control.limits.event_bytes == 0
        || control.limits.event_bytes > 1_048_576
    {
        return Err("limits");
    }
    path_custody(&control.cwd, false)?;
    path_custody(&control.raw_dir, false)?;
    path_custody(&control.output_dir, false)?;
    let output_meta = fs::metadata(&control.output_dir).map_err(|_| "output_directory")?;
    if !output_meta.is_dir()
        || output_meta.mode() & 0o777 != 0o700
        || output_meta.uid() != unsafe { libc::geteuid() }
        || !control.raw_dir.starts_with(&control.cwd)
        || control.raw_documents.is_empty()
        || control.raw_documents.len() > 256
    {
        return Err("directory_or_raw_scope");
    }
    fresh_outputs(&control, false)?;
    let mut actual = BTreeSet::new();
    raw_inventory(&control.raw_dir, &mut actual, 0, &mut 0)?;
    let mut declared = BTreeSet::new();
    let mut total = 0usize;
    for document in &control.raw_documents {
        if !document.path.starts_with(&control.raw_dir)
            || document.path.extension().is_none_or(|v| v != "pdf")
            || !declared.insert(document.path.clone())
        {
            return Err("raw_scope_or_duplicate");
        }
        total = total
            .checked_add(asset(document, 67_108_864, false)?.len())
            .ok_or("raw_total")?;
        if total > 67_108_864 {
            return Err("raw_total");
        }
    }
    if actual != declared {
        return Err("raw_inventory_mismatch");
    }
    match (&control.treatment_capabilities, control.arm.as_str()) {
        (None, "baseline") => {}
        (Some(reference), "treatment") => {
            let value: CapabilityReferences =
                serde_json::from_slice(&asset(reference, 65_536, false)?)
                    .map_err(|_| "capability_schema")?;
            if value.schema_version != 1
                || [
                    &value.generation_v2,
                    &value.regex,
                    &value.tree,
                    &value.visual,
                    &value.multimodal,
                    &value.decisions_helper,
                ]
                .iter()
                .any(|v| !digest_shape(v))
            {
                return Err("capability_references");
            }
        }
        _ => return Err("arm_capabilities"),
    }
    executable(&control.runtime)?;
    let schema = asset(&control.answer_schema, 65_536, false)?;
    let schema_value: Value = serde_json::from_slice(&schema).map_err(|_| "answer_schema")?;
    if schema_value
        != json!({"type":"object","properties":{"answer":{"type":"string","maxLength":12000}},"required":["answer"],"additionalProperties":false})
    {
        return Err("answer_schema");
    }
    let prompt = if run {
        Some(asset(&control.prompt, 65_536, true)?)
    } else {
        path_custody(&control.prompt.path, true)?;
        if !digest_shape(&control.prompt.sha256)
            || fs::symlink_metadata(&control.prompt.path).is_ok()
        {
            return Err("prompt_not_fresh");
        }
        None
    };
    if prompt
        .as_ref()
        .is_some_and(|v| v.is_empty() || std::str::from_utf8(v).is_err())
    {
        return Err("prompt_utf8_or_empty");
    }
    prelaunch_guard(&control.phase)?;
    Ok(Admitted {
        control,
        control_sha256: expected.to_owned(),
        schema,
        prompt,
    })
}

fn argv(control: &Control) -> Vec<String> {
    let output = &control.output_dir;
    vec![
        "-c".into(),
        "model_reasoning_effort=\"max\"".into(),
        "-c".into(),
        "service_tier=\"fast\"".into(),
        "-c".into(),
        "web_search=\"disabled\"".into(),
        "exec".into(),
        "--strict-config".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--ephemeral".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--skip-git-repo-check".into(),
        "--model".into(),
        control.profile.model.clone(),
        "--cd".into(),
        control.cwd.to_string_lossy().into_owned(),
        "--json".into(),
        "--color".into(),
        "never".into(),
        "--output-schema".into(),
        output
            .join("answer.schema.json")
            .to_string_lossy()
            .into_owned(),
        "--output-last-message".into(),
        output
            .join("final-message.json")
            .to_string_lossy()
            .into_owned(),
        "-".into(),
    ]
}

#[derive(Clone, Default, Serialize)]
struct Observations {
    completed_turns: usize,
    unique_items: usize,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

#[derive(Default)]
struct Events {
    observations: Observations,
    item_ids: BTreeSet<String>,
    active_turn: bool,
    failed: bool,
    projected: Vec<u8>,
}

impl Events {
    fn line(&mut self, bytes: &[u8]) -> Result<()> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| "native_event_json")?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("native_event_type")?;
        let mut projection = json!({"type":kind});
        let mut terminal_commit = None;
        match kind {
            "thread.started" => {
                if value.get("thread_id").and_then(Value::as_str).is_none() {
                    return Err("thread_event");
                }
            }
            "turn.started" => {
                if self.active_turn {
                    return Err("turn_overlap");
                }
                self.active_turn = true;
            }
            "turn.completed" => {
                if !self.active_turn {
                    return Err("duplicate_or_unstarted_terminal");
                }
                let usage = value.get("usage");
                if usage.is_some_and(|v| !v.is_object() && !v.is_null()) {
                    return Err("native_usage_schema");
                }
                let mut candidate = self.observations.clone();
                let first = candidate.completed_turns == 0;
                let merge = |total: &mut Option<u64>, key: &str| -> Result<()> {
                    let field = usage.and_then(|v| v.get(key));
                    if field.is_some_and(|v| !v.is_null() && v.as_u64().is_none()) {
                        return Err("native_usage_number");
                    }
                    let current = field.and_then(Value::as_u64);
                    *total = if first {
                        current
                    } else {
                        match (*total, current) {
                            (Some(a), Some(b)) => Some(a.checked_add(b).ok_or("usage_overflow")?),
                            _ => None,
                        }
                    };
                    Ok(())
                };
                merge(&mut candidate.input_tokens, "input_tokens")?;
                merge(&mut candidate.cached_input_tokens, "cached_input_tokens")?;
                merge(
                    &mut candidate.cache_write_input_tokens,
                    "cache_write_input_tokens",
                )?;
                merge(&mut candidate.output_tokens, "output_tokens")?;
                merge(
                    &mut candidate.reasoning_output_tokens,
                    "reasoning_output_tokens",
                )?;
                candidate.completed_turns = candidate
                    .completed_turns
                    .checked_add(1)
                    .ok_or("usage_overflow")?;
                projection["usage"] = serde_json::to_value(&candidate).map_err(|_| "projection")?;
                terminal_commit = Some(candidate);
            }
            "item.started" | "item.updated" | "item.completed" => {
                let id = value
                    .get("item")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .ok_or("item_id")?;
                if id.len() > 1024 || self.item_ids.len() >= 4096 && !self.item_ids.contains(id) {
                    return Err("item_limit");
                }
                self.item_ids.insert(id.to_owned());
                self.observations.unique_items = self.item_ids.len();
                projection["item_id_sha256"] = Value::String(sha(id.as_bytes()));
            }
            "turn.failed" | "error" => {
                self.failed = true;
                self.active_turn = false;
            }
            _ => return Err("unsupported_native_event"),
        }
        let mut encoded = serde_json::to_vec(&projection).map_err(|_| "projection")?;
        encoded.push(b'\n');
        if self.projected.len() + encoded.len() > 1_048_576 {
            return Err("projection_limit");
        }
        self.projected.extend(encoded);
        if let Some(candidate) = terminal_commit {
            self.observations = candidate;
            self.active_turn = false;
        }
        Ok(())
    }
}

fn nonblocking(file: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("pipe_nonblocking");
    }
    Ok(())
}

fn stop(child: &mut Child) -> Result<()> {
    let pid = child.id() as i32;
    // Cleanup never renews the phase deadline. KILL is immediate; no new native process.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err("owned_group_stop");
    }
    // This is a bounded diagnostic cleanup interval, never a renewed agent phase.
    // KILL was already sent; an unreaped leader remains explicitly unknown.
    let cleanup_started = Instant::now();
    loop {
        if child.try_wait().map_err(|_| "owned_leader_reap")?.is_some() {
            return Ok(());
        }
        if cleanup_started.elapsed() >= Duration::from_millis(250) {
            return Err("owned_leader_reap_unknown");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct OwnedChild {
    child: Child,
    reaped: bool,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = stop(&mut self.child);
        }
    }
}

fn answer(control: &Control) -> Result<Value> {
    let bytes = snapshot(&control.cwd.join("answer.json"), 65_536, true)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "answer_json")?;
    let object = value.as_object().ok_or("answer_object")?;
    let text = object
        .get("answer")
        .and_then(Value::as_str)
        .ok_or("answer_string")?;
    if object.len() != 1 || text.chars().count() > 12000 {
        return Err("answer_contract");
    }
    Ok(json!({"sha256":sha(&bytes),"bytes":bytes.len(),"characters":text.chars().count()}))
}

fn execute(admitted: Admitted) -> Result<Value> {
    let control = &admitted.control;
    prelaunch_guard(&control.phase)?;
    fresh_outputs(control, false)?;
    private_new(
        &control.output_dir.join("native-run.claim"),
        admitted.control_sha256.as_bytes(),
    )?;
    private_new(
        &control.output_dir.join("answer.schema.json"),
        &admitted.schema,
    )?;
    let start = Instant::now();
    prelaunch_guard(&control.phase)?;
    // Exact file snapshots are admitted. Read-only native/tool mounts remain an external gate.
    executable(&control.runtime)?;
    for document in &control.raw_documents {
        asset(document, 67_108_864, false)?;
    }
    prelaunch_guard(&control.phase)?;
    let mut command = Command::new(&control.runtime.path);
    command
        .args(argv(control))
        .current_dir(&control.cwd)
        .env_clear()
        .envs(&control.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    fresh_outputs(control, true)?;
    if snapshot(&control.output_dir.join("native-run.claim"), 64, true)?
        != admitted.control_sha256.as_bytes()
        || snapshot(&control.output_dir.join("answer.schema.json"), 65_536, true)?
            != admitted.schema
    {
        return Err("reserved_output_changed");
    }
    prelaunch_guard(&control.phase)?;
    let mut owned_child = OwnedChild {
        child: command.spawn().map_err(|_| "native_spawn")?,
        reaped: false,
    };
    let child = &mut owned_child.child;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().ok_or("stdout_pipe")?;
    let mut stderr = child.stderr.take().ok_or("stderr_pipe")?;
    let mut failure = None;
    if nonblocking(&stdout)
        .and_then(|()| nonblocking(&stderr))
        .and_then(|()| nonblocking(stdin.as_ref().ok_or("stdin_pipe")?))
        .is_err()
    {
        failure = Some("pipe_nonblocking");
    }
    let prompt = admitted.prompt.as_deref().ok_or("prompt_missing")?;
    let mut sent = 0usize;
    let mut stdout_bytes = 0usize;
    let mut stderr_bytes = 0usize;
    let mut stdout_hash = Sha256::new();
    let mut stderr_hash = Sha256::new();
    let mut line = Vec::new();
    let mut events = Events::default();
    let mut eof = [false, false];
    let mut exit = None;
    while failure.is_none() && !(exit.is_some() && eof.iter().all(|v| *v)) {
        if INTERRUPTED.load(Ordering::Relaxed) {
            failure = Some("launcher_interrupted");
            break;
        }
        if let Err(error) = control.phase.check() {
            failure = Some(error);
            break;
        }
        if let Some(input) = stdin.as_mut() {
            match input.write(&prompt[sent..]) {
                Ok(0) => {
                    failure = Some("stdin_closed");
                }
                Ok(n) => {
                    sent += n;
                    if sent == prompt.len() {
                        stdin.take();
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    failure = Some("stdin_write");
                }
            }
        }
        for (stream, stream_eof) in eof.iter_mut().enumerate() {
            if *stream_eof {
                continue;
            }
            let mut scratch = [0u8; 4096];
            let read = if stream == 0 {
                stdout.read(&mut scratch)
            } else {
                stderr.read(&mut scratch)
            };
            match read {
                Ok(0) => *stream_eof = true,
                Ok(n) => {
                    let (count, limit, hash) = if stream == 0 {
                        (
                            &mut stdout_bytes,
                            control.limits.stdout_bytes,
                            &mut stdout_hash,
                        )
                    } else {
                        (
                            &mut stderr_bytes,
                            control.limits.stderr_bytes,
                            &mut stderr_hash,
                        )
                    };
                    if n > limit.saturating_sub(*count) {
                        failure = Some(if stream == 0 {
                            "stdout_limit"
                        } else {
                            "stderr_limit"
                        });
                        break;
                    }
                    *count += n;
                    hash.update(&scratch[..n]);
                    if stream == 0 {
                        for &byte in &scratch[..n] {
                            if byte == b'\n' {
                                if let Err(error) = events.line(&line) {
                                    failure = Some(error);
                                    break;
                                }
                                line.clear();
                            } else if line.len() >= control.limits.event_bytes {
                                failure = Some("event_limit");
                                break;
                            } else {
                                line.push(byte);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    failure = Some("pipe_read");
                }
            }
        }
        if exit.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit = Some(status);
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    failure = Some("native_wait");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let cleanup = stop(child);
    let status = exit.or_else(|| child.try_wait().ok().flatten());
    // Cleanup has been attempted exactly once; do not silently retry a failed stop.
    owned_child.reaped = true;
    if cleanup.is_err() {
        failure = Some("owned_cleanup_unknown");
    }
    if !line.is_empty() && failure.is_none() {
        failure = Some("incomplete_jsonl");
    }
    if events.failed && failure.is_none() {
        failure = Some("native_event_failure");
    }
    if status.is_none_or(|s| !s.success()) && failure.is_none() {
        failure = Some("native_nonzero");
    }
    if (events.active_turn || events.observations.completed_turns == 0) && failure.is_none() {
        failure = Some("missing_terminal");
    }
    if let Err(error) = control.phase.check()
        && failure.is_none()
    {
        failure = Some(error);
    }
    let answer_result = if failure.is_none() {
        match answer(control) {
            Ok(value) => Some(value),
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    } else {
        None
    };
    private_new(
        &control.output_dir.join("native-events.jsonl"),
        &events.projected,
    )?;
    if let Err(error) = control.phase.check()
        && failure.is_none()
    {
        failure = Some(error);
    }
    let mut receipt = json!({
        "schema_version":1,"status":if failure.is_some(){"failed"}else{"answered_ungraded"},"failure":failure,
        "runtime_kind":control.runtime_kind,"arm":control.arm,"control_sha256":admitted.control_sha256,
        "phase":control.phase,"requested":control.profile,
        "actual":{"model":null,"provider":null,"reasoning_effort":null,"service_tier":null,"paid_request_count":null,"cost_usd":null},
        "observations":events.observations,"native_exit_code":status.and_then(|s|s.code()),"native_signal":status.and_then(|s|s.signal()),
        "owned_leader_reaped":cleanup.is_ok(),"descendant_reaping_qualified":false,
        "stdout_admitted_bytes":stdout_bytes,"stderr_admitted_bytes":stderr_bytes,"stdout_admitted_sha256":format!("{:x}",stdout_hash.finalize()),"stderr_admitted_sha256":format!("{:x}",stderr_hash.finalize()),
        "latency_ms":start.elapsed().as_millis(),"answer":answer_result,"environment_names":control.environment.keys().collect::<Vec<_>>(),
        "execution_qualified":false,"qualification":"source component only; external runtime/auth/mount/resource/tool adoption required",
        "raw_output_retained":false,"projection":"event kinds, hashed item IDs and numeric usage only; stderr contents discarded"
    });
    let mut bytes = serde_json::to_vec(&receipt).map_err(|_| "receipt_encode")?;
    if bytes.len() > 65_536 {
        return Err("receipt_limit");
    }
    // Hold the prepared receipt until its original deadline survives serialization/fsync.
    let pending = control.output_dir.join(".receipt.pending");
    let final_path = control.output_dir.join("receipt.json");
    private_new(&pending, &bytes)?;
    if receipt["status"] == "answered_ungraded"
        && let Err(error) = control.phase.check()
    {
        receipt["status"] = json!("failed");
        receipt["failure"] = json!(error);
        receipt["answer"] = Value::Null;
        bytes = serde_json::to_vec(&receipt).map_err(|_| "receipt_encode")?;
        fs::remove_file(&pending).map_err(|_| "pending_remove")?;
        private_new(&pending, &bytes)?;
    }
    fs::hard_link(&pending, &final_path).map_err(|_| "fresh_receipt_required")?;
    fs::remove_file(&pending).map_err(|_| "pending_remove")?;
    if receipt["status"] == "answered_ungraded"
        && let Err(error) = control.phase.check()
    {
        // Only this invocation's newly linked receipt is removed; no prior output is replaced.
        fs::remove_file(&final_path).map_err(|_| "receipt_remove")?;
        receipt["status"] = json!("failed");
        receipt["failure"] = json!(error);
        receipt["answer"] = Value::Null;
        bytes = serde_json::to_vec(&receipt).map_err(|_| "receipt_encode")?;
        private_new(&final_path, &bytes)?;
    }
    Ok(receipt)
}

fn main() {
    unsafe {
        libc::signal(libc::SIGTERM, interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t);
    }
    let args = Args::parse();
    let result = admit(&args.control, &args.control_sha256, args.run).and_then(|admitted| {
        if args.run { execute(admitted) } else { Ok(json!({"schema_version":1,"status":"admitted_source_only","control_sha256":args.control_sha256,"phase":admitted.control.phase,"requested":admitted.control.profile,"prompt_path":admitted.control.prompt.path,"prompt_sha256":admitted.control.prompt.sha256,"output_dir":admitted.control.output_dir,"execution_qualified":false})) }
    });
    match result {
        Ok(receipt) => {
            println!("{}", receipt);
            if args.run && receipt["status"] != "answered_ungraded" {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                json!({"schema_version":1,"status":"rejected","failure":error})
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[path = "codex_agent_tests.rs"]
mod tests;
