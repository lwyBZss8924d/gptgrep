//! Offline adversarial replay: these tests never launch a provider or a Codex runtime.
use crate::{
    ocr::{PreparedPage, hash, ocr_command},
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

pub(crate) fn configured_image_fixture_png() -> Vec<u8> {
    let mut seed = 17u32;
    let pixels = (0..1024 * 1024 * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect::<Vec<_>>();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, 1024, 1024, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert!(png.len() > 1_048_576 && png.len() <= 4_194_304);
    png
}

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub task: OcrTask,
    pub image: Vec<u8>,
}
impl Fixture {
    pub(crate) fn new() -> Self {
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
    pub(crate) fn page(&self) -> PreparedPage {
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
    replay_bytes(fixture, transcript(messages), capture).await
}
async fn replay_bytes(
    fixture: &Fixture,
    bytes: Vec<u8>,
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
        BufReader::new(Cursor::new(bytes)),
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
async fn automatic_metadata_is_discarded_during_startup_and_after_final_item() {
    let fixture = Fixture::new();
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    frames.insert(0, json!({"method":"remoteControl/status/changed","params":{"status":"disabled","installationId":"MOCK-private-installation","serverName":"MOCK-private-server"}}));
    frames.insert(
        2,
        json!({"method":"account/updated","params":{"authMode":"chatgpt","planType":null}}),
    );
    frames.insert(
        4,
        json!({"method":"account/rateLimits/updated","params":{"rateLimits":{}}}),
    );
    frames.push(tool(1, fixture.image_args()));
    frames.push(json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","id":"answer","phase":"final_answer","text":"{}"}}}));
    frames.push(json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"MOCK-private-limit","planType":"pro","primary":{"usedPercent":5,"resetsAt":null}}}}));
    frames.push(completed("{}".into()));
    let (result, budget) = replay(&fixture, frames, capture.clone()).await;
    let result = result.unwrap();
    result.page.verify_delivery().unwrap();
    assert_eq!(result.answer, "{}");
    assert_eq!(result.tool_calls, 1);
    assert_eq!(budget.terminal_admissions, 1);
    let sent = capture.frames();
    assert_eq!(
        sent.iter()
            .filter(|frame| frame["method"] == "turn/start")
            .count(),
        1
    );
    assert!(
        !serde_json::to_string(&sent)
            .unwrap()
            .contains("MOCK-private")
    );
    assert!(
        sent.iter()
            .filter_map(|frame| frame["method"].as_str())
            .all(|method| !method.starts_with("account/") && !method.starts_with("remoteControl/"))
    );
}

#[tokio::test]
async fn automatic_metadata_cap_is_shared_across_startup_and_active_turn() {
    for overflow in [false, true] {
        let fixture = Fixture::new();
        let capture = Capture::default();
        let mut frames =
            vec![json!({"method":"account/updated","params":{"authMode":"chatgpt"}}); 16];
        frames.extend(startup(json!({})));
        frames.extend(vec![
            json!({"method":"account/rateLimits/updated","params":{"rateLimits":{}}});
            16
        ]);
        if overflow {
            frames.push(json!({"method":"remoteControl/status/changed","params":{"status":"disabled","installationId":"MOCK","serverName":"MOCK"}}));
        }
        frames.push(tool(1, fixture.image_args()));
        frames.push(completed("{}".into()));
        let (result, budget) = replay(&fixture, frames, capture.clone()).await;
        if overflow {
            assert_eq!(
                result.err().unwrap().to_string(),
                "ocr_automatic_metadata_limit"
            );
            assert_eq!(budget.quarantined_slots, 1);
            assert!(!capture.frames().iter().any(|frame| frame["id"] == 101));
        } else {
            result.unwrap().page.verify_delivery().unwrap();
            assert_eq!(budget.terminal_admissions, 1);
        }
        assert_eq!(budget.charged_admissions, 1);
    }
}

#[tokio::test]
async fn automatic_metadata_does_not_change_warning_reroute_or_server_request_refusal() {
    let fixture = Fixture::new();
    for method in ["configWarning", "config/warning", "model/rerouted"] {
        let mut frames = startup(json!({}));
        frames.push(json!({"method":method,"params":{}}));
        let (result, _) = replay(&fixture, frames, Capture::default()).await;
        assert_eq!(
            result.err().unwrap().to_string(),
            "ocr_runtime_configuration_not_admitted"
        );
    }
    for method in [
        "account/updated",
        "account/rateLimits/updated",
        "remoteControl/status/changed",
    ] {
        let capture = Capture::default();
        let mut frames = startup(json!({}));
        frames.push(json!({"id":99,"method":method,"params":{}}));
        let (result, _) = replay(&fixture, frames, capture.clone()).await;
        assert_eq!(
            result.err().unwrap().to_string(),
            "ocr_unapproved_server_request"
        );
        assert!(
            capture
                .frames()
                .iter()
                .any(|frame| frame["id"] == 99 && frame["error"]["code"] == -32601)
        );
    }
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
        assert_eq!(serialized.as_object().unwrap().len(), 6);
        assert_eq!(
            serialized["diagnostics"]["schema_version"],
            "gptgrep.codex.error-diagnostics.v1"
        );
        assert_eq!(typed.server_retry_notifications, 0);
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

fn intermediate_error(info: Value, retry: Value) -> Value {
    json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":retry,"error":{"message":"PRIVATE_RETRY_ERROR_SENTINEL","additionalDetails":"PRIVATE_RETRY_DETAILS_SENTINEL","codexErrorInfo":info}}})
}

#[tokio::test]
async fn raw_duplicate_error_fields_fail_before_recovery_or_image_delivery() {
    // Value-based fixtures cannot express duplicate keys: exercise actual wire bytes.
    for fields in [
        r#""willRetry":false,"willRetry":true,"error":{"message":"failed","codexErrorInfo":{"responseStreamDisconnected":{}}}"#,
        r#""willRetry":false,"will\u0052etry":true,"error":{"message":"failed","codexErrorInfo":{"responseStreamDisconnected":{}}}"#,
        r#""willRetry":true,"error":{"message":"failed","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":700,"httpStatusCode":503}}}"#,
        r#""willRetry":true,"error":{"message":"failed","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":700,"httpStatus\u0043ode":503}}}"#,
        r#""willRetry":true,"error":{"message":"failed","codexErrorInfo":"internalServerError","codexErrorInfo":{"responseStreamDisconnected":{}}}"#,
    ] {
        let fixture = Fixture::new();
        let capture = Capture::default();
        let mut bytes = transcript(startup(json!({})));
        bytes.extend_from_slice(
            format!(
                "{{\"method\":\"error\",\"params\":{{\"threadId\":\"thread-1\",\"turnId\":\"turn-1\",{fields}}}}}\n"
            )
            .as_bytes(),
        );
        bytes.extend(transcript(vec![
            tool(1, fixture.image_args()),
            completed("{}".into()),
        ]));
        let (result, budget) = replay_bytes(&fixture, bytes, capture.clone()).await;
        assert_eq!(
            result.err().unwrap().to_string(),
            "ocr_invalid_protocol_json"
        );
        let sent = capture.frames();
        assert_eq!(sent.len(), 5); // Startup only; no recovery RPC or image response.
        assert_eq!(
            sent.iter().filter(|f| f["method"] == "turn/start").count(),
            1
        );
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.quarantined_slots, 1);
        assert_eq!(budget.terminal_admissions, 0);
    }
}

#[tokio::test]
async fn intermediate_transport_notifications_recover_without_another_turn_or_reservation() {
    for (variant, count) in [
        ("responseStreamDisconnected", 1),
        ("responseStreamConnectionFailed", 1),
        ("httpConnectionFailed", 5),
    ] {
        let fixture = Fixture::new();
        let capture = Capture::default();
        let mut frames = startup(json!({}));
        for _ in 0..count {
            frames.push(intermediate_error(
                json!({variant:{"httpStatusCode":null}}),
                json!(true),
            ));
        }
        frames.push(tool(1, fixture.image_args()));
        frames.push(completed("{}".into()));
        let (result, budget) = replay(&fixture, frames, capture.clone()).await;
        let result = result.unwrap();
        result.page.verify_delivery().unwrap();
        assert_eq!(result.retries, count);
        assert_eq!(result.tool_calls, 1);
        assert!(result.usage.is_none());
        let sent = capture.frames();
        assert_eq!(
            sent.iter().filter(|f| f["method"] == "turn/start").count(),
            1
        );
        assert_eq!(sent.iter().filter(|f| f["method"].is_string()).count(), 5);
        assert_eq!(sent.len(), 6); // four requests, initialized, one genuine image response
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.terminal_admissions, 1);
        assert_eq!(budget.quarantined_slots, 0);
    }
}

