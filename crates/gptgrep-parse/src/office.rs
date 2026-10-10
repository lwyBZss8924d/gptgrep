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
    path::{Component, Path, PathBuf},
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
#[cfg(test)]
fn inventory_bytes(root: &Path, count: &mut usize) -> Result<u64> {
    inventory_bytes_with_font_cache(root, count, None)
}
fn is_font_cache_alias(path: &Path) -> bool {
    let Ok(target) = fs::read_link(path) else {
        return false;
    };
    let mut components = target.components();
    let Some(Component::Normal(name)) = components.next() else {
        return false;
    };
    // Comparing the original spelling also rejects normalized "./name" and
    // trailing separators. Inspect the sibling itself; never follow the alias.
    components.next().is_none()
        && target.as_os_str() == name
        && fs::symlink_metadata(path.with_file_name(name)).is_ok_and(|metadata| metadata.is_file())
}
#[derive(Clone, Copy)]
enum InventoryPhase<'a> {
    Final,
    Live { profile: &'a Path, tmp: &'a Path },
}
impl InventoryPhase<'_> {
    fn observe<T>(self, path: &Path, result: std::io::Result<T>) -> std::io::Result<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                let mutable_descendant = match self {
                    Self::Final => false,
                    Self::Live { profile, tmp } => [profile, tmp]
                        .into_iter()
                        .any(|root| path != root && path.starts_with(root)),
                };
                if mutable_descendant && error.kind() == std::io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }
}
#[cfg(test)]
fn inventory_bytes_with_font_cache(
    root: &Path,
    count: &mut usize,
    font_cache: Option<&Path>,
) -> Result<u64> {
    inventory_entries(
        root,
        fs::read_dir(root)?,
        count,
        font_cache,
        InventoryPhase::Final,
    )
}
fn inventory_entries(
    root: &Path,
    entries: impl Iterator<Item = std::io::Result<fs::DirEntry>>,
    count: &mut usize,
    font_cache: Option<&Path>,
    phase: InventoryPhase<'_>,
) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in entries {
        *count += 1;
        ensure!(
            *count <= 8192,
            "resource_limit: Office conversion file count"
        );
        // Iterator errors have no attributable child path and remain strict.
        let path = entry?.path();
        let Some(metadata) = phase.observe(&path, fs::symlink_metadata(&path))? else {
            continue;
        };
        bytes = bytes
            .checked_add(if metadata.file_type().is_symlink() {
                // Fontconfig creates version aliases inside the converter's
                // private cache. Account for their link bytes, without reading
                // their targets through the aliases or relaxing other paths.
                ensure!(
                    font_cache == Some(root) && is_font_cache_alias(&path),
                    "Office staging symlink rejected"
                );
                metadata.len()
            } else if metadata.is_dir() {
                // A converter may remove a scratch directory after metadata was
                // observed. Skip only that missing child, never a subtree error
                // that could discard bytes already counted from its siblings.
                if let Some(children) = phase.observe(&path, fs::read_dir(&path))? {
                    inventory_entries(&path, children, count, font_cache, phase)?
                } else {
                    0
                }
            } else {
                ensure!(metadata.is_file(), "Office staging special file rejected");
                metadata.len()
            })
            .context("Office staging length overflow")?;
    }
    Ok(bytes)
}
// This boundary must never return filesystem paths or arbitrary error chains.
// The locked anyhow version preserves these existing literal inventory errors
// as &'static str; unknown errors remain a closed, redacted cause.
fn inventory_failure(error: anyhow::Error, cap: u64, entries: usize) -> anyhow::Error {
    let cause = if let Some(error) = error.downcast_ref::<std::io::Error>() {
        match error.kind() {
            std::io::ErrorKind::NotFound => "missing",
            std::io::ErrorKind::PermissionDenied => "io_permission_denied",
            std::io::ErrorKind::Interrupted => "io_interrupted",
            std::io::ErrorKind::InvalidInput => "io_invalid_input",
            std::io::ErrorKind::InvalidData => "io_invalid_data",
            std::io::ErrorKind::WouldBlock => "io_would_block",
            std::io::ErrorKind::TimedOut => "io_timed_out",
            std::io::ErrorKind::NotADirectory => "io_not_a_directory",
            std::io::ErrorKind::IsADirectory => "io_is_a_directory",
            std::io::ErrorKind::OutOfMemory => "io_out_of_memory",
            _ => "io_other",
        }
    } else {
        match error.downcast_ref::<&'static str>().copied() {
            Some("resource_limit: Office conversion file count") => "entry_limit",
            Some("Office staging symlink rejected") => "symlink",
            Some("Office staging special file rejected") => "special",
            Some("Office staging length overflow") => "arithmetic",
            _ => "unknown",
        }
    };
    anyhow::anyhow!(
        "Office conversion staging inventory failed: cause={cause}, observed_bytes=unavailable, cap_bytes={cap}, entries_visited={entries}"
    )
}
#[cfg(test)]
fn check_staging(root: &Path, cap: u64) -> Result<()> {
    check_staging_with_font_cache(root, cap, None)
}
fn check_staging_with_font_cache(root: &Path, cap: u64, font_cache: Option<&Path>) -> Result<()> {
    check_staging_in_phase(root, cap, font_cache, InventoryPhase::Final)
}
fn check_staging_in_phase(
    root: &Path,
    cap: u64,
    font_cache: Option<&Path>,
    phase: InventoryPhase<'_>,
) -> Result<()> {
    let mut entries = 0;
    // The work root must exist even during a live sample. Only enumerated
    // descendants of the converter-owned mutable scratch may disappear.
    let bytes = fs::read_dir(root)
        .map_err(anyhow::Error::from)
        .and_then(|children| inventory_entries(root, children, &mut entries, font_cache, phase))
        .map_err(|error| inventory_failure(error, cap, entries))?;
    ensure!(
        bytes <= cap,
        "resource_limit: Office conversion staging exceeded: cause=logical_bytes, observed_bytes={bytes}, cap_bytes={cap}, entries_visited={entries}"
    );
    Ok(())
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
    let font_cache = profile.join(".cache/fontconfig");
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
                    if let Err(error) = check_staging_in_phase(
                        work, cap, Some(&font_cache), InventoryPhase::Live { profile: &profile, tmp: &tmp },
                    ) {
                        break Err(error);
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
    check_staging_with_font_cache(work, cap, Some(&font_cache))
        .context("Office final staging admission failed")?;
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
    fn live_inventory_skips_disappeared_scratch_entries_but_final_is_strict() -> Result<()> {
        for directory in [false, true] {
            for final_pass in [false, true] {
                let dir = tempfile::tempdir()?;
                let profile = dir.path().join("profile");
                let tmp = dir.path().join("tmp");
                fs::create_dir(&profile)?;
                fs::create_dir(&tmp)?;
                let vanished = profile.join("transient");
                if directory {
                    fs::create_dir(&vanished)?;
                } else {
                    fs::write(&vanished, b"temporary")?;
                }
                // Capture the real directory entry before removing it. This
                // exercises the live enumeration/metadata race deterministically.
                let entry = fs::read_dir(&profile)?.next().unwrap();
                if directory {
                    fs::remove_dir(&vanished)?;
                } else {
                    fs::remove_file(&vanished)?;
                }
                let phase = if final_pass {
                    InventoryPhase::Final
                } else {
                    InventoryPhase::Live {
                        profile: &profile,
                        tmp: &tmp,
                    }
                };
                let mut count = 0;
                let result =
                    inventory_entries(&profile, std::iter::once(entry), &mut count, None, phase);
                assert_eq!(count, 1);
                if final_pass {
                    let error = inventory_failure(result.unwrap_err(), 4, count);
                    assert!(error.to_string().contains("cause=missing,"));
                    assert!(!format!("{error:#?}").contains("transient"));
                } else {
                    assert_eq!(result?, 0);
                    // A directory can also disappear after its metadata lookup,
                    // before descent. Exercise that exact I/O result separately.
                    assert!(phase.observe(&vanished, fs::read_dir(&vanished))?.is_none());
                }
            }
        }
        Ok(())
    }
    #[test]
    fn live_inventory_keeps_root_input_output_and_non_missing_errors_strict() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let profile = dir.path().join("profile");
        let tmp = dir.path().join("tmp");
        let phase = InventoryPhase::Live {
            profile: &profile,
            tmp: &tmp,
        };
        for path in [
            &profile,
            &tmp,
            &dir.path().join("input.docx"),
            &dir.path().join("output/result.pdf"),
        ] {
            let error = phase.observe(path, fs::symlink_metadata(path)).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
        for phase in [phase, InventoryPhase::Final] {
            let error = check_staging_in_phase(&dir.path().join("missing-root"), 4, None, phase)
                .unwrap_err();
            assert!(error.to_string().contains("cause=missing,"));
            assert!(error.to_string().contains("entries_visited=0"));
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotADirectory,
        ] {
            let error = phase
                .observe::<()>(&profile.join("transient"), Err(std::io::Error::from(kind)))
                .unwrap_err();
            assert_eq!(error.kind(), kind);
        }
        let mut count = 0;
        let iterator_error =
            std::iter::once(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        let error =
            inventory_entries(&profile, iterator_error, &mut count, None, phase).unwrap_err();
        assert_eq!(count, 1);
        assert!(
            inventory_failure(error, 4, count)
                .to_string()
                .contains("cause=missing,")
        );
        Ok(())
    }
    #[test]
    fn live_inventory_retains_bytes_and_entry_limit_when_a_sibling_disappears() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let profile = dir.path().join("profile");
        let tmp = dir.path().join("tmp");
        fs::create_dir(&profile)?;
        fs::create_dir(&tmp)?;
        fs::write(profile.join("survivor"), b"data")?;
        let vanished = profile.join("transient");
        fs::write(&vanished, b"temporary")?;
        let mut entries = fs::read_dir(&profile)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        fs::remove_file(&vanished)?;
        let phase = InventoryPhase::Live {
            profile: &profile,
            tmp: &tmp,
        };
        let mut count = 0;
        let bytes = inventory_entries(
            &profile,
            entries.into_iter().map(Ok),
            &mut count,
            None,
            phase,
        )?;
        assert_eq!((bytes, count), (4, 2));
        let error = check_staging_in_phase(dir.path(), 3, None, phase).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cause=logical_bytes, observed_bytes=4, cap_bytes=3,")
        );
        fs::write(&vanished, b"temporary")?;
        let entry = fs::read_dir(&profile)?
            .find(|entry| entry.as_ref().is_ok_and(|entry| entry.path() == vanished))
            .unwrap();
        fs::remove_file(&vanished)?;
        let mut count = 8192;
        let error = inventory_entries(&profile, std::iter::once(entry), &mut count, None, phase)
            .unwrap_err();
        assert_eq!(count, 8193);
        assert!(
            inventory_failure(error, 4, count)
                .to_string()
                .contains("cause=entry_limit,")
        );
        Ok(())
    }
    #[test]
    fn staging_admission_reports_measured_bytes_and_keeps_exact_cap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("private-directory"))?;
        fs::write(
            dir.path().join("private-directory/private-document"),
            b"data",
        )?;
        check_staging(dir.path(), 4)?;
        let error = check_staging(dir.path(), 3).unwrap_err();
        assert_eq!(
            error.to_string(),
            "resource_limit: Office conversion staging exceeded: cause=logical_bytes, observed_bytes=4, cap_bytes=3, entries_visited=2"
        );
        assert!(!format!("{error:#?}").contains("private-"));
        Ok(())
    }
    #[test]
    fn staging_missing_and_symlink_failures_are_not_reported_as_byte_overruns() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = check_staging(&dir.path().join("private-missing"), 64).unwrap_err();
        assert_eq!(
            missing.to_string(),
            "Office conversion staging inventory failed: cause=missing, observed_bytes=unavailable, cap_bytes=64, entries_visited=0"
        );
        std::os::unix::fs::symlink("private-target", dir.path().join("private-link"))?;
        let link = check_staging(dir.path(), 64).unwrap_err();
        assert_eq!(
            link.to_string(),
            "Office conversion staging inventory failed: cause=symlink, observed_bytes=unavailable, cap_bytes=64, entries_visited=1"
        );
        for error in [missing, link] {
            assert!(!format!("{error:#?}").contains("private-"));
            assert!(!error.to_string().contains("staging exceeded"));
        }
        Ok(())
    }
    #[test]
    fn staging_entry_limit_keeps_the_rejected_entry_count() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("private-document"), b"data")?;
        let mut entries = 8192;
        let error = inventory_bytes(dir.path(), &mut entries).unwrap_err();
        assert_eq!(entries, 8193);
        assert_eq!(
            inventory_failure(error, 64, entries).to_string(),
            "Office conversion staging inventory failed: cause=entry_limit, observed_bytes=unavailable, cap_bytes=64, entries_visited=8193"
        );
        Ok(())
    }
    #[test]
    fn converter_font_cache_aliases_keep_byte_and_entry_accounting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("profile/.cache/fontconfig");
        fs::create_dir_all(&cache)?;
        fs::write(cache.join("current-cache"), b"cache")?;
        std::os::unix::fs::symlink("current-cache", cache.join("previous-cache"))?;
        let mut entries = 0;
        let bytes = inventory_bytes_with_font_cache(dir.path(), &mut entries, Some(&cache))?;
        assert_eq!((bytes, entries), (18, 5));
        check_staging_with_font_cache(dir.path(), 18, Some(&cache))?;
        assert_eq!(
            check_staging_with_font_cache(dir.path(), 17, Some(&cache))
                .unwrap_err()
                .to_string(),
            "resource_limit: Office conversion staging exceeded: cause=logical_bytes, observed_bytes=18, cap_bytes=17, entries_visited=5"
        );
        assert!(
            check_staging(dir.path(), 18)
                .unwrap_err()
                .to_string()
                .contains("cause=symlink,")
        );
        let mut entries = 8188;
        let error =
            inventory_bytes_with_font_cache(dir.path(), &mut entries, Some(&cache)).unwrap_err();
        assert_eq!(entries, 8193);
        assert!(
            inventory_failure(error, 18, entries)
                .to_string()
                .contains("cause=entry_limit,")
        );
        Ok(())
    }
    #[test]
    fn converter_font_cache_rejects_non_sibling_and_non_regular_aliases() -> Result<()> {
        for target in [
            "absolute-cache",
            "../current-cache",
            "nested/current-cache",
            "./current-cache",
            "current-cache/",
            "missing-cache",
            "directory-cache",
            "chained-cache",
            "previous-cache",
        ] {
            let dir = tempfile::tempdir()?;
            let cache = dir.path().join("profile/.cache/fontconfig");
            fs::create_dir_all(cache.join("nested"))?;
            fs::create_dir(cache.join("directory-cache"))?;
            fs::write(cache.join("current-cache"), b"cache")?;
            fs::write(cache.join("nested/current-cache"), b"cache")?;
            fs::write(cache.parent().unwrap().join("current-cache"), b"cache")?;
            std::os::unix::fs::symlink("current-cache", cache.join("chained-cache"))?;
            let target = if target == "absolute-cache" {
                cache.canonicalize()?.join("current-cache")
            } else {
                PathBuf::from(target)
            };
            std::os::unix::fs::symlink(&target, cache.join("previous-cache"))?;
            assert!(
                !is_font_cache_alias(&cache.join("previous-cache")),
                "accepted {target:?}"
            );
            let error = check_staging_with_font_cache(dir.path(), 4096, Some(&cache)).unwrap_err();
            assert!(
                error.to_string().contains("cause=symlink,"),
                "{target:?}: {error:#}"
            );
            assert!(!format!("{error:#?}").contains(dir.path().to_str().unwrap()));
            assert!(!error.to_string().contains("staging exceeded"));
        }
        Ok(())
    }
    #[test]
    fn converter_font_cache_allowance_does_not_extend_to_other_paths() -> Result<()> {
        for alias in [
            "input.docx",
            "output/result.pdf",
            "tmp/alias",
            "profile/alias",
            "profile/.cache/alias",
            "profile/.cache/fontconfig",
            "profile/.cache/fontconfig/nested/alias",
        ] {
            let dir = tempfile::tempdir()?;
            let cache = dir.path().join("profile/.cache/fontconfig");
            let alias = dir.path().join(alias);
            if alias != cache {
                fs::create_dir_all(&cache)?;
            }
            let parent = alias.parent().unwrap();
            fs::create_dir_all(parent)?;
            fs::write(parent.join("current-cache"), b"cache")?;
            std::os::unix::fs::symlink("current-cache", &alias)?;
            assert!(is_font_cache_alias(&alias));
            assert!(
                check_staging_with_font_cache(dir.path(), 4096, Some(&cache))
                    .unwrap_err()
                    .to_string()
                    .contains("cause=symlink,")
            );
        }
        Ok(())
    }
    #[test]
    fn staging_inventory_error_causes_are_closed_and_redact_context() {
        for (message, cause) in [
            ("Office staging special file rejected", "special"),
            ("Office staging length overflow", "arithmetic"),
            ("private-path-and-document-content", "unknown"),
        ] {
            let error = inventory_failure(anyhow::Error::msg(message), 64, 7);
            assert_eq!(
                error.to_string(),
                format!(
                    "Office conversion staging inventory failed: cause={cause}, observed_bytes=unavailable, cap_bytes=64, entries_visited=7"
                )
            );
            assert!(!format!("{error:#?}").contains("private-"));
        }
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private-path-and-document-content",
        ))
        .context("private-outer-context");
        let diagnostic = inventory_failure(error, 64, 7);
        assert_eq!(
            diagnostic.to_string(),
            "Office conversion staging inventory failed: cause=io_permission_denied, observed_bytes=unavailable, cap_bytes=64, entries_visited=7"
        );
        assert!(!format!("{diagnostic:#?}").contains("private-"));
        let opaque = inventory_failure(anyhow::anyhow!("private-{}", "dynamic-content"), 64, 7);
        assert!(opaque.to_string().contains("cause=unknown,"));
        assert!(!format!("{opaque:#?}").contains("private-"));
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
                "mkdir -p \"$HOME/.cache/fontconfig\"\nprintf cache > \"$HOME/.cache/fontconfig/current-cache\"\nln -s current-cache \"$HOME/.cache/fontconfig/previous-cache\"\nprintf x >> {}\n/bin/cp {} \"$8/result.pdf\"",
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
