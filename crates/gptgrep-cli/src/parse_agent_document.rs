//! Narrow typed bridge: the document agent can select existing resources, never
//! authorize source paths, budgets, executors, model settings or paid results.
use super::*;
use gptgrep_host::parse_agent_driver::{
    self as driver, DocumentAgentControl, DocumentAgentOutcome, DocumentBackend,
    DocumentImageGrant, DocumentPreparedState, DocumentToolOperation, DocumentToolReply,
    DocumentVerifiedProduct, DocumentWorking, DocumentWorkingLease,
};
use gptgrep_parse::{ArtifactKind, ArtifactRef, CoverageState, NativeOcrCrop, OcrResultStatus};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: T,
}
#[derive(Deserialize)]
struct FrozenSummary {
    handle: String,
    summary: gptgrep_parse::PreparedSummary,
    original_pages: Option<usize>,
    artifact_count: usize,
    ocr_task_count: Option<usize>,
    ocr_blocker_count: Option<usize>,
}
#[derive(Deserialize)]
struct Rows<T> {
    kind: String,
    total: usize,
    start: usize,
    rows: Vec<T>,
    next_cursor: Option<String>,
}
#[derive(Deserialize)]
struct PublicCursor<'a> {
    #[serde(borrow)]
    next_cursor: Option<&'a str>,
}
#[derive(Deserialize)]
struct PageIdentity {
    number: u32,
}
#[derive(Deserialize)]
struct Window {
    kind: String,
    artifact: ArtifactRef,
    offset: usize,
    total_bytes: usize,
    text: String,
    eof: bool,
}
#[derive(Serialize)]
struct NativeImageMetadata<'a> {
    kind: &'static str,
    artifact: &'a ArtifactRef,
    width: u32,
    height: u32,
    page_to_raster: gptgrep_parse::AffineTransform,
    data_url: &'static str,
}

