//! Public/synthetic discriminating tests, authored only. Future execution needs
//! checked external TMPDIR admission; the image case additionally needs PDFium.
use super::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    control_path: PathBuf,
    config: Control,
}
impl Fixture {
    fn new(text: &str) -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        fs::create_dir(root.join("staging"))?;
        let path = root.join("source.txt");
        fs::write(&path, text)?;
        let config = Control {
            schema: CONTROL_SCHEMA.into(),
            invocation_id: "synthetic-invocation".into(),
            nonce: "17".repeat(32),
            staging_root: root.join("staging"),
            sources: vec![Source {
                alias: "approved".into(),
                path,
                bytes: text.len() as u64,
                sha256: digest(text.as_bytes()),
            }],
            resources: ResourceBudget::default(),
            toolbox: ToolboxControl {
                request_bytes: 65_536,
                reply_bytes: REPLY_MAX,
                artifact_window_bytes: 32_768,
                operation_working_bytes: 33_554_432,
            },
            frontend: FrontendBounds {
                session_timeout_ms: 60_000,
                request_timeout_ms: 30_000,
                max_requests: 1000,
                request_bytes: 65_536,
                total_wire_bytes: 16_777_216,
                output_file_bytes: 8_388_608,
            },
            native: NativeControl {
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
            ocr: None,
        };
        let control_path = root.join("control.json");
        let fixture = Self {
            _temporary: temporary,
            root,
            control_path,
            config,
        };
        fixture.save()?;
        Ok(fixture)
    }
    fn save(&self) -> Result<String> {
        let bytes = serde_json::to_vec(&self.config)?;
        fs::write(&self.control_path, &bytes)?;
        Ok(digest(&bytes))
    }
    fn session(&self) -> Result<Session> {
        Session::new(self.config.clone(), Instant::now())
    }
}
fn bounds() -> FrontendBounds {
    FrontendBounds {
        session_timeout_ms: 1000,
        request_timeout_ms: 1000,
        max_requests: 3,
        request_bytes: 16,
        total_wire_bytes: 4096,
        output_file_bytes: 1024,
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[test]
fn escaped_frames_count_the_actual_envelope_and_one_lf() -> Result<()> {
    let value = json!({"id":"quoted\\\"\u{0}é","text":"\\\"\u{0}中\n".repeat(80)});
    let count = framed_count(&value, 4096, deadline())?;
    let bytes = frame(&value, count as usize, deadline())?;
    assert_eq!(bytes.len() as u64, count);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert_eq!(serde_json::from_slice::<Value>(&bytes)?, value);
    assert!(frame(&value, bytes.len() - 1, deadline()).is_err());
    assert!(framed_count(&value, count - 1, deadline()).is_err());
    Ok(())
}

#[tokio::test]
async fn input_overflow_and_unterminated_eof_never_become_submissions() -> Result<()> {
    let mut limits = bounds();
    limits.request_bytes = 8;
    let mut quota = Quota {
        requests: 0,
        wire: 0,
        bounds: limits.clone(),
    };
    let mut reader = &b"12345678\n"[..];
    let error = read_frame(&mut reader, &mut quota, deadline())
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "parse_agent_request_limit");
    assert_eq!(quota.requests, 0);
    assert_eq!(quota.wire, 9);
    let mut quota = Quota {
        requests: 0,
        wire: 0,
        bounds: limits,
    };
    let mut reader = &b"{}"[..];
    assert_eq!(
        read_frame(&mut reader, &mut quota, deadline())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_unterminated_request"
    );
    assert_eq!(quota.requests, 0);
    Ok(())
}

#[tokio::test]
async fn exact_count_eof_and_total_wire_refusal_are_distinct() -> Result<()> {
    let mut limits = bounds();
    limits.max_requests = 1;
    let mut quota = Quota {
        requests: 1,
        wire: 0,
        bounds: limits.clone(),
    };
    let mut empty = &b""[..];
    assert!(
        read_frame(&mut empty, &mut quota, deadline())
            .await?
            .is_none()
    );
    let mut another = &b"{}\n"[..];
    assert_eq!(
        read_frame(&mut another, &mut quota, deadline())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_request_count"
    );
    let mut quota = Quota {
        requests: 0,
        wire: limits.total_wire_bytes,
        bounds: limits,
    };
    let mut reader = &b"{}\n"[..];
    assert_eq!(
        read_frame(&mut reader, &mut quota, deadline())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_total_wire_limit"
    );
    assert_eq!(quota.requests, 0);
    let mut reader = &b"{}\n"[..];
    assert_eq!(
        read_frame(&mut reader, &mut quota, Instant::now())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_timeout"
    );
    Ok(())
}

#[test]
fn controls_use_exact_pins_and_reject_secret_shape_symlinks_and_stale_sources() -> Result<()> {
    let fixture = Fixture::new("exact source\n")?;
    let sha = fixture.save()?;
    assert!(control(&fixture.control_path, &sha).is_ok());
    let error = control(&fixture.control_path, &"a".repeat(64))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "parse_agent_control_pin");
    assert!(!error.to_string().contains(fixture.root.to_str().unwrap()));
    let named_auth = fixture.root.join("auth.json");
    fs::write(&named_auth, b"synthetic only")?;
    assert_eq!(
        control(&named_auth, &digest(b"synthetic only"))
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_credential_path"
    );
    #[cfg(unix)]
    {
        let link = fixture.root.join("control-link.json");
        std::os::unix::fs::symlink(&fixture.control_path, &link)?;
        assert!(control(&link, &sha).is_err());
    }
    let mut changed = fs::read(&fixture.config.sources[0].path)?;
    changed[0] ^= 1;
    fs::write(&fixture.config.sources[0].path, changed)?;
    // Length remains equal; constructor must also reject the source hash.
    assert_eq!(
        fixture.session().err().unwrap().to_string(),
        "parse_agent_source_pin"
    );
    Ok(())
}

