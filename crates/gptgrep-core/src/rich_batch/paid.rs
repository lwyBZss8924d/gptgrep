//! Whole-batch admission around the genuine mandatory two-stage search.
//! Serial operations retain the underlying search's bounded concurrent drain.
use super::*;
use crate::{
    PreparedRichMultimodalSearch, RichMultimodalAttempt, RichMultimodalSearchOptions,
    RichMultimodalSearchReport, prepare_rich_multimodal_search,
};
use gptgrep_jev::{ExecutionSnapshot, JevClient, UsageObservation, UsagePresence};

const PAID_ITEM_OVERHEAD: usize = 4096;
const MAX_RETAINED_PREPARATION_BYTES: u64 = 268_435_456;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RichBatchItemDecisions {
    pub reserved_calls: usize,
    pub scheduled_logical_attempts: usize,
    pub validated_responses: usize,
    pub observed_cost_subtotal_usd: Option<f64>,
    pub observed_cost_attempts: usize,
    pub cost_complete: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RichBatchDecisionsMetrics {
    pub attempt_identity_scope: String,
    pub reserved_calls: usize,
    pub unique_scheduled_logical_attempts: usize,
    pub validated_responses: usize,
    pub observed_cost_subtotal_usd: Option<f64>,
    pub observed_cost_attempts: usize,
    pub cost_complete: bool,
    pub allocated: bool,
    pub actual_provider_requests: Option<u64>,
    pub cost_sum_overflowed: bool,
}

/// One retained reader, preparing client and deterministic output/call shares.
/// Preparation performs no submit, reservation, environment or credential read.
pub struct PreparedRichBatch {
    reader: RichGenerationReader,
    request: RichBatchRequest,
    client: Option<JevClient>,
    context_before: Option<ExecutionSnapshot>,
    quotas: Vec<usize>,
    starts: Vec<Result<usize, RichBatchError>>,
    searches: Vec<Option<PreparedRichMultimodalSearch>>,
    call_shares: Vec<usize>,
    before: ReaderReadMetrics,
    started: Instant,
}
impl PreparedRichBatch {
    pub fn generation(&self) -> GenerationPin {
        self.reader.pin()
    }
    pub fn reserved_output_bytes(&self) -> usize {
        self.request.budget.max_output_bytes
    }
    pub fn reserved_calls(&self) -> usize {
        self.call_shares.iter().sum()
    }
    pub fn item_call_shares(&self) -> &[usize] {
        &self.call_shares
    }
    /// Actual source-bound preparations, borrowed from this retained batch.
    /// Inspection grants no delivery/citation or execution rights.
    pub fn prepared_searches(&self) -> impl Iterator<Item = (&str, &PreparedRichMultimodalSearch)> {
        self.request
            .operations
            .iter()
            .zip(&self.searches)
            .filter_map(|(op, search)| search.as_ref().map(|search| (op.id(), search)))
    }
}

fn preparation_error(e: &anyhow::Error) -> RichBatchError {
    let code = if e.chain().any(|e| {
        let s = e.to_string();
        s.contains("source changed") || s.contains("source is stale")
    }) {
        RichBatchErrorCode::SourceStale
    } else if e.to_string().contains("budget_insufficient") || e.to_string().contains("overflow") {
        RichBatchErrorCode::OutputBudgetInsufficient
    } else if e.to_string().contains("deadline") {
        RichBatchErrorCode::DeadlineExceeded
    } else {
        RichBatchErrorCode::InvalidOperation
    };
    error(code, "Paid search preparation failed before any submission")
}
fn reader_admission(
    reader: &RichGenerationReader,
    request: &RichBatchRequest,
) -> Result<(), RichBatchError> {
    validate_request(request)?;
    let pin = reader.pin();
    if matches!(&request.generation, RichBatchGeneration::Exact(expected) if expected != &pin) {
        return Err(error(
            RichBatchErrorCode::InvalidGeneration,
            "Reader does not match the exact requested pin",
        ));
    }
    let cache = reader.cache_status().map_err(reader_error)?;
    if cache["capacity_bytes"]
        .as_u64()
        .is_none_or(|v| v > request.budget.cache.max_bytes as u64)
        || cache["capacity_entries"]
            .as_u64()
            .is_none_or(|v| v > request.budget.cache.max_entries as u64)
        || (request.budget.cache.strict_cache_only && cache["strict_cache_only"] != true)
    {
        return Err(error(
            RichBatchErrorCode::InvalidRequest,
            "Supplied reader cache exceeds declared budget or strict policy",
        ));
    }
    reader.validate().map_err(reader_error)
}

pub fn prepare_rich_batch_with_reader(
    reader: &RichGenerationReader,
    request: RichBatchRequest,
    client: Option<JevClient>,
) -> Result<PreparedRichBatch, Box<RichBatchFailure>> {
    let pin = reader.pin();
    reader_admission(reader, &request).map_err(|e| failure(e, Some(pin.clone()), vec![]))?;
    let before = reader
        .read_metrics()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), vec![]))?;
    let snapshot = client
        .as_ref()
        .map(|c| c.execution_context().snapshot())
        .transpose()
        .map_err(|_| {
            failure(
                error(
                    RichBatchErrorCode::ModelContextRequired,
                    "Execution context could not be inspected",
                ),
                Some(pin.clone()),
                vec![],
            )
        })?;
    let count = request.operations.len();
    let available = request.budget.max_output_bytes - ENVELOPE_RESERVE;
    let quotas: Vec<_> = (0..count)
        .map(|i| available / count + usize::from(i < available % count))
        .collect();
    let pending = || {
        request
            .operations
            .iter()
            .zip(&quotas)
            .map(|(op, quota)| RichBatchItem::pending(op.id(), *quota))
            .collect()
    };
    let manifest = reader
        .manifest()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), pending()))?;
    let mut starts = Vec::with_capacity(count);
    let mut searches = Vec::with_capacity(count);
    let mut call_shares = vec![0; count];
    let context_remaining = snapshot.as_ref().map_or(0, |s| {
        (s.max_submissions as usize).saturating_sub(s.logical_reservations)
    });
    let mut remaining_calls =
        (request.budget.max_decisions_submissions as usize).min(context_remaining);
    let mut retained = 0u64;
    for (i, op) in request.operations.iter().enumerate() {
        let mut ready = preflight(&pin, op).and_then(|start| {
            preflight_resource(manifest, op, start)?;
            Ok(start)
        });
        let mut search = None;
        if let RichBatchOperation::SearchMultimodal {
            query,
            document,
            window_bytes,
            max_visual_candidates,
            max_concurrent_batches,
            min_score,
            ..
        } = op
            && ready.is_ok()
        {
            ready = (|| {
                let client = client.as_ref().ok_or_else(|| {
                    error(
                        RichBatchErrorCode::ModelContextRequired,
                        "Paid operations require a finite Decisions client",
                    )
                })?;
                if client.execution_context().run_id().is_none()
                    || client.execution_context().plan_digest().is_none()
                {
                    return Err(error(
                        RichBatchErrorCode::ModelContextRequired,
                        "Paid operations require an explicit finite execution context",
                    ));
                }
                let result_budget = quotas[i]
                    .checked_sub(PAID_ITEM_OVERHEAD)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        error(
                            RichBatchErrorCode::OutputBudgetInsufficient,
                            "Paid item cannot reserve its complete result and accounting envelope",
                        )
                    })?;
                let mut resources = manifest.resource_budget.clone();
                resources.owned_working_bytes = resources
                    .owned_working_bytes
                    .min(MAX_RETAINED_PREPARATION_BYTES.saturating_sub(retained));
                let options = RichMultimodalSearchOptions {
                    document: document.clone(),
                    window_bytes: *window_bytes,
                    max_visual_candidates: *max_visual_candidates,
                    max_concurrent_batches: *max_concurrent_batches,
                    max_output_bytes: result_budget,
                    min_score: *min_score,
                    resources,
                    ..Default::default()
                };
                let prepared =
                    prepare_rich_multimodal_search(reader.clone(), client.clone(), query, &options)
                        .map_err(|e| preparation_error(&e))?;
                let calls =
                    prepared.routing_requests().count() + prepared.rerank_pool_requests().count();
                if calls > remaining_calls {
                    return Err(error(
                        RichBatchErrorCode::SubmissionBudgetInsufficient,
                        "Complete routing and rerank call share cannot fit the batch or parent allowance",
                    ));
                }
                retained = retained
                    .checked_add(prepared.working_bytes_bound())
                    .filter(|n| *n <= MAX_RETAINED_PREPARATION_BYTES)
                    .ok_or_else(|| {
                        error(
                            RichBatchErrorCode::OutputBudgetInsufficient,
                            "Retained paid preparations exceed the batch working bound",
                        )
                    })?;
                remaining_calls -= calls;
                call_shares[i] = calls;
                search = Some(prepared);
                Ok(0)
            })();
        }
        starts.push(ready);
        searches.push(search);
    }
    reader
        .validate()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), pending()))?;
    Ok(PreparedRichBatch {
        reader: reader.clone(),
        request,
        client,
        context_before: snapshot,
        quotas,
        starts,
        searches,
        call_shares,
        before,
        started: Instant::now(),
    })
}

