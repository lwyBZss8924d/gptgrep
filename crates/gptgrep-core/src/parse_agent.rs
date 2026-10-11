//! Invocation-owned prepared-document toolbox. It grants neither storage
//! admission nor provider authority. Callers supply admitted paths/budgets and
//! an admitted OcrExecutor through the typed API; JSON cannot submit OCR results.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use gptgrep_parse::{
    ArtifactKind, ArtifactRef, BoundingBox, NativeCandidate, NativeOcrCrop, NativeOptions,
    NativePage, OcrExecutor, OcrImage, OcrMode, OcrPageResult, OcrTask, PreparationOptions,
    PreparedDocument, PreparedSummary, ResourceBudget, RichParsedDocument, crop_rendition,
    extract_native, finalize_document, prepare_document, validate_ocr_result, validate_ocr_task,
    validate_rendition_metadata,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::custody::{HeldDirectory, VerifiedFile};
use crate::read_cache::ReadMeter;

pub const REQUEST_SCHEMA: &str = "gptgrep.parse-toolbox.request.v1";
pub const REPLY_SCHEMA: &str = "gptgrep.parse-toolbox.reply.v1";
const MAX_REQUEST: usize = 65_536;
const MAX_REPLY: usize = 4_194_304;
const MAX_WINDOW: usize = 32_768;
const MAX_ROWS: usize = 128;
const MAX_CURSORS: usize = 1024;
const MAX_SOURCES: usize = 64;
static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(0);

/// The same complete supported-source selection used by rich ingestion. It
/// grants neither source registration nor parser/provider execution authority.
#[derive(Clone, Debug, Serialize)]
pub struct IndexSource {
    pub relative_path: String,
    pub path: PathBuf,
}
#[derive(Clone, Debug, Serialize)]
pub struct IndexSourceInventory {
    pub root: PathBuf,
    pub sources: Vec<IndexSource>,
    pub excluded_unsupported_files: usize,
}
pub fn index_source_inventory(root: &Path, max_files: usize) -> Result<IndexSourceInventory> {
    ensure!(
        (1..=100_000).contains(&max_files),
        "parse_agent_index_max_files"
    );
    crate::no_symlink(root)?;
    let root = root.canonicalize()?;
    ensure!(root.is_dir(), "parse_agent_index_root");
    let (paths, excluded_unsupported_files) =
        crate::rich_ingest::selected_sources(&root, max_files)?;
    let mut sources = Vec::with_capacity(paths.len());
    for relative_path in paths {
        let path = crate::source_path(&root, &relative_path)?;
        ensure!(
            gptgrep_parse::supports_path(&path),
            "parse_agent_index_unsupported_source"
        );
        sources.push(IndexSource {
            relative_path,
            path,
        });
    }
    Ok(IndexSourceInventory {
        root,
        sources,
        excluded_unsupported_files,
    })
}

/// Wire/scratch allowance is reserved before passing a tighter same-invocation
/// budget to the parser. One active document avoids multiplying retained native
/// candidates. These are logical owned bounds, not a process/native RSS cap.
#[derive(Clone, Debug)]
pub struct ToolboxLimits {
    pub request_bytes: usize,
    pub reply_bytes: usize,
    pub artifact_window_bytes: usize,
    pub operation_working_bytes: u64,
}
impl Default for ToolboxLimits {
    fn default() -> Self {
        Self {
            request_bytes: MAX_REQUEST,
            reply_bytes: MAX_REPLY,
            artifact_window_bytes: MAX_WINDOW,
            operation_working_bytes: 33_554_432,
        }
    }
}
impl ToolboxLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.request_bytes > 0 && self.request_bytes <= MAX_REQUEST,
            "parse_toolbox_request_limit"
        );
        ensure!(
            (1024..=MAX_REPLY).contains(&self.reply_bytes),
            "parse_toolbox_reply_limit"
        );
        ensure!(
            self.artifact_window_bytes > 0 && self.artifact_window_bytes <= MAX_WINDOW,
            "parse_toolbox_window_limit"
        );
        ensure!(
            self.operation_working_bytes > 0,
            "parse_toolbox_working_limit"
        );
        Ok(())
    }
    fn wire_reservation(&self) -> Result<u64> {
        (self.reply_bytes as u64)
            .checked_mul(4)
            .and_then(|n| n.checked_add((self.artifact_window_bytes as u64) * 2))
            .and_then(|n| n.checked_add(2_097_152))
            .context("parse_toolbox_working_overflow")
    }
}

fn default_reply() -> usize {
    262_144
}
fn default_rows() -> usize {
    32
}
fn default_window() -> usize {
    16_384
}
fn default_page_window() -> usize {
    25
}
fn default_dpi() -> f32 {
    96.0
}
fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolboxRequest {
    pub schema: String,
    pub id: String,
    #[serde(default = "default_reply")]
    pub max_reply_bytes: usize,
    pub operation: ToolboxOperation,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolboxOperation {
    Prepare {
        source: String,
    },
    ExtractNative {
        handle: String,
        #[serde(default = "default_page_window")]
        page_window: usize,
        #[serde(default = "default_dpi")]
        dpi: f32,
        #[serde(default = "yes")]
        render_pages: bool,
    },
    InspectPages {
        handle: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_rows")]
        page_size: usize,
    },
    InspectArtifacts {
        handle: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_rows")]
        page_size: usize,
    },
    ReadMetadataWindow {
        handle: String,
        artifact_id: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_window")]
        max_bytes: usize,
    },
    ReadArtifact {
        handle: String,
        artifact_id: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_window")]
        max_bytes: usize,
    },
    InspectImages {
        handle: String,
        #[serde(default)]
        page: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_rows")]
        page_size: usize,
    },
    Crop {
        handle: String,
        artifact_id: String,
        bbox: BoundingBox,
    },
    InspectOcrPlan {
        handle: String,
        #[serde(default)]
        section: OcrPlanSection,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_rows")]
        page_size: usize,
    },
    Finalize {
        handle: String,
        mode: OcrMode,
    },
    Close {
        handle: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrPlanSection {
    #[default]
    Tasks,
    Pages,
    Blockers,
}

struct SourcePin {
    path: PathBuf,
    parent: HeldDirectory,
    leaf: PathBuf,
    bytes: u64,
    sha256: String,
}

// Exactly one document stage is retained per admitted invocation. Keep its
// bounded state inline rather than add another heap allocation to the parser's
// owned working storage merely to reduce the size of this single enum.
#[allow(clippy::large_enum_variant)]
enum Stage {
    Prepared,
    Native(NativeCandidate),
    Finalized(RichParsedDocument, OcrMode),
    Failed,
}
impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Native(_) => "native",
            Self::Finalized(_, _) => "finalized",
            Self::Failed => "failed",
        }
    }
    fn pages(&self) -> Result<&[NativePage]> {
        match self {
            Self::Native(candidate) => Ok(candidate.pages()),
            Self::Finalized(document, _) => Ok(&document.pages),
            _ => Err(anyhow::anyhow!("parse_toolbox_native_stage_required")),
        }
    }
    fn tasks(&self) -> Result<&[OcrTask]> {
        match self {
            Self::Native(candidate) => Ok(&candidate.plan().tasks),
            Self::Finalized(document, _) => Ok(&document.ocr_tasks),
            _ => Err(anyhow::anyhow!("parse_toolbox_native_stage_required")),
        }
    }
    fn coverage(&self) -> Result<&[(u32, gptgrep_parse::CoverageState)]> {
        match self {
            Self::Native(candidate) => Ok(&candidate.plan().pages),
            Self::Finalized(document, _) => Ok(&document.coverage),
            _ => Err(anyhow::anyhow!("parse_toolbox_native_stage_required")),
        }
    }
    fn native_profile_sha256(&self) -> Option<&str> {
        match self {
            Self::Native(candidate) => Some(candidate.native_profile_sha256()),
            Self::Finalized(document, _) => Some(&document.native_profile_sha256),
            _ => None,
        }
    }
}

