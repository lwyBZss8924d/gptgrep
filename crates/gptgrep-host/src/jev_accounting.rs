use crate::{Citation, ModelAttempt, ModelUsage, QueryPlanReport, ToolReceipt};

#[derive(Debug, Serialize)]
pub(crate) struct InitializationError {
    pub cause: String,
}
impl std::fmt::Display for InitializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host_jev_initialization_failed")
    }
}
impl std::error::Error for InitializationError {}
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::{
    Coverage, JevSearchProgress, Metrics, PlannedCoverage, PlannedSearchEvent, PlannedSearchReport,
    SearchReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlannedSearchTelemetry {
    pub coverage: Option<PlannedCoverage>,
    pub operations: BTreeMap<String, PlannedSearchEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchTelemetry {
    pub search_id: String,
    pub required_initial: bool,
    pub mode: String,
    pub document_scope: Option<String>,
    pub generation: Option<String>,
    pub query_sha256: String,
    pub status: String,
    pub stage: String,
    pub metrics: Metrics,
    pub coverage: Option<Coverage>,
    pub delivered_hits: usize,
    pub output_truncated: bool,
    pub accounting_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlannedSearchTelemetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevReport {
    pub required: bool,
    pub initial_status: String,
    /// Successful validated responses; attempted calls are reported separately.
    pub requests: usize,
    /// Logical client-call attempts, not proof that an HTTP request was sent.
    pub attempted_calls: usize,
    pub unobserved_attempts: usize,
    pub models: Vec<String>,
    pub usage: Vec<Value>,
    pub accounting_complete: bool,
    pub searches: Vec<SearchTelemetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptSummary {
    pub call_id: String,
    pub tool: String,
    pub required_initial: bool,
    pub arguments_sha256: String,
    pub generation: String,
    pub success: bool,
    pub output_sha256: String,
    pub evidence: Vec<Citation>,
    pub search: Option<SearchTelemetry>,
}

impl From<&ToolReceipt> for ReceiptSummary {
    fn from(receipt: &ToolReceipt) -> Self {
        Self {
            call_id: receipt.call_id.clone(),
            tool: receipt.tool.clone(),
            required_initial: receipt.required_initial,
            arguments_sha256: crate::retrieval::hash(
                &serde_json::to_vec(&receipt.arguments).unwrap_or_default(),
            ),
            generation: receipt.generation.clone(),
            success: receipt.success,
            output_sha256: receipt.output_sha256.clone(),
            evidence: receipt.evidence.clone(),
            search: receipt.search.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HostRetrievalError {
    pub code: String,
    pub stage: String,
    pub generation: String,
    pub ledger_path: PathBuf,
    pub jev: JevReport,
    pub receipts: Vec<ReceiptSummary>,
    pub cause: Option<Value>,
    pub elapsed_ms: u128,
    pub usage_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_plan: Option<QueryPlanReport>,
    pub model_attempts: Vec<ModelAttempt>,
    pub model_usage: ModelUsage,
}
impl std::fmt::Display for HostRetrievalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.stage)
    }
}
impl std::error::Error for HostRetrievalError {}

#[derive(Clone)]
pub(crate) struct Accounting(Arc<Mutex<State>>);
struct State {
    file: File,
    path: PathBuf,
    generation: String,
    searches: Vec<SearchTelemetry>,
    search_started: Vec<Instant>,
    model_attempt_limit: usize,
    model_attempts: Vec<ModelAttempt>,
    workflow: Option<Value>,
    bytes: usize,
    events: usize,
    terminal: bool,
}

/// Dropping the outer workflow leaves a durable incomplete marker, including on cancellation.
pub(crate) struct AttemptGuard(pub Accounting);
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        let _ = self.0.finish("interrupted");
    }
}

impl Accounting {
    pub fn create(root: &Path, generation: &str) -> Result<Self> {
        let root = root.canonicalize()?;
        let (file, path) = create_ledger(&root)?;
        let accounting = Self(Arc::new(Mutex::new(State {
            file,
            path,
            generation: generation.into(),
            searches: vec![],
            search_started: vec![],
            model_attempt_limit: 1,
            model_attempts: vec![],
            workflow: None,
            bytes: 0,
            events: 0,
            terminal: false,
        })));
        accounting.append("created", None, None)?;
        Ok(accounting)
    }
    pub fn path(&self) -> PathBuf {
        self.0.lock().expect("accounting mutex").path.clone()
    }
    pub fn summary(&self) -> JevReport {
        summarize(&self.0.lock().expect("accounting mutex").searches)
    }
    pub fn bind_workflow(&self, question: &str, document_scope: Option<&str>) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(
            !state.terminal && state.model_attempts.is_empty(),
            "host_workflow_binding_closed"
        );
        let workflow = json!({"query_sha256":crate::retrieval::hash(question.as_bytes()),"document_scope":document_scope,"generation":state.generation});
        ensure!(
            state
                .workflow
                .as_ref()
                .is_none_or(|prior| prior == &workflow),
            "host_workflow_binding_changed"
        );
        state.workflow = Some(workflow.clone());
        append_locked(
            &mut state,
            "workflow_bound",
            None,
            Some(json!({"workflow":workflow})),
        )
    }
    pub fn set_model_attempt_limit(&self, limit: usize) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(
            !state.terminal && state.model_attempts.is_empty() && (1..=2).contains(&limit),
            "host_model_attempt_budget_invalid"
        );
        state.model_attempt_limit = limit;
        append_locked(
            &mut state,
            "model_attempt_budget",
            None,
            Some(json!({"model_attempt_limit":limit})),
        )
    }
    pub fn reserve_model_attempt(&self, attempt: ModelAttempt) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(
            !state.terminal
                && state.workflow.is_some()
                && state.model_attempts.len() < state.model_attempt_limit,
            "host_model_attempt_limit"
        );
        ensure!(
            matches!(attempt.role.as_str(), "query_planner" | "final_reader")
                && attempt.status == "reserved",
            "host_model_attempt_invalid"
        );
        ensure!(
            !state
                .model_attempts
                .iter()
                .any(|prior| prior.attempt_id == attempt.attempt_id || prior.role == attempt.role),
            "host_model_attempt_duplicate"
        );
        ensure!(
            attempt.attempt_id.len() <= 128 && !attempt.attempt_id.is_empty(),
            "host_model_attempt_invalid"
        );
        let detail = json!({"model_attempt":attempt});
        state.model_attempts.push(attempt);
        append_locked(&mut state, "model_attempt_reserved", None, Some(detail))
    }
    pub fn record_model_attempt(&self, attempt: &ModelAttempt) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(!state.terminal, "host_model_attempt_closed");
        let previous = state
            .model_attempts
            .iter_mut()
            .find(|prior| prior.attempt_id == attempt.attempt_id)
            .ok_or_else(|| anyhow!("host_model_attempt_unreserved"))?;
        ensure!(
            previous.role == attempt.role
                && previous.requested_model == attempt.requested_model
                && previous.requested_reasoning_effort == attempt.requested_reasoning_effort
                && previous.requested_service_tier == attempt.requested_service_tier
                && previous
                    .thread_id
                    .as_ref()
                    .is_none_or(|id| attempt.thread_id.as_ref() == Some(id))
                && previous
                    .turn_id
                    .as_ref()
                    .is_none_or(|id| attempt.turn_id.as_ref() == Some(id)),
            "host_model_attempt_identity_changed"
        );
        *previous = attempt.clone();
        append_locked(
            &mut state,
            "model_attempt_updated",
            None,
            Some(json!({"model_attempt":attempt})),
        )
    }
    pub fn model_attempts(&self) -> Vec<ModelAttempt> {
        self.0
            .lock()
            .expect("accounting mutex")
            .model_attempts
            .clone()
    }
    pub fn start_search(
        &self,
        id: &str,
        query: &str,
        mode: &str,
        document: Option<&str>,
        required: bool,
    ) -> Result<usize> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(state.searches.len() < 128, "host_jev_ledger_search_limit");
        ensure!(
            !state.searches.iter().any(|search| search.search_id == id),
            "host_jev_duplicate_search_id"
        );
        let index = state.searches.len();
        state.searches.push(SearchTelemetry {
            search_id: id.into(),
            required_initial: required,
            mode: mode.into(),
            document_scope: document.map(str::to_owned),
            generation: None,
            query_sha256: crate::retrieval::hash(query.as_bytes()),
            status: "running".into(),
            stage: "preflight".into(),
            metrics: Metrics::default(),
            coverage: None,
            delivered_hits: 0,
            output_truncated: false,
            accounting_complete: false,
            plan: None,
        });
        state.search_started.push(Instant::now());
        append_locked(&mut state, "search_started", Some(index), None)?;
        Ok(index)
    }
    pub fn progress(&self, index: usize, progress: &JevSearchProgress) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        let search = &mut state.searches[index];
        search.stage = progress.stage.clone();
        search.metrics = progress.metrics.clone();
        search.coverage = Some(progress.coverage.clone());
        search.document_scope = progress.document_scope.clone();
        search.generation = progress.generation.clone();
        search.accounting_complete = false;
        append_locked(&mut state, &progress.event, Some(index), None)
    }
    pub fn plan_event(&self, index: usize, event: &PlannedSearchEvent) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(!state.terminal, "host_jev_ledger_closed");
        ensure!(
            matches!(
                event.operation_id.as_str(),
                "q0.route" | "q1.route" | "q2.route" | "union.rerank"
            ),
            "host_jev_plan_operation_invalid"
        );
        ensure!(
            matches!(
                event.event.as_str(),
                "admitted" | "before_call" | "after_reply" | "finished" | "failed" | "interrupted"
            ) && event.metrics.jev_calls_attempted <= 1
                && event.metrics.jev_requests <= event.metrics.jev_calls_attempted,
            "host_jev_plan_event_invalid"
        );
        let elapsed_ms = state.search_started[index].elapsed().as_millis();
        let search = &mut state.searches[index];
        ensure!(
            event.original_question_sha256 == search.query_sha256,
            "host_jev_plan_question_changed"
        );
        let plan = search
            .plan
            .get_or_insert_with(PlannedSearchTelemetry::default);
        if let Some(prior) = plan.operations.get(&event.operation_id) {
            ensure!(
                prior.query_sha256 == event.query_sha256
                    && prior.metrics.jev_calls_attempted <= event.metrics.jev_calls_attempted
                    && prior.metrics.jev_requests <= event.metrics.jev_requests,
                "host_jev_plan_event_regressed"
            );
        }
        plan.operations
            .insert(event.operation_id.clone(), event.clone());
        search.metrics = plan_metrics(&plan.operations, elapsed_ms);
        search.coverage = None;
        search.stage = event.operation_id.clone();
        search.accounting_complete = false;
        append_locked(
            &mut state,
            "plan_progress",
            Some(index),
            Some(json!({"operation_id":event.operation_id,"event":event.event})),
        )
    }
    pub fn planned_success(
        &self,
        index: usize,
        report: &PlannedSearchReport,
    ) -> Result<SearchTelemetry> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        let search = &mut state.searches[index];
        let plan = search
            .plan
            .get_or_insert_with(PlannedSearchTelemetry::default);
        for event in &report.operations {
            plan.operations
                .insert(event.operation_id.clone(), event.clone());
        }
        let observed = plan_metrics(&plan.operations, report.metrics.elapsed_ms);
        ensure!(
            observed.jev_calls_attempted == report.metrics.jev_calls_attempted
                && observed.jev_requests == report.metrics.jev_requests,
            "host_jev_plan_accounting_mismatch"
        );
        plan.coverage = Some(report.coverage.clone());
        search.metrics = observed;
        search.coverage = None;
        search.generation = Some(report.generation.clone());
        search.document_scope = report.document_scope.clone();
        search.stage = "delivery".into();
        search.status = "awaiting_delivery".into();
        search.accounting_complete = false;
        let result = search.clone();
        append_locked(&mut state, "plan_fused", Some(index), None)?;
        Ok(result)
    }
    pub fn success(
        &self,
        index: usize,
        report: &SearchReport,
        delivered: usize,
        truncated: bool,
    ) -> Result<SearchTelemetry> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        let search = &mut state.searches[index];
        search.metrics = report.metrics.clone();
        search.coverage = Some(report.coverage.clone());
        search.generation = Some(report.generation.clone());
        search.document_scope = report.document_scope.clone();
        search.delivered_hits = delivered;
        search.output_truncated = truncated;
        search.stage = "finished".into();
        search.status = if report.coverage.indexed_files == 0 {
            "empty_corpus"
        } else if report.coverage.reranked_candidates == 0
            && matches!(report.mode.as_str(), "hybrid" | "semantic")
        {
            "no_candidates"
        } else if report.hits.is_empty() && report.coverage.reranked_candidates > 0 {
            "filtered_all"
        } else if matches!(report.mode.as_str(), "hybrid" | "semantic") {
            "reranked"
        } else {
            "local_refinement"
        }
        .into();
        search.accounting_complete = report.metrics.jev_calls_attempted
            == report.metrics.jev_requests
            && report.metrics.jev_usage.len() == report.metrics.jev_requests
            && report.metrics.jev_usage.iter().all(usage_observed);
        let result = search.clone();
        append_locked(&mut state, "search_finished", Some(index), None)?;
        Ok(result)
    }
    pub fn failure(&self, index: usize, error: &anyhow::Error) -> Result<SearchTelemetry> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        let search = &mut state.searches[index];
        search.status = "failed".into();
        search.accounting_complete = false;
        if error.downcast_ref::<InitializationError>().is_some() {
            search.stage = "initialization".into();
        }
        if let Some(error) = error.downcast_ref::<gptgrep_core::JevSearchError>() {
            search.stage = error.stage.clone();
            search.metrics = error.metrics.clone();
            search.coverage = Some(error.coverage.clone());
            search.document_scope = error.document_scope.clone();
            search.generation = error.generation.clone();
        }
        if let Some(error) = error.downcast_ref::<gptgrep_core::PlannedSearchError>() {
            search.stage = error.stage.clone();
            search.document_scope = error.document_scope.clone();
            search.generation = error.generation.clone();
            search
                .plan
                .get_or_insert_with(PlannedSearchTelemetry::default)
                .coverage = Some(error.coverage.clone());
        }
        let result = search.clone();
        append_locked(&mut state, "search_failed", Some(index), None)?;
        Ok(result)
    }
    pub fn receipt(&self, receipt: &ToolReceipt) -> Result<()> {
        self.append("receipt", None, Some(json!(ReceiptSummary::from(receipt))))
    }
    pub fn delivery(&self, search: &SearchTelemetry) -> Result<SearchTelemetry> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        let index = state
            .searches
            .iter()
            .position(|value| value.search_id == search.search_id)
            .ok_or_else(|| anyhow!("host_jev_ledger_search_missing"))?;
        state.searches[index] = search.clone();
        let current = &mut state.searches[index];
        if current.plan.is_some() && current.status == "awaiting_delivery" {
            current.status = if current.delivered_hits == 0 {
                "filtered_all"
            } else {
                "reranked"
            }
            .into();
            current.stage = "finished".into();
            current.accounting_complete = current.metrics.jev_calls_attempted
                == current.metrics.jev_requests
                && current.metrics.jev_usage.len() == current.metrics.jev_requests
                && current.metrics.jev_usage.iter().all(usage_observed);
        }
        let result = current.clone();
        append_locked(&mut state, "delivery", Some(index), None)?;
        Ok(result)
    }
    pub fn finish(&self, status: &str) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        if state.terminal {
            return Ok(());
        }
        if status != "completed" {
            for search in &mut state.searches {
                if matches!(search.status.as_str(), "running" | "awaiting_delivery") {
                    search.status = "failed".into();
                    search.accounting_complete = false;
                }
            }
            for attempt in &mut state.model_attempts {
                if matches!(
                    attempt.status.as_str(),
                    "reserved" | "running" | "process_completed"
                ) {
                    attempt.status = if status == "interrupted" {
                        "interrupted"
                    } else {
                        "failed"
                    }
                    .into();
                    attempt.accounting_complete = false;
                }
            }
        }
        append_locked(&mut state, status, None, None)?;
        state.terminal = true;
        Ok(())
    }
    fn append(&self, event: &str, index: Option<usize>, detail: Option<Value>) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        append_locked(&mut state, event, index, detail)
    }
}

