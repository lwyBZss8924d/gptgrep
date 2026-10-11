//! One explicitly authorized, two-arm native Page image probe. No acceptance.
//!
//! Run the compiled example with CONTROL.json CONTROL_SHA256, or prepend
//! --describe-runtime to print factory values without executing either probe.
//! Control uses the closed `Control` schema below (at most 64 KiB). Paths and outcomes stay in the
//! private NDJSON evidence file; stdout contains only the terminal disposition.
//! The owner must supply generated image sources, existing external private
//! directories and TMPDIR/TMP/TEMP pointing at `temp_dir`. No fixtures, expected
//! answers, authentication files, source downloads or services are created here.
//! This is a fresh run only: evidence and ledger/lock must not already exist.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use gptgrep_host::parse_agent_driver;
use gptgrep_host::{
    NativeOcrProbeAdmission, OcrExecutionContext, OcrHostConfig, OcrHostExecutor,
    OcrImageTransport, OcrRuntimeProfile,
};
use gptgrep_jev::RunClockAnchor;
use gptgrep_parse::{
    NativeOptions, PreparationOptions, ResourceBudget, extract_native, prepare_document,
    validate_ocr_task,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CONTROL_BYTES: u64 = 65_536;
const EVIDENCE_BYTES: usize = 4_194_304;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FilePin {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

/// These source/protocol/guard pins, plus the binary pin and current library
/// policy helpers, determine the unqualified runtime profile without a receipt.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfilePins {
    source_revision: String,
    protocol_schema_sha256: String,
    guard_source: FilePin,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema: String,
    native_binary: FilePin,
    profile: ProfilePins,
    auth_home: PathBuf,
    state_dir: PathBuf,
    sources: [FilePin; 2],
    storage_root: PathBuf,
    storage_device_id: u64,
    staging_root: PathBuf,
    results_root: PathBuf,
    temp_dir: PathBuf,
    evidence_output: PathBuf,
    ledger: PathBuf,
    run_id: String,
    plan_sha256: String,
    authorization_receipt_sha256: String,
    timeout_seconds: u64,
    #[serde(default)]
    original_clock: Option<RunClockAnchor>,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "probe_path"
    );
    Ok(())
}

fn directory(path: &Path, private: bool) -> Result<()> {
    absolute(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && fs::canonicalize(path)? == path,
        "probe_directory"
    );
    ensure!(
        !private || metadata.permissions().mode() & 0o077 == 0,
        "probe_private_directory"
    );
    Ok(())
}

fn fresh(path: &Path) -> Result<()> {
    absolute(path)?;
    directory(path.parent().context("probe_parent")?, true)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => bail!("probe_destination_not_fresh"),
    }
}

fn same(a: &Metadata, b: &Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

fn open_regular(path: &Path, cap: u64) -> Result<(File, Metadata)> {
    absolute(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= cap && fs::canonicalize(path)? == path,
        "probe_file"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(same(&metadata, &file.metadata()?), "probe_file_changed");
    Ok((file, metadata))
}

fn verify_pin(pin: &FilePin, cap: u64) -> Result<()> {
    ensure!(
        digest(&pin.sha256) && pin.bytes > 0 && pin.bytes <= cap,
        "probe_pin"
    );
    let (mut file, before) = open_regular(&pin.path, cap)?;
    ensure!(before.len() == pin.bytes, "probe_pin_bytes");
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0; 65_536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).context("probe_pin_bytes")?;
        ensure!(count <= pin.bytes, "probe_pin_bytes");
        hash.update(&buffer[..n]);
    }
    ensure!(
        count == pin.bytes && format!("{:x}", hash.finalize()) == pin.sha256,
        "probe_pin_digest"
    );
    ensure!(
        same(&before, &file.metadata()?) && same(&before, &fs::symlink_metadata(&pin.path)?),
        "probe_file_changed"
    );
    Ok(())
}

struct Evidence {
    file: File,
    bytes: usize,
}
impl Evidence {
    fn record(&mut self, value: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&value)?;
        self.bytes = self
            .bytes
            .checked_add(bytes.len() + 1)
            .context("probe_evidence_limit")?;
        ensure!(self.bytes <= EVIDENCE_BYTES, "probe_evidence_limit");
        self.file.write_all(&bytes)?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()?;
        Ok(())
    }
}

