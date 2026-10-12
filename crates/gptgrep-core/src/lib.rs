//! Source-bound document snapshots and vectorless retrieval orchestration.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use gptgrep_jev::{
    Candidate, DecisionFailure, DecisionRetriedAttempt, JevClient, RerankResponse, TransientRetry,
};

pub mod batch;
mod custody;
mod derived_visual;
mod evidence_roles;
pub mod parse_agent;
pub mod rich_batch;
pub use gptgrep_index::{IndexContentLimits, IndexContentReport};
mod rich_decisions;
mod rich_multimodal;
pub use derived_visual::{
    DerivedCacheBudget, DerivedCacheStatus, DerivedRecipeToken, DerivedVisualCache,
    DerivedVisualPin, DerivedVisualReceipt, VisualSource,
};
pub use evidence_roles::{
    CandidateDisposition, CandidateEligibility, DeliveredSetSufficiency, EvidenceRole,
    EvidenceRoleCandidateDecision, EvidenceRoleDiagnostics, EvidenceRoleHint, EvidenceRoleSource,
    EvidenceRoleSpan, MAX_EVIDENCE_ROLE_METADATA_BYTES,
};
pub use rich_decisions::{
    PreparedRichTextSearch, RichTextAttempt, RichTextCoverage, RichTextHit, RichTextInitialPass,
    RichTextSearchError, RichTextSearchOptions, RichTextSearchReport, RichTextUsageProjection,
    prepare_rich_text_search,
};
pub use rich_multimodal::{
    PreparedRichMultimodalSearch, PreparedRichRerank, RichMultimodalAttempt, RichMultimodalContent,
    RichMultimodalCoverage, RichMultimodalHit, RichMultimodalInitialPass,
    RichMultimodalSearchError, RichMultimodalSearchOptions, RichMultimodalSearchReport,
    RichRankedCandidate, RichRerankBudget, RichRerankCandidate, RichRerankCoverage,
    RichRerankFailure, RichRerankReport, RichRerankRequest, open_rich_rerank_reader,
    parse_rich_rerank_request, prepare_rich_multimodal_search, prepare_rich_rerank,
    rich_rerank_context_failure, rich_rerank_input_io_failure,
};
mod navigation_overlay;
mod planned;
mod read_cache;
mod rich_generation;
mod rich_ingest;
use gptgrep_pageindex::TreeNode;
pub use navigation_overlay::{
    BoundNavigationOverlay, MAX_NAVIGATION_HINT_BYTES, MAX_NAVIGATION_OVERLAY_BYTES,
    NavigationDocumentCursor, NavigationDocumentHints, NavigationDocumentIdentity,
    NavigationDocumentWindow, NavigationHint, NavigationHintOrigin, NavigationHintTarget,
    NavigationJevIdentity, NavigationOverlay, NavigationOverlayBinding, NavigationOverlayCoverage,
    NavigationOverlayProducer, NavigationOverlayPublication, NavigationWindowBinding,
    navigation_overlay_binding, open_navigation_document, publish_navigation_overlay,
    read_document_window, read_navigation_overlay,
};
pub use planned::{
    PlannedCoverage, PlannedScoringPolicy, PlannedSearchError, PlannedSearchEvent,
    PlannedSearchObserver, PlannedSearchReport, PlannedSpanProvenance, PlannedViewCoverage,
    search_planned_with_client_and_observer, search_planned_with_policy_and_observer,
};
use read_cache::{CacheError, CacheKey, ReadCache, ReadMeter, ReadMetrics, ReadOrigin, ReadPin};
pub use rich_generation::{
    ArtifactSearchReport, ArtifactTextWindow, EvidenceRef, MAX_NATIVE_METADATA_BYTES,
    MAX_NATIVE_METADATA_WORKING_BYTES, MAX_VISUAL_OUTPUT_BYTES, NATIVE_CHARACTER_FIELDS,
    NATIVE_IMAGE_FIELDS, NATIVE_METADATA_SCALAR_FIELDS, NATIVE_OBJECT_FIELDS,
    NATIVE_SEGMENT_FIELDS, NATIVE_WORD_FIELDS, NativeMetadataBinding, NativeMetadataCollection,
    NativeMetadataLease, NativeMetadataReadBudget, NativeMetadataSelection,
    NativePageMetadataSnapshot, RICH_MANIFEST_SCHEMA, RichDocument, RichGenerationReader,
    RichGenerationWriter, RichManifest, StoredArtifact, TextEvidenceBinding, VerifiedRenditionFile,
    VerifiedVisualPayload, VisualCandidate, VisualCandidateReport, VisualCandidateRequest,
    VisualCoverage, VisualGenerationReader, VisualScope,
};
pub use rich_ingest::{
    MAX_RICH_OCR_PAGES, PreparedRichIngest, RichIngestDocumentPlan, RichIngestOptions,
    RichIngestPagePlan, RichIngestReport, prepare_rich_ingest, read_rich_runtime_descriptor,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "gptgrep.v1";
const STATE: &str = ".gptgrep";
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 256 * 1024 * 1024;

pub const MAX_READER_CACHE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_READER_CACHE_ENTRIES: usize = 1024;

/// Cache admission is separate from the historical text/type size limits.
/// Default readers use a bounded uncached load when cache capacity is unavailable.
/// Byte capacity charges immutable payloads retained by cache entries or pins;
/// it does not bound uncached copies, manifests, native allocations or process RSS.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderCacheBudget {
    pub max_bytes: usize,
    pub max_entries: usize,
    pub strict_cache_only: bool,
}
impl Default for ReaderCacheBudget {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_entries: 128,
            strict_cache_only: false,
        }
    }
}
impl ReaderCacheBudget {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_bytes > 0
                && self.max_bytes <= MAX_READER_CACHE_BYTES
                && self.max_entries > 0
                && self.max_entries <= MAX_READER_CACHE_ENTRIES,
            "invalid reader cache budget"
        );
        Ok(())
    }
}

/// Observed byte counts, never kernel disk traffic or native index IO. Logical
/// bytes are text windows returned by reader APIs, not final wire/provider bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReaderReadMetrics {
    pub logical_reader_bytes: u64,
    pub cached_logical_bytes: u64,
    pub uncached_logical_bytes: u64,
    pub observed_rust_read_bytes: u64,
    pub observed_hash_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_fallbacks: u64,
    pub uncached_load_bytes: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub coverage: String,
}
impl From<ReadMetrics> for ReaderReadMetrics {
    fn from(metrics: ReadMetrics) -> Self {
        Self {
            logical_reader_bytes: metrics.logical_delivered_bytes,
            cached_logical_bytes: metrics.cached_logical_bytes,
            uncached_logical_bytes: metrics.uncached_logical_bytes,
            observed_rust_read_bytes: metrics.physical_read_bytes,
            observed_hash_bytes: metrics.physical_hash_bytes,
            cache_hits: metrics.cache_hits, cache_misses: metrics.cache_misses,
            cache_fallbacks: metrics.cache_fallbacks, uncached_load_bytes: metrics.uncached_load_bytes,
            coverage: "partial: Rust reader read/hash paths and node/artifact text-window delivery; excludes search/navigation logical delivery, native index/mmap, kernel disk traffic, parser and full-generation verification helpers; counters shared across reader clones".into(),
        }
    }
}
impl ReaderReadMetrics {
    pub(crate) fn since(&self, before: &Self) -> Result<Self> {
        macro_rules! delta {
            ($field:ident) => {
                self.$field
                    .checked_sub(before.$field)
                    .context("reader meter counter decreased")?
            };
        }
        Ok(Self {
            logical_reader_bytes: delta!(logical_reader_bytes),
            cached_logical_bytes: delta!(cached_logical_bytes),
            uncached_logical_bytes: delta!(uncached_logical_bytes),
            observed_rust_read_bytes: delta!(observed_rust_read_bytes),
            observed_hash_bytes: delta!(observed_hash_bytes),
            cache_hits: delta!(cache_hits),
            cache_misses: delta!(cache_misses),
            cache_fallbacks: delta!(cache_fallbacks),
            uncached_load_bytes: delta!(uncached_load_bytes),
            coverage: self.coverage.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSpan {
    pub number: u32,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub path: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub text_sha256: String,
    pub parser: String,
    pub title: String,
    pub pages: Vec<PageSpan>,
    pub nodes: Vec<TreeNode>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: String,
    pub generation: String,
    pub root: PathBuf,
    pub created_unix_ms: u128,
    pub documents: Vec<Document>,
    pub excluded_files: usize,
    #[serde(default)]
    pub tree_profile: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pointer {
    schema_version: String,
    generation: String,
    manifest_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub schema_version: String,
    pub root: PathBuf,
    pub generation: String,
    pub indexed_files: usize,
    pub excluded_files: usize,
    pub source_bytes: u64,
    pub elapsed_ms: u128,
    pub warnings: Vec<String>,
    pub tree_profile: String,
}

/// Coverage of one excerpt within its node, not cumulative coverage across reads.
/// Unread counts are node bytes outside this excerpt; prior reads may cover them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCoverage {
    pub complete: bool,
    pub unread_before_bytes: usize,
    pub unread_after_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub path: String,
    pub node_id: String,
    pub title: String,
    pub line_start: usize,
    pub line_end: usize,
    pub page_start: u32,
    pub page_end: u32,
    pub match_line: Option<usize>,
    /// One-based UTF-8 byte column of the match within its line.
    pub match_column: Option<usize>,
    /// Exact half-open UTF-8 byte range in canonical text (raw source for plaintext).
    pub byte_start: usize,
    pub byte_end: usize,
    /// Byte offset from this node's canonical start. None when grep context
    /// crosses the matched node's boundaries.
    pub node_offset: Option<usize>,
    /// First unread byte relative to the node, or None at EOF/outside the node.
    pub next_offset: Option<usize>,
    /// None when the excerpt is not contained in one verified node interval.
    #[serde(default)]
    pub node_coverage: Option<NodeCoverage>,
    pub column_start: usize,
    pub coordinate_system: String,
    pub text: String,
    pub text_truncated: bool,
    pub score: f64,
    pub confidence: Option<f64>,
    /// A verified exact token query match retained by the hybrid lexical lane.
    pub literal_anchor: bool,
    /// The model relevance score is below the search's `minimum_relevance_score`.
    /// Such a hit is delivered only as a retained literal anchor or, when no
    /// reranked candidate met the floor, as one of at most three flagged
    /// low-confidence fallback candidates; it never passed the floor. Local
    /// regex/lexical hits apply no floor and are never flagged.
    #[serde(default)]
    pub below_threshold: bool,
    pub source_sha256: String,
    pub source_fresh: bool,
    pub citation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub indexed_files: usize,
    /// Documents in the selected search scope, before candidate filtering.
    pub scoped_files: usize,
    pub candidate_files: usize,
    pub verified_files: usize,
    pub routed_documents: usize,
    pub reranked_candidates: usize,
    pub filtered_candidates: usize,
    pub retained_literal_anchors: usize,
    /// Below-floor candidates delivered, flagged `below_threshold`, because no
    /// reranked candidate met the relevance floor (at most three; otherwise 0).
    /// They are also counted in `filtered_candidates`.
    #[serde(default)]
    pub below_threshold_fallback: usize,
    pub stale_files: Vec<String>,
    pub truncated: bool,
    pub semantic_scope_complete: bool,
    /// Model-assisted document routing: `routed` (one Decisions call),
    /// `skipped_single_document` (exactly one routing candidate is selected
    /// without a call; tree candidates are unchanged) or `no_candidates`.
    /// Absent for local regex/lexical searches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_status: Option<String>,
}

/// One failed Jev submission that the bounded transient retry repeated. The
/// failed attempt keeps its HTTP status; its usage stays unknown unless the
/// provider reported it, and is never counted as zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevRetriedAttempt {
    /// `document_routing` or `evidence_reranking`.
    pub stage: String,
    #[serde(flatten)]
    pub attempt: DecisionRetriedAttempt,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub elapsed_ms: u128,
    /// Logical Jev call attempts, including a pre-call observer that may stop
    /// before network I/O, and each bounded transient retry submission. Never a
    /// claim of physical provider requests.
    pub jev_calls_attempted: usize,
    /// Successful validated Jev responses observed by this search.
    pub jev_requests: usize,
    /// Served (provider-returned) model of each validated response.
    pub jev_models: Vec<String>,
    /// Requested Decisions selector of each logical call attempt, in attempt
    /// order. It differs from the served build for canonical mappings and
    /// `~` aliases; both are reported so neither is inferred from the other.
    #[serde(default)]
    pub jev_requested_models: Vec<String>,
    pub jev_usage: Vec<Value>,
    pub jev_candidate_bytes: usize,
    /// Failed submissions repeated by the bounded transient retry, in attempt
    /// order. They are counted in `jev_calls_attempted` but have no `jev_usage`
    /// entry: their usage and cost remain unknown, so accounting is incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jev_retried_attempts: Vec<JevRetriedAttempt>,
}

/// Count each failed submission that a logical Jev call retried as its own
/// attempt with the same requested selector. No usage entry is invented.
fn record_retried_attempts(
    metrics: &mut Metrics,
    stage: &str,
    requested_model: &str,
    attempts: &[DecisionRetriedAttempt],
) {
    for attempt in attempts {
        metrics.jev_calls_attempted += 1;
        metrics
            .jev_requested_models
            .push(requested_model.to_owned());
        metrics.jev_retried_attempts.push(JevRetriedAttempt {
            stage: stage.into(),
            attempt: attempt.clone(),
        });
    }
}

/// Earlier failed submissions carried by a failure after a bounded retry.
fn retried_attempts_of(error: &anyhow::Error) -> &[DecisionRetriedAttempt] {
    error
        .downcast_ref::<DecisionFailure>()
        .map_or(&[], |failure| &failure.diagnostic().prior_attempts)
}

/// The default retrieval client: one bounded retry of transient provider
/// statuses. Callers that supply their own client keep its configuration.
fn default_retrieval_client(client: JevClient) -> JevClient {
    if client.transient_retry().is_some() {
        client
    } else {
        client.with_transient_retry(TransientRetry::standard())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReport {
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
    pub coverage: Coverage,
    pub metrics: Metrics,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: String,
    /// Exact normalized source-relative path from the current index, if scoped.
    pub document: Option<String>,
    pub limit: usize,
    pub context: usize,
    pub case_insensitive: bool,
    pub literal: bool,
    pub max_candidates: usize,
    pub routing_docs: usize,
    pub model: Option<String>,
    pub min_score: f64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: "hybrid".into(),
            document: None,
            limit: 20,
            context: 0,
            case_insensitive: false,
            literal: false,
            max_candidates: 24,
            routing_docs: 32,
            model: None,
            min_score: 0.5,
        }
    }
}

/// Partial accounting for a failed mandatory Jev search stage. Causes are
/// sanitized and bounded; provider bodies, credentials and raw error chains are
/// not exposed. Metrics retain observed successful responses and their bounded
/// provider usage, even when a later attempted call fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevSearchError {
    /// `initialization`, `document_routing`, or `evidence_reranking`.
    pub stage: String,
    pub metrics: Metrics,
    pub coverage: Coverage,
    pub document_scope: Option<String>,
    pub generation: Option<String>,
    pub cause: String,
    /// Always false: failed calls can incur usage that was never returned.
    pub accounting_complete: bool,
}

impl std::fmt::Display for JevSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Jev search failed during {}: {} (accounting incomplete)",
            self.stage, self.cause
        )
    }
}

impl std::error::Error for JevSearchError {}

/// Nonterminal metadata receipt emitted before a logical call attempt and after
/// each validated reply. A caller can persist these before the next await so
/// cancellation does not discard already observed usage. No source text, request
/// payload, credential or raw provider body is included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevSearchProgress {
    pub stage: String,
    /// `before_call` or `after_reply`.
    pub event: String,
    pub metrics: Metrics,
    pub coverage: Coverage,
    pub document_scope: Option<String>,
    pub generation: Option<String>,
    /// Progress is nonterminal; final accounting belongs to the completed run.
    pub accounting_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

pub type JevSearchObserver<'a> = dyn Fn(&JevSearchProgress) -> Result<()> + Send + Sync + 'a;

fn jev_search_error(
    stage: &str,
    error: anyhow::Error,
    metrics: &Metrics,
    coverage: &Coverage,
    scope: Option<&str>,
    generation: Option<&str>,
    started: Instant,
) -> JevSearchError {
    let message = error.to_string();
    let retried = error
        .downcast_ref::<DecisionFailure>()
        .filter(|failure| !failure.diagnostic().prior_attempts.is_empty());
    let cause = if let Some(failure) = retried {
        // Fixed wording plus numeric statuses and validation codes only.
        failure.to_string()
    } else if message == "OPENROUTER_API_KEY is not available" {
        message
    } else if message.contains("API key") {
        "Jev credentials are missing or invalid".into()
    } else if let Some(status) = message
        .strip_prefix("Jev provider returned HTTP ")
        .and_then(|tail| tail.split(';').next())
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
    {
        format!("Jev provider returned HTTP {status}; request was not retried")
    } else if message.contains("timed out") {
        "Jev request timed out; usage may be unknown; no retry or fallback".into()
    } else if message.contains("transport failed") {
        "Jev transport failed; usage may be unknown; no retry or fallback".into()
    } else if message.contains("byte limit") {
        "Jev request or response exceeded its byte limit".into()
    } else if message == "Invalid Jev model identifier" {
        message
    } else {
        "Jev request or response validation failed; no local fallback".into()
    };
    let mut metrics = metrics.clone();
    metrics.elapsed_ms = started.elapsed().as_millis();
    JevSearchError {
        stage: stage.into(),
        metrics,
        coverage: coverage.clone(),
        document_scope: scope.map(str::to_owned),
        generation: generation.map(str::to_owned),
        cause,
        accounting_complete: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn observe_jev(
    observer: Option<&JevSearchObserver<'_>>,
    stage: &str,
    event: &str,
    metrics: &Metrics,
    coverage: &Coverage,
    options: &SearchOptions,
    generation: &str,
    started: Instant,
    reply: Option<&RerankResponse>,
) -> Result<()> {
    let Some(observer) = observer else {
        return Ok(());
    };
    let mut current = metrics.clone();
    current.elapsed_ms = started.elapsed().as_millis();
    let progress = JevSearchProgress {
        stage: stage.into(),
        event: event.into(),
        metrics: current,
        coverage: coverage.clone(),
        document_scope: options.document.clone(),
        generation: Some(generation.into()),
        accounting_complete: false,
        provider_response_id: reply.and_then(|reply| reply.provider_response_id.clone()),
        provider: reply.and_then(|reply| reply.provider.clone()),
    };
    observer(&progress).map_err(|error| {
        let mut failure = jev_search_error(stage, error, metrics, coverage, options.document.as_deref(), Some(generation), started);
        failure.cause = if event == "before_call" {
            "Jev progress observer failed before the call; no provider call was started".into()
        } else {
            "Jev progress observer failed after a validated reply; no further provider call was started".into()
        };
        anyhow::Error::new(failure)
    })
}

fn scoped_documents<'a>(
    documents: &'a [Document],
    requested: Option<&str>,
) -> Result<Vec<&'a Document>> {
    let Some(requested) = requested else {
        return Ok(documents.iter().collect());
    };
    let path = Path::new(requested);
    let normalized: PathBuf = path.components().collect();
    ensure!(
        !requested.is_empty()
            && requested.len() <= 4096
            && !requested.contains('\0')
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && normalized.as_os_str() == path.as_os_str(),
        "document scope must be an exact normalized source-relative path"
    );
    let document = documents
        .iter()
        .find(|document| document.path == requested)
        .with_context(|| format!("document not present in current index: {requested}"))?;
    Ok(vec![document])
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bounded_read(path: &Path, max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        f.metadata()?.len() <= max,
        "file exceeds {max} byte limit: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    f.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "file grew beyond read limit");
    Ok(bytes)
}

fn fill_reader_buffer(path: &Path, buffer: &mut [u8], meter: &ReadMeter) -> Result<usize> {
    use std::io::Read;
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == buffer.len() as u64,
        "file length changed before reader load"
    );
    let mut filled = 0;
    while filled < buffer.len() {
        let count = file.read(&mut buffer[filled..])?;
        meter.record_physical_read(count)?;
        ensure!(count > 0, "file shortened during reader load");
        filled += count;
    }
    let mut extra = [0u8; 1];
    let count = file.read(&mut extra)?;
    meter.record_physical_read(count)?;
    ensure!(count == 0, "file grew during reader load");
    Ok(filled)
}

