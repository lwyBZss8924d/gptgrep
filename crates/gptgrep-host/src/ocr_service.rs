//! Explicit foreground HTTP OCR. Image-only compatibility never authorizes a generation.
//! Construction validates caller-selected custody before `serve` can bind loopback.
use anyhow::{Result, anyhow, ensure};
use bytes::Bytes;
use gptgrep_jev::RunClockAnchor;
use gptgrep_parse::{
    AffineTransform, ArtifactKind, ArtifactRef, CompatibilityOcrTask, OcrImage, OcrProfile,
    OcrTask, OcrTaskBudget, ResourceBudget, artifact_id, compatibility_task_digest,
    validate_ocr_result_metadata, validate_ocr_task_metadata,
};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode, body::Body, header};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::{
    CompatibilityRuntimeProfile, OcrCancellation, OcrExecutionContext, OcrHostConfig,
    OcrHostExecutor,
    ocr::{PreparedPage, hash, open_directory, open_relative, verify_directory},
};

const TASK_BYTES: usize = 131_072;
const MULTIPART_OVERHEAD: usize = 16_384;
const RESPONSE_BYTES: usize = 262_144;
const CATALOG_BYTES: usize = 8_388_608;
const CLAIM_BYTES: usize = 1_048_576;
pub(crate) mod working;
use working::{LeasedResponse, WorkingBudget, WorkingLease};
// Retained roots/profiles, two headers, and eight bounded HTTP/error buffers.
const SERVICE_WORKING: u64 = 1_048_576;
const CATALOG_FACTOR: u64 = 64;
const BODY_FACTOR: u64 = 40;
const SERVICE_USAGE_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub struct ForegroundOcrOptions {
    pub bind: SocketAddr,
    /// Original raw sources for full-task custody. Compatibility does not use these.
    pub source_root: PathBuf,
    /// Existing private directories, distinct from sources, prepared assets and native home.
    pub spool_root: PathBuf,
    pub claim_root: PathBuf,
    pub resources: ResourceBudget,
    pub max_connections: usize,
    pub compatibility: Option<CompatibilityRuntimeProfile>,
    pub token_env_name: String,
}
impl ForegroundOcrOptions {
    pub fn new(source_root: PathBuf, spool_root: PathBuf, claim_root: PathBuf) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8828)),
            source_root,
            spool_root,
            claim_root,
            resources: ResourceBudget::default(),
            max_connections: 8,
            compatibility: None,
            token_env_name: "GPTGREP_OCR_SERVE_TOKEN".into(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.bind.ip().is_loopback() && self.bind.port() > 0,
            "ocr_service_loopback_required"
        );
        ensure!(
            (1..=8).contains(&self.max_connections),
            "ocr_service_connection_limit"
        );
        self.resources.validate()?;
        validate_token_env_name(&self.token_env_name)?;
        if let Some(profile) = &self.compatibility {
            profile.validate()?;
        }
        Ok(())
    }
}

/// A source-only allowlist. Caller-supplied wire paths are never read or restored.
/// Prepare this synchronous custody boundary on a blocking worker when called from async code.
pub struct PreparedOcrTaskCatalog {
    source_root: PathBuf,
    prepared_root: PathBuf,
    tasks: BTreeMap<String, Arc<OcrTask>>,
    working: WorkingBudget,
    _retained: Arc<WorkingLease>,
    _control: Option<Arc<WorkingLease>>,
    _context: Option<Arc<WorkingLease>>,
}
impl PreparedOcrTaskCatalog {
    pub fn prepare(
        source_root: &Path,
        prepared_root: &Path,
        tasks: Vec<OcrTask>,
        resources: &ResourceBudget,
    ) -> Result<Self> {
        ensure!(tasks.len() <= 1024, "ocr_service_catalog_count");
        resources.validate()?;
        let working = WorkingBudget::new(resources.owned_working_bytes);
        let mut total = 65_536u64;
        for task in &tasks {
            total = total
                .checked_add(catalog_task_bound(task)?)
                .ok_or(ServiceFailure::WorkingLimit)?;
        }
        let retained = Arc::new(working.reserve(total)?);
        Self::prepare_admitted(
            source_root,
            prepared_root,
            tasks,
            resources,
            working,
            retained,
        )
    }
    fn prepare_admitted(
        source_root: &Path,
        prepared_root: &Path,
        tasks: Vec<OcrTask>,
        resources: &ResourceBudget,
        working: WorkingBudget,
        retained: Arc<WorkingLease>,
    ) -> Result<Self> {
        ensure!(tasks.len() <= 1024, "ocr_service_catalog_count");
        resources.validate()?;
        let source_root = source_root.canonicalize()?;
        let prepared_root = prepared_root.canonicalize()?;
        let source = open_directory(&source_root)?;
        let images = open_directory(&prepared_root)?;
        let mut map = BTreeMap::new();
        let mut total = 0usize;
        for mut task in tasks {
            let serialized = bounded_json(&task, TASK_BYTES)?;
            total = total
                .checked_add(serialized.len())
                .ok_or_else(|| anyhow!("ocr_service_catalog_limit"))?;
            ensure!(total <= CATALOG_BYTES, "ocr_service_catalog_limit");
            validate_ocr_task_metadata(&task)?;
            let relative = task
                .source_path
                .strip_prefix(&source_root)
                .map_err(|_| anyhow!("ocr_service_source_scope"))?;
            let file = open_relative(&source, relative)?;
            ensure!(file.metadata()?.is_file(), "ocr_service_source_scope");
            for image in &task.images {
                ensure!(
                    image.path == prepared_root.join(&image.artifact.relative_path),
                    "ocr_service_image_scope"
                );
                let file = open_relative(&images, &image.artifact.relative_path)?;
                ensure!(file.metadata()?.is_file(), "ocr_service_image_scope");
            }
            task.budget.resources.adopt_shared_staging(resources)?;
            // Native image validation uses the same aggregate budget, before any
            // encoded/decode/color buffer allocation or private task clone.
            let _custody = working.reserve(image_working_bound(&task.images, resources)?)?;
            PreparedPage::validate_service_custody(task.clone(), &source_root, &prepared_root)?;
            ensure!(
                map.insert(task.task_sha256.clone(), Arc::new(task))
                    .is_none(),
                "ocr_service_duplicate_task"
            );
        }
        verify_directory(&source_root, &source)?;
        verify_directory(&prepared_root, &images)?;
        Ok(Self {
            source_root,
            prepared_root,
            tasks: map,
            working,
            _retained: retained,
            _control: None,
            _context: None,
        })
    }
    fn restore(&self, wire: OcrTask) -> Result<OcrTask> {
        validate_ocr_task_metadata(&wire)?;
        let original = self
            .tasks
            .get(&wire.task_sha256)
            .ok_or_else(|| anyhow!("ocr_service_unknown_task"))?;
        ensure!(
            bounded_json(&wire, TASK_BYTES)? == bounded_json(original.as_ref(), TASK_BYTES)?,
            "ocr_service_task_changed"
        );
        Ok(original.as_ref().clone())
    }
}

