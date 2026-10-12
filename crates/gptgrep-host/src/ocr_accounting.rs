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

/// Explicit finite product phase limits; legacy 24-admission journals stay v1.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OcrExecutionLimits {
    pub max_admissions: usize,
    pub max_concurrency: usize,
    pub ledger_bytes: u64,
}
impl OcrExecutionLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.max_admissions)
                && (1..=MAX_OCR_CONCURRENCY).contains(&self.max_concurrency)
                && (8192..=1_048_576).contains(&self.ledger_bytes)
                && 4096u64 + self.max_admissions as u64 * 512 <= self.ledger_bytes,
            "ocr_phase_limits"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct OcrExecutionContext(Arc<Inner>);

struct Inner {
    header: Header,
    ledger: Option<PathBuf>,
    require_existing: bool,
    state: Mutex<State>,
    #[cfg(test)]
    test_remaining: Option<Arc<std::sync::atomic::AtomicU64>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limits: Option<OcrExecutionLimits>,
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
            limits: None,
        };
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                header: header.clone(),
                reservations: Vec::new(),
            }),
            header,
            ledger: None,
            require_existing: false,
            #[cfg(test)]
            test_remaining: None,
        })))
    }

    /// Construct a separately versioned finite phase from its original anchor.
    /// This does not initialize a ledger or recapture a clock.
    pub fn with_phase_limits(
        run_id: &str,
        plan_sha256: &str,
        clock: RunClockAnchor,
        limits: OcrExecutionLimits,
    ) -> Result<Self> {
        limits.validate()?;
        let mut context = Self::with_clock_anchor(
            run_id,
            plan_sha256,
            clock,
            limits.max_admissions.min(MAX_OCR_ADMISSIONS),
        )?;
        let inner = Arc::get_mut(&mut context.0).expect("unshared constructor");
        inner.header.schema_version = "gptgrep.ocr-budget.v2".into();
        inner.header.max_admissions = limits.max_admissions;
        inner.header.max_concurrency = limits.max_concurrency;
        inner.header.limits = Some(limits);
        inner
            .state
            .get_mut()
            .map_err(|_| anyhow!("ocr_ledger_unavailable"))?
            .header = inner.header.clone();
        Ok(context)
    }

    /// Join a durable phase and require the complete original attachment anchor.
    /// A mismatched adopted anchor is rejected; it is never reset or extended.
    pub fn join_original_ledger(
        self,
        path: impl AsRef<Path>,
        original: &RunClockAnchor,
    ) -> Result<Self> {
        require_original(&self.0.header.clock, original)?;
        let context = self.attach_ledger(path.as_ref(), Some(original), true)?;
        ensure!(
            context.ledger_clock_anchor()?.as_ref() == Some(original),
            "ocr_original_anchor_mismatch"
        );
        Ok(context)
    }

    #[cfg(test)]
    pub(crate) fn pure_fixture(max_admissions: usize) -> Self {
        // Synthetic test clock only: no OS sample, capture or real run ledger.
        let clock:RunClockAnchor=serde_json::from_value(serde_json::json!({
            "schema":"gptgrep.run-clock-anchor.v1","deadline_unix_ms":999999,
            "clock_kind":"synthetic","boot_identity":"synthetic-not-an-observed-boot",
            "clock_namespace":null,"monotonic_started_ns":1,"monotonic_deadline_ns":60000000001u64})).unwrap();
        let limits = (max_admissions > 24).then_some(OcrExecutionLimits {
            max_admissions,
            max_concurrency: 2,
            ledger_bytes: 1_048_576,
        });
        let header = Header {
            schema_version: if limits.is_some() {
                "gptgrep.ocr-budget.v2"
            } else {
                "gptgrep.ocr-budget.v1"
            }
            .into(),
            run_id: "synthetic".into(),
            plan_sha256: "a".repeat(64),
            deadline_unix_ms: 999999,
            clock,
            max_admissions,
            max_concurrency: 2,
            limits,
        };
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                header: header.clone(),
                reservations: Vec::new(),
            }),
            header,
            ledger: None,
            require_existing: false,
            test_remaining: Some(Arc::new(std::sync::atomic::AtomicU64::new(60000))),
        }))
    }
    pub(crate) fn durable(&self) -> bool {
        self.0.ledger.is_some() && self.0.require_existing
    }
    /// Join only an existing journal/lock pair, requiring the exact original
    /// anchor. Missing state cannot initialize or reset a consumed phase.
    pub fn join_existing_original_ledger(
        self,
        path: impl AsRef<Path>,
        original: &RunClockAnchor,
    ) -> Result<Self> {
        require_original(&self.0.header.clock, original)?;
        self.attach_ledger(path.as_ref(), Some(original), false)
    }
    pub(crate) fn phase_binding(&self) -> Result<(String, usize)> {
        Ok((
            crate::ocr::hash(&serde_json::to_vec(&self.0.header)?),
            self.0.header.max_admissions,
        ))
    }
    /// Read-only owned service allowance; no clock/state access or journal writes.
    pub(crate) fn service_working_bound(&self) -> Result<u64> {
        self.ledger_limit()
            .checked_mul(40)
            .and_then(|n| n.checked_add(65_536))
            .ok_or_else(|| anyhow!("ocr_service_context_working_limit"))
    }
    fn ledger_limit(&self) -> u64 {
        self.0
            .header
            .limits
            .as_ref()
            .map_or(MAX_LEDGER_BYTES, |v| v.ledger_bytes)
    }

    /// Join the same OCR pool before cloning. Qualification attempts use this pool too.
    pub fn with_ledger(self, path: impl AsRef<Path>) -> Result<Self> {
        self.attach_ledger(path.as_ref(), None, true)
    }
    fn attach_ledger(
        self,
        path: &Path,
        original: Option<&RunClockAnchor>,
        create: bool,
    ) -> Result<Self> {
        ensure!(
            Arc::strong_count(&self.0) == 1,
            "ocr_context_already_shared"
        );
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
        let mut new_pair = create;
        if original.is_some() {
            let journal = fs::symlink_metadata(&path);
            let lock = fs::symlink_metadata(&existing_lock);
            match (journal, lock) {
                (Ok(journal), Ok(lock)) => {
                    ensure!(
                        journal.is_file()
                            && lock.is_file()
                            && !journal.file_type().is_symlink()
                            && !lock.file_type().is_symlink(),
                        "ocr_ledger_pair_invalid"
                    );
                    new_pair = false;
                }
                (Err(a), Err(b))
                    if create
                        && a.kind() == std::io::ErrorKind::NotFound
                        && b.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(anyhow!("ocr_existing_ledger_pair_required")),
            }
        }
        inner.require_existing = !new_pair;
        if existing_lock.exists() {
            // A racing initializer may have created this pair after the absent
            // precheck. Once observed, it must never be recreated by this join.
            observe_existing_pair(&mut inner, original);
            let mut lock = LedgerLock::acquire(open_no_follow(&existing_lock, false)?)?;
            let stored: Header = serde_json::from_slice(&read_bounded(
                &mut lock,
                inner
                    .header
                    .limits
                    .as_ref()
                    .map_or(MAX_LEDGER_BYTES, |v| v.ledger_bytes),
            )?)
            .map_err(|_| anyhow!("ocr_invalid_ledger_lock_header"))?;
            let mut comparable = inner.header.clone();
            comparable.clock = stored.clock.clone();
            ensure!(stored == comparable, "ocr_ledger_header_mismatch");
            stored
                .clock
                .verify_deadline(UNIX_EPOCH + Duration::from_millis(stored.deadline_unix_ms))?;
            if let Some(original) = original {
                require_original(&stored.clock, original)?;
            }
            inner.header = stored;
        }
        let context = Self(Arc::new(Inner {
            ledger: Some(path),
            ..inner
        }));
        context.access(|_| Ok(()))?;
        let mut context = context;
        if original.is_some() {
            Arc::get_mut(&mut context.0)
                .expect("unshared attachment")
                .require_existing = true;
        }
        Ok(context)
    }

    pub fn remaining(&self) -> Result<Duration> {
        #[cfg(test)]
        if let Some(ms) = &self.0.test_remaining {
            let ms = ms.load(std::sync::atomic::Ordering::SeqCst);
            ensure!(ms > 0, "ocr_test_deadline_expired");
            return Ok(Duration::from_millis(ms));
        }
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
                    >= state.header.max_concurrency
                {
                    return Ok(None);
                }
                let id = state.reservations.len();
                if state.header.limits.is_some() {
                    ensure!(
                        4096u64 + (state.reservations.len() as u64 + 1) * 512
                            <= self.ledger_limit(),
                        "ocr_ledger_size_limit"
                    );
                    state.reservations.try_reserve_exact(1)?;
                }
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
                        closeout_attempted: false,
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
        let (lock, created) = if self.0.require_existing {
            (open_no_follow(&lock_path, false)?, false)
        } else {
            match open_no_follow(&lock_path, true) {
                Ok(file) => (file, true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    (open_no_follow(&lock_path, false)?, false)
                }
                Err(error) => return Err(error.into()),
            }
        };
        let mut lock = LedgerLock::acquire(lock)?;
        if created {
            // Existing journal without its stable lock identity is never silently adopted.
            ensure!(!path.exists(), "ocr_ledger_lock_missing");
            lock.write_all(&serde_json::to_vec(&self.0.header)?)?;
            lock.sync_all()?;
            File::open(path.parent().expect("canonical ledger parent"))?.sync_all()?;
        } else {
            let stored: Header =
                serde_json::from_slice(&read_bounded(&mut lock, self.ledger_limit())?)
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
            serde_json::from_slice::<State>(&read_bounded(&mut file, self.ledger_limit())?)
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
        if let Some(limits) = &state.header.limits {
            limits.validate()?;
            ensure!(
                state.header.schema_version == "gptgrep.ocr-budget.v2"
                    && state.header.max_admissions == limits.max_admissions
                    && state.header.max_concurrency == limits.max_concurrency,
                "ocr_phase_header_mismatch"
            );
        }
        let bytes = serde_json::to_vec(&state)?;
        ensure!(
            bytes.len() as u64 <= self.ledger_limit(),
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
    closeout_attempted: bool,
}

impl OcrAdmission {
    /// Explicit local closeout. This cannot synthesize provider completion.
    /// A failed durable update stays visible and is not retried silently in Drop.
    pub(crate) fn closeout(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        ensure!(
            !self.closeout_attempted,
            "ocr_journal_closeout_already_attempted"
        );
        self.closeout_attempted = true;
        self.context.update(
            self.id,
            if self.submitted {
                Status::CompletionUnknown
            } else {
                Status::Terminal
            },
        )?;
        self.finished = true;
        Ok(())
    }
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
        if !self.finished && !self.closeout_attempted {
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

/// Unlock before closing: a concurrent process spawn can temporarily inherit
/// the open file description even though its descriptor is close-on-exec.
/// Closing only our descriptor would leave the shared flock held until exec.
struct LedgerLock(File);
impl LedgerLock {
    fn acquire(file: File) -> Result<Self> {
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow!(LedgerBusy)
            } else {
                error.into()
            }
        })?;
        Ok(Self(file))
    }
}
impl std::ops::Deref for LedgerLock {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl std::ops::DerefMut for LedgerLock {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}
impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
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
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        options.mode(0o600);
    }
    options.open(path)
}
fn read_bounded(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "ocr_ledger_size_limit"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "ocr_ledger_size_limit");
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

    #[test]
    fn ledger_lock_release_does_not_wait_for_an_inherited_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("inherited.lock");
        let guard = LedgerLock::acquire(open_no_follow(&path, true).unwrap()).unwrap();
        // A duplicate shares the same open file description as a descriptor
        // inherited between fork and exec, without forking the test process.
        let inherited = guard.try_clone().unwrap();
        let contender = open_no_follow(&path, false).unwrap();
        assert_eq!(
            contender.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(guard);
        contender.try_lock_exclusive().unwrap();
        assert!(inherited.metadata().unwrap().is_file());
        FileExt::unlock(&contender).unwrap();
        drop(inherited);
    }
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

