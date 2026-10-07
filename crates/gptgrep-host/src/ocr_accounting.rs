use anyhow::{Result, anyhow, ensure};
use fs2::FileExt;
use gptgrep_jev::RunClockAnchor;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const MAX_OCR_ADMISSIONS: usize = 24;
pub const MAX_OCR_CONCURRENCY: usize = 2;
const MAX_LEDGER_BYTES: u64 = 128 * 1024;

#[derive(Clone)]
pub struct OcrExecutionContext(Arc<Inner>);

struct Inner {
    header: Header,
    ledger: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Header {
    schema_version: String,
    run_id: String,
    plan_sha256: String,
    deadline_unix_ms: u64,
    clock: RunClockAnchor,
    max_admissions: usize,
    max_concurrency: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    header: Header,
    reservations: Vec<Reservation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    id: usize,
    task_sha256: String,
    profile_sha256: String,
    status: Status,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Reserved,
    Submitted,
    Terminal,
    CompletionUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrBudgetSnapshot {
    pub scope: String,
    pub max_admissions: usize,
    pub charged_admissions: usize,
    pub active_or_quarantined_slots: usize,
    pub quarantined_slots: usize,
    pub terminal_admissions: usize,
    pub deadline_unix_ms: u64,
}

impl OcrExecutionContext {
    pub fn new(
        run_id: &str,
        plan_sha256: &str,
        deadline: SystemTime,
        max_admissions: usize,
    ) -> Result<Self> {
        Self::with_clock_anchor(
            run_id,
            plan_sha256,
            RunClockAnchor::capture(deadline)?,
            max_admissions,
        )
    }

    /// Bind independent Decisions and OCR pools to the run's original shared anchor.
    /// Expired anchors remain inspectable; reserve() rejects expired submissions.
    pub fn with_clock_anchor(
        run_id: &str,
        plan_sha256: &str,
        clock: RunClockAnchor,
        max_admissions: usize,
    ) -> Result<Self> {
        ensure!(valid_id(run_id), "ocr_invalid_run_id");
        ensure!(valid_digest(plan_sha256), "ocr_invalid_plan_digest");
        ensure!(
            (1..=MAX_OCR_ADMISSIONS).contains(&max_admissions),
            "ocr_admission_limit"
        );
        let deadline_unix_ms = clock.deadline_unix_ms();
        clock.verify_deadline(UNIX_EPOCH + Duration::from_millis(deadline_unix_ms))?;
        let header = Header {
            schema_version: "gptgrep.ocr-budget.v1".into(),
            run_id: run_id.into(),
            plan_sha256: plan_sha256.into(),
            deadline_unix_ms,
            clock,
            max_admissions,
            max_concurrency: MAX_OCR_CONCURRENCY,
        };
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                header: header.clone(),
                reservations: Vec::new(),
            }),
            header,
            ledger: None,
        })))
    }

    /// Join the same OCR pool before cloning. Qualification attempts use this pool too.
    pub fn with_ledger(self, path: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            Arc::strong_count(&self.0) == 1,
            "ocr_context_already_shared"
        );
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("ocr_invalid_ledger_path"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent =
            fs::canonicalize(parent).map_err(|_| anyhow!("ocr_ledger_parent_unavailable"))?;
        let path = parent.join(name);
        ensure!(!path.is_symlink(), "ocr_ledger_symlink");
        let mut inner =
            Arc::try_unwrap(self.0).map_err(|_| anyhow!("ocr_context_already_shared"))?;
        // Adopt the first immutable same-boot anchor. A freshly captured anchor must
        // never extend a durable run following wall-clock rollback or process restart.
        let existing_lock = lock_path(&path);
        if existing_lock.exists() {
            let mut lock = open_no_follow(&existing_lock, false)?;
            lock.try_lock_exclusive()?;
            let stored: Header = serde_json::from_slice(&read_bounded(&mut lock)?)
                .map_err(|_| anyhow!("ocr_invalid_ledger_lock_header"))?;
            let mut comparable = inner.header.clone();
            comparable.clock = stored.clock.clone();
            ensure!(stored == comparable, "ocr_ledger_header_mismatch");
            stored
                .clock
                .verify_deadline(UNIX_EPOCH + Duration::from_millis(stored.deadline_unix_ms))?;
            inner.header = stored;
        }
        let context = Self(Arc::new(Inner {
            ledger: Some(path),
            ..inner
        }));
        context.access(|_| Ok(()))?;
        Ok(context)
    }

    pub fn remaining(&self) -> Result<Duration> {
        self.0.header.clock.remaining()
    }

    /// Return the full stored anchor adopted and validated during ledger attachment.
    /// Local contexts have no ledger binding. This reads the immutable attachment
    /// header without filesystem access, journal writes, captures or admissions;
    /// it does not freshly inspect the ledger. Later admissions revalidate its header.
    pub fn ledger_clock_anchor(&self) -> Result<Option<RunClockAnchor>> {
        Ok(self.0.ledger.as_ref().map(|_| self.0.header.clock.clone()))
    }

    pub fn snapshot(&self) -> Result<OcrBudgetSnapshot> {
        self.access(|state| {
            Ok(OcrBudgetSnapshot {
                scope: if self.0.ledger.is_some() {
                    "same_host_ledger"
                } else {
                    "single_invocation"
                }
                .into(),
                max_admissions: state.header.max_admissions,
                charged_admissions: state.reservations.len(),
                active_or_quarantined_slots: state
                    .reservations
                    .iter()
                    .filter(|r| r.status != Status::Terminal)
                    .count(),
                quarantined_slots: state
                    .reservations
                    .iter()
                    .filter(|r| r.status == Status::CompletionUnknown)
                    .count(),
                terminal_admissions: state
                    .reservations
                    .iter()
                    .filter(|r| r.status == Status::Terminal)
                    .count(),
                deadline_unix_ms: state.header.deadline_unix_ms,
            })
        })
    }

    pub(crate) async fn reserve(&self, task: &str, profile: &str) -> Result<OcrAdmission> {
        ensure!(
            valid_digest(task) && valid_digest(profile),
            "ocr_invalid_reservation_digest"
        );
        loop {
            self.remaining()?;
            let result = self.access(|state| {
                ensure!(
                    state.reservations.len() < state.header.max_admissions,
                    "ocr_admissions_exhausted"
                );
                if state
                    .reservations
                    .iter()
                    .filter(|r| r.status != Status::Terminal)
                    .count()
                    >= MAX_OCR_CONCURRENCY
                {
                    return Ok(None);
                }
                let id = state.reservations.len();
                state.reservations.push(Reservation {
                    id,
                    task_sha256: task.into(),
                    profile_sha256: profile.into(),
                    status: Status::Reserved,
                });
                Ok(Some(id))
            });
            match result {
                Ok(Some(id)) => {
                    return Ok(OcrAdmission {
                        context: self.clone(),
                        id,
                        submitted: false,
                        finished: false,
                        observed_terminal: false,
                    });
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(error) if is_busy(&error) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(error) => return Err(error),
            }
        }
    }

    fn update(&self, id: usize, status: Status) -> Result<()> {
        self.access(|state| {
            let entry = state
                .reservations
                .get_mut(id)
                .ok_or_else(|| anyhow!("ocr_reservation_missing"))?;
            ensure!(
                entry.id == id && entry.status != Status::Terminal,
                "ocr_invalid_reservation_transition"
            );
            entry.status = status;
            Ok(())
        })
    }

    fn access<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let Some(path) = &self.0.ledger else {
            let mut state = self
                .0
                .state
                .lock()
                .map_err(|_| anyhow!("ocr_ledger_unavailable"))?;
            return change(&mut state);
        };
        let lock_path = lock_path(path);
        ensure!(!lock_path.is_symlink(), "ocr_ledger_lock_symlink");
        let (mut lock, created) = match open_no_follow(&lock_path, true) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                (open_no_follow(&lock_path, false)?, false)
            }
            Err(error) => return Err(error.into()),
        };
        lock.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow!(LedgerBusy)
            } else {
                error.into()
            }
        })?;
        if created {
            // Existing journal without its stable lock identity is never silently adopted.
            ensure!(!path.exists(), "ocr_ledger_lock_missing");
            lock.write_all(&serde_json::to_vec(&self.0.header)?)?;
            lock.sync_all()?;
            File::open(path.parent().expect("canonical ledger parent"))?.sync_all()?;
        } else {
            let stored: Header = serde_json::from_slice(&read_bounded(&mut lock)?)
                .map_err(|_| anyhow!("ocr_invalid_ledger_lock_header"))?;
            ensure!(stored == self.0.header, "ocr_ledger_header_mismatch");
        }
        let mut state = if created {
            State {
                header: self.0.header.clone(),
                reservations: Vec::new(),
            }
        } else {
            ensure!(path.exists(), "ocr_ledger_journal_missing");
            let mut file = open_no_follow(path, false)?;
            serde_json::from_slice::<State>(&read_bounded(&mut file)?)
                .map_err(|_| anyhow!("ocr_invalid_ledger"))?
        };
        ensure!(state.header == self.0.header, "ocr_ledger_header_mismatch");
        ensure!(
            state.reservations.len() <= state.header.max_admissions,
            "ocr_invalid_ledger_reservations"
        );
        for (id, entry) in state.reservations.iter().enumerate() {
            ensure!(
                entry.id == id
                    && valid_digest(&entry.task_sha256)
                    && valid_digest(&entry.profile_sha256),
                "ocr_invalid_ledger_reservations"
            );
        }
        let result = change(&mut state)?;
        let bytes = serde_json::to_vec(&state)?;
        ensure!(
            bytes.len() as u64 <= MAX_LEDGER_BYTES,
            "ocr_ledger_size_limit"
        );
        // tempfile creates a unique owned file with create_new; no predictable staging name.
        let mut staging = tempfile::Builder::new()
            .prefix(".gptgrep-ocr-")
            .tempfile_in(path.parent().expect("canonical ledger parent"))?;
        staging.write_all(&bytes)?;
        staging.as_file().sync_all()?;
        ensure!(!path.is_symlink(), "ocr_ledger_symlink");
        staging
            .persist(path)
            .map_err(|_| anyhow!("ocr_ledger_publish_failed"))?;
        File::open(path.parent().expect("canonical ledger parent"))?.sync_all()?;
        Ok(result)
    }
}

