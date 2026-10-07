//! Trigram candidate retrieval and exact matching over an owned text snapshot.
//!
//! The caller supplies sibling text/index directories in an unpublished generation,
//! then publishes the generation only after `build` succeeds. These directories must
//! remain immutable. Current original-document freshness is the caller's concern.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tgrep_core::builder::{self, BuildOptions};
use tgrep_core::encoding;
use tgrep_core::meta::{self, ContentId, IndexMeta};
use tgrep_core::path_index;
use tgrep_core::query;
use tgrep_core::reader::IndexReader;

const ENGINE_REVISION: &str = "1120aca41dd192ae61bdd996e9886cb354a78a27";
const MANIFEST_NAME: &str = "gptgrep-index.json";
const MANIFEST_VERSION: u32 = 1;
const INDEX_ARTIFACTS: [&str; 7] = [
    MANIFEST_NAME,
    "meta.json",
    "files-extra.bin",
    "filestamps.json",
    "files.bin",
    "lookup.bin",
    "index.bin",
];

/// Counts from a completely built text snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStats {
    /// Number of normalized text files admitted to the index.
    pub files: usize,
    /// Original bytes in those normalized files, before BOM decoding.
    pub bytes: u64,
}

/// One verified matching line. Paths are relative to the normalized text root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Match {
    pub path: PathBuf,
    pub line_number: usize,
    /// UTF-8 text without a trailing newline.
    pub line: String,
}

/// Exact matches plus actual candidate/verification counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<Match>,
    pub candidate_files: usize,
    pub verified_files: usize,
    /// True only after observing another matching line beyond the limit.
    pub truncated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    engine_revision: String,
    complete: bool,
    /// Exactly one sibling directory: `../text`, for example.
    root_relative: PathBuf,
    file_table_id: String,
    files: BTreeMap<String, FileVersion>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileVersion {
    bytes: u64,
    sha256: String,
    /// tgrep strips a UTF-8 BOM; original-text verification must still see it.
    has_bom: bool,
}

/// Build into an empty index directory beside an immutable normalized-text root.
///
/// A complete GPTgrep manifest is written last. Indexing does not start a daemon,
/// modify the text corpus, or publish the containing generation.
pub fn build(root: &Path, index_dir: &Path) -> Result<BuildStats> {
    let root = root
        .canonicalize()
        .context("resolve normalized text root")?;
    ensure!(root.is_dir(), "normalized text root is not a directory");
    fs::create_dir_all(index_dir).context("create trigram index directory")?;
    let index_dir = index_dir
        .canonicalize()
        .context("resolve index directory")?;
    ensure!(
        root != index_dir,
        "text root and index directory must differ"
    );
    ensure!(
        root.parent() == index_dir.parent(),
        "text root and index directory must be siblings in one generation"
    );
    ensure!(
        fs::read_dir(&index_dir)?.next().is_none(),
        "index directory must be empty; build a new immutable generation"
    );

    // Ignore and size policies belong to preprocessing. This directory contains
    // only the admitted normalized text, so no second cap or ignore rule applies.
    builder::build_index_with_options(
        &root,
        Some(&index_dir),
        &BuildOptions {
            include_hidden: true,
            no_ignore: true,
            no_require_git: true,
            max_file_size: None,
            ..Default::default()
        },
    )
    .context("build tgrep index")?;
    let reader = open_complete_index(&index_dir)?;
    let evidence = meta::read_file_evidence(&index_dir)?;
    let mut files = BTreeMap::new();
    let mut bytes = 0_u64;
    for rel in reader.all_paths() {
        let data = read_owned_file(&root, rel)?;
        std::str::from_utf8(&data)
            .with_context(|| format!("normalized file is not UTF-8: {rel}"))?;
        let decoded = encoding::decode_for_index(&data);
        ensure!(
            evidence.content_id(rel) == Some(ContentId::from_indexed_bytes(&decoded)),
            "normalized file changed while the index was being built: {rel}"
        );
        bytes += data.len() as u64;
        files.insert(
            rel.clone(),
            FileVersion {
                bytes: data.len() as u64,
                sha256: sha256(&data),
                has_bom: data.starts_with(b"\xef\xbb\xbf"),
            },
        );
    }
    let stats = BuildStats {
        files: files.len(),
        bytes,
    };
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        engine_revision: ENGINE_REVISION.into(),
        complete: true,
        root_relative: Path::new("..").join(root.file_name().context("text root has no name")?),
        file_table_id: hex(&reader.file_table_id()),
        files,
    };
    // The generation is still private to the caller. A failed/truncated write
    // remains unusable because search requires a valid, complete manifest.
    fs::write(
        index_dir.join(MANIFEST_NAME),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .context("write completed GPTgrep index manifest")?;
    Ok(stats)
}

