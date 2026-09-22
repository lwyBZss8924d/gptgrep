//! Optional query-view retrieval over one immutable source snapshot.
use super::*;
use futures_util::{StreamExt, stream};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Per-view counts are separate observations, not unique-document totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedViewCoverage {
    pub view_id: String,
    pub query_sha256: String,
    pub coverage: Coverage,
    /// Raw candidate windows before exact-span deduplication, including spans
    /// represented by more than one node in this view.
    pub collected_candidates: usize,
    /// Unique selected union spans discovered by this view; overlaps with other
    /// views, but each exact span counts at most once within a view.
    pub union_candidates: Option<usize>,
    /// Unique spans from this view omitted by the union cap. Duplicate candidate
    /// windows merge into one span and are not counted as cap omissions.
    pub omitted_candidates: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedSpanProvenance {
    pub generation: String,
    pub path: String,
    pub source_sha256: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text_sha256: String,
    pub view_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlannedCoverage {
    pub indexed_files: usize,
    pub scoped_files: usize,
    /// Only completed views appear here on failure; operation receipts preserve
    /// partial/interrupted view observations without inventing candidate totals.
    pub views: Vec<PlannedViewCoverage>,
    /// Span totals remain unavailable until every view has completed and the
    /// deterministic union has been constructed successfully.
    pub candidates_before_dedup: Option<usize>,
    pub unique_candidates: Option<usize>,
    pub union_candidates: Option<usize>,
    pub omitted_unique_candidates: Option<usize>,
    /// At most 24 selected spans, before relevance filtering/delivery limits.
    pub selected_spans: Vec<PlannedSpanProvenance>,
    pub reranked_candidates: usize,
    pub filtered_candidates: usize,
    pub retained_literal_anchors: usize,
    pub stale_files: Vec<String>,
    pub truncated: bool,
}

/// A replacement snapshot for exactly one logical operation. Consumers replace
/// the same operation_id slot and sum disjoint slots once, never event history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedSearchEvent {
    /// q0.route, q1.route, q2.route, or union.rerank.
    pub operation_id: String,
    /// admitted, before_call, after_reply, finished, failed, or interrupted.
    pub event: String,
    pub query_sha256: String,
    pub original_question_sha256: String,
    pub metrics: Metrics,
    /// Available for a single view; union document counts are not aggregated.
    pub coverage: Option<Coverage>,
    pub provider_response_id: Option<String>,
    pub provider: Option<String>,
    pub cause: Option<String>,
    /// Whether all admitted logical calls returned validated replies. Optional
    /// provider usage remains null when unavailable; no physical/billing claim.
    pub accounting_complete: bool,
}

pub type PlannedSearchObserver<'a> = dyn Fn(&PlannedSearchEvent) -> Result<()> + Send + Sync + 'a;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedSearchReport {
    pub schema_version: String,
    pub query: String,
    pub mode: String,
    pub document_scope: Option<String>,
    pub root: PathBuf,
    pub generation: String,
    pub index_used: bool,
    pub source_fresh: Option<bool>,
    pub minimum_relevance_score: Option<f64>,
    pub hits: Vec<Hit>,
    pub coverage: PlannedCoverage,
    pub metrics: Metrics,
    pub warnings: Vec<String>,
    pub operations: Vec<PlannedSearchEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedSearchError {
    pub stage: String,
    pub cause: String,
    pub metrics: Metrics,
    pub coverage: PlannedCoverage,
    pub document_scope: Option<String>,
    pub generation: Option<String>,
    pub operations: Vec<PlannedSearchEvent>,
    pub accounting_complete: bool,
}

impl std::fmt::Display for PlannedSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Planned search failed during {}: {} (accounting incomplete)",
            self.stage, self.cause
        )
    }
}
impl std::error::Error for PlannedSearchError {}

struct Recorder<'a> {
    observer: &'a PlannedSearchObserver<'a>,
    operations: Mutex<BTreeMap<String, PlannedSearchEvent>>,
}

impl Recorder<'_> {
    fn record(&self, event: PlannedSearchEvent) -> Result<()> {
        // Retain the actual observation before the fallible durable callback.
        // The lock is never held across callbacks or network awaits.
        self.operations
            .lock()
            .unwrap()
            .insert(event.operation_id.clone(), event.clone());
        (self.observer)(&event).map_err(|_| {
            anyhow::anyhow!("Planned search observer failed; no further calls admitted")
        })
    }

    fn events(&self) -> Vec<PlannedSearchEvent> {
        self.operations.lock().unwrap().values().cloned().collect()
    }
}

