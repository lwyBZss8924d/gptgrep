//! Source-only native/OCR Decisions bootstrap. Model exposure grants no citation.
use crate::{
    ArtifactTextWindow, EvidenceRef, GenerationPin, ReaderReadMetrics, RichGenerationReader,
};
use anyhow::{Context, Result, ensure};
use gptgrep_jev::{Candidate, JevClient, PreparedRerank, RerankResponse};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Write, sync::Arc, time::Instant};

const MAX_OUTPUT: usize = 1_048_576;
const MAX_METADATA: usize = 524_288;
const MAX_BUFFERS: usize = 8_388_608;

#[derive(Clone)]
pub struct RichTextSearchOptions {
    pub document: Option<String>,
    pub window_bytes: usize,
    pub max_output_bytes: usize,
    pub max_metadata_bytes: usize,
    pub min_score: f64,
    /// Require the caller's already-attached original shared ledger context.
    pub require_shared_ledger: bool,
}
impl Default for RichTextSearchOptions {
    fn default() -> Self {
        Self {
            document: None,
            window_bytes: 1024,
            max_output_bytes: MAX_OUTPUT,
            max_metadata_bytes: MAX_METADATA,
            min_score: 0.5,
            require_shared_ledger: false,
        }
    }
}
#[derive(Clone, Serialize)]
pub struct RichTextUsageProjection {
    pub schema_version: &'static str,
    pub input_tokens: Option<Value>,
    pub output_tokens: Option<Value>,
    pub cost: Option<Value>,
    pub usage_observed: bool,
    pub complete_usage_sha256: Option<String>,
    pub projection: bool,
}
#[derive(Clone, Serialize)]
pub struct RichTextAttempt {
    pub stage: &'static str,
    pub requested_model: String,
    pub prepared_request_sha256: String,
    pub encoded_request_bytes: usize,
    pub question_count: usize,
    pub response_status: &'static str,
    pub returned_model: Option<String>,
    pub provider_response_id: Option<String>,
    pub provider: Option<String>,
    pub usage: RichTextUsageProjection,
    /// Null: logical calls do not establish physical provider request counts.
    pub actual_provider_requests: Option<u64>,
    #[serde(skip)]
    full_usage: Arc<Value>,
}
impl RichTextAttempt {
    /// Complete observed provider usage, borrowed from one retained allocation.
    pub fn full_usage(&self) -> &Value {
        &self.full_usage
    }
    fn pending(stage: &'static str, request: &PreparedRerank) -> Self {
        Self {
            stage,
            requested_model: request.requested_model().into(),
            prepared_request_sha256: request.body_sha256().into(),
            encoded_request_bytes: request.body_bytes(),
            question_count: request.question_count(),
            response_status: "failed_or_unreported",
            returned_model: None,
            provider_response_id: None,
            provider: None,
            usage: RichTextUsageProjection {
                schema_version: "gptgrep.usage_projection.v1",
                input_tokens: None,
                output_tokens: None,
                cost: None,
                usage_observed: false,
                complete_usage_sha256: None,
                projection: true,
            },
            actual_provider_requests: None,
            full_usage: Arc::new(Value::Null),
        }
    }
    fn observe(&mut self, response: &mut RerankResponse) -> Result<()> {
        let usage = std::mem::take(&mut response.usage);
        self.usage = RichTextUsageProjection {
            schema_version: "gptgrep.usage_projection.v1",
            input_tokens: usage.get("input_tokens").filter(|v| !v.is_null()).cloned(),
            output_tokens: usage.get("output_tokens").filter(|v| !v.is_null()).cloned(),
            cost: usage.get("cost").filter(|v| !v.is_null()).cloned(),
            usage_observed: !usage.is_null(),
            complete_usage_sha256: (!usage.is_null()).then(|| hash_json(&usage)).transpose()?,
            projection: true,
        };
        self.full_usage = Arc::new(usage);
        self.returned_model = Some(response.model.clone());
        self.provider_response_id = response.provider_response_id.clone();
        self.provider = response.provider.clone();
        self.response_status = "validated";
        Ok(())
    }
}
#[derive(Clone, Serialize)]
pub struct RichTextCoverage {
    pub scoped_documents: usize,
    pub routed_documents: usize,
    pub selected_documents: usize,
    pub available_artifacts: usize,
    pub inspected_artifacts: usize,
    pub omitted_artifacts: usize,
    pub scanned_text_bytes: u64,
    pub potential_windows: usize,
    pub union_windows: usize,
    pub filtered_windows: usize,
    pub lexical_truncated: bool,
    pub lexical_query_terms_truncated: bool,
    /// Routing samples and structural windows are bounded, even if all docs fit.
    pub semantic_scope_complete: bool,
    pub delivered_set_sufficiency: &'static str,
}
#[derive(Clone, Serialize)]
pub struct RichTextHit {
    pub candidate_id: String,
    pub window: ArtifactTextWindow,
    pub score: f64,
    pub confidence: Option<f64>,
    pub literal_anchor: bool,
}
#[derive(Serialize)]
pub struct RichTextSearchReport {
    pub schema_version: &'static str,
    pub generation: GenerationPin,
    pub query: String,
    pub query_options_sha256: String,
    pub run_id: String,
    pub plan_digest: String,
    pub budget_scope: &'static str,
    pub original_clock_anchor_sha256: Option<String>,
    pub hits: Vec<RichTextHit>,
    pub coverage: RichTextCoverage,
    pub attempts: Vec<RichTextAttempt>,
    pub logical_calls_attempted: usize,
    pub validated_responses: usize,
    /// All nonzero attempted logical calls returned validated replies. This does
    /// not imply search success, available usage, physical requests or billing.
    pub accounting_complete: bool,
    pub read_intervals: Vec<ReaderReadMetrics>,
    pub read_meter_scope: &'static str,
    pub elapsed_ms: u128,
    pub citation_status: &'static str,
}
#[derive(Serialize)]
pub struct RichTextSearchError {
    pub stage: &'static str,
    pub code: &'static str,
    pub report: RichTextSearchReport,
}
impl std::fmt::Display for RichTextSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} during {} (accounting incomplete)",
            self.code, self.stage
        )
    }
}
impl std::fmt::Debug for RichTextSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for RichTextSearchError {}

