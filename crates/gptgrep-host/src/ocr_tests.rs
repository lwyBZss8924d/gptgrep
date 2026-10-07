//! Offline adversarial replay: these tests never launch a provider or a Codex runtime.
use crate::{
    ocr::{PreparedPage, hash},
    ocr_accounting::OcrExecutionContext,
    ocr_protocol,
};
use gptgrep_parse::{
    AffineTransform, ArtifactKind, ArtifactRef, BoundingBox, OcrImage, OcrProfile, OcrRegion,
    OcrTask, OcrTaskBudget, artifact_id, ocr_task_digest,
};
use image::ImageEncoder;
use serde_json::{Value, json};
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, SystemTime},
};
use tokio::io::{AsyncWrite, BufReader};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub task: OcrTask,
    pub image: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(
            root.join("source.pdf"),
            b"synthetic source bytes, not a PDF parser fixture",
        )
        .unwrap();
        let mut image = Vec::new();
        image::codecs::png::PngEncoder::new(&mut image)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        fs::create_dir(root.join("images")).unwrap();
        fs::write(root.join("images/page.png"), &image).unwrap();
        let source_sha256 = hash(&fs::read(root.join("source.pdf")).unwrap());
        let content_sha256 = hash(&image);
        let profile_sha256 = HASH.to_owned();
        let id = artifact_id(
            &source_sha256,
            &profile_sha256,
            &ArtifactKind::Rendition,
            Some(1),
            None,
            &content_sha256,
        )
        .unwrap();
        let mut task = OcrTask {
            schema_version: "gptgrep.ocr.task.v1".into(),
            id: String::new(),
            task_sha256: String::new(),
            source_sha256,
            prepared_sha256: HASH.into(),
            source_path: root.join("source.pdf"),
            page: 1,
            page_width_points: 100.0,
            page_height_points: 100.0,
            regions: vec![OcrRegion {
                id: "region-1".into(),
                bbox: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                required: true,
                reason: "synthetic raster".into(),
            }],
            images: vec![OcrImage {
                artifact: ArtifactRef {
                    id,
                    kind: ArtifactKind::Rendition,
                    relative_path: PathBuf::from("images/page.png"),
                    sha256: content_sha256,
                    profile_sha256,
                    bytes: image.len() as u64,
                    mime: "image/png".into(),
                    page: Some(1),
                    region_id: None,
                },
                path: root.join("images/page.png"),
                width: 1,
                height: 1,
                page_to_raster: AffineTransform::IDENTITY,
            }],
            native_text: String::new(),
            layout_context: "Synthetic full page".into(),
            profile: OcrProfile::default(),
            budget: OcrTaskBudget::default(),
        };
        rebind(&mut task);
        Self { dir, task, image }
    }
    fn page(&self) -> PreparedPage {
        PreparedPage::new(self.task.clone(), self.dir.path()).unwrap()
    }
    fn image_args(&self) -> Value {
        json!({"resource":"image","task_sha256":self.task.task_sha256,"artifact_id":self.task.images[0].artifact.id,"sha256":self.task.images[0].artifact.sha256})
    }
}
fn rebind(task: &mut OcrTask) {
    task.task_sha256 = ocr_task_digest(task).unwrap();
    task.id = format!("ocr-{}", task.task_sha256);
}

