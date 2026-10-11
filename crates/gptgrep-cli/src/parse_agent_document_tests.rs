//! Authored public/synthetic tests. Future execution needs separately admitted
//! external temporary storage; no model/runtime launch is part of these tests.
use super::*;
use serde_json::json;
struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    config: Control,
}
impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        fs::create_dir(root.join("staging"))?;
        let path = root.join("source.txt");
        let text = "# Heading\n多\\\"\u{0} source\n\n## Later\ncomplete text\n";
        fs::write(&path, text)?;
        let config = Control {
            schema: CONTROL_SCHEMA.into(),
            invocation_id: "synthetic".into(),
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
                request_bytes: 65536,
                reply_bytes: 4194304,
                artifact_window_bytes: 32768,
                operation_working_bytes: 33554432,
            },
            frontend: FrontendBounds {
                session_timeout_ms: 60000,
                request_timeout_ms: 30000,
                max_requests: 1000,
                request_bytes: 65536,
                total_wire_bytes: 16777216,
                output_file_bytes: 8388608,
            },
            native: NativeControl {
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
            ocr: None,
        };
        Ok(Self {
            _temporary: temporary,
            root,
            config,
        })
    }
    async fn backend(&self) -> Result<Backend> {
        let limits = driver::DocumentAgentLimits {
            timeout_ms: 900000,
            max_tool_calls: 64,
            max_messages: 100,
            incoming_frame_bytes: 65536,
            outgoing_frame_bytes: 4194304,
            tool_argument_bytes: 4096,
            tool_reply_bytes: 262144,
            total_rpc_bytes: 33554432,
            working_bytes: 67108864,
            tracking_bytes: 8388608,
        };
        let clock = serde_json::from_value(
            json!({"schema":"gptgrep.run-clock-anchor.v1","deadline_unix_ms":999999,
            "clock_kind":"synthetic","boot_identity":"synthetic","clock_namespace":null,"monotonic_started_ns":1,"monotonic_deadline_ns":2}),
        )?;
        let pin = driver::DocumentFilePin {
            path: self.root.join("never-read"),
            bytes: 1,
            sha256: "f".repeat(64),
        };
        let document = DocumentAgentControl {
            schema: "gptgrep.parse-agent.document-control.v1".into(),
            invocation_id: "synthetic".into(),
            codex_bin: pin.clone(),
            runtime_profile: pin.clone(),
            model_descriptor: pin.clone(),
            permissions_config: pin,
            auth_home: self.root.join("never-read-auth"),
            state_dir: self.root.join("never-read-state"),
            cwd: self.root.clone(),
            environment_id: "local".into(),
            auth_mode: "chatgpt".into(),
            requested_model: "gpt-6-luna".into(),
            reasoning_effort: "max".into(),
            requested_service_tier: "fast".into(),
            original_clock: clock,
            deadline_unix_ms: 999999,
            limits,
            native_viewer: None,
            document_image_qualification: None,
        };
        let session = Session::new_with_working(
            self.config.clone(),
            Instant::now(),
            document.limits.working_bytes,
        )?;
        let working = DocumentWorking::new(document.limits.working_bytes);
        Backend::prepare(session, "approved", &document, &working).await
    }
}
#[tokio::test]
async fn complete_parser_product_does_not_require_model_reading_every_metadata_byte() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    assert_eq!(backend.artifacts.len(), backend.prepared.artifact_count);
    assert_eq!(
        backend.metadata.len(),
        backend.prepared.native_metadata_artifacts
    );
    assert!(backend.verify_complete().is_err());
    let summary = backend
        .operation(DocumentToolOperation::InspectDocument {})
        .await?;
    assert!(!summary.text.contains(fixture.root.to_str().unwrap()));
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    let verified = backend.verify_complete()?;
    assert!(verified.parser_complete_original_pages);
    assert!(verified.all_source_artifacts_verified);
    assert_eq!(verified.native_metadata_complete, 0);
    let product = backend
        .session
        .toolbox
        .finalized_document(&backend.prepared.handle)?;
    assert_eq!(
        product.document.text,
        fs::read_to_string(&fixture.config.sources[0].path)?
    );
    assert!(!product.document.nodes.is_empty());
    assert!(!product.segments.is_empty());
    Ok(())
}
#[tokio::test]
async fn freshness_is_rechecked_for_typed_product_and_every_original_artifact() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    assert!(backend.verify_complete().is_ok());
    let root = backend
        .session
        .toolbox
        .prepared_staging_root(&backend.prepared.handle)?
        .to_path_buf();
    let artifact = backend
        .artifacts
        .values()
        .find(|artifact| artifact.kind == ArtifactKind::NativeMetadata)
        .ok_or_else(|| anyhow!("synthetic metadata required"))?
        .clone();
    fs::write(
        root.join(artifact.relative_path),
        b"tampered synthetic artifact",
    )?;
    assert!(backend.verify_complete().is_err());
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    fs::write(&fixture.config.sources[0].path, b"changed synthetic source")?;
    assert!(backend.verify_complete().is_err());
    Ok(())
}
#[tokio::test]
async fn unknown_task_is_fatal_and_unknown_read_selector_grants_no_authority() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    assert!(
        backend
            .operation(DocumentToolOperation::ExecuteOcr {
                task_sha256: "a".repeat(64)
            })
            .await
            .is_err()
    );
    assert!(backend.tasks.is_empty());
    assert!(backend.session.context.is_none());
    let requests = backend.session.quota.requests;
    let refused = backend
        .operation(DocumentToolOperation::ReadArtifact {
            artifact_id: format!("artifact-{}", "b".repeat(64)),
            cursor: None,
            max_bytes: 32,
        })
        .await?;
    let refused: serde_json::Value = serde_json::from_str(&refused.text)?;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"], "parse_toolbox_unknown_artifact");
    assert!(refused.get("data").is_none());
    assert_eq!(backend.session.quota.requests, requests + 1);
    assert!(
        backend
            .operation(DocumentToolOperation::Crop {
                artifact_id: format!("artifact-{}", "b".repeat(64)),
                bbox: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0
                },
            })
            .await
            .is_err()
    );
    assert!(!backend.finalized);
    Ok(())
}

