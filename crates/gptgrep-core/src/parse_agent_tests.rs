//! Discriminating public/synthetic tests. Authored only; execution requires a
//! separately admitted run (including PDFium for the generated PDF case).
use super::*;
use serde_json::{Value, json};
use std::fs;

#[test]
fn ocr_failure_diagnostic_preserves_exact_cause_without_private_context() {
    let original = anyhow::anyhow!("OCR region coordinates mismatch")
        .context("private source path /private/canary");
    let diagnostic = summarize_ocr_failure(original);
    assert!(diagnostic.to_string().contains("cause=region_coordinates"));
    assert!(!format!("{diagnostic:#?}").contains("canary"));
    assert_eq!(diagnostic.chain().count(), 1);
    let once = diagnostic.to_string();
    let twice = summarize_ocr_failure(diagnostic.context("another private context"));
    assert_eq!(once, twice.to_string());
}

#[test]
fn ocr_failure_diagnostic_does_not_classify_substrings() {
    let error = summarize_ocr_failure(anyhow::anyhow!(
        "private payload: OCR region coordinates mismatch"
    ));
    assert!(error.to_string().contains("cause=unknown"));
    assert!(!error.to_string().contains("private payload"));
}

#[test]
fn ocr_failure_diagnostic_caps_unknown_unicode_error() {
    let raw = "中".repeat(5000);
    let error = summarize_ocr_failure(anyhow::anyhow!(raw.clone()));
    assert_eq!(
        error.to_string(),
        format!(
            "ocr_failure cause=unknown error_bytes_hashed=4096 error_sha256={:x} error_truncated=true",
            Sha256::digest(&raw.as_bytes()[..4096])
        )
    );
    assert!(!format!("{error:#?}").contains('中'));
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn text(text: &str) -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let source = root.join("source.txt");
        fs::write(&source, text)?;
        fs::create_dir(root.join("staging"))?;
        Ok(Self {
            _temporary: temporary,
            root,
            source,
        })
    }
    fn toolbox(&self, nonce: u8, budget: ResourceBudget) -> Result<ParseToolbox> {
        let mut toolbox = ParseToolbox::new(
            &self.root.join("staging"),
            [nonce; 32],
            budget,
            ToolboxLimits::default(),
        )?;
        let bytes = fs::read(&self.source)?;
        toolbox.register_source("approved", &self.source, bytes.len() as u64, &sha(&bytes))?;
        Ok(toolbox)
    }
}

