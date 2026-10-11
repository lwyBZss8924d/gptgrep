//! Complete owned-product index integration. Controls select existing authority;
//! this module creates no parser/model/storage grant and never reparses a product.
use crate::parse_agent::{self, DocumentAgentInvocation};
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::parse_agent::{IndexSourceInventory, index_source_inventory};
use gptgrep_core::{GenerationPin, RichGenerationWriter};
use gptgrep_host::parse_agent_driver::{
    DocumentAgentControl, DocumentFilePin, load_document_control, preflight_document_agent_control,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tokio::time::Instant;

const REGISTRY_BYTES: u64 = 65_536;
const CONTROL_TOTAL_BYTES: u64 = 1_048_576;
const SOURCE_ROWS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema: String,
    parse_agent_config: DocumentFilePin,
    controls: Vec<ControlRow>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRow {
    source_alias: String,
    document_control: DocumentFilePin,
}
struct LoadedControl {
    pin: DocumentFilePin,
    control: DocumentAgentControl,
}
struct RegisteredSource {
    alias: String,
    path: PathBuf,
    bytes: u64,
    sha256: String,
    relative: String,
}
struct PreparedIndex {
    root: PathBuf,
    max_files: usize,
    registry_pin: DocumentFilePin,
    config_pin: DocumentFilePin,
    controls: BTreeMap<String, LoadedControl>,
    sources: Vec<RegisteredSource>,
    invocation: DocumentAgentInvocation,
    initial_excluded: usize,
}
#[derive(Serialize)]
pub struct ParseAgentIndexReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub generation: GenerationPin,
    pub indexed_files: usize,
    pub source_bytes: u64,
    pub excluded_unsupported_files: usize,
    pub initial_excluded_unsupported_files: usize,
    pub controls_registry_sha256: String,
    pub config_sha256: String,
    pub document_agent_turns: usize,
    pub document_tool_submissions: usize,
    pub declared_document_turn_cap: usize,
    pub declared_document_tool_call_cap: u64,
    pub declared_document_rpc_byte_cap: u64,
    pub document_rpc_bytes: u64,
    pub native_document_controls: usize,
    pub native_document_image_deliveries: usize,
    pub parser_complete_original_pages: usize,
    pub model_inspected_pages: usize,
    pub model_complete_metadata_artifacts: usize,
    pub retained_staging_bytes: u64,
    pub actual_served_profile_verified: bool,
    pub physical_provider_requests: Option<usize>,
    pub actual_cost_usd: Option<f64>,
    pub native_crop_child_augmentation_complete: bool,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn credential(path: &Path) -> bool {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(c) => c.to_str(),
            _ => None,
        })
        .any(|c| {
            let c = c.to_ascii_lowercase();
            matches!(
                c.as_str(),
                "auth.json"
                    | "credentials"
                    | "credentials.json"
                    | "credentials.toml"
                    | "token.json"
                    | "tokens.json"
                    | ".env"
                    | ".ssh"
                    | ".aws"
                    | ".gnupg"
                    | "secrets"
                    | "keychain"
            ) || c.starts_with(".env.")
                || c.ends_with(".env")
                || c.ends_with(".key")
                || c.ends_with(".pem")
                || c.ends_with(".p12")
                || c.ends_with(".keychain")
                || c.ends_with(".keychain-db")
        })
}
fn regular(path: &Path, cap: u64) -> Result<fs::Metadata> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && !credential(path)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "index_control_path"
    );
    let meta = fs::symlink_metadata(path).map_err(|_| anyhow!("index_control_file"))?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() <= cap
            && fs::canonicalize(path).map_err(|_| anyhow!("index_control_file"))? == path,
        "index_control_file"
    );
    Ok(meta)
}
#[cfg(unix)]
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}
#[cfg(not(unix))]
fn same(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}
fn argument_pin(path: &Path, expected_sha: &str, cap: u64) -> Result<DocumentFilePin> {
    ensure!(digest(expected_sha), "index_control_pin");
    Ok(DocumentFilePin {
        path: path.into(),
        bytes: regular(path, cap)?.len(),
        sha256: expected_sha.into(),
    })
}
fn read_pin(pin: &DocumentFilePin, cap: u64) -> Result<Vec<u8>> {
    ensure!(digest(&pin.sha256) && pin.bytes <= cap, "index_control_pin");
    let before = regular(&pin.path, cap)?;
    ensure!(before.len() == pin.bytes, "index_control_pin");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    return Err(anyhow!("index_unix_custody_required"));
    let mut file = options
        .open(&pin.path)
        .map_err(|_| anyhow!("index_control_read"))?;
    ensure!(
        same(
            &before,
            &file.metadata().map_err(|_| anyhow!("index_control_read"))?
        ),
        "index_control_changed"
    );
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(pin.bytes as usize)
        .map_err(|_| anyhow!("index_control_allocation"))?;
    let mut scratch = [0; 4096];
    loop {
        let n = file
            .read(&mut scratch)
            .map_err(|_| anyhow!("index_control_read"))?;
        if n == 0 {
            break;
        }
        ensure!(n <= pin.bytes as usize - bytes.len(), "index_control_pin");
        bytes.extend_from_slice(&scratch[..n]);
    }
    ensure!(
        bytes.len() as u64 == pin.bytes
            && hash(&bytes) == pin.sha256
            && same(
                &before,
                &file.metadata().map_err(|_| anyhow!("index_control_read"))?
            )
            && same(&before, &regular(&pin.path, cap)?),
        "index_control_changed"
    );
    Ok(bytes)
}
fn fresh(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(anyhow!("index_native_destination_not_fresh")),
    }
}
fn bind_sources(
    invocation: &DocumentAgentInvocation,
    inventory: &IndexSourceInventory,
) -> Result<Vec<RegisteredSource>> {
    let registered = invocation.registered_sources().collect::<Vec<_>>();
    ensure!(
        registered.len() == inventory.sources.len() && !registered.is_empty(),
        "index_supported_source_registry_incomplete"
    );
    let mut seen = BTreeSet::new();
    let mut sources = Vec::with_capacity(registered.len());
    for source in registered {
        let selected = inventory
            .sources
            .iter()
            .find(|candidate| candidate.path == source.path)
            .ok_or_else(|| anyhow!("index_source_outside_complete_corpus"))?;
        ensure!(
            seen.insert(selected.relative_path.as_str()),
            "index_duplicate_source_path"
        );
        sources.push(RegisteredSource {
            alias: source.alias.into(),
            path: source.path.into(),
            bytes: source.bytes,
            sha256: source.sha256.into(),
            relative: selected.relative_path.clone(),
        });
    }
    sources.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(sources)
}
fn aliases(controls: &Registry) -> Result<BTreeSet<&str>> {
    ensure!(
        controls.schema == "gptgrep.parse-agent.index-controls.v1"
            && (1..=SOURCE_ROWS).contains(&controls.controls.len()),
        "index_controls_schema"
    );
    let mut aliases = BTreeSet::new();
    let mut bytes = 0u64;
    for row in &controls.controls {
        ensure!(
            !row.source_alias.is_empty()
                && row.source_alias.len() <= 128
                && !row.source_alias.chars().any(char::is_control)
                && aliases.insert(row.source_alias.as_str())
                && row.document_control.bytes <= 65_536
                && digest(&row.document_control.sha256),
            "index_controls_duplicate_or_invalid_alias"
        );
        bytes = bytes
            .checked_add(row.document_control.bytes)
            .filter(|n| *n <= CONTROL_TOTAL_BYTES)
            .ok_or_else(|| anyhow!("index_controls_total_bytes"))?;
    }
    Ok(aliases)
}
fn isolated_scopes(
    controls: &BTreeMap<String, LoadedControl>,
    config: &DocumentFilePin,
    registry: &DocumentFilePin,
) -> Result<()> {
    let mut directories = Vec::new();
    let mut destinations = BTreeSet::new();
    let mut files = vec![config.path.as_path(), registry.path.as_path()];
    let mut protected = Vec::new();
    for loaded in controls.values() {
        files.extend([
            loaded.pin.path.as_path(),
            loaded.control.runtime_profile.path.as_path(),
            loaded.control.model_descriptor.path.as_path(),
            loaded.control.permissions_config.path.as_path(),
        ]);
        protected.extend([
            loaded.control.auth_home.as_path(),
            loaded.control.state_dir.as_path(),
        ]);
        if let Some(qualification) = &loaded.control.document_image_qualification {
            files.push(qualification.path.as_path());
        }
        if let Some(native) = &loaded.control.native_viewer {
            for path in [&native.image_root, &native.receipt_root] {
                fresh(path)?;
                ensure!(
                    destinations.insert(path.as_path()),
                    "index_native_scope_reused"
                );
                ensure!(
                    directories
                        .iter()
                        .all(|previous: &(&Path, bool)| !path.starts_with(previous.0)
                            && !previous.0.starts_with(path)),
                    "index_native_scope_reused"
                );
                directories.push((path.as_path(), path == &native.image_root));
            }
            for path in [&native.admission_path, &native.image_catalog_path] {
                fresh(path)?;
                ensure!(
                    destinations.insert(path.as_path()),
                    "index_native_scope_reused"
                );
            }
            files.extend([
                native.admission_path.as_path(),
                native.image_catalog_path.as_path(),
            ]);
        }
    }
    for (directory, image) in directories {
        ensure!(
            protected
                .iter()
                .all(|path: &&Path| !path.starts_with(directory) && !directory.starts_with(path)),
            "index_native_account_state_scope"
        );
        ensure!(
            files.iter().all(|file| !file.starts_with(directory)),
            if image {
                "index_native_metadata_in_image_scope"
            } else {
                "index_native_metadata_in_receipt_scope"
            }
        );
    }
    Ok(())
}
fn prepare_inputs(
    root: &Path,
    max_files: usize,
    config_path: &Path,
    config_sha: &str,
    registry_path: &Path,
    registry_sha: &str,
) -> Result<PreparedIndex> {
    ensure!((1..=100_000).contains(&max_files), "index_max_files");
    let config_pin = argument_pin(config_path, config_sha, 65_536)?;
    let registry_pin = argument_pin(registry_path, registry_sha, REGISTRY_BYTES)?;
    let bytes = read_pin(&registry_pin, REGISTRY_BYTES)?;
    let registry: Registry =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("index_controls_schema"))?;
    drop(bytes);
    let selected_aliases = aliases(&registry)?;
    ensure!(
        registry.parse_agent_config == config_pin,
        "index_controls_config_pin"
    );
    let mut controls = BTreeMap::new();
    for row in &registry.controls {
        // Check all declared control bytes and exact digest, including later
        // documents, before constructing a turn or beginning a generation.
        read_pin(&row.document_control, 65_536)?;
        let control =
            load_document_control(&row.document_control.path, &row.document_control.sha256)?;
        controls.insert(
            row.source_alias.clone(),
            LoadedControl {
                pin: row.document_control.clone(),
                control,
            },
        );
    }
    let first = &controls
        .values()
        .next()
        .ok_or_else(|| anyhow!("index_controls_empty"))?
        .control;
    for loaded in controls.values() {
        ensure!(
            loaded.control.invocation_id == first.invocation_id
                && loaded.control.original_clock == first.original_clock
                && loaded.control.deadline_unix_ms == first.deadline_unix_ms
                && loaded.control.limits.working_bytes == first.limits.working_bytes,
            "index_controls_shared_invocation_binding"
        );
        first
            .original_clock
            .remaining()
            .map_err(|_| anyhow!("index_original_deadline"))?;
        preflight_document_agent_control(&loaded.control)?;
    }
    isolated_scopes(&controls, &config_pin, &registry_pin)?;
    let invocation = DocumentAgentInvocation::load(config_path, config_sha, first)?;
    let actual_aliases = invocation
        .registered_sources()
        .map(|source| source.alias)
        .collect::<BTreeSet<_>>();
    ensure!(
        actual_aliases == selected_aliases,
        "index_controls_alias_registry_incomplete"
    );
    // The complete registry has at most64 sources. Refuse an extra source,
    // rather than allocate an unrelated large corpus list or truncate it.
    let inventory = index_source_inventory(root, max_files.min(actual_aliases.len()))?;
    let sources = bind_sources(&invocation, &inventory)?;
    invocation.verify_registry_fresh()?;
    let root = inventory.root;
    let initial_excluded = inventory.excluded_unsupported_files;
    Ok(PreparedIndex {
        root,
        max_files: max_files.min(sources.len()),
        registry_pin,
        config_pin,
        controls,
        sources,
        invocation,
        initial_excluded,
    })
}
fn verify_before_publish(
    root: &Path,
    max_files: usize,
    sources: &[RegisteredSource],
    invocation: &DocumentAgentInvocation,
    registry: &DocumentFilePin,
    config: &DocumentFilePin,
    controls: &BTreeMap<String, LoadedControl>,
) -> Result<usize> {
    read_pin(registry, REGISTRY_BYTES)?;
    read_pin(config, 65_536)?;
    for loaded in controls.values() {
        read_pin(&loaded.pin, 65_536)?;
    }
    let inventory = index_source_inventory(root, max_files)?;
    ensure!(
        inventory.sources.len() == sources.len()
            && inventory
                .sources
                .iter()
                .zip(sources)
                .all(|(a, b)| a.path == b.path && a.relative_path == b.relative),
        "index_supported_source_inventory_changed"
    );
    invocation.verify_registry_fresh()?;
    Ok(inventory.excluded_unsupported_files)
}
pub async fn index(
    root: &Path,
    max_files: usize,
    config_path: &Path,
    config_sha: &str,
    registry_path: &Path,
    registry_sha: &str,
) -> Result<ParseAgentIndexReport> {
    let prepared = prepare_inputs(
        root,
        max_files,
        config_path,
        config_sha,
        registry_path,
        registry_sha,
    )?;
    let PreparedIndex {
        root,
        max_files,
        registry_pin,
        config_pin,
        controls,
        sources,
        mut invocation,
        initial_excluded,
    } = prepared;
    let declared_tools = controls.values().try_fold(0u64, |n, c| {
        n.checked_add(c.control.limits.max_tool_calls as u64)
            .ok_or_else(|| anyhow!("index_accounting"))
    })?;
    let declared_rpc = controls.values().try_fold(0u64, |n, c| {
        n.checked_add(c.control.limits.total_rpc_bytes as u64)
            .ok_or_else(|| anyhow!("index_accounting"))
    })?;
    let budget = invocation.resource_budget();
    let mut writer = RichGenerationWriter::begin(&root, budget.clone())?;
    let mut submissions = 0usize;
    let mut rpc = 0u64;
    let mut native_controls = 0usize;
    let mut native_deliveries = 0usize;
    let mut pages = 0usize;
    let mut inspected_pages = 0usize;
    let mut complete_metadata = 0usize;
    let mut source_bytes = 0u64;
    for source in &sources {
        let selected = &controls
            .get(&source.alias)
            .ok_or_else(|| anyhow!("index_controls_alias_registry_incomplete"))?
            .control;
        selected
            .original_clock
            .remaining()
            .map_err(|_| anyhow!("index_original_deadline"))?;
        let product = invocation.prepare_source(selected, &source.alias).await?;
        let parts = product.into_index_parts_and_invocation()?;
        parts.budget.ensure_same_invocation_within(&budget)?;
        ensure!(
            parts.document.summary.source_sha256 == source.sha256
                && parts.outcome.verified_product.source_sha256 == source.sha256
                && parts.document.summary.source_bytes == source.bytes
                && parts.document.pages.len() == parts.outcome.verified_product.original_pages
                && parts.outcome.purpose == "product"
                && parts
                    .outcome
                    .verified_product
                    .parser_complete_original_pages
                && parts.outcome.verified_product.all_source_artifacts_verified
                && parts.outcome.verified_product.required_ocr_complete
                && parts.outcome.native_viewer_enabled == selected.native_viewer.is_some(),
            "index_unverified_product"
        );
        submissions = submissions
            .checked_add(parts.outcome.tool_submissions)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        rpc = rpc
            .checked_add(parts.outcome.rpc_bytes as u64)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        native_controls = native_controls
            .checked_add(usize::from(parts.outcome.native_viewer_enabled))
            .ok_or_else(|| anyhow!("index_accounting"))?;
        let native_calls = parts
            .outcome
            .tool_submissions
            .checked_sub(parts.outcome.deliveries.len())
            .ok_or_else(|| anyhow!("index_accounting"))?;
        ensure!(
            native_calls == 0
                || parts.outcome.native_receipts_verified
                    && parts
                        .outcome
                        .native_image_source_prepared_assembled_verified,
            "index_native_delivery_unverified"
        );
        native_deliveries = native_deliveries
            .checked_add(native_calls)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        pages = pages
            .checked_add(parts.outcome.verified_product.original_pages)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        inspected_pages = inspected_pages
            .checked_add(parts.outcome.verified_product.delivered_pages)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        complete_metadata = complete_metadata
            .checked_add(parts.outcome.verified_product.native_metadata_complete)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        source_bytes = source_bytes
            .checked_add(source.bytes)
            .ok_or_else(|| anyhow!("index_accounting"))?;
        // Consume the verified typed candidate once. The writer verifies/copies
        // all source-bound artifacts and retains the same budget; no reparse.
        writer.add_document(&source.relative, &parts.staging_root, parts.document)?;
        invocation = parts.invocation;
        selected
            .original_clock
            .remaining()
            .map_err(|_| anyhow!("index_original_deadline"))?;
    }
    ensure!(
        submissions as u64 <= declared_tools && rpc <= declared_rpc,
        "index_accounting"
    );
    let mut excluded = initial_excluded;
    let generation = writer.publish_checked(|| {
        excluded = verify_before_publish(
            &root,
            max_files,
            &sources,
            &invocation,
            &registry_pin,
            &config_pin,
            &controls,
        )?;
        controls
            .values()
            .next()
            .ok_or_else(|| anyhow!("index_controls_empty"))?
            .control
            .original_clock
            .remaining()
            .map_err(|_| anyhow!("index_original_deadline_precommit"))?;
        Ok(())
    })?;
    controls
        .values()
        .next()
        .ok_or_else(|| anyhow!("index_controls_empty"))?
        .control
        .original_clock
        .remaining()
        .map_err(|_| anyhow!("index_original_deadline_after_publication"))?;
    Ok(ParseAgentIndexReport {
        schema: "gptgrep.parse-agent.index.v1",
        status: "complete",
        generation,
        indexed_files: sources.len(),
        source_bytes,
        excluded_unsupported_files: excluded,
        initial_excluded_unsupported_files: initial_excluded,
        controls_registry_sha256: registry_pin.sha256,
        config_sha256: config_pin.sha256,
        document_agent_turns: sources.len(),
        declared_document_turn_cap: sources.len(),
        declared_document_tool_call_cap: declared_tools,
        declared_document_rpc_byte_cap: declared_rpc,
        document_tool_submissions: submissions,
        document_rpc_bytes: rpc,
        native_document_controls: native_controls,
        native_document_image_deliveries: native_deliveries,
        parser_complete_original_pages: pages,
        model_inspected_pages: inspected_pages,
        model_complete_metadata_artifacts: complete_metadata,
        retained_staging_bytes: budget.staging_used(),
        actual_served_profile_verified: false,
        physical_provider_requests: None,
        actual_cost_usd: None,
        native_crop_child_augmentation_complete: false,
    })
}