#[tokio::test]
async fn intermediate_then_terminal_error_preserves_count_and_safe_diagnostics() {
    let fixture = Fixture::new();
    let mut frames = startup(json!({}));
    frames.push(intermediate_error(
        json!({"responseStreamDisconnected":{}}),
        json!(true),
    ));
    frames.push(intermediate_error(
        json!({"responseStreamConnectionFailed":{"httpStatusCode":503}}),
        json!(false),
    ));
    let (result, budget) = replay(&fixture, frames, Capture::default()).await;
    let error = result.err().unwrap();
    let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
    assert_eq!(typed.server_retry_notifications, 1);
    assert_eq!(
        typed.codex_error_info,
        Some(crate::CodexErrorInfo::ResponseStreamConnectionFailed)
    );
    assert_eq!(typed.http_status_code, Some(503));
    assert_eq!(typed.will_retry, Some(false));
    assert!(typed.usage.is_none());
    let safe = serde_json::to_string(typed).unwrap();
    assert!(!safe.contains("PRIVATE_RETRY"));
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
}

#[tokio::test]
async fn sixth_intermediate_notification_fails_before_later_image_delivery() {
    let fixture = Fixture::new();
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    for _ in 0..6 {
        frames.push(intermediate_error(
            json!({"responseStreamDisconnected":{}}),
            json!(true),
        ));
    }
    frames.push(tool(1, fixture.image_args()));
    frames.push(completed("{}".into()));
    let (result, budget) = replay(&fixture, frames, capture.clone()).await;
    let error = result.err().unwrap();
    let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
    assert_eq!(typed.server_retry_notifications, 6);
    assert_eq!(typed.will_retry, Some(true));
    assert!(capture.frames().iter().all(|f| f.get("result").is_none()));
    assert_eq!(capture.frames().len(), 5);
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
}

