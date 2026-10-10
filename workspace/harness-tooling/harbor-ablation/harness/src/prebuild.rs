//! Raw-only native coordinator and exact pinned structural seal. No reader/grader input.
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use gptgrep_core::{GenerationPin, RichGenerationReader, RichGenerationWriter};
use gptgrep_parse::{CoverageState, NativeOptions, OcrMode, ResourceBudget};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_CONTROL: u64 = 1_048_576;
const MAX_RAW: u64 = 67_108_864;
const MAX_RECEIPT: usize = 1_048_576;
const MAX_INVENTORY: usize = 4096;
const ENGINE_FILES: [&str; 7] = [
    "gptgrep-index.json",
    "meta.json",
    "files-extra.bin",
    "filestamps.json",
    "files.bin",
    "lookup.bin",
    "index.bin",
];
const RAW_SCHEMA: &str = "gptgrep.harbor.raw-group.v1";
const CONTROLS_SCHEMA: &str = "gptgrep.harbor.native-controls.v1";

#[derive(Parser)]
#[command(about = "Raw-only native prebuild and structural seal; no trials, agents or providers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    BuildNative {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        raw_manifest: PathBuf,
        #[arg(long)]
        build_controls: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
    },
    Seal {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        pin: PathBuf,
        #[arg(long)]
        raw_manifest: PathBuf,
        #[arg(long)]
        build_controls: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema_version: String,
    group_id: String,
    #[serde(deserialize_with = "raw_documents")]
    documents: Vec<RawFile>,
}
fn raw_documents<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<RawFile>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<RawFile>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most eight raw files")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(8);
            loop {
                if out.len() == 8 {
                    if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                        return Err(serde::de::Error::custom("raw_group_document_limit"));
                    }
                    return Ok(out);
                }
                let Some(value) = seq.next_element::<RawFile>()? else {
                    return Ok(out);
                };
                out.push(value);
            }
        }
    }
    d.deserialize_seq(Visitor)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeControls {
    page_window: usize,
    dpi: f32,
    render_pages: bool,
    /// Deprecated compatibility input; page counts do not limit document admission.
    #[serde(
        default,
        rename = "max_document_pages",
        skip_serializing_if = "Option::is_none"
    )]
    _legacy_max_document_pages: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HardControls {
    native_wall: bool,
    native_rss: bool,
    total_index_publication_bytes: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Controls {
    schema_version: String,
    group_id: String,
    observation_wall_seconds: u64,
    raw_group_bytes: u64,
    seal_inventory_bytes: u64,
    seal_inventory_files: usize,
    native: NativeControls,
    resource_budget: ResourceBudget,
    hard_limits: HardControls,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinInput {
    generation: String,
    manifest_sha256: String,
}
impl PinInput {
    fn pin(&self) -> GenerationPin {
        GenerationPin {
            generation: self.generation.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Current {
    schema_version: String,
    generation: String,
    manifest_sha256: String,
}
struct Timer {
    started: Instant,
    lifetime: Duration,
}
impl Timer {
    fn check(&self) -> Result<()> {
        ensure!(
            self.started.elapsed() < self.lifetime,
            "observation_deadline_exceeded"
        );
        Ok(())
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn relative(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty()
            && s.len() <= 240
            && !s.contains('\\')
            && !s.chars().any(char::is_control)
            && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
            && Path::new(s)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "unsafe_relative_path"
    );
    ensure!(
        s.split('/').count() <= 16 && !s.split('/').any(|p| p == ".gptgrep"),
        "unsafe_relative_path"
    );
    Ok(())
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "unsafe_absolute_path"
    );
    Ok(p)
}
#[cfg(unix)]
fn open_nofollow(path: &Path, directory: bool) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let p = absolute(path)?;
    let parts = p
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .collect::<Vec<_>>();
    let mut held = File::open("/")?;
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            bail!("unsafe_absolute_path")
        };
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let dir = i + 1 < parts.len() || directory;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if dir {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        // SAFETY: held owns the live directory fd and name is NUL terminated;
        // every returned descriptor transfers into exactly one File.
        let fd = unsafe { libc::openat(held.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "input_not_regular_or_symlink");
        // SAFETY: successful openat returned a new owned fd.
        held = unsafe { File::from_raw_fd(fd) };
    }
    ensure!(
        if directory {
            held.metadata()?.is_dir()
        } else {
            held.metadata()?.is_file()
        },
        "input_type_rejected"
    );
    Ok(held)
}
#[cfg(not(unix))]
fn open_nofollow(_path: &Path, _directory: bool) -> Result<File> {
    bail!("capability_unavailable: held no-follow input custody requires Unix")
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}
#[cfg(unix)]
fn identity(file: &File) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(Identity {
        device: m.dev(),
        inode: m.ino(),
    })
}
#[cfg(not(unix))]
fn identity(_file: &File) -> Result<Identity> {
    bail!("capability_unavailable: stable file identity requires Unix")
}

// Digests are retained from the admitted stream/buffer. Freshness reads use the
// SAME held descriptor, never replace receipt identity with a pathname rehash.
struct FileSnapshot {
    path: PathBuf,
    file: File,
    id: Identity,
    bytes: u64,
    sha256: String,
}
impl FileSnapshot {
    fn read(path: &Path, limit: u64, timer: &Timer) -> Result<Self> {
        timer.check()?;
        let path = absolute(path)?;
        let mut file = open_nofollow(&path, false)?;
        let id = identity(&file)?;
        let (bytes, sha256) = hash_held(&mut file, limit, timer)?;
        let s = Self {
            path,
            file,
            id,
            bytes,
            sha256,
        };
        s.fresh(timer)?;
        Ok(s)
    }
    fn fresh(&self, timer: &Timer) -> Result<()> {
        timer.check()?;
        ensure!(
            identity(&open_nofollow(&self.path, false)?)? == self.id,
            "admitted_path_identity_changed"
        );
        let mut file = self.file.try_clone()?;
        let actual = hash_held(&mut file, self.bytes, timer)?;
        ensure!(
            actual == (self.bytes, self.sha256.clone()),
            "admitted_bytes_changed"
        );
        Ok(())
    }
}
fn hash_held(file: &mut File, limit: u64, timer: &Timer) -> Result<(u64, String)> {
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "file_bytes_limit"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut sum = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 65536];
    loop {
        timer.check()?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).context("file_byte_overflow")?;
        ensure!(bytes <= limit, "file_bytes_limit");
        sum.update(&buf[..n]);
    }
    timer.check()?;
    Ok((bytes, format!("{:x}", sum.finalize())))
}
struct JsonSnapshot<T> {
    value: T,
    bytes: Vec<u8>,
    asset: FileSnapshot,
}
impl<T: serde::de::DeserializeOwned> JsonSnapshot<T> {
    fn load(path: &Path, limit: u64, timer: &Timer) -> Result<Self> {
        let mut file = open_nofollow(path, false)?;
        ensure!(file.metadata()?.len() <= limit, "json_bytes_limit");
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(file.metadata()?.len() as usize)?;
        (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= limit, "json_bytes_limit");
        let id = identity(&file)?;
        let sha256 = digest(&bytes);
        let value = serde_json::from_slice(&bytes)?;
        let asset = FileSnapshot {
            path: absolute(path)?,
            file,
            id,
            bytes: bytes.len() as u64,
            sha256,
        };
        asset.fresh(timer)?;
        Ok(Self {
            value,
            bytes,
            asset,
        })
    }
}
struct Inputs {
    root: PathBuf,
    root_fd: File,
    root_identity: Identity,
    manifest: JsonSnapshot<RawManifest>,
    controls: JsonSnapshot<Controls>,
    raw: Vec<FileSnapshot>,
    timer: Timer,
}
fn controls_valid(c: &Controls) -> Result<()> {
    ensure!(
        c.schema_version == CONTROLS_SCHEMA
            && (1..=180).contains(&c.observation_wall_seconds)
            && (1..=MAX_RAW).contains(&c.raw_group_bytes)
            && (1..=1_073_741_824).contains(&c.seal_inventory_bytes)
            && (1..=MAX_INVENTORY).contains(&c.seal_inventory_files),
        "controls_limits"
    );
    ensure!(
        !c.hard_limits.native_wall
            && !c.hard_limits.native_rss
            && !c.hard_limits.total_index_publication_bytes,
        "capability_unavailable: hard native wall/RSS/total-index-publication limits require a qualified executor"
    );
    ensure!(
        (1..=25).contains(&c.native.page_window)
            && c.native.dpi.is_finite()
            && c.native.dpi > 0.0
            && c.native.dpi <= 300.0
            && c.native.render_pages,
        "native_controls"
    );
    c.resource_budget.validate()
}
fn outside(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        !absolute(path)?.starts_with(root),
        "control_output_staging_must_be_outside_corpus"
    );
    Ok(())
}
impl Inputs {
    fn admit(root: &Path, manifest: &Path, controls: &Path) -> Result<Self> {
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos")),
            "capability_unavailable: atomic fresh directory publication requires qualified Linux/macOS implementation"
        );
        let started = Instant::now();
        let initial = Timer {
            started,
            lifetime: Duration::from_secs(180),
        };
        let root = absolute(root)?;
        let root_fd = open_nofollow(&root, true)?;
        ensure!(root.canonicalize()? == root, "root_must_be_canonical");
        let root_identity = identity(&root_fd)?;
        outside(&root, manifest)?;
        outside(&root, controls)?;
        let manifest = JsonSnapshot::<RawManifest>::load(manifest, MAX_CONTROL, &initial)?;
        let controls = JsonSnapshot::<Controls>::load(controls, MAX_CONTROL, &initial)?;
        ensure!(
            manifest.asset.id != controls.asset.id,
            "control_alias_rejected"
        );
        controls_valid(&controls.value)?;
        let timer = Timer {
            started,
            lifetime: Duration::from_secs(controls.value.observation_wall_seconds),
        };
        timer.check()?;
        let m = &manifest.value;
        ensure!(
            m.schema_version == RAW_SCHEMA
                && m.group_id == controls.value.group_id
                && !m.group_id.is_empty()
                && m.group_id.len() <= 64
                && m.group_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && (1..=8).contains(&m.documents.len()),
            "raw_group_limits"
        );
        let mut last = None;
        let mut total = 0u64;
        let mut raw = Vec::with_capacity(m.documents.len());
        // The frontend's bounded JSON buffers/descriptors are separate from
        // parser-owned working admission; neither field proves global RSS.
        for entry in &m.documents {
            relative(&entry.path)?;
            ensure!(
                Path::new(&entry.path)
                    .extension()
                    .is_some_and(|x| x == "pdf"),
                "raw_only_pdf_required"
            );
            ensure!(
                last.is_none_or(|p: &String| p < &entry.path),
                "raw_paths_sorted_unique"
            );
            last = Some(&entry.path);
            ensure!(
                entry.bytes > 0 && entry.bytes <= MAX_RAW && valid_digest(&entry.sha256),
                "raw_entry_limits"
            );
            total = total
                .checked_add(entry.bytes)
                .context("raw_total_overflow")?;
            ensure!(
                total <= controls.value.raw_group_bytes,
                "raw_group_bytes_limit"
            );
            let admitted = FileSnapshot::read(&root.join(&entry.path), entry.bytes, &timer)?;
            ensure!(
                admitted.id != manifest.asset.id
                    && admitted.id != controls.asset.id
                    && raw
                        .iter()
                        .all(|previous: &FileSnapshot| previous.id != admitted.id),
                "raw_source_alias_rejected"
            );
            ensure!(
                admitted.bytes == entry.bytes && admitted.sha256 == entry.sha256,
                "raw_bytes_mismatch"
            );
            raw.push(admitted);
        }
        let inputs = Self {
            root,
            root_fd,
            root_identity,
            manifest,
            controls,
            raw,
            timer,
        };
        inputs.fresh()?;
        Ok(inputs)
    }
    fn fresh(&self) -> Result<()> {
        self.timer.check()?;
        ensure!(
            identity(&self.root_fd)? == self.root_identity
                && identity(&open_nofollow(&self.root, true)?)? == self.root_identity,
            "root_identity_changed"
        );
        self.manifest.asset.fresh(&self.timer)?;
        self.controls.asset.fresh(&self.timer)?;
        let expected = self
            .manifest
            .value
            .documents
            .iter()
            .map(|d| d.path.clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            scan_files(&self.root, true, 8, &self.timer)? == expected,
            "raw_set_mismatch"
        );
        for raw in &self.raw {
            raw.fresh(&self.timer)?;
        }
        self.timer.check()
    }
    fn output_valid(&self, path: &Path) -> Result<()> {
        let path = absolute(path)?;
        outside(&self.root, &path)?;
        ensure!(
            !path.exists() && fs::symlink_metadata(&path).is_err(),
            "output_must_be_fresh"
        );
        let parent = path.parent().context("output_parent_required")?;
        open_nofollow(parent, true)?;
        ensure!(
            ![&self.manifest.asset.path, &self.controls.asset.path]
                .iter()
                .any(|p| p.starts_with(&path)),
            "output_overlaps_controls"
        );
        Ok(())
    }
}
fn scan_files(root: &Path, raw: bool, cap: usize, timer: &Timer) -> Result<BTreeSet<String>> {
    struct WalkState {
        files: BTreeSet<String>,
        nodes: usize,
    }
    fn walk(
        root: &Path,
        dir: &Path,
        raw: bool,
        cap: usize,
        timer: &Timer,
        state: &mut WalkState,
        depth: usize,
    ) -> Result<()> {
        timer.check()?;
        ensure!(depth <= 16, "inventory_depth_limit");
        open_nofollow(dir, true)?;
        for entry in fs::read_dir(dir)? {
            timer.check()?;
            let entry = entry?;
            state.nodes += 1;
            ensure!(state.nodes <= cap * 17 + 16, "inventory_node_limit");
            let p = entry.path();
            let name = p
                .strip_prefix(root)?
                .to_str()
                .context("path_utf8_required")?
                .to_owned();
            let meta = fs::symlink_metadata(&p)?;
            ensure!(!meta.file_type().is_symlink(), "inventory_symlink_rejected");
            if raw && dir == root && name == ".gptgrep" {
                ensure!(meta.is_dir(), "state_directory_required");
                continue;
            }
            ensure!(
                name.len() <= 512 && !name.chars().any(char::is_control),
                "inventory_path_limit"
            );
            if meta.is_dir() {
                walk(root, &p, raw, cap, timer, state, depth + 1)?;
            } else {
                ensure!(meta.is_file(), "inventory_regular_file_required");
                state.files.insert(name);
                ensure!(state.files.len() <= cap, "inventory_file_limit");
            }
        }
        Ok(())
    }
    let mut state = WalkState {
        files: BTreeSet::new(),
        nodes: 0,
    };
    walk(root, root, raw, cap, timer, &mut state, 0)?;
    Ok(state.files)
}
fn expect_file(set: &mut BTreeSet<String>, name: &str, cap: usize) -> Result<()> {
    ensure!(
        name.len() <= 512 && !name.chars().any(char::is_control),
        "expected_generation_path_limit"
    );
    if !set.contains(name) {
        ensure!(set.len() < cap, "expected_generation_file_limit");
        set.insert(name.to_owned());
    }
    Ok(())
}
#[derive(Serialize)]
struct Inventory {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Serialize)]
struct DocumentCoverage {
    path: String,
    source_sha256: String,
    prepared_sha256: String,
    preparation_profile_sha256: String,
    native_profile_sha256: String,
    pages: Vec<(u32, CoverageState)>,
}
#[derive(Serialize)]
struct Receipt {
    schema_version: &'static str,
    qualification: &'static str,
    producer_observation: &'static str,
    canonical_root: PathBuf,
    group_id: String,
    raw_manifest_sha256: String,
    controls_sha256: String,
    raw_documents: Vec<RawFile>,
    pin: GenerationPin,
    pin_input_sha256: Option<String>,
    current_snapshot_sha256: String,
    inventory_scope: &'static str,
    inventory: Vec<Inventory>,
    generation_bytes: u64,
    coverage: Vec<DocumentCoverage>,
    parser_identity: &'static str,
    liteparse_revision: &'static str,
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
    hard_executor_qualification: &'static str,
    wrapper_json_buffer_ceiling_bytes: u64,
    retained_raw_payload_bytes: u64,
    retained_raw_file_descriptors: usize,
    retained_generation_file_descriptors: usize,
    wrapper_metadata_scope: &'static str,
}
// Not constructible from a receipt DTO; every member retains source custody.
struct VerifiedSnapshot {
    reader: RichGenerationReader,
    inputs: Inputs,
    pin_input: Option<JsonSnapshot<PinInput>>,
    current: JsonSnapshot<Current>,
    manifest: FileSnapshot,
    artifacts: Vec<FileSnapshot>,
    receipt: Receipt,
}
impl VerifiedSnapshot {
    fn verify(
        inputs: Inputs,
        pin: GenerationPin,
        pin_input: Option<JsonSnapshot<PinInput>>,
        origin: &'static str,
        staging: Option<u64>,
    ) -> Result<Self> {
        inputs.fresh()?;
        ensure!(
            !pin.generation.is_empty()
                && pin.generation.len() <= 128
                && pin
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && valid_digest(&pin.manifest_sha256),
            "invalid_generation_pin"
        );
        let current = JsonSnapshot::<Current>::load(
            &inputs.root.join(".gptgrep/CURRENT.json"),
            MAX_CONTROL,
            &inputs.timer,
        )?;
        ensure!(
            current.value.schema_version == "gptgrep.manifest.v2"
                && current.value.generation == pin.generation
                && current.value.manifest_sha256 == pin.manifest_sha256,
            "current_pin_mismatch"
        );
        let generation = inputs
            .root
            .join(".gptgrep/generations")
            .join(&pin.generation);
        // Admit the manifest's bytes before the public reader deserializes it.
        // The reader's bounded typed/native allocations are a separate contract.
        let manifest = FileSnapshot::read(
            &generation.join("manifest.json"),
            inputs.controls.value.seal_inventory_bytes,
            &inputs.timer,
        )?;
        ensure!(
            manifest.sha256 == pin.manifest_sha256,
            "manifest_snapshot_pin_mismatch"
        );
        let reader = RichGenerationReader::open_pinned(&inputs.root, &pin)?;
        reader.verify_all()?;
        inputs.timer.check()?;
        let m = reader.manifest()?;
        ensure!(
            serde_json::to_value(&m.resource_budget)?
                == serde_json::to_value(&inputs.controls.value.resource_budget)?,
            "resource_controls_manifest_mismatch"
        );
        ensure!(
            m.documents.len() == inputs.raw.len(),
            "generation_raw_set_mismatch"
        );
        let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
        let mut coverage = Vec::new();
        for (doc, raw) in m.documents.iter().zip(&inputs.manifest.value.documents) {
            ensure!(
                doc.document.path == raw.path
                    && doc.document.source_sha256 == raw.sha256
                    && doc.document.source_bytes == raw.bytes,
                "generation_raw_binding_mismatch"
            );
            ensure!(
                doc.prepared.format == "pdf"
                    && doc.prepared.source_sha256 == raw.sha256
                    && doc.prepared.prepared_sha256 == raw.sha256,
                "raw_pdf_preparation_binding"
            );
            ensure!(
                doc.ocr_tasks.is_empty() && doc.ocr_results.is_empty(),
                "native_only_coverage_required"
            );
            native_coverage(
                &doc.pages
                    .iter()
                    .map(|p| (p.number, p.coverage.clone()))
                    .collect::<Vec<_>>(),
                &[],
            )?;
            expect_file(
                &mut expected,
                doc.prepared_path
                    .to_str()
                    .context("prepared_path_utf8_required")?,
                inputs.controls.value.seal_inventory_files,
            )?;
            expect_file(
                &mut expected,
                &format!("text/{}.txt", doc.document.id),
                inputs.controls.value.seal_inventory_files,
            )?;
            for artifact in &doc.artifacts {
                expect_file(
                    &mut expected,
                    artifact
                        .artifact
                        .relative_path
                        .to_str()
                        .context("artifact_path_utf8_required")?,
                    inputs.controls.value.seal_inventory_files,
                )?;
                if matches!(
                    artifact.artifact.kind,
                    gptgrep_parse::ArtifactKind::NativeText | gptgrep_parse::ArtifactKind::OcrText
                ) {
                    expect_file(
                        &mut expected,
                        &format!("artifact-text/{}.txt", artifact.artifact.id),
                        inputs.controls.value.seal_inventory_files,
                    )?;
                }
            }
            coverage.push(DocumentCoverage {
                path: raw.path.clone(),
                source_sha256: raw.sha256.clone(),
                prepared_sha256: doc.prepared.prepared_sha256.clone(),
                preparation_profile_sha256: doc.prepared.preparation_profile_sha256.clone(),
                native_profile_sha256: doc.native_profile_sha256.clone(),
                pages: doc
                    .pages
                    .iter()
                    .map(|p| (p.number, p.coverage.clone()))
                    .collect(),
            });
        }
        for name in ENGINE_FILES {
            expect_file(
                &mut expected,
                &format!("artifact-index/{name}"),
                inputs.controls.value.seal_inventory_files,
            )?;
        }
        let found = scan_files(
            &generation,
            false,
            inputs.controls.value.seal_inventory_files,
            &inputs.timer,
        )?;
        ensure!(found == expected, "selected_generation_file_set_mismatch");
        let mut artifacts = Vec::new();
        let mut inventory = Vec::new();
        let mut total = 0u64;
        for path in found {
            let remaining = inputs
                .controls
                .value
                .seal_inventory_bytes
                .checked_sub(total)
                .context("seal_inventory_bytes_limit")?;
            let asset = FileSnapshot::read(&generation.join(&path), remaining, &inputs.timer)?;
            total = total
                .checked_add(asset.bytes)
                .context("inventory_byte_overflow")?;
            ensure!(
                total <= inputs.controls.value.seal_inventory_bytes,
                "seal_inventory_bytes_limit"
            );
            inventory.push(Inventory {
                path,
                bytes: asset.bytes,
                sha256: asset.sha256.clone(),
            });
            artifacts.push(asset);
        }
        let index = artifacts
            .iter()
            .find(|a| a.path == generation.join("artifact-index/gptgrep-index.json"))
            .context("index_manifest_missing")?;
        ensure!(index.bytes <= MAX_CONTROL, "index_manifest_bytes_limit");
        let mut held = index.file.try_clone()?;
        held.seek(SeekFrom::Start(0))?;
        let mut index_bytes = Vec::new();
        held.take(MAX_CONTROL + 1).read_to_end(&mut index_bytes)?;
        ensure!(
            index_bytes.len() as u64 == index.bytes && digest(&index_bytes) == index.sha256,
            "index_manifest_snapshot_changed"
        );
        let index_json: serde_json::Value = serde_json::from_slice(&index_bytes)?;
        let engine = index_json["engine_revision"]
            .as_str()
            .context("index_engine_revision_missing")?
            .to_owned();
        let table = index_json["file_table_id"]
            .as_str()
            .context("index_file_table_id_missing")?
            .to_owned();
        let index_sha = index.sha256.clone();
        let receipt = Receipt {
            schema_version: "gptgrep.harbor.prebuild.receipt.v1",
            qualification: "source_structure_only_not_production_exporter_admission",
            producer_observation: origin,
            canonical_root: inputs.root.clone(),
            group_id: inputs.manifest.value.group_id.clone(),
            raw_manifest_sha256: inputs.manifest.asset.sha256.clone(),
            controls_sha256: inputs.controls.asset.sha256.clone(),
            raw_documents: inputs.manifest.value.documents.clone(),
            pin,
            pin_input_sha256: pin_input.as_ref().map(|p| p.asset.sha256.clone()),
            current_snapshot_sha256: current.asset.sha256.clone(),
            inventory_scope: "exact_current_snapshot_and_selected_generation_only_sibling_history_unverified",
            inventory,
            generation_bytes: total,
            coverage,
            parser_identity: gptgrep_parse::RICH_PARSER,
            liteparse_revision: gptgrep_parse::LITEPARSE_REVISION,
            index_engine_revision: engine,
            index_file_table_id: table,
            index_manifest_snapshot_sha256: index_sha,
            builder_source_revision: None,
            builder_binary_sha256: None,
            selected_resource_budget: inputs.controls.value.resource_budget.clone(),
            shared_staging_reservation_bytes_observed: staging,
            elapsed_ms: inputs.timer.started.elapsed().as_millis(),
            native_library_identity: None,
            native_peak_rss_bytes: None,
            current_frontdoor_provider_calls: 0,
            hard_executor_qualification: "HOLD_no_hard_native_wall_RSS_index_publication_or_Linux_runtime_proof",
            wrapper_json_buffer_ceiling_bytes: 8_388_608,
            retained_raw_payload_bytes: 0,
            retained_raw_file_descriptors: inputs.raw.len(),
            retained_generation_file_descriptors: artifacts.len() + 1,
            wrapper_metadata_scope: "bounded_paths_file_count_and_JSON_buffers_separate_from_parser_owned_working_not_global_RSS",
        };
        let s = Self {
            reader,
            inputs,
            pin_input,
            current,
            manifest,
            artifacts,
            receipt,
        };
        s.fresh()?;
        Ok(s)
    }
    fn fresh(&self) -> Result<()> {
        self.inputs.fresh()?;
        if let Some(pin) = &self.pin_input {
            pin.asset.fresh(&self.inputs.timer)?;
        }
        self.current.asset.fresh(&self.inputs.timer)?;
        self.manifest.fresh(&self.inputs.timer)?;
        for artifact in &self.artifacts {
            artifact.fresh(&self.inputs.timer)?;
        }
        let generation = self
            .inputs
            .root
            .join(".gptgrep/generations")
            .join(&self.receipt.pin.generation);
        let expected = self
            .receipt
            .inventory
            .iter()
            .map(|f| f.path.clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            scan_files(
                &generation,
                false,
                self.inputs.controls.value.seal_inventory_files,
                &self.inputs.timer
            )? == expected,
            "selected_generation_file_set_changed"
        );
        self.reader.verify_all()?;
        self.inputs.timer.check()
    }
    fn publish(self, output: &Path) -> Result<String> {
        self.inputs.output_valid(output)?;
        let bytes = bounded_json(&self.receipt)?;
        self.fresh()?;
        let output = absolute(output)?;
        let parent = output.parent().context("output_parent_required")?;
        let tmp = private_tempdir(parent, ".gptgrep-prebuild-seal-")?;
        let pin_bytes = bounded_json(&PinInput {
            generation: self.receipt.pin.generation.clone(),
            manifest_sha256: self.receipt.pin.manifest_sha256.clone(),
        })?;
        for (name, data) in [
            ("receipt.json", bytes.as_slice()),
            ("pin.json", pin_bytes.as_slice()),
            ("raw-manifest.json", self.inputs.manifest.bytes.as_slice()),
            ("build-controls.json", self.inputs.controls.bytes.as_slice()),
        ] {
            let p = tmp.path().join(name);
            let mut file = private_file(&p)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        File::open(tmp.path())?.sync_all()?;
        self.fresh()?;
        self.inputs.output_valid(&output)?;
        rename_fresh(tmp.path(), &output)?;
        let _ = tmp.keep();
        open_nofollow(parent, true)
            .and_then(|dir| dir.sync_all().map_err(Into::into))
            .context("publication_sync_unknown_no_retry_output_may_exist")?;
        Ok(digest(&bytes))
    }
}
struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self
            .0
            .len()
            .checked_add(b.len())
            .filter(|n| *n <= MAX_RECEIPT)
            .ok_or_else(|| std::io::Error::other("receipt_bytes_limit"))?;
        self.0
            .try_reserve_exact(n - self.0.len())
            .map_err(std::io::Error::other)?;
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bounded_json(v: &impl Serialize) -> Result<Vec<u8>> {
    let mut out = Limited(Vec::new());
    serde_json::to_writer(&mut out, v)?;
    Ok(out.0)
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_fresh(source: &Path, dest: &Path) -> Result<()> {
    let s = std::ffi::CString::new(source.as_os_str().as_encoded_bytes())?;
    let d = std::ffi::CString::new(dest.as_os_str().as_encoded_bytes())?;
    #[cfg(target_os = "linux")]
    // SAFETY: both names are NUL terminated, AT_FDCWD selects absolute paths;
    // RENAME_NOREPLACE cannot overwrite an existing destination.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: terminated absolute names and RENAME_EXCL forbid replacement.
    let result = unsafe { libc::renamex_np(s.as_ptr(), d.as_ptr(), libc::RENAME_EXCL) };
    ensure!(
        result == 0,
        "fresh_output_rename_failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_fresh(_source: &Path, _dest: &Path) -> Result<()> {
    bail!("capability_unavailable: atomic no-replace directory publication unsupported")
}
#[cfg(unix)]
fn private_tempdir(parent: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(0o700);
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(permissions.clone())
        .tempdir_in(parent)?;
    // Keep exact owner permissions even under a restrictive caller umask.
    fs::set_permissions(directory.path(), permissions)?;
    Ok(directory)
}
#[cfg(not(unix))]
fn private_tempdir(_parent: &Path, _prefix: &str) -> Result<tempfile::TempDir> {
    bail!("capability_unavailable: private POSIX directory modes unsupported")
}
#[cfg(unix)]
fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // Set permissions on the newly opened file before any content is written.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
#[cfg(not(unix))]
fn private_file(_path: &Path) -> Result<File> {
    bail!("capability_unavailable: private POSIX file modes unsupported")
}
fn prepared_import_root(prepared: &gptgrep_parse::PreparedDocument) -> Result<PathBuf> {
    let path = prepared.staging_root();
    ensure!(
        fs::symlink_metadata(path)?.is_dir()
            && !fs::symlink_metadata(path)?.file_type().is_symlink()
            && path.canonicalize()? == path,
        "prepared_import_root_must_be_canonical_nonsymlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // This newly prepared child is contained in our private outer TempDir.
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    bail!("capability_unavailable: private POSIX prepared directory modes unsupported");
    Ok(path.to_path_buf())
}
fn native_coverage(pages: &[(u32, CoverageState)], blockers: &[String]) -> Result<()> {
    ensure!(
        !pages.is_empty()
            && blockers.is_empty()
            && pages
                .iter()
                .all(|(_, c)| matches!(c, CoverageState::Native | CoverageState::BlankVerified)),
        "native_only_required_ocr_or_capability_blocker"
    );
    Ok(())
}
async fn build_native(inputs: Inputs, output: &Path) -> Result<String> {
    inputs.output_valid(output)?;
    inputs.fresh()?;
    let budget = inputs.controls.value.resource_budget.clone();
    let parent = absolute(output)?
        .parent()
        .context("output_parent_required")?
        .to_path_buf();
    let mut writer = RichGenerationWriter::begin(&inputs.root, budget.clone())?;
    for (raw, spec) in inputs.raw.iter().zip(&inputs.manifest.value.documents) {
        inputs.fresh()?;
        let staging = private_tempdir(&parent, ".gptgrep-native-preparation-")?;
        let prepared = gptgrep_parse::prepare_document(
            &raw.path,
            &staging.path().canonicalize()?,
            &gptgrep_parse::PreparationOptions,
            &budget,
        )
        .await?;
        let prepared_root = prepared_import_root(&prepared)?;
        inputs.fresh()?;
        ensure!(
            prepared.summary().source_sha256 == raw.sha256
                && prepared.summary().source_bytes == raw.bytes
                && prepared.summary().prepared_sha256 == raw.sha256,
            "admitted_prepared_source_mismatch"
        );
        let n = &inputs.controls.value.native;
        let candidate = gptgrep_parse::extract_native(
            &prepared,
            &NativeOptions {
                page_window: n.page_window,
                dpi: n.dpi,
                render_pages: n.render_pages,
            },
            &budget,
        )
        .await?;
        inputs.fresh()?;
        native_coverage(&candidate.plan().pages, &candidate.plan().blockers)?;
        ensure!(
            candidate.plan().tasks.is_empty(),
            "native_only_ocr_tasks_rejected"
        );
        let parsed = gptgrep_parse::finalize_document(candidate, vec![], OcrMode::Off, &budget)?;
        inputs.fresh()?;
        writer.add_document(&spec.path, &prepared_root, parsed)?;
    }
    inputs.fresh()?;
    // Synchronous index build/publication has no harness deadline or total-index
    // hook. A post-call expiry is observational and may leave a NEW CURRENT;
    // hard guarantees are rejected before writer creation by controls_valid.
    let pin = writer.publish()?;
    inputs
        .timer
        .check()
        .context("post_publish_observation_deadline_failed_CURRENT_may_have_changed")?;
    let staging = budget.staging_used();
    VerifiedSnapshot::verify(
        inputs,
        pin,
        None,
        "native_coordinator_local_execution_observed_not_authenticated_history",
        Some(staging),
    )?
    .publish(output)
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let result = match Cli::parse().command {
        Command::BuildNative {
            root,
            raw_manifest,
            build_controls,
            output_dir,
        } => {
            let inputs = Inputs::admit(&root, &raw_manifest, &build_controls)?;
            build_native(inputs, &output_dir).await?
        }
        Command::Seal {
            root,
            pin,
            raw_manifest,
            build_controls,
            output_dir,
        } => {
            let inputs = Inputs::admit(&root, &raw_manifest, &build_controls)?;
            outside(&inputs.root, &pin)?;
            let admitted = JsonSnapshot::<PinInput>::load(&pin, MAX_CONTROL, &inputs.timer)?;
            VerifiedSnapshot::verify(
                inputs,
                admitted.value.pin(),
                Some(admitted),
                "pinned_structure_only_historical_producer_not_authenticated",
                None,
            )?
            .publish(&output_dir)?
        }
    };
    println!(
        "{}",
        serde_json::json!({"receipt_sha256":result,"qualification":"source_structure_only_not_production_exporter_admission"})
    );
    Ok(())
}
#[cfg(test)]
#[path = "prebuild_tests.rs"]
mod tests;