struct PoolWindow {
    window: ArtifactTextWindow,
    candidate: Candidate,
    document_index: usize,
    structural: bool,
    lexical: bool,
    literal_anchor: bool,
}
/// Immutable source map and exact preparing client; never reconstructed from JSON.
pub struct PreparedRichTextSearch {
    reader: RichGenerationReader,
    client: JevClient,
    options: RichTextSearchOptions,
    pool: Vec<PoolWindow>,
    routing: Vec<(String, usize)>,
    route_request: Option<PreparedRerank>,
    full_pool_request: PreparedRerank,
    output_reserved: usize,
    report: RichTextSearchReport,
    started: Instant,
    last_meter: ReaderReadMetrics,
}
/// Genuine mandatory initial-pass result. No Clone/serde constructor or citations.
///
/// ```compile_fail
/// let _: gptgrep_core::RichTextInitialPass = serde_json::from_str("{}").unwrap();
/// ```
pub struct RichTextInitialPass {
    reader: RichGenerationReader,
    client: JevClient,
    pool: Vec<PoolWindow>,
    report: RichTextSearchReport,
}
impl RichTextInitialPass {
    pub fn report(&self) -> &RichTextSearchReport {
        &self.report
    }
    pub fn reader(&self) -> &RichGenerationReader {
        &self.reader
    }
    pub fn client(&self) -> &JevClient {
        &self.client
    }
    pub fn validate(&self) -> Result<()> {
        validate_pool(&self.reader, &self.client, &self.pool)
    }
    /// Presentation DTO consumes the handle and cannot recreate its authority.
    pub fn into_report(self) -> RichTextSearchReport {
        self.report
    }
}

