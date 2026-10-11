use super::*;
use crate::{RichGenerationWriter, digest};
use gptgrep_jev::ExecutionContext;
use gptgrep_pageindex::{PageInput, from_pages};
use gptgrep_parse::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

// Copied ordinary synthetic DTO custody fixture. This is not native/OCR accuracy.
fn candidate(root: &Path, texts: &[&str]) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = b"synthetic source bytes for artifact custody tests";
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = digest(source);
    let profile = digest(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create =
        |kind: ArtifactKind, page: Option<u32>, bytes: &[u8], name: &str| -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &digest(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: digest(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".txt") {
                    "text/plain; charset=utf-8"
                } else {
                    "application/json"
                }
                .into(),
                page,
                region_id: None,
            };
            fs::write(staging_path.join(name), bytes)?;
            artifacts.push(artifact.clone());
            Ok(artifact)
        };
    for (index, text) in texts.iter().enumerate() {
        let number = index as u32 + 1;
        let native_text = create(
            ArtifactKind::NativeText,
            Some(number),
            text.as_bytes(),
            &format!("p{number}-native.txt"),
        )?;
        let geometry = PageGeometry {
            width_points: 100.0,
            height_points: 200.0,
            rotation_quarter_turns: 0,
            user_unit: 1.0,
            source_to_page: AffineTransform::IDENTITY,
            page_to_source: AffineTransform::IDENTITY,
        };
        let complexity = serde_json::from_value(
            json!({"page_number":number,"text_length":text.len(),"text_coverage":0.5,
            "has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,
            "full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":20000.0,
            "needs_ocr":false,"reasons":[],"layout":null}),
        )?;
        let capabilities =
            BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
        let metadata_artifact = create(
            ArtifactKind::NativeMetadata,
            Some(number),
            &serde_json::to_vec(
                &json!({"page":{"page_number":number,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
                "capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]}),
            )?,
            &format!("p{number}-native.json"),
        )?;
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![],
            complexity,
            coverage: if text.is_empty() {
                CoverageState::BlankVerified
            } else {
                CoverageState::Native
            },
            capabilities,
        });
    }
    let inputs = texts
        .iter()
        .enumerate()
        .map(|(i, text)| PageInput {
            number: i as u32 + 1,
            text: (*text).into(),
        })
        .collect::<Vec<_>>();
    let document = from_pages("synthetic", &inputs, &[], &[], RICH_PARSER)?;
    create(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
    )?;
    create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
    let unmapped_merged_lines = document
        .text
        .split_inclusive('\n')
        .filter(|line| !line.trim_end_matches('\n').is_empty())
        .count();
    Ok((
        staging,
        RichParsedDocument {
            summary: PreparedSummary {
                source_sha256: sha.clone(),
                prepared_sha256: sha,
                source_bytes: source.len() as u64,
                prepared_bytes: source.len() as u64,
                format: "pdf".into(),
                parser_identity: RICH_PARSER.into(),
                preparation_profile_sha256: digest(&serde_json::to_vec(&(
                    "gptgrep.prepare.identity-pdf.v1",
                    gptgrep_parse::LITEPARSE_REVISION,
                ))?),
                conversion: FeatureState::Present(gptgrep_parse::ConversionIdentity {
                    converter: "identity-pdf".into(),
                    version: gptgrep_parse::LITEPARSE_REVISION.into(),
                    invocations: 0,
                }),
                image_geometry: FeatureState::Absent,
            },
            document,
            pages,
            artifacts,
            native_profile_sha256: profile,
            segments: vec![],
            ocr_tasks: vec![],
            ocr_results: vec![],
            coverage: texts
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    (
                        i as u32 + 1,
                        if text.is_empty() {
                            CoverageState::BlankVerified
                        } else {
                            CoverageState::Native
                        },
                    )
                })
                .collect(),
            unmapped_merged_lines,
        },
    ))
}

fn publish(root: &Path, texts: &[&str]) -> Result<GenerationPin> {
    let (staging, parsed) = candidate(root, texts)?;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()
}

