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
use tgrep_core::trigram;

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

/// Explicit admission bounds for a complete content verification. These bound
/// inputs, scan counts and conservative working allocations, not process RSS.
/// Files must remain immutable, including during upstream memory-mapped reads.
#[derive(Debug, Clone, Copy)]
pub struct IndexContentLimits {
    pub max_files: u32,
    pub max_control_bytes: u64,
    pub max_index_bytes: u64,
    pub max_file_bytes: u64,
    pub max_total_text_bytes: u64,
    pub max_trigrams: u64,
    pub max_postings: u64,
    pub max_work_bytes: u64,
}

/// Actual counts from an exhaustive, source-derived content comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexContentReport {
    pub files: u32,
    pub text_bytes: u64,
    pub trigrams: u64,
    pub postings: u64,
}

// Exact v3 wire layout in ENGINE_REVISION; these constants are private upstream.
const POSTING_BYTES: usize = 6;
const LOOKUP_BYTES: u64 = 16;
const FILE_TABLE_HEADER: &[u8; 12] = b"\xff\xff\xff\xff\0\0\xff\xff\xff\xff\0\0";

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
    /// Prove the complete engine's postings (IDs, location masks and next-byte
    /// masks) are derived from its actual normalized UTF-8 text. Editable file
    /// metadata or a caller-supplied index digest alone does not prove this.
    ///
    /// Admission precedes the upstream constructor and each mask extraction.
    /// One file's masks are retained at a time. No search/model/process starts.
    /// Named-source and index snapshots are rechecked before returning, but
    /// concurrent hostile in-place mmap writes remain outside this contract.
    pub fn verify_content(
        index_dir: &Path,
        limits: &IndexContentLimits,
    ) -> Result<IndexContentReport> {
        let admission = admit_content(index_dir, limits)?;
        let session = Self::open(index_dir)?;
        ensure!(
            session.reader.num_files() <= limits.max_files as usize
                && session.reader.num_trigrams() as u64 == admission.trigrams,
            "index content constructor counts changed"
        );
        let mut total_text = 0_u64;
        let mut largest_file = 0_u64;
        // Reserve every source size before reading any source or extracting masks.
        for (rel, version) in &session.manifest.files {
            let path = owned_file_path(&session.root, rel)?;
            let bytes = fs::symlink_metadata(&path)?.len();
            ensure!(
                bytes == version.bytes && bytes <= limits.max_file_bytes,
                "index content file size limit or changed source"
            );
            total_text = total_text
                .checked_add(bytes)
                .context("text size overflow")?;
            largest_file = largest_file.max(bytes);
        }
        ensure!(
            total_text <= limits.max_total_text_bytes,
            "index content total text limit"
        );
        // UTF-8 normalization means decode_for_index only borrows/strips a UTF-8
        // BOM. At most two distinct grams per byte window (original + lowercase).
        // 128 bytes per possible entry conservatively reserves the pinned map's
        // entries, growth/spare capacity and traversal overhead; not an RSS claim.
        let mask_slots = largest_file
            .saturating_sub(2)
            .checked_mul(2)
            .context("mask capacity overflow")?;
        let work = admission
            .control_bytes
            .checked_mul(16)
            .and_then(|n| n.checked_add(u64::from(limits.max_files) * 512))
            .and_then(|n| n.checked_add(largest_file))
            .and_then(|n| n.checked_add(mask_slots.checked_mul(128)?))
            .context("index content working reservation overflow")?;
        ensure!(work <= limits.max_work_bytes, "index content working limit");

        // Validate borrowed posting ranges before any per-entry materialization.
        // Strict IDs and exact membership counts prohibit duplicate/extra rows.
        let mut actual_postings = 0_u64;
        let mut next_pointer = None;
        for i in 0..session.reader.num_trigrams() {
            let (tri, raw) = session
                .reader
                .nth_trigram_raw(i)
                .context("index content posting range")?;
            ensure!(
                tri <= 0x00ff_ffff && !raw.is_empty() && raw.len() % POSTING_BYTES == 0,
                "index content posting format"
            );
            if let Some(expected) = next_pointer {
                ensure!(
                    raw.as_ptr() as usize == expected,
                    "index content posting layout"
                );
            }
            next_pointer = Some(
                (raw.as_ptr() as usize)
                    .checked_add(raw.len())
                    .context("posting pointer overflow")?,
            );
            let rows = (raw.len() / POSTING_BYTES) as u64;
            ensure!(
                rows <= session.reader.num_files() as u64,
                "index content row limit"
            );
            actual_postings = actual_postings
                .checked_add(rows)
                .context("posting count overflow")?;
            ensure!(
                actual_postings <= limits.max_postings,
                "index content posting limit"
            );
            let mut previous = None;
            for row in raw.chunks_exact(POSTING_BYTES) {
                let id = u32::from_le_bytes(row[..4].try_into()?);
                ensure!(
                    session.reader.file_path(id).is_some()
                        && previous.is_none_or(|p| id > p)
                        && row[4] != 0,
                    "index content posting IDs or masks"
                );
                previous = Some(id);
            }
        }
        ensure!(
            actual_postings == admission.postings,
            "index content unreferenced postings"
        );

        let mut expected_postings = 0_u64;
        for (file_id, rel) in session.reader.all_paths().iter().enumerate() {
            let version = &session.manifest.files[rel];
            let path = owned_file_path(&session.root, rel)?;
            let data = read_limited(&path, version.bytes)?;
            ensure!(
                data.len() as u64 == version.bytes
                    && sha256(&data) == version.sha256
                    && data.starts_with(b"\xef\xbb\xbf") == version.has_bom,
                "index content source digest or BOM changed"
            );
            std::str::from_utf8(&data).context("index content source is not normalized UTF-8")?;
            let decoded = encoding::decode_for_index(&data);
            ensure!(
                !trigram::is_binary(&decoded),
                "index content indexed binary source"
            );
            let expected = trigram::extract_merged_masks(&decoded);
            expected_postings = expected_postings
                .checked_add(expected.len() as u64)
                .context("expected posting count overflow")?;
            ensure!(
                expected_postings <= limits.max_postings,
                "index content expected posting limit"
            );
            for (tri, masks) in expected {
                let raw =
                    posting_for(&session.reader, tri).context("index content missing trigram")?;
                // The first pass proved sorted unique in-range IDs. Binary-search
                // the borrowed rows without allocating a whole posting vector.
                let row = posting_row(raw, file_id as u32)
                    .context("index content missing file posting")?;
                ensure!(
                    row[4] == masks.loc_mask && row[5] == masks.next_mask,
                    "index content posting mask mismatch"
                );
            }
        }
        ensure!(
            expected_postings == actual_postings,
            "index content extra postings"
        );
        for (rel, version) in &session.manifest.files {
            ensure!(
                artifact_digest(&owned_file_path(&session.root, rel)?)?
                    == (version.bytes, version.sha256.clone()),
                "index content named source changed"
            );
        }
        session.validate()?;
        // Re-admit named sizes/counts after all work; upstream artifacts remain
        // under the immutable-generation owner's custody throughout this call.
        ensure!(
            admit_content(index_dir, limits)? == admission,
            "index content admission changed"
        );
        Ok(IndexContentReport {
            files: session.reader.num_files() as u32,
            text_bytes: total_text,
            trigrams: admission.trigrams,
            postings: actual_postings,
        })
    }

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
        // Dense unique file IDs do not imply unique source paths. A duplicate
        // path could otherwise substitute A twice while omitting declared B.
        // This borrowed path vector is bounded by the admitted file count in
        // verify_content; compare the whole sorted set, not membership alone.
        let mut paths = reader
            .all_paths()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        paths.sort_unstable();
        ensure!(
            manifest.file_table_id == hex(&reader.file_table_id())
                && manifest.files.len() == reader.num_files()
                && paths.windows(2).all(|pair| pair[0] != pair[1])
                && paths
                    .into_iter()
                    .eq(manifest.files.keys().map(String::as_str)),
            "GPTgrep manifest does not match the unique index file table source set"
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

#[derive(Debug, PartialEq, Eq)]
struct ContentAdmission {
    control_bytes: u64,
    trigrams: u64,
    postings: u64,
}

fn admit_content(index_dir: &Path, limits: &IndexContentLimits) -> Result<ContentAdmission> {
    ensure!(
        limits.max_control_bytes > 0
            && limits.max_index_bytes > 0
            && limits.max_file_bytes > 0
            && limits.max_work_bytes > 0
            && limits.max_file_bytes < usize::MAX as u64
            && limits.max_control_bytes < usize::MAX as u64,
        "invalid index content limits"
    );
    let directory = fs::symlink_metadata(index_dir)?;
    ensure!(
        directory.is_dir() && !directory.file_type().is_symlink(),
        "index content directory custody"
    );
    let mut total = 0_u64;
    let mut control_bytes = 0_u64;
    let mut trigrams = 0;
    let mut postings = 0;
    for name in INDEX_ARTIFACTS {
        let metadata = fs::symlink_metadata(index_dir.join(name))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "index content artifact custody"
        );
        let size = metadata.len();
        total = total
            .checked_add(size)
            .context("index content size overflow")?;
        ensure!(
            total <= limits.max_index_bytes,
            "index content total index limit"
        );
        match name {
            "lookup.bin" => {
                ensure!(size % LOOKUP_BYTES == 0, "index content lookup format");
                trigrams = size / LOOKUP_BYTES;
                ensure!(
                    trigrams <= limits.max_trigrams,
                    "index content trigram limit"
                );
            }
            "index.bin" => {
                ensure!(
                    size % POSTING_BYTES as u64 == 0,
                    "index content posting format"
                );
                postings = size / POSTING_BYTES as u64;
                ensure!(
                    postings <= limits.max_postings,
                    "index content posting limit"
                );
            }
            _ => {
                ensure!(
                    size <= limits.max_control_bytes,
                    "index content control limit"
                );
                control_bytes = control_bytes
                    .checked_add(size)
                    .context("index control size overflow")?;
            }
        }
    }
    let base_work = control_bytes
        .checked_mul(16)
        .and_then(|n| n.checked_add(u64::from(limits.max_files) * 512))
        .context("index control working overflow")?;
    ensure!(
        base_work <= limits.max_work_bytes,
        "index content working limit"
    );
    let manifest: Manifest = serde_json::from_slice(&read_limited(
        &index_dir.join(MANIFEST_NAME),
        limits.max_control_bytes,
    )?)?;
    ensure!(
        manifest.files.len() <= limits.max_files as usize,
        "index content file count limit"
    );
    let mut text_bytes = 0_u64;
    let mut largest = 0_u64;
    for (rel, version) in &manifest.files {
        normalized_document_path(Path::new(rel))?;
        ensure!(
            version.bytes <= limits.max_file_bytes,
            "index content file size limit"
        );
        text_bytes = text_bytes
            .checked_add(version.bytes)
            .context("index text size overflow")?;
        largest = largest.max(version.bytes);
    }
    ensure!(
        text_bytes <= limits.max_total_text_bytes,
        "index content total text limit"
    );
    let mask_work = largest
        .saturating_sub(2)
        .checked_mul(2)
        .and_then(|n| n.checked_mul(128))
        .and_then(|n| n.checked_add(largest))
        .and_then(|n| n.checked_add(base_work))
        .context("index mask working overflow")?;
    ensure!(
        mask_work <= limits.max_work_bytes,
        "index content working limit"
    );
    let meta: IndexMeta = serde_json::from_slice(&read_limited(
        &index_dir.join("meta.json"),
        limits.max_control_bytes,
    )?)?;
    ensure!(
        meta.num_files <= u64::from(limits.max_files) && meta.num_trigrams == trigrams,
        "index content metadata counts"
    );
    // Count bounded file-table records before IndexReader allocates path vectors.
    let table = read_limited(&index_dir.join("files.bin"), limits.max_control_bytes)?;
    ensure!(
        table.starts_with(FILE_TABLE_HEADER) && table.get(12..16) == Some(&3u32.to_le_bytes()),
        "index content file table version"
    );
    let mut pos = 16_usize;
    let mut files = 0_u32;
    while pos < table.len() {
        let header = table
            .get(pos..pos.checked_add(6).context("file table overflow")?)
            .context("index content truncated file table")?;
        let len = u16::from_le_bytes(header[4..6].try_into()?) as usize;
        ensure!(len > 0 && len <= 4096, "index content path limit");
        pos += 6;
        let end = pos.checked_add(len).context("file table path overflow")?;
        let path = std::str::from_utf8(
            table
                .get(pos..end)
                .context("index content truncated path")?,
        )?;
        normalized_document_path(Path::new(path))?;
        pos = end;
        files = files.checked_add(1).context("file count overflow")?;
        ensure!(files <= limits.max_files, "index content file count limit");
    }
    ensure!(
        files as usize == manifest.files.len() && u64::from(files) == meta.num_files,
        "index content file counts differ"
    );
    Ok(ContentAdmission {
        control_bytes,
        trigrams,
        postings,
    })
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit,
        "index content bounded read custody or limit"
    );
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() == metadata.len(),
        "index content read size changed"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    file.take(
        metadata
            .len()
            .checked_add(1)
            .context("read size overflow")?,
    )
    .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "index content read length changed"
    );
    Ok(bytes)
}

fn posting_for(reader: &IndexReader, trigram: u32) -> Option<&[u8]> {
    let mut low = 0;
    let mut high = reader.num_trigrams();
    while low < high {
        let mid = low + (high - low) / 2;
        let (value, raw) = reader.nth_trigram_raw(mid)?;
        match value.cmp(&trigram) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => return Some(raw),
        }
    }
    None
}

fn posting_row(raw: &[u8], id: u32) -> Option<&[u8]> {
    let mut low = 0;
    let mut high = raw.len() / POSTING_BYTES;
    while low < high {
        let mid = low + (high - low) / 2;
        let row = &raw[mid * POSTING_BYTES..(mid + 1) * POSTING_BYTES];
        let value = u32::from_le_bytes(row[..4].try_into().ok()?);
        match value.cmp(&id) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => return Some(row),
        }
    }
    None
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
    let full = owned_file_path(root, rel)?;
    fs::read(&full).with_context(|| format!("read normalized snapshot file: {rel}"))
}

fn owned_file_path(root: &Path, rel: &str) -> Result<PathBuf> {
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
    Ok(full)
}

fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|value| format!("{value:02x}")).collect()
}