impl Session {
    /// Private controller handoff. Conserved legacy request/data-URL sizes are
    /// logical quota charges, not physical RPC bytes. Only a bounded metadata
    /// frame is serialized; the complete verified image never becomes Base64.
    fn native_rendition<T>(
        &mut self,
        handle: &str,
        artifact: &ArtifactRef,
        max_bytes: usize,
        consume: impl FnOnce(&gptgrep_parse::OcrImage, &[u8], Instant) -> Result<T>,
    ) -> Result<(T, Vec<u8>)> {
        self.quota.request()?;
        self.check()?;
        let deadline = self.request_deadline()?;
        let cap = self.config.toolbox.reply_bytes;
        let request = ToolboxRequest {
            schema: REQUEST_SCHEMA.into(),
            id: "run".into(),
            max_reply_bytes: cap,
            operation: ToolboxOperation::ReadArtifact {
                handle: handle.into(),
                artifact_id: artifact.id.clone(),
                cursor: None,
                max_bytes,
            },
        };
        let request_bytes = framed_count(
            &request,
            self.config
                .frontend
                .request_bytes
                .min(self.config.toolbox.request_bytes) as u64,
            deadline,
        )?;
        self.quota.wire(request_bytes as usize)?;
        ensure!(
            max_bytes > 0 && max_bytes <= self.config.toolbox.artifact_window_bytes,
            "parse_toolbox_window_limit"
        );
        let data_url_bytes = artifact
            .bytes
            .checked_add(2)
            .and_then(|bytes| bytes.checked_div(3))
            .and_then(|bytes| bytes.checked_mul(4))
            .and_then(|bytes| bytes.checked_add(artifact.mime.len() as u64 + 13))
            .ok_or_else(|| anyhow!("document_image_quota"))?;
        self.quota.room(data_url_bytes)?;
        let quota = &mut self.quota;
        let result =
            self.toolbox
                .with_verified_rendition(handle, &artifact.id, |image, bytes| {
                    ensure!(
                        same_artifact(artifact, &image.artifact),
                        "document_complete_image"
                    );
                    let metadata = NativeImageMetadata {
                        kind: "complete_image",
                        artifact: &image.artifact,
                        width: image.width,
                        height: image.height,
                        page_to_raster: image.page_to_raster,
                        data_url: "",
                    };
                    let frame = frame(
                        &Reply {
                            schema: REPLY_SCHEMA,
                            id: "run",
                            ok: true,
                            data: &metadata,
                        },
                        cap,
                        deadline,
                    )?;
                    let conserved_bytes = (frame.len() as u64)
                        .checked_add(data_url_bytes)
                        .and_then(|bytes| usize::try_from(bytes).ok())
                        .ok_or_else(|| anyhow!("document_image_quota"))?;
                    quota.wire(conserved_bytes)?;
                    ensure!(Instant::now() < deadline, "parse_agent_request_timeout");
                    let value = consume(image, bytes, deadline)?;
                    Ok((value, frame))
                })?;
        self.check()?;
        ensure!(Instant::now() < deadline, "parse_agent_request_timeout");
        Ok(result)
    }
}
struct MetadataInspection {
    bytes: u64,
    delivered: u64,
    eof: bool,
}
struct TaskState {
    page: u32,
    inspected: bool,
    submitted: bool,
    complete: bool,
}
struct PendingCrop {
    crop: NativeOcrCrop,
}
struct Backend {
    session: Session,
    prepared: DocumentPreparedState,
    artifacts: BTreeMap<String, ArtifactRef>,
    metadata: BTreeMap<String, MetadataInspection>,
    tasks: BTreeMap<String, TaskState>,
    inspected_pages: BTreeSet<u32>,
    public_cursors: BTreeSet<String>,
    images: Vec<DocumentImageGrant>,
    finalized: bool,
    direct: bool,
    reply_cap: usize,
    tracking_limit: u64,
    tracking_used: u64,
    _tracking: DocumentWorkingLease,
    crop_working: DocumentWorking,
    pending_crops: BTreeMap<String, Vec<PendingCrop>>,
    ocr_cleanup: OcrCleanup,
    _ocr_cleanup_working: DocumentWorkingLease,
}
impl Backend {
    fn charge(&mut self, bytes: u64) -> Result<()> {
        let used = self
            .tracking_used
            .checked_add(bytes)
            .filter(|n| *n <= self.tracking_limit)
            .ok_or_else(|| anyhow!("document_backend_tracking"))?;
        self._tracking.grow_to(used)?;
        self.tracking_used = used;
        Ok(())
    }
    async fn prepare(
        mut session: Session,
        source: &str,
        control: &DocumentAgentControl,
        working: &DocumentWorking,
    ) -> Result<Self> {
        let max_operations = control.limits.max_tool_calls.min(64);
        let cleanup_working = working.reserve(OcrCleanup::working_bytes(max_operations)?)?;
        let ocr_cleanup = OcrCleanup::new(max_operations)?;
        session.ocr_cleanup = Some(ocr_cleanup.clone());
        let frame = session
            .core(
                ToolboxOperation::Prepare {
                    source: source.into(),
                },
                4096,
            )
            .await?;
        let reply: Envelope<FrozenSummary> =
            serde_json::from_slice(&frame).map_err(|_| anyhow!("document_prepare_reply"))?;
        ensure!(
            reply.ok && hash64(&reply.data.handle),
            "document_prepare_reply"
        );
        let handle = reply.data.handle;
        let frame = session
            .core(
                ToolboxOperation::ExtractNative {
                    handle: handle.clone(),
                    page_window: session.config.native.page_window,
                    dpi: session.config.native.dpi,
                    render_pages: session.config.native.render_pages,
                },
                4096,
            )
            .await?;
        let reply: Envelope<FrozenSummary> =
            serde_json::from_slice(&frame).map_err(|_| anyhow!("document_native_reply"))?;
        ensure!(
            reply.ok
                && reply.data.handle == handle
                && reply.data.original_pages.is_some()
                && reply.data.ocr_blocker_count == Some(0)
                && reply.data.ocr_task_count.is_some(),
            "document_native_blocked"
        );
        let summary = reply.data;
        let prepared = DocumentPreparedState {
            schema: "gptgrep.parse-agent.prepared-document.v1",
            handle,
            source_sha256: summary.summary.source_sha256,
            prepared_sha256: summary.summary.prepared_sha256,
            task_sha256: String::new(),
            original_pages: summary.original_pages.expect("checked pages"),
            artifact_count: summary.artifact_count,
            native_metadata_artifacts: 0,
            required_ocr_tasks: summary.ocr_task_count.expect("checked tasks"),
            original_rendition_artifacts: 0,
        };
        let mut backend = Self {
            session,
            prepared,
            artifacts: BTreeMap::new(),
            metadata: BTreeMap::new(),
            tasks: BTreeMap::new(),
            inspected_pages: BTreeSet::new(),
            images: Vec::new(),
            finalized: false,
            direct: control.native_viewer.is_some(),
            reply_cap: control.limits.tool_reply_bytes,
            tracking_limit: control.limits.tracking_bytes,
            tracking_used: 0,
            _tracking: working.reserve(0)?,
            crop_working: working.clone(),
            pending_crops: BTreeMap::new(),
            public_cursors: BTreeSet::new(),
            ocr_cleanup,
            _ocr_cleanup_working: cleanup_working,
        };
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let frame = backend
                .session
                .core(
                    ToolboxOperation::InspectArtifacts {
                        handle: backend.prepared.handle.clone(),
                        cursor,
                        page_size: 16,
                    },
                    backend.reply_cap,
                )
                .await?;
            let reply: Envelope<Rows<ArtifactRef>> = serde_json::from_slice(&frame)
                .map_err(|_| anyhow!("document_artifact_inventory"))?;
            ensure!(
                reply.ok
                    && reply.data.kind == "artifacts"
                    && reply.data.start == seen
                    && reply.data.total == backend.prepared.artifact_count,
                "document_artifact_inventory"
            );
            for artifact in reply.data.rows {
                ensure!(
                    artifact.id.strip_prefix("artifact-").is_some_and(hash64)
                        && !backend.artifacts.contains_key(&artifact.id),
                    "document_artifact_inventory"
                );
                let bytes = serde_json::to_vec(&artifact)
                    .map_err(|_| anyhow!("document_artifact_inventory"))?
                    .len() as u64;
                backend.charge(
                    bytes
                        .checked_mul(4)
                        .and_then(|n| n.checked_add(1024))
                        .ok_or_else(|| anyhow!("document_backend_tracking"))?,
                )?;
                if artifact.kind == ArtifactKind::NativeMetadata {
                    backend.prepared.native_metadata_artifacts += 1;
                    backend.metadata.insert(
                        artifact.id.clone(),
                        MetadataInspection {
                            bytes: artifact.bytes,
                            delivered: 0,
                            eof: false,
                        },
                    );
                }
                if artifact.kind == ArtifactKind::Rendition {
                    backend.prepared.original_rendition_artifacts += 1;
                }
                backend.artifacts.insert(artifact.id.clone(), artifact);
                seen += 1;
            }
            cursor = reply.data.next_cursor;
            if cursor.is_none() {
                ensure!(
                    seen == backend.prepared.artifact_count,
                    "document_artifact_inventory"
                );
                break;
            }
            ensure!(
                seen < backend.prepared.artifact_count,
                "document_artifact_inventory"
            );
        }
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let frame = backend
                .session
                .core(
                    ToolboxOperation::InspectOcrPlan {
                        handle: backend.prepared.handle.clone(),
                        section: OcrPlanSection::Tasks,
                        cursor,
                        page_size: 1,
                    },
                    backend.reply_cap.min(TASK_REPLY),
                )
                .await?;
            let reply: Envelope<Rows<TaskIdentity>> =
                serde_json::from_slice(&frame).map_err(|_| anyhow!("document_task_inventory"))?;
            ensure!(
                reply.ok
                    && reply.data.kind == "ocr_tasks"
                    && reply.data.start == seen
                    && reply.data.total == backend.prepared.required_ocr_tasks
                    && reply.data.rows.len() <= 1,
                "document_task_inventory"
            );
            for task in reply.data.rows {
                ensure!(
                    hash64(&task.task_sha256)
                        && task.page > 0
                        && task.page as usize <= backend.prepared.original_pages
                        && !backend.tasks.contains_key(&task.task_sha256),
                    "document_task_inventory"
                );
                backend.charge(1024)?;
                backend.tasks.insert(
                    task.task_sha256,
                    TaskState {
                        page: task.page,
                        inspected: false,
                        submitted: false,
                        complete: false,
                    },
                );
                seen += 1;
            }
            cursor = reply.data.next_cursor;
            if cursor.is_none() {
                ensure!(
                    seen == backend.prepared.required_ocr_tasks,
                    "document_task_inventory"
                );
                break;
            }
            ensure!(
                seen < backend.prepared.required_ocr_tasks,
                "document_task_inventory"
            );
        }
        if !backend.tasks.is_empty() {
            let ocr = backend
                .session
                .config
                .ocr
                .as_ref()
                .ok_or_else(|| anyhow!("document_native_ocr_configuration_required"))?;
            ensure!(
                ocr.runtime_profile.transport() == gptgrep_host::OcrImageTransport::NativeViewer
                    && ocr.runtime_profile.image_qualification_accepted(),
                "document_native_ocr_qualification_required"
            );
            let snapshot = code(
                backend
                    .session
                    .context
                    .as_ref()
                    .ok_or_else(|| anyhow!("document_ocr_context"))?
                    .snapshot(),
                "document_ocr_budget",
            )?;
            ensure!(
                backend.tasks.len()
                    <= snapshot
                        .max_admissions
                        .saturating_sub(snapshot.charged_admissions),
                "document_ocr_budget"
            );
        }
        backend.prepared.task_sha256 = digest(
            &serde_json::to_vec(&backend.prepared)
                .map_err(|_| anyhow!("document_task_identity"))?,
        );
        if let Some(native) = &control.native_viewer {
            backend.prepare_images(native).await?;
        }
        Ok(backend)
    }
    async fn prepare_images(&mut self, control: &driver::DocumentNativeControl) -> Result<()> {
        for path in [&control.image_root, &control.receipt_root] {
            lexical(path)?;
            directory(
                path.parent()
                    .ok_or_else(|| anyhow!("document_image_parent"))?,
            )?;
            ensure!(
                !path.starts_with(&self.session.config.staging_root)
                    && !self.session.config.staging_root.starts_with(path)
                    && !path.exists()
                    && !path.is_symlink(),
                "document_image_directory_fresh"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .map_err(|_| anyhow!("document_image_directory_create"))?;
            }
            #[cfg(not(unix))]
            return Err(anyhow!("document_unix_required"));
        }
        // Clone only bounded source artifact metadata. All rendition artifacts
        // are copied; no page-number or first-N filter selects the closure.
        let id_bytes = self
            .artifacts
            .values()
            .filter(|artifact| artifact.kind == ArtifactKind::Rendition)
            .try_fold(0u64, |n, artifact| {
                n.checked_add(artifact.id.len() as u64 + 64)
                    .ok_or_else(|| anyhow!("document_backend_tracking"))
            })?;
        self.charge(id_bytes)?;
        let ids = self
            .artifacts
            .values()
            .filter(|artifact| artifact.kind == ArtifactKind::Rendition)
            .map(|artifact| artifact.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.session.check()?;
            let artifact = self
                .artifacts
                .get(&id)
                .ok_or_else(|| anyhow!("document_image_identity"))?
                .clone();
            let suffix = match artifact.mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                _ => return Err(anyhow!("document_image_mime")),
            };
            let path = control
                .image_root
                .join(format!("{}.{}", artifact.id, suffix));
            self.charge(path.as_os_str().len() as u64 * 2 + 1024)?;
            self.images
                .try_reserve_exact(1)
                .map_err(|_| anyhow!("document_backend_tracking"))?;
            let budget = self.session.budget.clone();
            let ((width, height), _) = self.session.native_rendition(
                &self.prepared.handle,
                &artifact,
                1,
                |image, bytes, deadline| {
                    budget
                        .reserve_retained_staging(bytes.len() as u64)
                        .map_err(|_| anyhow!("document_image_staging"))?;
                    #[cfg(unix)]
                    {
                        let mut output = unix::FreshOutput::new(&path)?;
                        let mut writer = ProductWriter {
                            file: &mut output.file,
                            used: 0,
                            cap: artifact.bytes,
                            deadline,
                        };
                        writer
                            .write_all(bytes)
                            .map_err(|_| anyhow!("document_image_copy"))?;
                        ensure!(writer.used == artifact.bytes, "document_image_copy");
                        output
                            .file
                            .sync_all()
                            .map_err(|_| anyhow!("document_image_copy"))?;
                        output.check(artifact.bytes)?;
                        Ok((image.width, image.height))
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = (image, bytes, deadline);
                        Err(anyhow!("document_unix_required"))
                    }
                },
            )?;
            self.images.push(DocumentImageGrant {
                artifact,
                copied_path: path,
                source_sha256: self.prepared.source_sha256.clone(),
                prepared_sha256: self.prepared.prepared_sha256.clone(),
                width,
                height,
            });
        }
        ensure!(
            self.images.len() == self.prepared.original_rendition_artifacts,
            "document_complete_image_catalogue"
        );
        self.session.check()?;
        Ok(())
    }
    async fn operation(&mut self, operation: DocumentToolOperation) -> Result<DocumentToolReply> {
        self.session.check()?;
        let handle = self.prepared.handle.clone();
        if let DocumentToolOperation::InspectDocument {} = operation {
            let bytes = frame(
                &Reply {
                    schema: REPLY_SCHEMA,
                    id: "document",
                    ok: true,
                    data: &self.prepared,
                },
                self.reply_cap,
                self.session.request_deadline()?,
            )?;
            return DocumentToolReply::from_core_frame(bytes, self.reply_cap);
        }
        if let DocumentToolOperation::ExecuteOcr { ref task_sha256 } = operation {
            let task = self
                .tasks
                .get_mut(task_sha256)
                .ok_or_else(|| anyhow!("document_unregistered_task"))?;
            ensure!(!task.submitted, "document_task_consumed");
            task.submitted = true;
            let pending = self.pending_crops.remove(task_sha256).unwrap_or_default();
            let mut crops = Vec::new();
            for PendingCrop { crop } in pending {
                crops.push(crop);
            }
            // These leases survive the awaited owned child, including its
            // cancellation/reaping. Moving bytes does not refund admission.
            let summary = self
                .session
                .ocr_summary_with_native_crops(&handle, task_sha256, crops)
                .await?;
            ensure!(
                summary.metadata_validated
                    && summary.task_sha256 == *task_sha256
                    && matches!(
                        summary.status,
                        OcrResultStatus::Complete | OcrResultStatus::Partial
                    ),
                "document_required_ocr_failed"
            );
            let task = self.tasks.get_mut(task_sha256).expect("registered task");
            task.complete = true;
            ensure!(summary.page == task.page, "document_required_ocr_page");
            let bytes = frame(
                &Reply {
                    schema: REPLY_SCHEMA,
                    id: "document",
                    ok: true,
                    data: &summary,
                },
                self.reply_cap,
                self.session.request_deadline()?,
            )?;
            return DocumentToolReply::from_core_frame(bytes, self.reply_cap);
        }
        if let DocumentToolOperation::Crop {
            ref artifact_id,
            bbox,
        } = operation
            && self.direct
        {
            ensure!(
                self.artifacts
                    .get(artifact_id)
                    .is_some_and(|a| a.kind == ArtifactKind::Rendition)
                    && !self.pending_crops.values().flatten().any(|pending| pending
                        .crop
                        .proof()
                        .recipe
                        .parent_artifact_id
                        == *artifact_id),
                "document_native_crop_parent"
            );
            // Reserve output capacity, proof copies and collection slots
            // before the parser allocates any derived image buffer.
            let retained = self
                .session
                .budget
                .image_file_bytes
                .checked_add(16_384 * 42 * 4 + 4096)
                .ok_or_else(|| anyhow!("document_native_crop_working"))?;
            let lease = self.crop_working.reserve(retained)?;
            let crop = self
                .session
                .native_crop(&handle, artifact_id, bbox)?
                .retain_working_owner(std::sync::Arc::new(lease))?;
            let proof = crop.proof();
            let task = self
                .tasks
                .get(&proof.task_sha256)
                .ok_or_else(|| anyhow!("document_native_crop_task"))?;
            ensure!(
                !task.submitted && task.page == proof.recipe.parent_page,
                "document_task_consumed"
            );
            // Only source-derived transform/lineage metadata reaches the
            // current Doc turn. No crop bytes, path or native grant is added
            // to that already frozen turn.
            let data = serde_json::json!({"kind":"native_crop_pending","proof":proof,
                    "native_view_available_in_current_turn":false,
                    "native_child_static_supplement":true,
                    "original_image_delivery_required":true});
            let bytes = frame(
                &Reply {
                    schema: REPLY_SCHEMA,
                    id: "document",
                    ok: true,
                    data: &data,
                },
                self.reply_cap,
                self.session.request_deadline()?,
            )?;
            let reply = DocumentToolReply::from_core_frame(bytes, self.reply_cap)?;
            let task_sha = proof.task_sha256.clone();
            self.pending_crops
                .entry(task_sha)
                .or_default()
                .push(PendingCrop { crop });
            return Ok(reply);
        }
        if matches!(operation, DocumentToolOperation::Finalize {}) {
            ensure!(
                self.tasks.values().all(|task| task.complete),
                "document_required_ocr_pending"
            );
            ensure!(
                self.pending_crops.is_empty(),
                "document_native_crop_pending"
            );
            let bytes = self
                .session
                .core(
                    ToolboxOperation::Finalize {
                        handle,
                        mode: OcrMode::Auto,
                    },
                    self.reply_cap,
                )
                .await?;
            self.finalized = true;
            self.verify_complete()?;
            return DocumentToolReply::from_core_frame(bytes, self.reply_cap);
        }
        let core = match &operation {
            DocumentToolOperation::InspectPages { cursor, page_size } => {
                ToolboxOperation::InspectPages {
                    handle,
                    cursor: cursor.clone(),
                    page_size: *page_size,
                }
            }
            DocumentToolOperation::InspectArtifacts { cursor, page_size } => {
                ToolboxOperation::InspectArtifacts {
                    handle,
                    cursor: cursor.clone(),
                    page_size: *page_size,
                }
            }
            DocumentToolOperation::ReadMetadataWindow {
                artifact_id,
                cursor,
                max_bytes,
            } => ToolboxOperation::ReadMetadataWindow {
                handle,
                artifact_id: artifact_id.clone(),
                cursor: cursor.clone(),
                max_bytes: *max_bytes,
            },
            DocumentToolOperation::ReadArtifact {
                artifact_id,
                cursor,
                max_bytes,
            } => ToolboxOperation::ReadArtifact {
                handle,
                artifact_id: artifact_id.clone(),
                cursor: cursor.clone(),
                max_bytes: *max_bytes,
            },
            DocumentToolOperation::InspectImages {
                page,
                cursor,
                page_size,
            } => ToolboxOperation::InspectImages {
                handle,
                page: *page,
                cursor: cursor.clone(),
                page_size: *page_size,
            },
            DocumentToolOperation::Crop { artifact_id, bbox } => ToolboxOperation::Crop {
                handle,
                artifact_id: artifact_id.clone(),
                bbox: *bbox,
            },
            DocumentToolOperation::InspectOcrPlan {
                section,
                cursor,
                page_size,
            } => ToolboxOperation::InspectOcrPlan {
                handle,
                cursor: cursor.clone(),
                page_size: *page_size,
                section: match section.as_str() {
                    "tasks" => OcrPlanSection::Tasks,
                    "pages" => OcrPlanSection::Pages,
                    "blockers" => OcrPlanSection::Blockers,
                    _ => return Err(anyhow!("document_plan_section")),
                },
            },
            _ => return Err(anyhow!("document_tool_operation")),
        };
        let complete_image = matches!(operation,DocumentToolOperation::ReadArtifact {ref artifact_id,..}
            if self.artifacts.get(artifact_id).is_some_and(|artifact|artifact.kind==ArtifactKind::Rendition))
            || matches!(operation, DocumentToolOperation::Crop { .. });
        let cap = if complete_image {
            self.session.config.toolbox.reply_bytes
        } else {
            self.reply_cap
        };
        if let Some(error) = self.read_selector_error(&operation) {
            // A rejected pure selector still consumes the original request and
            // wire quotas. No toolbox dispatch, inspection progress or grant is
            // produced; the host's already-charged call may receive this reply.
            self.session.quota.request()?;
            let deadline = self.session.request_deadline()?;
            let request = ToolboxRequest {
                schema: REQUEST_SCHEMA.into(),
                id: "run".into(),
                max_reply_bytes: cap.min(self.session.config.toolbox.reply_bytes),
                operation: core,
            };
            let request_bytes = framed_count(
                &request,
                self.session.config.frontend.request_bytes as u64,
                deadline,
            )?;
            self.session.quota.wire(request_bytes as usize)?;
            let refusal = if matches!(&operation, DocumentToolOperation::InspectOcrPlan { .. }) {
                serde_json::json!({"schema":REPLY_SCHEMA,"id":"document","ok":false,
                    "error":error,
                    "guidance":"Begin inspect_ocr_plan with cursor=null. For pagination, use only the exact returned next_cursor from the same operation and section. This refusal contains no document content or native grant."})
            } else {
                serde_json::json!({"schema":REPLY_SCHEMA,"id":"document","ok":false,
                    "error":error,"max_bytes_limit":self.session.config.toolbox.artifact_window_bytes,
                    "guidance":"Use inspect_artifacts and an issued artifact ID with a compatible read operation. Read windows require max_bytes from 1 through max_bytes_limit. Copy the exact returned next_cursor for the same artifact and read kind, or start with cursor=null. Image reads require cursor=null and max_bytes=1. This refusal contains no document content or native grant."})
            };
            let bytes = driver_frame(
                &refusal,
                self.reply_cap.min(self.session.config.toolbox.reply_bytes),
                deadline,
            )?;
            self.session.check()?;
            self.session.quota.wire(bytes.len())?;
            return Ok(DocumentToolReply {
                source_frame_sha256: digest(&bytes),
                text: String::from_utf8(bytes).map_err(|_| anyhow!("document_selector_reply"))?,
                image_url: None,
            });
        }
        let frame = if self.direct
            && complete_image
            && matches!(
                operation,
                DocumentToolOperation::ReadArtifact { cursor: None, .. }
            ) {
            let DocumentToolOperation::ReadArtifact {
                artifact_id,
                max_bytes,
                ..
            } = &operation
            else {
                return Err(anyhow!("document_native_crop_typed_route_required"));
            };
            let artifact = self
                .artifacts
                .get(artifact_id)
                .ok_or_else(|| anyhow!("document_image_identity"))?;
            self.session
                .native_rendition(
                    &self.prepared.handle,
                    artifact,
                    *max_bytes,
                    |_, _, _| Ok(()),
                )?
                .1
        } else {
            self.session.core(core, cap).await?
        };
        self.observe(&operation, &frame)?;
        if self.direct && matches!(operation, DocumentToolOperation::InspectImages { .. }) {
            let reply = self.native_image_rows_reply(frame)?;
            return self.track_public_cursor(reply);
        }
        if self.direct && complete_image {
            let mut value: serde_json::Value =
                serde_json::from_slice(&frame).map_err(|_| anyhow!("document_image_metadata"))?;
            ensure!(value["ok"] == true, "document_image_metadata");
            let data = value["data"]
                .as_object_mut()
                .ok_or_else(|| anyhow!("document_image_metadata"))?;
            ensure!(data.remove("data_url").is_some(), "document_image_metadata");
            match &operation {
                DocumentToolOperation::ReadArtifact { artifact_id, .. } => {
                    let image = self
                        .images
                        .iter()
                        .find(|image| image.artifact.id == *artifact_id)
                        .ok_or_else(|| anyhow!("document_image_ungranted"))?;
                    data.insert(
                        "native_path".into(),
                        serde_json::to_value(&image.copied_path)?,
                    );
                    data.insert(
                        "native_grant".into(),
                        serde_json::json!("frozen_source_bound_whole_image"),
                    );
                }
                DocumentToolOperation::Crop { .. } => {
                    return Err(anyhow!("document_native_crop_typed_route_required"));
                }
                _ => return Err(anyhow!("document_image_metadata")),
            }
            let bytes = driver_frame(&value, self.reply_cap, self.session.request_deadline()?)?;
            return self.track_public_cursor(DocumentToolReply {
                text: String::from_utf8(bytes).map_err(|_| anyhow!("document_image_metadata"))?,
                image_url: None,
                source_frame_sha256: digest(&frame),
            });
        }
        let reply = DocumentToolReply::from_core_frame(frame, cap)?;
        self.track_public_cursor(reply)
    }
    fn track_public_cursor(&mut self, reply: DocumentToolReply) -> Result<DocumentToolReply> {
        // Borrow only the outward token; private preparation/inventory replies
        // never pass here. Retain every issued token, including consumed ones,
        // so subsequent misuse remains subject to the authoritative core checks.
        let envelope: Envelope<PublicCursor<'_>> = serde_json::from_str(&reply.text)
            .map_err(|_| anyhow!("document_public_cursor_reply"))?;
        ensure!(envelope.ok, "document_public_cursor_reply");
        if let Some(cursor) = envelope.data.next_cursor
            && !self.public_cursors.contains(cursor)
        {
            let bytes = (cursor.len() as u64)
                .checked_mul(4)
                .and_then(|bytes| bytes.checked_add(256))
                .ok_or_else(|| anyhow!("document_backend_tracking"))?;
            self.charge(bytes)?;
            self.public_cursors.insert(cursor.to_owned());
        }
        Ok(reply)
    }
    fn read_selector_error(&self, operation: &DocumentToolOperation) -> Option<&'static str> {
        if let DocumentToolOperation::InspectOcrPlan { cursor, .. } = operation {
            // Only a never-public selector is recoverable. Every public token,
            // including consumed tokens, still reaches authoritative Core checks.
            return cursor
                .as_ref()
                .filter(|cursor| !self.public_cursors.contains(*cursor))
                .map(|_| "parse_toolbox_unknown_cursor");
        }
        let (artifact_id, cursor, max_bytes, metadata) = match operation {
            DocumentToolOperation::ReadArtifact {
                artifact_id,
                cursor,
                max_bytes,
            } => (artifact_id, cursor, *max_bytes, false),
            DocumentToolOperation::ReadMetadataWindow {
                artifact_id,
                cursor,
                max_bytes,
            } => (artifact_id, cursor, *max_bytes, true),
            _ => return None,
        };
        // A previously public token always reaches Core, even when combined
        // with another invalid selector. Replays, binding and custody failures
        // must not be converted into a recoverable application refusal.
        if cursor
            .as_ref()
            .is_some_and(|cursor| self.public_cursors.contains(cursor))
        {
            return None;
        }
        if max_bytes == 0 || max_bytes > self.session.config.toolbox.artifact_window_bytes {
            return Some("parse_toolbox_window_limit");
        }
        let Some(artifact) = self.artifacts.get(artifact_id) else {
            return Some("parse_toolbox_unknown_artifact");
        };
        if metadata && artifact.kind != ArtifactKind::NativeMetadata {
            return Some("parse_toolbox_metadata_kind");
        }
        if artifact.kind == ArtifactKind::Rendition {
            return cursor.as_ref().map(|_| "parse_toolbox_image_not_a_window");
        }
        if !artifact.mime.starts_with("text/") && artifact.mime != "application/json" {
            return Some("parse_toolbox_text_artifact_required");
        }
        if cursor.is_some() {
            return Some("parse_toolbox_unknown_cursor");
        }
        None
    }
    fn native_image_rows_reply(&self, frame: Vec<u8>) -> Result<DocumentToolReply> {
        ensure!(self.direct, "document_native_images_not_admitted");
        let mut value: serde_json::Value =
            serde_json::from_slice(&frame).map_err(|_| anyhow!("document_image_metadata"))?;
        ensure!(
            value["ok"] == true && value["data"]["kind"] == "images",
            "document_image_metadata"
        );
        let rows = value["data"]["rows"]
            .as_array_mut()
            .ok_or_else(|| anyhow!("document_image_metadata"))?;
        for row in rows {
            let artifact: ArtifactRef = serde_json::from_value(row["artifact"].clone())
                .map_err(|_| anyhow!("document_image_metadata"))?;
            let image = self
                .images
                .iter()
                .find(|image| image.artifact.id == artifact.id)
                .ok_or_else(|| anyhow!("document_image_ungranted"))?;
            ensure!(
                artifact.kind == ArtifactKind::Rendition
                    && same_artifact(&artifact, &image.artifact)
                    && self
                        .artifacts
                        .get(&artifact.id)
                        .is_some_and(|original| same_artifact(&artifact, original))
                    && image.source_sha256 == self.prepared.source_sha256
                    && image.prepared_sha256 == self.prepared.prepared_sha256
                    && row["width"].as_u64() == Some(u64::from(image.width))
                    && row["height"].as_u64() == Some(u64::from(image.height)),
                "document_image_binding"
            );
            let row = row
                .as_object_mut()
                .ok_or_else(|| anyhow!("document_image_metadata"))?;
            row.insert(
                "native_path".into(),
                serde_json::to_value(&image.copied_path)?,
            );
            row.insert(
                "native_grant".into(),
                serde_json::json!("frozen_source_bound_whole_image"),
            );
        }
        let bytes = driver_frame(&value, self.reply_cap, self.session.request_deadline()?)?;
        Ok(DocumentToolReply {
            text: String::from_utf8(bytes).map_err(|_| anyhow!("document_image_metadata"))?,
            image_url: None,
            source_frame_sha256: digest(&frame),
        })
    }
    fn observe(&mut self, operation: &DocumentToolOperation, frame: &[u8]) -> Result<()> {
        match operation {
            DocumentToolOperation::InspectPages { .. } => {
                let reply: Envelope<Rows<PageIdentity>> = serde_json::from_slice(frame)
                    .map_err(|_| anyhow!("document_page_inspection"))?;
                ensure!(
                    reply.ok
                        && reply.data.kind == "pages"
                        && reply.data.total == self.prepared.original_pages,
                    "document_page_inspection"
                );
                for page in reply.data.rows {
                    ensure!(
                        page.number > 0 && page.number as usize <= self.prepared.original_pages,
                        "document_page_inspection"
                    );
                    if !self.inspected_pages.contains(&page.number) {
                        self.charge(128)?;
                        self.inspected_pages.insert(page.number);
                    }
                }
            }
            DocumentToolOperation::ReadMetadataWindow { artifact_id, .. }
            | DocumentToolOperation::ReadArtifact { artifact_id, .. }
                if self.metadata.contains_key(artifact_id) =>
            {
                let reply: Envelope<Window> = serde_json::from_slice(frame)
                    .map_err(|_| anyhow!("document_metadata_inspection"))?;
                ensure!(
                    reply.ok
                        && matches!(
                            reply.data.kind.as_str(),
                            "metadata_window" | "artifact_window"
                        )
                        && reply.data.artifact.id == *artifact_id,
                    "document_metadata_inspection"
                );
                let state = self
                    .metadata
                    .get_mut(artifact_id)
                    .expect("metadata selected");
                let end = (reply.data.offset as u64)
                    .checked_add(reply.data.text.len() as u64)
                    .ok_or_else(|| anyhow!("document_metadata_inspection"))?;
                ensure!(
                    reply.data.total_bytes as u64 == state.bytes && end <= state.bytes,
                    "document_metadata_inspection"
                );
                if reply.data.offset as u64 <= state.delivered {
                    state.delivered = state.delivered.max(end);
                }
                state.eof |= state.delivered == state.bytes && reply.data.eof;
            }
            DocumentToolOperation::InspectOcrPlan { section, .. } if section == "tasks" => {
                let reply: Envelope<Rows<TaskIdentity>> = serde_json::from_slice(frame)
                    .map_err(|_| anyhow!("document_task_inspection"))?;
                ensure!(
                    reply.ok
                        && reply.data.kind == "ocr_tasks"
                        && reply.data.total == self.tasks.len(),
                    "document_task_inspection"
                );
                for task in reply.data.rows {
                    let selected = self
                        .tasks
                        .get_mut(&task.task_sha256)
                        .ok_or_else(|| anyhow!("document_task_inspection"))?;
                    ensure!(selected.page == task.page, "document_task_inspection");
                    selected.inspected = true;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
impl DocumentBackend for Backend {
    fn resource_budget(&self) -> gptgrep_parse::ResourceBudget {
        self.session.budget.clone()
    }
    fn prepared(&self) -> &DocumentPreparedState {
        &self.prepared
    }
    fn images(&self) -> &[DocumentImageGrant] {
        &self.images
    }
    fn reserve_retained(&self, bytes: u64) -> Result<()> {
        self.session
            .budget
            .reserve_retained_staging(bytes)
            .map_err(|_| anyhow!("document_shared_staging"))
    }
    fn call<'a>(
        &'a mut self,
        operation: DocumentToolOperation,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentToolReply>> + Send + 'a>> {
        Box::pin(self.operation(operation))
    }
    fn cancel_and_drain_ocr<'a>(
        &'a mut self,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.ocr_cleanup.cancel_and_drain(deadline))
    }
    fn verify_complete(&self) -> Result<DocumentVerifiedProduct> {
        ensure!(
            self.finalized && self.tasks.values().all(|task| task.complete),
            "document_finalized_product_required"
        );
        ensure!(
            self.pending_crops.is_empty(),
            "document_native_crop_pending"
        );
        self.session.check()?;
        let document = code(
            self.session
                .toolbox
                .finalized_document(&self.prepared.handle),
            "document_full_source_freshness",
        )?;
        ensure!(
            document.summary.source_sha256 == self.prepared.source_sha256
                && document.summary.prepared_sha256 == self.prepared.prepared_sha256
                && document.pages.len() == self.prepared.original_pages
                && document
                    .pages
                    .iter()
                    .enumerate()
                    .all(|(index, page)| page.number as usize == index + 1)
                && !document
                    .pages
                    .iter()
                    .any(|page| page.coverage == CoverageState::RequiredOcr)
                && document.ocr_results.len() == self.tasks.len(),
            "document_full_product_incomplete"
        );
        for artifact in self.artifacts.values() {
            ensure!(
                document
                    .artifacts
                    .iter()
                    .any(|current| same_artifact(artifact, current)),
                "document_source_artifact_inventory_changed"
            );
        }
        for result in &document.ocr_results {
            ensure!(
                self.tasks.contains_key(&result.task_sha256)
                    && matches!(
                        result.status,
                        OcrResultStatus::Complete | OcrResultStatus::Partial
                    ),
                "document_required_ocr_failed"
            );
        }
        let metadata_complete = self.metadata.values().filter(|state| state.eof).count();
        let inspected = self.tasks.values().filter(|task| task.inspected).count();
        Ok(DocumentVerifiedProduct {
            handle: self.prepared.handle.clone(),
            source_sha256: self.prepared.source_sha256.clone(),
            prepared_sha256: self.prepared.prepared_sha256.clone(),
            original_pages: self.prepared.original_pages,
            delivered_pages: self.inspected_pages.len(),
            native_metadata_artifacts: self.metadata.len(),
            native_metadata_complete: metadata_complete,
            registered_ocr_tasks: self.tasks.len(),
            inspected_ocr_tasks: inspected,
            required_ocr_complete: true,
            parser_complete_original_pages: true,
            all_source_artifacts_verified: true,
            model_inspection_complete: self.inspected_pages.len() == self.prepared.original_pages
                && metadata_complete == self.metadata.len()
                && inspected == self.tasks.len(),
        })
    }
}
fn same_artifact(a: &ArtifactRef, b: &ArtifactRef) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.relative_path == b.relative_path
        && a.sha256 == b.sha256
        && a.profile_sha256 == b.profile_sha256
        && a.bytes == b.bytes
        && a.mime == b.mime
        && a.page == b.page
        && a.region_id == b.region_id
}
fn driver_frame(value: &impl Serialize, cap: usize, deadline: Instant) -> Result<Vec<u8>> {
    frame(value, cap, deadline)
}

pub(super) async fn agent(
    config_path: &Path,
    expected_sha: &str,
    document_control_path: &Path,
    document_control_sha: &str,
    source_alias: &str,
    output_path: &Path,
) -> Result<PreparedDocumentAgent> {
    unix_output_metadata(output_path)?;
    let mut prepared = prepare_agent(
        config_path,
        expected_sha,
        document_control_path,
        document_control_sha,
        source_alias,
    )
    .await?;
    prepared.require_product_qualification()?;
    write_product(
        &mut prepared.backend.session,
        &prepared.backend.prepared.handle,
        OcrMode::Auto,
        output_path,
    )?;
    prepared.backend.verify_complete()?;
    Ok(prepared)
}
pub struct PreparedDocumentAgent {
    backend: Backend,
    outcome: DocumentAgentOutcome,
    working: DocumentWorking,
    working_bytes: u64,
    original_clock: RunClockAnchor,
    config_pin: driver::DocumentFilePin,
}
impl PreparedDocumentAgent {
    fn require_product_qualification(&self) -> Result<()> {
        ensure!(
            self.outcome.purpose == "product"
                && (!self.outcome.native_viewer_enabled
                    || (self.outcome.native_image_influence_qualified
                        && self.outcome.native_image_qualification.is_some())),
            "document_native_qualification_required"
        );
        Ok(())
    }
    pub fn document(&self) -> Result<&RichParsedDocument> {
        self.backend.verify_complete()?;
        code(
            self.backend
                .session
                .toolbox
                .finalized_document(&self.backend.prepared.handle),
            "document_full_source_freshness",
        )
    }
    /// Private native provenance and assembly evidence; root owns its local sink.
    pub fn outcome(&self) -> &DocumentAgentOutcome {
        &self.outcome
    }
    /// Preserve one loaded control, original clock/ledger, quota and shared
    /// counter when the manager advances to another registered source alias.
    pub fn into_index_parts_and_invocation(self) -> Result<DocumentAgentIndexParts> {
        self.require_product_qualification()?;
        let Self {
            backend,
            outcome,
            working,
            working_bytes,
            original_clock,
            config_pin,
        } = self;
        let (session, staging_root, document, budget) = backend.take_index_parts()?;
        Ok(DocumentAgentIndexParts {
            invocation: DocumentAgentInvocation {
                session,
                working,
                working_bytes,
                original_clock,
                config_pin,
            },
            staging_root,
            document,
            budget,
            outcome,
        })
    }
}
impl Backend {
    fn take_index_parts(
        mut self,
    ) -> Result<(Session, PathBuf, RichParsedDocument, ResourceBudget)> {
        self.verify_complete()?;
        let staging_root = code(
            self.session
                .toolbox
                .prepared_staging_root(&self.prepared.handle),
            "document_full_source_freshness",
        )?
        .to_path_buf();
        let document = code(
            self.session
                .toolbox
                .take_finalized_document(&self.prepared.handle),
            "document_verified_ownership_transfer",
        )?;
        let budget = self.session.toolbox.resource_budget();
        Ok((self.session, staging_root, document, budget))
    }
}
pub struct DocumentAgentIndexParts {
    pub invocation: DocumentAgentInvocation,
    pub staging_root: PathBuf,
    pub document: RichParsedDocument,
    pub budget: ResourceBudget,
    pub outcome: DocumentAgentOutcome,
}
/// Consumed on any failed document operation. No control reload, quota reset,
/// new ResourceBudget or retry is hidden behind sequential document indexing.
pub struct RegisteredDocumentSource<'a> {
    pub alias: &'a str,
    pub path: &'a Path,
    pub bytes: u64,
    pub sha256: &'a str,
}
pub struct DocumentAgentInvocation {
    session: Session,
    working: DocumentWorking,
    working_bytes: u64,
    original_clock: RunClockAnchor,
    config_pin: driver::DocumentFilePin,
}
impl DocumentAgentInvocation {
    pub fn load(
        config_path: &Path,
        expected_sha: &str,
        document: &DocumentAgentControl,
    ) -> Result<Self> {
        let started = Instant::now();
        let config = control(config_path, expected_sha)?;
        let config_pin = driver::DocumentFilePin {
            path: config_path.into(),
            bytes: regular(config_path, CONTROL_BYTES as u64, true)?.len(),
            sha256: expected_sha.into(),
        };
        document.limits.validate()?;
        ensure!(
            document.invocation_id == config.invocation_id,
            "document_invocation_binding"
        );
        if let Some(ocr) = &config.ocr {
            ensure!(
                document.original_clock == ocr.original_clock,
                "document_original_ocr_clock_binding"
            );
        }
        let working_bytes = document.limits.working_bytes;
        let session = Session::new_with_working(config, started, working_bytes)?;
        Ok(Self {
            session,
            working: DocumentWorking::new(working_bytes),
            working_bytes,
            original_clock: document.original_clock.clone(),
            config_pin,
        })
    }
    pub fn registered_sources(
        &self,
    ) -> impl ExactSizeIterator<Item = RegisteredDocumentSource<'_>> + '_ {
        self.session
            .config
            .sources
            .iter()
            .map(|source| RegisteredDocumentSource {
                alias: &source.alias,
                path: &source.path,
                bytes: source.bytes,
                sha256: &source.sha256,
            })
    }
    pub fn resource_budget(&self) -> ResourceBudget {
        self.session.toolbox.resource_budget()
    }
    /// Hash only the exact finite registry and original control with fixed
    /// scratch storage; do not deserialize another control or reset counters.
    fn verify_original_control(&self) -> Result<Instant> {
        self.session.check()?;
        let remaining = code(
            self.original_clock.remaining(),
            "document_original_deadline",
        )?;
        let deadline = self.session.deadline.min(
            Instant::now()
                .checked_add(remaining)
                .ok_or_else(|| anyhow!("document_original_deadline"))?,
        );
        verify_registered_pin(&self.config_pin, CONTROL_BYTES as u64, deadline)?;
        Ok(deadline)
    }
    pub fn verify_registry_fresh(&self) -> Result<()> {
        let deadline = self.verify_original_control()?;
        for source in &self.session.config.sources {
            verify_registered_pin(
                &driver::DocumentFilePin {
                    path: source.path.clone(),
                    bytes: source.bytes,
                    sha256: source.sha256.clone(),
                },
                self.session.config.resources.document_staging_bytes,
                deadline,
            )?;
        }
        self.session.check()?;
        code(
            self.original_clock.remaining(),
            "document_original_deadline",
        )?;
        Ok(())
    }
    pub async fn prepare_source(
        self,
        document: &DocumentAgentControl,
        source_alias: &str,
    ) -> Result<PreparedDocumentAgent> {
        let started = Instant::now();
        self.verify_original_control()?;
        ensure!(
            document.invocation_id == self.session.config.invocation_id
                && document.limits.working_bytes == self.working_bytes
                && document.original_clock == self.original_clock
                && source_alias.len() <= 128
                && self
                    .session
                    .config
                    .sources
                    .iter()
                    .any(|source| source.alias == source_alias),
            "document_invocation_binding"
        );
        if let Some(ocr) = &self.session.config.ocr {
            ensure!(
                document.original_clock == ocr.original_clock,
                "document_original_ocr_clock_binding"
            );
        }
        let mut backend =
            Backend::prepare(self.session, source_alias, document, &self.working).await?;
        let outcome =
            driver::drive_document_agent(document, &mut backend, self.working.clone(), started)
                .await?;
        backend.verify_complete()?;
        Ok(PreparedDocumentAgent {
            backend,
            outcome,
            working: self.working,
            working_bytes: self.working_bytes,
            original_clock: self.original_clock,
            config_pin: self.config_pin,
        })
    }
}
fn verify_registered_pin(pin: &driver::DocumentFilePin, cap: u64, deadline: Instant) -> Result<()> {
    ensure!(
        hash64(&pin.sha256) && pin.bytes <= cap,
        "document_registered_source_pin"
    );
    let before = regular(&pin.path, cap, true)?;
    ensure!(before.len() == pin.bytes, "document_registered_source_pin");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    return Err(anyhow!("parse_agent_unix_custody_required"));
    let mut file = options
        .open(&pin.path)
        .map_err(|_| anyhow!("document_registered_source_read"))?;
    #[cfg(unix)]
    ensure!(
        Stamp::of(&before)
            == Stamp::of(
                &file
                    .metadata()
                    .map_err(|_| anyhow!("document_registered_source_read"))?
            ),
        "document_registered_source_changed"
    );
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut scratch = [0; 65_536];
    loop {
        ensure!(Instant::now() < deadline, "document_original_deadline");
        let n = file
            .read(&mut scratch)
            .map_err(|_| anyhow!("document_registered_source_read"))?;
        if n == 0 {
            break;
        }
        bytes = bytes
            .checked_add(n as u64)
            .filter(|n| *n <= pin.bytes)
            .ok_or_else(|| anyhow!("document_registered_source_pin"))?;
        hash.update(&scratch[..n]);
    }
    ensure!(
        bytes == pin.bytes && format!("{:x}", hash.finalize()) == pin.sha256,
        "document_registered_source_pin"
    );
    let after = regular(&pin.path, cap, true)?;
    #[cfg(unix)]
    ensure!(
        Stamp::of(&before) == Stamp::of(&after)
            && Stamp::of(&after)
                == Stamp::of(
                    &file
                        .metadata()
                        .map_err(|_| anyhow!("document_registered_source_read"))?
                ),
        "document_registered_source_changed"
    );
    Ok(())
}
pub(super) async fn prepare_agent(
    config_path: &Path,
    expected_sha: &str,
    document_control_path: &Path,
    document_control_sha: &str,
    source_alias: &str,
) -> Result<PreparedDocumentAgent> {
    let document = driver::load_document_control(document_control_path, document_control_sha)?;
    DocumentAgentInvocation::load(config_path, expected_sha, &document)?
        .prepare_source(&document, source_alias)
        .await
}