#[test]
fn native_qualification_cannot_reuse_the_legacy_image_flag_or_create_a_ledger() -> Result<()> {
    let mut fixture = Fixture::new("no paid request\n")?;
    let binary = fixture.root.join("codex");
    fs::write(&binary, b"synthetic; never executed")?;
    for name in ["independent-auth", "state", "results"] {
        fs::create_dir(fixture.root.join(name))?;
    }
    fs::create_dir(fixture.root.join("state/logs"))?;
    let mut profile = OcrRuntimeProfile::unqualified_native(
        "a".repeat(40),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(64),
    )?;
    assert_eq!(
        profile.transport(),
        gptgrep_host::OcrImageTransport::NativeViewer
    );
    assert!(!profile.image_qualification_accepted());
    profile.accepted_image_qualification_sha256 = Some("e".repeat(64));
    assert!(profile.validate().is_err());
    profile.accepted_image_qualification_sha256 = None;
    // Deliberately synthetic and not usable as an original live anchor. The
    // absent native qualification must be rejected before any clock/ledger use.
    let clock: RunClockAnchor =
        serde_json::from_value(json!({"schema":"gptgrep.run-clock-anchor.v1",
        "deadline_unix_ms":999999,"clock_kind":"synthetic","boot_identity":"synthetic",
        "clock_namespace":null,"monotonic_started_ns":1,"monotonic_deadline_ns":2}))?;
    let ledger = fixture.root.join("missing-ledger.json");
    fixture.config.ocr = Some(OcrControl {
        codex_bin: binary,
        auth_home: fixture.root.join("independent-auth"),
        state_dir: fixture.root.join("state"),
        results_root: fixture.root.join("results"),
        runtime_profile: profile,
        run_id: "synthetic".into(),
        plan_sha256: "f".repeat(64),
        original_clock: clock,
        deadline_unix_ms: 999999,
        max_pages: 1,
        phase_limits: None,
        ledger: ledger.clone(),
    });
    assert_eq!(
        validate(&fixture.config).err().unwrap().to_string(),
        "parse_agent_ocr_qualification_required"
    );
    assert!(!ledger.exists());
    Ok(())
}

#[tokio::test]
async fn stdio_forwards_the_facade_frame_without_an_extra_delimiter_or_paths() -> Result<()> {
    let fixture = Fixture::new("stdio source\n")?;
    let session = fixture.session()?;
    let request = json!({"schema":REQUEST_SCHEMA,"id":"quoted\\\"é","max_reply_bytes":4096,
        "operation":{"op":"prepare","source":"approved"}});
    let input = frame(&request, 4096, deadline())?;
    let mut reader = input.as_slice();
    let mut output = Vec::new();
    assert_eq!(serve_io(session, &mut reader, &mut output).await?, 0);
    assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    let reply: Value = serde_json::from_slice(&output)?;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["id"], request["id"]);
    assert!(reply["data"]["original_pages"].is_null());
    assert!(!String::from_utf8(output)?.contains(fixture.root.to_str().unwrap()));
    Ok(())
}