async fn call(
    toolbox: &mut ParseToolbox,
    operation: ToolboxOperation,
    cap: usize,
) -> Result<Value> {
    let reply = toolbox
        .dispatch(ToolboxRequest {
            schema: REQUEST_SCHEMA.into(),
            id: "test".into(),
            max_reply_bytes: cap,
            operation,
        })
        .await?;
    ensure!(
        reply.len() <= cap,
        "test reply escaped its declared wire cap"
    );
    ensure!(
        reply.last() == Some(&b'\n'),
        "reply must include its charged NDJSON delimiter"
    );
    Ok(serde_json::from_slice(&reply)?)
}
async fn native_text(toolbox: &mut ParseToolbox) -> Result<String> {
    let prepared = call(
        toolbox,
        ToolboxOperation::Prepare {
            source: "approved".into(),
        },
        4096,
    )
    .await?;
    ensure!(
        prepared["data"]["original_pages"].is_null(),
        "unreached page count was invented"
    );
    let handle = prepared["data"]["handle"]
        .as_str()
        .context("handle missing")?
        .to_string();
    call(
        toolbox,
        ToolboxOperation::ExtractNative {
            handle: handle.clone(),
            page_window: 25,
            dpi: 96.0,
            render_pages: false,
        },
        4096,
    )
    .await?;
    Ok(handle)
}
async fn artifacts(toolbox: &mut ParseToolbox, handle: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let reply = call(
            toolbox,
            ToolboxOperation::InspectArtifacts {
                handle: handle.into(),
                cursor,
                page_size: 1,
            },
            4096,
        )
        .await?;
        rows.extend(
            reply["data"]["rows"]
                .as_array()
                .context("artifact rows missing")?
                .iter()
                .cloned(),
        );
        cursor = reply["data"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(rows)
}

#[tokio::test]
async fn handles_are_invocation_owned_and_json_cannot_admit_paths_or_paid_results() -> Result<()> {
    let fixture = Fixture::text("local source\n")?;
    let budget = ResourceBudget::default();
    // Even repeated host nonces do not make same-process invocations share handles.
    let mut first = fixture.toolbox(7, budget.clone())?;
    let mut second = fixture.toolbox(7, budget)?;
    let first_handle = native_text(&mut first).await?;
    let second_handle = native_text(&mut second).await?;
    assert_ne!(first_handle, second_handle);
    assert!(
        call(
            &mut second,
            ToolboxOperation::InspectPages {
                handle: first_handle,
                cursor: None,
                page_size: 1
            },
            4096
        )
        .await
        .is_err()
    );
    let forbidden = json!({"schema":REQUEST_SCHEMA,"id":"safe","max_reply_bytes":4096,
        "operation":{"op":"prepare","source":"approved","path":"/not-admitted"}});
    let reply = first
        .dispatch_json(&serde_json::to_vec(&forbidden)?)
        .await?;
    let reply: Value = serde_json::from_slice(&reply)?;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"]["code"], "request_schema");
    let forbidden = json!({"schema":REQUEST_SCHEMA,"id":"safe","max_reply_bytes":4096,
        "operation":{"op":"submit_ocr_result","handle":second_handle,"result":{}}});
    let reply = second
        .dispatch_json(&serde_json::to_vec(&forbidden)?)
        .await?;
    assert_eq!(serde_json::from_slice::<Value>(&reply)?["ok"], false);
    Ok(())
}

#[tokio::test]
async fn escaped_unicode_windows_reassemble_exact_source_under_complete_frame_cap() -> Result<()> {
    let source = "\\\"\u{0}é中\n".repeat(1400);
    let fixture = Fixture::text(&source)?;
    let mut toolbox = fixture.toolbox(1, ResourceBudget::default())?;
    let handle = native_text(&mut toolbox).await?;
    let rows = artifacts(&mut toolbox, &handle).await?;
    let artifact = rows
        .iter()
        .find(|row| row["kind"] == "native_text")
        .context("text artifact missing")?;
    let artifact_id = artifact["id"]
        .as_str()
        .context("artifact id missing")?
        .to_string();
    let mut cursor = None;
    let mut reconstructed = String::new();
    let mut count = 0;
    loop {
        let request = ToolboxRequest {
            schema: REQUEST_SCHEMA.into(),
            id: "\\\"escaped".into(),
            max_reply_bytes: 2048,
            operation: ToolboxOperation::ReadArtifact {
                handle: handle.clone(),
                artifact_id: artifact_id.clone(),
                cursor,
                max_bytes: 16_384,
            },
        };
        let raw = toolbox.dispatch(request).await?;
        assert!(raw.len() <= 2048);
        let reply: Value = serde_json::from_slice(&raw)?;
        let data = &reply["data"];
        assert_eq!(data["offset"].as_u64(), Some(reconstructed.len() as u64));
        let window = data["text"].as_str().context("window missing")?;
        assert!(!window.is_empty());
        reconstructed.push_str(window);
        count += 1;
        cursor = data["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            assert_eq!(data["eof"], true);
            break;
        }
        assert_eq!(data["eof"], false);
        assert!(count < source.len());
    }
    assert!(count > 1);
    assert_eq!(reconstructed, source);
    assert_eq!(
        sha(reconstructed.as_bytes()),
        artifact["sha256"].as_str().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn all_metadata_bytes_are_reachable_and_tampering_refuses_continuation() -> Result<()> {
    let fixture = Fixture::text("source lines\nsecond line\n")?;
    let mut toolbox = fixture.toolbox(2, ResourceBudget::default())?;
    let handle = native_text(&mut toolbox).await?;
    let rows = artifacts(&mut toolbox, &handle).await?;
    let metadata = rows
        .iter()
        .find(|row| row["kind"] == "native_metadata")
        .context("metadata missing")?;
    let id = metadata["id"].as_str().unwrap().to_string();
    let first = call(
        &mut toolbox,
        ToolboxOperation::ReadMetadataWindow {
            handle: handle.clone(),
            artifact_id: id.clone(),
            cursor: None,
            max_bytes: 7,
        },
        4096,
    )
    .await?;
    let cursor = first["data"]["next_cursor"]
        .as_str()
        .context("metadata must continue")?
        .to_string();
    let mut complete = first["data"]["text"].as_str().unwrap().to_string();
    let mut next = Some(cursor.clone());
    while let Some(token) = next {
        let reply = call(
            &mut toolbox,
            ToolboxOperation::ReadMetadataWindow {
                handle: handle.clone(),
                artifact_id: id.clone(),
                cursor: Some(token),
                max_bytes: 7,
            },
            4096,
        )
        .await?;
        complete.push_str(reply["data"]["text"].as_str().unwrap());
        next = reply["data"]["next_cursor"].as_str().map(str::to_string);
    }
    let metadata_value: Value = serde_json::from_str(&complete)?;
    assert_eq!(
        metadata_value["source_sha256"],
        sha(b"source lines\nsecond line\n")
    );
    assert_eq!(metadata_value["lines"], 2);
    assert_eq!(
        sha(complete.as_bytes()),
        metadata["sha256"].as_str().unwrap()
    );
    let first = call(
        &mut toolbox,
        ToolboxOperation::ReadMetadataWindow {
            handle: handle.clone(),
            artifact_id: id.clone(),
            cursor: None,
            max_bytes: 7,
        },
        4096,
    )
    .await?;
    let cursor = first["data"]["next_cursor"].as_str().unwrap().to_string();
    let root = toolbox.prepared_staging_root(&handle)?.to_path_buf();
    let artifact_path = root.join(metadata["relative_path"].as_str().unwrap());
    let original = fs::read(&artifact_path)?;
    let mut changed = original.clone();
    changed[0] ^= 1;
    fs::write(&artifact_path, &changed)?;
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::ReadMetadataWindow {
                handle: handle.clone(),
                artifact_id: id,
                cursor: Some(cursor),
                max_bytes: 7
            },
            4096
        )
        .await
        .is_err()
    );
    fs::write(&artifact_path, original)?;
    fs::write(&fixture.source, b"changed original source")?;
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::InspectPages {
                handle,
                cursor: None,
                page_size: 1
            },
            4096
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn close_invalidates_handles_and_cursors_without_refunding_shared_staging() -> Result<()> {
    let fixture = Fixture::text(&"retained data\n".repeat(200))?;
    let budget = ResourceBudget::default();
    let mut toolbox = fixture.toolbox(3, budget.clone())?;
    let handle = native_text(&mut toolbox).await?;
    let rows = artifacts(&mut toolbox, &handle).await?;
    let artifact = rows
        .iter()
        .find(|row| row["kind"] == "native_text")
        .unwrap();
    let artifact_id = artifact["id"].as_str().unwrap().to_string();
    let reply = call(
        &mut toolbox,
        ToolboxOperation::ReadArtifact {
            handle: handle.clone(),
            artifact_id: artifact_id.clone(),
            cursor: None,
            max_bytes: 16,
        },
        4096,
    )
    .await?;
    let cursor = reply["data"]["next_cursor"].as_str().unwrap().to_string();
    let root = toolbox.prepared_staging_root(&handle)?.to_path_buf();
    let retained = budget.staging_used();
    assert!(retained > 0);
    call(
        &mut toolbox,
        ToolboxOperation::Close {
            handle: handle.clone(),
        },
        4096,
    )
    .await?;
    assert!(root.is_dir());
    assert_eq!(budget.staging_used(), retained);
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::ReadArtifact {
                handle,
                artifact_id,
                cursor: Some(cursor),
                max_bytes: 16
            },
            4096
        )
        .await
        .is_err()
    );
    let new_handle = native_text(&mut toolbox).await?;
    assert!(!new_handle.is_empty());
    assert!(budget.staging_used() > retained);
    let mut second = fixture.toolbox(4, budget.clone())?;
    let before = budget.staging_used();
    native_text(&mut second).await?;
    assert!(budget.staging_used() > before);
    Ok(())
}