/// Prepared service has genuine executor/context custody and no arbitrary backend factory.
/// `serve` is an explicit runtime effect; construction alone does not bind a socket.
pub struct PreparedForegroundOcr {
    service: Arc<Service>,
    bind: SocketAddr,
    max_connections: usize,
}
struct Service {
    catalog: PreparedOcrTaskCatalog,
    executor: OcrHostExecutor,
    compatibility_executor: Option<OcrHostExecutor>,
    compatibility: Option<CompatibilityRuntimeProfile>,
    resources: ResourceBudget,
    auth: [u8; 32],
    active: Arc<Semaphore>,
    claims: Arc<Claims>,
    spool: Arc<PrivateDirectory>,
    jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    stop: OcrCancellation,
    working: WorkingBudget,
    _retained: Arc<WorkingLease>,
    #[cfg(test)]
    custody_gate: Option<Arc<tests::CustodyGate>>,
}

/// Caller supplies a real previously joined finite context and its full original
/// anchor. No clock is captured, OCR admission ledger initialized or model invoked here.
/// The caller-selected private request journal is attached/created here.
#[allow(clippy::too_many_arguments)]
pub fn prepare_foreground_ocr(
    options: ForegroundOcrOptions,
    config: OcrHostConfig,
    context: OcrExecutionContext,
    original: &RunClockAnchor,
    catalog: PreparedOcrTaskCatalog,
    token: &[u8],
) -> Result<PreparedForegroundOcr> {
    options.validate()?;
    ensure!(
        (32..=256).contains(&token.len()) && token.iter().all(|b| b.is_ascii_graphic()),
        "ocr_service_token_shape"
    );
    ensure!(
        context.durable() && context.ledger_clock_anchor()?.as_ref() == Some(original),
        "ocr_service_original_durable_anchor_required"
    );
    let (phase, max_requests) = context.phase_binding()?;
    ensure!(
        catalog.tasks.len() <= max_requests,
        "ocr_service_catalog_phase_limit"
    );
    ensure!(
        catalog.source_root == options.source_root.canonicalize()?
            && catalog.prepared_root == config.prepared_root.canonicalize()?,
        "ocr_service_catalog_root_binding"
    );
    ensure!(
        config
            .runtime_profile
            .accepted_image_qualification_sha256
            .is_some()
            || catalog.tasks.is_empty(),
        "ocr_service_full_task_qualification_required"
    );
    let spool = Arc::new(PrivateDirectory::existing(&options.spool_root)?);
    let claim_parent = PrivateDirectory::existing(&options.claim_root)?;
    let actor_home = config.codex_home.canonicalize()?;
    let raw = PrivateDirectory::existing(&config.raw_results_dir)?;
    let native_state = crate::native_state_paths(config.codex_state_dir.as_deref())?;
    // New private retained writes cannot overlap source/native-account/state namespaces.
    for path in [&spool.path, &claim_parent.path, &raw.path] {
        for reserved in [&catalog.source_root, &catalog.prepared_root, &actor_home]
            .into_iter()
            .chain(native_state.as_ref().map(|(state, _)| state))
        {
            ensure!(
                !path.starts_with(reserved) && !reserved.starts_with(path),
                "ocr_service_namespace_overlap"
            );
        }
    }
    let outputs = [&spool.path, &claim_parent.path, &raw.path];
    for (index, path) in outputs.iter().enumerate() {
        for other in &outputs[index + 1..] {
            ensure!(
                !path.starts_with(other) && !other.starts_with(path),
                "ocr_service_namespace_overlap"
            );
        }
    }
    ensure!(
        catalog.working.limit() == options.resources.owned_working_bytes,
        "ocr_service_working_budget_binding"
    );
    let working = catalog.working.clone();
    let context_bound = context.service_working_bound()?;
    let context_extra = if let Some(share) = &catalog._context {
        ensure!(
            share.bytes() == context_bound,
            "ocr_service_context_working_binding"
        );
        0
    } else {
        context_bound
    };
    let retained = Arc::new(
        working.reserve(SERVICE_WORKING + claim_working_bound(max_requests)? + context_extra)?,
    );
    let claims = Arc::new(Claims::attach(
        claim_parent,
        &phase,
        max_requests,
        &options.resources,
    )?);
    let executor = OcrHostExecutor::new(config.clone(), context.clone())?
        .with_service_roots(&catalog.source_root)?
        .with_service_working(working.clone())?
        .with_service_secret(&options.token_env_name)?;
    let compatibility_executor = if let Some(profile) = &options.compatibility {
        ensure!(
            profile.accepted_qualification_sha256.is_some(),
            "ocr_compatibility_qualification_required"
        );
        ensure!(
            profile.runtime.sha256()? == config.runtime_profile.sha256()?,
            "ocr_compatibility_runtime_binding"
        );
        let mut compat_config = config;
        compat_config.prepared_root = spool.path.clone();
        Some(
            OcrHostExecutor::new(compat_config, context)?
                .with_service_roots(&spool.path)?
                .with_service_working(working.clone())?
                .with_service_secret(&options.token_env_name)?,
        )
    } else {
        None
    };
    let active_limit = 2;
    Ok(PreparedForegroundOcr {
        bind: options.bind,
        max_connections: options.max_connections,
        service: Arc::new(Service {
            catalog,
            executor,
            compatibility_executor,
            compatibility: options.compatibility,
            resources: options.resources,
            auth: token_digest(token),
            active: Arc::new(Semaphore::new(active_limit)),
            claims,
            spool,
            jobs: Mutex::new(Vec::new()),
            stop: OcrCancellation::default(),
            working,
            _retained: retained,
            #[cfg(test)]
            custody_gate: None,
        }),
    })
}

