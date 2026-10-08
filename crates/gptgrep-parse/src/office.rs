//! Convert an immutable raw snapshot once; all later native/OCR work uses the
//! retained PDF. Child diagnostics are drained into counters, never returned.
//! Polling deadline/disk limits are observations, not hard OS/RSS enforcement.
use crate::{
    ResourceBudget,
    artifact::{hash_file, sha256},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

pub const OFFICE_ADAPTER: &str = "libreoffice-private-profile-retained-pdf-v1";
const DIAGNOSTIC_LIMIT: u64 = 65_536;
const TIMEOUT: Duration = Duration::from_secs(120);
pub fn is_office_extension(ext: &str) -> bool {
    matches!(
        ext,
        "doc"
            | "docx"
            | "docm"
            | "dot"
            | "dotm"
            | "dotx"
            | "odt"
            | "ott"
            | "rtf"
            | "pages"
            | "ppt"
            | "pptx"
            | "pptm"
            | "pot"
            | "potm"
            | "potx"
            | "odp"
            | "otp"
            | "key"
            | "xls"
            | "xlsx"
            | "xlsm"
            | "xlsb"
            | "ods"
            | "ots"
            | "numbers"
    )
}
pub fn office_preparation_profile_digest(extension: &str) -> Result<String> {
    ensure!(
        is_office_extension(extension),
        "unsupported Office extension"
    );
    Ok(sha256(&serde_json::to_vec(&(
        "gptgrep.prepare.office-normalized-pdf.v1",
        crate::LITEPARSE_REVISION,
        OFFICE_ADAPTER,
        extension,
        "once; immutable-input; private-profile; macros-disabled; normalized-PDF-coordinates; original-page-cell-mapping-unavailable",
    ))?))
}
fn installed_converter() -> Result<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            // Never resolve relative PATH components against document staging.
            if directory.is_absolute() {
                for name in ["libreoffice", "soffice"] {
                    paths.push(directory.join(name));
                }
            }
        }
    }
    paths.push(PathBuf::from(
        "/Applications/LibreOffice.app/Contents/MacOS/soffice",
    ));
    paths.push(PathBuf::from(
        r"C:\Program Files\LibreOffice\program\soffice.exe",
    ));
    for path in paths {
        if path.is_file() {
            return Ok(path.canonicalize()?);
        }
    }
    anyhow::bail!(
        "capability_unavailable: installed LibreOffice converter required for Office rich ingestion"
    )
}
fn profile_url(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .context("Office profile path must be UTF-8")?
        .replace('\\', "/");
    let mut url = if path.starts_with('/') {
        "file://".to_owned()
    } else {
        "file:///".to_owned()
    };
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            url.push(byte as char)
        } else {
            url.push_str(&format!("%{byte:02X}"))
        }
    }
    Ok(url)
}
fn inventory_bytes(root: &Path, count: &mut usize) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in fs::read_dir(root)? {
        *count += 1;
        ensure!(
            *count <= 8192,
            "resource_limit: Office conversion file count"
        );
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Office staging symlink rejected"
        );
        bytes = bytes
            .checked_add(if metadata.is_dir() {
                inventory_bytes(&path, count)?
            } else {
                ensure!(metadata.is_file(), "Office staging special file rejected");
                metadata.len()
            })
            .context("Office staging length overflow")?;
    }
    Ok(bytes)
}
async fn drain(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Result<()> {
    let mut buffer = [0u8; 4096];
    let mut count = 0u64;
    loop {
        let n = pipe.read(&mut buffer).await?;
        if n == 0 {
            return Ok(());
        }
        count += n as u64;
        ensure!(
            count <= DIAGNOSTIC_LIMIT,
            "resource_limit: Office diagnostic byte limit"
        );
    }
}
async fn convert(
    converter: &Path,
    input: &Path,
    work: &Path,
    cap: u64,
    timeout: Duration,
) -> Result<PathBuf> {
    let profile = work.join("profile");
    let output = work.join("output");
    let tmp = work.join("tmp");
    for path in [&profile, &output, &tmp] {
        fs::create_dir(path)?;
    }
    // Private profile; do not load/write the user's existing Office state.
    fs::create_dir(profile.join("user"))?;
    fs::write(profile.join("user/registrymodifications.xcu"), b"<?xml version=\"1.0\"?><oor:items xmlns:oor=\"http://openoffice.org/2001/registry\"><item oor:path=\"/org.openoffice.Office.Common/Security/Scripting\"><prop oor:name=\"MacroSecurityLevel\" oor:op=\"fuse\"><value>3</value></prop></item></oor:items>")?;
    let mut child = Command::new(converter)
        .args([
            format!("-env:UserInstallation={}", profile_url(&profile)?),
            "--headless".into(),
            "--invisible".into(),
            "--norestore".into(),
            "--convert-to".into(),
            "pdf".into(),
            "--outdir".into(),
            output.to_string_lossy().into_owned(),
            input.to_string_lossy().into_owned(),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &profile)
        .env("USERPROFILE", &profile)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("capability_unavailable: Office converter could not start")?;
    let stdout = child.stdout.take().context("Office stdout pipe missing")?;
    let stderr = child.stderr.take().context("Office stderr pipe missing")?;
    let deadline = tokio::time::Instant::now() + timeout;
    let result = {
        let pipes = async {
            tokio::try_join!(drain(stdout), drain(stderr))?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(pipes);
        let mut pipes_done = false;
        let mut status = None;
        loop {
            tokio::select! {
                result = &mut pipes, if !pipes_done => {
                    if let Err(error) = result {break Err(error)}
                    pipes_done = true;
                }
                _ = tokio::time::sleep_until(deadline) => {break Err(anyhow::anyhow!("Office conversion deadline exceeded"));}
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    match inventory_bytes(work, &mut 0) {
                        Ok(bytes) if bytes <= cap => {},
                        _ => break Err(anyhow::anyhow!("resource_limit: Office conversion staging exceeded")),
                    }
                    match child.try_wait() {
                        Ok(Some(exit)) => status = Some(exit),
                        Ok(None) => {},
                        Err(error) => break Err(error.into()),
                    }
                    if tokio::time::Instant::now() >= deadline { break Err(anyhow::anyhow!("Office conversion deadline exceeded")); }
                    if pipes_done && let Some(exit) = status {
                        break if exit.success() {Ok(())} else {Err(anyhow::anyhow!("Office conversion failed"))};
                    }
                }
            }
        }
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result?;
    ensure!(
        inventory_bytes(work, &mut 0)? <= cap,
        "resource_limit: Office final staging admission"
    );
    let files = fs::read_dir(&output)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        files.len() == 1,
        "Office conversion must produce exactly one PDF"
    );
    let pdf = files[0].path();
    ensure!(
        pdf.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
            && fs::symlink_metadata(&pdf)?.is_file(),
        "Office conversion output is not a regular PDF"
    );
    let mut header = [0u8; 5];
    fs::File::open(&pdf)?.read_exact(&mut header)?;
    ensure!(
        &header == b"%PDF-",
        "Office conversion output has no PDF signature"
    );
    ensure!(
        tokio::time::Instant::now() < deadline,
        "Office conversion deadline exceeded"
    );
    Ok(pdf)
}
pub(crate) fn prepare_once(
    source: &Path,
    staging: &Path,
    prepared: &Path,
    extension: &str,
    identity: (u64, &str),
    budget: &ResourceBudget,
) -> Result<(u64, String)> {
    let converter = installed_converter()?;
    prepare_with_converter(
        &converter, source, staging, prepared, extension, identity, budget,
    )
}
fn prepare_with_converter(
    converter: &Path,
    source: &Path,
    staging: &Path,
    prepared: &Path,
    extension: &str,
    identity: (u64, &str),
    budget: &ResourceBudget,
) -> Result<(u64, String)> {
    let (bytes, digest) = identity;
    let cap = budget
        .document_staging_bytes
        .checked_sub(bytes)
        .context("Office staging budget exhausted")?
        .min(budget.owned_working_bytes / 4);
    ensure!(cap > 0, "resource_limit: Office conversion admission");
    budget.reserve_staging(cap)?;
    let work = staging.join("office-once");
    fs::create_dir(&work)?;
    let input = work.join(format!("input.{extension}"));
    let mut output = crate::resource::LimitedWriter {
        inner: fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&input)?,
        limit: bytes,
        written: 0,
    };
    std::io::copy(&mut fs::File::open(source)?, &mut output)?;
    output.inner.sync_all()?;
    ensure!(
        hash_file(&input, bytes)? == (bytes, digest.into()),
        "source changed while snapshotting Office input"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let pdf = runtime.block_on(convert(converter, &input, &work, cap + bytes, TIMEOUT))?;
    ensure!(
        hash_file(&input, bytes)? == (bytes, digest.into()),
        "immutable Office input changed during conversion"
    );
    ensure!(
        hash_file(source, bytes)? == (bytes, digest.into()),
        "source changed during Office conversion"
    );
    let identity = hash_file(&pdf, cap)?;
    ensure!(!prepared.exists(), "Office prepared output already exists");
    fs::rename(&pdf, prepared)?;
    fs::File::open(prepared)?.sync_all()?;
    // Successful conversion scratch is removed. Its conservative shared staging
    // reservation remains charged; failed scratch remains for inspection.
    fs::remove_dir_all(work)?;
    Ok(identity)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{ConversionIdentity, FeatureState, NativeOptions, OcrMode, PreparedSummary};
    use std::os::unix::fs::PermissionsExt;
    fn script(dir: &Path, body: &str) -> Result<PathBuf> {
        let path = dir.join("mock-converter");
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(path)
    }
    fn quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
    }
    #[test]
    fn one_conversion_is_retained_for_extract_render_finalize_and_raw_identity() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let source = root.join("raw.docx");
        fs::write(
            &source,
            b"Synthetic opaque raw fixture, not a genuine Office conversion",
        )?;
        let raw = hash_file(&source, 4096)?;
        let fixture = root.join("fixture.pdf");
        fs::write(&fixture, crate::tests::pdf_fixture(true))?;
        let calls = root.join("calls");
        let converter = script(
            &root,
            &format!(
                "printf x >> {}\n/bin/cp {} \"$8/result.pdf\"",
                quote(&calls),
                quote(&fixture)
            ),
        )?;
        let stage = root.join("stage");
        fs::create_dir(&stage)?;
        let prepared_path = stage.join("prepared.pdf");
        let budget = ResourceBudget::default();
        budget.reserve_staging(raw.0)?;
        let output = prepare_with_converter(
            &converter,
            &source,
            &stage,
            &prepared_path,
            "docx",
            (raw.0, &raw.1),
            &budget,
        )?;
        let prepared = crate::PreparedDocument {
            summary: PreparedSummary {
                source_sha256: raw.1,
                source_bytes: raw.0,
                prepared_sha256: output.1,
                prepared_bytes: output.0,
                format: "docx".into(),
                parser_identity: crate::RICH_PARSER.into(),
                preparation_profile_sha256: office_preparation_profile_digest("docx")?,
                conversion: FeatureState::Present(ConversionIdentity {
                    converter: "gptgrep-bounded-libreoffice-pdf".into(),
                    version: OFFICE_ADAPTER.into(),
                    invocations: 1,
                }),
                image_geometry: FeatureState::Absent,
            },
            source_path: source,
            prepared_path,
            staging_root: stage,
            invocation_budget: budget.clone(),
        };
        let candidate =
            crate::rich::extract_sync(prepared.clone(), NativeOptions::default(), budget.clone())?;
        assert!(candidate.pages().iter().all(|p| !p.renditions.is_empty()));
        let complete = crate::finalize_document(candidate, Vec::new(), OcrMode::Off, &budget)?;
        prepared.verify(&budget)?;
        assert_eq!(complete.summary.source_bytes, raw.0);
        assert!(complete.pages.iter().all(|p| matches!(
            p.capabilities.get("original_office_page_cell_mapping"),
            Some(FeatureState::Unsupported(_))
        )));
        assert_eq!(fs::read(calls)?, b"x");
        Ok(())
    }
    #[test]
    fn conversion_source_change_and_excess_diagnostics_fail_without_prepared_output() -> Result<()>
    {
        for mutate in [true, false] {
            let dir = tempfile::tempdir()?;
            let root = dir.path().canonicalize()?;
            let source = root.join("raw.docx");
            fs::write(&source, b"raw snapshot")?;
            let raw = hash_file(&source, 4096)?;
            let body = if mutate {
                format!(
                    "printf changed > {}\nprintf '%%PDF-1.4\n' > \"$8/result.pdf\"",
                    quote(&source)
                )
            } else {
                "i=0; while [ \"$i\" -lt 9000 ]; do printf sensitive-output; i=$((i+1)); done"
                    .into()
            };
            let converter = script(&root, &body)?;
            let stage = root.join("stage");
            fs::create_dir(&stage)?;
            let prepared = stage.join("prepared.pdf");
            let error = prepare_with_converter(
                &converter,
                &source,
                &stage,
                &prepared,
                "docx",
                (raw.0, &raw.1),
                &ResourceBudget::default(),
            )
            .unwrap_err();
            assert!(!prepared.exists());
            assert!(!format!("{error:#}").contains("sensitive-output"));
            assert!(
                error.to_string().contains(if mutate {
                    "source changed"
                } else {
                    "diagnostic byte limit"
                }),
                "{error:#}"
            );
        }
        Ok(())
    }
    #[test]
    fn child_deadline_rejects_without_any_pdf_return() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let source = root.join("raw.docx");
        fs::write(&source, b"raw")?;
        let converter = script(&root, "exec /bin/sleep 1")?;
        let work = root.join("work");
        fs::create_dir(&work)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let error = runtime
            .block_on(convert(
                &converter,
                &source,
                &work,
                4096,
                Duration::from_millis(30),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"));
        Ok(())
    }
    #[test]
    fn missing_converter_and_prelaunch_staging_admission_are_explicit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let source = root.join("raw.docx");
        fs::write(&source, b"raw")?;
        let raw = hash_file(&source, 4096)?;
        let stage = root.join("stage");
        fs::create_dir(&stage)?;
        let missing = root.join("missing-converter");
        let error = prepare_with_converter(
            &missing,
            &source,
            &stage,
            &stage.join("prepared.pdf"),
            "docx",
            (raw.0, &raw.1),
            &ResourceBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("capability_unavailable"));
        let mut budget = ResourceBudget::default();
        budget.run_staging_bytes = 1;
        assert!(
            prepare_with_converter(
                &missing,
                &source,
                &stage,
                &stage.join("prepared.pdf"),
                "docx",
                (raw.0, &raw.1),
                &budget
            )
            .unwrap_err()
            .to_string()
            .contains("shared run staging")
        );
        Ok(())
    }
}
