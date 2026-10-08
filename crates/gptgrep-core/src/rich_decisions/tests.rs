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
            coverage: CoverageState::Native,
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
    let unmapped_merged_lines = document.text.lines().count().max(1);
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
                .map(|(i, _)| (i as u32 + 1, CoverageState::Native))
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
async fn mock_with_score(
    count: usize,
    action: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    fail_second: bool,
    score: f64,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<MockRequest>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/decisions", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for ordinal in 0..count {
            let (mut socket, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut buffer = [0u8; 4096];
                let n = socket.read(&mut buffer).await?;
                ensure!(n > 0, "request header closed");
                bytes.extend_from_slice(&buffer[..n]);
                ensure!(bytes.len() <= 131072, "mock request admission");
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end])?;
                    let len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>())
                        })
                        .context("content length")??;
                    break (end + 4, len);
                }
            };
            while bytes.len() < header_end + length {
                let mut buffer = [0u8; 4096];
                let n = socket.read(&mut buffer).await?;
                ensure!(n > 0, "request body closed");
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[header_end..header_end + length])?;
            action(ordinal)?;
            let status = if fail_second && ordinal == 1 {
                500
            } else {
                200
            };
            let answers = request["questions"]
                .as_object()
                .context("questions")?
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        json!({"type":"score","score":score,"confidence":0.75}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let body = serde_json::to_vec(
                &json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,
                "usage":{"input_tokens":5,"output_tokens":2,"cost":0.001,"unknown_detail":"x".repeat(131072)},"id":"synthetic-response","provider":"synthetic-loopback"}),
            )?;
            socket.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;
            requests.push(MockRequest {
                body: bytes[header_end..header_end + length].to_vec(),
                parsed: request,
            });
        }
        Ok(requests)
    });
    Ok((endpoint, task))
}
async fn mock(
    count: usize,
    action: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    fail_second: bool,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<MockRequest>>>)> {
    mock_with_score(count, action, fail_second, 3.0).await
}
fn client(endpoint: &str, context: ExecutionContext) -> Result<JevClient> {
    Ok(
        JevClient::with_endpoint("ordinary-fixture-not-a-secret", None, endpoint)?
            .with_execution_context(context),
    )
}

#[tokio::test]
async fn native_initial_pass_routes_unmatched_source_and_preserves_full_usage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["An entirely distinct source statement.\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let prepared = prepare_rich_text_search(
        reader,
        client(&endpoint, execution.clone())?,
        "maturity mechanism",
        &RichTextSearchOptions::default(),
    )?;
    assert_eq!(prepared.pool.len(), 1);
    assert!(!prepared.pool[0].lexical);
    let reserved = prepared.reserved_output_bytes();
    let initial = prepared.execute().await?;
    let requests = server.await??;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["questions"].as_object().unwrap().len(), 1);
    initial.validate()?;
    let report = initial.report();
    assert_eq!(report.hits.len(), 1);
    assert_eq!(report.validated_responses, 2);
    assert_eq!(report.logical_calls_attempted, 2);
    assert!(report.accounting_complete);
    assert!(
        report.attempts[0].full_usage()["unknown_detail"]
            .as_str()
            .unwrap()
            .len()
            > 100000
    );
    assert_eq!(report.attempts[1].usage.cost, Some(json!(0.001)));
    let encoded = serde_json::to_vec(report)?;
    assert!(encoded.len() < reserved);
    assert!(!std::str::from_utf8(&encoded)?.contains("unknown_detail"));
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    Ok(())
}

#[tokio::test]
async fn preflight_output_and_encoded_request_reject_before_admission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source text\n"])?;
    let execution = context(2)?;
    let bound = client("http://127.0.0.1:9/decisions", execution.clone())?;
    let options = RichTextSearchOptions {
        max_output_bytes: 1024,
        ..Default::default()
    };
    let error = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        bound.clone(),
        "source",
        &options,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("output_budget_insufficient"));
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    let options = RichTextSearchOptions {
        require_shared_ledger: true,
        ..Default::default()
    };
    assert!(
        prepare_rich_text_search(
            RichGenerationReader::open(&root)?,
            bound,
            "source",
            &options
        )
        .is_err()
    );
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    // A complete potential pool is admitted before routing, even when a subset would fit.
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    for i in 0..8 {
        let text = "\u{0001}".repeat(2048) + " source";
        let (staging, parsed) = candidate(&root, &[&text])?;
        let path = format!("d{i}.pdf");
        fs::copy(root.join("source.pdf"), root.join(&path))?;
        writer.add_document(&path, &staging.path().canonicalize()?, parsed)?;
    }
    writer.publish()?;
    let options = RichTextSearchOptions {
        window_bytes: 2048,
        ..Default::default()
    };
    let error = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client("http://127.0.0.1:9/decisions", execution.clone())?,
        "source",
        &options,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("request exceeded"));
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    Ok(())
}