#[tokio::test]
async fn intermediate_unknown_or_malformed_error_never_recovers() {
    let fixture = Fixture::new();
    let base = intermediate_error(json!({"responseStreamDisconnected":{}}), json!(true));
    let mut cases = Vec::new();
    for info in [
        Value::Null,
        json!("unauthorized"),
        json!("unknownFutureVariant"),
        json!({"responseTooManyFailedAttempts":{}}),
    ] {
        let mut f = base.clone();
        f["params"]["error"]["codexErrorInfo"] = info;
        cases.push(f);
    }
    for retry in [Value::Null, json!("true"), json!(false)] {
        let mut f = base.clone();
        f["params"]["willRetry"] = retry;
        cases.push(f);
    }
    for (name, value) in [
        ("message", json!(false)),
        ("additionalDetails", json!({"secret":"not retained"})),
        ("misalignment", json!({"errorType":"private"})),
        ("unknown", json!("private")),
    ] {
        let mut f = base.clone();
        f["params"]["error"][name] = value;
        cases.push(f);
    }
    let mut extra = base.clone();
    extra["params"]["error"]["codexErrorInfo"]["responseStreamDisconnected"]["headers"] =
        json!({"private":"not retained"});
    cases.push(extra);
    let mut invalid_status = base.clone();
    invalid_status["params"]["error"]["codexErrorInfo"]["responseStreamDisconnected"]["httpStatusCode"] =
        json!(700);
    cases.push(invalid_status);
    for f in cases {
        let capture = Capture::default();
        let mut frames = startup(json!({}));
        frames.push(f);
        frames.push(completed("{}".into()));
        let (result, budget) = replay(&fixture, frames, capture.clone()).await;
        let error = result.err().unwrap();
        let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
        assert_eq!(typed.server_retry_notifications, 0);
        assert_eq!(capture.frames().len(), 5);
        assert_eq!(budget.charged_admissions, 1);
        assert_eq!(budget.quarantined_slots, 1);
    }
}

