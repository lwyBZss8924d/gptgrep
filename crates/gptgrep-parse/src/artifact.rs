use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::resource::{LimitedWriter, encoded_size};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    NativeText,
    NativeMetadata,
    OcrText,
    OcrRegions,
    MergedText,
    Layout,
    Rendition,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub id: String,
    pub kind: ArtifactKind,
    pub relative_path: PathBuf,
    pub sha256: String,
    pub profile_sha256: String,
    pub bytes: u64,
    pub mime: String,
    pub page: Option<u32>,
    pub region_id: Option<String>,
}

/// Stable identity for one source-bound extraction artifact. Transport paths
/// and batching do not participate; kind, page, region and recipe do.
pub fn artifact_id(
    source_sha256: &str,
    profile_sha256: &str,
    kind: &ArtifactKind,
    page: Option<u32>,
    region_id: Option<&str>,
    content_sha256: &str,
) -> Result<String> {
    for digest in [source_sha256, profile_sha256, content_sha256] {
        ensure!(valid_digest(digest), "invalid artifact identity digest");
    }
    ensure!(
        region_id.is_none_or(|id| id.len() <= 1024),
        "artifact region ID too long"
    );
    Ok(format!(
        "artifact-{}",
        sha256(&serde_json::to_vec(&(
            "gptgrep.artifact.v1",
            source_sha256,
            profile_sha256,
            kind,
            page,
            region_id,
            content_sha256,
        ))?)
    ))
}

pub(crate) fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) struct ArtifactBinding<'a> {
    pub source: &'a str,
    pub profile: &'a str,
    pub kind: ArtifactKind,
    pub page: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum FeatureState<T> {
    Present(T),
    #[default]
    Absent,
    Unsupported(String),
    Failed(String),
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}

pub(crate) fn json_sha256(value: &impl Serialize, limit: u64) -> Result<String> {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = LimitedWriter {
        inner: HashWriter(Sha256::new()),
        limit,
        written: 0,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer
        .inner
        .0
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn hash_file(path: &Path, limit: u64) -> Result<(u64, String)> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "source/artifact must be a nonsymlink regular file"
    );
    ensure!(
        meta.len() <= limit,
        "resource_limit: source/artifact file length"
    );
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .context("resource_limit: source length overflow")?;
        ensure!(
            bytes <= limit,
            "resource_limit: source changed beyond admission"
        );
        hasher.update(&buffer[..read]);
    }
    Ok((
        bytes,
        hasher
            .finalize()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect(),
    ))
}

/// Retain at most the admitted bytes, even if the file grows after metadata.
pub(crate) fn read_bounded(path: &Path, expected: u64, digest: &str) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() == expected,
        "artifact length/type changed before bounded read"
    );
    let mut file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() == expected,
        "artifact length changed while opening"
    );
    let mut bytes = vec![0; usize::try_from(expected).context("artifact bytes not addressable")?];
    file.read_exact(&mut bytes)?;
    let mut extra = [0u8; 1];
    ensure!(
        file.read(&mut extra)? == 0 && sha256(&bytes) == digest,
        "artifact changed during bounded read"
    );
    Ok(bytes)
}

pub(crate) fn validate_relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "artifact path must be a normalized relative path"
    );
    ensure!(
        path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "artifact path must be normalized"
    );
    Ok(())
}

pub(crate) fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "artifact root became a symlink"
    );
    validate_relative(relative)?;
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "artifact path contains a symlink"
        );
    }
    Ok(path)
}

pub(crate) fn write_bytes(
    root: &Path,
    name: &str,
    binding: ArtifactBinding<'_>,
    bytes: &[u8],
    mime: &str,
    limit: u64,
) -> Result<ArtifactRef> {
    ensure!(
        bytes.len() as u64 <= limit,
        "resource_limit: artifact admission"
    );
    let relative = PathBuf::from(name);
    validate_relative(&relative)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join(&relative))?;
    output.write_all(bytes)?;
    output.sync_all()?;
    let digest = sha256(bytes);
    Ok(ArtifactRef {
        id: artifact_id(
            binding.source,
            binding.profile,
            &binding.kind,
            binding.page,
            None,
            &digest,
        )?,
        kind: binding.kind,
        relative_path: relative,
        sha256: digest,
        profile_sha256: binding.profile.into(),
        bytes: bytes.len() as u64,
        mime: mime.into(),
        page: binding.page,
        region_id: None,
    })
}

pub(crate) fn write_json(
    root: &Path,
    name: &str,
    binding: ArtifactBinding<'_>,
    value: &impl Serialize,
    limit: u64,
) -> Result<ArtifactRef> {
    let size = encoded_size(value, limit)?;
    validate_relative(Path::new(name))?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join(name))?;
    let mut output = LimitedWriter {
        inner: file,
        limit: size,
        written: 0,
    };
    serde_json::to_writer(&mut output, value)?;
    output.flush()?;
    output.inner.sync_all()?;
    let (bytes, digest) = hash_file(&root.join(name), size)?;
    Ok(ArtifactRef {
        id: artifact_id(
            binding.source,
            binding.profile,
            &binding.kind,
            binding.page,
            None,
            &digest,
        )?,
        kind: binding.kind,
        relative_path: name.into(),
        sha256: digest,
        profile_sha256: binding.profile.into(),
        bytes,
        mime: "application/json".into(),
        page: binding.page,
        region_id: None,
    })
}

pub(crate) fn verify_artifact(root: &Path, artifact: &ArtifactRef) -> Result<()> {
    let (bytes, digest) = hash_file(&safe_path(root, &artifact.relative_path)?, artifact.bytes)?;
    ensure!(
        bytes == artifact.bytes && digest == artifact.sha256,
        "artifact bytes changed"
    );
    Ok(())
}
