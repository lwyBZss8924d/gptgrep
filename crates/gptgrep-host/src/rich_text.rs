//! Explicit v2 text workflow. No ordinary ask, CLI or image capability activation.
pub use crate::protocol::RichTextCitation;
use crate::{
    HostConfig,
    jev_accounting::{Accounting, AttemptGuard},
    model_attempts::ModelAttemptTracker,
    protocol,
};
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::{
    ArtifactTextWindow, PreparedRichTextSearch, ReaderReadMetrics, RichGenerationReader,
    RichTextAttempt, RichTextInitialPass, RichTextSearchOptions, RichTextSearchReport,
    prepare_rich_text_search,
};
use gptgrep_jev::JevClient;
use gptgrep_parse::ArtifactKind;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, time::Duration};

const MAX_OUTPUT: usize = 1_048_576;
const MAX_METADATA: usize = 16384;
const MAX_TEXT: usize = 2048;
const MAX_CITATIONS: usize = 8;
const SEED_HITS: usize = 3;
const MAX_TOOL_OUTPUT: usize = 131072;
pub(crate) const NATIVE_METADATA_TOOL: &str = "gptgrep_inspect_native_metadata";
pub(crate) const DOCUMENT_DISCOVERY_GUIDANCE: &str = "For document arguments, use the admitted path in the tool schema enum or documents[].path from gptgrep_catalog, never documents[].id. Discover artifact_id and resource_id values from existing tool results and use them only in their matching parameters.";
const MAX_NATIVE_ARGUMENTS: usize = 20480;
const MAX_NATIVE_ITEM_OUTPUT: usize = 16384;
const MAX_NATIVE_BATCH_OUTPUT: usize = 32768;
const MAX_NATIVE_ENVELOPE: usize = MAX_NATIVE_BATCH_OUTPUT + 118;
// Compact inner JSON has no unescaped controls: quoting at most doubles its
// bytes. The remainder includes a maximally escaped 256-byte RPC ID, false
// success flag, fixed tool wrapper and newline.
pub(crate) const MAX_NATIVE_METADATA_FRAME: usize = 2 * MAX_NATIVE_ENVELOPE + 1621;
// The original core response keeps its own lease. This extra reservation covers
// envelope/frame/proof buffers and their allocation overlap, parsed arguments,
// small container/trace-label allocations, and fixed bookkeeping through close.
const NATIVE_TRANSPORT_RETAINED: u64 = (16 * MAX_NATIVE_METADATA_FRAME + 4096) as u64;

pub(crate) struct NativeMetadataReply {
    _response: gptgrep_core::rich_batch::RichBatchResponse,
    _transport: gptgrep_core::NativeMetadataLease,
    artifact_id: Option<String>,
    result: Value,
}
impl NativeMetadataReply {
    pub(crate) fn validate(&self, reader: &RichGenerationReader) -> Result<()> {
        if let Some(id) = &self.artifact_id {
            reader.recheck_page_native_metadata(id)?;
        }
        Ok(())
    }
    pub(crate) fn take_frame(&mut self, rpc_id: Value) -> Result<Value> {
        ensure!(!self.result.is_null(), "host_rich_native_reply_consumed");
        // Move the payload instead of serializing it into another owned Value.
        let mut frame = serde_json::Map::new();
        frame.insert("id".into(), rpc_id);
        frame.insert("result".into(), std::mem::take(&mut self.result));
        let frame = Value::Object(frame);
        measure(&frame, MAX_NATIVE_METADATA_FRAME - 1)?;
        Ok(frame)
    }
}

#[derive(Clone)]
pub struct RichTextHostOptions {
    pub retrieval: RichTextSearchOptions,
    pub max_output_bytes: usize,
    pub max_citations: usize,
}
impl Default for RichTextHostOptions {
    fn default() -> Self {
        Self {
            retrieval: RichTextSearchOptions::default(),
            max_output_bytes: MAX_OUTPUT,
            max_citations: MAX_CITATIONS,
        }
    }
}
/// Exact prepared source/bootstrap and finite original deadline; no DTO factory.
///
/// ```compile_fail
/// let _: gptgrep_host::PreparedRichTextHost = serde_json::from_str("{}").unwrap();
/// ```
pub struct PreparedRichTextHost {
    bootstrap: PreparedRichTextSearch,
    reader: RichGenerationReader,
    question: String,
    config: HostConfig,
    options: RichTextHostOptions,
    deadline: tokio::time::Instant,
    output_reserved: usize,
}
#[derive(Serialize)]
pub struct RichTextHostReport {
    pub schema_version: &'static str,
    pub answer: String,
    pub insufficient_evidence: bool,
    pub citations: Vec<RichTextCitation>,
    pub initial_retrieval: RichTextSearchReport,
    pub model_attempts: Vec<crate::ModelAttempt>,
    pub model_usage: crate::ModelUsage,
    pub tool_calls: usize,
    pub read_intervals: Vec<ReaderReadMetrics>,
    pub read_meter_scope: &'static str,
    pub retained_text_registry_bytes: usize,
    pub citation_observation: &'static str,
    pub qualification: &'static str,
    #[serde(skip)]
    _retained: Option<protocol::RichDeliveryHandle>,
}
#[derive(Serialize)]
pub struct RichTextHostError {
    pub code: &'static str,
    pub stage: &'static str,
    pub initial_retrieval: Option<Box<RichTextSearchReport>>,
    pub model_attempts: Vec<crate::ModelAttempt>,
    pub accounting_complete: bool,
    pub unknown_text_deliveries: usize,
    pub retained_text_registry_bytes: usize,
    #[serde(skip)]
    _retained: Option<protocol::RichDeliveryHandle>,
}
impl std::fmt::Display for RichTextHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} during {}", self.code, self.stage)
    }
}
impl std::fmt::Debug for RichTextHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for RichTextHostError {}

