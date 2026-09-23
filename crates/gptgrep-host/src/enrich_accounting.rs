//! A separate append-only ledger for the explicit raw-document builder.
//! No ask-role counters or protocol observers are shared with this workflow.
use crate::enrich::{BuildBinding, CompletedWindow, EnrichCursor};
use crate::retrieval::hash;
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::NavigationOverlayPublication;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_RECORD_BYTES: usize = 32 * 1024;
pub(crate) const MAX_LEDGER_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CallKind {
    Builder,
    Jev,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reservation {
    pub call_id: usize,
    pub kind: CallKind,
    pub anchor_id: String,
    pub requested_model: String,
    pub request_sha256: String,
    pub request_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observation {
    pub model: String,
    pub provider: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub response_id: Option<String>,
    pub effective_reasoning_effort: Option<String>,
    pub effective_service_tier: Option<String>,
    pub server_retry_notifications: Option<usize>,
    /// Numeric allowlist only. Missing usage is unknown, including on failed calls.
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub call_id: usize,
    pub elapsed_ms: u64,
    pub error_code: Option<String>,
    pub observed: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_usage: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCategory {
    Transport,
    Timeout,
    Http,
    Validation,
    Protocol,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailureInfo {
    pub category: FailureCategory,
    pub http_status_code: Option<u16>,
    pub codex_error_info: Option<crate::CodexErrorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_error_kind: Option<crate::HostProtocolErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_retry_notifications: Option<usize>,
}
impl FailureInfo {
    fn allows_rebuild(&self) -> bool {
        matches!(
            self.category,
            FailureCategory::Transport | FailureCategory::Timeout
        ) || (self.category == FailureCategory::Http
            && self
                .http_status_code
                .is_some_and(|status| matches!(status, 408 | 429 | 500..=599)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupportRetryAdmission {
    pub mode: String,
    pub ordinal: usize,
    pub failed_call_id: usize,
    pub retry_call_id: usize,
    pub backoff_ms: u64,
    pub failure: FailureInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RebuildAdmission {
    pub mode: String,
    pub ordinal: usize,
    pub failed_call_id: usize,
    pub failed_kind: CallKind,
    pub failure_classification: String,
    pub retired_call_start: usize,
    pub retired_call_end: usize,
    pub next_builder_call_id: usize,
    pub anchor_id: String,
    pub builder_input_sha256: String,
    pub builder_input_bytes: usize,
    pub effective_max_builder_calls: usize,
    pub effective_max_jev_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Event {
    Bound {
        binding: BuildBinding,
        plan_sha256: String,
    },
    CallReserved {
        reservation: Reservation,
    },
    CallFinished {
        receipt: Receipt,
    },
    WindowCompleted {
        window: CompletedWindow,
    },
    Checkpoint {
        cursor: Option<EnrichCursor>,
        reason: String,
    },
    Failed {
        code: String,
    },
    WindowRebuildAdmitted {
        admission: RebuildAdmission,
    },
    SupportRetryReserved {
        admission: SupportRetryAdmission,
        reservation: Reservation,
    },
    PublicationPrepared {
        artifact_sha256: String,
    },
    Published {
        publication: NavigationOverlayPublication,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    sequence: usize,
    previous_sha256: Option<String>,
    payload: Event,
    sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnrichCallSummary {
    /// Logical calls admitted before launch; not inferred physical or billed requests.
    pub attempted_calls: usize,
    pub completed_calls: usize,
    pub failed_calls: usize,
    pub unobserved_calls: usize,
    pub missing_usage_calls: usize,
    pub models: Vec<String>,
    pub known_total_tokens: Option<u64>,
    pub missing_total_tokens: usize,
    pub total_tokens_overflowed: bool,
    #[serde(default)]
    pub partial_usage_calls: usize,
}

pub(crate) struct Ledger {
    file: File,
    pub path: PathBuf,
    pub binding: BuildBinding,
    plan_sha256: String,
    pub windows: Vec<CompletedWindow>,
    pub reservations: Vec<Reservation>,
    pub receipts: Vec<Receipt>,
    pub failed: bool,
    pub publication_prepared: Option<String>,
    pub publication: Option<NavigationOverlayPublication>,
    sequence: usize,
    previous_sha256: Option<String>,
    bytes: u64,
    failed_write: bool,
    builder_calls: usize,
    jev_calls: usize,
    committed_calls: usize,
    rebuilds: Vec<RebuildAdmission>,
    support_retries: Vec<SupportRetryAdmission>,
}

impl Ledger {
    pub fn open(
        path: &Path,
        binding: BuildBinding,
        plan_sha256: &str,
        resume: bool,
    ) -> Result<Self> {
        Self::open_mode(path, binding, plan_sha256, resume, None)
    }

    pub fn open_for_rebuild(
        path: &Path,
        binding: BuildBinding,
        plan_sha256: &str,
        failed_call_id: usize,
    ) -> Result<Self> {
        Self::open_mode(path, binding, plan_sha256, true, Some(failed_call_id))
    }

    fn open_mode(
        path: &Path,
        binding: BuildBinding,
        plan_sha256: &str,
        resume: bool,
        rebuild: Option<usize>,
    ) -> Result<Self> {
        ensure!(path.is_absolute(), "enrich_ledger_path_must_be_absolute");
        no_symlinks(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("enrich_ledger_path_invalid"))?;
        fs::create_dir_all(parent).map_err(|_| anyhow!("enrich_ledger_storage_failed"))?;
        no_symlinks(path)?;
        let mut options = OpenOptions::new();
        options.read(true).append(true);
        if !resume {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(path)
            .map_err(|_| anyhow!("enrich_ledger_open_failed"))?;
        file.try_lock()
            .map_err(|_| anyhow!("enrich_ledger_locked"))?;
        let size = file.metadata()?.len();
        ensure!(
            size <= MAX_LEDGER_BYTES && size <= binding.max_ledger_bytes,
            "enrich_ledger_limit"
        );
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        let mut ledger = Self {
            file,
            path: path.to_owned(),
            binding: binding.clone(),
            plan_sha256: plan_sha256.into(),
            windows: vec![],
            reservations: vec![],
            receipts: vec![],
            failed: false,
            publication_prepared: None,
            publication: None,
            sequence: 0,
            previous_sha256: None,
            bytes: 0,
            failed_write: false,
            builder_calls: 0,
            jev_calls: 0,
            committed_calls: 0,
            rebuilds: vec![],
            support_retries: vec![],
        };
        if resume {
            ensure!(
                !bytes.is_empty() && bytes.ends_with(b"\n"),
                "enrich_ledger_partial_or_empty"
            );
            for raw in bytes.split_inclusive(|byte| *byte == b'\n') {
                ensure!(raw.len() <= MAX_RECORD_BYTES, "enrich_ledger_record_limit");
                let line: Line =
                    serde_json::from_slice(raw).map_err(|_| anyhow!("enrich_ledger_invalid"))?;
                ensure!(
                    line.sequence == ledger.sequence
                        && line.previous_sha256 == ledger.previous_sha256,
                    "enrich_ledger_chain_invalid"
                );
                ensure!(
                    line.sha256 == event_hash(line.sequence, &line.previous_sha256, &line.payload)?,
                    "enrich_ledger_digest_invalid"
                );
                ledger.apply(&line.payload)?;
                ledger.sequence += 1;
                ledger.previous_sha256 = Some(line.sha256);
                ledger.bytes += raw.len() as u64;
            }
            ensure!(
                ledger.reservations.len() == ledger.receipts.len(),
                "enrich_pending_call_blocks_resume"
            );
            if let Some(failed_call_id) = rebuild {
                ledger.rebuild_admission(failed_call_id)?;
            } else {
                ensure!(!ledger.failed, "enrich_failed_build_cannot_resume");
                ensure!(
                    ledger.completed_call_count() == ledger.reservations.len(),
                    "enrich_uncommitted_window_blocks_resume"
                );
            }
        } else {
            ledger.append(Event::Bound {
                binding,
                plan_sha256: plan_sha256.into(),
            })?;
            File::open(parent)?.sync_all()?;
        }
        Ok(ledger)
    }

    fn completed_call_count(&self) -> usize {
        self.committed_calls
    }

    pub fn attempt_count(&self, kind: CallKind) -> usize {
        match kind {
            CallKind::Builder => self.builder_calls,
            CallKind::Jev => self.jev_calls,
        }
    }

    pub fn effective_call_limit(&self, kind: CallKind) -> usize {
        match kind {
            CallKind::Builder => self.binding.max_builder_calls + self.rebuilds.len(),
            CallKind::Jev => {
                self.binding.max_jev_calls + self.rebuilds.len() + self.support_retries.len()
            }
        }
    }

    fn rebuild_admission(&self, failed_call_id: usize) -> Result<RebuildAdmission> {
        ensure!(
            self.publication_prepared.is_none() && self.publication.is_none(),
            "enrich_rebuild_publication_ineligible"
        );
        ensure!(self.rebuilds.len() < 2, "enrich_rebuild_limit");
        ensure!(
            self.reservations.len() == self.receipts.len()
                && self.reservations.len().checked_sub(1) == Some(failed_call_id),
            "enrich_rebuild_call_mismatch"
        );
        let start = self.completed_call_count();
        let tail = self
            .reservations
            .get(start..)
            .ok_or_else(|| anyhow!("enrich_rebuild_window_ineligible"))?;
        ensure!(
            (1..=2 + self.binding.support_retries).contains(&tail.len())
                && tail[0].kind == CallKind::Builder
                && (tail.len() == 1
                    || (tail[1..].iter().all(|call| call.kind == CallKind::Jev)
                        && self.receipts[start].error_code.is_none()
                        && (start + 1..failed_call_id).all(|id| self.support_retries.iter().any(
                            |retry| retry.failed_call_id == id && retry.retry_call_id == id + 1
                        )))),
            "enrich_rebuild_window_ineligible"
        );
        let failed = &self.receipts[failed_call_id];
        ensure!(
            matches!(
                failed.error_code.as_deref(),
                Some(
                    "enrich_builder_call_failed"
                        | "enrich_support_call_failed"
                        | "enrich_support_timeout"
                )
            ),
            "enrich_rebuild_failure_ineligible"
        );
        ensure!(
            failed
                .failure
                .as_ref()
                .is_none_or(FailureInfo::allows_rebuild),
            "enrich_rebuild_failure_ineligible"
        );
        ensure!(
            tail.iter().all(|call| call.anchor_id == tail[0].anchor_id),
            "enrich_rebuild_window_ineligible"
        );
        let ordinal = self.rebuilds.len() + 1;
        Ok(RebuildAdmission {
            mode: "rebuild_failed_window_new_builder_sample".into(),
            ordinal,
            failed_call_id,
            failed_kind: self.reservations[failed_call_id].kind,
            failure_classification: failed
                .failure
                .as_ref()
                .map(|failure| {
                    serde_json::to_value(&failure.category)
                        .expect("category")
                        .as_str()
                        .expect("category string")
                        .to_owned()
                })
                .unwrap_or_else(|| "legacy_unclassified".into()),
            retired_call_start: start,
            retired_call_end: self.reservations.len(),
            next_builder_call_id: self.reservations.len(),
            anchor_id: tail[0].anchor_id.clone(),
            builder_input_sha256: tail[0].request_sha256.clone(),
            builder_input_bytes: tail[0].request_bytes,
            effective_max_builder_calls: self.binding.max_builder_calls + ordinal,
            effective_max_jev_calls: self.binding.max_jev_calls
                + ordinal
                + self.support_retries.len(),
        })
    }

    pub fn admit_rebuild(
        &mut self,
        failed_call_id: usize,
        anchor_id: &str,
        builder_input_sha256: &str,
        builder_input_bytes: usize,
    ) -> Result<()> {
        let admission = self.rebuild_admission(failed_call_id)?;
        ensure!(
            admission.anchor_id == anchor_id
                && admission.builder_input_sha256 == builder_input_sha256
                && admission.builder_input_bytes == builder_input_bytes,
            "enrich_rebuild_source_changed"
        );
        ensure!(self.capacity_for_window(), "enrich_rebuild_ledger_capacity");
        self.append(Event::WindowRebuildAdmitted { admission })
    }

    pub fn recovery_summary(&self) -> Value {
        json!({"mode":"explicit_recovery_policies","max_admissions":2,"admissions":self.rebuilds,
            "support_retry_limit":self.binding.support_retries,"support_retry_admissions":self.support_retries,
            "original_max_builder_calls":self.binding.max_builder_calls,"original_max_jev_calls":self.binding.max_jev_calls,
            "effective_max_builder_calls":self.effective_call_limit(CallKind::Builder),"effective_max_jev_calls":self.effective_call_limit(CallKind::Jev)})
    }

    pub fn support_retry(&self, failed_call_id: usize) -> Option<SupportRetryAdmission> {
        if self.support_retries.len() >= self.binding.support_retries
            || self.binding.support_retries > 2
            || self.reservations.len() != self.receipts.len()
            || self.reservations.len().checked_sub(1) != Some(failed_call_id)
            || self.reservations[failed_call_id].kind != CallKind::Jev
            || self.receipts[failed_call_id].error_code.is_none()
        {
            return None;
        }
        let failure = self.receipts[failed_call_id].failure.as_ref()?;
        if !failure.allows_rebuild() {
            return None;
        }
        let ordinal = self.support_retries.len() + 1;
        Some(SupportRetryAdmission {
            mode: "same_request_support_retry".into(),
            ordinal,
            failed_call_id,
            retry_call_id: self.reservations.len(),
            backoff_ms: 250 * ordinal as u64,
            failure: failure.clone(),
        })
    }

    pub fn reserve_support_retry(
        &mut self,
        admission: SupportRetryAdmission,
        reservation: Reservation,
    ) -> Result<()> {
        self.append(Event::SupportRetryReserved {
            admission,
            reservation,
        })
    }

    pub fn last_failed_call(&self) -> Option<Value> {
        self.receipts.iter().rev().find(|receipt| receipt.error_code.is_some()).map(|receipt| json!({
            "call_id":receipt.call_id,"kind":self.reservations[receipt.call_id].kind,"error_code":receipt.error_code,
            "failure":receipt.failure,"classification_available":receipt.failure.is_some(),"partial_usage":receipt.partial_usage,
            "elapsed_ms":receipt.elapsed_ms,"observed":receipt.observed}))
    }

    fn apply(&mut self, event: &Event) -> Result<()> {
        ensure!(
            self.publication.is_none()
                && (!self.failed || matches!(event, Event::WindowRebuildAdmitted { .. })),
            "enrich_ledger_terminal"
        );
        match event {
            Event::Bound {
                binding,
                plan_sha256,
            } => ensure!(
                self.sequence == 0 && *binding == self.binding && *plan_sha256 == self.plan_sha256,
                "enrich_binding_changed"
            ),
            _ if self.sequence == 0 => return Err(anyhow!("enrich_ledger_unbound")),
            Event::CallReserved { reservation } => {
                ensure!(
                    self.publication_prepared.is_none()
                        && reservation.call_id == self.reservations.len()
                        && self.reservations.len() == self.receipts.len(),
                    "enrich_call_reservation_invalid"
                );
                ensure!(
                    reservation.request_sha256.len() == 64 && reservation.request_bytes > 0,
                    "enrich_request_binding_invalid"
                );
                let count = self.attempt_count(reservation.kind);
                let limit = self.effective_call_limit(reservation.kind);
                ensure!(count < limit, "enrich_call_budget_exhausted");
                let expected = match reservation.kind {
                    CallKind::Builder => &self.binding.builder_model,
                    CallKind::Jev => &self.binding.jev_model,
                };
                ensure!(
                    reservation.requested_model == *expected,
                    "enrich_call_profile_changed"
                );
                self.reservations.push(reservation.clone());
                match reservation.kind {
                    CallKind::Builder => self.builder_calls += 1,
                    CallKind::Jev => self.jev_calls += 1,
                }
            }
            Event::CallFinished { receipt } => {
                ensure!(
                    receipt.call_id == self.receipts.len()
                        && self.reservations.len() == self.receipts.len() + 1,
                    "enrich_call_receipt_invalid"
                );
                self.receipts.push(receipt.clone());
            }
            Event::SupportRetryReserved {
                admission,
                reservation,
            } => {
                let expected = self
                    .support_retry(admission.failed_call_id)
                    .ok_or_else(|| anyhow!("enrich_support_retry_ineligible"))?;
                let prior = &self.reservations[admission.failed_call_id];
                ensure!(
                    serde_json::to_value(admission)? == serde_json::to_value(expected)?
                        && reservation.call_id == admission.retry_call_id
                        && reservation.kind == CallKind::Jev
                        && reservation.anchor_id == prior.anchor_id
                        && reservation.requested_model == prior.requested_model
                        && reservation.request_sha256 == prior.request_sha256
                        && reservation.request_bytes == prior.request_bytes,
                    "enrich_support_retry_request_changed"
                );
                self.support_retries.push(admission.clone());
                self.apply(&Event::CallReserved {
                    reservation: reservation.clone(),
                })?;
            }
            Event::WindowCompleted { window } => {
                let start = self.completed_call_count();
                let end = window
                    .jev_call_id
                    .map_or(Some(start + 1), |id| id.checked_add(1))
                    .ok_or_else(|| anyhow!("enrich_window_call_binding_invalid"))?;
                ensure!(
                    self.reservations.len() == end
                        && self.receipts.len() == end
                        && window.builder_call_id == start
                        && window.jev_call_id.is_none_or(|id| id > start)
                        && end <= start + 2 + self.binding.support_retries,
                    "enrich_window_call_binding_invalid"
                );
                for call in start..end {
                    ensure!(
                        self.reservations[call].anchor_id == window.anchor.anchor_id
                            && (if call == start || call + 1 == end {
                                self.receipts[call].error_code.is_none()
                                    && self.receipts[call].observed.is_some()
                            } else {
                                self.receipts[call].error_code.is_some()
                                    && self.support_retries.iter().any(|retry| {
                                        retry.failed_call_id == call
                                            && retry.retry_call_id == call + 1
                                    })
                            }),
                        "enrich_window_receipt_invalid"
                    );
                }
                ensure!(
                    self.reservations[start].kind == CallKind::Builder
                        && (start + 1..end).all(|id| self.reservations[id].kind == CallKind::Jev),
                    "enrich_window_call_kind_invalid"
                );
                self.windows.push(window.clone());
                self.committed_calls = end;
            }
            Event::Checkpoint { .. } => ensure!(
                self.reservations.len() == self.completed_call_count(),
                "enrich_checkpoint_incomplete_window"
            ),
            Event::Failed { .. } => self.failed = true,
            Event::WindowRebuildAdmitted { admission } => {
                let expected = self.rebuild_admission(admission.failed_call_id)?;
                ensure!(
                    serde_json::to_value(admission)? == serde_json::to_value(&expected)?,
                    "enrich_rebuild_admission_invalid"
                );
                self.committed_calls = self.reservations.len();
                self.failed = false;
                self.rebuilds.push(admission.clone());
            }
            Event::PublicationPrepared { artifact_sha256 } => {
                ensure!(
                    self.reservations.len() == self.completed_call_count()
                        && self.publication_prepared.is_none()
                        && artifact_sha256.len() == 64,
                    "enrich_publication_binding_invalid"
                );
                self.publication_prepared = Some(artifact_sha256.clone());
            }
            Event::Published { publication } => {
                ensure!(
                    self.publication_prepared.as_ref() == Some(&publication.artifact_sha256)
                        && publication.generation == self.binding.source.generation
                        && publication.manifest_sha256 == self.binding.source.manifest_sha256,
                    "enrich_publication_receipt_invalid"
                );
                self.publication = Some(publication.clone());
            }
        }
        Ok(())
    }

    pub fn capacity_for_window(&self) -> bool {
        // Reserve room for both calls, their receipts, the window and a terminal checkpoint.
        let retries = self
            .binding
            .support_retries
            .saturating_sub(self.support_retries.len());
        self.bytes + (MAX_RECORD_BYTES as u64 * (7 + 2 * retries) as u64)
            <= self.binding.max_ledger_bytes
    }

    pub fn append(&mut self, payload: Event) -> Result<()> {
        ensure!(!self.failed_write, "enrich_ledger_storage_failed");
        let sha256 = event_hash(self.sequence, &self.previous_sha256, &payload)?;
        let line = Line {
            sequence: self.sequence,
            previous_sha256: self.previous_sha256.clone(),
            payload,
            sha256: sha256.clone(),
        };
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES
                && self.bytes + bytes.len() as u64 <= self.binding.max_ledger_bytes,
            "enrich_ledger_limit"
        );
        // In-memory validation comes first; a failed write poisons this handle and resume
        // validates the whole durable chain. No provider call follows a failed reservation.
        self.apply(&line.payload)?;
        if self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_all())
            .is_err()
        {
            self.failed_write = true;
            return Err(anyhow!("enrich_ledger_storage_failed"));
        }
        self.sequence += 1;
        self.previous_sha256 = Some(sha256);
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    pub fn summary(&self, kind: CallKind) -> EnrichCallSummary {
        let calls: Vec<_> = self
            .reservations
            .iter()
            .filter(|call| call.kind == kind)
            .collect();
        let mut result = EnrichCallSummary {
            attempted_calls: calls.len(),
            ..Default::default()
        };
        let mut models = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for call in calls {
            let receipt = self.receipts.get(call.call_id);
            let observed = receipt.and_then(|receipt| receipt.observed.as_ref());
            if let Some(receipt) = receipt {
                if receipt.error_code.is_none() {
                    result.completed_calls += 1;
                } else {
                    result.failed_calls += 1;
                }
            }
            if observed.is_none() {
                result.unobserved_calls += 1;
            }
            let usage = observed
                .and_then(|value| value.usage.as_ref())
                .or_else(|| receipt.and_then(|receipt| receipt.partial_usage.as_ref()));
            if receipt.is_some_and(|receipt| receipt.partial_usage.is_some()) {
                result.partial_usage_calls += 1;
            }
            if usage.is_none() {
                result.missing_usage_calls += 1;
            }
            if let Some(observed) = observed {
                models.insert(observed.model.clone());
                // Never add the same actual native turn twice. Jev receipts without a
                // provider ID are still independent admitted logical calls.
                let identity = match kind {
                    CallKind::Builder => format!("{:?}:{:?}", observed.thread_id, observed.turn_id),
                    CallKind::Jev => observed
                        .response_id
                        .clone()
                        .unwrap_or_else(|| format!("call:{}", call.call_id)),
                };
                if !seen.insert(identity) {
                    continue;
                }
            }
            let total = usage.and_then(|usage| match kind {
                CallKind::Builder => usage["total"]["totalTokens"].as_u64(),
                CallKind::Jev => usage["total_tokens"].as_u64(),
            });
            match total {
                Some(total) if !result.total_tokens_overflowed => {
                    result.known_total_tokens =
                        result.known_total_tokens.unwrap_or(0).checked_add(total);
                    result.total_tokens_overflowed = result.known_total_tokens.is_none();
                }
                Some(_) => (),
                None => result.missing_total_tokens += 1,
            }
        }
        result.models = models.into_iter().collect();
        result
    }
}

fn event_hash(sequence: usize, previous: &Option<String>, payload: &Event) -> Result<String> {
    Ok(hash(&serde_json::to_vec(
        &json!({"sequence":sequence,"previous_sha256":previous,"payload":payload}),
    )?))
}

fn no_symlinks(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(!metadata.file_type().is_symlink(), "enrich_ledger_symlink"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(anyhow!("enrich_ledger_storage_failed")),
        }
    }
    Ok(())
}

pub(crate) fn builder_usage(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    let mut total = serde_json::Map::new();
    for field in [
        "totalTokens",
        "inputTokens",
        "cachedInputTokens",
        "cacheWriteInputTokens",
        "outputTokens",
        "reasoningOutputTokens",
    ] {
        if let Some(number) = value["total"][field].as_u64() {
            total.insert(field.into(), json!(number));
        }
    }
    (!total.is_empty()).then(|| json!({"total":total}))
}

pub(crate) fn jev_usage(value: &Value) -> Option<Value> {
    let mut usage = serde_json::Map::new();
    for field in [
        "input_tokens",
        "output_tokens",
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
    ] {
        if let Some(number) = value[field].as_u64() {
            usage.insert(field.into(), json!(number));
        }
    }
    if let Some(cost) = value["cost"]
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
    {
        usage.insert("cost".into(), json!(cost));
    }
    (!usage.is_empty()).then_some(Value::Object(usage))
}