/// Private qualification evidence has no output/index-transfer surface. The
/// purpose request is pinned and source-bound before the native document turn.
pub struct DocumentImageQualificationEvidence {
    prepared: PreparedDocumentAgent,
    purpose_sha256: String,
}
pub struct DocumentImageQualificationFixtureParts {
    pub staging_root: PathBuf,
    pub document: RichParsedDocument,
    pub budget: ResourceBudget,
    pub outcome: DocumentAgentOutcome,
    pub purpose_sha256: String,
}
impl DocumentImageQualificationEvidence {
    pub fn purpose_sha256(&self) -> &str {
        &self.purpose_sha256
    }
    /// Consuming private fixture evidence; not product/index authorization.
    pub fn into_private_fixture_parts(self) -> Result<DocumentImageQualificationFixtureParts> {
        ensure!(
            self.prepared.outcome.purpose == "native_document_image_qualification"
                && !self.prepared.outcome.native_image_influence_qualified
                && self
                    .prepared
                    .outcome
                    .qualification_request_sha256
                    .as_deref()
                    == Some(self.purpose_sha256.as_str()),
            "document_qualification_purpose"
        );
        let PreparedDocumentAgent {
            backend, outcome, ..
        } = self.prepared;
        let (_, staging_root, document, budget) = backend.take_index_parts()?;
        Ok(DocumentImageQualificationFixtureParts {
            staging_root,
            document,
            budget,
            outcome,
            purpose_sha256: self.purpose_sha256,
        })
    }
}
pub(super) async fn qualify_image_source(
    config_path: &Path,
    expected_sha: &str,
    document_control_path: &Path,
    document_control_sha: &str,
    source_alias: &str,
    purpose_path: &Path,
    purpose_sha: &str,
) -> Result<DocumentImageQualificationEvidence> {
    let started = Instant::now();
    let document = driver::load_document_control(document_control_path, document_control_sha)?;
    let request = driver::load_document_image_qualification_request(purpose_path, purpose_sha)?;
    let selected = driver::DocumentFilePin {
        path: document_control_path.into(),
        bytes: regular(document_control_path, 65_536, true)?.len(),
        sha256: document_control_sha.into(),
    };
    ensure!(
        request.source_alias() == source_alias && request.document_control_pin() == &selected,
        "document_qualification_source_binding"
    );
    let invocation = DocumentAgentInvocation::load(config_path, expected_sha, &document)?;
    invocation.verify_original_control()?;
    ensure!(
        invocation
            .session
            .config
            .sources
            .iter()
            .any(|source| source.alias == source_alias),
        "document_invocation_binding"
    );
    let mut backend = Backend::prepare(
        invocation.session,
        source_alias,
        &document,
        &invocation.working,
    )
    .await?;
    let outcome = driver::drive_document_image_qualification(
        &document,
        &mut backend,
        invocation.working.clone(),
        started,
        &request,
    )
    .await?;
    backend.verify_complete()?;
    let prepared = PreparedDocumentAgent {
        backend,
        outcome,
        working: invocation.working,
        working_bytes: invocation.working_bytes,
        original_clock: invocation.original_clock,
        config_pin: invocation.config_pin,
    };
    Ok(DocumentImageQualificationEvidence {
        prepared,
        purpose_sha256: request.sha256().into(),
    })
}

#[cfg(test)]
#[path = "parse_agent_document_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "parse_agent_document_native_crop_tests.rs"]
mod native_crop_tests;