#[derive(Clone, Default)]
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    delete_after_eighth: Option<PathBuf>,
    delay_after_method: Option<(&'static str, Duration)>,
}
impl AsyncWrite for Capture {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut output = self.bytes.lock().unwrap();
        output.extend_from_slice(bytes);
        if let Some((method, delay)) = self.delay_after_method
            && serde_json::from_slice::<Value>(bytes)
                .ok()
                .is_some_and(|frame| frame["method"] == method)
        {
            // Deliberately block the poll to reproduce work timeout_at cannot preempt.
            std::thread::sleep(delay);
        }
        if let Some(path) = &self.delete_after_eighth {
            let responses = output
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .filter(|frame| frame["result"]["contentItems"].is_array())
                .count();
            if responses == 8 {
                let _ = fs::remove_file(path);
            }
        }
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn synchronous_startup_crossing_deadline_never_submits_turn() {
    let fixture = Fixture::new();
    let capture = Capture {
        delay_after_method: Some(("thread/start", Duration::from_millis(30))),
        ..Capture::default()
    };
    let context = OcrExecutionContext::new(
        "deadline-mock",
        HASH,
        SystemTime::now() + Duration::from_secs(5),
        24,
    )
    .unwrap();
    let mut admission = context
        .reserve(&fixture.task.task_sha256, HASH)
        .await
        .unwrap();
    let result = ocr_protocol::run(
        BufReader::new(Cursor::new(transcript(startup(json!({}))))),
        capture.clone(),
        Path::new("."),
        fixture.page(),
        HASH,
        &mut admission,
        ocr_protocol::OcrRunBudget {
            deadline: tokio::time::Instant::now() + Duration::from_millis(10),
            transport: ocr_protocol::TransportBudget::new(
                fixture.task.budget.transport_bytes as usize,
            ),
        },
    )
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_page_deadline_completion_unknown"
    );
    assert!(capture.frames().iter().all(|f| f["method"] != "turn/start"));
    drop(admission);
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.terminal_admissions, 1);
    assert_eq!(snapshot.quarantined_slots, 0);
}
impl Capture {
    fn frames(&self) -> Vec<Value> {
        self.bytes
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }
}
fn startup(mcp: Value) -> Vec<Value> {
    vec![
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":{"config":{"mcp_servers":mcp}}}),
        json!({"id":3,"result":{"thread":{"id":"thread-1"},"model":"gpt-6-luna","modelProvider":"openai","approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"fast"}}),
        json!({"id":4,"result":{"turn":{"id":"turn-1"}}}),
    ]
}
fn tool(call: usize, args: Value) -> Value {
    json!({"id":100+call,"method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","callId":format!("call-{call}"),"namespace":"gptgrep_ocr","tool":"ocr_resource","arguments":args}})
}
fn completed(answer: String) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","error":null,"items":[{"type":"agentMessage","id":"answer","text":answer,"phase":"final_answer"}]}}})
}
fn transcript(messages: Vec<Value>) -> Vec<u8> {
    messages
        .into_iter()
        .flat_map(|message| {
            let mut bytes = serde_json::to_vec(&message).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}
async fn replay(
    fixture: &Fixture,
    messages: Vec<Value>,
    capture: Capture,
) -> (
    anyhow::Result<ocr_protocol::OcrOutcome>,
    crate::OcrBudgetSnapshot,
) {
    let context = OcrExecutionContext::new(
        "offline-mock",
        HASH,
        SystemTime::now() + Duration::from_secs(5),
        24,
    )
    .unwrap();
    let mut admission = context
        .reserve(&fixture.task.task_sha256, HASH)
        .await
        .unwrap();
    let result = ocr_protocol::run(
        BufReader::new(Cursor::new(transcript(messages))),
        capture,
        Path::new("."),
        fixture.page(),
        HASH,
        &mut admission,
        ocr_protocol::OcrRunBudget {
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            transport: ocr_protocol::TransportBudget::new(
                fixture.task.budget.transport_bytes as usize,
            ),
        },
    )
    .await;
    admission.confirm_terminal().unwrap();
    drop(admission);
    (result, context.snapshot().unwrap())
}

#[tokio::test]
async fn dynamic_image_is_actual_bound_content_and_exact_profile() {
    let fixture = Fixture::new();
    gptgrep_parse::validate_ocr_task_metadata(&fixture.task).unwrap();
    let mut previous_model = fixture.task.clone();
    previous_model.profile.model = "gpt-6.1-luna".into();
    rebind(&mut previous_model);
    assert_eq!(
        gptgrep_parse::validate_ocr_task_metadata(&previous_model)
            .unwrap_err()
            .to_string(),
        "OCR profile does not match selected model/effort/tier"
    );
    assert!(PreparedPage::new(previous_model, fixture.dir.path()).is_err());
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    frames.push(tool(1, fixture.image_args()));
    frames.push(completed("{}".into()));
    let (result, budget) = replay(&fixture, frames, capture.clone()).await;
    let result = result.unwrap();
    result.page.verify_delivery().unwrap();
    assert_eq!(result.tool_calls, 1);
    assert_eq!(result.page.image_deliveries, 1);
    assert_eq!(budget.terminal_admissions, 1);
    assert_eq!(budget.charged_admissions, 1);
    let sent = capture.frames();
    let thread = sent.iter().find(|f| f["method"] == "thread/start").unwrap();
    assert_eq!(thread["params"]["model"], "gpt-6-luna");
    assert_eq!(thread["params"]["allowProviderModelFallback"], false);
    assert_eq!(thread["params"]["environments"], json!([]));
    assert_eq!(thread["params"]["config"]["features.view_image"], false);
    assert_eq!(
        thread["params"]["config"]["features.code_mode.enabled"],
        false
    );
    assert_eq!(
        sent.iter().filter(|f| f["method"] == "turn/start").count(),
        1
    );
    let content =
        sent.iter().find(|f| f["id"] == 101).unwrap()["result"]["contentItems"][1]["imageUrl"]
            .as_str()
            .unwrap();
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(content.strip_prefix("data:image/png;base64,").unwrap())
            .unwrap(),
        fixture.image
    );
}

#[tokio::test]
async fn ninth_queued_tool_is_denied_before_image_read() {
    let fixture = Fixture::new();
    let capture = Capture {
        delete_after_eighth: Some(fixture.task.images[0].path.clone()),
        ..Capture::default()
    };
    let mut frames = startup(json!({}));
    for call in 1..=9 {
        frames.push(tool(call, fixture.image_args()));
    }
    let (result, budget) = replay(&fixture, frames, capture.clone()).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_tool_budget_exhausted"
    );
    assert_eq!(
        capture
            .frames()
            .iter()
            .filter(|f| f["result"]["contentItems"].is_array())
            .count(),
        8
    );
    assert!(
        capture
            .frames()
            .iter()
            .any(|f| f["id"] == 109 && f.get("error").is_some())
    );
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.charged_admissions, 1);
}