#[tokio::test]
async fn read_selector_refusals_are_charged_without_progress_and_allow_a_later_issued_read()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    let id = backend.metadata.keys().next().unwrap().clone();
    let unknown = format!("artifact-{}", "e".repeat(64));
    for (artifact_id, max_bytes, expected) in [
        (unknown.clone(), 8192, "parse_toolbox_unknown_artifact"),
        (id.clone(), 0, "parse_toolbox_window_limit"),
        (id.clone(), 32769, "parse_toolbox_window_limit"),
    ] {
        let requests = backend.session.quota.requests;
        let wire = backend.session.quota.wire;
        let reply = backend
            .operation(DocumentToolOperation::ReadMetadataWindow {
                artifact_id,
                cursor: None,
                max_bytes,
            })
            .await?;
        assert!(reply.image_url.is_none());
        assert!(!reply.text.contains(&unknown));
        assert!(!reply.text.contains(fixture.root.to_str().unwrap()));
        let value: serde_json::Value = serde_json::from_str(&reply.text)?;
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], expected);
        assert_eq!(value["max_bytes_limit"], 32768);
        assert!(value.get("data").is_none());
        assert_eq!(backend.session.quota.requests, requests + 1);
        assert!(backend.session.quota.wire > wire);
        assert!(
            backend
                .metadata
                .values()
                .all(|state| state.delivered == 0 && !state.eof)
        );
        assert!(backend.inspected_pages.is_empty());
        assert!(backend.pending_crops.is_empty());
        assert!(!backend.finalized);
    }
    let valid = backend
        .operation(DocumentToolOperation::ReadMetadataWindow {
            artifact_id: id.clone(),
            cursor: None,
            max_bytes: 32,
        })
        .await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&valid.text)?["ok"],
        true
    );
    assert!(backend.metadata[&id].delivered > 0);
    let refused = || DocumentToolOperation::ReadArtifact {
        artifact_id: unknown.clone(),
        cursor: None,
        max_bytes: 32,
    };
    let requests = backend.session.quota.requests;
    backend.reply_cap = 1;
    assert!(backend.operation(refused()).await.is_err());
    assert_eq!(backend.session.quota.requests, requests + 1);
    backend.reply_cap = 262_144;
    backend.session.quota.bounds.max_requests = backend.session.quota.requests + 1;
    backend.operation(refused()).await?;
    assert!(backend.operation(refused()).await.is_err());
    Ok(())
}
#[tokio::test]
async fn never_public_cursor_refusal_is_charged_and_a_fresh_read_can_follow() -> Result<()> {
    for metadata in [false, true] {
        let fixture = Fixture::new()?;
        let mut backend = fixture.backend().await?;
        assert!(backend.public_cursors.is_empty());
        let id = backend.metadata.keys().next().unwrap().clone();
        let never_issued = format!("cursor-{}", digest(b"synthetic never-issued token"));
        let operation = |cursor| {
            if metadata {
                DocumentToolOperation::ReadMetadataWindow {
                    artifact_id: id.clone(),
                    cursor,
                    max_bytes: 8,
                }
            } else {
                DocumentToolOperation::ReadArtifact {
                    artifact_id: id.clone(),
                    cursor,
                    max_bytes: 8,
                }
            }
        };
        let requests = backend.session.quota.requests;
        let wire = backend.session.quota.wire;
        let tracking = backend.tracking_used;
        let refused = backend
            .operation(operation(Some(never_issued.clone())))
            .await?;
        assert!(refused.image_url.is_none());
        for private in [
            never_issued.as_str(),
            id.as_str(),
            fixture.root.to_str().unwrap(),
        ] {
            assert!(!refused.text.contains(private));
        }
        let value: serde_json::Value = serde_json::from_str(&refused.text)?;
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "parse_toolbox_unknown_cursor");
        assert!(value.get("data").is_none());
        assert!(value["guidance"].as_str().unwrap().contains(
            "Copy the exact returned next_cursor for the same artifact and read kind, or start with cursor=null"
        ));
        assert_eq!(backend.session.quota.requests, requests + 1);
        assert!(backend.session.quota.wire > wire);
        assert_eq!(backend.tracking_used, tracking);
        assert!(backend.public_cursors.is_empty());
        assert!(
            backend
                .metadata
                .values()
                .all(|state| state.delivered == 0 && !state.eof)
        );
        assert!(backend.inspected_pages.is_empty());
        assert!(backend.pending_crops.is_empty());
        assert!(backend.images.is_empty());
        assert!(!backend.finalized);
        let valid = backend.operation(operation(None)).await?;
        let valid: serde_json::Value = serde_json::from_str(&valid.text)?;
        assert_eq!(valid["ok"], true);
        let token = valid["data"]["next_cursor"].as_str().unwrap();
        assert!(backend.public_cursors.contains(token));
        assert_eq!(backend.session.quota.requests, requests + 2);
        assert!(backend.metadata[&id].delivered > 0);
        assert!(backend.tracking_used > tracking);
    }
    Ok(())
}
#[tokio::test]
async fn public_cursor_binding_replay_and_freshness_errors_remain_fatal() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    let id = backend.metadata.keys().next().unwrap().clone();
    let read = |cursor| DocumentToolOperation::ReadMetadataWindow {
        artifact_id: id.clone(),
        cursor,
        max_bytes: 8,
    };
    let first = backend.operation(read(None)).await?;
    let first: serde_json::Value = serde_json::from_str(&first.text)?;
    let issued = first["data"]["next_cursor"].as_str().unwrap().to_owned();
    assert!(backend.public_cursors.contains(&issued));
    let error = backend
        .operation(DocumentToolOperation::ReadArtifact {
            artifact_id: id.clone(),
            cursor: Some(issued.clone()),
            max_bytes: 8,
        })
        .await
        .err()
        .expect("known wrong-kind cursor is fatal");
    assert!(format!("{error:#}").contains("parse_toolbox_cursor_binding"));
    let other = backend
        .artifacts
        .keys()
        .find(|candidate| *candidate != &id)
        .unwrap()
        .clone();
    assert!(
        backend
            .operation(DocumentToolOperation::ReadArtifact {
                artifact_id: other,
                cursor: Some(issued.clone()),
                max_bytes: 8,
            })
            .await
            .is_err()
    );
    // Known tokens bypass all recoverable-selector checks, including an
    // otherwise recoverable unknown artifact or invalid window bound.
    assert!(
        backend
            .operation(DocumentToolOperation::ReadArtifact {
                artifact_id: format!("artifact-{}", digest(b"unknown synthetic artifact")),
                cursor: Some(issued.clone()),
                max_bytes: 8,
            })
            .await
            .is_err()
    );
    assert!(
        backend
            .operation(DocumentToolOperation::ReadMetadataWindow {
                artifact_id: id.clone(),
                cursor: Some(issued.clone()),
                max_bytes: 0,
            })
            .await
            .is_err()
    );
    backend.operation(read(Some(issued.clone()))).await?;
    assert!(backend.public_cursors.contains(&issued));
    let error = backend
        .operation(read(Some(issued)))
        .await
        .err()
        .expect("replay is fatal");
    assert!(format!("{error:#}").contains("parse_toolbox_unknown_cursor"));

    let enumeration = backend
        .operation(DocumentToolOperation::InspectArtifacts {
            cursor: None,
            page_size: 1,
        })
        .await?;
    let enumeration: serde_json::Value = serde_json::from_str(&enumeration.text)?;
    let enumeration_cursor = enumeration["data"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(backend.public_cursors.contains(&enumeration_cursor));
    let error = backend
        .operation(read(Some(enumeration_cursor)))
        .await
        .err()
        .expect("enumeration cursor in text read is fatal");
    assert!(format!("{error:#}").contains("parse_toolbox_cursor_binding"));
    let fresh = backend.operation(read(None)).await?;
    let fresh: serde_json::Value = serde_json::from_str(&fresh.text)?;
    let fresh = fresh["data"]["next_cursor"].as_str().unwrap().to_owned();
    fs::write(&fixture.config.sources[0].path, b"changed synthetic source")?;
    assert!(backend.operation(read(Some(fresh))).await.is_err());
    Ok(())
}
#[tokio::test]
async fn public_cursor_tracking_exhaustion_fails_closed() -> Result<()> {
    for working_exhausted in [false, true] {
        let fixture = Fixture::new()?;
        let mut backend = fixture.backend().await?;
        let id = backend.metadata.keys().next().unwrap().clone();
        let tracking = backend.tracking_used;
        if working_exhausted {
            // Keep the current charge but leave no space for another token in
            // the actual shared DocumentWorking lease used by Backend::charge.
            let working = DocumentWorking::new(tracking);
            backend._tracking = working.reserve(tracking)?;
        } else {
            backend.tracking_limit = tracking;
        }
        let requests = backend.session.quota.requests;
        let error = backend
            .operation(DocumentToolOperation::ReadMetadataWindow {
                artifact_id: id,
                cursor: None,
                max_bytes: 8,
            })
            .await
            .err()
            .expect("cursor must not be returned without retained accounting");
        assert_eq!(
            error.to_string(),
            if working_exhausted {
                "document_agent_working"
            } else {
                "document_backend_tracking"
            }
        );
        assert_eq!(backend.session.quota.requests, requests + 1);
        assert_eq!(backend.tracking_used, tracking);
        assert!(backend.public_cursors.is_empty());
        assert!(!backend.finalized);
    }
    Ok(())
}
#[tokio::test]
async fn document_working_and_outputs_preserve_one_retained_staging_counter() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.config.resources.clone();
    let mut backend = fixture.backend().await?;
    assert_eq!(
        original.staging_used(),
        backend.session.budget.staging_used()
    );
    let before = original.staging_used();
    backend.reserve_retained(1234)?;
    assert_eq!(original.staging_used() - before, 1234);
    assert_eq!(
        original.staging_used(),
        backend.session.toolbox.staging_used()
    );
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    let output = fixture.root.join("product.json");
    let before = original.staging_used();
    write_product(
        &mut backend.session,
        &backend.prepared.handle,
        OcrMode::Auto,
        &output,
    )?;
    assert_eq!(
        original.staging_used() - before,
        fs::metadata(&output)?.len()
    );
    assert_eq!(fs::read(&output)?.last(), Some(&b'\n'));
    Ok(())
}
#[tokio::test]
async fn delivered_metadata_windows_are_partial_observations_and_not_parser_coverage() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    let artifact = backend
        .metadata
        .keys()
        .next()
        .ok_or_else(|| anyhow!("synthetic metadata required"))?
        .clone();
    backend
        .operation(DocumentToolOperation::ReadMetadataWindow {
            artifact_id: artifact.clone(),
            cursor: None,
            max_bytes: 32,
        })
        .await?;
    assert!(backend.metadata[&artifact].delivered > 0);
    assert!(!backend.metadata[&artifact].eof);
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    let verified = backend.verify_complete()?;
    assert!(verified.all_source_artifacts_verified);
    assert!(verified.required_ocr_complete);
    assert!(!verified.model_inspection_complete);
    Ok(())
}
#[tokio::test]
async fn owned_index_transfer_invalidates_the_handle_without_reparse_or_budget_reset() -> Result<()>
{
    let fixture = Fixture::new()?;
    let original = fixture.config.resources.clone();
    let mut backend = fixture.backend().await?;
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    let handle = backend.prepared.handle.clone();
    let requests = backend.session.quota.requests;
    let expected = backend
        .session
        .toolbox
        .finalized_document(&handle)?
        .document
        .text
        .clone();
    let (mut session, staging, product, budget) = backend.take_index_parts()?;
    assert_eq!(product.document.text, expected);
    assert!(staging.is_dir());
    assert!(session.toolbox.finalized_document(&handle).is_err());
    assert_eq!(session.quota.requests, requests);
    assert_eq!(budget.staging_used(), original.staging_used());
    let before = original.staging_used();
    budget.reserve_retained_staging(123)?;
    assert_eq!(original.staging_used() - before, 123);
    session
        .core(
            ToolboxOperation::Prepare {
                source: "approved".into(),
            },
            4096,
        )
        .await?;
    assert_eq!(session.quota.requests, requests + 1);
    assert_eq!(
        session.toolbox.resource_budget().staging_used(),
        budget.staging_used()
    );
    assert!(original.staging_used() > before + 123);
    Ok(())
}
#[tokio::test]
async fn owned_index_transfer_refuses_changed_source_before_moving_the_product() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    backend
        .operation(DocumentToolOperation::Finalize {})
        .await?;
    fs::write(
        &fixture.config.sources[0].path,
        b"changed synthetic source before take",
    )?;
    assert!(backend.take_index_parts().is_err());
    Ok(())
}