fn validate(control: &Control) -> Result<()> {
    ensure!(
        control.schema == "gptgrep.native-page-probe.control.v1"
            && (1..=900).contains(&control.timeout_seconds)
            && digest(&control.plan_sha256)
            && digest(&control.authorization_receipt_sha256),
        "probe_control"
    );
    directory(&control.auth_home, false)?;
    directory(&control.state_dir, true)?;
    directory(&control.state_dir.join("logs"), false)?;
    ensure!(
        !control.auth_home.starts_with(&control.state_dir)
            && !control.state_dir.starts_with(&control.auth_home),
        "probe_account_scope"
    );
    directory(&control.storage_root, false)?;
    ensure!(
        fs::metadata(&control.storage_root)?.dev() == control.storage_device_id,
        "probe_storage_device_changed"
    );
    let directories = [
        &control.staging_root,
        &control.results_root,
        &control.temp_dir,
    ];
    for (index, path) in directories.iter().enumerate() {
        directory(path, true)?;
        ensure!(
            path.starts_with(&control.storage_root) && *path != &control.storage_root,
            "probe_storage_scope"
        );
        ensure!(
            directories[..index]
                .iter()
                .all(|other| !path.starts_with(other) && !other.starts_with(path)),
            "probe_output_overlap"
        );
        ensure!(
            !path.starts_with(&control.auth_home)
                && !control.auth_home.starts_with(path)
                && !path.starts_with(&control.state_dir)
                && !control.state_dir.starts_with(path),
            "probe_account_scope"
        );
    }
    ensure!(
        std::env::temp_dir() == control.temp_dir,
        "probe_temp_directory_required"
    );
    for name in ["TMPDIR", "TMP", "TEMP"] {
        ensure!(
            std::env::var_os(name).as_deref() == Some(control.temp_dir.as_os_str()),
            "probe_temp_environment_required"
        );
    }
    for path in [&control.evidence_output, &control.ledger] {
        fresh(path)?;
        ensure!(
            path.starts_with(&control.storage_root)
                && directories.iter().all(|dir| !path.starts_with(dir))
                && !path.starts_with(&control.auth_home)
                && !path.starts_with(&control.state_dir),
            "probe_private_output_scope"
        );
    }
    let mut lock = control.ledger.as_os_str().to_os_string();
    lock.push(".lock");
    fresh(Path::new(&lock))?;
    ensure!(
        control.evidence_output != control.ledger && control.evidence_output != Path::new(&lock),
        "probe_output_collision"
    );
    ensure!(
        control.sources[0].path != control.sources[1].path
            && control.sources[0].sha256 != control.sources[1].sha256,
        "probe_distinct_sources"
    );
    for pin in control
        .sources
        .iter()
        .chain([&control.native_binary, &control.profile.guard_source])
    {
        ensure!(
            !pin.path.starts_with(&control.auth_home)
                && !pin.path.starts_with(&control.state_dir)
                && directories.iter().all(|dir| !pin.path.starts_with(dir)),
            "probe_input_scope"
        );
    }
    for source in &control.sources {
        ensure!(
            matches!(
                source.path.extension().and_then(|s| s.to_str()),
                Some("png" | "jpg" | "jpeg")
            ),
            "probe_raster_source_required"
        );
        verify_pin(source, ResourceBudget::default().image_file_bytes)?;
    }
    verify_pin(&control.native_binary, 1_073_741_824)?;
    verify_pin(&control.profile.guard_source, 4_194_304)?;
    Ok(())
}

fn runtime_profile(control: &Control) -> Result<OcrRuntimeProfile> {
    OcrRuntimeProfile::unqualified_native(
        control.profile.source_revision.clone(),
        control.profile.protocol_schema_sha256.clone(),
        control.native_binary.sha256.clone(),
        control.profile.guard_source.sha256.clone(),
    )
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut pairs = object.into_iter().collect::<Vec<_>>();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                pairs
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        other => other,
    }
}