#[tokio::test]
async fn forged_runtime_and_tool_bindings_fail_without_delivery() {
    for forgery in [
        "thread",
        "turn",
        "namespace",
        "path",
        "digest",
        "task",
        "duplicate",
    ] {
        let fixture = Fixture::new();
        let capture = Capture::default();
        let mut request = tool(1, fixture.image_args());
        match forgery {
            "thread" => request["params"]["threadId"] = json!("forged"),
            "turn" => request["params"]["turnId"] = json!("forged"),
            "namespace" => request["params"]["namespace"] = json!("shell"),
            "path" => request["params"]["arguments"]["path"] = json!("/unplanned/file"),
            "digest" => request["params"]["arguments"]["sha256"] = json!(HASH),
            "task" => request["params"]["arguments"]["task_sha256"] = json!(HASH),
            "duplicate" => {}
            _ => unreachable!(),
        }
        let mut frames = startup(json!({}));
        if forgery == "duplicate" {
            let mut context = tool(
                1,
                json!({"resource":"context","task_sha256":fixture.task.task_sha256}),
            );
            frames.push(context.clone());
            context["id"] = json!(999);
            frames.push(context);
        } else {
            frames.push(request);
        }
        let (result, _) = replay(&fixture, frames, capture.clone()).await;
        assert!(result.is_err(), "accepted {forgery}");
        assert!(!capture.frames().iter().any(|f| {
            f["result"]["contentItems"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["type"] == "inputImage"))
        }));
    }
}