fn bounded_read_metered(path: &Path, max: u64, meter: &ReadMeter) -> Result<Vec<u8>> {
    let size = fs::metadata(path)?.len();
    ensure!(
        size <= max,
        "file exceeds {max} byte limit: {}",
        path.display()
    );
    let size = usize::try_from(size)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .context("reader allocation failed")?;
    bytes.resize(size, 0);
    fill_reader_buffer(path, &mut bytes, meter)?;
    Ok(bytes)
}

fn digest_metered(bytes: &[u8], meter: &ReadMeter) -> Result<String> {
    let digest = digest(bytes);
    meter.record_physical_hash(bytes.len())?;
    Ok(digest)
}

fn hash_reader_file(path: &Path, max: u64, meter: &ReadMeter) -> Result<(u64, String)> {
    use std::io::Read;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= max,
        "reader file type/size rejected"
    );
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65_536];
    loop {
        let admitted = usize::try_from((max - bytes).saturating_add(1).min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..admitted])?;
        meter.record_physical_read(count)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(bytes <= max, "reader file grew beyond admission");
        hash.update(&buffer[..count]);
        meter.record_physical_hash(count)?;
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

enum ReaderBuffer {
    Cached(ReadPin),
    Uncached(Vec<u8>),
}
struct ReaderText {
    bytes: ReaderBuffer,
    origin: ReadOrigin,
}
impl ReaderText {
    fn into_string(self) -> Result<String> {
        match self.bytes {
            ReaderBuffer::Uncached(bytes) => Ok(String::from_utf8(bytes)?),
            ReaderBuffer::Cached(pin) => {
                let mut text = String::new();
                text.try_reserve_exact(pin.len())
                    .context("reader navigation text allocation failed")?;
                text.push_str(std::str::from_utf8(&pin)?);
                Ok(text)
            }
        }
    }
}
impl std::ops::Deref for ReaderText {
    type Target = str;
    fn deref(&self) -> &str {
        let bytes: &[u8] = match &self.bytes {
            ReaderBuffer::Cached(pin) => pin,
            ReaderBuffer::Uncached(bytes) => bytes,
        };
        std::str::from_utf8(bytes).expect("reader text validated before construction")
    }
}

fn load_reader_text(
    cache: &ReadCache,
    budget: &ReaderCacheBudget,
    meter: &ReadMeter,
    key: &CacheKey,
    path: &Path,
    known_bytes: u64,
    type_limit: u64,
) -> Result<ReaderText> {
    ensure!(
        known_bytes <= type_limit,
        "text exceeds historical reader type limit"
    );
    let known = usize::try_from(known_bytes)?;
    let loaded = cache.get_or_load(key, known, meter, |buffer, meter| {
        fill_reader_buffer(path, buffer, meter)
    });
    let (bytes, origin) = match loaded {
        Ok(read) => (ReaderBuffer::Cached(read.bytes), read.origin),
        Err(error @ (CacheError::Oversized { .. } | CacheError::CapacityRetained))
            if !budget.strict_cache_only =>
        {
            meter.record_fallback()?;
            let _ = error; // Explicit fallback is reported by the shared meter.
            let bytes = bounded_read_metered(path, known_bytes, meter)?;
            ensure!(bytes.len() == known, "uncached text length changed");
            meter.record_uncached_load(bytes.len())?;
            ensure!(
                digest_metered(&bytes, meter)? == key.content_sha256(),
                "uncached indexed text digest mismatch"
            );
            (ReaderBuffer::Uncached(bytes), ReadOrigin::Uncached)
        }
        Err(error) => return Err(error.into()),
    };
    let raw: &[u8] = match &bytes {
        ReaderBuffer::Cached(pin) => pin,
        ReaderBuffer::Uncached(bytes) => bytes,
    };
    std::str::from_utf8(raw).context("indexed text is not UTF-8")?;
    Ok(ReaderText { bytes, origin })
}

fn source_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    ensure!(
        !rel.as_os_str().is_empty() && rel.components().all(|x| matches!(x, Component::Normal(_))),
        "invalid manifest source path"
    );
    let mut full = root.to_path_buf();
    for component in rel.components() {
        full.push(component);
        ensure!(
            !fs::symlink_metadata(&full)?.file_type().is_symlink(),
            "source became a symlink: {relative}"
        );
    }
    ensure!(full.is_file(), "source is not a regular file: {relative}");
    Ok(full)
}

fn denied(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_lowercase();
        matches!(
            name.as_str(),
            ".git" | ".gptgrep" | "node_modules" | "target" | ".local" | "auth.json"
        ) || name == ".env"
            || name.starts_with(".env.")
            || name.ends_with(".pem")
            || name.ends_with(".key")
    })
}

fn no_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "index path cannot be a symlink: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for doc in &manifest.documents {
        ensure!(
            ids.insert(&doc.id) && paths.insert(&doc.path),
            "duplicate index document identity"
        );
        ensure!(
            doc.id == digest(doc.path.as_bytes())[..24],
            "document identity does not match source path"
        );
        ensure!(
            Path::new(&doc.path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "invalid source path"
        );
        if doc.pages.is_empty() {
            ensure!(
                doc.nodes.is_empty() && doc.parser == "plaintext" && doc.text_sha256 == digest(b""),
                "document has no page coverage"
            );
            continue;
        }
        let mut prior_end = 0;
        for (i, page) in doc.pages.iter().enumerate() {
            ensure!(
                page.number as usize == i + 1
                    && page.line_start >= 1
                    && page.line_end >= page.line_start
                    && page.line_start > prior_end,
                "invalid or overlapping page coordinates for {}",
                doc.path
            );
            prior_end = page.line_end;
        }
        let mut nodes = HashMap::new();
        for node in &doc.nodes {
            ensure!(
                !node.id.is_empty() && nodes.insert(node.id.as_str(), node).is_none(),
                "duplicate or empty tree node ID"
            );
            ensure!(
                node.line_start >= 1
                    && node.line_start <= node.line_end
                    && node.line_end <= prior_end
                    && node.page_start >= 1
                    && node.page_start <= node.page_end
                    && node.page_end as usize <= doc.pages.len(),
                "invalid tree node coordinates for {}",
                doc.path
            );
        }
        ensure!(!nodes.is_empty(), "document tree is empty");
        for node in &doc.nodes {
            if let Some(id) = &node.parent_id {
                let parent = nodes.get(id.as_str()).context("unknown tree parent")?;
                ensure!(
                    parent.level < node.level
                        && parent.line_start <= node.line_start
                        && parent.line_end >= node.line_end
                        && parent.page_start <= node.page_start
                        && parent.page_end >= node.page_end,
                    "invalid tree hierarchy for {}",
                    doc.path
                );
            }
        }
    }
    Ok(())
}

/// Construct an immutable generation and atomically publish only after all parses and indexing succeed.
pub async fn index(root: &Path, max_files: usize) -> Result<IndexReport> {
    index_with_options(root, max_files, false).await
}

pub async fn index_with_options(
    root: &Path,
    max_files: usize,
    optimize_merge: bool,
) -> Result<IndexReport> {
    let started = Instant::now();
    ensure!(
        max_files > 0 && max_files <= 1_000_000,
        "max-files must be 1..1000000"
    );
    let root = root
        .canonicalize()
        .context("document root does not exist")?;
    ensure!(root.is_dir(), "document root must be a directory");
    let state = root.join(STATE);
    if state.exists() {
        ensure!(
            !fs::symlink_metadata(&state)?.file_type().is_symlink(),
            "index directory cannot be a symlink"
        );
    }
    no_symlink(&state.join("generations"))?;
    no_symlink(&state.join("write.lock"))?;
    no_symlink(&state.join("CURRENT.json"))?;
    fs::create_dir_all(state.join("generations"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("write.lock"))?;
    lock.try_lock_exclusive()
        .context("another index writer owns this corpus")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let generation = format!("g{}-{}", now.as_nanos(), std::process::id());
    let dir = state.join("generations").join(&generation);
    fs::create_dir(&dir)?;
    fs::create_dir(dir.join("text"))?;
    let mut sources = Vec::new();
    let mut excluded = 0;
    let walk_root = root.clone();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(true)
        .follow_links(false)
        .require_git(false)
        .filter_entry(move |entry| {
            !denied(
                entry
                    .path()
                    .strip_prefix(&walk_root)
                    .unwrap_or(entry.path()),
            )
        })
        .build();
    for item in walker {
        let entry = item.context("document traversal failed")?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            if gptgrep_parse::supports_path(entry.path()) {
                ensure!(
                    sources.len() < max_files,
                    "corpus exceeds --max-files {max_files}; existing index is unchanged"
                );
                sources.push(entry.path().to_path_buf());
            } else {
                excluded += 1;
            }
        }
    }
    sources.sort();
    ensure!(
        !sources.is_empty(),
        "no supported documents found (hidden, ignored and secret files are excluded)"
    );
    let mut documents = Vec::with_capacity(sources.len());
    let mut warnings = Vec::new();
    for path in sources {
        let relative = path
            .strip_prefix(&root)?
            .to_str()
            .context("non-UTF-8 document path is unsupported")?
            .to_owned();
        let safe_path = source_path(&root, &relative)?;
        let bytes = bounded_read(&safe_path, MAX_SOURCE_BYTES)?;
        let source_sha256 = digest(&bytes);
        let mut parsed = gptgrep_parse::parse_path(&safe_path)
            .await
            .with_context(|| format!("parse failed for {relative}; existing index is unchanged"))?;
        if optimize_merge && parsed.parser != "plaintext" {
            parsed = gptgrep_pageindex::optimize_merge(&parsed)?;
        }
        ensure!(
            digest(&bounded_read(
                &source_path(&root, &relative)?,
                MAX_SOURCE_BYTES
            )?) == source_sha256,
            "source changed while parsing: {relative}; retry indexing"
        );
        ensure!(
            parsed.text.len() as u64 <= MAX_TEXT_BYTES,
            "extracted text exceeds document limit: {relative}"
        );
        ensure!(
            parsed.nodes.len() <= 100_000,
            "document tree exceeds 100000 nodes; split the corpus document"
        );
        let id = digest(relative.as_bytes())[..24].to_owned();
        fs::write(dir.join("text").join(format!("{id}.txt")), &parsed.text)?;
        warnings.extend(parsed.warnings.iter().map(|w| format!("{relative}: {w}")));
        documents.push(Document {
            id,
            path: relative,
            source_sha256,
            source_bytes: bytes.len() as u64,
            text_sha256: digest(parsed.text.as_bytes()),
            parser: parsed.parser,
            title: parsed.title,
            pages: parsed
                .pages
                .into_iter()
                .map(|p| PageSpan {
                    number: p.number,
                    line_start: p.line_start,
                    line_end: p.line_end,
                })
                .collect(),
            nodes: parsed.nodes,
            warnings: parsed.warnings,
        });
    }
    let manifest = Manifest {
        schema_version: SCHEMA.into(),
        generation: generation.clone(),
        root: root.clone(),
        created_unix_ms: now.as_millis(),
        documents,
        excluded_files: excluded,
        tree_profile: if optimize_merge {
            "liteparse-layout-v1+flash-merge-v1"
        } else {
            "liteparse-layout-v1"
        }
        .into(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "manifest exceeds supported size; current generation remains unchanged"
    );
    validate_manifest(&manifest)?;
    fs::write(dir.join("manifest.json"), &manifest_bytes)?;
    gptgrep_index::build(&dir.join("text"), &dir.join("trigram"))?;
    let pointer = Pointer {
        schema_version: SCHEMA.into(),
        generation: generation.clone(),
        manifest_sha256: digest(&manifest_bytes),
    };
    let mut temp = tempfile::NamedTempFile::new_in(&state)?;
    serde_json::to_writer_pretty(&mut temp, &pointer)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(state.join("CURRENT.json"))
        .context("failed to publish index pointer")?;
    File::open(&state)?.sync_all()?;
    Ok(IndexReport {
        schema_version: SCHEMA.into(),
        root,
        generation,
        indexed_files: manifest.documents.len(),
        excluded_files: excluded,
        source_bytes: manifest.documents.iter().map(|d| d.source_bytes).sum(),
        elapsed_ms: started.elapsed().as_millis(),
        warnings,
        tree_profile: manifest.tree_profile,
    })
}

/// Exact retained generation identity. Passing a pin never follows CURRENT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationPin {
    pub generation: String,
    pub manifest_sha256: String,
}

/// Reusable reader of a retained, immutable generation.
///
/// Opening resolves CURRENT once. Source freshness and candidate text digests
/// are checked for each operation; retained generations must not be removed or
/// edited while readers use them. No source discovery or publication occurs.
#[derive(Clone)]
pub struct GenerationReader {
    manifest: Arc<Manifest>,
    dir: PathBuf,
    pin: GenerationPin,
    index: Arc<OnceLock<std::result::Result<gptgrep_index::IndexSession, String>>>,
    cache: ReadCache,
    cache_budget: ReaderCacheBudget,
    read_meter: ReadMeter,
}

type Snapshot = GenerationReader;

impl GenerationReader {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_cache_budget(root, &ReaderCacheBudget::default())
    }
    pub fn open_with_cache_budget(root: &Path, budget: &ReaderCacheBudget) -> Result<Self> {
        Self::open_with_read_meter(root, budget, ReadMeter::default())
    }
    fn open_with_read_meter(
        root: &Path,
        budget: &ReaderCacheBudget,
        meter: ReadMeter,
    ) -> Result<Self> {
        budget.validate()?;
        let root = root.canonicalize()?;
        let state = root.join(STATE);
        ensure!(
            !fs::symlink_metadata(&state)
                .context("index missing; run gptgrep index ROOT")?
                .file_type()
                .is_symlink(),
            "index directory cannot be a symlink"
        );
        no_symlink(&state.join("CURRENT.json"))?;
        let pointer: Pointer = serde_json::from_slice(&bounded_read_metered(
            &state.join("CURRENT.json"),
            4096,
            &meter,
        )?)?;
        ensure!(
            pointer.schema_version == SCHEMA,
            "unsupported index schema; reindex required"
        );
        Self::open_pinned_with_read_meter(
            &root,
            &GenerationPin {
                generation: pointer.generation,
                manifest_sha256: pointer.manifest_sha256,
            },
            budget,
            meter,
        )
    }

    /// Open exactly a retained generation without reading CURRENT.
    pub fn open_pinned(root: &Path, pin: &GenerationPin) -> Result<Self> {
        Self::open_pinned_with_cache_budget(root, pin, &ReaderCacheBudget::default())
    }
    pub fn open_pinned_with_cache_budget(
        root: &Path,
        pin: &GenerationPin,
        budget: &ReaderCacheBudget,
    ) -> Result<Self> {
        Self::open_pinned_with_read_meter(root, pin, budget, ReadMeter::default())
    }
    fn open_pinned_with_read_meter(
        root: &Path,
        pin: &GenerationPin,
        budget: &ReaderCacheBudget,
        meter: ReadMeter,
    ) -> Result<Self> {
        budget.validate()?;
        let root = root.canonicalize()?;
        let state = root.join(STATE);
        no_symlink(&state)?;
        ensure!(
            pin.manifest_sha256.len() == 64
                && pin.manifest_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid manifest digest"
        );
        ensure!(
            !pin.generation.is_empty()
                && pin
                    .generation
                    .bytes()
                    .all(|x| x.is_ascii_alphanumeric() || x == b'-'),
            "invalid generation ID"
        );
        let dir = state.join("generations").join(&pin.generation);
        no_symlink(&state.join("generations"))?;
        no_symlink(&dir)?;
        no_symlink(&dir.join("manifest.json"))?;
        no_symlink(&dir.join("trigram"))?;
        let bytes = bounded_read_metered(&dir.join("manifest.json"), MAX_MANIFEST_BYTES, &meter)?;
        ensure!(
            digest_metered(&bytes, &meter)? == pin.manifest_sha256,
            "index manifest digest mismatch; reindex required"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.schema_version == SCHEMA
                && manifest.root == root
                && manifest.generation == pin.generation,
            "index identity mismatch; reindex at current root"
        );
        ensure!(
            manifest
                .documents
                .iter()
                .all(|d| d.id.len() == 24 && d.id.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid document IDs"
        );
        validate_manifest(&manifest)?;
        Ok(Self {
            manifest: Arc::new(manifest),
            dir,
            pin: pin.clone(),
            index: Arc::new(OnceLock::new()),
            cache: ReadCache::new(budget.max_bytes, budget.max_entries)?,
            cache_budget: budget.clone(),
            read_meter: meter,
        })
    }
    pub fn pin(&self) -> GenerationPin {
        self.pin.clone()
    }
    pub fn read_metrics(&self) -> Result<ReaderReadMetrics> {
        Ok(self.read_meter.snapshot()?.into())
    }
    pub fn clear_cache(&self) -> Result<()> {
        Ok(self.cache.clear()?)
    }
    pub fn cache_status(&self) -> Result<Value> {
        let status = self.cache.stats()?;
        Ok(
            json!({"capacity_bytes":status.capacity_bytes,"capacity_entries":status.capacity_entries,
            "retained_bytes":status.retained_bytes,"retained_entries":status.retained_entries,
            "resident_bytes":status.resident_bytes,"resident_entries":status.resident_entries,
            "strict_cache_only":self.cache_budget.strict_cache_only}),
        )
    }

    fn validate(&self) -> Result<()> {
        let state = self.manifest.root.join(STATE);
        no_symlink(&state)?;
        no_symlink(&state.join("generations"))?;
        no_symlink(&self.dir)?;
        no_symlink(&self.dir.join("manifest.json"))?;
        no_symlink(&self.dir.join("trigram"))?;
        ensure!(
            digest_metered(
                &bounded_read_metered(
                    &self.dir.join("manifest.json"),
                    MAX_MANIFEST_BYTES,
                    &self.read_meter
                )?,
                &self.read_meter
            )? == self.pin.manifest_sha256,
            "retained generation manifest changed; reindex required"
        );
        if let Some(Ok(index)) = self.index.get() {
            index.validate()?;
        }
        Ok(())
    }

    fn index_session(&self) -> Result<&gptgrep_index::IndexSession> {
        match self.index.get_or_init(|| {
            gptgrep_index::IndexSession::open(&self.dir.join("trigram"))
                .map_err(|error| format!("{error:#}"))
        }) {
            Ok(index) => Ok(index),
            Err(error) => bail!("retained generation index is invalid: {error}"),
        }
    }

    pub fn status(&self) -> Result<Value> {
        reader_status(self)
    }
    pub fn catalog(&self) -> Result<Value> {
        reader_catalog(self)
    }
    pub fn tree(&self, path: &Path) -> Result<Value> {
        reader_tree(self, path)
    }
    pub fn read_node(&self, node_id: &str, max_bytes: usize) -> Result<Hit> {
        self.read_node_window(node_id, max_bytes, 0)
    }
    pub fn read_node_window(
        &self,
        node_id: &str,
        max_bytes: usize,
        offset_bytes: usize,
    ) -> Result<Hit> {
        reader_read_node_window(self, node_id, max_bytes, offset_bytes)
    }
    pub async fn search(&self, query: &str, options: &SearchOptions) -> Result<SearchReport> {
        search_reader_using_client(self, query, options, None, None).await
    }
    pub async fn search_with_client(
        &self,
        query: &str,
        options: &SearchOptions,
        client: &JevClient,
    ) -> Result<SearchReport> {
        search_reader_using_client(self, query, options, Some(client), None).await
    }
    pub async fn search_with_client_and_observer(
        &self,
        query: &str,
        options: &SearchOptions,
        client: &JevClient,
        observer: &JevSearchObserver<'_>,
    ) -> Result<SearchReport> {
        search_reader_using_client(self, query, options, Some(client), Some(observer)).await
    }

    fn text(&self, doc: &Document) -> Result<String> {
        self.text_cached(doc)?.into_string()
    }
    fn text_cached(&self, doc: &Document) -> Result<ReaderText> {
        no_symlink(&self.dir.join("text"))?;
        no_symlink(&self.dir.join("text").join(format!("{}.txt", doc.id)))?;
        let path = self.dir.join("text").join(format!("{}.txt", doc.id));
        let size = fs::metadata(&path)?.len();
        let key = CacheKey::new(&self.pin.generation, &doc.id, &doc.text_sha256)?;
        let text = load_reader_text(
            &self.cache,
            &self.cache_budget,
            &self.read_meter,
            &key,
            &path,
            size,
            MAX_TEXT_BYTES,
        )?;
        if text.origin == ReadOrigin::Cache {
            ensure!(
                hash_reader_file(&path, MAX_TEXT_BYTES, &self.read_meter)?
                    == (size, doc.text_sha256.clone()),
                "indexed text digest mismatch for {}; reindex",
                doc.path
            );
        }
        ensure!(
            (text.is_empty() && doc.pages.is_empty())
                || doc
                    .pages
                    .last()
                    .is_some_and(|p| p.line_end == text.lines().count().max(1)),
            "text and tree line coverage mismatch"
        );
        Ok(text)
    }
    fn fresh(&self, doc: &Document) -> bool {
        source_path(&self.manifest.root, &doc.path)
            .and_then(|p| hash_reader_file(&p, MAX_SOURCE_BYTES, &self.read_meter))
            .is_ok_and(|(_, digest)| digest == doc.source_sha256)
    }
}

