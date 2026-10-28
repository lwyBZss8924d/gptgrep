//! Image rights are minted by the same private, completed Rpc frame as text.
use super::{CompletedFrame, Handle, PendingCall, TextSendReceipt, hash};
use anyhow::{Result, anyhow, ensure};
use base64::Engine;
use gptgrep_core::{
    DerivedVisualCache, DerivedVisualPin, DerivedVisualReceipt, EvidenceRef, RichGenerationReader,
    VerifiedVisualPayload, VisualCandidate,
};
use gptgrep_parse::{BoundingBox, ResourceBudget};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub(super) const ENTRY_CHARGE: usize = 2_097_152;
pub(crate) const MAX_RENDER_FRAME: usize = 1_048_576;
const PAYLOAD_ENVELOPE: usize = 786_432;
const MAX_IMAGES: usize = 8;
const MAX_VISUAL_METADATA: usize = 32_768;

#[derive(Clone)]
pub(crate) struct VisualSettings {
    pub resources: ResourceBudget,
    pub cache: DerivedVisualCache,
    pub document: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderRequest {
    pub resource_id: String,
    #[serde(default)]
    pub bbox: Option<BoundingBox>,
}
impl RenderRequest {
    pub(crate) fn parse(value: &Value) -> Result<Self> {
        let request: Self = serde_json::from_value(value.clone())
            .map_err(|_| anyhow!("host_rich_render_arguments"))?;
        ensure!(
            !request.resource_id.is_empty() && request.resource_id.len() <= 256,
            "host_rich_render_arguments"
        );
        Ok(request)
    }
}

/// Exact manifest rendition or locally generated, source-bound PNG crop.
/// No model-supplied metadata or reconstructed receipt can enter the registry.
#[derive(Clone, Serialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum VisualDeliveryIdentity {
    Manifest { candidate: Box<VisualCandidate> },
    Crop { receipt: Box<DerivedVisualReceipt> },
}
#[derive(Clone, Serialize)]
pub struct DeliveredVisualCitation {
    pub issued_id: String,
    pub evidence: EvidenceRef,
    pub image: VisualDeliveryIdentity,
    pub delivered_image_sha256: String,
    pub host_send: TextSendReceipt,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VisualCitationRequest {
    pub issued_id: String,
}

pub(crate) fn admit_candidate_metadata(candidate: &VisualCandidate) -> Result<()> {
    let EvidenceRef::VisualRegion {
        generation,
        document_id,
        source_path,
        source_sha256,
        rendition_id,
        rendition_sha256,
        ..
    } = &candidate.evidence
    else {
        return Err(anyhow!("host_rich_visual_binding"));
    };
    let mut metadata = 8192usize;
    for value in [
        &generation.generation,
        &generation.manifest_sha256,
        document_id,
        source_path,
        source_sha256,
        rendition_id,
        rendition_sha256,
        &candidate.profile_sha256,
        &candidate.prepared_sha256,
        &candidate.mime,
    ] {
        metadata = metadata
            .checked_add(
                value
                    .len()
                    .checked_mul(24)
                    .ok_or_else(|| anyhow!("host_rich_visual_metadata_budget"))?,
            )
            .ok_or_else(|| anyhow!("host_rich_visual_metadata_budget"))?;
    }
    ensure!(
        metadata <= MAX_VISUAL_METADATA,
        "host_rich_visual_metadata_budget"
    );
    Ok(())
}

enum Payload {
    Manifest(Box<VerifiedVisualPayload>),
    Crop(DerivedVisualPin),
}
impl Payload {
    fn identity(&self) -> VisualDeliveryIdentity {
        match self {
            Self::Manifest(p) => VisualDeliveryIdentity::Manifest {
                candidate: Box::new(p.candidate().clone()),
            },
            Self::Crop(p) => VisualDeliveryIdentity::Crop {
                receipt: Box::new(p.receipt().clone()),
            },
        }
    }
    fn parent(&self) -> &VisualCandidate {
        match self {
            Self::Manifest(p) => p.candidate(),
            Self::Crop(p) => &p.receipt().parent,
        }
    }
    fn sha256(&self) -> &str {
        match self {
            Self::Manifest(p) => match &p.candidate().evidence {
                EvidenceRef::VisualRegion {
                    rendition_sha256, ..
                } => rendition_sha256,
                _ => unreachable!("reader-issued visual payload"),
            },
            Self::Crop(p) => &p.receipt().output_sha256,
        }
    }
    fn mime(&self) -> &str {
        match self {
            Self::Manifest(p) => &p.candidate().mime,
            Self::Crop(p) => &p.receipt().mime,
        }
    }
    fn bytes<'a>(&'a self, settings: &VisualSettings) -> Result<&'a [u8]> {
        match self {
            Self::Manifest(p) => Ok(p.bytes()),
            Self::Crop(p) => p.read(&settings.resources, PAYLOAD_ENVELOPE),
        }
    }
    fn fresh(&self, reader: &RichGenerationReader, settings: &VisualSettings) -> Result<()> {
        let visual = reader.visual_reader()?;
        ensure!(
            visual.pin() == reader.pin(),
            "host_rich_visual_pin_mismatch"
        );
        let (id, document) = match &self.parent().evidence {
            EvidenceRef::VisualRegion {
                rendition_id,
                document_id,
                ..
            } => (rendition_id, document_id),
            _ => return Err(anyhow!("host_rich_visual_binding")),
        };
        // A held, hash-verified file, including original geometry/profile, is
        // revalidated on every commit/citation, without changing CURRENT.
        let original = visual.open_rendition(id, &settings.resources)?;
        ensure!(
            serde_json::to_vec(original.candidate())? == serde_json::to_vec(self.parent())?,
            "host_rich_visual_source_changed"
        );
        visual.verify_source(document)?;
        ensure!(
            hash(self.bytes(settings)?) == self.sha256(),
            "host_rich_visual_payload_changed"
        );
        reader.validate()?;
        Ok(())
    }
}
pub(super) struct VisualEntry {
    payload: Payload,
    send: TextSendReceipt,
}