/// Validate complete host/bootstrap bounds before admitting the mandatory Jev pass.
/// Caller supplies an explicit client with its existing finite original context.
pub fn prepare_rich_text_host(
    reader: RichGenerationReader,
    client: JevClient,
    question: &str,
    config: &HostConfig,
    options: &RichTextHostOptions,
) -> Result<PreparedRichTextHost> {
    crate::validate_config(config, question)?;
    ensure!(
        config.query_plan.is_none()
            && config.navigation.is_none()
            && !config.source_continuation
            && config.jev_model.is_none(),
        "host_rich_legacy_configuration_conflict"
    );
    ensure!(
        (1..=MAX_OUTPUT).contains(&options.max_output_bytes)
            && (1..=MAX_CITATIONS).contains(&options.max_citations),
        "host_rich_output_options"
    );
    ensure!(
        config.document == options.retrieval.document,
        "host_rich_document_scope_conflict"
    );
    // Capture before sampling the existing clocks: time spent sampling/preparing
    // is subtracted rather than added to the caller's original lifetime.
    let captured = tokio::time::Instant::now();
    let lifetime = client.execution_context().original_run_remaining()?;
    let deadline = captured + lifetime.min(Duration::from_secs(config.timeout_secs));
    ensure!(
        tokio::time::Instant::now() < deadline,
        "host_deadline_exceeded"
    );
    // Every prepared bootstrap request is bound to this same stricter view
    // before preparation creates its client identity. The original context,
    // shared ledger/clock and accounting remain retained by the view.
    let execution = client
        .execution_context()
        .with_deadline(deadline.into_std())?;
    let client = client.with_execution_context(execution);
    reader.validate()?;
    // Bound every possible routing document's reference before paid bootstrap;
    // the host never discovers an oversized selected seed only after payment.
    let manifest = reader.manifest()?;
    let mut docs = Vec::with_capacity(9);
    for doc in manifest.documents.iter().filter(|d| {
        config
            .document
            .as_ref()
            .is_none_or(|p| p == &d.document.path)
    }) {
        let pos = docs
            .binary_search_by(|current: &&gptgrep_core::RichDocument| {
                current.document.path.cmp(&doc.document.path)
            })
            .unwrap_or_else(|p| p);
        if pos < 8 {
            docs.insert(pos, doc);
            docs.truncate(8);
        }
    }
    for doc in docs {
        for stored in doc.artifacts.iter().filter(|a| {
            matches!(
                a.artifact.kind,
                ArtifactKind::NativeText | ArtifactKind::OcrText
            )
        }) {
            ensure!(
                metadata_bound(&reader, &stored.artifact.id)? <= MAX_METADATA,
                "host_rich_seed_metadata_budget"
            );
            client.execution_context().check_deadline()?;
        }
    }
    let prompt_bound = 8192 + question.len() * 6 + SEED_HITS * (MAX_METADATA + MAX_TEXT * 6) + 4096;
    ensure!(
        prompt_bound <= config.max_input_bytes && prompt_bound * 6 + 8192 <= 4 * 1024 * 1024,
        "host_rich_input_budget"
    );
    let mut core_options = options.retrieval.clone();
    core_options.max_output_bytes = core_options.max_output_bytes.min(524288);
    let bootstrap =
        prepare_rich_text_search(reader.clone(), client.clone(), question, &core_options)?;
    let output_reserved = bootstrap
        .reserved_output_bytes()
        .checked_add(
            options.max_citations * (MAX_METADATA + MAX_TEXT * 6 + 1024)
                + 8192 * 6
                + 65536
                + config.max_tool_calls * 1024,
        )
        .ok_or_else(|| anyhow!("host_rich_output_budget"))?;
    ensure!(
        output_reserved <= options.max_output_bytes,
        "host_rich_output_budget"
    );
    client.execution_context().check_deadline()?;
    ensure!(
        tokio::time::Instant::now() < deadline,
        "host_deadline_exceeded"
    );
    Ok(PreparedRichTextHost {
        bootstrap,
        reader,
        question: question.into(),
        config: config.clone(),
        options: options.clone(),
        deadline,
        output_reserved,
    })
}
impl PreparedRichTextHost {
    pub fn reserved_output_bytes(&self) -> usize {
        self.output_reserved
    }
    /// Explicit activation; original ask/CLI/default dispatch is unchanged.
    /// Complete observed Jev usage is borrowed by this private sink before a later await.
    pub async fn execute(
        self,
        observer: &(dyn Fn(&RichTextAttempt) -> Result<()> + Send + Sync),
    ) -> std::result::Result<RichTextHostReport, RichTextHostError> {
        let initial = match self.bootstrap.execute_with_observer(observer).await {
            Ok(initial) => initial,
            Err(error) => {
                return Err(RichTextHostError {
                    code: error.code,
                    stage: error.stage,
                    initial_retrieval: Some(Box::new(error.report)),
                    model_attempts: vec![],
                    accounting_complete: false,
                    unknown_text_deliveries: 0,
                    retained_text_registry_bytes: 0,
                    _retained: None,
                });
            }
        };
        let mut evidence = match RichEvidence::from_initial(
            initial,
            self.options.max_citations,
            self.deadline,
            self.options.retrieval.document.clone(),
        ) {
            Ok(evidence) => evidence,
            Err(error) => return Err(error),
        };
        let manifest = match self.reader.manifest() {
            Ok(manifest) => manifest,
            Err(_) => {
                return Err(evidence.failure(
                    "host_rich_source_unavailable",
                    "reader_start",
                    vec![],
                ));
            }
        };
        let accounting = match Accounting::create(&manifest.root, &self.reader.pin().generation) {
            Ok(accounting) => accounting,
            Err(_) => {
                return Err(evidence.failure(
                    "host_rich_accounting_unavailable",
                    "reader_start",
                    vec![],
                ));
            }
        };
        let _guard = AttemptGuard(accounting.clone());
        if accounting
            .bind_workflow(&self.question, self.options.retrieval.document.as_deref())
            .is_err()
        {
            return Err(evidence.failure(
                "host_rich_accounting_unavailable",
                "reader_start",
                accounting.model_attempts(),
            ));
        }
        let mut tracker =
            match ModelAttemptTracker::reserve(&accounting, "final_reader", &self.config) {
                Ok(tracker) => tracker,
                Err(_) => {
                    return Err(evidence.failure(
                        "host_rich_model_attempt_unavailable",
                        "reader_start",
                        accounting.model_attempts(),
                    ));
                }
            };
        let result = crate::run_process_until(
            &self.config,
            protocol::Workflow::RichText {
                question: &self.question,
                evidence: &mut evidence,
            },
            Some(self.deadline),
            protocol::RunOptions {
                observer: Some(tracker.observer()),
                max_output_bytes: Some(65536),
                ..Default::default()
            },
        )
        .await;
        let observed_error = result.as_ref().err();
        if tracker.finish(observed_error).is_err() {
            return Err(evidence.failure(
                "host_rich_accounting_unavailable",
                "reader_finish",
                accounting.model_attempts(),
            ));
        }
        let outcome = match result {
            Ok(outcome) => outcome.result,
            Err(_) => {
                return Err(evidence.failure(
                    "host_rich_reader_failed",
                    "reader",
                    accounting.model_attempts(),
                ));
            }
        };
        let final_value = match evidence.finish(&outcome.answer) {
            Ok(value) => value,
            Err(_) => {
                return Err(evidence.failure(
                    "host_rich_final_evidence_rejected",
                    "reader_finish",
                    accounting.model_attempts(),
                ));
            }
        };
        let attempts = accounting.model_attempts();
        let usage = crate::model_attempts::summarize(&attempts);
        evidence.into_report(
            final_value,
            attempts,
            usage,
            self.output_reserved,
            Some(&accounting),
        )
    }
}

