//! Independently bound native/OCR artifacts in an immutable v2 generation.
//! Publication consumes completed parser output; it never starts a model.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use gptgrep_parse::{
    ArtifactKind, ArtifactRef, BoundingBox, CoverageState, NativePage, OcrPageResult, OcrTask,
    PreparedSummary, ResourceBudget, RichParsedDocument, SegmentMap,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::read_cache::{CacheKey, ReadCache, ReadMeter, ReadOrigin};
use crate::{
    Document, GenerationPin, MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES, Manifest, PageSpan, Pointer,
    SCHEMA, STATE, bounded_read, denied, digest, no_symlink, source_path, validate_manifest,
};
use crate::{
    ReaderCacheBudget, ReaderReadMetrics, ReaderText, bounded_read_metered, digest_metered,
    hash_reader_file, load_reader_text,
};

pub const RICH_MANIFEST_SCHEMA: &str = "gptgrep.manifest.v2";
const MAX_OUTPUT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredArtifact {
    pub document_id: String,
    /// The parser's deterministic source/profile/kind/page/content identity.
    pub parser_artifact_id: String,
    /// Generation-relative storage and a document namespace added by core.
    pub artifact: ArtifactRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichDocument {
    pub document: Document,
    pub prepared: PreparedSummary,
    pub prepared_path: PathBuf,
    pub native_profile_sha256: String,
    pub pages: Vec<NativePage>,
    pub artifacts: Vec<StoredArtifact>,
    pub segments: Vec<SegmentMap>,
    pub ocr_tasks: Vec<OcrTask>,
    pub ocr_results: Vec<OcrPageResult>,
    pub unmapped_merged_lines: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichManifest {
    pub schema_version: String,
    pub generation: String,
    pub root: PathBuf,
    pub created_unix_ms: u128,
    pub resource_budget: ResourceBudget,
    pub documents: Vec<RichDocument>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEvidenceBinding {
    pub generation: GenerationPin,
    pub document_id: String,
    pub source_path: String,
    pub source_sha256: String,
    pub artifact_id: String,
    pub artifact_sha256: String,
    pub page: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub line: usize,
}

/// Text coordinates always refer to a saved native/OCR artifact. A page image
/// carries geometry instead of fabricated byte or line coordinates.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceRef {
    NativeText {
        binding: TextEvidenceBinding,
    },
    OcrText {
        binding: TextEvidenceBinding,
        region_ids: Vec<String>,
        ocr_profile_sha256: String,
        ocr_receipt_sha256: String,
    },
    VisualRegion {
        generation: GenerationPin,
        document_id: String,
        source_path: String,
        source_sha256: String,
        page: u32,
        region_id: String,
        rendition_id: String,
        rendition_sha256: String,
        bbox: BoundingBox,
        page_to_raster: gptgrep_parse::AffineTransform,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactTextWindow {
    pub evidence: EvidenceRef,
    pub text: String,
    pub next_offset: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactSearchReport {
    pub schema_version: String,
    pub generation: GenerationPin,
    pub hits: Vec<ArtifactTextWindow>,
    pub available_artifacts: usize,
    pub candidate_artifacts: usize,
    pub verified_artifacts: usize,
    pub truncated: bool,
    pub stale_documents: Vec<String>,
}

/// Holds the corpus writer lock until publication or drop. Failed candidates
/// remain unpublished and retained; they do not replace CURRENT.
pub struct RichGenerationWriter {
    manifest: RichManifest,
    dir: PathBuf,
    budget: ResourceBudget,
    poisoned: bool,
    _lock: File,
}

impl RichGenerationWriter {
    pub fn begin(root: &Path, budget: ResourceBudget) -> Result<Self> {
        budget.validate()?;
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "document root must be a directory");
        let state = root.join(STATE);
        for path in [
            &state,
            &state.join("generations"),
            &state.join("write.lock"),
            &state.join("CURRENT.json"),
        ] {
            no_symlink(path)?;
        }
        fs::create_dir_all(state.join("generations"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.join("write.lock"))?;
        lock.try_lock_exclusive()
            .context("another index writer owns this corpus")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let generation = format!("g{}-{}", now.as_nanos(), std::process::id());
        let dir = state.join("generations").join(&generation);
        fs::create_dir(&dir)?;
        for child in ["artifact-text", "artifacts", "text"] {
            fs::create_dir(dir.join(child))?;
        }
        Ok(Self {
            manifest: RichManifest {
                schema_version: RICH_MANIFEST_SCHEMA.into(),
                generation,
                root,
                created_unix_ms: now.as_millis(),
                resource_budget: budget.clone(),
                documents: Vec::new(),
            },
            dir,
            budget,
            poisoned: false,
            _lock: lock,
        })
    }

    pub fn candidate_dir(&self) -> &Path {
        &self.dir
    }

    /// Import a finalized parser candidate. All files are verified and copied
    /// into owned storage before their IDs become generation resources.
    pub fn add_document(
        &mut self,
        relative: &str,
        staging: &Path,
        parsed: RichParsedDocument,
    ) -> Result<()> {
        ensure!(
            !self.poisoned,
            "candidate contains a failed import; start a new generation"
        );
        self.poisoned = true;
        ensure!(
            !denied(Path::new(relative)),
            "source path is excluded by corpus policy"
        );
        let path = source_path(&self.manifest.root, relative)?;
        ensure!(
            self.manifest.documents.len() < 100_000,
            "rich generation document limit"
        );
        ensure!(
            !self
                .manifest
                .documents
                .iter()
                .any(|d| d.document.path == relative),
            "duplicate document path"
        );
        let (bytes, source_sha256) = hash_file(&path, MAX_SOURCE_BYTES)?;
        ensure!(
            source_sha256 == parsed.summary.source_sha256 && bytes == parsed.summary.source_bytes,
            "source changed before artifact import"
        );
        ensure!(
            staging.canonicalize()? == staging,
            "staging path must be canonical and nonsymlink"
        );
        ensure!(
            parsed.coverage
                == parsed
                    .pages
                    .iter()
                    .map(|p| (p.number, p.coverage.clone()))
                    .collect::<Vec<_>>(),
            "completed coverage summary differs from physical pages"
        );
        let retained = parsed
            .artifacts
            .iter()
            .try_fold(parsed.summary.prepared_bytes, |total, a| {
                total.checked_add(a.bytes)
            })
            .context("document retained byte overflow")?;
        ensure!(
            retained <= self.budget.document_staging_bytes,
            "prepared plus artifacts exceed selected document staging admission"
        );
        let metadata = parsed
            .artifacts
            .iter()
            .filter(|a| {
                matches!(
                    a.kind,
                    ArtifactKind::NativeMetadata | ArtifactKind::OcrRegions | ArtifactKind::Layout
                )
            })
            .try_fold(0u64, |total, a| total.checked_add(a.bytes))
            .context("document metadata overflow")?;
        ensure!(
            metadata <= self.budget.document_metadata_bytes,
            "document metadata exceeds selected admission"
        );
        let id = digest(relative.as_bytes())[..24].to_owned();
        let mut record = RichDocument {
            document: Document {
                id: id.clone(),
                path: relative.into(),
                source_sha256,
                source_bytes: bytes,
                text_sha256: digest(parsed.document.text.as_bytes()),
                parser: parsed.document.parser,
                title: parsed.document.title,
                pages: parsed
                    .document
                    .pages
                    .into_iter()
                    .map(|p| PageSpan {
                        number: p.number,
                        line_start: p.line_start,
                        line_end: p.line_end,
                    })
                    .collect(),
                nodes: parsed.document.nodes,
                warnings: parsed.document.warnings,
            },
            prepared: parsed.summary,
            prepared_path: Path::new("artifacts").join(&id).join("prepared.pdf"),
            native_profile_sha256: parsed.native_profile_sha256,
            pages: parsed.pages,
            artifacts: Vec::new(),
            segments: parsed.segments,
            ocr_tasks: parsed.ocr_tasks,
            ocr_results: parsed.ocr_results,
            unmapped_merged_lines: parsed.unmapped_merged_lines,
        };
        fs::create_dir(self.dir.join("artifacts").join(&id))?;
        copy_verified(
            staging,
            Path::new("prepared.pdf"),
            &self.dir,
            &record.prepared_path,
            record.prepared.prepared_bytes,
            &record.prepared.prepared_sha256,
            &self.budget,
        )?;
        let mut remap = BTreeMap::new();
        for original in parsed.artifacts {
            ensure!(
                original.bytes <= artifact_limit(&original, &self.budget),
                "artifact exceeds kind admission limit"
            );
            ensure!(
                !remap.contains_key(&original.id),
                "duplicate parser artifact ID"
            );
            let mut artifact = original.clone();
            artifact.id = namespaced_id(&id, &original.id);
            artifact.relative_path = storage_path(&id, &artifact);
            ensure!(
                !record
                    .artifacts
                    .iter()
                    .any(|a| a.artifact.relative_path == artifact.relative_path),
                "duplicate artifact storage path"
            );
            copy_verified(
                staging,
                &original.relative_path,
                &self.dir,
                &artifact.relative_path,
                original.bytes,
                &original.sha256,
                &self.budget,
            )?;
            remap.insert(original.id.clone(), artifact.clone());
            record.artifacts.push(StoredArtifact {
                document_id: id.clone(),
                parser_artifact_id: original.id,
                artifact,
            });
        }
        for page in &mut record.pages {
            page.native_text = mapped(&remap, &page.native_text)?.clone();
            page.metadata_artifact = mapped(&remap, &page.metadata_artifact)?.clone();
            for image in &mut page.renditions {
                image.artifact = mapped(&remap, &image.artifact)?.clone();
                image.path = self.dir.join(&image.artifact.relative_path);
            }
        }
        for segment in &mut record.segments {
            segment.artifact_id = remap
                .get(&segment.artifact_id)
                .context("segment has unknown artifact")?
                .id
                .clone();
        }
        validate_document_shape(&record, &self.budget)?;
        verify_document(&self.manifest.root, &self.dir, &record, &self.budget)?;
        self.manifest.documents.push(record);
        self.poisoned = false;
        Ok(())
    }

    pub fn publish(mut self) -> Result<GenerationPin> {
        ensure!(
            !self.poisoned,
            "candidate contains a failed import; CURRENT remains unchanged"
        );
        ensure!(
            !self.manifest.documents.is_empty(),
            "no completed rich documents to publish"
        );
        self.manifest
            .documents
            .sort_by(|a, b| a.document.path.cmp(&b.document.path));
        validate_rich_manifest(&self.manifest)?;
        let size = encoded_size(&self.manifest, MAX_MANIFEST_BYTES)?;
        self.budget.reserve_retained_staging(size)?;
        let bytes = serde_json::to_vec(&self.manifest)?;
        write_new(&self.dir.join("manifest.json"), &bytes)?;
        gptgrep_index::build(
            &self.dir.join("artifact-text"),
            &self.dir.join("artifact-index"),
        )?;
        let pin = GenerationPin {
            generation: self.manifest.generation.clone(),
            manifest_sha256: digest(&bytes),
        };
        RichGenerationReader::open_pinned(&self.manifest.root, &pin)?.verify_all()?;
        sync_tree(&self.dir)?;
        // Recheck the entire source set immediately before the atomic pointer change.
        for doc in &self.manifest.documents {
            verify_source(&self.manifest.root, doc)?;
        }
        let state = self.manifest.root.join(STATE);
        no_symlink(&state.join("CURRENT.json"))?;
        let pointer = Pointer {
            schema_version: RICH_MANIFEST_SCHEMA.into(),
            generation: pin.generation.clone(),
            manifest_sha256: pin.manifest_sha256.clone(),
        };
        let mut temp = tempfile::NamedTempFile::new_in(&state)?;
        serde_json::to_writer(&mut temp, &pointer)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(state.join("CURRENT.json"))?;
        File::open(state)?.sync_all()?;
        Ok(pin)
    }
}

#[derive(Clone)]
pub struct RichGenerationReader {
    manifest: Arc<RichManifest>,
    pin: GenerationPin,
    dir: PathBuf,
    index: Arc<OnceLock<std::result::Result<gptgrep_index::IndexSession, String>>>,
    cache: ReadCache,
    cache_budget: ReaderCacheBudget,
    read_meter: ReadMeter,
}

#[derive(Deserialize)]
struct RichManifestHeader<'a> {
    schema_version: &'a str,
}

impl RichGenerationReader {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_cache_budget(root, &ReaderCacheBudget::default())
    }
    pub fn open_with_cache_budget(root: &Path, budget: &ReaderCacheBudget) -> Result<Self> {
        budget.validate()?;
        let meter = ReadMeter::default();
        let root = root.canonicalize()?;
        let state = root.join(STATE);
        no_symlink(&state)?;
        let pointer: Pointer = serde_json::from_slice(&bounded_read_metered(
            &owned_path(&state, Path::new("CURRENT.json"))?,
            4096,
            &meter,
        )?)?;
        ensure!(
            pointer.schema_version == RICH_MANIFEST_SCHEMA,
            "rebuild_required: rich artifacts require manifest v2"
        );
        Self::open_pinned_with_read_meter(
            &root,
            &GenerationPin {
                generation: pointer.generation,
                manifest_sha256: pointer.manifest_sha256,
            },
            budget,
            meter,
        )
    }

    pub fn open_pinned(root: &Path, pin: &GenerationPin) -> Result<Self> {
        Self::open_pinned_with_cache_budget(root, pin, &ReaderCacheBudget::default())
    }
    pub fn open_pinned_with_cache_budget(
        root: &Path,
        pin: &GenerationPin,
        budget: &ReaderCacheBudget,
    ) -> Result<Self> {
        Self::open_pinned_with_read_meter(root, pin, budget, ReadMeter::default())
    }
    fn open_pinned_with_read_meter(
        root: &Path,
        pin: &GenerationPin,
        budget: &ReaderCacheBudget,
        meter: ReadMeter,
    ) -> Result<Self> {
        budget.validate()?;
        ensure!(
            valid_digest(&pin.manifest_sha256)
                && !pin.generation.is_empty()
                && pin
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid generation pin"
        );
        let root = root.canonicalize()?;
        let dir = owned_path(
            &root,
            &Path::new(STATE).join("generations").join(&pin.generation),
        )?;
        let bytes = bounded_read_metered(
            &owned_path(&dir, Path::new("manifest.json"))?,
            MAX_MANIFEST_BYTES,
            &meter,
        )?;
        ensure!(
            digest_metered(&bytes, &meter)? == pin.manifest_sha256,
            "rich manifest digest mismatch"
        );
        let header: RichManifestHeader<'_> = serde_json::from_slice(&bytes)
            .context("rebuild_required: invalid rich manifest header")?;
        ensure!(
            header.schema_version == RICH_MANIFEST_SCHEMA,
            "rebuild_required: rich artifacts require manifest v2"
        );
        let manifest: RichManifest = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.root == root && manifest.generation == pin.generation,
            "rich generation identity mismatch"
        );
        validate_rich_manifest(&manifest)?;
        Ok(Self {
            manifest: Arc::new(manifest),
            pin: pin.clone(),
            dir,
            index: Arc::new(OnceLock::new()),
            cache: ReadCache::new(budget.max_bytes, budget.max_entries)?,
            cache_budget: budget.clone(),
            read_meter: meter,
        })
    }

    pub fn pin(&self) -> GenerationPin {
        self.pin.clone()
    }
    pub fn read_metrics(&self) -> Result<ReaderReadMetrics> {
        Ok(self.read_meter.snapshot()?.into())
    }
    pub fn clear_cache(&self) -> Result<()> {
        Ok(self.cache.clear()?)
    }
    pub fn cache_status(&self) -> Result<serde_json::Value> {
        let status = self.cache.stats()?;
        Ok(
            serde_json::json!({"capacity_bytes":status.capacity_bytes,"capacity_entries":status.capacity_entries,
            "retained_bytes":status.retained_bytes,"retained_entries":status.retained_entries,
            "resident_bytes":status.resident_bytes,"resident_entries":status.resident_entries,
            "strict_cache_only":self.cache_budget.strict_cache_only}),
        )
    }
    pub fn manifest(&self) -> Result<&RichManifest> {
        self.validate()?;
        Ok(&self.manifest)
    }

    pub fn validate(&self) -> Result<()> {
        let expected = owned_path(
            &self.manifest.root,
            &Path::new(STATE)
                .join("generations")
                .join(&self.pin.generation),
        )?;
        ensure!(
            expected == self.dir,
            "retained generation directory changed"
        );
        ensure!(
            digest_metered(
                &bounded_read_metered(
                    &owned_path(&self.dir, Path::new("manifest.json"))?,
                    MAX_MANIFEST_BYTES,
                    &self.read_meter
                )?,
                &self.read_meter
            )? == self.pin.manifest_sha256,
            "retained rich manifest changed"
        );
        if let Some(Ok(index)) = self.index.get() {
            index.validate()?;
        }
        Ok(())
    }

    pub fn verify_all(&self) -> Result<()> {
        self.validate()?;
        for doc in &self.manifest.documents {
            verify_document(
                &self.manifest.root,
                &self.dir,
                doc,
                &self.manifest.resource_budget,
            )?;
        }
        self.index_session()?.validate()?;
        Ok(())
    }

    fn index_session(&self) -> Result<&gptgrep_index::IndexSession> {
        match self.index.get_or_init(|| {
            gptgrep_index::IndexSession::open(&self.dir.join("artifact-index"))
                .map_err(|e| format!("{e:#}"))
        }) {
            Ok(index) => Ok(index),
            Err(e) => anyhow::bail!("retained artifact index invalid: {e}"),
        }
    }

    fn find_artifact(&self, id: &str) -> Result<(&RichDocument, &StoredArtifact)> {
        self.manifest
            .documents
            .iter()
            .find_map(|doc| {
                doc.artifacts
                    .iter()
                    .find(|a| a.artifact.id == id)
                    .map(|a| (doc, a))
            })
            .context("artifact not present in pinned generation")
    }
    fn verify_source_metered(&self, doc: &RichDocument) -> Result<()> {
        ensure!(
            hash_reader_file(
                &source_path(&self.manifest.root, &doc.document.path)?,
                MAX_SOURCE_BYTES,
                &self.read_meter
            )? == (
                doc.document.source_bytes,
                doc.document.source_sha256.clone()
            ),
            "source changed or disappeared: {}",
            doc.document.path
        );
        Ok(())
    }
    fn text_cached(&self, artifact: &ArtifactRef) -> Result<ReaderText> {
        ensure!(is_text(&artifact.kind), "artifact is not native/OCR text");
        let path = owned_path(&self.dir, &artifact.relative_path)?;
        let limit = artifact_limit(artifact, &self.manifest.resource_budget);
        ensure!(
            hash_reader_file(&path, limit, &self.read_meter)?
                == (artifact.bytes, artifact.sha256.clone()),
            "artifact bytes changed: {}",
            artifact.id
        );
        let key = CacheKey::new(&self.pin.generation, &artifact.id, &artifact.sha256)?;
        let text = load_reader_text(
            &self.cache,
            &self.cache_budget,
            &self.read_meter,
            &key,
            &path,
            artifact.bytes,
            limit,
        )?;
        // Preserve the second current-file digest verification on a hit. A
        // cold load instead checks its actual filled bytes inside the cache.
        if text.origin == ReadOrigin::Cache {
            ensure!(
                hash_reader_file(&path, limit, &self.read_meter)?
                    == (artifact.bytes, artifact.sha256.clone()),
                "artifact changed while reading"
            );
        }
        Ok(text)
    }

    pub fn read_text(
        &self,
        artifact_id: &str,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ArtifactTextWindow> {
        ensure!(
            max_bytes > 0 && max_bytes <= 65_536,
            "text window must be 1..65536 bytes"
        );
        self.validate()?;
        let (doc, stored) = self.find_artifact(artifact_id)?;
        self.verify_source_metered(doc)?;
        let text = self.text_cached(&stored.artifact)?;
        ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "invalid text window offset"
        );
        let mut end = text.len().min(offset.saturating_add(max_bytes));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        ensure!(
            end > offset || offset == text.len(),
            "text budget cannot fit the next UTF-8 scalar"
        );
        let evidence = self.text_evidence(doc, stored, &text, offset, end)?;
        let window = ArtifactTextWindow {
            evidence,
            text: text[offset..end].into(),
            next_offset: (end < text.len()).then_some(end),
        };
        encoded_size(&window, MAX_OUTPUT_BYTES as u64)?;
        self.verify_source_metered(doc)?;
        self.validate()?;
        self.read_meter
            .record_delivery(window.text.len(), text.origin)?;
        Ok(window)
    }

    /// Verify artifact custody and exact coordinates of a nonempty reference.
    /// Reader delivery must be recorded separately before accepting a citation.
    pub fn validate_text_evidence(&self, evidence: &EvidenceRef) -> Result<()> {
        let binding = match evidence {
            EvidenceRef::NativeText { binding } | EvidenceRef::OcrText { binding, .. } => binding,
            EvidenceRef::VisualRegion { .. } => {
                anyhow::bail!("visual evidence requires rendition delivery validation")
            }
        };
        ensure!(
            binding.generation == self.pin(),
            "evidence belongs to another generation"
        );
        self.validate()?;
        let (doc, stored) = self.find_artifact(&binding.artifact_id)?;
        self.verify_source_metered(doc)?;
        let text = self.text_cached(&stored.artifact)?;
        ensure!(
            binding.byte_start < binding.byte_end
                && text.get(binding.byte_start..binding.byte_end).is_some(),
            "evidence text range is empty or not on UTF-8 boundaries"
        );
        let expected =
            self.text_evidence(doc, stored, &text, binding.byte_start, binding.byte_end)?;
        ensure!(
            serde_json::to_value(evidence)? == serde_json::to_value(expected)?,
            "evidence source/page/digest/coordinates/receipt mismatch"
        );
        self.verify_source_metered(doc)?;
        self.validate()
    }

    pub fn search_artifacts(
        &self,
        pattern: &str,
        case_insensitive: bool,
        literal: bool,
        limit: usize,
        document: Option<&str>,
        max_output_bytes: usize,
    ) -> Result<ArtifactSearchReport> {
        ensure!(
            limit > 0
                && limit <= 1000
                && max_output_bytes > 0
                && max_output_bytes <= MAX_OUTPUT_BYTES,
            "invalid artifact search budget"
        );
        self.validate()?;
        let scope: Vec<_> = self
            .manifest
            .documents
            .iter()
            .filter(|d| document.is_none_or(|p| p == d.document.path))
            .collect();
        ensure!(
            !scope.is_empty(),
            "document scope is not present in pinned generation"
        );
        let texts: BTreeMap<_, _> = scope
            .iter()
            .flat_map(|doc| {
                doc.artifacts
                    .iter()
                    .filter(|a| is_text(&a.artifact.kind))
                    .map(move |a| {
                        (
                            a.artifact.relative_path.file_name().unwrap().to_owned(),
                            (*doc, a),
                        )
                    })
            })
            .collect();
        // A full matching line can occupy its whole artifact. Reserve JSON escaping
        // and all citation fields before matching; never silently cut a result.
        let max_text = texts
            .values()
            .map(|(_, a)| a.artifact.bytes)
            .max()
            .unwrap_or(0);
        let worst = (limit as u64)
            .checked_mul(
                max_text
                    .checked_mul(6)
                    .and_then(|n| n.checked_add(32_768))
                    .context("search output reservation overflow")?,
            )
            .and_then(|n| n.checked_add(16_384 + scope.len() as u64 * 8192))
            .context("search output reservation overflow")?;
        ensure!(
            worst <= max_output_bytes as u64,
            "resource_limit: complete artifact search results exceed output reservation; reduce limit or read a bounded text window"
        );
        let mut stale = BTreeSet::new();
        let result = self.index_session()?.search_with_document_and_predicate(
            pattern,
            case_insensitive,
            literal,
            limit,
            None,
            |path| {
                let Some((doc, _)) = texts.get(path.as_os_str()) else {
                    return Ok(false);
                };
                if self.verify_source_metered(doc).is_err() {
                    stale.insert(doc.document.path.clone());
                    return Ok(false);
                }
                Ok(true)
            },
        )?;
        let mut hits = Vec::with_capacity(result.hits.len());
        for hit in result.hits {
            let (doc, stored) = texts
                .get(hit.path.as_os_str())
                .context("artifact index references unknown text identity")?;
            let text = self.text_cached(&stored.artifact)?;
            let (start, end) = line_range(&text, hit.line_number)?;
            ensure!(
                text[start..end].trim_end_matches('\r') == hit.line,
                "artifact index line mismatch"
            );
            hits.push(ArtifactTextWindow {
                evidence: self.text_evidence(doc, stored, &text, start, end)?,
                text: text[start..end].into(),
                next_offset: None,
            });
            self.verify_source_metered(doc)?;
        }
        self.validate()?;
        let report = ArtifactSearchReport {
            schema_version: "gptgrep.evidence.v2".into(),
            generation: self.pin(),
            hits,
            available_artifacts: texts.len(),
            candidate_artifacts: result.candidate_files,
            verified_artifacts: result.verified_files,
            truncated: result.truncated,
            stale_documents: stale.into_iter().collect(),
        };
        encoded_size(&report, max_output_bytes as u64)?;
        Ok(report)
    }

    fn text_evidence(
        &self,
        doc: &RichDocument,
        stored: &StoredArtifact,
        text: &str,
        start: usize,
        end: usize,
    ) -> Result<EvidenceRef> {
        let artifact = &stored.artifact;
        ensure!(
            is_text(&artifact.kind),
            "artifact is not native/OCR text evidence"
        );
        let page = artifact
            .page
            .context("text artifact has no physical page")?;
        let binding = TextEvidenceBinding {
            generation: self.pin(),
            document_id: doc.document.id.clone(),
            source_path: doc.document.path.clone(),
            source_sha256: doc.document.source_sha256.clone(),
            artifact_id: artifact.id.clone(),
            artifact_sha256: artifact.sha256.clone(),
            page,
            byte_start: start,
            byte_end: end,
            line: text[..start].bytes().filter(|b| *b == b'\n').count() + 1,
        };
        if artifact.kind == ArtifactKind::NativeText {
            return Ok(EvidenceRef::NativeText { binding });
        }
        let outcome = doc
            .ocr_results
            .iter()
            .find(|r| r.page == page)
            .context("OCR text has no bound receipt")?;
        let mut regions = outcome.regions.iter().collect::<Vec<_>>();
        regions.sort_by_key(|r| r.reading_order);
        let mut position = 0usize;
        let mut region_ids = Vec::new();
        for region in regions {
            let next = position + region.text.len() + 1;
            if start < next && end > position {
                region_ids.push(region.id.clone());
            }
            position = next;
        }
        Ok(EvidenceRef::OcrText {
            binding,
            region_ids,
            ocr_profile_sha256: outcome.receipt.profile_sha256.clone(),
            ocr_receipt_sha256: digest(&serde_json::to_vec(&outcome.receipt)?),
        })
    }
}

fn validate_rich_manifest(manifest: &RichManifest) -> Result<()> {
    manifest.resource_budget.validate()?;
    ensure!(
        manifest.schema_version == RICH_MANIFEST_SCHEMA,
        "rebuild_required: unsupported rich manifest schema"
    );
    ensure!(
        !manifest.documents.is_empty() && manifest.documents.len() <= 100_000,
        "invalid rich document count"
    );
    validate_manifest(&Manifest {
        schema_version: SCHEMA.into(),
        generation: manifest.generation.clone(),
        root: manifest.root.clone(),
        created_unix_ms: manifest.created_unix_ms,
        documents: manifest
            .documents
            .iter()
            .map(|d| d.document.clone())
            .collect(),
        excluded_files: 0,
        tree_profile: "liteparse-rich-v1".into(),
    })?;
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for doc in &manifest.documents {
        validate_document_shape(doc, &manifest.resource_budget)?;
        ensure!(
            paths.insert(&doc.prepared_path),
            "duplicate prepared artifact path"
        );
        for stored in &doc.artifacts {
            ensure!(
                ids.insert(&stored.artifact.id) && paths.insert(&stored.artifact.relative_path),
                "duplicate rich artifact identity/path"
            );
        }
    }
    Ok(())
}

fn validate_document_shape(doc: &RichDocument, budget: &ResourceBudget) -> Result<()> {
    ensure!(
        valid_digest(&doc.native_profile_sha256)
            && valid_digest(&doc.prepared.preparation_profile_sha256)
            && doc.prepared.source_sha256 == doc.document.source_sha256
            && doc.prepared.source_bytes == doc.document.source_bytes
            && valid_digest(&doc.prepared.prepared_sha256)
            && doc.prepared.parser_identity == doc.document.parser,
        "invalid prepared/parser identity"
    );
    match (
        &*doc.prepared.format,
        &doc.prepared.conversion,
        &doc.prepared.image_geometry,
    ) {
        (
            "pdf",
            gptgrep_parse::FeatureState::Present(conversion),
            gptgrep_parse::FeatureState::Absent,
        ) => {
            ensure!(
                conversion.converter == "identity-pdf"
                    && conversion.version == gptgrep_parse::LITEPARSE_REVISION
                    && conversion.invocations == 0
                    && doc.prepared.source_sha256 == doc.prepared.prepared_sha256
                    && doc.prepared.source_bytes == doc.prepared.prepared_bytes,
                "PDF identity preparation does not match raw source"
            );
            ensure!(
                doc.prepared.preparation_profile_sha256
                    == digest(&serde_json::to_vec(&(
                        "gptgrep.prepare.identity-pdf.v1",
                        gptgrep_parse::LITEPARSE_REVISION
                    ))?),
                "PDF preparation recipe mismatch"
            );
        }
        (
            "png",
            gptgrep_parse::FeatureState::Present(conversion),
            gptgrep_parse::FeatureState::Present(geometry),
        ) => {
            ensure!(
                conversion.converter == "gptgrep-bounded-image-pdf"
                    && conversion.version == "image-0.25.10/png-rgb8-pdf-v2"
                    && conversion.invocations == 1
                    && doc.pages.len() == 1,
                "image preparation identity/count mismatch"
            );
            budget.admit_image(
                geometry.source_width_pixels,
                geometry.source_height_pixels,
                doc.prepared.source_bytes,
            )?;
            ensure!(
                geometry.page_width_points == f64::from(geometry.source_width_pixels)
                    && geometry.page_height_points == f64::from(geometry.source_height_pixels)
                    && geometry.source_pixels_to_page == gptgrep_parse::AffineTransform::IDENTITY
                    && doc.pages[0].width_points == geometry.page_width_points
                    && doc.pages[0].height_points == geometry.page_height_points,
                "prepared image/page coordinate mapping mismatch"
            );
            ensure!(
                doc.prepared.preparation_profile_sha256
                    == gptgrep_parse::image_preparation_profile_digest(
                        &doc.prepared.format,
                        geometry
                    )?,
                "image preparation recipe mismatch"
            );
        }
        _ => anyhow::bail!("source conversion or source geometry not qualified"),
    }
    ensure!(
        doc.prepared_path
            == Path::new("artifacts")
                .join(&doc.document.id)
                .join("prepared.pdf"),
        "invalid prepared storage path"
    );
    ensure!(
        doc.pages.len() == doc.document.pages.len() && !doc.pages.is_empty(),
        "rich page coverage mismatch"
    );
    let mut ids = BTreeSet::new();
    let mut parser_ids = BTreeSet::new();
    let mut total = doc.prepared.prepared_bytes;
    for stored in &doc.artifacts {
        let a = &stored.artifact;
        ensure!(
            stored.document_id == doc.document.id
                && ids.insert(&a.id)
                && parser_ids.insert(&stored.parser_artifact_id),
            "duplicate/misowned rich artifact"
        );
        ensure!(
            valid_digest(&a.sha256) && valid_digest(&a.profile_sha256),
            "invalid artifact digest/profile"
        );
        let text_kind = matches!(
            a.kind,
            ArtifactKind::NativeText | ArtifactKind::OcrText | ArtifactKind::MergedText
        );
        ensure!(
            if text_kind {
                a.mime == "text/plain; charset=utf-8"
            } else if a.kind == ArtifactKind::Rendition {
                matches!(a.mime.as_str(), "image/png" | "image/jpeg")
            } else {
                a.mime == "application/json"
            },
            "artifact kind/MIME mismatch"
        );
        ensure!(
            a.region_id.is_none(),
            "unexpected region artifact in page-level generation"
        );
        ensure!(
            if matches!(a.kind, ArtifactKind::Layout | ArtifactKind::MergedText) {
                a.page.is_none()
            } else {
                a.page.is_some()
            },
            "artifact kind/page mismatch"
        );
        if matches!(
            a.kind,
            ArtifactKind::NativeText | ArtifactKind::NativeMetadata | ArtifactKind::Rendition
        ) {
            ensure!(
                a.profile_sha256 == doc.native_profile_sha256,
                "native artifact recipe mismatch"
            );
        }
        ensure!(
            stored.parser_artifact_id
                == gptgrep_parse::artifact_id(
                    &doc.document.source_sha256,
                    &a.profile_sha256,
                    &a.kind,
                    a.page,
                    a.region_id.as_deref(),
                    &a.sha256
                )?
                && a.id == namespaced_id(&doc.document.id, &stored.parser_artifact_id)
                && a.relative_path == storage_path(&doc.document.id, a),
            "artifact identity/storage mismatch"
        );
        ensure!(
            a.bytes <= artifact_limit(a, budget),
            "artifact exceeds kind limit"
        );
        total = total
            .checked_add(a.bytes)
            .context("artifact byte overflow")?;
        if let Some(page) = a.page {
            ensure!(
                page > 0 && page as usize <= doc.pages.len(),
                "artifact page outside document"
            );
        }
    }
    ensure!(
        total <= budget.document_staging_bytes,
        "document artifact staging exceeds frozen limit"
    );
    let metadata_bytes = doc
        .artifacts
        .iter()
        .filter(|a| {
            matches!(
                a.artifact.kind,
                ArtifactKind::NativeMetadata | ArtifactKind::OcrRegions | ArtifactKind::Layout
            )
        })
        .try_fold(0u64, |total, a| total.checked_add(a.artifact.bytes))
        .context("metadata byte overflow")?;
    ensure!(
        metadata_bytes <= budget.document_metadata_bytes,
        "document metadata exceeds selected budget"
    );
    ensure!(
        doc.artifacts
            .iter()
            .filter(|a| a.artifact.kind == ArtifactKind::MergedText)
            .count()
            == 1,
        "exactly one merged text artifact required"
    );
    ensure!(
        doc.artifacts
            .iter()
            .filter(|a| a.artifact.kind == ArtifactKind::Layout)
            .count()
            == 1,
        "exactly one complete-document layout artifact required"
    );
    let final_profile = &doc
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == ArtifactKind::MergedText)
        .context("merged artifact absent")?
        .artifact
        .profile_sha256;
    ensure!(
        doc.artifacts
            .iter()
            .filter(|a| matches!(
                a.artifact.kind,
                ArtifactKind::MergedText
                    | ArtifactKind::Layout
                    | ArtifactKind::OcrText
                    | ArtifactKind::OcrRegions
            ))
            .all(|a| &a.artifact.profile_sha256 == final_profile),
        "final artifact recipe mismatch"
    );
    for (i, page) in doc.pages.iter().enumerate() {
        ensure!(
            page.number as usize == i + 1
                && page.width_points.is_finite()
                && page.height_points.is_finite()
                && page.width_points > 0.0
                && page.height_points > 0.0,
            "invalid physical page identity/geometry"
        );
        ensure!(
            page.coverage != CoverageState::RequiredOcr,
            "required OCR coverage incomplete; publication blocked"
        );
        page.geometry.source_to_page.validate()?;
        page.geometry.page_to_source.validate()?;
        ensure!(
            page.geometry.width_points == page.width_points
                && page.geometry.height_points == page.height_points
                && page.geometry.user_unit.is_finite()
                && page.geometry.user_unit > 0.0,
            "page coordinate frame mismatch"
        );
        for (x, y) in [
            (0.0, 0.0),
            (page.width_points, 0.0),
            (0.0, page.height_points),
        ] {
            let (sx, sy) = page.geometry.page_to_source.apply(x, y);
            let (px, py) = page.geometry.source_to_page.apply(sx, sy);
            ensure!(
                (px - x).abs() <= 0.001 && (py - y).abs() <= 0.001,
                "page coordinate transforms are not inverses"
            );
        }
        let annotation_count = match page.capabilities.get("annotation_count") {
            Some(gptgrep_parse::FeatureState::Present(value)) => {
                value.parse::<u32>().context("invalid annotation count")?
            }
            _ => anyhow::bail!("native annotation coverage not qualified"),
        };
        ensure!(
            page.complexity.page_number == page.number as usize,
            "complexity physical page mismatch"
        );
        if page.coverage != CoverageState::ValidatedOcr {
            ensure!(
                !page.complexity.needs_ocr && annotation_count == 0,
                "native/blank coverage contradicts required OCR signals"
            );
        } else {
            ensure!(
                page.complexity.needs_ocr || annotation_count > 0,
                "OCR coverage has no original required-page signal"
            );
        }
        for (reference, kind) in [
            (&page.native_text, ArtifactKind::NativeText),
            (&page.metadata_artifact, ArtifactKind::NativeMetadata),
        ] {
            ensure!(
                doc.artifacts
                    .iter()
                    .filter(|a| a.artifact.kind == kind && a.artifact.page == Some(page.number))
                    .count()
                    == 1,
                "exactly one native text/metadata artifact per physical page required"
            );
            ensure!(
                reference.kind == kind
                    && reference.page == Some(page.number)
                    && doc
                        .artifacts
                        .iter()
                        .any(|a| same_artifact(&a.artifact, reference)),
                "page references unbound artifact"
            );
        }
        ensure!(
            page.renditions.len() <= budget.images_per_page,
            "too many page renditions"
        );
        let rendition_ids = page
            .renditions
            .iter()
            .map(|r| &r.artifact.id)
            .collect::<BTreeSet<_>>();
        ensure!(
            rendition_ids.len() == page.renditions.len()
                && doc
                    .artifacts
                    .iter()
                    .filter(|a| a.artifact.kind == ArtifactKind::Rendition
                        && a.artifact.page == Some(page.number))
                    .count()
                    == page.renditions.len(),
            "page renditions duplicate or leave an unreferenced artifact"
        );
        let mut pixels = 0u64;
        let mut bytes = 0u64;
        for image in &page.renditions {
            ensure!(
                image.artifact.kind == ArtifactKind::Rendition
                    && image.artifact.page == Some(page.number)
                    && doc
                        .artifacts
                        .iter()
                        .any(|a| same_artifact(&a.artifact, &image.artifact)),
                "page rendition is unbound"
            );
            pixels += budget.admit_image(image.width, image.height, image.artifact.bytes)?;
            bytes += image.artifact.bytes;
            image.page_to_raster.validate()?;
            for (x, y, rx, ry) in [
                (0.0, 0.0, 0.0, 0.0),
                (page.width_points, 0.0, f64::from(image.width), 0.0),
                (0.0, page.height_points, 0.0, f64::from(image.height)),
            ] {
                let (actual_x, actual_y) = image.page_to_raster.apply(x, y);
                ensure!(
                    (actual_x - rx).abs() <= 0.001 && (actual_y - ry).abs() <= 0.001,
                    "page-to-raster dimensions mismatch"
                );
            }
        }
        ensure!(
            pixels <= budget.image_pixels_per_page && bytes <= budget.image_file_bytes_per_page,
            "page rendition aggregate exceeds budget"
        );
        let tasks = doc
            .ocr_tasks
            .iter()
            .filter(|t| t.page == page.number)
            .count();
        let results = doc
            .ocr_results
            .iter()
            .filter(|r| r.page == page.number)
            .count();
        for kind in [ArtifactKind::OcrText, ArtifactKind::OcrRegions] {
            let expected = usize::from(page.coverage == CoverageState::ValidatedOcr);
            ensure!(
                doc.artifacts
                    .iter()
                    .filter(|a| a.artifact.kind == kind && a.artifact.page == Some(page.number))
                    .count()
                    == expected,
                "unexpected or missing raw OCR artifact for physical page"
            );
        }
        ensure!(
            (page.coverage == CoverageState::ValidatedOcr && tasks == 1 && results == 1)
                || (page.coverage != CoverageState::ValidatedOcr && tasks == 0 && results == 0),
            "page OCR state/task/result mismatch"
        );
    }
    ensure!(
        doc.ocr_tasks.len() == doc.ocr_results.len() && doc.ocr_tasks.len() <= doc.pages.len(),
        "unexpected OCR task/result"
    );
    Ok(())
}

#[derive(Deserialize)]
struct NativeCoverageMetadata {
    page: NativeCoveragePage,
    objects: ObjectCoverage,
    complexity: serde_json::Value,
    geometry: gptgrep_parse::PageGeometry,
    image_rects: Vec<NativeImageRect>,
    capabilities: BTreeMap<String, gptgrep_parse::FeatureState<String>>,
}
#[derive(Deserialize)]
struct NativeCoveragePage {
    page_number: usize,
    page_width: f32,
    page_height: f32,
    text_items: SequenceCount,
}
#[derive(Deserialize)]
struct NativeImageRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}
struct ObjectCoverage {
    count: usize,
    paths: usize,
    images: usize,
}
impl<'de> Deserialize<'de> for ObjectCoverage {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Object {
            kind: String,
        }
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ObjectCoverage;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("saved native objects")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut summary = ObjectCoverage {
                    count: 0,
                    paths: 0,
                    images: 0,
                };
                while let Some(object) = seq.next_element::<Object>()? {
                    summary.count = summary
                        .count
                        .checked_add(1)
                        .ok_or_else(|| serde::de::Error::custom("object count overflow"))?;
                    match object.kind.as_str() {
                        "path" => summary.paths += 1,
                        "image" => summary.images += 1,
                        _ => {
                            return Err(serde::de::Error::custom("unqualified native object kind"));
                        }
                    }
                }
                Ok(summary)
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}
struct SequenceCount(usize);
impl<'de> Deserialize<'de> for SequenceCount {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SequenceCount;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut count = 0usize;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| serde::de::Error::custom("metadata count overflow"))?;
                }
                Ok(SequenceCount(count))
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}