/// Lives inside the borrowed route/rerank future. Dropping the whole plan (or a
/// failed sibling) drops this guard and synchronously records interruption.
struct Operation<'a, 'b> {
    recorder: &'a Recorder<'b>,
    initial: PlannedSearchEvent,
    started: Instant,
}

impl<'a, 'b> Operation<'a, 'b> {
    fn new(
        recorder: &'a Recorder<'b>,
        id: String,
        query: &str,
        original: &str,
        coverage: Option<Coverage>,
    ) -> Self {
        Self {
            recorder,
            initial: PlannedSearchEvent {
                operation_id: id,
                event: "admitted".into(),
                query_sha256: digest(query.as_bytes()),
                original_question_sha256: digest(original.as_bytes()),
                metrics: Metrics::default(),
                coverage,
                provider_response_id: None,
                provider: None,
                cause: None,
                accounting_complete: false,
            },
            started: Instant::now(),
        }
    }

    fn current(&self) -> PlannedSearchEvent {
        self.recorder
            .operations
            .lock()
            .unwrap()
            .get(&self.initial.operation_id)
            .cloned()
            .unwrap_or_else(|| self.initial.clone())
    }

    fn emit(&self, mut event: PlannedSearchEvent) -> Result<()> {
        event.metrics.elapsed_ms = self.started.elapsed().as_millis();
        self.recorder.record(event)
    }

    fn progress(&self, progress: &JevSearchProgress) -> Result<()> {
        let mut event = self.current();
        event.event.clone_from(&progress.event);
        event.metrics = progress.metrics.clone();
        if event.coverage.is_some() {
            event.coverage = Some(progress.coverage.clone());
        }
        if progress.event == "after_reply" {
            event
                .provider_response_id
                .clone_from(&progress.provider_response_id);
            event.provider.clone_from(&progress.provider);
        }
        self.emit(event)
    }

    fn finish(&self, metrics: Metrics, coverage: Option<Coverage>) -> Result<()> {
        let mut event = self.current();
        event.event = "finished".into();
        event.accounting_complete = metrics.jev_calls_attempted == metrics.jev_requests;
        event.metrics = metrics;
        event.coverage = coverage;
        self.emit(event)
    }

    fn fail(&self, error: anyhow::Error) -> anyhow::Error {
        let mut event = self.current();
        event.event = "failed".into();
        event.accounting_complete = false;
        event.cause = Some(sanitized_cause(&error));
        // If persistence itself failed, retain the observed slot in the typed
        // error and let the caller report its ledger failure separately.
        let _ = self.emit(event);
        error
    }
}

impl Drop for Operation<'_, '_> {
    fn drop(&mut self) {
        let mut event = self.current();
        if !matches!(event.event.as_str(), "finished" | "failed" | "interrupted") {
            event.event = "interrupted".into();
            event.accounting_complete = false;
            event.cause = Some(
                "Operation interrupted; provider processing and unreturned usage are unknown"
                    .into(),
            );
            let _ = self.emit(event);
        }
    }
}

fn sanitized_cause(error: &anyhow::Error) -> String {
    if let Some(error) = error.downcast_ref::<JevSearchError>() {
        return error.cause.clone();
    }
    let text = error.to_string();
    match text.as_str() {
        "index generation changed during planned search"
        | "planned candidate spans conflict"
        | "planned search has no candidates for required evidence reranking"
        | "planned search requires hybrid mode"
        | "planned search accepts at most two alternate queries"
        | "alternate queries must contain 1..1024 UTF-8 bytes and a word"
        | "query views must be distinct after trimming"
        | "Planned search observer failed; no further calls admitted" => text,
        _ => "Planned search validation failed; no fallback or partial delivery".into(),
    }
}

fn aggregate(operations: &[PlannedSearchEvent], started: Instant) -> Metrics {
    let mut result = Metrics {
        elapsed_ms: started.elapsed().as_millis(),
        ..Metrics::default()
    };
    for operation in operations {
        result.jev_calls_attempted += operation.metrics.jev_calls_attempted;
        result.jev_requests += operation.metrics.jev_requests;
        result.jev_candidate_bytes += operation.metrics.jev_candidate_bytes;
        result
            .jev_models
            .extend(operation.metrics.jev_models.clone());
        result.jev_usage.extend(operation.metrics.jev_usage.clone());
    }
    result
}

