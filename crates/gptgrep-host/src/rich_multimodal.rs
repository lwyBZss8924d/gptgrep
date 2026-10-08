//! Explicit source-bound multimodal reader over actual dynamic-tool RPC.
//!
//! `prepare_rich_multimodal_host` requires the caller's pinned rich reader,
//! normal Decisions client and finite original execution context. Its consumed
//! core initial pass preserves routing/rerank observations, not citation rights.
//! The reader can request a manifest rendition through `gptgrep_render`, or a
//! PNG crop in original page points. The conservative complete RPC response is
//! limited to 1 MiB, with at most eight retained visual reservations; oversize
//! renditions fail before payload reads. These are owned-buffer/transport bounds,
//! not global RSS or native image-influence claims. Only completed real writes
//! and flushes followed by source/geometry/hash checks issue citation identities.
pub use crate::protocol::{RichTextCitation, RichVisualCitation, VisualDeliveryIdentity};
use crate::rich_text::{RichEvidence, measure, meter_delta};
use crate::{
    HostConfig,
    jev_accounting::{Accounting, AttemptGuard},
    model_attempts::ModelAttemptTracker,
    protocol,
};
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::{
    PreparedRichMultimodalSearch, ReaderReadMetrics, RichGenerationReader, RichMultimodalAttempt,
    RichMultimodalInitialPass, RichMultimodalSearchOptions, RichMultimodalSearchReport,
    prepare_rich_multimodal_search,
};
use gptgrep_jev::JevClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::time::Duration;

const MAX_OUTPUT: usize = 1_048_576;
const MAX_METADATA: usize = 32768;
const MAX_TEXT: usize = 2048;
const MAX_CITATIONS: usize = 8;
const SEED_HITS: usize = 3;

#[derive(Clone)]
pub struct RichMultimodalHostOptions {
    pub retrieval: RichMultimodalSearchOptions,
    pub max_output_bytes: usize,
    pub max_citations: usize,
}
impl Default for RichMultimodalHostOptions {
    fn default() -> Self {
        Self {
            retrieval: RichMultimodalSearchOptions::default(),
            max_output_bytes: MAX_OUTPUT,
            max_citations: MAX_CITATIONS,
        }
    }
}
/// Exact prepared source/bootstrap and finite original deadline; no DTO factory.
///
/// ```compile_fail
/// let _: gptgrep_host::PreparedRichMultimodalHost = serde_json::from_str("{}").unwrap();
/// ```
pub struct PreparedRichMultimodalHost {
    bootstrap: PreparedRichMultimodalSearch,
    reader: RichGenerationReader,
    question: String,
    config: HostConfig,
    options: RichMultimodalHostOptions,
    deadline: tokio::time::Instant,
    output_reserved: usize,
}
#[derive(Serialize)]
pub struct RichMultimodalHostReport {
    pub schema_version: &'static str,
    pub answer: String,
    pub insufficient_evidence: bool,
    pub citations: Vec<RichMultimodalCitation>,
    pub initial_retrieval: RichMultimodalSearchReport,
    pub model_attempts: Vec<crate::ModelAttempt>,
    pub model_usage: crate::ModelUsage,
    pub tool_calls: usize,
    pub read_intervals: Vec<ReaderReadMetrics>,
    pub read_meter_scope: &'static str,
    pub reserved_delivery_registry_bytes: usize,
    pub citation_observation: &'static str,
    pub qualification: &'static str,
    #[serde(skip)]
    _retained: Option<protocol::RichDeliveryHandle>,
}
#[derive(Serialize)]
pub struct RichMultimodalHostError {
    pub code: &'static str,
    pub stage: &'static str,
    pub initial_retrieval: Option<Box<RichMultimodalSearchReport>>,
    pub model_attempts: Vec<crate::ModelAttempt>,
    pub accounting_complete: bool,
    pub unknown_deliveries: usize,
    pub reserved_delivery_registry_bytes: usize,
    #[serde(skip)]
    _retained: Option<protocol::RichDeliveryHandle>,
}
impl std::fmt::Display for RichMultimodalHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} during {}", self.code, self.stage)
    }
}
impl std::fmt::Debug for RichMultimodalHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for RichMultimodalHostError {}