fn describe_runtime(control: &Control) -> Result<()> {
    let profile = runtime_profile(control)?;
    let descriptor = parse_agent_driver::document_model_descriptor();
    let template = canonical(parse_agent_driver::document_permissions_template(true));
    ensure!(
        (1..=900).contains(&control.timeout_seconds),
        "probe_timeout"
    );
    let deadline = SystemTime::now()
        .checked_add(Duration::from_secs(control.timeout_seconds))
        .context("probe_deadline")?;
    println!(
        "{}",
        json!({"schema":"gptgrep.native-page-probe.runtime-description.v1",
        "page_runtime_profile":profile,"page_runtime_profile_sha256":profile.sha256()?,
        "document_model_descriptor":descriptor,"document_model_descriptor_sha256":sha(&serde_json::to_vec(&descriptor)?),
        "document_native_permissions_template":template,"document_permissions_template_sha256":sha(&serde_json::to_vec(&template)?),
        "document_tools_sha256":parse_agent_driver::document_tool_policy_sha256()?,
        "document_instructions_sha256":parse_agent_driver::document_instructions_sha256(),
        "document_guard_contract_sha256":parse_agent_driver::document_guard_contract_sha256()?,
        "new_original_clock":RunClockAnchor::capture(deadline)?,
        "pins_verified":false,"qualification_accepted":false,"models_started":false})
    );
    Ok(())
}