fn context(limit: u32) -> Result<ExecutionContext> {
    ExecutionContext::new(
        "ordinary-rich-text-fixture",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(60),
        limit,
    )
}
struct MockRequest {
    body: Vec<u8>,
    parsed: Value,
}
impl std::ops::Deref for MockRequest {
    type Target = Value;
    fn deref(&self) -> &Self::Target {
        &self.parsed
    }
}
fn fixture_artifact(
    parsed: &mut RichParsedDocument,
    staging: &Path,
    kind: ArtifactKind,
    page: Option<u32>,
    bytes: &[u8],
    name: &str,
    mime: &str,
) -> Result<ArtifactRef> {
    let artifact = ArtifactRef {
        id: artifact_id(
            &parsed.summary.source_sha256,
            &parsed.native_profile_sha256,
            &kind,
            page,
            None,
            &digest(bytes),
        )?,
        kind,
        relative_path: name.into(),
        sha256: digest(bytes),
        profile_sha256: parsed.native_profile_sha256.clone(),
        bytes: bytes.len() as u64,
        mime: mime.into(),
        page,
        region_id: None,
    };
    fs::write(staging.join(name), bytes)?;
    parsed.artifacts.push(artifact.clone());
    Ok(artifact)
}
pub(super) fn publish_saved_ocr(root: &Path) -> Result<GenerationPin> {
    let (staging, mut parsed) = candidate(root, &["Native description\n"])?;
    let staging = staging.path().canonicalize()?;
    // Synthetic source/receipt DTO only: no OCR execution or image qualification.
    let mut png = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut png),
        &[255u8; 24],
        2,
        4,
        image::ColorType::Rgb8,
        image::ImageFormat::Png,
    )?;
    let rendition = fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::Rendition,
        Some(1),
        &png,
        "page.png",
        "image/png",
    )?;
    let image = OcrImage {
        artifact: rendition,
        path: staging.join("page.png"),
        width: 2,
        height: 4,
        page_to_raster: AffineTransform {
            a: 0.02,
            b: 0.0,
            c: 0.0,
            d: 0.02,
            e: 0.0,
            f: 0.0,
        },
    };
    parsed.pages[0].renditions = vec![image.clone()];
    parsed.pages[0].coverage = CoverageState::ValidatedOcr;
    parsed.coverage = vec![(1, CoverageState::ValidatedOcr)];
    parsed.pages[0].complexity.needs_ocr = true;
    parsed.pages[0]
        .capabilities
        .insert("annotation_count".into(), FeatureState::Present("1".into()));
    let old = parsed.pages[0].metadata_artifact.id.clone();
    parsed.artifacts.retain(|a| a.id != old);
    let metadata = serde_json::to_vec(
        &json!({"page":{"page_number":1,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
        "capabilities":parsed.pages[0].capabilities,"complexity":parsed.pages[0].complexity,"geometry":parsed.pages[0].geometry,"image_rects":[]}),
    )?;
    parsed.pages[0].metadata_artifact = fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::NativeMetadata,
        Some(1),
        &metadata,
        "ocr-native.json",
        "application/json",
    )?;
    let mut task = OcrTask {
        schema_version: "gptgrep.ocr.task.v1".into(),
        id: String::new(),
        task_sha256: String::new(),
        source_sha256: parsed.summary.source_sha256.clone(),
        prepared_sha256: parsed.summary.prepared_sha256.clone(),
        source_path: root.join("source.pdf"),
        page: 1,
        page_width_points: 100.0,
        page_height_points: 200.0,
        regions: vec![OcrRegion {
            id: "p1-required".into(),
            bbox: BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 200.0,
            },
            required: true,
            reason: format!(
                "required visible coverage; native complexity={:?}; annotation count=1",
                parsed.pages[0].complexity.reasons
            ),
        }],
        images: vec![image.clone()],
        native_text: "Native description\n".into(),
        layout_context: String::new(),
        profile: OcrProfile::default(),
        budget: OcrTaskBudget::default(),
    };
    task.task_sha256 = ocr_task_digest(&task)?;
    task.id = format!("ocr-{}", task.task_sha256);
    let raw = "Raw\r\n\t图像 label  ";
    let outcome = OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: 1,
        status: OcrResultStatus::Complete,
        regions: vec![OcrRegionResult {
            id: "p1-required".into(),
            bbox: task.regions[0].bbox,
            text: raw.into(),
            reading_order: 0,
            status: OcrRegionStatus::Complete,
        }],
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: vec![OcrImageDelivery {
                artifact_id: image.artifact.id.clone(),
                artifact_sha256: image.artifact.sha256.clone(),
                tool_ordinal: 1,
                mime: "image/png".into(),
                width: 2,
                height: 4,
                tool_response_sha256: "4".repeat(64),
                native: None,
            }],
            usage: None,
            latency_ms: None,
        },
    };
    fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::OcrRegions,
        Some(1),
        &serde_json::to_vec(&outcome)?,
        "ocr-regions.json",
        "application/json",
    )?;
    fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::OcrText,
        Some(1),
        format!("{raw}\n").as_bytes(),
        "ocr.txt",
        "text/plain; charset=utf-8",
    )?;
    parsed.ocr_tasks = vec![task];
    parsed.ocr_results = vec![outcome];
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging, parsed)?;
    writer.publish()
}

