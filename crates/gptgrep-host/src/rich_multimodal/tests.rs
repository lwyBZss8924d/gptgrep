//! Synthetic writer/Decisions and in-memory actual Rpc fixtures, no native model.
use super::*;
use anyhow::Context;
use gptgrep_core::RichGenerationWriter;
use gptgrep_parse::*;
use image::ImageEncoder;
use std::{
    fs,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as PollContext, Poll},
};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

fn config() -> HostConfig {
    HostConfig {
        model: "gpt-6-luna".into(),
        ..Default::default()
    }
}
async fn generation(
    color: u8,
) -> Result<(tempfile::TempDir, RichGenerationReader, Vec<u8>, String)> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (staging, mut parsed) =
        crate::rich_text::tests::candidate(&root, &["alpha\r\n中 tail\n"]).await?;
    let stage = staging.path().canonicalize()?;
    let mut png = vec![];
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        &[color, 22, 33, 255].repeat(8 * 16),
        8,
        16,
        image::ExtendedColorType::Rgba8,
    )?;
    let page = &mut parsed.pages[0];
    let sha = crate::rich_text::hash(&png);
    let artifact = ArtifactRef {
        id: gptgrep_parse::artifact_id(
            &parsed.summary.source_sha256,
            &parsed.native_profile_sha256,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &sha,
        )?,
        kind: ArtifactKind::Rendition,
        relative_path: "rendition.png".into(),
        sha256: sha,
        profile_sha256: parsed.native_profile_sha256.clone(),
        bytes: png.len() as u64,
        mime: "image/png".into(),
        page: Some(1),
        region_id: None,
    };
    fs::write(stage.join("rendition.png"), &png)?;
    page.renditions.push(OcrImage {
        path: PathBuf::from("rendition.png"),
        artifact: artifact.clone(),
        width: 8,
        height: 16,
        page_to_raster: AffineTransform {
            a: 0.08,
            b: 0.0,
            c: 0.0,
            d: 0.08,
            e: 0.0,
            f: 0.0,
        },
    });
    parsed.artifacts.push(artifact);
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &stage, parsed)?;
    writer.publish()?;
    let reader = RichGenerationReader::open(&root)?;
    let id = reader.manifest()?.documents[0].pages[0].renditions[0]
        .artifact
        .id
        .clone();
    Ok((temp, reader, png, id))
}
async fn fixture(color: u8) -> Result<(tempfile::TempDir, RichEvidence, Vec<u8>, String)> {
    let (temp, reader, png, id) = generation(color).await?;
    let context = crate::rich_text::tests::context(2)?;
    let (endpoint, server) = crate::rich_text::tests::mock(2, Arc::new(|_| Ok(())), false).await?;
    let mut options = RichMultimodalHostOptions::default();
    options.retrieval.require_shared_ledger = false;
    let prepared = prepare_rich_multimodal_host(
        reader,
        crate::rich_text::tests::client(&endpoint, context)?,
        "same neutral question",
        &config(),
        &options,
    )?;
    let initial = prepared.bootstrap.execute().await?;
    assert_eq!(server.await??.len(), 2);
    let evidence = from_initial(
        initial,
        8,
        prepared.deadline,
        None,
        options.retrieval.resources,
    )?;
    Ok((temp, evidence, png, id))
}
async fn line(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Result<Value> {
    let mut s = String::new();
    ensure!(reader.read_line(&mut s).await? > 0, "fixture closed");
    Ok(serde_json::from_str(&s)?)
}
async fn emit(writer: &mut (impl AsyncWrite + Unpin), value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
#[derive(Clone, Copy)]
enum Mode {
    Image,
    Crop,
    Mixed,
    Exposure,
    Metadata,
    Forged,
    Replayed,
    Budget,
}
async fn native_fixture(
    stream: tokio::io::DuplexStream,
    id: String,
    mode: Mode,
) -> Result<Vec<Value>> {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut frames = vec![];
    loop {
        let request = line(&mut read).await?;
        frames.push(request.clone());
        match request["method"].as_str() {
            Some("initialize") => emit(&mut write, json!({"id":1,"result":{}})).await?,
            Some("initialized") => {}
            Some("account/read") => {
                emit(
                    &mut write,
                    json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
                )
                .await?
            }
            Some("config/read") => {
                emit(
                    &mut write,
                    json!({"id":2,"result":{"config":{"mcp_servers":{}}}}),
                )
                .await?
            }
            Some("thread/start") => {
                assert_eq!(request["params"]["model"], "gpt-6-luna");
                assert_eq!(request["params"]["serviceTier"], "fast");
                emit(&mut write,json!({"id":3,"result":{"thread":{"id":"visual-thread"},"model":"gpt-6-luna","modelProvider":"openai",
                    "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"priority"}})).await?;
            }
            Some("turn/start") => {
                let state: Value = serde_json::from_str(
                    request["params"]["input"][0]["text"]
                        .as_str()
                        .context("state")?,
                )?;
                assert_eq!(
                    state["citation_status"],
                    "model_exposure_only_explicit_delivery_required"
                );
                let mut text_citation = None;
                if matches!(mode, Mode::Mixed) {
                    let text_hit = state["initial_retrieval"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|hit| hit["content"]["kind"] == "text")
                        .context("text seed")?;
                    let artifact =
                        text_hit["content"]["window"]["evidence"]["binding"]["artifact_id"].clone();
                    emit(&mut write, json!({"id":"text-before-image","method":"item/tool/call","params":{
                        "threadId":"visual-thread","turnId":"visual-turn","callId":"text-call","namespace":"gptgrep",
                        "tool":"gptgrep_read_artifact","arguments":{"artifact_id":artifact,"offset":0,"max_bytes":2048}}})).await?;
                    let response = line(&mut read).await?;
                    assert_eq!(response["id"], "text-before-image");
                    assert_eq!(
                        response["result"]["contentItems"].as_array().unwrap().len(),
                        1
                    );
                    let body: Value = serde_json::from_str(
                        response["result"]["contentItems"][0]["text"]
                            .as_str()
                            .unwrap(),
                    )?;
                    assert_eq!(body["window"]["text"], "alpha\r\n中 tail\n");
                    text_citation = Some(
                        json!({"kind":"text","issued_id":body["issued_id"],"offset":7,"length":3,"quote":"中"}),
                    );
                    frames.push(response);
                }
                let issued = if matches!(mode, Mode::Exposure) {
                    state["initial_retrieval"][0]["candidate_id"].clone()
                } else {
                    let (tool, args) = if matches!(mode, Mode::Metadata) {
                        ("gptgrep_inspect_artifact", json!({"document":"source.pdf"}))
                    } else if matches!(mode, Mode::Crop) {
                        (
                            "gptgrep_render",
                            json!({"resource_id":id,"bbox":{"x":0.0,"y":0.0,"width":50.0,"height":100.0}}),
                        )
                    } else {
                        ("gptgrep_render", json!({"resource_id":id}))
                    };
                    // Dynamic tool arrives before the id4 turn/start ACK.
                    let call = json!({"id":-45,"method":"item/tool/call","params":{"threadId":"visual-thread","turnId":"visual-turn",
                        "callId":"visual-call","namespace":"gptgrep","tool":tool,"arguments":args}});
                    emit(&mut write, call.clone()).await?;
                    let response = line(&mut read).await?;
                    frames.push(response.clone());
                    assert_eq!(response["id"], -45);
                    let body: Value = serde_json::from_str(
                        response["result"]["contentItems"][0]["text"]
                            .as_str()
                            .context("body")?,
                    )?;
                    if matches!(mode, Mode::Metadata) {
                        assert_eq!(
                            response["result"]["contentItems"].as_array().unwrap().len(),
                            1
                        );
                        json!(id)
                    } else {
                        assert_eq!(response["result"]["contentItems"][1]["type"], "inputImage");
                        assert!(
                            response["result"]["contentItems"][1]["imageUrl"]
                                .as_str()
                                .unwrap()
                                .starts_with("data:image/png;base64,")
                        );
                        if matches!(mode, Mode::Replayed) {
                            emit(&mut write, call).await?;
                            return Ok(frames);
                        }
                        body["issued_id"].clone()
                    }
                };
                emit(
                    &mut write,
                    json!({"id":4,"result":{"turn":{"id":"visual-turn"}}}),
                )
                .await?;
                let citation = if matches!(mode, Mode::Forged) {
                    json!({"kind":"image","issued_id":issued,"rendition_sha256":"forged"})
                } else {
                    json!({"kind":"image","issued_id":issued})
                };
                let mut citations = vec![citation];
                if let Some(text) = text_citation {
                    citations.push(text);
                }
                let answer=json!({"answer":"observed figure","insufficient_evidence":false,"citations":citations}).to_string();
                emit(&mut write,json!({"method":"turn/completed","params":{"threadId":"visual-thread","turn":{"id":"visual-turn","status":"completed","error":null,
                    "items":[{"type":"agentMessage","text":answer}]}}})).await?;
                return Ok(frames);
            }
            _ => return Err(anyhow!("fixture unexpected method")),
        }
    }
}
async fn drive(
    evidence: &mut RichEvidence,
    id: String,
    mode: Mode,
) -> Result<(protocol::Outcome, Vec<Value>)> {
    let (host, server) = tokio::io::duplex(262144);
    let (read, write) = tokio::io::split(host);
    let task = tokio::spawn(native_fixture(server, id, mode));
    let deadline = evidence.deadline;
    let result = protocol::run_observed_budget(
        read,
        write,
        Path::new("."),
        &config(),
        protocol::Workflow::RichMultimodal {
            question: "same neutral question",
            evidence,
        },
        None,
        protocol::RunOptions::default(),
        crate::transport::TransportBudget::new(if matches!(mode, Mode::Budget) {
            1_048_576 - 1
        } else {
            protocol::MAX_TOTAL_BYTES
        }),
        deadline,
    )
    .await;
    match result {
        Ok(outcome) => Ok((outcome, task.await??)),
        Err(error) => {
            task.abort();
            Err(error)
        }
    }
}
#[tokio::test]
async fn actual_rpc_image_and_crop_delivery_mint_only_fresh_exact_identities() -> Result<()> {
    for mode in [Mode::Image, Mode::Crop] {
        let (_temp, mut evidence, png, id) = fixture(11).await?;
        let (outcome, frames) = drive(&mut evidence, id, mode).await?;
        let value = finish(&evidence, &outcome.answer)?;
        let RichMultimodalCitation::Image { delivery } = &value.citations[0] else {
            panic!("visual citation")
        };
        let frame = frames.iter().find(|v| v["id"] == -45).unwrap();
        let data = frame["result"]["contentItems"][1]["imageUrl"]
            .as_str()
            .unwrap();
        use base64::Engine;
        let received = base64::engine::general_purpose::STANDARD
            .decode(data.strip_prefix("data:image/png;base64,").unwrap())?;
        assert_eq!(
            delivery.delivered_image_sha256,
            crate::rich_text::hash(&received)
        );
        let mut wire = serde_json::to_vec(frame)?;
        wire.push(b'\n');
        assert_eq!(
            delivery.host_send.frame_sha256,
            crate::rich_text::hash(&wire)
        );
        assert_eq!(delivery.host_send.observation, "host_write_and_flush");
        if matches!(mode, Mode::Image) {
            assert_eq!(received, png);
        } else {
            let VisualDeliveryIdentity::Crop { receipt } = &delivery.image else {
                panic!("crop identity")
            };
            assert_eq!((receipt.width, receipt.height), (4, 8));
            assert_eq!(receipt.recipe.selected_page_bbox.width, 50.0);
        }
        let report = into_report(
            evidence,
            value,
            vec![],
            crate::ModelUsage::default(),
            MAX_OUTPUT,
            None,
        )?;
        assert_eq!(report.citations.len(), 1);
        assert!(serde_json::to_vec(&report)?.len() <= MAX_OUTPUT);
    }
    Ok(())
}
#[tokio::test]
async fn model_only_exposure_metadata_and_forged_refs_never_mint_image_citations() -> Result<()> {
    for mode in [Mode::Exposure, Mode::Metadata, Mode::Forged] {
        let (_temp, mut evidence, _png, id) = fixture(11).await?;
        let (outcome, _) = drive(&mut evidence, id, mode).await?;
        assert!(finish(&evidence, &outcome.answer).is_err());
    }
    Ok(())
}
#[tokio::test]
async fn same_question_different_images_and_post_delivery_tampering_stay_distinct() -> Result<()> {
    let mut hashes = vec![];
    for color in [11, 222] {
        let (temp, mut evidence, _png, id) = fixture(color).await?;
        let (outcome, _) = drive(&mut evidence, id, Mode::Image).await?;
        let value = finish(&evidence, &outcome.answer)?;
        let RichMultimodalCitation::Image { delivery } = &value.citations[0] else {
            panic!()
        };
        hashes.push(delivery.delivered_image_sha256.clone());
        fs::write(temp.path().join("source.pdf"), b"changed source")?;
        assert!(finish(&evidence, &outcome.answer).is_err());
    }
    assert_ne!(hashes[0], hashes[1]);
    Ok(())
}
#[tokio::test]
async fn replayed_rpc_ids_reject_after_one_real_delivery() -> Result<()> {
    let (_temp, mut evidence, _png, id) = fixture(11).await?;
    assert!(drive(&mut evidence, id, Mode::Replayed).await.is_err());
    Ok(())
}

#[derive(Clone, Copy)]
enum Failure {
    Partial,
    Flush,
    Source,
    Artifact,
}
struct Writer<W> {
    inner: W,
    mode: Failure,
    delivery: bool,
    failed: bool,
    source: PathBuf,
    artifact: PathBuf,
    captured: Arc<Mutex<Vec<u8>>>,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for Writer<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut PollContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if bytes.windows(12).any(|v| v == b"\"inputImage\"") {
            self.delivery = true;
        }
        if self.delivery && matches!(self.mode, Failure::Partial) {
            if self.failed {
                return Poll::Ready(Err(std::io::Error::other("synthetic partial image")));
            }
            self.failed = true;
            let n = bytes.len().min(32);
            self.captured.lock().unwrap().extend_from_slice(&bytes[..n]);
            return Poll::Ready(Ok(n));
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(n)) = &result {
            self.captured
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..*n]);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut PollContext<'_>) -> Poll<std::io::Result<()>> {
        if self.delivery && !self.failed {
            self.failed = true;
            match self.mode {
                Failure::Flush => {
                    return Poll::Ready(Err(std::io::Error::other("synthetic flush failure")));
                }
                Failure::Source => fs::write(&self.source, b"changed source").unwrap(),
                Failure::Artifact => fs::write(&self.artifact, b"changed rendition").unwrap(),
                Failure::Partial => {}
            }
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut PollContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
#[tokio::test]
async fn partial_write_failed_flush_and_stale_source_or_artifact_cannot_register() -> Result<()> {
    for mode in [
        Failure::Partial,
        Failure::Flush,
        Failure::Source,
        Failure::Artifact,
    ] {
        let (_temp, mut evidence, _png, id) = fixture(11).await?;
        let manifest = evidence.reader.manifest()?;
        let source = manifest.root.join("source.pdf");
        let artifact = manifest
            .root
            .join(".gptgrep/generations")
            .join(evidence.reader.pin().generation)
            .join(
                &manifest.documents[0].pages[0].renditions[0]
                    .artifact
                    .relative_path,
            );
        let (host, server) = tokio::io::duplex(262144);
        let (read, inner) = tokio::io::split(host);
        let captured = Arc::new(Mutex::new(vec![]));
        let writer = Writer {
            inner,
            mode,
            source,
            artifact,
            delivery: false,
            failed: false,
            captured: captured.clone(),
        };
        let task = tokio::spawn(native_fixture(server, id, Mode::Image));
        let deadline = evidence.deadline;
        let result = protocol::run_observed_budget(
            read,
            writer,
            Path::new("."),
            &config(),
            protocol::Workflow::RichMultimodal {
                question: "same neutral question",
                evidence: &mut evidence,
            },
            None,
            protocol::RunOptions::default(),
            crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES),
            deadline,
        )
        .await;
        assert!(result.is_err());
        task.abort();
        assert_eq!(evidence.delivery.as_ref().unwrap().unknown_entries(), 1);
        assert!(evidence.delivery.as_ref().unwrap().retained_bytes() >= 2_097_152);
        if !matches!(mode, Failure::Partial) {
            let bytes = captured.lock().unwrap();
            let frame = bytes
                .split(|b| *b == b'\n')
                .filter_map(|s| serde_json::from_slice::<Value>(s).ok())
                .find(|v| v["id"] == -45)
                .unwrap();
            let body: Value =
                serde_json::from_str(frame["result"]["contentItems"][0]["text"].as_str().unwrap())?;
            let raw=json!({"answer":"claim","insufficient_evidence":false,"citations":[{"kind":"image","issued_id":body["issued_id"]}]}).to_string();
            assert!(finish(&evidence, &raw).is_err());
        }
    }
    Ok(())
}

#[tokio::test]
async fn host_preflight_limits_fail_before_any_retrieval_admission() -> Result<()> {
    let (_dir, reader, _png, _id) = generation(19).await?;
    let execution = crate::rich_text::tests::context(2)?;
    let client =
        crate::rich_text::tests::client("http://127.0.0.1:9/decisions", execution.clone())?;
    let options = RichMultimodalHostOptions::default();
    let mut small_output = options.clone();
    small_output.max_output_bytes = 1;
    let mut small_working = options.clone();
    small_working.retrieval.resources.owned_working_bytes = 3 * 1_048_576;
    let mut small_input = config();
    small_input.max_input_bytes = 1024;
    for (config, options, code) in [
        (config(), small_output, "host_rich_output_budget"),
        (config(), small_working, "host_rich_visual_working_budget"),
        (small_input, options.clone(), "host_rich_input_budget"),
    ] {
        let result = prepare_rich_multimodal_host(
            reader.clone(),
            client.clone(),
            "neutral",
            &config,
            &options,
        );
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().to_string(), code);
        assert_eq!(execution.snapshot()?.logical_reservations, 0);
    }
    let client = JevClient::with_endpoint(
        "ordinary-fixture-not-a-secret",
        None,
        "http://127.0.0.1:9/decisions",
    )?;
    assert!(
        prepare_rich_multimodal_host(reader.clone(), client, "neutral", &config(), &options)
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn multimodal_host_captured_deadline_blocks_delayed_execute_before_send() -> Result<()> {
    let (_dir, reader, _png, _id) = generation(27).await?;
    let execution = crate::rich_text::tests::context(2)?;
    let (endpoint, server) = crate::rich_text::tests::mock(2, Arc::new(|_| Ok(())), false).await?;
    let mut config = config();
    config.timeout_secs = 1;
    let prepared = prepare_rich_multimodal_host(
        reader,
        crate::rich_text::tests::client(&endpoint, execution.clone())?,
        "neutral",
        &config,
        &RichMultimodalHostOptions::default(),
    )?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let result = prepared.execute(&|_| Ok(())).await;
    server.abort();
    assert!(result.is_err());
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    assert!(execution.original_run_remaining()? > Duration::from_secs(50));
    Ok(())
}

#[tokio::test]
async fn actual_rpc_mixed_text_image_citations_preserve_raw_utf8_ranges_and_order() -> Result<()> {
    let (_temp, mut evidence, _png, id) = fixture(41).await?;
    let (outcome, frames) = drive(&mut evidence, id, Mode::Mixed).await?;
    let value = finish(&evidence, &outcome.answer)?;
    assert_eq!(value.citations.len(), 2);
    assert!(matches!(
        value.citations[0],
        RichMultimodalCitation::Image { .. }
    ));
    let RichMultimodalCitation::Text { delivery } = &value.citations[1] else {
        panic!("text citation")
    };
    assert_eq!(delivery.quote, "中");
    let gptgrep_core::EvidenceRef::NativeText { binding } = &delivery.evidence else {
        panic!("native text")
    };
    assert_eq!((binding.byte_start, binding.byte_end), (7, 10));
    assert_eq!(delivery.host_send.observation, "host_write_and_flush");
    assert!(
        frames
            .iter()
            .any(|frame| frame["id"] == "text-before-image")
    );
    assert!(frames.iter().any(|frame| frame["id"] == -45));
    Ok(())
}

#[tokio::test]
async fn exhausted_complete_frame_budget_fails_before_image_reservation() -> Result<()> {
    let (_temp, mut evidence, _png, id) = fixture(73).await?;
    let result = drive(&mut evidence, id, Mode::Budget).await;
    assert!(result.is_err());
    assert_eq!(
        result.err().unwrap().to_string(),
        "host_rich_visual_transport_budget"
    );
    let registry = evidence.delivery.as_ref().unwrap();
    assert_eq!(registry.retained_bytes(), 0);
    assert_eq!(registry.unknown_entries(), 0);
    Ok(())
}