#[tokio::test]
async fn inherited_mcp_and_native_items_are_rejected() {
    let fixture = Fixture::new();
    let capture = Capture::default();
    let (result, budget) = replay(
        &fixture,
        startup(json!({"inherited":{"command":"never-run"}})),
        capture.clone(),
    )
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_inherited_mcp_not_admitted"
    );
    assert!(
        !capture
            .frames()
            .iter()
            .any(|f| f["method"] == "thread/start")
    );
    assert_eq!(budget.terminal_admissions, 1);
    let mut frames = startup(json!({}));
    frames.push(json!({"method":"item/started","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"imageView","id":"native-1","path":"anything"}}}));
    let (result, budget) = replay(&fixture, frames, Capture::default()).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_native_or_unapproved_item"
    );
    assert_eq!(budget.quarantined_slots, 1);
}

#[tokio::test]
async fn runtime_failure_retry_and_closed_stream_keep_denominators() {
    let fixture = Fixture::new();
    const SECRET: &str = "PRIVATE_OCR_ERROR_SENTINEL";
    for retry in [false, true] {
        let mut frames = startup(json!({}));
        let mut tokens = serde_json::Map::new();
        for field in [
            "totalTokens",
            "inputTokens",
            "cachedInputTokens",
            "cacheWriteInputTokens",
            "outputTokens",
            "reasoningOutputTokens",
        ] {
            tokens.insert(field.into(), json!(u64::MAX));
        }
        tokens.insert("message".into(), json!(SECRET));
        let observed = json!({"total":tokens,"last":tokens,"modelContextWindow":u64::MAX,"headers":{"Authorization":SECRET}});
        frames.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":observed}}));
        frames.push(json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":retry,"error":{"message":SECRET,"additionalDetails":{"token":SECRET},"codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502,"headers":{"Authorization":SECRET}}}}}}));
        let capture = Capture::default();
        let (result, budget) = replay(&fixture, frames, capture.clone()).await;
        let error = result.err().unwrap();
        assert_eq!(error.to_string(), "ocr_runtime_error_completion_unknown");
        let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
        assert_eq!(typed.code(), "ocr_runtime_error_completion_unknown");
        assert_eq!(
            typed.codex_error_info,
            Some(crate::CodexErrorInfo::ResponseStreamDisconnected)
        );
        assert_eq!(typed.http_status_code, Some(502));
        assert_eq!(typed.will_retry, Some(retry));
        assert_eq!(
            typed.usage.as_ref().unwrap()["total"]["totalTokens"],
            json!(u64::MAX)
        );
        assert_eq!(
            typed.usage.as_ref().unwrap()["last"]
                .as_object()
                .unwrap()
                .len(),
            6
        );
        let serialized = serde_json::to_value(typed).unwrap();
        assert_eq!(serialized.as_object().unwrap().len(), 4);
        assert!(serde_json::to_vec(typed).unwrap().len() <= 2048);
        for safe in [
            serialized.to_string(),
            format!("{typed:?}"),
            format!("{error:#}"),
        ] {
            assert!(!safe.contains(SECRET));
            assert!(!safe.contains("thread-1"));
            assert!(!safe.contains("turn-1"));
            assert!(!safe.contains("Authorization"));
            assert!(!safe.contains("additionalDetails"));
        }
        assert_eq!(
            capture
                .frames()
                .iter()
                .filter(|f| f["method"] == "turn/start")
                .count(),
            1
        );
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.quarantined_slots, 1);
        assert_eq!(budget.terminal_admissions, 0);
    }
    let (result, budget) = replay(&fixture, startup(json!({})), Capture::default()).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_stream_closed_completion_unknown"
    );
    assert_eq!(budget.quarantined_slots, 1);
}

#[tokio::test]
async fn runtime_error_malformed_and_unknown_diagnostics_remain_nullable() {
    let fixture = Fixture::new();
    for error_info in [
        Value::Null,
        json!({"message":"private","codexErrorInfo":"unknownFutureVariant"}),
        json!({"message":"private","codexErrorInfo":"flexUnavailable"}),
        json!({"message":"private","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":700}}}),
        json!({"message":false,"codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":503}}}),
        json!({"message":"private","codexErrorInfo":"responseStreamDisconnected"}),
    ] {
        let mut frames = startup(json!({}));
        frames.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"total":{"inputTokens":-1,"outputTokens":"0","details":"private"}}}}));
        frames.push(json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":"true","error":error_info}}));
        let (result, budget) = replay(&fixture, frames, Capture::default()).await;
        let error = result.err().unwrap();
        assert_eq!(error.to_string(), "ocr_runtime_error_completion_unknown");
        let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
        assert!(typed.codex_error_info.is_none());
        assert!(typed.http_status_code.is_none());
        assert!(typed.will_retry.is_none());
        assert!(typed.usage.is_none());
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.quarantined_slots, 1);
        assert_eq!(budget.terminal_admissions, 0);
    }
}