#[tokio::test]
async fn intermediate_error_never_weakens_identity_or_tool_budget() {
    let fixture = Fixture::new();
    for field in ["threadId", "turnId"] {
        let mut frames = startup(json!({}));
        let mut error = intermediate_error(json!({"responseStreamDisconnected":{}}), json!(true));
        error["params"][field] = json!("forged");
        frames.push(error);
        frames.push(completed("{}".into()));
        let (result, budget) = replay(&fixture, frames, Capture::default()).await;
        assert!(
            result
                .err()
                .unwrap()
                .downcast_ref::<crate::OcrRuntimeError>()
                .is_none()
        );
        assert_eq!(budget.quarantined_slots, 1);
    }
    let capture = Capture::default();
    let mut frames = startup(json!({}));
    frames.push(tool(1, fixture.image_args()));
    frames.push(intermediate_error(
        json!({"responseStreamDisconnected":{}}),
        json!(true),
    ));
    for n in 2..=9 {
        frames.push(tool(n, fixture.image_args()));
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

#[tokio::test]
async fn intermediate_notification_does_not_renew_original_outer_deadline() {
    struct PendingAfterFrames(Cursor<Vec<u8>>);
    impl tokio::io::AsyncRead for PendingAfterFrames {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let start = self.0.position() as usize;
            if start == self.0.get_ref().len() {
                return Poll::Pending;
            }
            let end = (start + buf.remaining()).min(self.0.get_ref().len());
            buf.put_slice(&self.0.get_ref()[start..end]);
            self.0.set_position(end as u64);
            Poll::Ready(Ok(()))
        }
    }
    let fixture = Fixture::new();
    let context = OcrExecutionContext::new(
        "offline-original-deadline",
        HASH,
        SystemTime::now() + Duration::from_secs(5),
        24,
    )
    .unwrap();
    let mut admission = context
        .reserve(&fixture.task.task_sha256, HASH)
        .await
        .unwrap();
    let mut frames = startup(json!({}));
    frames.push(intermediate_error(
        json!({"responseStreamDisconnected":{}}),
        json!(true),
    ));
    let capture = Capture::default();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    let transport =
        ocr_protocol::TransportBudget::new(fixture.task.budget.transport_bytes as usize);
    let result = tokio::time::timeout_at(
        deadline,
        ocr_protocol::run(
            PendingAfterFrames(Cursor::new(transcript(frames))),
            capture.clone(),
            Path::new("."),
            fixture.page(),
            HASH,
            &mut admission,
            ocr_protocol::OcrRunBudget {
                deadline,
                transport: transport.clone(),
            },
        ),
    )
    .await;
    let error = crate::ocr::capture_page_outcome(result, &transport)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "ocr_page_deadline_completion_unknown");
    assert_eq!(capture.frames().len(), 5);
    assert_eq!(
        capture
            .frames()
            .iter()
            .filter(|f| f["method"] == "turn/start")
            .count(),
        1
    );
    drop(admission);
    let budget = context.snapshot().unwrap();
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
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
        let diagnostic = typed.diagnostics.as_ref().unwrap();
        assert_eq!(
            diagnostic["schema_version"],
            "gptgrep.codex.error-diagnostics.v1"
        );
        assert_eq!(diagnostic["native_error_shape_valid"], true);
        assert_eq!(diagnostic["message"]["bytes"], 7);
        let serialized = serde_json::to_string(typed).unwrap();
        assert!(!serialized.contains("private"));
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
pub(crate) fn mock_executor(
    fixture: &Fixture,
    mode: &str,
    accepted: bool,
) -> (crate::OcrHostExecutor, OcrExecutionContext, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let script = fixture.dir.path().join("mock-codex");
    let code = r##"#!/usr/bin/env python3
import json, os, pathlib, signal, subprocess, sys, time
MODE = '__MODE__'
cleanup_signaled = False
def note_cleanup(signum, frame):
    global cleanup_signaled
    cleanup_signaled = True
if MODE in ('cleanup_error_flood', 'cleanup_success_flood'):
    signal.signal(signal.SIGTERM, note_cleanup)
def cleanup_flood():
    while not cleanup_signaled:
        time.sleep(0.001)
    pathlib.Path(__file__ + '.after-term').write_text('owned cleanup TERM observed')
    try:
        for _ in range(80):
            data = b'e' * (256 * 1024)
            while data:
                data = data[os.write(2, data):]
    except OSError:
        pass
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
        if MODE in ('cleanup_error_flood', 'cleanup_success_flood'):
            pathlib.Path(__file__ + '.pid-writing').write_text(json.dumps([os.getpid()]))
            os.replace(__file__ + '.pid-writing', __file__ + '.pid')
        if MODE == 'cleanup_error_flood':
            send({'method': 'thread/tokenUsage/updated', 'params': {'threadId': 'thread-1', 'turnId': 'turn-1', 'tokenUsage': {'total': {'inputTokens': 7, 'outputTokens': 0, 'private': 'not retained'}}}})
            send({'method': 'error', 'params': {'threadId': 'thread-1', 'turnId': 'turn-1', 'willRetry': False, 'error': {'message': 'private synthetic diagnostic', 'codexErrorInfo': {'responseStreamDisconnected': {'httpStatusCode': 503}}}}})
            cleanup_flood()
            sys.exit(0)
        if MODE in ('hang', 'hang_journal', 'stderr_flood'):
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
        if MODE == 'cleanup_success_flood':
            cleanup_flood()
            sys.exit(0)
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
    let context = if mode == "hang_journal" {
        context
            .with_ledger(fixture.dir.path().join("mock-ocr-ledger.json"))
            .unwrap()
    } else {
        context
    };
    let executor = crate::OcrHostExecutor::new(
        crate::OcrHostConfig {
            codex_bin: script.to_string_lossy().into_owned(),
            codex_home: home,
            codex_state_dir: None,
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
                native_viewer: None,
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
async fn captured_runtime_error_survives_cleanup_time_stderr_overflow() {
    let fixture = Fixture::new();
    let (executor, context, pid_path) = mock_executor(&fixture, "cleanup_error_flood", false);
    let error = executor.probe_page(fixture.task.clone()).await.unwrap_err();
    assert_eq!(error.to_string(), "ocr_runtime_error_completion_unknown");
    let typed = error.downcast_ref::<crate::OcrRuntimeError>().unwrap();
    assert_eq!(
        typed.codex_error_info,
        Some(crate::CodexErrorInfo::ResponseStreamDisconnected)
    );
    assert_eq!(typed.http_status_code, Some(503));
    assert_eq!(typed.will_retry, Some(false));
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["inputTokens"], 7);
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["outputTokens"], 0);
    assert!(!serde_json::to_string(typed).unwrap().contains("private"));
    assert!(fixture.dir.path().join("mock-codex.after-term").exists());
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_path).unwrap()).unwrap();
    assert!(pids.into_iter().all(owned_pid_is_gone_or_zombie));
    let budget = context.snapshot().unwrap();
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
    assert!(!fixture.dir.path().join("raw").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn captured_success_rejects_cleanup_time_stderr_overflow() {
    let fixture = Fixture::new();
    let (executor, context, pid_path) = mock_executor(&fixture, "cleanup_success_flood", false);
    let error = executor.probe_page(fixture.task.clone()).await.unwrap_err();
    assert_eq!(error.to_string(), "ocr_total_transport_limit");
    assert!(fixture.dir.path().join("mock-codex.after-term").exists());
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_path).unwrap()).unwrap();
    assert!(pids.into_iter().all(owned_pid_is_gone_or_zombie));
    let budget = context.snapshot().unwrap();
    assert_eq!(budget.charged_admissions, 1);
    assert_eq!(budget.quarantined_slots, 1);
    assert_eq!(budget.terminal_admissions, 0);
    assert!(!fixture.dir.path().join("raw").exists());
}