impl Handle {
    pub(crate) fn enable_visual(&self, settings: VisualSettings) -> Result<()> {
        settings.resources.validate()?;
        ensure!(
            settings.resources.owned_working_bytes >= 6 * MAX_RENDER_FRAME as u64,
            "host_rich_visual_working_budget"
        );
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            state.visual.is_none() && state.rpc_ids.is_empty(),
            "host_rich_visual_already_attached"
        );
        state.visual = Some(settings);
        Ok(())
    }
    pub(crate) fn visual_enabled(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .visual
            .is_some()
    }
    /// Reserve retained payload/metadata capacity before artifact bytes/crop.
    pub(in crate::protocol) fn reserve_visual(
        &self,
        pending: &PendingCall,
        reader: &RichGenerationReader,
    ) -> Result<VisualReserved> {
        self.validate_owner(reader)?;
        ensure!(pending.tool == "gptgrep_render", "host_rich_delivery_tool");
        let request = RenderRequest::parse(&pending.arguments)?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            pending.session == state.session && state.active_order == Some(pending.order),
            "host_rich_pending_order"
        );
        let settings = state
            .visual
            .clone()
            .ok_or_else(|| anyhow!("capability_unavailable"))?;
        ensure!(
            state.visual_reserved < state.max_entries.min(MAX_IMAGES),
            "host_rich_visual_entry_budget"
        );
        let manifest = reader.manifest()?;
        let (doc, page, image) = manifest
            .documents
            .iter()
            .filter(|d| {
                settings
                    .document
                    .as_ref()
                    .is_none_or(|p| p == &d.document.path)
            })
            .find_map(|d| {
                d.pages.iter().find_map(|p| {
                    p.renditions
                        .iter()
                        .find(|i| i.artifact.id == request.resource_id)
                        .map(|i| (d, p, i))
                })
            })
            .ok_or_else(|| anyhow!("host_rich_visual_resource_unavailable"))?;
        // Bound the duplicated source/candidate/recipe strings and fixed numeric
        // geometry before opening or copying image data. JSON escaping <=6x;
        // each source string appears <=4 times in either eventual sidecar.
        let pin = reader.pin();
        let mut metadata = 8192usize;
        for value in [
            &pin.generation,
            &pin.manifest_sha256,
            &doc.document.id,
            &doc.document.path,
            &doc.document.source_sha256,
            &doc.prepared.prepared_sha256,
            &image.artifact.id,
            &image.artifact.sha256,
            &image.artifact.profile_sha256,
            &image.artifact.mime,
        ] {
            metadata = metadata
                .checked_add(
                    value
                        .len()
                        .checked_mul(24)
                        .ok_or_else(|| anyhow!("host_rich_visual_metadata_budget"))?,
                )
                .ok_or_else(|| anyhow!("host_rich_visual_metadata_budget"))?;
        }
        ensure!(
            metadata <= MAX_VISUAL_METADATA,
            "host_rich_visual_metadata_budget"
        );
        settings
            .resources
            .admit_image(image.width, image.height, image.artifact.bytes)?;
        if let Some(bbox) = request.bbox {
            bbox.validate(page.width_points, page.height_points)?;
        }
        state.visual_reserved += 1;
        let id = format!("v{:016x}_{:016x}", state.session, pending.order);
        Ok(VisualReserved {
            handle: self.clone(),
            id,
            settings,
            request,
            payload: None,
            result_sha256: None,
            armed: true,
        })
    }
    pub(crate) fn resolve_visual(
        &self,
        requests: &[VisualCitationRequest],
        reader: &RichGenerationReader,
        maximum: usize,
    ) -> Result<Vec<DeliveredVisualCitation>> {
        self.validate_owner(reader)?;
        ensure!(
            requests.len() <= maximum && maximum <= 8,
            "host_rich_citation_count"
        );
        let (settings, entries) = {
            let state = self
                .0
                .lock()
                .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
            let settings = state
                .visual
                .clone()
                .ok_or_else(|| anyhow!("host_rich_visual_disabled"))?;
            let mut ids = BTreeSet::new();
            let entries = requests
                .iter()
                .map(|r| {
                    ensure!(
                        r.issued_id.len() <= 64 && ids.insert(&r.issued_id),
                        "host_rich_citation_duplicate"
                    );
                    state
                        .visual_entries
                        .get(&r.issued_id)
                        .cloned()
                        .ok_or_else(|| anyhow!("host_rich_citation_unissued"))
                })
                .collect::<Result<Vec<_>>>()?;
            (settings, entries)
        };
        let mut citations = Vec::with_capacity(entries.len());
        for (request, entry) in requests.iter().zip(entries) {
            entry.payload.fresh(reader, &settings)?;
            self.validate_owner(reader)?;
            citations.push(DeliveredVisualCitation {
                issued_id: request.issued_id.clone(),
                evidence: entry.payload.parent().evidence.clone(),
                image: entry.payload.identity(),
                delivered_image_sha256: entry.payload.sha256().into(),
                host_send: entry.send.clone(),
            });
        }
        self.validate_owner(reader)?;
        Ok(citations)
    }
}