#[tokio::test]
async fn exhausted_original_shared_budget_cannot_be_reset_by_prepare_or_constructor() -> Result<()>
{
    let fixture = Fixture::text("must not bypass the shared reservation\n")?;
    let budget = ResourceBudget::default();
    budget.reserve_retained_staging(budget.run_staging_bytes)?;
    let mut first = fixture.toolbox(5, budget.clone())?;
    let mut second = fixture.toolbox(6, budget.clone())?;
    for toolbox in [&mut first, &mut second] {
        assert!(
            call(
                toolbox,
                ToolboxOperation::Prepare {
                    source: "approved".into()
                },
                4096
            )
            .await
            .is_err()
        );
    }
    assert_eq!(budget.staging_used(), budget.run_staging_bytes);
    Ok(())
}

/// Public synthetic PDF, generated entirely from declared page ordinals.
/// Twenty-six pages discriminate a processing window from a document cap.
fn generated_pdf(pages: usize) -> Vec<u8> {
    let font_id = 3 + pages * 2;
    let kids = (0..pages)
        .map(|n| format!("{} 0 R", 3 + n * 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Count {pages} /Kids [{kids}] >>"),
    ];
    for page in 0..pages {
        let content = format!("BT /F1 12 Tf 10 40 Td (page {}) Tj ET", page + 1);
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 80] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {} 0 R >>", 4 + page * 2));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            content
        ));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    let mut output = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(output.len());
        output.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
    }
    let xref = output.len();
    output
        .extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    output.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    output
}

