//! Local, ephemeral Codex workflows over source-bound GPTgrep evidence.
mod codex_error;
mod completion;
mod enrich;
mod enrich_accounting;
mod evidence_roles;
pub use codex_error::{
    CodexErrorInfo, DocumentRuntimeError, DocumentRuntimeEvent, DocumentRuntimeIdentity,
    DocumentRuntimeStage, DocumentRuntimeTransport, DocumentStartupMethod, HostProtocolError,
    HostProtocolErrorKind, OcrRuntimeError, ToolBudgetDiagnostics,
};
pub use enrich::{
    DEFAULT_ENRICH_MODEL, EnrichBinding, EnrichConfig, EnrichCursor, EnrichReport, EnrichmentPlan,
    EnrichmentUnit, enrich, plan_enrichment,
};
pub use enrich_accounting::EnrichCallSummary;
mod ocr;
mod ocr_accounting;
mod ocr_compat;
mod ocr_native;
pub(crate) use ocr::agent as ocr_agent;
pub use ocr_agent::NativeAgentRuntimeProfile;
mod ocr_protocol;
mod ocr_service;
#[cfg(test)]
mod ocr_tests;
pub mod parse_agent_driver;
mod process_group;
mod protocol;
mod transport;
pub use completion::{
    CompletionError, CompletionReport, MAX_COMPLETION_INPUT_BYTES, MAX_COMPLETION_OUTPUT_BYTES,
    complete_json,
};
pub use ocr::{
    DEFAULT_OCR_MODEL, OcrCancellation, OcrCleanup, OcrHostConfig, OcrHostExecutor,
    OcrImageTransport, OcrProbeReport, OcrRuntimeProfile,
};
pub use ocr_accounting::{
    MAX_OCR_ADMISSIONS, MAX_OCR_CONCURRENCY, OcrBudgetSnapshot, OcrExecutionContext,
    OcrExecutionLimits,
};
pub use ocr_compat::{
    CompatibilityRuntimeProfile, mode_policy_digest as compatibility_mode_policy_digest,
};
pub use ocr_native::{NativeOcrProbeAdmission, NativeOcrQualification, NativeViewerRuntimeProfile};
pub use ocr_service::{
    ForegroundOcrControl, ForegroundOcrOptions, PreparedForegroundOcr, PreparedOcrTaskCatalog,
    prepare_foreground_ocr, validate_token_env_name,
};
mod jev_accounting;
mod model_attempts;
mod native_ptc;
pub(crate) mod native_runtime;
pub use native_runtime::NativeRuntimePackage;
mod native_skills;
pub use native_skills::NativeSkillInput;
mod native_task;
pub use native_ptc::{NativeCommandStatus, NativePtcReport, NativeToolKind, NativeToolObservation};
pub use native_task::{NativeTaskReport, NativeTaskRequest, run_native_task};
mod navigation;
mod query_plan;
mod retrieval;
mod rich_multimodal;
mod rich_text;
pub use rich_multimodal::{
    PreparedRichMultimodalHost, RichMultimodalCitation, RichMultimodalHostError,
    RichMultimodalHostOptions, RichMultimodalHostReport, RichVisualCitation,
    VisualDeliveryIdentity, prepare_rich_multimodal_host,
};
pub use rich_text::{
    PreparedRichTextHost, RichTextCitation, RichTextHostError, RichTextHostOptions,
    RichTextHostReport, prepare_rich_text_host,
};
mod trace;
pub use jev_accounting::{HostRetrievalError, JevReport, ReceiptSummary, SearchTelemetry};
pub use model_attempts::{ModelAttempt, ModelTokenMissingCounts, ModelTokenTotals, ModelUsage};
pub use navigation::{
    NavigationBatchReport, NavigationConfig, NavigationQueryReport, NavigationSelection,
};
pub use query_plan::{QueryPlanConfig, QueryPlanReport};