pub(in crate::protocol) struct VisualReserved {
    handle: Handle,
    id: String,
    settings: VisualSettings,
    request: RenderRequest,
    payload: Option<Payload>,
    result_sha256: Option<String>,
    armed: bool,
}
impl VisualReserved {
    pub(in crate::protocol) fn load(&mut self, reader: &RichGenerationReader) -> Result<()> {
        self.handle.validate_owner(reader)?;
        ensure!(self.payload.is_none(), "host_rich_visual_load_replayed");
        let visual = reader.visual_reader()?;
        let payload = if let Some(bbox) = self.request.bbox {
            Payload::Crop(self.settings.cache.derive(
                visual.open_rendition(&self.request.resource_id, &self.settings.resources)?,
                bbox,
                &self.settings.resources,
                PAYLOAD_ENVELOPE,
            )?)
        } else {
            Payload::Manifest(Box::new(visual.read_rendition(
                &self.request.resource_id,
                &self.settings.resources,
                PAYLOAD_ENVELOPE,
            )?))
        };
        payload.fresh(reader, &self.settings)?;
        self.handle.validate_owner(reader)?;
        self.payload = Some(payload);
        Ok(())
    }
    pub(in crate::protocol) fn result(&mut self) -> Result<Value> {
        let payload = self
            .payload
            .as_ref()
            .ok_or_else(|| anyhow!("host_rich_visual_not_loaded"))?;
        let body = json!({"schema_version":"gptgrep.rich_visual.delivery.v1", "issued_id":self.id,
            "image":payload.identity(), "evidence":payload.parent().evidence,
            "delivered_image_sha256":payload.sha256(),
            "citation_status":"registered_only_after_host_write_flush_and_freshness"});
        crate::rich_text::measure(&body, MAX_VISUAL_METADATA)?;
        let bytes = payload.bytes(&self.settings)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        // Same DynamicToolCallResponse inputImage/imageUrl DTO as product OCR.
        let result = json!({"success":true,"contentItems":[
            {"type":"inputText","text":serde_json::to_string(&body)?},
            {"type":"inputImage","imageUrl":format!("data:{};base64,{}",payload.mime(),encoded)}]});
        crate::rich_text::measure(&result, MAX_RENDER_FRAME - 4096)?;
        self.result_sha256 = Some(hash(&serde_json::to_vec(&result)?));
        Ok(result)
    }
    pub(in crate::protocol) fn commit(
        mut self,
        proof: CompletedFrame,
        pending: PendingCall,
        reader: &RichGenerationReader,
    ) -> Result<()> {
        self.handle.validate_owner(reader)?;
        self.payload
            .as_ref()
            .ok_or_else(|| anyhow!("host_rich_visual_not_loaded"))?
            .fresh(reader, &self.settings)?;
        ensure!(
            self.result_sha256.is_some()
                && proof.rpc_id.as_ref() == Some(&pending.rpc_id)
                && proof.result_sha256 == self.result_sha256,
            "host_rich_written_payload_mismatch"
        );
        let mut state = self
            .handle
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            pending.session == state.session && state.active_order == Some(pending.order),
            "host_rich_pending_order"
        );
        state.client.execution_context().check_deadline()?;
        ensure!(
            tokio::time::Instant::now() < state.deadline,
            "host_deadline_exceeded"
        );
        ensure!(
            !state.visual_entries.contains_key(&self.id),
            "host_rich_citation_collision"
        );
        state.visual_entries.insert(
            self.id.clone(),
            Arc::new(VisualEntry {
                payload: self.payload.take().expect("loaded"),
                send: TextSendReceipt {
                    channel: "codex_dynamic_tool_content_items_input_image",
                    observation: "host_write_and_flush",
                    frame_sha256: proof.sha256,
                    frame_bytes: proof.bytes,
                },
            }),
        );
        state.active_order = None;
        self.armed = false;
        Ok(())
    }
}
impl Drop for VisualReserved {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.handle.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(payload) = self.payload.take() {
                state.visual_unknown.push(Arc::new(VisualEntry {
                    payload,
                    send: TextSendReceipt {
                        channel: "codex_dynamic_tool_content_items_input_image",
                        observation: "host_write_flush_or_freshness_unconfirmed",
                        frame_sha256: String::new(),
                        frame_bytes: 0,
                    },
                }));
            }
            state.active_order = None;
        }
    }
}