pub(crate) struct RichEvidence {
    pub(crate) initial: Option<RichTextInitialPass>,
    pub(crate) multimodal: Option<gptgrep_core::RichMultimodalInitialPass>,
    pub(crate) visual_resources: Option<gptgrep_parse::ResourceBudget>,
    pub(crate) reader: RichGenerationReader,
    pub(crate) client: JevClient,
    pub(crate) delivery: Option<protocol::RichDeliveryHandle>,
    pub(crate) max_citations: usize,
    pub(crate) deadline: tokio::time::Instant,
    pub(crate) before: ReaderReadMetrics,
    pub(crate) calls: usize,
    pub(crate) document: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Final {
    answer: String,
    citations: Vec<protocol::RichCitationRequest>,
    insufficient_evidence: bool,
}
pub(crate) struct ValidatedFinal {
    answer: String,
    citations: Vec<RichTextCitation>,
    insufficient_evidence: bool,
    requests: Vec<protocol::RichCitationRequest>,
}
impl RichEvidence {
    pub(crate) fn from_initial(
        initial: RichTextInitialPass,
        max_citations: usize,
        deadline: tokio::time::Instant,
        document: Option<String>,
    ) -> std::result::Result<Self, RichTextHostError> {
        if initial.validate().is_err() || tokio::time::Instant::now() >= deadline {
            return Err(RichTextHostError {
                code: "host_rich_initial_stale_or_expired",
                stage: "reader_start",
                initial_retrieval: Some(Box::new(initial.into_report())),
                model_attempts: vec![],
                accounting_complete: false,
                unknown_text_deliveries: 0,
                retained_text_registry_bytes: 0,
                _retained: None,
            });
        }
        let reader = initial.reader().clone();
        let client = initial.client().clone();
        let before = match reader.read_metrics() {
            Ok(before) => before,
            Err(_) => {
                return Err(RichTextHostError {
                    code: "host_rich_meter_unavailable",
                    stage: "reader_start",
                    initial_retrieval: Some(Box::new(initial.into_report())),
                    model_attempts: vec![],
                    accounting_complete: false,
                    unknown_text_deliveries: 0,
                    retained_text_registry_bytes: 0,
                    _retained: None,
                });
            }
        };
        Ok(Self {
            initial: Some(initial),
            multimodal: None,
            visual_resources: None,
            reader,
            client,
            delivery: None,
            max_citations,
            deadline,
            before,
            calls: 0,
            document,
        })
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.client.execution_context().check_deadline()?;
        ensure!(
            tokio::time::Instant::now() < self.deadline,
            "host_deadline_exceeded"
        );
        if let Some(initial) = &self.initial {
            initial.validate()
        } else {
            self.multimodal
                .as_ref()
                .expect("one genuine initial pass")
                .validate()
        }
    }
    pub(crate) fn state(&self, question: &str) -> Result<Value> {
        self.validate()?;
        if let Some(initial) = &self.multimodal {
            let seed: Vec<_> = initial.report().hits.iter().take(SEED_HITS).collect();
            let value = json!({"question":question,"generation":self.reader.pin(),"initial_retrieval":seed,
                "coverage":initial.report().coverage,"citation_status":"model_exposure_only_explicit_delivery_required"});
            measure(&value, 196608)?;
            self.validate()?;
            return Ok(value);
        }
        let initial = self
            .initial
            .as_ref()
            .expect("one genuine initial pass")
            .report();
        let seed: Vec<_> = initial
            .hits
            .iter()
            .take(SEED_HITS)
            .map(|hit| json!({"candidate_id":hit.candidate_id,"window":hit.window}))
            .collect();
        let value = json!({"question":question,"generation":self.reader.pin(),"initial_retrieval":seed,
            "coverage":initial.coverage,"citation_status":"model_exposure_only_read_artifact_required"});
        measure(&value, 196608)?;
        self.validate()?;
        Ok(value)
    }
    pub(crate) fn max_citations(&self) -> usize {
        self.max_citations
    }
    pub(crate) fn tool_called(&mut self) {
        self.calls += 1;
    }
    pub(crate) fn attach(&mut self, handle: protocol::RichDeliveryHandle) -> Result<()> {
        ensure!(
            self.delivery.is_none(),
            "host_rich_delivery_already_attached"
        );
        self.delivery = Some(handle);
        Ok(())
    }
    pub(crate) fn read(&self, args: &Value) -> Result<ArtifactTextWindow> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            artifact_id: String,
            #[serde(default)]
            offset: usize,
            #[serde(default = "read_default")]
            max_bytes: usize,
        }
        let request: Read = serde_json::from_value(args.clone())
            .map_err(|_| anyhow!("host_rich_invalid_arguments"))?;
        ensure!(
            !request.artifact_id.is_empty()
                && request.artifact_id.len() <= 256
                && (1..=MAX_TEXT).contains(&request.max_bytes),
            "host_rich_invalid_arguments"
        );
        self.ensure_artifact(&request.artifact_id)?;
        ensure!(
            metadata_bound(&self.reader, &request.artifact_id)? <= MAX_METADATA,
            "host_rich_metadata_budget"
        );
        self.validate()?;
        let window =
            self.reader
                .read_text(&request.artifact_id, request.offset, request.max_bytes)?;
        self.validate()?;
        Ok(window)
    }
    fn ensure_artifact(&self, id: &str) -> Result<()> {
        let manifest = self.reader.manifest()?;
        ensure!(
            manifest.documents.iter().any(|d| self
                .document
                .as_ref()
                .is_none_or(|scope| scope == &d.document.path)
                && d.artifacts.iter().any(|a| a.artifact.id == id
                    && matches!(
                        a.artifact.kind,
                        ArtifactKind::NativeText | ArtifactKind::OcrText
                    ))),
            "host_rich_artifact_unavailable"
        );
        Ok(())
    }
    fn ensure_native_metadata_artifact(&self, id: &str) -> Result<()> {
        let manifest = self.reader.manifest()?;
        ensure!(
            manifest.documents.iter().any(|document| {
                self.document
                    .as_ref()
                    .is_none_or(|scope| scope == &document.document.path)
                    && document.artifacts.iter().any(|stored| {
                        stored.artifact.id == id
                            && stored.artifact.kind == ArtifactKind::NativeMetadata
                            && stored.artifact.page.is_some_and(|number| {
                                document.pages.iter().any(|page| {
                                    page.number == number && page.metadata_artifact.id == id
                                })
                            })
                    })
            }),
            "host_rich_native_metadata_unavailable"
        );
        Ok(())
    }
    pub(crate) fn native_metadata_tool(&self, args: &Value) -> Result<NativeMetadataReply> {
        use gptgrep_core::rich_batch::{
            RichBatchBudget, RichBatchGeneration, RichBatchOperation, RichBatchRequest,
            RichBatchStatus,
        };
        measure(args, MAX_NATIVE_ARGUMENTS)?;
        // Reserve before retained argument, metadata or serialization copies. The
        // core response's projection lease remains separate and also stays live.
        let transport = self
            .reader
            .reserve_native_metadata_transport(NATIVE_TRANSPORT_RETAINED)?;
        let request = NativeMetadataArgs::parse(args)?;
        self.ensure_native_metadata_artifact(&request.artifact_id)?;
        self.validate()?;
        let artifact_id = request.artifact_id.clone();
        let response = gptgrep_core::rich_batch::dispatch_rich_batch_with_reader(
            &self.reader,
            RichBatchRequest {
                schema_version: "gptgrep.batch.rich.request.v1".into(),
                generation: RichBatchGeneration::Exact(self.reader.pin()),
                budget: RichBatchBudget {
                    max_operations: 1,
                    max_output_bytes: MAX_NATIVE_BATCH_OUTPUT,
                    ..Default::default()
                },
                operations: vec![RichBatchOperation::InspectNativeMetadata {
                    id: "metadata".into(),
                    artifact_id: request.artifact_id,
                    fields: request.fields,
                    collection: request.collection,
                    item_fields: request.item_fields,
                    object_id: request.object_id,
                    page_size: request.page_size,
                    max_output_bytes: request.max_output_bytes,
                    cursor: request.cursor,
                }],
            },
        )
        .map_err(|_| anyhow!("host_rich_native_metadata_rejected"))?;
        self.validate()?;
        measure(&response, MAX_NATIVE_BATCH_OUTPUT)?;
        let success = response
            .items
            .first()
            .is_some_and(|item| item.status == RichBatchStatus::Ok);
        #[derive(Serialize)]
        struct Envelope<'a> {
            schema_version: &'static str,
            result: &'a gptgrep_core::rich_batch::RichBatchResponse,
            citation_status: &'static str,
        }
        let text = bounded_json_string(
            &Envelope {
                schema_version: "gptgrep.rich_text.metadata.v1",
                result: &response,
                citation_status: "not_registered_read_artifact_required",
            },
            MAX_NATIVE_ENVELOPE,
        )?;
        let mut content = serde_json::Map::new();
        content.insert("type".into(), Value::String("inputText".into()));
        content.insert("text".into(), Value::String(text));
        let mut result = serde_json::Map::new();
        result.insert("success".into(), Value::Bool(success));
        result.insert(
            "contentItems".into(),
            Value::Array(vec![Value::Object(content)]),
        );
        let result = Value::Object(result);
        measure(&result, MAX_TOOL_OUTPUT)?;
        Ok(NativeMetadataReply {
            _response: response,
            _transport: transport,
            artifact_id: success.then_some(artifact_id),
            result,
        })
    }
    /// Preserve explicit resource scope even when another argument is invalid.
    /// This inspects manifest identities only; it cannot read or grant evidence.
    pub(crate) fn validate_argument_scope(&self, tool: &str, args: &Value) -> Result<()> {
        if let Some(scope) = &self.document {
            match tool {
                "gptgrep_inspect_artifact" | "gptgrep_search_text" => {
                    if let Some(document) = args["document"].as_str().filter(|s| !s.is_empty()) {
                        ensure!(document == scope, "host_rich_document_scope");
                    }
                }
                "gptgrep_catalog" => {
                    ensure!(args["cursor"].is_null(), "host_rich_scoped_catalog_cursor");
                }
                _ => {}
            }
        }
        if let Some(id) = args["artifact_id"].as_str().filter(|s| !s.is_empty()) {
            match tool {
                "gptgrep_read_artifact" => self.ensure_artifact(id)?,
                NATIVE_METADATA_TOOL => self.ensure_native_metadata_artifact(id)?,
                _ => {}
            }
        }
        if tool == "gptgrep_render"
            && self.multimodal.is_some()
            && let Some(id) = args["resource_id"].as_str().filter(|s| !s.is_empty())
        {
            let manifest = self.reader.manifest()?;
            ensure!(
                manifest.documents.iter().any(|document| {
                    self.document
                        .as_ref()
                        .is_none_or(|scope| scope == &document.document.path)
                        && document
                            .pages
                            .iter()
                            .any(|page| page.renditions.iter().any(|image| image.artifact.id == id))
                }),
                "host_rich_visual_resource_unavailable"
            );
        }
        Ok(())
    }
    pub(crate) fn metadata_tool(&self, tool: &str, args: &Value) -> Result<Value> {
        validate_arguments(tool, args)?;
        self.validate()?;
        use gptgrep_core::rich_batch::{
            RichBatchBudget, RichBatchGeneration, RichBatchOperation, RichBatchRequest,
            RichBatchStatus,
        };
        let operation = match tool {
            "gptgrep_catalog" => {
                let request: PageArgs = serde_json::from_value(args.clone())?;
                if let Some(scope) = &self.document {
                    ensure!(request.cursor.is_none(), "host_rich_scoped_catalog_cursor");
                    let manifest = self.reader.manifest()?;
                    let document = manifest
                        .documents
                        .iter()
                        .find(|d| &d.document.path == scope)
                        .ok_or_else(|| anyhow!("host_rich_document_unavailable"))?;
                    // Borrow/measure the projection before copying any source strings.
                    #[derive(Serialize)]
                    struct Projection<'a> {
                        id: &'a str,
                        path: &'a str,
                        title: &'a str,
                        source_sha256: &'a str,
                        pages: usize,
                    }
                    let projection = Projection {
                        id: &document.document.id,
                        path: &document.document.path,
                        title: &document.document.title,
                        source_sha256: &document.document.source_sha256,
                        pages: document.pages.len(),
                    };
                    measure(&projection, 32768)?;
                    let value = metadata_response(
                        true,
                        &json!({"documents":[serde_json::to_value(projection)?],"cursor":null}),
                    )?;
                    self.validate()?;
                    return Ok(value);
                }
                RichBatchOperation::InspectDocuments {
                    id: "metadata".into(),
                    fields: vec!["title".into(), "pages".into()],
                    page_size: request.page_size,
                    cursor: request.cursor,
                }
            }
            "gptgrep_inspect_artifact" => {
                let request: InspectArgs = serde_json::from_value(args.clone())?;
                ensure!(
                    self.document
                        .as_ref()
                        .is_none_or(|scope| scope == &request.document),
                    "host_rich_document_scope"
                );
                RichBatchOperation::InspectPages {
                    id: "metadata".into(),
                    document: request.document,
                    fields: if self.multimodal.is_some() {
                        [
                            "coverage",
                            "native_text",
                            "ocr_text",
                            "metadata_artifact",
                            "renditions",
                        ]
                        .map(str::to_owned)
                        .to_vec()
                    } else {
                        ["coverage", "native_text", "ocr_text", "metadata_artifact"]
                            .map(str::to_owned)
                            .to_vec()
                    },
                    page_size: request.page_size,
                    cursor: request.cursor,
                }
            }
            "gptgrep_search_text" => {
                let request: SearchArgs = serde_json::from_value(args.clone())?;
                ensure!(
                    request
                        .document
                        .as_ref()
                        .is_none_or(|doc| self.document.as_ref().is_none_or(|scope| scope == doc)),
                    "host_rich_document_scope"
                );
                RichBatchOperation::SearchArtifacts {
                    id: "metadata".into(),
                    query: request.query,
                    case_insensitive: request.case_insensitive,
                    literal: request.literal,
                    document: self.document.clone().or(request.document),
                    max_hits: 3,
                    page_size: 3,
                    cursor: request.cursor,
                }
            }
            "gptgrep_render" => return Ok(tool_error("capability_unavailable")),
            _ => return Err(anyhow!("host_rich_tool_allowlist")),
        };
        let request = RichBatchRequest {
            schema_version: "gptgrep.batch.rich.request.v1".into(),
            generation: RichBatchGeneration::Exact(self.reader.pin()),
            budget: RichBatchBudget {
                max_operations: 1,
                max_output_bytes: 65536,
                ..Default::default()
            },
            operations: vec![operation],
        };
        let response =
            gptgrep_core::rich_batch::dispatch_rich_batch_with_reader(&self.reader, request);
        self.validate()?;
        let response = match response {
            Ok(response) => response,
            Err(_) => return Ok(tool_error("resource_or_source_rejected")),
        };
        metadata_response(
            response
                .items
                .first()
                .is_some_and(|item| item.status == RichBatchStatus::Ok),
            &response,
        )
    }
    pub(crate) fn finish(&self, raw: &str) -> Result<ValidatedFinal> {
        self.validate()?;
        ensure!(raw.len() <= 65536, "host_rich_final_budget");
        let value: Final =
            serde_json::from_str(raw).map_err(|_| anyhow!("host_rich_final_schema"))?;
        ensure!(
            value.answer.len() <= 8192 && value.citations.len() <= self.max_citations,
            "host_rich_final_budget"
        );
        ensure!(
            value.insufficient_evidence || !value.citations.is_empty(),
            "host_rich_substantive_answer_uncited"
        );
        let handle = self
            .delivery
            .as_ref()
            .ok_or_else(|| anyhow!("host_rich_delivery_unattached"))?;
        let citations = handle.resolve(&value.citations, &self.reader, self.max_citations)?;
        self.validate()?;
        Ok(ValidatedFinal {
            answer: value.answer,
            citations,
            insufficient_evidence: value.insufficient_evidence,
            requests: value.citations,
        })
    }
    pub(crate) fn failure(
        mut self,
        code: &'static str,
        stage: &'static str,
        model_attempts: Vec<crate::ModelAttempt>,
    ) -> RichTextHostError {
        let unknown = self.delivery.as_ref().map_or(0, |h| h.unknown_entries());
        RichTextHostError {
            code,
            stage,
            initial_retrieval: self
                .initial
                .take()
                .map(|initial| Box::new(initial.into_report())),
            model_attempts,
            accounting_complete: false,
            unknown_text_deliveries: unknown,
            retained_text_registry_bytes: self.delivery.as_ref().map_or(0, |h| h.retained_bytes()),
            _retained: self.delivery.take(),
        }
    }
    pub(crate) fn into_report(
        mut self,
        mut final_value: ValidatedFinal,
        model_attempts: Vec<crate::ModelAttempt>,
        model_usage: crate::ModelUsage,
        limit: usize,
        accounting: Option<&Accounting>,
    ) -> std::result::Result<RichTextHostReport, RichTextHostError> {
        // Borrow the complete eventual public envelope while the genuine handle
        // still exists. No large clone or consumption before the final CPU/source gap.
        #[derive(Serialize)]
        struct Preview<'a> {
            schema_version: &'static str,
            answer: &'a str,
            insufficient_evidence: bool,
            citations: &'a [RichTextCitation],
            initial_retrieval: &'a RichTextSearchReport,
            model_attempts: &'a [crate::ModelAttempt],
            model_usage: &'a crate::ModelUsage,
            retained_text_registry_bytes: usize,
            tool_calls: usize,
            read_intervals: Vec<ReaderReadMetrics>,
            read_meter_scope: &'static str,
            citation_observation: &'static str,
            qualification: &'static str,
        }
        let checked = (|| -> Result<_> {
            let interval = meter_delta(&self.reader.read_metrics()?, &self.before)?;
            let preview = Preview {
                schema_version: "gptgrep.rich_text.host.v1",
                answer: &final_value.answer,
                insufficient_evidence: final_value.insufficient_evidence,
                citations: &final_value.citations,
                initial_retrieval: self.initial.as_ref().expect("one genuine initial").report(),
                model_attempts: &model_attempts,
                model_usage: &model_usage,
                retained_text_registry_bytes: self
                    .delivery
                    .as_ref()
                    .map_or(0, |h| h.retained_bytes()),
                tool_calls: self.calls,
                read_intervals: vec![interval],
                read_meter_scope: "shared_retained_reader_interval_not_exclusive",
                citation_observation: "host_complete_frame_write_flush_and_fresh_typed_text",
                qualification: "source_foundation_only_native_backend_unqualified",
            };
            measure(&preview, limit)?;
            self.validate()?;
            if let Some(accounting) = accounting {
                accounting.finish("completed")?;
            }
            // Source and original clocks are checked after both final CPU work
            // and the existing private model-attempt closeout write.
            self.validate()?;
            let citations = self
                .delivery
                .as_ref()
                .ok_or_else(|| anyhow!("host_rich_delivery_unattached"))?
                .resolve(&final_value.requests, &self.reader, self.max_citations)?;
            self.validate()?;
            let interval = meter_delta(&self.reader.read_metrics()?, &self.before)?;
            Ok((citations, interval))
        })();
        let (citations, interval) = match checked {
            Ok(value) => value,
            Err(_) => {
                return Err(self.failure(
                    "host_rich_completion_rejected",
                    "reader_finish",
                    model_attempts,
                ));
            }
        };
        final_value.citations = citations;
        Ok(RichTextHostReport {
            schema_version: "gptgrep.rich_text.host.v1",
            answer: final_value.answer,
            insufficient_evidence: final_value.insufficient_evidence,
            citations: final_value.citations,
            initial_retrieval: self
                .initial
                .take()
                .expect("one genuine initial")
                .into_report(),
            model_attempts,
            model_usage,
            retained_text_registry_bytes: self.delivery.as_ref().map_or(0, |h| h.retained_bytes()),
            tool_calls: self.calls,
            read_intervals: vec![interval],
            read_meter_scope: "shared_retained_reader_interval_not_exclusive",
            citation_observation: "host_complete_frame_write_flush_and_fresh_typed_text",
            qualification: "source_foundation_only_native_backend_unqualified",
            _retained: self.delivery.take(),
        })
    }
}
fn read_default() -> usize {
    1024
}
fn page_default() -> usize {
    8
}
fn true_default() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageArgs {
    #[serde(default = "page_default")]
    page_size: usize,
    #[serde(default)]
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectArgs {
    document: String,
    #[serde(default = "page_default")]
    page_size: usize,
    #[serde(default)]
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    document: Option<String>,
    #[serde(default = "true_default")]
    literal: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    cursor: Option<String>,
}
fn native_page_default() -> usize {
    100
}
fn native_output_default() -> usize {
    MAX_NATIVE_ITEM_OUTPUT
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeMetadataArgs {
    artifact_id: String,
    #[serde(default)]
    fields: Vec<String>,
    collection: gptgrep_core::NativeMetadataCollection,
    #[serde(default)]
    item_fields: Vec<String>,
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default = "native_page_default")]
    page_size: usize,
    #[serde(default = "native_output_default")]
    max_output_bytes: usize,
    #[serde(default)]
    cursor: Option<String>,
}
impl NativeMetadataArgs {
    fn parse(args: &Value) -> Result<Self> {
        measure(args, MAX_NATIVE_ARGUMENTS)?;
        let request = Self::deserialize(args)?;
        ensure!(
            !request.artifact_id.is_empty()
                && request.artifact_id.len() <= 512
                && (1..=1000).contains(&request.page_size)
                && (1..=MAX_NATIVE_ITEM_OUTPUT).contains(&request.max_output_bytes)
                && request.cursor.as_ref().is_none_or(|cursor| {
                    cursor.len() <= 16384
                        && cursor.len() % 2 == 0
                        && cursor
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }),
            "host_rich_native_metadata_arguments"
        );
        gptgrep_core::NativeMetadataSelection {
            fields: request.fields.clone(),
            collection: request.collection,
            item_fields: request.item_fields.clone(),
            object_id: request.object_id.clone(),
        }
        .validate()?;
        Ok(request)
    }
}
pub(crate) fn argument_limit(tool: &str) -> usize {
    if tool == NATIVE_METADATA_TOOL {
        MAX_NATIVE_ARGUMENTS
    } else {
        4096
    }
}
pub(crate) fn validate_arguments(tool: &str, args: &Value) -> Result<()> {
    fn cursor(value: &Option<String>) -> Result<()> {
        ensure!(
            value.as_ref().is_none_or(|c| c.len() <= 16384),
            "host_rich_cursor_limit"
        );
        Ok(())
    }
    match tool {
        NATIVE_METADATA_TOOL => {
            NativeMetadataArgs::parse(args)?;
        }
        "gptgrep_catalog" => {
            let r: PageArgs = serde_json::from_value(args.clone())?;
            ensure!((1..=8).contains(&r.page_size), "host_rich_page_limit");
            cursor(&r.cursor)?;
        }
        "gptgrep_inspect_artifact" => {
            let r: InspectArgs = serde_json::from_value(args.clone())?;
            ensure!(
                !r.document.is_empty()
                    && r.document.len() <= 4096
                    && (1..=8).contains(&r.page_size),
                "host_rich_page_limit"
            );
            cursor(&r.cursor)?;
        }
        "gptgrep_search_text" => {
            let r: SearchArgs = serde_json::from_value(args.clone())?;
            ensure!(
                !r.query.is_empty()
                    && r.query.len() <= 8192
                    && r.document
                        .as_ref()
                        .is_none_or(|d| !d.is_empty() && d.len() <= 4096),
                "host_rich_search_arguments"
            );
            cursor(&r.cursor)?;
        }
        "gptgrep_read_artifact" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Read {
                artifact_id: String,
                #[serde(default)]
                offset: usize,
                #[serde(default = "read_default")]
                max_bytes: usize,
            }
            let r: Read = serde_json::from_value(args.clone())?;
            ensure!(
                !r.artifact_id.is_empty()
                    && r.artifact_id.len() <= 256
                    && (1..=MAX_TEXT).contains(&r.max_bytes),
                "host_rich_read_arguments"
            );
            let _ = r.offset;
        }
        "gptgrep_render" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Render {
                resource_id: String,
            }
            let r: Render = serde_json::from_value(args.clone())?;
            ensure!(
                !r.resource_id.is_empty() && r.resource_id.len() <= 256,
                "host_rich_render_arguments"
            );
        }
        _ => return Err(anyhow!("host_rich_tool_allowlist")),
    }
    Ok(())
}
fn metadata_response(success: bool, result: &impl Serialize) -> Result<Value> {
    measure(result, 65536)?;
    let content = json!({"schema_version":"gptgrep.rich_text.metadata.v1","result":serde_json::to_value(result)?,"citation_status":"not_registered_read_artifact_required"});
    let value = json!({"success":success,"contentItems":[{"type":"inputText","text":serde_json::to_string(&content)?}]});
    measure(&value, MAX_TOOL_OUTPUT)?;
    Ok(value)
}