fn ensure_generation(snapshot: &Snapshot) -> Result<()> {
    let pointer_path = snapshot.manifest.root.join(STATE).join("CURRENT.json");
    no_symlink(&pointer_path)?;
    let pointer: Pointer = serde_json::from_slice(&bounded_read(&pointer_path, 4096)?)?;
    ensure!(
        pointer.schema_version == SCHEMA && pointer.generation == snapshot.manifest.generation,
        "index generation changed during planned search"
    );
    Ok(())
}

/// Collect original plus at most two alternate views on one pinned snapshot,
/// with at most two routing futures alive and one original-question rerank.
///
/// The caller owns its absolute deadline by dropping this borrowed future. No
/// tasks are spawned. Every admitted operation then reports interruption, and
/// already observed replies remain in disjoint observer slots. No failed route
/// can produce a partial-success report or issue evidence.
#[allow(clippy::too_many_arguments)]
pub async fn search_planned_with_client_and_observer(
    root: &Path,
    original_question: &str,
    alternate_queries: &[String],
    options: &SearchOptions,
    expected_generation: &str,
    client: &JevClient,
    observer: &PlannedSearchObserver<'_>,
) -> Result<PlannedSearchReport> {
    let started = Instant::now();
    let recorder = Recorder {
        observer,
        operations: Mutex::new(BTreeMap::new()),
    };
    let mut coverage = PlannedCoverage::default();
    let mut stage = "initialization";
    let mut generation = None;
    let result = async {
        validate_search_options(original_question, options)?;
        ensure!(
            options.mode == "hybrid",
            "planned search requires hybrid mode"
        );
        ensure!(
            alternate_queries.len() <= 2,
            "planned search accepts at most two alternate queries"
        );
        let mut seen = HashSet::from([original_question.trim()]);
        for query in alternate_queries {
            ensure!(
                !query.trim().is_empty() && query.len() <= 1024 && !tokens(query).is_empty(),
                "alternate queries must contain 1..1024 UTF-8 bytes and a word"
            );
            ensure!(
                seen.insert(query.trim()),
                "query views must be distinct after trimming"
            );
        }
        let snapshot = Snapshot::open(root)?;
        generation = Some(snapshot.manifest.generation.clone());
        ensure!(
            snapshot.manifest.generation == expected_generation,
            "index generation changed during planned search"
        );
        let scoped = scoped_documents(&snapshot.manifest.documents, options.document.as_deref())?;
        coverage.indexed_files = snapshot.manifest.documents.len();
        coverage.scoped_files = scoped.len();
        let common = Coverage {
            indexed_files: coverage.indexed_files,
            scoped_files: coverage.scoped_files,
            ..Coverage::default()
        };
        let queries: Vec<String> = std::iter::once(original_question.to_owned())
            .chain(alternate_queries.iter().cloned())
            .collect();
        stage = "document_routing";
        let mut batches = Vec::with_capacity(queries.len());
        {
            let mut pending = stream::iter(queries.clone().into_iter().enumerate())
                .map(|(ordinal, query)| {
                    let snapshot = &snapshot;
                    let recorder = &recorder;
                    let common = common.clone();
                    async move {
                        collect_planned_view(
                            snapshot,
                            ordinal,
                            &query,
                            original_question,
                            options,
                            client,
                            recorder,
                            common,
                        )
                        .await
                    }
                })
                .buffer_unordered(2);
            while let Some(result) = pending.next().await {
                let (ordinal, batch) = result?;
                coverage.views.push(PlannedViewCoverage {
                    view_id: format!("q{ordinal}"),
                    query_sha256: digest(queries[ordinal].as_bytes()),
                    coverage: batch.coverage.clone(),
                    collected_candidates: batch.hits.len(),
                    union_candidates: None,
                    omitted_candidates: None,
                });
                batches.push((ordinal, batch));
            }
        }
        coverage
            .views
            .sort_by(|left, right| left.view_id.cmp(&right.view_id));
        batches.sort_by_key(|(ordinal, _)| *ordinal);
        stage = "union";
        ensure_generation(&snapshot)?;
        let mut union = merge_candidates(
            &snapshot.manifest.generation,
            &batches,
            options.max_candidates,
            &mut coverage,
        )?;
        stage = "union.rerank";
        let operation = Operation::new(
            &recorder,
            "union.rerank".into(),
            original_question,
            original_question,
            None,
        );
        let reranked = async {
            operation.emit(operation.initial.clone())?;
            ensure!(
                !union.hits.is_empty(),
                "planned search has no candidates for required evidence reranking"
            );
            let observer = |progress: &JevSearchProgress| operation.progress(progress);
            let mut metrics = Metrics::default();
            score_candidates(
                &snapshot,
                original_question,
                options,
                client,
                Some(&observer),
                &mut union,
                &mut metrics,
                operation.started,
            )
            .await?;
            operation.finish(metrics, None)
        }
        .await;
        // Preserve observed scoring counts even if reply persistence or a later
        // generation check fails before delivery.
        coverage.reranked_candidates = union.coverage.reranked_candidates;
        coverage.filtered_candidates = union.coverage.filtered_candidates;
        coverage.retained_literal_anchors = union.coverage.retained_literal_anchors;
        if let Err(error) = reranked {
            return Err(operation.fail(error));
        }
        stage = "delivery";
        ensure_generation(&snapshot)?;
        finish_candidate_batch(&snapshot, options, &mut union);
        // The final source reads above are synchronous; check the pointer again
        // after them so a concurrently published generation cannot be delivered.
        ensure_generation(&snapshot)?;
        coverage.reranked_candidates = union.coverage.reranked_candidates;
        coverage.filtered_candidates = union.coverage.filtered_candidates;
        coverage.retained_literal_anchors = union.coverage.retained_literal_anchors;
        coverage.stale_files = union.coverage.stale_files.clone();
        coverage.truncated |= union.coverage.truncated;
        let operations = recorder.events();
        let metrics = aggregate(&operations, started);
        Ok(PlannedSearchReport {
            schema_version: SCHEMA.into(),
            query: original_question.into(),
            mode: options.mode.clone(),
            document_scope: options.document.clone(),
            root: snapshot.manifest.root,
            generation: snapshot.manifest.generation,
            index_used: true,
            source_fresh: if union.hits.is_empty() {
                None
            } else {
                Some(true)
            },
            minimum_relevance_score: Some(options.min_score),
            hits: union.hits,
            coverage: coverage.clone(),
            metrics,
            warnings: union.warnings,
            operations,
        })
    }
    .await;
    result.map_err(|error| {
        let operations = recorder.events();
        let failed_stage = operations
            .iter()
            .find(|operation| operation.event == "failed")
            .map(|operation| operation.operation_id.clone())
            .unwrap_or_else(|| stage.into());
        // Completion order is not part of the failure receipt's identity either.
        coverage
            .views
            .sort_by(|left, right| left.view_id.cmp(&right.view_id));
        anyhow::Error::new(PlannedSearchError {
            stage: failed_stage,
            cause: sanitized_cause(&error),
            metrics: aggregate(&operations, started),
            coverage,
            document_scope: options.document.clone(),
            generation,
            operations,
            accounting_complete: false,
        })
    })
}