fn add_chart(parsed: &mut RichParsedDocument, staging: &Path, color: [u8; 3]) -> Result<()> {
    let mut pixels = vec![255u8; 64 * 32 * 3];
    // Ordinary synthetic chart without keyword text; only the raster conveys bars.
    for y in 8..30 {
        for x in 6..18 {
            pixels[(y * 64 + x) * 3..(y * 64 + x) * 3 + 3].copy_from_slice(&color);
        }
    }
    for y in 18..30 {
        for x in 30..48 {
            pixels[(y * 64 + x) * 3..(y * 64 + x) * 3 + 3].copy_from_slice(&[32, 32, 32]);
        }
    }
    let mut png = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut png),
        &pixels,
        64,
        32,
        image::ColorType::Rgb8,
        image::ImageFormat::Png,
    )?;
    let artifact = fixture_artifact(
        parsed,
        staging,
        ArtifactKind::Rendition,
        Some(1),
        &png,
        "chart.png",
        "image/png",
    )?;
    parsed.pages[0].renditions = vec![OcrImage {
        artifact,
        path: staging.join("chart.png"),
        width: 64,
        height: 32,
        page_to_raster: AffineTransform {
            a: 0.64,
            b: 0.0,
            c: 0.0,
            d: 0.16,
            e: 0.0,
            f: 0.0,
        },
    }];
    Ok(())
}
fn publish_charts(root: &Path, count: usize, text: &str) -> Result<GenerationPin> {
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    for index in 0..count {
        let (staging, mut parsed) = candidate(root, &[text])?;
        let staging_path = staging.path().canonicalize()?;
        add_chart(
            &mut parsed,
            &staging_path,
            if index % 2 == 0 {
                [220, 30, 30]
            } else {
                [30, 30, 220]
            },
        )?;
        let path = format!("d{index:02}.pdf");
        fs::copy(root.join("source.pdf"), root.join(&path))?;
        writer.add_document(&path, &staging_path, parsed)?;
    }
    writer.publish()
}
fn client(endpoint: &str, context: ExecutionContext) -> Result<JevClient> {
    Ok(
        JevClient::with_endpoint("synthetic-not-a-secret", None, endpoint)?
            .with_execution_context(context),
    )
}
async fn mock(
    count: usize,
    fail: Option<usize>,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<MockRequest>>>)> {
    mock_with_meter(
        count,
        fail,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .await
}
async fn mock_with_meter(
    count: usize,
    fail: Option<usize>,
    maximum: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<MockRequest>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/decisions", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let mut tasks = Vec::new();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for ordinal in 0..count {
            let (mut socket, _) =
                tokio::time::timeout(Duration::from_secs(15), listener.accept()).await??;
            let active = active.clone();
            let maximum = maximum.clone();
            tasks.push(tokio::spawn(async move {
                let mut bytes = Vec::new();
                let (end, length) = loop {
                    let mut buffer = [0u8; 4096];
                    let n = socket.read(&mut buffer).await?;
                    ensure!(n > 0, "synthetic request closed");
                    bytes.extend_from_slice(&buffer[..n]);
                    ensure!(bytes.len() <= 131072, "synthetic header limit");
                    if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&bytes[..end])?;
                        let length = header.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>())).context("length")??;
                        ensure!(length <= MAX_MULTIMODAL_REQUEST_BYTES, "synthetic body limit");
                        break (end + 4, length);
                    }
                };
                while bytes.len() < end + length {
                    let mut buffer = [0u8; 4096];
                    let n = socket.read(&mut buffer).await?;
                    ensure!(n > 0, "synthetic body closed");
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let body = bytes[end..end + length].to_vec();
                let parsed: Value = serde_json::from_slice(&body)?;
                let answers = parsed["questions"].as_object().context("questions")?.keys().map(|id| (id.clone(), json!({"type":"score","score":3.0,"confidence":0.75}))).collect::<serde_json::Map<_, _>>();
                let response = serde_json::to_vec(&json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,"provider":"synthetic-loopback","id":format!("synthetic-{ordinal}"),"usage":{"input_tokens":5,"output_tokens":2,"cost":0.001,"detail":"retained-private-observation"}}))?;
                let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                maximum.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                // Reverse completion order; the collector must retain the bound candidate IDs.
                tokio::time::sleep(Duration::from_millis((count - ordinal) as u64 * 2)).await;
                let status = if fail == Some(ordinal) { 500 } else { 200 };
                socket.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len()).as_bytes()).await?;
                socket.write_all(&response).await?;
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<_, anyhow::Error>(MockRequest { body, parsed })
            }));
        }
        let mut requests = Vec::new();
        for task in tasks {
            requests.push(task.await??);
        }
        Ok(requests)
    });
    Ok((endpoint, task))
}

