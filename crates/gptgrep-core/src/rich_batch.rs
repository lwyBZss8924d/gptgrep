//! Local and explicitly paid batch operations over one retained v2 generation.
//! Inspection returns indexed metadata, not fresh source evidence. Native/OCR
//! text windows preserve the reader's typed evidence and freshness checks.
//! Empty EOF windows contain no citeable bytes; evidence consumers still use
//! the retained reader's nonempty evidence validation boundary.

use crate::{
    GenerationPin, ReaderCacheBudget, ReaderReadMetrics, RichDocument, RichGenerationReader,
    VisualCandidate, VisualCandidateRequest, VisualGenerationReader, VisualScope,
};
use gptgrep_parse::{ArtifactRef, NativePage};
use serde::{
    Deserialize, Serialize,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Component, Path},
    time::Instant,
};

pub const RICH_BATCH_REQUEST_SCHEMA: &str = "gptgrep.batch.rich.request.v1";
pub const RICH_BATCH_RESPONSE_SCHEMA: &str = "gptgrep.batch.rich.response.v1";
pub const MAX_RICH_BATCH_OPERATIONS: usize = 64;
pub const MAX_RICH_BATCH_INPUT_BYTES: usize = 1_048_576;
pub const MAX_RICH_BATCH_OUTPUT_BYTES: usize = 1_048_576;
pub const MAX_RICH_BATCH_CURSOR_BYTES: usize = 16_384;
const ENVELOPE_RESERVE: usize = 4096;
const MIN_ITEM_OUTPUT: usize = 1536;

