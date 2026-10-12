//! Deterministic parser/OCR stages and stdIO for external Codex/PTC composition.
//! The explicit agent successor launches one separately admitted Codex document
//! turn over these same prepared stages; deterministic run and external PTC serve
//! remain independently usable. The enclosing owner supplies all admission.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Result, anyhow, ensure};
use gptgrep_core::parse_agent::{
    OcrExecutionSummary, OcrPlanSection, ParseToolbox, REPLY_SCHEMA, REQUEST_SCHEMA, ToolboxLimits,
    ToolboxOperation, ToolboxRequest,
};
use gptgrep_host::{
    OcrCancellation, OcrCleanup, OcrExecutionContext, OcrExecutionLimits, OcrHostConfig,
    OcrHostExecutor, OcrRuntimeProfile,
};
use gptgrep_jev::RunClockAnchor;
use gptgrep_parse::{
    BoundingBox, NativeOcrCrop, OcrExecutor, OcrMode, OcrPageResult, OcrTask, ResourceBudget,
    RichParsedDocument,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, timeout_at};

const CONTROL_SCHEMA: &str = "gptgrep.parse-agent.control.v1";
const PRODUCT_SCHEMA: &str = "gptgrep.parse-agent.product.v1";
const CONTROL_BYTES: usize = 65_536;
const FRONT_WORKING: u64 = 8_388_608;
const REPLY_MAX: usize = 4_194_304;
const TASK_REPLY: usize = 262_144;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema: String,
    invocation_id: String,
    nonce: String,
    staging_root: PathBuf,
    sources: Vec<Source>,
    resources: ResourceBudget,
    toolbox: ToolboxControl,
    frontend: FrontendBounds,
    native: NativeControl,
    ocr: Option<OcrControl>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    alias: String,
    path: PathBuf,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolboxControl {
    request_bytes: usize,
    reply_bytes: usize,
    artifact_window_bytes: usize,
    operation_working_bytes: u64,
}
impl ToolboxControl {
    fn limits(&self) -> ToolboxLimits {
        ToolboxLimits {
            request_bytes: self.request_bytes,
            reply_bytes: self.reply_bytes,
            artifact_window_bytes: self.artifact_window_bytes,
            operation_working_bytes: self.operation_working_bytes,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrontendBounds {
    session_timeout_ms: u64,
    request_timeout_ms: u64,
    max_requests: u64,
    request_bytes: usize,
    total_wire_bytes: u64,
    output_file_bytes: u64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeControl {
    page_window: usize,
    dpi: f32,
    render_pages: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OcrControl {
    codex_bin: PathBuf,
    auth_home: PathBuf,
    state_dir: PathBuf,
    results_root: PathBuf,
    runtime_profile: OcrRuntimeProfile,
    run_id: String,
    plan_sha256: String,
    original_clock: RunClockAnchor,
    deadline_unix_ms: u64,
    max_pages: usize,
    /// None selects the existing v1 journal (24 admissions, 128 KiB, two slots).
    /// Some must exactly match the separately admitted existing v2 journal.
    phase_limits: Option<OcrExecutionLimits>,
    ledger: PathBuf,
}

fn code<T>(result: Result<T>, message: &'static str) -> Result<T> {
    result.map_err(|_| anyhow!(message))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hash64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn nonce(text: &str) -> Result<[u8; 32]> {
    ensure!(
        hash64(text) && text.bytes().any(|b| b != b'0'),
        "parse_agent_nonce"
    );
    let mut out = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b - b'a' + 10
            }
        };
        out[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(out)
}
fn credential_shaped(path: &Path) -> bool {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .any(|part| {
            let part = part.to_ascii_lowercase();
            matches!(
                part.as_str(),
                "auth.json"
                    | "credentials"
                    | "credentials.json"
                    | "token.json"
                    | "tokens.json"
                    | "secrets"
                    | "secret.json"
                    | "credentials.toml"
                    | ".aws"
                    | ".ssh"
                    | ".gnupg"
                    | ".password-store"
                    | "id_rsa"
                    | "id_ed25519"
                    | "id_ecdsa"
                    | ".env"
                    | "keychain"
            ) || part.starts_with(".env.")
                || part.ends_with(".env")
                || part.ends_with(".pem")
                || part.ends_with(".key")
                || part.ends_with(".p12")
                || part.ends_with(".keychain")
                || part.ends_with(".keychain-db")
        })
}
fn lexical(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && path.components().count() <= 128
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "parse_agent_path"
    );
    ensure!(
        path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "parse_agent_path"
    );
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    lexical(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| anyhow!("parse_agent_directory"))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "parse_agent_directory"
    );
    ensure!(
        fs::canonicalize(path).map_err(|_| anyhow!("parse_agent_directory"))? == path,
        "parse_agent_directory"
    );
    Ok(())
}
fn regular(path: &Path, limit: u64, reject_credentials: bool) -> Result<fs::Metadata> {
    lexical(path)?;
    ensure!(
        !reject_credentials || !credential_shaped(path),
        "parse_agent_credential_path"
    );
    let metadata = fs::symlink_metadata(path).map_err(|_| anyhow!("parse_agent_file"))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit,
        "parse_agent_file"
    );
    ensure!(
        fs::canonicalize(path).map_err(|_| anyhow!("parse_agent_file"))? == path,
        "parse_agent_file"
    );
    Ok(metadata)
}

#[cfg(unix)]
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
#[cfg(unix)]
impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}
fn control(path: &Path, expected_sha: &str) -> Result<Control> {
    ensure!(hash64(expected_sha), "parse_agent_control_pin");
    let before = regular(path, CONTROL_BYTES as u64, true)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    return Err(anyhow!("parse_agent_unix_custody_required"));
    let mut file = options
        .open(path)
        .map_err(|_| anyhow!("parse_agent_control_read"))?;
    #[cfg(unix)]
    ensure!(
        Stamp::of(&before)
            == Stamp::of(
                &file
                    .metadata()
                    .map_err(|_| anyhow!("parse_agent_control_read"))?
            ),
        "parse_agent_control_changed"
    );
    regular(path, CONTROL_BYTES as u64, true)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(CONTROL_BYTES)
        .map_err(|_| anyhow!("parse_agent_control_allocation"))?;
    let mut scratch = [0; 4096];
    loop {
        let count = file
            .read(&mut scratch)
            .map_err(|_| anyhow!("parse_agent_control_read"))?;
        if count == 0 {
            break;
        }
        ensure!(
            count <= CONTROL_BYTES.saturating_sub(bytes.len()),
            "parse_agent_control_limit"
        );
        bytes.extend_from_slice(&scratch[..count]);
    }
    ensure!(
        bytes.len() <= CONTROL_BYTES
            && bytes.len() as u64 == before.len()
            && digest(&bytes) == expected_sha,
        "parse_agent_control_pin"
    );
    let after = regular(path, CONTROL_BYTES as u64, true)?;
    #[cfg(unix)]
    ensure!(
        Stamp::of(&before) == Stamp::of(&after)
            && Stamp::of(&after)
                == Stamp::of(
                    &file
                        .metadata()
                        .map_err(|_| anyhow!("parse_agent_control_read"))?
                ),
        "parse_agent_control_changed"
    );
    let config: Control =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("parse_agent_control_schema"))?;
    validate(&config)?;
    Ok(config)
}
fn validate(config: &Control) -> Result<()> {
    ensure!(
        config.schema == CONTROL_SCHEMA
            && !config.invocation_id.is_empty()
            && config.invocation_id.len() <= 128
            && !config.invocation_id.chars().any(char::is_control),
        "parse_agent_control_schema"
    );
    nonce(&config.nonce)?;
    code(config.resources.validate(), "parse_agent_resource_limits")?;
    directory(&config.staging_root)?;
    ensure!(
        (1..=64).contains(&config.sources.len()),
        "parse_agent_source_count"
    );
    let mut aliases = BTreeSet::new();
    for source in &config.sources {
        ensure!(
            !source.alias.is_empty()
                && source.alias.len() <= 128
                && !source.alias.chars().any(char::is_control)
                && aliases.insert(&source.alias)
                && hash64(&source.sha256),
            "parse_agent_source_schema"
        );
        let metadata = regular(&source.path, config.resources.document_staging_bytes, true)?;
        ensure!(metadata.len() == source.bytes, "parse_agent_source_pin");
    }
    let t = &config.toolbox;
    ensure!(
        t.request_bytes > 0
            && t.request_bytes <= 65_536
            && (1024..=REPLY_MAX).contains(&t.reply_bytes)
            && (1..=32_768).contains(&t.artifact_window_bytes)
            && t.operation_working_bytes > 0,
        "parse_agent_toolbox_limits"
    );
    let b = &config.frontend;
    ensure!(
        (1..=86_400_000).contains(&b.session_timeout_ms)
            && b.request_timeout_ms > 0
            && b.request_timeout_ms <= b.session_timeout_ms
            && (1..=1_048_576).contains(&b.max_requests)
            && (2..=t.request_bytes).contains(&b.request_bytes)
            && (1024..=1_073_741_824).contains(&b.total_wire_bytes)
            && b.output_file_bytes > 0
            && b.output_file_bytes <= config.resources.document_staging_bytes,
        "parse_agent_frontend_limits"
    );
    ensure!(
        (1..=25).contains(&config.native.page_window)
            && config.native.dpi.is_finite()
            && config.native.dpi > 0.0
            && config.native.dpi <= 300.0,
        "parse_agent_native_limits"
    );
    if let Some(ocr) = &config.ocr {
        regular(&ocr.codex_bin, 1_073_741_824, true)?;
        directory(&ocr.auth_home)?;
        directory(&ocr.state_dir)?;
        directory(&ocr.state_dir.join("logs"))?;
        directory(&ocr.results_root)?;
        ensure!(
            !ocr.auth_home.starts_with(&config.staging_root)
                && !config.staging_root.starts_with(&ocr.auth_home)
                && !ocr.state_dir.starts_with(&ocr.auth_home)
                && !ocr.auth_home.starts_with(&ocr.state_dir)
                && !ocr.results_root.starts_with(&ocr.auth_home)
                && !ocr.auth_home.starts_with(&ocr.results_root),
            "parse_agent_auth_home_scope"
        );
        code(ocr.runtime_profile.validate(), "parse_agent_ocr_profile")?;
        ensure!(
            ocr.runtime_profile.image_qualification_accepted(),
            "parse_agent_ocr_qualification_required"
        );
        ensure!(
            !ocr.run_id.is_empty()
                && ocr.run_id.len() <= 256
                && !ocr.run_id.chars().any(char::is_control)
                && hash64(&ocr.plan_sha256)
                && ocr.deadline_unix_ms == ocr.original_clock.deadline_unix_ms(),
            "parse_agent_ocr_run_binding"
        );
        let deadline = UNIX_EPOCH
            .checked_add(Duration::from_millis(ocr.deadline_unix_ms))
            .ok_or_else(|| anyhow!("parse_agent_ocr_deadline"))?;
        code(
            ocr.original_clock.verify_deadline(deadline),
            "parse_agent_ocr_deadline",
        )?;
        let ledger_limit = if let Some(limits) = &ocr.phase_limits {
            code(limits.validate(), "parse_agent_ocr_phase_limits")?;
            ensure!(
                limits.max_admissions == ocr.max_pages,
                "parse_agent_ocr_phase_limits"
            );
            limits.ledger_bytes
        } else {
            ensure!(
                (1..=24).contains(&ocr.max_pages),
                "parse_agent_ocr_phase_limits"
            );
            131_072
        };
        regular(&ocr.ledger, ledger_limit, true)?;
        // Names/metadata only here. Existing original journal/header validation
        // occurs through the host API; no missing pair can initialize one.
    }
    Ok(())
}