fn observed<T>(
    usage: &Value,
    field: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> UsageObservation<T> {
    match usage.get(field) {
        None => UsageObservation {
            presence: UsagePresence::Absent,
            value: None,
        },
        Some(Value::Null) => UsageObservation {
            presence: UsagePresence::Null,
            value: None,
        },
        Some(value) => match parse(value) {
            Some(value) => UsageObservation {
                presence: UsagePresence::Reported,
                value: Some(value),
            },
            None => UsageObservation {
                presence: UsagePresence::Invalid,
                value: None,
            },
        },
    }
}
#[derive(Serialize)]
struct BatchUsage<'a> {
    input_tokens: UsageObservation<u64>,
    output_tokens: UsageObservation<u64>,
    cost: UsageObservation<f64>,
    usage_observed: bool,
    complete_usage_sha256: Option<&'a str>,
    projection: &'static str,
}
#[derive(Serialize)]
struct BatchAttempt<'a> {
    batch_attempt_id: String,
    batch_attempt_identity_scope: &'static str,
    #[serde(flatten)]
    attempt: AttemptFields<'a>,
    usage: BatchUsage<'a>,
}
#[derive(Serialize)]
struct AttemptFields<'a> {
    stage: &'a str,
    batch_index: usize,
    candidate_ids: &'a [String],
    requested_model: &'a str,
    prepared_request_sha256: &'a str,
    encoded_request_bytes: usize,
    question_count: usize,
    response_status: &'a str,
    returned_model: Option<&'a str>,
    provider_response_id: Option<&'a str>,
    provider: Option<&'a str>,
    failure: Option<&'a gptgrep_jev::DecisionFailureDiagnostic>,
    actual_provider_requests: Option<u64>,
}
fn attempt_id(operation: &str, a: &RichMultimodalAttempt) -> String {
    digest(&(operation, a.stage, a.batch_index))
}
fn cost(a: &RichMultimodalAttempt) -> Option<f64> {
    a.full_usage()
        .get("cost")
        .and_then(Value::as_f64)
        .or_else(|| a.failure.as_ref()?.reported_usage.as_ref()?.cost.value)
        .filter(|v| v.is_finite() && *v >= 0.0)
}
fn attempts<'a>(operation: &str, report: &'a RichMultimodalSearchReport) -> Vec<BatchAttempt<'a>> {
    report.attempts.iter().map(|a| BatchAttempt {
        batch_attempt_id: attempt_id(operation, a),
        batch_attempt_identity_scope: "batch_response_local_not_sdk_native_or_cross_batch",
        attempt: AttemptFields { stage:a.stage, batch_index:a.batch_index,
            candidate_ids:&a.candidate_ids, requested_model:&a.requested_model,
            prepared_request_sha256:&a.prepared_request_sha256, encoded_request_bytes:a.encoded_request_bytes,
            question_count:a.question_count, response_status:a.response_status,
            returned_model:a.returned_model.as_deref(), provider_response_id:a.provider_response_id.as_deref(),
            provider:a.provider.as_deref(), failure:a.failure.as_ref(), actual_provider_requests:None },
        usage: BatchUsage {
            input_tokens: observed(a.full_usage(), "input_tokens", Value::as_u64),
            output_tokens: observed(a.full_usage(), "output_tokens", Value::as_u64),
            cost: observed(a.full_usage(), "cost", |v| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)),
            usage_observed:a.usage.usage_observed,
            complete_usage_sha256:a.usage.complete_usage_sha256.as_deref(),
            projection:"bounded_numeric_fields; cost is a lossy finite_f64 projection; other usage metadata omitted and SHA retained",
        },
    }).collect()
}
#[derive(Serialize)]
struct BatchReport<'a> {
    schema_version: &'a str,
    generation: &'a GenerationPin,
    query: &'a str,
    query_options_sha256: &'a str,
    run_id: &'a str,
    plan_digest: &'a str,
    budget_scope: &'a str,
    original_clock_anchor_sha256: Option<&'a str>,
    hits: &'a [crate::RichMultimodalHit],
    coverage: &'a crate::RichMultimodalCoverage,
    attempts: Vec<BatchAttempt<'a>>,
    logical_calls_attempted: usize,
    validated_responses: usize,
    accounting_complete: bool,
    read_intervals: &'a [ReaderReadMetrics],
    read_meter_scope: &'a str,
    elapsed_ms: u128,
    citation_status: &'a str,
}
fn projection<'a>(op: &str, r: &'a RichMultimodalSearchReport) -> BatchReport<'a> {
    BatchReport {
        schema_version: r.schema_version,
        generation: &r.generation,
        query: &r.query,
        query_options_sha256: &r.query_options_sha256,
        run_id: &r.run_id,
        plan_digest: &r.plan_digest,
        budget_scope: r.budget_scope,
        original_clock_anchor_sha256: r.original_clock_anchor_sha256.as_deref(),
        hits: &r.hits,
        coverage: &r.coverage,
        attempts: attempts(op, r),
        logical_calls_attempted: r.logical_calls_attempted,
        validated_responses: r.validated_responses,
        accounting_complete: r.accounting_complete,
        read_intervals: &r.read_intervals,
        read_meter_scope: r.read_meter_scope,
        elapsed_ms: r.elapsed_ms,
        citation_status: r.citation_status,
    }
}