#[tokio::test]
async fn run_projects_every_product_field_and_refuses_overwrite() -> Result<()> {
    let text = "# Heading\n多\\\"\u{0} data\n\n## Second\nlast\n";
    let fixture = Fixture::new(text)?;
    let sha = fixture.save()?;
    let output = fixture.root.join("product.json");
    assert_eq!(
        run(
            &fixture.control_path,
            &sha,
            "approved",
            OcrMode::Off,
            &output
        )
        .await?,
        0
    );
    let bytes = fs::read(&output)?;
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    let mut value: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(value["schema"], PRODUCT_SCHEMA);
    assert_eq!(value["complete_document_traversal"], true);
    for key in [
        "summary",
        "document",
        "pages",
        "artifacts",
        "segments",
        "ocr_results",
        "ocr_tasks",
        "native_profile_sha256",
        "coverage",
        "unmapped_merged_lines",
    ] {
        assert!(
            value["product"].get(key).is_some(),
            "missing first-class field {key}"
        );
    }
    let document: RichParsedDocument = serde_json::from_value(value["product"].take())?;
    assert_eq!(document.document.text, text);
    assert!(!document.document.nodes.is_empty());
    assert!(!document.segments.is_empty());
    assert!(document.pages.is_empty()); // plaintext has logical units, not physical pages
    assert!(
        run(
            &fixture.control_path,
            &sha,
            "approved",
            OcrMode::Off,
            &output
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(&output)?, bytes);
    Ok(())
}

#[tokio::test]
async fn output_reservation_is_exact_retained_and_requires_fresh_source() -> Result<()> {
    let fixture = Fixture::new("retained output\n")?;
    let mut session = fixture.session()?;
    let bytes = session
        .core(
            ToolboxOperation::Prepare {
                source: "approved".into(),
            },
            4096,
        )
        .await?;
    let reply: StageReply = serde_json::from_slice(&bytes)?;
    let handle = reply.data.handle;
    session
        .core(
            ToolboxOperation::ExtractNative {
                handle: handle.clone(),
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
            4096,
        )
        .await?;
    session
        .core(
            ToolboxOperation::Finalize {
                handle: handle.clone(),
                mode: OcrMode::Off,
            },
            4096,
        )
        .await?;
    let before = session.budget.staging_used();
    let output = fixture.root.join("exact.json");
    write_product(&mut session, &handle, OcrMode::Off, &output)?;
    let expected = fs::metadata(&output)?.len();
    assert_eq!(session.budget.staging_used() - before, expected);
    let used = session.budget.staging_used();
    // A racing pre-existing target is never overwritten or refunded.
    assert!(write_product(&mut session, &handle, OcrMode::Off, &output).is_err());
    assert_eq!(session.budget.staging_used() - used, expected);
    assert_eq!(fs::metadata(&output)?.len(), expected);
    let stale_target = fixture.root.join("stale.json");
    fs::write(&fixture.config.sources[0].path, "changed source\n")?;
    let used = session.budget.staging_used();
    assert!(write_product(&mut session, &handle, OcrMode::Off, &stale_target).is_err());
    assert_eq!(session.budget.staging_used(), used);
    assert!(!stale_target.exists());
    Ok(())
}

struct FailingExecutor {
    calls: AtomicU64,
}
impl OcrExecutor for FailingExecutor {
    fn execute<'a>(
        &'a self,
        _task: OcrTask,
    ) -> Pin<Box<dyn Future<Output = Result<OcrPageResult>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(anyhow!("synthetic executor error; no provider call")) })
    }
}

#[tokio::test]
async fn json_paid_authority_is_closed_and_typed_failures_cannot_renew_custody() -> Result<()> {
    let mut fixture = Fixture::new("")?;
    let path = fixture.root.join("generated.png");
    image::RgbaImage::from_fn(32, 32, |x, y| {
        let value = if (x + y) % 2 == 0 { 0 } else { 255 };
        image::Rgba([value, value, value, 255])
    })
    .save(&path)?;
    let bytes = fs::read(&path)?;
    fixture.config.sources[0] = Source {
        alias: "approved".into(),
        path,
        bytes: bytes.len() as u64,
        sha256: digest(&bytes),
    };
    fixture.config.native.render_pages = true;
    let mut session = fixture.session()?;
    let prepared = session
        .core(
            ToolboxOperation::Prepare {
                source: "approved".into(),
            },
            4096,
        )
        .await?;
    let handle = serde_json::from_slice::<StageReply>(&prepared)?.data.handle;
    session
        .core(
            ToolboxOperation::ExtractNative {
                handle: handle.clone(),
                page_window: 25,
                dpi: 96.0,
                render_pages: true,
            },
            4096,
        )
        .await?;
    let plan = session
        .core(
            ToolboxOperation::InspectOcrPlan {
                handle: handle.clone(),
                section: OcrPlanSection::Tasks,
                cursor: None,
                page_size: 1,
            },
            TASK_REPLY,
        )
        .await?;
    let mut plan: TaskReply = serde_json::from_slice(&plan)?;
    assert_eq!(plan.data.rows.len(), 1);
    let task = plan.data.rows.remove(0).task_sha256;
    let forged = json!({"schema":REQUEST_SCHEMA,"id":"safe","max_reply_bytes":4096,
        "operation":{"op":"execute_ocr","handle":handle,"task_sha256":task,"result":{}}});
    let request = frame(&forged, 65_536, deadline())?;
    assert_eq!(
        session
            .operation(&request, deadline())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_request_schema"
    );
    let request = json!({"schema":REQUEST_SCHEMA,"id":"safe","max_reply_bytes":4096,
        "operation":{"op":"execute_ocr","handle":handle,"task_sha256":task}});
    let request = frame(&request, 65_536, deadline())?;
    assert_eq!(
        session
            .operation(&request, deadline())
            .await
            .err()
            .unwrap()
            .to_string(),
        "parse_agent_ocr_configuration_required"
    );
    let executor = FailingExecutor {
        calls: AtomicU64::new(0),
    };
    assert!(
        execute_registered(&mut session.toolbox, &handle, &task, &executor)
            .await
            .is_err()
    );
    assert!(
        execute_registered(&mut session.toolbox, &handle, &task, &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session
        .core(
            ToolboxOperation::Close {
                handle: handle.clone(),
            },
            4096,
        )
        .await?;
    assert!(
        execute_registered(&mut session.toolbox, &handle, &task, &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