#[tokio::test]
async fn runtime_error_known_variants_preserve_only_safe_observations() {
    let fixture = Fixture::new();
    for (info, expected, status) in [
        (
            json!("unauthorized"),
            crate::CodexErrorInfo::Unauthorized,
            None,
        ),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":401}}),
            crate::CodexErrorInfo::HttpConnectionFailed,
            Some(401),
        ),
        (
            json!({"activeTurnNotSteerable":{"turnKind":"review","extra":"private"}}),
            crate::CodexErrorInfo::ActiveTurnNotSteerable,
            None,
        ),
    ] {
        let mut frames = startup(json!({}));
        frames.push(json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":false,"error":{"message":"private","codexErrorInfo":info}}}));
        let (result, budget) = replay(&fixture, frames, Capture::default()).await;
        let error = result.err().unwrap();
        let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
        assert_eq!(typed.codex_error_info, Some(expected));
        assert_eq!(typed.http_status_code, status);
        assert_eq!(typed.will_retry, Some(false));
        assert!(typed.usage.is_none());
        assert_eq!(budget.quarantined_slots, 1);
    }
}

#[tokio::test]
async fn runtime_error_identity_mismatch_precedes_diagnostics() {
    let fixture = Fixture::new();
    for (field, expected) in [
        ("threadId", "ocr_thread_identity_mismatch"),
        ("turnId", "ocr_turn_identity_mismatch"),
    ] {
        let mut frames = startup(json!({}));
        frames.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"total":{"outputTokens":0}}}}));
        let mut error = json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":true,"error":{"message":"private","codexErrorInfo":"unauthorized"}}});
        error["params"][field] = json!("forged");
        frames.push(error);
        let capture = Capture::default();
        let (result, budget) = replay(&fixture, frames, capture.clone()).await;
        let error = result.err().unwrap();
        assert_eq!(error.to_string(), expected);
        assert!(error.downcast_ref::<crate::OcrRuntimeError>().is_none());
        assert_eq!(
            capture
                .frames()
                .iter()
                .filter(|f| f["method"] == "turn/start")
                .count(),
            1
        );
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.quarantined_slots, 1);
    }
}

#[tokio::test]
async fn oversized_results_and_tight_response_budget_reject_without_delivery() {
    let fixture = Fixture::new();
    let mut frames = startup(json!({}));
    frames.push(completed("x".repeat(256 * 1024 + 1)));
    let (result, budget) = replay(&fixture, frames, Capture::default()).await;
    assert_eq!(result.err().unwrap().to_string(), "ocr_final_result_limit");
    assert_eq!(budget.terminal_admissions, 1);

    let mut fixture = Fixture::new();
    fixture.task.budget.frame_bytes = 4096;
    rebind(&mut fixture.task);
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    frames.push(tool(1, fixture.image_args()));
    let (result, budget) = replay(&fixture, frames, capture.clone()).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_tool_transport_admission"
    );
    assert!(
        !capture
            .frames()
            .iter()
            .any(|frame| frame["id"] == 101 && frame.get("result").is_some())
    );
    assert_eq!(budget.quarantined_slots, 1);
}

#[test]
fn source_digest_image_dimensions_and_input_limits_are_rechecked() {
    let fixture = Fixture::new();
    let mut page = fixture.page();
    fs::write(&fixture.task.source_path, b"changed source").unwrap();
    assert_eq!(
        page.call(fixture.image_args()).unwrap_err().to_string(),
        "ocr_source_changed"
    );
    let mut fixture = Fixture::new();
    fixture.task.images[0].width = 2;
    rebind(&mut fixture.task);
    assert!(PreparedPage::new(fixture.task, fixture.dir.path()).is_err());
    let mut fixture = Fixture::new();
    fixture.task.budget.resources.image_file_bytes = 1;
    rebind(&mut fixture.task);
    assert!(PreparedPage::new(fixture.task, fixture.dir.path()).is_err());
    let mut fixture = Fixture::new();
    fixture.task.budget.resources.image_pixels = 0;
    rebind(&mut fixture.task);
    assert!(PreparedPage::new(fixture.task, fixture.dir.path()).is_err());
    let mut fixture = Fixture::new();
    fixture.task.images[0].page_to_raster.a = f64::INFINITY;
    assert!(PreparedPage::new(fixture.task, fixture.dir.path()).is_err());
}

#[cfg(unix)]
#[test]
fn traversal_and_ancestor_symlink_swaps_never_read_external_images() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let mut page = fixture.page();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("page.png"), &fixture.image).unwrap();
    fs::rename(
        fixture.dir.path().join("images"),
        fixture.dir.path().join("held-images"),
    )
    .unwrap();
    symlink(external.path(), fixture.dir.path().join("images")).unwrap();
    assert_eq!(
        page.call(fixture.image_args()).unwrap_err().to_string(),
        "ocr_image_open_failed"
    );
    let mut fixture = Fixture::new();
    fixture.task.images[0].artifact.relative_path = PathBuf::from("../page.png");
    rebind(&mut fixture.task);
    assert!(PreparedPage::new(fixture.task, fixture.dir.path()).is_err());
}

