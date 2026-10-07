//! Bounded admission shared by clones; optional same-host durable reservations.
use crate::RunClockAnchor;
use anyhow::{Result, anyhow, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const MAX_CONCURRENT_REQUESTS: usize = 4;
pub const MAX_SUBMISSIONS: u32 = 200;
const MAX_LEDGER_BYTES: u64 = 128 * 1024;

#[derive(Clone)]
pub struct ExecutionContext(Arc<Inner>);
struct Inner {
    header: Header,
    deadline: Option<Instant>,
    ledger: Option<PathBuf>,
    clock_anchor: Mutex<Option<RunClockAnchor>>,
    state: Mutex<State>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    run_id: String,
    plan_digest: String,
    deadline_unix_ms: Option<u64>,
    max_submissions: u32,
    max_concurrent_requests: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialization {
    header: Header,
    clock_anchor: RunClockAnchor,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    header: Header,
    submission_observations: u64,
    reservations: Vec<Entry>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: u32,
    body_sha256: String,
    requested_model: String,
    send_started: bool,
    outcome: Outcome,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Reserved,
    InFlight,
    Unknown,
    Terminal,
}
/// Counts admission and local send-start observations, never physical provider requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionSnapshot {
    pub scope: String,
    /// Actual immutable explicit context binding; legacy invocation is unbound.
    pub run_id: Option<String>,
    pub plan_digest: Option<String>,
    pub max_submissions: u32,
    pub logical_reservations: usize,
    pub send_start_observations: usize,
    pub submission_observations: u64,
    pub active_or_quarantined_slots: usize,
    pub quarantined_slots: usize,
    pub terminal_submissions: usize,
    pub deadline_unix_ms: Option<u64>,
}

impl ExecutionContext {
    /// An explicit experiment context. The absolute deadline also has a monotonic backstop.
    pub fn new(
        run_id: &str,
        plan_digest: &str,
        absolute_deadline: SystemTime,
        max_submissions: u32,
    ) -> Result<Self> {
        ensure!(
            !run_id.is_empty() && run_id.len() <= 256 && !run_id.chars().any(char::is_control),
            "Invalid Decisions run identifier"
        );
        ensure!(valid_digest(plan_digest), "Invalid Decisions plan digest");
        ensure!(
            (1..=MAX_SUBMISSIONS).contains(&max_submissions),
            "Decisions submission limit must be 1 to 200"
        );
        let remaining = absolute_deadline
            .duration_since(SystemTime::now())
            .map_err(|_| anyhow!("Decisions absolute deadline has expired"))?;
        let deadline_unix_ms = u64::try_from(
            absolute_deadline
                .duration_since(UNIX_EPOCH)
                .map_err(|_| anyhow!("Invalid Decisions deadline"))?
                .as_millis(),
        )
        .map_err(|_| anyhow!("Invalid Decisions deadline"))?;
        Self::create(
            Header {
                schema: "gptgrep.decisions-budget.v1".into(),
                run_id: run_id.into(),
                plan_digest: plan_digest.into(),
                deadline_unix_ms: Some(deadline_unix_ms),
                max_submissions,
                max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
            },
            Some(
                Instant::now()
                    .checked_add(remaining)
                    .ok_or_else(|| anyhow!("Invalid Decisions deadline"))?,
            ),
        )
    }

    /// Add an explicit persistent ledger before sharing the context with callers.
    /// Existing ledgers must match the entire immutable run/plan/deadline/limits header.
    pub fn with_ledger(self, path: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            self.0.header.deadline_unix_ms.is_some(),
            "A ledger requires an explicit absolute deadline"
        );
        ensure!(
            Arc::strong_count(&self.0) == 1,
            "Attach the ledger before cloning the execution context"
        );
        ensure!(
            self.0.ledger.is_none(),
            "Execution context already has a ledger"
        );
        {
            let state = self
                .0
                .state
                .lock()
                .map_err(|_| anyhow!("Decisions budget state unavailable"))?;
            ensure!(
                state.reservations.is_empty() && state.submission_observations == 0,
                "Attach the ledger before any execution context use"
            );
        }
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("Invalid Decisions ledger path"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = fs::canonicalize(parent)
            .map_err(|_| anyhow!("Decisions ledger parent is unavailable"))?
            .join(name);
        ensure!(!path.is_symlink(), "Decisions ledger must not be a symlink");
        let inner =
            Arc::try_unwrap(self.0).map_err(|_| anyhow!("Execution context is already shared"))?;
        let context = Self(Arc::new(Inner {
            ledger: Some(path),
            ..inner
        }));
        let join_deadline = Instant::now() + context.remaining()?;
        loop {
            match context.access(|_| Ok(())) {
                Err(error) if would_block(&error) && Instant::now() < join_deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => {
                    result?;
                    break;
                }
            }
        }
        Ok(context)
    }

    pub fn snapshot(&self) -> Result<ExecutionSnapshot> {
        self.access(|state| {
            Ok(ExecutionSnapshot {
                scope: if self.0.ledger.is_some() {
                    "same_host_ledger"
                } else {
                    "single_invocation"
                }
                .into(),
                run_id: self.run_id().map(str::to_owned),
                plan_digest: self.plan_digest().map(str::to_owned),
                max_submissions: state.header.max_submissions,
                logical_reservations: state.reservations.len(),
                send_start_observations: state
                    .reservations
                    .iter()
                    .filter(|e| e.send_started)
                    .count(),
                submission_observations: state.submission_observations,
                active_or_quarantined_slots: state
                    .reservations
                    .iter()
                    .filter(|e| e.outcome != Outcome::Terminal)
                    .count(),
                quarantined_slots: state
                    .reservations
                    .iter()
                    .filter(|e| e.outcome == Outcome::Unknown)
                    .count(),
                terminal_submissions: state
                    .reservations
                    .iter()
                    .filter(|e| e.outcome == Outcome::Terminal)
                    .count(),
                deadline_unix_ms: state.header.deadline_unix_ms,
            })
        })
    }
    /// Read the actual explicit immutable binding, without budget or transport effects.
    pub fn run_id(&self) -> Option<&str> {
        self.0
            .header
            .deadline_unix_ms
            .is_some()
            .then_some(self.0.header.run_id.as_str())
    }
    pub fn plan_digest(&self) -> Option<&str> {
        self.0
            .header
            .deadline_unix_ms
            .is_some()
            .then_some(self.0.header.plan_digest.as_str())
    }

    /// The ledger's original monotonic anchor, for binding independent role
    /// pools to the same run. Reading it never creates an anchor or admission.
    /// An invocation without a shared ledger has no portable run anchor.
    pub fn ledger_clock_anchor(&self) -> Result<Option<RunClockAnchor>> {
        Ok(self
            .0
            .clock_anchor
            .lock()
            .map_err(|_| anyhow!("Decisions run clock unavailable"))?
            .clone())
    }

    pub(crate) fn compatibility() -> Self {
        Self::create(
            Header {
                schema: "gptgrep.decisions-budget.v1".into(),
                run_id: "single_invocation".into(),
                plan_digest: "unbound_legacy_context".into(),
                deadline_unix_ms: None,
                max_submissions: MAX_SUBMISSIONS,
                max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
            },
            None,
        )
        .expect("compatibility context is valid")
    }
    fn create(header: Header, deadline: Option<Instant>) -> Result<Self> {
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                header: header.clone(),
                submission_observations: 0,
                reservations: Vec::new(),
            }),
            header,
            deadline,
            ledger: None,
            clock_anchor: Mutex::new(None),
        })))
    }
    pub(crate) fn remaining(&self) -> Result<Duration> {
        let wall = match self.0.header.deadline_unix_ms {
            Some(deadline) => SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_millis(deadline))
                .and_then(|d| d.duration_since(SystemTime::now()).ok())
                .ok_or_else(|| anyhow!("Decisions absolute deadline has expired"))?,
            None => crate::REQUEST_TIMEOUT,
        };
        let monotonic = self
            .0
            .deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(crate::REQUEST_TIMEOUT);
        let shared = self
            .0
            .clock_anchor
            .lock()
            .map_err(|_| anyhow!("Decisions run clock unavailable"))?
            .as_ref()
            .map(RunClockAnchor::remaining)
            .transpose()?
            .unwrap_or(crate::REQUEST_TIMEOUT);
        let remaining = wall.min(monotonic).min(shared).min(crate::REQUEST_TIMEOUT);
        ensure!(
            !remaining.is_zero(),
            "Decisions absolute deadline has expired"
        );
        Ok(remaining)
    }
    pub(crate) async fn observe_submission(&self) -> Result<()> {
        self.wait_access(|s| {
            s.submission_observations = s.submission_observations.saturating_add(1);
            Ok(())
        })
        .await
    }
    async fn wait_access<T>(&self, mut action: impl FnMut(&mut State) -> Result<T>) -> Result<T> {
        loop {
            match self.access(|state| action(state)) {
                Err(error) if would_block(&error) => {
                    self.remaining()?;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                result => return result,
            }
        }
    }
    pub(crate) async fn reserve(
        &self,
        body_sha256: &str,
        requested_model: &str,
    ) -> Result<Reservation> {
        loop {
            self.remaining()?;
            let result = self.access(|state| {
                self.remaining()?;
                ensure!(
                    state.reservations.len() < state.header.max_submissions as usize,
                    "Decisions submission budget exhausted"
                );
                if state
                    .reservations
                    .iter()
                    .filter(|e| e.outcome != Outcome::Terminal)
                    .count()
                    >= state.header.max_concurrent_requests
                {
                    return Ok(None);
                }
                let id = state.reservations.len() as u32 + 1;
                state.reservations.push(Entry {
                    id,
                    body_sha256: body_sha256.into(),
                    requested_model: requested_model.into(),
                    send_started: false,
                    outcome: Outcome::Reserved,
                });
                Ok(Some(id))
            });
            match result {
                Ok(Some(id)) => {
                    return Ok(Reservation {
                        context: self.clone(),
                        id,
                        terminal: false,
                    });
                }
                Ok(None) => {}
                Err(e) if would_block(&e) => {}
                Err(e) => return Err(e),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    fn access<T>(&self, action: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut local = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow!("Decisions budget state unavailable"))?;
        let Some(path) = &self.0.ledger else {
            return action(&mut local);
        };
        let lock_path = sidecar(path, ".lock");
        ensure!(
            !lock_path.is_symlink(),
            "Decisions ledger lock must not be a symlink"
        );
        let (mut lock, created_lock_inode) = open_lock(&lock_path)?;
        // The journal update never holds a lock across HTTP or an await. Bound local
        // contention too: a foreign process cannot block this runtime indefinitely.
        let lock_deadline = Instant::now()
            + self
                .remaining()
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
        loop {
            match FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < lock_deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(anyhow!(LedgerBusy));
                }
                Err(error) => return Err(error.into()),
            }
        }
        // Bind initialization to the stable lock inode before creating the journal.
        // A crash or loss of the journal must never look like an unstarted run.
        let initialized_header_under_this_lock;
        let initialization = if lock.metadata()?.len() > 0 {
            initialized_header_under_this_lock = false;
            let mut bytes = Vec::new();
            (&lock).take(4097).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 4096,
                "Invalid Decisions ledger initialization header"
            );
            let initialization: Initialization = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow!("Invalid Decisions ledger initialization header"))?;
            ensure!(
                initialization.header == self.0.header,
                "Decisions ledger initialization run/plan/deadline/limits mismatch"
            );
            initialization
        } else {
            ensure!(
                created_lock_inode,
                "Invalid Decisions ledger initialization header"
            );
            initialized_header_under_this_lock = true;
            ensure!(
                !path.exists(),
                "Decisions journal has no bound initialization header"
            );
            let deadline =
                UNIX_EPOCH
                    + Duration::from_millis(self.0.header.deadline_unix_ms.ok_or_else(|| {
                        anyhow!("A ledger requires an explicit absolute deadline")
                    })?);
            let initialization = Initialization {
                header: self.0.header.clone(),
                clock_anchor: RunClockAnchor::capture(deadline)?,
            };
            let bytes = serde_json::to_vec(&initialization)?;
            ensure!(
                bytes.len() <= 4096,
                "Invalid Decisions ledger initialization header"
            );
            lock.write_all(&bytes)?;
            lock.sync_all()?;
            File::open(path.parent().expect("canonical ledger parent"))?.sync_all()?;
            initialization
        };
        initialization.clock_anchor.verify_deadline(
            UNIX_EPOCH
                + Duration::from_millis(
                    self.0
                        .header
                        .deadline_unix_ms
                        .expect("explicit ledger deadline"),
                ),
        )?;
        {
            let mut adopted = self
                .0
                .clock_anchor
                .lock()
                .map_err(|_| anyhow!("Decisions run clock unavailable"))?;
            if let Some(existing) = &*adopted {
                ensure!(
                    *existing == initialization.clock_anchor,
                    "Decisions ledger clock anchor changed"
                );
            } else {
                *adopted = Some(initialization.clock_anchor);
            }
        }
        let existed = path.exists();
        let mut state = if existed {
            ensure!(!path.is_symlink(), "Decisions ledger must not be a symlink");
            let mut bytes = Vec::new();
            File::open(path)?
                .take(MAX_LEDGER_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_LEDGER_BYTES,
                "Decisions ledger exceeded its byte limit"
            );
            let state: State =
                serde_json::from_slice(&bytes).map_err(|_| anyhow!("Invalid Decisions ledger"))?;
            ensure!(
                state.header == self.0.header,
                "Decisions ledger run/plan/deadline/limits mismatch"
            );
            ensure!(
                state.reservations.len() <= state.header.max_submissions as usize
                    && state
                        .reservations
                        .iter()
                        .enumerate()
                        .all(|(i, e)| e.id as usize == i + 1)
                    && state
                        .reservations
                        .iter()
                        .all(|e| valid_digest(&e.body_sha256)
                            && !e.requested_model.is_empty()
                            && e.requested_model.len() <= 256
                            && !e.requested_model.chars().any(char::is_control)
                            && (e.send_started
                                || matches!(e.outcome, Outcome::Reserved | Outcome::Unknown)))
                    && state
                        .reservations
                        .iter()
                        .filter(|e| e.outcome != Outcome::Terminal)
                        .count()
                        <= MAX_CONCURRENT_REQUESTS,
                "Invalid Decisions ledger reservations"
            );
            state
        } else {
            ensure!(
                initialized_header_under_this_lock,
                "Decisions ledger is missing beside an existing lock; budget state cannot be reset"
            );
            local.clone()
        };
        let original = state.clone();
        let result = action(&mut state)?;
        if existed && state == original {
            *local = state;
            return Ok(result);
        }
        let bytes = serde_json::to_vec(&state)
            .map_err(|_| anyhow!("Could not serialize Decisions ledger"))?;
        ensure!(
            bytes.len() as u64 <= MAX_LEDGER_BYTES,
            "Decisions ledger exceeded its byte limit"
        );
        // Owned create_new staging in the destination directory cannot alias
        // another selected ledger such as budget.pending or truncate its state.
        let parent = path.parent().expect("canonical ledger parent");
        let mut temporary = tempfile::Builder::new()
            .prefix(".gptgrep-ledger-")
            .tempfile_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        File::open(path.parent().expect("canonical ledger parent"))?.sync_all()?;
        *local = state;
        Ok(result)
    }
}

