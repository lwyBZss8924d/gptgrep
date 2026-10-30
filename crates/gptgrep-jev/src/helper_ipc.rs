//! Explicit Linux Unix transport for a keyless Decisions client.
//!
//! The foreground helper alone owns the provider credential and the original
//! durable budget. This is a typed product transport, not an HTTP proxy.
use crate::{
    ClientBackend, DecisionResponse, ExecutionContext, ExecutionSnapshot, JevClient,
    MAX_CONCURRENT_REQUESTS, MAX_MULTIMODAL_REQUEST_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    PreparedBodyMode, PreparedDecision, RetryNotice, RunClockAnchor, TransientRetry,
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub const CONTROL_ENV: &str = "GPTGREP_DECISIONS_IPC_CONTROL";
pub const CONTROL_SHA_ENV: &str = "GPTGREP_DECISIONS_IPC_SHA256";
pub const CLIENT_SCHEMA: &str = "gptgrep.decisions-helper-client.v1";
pub const SERVER_SCHEMA: &str = "gptgrep.decisions-helper-server.v1";
pub const METADATA_MAX: usize = 8 * 1024;
pub const RESPONSE_WIRE_MAX: usize = 10 * MAX_RESPONSE_BYTES + 64 * 1024;
const CONTROL_MAX: usize = 64 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeIdentity {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub pid_namespace: u64,
    pub cgroup: String,
    pub boot_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeerScope {
    Container {
        init: ProcessIdentity,
    },
    /// Provider-disabled process fixtures only. Never a live host UID grant.
    EngineeringDescendants {
        ancestor: ProcessIdentity,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicBudget {
    pub run_id: String,
    pub plan_digest: String,
    pub deadline_unix_ms: u64,
    pub max_submissions: u32,
    pub clock_anchor: RunClockAnchor,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientControl {
    pub schema_version: String,
    pub instance: String,
    pub task_binding_sha256: String,
    pub socket_parent: PathBuf,
    pub parent_identity: NodeIdentity,
    pub socket_identity: NodeIdentity,
    pub server: ProcessIdentity,
    pub server_uid: u32,
    pub server_gid: u32,
    pub model: String,
    pub provider_disabled: bool,
    pub budget: PublicBudget,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerControl {
    pub schema_version: String,
    pub task_binding_sha256: String,
    pub socket_parent: PathBuf,
    pub parent_identity: NodeIdentity,
    pub client_visible_parent: PathBuf,
    pub client_control_path: PathBuf,
    pub budget: FilePin,
    pub owner_binding: FilePin,
    pub peer: PeerScope,
    pub model: String,
    pub provider_disabled: bool,
}

pub fn environment_selected() -> bool {
    std::env::var_os(CONTROL_ENV).is_some() || std::env::var_os(CONTROL_SHA_ENV).is_some()
}

pub fn read_client_control(pin: &FilePin) -> Result<ClientControl> {
    let control: ClientControl = platform::read_pin(pin, CONTROL_MAX)?;
    control.validate()?;
    Ok(control)
}

fn environment_control() -> Result<ClientControl> {
    ensure!(
        cfg!(target_os = "linux"),
        "Decisions helper IPC requires Linux"
    );
    ensure!(
        std::env::var_os("OPENROUTER_API_KEY").is_none(),
        "Helper clients must not receive a provider credential"
    );
    let path = std::env::var(CONTROL_ENV)
        .map_err(|_| anyhow!("Helper control and digest are required"))?;
    let digest = std::env::var(CONTROL_SHA_ENV)
        .map_err(|_| anyhow!("Helper control and digest are required"))?;
    read_client_control(&FilePin {
        path: path.into(),
        sha256: digest,
    })
}

pub(crate) fn execution_from_budget(budget: &crate::DecisionsBudget) -> Result<ExecutionContext> {
    let control = environment_control()?;
    ensure!(
        budget.run_id == control.budget.run_id
            && budget.plan_digest == control.budget.plan_digest
            && budget.max_submissions == control.budget.max_submissions
            && budget.deadline_unix_ms == control.budget.deadline_unix_ms,
        "Caller budget differs from the fixed helper budget"
    );
    Ok(ExecutionContext::helper_view(
        RemoteContext::new(control),
        None,
    ))
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

#[cfg(target_os = "linux")]
fn unique<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    crate::parse_unique_json(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| anyhow!("Invalid closed helper record"))
}

impl ClientControl {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == CLIENT_SCHEMA
                && hex(&self.instance, 32)
                && hex(&self.task_binding_sha256, 64)
                && absolute(&self.socket_parent)
                && self.parent_identity.uid == self.server_uid
                && self.socket_identity.uid == self.server_uid
                && self.parent_identity.gid == self.server_gid
                && self.socket_identity.gid == self.server_gid
                && self.parent_identity.mode == 0o700
                && self.socket_identity.mode & 0o077 == 0,
            "Invalid helper endpoint control"
        );
        crate::models::validate_requested_model(&self.model)?;
        ensure!(
            !self.budget.run_id.is_empty()
                && self.budget.run_id.len() <= 256
                && hex(&self.budget.plan_digest, 64)
                && (1..=crate::MAX_SUBMISSIONS).contains(&self.budget.max_submissions),
            "Invalid helper budget binding"
        );
        self.budget.clock_anchor.remaining()?;
        Ok(())
    }
}

pub(crate) struct RemoteContext {
    pub(crate) control: Arc<ClientControl>,
}

impl RemoteContext {
    fn new(control: ClientControl) -> Arc<Self> {
        Arc::new(Self {
            control: Arc::new(control),
        })
    }

    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        matches!((serde_json::to_vec(self.control.as_ref()),
            serde_json::to_vec(other.control.as_ref())), (Ok(left), Ok(right)) if left == right)
    }

    fn observe(
        &self,
        mut snapshot: ExecutionSnapshot,
        request: &str,
        event: u64,
    ) -> Result<ExecutionSnapshot> {
        let budget = &self.control.budget;
        ensure!(
            snapshot.run_id.as_deref() == Some(budget.run_id.as_str())
                && snapshot.plan_digest.as_deref() == Some(budget.plan_digest.as_str())
                && snapshot.max_submissions == budget.max_submissions
                && snapshot.deadline_unix_ms == Some(budget.deadline_unix_ms)
                && snapshot.logical_reservations <= budget.max_submissions as usize
                && snapshot.send_start_observations <= snapshot.logical_reservations
                && snapshot.terminal_submissions <= snapshot.logical_reservations
                && snapshot.quarantined_slots <= snapshot.active_or_quarantined_slots
                && snapshot.active_or_quarantined_slots <= MAX_CONCURRENT_REQUESTS,
            "Invalid helper budget observation"
        );
        snapshot.scope = format!("helper_server_event:{request}:{event}");
        Ok(snapshot)
    }

    pub(crate) fn snapshot(&self) -> Result<ExecutionSnapshot> {
        let budget = &self.control.budget;
        if let Some(snapshot) = callback_snapshot(
            Some(&budget.run_id),
            Some(&budget.plan_digest),
            Some(budget.deadline_unix_ms),
            budget.max_submissions,
            Some(&self.control.instance),
        ) {
            return Ok(snapshot);
        }
        let snapshot = platform::status(self.control.as_ref())?;
        self.observe(snapshot, "status", 0)
    }
}

pub(crate) struct HelperClient {
    remote: Arc<RemoteContext>,
}

impl HelperClient {
    pub(crate) fn from_environment(model: Option<&str>) -> Result<Self> {
        let control = environment_control()?;
        ensure!(
            model.unwrap_or(crate::DEFAULT_MODEL) == control.model,
            "Helper model differs from its frozen control"
        );
        Ok(Self {
            remote: RemoteContext::new(control),
        })
    }

    pub(crate) fn into_client(self) -> Result<JevClient> {
        self.remote.control.validate()?;
        let execution = ExecutionContext::helper_view(Arc::clone(&self.remote), None);
        Ok(JevClient {
            endpoint: reqwest::Url::parse(crate::DEFAULT_ENDPOINT)?,
            model: self.remote.control.model.clone(),
            backend: ClientBackend::Helper(Arc::new(self)),
            identity: Arc::new(()),
            execution,
            observer: None,
            retry: None,
        })
    }

    pub(crate) fn delegated_view(&self, context: ExecutionContext) -> ExecutionContext {
        ExecutionContext::helper_view(Arc::clone(&self.remote), Some(&context))
    }

    pub(crate) async fn submit(
        &self,
        client: &JevClient,
        prepared: PreparedDecision,
        retry: Option<&TransientRetry>,
        notice: Option<&RetryNotice<'_>>,
    ) -> Result<DecisionResponse> {
        self.remote.control.validate()?;
        client.execution.remaining()?;
        let limit = match prepared.mode {
            PreparedBodyMode::Ordinary => MAX_REQUEST_BYTES,
            PreparedBodyMode::TypedInput => MAX_MULTIMODAL_REQUEST_BYTES,
        };
        ensure!(
            prepared.body.len() <= limit,
            "Helper prepared body exceeds its original codec bound"
        );
        platform::submit(&self.remote, client, prepared, retry.is_some(), notice).await
    }
}

pub(crate) fn callback_snapshot(
    run_id: Option<&str>,
    plan_digest: Option<&str>,
    deadline: Option<u64>,
    max_submissions: u32,
    instance: Option<&str>,
) -> Option<ExecutionSnapshot> {
    platform::callback_snapshot(run_id, plan_digest, deadline, max_submissions, instance)
}

/// Read one pinned nonsecret private control without credential discovery.
pub fn read_server_control(pin: &FilePin) -> Result<ServerControl> {
    let control: ServerControl = platform::read_pin(pin, CONTROL_MAX)?;
    control.validate()?;
    Ok(control)
}

impl ServerControl {
    fn validate(&self) -> Result<()> {
        let control = self;
        ensure!(
            control.schema_version == SERVER_SCHEMA
                && hex(&control.task_binding_sha256, 64)
                && absolute(&control.socket_parent)
                && absolute(&control.client_visible_parent)
                && control.client_control_path.parent() == Some(control.socket_parent.as_path())
                && control
                    .client_control_path
                    .file_name()
                    .is_some_and(|v| v == "client.json")
                && control.parent_identity.mode == 0o700,
            "Invalid helper server control"
        );
        crate::models::validate_requested_model(&control.model)?;
        ensure!(
            control.provider_disabled || matches!(control.peer, PeerScope::Container { .. }),
            "Live helper requires an admitted model-container generation"
        );
        Ok(())
    }
}

/// Provider-disabled endpoint probe. It neither reserves nor submits a decision.
pub async fn probe(control: &ClientControl) -> Result<ExecutionSnapshot> {
    control.validate()?;
    ensure!(
        std::env::var_os("OPENROUTER_API_KEY").is_none(),
        "Helper probe must be keyless"
    );
    ensure!(
        control.provider_disabled,
        "Engineering probe requires a provider-disabled helper"
    );
    platform::probe(control).await
}

/// Harden the keyholder before reading its stdin credential; never applied to a client.
pub fn harden_keyholder() -> Result<()> {
    platform::harden()
}

/// Validate the active private owner and original budget before bounded stdin input.
pub fn key_input_remaining(control: &ServerControl) -> Result<std::time::Duration> {
    control.validate()?;
    platform::key_input_remaining(control)
}

/// Screen a keyholder's own metadata before its first output write.
pub fn keyholder_metadata_safe(key: Option<&str>, value: &Value) -> bool {
    platform::metadata_safe(key, value)
}

/// The caller owns the already checked foreground process and its shutdown signal.
pub async fn serve(
    control: ServerControl,
    key: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<Value> {
    platform::serve(control, key, shutdown, |_| Ok(())).await
}

/// The readiness callback receives only already screened endpoint metadata.
pub async fn serve_with_readiness(
    control: ServerControl,
    key: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send,
    ready: impl FnOnce(Value) -> Result<()> + Send,
) -> Result<Value> {
    platform::serve(control, key, shutdown, ready).await
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WireHeader {
        version: u32,
        instance: String,
        task_binding_sha256: String,
        request: String,
        operation: Operation,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum Operation {
        Submit {
            mode: PreparedBodyMode,
            body_sha256: String,
            body_bytes: usize,
            deadline_monotonic_ns: u64,
            retry: bool,
        },
        Status,
        Probe,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Event {
        BeforeSend {
            body_sha256: String,
            body_bytes: usize,
            question_count: usize,
        },
        Validated {
            response_bytes: usize,
            response_sha256: String,
        },
        Failed,
        BeforeRetry {
            prior: DecisionRetriedAttempt,
        },
        Complete {
            response_bytes: usize,
            response_sha256: String,
            prior_attempts: Vec<DecisionRetriedAttempt>,
        },
        Error {
            failure: crate::failure::HelperFailure,
        },
        Unavailable {
            reason: AdmissionReason,
        },
        Refused,
        Status,
        Probe,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum AdmissionReason {
        BudgetExhausted,
        Deadline,
        Unavailable,
    }
    impl AdmissionReason {
        fn from_error(error: &anyhow::Error) -> Self {
            match error.to_string().as_str() {
                "Decisions submission budget exhausted" => Self::BudgetExhausted,
                "Decisions absolute deadline has expired" | "Run absolute deadline has expired" => {
                    Self::Deadline
                }
                _ => Self::Unavailable,
            }
        }
        fn error(self) -> anyhow::Error {
            anyhow!(match self {
                Self::BudgetExhausted => "Decisions submission budget exhausted",
                Self::Deadline => "Decisions original admission deadline expired",
                Self::Unavailable =>
                    "Decisions helper admission is unavailable; no replay was attempted",
            })
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ServerFrame {
        request: String,
        sequence: u64,
        snapshot: ExecutionSnapshot,
        event: Event,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ack {
        request: String,
        sequence: u64,
        proceed: bool,
    }

    use crate::{
        DecisionObserver, DecisionRequestObservation, DecisionRetriedAttempt, DecisionsBudget,
    };
    use sha2::{Digest, Sha256};
    use std::{
        cell::RefCell,
        ffi::CString,
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
                net::{UnixListener as StdListener, UnixStream as StdStream},
            },
        },
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SOCKET_NAME: &str = "decisions.sock";
    fn transport_error() -> anyhow::Error {
        anyhow!(
            "Decisions helper channel failed or its original deadline expired; no replay was attempted"
        )
    }
    fn mono() -> Result<u64> {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } == 0,
            "Helper monotonic clock unavailable"
        );
        u64::try_from(value.tv_sec)
            .ok()
            .and_then(|v| v.checked_mul(1_000_000_000))
            .and_then(|v| {
                u64::try_from(value.tv_nsec)
                    .ok()
                    .and_then(|n| v.checked_add(n))
            })
            .ok_or_else(|| anyhow!("Invalid helper monotonic clock"))
    }
    fn left(deadline: u64) -> Result<Duration> {
        Ok(Duration::from_nanos(
            deadline
                .checked_sub(mono()?)
                .filter(|v| *v > 0)
                .ok_or_else(transport_error)?,
        ))
    }
    fn deadline_after(duration: Duration) -> Result<u64> {
        mono()?
            .checked_add(u64::try_from(duration.as_nanos())?)
            .ok_or_else(transport_error)
    }
    fn initial_deadline(control: &ClientControl) -> Result<u64> {
        let wall = UNIX_EPOCH
            .checked_add(Duration::from_millis(control.budget.deadline_unix_ms))
            .and_then(|v| v.duration_since(SystemTime::now()).ok())
            .ok_or_else(transport_error)?;
        deadline_after(
            wall.min(control.budget.clock_anchor.remaining()?)
                .min(crate::REQUEST_TIMEOUT),
        )
    }
    fn node(value: &std::fs::Metadata) -> NodeIdentity {
        NodeIdentity {
            device: value.dev(),
            inode: value.ino(),
            uid: value.uid(),
            gid: value.gid(),
            mode: value.mode() & 0o7777,
        }
    }
    fn open_directory(path: &Path) -> Result<File> {
        ensure!(
            absolute(path),
            "Helper directory must be absolute and physical"
        );
        let root = CString::new("/")?;
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "Could not open helper directory root");
        let mut file = unsafe { File::from_raw_fd(fd) };
        for part in path.components() {
            let std::path::Component::Normal(name) = part else {
                ensure!(
                    matches!(part, std::path::Component::RootDir),
                    "Unsupported helper path component"
                );
                continue;
            };
            let name = CString::new(name.as_bytes())
                .map_err(|_| anyhow!("Invalid helper directory name"))?;
            let fd = unsafe {
                libc::openat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            ensure!(fd >= 0, "Helper directory component is unavailable");
            file = unsafe { File::from_raw_fd(fd) };
        }
        Ok(file)
    }
    struct Endpoint {
        parent: PathBuf,
        directory: File,
        identity: NodeIdentity,
    }
    impl Endpoint {
        fn open(parent: &Path, expected: &NodeIdentity) -> Result<Self> {
            let directory = open_directory(parent)?;
            ensure!(
                node(&directory.metadata()?) == *expected
                    && expected.uid == unsafe { libc::getuid() }
                    && expected.mode == 0o700,
                "Helper directory custody changed"
            );
            let value = Self {
                parent: parent.to_owned(),
                directory,
                identity: expected.clone(),
            };
            value.check_parent()?;
            Ok(value)
        }
        fn check_parent(&self) -> Result<()> {
            ensure!(
                node(&open_directory(&self.parent)?.metadata()?) == self.identity
                    && node(&self.directory.metadata()?) == self.identity
                    && std::fs::read_link(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))?
                        == self.parent,
                "Helper directory identity or descriptor path changed"
            );
            ensure!(
                unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_GETFD) }
                    & libc::FD_CLOEXEC
                    != 0,
                "Helper directory descriptor must be close-on-exec"
            );
            Ok(())
        }
        fn address(&self) -> Result<PathBuf> {
            self.check_parent()?;
            let address = PathBuf::from(format!(
                "/proc/self/fd/{}/{SOCKET_NAME}",
                self.directory.as_raw_fd()
            ));
            let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
                .sun_path
                .len();
            ensure!(
                address.as_os_str().as_bytes().len() + 1 <= capacity,
                "Helper Unix address exceeds capacity"
            );
            Ok(address)
        }
        fn leaf(&self) -> Result<NodeIdentity> {
            self.check_parent()?;
            let name = CString::new(SOCKET_NAME)?;
            let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
            ensure!(
                unsafe {
                    libc::fstatat(
                        self.directory.as_raw_fd(),
                        name.as_ptr(),
                        value.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } == 0,
                "Helper socket leaf unavailable"
            );
            let value = unsafe { value.assume_init() };
            ensure!(
                value.st_mode & libc::S_IFMT == libc::S_IFSOCK && value.st_nlink == 1,
                "Helper endpoint must be one Unix socket inode"
            );
            Ok(NodeIdentity {
                device: value.st_dev,
                inode: value.st_ino,
                uid: value.st_uid,
                gid: value.st_gid,
                mode: value.st_mode & 0o7777,
            })
        }
        fn verify_leaf(&self, expected: &NodeIdentity) -> Result<()> {
            ensure!(self.leaf()? == *expected, "Helper socket leaf changed");
            Ok(())
        }
    }
    pub(super) fn read_pin<T: serde::de::DeserializeOwned>(
        pin: &FilePin,
        limit: usize,
    ) -> Result<T> {
        ensure!(
            hex(&pin.sha256, 64) && absolute(&pin.path),
            "Invalid helper control pin"
        );
        let parent = open_directory(
            pin.path
                .parent()
                .ok_or_else(|| anyhow!("Helper control needs a parent"))?,
        )?;
        let name = pin
            .path
            .file_name()
            .ok_or_else(|| anyhow!("Helper control needs a name"))?;
        ensure!(
            name != "auth.json" && !name.as_bytes().starts_with(b".env"),
            "Credential files are not helper controls"
        );
        let name =
            CString::new(name.as_bytes()).map_err(|_| anyhow!("Invalid helper control name"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "Helper control unavailable");
        let mut file = unsafe { File::from_raw_fd(fd) };
        let before = file.metadata()?;
        ensure!(
            before.is_file()
                && before.nlink() == 1
                && before.uid() == unsafe { libc::getuid() }
                && before.mode() & 0o777 == 0o600
                && before.len() <= limit as u64,
            "Helper control requires private regular-file custody"
        );
        let mut bytes = Vec::new();
        (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(
            bytes.len() <= limit
                && node(&before) == node(&after)
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
                && format!("{:x}", Sha256::digest(&bytes)) == pin.sha256,
            "Helper control pin changed"
        );
        unique(&bytes)
    }
    fn read_proc(pid: u32, name: &str, limit: usize) -> Result<String> {
        let file = File::open(format!("/proc/{pid}/{name}"))
            .map_err(|_| anyhow!("Helper peer kernel evidence unavailable"))?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "Helper peer evidence exceeds its bound"
        );
        String::from_utf8(bytes).map_err(|_| anyhow!("Invalid helper kernel evidence"))
    }
    fn process(pid: u32) -> Result<ProcessIdentity> {
        let raw = read_proc(pid, "stat", 4096)?;
        let fields: Vec<_> = raw
            .rsplit_once(") ")
            .ok_or_else(|| anyhow!("Invalid peer process stat"))?
            .1
            .split_whitespace()
            .collect();
        let start_ticks = fields
            .get(19)
            .ok_or_else(|| anyhow!("Peer process clock unavailable"))?
            .parse()?;
        let namespace = std::fs::read_link(format!("/proc/{pid}/ns/pid"))?;
        let namespace = namespace
            .to_str()
            .and_then(|v| v.strip_prefix("pid:["))
            .and_then(|v| v.strip_suffix(']'))
            .ok_or_else(|| anyhow!("Peer PID namespace unavailable"))?
            .parse()?;
        let group = read_proc(pid, "cgroup", 8192)?;
        let groups: Vec<_> = group
            .lines()
            .filter_map(|line| line.strip_prefix("0::"))
            .collect();
        ensure!(
            groups.len() == 1 && groups[0].starts_with('/'),
            "Unified peer cgroup required"
        );
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        ensure!(boot_id.len() <= 64, "Invalid helper boot identity");
        Ok(ProcessIdentity {
            pid,
            start_ticks,
            pid_namespace: namespace,
            cgroup: groups[0].to_owned(),
            boot_id,
        })
    }
    fn zero_time_namespace(pid: u32) -> Result<()> {
        let raw = read_proc(pid, "timens_offsets", 512)?;
        let mut count = 0;
        for line in raw.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            ensure!(
                fields.len() == 3
                    && ["monotonic", "boottime"].contains(&fields[0])
                    && fields[1].parse::<i64>()? == 0
                    && fields[2].parse::<i64>()? == 0,
                "Helper peers require the original kernel clock namespace"
            );
            count += 1;
        }
        ensure!(count == 2, "Helper time namespace evidence unavailable");
        Ok(())
    }
    fn pidfd(pid: u32) -> Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(fd >= 0, "Helper peer pidfd unavailable");
        Ok(unsafe { OwnedFd::from_raw_fd(i32::try_from(fd)?) })
    }
    fn alive(fd: &OwnedFd) -> Result<()> {
        let mut value = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut value, 1, 0) } == 0,
            "Helper peer generation is terminal"
        );
        Ok(())
    }
    fn credentials(fd: i32) -> Result<libc::ucred> {
        let mut value = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    value.as_mut_ptr().cast(),
                    &mut size,
                )
            } == 0
                && size as usize == std::mem::size_of::<libc::ucred>(),
            "Helper peer credentials unavailable"
        );
        Ok(unsafe { value.assume_init() })
    }
    fn check_server(fd: i32, control: &ClientControl) -> Result<()> {
        let peer = credentials(fd)?;
        ensure!(
            peer.uid == control.server_uid && peer.gid == control.server_gid,
            "Helper server peer differs from its control"
        );
        let own = process(std::process::id())?;
        if own.pid_namespace == control.server.pid_namespace {
            ensure!(
                peer.pid > 0
                    && u32::try_from(peer.pid)? == control.server.pid
                    && process_start(control.server.pid)? == control.server.start_ticks
                    && own.boot_id == control.server.boot_id,
                "Helper server generation changed"
            );
        } else {
            ensure!(peer.pid == 0, "Unqualified Unix proxy server generation");
        }
        zero_time_namespace(std::process::id())
    }
    fn process_start(pid: u32) -> Result<u64> {
        let raw = read_proc(pid, "stat", 4096)?;
        raw.rsplit_once(") ")
            .and_then(|v| v.1.split_whitespace().nth(19))
            .ok_or_else(|| anyhow!("Helper process generation unavailable"))?
            .parse()
            .map_err(|_| anyhow!("Invalid helper process generation"))
    }

    struct CallbackObservation {
        instance: String,
        snapshot: ExecutionSnapshot,
    }
    thread_local! {
        static CALLBACKS: RefCell<Vec<CallbackObservation>> = const { RefCell::new(Vec::new()) };
    }
    pub(super) fn callback_snapshot(
        run_id: Option<&str>,
        plan: Option<&str>,
        deadline: Option<u64>,
        max: u32,
        instance: Option<&str>,
    ) -> Option<ExecutionSnapshot> {
        CALLBACKS.with(|values| {
            values
                .borrow()
                .iter()
                .rev()
                .find(|value| {
                    value.snapshot.run_id.as_deref() == run_id
                        && value.snapshot.plan_digest.as_deref() == plan
                        && value.snapshot.deadline_unix_ms == deadline
                        && value.snapshot.max_submissions == max
                        && instance.is_none_or(|expected| expected == value.instance)
                })
                .map(|v| v.snapshot.clone())
        })
    }
    struct CallbackScope(usize);
    impl CallbackScope {
        fn enter(
            remote: &RemoteContext,
            snapshot: ExecutionSnapshot,
            request: &str,
            event: u64,
        ) -> Result<Self> {
            let snapshot = remote.observe(snapshot, request, event)?;
            Ok(CALLBACKS.with(|values| {
                let mut values = values.borrow_mut();
                let depth = values.len();
                values.push(CallbackObservation {
                    instance: remote.control.instance.clone(),
                    snapshot,
                });
                Self(depth)
            }))
        }
    }
    impl Drop for CallbackScope {
        fn drop(&mut self) {
            CALLBACKS.with(|values| values.borrow_mut().truncate(self.0));
        }
    }

    static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    fn header(control: &ClientControl, operation: Operation) -> WireHeader {
        let source = format!(
            "{}:{}:{}",
            control.instance,
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        );
        WireHeader {
            version: 1,
            instance: control.instance.clone(),
            task_binding_sha256: control.task_binding_sha256.clone(),
            request: format!("{:x}", Sha256::digest(source.as_bytes())),
            operation,
        }
    }
    fn validate_header(header: &WireHeader, control: &ClientControl) -> Result<()> {
        ensure!(
            header.version == 1
                && header.instance == control.instance
                && header.task_binding_sha256 == control.task_binding_sha256
                && hex(&header.request, 64),
            "Helper request belongs to another frozen task or server generation"
        );
        Ok(())
    }
    fn response_body(
        response: &DecisionResponse,
    ) -> Result<(Vec<u8>, Vec<DecisionRetriedAttempt>)> {
        let mut response = response.clone();
        let prior = std::mem::take(&mut response.retried_attempts);
        let bytes = serde_json::to_vec(&response)?;
        ensure!(
            bytes.len() <= RESPONSE_WIRE_MAX,
            "Helper typed response exceeds its proven wire bound"
        );
        Ok((bytes, prior))
    }
    fn decode_response(
        bytes: &[u8],
        questions: &Value,
        model: &str,
        prior: Vec<DecisionRetriedAttempt>,
    ) -> Result<DecisionResponse> {
        ensure!(
            bytes.len() <= RESPONSE_WIRE_MAX && prior.len() <= 1,
            "Invalid helper response bound"
        );
        let mut response =
            crate::validation::validate_response(crate::parse_unique_json(bytes)?, questions)?;
        ensure!(
            crate::response_model_matches(model, &response.model),
            "Helper response model differs"
        );
        for value in &prior {
            value.validate_helper()?;
        }
        response.retried_attempts = prior;
        Ok(response)
    }
    async fn connect_async(
        control: &ClientControl,
        deadline: u64,
    ) -> Result<(tokio::net::UnixStream, Endpoint)> {
        let endpoint = Endpoint::open(&control.socket_parent, &control.parent_identity)?;
        endpoint.verify_leaf(&control.socket_identity)?;
        let stream = tokio::time::timeout(
            left(deadline)?,
            tokio::net::UnixStream::connect(endpoint.address()?),
        )
        .await
        .map_err(|_| transport_error())?
        .map_err(|_| transport_error())?;
        check_server(stream.as_raw_fd(), control)?;
        endpoint.verify_leaf(&control.socket_identity)?;
        Ok((stream, endpoint))
    }
    fn connect_sync(control: &ClientControl, deadline: u64) -> Result<(StdStream, Endpoint)> {
        let endpoint = Endpoint::open(&control.socket_parent, &control.parent_identity)?;
        endpoint.verify_leaf(&control.socket_identity)?;
        let address = endpoint.address()?;
        let raw = address.as_os_str().as_bytes();
        let mut socket_address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        socket_address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, source) in socket_address.sun_path.iter_mut().zip(raw.iter()) {
            *target = *source as libc::c_char;
        }
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        ensure!(fd >= 0, "Could not allocate helper status socket");
        let stream = unsafe { StdStream::from_raw_fd(fd) };
        loop {
            left(deadline)?;
            let result = unsafe {
                libc::connect(
                    fd,
                    (&socket_address as *const libc::sockaddr_un).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                )
            };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EISCONN)
            {
                break;
            }
            let error = std::io::Error::last_os_error().raw_os_error();
            ensure!(
                matches!(
                    error,
                    Some(libc::EINPROGRESS | libc::EALREADY | libc::EAGAIN | libc::EINTR)
                ),
                "Helper status connect failed; no replay was attempted"
            );
            let mut wait = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = left(deadline)?
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut wait, 1, millis) };
            ensure!(
                result >= 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted,
                "Helper status connect failed"
            );
            if result > 0 {
                std::thread::sleep(left(deadline)?.min(Duration::from_millis(1)));
            }
        }
        stream.set_nonblocking(false)?;
        check_server(stream.as_raw_fd(), control)?;
        endpoint.verify_leaf(&control.socket_identity)?;
        Ok((stream, endpoint))
    }
    pub(super) fn status(control: &ClientControl) -> Result<ExecutionSnapshot> {
        let deadline = initial_deadline(control)?;
        let request = header(control, Operation::Status);
        let (mut stream, endpoint) = connect_sync(control, deadline)?;
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request)?,
            METADATA_MAX,
            deadline,
        )?;
        let frame: ServerFrame = unique(&read_frame(&mut stream, METADATA_MAX, deadline)?)?;
        ensure!(
            frame.request == request.request
                && frame.sequence == 1
                && matches!(frame.event, Event::Status),
            "Helper readonly status response differs"
        );
        endpoint.verify_leaf(&control.socket_identity)?;
        Ok(frame.snapshot)
    }
    pub(super) async fn probe(control: &ClientControl) -> Result<ExecutionSnapshot> {
        let deadline = initial_deadline(control)?;
        let request = header(control, Operation::Probe);
        let (mut stream, endpoint) = connect_async(control, deadline).await?;
        write_frame_async(
            &mut stream,
            &serde_json::to_vec(&request)?,
            METADATA_MAX,
            deadline,
        )
        .await?;
        let frame: ServerFrame =
            unique(&read_frame_async(&mut stream, METADATA_MAX, deadline).await?)?;
        ensure!(
            frame.request == request.request
                && frame.sequence == 1
                && matches!(frame.event, Event::Probe),
            "Helper engineering probe response differs"
        );
        endpoint.verify_leaf(&control.socket_identity)?;
        Ok(frame.snapshot)
    }
    async fn raw_response(
        stream: &mut tokio::net::UnixStream,
        count: usize,
        digest: &str,
        deadline: u64,
        questions: &Value,
        model: &str,
        prior: Vec<DecisionRetriedAttempt>,
    ) -> Result<DecisionResponse> {
        ensure!(
            count <= RESPONSE_WIRE_MAX && hex(digest, 64),
            "Invalid helper response descriptor"
        );
        let bytes = read_frame_async(stream, RESPONSE_WIRE_MAX, deadline).await?;
        ensure!(
            bytes.len() == count && format!("{:x}", Sha256::digest(&bytes)) == digest,
            "Helper response payload differs from its descriptor"
        );
        decode_response(&bytes, questions, model, prior)
    }
    fn invoke_callback<T>(operation: impl FnOnce() -> T) -> T {
        if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(operation)
        } else {
            operation()
        }
    }
    pub(super) async fn submit(
        remote: &RemoteContext,
        client: &JevClient,
        prepared: PreparedDecision,
        retry: bool,
        notice: Option<&RetryNotice<'_>>,
    ) -> Result<DecisionResponse> {
        let deadline = deadline_after(client.execution.remaining()?)?;
        let request = header(
            &remote.control,
            Operation::Submit {
                mode: prepared.mode,
                body_sha256: prepared.body_sha256.clone(),
                body_bytes: prepared.body.len(),
                deadline_monotonic_ns: deadline,
                retry,
            },
        );
        let (mut stream, endpoint) = connect_async(&remote.control, deadline).await?;
        write_frame_async(
            &mut stream,
            &serde_json::to_vec(&request)?,
            METADATA_MAX,
            deadline,
        )
        .await?;
        write_frame_async(
            &mut stream,
            &prepared.body,
            MAX_MULTIMODAL_REQUEST_BYTES,
            deadline,
        )
        .await?;
        let mut expected = 1;
        let mut notice_error = None;
        let result = async {
            loop {
                let frame: ServerFrame =
                    unique(&read_frame_async(&mut stream, METADATA_MAX, deadline).await?)?;
                ensure!(
                    frame.request == request.request && frame.sequence == expected && expected <= 9,
                    "Helper event sequence differs"
                );
                expected += 1;
                endpoint.verify_leaf(&remote.control.socket_identity)?;
                let observation = DecisionRequestObservation {
                    requested_model: &client.model,
                    body_sha256: &prepared.body_sha256,
                    body_bytes: prepared.body.len(),
                    question_count: prepared.questions.as_object().map_or(0, |v| v.len()),
                };
                let outcome = match frame.event {
                    Event::Complete {
                        response_bytes,
                        response_sha256,
                        prior_attempts,
                    } => {
                        let response = raw_response(
                            &mut stream,
                            response_bytes,
                            &response_sha256,
                            deadline,
                            &prepared.questions,
                            &client.model,
                            prior_attempts,
                        )
                        .await?;
                        remote.observe(frame.snapshot, &frame.request, frame.sequence)?;
                        return Ok(response);
                    }
                    Event::Error { failure } => {
                        remote.observe(frame.snapshot, &frame.request, frame.sequence)?;
                        return Err(crate::DecisionFailure::from_helper_wire(failure)?.into());
                    }
                    Event::Unavailable { reason } => {
                        remote.observe(frame.snapshot, &frame.request, frame.sequence)?;
                        return Err(reason.error());
                    }
                    Event::Refused => {
                        return Err(anyhow!("Decisions helper refused the unadmitted operation"));
                    }
                    Event::BeforeSend {
                        body_sha256,
                        body_bytes,
                        question_count,
                    } => {
                        ensure!(
                            body_sha256 == prepared.body_sha256
                                && body_bytes == prepared.body.len()
                                && question_count == observation.question_count,
                            "Helper prepared body differs"
                        );
                        invoke_callback(|| {
                            let _scope = CallbackScope::enter(
                                remote,
                                frame.snapshot,
                                &frame.request,
                                frame.sequence,
                            )?;
                            client.observer.as_ref().map_or(Ok(()), |observer| {
                                observer.send_started_with_body(&observation, &prepared.body)
                            })
                        })
                    }
                    Event::Validated {
                        response_bytes,
                        response_sha256,
                    } => {
                        let response = raw_response(
                            &mut stream,
                            response_bytes,
                            &response_sha256,
                            deadline,
                            &prepared.questions,
                            &client.model,
                            Vec::new(),
                        )
                        .await?;
                        invoke_callback(|| {
                            let _scope = CallbackScope::enter(
                                remote,
                                frame.snapshot,
                                &frame.request,
                                frame.sequence,
                            )?;
                            client.observer.as_ref().map_or(Ok(()), |observer| {
                                observer.validated_response(&observation, &response)
                            })
                        })
                    }
                    Event::Failed => invoke_callback(|| {
                        let _scope = CallbackScope::enter(
                            remote,
                            frame.snapshot,
                            &frame.request,
                            frame.sequence,
                        )?;
                        client
                            .observer
                            .as_ref()
                            .map_or(Ok(()), |observer| observer.failed(&observation))
                    }),
                    Event::BeforeRetry { prior } => {
                        prior.validate_helper()?;
                        let result = invoke_callback(|| {
                            let _scope = CallbackScope::enter(
                                remote,
                                frame.snapshot,
                                &frame.request,
                                frame.sequence,
                            )?;
                            notice.map_or(Ok(()), |notice| notice(&prior))
                        });
                        if let Err(error) = result {
                            notice_error = Some(error);
                            Err(anyhow!("Retry notice refused"))
                        } else {
                            Ok(())
                        }
                    }
                    Event::Status | Event::Probe => {
                        return Err(anyhow!("Unexpected helper control event"));
                    }
                };
                let ack = Ack {
                    request: frame.request,
                    sequence: frame.sequence,
                    proceed: outcome.is_ok(),
                };
                write_frame_async(
                    &mut stream,
                    &serde_json::to_vec(&ack)?,
                    METADATA_MAX,
                    deadline,
                )
                .await?;
            }
        }
        .await;
        if let Some(error) = notice_error {
            return Err(error);
        }
        result
    }
    struct PeerLease {
        identity: ProcessIdentity,
        fd: OwnedFd,
        scope_fd: OwnedFd,
        scope: PeerScope,
        uid: u32,
    }
    impl PeerLease {
        fn admit(fd: i32, scope: &PeerScope, provider_disabled: bool) -> Result<Self> {
            let peer = credentials(fd)?;
            ensure!(
                peer.pid > 0
                    && peer.uid == unsafe { libc::getuid() }
                    && peer.gid == unsafe { libc::getgid() },
                "Helper client is outside its selected identity"
            );
            let identity = process(u32::try_from(peer.pid)?)?;
            let scope_pid = match scope {
                PeerScope::Container { init } => init.pid,
                PeerScope::EngineeringDescendants { ancestor } => ancestor.pid,
            };
            let value = Self {
                fd: pidfd(identity.pid)?,
                scope_fd: pidfd(scope_pid)?,
                identity,
                scope: scope.clone(),
                uid: peer.uid,
            };
            ensure!(
                provider_disabled || matches!(scope, PeerScope::Container { .. }),
                "Live helper requires a model-container peer"
            );
            value.check()?;
            Ok(value)
        }
        fn check(&self) -> Result<()> {
            alive(&self.fd)?;
            alive(&self.scope_fd)?;
            ensure!(
                process(self.identity.pid)? == self.identity,
                "Helper client generation changed"
            );
            zero_time_namespace(self.identity.pid)?;
            ensure!(
                std::fs::metadata(format!("/proc/{}", self.identity.pid))?.uid() == self.uid,
                "Helper client identity changed"
            );
            match &self.scope {
                PeerScope::Container { init } => {
                    ensure!(
                        process(init.pid)? == *init
                            && self.identity.boot_id == init.boot_id
                            && self.identity.pid_namespace
                                != process(std::process::id())?.pid_namespace
                            && (self.identity.cgroup == init.cgroup
                                || self
                                    .identity
                                    .cgroup
                                    .starts_with(&(init.cgroup.clone() + "/"))),
                        "Helper peer is outside the selected container generation"
                    );
                }
                PeerScope::EngineeringDescendants { ancestor } => {
                    ensure!(
                        process(ancestor.pid)? == *ancestor,
                        "Engineering ancestor generation changed"
                    );
                    let mut current = self.identity.pid;
                    let mut admitted = false;
                    for _ in 0..64 {
                        if current == ancestor.pid {
                            admitted = true;
                            break;
                        }
                        let raw = read_proc(current, "stat", 4096)?;
                        let fields: Vec<_> = raw
                            .rsplit_once(") ")
                            .ok_or_else(|| anyhow!("Invalid engineering stat"))?
                            .1
                            .split_whitespace()
                            .collect();
                        current = fields
                            .get(1)
                            .ok_or_else(|| anyhow!("Engineering ancestry unavailable"))?
                            .parse()?;
                        if current == 0 {
                            break;
                        }
                    }
                    ensure!(
                        admitted,
                        "Peer is outside the selected engineering process tree"
                    );
                }
            }
            Ok(())
        }
    }
    fn read_exact(stream: &mut StdStream, mut bytes: &mut [u8], deadline: u64) -> Result<()> {
        while !bytes.is_empty() {
            stream.set_read_timeout(Some(left(deadline)?))?;
            match stream.read(bytes) {
                Ok(0) => return Err(transport_error()),
                Ok(count) => bytes = &mut bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(transport_error()),
            }
        }
        Ok(())
    }
    fn write_all(stream: &mut StdStream, mut bytes: &[u8], deadline: u64) -> Result<()> {
        while !bytes.is_empty() {
            stream.set_write_timeout(Some(left(deadline)?))?;
            match stream.write(bytes) {
                Ok(0) => return Err(transport_error()),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(transport_error()),
            }
        }
        Ok(())
    }
    fn read_frame(stream: &mut StdStream, limit: usize, deadline: u64) -> Result<Vec<u8>> {
        let mut prefix = [0; 4];
        read_exact(stream, &mut prefix, deadline)?;
        let length = u32::from_be_bytes(prefix) as usize;
        ensure!(
            length > 0 && length <= limit,
            "Helper frame exceeds its admitted bound"
        );
        let mut bytes = vec![0; length];
        read_exact(stream, &mut bytes, deadline)?;
        Ok(bytes)
    }
    fn write_frame(
        stream: &mut StdStream,
        bytes: &[u8],
        limit: usize,
        deadline: u64,
    ) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= limit,
            "Helper frame exceeds its admitted bound"
        );
        write_all(stream, &u32::try_from(bytes.len())?.to_be_bytes(), deadline)?;
        write_all(stream, bytes, deadline)
    }
    async fn read_async(
        stream: &mut tokio::net::UnixStream,
        mut bytes: &mut [u8],
        deadline: u64,
    ) -> Result<()> {
        while !bytes.is_empty() {
            let count = tokio::time::timeout(left(deadline)?, stream.read(bytes))
                .await
                .map_err(|_| transport_error())??;
            ensure!(count > 0, "Helper channel closed without a terminal result");
            bytes = &mut bytes[count..];
        }
        Ok(())
    }
    async fn write_async(
        stream: &mut tokio::net::UnixStream,
        mut bytes: &[u8],
        deadline: u64,
    ) -> Result<()> {
        while !bytes.is_empty() {
            let count = tokio::time::timeout(left(deadline)?, stream.write(bytes))
                .await
                .map_err(|_| transport_error())??;
            ensure!(count > 0, "Helper channel closed without a terminal result");
            bytes = &bytes[count..];
        }
        Ok(())
    }
    async fn read_frame_async(
        stream: &mut tokio::net::UnixStream,
        limit: usize,
        deadline: u64,
    ) -> Result<Vec<u8>> {
        let mut prefix = [0; 4];
        read_async(stream, &mut prefix, deadline).await?;
        let length = u32::from_be_bytes(prefix) as usize;
        ensure!(
            length > 0 && length <= limit,
            "Helper frame exceeds its admitted bound"
        );
        let mut bytes = vec![0; length];
        read_async(stream, &mut bytes, deadline).await?;
        Ok(bytes)
    }
    async fn write_frame_async(
        stream: &mut tokio::net::UnixStream,
        bytes: &[u8],
        limit: usize,
        deadline: u64,
    ) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= limit,
            "Helper frame exceeds its admitted bound"
        );
        write_async(stream, &u32::try_from(bytes.len())?.to_be_bytes(), deadline).await?;
        write_async(stream, bytes, deadline).await
    }

    pub(super) fn harden() -> Result<()> {
        let core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        ensure!(
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core) } == 0
                && unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0
                && unsafe { libc::prctl(libc::PR_GET_DUMPABLE) } == 0
                && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
            "Helper process hardening failed"
        );
        let mut observed = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        ensure!(
            unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut observed) } == 0
                && observed.rlim_cur == 0
                && observed.rlim_max == 0,
            "Helper core-dump protection unavailable"
        );
        for entry in std::fs::read_dir("/proc/self/fd")? {
            let entry = entry?;
            if let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.parse::<i32>().ok())
                && fd > 2
            {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                // The directory iterator's own descriptor may already have closed.
                ensure!(
                    flags < 0
                        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
                    "Helper descriptor hardening failed"
                );
            }
        }
        Ok(())
    }

    fn hardened_private_startup() -> Result<()> {
        let mut core = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        ensure!(
            unsafe { libc::prctl(libc::PR_GET_DUMPABLE) } == 0
                && unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 1
                && unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut core) } == 0
                && core.rlim_cur == 0
                && core.rlim_max == 0,
            "Helper process must be hardened before credential ingestion"
        );
        let status = read_proc(std::process::id(), "status", 8192)?;
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:\t"))
            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok());
        ensure!(
            mask.is_some_and(|value| value & 0o077 == 0o077),
            "Helper must inherit a private umask before startup"
        );
        Ok(())
    }

    struct SecretGuard(Option<String>);
    impl SecretGuard {
        fn safe(&self, bytes: &[u8]) -> bool {
            let Some(key) = self.0.as_ref() else {
                return true;
            };
            if bytes.windows(key.len()).any(|part| part == key.as_bytes()) {
                return false;
            }
            fn strings(value: &Value, key: &str) -> bool {
                match value {
                    Value::String(value) => !value.contains(key),
                    Value::Array(values) => values.iter().all(|value| strings(value, key)),
                    Value::Object(values) => values
                        .iter()
                        .all(|(name, value)| !name.contains(key) && strings(value, key)),
                    _ => true,
                }
            }
            crate::parse_unique_json(bytes).is_ok_and(|value| strings(&value, key))
        }
        fn check(&self, bytes: &[u8]) -> Result<()> {
            ensure!(self.safe(bytes), "Helper refused credential reflection");
            Ok(())
        }
    }
    pub(super) fn metadata_safe(key: Option<&str>, value: &Value) -> bool {
        key.is_none_or(|value| !value.is_empty())
            && serde_json::to_vec(value)
                .is_ok_and(|bytes| SecretGuard(key.map(str::to_owned)).safe(&bytes))
    }

    fn private_bytes(path: &Path, limit: usize) -> Result<Vec<u8>> {
        let parent = open_directory(
            path.parent()
                .ok_or_else(|| anyhow!("Private helper record needs a parent"))?,
        )?;
        let leaf = path
            .file_name()
            .ok_or_else(|| anyhow!("Private helper record needs a name"))?;
        ensure!(
            leaf != "auth.json" && !leaf.as_bytes().starts_with(b".env"),
            "Credential paths are not helper records"
        );
        let leaf = CString::new(leaf.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "Private helper record unavailable");
        let mut file = unsafe { File::from_raw_fd(fd) };
        let before = file.metadata()?;
        ensure!(
            before.is_file()
                && before.nlink() == 1
                && before.uid() == unsafe { libc::getuid() }
                && before.mode() & 0o777 == 0o600
                && before.len() <= limit as u64,
            "Private helper record custody differs"
        );
        let mut bytes = Vec::new();
        (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(
            bytes.len() <= limit
                && node(&before) == node(&after)
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "Private helper record changed"
        );
        Ok(bytes)
    }

    struct OwnerLease {
        binding: FilePin,
        lease: FilePin,
        value: Value,
        owner: ProcessIdentity,
        owner_fd: OwnedFd,
        deadline: u64,
    }
    impl OwnerLease {
        fn open(control: &ServerControl) -> Result<Self> {
            ensure!(
                std::env::var_os("ENVIRONMENTS_HARNESS_STORAGE_BINDING").as_deref()
                    == Some(control.owner_binding.path.as_os_str())
                    && std::env::var("ENVIRONMENTS_HARNESS_STORAGE_DIGEST")
                        .ok()
                        .as_deref()
                        == Some(control.owner_binding.sha256.as_str()),
                "Helper must inherit its exact checked owner binding"
            );
            let value: Value = read_pin(&control.owner_binding, CONTROL_MAX)?;
            ensure!(
                value["schema"] == "environments-harness.storage-binding.v1",
                "Helper storage owner contract differs"
            );
            let plan = &value["plan"];
            let temporary = Path::new(
                plan["targets"]["temporary"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Checked helper temporary target unavailable"))?,
            );
            ensure!(
                absolute(temporary)
                    && control.socket_parent.starts_with(temporary)
                    && control.socket_parent != temporary,
                "Helper endpoint is outside the checked temporary target"
            );
            let state = Path::new(
                plan["run_state"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Checked owner run state unavailable"))?,
            );
            ensure!(
                absolute(state)
                    && control.owner_binding.path.parent()
                        == Some(state.join("bindings").as_path()),
                "Helper owner binding location differs"
            );
            let path = state.join("run.lock");
            let bytes = private_bytes(&path, 4096)?;
            let lease = FilePin {
                path,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            };
            let pid = u32::try_from(
                value["lease_owner_pid"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("Owner PID unavailable"))?,
            )?;
            let owner = process(pid)?;
            let owner_fd = pidfd(pid)?;
            let deadline = value["deadline_monotonic_ns"]
                .as_u64()
                .ok_or_else(|| anyhow!("Owner deadline unavailable"))?;
            let result = Self {
                binding: control.owner_binding.clone(),
                lease,
                value,
                owner,
                owner_fd,
                deadline,
            };
            result.check()?;
            ensure!(
                descends_from(std::process::id(), pid)?,
                "Helper is outside the checked owner process tree"
            );
            Ok(result)
        }
        fn check(&self) -> Result<()> {
            left(self.deadline)?;
            alive(&self.owner_fd)?;
            ensure!(
                process(self.owner.pid)? == self.owner,
                "Checked owner process generation changed"
            );
            let binding: Value = read_pin(&self.binding, CONTROL_MAX)?;
            ensure!(
                binding == self.value,
                "Checked helper owner binding changed"
            );
            let lease: Value = read_pin(&self.lease, 4096)?;
            ensure!(
                lease
                    == serde_json::json!({"lease_id": self.value["lease_id"],
            "owner_pid": self.value["lease_owner_pid"], "plan_digest": self.value["plan"]["digest"]}),
                "Checked helper owner lease changed"
            );
            let lock = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&self.lease.path)
                .map_err(|_| anyhow!("Checked owner lock unavailable"))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                unsafe {
                    libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
                }
                return Err(anyhow!("Checked helper owner lease is no longer held"));
            }
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK),
                "Checked helper owner lease lock unavailable"
            );
            Ok(())
        }
    }
    pub(super) fn key_input_remaining(control: &ServerControl) -> Result<Duration> {
        ensure!(
            !environment_selected() && std::env::var_os("OPENROUTER_API_KEY").is_none(),
            "Keyholder must not inherit client routing or credential environment"
        );
        hardened_private_startup()?;
        let owner = OwnerLease::open(control)?;
        let _scope = selected_scope(control, false)?;
        let (_, execution) = join_server_budget(control)?;
        Ok(left(owner.deadline)?.min(execution.original_run_remaining()?))
    }

    fn join_server_budget(control: &ServerControl) -> Result<(DecisionsBudget, ExecutionContext)> {
        let budget: DecisionsBudget = read_pin(&control.budget, CONTROL_MAX)?;
        let name = budget
            .ledger_path
            .file_name()
            .ok_or_else(|| anyhow!("Helper ledger needs a name"))?;
        ensure!(
            budget.schema_version == crate::DECISIONS_BUDGET_CONTRACT_SCHEMA
                && absolute(&budget.ledger_path)
                && name != "auth.json"
                && !name.as_bytes().starts_with(b".env"),
            "Invalid private helper budget"
        );
        let execution = budget.join()?;
        Ok((budget, execution))
    }

    fn selected_scope(
        control: &ServerControl,
        engineering_fixture: bool,
    ) -> Result<(ProcessIdentity, OwnedFd)> {
        let identity = match &control.peer {
            PeerScope::Container { init } => {
                let own = process(std::process::id())?;
                ensure!(
                    process(init.pid)? == *init
                        && init.pid_namespace != own.pid_namespace
                        && init.cgroup != "/"
                        && init.cgroup != own.cgroup,
                    "Helper requires the original selected container generation"
                );
                zero_time_namespace(init.pid)?;
                init.clone()
            }
            PeerScope::EngineeringDescendants { ancestor } => {
                ensure!(
                    engineering_fixture || control.provider_disabled,
                    "Live helper does not admit an engineering process scope"
                );
                ensure!(
                    process(ancestor.pid)? == *ancestor,
                    "Engineering process scope changed"
                );
                ancestor.clone()
            }
        };
        let fd = pidfd(identity.pid)?;
        alive(&fd)?;
        ensure!(
            process(identity.pid)? == identity,
            "Selected peer scope generation changed"
        );
        Ok((identity, fd))
    }
    fn descends_from(mut current: u32, ancestor: u32) -> Result<bool> {
        for _ in 0..64 {
            if current == ancestor {
                return Ok(true);
            }
            let raw = read_proc(current, "stat", 4096)?;
            current = raw
                .rsplit_once(") ")
                .and_then(|value| value.1.split_whitespace().nth(1))
                .ok_or_else(|| anyhow!("Helper ancestry unavailable"))?
                .parse()?;
            if current == 0 {
                return Ok(false);
            }
        }
        Ok(false)
    }

    struct ServerState {
        control: ClientControl,
        endpoint: Endpoint,
        owner: Option<OwnerLease>,
        peer: PeerScope,
        execution: ExecutionContext,
        provider: Option<JevClient>,
        guard: SecretGuard,
        deadline: u64,
        active: Arc<tokio::sync::Semaphore>,
        submissions: Arc<tokio::sync::Semaphore>,
        readonly: Arc<tokio::sync::Semaphore>,
        stopping: AtomicBool,
        scope_identity: ProcessIdentity,
        scope_fd: OwnedFd,
    }
    impl ServerState {
        fn check(&self) -> Result<()> {
            ensure!(
                !self.stopping.load(Ordering::SeqCst),
                "Helper admissions are closed"
            );
            left(self.deadline)?;
            alive(&self.scope_fd)?;
            ensure!(
                process(self.scope_identity.pid)? == self.scope_identity,
                "Selected helper peer scope generation changed"
            );
            self.endpoint.verify_leaf(&self.control.socket_identity)?;
            if let Some(owner) = &self.owner {
                owner.check()?;
            }
            self.execution.check_deadline()
        }
    }
    struct Session {
        stream: Mutex<StdStream>,
        peer: PeerLease,
        state: Arc<ServerState>,
        request: String,
        sequence: AtomicU64,
        deadline: u64,
        privacy_refused: AtomicBool,
    }
    impl Session {
        fn send(&self, event: Event, response: Option<&[u8]>, acknowledge: bool) -> Result<()> {
            self.state.check()?;
            self.peer.check()?;
            left(self.deadline)?;
            let sequence = self.sequence.load(Ordering::SeqCst);
            ensure!(sequence <= 9, "Helper callback sequence exceeds its bound");
            let frame = ServerFrame {
                request: self.request.clone(),
                sequence,
                snapshot: self.state.execution.snapshot()?,
                event,
            };
            let bytes = serde_json::to_vec(&frame)?;
            if !self.state.guard.safe(&bytes)
                || response.is_some_and(|value| !self.state.guard.safe(value))
            {
                self.privacy_refused.store(true, Ordering::SeqCst);
                return Err(anyhow!("Helper refused credential reflection"));
            }
            let mut stream = self.stream.lock().map_err(|_| transport_error())?;
            // A privacy refusal emits no bytes and must consume no sequence.
            // Commit only after screening, so the following fixed failure stays contiguous.
            self.sequence
                .compare_exchange(sequence, sequence + 1, Ordering::SeqCst, Ordering::SeqCst)
                .map_err(|_| transport_error())?;
            write_frame(&mut stream, &bytes, METADATA_MAX, self.deadline)?;
            if let Some(response) = response {
                write_frame(&mut stream, response, RESPONSE_WIRE_MAX, self.deadline)?;
            }
            if acknowledge {
                let ack: Ack = unique(&read_frame(&mut stream, METADATA_MAX, self.deadline)?)?;
                ensure!(
                    ack.request == self.request && ack.sequence == sequence && ack.proceed,
                    "Client callback refused or its channel closed"
                );
                self.peer.check()?;
                self.state.check()?;
            }
            Ok(())
        }
        fn bridge(&self, event: Event, response: Option<&[u8]>) -> Result<()> {
            // Four synchronous callback ACKs must not occupy all executor workers.
            tokio::task::block_in_place(|| self.send(event, response, true))
        }
    }
    struct Bridge(Arc<Session>);
    impl DecisionObserver for Bridge {
        fn send_started(&self, request: &DecisionRequestObservation<'_>) -> Result<()> {
            self.0.bridge(
                Event::BeforeSend {
                    body_sha256: request.body_sha256.to_owned(),
                    body_bytes: request.body_bytes,
                    question_count: request.question_count,
                },
                None,
            )
        }
        fn validated_response(
            &self,
            _: &DecisionRequestObservation<'_>,
            response: &DecisionResponse,
        ) -> Result<()> {
            let (bytes, _) = response_body(response)?;
            self.0.bridge(
                Event::Validated {
                    response_bytes: bytes.len(),
                    response_sha256: format!("{:x}", Sha256::digest(&bytes)),
                },
                Some(&bytes),
            )
        }
        fn failed(&self, _: &DecisionRequestObservation<'_>) -> Result<()> {
            self.0.bridge(Event::Failed, None)
        }
    }

    fn prepare_server(
        client: &JevClient,
        mode: PreparedBodyMode,
        bytes: Vec<u8>,
    ) -> Result<PreparedDecision> {
        let questions = match mode {
            PreparedBodyMode::Ordinary => {
                ensure!(
                    bytes.len() <= MAX_REQUEST_BYTES,
                    "Ordinary helper body exceeds its codec bound"
                );
                let body = crate::parse_unique_json(&bytes)?;
                let fields = body
                    .as_object()
                    .ok_or_else(|| anyhow!("Invalid helper request body"))?;
                ensure!(
                    fields.len() == 4
                        && body["model"].as_str() == Some(client.model.as_str())
                        && body["provider"] == serde_json::json!({"allow_fallbacks": false}),
                    "Helper request model or provider policy differs"
                );
                let prepared =
                    client.prepare_decision(body["state"].clone(), body["questions"].clone())?;
                ensure!(
                    prepared.body == bytes,
                    "Helper request is not the canonical prepared body"
                );
                prepared.questions
            }
            PreparedBodyMode::TypedInput => {
                crate::input::validate_helper_flat_body(client, &bytes)?
            }
        };
        Ok(client.bind_prepared_mode(bytes, questions, mode))
    }

    async fn connection(mut stream: tokio::net::UnixStream, state: Arc<ServerState>) -> Result<()> {
        state.check()?;
        let peer = PeerLease::admit(
            stream.as_raw_fd(),
            &state.peer,
            state.control.provider_disabled,
        )?;
        let initial = deadline_after(state.execution.remaining()?)?.min(state.deadline);
        let header: WireHeader =
            unique(&read_frame_async(&mut stream, METADATA_MAX, initial).await?)?;
        validate_header(&header, &state.control)?;
        let operation = header.operation;
        match operation {
            Operation::Status | Operation::Probe => {
                let _permit = Arc::clone(&state.readonly)
                    .try_acquire_owned()
                    .map_err(|_| transport_error())?;
                let probe = matches!(operation, Operation::Probe);
                ensure!(
                    !probe || state.control.provider_disabled,
                    "Live helper does not admit engineering probes"
                );
                state.check()?;
                peer.check()?;
                let frame = ServerFrame {
                    request: header.request,
                    sequence: 1,
                    snapshot: state.execution.snapshot()?,
                    event: if probe { Event::Probe } else { Event::Status },
                };
                let bytes = serde_json::to_vec(&frame)?;
                state.guard.check(&bytes)?;
                write_frame_async(&mut stream, &bytes, METADATA_MAX, initial).await?;
            }
            Operation::Submit {
                mode,
                body_sha256,
                body_bytes,
                deadline_monotonic_ns,
                retry,
            } => {
                let _waiting = Arc::clone(&state.submissions)
                    .try_acquire_owned()
                    .map_err(|_| transport_error())?;
                ensure!(
                    state.provider.is_some() && hex(&body_sha256, 64),
                    "Helper provider submissions are disabled"
                );
                let deadline = initial.min(deadline_monotonic_ns);
                left(deadline)?;
                let limit = match mode {
                    PreparedBodyMode::Ordinary => MAX_REQUEST_BYTES,
                    PreparedBodyMode::TypedInput => MAX_MULTIMODAL_REQUEST_BYTES,
                };
                ensure!(
                    body_bytes > 0 && body_bytes <= limit,
                    "Helper body exceeds the selected codec bound"
                );
                let body = read_frame_async(&mut stream, limit, deadline).await?;
                ensure!(
                    body.len() == body_bytes
                        && format!("{:x}", Sha256::digest(&body)) == body_sha256,
                    "Helper body differs from its descriptor"
                );
                let _active = tokio::time::timeout(
                    left(deadline)?,
                    Arc::clone(&state.active).acquire_owned(),
                )
                .await
                .map_err(|_| transport_error())??;
                state.check()?;
                peer.check()?;
                let stream = stream.into_std()?;
                stream.set_nonblocking(false)?;
                let session = Arc::new(Session {
                    stream: Mutex::new(stream),
                    peer,
                    state: Arc::clone(&state),
                    request: header.request,
                    sequence: AtomicU64::new(1),
                    deadline,
                    privacy_refused: AtomicBool::new(false),
                });
                let context = state
                    .execution
                    .with_deadline(Instant::now() + left(deadline)?)?;
                let client = state
                    .provider
                    .as_ref()
                    .expect("provider checked")
                    .clone()
                    .with_execution_context(context)
                    .with_decision_observer(Arc::new(Bridge(Arc::clone(&session))));
                let prepared = prepare_server(&client, mode, body)?;
                let notice_session = Arc::clone(&session);
                let notice = move |prior: &DecisionRetriedAttempt| {
                    notice_session.bridge(
                        Event::BeforeRetry {
                            prior: prior.clone(),
                        },
                        None,
                    )
                };
                let policy = TransientRetry::standard();
                let result = client
                    .submit_prepared_with(prepared, retry.then_some(&policy), Some(&notice))
                    .await;
                tokio::task::block_in_place(|| match result {
                    Ok(response) => {
                        let (bytes, prior_attempts) = response_body(&response)?;
                        let event = Event::Complete {
                            response_bytes: bytes.len(),
                            response_sha256: format!("{:x}", Sha256::digest(&bytes)),
                            prior_attempts,
                        };
                        session.send(event, Some(&bytes), false)
                    }
                    Err(error) => {
                        if let Some(failure) = error.downcast_ref::<crate::DecisionFailure>() {
                            let mut failure = failure.helper_wire();
                            if session.privacy_refused.load(Ordering::SeqCst) {
                                failure = failure.privacy_refusal();
                            }
                            session.send(Event::Error { failure }, None, false)
                        } else {
                            // A pre-observer admission/ledger error is not an observer failure.
                            // Unknown dynamic text stays private; unavailable snapshots close the
                            // channel rather than inventing progress or zero accounting.
                            session.send(
                                Event::Unavailable {
                                    reason: AdmissionReason::from_error(&error),
                                },
                                None,
                                false,
                            )
                        }
                    }
                })?;
            }
        }
        Ok(())
    }

    pub(super) async fn serve(
        control: ServerControl,
        key: Option<String>,
        shutdown: impl std::future::Future<Output = ()> + Send,
        ready: impl FnOnce(Value) -> Result<()> + Send,
    ) -> Result<Value> {
        control.validate()?;
        ensure!(
            !environment_selected() && std::env::var_os("OPENROUTER_API_KEY").is_none(),
            "Keyholder must not inherit client routing or credential environment"
        );
        let owner = OwnerLease::open(&control)?;
        let provider = if control.provider_disabled {
            ensure!(
                key.is_none(),
                "Provider-disabled helper accepts no credential"
            );
            None
        } else {
            Some(JevClient::new(
                key.as_deref()
                    .ok_or_else(|| anyhow!("Helper stdin credential required"))?,
                Some(&control.model),
            )?)
        };
        serve_inner(control, key, Some(owner), provider, shutdown, ready, false).await
    }

    async fn serve_inner(
        control: ServerControl,
        key: Option<String>,
        owner: Option<OwnerLease>,
        provider: Option<JevClient>,
        shutdown: impl std::future::Future<Output = ()> + Send,
        ready: impl FnOnce(Value) -> Result<()> + Send,
        engineering_fixture: bool,
    ) -> Result<Value> {
        ensure!(
            tokio::runtime::Handle::current().runtime_flavor()
                == tokio::runtime::RuntimeFlavor::MultiThread,
            "Helper server requires the bounded multithread runtime"
        );
        if !engineering_fixture {
            hardened_private_startup()?;
        }
        zero_time_namespace(std::process::id())?;
        let (scope_identity, scope_fd) = selected_scope(&control, engineering_fixture)?;
        let (budget, execution) = join_server_budget(&control)?;
        let anchor = execution
            .ledger_clock_anchor()?
            .ok_or_else(|| anyhow!("Helper original budget clock unavailable"))?;
        let deadline = deadline_after(execution.original_run_remaining()?)?
            .min(owner.as_ref().map_or(u64::MAX, |owner| owner.deadline));
        let endpoint = Endpoint::open(&control.socket_parent, &control.parent_identity)?;
        let address = endpoint.address()?;
        // bind is create-only: a stale or foreign leaf is never removed or adopted.
        let listener = StdListener::bind(&address)
            .map_err(|_| anyhow!("Helper endpoint is unavailable or already exists"))?;
        if engineering_fixture {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&address, std::fs::Permissions::from_mode(0o600))?;
        }
        let socket = endpoint.leaf()?;
        ensure!(
            socket.uid == unsafe { libc::getuid() }
                && socket.gid == unsafe { libc::getgid() }
                && socket.mode & 0o077 == 0,
            "Helper socket custody is not private"
        );
        let identity = process(std::process::id())?;
        let instance_source = format!(
            "{}:{}:{}:{}",
            identity.boot_id,
            identity.pid,
            identity.start_ticks,
            mono()?
        );
        let instance = format!("{:x}", Sha256::digest(instance_source.as_bytes()))[..32].to_owned();
        let client = ClientControl {
            schema_version: CLIENT_SCHEMA.to_owned(),
            instance,
            task_binding_sha256: control.task_binding_sha256.clone(),
            socket_parent: control.client_visible_parent,
            parent_identity: endpoint.identity.clone(),
            socket_identity: socket,
            server: identity,
            server_uid: unsafe { libc::getuid() },
            server_gid: unsafe { libc::getgid() },
            model: control.model,
            provider_disabled: control.provider_disabled,
            budget: PublicBudget {
                run_id: budget.run_id,
                plan_digest: budget.plan_digest,
                deadline_unix_ms: budget.deadline_unix_ms,
                max_submissions: budget.max_submissions,
                clock_anchor: anchor,
            },
        };
        client.validate()?;
        let guard = SecretGuard(key);
        let bytes = serde_json::to_vec(&client)?;
        guard.check(&bytes)?;
        let client_name = CString::new("client.json")?;
        let fd = unsafe {
            libc::openat(
                endpoint.directory.as_raw_fd(),
                client_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(
            fd >= 0,
            "Helper client control must be a new private record"
        );
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        file.sync_all()?;
        ensure!(
            node(&file.metadata()?).mode == 0o600 && file.metadata()?.nlink() == 1,
            "Helper client control custody differs"
        );
        let readiness = serde_json::json!({"schema_version":"gptgrep.decisions-helper-ready.v1",
        "ready":true,"provider_disabled":client.provider_disabled,"server_pid":client.server.pid,
        "server_start_ticks":client.server.start_ticks,"instance":client.instance,
        "client_control":control.client_control_path,"client_control_sha256":format!("{:x}", Sha256::digest(&bytes))});
        guard.check(&serde_json::to_vec(&readiness)?)?;
        let mut host_client = client.clone();
        host_client.socket_parent = endpoint.parent.clone();
        let state = Arc::new(ServerState {
            control: host_client,
            endpoint,
            owner,
            peer: control.peer,
            execution,
            provider,
            guard,
            deadline,
            active: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            submissions: Arc::new(tokio::sync::Semaphore::new(2 * MAX_CONCURRENT_REQUESTS)),
            readonly: Arc::new(tokio::sync::Semaphore::new(1)),
            stopping: AtomicBool::new(false),
            scope_identity,
            scope_fd,
        });
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        ready(readiness)?;
        let connections = Arc::new(tokio::sync::Semaphore::new(2 * MAX_CONCURRENT_REQUESTS + 1));
        let mut tasks = tokio::task::JoinSet::new();
        tokio::pin!(shutdown);
        let lifetime = tokio::time::sleep(left(deadline)?);
        tokio::pin!(lifetime);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = &mut lifetime => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                incoming = listener.accept() => {
                    let (stream, _) = incoming.map_err(|_| anyhow!("Helper listener failed"))?;
                    if let Ok(permit) = Arc::clone(&connections).try_acquire_owned() {
                        let state = Arc::clone(&state);
                        tasks.spawn(async move { let _permit = permit; let _ = connection(stream, state).await; });
                    }
                }
            }
        }
        state.stopping.store(true, Ordering::SeqCst);
        drop(listener);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        let result = serde_json::json!({"schema_version":"gptgrep.decisions-helper-terminal.v1",
        "terminal":true,"provider_disabled":state.control.provider_disabled,
        "snapshot":state.execution.snapshot()?,"endpoint_retained":true,
        "cleanup":"outer owner must verify terminal generation and exact inode before removal"});
        state.guard.check(&serde_json::to_vec(&result)?)?;
        Ok(result)
    }

    #[cfg(test)]
    include!("helper_ipc_tests.rs");
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;
    pub(super) fn read_pin<T: serde::de::DeserializeOwned>(_: &FilePin, _: usize) -> Result<T> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) fn status(_: &ClientControl) -> Result<ExecutionSnapshot> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) async fn probe(_: &ClientControl) -> Result<ExecutionSnapshot> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) fn harden() -> Result<()> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) fn key_input_remaining(_: &ServerControl) -> Result<std::time::Duration> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) fn metadata_safe(key: Option<&str>, value: &Value) -> bool {
        key.is_none() && serde_json::to_vec(value).is_ok()
    }
    pub(super) fn callback_snapshot(
        _: Option<&str>,
        _: Option<&str>,
        _: Option<u64>,
        _: u32,
        _: Option<&str>,
    ) -> Option<ExecutionSnapshot> {
        None
    }
    pub(super) async fn submit(
        _: &RemoteContext,
        _: &JevClient,
        _: PreparedDecision,
        _: bool,
        _: Option<&RetryNotice<'_>>,
    ) -> Result<DecisionResponse> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
    pub(super) async fn serve(
        _: ServerControl,
        _: Option<String>,
        _: impl std::future::Future<Output = ()> + Send,
        _: impl FnOnce(Value) -> Result<()> + Send,
    ) -> Result<Value> {
        Err(anyhow!("Decisions helper IPC requires Linux"))
    }
}