struct Quota {
    requests: u64,
    wire: u64,
    bounds: FrontendBounds,
}
impl Quota {
    fn request(&mut self) -> Result<()> {
        ensure!(
            self.requests < self.bounds.max_requests,
            "parse_agent_request_count"
        );
        self.requests += 1;
        Ok(())
    }
    fn room(&self, bytes: u64) -> Result<()> {
        ensure!(
            self.wire
                .checked_add(bytes)
                .is_some_and(|n| n <= self.bounds.total_wire_bytes),
            "parse_agent_total_wire_limit"
        );
        Ok(())
    }
    fn wire(&mut self, bytes: usize) -> Result<()> {
        self.room(bytes as u64)?;
        self.wire += bytes as u64;
        Ok(())
    }
}
struct Session {
    config: Control,
    budget: ResourceBudget,
    toolbox: ParseToolbox,
    context: Option<OcrExecutionContext>,
    ocr_cleanup: Option<OcrCleanup>,
    quota: Quota,
    deadline: Instant,
}
impl Session {
    fn new(config: Control, started: Instant) -> Result<Self> {
        Self::new_with_working(config, started, 0)
    }
    fn new_with_working(config: Control, started: Instant, document_working: u64) -> Result<Self> {
        validate(&config)?;
        let original = config.resources.clone();
        let ledger_working = config
            .ocr
            .as_ref()
            .map(|ocr| {
                ocr.phase_limits
                    .as_ref()
                    .map_or(131_072, |limits| limits.ledger_bytes)
            })
            .map(|bytes| {
                bytes
                    .checked_mul(40)
                    .and_then(|n| n.checked_add(65_536))
                    .ok_or_else(|| anyhow!("parse_agent_working_limit"))
            })
            .transpose()?
            .unwrap_or(0);
        let reserved = FRONT_WORKING
            .checked_add(ledger_working)
            .and_then(|n| n.checked_add(document_working))
            .ok_or_else(|| anyhow!("parse_agent_working_limit"))?;
        let mut lower = original.clone();
        lower.owned_working_bytes = original
            .owned_working_bytes
            .checked_sub(reserved)
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("parse_agent_working_limit"))?;
        let mut toolbox = code(
            ParseToolbox::new(
                &config.staging_root,
                nonce(&config.nonce)?,
                lower,
                config.toolbox.limits(),
            ),
            "parse_agent_toolbox_construction",
        )?;
        for source in &config.sources {
            code(
                toolbox.register_source(&source.alias, &source.path, source.bytes, &source.sha256),
                "parse_agent_source_pin",
            )?;
        }
        let context = if let Some(ocr) = &config.ocr {
            let context = if let Some(limits) = &ocr.phase_limits {
                code(
                    OcrExecutionContext::with_phase_limits(
                        &ocr.run_id,
                        &ocr.plan_sha256,
                        ocr.original_clock.clone(),
                        limits.clone(),
                    ),
                    "parse_agent_ocr_context",
                )?
            } else {
                code(
                    OcrExecutionContext::with_clock_anchor(
                        &ocr.run_id,
                        &ocr.plan_sha256,
                        ocr.original_clock.clone(),
                        ocr.max_pages,
                    ),
                    "parse_agent_ocr_context",
                )?
            };
            Some(code(
                context.join_existing_original_ledger(&ocr.ledger, &ocr.original_clock),
                "parse_agent_existing_ocr_ledger",
            )?)
        } else {
            None
        };
        let deadline = started
            .checked_add(Duration::from_millis(config.frontend.session_timeout_ms))
            .ok_or_else(|| anyhow!("parse_agent_deadline"))?;
        let session = Self {
            quota: Quota {
                requests: 0,
                wire: 0,
                bounds: config.frontend.clone(),
            },
            config,
            budget: original,
            toolbox,
            context,
            ocr_cleanup: None,
            deadline,
        };
        session.check()?;
        Ok(session)
    }
    fn check(&self) -> Result<()> {
        ensure!(Instant::now() < self.deadline, "parse_agent_timeout");
        if let Some(context) = &self.context {
            code(context.remaining(), "parse_agent_original_deadline")?;
        }
        Ok(())
    }
    fn request_deadline(&self) -> Result<Instant> {
        self.check()?;
        let mut remaining = Duration::from_millis(self.config.frontend.request_timeout_ms);
        if let Some(context) = &self.context {
            remaining = remaining.min(code(context.remaining(), "parse_agent_original_deadline")?);
        }
        Ok(self.deadline.min(
            Instant::now()
                .checked_add(remaining)
                .ok_or_else(|| anyhow!("parse_agent_deadline"))?,
        ))
    }
    fn host(&self, handle: &str) -> Result<OcrHostExecutor> {
        let config = self
            .config
            .ocr
            .as_ref()
            .ok_or_else(|| anyhow!("parse_agent_ocr_configuration_required"))?;
        ensure!(
            config.runtime_profile.image_qualification_accepted(),
            "parse_agent_ocr_qualification_required"
        );
        let prepared_root = code(
            self.toolbox.prepared_staging_root(handle),
            "parse_agent_unknown_handle",
        )?
        .to_path_buf();
        code(
            OcrHostExecutor::new(
                OcrHostConfig {
                    codex_bin: config
                        .codex_bin
                        .to_str()
                        .ok_or_else(|| anyhow!("parse_agent_binary_path"))?
                        .into(),
                    codex_home: config.auth_home.clone(),
                    codex_state_dir: Some(config.state_dir.clone()),
                    prepared_root,
                    raw_results_dir: config.results_root.clone(),
                    runtime_profile: config.runtime_profile.clone(),
                    transport: config.runtime_profile.transport(),
                },
                self.context
                    .as_ref()
                    .ok_or_else(|| anyhow!("parse_agent_ocr_configuration_required"))?
                    .clone(),
            ),
            "parse_agent_ocr_executor",
        )
        .map(|host| match &self.ocr_cleanup {
            Some(cleanup) => host.with_cleanup(cleanup.clone()),
            None => host,
        })
    }
    async fn operation(&mut self, input: &[u8], deadline: Instant) -> Result<Vec<u8>> {
        self.quota.request()?;
        self.check()?;
        ensure!(Instant::now() < deadline, "parse_agent_request_timeout");
        ensure!(
            input.len() <= self.config.frontend.request_bytes && input.last() == Some(&b'\n'),
            "parse_agent_request_framing"
        );
        let request: FrontRequest =
            serde_json::from_slice(input).map_err(|_| anyhow!("parse_agent_request_schema"))?;
        ensure!(
            request.schema == REQUEST_SCHEMA
                && request.id.len() <= 128
                && (1024..=self.config.toolbox.reply_bytes).contains(&request.max_reply_bytes),
            "parse_agent_request_schema"
        );
        let op = request
            .operation
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let id = request.id;
        let cap = request.max_reply_bytes;
        let reply = if op == "execute_ocr" {
            let operation: ExecuteRequest = serde_json::from_value(request.operation)
                .map_err(|_| anyhow!("parse_agent_request_schema"))?;
            ensure!(
                operation.op == "execute_ocr"
                    && hash64(&operation.handle)
                    && hash64(&operation.task_sha256),
                "parse_agent_request_schema"
            );
            let worst = OcrExecutionSummary {
                task_sha256: operation.task_sha256.clone(),
                page: u32::MAX,
                status: gptgrep_parse::OcrResultStatus::Unreadable,
                metadata_validated: true,
            };
            let needed = framed_count(
                &Reply {
                    schema: REPLY_SCHEMA,
                    id: &id,
                    ok: true,
                    data: &worst,
                },
                cap as u64,
                deadline,
            )?;
            self.quota.room(needed)?;
            let executor = self.host(&operation.handle)?;
            let executor = Cancellable {
                host: &executor,
                cancel: OcrCancellation::default(),
            };
            match timeout_at(
                deadline,
                execute_registered(
                    &mut self.toolbox,
                    &operation.handle,
                    &operation.task_sha256,
                    &executor,
                ),
            )
            .await
            {
                Ok(Ok(summary)) => frame(
                    &Reply {
                        schema: REPLY_SCHEMA,
                        id: &id,
                        ok: true,
                        data: &summary,
                    },
                    cap,
                    deadline,
                )?,
                Ok(Err(_)) => error_frame(&id, "operation_failed", cap, deadline)?,
                Err(_) => return Err(anyhow!("parse_agent_request_timeout")),
            }
        } else {
            let operation: ToolboxOperation = serde_json::from_value(request.operation)
                .map_err(|_| anyhow!("parse_agent_request_schema"))?;
            match timeout_at(
                deadline,
                self.toolbox.dispatch(ToolboxRequest {
                    schema: request.schema,
                    id: id.clone(),
                    max_reply_bytes: cap,
                    operation,
                }),
            )
            .await
            {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => error_frame(&id, "operation_failed", cap, deadline)?,
                Err(_) => return Err(anyhow!("parse_agent_request_timeout")),
            }
        };
        self.check()?;
        self.quota.wire(reply.len())?;
        Ok(reply)
    }
    async fn core(&mut self, operation: ToolboxOperation, cap: usize) -> Result<Vec<u8>> {
        self.quota.request()?;
        self.check()?;
        let deadline = self.request_deadline()?;
        let request = ToolboxRequest {
            schema: REQUEST_SCHEMA.into(),
            id: "run".into(),
            max_reply_bytes: cap.min(self.config.toolbox.reply_bytes),
            operation,
        };
        let size = framed_count(
            &request,
            self.config.frontend.request_bytes as u64,
            deadline,
        )?;
        self.quota.wire(size as usize)?;
        let reply = timeout_at(deadline, self.toolbox.dispatch(request))
            .await
            .map_err(|_| anyhow!("parse_agent_request_timeout"))?
            .map_err(|error| error.context("parse_agent_operation_failed"))?;
        self.check()?;
        self.quota.wire(reply.len())?;
        Ok(reply)
    }
    async fn ocr(&mut self, handle: &str, task: &str) -> Result<()> {
        self.ocr_summary(handle, task).await.map(|_| ())
    }
    async fn ocr_summary(&mut self, handle: &str, task: &str) -> Result<OcrExecutionSummary> {
        self.ocr_summary_with_native_crops(handle, task, Vec::new())
            .await
    }
    async fn ocr_summary_with_native_crops(
        &mut self,
        handle: &str,
        task: &str,
        crops: Vec<NativeOcrCrop>,
    ) -> Result<OcrExecutionSummary> {
        self.quota.request()?;
        self.check()?;
        let deadline = self.request_deadline()?;
        let cap = 4096.min(self.config.toolbox.reply_bytes);
        let request_bytes = framed_count(
            &serde_json::json!({"schema":REQUEST_SCHEMA,"id":"run","max_reply_bytes":cap,
            "operation":{"op":"execute_ocr","handle":handle,"task_sha256":task}}),
            self.config.frontend.request_bytes as u64,
            deadline,
        )?;
        self.quota.wire(request_bytes as usize)?;
        self.quota.room(cap as u64)?;
        let host = self.host(handle)?;
        let executor = Cancellable {
            host: &host,
            cancel: OcrCancellation::default(),
        };
        let summary = timeout_at(
            deadline,
            self.toolbox
                .execute_ocr_with_native_crops(handle, task, crops, &executor),
        )
        .await
        .map_err(|_| anyhow!("parse_agent_request_timeout"))?
        .map_err(gptgrep_core::parse_agent::summarize_ocr_failure)?;
        let bytes = framed_count(
            &Reply {
                schema: REPLY_SCHEMA,
                id: "run",
                ok: true,
                data: &summary,
            },
            cap as u64,
            deadline,
        )?;
        self.quota.wire(bytes as usize)?;
        self.check()?;
        Ok(summary)
    }
    /// The same invocation quota and clock cover creation and its portable
    /// metadata. The caller already holds the shared document-working lease.
    fn native_crop(
        &mut self,
        handle: &str,
        parent: &str,
        bbox: BoundingBox,
    ) -> Result<NativeOcrCrop> {
        self.quota.request()?;
        self.check()?;
        let deadline = self.request_deadline()?;
        let request = serde_json::json!({"schema":REQUEST_SCHEMA,"operation":{
            "op":"crop","handle":handle,"artifact_id":parent,"bbox":bbox}});
        let request_bytes = framed_count(
            &request,
            self.config.frontend.request_bytes as u64,
            deadline,
        )?;
        self.quota.wire(request_bytes as usize)?;
        self.quota.room(16_385)?;
        let crop = code(
            self.toolbox.create_native_crop(handle, parent, bbox),
            "document_native_crop",
        )?;
        let bytes = framed_count(crop.proof(), 16_385, deadline)?;
        self.quota.wire(bytes as usize)?;
        self.check()?;
        ensure!(Instant::now() < deadline, "parse_agent_request_timeout");
        Ok(crop)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontRequest {
    schema: String,
    id: String,
    #[serde(default = "default_reply")]
    max_reply_bytes: usize,
    operation: serde_json::Value,
}
fn default_reply() -> usize {
    262_144
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    op: String,
    handle: String,
    task_sha256: String,
}
struct Cancellable<'a> {
    host: &'a OcrHostExecutor,
    cancel: OcrCancellation,
}
impl OcrExecutor for Cancellable<'_> {
    fn execute<'a>(
        &'a self,
        task: OcrTask,
    ) -> Pin<Box<dyn Future<Output = Result<OcrPageResult>> + Send + 'a>> {
        Box::pin(self.host.execute_cancellable(task, self.cancel.clone()))
    }
    fn execute_with_native_crops<'a>(
        &'a self,
        task: OcrTask,
        crops: Vec<NativeOcrCrop>,
    ) -> Pin<Box<dyn Future<Output = Result<OcrPageResult>> + Send + 'a>> {
        Box::pin(
            self.host
                .execute_cancellable_with_native_crops(task, crops, self.cancel.clone()),
        )
    }
}
async fn execute_registered(
    toolbox: &mut ParseToolbox,
    handle: &str,
    task: &str,
    executor: &dyn OcrExecutor,
) -> Result<OcrExecutionSummary> {
    toolbox.execute_ocr(handle, task, executor).await
}

