use super::*;
use crate::{OcrImageTransport, OcrRuntimeProfile};
use std::fs;
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn private_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
fn runtime() -> OcrRuntimeProfile {
    OcrRuntimeProfile::unqualified("a".repeat(40), HASH.into(), HASH.into()).unwrap()
}
#[test]
fn foreground_native_state_overlap_refuses_before_request_journal_or_admission() {
    for selected in 0..3 {
        for relation in ["equal", "ancestor", "descendant", "disjoint"] {
            let dir = private_root();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("source");
            let prepared = root.join("prepared");
            let actor = root.join("actor");
            let mut outputs = [root.join("spool"), root.join("claims"), root.join("raw")];
            let state = match relation {
                "equal" => outputs[selected].clone(),
                "ancestor" => {
                    let state = root.join("native-state");
                    outputs[selected] = state.join("output");
                    state
                }
                "descendant" => outputs[selected].join("native-state"),
                _ => root.join("native-state"),
            };
            for path in [&source, &prepared, &actor, &state]
                .into_iter()
                .chain(outputs.iter())
            {
                fs::create_dir_all(path).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
            fs::create_dir(state.join("logs")).unwrap();
            let clock =
                RunClockAnchor::capture(std::time::SystemTime::now() + Duration::from_secs(60))
                    .unwrap();
            let context = OcrExecutionContext::with_phase_limits(
                "state-overlap-test",
                HASH,
                clock.clone(),
                crate::OcrExecutionLimits {
                    max_admissions: 2,
                    max_concurrency: 2,
                    ledger_bytes: 8192,
                },
            )
            .unwrap()
            .join_original_ledger(root.join("ocr-ledger.json"), &clock)
            .unwrap();
            let options =
                ForegroundOcrOptions::new(source.clone(), outputs[0].clone(), outputs[1].clone());
            let catalog =
                PreparedOcrTaskCatalog::prepare(&source, &prepared, Vec::new(), &options.resources)
                    .unwrap();
            let result = prepare_foreground_ocr(
                options,
                OcrHostConfig {
                    codex_bin: "must-not-launch".into(),
                    codex_home: actor,
                    codex_state_dir: Some(state),
                    prepared_root: prepared,
                    raw_results_dir: outputs[2].clone(),
                    runtime_profile: runtime(),
                    transport: OcrImageTransport::DynamicImages,
                },
                context.clone(),
                &clock,
                catalog,
                b"12345678901234567890123456789012",
            );
            if relation == "disjoint" {
                assert!(result.is_ok(), "disjoint state must remain supported");
            } else {
                assert_eq!(
                    result.err().expect("state alias was admitted").to_string(),
                    "ocr_service_namespace_overlap",
                    "output {selected}, relation {relation}"
                );
                assert!(!outputs[1].join("claims.json").exists());
            }
            assert_eq!(context.snapshot().unwrap().charged_admissions, 0);
        }
    }
}
fn service(root: &Path) -> Arc<Service> {
    let claims = root.join("claims");
    let spool = root.join("spool");
    fs::create_dir(&claims).unwrap();
    fs::create_dir(&spool).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&claims, &spool] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let budget = ResourceBudget::default();
    let context = OcrExecutionContext::pure_fixture(2);
    let executor = OcrHostExecutor::new(
        OcrHostConfig {
            codex_bin: "not-a-runtime".into(),
            codex_home: root.join("actor"),
            codex_state_dir: None,
            prepared_root: root.into(),
            raw_results_dir: root.join("raw"),
            runtime_profile: runtime(),
            transport: OcrImageTransport::DynamicImages,
        },
        context,
    )
    .unwrap();
    let working = WorkingBudget::new(budget.owned_working_bytes);
    let retained = Arc::new(
        working
            .reserve(SERVICE_WORKING + claim_working_bound(2).unwrap())
            .unwrap(),
    );
    Arc::new(Service {
        catalog: PreparedOcrTaskCatalog {
            source_root: root.into(),
            prepared_root: root.into(),
            tasks: BTreeMap::new(),
            working: working.clone(),
            _retained: Arc::new(working.reserve(65_536).unwrap()),
            _control: None,
            _context: None,
        },
        executor,
        compatibility_executor: None,
        compatibility: None,
        resources: budget.clone(),
        auth: token_digest(b"12345678901234567890123456789012"),
        active: Arc::new(Semaphore::new(2)),
        claims: Arc::new(
            Claims::attach(
                PrivateDirectory::existing(&claims).unwrap(),
                HASH,
                2,
                &budget,
            )
            .unwrap(),
        ),
        spool: Arc::new(PrivateDirectory::existing(&spool).unwrap()),
        jobs: Mutex::new(Vec::new()),
        stop: OcrCancellation::default(),
        working,
        _retained: retained,
        custody_gate: None,
    })
}
fn multipart(image: &[u8], language: &str) -> Vec<u8> {
    let mut bytes=b"--fixed\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    bytes.extend_from_slice(image);
    bytes.extend_from_slice(
        b"\r\n--fixed\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n",
    );
    bytes.extend_from_slice(language.as_bytes());
    bytes.extend_from_slice(b"\r\n--fixed--\r\n");
    bytes
}
fn png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[0, 0, 0, 255, 255, 255],
            2,
            1,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    bytes
}
#[test]
fn compatibility_multipart_extent_and_caps_are_real() {
    let bytes = png();
    let wire = multipart(&bytes, "en-US");
    let parsed = parse_multipart("multipart/form-data; boundary=fixed", &wire, 1024).unwrap();
    assert_eq!(parsed.bytes, bytes);
    assert_eq!(parsed.language, "en-US");
    assert_eq!(
        png_dimensions(&bytes, &ResourceBudget::default()).unwrap(),
        (2, 1)
    );
    assert!(
        parse_multipart(
            "multipart/form-data; boundary=fixed",
            &wire,
            bytes.len() - 1
        )
        .is_err()
    );
    assert!(
        parse_multipart(
            "multipart/form-data; boundary=fixed",
            &multipart(&bytes, "en;prompt"),
            1024
        )
        .is_err()
    );
    let mut huge = bytes.clone();
    huge[16..20].copy_from_slice(&8_000_001u32.to_be_bytes());
    assert!(png_dimensions(&huge, &ResourceBudget::default()).is_err());
    let mut truncated = bytes;
    truncated.truncate(40);
    assert!(png_dimensions(&truncated, &ResourceBudget::default()).is_err());
}
#[tokio::test]
async fn in_memory_http_auth_mode_body_and_busy_refuse_before_execution() {
    let root = private_root();
    let service = service(root.path());
    let request = Request::builder()
        .method("POST")
        .uri("/ocr")
        .header("content-type", "multipart/form-data; boundary=fixed")
        .body(Full::new(Bytes::from(multipart(&png(), "en"))))
        .unwrap();
    assert_eq!(
        service.handle(request).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let browser = Request::builder()
        .method("POST")
        .uri("/ocr")
        .header("authorization", "Bearer 12345678901234567890123456789012")
        .header("origin", "https://browser.invalid")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        service.handle(browser).await.status(),
        StatusCode::BAD_REQUEST
    );
    let valid = Request::builder()
        .method("POST")
        .uri("/ocr")
        .header("content-type", "multipart/form-data; boundary=fixed")
        .header("authorization", "Bearer 12345678901234567890123456789012")
        .body(Full::new(Bytes::from(multipart(&png(), "en"))))
        .unwrap();
    assert_eq!(
        service.handle(valid).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let too_large = Request::builder()
        .method("POST")
        .uri("/v1/ocr/tasks")
        .header("content-type", "application/json")
        .header("authorization", "Bearer 12345678901234567890123456789012")
        .body(Full::new(Bytes::from(vec![0; TASK_BYTES + 1])))
        .unwrap();
    assert_eq!(
        service.handle(too_large).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let _first = service.active.clone().acquire_owned().await.unwrap();
    let _second = service.active.clone().acquire_owned().await.unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ocr/tasks")
        .header("authorization", "Bearer 12345678901234567890123456789012")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        service.handle(request).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(service.jobs.lock().unwrap().is_empty());
    assert!(
        serde_json::from_slice::<ClaimState>(
            &service
                .claims
                .directory
                .read("claims.json", CLAIM_BYTES)
                .unwrap()
        )
        .unwrap()
        .entries
        .is_empty()
    );
}
#[test]
fn durable_claim_unknown_cache_digest_and_phase_capacity_are_preserved() {
    let dir = private_root();
    let budget = ResourceBudget::default();
    let claims = Claims::attach(
        PrivateDirectory::existing(dir.path()).unwrap(),
        HASH,
        2,
        &budget,
    )
    .unwrap();
    assert!(matches!(claims.begin(HASH).unwrap(), ClaimStart::New));
    assert!(matches!(claims.begin(HASH).unwrap(), ClaimStart::Unknown));
    let second = "b".repeat(64);
    assert!(matches!(claims.begin(&second).unwrap(), ClaimStart::New));
    claims.complete(&second, b"{\"synthetic\":true}").unwrap();
    assert!(matches!(
        claims.begin(&second).unwrap(),
        ClaimStart::Cached(_)
    ));
    assert!(claims.begin(&"c".repeat(64)).is_err());
    let reopened = Claims::attach(
        PrivateDirectory::existing(dir.path()).unwrap(),
        HASH,
        2,
        &budget,
    )
    .unwrap();
    assert!(matches!(reopened.begin(HASH).unwrap(), ClaimStart::Unknown));
    fs::write(dir.path().join(format!("{second}.json")), b"{}").unwrap();
    assert!(reopened.begin(&second).is_err());
    fs::remove_file(dir.path().join("claims.json")).unwrap();
    assert!(
        Claims::attach(
            PrivateDirectory::existing(dir.path()).unwrap(),
            HASH,
            2,
            &budget
        )
        .is_err()
    );
}
#[test]
fn compatibility_mode_has_distinct_frozen_policy_and_old_receipt_cannot_authorize() {
    let mut base = runtime();
    base.accepted_image_qualification_sha256 = Some(HASH.into());
    let mut profile = CompatibilityRuntimeProfile::unqualified(base).unwrap();
    let digest = profile.sha256().unwrap();
    profile.accepted_qualification_sha256 = Some(HASH.into());
    assert!(profile.validate().is_err());
    profile.accepted_qualification_sha256 = Some("b".repeat(64));
    profile.validate().unwrap();
    assert_eq!(digest, profile.sha256().unwrap());
    profile.mode_policy_sha256 = "c".repeat(64);
    assert!(profile.validate().is_err());
}
#[tokio::test]
async fn cloned_cancellation_is_observed_before_and_during_wait() {
    let cancel = OcrCancellation::default();
    let clone = cancel.clone();
    let waiting = tokio::spawn(async move {
        clone.wait_service().await;
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), cancel.wait_service())
        .await
        .unwrap();
}
#[test]
fn private_retention_reserves_shared_bytes_and_keeps_exact_modes() {
    let dir = private_root();
    let budget = ResourceBudget::default();
    let directory = PrivateDirectory::existing(dir.path()).unwrap();
    directory.write_once("owned.json", b"abc", &budget).unwrap();
    let charged = budget.staging_used();
    assert_eq!(charged, 3);
    directory.write_once("owned.json", b"abc", &budget).unwrap();
    assert_eq!(budget.staging_used(), charged);
    assert!(
        directory
            .write_once("owned.json", b"changed", &budget)
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.path().join("owned.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
fn native_task(source: &Path, prepared: &Path) -> OcrTask {
    use gptgrep_parse::{BoundingBox, OcrRegion};
    fs::write(source.join("raw.source"), b"synthetic raw source").unwrap();
    let bytes = png();
    fs::write(prepared.join("page.png"), &bytes).unwrap();
    let source_sha = hash(b"synthetic raw source");
    let image_sha = hash(&bytes);
    let image = OcrImage {
        artifact: ArtifactRef {
            id: artifact_id(
                &source_sha,
                HASH,
                &ArtifactKind::Rendition,
                Some(1),
                None,
                &image_sha,
            )
            .unwrap(),
            kind: ArtifactKind::Rendition,
            relative_path: "page.png".into(),
            sha256: image_sha,
            profile_sha256: HASH.into(),
            bytes: bytes.len() as u64,
            mime: "image/png".into(),
            page: Some(1),
            region_id: None,
        },
        path: prepared.join("page.png"),
        width: 2,
        height: 1,
        page_to_raster: AffineTransform::IDENTITY,
    };
    let mut task = OcrTask {
        schema_version: "gptgrep.ocr.task.v1".into(),
        id: String::new(),
        task_sha256: String::new(),
        source_sha256: source_sha,
        prepared_sha256: HASH.into(),
        source_path: source.join("raw.source"),
        page: 1,
        page_width_points: 2.0,
        page_height_points: 1.0,
        regions: vec![OcrRegion {
            id: "region".into(),
            bbox: BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 1.0,
            },
            required: true,
            reason: "synthetic".into(),
        }],
        images: vec![image],
        native_text: String::new(),
        layout_context: String::new(),
        profile: OcrProfile::default(),
        budget: OcrTaskBudget::default(),
    };
    task.task_sha256 = gptgrep_parse::ocr_task_digest(&task).unwrap();
    task.id = format!("ocr-{}", task.task_sha256);
    task
}
#[test]
fn full_task_rehydrates_only_bound_catalog_paths_and_detects_namespace_mutation() {
    let source = private_root();
    let prepared = private_root();
    let budget = ResourceBudget::default();
    let original = native_task(source.path(), prepared.path());
    let canonical = bounded_json(&original, TASK_BYTES).unwrap();
    let catalog = PreparedOcrTaskCatalog::prepare(
        source.path(),
        prepared.path(),
        vec![original.clone()],
        &budget,
    )
    .unwrap();
    let mut wire: OcrTask = serde_json::from_slice(&canonical).unwrap();
    assert!(wire.source_path.as_os_str().is_empty());
    wire.source_path = PathBuf::from("caller-path-not-restored");
    wire.images[0].path = PathBuf::from("caller-image-not-restored");
    let restored = catalog.restore(wire.clone()).unwrap();
    assert_eq!(restored.source_path, original.source_path);
    assert_eq!(restored.images[0].path, original.images[0].path);
    wire.native_text = "changed".into();
    assert!(catalog.restore(wire).is_err());
    fs::create_dir(prepared.path().join("raw")).unwrap();
    let executor = OcrHostExecutor::new(
        OcrHostConfig {
            codex_bin: "not-a-runtime".into(),
            codex_home: source.path().join("actor"),
            codex_state_dir: None,
            prepared_root: prepared.path().into(),
            raw_results_dir: prepared.path().join("raw"),
            runtime_profile: runtime(),
            transport: OcrImageTransport::DynamicImages,
        },
        OcrExecutionContext::pure_fixture(2),
    )
    .unwrap()
    .with_service_roots(source.path())
    .unwrap();
    executor.check_service_task(&original).unwrap();
    fs::rename(
        source.path().join("raw.source"),
        source.path().join("old.source"),
    )
    .unwrap();
    fs::write(source.path().join("raw.source"), b"changed source").unwrap();
    assert!(executor.check_service_task(&original).is_err());
}
#[test]
fn compatibility_task_is_source_scoped_and_not_a_legacy_result() {
    let root = private_root();
    let service = service(root.path());
    let input = MultipartImage {
        bytes: png(),
        language: "fr".into(),
    };
    let task = service.compatibility_task(input).unwrap();
    task.validate().unwrap();
    let internal = crate::ocr_compat::internal_task(&task).unwrap();
    validate_ocr_task_metadata(&internal).unwrap();
    assert_eq!(internal.source_path, task.image.path);
    assert_eq!(internal.source_sha256, task.image.artifact.sha256);
    let schema =
        crate::ocr_compat::schema(&task, &crate::ocr_compat::mode_policy_digest().unwrap());
    assert_eq!(schema["properties"]["language"]["const"], "fr");
    assert_eq!(
        schema["properties"]["regions"]["items"]["required"],
        serde_json::json!(["text", "bbox", "confidence"])
    );
    assert_eq!(
        schema["properties"]["regions"]["items"]["properties"]["bbox"]["const"],
        serde_json::json!([0, 0, 2, 1])
    );
}
#[tokio::test]
async fn streamed_body_limit_rejects_the_crossing_frame_without_reading_more() {
    struct Chunks {
        frames: std::vec::IntoIter<Bytes>,
        observed: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl hyper::body::Body for Chunks {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>>
        {
            self.observed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Ready(
                self.frames
                    .next()
                    .map(|bytes| Ok(hyper::body::Frame::data(bytes))),
            )
        }
    }
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let body = Chunks {
        frames: vec![
            Bytes::from_static(b"abc"),
            Bytes::from_static(b"de"),
            Bytes::from_static(b"later"),
        ]
        .into_iter(),
        observed: observed.clone(),
    };
    assert!(collect_body(body, 4).await.is_err());
    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 2);
}
#[derive(Clone, Default)]
struct MockWrites(Arc<Mutex<Vec<u8>>>);
impl tokio::io::AsyncWrite for MockWrites {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut written = self.0.lock().unwrap();
        assert!(written.len() + bytes.len() <= 16_777_216);
        written.extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
#[tokio::test]
async fn compatibility_mode_uses_real_mediated_image_and_binds_language_schema_receipt() {
    use serde_json::json;
    let root = private_root();
    let service = service(root.path());
    let task = service
        .compatibility_task(MultipartImage {
            bytes: png(),
            language: "fra".into(),
        })
        .unwrap();
    let internal = crate::ocr_compat::internal_task(&task).unwrap();
    let mode = crate::ocr_compat::mode_policy_digest().unwrap();
    let answer=json!({"schema_version":"gptgrep.ocr.compat.result.v1","task_sha256":task.task_sha256,
        "image_sha256":task.image.artifact.sha256,"language":"fra","mode_sha256":mode,"status":"complete",
        "regions":[{"text":"Synthetic transcript","bbox":[0,0,2,1],"confidence":0.76}]}).to_string();
    let frames = vec![
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":{"config":{"mcp_servers":{}}}}),
        json!({"id":3,"result":{"thread":{"id":"synthetic-thread"},"model":"gpt-6-luna","modelProvider":"openai",
            "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"fast"}}),
        json!({"id":4,"result":{"turn":{"id":"synthetic-turn"}}}),
        json!({"id":100,"method":"item/tool/call","params":{"threadId":"synthetic-thread","turnId":"synthetic-turn",
            "callId":"synthetic-call","namespace":"gptgrep_ocr","tool":"ocr_resource","arguments":{"resource":"image",
                "task_sha256":internal.task_sha256,"artifact_id":task.image.artifact.id,"sha256":task.image.artifact.sha256}}}),
        json!({"method":"turn/completed","params":{"threadId":"synthetic-thread","turn":{"id":"synthetic-turn","status":"completed",
            "error":null,"items":[{"type":"agentMessage","id":"synthetic-message","phase":"final_answer","text":answer}]}}}),
    ];
    let transcript = frames
        .into_iter()
        .flat_map(|frame| {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect::<Vec<_>>();
    let context = OcrExecutionContext::pure_fixture(2);
    let writes = MockWrites::default();
    let mut admission = context.reserve(&task.task_sha256, HASH).await.unwrap();
    let outcome = crate::ocr_protocol::run_mode(
        std::io::Cursor::new(transcript),
        writes.clone(),
        Path::new("."),
        PreparedPage::new(internal, service.spool.path.as_path()).unwrap(),
        HASH,
        &mut admission,
        crate::ocr_protocol::OcrRunBudget {
            deadline: tokio::time::Instant::now() + Duration::from_secs(1),
            transport: crate::ocr_protocol::TransportBudget::new(16_777_216),
        },
        Some(task.clone()),
    )
    .await
    .unwrap();
    outcome.page.verify_delivery().unwrap();
    assert_eq!(outcome.page.delivery_events.len(), 1);
    admission.confirm_terminal().unwrap();
    assert_eq!(context.snapshot().unwrap().charged_admissions, 1);
    assert_eq!(context.snapshot().unwrap().terminal_admissions, 1);
    let writes = writes.0.lock().unwrap();
    let outgoing = writes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let request = outgoing
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap();
    assert_eq!(request["params"]["effort"], "max");
    assert_eq!(request["params"]["serviceTier"], "fast");
    assert_eq!(
        request["params"]["outputSchema"]["properties"]["language"]["const"],
        "fra"
    );
    assert_eq!(
        request["params"]["outputSchema"],
        crate::ocr_compat::schema(&task, &mode)
    );
    assert!(
        outgoing.iter().any(|frame| frame["id"] == 100
            && frame["result"]["contentItems"][1]["type"] == "inputImage")
    );
    assert_eq!(
        outgoing
            .iter()
            .filter(|frame| frame["method"] == "turn/start")
            .count(),
        1
    );
    assert!(crate::ocr_accounting::valid_digest(&outcome.request_sha256));
}

#[test]
fn service_bearer_variable_is_removed_only_from_owned_child_environment() {
    let mut child = tokio::process::Command::new("not-a-runtime");
    child.env("CODEX_HOME", "selected-native-account");
    crate::ocr::strip_service_secret(&mut child, "OCR_PRIVATE_SERVICE_TOKEN").unwrap();
    let vars = child.as_std().get_envs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        vars[std::ffi::OsStr::new("OCR_PRIVATE_SERVICE_TOKEN")],
        None
    );
    assert_eq!(vars[std::ffi::OsStr::new("GPTGREP_OCR_SERVE_TOKEN")], None);
    assert_eq!(
        vars[std::ffi::OsStr::new("CODEX_HOME")],
        Some(std::ffi::OsStr::new("selected-native-account"))
    );
    assert!(crate::ocr::strip_service_secret(&mut child, "HOME").is_err());
}

/// A held blocking worker, not a runtime/custody bypass. Releasing it returns an
/// explicit fixture failure before executor/native admission on every path.
pub(super) struct CustodyGate {
    entered: std::sync::atomic::AtomicUsize,
    released: Mutex<bool>,
    condition: std::sync::Condvar,
    changed: tokio::sync::Notify,
}
impl CustodyGate {
    fn new() -> Self {
        Self {
            entered: 0.into(),
            released: Mutex::new(false),
            condition: std::sync::Condvar::new(),
            changed: tokio::sync::Notify::new(),
        }
    }
    pub(super) fn wait(&self) -> Result<()> {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_waiters();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.condition.wait(released).unwrap();
        }
        Err(anyhow!("synthetic_custody_stopped"))
    }
    async fn entered(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.entered.load(std::sync::atomic::Ordering::SeqCst) >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.condition.notify_all();
    }
}
fn full_request(task: &OcrTask) -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri("/v1/ocr/tasks")
        .header("authorization", "Bearer 12345678901234567890123456789012")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&serde_json::json!({
            "schema_version":"gptgrep.ocr.service.request.v1", "task":task}))
            .unwrap(),
        )))
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_new_and_cached_custody_retains_capacity_and_shutdown_waits() {
    for cached in [false, true] {
        let root = private_root();
        let mut service = service(root.path());
        let first = native_task(root.path(), root.path());
        // The supported ordinary frame budget remains unchanged at 4MiB.
        let mut second = first.clone();
        second.native_text = "second source-only context".into();
        second.task_sha256 = gptgrep_parse::ocr_task_digest(&second).unwrap();
        second.id = format!("ocr-{}", second.task_sha256);
        let gate = Arc::new(CustodyGate::new());
        let mutable = Arc::get_mut(&mut service).unwrap();
        let total =
            65_536 + catalog_task_bound(&first).unwrap() + catalog_task_bound(&second).unwrap();
        let share = Arc::new(mutable.working.reserve(total).unwrap());
        mutable.catalog = PreparedOcrTaskCatalog::prepare_admitted(
            root.path(),
            root.path(),
            vec![first.clone(), second.clone()],
            &mutable.resources,
            mutable.working.clone(),
            share,
        )
        .unwrap();
        mutable.custody_gate = Some(gate.clone());
        if cached {
            for task in [&first, &second] {
                let key = Job::Full(Box::new(task.clone()))
                    .key(None, service.executor.runtime_profile_digest().unwrap())
                    .unwrap();
                assert!(matches!(
                    service.claims.begin(&key).unwrap(),
                    ClaimStart::New
                ));
                service
                    .claims
                    .complete(&key, b"{\"synthetic\":true}")
                    .unwrap();
            }
        }
        let before = service.working.used();
        let first_service = service.clone();
        let first_handler =
            tokio::spawn(async move { first_service.handle(full_request(&first)).await });
        gate.entered(1).await;
        first_handler.abort();
        let _ = first_handler.await;
        let second_service = service.clone();
        let second_handler =
            tokio::spawn(async move { second_service.handle(full_request(&second)).await });
        gate.entered(2).await;
        second_handler.abort();
        let _ = second_handler.await;
        assert_eq!(service.active.available_permits(), 0);
        assert!(service.working.used() > before);
        // An authenticated third request cannot acquire a slot even after both
        // HTTP futures have gone. Neither New nor Cached detaches custody work.
        let third = Request::builder()
            .method("POST")
            .uri("/v1/ocr/tasks")
            .header("authorization", "Bearer 12345678901234567890123456789012")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(
            service.handle(third).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        service.stop.cancel();
        let shutdown_service = service.clone();
        let shutdown = tokio::spawn(async move { shutdown_service.wait_owned_jobs().await });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(service.active.available_permits(), 2);
        assert_eq!(service.working.used(), before);
        assert_eq!(
            service
                .executor
                .service_admissions_for_test()
                .charged_admissions,
            0
        );
    }
}
#[test]
fn aggregate_working_reserves_retained_state_two_jobs_and_refuses_before_next_allocation() {
    let budget = ResourceBudget::default();
    let shared = WorkingBudget::new(budget.owned_working_bytes);
    let state = Arc::new(
        shared
            .reserve(
                SERVICE_WORKING
                    + claim_working_bound(24).unwrap()
                    + 65_536
                    + OcrExecutionContext::pure_fixture(24)
                        .service_working_bound()
                        .unwrap(),
            )
            .unwrap(),
    );
    let image = png();
    let job = job_working_bound(&[(2, 1, image.len() as u64)], &OcrTaskBudget::default()).unwrap()
        + multipart(&image, "en").len() as u64 * BODY_FACTOR;
    let first = shared.reserve(job).unwrap();
    let second = shared.clone().reserve(job).unwrap();
    assert!(shared.used() < budget.owned_working_bytes);
    // Default supports TWO ordinary full 4MiB frame jobs, while an arbitrary
    // incoming frame may need additional dynamic admission before parsing.
    let tight = WorkingBudget::new(
        (SERVICE_WORKING
            + claim_working_bound(24).unwrap()
            + 65_536
            + OcrExecutionContext::pure_fixture(24)
                .service_working_bound()
                .unwrap())
            + job * 2
            - 1,
    );
    let retained = tight
        .reserve(
            SERVICE_WORKING
                + claim_working_bound(24).unwrap()
                + 65_536
                + OcrExecutionContext::pure_fixture(24)
                    .service_working_bound()
                    .unwrap(),
        )
        .unwrap();
    let one = tight.reserve(job).unwrap();
    let allocation_started = std::sync::atomic::AtomicBool::new(false);
    let attempt = (|| -> Result<()> {
        let _lease = tight.clone().reserve(job)?;
        allocation_started.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    })();
    assert!(attempt.is_err());
    assert!(!allocation_started.load(std::sync::atomic::Ordering::SeqCst));
    drop(one);
    drop(retained);
    assert_eq!(tight.used(), 0);
    drop(first);
    drop(second);
    drop(state);
    assert_eq!(shared.used(), 0);
}
#[tokio::test]
async fn response_bytes_clones_and_slices_retain_working_charge_after_owner_finishes() {
    let shared = WorkingBudget::new(4096);
    let lease = Arc::new(shared.reserve(4096).unwrap());
    let source = private_root();
    let prepared = private_root();
    let job = Job::Full(Box::new(native_task(source.path(), prepared.path())));
    let response = job
        .response(b"{\"retained\":true}".to_vec(), lease.clone())
        .unwrap();
    drop(lease);
    let data = response.into_body().collect().await.unwrap().to_bytes();
    let cloned = data.clone();
    let sliced = data.slice(0..1);
    assert_eq!(shared.used(), 4096);
    assert!(shared.reserve(1).is_err());
    drop(data);
    drop(cloned);
    assert_eq!(shared.used(), 4096);
    drop(sliced);
    assert_eq!(shared.used(), 0);
    assert!(shared.reserve(4096).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_join_failure_still_drains_the_other_registered_custody_owner() {
    let root = private_root();
    let service = service(root.path());
    let gate = Arc::new(CustodyGate::new());
    let permit = service.active.clone().acquire_owned().await.unwrap();
    let lease = service.working.reserve(4096).unwrap();
    let worker_gate = gate.clone();
    let worker = tokio::spawn(async move {
        let _permit = permit;
        let _lease = lease;
        let _ = tokio::task::spawn_blocking(move || worker_gate.wait()).await;
    });
    let failed = tokio::spawn(async { panic!("synthetic owner join failure") });
    gate.entered(1).await;
    service.jobs.lock().unwrap().extend([failed, worker]);
    let shutdown_service = service.clone();
    let shutdown = tokio::spawn(async move { shutdown_service.wait_owned_jobs().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    assert_eq!(service.active.available_permits(), 1);
    gate.release();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(service.active.available_permits(), 2);
}