#[tokio::test]
async fn keywordless_chart_uses_normal_images_two_stages_and_retained_context() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let pin = publish_charts(&root, 1, "")?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, None).await?;
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "Compare the bars",
        &Default::default(),
    )?;
    assert_eq!(prepared.routing_requests().count(), 1);
    assert_eq!(prepared.rerank_pool_requests().count(), 1);
    assert_eq!(prepared.candidate_scope().count(), 1);
    let reserved = prepared.reserved_output_bytes();
    let initial = prepared.execute().await?;
    let requests = server.await??;
    initial.validate()?;
    assert_eq!(initial.report().generation, pin);
    assert_eq!(initial.report().hits.len(), 1);
    assert!(matches!(
        initial.report().hits[0].content.evidence(),
        EvidenceRef::VisualRegion { .. }
    ));
    assert_eq!(
        initial.report().citation_status,
        "model_exposure_only_delivery_unqualified"
    );
    assert_eq!(initial.report().coverage.potential_text_candidates, 0);
    assert_eq!(initial.report().coverage.union_visual_candidates, 1);
    assert!(!initial.report().coverage.semantic_scope_complete);
    assert_eq!(initial.report().logical_calls_attempted, 2);
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    for (request, attempt) in requests.iter().zip(&initial.report().attempts) {
        assert_eq!(digest(&request.body), attempt.prepared_request_sha256);
        assert_eq!(request["model"], gptgrep_jev::DEFAULT_MODEL);
        assert_eq!(request["provider"]["allow_fallbacks"], false);
        assert_eq!(request["state"][0]["type"], "image_url");
        assert!(
            request["state"][0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(
            request["questions"]["candidate_0"]["instructions"]["candidate"]["image_indices"],
            json!([0])
        );
        assert_eq!(
            attempt.returned_model.as_deref(),
            Some("openai/gpt-6-luna-decisions-20261006")
        );
        assert_eq!(attempt.usage.cost, Some(json!(0.001)));
        assert!(attempt.actual_provider_requests.is_none());
        assert_eq!(
            attempt.full_usage()["detail"],
            "retained-private-observation"
        );
    }
    assert!(serde_json::to_vec(initial.report())?.len() <= reserved);
    Ok(())
}

#[tokio::test]
async fn same_text_different_rasters_union_keeps_exact_source_refs_and_image_order() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 2, "An unrelated structural caption.\n")?;
    let (endpoint, server) = mock(2, None).await?;
    let initial = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "Compare the bars",
        &Default::default(),
    )?
    .execute()
    .await?;
    let requests = server.await??;
    assert_eq!(initial.report().coverage.union_text_candidates, 2);
    assert_eq!(initial.report().coverage.union_visual_candidates, 2);
    let visuals: Vec<_> = initial
        .report()
        .hits
        .iter()
        .filter_map(|h| match &h.content {
            RichMultimodalContent::Visual { candidate } => Some(candidate),
            _ => None,
        })
        .collect();
    assert_eq!(visuals.len(), 2);
    assert_ne!(
        hash_json(&visuals[0].evidence)?,
        hash_json(&visuals[1].evidence)?
    );
    let images: Vec<_> = requests[1]["state"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v.is_object())
        .collect();
    assert_eq!(images.len(), 2);
    assert_ne!(images[0], images[1]);
    for question in requests[1]["questions"].as_object().unwrap().values() {
        let c = &question["instructions"]["candidate"];
        assert_eq!(
            c["image_indices"].as_array().unwrap().len()
                + c["text_candidate_ids"].as_array().unwrap().len(),
            1
        );
    }
    initial.validate()?;
    Ok(())
}