pub(crate) struct Reservation {
    context: ExecutionContext,
    id: u32,
    terminal: bool,
}
impl Reservation {
    pub(crate) async fn send_started(&mut self) -> Result<()> {
        self.context.remaining()?;
        self.context
            .wait_access(|s| {
                let entry = &mut s.reservations[self.id as usize - 1];
                ensure!(
                    entry.outcome == Outcome::Reserved,
                    "Invalid Decisions reservation state"
                );
                entry.send_started = true;
                entry.outcome = Outcome::InFlight;
                Ok(())
            })
            .await
    }
    pub(crate) async fn terminal(&mut self) -> Result<()> {
        self.context
            .wait_access(|s| {
                s.reservations[self.id as usize - 1].outcome = Outcome::Terminal;
                Ok(())
            })
            .await?;
        self.terminal = true;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.terminal {
            // A failed write leaves Reserved/InFlight durably charged and occupying its slot.
            let _ = self.context.access(|s| {
                s.reservations[self.id as usize - 1].outcome = Outcome::Unknown;
                Ok(())
            });
        }
    }
}
fn open_lock(path: &Path) -> std::io::Result<(File, bool)> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            options.create_new(false).create(false);
            options.open(path).map(|file| (file, false))
        }
        Err(error) => Err(error),
    }
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}
fn would_block(error: &anyhow::Error) -> bool {
    error.is::<LedgerBusy>()
}
#[derive(Debug)]
struct LedgerBusy;
impl std::fmt::Display for LedgerBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Decisions ledger lock is busy")
    }
}
impl std::error::Error for LedgerBusy {}
pub(crate) fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").unwrap_or(digest).len() == 64
        && digest
            .strip_prefix("sha256:")
            .unwrap_or(digest)
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn context(deadline: SystemTime, cap: u32) -> ExecutionContext {
        ExecutionContext::new("invented-test-run", DIGEST, deadline, cap).unwrap()
    }
    fn directory() -> PathBuf {
        tempfile::Builder::new()
            .prefix("gptgrep-ledger-")
            .tempdir()
            .unwrap()
            .keep()
    }
    #[test]
    fn immutable_binding_accessors_and_snapshots_have_no_admission_effects() {
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let ctx = context(deadline, 2);
        let before = ctx.snapshot().unwrap();
        assert!(ctx.ledger_clock_anchor().unwrap().is_none());
        assert_eq!(ctx.run_id(), Some("invented-test-run"));
        assert_eq!(ctx.plan_digest(), Some(DIGEST));
        assert_eq!(before.run_id.as_deref(), ctx.run_id());
        assert_eq!(before.plan_digest.as_deref(), ctx.plan_digest());
        assert_eq!(ctx.clone().snapshot().unwrap(), before);
        let dir = directory();
        let path = dir.join("binding");
        let bound = ctx.with_ledger(&path).unwrap();
        let joined = context(deadline, 2).with_ledger(&path).unwrap();
        assert_eq!(bound.run_id(), joined.run_id());
        assert_eq!(bound.plan_digest(), joined.plan_digest());
        let original_clock = bound.ledger_clock_anchor().unwrap().unwrap();
        assert!(Some(original_clock.clone()) == joined.ledger_clock_anchor().unwrap());
        original_clock.verify_deadline(deadline).unwrap();
        let snapshot = joined.snapshot().unwrap();
        assert_eq!(snapshot.run_id.as_deref(), Some("invented-test-run"));
        assert_eq!(snapshot.plan_digest.as_deref(), Some(DIGEST));
        assert_eq!(snapshot.logical_reservations, 0);
        assert_eq!(snapshot.submission_observations, 0);
        let legacy = ExecutionContext::compatibility();
        assert!(legacy.run_id().is_none());
        assert!(legacy.plan_digest().is_none());
        assert!(legacy.snapshot().unwrap().run_id.is_none());
        assert!(legacy.snapshot().unwrap().plan_digest.is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn charges_failures_never_frees_counts_and_quarantines_drop() {
        let ctx = context(SystemTime::now() + Duration::from_secs(5), 2);
        let mut first = ctx.reserve(DIGEST, "invented-model").await.unwrap();
        first.send_started().await.unwrap();
        first.terminal().await.unwrap();
        let second = ctx.reserve(DIGEST, "invented-model").await.unwrap();
        drop(second);
        let snapshot = ctx.snapshot().unwrap();
        assert_eq!(snapshot.logical_reservations, 2);
        assert_eq!(snapshot.send_start_observations, 1);
        assert_eq!(snapshot.active_or_quarantined_slots, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
        assert!(ctx.reserve(DIGEST, "invented-model").await.is_err());
    }
    #[test]
    fn ledger_header_is_immutable_and_contains_no_bodies() {
        let dir = directory();
        let path = dir.join("budget.json");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let ctx = context(deadline, 2).with_ledger(&path).unwrap();
        assert_eq!(ctx.snapshot().unwrap().scope, "same_host_ledger");
        assert!(context(deadline, 3).with_ledger(&path).is_err());
        assert!(
            context(deadline + Duration::from_secs(1), 2)
                .with_ledger(&path)
                .is_err()
        );
        assert!(
            ExecutionContext::new("other-run", DIGEST, deadline, 2)
                .unwrap()
                .with_ledger(&path)
                .is_err()
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("authorization"));
        assert!(!text.contains("questions"));
        assert!(!text.contains("state"));
        fs::remove_file(&path).unwrap();
        assert!(
            ctx.snapshot()
                .unwrap_err()
                .to_string()
                .contains("cannot be reset")
        );
        assert!(context(deadline, 2).with_ledger(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn ledger_filename_lock_and_pending_suffixes_do_not_alias_sidecars() {
        let dir = directory();
        let deadline = SystemTime::now() + Duration::from_secs(5);
        for name in ["budget.lock", "budget.pending", "same.json", "same.txt"] {
            let path = dir.join(name);
            let ctx = context(deadline, 2).with_ledger(&path).unwrap();
            let mut reservation = ctx.reserve(DIGEST, "invented-model").await.unwrap();
            reservation.send_started().await.unwrap();
            reservation.terminal().await.unwrap();
            assert_eq!(ctx.snapshot().unwrap().logical_reservations, 1);
            let journal: State = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(journal.reservations.len(), 1);
            let initialization: Initialization =
                serde_json::from_slice(&fs::read(sidecar(&path, ".lock")).unwrap()).unwrap();
            assert!(initialization.header == journal.header);
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn distinct_selected_ledgers_cannot_be_truncated_by_staging() {
        let dir = directory();
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let first_path = dir.join("budget.pending");
        let first = context(deadline, 2).with_ledger(&first_path).unwrap();
        let held = first.reserve(DIGEST, "invented-model").await.unwrap();
        let before = fs::read(&first_path).unwrap();
        let second = context(deadline, 2)
            .with_ledger(dir.join("budget"))
            .unwrap();
        let mut other = second.reserve(DIGEST, "invented-model").await.unwrap();
        other.send_started().await.unwrap();
        other.terminal().await.unwrap();
        assert_eq!(fs::read(&first_path).unwrap(), before);
        assert_eq!(first.snapshot().unwrap().active_or_quarantined_slots, 1);
        assert_eq!(second.snapshot().unwrap().terminal_submissions, 1);
        drop(held);
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn attachment_rejects_rebinding_and_prior_use_after_clones_drop() {
        let dir = directory();
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let bound = context(deadline, 2).with_ledger(dir.join("first")).unwrap();
        assert!(bound.with_ledger(dir.join("second")).is_err());
        assert!(!dir.join("second").exists());
        let used = context(deadline, 2);
        let clone = used.clone();
        let mut reservation = clone.reserve(DIGEST, "invented-model").await.unwrap();
        reservation.send_started().await.unwrap();
        reservation.terminal().await.unwrap();
        drop(reservation);
        drop(clone);
        assert!(used.with_ledger(dir.join("charged")).is_err());
        assert!(!dir.join("charged").exists());
        let observed = context(deadline, 2);
        let clone = observed.clone();
        clone.observe_submission().await.unwrap();
        drop(clone);
        assert!(observed.with_ledger(dir.join("observed")).is_err());
        assert!(!dir.join("observed").exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn only_the_acquired_lock_initializer_can_create_a_missing_journal() {
        let dir = directory();
        let path = dir.join("budget");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        // Atomic inode creation provenance cannot be inherited by a contender.
        // A retained empty inode is malformed, even when its journal is absent.
        let (created, ownership) = open_lock(&sidecar(&path, ".lock")).unwrap();
        assert!(ownership);
        let (_, contender_ownership) = open_lock(&sidecar(&path, ".lock")).unwrap();
        assert!(!contender_ownership);
        assert!(context(deadline, 2).with_ledger(&path).is_err());
        assert_eq!(created.metadata().unwrap().len(), 0);
        assert!(!path.exists());
        fs::remove_file(sidecar(&path, ".lock")).unwrap();
        let ctx = context(deadline, 2).with_ledger(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(context(deadline, 2).with_ledger(&path).is_err());
        assert!(ctx.snapshot().is_err());
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn monotonic_deadline_backstop_and_expired_no_admission() {
        let ctx = context(SystemTime::now() + Duration::from_millis(30), 2);
        let mut divergent_wall = context(SystemTime::now() + Duration::from_secs(3600), 2);
        // Emulate wall-clock rollback without changing the machine clock: wall time
        // permits another hour, but the captured monotonic lifetime has expired.
        Arc::get_mut(&mut divergent_wall.0).unwrap().deadline =
            Some(Instant::now() + Duration::from_millis(30));
        tokio::time::sleep(Duration::from_millis(45)).await;
        assert!(ctx.reserve(DIGEST, "invented-model").await.is_err());
        assert!(
            divergent_wall
                .reserve(DIGEST, "invented-model")
                .await
                .is_err()
        );
        assert_eq!(divergent_wall.snapshot().unwrap().logical_reservations, 0);
        assert_eq!(ctx.snapshot().unwrap().logical_reservations, 0);
        assert!(
            ExecutionContext::new(
                "test",
                DIGEST,
                SystemTime::now() - Duration::from_secs(1),
                2
            )
            .is_err()
        );
        assert!(
            ExecutionContext::new(
                "test",
                DIGEST,
                SystemTime::now() + Duration::from_secs(1),
                201
            )
            .is_err()
        );
    }
    // Spawn the real test binary to exercise OS locks and process-local state separation.
    #[test]
    fn fresh_process_adopts_expired_original_anchor_despite_future_utc() {
        let dir = directory();
        let path = dir.join("rollback-fixture");
        let deadline = SystemTime::now() + Duration::from_secs(3600);
        let ctx = context(deadline, 6).with_ledger(&path).unwrap();
        let deadline_ms = ctx.0.header.deadline_unix_ms.unwrap();
        let lock_path = sidecar(&path, ".lock");
        let mut initialization: serde_json::Value =
            serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
        // Controlled clock-divergence fixture: the original continuous lifetime
        // is nearly exhausted while UTC still appears to allow another hour.
        // This changes no machine clock and is not provider/runtime qualification.
        let original =
            RunClockAnchor::capture(SystemTime::now() + Duration::from_millis(50)).unwrap();
        let mut original = serde_json::to_value(original).unwrap();
        original["deadline_unix_ms"] = deadline_ms.into();
        initialization["clock_anchor"] = original;
        let fixture = serde_json::to_vec(&initialization).unwrap();
        fs::write(&lock_path, &fixture).unwrap();
        std::thread::sleep(Duration::from_millis(70));
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "execution::tests::ledger_child", "--nocapture"])
            .env("GPTGREP_LEDGER_TEST_PATH", &path)
            .env("GPTGREP_LEDGER_TEST_DEADLINE", deadline_ms.to_string())
            .env("GPTGREP_LEDGER_TEST_MODE", "join_rollback")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&lock_path).unwrap(), fixture);
        let state: State = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(state.reservations.is_empty());
        assert_eq!(state.submission_observations, 0);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cross_process_aggregate_slots_and_submission_cap_and_hard_crash() {
        let dir = directory();
        let path = dir.join("budget.json");
        let deadline = SystemTime::now() + Duration::from_secs(15);
        let ctx = context(deadline, 6).with_ledger(&path).unwrap();
        let deadline_ms = ctx.0.header.deadline_unix_ms.unwrap().to_string();
        let spawn = |mode: &str| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "execution::tests::ledger_child", "--nocapture"])
                .env("GPTGREP_LEDGER_TEST_PATH", &path)
                .env("GPTGREP_LEDGER_TEST_DEADLINE", &deadline_ms)
                .env("GPTGREP_LEDGER_TEST_MODE", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut children: Vec<_> = (0..8).map(|_| spawn("terminal")).collect();
        let mut max_slots = 0;
        while children.iter_mut().any(|c| c.try_wait().unwrap().is_none()) {
            if let Ok(snapshot) = ctx.snapshot() {
                max_slots = max_slots.max(snapshot.active_or_quarantined_slots);
                assert!(snapshot.active_or_quarantined_slots <= 4);
                assert!(snapshot.logical_reservations <= 6);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "child stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let snapshot = ctx.snapshot().unwrap();
        assert_eq!(snapshot.logical_reservations, 6);
        assert_eq!(snapshot.terminal_submissions, 6);
        assert!(max_slots > 1);
        // A separate frozen run is needed: counts cannot be reset in the first ledger.
        let crash_path = dir.join("crash.json");
        let crash_ctx = context(deadline, 6).with_ledger(&crash_path).unwrap();
        for _ in 0..4 {
            let crash = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "execution::tests::ledger_child"])
                .env("GPTGREP_LEDGER_TEST_PATH", &crash_path)
                .env("GPTGREP_LEDGER_TEST_DEADLINE", &deadline_ms)
                .env("GPTGREP_LEDGER_TEST_MODE", "crash")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            assert!(crash.wait_with_output().unwrap().status.success());
        }
        let joined = context(deadline, 6).with_ledger(&crash_path).unwrap();
        assert_eq!(joined.snapshot().unwrap().active_or_quarantined_slots, 4);
        assert_eq!(crash_ctx.snapshot().unwrap().logical_reservations, 4);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(40),
                    joined.reserve(DIGEST, "invented-model")
                )
                .await
                .is_err()
            );
        });
        assert_eq!(joined.snapshot().unwrap().logical_reservations, 4);
        // The fresh child has no in-memory ledger state. Neither loss nor corruption
        // of the journal can erase the reservations retained by the initialization lock.
        for mode in ["join_lost", "join_malformed"] {
            if mode == "join_lost" {
                fs::remove_file(&crash_path).unwrap();
            } else {
                fs::write(&crash_path, b"{").unwrap();
            }
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "execution::tests::ledger_child", "--nocapture"])
                .env("GPTGREP_LEDGER_TEST_PATH", &crash_path)
                .env("GPTGREP_LEDGER_TEST_DEADLINE", &deadline_ms)
                .env("GPTGREP_LEDGER_TEST_MODE", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn ledger_child() {
        let Ok(path) = std::env::var("GPTGREP_LEDGER_TEST_PATH") else {
            return;
        };
        let deadline = UNIX_EPOCH
            + Duration::from_millis(
                std::env::var("GPTGREP_LEDGER_TEST_DEADLINE")
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        let mode = std::env::var("GPTGREP_LEDGER_TEST_MODE").unwrap();
        let candidate = context(deadline, 6).with_ledger(path);
        if mode == "join_lost" || mode == "join_malformed" {
            let error = candidate
                .err()
                .expect("a fresh process must reject a lost or malformed existing journal");
            assert!(error.to_string().contains(if mode == "join_lost" {
                "cannot be reset"
            } else {
                "Invalid Decisions ledger"
            }));
            return;
        }
        let ctx = candidate.unwrap();
        if mode == "join_rollback" {
            assert!(ctx.remaining().is_err());
            assert!(ctx.reserve(DIGEST, "invented-model").await.is_err());
            assert_eq!(ctx.snapshot().unwrap().logical_reservations, 0);
            return;
        }
        match ctx.reserve(DIGEST, "invented-model").await {
            Ok(mut reservation) => {
                reservation.send_started().await.unwrap();
                if mode == "crash" {
                    // Process termination bypasses Drop: the durable InFlight slot must remain.
                    std::process::exit(0);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                reservation.terminal().await.unwrap();
            }
            Err(error) => assert!(error.to_string().contains("budget exhausted")),
        }
    }
}