struct JsonCounter {
    bytes: usize,
    limit: usize,
}
impl Write for JsonCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("rich_decisions_output_budget_insufficient"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn measure(value: &impl Serialize, limit: usize) -> Result<usize> {
    let mut counter = JsonCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn hash_json(value: &impl Serialize) -> Result<String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", writer.0.finalize()))
}
pub(crate) fn binding(window: &ArtifactTextWindow) -> &crate::TextEvidenceBinding {
    match &window.evidence {
        EvidenceRef::NativeText { binding } | EvidenceRef::OcrText { binding, .. } => binding,
        EvidenceRef::VisualRegion { .. } => unreachable!("reader text window is native/OCR only"),
    }
}
fn validate_pool(
    reader: &RichGenerationReader,
    client: &JevClient,
    pool: &[PoolWindow],
) -> Result<()> {
    client.execution_context().check_deadline()?;
    reader.validate()?;
    for candidate in pool {
        reader.validate_text_evidence(&candidate.window.evidence)?;
        client.execution_context().check_deadline()?;
    }
    reader.validate()?;
    client.execution_context().check_deadline()
}

/// Prepares all actual text/request/output bounds before the first paid admission.
/// The caller supplies its bound context/client; this function reads no credentials.
pub fn prepare_rich_text_search(
    reader: RichGenerationReader,
    client: JevClient,
    query: &str,
    options: &RichTextSearchOptions,
) -> Result<PreparedRichTextSearch> {
    let started = Instant::now();
    ensure!(
        !query.trim().is_empty() && query.len() <= 8192,
        "rich_decisions_invalid_query"
    );
    ensure!(
        (4..=2048).contains(&options.window_bytes)
            && (1..=MAX_OUTPUT).contains(&options.max_output_bytes)
            && (1..=MAX_METADATA).contains(&options.max_metadata_bytes)
            && options.min_score.is_finite()
            && (0.0..=1.0).contains(&options.min_score)
            && options
                .document
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 8192),
        "rich_decisions_invalid_options"
    );
    let context = client.execution_context();
    context.check_deadline()?;
    let run_id = context
        .run_id()
        .context("rich_decisions_explicit_execution_context_required")?;
    let plan_digest = context
        .plan_digest()
        .context("rich_decisions_explicit_execution_context_required")?;
    let clock = context.ledger_clock_anchor()?;
    ensure!(
        !options.require_shared_ledger || clock.is_some(),
        "rich_decisions_shared_ledger_required"
    );
    let before = reader.read_metrics()?;
    let locations = reader.text_window_locations(
        query,
        options.document.as_deref(),
        options.window_bytes,
        &|| context.check_deadline(),
    )?;
    let mut metadata = 0usize;
    for location in &locations.windows {
        metadata = metadata
            .checked_add(reader.text_location_metadata_bound(location)?)
            .context("rich_decisions_metadata_overflow")?;
        ensure!(
            metadata <= options.max_metadata_bytes,
            "rich_decisions_metadata_budget_insufficient"
        );
    }
    // Source-derived routing labels and coverage are charged before their copies.
    let manifest = reader.manifest()?;
    for &index in &locations.routing_documents {
        let doc = &manifest.documents[index].document;
        metadata = metadata
            .checked_add(4096 + doc.id.len() * 6 + doc.path.len() * 6)
            .context("rich_decisions_metadata_overflow")?;
        ensure!(
            metadata <= options.max_metadata_bytes,
            "rich_decisions_metadata_budget_insufficient"
        );
    }
    let owned_bound = metadata
        .checked_mul(3)
        .and_then(|n| n.checked_add(24 * 2048 * 6 + 4 * 65536 + MAX_OUTPUT + 32768))
        .context("rich_decisions_working_overflow")?;
    ensure!(
        owned_bound <= MAX_BUFFERS,
        "rich_decisions_working_budget_insufficient"
    );
    let mut pool: Vec<PoolWindow> = Vec::new();
    for location in &locations.windows {
        let artifact = &manifest.documents[location.document_index].artifacts
            [location.artifact_index]
            .artifact;
        let window = reader.read_text(&artifact.id, location.offset, options.window_bytes)?;
        if window.text.trim().is_empty() {
            continue;
        }
        reader.validate_text_evidence(&window.evidence)?;
        let id = hash_json(&window.evidence)?;
        if let Some(previous) = pool
            .iter_mut()
            .find(|candidate| candidate.candidate.id == id)
        {
            previous.lexical |= location.lexical;
            previous.structural |= !location.lexical;
            previous.literal_anchor |=
                location.lexical && crate::literal_anchor(query, &window.text);
            continue;
        }
        let literal_anchor = location.lexical && crate::literal_anchor(query, &window.text);
        let candidate = Candidate {
            id,
            text: window.text.clone(),
        };
        pool.push(PoolWindow {
            window,
            candidate,
            document_index: location.document_index,
            structural: !location.lexical,
            lexical: location.lexical,
            literal_anchor,
        });
        context.check_deadline()?;
    }
    pool.sort_by(|left, right| {
        let a = binding(&left.window);
        let b = binding(&right.window);
        (
            &a.source_path,
            a.page,
            &a.artifact_id,
            a.byte_start,
            a.byte_end,
        )
            .cmp(&(
                &b.source_path,
                b.page,
                &b.artifact_id,
                b.byte_start,
                b.byte_end,
            ))
    });
    ensure!(
        !pool.is_empty() && pool.len() <= 24,
        "rich_decisions_no_text_candidates"
    );
    let mut routing = Vec::new();
    let mut route_candidates = Vec::new();
    for &index in &locations.routing_documents {
        let doc = &manifest.documents[index].document;
        let texts: Vec<_> = pool
            .iter()
            .filter(|p| p.document_index == index && p.structural)
            .map(|p| p.window.text.as_str())
            .collect();
        if texts.is_empty() {
            continue;
        }
        let id = hash_json(&(reader.pin(), &doc.id, &doc.source_sha256))?;
        let text = format!(
            "Source path: {}\nTitle: {}\nSource windows:\n{}",
            crate::truncate(&doc.path, 512).0,
            crate::truncate(&doc.title, 512).0,
            texts.join("\n--- source window ---\n")
        );
        route_candidates.push(Candidate {
            id: id.clone(),
            text,
        });
        routing.push((id, index));
    }
    ensure!(
        !routing.is_empty() && routing.len() <= 8,
        "rich_decisions_no_routing_text"
    );
    let full_candidates: Vec<_> = pool.iter().map(|p| p.candidate.clone()).collect();
    let full_request = client.prepare_rerank(query, &full_candidates)?;
    let route_request = client.prepare_rerank(query, &route_candidates)?;
    // Fixed overhead covers query/options hash, run binding, scalar rank fields,
    // up to three meter intervals and two complete bounded attempt projections.
    // Metadata bound covers every possible window and all route/coverage labels.
    let output_reserved = 32768usize
        .checked_add(2 * 8192)
        .and_then(|n| n.checked_add(query.len() * 6))
        .and_then(|n| n.checked_add(metadata))
        .and_then(|n| n.checked_add(pool.len() * (options.window_bytes * 6 + 512)))
        .context("rich_decisions_output_reservation_overflow")?;
    ensure!(
        output_reserved <= options.max_output_bytes,
        "rich_decisions_output_budget_insufficient"
    );
    let after = reader.read_metrics()?;
    let report = RichTextSearchReport {
        schema_version: "gptgrep.rich_text_search.v1",
        generation: reader.pin(),
        query: query.into(),
        query_options_sha256: hash_json(&(
            "gptgrep.rich_text_search.v1",
            query,
            &options.document,
            options.max_output_bytes,
            options.window_bytes,
            options.min_score,
            options.require_shared_ledger,
            options.max_metadata_bytes,
        ))?,
        run_id: run_id.into(),
        plan_digest: plan_digest.into(),
        budget_scope: if clock.is_some() {
            "shared_ledger"
        } else {
            "invocation"
        },
        original_clock_anchor_sha256: clock.as_ref().map(hash_json).transpose()?,
        hits: Vec::new(),
        coverage: RichTextCoverage {
            scoped_documents: locations.scoped_documents,
            routed_documents: routing.len(),
            selected_documents: 0,
            available_artifacts: locations.available_artifacts,
            inspected_artifacts: locations.inspected_artifacts,
            omitted_artifacts: locations
                .available_artifacts
                .saturating_sub(locations.inspected_artifacts),
            scanned_text_bytes: locations.scanned_bytes,
            potential_windows: pool.len(),
            union_windows: 0,
            filtered_windows: 0,
            lexical_truncated: locations.lexical_truncated,
            lexical_query_terms_truncated: locations.lexical_terms_truncated,
            semantic_scope_complete: false,
            delivered_set_sufficiency: "unassessed",
        },
        attempts: Vec::with_capacity(2),
        logical_calls_attempted: 0,
        validated_responses: 0,
        accounting_complete: false,
        read_intervals: vec![after.since(&before)?],
        read_meter_scope: "shared_reader_intervals_not_exclusive_operation",
        elapsed_ms: 0,
        citation_status: "model_exposure_only_delivery_unqualified",
    };
    validate_pool(&reader, &client, &pool)?;
    context.check_deadline()?;
    Ok(PreparedRichTextSearch {
        reader,
        client,
        options: options.clone(),
        pool,
        routing,
        route_request: Some(route_request),
        full_pool_request: full_request,
        output_reserved,
        report,
        started,
        last_meter: after,
    })
}
impl PreparedRichTextSearch {
    pub fn generation(&self) -> &GenerationPin {
        &self.report.generation
    }
    pub fn reserved_output_bytes(&self) -> usize {
        self.output_reserved
    }
    pub fn full_pool_request_bytes(&self) -> usize {
        self.full_pool_request.body_bytes()
    }
    /// Exact first-stage request; inspection neither charges nor submits it.
    /// Contains source text, so callers must not publish the body as telemetry.
    pub fn routing_request(&self) -> &PreparedRerank {
        self.route_request
            .as_ref()
            .expect("one prepared routing request")
    }
    /// Complete source-derived rerank pool in stable candidate_N order.
    /// Execution selects an ordered subset after routing; this is a conservative
    /// request bound, not a claim that the entire pool will be sent.
    pub fn rerank_pool_request(&self) -> &PreparedRerank {
        &self.full_pool_request
    }
    fn finalize_logical_accounting(&mut self) {
        self.report.accounting_complete = self.report.logical_calls_attempted != 0
            && self.report.logical_calls_attempted == self.report.validated_responses;
    }
    fn failure(mut self, stage: &'static str, code: &'static str) -> RichTextSearchError {
        self.finalize_logical_accounting();
        if self.report.read_intervals.len() < 3
            && let Ok(after) = self.reader.read_metrics()
            && let Ok(delta) = after.since(&self.last_meter)
        {
            self.report.read_intervals.push(delta);
        }
        self.report.elapsed_ms = self.started.elapsed().as_millis();
        RichTextSearchError {
            stage,
            code,
            report: self.report,
        }
    }
    /// Each exact request uses the same context's durable admission. A second
    /// stage can exhaust global quota; no two-credit atomic guarantee is made.
    pub async fn execute(self) -> std::result::Result<RichTextInitialPass, RichTextSearchError> {
        self.execute_with_observer(&|_| Ok(())).await
    }

