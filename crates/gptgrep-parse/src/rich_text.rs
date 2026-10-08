//! Identity UTF-8 preparation. Logical source units are not physical PDF pages.
use crate::artifact::{
    ArtifactBinding, json_sha256, read_bounded, sha256, write_bytes, write_json,
};
use crate::rich::{
    NativeCandidate, OcrMode, OcrPlan, PreparedDocument, RichParsedDocument, admit_staging,
};
use crate::{ArtifactKind, ResourceBudget, SegmentMap};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaintextMetadata {
    pub schema: String,
    pub coordinate_system: String,
    pub source_sha256: String,
    pub utf8_bytes: u64,
    pub lines: usize,
}
pub fn plaintext_preparation_profile_digest() -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&(
        "gptgrep.prepare.identity-utf8.v1",
        crate::LITEPARSE_REVISION,
    ))?))
}
pub fn plaintext_native_profile_digest() -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&(
        "gptgrep.native.source-utf8-byte-line.v1",
        plaintext_preparation_profile_digest()?,
    ))?))
}
fn final_profile(native: &str) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&(
        "gptgrep.finalize.source-utf8-byte-line.v1",
        native,
    ))?))
}
fn text(prepared: &PreparedDocument, budget: &ResourceBudget) -> Result<String> {
    // Includes source text, parser events, line starts, heading/tree nodes,
    // document projections and bounded JSON copies. This is owned allocation
    // admission, not a process/native RSS measurement.
    ensure!(
        prepared
            .summary
            .prepared_bytes
            .saturating_mul(256)
            .saturating_add(65_536)
            <= budget.owned_working_bytes,
        "resource_limit: plaintext working admission"
    );
    ensure!(
        prepared.summary.prepared_bytes <= budget.page_layout_bytes,
        "resource_limit: plaintext logical unit admission"
    );
    Ok(String::from_utf8(read_bounded(
        &prepared.prepared_path,
        prepared.summary.prepared_bytes,
        &prepared.summary.prepared_sha256,
    )?)?)
}
pub(crate) fn extract(
    prepared: PreparedDocument,
    budget: ResourceBudget,
) -> Result<NativeCandidate> {
    let text = text(&prepared, &budget)?;
    let profile = plaintext_native_profile_digest()?;
    let mut staged = prepared.summary.prepared_bytes;
    let mut artifacts = Vec::new();
    let mut metadata_used = 0;
    if !text.is_empty() {
        admit_staging(&mut staged, text.len() as u64, &budget)?;
        artifacts.push(write_bytes(
            &prepared.staging_root,
            "source-native.txt",
            ArtifactBinding {
                source: &prepared.summary.source_sha256,
                profile: &profile,
                kind: ArtifactKind::NativeText,
                page: Some(1),
            },
            text.as_bytes(),
            "text/plain; charset=utf-8",
            budget.page_layout_bytes,
        )?);
        let metadata = PlaintextMetadata {
            schema: "gptgrep.plaintext-metadata.v1".into(),
            coordinate_system:
                "original_utf8_bytes_and_lines; logical_source_unit_1; no_physical_pages".into(),
            source_sha256: prepared.summary.source_sha256.clone(),
            utf8_bytes: text.len() as u64,
            lines: text.lines().count().max(1),
        };
        metadata_used = crate::resource::encoded_size(&metadata, budget.page_layout_bytes)?;
        admit_staging(&mut staged, metadata_used, &budget)?;
        artifacts.push(write_json(
            &prepared.staging_root,
            "source-native.json",
            ArtifactBinding {
                source: &prepared.summary.source_sha256,
                profile: &profile,
                kind: ArtifactKind::NativeMetadata,
                page: Some(1),
            },
            &metadata,
            budget.page_layout_bytes,
        )?);
    }
    let mut candidate = NativeCandidate {
        summary: prepared.summary.clone(),
        pages: Vec::new(),
        artifacts,
        ocr_plan: OcrPlan {
            pages: Vec::new(),
            tasks: Vec::new(),
            blockers: Vec::new(),
        },
        native_profile_sha256: profile,
        native_owned_upper: prepared.summary.prepared_bytes * 256 + 65_536,
        metadata_admitted_upper: metadata_used,
        document_staging_used: staged,
        prepared,
        raw_pages: Vec::new(),
        execution_budget: budget,
        frozen_integrity_sha256: String::new(),
        annotation_counts: Vec::new(),
    };
    candidate.frozen_integrity_sha256 = candidate.integrity_digest()?;
    Ok(candidate)
}
pub(crate) fn verify_candidate(candidate: &NativeCandidate, budget: &ResourceBudget) -> Result<()> {
    ensure!(
        candidate.pages.is_empty()
            && candidate.raw_pages.is_empty()
            && candidate.annotation_counts.is_empty()
            && candidate.ocr_plan.pages.is_empty()
            && candidate.ocr_plan.tasks.is_empty()
            && candidate.ocr_plan.blockers.is_empty()
            && candidate.native_profile_sha256 == plaintext_native_profile_digest()?,
        "plaintext cannot claim physical pages/OCR/renditions"
    );
    let text = text(&candidate.prepared, budget)?;
    let expected = if text.is_empty() { 0 } else { 2 };
    ensure!(
        candidate.artifacts.len() == expected,
        "plaintext native artifact count mismatch"
    );
    for a in &candidate.artifacts {
        crate::artifact::verify_artifact(&candidate.prepared.staging_root, a)?;
        ensure!(
            a.page == Some(1)
                && a.profile_sha256 == candidate.native_profile_sha256
                && a.id
                    == crate::artifact_id(
                        &candidate.summary.source_sha256,
                        &a.profile_sha256,
                        &a.kind,
                        a.page,
                        a.region_id.as_deref(),
                        &a.sha256
                    )?,
            "plaintext native artifact binding mismatch"
        );
    }
    if !text.is_empty() {
        ensure!(
            candidate.artifacts[0].kind == ArtifactKind::NativeText
                && candidate.artifacts[0].sha256 == candidate.summary.source_sha256
                && candidate.artifacts[1].kind == ArtifactKind::NativeMetadata,
            "plaintext source text mismatch"
        );
    }
    Ok(())
}
pub(crate) fn finalize(
    mut candidate: NativeCandidate,
    _mode: OcrMode,
    budget: &ResourceBudget,
) -> Result<RichParsedDocument> {
    let text = text(&candidate.prepared, budget)?;
    let title = candidate
        .prepared
        .source_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let document = gptgrep_pageindex::from_text(&title, &text);
    let profile = final_profile(&candidate.native_profile_sha256)?;
    let layout = (&document.pages, &document.nodes);
    let layout_size = crate::resource::encoded_size(&layout, budget.document_metadata_bytes)?;
    ensure!(
        layout_size + candidate.metadata_admitted_upper <= budget.document_metadata_bytes,
        "resource_limit: plaintext document metadata"
    );
    admit_staging(&mut candidate.document_staging_used, layout_size, budget)?;
    candidate.artifacts.push(write_json(
        &candidate.prepared.staging_root,
        "source-layout.json",
        ArtifactBinding {
            source: &candidate.summary.source_sha256,
            profile: &profile,
            kind: ArtifactKind::Layout,
            page: None,
        },
        &layout,
        budget.document_metadata_bytes,
    )?);
    admit_staging(
        &mut candidate.document_staging_used,
        text.len() as u64,
        budget,
    )?;
    candidate.artifacts.push(write_bytes(
        &candidate.prepared.staging_root,
        "source-merged.txt",
        ArtifactBinding {
            source: &candidate.summary.source_sha256,
            profile: &profile,
            kind: ArtifactKind::MergedText,
            page: None,
        },
        text.as_bytes(),
        "text/plain; charset=utf-8",
        budget.document_metadata_bytes,
    )?);
    let segments = if text.is_empty() {
        Vec::new()
    } else {
        vec![SegmentMap {
            page: 1,
            merged_start: 0,
            merged_end: text.len() as u64,
            artifact_id: candidate.artifacts[0].id.clone(),
            artifact_start: 0,
            artifact_end: text.len() as u64,
            kind: ArtifactKind::NativeText,
        }]
    };
    candidate.prepared.verify(budget)?;
    // Account for serialized document/projection before returning the owned DTO.
    json_sha256(&document, budget.document_metadata_bytes)?;
    Ok(RichParsedDocument {
        summary: candidate.summary,
        document,
        pages: Vec::new(),
        artifacts: candidate.artifacts,
        segments,
        ocr_results: Vec::new(),
        ocr_tasks: Vec::new(),
        native_profile_sha256: candidate.native_profile_sha256,
        coverage: Vec::new(),
        unmapped_merged_lines: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NativeOptions, PreparationOptions, extract_native, finalize_document, prepare_document,
    };
    #[tokio::test]
    async fn source_utf8_crlf_markdown_and_empty_are_lossless_without_physical_pages() -> Result<()>
    {
        for (name, bytes) in [
            ("note.md", "# Heading\r\n中 é\r\n\r\n## Second\nLast"),
            ("note.txt", "\n\r\n"),
            ("README", ""),
        ] {
            let dir = tempfile::tempdir()?;
            let source = dir.path().join(name);
            std::fs::write(&source, bytes)?;
            let budget = ResourceBudget::default();
            let prepared = prepare_document(
                &source,
                &dir.path().join("stage"),
                &PreparationOptions,
                &budget,
            )
            .await?;
            assert_eq!(prepared.summary().format, "plaintext");
            assert_eq!(
                prepared.summary().source_sha256,
                prepared.summary().prepared_sha256
            );
            assert_eq!(prepared.prepared_path().extension().unwrap(), "txt");
            let native = extract_native(&prepared, &NativeOptions::default(), &budget).await?;
            assert!(native.pages().is_empty());
            assert!(native.plan().tasks.is_empty());
            let parsed = finalize_document(native, Vec::new(), OcrMode::Auto, &budget)?;
            assert_eq!(parsed.document.text.as_bytes(), bytes.as_bytes());
            assert_eq!(parsed.document.parser, "plaintext");
            assert_eq!(parsed.document.pages.len(), usize::from(!bytes.is_empty()));
            assert!(parsed.pages.is_empty() && parsed.coverage.is_empty());
            assert!(
                parsed
                    .artifacts
                    .iter()
                    .all(|a| a.kind != ArtifactKind::Rendition)
            );
            if !bytes.is_empty() {
                assert_eq!(
                    parsed.document.pages[0].line_end,
                    bytes.lines().count().max(1)
                );
                assert_eq!(parsed.segments[0].artifact_end, bytes.len() as u64);
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn invalid_utf8_source_change_and_small_working_cap_fail_closed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("note.txt");
        std::fs::write(&source, [255u8])?;
        let budget = ResourceBudget::default();
        let prepared = prepare_document(
            &source,
            &dir.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await?;
        assert!(
            extract_native(&prepared, &NativeOptions::default(), &budget)
                .await
                .is_err()
        );
        std::fs::write(&source, "old")?;
        let prepared = prepare_document(
            &source,
            &dir.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await?;
        std::fs::write(&source, "new")?;
        assert!(
            extract_native(&prepared, &NativeOptions::default(), &budget)
                .await
                .is_err()
        );
        let mut budget = ResourceBudget::default();
        budget.owned_working_bytes = 100;
        let prepared = prepare_document(
            &source,
            &dir.path().join("stage"),
            &PreparationOptions,
            &budget,
        )
        .await?;
        assert!(
            extract_native(&prepared, &NativeOptions::default(), &budget)
                .await
                .unwrap_err()
                .to_string()
                .contains("working admission")
        );
        Ok(())
    }
}