#[tokio::test]
async fn missing_image_or_forged_final_event_cannot_complete() {
    let fixture = Fixture::new();
    let mut frames = startup(json!({}));
    frames.push(completed("{}".into()));
    let (result, budget) = replay(&fixture, frames, Capture::default()).await;
    assert_eq!(
        result
            .unwrap()
            .page
            .verify_delivery()
            .unwrap_err()
            .to_string(),
        "ocr_required_image_not_delivered"
    );
    assert_eq!(budget.terminal_admissions, 1);
    let mut frames = startup(json!({}));
    frames.push(json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","error":null,"items":[{"type":"dynamicToolCall","id":"unmediated","namespace":"gptgrep_ocr","tool":"ocr_resource"},{"type":"agentMessage","text":"{}"}]}}}));
    let (result, _) = replay(&fixture, frames, Capture::default()).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "ocr_unmediated_dynamic_item"
    );
}

#[cfg(unix)]
fn mock_executor(
    fixture: &Fixture,
    mode: &str,
    accepted: bool,
) -> (crate::OcrHostExecutor, OcrExecutionContext, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let script = fixture.dir.path().join("mock-codex");
    let code = r##"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
MODE = '__MODE__'
def send(value):
    print(json.dumps(value, separators=(',', ':')), flush=True)
pending = None
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'id': message['id'], 'result': {}})
    elif method == 'config/read':
        send({'id': message['id'], 'result': {'config': {'mcp_servers': {}}}})
    elif method == 'thread/start':
        send({'id': message['id'], 'result': {'thread': {'id': 'thread-1'}, 'model': 'gpt-6-luna', 'modelProvider': 'openai', 'approvalPolicy': 'never', 'sandbox': {'type': 'readOnly', 'networkAccess': False}, 'reasoningEffort': 'max', 'serviceTier': 'fast'}})
    elif method == 'turn/start':
        pending = json.loads(message['params']['input'][0]['text'])
        send({'id': message['id'], 'result': {'turn': {'id': 'turn-1'}}})
        artifact = pending['images'][0]['artifact']
        send({'id': 101, 'method': 'item/tool/call', 'params': {'threadId': 'thread-1', 'turnId': 'turn-1', 'callId': 'call-1', 'namespace': 'gptgrep_ocr', 'tool': 'ocr_resource', 'arguments': {'task_sha256': pending['task_sha256'], 'resource': 'image', 'artifact_id': artifact['id'], 'sha256': artifact['sha256']}}})
    elif message.get('id') == 101:
        if MODE in ('hang', 'stderr_flood'):
            child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
            pathlib.Path(__file__ + '.pid-writing').write_text(json.dumps([os.getpid(), child.pid]))
            os.replace(__file__ + '.pid-writing', __file__ + '.pid')
            if MODE == 'stderr_flood':
                for _ in range(80):
                    data = b'x' * (256 * 1024)
                    while data:
                        data = data[os.write(2, data):]
            time.sleep(60)
        if MODE == 'stderr_bounded':
            os.write(2, b'x' * 997)
        status = 'partial' if MODE == 'partial' else 'complete'
        result = {'schema_version': 'gptgrep.ocr.result.v1', 'task_id': pending['task_id'], 'task_sha256': pending['task_sha256'], 'source_sha256': pending['source_sha256'], 'prepared_sha256': pending['prepared_sha256'], 'page': pending['page'], 'status': status, 'regions': [{'id': region['id'], 'bbox': region['bbox'], 'text': 'synthetic transcription', 'reading_order': index, 'status': status} for index, region in enumerate(pending['regions'])]}
        send({'method': 'turn/completed', 'params': {'threadId': 'thread-1', 'turn': {'id': 'turn-1', 'status': 'completed', 'error': None, 'items': [{'type': 'agentMessage', 'id': 'answer', 'phase': 'final_answer', 'text': json.dumps(result)}]}}})
