//! `gptgrep decisions-budget`: create a new same-host Decisions ledger and its
//! portable control file, or read its status without locking or writing.

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const INIT_SCHEMA: &str = "gptgrep.decisions-budget-init.v1";
pub(crate) const MAX_PLAN_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_DEADLINE_SECONDS: u64 = 86_400;

/// Typed command failure so JSON errors carry a stable code.
#[derive(Debug)]
pub(crate) struct DecisionsBudgetCommandError(pub(crate) String);
impl std::fmt::Display for DecisionsBudgetCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for DecisionsBudgetCommandError {}

fn command_error(error: anyhow::Error) -> anyhow::Error {
    DecisionsBudgetCommandError(format!("{error:#}")).into()
}

/// Lowercase SHA-256 of a bounded regular plan file, read once.
pub(crate) fn plan_digest(path: &Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Refuse FIFOs and devices without waiting for a writer.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| anyhow!("Decisions plan file unavailable"))?;
    ensure!(
        file.metadata()
            .map_err(|_| anyhow!("Decisions plan file unavailable"))?
            .is_file(),
        "Decisions plan file must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("Decisions plan file unavailable"))?;
    ensure!(
        bytes.len() as u64 <= MAX_PLAN_BYTES,
        "Decisions plan file exceeds 1 MiB"
    );
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub(crate) fn init(
    run_id: &str,
    plan_file: &Path,
    deadline_seconds: u64,
    max_submissions: u32,
    ledger: &Path,
    output: &Path,
) -> Result<Value> {
    (|| -> Result<Value> {
        ensure!(
            (1..=MAX_DEADLINE_SECONDS).contains(&deadline_seconds),
            "Decisions deadline must be 1 to 86400 seconds"
        );
        ensure!(
            ledger.is_absolute(),
            "Decisions ledger path must be absolute"
        );
        ensure!(
            output.is_absolute(),
            "Decisions budget control path must be absolute"
        );
        let plan_digest = plan_digest(plan_file)?;
        let now_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock precedes the Unix epoch")?
                .as_millis(),
        )?;
        let deadline_unix_ms = now_ms
            .checked_add(deadline_seconds * 1000)
            .context("Invalid Decisions deadline")?;
        let budget = gptgrep_jev::DecisionsBudget::initialize(
            run_id,
            &plan_digest,
            deadline_unix_ms,
            max_submissions,
            ledger,
            output,
        )?;
        let control = std::fs::read(output).context("Decisions budget file unavailable")?;
        Ok(json!({
            "schema_version": INIT_SCHEMA,
            "status": "initialized",
            "control_schema": gptgrep_jev::DECISIONS_BUDGET_CONTRACT_SCHEMA,
            "run_id": budget.run_id,
            "plan_digest": budget.plan_digest,
            "deadline_unix_ms": budget.deadline_unix_ms,
            "max_submissions": budget.max_submissions,
            "max_concurrent_requests": gptgrep_jev::MAX_CONCURRENT_REQUESTS,
            "ledger_path": budget.ledger_path,
            "control_path": output,
            "control_sha256": format!("{:x}", Sha256::digest(&control)),
            "control_mode": "0600",
            "joins_existing": false
        }))
    })()
    .map_err(command_error)
}

pub(crate) fn status(control: &Path) -> Result<Value> {
    (|| -> Result<Value> {
        let status = gptgrep_jev::DecisionsBudget::read(control)?.status()?;
        Ok(serde_json::to_value(status)?)
    })()
    .map_err(command_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_digest_is_bounded_regular_file_sha256() {
        let directory = tempfile::tempdir().unwrap();
        let plan = directory.path().join("plan.json");
        std::fs::write(&plan, b"{\"steps\":[]}\n").unwrap();
        assert_eq!(
            plan_digest(&plan).unwrap(),
            format!("{:x}", Sha256::digest(b"{\"steps\":[]}\n"))
        );
        assert!(plan_digest(directory.path()).is_err());
        assert!(plan_digest(&directory.path().join("missing.json")).is_err());
        let large = directory.path().join("large.json");
        std::fs::write(&large, vec![b' '; MAX_PLAN_BYTES as usize + 1]).unwrap();
        assert!(plan_digest(&large).is_err());
    }

    #[test]
    fn init_rejects_relative_paths_and_out_of_range_deadlines_before_creating_files() {
        let directory = tempfile::tempdir().unwrap();
        let plan = directory.path().join("plan.json");
        std::fs::write(&plan, b"{}").unwrap();
        let ledger = directory.path().join("ledger.json");
        let output = directory.path().join("control.json");
        for (deadline, ledger, output) in [
            (0, ledger.as_path(), output.as_path()),
            (MAX_DEADLINE_SECONDS + 1, ledger.as_path(), output.as_path()),
            (60, Path::new("relative-ledger.json"), output.as_path()),
            (60, ledger.as_path(), Path::new("relative-control.json")),
        ] {
            let error = init("synthetic-run", &plan, deadline, 2, ledger, output).unwrap_err();
            assert!(error.is::<DecisionsBudgetCommandError>());
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