#[tokio::test]
async fn required_second_stage_failure_preserves_first_receipt_and_observer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source\n"])?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), true).await?;
    let seen = std::sync::Mutex::new(Vec::new());
    let prepared = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "source",
        &Default::default(),
    )?;
    let error = prepared
        .execute_with_observer(&|attempt| {
            seen.lock()
                .unwrap()
                .push(attempt.full_usage()["input_tokens"].clone());
            Ok(())
        })
        .await
        .err()
        .unwrap();
    server.await??;
    assert_eq!(error.stage, "evidence_reranking");
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(error.report.logical_calls_attempted, 2);
    assert_eq!(error.report.attempts[0].usage.input_tokens, Some(json!(5)));
    assert_eq!(error.report.attempts[1].usage.cost, None);
    assert_eq!(*seen.lock().unwrap(), vec![json!(5)]);
    assert!(!error.report.accounting_complete);
    assert!(error.report.hits.is_empty());
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    Ok(())
}

#[tokio::test]
async fn source_change_after_routing_stops_before_second_submission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source\n"])?;
    let source = root.join("source.pdf");
    let execution = context(2)?;
    let (endpoint, server) = mock(
        1,
        Arc::new(move |_| {
            fs::write(&source, b"changed source")?;
            Ok(())
        }),
        false,
    )
    .await?;
    let error = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "source",
        &Default::default(),
    )?
    .execute()
    .await
    .err()
    .unwrap();
    server.await??;
    assert_eq!(error.stage, "document_routing");
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(error.report.logical_calls_attempted, 1);
    assert_eq!(execution.snapshot()?.logical_reservations, 1);
    Ok(())
}

#[tokio::test]
async fn global_one_credit_is_measured_partial_failure_without_fallback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source\n"])?;
    let execution = context(1)?;
    let (endpoint, server) = mock(1, Arc::new(|_| Ok(())), false).await?;
    let error = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "source",
        &Default::default(),
    )?
    .execute()
    .await
    .err()
    .unwrap();
    server.await??;
    assert_eq!(error.stage, "evidence_reranking");
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(execution.snapshot()?.logical_reservations, 1);
    assert_eq!(error.report.logical_calls_attempted, 2);
    assert!(error.report.hits.is_empty());
    Ok(())
}

#[tokio::test]
async fn retained_pin_survives_new_current_and_literal_ties_are_stable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let pin = publish(&root, &["ordinary source\nsecond line\n"])?;
    let old = RichGenerationReader::open_pinned(&root, &pin)?;
    let publish_root = root.clone();
    let (endpoint, server) = mock(
        2,
        Arc::new(move |ordinal| {
            if ordinal == 0 {
                publish(&publish_root, &["new parser text\n"])?;
            }
            Ok(())
        }),
        false,
    )
    .await?;
    let initial = prepare_rich_text_search(
        old,
        client(&endpoint, context(2)?)?,
        "ordinary",
        &Default::default(),
    )?
    .execute()
    .await?;
    server.await??;
    assert_eq!(initial.report().generation, pin);
    initial.validate()?;
    assert!(initial.report().hits.iter().any(|hit| hit.literal_anchor));
    assert!(
        initial
            .report()
            .hits
            .iter()
            .all(|hit| binding(&hit.window).generation == pin)
    );
    Ok(())
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
fn publish_saved_ocr(root: &Path) -> Result<GenerationPin> {
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
#[tokio::test]
async fn saved_ocr_and_native_windows_remain_distinct_with_raw_region_receipt_binding() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_saved_ocr(&root)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let initial = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "unmatched conceptual query",
        &Default::default(),
    )?
    .execute()
    .await?;
    server.await??;
    initial.validate()?;
    assert!(
        initial
            .report()
            .hits
            .iter()
            .any(|hit| matches!(hit.window.evidence, EvidenceRef::NativeText { .. }))
    );
    let hit = initial
        .report()
        .hits
        .iter()
        .find(|hit| matches!(hit.window.evidence, EvidenceRef::OcrText { .. }))
        .unwrap();
    assert_eq!(hit.window.text, "Raw\r\n\t图像 label  \n");
    let EvidenceRef::OcrText {
        binding,
        region_ids,
        ocr_profile_sha256,
        ocr_receipt_sha256,
    } = &hit.window.evidence
    else {
        unreachable!()
    };
    assert_eq!(binding.byte_start, 0);
    assert_eq!(binding.byte_end, hit.window.text.len());
    assert_eq!(region_ids, &vec!["p1-required".to_owned()]);
    assert_eq!(ocr_profile_sha256.len(), 64);
    assert_eq!(ocr_receipt_sha256.len(), 64);
    assert_eq!(
        initial.report().citation_status,
        "model_exposure_only_delivery_unqualified"
    );
    Ok(())
}