mod paid;
pub use paid::{
    PreparedRichBatch, RichBatchDecisionsMetrics, RichBatchItemDecisions,
    dispatch_rich_batch_with_client, dispatch_rich_batch_with_reader_and_client,
    prepare_rich_batch_with_reader,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichBatchRequest {
    pub schema_version: String,
    #[serde(default)]
    pub generation: RichBatchGeneration,
    #[serde(default)]
    pub budget: RichBatchBudget,
    pub operations: Vec<RichBatchOperation>,
}
impl RichBatchRequest {
    pub fn requires_model(&self) -> bool {
        self.operations
            .iter()
            .any(RichBatchOperation::requires_model)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RichBatchGeneration {
    Current(CurrentRichGeneration),
    Exact(GenerationPin),
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum CurrentRichGeneration {
    #[default]
    #[serde(rename = "CURRENT")]
    Current,
}
impl Default for RichBatchGeneration {
    fn default() -> Self {
        Self::Current(CurrentRichGeneration::Current)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RichBatchBudget {
    pub max_operations: usize,
    pub max_output_bytes: usize,
    pub cache: ReaderCacheBudget,
    /// Whole-batch worst-case logical calls. Does not reset the parent context.
    pub max_decisions_submissions: u32,
}
impl Default for RichBatchBudget {
    fn default() -> Self {
        Self {
            max_operations: 64,
            max_output_bytes: MAX_RICH_BATCH_OUTPUT_BYTES,
            cache: ReaderCacheBudget::default(),
            max_decisions_submissions: 8,
        }
    }
}
fn default_page_size() -> usize {
    100
}
fn default_read_bytes() -> usize {
    4096
}
fn default_search_page_size() -> usize {
    10
}
fn default_max_hits() -> usize {
    20
}
fn default_visual_candidates() -> usize {
    16
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum RichBatchOperation {
    /// Score only these exact source references; no routing, cursor or dependency.
    Rerank {
        id: String,
        query: String,
        candidates: Vec<crate::RichRerankCandidate>,
        #[serde(default = "default_multimodal_concurrency")]
        max_concurrent_batches: usize,
    },
    InspectDocuments {
        id: String,
        #[serde(default)]
        fields: Vec<String>,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    InspectPages {
        id: String,
        document: String,
        #[serde(default)]
        fields: Vec<String>,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Fresh structural page metadata. Never citation or image delivery authority.
    InspectNativeMetadata {
        id: String,
        artifact_id: String,
        #[serde(default)]
        fields: Vec<String>,
        collection: crate::NativeMetadataCollection,
        #[serde(default)]
        item_fields: Vec<String>,
        #[serde(default)]
        object_id: Option<String>,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default = "default_native_metadata_output")]
        max_output_bytes: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    ReadArtifact {
        id: String,
        artifact_id: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_read_bytes")]
        max_bytes: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Pagination covers this bounded result window. Truncation at max_hits is
    /// explicit, and no cursor promises matches beyond the reader's limit.
    SearchArtifacts {
        id: String,
        query: String,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default)]
        literal: bool,
        #[serde(default)]
        document: Option<String>,
        #[serde(default = "default_max_hits")]
        max_hits: usize,
        #[serde(default = "default_search_page_size")]
        page_size: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Local visual metadata; its continuation belongs to the visual reader.
    InspectVisual {
        id: String,
        #[serde(default)]
        scope: VisualScope,
        #[serde(default)]
        query: String,
        #[serde(default = "default_visual_candidates")]
        max_candidates: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// One complete locally verified rendition, never a partial image window.
    ReadVisual { id: String, rendition_id: String },
    /// Required document routing and source-bound text/visual reranking.
    /// No inference cursor or implicit reference to another operation.
    SearchMultimodal {
        id: String,
        query: String,
        #[serde(default)]
        document: Option<String>,
        #[serde(default = "default_multimodal_window")]
        window_bytes: usize,
        #[serde(default = "default_multimodal_visual")]
        max_visual_candidates: usize,
        #[serde(default = "default_multimodal_concurrency")]
        max_concurrent_batches: usize,
        #[serde(default = "default_multimodal_score")]
        min_score: f64,
    },
}
fn default_multimodal_window() -> usize {
    1024
}
fn default_native_metadata_output() -> usize {
    MAX_RICH_BATCH_OUTPUT_BYTES
}
fn default_multimodal_visual() -> usize {
    8
}
fn default_multimodal_concurrency() -> usize {
    4
}
fn default_multimodal_score() -> f64 {
    0.5
}
impl RichBatchOperation {
    fn requires_model(&self) -> bool {
        matches!(self, Self::SearchMultimodal { .. } | Self::Rerank { .. })
    }
    pub fn id(&self) -> &str {
        match self {
            Self::InspectDocuments { id, .. }
            | Self::InspectPages { id, .. }
            | Self::InspectNativeMetadata { id, .. }
            | Self::ReadArtifact { id, .. }
            | Self::SearchArtifacts { id, .. }
            | Self::InspectVisual { id, .. }
            | Self::ReadVisual { id, .. }
            | Self::SearchMultimodal { id, .. }
            | Self::Rerank { id, .. } => id,
        }
    }
    fn binding(&self) -> (&str, &str, Value, Option<&str>) {
        match self {
            Self::InspectDocuments {
                fields,
                page_size,
                cursor,
                ..
            } => (
                "inspect_documents",
                "indexed_documents",
                serde_json::json!({"fields":fields,"page_size":page_size}),
                cursor.as_deref(),
            ),
            Self::InspectPages {
                document,
                fields,
                page_size,
                cursor,
                ..
            } => (
                "inspect_pages",
                document,
                serde_json::json!({"fields":fields,"page_size":page_size}),
                cursor.as_deref(),
            ),
            Self::ReadArtifact {
                artifact_id,
                offset,
                max_bytes,
                cursor,
                ..
            } => (
                "read_artifact",
                artifact_id,
                serde_json::json!({"offset":offset,"max_bytes":max_bytes}),
                cursor.as_deref(),
            ),
            Self::SearchArtifacts {
                query,
                case_insensitive,
                literal,
                document,
                max_hits,
                page_size,
                cursor,
                ..
            } => (
                "search_artifacts",
                document.as_deref().unwrap_or("all_documents"),
                serde_json::json!({"query":query,"case_insensitive":case_insensitive,"literal":literal,"document":document,"max_hits":max_hits,"page_size":page_size}),
                cursor.as_deref(),
            ),
            // These operations never use the text/metadata cursor codec.
            Self::InspectNativeMetadata { .. }
            | Self::InspectVisual { .. }
            | Self::ReadVisual { .. }
            | Self::SearchMultimodal { .. }
            | Self::Rerank { .. } => ("visual", "visual", Value::Null, None),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RichBatchErrorCode {
    InputIoFailure,
    InvalidRequest,
    DuplicateOperationId,
    InvalidGeneration,
    InvalidFields,
    InvalidRegex,
    InvalidCursor,
    InvalidOperation,
    ResourceNotFound,
    SourceStale,
    SharedIntegrity,
    OutputBudgetInsufficient,
    NotExecuted,
    ModelContextRequired,
    SubmissionBudgetInsufficient,
    ProviderFailure,
    PaidSearchFailure,
    PaidRerankFailure,
    DeadlineExceeded,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchError {
    pub code: RichBatchErrorCode,
    pub message: String,
}
impl std::fmt::Display for RichBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RichBatchError {}
fn error(code: RichBatchErrorCode, message: &str) -> RichBatchError {
    RichBatchError {
        code,
        message: message.into(),
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RichBatchStatus {
    Ok,
    Error,
    NotExecuted,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RichBatchItemCoverage {
    pub unit: String,
    pub available: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub selected: usize,
    pub exhausted: bool,
    pub budget_truncated: bool,
    pub search_ceiling_reached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
/// Immutable owned JSON. Clones share the actual allocation, so a retained
/// metadata admission lease cannot outlive an unaccounted deep-cloned payload.
#[derive(Debug, Clone)]
pub struct RichBatchValue(std::sync::Arc<Value>, Option<crate::NativeMetadataLease>);
// Admission leases describe ownership; equality retains JSON value semantics.
impl PartialEq for RichBatchValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for RichBatchValue {}
impl From<Value> for RichBatchValue {
    fn from(value: Value) -> Self {
        Self(std::sync::Arc::new(value), None)
    }
}
impl std::ops::Deref for RichBatchValue {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}
impl Serialize for RichBatchValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for RichBatchValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Into::into)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchItem {
    pub id: String,
    pub status: RichBatchStatus,
    pub reserved_output_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RichBatchValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RichBatchError>,
    pub coverage: RichBatchItemCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_metrics: Option<ReaderReadMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decisions: Option<paid::RichBatchItemDecisions>,
    /// Retained owned metadata JSON stays charged across item/response clones.
    #[serde(skip)]
    pub(crate) metadata_lease: Option<crate::NativeMetadataLease>,
}
impl RichBatchItem {
    fn failed(id: &str, quota: usize, failure: RichBatchError) -> Self {
        Self {
            id: id.into(),
            status: RichBatchStatus::Error,
            reserved_output_bytes: quota,
            result: None,
            error: Some(failure),
            coverage: Default::default(),
            read_metrics: None,
            decisions: None,
            metadata_lease: None,
        }
    }
    fn pending(id: &str, quota: usize) -> Self {
        let mut item = Self::failed(
            id,
            quota,
            error(
                RichBatchErrorCode::NotExecuted,
                "Operation was not executed",
            ),
        );
        item.status = RichBatchStatus::NotExecuted;
        item
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchExecution {
    pub scope: String,
    /// None when exact batch-attributable submissions are unavailable.
    pub model_submissions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<gptgrep_jev::ExecutionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<gptgrep_jev::ExecutionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_observation_scope: Option<String>,
}
impl Default for RichBatchExecution {
    fn default() -> Self {
        Self {
            scope: "local_deterministic_no_model_context".into(),
            model_submissions: Some(0),
            context_before: None,
            context_after: None,
            context_observation_scope: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchCoverage {
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub not_executed: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchMetrics {
    pub elapsed_ms: u128,
    pub shared_reader: bool,
    pub physical_bytes: Option<u64>,
    pub model_submissions: Option<u32>,
    pub read_metrics: Option<ReaderReadMetrics>,
    pub cache_status: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decisions: Option<paid::RichBatchDecisionsMetrics>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchResponse {
    pub schema_version: String,
    pub generation: GenerationPin,
    pub execution: RichBatchExecution,
    pub items: Vec<RichBatchItem>,
    pub coverage: RichBatchCoverage,
    pub metrics: RichBatchMetrics,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchFailure {
    pub schema_version: String,
    pub generation: Option<GenerationPin>,
    pub execution: RichBatchExecution,
    pub error: RichBatchError,
    pub items: Vec<RichBatchItem>,
    pub read_metrics: Option<ReaderReadMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decisions: Option<paid::RichBatchDecisionsMetrics>,
}
impl std::fmt::Display for RichBatchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error.message)
    }
}
impl std::error::Error for RichBatchFailure {}
fn failure(
    e: RichBatchError,
    pin: Option<GenerationPin>,
    items: Vec<RichBatchItem>,
) -> Box<RichBatchFailure> {
    Box::new(RichBatchFailure {
        schema_version: RICH_BATCH_RESPONSE_SCHEMA.into(),
        generation: pin,
        execution: Default::default(),
        error: e,
        items,
        read_metrics: None,
        decisions: None,
    })
}

/// Bounded diagnostic for unavailable stdin/file input. No input paths or OS
/// errors are included and no generation or model context has been opened.
pub fn rich_batch_input_io_failure() -> RichBatchFailure {
    *failure(
        error(
            RichBatchErrorCode::InputIoFailure,
            "Rich batch input could not be read",
        ),
        None,
        vec![],
    )
}

struct ByteCounter {
    bytes: usize,
    limit: usize,
}
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("serialized byte admission exceeded"))?;
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn measure(value: &impl Serialize, limit: usize) -> Result<usize, RichBatchError> {
    let mut counter = ByteCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| {
        error(
            RichBatchErrorCode::OutputBudgetInsufficient,
            "Complete serialized object exceeds reserved output",
        )
    })?;
    Ok(counter.bytes)
}
fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("bounded serializable cursor options"))
    )
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_pin(pin: &GenerationPin) -> bool {
    !pin.generation.is_empty()
        && pin.generation.len() <= 256
        && pin
            .generation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && valid_digest(&pin.manifest_sha256)
}
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

pub fn parse_rich_batch_request(bytes: &[u8]) -> Result<RichBatchRequest, Box<RichBatchFailure>> {
    if bytes.len() > MAX_RICH_BATCH_INPUT_BYTES {
        return Err(failure(
            error(
                RichBatchErrorCode::InvalidRequest,
                "Rich batch input exceeds1MiB",
            ),
            None,
            vec![],
        ));
    }
    let value = gptgrep_jev::parse_unique_json(bytes).map_err(|_| {
        failure(
            error(
                RichBatchErrorCode::InvalidRequest,
                "Rich batch JSON schema is invalid",
            ),
            None,
            vec![],
        )
    })?;
    let request: RichBatchRequest = serde_json::from_value(value).map_err(|_| {
        failure(
            error(
                RichBatchErrorCode::InvalidRequest,
                "Rich batch JSON schema is invalid",
            ),
            None,
            vec![],
        )
    })?;
    validate_request(&request).map_err(|e| failure(e, None, vec![]))?;
    Ok(request)
}
fn validate_request(request: &RichBatchRequest) -> Result<(), RichBatchError> {
    if request.schema_version != RICH_BATCH_REQUEST_SCHEMA
        || request.operations.is_empty()
        || request.operations.len() > MAX_RICH_BATCH_OPERATIONS
        || !(1..=64).contains(&request.budget.max_operations)
        || request.operations.len() > request.budget.max_operations
        || request.budget.max_output_bytes > MAX_RICH_BATCH_OUTPUT_BYTES
        || request.budget.max_decisions_submissions > gptgrep_jev::MAX_SUBMISSIONS
        || request.budget.cache.max_bytes == 0
        || request.budget.cache.max_bytes > crate::MAX_READER_CACHE_BYTES
        || request.budget.cache.max_entries == 0
        || request.budget.cache.max_entries > crate::MAX_READER_CACHE_ENTRIES
    {
        return Err(error(
            RichBatchErrorCode::InvalidRequest,
            "Rich batch schema or declared resource budget is invalid",
        ));
    }
    if request.budget.max_output_bytes
        < ENVELOPE_RESERVE + request.operations.len() * MIN_ITEM_OUTPUT
    {
        return Err(error(
            RichBatchErrorCode::OutputBudgetInsufficient,
            "Output budget cannot reserve complete responses for every operation",
        ));
    }
    measure(request, MAX_RICH_BATCH_INPUT_BYTES).map_err(|_| {
        error(
            RichBatchErrorCode::InvalidRequest,
            "Rich batch input exceeds1MiB",
        )
    })?;
    if let RichBatchGeneration::Exact(pin) = &request.generation
        && !valid_pin(pin)
    {
        return Err(error(
            RichBatchErrorCode::InvalidGeneration,
            "Exact generation pin is invalid",
        ));
    }
    let mut ids = BTreeSet::new();
    for op in &request.operations {
        validate_native_operation(op)?;
        let id = op.id();
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
            return Err(error(
                RichBatchErrorCode::InvalidRequest,
                "Operation ID is invalid",
            ));
        }
        if !ids.insert(id) {
            return Err(error(
                RichBatchErrorCode::DuplicateOperationId,
                "Operation IDs must be unique",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorBody {
    schema: String,
    pin: GenerationPin,
    lane: String,
    scope: String,
    options_sha256: String,
    position: usize,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    body: CursorBody,
    sha256: String,
}
fn cursor_body(pin: &GenerationPin, op: &RichBatchOperation, position: usize) -> CursorBody {
    let (lane, scope, options, _) = op.binding();
    CursorBody {
        schema: "gptgrep.batch.rich.cursor.v1".into(),
        pin: pin.clone(),
        lane: lane.into(),
        scope: scope.into(),
        options_sha256: digest(&options),
        position,
    }
}
fn cursor(pin: &GenerationPin, op: &RichBatchOperation, position: usize) -> String {
    let body = cursor_body(pin, op, position);
    let hash = digest(&body);
    let bytes = serde_json::to_vec(&Cursor { body, sha256: hash }).expect("bounded cursor DTO");
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut token, "{byte:02x}").expect("String write");
    }
    token
}
fn position(pin: &GenerationPin, op: &RichBatchOperation) -> Result<usize, RichBatchError> {
    let (lane, scope, options, token) = op.binding();
    let Some(token) = token else {
        return Ok(match op {
            RichBatchOperation::ReadArtifact { offset, .. } => *offset,
            _ => 0,
        });
    };
    let bad = || {
        error(
            RichBatchErrorCode::InvalidCursor,
            "Cursor does not match generation, scope or options",
        )
    };
    if token.len() > MAX_RICH_BATCH_CURSOR_BYTES
        || token.len() % 2 != 0
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad());
    }
    let bytes: Result<Vec<u8>, _> = token
        .as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).expect("ASCII checked"), 16))
        .collect();
    let value: Cursor = serde_json::from_slice(&bytes.map_err(|_| bad())?).map_err(|_| bad())?;
    if value.body.schema != "gptgrep.batch.rich.cursor.v1"
        || value.body.pin != *pin
        || value.body.lane != lane
        || value.body.scope != scope
        || value.body.options_sha256 != digest(&options)
        || value.sha256 != digest(&value.body)
    {
        return Err(bad());
    }
    Ok(value.body.position)
}
const DOCUMENT_FIELDS: &[&str] = &[
    "id",
    "path",
    "title",
    "source_sha256",
    "source_bytes",
    "parser",
    "pages",
    "nodes",
    "warnings",
    "prepared",
];
const PAGE_FIELDS: &[&str] = &[
    "page",
    "label",
    "width_points",
    "height_points",
    "geometry",
    "coverage",
    "complexity",
    "layout",
    "capabilities",
    "native_text",
    "metadata_artifact",
    "ocr_text",
    "renditions",
];
fn fields_valid(fields: &[String], allow: &[&str]) -> Result<(), RichBatchError> {
    let mut seen = BTreeSet::new();
    if fields.len() > 32
        || fields
            .iter()
            .any(|f| f.len() > 64 || !allow.contains(&f.as_str()) || !seen.insert(f))
    {
        return Err(error(
            RichBatchErrorCode::InvalidFields,
            "Field selection contains unsupported or duplicate fields",
        ));
    }
    Ok(())
}
fn preflight(pin: &GenerationPin, op: &RichBatchOperation) -> Result<usize, RichBatchError> {
    let bad = || {
        error(
            RichBatchErrorCode::InvalidOperation,
            "Operation options exceed declared bounds",
        )
    };
    match op {
        RichBatchOperation::Rerank {
            query,
            candidates,
            max_concurrent_batches,
            ..
        } => {
            let request = crate::RichRerankRequest {
                schema_version: "gptgrep.rich_rerank.request.v1".into(),
                generation: RichBatchGeneration::Exact(pin.clone()),
                query: query.clone(),
                candidates: candidates.clone(),
                budget: crate::RichRerankBudget {
                    max_concurrent_batches: *max_concurrent_batches,
                    ..Default::default()
                },
            };
            crate::rich_multimodal::validate_rich_rerank_request(&request).map_err(|_| bad())?;
            return Ok(0);
        }
        RichBatchOperation::InspectNativeMetadata { .. } => {
            validate_native_operation(op)?;
            return Ok(0);
        }
        RichBatchOperation::InspectDocuments {
            fields, page_size, ..
        } => {
            fields_valid(fields, DOCUMENT_FIELDS)?;
            if !(1..=1000).contains(page_size) {
                return Err(bad());
            }
        }
        RichBatchOperation::InspectPages {
            document,
            fields,
            page_size,
            ..
        } => {
            fields_valid(fields, PAGE_FIELDS)?;
            if !valid_path(document) || !(1..=1000).contains(page_size) {
                return Err(bad());
            }
        }
        RichBatchOperation::ReadArtifact {
            artifact_id,
            max_bytes,
            ..
        } => {
            if artifact_id.is_empty() || artifact_id.len() > 512 || !(1..=65536).contains(max_bytes)
            {
                return Err(bad());
            }
        }
        RichBatchOperation::SearchArtifacts {
            query,
            case_insensitive,
            literal,
            document,
            max_hits,
            page_size,
            ..
        } => {
            if query.is_empty()
                || query.len() > 16_384
                || document.as_deref().is_some_and(|s| !valid_path(s))
                || !(1..=1000).contains(max_hits)
                || !(1..=1000).contains(page_size)
                || page_size > max_hits
            {
                return Err(bad());
            }
            let pattern = if *literal {
                regex::escape(query)
            } else {
                query.clone()
            };
            regex::RegexBuilder::new(&pattern)
                .case_insensitive(*case_insensitive)
                .build()
                .map_err(|_| {
                    error(
                        RichBatchErrorCode::InvalidRegex,
                        "Regular expression is invalid",
                    )
                })?;
        }
        RichBatchOperation::InspectVisual {
            scope,
            query,
            max_candidates,
            cursor,
            ..
        } => {
            let mut ids = BTreeSet::new();
            if query.len() > 65_536
                || !(1..=256).contains(max_candidates)
                || cursor.as_ref().is_some_and(|v| v.len() > 4096)
                || scope
                    .document_ids
                    .iter()
                    .any(|id| id.is_empty() || id.len() > 256 || !ids.insert(id))
                || scope.first_page == Some(0)
                || scope.last_page == Some(0)
                || scope
                    .last_page
                    .is_some_and(|p| p < scope.first_page.unwrap_or(1))
                || measure(scope, 65_536).is_err()
            {
                return Err(bad());
            }
            return Ok(0);
        }
        RichBatchOperation::ReadVisual { rendition_id, .. } => {
            if rendition_id.is_empty() || rendition_id.len() > 256 {
                return Err(bad());
            }
            return Ok(0);
        }
        RichBatchOperation::SearchMultimodal {
            query,
            document,
            window_bytes,
            max_visual_candidates,
            max_concurrent_batches,
            min_score,
            ..
        } => {
            if query.trim().is_empty()
                || query.len() > 8192
                || document.as_deref().is_some_and(|s| !valid_path(s))
                || !(4..=2048).contains(window_bytes)
                || !(1..=16).contains(max_visual_candidates)
                || !(1..=4).contains(max_concurrent_batches)
                || !min_score.is_finite()
                || !(0.0..=1.0).contains(min_score)
            {
                return Err(bad());
            }
            return Ok(0);
        }
    }
    let cursor_bound = Cursor {
        body: cursor_body(pin, op, usize::MAX),
        sha256: "0".repeat(64),
    };
    measure(&cursor_bound, MAX_RICH_BATCH_CURSOR_BYTES / 2).map_err(|_| {
        error(
            RichBatchErrorCode::InvalidOperation,
            "Scope cannot produce a bounded continuation",
        )
    })?;
    position(pin, op)
}

fn meter_delta(
    after: &ReaderReadMetrics,
    before: &ReaderReadMetrics,
) -> Result<ReaderReadMetrics, RichBatchError> {
    macro_rules! delta {
        ($field:ident) => {
            after.$field.checked_sub(before.$field).ok_or_else(|| {
                error(
                    RichBatchErrorCode::SharedIntegrity,
                    "Reader meter counter decreased",
                )
            })?
        };
    }
    Ok(ReaderReadMetrics {
        logical_reader_bytes: delta!(logical_reader_bytes),
        cached_logical_bytes: delta!(cached_logical_bytes),
        uncached_logical_bytes: delta!(uncached_logical_bytes),
        observed_rust_read_bytes: delta!(observed_rust_read_bytes),
        observed_hash_bytes: delta!(observed_hash_bytes),
        cache_hits: delta!(cache_hits),
        cache_misses: delta!(cache_misses),
        cache_fallbacks: delta!(cache_fallbacks),
        uncached_load_bytes: delta!(uncached_load_bytes),
        coverage: after.coverage.clone(),
    })
}

fn preflight_resource(
    manifest: &crate::RichManifest,
    op: &RichBatchOperation,
    start: usize,
) -> Result<(), RichBatchError> {
    let absent = || {
        error(
            RichBatchErrorCode::ResourceNotFound,
            "Resource is absent from the pinned generation",
        )
    };
    match op {
        RichBatchOperation::InspectPages { document, .. }
        | RichBatchOperation::SearchArtifacts {
            document: Some(document),
            ..
        }
        | RichBatchOperation::SearchMultimodal {
            document: Some(document),
            ..
        } => {
            let doc = manifest
                .documents
                .iter()
                .find(|d| d.document.path == *document)
                .ok_or_else(absent)?;
            if matches!(op, RichBatchOperation::InspectPages { .. }) && start > doc.pages.len() {
                return Err(error(
                    RichBatchErrorCode::InvalidCursor,
                    "Cursor exceeds indexed page enumeration",
                ));
            }
        }
        RichBatchOperation::InspectDocuments { .. } => {
            if start > manifest.documents.len() {
                return Err(error(
                    RichBatchErrorCode::InvalidCursor,
                    "Cursor exceeds indexed document enumeration",
                ));
            }
        }
        RichBatchOperation::ReadArtifact {
            artifact_id,
            cursor,
            ..
        } => {
            let artifact = manifest
                .documents
                .iter()
                .flat_map(|d| &d.artifacts)
                .find(|a| a.artifact.id == *artifact_id)
                .ok_or_else(absent)?;
            if !matches!(
                artifact.artifact.kind,
                gptgrep_parse::ArtifactKind::NativeText | gptgrep_parse::ArtifactKind::OcrText
            ) {
                return Err(error(
                    RichBatchErrorCode::InvalidOperation,
                    "Artifact is not native/OCR text evidence",
                ));
            }
            if start as u64 > artifact.artifact.bytes {
                return Err(error(
                    if cursor.is_some() {
                        RichBatchErrorCode::InvalidCursor
                    } else {
                        RichBatchErrorCode::InvalidOperation
                    },
                    "Text offset exceeds indexed artifact bytes",
                ));
            }
        }
        RichBatchOperation::InspectVisual { scope, .. } => {
            if scope
                .document_ids
                .iter()
                .any(|id| !manifest.documents.iter().any(|doc| &doc.document.id == id))
            {
                return Err(absent());
            }
        }
        RichBatchOperation::ReadVisual { rendition_id, .. }
            if !manifest.documents.iter().any(|doc| {
                doc.pages.iter().any(|page| {
                    page.renditions
                        .iter()
                        .any(|r| &r.artifact.id == rendition_id)
                })
            }) =>
        {
            return Err(absent());
        }
        _ => {}
    }
    Ok(())
}
fn item_meter(
    reader: &RichGenerationReader,
    before: &ReaderReadMetrics,
) -> Result<ReaderReadMetrics, RichBatchError> {
    let mut metrics = meter_delta(&reader.read_metrics().map_err(reader_error)?, before)?;
    metrics.coverage.clear();
    Ok(metrics)
}
fn reader_error(e: anyhow::Error) -> RichBatchError {
    let text = format!("{e:#}");
    if text.contains("source changed") || text.contains("source is stale") {
        error(
            RichBatchErrorCode::SourceStale,
            "Source changed or disappeared; reindex before reading",
        )
    } else if text.contains("not present") {
        error(
            RichBatchErrorCode::ResourceNotFound,
            "Resource is absent from the pinned generation",
        )
    } else if text.contains("resource_limit")
        || text.contains("output reservation")
        || text.contains("read_cache_oversized")
        || text.contains("read_cache_capacity_retained")
    {
        error(
            RichBatchErrorCode::OutputBudgetInsufficient,
            "Complete resource output exceeds its declared reservation",
        )
    } else if text.contains("invalid text window offset")
        || text.contains("text budget cannot fit the next UTF-8 scalar")
        || text.contains("cannot hold")
        || text.contains("not native/OCR")
    {
        error(
            RichBatchErrorCode::InvalidOperation,
            "Text window or artifact type is invalid",
        )
    } else {
        error(
            RichBatchErrorCode::SharedIntegrity,
            "Pinned generation or artifact integrity validation failed",
        )
    }
}

fn visual_error(
    visual: &VisualGenerationReader,
    manifest: &crate::RichManifest,
    scope: &VisualScope,
    cause: anyhow::Error,
) -> RichBatchError {
    let message = format!("{cause:#}");
    let code = if message.contains("visual_cursor") {
        RichBatchErrorCode::InvalidCursor
    } else if message.contains("visual_response") || message.contains("visual_working") {
        RichBatchErrorCode::OutputBudgetInsufficient
    } else if message.contains("visual_document_not_in_pin")
        || message.contains("visual_rendition_not_in_pin")
    {
        RichBatchErrorCode::ResourceNotFound
    } else if message.contains("visual_request_limit")
        || message.contains("visual_scope_rejected")
        || message.contains("visual_rendition_id_rejected")
    {
        RichBatchErrorCode::InvalidOperation
    } else if visual.validate().is_err() {
        RichBatchErrorCode::SharedIntegrity
    } else if manifest.documents.iter().any(|doc| {
        (scope.document_ids.is_empty() || scope.document_ids.contains(&doc.document.id))
            && visual.verify_source(&doc.document.id).is_err()
    }) {
        // Source-leaf changes are item-local; retained manifest/dir/artifact
        // corruption still stops the batch. No diagnostic path is returned.
        RichBatchErrorCode::SourceStale
    } else {
        RichBatchErrorCode::SharedIntegrity
    };
    error(
        code,
        "Local visual request or pinned custody validation failed",
    )
}

fn visual_metrics_upper() -> ReaderReadMetrics {
    ReaderReadMetrics {
        logical_reader_bytes: u64::MAX,
        cached_logical_bytes: u64::MAX,
        uncached_logical_bytes: u64::MAX,
        observed_rust_read_bytes: u64::MAX,
        observed_hash_bytes: u64::MAX,
        cache_hits: u64::MAX,
        cache_misses: u64::MAX,
        cache_fallbacks: u64::MAX,
        uncached_load_bytes: u64::MAX,
        coverage: String::new(),
    }
}

fn checked_visual_source(
    visual: &VisualGenerationReader,
    document_id: &str,
) -> Result<(), RichBatchError> {
    visual.validate().map_err(|_| {
        error(
            RichBatchErrorCode::SharedIntegrity,
            "Retained visual custody changed",
        )
    })?;
    visual.verify_source(document_id).map_err(|_| {
        error(
            RichBatchErrorCode::SourceStale,
            "Visual source changed or disappeared",
        )
    })
}

fn visual_coverage_upper() -> RichBatchItemCoverage {
    RichBatchItemCoverage {
        unit: "visual_candidates".into(),
        available: Some(usize::MAX),
        start: usize::MAX,
        end: usize::MAX,
        selected: usize::MAX,
        exhausted: false,
        budget_truncated: false,
        search_ceiling_reached: false,
        // The native visual token appears once, in result.next_cursor.
        cursor: None,
    }
}

fn visual_result_limit(op: &RichBatchOperation, quota: usize) -> Result<usize, RichBatchError> {
    let base = measure(
        &ItemCandidate {
            id: op.id(),
            status: RichBatchStatus::Ok,
            reserved_output_bytes: quota,
            result: &Value::Null,
            coverage: &visual_coverage_upper(),
            read_metrics: &visual_metrics_upper(),
            error: None,
        },
        quota,
    )?;
    // Replace the four-byte null with a complete bounded visual result.
    quota
        .checked_sub(base - 4)
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            error(
                RichBatchErrorCode::OutputBudgetInsufficient,
                "Visual item envelope cannot fit",
            )
        })
}

#[derive(Serialize)]
struct LocalVisual<T: Serialize> {
    #[serde(flatten)]
    observation: T,
    actual_image_delivery: bool,
    citation_authority: bool,
}

#[derive(Serialize)]
struct VisualRead<'a> {
    schema_version: &'static str,
    candidate: &'a VisualCandidate,
    image_base64: &'a str,
    actual_image_delivery: bool,
    citation_authority: bool,
}

fn visual_base64(bytes: &[u8], admitted_length: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(admitted_length);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        result.push(ALPHABET[(a >> 2) as usize] as char);
        result.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

fn execute_visual(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    quota: usize,
    before: &ReaderReadMetrics,
) -> Result<RichBatchItem, RichBatchError> {
    if reader.cache_status().map_err(reader_error)?["strict_cache_only"] == true {
        return Err(error(
            RichBatchErrorCode::InvalidOperation,
            "Visual operations require uncached verified input access; no rendition cache exists",
        ));
    }
    let manifest = reader.manifest().map_err(reader_error)?;
    let visual = reader.visual_reader().map_err(reader_error)?;
    let limit = visual_result_limit(op, quota)?;
    match op {
        RichBatchOperation::InspectVisual {
            scope,
            query,
            max_candidates,
            cursor,
            ..
        } => {
            let limit = limit
                .checked_sub(b",\"actual_image_delivery\":false,\"citation_authority\":false".len())
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    error(
                        RichBatchErrorCode::OutputBudgetInsufficient,
                        "Visual result cannot fit",
                    )
                })?;
            let report = visual
                .enumerate(&VisualCandidateRequest {
                    scope: scope.clone(),
                    query: query.clone(),
                    max_candidates: *max_candidates,
                    max_output_bytes: limit,
                    resources: manifest.resource_budget.clone(),
                    cursor: cursor.clone(),
                })
                .map_err(|e| visual_error(&visual, manifest, scope, e))?;
            let selected = report.candidates.len();
            let cover = RichBatchItemCoverage {
                unit: "visual_candidates".into(),
                available: Some(report.coverage.available_candidates),
                start: report.offset,
                end: report.offset + selected,
                selected,
                exhausted: report.coverage.enumeration_complete,
                budget_truncated: selected < *max_candidates
                    && !report.coverage.enumeration_complete,
                search_ceiling_reached: false,
                cursor: None,
            };
            successful(
                reader,
                op,
                quota,
                before,
                &LocalVisual {
                    observation: report,
                    actual_image_delivery: false,
                    citation_authority: false,
                },
                cover,
            )
        }
        RichBatchOperation::ReadVisual { rendition_id, .. } => {
            let (doc, page) = manifest
                .documents
                .iter()
                .find_map(|doc| {
                    doc.pages
                        .iter()
                        .find(|page| {
                            page.renditions
                                .iter()
                                .any(|r| &r.artifact.id == rendition_id)
                        })
                        .map(|page| (doc, page))
                })
                .ok_or_else(|| {
                    error(
                        RichBatchErrorCode::ResourceNotFound,
                        "Rendition is absent from pinned generation",
                    )
                })?;
            let scope = VisualScope {
                document_ids: vec![doc.document.id.clone()],
                first_page: Some(page.number),
                last_page: Some(page.number),
            };
            checked_visual_source(&visual, &doc.document.id)?;
            // Enumerate only this physical page's at-most-four metadata entries.
            // This copies no rendition bytes and preserves native visual custody.
            let mut request = VisualCandidateRequest {
                scope: scope.clone(),
                query: String::new(),
                max_candidates: 1,
                max_output_bytes: limit,
                resources: manifest.resource_budget.clone(),
                cursor: None,
            };
            let mut candidate = None;
            for _ in 0..4 {
                let mut report = visual
                    .enumerate(&request)
                    .map_err(|e| visual_error(&visual, manifest, &scope, e))?;
                if let Some(found) = report.candidates.pop().filter(|value| {
                    matches!(&value.evidence, crate::EvidenceRef::VisualRegion { rendition_id: id, .. } if id == rendition_id)
                }) {
                    candidate = Some(found);
                    break;
                }
                request.cursor = report.next_cursor;
                if request.cursor.is_none() {
                    break;
                }
            }
            let candidate = candidate.ok_or_else(|| {
                error(
                    RichBatchErrorCode::SharedIntegrity,
                    "Indexed rendition is absent from visual metadata",
                )
            })?;
            let bytes = usize::try_from(candidate.encoded_bytes).map_err(|_| {
                error(
                    RichBatchErrorCode::OutputBudgetInsufficient,
                    "Visual byte count exceeds output bounds",
                )
            })?;
            let encoded_length = bytes
                .checked_add(2)
                .and_then(|n| n.checked_div(3))
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| {
                    error(
                        RichBatchErrorCode::OutputBudgetInsufficient,
                        "Visual base64 count exceeds output bounds",
                    )
                })?;
            let cover = RichBatchItemCoverage {
                unit: "image_bytes".into(),
                available: Some(bytes),
                start: 0,
                end: bytes,
                selected: bytes,
                exhausted: true,
                budget_truncated: false,
                search_ceiling_reached: false,
                cursor: None,
            };
            let empty = VisualRead {
                schema_version: "gptgrep.visual.payload.v1",
                candidate: &candidate,
                image_base64: "",
                actual_image_delivery: false,
                citation_authority: false,
            };
            let base = measure(
                &ItemCandidate {
                    id: op.id(),
                    status: RichBatchStatus::Ok,
                    reserved_output_bytes: quota,
                    result: &empty,
                    coverage: &cover,
                    read_metrics: &visual_metrics_upper(),
                    error: None,
                },
                quota,
            )?;
            if base.checked_add(encoded_length).is_none_or(|n| n > quota) {
                return Err(error(
                    RichBatchErrorCode::OutputBudgetInsufficient,
                    "Complete image and batch item exceed reserved share",
                ));
            }
            // Full batch item + ASCII base64 admission precedes artifact reads.
            let payload = visual
                .read_rendition(rendition_id, &manifest.resource_budget, limit)
                .map_err(|e| visual_error(&visual, manifest, &scope, e))?;
            checked_visual_source(&visual, &doc.document.id)?;
            if payload.bytes().len() != bytes {
                return Err(error(
                    RichBatchErrorCode::SharedIntegrity,
                    "Verified rendition byte count changed",
                ));
            }
            let encoded = visual_base64(payload.bytes(), encoded_length);
            successful(
                reader,
                op,
                quota,
                before,
                &VisualRead {
                    schema_version: "gptgrep.visual.payload.v1",
                    candidate: payload.candidate(),
                    image_base64: &encoded,
                    actual_image_delivery: false,
                    citation_authority: false,
                },
                cover,
            )
        }
        _ => Err(error(
            RichBatchErrorCode::InvalidOperation,
            "Not a visual operation",
        )),
    }
}

pub fn dispatch_rich_batch(
    root: &Path,
    request: RichBatchRequest,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    let reader = open_rich_batch_reader(root, &request)?;
    dispatch_with_baseline(&reader, request, ReaderReadMetrics::default())
}

/// Resolves CURRENT once or opens the exact pin, without reading credentials.
pub fn open_rich_batch_reader(
    root: &Path,
    request: &RichBatchRequest,
) -> Result<RichGenerationReader, Box<RichBatchFailure>> {
    validate_request(request).map_err(|e| failure(e, None, vec![]))?;
    let reader = match &request.generation {
        RichBatchGeneration::Current(_) => {
            RichGenerationReader::open_with_cache_budget(root, &request.budget.cache)
        }
        RichBatchGeneration::Exact(pin) => {
            RichGenerationReader::open_pinned_with_cache_budget(root, pin, &request.budget.cache)
        }
    }
    .map_err(|_| {
        failure(
            error(
                RichBatchErrorCode::SharedIntegrity,
                "A retained v2-rich generation is required",
            ),
            None,
            vec![],
        )
    })?;
    Ok(reader)
}
pub fn dispatch_rich_batch_with_reader(
    reader: &RichGenerationReader,
    request: RichBatchRequest,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    let before = reader
        .read_metrics()
        .map_err(|e| failure(reader_error(e), Some(reader.pin()), vec![]))?;
    dispatch_with_baseline(reader, request, before)
}
fn dispatch_with_baseline(
    reader: &RichGenerationReader,
    request: RichBatchRequest,
    before: ReaderReadMetrics,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    let max = request.budget.max_output_bytes;
    let mut result = dispatch_inner(reader, &request);
    let delta = reader
        .read_metrics()
        .map_err(reader_error)
        .and_then(|after| meter_delta(&after, &before));
    match (&mut result, delta) {
        (Ok(response), Ok(metrics)) => {
            response.metrics.read_metrics = Some(metrics);
            if measure(response, max).is_err() {
                let mut bad = failure(
                    error(
                        RichBatchErrorCode::OutputBudgetInsufficient,
                        "Complete batch exceeds reserved envelope",
                    ),
                    Some(response.generation.clone()),
                    response.items.clone(),
                );
                bad.read_metrics = response.metrics.read_metrics.clone();
                result = Err(bad);
            }
        }
        (Err(failed), Ok(metrics)) => failed.read_metrics = Some(metrics),
        (_, Err(e)) => return Err(failure(e, Some(reader.pin()), vec![])),
    }
    result
}
fn dispatch_inner(
    reader: &RichGenerationReader,
    request: &RichBatchRequest,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    let started = Instant::now();
    let pin = reader.pin();
    validate_request(request).map_err(|e| failure(e, Some(pin.clone()), vec![]))?;
    let cache = reader
        .cache_status()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), vec![]))?;
    if cache["capacity_bytes"]
        .as_u64()
        .is_none_or(|v| v > request.budget.cache.max_bytes as u64)
        || cache["capacity_entries"]
            .as_u64()
            .is_none_or(|v| v > request.budget.cache.max_entries as u64)
        || (request.budget.cache.strict_cache_only && cache["strict_cache_only"] != true)
    {
        return Err(failure(
            error(
                RichBatchErrorCode::InvalidRequest,
                "Supplied reader cache exceeds declared budget or strict policy",
            ),
            Some(pin),
            vec![],
        ));
    }
    if matches!(&request.generation,RichBatchGeneration::Exact(expected) if expected!=&pin) {
        return Err(failure(
            error(
                RichBatchErrorCode::InvalidGeneration,
                "Reader does not match the exact requested pin",
            ),
            Some(pin),
            vec![],
        ));
    }
    let count = request.operations.len();
    let available = request.budget.max_output_bytes - ENVELOPE_RESERVE;
    let quotas: Vec<_> = (0..count)
        .map(|i| available / count + usize::from(i < available % count))
        .collect();
    let mut items: Vec<_> = request
        .operations
        .iter()
        .zip(&quotas)
        .map(|(op, q)| RichBatchItem::pending(op.id(), *q))
        .collect();
    let manifest = reader
        .manifest()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), items.clone()))?;
    let preflight: Vec<_> = request
        .operations
        .iter()
        .map(|op| {
            preflight(&pin, op).and_then(|start| {
                preflight_resource(manifest, op, start)?;
                Ok(start)
            })
        })
        .collect();
    reader
        .validate()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), items.clone()))?;
    for (i, op) in request.operations.iter().enumerate() {
        let item_before = reader
            .read_metrics()
            .map_err(|e| failure(reader_error(e), Some(pin.clone()), items.clone()))?;
        if let Err(e) = reader.validate() {
            items[i].read_metrics = item_meter(reader, &item_before).ok();
            return Err(failure(reader_error(e), Some(pin), items));
        }
        let value = match &preflight[i] {
            Ok(start) => execute(reader, op, *start, quotas[i], &item_before),
            Err(e) => Err(e.clone()),
        };
        if let Err(e) = reader.validate() {
            let failed = reader_error(e);
            items[i] = RichBatchItem::failed(op.id(), quotas[i], failed.clone());
            items[i].read_metrics = item_meter(reader, &item_before).ok();
            return Err(failure(failed, Some(pin), items));
        }
        let mut item = match value {
            Ok(item) => item,
            Err(e) if e.code == RichBatchErrorCode::SharedIntegrity => {
                items[i] = RichBatchItem::failed(op.id(), quotas[i], e.clone());
                items[i].read_metrics = item_meter(reader, &item_before).ok();
                return Err(failure(e, Some(pin), items));
            }
            Err(e) => RichBatchItem::failed(op.id(), quotas[i], e),
        };
        item.read_metrics = Some(
            item_meter(reader, &item_before)
                .map_err(|e| failure(e, Some(pin.clone()), items.clone()))?,
        );
        if measure(&item, quotas[i]).is_err() {
            item = RichBatchItem::failed(
                op.id(),
                quotas[i],
                error(
                    RichBatchErrorCode::OutputBudgetInsufficient,
                    "Complete operation exceeds reserved share",
                ),
            );
            item.read_metrics = Some(
                item_meter(reader, &item_before)
                    .map_err(|e| failure(e, Some(pin.clone()), items.clone()))?,
            );
        }
        items[i] = item;
    }
    reader
        .validate()
        .map_err(|e| failure(reader_error(e), Some(pin.clone()), items.clone()))?;
    let coverage = RichBatchCoverage {
        requested: count,
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
    Ok(RichBatchResponse {
        schema_version: RICH_BATCH_RESPONSE_SCHEMA.into(),
        generation: pin,
        execution: Default::default(),
        items,
        coverage,
        metrics: RichBatchMetrics {
            elapsed_ms: started.elapsed().as_millis(),
            shared_reader: true,
            physical_bytes: None,
            model_submissions: Some(0),
            read_metrics: None,
            cache_status: reader.cache_status().ok(),
            decisions: None,
        },
    })
}

#[derive(Serialize)]
struct ItemCandidate<'a, T: Serialize> {
    id: &'a str,
    status: RichBatchStatus,
    reserved_output_bytes: usize,
    result: &'a T,
    coverage: &'a RichBatchItemCoverage,
    read_metrics: &'a ReaderReadMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a RichBatchError>,
}
fn successful<T: Serialize>(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    quota: usize,
    before: &ReaderReadMetrics,
    result: &T,
    coverage: RichBatchItemCoverage,
) -> Result<RichBatchItem, RichBatchError> {
    successful_with_error(reader, op, quota, before, result, coverage, None)
}
fn successful_with_error<T: Serialize>(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    quota: usize,
    before: &ReaderReadMetrics,
    result: &T,
    coverage: RichBatchItemCoverage,
    failure: Option<RichBatchError>,
) -> Result<RichBatchItem, RichBatchError> {
    let metrics = item_meter(reader, before)?;
    let status = if failure.is_some() {
        RichBatchStatus::Error
    } else {
        RichBatchStatus::Ok
    };
    measure(
        &ItemCandidate {
            id: op.id(),
            status,
            reserved_output_bytes: quota,
            result,
            coverage: &coverage,
            read_metrics: &metrics,
            error: failure.as_ref(),
        },
        quota,
    )?;
    // Only bounded admitted serialization can now allocate owned JSON values.
    let result = serde_json::to_value(result).map_err(|_| {
        error(
            RichBatchErrorCode::SharedIntegrity,
            "Bounded result serialization failed",
        )
    })?;
    Ok(RichBatchItem {
        id: op.id().into(),
        status,
        reserved_output_bytes: quota,
        result: Some(result.into()),
        error: failure,
        coverage,
        read_metrics: Some(metrics),
        decisions: None,
        metadata_lease: None,
    })
}
fn coverage(
    pin: &GenerationPin,
    op: &RichBatchOperation,
    total: usize,
    start: usize,
    end: usize,
    unit: &str,
    truncated: bool,
) -> RichBatchItemCoverage {
    RichBatchItemCoverage {
        unit: unit.into(),
        available: Some(total),
        start,
        end,
        selected: end - start,
        exhausted: end == total,
        budget_truncated: truncated,
        search_ceiling_reached: false,
        cursor: (end < total).then(|| cursor(pin, op, end)),
    }
}
fn selected(fields: &[String], name: &str) -> bool {
    fields.is_empty() || fields.iter().any(|f| f == name)
}
struct DocumentProjection<'a> {
    doc: &'a RichDocument,
    fields: &'a [String],
}
impl Serialize for DocumentProjection<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let d = &self.doc.document;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("id", &d.id)?;
        m.serialize_entry("path", &d.path)?;
        m.serialize_entry("source_sha256", &d.source_sha256)?;
        m.serialize_entry("source_status", "indexed_metadata_only")?;
        macro_rules! field {
            ($name:literal,$value:expr) => {
                if selected(self.fields, $name) {
                    m.serialize_entry($name, $value)?;
                }
            };
        }
        field!("title", &d.title);
        field!("source_bytes", &d.source_bytes);
        field!("parser", &d.parser);
        field!("pages", &d.pages.len());
        field!("nodes", &d.nodes.len());
        field!("warnings", &d.warnings);
        field!("prepared", &self.doc.prepared);
        m.end()
    }
}
struct PageProjection<'a> {
    doc: &'a RichDocument,
    page: &'a NativePage,
    fields: &'a [String],
}
impl Serialize for PageProjection<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let p = self.page;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("document_id", &self.doc.document.id)?;
        m.serialize_entry("source_sha256", &self.doc.document.source_sha256)?;
        m.serialize_entry("page", &p.number)?;
        m.serialize_entry("source_status", "indexed_metadata_only")?;
        macro_rules! field {
            ($name:literal,$value:expr) => {
                if selected(self.fields, $name) {
                    m.serialize_entry($name, $value)?;
                }
            };
        }
        field!("label", &p.label);
        field!("width_points", &p.width_points);
        field!("height_points", &p.height_points);
        field!("geometry", &p.geometry);
        field!("coverage", &p.coverage);
        field!("complexity", &p.complexity);
        field!("layout", &p.complexity.layout);
        field!("capabilities", &p.capabilities);
        field!("native_text", &p.native_text);
        field!("metadata_artifact", &p.metadata_artifact);
        field!("renditions", &p.renditions);
        if selected(self.fields, "ocr_text") {
            m.serialize_entry("ocr_text", &PageOcrArtifacts(self.doc, p.number))?;
        }
        m.end()
    }
}
struct PageOcrArtifacts<'a>(&'a RichDocument, u32);
impl Serialize for PageOcrArtifacts<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        for a in &self.0.artifacts {
            if a.artifact.page == Some(self.1)
                && a.artifact.kind == gptgrep_parse::ArtifactKind::OcrText
            {
                seq.serialize_element(&a.artifact)?;
            }
        }
        seq.end()
    }
}