struct DocumentState {
    handle: String,
    source_alias: String,
    prepared: PreparedDocument,
    root: HeldDirectory,
    stage: Stage,
    revision: u64,
    original_pages: Option<usize>,
    artifacts: BTreeMap<String, ArtifactRef>,
    catalog_working: u64,
    plan_blockers: Vec<String>,
    plan_working: u64,
    outcomes: BTreeMap<String, (OcrPageResult, u64)>,
    failed_ocr: std::collections::BTreeSet<String>,
}
impl DocumentState {
    fn side_retained(&self) -> Result<u64> {
        let base = self
            .catalog_working
            .checked_add(self.plan_working)
            .and_then(|n| n.checked_add((self.failed_ocr.len() as u64).checked_mul(1024)?))
            .context("parse_toolbox_working_overflow")?;
        self.outcomes.values().try_fold(base, |n, (_, bytes)| {
            n.checked_add(*bytes)
                .context("parse_toolbox_working_overflow")
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CursorKind {
    Pages,
    Artifacts,
    Images(Option<u32>),
    Plan(OcrPlanSection),
    Artifact(String, bool),
}
struct Cursor {
    handle: String,
    revision: u64,
    kind: CursorKind,
    position: usize,
}

/// One explicitly admitted invocation. No default temp directory, account,
/// network, model, index writer or generation publication is constructed here.
pub struct ParseToolbox {
    staging_path: PathBuf,
    staging: HeldDirectory,
    namespace: String,
    serial: u64,
    budget: ResourceBudget,
    parser_budget: ResourceBudget,
    limits: ToolboxLimits,
    sources: BTreeMap<String, SourcePin>,
    document: Option<DocumentState>,
    cursors: BTreeMap<String, Cursor>,
    meter: ReadMeter,
}

#[derive(Debug, Serialize)]
pub struct OcrExecutionSummary {
    pub task_sha256: String,
    pub page: u32,
    pub status: gptgrep_parse::OcrResultStatus,
    pub metadata_validated: bool,
}

impl ParseToolbox {
    /// The host supplies a unique nonzero nonce from its admitted invocation.
    /// This is an application capability namespace, never a native session ID.
    pub fn new(
        staging_path: &Path,
        invocation_nonce: [u8; 32],
        budget: ResourceBudget,
        limits: ToolboxLimits,
    ) -> Result<Self> {
        budget.validate()?;
        limits.validate()?;
        ensure!(
            invocation_nonce.iter().any(|byte| *byte != 0),
            "parse_toolbox_nonce_required"
        );
        let staging = HeldDirectory::open_absolute(staging_path)?;
        let serial = NEXT_INVOCATION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("parse_toolbox_namespace_exhausted"))?;
        let namespace = sha(&serde_json::to_vec(&(
            "gptgrep.parse-toolbox.namespace.v1",
            invocation_nonce,
            serial,
        ))?);
        let parser_working = budget
            .owned_working_bytes
            .checked_sub(limits.wire_reservation()?)
            .and_then(|n| n.checked_sub(limits.operation_working_bytes))
            .filter(|n| *n > 0)
            .context("parse_toolbox_working_limit")?;
        let mut parser_budget = budget.clone();
        parser_budget.owned_working_bytes = parser_working;
        parser_budget.validate()?;
        Ok(Self {
            staging_path: staging_path.to_path_buf(),
            staging,
            namespace,
            serial: 0,
            budget,
            parser_budget,
            limits,
            sources: BTreeMap::new(),
            document: None,
            cursors: BTreeMap::new(),
            meter: ReadMeter::default(),
        })
    }

    /// Typed host-only source admission. JSON can select an alias, never grant a
    /// new path. The caller is responsible for authorizing this exact source pin.
    pub fn register_source(
        &mut self,
        alias: &str,
        path: &Path,
        bytes: u64,
        sha256: &str,
    ) -> Result<()> {
        ensure!(
            !alias.is_empty() && alias.len() <= 128 && self.sources.len() < MAX_SOURCES,
            "parse_toolbox_source_registry_limit"
        );
        ensure!(
            !self.sources.contains_key(alias),
            "parse_toolbox_source_alias_exists"
        );
        ensure!(
            bytes <= self.budget.document_staging_bytes,
            "parse_toolbox_source_limit"
        );
        let parent_path = path.parent().context("parse_toolbox_source_path")?;
        let leaf = PathBuf::from(path.file_name().context("parse_toolbox_source_path")?);
        let parent = HeldDirectory::open_absolute(parent_path)?;
        parent.open_verified(
            &leaf,
            Some(bytes),
            sha256,
            self.budget.document_staging_bytes,
            &self.meter,
        )?;
        self.sources.insert(
            alias.into(),
            SourcePin {
                path: path.to_path_buf(),
                parent,
                leaf,
                bytes,
                sha256: sha256.into(),
            },
        );
        Ok(())
    }

    pub fn staging_used(&self) -> u64 {
        self.budget.staging_used()
    }

    /// Host-only transfer of the same invocation's original shared counters.
    /// Cloning this budget never resets retained staging or grants publication.
    pub fn resource_budget(&self) -> ResourceBudget {
        self.budget.clone()
    }

    /// Typed host integration only. JSON replies never reveal staging paths.
    pub fn prepared_staging_root(&self, handle: &str) -> Result<&Path> {
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        Ok(doc.prepared.staging_root())
    }
    /// Controller-only complete-image access. JSON/tool reply limits are not
    /// image transport budgets. The verified buffer cannot outlive this call;
    /// the controller must charge its quotas and retained copies in `consume`.
    /// No source path, image registration, model tool or publication is granted.
    pub fn with_verified_rendition<T>(
        &self,
        handle: &str,
        artifact_id: &str,
        consume: impl FnOnce(&OcrImage, &[u8]) -> Result<T>,
    ) -> Result<T> {
        let doc = self.doc(handle)?;
        let artifact = self.artifact(doc, artifact_id)?;
        let image = image(doc, artifact_id)?;
        ensure!(
            artifact.id == image.artifact.id
                && artifact.kind == image.artifact.kind
                && artifact.relative_path == image.artifact.relative_path
                && artifact.sha256 == image.artifact.sha256
                && artifact.profile_sha256 == image.artifact.profile_sha256
                && artifact.bytes == image.artifact.bytes
                && artifact.mime == image.artifact.mime
                && artifact.page == image.artifact.page
                && artifact.region_id == image.artifact.region_id,
            "parse_toolbox_artifact_binding"
        );
        ensure!(artifact.bytes > 0, "parse_toolbox_image_size");
        validate_rendition_metadata(image, &self.parser_budget)?;
        // This allowance was removed from the parser's original 256 MiB
        // working admission at construction. Reserve the complete encoded
        // buffer and custody scratch alongside retained metadata before read.
        let additional = artifact
            .bytes
            .checked_add(65_536)
            .context("parse_toolbox_working_overflow")?;
        self.operation_budget(doc, additional)?;
        let mut held = self.open_artifact(doc, artifact)?;
        let bytes = held.read_bounded(self.budget.image_file_bytes, &self.meter)?;
        let result = consume(image, &bytes)?;
        // The FD remains held across the controller copy. No grant survives a
        // replaced artifact, parent directory or original/prepared source.
        held.check()?;
        self.fresh(doc)?;
        Ok(result)
    }

    /// Borrow the complete finalized product without granting publication.
    pub fn finalized_document(&self, handle: &str) -> Result<&RichParsedDocument> {
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        match &doc.stage {
            Stage::Finalized(document, _) => {
                for artifact in doc.artifacts.values() {
                    self.open_artifact(doc, artifact)?;
                }
                Ok(document)
            }
            _ => Err(anyhow::anyhow!("parse_toolbox_finalized_stage_required")),
        }
    }

    /// Move the complete verified product to a typed publisher without parsing
    /// again or cloning document contents. Source and every artifact are checked
    /// before ownership changes; failure leaves the active handle intact.
    /// Transferred handles/cursors are invalidated, while files and the original
    /// shared staging charge remain retained.
    pub fn take_finalized_document(&mut self, handle: &str) -> Result<RichParsedDocument> {
        self.finalized_document(handle)?;
        let document = self
            .document
            .take()
            .context("parse_toolbox_unknown_handle")?;
        self.cursors.clear();
        match document.stage {
            Stage::Finalized(parsed, _) => Ok(parsed),
            _ => Err(anyhow::anyhow!("parse_toolbox_finalized_stage_required")),
        }
    }

    fn token(&mut self, kind: &str) -> Result<String> {
        self.serial = self
            .serial
            .checked_add(1)
            .context("parse_toolbox_token_exhausted")?;
        Ok(sha(&serde_json::to_vec(&(
            "gptgrep.parse-toolbox.token.v1",
            &self.namespace,
            kind,
            self.serial,
        ))?))
    }
    fn doc(&self, handle: &str) -> Result<&DocumentState> {
        let doc = self
            .document
            .as_ref()
            .context("parse_toolbox_unknown_handle")?;
        ensure!(doc.handle == handle, "parse_toolbox_unknown_handle");
        Ok(doc)
    }
    fn fresh(&self, doc: &DocumentState) -> Result<()> {
        self.staging.validate()?;
        doc.root.validate()?;
        let source = self
            .sources
            .get(&doc.source_alias)
            .context("parse_toolbox_source_not_admitted")?;
        source.parent.open_verified(
            &source.leaf,
            Some(source.bytes),
            &source.sha256,
            self.budget.document_staging_bytes,
            &self.meter,
        )?;
        let prepared_leaf = Path::new(
            doc.prepared
                .prepared_path()
                .file_name()
                .context("parse_toolbox_prepared_path")?,
        );
        doc.root.open_verified(
            prepared_leaf,
            Some(doc.prepared.summary().prepared_bytes),
            &doc.prepared.summary().prepared_sha256,
            self.budget.document_staging_bytes,
            &self.meter,
        )?;
        doc.prepared.verify(&self.parser_budget)?;
        Ok(())
    }
    fn artifact<'a>(&self, doc: &'a DocumentState, id: &str) -> Result<&'a ArtifactRef> {
        doc.artifacts
            .get(id)
            .context("parse_toolbox_unknown_artifact")
    }
    fn open_artifact(&self, doc: &DocumentState, artifact: &ArtifactRef) -> Result<VerifiedFile> {
        self.fresh(doc)?;
        ensure!(
            artifact.id
                == gptgrep_parse::artifact_id(
                    &doc.prepared.summary().source_sha256,
                    &artifact.profile_sha256,
                    &artifact.kind,
                    artifact.page,
                    artifact.region_id.as_deref(),
                    &artifact.sha256
                )?,
            "parse_toolbox_artifact_binding"
        );
        doc.root.open_verified(
            &artifact.relative_path,
            Some(artifact.bytes),
            &artifact.sha256,
            self.budget.document_staging_bytes,
            &self.meter,
        )
    }
    fn position(
        &self,
        doc: &DocumentState,
        kind: &CursorKind,
        token: Option<&str>,
    ) -> Result<usize> {
        match token {
            None => Ok(0),
            Some(token) => {
                let cursor = self
                    .cursors
                    .get(token)
                    .context("parse_toolbox_unknown_cursor")?;
                ensure!(
                    cursor.handle == doc.handle
                        && cursor.revision == doc.revision
                        && cursor.kind == *kind,
                    "parse_toolbox_cursor_binding"
                );
                Ok(cursor.position)
            }
        }
    }
    fn commit_cursor(
        &mut self,
        old: Option<&str>,
        token: String,
        cursor: Option<Cursor>,
    ) -> Result<()> {
        ensure!(
            cursor.is_none() || old.is_some() || self.cursors.len() < MAX_CURSORS,
            "parse_toolbox_cursor_limit"
        );
        if let Some(old) = old {
            self.cursors.remove(old);
        }
        if let Some(cursor) = cursor {
            self.cursors.insert(token, cursor);
        }
        Ok(())
    }
    fn operation_budget(&self, doc: &DocumentState, additional: u64) -> Result<ResourceBudget> {
        let retained = doc
            .side_retained()?
            .checked_add(additional)
            .context("parse_toolbox_working_overflow")?;
        let available = self
            .limits
            .operation_working_bytes
            .checked_sub(retained)
            .filter(|n| *n > 0)
            .context("parse_toolbox_working_limit")?;
        let mut budget = self.budget.clone();
        budget.owned_working_bytes = available;
        budget.validate()?;
        Ok(budget)
    }
    fn native_crop_scope(&self, doc: &DocumentState) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"gptgrep.parse-toolbox.native-crop-scope.v1\0");
        hash.update(self.namespace.as_bytes());
        hash.update(b"\0");
        hash.update(doc.handle.as_bytes());
        hash.finalize().into()
    }

    /// Executes one registered source-derived task using the host's admitted
    /// executor. No JSON operation can inject a result, executor or receipt.
    /// Failed submissions are consumed here: retry requires a new invocation.
    pub async fn execute_ocr(
        &mut self,
        handle: &str,
        task_sha256: &str,
        executor: &dyn OcrExecutor,
    ) -> Result<OcrExecutionSummary> {
        self.execute_ocr_with_native_crops(handle, task_sha256, Vec::new(), executor)
            .await
    }

    /// The only supplementary input is an opaque crop constructed from the
    /// registered source below. Original task identity and submit-once state stay
    /// unchanged. The executor cannot return a crop outside this finite set.
    pub async fn execute_ocr_with_native_crops(
        &mut self,
        handle: &str,
        task_sha256: &str,
        crops: Vec<NativeOcrCrop>,
        executor: &dyn OcrExecutor,
    ) -> Result<OcrExecutionSummary> {
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        ensure!(
            matches!(&doc.stage, Stage::Native(_)),
            "parse_toolbox_native_stage_required"
        );
        ensure!(
            !doc.outcomes.contains_key(task_sha256) && !doc.failed_ocr.contains(task_sha256),
            "parse_toolbox_ocr_submission_consumed"
        );
        let task = doc
            .stage
            .tasks()?
            .iter()
            .find(|task| task.task_sha256 == task_sha256)
            .context("parse_toolbox_unknown_task")?;
        self.verify_task(doc, task)?;
        gptgrep_parse::validate_native_crop_set(task, &crops)?;
        let crop_working = crops.iter().try_fold(0u64, |n, crop| {
            crop.verify_document_scope(&self.native_crop_scope(doc))?;
            crop.validate_for_task(task)?;
            n.checked_add(task.budget.resources.image_file_bytes)
                .and_then(|n| n.checked_add(65_536))
                .context("parse_toolbox_working_overflow")
        })?;
        let reservation = task
            .budget
            .resources
            .ocr_result_bytes
            .checked_mul(16)
            .context("parse_toolbox_working_overflow")?;
        let task_working = bounded_count(
            task,
            task.budget.resources.native_layout_input_bytes + 65_536,
        )?
        .checked_mul(32)
        .context("parse_toolbox_working_overflow")?;
        self.operation_budget(
            doc,
            reservation
                .checked_add(task_working)
                .and_then(|n| n.checked_add(crop_working))
                .and_then(|n| n.checked_add(1024))
                .context("parse_toolbox_working_overflow")?,
        )?;
        let crop_proofs: Vec<_> = crops.iter().map(|crop| crop.proof().clone()).collect();
        let task = task.clone();
        self.document
            .as_mut()
            .expect("handle was resolved")
            .failed_ocr
            .insert(task_sha256.into());
        let result = executor
            .execute_with_native_crops(task.clone(), crops)
            .await
            .map_err(|_| anyhow::anyhow!("parse_toolbox_ocr_executor_failed"))?;
        self.verify_task(self.doc(handle)?, &task)?;
        let working = result_working(&result, task.budget.resources.ocr_result_bytes)?;
        ensure!(
            working <= reservation,
            "parse_toolbox_ocr_result_working_limit"
        );
        gptgrep_parse::validate_ocr_result_metadata(&task, &result)?;
        for delivery in &result.receipt.image_deliveries {
            if let Some(proof) = delivery
                .native
                .as_ref()
                .and_then(|native| native.native_crop.as_ref())
            {
                let supplied = crop_proofs
                    .iter()
                    .find(|p| p.derived_artifact_id == proof.derived_artifact_id)
                    .context("parse_toolbox_unsubmitted_native_crop")?;
                ensure!(
                    serde_json::to_vec(supplied)? == serde_json::to_vec(proof)?,
                    "parse_toolbox_native_crop_delivery_changed"
                );
            }
        }
        validate_ocr_result(&task, &result)?;
        let summary = OcrExecutionSummary {
            task_sha256: task_sha256.into(),
            page: result.page,
            status: result.status.clone(),
            metadata_validated: true,
        };
        let doc = self.document.as_mut().expect("handle was resolved");
        doc.failed_ocr.remove(task_sha256);
        doc.outcomes.insert(task_sha256.into(), (result, working));
        Ok(summary)
    }

    /// Pure typed front door: no arbitrary path, result, task or rendition
    /// registration. The parent must belong to exactly one unconsumed task of
    /// this current source. Callers reserve retained output before this call.
    pub fn create_native_crop(
        &self,
        handle: &str,
        parent_artifact_id: &str,
        selected: BoundingBox,
    ) -> Result<NativeOcrCrop> {
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        ensure!(
            matches!(&doc.stage, Stage::Native(_)),
            "parse_toolbox_native_stage_required"
        );
        let mut tasks = doc.stage.tasks()?.iter().filter(|task| {
            task.images
                .iter()
                .any(|image| image.artifact.id == parent_artifact_id)
        });
        let task = tasks
            .next()
            .context("parse_toolbox_native_crop_unknown_parent")?;
        ensure!(
            tasks.next().is_none(),
            "parse_toolbox_native_crop_ambiguous_parent"
        );
        ensure!(
            !doc.outcomes.contains_key(&task.task_sha256)
                && !doc.failed_ocr.contains(&task.task_sha256),
            "parse_toolbox_ocr_submission_consumed"
        );
        self.verify_task(doc, task)?;
        let mut budget = self.operation_budget(doc, 65_536)?;
        budget.owned_working_bytes = budget
            .owned_working_bytes
            .min(task.budget.resources.owned_working_bytes);
        let artifact = self.artifact(doc, parent_artifact_id)?;
        let file = self.open_artifact(doc, artifact)?.into_file()?;
        let crop = gptgrep_parse::create_native_ocr_crop(
            task,
            parent_artifact_id,
            file,
            selected,
            &budget,
        )?;
        self.open_artifact(doc, artifact)?;
        crop.bind_document_scope(self.native_crop_scope(doc))
    }

    fn verify_task(&self, doc: &DocumentState, task: &OcrTask) -> Result<()> {
        self.fresh(doc)?;
        validate_ocr_task(task)?;
        for image in &task.images {
            self.open_artifact(doc, self.artifact(doc, &image.artifact.id)?)?;
        }
        Ok(())
    }

    /// Bounded closed JSON front door. Errors never copy parser paths, source
    /// text, provider bodies or caller-selected error strings into public output.
    pub async fn dispatch_json(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if input.len() > self.limits.request_bytes {
            return encode_error("", "request_limit", 1024);
        }
        let request: ToolboxRequest = match serde_json::from_slice(input) {
            Ok(request) => request,
            Err(_) => return encode_error("", "request_schema", 1024),
        };
        if request.schema != REQUEST_SCHEMA
            || request.id.len() > 128
            || !(1024..=self.limits.reply_bytes).contains(&request.max_reply_bytes)
        {
            return encode_error("", "request_schema", 1024);
        }
        let id = request.id.clone();
        let cap = request.max_reply_bytes;
        match self.dispatch(request).await {
            Ok(reply) => Ok(reply),
            Err(_) => encode_error(&id, "operation_failed", cap),
        }
    }

    pub async fn dispatch(&mut self, request: ToolboxRequest) -> Result<Vec<u8>> {
        ensure!(
            request.schema == REQUEST_SCHEMA
                && request.id.len() <= 128
                && (1024..=self.limits.reply_bytes).contains(&request.max_reply_bytes),
            "parse_toolbox_request_schema"
        );
        bounded(&request, self.limits.request_bytes)?;
        let id = request.id;
        let cap = request.max_reply_bytes;
        let next_token = self.token("cursor")?;
        match request.operation {
            ToolboxOperation::Prepare { source } => {
                ensure!(self.document.is_none(), "parse_toolbox_one_active_document");
                self.staging.validate()?;
                let pin = self
                    .sources
                    .get(&source)
                    .context("parse_toolbox_source_not_admitted")?;
                pin.parent.open_verified(
                    &pin.leaf,
                    Some(pin.bytes),
                    &pin.sha256,
                    self.budget.document_staging_bytes,
                    &self.meter,
                )?;
                let prepared = prepare_document(
                    &pin.path,
                    &self.staging_path,
                    &PreparationOptions,
                    &self.parser_budget,
                )
                .await?;
                self.staging.validate()?;
                pin.parent.open_verified(
                    &pin.leaf,
                    Some(pin.bytes),
                    &pin.sha256,
                    self.budget.document_staging_bytes,
                    &self.meter,
                )?;
                ensure!(
                    prepared.summary().source_sha256 == pin.sha256
                        && prepared.summary().source_bytes == pin.bytes,
                    "parse_toolbox_source_changed"
                );
                prepared.verify(&self.parser_budget)?;
                let root = HeldDirectory::open_absolute(prepared.staging_root())?;
                let handle = self.token("document")?;
                let reply = encode(
                    &id,
                    &SummaryData {
                        handle: &handle,
                        stage: "prepared",
                        summary: prepared.summary(),
                        native_profile_sha256: None,
                        original_pages: None,
                        artifact_count: 0,
                        ocr_task_count: None,
                        ocr_blocker_count: None,
                        unmapped_merged_lines: None,
                        required_ocr_complete: None,
                    },
                    cap,
                )?;
                self.document = Some(DocumentState {
                    handle,
                    source_alias: source,
                    prepared,
                    root,
                    stage: Stage::Prepared,
                    revision: 0,
                    original_pages: None,
                    artifacts: BTreeMap::new(),
                    catalog_working: 0,
                    plan_blockers: Vec::new(),
                    plan_working: 0,
                    outcomes: BTreeMap::new(),
                    failed_ocr: std::collections::BTreeSet::new(),
                });
                Ok(reply)
            }
            ToolboxOperation::ExtractNative {
                handle,
                page_window,
                dpi,
                render_pages,
            } => {
                let doc = self.doc(&handle)?;
                self.fresh(doc)?;
                ensure!(
                    matches!(&doc.stage, Stage::Prepared),
                    "parse_toolbox_prepared_stage_required"
                );
                let candidate = extract_native(
                    &doc.prepared,
                    &NativeOptions {
                        page_window,
                        dpi,
                        render_pages,
                    },
                    &self.parser_budget,
                )
                .await?;
                self.fresh(self.doc(&handle)?)?;
                let pages = candidate
                    .document_metadata()
                    .map(|metadata| metadata.physical_page_count as usize)
                    .unwrap_or(0);
                ensure!(
                    candidate.pages().len() == pages,
                    "parse_toolbox_complete_pages"
                );
                ensure!(
                    candidate
                        .pages()
                        .iter()
                        .enumerate()
                        .all(|(index, page)| page.number as usize == index + 1),
                    "parse_toolbox_complete_pages"
                );
                let (artifacts, working) =
                    catalog(candidate.artifacts(), self.limits.operation_working_bytes)?;
                let plan_working = bounded_count(
                    &candidate.plan().blockers,
                    self.limits.operation_working_bytes,
                )?
                .checked_mul(16)
                .context("parse_toolbox_working_overflow")?;
                ensure!(
                    working
                        .checked_add(plan_working)
                        .is_some_and(|n| n < self.limits.operation_working_bytes),
                    "parse_toolbox_catalog_working_limit"
                );
                let plan_blockers = candidate.plan().blockers.clone();
                let doc = self.document.as_mut().expect("handle was resolved");
                doc.original_pages = Some(pages);
                doc.artifacts = artifacts;
                doc.catalog_working = working;
                doc.plan_blockers = plan_blockers;
                doc.plan_working = plan_working;
                doc.revision += 1;
                doc.stage = Stage::Native(candidate);
                self.cursors.clear();
                self.summary_reply(&id, &handle, cap)
            }
            ToolboxOperation::InspectPages {
                handle,
                cursor,
                page_size,
            } => {
                let (reply, next) =
                    self.pages_reply(&id, &handle, cursor.as_deref(), page_size, &next_token, cap)?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::InspectArtifacts {
                handle,
                cursor,
                page_size,
            } => {
                let (reply, next) = self.artifacts_reply(
                    &id,
                    &handle,
                    cursor.as_deref(),
                    page_size,
                    &next_token,
                    cap,
                )?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::ReadMetadataWindow {
                handle,
                artifact_id,
                cursor,
                max_bytes,
            } => {
                let (reply, next) = self.window_reply(
                    &id,
                    &handle,
                    &artifact_id,
                    cursor.as_deref(),
                    max_bytes,
                    true,
                    &next_token,
                    cap,
                )?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::ReadArtifact {
                handle,
                artifact_id,
                cursor,
                max_bytes,
            } => {
                let (reply, next) = self.window_reply(
                    &id,
                    &handle,
                    &artifact_id,
                    cursor.as_deref(),
                    max_bytes,
                    false,
                    &next_token,
                    cap,
                )?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::InspectImages {
                handle,
                page,
                cursor,
                page_size,
            } => {
                let (reply, next) = self.images_reply(
                    &id,
                    &handle,
                    page,
                    cursor.as_deref(),
                    page_size,
                    &next_token,
                    cap,
                )?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::Crop {
                handle,
                artifact_id,
                bbox,
            } => {
                let doc = self.doc(&handle)?;
                let image = image(doc, &artifact_id)?;
                let page = doc
                    .stage
                    .pages()?
                    .iter()
                    .find(|page| Some(page.number) == image.artifact.page)
                    .context("parse_toolbox_image_page")?;
                let budget = self.operation_budget(doc, 0)?;
                let file = self.open_artifact(doc, &image.artifact)?.into_file()?;
                let output = crop_rendition(
                    image,
                    file,
                    bbox,
                    (page.width_points, page.height_points),
                    &budget,
                )?;
                self.open_artifact(doc, &image.artifact)?;
                let preview = CropData {
                    kind: "complete_crop",
                    source_sha256: &doc.prepared.summary().source_sha256,
                    prepared_sha256: &doc.prepared.summary().prepared_sha256,
                    sha256: &output.sha256,
                    bytes: output.png.len(),
                    data_url: "",
                    recipe: &output.recipe,
                    recipe_sha256: &output.recipe_sha256,
                    admission: &output.admission,
                };
                admit_data_url_frame(&id, &preview, "image/png", output.png.len(), cap)?;
                let data_url = data_url("image/png", &output.png)?;
                encode(
                    &id,
                    &CropData {
                        data_url: &data_url,
                        ..preview
                    },
                    cap,
                )
            }
            ToolboxOperation::InspectOcrPlan {
                handle,
                section,
                cursor,
                page_size,
            } => {
                let (reply, next) = self.plan_reply(
                    &id,
                    &handle,
                    section,
                    cursor.as_deref(),
                    page_size,
                    &next_token,
                    cap,
                )?;
                self.commit_cursor(cursor.as_deref(), next_token, next)?;
                Ok(reply)
            }
            ToolboxOperation::Finalize { handle, mode } => {
                let doc = self.doc(&handle)?;
                self.fresh(doc)?;
                if let Stage::Finalized(_, selected_mode) = &doc.stage {
                    ensure!(
                        *selected_mode == mode,
                        "parse_toolbox_finalized_mode_changed"
                    );
                    return self.summary_reply(&id, &handle, cap);
                }
                ensure!(
                    matches!(&doc.stage, Stage::Native(_)),
                    "parse_toolbox_native_stage_required"
                );
                ensure!(
                    doc.failed_ocr.is_empty(),
                    "parse_toolbox_failed_ocr_submission"
                );
                if mode == OcrMode::Off {
                    ensure!(doc.outcomes.is_empty(), "parse_toolbox_ocr_off_results");
                } else {
                    ensure!(
                        doc.plan_blockers.is_empty(),
                        "parse_toolbox_required_ocr_blocked"
                    );
                    ensure!(
                        doc.stage
                            .tasks()?
                            .iter()
                            .all(|task| doc.outcomes.contains_key(&task.task_sha256)),
                        "parse_toolbox_required_ocr_pending"
                    );
                }
                let doc = self.document.as_mut().expect("handle was resolved");
                let Stage::Native(candidate) = std::mem::replace(&mut doc.stage, Stage::Failed)
                else {
                    unreachable!()
                };
                let outcomes = std::mem::take(&mut doc.outcomes)
                    .into_values()
                    .map(|(result, _)| result)
                    .collect();
                let parsed = finalize_document(candidate, outcomes, mode, &self.parser_budget)?;
                ensure!(
                    Some(parsed.pages.len()) == doc.original_pages
                        && parsed
                            .pages
                            .iter()
                            .enumerate()
                            .all(|(index, page)| page.number as usize == index + 1),
                    "parse_toolbox_complete_pages"
                );
                // The parser's complete product already owns the references.
                // Drop the previous facade catalog before allocating its replacement.
                doc.artifacts = BTreeMap::new();
                doc.catalog_working = 0;
                let (artifacts, working) =
                    catalog(&parsed.artifacts, self.limits.operation_working_bytes)?;
                ensure!(
                    working
                        .checked_add(doc.plan_working)
                        .is_some_and(|n| n < self.limits.operation_working_bytes),
                    "parse_toolbox_catalog_working_limit"
                );
                doc.artifacts = artifacts;
                doc.catalog_working = working;
                doc.revision += 1;
                doc.stage = Stage::Finalized(parsed, mode);
                self.cursors.clear();
                self.summary_reply(&id, &handle, cap)
            }
            ToolboxOperation::Close { handle } => {
                self.doc(&handle)?;
                let reply = encode(
                    &id,
                    &ClosedData {
                        handle: &handle,
                        closed: true,
                        shared_staging_used: self.budget.staging_used(),
                        retained_artifacts: true,
                    },
                    cap,
                )?;
                self.document = None;
                self.cursors.clear();
                Ok(reply)
            }
        }
    }

    fn summary_reply(&self, id: &str, handle: &str, cap: usize) -> Result<Vec<u8>> {
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        encode(
            id,
            &SummaryData {
                handle,
                stage: doc.stage.name(),
                summary: doc.prepared.summary(),
                native_profile_sha256: doc.stage.native_profile_sha256(),
                original_pages: doc.original_pages,
                artifact_count: doc.artifacts.len(),
                ocr_task_count: doc.stage.tasks().ok().map(|tasks| tasks.len()),
                ocr_blocker_count: doc.stage.tasks().ok().map(|_| doc.plan_blockers.len()),
                unmapped_merged_lines: match &doc.stage {
                    Stage::Finalized(document, _) => Some(document.unmapped_merged_lines),
                    _ => None,
                },
                required_ocr_complete: doc.stage.pages().ok().map(|pages| {
                    !pages
                        .iter()
                        .any(|page| page.coverage == gptgrep_parse::CoverageState::RequiredOcr)
                }),
            },
            cap,
        )
    }
    fn next(
        doc: &DocumentState,
        kind: CursorKind,
        position: usize,
        total: usize,
    ) -> Option<Cursor> {
        (position < total).then(|| Cursor {
            handle: doc.handle.clone(),
            revision: doc.revision,
            kind,
            position,
        })
    }
    fn pages_reply(
        &self,
        id: &str,
        handle: &str,
        cursor: Option<&str>,
        count: usize,
        token: &str,
        cap: usize,
    ) -> Result<(Vec<u8>, Option<Cursor>)> {
        rows(count)?;
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        let pages = doc.stage.pages()?;
        let start = self.position(doc, &CursorKind::Pages, cursor)?;
        ensure!(start <= pages.len(), "parse_toolbox_cursor_range");
        for end in (start..=start.saturating_add(count).min(pages.len())).rev() {
            let next = Self::next(doc, CursorKind::Pages, end, pages.len());
            if let Ok(reply) = encode(
                id,
                &RowsData {
                    kind: "pages",
                    total: pages.len(),
                    start,
                    rows: &pages[start..end],
                    next_cursor: next.as_ref().map(|_| token),
                },
                cap,
            ) {
                ensure!(
                    end > start || start == pages.len(),
                    "parse_toolbox_reply_limit"
                );
                return Ok((reply, next));
            }
        }
        Err(anyhow::anyhow!("parse_toolbox_reply_limit"))
    }
    #[allow(clippy::too_many_arguments)]
    fn images_reply(
        &self,
        id: &str,
        handle: &str,
        page: Option<u32>,
        cursor: Option<&str>,
        count: usize,
        token: &str,
        cap: usize,
    ) -> Result<(Vec<u8>, Option<Cursor>)> {
        rows(count)?;
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        let pages = doc.stage.pages()?;
        if let Some(page) = page {
            ensure!(
                page > 0 && page as usize <= pages.len(),
                "parse_toolbox_page_range"
            );
        }
        let total = pages
            .iter()
            .filter(|p| page.is_none_or(|n| n == p.number))
            .map(|page| page.renditions.len())
            .sum::<usize>();
        let kind = CursorKind::Images(page);
        let start = self.position(doc, &kind, cursor)?;
        ensure!(start <= total, "parse_toolbox_cursor_range");
        let selected = pages
            .iter()
            .filter(|p| page.is_none_or(|n| n == p.number))
            .flat_map(|page| page.renditions.iter())
            .skip(start)
            .take(count)
            .collect::<Vec<_>>();
        for length in (0..=selected.len()).rev() {
            let next = Self::next(doc, kind.clone(), start + length, total);
            if let Ok(reply) = encode(
                id,
                &RowsData {
                    kind: "images",
                    total,
                    start,
                    rows: &selected[..length],
                    next_cursor: next.as_ref().map(|_| token),
                },
                cap,
            ) {
                ensure!(length > 0 || start == total, "parse_toolbox_reply_limit");
                return Ok((reply, next));
            }
        }
        Err(anyhow::anyhow!("parse_toolbox_reply_limit"))
    }
    fn artifacts_reply(
        &self,
        id: &str,
        handle: &str,
        cursor: Option<&str>,
        count: usize,
        token: &str,
        cap: usize,
    ) -> Result<(Vec<u8>, Option<Cursor>)> {
        rows(count)?;
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        let start = self.position(doc, &CursorKind::Artifacts, cursor)?;
        let total = doc.artifacts.len();
        ensure!(start <= total, "parse_toolbox_cursor_range");
        let selected = doc
            .artifacts
            .values()
            .skip(start)
            .take(count)
            .collect::<Vec<_>>();
        for length in (0..=selected.len()).rev() {
            let next = Self::next(doc, CursorKind::Artifacts, start + length, total);
            if let Ok(reply) = encode(
                id,
                &RowsData {
                    kind: "artifacts",
                    total,
                    start,
                    rows: &selected[..length],
                    next_cursor: next.as_ref().map(|_| token),
                },
                cap,
            ) {
                ensure!(length > 0 || start == total, "parse_toolbox_reply_limit");
                return Ok((reply, next));
            }
        }
        Err(anyhow::anyhow!("parse_toolbox_reply_limit"))
    }
    #[allow(clippy::too_many_arguments)]
    fn plan_reply(
        &self,
        id: &str,
        handle: &str,
        section: OcrPlanSection,
        cursor: Option<&str>,
        count: usize,
        token: &str,
        cap: usize,
    ) -> Result<(Vec<u8>, Option<Cursor>)> {
        rows(count)?;
        let doc = self.doc(handle)?;
        self.fresh(doc)?;
        let kind = CursorKind::Plan(section.clone());
        let start = self.position(doc, &kind, cursor)?;
        let total = match &section {
            OcrPlanSection::Tasks => doc.stage.tasks()?.len(),
            OcrPlanSection::Pages => doc.stage.coverage()?.len(),
            OcrPlanSection::Blockers => {
                doc.stage.tasks()?;
                doc.plan_blockers.len()
            }
        };
        ensure!(start <= total, "parse_toolbox_cursor_range");
        for end in (start..=start.saturating_add(count).min(total)).rev() {
            let next = Self::next(doc, kind.clone(), end, total);
            let next_cursor = next.as_ref().map(|_| token);
            let reply = match &section {
                OcrPlanSection::Tasks => encode(
                    id,
                    &RowsData {
                        kind: "ocr_tasks",
                        total,
                        start,
                        rows: &doc.stage.tasks()?[start..end],
                        next_cursor,
                    },
                    cap,
                ),
                OcrPlanSection::Pages => encode(
                    id,
                    &RowsData {
                        kind: "ocr_page_coverage",
                        total,
                        start,
                        rows: &doc.stage.coverage()?[start..end],
                        next_cursor,
                    },
                    cap,
                ),
                OcrPlanSection::Blockers => encode(
                    id,
                    &RowsData {
                        kind: "ocr_blockers",
                        total,
                        start,
                        rows: &doc.plan_blockers[start..end],
                        next_cursor,
                    },
                    cap,
                ),
            };
            if let Ok(reply) = reply {
                ensure!(end > start || start == total, "parse_toolbox_reply_limit");
                return Ok((reply, next));
            }
        }
        Err(anyhow::anyhow!("parse_toolbox_reply_limit"))
    }
    #[allow(clippy::too_many_arguments)]
    fn window_reply(
        &self,
        id: &str,
        handle: &str,
        artifact_id: &str,
        cursor: Option<&str>,
        count: usize,
        metadata: bool,
        token: &str,
        cap: usize,
    ) -> Result<(Vec<u8>, Option<Cursor>)> {
        ensure!(
            count > 0 && count <= self.limits.artifact_window_bytes,
            "parse_toolbox_window_limit"
        );
        let doc = self.doc(handle)?;
        let artifact = self.artifact(doc, artifact_id)?;
        if metadata {
            ensure!(
                artifact.kind == ArtifactKind::NativeMetadata,
                "parse_toolbox_metadata_kind"
            );
        }
        if artifact.kind == ArtifactKind::Rendition {
            ensure!(
                !metadata && cursor.is_none(),
                "parse_toolbox_image_not_a_window"
            );
            let image = image(doc, artifact_id)?;
            validate_rendition_metadata(image, &self.parser_budget)?;
            let preview = ImageData {
                kind: "complete_image",
                artifact,
                width: image.width,
                height: image.height,
                page_to_raster: image.page_to_raster,
                data_url: "",
            };
            admit_data_url_frame(
                id,
                &preview,
                &artifact.mime,
                usize::try_from(artifact.bytes).context("parse_toolbox_artifact_size")?,
                cap,
            )?;
            let mut held = self.open_artifact(doc, artifact)?;
            let bytes = held.read_bounded(self.budget.image_file_bytes, &self.meter)?;
            let data_url = data_url(&artifact.mime, &bytes)?;
            return Ok((
                encode(
                    id,
                    &ImageData {
                        data_url: &data_url,
                        ..preview
                    },
                    cap,
                )?,
                None,
            ));
        }
        ensure!(
            artifact.mime.starts_with("text/") || artifact.mime == "application/json",
            "parse_toolbox_text_artifact_required"
        );
        let kind = CursorKind::Artifact(artifact_id.into(), metadata);
        let start = self.position(doc, &kind, cursor)?;
        let total = usize::try_from(artifact.bytes).context("parse_toolbox_artifact_size")?;
        ensure!(start <= total, "parse_toolbox_cursor_range");
        let mut held = self.open_artifact(doc, artifact)?;
        let amount = count.min(total - start);
        let mut raw = vec![0; amount];
        held.file_mut().seek(SeekFrom::Start(start as u64))?;
        let mut received = 0;
        while received < raw.len() {
            let read = held.file_mut().read(&mut raw[received..])?;
            self.meter.record_physical_read(read)?;
            ensure!(read > 0, "parse_toolbox_artifact_changed");
            received += read;
        }
        held.check()?;
        let text = match String::from_utf8(raw) {
            Ok(text) => text,
            Err(error) if error.utf8_error().error_len().is_none() => {
                let length = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(length);
                String::from_utf8(bytes).context("parse_toolbox_artifact_utf8")?
            }
            Err(_) => return Err(anyhow::anyhow!("parse_toolbox_artifact_utf8")),
        };
        ensure!(
            !text.is_empty() || start == total,
            "parse_toolbox_window_must_hold_one_character"
        );
        let make = |length: usize| {
            let end = start + length;
            let next = Self::next(doc, kind.clone(), end, total);
            encode(
                id,
                &WindowData {
                    kind: if metadata {
                        "metadata_window"
                    } else {
                        "artifact_window"
                    },
                    artifact,
                    offset: start,
                    total_bytes: total,
                    text: &text[..length],
                    eof: end == total,
                    next_cursor: next.as_ref().map(|_| token),
                },
                cap,
            )
            .map(|reply| (reply, next))
        };
        if let Ok(reply) = make(text.len()) {
            return Ok(reply);
        }
        let mut low = 0;
        let mut high = text.len();
        let mut best = None;
        while low < high {
            let mut middle = low + (high - low).div_ceil(2);
            while !text.is_char_boundary(middle) {
                middle -= 1;
            }
            if middle == low {
                middle = low
                    + text[low..]
                        .chars()
                        .next()
                        .context("parse_toolbox_artifact_utf8")?
                        .len_utf8();
                if middle > high {
                    break;
                }
            }
            match make(middle) {
                Ok(reply) => {
                    low = middle;
                    best = Some(reply);
                }
                Err(_) => {
                    high = middle - 1;
                }
            }
        }
        best.context("parse_toolbox_reply_must_hold_one_character")
    }
}

fn image<'a>(doc: &'a DocumentState, id: &str) -> Result<&'a OcrImage> {
    doc.stage
        .pages()?
        .iter()
        .flat_map(|page| page.renditions.iter())
        .find(|image| image.artifact.id == id)
        .context("parse_toolbox_unknown_image")
}
fn rows(count: usize) -> Result<()> {
    ensure!((1..=MAX_ROWS).contains(&count), "parse_toolbox_page_size");
    Ok(())
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn catalog(artifacts: &[ArtifactRef], cap: u64) -> Result<(BTreeMap<String, ArtifactRef>, u64)> {
    let encoded = bounded_count(artifacts, cap)?;
    let working = encoded
        .checked_mul(16)
        .context("parse_toolbox_working_overflow")?;
    ensure!(working < cap, "parse_toolbox_catalog_working_limit");
    let mut result = BTreeMap::new();
    for artifact in artifacts {
        ensure!(
            result
                .insert(artifact.id.clone(), artifact.clone())
                .is_none(),
            "parse_toolbox_duplicate_artifact"
        );
    }
    Ok((result, working))
}
fn result_working(result: &OcrPageResult, serialized_cap: u64) -> Result<u64> {
    let mut owned = size_of::<OcrPageResult>() as u64;
    add_allocation(
        &mut owned,
        result.regions.capacity(),
        size_of::<gptgrep_parse::OcrRegionResult>(),
    )?;
    for region in &result.regions {
        add_allocation(&mut owned, region.id.capacity(), 1)?;
        add_allocation(&mut owned, region.text.capacity(), 1)?;
    }
    add_allocation(
        &mut owned,
        result.receipt.image_deliveries.capacity(),
        size_of::<gptgrep_parse::OcrImageDelivery>(),
    )?;
    for delivery in &result.receipt.image_deliveries {
        for text in [
            &delivery.artifact_id,
            &delivery.artifact_sha256,
            &delivery.mime,
            &delivery.tool_response_sha256,
        ] {
            add_allocation(&mut owned, text.capacity(), 1)?;
        }
        if let Some(native) = &delivery.native {
            for text in [
                &native.schema_version,
                &native.admission_sha256,
                &native.task_sha256,
                &native.source_sha256,
                &native.prepared_sha256,
                &native.runtime_profile_sha256,
                &native.call_id_sha256,
                &native.source_receipt_sha256,
                &native.prepared_receipt_sha256,
                &native.assembled_receipt_sha256,
                &native.rendition_sha256,
                &native.prepared_image_sha256,
            ] {
                add_allocation(&mut owned, text.capacity(), 1)?;
            }
            let reference = match &native.reference {
                gptgrep_parse::OcrNativeImageReference::Inline { sha256, .. } => sha256,
                gptgrep_parse::OcrNativeImageReference::File { reference_sha256 } => {
                    reference_sha256
                }
            };
            add_allocation(&mut owned, reference.capacity(), 1)?;
            if let Some(crop) = &native.native_crop {
                owned = owned
                    .checked_add(crop.heap_working_bytes()?)
                    .context("parse_toolbox_working_overflow")?;
            }
        }
    }
    for text in [
        &result.schema_version,
        &result.task_id,
        &result.task_sha256,
        &result.source_sha256,
        &result.prepared_sha256,
        &result.receipt.model,
        &result.receipt.profile_sha256,
        &result.receipt.request_sha256,
        &result.receipt.response_sha256,
        &result.receipt.runtime_profile_sha256,
    ] {
        add_allocation(&mut owned, text.capacity(), 1)?;
    }
    if let Some(usage) = &result.receipt.usage {
        owned = owned
            .checked_add(value_working(usage, 0)?)
            .context("parse_toolbox_working_overflow")?;
    }
    add_allocation(&mut owned, 16, size_of::<(String, (OcrPageResult, u64))>())?;
    let encoded = bounded_count(result, serialized_cap)?;
    Ok(owned.max(
        encoded
            .checked_mul(16)
            .context("parse_toolbox_working_overflow")?,
    ))
}
fn add_allocation(total: &mut u64, count: usize, item_bytes: usize) -> Result<()> {
    *total = (count as u64)
        .checked_mul(item_bytes as u64)
        .and_then(|bytes| total.checked_add(bytes))
        .context("parse_toolbox_working_overflow")?;
    Ok(())
}
fn value_working(value: &serde_json::Value, depth: usize) -> Result<u64> {
    use serde_json::Value;
    ensure!(depth <= 64, "parse_toolbox_usage_depth_limit");
    let base = size_of::<Value>() as u64;
    match value {
        Value::String(text) => base.checked_add(text.capacity() as u64),
        Value::Array(items) => {
            let mut n = base;
            add_allocation(&mut n, items.capacity(), size_of::<Value>())?;
            for item in items {
                n = n
                    .checked_add(value_working(item, depth + 1)?)
                    .context("parse_toolbox_working_overflow")?;
            }
            return Ok(n);
        }
        Value::Object(items) => {
            let mut n = base;
            for (key, item) in items {
                add_allocation(&mut n, key.capacity(), 1)?;
                add_allocation(&mut n, 16, size_of::<String>() + size_of::<Value>())?;
                n = n
                    .checked_add(value_working(item, depth + 1)?)
                    .context("parse_toolbox_working_overflow")?;
            }
            return Ok(n);
        }
        _ => Some(base),
    }
    .context("parse_toolbox_working_overflow")
}

struct JsonWriter {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for JsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("parse_toolbox_reply_limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bounded(value: &(impl Serialize + ?Sized), cap: usize) -> Result<Vec<u8>> {
    let mut writer = JsonWriter {
        bytes: Vec::new(),
        cap,
    };
    writer
        .bytes
        .try_reserve_exact(cap)
        .context("parse_toolbox_reply_allocation")?;
    serde_json::to_writer(&mut writer, value).context("parse_toolbox_reply_limit")?;
    Ok(writer.bytes)
}
fn bounded_count(value: &(impl Serialize + ?Sized), cap: u64) -> Result<u64> {
    struct Counter {
        used: u64,
        cap: u64,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.used = self
                .used
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= self.cap)
                .ok_or_else(|| io::Error::other("parse_toolbox_serialized_limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Counter { used: 0, cap };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.used)
}
#[derive(Serialize)]
struct Envelope<'a, T: Serialize + ?Sized> {
    schema: &'static str,
    id: &'a str,
    ok: bool,
    data: &'a T,
}
fn encode(id: &str, data: &(impl Serialize + ?Sized), cap: usize) -> Result<Vec<u8>> {
    framed(
        &Envelope {
            schema: REPLY_SCHEMA,
            id,
            ok: true,
            data,
        },
        cap,
    )
}
fn encode_error(id: &str, code: &str, cap: usize) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Error<'a> {
        schema: &'static str,
        id: &'a str,
        ok: bool,
        error: Code<'a>,
    }
    #[derive(Serialize)]
    struct Code<'a> {
        code: &'a str,
    }
    framed(
        &Error {
            schema: REPLY_SCHEMA,
            id,
            ok: false,
            error: Code { code },
        },
        cap,
    )
}
fn framed(value: &(impl Serialize + ?Sized), cap: usize) -> Result<Vec<u8>> {
    let mut writer = JsonWriter {
        bytes: Vec::new(),
        cap: cap.checked_sub(1).context("parse_toolbox_reply_limit")?,
    };
    writer
        .bytes
        .try_reserve_exact(cap)
        .context("parse_toolbox_reply_allocation")?;
    serde_json::to_writer(&mut writer, value).context("parse_toolbox_reply_limit")?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}
fn data_url_len(mime: &str, bytes: usize) -> Result<usize> {
    ensure!(
        mime == "image/png" || mime == "image/jpeg",
        "parse_toolbox_image_mime"
    );
    bytes
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add(mime.len() + 13))
        .context("parse_toolbox_image_size")
}
fn admit_data_url_frame(
    id: &str,
    empty_payload: &impl Serialize,
    mime: &str,
    bytes: usize,
    cap: usize,
) -> Result<()> {
    let framing = bounded_count(
        &Envelope {
            schema: REPLY_SCHEMA,
            id,
            ok: true,
            data: empty_payload,
        },
        cap as u64,
    )?;
    // The fixed PNG/JPEG data URL alphabet has no JSON escape expansion.
    ensure!(
        framing
            .checked_add(data_url_len(mime, bytes)? as u64)
            .and_then(|n| n.checked_add(1))
            .is_some_and(|n| n <= cap as u64),
        "parse_toolbox_complete_image_reply_limit"
    );
    Ok(())
}
fn data_url(mime: &str, bytes: &[u8]) -> Result<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let size = data_url_len(mime, bytes.len())?;
    let mut result = String::new();
    result
        .try_reserve_exact(size)
        .context("parse_toolbox_image_allocation")?;
    result.push_str("data:");
    result.push_str(mime);
    result.push_str(";base64,");
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        result.push(ALPHABET[((bits >> 18) & 63) as usize] as char);
        result.push(ALPHABET[((bits >> 12) & 63) as usize] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    Ok(result)
}

#[derive(Serialize)]
struct SummaryData<'a> {
    handle: &'a str,
    stage: &'static str,
    summary: &'a PreparedSummary,
    native_profile_sha256: Option<&'a str>,
    original_pages: Option<usize>,
    artifact_count: usize,
    ocr_task_count: Option<usize>,
    ocr_blocker_count: Option<usize>,
    unmapped_merged_lines: Option<usize>,
    required_ocr_complete: Option<bool>,
}
#[derive(Serialize)]
struct RowsData<'a, T: Serialize> {
    kind: &'static str,
    total: usize,
    start: usize,
    rows: &'a [T],
    next_cursor: Option<&'a str>,
}
#[derive(Serialize)]
struct WindowData<'a> {
    kind: &'static str,
    artifact: &'a ArtifactRef,
    offset: usize,
    total_bytes: usize,
    text: &'a str,
    eof: bool,
    next_cursor: Option<&'a str>,
}
#[derive(Serialize)]
struct ImageData<'a> {
    kind: &'static str,
    artifact: &'a ArtifactRef,
    width: u32,
    height: u32,
    page_to_raster: gptgrep_parse::AffineTransform,
    data_url: &'a str,
}
#[derive(Serialize)]
struct CropData<'a> {
    kind: &'static str,
    source_sha256: &'a str,
    prepared_sha256: &'a str,
    sha256: &'a str,
    bytes: usize,
    data_url: &'a str,
    recipe: &'a gptgrep_parse::CropRecipe,
    recipe_sha256: &'a str,
    admission: &'a gptgrep_parse::CropAdmission,
}
#[derive(Serialize)]
struct ClosedData<'a> {
    handle: &'a str,
    closed: bool,
    shared_staging_used: u64,
    retained_artifacts: bool,
}

#[cfg(test)]
#[path = "parse_agent_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "parse_agent_native_crop_tests.rs"]
mod native_crop_tests;