    /// An explicit private sink can retain complete observed usage before the
    /// next await. A sink failure stops the operation without another submit.
    pub async fn execute_with_observer(
        mut self,
        observer: &(dyn Fn(&RichTextAttempt) -> Result<()> + Send + Sync),
    ) -> std::result::Result<RichTextInitialPass, RichTextSearchError> {
        if validate_pool(&self.reader, &self.client, &self.pool).is_err() {
            return Err(self.failure(
                "document_routing",
                "rich_decisions_source_or_deadline_invalid",
            ));
        }
        self.report.attempts.push(RichTextAttempt::pending(
            "document_routing",
            self.route_request
                .as_ref()
                .expect("one prepared routing request"),
        ));
        self.report.logical_calls_attempted += 1;
        let request = self
            .route_request
            .take()
            .expect("one prepared routing request");
        // The private field is moved once; a replacement is never sent.
        let mut route = match self.client.submit_rerank(request).await {
            Ok(response) => response,
            Err(_) => {
                self.report.elapsed_ms = self.started.elapsed().as_millis();
                return Err(
                    self.failure("document_routing", "rich_decisions_required_stage_failed")
                );
            }
        };
        if self.report.attempts[0].observe(&mut route).is_err() {
            return Err(self.failure("document_routing", "rich_decisions_accounting_failed"));
        }
        self.report.validated_responses += 1;
        self.report.elapsed_ms = self.started.elapsed().as_millis();
        if observer(self.report.attempts.last().expect("observed attempt")).is_err() {
            let stage = self.report.attempts.last().expect("observed attempt").stage;
            return Err(self.failure(stage, "rich_decisions_observer_failed"));
        }
        if validate_pool(&self.reader, &self.client, &self.pool).is_err() {
            return Err(self.failure(
                "document_routing",
                "rich_decisions_source_or_deadline_invalid",
            ));
        }
        // Subsequent handling no longer needs the moved routing request.
        let selected: Vec<_> = route
            .rankings
            .iter()
            .take(4)
            .filter_map(|rank| {
                self.routing
                    .iter()
                    .find(|(id, _)| id == &rank.id)
                    .map(|(_, index)| *index)
            })
            .collect();
        self.report.coverage.selected_documents = selected.len();
        let positions: Vec<_> = self
            .pool
            .iter()
            .enumerate()
            .filter_map(|(index, p)| {
                (p.lexical || (p.structural && selected.contains(&p.document_index)))
                    .then_some(index)
            })
            .collect();
        let candidates: Vec<_> = positions
            .iter()
            .map(|index| self.pool[*index].candidate.clone())
            .collect();
        self.report.coverage.union_windows = candidates.len();
        let prepared = match self.client.prepare_rerank(&self.report.query, &candidates) {
            Ok(request) if request.body_bytes() <= self.full_pool_request.body_bytes() => request,
            _ => {
                return Err(self.failure(
                    "evidence_reranking",
                    "rich_decisions_preparation_invariant_failed",
                ));
            }
        };
        let after = match self.reader.read_metrics() {
            Ok(m) => m,
            Err(_) => return Err(self.failure("evidence_reranking", "rich_decisions_meter_failed")),
        };
        if let Ok(delta) = after.since(&self.last_meter) {
            self.report.read_intervals.push(delta);
        }
        self.last_meter = after;
        if validate_pool(&self.reader, &self.client, &self.pool).is_err() {
            return Err(self.failure(
                "evidence_reranking",
                "rich_decisions_source_or_deadline_invalid",
            ));
        }
        self.report
            .attempts
            .push(RichTextAttempt::pending("evidence_reranking", &prepared));
        self.report.logical_calls_attempted += 1;
        let mut reranked = match self.client.submit_rerank(prepared).await {
            Ok(response) => response,
            Err(_) => {
                return Err(
                    self.failure("evidence_reranking", "rich_decisions_required_stage_failed")
                );
            }
        };
        if self.report.attempts[1].observe(&mut reranked).is_err() {
            return Err(self.failure("evidence_reranking", "rich_decisions_accounting_failed"));
        }
        self.report.validated_responses += 1;
        self.report.elapsed_ms = self.started.elapsed().as_millis();
        if observer(self.report.attempts.last().expect("observed attempt")).is_err() {
            let stage = self.report.attempts.last().expect("observed attempt").stage;
            return Err(self.failure(stage, "rich_decisions_observer_failed"));
        }
        if validate_pool(&self.reader, &self.client, &self.pool).is_err() {
            return Err(self.failure(
                "evidence_reranking",
                "rich_decisions_source_or_deadline_invalid",
            ));
        }
        for rank in reranked.rankings {
            let Some(candidate) = self.pool.iter().find(|p| {
                p.candidate.id == rank.id
                    && positions
                        .iter()
                        .any(|index| self.pool[*index].candidate.id == rank.id)
            }) else {
                return Err(self.failure(
                    "evidence_reranking",
                    "rich_decisions_identity_invariant_failed",
                ));
            };
            if !candidate.literal_anchor && rank.score < self.options.min_score {
                self.report.coverage.filtered_windows += 1;
                continue;
            }
            self.report.hits.push(RichTextHit {
                candidate_id: candidate.candidate.id.clone(),
                window: candidate.window.clone(),
                score: rank.score,
                confidence: rank.confidence,
                literal_anchor: candidate.literal_anchor,
            });
        }
        self.report.hits.sort_by(|a, b| {
            let x = binding(&a.window);
            let y = binding(&b.window);
            b.literal_anchor
                .cmp(&a.literal_anchor)
                .then(b.score.total_cmp(&a.score))
                .then(
                    (
                        &x.source_path,
                        x.page,
                        &x.artifact_id,
                        x.byte_start,
                        x.byte_end,
                    )
                        .cmp(&(
                            &y.source_path,
                            y.page,
                            &y.artifact_id,
                            y.byte_start,
                            y.byte_end,
                        )),
                )
        });
        let after = match self.reader.read_metrics() {
            Ok(m) => m,
            Err(_) => return Err(self.failure("completion", "rich_decisions_meter_failed")),
        };
        if let Ok(delta) = after.since(&self.last_meter) {
            self.report.read_intervals.push(delta);
        }
        self.report.elapsed_ms = self.started.elapsed().as_millis();
        self.finalize_logical_accounting();
        if measure(&self.report, self.output_reserved).is_err()
            || self.client.execution_context().check_deadline().is_err()
        {
            return Err(self.failure("completion", "rich_decisions_completion_invariant_failed"));
        }
        Ok(RichTextInitialPass {
            reader: self.reader,
            client: self.client,
            pool: self.pool,
            report: self.report,
        })
    }
}

#[cfg(test)]
mod tests;