async fn native_image_test_backend() -> Result<(Fixture, Backend, Vec<u8>)> {
    let mut fixture = Fixture::new()?;
    let source = fixture.root.join("generated.png");
    let mut state = 0x81c7_5d39_u32;
    image::RgbaImage::from_fn(512, 256, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgba([state as u8, (state >> 8) as u8, (state >> 16) as u8, 255])
    })
    .save(&source)?;
    let png = fs::read(&source)?;
    fixture.config.sources[0] = Source {
        alias: "approved".into(),
        path: source.clone(),
        bytes: png.len() as u64,
        sha256: digest(&png),
    };
    fixture.config.toolbox.reply_bytes = 262_144;
    let mut session = Session::new(fixture.config.clone(), Instant::now())?;
    let prepared: Envelope<FrozenSummary> = serde_json::from_slice(
        &session
            .core(
                ToolboxOperation::Prepare {
                    source: "approved".into(),
                },
                4096,
            )
            .await?,
    )?;
    let extracted: Envelope<FrozenSummary> = serde_json::from_slice(
        &session
            .core(
                ToolboxOperation::ExtractNative {
                    handle: prepared.data.handle,
                    page_window: 25,
                    dpi: 96.0,
                    render_pages: true,
                },
                4096,
            )
            .await?,
    )?;
    let handle = extracted.data.handle.clone();
    let inventory: Envelope<Rows<ArtifactRef>> = serde_json::from_slice(
        &session
            .core(
                ToolboxOperation::InspectArtifacts {
                    handle: handle.clone(),
                    cursor: None,
                    page_size: 64,
                },
                262_144,
            )
            .await?,
    )?;
    assert!(inventory.data.next_cursor.is_none());
    let artifacts: BTreeMap<_, _> = inventory
        .data
        .rows
        .into_iter()
        .map(|artifact| (artifact.id.clone(), artifact))
        .collect();
    let original_rendition_artifacts = artifacts
        .values()
        .filter(|artifact| artifact.kind == ArtifactKind::Rendition)
        .count();
    assert_eq!(original_rendition_artifacts, 1);
    let working = DocumentWorking::new(67_108_864);
    // Already-extracted synthetic backend for discovery only. This does not
    // qualify OCR, finalize a product, or authorize any native/model execution.
    let backend = Backend {
        session,
        prepared: DocumentPreparedState {
            schema: "gptgrep.parse-agent.prepared-document.v1",
            handle,
            source_sha256: extracted.data.summary.source_sha256,
            prepared_sha256: extracted.data.summary.prepared_sha256,
            task_sha256: "f".repeat(64),
            original_pages: extracted.data.original_pages.unwrap(),
            artifact_count: artifacts.len(),
            native_metadata_artifacts: 0,
            required_ocr_tasks: extracted.data.ocr_task_count.unwrap(),
            original_rendition_artifacts,
        },
        artifacts,
        metadata: BTreeMap::new(),
        tasks: BTreeMap::new(),
        inspected_pages: BTreeSet::new(),
        images: Vec::new(),
        finalized: false,
        direct: true,
        reply_cap: 262_144,
        tracking_limit: 8_388_608,
        tracking_used: 0,
        _tracking: working.reserve(0)?,
        ocr_cleanup: OcrCleanup::new(64)?,
        _ocr_cleanup_working: working.reserve(OcrCleanup::working_bytes(64)?)?,
        crop_working: working,
        pending_crops: BTreeMap::new(),
        public_cursors: BTreeSet::new(),
    };
    Ok((fixture, backend, png))
}

