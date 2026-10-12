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
    Coverage, EvidenceRoleDiagnostics, JevSearchProgress, Metrics, PlannedCoverage,
    PlannedSearchEvent, PlannedSearchReport, SearchReport,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_roles: Option<EvidenceRoleDiagnostics>,
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
    /// Served (provider-returned) Decisions builds of validated responses.
    pub models: Vec<String>,
    /// Requested Decisions selectors of all attempts, reported separately.
    #[serde(default)]
    pub requested_models: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_budget: Option<crate::ToolBudgetDiagnostics>,
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
            tool_budget: receipt.tool_budget,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation: Option<crate::NavigationQueryReport>,
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
    navigation: Option<crate::NavigationQueryReport>,
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
            navigation: None,
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
        summarize_state(&self.0.lock().expect("accounting mutex"))
    }
    pub fn navigation(&self) -> Option<crate::NavigationQueryReport> {
        self.0.lock().expect("accounting mutex").navigation.clone()
    }
    pub fn navigation_admit(&self, report: &crate::NavigationQueryReport) -> Result<()> {
        let state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        // Simulate the largest bounded metadata/score receipt before any request
        // is admitted. Existing event/record/ledger caps remain unchanged.
        let mut worst = report.clone();
        worst.status = "completed_empty".into();
        worst.error_code = Some("x".repeat(96));
        worst.packet_sha256 = Some("f".repeat(64));
        worst.packet_bytes = Some(8192);
        worst.selected = (0..8)
            .map(|_| crate::NavigationSelection {
                hint_id: "f".repeat(64),
                score: 1.2345678901234567e-300,
            })
            .collect();
        let compact = worst.compact()?;
        let terminal = entry_bytes(
            &state,
            "navigation_completed",
            None,
            Some(&json!({"navigation":compact})),
        )?
        .len()
            + 2048;
        let mut required_bytes = terminal * 2;
        for batch in &report.batches {
            let mut batch = batch.clone();
            batch.status = "reply_received".into();
            batch.model = Some("\"".repeat(256));
            batch.provider = batch.model.clone();
            batch.provider_response_id = batch.model.clone();
            batch.error_code = Some("x".repeat(96));
            batch.elapsed_ms = Some(u64::MAX);
            batch.scores_sha256 = Some("f".repeat(64));
            batch.usage = Some(
                json!({"input_tokens":u64::MAX,"output_tokens":u64::MAX,"prompt_tokens":u64::MAX,"completion_tokens":u64::MAX,"total_tokens":u64::MAX,"cost":1.2345678901234567e300}),
            );
            let scores: BTreeMap<_, _> = (0..batch.candidates)
                .map(|index| (format!("{index:064x}"), 1.2345678901234567e-300))
                .collect();
            let detail = json!({"navigation":compact,"batch":batch,"scores":scores});
            let bytes =
                entry_bytes(&state, "navigation_after_call", None, Some(&detail))?.len() + 2048;
            ensure!(bytes <= 65536, "host_navigation_ledger_record_admission");
            required_bytes = required_bytes
                .checked_add(bytes * 3)
                .ok_or_else(|| anyhow!("host_navigation_ledger_admission"))?;
        }
        ensure!(
            state.events + report.batches.len() * 3 + 2 < 1024
                && state.bytes + required_bytes <= 4 * 1024 * 1024,
            "host_navigation_ledger_admission"
        );
        drop(state);
        self.navigation_update("navigation_admitted", report, None, None)
    }
    pub fn navigation_update(
        &self,
        event: &str,
        report: &crate::NavigationQueryReport,
        batch: Option<usize>,
        scores: Option<Value>,
    ) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(
            !state.terminal
                && report.generation == state.generation
                && report.batches.len() <= crate::navigation::MAX_BATCHES
                && report.requests <= report.attempted_calls
                && report.attempted_calls <= report.batches.len(),
            "host_navigation_accounting_invalid"
        );
        let workflow = state
            .workflow
            .as_ref()
            .ok_or_else(|| anyhow!("host_navigation_workflow_unbound"))?;
        ensure!(
            workflow["query_sha256"] == report.query_sha256
                && workflow["document_scope"] == report.document_scope,
            "host_navigation_workflow_changed"
        );
        if let Some(prior) = &state.navigation {
            ensure!(
                prior.artifact_sha256 == report.artifact_sha256
                    && prior.query_sha256 == report.query_sha256
                    && prior.attempted_calls <= report.attempted_calls
                    && prior.requests <= report.requests,
                "host_navigation_accounting_regressed"
            );
        }
        let row = batch
            .map(|index| {
                report
                    .batches
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("host_navigation_batch_invalid"))
            })
            .transpose()?;
        let detail = json!({"navigation":report.compact()?,"batch":row,"scores":scores});
        state.navigation = Some(report.clone());
        append_locked(&mut state, event, None, Some(detail))
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
        let mut operation = event.clone();
        let diagnostics = operation.evidence_roles.take();
        plan.operations
            .insert(event.operation_id.clone(), operation);
        search.metrics = plan_metrics(&plan.operations, elapsed_ms);
        search.coverage = None;
        search.stage = event.operation_id.clone();
        search.accounting_complete = false;
        if let Some(diagnostics) = diagnostics {
            diagnostics.validate_bound()?;
            ensure!(
                diagnostics.original_question_sha256 == search.query_sha256,
                "host_evidence_role_question_changed"
            );
            search.plan.as_mut().expect("planned search").evidence_roles = Some(diagnostics);
        }
        let detail = json!({"operation_id":event.operation_id,"event":event.event});
        if event.event == "before_call"
            && let Some(diagnostics) = state.searches[index]
                .plan
                .as_ref()
                .and_then(|plan| plan.evidence_roles.as_ref())
        {
            let actual = serde_json::to_vec(diagnostics)?.len();
            let extra = diagnostics
                .reserved_metadata_bytes
                .checked_sub(actual)
                .ok_or_else(|| anyhow!("host_evidence_role_reservation_invalid"))?;
            let reserved = entry_bytes(&state, "plan_progress", Some(index), Some(&detail))?
                .len()
                .checked_add(extra)
                .ok_or_else(|| anyhow!("host_evidence_role_reservation_invalid"))?;
            ensure!(
                state.events < 1024
                    && reserved <= 65536
                    && state.bytes + reserved <= 4 * 1024 * 1024,
                "host_evidence_role_ledger_reservation"
            );
        }
        append_locked(&mut state, "plan_progress", Some(index), Some(detail))
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
            let mut operation = event.clone();
            operation.evidence_roles = None;
            plan.operations
                .insert(event.operation_id.clone(), operation);
        }
        if let Some(diagnostics) = &report.evidence_roles {
            diagnostics.validate_bound()?;
        }
        plan.evidence_roles = report.evidence_roles.clone();
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
    pub fn role_delivery(
        &self,
        index: usize,
        diagnostics: &EvidenceRoleDiagnostics,
    ) -> Result<SearchTelemetry> {
        diagnostics.validate_bound()?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_jev_ledger_unavailable"))?;
        ensure!(!state.terminal, "host_jev_ledger_closed");
        let search = &mut state.searches[index];
        ensure!(
            search.status == "awaiting_delivery",
            "host_evidence_role_delivery_state"
        );
        let plan = search
            .plan
            .as_mut()
            .ok_or_else(|| anyhow!("host_evidence_role_assessment_missing"))?;
        let prior = plan
            .evidence_roles
            .as_ref()
            .ok_or_else(|| anyhow!("host_evidence_role_assessment_missing"))?;
        // Delivery may update dispositions and actual spans, never the judgments
        // or their original identity. Keep all <=24 rows, including exclusions.
        let strip_delivery = |value: &EvidenceRoleDiagnostics| -> Result<Value> {
            let mut value = serde_json::to_value(value)?;
            value
                .as_object_mut()
                .expect("diagnostics object")
                .remove("delivery_packet_sha256");
            for candidate in value["candidates"].as_array_mut().expect("candidate array") {
                candidate
                    .as_object_mut()
                    .expect("candidate object")
                    .remove("disposition");
                candidate
                    .as_object_mut()
                    .expect("candidate object")
                    .remove("delivered_span");
            }
            Ok(value)
        };
        ensure!(
            strip_delivery(prior)? == strip_delivery(diagnostics)?,
            "host_evidence_role_assessment_changed"
        );
        let previous = search.clone();
        let mut result = previous.clone();
        result.plan.as_mut().expect("planned search").evidence_roles = Some(diagnostics.clone());
        // Reserve the exact future delivery entry, but keep durable dispositions
        // pending until issue succeeds. A prepared packet is not yet delivered.
        state.searches[index] = result.clone();
        let projected = entry_bytes(&state, "delivery", Some(index), None);
        state.searches[index] = previous;
        let projected = projected?.len();
        let detail = json!({"delivery_packet_sha256":diagnostics.delivery_packet_sha256});
        let marker =
            entry_bytes(&state, "role_delivery_prepared", Some(index), Some(&detail))?.len();
        ensure!(
            state.events + 2 <= 1024
                && projected <= 65536
                && marker <= 65536
                && state.bytes + projected + marker <= 4 * 1024 * 1024,
            "host_evidence_role_ledger_reservation"
        );
        append_locked(
            &mut state,
            "role_delivery_prepared",
            Some(index),
            Some(detail),
        )?;
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
        if let Some(diagnostics) = search
            .plan
            .as_mut()
            .and_then(|plan| plan.evidence_roles.as_mut())
        {
            diagnostics.mark_delivery_failure();
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
            if let Some(navigation) = &mut state.navigation {
                navigation.interrupt(if status == "interrupted" {
                    "host_navigation_interrupted"
                } else {
                    "host_navigation_failed"
                });
            }
            for search in &mut state.searches {
                if matches!(search.status.as_str(), "running" | "awaiting_delivery") {
                    search.status = "failed".into();
                    search.accounting_complete = false;
                    if let Some(diagnostics) = search
                        .plan
                        .as_mut()
                        .and_then(|plan| plan.evidence_roles.as_mut())
                    {
                        diagnostics.mark_delivery_failure();
                    }
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
            .jev_requested_models
            .extend(operation.metrics.jev_requested_models.iter().cloned());
        metrics
            .jev_usage
            .extend(operation.metrics.jev_usage.iter().cloned());
        metrics
            .jev_retried_attempts
            .extend(operation.metrics.jev_retried_attempts.iter().cloned());
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
    let requested_models: BTreeSet<_> = searches
        .iter()
        .flat_map(|s| s.metrics.jev_requested_models.iter().cloned())
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
        requested_models: requested_models.into_iter().collect(),
        usage: searches
            .iter()
            .flat_map(|s| s.metrics.jev_usage.iter().cloned())
            .collect(),
        accounting_complete: !searches.is_empty() && searches.iter().all(|s| s.accounting_complete),
        searches: searches.to_vec(),
    }
}
fn summarize_state(state: &State) -> JevReport {
    let mut summary = summarize(&state.searches);
    if let Some(navigation) = &state.navigation {
        summary.attempted_calls += navigation.attempted_calls;
        summary.requests += navigation.requests;
        summary.unobserved_attempts += navigation
            .attempted_calls
            .saturating_sub(navigation.requests);
        let mut models: BTreeSet<_> = summary.models.into_iter().collect();
        let mut requested: BTreeSet<_> = summary.requested_models.into_iter().collect();
        for batch in &navigation.batches {
            requested.insert(batch.requested_model.clone());
            if let Some(model) = &batch.model {
                models.insert(model.clone());
                summary
                    .usage
                    .push(batch.usage.clone().unwrap_or(Value::Null));
            }
        }
        summary.models = models.into_iter().collect();
        summary.requested_models = requested.into_iter().collect();
        summary.accounting_complete &= navigation.accounting_complete();
    }
    summary
}
fn append_locked(
    state: &mut State,
    event: &str,
    index: Option<usize>,
    detail: Option<Value>,
) -> Result<()> {
    let bytes = entry_bytes(state, event, index, detail.as_ref())?;
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

fn entry_bytes(
    state: &State,
    event: &str,
    index: Option<usize>,
    detail: Option<&Value>,
) -> Result<Vec<u8>> {
    let summary = summarize_state(state);
    let mut entry = json!({
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
    if let Some(navigation) = &state.navigation {
        entry["navigation"] = navigation.compact()?;
    }
    let mut bytes = serde_json::to_vec(&entry)?;
    bytes.push(b'\n');
    Ok(bytes)
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

#[cfg(test)]
mod evidence_role_storage_tests {
    use super::*;
    use crate::evidence_roles::tests::{QUESTION, diagnostics, mock_roles, source_hit};
    use gptgrep_core::{CandidateDisposition, PlannedScoringPolicy, SearchOptions};

    #[tokio::test]
    async fn source_bound_reservation_fails_before_mixed_provider_call() {
        let (root, generation, _) = source_hit("Copper container has a ceramic liner.\n").await;
        let accounting = Accounting::create(root.path(), &generation).unwrap();
        let index = accounting
            .start_search("initial", QUESTION, "hybrid", None, true)
            .unwrap();
        // One document: the view selects it without a routing call, so the
        // mixed request is the first and only admission.
        let (client, server) = mock_roles(0).await;
        let observer = |event: &PlannedSearchEvent| {
            if event.evidence_roles.is_some() && event.event == "before_call" {
                accounting.0.lock().unwrap().bytes = 4 * 1024 * 1024 - 4096;
            }
            accounting.plan_event(index, event)
        };
        let error = gptgrep_core::search_planned_with_policy_and_observer(
            root.path(),
            QUESTION,
            &[],
            &SearchOptions::default(),
            &generation,
            &client,
            PlannedScoringPolicy::EvidenceRoles,
            &observer,
        )
        .await
        .unwrap_err();
        assert_eq!(server.await.unwrap().len(), 0);
        let failure = error
            .downcast_ref::<gptgrep_core::PlannedSearchError>()
            .unwrap();
        assert!(failure.cause.contains("observer"));
        assert_eq!(failure.metrics.jev_requests, 0);
        assert_eq!(accounting.summary().requests, 0);
        assert!(!accounting.summary().accounting_complete);
    }

    #[tokio::test]
    async fn reply_storage_failure_preserves_observed_usage_and_judgments() {
        let (root, generation, _) = source_hit("Copper container has a ceramic liner.\n").await;
        let accounting = Accounting::create(root.path(), &generation).unwrap();
        let index = accounting
            .start_search("initial", QUESTION, "hybrid", None, true)
            .unwrap();
        let ledger = accounting.path();
        // One document: no routing call; the mixed union request is the only one.
        let (client, server) = mock_roles(1).await;
        let observer = |event: &PlannedSearchEvent| {
            if event.evidence_roles.is_some() && event.event == "after_reply" {
                accounting.0.lock().unwrap().file = File::open(&ledger).unwrap();
            }
            accounting.plan_event(index, event)
        };
        let error = gptgrep_core::search_planned_with_policy_and_observer(
            root.path(),
            QUESTION,
            &[],
            &SearchOptions::default(),
            &generation,
            &client,
            PlannedScoringPolicy::EvidenceRoles,
            &observer,
        )
        .await
        .unwrap_err();
        assert_eq!(server.await.unwrap().len(), 1);
        assert!(
            error
                .downcast_ref::<gptgrep_core::PlannedSearchError>()
                .unwrap()
                .cause
                .contains("observer")
        );
        let summary = accounting.summary();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.usage.len(), 1);
        // Served builds and requested selectors are reported separately.
        assert_eq!(summary.requested_models, [gptgrep_jev::DEFAULT_MODEL]);
        assert!(
            summary
                .models
                .iter()
                .all(|model| model != gptgrep_jev::DEFAULT_MODEL)
        );
        assert!(!summary.accounting_complete);
        let plan = summary.searches[0].plan.as_ref().unwrap();
        assert!(
            plan.operations
                .values()
                .all(|operation| operation.evidence_roles.is_none())
        );
        assert!(
            plan.evidence_roles
                .as_ref()
                .unwrap()
                .candidates
                .iter()
                .all(|candidate| candidate.role.is_some() && candidate.relevance_score.is_some())
        );
    }

    #[tokio::test]
    async fn prepared_packet_does_not_promote_delivery_and_failure_clears_scope() {
        let (root, generation, hit) = source_hit("Copper container has a ceramic liner.\n").await;
        let accounting = Accounting::create(root.path(), &generation).unwrap();
        let index = accounting
            .start_search("initial", QUESTION, "hybrid", None, true)
            .unwrap();
        let original = diagnostics(&generation, std::slice::from_ref(&hit));
        {
            let mut state = accounting.0.lock().unwrap();
            state.searches[index].status = "awaiting_delivery".into();
            state.searches[index].plan = Some(PlannedSearchTelemetry {
                evidence_roles: Some(original.clone()),
                ..Default::default()
            });
        }
        let mut delivered = original.clone();
        delivered.record_delivery(&hit, Some(&hit)).unwrap();
        delivered.delivery_packet_sha256 = Some("d".repeat(64));
        let prepared = accounting.role_delivery(index, &delivered).unwrap();
        assert_eq!(
            prepared
                .plan
                .as_ref()
                .unwrap()
                .evidence_roles
                .as_ref()
                .unwrap()
                .candidates[0]
                .disposition,
            CandidateDisposition::Delivered
        );
        let retained = accounting.summary();
        assert_eq!(
            retained.searches[0]
                .plan
                .as_ref()
                .unwrap()
                .evidence_roles
                .as_ref()
                .unwrap()
                .candidates[0]
                .disposition,
            CandidateDisposition::AwaitingDelivery
        );
        let ledger = std::fs::read_to_string(accounting.path()).unwrap();
        let last: Value = serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(last["event"], "role_delivery_prepared");
        assert_eq!(
            last["search"]["plan"]["evidence_roles"]["candidates"][0]["disposition"],
            "awaiting_delivery"
        );
        assert!(
            last["search"]["plan"]["evidence_roles"]["candidates"][0]["delivered_span"].is_null()
        );
        let mut changed = original;
        changed.candidates[0].role_confidence = Some(0.99);
        assert!(
            accounting
                .role_delivery(index, &changed)
                .unwrap_err()
                .to_string()
                .contains("assessment_changed")
        );
        let failed = accounting
            .failure(index, &anyhow!("host_test_delivery_failed"))
            .unwrap();
        let roles = failed.plan.unwrap().evidence_roles.unwrap();
        assert_eq!(
            roles.candidates[0].disposition,
            CandidateDisposition::DeliveryFailed
        );
        assert!(
            roles.candidates[0].delivered_span.is_none() && roles.delivery_packet_sha256.is_none()
        );
        assert!(
            accounting
                .role_delivery(index, &changed)
                .unwrap_err()
                .to_string()
                .contains("delivery_state")
        );
    }
}