#[tokio::test]
async fn source_changed_after_routing_preserves_receipt_and_stops_rerank() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 1, "")?;
    let execution = context(2)?;
    let (endpoint, server) = mock(1, None).await?;
    let error = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "Compare the bars",
        &Default::default(),
    )?
    .execute_with_observer(&|_| {
        fs::write(root.join("d00.pdf"), b"changed source")?;
        Ok(())
    })
    .await
    .err()
    .unwrap();
    server.await??;
    assert_eq!(error.stage, "document_routing");
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(error.report.logical_calls_attempted, 1);
    assert_eq!(error.report.attempts[0].usage.input_tokens, Some(json!(5)));
    assert!(error.report.hits.is_empty());
    assert_eq!(execution.snapshot()?.logical_reservations, 1);
    Ok(())
}

#[tokio::test]
async fn source_or_rendition_change_before_execute_has_zero_reservations() -> Result<()> {
    for artifact_change in [false, true] {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        publish_charts(&root, 1, "")?;
        let reader = RichGenerationReader::open(&root)?;
        let execution = context(2)?;
        let prepared = prepare_rich_multimodal_search(
            reader.clone(),
            client("http://127.0.0.1:9/decisions", execution.clone())?,
            "Compare the bars",
            &Default::default(),
        )?;
        if artifact_change {
            let manifest = reader.manifest()?;
            let artifact = manifest.documents[0]
                .artifacts
                .iter()
                .find(|a| a.artifact.kind == ArtifactKind::Rendition)
                .unwrap();
            fs::write(
                root.join(".gptgrep")
                    .join("generations")
                    .join(&reader.pin().generation)
                    .join(&artifact.artifact.relative_path),
                b"changed rendition",
            )?;
        } else {
            fs::write(root.join("d00.pdf"), b"changed source")?;
        }
        let error = prepared.execute().await.err().unwrap();
        assert_eq!(error.report.logical_calls_attempted, 0);
        assert_eq!(execution.snapshot()?.logical_reservations, 0);
    }
    Ok(())
}

#[tokio::test]
async fn all_inflight_routing_observations_are_retained_on_partial_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 8, "")?;
    let execution = context(8)?;
    let (endpoint, server) = mock(2, Some(0)).await?;
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "Compare the bars",
        &Default::default(),
    )?;
    assert_eq!(prepared.routing_requests().count(), 2);
    let error = prepared.execute().await.err().unwrap();
    server.await??;
    assert_eq!(error.stage, "document_routing");
    assert_eq!(error.report.attempts.len(), 2);
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(error.report.logical_calls_attempted, 2);
    assert!(!error.report.accounting_complete);
    assert!(error.report.hits.is_empty());
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    Ok(())
}

#[tokio::test]
async fn independent_visual_lane_survives_router_cap_and_reports_truncation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 10, "")?;
    let options = RichMultimodalSearchOptions {
        max_visual_candidates: 9,
        ..Default::default()
    };
    let (endpoint, server) = mock(5, None).await?;
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(5)?)?,
        "Compare the bars",
        &options,
    )?;
    assert_eq!(prepared.candidate_scope().count(), 9);
    assert_eq!(prepared.routing_requests().count(), 2);
    assert_eq!(prepared.rerank_pool_requests().count(), 3);
    let initial = prepared.execute().await?;
    server.await??;
    assert_eq!(initial.report().coverage.routed_documents, 8);
    assert_eq!(initial.report().coverage.selected_documents, 4);
    assert_eq!(initial.report().coverage.union_visual_candidates, 9);
    assert_eq!(initial.report().coverage.omitted_visual_candidates, 1);
    assert!(!initial.report().coverage.visual_enumeration_complete);
    assert_eq!(initial.report().hits.len(), 9);
    Ok(())
}