#[tokio::test]
async fn native_image_discovery_projects_only_the_frozen_original_grant() -> Result<()> {
    let (fixture, mut backend, png) = native_image_test_backend().await?;
    let source = fixture.config.sources[0].path.clone();
    let artifact = backend
        .artifacts
        .values()
        .find(|artifact| artifact.kind == ArtifactKind::Rendition)
        .unwrap()
        .clone();
    assert!(artifact.bytes.div_ceil(3) * 4 > 262_144);
    assert!(artifact.bytes < 1_048_576);
    let legacy = backend
        .session
        .core(
            ToolboxOperation::ReadArtifact {
                handle: backend.prepared.handle.clone(),
                artifact_id: artifact.id.clone(),
                cursor: None,
                max_bytes: 1,
            },
            262_144,
        )
        .await
        .unwrap_err();
    assert!(format!("{legacy:#}").contains("parse_toolbox_complete_image_reply_limit"));
    let before = backend.session.budget.staging_used();
    let requests = backend.session.quota.requests;
    let conserved_wire = backend.session.quota.wire;
    backend
        .prepare_images(&driver::DocumentNativeControl {
            image_root: fixture.root.join("native-images"),
            receipt_root: fixture.root.join("native-receipts"),
            admission_path: fixture.root.join("unused-admission.json"),
            image_catalog_path: fixture.root.join("unused-catalog.ndjson"),
            owner_admission_sha256: "a".repeat(64),
            image_wire_bytes: 33_554_432,
        })
        .await?;
    assert_eq!(
        backend.session.budget.staging_used() - before,
        artifact.bytes
    );
    assert_eq!(backend.session.quota.requests, requests + 1);
    assert!(backend.session.quota.wire - conserved_wire > artifact.bytes.div_ceil(3) * 4);
    let copied = fs::read(&backend.images[0].copied_path)?;
    assert_eq!(copied.len() as u64, artifact.bytes);
    assert_eq!(digest(&copied), artifact.sha256);
    assert_eq!(
        copied,
        fs::read(
            backend
                .session
                .toolbox
                .prepared_staging_root(&backend.prepared.handle)?
                .join(&artifact.relative_path)
        )?
    );
    let original_frame = backend
        .session
        .core(
            ToolboxOperation::InspectImages {
                handle: backend.prepared.handle.clone(),
                page: None,
                cursor: None,
                page_size: 1,
            },
            backend.reply_cap,
        )
        .await?;
    let original: serde_json::Value = serde_json::from_slice(&original_frame)?;
    let inspect = || DocumentToolOperation::InspectImages {
        page: None,
        cursor: None,
        page_size: 1,
    };
    let reply = backend.operation(inspect()).await?;
    assert!(reply.image_url.is_none());
    let mut projected: serde_json::Value = serde_json::from_str(&reply.text)?;
    let artifact_id = backend.images[0].artifact.id.clone();
    let native_path = serde_json::to_value(&backend.images[0].copied_path)?;
    for (operation, code) in [
        (
            DocumentToolOperation::ReadMetadataWindow {
                artifact_id: artifact_id.clone(),
                cursor: None,
                max_bytes: 8192,
            },
            "parse_toolbox_metadata_kind",
        ),
        (
            DocumentToolOperation::ReadArtifact {
                artifact_id: artifact_id.clone(),
                cursor: Some("e".repeat(64)),
                max_bytes: 1,
            },
            "parse_toolbox_image_not_a_window",
        ),
    ] {
        let requests = backend.session.quota.requests;
        let refused = backend.operation(operation).await?;
        assert!(refused.image_url.is_none());
        let value: serde_json::Value = serde_json::from_str(&refused.text)?;
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], code);
        assert!(value.get("data").is_none());
        assert!(!refused.text.contains(native_path.as_str().unwrap()));
        assert_eq!(backend.images.len(), 1);
        assert_eq!(backend.session.quota.requests, requests + 1);
    }
    assert_eq!(projected["data"]["rows"][0]["native_path"], native_path);
    assert_eq!(
        projected["data"]["rows"][0]["native_grant"],
        "frozen_source_bound_whole_image"
    );
    assert_eq!(
        digest(&fs::read(&backend.images[0].copied_path)?),
        backend.images[0].artifact.sha256
    );
    assert_eq!(backend.images[0].source_sha256, digest(&png));
    let read = backend
        .operation(DocumentToolOperation::ReadArtifact {
            artifact_id,
            cursor: None,
            max_bytes: 1,
        })
        .await?;
    let read: serde_json::Value = serde_json::from_str(&read.text)?;
    assert!(read["data"].get("data_url").is_none());
    assert_eq!(read["data"]["native_path"], native_path);
    // A cursor already issued publicly must still reach the authoritative Core
    // image/window refusal instead of being discarded by the typed path.
    let issued = backend
        .operation(DocumentToolOperation::InspectArtifacts {
            cursor: None,
            page_size: 1,
        })
        .await?;
    let issued: serde_json::Value = serde_json::from_str(&issued.text)?;
    let cursor = issued["data"]["next_cursor"].as_str().unwrap().to_string();
    let requests = backend.session.quota.requests;
    let error = backend
        .operation(DocumentToolOperation::ReadArtifact {
            artifact_id: artifact.id.clone(),
            cursor: Some(cursor),
            max_bytes: 1,
        })
        .await
        .err()
        .expect("issued image cursor must refuse");
    assert!(format!("{error:#}").contains("parse_toolbox_image_not_a_window"));
    assert_eq!(backend.session.quota.requests, requests + 1);
    assert_eq!(
        read["data"]["native_grant"],
        projected["data"]["rows"][0]["native_grant"]
    );
    let row = projected["data"]["rows"][0].as_object_mut().unwrap();
    row.remove("native_path");
    row.remove("native_grant");
    assert_eq!(projected, original); // Includes unchanged pagination and image metadata.

    backend.direct = false;
    let legacy = backend.operation(inspect()).await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&legacy.text)?,
        original
    );
    assert!(
        backend
            .native_image_rows_reply(original_frame.clone())
            .is_err()
    );
    backend.direct = true;
    backend.images[0].source_sha256 = "e".repeat(64);
    assert!(backend.operation(inspect()).await.is_err());
    backend.images[0].source_sha256 = backend.prepared.source_sha256.clone();
    backend.images[0].width += 1;
    assert!(backend.operation(inspect()).await.is_err());
    backend.images[0].width -= 1;
    let original_sha = backend.images[0].artifact.sha256.clone();
    backend.images[0].artifact.sha256 = "e".repeat(64);
    assert!(backend.operation(inspect()).await.is_err());
    backend.images[0].artifact.sha256 = original_sha;
    backend.reply_cap = original_frame.len();
    assert!(backend.native_image_rows_reply(original_frame).is_err());
    backend.reply_cap = 262_144;
    fs::write(source, b"changed synthetic source")?;
    assert!(backend.operation(inspect()).await.is_err());
    assert!(!backend.finalized);
    Ok(())
}