"##.replace("__MODE__", mode);
    fs::write(&script, &code).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let home = fixture.dir.path().join("empty-codex-home");
    fs::create_dir(&home).unwrap();
    let context = OcrExecutionContext::new(
        "offline-owned-process",
        HASH,
        SystemTime::now() + Duration::from_secs(10),
        24,
    )
    .unwrap();
    let executor = crate::OcrHostExecutor::new(
        crate::OcrHostConfig {
            codex_bin: script.to_string_lossy().into_owned(),
            codex_home: home,
            prepared_root: fixture.dir.path().into(),
            raw_results_dir: fixture.dir.path().join("raw"),
            runtime_profile: crate::OcrRuntimeProfile {
                schema_version: "gptgrep.ocr.runtime-profile.v1".into(),
                source_revision: "a".repeat(40),
                protocol_schema_sha256: HASH.into(),
                codex_binary_sha256: hash(code.as_bytes()),
                operator_catalog_sha256: crate::OcrRuntimeProfile::operator_catalog_sha256()
                    .unwrap(),
                tool_policy_sha256: crate::OcrRuntimeProfile::tool_policy_sha256().unwrap(),
                accepted_image_qualification_sha256: accepted.then(|| HASH.into()),
            },
            transport: crate::OcrImageTransport::DynamicImages,
        },
        context.clone(),
    )
    .unwrap();
    (executor, context, script.with_file_name("mock-codex.pid"))
}