#[tokio::test]
async fn whole_original_document_survives_native_windows_and_paged_inspection() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    fs::create_dir(root.join("staging"))?;
    let source = root.join("generated.pdf");
    let bytes = generated_pdf(26);
    fs::write(&source, &bytes)?;
    let mut toolbox = ParseToolbox::new(
        &root.join("staging"),
        [8; 32],
        ResourceBudget::default(),
        ToolboxLimits::default(),
    )?;
    toolbox.register_source("approved", &source, bytes.len() as u64, &sha(&bytes))?;
    let handle = native_text(&mut toolbox).await?;
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let reply = call(
            &mut toolbox,
            ToolboxOperation::InspectPages {
                handle: handle.clone(),
                cursor,
                page_size: 3,
            },
            32_768,
        )
        .await?;
        assert_eq!(reply["data"]["total"], 26);
        for page in reply["data"]["rows"].as_array().unwrap() {
            pages.push(page["number"].as_u64().unwrap());
        }
        cursor = reply["data"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(pages, (1..=26).collect::<Vec<u64>>());
    let mut cursor = None;
    let mut coverage = Vec::new();
    loop {
        let reply = call(
            &mut toolbox,
            ToolboxOperation::InspectOcrPlan {
                handle: handle.clone(),
                section: OcrPlanSection::Pages,
                cursor,
                page_size: 3,
            },
            4096,
        )
        .await?;
        assert_eq!(reply["data"]["total"], 26);
        for row in reply["data"]["rows"].as_array().unwrap() {
            coverage.push(row[0].as_u64().unwrap());
        }
        cursor = reply["data"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(coverage, pages);
    let mut cursor = None;
    let mut blockers = 0;
    let blocker_total = loop {
        let reply = call(
            &mut toolbox,
            ToolboxOperation::InspectOcrPlan {
                handle: handle.clone(),
                section: OcrPlanSection::Blockers,
                cursor,
                page_size: 3,
            },
            4096,
        )
        .await?;
        blockers += reply["data"]["rows"].as_array().unwrap().len();
        cursor = reply["data"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break reply["data"]["total"].as_u64().unwrap() as usize;
        }
    };
    assert_eq!(blockers, blocker_total);
    let reply = call(
        &mut toolbox,
        ToolboxOperation::Finalize {
            handle: handle.clone(),
            mode: OcrMode::Off,
        },
        4096,
    )
    .await?;
    assert_eq!(reply["data"]["original_pages"], 26);
    let finalized = toolbox.finalized_document(&handle)?;
    assert_eq!(finalized.pages.len(), 26);
    assert!(finalized.document.text.contains("page 26"));
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::Finalize {
                handle,
                mode: OcrMode::Auto
            },
            4096
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn complete_images_and_crops_refuse_smaller_wire_frames_instead_of_returning_prefixes()
-> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    fs::create_dir(root.join("staging"))?;
    let source = root.join("generated.png");
    let mut state = 0x8b8b_2d17_u32;
    let pixels = image::RgbaImage::from_fn(96, 64, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgba([state as u8, (state >> 8) as u8, (state >> 16) as u8, 255])
    });
    pixels.save(&source)?;
    let bytes = fs::read(&source)?;
    let mut toolbox = ParseToolbox::new(
        &root.join("staging"),
        [9; 32],
        ResourceBudget::default(),
        ToolboxLimits::default(),
    )?;
    toolbox.register_source("approved", &source, bytes.len() as u64, &sha(&bytes))?;
    let prepared = call(
        &mut toolbox,
        ToolboxOperation::Prepare {
            source: "approved".into(),
        },
        4096,
    )
    .await?;
    let handle = prepared["data"]["handle"].as_str().unwrap().to_string();
    call(
        &mut toolbox,
        ToolboxOperation::ExtractNative {
            handle: handle.clone(),
            page_window: 25,
            dpi: 96.0,
            render_pages: true,
        },
        4096,
    )
    .await?;
    let reply = call(
        &mut toolbox,
        ToolboxOperation::InspectImages {
            handle: handle.clone(),
            page: Some(1),
            cursor: None,
            page_size: 1,
        },
        8192,
    )
    .await?;
    let image = &reply["data"]["rows"][0];
    let id = image["artifact"]["id"].as_str().unwrap().to_string();
    let complete = call(
        &mut toolbox,
        ToolboxOperation::ReadArtifact {
            handle: handle.clone(),
            artifact_id: id.clone(),
            cursor: None,
            max_bytes: 1,
        },
        MAX_REPLY,
    )
    .await?;
    assert_eq!(complete["data"]["kind"], "complete_image");
    assert_eq!(complete["data"]["width"], image["width"]);
    assert_eq!(complete["data"]["height"], image["height"]);
    let data_url = complete["data"]["data_url"].as_str().unwrap();
    assert!(data_url.starts_with("data:image/png;base64,iVBORw0KGgo"));
    let encoded = data_url.split_once(',').unwrap().1;
    let expected = image["artifact"]["bytes"].as_u64().unwrap().div_ceil(3) * 4;
    assert_eq!(encoded.len() as u64, expected);
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::ReadArtifact {
                handle: handle.clone(),
                artifact_id: id.clone(),
                cursor: None,
                max_bytes: 1
            },
            1024
        )
        .await
        .is_err()
    );
    let pages = call(
        &mut toolbox,
        ToolboxOperation::InspectPages {
            handle: handle.clone(),
            cursor: None,
            page_size: 1,
        },
        16_384,
    )
    .await?;
    let page = &pages["data"]["rows"][0];
    let bbox = BoundingBox {
        x: 0.0,
        y: 0.0,
        width: page["width_points"].as_f64().unwrap(),
        height: page["height_points"].as_f64().unwrap(),
    };
    let crop = call(
        &mut toolbox,
        ToolboxOperation::Crop {
            handle: handle.clone(),
            artifact_id: id.clone(),
            bbox,
        },
        MAX_REPLY,
    )
    .await?;
    assert_eq!(crop["data"]["kind"], "complete_crop");
    assert_eq!(crop["data"]["recipe"]["parent_artifact_id"], id);
    assert_eq!(crop["data"]["recipe"]["pixels"]["width"], image["width"]);
    assert_eq!(crop["data"]["recipe"]["pixels"]["height"], image["height"]);
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::Crop {
                handle,
                artifact_id: id,
                bbox
            },
            1024
        )
        .await
        .is_err()
    );
    Ok(())
}