#[test]
fn captured_typed_error_survives_later_meter_failure() {
    let transport = ocr_protocol::TransportBudget::new(64);
    let typed = crate::OcrRuntimeError::new(
        &json!({"message":"not retained","codexErrorInfo":"unauthorized"}),
        Some(false),
        None,
    );
    let captured = crate::ocr::capture_page_outcome::<()>(Ok(Err(typed.into())), &transport);
    assert!(transport.admit(65).is_err());
    let error = crate::ocr::finish_page_outcome(captured, transport.check()).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::OcrRuntimeError>()
            .unwrap()
            .codex_error_info,
        Some(crate::CodexErrorInfo::Unauthorized)
    );
}

#[test]
fn captured_typed_error_survives_later_drain_failure() {
    let transport = ocr_protocol::TransportBudget::new(64);
    let typed = crate::OcrRuntimeError::new(
        &json!({"message":"not retained","codexErrorInfo":"unauthorized"}),
        Some(false),
        None,
    );
    let captured = crate::ocr::capture_page_outcome::<()>(Ok(Err(typed.into())), &transport);
    let error = crate::ocr::finish_page_outcome(
        captured,
        Err(anyhow::anyhow!("ocr_stderr_cleanup_completion_unknown")),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<crate::OcrRuntimeError>().is_some());
}

#[tokio::test]
async fn capture_preserves_preobserved_overflow_and_actual_outer_timeout_codes() {
    let transport = ocr_protocol::TransportBudget::new(64);
    assert!(transport.admit(65).is_err());
    let error = crate::ocr::capture_page_outcome::<()>(
        Ok(Err(anyhow::anyhow!("captured_concrete_failure"))),
        &transport,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "ocr_total_transport_limit");

    let transport = ocr_protocol::TransportBudget::new(64);
    let timeout =
        tokio::time::timeout(Duration::ZERO, std::future::pending::<anyhow::Result<()>>()).await;
    let captured = crate::ocr::capture_page_outcome(timeout, &transport);
    let error =
        crate::ocr::finish_page_outcome(captured, Err(anyhow::anyhow!("later_cleanup_failure")))
            .unwrap_err();
    assert_eq!(error.to_string(), "ocr_page_deadline_completion_unknown");
}

