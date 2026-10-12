//! Bounded admission shared by clones; optional same-host durable reservations.
use crate::RunClockAnchor;
use anyhow::{Result, anyhow, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const MAX_CONCURRENT_REQUESTS: usize = 4;
pub const MAX_SUBMISSIONS: u32 = 200;
/// Schema of the portable control file that joins an existing ledger.
pub const DECISIONS_BUDGET_CONTRACT_SCHEMA: &str = "gptgrep.decisions-budget-contract.v1";
/// Schema of the read-only ledger status observation.
pub const DECISIONS_BUDGET_STATUS_SCHEMA: &str = "gptgrep.decisions-budget-status.v1";
const LEDGER_SCHEMA: &str = "gptgrep.decisions-budget.v1";
const MAX_LEDGER_BYTES: u64 = 128 * 1024;
const MAX_CONTROL_BYTES: u64 = 16 * 1024;
const MAX_INITIALIZATION_BYTES: u64 = 4096;

#[derive(Clone)]
pub struct ExecutionContext(Arc<Inner>, Option<Instant>);
struct Inner {
    header: Header,
    deadline: Option<Instant>,
    ledger: Option<PathBuf>,
    ledger_existing_only: bool,
    /// Create-only attachment: the first access must create the lock inode and
    /// write its initialization header itself. Cleared after attachment.
    ledger_create_new_pending: AtomicBool,
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

/// Portable control file for joining an already initialized shared Decisions budget.
/// The ledger retains its original run clock, deadline and consumed admissions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionsBudget {
    pub schema_version: String,
    pub run_id: String,
    pub plan_digest: String,
    pub deadline_unix_ms: u64,
    pub max_submissions: u32,
    pub ledger_path: PathBuf,
}

/// Charged reservations by their durable outcome. Every reservation consumes
/// the submission allowance permanently, including failures and unknowns.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReservationOutcomeCounts {
    pub total: usize,
    pub reserved: usize,
    pub in_flight: usize,
    pub unknown: usize,
    pub terminal: usize,
}

/// Read-only observation of an initialized ledger through its control file.
/// Reading it never creates, locks, joins, writes or renews the ledger. Counts
/// are local admission observations, not physical provider requests or a bill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionsBudgetStatus {
    pub schema_version: String,
    pub run_id: String,
    pub plan_digest: String,
    pub ledger_path: PathBuf,
    pub max_submissions: u32,
    pub max_concurrent_requests: usize,
    pub reservations: ReservationOutcomeCounts,
    pub send_started: usize,
    pub submission_observations: u64,
    pub remaining_submissions: u32,
    /// Non-terminal reservations, including quarantined unknown outcomes.
    pub occupied_slots: usize,
    /// Charged reservations per requested selector (never the served model).
    pub requested_models: BTreeMap<String, usize>,
    pub deadline_unix_ms: u64,
    /// `open`, `expired`, or `clock_binding_unavailable` (other boot or clock).
    pub deadline_state: String,
    pub wall_remaining_ms: u64,
    /// Remaining original-anchor lifetime; null when its boot/clock binding changed.
    pub anchor_remaining_ms: Option<u64>,
    /// A further reservation could be admitted now (deadline, cap and slots).
    pub admission_open: bool,
    pub read_only: bool,
}

impl DecisionsBudget {
    /// Read and validate a bounded portable control file without opening its ledger.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Refuse nonregular inputs without waiting for a FIFO writer.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path.as_ref())
            .map_err(|_| anyhow!("Decisions budget file unavailable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| anyhow!("Decisions budget file unavailable"))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_CONTROL_BYTES,
            "Decisions budget file exceeds the control byte limit"
        );
        let mut bytes = Vec::new();
        file.take(MAX_CONTROL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("Decisions budget file unavailable"))?;
        ensure!(
            bytes.len() as u64 <= MAX_CONTROL_BYTES,
            "Decisions budget file exceeds the control byte limit"
        );
        let value = crate::validation::unique_json(&bytes)
            .map_err(|_| anyhow!("Invalid Decisions budget contract"))?;
        let budget: Self = serde_json::from_value(value)
            .map_err(|_| anyhow!("Invalid Decisions budget contract"))?;
        ensure!(
            budget.schema_version == DECISIONS_BUDGET_CONTRACT_SCHEMA,
            "Unsupported Decisions budget contract"
        );
        ensure!(
            budget.ledger_path.is_absolute(),
            "Decisions ledger path must be absolute"
        );
        Ok(budget)
    }

    /// Join the existing matching ledger. Never initializes, resets or renews it.
    pub fn join(&self) -> Result<ExecutionContext> {
        ExecutionContext::new(
            &self.run_id,
            &self.plan_digest,
            self.deadline()?,
            self.max_submissions,
        )?
        .with_existing_ledger(&self.ledger_path)
    }