#[cfg(unix)]
#[tokio::test]
async fn real_owned_mock_process_binds_receipt_and_retains_partial_raw_output() {
    let fixture = Fixture::new();
    let (executor, context, _) = mock_executor(&fixture, "complete", false);
    let result = executor.probe_page(fixture.task.clone()).await.unwrap();
    assert!(!result.image_qualification_accepted);
    assert_eq!(result.image_deliveries, 1);
    assert_eq!(
        result.result.receipt.runtime_profile_sha256,
        result.runtime_profile_sha256
    );
    assert_eq!(context.snapshot().unwrap().terminal_admissions, 1);
    let raw = fixture.dir.path().join("raw").join(format!(
        "ocr-{}.json",
        result.result.receipt.response_sha256
    ));
    assert_eq!(
        hash(&fs::read(raw).unwrap()),
        result.result.receipt.response_sha256
    );

    let fixture = Fixture::new();
    let (executor, context, _) = mock_executor(&fixture, "partial", true);
    use gptgrep_parse::OcrExecutor;
    assert!(executor.execute(fixture.task.clone()).await.is_err());
    let files = fs::read_dir(fixture.dir.path().join("raw"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    let raw: Value = serde_json::from_slice(&fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(raw["status"], "partial");
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.terminal_admissions, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_is_counted_and_flood_kills_group_without_releasing_provider_slot() {
    let fixture = Fixture::new();
    let (executor, _, _) = mock_executor(&fixture, "complete", false);
    let baseline = executor.probe_page(fixture.task.clone()).await.unwrap();
    let fixture = Fixture::new();
    let (executor, _, _) = mock_executor(&fixture, "stderr_bounded", false);
    let bounded = executor.probe_page(fixture.task.clone()).await.unwrap();
    assert_eq!(bounded.transport_bytes, baseline.transport_bytes + 997);

    let fixture = Fixture::new();
    let (executor, context, pid_path) = mock_executor(&fixture, "stderr_flood", false);
    let started = std::time::Instant::now();
    let error = executor.probe_page(fixture.task.clone()).await.unwrap_err();
    assert_eq!(error.to_string(), "ocr_total_transport_limit");
    assert!(started.elapsed() < Duration::from_secs(5));
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_path).unwrap()).unwrap();
    for pid in pids {
        assert!(owned_pid_is_gone_or_zombie(pid));
    }
    let budget = context.snapshot().unwrap();
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
    assert!(!fixture.dir.path().join("raw").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn unqualified_runtime_rejects_normal_execution_before_admission() {
    let fixture = Fixture::new();
    let (executor, context, _) = mock_executor(&fixture, "complete", false);
    use gptgrep_parse::OcrExecutor;
    assert_eq!(
        executor
            .execute(fixture.task)
            .await
            .unwrap_err()
            .to_string(),
        "ocr_runtime_image_qualification_required"
    );
    assert_eq!(context.snapshot().unwrap().charged_admissions, 0);
}

#[test]
fn runtime_identity_binds_tool_metadata_and_is_stable_across_approval_reference() {
    let mut profile =
        crate::OcrRuntimeProfile::unqualified("a".repeat(40), HASH.into(), HASH.into()).unwrap();
    let original = profile.sha256().unwrap();
    profile.accepted_image_qualification_sha256 = Some(HASH.into());
    assert_eq!(profile.sha256().unwrap(), original);
    profile.tool_policy_sha256 = HASH.into();
    assert!(profile.validate().is_err());
}

#[cfg(unix)]
fn owned_pid_is_gone_or_zombie(pid: u32) -> bool {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    output.stdout.is_empty()
        || String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z')
}

#[cfg(unix)]
#[tokio::test]
async fn page_timeout_kills_owned_group_and_keeps_provider_slot_quarantined() {
    let mut fixture = Fixture::new();
    fixture.task.budget.max_seconds = 1;
    rebind(&mut fixture.task);
    let (executor, context, pid_file) = mock_executor(&fixture, "hang", true);
    use gptgrep_parse::OcrExecutor;
    let error = executor.execute(fixture.task.clone()).await.unwrap_err();
    assert_eq!(error.to_string(), "ocr_page_deadline_completion_unknown");
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_file).unwrap()).unwrap();
    for pid in pids {
        assert!(
            owned_pid_is_gone_or_zombie(pid),
            "owned mock process survived timeout"
        );
    }
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.quarantined_slots, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn task_cancellation_kills_owned_group_and_never_refunds_admission() {
    let fixture = Fixture::new();
    let (executor, context, pid_file) = mock_executor(&fixture, "hang", true);
    use gptgrep_parse::OcrExecutor;
    let task = fixture.task.clone();
    let handle = tokio::spawn(async move { executor.execute(task).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "mock process did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_file).unwrap()).unwrap();
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    loop {
        if pids.iter().all(|pid| owned_pid_is_gone_or_zombie(*pid)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned mock process survived cancellation"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.quarantined_slots, 1);
}

/// Explicit non-inference qualification of emitted protocol shapes against an
/// operator-selected, pinned schema bundle. No private path is embedded in source.
#[tokio::test]
#[ignore = "requires GPTGREP_OCR_RUNTIME_SCHEMA_ROOT for the selected runtime"]
async fn emitted_protocol_shapes_match_selected_runtime_schema() {
    let root = PathBuf::from(
        std::env::var_os("GPTGREP_OCR_RUNTIME_SCHEMA_ROOT").expect("selected schema root"),
    );
    let fixture = Fixture::new();
    eprintln!(
        "{}",
        json!({"operator_catalog_sha256":crate::OcrRuntimeProfile::operator_catalog_sha256().unwrap(),"tool_policy_sha256":crate::OcrRuntimeProfile::tool_policy_sha256().unwrap()})
    );
    if let Some(path) = std::env::var_os("GPTGREP_OCR_OPERATOR_CATALOG_OUT") {
        use std::io::Write;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap();
        output
            .write_all(&serde_json::to_vec(&crate::ocr::model_descriptor()).unwrap())
            .unwrap();
        output.sync_all().unwrap();
    }
    if let Some(path) = std::env::var_os("GPTGREP_OCR_POLICY_OUT") {
        use std::io::Write;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap();
        output
            .write_all(&serde_json::to_vec(&crate::ocr::ocr_policy()).unwrap())
            .unwrap();
        output.sync_all().unwrap();
    }
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    frames.push(tool(1, fixture.image_args()));
    frames.push(completed("{}".into()));
    replay(&fixture, frames, capture.clone()).await.0.unwrap();
    for frame in capture.frames() {
        let (schema, value) = match frame["method"].as_str() {
            Some("initialize") => ("v1/InitializeParams.json", &frame["params"]),
            Some("config/read") => ("v2/ConfigReadParams.json", &frame["params"]),
            Some("thread/start") => ("v2/ThreadStartParams.json", &frame["params"]),
            Some("turn/start") => ("v2/TurnStartParams.json", &frame["params"]),
            _ if frame["id"] == 101 => ("DynamicToolCallResponse.json", &frame["result"]),
            _ => continue,
        };
        let schema_value: Value =
            serde_json::from_slice(&fs::read(root.join(schema)).unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema_value).unwrap();
        assert!(
            validator.is_valid(value),
            "emitted shape rejected by {schema}"
        );
    }
}