#[tokio::test]
async fn preflight_capacity_model_scope_and_original_expiry_fail_without_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 1, "")?;
    let execution = context(3)?;
    let bound = client("http://127.0.0.1:9/decisions", execution.clone())?;
    for options in [
        RichMultimodalSearchOptions {
            max_output_bytes: 1024,
            ..Default::default()
        },
        RichMultimodalSearchOptions {
            max_metadata_bytes: 1,
            ..Default::default()
        },
        RichMultimodalSearchOptions {
            require_shared_ledger: true,
            ..Default::default()
        },
        RichMultimodalSearchOptions {
            document: Some("unavailable.pdf".into()),
            ..Default::default()
        },
        RichMultimodalSearchOptions {
            max_concurrent_batches: 5,
            ..Default::default()
        },
    ] {
        assert!(
            prepare_rich_multimodal_search(
                RichGenerationReader::open(&root)?,
                bound.clone(),
                "Compare the bars",
                &options
            )
            .is_err()
        );
    }
    let legacy = JevClient::with_endpoint(
        "synthetic-not-a-secret",
        Some("typesafe/jev-1.13"),
        "http://127.0.0.1:9/decisions",
    )?
    .with_execution_context(execution.clone());
    assert!(
        prepare_rich_multimodal_search(
            RichGenerationReader::open(&root)?,
            legacy,
            "Compare the bars",
            &Default::default()
        )
        .is_err()
    );
    let mut resources = ResourceBudget::default();
    resources.owned_working_bytes = 1_048_576;
    assert!(
        prepare_rich_multimodal_search(
            RichGenerationReader::open(&root)?,
            bound.clone(),
            "Compare the bars",
            &RichMultimodalSearchOptions {
                resources,
                ..Default::default()
            }
        )
        .is_err()
    );
    let short = bound.with_execution_context(
        execution
            .clone()
            .with_deadline(Instant::now() + Duration::from_millis(200))?,
    );
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        short,
        "Compare the bars",
        &Default::default(),
    )?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(prepared.execute().await.is_err());
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    assert!(execution.original_run_remaining()? > Duration::from_secs(40));
    Ok(())
}

#[tokio::test]
async fn saved_native_ocr_and_visual_refs_keep_independent_coordinate_kinds() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_saved_ocr(&root)?;
    let (endpoint, server) = mock(2, None).await?;
    let initial = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "unmatched conceptual query",
        &Default::default(),
    )?
    .execute()
    .await?;
    server.await??;
    assert!(
        initial
            .report()
            .hits
            .iter()
            .any(|h| matches!(h.content.evidence(), EvidenceRef::NativeText { .. }))
    );
    assert!(
        initial
            .report()
            .hits
            .iter()
            .any(|h| matches!(h.content.evidence(), EvidenceRef::OcrText { .. }))
    );
    assert!(
        initial
            .report()
            .hits
            .iter()
            .any(|h| matches!(h.content.evidence(), EvidenceRef::VisualRegion { .. }))
    );
    initial.validate()?;
    Ok(())
}

#[tokio::test]
async fn text_only_generation_still_has_mandatory_route_and_rerank() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["An ordinary source statement.\n"])?;
    let (endpoint, server) = mock(2, None).await?;
    let initial = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "unmatched concept",
        &Default::default(),
    )?
    .execute()
    .await?;
    server.await??;
    assert_eq!(initial.report().coverage.union_text_candidates, 1);
    assert_eq!(initial.report().coverage.union_visual_candidates, 0);
    assert!(matches!(
        initial.report().hits[0].content.evidence(),
        EvidenceRef::NativeText { .. }
    ));
    assert_eq!(initial.report().attempts[0].stage, "document_routing");
    assert_eq!(initial.report().attempts[1].stage, "evidence_reranking");
    Ok(())
}