pub fn status(root: &Path) -> Result<Value> {
    GenerationReader::open(root)?.status()
}

fn reader_status(snapshot: &GenerationReader) -> Result<Value> {
    snapshot.validate()?;
    let stale: Vec<_> = snapshot
        .manifest
        .documents
        .iter()
        .filter(|d| !snapshot.fresh(d))
        .map(|d| d.path.clone())
        .collect();
    Ok(
        json!({"schema_version":SCHEMA,"root":snapshot.manifest.root,"generation":snapshot.manifest.generation,
        "indexed_files":snapshot.manifest.documents.len(),"excluded_files":snapshot.manifest.excluded_files,
        "stale_files":stale,"source_fresh":stale.is_empty(),
        "scope":"existing indexed documents; rerun index to discover additions", "created_unix_ms":snapshot.manifest.created_unix_ms}),
    )
}

pub fn tree(root: &Path, path: &Path) -> Result<Value> {
    GenerationReader::open(root)?.tree(path)
}

fn reader_tree(snapshot: &GenerationReader, path: &Path) -> Result<Value> {
    snapshot.validate()?;
    let input = if path.is_absolute() {
        path.strip_prefix(&snapshot.manifest.root)?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let doc = snapshot
        .manifest
        .documents
        .iter()
        .find(|d| Path::new(&d.path) == input)
        .context("document not present in current index")?;
    ensure!(
        snapshot.fresh(doc),
        "source changed or disappeared; reindex before reading tree"
    );
    Ok(
        json!({"schema_version":SCHEMA,"generation":snapshot.manifest.generation,"source_fresh":true,"document":doc}),
    )
}

pub fn read_node(root: &Path, node_id: &str, max_bytes: usize) -> Result<Hit> {
    read_node_window(root, node_id, max_bytes, 0)
}

/// Read a bounded, exact UTF-8 window of a node's canonical text interval.
///
/// Offsets are bytes relative to the node's first source line. The interval
/// includes its line terminators, so successive windows concatenate without
/// losing LF or CRLF bytes. An offset at the interval's end returns an empty
/// EOF window; offsets past it or inside a UTF-8 scalar are errors. A budget
/// too small for the next scalar also errors, preventing stalled continuation.
pub fn read_node_window(
    root: &Path,
    node_id: &str,
    max_bytes: usize,
    offset_bytes: usize,
) -> Result<Hit> {
    ensure!(
        (1..=65536).contains(&max_bytes),
        "max-bytes must be 1..65536"
    );
    node_id
        .split_once(':')
        .context("node ID must be document_id:node_id from search/tree")?;
    GenerationReader::open(root)?.read_node_window(node_id, max_bytes, offset_bytes)
}

fn reader_read_node_window(
    snapshot: &GenerationReader,
    node_id: &str,
    max_bytes: usize,
    offset_bytes: usize,
) -> Result<Hit> {
    let (hit, origin) =
        reader_node_window_unmetered_delivery(snapshot, node_id, max_bytes, offset_bytes)?;
    snapshot
        .read_meter
        .record_delivery(hit.text.len(), origin)?;
    Ok(hit)
}

fn reader_node_window_unmetered_delivery(
    snapshot: &GenerationReader,
    node_id: &str,
    max_bytes: usize,
    offset_bytes: usize,
) -> Result<(Hit, ReadOrigin)> {
    ensure!(
        (1..=65536).contains(&max_bytes),
        "max-bytes must be 1..65536"
    );
    let (doc_id, local_id) = node_id
        .split_once(':')
        .context("node ID must be document_id:node_id from search/tree")?;
    snapshot.validate()?;
    let doc = snapshot
        .manifest
        .documents
        .iter()
        .find(|d| d.id == doc_id)
        .context("unknown document ID")?;
    let node = doc
        .nodes
        .iter()
        .find(|n| n.id == local_id)
        .context("unknown tree node ID")?;
    ensure!(
        snapshot.fresh(doc),
        "source changed or disappeared; reindex before reading evidence"
    );
    let view = TextView::new(snapshot.text_cached(doc)?);
    let (node_start, node_end) = view
        .node_bytes(node)
        .context("tree node range lies outside canonical text")?;
    let node_length = node_end - node_start;
    ensure!(
        offset_bytes <= node_length,
        "offset-bytes {offset_bytes} exceeds node length {node_length}"
    );
    let byte_start = node_start + offset_bytes;
    ensure!(
        view.text.is_char_boundary(byte_start),
        "offset-bytes {offset_bytes} is not a UTF-8 boundary"
    );
    let remaining = &view.text[byte_start..node_end];
    let (bounded, _) = truncate(remaining, max_bytes);
    if !remaining.is_empty() && bounded.is_empty() {
        bail!(
            "max-bytes {max_bytes} cannot hold the next UTF-8 character (requires {} bytes)",
            remaining.chars().next().expect("nonempty text").len_utf8()
        );
    }
    let byte_end = byte_start + bounded.len();
    let result = hit_from_bytes(
        doc,
        node,
        &view,
        ByteWindow {
            start: byte_start,
            end: byte_end,
            truncated: byte_start > node_start || byte_end < node_end,
        },
        None,
        1.0,
    );
    ensure!(
        snapshot.fresh(doc),
        "source changed or disappeared; reindex before reading evidence"
    );
    snapshot.validate()?;
    Ok((result, view.text.origin()))
}

fn truncate(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

enum TextStorage {
    Owned(String),
    Reader(ReaderText),
}
impl From<String> for TextStorage {
    fn from(text: String) -> Self {
        Self::Owned(text)
    }
}
impl From<ReaderText> for TextStorage {
    fn from(text: ReaderText) -> Self {
        Self::Reader(text)
    }
}
impl std::ops::Deref for TextStorage {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::Owned(text) => text,
            Self::Reader(text) => text,
        }
    }
}
impl TextStorage {
    fn origin(&self) -> ReadOrigin {
        match self {
            Self::Owned(_) => ReadOrigin::Loaded,
            Self::Reader(text) => text.origin,
        }
    }
}
struct TextView {
    text: TextStorage,
    lines: Vec<(usize, usize)>,
}

impl TextView {
    fn new(text: impl Into<TextStorage>) -> Self {
        let text = text.into();
        let mut offset = 0;
        let mut lines = Vec::new();
        for line in text.split_inclusive('\n') {
            let content = if let Some(without_lf) = line.strip_suffix('\n') {
                without_lf.strip_suffix('\r').unwrap_or(without_lf)
            } else {
                line
            };
            let length = content.len();
            lines.push((offset, offset + length));
            offset += line.len();
        }
        if lines.is_empty() {
            lines.push((0, 0));
        }
        Self { text, lines }
    }

    /// Full line intervals include terminators, including the last newline.
    fn node_bytes(&self, node: &TreeNode) -> Option<(usize, usize)> {
        if node.line_end < node.line_start {
            return None;
        }
        let start = self.lines.get(node.line_start.checked_sub(1)?)?.0;
        self.lines.get(node.line_end.checked_sub(1)?)?;
        let end = self
            .lines
            .get(node.line_end)
            .map_or(self.text.len(), |line| line.0);
        Some((start, end))
    }
}

struct ByteWindow {
    start: usize,
    end: usize,
    truncated: bool,
}

#[allow(clippy::too_many_arguments)]
fn hit(
    doc: &Document,
    node: &TreeNode,
    view: &TextView,
    start: usize,
    end: usize,
    matched: Option<(usize, usize)>,
    score: f64,
    max_bytes: usize,
) -> Hit {
    let text = &view.text;
    let lines = &view.lines;
    let start = start.max(1).min(lines.len());
    let end = end.max(start).min(lines.len());
    let mut byte_start = lines[start - 1].0;
    let initial_start = byte_start;
    let wanted_end = lines[end - 1].1;
    if let Some((line, column)) = matched {
        let anchor = lines[line - 1].0 + column;
        if anchor >= byte_start.saturating_add(max_bytes.saturating_sub(64)) {
            byte_start = anchor.saturating_sub(max_bytes / 4).max(lines[line - 1].0);
            while !text.is_char_boundary(byte_start) {
                byte_start += 1;
            }
        }
    }
    let (bounded, cut_end) = truncate(&text[byte_start..wanted_end], max_bytes);
    let mut bounded = bounded;
    while let Some(without_lf) = bounded.strip_suffix('\n') {
        bounded = without_lf.strip_suffix('\r').unwrap_or(without_lf);
    }
    let byte_end = byte_start + bounded.len();
    hit_from_bytes(
        doc,
        node,
        view,
        ByteWindow {
            start: byte_start,
            end: byte_end,
            truncated: cut_end || byte_start > initial_start || byte_end < wanted_end,
        },
        matched,
        score,
    )
}

/// Shared citation/coordinate construction for search snippets and exact reads.
/// Search retains its existing -C context and trailing-newline presentation;
/// windows pass their exact unmodified byte interval.
fn hit_from_bytes(
    doc: &Document,
    node: &TreeNode,
    view: &TextView,
    window: ByteWindow,
    matched: Option<(usize, usize)>,
    score: f64,
) -> Hit {
    let text = &view.text;
    let lines = &view.lines;
    let byte_start = window.start;
    let byte_end = window.end;
    let node_bounds = view.node_bytes(node);
    let start = lines
        .partition_point(|(offset, _)| *offset <= byte_start)
        .max(1);
    let end = lines
        .partition_point(|(offset, _)| *offset < byte_end)
        .max(start);
    // A zero-length node EOF may also be the next section's first byte. Its
    // citation remains on this node's final line, not the following section.
    let (start, end) = if byte_start == byte_end
        && node_bounds.is_some_and(|(_, node_end)| byte_end == node_end)
    {
        (node.line_end, node.line_end)
    } else {
        (start, end)
    };
    let contained = node_bounds.filter(|(start, end)| *start <= byte_start && byte_end <= *end);
    let node_offset = contained.map(|(start, _)| byte_start - start);
    let next_offset =
        contained.and_then(|(start, end)| (byte_end < end).then_some(byte_end - start));
    let node_coverage = contained.map(|(start, end)| NodeCoverage {
        complete: byte_start == start && byte_end == end,
        unread_before_bytes: byte_start - start,
        unread_after_bytes: end - byte_end,
    });
    let page_start = doc
        .pages
        .iter()
        .find(|p| p.line_start <= start && p.line_end >= start)
        .map_or(node.page_start, |p| p.number);
    let page_end = doc
        .pages
        .iter()
        .find(|p| p.line_start <= end && p.line_end >= end)
        .map_or(node.page_end, |p| p.number);
    let plaintext = doc.parser == "plaintext";
    let citation = if plaintext {
        format!("{}:L{}-L{}", doc.path, start, end)
    } else {
        format!("{}:p{}-p{}", doc.path, page_start, page_end)
    };
    Hit {
        path: doc.path.clone(),
        node_id: format!("{}:{}", doc.id, node.id),
        title: node.title.clone(),
        line_start: start,
        line_end: end,
        page_start,
        page_end,
        match_line: matched.map(|m| m.0),
        match_column: matched.map(|m| m.1 + 1),
        byte_start,
        byte_end,
        node_offset,
        next_offset,
        node_coverage,
        column_start: byte_start - lines[start - 1].0 + 1,
        coordinate_system: if plaintext {
            "source_lines"
        } else {
            "extracted_lines_and_source_pages"
        }
        .into(),
        text: text[byte_start..byte_end].into(),
        text_truncated: window.truncated,
        score,
        confidence: None,
        literal_anchor: false,
        below_threshold: false,
        source_sha256: doc.source_sha256.clone(),
        source_fresh: true,
        citation,
    }
}

fn node_at(doc: &Document, line: usize) -> Option<&TreeNode> {
    doc.nodes
        .iter()
        .filter(|n| n.line_start <= line && n.line_end >= line)
        .max_by_key(|n| {
            (
                n.level,
                std::cmp::Reverse(n.line_end.saturating_sub(n.line_start)),
            )
        })
}

/// Maximum lexical query terms used by one lexical/hybrid search.
const MAX_LEXICAL_TERMS: usize = 16;
/// Candidate terms ranked by scoped document frequency, in first-occurrence
/// order; later candidates are not considered. Bounds the frequency pass.
const MAX_RANKED_LEXICAL_TERMS: usize = 64;
/// A small fixed list of English function words. Each has at least three
/// letters; shorter ASCII-letter tokens are dropped by length instead.
const LEXICAL_STOPWORDS: &[&str] = &[
    "about", "above", "across", "after", "against", "all", "also", "although", "among", "and",
    "any", "are", "because", "been", "before", "being", "below", "between", "both", "but", "can",
    "could", "did", "does", "doing", "done", "during", "each", "either", "else", "every", "for",
    "from", "had", "has", "have", "having", "her", "here", "hers", "him", "his", "how", "into",
    "its", "itself", "may", "might", "mine", "must", "neither", "nor", "not", "off", "onto", "our",
    "ours", "out", "over", "per", "shall", "she", "should", "some", "such", "than", "that", "the",
    "their", "theirs", "them", "then", "there", "these", "they", "this", "those", "though",
    "through", "too", "toward", "towards", "under", "upon", "very", "via", "was", "were", "what",
    "when", "where", "whether", "which", "while", "who", "whom", "whose", "why", "will", "with",
    "within", "without", "would", "yet", "you", "your", "yours",
];

/// Lowercase alphanumeric/underscore tokens, deduplicated in first-occurrence order.
fn query_tokens(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

/// Candidate lexical terms before the 16-term cap. Fixed English function
/// words and ASCII-letter tokens shorter than three characters are dropped;
/// tokens containing a digit and non-ASCII tokens (for example CJK words) are
/// kept. A query made only of dropped tokens keeps all of its tokens.
fn candidate_terms(query: &str) -> Vec<String> {
    let tokens = query_tokens(query);
    let kept: Vec<String> = tokens
        .iter()
        .filter(|token| {
            token.chars().any(|c| c.is_ascii_digit())
                || !token.is_ascii()
                || (token.chars().count() >= 3 && !LEXICAL_STOPWORDS.contains(&token.as_str()))
        })
        .cloned()
        .collect();
    if kept.is_empty() { tokens } else { kept }
}

/// Order candidate terms for the cap: lower document frequency in the scoped
/// index first, terms absent from the scope last, ties by first occurrence.
/// `frequencies` is parallel to `terms`. Deterministic for a fixed snapshot.
fn rank_terms(terms: Vec<String>, frequencies: &[usize]) -> Vec<String> {
    let mut ranked: Vec<(usize, String)> = terms.into_iter().enumerate().collect();
    ranked.sort_by_key(|(index, _)| {
        let frequency = frequencies[*index];
        (frequency == 0, frequency, *index)
    });
    ranked
        .into_iter()
        .take(MAX_LEXICAL_TERMS)
        .map(|(_, term)| term)
        .collect()
}

/// Seed lexical terms for one search scope. At most 16 terms are kept; when
/// more candidates remain, the rarest in the scoped index are kept.
fn lexical_terms(
    snapshot: &Snapshot,
    query: &str,
    indexed_document: Option<&Path>,
) -> Result<Vec<String>> {
    let mut terms = candidate_terms(query);
    if terms.len() <= MAX_LEXICAL_TERMS {
        return Ok(terms);
    }
    terms.truncate(MAX_RANKED_LEXICAL_TERMS);
    let frequencies = snapshot
        .index_session()?
        .document_frequencies(&terms, indexed_document)?;
    Ok(rank_terms(terms, &frequencies))
}

/// Below-floor candidates delivered when no reranked candidate meets the floor.
const BELOW_THRESHOLD_FALLBACK: usize = 3;

fn lexical_score(text: &str, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    terms.iter().filter(|t| lower.contains(t.as_str())).count() as f64 / terms.len() as f64
}

fn literal_anchor(query: &str, line: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() || query.chars().any(|c| !c.is_alphanumeric() && c != '_') {
        return false;
    }
    line.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word.to_lowercase() == query)
}

pub async fn search(root: &Path, query: &str, options: &SearchOptions) -> Result<SearchReport> {
    search_using_client(root, query, options, None, None).await
}

/// Run the same search engine with an explicitly supplied Jev transport. This is
/// useful for embedding and loopback tests; it never disables Jev in hybrid or
/// semantic mode. Explicit regex/lexical modes do not invoke the supplied client.
/// The supplied client's model/endpoint configuration is authoritative.
pub async fn search_with_client(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    client: &JevClient,
) -> Result<SearchReport> {
    search_using_client(root, query, options, Some(client), None).await
}

/// Search with bounded progress receipts. A failed observer stops the workflow
/// before the next provider call and returns a JevSearchError with known usage.
pub async fn search_with_client_and_observer(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    client: &JevClient,
    observer: &JevSearchObserver<'_>,
) -> Result<SearchReport> {
    search_using_client(root, query, options, Some(client), Some(observer)).await
}

fn validate_search_options(query: &str, options: &SearchOptions) -> Result<()> {
    ensure!(
        matches!(
            options.mode.as_str(),
            "regex" | "lexical" | "hybrid" | "semantic"
        ),
        "unsupported search mode"
    );
    ensure!(
        !query.is_empty() && query.len() <= 8192,
        "query must contain 1..8192 bytes"
    );
    ensure!((1..=1000).contains(&options.limit), "limit must be 1..1000");
    ensure!(options.context <= 100, "context must be <=100");
    ensure!(
        (1..=24).contains(&options.max_candidates),
        "max-candidates must be 1..24"
    );
    ensure!(
        (1..=32).contains(&options.routing_docs),
        "routing-docs must be 1..32"
    );
    ensure!(
        options.min_score.is_finite() && (0.0..=1.0).contains(&options.min_score),
        "min-score must be a finite value in 0..1"
    );
    Ok(())
}

struct CandidateBatch {
    hits: Vec<Hit>,
    coverage: Coverage,
    warnings: Vec<String>,
}

async fn search_using_client(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    supplied_client: Option<&JevClient>,
    observer: Option<&JevSearchObserver<'_>>,
) -> Result<SearchReport> {
    validate_search_options(query, options)?;
    let reader = GenerationReader::open(root)?;
    search_reader_using_client(&reader, query, options, supplied_client, observer).await
}

async fn search_reader_using_client(
    snapshot: &GenerationReader,
    query: &str,
    options: &SearchOptions,
    supplied_client: Option<&JevClient>,
    observer: Option<&JevSearchObserver<'_>>,
) -> Result<SearchReport> {
    let started = Instant::now();
    validate_search_options(query, options)?;
    let model_assisted = matches!(options.mode.as_str(), "hybrid" | "semantic");
    snapshot.validate()?;
    let scoped = scoped_documents(&snapshot.manifest.documents, options.document.as_deref())?;
    let coverage = Coverage {
        indexed_files: snapshot.manifest.documents.len(),
        scoped_files: scoped.len(),
        ..Default::default()
    };
    let mut metrics = Metrics::default();
    let client = if model_assisted {
        Some(match supplied_client {
            Some(client) => client.clone(),
            None => JevClient::from_env(options.model.as_deref())
                .map(default_retrieval_client)
                .map_err(|error| {
                    jev_search_error(
                        "initialization",
                        error,
                        &metrics,
                        &coverage,
                        options.document.as_deref(),
                        Some(&snapshot.manifest.generation),
                        started,
                    )
                })?,
        })
    } else {
        None
    };
    let mut batch = collect_candidate_view(
        snapshot,
        query,
        options,
        client.as_ref(),
        observer,
        &mut metrics,
        started,
    )
    .await?;
    if let Some(client) = client.as_ref() {
        score_candidates(
            snapshot,
            query,
            options,
            client,
            observer,
            &mut batch,
            &mut metrics,
            started,
        )
        .await?;
    }
    finish_candidate_batch(snapshot, options, &mut batch);
    let CandidateBatch {
        hits,
        coverage,
        warnings,
    } = batch;
    snapshot.validate()?;
    metrics.elapsed_ms = started.elapsed().as_millis();
    Ok(SearchReport {
        schema_version: SCHEMA.into(),
        query: query.into(),
        mode: options.mode.clone(),
        document_scope: options.document.clone(),
        root: snapshot.manifest.root.clone(),
        generation: snapshot.manifest.generation.clone(),
        index_used: true,
        source_fresh: if hits.is_empty() { None } else { Some(true) },
        minimum_relevance_score: model_assisted.then_some(options.min_score),
        hits,
        coverage,
        metrics,
        warnings,
    })
}