fn verify_document(
    root: &Path,
    dir: &Path,
    doc: &RichDocument,
    budget: &ResourceBudget,
) -> Result<()> {
    validate_document_shape(doc, budget)?;
    verify_source(root, doc)?;
    ensure!(
        hash_file(
            &owned_path(dir, &doc.prepared_path)?,
            budget.document_staging_bytes
        )? == (
            doc.prepared.prepared_bytes,
            doc.prepared.prepared_sha256.clone()
        ),
        "prepared bytes changed"
    );
    for stored in &doc.artifacts {
        verify_artifact(dir, &stored.artifact, budget)?;
    }
    for page in &doc.pages {
        ensure!(
            page.metadata_artifact
                .bytes
                .checked_mul(32)
                .is_some_and(|bytes| bytes <= budget.owned_working_bytes),
            "native metadata validation working admission"
        );
        let bytes = bounded_read(
            &owned_path(dir, &page.metadata_artifact.relative_path)?,
            budget.page_layout_bytes,
        )?;
        let metadata: NativeCoverageMetadata = serde_json::from_slice(&bytes)
            .context("rebuild_required: retained native coverage metadata missing or invalid")?;
        ensure!(
            metadata.page.page_number == page.number as usize
                && f64::from(metadata.page.page_width) == page.width_points
                && f64::from(metadata.page.page_height) == page.height_points
                && metadata.complexity
                    == serde_json::from_slice::<serde_json::Value>(&serde_json::to_vec(
                        &page.complexity
                    )?)?
                && serde_json::to_value(&metadata.geometry)?
                    == serde_json::to_value(&page.geometry)?
                && serde_json::to_value(&metadata.capabilities)?
                    == serde_json::to_value(&page.capabilities)?,
            "native metadata/page capability binding mismatch"
        );
        ensure!(
            metadata.image_rects.len() == metadata.objects.images
                && metadata
                    .image_rects
                    .iter()
                    .all(
                        |r| [r.x, r.y, r.width, r.height].iter().all(|x| x.is_finite())
                            && r.width >= 0.0
                            && r.height >= 0.0
                    ),
            "native image classifier inputs differ from retained objects"
        );
        if page.coverage != CoverageState::ValidatedOcr {
            ensure!(
                metadata.objects.paths == 0,
                "saved vector objects require OCR coverage"
            );
        }
        if page.coverage == CoverageState::BlankVerified {
            ensure!(
                page.native_text.bytes == 0
                    && metadata.page.text_items.0 == 0
                    && metadata.objects.count == 0,
                "blank coverage contradicts saved native content"
            );
        } else if page.coverage == CoverageState::Native {
            ensure!(
                page.native_text.bytes > 0,
                "empty nonblank native page has no qualified content"
            );
        }
        for original in &page.renditions {
            let mut image = original.clone();
            image.path = owned_path(dir, &image.artifact.relative_path)?;
            gptgrep_parse::validate_rendition(&image, budget)?;
        }
    }
    for original in &doc.ocr_tasks {
        let mut task = original.clone();
        let page = doc
            .pages
            .iter()
            .find(|p| p.number == task.page)
            .context("OCR physical page missing")?;
        let annotation_count = match page.capabilities.get("annotation_count") {
            Some(gptgrep_parse::FeatureState::Present(value)) => value.parse::<u32>()?,
            _ => anyhow::bail!("original OCR annotation count unavailable"),
        };
        ensure!(
            task.regions.len() == 1
                && task.regions[0].id == format!("p{}-required", page.number)
                && task.regions[0].required
                && task.regions[0].bbox
                    == BoundingBox {
                        x: 0.0,
                        y: 0.0,
                        width: page.width_points,
                        height: page.height_points
                    }
                && task.regions[0].reason
                    == format!(
                        "required visible coverage; native complexity={:?}; annotation count={annotation_count}",
                        page.complexity.reasons
                    )
                && task.layout_context.is_empty(),
            "OCR task does not preserve original required full-page coverage"
        );
        ensure!(
            task.source_sha256 == doc.document.source_sha256
                && task.prepared_sha256 == doc.prepared.prepared_sha256
                && task.page_width_points == page.width_points
                && task.page_height_points == page.height_points
                && task.native_text == read_text(dir, &page.native_text)?
                && task.images.len() == page.renditions.len(),
            "OCR task differs from prepared/native physical page"
        );
        let mut task_image_ids = BTreeSet::new();
        task.source_path = source_path(root, &doc.document.path)?;
        for image in &mut task.images {
            ensure!(
                task_image_ids.insert(&image.artifact.id),
                "duplicate OCR task image"
            );
            let stored = doc
                .artifacts
                .iter()
                .find(|a| a.parser_artifact_id == image.artifact.id)
                .context("OCR task image absent from stored artifacts")?;
            let rendition = page
                .renditions
                .iter()
                .find(|r| r.artifact.id == stored.artifact.id)
                .context("OCR task image is not a rendition of this physical page")?;
            ensure!(
                image.artifact.sha256 == stored.artifact.sha256
                    && image.artifact.profile_sha256 == stored.artifact.profile_sha256
                    && image.artifact.kind == stored.artifact.kind
                    && image.artifact.page == stored.artifact.page
                    && image.artifact.mime == stored.artifact.mime
                    && image.artifact.bytes == stored.artifact.bytes
                    && image.artifact.region_id == stored.artifact.region_id
                    && image.width == rendition.width
                    && image.height == rendition.height
                    && image.page_to_raster == rendition.page_to_raster,
                "OCR task image differs from stored artifact"
            );
            image.path = owned_path(dir, &stored.artifact.relative_path)?;
        }
        let result = doc
            .ocr_results
            .iter()
            .find(|r| r.page == task.page)
            .context("required OCR result absent")?;
        gptgrep_parse::validate_ocr_result(&task, result)?;
        let regions = doc
            .artifacts
            .iter()
            .find(|a| {
                a.artifact.kind == ArtifactKind::OcrRegions && a.artifact.page == Some(task.page)
            })
            .context("raw OCR regions absent")?;
        ensure!(
            digest(&serde_json::to_vec(result)?) == regions.artifact.sha256,
            "raw OCR result differs from receipt"
        );
        let mut ordered = result.regions.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|r| r.reading_order);
        let raw = ordered
            .into_iter()
            .map(|r| format!("{}\n", r.text))
            .collect::<String>();
        let text = doc
            .artifacts
            .iter()
            .find(|a| {
                a.artifact.kind == ArtifactKind::OcrText && a.artifact.page == Some(task.page)
            })
            .context("raw OCR text absent")?;
        ensure!(
            digest(raw.as_bytes()) == text.artifact.sha256,
            "raw OCR text differs from regions"
        );
    }
    let merged = doc
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == ArtifactKind::MergedText)
        .context("merged text absent")?;
    let bytes = bounded_read(
        &owned_path(dir, &merged.artifact.relative_path)?,
        artifact_limit(&merged.artifact, budget),
    )?;
    ensure!(
        digest(&bytes) == doc.document.text_sha256,
        "merged document text differs from bound artifact"
    );
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        doc.document
            .pages
            .last()
            .is_some_and(|p| p.line_end == text.lines().count().max(1)),
        "merged text/page line coverage mismatch"
    );
    let mut last = 0;
    for segment in &doc.segments {
        ensure!(
            segment.merged_start >= last && segment.merged_end >= segment.merged_start,
            "overlapping/invalid merged segments"
        );
        last = segment.merged_end;
        let artifact = doc
            .artifacts
            .iter()
            .find(|a| a.artifact.id == segment.artifact_id)
            .context("unknown segment source")?;
        ensure!(
            artifact.artifact.kind == segment.kind
                && is_text(&segment.kind)
                && artifact.artifact.page == Some(segment.page),
            "segment source kind/page mismatch"
        );
        let source = read_text(dir, &artifact.artifact)?;
        let start = usize::try_from(segment.merged_start)?;
        let end = usize::try_from(segment.merged_end)?;
        let astart = usize::try_from(segment.artifact_start)?;
        let aend = usize::try_from(segment.artifact_end)?;
        ensure!(
            text.get(start..end).is_some() && text.get(start..end) == source.get(astart..aend),
            "merged segment bytes do not map exactly to original artifact"
        );
        let page = doc
            .document
            .pages
            .iter()
            .find(|p| p.number == segment.page)
            .context("segment physical page absent")?;
        ensure!(start < end, "empty merged segment");
        let start_line = text[..start].bytes().filter(|b| *b == b'\n').count() + 1;
        let last_line = text.as_bytes()[..end - 1]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        ensure!(
            start_line >= page.line_start && last_line <= page.line_end,
            "merged segment crosses its physical page projection"
        );
    }
    let mut byte_start = 0usize;
    let mut segment_index = 0usize;
    let mut unmapped = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        let end = byte_start + content.len();
        if !content.is_empty() {
            while segment_index < doc.segments.len()
                && doc.segments[segment_index].merged_end <= byte_start as u64
            {
                segment_index += 1;
            }
            let mut covered = byte_start as u64;
            let mut cursor = segment_index;
            while let Some(segment) = doc.segments.get(cursor) {
                if segment.merged_start > covered || segment.merged_start >= end as u64 {
                    break;
                }
                covered = covered.max(segment.merged_end);
                cursor += 1;
            }
            if covered < end as u64 {
                unmapped += 1;
            }
        }
        byte_start += line.len();
    }
    ensure!(
        unmapped == doc.unmapped_merged_lines,
        "unmapped merged-line summary differs from verified segments"
    );
    verify_source(root, doc)?;
    Ok(())
}