/// Retrieve candidates, verify their snapshot digests, and match their text.
///
/// A missing or changed candidate is an error. This does not claim that original
/// source documents are current; the immutable snapshot's owner validates those.
pub fn search(
    index_dir: &Path,
    pattern: &str,
    case_insensitive: bool,
    literal: bool,
    limit: usize,
) -> Result<SearchResult> {
    search_with_document(index_dir, pattern, case_insensitive, literal, limit, None)
}

/// Search an optional exact normalized-text document path before verification
/// and result limits. This path is relative to the immutable text root; GPTgrep
/// core maps a source document to its internal `<document-id>.txt` filename.
/// Unknown or non-normalized paths fail instead of silently producing no hits.
pub fn search_with_document(
    index_dir: &Path,
    pattern: &str,
    case_insensitive: bool,
    literal: bool,
    limit: usize,
    document: Option<&Path>,
) -> Result<SearchResult> {
    search_with_document_and_predicate(
        index_dir,
        pattern,
        case_insensitive,
        literal,
        limit,
        document,
        |_| Ok(true),
    )
}

/// Admit candidates before opening their snapshot text or consuming a match
/// budget. The predicate runs once per visited, deduplicated candidate after
/// trigram and document-scope filtering. Returning false skips that candidate;
/// errors propagate. It does not eagerly visit the entire indexed corpus.
/// `candidate_files` counts scoped trigram candidates; `verified_files` counts
/// admitted files actually verified against their snapshot digest.
pub fn search_with_document_and_predicate(
    index_dir: &Path,
    pattern: &str,
    case_insensitive: bool,
    literal: bool,
    limit: usize,
    document: Option<&Path>,
    accept: impl FnMut(&Path) -> Result<bool>,
) -> Result<SearchResult> {
    ensure!(limit > 0, "search limit must be greater than zero");
    IndexSession::open(index_dir)?.search_with_document_and_predicate(
        pattern,
        case_insensitive,
        literal,
        limit,
        document,
        accept,
    )
}

/// A validated index and file table pinned to one immutable text generation.
///
/// Repeated searches reuse the open tgrep reader. Candidate text is still read
/// and checked against its digest on every search; this session never treats a
/// previously returned match as evidence that a changed file is valid.
/// The owner must retain the generation and keep index files immutable.
pub struct IndexSession {
    dir: PathBuf,
    root: PathBuf,
    manifest: Manifest,
    reader: IndexReader,
    artifacts: Vec<(u64, String)>,
}