struct FailingExecutor {
    calls: AtomicU64,
}
impl OcrExecutor for FailingExecutor {
    fn execute<'a>(
        &'a self,
        _task: OcrTask,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OcrPageResult>> + Send + 'a>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(anyhow::anyhow!("synthetic executor failure")) })
    }
}

#[tokio::test]
async fn an_executor_failure_cannot_renew_the_registered_page_submission() -> Result<()> {
    let mut fixture = Fixture::text("")?;
    fixture.source = fixture.root.join("generated.png");
    image::RgbaImage::from_fn(32, 32, |x, y| {
        let value = if (x + y) % 2 == 0 { 0 } else { 255 };
        image::Rgba([value, value, value, 255])
    })
    .save(&fixture.source)?;
    let mut toolbox = fixture.toolbox(10, ResourceBudget::default())?;
    let prepared = call(
        &mut toolbox,
        ToolboxOperation::Prepare {
            source: "approved".into(),
        },
        4096,
    )
    .await?;
    let handle = prepared["data"]["handle"].as_str().unwrap().to_string();
    call(
        &mut toolbox,
        ToolboxOperation::ExtractNative {
            handle: handle.clone(),
            page_window: 25,
            dpi: 96.0,
            render_pages: true,
        },
        4096,
    )
    .await?;
    let plan = call(
        &mut toolbox,
        ToolboxOperation::InspectOcrPlan {
            handle: handle.clone(),
            section: OcrPlanSection::Tasks,
            cursor: None,
            page_size: 1,
        },
        65_536,
    )
    .await?;
    let task = plan["data"]["rows"][0]["task_sha256"]
        .as_str()
        .context("image page must require OCR")?;
    let executor = FailingExecutor {
        calls: AtomicU64::new(0),
    };
    assert!(toolbox.execute_ocr(&handle, task, &executor).await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(toolbox.execute_ocr(&handle, task, &executor).await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::Finalize {
                handle: handle.clone(),
                mode: OcrMode::Auto
            },
            4096
        )
        .await
        .is_err()
    );
    call(
        &mut toolbox,
        ToolboxOperation::Close {
            handle: handle.clone(),
        },
        4096,
    )
    .await?;
    assert!(toolbox.execute_ocr(&handle, task, &executor).await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn typed_transfer_invalidates_capabilities_and_preserves_original_shared_staging()
-> Result<()> {
    let fixture = Fixture::text(&"ownership transfer\n".repeat(80))?;
    let budget = ResourceBudget::default();
    let mut toolbox = fixture.toolbox(12, budget.clone())?;
    let handle = native_text(&mut toolbox).await?;
    assert!(toolbox.take_finalized_document(&handle).is_err());
    call(
        &mut toolbox,
        ToolboxOperation::Finalize {
            handle: handle.clone(),
            mode: OcrMode::Off,
        },
        4096,
    )
    .await?;
    let rows = artifacts(&mut toolbox, &handle).await?;
    let artifact = rows
        .iter()
        .find(|row| row["kind"] == "native_text")
        .context("text missing")?;
    let id = artifact["id"].as_str().context("id missing")?.to_string();
    let reply = call(
        &mut toolbox,
        ToolboxOperation::ReadArtifact {
            handle: handle.clone(),
            artifact_id: id.clone(),
            cursor: None,
            max_bytes: 7,
        },
        4096,
    )
    .await?;
    let cursor = reply["data"]["next_cursor"]
        .as_str()
        .context("continuation missing")?
        .to_string();
    let root = toolbox.prepared_staging_root(&handle)?.to_path_buf();
    let artifact_count = toolbox.finalized_document(&handle)?.artifacts.len();
    let used = toolbox.staging_used();
    let transferred_budget = toolbox.resource_budget();
    let parsed = toolbox.take_finalized_document(&handle)?;
    assert_eq!(parsed.artifacts.len(), artifact_count);
    assert_eq!(
        parsed.summary.source_sha256,
        sha(fs::read(&fixture.source)?.as_slice())
    );
    assert!(root.is_dir());
    assert_eq!(budget.staging_used(), used);
    assert_eq!(transferred_budget.staging_used(), used);
    assert!(toolbox.finalized_document(&handle).is_err());
    assert!(toolbox.take_finalized_document(&handle).is_err());
    assert!(
        call(
            &mut toolbox,
            ToolboxOperation::ReadArtifact {
                handle,
                artifact_id: id,
                cursor: Some(cursor),
                max_bytes: 7
            },
            4096
        )
        .await
        .is_err()
    );
    transferred_budget.reserve_retained_staging(1)?;
    assert_eq!(budget.staging_used(), used + 1);
    let second = native_text(&mut toolbox).await?;
    assert!(!second.is_empty());
    assert!(budget.staging_used() > used + 1);
    Ok(())
}

#[tokio::test]
async fn typed_transfer_rechecks_source_and_all_artifacts_before_moving() -> Result<()> {
    let fixture = Fixture::text("fresh typed output\n")?;
    let mut toolbox = fixture.toolbox(13, ResourceBudget::default())?;
    let handle = native_text(&mut toolbox).await?;
    call(
        &mut toolbox,
        ToolboxOperation::Finalize {
            handle: handle.clone(),
            mode: OcrMode::Off,
        },
        4096,
    )
    .await?;
    let original_source = fs::read(&fixture.source)?;
    let used = toolbox.staging_used();
    fs::write(&fixture.source, b"changed source")?;
    assert!(toolbox.take_finalized_document(&handle).is_err());
    assert_eq!(toolbox.staging_used(), used);
    fs::write(&fixture.source, original_source)?;
    let rows = artifacts(&mut toolbox, &handle).await?;
    let metadata = rows
        .iter()
        .find(|row| row["kind"] == "native_metadata")
        .context("metadata missing")?;
    let path = toolbox
        .prepared_staging_root(&handle)?
        .join(metadata["relative_path"].as_str().context("path missing")?);
    let original = fs::read(&path)?;
    let mut changed = original.clone();
    changed[0] ^= 1;
    fs::write(&path, changed)?;
    assert!(toolbox.take_finalized_document(&handle).is_err());
    assert_eq!(toolbox.staging_used(), used);
    fs::write(&path, original)?;
    assert!(toolbox.finalized_document(&handle).is_ok());
    let parsed = toolbox.take_finalized_document(&handle)?;
    assert_eq!(parsed.summary.source_sha256, sha(b"fresh typed output\n"));
    assert_eq!(toolbox.staging_used(), used);
    Ok(())
}

async fn large_native_rendition_fixture() -> Result<(
    tempfile::TempDir,
    PathBuf,
    ParseToolbox,
    String,
    ArtifactRef,
)> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    fs::create_dir(root.join("staging"))?;
    let source = root.join("synthetic-noise.png");
    let mut state = 0x5a91_7c23_u32;
    image::RgbaImage::from_fn(512, 256, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgba([state as u8, (state >> 8) as u8, (state >> 16) as u8, 255])
    })
    .save(&source)?;
    let source_bytes = fs::read(&source)?;
    let mut toolbox = ParseToolbox::new(
        &root.join("staging"),
        [19; 32],
        ResourceBudget::default(),
        ToolboxLimits {
            reply_bytes: 262_144,
            ..ToolboxLimits::default()
        },
    )?;
    toolbox.register_source(
        "approved",
        &source,
        source_bytes.len() as u64,
        &sha(&source_bytes),
    )?;
    let prepared = call(
        &mut toolbox,
        ToolboxOperation::Prepare {
            source: "approved".into(),
        },
        4096,
    )
    .await?;
    let handle = prepared["data"]["handle"].as_str().unwrap().to_string();
    call(
        &mut toolbox,
        ToolboxOperation::ExtractNative {
            handle: handle.clone(),
            page_window: 25,
            dpi: 96.0,
            render_pages: true,
        },
        4096,
    )
    .await?;
    let row = artifacts(&mut toolbox, &handle)
        .await?
        .into_iter()
        .find(|row| row["kind"] == "rendition")
        .context("synthetic rendition missing")?;
    let artifact: ArtifactRef = serde_json::from_value(row)?;
    assert!(artifact.bytes.div_ceil(3) * 4 > 262_144);
    assert!(artifact.bytes < 1_048_576);
    Ok((temporary, source, toolbox, handle, artifact))
}