fn verify_source(root: &Path, doc: &RichDocument) -> Result<()> {
    ensure!(
        hash_file(&source_path(root, &doc.document.path)?, MAX_SOURCE_BYTES)?
            == (
                doc.document.source_bytes,
                doc.document.source_sha256.clone()
            ),
        "source changed or disappeared: {}",
        doc.document.path
    );
    Ok(())
}
fn verify_artifact(dir: &Path, artifact: &ArtifactRef, budget: &ResourceBudget) -> Result<()> {
    ensure!(
        hash_file(
            &owned_path(dir, &artifact.relative_path)?,
            artifact_limit(artifact, budget)
        )? == (artifact.bytes, artifact.sha256.clone()),
        "artifact bytes changed: {}",
        artifact.id
    );
    Ok(())
}
fn read_text(dir: &Path, artifact: &ArtifactRef) -> Result<String> {
    ensure!(is_text(&artifact.kind), "artifact is not native/OCR text");
    verify_artifact(dir, artifact, &ResourceBudget::default())?;
    let bytes = bounded_read(&owned_path(dir, &artifact.relative_path)?, artifact.bytes)?;
    ensure!(
        digest(&bytes) == artifact.sha256,
        "artifact changed while reading"
    );
    Ok(String::from_utf8(bytes)?)
}
fn artifact_limit(a: &ArtifactRef, b: &ResourceBudget) -> u64 {
    match a.kind {
        ArtifactKind::Rendition => b.image_file_bytes,
        ArtifactKind::OcrText | ArtifactKind::OcrRegions => b.ocr_result_bytes,
        ArtifactKind::NativeText | ArtifactKind::NativeMetadata => b.page_layout_bytes,
        ArtifactKind::Layout | ArtifactKind::MergedText => b.document_metadata_bytes,
    }
}
fn is_text(kind: &ArtifactKind) -> bool {
    matches!(kind, ArtifactKind::NativeText | ArtifactKind::OcrText)
}
fn namespaced_id(doc: &str, parser_id: &str) -> String {
    format!(
        "a{}",
        digest(format!("gptgrep.artifact.v2\0{doc}\0{parser_id}").as_bytes())
    )
}
fn storage_path(doc: &str, a: &ArtifactRef) -> PathBuf {
    if is_text(&a.kind) {
        Path::new("artifact-text").join(format!("{}.txt", a.id))
    } else if a.kind == ArtifactKind::MergedText {
        Path::new("text").join(format!("{doc}.txt"))
    } else {
        Path::new("artifacts").join(doc).join(format!(
            "{}.{}",
            a.id,
            match a.mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                _ => "json",
            }
        ))
    }
}
fn same_artifact(a: &ArtifactRef, b: &ArtifactRef) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}
fn mapped<'a>(map: &'a BTreeMap<String, ArtifactRef>, a: &ArtifactRef) -> Result<&'a ArtifactRef> {
    map.get(&a.id)
        .context("page artifact absent from final artifact set")
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn owned_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && relative.components().collect::<PathBuf>().as_os_str() == relative.as_os_str(),
        "invalid artifact relative path"
    );
    ensure!(
        root.canonicalize()? == root && !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "artifact root changed"
    );
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "artifact path contains symlink"
        );
    }
    Ok(path)
}
fn hash_file(path: &Path, limit: u64) -> Result<(u64, String)> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit,
        "artifact/source type or size rejected"
    );
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        count = count
            .checked_add(size as u64)
            .context("file byte overflow")?;
        ensure!(count <= limit, "file grew beyond admission");
        hash.update(&buffer[..size]);
    }
    Ok((count, format!("{:x}", hash.finalize())))
}
fn copy_verified(
    from_root: &Path,
    from: &Path,
    to_root: &Path,
    to: &Path,
    bytes: u64,
    sha: &str,
    budget: &ResourceBudget,
) -> Result<()> {
    ensure!(
        valid_digest(sha) && bytes <= budget.document_staging_bytes,
        "invalid imported artifact bound"
    );
    let source = owned_path(from_root, from)?;
    ensure!(
        hash_file(&source, bytes)? == (bytes, sha.into()),
        "staging artifact changed before copy"
    );
    ensure!(
        to.parent().is_some_and(|p| owned_path(to_root, p).is_ok()),
        "destination parent is not owned"
    );
    budget.reserve_retained_staging(bytes)?;
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(to_root.join(to))?;
    let mut copied = 0u64;
    let mut buffer = [0u8; 65536];
    while copied < bytes {
        let admitted = usize::try_from((bytes - copied).min(buffer.len() as u64))?;
        let read = input.read(&mut buffer[..admitted])?;
        ensure!(read > 0, "staging artifact shortened during copy");
        output.write_all(&buffer[..read])?;
        copied += read as u64;
    }
    ensure!(
        input.read(&mut buffer[..1])? == 0,
        "staging artifact grew during copy"
    );
    output.sync_all()?;
    ensure!(
        hash_file(&to_root.join(to), bytes)? == (bytes, sha.into()),
        "staging artifact digest changed during copy"
    );
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
struct CountWriter {
    written: u64,
    limit: u64,
}
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self
            .written
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("resource_limit: serialized output"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_size(value: &impl Serialize, limit: u64) -> Result<u64> {
    let mut writer = CountWriter { written: 0, limit };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.written)
}
fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        ensure!(
            !entry.file_type()?.is_symlink(),
            "generation contains symlink"
        );
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}
fn line_range(text: &str, number: usize) -> Result<(usize, usize)> {
    ensure!(number > 0, "invalid line number");
    let mut offset = 0;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let end = offset + line.trim_end_matches('\n').len();
        if i + 1 == number {
            return Ok((offset, end));
        }
        offset += line.len();
    }
    anyhow::bail!("artifact index line outside saved text")
}

#[cfg(test)]
mod tests;