fn bounded_json_string(value: &impl Serialize, limit: usize) -> Result<String> {
    struct Buffer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes
                .len()
                .checked_add(bytes.len())
                .filter(|end| *end <= self.limit)
                .ok_or_else(|| std::io::Error::other("host_rich_output_budget"))?;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let length = measure(value, limit)?;
    let mut buffer = Buffer {
        bytes: Vec::new(),
        limit: length,
    };
    buffer.bytes.try_reserve_exact(length)?;
    serde_json::to_writer(&mut buffer, value)?;
    Ok(String::from_utf8(buffer.bytes)?)
}

pub(crate) fn argument_error(tool: &str) -> Value {
    let message = match tool {
        "gptgrep_inspect_artifact" => {
            "Use a nonempty document path and page_size from 1 through 8, as declared in the tool schema."
        }
        "gptgrep_catalog" => "Use page_size from 1 through 8, as declared in the tool schema.",
        _ => "Use arguments matching the declared tool schema and its bounds.",
    };
    json!({"success":false,"contentItems":[{"type":"inputText","text":json!({"error":{"code":"invalid_arguments","message":message}}).to_string()}]})
}
pub(crate) fn tool_error(code: &str) -> Value {
    json!({"success":false,"contentItems":[{"type":"inputText","text":json!({"error":{"code":code}}).to_string()}]})
}
pub(crate) fn allowed(tool: &str) -> bool {
    matches!(
        tool,
        "gptgrep_catalog"
            | "gptgrep_inspect_artifact"
            | NATIVE_METADATA_TOOL
            | "gptgrep_search_text"
            | "gptgrep_read_artifact"
            | "gptgrep_render"
    )
}
pub(crate) fn tools(document: Option<&str>) -> Value {
    let mut document_schema = json!({"type":"string","description":"Exact admitted document path (gptgrep_catalog documents[].path), not documents[].id or an artifact/resource ID."});
    if let Some(document) = document {
        document_schema["enum"] = json!([document]);
    }
    let native_item_fields: std::collections::BTreeSet<_> = [
        gptgrep_core::NATIVE_OBJECT_FIELDS,
        gptgrep_core::NATIVE_SEGMENT_FIELDS,
        gptgrep_core::NATIVE_IMAGE_FIELDS,
        gptgrep_core::NATIVE_CHARACTER_FIELDS,
        gptgrep_core::NATIVE_WORD_FIELDS,
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect();
    let fields = [
        (
            "gptgrep_catalog",
            json!({"page_size":{"type":"integer","minimum":1,"maximum":8},"cursor":{"type":"string"}}),
            vec![],
        ),
        (
            "gptgrep_inspect_artifact",
            json!({"document":document_schema,"page_size":{"type":"integer","minimum":1,"maximum":8},"cursor":{"type":"string"}}),
            vec!["document"],
        ),
        (
            NATIVE_METADATA_TOOL,
            json!({
                "artifact_id":{"type":"string","minLength":1,"maxLength":512,"description":"Page metadata_artifact.id from inspect_artifact; scoped source identity, never a path or citation ID"},
                "fields":{"type":"array","items":{"type":"string","enum":gptgrep_core::NATIVE_METADATA_SCALAR_FIELDS},"uniqueItems":true,"maxItems":4,"default":[]},
                "collection":{"type":"string","enum":["object_summaries","path_segments","image_geometry","native_characters","native_words"]},
                "item_fields":{"type":"array","items":{"type":"string","enum":native_item_fields},"uniqueItems":true,"maxItems":14,"default":[],"description":"Closed fields for the selected collection; empty selects all row fields"},
                "object_id":{"type":["string","null"],"pattern":"^[0-9a-f]{64}$","description":"Required only for path_segments: a path ID returned by object_summaries"},
                "page_size":{"type":"integer","minimum":1,"maximum":1000,"default":100},
                "max_output_bytes":{"type":"integer","minimum":1,"maximum":16384,"default":16384,"description":"Complete core item quota, including rows, scalars and cursor; enclosing batch is fixed at32768 bytes"},
                "cursor":{"type":["string","null"],"maxLength":16384,"pattern":"^([0-9a-f]{2})*$","description":"Return coverage.cursor unchanged with identical fields, collection, object, page_size and max_output_bytes"}
            }),
            vec!["artifact_id", "collection"],
        ),
        (
            "gptgrep_search_text",
            json!({"query":{"type":"string"},"document":document_schema,"literal":{"type":"boolean"},"case_insensitive":{"type":"boolean"},"cursor":{"type":"string"}}),
            vec!["query"],
        ),
        (
            "gptgrep_read_artifact",
            json!({"artifact_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":1,"maximum":2048}}),
            vec!["artifact_id"],
        ),
        (
            "gptgrep_render",
            json!({"resource_id":{"type":"string"}}),
            vec!["resource_id"],
        ),
    ];
    json!([{"type":"namespace","name":"gptgrep","description":"Source-bound v2 native/OCR text and non-citable native metadata; images unavailable",
        "tools":fields.into_iter().map(|(name,properties,required)|json!({"type":"function","name":name,"description":if name == NATIVE_METADATA_TOOL { "Inspect freshly verified page metadata. Discover metadata_artifact.id with inspect_artifact. Geometry and character/path/image placement are non-citable metadata, not visible pixels or citation bounds. Closed fields depend on collection; path_segments requires object_id. Whole rows and cursor must fit the selected quota." } else { match name {
                "gptgrep_catalog" => "List admitted documents. Use documents[].path for document arguments; documents[].id is not a document path.",
                "gptgrep_inspect_artifact" => "Inspect a document path and discover source artifact and page metadata IDs for subsequent tools.",
                "gptgrep_search_text" => "Search saved native/OCR text. The optional document argument is a document path; omission retains the admitted scope.",
                _ => name,
            } },
            "inputSchema":{"type":"object","additionalProperties":false,"properties":properties,"required":required}})).collect::<Vec<_>>()}])
}
pub(crate) fn final_schema(maximum: usize) -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
    "answer":{"type":"string","maxLength":8192},"insufficient_evidence":{"type":"boolean"},
    "citations":{"type":"array","maxItems":maximum,"items":{"type":"object","additionalProperties":false,
        "properties":{"issued_id":{"type":"string","maxLength":64},"offset":{"type":"integer","minimum":0},"length":{"type":"integer","minimum":1,"maximum":2048},"quote":{"type":"string","maxLength":2048}},
        "required":["issued_id","offset","length","quote"]}}},"required":["answer","citations","insufficient_evidence"]})
}