#[tokio::test]
async fn controller_image_handoff_is_complete_while_legacy_json_stays_capped() -> Result<()> {
    let (_temporary, _source, mut toolbox, handle, artifact) =
        large_native_rendition_fixture().await?;
    let error = call(
        &mut toolbox,
        ToolboxOperation::ReadArtifact {
            handle: handle.clone(),
            artifact_id: artifact.id.clone(),
            cursor: None,
            max_bytes: 1,
        },
        262_144,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("parse_toolbox_complete_image_reply_limit")
    );
    let expected = fs::read(
        toolbox
            .prepared_staging_root(&handle)?
            .join(&artifact.relative_path),
    )?;
    let used = toolbox.staging_used();
    let copied = toolbox.with_verified_rendition(&handle, &artifact.id, |image, bytes| {
        assert!(image.width > 0 && image.height > 0);
        assert_eq!(bytes.len() as u64, artifact.bytes);
        assert_eq!(sha(bytes), artifact.sha256);
        Ok(bytes.to_vec())
    })?;
    assert_eq!(copied, expected);
    assert_eq!(toolbox.staging_used(), used); // The controller owns retained-copy admission.
    Ok(())
}

#[tokio::test]
async fn controller_image_handoff_refuses_tamper_and_checks_the_held_fd_after_copy() -> Result<()> {
    let (_temporary, source, toolbox, handle, artifact) = large_native_rendition_fixture().await?;
    let path = toolbox
        .prepared_staging_root(&handle)?
        .join(&artifact.relative_path);
    let original = fs::read(&path)?;
    let mut altered = original.clone();
    let middle = altered.len() / 2;
    altered[middle] ^= 1;
    fs::write(&path, altered)?;
    assert!(
        toolbox
            .with_verified_rendition(&handle, &artifact.id, |_, _| -> Result<()> {
                panic!("tampered image reached controller")
            })
            .is_err()
    );
    fs::write(&path, &original)?;
    let changed_entry = path.with_extension("retained-original");
    let error = toolbox
        .with_verified_rendition(&handle, &artifact.id, |_, bytes| {
            fs::rename(&path, &changed_entry)?;
            fs::write(&path, bytes)?; // Identical bytes, different inode during the held copy.
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("custody_file_changed"));
    let error = toolbox
        .with_verified_rendition(&handle, &artifact.id, |_, _| {
            fs::write(&source, b"changed original synthetic source")?;
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("custody_"));
    assert!(
        toolbox
            .with_verified_rendition(&handle, &artifact.id, |_, _| -> Result<()> {
                panic!("stale source reached controller")
            })
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn controller_image_handoff_charges_working_before_copy_and_keeps_image_cap() -> Result<()> {
    let (_temporary, _source, mut toolbox, handle, artifact) =
        large_native_rendition_fixture().await?;
    let limit = toolbox.limits.operation_working_bytes;
    toolbox.limits.operation_working_bytes =
        toolbox.doc(&handle)?.side_retained()? + artifact.bytes + 65_536;
    let before = toolbox.staging_used();
    let error = toolbox
        .with_verified_rendition(&handle, &artifact.id, |_, _| -> Result<()> {
            panic!("working refusal reached controller")
        })
        .unwrap_err();
    assert!(error.to_string().contains("parse_toolbox_working_limit"));
    assert_eq!(toolbox.staging_used(), before);
    toolbox.limits.operation_working_bytes = limit;
    toolbox.parser_budget.image_file_bytes = artifact.bytes - 1;
    assert!(
        toolbox
            .with_verified_rendition(&handle, &artifact.id, |_, _| -> Result<()> {
                panic!("image cap refusal reached controller")
            })
            .is_err()
    );
    assert_eq!(toolbox.staging_used(), before);
    Ok(())
}