#[derive(Default)]
struct Totals {
    seen: BTreeSet<String>,
    metrics: RichBatchDecisionsMetrics,
}
impl Totals {
    fn observe(
        &mut self,
        operation: &str,
        reserved: usize,
        report: &RichMultimodalSearchReport,
    ) -> RichBatchItemDecisions {
        let mut item = RichBatchItemDecisions {
            reserved_calls: reserved,
            scheduled_logical_attempts: report.attempts.len(),
            validated_responses: report.validated_responses,
            ..Default::default()
        };
        for a in &report.attempts {
            if let Some(c) = cost(a) {
                item.observed_cost_attempts += 1;
                item.observed_cost_subtotal_usd =
                    Some(item.observed_cost_subtotal_usd.unwrap_or(0.0) + c);
            }
            if self.seen.insert(attempt_id(operation, a)) {
                self.metrics.unique_scheduled_logical_attempts += 1;
                if a.response_status == "validated" {
                    self.metrics.validated_responses += 1;
                }
                if let Some(c) = cost(a) {
                    self.metrics.observed_cost_attempts += 1;
                    if !self.metrics.cost_sum_overflowed {
                        let sum = self.metrics.observed_cost_subtotal_usd.unwrap_or(0.0) + c;
                        if sum.is_finite() {
                            self.metrics.observed_cost_subtotal_usd = Some(sum);
                        } else {
                            self.metrics.cost_sum_overflowed = true;
                            self.metrics.observed_cost_subtotal_usd = None;
                        }
                    }
                }
            }
        }
        item.cost_complete = item.scheduled_logical_attempts > 0
            && item.observed_cost_attempts == item.scheduled_logical_attempts;
        self.metrics.cost_complete = !self.metrics.cost_sum_overflowed
            && !self.seen.is_empty()
            && self.metrics.observed_cost_attempts == self.seen.len();
        if item
            .observed_cost_subtotal_usd
            .is_some_and(|n| !n.is_finite())
        {
            item.observed_cost_subtotal_usd = None;
            item.cost_complete = false;
        }
        if self
            .metrics
            .observed_cost_subtotal_usd
            .is_some_and(|n| !n.is_finite())
        {
            self.metrics.observed_cost_subtotal_usd = None;
            self.metrics.cost_complete = false;
        }
        item
    }
}

