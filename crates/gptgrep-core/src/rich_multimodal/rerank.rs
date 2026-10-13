//! Explicitly supplied source references. This path never mints an initial
//! routing pass, reader delivery or citation authority.
use super::*;
use crate::{ReaderCacheBudget, rich_batch::RichBatchGeneration};
use gptgrep_jev::{ExecutionSnapshot, UsageObservation, UsagePresence};
use serde::{Deserialize, Serializer, ser::SerializeSeq};
use std::path::Path;

pub const REQUEST_SCHEMA: &str = "gptgrep.rich_rerank.request.v1";
pub const RESPONSE_SCHEMA: &str = "gptgrep.rich_rerank.response.v1";
const MAX_INPUT: usize = 1_048_576;
const MAX_CANDIDATES: usize = 64;
const MAX_TEXT_WINDOW: usize = 2048;
const MAX_VISUAL: usize = 16;
const MAX_WORKING: u64 = 268_435_456;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichRerankCandidate {
    /// Output-side label only. The model receives the source-derived identity.
    pub id: String,
    pub evidence: EvidenceRef,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RichRerankBudget {
    pub max_output_bytes: usize,
    pub max_metadata_bytes: usize,
    pub max_decisions_submissions: u32,
    pub max_concurrent_batches: usize,
    pub cache: ReaderCacheBudget,
}
impl Default for RichRerankBudget {
    fn default() -> Self {
        Self {
            max_output_bytes: MAX_OUTPUT,
            max_metadata_bytes: MAX_METADATA,
            max_decisions_submissions: 8,
            max_concurrent_batches: 4,
            cache: ReaderCacheBudget::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichRerankRequest {
    pub schema_version: String,
    #[serde(default)]
    pub generation: RichBatchGeneration,
    pub query: String,
    pub candidates: Vec<RichRerankCandidate>,
    #[serde(default)]
    pub budget: RichRerankBudget,
}
#[derive(Clone, Serialize)]
pub struct RichRankedCandidate {
    pub id: String,
    pub candidate_id: String,
    pub evidence: EvidenceRef,
    pub score: f64,
    pub confidence: Option<f64>,
}
#[derive(Clone, Serialize)]
pub struct RichRerankCoverage {
    pub scope: &'static str,
    pub supplied_candidates: usize,
    pub text_candidates: usize,
    pub visual_candidates: usize,
    pub scored_candidates: usize,
    pub complete: bool,
}
#[derive(Serialize)]
pub struct RichRerankReport {
    pub schema_version: &'static str,
    pub generation: GenerationPin,
    pub query: String,
    pub query_options_sha256: String,
    pub run_id: String,
    pub plan_digest: String,
    pub budget_scope: &'static str,
    pub original_clock_anchor_sha256: Option<String>,
    pub ranked_candidates: Vec<RichRankedCandidate>,
    pub coverage: RichRerankCoverage,
    #[serde(serialize_with = "serialize_attempts")]
    pub attempts: Vec<RichMultimodalAttempt>,
    pub reserved_calls: usize,
    pub logical_calls_attempted: usize,
    pub validated_responses: usize,
    pub accounting_complete: bool,
    pub context_before: ExecutionSnapshot,
    pub context_after: Option<ExecutionSnapshot>,
    pub context_observation_scope: &'static str,
    pub read_intervals: Vec<ReaderReadMetrics>,
    pub read_meter_scope: &'static str,
    pub elapsed_ms: u128,
    pub citation_status: &'static str,
}
#[derive(Serialize)]
pub struct RichRerankFailure {
    pub schema_version: &'static str,
    pub code: &'static str,
    pub message: &'static str,
    pub generation: Option<GenerationPin>,
    pub report: Option<RichRerankReport>,
}
impl std::fmt::Debug for RichRerankFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::fmt::Display for RichRerankFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for RichRerankFailure {}
fn failure(
    code: &'static str,
    message: &'static str,
    pin: Option<GenerationPin>,
) -> Box<RichRerankFailure> {
    Box::new(RichRerankFailure {
        schema_version: RESPONSE_SCHEMA,
        code,
        message,
        generation: pin,
        report: None,
    })
}
pub fn rich_rerank_input_io_failure() -> Box<RichRerankFailure> {
    failure("input_io_failure", "Rerank input could not be read", None)
}
pub fn rich_rerank_context_failure(pin: GenerationPin) -> Box<RichRerankFailure> {
    failure(
        "model_context_required",
        "Rerank requires the supported model and a finite Decisions context",
        Some(pin),
    )
}
pub(crate) fn validate_request(r: &RichRerankRequest) -> Result<()> {
    ensure!(
        r.schema_version == REQUEST_SCHEMA && !r.query.trim().is_empty() && r.query.len() <= 8192,
        "rich_rerank_invalid_request"
    );
    ensure!(
        (1..=MAX_CANDIDATES).contains(&r.candidates.len())
            && (4096..=MAX_OUTPUT).contains(&r.budget.max_output_bytes)
            && (1..=MAX_METADATA).contains(&r.budget.max_metadata_bytes)
            && r.budget.max_decisions_submissions <= gptgrep_jev::MAX_SUBMISSIONS
            && (1..=4).contains(&r.budget.max_concurrent_batches)
            && (1..=crate::MAX_READER_CACHE_BYTES).contains(&r.budget.cache.max_bytes)
            && (1..=crate::MAX_READER_CACHE_ENTRIES).contains(&r.budget.cache.max_entries),
        "rich_rerank_invalid_budget"
    );
    if let RichBatchGeneration::Exact(pin) = &r.generation {
        ensure!(
            !pin.generation.is_empty()
                && pin.generation.len() <= 256
                && pin
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && pin.manifest_sha256.len() == 64
                && pin
                    .manifest_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "rich_rerank_generation_invalid"
        );
    }
    measure(r, MAX_INPUT)?;
    measure(&r.candidates, r.budget.max_metadata_bytes)?;
    let mut ids = BTreeSet::new();
    let mut evidence = BTreeSet::new();
    let mut visuals = 0;
    for c in &r.candidates {
        ensure!(
            !c.id.is_empty()
                && c.id.len() <= 128
                && !c.id.chars().any(char::is_control)
                && ids.insert(&c.id),
            "rich_rerank_candidate_id_invalid"
        );
        ensure!(
            evidence.insert(hash_json(&c.evidence)?),
            "rich_rerank_duplicate_evidence"
        );
        match &c.evidence {
            EvidenceRef::NativeText { binding } | EvidenceRef::OcrText { binding, .. } => {
                ensure!(
                    binding
                        .byte_end
                        .checked_sub(binding.byte_start)
                        .is_some_and(|n| (1..=MAX_TEXT_WINDOW).contains(&n)),
                    "rich_rerank_text_range_invalid"
                );
            }
            EvidenceRef::VisualRegion { .. } => visuals += 1,
        }
    }
    ensure!(visuals <= MAX_VISUAL, "rich_rerank_visual_limit");
    Ok(())
}
pub fn parse_rich_rerank_request(
    bytes: &[u8],
) -> Result<RichRerankRequest, Box<RichRerankFailure>> {
    if bytes.len() > MAX_INPUT {
        return Err(failure("invalid_request", "Rerank input exceeds1MiB", None));
    }
    let parsed = gptgrep_jev::parse_unique_json(bytes).and_then(serde_json::from_value);
    let request =
        parsed.map_err(|_| failure("invalid_request", "Rerank JSON schema is invalid", None))?;
    validate_request(&request).map_err(|_| {
        failure(
            "invalid_request",
            "Rerank request or budget is invalid",
            None,
        )
    })?;
    Ok(request)
}
pub fn open_rich_rerank_reader(
    root: &Path,
    request: &RichRerankRequest,
) -> Result<RichGenerationReader, Box<RichRerankFailure>> {
    validate_request(request).map_err(|_| {
        failure(
            "invalid_request",
            "Rerank request or budget is invalid",
            None,
        )
    })?;
    let opened = match &request.generation {
        RichBatchGeneration::Current(_) => {
            RichGenerationReader::open_with_cache_budget(root, &request.budget.cache)
        }
        RichBatchGeneration::Exact(pin) => {
            RichGenerationReader::open_pinned_with_cache_budget(root, pin, &request.budget.cache)
        }
    };
    opened.map_err(|_| {
        failure(
            "shared_integrity",
            "Pinned rich generation could not be opened",
            None,
        )
    })
}

/// Owns the actual resolved source content and the same preparing client.
/// It has no Debug/Deserialize or initial-pass capability implementation.
pub struct PreparedRichRerank {
    reader: RichGenerationReader,
    visual: VisualGenerationReader,
    client: JevClient,
    resources: ResourceBudget,
    pool: Arc<Vec<Entry>>,
    labels: Vec<String>,
    batches: Vec<Batch>,
    max_concurrent_batches: usize,
    output_reserved: usize,
    working_reserved: u64,
    report: RichRerankReport,
    last_meter: ReaderReadMetrics,
    started: Instant,
}
impl PreparedRichRerank {
    pub fn generation(&self) -> &GenerationPin {
        &self.report.generation
    }
    pub fn reserved_output_bytes(&self) -> usize {
        self.output_reserved
    }
    pub fn reserved_calls(&self) -> usize {
        self.batches.len()
    }
    pub fn requests(&self) -> impl Iterator<Item = &PreparedDecision> {
        self.batches
            .iter()
            .map(|b| b.request.as_ref().expect("prepared rerank request"))
    }
    pub(crate) fn working_bytes_bound(&self) -> u64 {
        self.working_reserved
    }
}
pub fn prepare_rich_rerank(
    reader: RichGenerationReader,
    client: JevClient,
    request: RichRerankRequest,
) -> Result<PreparedRichRerank, Box<RichRerankFailure>> {
    prepare_with_working_limit(reader, client, request, MAX_WORKING)
}
pub(crate) fn prepare_with_working_limit(
    reader: RichGenerationReader,
    client: JevClient,
    request: RichRerankRequest,
    working_limit: u64,
) -> Result<PreparedRichRerank, Box<RichRerankFailure>> {
    let pin = reader.pin();
    // Each batch is submitted exactly once; its attempt receipt counts one
    // submission, so an attached bounded transient retry is removed.
    let client = client.without_transient_retry();
    prepare_inner(reader, client, request, working_limit).map_err(|e| {
        let code = if e.to_string().contains("finite_context_required")
            || e.to_string().contains("luna_model_required")
        {
            "model_context_required"
        } else if e.to_string().contains("generation_mismatch") {
            "invalid_generation"
        } else if e.chain().any(|e| {
            e.to_string().contains("source changed") || e.to_string().contains("source is stale")
        }) {
            "source_stale"
        } else if e.to_string().contains("budget_insufficient")
            || e.to_string().contains("overflow")
        {
            "output_budget_insufficient"
        } else if e.to_string().contains("submission") {
            "submission_budget_insufficient"
        } else if e.to_string().contains("deadline") {
            "deadline_exceeded"
        } else {
            "invalid_candidate"
        };
        failure(
            code,
            "Source-bound rerank preparation failed before submission",
            Some(pin),
        )
    })
}
fn prepare_inner(
    reader: RichGenerationReader,
    client: JevClient,
    request: RichRerankRequest,
    working_limit: u64,
) -> Result<PreparedRichRerank> {
    let started = Instant::now();
    validate_request(&request)?;
    if let RichBatchGeneration::Exact(pin) = &request.generation {
        ensure!(*pin == reader.pin(), "rich_rerank_generation_mismatch");
    }
    let context = client.execution_context();
    context.original_run_remaining()?;
    context.check_deadline()?;
    let before = context.snapshot()?;
    let run_id = context
        .run_id()
        .context("rich_rerank_finite_context_required")?
        .to_owned();
    let plan_digest = context
        .plan_digest()
        .context("rich_rerank_finite_context_required")?
        .to_owned();
    let clock = context.ledger_clock_anchor()?;
    let before_reads = reader.read_metrics()?;
    let manifest = reader.manifest()?;
    let cache = reader.cache_status()?;
    ensure!(
        cache["capacity_bytes"]
            .as_u64()
            .is_some_and(|n| n <= request.budget.cache.max_bytes as u64)
            && cache["capacity_entries"]
                .as_u64()
                .is_some_and(|n| n <= request.budget.cache.max_entries as u64)
            && (!request.budget.cache.strict_cache_only || cache["strict_cache_only"] == true),
        "rich_rerank_cache_policy_invalid"
    );
    let resources = manifest.resource_budget.clone();
    let visual = reader.visual_reader()?;
    let mut pool = Vec::with_capacity(request.candidates.len());
    let mut labels = Vec::with_capacity(request.candidates.len());
    let mut metadata = 0usize;
    let mut encoded = 0u64;
    let mut text_bytes = 0usize;
    for selected in &request.candidates {
        let id = hash_json(&selected.evidence)?;
        let (document_id, content) = match &selected.evidence {
            EvidenceRef::NativeText { binding } | EvidenceRef::OcrText { binding, .. } => {
                reader.validate_text_evidence(&selected.evidence)?;
                let length = binding.byte_end - binding.byte_start;
                let window = reader.read_text(&binding.artifact_id, binding.byte_start, length)?;
                ensure!(
                    hash_json(&window.evidence)? == id,
                    "rich_rerank_text_evidence_mismatch"
                );
                text_bytes = text_bytes
                    .checked_add(window.text.len())
                    .context("rich_rerank_text_overflow")?;
                (
                    binding.document_id.clone(),
                    RichMultimodalContent::Text {
                        window: Box::new(window),
                        literal_anchor: false,
                    },
                )
            }
            EvidenceRef::VisualRegion {
                document_id,
                rendition_id,
                ..
            } => {
                ensure!(
                    !request.budget.cache.strict_cache_only,
                    "rich_rerank_visual_strict_cache_unavailable"
                );
                let held = visual.open_rendition(rendition_id, &resources)?;
                let candidate = held.candidate().clone();
                ensure!(
                    hash_json(&candidate.evidence)? == id,
                    "rich_rerank_visual_evidence_mismatch"
                );
                encoded = encoded
                    .checked_add(candidate.encoded_bytes)
                    .context("rich_rerank_image_overflow")?;
                (
                    document_id.clone(),
                    RichMultimodalContent::Visual {
                        candidate: Box::new(candidate),
                    },
                )
            }
        };
        metadata = metadata
            .checked_add(measure(&content, request.budget.max_metadata_bytes)?)
            .context("rich_rerank_metadata_overflow")?;
        let document_index = manifest
            .documents
            .iter()
            .position(|d| d.document.id == document_id)
            .context("rich_rerank_document_not_in_pin")?;
        pool.push(Entry {
            id,
            document_index,
            structural: false,
            lexical: false,
            content,
            image: None,
        });
        labels.push(selected.id.clone());
        context.check_deadline()?;
    }
    ensure!(
        metadata <= request.budget.max_metadata_bytes,
        "rich_rerank_metadata_budget_insufficient"
    );
    // Includes retained full-usage values and their numeric projection, without
    // equating owned-buffer admission to native allocation or process RSS.
    let largest_pixels = pool
        .iter()
        .filter_map(|e| match &e.content {
            RichMultimodalContent::Visual { candidate } => {
                Some(u64::from(candidate.width) * u64::from(candidate.height))
            }
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let preliminary = encoded
        .checked_mul(8)
        .and_then(|n| n.checked_add(largest_pixels * 16))
        .and_then(|n| n.checked_add(metadata as u64 * 6 + text_bytes as u64 * 6))
        .and_then(|n| n.checked_add(pool.len() as u64 * 2 * MAX_RESPONSE_BYTES as u64 + 1_048_576))
        .context("rich_rerank_working_overflow")?;
    ensure!(
        preliminary
            <= working_limit
                .min(MAX_WORKING)
                .min(resources.owned_working_bytes),
        "rich_rerank_working_budget_insufficient"
    );
    for entry in &mut pool {
        if let RichMultimodalContent::Visual { candidate } = &entry.content {
            let EvidenceRef::VisualRegion {
                document_id,
                rendition_id,
                rendition_sha256,
                source_sha256,
                ..
            } = &candidate.evidence
            else {
                unreachable!()
            };
            let payload =
                visual.read_rendition(rendition_id, &resources, crate::MAX_VISUAL_OUTPUT_BYTES)?;
            ensure!(
                hash_json(payload.candidate())? == hash_json(candidate.as_ref())?,
                "rich_rerank_visual_binding_invalid"
            );
            let image = ImageEvidence::from_bytes(
                SourceBinding {
                    source_id: document_id.clone(),
                    unit_id: rendition_id.clone(),
                    source_sha256: source_sha256.clone(),
                },
                payload.bytes().to_vec(),
            )?;
            ensure!(
                image.body_sha256() == rendition_sha256
                    && image.dimensions() == (candidate.width, candidate.height)
                    && image.media_type() == candidate.mime,
                "rich_rerank_image_binding_invalid"
            );
            entry.image = Some(image);
        }
        context.check_deadline()?;
    }
    let grouped = groups(&(0..pool.len()).collect::<Vec<_>>(), |i| {
        image_pixels(&pool[i])
    });
    let batches = grouped
        .into_iter()
        .map(|p| prepare_batch(&client, &request.query, p, &pool, None))
        .collect::<Result<Vec<_>>>()?;
    let calls = batches.len();
    ensure!(
        calls <= request.budget.max_decisions_submissions as usize
            && calls
                <= (before.max_submissions as usize).saturating_sub(before.logical_reservations),
        "rich_rerank_submission_budget_insufficient"
    );
    let output_reserved = 32768usize
        .checked_add(request.query.len() * 6 + metadata * 2)
        .and_then(|n| n.checked_add(calls * 8192))
        .context("rich_rerank_output_overflow")?;
    ensure!(
        output_reserved <= request.budget.max_output_bytes,
        "rich_rerank_output_budget_insufficient"
    );
    let body_bytes = batches.iter().map(|b| b.max_bytes as u64).sum::<u64>();
    let working_reserved = preliminary
        .checked_add(body_bytes * 2)
        .context("rich_rerank_working_overflow")?;
    ensure!(
        working_reserved
            <= working_limit
                .min(MAX_WORKING)
                .min(resources.owned_working_bytes),
        "rich_rerank_working_budget_insufficient"
    );
    let pool = Arc::new(pool);
    validate_pool(&reader, &visual, &client, &pool, &resources)?;
    let after = reader.read_metrics()?;
    let visual_count = pool.iter().filter(|e| e.image.is_some()).count();
    let report = RichRerankReport {
        schema_version: RESPONSE_SCHEMA,
        generation: reader.pin(),
        query: request.query.clone(),
        query_options_sha256: hash_json(&request)?,
        run_id,
        plan_digest,
        budget_scope: if clock.is_some() {
            "shared_ledger"
        } else {
            "invocation"
        },
        original_clock_anchor_sha256: clock.as_ref().map(hash_json).transpose()?,
        ranked_candidates: Vec::with_capacity(pool.len()),
        coverage: RichRerankCoverage {
            scope: "supplied_candidate_pool_only",
            supplied_candidates: pool.len(),
            text_candidates: pool.len() - visual_count,
            visual_candidates: visual_count,
            scored_candidates: 0,
            complete: false,
        },
        attempts: Vec::with_capacity(calls),
        reserved_calls: calls,
        logical_calls_attempted: 0,
        validated_responses: 0,
        accounting_complete: false,
        context_before: before,
        context_after: None,
        context_observation_scope: "shared_context_cumulative_not_operation_attribution",
        read_intervals: vec![after.since(&before_reads)?],
        read_meter_scope: "shared_reader_intervals_not_exclusive_operation",
        elapsed_ms: 0,
        citation_status: "model_exposure_only_delivery_unqualified",
    };
    Ok(PreparedRichRerank {
        reader,
        visual,
        client,
        resources,
        pool,
        labels,
        batches,
        max_concurrent_batches: request.budget.max_concurrent_batches,
        output_reserved,
        working_reserved,
        report,
        last_meter: after,
        started,
    })
}

impl PreparedRichRerank {
    fn finish_observations(&mut self) {
        if let Ok(after) = self.reader.read_metrics()
            && let Ok(delta) = after.since(&self.last_meter)
        {
            self.report.read_intervals.push(delta);
            self.last_meter = after;
        }
        self.report.elapsed_ms = self.started.elapsed().as_millis();
        self.report.context_after = self.client.execution_context().snapshot().ok();
        self.report.accounting_complete = self.report.logical_calls_attempted > 0
            && self.report.logical_calls_attempted == self.report.validated_responses;
    }
    fn failed(mut self, code: &'static str) -> Box<RichRerankFailure> {
        self.finish_observations();
        Box::new(RichRerankFailure {
            schema_version: RESPONSE_SCHEMA,
            code,
            message: "Required source-bound rerank failed; available observations retained without fallback",
            generation: Some(self.reader.pin()),
            report: Some(self.report),
        })
    }
    fn apply_scores(&mut self, scores: Vec<(String, f64, Option<f64>)>) -> Result<()> {
        let mut seen = BTreeSet::new();
        for (id, score, confidence) in scores {
            ensure!(
                score.is_finite() && (0.0..=1.0).contains(&score),
                "rich_rerank_score_invalid"
            );
            let position = self
                .pool
                .iter()
                .position(|p| p.id == id)
                .context("rich_rerank_score_id_invalid")?;
            ensure!(seen.insert(id.clone()), "rich_rerank_score_duplicate");
            self.report.ranked_candidates.push(RichRankedCandidate {
                id: self.labels[position].clone(),
                candidate_id: id,
                evidence: self.pool[position].content.evidence().clone(),
                score,
                confidence,
            });
        }
        ensure!(
            seen.len() == self.pool.len(),
            "rich_rerank_score_incomplete"
        );
        self.report.ranked_candidates.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.candidate_id.cmp(&b.candidate_id))
        });
        self.report.coverage.scored_candidates = self.report.ranked_candidates.len();
        Ok(())
    }
    pub async fn execute(self) -> Result<RichRerankReport, Box<RichRerankFailure>> {
        self.execute_with_observer(&|_| Ok(())).await
    }
    pub async fn execute_with_observer(
        mut self,
        observer: &(dyn Fn(&RichMultimodalAttempt) -> Result<()> + Send + Sync),
    ) -> Result<RichRerankReport, Box<RichRerankFailure>> {
        let batches = std::mem::take(&mut self.batches);
        let scores = EvidenceStage {
            reader: &self.reader,
            visual: &self.visual,
            client: &self.client,
            pool: &self.pool,
            resources: &self.resources,
            max_concurrent_batches: self.max_concurrent_batches,
            attempts: &mut self.report.attempts,
            logical_calls_attempted: &mut self.report.logical_calls_attempted,
            validated_responses: &mut self.report.validated_responses,
            read_intervals: &mut self.report.read_intervals,
            last_meter: &mut self.last_meter,
        }
        .run("evidence_reranking", batches, observer)
        .await;
        let scores = match scores {
            Ok(s) => s,
            Err(_) => return Err(self.failed("required_stage_failed")),
        };
        if self.apply_scores(scores).is_err() {
            return Err(self.failed("identity_invariant_failed"));
        }
        self.finish_observations();
        if validate_pool(
            &self.reader,
            &self.visual,
            &self.client,
            &self.pool,
            &self.resources,
        )
        .is_err()
            || measure(&self.report, self.output_reserved).is_err()
        {
            return Err(self.failed("completion_invariant_failed"));
        }
        self.report.coverage.complete = true;
        Ok(self.report)
    }
}

fn observed<T>(
    usage: &Value,
    name: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> UsageObservation<T> {
    let (presence, value) = match usage.get(name) {
        None => (UsagePresence::Absent, None),
        Some(Value::Null) => (UsagePresence::Null, None),
        Some(v) => match parse(v) {
            Some(v) => (UsagePresence::Reported, Some(v)),
            None => (UsagePresence::Invalid, None),
        },
    };
    UsageObservation { presence, value }
}
#[derive(Serialize)]
struct BoundedUsage<'a> {
    input_tokens: UsageObservation<u64>,
    output_tokens: UsageObservation<u64>,
    cost: UsageObservation<f64>,
    usage_observed: bool,
    complete_usage_sha256: Option<&'a str>,
    projection: &'static str,
}
#[derive(Serialize)]
struct BoundedAttempt<'a> {
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
    failure: Option<&'a DecisionFailureDiagnostic>,
    actual_provider_requests: Option<u64>,
    usage: BoundedUsage<'a>,
}
fn serialize_attempts<S: Serializer>(
    attempts: &[RichMultimodalAttempt],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut seq = serializer.serialize_seq(Some(attempts.len()))?;
    for a in attempts {
        let usage = a.full_usage();
        seq.serialize_element(&BoundedAttempt{stage:a.stage,batch_index:a.batch_index,candidate_ids:&a.candidate_ids,
            requested_model:&a.requested_model,prepared_request_sha256:&a.prepared_request_sha256,encoded_request_bytes:a.encoded_request_bytes,
            question_count:a.question_count,response_status:a.response_status,returned_model:a.returned_model.as_deref(),
            provider_response_id:a.provider_response_id.as_deref(),provider:a.provider.as_deref(),failure:a.failure.as_ref(),actual_provider_requests:None,
            usage:BoundedUsage{input_tokens:observed(usage,"input_tokens",Value::as_u64),output_tokens:observed(usage,"output_tokens",Value::as_u64),
                cost:observed(usage,"cost",|v|v.as_f64().filter(|n|n.is_finite()&&*n>=0.0)),usage_observed:a.usage.usage_observed,
                complete_usage_sha256:a.usage.complete_usage_sha256.as_deref(),
                projection:"bounded_numeric_fields; cost is a lossy finite_f64 projection; other usage metadata omitted and SHA retained"}})?;
    }
    seq.end()
}

#[cfg(test)]
#[path = "rerank_tests.rs"]
mod tests;
