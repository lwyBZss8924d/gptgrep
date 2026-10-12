//! Separate source-bound DOCUMENT qualification. Page OCR receipts never join.
use super::*;
use anyhow::Context;

/// Pin to the small control envelope, whose two subjects are sealed separately.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentImageQualification {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}
impl DocumentImageQualification {
    fn pin(&self) -> DocumentFilePin {
        DocumentFilePin {
            path: self.path.clone(),
            bytes: self.bytes,
            sha256: self.sha256.clone(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DocumentImageQualificationBindings {
    pub standalone_binary_sha256: String,
    pub guard_source_sha256: String,
    pub guard_schema: String,
    pub guard_contract_sha256: String,
    pub generic_runtime_profile_sha256: String,
    pub permissions_template_sha256: String,
    pub tools_sha256: String,
    pub instructions_sha256: String,
    pub requested_model: String,
    pub requested_effort: String,
    pub normalized_request_service_tier: String,
    pub max_tool_calls: u32,
    pub max_turns: u32,
    pub max_seconds: u32,
    pub frame_bytes: u64,
    pub image_transport_reserved_bytes: u64,
    pub rpc_transport_reserved_bytes: u64,
    pub receipt_schema: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Assertions {
    native_view_image_executed: bool,
    native_source_prepared_assembled_verified: bool,
    common_counter_before_argument_parse: bool,
    failed_duplicate_submissions_charged: bool,
    unknown_or_missing_guard_state_refused: bool,
    normalized_request_profile_verified_before_send: bool,
    complete_source_image_catalog_verified: bool,
    image_contrast_observed: bool,
    image_absence_refused: bool,
    metadata_only_not_accepted_as_image_delivery: bool,
    provider_ingestion_verified: bool,
    actual_served_profile_verified: bool,
}
impl Assertions {
    fn required() -> Self {
        Self {
            native_view_image_executed: true,
            native_source_prepared_assembled_verified: true,
            common_counter_before_argument_parse: true,
            failed_duplicate_submissions_charged: true,
            unknown_or_missing_guard_state_refused: true,
            normalized_request_profile_verified_before_send: true,
            complete_source_image_catalog_verified: true,
            image_contrast_observed: true,
            image_absence_refused: true,
            metadata_only_not_accepted_as_image_delivery: true,
            provider_ingestion_verified: false,
            actual_served_profile_verified: false,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    schema: String,
    qualification_record: DocumentFilePin,
    independent_review: DocumentFilePin,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    accepted: bool,
    bindings: DocumentImageQualificationBindings,
    assertions: Assertions,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subjects {
    qualification_record: DocumentFilePin,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    schema: String,
    verdict: String,
    accepted: bool,
    subjects: Subjects,
    bindings: DocumentImageQualificationBindings,
    assertions: Assertions,
    findings: Vec<Value>,
}
#[derive(Clone, Serialize)]
pub struct DocumentAcceptedImageQualification {
    pub control_sha256: String,
    pub qualification_record_sha256: String,
    pub independent_review_sha256: String,
    pub bindings_sha256: String,
}
fn metadata<T: serde::de::DeserializeOwned>(pin: &DocumentFilePin, cap: u64) -> Result<T> {
    serde_json::from_slice(&read_pin(pin, cap)?)
        .map_err(|_| anyhow!("document_qualification_schema"))
}
pub(super) fn expected(
    control: &DocumentAgentControl,
    profile: &RuntimeProfile,
) -> Result<DocumentImageQualificationBindings> {
    ensure!(
        control.native_viewer.is_some(),
        "document_qualification_native_required"
    );
    let guard = profile
        .native_document_guard
        .as_ref()
        .ok_or_else(|| anyhow!("document_native_guard_required"))?;
    let max_seconds = u32::try_from(profile.selected_max_seconds(control)?)
        .context("document_duration_binding_overflow")?;
    let mut generic = native::canonical(
        json!({"schema":"gptgrep.parse-agent.document-generic-runtime-profile.v1",
        "source_revision":profile.source_revision,"protocol_schema_sha256":profile.protocol_schema_sha256,
        "standalone_app_server":profile.standalone_app_server,"codex_binary_sha256":profile.codex_binary_sha256,
        "descriptor_sha256":profile.descriptor_sha256,"permissions_template_sha256":profile.permissions_template_sha256,
        "tools_sha256":profile.tools_sha256,"instructions_sha256":profile.instructions_sha256,
        "native_document_guard":{"schema":guard.schema,"source_sha256":guard.source_sha256,"contract_sha256":guard.contract_sha256,
            "image_limits":guard.image_limits}}),
    );
    if let Some(seconds) = profile.max_seconds {
        generic["max_seconds"] = json!(seconds);
        generic = native::canonical(generic);
    }
    Ok(DocumentImageQualificationBindings {
        standalone_binary_sha256: control.codex_bin.sha256.clone(),
        guard_source_sha256: guard.source_sha256.clone(),
        guard_schema: guard.schema.clone(),
        guard_contract_sha256: guard.contract_sha256.clone(),
        generic_runtime_profile_sha256: sha(&serde_json::to_vec(&generic)?),
        permissions_template_sha256: profile.permissions_template_sha256.clone(),
        tools_sha256: profile.tools_sha256.clone(),
        instructions_sha256: profile.instructions_sha256.clone(),
        requested_model: DOCUMENT_MODEL.into(),
        requested_effort: "max".into(),
        normalized_request_service_tier: "priority".into(),
        max_tool_calls: 64,
        max_turns: 1,
        max_seconds,
        frame_bytes: 4_194_304,
        image_transport_reserved_bytes: control
            .native_viewer
            .as_ref()
            .expect("validated native control")
            .image_limits
            .image_transport_bytes,
        rpc_transport_reserved_bytes: 33_554_432,
        receipt_schema: "gptgrep.native-document.receipt.v1".into(),
    })
}
pub(super) fn accepted(
    control: &DocumentAgentControl,
    profile: &RuntimeProfile,
) -> Result<Option<DocumentAcceptedImageQualification>> {
    if control.native_viewer.is_none() {
        ensure!(
            control.document_image_qualification.is_none(),
            "document_qualification_transport"
        );
        return Ok(None);
    }
    let selected = control
        .document_image_qualification
        .as_ref()
        .ok_or_else(|| anyhow!("document_native_qualification_required"))?;
    let selection: Selection = metadata(&selected.pin(), 4096)?;
    ensure!(
        selection.schema == "gptgrep.native-document.image-qualification-control.v1"
            && selection.qualification_record.path != selection.independent_review.path,
        "document_qualification_schema"
    );
    let record: Record = metadata(&selection.qualification_record, 16_384)?;
    let review: Review = metadata(&selection.independent_review, 16_384)?;
    let expected = expected(control, profile)?;
    ensure!(
        record.schema == "gptgrep.native-document.image-qualification.v1"
            && record.accepted
            && record.bindings == expected
            && record.assertions == Assertions::required(),
        "document_qualification_record"
    );
    ensure!(
        review.schema == "gptgrep.native-document.image-qualification-independent-review.v1"
            && review.verdict == "ACCEPT_NATIVE_DOCUMENT_IMAGE_QUALIFICATION"
            && review.accepted
            && review.subjects.qualification_record == selection.qualification_record
            && review.bindings == expected
            && review.assertions == record.assertions
            && review.findings.is_empty(),
        "document_qualification_review"
    );
    Ok(Some(DocumentAcceptedImageQualification {
        control_sha256: selected.sha256.clone(),
        qualification_record_sha256: selection.qualification_record.sha256,
        independent_review_sha256: selection.independent_review.sha256,
        bindings_sha256: sha(&serde_json::to_vec(&native::canonical(
            serde_json::to_value(&expected)?,
        ))?),
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    purpose: String,
    invocation_id: String,
    source_alias: String,
    source_sha256: String,
    document_control: DocumentFilePin,
    bindings: DocumentImageQualificationBindings,
    owner_admission_sha256: String,
}
/// Constructed only by reading a pinned purpose record, never by tool JSON.
pub struct DocumentImageQualificationRequest {
    selection: DocumentFilePin,
    request: Request,
}
impl DocumentImageQualificationRequest {
    pub fn source_alias(&self) -> &str {
        &self.request.source_alias
    }
    pub fn sha256(&self) -> &str {
        &self.selection.sha256
    }
    pub fn document_control_pin(&self) -> &DocumentFilePin {
        &self.request.document_control
    }
}
pub fn load_document_image_qualification_request(
    path: &Path,
    expected_sha: &str,
) -> Result<DocumentImageQualificationRequest> {
    let selection = DocumentFilePin {
        path: path.into(),
        bytes: regular(path, 16_384)?.len(),
        sha256: expected_sha.into(),
    };
    let request: Request = metadata(&selection, 16_384)?;
    ensure!(
        request.schema == "gptgrep.native-document.image-qualification-request.v1"
            && request.purpose == "native_document_image_qualification"
            && bounded_id(&request.invocation_id)
            && !request.source_alias.is_empty()
            && request.source_alias.len() <= 128
            && valid_hash(&request.source_sha256)
            && valid_hash(&request.owner_admission_sha256),
        "document_qualification_purpose"
    );
    Ok(DocumentImageQualificationRequest { selection, request })
}
pub(super) fn validate_request(
    request: &DocumentImageQualificationRequest,
    control: &DocumentAgentControl,
    profile: &RuntimeProfile,
    prepared: &DocumentPreparedState,
) -> Result<()> {
    let actual: DocumentAgentControl = metadata(&request.request.document_control, 65_536)?;
    ensure!(
        serde_json::to_value(&actual)? == serde_json::to_value(control)?,
        "document_qualification_control_binding"
    );
    let native = control
        .native_viewer
        .as_ref()
        .ok_or_else(|| anyhow!("document_qualification_native_required"))?;
    ensure!(
        request.request.invocation_id == control.invocation_id
            && request.request.source_sha256 == prepared.source_sha256
            && request.request.owner_admission_sha256 == native.owner_admission_sha256
            && request.request.bindings == expected(control, profile)?,
        "document_qualification_source_binding"
    );
    Ok(())
}