pub(crate) struct OcrAdmission {
    context: OcrExecutionContext,
    id: usize,
    submitted: bool,
    finished: bool,
    observed_terminal: bool,
}

impl OcrAdmission {
    pub fn observe_terminal(&mut self) {
        self.observed_terminal = true;
    }
    pub fn confirm_terminal(&mut self) -> Result<()> {
        if self.observed_terminal {
            self.terminal()?;
        }
        Ok(())
    }
    pub fn check_deadline(&self, page_deadline: tokio::time::Instant) -> Result<()> {
        ensure!(
            tokio::time::Instant::now() < page_deadline,
            "ocr_page_deadline_completion_unknown"
        );
        self.context.remaining()?;
        Ok(())
    }
    pub fn submitted(&mut self) -> Result<()> {
        self.context.update(self.id, Status::Submitted)?;
        self.submitted = true;
        Ok(())
    }
    pub fn terminal(&mut self) -> Result<()> {
        self.context.update(self.id, Status::Terminal)?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for OcrAdmission {
    fn drop(&mut self) {
        if !self.finished {
            // A failed closeout leaves the existing reservation charged and occupied.
            let _ = self.context.update(
                self.id,
                if self.submitted {
                    Status::CompletionUnknown
                } else {
                    Status::Terminal
                },
            );
        }
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}
fn open_no_follow(path: &Path, create: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        options.mode(0o600);
    }
    options.open(path)
}
fn read_bounded(file: &mut File) -> Result<Vec<u8>> {
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_LEDGER_BYTES,
        "ocr_ledger_size_limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "ocr_ledger_size_limit"
    );
    Ok(bytes)
}
pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
#[derive(Debug)]
struct LedgerBusy;
impl std::fmt::Display for LedgerBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ocr_ledger_busy")
    }
}
impl std::error::Error for LedgerBusy {}
fn is_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LedgerBusy>().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn context(deadline: SystemTime) -> OcrExecutionContext {
        OcrExecutionContext::new("test", HASH, deadline, 3).unwrap()
    }

    #[tokio::test]
    async fn submitted_cancellation_is_charged_and_quarantined() {
        let context = context(SystemTime::now() + Duration::from_secs(3));
        let mut admission = context.reserve(HASH, HASH).await.unwrap();
        admission.submitted().unwrap();
        drop(admission);
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
        let mut second = context.reserve(HASH, HASH).await.unwrap();
        second.terminal().unwrap();
        assert_eq!(context.snapshot().unwrap().charged_admissions, 2);
    }

    #[tokio::test]
    async fn durable_pool_and_lost_journal_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.json");
        let deadline = SystemTime::now() + Duration::from_secs(3);
        let first = context(deadline).with_ledger(&path).unwrap();
        let mut admission = first.reserve(HASH, HASH).await.unwrap();
        admission.submitted().unwrap();
        drop(admission);
        let second = context(deadline).with_ledger(&path).unwrap();
        assert_eq!(second.snapshot().unwrap().quarantined_slots, 1);
        fs::remove_file(&path).unwrap();
        assert!(context(deadline).with_ledger(&path).is_err());
        assert!(path.with_file_name("budget.json.lock").exists());
    }

    #[tokio::test]
    async fn terminals_release_slots_without_refunding_admissions() {
        let context = context(SystemTime::now() + Duration::from_secs(3));
        for _ in 0..3 {
            context
                .reserve(HASH, HASH)
                .await
                .unwrap()
                .terminal()
                .unwrap();
        }
        assert!(context.reserve(HASH, HASH).await.is_err());
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 3);
        assert_eq!(snapshot.active_or_quarantined_slots, 0);
    }

    #[tokio::test]
    async fn a_third_parallel_reservation_waits_without_charging_or_extending() {
        let context = context(SystemTime::now() + Duration::from_secs(3));
        let mut first = context.reserve(HASH, HASH).await.unwrap();
        let mut second = context.reserve(HASH, HASH).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), context.reserve(HASH, HASH))
                .await
                .is_err()
        );
        assert_eq!(context.snapshot().unwrap().charged_admissions, 2);
        first.terminal().unwrap();
        second.submitted().unwrap();
        drop(second);
        let mut third = context.reserve(HASH, HASH).await.unwrap();
        third.terminal().unwrap();
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 3);
        assert_eq!(snapshot.quarantined_slots, 1);
    }

    #[tokio::test]
    async fn joins_adopt_original_clock_and_preserve_foreign_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.json");
        let deadline = SystemTime::now() + Duration::from_secs(3);
        let local = context(deadline);
        assert!(local.ledger_clock_anchor().unwrap().is_none());
        let first = local.with_ledger(&path).unwrap();
        let original = first.ledger_clock_anchor().unwrap().unwrap();
        fs::write(dir.path().join("budget.json.pending"), b"foreign bytes").unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let fresh = context(deadline);
        assert!(fresh.0.header.clock != original);
        assert_eq!(
            fresh.0.header.clock.deadline_unix_ms(),
            original.deadline_unix_ms()
        );
        let second = fresh.with_ledger(&path).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let bytes_before = fs::read(&path).unwrap();
        assert!(second.ledger_clock_anchor().unwrap().as_ref() == Some(&original));
        assert!(first.ledger_clock_anchor().unwrap().as_ref() == Some(&original));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), bytes_before);
        second
            .reserve(HASH, HASH)
            .await
            .unwrap()
            .terminal()
            .unwrap();
        assert_eq!(first.snapshot().unwrap().charged_admissions, 1);
        assert_eq!(
            fs::read(dir.path().join("budget.json.pending")).unwrap(),
            b"foreign bytes"
        );
        assert!(
            OcrExecutionContext::new("other-run", HASH, deadline, 3)
                .unwrap()
                .with_ledger(&path)
                .is_err()
        );
    }
}
