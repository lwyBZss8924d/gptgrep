use anyhow::Result;
use gptgrep_parse::*;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn invoke(root: &std::path::Path, request: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .args(["batch", "--rich", "--root"])
        .arg(root)
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}
fn report(output: Output) -> Value {
    assert!(output.stdout.len() <= 1_048_576);
    assert_ne!(output.stdout.last(), Some(&b'\n'));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "CLI JSON: {e}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
fn request(operation: Value) -> Value {
    json!({"schema_version":"gptgrep.batch.rich.request.v1","operations":[operation]})
}
fn offline_ocr(task: &OcrTask) -> Result<OcrPageResult> {
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
                text: "Authored offline CLI transcription.".into(),
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
                    native: None,
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    })
}
#[tokio::test]
async fn native_metadata_actual_cli_discovery_to_payload_pagination_and_errors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let source = root.join("authored-graphics.pdf");
    std::fs::write(
        &source,
        include_bytes!("../../../tests/fixtures/native-graphics.pdf"),
    )?;
    let budget = ResourceBudget::default();
    let prepared =
        prepare_document(&source, &root.join("staging"), &PreparationOptions, &budget).await?;
    let candidate = extract_native(
        &prepared,
        &NativeOptions {
            dpi: 72.,
            render_pages: true,
            page_window: 1,
        },
        &budget,
    )
    .await?;
    let outcomes = candidate
        .plan()
        .tasks
        .iter()
        .map(offline_ocr)
        .collect::<Result<Vec<_>>>()?;
    let parsed = finalize_document(candidate, outcomes, OcrMode::Auto, &budget)?;
    let mut writer = gptgrep_core::RichGenerationWriter::begin(&root, budget)?;
    writer.add_document("authored-graphics.pdf", prepared.staging_root(), parsed)?;
    let pin = writer.publish()?;
    let discovery = request(
        json!({"id":"pages","op":"inspect_pages","document":"authored-graphics.pdf","fields":["page","metadata_artifact","native_text"]}),
    );
    let output = invoke(&root, &discovery);
    assert!(output.status.success(), "{:?}", output);
    let pages = report(output);
    assert_eq!(
        pages["items"][0]["result"]["source_status"],
        "indexed_metadata_only"
    );
    let artifact = pages["items"][0]["result"]["objects"][0]["metadata_artifact"]["id"]
        .as_str()
        .unwrap();
    let native_text = pages["items"][0]["result"]["objects"][0]["native_text"]["id"]
        .as_str()
        .unwrap();
    let mut op = json!({"id":"metadata","op":"inspect_native_metadata","artifact_id":artifact,"collection":"object_summaries",
        "fields":["geometry","content_bounds","counts"],"item_fields":["id","kind","segment_count","page_bounds"],"page_size":3,"max_output_bytes":16384});
    let output = invoke(&root, &request(op.clone()));
    assert!(output.status.success(), "{:?}", output);
    let first = report(output);
    assert_eq!(first["generation"], serde_json::to_value(&pin)?);
    assert_eq!(
        first["items"][0]["result"]["schema_version"],
        "gptgrep.native_metadata.inspection.v1"
    );
    assert_eq!(first["items"][0]["result"]["metadata_only"], true);
    assert_eq!(first["items"][0]["result"]["citation_authority"], false);
    assert_eq!(first["items"][0]["result"]["counts"]["objects"], 7);
    assert_eq!(
        first["items"][0]["result"]["content_bounds"]["semantics"],
        "native_reported_top_level_unclipped_geometric_union"
    );
    assert_eq!(first["metrics"]["model_submissions"], 0);
    let cursor = first["items"][0]["coverage"]["cursor"].as_str().unwrap();
    op["cursor"] = json!(cursor);
    let second = report(invoke(&root, &request(op.clone())));
    assert_eq!(second["items"][0]["coverage"]["start"], 3);
    let path = second["items"][0]["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "path")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let segments = request(
        json!({"id":"segments","op":"inspect_native_metadata","artifact_id":artifact,"collection":"path_segments",
        "object_id":path,"fields":["counts"],"item_fields":["ordinal","raw_point","page_point"],"page_size":1,"max_output_bytes":16384}),
    );
    let output = invoke(&root, &segments);
    assert!(output.status.success(), "{:?}", output);
    let segment = report(output);
    assert_eq!(
        segment["items"][0]["result"]["rows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(segment["items"][0]["result"]["rows"][0]["page_point"].is_array());
    let mut bad = op.clone();
    bad["fields"] = json!(["not_a_native_field"]);
    let output = invoke(&root, &request(bad));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(report(output)["error"]["code"], "invalid_fields");
    let mut bad = op.clone();
    bad["page_size"] = json!(4);
    assert_eq!(
        report(invoke(&root, &request(bad)))["items"][0]["error"]["code"],
        "invalid_cursor"
    );
    let mut tiny = op.clone();
    tiny.as_object_mut().unwrap().remove("cursor");
    tiny["max_output_bytes"] = json!(256);
    let tiny = report(invoke(&root, &request(tiny)));
    assert_eq!(
        tiny["items"][0]["error"]["code"],
        "output_budget_insufficient"
    );
    assert!(tiny["items"][0]["coverage"].get("cursor").is_none());
    let wrong = request(json!({"id":"wrong","op":"read_artifact","artifact_id":artifact}));
    assert_eq!(
        report(invoke(&root, &wrong))["items"][0]["error"]["code"],
        "invalid_operation"
    );
    let text =
        request(json!({"id":"text","op":"read_artifact","artifact_id":native_text,"max_bytes":64}));
    let text = report(invoke(&root, &text));
    assert_eq!(
        text["items"][0]["result"]["evidence"]["kind"],
        "native_text"
    );
    std::fs::write(&source, b"changed source")?;
    assert_eq!(
        report(invoke(&root, &request(op)))["items"][0]["error"]["code"],
        "source_stale"
    );
    Ok(())
}
#[test]
fn native_metadata_cli_schema_and_llms_describe_local_closed_collections() {
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("--schema")
        .output()
        .unwrap();
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    let metadata = &schema["commands"]["batch"]["rich"]["inspect_native_metadata"];
    assert_eq!(metadata["model_required"], false);
    assert!(
        metadata["collections"]["path_segments"]
            .as_array()
            .unwrap()
            .contains(&json!("page_point"))
    );
    assert!(
        metadata["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("content_bounds"))
    );
    let output = Command::new(env!("CARGO_BIN_EXE_gptgrep"))
        .arg("--llms")
        .output()
        .unwrap();
    assert!(output.status.success());
    let llms = String::from_utf8(output.stdout).unwrap();
    assert!(
        llms.contains("inspect_native_metadata")
            && llms.contains("metadata_only")
            && llms.contains("rebuild_required")
    );
}

#[tokio::test]
async fn native_metadata_prepare_real_wrapper_fixture() -> Result<()> {
    let Some(destination) = std::env::var_os("GPTGREP_NATIVE_INTEGRATION_ROOT") else {
        eprintln!(
            "native_metadata_prepare_real_wrapper_fixture: not requested; real wrapper TAP remains explicitly skipped"
        );
        return Ok(());
    };
    let root = std::path::PathBuf::from(destination);
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    anyhow::ensure!(
        !root.join(".gptgrep").exists() && !root.join("fixture-binding.json").exists(),
        "wrapper fixture already retained; use a new admitted directory"
    );
    let source = root.join("authored-graphics.pdf");
    std::fs::write(
        &source,
        include_bytes!("../../../tests/fixtures/native-graphics.pdf"),
    )?;
    let budget = ResourceBudget::default();
    let prepared =
        prepare_document(&source, &root.join("staging"), &PreparationOptions, &budget).await?;
    let candidate = extract_native(
        &prepared,
        &NativeOptions {
            dpi: 72.,
            render_pages: true,
            page_window: 1,
        },
        &budget,
    )
    .await?;
    let outcomes = candidate
        .plan()
        .tasks
        .iter()
        .map(offline_ocr)
        .collect::<Result<Vec<_>>>()?;
    let parsed = finalize_document(candidate, outcomes, OcrMode::Auto, &budget)?;
    let source_sha256 = parsed.summary.source_sha256.clone();
    let mut writer = gptgrep_core::RichGenerationWriter::begin(&root, budget)?;
    writer.add_document("authored-graphics.pdf", prepared.staging_root(), parsed)?;
    let pin = writer.publish()?;
    gptgrep_core::RichGenerationReader::open_pinned(&root, &pin)?.verify_all()?;
    let receipt = json!({"schema":"gptgrep.synthetic.native_wrapper_fixture.v1","source_relative":"authored-graphics.pdf",
        "source_sha256":source_sha256,"generation":pin,"synthetic_offline_ocr":true,
        "provenance":"actual native PDF extraction and library publication; declared synthetic OCR receipt; no model or index--rich OCR success claim"});
    std::fs::write(
        root.join("fixture-binding.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(())
}
