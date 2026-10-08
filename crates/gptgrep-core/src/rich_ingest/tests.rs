//! Genuine native parsing with self-authored fixtures and an explicitly declared
//! offline executor. DTO validation/publication tests do not qualify OCR accuracy.
use super::*;
use gptgrep_parse::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn pdf(mixed: bool) -> Vec<u8> {
    let body=format!("BT /F1 8 Tf 20 180 Td {} ET","(Ordinary native text provides enough printable content for faithful extraction.) Tj 0 -12 Td ".repeat(12));
    let first = if mixed {
        "0 0 1 rg 20 20 80 80 re f BT /F1 8 Tf 20 180 Td (Caption) Tj ET".into()
    } else {
        body.clone()
    };
    let objects=["<< /Type /Catalog /Pages 2 0 R >>".to_owned(),"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".into(),format!("<< /Length {} >>\nstream\n{first}\nendstream",first.len()),"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".into(),format!("<< /Length {} >>\nstream\n{body}\nendstream",body.len()),"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()];
    let mut pdf = "%PDF-1.4\n".to_owned();
    let mut offsets = vec![0];
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{obj}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()));
    for off in offsets.iter().skip(1) {
        pdf.push_str(&format!("{off:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len()
    ));
    pdf.into_bytes()
}
fn png(path: &Path, color: u8) -> Result<()> {
    use image::{ExtendedColorType, ImageEncoder};
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        &vec![color; 64 * 48 * 3],
        64,
        48,
        ExtendedColorType::Rgb8,
    )?;
    fs::write(path, bytes)?;
    Ok(())
}
fn options(id: &str) -> RichIngestOptions {
    RichIngestOptions {
        mode: OcrMode::Auto,
        runtime_profile_sha256: Some("3".repeat(64)),
        run_id: Some(id.into()),
        ..Default::default()
    }
}
fn result(task: &OcrTask) -> Result<OcrPageResult> {
    Ok(OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: task
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| OcrRegionResult {
                id: r.id.clone(),
                bbox: r.bbox,
                text: "Declared offline transcription; no model accuracy qualification".into(),
                reading_order: i as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(i, image)| OcrImageDelivery {
                    artifact_id: image.artifact.id.clone(),
                    artifact_sha256: image.artifact.sha256.clone(),
                    tool_ordinal: i as u32 + 1,
                    mime: image.artifact.mime.clone(),
                    width: image.width,
                    height: image.height,
                    tool_response_sha256: "4".repeat(64),
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    })
}
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    mode: &'static str,
    change: Option<PathBuf>,
}
impl Mock {
    fn new(mode: &'static str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            mode,
            change: None,
        }
    }
}
impl OcrExecutor for Mock {
    fn execute<'a>(
        &'a self,
        task: OcrTask,
    ) -> Pin<Box<dyn Future<Output = Result<OcrPageResult>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.mode == "error" {
                anyhow::bail!("MOCK-private-provider-error-never-retain")
            }
            let mut answer = result(&task)?;
            if self.mode == "partial" {
                answer.status = OcrResultStatus::Partial;
                answer.regions[0].status = OcrRegionStatus::Partial;
            }
            if self.mode == "profile" {
                answer.receipt.runtime_profile_sha256 = "5".repeat(64);
            }
            if let Some(path) = &self.change {
                let mut bytes = fs::read(path)?;
                bytes.extend_from_slice(b"\n% source changed\n");
                fs::write(path, bytes)?;
            }
            Ok(answer)
        })
    }
}