struct CountWriter {
    used: u64,
    cap: u64,
    deadline: Instant,
}
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other("parse_agent_timeout"));
        }
        self.used = self
            .used
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= self.cap)
            .ok_or_else(|| io::Error::other("parse_agent_serialized_limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn framed_count(value: &impl Serialize, cap: u64, deadline: Instant) -> Result<u64> {
    let mut writer = CountWriter {
        used: 0,
        cap,
        deadline,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow!("parse_agent_serialized_limit"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| anyhow!("parse_agent_serialized_limit"))?;
    Ok(writer.used)
}
struct VecWriter {
    bytes: Vec<u8>,
    cap: usize,
    deadline: Instant,
}
impl Write for VecWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline
            || bytes.len() > self.cap.saturating_sub(self.bytes.len())
        {
            return Err(io::Error::other("parse_agent_serialized_limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn frame(value: &impl Serialize, cap: usize, deadline: Instant) -> Result<Vec<u8>> {
    let mut writer = VecWriter {
        bytes: Vec::new(),
        cap,
        deadline,
    };
    writer
        .bytes
        .try_reserve_exact(cap)
        .map_err(|_| anyhow!("parse_agent_reply_allocation"))?;
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow!("parse_agent_serialized_limit"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| anyhow!("parse_agent_serialized_limit"))?;
    Ok(writer.bytes)
}
#[derive(Serialize)]
struct Reply<'a, T: Serialize> {
    schema: &'static str,
    id: &'a str,
    ok: bool,
    data: &'a T,
}
fn error_frame(id: &str, value: &'static str, cap: usize, deadline: Instant) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Error<'a> {
        schema: &'static str,
        id: &'a str,
        ok: bool,
        error: ErrorCode,
    }
    #[derive(Serialize)]
    struct ErrorCode {
        code: &'static str,
    }
    frame(
        &Error {
            schema: REPLY_SCHEMA,
            id,
            ok: false,
            error: ErrorCode { code: value },
        },
        cap,
        deadline,
    )
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    quota: &mut Quota,
    deadline: Instant,
) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(quota.bounds.request_bytes)
        .map_err(|_| anyhow!("parse_agent_request_allocation"))?;
    // Read one byte beyond the negotiated count/line boundary only to distinguish
    // EOF from a new forbidden submission. No overflow byte is retained.
    loop {
        ensure!(Instant::now() < deadline, "parse_agent_timeout");
        let mut byte = [0];
        let got = timeout_at(deadline, reader.read(&mut byte))
            .await
            .map_err(|_| anyhow!("parse_agent_timeout"))?
            .map_err(|_| anyhow!("parse_agent_input"))?;
        if got == 0 {
            ensure!(frame.is_empty(), "parse_agent_unterminated_request");
            return Ok(None);
        }
        quota.wire(1)?;
        ensure!(
            quota.requests < quota.bounds.max_requests,
            "parse_agent_request_count"
        );
        ensure!(
            frame.len() < quota.bounds.request_bytes,
            "parse_agent_request_limit"
        );
        frame.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(Some(frame));
        }
    }
}
async fn serve_io<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut session: Session,
    reader: &mut R,
    writer: &mut W,
) -> Result<i32> {
    loop {
        let deadline = session.request_deadline()?;
        let Some(input) = read_frame(reader, &mut session.quota, deadline).await? else {
            session.check()?;
            return Ok(0);
        };
        let reply = session.operation(&input, deadline).await?;
        timeout_at(deadline, writer.write_all(&reply))
            .await
            .map_err(|_| anyhow!("parse_agent_timeout"))?
            .map_err(|_| anyhow!("parse_agent_output"))?;
        timeout_at(deadline, writer.flush())
            .await
            .map_err(|_| anyhow!("parse_agent_timeout"))?
            .map_err(|_| anyhow!("parse_agent_output"))?;
    }
}