impl PreparedRichBatch {
    fn execution(&self, _totals: &Totals) -> RichBatchExecution {
        RichBatchExecution {
            scope: self
                .context_before
                .as_ref()
                .map_or("local_deterministic_no_model_context", |s| s.scope.as_str())
                .into(),
            // Shared snapshots are not exact batch attribution.
            model_submissions: None,
            context_before: self.context_before.clone(),
            context_after: self
                .client
                .as_ref()
                .and_then(|c| c.execution_context().snapshot().ok()),
            context_observation_scope: self
                .client
                .as_ref()
                .map(|_| "shared_context_cumulative_not_batch_attribution".into()),
        }
    }
    fn failed(
        &self,
        e: RichBatchError,
        items: Vec<RichBatchItem>,
        totals: &Totals,
    ) -> Box<RichBatchFailure> {
        let mut f = failure(e, Some(self.reader.pin()), items);
        f.execution = self.execution(totals);
        f.decisions = Some(totals.metrics.clone());
        f.read_metrics = self
            .reader
            .read_metrics()
            .ok()
            .and_then(|m| meter_delta(&m, &self.before).ok());
        f
    }
    pub async fn execute(mut self) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
        let pin = self.reader.pin();
        let mut items: Vec<_> = self
            .request
            .operations
            .iter()
            .zip(&self.quotas)
            .map(|(op, q)| RichBatchItem::pending(op.id(), *q))
            .collect();
        for (i, item) in items.iter_mut().enumerate() {
            if matches!(
                self.request.operations[i],
                RichBatchOperation::SearchMultimodal { .. }
            ) {
                item.decisions = Some(RichBatchItemDecisions {
                    reserved_calls: self.call_shares[i],
                    ..Default::default()
                });
            }
        }
        let mut totals = Totals::default();
        totals.metrics.attempt_identity_scope =
            "batch_response_local_not_sdk_native_or_cross_batch".into();
        totals.metrics.reserved_calls = self.reserved_calls();
        let mut stopped_paid = false;
        for i in 0..items.len() {
            if let Err(e) = self.reader.validate() {
                return Err(self.failed(reader_error(e), items, &totals));
            }
            let op = &self.request.operations[i];
            let paid = matches!(op, RichBatchOperation::SearchMultimodal { .. });
            if self
                .client
                .as_ref()
                .is_some_and(|c| c.execution_context().check_deadline().is_err())
            {
                for item in &mut items[i..] {
                    item.error = Some(error(
                        RichBatchErrorCode::DeadlineExceeded,
                        "Inherited execution deadline expired before this operation",
                    ));
                }
                break;
            }
            if paid && stopped_paid {
                continue;
            }
            let before = self
                .reader
                .read_metrics()
                .map_err(|e| self.failed(reader_error(e), items.clone(), &totals))?;
            let mut item = match &self.starts[i] {
                Err(e) => RichBatchItem::failed(op.id(), self.quotas[i], e.clone()),
                Ok(start) if !paid => {
                    match execute(&self.reader, op, *start, self.quotas[i], &before) {
                        Ok(item) => item,
                        Err(e) => RichBatchItem::failed(op.id(), self.quotas[i], e),
                    }
                }
                Ok(_) => {
                    let search = self.searches[i]
                        .take()
                        .expect("admitted paid operation owns preparation");
                    let (report, failed) = match search.execute().await {
                        Ok(pass) => (pass.into_report(), None),
                        Err(e) => {
                            stopped_paid = true;
                            let code = if e.report.attempts.iter().any(|a| a.failure.is_some()) {
                                RichBatchErrorCode::ProviderFailure
                            } else {
                                RichBatchErrorCode::PaidSearchFailure
                            };
                            (
                                e.report,
                                Some(error(
                                    code,
                                    "Required paid search failed; observations retained without fallback",
                                )),
                            )
                        }
                    };
                    let accounting = totals.observe(op.id(), self.call_shares[i], &report);
                    let coverage = RichBatchItemCoverage {
                        unit: "model_ranked_evidence".into(),
                        available: Some(report.hits.len()),
                        end: report.hits.len(),
                        selected: report.hits.len(),
                        exhausted: report.coverage.semantic_scope_complete,
                        ..Default::default()
                    };
                    // The complete projected result and enclosing accounting were reserved before submission.
                    let result = projection(op.id(), &report);
                    // Serialize the already bounded projection without requiring a
                    // successful post-paid reader-meter observation. Later shared
                    // integrity failures must retain this actual attempt receipt.
                    let value =
                        serde_json::to_value(&result).expect("bounded owned report projection");
                    RichBatchItem {
                        id: op.id().into(),
                        status: if failed.is_some() {
                            RichBatchStatus::Error
                        } else {
                            RichBatchStatus::Ok
                        },
                        reserved_output_bytes: self.quotas[i],
                        result: Some(value),
                        error: failed,
                        coverage,
                        read_metrics: None,
                        decisions: Some(accounting),
                    }
                }
            };
            item.read_metrics = item_meter(&self.reader, &before).ok();
            let shared = item
                .error
                .as_ref()
                .is_some_and(|e| e.code == RichBatchErrorCode::SharedIntegrity);
            if paid && item.decisions.is_none() {
                item.decisions = Some(RichBatchItemDecisions {
                    reserved_calls: self.call_shares[i],
                    ..Default::default()
                });
            }
            items[i] = item;
            if measure(&items[i], self.quotas[i]).is_err() {
                return Err(self.failed(
                    error(
                        RichBatchErrorCode::OutputBudgetInsufficient,
                        "Reserved complete item invariant failed",
                    ),
                    items,
                    &totals,
                ));
            }
            if shared {
                return Err(self.failed(
                    error(
                        RichBatchErrorCode::SharedIntegrity,
                        "Shared generation integrity failed",
                    ),
                    items,
                    &totals,
                ));
            }
            if let Err(e) = self.reader.validate() {
                return Err(self.failed(reader_error(e), items, &totals));
            }
        }
        if let Err(e) = self.reader.validate() {
            return Err(self.failed(reader_error(e), items, &totals));
        }
        let coverage = RichBatchCoverage {
            requested: items.len(),
            succeeded: items
                .iter()
                .filter(|i| i.status == RichBatchStatus::Ok)
                .count(),
            failed: items
                .iter()
                .filter(|i| i.status == RichBatchStatus::Error)
                .count(),
            not_executed: items
                .iter()
                .filter(|i| i.status == RichBatchStatus::NotExecuted)
                .count(),
        };
        let read_metrics = self
            .reader
            .read_metrics()
            .map_err(|e| self.failed(reader_error(e), items.clone(), &totals))?;
        let execution = self.execution(&totals);
        let delta = meter_delta(&read_metrics, &self.before)
            .map_err(|e| self.failed(e, items.clone(), &totals))?;
        let response = RichBatchResponse {
            schema_version: RICH_BATCH_RESPONSE_SCHEMA.into(),
            generation: pin,
            execution,
            items,
            coverage,
            metrics: RichBatchMetrics {
                elapsed_ms: self.started.elapsed().as_millis(),
                shared_reader: true,
                physical_bytes: None,
                model_submissions: None,
                read_metrics: Some(delta),
                cache_status: self.reader.cache_status().ok(),
                decisions: Some(totals.metrics.clone()),
            },
        };
        if measure(&response, self.request.budget.max_output_bytes).is_err() {
            return Err(self.failed(
                error(
                    RichBatchErrorCode::OutputBudgetInsufficient,
                    "Reserved complete batch invariant failed",
                ),
                response.items,
                &totals,
            ));
        }
        Ok(response)
    }
}

pub async fn dispatch_rich_batch_with_reader_and_client(
    reader: &RichGenerationReader,
    request: RichBatchRequest,
    client: Option<JevClient>,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    if !request.requires_model() {
        return dispatch_rich_batch_with_reader(reader, request);
    }
    prepare_rich_batch_with_reader(reader, request, client)?
        .execute()
        .await
}
pub async fn dispatch_rich_batch_with_client(
    root: &Path,
    request: RichBatchRequest,
    client: Option<JevClient>,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    if !request.requires_model() {
        return dispatch_rich_batch(root, request);
    }
    let reader = open_rich_batch_reader(root, &request)?;
    dispatch_rich_batch_with_reader_and_client(&reader, request, client).await
}

#[cfg(test)]
#[path = "paid_tests.rs"]
mod tests;
