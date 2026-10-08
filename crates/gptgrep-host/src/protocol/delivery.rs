//! Private text registry: only the actual Rpc decoder and writer mint rights.
use super::{CompletedFrame, ReceivedFrame};
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::{ArtifactTextWindow, EvidenceRef, RichGenerationReader, TextEvidenceBinding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
pub(super) mod visual;
pub(crate) use visual::VisualCitationRequest;
pub use visual::{DeliveredVisualCitation, VisualDeliveryIdentity};
pub(super) use visual::{RenderRequest, VisualSettings};

pub(super) const MAX_TEXT: usize = 2048;
pub(super) const MAX_METADATA: usize = 16384;
const ENTRY_CHARGE: usize = 131072;
static SESSIONS: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RpcId {
    String(String),
    Integer(i64),
}
impl RpcId {
    pub(super) fn decode(value: &Value) -> Result<Self> {
        if let Some(value) = value.as_str() {
            ensure!(value.len() <= 256, "host_rich_rpc_id_limit");
            return Ok(Self::String(value.into()));
        }
        value
            .as_i64()
            .map(Self::Integer)
            .ok_or_else(|| anyhow!("host_rich_rpc_id_type"))
    }
    pub(super) fn value(&self) -> Value {
        match self {
            Self::String(s) => s.clone().into(),
            Self::Integer(n) => (*n).into(),
        }
    }
}

pub(super) struct PendingCall {
    session: u64,
    order: u64,
    pub(super) rpc_id: RpcId,
    pub(super) tool: String,
    pub(super) arguments: Value,
}

#[derive(Clone, Serialize)]
pub struct TextSendReceipt {
    pub channel: &'static str,
    pub observation: &'static str,
    pub frame_sha256: String,
    pub frame_bytes: usize,
}
#[derive(Clone, Serialize)]
pub struct DeliveredTextCitation {
    pub issued_id: String,
    pub evidence: EvidenceRef,
    pub quote: String,
    pub delivered_text_sha256: String,
    pub host_send: TextSendReceipt,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CitationRequest {
    pub issued_id: String,
    /// UTF-8 byte coordinates relative to this exact delivered window.
    pub offset: usize,
    pub length: usize,
    pub quote: String,
}

struct Entry {
    window: ArtifactTextWindow,
    text_sha256: String,
    send: TextSendReceipt,
}
pub(super) struct Registry {
    reader: RichGenerationReader,
    client: gptgrep_jev::JevClient,
    deadline: tokio::time::Instant,
    session: u64,
    rpc_ids: BTreeSet<RpcId>,
    call_ids: BTreeSet<String>,
    next_order: u64,
    active_order: Option<u64>,
    retained_entries: usize,
    max_entries: usize,
    entries: BTreeMap<String, Arc<Entry>>,
    unknown: Vec<Arc<Entry>>,
    visual: Option<VisualSettings>,
    visual_reserved: usize,
    visual_entries: BTreeMap<String, Arc<visual::VisualEntry>>,
    visual_unknown: Vec<Arc<visual::VisualEntry>>,
}
#[derive(Clone)]
pub(crate) struct Handle(Arc<Mutex<Registry>>);
impl Handle {
    pub(super) fn attach(
        reader: RichGenerationReader,
        client: gptgrep_jev::JevClient,
        deadline: tokio::time::Instant,
        max_entries: usize,
    ) -> Result<Self> {
        reader.validate()?;
        client.execution_context().check_deadline()?;
        ensure!((1..=64).contains(&max_entries), "host_rich_entry_budget");
        Ok(Self(Arc::new(Mutex::new(Registry {
            reader,
            client,
            deadline,
            session: SESSIONS.fetch_add(1, Ordering::SeqCst),
            rpc_ids: BTreeSet::new(),
            call_ids: BTreeSet::new(),
            next_order: 0,
            active_order: None,
            retained_entries: 0,
            max_entries,
            entries: BTreeMap::new(),
            unknown: Vec::new(),
            visual: None,
            visual_reserved: 0,
            visual_entries: BTreeMap::new(),
            visual_unknown: Vec::new(),
        }))))
    }
    pub(super) fn admit(
        &self,
        frame: ReceivedFrame,
        thread: &str,
        turn: &str,
        maximum_calls: usize,
    ) -> Result<PendingCall> {
        {
            let state = self
                .0
                .lock()
                .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
            state.client.execution_context().check_deadline()?;
            ensure!(
                tokio::time::Instant::now() < state.deadline,
                "host_deadline_exceeded"
            );
        }
        let mut frame = frame.0;
        ensure!(
            frame["method"] == "item/tool/call",
            "host_rich_pending_method"
        );
        ensure!(
            frame["params"]["threadId"] == thread && frame["params"]["turnId"] == turn,
            "host_rich_pending_scope"
        );
        let rpc_id = RpcId::decode(&frame["id"])?;
        let call_id = super::identifier(&frame["params"]["callId"])?;
        let tool = frame["params"]["tool"]
            .as_str()
            .ok_or_else(|| anyhow!("host_rich_tool_type"))?
            .to_owned();
        ensure!(
            frame["params"]["namespace"] == "gptgrep" && crate::rich_text::allowed(&tool),
            "host_rich_tool_allowlist"
        );
        let arguments = frame["params"]["arguments"].take();
        crate::rich_text::measure(&arguments, 4096)?;
        if tool == "gptgrep_render" && self.visual_enabled() {
            RenderRequest::parse(&arguments)?;
        } else {
            crate::rich_text::validate_arguments(&tool, &arguments)?;
        }
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            state.rpc_ids.len() < maximum_calls && state.call_ids.len() < maximum_calls,
            "host_rich_tool_budget"
        );
        ensure!(
            !state.rpc_ids.contains(&rpc_id) && !state.call_ids.contains(&call_id),
            "host_rich_call_replayed"
        );
        ensure!(state.active_order.is_none(), "host_rich_dispatch_busy");
        let order = state.next_order;
        state.next_order = order
            .checked_add(1)
            .ok_or_else(|| anyhow!("host_rich_dispatch_exhausted"))?;
        state.rpc_ids.insert(rpc_id.clone());
        state.call_ids.insert(call_id);
        state.active_order = Some(order);
        Ok(PendingCall {
            session: state.session,
            order,
            rpc_id,
            tool,
            arguments,
        })
    }
    pub(super) fn close(&self, pending: PendingCall) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            pending.session == state.session && state.active_order == Some(pending.order),
            "host_rich_pending_order"
        );
        state.active_order = None;
        Ok(())
    }
    pub(super) fn reserve(
        &self,
        pending: &PendingCall,
        window: ArtifactTextWindow,
        reader: &RichGenerationReader,
    ) -> Result<Reserved> {
        self.validate_owner(reader)?;
        ensure!(
            pending.tool == "gptgrep_read_artifact",
            "host_rich_delivery_tool"
        );
        let binding = binding(&window.evidence)?;
        ensure!(
            pending.arguments["artifact_id"] == binding.artifact_id
                && pending.arguments["offset"].as_u64().unwrap_or(0) == binding.byte_start as u64,
            "host_rich_delivery_resource"
        );
        let requested = pending.arguments["max_bytes"].as_u64().unwrap_or(1024);
        ensure!(
            !window.text.is_empty()
                && window.text.len() <= MAX_TEXT
                && window.text.len() as u64 <= requested,
            "host_rich_delivery_window"
        );
        crate::rich_text::measure(&window.evidence, MAX_METADATA)?;
        fresh(reader, &window)?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            pending.session == state.session && state.active_order == Some(pending.order),
            "host_rich_pending_order"
        );
        ensure!(
            state.retained_entries < state.max_entries,
            "host_rich_entry_budget"
        );
        state.retained_entries += 1;
        let id = format!("r{:016x}_{:016x}", state.session, pending.order);
        drop(state);
        Ok(Reserved {
            handle: self.clone(),
            id,
            window: Some(window),
            armed: true,
        })
    }
    fn validate_owner(&self, reader: &RichGenerationReader) -> Result<()> {
        let state = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
        ensure!(
            state.reader.pin() == reader.pin(),
            "host_rich_registry_pin_mismatch"
        );
        state.client.execution_context().check_deadline()?;
        state.reader.validate()?;
        state.client.execution_context().check_deadline()?;
        ensure!(
            tokio::time::Instant::now() < state.deadline,
            "host_deadline_exceeded"
        );
        Ok(())
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.retained_entries * ENTRY_CHARGE + state.visual_reserved * visual::ENTRY_CHARGE
    }
    pub(crate) fn unknown_entries(&self) -> usize {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.unknown.len() + state.visual_unknown.len()
    }
    pub(crate) fn resolve(
        &self,
        requests: &[CitationRequest],
        reader: &RichGenerationReader,
        maximum: usize,
    ) -> Result<Vec<DeliveredTextCitation>> {
        self.validate_owner(reader)?;
        ensure!(
            requests.len() <= maximum && maximum <= 8,
            "host_rich_citation_count"
        );
        let selected = {
            let state = self
                .0
                .lock()
                .map_err(|_| anyhow!("host_rich_registry_unavailable"))?;
            let mut ids = BTreeSet::new();
            requests
                .iter()
                .map(|request| {
                    ensure!(
                        request.issued_id.len() <= 64 && ids.insert(request.issued_id.as_str()),
                        "host_rich_citation_duplicate"
                    );
                    state
                        .entries
                        .get(&request.issued_id)
                        .cloned()
                        .ok_or_else(|| anyhow!("host_rich_citation_unissued"))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut citations = Vec::with_capacity(requests.len());
        for (request, entry) in requests.iter().zip(selected) {
            fresh(reader, &entry.window)?;
            ensure!(
                request.length > 0 && request.length <= MAX_TEXT,
                "host_rich_citation_range"
            );
            let end = request
                .offset
                .checked_add(request.length)
                .ok_or_else(|| anyhow!("host_rich_citation_range"))?;
            let quote = entry
                .window
                .text
                .get(request.offset..end)
                .ok_or_else(|| anyhow!("host_rich_citation_range"))?;
            ensure!(quote == request.quote, "host_rich_citation_quote");
            let original = binding(&entry.window.evidence)?;
            let offset = original
                .byte_start
                .checked_add(request.offset)
                .ok_or_else(|| anyhow!("host_rich_citation_range"))?;
            let exact = reader.read_text(&original.artifact_id, offset, request.length)?;
            ensure!(exact.text == quote, "host_rich_citation_source_changed");
            reader.validate_text_evidence(&exact.evidence)?;
            citations.push(DeliveredTextCitation {
                issued_id: request.issued_id.clone(),
                evidence: exact.evidence,
                quote: quote.into(),
                delivered_text_sha256: entry.text_sha256.clone(),
                host_send: entry.send.clone(),
            });
            fresh(reader, &entry.window)?;
        }
        Ok(citations)
    }
}
pub(super) struct Reserved {
    handle: Handle,
    pub(super) id: String,
    window: Option<ArtifactTextWindow>,
    armed: bool,
}
impl Reserved {
    pub(super) fn window(&self) -> &ArtifactTextWindow {
        self.window.as_ref().expect("one reserved window")
    }
    pub(super) fn result(&self) -> Result<Value> {
        let body = serde_json::json!({"schema_version":"gptgrep.rich_text.delivery.v1",
            "issued_id":self.id,"window":self.window(),"citation_status":"registered_only_after_host_write_flush_and_freshness"});
        crate::rich_text::measure(&body, 65536)?;
        Ok(
            serde_json::json!({"success":true,"contentItems":[{"type":"inputText","text":serde_json::to_string(&body)?}]}),
        )
    }
    pub(super) fn commit(
        mut self,
        proof: CompletedFrame,
        pending: PendingCall,
        reader: &RichGenerationReader,
    ) -> Result<()> {
        self.handle.validate_owner(reader)?;
        fresh(reader, self.window())?;
        ensure!(
            proof.rpc_id.as_ref() == Some(&pending.rpc_id)
                && proof.result_sha256.as_ref()
                    == Some(&hash(serde_json::to_string(&self.result()?)?.as_bytes())),
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
            !state.entries.contains_key(&self.id),
            "host_rich_citation_collision"
        );
        let window = self.window.take().expect("one reserved window");
        let entry = Arc::new(Entry {
            text_sha256: hash(window.text.as_bytes()),
            window,
            send: TextSendReceipt {
                channel: "codex_dynamic_tool_content_items_input_text",
                observation: "host_write_and_flush",
                frame_sha256: proof.sha256,
                frame_bytes: proof.bytes,
            },
        });
        state.entries.insert(self.id.clone(), entry);
        state.active_order = None;
        self.armed = false;
        Ok(())
    }
}
impl Drop for Reserved {
    fn drop(&mut self) {
        if self.armed
            && let Some(window) = self.window.take()
        {
            let mut state = self.handle.0.lock().unwrap_or_else(|e| e.into_inner());
            state.unknown.push(Arc::new(Entry {
                text_sha256: hash(window.text.as_bytes()),
                window,
                send: TextSendReceipt {
                    channel: "codex_dynamic_tool_content_items_input_text",
                    observation: "unconfirmed_send_or_freshness",
                    frame_sha256: String::new(),
                    frame_bytes: 0,
                },
            }));
            state.active_order = None;
        }
    }
}
pub(super) fn binding(evidence: &EvidenceRef) -> Result<&TextEvidenceBinding> {
    match evidence {
        EvidenceRef::NativeText { binding } | EvidenceRef::OcrText { binding, .. } => Ok(binding),
        _ => Err(anyhow!("host_rich_text_kind")),
    }
}
fn fresh(reader: &RichGenerationReader, window: &ArtifactTextWindow) -> Result<()> {
    reader.validate_text_evidence(&window.evidence)?;
    let binding = binding(&window.evidence)?;
    let current = reader.read_text(&binding.artifact_id, binding.byte_start, window.text.len())?;
    ensure!(
        current.text == window.text
            && serde_json::to_value(current.evidence)? == serde_json::to_value(&window.evidence)?,
        "host_rich_delivered_source_changed"
    );
    reader.validate()?;
    Ok(())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