async fn run(control: &Control, control_sha256: &str, evidence: &mut Evidence) -> Result<()> {
    let profile = runtime_profile(control)?;
    let runtime_sha = profile.sha256()?;
    let clock = control
        .original_clock
        .as_ref()
        .context("probe_original_clock_required")?
        .clone();
    ensure!(
        clock.remaining()? <= Duration::from_secs(control.timeout_seconds),
        "probe_original_clock_timeout"
    );
    let context = OcrExecutionContext::with_clock_anchor(
        &control.run_id,
        &control.plan_sha256,
        clock.clone(),
        2,
    )?
    .join_original_ledger(&control.ledger, &clock)?;
    ensure!(
        context.snapshot()?.charged_admissions == 0,
        "probe_ledger_consumed"
    );
    evidence.record(
        json!({"schema":"gptgrep.native-page-probe.evidence.v1","event":"started",
        "control_sha256":control_sha256,"control":control,"original_clock":clock,
        "runtime_profile":profile,"runtime_profile_sha256":runtime_sha,
        "qualification_accepted":false,"actual_served_profile_verified":false}),
    )?;
    let budget = ResourceBudget::default();
    let mut arms = Vec::with_capacity(2);
    // Prepare and validate BOTH complete sources before the first model call.
    for source in &control.sources {
        context.remaining()?;
        verify_pin(source, budget.image_file_bytes)?;
        let prepared = prepare_document(
            &source.path,
            &control.staging_root,
            &PreparationOptions,
            &budget,
        )
        .await?;
        let candidate = extract_native(&prepared, &NativeOptions::default(), &budget).await?;
        context.remaining()?;
        verify_pin(source, budget.image_file_bytes)?;
        prepared.verify(&budget)?;
        ensure!(
            prepared.summary().source_sha256 == source.sha256
                && prepared.summary().source_bytes == source.bytes
                && candidate.plan().blockers.is_empty()
                && candidate.plan().tasks.len() == 1,
            "probe_requires_one_complete_image_task"
        );
        let task = candidate.plan().tasks[0].clone();
        ensure!(
            !task.images.is_empty()
                && task.regions.iter().any(|region| region.required)
                && task.native_text.is_empty(),
            "probe_image_only_task_required"
        );
        validate_ocr_task(&task)?;
        arms.push((prepared, task));
    }
    let started = Instant::now();
    for (index, (prepared, task)) in arms.into_iter().enumerate() {
        context.remaining()?;
        // Parser and host also check the original bytes and exact rendition
        // bindings. Neither synthetic expectations nor the other arm is sent.
        verify_pin(&control.sources[index], budget.image_file_bytes)?;
        verify_pin(&control.native_binary, 1_073_741_824)?;
        verify_pin(&control.profile.guard_source, 4_194_304)?;
        prepared.verify(&budget)?;
        validate_ocr_task(&task)?;
        let admission = NativeOcrProbeAdmission {
            schema_version: "gptgrep.ocr.native-probe-admission.v1".into(),
            authorization_receipt_sha256: control.authorization_receipt_sha256.clone(),
            task_sha256: task.task_sha256.clone(),
            runtime_profile_sha256: runtime_sha.clone(),
            original_deadline_unix_ms: clock.deadline_unix_ms(),
        };
        let executor = OcrHostExecutor::new(
            OcrHostConfig {
                codex_bin: control
                    .native_binary
                    .path
                    .to_str()
                    .context("probe_binary_path")?
                    .into(),
                codex_home: control.auth_home.clone(),
                codex_state_dir: Some(control.state_dir.clone()),
                prepared_root: prepared.staging_root().into(),
                raw_results_dir: control.results_root.clone(),
                runtime_profile: profile.clone(),
                transport: OcrImageTransport::NativeViewer,
            },
            context.clone(),
        )?;
        evidence.record(json!({"event":"arm_admitted_for_probe","arm":index,
            "source_pin":control.sources[index],"prepared":prepared.summary(),
            "prepared_root":prepared.staging_root(),"task":task,"admission":admission,
            "qualification_accepted":false}))?;
        let outcome = executor.probe_native_page(task, admission).await;
        match outcome {
            Ok(report) => evidence.record(json!({"event":"arm_observed","arm":index,
                "report":report,"accounting":context.snapshot()?,"qualification_accepted":false}))?,
            Err(error) => {
                // A failed API call supplies no verified aggregate tool count.
                // Preserve its durable accounting and native receipt directory;
                // never replace unavailable counters with fabricated zeroes.
                evidence.record(json!({"event":"arm_failed","arm":index,"failure":format!("{error:#}"),
                    "accounting":context.snapshot().ok(),"tool_calls":null,"image_deliveries":null,
                    "runtime_error":error.downcast_ref::<gptgrep_host::OcrRuntimeError>(),
                    "protocol_error":error.downcast_ref::<gptgrep_host::HostProtocolError>(),
                    "retained_native_results_root":control.results_root,"qualification_accepted":false}))?;
                return Err(error);
            }
        }
    }
    evidence.record(json!({"event":"complete","arms_observed":2,"elapsed_probe_ms":started.elapsed().as_millis(),
        "accounting":context.snapshot()?,"qualification_accepted":false,
        "provider_ingestion_verified":false,"actual_served_profile_verified":false}))?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let describe = args.first().is_some_and(|arg| arg == "--describe-runtime");
    if describe {
        args.remove(0);
    }
    ensure!(
        args.len() == 2,
        "usage: native_page_probe [--describe-runtime] CONTROL.json CONTROL_SHA256"
    );
    let path = Path::new(&args[0]);
    let expected = args[1].to_str().context("probe_control_digest")?;
    ensure!(digest(expected), "probe_control_digest");
    let (file, metadata) = open_regular(path, CONTROL_BYTES)?;
    let mut bytes = Vec::new();
    file.take(CONTROL_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len() && sha(&bytes) == expected,
        "probe_control_pin"
    );
    let control: Control = serde_json::from_slice(&bytes).context("probe_control_schema")?;
    ensure!(
        control.schema == "gptgrep.native-page-probe.control.v1",
        "probe_control_schema"
    );
    if describe {
        return describe_runtime(&control);
    }
    validate(&control)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&control.evidence_output)?;
    let mut evidence = Evidence { file, bytes: 0 };
    let result = run(&control, expected, &mut evidence).await;
    if let Err(error) = &result {
        evidence.record(
            json!({"event":"failed","control_sha256":expected,"failure":format!("{error:#}"),"qualification_accepted":false}),
        )?;
    }
    println!(
        "{}",
        json!({"schema":"gptgrep.native-page-probe.terminal.v1",
        "status":if result.is_ok(){"evidence_written"}else{"failed"},"qualification_accepted":false})
    );
    result
}