#[tokio::test]
async fn native_image_handoff_keeps_shared_staging_quota_and_clock_refusals() -> Result<()> {
    for refusal in ["staging", "quota", "clock"] {
        let (fixture, mut backend, _) = native_image_test_backend().await?;
        match refusal {
            "staging" => {
                let budget = &backend.session.budget;
                budget
                    .reserve_retained_staging(budget.run_staging_bytes - budget.staging_used())?;
            }
            "quota" => backend.session.quota.bounds.total_wire_bytes = backend.session.quota.wire,
            "clock" => backend.session.deadline = Instant::now(),
            _ => unreachable!(),
        }
        let before = backend.session.budget.staging_used();
        let images = fixture.root.join("refused-images");
        let error = backend
            .prepare_images(&driver::DocumentNativeControl {
                image_root: images.clone(),
                receipt_root: fixture.root.join("refused-receipts"),
                admission_path: fixture.root.join("never-admitted.json"),
                image_catalog_path: fixture.root.join("never-catalog.ndjson"),
                owner_admission_sha256: "a".repeat(64),
                image_wire_bytes: 33_554_432,
            })
            .await
            .unwrap_err();
        let expected = match refusal {
            "staging" => "document_image_staging",
            "quota" => "parse_agent_total_wire_limit",
            _ => "parse_agent_timeout",
        };
        assert!(format!("{error:#}").contains(expected));
        assert!(backend.images.is_empty());
        assert_eq!(fs::read_dir(&images)?.count(), 0);
        assert_eq!(backend.session.budget.staging_used(), before);
        assert_eq!(backend.session.toolbox.staging_used(), before);
    }
    Ok(())
}

