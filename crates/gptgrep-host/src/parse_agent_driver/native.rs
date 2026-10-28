//! The separate DOCUMENT class of the isolated native guard. Crop custody is
//! deferred to a subsequent PAGE child; this turn's original catalog stays fixed.
use super::*;
use std::collections::BTreeMap;
const FRAME: u64 = 4_194_304;
const RPC: u64 = 33_554_432;
// R10 Document policy: ready + turn + 64 calls, each with up to three
// native-image receipts, at the unchanged 16 KiB per-receipt ceiling.
const RECEIPT_STAGING: u64 = (2 + 64 * (1 + 3)) * 16_384;
#[derive(Clone, Serialize)]
struct ImageRow {
    artifact_id: String,
    relative_path: PathBuf,
    bytes: u64,
    sha256: String,
    width: u32,
    height: u32,
    rendition_sha256: String,
}
pub(super) fn contract_sha256() -> Result<String> {
    contract_sha256_for(900)
}
pub(super) fn duration_millis(seconds: u64) -> Result<u64> {
    ensure!(
        (1..=3600).contains(&seconds),
        "document_duration_out_of_range"
    );
    seconds
        .checked_mul(1000)
        .ok_or_else(|| anyhow!("document_duration_overflow"))
}
pub(super) fn contract_sha256_for(max_seconds: u64) -> Result<String> {
    duration_millis(max_seconds)?;
    Ok(sha(&serde_json::to_vec(
        &json!({"schema_version":"gptgrep.native-document.guard-contract.v1",
        "admission":"gptgrep.native-document.admission.v1","ready":"gptgrep.native-document.ready.v1",
        "receipt":"gptgrep.native-document.receipt.v1","mode":"document","native_tool":"view_image",
        "dynamic_namespace":"gptgrep_parse","dynamic_function":"toolbox","max_tool_calls":64,"max_turns":1,
        "max_seconds":max_seconds,"frame_bytes":FRAME,"rpc_bytes":RPC,"image_limits":"resource_budget_derived_v1",
        "counter":"before_argument_parse_lookup_hooks_handler; failed and duplicate submissions charged",
        "normalized_request_profile_verified_before_send":true,
        "permissions_profile_id":PERMISSIONS,"filesystem":"root deny and private image directory read",
        "image_catalog":"pinned NDJSON outside image and receipt roots; sorted single leaves; 4MiB/8192-byte rows",
        "dynamic_image_outputs_admitted":false,"environment_ids":["local"],"environment_id_argument_admitted":false,
        "receipt_join":["tool_admitted","native_source_buffer","native_prepared_image","native_image_assembled"],
        "provider_ingestion_verified":false,"actual_served_profile_verified":false,
        "native_crop_child_custody":"typed_static_original_task_supplement_v1",
        "native_crop_current_document_grant":false,
        "original_child_images_required_independently_of_crops":true}),
    )?))
}
pub(super) fn permission_value(native: Option<&DocumentNativeControl>) -> Value {
    let mut entries =
        vec![json!({"path":{"type":"special","value":{"kind":"root"}},"access":"deny"})];
    if let Some(native) = native {
        entries.push(json!({"path":{"type":"path","path":native.image_root},"access":"read"}));
    }
    entries.sort_by_key(|value| canonical(value.clone()).to_string());
    canonical(
        json!({"type":"managed","network":"restricted","file_system":{"type":"restricted","entries":entries}}),
    )
}
pub(super) fn canonical(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut pairs = object.into_iter().collect::<Vec<_>>();
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
            let mut result = serde_json::Map::new();
            for (key, value) in pairs {
                result.insert(key, canonical(value));
            }
            Value::Object(result)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        other => other,
    }
}
pub(super) fn validate_control(
    native: &DocumentNativeControl,
    control: &DocumentAgentControl,
) -> Result<()> {
    native.image_limits.validate()?;
    ensure!(
        valid_hash(&native.owner_admission_sha256)
            && native.image_wire_bytes == native.image_limits.image_transport_bytes
            && control.limits.max_tool_calls == 64
            && (1000..=3_600_000).contains(&control.limits.timeout_ms)
            && control.limits.timeout_ms.is_multiple_of(1000)
            && control.limits.incoming_frame_bytes <= FRAME as usize
            && control.limits.outgoing_frame_bytes <= FRAME as usize
            && control.limits.total_rpc_bytes == RPC as usize,
        "document_native_limits"
    );
    for path in [
        &native.image_root,
        &native.receipt_root,
        &native.admission_path,
        &native.image_catalog_path,
    ] {
        lexical(path)?;
        ensure!(!credential(path), "document_credential_path");
        directory(
            path.parent()
                .ok_or_else(|| anyhow!("document_native_parent"))?,
        )?;
        ensure!(
            !path.starts_with(&control.auth_home)
                && !control.auth_home.starts_with(path)
                && !path.starts_with(&control.state_dir)
                && !control.state_dir.starts_with(path),
            "document_native_scope"
        );
    }
    for path in [
        &native.receipt_root,
        &native.admission_path,
        &native.image_catalog_path,
    ] {
        ensure!(
            !path.starts_with(&native.image_root) && !native.image_root.starts_with(path),
            "document_native_scope"
        );
    }
    ensure!(
        !native.image_catalog_path.starts_with(&native.receipt_root)
            && !native.admission_path.starts_with(&native.receipt_root)
            && native.admission_path != native.image_catalog_path,
        "document_native_scope"
    );
    Ok(())
}
pub(super) struct NativeSession {
    control: DocumentNativeControl,
    admission_sha256: String,
    ready: Value,
    catalog: DocumentFilePin,
    initialized: bool,
    paths: BTreeSet<String>,
    calls: BTreeMap<String, String>,
    image_directory: File,
    receipt_directory: File,
    deadline: Instant,
    working: DocumentWorking,
    _retained: DocumentWorkingLease,
    calls_lease: DocumentWorkingLease,
}
impl NativeSession {
    #[cfg(test)]
    pub(super) fn prepare(
        config: &DocumentNativeControl,
        control: &DocumentAgentControl,
        backend: &dyn DocumentBackend,
        working: &DocumentWorking,
        deadline: Instant,
    ) -> Result<Self> {
        Self::prepare_with_duration(config, control, backend, working, deadline, None)
    }
    pub(super) fn prepare_with_duration(
        config: &DocumentNativeControl,
        control: &DocumentAgentControl,
        backend: &dyn DocumentBackend,
        working: &DocumentWorking,
        deadline: Instant,
        max_seconds: Option<u64>,
    ) -> Result<Self> {
        let selected_seconds = max_seconds.unwrap_or(900);
        ensure!(
            duration_millis(selected_seconds)? == control.limits.timeout_ms,
            "document_duration_caller_mismatch"
        );
        let prepared = backend.prepared();
        let resources = backend.resource_budget();
        ensure!(
            config.image_limits
                == document_native_image_limits(&resources, config.image_wire_bytes)?,
            "document_native_original_resource_limits"
        );
        ensure!(
            !backend.images().is_empty()
                && backend.images().len() == prepared.original_rendition_artifacts,
            "document_native_complete_closure"
        );
        let metadata = backend.images().iter().try_fold(8_388_608u64, |n, image| {
            n.checked_add(image.copied_path.as_os_str().len() as u64 * 2 + 2048)
                .ok_or_else(|| anyhow!("document_native_metadata_working"))
        })?;
        let retained = working.reserve(metadata)?;
        backend.reserve_retained(RECEIPT_STAGING)?;
        private_directory(&config.image_root)?;
        private_directory(&config.receipt_root)?;
        ensure!(
            fs::read_dir(&config.receipt_root)
                .map_err(|_| anyhow!("document_native_receipts"))?
                .next()
                .is_none(),
            "document_native_receipts_not_fresh"
        );
        let image_directory = open_directory(&config.image_root)?;
        let receipt_directory = open_directory(&config.receipt_root)?;
        let mut paths = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut rows = Vec::new();
        for image in backend.images() {
            resources.admit_image(image.width, image.height, image.artifact.bytes)?;
            ensure!(
                image.source_sha256 == prepared.source_sha256
                    && image.prepared_sha256 == prepared.prepared_sha256
                    && image.artifact.bytes > 0
                    && image.artifact.bytes <= config.image_limits.image_file_bytes
                    && image
                        .artifact
                        .id
                        .strip_prefix("artifact-")
                        .is_some_and(valid_hash)
                    && ids.insert(image.artifact.id.clone())
                    && image
                        .artifact
                        .page
                        .is_some_and(|page| page > 0 && page as usize <= prepared.original_pages)
                    && image.width > 0
                    && image.height > 0
                    && u64::from(image.width) * u64::from(image.height) <= 8_000_000
                    && image.copied_path.parent() == Some(config.image_root.as_path())
                    && paths.insert(
                        image
                            .copied_path
                            .to_str()
                            .ok_or_else(|| anyhow!("document_native_path"))?
                            .into()
                    ),
                "document_native_source_grant"
            );
            read_pin_stream(
                &DocumentFilePin {
                    path: image.copied_path.clone(),
                    bytes: image.artifact.bytes,
                    sha256: image.artifact.sha256.clone(),
                },
                deadline,
            )?;
            rows.push(ImageRow {
                artifact_id: image.artifact.id.clone(),
                relative_path: PathBuf::from(
                    image
                        .copied_path
                        .file_name()
                        .ok_or_else(|| anyhow!("document_native_leaf"))?,
                ),
                bytes: image.artifact.bytes,
                sha256: image.artifact.sha256.clone(),
                width: image.width,
                height: image.height,
                rendition_sha256: image.artifact.profile_sha256.clone(),
            });
        }
        verify_image_namespace(
            &config.image_root,
            &image_directory,
            &paths,
            config.image_limits.image_file_bytes,
        )?;
        rows.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let mut catalog_bytes = Vec::new();
        for row in &rows {
            let bytes = crate::transport::encode_frame(&serde_json::to_value(row)?, 8192)?;
            ensure!(
                catalog_bytes
                    .len()
                    .checked_add(bytes.len())
                    .is_some_and(|n| n <= 4_194_304),
                "document_native_catalog_bytes"
            );
            catalog_bytes
                .try_reserve_exact(bytes.len())
                .map_err(|_| anyhow!("document_native_catalog_allocation"))?;
            catalog_bytes.extend_from_slice(&bytes);
        }
        backend.reserve_retained(catalog_bytes.len() as u64)?;
        write_fresh(&config.image_catalog_path, &catalog_bytes)?;
        let catalog = DocumentFilePin {
            path: config.image_catalog_path.clone(),
            bytes: catalog_bytes.len() as u64,
            sha256: sha(&catalog_bytes),
        };
        let permission_sha256 = sha(&serde_json::to_vec(&permission_value(Some(config)))?);
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("document_native_clock"))?
            .as_millis();
        let fixed_deadline = u64::try_from(
            now.checked_add(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .ok_or_else(|| anyhow!("document_native_clock"))?,
        )
        .map_err(|_| anyhow!("document_native_clock"))?
        .min(control.deadline_unix_ms);
        let mut manifest = json!({"schema_version":"gptgrep.native-document.admission.v1",
            "task_sha256":prepared.task_sha256,"source_sha256":prepared.source_sha256,"prepared_sha256":prepared.prepared_sha256,
            "runtime_profile_sha256":control.runtime_profile.sha256,"deadline_unix_ms":fixed_deadline,
            "max_tool_calls":64,"max_turns":1,"frame_bytes":FRAME,
            "image_transport_reserved_bytes":config.image_wire_bytes,"image_limits":config.image_limits,
            "image_root":config.image_root,"receipt_root":config.receipt_root,
            "permissions_profile_sha256":permission_sha256,"images":[],
            "image_catalog":{"path":catalog.path,"bytes":catalog.bytes,"sha256":catalog.sha256,"count":rows.len()}});
        if let Some(seconds) = max_seconds {
            manifest["max_seconds"] = json!(seconds);
        }
        bounded_json(&manifest, 65_536)?;
        let bytes = serde_json::to_vec(&manifest)?;
        backend.reserve_retained(bytes.len() as u64)?;
        write_fresh(&config.admission_path, &bytes)?;
        let admission_sha256 = sha(&bytes);
        let ready = json!({"schema_version":"gptgrep.native-document.ready.v1","required":true,
            "admission_sha256":admission_sha256,"task_sha256":prepared.task_sha256,"source_sha256":prepared.source_sha256,
            "prepared_sha256":prepared.prepared_sha256,"runtime_profile_sha256":control.runtime_profile.sha256,
            "permissions_profile_id":PERMISSIONS,"permissions_profile_sha256":permission_sha256,
            "environment_ids":["local"],"environment_id_argument_admitted":false,
            "max_tool_calls":64,"max_turns":1,"max_seconds":selected_seconds,"deadline_unix_ms":fixed_deadline,
            "frame_bytes":FRAME,"image_transport_reserved_bytes":config.image_wire_bytes,"image_limits":config.image_limits,
            "tool_counter_before_lookup_hooks_handler":true,"tool_counter_before_argument_parse":true,
            "normalized_request_profile_verified_before_send":true,"native_image_read_bounded":true,
            "assembled_image_receipts_required":true,"requested_model":DOCUMENT_MODEL,"requested_effort":"max",
            "normalized_request_service_tier":"priority","actual_served_profile_verified":false,
            "image_catalog_sha256":catalog.sha256,"image_catalog_count":rows.len()});
        Ok(Self {
            control: config.clone(),
            admission_sha256,
            ready,
            catalog,
            initialized: false,
            paths,
            calls: BTreeMap::new(),
            image_directory,
            receipt_directory,
            deadline,
            working: working.clone(),
            _retained: retained,
            calls_lease: working.reserve(0)?,
        })
    }
    pub(super) fn configure_command(&self, command: &mut Command) {
        command
            .env("GPTGREP_NATIVE_DOCUMENT_REQUIRED", "1")
            .env(
                "GPTGREP_NATIVE_DOCUMENT_ADMISSION",
                &self.control.admission_path,
            )
            .env(
                "GPTGREP_NATIVE_DOCUMENT_ADMISSION_SHA256",
                &self.admission_sha256,
            );
    }
    pub(super) fn initialize(&mut self, value: &Value) -> Result<()> {
        ensure!(
            !self.initialized && value.get("gptgrepNativeDocument") == Some(&self.ready),
            "document_native_required_ready"
        );
        self.initialized = true;
        Ok(())
    }
    pub(super) fn prompt_images(&self) -> Value {
        json!({"catalogue_complete":true,"image_root":self.control.image_root,
        "image_catalog_sha256":self.catalog.sha256,"image_catalog_count":self.paths.len(),
        "discovery":"inspect_images/read_artifact return registered native paths; no arbitrary filesystem path or dynamic image",
        "crop":"crop returns a source-derived pending proof for its original OCR task; execute_ocr freezes that optional supplement into its subsequent PAGE child. It creates no path or new native grant in this DOCUMENT turn; the child must still view every original image and return the original region coordinates."})
    }
    pub(super) fn observe(&mut self, item: &Value, max: usize) -> Result<()> {
        ensure!(self.initialized, "document_native_required_ready");
        let id = identifier(&item["id"])?;
        let path = item["path"]
            .as_str()
            .filter(|path| self.paths.contains(*path))
            .ok_or_else(|| anyhow!("document_native_ungranted_path"))?;
        if let Some(previous) = self.calls.get(&id) {
            ensure!(previous == path, "document_native_path_changed");
        } else {
            ensure!(self.calls.len() < max, "document_native_tool_count");
            self.calls_lease
                .grow_to((self.calls.len() as u64 + 1) * 8192)?;
            self.calls.insert(id, path.into());
        }
        Ok(())
    }
    pub(super) fn finish(
        &self,
        outcome: &DocumentAgentOutcome,
        backend: &dyn DocumentBackend,
    ) -> Result<usize> {
        ensure!(self.initialized, "document_native_required_ready");
        let _receipts = self.working.reserve(16_384 * 42 * 4)?;
        read_pin(&self.catalog, 4_194_304)?;
        verify_image_namespace(
            &self.control.image_root,
            &self.image_directory,
            &self.paths,
            self.control.image_limits.image_file_bytes,
        )?;
        ensure!(
            stable_directory(
                &open_directory(&self.control.receipt_root)?,
                &self.receipt_directory
            ),
            "document_native_receipt_directory_changed"
        );
        let count = outcome
            .tool_submissions
            .checked_add(self.calls.len())
            .filter(|n| *n <= 64)
            .ok_or_else(|| anyhow!("document_native_tool_count"))?;
        ensure!(
            read_receipt(&self.control.receipt_root, "ready.json")? == self.ready,
            "document_native_ready_receipt"
        );
        ensure!(
            read_receipt(&self.control.receipt_root, "turn.json")?
                == json!({"schema_version":"gptgrep.native-document.receipt.v1",
            "kind":"turn_admitted","admission_sha256":self.admission_sha256,"task_sha256":backend.prepared().task_sha256,
            "turn_id":outcome.turn_id,"max_turns":1,"permissions_profile_sha256":self.ready["permissions_profile_sha256"]}),
            "document_native_turn_receipt"
        );
        let mut expected = BTreeSet::from(["ready.json".to_owned(), "turn.json".to_owned()]);
        let mut seen = BTreeSet::new();
        let mut cumulative = 0u64;
        for ordinal in 1..=count {
            let leaf = format!("call-{ordinal}.json");
            expected.insert(leaf.clone());
            let call = read_receipt(&self.control.receipt_root, &leaf)?;
            let id = identifier(&call["call_id"])?;
            ensure!(seen.insert(id.clone()), "document_native_duplicate_receipt");
            let native = self.calls.contains_key(&id);
            let mediated = outcome
                .deliveries
                .iter()
                .filter(|delivery| delivery.call_id_sha256 == sha(id.as_bytes()))
                .count();
            ensure!(
                (native && mediated == 0) || (!native && mediated == 1),
                "document_native_unobserved_call"
            );
            ensure!(
                call == json!({"schema_version":"gptgrep.native-document.receipt.v1","kind":"tool_admitted",
                "admission_sha256":self.admission_sha256,"task_sha256":backend.prepared().task_sha256,"turn_id":outcome.turn_id,
                "call_id":id,"ordinal":ordinal,"native_image":native,"handler_not_yet_executed":true,
                "argument_parser_not_yet_executed":true}),
                "document_native_call_receipt"
            );
            if !native {
                continue;
            }
            let path = self.calls.get(&id).expect("known native call");
            let grant = backend
                .images()
                .iter()
                .find(|image| image.copied_path == Path::new(path))
                .ok_or_else(|| anyhow!("document_native_grant"))?;
            let source_leaf = format!("source-{ordinal}.json");
            let prepared_leaf = format!("prepared-{ordinal}.json");
            let assembled_leaf = format!("assembled-{ordinal}.json");
            expected.extend([
                source_leaf.clone(),
                prepared_leaf.clone(),
                assembled_leaf.clone(),
            ]);
            ensure!(
                read_receipt(&self.control.receipt_root, &source_leaf)?
                    == json!({"schema_version":"gptgrep.native-document.receipt.v1",
                "kind":"native_source_buffer","admission_sha256":self.admission_sha256,"task_sha256":backend.prepared().task_sha256,
                "call_id":id,"ordinal":ordinal,"artifact_id":grant.artifact.id,"source_buffer_bytes":grant.artifact.bytes,
                "source_buffer_sha256":grant.artifact.sha256,"source_width":grant.width,"source_height":grant.height,
                "rendition_sha256":grant.artifact.profile_sha256}),
                "document_native_source_receipt"
            );
            let prepared = read_receipt(&self.control.receipt_root, &prepared_leaf)?;
            let payload = number(&prepared, "prepared_image_bytes")?;
            let width = number(&prepared, "prepared_width")?;
            let height = number(&prepared, "prepared_height")?;
            let pixels = width
                .checked_mul(height)
                .ok_or_else(|| anyhow!("document_native_pixels"))?;
            let wire = number(&prepared, "image_wire_bytes")?;
            cumulative = cumulative
                .checked_add(wire)
                .ok_or_else(|| anyhow!("document_native_image_wire"))?;
            let image_hash = prepared["prepared_image_sha256"]
                .as_str()
                .filter(|hash| valid_hash(hash))
                .ok_or_else(|| anyhow!("document_native_prepared_hash"))?;
            ensure!(
                payload > 0
                    && payload <= self.control.image_limits.image_file_bytes
                    && pixels > 0
                    && pixels <= 16 * 1024 * 1024
                    && wire > payload
                    && wire <= self.control.image_limits.image_wire_bytes
                    && cumulative <= self.control.image_wire_bytes,
                "document_native_prepared_bounds"
            );
            ensure!(
                prepared
                    == json!({"schema_version":"gptgrep.native-document.receipt.v1","kind":"native_prepared_image",
                "admission_sha256":self.admission_sha256,"task_sha256":backend.prepared().task_sha256,"call_id":id,"ordinal":ordinal,
                "artifact_id":grant.artifact.id,"source_buffer_sha256":grant.artifact.sha256,"prepared_image_bytes":payload,
                "prepared_image_sha256":image_hash,"source_width":grant.width,"source_height":grant.height,
                "prepared_width":width,"prepared_height":height,"image_wire_bytes":wire,"cumulative_image_wire_bytes":cumulative,
                "provider_ingestion_verified":false}),
                "document_native_prepared_receipt"
            );
            let assembled = read_receipt(&self.control.receipt_root, &assembled_leaf)?;
            let reference: gptgrep_parse::OcrNativeImageReference =
                serde_json::from_value(assembled["reference"].clone())
                    .map_err(|_| anyhow!("document_native_assembled_reference"))?;
            match &reference {
                gptgrep_parse::OcrNativeImageReference::Inline { bytes, sha256 } => ensure!(
                    *bytes == payload && sha256 == image_hash,
                    "document_native_inline_reference"
                ),
                gptgrep_parse::OcrNativeImageReference::File { reference_sha256 } => ensure!(
                    valid_hash(reference_sha256),
                    "document_native_file_reference"
                ),
            }
            ensure!(
                assembled
                    == json!({"schema_version":"gptgrep.native-document.receipt.v1","kind":"native_image_assembled",
                "admission_sha256":self.admission_sha256,"task_sha256":backend.prepared().task_sha256,"call_id":id,"ordinal":ordinal,
                "artifact_id":grant.artifact.id,"source_buffer_sha256":grant.artifact.sha256,"reference":reference,
                "prepared_image_sha256":image_hash,"prepared_image_bytes":payload,"prepared_width":width,"prepared_height":height,
                "image_content_wire_charge":wire,"local_assembly_verified":true,
                "provider_ingestion_verified":false}),
                "document_native_assembled_receipt"
            );
        }
        let mut actual = BTreeSet::new();
        for entry in fs::read_dir(&self.control.receipt_root)
            .map_err(|_| anyhow!("document_native_receipts"))?
        {
            let entry = entry.map_err(|_| anyhow!("document_native_receipts"))?;
            ensure!(
                actual.len() < expected.len(),
                "document_native_extra_receipt"
            );
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("document_native_receipt_name"))?;
            ensure!(
                expected.contains(&name) && actual.insert(name),
                "document_native_extra_receipt"
            );
        }
        ensure!(actual == expected, "document_native_missing_receipt");
        for grant in backend.images() {
            read_pin_stream(
                &DocumentFilePin {
                    path: grant.copied_path.clone(),
                    bytes: grant.artifact.bytes,
                    sha256: grant.artifact.sha256.clone(),
                },
                self.deadline,
            )?;
        }
        Ok(self.calls.len())
    }
    pub(super) fn observed_artifacts(
        &self,
        backend: &dyn DocumentBackend,
    ) -> Result<BTreeSet<String>> {
        self.calls
            .values()
            .map(|path| {
                backend
                    .images()
                    .iter()
                    .find(|grant| grant.copied_path == Path::new(path))
                    .map(|grant| grant.artifact.id.clone())
                    .ok_or_else(|| anyhow!("document_native_grant"))
            })
            .collect()
    }
}
fn number(value: &Value, key: &str) -> Result<u64> {
    value[key]
        .as_u64()
        .ok_or_else(|| anyhow!("document_native_receipt_number"))
}
fn read_receipt(root: &Path, leaf: &str) -> Result<Value> {
    ensure!(
        !leaf.contains('/') && leaf.len() <= 64,
        "document_native_receipt_name"
    );
    let path = root.join(leaf);
    let metadata = regular(&path, 16_384)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1 && metadata.mode() & 0o077 == 0,
            "document_native_receipt_mode"
        );
    }
    let mut file = open_pin(
        &DocumentFilePin {
            path: path.clone(),
            bytes: metadata.len(),
            sha256: "0".repeat(64),
        },
        16_384,
    )?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(metadata.len() as usize)
        .map_err(|_| anyhow!("document_native_receipt_allocation"))?;
    let mut scratch = [0; 4096];
    loop {
        let n = file
            .read(&mut scratch)
            .map_err(|_| anyhow!("document_native_receipt_read"))?;
        if n == 0 {
            break;
        }
        ensure!(
            n <= metadata.len() as usize - bytes.len(),
            "document_native_receipt_limit"
        );
        bytes.extend_from_slice(&scratch[..n]);
    }
    ensure!(
        bytes.len() as u64 == metadata.len()
            && stable(&metadata, &file.metadata()?)
            && stable(&metadata, &regular(&path, 16_384)?),
        "document_native_receipt_changed"
    );
    gptgrep_jev::parse_unique_json(&bytes).map_err(|_| anyhow!("document_native_receipt_json"))
}
fn verify_image_namespace(
    root: &Path,
    held: &File,
    paths: &BTreeSet<String>,
    image_file_bytes: u64,
) -> Result<()> {
    ensure!(
        stable_directory(&open_directory(root)?, held),
        "document_native_image_directory_changed"
    );
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(root).map_err(|_| anyhow!("document_native_images"))? {
        let path = entry.map_err(|_| anyhow!("document_native_images"))?.path();
        ensure!(
            actual.len() < paths.len() && path.to_str().is_some_and(|path| paths.contains(path)),
            "document_native_extra_image"
        );
        let metadata = regular(&path, image_file_bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.nlink() == 1 && metadata.mode() & 0o077 == 0,
                "document_native_image_mode"
            );
        }
        actual.insert(
            path.to_str()
                .ok_or_else(|| anyhow!("document_native_path"))?
                .to_owned(),
        );
    }
    ensure!(&actual == paths, "document_native_missing_image");
    Ok(())
}
fn stable_directory(a: &File, b: &File) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (a.metadata(), b.metadata()) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}
fn open_directory(path: &Path) -> Result<File> {
    directory(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
        .open(path)
        .map_err(|_| anyhow!("document_native_directory"))
}
fn private_directory(path: &Path) -> Result<()> {
    directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            fs::metadata(path)?.mode() & 0o077 == 0,
            "document_native_directory_mode"
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(anyhow!("document_unix_required"))
    }
}
fn write_fresh(path: &Path, bytes: &[u8]) -> Result<()> {
    lexical(path)?;
    ensure!(!credential(path), "document_credential_path");
    directory(
        path.parent()
            .ok_or_else(|| anyhow!("document_native_parent"))?,
    )?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .map_err(|_| anyhow!("document_native_manifest_create"))?;
    file.write_all(bytes)
        .map_err(|_| anyhow!("document_native_manifest_write"))?;
    file.sync_all()
        .map_err(|_| anyhow!("document_native_manifest_sync"))?;
    Ok(())
}
