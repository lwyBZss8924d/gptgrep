//! Authored synthetic source tests only. No process/model is launched here.
//! A future run must use separately admitted external temporary storage.
use super::*;
use gptgrep_host::parse_agent_driver::{
    self as driver, DocumentAgentLimits, DocumentNativeControl,
};
use gptgrep_jev::RunClockAnchor;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};

struct Fixture {
    _temporary: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    config: DocumentFilePin,
    controls: Vec<DocumentAgentControl>,
    pins: Vec<DocumentFilePin>,
    registry: DocumentFilePin,
}
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut rows = map.into_iter().collect::<Vec<_>>();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                rows.into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        other => other,
    }
}
fn write_value(path: &Path, value: &impl Serialize) -> Result<DocumentFilePin> {
    let bytes = serde_json::to_vec(value)?;
    fs::write(path, &bytes)?;
    Ok(DocumentFilePin {
        path: path.into(),
        bytes: bytes.len() as u64,
        sha256: hash(&bytes),
    })
}
impl Fixture {
    fn new(count: usize) -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let base = temporary.path().canonicalize()?;
        let root = base.join("corpus");
        fs::create_dir(&root)?;
        fs::create_dir(base.join("staging"))?;
        fs::create_dir(base.join("account"))?;
        fs::create_dir(base.join("state"))?;
        fs::create_dir(base.join("state/logs"))?;
        let mut sources = Vec::new();
        for index in 0..count {
            let text =
                format!("# Source{index}\n多 quoted\\\" text\n\n## Complete\nall source content\n");
            let path = root.join(format!("source-{index}.txt"));
            fs::write(&path, &text)?;
            sources.push(json!({"alias":format!("source-{index}"),"path":path,"bytes":text.len(),"sha256":hash(text.as_bytes())}));
        }
        fs::write(root.join("unsupported.bin"), b"ordinary unsupported bytes")?;
        let config = write_value(
            &base.join("control.json"),
            &json!({
            "schema":"gptgrep.parse-agent.control.v1","invocation_id":"synthetic-index","nonce":"17".repeat(32),
            "staging_root":base.join("staging"),"sources":sources,"resources":gptgrep_parse::ResourceBudget::default(),
            "toolbox":{"request_bytes":65536,"reply_bytes":4194304,"artifact_window_bytes":32768,"operation_working_bytes":33554432},
            "frontend":{"session_timeout_ms":600000,"request_timeout_ms":30000,"max_requests":1000,
                "request_bytes":65536,"total_wire_bytes":67108864,"output_file_bytes":8388608},
            "native":{"page_window":25,"dpi":96.0,"render_pages":false},"ocr":null}),
        )?;
        let clock = RunClockAnchor::capture(SystemTime::now() + Duration::from_secs(300))?;
        let descriptor = write_value(
            &base.join("descriptor.json"),
            &driver::document_model_descriptor(),
        )?;
        let mut controls = Vec::new();
        let mut pins = Vec::new();
        for index in 0..count {
            let binary_bytes = b"synthetic non-executable app-server";
            let binary_path = base.join(format!("binary-{index}"));
            fs::write(&binary_path, binary_bytes)?;
            let binary = DocumentFilePin {
                path: binary_path,
                bytes: binary_bytes.len() as u64,
                sha256: hash(binary_bytes),
            };
            let permissions = write_value(
                &base.join(format!("permissions-{index}.json")),
                &driver::document_permissions_config(None),
            )?;
            let profile = write_value(
                &base.join(format!("profile-{index}.json")),
                &json!({
                "schema":"gptgrep.parse-agent.document-runtime-profile.v1","source_revision":"a".repeat(40),
                "protocol_schema_sha256":"b".repeat(64),"codex_binary_sha256":binary.sha256,
                "descriptor_sha256":descriptor.sha256,"permissions_config_sha256":permissions.sha256,
                "permissions_template_sha256":hash(&serde_json::to_vec(&canonical(driver::document_permissions_template(false)))?),
                "tools_sha256":driver::document_tool_policy_sha256()?,"instructions_sha256":driver::document_instructions_sha256(),
                "standalone_app_server":true,"native_document_guard":null}),
            )?;
            let control = DocumentAgentControl {
                schema: "gptgrep.parse-agent.document-control.v1".into(),
                invocation_id: "synthetic-index".into(),
                codex_bin: binary,
                runtime_profile: profile,
                model_descriptor: descriptor.clone(),
                permissions_config: permissions,
                auth_home: base.join("account"),
                state_dir: base.join("state"),
                cwd: root.clone(),
                environment_id: "local".into(),
                auth_mode: "chatgpt".into(),
                requested_model: "gpt-6-luna".into(),
                reasoning_effort: "max".into(),
                requested_service_tier: "fast".into(),
                original_clock: clock.clone(),
                deadline_unix_ms: clock.deadline_unix_ms(),
                limits: DocumentAgentLimits {
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
                },
                native_viewer: None,
                document_image_qualification: None,
            };
            pins.push(write_value(
                &base.join(format!("document-control-{index}.json")),
                &control,
            )?);
            controls.push(control);
        }
        let registry = write_value(
            &base.join("registry.json"),
            &json!({"schema":"gptgrep.parse-agent.index-controls.v1",
            "parse_agent_config":config,"controls":pins.iter().enumerate().map(|(index,pin)|
                json!({"source_alias":format!("source-{index}"),"document_control":pin})).collect::<Vec<_>>()}),
        )?;
        Ok(Self {
            _temporary: temporary,
            base,
            root,
            config,
            controls,
            pins,
            registry,
        })
    }
    fn prepare(&self) -> Result<PreparedIndex> {
        prepare_inputs(
            &self.root,
            100000,
            &self.config.path,
            &self.config.sha256,
            &self.registry.path,
            &self.registry.sha256,
        )
    }
    fn registry_value(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(&self.registry.path)?)?)
    }
    fn set_registry(&mut self, value: Value) -> Result<()> {
        self.registry = write_value(&self.registry.path, &value)?;
        Ok(())
    }
    fn has_no_generation(&self) -> bool {
        !self.root.join(".gptgrep").exists()
    }
}
fn verify(prepared: &PreparedIndex) -> Result<usize> {
    verify_before_publish(
        &prepared.root,
        prepared.max_files,
        &prepared.sources,
        &prepared.invocation,
        &prepared.registry_pin,
        &prepared.config_pin,
        &prepared.controls,
    )
}

