//! Local deterministic batch operations over one retained v2 generation.
//! Inspection returns indexed metadata, not fresh source evidence. Native/OCR
//! text windows preserve the reader's typed evidence and freshness checks.
//! Empty EOF windows contain no citeable bytes; evidence consumers still use
//! the retained reader's nonempty evidence validation boundary.

use crate::{
    GenerationPin, ReaderCacheBudget, ReaderReadMetrics, RichDocument, RichGenerationReader,
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
}
impl Default for RichBatchBudget {
    fn default() -> Self {
        Self {
            max_operations: 64,
            max_output_bytes: MAX_RICH_BATCH_OUTPUT_BYTES,
            cache: ReaderCacheBudget::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum RichBatchOperation {
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
}
impl RichBatchOperation {
    pub fn id(&self) -> &str {
        match self {
            Self::InspectDocuments { id, .. }
            | Self::InspectPages { id, .. }
            | Self::ReadArtifact { id, .. }
            | Self::SearchArtifacts { id, .. } => id,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichBatchItem {
    pub id: String,
    pub status: RichBatchStatus,
    pub reserved_output_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RichBatchError>,
    pub coverage: RichBatchItemCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_metrics: Option<ReaderReadMetrics>,
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
    pub model_submissions: u32,
}
impl Default for RichBatchExecution {
    fn default() -> Self {
        Self {
            scope: "local_deterministic_no_model_context".into(),
            model_submissions: 0,
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
    pub model_submissions: u32,
    pub read_metrics: Option<ReaderReadMetrics>,
    pub cache_status: Option<Value>,
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
    let request: RichBatchRequest = serde_json::from_slice(bytes).map_err(|_| {
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

pub fn dispatch_rich_batch(
    root: &Path,
    request: RichBatchRequest,
) -> Result<RichBatchResponse, Box<RichBatchFailure>> {
    validate_request(&request).map_err(|e| failure(e, None, vec![]))?;
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
    dispatch_with_baseline(&reader, request, ReaderReadMetrics::default())
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
            model_submissions: 0,
            read_metrics: None,
            cache_status: reader.cache_status().ok(),
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
        result: Some(result),
        error: failure,
        coverage,
        read_metrics: Some(metrics),
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
fn execute(
    reader: &RichGenerationReader,
    op: &RichBatchOperation,
    start: usize,
    quota: usize,
    before: &ReaderReadMetrics,
) -> Result<RichBatchItem, RichBatchError> {
    match op {
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
mod tests;