impl PreparedForegroundOcr {
    /// Explicit foreground loopback listener. Shutdown cancels admissions, then
    /// awaits every owned request's stop/reap/drain. No detached native children.
    pub async fn serve(self, shutdown: OcrCancellation) -> Result<()> {
        let now = tokio::time::Instant::now();
        let deadline = now + self.service.executor.service_remaining()?;
        let listener = tokio::time::timeout_at(deadline, tokio::net::TcpListener::bind(self.bind))
            .await
            .map_err(|_| anyhow!("ocr_service_phase_expired"))?
            .map_err(|_| anyhow!("ocr_service_listener_unavailable"))?;
        let slots = Arc::new(Semaphore::new(self.max_connections));
        let mut connections = tokio::task::JoinSet::new();
        let mut failure = None;
        loop {
            tokio::select! {
                _=shutdown.wait_service()=>break,
                _=tokio::time::sleep_until(deadline)=>break,
                _=self.service.stop.wait_service()=>break,
                Some(result)=connections.join_next(),if !connections.is_empty()=>{
                    if result.is_err(){failure=Some(anyhow!("ocr_service_connection_task_failed"));break;}
                }
                accepted=listener.accept()=>{
                    let (stream,peer)=match accepted {Ok(value)=>value,Err(_)=>{failure=Some(anyhow!("ocr_service_accept_failed"));break;}};
                    if !peer.ip().is_loopback(){continue;}
                    let Ok(permit)=slots.clone().try_acquire_owned() else {drop(stream);continue;};
                    let service=self.service.clone();
                    connections.spawn(async move {
                        let _permit=permit;
                        let io=hyper_util::rt::TokioIo::new(stream);
                        let mut builder=hyper::server::conn::http1::Builder::new();
                        builder.keep_alive(false).max_buf_size(16_384).max_headers(32)
                            .timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
                        let handler=hyper::service::service_fn(move |request|{
                            let service=service.clone();
                            async move {Ok::<_,std::convert::Infallible>(service.handle(request).await)}
                        });
                        let _=builder.serve_connection(io,handler).await;
                    });
                }
            }
        }
        drop(listener);
        self.service.stop.cancel();
        // HTTP connections have no authority to outlive their OCR jobs; their
        // futures may be dropped only after requesting cooperative job cleanup.
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        self.service.wait_owned_jobs().await?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
}

impl Service {
    async fn handle<B>(self: &Arc<Self>, request: Request<B>) -> Response<Full<Bytes>>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Debug,
    {
        match self.handle_checked(request).await {
            Ok(response) => response,
            Err(error) => {
                // Closed taxonomy only; no error strings, paths, request contents or secrets.
                let code = error
                    .downcast_ref::<ServiceFailure>()
                    .copied()
                    .unwrap_or(ServiceFailure::Unavailable);
                error_response(code)
            }
        }
    }
    async fn handle_checked<B>(
        self: &Arc<Self>,
        request: Request<B>,
    ) -> Result<Response<Full<Bytes>>>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Debug,
    {
        let (parts, body) = request.into_parts();
        ensure!(
            authorized(&parts.headers, &self.auth),
            ServiceFailure::Unauthorized
        );
        ensure!(
            parts.method == Method::POST
                && parts.uri.query().is_none()
                && !parts.headers.contains_key(header::ORIGIN),
            ServiceFailure::InvalidRequest
        );
        let compatibility = match parts.uri.path() {
            "/v1/ocr/tasks" => false,
            "/ocr" => true,
            _ => return Err(ServiceFailure::NotFound.into()),
        };
        ensure!(!self.stop.is_cancelled(), ServiceFailure::Unavailable);
        self.executor
            .service_remaining()
            .map_err(|_| ServiceFailure::Unavailable)?;
        let permit = self
            .active
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServiceFailure::Busy)?;
        let content = single_header(&parts.headers, header::CONTENT_TYPE)?;
        let body_limit = if compatibility {
            usize::try_from(self.resources.image_file_bytes)? + MULTIPART_OVERHEAD
        } else {
            TASK_BYTES
        };
        let incoming_bound = body.size_hint().upper().unwrap_or(body_limit as u64);
        ensure!(
            incoming_bound <= body_limit as u64,
            ServiceFailure::BodyLimit
        );
        // This share covers collected bytes, unique JSON/multipart and task copies.
        let mut lease = self.working.reserve(
            incoming_bound
                .checked_mul(BODY_FACTOR)
                .ok_or(ServiceFailure::WorkingLimit)?,
        )?;
        if let Some(length) = parts.headers.get(header::CONTENT_LENGTH) {
            let length = length
                .to_str()
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .ok_or(ServiceFailure::InvalidRequest)?;
            ensure!(length <= body_limit, ServiceFailure::BodyLimit);
        }
        let bytes = tokio::time::timeout(
            self.executor
                .service_remaining()?
                .min(Duration::from_secs(15)),
            collect_body(body, usize::try_from(incoming_bound)?),
        )
        .await
        .map_err(|_| ServiceFailure::BodyLimit)??;
        lease.shrink_to(
            (bytes.len() as u64)
                .checked_mul(BODY_FACTOR)
                .ok_or(ServiceFailure::WorkingLimit)?,
        )?;
        let body_working = bytes.len() as u64 * BODY_FACTOR;
        let job = if compatibility {
            ensure!(
                self.compatibility.is_some(),
                ServiceFailure::ModeUnavailable
            );
            let image =
                parse_multipart(&content, &bytes, self.resources.image_file_bytes as usize)?;
            let (width, height) = png_header(&image.bytes, &self.resources)?;
            // Header/count admission precedes the decoder and every later native buffer.
            lease.grow_to(
                body_working
                    .checked_add(job_working_bound(
                        std::slice::from_ref(&(width, height, image.bytes.len() as u64)),
                        &OcrTaskBudget {
                            resources: self.resources.clone(),
                            ..OcrTaskBudget::default()
                        },
                    )?)
                    .ok_or(ServiceFailure::WorkingLimit)?,
            )?;
            Job::Compatibility(Box::new(self.compatibility_task(image)?))
        } else {
            ensure!(
                content == "application/json",
                ServiceFailure::InvalidRequest
            );
            let value = gptgrep_jev::parse_unique_json(&bytes)
                .map_err(|_| ServiceFailure::InvalidRequest)?;
            let envelope: TaskEnvelope =
                serde_json::from_value(value).map_err(|_| ServiceFailure::InvalidRequest)?;
            ensure!(
                envelope.schema_version == "gptgrep.ocr.service.request.v1",
                ServiceFailure::InvalidRequest
            );
            // Only an exact catalog member can nominate dimensions/budget.
            let original = self
                .catalog
                .tasks
                .get(&envelope.task.task_sha256)
                .ok_or(ServiceFailure::TaskBinding)?;
            let images = original
                .images
                .iter()
                .map(|image| (image.width, image.height, image.artifact.bytes))
                .collect::<Vec<_>>();
            lease.grow_to(
                body_working
                    .checked_add(job_working_bound(&images, &original.budget)?)
                    .ok_or(ServiceFailure::WorkingLimit)?,
            )?;
            Job::Full(Box::new(
                self.catalog
                    .restore(envelope.task)
                    .map_err(|_| ServiceFailure::TaskBinding)?,
            ))
        };
        let key = job.key(
            self.compatibility.as_ref(),
            self.executor.runtime_profile_digest()?,
        )?;
        self.executor
            .service_remaining()
            .map_err(|_| ServiceFailure::Unavailable)?;
        let start = self.claims.begin(&key)?;
        ensure!(
            !matches!(start, ClaimStart::Unknown),
            ServiceFailure::ConsumedUnknown
        );
        // Drop raw/decoded request buffers before transferring the conservative
        // share to the owner. No await or custody worker exists before registration.
        drop(bytes);
        self.run_registered(job, key, start, permit, Arc::new(lease))
            .await
    }
    async fn run_registered(
        self: &Arc<Self>,
        job: Job,
        key: String,
        start: ClaimStart,
        permit: tokio::sync::OwnedSemaphorePermit,
        lease: Arc<WorkingLease>,
    ) -> Result<Response<Full<Bytes>>> {
        let cancel = OcrCancellation::default();
        let guard = CancelOnDrop(cancel.clone());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let owner = self.clone();
        {
            // Lock/check before spawning; no owner can be lost to registration failure.
            let mut jobs = self.jobs.lock().map_err(|_| ServiceFailure::Unavailable)?;
            ensure!(!self.stop.is_cancelled(), ServiceFailure::Unavailable);
            jobs.retain(|job| !job.is_finished());
            jobs.push(tokio::spawn(async move {
                let _permit = permit;
                let run = async {
                    // The owner awaits the blocking worker even after HTTP drop.
                    owner.verify_job(&job).await?;
                    ensure!(
                        !cancel.is_cancelled() && !owner.stop.is_cancelled(),
                        ServiceFailure::Unavailable
                    );
                    owner.executor.service_remaining()?;
                    let is_new = matches!(start, ClaimStart::New);
                    let output = match start {
                        ClaimStart::Cached(bytes) => bytes,
                        ClaimStart::New => match &job {
                            Job::Full(task) => bounded_json(
                                &owner
                                    .executor
                                    .execute_cancellable(task.as_ref().clone(), cancel.clone())
                                    .await?,
                                RESPONSE_BYTES,
                            )?,
                            Job::Compatibility(task) => bounded_json(
                                &owner
                                    .compatibility_executor
                                    .as_ref()
                                    .ok_or(ServiceFailure::ModeUnavailable)?
                                    .execute_compatibility(
                                        task.as_ref().clone(),
                                        owner
                                            .compatibility
                                            .as_ref()
                                            .ok_or(ServiceFailure::ModeUnavailable)?,
                                        cancel.clone(),
                                    )
                                    .await?,
                                RESPONSE_BYTES,
                            )?,
                        },
                        ClaimStart::Unknown => return Err(ServiceFailure::ConsumedUnknown.into()),
                    };
                    job.validate_cached(
                        &output,
                        owner.compatibility.as_ref(),
                        &owner.executor.runtime_profile_digest()?,
                    )?;
                    // Final custody can block too: await it under the SAME registered owner.
                    owner.verify_job(&job).await?;
                    ensure!(
                        !cancel.is_cancelled() && !owner.stop.is_cancelled(),
                        ServiceFailure::Unavailable
                    );
                    owner.executor.service_remaining()?;
                    if is_new {
                        owner.claims.complete(&key, &output)?;
                    }
                    job.response(output, lease.clone())
                };
                tokio::pin!(run);
                let result = tokio::select! {
                    result=&mut run=>result,
                    _=owner.stop.wait_service()=>{cancel.cancel();run.await}
                };
                let _ = sender.send(result);
            }));
        }
        let result = receiver.await.map_err(|_| ServiceFailure::Unavailable)?;
        drop(guard);
        result
    }
    async fn wait_owned_jobs(&self) -> Result<()> {
        let jobs = std::mem::take(
            &mut *self
                .jobs
                .lock()
                .map_err(|_| anyhow!("ocr_service_jobs_unavailable"))?,
        );
        let mut failed = false;
        for job in jobs {
            // Never detach later owners because an earlier join failed.
            failed |= job.await.is_err();
        }
        ensure!(!failed, "ocr_service_request_cleanup_unknown");
        Ok(())
    }
    fn compatibility_task(&self, input: MultipartImage) -> Result<CompatibilityOcrTask> {
        let (width, height) = png_dimensions(&input.bytes, &self.resources)?;
        let digest = hash(&input.bytes);
        let policy = crate::ocr_compat::mode_policy_digest()?;
        let filename = format!("{digest}.png");
        self.spool
            .write_once(&filename, &input.bytes, &self.resources)?;
        let id = artifact_id(
            &digest,
            &policy,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &digest,
        )?;
        let mut task = CompatibilityOcrTask {
            schema_version: gptgrep_parse::COMPATIBILITY_MODE.into(),
            task_sha256: String::new(),
            image: OcrImage {
                artifact: ArtifactRef {
                    id,
                    kind: ArtifactKind::Rendition,
                    relative_path: filename.clone().into(),
                    sha256: digest,
                    profile_sha256: policy,
                    bytes: input.bytes.len() as u64,
                    mime: "image/png".into(),
                    page: Some(1),
                    region_id: None,
                },
                path: self.spool.path.join(filename),
                width,
                height,
                page_to_raster: AffineTransform::IDENTITY,
            },
            language: input.language,
            profile: OcrProfile::default(),
            budget: OcrTaskBudget {
                resources: self.resources.clone(),
                ..OcrTaskBudget::default()
            },
        };
        task.task_sha256 = compatibility_task_digest(&task)?;
        task.validate()?;
        Ok(task)
    }
    async fn verify_job(&self, job: &Job) -> Result<()> {
        let job = job.clone();
        let executor = self.executor.clone();
        let compatibility = self.compatibility_executor.clone();
        let spool = self.spool.clone();
        #[cfg(test)]
        let gate = self.custody_gate.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.wait()?;
            }
            match &job {
                Job::Full(task) => executor.check_service_task(task),
                Job::Compatibility(task) => {
                    spool.verify()?;
                    compatibility
                        .as_ref()
                        .ok_or(ServiceFailure::ModeUnavailable)?
                        .check_service_task(&crate::ocr_compat::internal_task(task)?)
                }
            }
        })
        .await
        .map_err(|_| anyhow!("ocr_service_custody_worker_failed"))?
    }
}
struct CancelOnDrop(OcrCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskEnvelope {
    schema_version: String,
    task: OcrTask,
}
#[derive(Clone)]
enum Job {
    Full(Box<OcrTask>),
    Compatibility(Box<CompatibilityOcrTask>),
}
impl Job {
    fn key(
        &self,
        profile: Option<&CompatibilityRuntimeProfile>,
        runtime: String,
    ) -> Result<String> {
        let (task, mode) = match self {
            Self::Full(task) => (task.task_sha256.as_str(), runtime),
            Self::Compatibility(task) => (
                task.task_sha256.as_str(),
                profile.ok_or(ServiceFailure::ModeUnavailable)?.sha256()?,
            ),
        };
        Ok(hash(&bounded_json(
            &("gptgrep.ocr.service.claim.v1", task, mode),
            4096,
        )?))
    }
    fn validate_cached(
        &self,
        bytes: &[u8],
        profile: Option<&CompatibilityRuntimeProfile>,
        runtime: &str,
    ) -> Result<()> {
        ensure!(bytes.len() <= RESPONSE_BYTES, ServiceFailure::OutputLimit);
        match self {
            Self::Full(task) => {
                let value: gptgrep_parse::OcrPageResult = serde_json::from_slice(bytes)?;
                validate_ocr_result_metadata(task, &value)?;
                ensure!(
                    value.receipt.runtime_profile_sha256 == runtime,
                    ServiceFailure::TaskBinding
                );
            }
            Self::Compatibility(task) => {
                let value: gptgrep_parse::CompatibilityOcrResult = serde_json::from_slice(bytes)?;
                let profile = profile.ok_or(ServiceFailure::ModeUnavailable)?;
                ensure!(
                    value.receipt.runtime_profile_sha256 == profile.sha256()?,
                    ServiceFailure::TaskBinding
                );
                gptgrep_parse::validate_compatibility_result(
                    task,
                    &value,
                    &profile.mode_policy_sha256,
                )?;
            }
        }
        Ok(())
    }
    fn response(&self, bytes: Vec<u8>, lease: Arc<WorkingLease>) -> Result<Response<Full<Bytes>>> {
        let bytes = match self {
            Self::Full(_) => bytes,
            Self::Compatibility(_) => {
                let result: gptgrep_parse::CompatibilityOcrResult = serde_json::from_slice(&bytes)?;
                // Exact upstream Standard response. Confidence is the actual model
                // report; no polygon or fabricated detected-word coordinates.
                bounded_json(
                    &serde_json::json!({"results":result.regions}),
                    RESPONSE_BYTES,
                )?
            }
        };
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Full::new(Bytes::from_owner(LeasedResponse {
                bytes,
                _lease: lease,
            })))?)
    }
}