fn plan_metrics(operations: &BTreeMap<String, PlannedSearchEvent>, elapsed_ms: u128) -> Metrics {
    let mut metrics = Metrics {
        elapsed_ms,
        ..Metrics::default()
    };
    for operation in operations.values() {
        metrics.jev_calls_attempted += operation.metrics.jev_calls_attempted;
        metrics.jev_requests += operation.metrics.jev_requests;
        metrics.jev_candidate_bytes += operation.metrics.jev_candidate_bytes;
        metrics
            .jev_models
            .extend(operation.metrics.jev_models.iter().cloned());
        metrics
            .jev_usage
            .extend(operation.metrics.jev_usage.iter().cloned());
    }
    metrics
}

fn usage_observed(usage: &Value) -> bool {
    usage.get("input_tokens").and_then(Value::as_u64).is_some()
        && usage.get("output_tokens").and_then(Value::as_u64).is_some()
        && usage.get("cost").and_then(Value::as_f64).is_some()
}
fn summarize(searches: &[SearchTelemetry]) -> JevReport {
    let requests = searches.iter().map(|s| s.metrics.jev_requests).sum();
    let attempted_calls = searches
        .iter()
        .map(|s| s.metrics.jev_calls_attempted)
        .sum::<usize>();
    let models: BTreeSet<_> = searches
        .iter()
        .flat_map(|s| s.metrics.jev_models.iter().cloned())
        .collect();
    JevReport {
        required: true,
        initial_status: searches
            .iter()
            .find(|s| s.required_initial)
            .map_or("not_started", |s| s.status.as_str())
            .into(),
        requests,
        attempted_calls,
        unobserved_attempts: attempted_calls.saturating_sub(requests),
        models: models.into_iter().collect(),
        usage: searches
            .iter()
            .flat_map(|s| s.metrics.jev_usage.iter().cloned())
            .collect(),
        accounting_complete: !searches.is_empty() && searches.iter().all(|s| s.accounting_complete),
        searches: searches.to_vec(),
    }
}
fn append_locked(
    state: &mut State,
    event: &str,
    index: Option<usize>,
    detail: Option<Value>,
) -> Result<()> {
    let summary = summarize(&state.searches);
    let entry = json!({
        "schema_version":"gptgrep.jev-attempt.v1","event":event,"generation":state.generation,
        "observed_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        "accounting_complete":event=="completed" && summary.accounting_complete,
        "initial_status":summary.initial_status,"requests":summary.requests,
        "attempted_calls":summary.attempted_calls,"unobserved_attempts":summary.unobserved_attempts,
        "search":index.map(|index|&state.searches[index]),"receipt":detail,
        "model_attempt_limit":state.model_attempt_limit,"model_attempts":state.model_attempts,
        "model_usage":crate::model_attempts::summarize(&state.model_attempts),
        "workflow":state.workflow,
    });
    let mut bytes = serde_json::to_vec(&entry)?;
    bytes.push(b'\n');
    ensure!(
        state.events < 1024 && bytes.len() <= 65536 && state.bytes + bytes.len() <= 4 * 1024 * 1024,
        "host_jev_ledger_limit"
    );
    state.bytes += bytes.len();
    state.events += 1;
    state
        .file
        .write_all(&bytes)
        .map_err(|_| anyhow!("host_jev_ledger_write_failed"))?;
    state
        .file
        .sync_data()
        .map_err(|_| anyhow!("host_jev_ledger_sync_failed"))?;
    Ok(())
}