    fn deadline(&self) -> Result<SystemTime> {
        UNIX_EPOCH
            .checked_add(Duration::from_millis(self.deadline_unix_ms))
            .ok_or_else(|| anyhow!("Invalid Decisions deadline"))
    }

    /// The exact immutable ledger header this control binds, without requiring
    /// an unexpired deadline (status remains readable after expiry).
    fn header(&self) -> Result<Header> {
        validate_binding(&self.run_id, &self.plan_digest, self.max_submissions)?;
        self.deadline()?;
        Ok(Header {
            schema: LEDGER_SCHEMA.into(),
            run_id: self.run_id.clone(),
            plan_digest: self.plan_digest.clone(),
            deadline_unix_ms: Some(self.deadline_unix_ms),
            max_submissions: self.max_submissions,
            max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
        })
    }

    /// Create a NEW same-host ledger and publish its portable control file.
    ///
    /// Both paths must be absolute. The ledger, its `.lock` sidecar and the control
    /// file must not exist: initialization never joins, resets or renews an existing
    /// ledger, and never replaces a control file. The ledger is created through
    /// `ExecutionContext::new(..).with_new_ledger(..)`, the create-only form of the
    /// `with_ledger` initializer, so the original clock anchor is captured once.
    /// The control file is written completely with mode 0600 and published without
    /// replacement. If publication fails after the ledger exists, the ledger is
    /// retained unchanged and the error says so; choose a new ledger path.
    pub fn initialize(
        run_id: &str,
        plan_digest: &str,
        deadline_unix_ms: u64,
        max_submissions: u32,
        ledger: &Path,
        control: &Path,
    ) -> Result<Self> {
        ensure!(
            ledger.is_absolute(),
            "Decisions ledger path must be absolute"
        );
        ensure!(
            control.is_absolute(),
            "Decisions budget control path must be absolute"
        );
        let canonical = |path: &Path, unavailable: &'static str| -> Result<PathBuf> {
            let name = path.file_name().ok_or_else(|| anyhow!(unavailable))?;
            let parent = path.parent().ok_or_else(|| anyhow!(unavailable))?;
            let parent = fs::canonicalize(parent).map_err(|_| anyhow!(unavailable))?;
            ensure!(parent.is_dir(), unavailable);
            Ok(parent.join(name))
        };
        let ledger = canonical(ledger, "Decisions ledger parent is unavailable")?;
        let control = canonical(control, "Decisions budget control parent is unavailable")?;
        let lock = sidecar(&ledger, ".lock");
        ensure!(
            control != ledger && control != lock,
            "Decisions budget control path must differ from its ledger files"
        );
        for path in [&ledger, &lock, &control] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => {
                    return Err(anyhow!(
                        "Decisions budget initialization refuses an existing ledger, lock or control path; it never joins, resets or renews"
                    ));
                }
            }
        }
        let deadline = UNIX_EPOCH
            .checked_add(Duration::from_millis(deadline_unix_ms))
            .ok_or_else(|| anyhow!("Invalid Decisions deadline"))?;
        let context = ExecutionContext::new(run_id, plan_digest, deadline, max_submissions)?
            .with_new_ledger(&ledger)?;
        let snapshot = context.snapshot()?;
        ensure!(
            snapshot.logical_reservations == 0
                && snapshot.submission_observations == 0
                && snapshot.deadline_unix_ms == Some(deadline_unix_ms),
            "New Decisions ledger is not in its initial state"
        );
        let budget = Self {
            schema_version: DECISIONS_BUDGET_CONTRACT_SCHEMA.into(),
            run_id: run_id.into(),
            plan_digest: plan_digest.into(),
            deadline_unix_ms,
            max_submissions,
            ledger_path: context
                .ledger_path()
                .ok_or_else(|| anyhow!("Decisions ledger path unavailable"))?
                .to_path_buf(),
        };
        drop(context);
        let mut bytes = serde_json::to_vec_pretty(&budget)
            .map_err(|_| anyhow!("Could not serialize Decisions budget contract"))?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_CONTROL_BYTES,
            "Decisions budget file exceeds the control byte limit"
        );
        let retained = "Decisions budget control file could not be published; the new ledger is retained unchanged and was not reset";
        let parent = control.parent().expect("canonical control parent");
        let mut staged = tempfile::Builder::new()
            .prefix(".gptgrep-budget-control-")
            .tempfile_in(parent)
            .map_err(|_| anyhow!(retained))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            staged
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| anyhow!(retained))?;
        }
        staged.write_all(&bytes).map_err(|_| anyhow!(retained))?;
        staged.as_file().sync_all().map_err(|_| anyhow!(retained))?;
        staged
            .persist_noclobber(&control)
            .map_err(|_| anyhow!(retained))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| anyhow!(retained))?;
        ensure!(Self::read(&control)? == budget, retained);
        Ok(budget)
    }

    /// Read-only ledger status: verifies the control/lock/journal binding and
    /// reports reservations by outcome, remaining allowance and the deadline.
    /// No lock is taken and nothing is created or written; the journal is only
    /// ever replaced atomically, so one read observes one complete version.
    pub fn status(&self) -> Result<DecisionsBudgetStatus> {
        let header = self.header()?;
        let lock = read_ledger_file(
            &sidecar(&self.ledger_path, ".lock"),
            MAX_INITIALIZATION_BYTES,
            "Existing Decisions ledger lock is unavailable",
        )?;
        let initialization: Initialization = serde_json::from_slice(&lock)
            .map_err(|_| anyhow!("Invalid Decisions ledger initialization header"))?;
        ensure!(
            initialization.header == header,
            "Decisions ledger initialization run/plan/deadline/limits mismatch"
        );
        let journal = read_ledger_file(
            &self.ledger_path,
            MAX_LEDGER_BYTES,
            "Existing Decisions ledger is unavailable",
        )?;
        let state: State =
            serde_json::from_slice(&journal).map_err(|_| anyhow!("Invalid Decisions ledger"))?;
        ensure!(
            state.header == header,
            "Decisions ledger run/plan/deadline/limits mismatch"
        );
        validate_reservations(&state)?;
        let mut reservations = ReservationOutcomeCounts {
            total: state.reservations.len(),
            ..ReservationOutcomeCounts::default()
        };
        let mut requested_models = BTreeMap::new();
        for entry in &state.reservations {
            match entry.outcome {
                Outcome::Reserved => reservations.reserved += 1,
                Outcome::InFlight => reservations.in_flight += 1,
                Outcome::Unknown => reservations.unknown += 1,
                Outcome::Terminal => reservations.terminal += 1,
            }
            *requested_models
                .entry(entry.requested_model.clone())
                .or_insert(0usize) += 1;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let wall_remaining_ms = self.deadline_unix_ms.saturating_sub(now_ms);
        let anchor_remaining_ms = initialization
            .clock_anchor
            .verify_deadline(self.deadline()?)
            .ok()
            .map(|()| {
                initialization.clock_anchor.remaining().map_or(0, |left| {
                    u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
                })
            });
        let deadline_state = match anchor_remaining_ms {
            None => "clock_binding_unavailable",
            Some(anchor) if anchor == 0 || wall_remaining_ms == 0 => "expired",
            Some(_) => "open",
        };
        let occupied_slots = reservations.total - reservations.terminal;
        let remaining_submissions = self
            .max_submissions
            .saturating_sub(u32::try_from(reservations.total).unwrap_or(u32::MAX));
        Ok(DecisionsBudgetStatus {
            schema_version: DECISIONS_BUDGET_STATUS_SCHEMA.into(),
            run_id: self.run_id.clone(),
            plan_digest: self.plan_digest.clone(),
            ledger_path: self.ledger_path.clone(),
            max_submissions: self.max_submissions,
            max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
            send_started: state.reservations.iter().filter(|e| e.send_started).count(),
            submission_observations: state.submission_observations,
            remaining_submissions,
            occupied_slots,
            requested_models,
            deadline_unix_ms: self.deadline_unix_ms,
            deadline_state: deadline_state.into(),
            wall_remaining_ms,
            anchor_remaining_ms,
            admission_open: deadline_state == "open"
                && remaining_submissions > 0
                && occupied_slots < MAX_CONCURRENT_REQUESTS,
            read_only: true,
            reservations,
        })
    }
}

