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
    /// A resumed source-repair phase keeps its original same-boot deadline.
    #[serde(default)]
    original_clock: Option<RunClockAnchor>,
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
    let mut prepared = prepare(&control)?;
    prepared["input"] = json!({"path":input_path,"bytes":bytes.len(),"sha256":expected_sha});
    println!("{prepared}");
    Ok(())
}

fn prepare(control: &Control) -> Result<Value> {
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

    let timeout = Duration::from_secs(control.timeout_seconds);
    let original_clock = match &control.original_clock {
        Some(clock) => clock.clone(),
        None => RunClockAnchor::capture(
            SystemTime::now()
                .checked_add(timeout)
                .context("prepare_ocr_phase_deadline")?,
        )?,
    };
    // Validate expiry and same-boot identity before either ledger file exists.
    // Imported anchors are never recaptured or shortened to a new deadline.
    ensure!(
        original_clock.remaining()? <= timeout,
        "prepare_ocr_phase_clock_exceeds_timeout"
    );
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
    Ok(json!({
        "schema":"gptgrep.ocr-phase.prepared.v1",
        "run_id":control.run_id,"plan_sha256":control.plan_sha256,
        "original_clock":original_clock,"deadline_unix_ms":snapshot.deadline_unix_ms,
        "phase_limits":control.phase_limits,"context_snapshot":snapshot,
        "ledger":file_pin(&control.ledger_path,ledger_limit)?,
        "ledger_lock":file_pin(&lock_path,ledger_limit)?,
        "models_started":false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(root: &Path, original_clock: Option<RunClockAnchor>) -> Control {
        Control {
            schema: "gptgrep.ocr-phase.prepare-input.v1".into(),
            run_id: "synthetic-helper-fixture".into(),
            plan_sha256: "a".repeat(64),
            timeout_seconds: 60,
            original_clock,
            max_admissions: 1,
            phase_limits: None,
            ledger_path: root.join("synthetic-ledger.json"),
        }
    }

    #[test]
    fn carried_clock_is_retained_in_output_and_fresh_ledger() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let clock = RunClockAnchor::capture(SystemTime::now() + Duration::from_secs(30))?;
        let selected = control(&root, Some(clock.clone()));
        let output = prepare(&selected)?;
        assert_eq!(output["original_clock"], serde_json::to_value(&clock)?);
        assert_eq!(output["deadline_unix_ms"], clock.deadline_unix_ms());
        let joined = OcrExecutionContext::with_clock_anchor(
            &selected.run_id,
            &selected.plan_sha256,
            clock.clone(),
            selected.max_admissions,
        )?
        .join_existing_original_ledger(&selected.ledger_path, &clock)?;
        assert!(joined.ledger_clock_anchor()?.as_ref() == Some(&clock));
        assert_eq!(joined.snapshot()?.charged_admissions, 0);
        Ok(())
    }

    #[test]
    fn expired_foreign_or_overlong_clocks_fail_before_ledger_creation() -> Result<()> {
        let clock = RunClockAnchor::capture(SystemTime::now() + Duration::from_secs(60))?;
        for case in ["expired", "foreign", "timeout"] {
            let temporary = tempfile::tempdir()?;
            let root = temporary.path().canonicalize()?;
            let mut value = serde_json::to_value(&clock)?;
            if case == "expired" {
                value["deadline_unix_ms"] = json!(1);
            } else if case == "foreign" {
                value["boot_identity"] = json!("synthetic-foreign-boot");
            }
            let mut selected = control(&root, Some(serde_json::from_value(value)?));
            if case == "timeout" {
                selected.timeout_seconds = 1;
            }
            assert!(prepare(&selected).is_err(), "{case}");
            assert!(!selected.ledger_path.exists(), "{case}");
            assert!(!root.join("synthetic-ledger.json.lock").exists(), "{case}");
        }
        Ok(())
    }

    #[test]
    fn omitted_clock_keeps_fresh_capture_and_existing_pair_refusal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let selected = control(&root, None);
        let started = SystemTime::now();
        let output = prepare(&selected)?;
        let clock: RunClockAnchor = serde_json::from_value(output["original_clock"].clone())?;
        assert!(clock.remaining()? <= Duration::from_secs(selected.timeout_seconds));
        assert!(SystemTime::UNIX_EPOCH + Duration::from_millis(clock.deadline_unix_ms()) > started);
        let ledger = fs::read(&selected.ledger_path)?;
        assert!(prepare(&selected).is_err());
        assert_eq!(fs::read(&selected.ledger_path)?, ledger);
        Ok(())
    }
}