/// Serve a pinned control through this process's explicitly owned stdIO only.
/// The enclosing owner supplies exclusive caller/stdio and storage authority.
pub async fn serve(config_path: &Path, expected_sha: &str) -> Result<i32> {
    let started = Instant::now();
    let config = control(config_path, expected_sha)?;
    let session = Session::new(config, started)?;
    #[cfg(unix)]
    {
        let mut input = unix::Stream::new(0, false)?;
        let mut output = unix::Stream::new(1, true)?;
        serve_io(session, &mut input, &mut output).await
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        Err(anyhow!("parse_agent_unix_custody_required"))
    }
}

/// One explicit document-agent turn. Its private outcome is evidence for the
/// enclosing owner, not a release or native qualification. Root owns exit/error
/// rendering and any private outcome sink; the product is written only on pass.
pub async fn agent(
    config_path: &Path,
    expected_sha: &str,
    document_control_path: &Path,
    document_control_sha: &str,
    source_alias: &str,
    output_path: &Path,
) -> Result<PreparedDocumentAgent> {
    document_agent::agent(
        config_path,
        expected_sha,
        document_control_path,
        document_control_sha,
        source_alias,
        output_path,
    )
    .await
}
pub use document_agent::{
    DocumentAgentInvocation, DocumentImageQualificationEvidence, PreparedDocumentAgent,
};
/// Explicit private qualification purpose; never product publication/indexing.
pub async fn qualify_image_source(
    config_path: &Path,
    expected_sha: &str,
    document_control_path: &Path,
    document_control_sha: &str,
    source_alias: &str,
    purpose_path: &Path,
    purpose_sha: &str,
) -> Result<DocumentImageQualificationEvidence> {
    document_agent::qualify_image_source(
        config_path,
        expected_sha,
        document_control_path,
        document_control_sha,
        source_alias,
        purpose_path,
        purpose_sha,
    )
    .await
}
#[path = "parse_agent_document.rs"]
mod document_agent;