#[tokio::test]
async fn native_image_copy_freshness_failure_keeps_retained_bytes_without_a_grant() -> Result<()> {
    let (fixture, mut backend, _) = native_image_test_backend().await?;
    let artifact = backend
        .artifacts
        .values()
        .find(|artifact| artifact.kind == ArtifactKind::Rendition)
        .unwrap()
        .clone();
    let source = fixture.config.sources[0].path.clone();
    let copied_path = fixture.root.join("retained-failed-copy.png");
    let budget = backend.session.budget.clone();
    let before = budget.staging_used();
    let error = backend
        .session
        .native_rendition(&backend.prepared.handle, &artifact, 1, |_, bytes, _| {
            budget.reserve_retained_staging(bytes.len() as u64)?;
            fs::write(&copied_path, bytes)?;
            fs::write(&source, b"source changed during controller copy")?;
            Ok(())
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("custody_"));
    assert_eq!(budget.staging_used() - before, artifact.bytes);
    assert_eq!(
        backend.session.toolbox.staging_used(),
        budget.staging_used()
    );
    assert_eq!(digest(&fs::read(copied_path)?), artifact.sha256);
    assert!(backend.images.is_empty());
    Ok(())
}

#[tokio::test]
async fn ocr_plan_never_public_cursor_is_charged_content_free_and_null_start_works() -> Result<()> {
    for (section, kind) in [
        ("tasks", "ocr_tasks"),
        ("pages", "ocr_page_coverage"),
        ("blockers", "ocr_blockers"),
    ] {
        let fixture = Fixture::new()?;
        let mut backend = fixture.backend().await?;
        let unknown = digest(b"synthetic never-public plan cursor");
        assert!(!backend.public_cursors.contains(&unknown));
        let requests = backend.session.quota.requests;
        let wire = backend.session.quota.wire;
        let tracking = backend.tracking_used;
        let clock = backend.session.deadline;
        let reply = backend
            .operation(DocumentToolOperation::InspectOcrPlan {
                section: section.into(),
                cursor: Some(unknown.clone()),
                page_size: 1,
            })
            .await?;
        assert!(reply.text.len() <= backend.reply_cap);
        assert!(reply.image_url.is_none());
        assert_eq!(reply.source_frame_sha256, digest(reply.text.as_bytes()));
        let value: serde_json::Value = serde_json::from_str(&reply.text)?;
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "parse_toolbox_unknown_cursor");
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 5);
        for key in ["schema", "id", "ok", "error", "guidance"] {
            assert!(object.contains_key(key));
        }
        assert_eq!(value["schema"], REPLY_SCHEMA);
        assert_eq!(value["id"], "document");
        assert!(value["guidance"].as_str().unwrap().contains("cursor=null"));
        assert!(
            value["guidance"]
                .as_str()
                .unwrap()
                .contains("same operation and section")
        );
        for private in [
            unknown.as_str(),
            backend.prepared.handle.as_str(),
            fixture.root.to_str().unwrap(),
            "complete text",
        ] {
            assert!(!reply.text.contains(private));
        }
        assert_eq!(backend.session.quota.requests, requests + 1);
        assert!(backend.session.quota.wire > wire);
        assert_eq!(backend.session.deadline, clock);
        assert_eq!(backend.tracking_used, tracking);
        assert!(backend.public_cursors.is_empty());
        assert!(backend.inspected_pages.is_empty());
        assert!(backend.pending_crops.is_empty());
        assert!(backend.images.is_empty());
        assert!(backend.session.context.is_none());
        assert!(!backend.finalized);
        assert!(
            backend
                .tasks
                .values()
                .all(|task| { !task.inspected && !task.submitted && !task.complete })
        );
        assert!(
            backend
                .metadata
                .values()
                .all(|state| state.delivered == 0 && !state.eof)
        );
        let valid = backend
            .operation(DocumentToolOperation::InspectOcrPlan {
                section: section.into(),
                cursor: None,
                page_size: 1,
            })
            .await?;
        let valid: serde_json::Value = serde_json::from_str(&valid.text)?;
        assert_eq!(valid["ok"], true);
        assert_eq!(valid["data"]["kind"], kind);
        assert_eq!(valid["data"]["start"], 0);
        assert_eq!(backend.session.quota.requests, requests + 2);
    }
    Ok(())
}