#[test]
fn successful_capture_requires_final_meter_and_drain_safeguards() {
    let transport = ocr_protocol::TransportBudget::new(64);
    let captured = crate::ocr::capture_page_outcome(Ok(Ok(())), &transport);
    assert!(transport.admit(65).is_err());
    assert_eq!(
        crate::ocr::finish_page_outcome(captured, transport.check())
            .unwrap_err()
            .to_string(),
        "ocr_total_transport_limit"
    );
    assert_eq!(
        crate::ocr::finish_page_outcome(
            Ok(()),
            Err(anyhow::anyhow!("ocr_stderr_cleanup_completion_unknown")),
        )
        .unwrap_err()
        .to_string(),
        "ocr_stderr_cleanup_completion_unknown"
    );
    crate::ocr::finish_page_outcome(Ok(()), Ok(())).unwrap();
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

#[test]
fn official_runtime_policy_omits_obsolete_flag_and_preserves_restrictions() {
    // Frozen from the unmodified 3f1461 product helper, excluding only its
    // observed unsupported override. This catches collateral policy changes.
    let expected = json!({
        "agents.enabled": false,
        "analytics.enabled": false,
        "cloud.skills.enabled": false,
        "features.apps": false,
        "features.artifact": false,
        "features.browser_use": false,
        "features.browser_use_external": false,
        "features.code_mode.direct_only_tool_namespaces": [
            "gptgrep"
        ],
        "features.code_mode.enabled": false,
        "features.code_mode_host.disable_in_process_fallback": false,
        "features.code_mode_host.enabled": false,
        "features.code_mode_only": false,
        "features.code_mode_prewarm": false,
        "features.computer_use": false,
        "features.current_time_reminder": false,
        "features.deferred_executor": false,
        "features.fast_mode": true,
        "features.goals": false,
        "features.hooks": false,
        "features.image_generation": false,
        "features.in_app_browser": false,
        "features.memories": false,
        "features.multi_agent_v2": false,
        "features.plugins": false,
        "features.realtime_conversation": false,
        "features.request_permissions_tool": false,
        "features.send_message_to_user_async": false,
        "features.shell_tool": false,
        "features.sleep_tool": false,
        "features.token_budget": false,
        "features.tool_suggest": false,
        "features.view_image": false,
        "model_reasoning_effort": "max",
        "notify": [],
        "orchestrator.mcp.enabled": false,
        "project_doc_max_bytes": 0,
        "service_tier": "fast",
        "tools.experimental_request_user_input.enabled": false,
        "tools.update_plan.enabled": false,
        "web_search": "disabled"
    });
    let policy = crate::ocr::ocr_policy();
    assert!(policy.get("features.stable_environment_tools").is_none());
    assert_eq!(policy, expected);
    assert_eq!(policy["model_reasoning_effort"], "max");
    assert_eq!(policy["service_tier"], "fast");
    assert_eq!(policy["features.shell_tool"], false);
    assert_eq!(policy["features.view_image"], false);
    assert_eq!(policy["features.request_permissions_tool"], false);
    assert_eq!(policy["web_search"], "disabled");
    assert_eq!(policy["orchestrator.mcp.enabled"], false);
    assert_eq!(
        policy["features.code_mode.direct_only_tool_namespaces"],
        json!(["gptgrep"])
    );
}

#[test]
fn ocr_explicit_state_keeps_auth_home_and_refuses_missing_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let state = root.join("state");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(state.join("logs")).unwrap();
    let descriptor = root.join("model-catalog.json");
    let binary = Path::new("unused-native-binary");
    let command = ocr_command(binary, &root, &home, Some(&state), &descriptor).unwrap();
    let command = command.as_std();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect();
    assert!(
        args.contains(&format!("sqlite_home={}", serde_json::to_string(&state).unwrap()).into())
    );
    assert!(
        args.contains(
            &format!(
                "log_dir={}",
                serde_json::to_string(&state.join("logs")).unwrap()
            )
            .into()
        )
    );
    assert!(
        command
            .get_envs()
            .any(|(name, value)| name == "CODEX_HOME" && value == Some(home.as_os_str()))
    );
    assert!(
        command
            .get_envs()
            .any(|(name, value)| name == "CODEX_SQLITE_HOME" && value.is_none())
    );
    let legacy = ocr_command(binary, &root, &home, None, &descriptor).unwrap();
    let args: Vec<_> = legacy
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect();
    assert!(
        args.contains(&format!("sqlite_home={}", serde_json::to_string(&home).unwrap()).into())
    );
    assert!(!args.iter().any(|arg| arg.starts_with("log_dir=")));
    assert!(
        ocr_command(
            binary,
            &root,
            &home,
            Some(&root.join("missing")),
            &descriptor
        )
        .is_err()
    );
    std::fs::remove_dir(state.join("logs")).unwrap();
    assert!(ocr_command(binary, &root, &home, Some(&state), &descriptor).is_err());
}