#[test]
fn incomplete_and_duplicate_control_sets_refuse_before_any_generation_or_turn() -> Result<()> {
    let mut fixture = Fixture::new(2)?;
    let mut value = fixture.registry_value()?;
    value["controls"].as_array_mut().unwrap().pop();
    fixture.set_registry(value)?;
    assert!(fixture.prepare().is_err());
    assert!(fixture.has_no_generation());
    let mut fixture = Fixture::new(2)?;
    let mut value = fixture.registry_value()?;
    value["controls"][1]["source_alias"] = json!("source-0");
    fixture.set_registry(value)?;
    assert!(fixture.prepare().is_err());
    assert!(fixture.has_no_generation());
    Ok(())
}
#[test]
fn corpus_inventory_is_complete_and_unsupported_counts_are_separate() -> Result<()> {
    let fixture = Fixture::new(2)?;
    let prepared = fixture.prepare()?;
    assert_eq!(prepared.sources.len(), 2);
    assert_eq!(prepared.initial_excluded, 1);
    assert_eq!(prepared.sources[0].relative, "source-0.txt");
    assert_eq!(prepared.sources[1].relative, "source-1.txt");
    assert!(
        index_source_inventory(&fixture.root, 1).is_err(),
        "max-files must refuse, never truncate"
    );
    fs::write(
        fixture.root.join("unregistered.txt"),
        b"extra supported source",
    )?;
    assert!(fixture.prepare().is_err());
    assert!(fixture.has_no_generation());
    Ok(())
}
#[test]
fn later_binary_pin_is_verified_before_the_first_turn() -> Result<()> {
    let fixture = Fixture::new(2)?;
    let path = &fixture.controls[1].codex_bin.path;
    let mut bytes = fs::read(path)?;
    bytes[0] ^= 1;
    fs::write(path, bytes)?;
    let error = fixture
        .prepare()
        .err()
        .expect("later mismatched binary is rejected")
        .to_string();
    assert!(!error.contains(fixture.base.to_str().unwrap()));
    assert!(fixture.has_no_generation());
    Ok(())
}
#[test]
fn full_inventory_and_same_length_source_freshness_are_rechecked_before_publication() -> Result<()>
{
    let fixture = Fixture::new(1)?;
    let prepared = fixture.prepare()?;
    fs::write(fixture.root.join("another.bin"), b"unsupported metadata")?;
    assert_eq!(
        verify(&prepared)?,
        2,
        "unsupported files are counted, not missing admitted sources"
    );
    fs::write(
        fixture.root.join("another.txt"),
        b"supported source added after preparation",
    )?;
    assert!(verify(&prepared).is_err());
    assert!(fixture.has_no_generation());
    let fixture = Fixture::new(1)?;
    let prepared = fixture.prepare()?;
    let path = &prepared.sources[0].path;
    let mut bytes = fs::read(path)?;
    bytes[0] ^= 1;
    fs::write(path, bytes)?;
    assert!(verify(&prepared).is_err());
    assert!(fixture.has_no_generation());
    Ok(())
}
#[test]
fn exact_config_registry_pins_and_credential_paths_are_not_model_authority() -> Result<()> {
    let mut fixture = Fixture::new(1)?;
    let mut value = fixture.registry_value()?;
    value["parse_agent_config"]["bytes"] = json!(fixture.config.bytes + 1);
    fixture.set_registry(value)?;
    assert!(fixture.prepare().is_err());
    let mut fixture = Fixture::new(1)?;
    let mut value = fixture.registry_value()?;
    value["controls"][0]["document_control"]["path"] = json!(fixture.base.join("auth.json"));
    fixture.set_registry(value)?;
    let error = fixture
        .prepare()
        .err()
        .expect("credential-shaped pin refuses before read")
        .to_string();
    assert!(!error.contains(fixture.base.to_str().unwrap()));
    assert!(fixture.has_no_generation());
    Ok(())
}
#[test]
fn per_source_native_image_receipt_admission_and_catalogue_scopes_cannot_be_reused() -> Result<()> {
    let fixture = Fixture::new(2)?;
    let mut loaded = BTreeMap::new();
    for (index, control) in fixture.controls.iter().enumerate() {
        let mut control = control.clone();
        control.native_viewer = Some(DocumentNativeControl {
            image_root: fixture.base.join(format!("images-{index}")),
            receipt_root: fixture.base.join(format!("receipts-{index}")),
            admission_path: fixture.base.join("shared-admission.json"),
            image_catalog_path: fixture.base.join(format!("catalog-{index}.ndjson")),
            owner_admission_sha256: "a".repeat(64),
            image_wire_bytes: 33554432,
        });
        loaded.insert(
            format!("source-{index}"),
            LoadedControl {
                pin: fixture.pins[index].clone(),
                control,
            },
        );
    }
    assert!(isolated_scopes(&loaded, &fixture.config, &fixture.registry).is_err());
    loaded
        .get_mut("source-1")
        .unwrap()
        .control
        .native_viewer
        .as_mut()
        .unwrap()
        .admission_path = fixture.base.join("admission-1.json");
    assert!(isolated_scopes(&loaded, &fixture.config, &fixture.registry).is_ok());
    let shared = loaded["source-0"]
        .control
        .native_viewer
        .as_ref()
        .unwrap()
        .image_root
        .clone();
    loaded
        .get_mut("source-1")
        .unwrap()
        .control
        .native_viewer
        .as_mut()
        .unwrap()
        .image_root = shared;
    assert!(isolated_scopes(&loaded, &fixture.config, &fixture.registry).is_err());
    Ok(())
}
#[test]
fn private_qualification_output_preflight_refuses_existing_or_widely_readable_sinks() -> Result<()>
{
    let fixture = Fixture::new(1)?;
    let directory = fixture.base.join("private-evidence");
    fs::create_dir(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join("fixture.json");
    preflight_private_qualification_output(&path)?;
    fs::write(&path, b"retained old evidence")?;
    assert!(preflight_private_qualification_output(&path).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))?;
        assert!(preflight_private_qualification_output(&directory.join("new.json")).is_err());
    }
    assert!(fixture.has_no_generation());
    Ok(())
}
#[tokio::test]
async fn typed_generation_transfer_retains_the_original_invocation_counter_and_failed_reservations()
-> Result<()> {
    use gptgrep_core::parse_agent::{
        ParseToolbox, REQUEST_SCHEMA, ToolboxLimits, ToolboxOperation, ToolboxRequest,
    };
    let fixture = Fixture::new(1)?;
    let prepared = fixture.prepare()?;
    let budget = prepared.invocation.resource_budget();
    let before = budget.staging_used();
    budget.reserve_retained_staging(37)?;
    assert_eq!(
        prepared.invocation.resource_budget().staging_used(),
        before + 37
    );
    let mut toolbox = ParseToolbox::new(
        &fixture.base.join("staging"),
        [33; 32],
        budget.clone(),
        ToolboxLimits::default(),
    )?;
    let source = &prepared.sources[0];
    toolbox.register_source(&source.alias, &source.path, source.bytes, &source.sha256)?;
    let request = |id: &str, operation| ToolboxRequest {
        schema: REQUEST_SCHEMA.into(),
        id: id.into(),
        max_reply_bytes: 262144,
        operation,
    };
    let frame = toolbox
        .dispatch(request(
            "prepare",
            ToolboxOperation::Prepare {
                source: source.alias.clone(),
            },
        ))
        .await?;
    let value: Value = serde_json::from_slice(&frame)?;
    let handle = value["data"]["handle"].as_str().unwrap().to_owned();
    toolbox
        .dispatch(request(
            "native",
            ToolboxOperation::ExtractNative {
                handle: handle.clone(),
                page_window: 25,
                dpi: 96.0,
                render_pages: false,
            },
        ))
        .await?;
    toolbox
        .dispatch(request(
            "finalize",
            ToolboxOperation::Finalize {
                handle: handle.clone(),
                mode: gptgrep_parse::OcrMode::Off,
            },
        ))
        .await?;
    let staging = toolbox.prepared_staging_root(&handle)?.to_path_buf();
    let product = toolbox.take_finalized_document(&handle)?;
    assert!(toolbox.finalized_document(&handle).is_err());
    assert_eq!(product.document.text, fs::read_to_string(&source.path)?);
    assert_eq!(
        budget.staging_used(),
        prepared.invocation.resource_budget().staging_used()
    );
    let mut writer = RichGenerationWriter::begin(&fixture.root, budget.clone())?;
    budget.reserve_retained_staging(budget.run_staging_bytes - budget.staging_used())?;
    assert!(
        writer
            .add_document(&source.relative, &staging, product)
            .is_err(),
        "writer cannot borrow a reset counter"
    );
    assert_eq!(budget.staging_used(), budget.run_staging_bytes);
    assert_eq!(
        prepared.invocation.resource_budget().staging_used(),
        budget.run_staging_bytes
    );
    drop(writer);
    assert_eq!(budget.staging_used(), budget.run_staging_bytes);
    assert!(!fixture.root.join(".gptgrep/CURRENT.json").exists());
    Ok(())
}