/// Bounded read of one existing regular ledger file, refusing symlinks and FIFOs.
fn read_ledger_file(path: &Path, limit: u64, unavailable: &'static str) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| anyhow!(unavailable))?;
    ensure!(
        file.metadata().map_err(|_| anyhow!(unavailable))?.is_file(),
        unavailable
    );
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!(unavailable))?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Decisions ledger exceeded its byte limit"
    );
    Ok(bytes)
}

fn validate_binding(run_id: &str, plan_digest: &str, max_submissions: u32) -> Result<()> {
    ensure!(
        !run_id.is_empty() && run_id.len() <= 256 && !run_id.chars().any(char::is_control),
        "Invalid Decisions run identifier"
    );
    ensure!(valid_digest(plan_digest), "Invalid Decisions plan digest");
    ensure!(
        (1..=MAX_SUBMISSIONS).contains(&max_submissions),
        "Decisions submission limit must be 1 to 200"
    );
    Ok(())
}

fn validate_reservations(state: &State) -> Result<()> {
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
    Ok(())
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("shared_ledger", &self.0.ledger.is_some())
            .field("max_submissions", &self.0.header.max_submissions)
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    /// Read a bounded portable budget contract and join its existing ledger.
    /// No missing journal, lock, or clock anchor is initialized by this front door.
    pub fn from_budget_file(path: impl AsRef<Path>) -> Result<Self> {
        DecisionsBudget::read(path)?.join()
    }

    /// An explicit experiment context. The absolute deadline also has a monotonic backstop.
    pub fn new(
        run_id: &str,
        plan_digest: &str,
        absolute_deadline: SystemTime,
        max_submissions: u32,
    ) -> Result<Self> {
        validate_binding(run_id, plan_digest, max_submissions)?;
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
                schema: LEDGER_SCHEMA.into(),
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
        self.attach_ledger(path.as_ref(), false, false)
    }

    /// Join an already initialized same-host ledger, adopting its original anchor.
    /// Missing journal or lock files are errors, including during later accesses.
    /// This mode never creates a lock, journal, or run clock anchor.
    pub fn with_existing_ledger(self, path: impl AsRef<Path>) -> Result<Self> {
        self.attach_ledger(path.as_ref(), true, false)
    }

    /// Create-only form of [`Self::with_ledger`]: the attaching access must create
    /// the lock inode and write the immutable initialization header itself. An
    /// existing lock or journal, including one created concurrently, is refused;
    /// this never joins, resets or renews another ledger.
    pub fn with_new_ledger(self, path: impl AsRef<Path>) -> Result<Self> {
        self.attach_ledger(path.as_ref(), false, true)
    }

    /// The canonical attached ledger journal path, if a ledger is attached.
    pub fn ledger_path(&self) -> Option<&Path> {
        self.0.ledger.as_deref()
    }

    fn attach_ledger(self, path: &Path, existing_only: bool, create_new: bool) -> Result<Self> {
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
        if create_new {
            for existing in [&path, &sidecar(&path, ".lock")] {
                ensure!(
                    matches!(fs::symlink_metadata(existing), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                    "Decisions ledger already exists; initialization never joins, resets or renews it"
                );
            }
        }
        let inner =
            Arc::try_unwrap(self.0).map_err(|_| anyhow!("Execution context is already shared"))?;
        let context = Self(
            Arc::new(Inner {
                ledger: Some(path),
                ledger_existing_only: existing_only,
                ledger_create_new_pending: AtomicBool::new(create_new),
                ..inner
            }),
            self.1,
        );
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
        context
            .0
            .ledger_create_new_pending
            .store(false, Ordering::SeqCst);
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

    /// Check the original run deadline without admitting work or touching the ledger.
    pub fn check_deadline(&self) -> Result<()> {
        self.remaining().map(|_| ())
    }

    /// Remaining lifetime of the same finite original run, without the HTTP
    /// request timeout cap. No anchor, ledger attachment or admission is created.
    /// Legacy invocation-only contexts have no finite run deadline and reject.
    pub fn original_run_remaining(&self) -> Result<Duration> {
        let deadline = self
            .0
            .header
            .deadline_unix_ms
            .ok_or_else(|| anyhow!("Decisions original finite deadline unavailable"))?;
        let wall = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_millis(deadline))
            .and_then(|d| d.duration_since(SystemTime::now()).ok())
            .ok_or_else(|| anyhow!("Decisions absolute deadline has expired"))?;
        let monotonic = self
            .0
            .deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
            .ok_or_else(|| anyhow!("Decisions original finite deadline unavailable"))?;
        let shared = self
            .0
            .clock_anchor
            .lock()
            .map_err(|_| anyhow!("Decisions run clock unavailable"))?
            .as_ref()
            .map(RunClockAnchor::remaining)
            .transpose()?;
        let remaining = shared.map_or(wall.min(monotonic), |d| wall.min(monotonic).min(d));
        ensure!(
            !remaining.is_zero(),
            "Decisions absolute deadline has expired"
        );
        Ok(remaining)
    }

    pub(crate) fn compatibility() -> Self {
        Self::create(
            Header {
                schema: LEDGER_SCHEMA.into(),
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
        Ok(Self(
            Arc::new(Inner {
                state: Mutex::new(State {
                    header: header.clone(),
                    submission_observations: 0,
                    reservations: Vec::new(),
                }),
                header,
                deadline,
                ledger: None,
                ledger_existing_only: false,
                ledger_create_new_pending: AtomicBool::new(false),
                clock_anchor: Mutex::new(None),
            }),
            None,
        ))
    }
    /// An immutable stricter admission/HTTP deadline sharing the original run.
    /// Attach a ledger before creating/sharing views. This reads the existing
    /// clocks but never captures a clock, joins a ledger, writes its header, or
    /// replaces the shared reservations/counters. Clones retain the local cap;
    /// applying a later cap cannot extend an earlier one. The original-run
    /// accessor deliberately continues to report only the original lifetime.
    pub fn with_deadline(&self, deadline: Instant) -> Result<Self> {
        self.original_run_remaining()?;
        let deadline = deadline
            .min(self.0.deadline.expect("finite original context"))
            .min(self.1.unwrap_or(deadline));
        let view = Self(Arc::clone(&self.0), Some(deadline));
        view.remaining()?;
        Ok(view)
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
        let local = self
            .1
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(crate::REQUEST_TIMEOUT);
        let remaining = wall
            .min(monotonic)
            .min(shared)
            .min(local)
            .min(crate::REQUEST_TIMEOUT);
        ensure!(
            !remaining.is_zero(),
            "Decisions absolute deadline has expired"
        );
        Ok(remaining)
    }
    pub(crate) async fn observe_submission(&self) -> Result<()> {
        self.remaining()?;
        self.wait_access(|s| {
            self.remaining()?;
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
        let (mut lock, created_lock_inode) = if self.0.ledger_existing_only {
            ensure!(path.is_file(), "Existing Decisions ledger is unavailable");
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let file = options
                .open(&lock_path)
                .map_err(|_| anyhow!("Existing Decisions ledger lock is unavailable"))?;
            ensure!(
                file.metadata()?.is_file(),
                "Existing Decisions ledger lock is invalid"
            );
            (file, false)
        } else {
            open_lock(&lock_path)?
        };
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
        ensure!(
            initialized_header_under_this_lock
                || !self.0.ledger_create_new_pending.load(Ordering::SeqCst),
            "Decisions ledger already exists; initialization never joins, resets or renews it"
        );
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
            validate_reservations(&state)?;
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
                self.context.remaining()?;
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
    #[cfg(unix)]
    #[test]
    fn budget_file_rejects_fifo_without_blocking() {
        const CHILD_PATH: &str = "GPTGREP_BUDGET_FIFO_TEST_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let error = ExecutionContext::from_budget_file(PathBuf::from(path)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Decisions budget file exceeds the control byte limit"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // The owned FIFO is used only by the bounded test child, with no writer.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "execution::tests::budget_file_rejects_fifo_without_blocking",
            ])
            .env_clear()
            .env(CHILD_PATH, &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO refusal child failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("budget FIFO acquisition blocked before regular-file refusal");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[tokio::test]
    async fn portable_budget_file_joins_original_clock_and_charged_quota_without_initialization() {
        let dir = directory();
        let ledger = dir.join("budget.json");
        let control = dir.join("control.json");
        let deadline_ms = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            + 10_000;
        let deadline = UNIX_EPOCH + Duration::from_millis(deadline_ms);
        let budget = DecisionsBudget {
            schema_version: "gptgrep.decisions-budget-contract.v1".into(),
            run_id: "invented-test-run".into(),
            plan_digest: DIGEST.into(),
            deadline_unix_ms: deadline_ms,
            max_submissions: 2,
            ledger_path: ledger.clone(),
        };
        fs::write(&control, serde_json::to_vec(&budget).unwrap()).unwrap();
        assert!(ExecutionContext::from_budget_file(&control).is_err());
        assert!(!ledger.exists());
        assert!(!sidecar(&ledger, ".lock").exists());
        let original = context(deadline, 2).with_ledger(&ledger).unwrap();
        let mut first = original
            .reserve(DIGEST, crate::DEFAULT_MODEL)
            .await
            .unwrap();
        first.send_started().await.unwrap();
        first.terminal().await.unwrap();
        let joined = ExecutionContext::from_budget_file(&control).unwrap();
        assert!(joined.ledger_clock_anchor().unwrap() == original.ledger_clock_anchor().unwrap());
        assert_eq!(joined.snapshot().unwrap(), original.snapshot().unwrap());
        let held = joined
            .clone()
            .reserve(DIGEST, crate::DEFAULT_MODEL)
            .await
            .unwrap();
        drop(held);
        assert!(
            original
                .reserve(DIGEST, crate::DEFAULT_MODEL)
                .await
                .is_err()
        );
        let before = fs::read(&ledger).unwrap();
        let mut mismatch = budget.clone();
        mismatch.deadline_unix_ms += 1;
        fs::write(&control, serde_json::to_vec(&mismatch).unwrap()).unwrap();
        assert!(ExecutionContext::from_budget_file(&control).is_err());
        assert_eq!(fs::read(&ledger).unwrap(), before);
        fs::write(
            &control,
            br#"{"schema_version":"one","schema_version":"two"}"#,
        )
        .unwrap();
        assert!(ExecutionContext::from_budget_file(&control).is_err());
        fs::write(&control, vec![b' '; 16 * 1024 + 1]).unwrap();
        assert!(ExecutionContext::from_budget_file(&control).is_err());
        fs::remove_dir_all(dir).unwrap();
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
    async fn original_remaining_is_uncapped_readonly_and_never_resets_expiry() {
        let deadline = SystemTime::now() + Duration::from_secs(90);
        let ctx = context(deadline, 2);
        let snapshot = ctx.snapshot().unwrap();
        assert!(ctx.original_run_remaining().unwrap() > Duration::from_secs(60));
        assert!(ctx.remaining().unwrap() <= crate::REQUEST_TIMEOUT);
        assert_eq!(ctx.snapshot().unwrap(), snapshot);
        assert!(
            ExecutionContext::compatibility()
                .original_run_remaining()
                .is_err()
        );
        let dir = directory();
        let path = dir.join("original-remaining");
        let bound = ctx.with_ledger(&path).unwrap();
        let joined = context(deadline, 2).with_ledger(&path).unwrap();
        let anchor = bound.ledger_clock_anchor().unwrap();
        let journal = fs::read(&path).unwrap();
        let header = fs::read(sidecar(&path, ".lock")).unwrap();
        for _ in 0..3 {
            assert!(joined.original_run_remaining().unwrap() > Duration::from_secs(60));
        }
        assert!(anchor == joined.ledger_clock_anchor().unwrap());
        assert_eq!(fs::read(&path).unwrap(), journal);
        assert_eq!(fs::read(sidecar(&path, ".lock")).unwrap(), header);
        let mut expired = context(SystemTime::now() + Duration::from_secs(90), 2);
        Arc::get_mut(&mut expired.0).unwrap().deadline =
            Some(Instant::now() + Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(expired.original_run_remaining().is_err());
        assert!(expired.clone().original_run_remaining().is_err());
        assert_eq!(expired.snapshot().unwrap().logical_reservations, 0);
        fs::remove_dir_all(dir).unwrap();
    }
    async fn assert_stricter_view(persistent: bool) {
        assert!(
            ExecutionContext::compatibility()
                .with_deadline(Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        let dir = directory();
        let path = dir.join("deadline-view");
        let original = context(SystemTime::now() + Duration::from_secs(60), 3);
        let original = if persistent {
            original.with_ledger(&path).unwrap()
        } else {
            original
        };
        let before = original.snapshot().unwrap();
        let anchor = original.ledger_clock_anchor().unwrap();
        let journal = persistent.then(|| fs::read(&path).unwrap());
        let header = persistent.then(|| fs::read(sidecar(&path, ".lock")).unwrap());
        let cap = Instant::now() + Duration::from_millis(500);
        let view = original.with_deadline(cap).unwrap();
        let later = view.with_deadline(cap + Duration::from_secs(30)).unwrap();
        assert!(Arc::ptr_eq(&original.0, &view.0));
        assert!(Arc::ptr_eq(&original.0, &later.0));
        assert!(view.1 == Some(cap) && later.1 == Some(cap));
        assert_eq!(view.snapshot().unwrap(), before);
        assert!(view.ledger_clock_anchor().unwrap() == anchor);
        assert!(view.original_run_remaining().unwrap() > Duration::from_secs(50));
        if persistent {
            assert_eq!(fs::read(&path).unwrap(), journal.unwrap());
            assert_eq!(fs::read(sidecar(&path, ".lock")).unwrap(), header.unwrap());
        }
        view.observe_submission().await.unwrap();
        let mut completed = view.reserve(DIGEST, "synthetic-model").await.unwrap();
        completed.send_started().await.unwrap();
        let mut unsent = view.reserve(DIGEST, "synthetic-model").await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(view.check_deadline().is_err());
        assert!(view.clone().check_deadline().is_err());
        assert!(later.check_deadline().is_err());
        assert!(
            view.with_deadline(Instant::now() + Duration::from_secs(30))
                .is_err()
        );
        assert!(view.observe_submission().await.is_err());
        assert!(view.reserve(DIGEST, "synthetic-model").await.is_err());
        assert!(unsent.send_started().await.is_err());
        // Cleanup retains the original journal and can record its outcome
        // after the local cap expires; no slot or charged count is refunded.
        completed.terminal().await.unwrap();
        drop(unsent);
        let snapshot = original.snapshot().unwrap();
        assert_eq!(snapshot.submission_observations, 1);
        assert_eq!(snapshot.logical_reservations, 2);
        assert_eq!(snapshot.send_start_observations, 1);
        assert_eq!(snapshot.terminal_submissions, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
        assert_eq!(snapshot.active_or_quarantined_slots, 1);
        assert_eq!(view.snapshot().unwrap(), snapshot);
        assert!(original.ledger_clock_anchor().unwrap() == anchor);
        assert!(original.check_deadline().is_ok());
        assert!(original.original_run_remaining().unwrap() > Duration::from_secs(50));
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn stricter_local_view_never_extends_deadline_or_refunds() {
        assert_stricter_view(false).await;
    }
    #[tokio::test]
    async fn stricter_shared_view_preserves_original_anchor_and_accounting() {
        assert_stricter_view(true).await;
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
    fn existing_only_join_never_initializes_missing_files_or_recaptures_anchor() {
        let dir = directory();
        let path = dir.join("existing-only");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        assert!(context(deadline, 2).with_existing_ledger(&path).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let original = context(deadline, 2).with_ledger(&path).unwrap();
        let joined = context(deadline, 2).with_existing_ledger(&path).unwrap();
        assert_eq!(original.snapshot().unwrap(), joined.snapshot().unwrap());
        assert!(original.ledger_clock_anchor().unwrap() == joined.ledger_clock_anchor().unwrap());
        let lock = sidecar(&path, ".lock");
        fs::remove_file(&lock).unwrap();
        assert!(joined.snapshot().is_err());
        assert!(!lock.exists());
        fs::remove_file(&path).unwrap();
        assert!(joined.snapshot().is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(context(deadline, 2).with_existing_ledger(&path).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn existing_only_join_retains_charged_unknown_and_shared_quota() {
        let dir = directory();
        let path = dir.join("charged-existing");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let original = context(deadline, 1).with_ledger(&path).unwrap();
        let mut reservation = original.reserve(DIGEST, "ordinary-fixture").await.unwrap();
        reservation.send_started().await.unwrap();
        drop(reservation);
        let joined = context(deadline, 1).with_existing_ledger(&path).unwrap();
        assert_eq!(original.snapshot().unwrap(), joined.snapshot().unwrap());
        assert_eq!(joined.snapshot().unwrap().quarantined_slots, 1);
        assert!(joined.reserve(DIGEST, "ordinary-fixture").await.is_err());
        assert_eq!(joined.snapshot().unwrap().logical_reservations, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    fn deadline_ms(after: Duration) -> u64 {
        (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + after).as_millis() as u64
    }

    #[test]
    fn create_only_attachment_refuses_existing_or_concurrently_initialized_ledgers() {
        let dir = directory();
        let path = dir.join("create-only");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        let created = context(deadline, 2).with_new_ledger(&path).unwrap();
        assert_eq!(created.ledger_path(), Some(path.as_path()));
        assert_eq!(created.snapshot().unwrap().logical_reservations, 0);
        let journal = fs::read(&path).unwrap();
        let header = fs::read(sidecar(&path, ".lock")).unwrap();
        // Even an identical header is not joined by the create-only form.
        let error = context(deadline, 2).with_new_ledger(&path).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(&path).unwrap(), journal);
        assert_eq!(fs::read(sidecar(&path, ".lock")).unwrap(), header);
        // The ordinary initializer still joins the matching ledger.
        assert!(context(deadline, 2).with_ledger(&path).is_ok());
        // A lock created by another initializer after the pre-check is refused
        // by the access that did not create and initialize that inode.
        let racing = dir.join("racing");
        let other = context(deadline, 2).with_ledger(&racing).unwrap();
        let fresh = context(deadline, 2);
        let inner = Arc::try_unwrap(fresh.0).ok().unwrap();
        let raced = ExecutionContext(
            Arc::new(Inner {
                ledger: Some(racing.clone()),
                ledger_existing_only: false,
                ledger_create_new_pending: AtomicBool::new(true),
                ..inner
            }),
            None,
        );
        assert!(
            raced
                .access(|_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(other.snapshot().unwrap().logical_reservations, 0);
        // An orphan journal or lock is never adopted or reset by create-only attachment.
        let orphan = dir.join("orphan");
        fs::write(&orphan, b"{}").unwrap();
        assert!(context(deadline, 2).with_new_ledger(&orphan).is_err());
        assert!(!sidecar(&orphan, ".lock").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn initialize_publishes_private_control_once_and_never_resets_or_renews() {
        let dir = directory();
        let ledger = dir.join("budget.json");
        let control = dir.join("control.json");
        let deadline_unix_ms = deadline_ms(Duration::from_secs(30));
        let budget = DecisionsBudget::initialize(
            "invented-test-run",
            DIGEST,
            deadline_unix_ms,
            2,
            &ledger,
            &control,
        )
        .unwrap();
        assert_eq!(budget.schema_version, DECISIONS_BUDGET_CONTRACT_SCHEMA);
        assert_eq!(budget.ledger_path, fs::canonicalize(&ledger).unwrap());
        assert_eq!(DecisionsBudget::read(&control).unwrap(), budget);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&control, &ledger, &sidecar(&ledger, ".lock")] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        let joined = ExecutionContext::from_budget_file(&control).unwrap();
        let mut first = joined
            .reserve(DIGEST, "~typesafe/jev-latest")
            .await
            .unwrap();
        first.send_started().await.unwrap();
        first.terminal().await.unwrap();
        let control_bytes = fs::read(&control).unwrap();
        let journal = fs::read(&ledger).unwrap();
        let header = fs::read(sidecar(&ledger, ".lock")).unwrap();
        // A second init, identical or renewed, is refused before any write.
        for (deadline, max, other_control) in [
            (deadline_unix_ms, 2, control.clone()),
            (deadline_ms(Duration::from_secs(60)), 2, control.clone()),
            (
                deadline_ms(Duration::from_secs(60)),
                3,
                dir.join("renewed.json"),
            ),
        ] {
            let error = DecisionsBudget::initialize(
                "invented-test-run",
                DIGEST,
                deadline,
                max,
                &ledger,
                &other_control,
            )
            .unwrap_err();
            assert!(error.to_string().contains("never joins, resets or renews"));
        }
        assert!(!dir.join("renewed.json").exists());
        assert_eq!(fs::read(&control).unwrap(), control_bytes);
        assert_eq!(fs::read(&ledger).unwrap(), journal);
        assert_eq!(fs::read(sidecar(&ledger, ".lock")).unwrap(), header);
        // An existing control file is never replaced, even for a new ledger.
        let fresh_ledger = dir.join("fresh.json");
        assert!(
            DecisionsBudget::initialize(
                "invented-test-run",
                DIGEST,
                deadline_unix_ms,
                2,
                &fresh_ledger,
                &control,
            )
            .is_err()
        );
        assert!(!fresh_ledger.exists());
        assert_eq!(fs::read(&control).unwrap(), control_bytes);
        let status = budget.status().unwrap();
        assert_eq!(status.reservations.total, 1);
        assert_eq!(status.reservations.terminal, 1);
        assert_eq!(status.remaining_submissions, 1);
        assert_eq!(status.requested_models["~typesafe/jev-latest"], 1);
        for (ledger, control) in [
            (Path::new("relative.json"), dir.join("c1.json")),
            (
                dir.join("l1.json").as_path(),
                PathBuf::from("relative-control.json"),
            ),
        ] {
            assert!(
                DecisionsBudget::initialize("r", DIGEST, deadline_unix_ms, 2, ledger, &control)
                    .is_err()
            );
        }
        assert!(
            DecisionsBudget::initialize(
                "r",
                DIGEST,
                deadline_unix_ms,
                2,
                &dir.join("missing-parent/ledger.json"),
                &dir.join("c2.json"),
            )
            .is_err()
        );
        assert!(
            DecisionsBudget::initialize(
                "r",
                DIGEST,
                deadline_unix_ms,
                2,
                &dir.join("same"),
                &dir.join("same")
            )
            .is_err()
        );
        assert!(!dir.join("same").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn status_is_read_only_counts_outcomes_and_reports_expiry() {
        let dir = directory();
        let ledger = dir.join("status.json");
        let control = dir.join("status-control.json");
        let budget = DecisionsBudget::initialize(
            "invented-status-run",
            DIGEST,
            deadline_ms(Duration::from_millis(3000)),
            3,
            &ledger,
            &control,
        )
        .unwrap();
        let joined = budget.join().unwrap();
        let mut done = joined.reserve(DIGEST, "typesafe/jev-1.13").await.unwrap();
        done.send_started().await.unwrap();
        done.terminal().await.unwrap();
        let mut unknown = joined.reserve(DIGEST, "typesafe/jev-1.13").await.unwrap();
        unknown.send_started().await.unwrap();
        drop(unknown);
        let reserved = joined.reserve(DIGEST, crate::DEFAULT_MODEL).await.unwrap();
        let paths = [ledger.clone(), sidecar(&ledger, ".lock"), control.clone()];
        let before: Vec<_> = paths
            .iter()
            .map(|p| {
                (
                    fs::read(p).unwrap(),
                    fs::metadata(p).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let status = budget.status().unwrap();
        assert_eq!(status.schema_version, DECISIONS_BUDGET_STATUS_SCHEMA);
        assert!(status.read_only);
        assert_eq!(
            status.reservations,
            ReservationOutcomeCounts {
                total: 3,
                reserved: 1,
                in_flight: 0,
                unknown: 1,
                terminal: 1
            }
        );
        assert_eq!(status.send_started, 2);
        assert_eq!(status.remaining_submissions, 0);
        assert_eq!(status.occupied_slots, 2);
        assert_eq!(status.requested_models["typesafe/jev-1.13"], 2);
        assert_eq!(status.deadline_state, "open");
        assert!(!status.admission_open);
        assert!(status.anchor_remaining_ms.unwrap() > 0);
        let after: Vec<_> = paths
            .iter()
            .map(|p| {
                (
                    fs::read(p).unwrap(),
                    fs::metadata(p).unwrap().modified().unwrap(),
                )
            })
            .collect();
        assert_eq!(before, after);
        drop(reserved);
        tokio::time::sleep(Duration::from_millis(status.wall_remaining_ms + 150)).await;
        // Status remains readable after expiry; joining is refused.
        let expired = DecisionsBudget::read(&control).unwrap().status().unwrap();
        assert_eq!(expired.deadline_state, "expired");
        assert_eq!(expired.wall_remaining_ms, 0);
        assert!(!expired.admission_open);
        assert_eq!(expired.reservations.unknown, 2);
        assert!(ExecutionContext::from_budget_file(&control).is_err());
        // Mismatched controls and tampered journals are rejected, not repaired.
        let mut mismatch = budget.clone();
        mismatch.max_submissions = 4;
        assert!(mismatch.status().is_err());
        let journal = fs::read(&ledger).unwrap();
        fs::write(&ledger, b"{").unwrap();
        assert!(budget.status().is_err());
        fs::write(&ledger, &journal).unwrap();
        fs::remove_file(sidecar(&ledger, ".lock")).unwrap();
        assert!(budget.status().is_err());
        assert!(!sidecar(&ledger, ".lock").exists());
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
                .expect_err("a fresh process must reject a lost or malformed existing journal");
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
