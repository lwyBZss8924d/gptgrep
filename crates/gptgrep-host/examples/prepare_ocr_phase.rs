//! Initialize one explicitly authorized fresh OCR phase through the typed API.
//!
//! Usage: prepare_ocr_phase CONTROL.json CONTROL_SHA256
//! The closed control below is bounded to 64 KiB. All paths must be absolute,
//! physical paths. The ledger parent must already exist and be writable; both
//! ledger and ledger.lock must be absent. Output contains private clock/path
//! metadata and must remain in the owner's private evidence directory.
//! No models, authentication, listeners, retries, reset or cleanup are performed.

use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use gptgrep_host::{MAX_OCR_ADMISSIONS, OcrExecutionContext, OcrExecutionLimits};
use gptgrep_jev::RunClockAnchor;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CONTROL_BYTES: u64 = 65_536;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema: String,
    run_id: String,
    plan_sha256: String,
    timeout_seconds: u64,
    max_admissions: usize,
    #[serde(default)]
    phase_limits: Option<OcrExecutionLimits>,
    ledger_path: PathBuf,
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
        "prepare_ocr_phase_path"
    );
    Ok(())
}

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    absolute(path)?;
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && before.len() <= limit && fs::canonicalize(path)? == path,
        "prepare_ocr_phase_regular_file"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file()
            && (opened.dev(), opened.ino(), opened.len())
                == (before.dev(), before.ino(), before.len()),
        "prepare_ocr_phase_file_changed"
    );
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    let after = fs::symlink_metadata(path)?;
    ensure!(
        bytes.len() as u64 == opened.len()
            && bytes.len() as u64 <= limit
            && (after.dev(), after.ino(), after.len())
                == (opened.dev(), opened.ino(), opened.len())
            && after.is_file(),
        "prepare_ocr_phase_file_changed"
    );
    Ok(bytes)
}

fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => bail!("prepare_ocr_phase_requires_fresh_ledger_pair"),
    }
}

fn file_pin(path: &Path, limit: u64) -> Result<Value> {
    let bytes = read_regular(path, limit)?;
    Ok(json!({"path":path,"bytes":bytes.len(),"sha256":sha(&bytes)}))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "usage: prepare_ocr_phase CONTROL.json CONTROL_SHA256"
    );
    let input_path = Path::new(&args[0]);
    let expected_sha = args[1].to_str().context("prepare_ocr_phase_input_sha256")?;
    ensure!(digest(expected_sha), "prepare_ocr_phase_input_sha256");
    let bytes = read_regular(input_path, CONTROL_BYTES)?;
    ensure!(
        sha(&bytes) == expected_sha,
        "prepare_ocr_phase_input_sha256"
    );
    let control: Control = serde_json::from_slice(&bytes)?;
    ensure!(
        control.schema == "gptgrep.ocr-phase.prepare-input.v1",
        "prepare_ocr_phase_input_schema"
    );
    ensure!(
        !control.run_id.is_empty()
            && control.run_id.len() <= 256
            && !control.run_id.chars().any(char::is_control)
            && digest(&control.plan_sha256),
        "prepare_ocr_phase_run_binding"
    );
    ensure!(
        (1..=86_400).contains(&control.timeout_seconds),
        "prepare_ocr_phase_timeout"
    );
    if let Some(limits) = &control.phase_limits {
        limits.validate()?;
        ensure!(
            limits.max_admissions == control.max_admissions,
            "prepare_ocr_phase_admission_binding"
        );
    } else {
        ensure!(
            (1..=MAX_OCR_ADMISSIONS).contains(&control.max_admissions),
            "prepare_ocr_phase_admission_limit"
        );
    }
    absolute(&control.ledger_path)?;
    control
        .ledger_path
        .file_name()
        .context("prepare_ocr_phase_ledger_name")?;
    let parent = control
        .ledger_path
        .parent()
        .context("prepare_ocr_phase_ledger_parent")?;
    ensure!(
        fs::symlink_metadata(parent)?.is_dir() && fs::canonicalize(parent)? == parent,
        "prepare_ocr_phase_ledger_parent"
    );
    let parent_c = CString::new(parent.as_os_str().as_bytes())?;
    // Check the existing parent without creating a separate write-probe artifact.
    ensure!(
        unsafe { libc::access(parent_c.as_ptr(), libc::W_OK | libc::X_OK) } == 0,
        "prepare_ocr_phase_parent_not_writable"
    );
    let mut lock_name = control.ledger_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    absent(&control.ledger_path)?;
    absent(&lock_path)?;

    // Exactly one clock capture for this fresh phase; all later use adopts it.
    let deadline = SystemTime::now()
        .checked_add(Duration::from_secs(control.timeout_seconds))
        .context("prepare_ocr_phase_deadline")?;
    let original_clock = RunClockAnchor::capture(deadline)?;
    let context = match &control.phase_limits {
        Some(limits) => OcrExecutionContext::with_phase_limits(
            &control.run_id,
            &control.plan_sha256,
            original_clock.clone(),
            limits.clone(),
        )?,
        None => OcrExecutionContext::with_clock_anchor(
            &control.run_id,
            &control.plan_sha256,
            original_clock.clone(),
            control.max_admissions,
        )?,
    };
    absent(&control.ledger_path)?;
    absent(&lock_path)?;
    context.remaining()?;
    let context = context.join_original_ledger(&control.ledger_path, &original_clock)?;
    let snapshot = context.snapshot()?;
    ensure!(
        snapshot.charged_admissions == 0 && snapshot.active_or_quarantined_slots == 0,
        "prepare_ocr_phase_not_fresh"
    );
    let ledger_limit = control
        .phase_limits
        .as_ref()
        .map_or(128 * 1024, |limits| limits.ledger_bytes);
    println!(
        "{}",
        json!({
            "schema":"gptgrep.ocr-phase.prepared.v1",
            "input":{"path":input_path,"bytes":bytes.len(),"sha256":expected_sha},
            "run_id":control.run_id,"plan_sha256":control.plan_sha256,
            "original_clock":original_clock,"deadline_unix_ms":snapshot.deadline_unix_ms,
            "phase_limits":control.phase_limits,"context_snapshot":snapshot,
            "ledger":file_pin(&control.ledger_path,ledger_limit)?,
            "ledger_lock":file_pin(&lock_path,ledger_limit)?,
            "models_started":false
        })
    );
    Ok(())
}