/// Explicit private evidence sink for the qualification subcommand. It writes
/// complete borrowed fields with LF and counts exact retained bytes before I/O.
pub fn preflight_private_qualification_output(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && !credential(path)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && !path.exists()
            && !path.is_symlink(),
        "qualification_private_output_path"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("qualification_private_output_path"))?;
    ensure!(
        fs::canonicalize(parent).map_err(|_| anyhow!("qualification_private_output_path"))?
            == parent
            && fs::metadata(parent)
                .map_err(|_| anyhow!("qualification_private_output_path"))?
                .is_dir(),
        "qualification_private_output_path"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(parent)?.permissions().mode() & 0o077 == 0,
            "qualification_private_output_directory"
        );
    }
    #[cfg(not(unix))]
    return Err(anyhow!("qualification_unix_custody_required"));
    Ok(())
}
struct PrivateOutput {
    file: File,
    #[cfg(unix)]
    parent: File,
    #[cfg(unix)]
    parent_path: PathBuf,
    #[cfg(unix)]
    leaf: std::ffi::CString,
    #[cfg(unix)]
    parent_identity: (u64, u64),
}
impl PrivateOutput {
    fn new(path: &Path) -> Result<Self> {
        preflight_private_qualification_output(path)?;
        #[cfg(not(unix))]
        return Err(anyhow!("qualification_unix_custody_required"));
        #[cfg(unix)]
        {
            use std::os::unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
                io::{AsRawFd, FromRawFd},
            };
            let parent_path = path
                .parent()
                .ok_or_else(|| anyhow!("qualification_private_output_path"))?
                .to_path_buf();
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let parent = options
                .open(&parent_path)
                .map_err(|_| anyhow!("qualification_private_output_parent"))?;
            let metadata = parent
                .metadata()
                .map_err(|_| anyhow!("qualification_private_output_parent"))?;
            let parent_identity = (metadata.dev(), metadata.ino());
            let leaf = std::ffi::CString::new(
                path.file_name()
                    .ok_or_else(|| anyhow!("qualification_private_output_path"))?
                    .as_bytes(),
            )
            .map_err(|_| anyhow!("qualification_private_output_path"))?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            ensure!(fd >= 0, "qualification_private_output_not_fresh");
            let output = Self {
                file: unsafe { File::from_raw_fd(fd) },
                parent,
                parent_path,
                leaf,
                parent_identity,
            };
            output.check(0)?;
            Ok(output)
        }
    }
    #[allow(clippy::unnecessary_cast)] // Unix stat widths differ across supported ABIs.
    fn check(&self, bytes: u64) -> Result<()> {
        #[cfg(not(unix))]
        return Err(anyhow!("qualification_unix_custody_required"));
        #[cfg(unix)]
        {
            use std::os::unix::{
                fs::{MetadataExt, PermissionsExt},
                io::AsRawFd,
            };
            let parent = fs::symlink_metadata(&self.parent_path)
                .map_err(|_| anyhow!("qualification_private_output_changed"))?;
            let held = self
                .parent
                .metadata()
                .map_err(|_| anyhow!("qualification_private_output_changed"))?;
            ensure!(
                parent.is_dir()
                    && !parent.file_type().is_symlink()
                    && parent.permissions().mode() & 0o077 == 0
                    && (parent.dev(), parent.ino()) == self.parent_identity
                    && (held.dev(), held.ino()) == self.parent_identity
                    && fs::canonicalize(&self.parent_path)
                        .map_err(|_| anyhow!("qualification_private_output_changed"))?
                        == self.parent_path,
                "qualification_private_output_changed"
            );
            let metadata = self
                .file
                .metadata()
                .map_err(|_| anyhow!("qualification_private_output_changed"))?;
            let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
            ensure!(
                unsafe {
                    libc::fstatat(
                        self.parent.as_raw_fd(),
                        self.leaf.as_ptr(),
                        entry.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } == 0,
                "qualification_private_output_changed"
            );
            let entry = unsafe { entry.assume_init() };
            ensure!(
                metadata.is_file()
                    && metadata.len() == bytes
                    && metadata.nlink() == 1
                    && metadata.permissions().mode() & 0o077 == 0
                    && entry.st_mode & libc::S_IFMT == libc::S_IFREG
                    && entry.st_dev as u64 == metadata.dev()
                    && entry.st_ino as u64 == metadata.ino(),
                "qualification_private_output_changed"
            );
            Ok(())
        }
    }
}
pub fn write_private_qualification(
    evidence: parse_agent::DocumentImageQualificationEvidence,
    path: &Path,
    control: &DocumentAgentControl,
) -> Result<()> {
    preflight_private_qualification_output(path)?;
    let parts = evidence.into_private_fixture_parts()?;
    #[derive(Serialize)]
    struct Output<'a> {
        schema: &'static str,
        purpose_sha256: &'a str,
        publication_authorized: bool,
        staging_root: &'a Path,
        document: &'a gptgrep_parse::RichParsedDocument,
        outcome: &'a gptgrep_host::parse_agent_driver::DocumentAgentOutcome,
    }
    let output = Output {
        schema: "gptgrep.native-document.private-qualification-fixture.v1",
        purpose_sha256: &parts.purpose_sha256,
        publication_authorized: false,
        staging_root: &parts.staging_root,
        document: &parts.document,
        outcome: &parts.outcome,
    };
    let remaining = control
        .original_clock
        .remaining()
        .map_err(|_| anyhow!("qualification_original_deadline"))?;
    let deadline = Instant::now()
        .checked_add(remaining)
        .ok_or_else(|| anyhow!("qualification_original_deadline"))?;
    let cap = control.limits.total_rpc_bytes as u64;
    struct Count {
        bytes: u64,
        cap: u64,
        deadline: Instant,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::other("qualification_original_deadline"));
            }
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= self.cap)
                .ok_or_else(|| std::io::Error::other("qualification_output_limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count {
        bytes: 0,
        cap,
        deadline,
    };
    serde_json::to_writer(&mut count, &output)?;
    count.write_all(b"\n")?;
    parts.budget.reserve_retained_staging(count.bytes)?;
    let mut private = PrivateOutput::new(path)?;
    struct Bounded<'a> {
        file: &'a mut File,
        count: Count,
    }
    impl Write for Bounded<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.count.write_all(bytes)?;
            self.file.write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    let mut writer = Bounded {
        file: &mut private.file,
        count: Count {
            bytes: 0,
            cap: count.bytes,
            deadline,
        },
    };
    serde_json::to_writer(&mut writer, &output)?;
    writer.write_all(b"\n")?;
    ensure!(
        writer.count.bytes == count.bytes,
        "qualification_output_changed"
    );
    private.file.sync_all()?;
    private.check(count.bytes)?;
    control
        .original_clock
        .remaining()
        .map_err(|_| anyhow!("qualification_original_deadline"))?;
    Ok(())
}

#[cfg(test)]
#[path = "parse_agent_index_tests.rs"]
mod tests;
