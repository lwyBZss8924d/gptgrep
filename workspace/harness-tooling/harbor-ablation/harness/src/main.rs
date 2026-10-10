use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_JSON: u64 = 1024 * 1024;
const MAX_OUTPUT: usize = 128 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "Offline Harbor source export; never launches agents or providers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Project every pinned source row into reader-only and separate grader inputs.
    ProjectCohort {
        #[arg(long)]
        questions: PathBuf,
        #[arg(long)]
        questions_sha256: String,
        #[arg(long)]
        sources_lock: PathBuf,
        #[arg(long)]
        sources_lock_sha256: String,
        #[arg(long)]
        mapping: PathBuf,
        #[arg(long)]
        mapping_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
    /// Bind one frozen cohort batch to its validated reader export; never selects rows.
    BindCohortGraders {
        #[arg(long)]
        cohort: PathBuf,
        #[arg(long)]
        cohort_sha256: String,
        #[arg(long)]
        batch: String,
        #[arg(long)]
        reader_export: PathBuf,
        #[arg(long)]
        reader_export_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
    ReportPaired {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        input_sha256: String,
    },
    PlanProduction {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        corpus_sha256: String,
        #[arg(long)]
        readers: PathBuf,
        #[arg(long)]
        readers_sha256: String,
        #[arg(long)]
        options: PathBuf,
        #[arg(long)]
        options_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
    ExportProductionReaders {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        corpus_sha256: String,
        #[arg(long)]
        readers: PathBuf,
        #[arg(long)]
        readers_sha256: String,
        #[arg(long)]
        options: PathBuf,
        #[arg(long)]
        options_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
    ExportProductionVerifier {
        #[arg(long)]
        graders: PathBuf,
        #[arg(long)]
        reader_export: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
    },
    Plan {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        readers: PathBuf,
        #[arg(long)]
        options: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, value_delimiter = ',')]
        task_ids: Vec<String>,
    },
    ExportReaders {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        readers: PathBuf,
        #[arg(long)]
        options: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, value_delimiter = ',')]
        task_ids: Vec<String>,
    },
    ExportVerifier {
        #[arg(long)]
        graders: PathBuf,
        #[arg(long)]
        reader_export: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
    },
    Validate {
        #[arg(long)]
        export_dir: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    path: String,
    sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    id: String,
    raw: Asset,
    prebuilt: Vec<Derived>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Derived {
    asset: Asset,
    source_sha256: String,
    derivation: String,
    query_agnostic: bool,
    receipt: Asset,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    asset: Asset,
    version: String,
    source_revision: String,
    synthetic: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    schema_version: String,
    documents: Vec<Document>,
    tool: Tool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reader {
    id: String,
    question: String,
    question_sha256: String,
    document_scope: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Readers {
    schema_version: String,
    corpus_sha256: String,
    tasks: Vec<Reader>,
}
#[derive(Debug, Serialize, Deserialize)]
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
    repetitions: u32,
    seed: u32,
    synthetic: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grade {
    id: String,
    question_sha256: String,
    reference_answer: String,
}
#[derive(Debug, Serialize, Deserialize)]
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
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivationReceipt {
    schema_version: String,
    source_sha256: String,
    artifact_sha256: String,
    derivation: String,
    query_agnostic: bool,
    synthetic: bool,
    preprocessing_latency_ms: u64,
    preprocessing_cost_usd: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    id: String,
    question_sha256: String,
    raw_sha256: BTreeMap<String, String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    role: String,
    corpus_sha256: String,
    reader_projection_sha256: String,
    reader_export_sha256: Option<String>,
    options_sha256: String,
    synthetic: bool,
    executable: bool,
    task_bindings: Vec<Binding>,
    files: BTreeMap<String, String>,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn check_hash(value: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err("invalid_sha256".into())
    }
}
fn check_id(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Ok(())
    } else {
        Err("invalid_id".into())
    }
}
fn check_rel(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 240
        || value.contains('\\')
        || value.contains('\0')
        || !Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        || value
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err("unsafe_relative_path".into());
    }
    Ok(())
}
// Walk every directory using no-follow descriptors. Neither a leaf symlink nor a
// swapped intermediate symlink is followed by the read front door.
fn regular_file(path: &Path, limit: u64) -> Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut directory = File::open("/").map_err(|e| e.to_string())?;
    let parts: Vec<_> = absolute
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .collect();
    let mut leaf = None;
    for (i, component) in parts.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("unsafe_input_path".into());
        };
        let name = std::ffi::CString::new(name.as_encoded_bytes()).map_err(|_| "input_nul")?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if i + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        // SAFETY: directory owns a live fd, name is terminated, and successful fd is owned once.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err("input_not_regular_or_symlink".into());
        }
        // SAFETY: fd is freshly returned by openat and ownership transfers here.
        let file = unsafe { File::from_raw_fd(fd) };
        if i + 1 == parts.len() {
            leaf = Some(file);
        } else {
            directory = file;
        }
    }
    let file = leaf.ok_or("missing_input_leaf")?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("input_type_or_size_limit".into());
    }
    Ok(file)
}
fn regular_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = regular_file(path, limit)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("input_size_limit".into());
    }
    Ok(bytes)
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<(T, String)> {
    let bytes = regular_bytes(path, MAX_JSON)?;
    let value = serde_json::from_slice(&bytes).map_err(|e| format!("schema: {e}"))?;
    Ok((value, hash(&bytes)))
}
fn asset(root: &Path, value: &Asset) -> Result<Vec<u8>> {
    check_rel(&value.path)?;
    check_hash(&value.sha256)?;
    let bytes = regular_bytes(&root.join(&value.path), MAX_FILE)?;
    if hash(&bytes) != value.sha256 {
        return Err("asset_digest_mismatch".into());
    }
    Ok(bytes)
}
fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|e| e.to_string())
}
fn put(files: &mut BTreeMap<String, Vec<u8>>, path: String, bytes: Vec<u8>) -> Result<()> {
    check_rel(&path)?;
    if files.insert(path, bytes).is_some() {
        return Err("output_collision".into());
    }
    if files.values().map(Vec::len).sum::<usize>() > MAX_OUTPUT {
        return Err("output_size_limit".into());
    }
    Ok(())
}
fn create_output(
    path: &Path,
    files: &BTreeMap<String, Vec<u8>>,
    manifest: &Manifest,
) -> Result<()> {
    let parent = path.parent().ok_or("output_parent_required")?;
    let canonical = parent.canonicalize().map_err(|_| "output_parent_missing")?;
    let absolute_parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(parent)
    };
    if canonical != absolute_parent {
        return Err("output_parent_not_canonical".into());
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| "output_must_be_new")?;
    for (name, bytes) in files {
        let target = path.join(name);
        fs::create_dir_all(target.parent().ok_or("bad_output")?).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(target)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
    }
    // A missing completion manifest makes any partial output inadmissible.
    let mut done = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path.join("export-manifest.json"))
        .map_err(|e| e.to_string())?;
    done.write_all(&json(manifest)?)
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn inputs(
    corpus_path: &Path,
    readers_path: &Path,
    options_path: &Path,
) -> Result<(Corpus, Readers, Options, Manifest)> {
    let (corpus, corpus_sha256): (Corpus, _) = load(corpus_path)?;
    let (readers, reader_projection_sha256): (Readers, _) = load(readers_path)?;
    let (options, options_sha256): (Options, _) = load(options_path)?;
    if corpus.schema_version != "1"
        || readers.schema_version != "1"
        || options.schema_version != "1"
        || readers.corpus_sha256 != corpus_sha256
    {
        return Err("schema_or_corpus_binding".into());
    }
    if !options.synthetic || !corpus.tool.synthetic {
        return Err("synthetic_only_candidate".into());
    }
    if corpus.documents.is_empty()
        || corpus.documents.len() > 16
        || readers.tasks.is_empty()
        || readers.tasks.len() > 64
        || !(1..=10).contains(&options.repetitions)
        || !(1..=3600).contains(&options.timeout_sec)
        || !(1..=16).contains(&options.cpus)
        || !(256..=16384).contains(&options.memory_mb)
        || options.network_mode != "no-network"
        || options.codex_argv.first().map(String::as_str) != Some("codex")
        || options.codex_argv.get(1).map(String::as_str) != Some("exec")
        || options.codex_argv.len() > 32
        || options.codex_argv.iter().any(|x| x.len() > 1024)
        || !options.reader_image.contains("@sha256:")
        || !options.verifier_image.contains("@sha256:")
        || [&options.model, &options.effort, &options.service_tier]
            .iter()
            .any(|s| s.is_empty() || s.len() > 128)
    {
        return Err("frozen_options_or_size_limit".into());
    }
    check_hash(&options.codex_binary_sha256)?;
    for image in [&options.reader_image, &options.verifier_image] {
        check_hash(image.rsplit_once("@sha256:").ok_or("image_digest")?.1)?;
        if !image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/.:_-@".contains(&b))
        {
            return Err("image_name_injection".into());
        }
    }
    let mut docs = BTreeMap::new();
    for document in &corpus.documents {
        check_id(&document.id)?;
        if docs.insert(&document.id, &document.raw.sha256).is_some() {
            return Err("duplicate_document".into());
        }
        if !document.raw.path.ends_with(".pdf")
            || document.prebuilt.is_empty()
            || document.prebuilt.len() > 16
        {
            return Err("raw_pdf_required".into());
        }
    }
    let mut ids = BTreeSet::new();
    let mut task_bindings = Vec::new();
    for task in &readers.tasks {
        check_id(&task.id)?;
        if !ids.insert(&task.id)
            || task.question.is_empty()
            || task.question.len() > 12000
            || hash(task.question.as_bytes()) != task.question_sha256
            || task.document_scope.is_empty()
        {
            return Err("reader_question_binding".into());
        }
        let mut raw_sha256 = BTreeMap::new();
        for id in &task.document_scope {
            let digest = docs.get(id).ok_or("unknown_document_scope")?;
            if raw_sha256.insert(id.clone(), (*digest).clone()).is_some() {
                return Err("duplicate_scope".into());
            }
        }
        task_bindings.push(Binding {
            id: task.id.clone(),
            question_sha256: task.question_sha256.clone(),
            raw_sha256,
        });
    }
    let manifest = Manifest {
        schema_version: "1".into(),
        role: "reader".into(),
        corpus_sha256,
        reader_projection_sha256,
        reader_export_sha256: None,
        options_sha256,
        synthetic: true,
        executable: false,
        task_bindings,
        files: BTreeMap::new(),
    };
    Ok((corpus, readers, options, manifest))
}
fn prepare(
    corpus_path: &Path,
    readers_path: &Path,
    options_path: &Path,
    export: bool,
    limit: Option<usize>,
    task_ids: &[String],
) -> Result<(BTreeMap<String, Vec<u8>>, Manifest)> {
    let (corpus, readers, options, mut manifest) = inputs(corpus_path, readers_path, options_path)?;
    let root = corpus_path.parent().ok_or("corpus_parent")?;
    let mut admitted = BTreeMap::new();
    for document in &corpus.documents {
        let raw = asset(root, &document.raw)?;
        if !raw.starts_with(b"%PDF-") {
            return Err("raw_pdf_header".into());
        }
        admitted.insert(document.raw.path.clone(), raw);
        for derived in &document.prebuilt {
            if !derived.query_agnostic || derived.source_sha256 != document.raw.sha256 {
                return Err("prebuilt_source_binding".into());
            }
            let bytes = asset(root, &derived.asset)?;
            let receipt_bytes = asset(root, &derived.receipt)?;
            let receipt: DerivationReceipt =
                serde_json::from_slice(&receipt_bytes).map_err(|_| "derivation_receipt_schema")?;
            if receipt.schema_version != "1"
                || !receipt.synthetic
                || !receipt.query_agnostic
                || receipt.source_sha256 != derived.source_sha256
                || receipt.artifact_sha256 != derived.asset.sha256
                || receipt.derivation != derived.derivation
            {
                return Err("derivation_receipt_binding".into());
            }
            admitted.insert(derived.asset.path.clone(), bytes);
            admitted.insert(derived.receipt.path.clone(), receipt_bytes);
        }
    }
    admitted.insert(
        corpus.tool.asset.path.clone(),
        asset(root, &corpus.tool.asset)?,
    );
    let mut files = BTreeMap::new();
    let frozen_options = regular_bytes(options_path, MAX_JSON)?;
    if hash(&frozen_options) != manifest.options_sha256 {
        return Err("options_changed_during_admission".into());
    }
    put(
        &mut files,
        "frozen-options.json".into(),
        frozen_options.clone(),
    )?;
    let mut schedule = Vec::new();
    if limit == Some(0) || limit.is_some_and(|n| n > 64) {
        return Err("selection_limit".into());
    }
    let wanted: BTreeSet<_> = task_ids.iter().collect();
    if wanted.len() != task_ids.len()
        || wanted
            .iter()
            .any(|id| !readers.tasks.iter().any(|t| &t.id == *id))
    {
        return Err("selection_task_ids".into());
    }
    let selected: BTreeSet<_> = readers
        .tasks
        .iter()
        .filter(|t| wanted.is_empty() || wanted.contains(&t.id))
        .take(limit.unwrap_or(64))
        .map(|t| &t.id)
        .collect();
    manifest.task_bindings.retain(|b| selected.contains(&b.id));
    for (ordinal, task) in readers.tasks.iter().enumerate() {
        if !selected.contains(&task.id) {
            continue;
        }
        for repeat in 0..options.repetitions {
            let arms = if (ordinal as u32 + repeat + options.seed).is_multiple_of(2) {
                ["control", "gptgrep"]
            } else {
                ["gptgrep", "control"]
            };
            for (position, arm) in arms.iter().enumerate() {
                schedule.push(serde_json::json!({"task_id":task.id,"repeat":repeat,"arm":arm,"within_pair_order":position,"executed":false}));
            }
        }
        if !export {
            continue;
        }
        for arm in ["control", "gptgrep"] {
            let prefix = format!("{arm}/{}", task.id);
            let projection = serde_json::json!({"schema_version":"1","task_id":task.id,"question":task.question,
                "question_sha256":task.question_sha256,"document_scope":task.document_scope,"arm":arm});
            put(
                &mut files,
                format!("{prefix}/environment/reader-input.json"),
                json(&projection)?,
            )?;
            put(
                &mut files,
                format!("{prefix}/environment/codex-config.json"),
                frozen_options.clone(),
            )?;
            put(&mut files, format!("{prefix}/instruction.md"), format!("{}\n\nUse documents under /app/raw. Write only your final answer as {{\"answer\":\"...\"}} to /app/answer.json.\n", task.question).into_bytes())?;
            let mut docker = format!(
                "FROM {}\nWORKDIR /app\nCOPY raw/ /app/raw/\nCOPY reader-input.json codex-config.json /app/\n",
                options.reader_image
            );
            for id in &task.document_scope {
                let document = corpus
                    .documents
                    .iter()
                    .find(|d| &d.id == id)
                    .ok_or("scope")?;
                put(
                    &mut files,
                    format!("{prefix}/environment/raw/{id}.pdf"),
                    admitted[&document.raw.path].clone(),
                )?;
                if arm == "gptgrep" {
                    for (index, derived) in document.prebuilt.iter().enumerate() {
                        put(
                            &mut files,
                            format!("{prefix}/environment/prebuilt/{id}/{}", derived.asset.path),
                            admitted[&derived.asset.path].clone(),
                        )?;
                        put(
                            &mut files,
                            format!("{prefix}/environment/prebuilt/{id}/{index}.receipt.json"),
                            admitted[&derived.receipt.path].clone(),
                        )?;
                    }
                }
            }
            if arm == "gptgrep" {
                docker.push_str("COPY prebuilt/ /app/prebuilt/\nCOPY tool/ /app/tool/\nRUN chmod 0555 /app/tool/gptgrep\n");
                put(
                    &mut files,
                    format!("{prefix}/environment/tool/gptgrep"),
                    admitted[&corpus.tool.asset.path].clone(),
                )?;
                put(
                    &mut files,
                    format!("{prefix}/environment/tool/provenance.json"),
                    json(&corpus.tool)?,
                )?;
            }
            put(
                &mut files,
                format!("{prefix}/environment/Dockerfile"),
                docker.into_bytes(),
            )?;
            put(
                &mut files,
                format!("{prefix}/task-descriptor.json"),
                json(&serde_json::json!({"task_id":task.id,"arm":arm,"options":options}))?,
            )?;
        }
    }
    put(&mut files, "paired-schedule.json".into(), json(&schedule)?)?;
    manifest.role = if export { "reader" } else { "plan" }.into();
    manifest.files = files.iter().map(|(p, b)| (p.clone(), hash(b))).collect();
    Ok((files, manifest))
}
fn inventory(
    root: &Path,
    prefix: &str,
    found: &mut BTreeMap<String, String>,
    nodes: &mut usize,
    total: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        *nodes += 1;
        if *nodes > 4096 || prefix.split('/').count() > 16 {
            return Err("output_tree_limit".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or("non_utf8_output")?
            .to_string();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        check_rel(&rel)?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("output_symlink".into());
        }
        if kind.is_dir() {
            inventory(&entry.path(), &rel, found, nodes, total)?;
        } else if kind.is_file() && rel != "export-manifest.json" {
            let bytes = regular_bytes(&entry.path(), MAX_FILE)?;
            *total += bytes.len();
            if *total > MAX_OUTPUT {
                return Err("output_size_limit".into());
            }
            found.insert(rel, hash(&bytes));
        } else if !kind.is_file() {
            return Err("output_special_file".into());
        }
        if found.len() > 4096 {
            return Err("output_file_limit".into());
        }
    }
    Ok(())
}
fn validate(root: &Path) -> Result<Manifest> {
    let (manifest, _): (Manifest, _) = load(&root.join("export-manifest.json"))?;
    if manifest.schema_version != "1"
        || manifest.executable
        || !manifest.synthetic
        || !["reader", "grader", "plan"].contains(&manifest.role.as_str())
    {
        return Err("export_status".into());
    }
    match (manifest.role.as_str(), &manifest.reader_export_sha256) {
        ("grader", Some(expected)) => check_hash(expected)?,
        ("reader" | "plan", None) => {}
        _ => return Err("export_reader_binding".into()),
    }
    let mut found = BTreeMap::new();
    inventory(root, "", &mut found, &mut 0, &mut 0)?;
    if found != manifest.files {
        return Err("export_inventory_mismatch".into());
    }
    if manifest.role == "reader" {
        let options = regular_bytes(&root.join("frozen-options.json"), MAX_JSON)?;
        if hash(&options) != manifest.options_sha256 {
            return Err("options_digest_binding".into());
        }
        for binding in &manifest.task_bindings {
            for arm in ["control", "gptgrep"] {
                let base = root.join(arm).join(&binding.id);
                if regular_bytes(&base.join("environment/codex-config.json"), MAX_JSON)? != options
                {
                    return Err("codex_arm_config_mismatch".into());
                }
                let value: serde_json::Value = serde_json::from_slice(&regular_bytes(
                    &base.join("environment/reader-input.json"),
                    MAX_JSON,
                )?)
                .map_err(|_| "reader_input_schema")?;
                if value["task_id"] != binding.id
                    || value["arm"] != arm
                    || value["question_sha256"] != binding.question_sha256
                    || hash(
                        value["question"]
                            .as_str()
                            .ok_or("question_required")?
                            .as_bytes(),
                    ) != binding.question_sha256
                {
                    return Err("reader_input_binding".into());
                }
                for (id, sha) in &binding.raw_sha256 {
                    if hash(&regular_bytes(
                        &base.join(format!("environment/raw/{id}.pdf")),
                        MAX_FILE,
                    )?) != *sha
                    {
                        return Err("mounted_raw_digest".into());
                    }
                }
            }
        }
    }
    Ok(manifest)
}
fn verifier(
    graders_path: &Path,
    reader_root: &Path,
) -> Result<(BTreeMap<String, Vec<u8>>, Manifest)> {
    let mut manifest = validate(reader_root)?;
    if manifest.role != "reader" {
        return Err("reader_export_role".into());
    }
    let (graders, _): (Graders, _) = load(graders_path)?;
    check_hash(&graders.rubric_sha256)?;
    check_hash(&graders.reader_export_sha256)?;
    if graders.schema_version != "1"
        || graders.corpus_sha256 != manifest.corpus_sha256
        || graders.reader_projection_sha256 != manifest.reader_projection_sha256
        || graders.reader_export_sha256
            != hash(&regular_bytes(
                &reader_root.join("export-manifest.json"),
                MAX_JSON,
            )?)
        || graders.judge_status != "unavailable"
        || graders.tasks.len() != manifest.task_bindings.len()
    {
        return Err("grader_binding_or_qualification".into());
    }
    let mut files = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for grade in &graders.tasks {
        let binding = manifest
            .task_bindings
            .iter()
            .find(|b| b.id == grade.id)
            .ok_or("grader_task_id")?;
        if !seen.insert(&grade.id)
            || grade.question_sha256 != binding.question_sha256
            || grade.reference_answer.is_empty()
            || grade.reference_answer.len() > 12000
        {
            return Err("grader_task_binding".into());
        }
        put(
            &mut files,
            format!("{}/reference.json", grade.id),
            json(
                &serde_json::json!({"reference_answer":grade.reference_answer,
            "rubric_sha256":graders.rubric_sha256,"judge_status":"unavailable","reward_rule":"original_answer_equivalence_only"}),
            )?,
        )?;
    }
    manifest.role = "grader".into();
    manifest.reader_export_sha256 = Some(graders.reader_export_sha256);
    manifest.files = files.iter().map(|(p, b)| (p.clone(), hash(b))).collect();
    Ok((files, manifest))
}
fn run(command: Command) -> Result<()> {
    match command {
        Command::ProjectCohort {
            questions,
            questions_sha256,
            sources_lock,
            sources_lock_sha256,
            mapping,
            mapping_sha256,
            output_dir,
        } => production_export::cohort_projection::project(
            &questions,
            &questions_sha256,
            &sources_lock,
            &sources_lock_sha256,
            &mapping,
            &mapping_sha256,
            &output_dir,
        )?,
        Command::BindCohortGraders {
            cohort,
            cohort_sha256,
            batch,
            reader_export,
            reader_export_sha256,
            output_dir,
        } => production_export::cohort_projection::bind(
            &cohort,
            &cohort_sha256,
            &batch,
            &reader_export,
            &reader_export_sha256,
            &output_dir,
        )?,
        Command::ReportPaired {
            input,
            input_sha256,
        } => {
            check_hash(&input_sha256)?;
            let bytes = regular_bytes(&input, MAX_JSON)?;
            if hash(&bytes) != input_sha256 {
                return Err("paired_input_digest_mismatch".into());
            }
            let mut report = report_metrics::report(&bytes)?;
            report["input_sha256"] = serde_json::Value::String(input_sha256);
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
        }
        Command::PlanProduction {
            corpus,
            corpus_sha256,
            readers,
            readers_sha256,
            options,
            options_sha256,
            output_dir,
        } => {
            production_export::prepare_bound(
                &corpus,
                &readers,
                &options,
                &corpus_sha256,
                &readers_sha256,
                &options_sha256,
                false,
            )?
            .publish(&output_dir)?;
        }
        Command::ExportProductionReaders {
            corpus,
            corpus_sha256,
            readers,
            readers_sha256,
            options,
            options_sha256,
            output_dir,
        } => {
            production_export::prepare_bound(
                &corpus,
                &readers,
                &options,
                &corpus_sha256,
                &readers_sha256,
                &options_sha256,
                true,
            )?
            .publish(&output_dir)?;
        }
        Command::ExportProductionVerifier {
            graders,
            reader_export,
            output_dir,
        } => {
            production_export::verifier(&graders, &reader_export)?.publish(&output_dir)?;
        }
        Command::Plan {
            corpus,
            readers,
            options,
            output_dir,
            limit,
            task_ids,
        } => {
            let (f, m) = prepare(&corpus, &readers, &options, false, limit, &task_ids)?;
            create_output(&output_dir, &f, &m)?;
        }
        Command::ExportReaders {
            corpus,
            readers,
            options,
            output_dir,
            limit,
            task_ids,
        } => {
            let (f, m) = prepare(&corpus, &readers, &options, true, limit, &task_ids)?;
            create_output(&output_dir, &f, &m)?;
        }
        Command::ExportVerifier {
            graders,
            reader_export,
            output_dir,
        } => {
            let (f, m) = verifier(&graders, &reader_export)?;
            create_output(&output_dir, &f, &m)?;
        }
        Command::Validate { export_dir } => {
            let bytes = regular_bytes(&export_dir.join("export-manifest.json"), MAX_JSON)?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if value["schema_version"] == production_export::EXPORT_SCHEMA {
                production_export::validate(&export_dir)?;
                println!(
                    "{}",
                    serde_json::json!({"valid":true,"scope":"package_integrity_only_not_source_adoption","executable":false})
                );
                return Ok(());
            }
            let m = validate(&export_dir)?;
            println!(
                "{}",
                String::from_utf8(json(
                    &serde_json::json!({"valid":true,"role":m.role,"executable":false})
                )?)
                .map_err(|e| e.to_string())?
            );
        }
    }
    Ok(())
}
fn main() {
    if let Err(error) = run(Cli::parse().command) {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

mod production_export;
mod report_metrics;
#[cfg(test)]
mod tests;