#[tokio::test]
async fn expired_original_context_and_observer_stop_do_not_submit_later_stage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source\n"])?;
    let execution = ExecutionContext::new(
        "ordinary-short-deadline",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(1),
        2,
    )?;
    let prepared = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client("http://127.0.0.1:9/decisions", execution.clone())?,
        "source",
        &Default::default(),
    )?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let error = prepared.execute().await.err().unwrap();
    assert_eq!(error.report.logical_calls_attempted, 0);
    assert!(!error.report.accounting_complete);
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    let execution = context(2)?;
    let (endpoint, server) = mock(1, Arc::new(|_| Ok(())), false).await?;
    let prepared = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "source",
        &Default::default(),
    )?;
    let error = prepared
        .execute_with_observer(&|_| Err(anyhow::anyhow!("synthetic sink stop")))
        .await
        .err()
        .unwrap();
    server.await??;
    assert_eq!(error.code, "rich_decisions_observer_failed");
    assert_eq!(error.report.validated_responses, 1);
    assert_eq!(error.report.logical_calls_attempted, 1);
    assert!(error.report.accounting_complete);
    assert_eq!(execution.snapshot()?.logical_reservations, 1);
    Ok(())
}

#[tokio::test]
async fn aggregate_metadata_admission_precedes_window_copy_and_model_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source one\n", "ordinary source two\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let observed = reader.clone();
    let execution = context(2)?;
    let options = RichTextSearchOptions {
        max_metadata_bytes: 4096,
        ..Default::default()
    };
    let error = prepare_rich_text_search(
        reader,
        client("http://127.0.0.1:9/decisions", execution.clone())?,
        "source",
        &options,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("metadata_budget_insufficient"));
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    assert_eq!(observed.read_metrics()?.logical_reader_bytes, 0);
    Ok(())
}