fn create_ledger(root: &Path) -> Result<(File, PathBuf)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let state = root.join(".gptgrep");
    let metadata = std::fs::symlink_metadata(&state)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && state.canonicalize()? == state,
        "host_jev_ledger_path"
    );
    let directory = state.join("host-attempts");
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = format!(
        "attempt-{nonce}-{}-{}.jsonl",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = directory.join(&name);
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::OpenOptionsExt;
        let state_fd = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&state)?;
        let dirname = std::ffi::CString::new("host-attempts")?;
        let made = unsafe { libc::mkdirat(state_fd.as_raw_fd(), dirname.as_ptr(), 0o700) };
        ensure!(
            made == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
            "host_jev_ledger_directory"
        );
        let fd = unsafe {
            libc::openat(
                state_fd.as_raw_fd(),
                dirname.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        ensure!(fd >= 0, "host_jev_ledger_path");
        let dir = unsafe { File::from_raw_fd(fd) };
        ensure!(
            directory.canonicalize()? == directory,
            "host_jev_ledger_path"
        );
        let name = std::ffi::CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                0o600,
            )
        };
        ensure!(fd >= 0, "host_jev_ledger_create");
        Ok((unsafe { File::from_raw_fd(fd) }, path))
    }
    #[cfg(not(unix))]
    {
        if !directory.exists() {
            std::fs::create_dir(&directory)?;
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && directory.canonicalize()? == directory,
            "host_jev_ledger_path"
        );
        Ok((
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?,
            path,
        ))
    }
}
