//! Bounded private decision provenance. No source text or citation authority.
use super::*;
pub use gptgrep_jev::EvidenceRole;
use gptgrep_jev::{EvidenceRoleResponse, PreparedEvidenceRoles, evidence_role_contract};
use std::collections::BTreeMap;

pub const MAX_EVIDENCE_ROLE_METADATA_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateEligibility {
    OriginalLiteralAnchor,
    ScoreFloor,
    /// No judged candidate met the floor or is an original literal anchor, so
    /// this one is among the highest-scoring few delivered as a flagged
    /// `below_threshold` low-confidence candidate; it never passed the floor.
    BelowFloorFallback,
    NotEligible,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDisposition {
    JudgmentUnavailable,
    JudgedAwaitingSelection,
    ExcludedRelevanceFloor,
    EligibleBelowResultLimit,
    StaleBeforeDelivery,
    AwaitingDelivery,
    SelectedRemovedByEnvelope,
    Delivered,
    DeliveredScopeChangedUnassessed,
    DeliveryFailed,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveredSetSufficiency {
    Unassessed,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceRoleSource {
    pub path: String,
    pub source_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceRoleSpan {
    pub byte_start: usize,
    pub byte_end: usize,
    pub text_sha256: String,
}
impl EvidenceRoleSpan {
    fn from_hit(hit: &Hit) -> Self {
        Self {
            byte_start: hit.byte_start,
            byte_end: hit.byte_end,
            text_sha256: digest(hit.text.as_bytes()),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRoleCandidateDecision {
    pub candidate_id: String,
    pub source_index: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text_sha256: String,
    pub literal_anchor: bool,
    pub relevance_score: Option<f64>,
    pub relevance_confidence: Option<f64>,
    pub role: Option<EvidenceRole>,
    pub role_confidence: Option<f64>,
    pub role_probabilities: Option<BTreeMap<EvidenceRole, f64>>,
    pub eligibility: Option<CandidateEligibility>,
    /// One-based rank after eligibility and declared policy ordering.
    pub selection_rank: Option<usize>,
    pub disposition: CandidateDisposition,
    pub delivered_span: Option<EvidenceRoleSpan>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRoleHint {
    pub role: EvidenceRole,
    pub confidence: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRoleDiagnostics {
    pub strategy: String,
    pub generation: String,
    pub original_question_sha256: String,
    pub decision_contract_sha256: String,
    pub request_sha256: String,
    pub request_bytes: usize,
    pub question_count: usize,
    pub score_question_count: usize,
    pub choice_question_count: usize,
    /// Conservative bound including every optional result/delivery field.
    pub reserved_metadata_bytes: usize,
    pub delivered_set_sufficiency: DeliveredSetSufficiency,
    pub delivery_packet_sha256: Option<String>,
    pub sources: Vec<EvidenceRoleSource>,
    pub candidates: Vec<EvidenceRoleCandidateDecision>,
}

impl EvidenceRoleDiagnostics {
    pub(super) fn prepare(
        generation: &str,
        query: &str,
        hits: &[Hit],
        client: &JevClient,
    ) -> Result<(PreparedEvidenceRoles, Self)> {
        let candidates: Vec<_> = hits
            .iter()
            .enumerate()
            .map(|(i, hit)| Candidate {
                id: format!("c{i}"),
                text: format!(
                    "Document: {}\nSection: {}\nEvidence: {}",
                    hit.path, hit.title, hit.text
                ),
            })
            .collect();
        let prepared = client.prepare_evidence_roles(query, &candidates)?;
        let mut sources = Vec::new();
        let mut decisions = Vec::new();
        for (index, hit) in hits.iter().enumerate() {
            let source = EvidenceRoleSource {
                path: hit.path.clone(),
                source_sha256: hit.source_sha256.clone(),
            };
            let source_index = sources
                .iter()
                .position(|item| item == &source)
                .unwrap_or_else(|| {
                    sources.push(source);
                    sources.len() - 1
                });
            decisions.push(EvidenceRoleCandidateDecision {
                candidate_id: format!("c{index}"),
                source_index,
                byte_start: hit.byte_start,
                byte_end: hit.byte_end,
                text_sha256: digest(hit.text.as_bytes()),
                literal_anchor: hit.literal_anchor,
                relevance_score: None,
                relevance_confidence: None,
                role: None,
                role_confidence: None,
                role_probabilities: None,
                eligibility: None,
                selection_rank: None,
                disposition: CandidateDisposition::JudgmentUnavailable,
                delivered_span: None,
            });
        }
        let mut metadata = Self {
            strategy: "jev-evidence-role-v1".into(),
            generation: generation.into(),
            original_question_sha256: digest(query.as_bytes()),
            decision_contract_sha256: digest(&serde_json::to_vec(&evidence_role_contract())?),
            request_sha256: digest(prepared.encoded_request()),
            request_bytes: prepared.request_bytes(),
            question_count: prepared.question_count(),
            score_question_count: hits.len(),
            choice_question_count: hits.len(),
            reserved_metadata_bytes: MAX_EVIDENCE_ROLE_METADATA_BYTES,
            delivered_set_sufficiency: DeliveredSetSufficiency::Unassessed,
            delivery_packet_sha256: None,
            sources,
            candidates: decisions,
        };
        // Reserve before any provider admission. Use every optional field and a
        // 32-byte allowance per bounded float (longer than finite f64 JSON).
        // Source strings/identities never grow; delivered offsets are contained.
        let mut worst = metadata.clone();
        worst.delivery_packet_sha256 = Some("f".repeat(64));
        for candidate in &mut worst.candidates {
            candidate.relevance_score = Some(f64::MIN_POSITIVE);
            candidate.relevance_confidence = Some(f64::MIN_POSITIVE);
            candidate.role = Some(EvidenceRole::SourceLocalIncomplete);
            candidate.role_confidence = Some(f64::MIN_POSITIVE);
            candidate.role_probabilities = Some(
                EvidenceRole::ALL
                    .into_iter()
                    .map(|role| (role, f64::MIN_POSITIVE))
                    .collect(),
            );
            candidate.eligibility = Some(CandidateEligibility::OriginalLiteralAnchor);
            candidate.selection_rank = Some(usize::MAX);
            candidate.disposition = CandidateDisposition::DeliveredScopeChangedUnassessed;
            candidate.delivered_span = Some(EvidenceRoleSpan {
                byte_start: usize::MAX,
                byte_end: usize::MAX,
                text_sha256: "f".repeat(64),
            });
        }
        let float_padding = 32usize.saturating_sub(serde_json::to_vec(&f64::MIN_POSITIVE)?.len());
        let reservation = serde_json::to_vec(&worst)?.len() + hits.len() * 7 * float_padding;
        ensure!(
            reservation <= MAX_EVIDENCE_ROLE_METADATA_BYTES,
            "evidence role metadata exceeded the byte limit before provider admission"
        );
        metadata.reserved_metadata_bytes = reservation;
        metadata.validate_bound()?;
        Ok((prepared, metadata))
    }

    pub fn validate_bound(&self) -> Result<()> {
        ensure!(
            self.delivery_packet_sha256.as_ref().is_none_or(
                |value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            ),
            "evidence role delivery packet digest is invalid"
        );
        ensure!(
            self.candidates.len() <= 24 && self.sources.len() <= 24,
            "evidence role metadata candidate limit exceeded"
        );
        ensure!(
            self.candidates
                .iter()
                .all(|candidate| candidate.source_index < self.sources.len()),
            "evidence role metadata source identity is invalid"
        );
        let bytes = serde_json::to_vec(self)?.len();
        ensure!(
            bytes <= self.reserved_metadata_bytes
                && self.reserved_metadata_bytes <= MAX_EVIDENCE_ROLE_METADATA_BYTES,
            "evidence role metadata exceeded its reserved byte limit"
        );
        Ok(())
    }

    fn matches(&self, candidate: &EvidenceRoleCandidateDecision, hit: &Hit) -> bool {
        self.sources
            .get(candidate.source_index)
            .is_some_and(|source| {
                source.path == hit.path && source.source_sha256 == hit.source_sha256
            })
            && candidate.byte_start == hit.byte_start
            && candidate.byte_end == hit.byte_end
            && candidate.text_sha256 == digest(hit.text.as_bytes())
    }
    fn index_of(&self, hit: &Hit) -> Result<usize> {
        self.candidates
            .iter()
            .position(|candidate| self.matches(candidate, hit))
            .context("evidence role assessment span identity mismatch")
    }

    /// Caller must also preserve explicit original-to-delivered association: a
    /// clipped span must not borrow an independently assessed overlapping span.
    pub fn hint_for_hit(&self, generation: &str, hit: &Hit) -> Option<EvidenceRoleHint> {
        if generation != self.generation {
            return None;
        }
        self.candidates
            .iter()
            .find(|candidate| self.matches(candidate, hit))
            .and_then(|candidate| {
                matches!(
                    candidate.disposition,
                    CandidateDisposition::AwaitingDelivery | CandidateDisposition::Delivered
                )
                .then_some(EvidenceRoleHint {
                    role: candidate.role?,
                    confidence: candidate.role_confidence,
                })
            })
    }

    /// Association-safe hint: clipping cannot borrow another candidate's role
    /// even when the shorter window exactly equals that other candidate.
    pub fn hint_for_delivery(
        &self,
        generation: &str,
        assessed: &Hit,
        delivered: &Hit,
    ) -> Option<EvidenceRoleHint> {
        let index = self.index_of(assessed).ok()?;
        if !self.matches(&self.candidates[index], delivered) {
            return None;
        }
        self.hint_for_hit(generation, assessed)
    }

    /// Update only the explicitly associated assessed candidate. This creates no
    /// citation authority; the host still owns envelope, issue, and ledger gates.
    pub fn record_delivery(&mut self, assessed: &Hit, delivered: Option<&Hit>) -> Result<()> {
        let index = self.index_of(assessed)?;
        ensure!(
            matches!(
                self.candidates[index].disposition,
                CandidateDisposition::AwaitingDelivery
                    | CandidateDisposition::Delivered
                    | CandidateDisposition::DeliveredScopeChangedUnassessed
                    | CandidateDisposition::SelectedRemovedByEnvelope
            ),
            "evidence role candidate was not selected for delivery"
        );
        let (disposition, span) = if let Some(hit) = delivered {
            ensure!(
                hit.path == assessed.path
                    && hit.source_sha256 == assessed.source_sha256
                    && hit.byte_start >= assessed.byte_start
                    && hit.byte_end <= assessed.byte_end
                    && hit.byte_start <= hit.byte_end,
                "evidence role delivered span identity mismatch"
            );
            let start = hit.byte_start - assessed.byte_start;
            let end = hit.byte_end - assessed.byte_start;
            ensure!(
                assessed.text.get(start..end) == Some(hit.text.as_str()),
                "evidence role delivered text mismatch"
            );
            let span = EvidenceRoleSpan::from_hit(hit);
            (
                if self.matches(&self.candidates[index], hit) {
                    CandidateDisposition::Delivered
                } else {
                    CandidateDisposition::DeliveredScopeChangedUnassessed
                },
                Some(span),
            )
        } else {
            (CandidateDisposition::SelectedRemovedByEnvelope, None)
        };
        self.candidates[index].disposition = disposition;
        self.candidates[index].delivered_span = span;
        self.validate_bound()
    }

    pub fn mark_delivery_failure(&mut self) {
        self.delivery_packet_sha256 = None;
        for candidate in &mut self.candidates {
            if matches!(
                candidate.disposition,
                CandidateDisposition::JudgedAwaitingSelection
                    | CandidateDisposition::AwaitingDelivery
                    | CandidateDisposition::Delivered
                    | CandidateDisposition::DeliveredScopeChangedUnassessed
            ) {
                candidate.disposition = CandidateDisposition::DeliveryFailed;
                candidate.delivered_span = None;
            }
        }
    }

    pub(super) fn observe(&mut self, response: &EvidenceRoleResponse) -> Result<()> {
        ensure!(
            response.candidates.len() == self.candidates.len(),
            "evidence role result identity mismatch"
        );
        for (candidate, answer) in self.candidates.iter_mut().zip(&response.candidates) {
            ensure!(
                candidate.candidate_id == answer.relevance.id,
                "evidence role result identity mismatch"
            );
            candidate.relevance_score = Some(answer.relevance.score);
            candidate.relevance_confidence = answer.relevance.confidence;
            candidate.role = Some(answer.role);
            candidate.role_confidence = answer.role_confidence;
            candidate.role_probabilities = answer.role_probabilities.clone();
            candidate.disposition = CandidateDisposition::JudgedAwaitingSelection;
        }
        self.validate_bound()
    }

    pub(super) fn order(
        &mut self,
        batch: &mut CandidateBatch,
        options: &SearchOptions,
    ) -> Result<()> {
        for hit in &mut batch.hits {
            let index = self.index_of(hit)?;
            let candidate = &mut self.candidates[index];
            hit.score = candidate
                .relevance_score
                .context("evidence role relevance is unavailable")?;
            hit.confidence = candidate.relevance_confidence;
            hit.below_threshold = hit.score < options.min_score;
            candidate.eligibility = Some(if hit.literal_anchor {
                CandidateEligibility::OriginalLiteralAnchor
            } else if hit.score >= options.min_score {
                CandidateEligibility::ScoreFloor
            } else {
                CandidateEligibility::NotEligible
            });
            candidate.disposition =
                if candidate.eligibility == Some(CandidateEligibility::NotEligible) {
                    CandidateDisposition::ExcludedRelevanceFloor
                } else {
                    CandidateDisposition::AwaitingDelivery
                };
        }
        batch.coverage.retained_literal_anchors = batch
            .hits
            .iter()
            .filter(|hit| hit.literal_anchor && hit.score < options.min_score)
            .count();
        let before = batch.hits.len();
        let passing = batch
            .hits
            .iter()
            .filter(|hit| hit.literal_anchor || hit.score >= options.min_score)
            .count();
        // Never deliver an empty seed when judged candidates exist: as in
        // ordinary relevance scoring, the highest-scoring few are delivered as
        // flagged low-confidence candidates. They never count as passing.
        let mut fallback = Vec::new();
        if passing == 0 {
            let mut ranked: Vec<&Hit> = batch.hits.iter().collect();
            ranked.sort_by(|left, right| {
                right
                    .score
                    .total_cmp(&left.score)
                    .then(left.path.cmp(&right.path))
                    .then(left.byte_start.cmp(&right.byte_start))
                    .then(left.byte_end.cmp(&right.byte_end))
            });
            for hit in ranked.into_iter().take(BELOW_THRESHOLD_FALLBACK) {
                fallback.push(self.index_of(hit)?);
            }
            for &index in &fallback {
                self.candidates[index].eligibility = Some(CandidateEligibility::BelowFloorFallback);
                self.candidates[index].disposition = CandidateDisposition::AwaitingDelivery;
            }
        }
        let mut keep = Vec::with_capacity(batch.hits.len());
        for hit in &batch.hits {
            keep.push(
                hit.literal_anchor
                    || hit.score >= options.min_score
                    || fallback.contains(&self.index_of(hit)?),
            );
        }
        let mut kept = keep.into_iter();
        batch
            .hits
            .retain(|_| kept.next().expect("one decision per candidate"));
        batch.coverage.filtered_candidates = before - passing;
        // Recounted against the hits actually delivered by finish_candidate_batch.
        batch.coverage.below_threshold_fallback = fallback.len();
        // Exact identity is already verified above. No probability multiplication
        // or new floor: category priority only inside the protected anchor lane.
        batch.hits.sort_by(|left, right| {
            let a = &self.candidates[self.index_of(left).expect("verified candidate")];
            let b = &self.candidates[self.index_of(right).expect("verified candidate")];
            right
                .literal_anchor
                .cmp(&left.literal_anchor)
                .then(a.role.cmp(&b.role))
                .then(right.score.total_cmp(&left.score))
                .then(left.path.cmp(&right.path))
                .then(left.byte_start.cmp(&right.byte_start))
                .then(left.byte_end.cmp(&right.byte_end))
        });
        for (index, hit) in batch.hits.iter().enumerate() {
            let position = self.index_of(hit)?;
            self.candidates[position].selection_rank = Some(index + 1);
            if index >= options.limit {
                self.candidates[position].disposition =
                    CandidateDisposition::EligibleBelowResultLimit;
            }
        }
        self.validate_bound()
    }

    pub(super) fn finish_freshness(&mut self, hits: &[Hit]) -> Result<()> {
        let stale: Vec<_> = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.disposition == CandidateDisposition::AwaitingDelivery
                    && !hits.iter().any(|hit| self.matches(candidate, hit))
            })
            .map(|(index, _)| index)
            .collect();
        for index in stale {
            self.candidates[index].disposition = CandidateDisposition::StaleBeforeDelivery;
        }
        self.validate_bound()
    }
}

#[cfg(test)]
mod tests;
