//! Source-custodied native v2 groups; packaging never parses documents or runs an agent.
use super::{
    MAX_FILE, MAX_JSON, MAX_OUTPUT, Result, check_hash, check_id, check_rel, hash, json,
    regular_file,
};
use gptgrep_core::{GenerationPin, RichGenerationReader};
use gptgrep_parse::{CoverageState, ResourceBudget};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) const EXPORT_SCHEMA: &str = "gptgrep.harbor.export.v2";
const MAX_FILES: usize = 4096;
const ENGINE_FILES: [&str; 7] = [
    "gptgrep-index.json",
    "meta.json",
    "files-extra.bin",
    "filestamps.json",
    "files.bin",
    "lookup.bin",
    "index.bin",
];
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn strict<T: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(err)
}

struct Held {
    path: PathBuf,
    file: File,
    sha: String,
    bytes: u64,
    device: u64,
    inode: u64,
}
impl Held {
    fn open(path: &Path, expected: Option<&str>) -> Result<Arc<Self>> {
        Self::open_limit(path, expected, MAX_FILE)
    }
    fn open_limit(path: &Path, expected: Option<&str>, limit: u64) -> Result<Arc<Self>> {
        let file = regular_file(path, limit)?;
        let m = file.metadata().map_err(err)?;
        let mut h = Self {
            path: path.to_path_buf(),
            file,
            sha: String::new(),
            bytes: m.len(),
            device: m.dev(),
            inode: m.ino(),
        };
        h.sha = h.measure(None)?;
        if expected.is_some_and(|s| s != h.sha) {
            return Err("production_asset_digest".into());
        }
        Ok(Arc::new(h))
    }
    fn measure(&self, mut output: Option<&mut File>) -> Result<String> {
        let mut file = self.file.try_clone().map_err(err)?;
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut digest = Sha256::new();
        let mut n = 0u64;
        let mut scratch = [0u8; 65536];
        loop {
            let got = file.read(&mut scratch).map_err(err)?;
            if got == 0 {
                break;
            }
            n = n
                .checked_add(got as u64)
                .filter(|n| *n <= self.bytes && *n <= MAX_FILE)
                .ok_or("production_copy_growth")?;
            digest.update(&scratch[..got]);
            if let Some(target) = output.as_mut() {
                target.write_all(&scratch[..got]).map_err(err)?;
            }
        }
        if n != self.bytes {
            return Err("production_copy_shortened".into());
        }
        Ok(format!("{:x}", digest.finalize()))
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        if self.bytes > MAX_JSON {
            return Err("production_json_limit".into());
        }
        let mut f = self.file.try_clone().map_err(err)?;
        f.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut bytes = Vec::new();
        f.take(MAX_JSON + 1).read_to_end(&mut bytes).map_err(err)?;
        if bytes.len() as u64 != self.bytes || hash(&bytes) != self.sha {
            return Err("production_snapshot_changed".into());
        }
        Ok(bytes)
    }
    fn fresh(&self) -> Result<()> {
        let f = regular_file(&self.path, MAX_FILE)?;
        let m = f.metadata().map_err(err)?;
        if (m.dev(), m.ino(), m.len()) != (self.device, self.inode, self.bytes)
            || self.measure(None)? != self.sha
        {
            return Err("production_source_changed".into());
        }
        let mut fresh = Self {
            path: self.path.clone(),
            file: f,
            sha: self.sha.clone(),
            bytes: self.bytes,
            device: self.device,
            inode: self.inode,
        };
        fresh.sha = fresh.measure(None)?;
        if fresh.sha != self.sha {
            return Err("production_named_source_changed".into());
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct External {
    path: PathBuf,
    sha256: String,
}
impl External {
    fn admit(&self) -> Result<Arc<Held>> {
        check_hash(&self.sha256)?;
        if !self.path.is_absolute() {
            return Err("production_asset_absolute".into());
        }
        Held::open(&self.path, Some(&self.sha256))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupInput {
    id: String,
    source_root: PathBuf,
    runtime_root: PathBuf,
    sealed_receipt: External,
    sealed_pin: External,
    sealed_raw_manifest: External,
    sealed_controls: External,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    asset_path: PathBuf,
    sha256: String,
    version: String,
    source_revision: String,
    os: String,
    architecture: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    schema_version: String,
    groups: Vec<GroupInput>,
    tool: Tool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema_version: String,
    group_id: String,
    documents: Vec<Raw>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Native {
    page_window: u32,
    dpi: f64,
    render_pages: bool,
    max_document_pages: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hard {
    native_wall: bool,
    native_rss: bool,
    total_index_publication_bytes: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Controls {
    schema_version: String,
    group_id: String,
    observation_wall_seconds: u64,
    raw_group_bytes: u64,
    seal_inventory_bytes: u64,
    seal_inventory_files: usize,
    native: Native,
    resource_budget: ResourceBudget,
    hard_limits: Hard,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Inventory {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    schema_version: String,
    qualification: String,
    producer_observation: String,
    canonical_root: PathBuf,
    group_id: String,
    raw_manifest_sha256: String,
    controls_sha256: String,
    raw_documents: Vec<Raw>,
    pin: GenerationPin,
    pin_input_sha256: Option<String>,
    current_snapshot_sha256: String,
    inventory_scope: String,
    inventory: Vec<Inventory>,
    generation_bytes: u64,
    coverage: Value,
    parser_identity: String,
    liteparse_revision: String,
    index_engine_revision: String,
    index_file_table_id: String,
    index_manifest_snapshot_sha256: String,
    builder_source_revision: Option<String>,
    builder_binary_sha256: Option<String>,
    selected_resource_budget: ResourceBudget,
    shared_staging_reservation_bytes_observed: Option<u64>,
    elapsed_ms: u128,
    native_library_identity: Option<String>,
    native_peak_rss_bytes: Option<u64>,
    current_frontdoor_provider_calls: u32,
    hard_executor_qualification: String,
    wrapper_json_buffer_ceiling_bytes: u64,
    retained_raw_payload_bytes: u64,
    retained_raw_file_descriptors: usize,
    retained_generation_file_descriptors: usize,
    wrapper_metadata_scope: String,
}
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupBinding {
    id: String,
    canonical_root: PathBuf,
    runtime_root: PathBuf,
    raw_manifest_sha256: String,
    controls_sha256: String,
    seal_sha256: String,
    current_snapshot_sha256: String,
    pin: GenerationPin,
    raw_documents: Vec<Raw>,
    generation_inventory: Vec<Inventory>,
    generation_bytes: u64,
    native_coverage: Value,
    index_engine_revision: String,
    index_file_table_id: String,
    index_manifest_sha256: String,
}
struct Group {
    binding: GroupBinding,
    reader: RichGenerationReader,
    raw: Vec<(String, Arc<Held>)>,
    generation: Vec<(String, Arc<Held>)>,
    current: Arc<Held>,
    seals: Vec<(String, Arc<Held>)>,
}
struct AdmittedCorpus {
    digest: String,
    groups: Vec<Group>,
    tool: Tool,
    tool_file: Arc<Held>,
    held: Vec<Arc<Held>>,
}
fn scan(root: &Path, skip_state: bool) -> Result<BTreeSet<String>> {
    fn walk(
        root: &Path,
        prefix: &str,
        skip: bool,
        out: &mut BTreeSet<String>,
        visited: &mut usize,
        depth: usize,
    ) -> Result<()> {
        if depth > 16 {
            return Err("production_depth_limit".into());
        }
        for e in fs::read_dir(root).map_err(err)? {
            let e = e.map_err(err)?;
            *visited += 1;
            if *visited > MAX_FILES {
                return Err("production_inventory_limit".into());
            }
            let leaf = e
                .file_name()
                .into_string()
                .map_err(|_| "production_path_utf8")?;
            if skip && prefix.is_empty() && leaf == ".gptgrep" {
                if !e.file_type().map_err(err)?.is_dir() {
                    return Err("production_state_type".into());
                }
                continue;
            }
            let name = if prefix.is_empty() {
                leaf
            } else {
                format!("{prefix}/{leaf}")
            };
            check_rel(&name)?;
            let kind = e.file_type().map_err(err)?;
            if kind.is_dir() {
                walk(&e.path(), &name, false, out, visited, depth + 1)?;
            } else if kind.is_file() {
                out.insert(name);
            } else {
                return Err("production_special_or_symlink".into());
            }
        }
        Ok(())
    }
    let mut out = BTreeSet::new();
    walk(root, "", skip_state, &mut out, &mut 0, 0)?;
    Ok(out)
}
fn canonical_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.canonicalize().map_err(err)? != path
        || !fs::symlink_metadata(path).map_err(err)?.is_dir()
    {
        return Err("production_root_canonical".into());
    }
    // Dockerfile destinations are literal bounded paths, never shell strings.
    if path.as_os_str().len() > 512
        || !path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(b))
    {
        return Err("production_runtime_path".into());
    }
    Ok(path.to_path_buf())
}
fn admit(path: &Path) -> Result<AdmittedCorpus> {
    let corpus_file = Held::open(path, None)?;
    let bytes = corpus_file.bytes()?;
    let corpus: Corpus = strict(&bytes)?;
    if corpus.schema_version != "gptgrep.harbor.production-corpus.v1"
        || corpus.groups.is_empty()
        || corpus.groups.len() > 16
    {
        return Err("production_corpus_schema_or_count".into());
    }
    let tool = &corpus.tool;
    check_hash(&tool.sha256)?;
    if tool.version.is_empty()
        || tool.version.len() > 128
        || tool.source_revision.len() != 40
        || !tool.source_revision.bytes().all(|b| b.is_ascii_hexdigit())
        || [&tool.os, &tool.architecture]
            .iter()
            .any(|s| s.is_empty() || s.len() > 64)
    {
        return Err("production_tool_metadata".into());
    }
    let tool_file = Held::open(&tool.asset_path, Some(&tool.sha256))?;
    if tool_file.bytes == 0 {
        return Err("production_empty_tool".into());
    }
    let mut held = vec![corpus_file, tool_file.clone()];
    let mut groups = Vec::new();
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut raw_aliases = BTreeSet::new();
    let mut docs = 0usize;
    for input in corpus.groups {
        check_id(&input.id)?;
        let root = canonical_root(&input.source_root)?;
        if input.runtime_root != root {
            return Err("production_relocation_requires_target_root_rebuild".into());
        }
        if !ids.insert(input.id.clone()) || !roots.insert(root.clone()) {
            return Err("production_duplicate_group".into());
        }
        let receipt = input.sealed_receipt.admit()?;
        let pin_file = input.sealed_pin.admit()?;
        let raw_file = input.sealed_raw_manifest.admit()?;
        let control_file = input.sealed_controls.admit()?;
        let seal: Seal = strict(&receipt.bytes()?)?;
        let pin: GenerationPin = strict(&pin_file.bytes()?)?;
        let raw: RawManifest = strict(&raw_file.bytes()?)?;
        let controls: Controls = strict(&control_file.bytes()?)?;
        if seal.schema_version != "gptgrep.harbor.prebuild.receipt.v1"
            || seal.qualification != "source_structure_only_not_production_exporter_admission"
            || seal.canonical_root != root
            || seal.group_id != input.id
            || raw.schema_version != "gptgrep.harbor.raw-group.v1"
            || raw.group_id != input.id
            || controls.schema_version != "gptgrep.harbor.native-controls.v1"
            || controls.group_id != input.id
            || seal.pin != pin
            || seal.raw_manifest_sha256 != raw_file.sha
            || seal.controls_sha256 != control_file.sha
            || json(&seal.raw_documents)? != json(&raw.documents)?
        {
            return Err("production_seal_binding".into());
        }
        if controls.observation_wall_seconds == 0
            || controls.observation_wall_seconds > 3600
            || controls.raw_group_bytes == 0
            || controls.raw_group_bytes > MAX_FILE
            || controls.seal_inventory_bytes == 0
            || controls.seal_inventory_bytes > 1_073_741_824
            || controls.seal_inventory_files == 0
            || controls.seal_inventory_files > MAX_FILES
            || controls.native.page_window == 0
            || controls.native.page_window > 100
            || !controls.native.dpi.is_finite()
            || controls.native.dpi <= 0.0
            || !controls.native.render_pages
            || controls.native.max_document_pages == 0
            || controls.hard_limits.native_wall
            || controls.hard_limits.native_rss
            || controls.hard_limits.total_index_publication_bytes
        {
            return Err("production_controls_bounds".into());
        }
        controls.resource_budget.validate().map_err(err)?;
        if json(&controls.resource_budget)? != json(&seal.selected_resource_budget)?
            || seal.parser_identity != gptgrep_parse::RICH_PARSER
            || seal.liteparse_revision != gptgrep_parse::LITEPARSE_REVISION
            || seal.inventory_scope
                != "exact_current_snapshot_and_selected_generation_only_sibling_history_unverified"
        {
            return Err("production_profile_binding".into());
        }
        // Bind optional/observational metadata without treating it as authenticated authority.
        let _observations = (
            &seal.producer_observation,
            &seal.pin_input_sha256,
            &seal.builder_source_revision,
            &seal.builder_binary_sha256,
            &seal.shared_staging_reservation_bytes_observed,
            seal.elapsed_ms,
            &seal.native_library_identity,
            seal.native_peak_rss_bytes,
            seal.current_frontdoor_provider_calls,
            &seal.hard_executor_qualification,
            seal.wrapper_json_buffer_ceiling_bytes,
            seal.retained_raw_payload_bytes,
            seal.retained_raw_file_descriptors,
            seal.retained_generation_file_descriptors,
            &seal.wrapper_metadata_scope,
        );
        let current = Held::open(
            &root.join(".gptgrep/CURRENT.json"),
            Some(&seal.current_snapshot_sha256),
        )?;
        let pointer: Value = strict(&current.bytes()?)?;
        if pointer
            != value!({"schema_version":"gptgrep.manifest.v2","generation":pin.generation,"manifest_sha256":pin.manifest_sha256})
        {
            return Err("production_current_pin".into());
        }
        if raw.documents.is_empty() {
            return Err("production_empty_raw".into());
        }
        docs += raw.documents.len();
        if docs > 16 {
            return Err("production_document_limit".into());
        }
        let mut raw_assets = Vec::new();
        let mut raw_set = BTreeSet::new();
        let mut total = 0u64;
        for doc in &raw.documents {
            check_rel(&doc.path)?;
            check_hash(&doc.sha256)?;
            if !doc.path.ends_with(".pdf") || !raw_set.insert(doc.path.clone()) {
                return Err("production_raw_path".into());
            }
            let asset = Held::open(&root.join(&doc.path), Some(&doc.sha256))?;
            if asset.bytes != doc.bytes
                || asset.bytes == 0
                || !raw_aliases.insert((asset.device, asset.inode))
            {
                return Err("production_raw_size_or_alias".into());
            }
            let mut prefix = [0u8; 5];
            let mut f = asset.file.try_clone().map_err(err)?;
            f.seek(SeekFrom::Start(0)).map_err(err)?;
            f.read_exact(&mut prefix).map_err(err)?;
            if &prefix != b"%PDF-" {
                return Err("production_raw_pdf_header".into());
            }
            total = total
                .checked_add(asset.bytes)
                .filter(|n| *n <= controls.raw_group_bytes)
                .ok_or("production_raw_budget")?;
            raw_assets.push((doc.path.clone(), asset));
        }
        if scan(&root, true)? != raw_set {
            return Err("production_raw_set".into());
        }
        let gen_root = root.join(".gptgrep/generations").join(&pin.generation);
        if seal.inventory.len() > controls.seal_inventory_files
            || seal.generation_bytes > MAX_OUTPUT as u64
        {
            return Err("production_generation_budget".into());
        }
        let mut generation = Vec::new();
        let mut names = BTreeSet::new();
        let mut n = 0u64;
        for item in &seal.inventory {
            check_rel(&item.path)?;
            check_hash(&item.sha256)?;
            if !names.insert(item.path.clone()) {
                return Err("production_duplicate_artifact".into());
            }
            let file = Held::open(&gen_root.join(&item.path), Some(&item.sha256))?;
            if file.bytes != item.bytes {
                return Err("production_generation_size".into());
            }
            n = n
                .checked_add(file.bytes)
                .filter(|n| *n <= controls.seal_inventory_bytes && *n <= MAX_OUTPUT as u64)
                .ok_or("production_generation_budget")?;
            generation.push((item.path.clone(), file));
        }
        if n != seal.generation_bytes || scan(&gen_root, false)? != names {
            return Err("production_generation_inventory".into());
        }
        let reader = RichGenerationReader::open_pinned(&root, &pin).map_err(err)?;
        reader.verify_all().map_err(err)?;
        let m = reader.manifest().map_err(err)?;
        if m.documents.len() != raw.documents.len()
            || json(&m.resource_budget)? != json(&controls.resource_budget)?
        {
            return Err("production_manifest_raw_or_resource".into());
        }
        let mut expected = BTreeSet::from(["manifest.json".to_string()]);
        let mut coverage = Vec::new();
        for (d, r) in m.documents.iter().zip(&raw.documents) {
            if d.document.path != r.path
                || d.document.source_sha256 != r.sha256
                || d.document.source_bytes != r.bytes
                || d.prepared.source_sha256 != r.sha256
                || d.prepared.prepared_sha256 != r.sha256
                || d.prepared.format != "pdf"
                || !d.ocr_tasks.is_empty()
                || !d.ocr_results.is_empty()
                || d.pages.is_empty()
                || d.pages.len() > controls.native.max_document_pages
                || d.pages.iter().any(|p| {
                    !matches!(
                        p.coverage,
                        CoverageState::Native | CoverageState::BlankVerified
                    )
                })
            {
                return Err("production_native_coverage_or_source".into());
            }
            expected.insert(
                d.prepared_path
                    .to_str()
                    .ok_or("production_utf8")?
                    .to_string(),
            );
            expected.insert(format!("text/{}.txt", d.document.id));
            for a in &d.artifacts {
                expected.insert(
                    a.artifact
                        .relative_path
                        .to_str()
                        .ok_or("production_utf8")?
                        .to_string(),
                );
                if matches!(
                    a.artifact.kind,
                    gptgrep_parse::ArtifactKind::NativeText | gptgrep_parse::ArtifactKind::OcrText
                ) {
                    expected.insert(format!("artifact-text/{}.txt", a.artifact.id));
                }
            }
            coverage.push(value!({"path":r.path,"source_sha256":r.sha256,"prepared_sha256":d.prepared.prepared_sha256,"preparation_profile_sha256":d.prepared.preparation_profile_sha256,"native_profile_sha256":d.native_profile_sha256,"pages":d.pages.iter().map(|p|(p.number,&p.coverage)).collect::<Vec<_>>()}));
        }
        for f in ENGINE_FILES {
            expected.insert(format!("artifact-index/{f}"));
        }
        if expected != names || value!(coverage) != seal.coverage {
            return Err("production_complete_generation_or_coverage".into());
        }
        let index = generation
            .iter()
            .find(|(n, _)| n == "artifact-index/gptgrep-index.json")
            .ok_or("production_index_missing")?
            .1
            .clone();
        let index_json: Value = strict(&index.bytes()?)?;
        if index.sha != seal.index_manifest_snapshot_sha256
            || index_json["engine_revision"] != seal.index_engine_revision
            || index_json["file_table_id"] != seal.index_file_table_id
        {
            return Err("production_index_identity".into());
        }
        let binding = GroupBinding {
            id: input.id,
            canonical_root: root.clone(),
            runtime_root: root,
            raw_manifest_sha256: raw_file.sha.clone(),
            controls_sha256: control_file.sha.clone(),
            seal_sha256: receipt.sha.clone(),
            current_snapshot_sha256: current.sha.clone(),
            pin,
            raw_documents: raw.documents,
            generation_inventory: seal.inventory,
            generation_bytes: n,
            native_coverage: seal.coverage,
            index_engine_revision: seal.index_engine_revision,
            index_file_table_id: seal.index_file_table_id,
            index_manifest_sha256: index.sha.clone(),
        };
        let seals = vec![
            ("receipt.json".into(), receipt),
            ("pin.json".into(), pin_file),
            ("raw-manifest.json".into(), raw_file),
            ("build-controls.json".into(), control_file),
        ];
        held.extend(raw_assets.iter().map(|(_, a)| a.clone()));
        held.extend(generation.iter().map(|(_, a)| a.clone()));
        held.push(current.clone());
        held.extend(seals.iter().map(|(_, a)| a.clone()));
        if held.len() > MAX_FILES {
            return Err("production_fd_limit".into());
        }
        groups.push(Group {
            binding,
            reader,
            raw: raw_assets,
            generation,
            current,
            seals,
        });
    }
    let c = AdmittedCorpus {
        digest: hash(&bytes),
        groups,
        tool: corpus.tool,
        tool_file,
        held,
    };
    c.fresh()?;
    Ok(c)
}
impl AdmittedCorpus {
    fn fresh(&self) -> Result<()> {
        for f in &self.held {
            f.fresh()?;
        }
        for g in &self.groups {
            g.reader.verify_all().map_err(err)?;
            if scan(&g.binding.canonical_root, true)?
                != g.binding
                    .raw_documents
                    .iter()
                    .map(|r| r.path.clone())
                    .collect()
                || scan(
                    &g.binding
                        .canonical_root
                        .join(".gptgrep/generations")
                        .join(&g.binding.pin.generation),
                    false,
                )? != g
                    .binding
                    .generation_inventory
                    .iter()
                    .map(|i| i.path.clone())
                    .collect()
            {
                return Err("production_source_set_changed".into());
            }
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    id: String,
    question: String,
    question_sha256: String,
    group_scope: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Readers {
    schema_version: String,
    corpus_sha256: String,
    tasks: Vec<TaskInput>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    schema_version: String,
    reader_image: String,
    verifier_image: String,
    codex_binary_sha256: String,
    codex_argv: Vec<String>,
    model: String,
    effort: String,
    service_tier: String,
    timeout_sec: u32,
    cpus: u32,
    memory_mb: u32,
    network_mode: String,
    #[serde(
        default,
        deserialize_with = "deserialize_hosts",
        skip_serializing_if = "Option::is_none"
    )]
    allowed_hosts: Option<Vec<String>>,
    repetitions: u32,
    seed: u32,
    reader_uid: u32,
    reader_gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_debian_packages: Option<External>,
}

const MAX_RUNTIME_PACKAGES: usize = 32;
const MAX_RUNTIME_DEB: u64 = 16 * 1024 * 1024;
const MAX_RUNTIME_LICENSE: u64 = 256 * 1024;
const MAX_RUNTIME_BYTES: u64 = 32 * 1024 * 1024;
const RUNTIME_DOCKER: &str = "COPY runtime-debian/ /opt/gptgrep-runtime-debian/\nRUN cd /opt/gptgrep-runtime-debian && sha256sum --check checksums.sha256 && dpkg -i debs/*.deb\n";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAsset {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimePackage {
    deb: RuntimeAsset,
    license: RuntimeAsset,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeInputs {
    schema_version: String,
    packages: Vec<RuntimePackage>,
}
// Metadata validation completes before any package descriptor or buffer is opened.
fn runtime_assets(bytes: &[u8]) -> Result<BTreeMap<String, RuntimeAsset>> {
    let input: RuntimeInputs = strict(bytes)?;
    if input.schema_version != "gptgrep.harbor.runtime-debian.v1"
        || input.packages.is_empty()
        || input.packages.len() > MAX_RUNTIME_PACKAGES
    {
        return Err("production_runtime_package_count".into());
    }
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    let mut deb_paths = BTreeSet::new();
    for (i, package) in input.packages.into_iter().enumerate() {
        if !deb_paths.insert(package.deb.path.clone()) {
            return Err("production_runtime_duplicate_deb".into());
        }
        for (kind, asset, cap) in [
            ("debs", package.deb, MAX_RUNTIME_DEB),
            ("licenses", package.license, MAX_RUNTIME_LICENSE),
        ] {
            check_hash(&asset.sha256)?;
            let name = asset
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("production_runtime_filename")?;
            if !asset.path.is_absolute()
                || asset.path.components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                })
                || name.len() > 160
                || !name
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._%+:-".contains(&b))
                || (kind == "debs" && !name.ends_with(".deb"))
            {
                return Err("production_runtime_filename".into());
            }
            if asset.bytes == 0 || asset.bytes > cap {
                return Err("production_runtime_asset_size".into());
            }
            total = total
                .checked_add(asset.bytes)
                .filter(|n| *n <= MAX_RUNTIME_BYTES)
                .ok_or("production_runtime_total_size")?;
            // Original Debian filenames may contain %, + or :. Only generated
            // alphanumeric names enter Docker destinations and checksum lines.
            let extension = if kind == "debs" { "deb" } else { "txt" };
            files.insert(format!("{kind}/package-{i:04}.{extension}"), asset);
        }
    }
    Ok(files)
}
fn runtime_metadata(
    input_sha: &str,
    assets: &BTreeMap<String, RuntimeAsset>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let records = assets
        .iter()
        .map(|(path, a)| {
            value!({"path":path,"source_filename":a.path.file_name().and_then(|n|n.to_str()),"bytes":a.bytes,"sha256":a.sha256})
        })
        .collect::<Vec<_>>();
    let checksums = assets
        .iter()
        .map(|(name, a)| format!("{}  {name}\n", a.sha256))
        .collect::<String>()
        .into_bytes();
    Ok((
        json(
            &value!({"schema_version":"gptgrep.harbor.runtime-debian.frozen.v1","input_sha256":input_sha,"files":records}),
        )?,
        checksums,
    ))
}
struct AdmittedRuntime {
    input: Arc<Held>,
    files: BTreeMap<String, Arc<Held>>,
    metadata: Vec<u8>,
    checksums: Vec<u8>,
}
fn admit_runtime(input: &External) -> Result<AdmittedRuntime> {
    check_hash(&input.sha256)?;
    if !input.path.is_absolute() {
        return Err("production_asset_absolute".into());
    }
    // The JSON ceiling is checked before hashing or deserializing the manifest.
    let manifest = Held::open_limit(&input.path, Some(&input.sha256), MAX_JSON)?;
    let assets = runtime_assets(&manifest.bytes()?)?;
    let (metadata, checksums) = runtime_metadata(&manifest.sha, &assets)?;
    let mut files = BTreeMap::new();
    for (name, asset) in assets {
        let limit = if name.starts_with("debs/") {
            MAX_RUNTIME_DEB
        } else {
            MAX_RUNTIME_LICENSE
        };
        let held = Held::open_limit(&asset.path, Some(&asset.sha256), limit)?;
        if held.bytes != asset.bytes {
            return Err("production_runtime_size_binding".into());
        }
        if name.starts_with("debs/") {
            let mut file = held.file.try_clone().map_err(err)?;
            file.seek(SeekFrom::Start(0)).map_err(err)?;
            let mut magic = [0u8; 8];
            file.read_exact(&mut magic).map_err(err)?;
            if &magic != b"!<arch>\n" {
                return Err("production_runtime_deb_header".into());
            }
        }
        files.insert(name, held);
    }
    Ok(AdmittedRuntime {
        input: manifest,
        files,
        metadata,
        checksums,
    })
}
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct TaskBinding {
    id: String,
    question_sha256: String,
    group_scope: Vec<String>,
    raw_sha256: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    schema_version: String,
    role: String,
    corpus_sha256: String,
    reader_projection_sha256: String,
    reader_export_sha256: Option<String>,
    options_sha256: String,
    source_mode: String,
    executable: bool,
    qualification: Value,
    group_bindings: Vec<GroupBinding>,
    task_bindings: Vec<TaskBinding>,
    files: BTreeMap<String, String>,
}
fn qualification() -> Value {
    value!({"artifact_scope":"native_v2_source_structure_only","historical_builder":"unattested","runtime":"HOLD","complete_multimodal_helper_treatment":"HOLD","physical_isolation":"HOLD","judge":"unavailable"})
}
enum Payload {
    Bytes(Vec<u8>),
    File(Arc<Held>),
}
impl Payload {
    fn size(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::File(f) => f.bytes,
        }
    }
    fn digest(&self) -> String {
        match self {
            Self::Bytes(b) => hash(b),
            Self::File(f) => f.sha.clone(),
        }
    }
}
pub(super) struct Plan {
    manifest: Manifest,
    files: BTreeMap<String, Payload>,
    corpus: Option<AdmittedCorpus>,
    held: Vec<Arc<Held>>,
}
impl Plan {
    fn put(&mut self, name: String, payload: Payload) -> Result<()> {
        check_rel(&name)?;
        if payload.size() > MAX_FILE || self.files.insert(name, payload).is_some() {
            return Err("production_output_collision_or_file_limit".into());
        }
        if self.files.len() > MAX_FILES
            || self
                .files
                .values()
                .try_fold(0u64, |n, p| n.checked_add(p.size()))
                .ok_or("production_size_overflow")?
                > MAX_OUTPUT as u64
        {
            return Err("production_output_budget".into());
        }
        Ok(())
    }
    fn fresh(&self) -> Result<()> {
        if let Some(c) = &self.corpus {
            c.fresh()?;
        }
        for f in &self.held {
            f.fresh()?;
        }
        Ok(())
    }
    pub(super) fn publish(mut self, path: &Path) -> Result<()> {
        self.manifest.files = self
            .files
            .iter()
            .map(|(n, p)| (n.clone(), p.digest()))
            .collect();
        let complete = json(&self.manifest)?;
        if complete.len() as u64 > MAX_JSON
            || self.files.values().map(Payload::size).sum::<u64>() + complete.len() as u64
                > MAX_OUTPUT as u64
        {
            return Err("production_completion_budget".into());
        }
        self.fresh()?;
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(err)?.join(path)
        };
        let parent = absolute.parent().ok_or("production_output_parent")?;
        canonical_root(parent)?;
        let source_paths = self
            .held
            .iter()
            .chain(self.corpus.iter().flat_map(|c| c.held.iter()))
            .map(|h| &h.path);
        if source_paths.into_iter().any(|p| p.starts_with(&absolute))
            || self.corpus.as_ref().is_some_and(|c| {
                c.groups
                    .iter()
                    .any(|g| absolute.starts_with(&g.binding.canonical_root))
            })
        {
            return Err("production_output_input_overlap".into());
        }
        let writer = PrivateOutput::new(&absolute)?;
        for (name, payload) in &self.files {
            let mut f = writer.file(name)?;
            match payload {
                Payload::Bytes(b) => f.write_all(b).map_err(err)?,
                Payload::File(h) => {
                    if h.measure(Some(&mut f))? != h.sha {
                        return Err("production_copy_digest".into());
                    }
                }
            }
            f.sync_all().map_err(err)?;
        }
        self.fresh()?;
        let found = scan(&absolute, false)?;
        if found != self.files.keys().cloned().collect() {
            return Err("production_output_set".into());
        }
        for (name, payload) in &self.files {
            Held::open(&absolute.join(name), Some(&payload.digest()))?;
        }
        let mut done = writer.file("export-manifest.json")?;
        done.write_all(&complete).map_err(err)?;
        done.sync_all().map_err(err)?;
        writer.root.sync_all().map_err(err)?;
        Ok(())
    }
}
fn directory_at(parent: &File, name: &str, create: bool, fresh: bool) -> Result<File> {
    let name = std::ffi::CString::new(name).map_err(err)?;
    let mut created = false;
    if create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if result == 0 {
            created = true;
        } else if fresh || std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            return Err("production_output_must_be_fresh".into());
        }
    }
    if created
        && unsafe {
            libc::fchmodat(
                parent.as_raw_fd(),
                name.as_ptr(),
                0o700,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
    {
        return Err("production_private_directory_mode".into());
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("production_directory_nofollow".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if created {
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(err)?;
    }
    Ok(file)
}
struct PrivateOutput {
    root: File,
}
impl PrivateOutput {
    fn new(path: &Path) -> Result<Self> {
        let mut dir = File::open("/").map_err(err)?;
        let parts = path
            .components()
            .filter_map(|p| {
                if let std::path::Component::Normal(n) = p {
                    Some(n)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for (i, p) in parts.iter().enumerate() {
            dir = directory_at(
                &dir,
                p.to_str().ok_or("production_utf8")?,
                i + 1 == parts.len(),
                i + 1 == parts.len(),
            )?;
        }
        Ok(Self { root: dir })
    }
    fn file(&self, name: &str) -> Result<File> {
        check_rel(name)?;
        let mut dir = self.root.try_clone().map_err(err)?;
        let parts = name.split('/').collect::<Vec<_>>();
        for p in &parts[..parts.len() - 1] {
            dir = directory_at(&dir, p, true, false)?;
        }
        let leaf =
            std::ffi::CString::new(*parts.last().ok_or("production_output_leaf")?).map_err(err)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err("production_output_file_exists".into());
        }
        let f = unsafe { File::from_raw_fd(fd) };
        f.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(err)?;
        Ok(f)
    }
}
fn options(bytes: &[u8]) -> Result<Options> {
    let o: Options = strict(bytes)?;
    if o.schema_version != "gptgrep.harbor.production-options.v1"
        || o.model != "gpt-6-luna"
        || o.effort != "max"
        || o.service_tier != "fast"
        || !(1..=3600).contains(&o.timeout_sec)
        || !(1..=16).contains(&o.cpus)
        || !(256..=16384).contains(&o.memory_mb)
        || !(1..=10).contains(&o.repetitions)
        || o.reader_uid == 0
        || o.reader_gid == 0
        || o.codex_argv.len() > 32
        || o.codex_argv.first().map(String::as_str) != Some("codex")
        || o.codex_argv.get(1).map(String::as_str) != Some("exec")
        || o.codex_argv.iter().any(|s| s.len() > 1024)
    {
        return Err("production_options".into());
    }
    network_policy(&o.network_mode, o.allowed_hosts.as_deref())?;
    check_hash(&o.codex_binary_sha256)?;
    for image in [&o.reader_image, &o.verifier_image] {
        check_hash(
            image
                .rsplit_once("@sha256:")
                .ok_or("production_image_digest")?
                .1,
        )?;
        if !image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/.:_-@".contains(&b))
        {
            return Err("production_image_injection".into());
        }
    }
    Ok(o)
}
fn network_policy(mode: &str, hosts: Option<&[String]>) -> Result<()> {
    let hosts = hosts.unwrap_or_default();
    if !matches!(mode, "no-network" | "allowlist")
        || (mode == "no-network" && !hosts.is_empty())
        || (mode == "allowlist" && hosts.is_empty())
        || hosts.len() > 32
    {
        return Err("production_network_policy".into());
    }
    let mut seen = BTreeSet::new();
    for host in hosts {
        if !exact_network_host(host) || !seen.insert(host) {
            return Err("production_network_host".into());
        }
    }
    Ok(())
}
fn deserialize_hosts<'de, D>(deserializer: D) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}
fn exact_network_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || !host.is_ascii() || host.contains('%') {
        return false;
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return address.to_string() == host;
    }
    // Numeric final labels can be interpreted as legacy IPv4 aliases by consumers.
    // Refuse that fallback instead of treating an invalid IP spelling as DNS.
    let final_label = host.rsplit('.').next().unwrap_or_default();
    if final_label.bytes().all(|b| b.is_ascii_digit())
        || final_label
            .strip_prefix("0x")
            .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

#[cfg(test)]
mod network_policy_tests {
    use super::*;
    fn hosts(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    fn fixture() -> Value {
        value!({"schema_version":"gptgrep.harbor.production-options.v1", "reader_image":format!("example/reader@sha256:{}","a".repeat(64)), "verifier_image":format!("example/verifier@sha256:{}","b".repeat(64)), "codex_binary_sha256":"c".repeat(64), "codex_argv":["codex","exec","-"], "model":"gpt-6-luna", "effort":"max", "service_tier":"fast", "timeout_sec":60, "cpus":1, "memory_mb":512, "network_mode":"no-network", "repetitions":1, "seed":0, "reader_uid":1000, "reader_gid":1000})
    }
    #[test]
    fn legacy_default_omits_allowlist_and_explicit_empty_stays_offline() {
        let mut input = fixture();
        let admitted = options(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert!(
            serde_json::to_value(admitted)
                .unwrap()
                .get("allowed_hosts")
                .is_none()
        );
        input["allowed_hosts"] = value!([]);
        let admitted = options(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(admitted).unwrap()["allowed_hosts"],
            value!([])
        );
    }
    #[test]
    fn explicit_bounded_exact_hosts_are_admitted_and_retained() {
        let mut input = fixture();
        input["network_mode"] = value!("allowlist");
        input["allowed_hosts"] = value!([
            "api.openai.com",
            "openrouter.ai",
            "192.0.2.1",
            "2001:db8::1"
        ]);
        let admitted = options(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(admitted).unwrap()["allowed_hosts"],
            input["allowed_hosts"]
        );
        let bounded: Vec<_> = (0..32)
            .map(|index| format!("host-{index}.example.com"))
            .collect();
        network_policy("allowlist", Some(&bounded)).unwrap();
    }
    #[test]
    fn cross_policy_empty_missing_and_duplicate_hosts_are_refused() {
        assert!(network_policy("allowlist", None).is_err());
        assert!(network_policy("allowlist", Some(&[])).is_err());
        assert!(network_policy("public", None).is_err());
        assert!(network_policy("no-network", Some(&hosts(&["openrouter.ai"]))).is_err());
        assert!(
            network_policy(
                "allowlist",
                Some(&hosts(&["openrouter.ai", "openrouter.ai"]))
            )
            .is_err()
        );
        let too_many: Vec<_> = (0..33)
            .map(|index| format!("host-{index}.example.com"))
            .collect();
        assert!(network_policy("allowlist", Some(&too_many)).is_err());
        let mut input = fixture();
        input["allowed_hosts"] = Value::Null;
        assert!(options(&serde_json::to_vec(&input).unwrap()).is_err());
    }
    #[test]
    fn numeric_aliases_and_scoped_ipv6_are_refused_without_blocking_canonical_hosts() {
        for host in [
            "192.0.2.01",
            "127.1",
            "127.0.1",
            "2130706433",
            "127",
            "0177.0.0.1",
            "0x7f000001",
            "0x7f.0.0.1",
            "0x7f.0x1",
            "0x",
            "256.1.1.1",
            "4294967296",
            "example.123",
            "example.0x10",
            "fe80::1%eth0",
            "fe80::1%25en0",
            "2001:db8::1%2",
            "::ffff:192.0.2.01",
        ] {
            assert!(!exact_network_host(host), "{host}");
            assert!(
                network_policy("allowlist", Some(&hosts(&[host]))).is_err(),
                "{host}"
            );
            let mut input = fixture();
            input["network_mode"] = value!("allowlist");
            input["allowed_hosts"] = value!([host]);
            assert!(
                options(&serde_json::to_vec(&input).unwrap()).is_err(),
                "{host}"
            );
        }
        for host in [
            "192.0.2.1",
            "2001:db8::1",
            "::1",
            "api2.example.com",
            "123.api.example.com",
            "127.example.com",
            "0xrouter.example.com",
            "localhost",
        ] {
            assert!(exact_network_host(host), "{host}");
            let mut input = fixture();
            input["network_mode"] = value!("allowlist");
            input["allowed_hosts"] = value!([host]);
            let admitted = options(&serde_json::to_vec(&input).unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(admitted).unwrap()["allowed_hosts"],
                value!([host])
            );
        }
    }
    #[test]
    fn urls_credentials_wildcards_cidr_ports_and_noncanonical_hosts_are_refused() {
        for host in [
            "",
            "*.example.com",
            "api.*.example.com",
            "https://api.openai.com",
            "user:password@api.openai.com",
            "192.0.2.0/24",
            "2001:db8::/32",
            "api.openai.com:443",
            "api.openai.com/path",
            "API.OPENAI.COM",
            "api.openai.com.",
            " api.openai.com",
            "api.openai.com\n",
            "-bad.example.com",
            "bad-.example.com",
            "a..example.com",
            "bad_host.example.com",
            "é.example.com",
            "2001:0db8::1",
        ] {
            assert!(
                network_policy("allowlist", Some(&hosts(&[host]))).is_err(),
                "{host}"
            );
        }
        assert!(!exact_network_host(&format!(
            "{}.example.com",
            "a".repeat(64)
        )));
        assert!(!exact_network_host(&format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(63)
        )));
    }
}
pub(super) fn reader_instruction(question: &str, arm: &str) -> Vec<u8> {
    let mut instruction = format!(
        "{question}\n\nRead the raw PDF groups named in /app/reader-input.json. Write only {{\"answer\":\"...\"}} to /app/answer.json.\n"
    );
    if arm == "gptgrep" {
        instruction.push_str(
            "\nGPTgrep retrieval is available at /app/tool/gptgrep. Discover its command and request contracts with /app/tool/gptgrep --schema and /app/tool/gptgrep --llms. For each relevant group named in /app/reader-input.json, read its canonical_root and exact generation pin from /app/prebuild-seals/GROUP/receipt.json and /app/prebuild-seals/GROUP/pin.json. Use the pinned rich batch interface to retrieve document evidence: save a bounded JSON request to a file and invoke /app/tool/gptgrep batch --rich --root ROOT --input FILE as a direct command. Use inspect_documents and inspect_pages to discover artifact identifiers, then search_artifacts and read_artifact for relevant native text. Preserve the exact generation pin and continuation cursors, respect the documented operation/output limits, and inspect coverage and errors. The tool and prebuilt generation are local retrieval resources; they do not supply a verified answer or an interactive model helper. Native PDF tools remain available.\n",
        );
    }
    instruction.into_bytes()
}

fn reader_dockerfile(
    o: &Options,
    groups: &[GroupBinding],
    scope: &[String],
    arm: &str,
) -> Result<Vec<u8>> {
    let mut docker = format!(
        "FROM {}\nWORKDIR /app\nCOPY --chown={}:{} reader-input.json codex-config.json /app/\n",
        o.reader_image, o.reader_uid, o.reader_gid
    );
    if o.runtime_debian_packages.is_some() {
        docker.push_str(RUNTIME_DOCKER);
    }
    for id in scope {
        let group = groups
            .iter()
            .find(|g| &g.id == id)
            .ok_or("production_scope")?;
        if arm == "gptgrep" {
            docker.push_str(&format!(
                "COPY --chown={}:{} groups/{id}/seal/ /app/prebuild-seals/{id}/\n",
                o.reader_uid, o.reader_gid
            ));
        }
        docker.push_str(&format!(
            "COPY --chown={}:{} groups/{id}/raw/ {}/\n",
            o.reader_uid,
            o.reader_gid,
            group.runtime_root.display()
        ));
    }
    if arm == "gptgrep" {
        docker.push_str("COPY tool/ /app/tool/\nRUN chmod 0555 /app/tool/gptgrep\n");
    }
    docker.push_str(&format!("USER {}:{}\n", o.reader_uid, o.reader_gid));
    Ok(docker.into_bytes())
}
pub(super) fn prepare(
    corpus_path: &Path,
    reader_path: &Path,
    option_path: &Path,
    export: bool,
) -> Result<Plan> {
    // Admission order is deliberate: no reader question is loaded during corpus verification.
    let corpus = admit(corpus_path)?;
    let readers_file = Held::open(reader_path, None)?;
    let readers: Readers = strict(&readers_file.bytes()?)?;
    let option_file = Held::open(option_path, None)?;
    let option_bytes = option_file.bytes()?;
    let o = options(&option_bytes)?;
    let runtime = o
        .runtime_debian_packages
        .as_ref()
        .map(admit_runtime)
        .transpose()?;
    if readers.schema_version != "gptgrep.harbor.production-readers.v1"
        || readers.corpus_sha256 != corpus.digest
        || readers.tasks.is_empty()
        || readers.tasks.len() > 64
    {
        return Err("production_reader_schema_or_binding".into());
    }
    let bindings = corpus.groups.iter().map(|g| g.binding.clone()).collect();
    let mut plan = Plan {
        manifest: Manifest {
            schema_version: EXPORT_SCHEMA.into(),
            role: if export { "reader" } else { "plan" }.into(),
            corpus_sha256: corpus.digest.clone(),
            reader_projection_sha256: readers_file.sha.clone(),
            reader_export_sha256: None,
            options_sha256: option_file.sha.clone(),
            source_mode: "raw_only_native_v2_structure".into(),
            executable: false,
            qualification: qualification(),
            group_bindings: bindings,
            task_bindings: Vec::new(),
            files: BTreeMap::new(),
        },
        files: BTreeMap::new(),
        corpus: None,
        held: vec![readers_file, option_file],
    };
    plan.put(
        "frozen-options.json".into(),
        Payload::Bytes(option_bytes.clone()),
    )?;
    if let Some(runtime) = &runtime {
        plan.put(
            "frozen-runtime-debian-inputs.json".into(),
            Payload::File(runtime.input.clone()),
        )?;
        plan.held.push(runtime.input.clone());
        plan.held.extend(runtime.files.values().cloned());
    }
    let mut seen = BTreeSet::new();
    let mut schedule = Vec::new();
    for (ordinal, t) in readers.tasks.into_iter().enumerate() {
        check_id(&t.id)?;
        if !seen.insert(t.id.clone())
            || t.question.is_empty()
            || t.question.len() > 12000
            || hash(t.question.as_bytes()) != t.question_sha256
            || t.group_scope.is_empty()
        {
            return Err("production_question_binding".into());
        }
        let mut scope = BTreeSet::new();
        let mut raw = BTreeMap::new();
        for id in &t.group_scope {
            if !scope.insert(id) {
                return Err("production_duplicate_scope".into());
            }
            let g = corpus
                .groups
                .iter()
                .find(|g| &g.binding.id == id)
                .ok_or("production_unknown_whole_group")?;
            for d in &g.binding.raw_documents {
                raw.insert(format!("{id}/{}", d.path), d.sha256.clone());
            }
        }
        plan.manifest.task_bindings.push(TaskBinding {
            id: t.id.clone(),
            question_sha256: t.question_sha256.clone(),
            group_scope: t.group_scope.clone(),
            raw_sha256: raw,
        });
        for repeat in 0..o.repetitions {
            let arms =
                if ((ordinal as u32).wrapping_add(repeat).wrapping_add(o.seed)).is_multiple_of(2) {
                    ["control", "gptgrep"]
                } else {
                    ["gptgrep", "control"]
                };
            for (position, arm) in arms.iter().enumerate() {
                schedule.push(value!({"task_id":t.id,"repeat":repeat,"arm":arm,"within_pair_order":position,"executed":false}));
            }
        }
        if !export {
            continue;
        }
        for arm in ["control", "gptgrep"] {
            let prefix = format!("{arm}/{}", t.id);
            plan.put(
                format!("{prefix}/instruction.md"),
                Payload::Bytes(reader_instruction(&t.question, arm)),
            )?;
            plan.put(format!("{prefix}/environment/reader-input.json"),Payload::Bytes(json(&value!({"schema_version":"gptgrep.harbor.production-reader-input.v1","task_id":t.id,"question":t.question,"question_sha256":t.question_sha256,"group_scope":t.group_scope,"arm":arm}))?))?;
            plan.put(
                format!("{prefix}/environment/codex-config.json"),
                Payload::Bytes(option_bytes.clone()),
            )?;
            if let Some(runtime) = &runtime {
                for (name, file) in &runtime.files {
                    plan.put(
                        format!("{prefix}/environment/runtime-debian/{name}"),
                        Payload::File(file.clone()),
                    )?;
                }
                for (name, bytes) in [
                    ("manifest.json", &runtime.metadata),
                    ("checksums.sha256", &runtime.checksums),
                ] {
                    plan.put(
                        format!("{prefix}/environment/runtime-debian/{name}"),
                        Payload::Bytes(bytes.clone()),
                    )?;
                }
            }
            for id in &t.group_scope {
                let g = corpus
                    .groups
                    .iter()
                    .find(|g| &g.binding.id == id)
                    .ok_or("production_scope")?;
                let group = format!("{prefix}/environment/groups/{id}");
                for (name, f) in &g.raw {
                    plan.put(format!("{group}/raw/{name}"), Payload::File(f.clone()))?;
                }
                if arm == "gptgrep" {
                    plan.put(
                        format!("{group}/raw/.gptgrep/CURRENT.json"),
                        Payload::File(g.current.clone()),
                    )?;
                    for (name, f) in &g.generation {
                        plan.put(
                            format!(
                                "{group}/raw/.gptgrep/generations/{}/{name}",
                                g.binding.pin.generation
                            ),
                            Payload::File(f.clone()),
                        )?;
                    }
                    for (name, f) in &g.seals {
                        plan.put(format!("{group}/seal/{name}"), Payload::File(f.clone()))?;
                    }
                }
            }
            if arm == "gptgrep" {
                plan.put(
                    format!("{prefix}/environment/tool/gptgrep"),
                    Payload::File(corpus.tool_file.clone()),
                )?;
                plan.put(format!("{prefix}/environment/tool/provenance.json"),Payload::Bytes(json(&value!({"declared_artifact":corpus.tool,"qualification":"HOLD_metadata_not_runtime_or_helper_capability"}))?))?;
            }
            plan.put(
                format!("{prefix}/environment/Dockerfile"),
                Payload::Bytes(reader_dockerfile(
                    &o,
                    &plan.manifest.group_bindings,
                    &t.group_scope,
                    arm,
                )?),
            )?;
            plan.put(
                format!("{prefix}/task-descriptor.json"),
                Payload::Bytes(json(&value!({"task_id":t.id,"arm":arm,"options":o}))?),
            )?;
        }
    }
    plan.put(
        "paired-schedule.json".into(),
        Payload::Bytes(json(&schedule)?),
    )?;
    plan.corpus = Some(corpus);
    plan.fresh()?;
    Ok(plan)
}
pub(super) fn validate(root: &Path) -> Result<(Manifest, String)> {
    let bytes = super::regular_bytes(&root.join("export-manifest.json"), MAX_JSON)?;
    let m: Manifest = strict(&bytes)?;
    if m.schema_version != EXPORT_SCHEMA
        || m.executable
        || m.source_mode != "raw_only_native_v2_structure"
        || m.qualification != qualification()
        || !["reader", "grader", "plan"].contains(&m.role.as_str())
    {
        return Err("production_export_status".into());
    }
    match (m.role.as_str(), &m.reader_export_sha256) {
        ("grader", Some(s)) => check_hash(s)?,
        ("reader" | "plan", None) => {}
        _ => return Err("production_role_binding".into()),
    }
    let mut found = BTreeMap::new();
    super::inventory(root, "", &mut found, &mut 0, &mut 0)?;
    if found != m.files {
        return Err("production_export_inventory".into());
    }
    if m.role == "reader" {
        let mut all_allowed = BTreeSet::from([
            "frozen-options.json".to_string(),
            "paired-schedule.json".to_string(),
        ]);
        let option_bytes = super::regular_bytes(&root.join("frozen-options.json"), MAX_JSON)?;
        let o = options(&option_bytes)?;
        if hash(&option_bytes) != m.options_sha256 {
            return Err("production_options_binding".into());
        }
        let runtime = if let Some(input) = &o.runtime_debian_packages {
            check_hash(&input.sha256)?;
            let name = "frozen-runtime-debian-inputs.json";
            let bytes = super::regular_bytes(&root.join(name), MAX_JSON)?;
            if hash(&bytes) != input.sha256 {
                return Err("production_runtime_manifest_binding".into());
            }
            all_allowed.insert(name.into());
            let assets = runtime_assets(&bytes)?;
            let (metadata, checksums) = runtime_metadata(&input.sha256, &assets)?;
            Some((assets, metadata, checksums))
        } else {
            None
        };
        for t in &m.task_bindings {
            for arm in ["control", "gptgrep"] {
                let prefix = format!("{arm}/{}", t.id);
                if super::regular_bytes(
                    &root.join(&prefix).join("environment/Dockerfile"),
                    MAX_JSON,
                )? != reader_dockerfile(&o, &m.group_bindings, &t.group_scope, arm)?
                {
                    return Err("production_reader_docker_binding".into());
                }
                if super::regular_bytes(
                    &root.join(&prefix).join("environment/codex-config.json"),
                    MAX_JSON,
                )? != option_bytes
                {
                    return Err("production_config_difference".into());
                }
                let input: Value = strict(&super::regular_bytes(
                    &root.join(&prefix).join("environment/reader-input.json"),
                    MAX_JSON,
                )?)?;
                if input["task_id"] != t.id
                    || input["arm"] != arm
                    || input["group_scope"] != value!(t.group_scope)
                    || input["question_sha256"] != t.question_sha256
                    || hash(
                        input["question"]
                            .as_str()
                            .ok_or("production_question")?
                            .as_bytes(),
                    ) != t.question_sha256
                {
                    return Err("production_input_binding".into());
                }
                if super::regular_bytes(&root.join(&prefix).join("instruction.md"), MAX_JSON)?
                    != reader_instruction(
                        input["question"].as_str().ok_or("production_question")?,
                        arm,
                    )
                {
                    return Err("production_instruction_binding".into());
                }
                let mut expected_raw = BTreeMap::new();
                for id in &t.group_scope {
                    let g = m
                        .group_bindings
                        .iter()
                        .find(|g| &g.id == id)
                        .ok_or("production_group_binding")?;
                    if g.canonical_root != g.runtime_root {
                        return Err("production_relocated_binding".into());
                    }
                    for raw in &g.raw_documents {
                        expected_raw.insert(format!("{id}/{}", raw.path), raw.sha256.clone());
                        let name = format!("{prefix}/environment/groups/{id}/raw/{}", raw.path);
                        if m.files.get(&name) != Some(&raw.sha256) {
                            return Err("production_mounted_raw".into());
                        }
                    }
                    if arm == "gptgrep" {
                        let group = format!("{prefix}/environment/groups/{id}");
                        if m.files.get(&format!("{group}/raw/.gptgrep/CURRENT.json"))
                            != Some(&g.current_snapshot_sha256)
                        {
                            return Err("production_current_export".into());
                        }
                        for item in &g.generation_inventory {
                            if m.files.get(&format!(
                                "{group}/raw/.gptgrep/generations/{}/{}",
                                g.pin.generation, item.path
                            )) != Some(&item.sha256)
                            {
                                return Err("production_generation_export".into());
                            }
                        }
                    }
                }
                if expected_raw != t.raw_sha256 {
                    return Err("production_task_raw_set".into());
                }
                let marker = format!("{prefix}/environment/groups/");
                let allowed = t
                    .group_scope
                    .iter()
                    .flat_map(|id| {
                        let g = m
                            .group_bindings
                            .iter()
                            .find(|g| &g.id == id)
                            .expect("scope checked");
                        let mut names = g
                            .raw_documents
                            .iter()
                            .map(|r| format!("{marker}{id}/raw/{}", r.path))
                            .collect::<BTreeSet<_>>();
                        if arm == "gptgrep" {
                            names.insert(format!("{marker}{id}/raw/.gptgrep/CURRENT.json"));
                            for i in &g.generation_inventory {
                                names.insert(format!(
                                    "{marker}{id}/raw/.gptgrep/generations/{}/{}",
                                    g.pin.generation, i.path
                                ));
                            }
                            for n in [
                                "receipt.json",
                                "pin.json",
                                "raw-manifest.json",
                                "build-controls.json",
                            ] {
                                names.insert(format!("{marker}{id}/seal/{n}"));
                            }
                        }
                        names
                    })
                    .collect::<BTreeSet<_>>();
                if m.files
                    .keys()
                    .filter(|n| n.starts_with(&marker))
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    != allowed
                {
                    return Err("production_group_output_set".into());
                }
                all_allowed.extend(allowed);
                if let Some((assets, metadata, checksums)) = &runtime {
                    let runtime_prefix = format!("{prefix}/environment/runtime-debian");
                    for (name, asset) in assets {
                        let output = format!("{runtime_prefix}/{name}");
                        if m.files.get(&output) != Some(&asset.sha256)
                            || regular_file(&root.join(&output), asset.bytes)?
                                .metadata()
                                .map_err(err)?
                                .len()
                                != asset.bytes
                        {
                            return Err("production_runtime_asset_binding".into());
                        }
                        all_allowed.insert(output);
                    }
                    for (name, expected) in
                        [("manifest.json", metadata), ("checksums.sha256", checksums)]
                    {
                        let output = format!("{runtime_prefix}/{name}");
                        if super::regular_bytes(&root.join(&output), MAX_JSON)? != *expected {
                            return Err("production_runtime_mapping_binding".into());
                        }
                        all_allowed.insert(output);
                    }
                }
                for name in [
                    "instruction.md",
                    "task-descriptor.json",
                    "environment/Dockerfile",
                    "environment/reader-input.json",
                    "environment/codex-config.json",
                ] {
                    all_allowed.insert(format!("{prefix}/{name}"));
                }
                if arm == "gptgrep" {
                    all_allowed.insert(format!("{prefix}/environment/tool/gptgrep"));
                    all_allowed.insert(format!("{prefix}/environment/tool/provenance.json"));
                }
                if arm == "control"
                    && m.files
                        .keys()
                        .any(|n| n.starts_with(&format!("{prefix}/environment/tool/")))
                {
                    return Err("production_baseline_tool".into());
                }
            }
        }
        if m.files.keys().cloned().collect::<BTreeSet<_>>() != all_allowed {
            return Err("production_reader_namespace".into());
        }
    }
    Ok((m, hash(&bytes)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grade {
    id: String,
    question_sha256: String,
    reference_asset: External,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Graders {
    schema_version: String,
    corpus_sha256: String,
    reader_projection_sha256: String,
    reader_export_sha256: String,
    rubric_sha256: String,
    judge_status: String,
    tasks: Vec<Grade>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    question: String,
    answer: Value,
    answer_format: String,
}
pub(super) fn verifier(graders: &Path, reader: &Path) -> Result<Plan> {
    let (mut manifest, reader_sha) = validate(reader)?;
    if manifest.role != "reader" {
        return Err("production_reader_role".into());
    }
    let source = Held::open(graders, None)?;
    let g: Graders = strict(&source.bytes()?)?;
    check_hash(&g.rubric_sha256)?;
    if g.schema_version != "gptgrep.harbor.production-verifiers.v1"
        || g.corpus_sha256 != manifest.corpus_sha256
        || g.reader_projection_sha256 != manifest.reader_projection_sha256
        || g.reader_export_sha256 != reader_sha
        || g.judge_status != "unavailable"
        || g.tasks.len() != manifest.task_bindings.len()
    {
        return Err("production_verifier_binding".into());
    }
    let reader_manifest = Held::open(&reader.join("export-manifest.json"), Some(&reader_sha))?;
    let mut plan = Plan {
        manifest: Manifest {
            files: BTreeMap::new(),
            ..manifest
        },
        files: BTreeMap::new(),
        corpus: None,
        held: vec![source, reader_manifest],
    };
    let mut seen = BTreeSet::new();
    for task in g.tasks {
        let b = plan
            .manifest
            .task_bindings
            .iter()
            .find(|b| b.id == task.id)
            .ok_or("production_grade_id")?;
        if !seen.insert(task.id.clone()) || b.question_sha256 != task.question_sha256 {
            return Err("production_grade_question".into());
        }
        let reference = task.reference_asset.admit()?;
        if reference.bytes > 65536 {
            return Err("production_reference_limit".into());
        }
        let r: Reference = strict(&reference.bytes()?)?;
        if r.answer.is_null()
            || r.question.is_empty()
            || r.question.len() > 12000
            || hash(r.question.as_bytes()) != task.question_sha256
            || r.answer_format.is_empty()
            || r.answer_format.len() > 4096
        {
            return Err("production_reference_contract".into());
        }
        plan.held.push(reference.clone());
        plan.put(
            format!("{}/reference.json", task.id),
            Payload::File(reference),
        )?;
    }
    manifest = plan.manifest;
    manifest.role = "grader".into();
    manifest.reader_export_sha256 = Some(reader_sha);
    plan.manifest = manifest;
    plan.fresh()?;
    Ok(plan)
}