#[derive(Clone, Copy, Debug)]
enum ServiceFailure {
    Unauthorized,
    InvalidRequest,
    NotFound,
    Busy,
    BodyLimit,
    WorkingLimit,
    TaskBinding,
    ModeUnavailable,
    ConsumedUnknown,
    OutputLimit,
    Unavailable,
}
impl std::fmt::Display for ServiceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "unauthorized",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::Busy => "busy",
            Self::BodyLimit => "body_limit",
            Self::WorkingLimit => "working_limit",
            Self::TaskBinding => "task_binding",
            Self::ModeUnavailable => "mode_unavailable",
            Self::ConsumedUnknown => "consumed_unknown",
            Self::OutputLimit => "output_limit",
            Self::Unavailable => "unavailable",
        })
    }
}
impl std::error::Error for ServiceFailure {}
fn error_response(code: ServiceFailure) -> Response<Full<Bytes>> {
    let status = match code {
        ServiceFailure::Unauthorized => StatusCode::UNAUTHORIZED,
        ServiceFailure::NotFound => StatusCode::NOT_FOUND,
        ServiceFailure::Busy | ServiceFailure::WorkingLimit => StatusCode::TOO_MANY_REQUESTS,
        ServiceFailure::BodyLimit => StatusCode::PAYLOAD_TOO_LARGE,
        ServiceFailure::ConsumedUnknown => StatusCode::CONFLICT,
        ServiceFailure::Unavailable | ServiceFailure::ModeUnavailable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::BAD_REQUEST,
    };
    let bytes = serde_json::to_vec(
        &serde_json::json!({"schema_version":"gptgrep.ocr.service.error.v1","code":code.to_string(),
        "retry_allowed":false,"completion":"unavailable_or_unknown"}),
    )
    .expect("fixed closed error");
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("fixed response")
}
fn token_digest(token: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(token).into()
}
fn authorized(headers: &hyper::HeaderMap, digest: &[u8; 32]) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Some(token) = value.as_bytes().strip_prefix(b"Bearer ") else {
        return false;
    };
    (32..=256).contains(&token.len()) && token_digest(token).ct_eq(digest).into()
}
fn single_header(headers: &hyper::HeaderMap, name: header::HeaderName) -> Result<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(ServiceFailure::InvalidRequest)?;
    ensure!(values.next().is_none(), ServiceFailure::InvalidRequest);
    let text = value.to_str().map_err(|_| ServiceFailure::InvalidRequest)?;
    ensure!(text.len() <= 1024, ServiceFailure::InvalidRequest);
    Ok(text.to_owned())
}
async fn collect_body<B>(mut body: B, limit: usize) -> Result<Vec<u8>>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Debug,
{
    ensure!(
        body.size_hint().upper().is_none_or(|n| n <= limit as u64),
        ServiceFailure::BodyLimit
    );
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ServiceFailure::InvalidRequest)?;
        let data = frame
            .into_data()
            .map_err(|_| ServiceFailure::InvalidRequest)?;
        let next = bytes
            .len()
            .checked_add(data.len())
            .ok_or(ServiceFailure::BodyLimit)?;
        ensure!(next <= limit, ServiceFailure::BodyLimit);
        bytes.try_reserve_exact(data.len())?;
        bytes.extend_from_slice(&data);
    }
    Ok(bytes)
}
fn json_size(value: &impl Serialize, limit: usize) -> Result<usize> {
    struct Count {
        used: usize,
        limit: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.used = self
                .used
                .checked_add(bytes.len())
                .filter(|n| *n <= self.limit)
                .ok_or_else(|| std::io::Error::other("bounded JSON"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { used: 0, limit };
    serde_json::to_writer(&mut count, value)?;
    Ok(count.used)
}
pub(crate) fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let size = json_size(value, limit)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size)?;
    serde_json::to_writer(&mut bytes, value)?;
    ensure!(bytes.len() == size, ServiceFailure::OutputLimit);
    Ok(bytes)
}

fn catalog_task_bound(task: &OcrTask) -> Result<u64> {
    let bytes = json_size(task, TASK_BYTES)? as u64;
    // JSON trees, task strings/vectors/maps and private (serde-skipped) bounded paths.
    bytes
        .checked_mul(CATALOG_FACTOR)
        .and_then(|n| n.checked_add(16_384))
        .ok_or_else(|| ServiceFailure::WorkingLimit.into())
}
fn claim_state_bound(max: usize) -> Result<usize> {
    let bound = 4096usize
        .checked_add(max.checked_mul(512).ok_or(ServiceFailure::WorkingLimit)?)
        .ok_or(ServiceFailure::WorkingLimit)?;
    ensure!(bound <= CLAIM_BYTES, ServiceFailure::WorkingLimit);
    Ok(bound)
}
fn claim_working_bound(max: usize) -> Result<u64> {
    (claim_state_bound(max)? as u64)
        .checked_mul(40)
        .and_then(|n| n.checked_add(262_144))
        .ok_or_else(|| ServiceFailure::WorkingLimit.into())
}
fn image_working_one(width: u32, height: u32, bytes: u64, budget: &ResourceBudget) -> Result<u64> {
    let pixels = budget.admit_image(width, height, bytes)?;
    pixels
        .checked_mul(8)
        .and_then(|n| n.checked_add(budget.image_pixel_buffer_bytes))
        .and_then(|n| bytes.checked_mul(3).and_then(|b| n.checked_add(b)))
        .and_then(|n| n.checked_add(65_536))
        .ok_or_else(|| ServiceFailure::WorkingLimit.into())
}
fn image_working_bound(images: &[OcrImage], budget: &ResourceBudget) -> Result<u64> {
    images.iter().try_fold(0, |upper, image| {
        Ok(upper.max(image_working_one(
            image.width,
            image.height,
            image.artifact.bytes,
            budget,
        )?))
    })
}
fn job_working_bound(images: &[(u32, u32, u64)], budget: &OcrTaskBudget) -> Result<u64> {
    // Decode/validation is sequential, but all declared encoded images may be held
    // by PreparedPage. Outgoing Value/base64/hash/serialized-frame copies are known
    // string shapes; arbitrary incoming JSON gets a separate aggregate frame lease.
    let mut decoded = 0u64;
    let mut encoded = 0u64;
    for &(width, height, bytes) in images {
        decoded = decoded.max(image_working_one(width, height, bytes, &budget.resources)?);
        encoded = encoded
            .checked_add(bytes)
            .ok_or(ServiceFailure::WorkingLimit)?;
    }
    decoded
        .checked_add(
            encoded
                .checked_mul(12)
                .ok_or(ServiceFailure::WorkingLimit)?,
        )
        .and_then(|n| n.checked_add(budget.frame_bytes.checked_mul(4)?))
        .and_then(|n| n.checked_add(RESPONSE_BYTES as u64 * 40))
        .and_then(|n| n.checked_add(SERVICE_USAGE_BYTES as u64 * 80 + 262_144))
        .ok_or_else(|| ServiceFailure::WorkingLimit.into())
}

struct MultipartImage {
    bytes: Vec<u8>,
    language: String,
}
fn parse_multipart(content_type: &str, body: &[u8], image_limit: usize) -> Result<MultipartImage> {
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .ok_or(ServiceFailure::InvalidRequest)?;
    let boundary = boundary
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(boundary);
    ensure!(
        !boundary.is_empty()
            && boundary.len() <= 70
            && boundary
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        ServiceFailure::InvalidRequest
    );
    let marker = format!("--{boundary}");
    let mut rest = body;
    let mut image = None;
    let mut language = None;
    let mut parts = 0;
    loop {
        ensure!(
            rest.starts_with(marker.as_bytes()),
            ServiceFailure::InvalidRequest
        );
        rest = &rest[marker.len()..];
        if rest == b"--\r\n" || rest == b"--" {
            break;
        }
        ensure!(rest.starts_with(b"\r\n"), ServiceFailure::InvalidRequest);
        rest = &rest[2..];
        parts += 1;
        ensure!(parts <= 2, ServiceFailure::InvalidRequest);
        let header_end = find_bytes(rest, b"\r\n\r\n").ok_or(ServiceFailure::InvalidRequest)?;
        ensure!(header_end <= 4096, ServiceFailure::InvalidRequest);
        let fields =
            std::str::from_utf8(&rest[..header_end]).map_err(|_| ServiceFailure::InvalidRequest)?;
        let mut disposition = None;
        let mut mime = None;
        for line in fields.split("\r\n") {
            if let Some(value) = line.strip_prefix("Content-Disposition: ") {
                ensure!(
                    disposition.replace(value).is_none(),
                    ServiceFailure::InvalidRequest
                );
            } else if let Some(value) = line.strip_prefix("Content-Type: ") {
                ensure!(
                    mime.replace(value).is_none(),
                    ServiceFailure::InvalidRequest
                );
            } else {
                return Err(ServiceFailure::InvalidRequest.into());
            }
        }
        rest = &rest[header_end + 4..];
        let delimiter = format!("\r\n{marker}");
        let end = find_bytes(rest, delimiter.as_bytes()).ok_or(ServiceFailure::InvalidRequest)?;
        let data = &rest[..end];
        rest = &rest[end + 2..];
        let disposition = disposition.ok_or(ServiceFailure::InvalidRequest)?;
        if disposition == "form-data; name=\"file\"; filename=\"image.png\"" {
            ensure!(
                mime == Some("image/png")
                    && image.is_none()
                    && !data.is_empty()
                    && data.len() <= image_limit,
                ServiceFailure::InvalidRequest
            );
            let mut owned = Vec::new();
            owned.try_reserve_exact(data.len())?;
            owned.extend_from_slice(data);
            image = Some(owned);
        } else if disposition == "form-data; name=\"language\"" {
            ensure!(
                mime.is_none() && language.is_none() && !data.is_empty() && data.len() <= 64,
                ServiceFailure::InvalidRequest
            );
            ensure!(
                data.iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
                ServiceFailure::InvalidRequest
            );
            language = Some(std::str::from_utf8(data)?.to_owned());
        } else {
            return Err(ServiceFailure::InvalidRequest.into());
        }
    }
    Ok(MultipartImage {
        bytes: image.ok_or(ServiceFailure::InvalidRequest)?,
        language: language.unwrap_or_else(|| "eng".into()),
    })
}
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|value| value == needle)
}
fn png_header(bytes: &[u8], budget: &ResourceBudget) -> Result<(u32, u32)> {
    ensure!(
        bytes.len() >= 33
            && &bytes[..8] == b"\x89PNG\r\n\x1a\n"
            && &bytes[8..12] == 13u32.to_be_bytes().as_slice()
            && &bytes[12..16] == b"IHDR",
        ServiceFailure::InvalidRequest
    );
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    budget.admit_image(width, height, bytes.len() as u64)?;
    Ok((width, height))
}
fn png_dimensions(bytes: &[u8], budget: &ResourceBudget) -> Result<(u32, u32)> {
    let (width, height) = png_header(bytes, budget)?;
    // Aggregate service admission has already reserved this complete decoder envelope.
    ensure!(
        image_working_one(width, height, bytes.len() as u64, budget)? <= budget.owned_working_bytes,
        ServiceFailure::BodyLimit
    );
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(budget.image_pixel_buffer_bytes);
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| ServiceFailure::InvalidRequest)?;
    ensure!(
        decoded.width() == width
            && decoded.height() == height
            && decoded.as_bytes().len() as u64 <= budget.image_pixel_buffer_bytes,
        ServiceFailure::InvalidRequest
    );
    Ok((width, height))
}