fn observe_existing_pair(inner: &mut Inner, original: Option<&RunClockAnchor>) {
    if original.is_some() {
        inner.require_existing = true;
    }
}

fn require_original(stored: &RunClockAnchor, original: &RunClockAnchor) -> Result<()> {
    ensure!(stored == original, "ocr_original_anchor_mismatch");
    Ok(())
}
#[cfg(test)]
mod phase_tests {
    use super::*;
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    #[tokio::test]
    async fn finite_phase_is_shared_and_unknowns_never_refund_or_reset() {
        let context = OcrExecutionContext::pure_fixture(50);
        for _ in 0..49 {
            context
                .clone()
                .reserve(HASH, HASH)
                .await
                .unwrap()
                .terminal()
                .unwrap();
        }
        let mut last = context.reserve(HASH, HASH).await.unwrap();
        last.submitted().unwrap();
        drop(last);
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 50);
        assert_eq!(snapshot.quarantined_slots, 1);
        assert!(context.clone().reserve(HASH, HASH).await.is_err());
        assert_eq!(snapshot.max_admissions, 50);
        assert_eq!(MAX_OCR_ADMISSIONS, 24);
        assert!(context.ledger_clock_anchor().unwrap().is_none());
    }
    #[test]
    fn legacy_serialization_and_complete_original_anchor_are_preserved() {
        let context = OcrExecutionContext::pure_fixture(2);
        let bytes = serde_json::to_vec(&context.0.header).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("limits").is_none());
        let roundtrip: Header = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&roundtrip).unwrap());
        let mut changed = serde_json::to_value(&context.0.header.clock).unwrap();
        changed["monotonic_started_ns"] = 2.into();
        let different: RunClockAnchor = serde_json::from_value(changed).unwrap();
        assert_eq!(
            different.deadline_unix_ms(),
            context.0.header.clock.deadline_unix_ms()
        );
        assert!(require_original(&different, &context.0.header.clock).is_err());
        require_original(&context.0.header.clock, &context.0.header.clock).unwrap();
        for limit in [
            OcrExecutionLimits {
                max_admissions: 1025,
                max_concurrency: 2,
                ledger_bytes: 1_048_576,
            },
            OcrExecutionLimits {
                max_admissions: 1024,
                max_concurrency: 2,
                ledger_bytes: 8192,
            },
        ] {
            assert!(limit.validate().is_err());
        }
    }
    #[tokio::test]
    async fn expiry_closes_future_admissions_but_retains_observed_unknown() {
        let context = OcrExecutionContext::pure_fixture(2);
        let mut admission = context.reserve(HASH, HASH).await.unwrap();
        admission.submitted().unwrap();
        context
            .0
            .test_remaining
            .as_ref()
            .unwrap()
            .store(0, std::sync::atomic::Ordering::SeqCst);
        drop(admission);
        assert!(context.reserve(HASH, HASH).await.is_err());
        assert_eq!(context.snapshot().unwrap().quarantined_slots, 1);
    }
    #[tokio::test]
    async fn strict_joined_phase_never_recreates_a_removed_journal_or_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("phase.json");
        let mut context = OcrExecutionContext::pure_fixture(50);
        let inner = Arc::get_mut(&mut context.0).unwrap();
        inner.ledger = Some(path.clone());
        // The absent initial precheck permits creation. A separate synthetic
        // initializer then publishes the same bound pair before attachment.
        inner.require_existing = false;
        fs::write(lock_path(&path), serde_json::to_vec(&inner.header).unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&State {
                header: inner.header.clone(),
                reservations: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let inner = Arc::get_mut(&mut context.0).unwrap();
        let expected = inner.header.clock.clone();
        observe_existing_pair(inner, Some(&expected));
        assert!(inner.require_existing);
        let original = context.ledger_clock_anchor().unwrap().unwrap();
        let mut admission = context.reserve(HASH, HASH).await.unwrap();
        admission.submitted().unwrap();
        drop(admission);
        assert_eq!(context.snapshot().unwrap().quarantined_slots, 1);
        let bound: State = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(bound.header.schema_version, "gptgrep.ocr-budget.v2");
        assert!(bound.header.clock == original);
        assert_eq!(bound.header.max_admissions, 50);
        let lock_bytes = fs::read(lock_path(&path)).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(context.snapshot().is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(lock_path(&path)).unwrap(), lock_bytes);
        fs::remove_file(lock_path(&path)).unwrap();
        assert!(context.snapshot().is_err());
        assert!(!path.exists() && !lock_path(&path).exists());
    }
}