pub(crate) fn metadata_bound(reader: &RichGenerationReader, id: &str) -> Result<usize> {
    let manifest = reader.manifest()?;
    let (doc, stored) = manifest
        .documents
        .iter()
        .find_map(|d| {
            d.artifacts
                .iter()
                .find(|a| a.artifact.id == id)
                .map(|a| (d, a))
        })
        .ok_or_else(|| anyhow!("host_rich_artifact_unavailable"))?;
    let mut bytes = 4096usize;
    let pin = reader.pin();
    for value in [
        &pin.generation,
        &pin.manifest_sha256,
        &doc.document.id,
        &doc.document.path,
        &doc.document.source_sha256,
        &stored.artifact.id,
        &stored.artifact.sha256,
    ] {
        bytes = bytes
            .checked_add(value.len() * 6)
            .ok_or_else(|| anyhow!("host_rich_metadata_budget"))?;
    }
    if stored.artifact.kind == ArtifactKind::OcrText {
        let outcome = doc
            .ocr_results
            .iter()
            .find(|o| Some(o.page) == stored.artifact.page)
            .ok_or_else(|| anyhow!("host_rich_ocr_receipt"))?;
        bytes = bytes
            .checked_add(outcome.receipt.profile_sha256.len() * 6)
            .ok_or_else(|| anyhow!("host_rich_metadata_budget"))?;
        for region in &outcome.regions {
            bytes = bytes
                .checked_add(region.id.len() * 6 + 3)
                .ok_or_else(|| anyhow!("host_rich_metadata_budget"))?;
        }
    }
    Ok(bytes)
}
pub(crate) fn measure(value: &impl Serialize, limit: usize) -> Result<usize> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.used = self
                .used
                .checked_add(b.len())
                .filter(|n| *n <= self.limit)
                .ok_or_else(|| std::io::Error::other("host_rich_output_budget"))?;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Counter { used: 0, limit };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.used)
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
pub(crate) mod tests;

pub(crate) fn meter_delta(
    after: &ReaderReadMetrics,
    before: &ReaderReadMetrics,
) -> Result<ReaderReadMetrics> {
    ensure!(
        after.coverage == before.coverage,
        "host_rich_meter_scope_changed"
    );
    macro_rules! delta {
        ($field:ident) => {
            after
                .$field
                .checked_sub(before.$field)
                .ok_or_else(|| anyhow!("host_rich_meter_decreased"))?
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