#[cfg(unix)]
pub(crate) async fn wait_for_submitted_mock(pid_file: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "mock did not submit"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn retained_document_ocr_cleanup_joins_after_submitted_tool_future_is_dropped() {
    for timed_out in [false, true] {
        let fixture = Fixture::new();
        let (executor, context, pid_file) = mock_executor(&fixture, "hang", true);
        let original = context.snapshot().unwrap();
        let cleanup = crate::OcrCleanup::new(64).unwrap();
        let executor = executor.with_cleanup(cleanup.clone());
        let cancel = crate::OcrCancellation::default();
        let mut operation =
            Box::pin(executor.execute_cancellable(fixture.task.clone(), cancel.clone()));
        tokio::select! {
            result = &mut operation => panic!("mock ended before submission: {result:?}"),
            () = wait_for_submitted_mock(&pid_file) => {}
        }
        let pids: Vec<u32> = serde_json::from_slice(&fs::read(&pid_file).unwrap()).unwrap();
        assert!(!owned_pid_is_gone_or_zombie(pids[0]));
        if timed_out {
            assert!(
                tokio::time::timeout(Duration::from_millis(1), operation)
                    .await
                    .is_err()
            );
        } else {
            cancel.cancel();
            drop(operation);
        }
        assert!(cancel.is_cancelled());
        cleanup
            .cancel_and_drain(tokio::time::Instant::now() + Duration::from_secs(10))
            .await
            .unwrap();
        // These assertions happen immediately at barrier return: no eventual
        // polling window is allowed to conceal a detached worker.
        for pid in &pids {
            assert!(
                owned_pid_is_gone_or_zombie(*pid),
                "owned process outlived cleanup"
            );
        }
        let leader = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pids[0].to_string()])
            .output()
            .unwrap();
        assert!(leader.stdout.is_empty(), "owned leader was not reaped");
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 1);
        assert_eq!(snapshot.active_or_quarantined_slots, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
        assert_eq!(snapshot.terminal_admissions, 0);
        assert_eq!(snapshot.deadline_unix_ms, original.deadline_unix_ms);
        assert_eq!(snapshot.max_admissions, original.max_admissions);
        let late = executor
            .execute_cancellable(fixture.task.clone(), crate::OcrCancellation::default())
            .await
            .unwrap_err();
        assert_eq!(late.to_string(), "ocr_cleanup_closed");
        assert_eq!(context.snapshot().unwrap().charged_admissions, 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn retained_document_ocr_journal_failure_is_visible_and_never_synthesizes_terminal() {
    let fixture = Fixture::new();
    let (executor, _context, pid_file) = mock_executor(&fixture, "hang_journal", true);
    let cleanup = crate::OcrCleanup::new(64).unwrap();
    let executor = executor.with_cleanup(cleanup.clone());
    let cancel = crate::OcrCancellation::default();
    let mut operation =
        Box::pin(executor.execute_cancellable(fixture.task.clone(), cancel.clone()));
    tokio::select! {
        result = &mut operation => panic!("mock ended before submission: {result:?}"),
        () = wait_for_submitted_mock(&pid_file) => {}
    }
    let ledger = fixture.dir.path().join("mock-ocr-ledger.json");
    let submitted = fs::read(&ledger).unwrap();
    let rows: Value = serde_json::from_slice(&submitted).unwrap();
    assert_eq!(rows["reservations"].as_array().unwrap().len(), 1);
    assert_eq!(rows["reservations"][0]["status"], "submitted");
    let retained = fixture.dir.path().join("mock-ocr-ledger-retained.json");
    fs::rename(&ledger, &retained).unwrap();
    fs::create_dir(&ledger).unwrap(); // Deterministic synthetic journal I/O failure.
    cancel.cancel();
    let error = operation.await.unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("ocr_cancelled_completion_unknown")
    );
    assert!(error.to_string().contains("ocr_journal_closeout_unknown"));
    let error = cleanup
        .cancel_and_drain(tokio::time::Instant::now() + Duration::from_secs(10))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "ocr_owned_execution_cleanup_unknown");
    assert_eq!(fs::read(&retained).unwrap(), submitted);
    assert!(ledger.is_dir());
    let pids: Vec<u32> = serde_json::from_slice(&fs::read(pid_file).unwrap()).unwrap();
    assert!(pids.iter().all(|pid| owned_pid_is_gone_or_zombie(*pid)));
}