impl IndexSession {
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index_dir = index_dir
            .canonicalize()
            .context("resolve index directory")?;
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(index_dir.join(MANIFEST_NAME)).context("read GPTgrep index manifest")?,
        )
        .context("invalid GPTgrep index manifest")?;
        ensure!(
            manifest.complete
                && manifest.version == MANIFEST_VERSION
                && manifest.engine_revision == ENGINE_REVISION,
            "incomplete or incompatible GPTgrep index manifest; rebuild_required: build a new generation with the current engine"
        );
        let artifacts = INDEX_ARTIFACTS
            .iter()
            .map(|name| artifact_digest(&index_dir.join(name)))
            .collect::<Result<Vec<_>>>()?;
        let root = resolve_root(&index_dir, &manifest.root_relative)?;
        let reader = open_complete_index(&index_dir)?;
        ensure!(
            manifest.file_table_id == hex(&reader.file_table_id())
                && manifest.files.len() == reader.num_files()
                && reader
                    .all_paths()
                    .iter()
                    .all(|rel| manifest.files.contains_key(rel)),
            "GPTgrep manifest does not match the index file table"
        );
        let session = Self {
            dir: index_dir,
            root,
            manifest,
            reader,
            artifacts,
        };
        session.validate()?;
        Ok(session)
    }

    /// Recheck the retained artifacts without reopening the mapped reader.
    /// These streaming reads count as physical reads, even on a warm OS cache.
    /// The generation owner must prevent concurrent in-place writes to mapped
    /// files; validation detects corruption between operations, not hostile
    /// changes racing a memory-mapped access.
    pub fn validate(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.dir)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "index directory became a symlink or is not a directory"
        );
        ensure!(
            resolve_root(&self.dir, &self.manifest.root_relative)? == self.root,
            "normalized text root changed"
        );
        for (name, expected) in INDEX_ARTIFACTS.iter().zip(&self.artifacts) {
            ensure!(
                &artifact_digest(&self.dir.join(name))? == expected,
                "retained index artifact changed: {name}"
            );
        }
        Ok(())
    }

    pub fn search(
        &self,
        pattern: &str,
        case_insensitive: bool,
        literal: bool,
        limit: usize,
    ) -> Result<SearchResult> {
        self.search_with_document(pattern, case_insensitive, literal, limit, None)
    }

    pub fn search_with_document(
        &self,
        pattern: &str,
        case_insensitive: bool,
        literal: bool,
        limit: usize,
        document: Option<&Path>,
    ) -> Result<SearchResult> {
        self.search_with_document_and_predicate(
            pattern,
            case_insensitive,
            literal,
            limit,
            document,
            |_| Ok(true),
        )
    }

    pub fn search_with_document_and_predicate(
        &self,
        pattern: &str,
        case_insensitive: bool,
        literal: bool,
        limit: usize,
        document: Option<&Path>,
        mut accept: impl FnMut(&Path) -> Result<bool>,
    ) -> Result<SearchResult> {
        ensure!(limit > 0, "search limit must be greater than zero");
        self.validate()?;
        let root = &self.root;
        let manifest = &self.manifest;
        let reader = &self.reader;
        let document = document.map(normalized_document_path).transpose()?;
        if let Some(document) = document {
            ensure!(
                manifest.files.contains_key(document),
                "unknown indexed document: {document}"
            );
        }

        let escaped = if literal {
            regex::escape(pattern)
        } else {
            pattern.into()
        };
        // tgrep's lowercased postings cover ASCII, while regex case folding includes
        // Unicode (e.g. K/kelvin sign and S/long-s). Let regex-syntax expand the SAME
        // regex used below; its character classes conservatively yield MatchAll.
        let source = if case_insensitive {
            format!("(?i:{escaped})")
        } else {
            escaped
        };
        let matcher = Regex::new(&source).context("invalid search regex")?;
        let plan = query::build_query_plan(&source, false).map_err(anyhow::Error::msg)?;
        let ids = if plan.is_match_all() {
            reader.all_file_ids()
        } else {
            query::execute_plan_with_masks(&plan, &|tri| reader.lookup_trigram_with_masks(tri))
        };
        let mut paths = ids
            .into_iter()
            .map(|id| {
                reader
                    .file_path(id)
                    .context("index returned an unknown file ID")
            })
            .collect::<Result<Vec<_>>>()?;
        // A literal/escaped BOM cannot occur in tgrep's decoded postings. Include
        // these files conservatively, then verify the original UTF-8 text below.
        paths.extend(
            manifest
                .files
                .iter()
                .filter(|(_, version)| version.has_bom)
                .map(|(rel, _)| rel.as_str()),
        );
        paths.sort_unstable();
        paths.dedup();
        if let Some(document) = document {
            // Filter before opening/verifying candidate files or counting matches.
            // Unrelated matches must not consume this document's result budget.
            paths.retain(|relative| *relative == document);
        }
        let mut result = SearchResult {
            hits: Vec::new(),
            candidate_files: paths.len(),
            verified_files: 0,
            truncated: false,
        };
        for rel in paths {
            if !accept(Path::new(rel))? {
                continue;
            }
            let version = manifest
                .files
                .get(rel)
                .context("candidate missing from manifest")?;
            let data = read_owned_file(root, rel)?;
            ensure!(
                data.len() as u64 == version.bytes && sha256(&data) == version.sha256,
                "normalized snapshot file changed after indexing: {rel}"
            );
            let text = std::str::from_utf8(&data)
                .with_context(|| format!("normalized snapshot is not UTF-8: {rel}"))?;
            result.verified_files += 1;
            for (index, line) in text.lines().enumerate() {
                if matcher.is_match(line) {
                    if result.hits.len() == limit {
                        result.truncated = true;
                        return Ok(result);
                    }
                    result.hits.push(Match {
                        path: PathBuf::from(rel),
                        line_number: index + 1,
                        line: line.into(),
                    });
                }
            }
        }
        Ok(result)
    }
}