#[tokio::test]
async fn ocr_plan_never_public_refusal_keeps_reply_and_request_caps() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    let operation = || DocumentToolOperation::InspectOcrPlan {
        section: "pages".into(),
        cursor: Some(digest(b"synthetic bounded plan cursor")),
        page_size: 1,
    };
    let requests = backend.session.quota.requests;
    let original_cap = backend.reply_cap;
    backend.reply_cap = 1;
    assert!(backend.operation(operation()).await.is_err());
    assert_eq!(backend.session.quota.requests, requests + 1);
    backend.reply_cap = original_cap;
    backend.session.quota.bounds.max_requests = backend.session.quota.requests + 1;
    backend.operation(operation()).await?;
    assert!(backend.operation(operation()).await.is_err());
    assert!(backend.public_cursors.is_empty());
    assert!(backend.inspected_pages.is_empty());
    assert!(backend.pending_crops.is_empty());
    assert!(backend.session.context.is_none());
    assert!(!backend.finalized);
    Ok(())
}

async fn ocr_plan_pagination_test_backend() -> Result<(Fixture, Backend)> {
    let mut fixture = Fixture::new()?;
    // A complete synthetic two-page PDF gives Core real page-plan pagination.
    // No OCR/model execution, native grant, or accepted product is constructed.
    let first = b"BT /F1 12 Tf 72 720 Td (Synthetic first page for cursor pagination.) Tj ET\n";
    let second = b"BT /F1 12 Tf 72 720 Td (Synthetic second page for cursor pagination.) Tj ET\n";
    let stream = |body: &[u8]| {
        let mut object = format!("<< /Length {} >>\nstream\n", body.len()).into_bytes();
        object.extend_from_slice(body);
        object.extend_from_slice(b"endstream");
        object
    };
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 2 /Kids [3 0 R 5 0 R] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(first),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".to_vec(),
        stream(second),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    let source = fixture.root.join("synthetic-plan-pages.pdf");
    fs::write(&source, &pdf)?;
    fixture.config.sources[0] = Source {
        alias: "approved".into(),
        path: source,
        bytes: pdf.len() as u64,
        sha256: digest(&pdf),
    };
    let mut session = Session::new(fixture.config.clone(), Instant::now())?;
    let prepared: Envelope<FrozenSummary> = serde_json::from_slice(
        &session
            .core(
                ToolboxOperation::Prepare {
                    source: "approved".into(),
                },
                4096,
            )
            .await?,
    )?;
    let extracted: Envelope<FrozenSummary> = serde_json::from_slice(
        &session
            .core(
                ToolboxOperation::ExtractNative {
                    handle: prepared.data.handle,
                    page_window: 25,
                    dpi: 96.0,
                    render_pages: false,
                },
                4096,
            )
            .await?,
    )?;
    assert_eq!(extracted.data.original_pages, Some(2));
    let working = DocumentWorking::new(67_108_864);
    let backend = Backend {
        session,
        prepared: DocumentPreparedState {
            schema: "gptgrep.parse-agent.prepared-document.v1",
            handle: extracted.data.handle,
            source_sha256: extracted.data.summary.source_sha256,
            prepared_sha256: extracted.data.summary.prepared_sha256,
            task_sha256: digest(b"synthetic plan-only backend"),
            original_pages: 2,
            artifact_count: extracted.data.artifact_count,
            native_metadata_artifacts: 0,
            required_ocr_tasks: extracted.data.ocr_task_count.unwrap(),
            original_rendition_artifacts: 0,
        },
        artifacts: BTreeMap::new(),
        metadata: BTreeMap::new(),
        tasks: BTreeMap::new(),
        inspected_pages: BTreeSet::new(),
        public_cursors: BTreeSet::new(),
        images: Vec::new(),
        finalized: false,
        direct: false,
        reply_cap: 262_144,
        tracking_limit: 8_388_608,
        tracking_used: 0,
        _tracking: working.reserve(0)?,
        ocr_cleanup: OcrCleanup::new(64)?,
        _ocr_cleanup_working: working.reserve(OcrCleanup::working_bytes(64)?)?,
        crop_working: working,
        pending_crops: BTreeMap::new(),
    };
    Ok((fixture, backend))
}

