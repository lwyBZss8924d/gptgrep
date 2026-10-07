//! Bounded generation-pinned batch foundation. No model result is clipped after payment.
use crate::{GenerationPin, GenerationReader, SearchOptions};
use gptgrep_jev::{ExecutionSnapshot, JevClient};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path, time::Instant};

pub const BATCH_REQUEST_SCHEMA: &str = "gptgrep.batch.request.v1";
pub const BATCH_RESPONSE_SCHEMA: &str = "gptgrep.batch.response.v1";
pub const MAX_BATCH_OPERATIONS: usize = 64;
pub const MAX_BATCH_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_CURSOR_BYTES: usize = 16 * 1024;
const MIN_ITEM_OUTPUT_BYTES: usize = 1024;
const ENVELOPE_RESERVE_BYTES: usize = 4096;
/// Invalid declarations receive a bounded diagnostic; they are not admitted
/// output budgets. Accepted budgets also reserve 1024 bytes per operation.
pub const MAX_BATCH_DIAGNOSTIC_BYTES: usize = ENVELOPE_RESERVE_BYTES;

pub fn minimum_batch_output_bytes(operation_count: usize) -> Option<usize> {
    operation_count
        .checked_mul(MIN_ITEM_OUTPUT_BYTES)?
        .checked_add(ENVELOPE_RESERVE_BYTES)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub schema_version: String,
    #[serde(default)]
    pub generation: BatchGeneration,
    #[serde(default)]
    pub budget: BatchBudget,
    pub operations: Vec<BatchOperation>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BatchGeneration {
    Current(CurrentGeneration),
    Exact(GenerationPin),
}
impl Default for BatchGeneration {
    fn default() -> Self {
        Self::Current(CurrentGeneration::Current)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub enum CurrentGeneration {
    #[serde(rename = "CURRENT")]
    #[default]
    Current,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchBudget {
    pub max_operations: usize,
    pub max_output_bytes: usize,
    /// Local batch allowance, distinct from the caller's shared experiment cap.
    pub max_decisions_submissions: u32,
}
impl Default for BatchBudget {
    fn default() -> Self {
        Self {
            max_operations: 64,
            max_output_bytes: MAX_BATCH_OUTPUT_BYTES,
            max_decisions_submissions: 8,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BatchSearchMode {
    #[default]
    Regex,
    Lexical,
    Hybrid,
    Semantic,
}
impl BatchSearchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Regex => "regex",
            Self::Lexical => "lexical",
            Self::Hybrid => "hybrid",
            Self::Semantic => "semantic",
        }
    }
    fn paid(self) -> bool {
        matches!(self, Self::Hybrid | Self::Semantic)
    }
}
fn default_limit() -> usize {
    20
}
fn default_page_size() -> usize {
    100
}
fn default_read_bytes() -> usize {
    4096
}
fn default_candidates() -> usize {
    24
}
fn default_routing() -> usize {
    32
}
fn default_min_score() -> f64 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchOperation {
    Search {
        id: String,
        query: String,
        #[serde(default)]
        mode: BatchSearchMode,
        #[serde(default)]
        document: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        context: usize,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default)]
        literal: bool,
        #[serde(default = "default_candidates")]
        max_candidates: usize,
        #[serde(default = "default_routing")]
        routing_docs: usize,
        #[serde(default = "default_min_score")]
        min_score: f64,
    },
    Catalog {
        id: String,
        #[serde(default)]
        fields: Vec<String>,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    Tree {
        id: String,
        document: String,
        #[serde(default)]
        fields: Vec<String>,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    ReadNode {
        id: String,
        node_id: String,
        #[serde(default = "default_read_bytes")]
        max_bytes: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
}
impl BatchOperation {
    pub fn id(&self) -> &str {
        match self {
            Self::Search { id, .. }
            | Self::Catalog { id, .. }
            | Self::Tree { id, .. }
            | Self::ReadNode { id, .. } => id,
        }
    }
    fn cursor_options(&self) -> Value {
        match self {
            Self::Catalog {
                fields, page_size, ..
            } => json!({"fields":fields,"page_size":page_size}),
            Self::Tree {
                fields, page_size, ..
            } => json!({"fields":fields,"page_size":page_size}),
            Self::ReadNode { max_bytes, .. } => json!({"max_bytes":max_bytes}),
            Self::Search { .. } => Value::Null,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchErrorCode {
    InputIoFailure,
    InputTooLarge,
    InvalidRequest,
    DuplicateOperationId,
    InvalidGeneration,
    SharedIntegrity,
    InvalidRegex,
    ResourceNotFound,
    SourceStale,
    InvalidCursor,
    InvalidFields,
    OutputBudgetInsufficient,
    InvalidOperation,
    NotExecuted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchError {
    pub code: BatchErrorCode,
    pub message: String,
}
fn error(code: BatchErrorCode, message: &str) -> BatchError {
    BatchError {
        code,
        message: message.into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItemCoverage {
    pub unit: String,
    pub available: usize,
    pub inspected: usize,
    pub selected: usize,
    pub omitted: usize,
    pub start: usize,
    pub end: usize,
    pub exhausted: bool,
    pub budget_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
impl Default for BatchItemCoverage {
    fn default() -> Self {
        Self {
            unit: "objects".into(),
            available: 0,
            inspected: 0,
            selected: 0,
            omitted: 0,
            start: 0,
            end: 0,
            exhausted: false,
            budget_truncated: false,
            cursor: None,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Ok,
    Error,
    NotExecuted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItem {
    pub id: String,
    pub status: BatchItemStatus,
    pub reserved_output_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
    pub coverage: BatchItemCoverage,
    /// Model counters belong to this item, never a copied shared budget.
    pub metrics: crate::Metrics,
}
impl BatchItem {
    fn failed(id: &str, quota: usize, failure: BatchError) -> Self {
        Self {
            id: id.into(),
            status: BatchItemStatus::Error,
            reserved_output_bytes: quota,
            result: None,
            error: Some(failure),
            coverage: Default::default(),
            metrics: Default::default(),
        }
    }
    fn pending(id: &str, quota: usize) -> Self {
        let mut item = Self::failed(
            id,
            quota,
            error(BatchErrorCode::NotExecuted, "Operation was not executed"),
        );
        item.status = BatchItemStatus::NotExecuted;
        item
    }
    fn success(id: &str, quota: usize, result: Value, coverage: BatchItemCoverage) -> Self {
        Self {
            id: id.into(),
            status: BatchItemStatus::Ok,
            reserved_output_bytes: quota,
            result: Some(result),
            error: None,
            coverage,
            metrics: Default::default(),
        }
    }
    fn encoded_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |v| v.len())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchExecution {
    pub scope: String,
    pub run_id: Option<String>,
    pub plan_digest: Option<String>,
    pub binding_status: String,
    /// These observe the caller's shared context, which may include other callers.
    pub before: Option<ExecutionSnapshot>,
    pub after: Option<ExecutionSnapshot>,
}
impl BatchExecution {
    fn capture(client: Option<&JevClient>) -> Self {
        let snapshot = client.and_then(|c| c.execution_context().snapshot().ok());
        Self {
            scope: snapshot
                .as_ref()
                .map_or(
                    if client.is_some() {
                        "execution_snapshot_unavailable"
                    } else {
                        "no_model_context"
                    },
                    |s| s.scope.as_str(),
                )
                .into(),
            run_id: client
                .and_then(|c| c.execution_context().run_id())
                .map(str::to_owned),
            plan_digest: client
                .and_then(|c| c.execution_context().plan_digest())
                .map(str::to_owned),
            binding_status: if client.is_some() && snapshot.is_none() {
                "execution_snapshot_unavailable"
            } else if snapshot
                .as_ref()
                .is_some_and(|s| s.run_id.is_some() && s.plan_digest.is_some())
            {
                "explicit_immutable_context"
            } else {
                "unbound_invocation"
            }
            .into(),
            before: snapshot,
            after: None,
        }
    }
    fn finish(&mut self, client: Option<&JevClient>) {
        self.after = client.and_then(|c| c.execution_context().snapshot().ok());
        if client.is_some() && self.after.is_none() {
            self.binding_status = "execution_snapshot_unavailable".into();
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCoverage {
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub not_executed: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchMetrics {
    pub elapsed_ms: u128,
    pub shared_reader: bool,
    pub reserved_item_output_bytes: usize,
    pub model_submissions: usize,
    pub provider_usage: Vec<Value>,
    /// The bounded text LRU and physical-read accounting are a subsequent integration.
    pub physical_bytes: Option<usize>,
    pub cached_logical_bytes: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub schema_version: String,
    pub generation: GenerationPin,
    pub execution: BatchExecution,
    pub items: Vec<BatchItem>,
    pub coverage: BatchCoverage,
    pub metrics: BatchMetrics,
}
/// Global failures retain completed items and retrievable caller-context accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchFailure {
    pub schema_version: String,
    pub error: BatchError,
    pub generation: Option<GenerationPin>,
    pub execution: BatchExecution,
    pub items: Vec<BatchItem>,
}
impl std::fmt::Display for BatchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error.message)
    }
}
impl std::error::Error for BatchFailure {}
fn global(
    failure: BatchError,
    pin: Option<GenerationPin>,
    execution: BatchExecution,
    items: Vec<BatchItem>,
) -> Box<BatchFailure> {
    Box::new(BatchFailure {
        schema_version: BATCH_RESPONSE_SCHEMA.into(),
        error: failure,
        generation: pin,
        execution,
        items,
    })
}

/// Bounded typed failure for unavailable stdin/file input. No paths or OS errors
/// are included, and no generation or model execution context has been opened.
pub fn input_io_failure() -> Box<BatchFailure> {
    global(
        error(
            BatchErrorCode::InputIoFailure,
            "Batch input could not be read",
        ),
        None,
        BatchExecution::capture(None),
        vec![],
    )
}

pub fn parse_batch_request(bytes: &[u8]) -> Result<BatchRequest, Box<BatchFailure>> {
    let empty = || BatchExecution::capture(None);
    if bytes.len() > MAX_BATCH_INPUT_BYTES {
        return Err(global(
            error(BatchErrorCode::InputTooLarge, "Batch input exceeds 1 MiB"),
            None,
            empty(),
            vec![],
        ));
    }
    let request: BatchRequest = serde_json::from_slice(bytes).map_err(|_| {
        global(
            error(
                BatchErrorCode::InvalidRequest,
                "Batch request has invalid or unsupported schema fields",
            ),
            None,
            empty(),
            vec![],
        )
    })?;
    validate_request(&request).map_err(|e| global(e, None, empty(), vec![]))?;
    Ok(request)
}
fn validate_request(request: &BatchRequest) -> Result<(), BatchError> {
    if request.schema_version != BATCH_REQUEST_SCHEMA
        || request.operations.is_empty()
        || !(1..=64).contains(&request.budget.max_operations)
        || request.operations.len() > request.budget.max_operations
        || !(1..=MAX_BATCH_OUTPUT_BYTES).contains(&request.budget.max_output_bytes)
        || request.budget.max_decisions_submissions > 200
    {
        return Err(error(
            BatchErrorCode::InvalidRequest,
            "Batch schema or declared limits are invalid",
        ));
    }
    if serde_json::to_vec(request).map_or(true, |b| b.len() > MAX_BATCH_INPUT_BYTES) {
        return Err(error(
            BatchErrorCode::InputTooLarge,
            "Batch input exceeds 1 MiB",
        ));
    }
    if minimum_batch_output_bytes(request.operations.len())
        .is_none_or(|minimum| request.budget.max_output_bytes < minimum)
    {
        return Err(error(
            BatchErrorCode::OutputBudgetInsufficient,
            "Output budget requires a 4096-byte envelope and 1024 bytes per operation",
        ));
    }
    let mut ids = BTreeSet::new();
    for operation in &request.operations {
        let id = operation.id();
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
            return Err(error(
                BatchErrorCode::InvalidRequest,
                "Operation IDs require 1 to 128 noncontrol bytes",
            ));
        }
        if !ids.insert(id) {
            return Err(error(
                BatchErrorCode::DuplicateOperationId,
                "Operation IDs must be unique",
            ));
        }
    }
    if let BatchGeneration::Exact(pin) = &request.generation
        && (pin.generation.is_empty()
            || pin.generation.len() > 256
            || !pin
                .generation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || pin.manifest_sha256.len() != 64
            || !pin.manifest_sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(error(
            BatchErrorCode::InvalidGeneration,
            "Exact generation pin is invalid",
        ));
    }
    Ok(())
}

pub async fn dispatch_batch(
    root: &Path,
    request: BatchRequest,
    client: Option<&JevClient>,
) -> Result<BatchResponse, Box<BatchFailure>> {
    let mut execution = BatchExecution::capture(client);
    validate_request(&request).map_err(|e| {
        execution.finish(client);
        global(e, None, execution.clone(), vec![])
    })?;
    let reader = match &request.generation {
        BatchGeneration::Current(_) => GenerationReader::open(root),
        BatchGeneration::Exact(pin) => GenerationReader::open_pinned(root, pin),
    }
    .map_err(|_| {
        execution.finish(client);
        global(
            error(
                BatchErrorCode::SharedIntegrity,
                "Generation or shared index integrity validation failed",
            ),
            None,
            execution,
            vec![],
        )
    })?;
    dispatch_batch_with_reader(&reader, request, client).await
}
pub async fn dispatch_batch_with_reader(
    reader: &GenerationReader,
    request: BatchRequest,
    client: Option<&JevClient>,
) -> Result<BatchResponse, Box<BatchFailure>> {
    let started = Instant::now();
    let pin = reader.pin();
    let mut execution = BatchExecution::capture(client);
    validate_request(&request).map_err(|e| {
        execution.finish(client);
        global(e, Some(pin.clone()), execution.clone(), vec![])
    })?;
    if matches!(&request.generation,BatchGeneration::Exact(expected) if expected!=&pin) {
        execution.finish(client);
        return Err(global(
            error(
                BatchErrorCode::InvalidGeneration,
                "Supplied reader does not match the exact requested pin",
            ),
            Some(pin),
            execution,
            vec![],
        ));
    }
    shared_integrity(reader).map_err(|e| {
        execution.finish(client);
        global(e, Some(pin.clone()), execution.clone(), vec![])
    })?;
    let count = request.operations.len();
    let remaining = request
        .budget
        .max_output_bytes
        .checked_sub(ENVELOPE_RESERVE_BYTES)
        .filter(|b| *b / count >= MIN_ITEM_OUTPUT_BYTES)
        .ok_or_else(|| {
            execution.finish(client);
            global(
                error(
                    BatchErrorCode::OutputBudgetInsufficient,
                    "Output budget cannot reserve complete responses for every operation",
                ),
                Some(pin.clone()),
                execution.clone(),
                vec![],
            )
        })?;
    let quotas: Vec<_> = (0..count)
        .map(|i| remaining / count + usize::from(i < remaining % count))
        .collect();
    let mut items: Vec<_> = request
        .operations
        .iter()
        .zip(&quotas)
        .map(|(op, q)| BatchItem::pending(op.id(), *q))
        .collect();
    // Reservations are fixed in input order before any operation executes. This
    // initial implementation is sequential; it does not multiply model schedulers.
    for (index, operation) in request.operations.iter().enumerate() {
        if let Err(failure) = shared_integrity(reader) {
            execution.finish(client);
            return Err(global(failure, Some(pin), execution, items));
        }
        let result = execute_operation(reader, operation, quotas[index]).await;
        if let Err(failure) = shared_integrity(reader) {
            execution.finish(client);
            return Err(global(failure, Some(pin), execution, items));
        }
        items[index] = match result {
            Ok(item) => item,
            Err(failure) if failure.code == BatchErrorCode::SharedIntegrity => {
                execution.finish(client);
                return Err(global(failure, Some(pin), execution, items));
            }
            Err(failure) => BatchItem::failed(operation.id(), quotas[index], failure),
        };
        if items[index].encoded_bytes() > quotas[index] {
            items[index] = BatchItem::failed(
                operation.id(),
                quotas[index],
                error(
                    BatchErrorCode::OutputBudgetInsufficient,
                    "Complete operation output exceeds its reserved share",
                ),
            );
        }
    }
    shared_integrity(reader).map_err(|failure| {
        execution.finish(client);
        global(failure, Some(pin.clone()), execution.clone(), items.clone())
    })?;
    execution.finish(client);
    let response = BatchResponse {
        schema_version: BATCH_RESPONSE_SCHEMA.into(),
        generation: pin,
        execution,
        coverage: BatchCoverage {
            requested: count,
            succeeded: items
                .iter()
                .filter(|i| i.status == BatchItemStatus::Ok)
                .count(),
            failed: items
                .iter()
                .filter(|i| i.status == BatchItemStatus::Error)
                .count(),
            not_executed: items
                .iter()
                .filter(|i| i.status == BatchItemStatus::NotExecuted)
                .count(),
        },
        metrics: BatchMetrics {
            elapsed_ms: started.elapsed().as_millis(),
            shared_reader: true,
            reserved_item_output_bytes: quotas.iter().sum(),
            model_submissions: 0,
            provider_usage: vec![],
            physical_bytes: None,
            cached_logical_bytes: None,
        },
        items,
    };
    if serde_json::to_vec(&response).map_or(true, |b| b.len() > request.budget.max_output_bytes) {
        return Err(global(
            error(
                BatchErrorCode::OutputBudgetInsufficient,
                "Complete batch output exceeds its reserved envelope",
            ),
            Some(response.generation),
            response.execution,
            response.items,
        ));
    }
    Ok(response)
}
fn shared_integrity(reader: &GenerationReader) -> Result<(), BatchError> {
    reader
        .validate()
        .and_then(|_| reader.index_session()?.validate())
        .map_err(|_| {
            error(
                BatchErrorCode::SharedIntegrity,
                "Retained generation manifest or shared index changed",
            )
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorBody {
    version: String,
    pin: GenerationPin,
    lane: String,
    scope: String,
    options_sha256: String,
    next_position: usize,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorEnvelope {
    payload: CursorBody,
    sha256: String,
}
fn hash(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("bounded serializable DTO"))
    )
}
fn cursor(
    reader: &GenerationReader,
    lane: &str,
    scope: &str,
    options: &Value,
    next: usize,
) -> String {
    let payload = CursorBody {
        version: "gptgrep.cursor.v1".into(),
        pin: reader.pin(),
        lane: lane.into(),
        scope: scope.into(),
        options_sha256: hash(options),
        next_position: next,
    };
    serde_json::to_string(&CursorEnvelope {
        sha256: hash(&payload),
        payload,
    })
    .expect("bounded cursor DTO")
}
fn cursor_start(
    reader: &GenerationReader,
    lane: &str,
    scope: &str,
    options: &Value,
    token: Option<&str>,
) -> Result<usize, BatchError> {
    let Some(token) = token else { return Ok(0) };
    let bad = || {
        error(
            BatchErrorCode::InvalidCursor,
            "Cursor is damaged or does not match this generation, scope and options",
        )
    };
    if token.len() > MAX_BATCH_CURSOR_BYTES {
        return Err(bad());
    }
    let decoded: CursorEnvelope = serde_json::from_str(token).map_err(|_| bad())?;
    if decoded.sha256 != hash(&decoded.payload)
        || decoded.payload.version != "gptgrep.cursor.v1"
        || decoded.payload.pin != reader.pin()
        || decoded.payload.lane != lane
        || decoded.payload.scope != scope
        || decoded.payload.options_sha256 != hash(options)
    {
        return Err(bad());
    }
    Ok(decoded.payload.next_position)
}
fn valid_fields(fields: &[String], allowed: &[&str]) -> Result<(), BatchError> {
    let mut unique = BTreeSet::new();
    if fields.len() > 32
        || fields
            .iter()
            .any(|f| !allowed.contains(&f.as_str()) || !unique.insert(f))
    {
        return Err(error(
            BatchErrorCode::InvalidFields,
            "Field selection contains unsupported or duplicate fields",
        ));
    }
    Ok(())
}
fn project(value: &Value, fields: &[String], required: &[&str], prefix: &str) -> Value {
    if fields.is_empty() {
        return value.clone();
    }
    project_selected(value, fields, required, prefix)
}
fn project_selected(value: &Value, fields: &[String], required: &[&str], prefix: &str) -> Value {
    let mut result = serde_json::Map::new();
    for field in required
        .iter()
        .copied()
        .chain(fields.iter().filter_map(|f| f.strip_prefix(prefix)))
    {
        if let Some(value) = value.get(field) {
            result.insert(field.into(), value.clone());
        }
    }
    Value::Object(result)
}
fn coverage(
    total: usize,
    start: usize,
    end: usize,
    unit: &str,
    budget_truncated: bool,
    next: Option<String>,
) -> BatchItemCoverage {
    BatchItemCoverage {
        unit: unit.into(),
        available: total,
        inspected: end - start,
        selected: end - start,
        omitted: total - (end - start),
        start,
        end,
        exhausted: end == total,
        budget_truncated,
        cursor: next,
    }
}
struct Enumeration<'a> {
    pointer: &'a str,
    lane: &'a str,
    scope: &'a str,
    page_size: usize,
    token: Option<&'a str>,
}
fn enumerate(
    reader: &GenerationReader,
    op: &BatchOperation,
    quota: usize,
    mut base: Value,
    objects: Vec<Value>,
    enumeration: Enumeration<'_>,
) -> Result<BatchItem, BatchError> {
    let Enumeration {
        pointer,
        lane,
        scope,
        page_size,
        token,
    } = enumeration;
    if !(1..=1000).contains(&page_size) {
        return Err(error(
            BatchErrorCode::InvalidOperation,
            "Page size must be 1 to 1000",
        ));
    }
    let options = op.cursor_options();
    let start = cursor_start(reader, lane, scope, &options, token)?;
    if start > objects.len() {
        return Err(error(
            BatchErrorCode::InvalidCursor,
            "Cursor position exceeds the deterministic enumeration",
        ));
    }
    let limit = start.saturating_add(page_size).min(objects.len());
    let mut selected = Vec::new();
    let mut latest = None;
    for end in start..=limit {
        if end > start {
            selected.push(objects[end - 1].clone())
        }
        *base.pointer_mut(pointer).expect("known enumeration array") =
            Value::Array(selected.clone());
        let next = (end < objects.len()).then(|| cursor(reader, lane, scope, &options, end));
        let item = BatchItem::success(
            op.id(),
            quota,
            base.clone(),
            coverage(objects.len(), start, end, "objects", end < limit, next),
        );
        if item.encoded_bytes() > quota {
            break;
        }
        latest = Some(item);
    }
    let item = latest.ok_or_else(|| {
        error(
            BatchErrorCode::OutputBudgetInsufficient,
            "Enumeration metadata exceeds the reserved output share",
        )
    })?;
    if item.coverage.end == start && start < objects.len() {
        return Err(error(
            BatchErrorCode::OutputBudgetInsufficient,
            "Reserved output cannot hold the next complete object and cursor",
        ));
    }
    Ok(item)
}

async fn execute_operation(
    reader: &GenerationReader,
    op: &BatchOperation,
    quota: usize,
) -> Result<BatchItem, BatchError> {
    match op {
        BatchOperation::Catalog {
            fields,
            page_size,
            cursor: token,
            ..
        } => {
            valid_fields(
                fields,
                &[
                    "id",
                    "path",
                    "title",
                    "pages",
                    "nodes",
                    "parser",
                    "source_sha256",
                ],
            )?;
            let mut result = reader.catalog().map_err(reader_error)?;
            let objects = result["documents"]
                .as_array()
                .expect("catalog schema")
                .iter()
                .map(|v| project(v, fields, &["id", "path", "source_sha256"], ""))
                .collect();
            result["documents"] = json!([]);
            enumerate(
                reader,
                op,
                quota,
                result,
                objects,
                Enumeration {
                    pointer: "/documents",
                    lane: "catalog",
                    scope: "indexed_documents",
                    page_size: *page_size,
                    token: token.as_deref(),
                },
            )
        }
        BatchOperation::Tree {
            document,
            fields,
            page_size,
            cursor: token,
            ..
        } => {
            valid_document(document)?;
            valid_fields(
                fields,
                &[
                    "id",
                    "path",
                    "title",
                    "source_sha256",
                    "source_bytes",
                    "text_sha256",
                    "parser",
                    "pages",
                    "warnings",
                    "nodes.id",
                    "nodes.parent_id",
                    "nodes.title",
                    "nodes.level",
                    "nodes.page_start",
                    "nodes.page_end",
                    "nodes.line_start",
                    "nodes.line_end",
                    "nodes.key_items",
                    "nodes.pages",
                ],
            )?;
            let mut result = reader.tree(Path::new(document)).map_err(reader_error)?;
            let original = result["document"].clone();
            let node_fields: Vec<String> = fields
                .iter()
                .filter(|f| f.starts_with("nodes."))
                .cloned()
                .collect();
            let objects = original["nodes"]
                .as_array()
                .expect("tree schema")
                .iter()
                .map(|v| {
                    let mut node = if fields.is_empty() {
                        v.clone()
                    } else {
                        project_selected(
                            v,
                            &node_fields,
                            &["id", "line_start", "line_end", "page_start", "page_end"],
                            "nodes.",
                        )
                    };
                    if node_fields.iter().any(|f| f == "nodes.pages") {
                        node["pages"] = Value::Array(
                            original["pages"]
                                .as_array()
                                .expect("page schema")
                                .iter()
                                .filter_map(|p| p["number"].as_u64())
                                .filter(|p| {
                                    *p >= v["page_start"].as_u64().unwrap_or(0)
                                        && *p <= v["page_end"].as_u64().unwrap_or(0)
                                })
                                .map(Value::from)
                                .collect(),
                        );
                    }
                    node
                })
                .collect();
            if !fields.is_empty() {
                result["document"] =
                    project(&original, fields, &["id", "path", "source_sha256"], "");
            }
            result["document"]["nodes"] = json!([]);
            enumerate(
                reader,
                op,
                quota,
                result,
                objects,
                Enumeration {
                    pointer: "/document/nodes",
                    lane: "tree",
                    scope: document,
                    page_size: *page_size,
                    token: token.as_deref(),
                },
            )
        }
        BatchOperation::ReadNode {
            node_id,
            max_bytes,
            cursor: token,
            ..
        } => {
            if node_id.len() > 512 || !(1..=65536).contains(max_bytes) {
                return Err(error(
                    BatchErrorCode::InvalidOperation,
                    "Read node ID or max_bytes is invalid",
                ));
            }
            let options = op.cursor_options();
            let start = cursor_start(reader, "read_node", node_id, &options, token.as_deref())?;
            let mut bytes = *max_bytes;
            loop {
                let hit = reader
                    .read_node_window(node_id, bytes, start)
                    .map_err(reader_error)?;
                let node = hit.node_coverage.expect("read node coverage");
                let total = node.unread_before_bytes + hit.text.len() + node.unread_after_bytes;
                let end = start + hit.text.len();
                let next = hit
                    .next_offset
                    .map(|n| cursor(reader, "read_node", node_id, &options, n));
                let item = BatchItem::success(
                    op.id(),
                    quota,
                    serde_json::to_value(&hit).expect("Hit DTO"),
                    coverage(total, start, end, "bytes", bytes < *max_bytes, next),
                );
                if item.encoded_bytes() <= quota {
                    return Ok(item);
                }
                if bytes <= 4 {
                    return Err(error(
                        BatchErrorCode::OutputBudgetInsufficient,
                        "Reserved output cannot hold a complete read window and cursor",
                    ));
                }
                bytes = (bytes / 2).max(4);
            }
        }
        BatchOperation::Search {
            query,
            mode,
            document,
            limit,
            context,
            case_insensitive,
            literal,
            max_candidates,
            routing_docs,
            min_score,
            ..
        } => {
            let options = SearchOptions {
                mode: mode.as_str().into(),
                document: document.clone(),
                limit: *limit,
                context: *context,
                case_insensitive: *case_insensitive,
                literal: *literal,
                max_candidates: *max_candidates,
                routing_docs: *routing_docs,
                min_score: *min_score,
                model: None,
            };
            if let Some(document) = document {
                valid_document(document)?
            }
            crate::validate_search_options(query, &options).map_err(|_| {
                error(
                    BatchErrorCode::InvalidOperation,
                    "Search options are invalid",
                )
            })?;
            if mode.paid() {
                // Provider usage alone can occupy a 1 MiB response per call and may
                // expand when embedded in result JSON. No complete worst-case model
                // result fits this foundation's 1 MiB total envelope. Reject before
                // candidate/provider work; no probabilistic continuation is issued.
                return Err(error(
                    BatchErrorCode::OutputBudgetInsufficient,
                    "Complete model output including provider usage cannot be reserved within this batch share",
                ));
            }
            let report = reader.search(query, &options).await.map_err(reader_error)?;
            let total = report.hits.len();
            let source_stale = !report.coverage.stale_files.is_empty();
            let mut item = BatchItem::success(
                op.id(),
                quota,
                serde_json::to_value(&report).expect("SearchReport DTO"),
                coverage(total, 0, total, "hits", false, None),
            );
            item.metrics = report.metrics;
            if source_stale {
                item.status = BatchItemStatus::Error;
                item.error = Some(error(
                    BatchErrorCode::SourceStale,
                    "Search encountered a changed or disappeared source; reindex before reading",
                ));
            }
            if item.encoded_bytes() > quota {
                return Err(error(
                    BatchErrorCode::OutputBudgetInsufficient,
                    "Complete deterministic search result exceeds its reserved output share",
                ));
            }
            Ok(item)
        }
    }
}
fn valid_document(document: &str) -> Result<(), BatchError> {
    if document.is_empty()
        || document.len() > 4096
        || document.chars().any(char::is_control)
        || !Path::new(document)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(error(
            BatchErrorCode::InvalidOperation,
            "Document must be a bounded source-relative path",
        ));
    }
    Ok(())
}
fn reader_error(error_value: anyhow::Error) -> BatchError {
    if error_value.is::<regex::Error>() {
        return error(
            BatchErrorCode::InvalidRegex,
            "Regular expression is invalid",
        );
    }
    let message = format!("{error_value:#}");
    if message.contains("source changed") || message.contains("source is stale") {
        return error(
            BatchErrorCode::SourceStale,
            "Source changed or disappeared; reindex before reading",
        );
    }
    if message.contains("unknown document")
        || message.contains("unknown tree node")
        || message.contains("not present in current index")
    {
        return error(
            BatchErrorCode::ResourceNotFound,
            "Requested document or node is absent from the pinned generation",
        );
    }
    if message.contains("digest mismatch")
        || message.contains("text and tree")
        || message.contains("indexed text is not UTF-8")
        || message.contains("tree does not cover")
        || message.contains("trigram verification mismatch")
        || message.contains("normalized snapshot")
        || message.contains("snapshot path became a symlink")
        || message.contains("normalized text root changed")
        || message.contains("tree node range lies outside canonical text")
        || error_value.is::<std::io::Error>()
    {
        return error(
            BatchErrorCode::SharedIntegrity,
            "Pinned canonical evidence or shared index is corrupt",
        );
    }
    error(
        BatchErrorCode::InvalidOperation,
        "Operation failed validation or could not read its resource",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use gptgrep_jev::{Candidate, ExecutionContext};
    use std::{
        fs,
        time::{Duration, SystemTime},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn input_io_failure_has_static_bounded_json_and_no_execution_context() {
        let failure = input_io_failure();
        assert_eq!(failure.error.code, BatchErrorCode::InputIoFailure);
        assert_eq!(failure.error.message, "Batch input could not be read");
        assert!(failure.generation.is_none());
        assert!(failure.items.is_empty());
        assert_eq!(failure.execution.scope, "no_model_context");
        assert!(failure.execution.run_id.is_none());
        assert!(failure.execution.plan_digest.is_none());
        assert!(failure.execution.before.is_none());
        assert!(failure.execution.after.is_none());
        let bytes = serde_json::to_vec(&failure).unwrap();
        assert!(bytes.len() <= MAX_BATCH_DIAGNOSTIC_BYTES);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema_version"], BATCH_RESPONSE_SCHEMA);
        assert_eq!(value["error"]["code"], "input_io_failure");
    }

    fn request(operations: Value) -> BatchRequest {
        parse_batch_request(
            &serde_json::to_vec(&json!({
                "schema_version": BATCH_REQUEST_SCHEMA, "operations": operations
            }))
            .unwrap(),
        )
        .unwrap()
    }
    async fn fixture() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("a.md"),
            "# Alpha\nneedle β 中\n## Nested\nneedle second\n",
        )?;
        fs::write(dir.path().join("b.txt"), "needle other\n")?;
        fs::write(dir.path().join("c.txt"), "last document\n")?;
        crate::index(dir.path(), 10).await?;
        Ok(dir)
    }
    fn comparable(mut value: Value) -> Value {
        value["metrics"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms");
        value
    }
    fn client() -> JevClient {
        JevClient::new("synthetic-offline-test-key", None)
            .unwrap()
            .with_execution_context(
                ExecutionContext::new(
                    "invented-batch-run",
                    &"a".repeat(64),
                    SystemTime::now() + Duration::from_secs(30),
                    2,
                )
                .unwrap(),
            )
    }

    #[test]
    fn strict_schema_ids_and_all_request_bounds_reject() {
        for value in [
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"operations":[{"id":"a","op":"catalog","unknown":1}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"unknown":1,"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"budget":{"unknown":1},"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"operations":[{"id":"a","op":"catalog"},{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"generation":{"generation":"../bad","manifest_sha256":"a".repeat(64)},"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"generation":{"generation":"valid","manifest_sha256":"a".repeat(64),"unknown":1},"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"budget":{"max_output_bytes":1048577},"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"budget":{"max_decisions_submissions":201},"operations":[{"id":"a","op":"catalog"}]}),
            json!({"schema_version":BATCH_REQUEST_SCHEMA,"operations":(0..65).map(|i|json!({"id":i.to_string(),"op":"catalog"})).collect::<Vec<_>>()}),
        ] {
            assert!(parse_batch_request(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        assert!(parse_batch_request(&vec![b' '; MAX_BATCH_INPUT_BYTES + 1]).is_err());
        assert!(parse_batch_request(br#"{"schema_version":"gptgrep.batch.request.v1","operations":[{"id":"first","id":"second","op":"catalog"}]}"#).is_err());
    }

    #[tokio::test]
    async fn deterministic_batch_matches_single_fields_and_keeps_item_order() -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let doc = &reader.manifest.documents[0];
        let node_id = format!("{}:{}", doc.id, doc.nodes[0].id);
        let operations = json!([
            {"id":"catalog","op":"catalog"},
            {"id":"tree","op":"tree","document":"a.md"},
            {"id":"regex","op":"search","query":"needle","mode":"regex"},
            {"id":"bad-regex","op":"search","query":"[","mode":"regex"},
            {"id":"lexical","op":"search","query":"needle","mode":"lexical"},
            {"id":"read","op":"read_node","node_id":node_id,"max_bytes":65536},
            {"id":"missing","op":"tree","document":"absent.txt"}
        ]);
        let req = request(operations);
        let client = client();
        let before = client.execution_context().snapshot()?;
        let response = dispatch_batch_with_reader(&reader, req, Some(&client)).await?;
        assert_eq!(
            response
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            [
                "catalog",
                "tree",
                "regex",
                "bad-regex",
                "lexical",
                "read",
                "missing"
            ]
        );
        assert_eq!(
            response.items[0].result.as_ref().unwrap(),
            &reader.catalog()?
        );
        assert_eq!(
            response.items[1].result.as_ref().unwrap(),
            &reader.tree(Path::new("a.md"))?
        );
        for (index, mode) in [(2, "regex"), (4, "lexical")] {
            let single = reader
                .search(
                    "needle",
                    &SearchOptions {
                        mode: mode.into(),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(
                comparable(response.items[index].result.clone().unwrap()),
                comparable(serde_json::to_value(single)?)
            );
        }
        assert_eq!(
            response.items[5].result.as_ref().unwrap(),
            &serde_json::to_value(reader.read_node(&node_id, 65536)?)?
        );
        assert_eq!(
            response.items[3].error.as_ref().unwrap().code,
            BatchErrorCode::InvalidRegex
        );
        assert_eq!(
            response.items[6].error.as_ref().unwrap().code,
            BatchErrorCode::ResourceNotFound
        );
        assert_eq!(response.coverage.succeeded, 5);
        assert_eq!(response.coverage.failed, 2);
        assert_eq!(client.execution_context().snapshot()?, before);
        assert!(serde_json::to_vec(&response)?.len() <= MAX_BATCH_OUTPUT_BYTES);
        Ok(())
    }

    #[tokio::test]
    async fn deterministic_cursors_cover_complete_objects_and_bind_pin_scope_options() -> Result<()>
    {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let client = client();
        let mut token = None;
        let mut documents = Vec::new();
        loop {
            let req = request(
                json!([{"id":"catalog","op":"catalog","fields":["title"],"page_size":1,"cursor":token}]),
            );
            let response = dispatch_batch_with_reader(&reader, req, Some(&client)).await?;
            let item = &response.items[0];
            assert_eq!(item.status, BatchItemStatus::Ok);
            let page = item.result.as_ref().unwrap()["documents"]
                .as_array()
                .unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].as_object().unwrap().len(), 4);
            documents.extend(page.iter().cloned());
            token = item.coverage.cursor.clone();
            if token.is_none() {
                break;
            }
        }
        let catalog = reader.catalog()?;
        let fields = vec!["title".into()];
        let expected: Vec<_> = catalog["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| project(d, &fields, &["id", "path", "source_sha256"], ""))
            .collect();
        assert_eq!(documents, expected);
        let first = dispatch_batch_with_reader(
            &reader,
            request(json!([{"id":"c","op":"catalog","page_size":1}])),
            Some(&client),
        )
        .await?;
        let token = first.items[0].coverage.cursor.clone().unwrap();
        for op in [
            json!({"id":"different-options","op":"catalog","page_size":2,"cursor":token}),
            json!({"id":"different-lane","op":"tree","document":"a.md","page_size":1,"cursor":token}),
            json!({"id":"damaged","op":"catalog","page_size":1,"cursor":format!("{token}x")}),
        ] {
            let rejected =
                dispatch_batch_with_reader(&reader, request(json!([op])), Some(&client)).await?;
            assert_eq!(
                rejected.items[0].error.as_ref().unwrap().code,
                BatchErrorCode::InvalidCursor
            );
        }
        let projected=dispatch_batch_with_reader(&reader,request(json!([{"id":"t","op":"tree","document":"a.md","fields":["title"]},{"id":"pages","op":"tree","document":"a.md","fields":["nodes.pages"]}])),Some(&client)).await?;
        let tree = projected.items[0].result.as_ref().unwrap();
        assert!(
            tree["document"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n.as_object().unwrap().len() == 5)
        );
        assert!(
            projected.items[1].result.as_ref().unwrap()["document"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n["pages"].is_array())
        );
        assert_eq!(
            client.execution_context().snapshot()?.logical_reservations,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn utf8_read_cursors_replay_exact_windows_without_model_calls() -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let doc = &reader.manifest.documents[0];
        let id = format!("{}:{}", doc.id, doc.nodes[0].id);
        let full = reader.read_node(&id, 65536)?;
        let mut token = None;
        let mut text = String::new();
        let mut offset = 0;
        loop {
            let response=dispatch_batch_with_reader(&reader,request(json!([{"id":"read","op":"read_node","node_id":id,"max_bytes":4,"cursor":token}])),None).await?;
            let item = &response.items[0];
            assert_eq!(item.status, BatchItemStatus::Ok);
            let hit = item.result.as_ref().unwrap();
            assert_eq!(
                hit,
                &serde_json::to_value(reader.read_node_window(&id, 4, offset)?)?
            );
            let window = hit["text"].as_str().unwrap();
            assert!(!window.is_empty());
            text.push_str(window);
            offset += window.len();
            token = item.coverage.cursor.clone();
            if token.is_none() {
                break;
            }
        }
        assert_eq!(text, full.text);
        Ok(())
    }

    #[tokio::test]
    async fn publication_never_migrates_batch_pin_or_cursor() -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let pin = reader.pin();
        let first = dispatch_batch_with_reader(
            &reader,
            request(json!([{"id":"c","op":"catalog","page_size":1}])),
            None,
        )
        .await?;
        let token = first.items[0].coverage.cursor.clone().unwrap();
        fs::write(dir.path().join("d.txt"), "new needle\n")?;
        let request = request(
            json!([{"id":"s","op":"search","query":"needle"},{"id":"c","op":"catalog","page_size":1,"cursor":token}]),
        );
        let (published, batch) = tokio::join!(
            crate::index(dir.path(), 10),
            dispatch_batch_with_reader(&reader, request.clone(), None)
        );
        assert_ne!(published?.generation, pin.generation);
        let batch = batch?;
        assert_eq!(batch.generation, pin);
        assert_eq!(batch.items[1].status, BatchItemStatus::Ok);
        assert_eq!(
            batch.items[0].result.as_ref().unwrap()["generation"],
            pin.generation
        );
        let latest = GenerationReader::open(dir.path())?;
        let rejected = dispatch_batch_with_reader(&latest, request, None).await?;
        assert_eq!(
            rejected.items[1].error.as_ref().unwrap().code,
            BatchErrorCode::InvalidCursor
        );
        fs::write(dir.path().join(".gptgrep/CURRENT.json"), "broken")?;
        let mut exact = request_clone_for_catalog();
        exact.generation = BatchGeneration::Exact(pin);
        assert!(dispatch_batch(dir.path(), exact, None).await.is_ok());
        Ok(())
    }
    fn request_clone_for_catalog() -> BatchRequest {
        request(json!([{"id":"c","op":"catalog"}]))
    }

    #[tokio::test]
    async fn fair_output_reservations_precede_work_and_paid_search_rejects_zero_calls() -> Result<()>
    {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let client = client();
        let doc = &reader.manifest.documents[0];
        let node = format!("{}:{}", doc.id, doc.nodes[0].id);
        let mut req = request(json!([
            {"id":"model","op":"search","query":"needle","mode":"hybrid"},
            {"id":"semantic","op":"search","query":"needle","mode":"semantic"},
            {"id":"read","op":"read_node","node_id":node,"max_bytes":65536},
            {"id":"catalog","op":"catalog","fields":["title"],"page_size":1}
        ]));
        req.budget.max_output_bytes = 16384;
        req.budget.max_decisions_submissions = 0;
        let response = dispatch_batch_with_reader(&reader, req.clone(), Some(&client)).await?;
        assert_eq!(
            response.items[0].error.as_ref().unwrap().code,
            BatchErrorCode::OutputBudgetInsufficient
        );
        assert_eq!(
            response.items[1].error.as_ref().unwrap().code,
            BatchErrorCode::OutputBudgetInsufficient
        );
        assert_eq!(response.items[3].status, BatchItemStatus::Ok);
        let quotas: Vec<_> = response
            .items
            .iter()
            .map(|i| i.reserved_output_bytes)
            .collect();
        assert!(quotas.iter().all(|q| *q == 3072));
        assert!(
            response
                .items
                .iter()
                .all(|i| i.encoded_bytes() <= i.reserved_output_bytes)
        );
        assert!(serde_json::to_vec(&response)?.len() <= req.budget.max_output_bytes);
        let before = client.execution_context().snapshot()?;
        req.budget.max_output_bytes = 4096;
        let failed = dispatch_batch_with_reader(&reader, req, Some(&client))
            .await
            .unwrap_err();
        assert_eq!(failed.error.code, BatchErrorCode::OutputBudgetInsufficient);
        assert_eq!(client.execution_context().snapshot()?, before);
        assert_eq!(before.logical_reservations, 0);
        assert_eq!(before.send_start_observations, 0);
        Ok(())
    }

    #[tokio::test]
    async fn source_stale_is_item_failure_and_shared_corruption_keeps_charged_ledger() -> Result<()>
    {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        fs::write(dir.path().join("a.md"), "changed source\n")?;
        let stale = dispatch_batch_with_reader(
            &reader,
            request(json!([{"id":"t","op":"tree","document":"a.md"},{"id":"c","op":"catalog"}])),
            None,
        )
        .await?;
        assert_eq!(
            stale.items[0].error.as_ref().unwrap().code,
            BatchErrorCode::SourceStale
        );
        assert_eq!(stale.items[1].status, BatchItemStatus::Ok);
        // One real loopback HTTP failure primes the shared ledger. No internet
        // endpoint is called; direct loopback routing remains application-local.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/decisions", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 65536];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let ledger = dir.path().join("budget");
        let context = ExecutionContext::new(
            "invented-batch-run",
            &"a".repeat(64),
            SystemTime::now() + Duration::from_secs(30),
            2,
        )?
        .with_ledger(&ledger)?;
        let client = JevClient::with_endpoint("synthetic-offline-test-key", None, &endpoint)?
            .with_execution_context(context);
        assert!(
            client
                .rerank(
                    "invented-query",
                    &[Candidate {
                        id: "invented-id".into(),
                        text: "invented-text".into()
                    }]
                )
                .await
                .is_err()
        );
        server.await?;
        let before = client.execution_context().snapshot()?;
        assert_eq!(before.logical_reservations, 1);
        let bytes = fs::read(&ledger)?;
        fs::write(reader.dir.join("manifest.json"), "{}")?;
        let failed =
            dispatch_batch_with_reader(&reader, request_clone_for_catalog(), Some(&client))
                .await
                .unwrap_err();
        assert_eq!(failed.error.code, BatchErrorCode::SharedIntegrity);
        assert_eq!(failed.execution.before, Some(before.clone()));
        assert_eq!(failed.execution.after, Some(before.clone()));
        assert_eq!(client.execution_context().snapshot()?, before);
        assert_eq!(fs::read(&ledger)?, bytes);
        Ok(())
    }
    #[tokio::test]
    async fn canonical_text_and_index_corruption_fail_globally_before_success() -> Result<()> {
        for corruption in ["text", "missing-text", "index"] {
            let dir = fixture().await?;
            let reader = GenerationReader::open(dir.path())?;
            let doc = &reader.manifest.documents[0];
            if corruption == "text" {
                fs::write(
                    reader.dir.join("text").join(format!("{}.txt", doc.id)),
                    "needle altered snapshot\n",
                )?;
            } else if corruption == "missing-text" {
                fs::remove_file(reader.dir.join("text").join(format!("{}.txt", doc.id)))?;
            } else {
                fs::write(reader.dir.join("trigram/index.bin"), "altered index")?;
            }
            let client = client();
            let before = client.execution_context().snapshot()?;
            let failed=dispatch_batch_with_reader(&reader,request(json!([{"id":"s","op":"search","query":"needle","mode":"regex"},{"id":"c","op":"catalog"}])),Some(&client)).await.unwrap_err();
            assert_eq!(
                failed.error.code,
                BatchErrorCode::SharedIntegrity,
                "{corruption}"
            );
            assert!(!failed.items.iter().any(|i| i.status == BatchItemStatus::Ok));
            assert_eq!(client.execution_context().snapshot()?, before);
        }
        Ok(())
    }

    #[tokio::test]
    async fn output_truncation_has_complete_objects_and_later_item_reserved_share() -> Result<()> {
        let dir = fixture().await?;
        for i in 0..20 {
            fs::write(
                dir.path().join(format!("extra-{i:02}.txt")),
                format!("Invented title {i}\ncontent\n"),
            )?;
        }
        crate::index(dir.path(), 30).await?;
        let reader = GenerationReader::open(dir.path())?;
        let client = client();
        let mut token = None;
        let mut all = Vec::new();
        loop {
            let mut req = request(json!([
                {"id":"enumerate","op":"catalog","page_size":1000,"cursor":token},
                {"id":"later","op":"search","query":"[","mode":"regex"}
            ]));
            req.budget.max_output_bytes = 8192;
            let response = dispatch_batch_with_reader(&reader, req, Some(&client)).await?;
            assert_eq!(response.items[0].status, BatchItemStatus::Ok);
            assert_eq!(
                response.items[1].error.as_ref().unwrap().code,
                BatchErrorCode::InvalidRegex
            );
            assert_eq!(response.items[0].reserved_output_bytes, 2048);
            assert_eq!(response.items[1].reserved_output_bytes, 2048);
            assert!(response.items.iter().all(|i| i.encoded_bytes() <= 2048));
            assert!(serde_json::to_vec(&response)?.len() <= 8192);
            let item = &response.items[0];
            let documents = item.result.as_ref().unwrap()["documents"]
                .as_array()
                .unwrap();
            assert!(!documents.is_empty());
            assert!(documents.iter().all(|d| d["id"].is_string()
                && d["path"].is_string()
                && d["source_sha256"].is_string()));
            all.extend(documents.iter().cloned());
            token = item.coverage.cursor.clone();
            if token.is_none() {
                assert!(item.coverage.exhausted);
                break;
            }
            assert!(item.coverage.budget_truncated);
        }
        assert_eq!(all, *reader.catalog()?["documents"].as_array().unwrap());
        assert_eq!(
            client.execution_context().snapshot()?.logical_reservations,
            0
        );
        Ok(())
    }
    #[tokio::test]
    async fn actual_execution_bindings_and_unbound_invocation_are_reported() -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let explicit = client();
        let response =
            dispatch_batch_with_reader(&reader, request_clone_for_catalog(), Some(&explicit))
                .await?;
        assert_eq!(
            response.execution.run_id.as_deref(),
            Some("invented-batch-run")
        );
        assert_eq!(
            response.execution.plan_digest.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(
            response.execution.binding_status,
            "explicit_immutable_context"
        );
        assert_eq!(response.execution.scope, "single_invocation");
        assert_eq!(
            response.execution.before.as_ref().unwrap().run_id,
            response.execution.run_id
        );
        assert_eq!(
            response.execution.after.as_ref().unwrap().plan_digest,
            response.execution.plan_digest
        );
        let legacy = JevClient::new("synthetic-offline-test-key", None)?;
        let unbound =
            dispatch_batch_with_reader(&reader, request_clone_for_catalog(), Some(&legacy)).await?;
        assert_eq!(unbound.execution.binding_status, "unbound_invocation");
        assert!(unbound.execution.run_id.is_none());
        assert!(unbound.execution.plan_digest.is_none());
        assert!(unbound.execution.before.as_ref().unwrap().run_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn output_admission_and_diagnostics_bound_maximum_execution_bindings() -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let run_id = "\"".repeat(256);
        let digest = format!("sha256:{}", "a".repeat(64));
        let ctx = ExecutionContext::new(
            &run_id,
            &digest,
            SystemTime::now() + Duration::from_secs(30),
            2,
        )?
        .with_ledger(dir.path().join("max-binding-budget"))?;
        let client =
            JevClient::new("synthetic-offline-test-key", None)?.with_execution_context(ctx);
        let before = client.execution_context().snapshot()?;
        let operations = json!([{"id":"\\".repeat(128),"op":"tree","document":"absent.txt"}]);
        let minimum = minimum_batch_output_bytes(1).unwrap();
        assert_eq!(minimum, 5120);
        for budget in [0, 1, 32, 1024, 4096, minimum - 1] {
            let bytes = serde_json::to_vec(&json!({"schema_version":BATCH_REQUEST_SCHEMA,
                "budget":{"max_output_bytes":budget},"operations":operations}))?;
            let parse_failure = parse_batch_request(&bytes).unwrap_err();
            assert!(serde_json::to_vec(&parse_failure)?.len() <= MAX_BATCH_DIAGNOSTIC_BYTES);
            let mut req = request(operations.clone());
            req.budget.max_output_bytes = budget;
            let failure = dispatch_batch_with_reader(&reader, req, Some(&client))
                .await
                .unwrap_err();
            // Tiny declarations are rejected, not admitted output caps. The
            // rejection diagnostic has its own fixed public bound.
            assert!(serde_json::to_vec(&failure)?.len() <= MAX_BATCH_DIAGNOSTIC_BYTES);
        }
        let mut req = request(operations);
        req.budget.max_output_bytes = minimum;
        assert!(validate_request(&req).is_ok());
        let response = dispatch_batch_with_reader(&reader, req.clone(), Some(&client)).await?;
        assert_eq!(
            response.items[0].error.as_ref().unwrap().code,
            BatchErrorCode::ResourceNotFound
        );
        assert!(response.items[0].encoded_bytes() <= MIN_ITEM_OUTPUT_BYTES);
        assert!(serde_json::to_vec(&response)?.len() <= minimum);
        // The diagnostic envelope remains bounded even with the maximal count
        // of complete reserved item objects retained in a dispatched failure.
        let max_count = MAX_BATCH_OPERATIONS;
        let max_budget = minimum_batch_output_bytes(max_count).unwrap();
        let mut items = Vec::new();
        for i in 0..max_count {
            let id = format!("{i:02}{}", "\\".repeat(126));
            let mut item = BatchItem::success(
                &id,
                MIN_ITEM_OUTPUT_BYTES,
                json!({"bytes":""}),
                Default::default(),
            );
            let available = MIN_ITEM_OUTPUT_BYTES - item.encoded_bytes();
            item.result = Some(json!({"bytes":"x".repeat(available)}));
            assert_eq!(item.encoded_bytes(), MIN_ITEM_OUTPUT_BYTES);
            items.push(item);
        }
        let mut execution = BatchExecution::capture(Some(&client));
        execution.finish(Some(&client));
        let failed = global(
            error(
                BatchErrorCode::SharedIntegrity,
                "Pinned canonical evidence or shared index is corrupt",
            ),
            Some(reader.pin()),
            execution,
            items,
        );
        assert!(serde_json::to_vec(&failed)?.len() <= max_budget);
        let mut envelope = failed.clone();
        envelope.items.clear();
        assert!(serde_json::to_vec(&envelope)?.len() <= MAX_BATCH_DIAGNOSTIC_BYTES);
        fs::write(reader.dir.join("manifest.json"), "{}")?;
        let failure = dispatch_batch_with_reader(&reader, req, Some(&client))
            .await
            .unwrap_err();
        assert_eq!(failure.error.code, BatchErrorCode::SharedIntegrity);
        assert!(serde_json::to_vec(&failure)?.len() <= minimum);
        assert!(serde_json::to_vec(&failure)?.len() <= MAX_BATCH_DIAGNOSTIC_BYTES);
        assert_eq!(client.execution_context().snapshot()?, before);
        assert_eq!(minimum_batch_output_bytes(64), Some(69632));
        assert!(minimum_batch_output_bytes(usize::MAX).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn operation_field_and_cursor_errors_are_bounded_items_without_submissions() -> Result<()>
    {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        let client = client();
        let operations = json!([
            {"id":"unknown-field","op":"catalog","fields":["unknown"]},
            {"id":"duplicate-field","op":"catalog","fields":["id","id"]},
            {"id":"bad-node-field","op":"tree","document":"a.md","fields":["nodes.unknown"]},
            {"id":"page-zero","op":"catalog","page_size":0},
            {"id":"page-huge","op":"tree","document":"a.md","page_size":1001},
            {"id":"cursor-oversized","op":"catalog","cursor":"x".repeat(MAX_BATCH_CURSOR_BYTES+1)},
            {"id":"cursor-wrong-shape","op":"catalog","cursor":"[]"},
            {"id":"read-too-large","op":"read_node","node_id":"absent:absent","max_bytes":65537},
            {"id":"read-missing","op":"read_node","node_id":"absent:absent"},
            {"id":"path-escape","op":"tree","document":"../a.md"},
            {"id":"later-valid","op":"catalog","fields":["title"],"page_size":1}
        ]);
        let before = client.execution_context().snapshot()?;
        let mut req = request(operations);
        req.budget.max_output_bytes = minimum_batch_output_bytes(req.operations.len()).unwrap()
            + req.operations.len() * MIN_ITEM_OUTPUT_BYTES;
        let response = dispatch_batch_with_reader(&reader, req.clone(), Some(&client)).await?;
        assert_eq!(response.items.len(), 11);
        let expected = [
            BatchErrorCode::InvalidFields,
            BatchErrorCode::InvalidFields,
            BatchErrorCode::InvalidFields,
            BatchErrorCode::InvalidOperation,
            BatchErrorCode::InvalidOperation,
            BatchErrorCode::InvalidCursor,
            BatchErrorCode::InvalidCursor,
            BatchErrorCode::InvalidOperation,
            BatchErrorCode::ResourceNotFound,
            BatchErrorCode::InvalidOperation,
        ];
        for (item, expected) in response.items.iter().zip(expected) {
            assert_eq!(item.error.as_ref().unwrap().code, expected);
            assert!(item.encoded_bytes() <= MIN_ITEM_OUTPUT_BYTES);
        }
        assert_eq!(response.items[10].status, BatchItemStatus::Ok);
        assert!(
            response
                .items
                .iter()
                .all(|i| i.encoded_bytes() <= i.reserved_output_bytes)
        );
        assert!(serde_json::to_vec(&response)?.len() <= req.budget.max_output_bytes);
        assert_eq!(client.execution_context().snapshot()?, before);
        Ok(())
    }

    #[tokio::test]
    async fn scoped_regex_and_lexical_source_stale_items_keep_partial_reports_and_continue()
    -> Result<()> {
        let dir = fixture().await?;
        let reader = GenerationReader::open(dir.path())?;
        for mode in ["regex", "lexical"] {
            let baseline = reader
                .search(
                    "needle",
                    &SearchOptions {
                        mode: mode.into(),
                        document: Some("a.md".into()),
                        ..Default::default()
                    },
                )
                .await?;
            assert!(!baseline.hits.is_empty());
            assert!(baseline.coverage.stale_files.is_empty());
        }
        fs::write(
            dir.path().join("a.md"),
            "changed source without old matches\n",
        )?;
        let request = request(json!([
            {"id":"regex-stale","op":"search","query":"needle","mode":"regex","document":"a.md"},
            {"id":"lexical-stale","op":"search","query":"needle","mode":"lexical","document":"a.md"},
            {"id":"later-catalog","op":"catalog"}
        ]));
        let response = dispatch_batch_with_reader(&reader, request, None).await?;
        assert_eq!(
            response
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["regex-stale", "lexical-stale", "later-catalog"]
        );
        for (index, mode) in [(0, "regex"), (1, "lexical")] {
            let item = &response.items[index];
            assert_eq!(item.status, BatchItemStatus::Error);
            assert_eq!(
                item.error.as_ref().unwrap().code,
                BatchErrorCode::SourceStale
            );
            let partial = item
                .result
                .as_ref()
                .expect("bounded partial report retained");
            assert_eq!(partial["coverage"]["stale_files"], json!(["a.md"]));
            assert!(partial["hits"].as_array().unwrap().is_empty());
            let single = reader
                .search(
                    "needle",
                    &SearchOptions {
                        mode: mode.into(),
                        document: Some("a.md".into()),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(
                comparable(partial.clone()),
                comparable(serde_json::to_value(single)?)
            );
            assert_eq!(serde_json::to_value(&item.metrics)?, partial["metrics"]);
            assert_eq!(item.metrics.jev_calls_attempted, 0);
            assert_eq!(item.metrics.jev_requests, 0);
            assert!(item.encoded_bytes() <= item.reserved_output_bytes);
        }
        assert_eq!(response.items[2].status, BatchItemStatus::Ok);
        assert_eq!(
            response.items[2].result.as_ref().unwrap(),
            &reader.catalog()?
        );
        assert_eq!(response.coverage.failed, 2);
        assert_eq!(response.coverage.succeeded, 1);
        assert_eq!(response.metrics.model_submissions, 0);
        assert_eq!(response.execution.scope, "no_model_context");
        assert!(response.execution.before.is_none());
        assert!(response.execution.after.is_none());
        Ok(())
    }
}