#[derive(Deserialize)]
struct StageReply {
    ok: bool,
    data: StageSummary,
}
#[derive(Deserialize)]
struct StageSummary {
    handle: String,
    original_pages: Option<usize>,
    required_ocr_complete: Option<bool>,
    ocr_blocker_count: Option<usize>,
}
#[derive(Deserialize)]
struct TaskReply {
    ok: bool,
    data: TaskRows,
}
#[derive(Deserialize)]
struct TaskRows {
    kind: String,
    total: usize,
    start: usize,
    rows: Vec<TaskIdentity>,
    next_cursor: Option<String>,
}
#[derive(Deserialize)]
struct TaskIdentity {
    task_sha256: String,
    page: u32,
}
#[derive(Serialize)]
struct Product<'a> {
    schema: &'static str,
    invocation_id: &'a str,
    mode: OcrMode,
    complete_document_traversal: bool,
    required_ocr_complete: bool,
    product: &'a RichParsedDocument,
}

/// Deterministic complete stage traversal. This is not index publication or a
/// generation acceptance. Auto-mode incomplete OCR returns two without output.
pub async fn run(
    config_path: &Path,
    expected_sha: &str,
    source_alias: &str,
    mode: OcrMode,
    output_path: &Path,
) -> Result<i32> {
    match run_inner(config_path, expected_sha, source_alias, mode, output_path).await {
        Err(_) if mode == OcrMode::Auto => Ok(2),
        outcome => outcome,
    }
}
async fn run_inner(
    config_path: &Path,
    expected_sha: &str,
    source_alias: &str,
    mode: OcrMode,
    output_path: &Path,
) -> Result<i32> {
    let started = Instant::now();
    let config = control(config_path, expected_sha)?;
    ensure!(
        source_alias.len() <= 128
            && config
                .sources
                .iter()
                .any(|source| source.alias == source_alias),
        "parse_agent_source_alias"
    );
    let mut session = Session::new(config, started)?;
    code(unix_output_metadata(output_path), "parse_agent_output_path")?;
    let prepare = session
        .core(
            ToolboxOperation::Prepare {
                source: source_alias.into(),
            },
            4096,
        )
        .await?;
    let prepared: StageReply =
        serde_json::from_slice(&prepare).map_err(|_| anyhow!("parse_agent_stage_reply"))?;
    ensure!(
        prepared.ok && hash64(&prepared.data.handle),
        "parse_agent_stage_reply"
    );
    let handle = prepared.data.handle;
    let native = session
        .core(
            ToolboxOperation::ExtractNative {
                handle: handle.clone(),
                page_window: session.config.native.page_window,
                dpi: session.config.native.dpi,
                render_pages: session.config.native.render_pages,
            },
            4096,
        )
        .await?;
    let native: StageReply =
        serde_json::from_slice(&native).map_err(|_| anyhow!("parse_agent_stage_reply"))?;
    ensure!(
        native.ok && native.data.handle == handle && native.data.original_pages.is_some(),
        "parse_agent_stage_reply"
    );
    if mode == OcrMode::Auto {
        if native.data.ocr_blocker_count.unwrap_or(usize::MAX) != 0 {
            return Ok(2);
        }
        let mut cursor = None;
        let mut tasks: Vec<(u32, String)> = Vec::new();
        let mut expected = None;
        loop {
            let bytes = session
                .core(
                    ToolboxOperation::InspectOcrPlan {
                        handle: handle.clone(),
                        section: OcrPlanSection::Tasks,
                        cursor,
                        page_size: 1,
                    },
                    session.config.toolbox.reply_bytes.min(TASK_REPLY),
                )
                .await?;
            // Project only task identity; ignored native/layout/image members do
            // not allocate a whole serde_json::Value or a 4 MiB clone.
            let reply: TaskReply =
                serde_json::from_slice(&bytes).map_err(|_| anyhow!("parse_agent_plan_reply"))?;
            ensure!(
                reply.ok
                    && reply.data.kind == "ocr_tasks"
                    && reply.data.start == tasks.len()
                    && reply.data.rows.len() <= 1,
                "parse_agent_plan_reply"
            );
            ensure!(
                expected.is_none_or(|n| n == reply.data.total),
                "parse_agent_plan_reply"
            );
            expected = Some(reply.data.total);
            if reply.data.total > 0 {
                let Some(ocr) = &session.config.ocr else {
                    return Ok(2);
                };
                let snapshot = code(
                    session
                        .context
                        .as_ref()
                        .ok_or_else(|| anyhow!("parse_agent_ocr_context"))?
                        .snapshot(),
                    "parse_agent_ocr_budget",
                )?;
                if reply.data.total > ocr.max_pages
                    || reply.data.total
                        > snapshot
                            .max_admissions
                            .saturating_sub(snapshot.charged_admissions)
                {
                    return Ok(2);
                }
            }
            for task in reply.data.rows {
                ensure!(
                    hash64(&task.task_sha256)
                        && task.page > 0
                        && !tasks.iter().any(|(_, digest)| digest == &task.task_sha256),
                    "parse_agent_plan_reply"
                );
                tasks.push((task.page, task.task_sha256));
            }
            cursor = reply.data.next_cursor;
            if cursor.is_none() {
                ensure!(tasks.len() == reply.data.total, "parse_agent_plan_reply");
                break;
            }
            ensure!(
                hash64(cursor.as_ref().expect("cursor present")) && tasks.len() < reply.data.total,
                "parse_agent_plan_reply"
            );
        }
        for (_, task) in tasks {
            if session.ocr(&handle, &task).await.is_err() {
                return Ok(2);
            }
        }
    }
    let finalized = session
        .core(
            ToolboxOperation::Finalize {
                handle: handle.clone(),
                mode,
            },
            4096,
        )
        .await;
    let finalized = match finalized {
        Ok(reply) => reply,
        Err(_) if mode == OcrMode::Auto => return Ok(2),
        Err(error) => return Err(error),
    };
    let summary: StageReply =
        serde_json::from_slice(&finalized).map_err(|_| anyhow!("parse_agent_stage_reply"))?;
    ensure!(
        summary.ok && summary.data.handle == handle,
        "parse_agent_stage_reply"
    );
    if mode == OcrMode::Auto && summary.data.required_ocr_complete != Some(true) {
        return Ok(2);
    }
    write_product(&mut session, &handle, mode, output_path)?;
    Ok(0)
}
fn unix_output_metadata(path: &Path) -> Result<()> {
    lexical(path)?;
    ensure!(!credential_shaped(path), "parse_agent_credential_path");
    directory(
        path.parent()
            .ok_or_else(|| anyhow!("parse_agent_output_path"))?,
    )?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(anyhow!("parse_agent_output_not_fresh")),
    }
}
fn write_product(session: &mut Session, handle: &str, mode: OcrMode, path: &Path) -> Result<()> {
    session.check()?;
    let deadline = session.request_deadline()?;
    let document = code(
        session.toolbox.finalized_document(handle),
        "parse_agent_source_freshness",
    )?;
    let complete = !document
        .pages
        .iter()
        .any(|page| page.coverage == gptgrep_parse::CoverageState::RequiredOcr);
    ensure!(
        mode == OcrMode::Off || complete,
        "parse_agent_required_ocr_incomplete"
    );
    let product = Product {
        schema: PRODUCT_SCHEMA,
        invocation_id: &session.config.invocation_id,
        mode,
        complete_document_traversal: true,
        required_ocr_complete: complete,
        product: document,
    };
    let bytes = framed_count(
        &product,
        session.config.frontend.output_file_bytes,
        deadline,
    )?;
    session.quota.room(bytes)?;
    // Reserve before any create/write. Failures retain the complete reservation.
    code(
        session.budget.reserve_retained_staging(bytes),
        "parse_agent_output_staging_limit",
    )?;
    session.quota.wire(bytes as usize)?;
    #[cfg(unix)]
    {
        let mut output = unix::FreshOutput::new(path)?;
        let mut writer = ProductWriter {
            file: &mut output.file,
            used: 0,
            cap: bytes,
            deadline,
        };
        serde_json::to_writer(&mut writer, &product)
            .map_err(|_| anyhow!("parse_agent_product_write"))?;
        writer
            .write_all(b"\n")
            .map_err(|_| anyhow!("parse_agent_product_write"))?;
        ensure!(writer.used == bytes, "parse_agent_product_count_changed");
        output
            .file
            .sync_all()
            .map_err(|_| anyhow!("parse_agent_product_sync"))?;
        output.check(bytes)?;
        code(
            session.toolbox.finalized_document(handle),
            "parse_agent_source_freshness",
        )?;
        session.check()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, product, bytes);
        Err(anyhow!("parse_agent_unix_custody_required"))
    }
}
struct ProductWriter<'a> {
    file: &'a mut File,
    used: u64,
    cap: u64,
    deadline: Instant,
}
impl Write for ProductWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline
            || self
                .used
                .checked_add(bytes.len() as u64)
                .is_none_or(|n| n > self.cap)
        {
            return Err(io::Error::other("parse_agent_product_limit"));
        }
        let written = self.file.write(bytes)?;
        self.used += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    };
    use std::task::{Context, Poll};
    use tokio::io::{ReadBuf, unix::AsyncFd};

    enum Readiness {
        Async(AsyncFd<File>),
        Regular(File),
    }
    pub(super) struct Stream {
        readiness: Readiness,
        original_flags: i32,
    }
    impl Stream {
        pub(super) fn new(fd: i32, _write: bool) -> Result<Self> {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(flags >= 0, "parse_agent_stdio_descriptor");
            let owned = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            ensure!(owned >= 0, "parse_agent_stdio_descriptor");
            let file = unsafe { File::from_raw_fd(owned) };
            let metadata = file
                .metadata()
                .map_err(|_| anyhow!("parse_agent_stdio_descriptor"))?;
            let changed = unsafe { libc::fcntl(owned, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            ensure!(changed == 0, "parse_agent_stdio_descriptor");
            let readiness = if metadata.is_file() {
                Readiness::Regular(file)
            } else {
                match AsyncFd::new(file) {
                    Ok(file) => Readiness::Async(file),
                    Err(_) => {
                        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
                        return Err(anyhow!("parse_agent_stdio_registration"));
                    }
                }
            };
            Ok(Self {
                readiness,
                original_flags: flags,
            })
        }
        fn fd(&self) -> i32 {
            match &self.readiness {
                Readiness::Async(file) => file.get_ref().as_raw_fd(),
                Readiness::Regular(file) => file.as_raw_fd(),
            }
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe { libc::fcntl(self.fd(), libc::F_SETFL, self.original_flags) };
        }
    }
    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let read = |file: &File, buffer: &mut ReadBuf<'_>| {
                let out = buffer.initialize_unfilled();
                let n = unsafe {
                    libc::read(
                        file.as_raw_fd(),
                        out.as_mut_ptr().cast(),
                        out.len().min(16_384),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    buffer.advance(n as usize);
                    Ok(())
                }
            };
            match &mut this.readiness {
                Readiness::Regular(file) => Poll::Ready(read(file, buffer)),
                Readiness::Async(file) => loop {
                    let mut guard = match file.poll_read_ready(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Ready(Ok(guard)) => guard,
                    };
                    match guard.try_io(|fd| read(fd.get_ref(), buffer)) {
                        Ok(value) => return Poll::Ready(value),
                        Err(_) => continue,
                    }
                },
            }
        }
    }
    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let write = |file: &File| {
                let n = unsafe {
                    libc::write(
                        file.as_raw_fd(),
                        buffer.as_ptr().cast(),
                        buffer.len().min(16_384),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            };
            match &mut this.readiness {
                Readiness::Regular(file) => Poll::Ready(write(file)),
                Readiness::Async(file) => loop {
                    let mut guard = match file.poll_write_ready(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Ready(Ok(guard)) => guard,
                    };
                    match guard.try_io(|fd| write(fd.get_ref())) {
                        Ok(value) => return Poll::Ready(value),
                        Err(_) => continue,
                    }
                },
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    pub(super) struct FreshOutput {
        pub(super) file: File,
        parent: File,
        parent_path: PathBuf,
        leaf: CString,
        parent_identity: (u64, u64),
    }
    impl FreshOutput {
        pub(super) fn new(path: &Path) -> Result<Self> {
            unix_output_metadata(path)?;
            let parent_path = path
                .parent()
                .ok_or_else(|| anyhow!("parse_agent_output_path"))?
                .to_path_buf();
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let parent = options
                .open(&parent_path)
                .map_err(|_| anyhow!("parse_agent_output_parent"))?;
            let metadata = parent
                .metadata()
                .map_err(|_| anyhow!("parse_agent_output_parent"))?;
            let parent_identity = (metadata.dev(), metadata.ino());
            let leaf = CString::new(
                path.file_name()
                    .ok_or_else(|| anyhow!("parse_agent_output_path"))?
                    .as_bytes(),
            )
            .map_err(|_| anyhow!("parse_agent_output_path"))?;
            ensure!(
                fs::canonicalize(&parent_path).map_err(|_| anyhow!("parse_agent_output_parent"))?
                    == parent_path,
                "parse_agent_output_parent"
            );
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            ensure!(fd >= 0, "parse_agent_output_not_fresh");
            Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
                parent,
                parent_path,
                leaf,
                parent_identity,
            })
        }
        #[allow(clippy::unnecessary_cast)] // stat integer widths differ across Unix ABIs.
        pub(super) fn check(&self, bytes: u64) -> Result<()> {
            let parent = fs::symlink_metadata(&self.parent_path)
                .map_err(|_| anyhow!("parse_agent_output_changed"))?;
            let held = self
                .parent
                .metadata()
                .map_err(|_| anyhow!("parse_agent_output_changed"))?;
            ensure!(
                parent.is_dir()
                    && !parent.file_type().is_symlink()
                    && (parent.dev(), parent.ino()) == self.parent_identity
                    && (held.dev(), held.ino()) == self.parent_identity
                    && fs::canonicalize(&self.parent_path)
                        .map_err(|_| anyhow!("parse_agent_output_changed"))?
                        == self.parent_path,
                "parse_agent_output_changed"
            );
            let metadata = self
                .file
                .metadata()
                .map_err(|_| anyhow!("parse_agent_output_changed"))?;
            let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
            let status = unsafe {
                libc::fstatat(
                    self.parent.as_raw_fd(),
                    self.leaf.as_ptr(),
                    entry.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            ensure!(status == 0, "parse_agent_output_changed");
            let entry = unsafe { entry.assume_init() };
            ensure!(
                metadata.is_file()
                    && metadata.len() == bytes
                    && entry.st_mode & libc::S_IFMT == libc::S_IFREG
                    && entry.st_dev as u64 == metadata.dev()
                    && entry.st_ino as u64 == metadata.ino(),
                "parse_agent_output_changed"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "parse_agent_tests.rs"]
mod tests;