use anyhow::{Result, anyhow, ensure};
pub use retrieval::{Citation, ToolReceipt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
#[cfg(test)]
use tokio::io::BufReader;
use tokio::{io::AsyncReadExt, process::Command};

pub const DEFAULT_MODEL: &str = "gpt-6-luna";
pub const DEFAULT_REASONING_EFFORT: &str = "max";
pub const DEFAULT_SERVICE_TIER: &str = "fast";

#[derive(Debug)]
pub struct HostCapabilityError;
impl std::fmt::Display for HostCapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host_no_evidence_tools")
    }
}
impl std::error::Error for HostCapabilityError {}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub codex_bin: String,
    pub codex_home: PathBuf,
    /// Optional canonical existing state directory with an existing logs child.
    /// Authentication and profile selection continue to use codex_home.
    pub codex_state_dir: Option<PathBuf>,
    /// Explicit existing named permission profile. Legacy read-only mode is used
    /// when omitted; a selected profile must be acknowledged before a turn.
    pub permission_profile: Option<String>,
    /// Use native Codex capabilities under the selected named permissions.
    /// Native budgets are observed after execution may start.
    pub native_ptc: bool,
    /// Explicit native capability overrides; other selected runtime settings are inherited.
    pub native_capabilities: std::collections::BTreeMap<String, serde_json::Value>,
    /// Explicit pinned Skill inputs for native turns; unselected runtime Skills remain inherited.
    pub native_skills: Vec<NativeSkillInput>,
    /// Selected frozen native project package, staged into the actual native task cwd.
    pub native_runtime_package: Option<crate::NativeRuntimePackage>,
    /// Existing working directory for a native task; omitted requests use an owned temporary directory.
    pub working_dir: Option<PathBuf>,
    /// Additional process environment names withheld from this selected runtime.
    pub excluded_env_vars: Vec<String>,
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub timeout_secs: u64,
    pub max_tool_calls: usize,
    pub max_input_bytes: usize,
    pub trace_path: Option<PathBuf>,
    pub jev_model: Option<String>,
    /// Shared Decisions admissions, original deadline and optional existing ledger.
    pub decisions_execution: Option<gptgrep_jev::ExecutionContext>,
    pub document: Option<String>,
    pub query_plan: Option<QueryPlanConfig>,
    pub navigation: Option<NavigationConfig>,
    pub source_continuation: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            codex_bin: "codex".into(),
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex")
                }),
            codex_state_dir: None,
            permission_profile: None,
            native_ptc: false,
            native_capabilities: Default::default(),
            native_skills: Vec::new(),
            native_runtime_package: None,
            working_dir: None,
            excluded_env_vars: Vec::new(),
            model: DEFAULT_MODEL.into(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.into(),
            service_tier: DEFAULT_SERVICE_TIER.into(),
            timeout_secs: 180,
            max_tool_calls: 12,
            max_input_bytes: 256 * 1024,
            trace_path: None,
            jev_model: None,
            decisions_execution: None,
            document: None,
            query_plan: None,
            navigation: None,
            source_continuation: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostReport {
    pub schema_version: String,
    pub status: String,
    pub operation: String,
    pub generation: String,
    pub thread_id: String,
    pub turn_id: String,
    pub requested_model: String,
    pub model: String,
    pub model_provider: String,
    pub auth_mode: String,
    pub codex_home: PathBuf,
    pub requested_reasoning_effort: String,
    pub effective_reasoning_effort: Option<String>,
    pub requested_service_tier: String,
    pub effective_service_tier: Option<String>,
    pub server_retry_notifications: usize,
    pub answer: String,
    pub citations: Vec<Citation>,
    pub tool_calls: Vec<ToolReceipt>,
    pub usage: Option<Value>,
    #[serde(default = "final_reader_usage_scope")]
    pub usage_scope: String,
    #[serde(default)]
    pub query_plan: Option<QueryPlanReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<NavigationQueryReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_continuation: Option<Value>,
    #[serde(default)]
    pub model_attempts: Vec<ModelAttempt>,
    #[serde(default)]
    pub model_usage: ModelUsage,
    pub elapsed_ms: u128,
    pub stderr_bytes: Option<u64>,
    pub stderr_truncated: Option<bool>,
    pub warnings: Vec<String>,
    pub jev: JevReport,
    pub ledger_path: PathBuf,
}

fn final_reader_usage_scope() -> String {
    "final_reader".into()
}

pub async fn ask(root: &Path, question: &str, config: &HostConfig) -> Result<HostReport> {
    execute(root, question, None, config).await
}

pub async fn summarize(root: &Path, node_id: &str, config: &HostConfig) -> Result<HostReport> {
    ensure!(config.query_plan.is_none(), "host_query_plan_requires_ask");
    ensure!(
        !node_id.is_empty() && node_id.len() <= 256,
        "Invalid summary node ID"
    );
    execute(root, "Summarize the selected node, its scope, key claims and limitations. Expand its document tree and read relevant child or neighboring nodes when needed. Cite the evidence used.", Some(node_id), config).await
}

async fn execute(
    root: &Path,
    question: &str,
    node_id: Option<&str>,
    config: &HostConfig,
) -> Result<HostReport> {
    execute_with_client(root, question, node_id, config, None).await
}

async fn execute_with_client(
    root: &Path,
    question: &str,
    node_id: Option<&str>,
    config: &HostConfig,
    client: Option<gptgrep_jev::JevClient>,
) -> Result<HostReport> {
    validate_config(config, question)?;
    ensure!(
        node_id.is_none() || config.query_plan.is_none(),
        "host_query_plan_requires_ask"
    );
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
    let bounded_config;
    let config = if let Some(context) = &config.decisions_execution {
        bounded_config = HostConfig {
            decisions_execution: Some(context.with_deadline(deadline.into_std())?),
            ..config.clone()
        };
        &bounded_config
    } else {
        config
    };
    let root = root
        .canonicalize()
        .map_err(|_| anyhow!("Document root is unavailable"))?;
    let mut evidence = retrieval::Evidence::open(&root, node_id)?;
    let accounting = jev_accounting::Accounting::create(&root, &evidence.generation)?;
    let _guard = jev_accounting::AttemptGuard(accounting.clone());
    let mut stage = "scope";
    let mut query_plan_report = None;
    let outcome:Result<HostReport>=async {
    evidence.configure(config,accounting.clone(),client)?;
    accounting.bind_workflow(question, evidence.document_scope())?;
    accounting.set_model_attempt_limit(if config.query_plan.is_some() { 2 } else { 1 })?;
    if let Some(navigation) = &config.navigation {
        stage="navigation";
        evidence.prepare_navigation(question, navigation, deadline).await?;
    }
    if let Some(query_config) = &config.query_plan {
        stage="query_plan_prepare";
        let input = evidence.prepare_query_plan(question)?;
        query_plan_report = Some(QueryPlanReport {
            strategy: "luna_queries_v1".into(),
            status: "running".into(),
            alternate_queries: vec![],
            duplicates_removed: 0,
            state_sha256: retrieval::hash(&serde_json::to_vec(&input.state)?),
            instructions_sha256: retrieval::hash(input.instructions.as_bytes()),
            schema_sha256: retrieval::hash(&serde_json::to_vec(&input.schema)?),
        });
        let mut planner_config = planner_runtime_config(config, query_config);
        planner_config.trace_path = config.trace_path.as_ref().map(|path| {
            let mut name = path.as_os_str().to_os_string();
            name.push(".planner-");
            name.push(accounting.path().file_stem().expect("attempt ledger filename"));
            PathBuf::from(name)
        });
        let planner_deadline = deadline.min(tokio::time::Instant::now()
            + Duration::from_secs(query_config.planner_timeout_secs));
        ensure!(tokio::time::Instant::now() < planner_deadline, "host_deadline_exceeded");
        stage="query_plan";
        let mut planner = model_attempts::ModelAttemptTracker::reserve(&accounting, "query_planner", &planner_config)?;
        let planned = completion::complete_json_until(
            &input.instructions, input.state, input.schema, &planner_config,
            Some(planner_deadline), protocol::RunOptions {
                observer: Some(planner.observer()),
                max_output_bytes: Some(query_plan::MAX_PLAN_OUTPUT_BYTES),
                ..Default::default()
            },
        ).await.and_then(|completion| query_plan::validate_plan(question, &completion.value));
        if let Some(report) = &mut query_plan_report {
            report.status = if planned.is_ok() { "completed" } else { "failed" }.into();
            if let Ok(plan) = &planned {
                report.alternate_queries = plan.alternate_queries.clone();
                report.duplicates_removed = plan.duplicates_removed;
            }
        }
        let finish_result = planner.finish(planned.as_ref().err());
        let plan = planned?;
        finish_result?;
        stage="initial_search";
        tokio::time::timeout_at(deadline, evidence.bootstrap_planned(question, &plan.alternate_queries)).await
            .map_err(|_| anyhow!("host_jev_initial_timeout"))??;
    } else {
    stage="initial_search";
    tokio::time::timeout_at(deadline,evidence.bootstrap(question)).await
        .map_err(|_|anyhow!("host_jev_initial_timeout"))??;
    }
    ensure!(evidence.initial_payload.is_some(),"host_jev_initial_required");
    stage="codex";
    ensure!(tokio::time::Instant::now() < deadline, "host_deadline_exceeded");
    let mut reader = model_attempts::ModelAttemptTracker::reserve(&accounting, "final_reader", config)?;
    let validated: Result<_> = async {
    let completed = run_process_until(
        config,
        protocol::Workflow::Retrieval {
            question,
            node_id,
            evidence: &mut evidence,
        },
        Some(deadline),
        protocol::RunOptions { observer: Some(reader.observer()), max_output_bytes: None, ..Default::default() },
    )
    .await?;
    stage="citation_validation";
    let validated = evidence.finish(&completed.result.answer)?;
    Ok((completed, validated))
    }.await;
    let finish_result = reader.finish(validated.as_ref().err());
    let (completed, (answer, citations, insufficient)) = validated?;
    finish_result?;
    let ProcessOutcome {
        result,
        home,
        elapsed_ms:_,
        stderr_bytes,
        stderr_truncated,
        ..
    } = completed;
    let mut warnings = result.warnings;
    warnings.push("Citation validation checks issued snapshot identity and current source freshness; it does not prove semantic entailment.".into());
    let report=HostReport {
        schema_version: "gptgrep.host.v1".into(),
        status: if insufficient {
            "insufficient_evidence"
        } else {
            "completed"
        }
        .into(),
        operation: if node_id.is_some() {
            "summarize"
        } else {
            "ask"
        }
        .into(),
        generation: evidence.generation.clone(),
        thread_id: result.thread_id,
        turn_id: result.turn_id,
        requested_model: config.model.clone(),
        model: result.model,
        model_provider: result.provider,
        auth_mode: "chatgpt".into(),
        codex_home: home,
        requested_reasoning_effort: config.reasoning_effort.clone(),
        effective_reasoning_effort: result.effort,
        requested_service_tier: config.service_tier.clone(),
        effective_service_tier: result.service_tier,
        server_retry_notifications: result.server_retry_notifications,
        answer,
        citations,
        tool_calls: evidence.receipts.clone(),
        usage: result.usage,
        usage_scope: "final_reader".into(),
        query_plan: query_plan_report.clone(),
        navigation: accounting.navigation(),
        source_continuation: source_continuation_receipt(config),
        model_usage: model_attempts::summarize(&accounting.model_attempts()),
        model_attempts: accounting.model_attempts(),
        elapsed_ms:started.elapsed().as_millis(),
        stderr_bytes,
        stderr_truncated,
        warnings,
        jev:accounting.summary(),ledger_path:accounting.path(),
    };
    stage="ledger_closeout";
    accounting.finish("completed")?;
    Ok(report)
    }.await;
    match outcome {
        Ok(report) => Ok(report),
        Err(error) => {
            let _ = accounting.finish("failed");
            let code = failure_code(&error, stage);
            let cause = error
                .downcast_ref::<gptgrep_core::JevSearchError>()
                .and_then(|error| serde_json::to_value(error).ok())
                .or_else(|| {
                    error
                        .downcast_ref::<gptgrep_core::PlannedSearchError>()
                        .and_then(|error| serde_json::to_value(error).ok())
                })
                .or_else(|| {
                    error
                        .downcast_ref::<CompletionError>()
                        .map(|error| json!({"code":error.code()}))
                })
                .or_else(|| {
                    error
                        .downcast_ref::<HostProtocolError>()
                        .and_then(|error| serde_json::to_value(error).ok())
                })
                .or_else(|| {
                    error
                        .downcast_ref::<jev_accounting::InitializationError>()
                        .map(|error| json!({"stage":"initialization","cause":error.cause}))
                })
                .or_else(|| {
                    (stage == "citation_validation")
                        .then(|| json!({"code": retrieval::citation_validation_code(&error)}))
                });
            Err(HostRetrievalError {
                code,
                stage: stage.into(),
                generation: evidence.generation.clone(),
                ledger_path: accounting.path(),
                jev: accounting.summary(),
                receipts: evidence.receipts.iter().map(ReceiptSummary::from).collect(),
                cause,
                elapsed_ms: started.elapsed().as_millis(),
                usage_scope: "final_reader".into(),
                query_plan: query_plan_report,
                navigation: accounting.navigation(),
                model_usage: model_attempts::summarize(&accounting.model_attempts()),
                model_attempts: accounting.model_attempts(),
            }
            .into())
        }
    }
}

/// The ask-level failure code: typed Jev and protocol failures first, then a
/// host code carried by the error; any other error keeps only its stage.
fn failure_code(error: &anyhow::Error, stage: &str) -> String {
    if error
        .downcast_ref::<gptgrep_core::JevSearchError>()
        .is_some()
        || error
            .downcast_ref::<gptgrep_core::PlannedSearchError>()
            .is_some()
    {
        "host_jev_search_failed".to_owned()
    } else if let Some(protocol) = error.downcast_ref::<HostProtocolError>() {
        protocol.code().to_owned()
    } else if error.to_string().starts_with("host_") {
        error.to_string()
    } else {
        format!("host_{stage}_failed")
    }
}

fn planner_runtime_config(config: &HostConfig, query_config: &QueryPlanConfig) -> HostConfig {
    let mut planner = config.clone();
    planner.navigation = None;
    planner.source_continuation = false;
    planner.native_ptc = false;
    planner.model = query_config.planner_model.clone();
    planner.service_tier = DEFAULT_SERVICE_TIER.into();
    planner.max_input_bytes = config.max_input_bytes.min(query_plan::MAX_PLAN_INPUT_BYTES);
    planner.query_plan = None;
    planner
}

struct ProcessOutcome {
    result: protocol::Outcome,
    home: PathBuf,
    elapsed_ms: u128,
    stderr_bytes: Option<u64>,
    stderr_truncated: Option<bool>,
    transport_bytes: usize,
}

async fn run_process_until(
    config: &HostConfig,
    workflow: protocol::Workflow<'_>,
    deadline: Option<tokio::time::Instant>,
    options: protocol::RunOptions<'_>,
) -> Result<ProcessOutcome> {
    let deadline = match deadline {
        Some(value) => value,
        None => tokio::time::Instant::now()
            .checked_add(Duration::from_secs(config.timeout_secs))
            .ok_or_else(|| anyhow!("host_deadline_overflow"))?,
    };
    ensure!(
        tokio::time::Instant::now() < deadline,
        "host_deadline_exceeded"
    );
    let native_ptc = workflow.native_ptc_enabled(config)?;
    let transport = transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES);
    let trace = trace::Trace::open(config.trace_path.as_deref())?;
    let home = config
        .codex_home
        .canonicalize()
        .map_err(|_| anyhow!("host_codex_home_unavailable"))?;
    let temporary_cwd = if config.working_dir.is_none() {
        Some(
            tempfile::Builder::new()
                .prefix("gptgrep-host-")
                .tempdir()
                .map_err(|_| anyhow!("host_codex_working_directory_unavailable"))?,
        )
    } else {
        None
    };
    let cwd = match &config.working_dir {
        Some(path) => {
            let canonical = path
                .canonicalize()
                .map_err(|_| anyhow!("host_native_working_directory"))?;
            ensure!(
                canonical == *path && canonical.is_dir(),
                "host_native_working_directory"
            );
            canonical
        }
        None => temporary_cwd
            .as_ref()
            .expect("owned temporary cwd")
            .path()
            .canonicalize()
            .map_err(|_| anyhow!("host_native_working_directory"))?,
    };
    let mut selected_config = config.clone();
    if native_ptc {
        if let Some(package) = &config.native_runtime_package {
            let staged = native_runtime::stage_native_runtime(package, &cwd)?;
            selected_config.native_skills.extend(staged.skills);
            for (key, value) in staged.role_capabilities {
                selected_config
                    .native_capabilities
                    .entry(key)
                    .or_insert(value);
            }
        }
        native_skills::validate(&selected_config.native_skills)?;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "host_deadline_exceeded"
        );
    }
    let config = &selected_config;
    let binary = if Path::new(&config.codex_bin).components().count() > 1 {
        PathBuf::from(&config.codex_bin)
            .canonicalize()
            .map_err(|_| anyhow!("host_codex_binary_unavailable"))?
    } else {
        PathBuf::from(&config.codex_bin)
    };
    ensure!(home.is_dir(), "host_codex_home_not_directory");
    let capabilities = if native_ptc {
        native_ptc::runtime_config(config)?
    } else {
        protocol::base_config()
    };
    let mut command = process_command_with_config(
        &binary,
        &cwd,
        &home,
        config.codex_state_dir.as_deref(),
        &capabilities,
    )?;
    for name in &config.excluded_env_vars {
        ensure!(
            !name.is_empty() && !name.contains('=') && !name.chars().any(char::is_control),
            "host_excluded_environment_name"
        );
        command.env_remove(name);
    }
    if native_ptc {
        for name in [
            "GPTGREP_NATIVE_OCR_REQUIRED",
            "GPTGREP_NATIVE_OCR_ADMISSION",
            "GPTGREP_NATIVE_OCR_ADMISSION_SHA256",
            "GPTGREP_NATIVE_DOCUMENT_REQUIRED",
            "GPTGREP_NATIVE_DOCUMENT_ADMISSION",
            "GPTGREP_NATIVE_DOCUMENT_ADMISSION_SHA256",
        ] {
            command.env_remove(name);
        }
    }
    if let Some(profile) = &config.permission_profile {
        command
            .arg("-c")
            .arg(format!(
                "default_permissions={}",
                protocol::override_json(profile)?
            ))
            .arg("-c")
            .arg("cli_auth_credentials_store=\"file\"")
            .arg("-c")
            .arg("forced_login_method=\"chatgpt\"")
            .arg("-c")
            .arg("model_provider=\"openai\"");
    }
    ensure!(
        tokio::time::Instant::now() < deadline,
        "host_deadline_exceeded"
    );
    if let Some(observer) = options.observer {
        observer.running()?;
    }
    ensure!(
        tokio::time::Instant::now() < deadline,
        "host_deadline_exceeded"
    );
    let mut child = command
        .spawn()
        .map_err(|_| anyhow!("host_codex_spawn_failed"))?;
    if let Some(proof) = options.cleanup_proof {
        proof.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    let mut process_group = process_group::OwnedGroup::new(&child)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("host_codex_stdin_unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("host_codex_stdout_unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("host_codex_stderr_unavailable"))?;
    let stderr_budget = transport.clone();
    let mut stderr_task = tokio::spawn(async move {
        let mut stderr = transport::BudgetedReader::new(stderr, stderr_budget);
        const CAP: usize = 64 * 1024;
        let mut retained = Vec::new();
        let mut total = 0u64;
        let mut buffer = [0; 4096];
        loop {
            let count = stderr
                .read(&mut buffer)
                .await
                .map_err(|_| anyhow!("host_stderr_transport_failed"))?;
            if count == 0 {
                break;
            }
            total = total.saturating_add(count as u64);
            let keep = count.min(CAP.saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
        // Raw stderr is private to this task and is never placed in a report or error.
        Ok::<_, anyhow::Error>((total, total > retained.len() as u64))
    });
    let started = Instant::now();
    let session = protocol::run_observed_budget(
        stdout,
        stdin,
        &cwd,
        config,
        workflow,
        trace,
        options,
        transport.clone(),
        deadline,
    );
    let outcome=tokio::time::timeout_at(deadline,async {
        // There is exactly one designated failure waiter for this invocation.
        tokio::select! { biased; _=ocr::cancellation_wait(options.cancel)=>Err(anyhow!("host_native_task_cancelled")), _=transport.exceeded()=>transport.check_host().and_then(|()|Err(anyhow!("host_transport_unknown"))), result=session=>result }
    }).await;
    // Capture primary invocation classification before cleanup can change the
    // meter or clock. Only a typed timeout is normalized as a timeout.
    let invocation = capture_invocation_result(outcome, &transport, options.observer.is_some());
    // All exits kill and reap the owned app-server. kill_on_drop also covers caller cancellation.
    if let Err(error) = process_group.stop(&mut child).await {
        stderr_task.abort();
        // Existing lifecycle-unknown priority remains: an unconfirmed owned
        // cleanup can supersede the captured invocation error (RPC-S2).
        return Err(error);
    }
    let stderr_result = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
    if !matches!(stderr_result, Ok(Ok(Ok(_)))) {
        stderr_task.abort();
    }
    let drain = match stderr_result {
        Ok(Ok(Ok(metrics))) => Ok(metrics),
        _ => Err(anyhow!("host_stderr_cleanup_unknown")),
    };
    if drain.is_ok()
        && let Some(proof) = options.cleanup_proof
    {
        proof.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let (result, stderr_bytes, stderr_truncated) =
        finalize_captured_invocation(invocation, &transport, drain)?;
    if let Some(observer) = options.observer {
        observer.process_completed()?;
    }
    Ok(ProcessOutcome {
        result,
        home,
        elapsed_ms: started.elapsed().as_millis(),
        stderr_bytes: Some(stderr_bytes),
        stderr_truncated: Some(stderr_truncated),
        transport_bytes: transport.used(),
    })
}

fn invocation_timeout(observed: bool) -> anyhow::Error {
    if observed {
        anyhow!("host_model_attempt_timeout")
    } else {
        // Unobserved invocations (independent completion, native tasks): the
        // owned app-server was terminated at the deadline.
        anyhow!("host_codex_process_timeout")
    }
}

fn capture_invocation_result<T>(
    outcome: std::result::Result<Result<T>, tokio::time::error::Elapsed>,
    transport: &transport::TransportBudget,
    observed: bool,
) -> Result<T> {
    ensure!(!transport.overflowed(), "host_total_transport_limit");
    match outcome {
        Err(_) => Err(invocation_timeout(observed)),
        Ok(Err(error)) if error.is::<tokio::time::error::Elapsed>() => {
            Err(invocation_timeout(observed))
        }
        Ok(result) => result,
    }
}

fn finalize_captured_invocation<T>(
    invocation: Result<T>,
    transport: &transport::TransportBudget,
    drain: Result<(u64, bool)>,
) -> Result<(T, u64, bool)> {
    let result = invocation?;
    // A captured success still needs the complete cleanup envelope to pass.
    transport.check_host()?;
    let (bytes, truncated) = drain?;
    Ok((result, bytes, truncated))
}

fn native_state_paths(state: Option<&Path>) -> Result<Option<(PathBuf, PathBuf)>> {
    // The state and logs directories must be absolute, existing, canonical
    // directories; symlinks and symlink ancestors are unsupported.
    fn directory(path: &Path) -> Result<PathBuf> {
        ensure!(path.is_absolute(), "host_codex_state_dir_not_absolute");
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| anyhow!("host_codex_state_dir_unavailable"))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "host_codex_state_dir_not_directory"
        );
        let canonical = path
            .canonicalize()
            .map_err(|_| anyhow!("host_codex_state_dir_unavailable"))?;
        ensure!(
            canonical.as_os_str() == path.as_os_str(),
            "host_codex_state_dir_not_canonical"
        );
        Ok(canonical)
    }
    let Some(state) = state else { return Ok(None) };
    let state = directory(state)?;
    let logs = directory(&state.join("logs"))?;
    Ok(Some((state, logs)))
}

#[cfg(test)]
fn process_command(
    binary: &Path,
    cwd: &Path,
    home: &Path,
    state: Option<&Path>,
) -> Result<Command> {
    process_command_with_config(binary, cwd, home, state, &protocol::base_config())
}

fn process_command_with_config(
    binary: &Path,
    cwd: &Path,
    home: &Path,
    state: Option<&Path>,
    capabilities: &serde_json::Value,
) -> Result<Command> {
    let state = native_state_paths(state)?;
    let mut command = Command::new(binary);
    process_group::configure(&mut command);
    command
        .arg("app-server")
        .args(["--listen", "stdio://"])
        .current_dir(cwd)
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "OPENROUTER_API_KEY",
        "CODEX_SQLITE_HOME",
        "CODEX_THREAD_ID",
        "CODEX_TURN_ID",
        "CODEX_PARENT_THREAD_ID",
        "CODEX_SESSION_ID",
    ] {
        command.env_remove(key);
    }
    // Preserve the original home binding unless a separate state root is explicit.
    // CLI overrides win over inherited CODEX_SQLITE_HOME and profile settings.
    let sqlite = state.as_ref().map_or(home, |(state, _)| state.as_path());
    // A path that is not valid UTF-8 cannot be encoded as an override.
    let path_override = |path: &Path| {
        serde_json::to_string(path).map_err(|_| anyhow!("host_codex_state_path_encoding"))
    };
    command
        .arg("-c")
        .arg(format!("sqlite_home={}", path_override(sqlite)?));
    if let Some((_, logs)) = state {
        command
            .arg("-c")
            .arg(format!("log_dir={}", path_override(logs.as_path())?));
    }
    for (key, value) in capabilities
        .as_object()
        .expect("validated configuration object")
    {
        command
            .arg("-c")
            .arg(format!("{key}={}", protocol::toml_override(value)?));
    }
    Ok(command)
}