struct PrivateDirectory {
    path: PathBuf,
    held: File,
}
impl PrivateDirectory {
    fn existing(path: &Path) -> Result<Self> {
        ensure!(!path.is_symlink(), "ocr_service_private_root_symlink");
        let path = path.canonicalize()?;
        let held = open_directory(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                held.metadata()?.permissions().mode() & 0o777 == 0o700,
                "ocr_service_private_mode"
            );
        }
        Ok(Self { path, held })
    }
    fn verify(&self) -> Result<()> {
        verify_directory(&self.path, &self.held)
    }
    fn read(&self, name: &str, limit: usize) -> Result<Vec<u8>> {
        self.verify()?;
        let file = open_relative(&self.held, Path::new(name))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= limit as u64,
            "ocr_service_retained_limit"
        );
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(metadata.len() as usize)?;
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == metadata.len(),
            "ocr_service_retained_changed"
        );
        self.verify()?;
        Ok(bytes)
    }
    fn write_once(&self, name: &str, bytes: &[u8], budget: &ResourceBudget) -> Result<()> {
        self.write_once_created(name, bytes, budget).map(|_| ())
    }
    fn write_once_created(
        &self,
        name: &str,
        bytes: &[u8],
        budget: &ResourceBudget,
    ) -> Result<bool> {
        self.verify()?;
        if exists_relative(&self.held, name)? {
            ensure!(
                self.read(name, bytes.len())? == bytes,
                "ocr_service_retained_changed"
            );
            return Ok(false);
        }
        budget.reserve_retained_staging(bytes.len() as u64)?;
        let created = match create_relative(&self.held, name) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
                self.held.sync_all()?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    self.read(name, bytes.len())? == bytes,
                    "ocr_service_retained_changed"
                );
                false
            }
            Err(error) => return Err(error.into()),
        };
        self.verify()?;
        Ok(created)
    }
    fn replace(&self, name: &str, bytes: &[u8], budget: &ResourceBudget) -> Result<()> {
        self.verify()?;
        budget.reserve_retained_staging(bytes.len() as u64)?;
        let temporary = format!(
            ".stage-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        let mut file = create_relative(&self.held, &temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        rename_relative(&self.held, &temporary, name)?;
        self.held.sync_all()?;
        self.verify()?;
        Ok(())
    }
}
static TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
#[cfg(unix)]
fn create_relative(root: &File, name: &str) -> std::io::Result<File> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    if name.is_empty()
        || name.len() > 256
        || name.contains('/')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(std::io::Error::other("invalid private filename"));
    }
    let name = CString::new(name).map_err(|_| std::io::Error::other("invalid private filename"))?;
    // SAFETY: held descriptor owns this directory; constant exclusive nofollow
    // flags and a validated single component create exactly one owned file.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[cfg(not(unix))]
