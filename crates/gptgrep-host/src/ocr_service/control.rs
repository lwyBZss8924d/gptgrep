//! Portable operator control snapshots. This module discovers no native account
//! configuration or credentials; it reads only explicitly selected control files.
use super::*;
use serde::Deserialize;
use std::fs::OpenOptions;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema_version: String,
    #[serde(default = "default_bind")]
    bind: SocketAddr,
    source_root: PathBuf,
    prepared_root: PathBuf,
    spool_root: PathBuf,
    claim_root: PathBuf,
    raw_results_dir: PathBuf,
    codex_home: PathBuf,
    codex_state_dir: Option<PathBuf>,
    #[serde(default = "default_binary")]
    codex_bin: String,
    runtime_profile: PathBuf,
    compatibility_profile: Option<PathBuf>,
    task_catalog: Option<PathBuf>,
    phase: PathBuf,
    #[serde(default)]
    resources: ResourceBudget,
    #[serde(default = "default_connections")]
    max_connections: usize,
}
fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8828))
}
fn default_binary() -> String {
    "codex".into()
}
fn default_connections() -> usize {
    8
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Phase {
    schema_version: String,
    run_id: String,
    plan_sha256: String,
    original_clock_anchor: RunClockAnchor,
    limits: crate::OcrExecutionLimits,
    ledger_path: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: String,
    tasks: Vec<CatalogTask>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogTask {
    source_relative_path: PathBuf,
    task: OcrTask,
}

/// An immutable, bounded, explicitly selected control snapshot. This is not
/// runtime qualification or permission to use any nominated private input.
pub struct ForegroundOcrControl {
    control: Control,
    base: PathBuf,
    digest: String,
}
impl ForegroundOcrControl {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = read_control(path, 16_384, "ocr_serve_control_unavailable")?;
        let value = gptgrep_jev::parse_unique_json(&bytes)
            .map_err(|_| anyhow!("ocr_serve_control_invalid"))?;
        let control: Control =
            serde_json::from_value(value).map_err(|_| anyhow!("ocr_serve_control_invalid"))?;
        ensure!(
            control.schema_version == "gptgrep.ocr.serve.v1",
            "ocr_serve_control_schema"
        );
        let options = ForegroundOcrOptions {
            bind: control.bind,
            source_root: control.source_root.clone(),
            spool_root: control.spool_root.clone(),
            claim_root: control.claim_root.clone(),
            resources: control.resources.clone(),
            max_connections: control.max_connections,
            compatibility: None,
            token_env_name: "GPTGREP_OCR_SERVE_TOKEN".into(),
        };
        options.validate()?;
        ensure!(
            control.task_catalog.is_some() || control.compatibility_profile.is_some(),
            "ocr_serve_no_routes"
        );
        ensure!(
            !control.codex_bin.is_empty() && control.codex_bin.len() <= 4096,
            "ocr_serve_binary_shape"
        );
        if let Some(path) = &control.codex_state_dir {
            path_shape(path)?;
        }
        for path in [
            &control.source_root,
            &control.prepared_root,
            &control.spool_root,
            &control.claim_root,
            &control.raw_results_dir,
            &control.codex_home,
            &control.runtime_profile,
            &control.phase,
        ] {
            path_shape(path)?;
        }
        for path in control
            .task_catalog
            .iter()
            .chain(control.compatibility_profile.iter())
        {
            path_shape(path)?;
        }
        let base = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|_| anyhow!("ocr_serve_control_parent_unavailable"))?;
        Ok(Self {
            control,
            base,
            digest: hash(&bytes),
        })
    }
    pub fn sha256(&self) -> &str {
        &self.digest
    }
    pub fn bind(&self) -> SocketAddr {
        self.control.bind
    }
    /// Attach a previously frozen original-anchor phase. Default join requires
    /// both existing regular journal and lock. Explicit initialization is create
    /// only; existing identities are validated and can never be reset or renewed.
    pub fn prepare(
        self,
        token: &[u8],
        token_env_name: &str,
        initialize_ledger: bool,
    ) -> Result<PreparedForegroundOcr> {
        validate_token_env_name(token_env_name)?;
        let control = self.control;
        let base = self.base;
        let working = WorkingBudget::new(control.resources.owned_working_bytes);
        // Profiles/control/phase decode and retained portable paths are owned too.
        let control_share = Arc::new(working.reserve(SERVICE_WORKING)?);
        let source = resolve(&base, &control.source_root)?;
        let prepared = resolve(&base, &control.prepared_root)?;
        let runtime: crate::OcrRuntimeProfile = read_json(
            &resolve(&base, &control.runtime_profile)?,
            16_384,
            "ocr_serve_runtime_profile_invalid",
        )?;
        runtime.validate()?;
        let compatibility = control
            .compatibility_profile
            .as_ref()
            .map(|path| {
                let profile: CompatibilityRuntimeProfile = read_json(
                    &resolve(&base, path)?,
                    16_384,
                    "ocr_serve_compatibility_profile_invalid",
                )?;
                profile.validate()?;
                ensure!(
                    profile.accepted_qualification_sha256.is_some()
                        && profile.runtime.sha256()? == runtime.sha256()?,
                    "ocr_serve_compatibility_qualification_required"
                );
                Ok(profile)
            })
            .transpose()?;
        if control.task_catalog.is_some() {
            ensure!(
                runtime.accepted_image_qualification_sha256.is_some(),
                "ocr_serve_full_task_qualification_required"
            );
        }
        let mut catalog = if let Some(path) = &control.task_catalog {
            let (bytes, catalog_share) = read_catalog(&resolve(&base, path)?, &working)?;
            let value = gptgrep_jev::parse_unique_json(&bytes)
                .map_err(|_| anyhow!("ocr_serve_catalog_invalid"))?;
            let wire: Catalog =
                serde_json::from_value(value).map_err(|_| anyhow!("ocr_serve_catalog_invalid"))?;
            ensure!(
                wire.schema_version == "gptgrep.ocr.task-catalog.v1" && wire.tasks.len() <= 1024,
                "ocr_serve_catalog_schema"
            );
            let mut tasks = Vec::new();
            tasks.try_reserve_exact(wire.tasks.len())?;
            for mut entry in wire.tasks {
                ensure!(
                    !entry.source_relative_path.as_os_str().is_empty()
                        && entry
                            .source_relative_path
                            .components()
                            .all(|part| matches!(part, std::path::Component::Normal(_))),
                    "ocr_serve_catalog_source_scope"
                );
                entry.task.source_path = source.join(entry.source_relative_path);
                for image in &mut entry.task.images {
                    image.path = prepared.join(&image.artifact.relative_path);
                }
                tasks.push(entry.task);
            }
            PreparedOcrTaskCatalog::prepare_admitted(
                &source,
                &prepared,
                tasks,
                &control.resources,
                working.clone(),
                catalog_share,
            )?
        } else {
            let share = Arc::new(working.reserve(65_536)?);
            PreparedOcrTaskCatalog::prepare_admitted(
                &source,
                &prepared,
                Vec::new(),
                &control.resources,
                working.clone(),
                share,
            )?
        };
        catalog._control = Some(control_share);
        let phase: Phase = read_json(
            &resolve(&base, &control.phase)?,
            16_384,
            "ocr_serve_phase_invalid",
        )?;
        ensure!(
            phase.schema_version == "gptgrep.ocr.service-phase.v1",
            "ocr_serve_phase_schema"
        );
        phase.limits.validate()?;
        let ledger = resolve(&base, &phase.ledger_path)?;
        if !initialize_ledger {
            regular(&ledger, "ocr_serve_existing_ledger_required")?;
            let mut lock = ledger.as_os_str().to_os_string();
            lock.push(".lock");
            regular(Path::new(&lock), "ocr_serve_existing_ledger_required")?;
        }
        let context_bound = phase
            .limits
            .ledger_bytes
            .checked_mul(40)
            .and_then(|n| n.checked_add(65_536))
            .ok_or(ServiceFailure::WorkingLimit)?;
        // Original-phase header/journal decode, current reservations and output
        // copies use the same shared allowance, BEFORE create/join reads them.
        catalog._context = Some(Arc::new(working.reserve(context_bound)?));
        let clock = phase.original_clock_anchor;
        let context = OcrExecutionContext::with_phase_limits(
            &phase.run_id,
            &phase.plan_sha256,
            clock.clone(),
            phase.limits,
        )?;
        let context = if initialize_ledger {
            context.join_original_ledger(&ledger, &clock)?
        } else {
            context.join_existing_original_ledger(&ledger, &clock)?
        };
        let config = OcrHostConfig {
            codex_bin: if Path::new(&control.codex_bin).components().count() > 1 {
                resolve(&base, Path::new(&control.codex_bin))?
                    .to_str()
                    .ok_or_else(|| anyhow!("ocr_serve_binary_shape"))?
                    .into()
            } else {
                control.codex_bin
            },
            codex_home: resolve(&base, &control.codex_home)?,
            codex_state_dir: control
                .codex_state_dir
                .as_ref()
                .map(|path| resolve(&base, path))
                .transpose()?,
            prepared_root: prepared,
            raw_results_dir: resolve(&base, &control.raw_results_dir)?,
            transport: runtime.transport(),
            runtime_profile: runtime,
        };
        prepare_foreground_ocr(
            ForegroundOcrOptions {
                bind: control.bind,
                source_root: source,
                spool_root: resolve(&base, &control.spool_root)?,
                claim_root: resolve(&base, &control.claim_root)?,
                resources: control.resources,
                max_connections: control.max_connections,
                compatibility,
                token_env_name: token_env_name.into(),
            },
            config,
            context,
            &clock,
            catalog,
            token,
        )
    }
}
pub fn validate_token_env_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
            && !matches!(name, "HOME" | "PATH" | "TMPDIR" | "CODEX_HOME"),
        "ocr_serve_token_env_name"
    );
    Ok(())
}
fn path_shape(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.as_os_str().len() <= 4096
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "ocr_serve_control_path_shape"
    );
    Ok(())
}
fn resolve(base: &Path, path: &Path) -> Result<PathBuf> {
    path_shape(path)?;
    Ok(if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    })
}
fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: usize,
    code: &'static str,
) -> Result<T> {
    let bytes = read_control(path, limit, code)?;
    let value = gptgrep_jev::parse_unique_json(&bytes).map_err(|_| anyhow!(code))?;
    serde_json::from_value(value).map_err(|_| anyhow!(code))
}
fn regular(path: &Path, code: &'static str) -> Result<()> {
    let file = open_control(path).map_err(|_| anyhow!(code))?;
    ensure!(file.metadata()?.is_file(), code);
    Ok(())
}
fn open_control(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    options.open(path)
}
fn read_catalog(path: &Path, working: &WorkingBudget) -> Result<(Vec<u8>, Arc<WorkingLease>)> {
    read_catalog_with(path, working, |file, size| {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size)?;
        file.take(size as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() == size, "ocr_serve_catalog_invalid");
        Ok(bytes)
    })
}
fn read_catalog_with(
    path: &Path,
    working: &WorkingBudget,
    read: impl FnOnce(File, usize) -> Result<Vec<u8>>,
) -> Result<(Vec<u8>, Arc<WorkingLease>)> {
    let file = open_control(path).map_err(|_| anyhow!("ocr_serve_catalog_invalid"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow!("ocr_serve_catalog_invalid"))?;
    ensure!(
        metadata.is_file() && metadata.len() <= CATALOG_BYTES as u64,
        "ocr_serve_catalog_invalid"
    );
    let bound = metadata
        .len()
        .checked_mul(CATALOG_FACTOR)
        .and_then(|n| n.checked_add(65_536))
        .ok_or(ServiceFailure::WorkingLimit)?;
    // The complete raw/unique-JSON/task/map/path upper is admitted before even
    // reserve/read. The same share stays with the catalog for service lifetime.
    let share = Arc::new(working.reserve(bound)?);
    let bytes = read(file, usize::try_from(metadata.len())?)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "ocr_serve_catalog_invalid"
    );
    Ok((bytes, share))
}
fn read_control(path: &Path, limit: usize, code: &'static str) -> Result<Vec<u8>> {
    let file = open_control(path).map_err(|_| anyhow!(code))?;
    let metadata = file.metadata().map_err(|_| anyhow!(code))?;
    ensure!(metadata.is_file() && metadata.len() <= limit as u64, code);
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(metadata.len() as usize)?;
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!(code))?;
    ensure!(bytes.len() as u64 == metadata.len(), code);
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_working_rejects_before_file_buffer_read_and_retains_admitted_share() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        // A real regular file exceeding selected working bytes, below file cap.
        let file = File::create(&path).unwrap();
        file.set_len(3 * 1_048_576).unwrap();
        let budget = WorkingBudget::new(2 * 1_048_576);
        let read_started = std::sync::atomic::AtomicBool::new(false);
        let result = read_catalog_with(&path, &budget, |_, _| {
            read_started.store(true, std::sync::atomic::Ordering::SeqCst);
            panic!("oversized catalog reached owned read")
        });
        assert!(result.is_err());
        assert!(!read_started.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(budget.used(), 0);
        let bytes = b"{\"schema_version\":\"gptgrep.ocr.task-catalog.v1\",\"tasks\":[]}";
        std::fs::write(&path, bytes).unwrap();
        let (actual, share) = read_catalog(&path, &budget).unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(budget.used(), bytes.len() as u64 * CATALOG_FACTOR + 65_536);
        drop(actual);
        assert!(budget.used() > 0);
        drop(share);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn control_snapshot_caps_unknown_fields_and_loopback_are_before_phase_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.json");
        let mut control = serde_json::json!({"schema_version":"gptgrep.ocr.serve.v1","source_root":"raw","prepared_root":"prepared",
            "spool_root":"spool","claim_root":"claims","raw_results_dir":"results","codex_home":"actor","runtime_profile":"profile.json",
            "compatibility_profile":"compat.json","phase":"phase.json"});
        std::fs::write(&path, serde_json::to_vec(&control).unwrap()).unwrap();
        let admitted = ForegroundOcrControl::load(&path).unwrap();
        assert_eq!(admitted.bind(), SocketAddr::from(([127, 0, 0, 1], 8828)));
        control["bind"] = "0.0.0.0:8828".into();
        std::fs::write(&path, serde_json::to_vec(&control).unwrap()).unwrap();
        assert!(ForegroundOcrControl::load(&path).is_err());
        control["bind"] = "127.0.0.1:8828".into();
        control["qualified"] = true.into();
        std::fs::write(&path, serde_json::to_vec(&control).unwrap()).unwrap();
        assert!(ForegroundOcrControl::load(&path).is_err());
        std::fs::write(&path, vec![b' '; 16385]).unwrap();
        assert!(ForegroundOcrControl::load(&path).is_err());
        assert!(validate_token_env_name("HOME").is_err());
        assert!(validate_token_env_name("token;code").is_err());
    }
}