fn validate_config(config: &HostConfig, question: &str) -> Result<()> {
    native_ptc::enabled(config, false)?;
    if config.native_ptc {
        native_skills::validate(&config.native_skills)?;
    }
    native_state_paths(config.codex_state_dir.as_deref())?;
    if let Some(profile) = &config.permission_profile {
        ensure!(
            !profile.is_empty()
                && profile.len() <= 128
                && profile.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'_' | b'-' | b':' | b'.')),
            "Invalid Codex permission profile"
        );
    }
    ensure!(
        !question.trim().is_empty() && question.len() <= 8192,
        "Question must contain 1..8192 bytes"
    );
    ensure!(
        config.timeout_secs > 0 && (config.native_ptc || config.timeout_secs <= 900),
        "Host timeout must be positive and within the selected execution mode"
    );
    ensure!(
        config.max_tool_calls > 0
            && config.max_tool_calls < usize::MAX
            && (config.native_ptc || config.max_tool_calls <= 64),
        "Host tool-call limit must be positive and within the selected execution mode"
    );
    ensure!(
        !config.model.is_empty()
            && config.model.len() <= 128
            && !config.model.chars().any(char::is_control),
        "Invalid host model"
    );
    ensure!(
        matches!(
            config.reasoning_effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        ),
        "Invalid reasoning effort"
    );
    ensure!(
        matches!(
            config.service_tier.as_str(),
            "fast" | "priority" | "flex" | "default"
        ),
        "Invalid service tier; supported values are fast, priority, flex, default"
    );
    ensure!(!config.codex_bin.is_empty(), "Codex binary is empty");
    if let Some(query_plan) = &config.query_plan {
        query_plan.validate()?;
    }
    if let Some(navigation) = &config.navigation {
        navigation.validate()?;
        ensure!(
            config.query_plan.is_some()
                && config
                    .document
                    .as_ref()
                    .is_some_and(|path| !path.is_empty()),
            "host_navigation_requires_planned_document_ask"
        );
    }
    if config.source_continuation {
        ensure!(
            config.query_plan.is_some()
                && config
                    .document
                    .as_ref()
                    .is_some_and(|path| !path.is_empty()),
            "host_source_continuation_requires_scoped_planned_ask"
        );
    }
    Ok(())
}

fn source_continuation_receipt(config: &HostConfig) -> Option<Value> {
    config.source_continuation.then(|| {
        json!({
            "schema_version":"gptgrep.source-continuation.v1", "enabled":true,
            "document_scope":config.document,
            "guidance_sha256":retrieval::hash(protocol::SOURCE_CONTINUATION_GUIDANCE.as_bytes())
        })
    })
}

pub(crate) fn final_schema() -> Value {
    json!({"type":"object","additionalProperties":false,
        "properties":{"answer":{"type":"string","maxLength":8192},
        "citations":{"type":"array","maxItems":24,"items":{"type":"string"}},
        "insufficient_evidence":{"type":"boolean"}},
        "required":["answer","citations","insufficient_evidence"]})
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transport_priority_tests;