#[tokio::test]
async fn bounded_documents_artifacts_union_and_submitted_question_order_are_explicit() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    for i in 0..10 {
        let (staging, parsed) = candidate(
            &root,
            &["ordinary one\n", "ordinary two\n", "ordinary three\n"],
        )?;
        let path = format!("d{i:02}.pdf");
        fs::copy(root.join("source.pdf"), root.join(&path))?;
        writer.add_document(&path, &staging.path().canonicalize()?, parsed)?;
    }
    writer.publish()?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let prepared = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "unmatched conceptual query",
        &Default::default(),
    )?;
    let original_ids: Vec<_> = prepared
        .pool
        .iter()
        .map(|p| p.candidate.id.clone())
        .collect();
    assert!(prepared.pool.iter().all(|p| {
        matches!(binding(&p.window).page, 1 | 2) && p.window.text != "ordinary three\n"
    }));
    let full_bytes = prepared.full_pool_request_bytes();
    let planned_route = prepared.routing_request().canonical_body().to_vec();
    let planned_pool: Value =
        serde_json::from_slice(prepared.rerank_pool_request().canonical_body())?;
    assert_eq!(
        crate::digest(&planned_route),
        prepared.routing_request().body_sha256()
    );
    assert_eq!(
        prepared.rerank_pool_request().canonical_body().len(),
        full_bytes
    );
    let initial = prepared.execute().await?;
    let requests = server.await??;
    assert_eq!(requests[0].body, planned_route);
    assert_eq!(initial.report().coverage.scoped_documents, 10);
    assert_eq!(initial.report().coverage.routed_documents, 8);
    assert_eq!(initial.report().coverage.selected_documents, 4);
    assert_eq!(initial.report().coverage.available_artifacts, 30);
    assert_eq!(initial.report().coverage.inspected_artifacts, 24);
    assert_eq!(initial.report().coverage.omitted_artifacts, 6);
    assert!(!initial.report().coverage.semantic_scope_complete);
    assert_eq!(initial.report().coverage.potential_windows, 16);
    assert_eq!(initial.report().coverage.union_windows, 8);
    assert!(initial.report().attempts[1].encoded_request_bytes <= full_bytes);
    let questions = requests[1]["questions"].as_object().unwrap();
    let submitted: Vec<_> = (0..questions.len())
        .map(|i| {
            questions[&format!("candidate_{i}")]["instructions"]["candidate"]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let positions: Vec<_> = submitted
        .iter()
        .map(|id| {
            original_ids
                .iter()
                .position(|original| original == id)
                .unwrap()
        })
        .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]));
    // The actual dependent request only deletes source candidates and renumbers
    // them; shared query, rubric, source text, model and provider stay unchanged.
    let mut expected_rerank = planned_pool.clone();
    expected_rerank["questions"] = Value::Object(
        positions
            .iter()
            .enumerate()
            .map(|(new_index, old_index)| {
                (
                    format!("candidate_{new_index}"),
                    planned_pool["questions"][format!("candidate_{old_index}")].clone(),
                )
            })
            .collect(),
    );
    assert_eq!(requests[1].body, serde_json::to_vec(&expected_rerank)?);
    Ok(())
}

#[tokio::test]
async fn literal_eligibility_remains_atomic_and_term_omission_is_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["ordinary source\n"])?;
    for (query, retained) in [("ordinary", true), ("ordinary source", false)] {
        let (endpoint, server) = mock_with_score(2, Arc::new(|_| Ok(())), false, 0.0).await?;
        let initial = prepare_rich_text_search(
            RichGenerationReader::open(&root)?,
            client(&endpoint, context(2)?)?,
            query,
            &Default::default(),
        )?
        .execute()
        .await?;
        server.await??;
        assert_eq!(!initial.report().hits.is_empty(), retained);
    }
    let query = (0..17)
        .map(|i| format!("term{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let prepared = prepare_rich_text_search(
        RichGenerationReader::open(&root)?,
        client("http://127.0.0.1:9/decisions", context(2)?)?,
        &query,
        &Default::default(),
    )?;
    assert!(prepared.report.coverage.lexical_query_terms_truncated);
    Ok(())
}

#[tokio::test]
async fn saved_text_change_after_rerank_rejects_results_and_keeps_both_receipts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let pin = publish(&root, &["ordinary source\n"])?;
    let reader = RichGenerationReader::open_pinned(&root, &pin)?;
    let relative = reader.manifest()?.documents[0].pages[0]
        .native_text
        .relative_path
        .clone();
    let artifact = root
        .join(crate::STATE)
        .join("generations")
        .join(&pin.generation)
        .join(relative);
    let (endpoint, server) = mock(
        2,
        Arc::new(move |ordinal| {
            if ordinal == 1 {
                fs::write(&artifact, b"changed saved text")?;
            }
            Ok(())
        }),
        false,
    )
    .await?;
    let error = prepare_rich_text_search(
        reader,
        client(&endpoint, context(2)?)?,
        "source",
        &Default::default(),
    )?
    .execute()
    .await
    .err()
    .unwrap();
    server.await??;
    assert_eq!(error.stage, "evidence_reranking");
    assert_eq!(error.report.validated_responses, 2);
    assert!(error.report.hits.is_empty());
    assert!(
        error
            .report
            .attempts
            .iter()
            .all(|a| a.response_status == "validated")
    );
    Ok(())
}