#[tokio::test]
async fn genuine_mixed_pdf_plan_publish_and_v2_evidence_are_composed() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    fs::write(root.join("mixed.pdf"), pdf(true))?;
    let planned = prepare_rich_ingest(&root, options("mixed")).await?;
    assert_eq!(planned.report().documents[0].pages.len(), 2);
    assert_eq!(planned.report().required_pages, 1);
    assert_eq!(planned.report().model_calls_in_plan, 0);
    assert!(planned.needs_ocr_executor());
    let roots = planned.ocr_staging_roots();
    for task in planned.tasks.values() {
        for image in &task.images {
            assert_eq!(
                roots[&task.task_sha256].join(&image.artifact.relative_path),
                image.path
            );
        }
    }
    assert!(!root.join(STATE).join("CURRENT.json").exists());
    let mock = Mock::new("complete");
    let report = planned.publish(Some(&mock)).await;
    assert_eq!(report.status, "published", "{report:?}");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let reader = crate::RichGenerationReader::open(&root)?;
    reader.verify_all()?;
    let manifest = reader.manifest()?;
    assert_eq!(manifest.documents.len(), 1);
    assert_eq!(manifest.documents[0].pages.len(), 2);
    assert!(
        manifest.documents[0]
            .pages
            .iter()
            .any(|p| p.coverage == CoverageState::ValidatedOcr)
    );
    let hits = reader.search_artifacts("Declared offline", false, true, 1, None, 1_048_576)?;
    assert!(!hits.hits.is_empty());
    for hit in &hits.hits {
        reader.validate_text_evidence(&hit.evidence)?;
    }
    Ok(())
}
#[tokio::test]
async fn genuine_images_execute_in_two_slots_and_verified_resume_never_calls_executor() -> Result<()>
{
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    for (i, color) in [72, 128, 200].iter().enumerate() {
        png(&root.join(format!("scan{i}.png")), *color)?;
    }
    let mock = Mock::new("complete");
    let report = prepare_rich_ingest(&root, options("images"))
        .await?
        .publish(Some(&mock))
        .await;
    assert_eq!(report.status, "published", "{report:?}");
    assert_eq!(report.new_ocr_executions, 3);
    assert_eq!(mock.peak.load(Ordering::SeqCst), 2);
    let mut opt = options("images");
    opt.resume = true;
    let resumed = prepare_rich_ingest(&root, opt).await?;
    assert!(!resumed.needs_ocr_executor());
    let resumed = resumed.publish(None).await;
    assert_eq!(resumed.status, "published", "{resumed:?}");
    assert_eq!(resumed.reused_verified_results, 3);
    assert_eq!(resumed.new_ocr_executions, 0);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 3);
    let reader = crate::RichGenerationReader::open(&root)?;
    for doc in &reader.manifest()?.documents {
        assert!(matches!(&doc.prepared.conversion,FeatureState::Present(x) if x.invocations==1));
    }
    Ok(())
}
#[tokio::test]
async fn required_failure_partial_profile_and_source_drift_preserve_previous_current() -> Result<()>
{
    for mode in ["error", "partial", "profile", "drift"] {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().canonicalize()?;
        fs::write(root.join("native.pdf"), pdf(false))?;
        let baseline = prepare_rich_ingest(&root, RichIngestOptions::default())
            .await?
            .publish(None)
            .await;
        assert_eq!(baseline.status, "published", "{baseline:?}");
        let before = fs::read(root.join(STATE).join("CURRENT.json"))?;
        png(&root.join("scan.png"), 128)?;
        let mut mock = Mock::new(mode);
        if mode == "drift" {
            mock.change = Some(root.join("native.pdf"));
        }
        let report = prepare_rich_ingest(&root, options(mode))
            .await?
            .publish(Some(&mock))
            .await;
        assert_eq!(report.status, "failed_unpublished", "{mode}: {report:?}");
        assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, before);
        assert!(!serde_json::to_string(&report)?.contains("MOCK-private"));
        if mode != "drift" {
            let mut opt = options(mode);
            opt.resume = true;
            let resumed = prepare_rich_ingest(&root, opt)
                .await?
                .publish(Some(&mock))
                .await;
            assert_eq!(resumed.status, "failed_unpublished");
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
            assert_eq!(resumed.new_ocr_executions, 0);
        } else {
            fs::write(root.join("native.pdf"), pdf(false))?;
            let mut opt = options(mode);
            opt.resume = true;
            let resumed = prepare_rich_ingest(&root, opt).await?.publish(None).await;
            assert_eq!(resumed.status, "published", "{resumed:?}");
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
            assert_eq!(resumed.new_ocr_executions, 0);
            assert_eq!(resumed.reused_verified_results, 1);
        }
    }
    Ok(())
}
#[tokio::test]
async fn off_and_office_blockers_precede_any_executor_call_or_publication() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    png(&root.join("scan.png"), 128)?;
    let off = prepare_rich_ingest(&root, RichIngestOptions::default()).await?;
    assert_eq!(off.report().status, "blocked_unpublished");
    let mock = Mock::new("complete");
    assert_eq!(off.publish(Some(&mock)).await.status, "failed_unpublished");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    fs::write(
        root.join("unsupported.docx"),
        b"explicit unsupported format",
    )?;
    let plan = prepare_rich_ingest(&root, options("blocked")).await?;
    assert!(
        plan.report()
            .blockers
            .iter()
            .any(|b| b.contains("Office/other formats"))
    );
    assert_eq!(plan.publish(Some(&mock)).await.new_ocr_executions, 0);
    assert!(!root.join(STATE).join("CURRENT.json").exists());
    Ok(())
}
#[tokio::test]
async fn resume_rejects_changed_result_or_plan_before_a_new_execution() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    png(&root.join("scan.png"), 128)?;
    let mock = Mock::new("complete");
    let prepared = prepare_rich_ingest(&root, options("pinned")).await?;
    let state = prepared.state_dir()?;
    let hash = prepared.tasks.keys().next().context("OCR task")?.clone();
    assert_eq!(prepared.publish(Some(&mock)).await.status, "published");
    let pointer = fs::read(root.join(STATE).join("CURRENT.json"))?;
    fs::write(state.join("results").join(format!("{hash}.json")), b"{}")?;
    let mut opt = options("pinned");
    opt.resume = true;
    assert_eq!(
        prepare_rich_ingest(&root, opt.clone())
            .await?
            .publish(Some(&mock))
            .await
            .status,
        "failed_unpublished"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read(root.join(STATE).join("CURRENT.json"))?, pointer);
    png(&root.join("scan.png"), 200)?;
    assert!(prepare_rich_ingest(&root, opt).await.is_err());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn finite_whole_plan_admission_and_source_inventory_gate_before_execution() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    png(&root.join("a.png"), 128)?;
    png(&root.join("b.png"), 129)?;
    let mock = Mock::new("complete");
    let mut opt = options("limited");
    opt.max_ocr_pages = 1;
    let plan = prepare_rich_ingest(&root, opt).await?;
    assert_eq!(plan.report().status, "blocked_unpublished");
    assert_eq!(plan.publish(Some(&mock)).await.new_ocr_executions, 0);
    let plan = prepare_rich_ingest(&root, options("changed")).await?;
    png(&root.join("c.png"), 130)?;
    let report = plan.publish(Some(&mock)).await;
    assert_eq!(report.status, "failed_unpublished");
    assert_eq!(report.new_ocr_executions, 0);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert!(!root.join(STATE).join("CURRENT.json").exists());
    let mut opt = options("memory");
    opt.resources.owned_working_bytes = 1;
    assert!(prepare_rich_ingest(&root, opt).await.is_err());
    let mut opt = options("disk");
    opt.resources.run_staging_bytes = 1;
    assert!(prepare_rich_ingest(&root, opt).await.is_err());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    Ok(())
}
#[cfg(unix)]
#[test]
fn runtime_descriptor_rejects_nonregular_symlinks_oversize_and_credential_names() -> Result<()> {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    let valid = root.join("runtime.json");
    fs::write(&valid, b"{}")?;
    assert_eq!(read_rich_runtime_descriptor(&valid)?, b"{}");
    let leaf = root.join("linked.json");
    symlink(&valid, &leaf)?;
    assert!(read_rich_runtime_descriptor(&leaf).is_err());
    let alias = root.join("alias");
    symlink(&root, &alias)?;
    assert!(read_rich_runtime_descriptor(&alias.join("runtime.json")).is_err());
    fs::write(root.join("large.json"), vec![b' '; 16385])?;
    assert!(read_rich_runtime_descriptor(&root.join("large.json")).is_err());
    let fifo = root.join("fifo.json");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())?;
    // SAFETY: owned temporary pathname, no process or external pipe activity.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(read_rich_runtime_descriptor(&fifo).is_err());
    for name in ["auth.json", ".env", "private.pem", "private.key"] {
        // The filename gate rejects before any credential input could be opened.
        assert!(read_rich_runtime_descriptor(&root.join(name)).is_err());
    }
    Ok(())
}