fn artifact_digest(path: &Path) -> Result<(u64, String)> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("retained index artifact missing: {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "retained index artifact is not an owned regular file: {}",
        path.display()
    );
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 16384];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read = read
            .checked_add(count as u64)
            .context("index artifact length overflow")?;
        ensure!(
            read <= metadata.len(),
            "index artifact grew during validation"
        );
        hash.update(&buffer[..count]);
    }
    ensure!(
        read == metadata.len(),
        "index artifact length changed during validation"
    );
    Ok((read, hex(&hash.finalize())))
}

fn normalized_document_path(document: &Path) -> Result<&str> {
    let relative = document.to_str().context("document scope must be UTF-8")?;
    let normalized: PathBuf = document.components().collect();
    ensure!(
        !relative.is_empty()
            && relative.len() <= 4096
            && !relative.contains('\0')
            && document
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            && normalized.as_os_str() == document.as_os_str(),
        "document scope must be a normalized relative indexed path"
    );
    Ok(relative)
}

fn open_complete_index(index_dir: &Path) -> Result<IndexReader> {
    let meta = IndexMeta::load(index_dir).context("read tgrep generation metadata")?;
    let reader = IndexReader::open(index_dir).context("open tgrep generation")?;
    reader.validate_lookup().map_err(anyhow::Error::msg)?;
    let filenames = path_index::read_filename_index(index_dir)?
        .context("index is missing filename coverage")?;
    let visibility = filenames
        .visibility
        .context("index has legacy filename coverage")?;
    ensure!(
        visibility.covers_index(&meta, reader.file_table_id()),
        "tgrep generation has incomplete or inconsistent coverage"
    );
    ensure!(
        filenames.paths.is_empty(),
        "normalized corpus contains files without searchable text"
    );
    Ok(reader)
}

fn resolve_root(index_dir: &Path, relative: &Path) -> Result<PathBuf> {
    let parts = relative.components().collect::<Vec<_>>();
    ensure!(
        matches!(
            parts.as_slice(),
            [Component::ParentDir, Component::Normal(_)]
        ),
        "normalized text root must be one sibling directory"
    );
    let root = index_dir.join(relative);
    let metadata = fs::symlink_metadata(&root).context("normalized text root is missing")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "invalid text root"
    );
    let root = root.canonicalize()?;
    ensure!(
        root.parent() == index_dir.parent() && root != index_dir,
        "text root escaped generation"
    );
    Ok(root)
}

fn read_owned_file(root: &Path, rel: &str) -> Result<Vec<u8>> {
    let relative = Path::new(rel);
    ensure!(
        !rel.is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "index contains an invalid relative path"
    );
    let mut full = root.to_path_buf();
    for part in relative.components() {
        full.push(part);
        let metadata = fs::symlink_metadata(&full)
            .with_context(|| format!("normalized snapshot path is missing: {rel}"))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "snapshot path became a symlink: {rel}"
        );
    }
    ensure!(
        full.is_file(),
        "normalized snapshot entry is not a file: {rel}"
    );
    fs::read(&full).with_context(|| format!("read normalized snapshot file: {rel}"))
}

fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|value| format!("{value:02x}")).collect()
}