#[tokio::test]
async fn bounded_concurrent_batches_share_one_global_context() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_charts(&root, 16, "")?;
    let maximum = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let execution = context(6)?;
    let options = RichMultimodalSearchOptions {
        max_visual_candidates: 16,
        max_concurrent_batches: 2,
        ..Default::default()
    };
    let (endpoint, server) = mock_with_meter(6, None, maximum.clone()).await?;
    let initial = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "Compare the bars",
        &options,
    )?
    .execute()
    .await?;
    let requests = server.await??;
    assert_eq!(requests.len(), 6);
    assert_eq!(maximum.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(initial.report().logical_calls_attempted, 6);
    assert_eq!(initial.report().coverage.union_visual_candidates, 16);
    assert_eq!(execution.snapshot()?.logical_reservations, 6);
    assert_eq!(execution.snapshot()?.terminal_submissions, 6);
    assert!(
        initial
            .report()
            .attempts
            .iter()
            .all(|a| a.question_count <= 4)
    );
    assert!(requests.iter().all(|r| {
        r["state"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p.is_object())
            .count()
            <= 4
    }));
    Ok(())
}

#[test]
fn normalized_lexical_windows_short_text_keeps_one_full_typed_candidate() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let window_bytes = 128;
    let text = format!("{} needle aa needle bb needle", "prefix ".repeat(8));
    assert!(text.len() < window_bytes);
    let previous_starts: BTreeSet<_> = text
        .match_indices("needle")
        .map(|(offset, _)| offset.saturating_sub(window_bytes / 4))
        .collect();
    assert_eq!(previous_starts.len(), 3);
    assert!(!previous_starts.contains(&0));
    publish_charts(&root, 1, &text)?;
    let reader = RichGenerationReader::open(&root)?;
    let locations = reader.text_window_locations("needle", None, window_bytes, &|| Ok(()))?;
    let lexical: Vec<_> = locations.windows.iter().filter(|row| row.lexical).collect();
    assert_eq!(lexical.len(), 1);
    assert_eq!(lexical[0].offset, 0);
    assert!(
        locations
            .windows
            .iter()
            .any(|row| !row.lexical && row.offset == 0)
    );
    let execution = context(2)?;
    let prepared = prepare_rich_multimodal_search(
        reader.clone(),
        client("http://127.0.0.1:1/unused", execution.clone())?,
        "needle",
        &RichMultimodalSearchOptions {
            window_bytes,
            max_visual_candidates: 1,
            ..Default::default()
        },
    )?;
    let text_entries: Vec<_> = prepared
        .pool
        .iter()
        .filter(|entry| matches!(entry.content, RichMultimodalContent::Text { .. }))
        .collect();
    assert_eq!(text_entries.len(), 1);
    let entry = text_entries[0];
    assert!(entry.lexical && entry.structural);
    let RichMultimodalContent::Text {
        window,
        literal_anchor,
    } = &entry.content
    else {
        unreachable!()
    };
    assert!(*literal_anchor);
    assert_eq!(window.text, text);
    assert_eq!(binding(window).byte_start, 0);
    assert_eq!(binding(window).byte_end, text.len());
    assert!(matches!(window.evidence, EvidenceRef::NativeText { .. }));
    reader.validate_text_evidence(&window.evidence)?;
    assert_eq!(prepared.routing_requests().count(), 1);
    let pools: Vec<_> = prepared.rerank_pool_requests().collect();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].question_count(), 2);
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    assert_eq!(execution.snapshot()?.submission_observations, 0);
    Ok(())
}

