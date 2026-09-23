//! Local, ephemeral Codex workflows over source-bound GPTgrep evidence.
mod codex_error;
mod completion;
mod enrich;
mod enrich_accounting;
mod evidence_roles;
pub use codex_error::{
    CodexErrorInfo, HostProtocolError, HostProtocolErrorKind, ToolBudgetDiagnostics,
};
pub use enrich::{
    DEFAULT_ENRICH_MODEL, EnrichBinding, EnrichConfig, EnrichCursor, EnrichReport, EnrichmentPlan,
    EnrichmentUnit, enrich, plan_enrichment,
};
pub use enrich_accounting::EnrichCallSummary;
mod process_group;
mod protocol;
pub use completion::{
    CompletionError, CompletionReport, MAX_COMPLETION_INPUT_BYTES, MAX_COMPLETION_OUTPUT_BYTES,
    complete_json,
};
mod jev_accounting;
mod model_attempts;
mod navigation;
mod query_plan;
mod retrieval;
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
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::Command,
};

pub const DEFAULT_MODEL: &str = "gpt-5.6-luna";
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
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub timeout_secs: u64,
    pub max_tool_calls: usize,
    pub max_input_bytes: usize,
    pub trace_path: Option<PathBuf>,
    pub jev_model: Option<String>,
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
            model: DEFAULT_MODEL.into(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.into(),
            service_tier: DEFAULT_SERVICE_TIER.into(),
            timeout_secs: 180,
            max_tool_calls: 12,
            max_input_bytes: 256 * 1024,
            trace_path: None,
            jev_model: None,
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
            },
        ).await.and_then(|completion| query_plan::validate_plan(question, &completion.value));
        if let Some(report) = &mut query_plan_report {
            report.status = if planned.is_ok() { "completed" } else { "failed" }.into();
            if let Ok(plan) = &planned {
                report.alternate_queries = plan.alternate_queries.clone();
                report.duplicates_removed = plan.duplicates_removed;
            }
        }
        planner.finish(planned.as_ref().err())?;
        let plan = planned?;
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
        protocol::RunOptions { observer: Some(reader.observer()), max_output_bytes: None },
    )
    .await?;
    stage="citation_validation";
    let validated = evidence.finish(&completed.result.answer)?;
    Ok((completed, validated))
    }.await;
    reader.finish(validated.as_ref().err())?;
    let (completed, (answer, citations, insufficient)) = validated?;
    let ProcessOutcome {
        result,
        home,
        elapsed_ms:_,
        stderr_bytes,
        stderr_truncated,
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
            let code = if error
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
            };
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

fn planner_runtime_config(config: &HostConfig, query_config: &QueryPlanConfig) -> HostConfig {
    let mut planner = config.clone();
    planner.navigation = None;
    planner.source_continuation = false;
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
}

async fn run_process_until(
    config: &HostConfig,
    workflow: protocol::Workflow<'_>,
    deadline: Option<tokio::time::Instant>,
    options: protocol::RunOptions<'_>,
) -> Result<ProcessOutcome> {
    if let Some(deadline) = deadline {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "host_deadline_exceeded"
        );
    }
    let trace = trace::Trace::open(config.trace_path.as_deref())?;
    let home = config
        .codex_home
        .canonicalize()
        .map_err(|_| anyhow!("Selected CODEX_HOME is unavailable"))?;
    let cwd = tempfile::Builder::new().prefix("gptgrep-host-").tempdir()?;
    let binary = if Path::new(&config.codex_bin).components().count() > 1 {
        PathBuf::from(&config.codex_bin)
            .canonicalize()
            .map_err(|_| anyhow!("Configured Codex binary is unavailable"))?
    } else {
        PathBuf::from(&config.codex_bin)
    };
    ensure!(home.is_dir(), "Selected CODEX_HOME must be a directory");
    let mut command = process_command(&binary, cwd.path(), &home)?;
    if let Some(deadline) = deadline {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "host_deadline_exceeded"
        );
    }
    if let Some(observer) = options.observer {
        observer.running()?;
    }
    let mut child = command
        .spawn()
        .map_err(|_| anyhow!("Could not start the configured Codex app-server"))?;
    let mut process_group = process_group::OwnedGroup::new(&child)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Codex stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Codex stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Codex stderr unavailable"))?;
    let mut stderr_task = tokio::spawn(async move {
        const CAP: usize = 64 * 1024;
        let mut retained = Vec::new();
        let mut total = 0u64;
        let mut buffer = [0; 4096];
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            total = total.saturating_add(count as u64);
            let keep = count.min(CAP.saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
        // Raw stderr is private to this task and is never placed in a report or error.
        (total, total > retained.len() as u64)
    });
    let started = Instant::now();
    let session = protocol::run_observed(
        BufReader::new(stdout),
        stdin,
        cwd.path(),
        config,
        workflow,
        trace,
        options,
    );
    let deadline = deadline
        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs));
    let outcome = tokio::time::timeout_at(deadline, session).await;
    // All exits kill and reap the owned app-server. kill_on_drop also covers caller cancellation.
    if let Err(error) = process_group.stop(&mut child).await {
        stderr_task.abort();
        return Err(error);
    }
    let stderr_result = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
    let (stderr_bytes, stderr_truncated) = match stderr_result {
        Ok(Ok((bytes, truncated))) => (Some(bytes), Some(truncated)),
        _ => {
            stderr_task.abort();
            (None, None)
        }
    };
    let result = outcome.map_err(|_| {
        if options.observer.is_some() {
            anyhow!("host_model_attempt_timeout")
        } else {
            anyhow!("Codex host exceeded its time limit; owned app-server was terminated")
        }
    })??;
    if let Some(observer) = options.observer {
        observer.process_completed()?;
    }
    Ok(ProcessOutcome {
        result,
        home,
        elapsed_ms: started.elapsed().as_millis(),
        stderr_bytes,
        stderr_truncated,
    })
}

fn process_command(binary: &Path, cwd: &Path, home: &Path) -> Result<Command> {
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
    // Explicit CLI state root wins over an inherited CODEX_SQLITE_HOME or config override.
    command
        .arg("-c")
        .arg(format!("sqlite_home={}", serde_json::to_string(home)?));
    for (key, value) in protocol::base_config().as_object().expect("static object") {
        command
            .arg("-c")
            .arg(format!("{key}={}", protocol::toml_override(value)?));
    }
    Ok(command)
}

fn validate_config(config: &HostConfig, question: &str) -> Result<()> {
    ensure!(
        !question.trim().is_empty() && question.len() <= 8192,
        "Question must contain 1..8192 bytes"
    );
    ensure!(
        (1..=900).contains(&config.timeout_secs),
        "Host timeout must be 1..900 seconds"
    );
    ensure!(
        (1..=64).contains(&config.max_tool_calls),
        "Host tool-call limit must be 1..64"
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