#[tokio::test]
async fn ocr_plan_public_pagination_replay_wrong_kind_and_freshness_keep_core_authority()
-> Result<()> {
    let (fixture, mut backend) = ocr_plan_pagination_test_backend().await?;
    let pages = |cursor| DocumentToolOperation::InspectOcrPlan {
        section: "pages".into(),
        cursor,
        page_size: 1,
    };
    let first = backend.operation(pages(None)).await?;
    let first: serde_json::Value = serde_json::from_str(&first.text)?;
    assert_eq!(first["ok"], true);
    assert_eq!(first["data"]["kind"], "ocr_page_coverage");
    assert_eq!(first["data"]["total"], 2);
    assert_eq!(first["data"]["start"], 0);
    assert_eq!(first["data"]["rows"].as_array().unwrap().len(), 1);
    let token = first["data"]["next_cursor"].as_str().unwrap().to_owned();
    assert!(backend.public_cursors.contains(&token));
    for operation in [
        DocumentToolOperation::InspectOcrPlan {
            section: "tasks".into(),
            cursor: Some(token.clone()),
            page_size: 1,
        },
        DocumentToolOperation::InspectArtifacts {
            cursor: Some(token.clone()),
            page_size: 1,
        },
    ] {
        let error = backend
            .operation(operation)
            .await
            .err()
            .expect("public binding violation stays fatal");
        assert!(format!("{error:#}").contains("parse_toolbox_cursor_binding"));
    }
    let second = backend.operation(pages(Some(token.clone()))).await?;
    let second: serde_json::Value = serde_json::from_str(&second.text)?;
    assert_eq!(second["ok"], true);
    assert_eq!(second["data"]["start"], 1);
    assert_eq!(second["data"]["rows"].as_array().unwrap().len(), 1);
    assert!(second["data"]["next_cursor"].is_null());
    assert!(backend.public_cursors.contains(&token));
    let requests = backend.session.quota.requests;
    let replay = backend
        .operation(pages(Some(token.clone())))
        .await
        .err()
        .expect("consumed public cursor stays fatal");
    assert!(format!("{replay:#}").contains("parse_toolbox_unknown_cursor"));
    assert_eq!(backend.session.quota.requests, requests + 1);
    assert!(backend.public_cursors.contains(&token));
    let fresh = backend.operation(pages(None)).await?;
    let fresh: serde_json::Value = serde_json::from_str(&fresh.text)?;
    let fresh = fresh["data"]["next_cursor"].as_str().unwrap().to_owned();
    assert!(backend.public_cursors.contains(&fresh));
    fs::write(
        &fixture.config.sources[0].path,
        b"changed synthetic PDF source",
    )?;
    assert!(backend.operation(pages(Some(fresh))).await.is_err());
    assert!(backend.session.context.is_none());
    assert!(backend.images.is_empty());
    assert!(backend.pending_crops.is_empty());
    assert!(!backend.finalized);
    Ok(())
}

#[tokio::test]
async fn document_backend_cleanup_hook_closes_the_actual_registry() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut backend = fixture.backend().await?;
    assert!(backend.session.ocr_cleanup.is_some());
    backend
        .cancel_and_drain_ocr(Instant::now() + Duration::from_secs(10))
        .await?;
    let error = backend
        .ocr_cleanup
        .cancel_and_drain(Instant::now() + Duration::from_secs(10))
        .await
        .expect_err("same registry was already closed by backend hook");
    assert_eq!(error.to_string(), "ocr_cleanup_already_closed");
    Ok(())
}