#[allow(clippy::too_many_arguments)]
async fn collect_planned_view(
    snapshot: &Snapshot,
    ordinal: usize,
    query: &str,
    original: &str,
    options: &SearchOptions,
    client: &JevClient,
    recorder: &Recorder<'_>,
    common: Coverage,
) -> Result<(usize, CandidateBatch)> {
    let operation = Operation::new(
        recorder,
        format!("q{ordinal}.route"),
        query,
        original,
        Some(common),
    );
    let result = async {
        operation.emit(operation.initial.clone())?;
        ensure_generation(snapshot)?;
        let observer = |progress: &JevSearchProgress| operation.progress(progress);
        let mut metrics = Metrics::default();
        let mut batch = collect_candidate_view(
            snapshot,
            query,
            options,
            Some(client),
            Some(&observer),
            &mut metrics,
            operation.started,
        )
        .await?;
        if ordinal != 0 {
            for hit in &mut batch.hits {
                hit.literal_anchor = false;
            }
        }
        operation.finish(metrics, Some(batch.coverage.clone()))?;
        Ok((ordinal, batch))
    }
    .await;
    result.map_err(|error| operation.fail(error))
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SpanKey {
    generation: String,
    path: String,
    source_sha256: String,
    start: usize,
    end: usize,
}

impl SpanKey {
    fn new(generation: &str, hit: &Hit) -> Self {
        Self {
            generation: generation.into(),
            path: hit.path.clone(),
            source_sha256: hit.source_sha256.clone(),
            start: hit.byte_start,
            end: hit.byte_end,
        }
    }
}

fn merge_candidates(
    generation: &str,
    batches: &[(usize, CandidateBatch)],
    limit: usize,
    coverage: &mut PlannedCoverage,
) -> Result<CandidateBatch> {
    let mut indices: HashMap<SpanKey, usize> = HashMap::new();
    let mut unique: Vec<(Hit, PlannedSpanProvenance)> = Vec::new();
    let mut per_view = Vec::new();
    let mut warnings = Vec::new();
    let mut stale = HashSet::new();
    let mut truncated = false;
    for (ordinal, batch) in batches {
        let mut view_indices = Vec::new();
        truncated |= batch.coverage.truncated;
        stale.extend(batch.coverage.stale_files.iter().cloned());
        for warning in &batch.warnings {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        let view_id = format!("q{ordinal}");
        for hit in &batch.hits {
            let key = SpanKey::new(generation, hit);
            let index = if let Some(&index) = indices.get(&key) {
                let (previous, provenance) = &mut unique[index];
                ensure!(
                    previous.text == hit.text,
                    "planned candidate spans conflict"
                );
                // Stable view order chooses the first node for identical spans.
                previous.literal_anchor |= *ordinal == 0 && hit.literal_anchor;
                if !provenance.view_ids.contains(&view_id) {
                    provenance.view_ids.push(view_id.clone());
                }
                index
            } else {
                let index = unique.len();
                indices.insert(key, index);
                let mut representative = hit.clone();
                representative.literal_anchor &= *ordinal == 0;
                unique.push((
                    representative,
                    PlannedSpanProvenance {
                        generation: generation.into(),
                        path: hit.path.clone(),
                        source_sha256: hit.source_sha256.clone(),
                        byte_start: hit.byte_start,
                        byte_end: hit.byte_end,
                        text_sha256: digest(hit.text.as_bytes()),
                        view_ids: vec![view_id.clone()],
                    },
                ));
                index
            };
            view_indices.push(index);
        }
        per_view.push(view_indices);
    }
    let mut selected = HashSet::new();
    let mut order = Vec::new();
    let rounds = per_view.iter().map(Vec::len).max().unwrap_or(0);
    'rounds: for offset in 0..rounds {
        for view in &per_view {
            if let Some(&index) = view.get(offset)
                && selected.insert(index)
            {
                order.push(index);
                if order.len() == limit {
                    break 'rounds;
                }
            }
        }
    }
    coverage.candidates_before_dedup =
        Some(batches.iter().map(|(_, batch)| batch.hits.len()).sum());
    coverage.unique_candidates = Some(unique.len());
    coverage.union_candidates = Some(order.len());
    coverage.omitted_unique_candidates = Some(unique.len() - order.len());
    coverage.truncated = truncated || unique.len() > order.len();
    for (view, view_indices) in coverage.views.iter_mut().zip(per_view) {
        let unique_view_indices: HashSet<_> = view_indices.into_iter().collect();
        let selected_count = unique_view_indices.intersection(&selected).count();
        view.union_candidates = Some(selected_count);
        view.omitted_candidates = Some(unique_view_indices.len() - selected_count);
    }
    let mut hits = Vec::with_capacity(order.len());
    for index in order {
        let (hit, provenance) = &unique[index];
        hits.push(hit.clone());
        coverage.selected_spans.push(provenance.clone());
    }
    let mut stale_files: Vec<_> = stale.into_iter().collect();
    stale_files.sort();
    coverage.stale_files = stale_files.clone();
    Ok(CandidateBatch {
        hits,
        coverage: Coverage {
            indexed_files: coverage.indexed_files,
            scoped_files: coverage.scoped_files,
            stale_files,
            truncated: coverage.truncated,
            ..Coverage::default()
        },
        warnings,
    })
}

#[cfg(test)]
mod tests;