/// Validate complete host/bootstrap bounds before admitting the mandatory Jev pass.
/// Caller supplies an explicit client with its existing finite original context.
pub fn prepare_rich_multimodal_host(
    reader: RichGenerationReader,
    client: JevClient,
    question: &str,
    config: &HostConfig,
    options: &RichMultimodalHostOptions,
) -> Result<PreparedRichMultimodalHost> {
    crate::validate_config(config, question)?;
    ensure!(
        config.model == "gpt-6-luna"
            && config.reasoning_effort == "max"
            && config.service_tier == "fast",
        "host_multimodal_selected_profile_required"
    );
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
    let prompt_bound = 8192 + question.len() * 6 + SEED_HITS * (MAX_METADATA + MAX_TEXT * 6) + 4096;
    ensure!(
        prompt_bound <= config.max_input_bytes && prompt_bound * 6 + 8192 <= 4 * 1024 * 1024,
        "host_rich_input_budget"
    );
    ensure!(
        options.retrieval.resources.owned_working_bytes >= 6 * 1_048_576,
        "host_rich_visual_working_budget"
    );
    let mut core_options = options.retrieval.clone();
    core_options.max_output_bytes = core_options.max_output_bytes.min(524288);
    core_options.max_metadata_bytes = core_options.max_metadata_bytes.min(MAX_METADATA);
    core_options.window_bytes = core_options.window_bytes.min(MAX_TEXT);
    let bootstrap =
        prepare_rich_multimodal_search(reader.clone(), client.clone(), question, &core_options)?;
    // Inspect the genuine prepared pool, including the independent visual lane
    // outside routing's first eight documents, before ANY paid admission.
    for (_, content) in bootstrap.candidate_scope() {
        match content {
            gptgrep_core::RichMultimodalContent::Text { window, .. } => {
                measure(&window.evidence, 16384)?;
                ensure!(window.text.len() <= MAX_TEXT, "host_rich_seed_text_budget");
            }
            gptgrep_core::RichMultimodalContent::Visual { candidate } => {
                protocol::admit_visual_candidate_metadata(candidate)?;
            }
        }
        client.execution_context().check_deadline()?;
    }
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
    Ok(PreparedRichMultimodalHost {
        bootstrap,
        reader,
        question: question.into(),
        config: config.clone(),
        options: options.clone(),
        deadline,
        output_reserved,
    })
}
impl PreparedRichMultimodalHost {
    pub fn reserved_output_bytes(&self) -> usize {
        self.output_reserved
    }
    /// Explicit activation; original ask/CLI/default dispatch is unchanged.
    /// Complete observed Jev usage is borrowed by this private sink before a later await.
    pub async fn execute(
        self,
        observer: &(dyn Fn(&RichMultimodalAttempt) -> Result<()> + Send + Sync),
    ) -> std::result::Result<RichMultimodalHostReport, RichMultimodalHostError> {
        let initial = match self.bootstrap.execute_with_observer(observer).await {
            Ok(initial) => initial,
            Err(error) => {
                return Err(RichMultimodalHostError {
                    code: error.code,
                    stage: error.stage,
                    initial_retrieval: Some(Box::new(error.report)),
                    model_attempts: vec![],
                    accounting_complete: false,
                    unknown_deliveries: 0,
                    reserved_delivery_registry_bytes: 0,
                    _retained: None,
                });
            }
        };
        let mut evidence = match from_initial(
            initial,
            self.options.max_citations,
            self.deadline,
            self.options.retrieval.document.clone(),
            self.options.retrieval.resources.clone(),
        ) {
            Ok(evidence) => evidence,
            Err(error) => return Err(error),
        };
        let manifest = match self.reader.manifest() {
            Ok(manifest) => manifest,
            Err(_) => {
                return Err(failure(
                    evidence,
                    "host_rich_source_unavailable",
                    "reader_start",
                    vec![],
                ));
            }
        };
        let accounting = match Accounting::create(&manifest.root, &self.reader.pin().generation) {
            Ok(accounting) => accounting,
            Err(_) => {
                return Err(failure(
                    evidence,
                    "host_rich_accounting_unavailable",
                    "reader_start",
                    vec![],
                ));
            }
        };
        let _guard = AttemptGuard(accounting.clone());
        let mut tracker =
            match ModelAttemptTracker::reserve(&accounting, "rich_multimodal_reader", &self.config)
            {
                Ok(tracker) => tracker,
                Err(_) => {
                    return Err(failure(
                        evidence,
                        "host_rich_model_attempt_unavailable",
                        "reader_start",
                        accounting.model_attempts(),
                    ));
                }
            };
        let result = crate::run_process_until(
            &self.config,
            protocol::Workflow::RichMultimodal {
                question: &self.question,
                evidence: &mut evidence,
            },
            Some(self.deadline),
            protocol::RunOptions {
                observer: Some(tracker.observer()),
                max_output_bytes: Some(65536),
            },
        )
        .await;
        let observed_error = result.as_ref().err();
        if tracker.finish(observed_error).is_err() {
            return Err(failure(
                evidence,
                "host_rich_accounting_unavailable",
                "reader_finish",
                accounting.model_attempts(),
            ));
        }
        let outcome = match result {
            Ok(outcome) => outcome.result,
            Err(_) => {
                return Err(failure(
                    evidence,
                    "host_rich_reader_failed",
                    "reader",
                    accounting.model_attempts(),
                ));
            }
        };
        let final_value = match finish(&evidence, &outcome.answer) {
            Ok(value) => value,
            Err(_) => {
                return Err(failure(
                    evidence,
                    "host_rich_final_evidence_rejected",
                    "reader_finish",
                    accounting.model_attempts(),
                ));
            }
        };
        let attempts = accounting.model_attempts();
        let usage = crate::model_attempts::summarize(&attempts);
        into_report(
            evidence,
            final_value,
            attempts,
            usage,
            self.output_reserved,
            Some(&accounting),
        )
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RichMultimodalCitation {
    Text { delivery: Box<RichTextCitation> },
    Image { delivery: Box<RichVisualCitation> },
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CitationRequest {
    Text {
        issued_id: String,
        offset: usize,
        length: usize,
        quote: String,
    },
    Image {
        issued_id: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Final {
    answer: String,
    citations: Vec<CitationRequest>,
    insufficient_evidence: bool,
}
struct ValidatedFinal {
    value: Final,
    citations: Vec<RichMultimodalCitation>,
}

fn from_initial(
    initial: RichMultimodalInitialPass,
    max_citations: usize,
    deadline: tokio::time::Instant,
    document: Option<String>,
    resources: gptgrep_parse::ResourceBudget,
) -> std::result::Result<RichEvidence, RichMultimodalHostError> {
    let checked = (|| -> Result<_> {
        initial.validate()?;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "host_deadline_exceeded"
        );
        initial.reader().read_metrics()
    })();
    let before = match checked {
        Ok(before) => before,
        Err(_) => {
            return Err(RichMultimodalHostError {
                code: "host_rich_initial_stale_or_expired",
                stage: "reader_start",
                initial_retrieval: Some(Box::new(initial.into_report())),
                model_attempts: vec![],
                accounting_complete: false,
                unknown_deliveries: 0,
                reserved_delivery_registry_bytes: 0,
                _retained: None,
            });
        }
    };
    Ok(RichEvidence {
        reader: initial.reader().clone(),
        client: initial.client().clone(),
        initial: None,
        multimodal: Some(initial),
        visual_resources: Some(resources),
        delivery: None,
        max_citations,
        deadline,
        before,
        calls: 0,
        document,
    })
}
fn failure(
    mut evidence: RichEvidence,
    code: &'static str,
    stage: &'static str,
    model_attempts: Vec<crate::ModelAttempt>,
) -> RichMultimodalHostError {
    RichMultimodalHostError {
        code,
        stage,
        initial_retrieval: evidence
            .multimodal
            .take()
            .map(|i| Box::new(i.into_report())),
        model_attempts,
        accounting_complete: false,
        unknown_deliveries: evidence
            .delivery
            .as_ref()
            .map_or(0, |h| h.unknown_entries()),
        reserved_delivery_registry_bytes: evidence
            .delivery
            .as_ref()
            .map_or(0, |h| h.retained_bytes()),
        _retained: evidence.delivery.take(),
    }
}
fn resolve(
    evidence: &RichEvidence,
    requests: &[CitationRequest],
) -> Result<Vec<RichMultimodalCitation>> {
    evidence.validate()?;
    ensure!(
        requests.len() <= evidence.max_citations,
        "host_rich_citation_count"
    );
    let handle = evidence
        .delivery
        .as_ref()
        .ok_or_else(|| anyhow!("host_rich_delivery_unattached"))?;
    let mut ids = std::collections::BTreeSet::new();
    let mut result = Vec::with_capacity(requests.len());
    for request in requests {
        let id = match request {
            CitationRequest::Text { issued_id, .. } | CitationRequest::Image { issued_id } => {
                issued_id
            }
        };
        ensure!(
            id.len() <= 64 && ids.insert(id),
            "host_rich_citation_duplicate"
        );
        match request {
            CitationRequest::Text {
                issued_id,
                offset,
                length,
                quote,
            } => {
                let request = protocol::RichCitationRequest {
                    issued_id: issued_id.clone(),
                    offset: *offset,
                    length: *length,
                    quote: quote.clone(),
                };
                let delivery = handle
                    .resolve(&[request], &evidence.reader, evidence.max_citations)?
                    .remove(0);
                result.push(RichMultimodalCitation::Text {
                    delivery: Box::new(delivery),
                });
            }
            CitationRequest::Image { issued_id } => {
                let request = protocol::RichVisualCitationRequest {
                    issued_id: issued_id.clone(),
                };
                let delivery = handle
                    .resolve_visual(&[request], &evidence.reader, evidence.max_citations)?
                    .remove(0);
                result.push(RichMultimodalCitation::Image {
                    delivery: Box::new(delivery),
                });
            }
        }
    }
    evidence.validate()?;
    Ok(result)
}
fn finish(evidence: &RichEvidence, raw: &str) -> Result<ValidatedFinal> {
    evidence.validate()?;
    ensure!(raw.len() <= 65536, "host_rich_final_budget");
    let value: Final = serde_json::from_str(raw).map_err(|_| anyhow!("host_rich_final_schema"))?;
    ensure!(
        value.answer.len() <= 8192 && value.citations.len() <= evidence.max_citations,
        "host_rich_final_budget"
    );
    ensure!(
        value.insufficient_evidence || !value.citations.is_empty(),
        "host_rich_substantive_answer_uncited"
    );
    let citations = resolve(evidence, &value.citations)?;
    Ok(ValidatedFinal { value, citations })
}
fn into_report(
    mut evidence: RichEvidence,
    mut final_value: ValidatedFinal,
    model_attempts: Vec<crate::ModelAttempt>,
    model_usage: crate::ModelUsage,
    limit: usize,
    accounting: Option<&Accounting>,
) -> std::result::Result<RichMultimodalHostReport, RichMultimodalHostError> {
    #[derive(Serialize)]
    struct Preview<'a> {
        schema_version: &'static str,
        answer: &'a str,
        insufficient_evidence: bool,
        citations: &'a [RichMultimodalCitation],
        initial_retrieval: &'a RichMultimodalSearchReport,
        model_attempts: &'a [crate::ModelAttempt],
        model_usage: &'a crate::ModelUsage,
        tool_calls: usize,
        read_intervals: Vec<ReaderReadMetrics>,
        read_meter_scope: &'static str,
        reserved_delivery_registry_bytes: usize,
        citation_observation: &'static str,
        qualification: &'static str,
    }
    const OBSERVATION: &str = "host_complete_frame_write_flush_and_fresh_typed_artifact";
    const QUALIFICATION: &str = "model_image_influence_and_actual_served_profile_unobserved";
    const METER: &str = "shared_retained_reader_interval_not_exclusive";
    let checked = (|| -> Result<_> {
        measure(
            &Preview {
                schema_version: "gptgrep.rich_multimodal.host.v1",
                answer: &final_value.value.answer,
                insufficient_evidence: final_value.value.insufficient_evidence,
                citations: &final_value.citations,
                initial_retrieval: evidence
                    .multimodal
                    .as_ref()
                    .expect("genuine opaque pass")
                    .report(),
                model_attempts: &model_attempts,
                model_usage: &model_usage,
                tool_calls: evidence.calls,
                read_intervals: vec![meter_delta(
                    &evidence.reader.read_metrics()?,
                    &evidence.before,
                )?],
                read_meter_scope: METER,
                reserved_delivery_registry_bytes: evidence
                    .delivery
                    .as_ref()
                    .map_or(0, |h| h.retained_bytes()),
                citation_observation: OBSERVATION,
                qualification: QUALIFICATION,
            },
            limit,
        )?;
        evidence.validate()?;
        if let Some(accounting) = accounting {
            accounting.finish("completed")?;
        }
        evidence.validate()?;
        let citations = resolve(&evidence, &final_value.value.citations)?;
        evidence.validate()?;
        let interval = meter_delta(&evidence.reader.read_metrics()?, &evidence.before)?;
        evidence.validate()?;
        Ok((citations, interval))
    })();
    let (citations, interval) = match checked {
        Ok(value) => value,
        Err(_) => {
            return Err(failure(
                evidence,
                "host_rich_completion_rejected",
                "reader_finish",
                model_attempts,
            ));
        }
    };
    final_value.citations = citations;
    Ok(RichMultimodalHostReport {
        schema_version: "gptgrep.rich_multimodal.host.v1",
        answer: final_value.value.answer,
        insufficient_evidence: final_value.value.insufficient_evidence,
        citations: final_value.citations,
        initial_retrieval: evidence
            .multimodal
            .take()
            .expect("genuine opaque pass")
            .into_report(),
        model_attempts,
        model_usage,
        tool_calls: evidence.calls,
        read_intervals: vec![interval],
        read_meter_scope: METER,
        reserved_delivery_registry_bytes: evidence
            .delivery
            .as_ref()
            .map_or(0, |h| h.retained_bytes()),
        citation_observation: OBSERVATION,
        qualification: QUALIFICATION,
        _retained: evidence.delivery.take(),
    })
}

pub(crate) fn tools() -> Value {
    let mut tools = crate::rich_text::tools();
    tools[0]["description"] =
        json!("Source-bound text and images; only actual completed deliveries issue citation IDs");
    for tool in tools[0]["tools"].as_array_mut().expect("fixed tools") {
        if tool["name"] == "gptgrep_render" {
            tool["inputSchema"]["properties"]["bbox"] = json!({"type":"object","additionalProperties":false,
                "properties":{"x":{"type":"number","minimum":0},"y":{"type":"number","minimum":0},"width":{"type":"number","exclusiveMinimum":0},"height":{"type":"number","exclusiveMinimum":0}},
                "required":["x","y","width","height"]});
            tool["description"] = json!(
                "Read exact pinned rendition as an image; optional bbox is in original page points, PNG crop only"
            );
        }
    }
    tools
}
pub(crate) fn final_schema(maximum: usize) -> Value {
    json!({"type":"object","additionalProperties":false,
        "properties":{"answer":{"type":"string","maxLength":8192},"insufficient_evidence":{"type":"boolean"},
        "citations":{"type":"array","maxItems":maximum,"items":{"oneOf":[
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"text"},"issued_id":{"type":"string","maxLength":64},
                "offset":{"type":"integer","minimum":0},"length":{"type":"integer","minimum":1,"maximum":2048},"quote":{"type":"string","maxLength":2048}},
                "required":["kind","issued_id","offset","length","quote"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"image"},"issued_id":{"type":"string","maxLength":64}},
                "required":["kind","issued_id"]}]}}},"required":["answer","citations","insufficient_evidence"]})
}

#[cfg(test)]
mod tests;