/// Construct bounded lexical/tree candidates without evidence reranking or the
/// delivery limit. Both ordinary and planned searches use this exact stage.
#[allow(clippy::too_many_arguments)]
async fn collect_candidate_view(
    snapshot: &Snapshot,
    query: &str,
    options: &SearchOptions,
    client: Option<&JevClient>,
    observer: Option<&JevSearchObserver<'_>>,
    metrics: &mut Metrics,
    started: Instant,
) -> Result<CandidateBatch> {
    let model_assisted = client.is_some();
    let scoped = scoped_documents(&snapshot.manifest.documents, options.document.as_deref())?;
    let docs: HashMap<_, _> = scoped.iter().copied().map(|d| (d.id.as_str(), d)).collect();
    let mut coverage = Coverage {
        indexed_files: snapshot.manifest.documents.len(),
        scoped_files: scoped.len(),
        ..Default::default()
    };
    let indexed_document = options
        .document
        .as_ref()
        .map(|_| PathBuf::from(format!("{}.txt", scoped[0].id)));
    let mut warnings = Vec::new();
    let mut freshness = HashMap::new();
    let mut texts = HashMap::new();
    let mut hits = Vec::new();
    let terms = if options.mode == "regex" || options.mode == "semantic" {
        Vec::new()
    } else {
        lexical_terms(snapshot, query, indexed_document.as_deref())?
    };
    if options.mode != "semantic" {
        let pattern = if options.mode == "regex" {
            query.to_owned()
        } else {
            ensure!(
                !terms.is_empty(),
                "lexical/hybrid query must contain a word"
            );
            terms
                .iter()
                .map(|x| regex::escape(x))
                .collect::<Vec<_>>()
                .join("|")
        };
        let verification_pattern = if options.literal && options.mode == "regex" {
            regex::escape(&pattern)
        } else {
            pattern.clone()
        };
        let verifier = regex::RegexBuilder::new(&verification_pattern)
            .case_insensitive(options.case_insensitive || options.mode != "regex")
            .build()?;
        let matches = snapshot
            .index_session()?
            .search_with_document_and_predicate(
                &pattern,
                options.case_insensitive || options.mode != "regex",
                options.literal && options.mode == "regex",
                if options.mode == "regex" {
                    options.limit + 1
                } else {
                    10_000
                },
                indexed_document.as_deref(),
                |relative| {
                    let id = relative
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .context("invalid trigram document path")?;
                    let document = docs
                        .get(id)
                        .context("trigram document missing from manifest")?;
                    Ok(*freshness
                        .entry(document.id.clone())
                        .or_insert_with(|| snapshot.fresh(document)))
                },
            )?;
        coverage.candidate_files = matches.candidate_files;
        coverage.verified_files = matches.verified_files;
        coverage.truncated = matches.truncated;
        let mut node_hits: HashMap<String, usize> = HashMap::new();
        for m in matches.hits {
            let id = m
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .context("invalid trigram document path")?;
            let doc = *docs
                .get(id)
                .context("trigram document missing from manifest")?;
            let fresh = *freshness
                .entry(doc.id.clone())
                .or_insert_with(|| snapshot.fresh(doc));
            if !fresh {
                continue;
            }
            if options.mode == "regex" && hits.len() >= options.limit {
                coverage.truncated = true;
                break;
            }
            if !texts.contains_key(&doc.id) {
                texts.insert(doc.id.clone(), TextView::new(snapshot.text_cached(doc)?));
            }
            let text = &texts[&doc.id];
            let Some(node) = node_at(doc, m.line_number) else {
                bail!("tree does not cover indexed match");
            };
            let (start, end) = if options.mode == "regex" {
                (
                    m.line_number.saturating_sub(options.context).max(1),
                    m.line_number.saturating_add(options.context),
                )
            } else {
                (
                    m.line_number.saturating_sub(4).max(node.line_start),
                    m.line_number.saturating_add(12).min(node.line_end),
                )
            };
            let mut h = hit(
                doc,
                node,
                text,
                start,
                end,
                Some((
                    m.line_number,
                    verifier
                        .find(&m.line)
                        .context("trigram verification mismatch")?
                        .start(),
                )),
                1.0,
                if model_assisted { 1400 } else { 4096 },
            );
            if options.mode != "regex" {
                h.score = lexical_score(&h.text, &terms);
                h.literal_anchor = literal_anchor(query, &m.line);
                // Compare bounded windows before collapsing a node. An early
                // incidental match must not hide a later, stronger query match.
                if let Some(&position) = node_hits.get(&h.node_id) {
                    let previous: &mut Hit = &mut hits[position];
                    if h.score.total_cmp(&previous.score).is_gt() {
                        *previous = h;
                    }
                    continue; // Equal windows retain the existing source-order tie.
                }
                node_hits.insert(h.node_id.clone(), hits.len());
            }
            hits.push(h);
        }
    }
    if options.mode != "regex" {
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.path.cmp(&b.path))
                .then(a.line_start.cmp(&b.line_start))
        });
    }
    if let Some(client) = client {
        // Document routing is a semantic lane independent of lexical matches. Coverage is explicit.
        let mut routing = Vec::new();
        for doc in scoped.iter().copied().take(options.routing_docs) {
            let fresh = *freshness
                .entry(doc.id.clone())
                .or_insert_with(|| snapshot.fresh(doc));
            if !fresh {
                continue;
            }
            if !texts.contains_key(&doc.id) {
                texts.insert(doc.id.clone(), TextView::new(snapshot.text_cached(doc)?));
            }
            let headings = doc
                .nodes
                .iter()
                .take(24)
                .flat_map(|n| {
                    std::iter::once(n.title.as_str()).chain(n.key_items.iter().map(String::as_str))
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let description = format!(
                "Path: {}\nTitle: {}\nSections: {}\nOpening: {}",
                doc.path,
                doc.title,
                truncate(&headings, 400).0,
                truncate(&texts[&doc.id].text, 300).0
            );
            routing.push(Candidate {
                id: doc.id.clone(),
                text: truncate(&description, 900).0.to_owned(),
            });
        }
        coverage.stale_files = known_stale_paths(&freshness, &docs);
        coverage.routed_documents = routing.len();
        coverage.semantic_scope_complete = routing.len() == scoped.len();
        if !coverage.semantic_scope_complete {
            warnings.push("Semantic routing covers a bounded document prefix; increase --routing-docs up to 32, select --document, or search narrower roots. Lexical retrieval uses the same explicit document scope.".into());
        }
        let skip_single = routing.len() == 1 && skip_single_document_routing();
        coverage.routing_status = Some(
            match routing.len() {
                0 => "no_candidates",
                _ if skip_single => "skipped_single_document",
                _ => "routed",
            }
            .into(),
        );
        if skip_single {
            // Routing selects the top-ranked documents regardless of score, so
            // one candidate is always selected. Its tree candidates, and thus
            // the rerank request, are identical without a Decisions call.
            let only = routing.pop().expect("one routing candidate");
            hits.truncate(options.max_candidates / 2);
            let document = docs
                .get(only.id.as_str())
                .copied()
                .context("routing document missing from scope")?;
            add_tree_candidates(&mut hits, &[document], &texts, options, &mut coverage);
        } else if !routing.is_empty() {
            metrics.jev_candidate_bytes += routing.iter().map(|c| c.text.len()).sum::<usize>();
            metrics.jev_calls_attempted += 1;
            metrics
                .jev_requested_models
                .push(client.requested_model().to_owned());
            observe_jev(
                observer,
                "document_routing",
                "before_call",
                metrics,
                &coverage,
                options,
                &snapshot.manifest.generation,
                started,
                None,
            )?;
            let ranked = match client.rerank(query, &routing).await {
                Ok(ranked) => ranked,
                Err(error) => {
                    record_retried_attempts(
                        metrics,
                        "document_routing",
                        client.requested_model(),
                        retried_attempts_of(&error),
                    );
                    return Err(jev_search_error(
                        "document_routing",
                        error,
                        metrics,
                        &coverage,
                        options.document.as_deref(),
                        Some(&snapshot.manifest.generation),
                        started,
                    )
                    .into());
                }
            };
            record_retried_attempts(
                metrics,
                "document_routing",
                client.requested_model(),
                &ranked.retried_attempts,
            );
            metrics.jev_requests += 1;
            metrics.jev_models.push(ranked.model.clone());
            metrics.jev_usage.push(ranked.usage.clone());
            observe_jev(
                observer,
                "document_routing",
                "after_reply",
                metrics,
                &coverage,
                options,
                &snapshot.manifest.generation,
                started,
                Some(&ranked),
            )?;
            let mut ranks = ranked.rankings;
            ranks.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
            // Reserve half the final budget for tree candidates, protecting no-keyword recall.
            hits.truncate(options.max_candidates / 2);
            let selected: Vec<_> = ranks
                .iter()
                .take(8)
                .filter_map(|r| docs.get(r.id.as_str()).copied())
                .collect();
            add_tree_candidates(&mut hits, &selected, &texts, options, &mut coverage);
        }
        coverage.truncated |= hits.len() > options.max_candidates;
        hits.truncate(options.max_candidates);
    }
    coverage.stale_files = known_stale_paths(&freshness, &docs);
    if !coverage.stale_files.is_empty() {
        warnings.push("Stale/deleted source documents were excluded; rerun index before relying on a negative result.".into());
    }
    Ok(CandidateBatch {
        hits,
        coverage,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
async fn score_candidates(
    snapshot: &Snapshot,
    query: &str,
    options: &SearchOptions,
    client: &JevClient,
    observer: Option<&JevSearchObserver<'_>>,
    batch: &mut CandidateBatch,
    metrics: &mut Metrics,
    started: Instant,
) -> Result<()> {
    let CandidateBatch {
        hits,
        coverage,
        warnings,
    } = batch;
    if !hits.is_empty() {
        let candidates: Vec<_> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| Candidate {
                id: format!("c{i}"),
                text: format!(
                    "Document: {}\nSection: {}\nEvidence: {}",
                    h.path, h.title, h.text
                ),
            })
            .collect();
        metrics.jev_candidate_bytes += candidates.iter().map(|c| c.text.len()).sum::<usize>();
        metrics.jev_calls_attempted += 1;
        metrics
            .jev_requested_models
            .push(client.requested_model().to_owned());
        observe_jev(
            observer,
            "evidence_reranking",
            "before_call",
            metrics,
            coverage,
            options,
            &snapshot.manifest.generation,
            started,
            None,
        )?;
        let ranked = match client.rerank(query, &candidates).await {
            Ok(ranked) => ranked,
            Err(error) => {
                record_retried_attempts(
                    metrics,
                    "evidence_reranking",
                    client.requested_model(),
                    retried_attempts_of(&error),
                );
                return Err(jev_search_error(
                    "evidence_reranking",
                    error,
                    metrics,
                    coverage,
                    options.document.as_deref(),
                    Some(&snapshot.manifest.generation),
                    started,
                )
                .into());
            }
        };
        record_retried_attempts(
            metrics,
            "evidence_reranking",
            client.requested_model(),
            &ranked.retried_attempts,
        );
        metrics.jev_requests += 1;
        metrics.jev_models.push(ranked.model.clone());
        metrics.jev_usage.push(ranked.usage.clone());
        coverage.reranked_candidates = candidates.len();
        observe_jev(
            observer,
            "evidence_reranking",
            "after_reply",
            metrics,
            coverage,
            options,
            &snapshot.manifest.generation,
            started,
            Some(&ranked),
        )?;
        let scores: HashMap<_, _> = ranked
            .rankings
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
        for (i, h) in hits.iter_mut().enumerate() {
            let rank = scores.get(&format!("c{i}")).ok_or_else(|| {
                jev_search_error(
                    "evidence_reranking",
                    anyhow::anyhow!("Jev omitted a candidate"),
                    metrics,
                    coverage,
                    options.document.as_deref(),
                    Some(&snapshot.manifest.generation),
                    started,
                )
            })?;
            h.score = rank.score;
            h.confidence = rank.confidence;
        }
        let before_filter = hits.len();
        for h in hits.iter_mut() {
            h.below_threshold = h.score < options.min_score;
        }
        coverage.retained_literal_anchors = hits
            .iter()
            .filter(|h| h.literal_anchor && h.below_threshold)
            .count();
        let passing = hits
            .iter()
            .filter(|h| h.literal_anchor || !h.below_threshold)
            .count();
        coverage.filtered_candidates = before_filter - passing;
        if passing == 0 {
            // Never deliver an empty seed when candidates exist: keep the best
            // few by score, each flagged as below the floor. They never count
            // as passing, and the reader is told they are low confidence.
            hits.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then(a.path.cmp(&b.path))
                    .then(a.line_start.cmp(&b.line_start))
            });
            hits.truncate(BELOW_THRESHOLD_FALLBACK);
            coverage.below_threshold_fallback = hits.len();
            warnings.push(format!(
                "No reranked candidate met the relevance floor {}; delivered the top {} as low-confidence candidates flagged below_threshold=true. They did not pass the floor; verify them before relying on them.",
                options.min_score,
                hits.len()
            ));
        } else {
            hits.retain(|h| h.literal_anchor || !h.below_threshold);
        }
        hits.sort_by(|a, b| {
            b.literal_anchor.cmp(&a.literal_anchor).then_with(|| {
                b.score
                    .total_cmp(&a.score)
                    .then(a.path.cmp(&b.path))
                    .then(a.line_start.cmp(&b.line_start))
            })
        });
    }
    Ok(())
}

fn finish_candidate_batch(
    snapshot: &Snapshot,
    options: &SearchOptions,
    batch: &mut CandidateBatch,
) {
    let CandidateBatch { hits, coverage, .. } = batch;
    coverage.truncated |= hits.len() > options.limit;
    hits.truncate(options.limit);
    // Recheck selected originals after potentially slow remote inference.
    let mut final_freshness = HashMap::new();
    hits.retain(|h| {
        let doc = snapshot
            .manifest
            .documents
            .iter()
            .find(|d| d.path == h.path)
            .expect("hit from manifest");
        if *final_freshness
            .entry(doc.id.as_str())
            .or_insert_with(|| snapshot.fresh(doc))
        {
            true
        } else {
            if !coverage.stale_files.contains(&doc.path) {
                coverage.stale_files.push(doc.path.clone());
            }
            false
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Test-only override that still sends the single-candidate routing call,
    /// so tests can compare the skipped path with the routed one and keep
    /// exercising routing-call mechanics with one-document fixtures.
    static ROUTE_SINGLE_DOCUMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only guard: while alive on this thread, a single routing candidate is
/// still sent to Decisions (the former behavior).
#[cfg(test)]
pub(crate) struct RouteSingleDocument;
#[cfg(test)]
impl RouteSingleDocument {
    pub(crate) fn enable() -> Self {
        ROUTE_SINGLE_DOCUMENT.with(|flag| flag.set(true));
        Self
    }
}
#[cfg(test)]
impl Drop for RouteSingleDocument {
    fn drop(&mut self) {
        ROUTE_SINGLE_DOCUMENT.with(|flag| flag.set(false));
    }
}

/// Exactly one routing candidate is selected without a Decisions call.
fn skip_single_document_routing() -> bool {
    #[cfg(test)]
    {
        !ROUTE_SINGLE_DOCUMENT.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// Add selected documents' leaf nodes round-robin in their routed order until
/// the candidate budget is full. Shared by routed and single-document scopes.
fn add_tree_candidates(
    hits: &mut Vec<Hit>,
    selected: &[&Document],
    texts: &HashMap<String, TextView>,
    options: &SearchOptions,
    coverage: &mut Coverage,
) {
    let mut seen: HashSet<_> = hits.iter().map(|h| h.node_id.clone()).collect();
    let selected: Vec<_> = selected
        .iter()
        .map(|document| (*document, leaf_nodes(&document.nodes)))
        .collect();
    let max_depth = selected
        .iter()
        .map(|(_, leaves)| leaves.len())
        .max()
        .unwrap_or(0);
    for offset in 0..max_depth {
        for (doc, leaves) in &selected {
            if let Some(node) = leaves.get(offset) {
                let h = hit(
                    doc,
                    node,
                    &texts[&doc.id],
                    node.line_start,
                    node.line_end,
                    None,
                    0.0,
                    1400,
                );
                if seen.insert(h.node_id.clone()) {
                    hits.push(h);
                }
            }
            if hits.len() >= options.max_candidates {
                break;
            }
        }
        if hits.len() >= options.max_candidates {
            break;
        }
    }
    coverage.truncated |= selected
        .iter()
        .map(|(doc, _)| doc.nodes.len())
        .sum::<usize>()
        > hits.len();
}

/// Two linear passes preserve source/preorder while removing parents. Compute
/// once per selected document, not once per candidate round.
fn leaf_nodes(nodes: &[TreeNode]) -> Vec<&TreeNode> {
    let parents: HashSet<&str> = nodes
        .iter()
        .filter_map(|node| node.parent_id.as_deref())
        .collect();
    nodes
        .iter()
        .filter(|node| !parents.contains(node.id.as_str()))
        .collect()
}

fn known_stale_paths(
    freshness: &HashMap<String, bool>,
    documents: &HashMap<&str, &Document>,
) -> Vec<String> {
    let mut paths: Vec<_> = freshness
        .iter()
        .filter(|(_, fresh)| !**fresh)
        .filter_map(|(id, _)| {
            documents
                .get(id.as_str())
                .map(|document| document.path.clone())
        })
        .collect();
    paths.sort();
    paths
}

/// Inspect all indexed documents without loading their body text.
pub fn catalog(root: &Path) -> Result<Value> {
    GenerationReader::open(root)?.catalog()
}

fn reader_catalog(snapshot: &GenerationReader) -> Result<Value> {
    snapshot.validate()?;
    let docs: Vec<_> = snapshot.manifest.documents.iter().map(|d| json!({"id":d.id,"path":d.path,"title":d.title,
        "pages":d.pages.len(),"nodes":d.nodes.len(),"parser":d.parser,"source_sha256":d.source_sha256})).collect();
    Ok(json!({"schema_version":SCHEMA,"generation":snapshot.manifest.generation,"documents":docs}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn regex_options() -> SearchOptions {
        SearchOptions {
            mode: "regex".into(),
            ..Default::default()
        }
    }

    fn comparable_report(report: SearchReport) -> Result<Value> {
        let mut value = serde_json::to_value(report)?;
        value["metrics"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms");
        Ok(value)
    }

    #[test]
    fn reader_meter_size_admission_precedes_owned_io_and_failed_hashes_keep_counts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ordinary.txt");
        fs::write(&path, "12345")?;
        let meter = ReadMeter::default();
        assert!(bounded_read_metered(&path, 4, &meter).is_err());
        assert_eq!(meter.snapshot()?, ReadMetrics::default());
        let bytes = bounded_read_metered(&path, 5, &meter)?;
        assert_eq!(bytes, b"12345");
        assert_eq!(meter.snapshot()?.physical_read_bytes, 5);
        assert_eq!(meter.snapshot()?.physical_hash_bytes, 0);
        assert_eq!(digest_metered(&bytes, &meter)?, digest(b"12345"));
        assert_eq!(meter.snapshot()?.physical_hash_bytes, 5);
        fs::remove_file(&path)?;
        assert!(hash_reader_file(&path, 5, &meter).is_err());
        assert_eq!(meter.snapshot()?.physical_read_bytes, 5);
        assert_eq!(meter.snapshot()?.physical_hash_bytes, 5);
        Ok(())
    }

    #[tokio::test]
    async fn reader_cache_warm_clone_counts_windows_and_revalidation_reads_exactly() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = "alpha β gamma\n";
        fs::write(dir.path().join("a.txt"), source)?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        let doc = &reader.manifest.documents[0];
        let node = format!("{}:{}", doc.id, doc.nodes[0].id);
        let manifest_bytes = fs::metadata(reader.dir.join("manifest.json"))?.len();
        let text_bytes =
            fs::metadata(reader.dir.join("text").join(format!("{}.txt", doc.id)))?.len();
        let before = reader.read_metrics()?;
        let first = reader.read_node_window(&node, 5, 0)?;
        let cold = reader.read_metrics()?.since(&before)?;
        assert_eq!(first.text, "alpha");
        assert_eq!(cold.logical_reader_bytes, 5);
        assert_eq!(cold.cached_logical_bytes, 0);
        assert_eq!(cold.cache_misses, 1);
        let expected = 2 * manifest_bytes + 2 * source.len() as u64 + text_bytes;
        assert_eq!(cold.observed_rust_read_bytes, expected);
        assert_eq!(cold.observed_hash_bytes, expected);
        let clone = reader.clone();
        assert!(Arc::ptr_eq(&reader.manifest, &clone.manifest));
        assert!(Arc::ptr_eq(&reader.index, &clone.index));
        let before = clone.read_metrics()?;
        let second = clone.read_node_window(&node, 5, 0)?;
        let warm = reader.read_metrics()?.since(&before)?;
        assert_eq!(serde_json::to_value(first)?, serde_json::to_value(second)?);
        assert_eq!(warm.logical_reader_bytes, 5);
        assert_eq!(warm.cached_logical_bytes, 5);
        assert_eq!(warm.cache_hits, 1);
        // Full current-file digest validation deliberately preserves IO, even
        // when an immutable cached payload avoids a second owned allocation.
        assert_eq!(warm.observed_rust_read_bytes, expected);
        assert_eq!(warm.observed_hash_bytes, expected);
        assert_eq!(reader.cache_status()?["retained_entries"], 1);
        assert!(warm.coverage.contains("kernel"));
        Ok(())
    }

    #[tokio::test]
    async fn reader_cache_pins_keep_capacity_across_clear_clone_and_thread() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "alpha needle\n")?;
        fs::write(dir.path().join("b.txt"), "bravo needle\n")?;
        index(dir.path(), 10).await?;
        let budget = ReaderCacheBudget {
            max_bytes: 13,
            max_entries: 1,
            strict_cache_only: false,
        };
        let reader = GenerationReader::open_with_cache_budget(dir.path(), &budget)?;
        let a = reader
            .manifest
            .documents
            .iter()
            .find(|d| d.path == "a.txt")
            .unwrap();
        let b = reader
            .manifest
            .documents
            .iter()
            .find(|d| d.path == "b.txt")
            .unwrap();
        let pin = reader.text_cached(a)?;
        let clone = reader.clone();
        clone.clear_cache()?;
        assert_eq!(reader.cache_status()?["resident_entries"], 0);
        assert_eq!(reader.cache_status()?["retained_bytes"], 13);
        let before = reader.read_metrics()?;
        let fallback =
            std::thread::scope(|scope| scope.spawn(|| clone.text_cached(b)).join().unwrap())?;
        assert_eq!(&*fallback, "bravo needle\n");
        assert_eq!(fallback.origin, ReadOrigin::Uncached);
        let delta = reader.read_metrics()?.since(&before)?;
        assert_eq!(delta.cache_fallbacks, 1);
        assert_eq!(delta.uncached_load_bytes, 13);
        assert_eq!(delta.observed_rust_read_bytes, 13);
        assert_eq!(delta.observed_hash_bytes, 13);
        assert_eq!(reader.cache_status()?["retained_entries"], 1);
        drop(pin);
        let refill = reader.text_cached(b)?;
        assert_eq!(refill.origin, ReadOrigin::Loaded);
        assert_eq!(reader.cache_status()?["retained_bytes"], 13);
        Ok(())
    }

    #[tokio::test]
    async fn reader_cache_tight_caps_preserve_default_fallback_and_strict_preallocation_failure()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "alpha needle\n")?;
        fs::write(dir.path().join("b.txt"), "bravo needle\n")?;
        index(dir.path(), 10).await?;
        let budget = ReaderCacheBudget {
            max_bytes: 4,
            max_entries: 1,
            strict_cache_only: false,
        };
        let reader = GenerationReader::open_with_cache_budget(dir.path(), &budget)?;
        let a = &reader.manifest.documents[0];
        let node = format!("{}:{}", a.id, a.nodes[0].id);
        let before = reader.read_metrics()?;
        assert_eq!(reader.read_node_window(&node, 5, 0)?.text, "alpha");
        let fallback = reader.read_metrics()?.since(&before)?;
        assert_eq!(fallback.logical_reader_bytes, 5);
        assert_eq!(fallback.uncached_logical_bytes, 5);
        assert_eq!(fallback.cache_fallbacks, 1);
        assert_eq!(fallback.uncached_load_bytes, 13);
        assert_eq!(reader.cache_status()?["retained_bytes"], 0);
        let strict = GenerationReader::open_with_cache_budget(
            dir.path(),
            &ReaderCacheBudget {
                strict_cache_only: true,
                ..budget
            },
        )?;
        let before = strict.read_metrics()?;
        assert!(strict.text_cached(&strict.manifest.documents[0]).is_err());
        let failed = strict.read_metrics()?.since(&before)?;
        assert_eq!(failed.observed_rust_read_bytes, 0);
        assert_eq!(failed.observed_hash_bytes, 0);
        assert_eq!(failed.cache_fallbacks, 0);
        let strict = GenerationReader::open_with_cache_budget(
            dir.path(),
            &ReaderCacheBudget {
                max_bytes: 13,
                max_entries: 1,
                strict_cache_only: true,
            },
        )?;
        let pin = strict.text_cached(&strict.manifest.documents[0])?;
        let before = strict.read_metrics()?;
        assert!(
            strict
                .clone()
                .text_cached(&strict.manifest.documents[1])
                .is_err()
        );
        assert_eq!(
            strict
                .read_metrics()?
                .since(&before)?
                .observed_rust_read_bytes,
            0
        );
        drop(pin);
        strict.text_cached(&strict.manifest.documents[1])?;
        assert!(
            GenerationReader::open_with_cache_budget(
                dir.path(),
                &ReaderCacheBudget {
                    max_bytes: 0,
                    ..Default::default()
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn reader_cache_warm_pin_stays_fixed_and_failed_mutations_keep_observed_bytes()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "needle original\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        let a = &reader.manifest.documents[0];
        let node = format!("{}:{}", a.id, a.nodes[0].id);
        reader.read_node(&node, 100)?;
        fs::write(dir.path().join("b.txt"), "new generation\n")?;
        index(dir.path(), 10).await?;
        assert_ne!(GenerationReader::open(dir.path())?.pin(), reader.pin());
        let before = reader.read_metrics()?;
        assert_eq!(
            reader.clone().read_node(&node, 100)?.text,
            "needle original\n"
        );
        assert_eq!(reader.clone().pin(), reader.pin());
        assert_eq!(reader.read_metrics()?.since(&before)?.cache_hits, 1);
        let text_path = reader.dir.join("text").join(format!("{}.txt", a.id));
        fs::write(&text_path, "needle tampered\n")?;
        let before = reader.read_metrics()?;
        assert!(reader.read_node(&node, 100).is_err());
        let failed = reader.read_metrics()?.since(&before)?;
        assert_eq!(failed.logical_reader_bytes, 0);
        assert_eq!(failed.cache_hits, 1);
        assert!(failed.observed_rust_read_bytes >= 16);
        assert!(failed.observed_hash_bytes >= 16);
        fs::write(&text_path, "needle original\n")?;
        fs::write(dir.path().join("a.txt"), "source changed!\n")?;
        let before = reader.read_metrics()?;
        assert!(reader.read_node(&node, 100).is_err());
        let failed = reader.read_metrics()?.since(&before)?;
        assert_eq!(failed.logical_reader_bytes, 0);
        assert_eq!(failed.cache_hits, 0);
        assert!(failed.observed_rust_read_bytes >= 16);
        fs::write(reader.dir.join("manifest.json"), "{}")?;
        let before = reader.read_metrics()?;
        assert!(reader.catalog().is_err());
        let failed = reader.read_metrics()?.since(&before)?;
        assert_eq!(failed.observed_rust_read_bytes, 2);
        assert_eq!(failed.observed_hash_bytes, 2);
        Ok(())
    }

    #[tokio::test]
    async fn generation_reader_reuses_index_and_matches_single_operations() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("a.md"),
            "# Heading\nalpha.beta\nKEY\n中国文档\n",
        )?;
        fs::write(dir.path().join("b.txt"), "alphaXbeta\nKEY\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        assert!(reader.index.get().is_none());
        assert_eq!(reader.catalog()?, catalog(dir.path())?);
        assert_eq!(
            reader.tree(Path::new("a.md"))?,
            tree(dir.path(), Path::new("a.md"))?
        );
        assert_eq!(reader.status()?, status(dir.path())?);
        for (query, literal, insensitive, limit, scope) in [
            ("alpha.beta", true, false, 10, None),
            ("^alpha.beta$", false, false, 1, None),
            ("key", true, true, 10, Some("a.md")),
            ("中国", true, false, 10, None),
            ("missing-token", true, false, 10, None),
        ] {
            let options = SearchOptions {
                literal,
                case_insensitive: insensitive,
                limit,
                document: scope.map(str::to_owned),
                ..regex_options()
            };
            let reused = reader.search(query, &options).await?;
            let single = search(dir.path(), query, &options).await?;
            assert_eq!(comparable_report(reused)?, comparable_report(single)?);
        }
        let first_index = reader.index_session()? as *const _;
        assert_eq!(first_index, reader.index_session()? as *const _);
        let id = format!(
            "{}:{}",
            reader.manifest.documents[0].id, reader.manifest.documents[0].nodes[0].id
        );
        assert_eq!(
            serde_json::to_value(reader.read_node_window(&id, 5, 0)?)?,
            serde_json::to_value(read_node_window(dir.path(), &id, 5, 0)?)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn generation_reader_stays_pinned_during_publication_and_retained_reopen() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "old needle\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        let pin = reader.pin();
        fs::write(dir.path().join("b.txt"), "new needle\n")?;
        let options = regex_options();
        let (published, observed) =
            tokio::join!(index(dir.path(), 10), reader.search("needle", &options));
        let published = published?;
        let observed = observed?;
        assert_ne!(published.generation, pin.generation);
        assert_eq!(observed.generation, pin.generation);
        assert_eq!(observed.hits.len(), 1);
        assert_eq!(observed.hits[0].path, "a.txt");
        assert_eq!(reader.catalog()?["documents"].as_array().unwrap().len(), 1);
        let reopened = GenerationReader::open_pinned(dir.path(), &pin)?;
        assert_eq!(reopened.pin(), pin);
        assert_eq!(
            reopened
                .search("needle", &regex_options())
                .await?
                .hits
                .len(),
            1
        );
        assert_eq!(
            search(dir.path(), "needle", &regex_options())
                .await?
                .hits
                .len(),
            2
        );
        fs::write(
            dir.path().join(STATE).join("CURRENT.json"),
            b"broken current pointer",
        )?;
        assert!(GenerationReader::open(dir.path()).is_err());
        assert_eq!(
            GenerationReader::open_pinned(dir.path(), &pin)?.catalog()?,
            reader.catalog()?
        );
        Ok(())
    }

    #[tokio::test]
    async fn generation_reader_rechecks_sources_text_and_shared_manifest() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "needle original\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        let doc = &reader.manifest.documents[0];
        let id = format!("{}:{}", doc.id, doc.nodes[0].id);
        reader.search("needle", &regex_options()).await?;
        fs::write(
            reader.dir.join("text").join(format!("{}.txt", doc.id)),
            "needle tampered\n",
        )?;
        assert!(reader.search("needle", &regex_options()).await.is_err());
        assert!(reader.read_node(&id, 100).is_err());
        fs::write(
            reader.dir.join("text").join(format!("{}.txt", doc.id)),
            "needle original\n",
        )?;
        fs::write(dir.path().join("a.txt"), "changed source\n")?;
        assert!(reader.read_node(&id, 100).is_err());
        assert!(reader.tree(Path::new("a.txt")).is_err());
        let stale = reader.search("needle", &regex_options()).await?;
        assert!(stale.hits.is_empty());
        assert_eq!(stale.coverage.stale_files, ["a.txt"]);
        fs::write(reader.dir.join("manifest.json"), "{}")?;
        assert!(reader.catalog().is_err());
        assert!(reader.status().is_err());
        assert!(reader.search("needle", &regex_options()).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn generation_pin_rejects_invalid_identity_and_allows_legacy_metadata_reads() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "needle\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        let pin = reader.pin();
        for bad in [
            GenerationPin {
                generation: "../escape".into(),
                ..pin.clone()
            },
            GenerationPin {
                manifest_sha256: "0".repeat(64),
                ..pin.clone()
            },
            GenerationPin {
                manifest_sha256: "invalid".into(),
                ..pin.clone()
            },
        ] {
            assert!(GenerationReader::open_pinned(dir.path(), &bad).is_err());
        }
        let index_manifest = reader.dir.join("trigram/gptgrep-index.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&index_manifest)?)?;
        value["engine_revision"] = json!("legacy-engine");
        fs::write(index_manifest, serde_json::to_vec(&value)?)?;
        assert_eq!(reader.catalog()?["documents"].as_array().unwrap().len(), 1);
        assert!(
            format!(
                "{:#}",
                reader.search("needle", &regex_options()).await.unwrap_err()
            )
            .contains("rebuild_required")
        );
        Ok(())
    }

    #[tokio::test]
    async fn generation_reader_propagates_shared_index_corruption_after_reuse() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "needle\n")?;
        index(dir.path(), 10).await?;
        let reader = GenerationReader::open(dir.path())?;
        reader.search("needle", &regex_options()).await?;
        fs::remove_file(reader.dir.join("trigram/gptgrep-index.json"))?;
        assert!(reader.search("needle", &regex_options()).await.is_err());
        assert!(reader.catalog().is_err());
        assert!(reader.tree(Path::new("a.txt")).is_err());
        assert!(reader.status().is_err());
        Ok(())
    }

    #[test]
    fn malformed_node_id_is_rejected_before_opening_a_missing_root() {
        let error =
            read_node_window(Path::new("/missing-gptgrep-root"), "bad-id", 100, 0).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("node ID must be document_id:node_id")
        );
    }

    #[tokio::test]
    async fn generation_reader_is_shared_across_threads_while_new_generation_publishes()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "old needle\n")?;
        index(dir.path(), 10).await?;
        let reader = Arc::new(GenerationReader::open(dir.path())?);
        let pin = reader.pin();
        fs::write(dir.path().join("b.txt"), "new needle\n")?;
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut threads = Vec::new();
        for _ in 0..3 {
            let reader = Arc::clone(&reader);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || -> Result<Vec<SearchReport>> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                barrier.wait();
                runtime.block_on(async {
                    let mut reports = Vec::new();
                    for _ in 0..8 {
                        reports.push(reader.search("needle", &regex_options()).await?);
                    }
                    Ok(reports)
                })
            }));
        }
        let publish_root = dir.path().to_owned();
        let publish_barrier = Arc::clone(&barrier);
        let publisher = std::thread::spawn(move || -> Result<IndexReport> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            publish_barrier.wait();
            runtime.block_on(index(&publish_root, 10))
        });
        barrier.wait();
        assert_ne!(publisher.join().unwrap()?.generation, pin.generation);
        for thread in threads {
            for report in thread.join().unwrap()? {
                assert_eq!(report.generation, pin.generation);
                assert_eq!(report.hits.len(), 1);
                assert_eq!(report.hits[0].path, "a.txt");
            }
        }
        assert_eq!(
            GenerationReader::open(dir.path())?.manifest.documents.len(),
            2
        );
        Ok(())
    }

    async fn mock_jev(
        statuses: Vec<u16>,
    ) -> Result<(JevClient, tokio::task::JoinHandle<Vec<Value>>)> {
        mock_jev_scored(statuses, Arc::new(|_| 3.0)).await
    }

    /// Like `mock_jev`, with a rubric score (0..3) chosen from each candidate's text.
    async fn mock_jev_scored(
        statuses: Vec<u16>,
        scorer: Arc<dyn Fn(&str) -> f64 + Send + Sync>,
    ) -> Result<(JevClient, tokio::task::JoinHandle<Vec<Value>>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/api/alpha/decisions", listener.local_addr()?);
        let client = JevClient::with_endpoint("synthetic-core-test-key", None, &endpoint)?;
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for (ordinal, status) in statuses.into_iter().enumerate() {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                let body = loop {
                    let mut buffer = [0; 4096];
                    let count =
                        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                            .await
                            .unwrap()
                            .unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() <= 128 * 1024);
                    if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers =
                            String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                        if bytes.len() >= offset + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &bytes[offset + 4..offset + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                bodies.push(body.clone());
                if status == 0 {
                    // Cancellation fixture: the test explicitly aborts this server.
                    std::future::pending::<()>().await;
                }
                let response = if status == 200 {
                    let answers: serde_json::Map<String, Value> = body["questions"]
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(id, question)| {
                            let text = question["instructions"]["candidate"]["text"]
                                .as_str()
                                .unwrap_or_default();
                            (
                                id.clone(),
                                json!({"type":"score","score":scorer(text),"confidence":0.9}),
                            )
                        })
                        .collect();
                    json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,
                        "usage":{"input_tokens":100+ordinal,"output_tokens":2,"cost":0.001*(ordinal+1) as f64}}).to_string()
                } else {
                    "secret-provider-body-sentinel".into()
                };
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Service Unavailable"
                };
                let wire = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                socket.write_all(wire.as_bytes()).await.unwrap();
            }
            bodies
        });
        Ok((client, server))
    }

    #[tokio::test]
    async fn default_hybrid_requires_credentials_without_local_fallback() -> Result<()> {
        assert_eq!(SearchOptions::default().mode, "hybrid");
        assert_eq!(SearchOptions::default().document, None);
        const CHILD: &str = "GPTGREP_CORE_NO_KEY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "tests::default_hybrid_requires_credentials_without_local_fallback",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env_remove("OPENROUTER_API_KEY")
                .output()?;
            ensure!(
                output.status.success(),
                "isolated no-key test failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }
        ensure!(
            std::env::var_os("OPENROUTER_API_KEY").is_none(),
            "child test requires an absent key"
        );
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("notes.txt"), "needle locally exists")?;
        let indexed = index(directory.path(), 10).await?;
        let error = search(directory.path(), "needle", &SearchOptions::default())
            .await
            .unwrap_err();
        let failure = error
            .downcast_ref::<JevSearchError>()
            .context("missing typed initialization failure")?;
        assert_eq!(failure.stage, "initialization");
        assert_eq!(failure.cause, "OPENROUTER_API_KEY is not available");
        assert_eq!(failure.metrics.jev_calls_attempted, 0);
        assert_eq!(failure.metrics.jev_requests, 0);
        assert_eq!(
            failure.generation.as_deref(),
            Some(indexed.generation.as_str())
        );
        assert!(!failure.accounting_complete);
        Ok(())
    }

    #[tokio::test]
    async fn scoped_document_is_not_starved_by_unrelated_match_limits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        // Pick arbitrary synthetic filenames by their actual internal hash order.
        // The selected text file must sort after the unrelated file in tgrep.
        let mut names: Vec<_> = (0..20)
            .map(|index| format!("synthetic-{index}.txt"))
            .collect();
        names.sort_by_key(|name| digest(name.as_bytes()));
        let unrelated = &names[0];
        let selected = &names[names.len() - 1];
        fs::write(
            directory.path().join(unrelated),
            "needle unrelated\n".repeat(12000),
        )?;
        fs::write(
            directory.path().join(selected),
            "needle selected evidence\n",
        )?;
        index(directory.path(), 10).await?;
        let unscoped = search(
            directory.path(),
            "needle",
            &SearchOptions {
                limit: 1,
                ..regex_options()
            },
        )
        .await?;
        assert_eq!(unscoped.hits[0].path, *unrelated);
        assert_eq!(unscoped.document_scope, None);
        assert_eq!(
            (
                unscoped.coverage.indexed_files,
                unscoped.coverage.scoped_files
            ),
            (2, 2)
        );
        for mode in ["regex", "lexical"] {
            let report = search(
                directory.path(),
                "needle",
                &SearchOptions {
                    mode: mode.into(),
                    document: Some(selected.clone()),
                    limit: 1,
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(report.hits.len(), 1);
            assert_eq!(report.hits[0].path, *selected);
            assert_eq!(report.document_scope.as_deref(), Some(selected.as_str()));
            assert_eq!(
                (report.coverage.indexed_files, report.coverage.scoped_files),
                (2, 1)
            );
            assert_eq!(
                (
                    report.coverage.candidate_files,
                    report.coverage.verified_files
                ),
                (1, 1)
            );
            assert!(!report.coverage.truncated);
            assert_eq!(report.metrics.jev_calls_attempted, 0);
        }
        let first_in_index = search(
            directory.path(),
            "needle",
            &SearchOptions {
                document: Some(unrelated.clone()),
                limit: 1,
                ..regex_options()
            },
        )
        .await?;
        assert_eq!(first_in_index.hits[0].path, *unrelated);
        assert_eq!(first_in_index.coverage.scoped_files, 1);
        assert_eq!(first_in_index.coverage.candidate_files, 1);
        assert!(first_in_index.coverage.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn stale_candidates_cannot_consume_fresh_regex_or_lexical_match_budgets() -> Result<()> {
        for (query, old_text, fresh_text) in [
            ("needle", "needle old evidence\n", "needle fresh evidence\n"),
            ("中国", "中国 旧证据\n", "中国 新证据\n"),
        ] {
            let directory = tempfile::tempdir()?;
            let mut names: Vec<_> = (0..20)
                .map(|index| format!("admission-{index}.txt"))
                .collect();
            names.sort_by_key(|name| digest(name.as_bytes()));
            let stale = &names[0];
            let unrelated = &names[names.len() / 2];
            let fresh = &names[names.len() - 1];
            fs::write(directory.path().join(stale), old_text.repeat(12000))?;
            fs::write(directory.path().join(fresh), fresh_text)?;
            fs::write(directory.path().join(unrelated), "zzzzzz\n")?;
            index(directory.path(), 10).await?;
            fs::write(directory.path().join(stale), "changed original")?;
            fs::remove_file(directory.path().join(unrelated))?;
            for mode in ["regex", "lexical"] {
                let report = search(
                    directory.path(),
                    query,
                    &SearchOptions {
                        mode: mode.into(),
                        limit: 1,
                        ..Default::default()
                    },
                )
                .await?;
                assert_eq!(report.hits.len(), 1, "{query}/{mode}");
                assert_eq!(report.hits[0].path, *fresh);
                assert_eq!(report.hits[0].text, fresh_text.trim_end_matches('\n'));
                assert!(report.hits[0].source_fresh);
                assert!(report.coverage.stale_files.contains(stale));
                assert_eq!(
                    (report.coverage.indexed_files, report.coverage.scoped_files),
                    (3, 3)
                );
                assert_eq!(report.coverage.verified_files, 1);
                assert_eq!(
                    report.coverage.stale_files.len() + 1,
                    report.coverage.candidate_files
                );
                assert!(!report.coverage.truncated);
                if mode == "regex" && query == "needle" {
                    // The selective ASCII plan excludes this deleted file.
                    // There is no eager full-corpus freshness pass. Conservative
                    // Unicode/case-fold plans may legitimately admit all files.
                    assert_eq!(report.coverage.candidate_files, 2);
                    assert!(!report.coverage.stale_files.contains(unrelated));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn leaf_precomputation_handles_large_flat_trees_in_source_order() {
        let mut nodes: Vec<_> = (0..20000)
            .map(|index| TreeNode {
                id: format!("node-{index}"),
                parent_id: None,
                title: format!("Heading {index}"),
                level: 1,
                page_start: 1,
                page_end: 1,
                line_start: 1,
                line_end: 1,
                key_items: Vec::new(),
            })
            .collect();
        let flat = leaf_nodes(&nodes);
        assert_eq!(flat.len(), nodes.len());
        assert!(
            flat.iter()
                .zip(&nodes)
                .all(|(leaf, node)| std::ptr::eq(*leaf, node))
        );
        nodes[500].parent_id = Some(nodes[1].id.clone());
        nodes[500].level = 2;
        nodes[501].parent_id = Some(nodes[500].id.clone());
        nodes[501].level = 3;
        let nested = leaf_nodes(&nodes);
        let expected: Vec<_> = nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| ![1, 500].contains(index))
            .map(|(_, node)| node.id.as_str())
            .collect();
        assert_eq!(
            nested
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[tokio::test]
    async fn semantic_leaf_candidates_keep_ranked_document_round_robin_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let text =
            "# Section Zero\nsource zero\n# Section One\nsource one\n# Section Two\nsource two\n";
        for name in ["synthetic-a.md", "synthetic-b.md"] {
            fs::write(directory.path().join(name), text)?;
        }
        index(directory.path(), 10).await?;
        let snapshot = Snapshot::open(directory.path())?;
        let mut ranked: Vec<_> = snapshot.manifest.documents.iter().collect();
        ranked.sort_by(|left, right| left.id.cmp(&right.id));
        let (client, server) = mock_jev(vec![200, 200]).await?;
        let report = search_with_client(
            directory.path(),
            "unrelated-query",
            &SearchOptions {
                mode: "semantic".into(),
                limit: 4,
                max_candidates: 4,
                ..Default::default()
            },
            &client,
        )
        .await?;
        let requests = server.await?;
        let questions = requests[1]["questions"].as_object().unwrap();
        assert_eq!(questions.len(), 4);
        for (candidate, section) in ["Section Zero", "Section One"].iter().enumerate() {
            for (document, expected) in ranked.iter().enumerate() {
                let id = format!("candidate_{}", candidate * ranked.len() + document);
                let content = questions[&id]["instructions"]["candidate"]["text"]
                    .as_str()
                    .unwrap();
                assert!(content.starts_with(&format!(
                    "Document: {}\nSection: {section}\n",
                    expected.path
                )));
            }
        }
        assert_eq!(report.hits.len(), 4);
        assert_eq!(report.coverage.reranked_candidates, 4);
        assert!(report.coverage.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn scope_validates_exact_paths_and_preserves_unicode_regex_and_freshness() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("目录"))?;
        let selected = "目录/研究.txt";
        fs::write(directory.path().join(selected), "中国文档\r\nKEY\r\n")?;
        fs::write(directory.path().join("other.txt"), "中国文档\nKEY\n")?;
        index(directory.path(), 10).await?;
        let options = SearchOptions {
            document: Some(selected.into()),
            case_insensitive: true,
            ..regex_options()
        };
        let report = search(directory.path(), "中国|key", &options).await?;
        assert_eq!(report.hits.len(), 2);
        assert!(report.hits.iter().all(|hit| hit.path == selected));
        assert_eq!(report.hits[1].text, "KEY");
        for requested in [
            "",
            "unknown.txt",
            "../other.txt",
            "/other.txt",
            "./other.txt",
            "目录//研究.txt",
            "目录/./研究.txt",
            "other.txt/",
        ] {
            assert!(
                search(
                    directory.path(),
                    "needle",
                    &SearchOptions {
                        document: Some(requested.into()),
                        ..regex_options()
                    }
                )
                .await
                .is_err(),
                "{requested}"
            );
        }
        fs::remove_file(directory.path().join(selected))?;
        let stale = search(directory.path(), "中国", &options).await?;
        assert!(stale.hits.is_empty());
        assert_eq!(stale.coverage.stale_files, [selected]);
        assert_eq!(stale.coverage.scoped_files, 1);
        Ok(())
    }

    #[tokio::test]
    async fn semantic_scope_precedes_routing_budget_and_emits_real_mock_receipts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let unrelated_count = SearchOptions::default().routing_docs + 3;
        for index in 0..unrelated_count {
            fs::write(
                directory.path().join(format!("unrelated-{index:02}.txt")),
                "background text",
            )?;
        }
        let selected = "zz-selected.md";
        fs::write(
            directory.path().join(selected),
            "# Scope\nDistinct source evidence.\n",
        )?;
        let indexed = index(directory.path(), unrelated_count + 1).await?;
        let (client, server) = mock_jev(vec![200]).await?;
        let events: Arc<Mutex<Vec<JevSearchProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let observer = move |progress: &JevSearchProgress| {
            recorded.lock().unwrap().push(progress.clone());
            Ok(())
        };
        let report = search_with_client_and_observer(
            directory.path(),
            "requested-topic",
            &SearchOptions {
                document: Some(selected.into()),
                ..Default::default()
            },
            &client,
            &observer,
        )
        .await?;
        // The scope leaves one routing candidate: it is selected without a
        // routing call, and only the scoped evidence is reranked.
        let requests = server.await?;
        assert_eq!(requests.len(), 1);
        let candidates = requests[0]["questions"].as_object().unwrap();
        assert!(!candidates.is_empty());
        for question in candidates.values() {
            let text = question["instructions"]["candidate"]["text"]
                .as_str()
                .unwrap();
            assert!(text.starts_with(&format!("Document: {selected}\n")));
            assert!(!text.contains("unrelated-"));
        }
        assert!(report.hits.iter().all(|hit| hit.path == selected));
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.coverage.indexed_files, unrelated_count + 1);
        assert_eq!(report.coverage.scoped_files, 1);
        assert_eq!(report.coverage.routed_documents, 1);
        assert_eq!(
            report.coverage.routing_status.as_deref(),
            Some("skipped_single_document")
        );
        assert!(report.coverage.semantic_scope_complete);
        assert_eq!(report.document_scope.as_deref(), Some(selected));
        assert_eq!(
            (
                report.metrics.jev_calls_attempted,
                report.metrics.jev_requests
            ),
            (1, 1)
        );
        assert_eq!(
            report.metrics.jev_models,
            ["openai/gpt-6-luna-decisions-20261006"]
        );
        // The requested selector and the served canonical build stay separate.
        assert_eq!(
            report.metrics.jev_requested_models,
            [gptgrep_jev::DEFAULT_MODEL]
        );
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| (event.stage.as_str(), event.event.as_str()))
                .collect::<Vec<_>>(),
            [
                ("evidence_reranking", "before_call"),
                ("evidence_reranking", "after_reply")
            ]
        );
        assert_eq!(
            (
                events[1].metrics.jev_calls_attempted,
                events[1].metrics.jev_requests
            ),
            (1, 1)
        );
        assert_eq!(events[1].metrics.jev_usage[0]["cost"], json!(0.001));
        assert!(events.iter().all(|event| event.generation.as_deref()
            == Some(indexed.generation.as_str())
            && !event.accounting_complete));
        Ok(())
    }

    /// Two indexed documents; `first.md` is the scoped one.
    async fn two_document_corpus() -> Result<tempfile::TempDir> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("first.md"),
            "# Intake\nneedle intake evidence\n# Storage\nshelf storage notes\n# Review\nreview cadence notes\n",
        )?;
        fs::write(
            directory.path().join("second.md"),
            "# Other\nneedle other evidence\n",
        )?;
        index(directory.path(), 10).await?;
        Ok(directory)
    }

    #[tokio::test]
    async fn single_document_routing_is_skipped_with_identical_rerank_input() -> Result<()> {
        let directory = two_document_corpus().await?;
        for mode in ["hybrid", "semantic"] {
            let options = SearchOptions {
                mode: mode.into(),
                document: Some("first.md".into()),
                ..Default::default()
            };
            // Reference: the former path, which sent the one-candidate routing call.
            let route = RouteSingleDocument::enable();
            let (client, server) = mock_jev(vec![200, 200]).await?;
            let routed = search_with_client(directory.path(), "needle", &options, &client).await;
            drop(route);
            let routed = routed?;
            let routed_requests = server.await?;
            assert_eq!(routed_requests.len(), 2, "{mode}");
            assert_eq!(
                routed.coverage.routing_status.as_deref(),
                Some("routed"),
                "{mode}"
            );
            // Default: no routing call; one request fewer.
            let (client, server) = mock_jev(vec![200]).await?;
            let skipped = search_with_client(directory.path(), "needle", &options, &client).await?;
            let skipped_requests = server.await?;
            assert_eq!(skipped_requests.len(), 1, "{mode}");
            // The rerank request is identical to the routed path's second request.
            assert_eq!(
                serde_json::to_vec(&skipped_requests[0])?,
                serde_json::to_vec(&routed_requests[1])?,
                "{mode}"
            );
            assert_eq!(
                skipped.coverage.routing_status.as_deref(),
                Some("skipped_single_document")
            );
            assert_eq!(
                (
                    skipped.metrics.jev_calls_attempted,
                    skipped.metrics.jev_requests
                ),
                (1, 1)
            );
            assert_eq!(
                (
                    routed.metrics.jev_calls_attempted,
                    routed.metrics.jev_requests
                ),
                (2, 2)
            );
            // Outputs are otherwise unchanged.
            let mut skipped_coverage = serde_json::to_value(&skipped.coverage)?;
            let mut routed_coverage = serde_json::to_value(&routed.coverage)?;
            skipped_coverage
                .as_object_mut()
                .unwrap()
                .remove("routing_status");
            routed_coverage
                .as_object_mut()
                .unwrap()
                .remove("routing_status");
            assert_eq!(skipped_coverage, routed_coverage, "{mode}");
            assert_eq!(
                serde_json::to_value(&skipped.hits)?,
                serde_json::to_value(&routed.hits)?,
                "{mode}"
            );
            assert!(!skipped.hits.is_empty());
            assert_eq!(skipped.warnings, routed.warnings);
        }
        Ok(())
    }

    #[tokio::test]
    async fn two_document_scope_still_routes() -> Result<()> {
        let directory = two_document_corpus().await?;
        let (client, server) = mock_jev(vec![200, 200]).await?;
        let report = search_with_client(
            directory.path(),
            "needle",
            &SearchOptions::default(),
            &client,
        )
        .await?;
        let requests = server.await?;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["questions"].as_object().unwrap().len(), 2);
        assert_eq!(report.coverage.routed_documents, 2);
        assert_eq!(report.coverage.routing_status.as_deref(), Some("routed"));
        assert_eq!(
            (
                report.metrics.jev_calls_attempted,
                report.metrics.jev_requests
            ),
            (2, 2)
        );
        // Local primitives carry no routing status.
        let lexical = search(
            directory.path(),
            "needle",
            &SearchOptions {
                mode: "lexical".into(),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(lexical.coverage.routing_status, None);
        assert!(!serde_json::to_string(&lexical.coverage)?.contains("routing_status"));
        Ok(())
    }

    #[tokio::test]
    async fn shared_decisions_context_spans_routing_reranking_and_later_searches() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("subject.txt"),
            "needle source evidence",
        )?;
        fs::write(directory.path().join("other.txt"), "other background")?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![200, 200]).await?;
        let context = gptgrep_jev::ExecutionContext::new(
            "ordinary-shared-search-test",
            &"a".repeat(64),
            std::time::SystemTime::now() + Duration::from_secs(10),
            2,
        )?;
        let client = client.with_execution_context(context.clone());
        let options = SearchOptions::default();
        let report =
            search_with_client(directory.path(), "needle", &options, &client.clone()).await?;
        assert_eq!(report.metrics.jev_requests, 2);
        assert_eq!(
            report.metrics.jev_models,
            [
                "openai/gpt-6-luna-decisions-20261006",
                "openai/gpt-6-luna-decisions-20261006"
            ]
        );
        assert_eq!(server.await?.len(), 2);
        let snapshot = context.snapshot()?;
        assert_eq!(snapshot.logical_reservations, 2);
        assert_eq!(snapshot.send_start_observations, 2);
        assert_eq!(snapshot.terminal_submissions, 2);
        assert!(
            search_with_client(directory.path(), "needle", &options, &client)
                .await
                .is_err()
        );
        assert_eq!(context.snapshot()?.logical_reservations, 2);
        Ok(())
    }

    #[tokio::test]
    async fn failed_rerank_keeps_routing_usage_in_typed_error() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("subject.txt"),
            "needle source evidence",
        )?;
        fs::write(directory.path().join("other.txt"), "other background")?;
        let indexed = index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![200, 503]).await?;
        let error = search_with_client(
            directory.path(),
            "needle",
            &SearchOptions::default(),
            &client,
        )
        .await
        .unwrap_err();
        let requests = server.await?;
        assert_eq!(requests.len(), 2);
        let failure = error
            .downcast_ref::<JevSearchError>()
            .context("missing typed Jev error")?;
        assert_eq!(failure.stage, "evidence_reranking");
        assert_eq!(
            (
                failure.metrics.jev_calls_attempted,
                failure.metrics.jev_requests
            ),
            (2, 1)
        );
        assert_eq!(
            failure.metrics.jev_models,
            ["openai/gpt-6-luna-decisions-20261006"]
        );
        // Both attempts, including the failed one, retain their requested selector.
        assert_eq!(
            failure.metrics.jev_requested_models,
            [gptgrep_jev::DEFAULT_MODEL, gptgrep_jev::DEFAULT_MODEL]
        );
        assert_eq!(failure.metrics.jev_usage[0]["cost"], json!(0.001));
        assert_eq!(failure.coverage.scoped_files, 2);
        assert_eq!(failure.document_scope, None);
        assert_eq!(
            failure.generation.as_deref(),
            Some(indexed.generation.as_str())
        );
        assert!(!failure.accounting_complete);
        assert!(failure.cause.contains("HTTP 503"));
        let serialized = serde_json::to_string(failure)?;
        assert!(!serialized.contains("secret-provider-body-sentinel"));
        assert!(!serialized.contains("synthetic-core-test-key"));
        let roundtrip: JevSearchError = serde_json::from_str(&serialized)?;
        assert_eq!(roundtrip.metrics.jev_usage, failure.metrics.jev_usage);
        Ok(())
    }

    #[tokio::test]
    async fn failed_first_stage_has_an_attempt_and_unknown_accounting() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("subject.txt"), "source evidence")?;
        fs::write(directory.path().join("other.txt"), "other background")?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![503]).await?;
        let error = search_with_client(
            directory.path(),
            "query",
            &SearchOptions::default(),
            &client,
        )
        .await
        .unwrap_err();
        server.await?;
        let failure = error.downcast_ref::<JevSearchError>().unwrap();
        assert_eq!(failure.stage, "document_routing");
        assert_eq!(
            (
                failure.metrics.jev_calls_attempted,
                failure.metrics.jev_requests
            ),
            (1, 0)
        );
        assert!(failure.metrics.jev_usage.is_empty());
        assert!(!failure.accounting_complete);
        Ok(())
    }

    #[tokio::test]
    async fn transient_status_is_retried_once_and_both_attempts_are_accounted() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("first.txt"), "needle first evidence")?;
        fs::write(
            directory.path().join("second.txt"),
            "needle second evidence",
        )?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![502, 200, 200]).await?;
        let client = client.with_transient_retry(TransientRetry::with_jitter(|| 0.0));
        let events: Arc<Mutex<Vec<JevSearchProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let observer = move |progress: &JevSearchProgress| {
            recorded.lock().unwrap().push(progress.clone());
            Ok(())
        };
        let report = search_with_client_and_observer(
            directory.path(),
            "needle",
            &SearchOptions::default(),
            &client,
            &observer,
        )
        .await?;
        let requests = server.await?;
        assert_eq!(requests.len(), 3);
        // The retry repeats the identical routing request.
        assert_eq!(requests[0], requests[1]);
        assert_eq!(
            (
                report.metrics.jev_calls_attempted,
                report.metrics.jev_requests
            ),
            (3, 2)
        );
        assert_eq!(report.metrics.jev_requested_models.len(), 3);
        // Only validated replies carry usage; the failed attempt stays unknown.
        assert_eq!(report.metrics.jev_usage.len(), 2);
        assert_eq!(report.metrics.jev_retried_attempts.len(), 1);
        let retried = &report.metrics.jev_retried_attempts[0];
        assert_eq!(retried.stage, "document_routing");
        assert_eq!(retried.attempt.attempt, 1);
        assert_eq!(retried.attempt.http_status, Some(502));
        assert!(retried.attempt.reported_usage.is_none());
        let metrics = serde_json::to_value(&report.metrics)?;
        assert_eq!(
            metrics["jev_retried_attempts"][0]["reported_usage"],
            Value::Null
        );
        assert_eq!(
            metrics["jev_retried_attempts"][0]["stage"],
            "document_routing"
        );
        let roundtrip: Metrics = serde_json::from_value(metrics)?;
        assert_eq!(
            roundtrip.jev_retried_attempts,
            report.metrics.jev_retried_attempts
        );
        assert!(!report.hits.is_empty());
        let events = events.lock().unwrap();
        let routed = events
            .iter()
            .find(|event| event.stage == "document_routing" && event.event == "after_reply")
            .unwrap();
        assert_eq!(routed.metrics.jev_retried_attempts.len(), 1);
        assert_eq!(
            (
                routed.metrics.jev_calls_attempted,
                routed.metrics.jev_requests
            ),
            (2, 1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn repeated_transient_failure_keeps_both_attempts_in_typed_error() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("first.txt"), "needle first evidence")?;
        fs::write(
            directory.path().join("second.txt"),
            "needle second evidence",
        )?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![200, 503, 503]).await?;
        let client = client.with_transient_retry(TransientRetry::with_jitter(|| 0.0));
        let error = search_with_client(
            directory.path(),
            "needle",
            &SearchOptions::default(),
            &client,
        )
        .await
        .unwrap_err();
        assert_eq!(server.await?.len(), 3);
        let failure = error
            .downcast_ref::<JevSearchError>()
            .context("missing typed Jev error")?;
        assert_eq!(failure.stage, "evidence_reranking");
        assert_eq!(
            (
                failure.metrics.jev_calls_attempted,
                failure.metrics.jev_requests
            ),
            (3, 1)
        );
        assert_eq!(failure.metrics.jev_usage.len(), 1);
        assert_eq!(failure.metrics.jev_retried_attempts.len(), 1);
        assert_eq!(
            failure.metrics.jev_retried_attempts[0].stage,
            "evidence_reranking"
        );
        assert_eq!(
            failure.metrics.jev_retried_attempts[0].attempt.http_status,
            Some(503)
        );
        assert!(failure.cause.contains("after one bounded retry"));
        assert!(failure.cause.contains("attempt 2: HTTP 503"));
        assert!(!failure.accounting_complete);
        let serialized = serde_json::to_string(failure)?;
        assert!(!serialized.contains("secret-provider-body-sentinel"));
        Ok(())
    }

    #[test]
    fn default_search_client_uses_the_bounded_retry_but_keeps_supplied_policy() {
        let endpoint = "http://127.0.0.1:9/api/alpha/decisions";
        let plain = JevClient::with_endpoint("synthetic-core-test-key", None, endpoint).unwrap();
        assert!(plain.transient_retry().is_none());
        assert!(default_retrieval_client(plain).transient_retry().is_some());
        let supplied = JevClient::with_endpoint("synthetic-core-test-key", None, endpoint)
            .unwrap()
            .with_transient_retry(TransientRetry::with_jitter(|| 1.0));
        assert_eq!(
            default_retrieval_client(supplied)
                .transient_retry()
                .unwrap()
                .backoff(),
            Duration::from_millis(1000)
        );
    }

    /// One document whose leaf sections carry distinct synthetic markers.
    async fn floor_corpus() -> Result<tempfile::TempDir> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("notes.md"),
            "# Alpha\nmarker-a quartz entry\n# Beta\nmarker-b slate entry\n# Gamma\nmarker-c basalt entry\n# Delta\nmarker-d marble entry\n# Epsilon\nmarker-e chalk entry\n",
        )?;
        index(directory.path(), 10).await?;
        Ok(directory)
    }

    /// Rubric scores below the 0.5 floor (1.5 of 3), distinct per section.
    fn below_floor_score(text: &str) -> f64 {
        [
            ("marker-a", 0.3),
            ("marker-b", 1.2),
            ("marker-c", 0.0),
            ("marker-d", 0.9),
            ("marker-e", 0.6),
        ]
        .iter()
        .find(|(marker, _)| text.contains(marker))
        .map_or(0.0, |(_, score)| *score)
    }

    #[tokio::test]
    async fn all_candidates_below_floor_deliver_top_three_flagged_low_confidence() -> Result<()> {
        let directory = floor_corpus().await?;
        let (client, server) = mock_jev_scored(vec![200], Arc::new(below_floor_score)).await?;
        let report = search_with_client(
            directory.path(),
            "unrelated survey question",
            &SearchOptions {
                mode: "semantic".into(),
                ..Default::default()
            },
            &client,
        )
        .await?;
        assert_eq!(server.await?.len(), 1);
        assert_eq!(report.coverage.reranked_candidates, 5);
        // None passed the floor; the best three by score are delivered, flagged.
        assert_eq!(report.coverage.filtered_candidates, 5);
        assert_eq!(report.coverage.below_threshold_fallback, 3);
        assert_eq!(report.minimum_relevance_score, Some(0.5));
        let delivered: Vec<_> = report.hits.iter().map(|hit| hit.title.as_str()).collect();
        assert_eq!(delivered, ["Beta", "Delta", "Epsilon"]);
        assert!(
            report
                .hits
                .windows(2)
                .all(|pair| pair[0].score > pair[1].score)
        );
        assert!(
            report
                .hits
                .iter()
                .all(|hit| hit.below_threshold && hit.score < 0.5 && !hit.literal_anchor)
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("low-confidence candidates"))
        );
        let value = serde_json::to_value(&report)?;
        assert_eq!(value["hits"][0]["below_threshold"], true);
        assert_eq!(value["coverage"]["below_threshold_fallback"], 3);
        Ok(())
    }

    #[tokio::test]
    async fn a_passing_candidate_keeps_the_floor_without_fallback() -> Result<()> {
        let directory = floor_corpus().await?;
        let (client, server) = mock_jev_scored(
            vec![200],
            Arc::new(|text: &str| {
                if text.contains("marker-c") {
                    2.4
                } else {
                    below_floor_score(text)
                }
            }),
        )
        .await?;
        let report = search_with_client(
            directory.path(),
            "unrelated survey question",
            &SearchOptions {
                mode: "semantic".into(),
                ..Default::default()
            },
            &client,
        )
        .await?;
        server.await?;
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].title, "Gamma");
        assert!(!report.hits[0].below_threshold);
        assert_eq!(report.coverage.filtered_candidates, 4);
        assert_eq!(report.coverage.below_threshold_fallback, 0);
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("low-confidence"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_floor_and_literal_anchor_stay_truthful() -> Result<()> {
        let directory = floor_corpus().await?;
        // A literal anchor below the floor passes as an anchor: no fallback,
        // but its score is still flagged as below the floor.
        let (client, server) = mock_jev_scored(vec![200], Arc::new(below_floor_score)).await?;
        let anchored = search_with_client(
            directory.path(),
            "quartz",
            &SearchOptions::default(),
            &client,
        )
        .await?;
        server.await?;
        assert_eq!(anchored.coverage.below_threshold_fallback, 0);
        assert_eq!(anchored.coverage.retained_literal_anchors, 1);
        assert_eq!(anchored.hits.len(), 1);
        assert!(anchored.hits[0].literal_anchor && anchored.hits[0].below_threshold);
        // An explicit stricter floor: every candidate scores 2/3, so the
        // fallback is flagged against the requested floor, never as passing.
        let (client, server) = mock_jev_scored(vec![200], Arc::new(|_| 2.0)).await?;
        let strict = search_with_client(
            directory.path(),
            "unrelated survey question",
            &SearchOptions {
                mode: "semantic".into(),
                min_score: 0.9,
                ..Default::default()
            },
            &client,
        )
        .await?;
        server.await?;
        assert_eq!(strict.minimum_relevance_score, Some(0.9));
        assert_eq!(strict.coverage.below_threshold_fallback, 3);
        assert!(strict.hits.iter().all(|hit| hit.below_threshold));
        assert!(
            strict
                .warnings
                .iter()
                .any(|warning| warning.contains("relevance floor 0.9"))
        );
        // Local modes apply no floor and never flag hits.
        let lexical = search(
            directory.path(),
            "quartz",
            &SearchOptions {
                mode: "lexical".into(),
                ..Default::default()
            },
        )
        .await?;
        assert!(lexical.hits.iter().all(|hit| !hit.below_threshold));
        Ok(())
    }

    #[tokio::test]
    async fn observer_failure_after_routing_stops_before_next_provider_call() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("subject.txt"), "source evidence")?;
        fs::write(directory.path().join("other.txt"), "other background")?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![200]).await?;
        let observer = |progress: &JevSearchProgress| {
            if progress.event == "after_reply" {
                bail!("secret-collector-sentinel");
            }
            Ok(())
        };
        let error = search_with_client_and_observer(
            directory.path(),
            "query",
            &SearchOptions::default(),
            &client,
            &observer,
        )
        .await
        .unwrap_err();
        assert_eq!(server.await?.len(), 1);
        let failure = error.downcast_ref::<JevSearchError>().unwrap();
        assert_eq!(
            (
                failure.metrics.jev_calls_attempted,
                failure.metrics.jev_requests
            ),
            (1, 1)
        );
        assert_eq!(failure.metrics.jev_usage[0]["cost"], json!(0.001));
        assert!(
            failure
                .cause
                .contains("observer failed after a validated reply")
        );
        assert!(!failure.cause.contains("secret-collector-sentinel"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_retains_prior_reply_in_progress_receipts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("subject.txt"), "source evidence")?;
        fs::write(directory.path().join("other.txt"), "other background")?;
        index(directory.path(), 10).await?;
        let (client, server) = mock_jev(vec![200, 0]).await?;
        let events: Arc<Mutex<Vec<JevSearchProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Mutex::new(Some(sender));
        let observer = move |progress: &JevSearchProgress| {
            recorded.lock().unwrap().push(progress.clone());
            if progress.stage == "evidence_reranking"
                && progress.event == "before_call"
                && let Some(sender) = sender.lock().unwrap().take()
            {
                let _ = sender.send(());
            }
            Ok(())
        };
        let root = directory.path().to_owned();
        let task = tokio::spawn(async move {
            search_with_client_and_observer(
                &root,
                "query",
                &SearchOptions::default(),
                &client,
                &observer,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), receiver).await??;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        server.abort();
        let _ = server.await;
        let events = events.lock().unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.event, "before_call");
        assert_eq!(
            (last.metrics.jev_calls_attempted, last.metrics.jev_requests),
            (2, 1)
        );
        assert_eq!(last.metrics.jev_usage[0]["cost"], json!(0.001));
        assert!(!last.accounting_complete);
        Ok(())
    }
    #[test]
    fn candidate_terms_drop_function_words_and_short_ascii_tokens() {
        assert_eq!(
            candidate_terms("What is the role of a Q3 2019 report in 杭州, per the report?"),
            ["role", "q3", "2019", "report", "杭州"]
        );
        // Tokens with digits are kept at any length; other short ASCII tokens drop.
        assert_eq!(candidate_terms("Is 7 of 12 ok"), ["7", "12"]);
        // A query of only dropped tokens keeps its tokens.
        assert_eq!(candidate_terms("is it on"), ["is", "it", "on"]);
        assert_eq!(
            candidate_terms("Ledger LEDGER ledger_2"),
            ["ledger", "ledger_2"]
        );
    }

    #[test]
    fn rarer_terms_rank_first_absent_last_and_ties_keep_question_order() {
        let terms: Vec<String> = (0..20).map(|i| format!("term{i:02}")).collect();
        let mut frequencies = vec![5; 20];
        frequencies[17] = 1;
        frequencies[19] = 1;
        frequencies[3] = 0;
        frequencies[2] = 2;
        let ranked = rank_terms(terms, &frequencies);
        assert_eq!(ranked.len(), MAX_LEXICAL_TERMS);
        assert_eq!(ranked[..3], ["term17", "term19", "term02"]);
        // Equal frequencies keep first occurrence; the absent term sorts last.
        assert_eq!(ranked[3..6], ["term00", "term01", "term04"]);
        assert!(!ranked.contains(&"term03".to_owned()));
    }

    #[tokio::test]
    async fn long_question_keeps_distinctive_terms_after_position_sixteen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let common = [
            "ledger", "archive", "record", "entry", "update", "review", "status", "note", "budget",
            "office", "region", "quarter", "metric", "policy", "summary", "filler",
        ];
        for index in 0..4 {
            fs::write(
                directory.path().join(format!("common-{index}.txt")),
                format!("{}\nroutine page {index}\n", common.join(" ")),
            )?;
        }
        fs::write(
            directory.path().join("rare.txt"),
            format!("{}\nThe zirconium flask holds quinoa.\n", common.join(" ")),
        )?;
        index(directory.path(), 10).await?;
        // Sixteen shared terms come first; the distinctive ones follow them.
        let question = format!(
            "Which of the {} lines also mention zirconium and quinoa?",
            common.join(" ")
        );
        let snapshot = Snapshot::open(directory.path())?;
        let selected = lexical_terms(&snapshot, &question, None)?;
        assert_eq!(selected.len(), MAX_LEXICAL_TERMS);
        assert_eq!(selected[..2], ["zirconium", "quinoa"]);
        // Equal document frequencies keep question order; the last two drop.
        assert_eq!(selected[2..], common[..14]);
        // Deterministic across runs and fresh snapshots.
        assert_eq!(selected, lexical_terms(&snapshot, &question, None)?);
        assert_eq!(
            selected,
            lexical_terms(&Snapshot::open(directory.path())?, &question, None)?
        );
        // Previously the first sixteen tokens filled the cap and no lexical
        // candidate reached the distinctive line.
        let report = search(
            directory.path(),
            &question,
            &SearchOptions {
                mode: "lexical".into(),
                limit: 1,
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(report.hits[0].path, "rare.txt");
        assert!(report.hits[0].text.contains("zirconium"));
        // Scoped frequencies use only the selected document.
        let scoped = lexical_terms(
            &snapshot,
            &question,
            Some(Path::new(&format!(
                "{}.txt",
                snapshot
                    .manifest
                    .documents
                    .iter()
                    .find(|document| document.path == "common-0.txt")
                    .unwrap()
                    .id
            ))),
        )?;
        // Absent distinctive terms sort last within one common document.
        assert_eq!(scoped[..], common[..]);
        Ok(())
    }

    #[test]
    fn unicode_boundaries() {
        assert_eq!(truncate("文档abc", 4), ("文", true));
    }
    #[test]
    fn secret_paths_denied() {
        for p in ["x/.env", "auth.json", "x/private.key", ".gptgrep/a"] {
            assert!(denied(Path::new(p)));
        }
    }
    #[test]
    fn lexical_is_not_semantic() {
        assert_eq!(
            lexical_score("car speed", &candidate_terms("automobile velocity")),
            0.0
        );
    }
    #[test]
    fn literal_anchors_preserve_exact_atomic_queries_not_substrings_or_questions() {
        assert!(literal_anchor("naïve", "Accent forms: naïve and résumé."));
        assert!(literal_anchor("杭州", "城市：杭州。"));
        assert!(literal_anchor("API", "The api accepts JSON."));
        assert!(!literal_anchor("api", "rapid matching"));
        assert!(!literal_anchor("how to repair", "how to repair a disk"));
    }

    #[tokio::test]
    async fn best_node_window_survives_fact_position_query_order_and_file_renaming() -> Result<()> {
        for (file, padding) in [
            ("manual.txt", 0),
            ("renamed-notes.txt", 25),
            ("文档.txt", 60),
        ] {
            let directory = tempfile::tempdir()?;
            let source = format!(
                "Saffron paint catalogue.\n{}The saffron capacitor threshold is 17 units.\n{}",
                "The orchard stones remain quiet.\n".repeat(padding),
                "Neutral trailing material.\n".repeat(20)
            );
            fs::write(directory.path().join(file), &source)?;
            index(directory.path(), 10).await?;
            for query in ["saffron capacitor", "capacitor saffron"] {
                let result = search(
                    directory.path(),
                    query,
                    &SearchOptions {
                        mode: "lexical".into(),
                        document: Some(file.into()),
                        ..Default::default()
                    },
                )
                .await?;
                assert_eq!(result.hits.len(), 1);
                let hit = &result.hits[0];
                assert_eq!(hit.path, file);
                assert_eq!(hit.score, 1.0);
                assert!(hit.text.contains("threshold is 17 units"));
                assert_eq!(hit.text, source[hit.byte_start..hit.byte_end]);
                assert_eq!(result.metrics.jev_requests, 0);
                assert!(hit.source_fresh);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn best_node_window_keeps_source_order_ties_and_regex_occurrences() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = format!(
            "saffron capacitor first\nsaffron capacitor overlapping\n{}saffron capacitor last\n",
            "Neutral intervening material.\n".repeat(30)
        );
        fs::write(directory.path().join("notes.txt"), &source)?;
        index(directory.path(), 10).await?;
        let options = SearchOptions {
            mode: "lexical".into(),
            ..Default::default()
        };
        for query in [
            "saffron capacitor",
            "capacitor saffron",
            "saffron capacitor",
        ] {
            let result = search(directory.path(), query, &options).await?;
            assert_eq!(result.hits.len(), 1);
            let hit = &result.hits[0];
            assert_eq!(hit.match_line, Some(1));
            assert_eq!(hit.byte_start, 0);
            assert!(hit.text.contains("first"));
            assert!(hit.text.contains("overlapping"));
            assert!(!hit.text.contains("last"));
        }
        let regex = search(directory.path(), "saffron", &regex_options()).await?;
        assert_eq!(
            regex
                .hits
                .iter()
                .map(|hit| hit.match_line)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(33)]
        );
        assert!(
            regex
                .hits
                .iter()
                .all(|hit| hit.node_id == regex.hits[0].node_id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn best_node_window_preserves_single_term_ties_and_literal_anchor_classification()
    -> Result<()> {
        for exact_first in [false, true] {
            let directory = tempfile::tempdir()?;
            let (first, last) = if exact_first {
                ("api accepts records", "rapid matching")
            } else {
                ("rapid matching", "api accepts records")
            };
            let source = format!("{first}\n{}{last}\n", "Neutral filler.\n".repeat(30));
            fs::write(directory.path().join("guide.txt"), source)?;
            index(directory.path(), 10).await?;
            let result = search(
                directory.path(),
                "api",
                &SearchOptions {
                    mode: "lexical".into(),
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(result.hits.len(), 1);
            assert_eq!(result.hits[0].literal_anchor, exact_first);
            assert!(result.hits[0].text.contains(first));
            assert_eq!(result.hits[0].match_line, Some(1));
        }
        Ok(())
    }

    #[tokio::test]
    async fn best_node_window_preserves_utf8_crlf_cursors_and_hybrid_candidate_budget() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let source = format!(
            "saffron mention\r\n{}{}saffron capacitor threshold is 17 units.{}\r\nTail.\r\n",
            "Neutral filler.\r\n".repeat(30),
            "文🙂".repeat(2000),
            "後".repeat(3000)
        );
        fs::write(directory.path().join("long.txt"), &source)?;
        index(directory.path(), 10).await?;
        for mode in ["lexical", "hybrid"] {
            let options = SearchOptions {
                mode: mode.into(),
                limit: 3,
                ..Default::default()
            };
            let report = if mode == "hybrid" {
                // One indexed document: routing selects it without a call.
                let (client, server) = mock_jev(vec![200]).await?;
                let report =
                    search_with_client(directory.path(), "saffron capacitor", &options, &client)
                        .await?;
                let requests = server.await?;
                assert_eq!(requests.len(), 1);
                let candidates = requests[0]["questions"].as_object().unwrap();
                assert_eq!(candidates.len(), 1); // Same-node semantic dedup keeps the winning lexical window.
                assert!(
                    candidates["candidate_0"]["instructions"]["candidate"]["text"]
                        .as_str()
                        .unwrap()
                        .contains("threshold is 17 units")
                );
                assert_eq!(report.coverage.reranked_candidates, 1);
                assert_eq!(
                    report.coverage.routing_status.as_deref(),
                    Some("skipped_single_document")
                );
                assert_eq!(report.metrics.jev_requests, 1);
                report
            } else {
                search(directory.path(), "saffron capacitor", &options).await?
            };
            assert_eq!(report.hits.len(), 1);
            let hit = &report.hits[0];
            assert!(hit.text.contains("threshold is 17 units"));
            assert!(hit.text.len() <= if mode == "hybrid" { 1400 } else { 4096 });
            assert!(hit.text_truncated);
            assert_eq!(hit.match_line, Some(32));
            assert!(hit.match_column.unwrap() > 1);
            assert_eq!(hit.text, source[hit.byte_start..hit.byte_end]);
            let replay = read_node_window(
                directory.path(),
                &hit.node_id,
                hit.text.len(),
                hit.node_offset.unwrap(),
            )?;
            assert_eq!(replay.text, hit.text);
            assert_eq!(replay.citation, hit.citation);
            assert_eq!(replay.node_coverage, hit.node_coverage);
            assert_eq!(
                (replay.byte_start, replay.byte_end),
                (hit.byte_start, hit.byte_end)
            );
            let next = hit
                .next_offset
                .context("long node should retain continuation")?;
            let continuation = read_node_window(directory.path(), &hit.node_id, 100, next)?;
            assert_eq!(continuation.byte_start, hit.byte_end);
            assert_eq!(
                continuation.text,
                source[continuation.byte_start..continuation.byte_end]
            );
        }
        Ok(())
    }
    #[tokio::test]
    async fn stale_and_failed_rebuild_preserve_generation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("guide.md"), "# Guide\nalpha searchable\n")?;
        let initial = index(dir.path(), 10).await?;
        let found = search(dir.path(), "alpha", &regex_options()).await?;
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].line_start, 2);
        assert_eq!(found.hits[0].coordinate_system, "source_lines");
        fs::write(dir.path().join("guide.md"), "# Changed\nbeta\n")?;
        let stale = search(dir.path(), "alpha", &regex_options()).await?;
        assert!(stale.hits.is_empty());
        assert_eq!(stale.coverage.stale_files, vec!["guide.md"]);
        fs::write(dir.path().join("another.txt"), "other")?;
        assert!(index(dir.path(), 1).await.is_err());
        assert_eq!(
            Snapshot::open(dir.path())?.manifest.generation,
            initial.generation
        );
        Ok(())
    }
    #[tokio::test]
    async fn ignored_and_secret_files_are_not_indexed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "public")?;
        fs::write(dir.path().join("auth.json"), "secret")?;
        fs::write(dir.path().join(".env"), "secret")?;
        fs::write(dir.path().join("ignored.txt"), "secret")?;
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n")?;
        let r = index(dir.path(), 10).await?;
        assert_eq!(r.indexed_files, 1);
        Ok(())
    }
    #[test]
    fn source_path_rejects_escape() {
        assert!(source_path(Path::new("/tmp"), "../secret").is_err());
    }

    #[tokio::test]
    async fn bounded_snippet_keeps_match_and_exact_source_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = format!("{}\r\n{}\r\nneedle\r\n", "p".repeat(5000), "q".repeat(5000));
        fs::write(dir.path().join("long.txt"), &source)?;
        index(dir.path(), 10).await?;
        let result = search(
            dir.path(),
            "needle",
            &SearchOptions {
                context: 2,
                ..regex_options()
            },
        )
        .await?;
        let h = &result.hits[0];
        assert!(h.text.contains("needle"));
        assert_eq!(h.line_start, 3);
        assert_eq!(&source[h.byte_start..h.byte_end], h.text);
        let long_line = format!("{}needle{}\n", "文".repeat(5000), "后".repeat(1000));
        fs::write(dir.path().join("long.txt"), &long_line)?;
        index(dir.path(), 10).await?;
        let result = search(dir.path(), "needle", &regex_options()).await?;
        let h = &result.hits[0];
        assert!(h.text.contains("needle"));
        assert!(h.column_start > 1 && h.text_truncated);
        assert_eq!(&long_line[h.byte_start..h.byte_end], h.text);
        let carriage_returns = "\r".repeat(5000);
        fs::write(dir.path().join("long.txt"), &carriage_returns)?;
        index(dir.path(), 10).await?;
        let result = search(dir.path(), "\r{3}$", &regex_options()).await?;
        let h = &result.hits[0];
        assert!(!h.text.is_empty());
        assert_eq!(&carriage_returns[h.byte_start..h.byte_end], h.text);
        Ok(())
    }

    fn indexed_node_id(root: &Path, file: &str, title: Option<&str>) -> Result<String> {
        let snapshot = Snapshot::open(root)?;
        let document = snapshot
            .manifest
            .documents
            .iter()
            .find(|doc| doc.path == file)
            .context("test document missing")?;
        let node = document
            .nodes
            .iter()
            .find(|node| title.is_none_or(|title| node.title == title))
            .context("test node missing")?;
        Ok(format!("{}:{}", document.id, node.id))
    }

    #[tokio::test]
    async fn node_coverage_distinguishes_unclipped_windows_from_complete_nodes() -> Result<()> {
        for ending in ["\n", "\r\n"] {
            for width in [1, 7] {
                for (before, after, file) in [
                    (0, 0, "single-fact.txt"),
                    (0, 37, "invented-ledger.txt"),
                    (17, 21, "renamed-orbit.txt"),
                    (61, 0, "云灯.txt"),
                ] {
                    let directory = tempfile::tempdir()?;
                    let filler = format!("{}{ending}", "moss 文🙂 ".repeat(width));
                    let fact = "COPPER_OTTER stores 83 glass beads.";
                    let source = format!(
                        "{}{fact}{}{}",
                        filler.repeat(before),
                        if after == 0 { "" } else { ending },
                        filler.repeat(after)
                    );
                    fs::write(directory.path().join(file), &source)?;
                    index(directory.path(), 10).await?;
                    for mode in ["regex", "lexical"] {
                        let report = search(
                            directory.path(),
                            "COPPER_OTTER",
                            &SearchOptions {
                                mode: mode.into(),
                                ..Default::default()
                            },
                        )
                        .await?;
                        assert_eq!(report.hits.len(), 1);
                        assert_eq!(report.metrics.jev_calls_attempted, 0);
                        assert_eq!(report.metrics.jev_requests, 0);
                        let hit = &report.hits[0];
                        assert!(hit.text.contains(fact));
                        assert!(hit.text.len() < 4096);
                        assert!(!hit.text_truncated);
                        assert_eq!(hit.text, source[hit.byte_start..hit.byte_end]);
                        assert_eq!(hit.score, 1.0);
                        assert_eq!(hit.literal_anchor, mode == "lexical");
                        assert!(hit.source_fresh);
                        let expected = NodeCoverage {
                            complete: before == 0 && after == 0,
                            unread_before_bytes: hit.byte_start,
                            unread_after_bytes: source.len() - hit.byte_end,
                        };
                        assert_eq!(hit.node_coverage, Some(expected));
                        assert_eq!(hit.node_offset, Some(expected.unread_before_bytes));
                        if after == 0 {
                            assert_eq!(expected.unread_before_bytes > 0, before > 0);
                            assert_eq!(expected.unread_after_bytes, 0);
                            assert_eq!(hit.next_offset, None);
                        } else {
                            assert!(expected.unread_after_bytes > 0);
                            assert_eq!(hit.next_offset, Some(hit.byte_end));
                        }
                        let mut serialized = serde_json::to_value(hit)?;
                        assert_eq!(serialized["node_coverage"], serde_json::to_value(expected)?);
                        let roundtrip: Hit = serde_json::from_value(serialized.clone())?;
                        assert_eq!(roundtrip.node_coverage, hit.node_coverage);
                        serialized.as_object_mut().unwrap().remove("node_coverage");
                        let legacy: Hit = serde_json::from_value(serialized)?;
                        assert_eq!(legacy.node_coverage, None);
                        let replay = read_node_window(
                            directory.path(),
                            &hit.node_id,
                            hit.text.len(),
                            hit.node_offset.unwrap(),
                        )?;
                        assert_eq!(replay.text, hit.text);
                        assert_eq!(replay.citation, hit.citation);
                        assert_eq!(replay.node_coverage, hit.node_coverage);
                        let full = read_node(directory.path(), &hit.node_id, 65536)?;
                        assert_eq!(full.text, source);
                        assert_eq!(full.next_offset, None);
                        assert_eq!(
                            full.node_coverage,
                            Some(NodeCoverage {
                                complete: true,
                                unread_before_bytes: 0,
                                unread_after_bytes: 0,
                            })
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn continuation_reconstructs_headingless_text_larger_than_64kib() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = format!("{}final 文🙂\r\n", "alpha 文🙂\r\n".repeat(9000));
        assert!(source.len() > 65536);
        fs::write(directory.path().join("large.txt"), &source)?;
        index(directory.path(), 10).await?;
        let id = indexed_node_id(directory.path(), "large.txt", None)?;
        let mut reconstructed = String::new();
        let mut offset = 0;
        let mut windows = 0;
        loop {
            let budget = if offset == 0 { 65536 } else { 6144 };
            let window = read_node_window(directory.path(), &id, budget, offset)?;
            assert_eq!(window.node_offset, Some(offset));
            assert!(!window.text.is_empty());
            assert!(window.text.len() <= budget);
            assert_eq!(window.byte_start, offset);
            assert_eq!(window.byte_end, offset + window.text.len());
            assert_eq!(window.text, source[window.byte_start..window.byte_end]);
            assert_eq!(
                window.node_coverage,
                Some(NodeCoverage {
                    complete: false,
                    unread_before_bytes: offset,
                    unread_after_bytes: source.len() - window.byte_end,
                })
            );
            assert_eq!(
                window.line_start,
                source[..window.byte_start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1
            );
            let prefix = &source[..window.byte_end];
            let end_line = prefix.bytes().filter(|byte| *byte == b'\n').count()
                + usize::from(!prefix.ends_with('\n'));
            assert_eq!(window.line_end, end_line);
            assert_eq!((window.page_start, window.page_end), (1, 1));
            let replay = read_node_window(directory.path(), &id, window.text.len(), offset)?;
            assert_eq!(replay.text, window.text);
            assert_eq!(
                (replay.byte_start, replay.byte_end),
                (window.byte_start, window.byte_end)
            );
            reconstructed.push_str(&window.text);
            windows += 1;
            match window.next_offset {
                Some(next) => {
                    assert_eq!(next, window.byte_end);
                    assert!(next > offset, "continuation stalled");
                    offset = next;
                }
                None => break,
            }
        }
        assert!(windows > 1);
        assert_eq!(reconstructed, source);
        let eof = read_node_window(directory.path(), &id, 4, source.len())?;
        assert_eq!(eof.node_offset, Some(source.len()));
        assert_eq!(eof.next_offset, None);
        assert_eq!(
            eof.node_coverage,
            Some(NodeCoverage {
                complete: false,
                unread_before_bytes: source.len(),
                unread_after_bytes: 0,
            })
        );
        assert!(eof.text.is_empty());
        assert_eq!((eof.byte_start, eof.byte_end), (source.len(), source.len()));
        assert_eq!(eof.line_start, source.lines().count());
        assert_eq!(eof.line_end, source.lines().count());
        Ok(())
    }

    #[tokio::test]
    async fn node_windows_preserve_crlf_and_reject_invalid_utf8_offsets() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = "# First\r\n甲🙂\r\n\r\n# Second\r\n终\r\n";
        fs::write(directory.path().join("sections.md"), source)?;
        index(directory.path(), 10).await?;
        let id = indexed_node_id(directory.path(), "sections.md", Some("First"))?;
        let end = source.find("# Second").unwrap();
        let full = read_node(directory.path(), &id, 65536)?;
        assert_eq!(full.text, source[..end]);
        assert_eq!(full.node_offset, Some(0));
        assert_eq!(full.next_offset, None);
        assert!(!full.text_truncated);
        assert_eq!(
            full.node_coverage,
            Some(NodeCoverage {
                complete: true,
                unread_before_bytes: 0,
                unread_after_bytes: 0,
            })
        );
        let mut combined = String::new();
        let mut offset = 0;
        loop {
            let window = read_node_window(directory.path(), &id, 4, offset)?;
            combined.push_str(&window.text);
            match window.next_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(combined, source[..end]);
        let cr = source.find("\r\n").unwrap();
        let carriage = read_node_window(directory.path(), &id, 1, cr)?;
        let newline = read_node_window(directory.path(), &id, 1, cr + 1)?;
        assert_eq!(carriage.text, "\r");
        assert_eq!(newline.text, "\n");
        assert_eq!((carriage.line_start, carriage.line_end), (1, 1));
        assert_eq!((newline.line_start, newline.line_end), (1, 1));
        assert_eq!(carriage.column_start, cr + 1);
        assert_eq!(newline.column_start, cr + 2);
        let chinese = source.find('甲').unwrap();
        assert!(
            read_node_window(directory.path(), &id, 4, chinese + 1)
                .unwrap_err()
                .to_string()
                .contains("UTF-8 boundary")
        );
        assert!(
            read_node_window(directory.path(), &id, 2, chinese)
                .unwrap_err()
                .to_string()
                .contains("requires 3 bytes")
        );
        assert!(read_node_window(directory.path(), &id, 4, end + 1).is_err());
        assert!(read_node_window(directory.path(), &id, 4, usize::MAX).is_err());
        assert!(read_node_window(directory.path(), &id, 0, 0).is_err());
        assert!(read_node_window(directory.path(), &id, 65537, 0).is_err());
        let eof = read_node_window(directory.path(), &id, 4, end)?;
        assert!(eof.text.is_empty());
        assert_eq!((eof.line_start, eof.line_end), (3, 3));
        assert_eq!(eof.next_offset, None);
        Ok(())
    }

    #[tokio::test]
    async fn blank_node_reads_return_exact_delimiters_and_explicit_eof() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("blank.txt"), "\r\n\r\n")?;
        fs::write(directory.path().join("empty.txt"), "")?;
        index(directory.path(), 10).await?;
        let id = indexed_node_id(directory.path(), "blank.txt", None)?;
        let mut output = String::new();
        for offset in 0..4 {
            let window = read_node_window(directory.path(), &id, 1, offset)?;
            assert_eq!(window.text.len(), 1);
            assert_eq!(window.node_offset, Some(offset));
            assert_eq!(window.next_offset, (offset < 3).then_some(offset + 1));
            assert_eq!(
                window.node_coverage,
                Some(NodeCoverage {
                    complete: false,
                    unread_before_bytes: offset,
                    unread_after_bytes: 3 - offset,
                })
            );
            output.push_str(&window.text);
        }
        assert_eq!(output, "\r\n\r\n");
        let eof = read_node_window(directory.path(), &id, 1, 4)?;
        assert!(eof.text.is_empty());
        assert_eq!(eof.next_offset, None);
        assert_eq!(eof.line_start, 2);
        let snapshot = Snapshot::open(directory.path())?;
        let empty = snapshot
            .manifest
            .documents
            .iter()
            .find(|doc| doc.path == "empty.txt")
            .unwrap();
        assert!(empty.nodes.is_empty());
        assert!(read_node_window(directory.path(), &format!("{}:0000", empty.id), 4, 0).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn search_cursors_replay_contained_hits_without_clipping_grep_context() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = "# One\nfirst\n## Nested\nneedle\n# Two\ntail\n";
        fs::write(directory.path().join("sections.md"), source)?;
        index(directory.path(), 10).await?;
        let contextual = search(
            directory.path(),
            "needle",
            &SearchOptions {
                context: 3,
                ..regex_options()
            },
        )
        .await?;
        let context = &contextual.hits[0];
        assert_eq!((context.line_start, context.line_end), (1, 6));
        assert_eq!(context.text, source.trim_end_matches('\n'));
        assert_eq!(context.node_offset, None);
        assert_eq!(context.next_offset, None);
        assert_eq!(context.node_coverage, None);
        assert!(serde_json::to_value(context)?["node_coverage"].is_null());
        let direct = search(directory.path(), "needle", &regex_options()).await?;
        let hit = &direct.hits[0];
        let node_start = source.find("## Nested").unwrap();
        assert_eq!(hit.node_offset, Some(hit.byte_start - node_start));
        assert_eq!(hit.next_offset, Some(hit.byte_end - node_start));
        assert_eq!(
            hit.node_coverage,
            Some(NodeCoverage {
                complete: false,
                unread_before_bytes: "## Nested\n".len(),
                unread_after_bytes: "\n".len(),
            })
        );
        let replay = read_node_window(
            directory.path(),
            &hit.node_id,
            hit.byte_end - hit.byte_start,
            hit.node_offset.unwrap(),
        )?;
        assert_eq!(replay.text, hit.text);
        assert_eq!(
            (replay.byte_start, replay.byte_end),
            (hit.byte_start, hit.byte_end)
        );
        assert_eq!(
            (replay.line_start, replay.line_end),
            (hit.line_start, hit.line_end)
        );
        assert_eq!(replay.citation, hit.citation);
        assert_eq!(replay.source_sha256, hit.source_sha256);
        Ok(())
    }

    #[tokio::test]
    async fn continuation_rechecks_changed_and_deleted_sources() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("large.txt");
        fs::write(&source, "content".repeat(10000))?;
        index(directory.path(), 10).await?;
        let id = indexed_node_id(directory.path(), "large.txt", None)?;
        let first = read_node_window(directory.path(), &id, 6144, 0)?;
        let offset = first
            .next_offset
            .context("fixture should need continuation")?;
        fs::write(&source, "changed")?;
        assert!(
            read_node_window(directory.path(), &id, 6144, offset)
                .unwrap_err()
                .to_string()
                .contains("source changed or disappeared")
        );
        fs::remove_file(&source)?;
        assert!(
            read_node_window(directory.path(), &id, 6144, offset)
                .unwrap_err()
                .to_string()
                .contains("source changed or disappeared")
        );
        Ok(())
    }

    #[test]
    fn byte_windows_report_native_physical_pages_and_extracted_lines() -> Result<()> {
        let parsed = gptgrep_pageindex::from_pages(
            "report",
            &[
                gptgrep_pageindex::PageInput {
                    number: 1,
                    text: "第一页".into(),
                },
                gptgrep_pageindex::PageInput {
                    number: 2,
                    text: "Second".into(),
                },
                gptgrep_pageindex::PageInput {
                    number: 3,
                    text: "末页".into(),
                },
            ],
            &[gptgrep_pageindex::Heading {
                title: "第一页".into(),
                level: 1,
                page: 1,
                line: 1,
            }],
            &[],
            "liteparse-test",
        )?;
        let doc = Document {
            id: "test-document".into(),
            path: "report.pdf".into(),
            source_sha256: "test-hash".into(),
            source_bytes: 0,
            text_sha256: digest(parsed.text.as_bytes()),
            parser: parsed.parser,
            title: parsed.title,
            warnings: parsed.warnings,
            nodes: parsed.nodes,
            pages: parsed
                .pages
                .iter()
                .map(|page| PageSpan {
                    number: page.number,
                    line_start: page.line_start,
                    line_end: page.line_end,
                })
                .collect(),
        };
        let view = TextView::new(parsed.text);
        let start = view.lines[1].0;
        let hit = hit_from_bytes(
            &doc,
            &doc.nodes[0],
            &view,
            ByteWindow {
                start,
                end: view.text.len(),
                truncated: true,
            },
            None,
            1.0,
        );
        assert_eq!((hit.line_start, hit.line_end), (2, 3));
        assert_eq!((hit.page_start, hit.page_end), (2, 3));
        assert_eq!(hit.node_offset, Some(start));
        assert_eq!(hit.next_offset, None);
        assert_eq!(hit.coordinate_system, "extracted_lines_and_source_pages");
        assert_eq!(hit.citation, "report.pdf:p2-p3");
        assert_eq!(hit.text, "Second\n末页\n");
        Ok(())
    }

    #[tokio::test]
    async fn empty_plaintext_remains_a_valid_indexed_document() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("empty.txt"), "")?;
        assert_eq!(index(dir.path(), 10).await?.indexed_files, 1);
        assert!(
            search(dir.path(), "needle", &regex_options())
                .await?
                .hits
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicitly_selected_hidden_root_can_be_indexed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join(".private-documents");
        fs::create_dir(&root)?;
        fs::write(root.join("public.txt"), "needle")?;
        fs::write(root.join(".env"), "not admitted")?;
        assert_eq!(index(&root, 10).await?.indexed_files, 1);
        assert_eq!(
            search(&root, "needle", &regex_options()).await?.hits.len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn damaged_manifest_is_rejected_even_with_updated_digest() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello\nworld\n")?;
        index(dir.path(), 10).await?;
        let snapshot = Snapshot::open(dir.path())?;
        let manifest_path = snapshot.dir.join("manifest.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        value["documents"][0]["nodes"][0]["line_end"] = json!(999);
        let bytes = serde_json::to_vec(&value)?;
        fs::write(&manifest_path, &bytes)?;
        let pointer_path = dir.path().join(STATE).join("CURRENT.json");
        let mut pointer: Pointer = serde_json::from_slice(&fs::read(&pointer_path)?)?;
        pointer.manifest_sha256 = digest(&bytes);
        fs::write(pointer_path, serde_json::to_vec(&pointer)?)?;
        assert!(Snapshot::open(dir.path()).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn index_generation_symlink_cannot_redirect_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello")?;
        fs::create_dir(dir.path().join(STATE))?;
        std::os::unix::fs::symlink(outside.path(), dir.path().join(STATE).join("generations"))?;
        assert!(index(dir.path(), 10).await.is_err());
        assert!(fs::read_dir(outside.path())?.next().is_none());
        Ok(())
    }
}
