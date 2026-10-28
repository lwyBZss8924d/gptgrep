//! Normal source-only rich ingestion. The caller injects the parser's OCR trait;
//! core never owns an account, model client, or native process.
use super::{GenerationPin, RichGenerationWriter, STATE, denied, digest, no_symlink, source_path};
use anyhow::{Context, Result, ensure};
use futures_util::future::join_all;
use gptgrep_parse::{
    CoverageState, NativeCandidate, NativeOptions, OcrExecutor, OcrMode, OcrPageResult, OcrProfile,
    OcrTask, OcrTaskBudget, PreparationOptions, ResourceBudget, extract_native, finalize_document,
    prepare_document, validate_ocr_result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime},
};

const JOURNAL_LIMIT: u64 = 2 * 1024 * 1024;
const PLAN_LIMIT: usize = 8 * 1024 * 1024;
pub const MAX_RICH_OCR_PAGES: usize = 24;

#[derive(Clone, Debug)]
pub struct RichIngestOptions {
    pub max_files: usize,
    pub mode: OcrMode,
    pub native: NativeOptions,
    pub resources: ResourceBudget,
    pub max_ocr_pages: usize,
    pub runtime_profile_sha256: Option<String>,
    pub run_id: Option<String>,
    pub resume: bool,
}
impl Default for RichIngestOptions {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            mode: OcrMode::Off,
            native: NativeOptions::default(),
            resources: ResourceBudget::default(),
            max_ocr_pages: MAX_RICH_OCR_PAGES,
            runtime_profile_sha256: None,
            run_id: None,
            resume: false,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct RichIngestPagePlan {
    pub page: u32,
    pub coverage: CoverageState,
    pub task_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RichIngestDocumentPlan {
    pub path: String,
    pub source_sha256: String,
    pub prepared_sha256: String,
    pub preparation_profile_sha256: String,
    pub native_profile_sha256: String,
    pub pages: Vec<RichIngestPagePlan>,
    pub blockers: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RichIngestReport {
    pub schema_version: &'static str,
    pub status: &'static str,
    pub mode: OcrMode,
    pub candidate: String,
    pub plan_sha256: String,
    pub generation: Option<GenerationPin>,
    pub documents: Vec<RichIngestDocumentPlan>,
    pub excluded_files: usize,
    pub required_pages: usize,
    pub unique_ocr_tasks: usize,
    pub new_ocr_executions: usize,
    pub reused_verified_results: usize,
    pub retained_staging_bytes: u64,
    pub blockers: Vec<String>,
    pub failure_code: Option<&'static str>,
    pub model_calls_in_plan: usize,
    pub requested_ocr_profile: OcrProfile,
    pub page_budget: OcrTaskBudget,
    pub max_concurrent_pages: usize,
    /// Executor admissions are observed locally; provider request count/cost are unavailable here.
    pub physical_provider_requests: Option<usize>,
    pub actual_cost_usd: Option<f64>,
}
struct Candidate {
    relative: String,
    staging: PathBuf,
    native: NativeCandidate,
    budget: ResourceBudget,
}
/// Holds the corpus writer lock and parser-owned candidates until explicit
/// publication. Dropping it retains candidate artifacts and never changes CURRENT.
pub struct PreparedRichIngest {
    root: PathBuf,
    staging_root: PathBuf,
    selected_sources: Vec<String>,
    run_lock: OnceLock<File>,
    writer: Option<RichGenerationWriter>,
    candidates: Vec<Candidate>,
    tasks: BTreeMap<String, OcrTask>,
    options: RichIngestOptions,
    report: RichIngestReport,
    journal: Option<Journal>,
}
impl PreparedRichIngest {
    pub fn report(&self) -> &RichIngestReport {
        &self.report
    }
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }
    /// Borrow the bounded raw-document-derived tasks for explicit executor custody.
    /// The caller can freeze task/path bindings before publication; no task is changed.
    pub fn ocr_tasks(&self) -> impl ExactSizeIterator<Item = &OcrTask> {
        self.tasks.values()
    }
    /// Exact task hash -> parser-owned root. The native host resolves image
    /// relative paths inside this document root, never the corpus ancestor.
    pub fn ocr_staging_roots(&self) -> BTreeMap<String, PathBuf> {
        let mut roots = BTreeMap::new();
        for candidate in &self.candidates {
            for task in &candidate.native.plan().tasks {
                roots
                    .entry(task.task_sha256.clone())
                    .or_insert_with(|| candidate.staging.clone());
            }
        }
        roots
    }
    /// Retained entries are still fully verified by publish. This only avoids
    /// constructing a live executor/clock when every task already has an entry;
    /// unknown, malformed or changed entries then fail locally without a retry.
    pub fn needs_ocr_executor(&self) -> bool {
        self.tasks.keys().any(|hash| {
            self.journal
                .as_ref()
                .is_none_or(|j| !j.results.contains_key(hash))
        })
    }
    pub fn state_dir(&self) -> Result<PathBuf> {
        let id = self
            .options
            .run_id
            .as_deref()
            .context("rich OCR requires an explicit run ID")?;
        ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "invalid rich OCR run ID"
        );
        Ok(self.root.join(STATE).join("ingest-runs").join(id))
    }
    fn lock_run(&self) -> Result<()> {
        if self.run_lock.get().is_some() {
            return Ok(());
        }
        use fs2::FileExt;
        let dir = self.state_dir()?;
        private_dir(&dir)?;
        let path = dir.join("ingest.lock");
        no_symlink(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = options.open(path)?;
        ensure!(
            lock.metadata()?.is_file(),
            "rich ingest lock must be regular"
        );
        lock.try_lock_exclusive()
            .context("another ingest owns this OCR run")?;
        self.run_lock
            .set(lock)
            .map_err(|_| anyhow::anyhow!("OCR run lock already set"))?;
        Ok(())
    }
    fn verify_source_inventory(&self) -> Result<()> {
        let (paths, _) = selected_sources(&self.root, self.options.max_files)?;
        ensure!(
            paths == self.selected_sources,
            "selected source inventory changed"
        );
        for document in &self.report.documents {
            let path = source_path(&self.root, &document.path)?;
            let bytes = read_regular(&path, super::MAX_SOURCE_BYTES)?;
            ensure!(
                digest(&bytes) == document.source_sha256,
                "source changed after native planning"
            );
        }
        Ok(())
    }
    /// Explicit live frontdoor only: capture once for a new run, or adopt the
    /// original durable SDK anchor on resume. Planning never calls this method.
    pub fn ocr_clock(&self, seconds: u64) -> Result<gptgrep_jev::RunClockAnchor> {
        ensure!(
            self.options.mode == OcrMode::Auto
                && !self.tasks.is_empty()
                && self.report.blockers.is_empty(),
            "no admitted live OCR plan"
        );
        ensure!(
            (1..=86400).contains(&seconds),
            "OCR deadline must be 1..=86400 seconds"
        );
        self.lock_run()?;
        let dir = self.state_dir()?;
        let path = dir.join("original-clock.json");
        if self.options.resume {
            let clock: gptgrep_jev::RunClockAnchor =
                serde_json::from_slice(&read_regular(&path, 16384)?)?;
            return Ok(clock);
        }
        ensure!(
            !path.exists(),
            "existing OCR original clock cannot be renewed; use resume"
        );
        let clock = gptgrep_jev::RunClockAnchor::capture(
            SystemTime::now()
                .checked_add(Duration::from_secs(seconds))
                .context("OCR deadline overflow")?,
        )?;
        write_new(&path, &serde_json::to_vec(&clock)?)?;
        Ok(clock)
    }
    /// Failures produce a retained unpublished report. An unknown/failed task is
    /// permanently charged in this run; resume never executes it a second time.
    pub async fn publish(mut self, executor: Option<&dyn OcrExecutor>) -> RichIngestReport {
        let result = self.complete(executor).await;
        if result.is_err() {
            self.report.status = "failed_unpublished";
            if self.report.failure_code.is_none() {
                self.report.failure_code = Some("rich_ingest_validation_or_publication_failed");
            }
        }
        if let Some(mut journal) = self.journal.take()
            && persist_journal(
                &self.state_dir().expect("validated run ID"),
                &mut journal,
                &self.options.resources,
            )
            .is_err()
        {
            self.report.failure_code = Some("rich_ingest_journal_closeout_failed");
            // Publication, if already durable, is reported truthfully; never retry it.
        }
        self.report.retained_staging_bytes = self.options.resources.staging_used();
        self.report
    }
    async fn complete(&mut self, executor: Option<&dyn OcrExecutor>) -> Result<()> {
        ensure!(
            self.report.blockers.is_empty(),
            "rich ingest plan has blockers"
        );
        self.verify_source_inventory()?;
        let mut results = BTreeMap::new();
        if self.options.mode == OcrMode::Auto && !self.tasks.is_empty() {
            let runtime = self
                .options
                .runtime_profile_sha256
                .as_deref()
                .context("rich OCR runtime profile required")?;
            ensure!(valid_digest(runtime), "invalid OCR runtime profile digest");
            self.lock_run()?;
            let dir = self.state_dir()?;
            private_dir(&dir.join("results"))?;
            self.journal.get_or_insert_with(|| Journal {
                schema_version: "gptgrep.rich-ingest-journal.v1".into(),
                plan_sha256: self.report.plan_sha256.clone(),
                runtime_profile_sha256: runtime.into(),
                retained_staging_bytes: 0,
                results: BTreeMap::new(),
            });
            let journal = self.journal.as_mut().expect("journal established");
            ensure!(
                journal.plan_sha256 == self.report.plan_sha256
                    && journal.runtime_profile_sha256 == runtime,
                "rich ingest resume identity mismatch"
            );
            for (hash, entry) in &journal.results {
                let task = self
                    .tasks
                    .get(hash)
                    .context("unplanned retained OCR task")?;
                ensure!(
                    entry.state == "validated",
                    "unknown or failed OCR task cannot be retried on resume"
                );
                let bytes = read_regular(
                    &dir.join("results").join(format!("{hash}.json")),
                    self.options.resources.ocr_result_bytes,
                )?;
                ensure!(
                    digest(&bytes) == entry.sha256,
                    "retained OCR result changed"
                );
                let result: OcrPageResult = serde_json::from_slice(&bytes)?;
                validate_result(task, &result, runtime)?;
                results.insert(hash.clone(), result);
                self.report.reused_verified_results += 1;
            }
            let pending = self
                .tasks
                .values()
                .filter(|t| !results.contains_key(&t.task_sha256))
                .cloned()
                .collect::<Vec<_>>();
            ensure!(
                pending.is_empty() || executor.is_some(),
                "OCR executor required for pending pages"
            );
            persist_journal(&dir, journal, &self.options.resources)?;
            for pair in pending.chunks(2) {
                // Persist every claim before calling the injected executor. Interrupted
                // or rejected calls stay completion_unknown, never a free new attempt.
                let journal = self.journal.as_mut().expect("journal established");
                for task in pair {
                    journal.results.insert(
                        task.task_sha256.clone(),
                        RetainedResult {
                            state: "completion_unknown".into(),
                            sha256: String::new(),
                        },
                    );
                }
                persist_journal(&dir, journal, &self.options.resources)?;
                self.report.new_ocr_executions += pair.len();
                let answers = join_all(
                    pair.iter()
                        .map(|task| executor.expect("pending executor").execute(task.clone())),
                )
                .await;
                let mut failed = false;
                for (task, answer) in pair.iter().zip(answers) {
                    match answer {
                        Ok(result) if validate_result(task, &result, runtime).is_ok() => {
                            let bytes = serde_json::to_vec(&result)?;
                            ensure!(
                                bytes.len() as u64 <= self.options.resources.ocr_result_bytes,
                                "OCR retained result exceeds budget"
                            );
                            self.options
                                .resources
                                .reserve_retained_staging(bytes.len() as u64)?;
                            write_new(
                                &dir.join("results")
                                    .join(format!("{}.json", task.task_sha256)),
                                &bytes,
                            )?;
                            self.journal
                                .as_mut()
                                .expect("journal established")
                                .results
                                .insert(
                                    task.task_sha256.clone(),
                                    RetainedResult {
                                        state: "validated".into(),
                                        sha256: digest(&bytes),
                                    },
                                );
                            results.insert(task.task_sha256.clone(), result);
                        }
                        _ => failed = true,
                    }
                }
                persist_journal(
                    &dir,
                    self.journal.as_mut().expect("journal established"),
                    &self.options.resources,
                )?;
                if failed {
                    self.report.failure_code = Some("rich_ocr_completion_unknown_or_invalid");
                    anyhow::bail!("required OCR page did not validate");
                }
            }
        }
        self.verify_source_inventory()?;
        for candidate in self.candidates.drain(..) {
            let outcomes = if self.options.mode == OcrMode::Auto {
                candidate
                    .native
                    .plan()
                    .tasks
                    .iter()
                    .map(|t| {
                        results
                            .get(&t.task_sha256)
                            .cloned()
                            .context("required OCR result absent")
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            let parsed = finalize_document(
                candidate.native,
                outcomes,
                self.options.mode,
                &candidate.budget,
            )?;
            self.writer
                .as_mut()
                .expect("unpublished writer")
                .add_document(&candidate.relative, &candidate.staging, parsed)?;
        }
        // Move the writer only after all documents have completed. There is one
        // publisher and one atomic pointer switch for the complete source inventory.
        self.report.generation = Some(
            self.writer
                .take()
                .context("unpublished writer")?
                .publish()?,
        );
        self.report.status = "published";
        Ok(())
    }
}

pub async fn prepare_rich_ingest(
    root: &Path,
    options: RichIngestOptions,
) -> Result<PreparedRichIngest> {
    options.resources.validate()?;
    ensure!(
        options.max_files > 0 && options.max_files <= 100_000,
        "rich max-files must be 1..=100000"
    );
    ensure!(
        (1..=MAX_RICH_OCR_PAGES).contains(&options.max_ocr_pages),
        "rich OCR page admission must be 1..=24"
    );
    let root = root.canonicalize()?;
    let writer = RichGenerationWriter::begin(&root, options.resources.clone())?;
    let staging_root = root.join(STATE).join("ingest-candidates").join(
        writer
            .candidate_dir()
            .file_name()
            .context("candidate identity")?,
    );
    private_dir(&staging_root)?;
    let mut prepared = PreparedRichIngest {
        report: RichIngestReport {
            schema_version: "gptgrep.rich-ingest.v1",
            status: "planned_unpublished",
            mode: options.mode,
            candidate: writer
                .candidate_dir()
                .file_name()
                .context("candidate identity")?
                .to_string_lossy()
                .into_owned(),
            plan_sha256: String::new(),
            generation: None,
            documents: Vec::new(),
            excluded_files: 0,
            required_pages: 0,
            unique_ocr_tasks: 0,
            new_ocr_executions: 0,
            reused_verified_results: 0,
            retained_staging_bytes: 0,
            blockers: Vec::new(),
            failure_code: None,
            model_calls_in_plan: 0,
            requested_ocr_profile: OcrProfile::default(),
            page_budget: OcrTaskBudget {
                resources: options.resources.clone(),
                ..Default::default()
            },
            max_concurrent_pages: 2,
            physical_provider_requests: None,
            actual_cost_usd: None,
        },
        root: root.clone(),
        staging_root,
        selected_sources: Vec::new(),
        run_lock: OnceLock::new(),
        writer: Some(writer),
        candidates: Vec::new(),
        tasks: BTreeMap::new(),
        options,
        journal: None,
    };
    if prepared.options.resume {
        prepared.lock_run()?;
        let dir = prepared.state_dir()?;
        let bytes = read_regular(&dir.join("ingest.json"), JOURNAL_LIMIT)?;
        let journal: Journal = serde_json::from_slice(&bytes)?;
        ensure!(
            journal.schema_version == "gptgrep.rich-ingest-journal.v1"
                && journal.results.len() <= MAX_RICH_OCR_PAGES,
            "invalid rich ingest journal"
        );
        prepared
            .options
            .resources
            .reserve_retained_staging(journal.retained_staging_bytes)?;
        prepared.journal = Some(journal);
    } else if prepared.options.run_id.is_some() {
        ensure!(
            !prepared.state_dir()?.join("ingest.json").exists(),
            "existing rich OCR run requires --resume"
        );
    }
    let (paths, excluded_files) = selected_sources(&root, prepared.options.max_files)?;
    prepared.report.excluded_files = excluded_files;
    prepared.selected_sources = paths.clone();
    ensure!(!paths.is_empty(), "no supported documents found");
    // Keep every native candidate alive for whole-plan admission. Conservatively
    // partition half the owned working budget across selected documents; leave
    // the other half for plan/result projections and the bounded two-page pool.
    let per_document = prepared.options.resources.owned_working_bytes / 2 / paths.len() as u64;
    ensure!(per_document > 0, "rich corpus working budget exhausted");
    for relative in paths {
        let path = source_path(&root, &relative)?;
        ensure!(
            fs::metadata(&path)?.len() <= super::MAX_SOURCE_BYTES,
            "source exceeds rich writer admission"
        );
        let mut budget = prepared.options.resources.clone();
        budget.owned_working_bytes = per_document;
        let source = prepare_document(
            &path,
            &prepared.staging_root().join("prepared"),
            &PreparationOptions,
            &budget,
        )
        .await?;
        let staging = source.staging_root().to_owned();
        let native = extract_native(&source, &prepared.options.native, &budget).await?;
        let plan = native.plan();
        prepared.report.required_pages += plan.tasks.len();
        prepared
            .report
            .blockers
            .extend(plan.blockers.iter().map(|b| format!("{relative}: {b}")));
        if prepared.options.mode == OcrMode::Off
            && plan
                .pages
                .iter()
                .any(|(_, c)| *c == CoverageState::RequiredOcr)
        {
            prepared.report.blockers.push(format!(
                "{relative}: required OCR disabled; complete publication blocked"
            ));
        }
        for task in &plan.tasks {
            prepared
                .tasks
                .entry(task.task_sha256.clone())
                .or_insert_with(|| task.clone());
        }
        prepared.report.documents.push(RichIngestDocumentPlan {
            path: relative.clone(),
            source_sha256: native.summary().source_sha256.clone(),
            prepared_sha256: native.summary().prepared_sha256.clone(),
            preparation_profile_sha256: native.summary().preparation_profile_sha256.clone(),
            native_profile_sha256: native.native_profile_sha256().into(),
            pages: plan
                .pages
                .iter()
                .map(|(page, coverage)| RichIngestPagePlan {
                    page: *page,
                    coverage: coverage.clone(),
                    task_sha256: plan
                        .tasks
                        .iter()
                        .find(|t| t.page == *page)
                        .map(|t| t.task_sha256.clone()),
                })
                .collect(),
            blockers: plan.blockers.clone(),
        });
        prepared.candidates.push(Candidate {
            relative,
            staging,
            native,
            budget,
        });
    }
    prepared.report.unique_ocr_tasks = prepared.tasks.len();
    if prepared.tasks.len() > prepared.options.max_ocr_pages {
        prepared
            .report
            .blockers
            .push("required OCR tasks exceed declared page admissions".into());
    }
    #[derive(Serialize)]
    struct PlanBinding<'a> {
        schema_version: &'static str,
        selected_sources: &'a [String],
        blockers: &'a [String],
        documents: &'a [RichIngestDocumentPlan],
        mode: OcrMode,
        max_ocr_pages: usize,
        runtime_profile_sha256: &'a Option<String>,
        budget: &'a ResourceBudget,
        page_window: usize,
        dpi: f32,
        render_pages: bool,
    }
    let binding = bounded_json(
        &PlanBinding {
            schema_version: "gptgrep.rich-ingest-plan.v1",
            selected_sources: &prepared.selected_sources,
            blockers: &prepared.report.blockers,
            documents: &prepared.report.documents,
            mode: prepared.options.mode,
            max_ocr_pages: prepared.options.max_ocr_pages,
            runtime_profile_sha256: &prepared.options.runtime_profile_sha256,
            budget: &prepared.options.resources,
            page_window: prepared.options.native.page_window,
            dpi: prepared.options.native.dpi,
            render_pages: prepared.options.native.render_pages,
        },
        PLAN_LIMIT,
    )?;
    prepared.report.plan_sha256 = digest(&binding);
    if let Some(journal) = &prepared.journal {
        ensure!(
            journal.plan_sha256 == prepared.report.plan_sha256
                && Some(&journal.runtime_profile_sha256)
                    == prepared.options.runtime_profile_sha256.as_ref(),
            "rich ingest resume source/profile/schema/plan mismatch"
        );
    }
    if !prepared.report.blockers.is_empty() {
        prepared.report.status = "blocked_unpublished";
    }
    prepared.report.retained_staging_bytes = prepared.options.resources.staging_used();
    Ok(prepared)
}

pub(crate) fn selected_sources(root: &Path, max_files: usize) -> Result<(Vec<String>, usize)> {
    let walk_root = root.to_owned();
    let mut paths = Vec::new();
    let mut excluded = 0;
    for item in ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .require_git(false)
        .filter_entry(move |e| !denied(e.path().strip_prefix(&walk_root).unwrap_or(e.path())))
        .build()
    {
        let entry = item?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            if gptgrep_parse::supports_path(entry.path()) {
                ensure!(paths.len() < max_files, "rich corpus exceeds max-files");
                paths.push(
                    entry
                        .path()
                        .strip_prefix(root)?
                        .to_str()
                        .context("non-UTF8 source path")?
                        .to_owned(),
                );
            } else {
                excluded += 1;
            }
        }
    }
    paths.sort();
    Ok((paths, excluded))
}
fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "rich ingest serialized metadata limit",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: String,
    plan_sha256: String,
    runtime_profile_sha256: String,
    retained_staging_bytes: u64,
    results: BTreeMap<String, RetainedResult>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedResult {
    state: String,
    sha256: String,
}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_result(task: &OcrTask, result: &OcrPageResult, runtime: &str) -> Result<()> {
    validate_ocr_result(task, result)?;
    ensure!(
        result.receipt.runtime_profile_sha256 == runtime,
        "OCR runtime profile mismatch"
    );
    Ok(())
}
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::{
            os::fd::{AsRawFd, FromRawFd},
            path::Component,
        };
        ensure!(path.is_absolute(), "ingest state path must be absolute");
        let mut parent = File::open("/")?;
        let parts = path.components().collect::<Vec<_>>();
        ensure!(
            parts.len() > 1 && parts.len() <= 64,
            "ingest state path limit"
        );
        for (i, part) in parts.iter().enumerate().skip(1) {
            let Component::Normal(name) = part else {
                anyhow::bail!("ingest state path shape")
            };
            let name = std::ffi::CString::new(name.as_encoded_bytes())?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if i + 1 < parts.len() {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            // SAFETY: live held parent and NUL-terminated single name; adopt the new fd once.
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            ensure!(fd >= 0, "ingest state regular nofollow input required");
            parent = unsafe { File::from_raw_fd(fd) };
        }
        ensure!(
            parent.metadata()?.is_file() && parent.metadata()?.len() <= limit,
            "ingest state input limit"
        );
        let mut bytes = Vec::new();
        parent.take(limit + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= limit, "ingest state input grew");
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, limit);
        anyhow::bail!("capability_unavailable: rich ingest state needs nofollow filesystem custody")
    }
}
/// Bounded nofollow loader for the CLI's noncredential runtime descriptor.
/// The host owns descriptor decoding and qualification; this only pins input bytes.
pub fn read_rich_runtime_descriptor(path: &Path) -> Result<Vec<u8>> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    ensure!(
        name != "auth.json"
            && !name.starts_with(".env")
            && !name.ends_with(".pem")
            && !name.ends_with(".key"),
        "runtime descriptor cannot be a credential input"
    );
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    read_regular(&absolute, 16384)
}
#[cfg(test)]
mod tests;
fn private_dir(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        no_symlink(parent)?;
    }
    fs::create_dir_all(path)?;
    ensure!(
        path.canonicalize()? == path,
        "ingest state parent must be canonical"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    no_symlink(path)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(path.parent().context("result parent")?)?.sync_all()?;
    Ok(())
}
fn persist_journal(dir: &Path, journal: &mut Journal, budget: &ResourceBudget) -> Result<()> {
    budget.reserve_retained_staging(JOURNAL_LIMIT)?;
    journal.retained_staging_bytes = budget.staging_used();
    let bytes = serde_json::to_vec(journal)?;
    ensure!(
        bytes.len() as u64 <= JOURNAL_LIMIT,
        "rich ingest journal limit"
    );
    no_symlink(&dir.join("ingest.json"))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join("ingest.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