struct EvidenceAdmission<'a> {
    pin: &'a GenerationPin,
    doc: &'a RichDocument,
    artifact: &'a ArtifactRef,
}
impl Serialize for EvidenceAdmission<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Binding<'a> {
            generation: &'a GenerationPin,
            document_id: &'a str,
            source_path: &'a str,
            source_sha256: &'a str,
            artifact_id: &'a str,
            artifact_sha256: &'a str,
            page: u32,
            byte_start: usize,
            byte_end: usize,
            line: usize,
        }
        let mut m = s.serialize_map(None)?;
        let native = self.artifact.kind == gptgrep_parse::ArtifactKind::NativeText;
        m.serialize_entry("kind", if native { "native_text" } else { "ocr_text" })?;
        m.serialize_entry(
            "binding",
            &Binding {
                generation: self.pin,
                document_id: &self.doc.document.id,
                source_path: &self.doc.document.path,
                source_sha256: &self.doc.document.source_sha256,
                artifact_id: &self.artifact.id,
                artifact_sha256: &self.artifact.sha256,
                page: u32::MAX,
                byte_start: usize::MAX,
                byte_end: usize::MAX,
                line: usize::MAX,
            },
        )?;
        if !native {
            let outcome = self
                .doc
                .ocr_results
                .iter()
                .find(|r| Some(r.page) == self.artifact.page)
                .ok_or_else(|| serde::ser::Error::custom("OCR artifact receipt absent"))?;
            struct Ids<'a>(&'a [gptgrep_parse::OcrRegionResult]);
            impl Serialize for Ids<'_> {
                fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                    let mut seq = s.serialize_seq(Some(self.0.len()))?;
                    for r in self.0 {
                        seq.serialize_element(&r.id)?;
                    }
                    seq.end()
                }
            }
            m.serialize_entry("region_ids", &Ids(&outcome.regions))?;
            m.serialize_entry("ocr_profile_sha256", &outcome.receipt.profile_sha256)?;
            m.serialize_entry(
                "ocr_receipt_sha256",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )?;
        }
        m.end()
    }
}
fn admitted_read_bytes(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    artifact_id: &str,
    requested: usize,
    quota: usize,
) -> Result<usize, RichBatchError> {
    let manifest = reader.manifest().map_err(reader_error)?;
    let (doc, artifact) = manifest
        .documents
        .iter()
        .find_map(|doc| {
            doc.artifacts
                .iter()
                .find(|a| a.artifact.id == artifact_id)
                .map(|a| (doc, &a.artifact))
        })
        .ok_or_else(|| {
            error(
                RichBatchErrorCode::ResourceNotFound,
                "Artifact is absent from pinned generation",
            )
        })?;
    if !matches!(
        artifact.kind,
        gptgrep_parse::ArtifactKind::NativeText | gptgrep_parse::ArtifactKind::OcrText
    ) {
        return Err(error(
            RichBatchErrorCode::InvalidOperation,
            "Artifact is not native/OCR text evidence",
        ));
    }
    if artifact.page.is_none()
        || (artifact.kind == gptgrep_parse::ArtifactKind::OcrText
            && !doc
                .ocr_results
                .iter()
                .any(|r| Some(r.page) == artifact.page))
    {
        return Err(error(
            RichBatchErrorCode::SharedIntegrity,
            "Text artifact page or bound OCR receipt is absent",
        ));
    }
    let pin = reader.pin();
    #[derive(Serialize)]
    struct Window<'a> {
        evidence: EvidenceAdmission<'a>,
        text: &'static str,
        next_offset: Option<usize>,
    }
    let window = Window {
        evidence: EvidenceAdmission {
            pin: &pin,
            doc,
            artifact,
        },
        text: "",
        next_offset: Some(usize::MAX),
    };
    let cover = RichBatchItemCoverage {
        unit: "bytes".into(),
        available: Some(usize::MAX),
        start: usize::MAX,
        end: usize::MAX,
        selected: usize::MAX,
        exhausted: false,
        budget_truncated: true,
        search_ceiling_reached: false,
        cursor: Some(cursor(&pin, op, usize::MAX)),
    };
    let metrics = ReaderReadMetrics {
        logical_reader_bytes: u64::MAX,
        cached_logical_bytes: u64::MAX,
        uncached_logical_bytes: u64::MAX,
        observed_rust_read_bytes: u64::MAX,
        observed_hash_bytes: u64::MAX,
        cache_hits: u64::MAX,
        cache_misses: u64::MAX,
        cache_fallbacks: u64::MAX,
        uncached_load_bytes: u64::MAX,
        coverage: String::new(),
    };
    let base = measure(
        &ItemCandidate {
            id: op.id(),
            status: RichBatchStatus::Ok,
            reserved_output_bytes: quota,
            result: &window,
            coverage: &cover,
            read_metrics: &metrics,
            error: None,
        },
        quota,
    )?;
    let bytes = (quota - base) / 6;
    if bytes == 0 {
        return Err(error(
            RichBatchErrorCode::OutputBudgetInsufficient,
            "Reserved share cannot hold a complete text window and typed evidence",
        ));
    }
    Ok(requested.min(bytes))
}
#[derive(Serialize)]
struct Inspection<'a> {
    source_status: &'static str,
    objects: &'a [Value],
}
fn enumerate<T: Serialize>(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    start: usize,
    quota: usize,
    before: &ReaderReadMetrics,
    objects: impl ExactSizeIterator<Item = T>,
    page_size: usize,
) -> Result<RichBatchItem, RichBatchError> {
    let total = objects.len();
    if start > total {
        return Err(error(
            RichBatchErrorCode::InvalidCursor,
            "Cursor exceeds indexed enumeration",
        ));
    }
    let limit = start.saturating_add(page_size).min(total);
    let mut values = Vec::new();
    let mut latest = None;
    if start == total {
        return successful(
            reader,
            op,
            quota,
            before,
            &Inspection {
                source_status: "indexed_metadata_only",
                objects: &values,
            },
            coverage(&reader.pin(), op, total, start, start, "objects", false),
        );
    }
    for (i, object) in objects.enumerate().skip(start).take(limit - start) {
        if measure(&object, quota).is_err() {
            break;
        }
        let value = serde_json::to_value(&object).map_err(|_| {
            error(
                RichBatchErrorCode::SharedIntegrity,
                "Metadata serialization failed",
            )
        })?;
        values.push(value);
        let result = Inspection {
            source_status: "indexed_metadata_only",
            objects: &values,
        };
        let cover = coverage(
            &reader.pin(),
            op,
            total,
            start,
            i + 1,
            "objects",
            i + 1 < limit,
        );
        match successful(reader, op, quota, before, &result, cover) {
            Ok(item) => latest = Some(item),
            Err(e) if e.code == RichBatchErrorCode::OutputBudgetInsufficient => break,
            Err(e) => return Err(e),
        };
    }
    latest.ok_or_else(|| {
        error(
            RichBatchErrorCode::OutputBudgetInsufficient,
            "Reserved share cannot hold the next complete metadata object and cursor",
        )
    })
}
fn native_selection(op: &RichBatchOperation) -> Option<crate::NativeMetadataSelection> {
    if let RichBatchOperation::InspectNativeMetadata {
        fields,
        collection,
        item_fields,
        object_id,
        ..
    } = op
    {
        Some(crate::NativeMetadataSelection {
            fields: fields.clone(),
            collection: *collection,
            item_fields: item_fields.clone(),
            object_id: object_id.clone(),
        })
    } else {
        None
    }
}
fn validate_native_operation(op: &RichBatchOperation) -> Result<(), RichBatchError> {
    let RichBatchOperation::InspectNativeMetadata {
        artifact_id,
        page_size,
        max_output_bytes,
        cursor,
        ..
    } = op
    else {
        return Ok(());
    };
    native_selection(op)
        .expect("selected operation")
        .validate()
        .map_err(|e| {
            if e.to_string().contains("invalid_fields") {
                error(
                    RichBatchErrorCode::InvalidFields,
                    "Native metadata fields are unknown, duplicated or invalid for this collection",
                )
            } else {
                error(
                    RichBatchErrorCode::InvalidOperation,
                    "Native metadata object selector does not match collection",
                )
            }
        })?;
    if artifact_id.is_empty()
        || artifact_id.len() > 512
        || !(1..=1000).contains(page_size)
        || *max_output_bytes == 0
        || *max_output_bytes > MAX_RICH_BATCH_OUTPUT_BYTES
        || cursor
            .as_ref()
            .is_some_and(|c| c.len() > MAX_RICH_BATCH_CURSOR_BYTES)
    {
        return Err(error(
            RichBatchErrorCode::InvalidOperation,
            "Native metadata options exceed bounded contract",
        ));
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCursorBody {
    schema: String,
    binding: crate::NativeMetadataBinding,
    options_sha256: String,
    position: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCursor {
    body: NativeCursorBody,
    sha256: String,
}
fn native_options_digest(op: &RichBatchOperation) -> String {
    let RichBatchOperation::InspectNativeMetadata {
        page_size,
        max_output_bytes,
        ..
    } = op
    else {
        unreachable!()
    };
    digest(&(
        native_selection(op)
            .expect("metadata selection")
            .normalized(),
        page_size,
        max_output_bytes,
    ))
}
fn native_cursor(
    binding: &crate::NativeMetadataBinding,
    op: &RichBatchOperation,
    position: usize,
    quota: usize,
) -> Result<String, RichBatchError> {
    let body = NativeCursorBody {
        schema: "gptgrep.native_metadata.cursor.v1".into(),
        binding: binding.clone(),
        options_sha256: native_options_digest(op),
        position,
    };
    let sha256 = digest(&body);
    let value = NativeCursor { body, sha256 };
    let bytes = measure(&value, (MAX_RICH_BATCH_CURSOR_BYTES / 2).min(quota / 2))?;
    let value = serde_json::to_vec(&value).map_err(|_| {
        error(
            RichBatchErrorCode::SharedIntegrity,
            "Native metadata cursor encoding failed",
        )
    })?;
    let mut token = String::with_capacity(bytes * 2);
    use std::fmt::Write;
    for b in value {
        write!(&mut token, "{b:02x}").expect("String write");
    }
    Ok(token)
}
fn native_position(
    binding: &crate::NativeMetadataBinding,
    op: &RichBatchOperation,
) -> Result<usize, RichBatchError> {
    let RichBatchOperation::InspectNativeMetadata { cursor, .. } = op else {
        unreachable!()
    };
    let Some(token) = cursor else { return Ok(0) };
    let bad = || {
        error(
            RichBatchErrorCode::InvalidCursor,
            "Native metadata cursor differs in pin, artifact, recipe, fields, collection, object or limits",
        )
    };
    if token.len() > MAX_RICH_BATCH_CURSOR_BYTES
        || token.len() % 2 != 0
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad());
    }
    let bytes: Result<Vec<u8>, _> = token
        .as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).expect("checked ASCII"), 16))
        .collect();
    let value: NativeCursor =
        serde_json::from_slice(&bytes.map_err(|_| bad())?).map_err(|_| bad())?;
    if value.body.schema != "gptgrep.native_metadata.cursor.v1"
        || digest(&value.body.binding) != digest(binding)
        || value.body.options_sha256 != native_options_digest(op)
        || value.sha256 != digest(&value.body)
    {
        return Err(bad());
    }
    Ok(value.body.position)
}
fn native_metadata_error(e: anyhow::Error) -> RichBatchError {
    let message = e.to_string();
    if message.contains("rebuild_required") {
        error(
            RichBatchErrorCode::InvalidOperation,
            "Requested native geometry is unsupported by retained recipe; rebuild required",
        )
    } else if message.contains("native_metadata_object_not_present") {
        error(
            RichBatchErrorCode::ResourceNotFound,
            "Native object is absent from selected page artifact",
        )
    } else if message.contains("native_metadata_object_not_path")
        || message.contains("native_metadata_page_artifact_required")
    {
        error(
            RichBatchErrorCode::InvalidOperation,
            "Selected resource is not the required page metadata or path type",
        )
    } else {
        reader_error(e)
    }
}
fn execute_native_metadata(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    quota: usize,
    before: &ReaderReadMetrics,
) -> Result<RichBatchItem, RichBatchError> {
    let RichBatchOperation::InspectNativeMetadata {
        artifact_id,
        page_size,
        max_output_bytes,
        ..
    } = op
    else {
        unreachable!()
    };
    let quota = quota.min(*max_output_bytes);
    let selection = native_selection(op).expect("metadata selection");
    // The output Value (including tree nodes/keys), serialization overlap and
    // cursor stay independently charged after the backing snapshot is dropped.
    let lease = reader
        .reserve_native_projection(
            (quota as u64).checked_mul(64).ok_or_else(|| {
                error(
                    RichBatchErrorCode::OutputBudgetInsufficient,
                    "Metadata output sizing overflow",
                )
            })?,
            crate::MAX_NATIVE_METADATA_WORKING_BYTES,
        )
        .map_err(native_metadata_error)?;
    let snapshot = reader
        .read_page_native_metadata(artifact_id, &crate::NativeMetadataReadBudget::default())
        .map_err(native_metadata_error)?;
    let total = snapshot
        .collection_len(&selection)
        .map_err(native_metadata_error)?;
    let start = native_position(snapshot.binding(), op)?;
    if start > total {
        return Err(error(
            RichBatchErrorCode::InvalidCursor,
            "Native metadata cursor exceeds collection",
        ));
    }
    let limit = start.saturating_add(*page_size).min(total);
    let metrics = item_meter(reader, before)?;
    let coverage_for = |end: usize| -> Result<RichBatchItemCoverage, RichBatchError> {
        Ok(RichBatchItemCoverage {
            unit: "native_metadata_rows".into(),
            available: Some(total),
            start,
            end,
            selected: end - start,
            exhausted: end == total,
            budget_truncated: end < limit,
            search_ceiling_reached: false,
            cursor: if end < total {
                Some(native_cursor(snapshot.binding(), op, end, quota)?)
            } else {
                None
            },
        })
    };
    let measure_page = |end: usize| -> Result<(), RichBatchError> {
        let projection = snapshot
            .projection(&selection, start, end)
            .map_err(native_metadata_error)?;
        measure(
            &ItemCandidate {
                id: op.id(),
                status: RichBatchStatus::Ok,
                reserved_output_bytes: quota,
                result: &projection,
                coverage: &coverage_for(end)?,
                read_metrics: &metrics,
                error: None,
            },
            quota,
        )?;
        Ok(())
    };
    // Only borrowed DTOs are traversed during admission; no row Value/string
    // copies occur until the complete scalars, rows, cursor and envelope fit.
    let mut end = start;
    if start == total {
        measure_page(start)?;
    } else {
        for candidate in start + 1..=limit {
            match measure_page(candidate) {
                Ok(()) => end = candidate,
                Err(e) if e.code == RichBatchErrorCode::OutputBudgetInsufficient => break,
                Err(e) => return Err(e),
            }
        }
        if end == start {
            return Err(error(
                RichBatchErrorCode::OutputBudgetInsufficient,
                "Reserved output cannot hold the next whole native metadata row and mandatory scalars",
            ));
        }
    }
    reader
        .recheck_native_metadata(&snapshot)
        .map_err(native_metadata_error)?;
    let projection = snapshot
        .projection(&selection, start, end)
        .map_err(native_metadata_error)?;
    let mut item = successful(reader, op, quota, before, &projection, coverage_for(end)?)?;
    item.result.as_mut().expect("successful metadata result").1 = Some(lease.clone());
    item.metadata_lease = Some(lease);
    reader
        .recheck_native_metadata(&snapshot)
        .map_err(native_metadata_error)?;
    Ok(item)
}