#[test]
fn normalized_lexical_windows_utf8_tail_contains_previous_window() -> Result<()> {
    let window_bytes = 32;
    for scalar in ['é', '界', '🦀'] {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let prefix = "p".repeat(window_bytes * 3);
        let suffix = format!(
            "{} needle",
            "s".repeat(window_bytes + 1 - scalar.len_utf8() - 7)
        );
        let text = format!("{prefix}{scalar}{suffix}");
        let tail = text.len() - window_bytes;
        assert!(!text.is_char_boundary(tail));
        let found = text.find("needle").unwrap();
        let mut previous_start = found.saturating_sub(window_bytes / 4);
        while !text.is_char_boundary(previous_start) {
            previous_start += 1;
        }
        assert!(previous_start > tail);
        publish(&root, &[&text])?;
        let reader = RichGenerationReader::open(&root)?;
        let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
        let previous = reader.read_text(&artifact.id, previous_start, window_bytes)?;
        let locations = reader.text_window_locations("needle", None, window_bytes, &|| Ok(()))?;
        let lexical: Vec<_> = locations.windows.iter().filter(|row| row.lexical).collect();
        assert_eq!(lexical.len(), 1);
        let start = lexical[0].offset;
        assert_eq!(start, prefix.len() + scalar.len_utf8());
        assert!(start < previous_start && text.is_char_boundary(start));
        let window = reader.read_text(&artifact.id, start, window_bytes)?;
        assert!(window.text.len() <= window_bytes);
        assert!(window.text.ends_with(&previous.text));
        assert!(window.text.contains("needle"));
        assert_eq!(binding(&window).byte_end, text.len());
        assert!(text.is_char_boundary(binding(&window).byte_end));
        assert!(window.next_offset.is_none());
        reader.validate_text_evidence(&window.evidence)?;
    }
    Ok(())
}

#[test]
fn normalized_lexical_windows_long_document_keeps_distinct_middle_offsets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let window_bytes = 128;
    let positions = [window_bytes, window_bytes * 3];
    let mut bytes = vec![b'x'; window_bytes * 8];
    for position in positions {
        bytes[position..position + 6].copy_from_slice(b"needle");
    }
    let text = String::from_utf8(bytes)?;
    publish(&root, &[&text])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
    let locations = reader.text_window_locations("needle", None, window_bytes, &|| Ok(()))?;
    let lexical: Vec<_> = locations.windows.iter().filter(|row| row.lexical).collect();
    assert_eq!(
        lexical.iter().map(|row| row.offset).collect::<Vec<_>>(),
        positions.map(|position| position - window_bytes / 4)
    );
    assert!(
        locations
            .windows
            .iter()
            .any(|row| !row.lexical && row.offset == 0)
    );
    for location in lexical {
        let window = reader.read_text(&artifact.id, location.offset, window_bytes)?;
        assert_eq!(window.text.len(), window_bytes);
        assert!(window.text.contains("needle"));
        assert!(window.next_offset.is_some());
        reader.validate_text_evidence(&window.evidence)?;
    }
    Ok(())
}

#[test]
fn normalized_lexical_windows_keep_native_and_ocr_artifacts_distinct() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_saved_ocr(&root)?;
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client("http://127.0.0.1:1/unused", context(2)?)?,
        "Native label",
        &Default::default(),
    )?;
    let windows: Vec<_> = prepared
        .pool
        .iter()
        .filter_map(|entry| match &entry.content {
            RichMultimodalContent::Text { window, .. } => Some(window),
            RichMultimodalContent::Visual { .. } => None,
        })
        .collect();
    assert_eq!(windows.len(), 2);
    assert!(
        windows
            .iter()
            .any(|window| matches!(window.evidence, EvidenceRef::NativeText { .. }))
    );
    assert!(
        windows
            .iter()
            .any(|window| matches!(window.evidence, EvidenceRef::OcrText { .. }))
    );
    assert!(windows.iter().all(|window| binding(window).byte_start == 0));
    assert_ne!(
        binding(windows[0]).artifact_id,
        binding(windows[1]).artifact_id
    );
    Ok(())
}

#[tokio::test]
async fn distinct_windows_of_same_artifact_have_distinct_typed_units() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let text = format!(
        "{} requested-marker\n",
        "ordinary structural prefix ".repeat(40)
    );
    publish(&root, &[&text])?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, None).await?;
    let options = RichMultimodalSearchOptions {
        window_bytes: 256,
        ..Default::default()
    };
    let prepared = prepare_rich_multimodal_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "requested-marker",
        &options,
    )?;
    let count = prepared.candidate_scope().count();
    assert!(count >= 2);
    let initial = prepared.execute().await?;
    server.await??;
    assert_eq!(initial.report().hits.len(), count);
    let bindings: Vec<_> = initial
        .report()
        .hits
        .iter()
        .map(|h| match &h.content {
            RichMultimodalContent::Text { window, .. } => binding(window),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(bindings[0].artifact_id, bindings[1].artifact_id);
    assert_ne!(bindings[0].byte_start, bindings[1].byte_start);
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    initial.validate()?;
    Ok(())
}