fn create_relative(_root: &File, _name: &str) -> std::io::Result<File> {
    Err(std::io::Error::other("atomic custody unsupported"))
}
#[cfg(unix)]
fn rename_relative(root: &File, from: &str, to: &str) -> Result<()> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let from = CString::new(from)?;
    let to = CString::new(to)?;
    // SAFETY: both names are private single components generated internally,
    // and both namespace operations use the same held directory descriptor.
    ensure!(
        unsafe {
            libc::renameat(
                root.as_raw_fd(),
                from.as_ptr(),
                root.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0,
        "ocr_service_publish_unknown"
    );
    Ok(())
}
#[cfg(not(unix))]
fn rename_relative(_root: &File, _from: &str, _to: &str) -> Result<()> {
    Err(anyhow!("atomic custody unsupported"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimState {
    schema_version: String,
    phase_sha256: String,
    max_requests: usize,
    entries: BTreeMap<String, ClaimEntry>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ClaimEntry {
    Unknown,
    Complete { sha256: String },
}
struct Claims {
    directory: PrivateDirectory,
    lock: Mutex<File>,
    phase: String,
    max: usize,
    state_bytes: usize,
    budget: ResourceBudget,
}
enum ClaimStart {
    New,
    Cached(Vec<u8>),
    Unknown,
}
impl Claims {
    fn attach(
        directory: PrivateDirectory,
        phase: &str,
        max: usize,
        budget: &ResourceBudget,
    ) -> Result<Self> {
        ensure!((1..=1024).contains(&max), "ocr_service_claim_limit");
        ensure!(
            (4096u64 + max as u64 * 512) * 32 <= budget.owned_working_bytes,
            "ocr_service_claim_working_admission"
        );
        let header = bounded_json(&("gptgrep.ocr.service.claims-lock.v1", phase, max), 4096)?;
        let created = directory.write_once_created("claims.lock", &header, budget)?;
        let lock = open_relative(&directory.held, Path::new("claims.lock"))?;
        ensure!(
            directory.read("claims.lock", 4096)? == header,
            "ocr_service_claim_binding"
        );
        if created {
            let state = ClaimState {
                schema_version: "gptgrep.ocr.service.claims.v1".into(),
                phase_sha256: phase.into(),
                max_requests: max,
                entries: BTreeMap::new(),
            };
            directory.write_once("claims.json", &bounded_json(&state, CLAIM_BYTES)?, budget)?;
        }
        let claims = Self {
            directory,
            lock: Mutex::new(lock),
            phase: phase.into(),
            max,
            state_bytes: claim_state_bound(max)?,
            budget: budget.clone(),
        };
        claims.access(|_| Ok(()))?;
        Ok(claims)
    }
    fn access<T>(&self, change: impl FnOnce(&mut ClaimState) -> Result<T>) -> Result<T> {
        use fs2::FileExt;
        let lock = self.lock.lock().map_err(|_| ServiceFailure::Unavailable)?;
        self.directory.verify()?;
        let current = open_relative(&self.directory.held, Path::new("claims.lock"))?;
        ensure!(
            crate::ocr::same_file(&current, &lock)?,
            "ocr_service_claim_lock_changed"
        );
        let expected = bounded_json(
            &(
                "gptgrep.ocr.service.claims-lock.v1",
                self.phase.as_str(),
                self.max,
            ),
            4096,
        )?;
        ensure!(
            self.directory.read("claims.lock", 4096)? == expected,
            "ocr_service_claim_binding"
        );
        lock.try_lock_exclusive()
            .map_err(|_| ServiceFailure::Busy)?;
        let result = (|| {
            let bytes = self.directory.read("claims.json", self.state_bytes)?;
            let mut state =
                serde_json::from_value::<ClaimState>(gptgrep_jev::parse_unique_json(&bytes)?)?;
            ensure!(
                state.schema_version == "gptgrep.ocr.service.claims.v1"
                    && state.phase_sha256 == self.phase
                    && state.max_requests == self.max
                    && state.entries.len() <= self.max
                    && state.entries.iter().all(
                        |(key, entry)| crate::ocr_accounting::valid_digest(key)
                            && match entry {
                                ClaimEntry::Unknown => true,
                                ClaimEntry::Complete { sha256 } =>
                                    crate::ocr_accounting::valid_digest(sha256),
                            }
                    ),
                "ocr_service_claim_binding"
            );
            let result = change(&mut state)?;
            let updated = bounded_json(&state, self.state_bytes)?;
            if updated != bytes {
                self.directory
                    .replace("claims.json", &updated, &self.budget)?;
            }
            Ok(result)
        })();
        let unlocked = FileExt::unlock(&*lock);
        result.and_then(|value| {
            unlocked?;
            Ok(value)
        })
    }
    fn begin(&self, key: &str) -> Result<ClaimStart> {
        ensure!(
            crate::ocr_accounting::valid_digest(key),
            "ocr_service_claim_binding"
        );
        self.access(|state| match state.entries.get(key) {
            Some(ClaimEntry::Complete { sha256 }) => {
                let bytes = self
                    .directory
                    .read(&format!("{key}.json"), RESPONSE_BYTES)?;
                ensure!(hash(&bytes) == *sha256, "ocr_service_cached_result_changed");
                Ok(ClaimStart::Cached(bytes))
            }
            Some(ClaimEntry::Unknown) => Ok(ClaimStart::Unknown),
            None => {
                ensure!(
                    state.entries.len() < self.max,
                    ServiceFailure::ConsumedUnknown
                );
                state.entries.insert(key.into(), ClaimEntry::Unknown);
                Ok(ClaimStart::New)
            }
        })
    }
    fn complete(&self, key: &str, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= RESPONSE_BYTES, ServiceFailure::OutputLimit);
        self.access(|state| {
            ensure!(
                matches!(state.entries.get(key), Some(ClaimEntry::Unknown)),
                "ocr_service_claim_transition"
            );
            self.directory
                .write_once(&format!("{key}.json"), bytes, &self.budget)?;
            state.entries.insert(
                key.into(),
                ClaimEntry::Complete {
                    sha256: hash(bytes),
                },
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(unix)]
fn exists_relative(root: &File, name: &str) -> Result<bool> {
    use std::{ffi::CString, os::fd::AsRawFd};
    ensure!(
        !name.is_empty() && name.len() <= 256 && !name.contains('/') && name != "." && name != "..",
        "ocr_service_private_name"
    );
    let name = CString::new(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to valid output storage; the owned root is alive and
    // the single-component name is validated, with no symlink traversal.
    let status = unsafe {
        libc::fstatat(
            root.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error.into())
    }
}
#[cfg(not(unix))]
fn exists_relative(_root: &File, _name: &str) -> Result<bool> {
    Err(anyhow!("atomic custody unsupported"))
}

mod control;
pub use control::{ForegroundOcrControl, validate_token_env_name};

pub(crate) fn retain_service_raw(
    path: &Path,
    held: &File,
    digest: &str,
    bytes: &[u8],
    budget: &ResourceBudget,
) -> Result<()> {
    ensure!(bytes.len() <= 262_144, "ocr_raw_result_limit");
    PrivateDirectory {
        path: path.into(),
        held: held.try_clone()?,
    }
    .write_once(&format!("ocr-{digest}.json"), bytes, budget)
}