#[cfg(unix)]
#[tokio::test]
async fn document_ocr_cleanup_record_limit_refuses_before_another_worker_or_admission() {
    assert!(crate::OcrCleanup::new(0).is_err());
    assert!(crate::OcrCleanup::new(65).is_err());
    assert_eq!(crate::OcrCleanup::working_bytes(64).unwrap(), 65_536);
    let fixture = Fixture::new();
    let (executor, context, _) = mock_executor(&fixture, "complete", false);
    let cleanup = crate::OcrCleanup::new(1).unwrap();
    let executor = executor.with_cleanup(cleanup.clone());
    let first = executor
        .execute_cancellable(fixture.task.clone(), crate::OcrCancellation::default())
        .await
        .unwrap_err();
    assert_eq!(
        first.to_string(),
        "ocr_runtime_image_qualification_required"
    );
    let second = executor
        .execute_cancellable(fixture.task.clone(), crate::OcrCancellation::default())
        .await
        .unwrap_err();
    assert_eq!(second.to_string(), "ocr_cleanup_record_limit");
    assert_eq!(context.snapshot().unwrap().charged_admissions, 0);
    cleanup
        .cancel_and_drain(tokio::time::Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
}

#[tokio::test]
async fn observed_terminal_with_failed_owned_stop_keeps_charge_and_slot() {
    for primary_timeout in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let ledger = directory.path().join("stop-failure-ledger.json");
        let context = OcrExecutionContext::new(
            "stop-failure",
            HASH,
            SystemTime::now() + Duration::from_secs(30),
            2,
        )
        .unwrap()
        .with_ledger(&ledger)
        .unwrap();
        let original_deadline = context.snapshot().unwrap().deadline_unix_ms;
        let mut admission = context.reserve(HASH, HASH).await.unwrap();
        admission.submitted().unwrap();
        // Model the protocol's validated terminal observation before local cleanup.
        admission.observe_terminal();
        let primary: anyhow::Result<()> = if primary_timeout {
            Err(anyhow::anyhow!("ocr_page_deadline_completion_unknown"))
        } else {
            Ok(())
        };
        let mut safeguards_called = false;
        let error = crate::ocr::finish_owned_page_outcome(
            primary,
            Err(anyhow::anyhow!("injected_owned_stop_reap_failure")),
            true,
            || {
                // Drain and transport succeed in the reported failure scenario.
                safeguards_called = true;
                admission.confirm_terminal()
            },
        )
        .unwrap_err();
        assert!(!safeguards_called);
        assert!(
            error
                .to_string()
                .contains("ocr_owned_execution_cleanup_unknown")
        );
        if primary_timeout {
            assert!(
                error
                    .to_string()
                    .starts_with("ocr_page_deadline_completion_unknown")
            );
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string() == "ocr_page_deadline_completion_unknown")
            );
        }
        let before_closeout: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(before_closeout["reservations"][0]["status"], "submitted");
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 1);
        assert_eq!(snapshot.active_or_quarantined_slots, 1);
        assert_eq!(snapshot.terminal_admissions, 0);

        // The production registered-worker path performs this explicit closeout.
        admission.closeout().unwrap();
        drop(admission);
        let snapshot = context.snapshot().unwrap();
        assert_eq!(snapshot.charged_admissions, 1);
        assert_eq!(snapshot.active_or_quarantined_slots, 1);
        assert_eq!(snapshot.quarantined_slots, 1);
        assert_eq!(snapshot.terminal_admissions, 0);
        assert_eq!(snapshot.deadline_unix_ms, original_deadline);
        assert_eq!(snapshot.max_admissions, 2);
        let after_closeout: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(
            after_closeout["reservations"][0]["status"],
            "completion_unknown"
        );
    }

    // A successful owned stop/reap plus terminal safeguards can still release.
    let context = OcrExecutionContext::new(
        "stop-success",
        HASH,
        SystemTime::now() + Duration::from_secs(30),
        2,
    )
    .unwrap();
    let mut admission = context.reserve(HASH, HASH).await.unwrap();
    admission.submitted().unwrap();
    admission.observe_terminal();
    crate::ocr::finish_owned_page_outcome(Ok(()), Ok(()), true, || admission.confirm_terminal())
        .unwrap();
    admission.closeout().unwrap();
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.charged_admissions, 1);
    assert_eq!(snapshot.active_or_quarantined_slots, 0);
    assert_eq!(snapshot.terminal_admissions, 1);
}