fn execute(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    start: usize,
    quota: usize,
    before: &ReaderReadMetrics,
) -> Result<RichBatchItem, RichBatchError> {
    match op {
        RichBatchOperation::InspectNativeMetadata { .. } => {
            execute_native_metadata(reader, op, quota, before)
        }
        RichBatchOperation::SearchMultimodal { .. } | RichBatchOperation::Rerank { .. } => {
            Err(error(
                RichBatchErrorCode::ModelContextRequired,
                "Paid rich operations require the explicit asynchronous client frontdoor",
            ))
        }
        RichBatchOperation::InspectVisual { .. } | RichBatchOperation::ReadVisual { .. } => {
            execute_visual(reader, op, quota, before)
        }
        RichBatchOperation::InspectDocuments {
            fields, page_size, ..
        } => {
            let manifest = reader.manifest().map_err(reader_error)?;
            enumerate(
                reader,
                op,
                start,
                quota,
                before,
                manifest
                    .documents
                    .iter()
                    .map(|doc| DocumentProjection { doc, fields }),
                *page_size,
            )
        }
        RichBatchOperation::InspectPages {
            document,
            fields,
            page_size,
            ..
        } => {
            let manifest = reader.manifest().map_err(reader_error)?;
            let doc = manifest
                .documents
                .iter()
                .find(|d| d.document.path == *document)
                .ok_or_else(|| {
                    error(
                        RichBatchErrorCode::ResourceNotFound,
                        "Document is absent from pinned generation",
                    )
                })?;
            enumerate(
                reader,
                op,
                start,
                quota,
                before,
                doc.pages
                    .iter()
                    .map(|page| PageProjection { doc, page, fields }),
                *page_size,
            )
        }
        RichBatchOperation::ReadArtifact {
            artifact_id,
            max_bytes,
            ..
        } => {
            // JSON escaping and a complete typed evidence object must fit before
            // the reader copies any text window. Existing reader resource caps
            // apply independently to its admitted backing text artifact.
            let bytes = admitted_read_bytes(reader, op, artifact_id, *max_bytes, quota)?;
            let window = reader
                .read_text(artifact_id, start, bytes)
                .map_err(reader_error)?;
            let end = start + window.text.len();
            let cover = RichBatchItemCoverage {
                unit: "bytes".into(),
                available: window.next_offset.is_none().then_some(end),
                start,
                end,
                selected: window.text.len(),
                exhausted: window.next_offset.is_none(),
                budget_truncated: bytes < *max_bytes,
                search_ceiling_reached: false,
                cursor: window.next_offset.map(|p| cursor(&reader.pin(), op, p)),
            };
            successful(reader, op, quota, before, &window, cover)
        }
        RichBatchOperation::SearchArtifacts {
            query,
            case_insensitive,
            literal,
            document,
            max_hits,
            page_size,
            ..
        } => {
            let report = reader
                .search_artifacts(
                    query,
                    *case_insensitive,
                    *literal,
                    *max_hits,
                    document.as_deref(),
                    quota.min(MAX_RICH_BATCH_OUTPUT_BYTES),
                )
                .map_err(reader_error)?;
            let total = report.hits.len();
            if start > total {
                return Err(error(
                    RichBatchErrorCode::InvalidCursor,
                    "Cursor exceeds bounded search window",
                ));
            }
            let end = start.saturating_add(*page_size).min(total);
            let cover = RichBatchItemCoverage {
                unit: "bounded_search_hits".into(),
                available: Some(total),
                start,
                end,
                selected: end - start,
                exhausted: end == total && !report.truncated && report.stale_documents.is_empty(),
                budget_truncated: false,
                search_ceiling_reached: report.truncated,
                cursor: (end < total).then(|| cursor(&reader.pin(), op, end)),
            };
            #[derive(Serialize)]
            struct SearchPage<'a> {
                hits: &'a [crate::ArtifactTextWindow],
                available_artifacts: usize,
                candidate_artifacts: usize,
                verified_artifacts: usize,
                search_ceiling_reached: bool,
                stale_documents: &'a [String],
            }
            let value = SearchPage {
                hits: &report.hits[start..end],
                available_artifacts: report.available_artifacts,
                candidate_artifacts: report.candidate_artifacts,
                verified_artifacts: report.verified_artifacts,
                search_ceiling_reached: report.truncated,
                stale_documents: &report.stale_documents,
            };
            let failure = (!report.stale_documents.is_empty()).then(|| {
                error(
                    RichBatchErrorCode::SourceStale,
                    "Search encountered stale sources; partial fresh evidence is retained",
                )
            });
            successful_with_error(reader, op, quota, before, &value, cover, failure)
        }
    }
}

#[cfg(test)]
#[path = "rich_batch/tests.rs"]
pub(crate) mod tests;
