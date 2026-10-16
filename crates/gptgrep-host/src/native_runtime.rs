//! Verified project-local native runtime assets. This stages files only; native
//! discovery and explicit Skill inputs remain separate launch-time observations.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRuntimePackage {
    pub root: PathBuf,
    pub manifest_sha256: String,
}

#[derive(Debug, PartialEq)]
pub(crate) struct NativeRuntimeProjection {
    pub skills: Vec<crate::NativeSkillInput>,
    pub role_capabilities: std::collections::BTreeMap<String, serde_json::Value>,
}

pub(crate) fn stage_native_runtime(
    package: &NativeRuntimePackage,
    cwd: &Path,
) -> Result<NativeRuntimeProjection> {
    #[cfg(unix)]
    {
        implementation::stage(package, cwd)
    }
    #[cfg(not(unix))]
    {
        let _ = (package, cwd);
        anyhow::bail!("host_native_runtime_unix_required")
    }
}

#[cfg(unix)]
mod implementation {
    use super::*;
    use anyhow::{anyhow, ensure};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeSet,
        ffi::{CString, OsStr},
        fs::{self, File, Metadata, OpenOptions},
        io::{self, Read, Seek, SeekFrom, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
        path::Component,
    };

    const MANIFEST_PATH: &str = "runtime/codex/manifest.json";
    const MAX_MANIFEST_BYTES: u64 = 65_536;
    const MAX_FILE_BYTES: u64 = 1_048_576;
    const MAX_PACKAGE_BYTES: u64 = 8_388_608;
    const MAX_FILES: usize = 64;

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        schema: String,
        files: Vec<FilePin>,
        skills: Vec<Entry>,
        agents: Vec<Entry>,
        supervisor_instructions: PathBuf,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct FilePin {
        source_path: PathBuf,
        target_path: PathBuf,
        bytes: u64,
        sha256: String,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        name: String,
        source_path: PathBuf,
        target_path: PathBuf,
    }

    pub(super) fn stage(
        package: &NativeRuntimePackage,
        cwd: &Path,
    ) -> Result<NativeRuntimeProjection> {
        ensure!(
            digest_shape(&package.manifest_sha256),
            "host_native_runtime_manifest_pin"
        );
        let source_root = Directory::root(&package.root)?;
        let destination_root = Directory::root(cwd)?;
        let mut manifest_file = source_root
            .file(Path::new(MANIFEST_PATH))?
            .ok_or_else(|| anyhow!("host_native_runtime_manifest_file"))?;
        let manifest_metadata = manifest_file
            .metadata()
            .map_err(|_| anyhow!("host_native_runtime_manifest_file"))?;
        ensure!(
            manifest_metadata.is_file()
                && manifest_metadata.len() > 0
                && manifest_metadata.len() <= MAX_MANIFEST_BYTES,
            "host_native_runtime_manifest_file"
        );
        let mut manifest_bytes = Vec::with_capacity(manifest_metadata.len() as usize);
        Read::by_ref(&mut manifest_file)
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut manifest_bytes)
            .map_err(|_| anyhow!("host_native_runtime_manifest_file"))?;
        ensure!(
            manifest_bytes.len() as u64 == manifest_metadata.len()
                && stable(
                    &manifest_metadata,
                    &manifest_file
                        .metadata()
                        .map_err(|_| anyhow!("host_native_runtime_manifest_file"))?
                )
                && format!("{:x}", Sha256::digest(&manifest_bytes)) == package.manifest_sha256,
            "host_native_runtime_manifest_pin"
        );
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| anyhow!("host_native_runtime_manifest_schema"))?;
        validate_manifest(&manifest)?;

        // Hold and verify every source before creating any destination directory
        // or file. Held file descriptors preserve the selected source identity.
        let mut sources = Vec::with_capacity(manifest.files.len());
        for pin in &manifest.files {
            let mut file = source_root
                .file(&pin.source_path)?
                .ok_or_else(|| anyhow!("host_native_runtime_source_missing"))?;
            let metadata = file
                .metadata()
                .map_err(|_| anyhow!("host_native_runtime_source"))?;
            verify(&mut file, pin)?;
            sources.push((file, metadata));
        }
        let mut existing = 0;
        for pin in &manifest.files {
            if let Some(mut file) = destination_root.file(&pin.target_path)? {
                verify(&mut file, pin).map_err(|_| anyhow!("host_native_runtime_collision"))?;
                existing += 1;
            }
        }
        ensure!(
            existing == 0 || existing == manifest.files.len(),
            "host_native_runtime_partial_destination"
        );
        source_root.assert_visible()?;
        destination_root.assert_visible()?;
        for (source, metadata) in &sources {
            ensure!(
                stable(
                    metadata,
                    &source
                        .metadata()
                        .map_err(|_| anyhow!("host_native_runtime_source"))?
                ),
                "host_native_runtime_source_changed"
            );
        }

        if existing == 0 {
            for (pin, (source, metadata)) in manifest.files.iter().zip(sources.iter_mut()) {
                let parent = destination_root
                    .parent(&pin.target_path, true)?
                    .ok_or_else(|| anyhow!("host_native_runtime_destination"))?;
                let name = pin
                    .target_path
                    .file_name()
                    .ok_or_else(|| anyhow!("host_native_runtime_path"))?;
                let mut output = parent.create_file(name)?;
                copy_verified(source, metadata, &mut output, pin)?;
            }
        }
        // Confirm the visible namespace still names the bytes just staged. An
        // I/O failure may leave new partial files; a later launch rejects them.
        destination_root.assert_visible()?;
        for pin in &manifest.files {
            let path = cwd.join(&pin.target_path);
            ensure!(
                fs::canonicalize(&path).map_err(|_| anyhow!("host_native_runtime_destination"))?
                    == path,
                "host_native_runtime_destination"
            );
            let mut file = destination_root
                .file(&pin.target_path)?
                .ok_or_else(|| anyhow!("host_native_runtime_destination"))?;
            verify(&mut file, pin)?;
        }
        let selected = manifest
            .skills
            .iter()
            .map(|skill| {
                let pin = manifest
                    .files
                    .iter()
                    .find(|pin| pin.target_path == skill.target_path)
                    .expect("validated manifest membership");
                crate::NativeSkillInput {
                    name: skill.name.clone(),
                    path: cwd.join(&skill.target_path),
                    sha256: pin.sha256.clone(),
                }
            })
            .collect();
        let role_capabilities = manifest
            .agents
            .iter()
            .map(|agent| {
                Ok((
                    format!("agents.{}.config_file", agent.name),
                    serde_json::to_value(cwd.join(&agent.target_path))
                        .map_err(|_| anyhow!("host_native_runtime_path"))?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(NativeRuntimeProjection {
            skills: selected,
            role_capabilities,
        })
    }

    fn validate_manifest(manifest: &Manifest) -> Result<()> {
        ensure!(
            manifest.schema == "gptgrep.native-runtime-package.v1"
                && manifest.supervisor_instructions == Path::new("AGENTS.md")
                && !manifest.files.is_empty()
                && manifest.files.len() <= MAX_FILES
                && !manifest.skills.is_empty()
                && manifest.skills.len() <= 32
                && manifest.agents.len() <= 32,
            "host_native_runtime_manifest_schema"
        );
        let mut source_paths = BTreeSet::new();
        let mut target_paths = BTreeSet::new();
        let mut total = 0_u64;
        for pin in &manifest.files {
            relative_path(&pin.source_path)?;
            relative_path(&pin.target_path)?;
            ensure!(
                source_paths.insert(pin.source_path.as_path())
                    && target_paths.insert(pin.target_path.as_path())
                    && pin.bytes > 0
                    && pin.bytes <= MAX_FILE_BYTES
                    && digest_shape(&pin.sha256),
                "host_native_runtime_file_pin"
            );
            total = total
                .checked_add(pin.bytes)
                .ok_or_else(|| anyhow!("host_native_runtime_size"))?;
        }
        ensure!(total <= MAX_PACKAGE_BYTES, "host_native_runtime_size");
        let mut assigned = BTreeSet::new();
        ensure!(
            target_paths.contains(Path::new("AGENTS.md")),
            "host_native_runtime_supervisor"
        );
        assigned.insert(Path::new("AGENTS.md"));
        for (entries, skills) in [(&manifest.skills, true), (&manifest.agents, false)] {
            let mut names = BTreeSet::new();
            for entry in entries {
                ensure!(
                    !entry.name.is_empty()
                        && entry.name.len() <= 128
                        && entry.name.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.'))
                        && (skills || !entry.name.contains('.'))
                        && entry.name != "."
                        && entry.name != ".."
                        && names.insert(entry.name.as_str()),
                    "host_native_runtime_entry_name"
                );
                relative_path(&entry.source_path)?;
                relative_path(&entry.target_path)?;
                let expected = if skills {
                    Path::new(".agents/skills")
                        .join(&entry.name)
                        .join("SKILL.md")
                } else {
                    Path::new(".codex/agents").join(format!("{}.toml", entry.name))
                };
                ensure!(
                    entry.target_path == expected
                        && assigned.insert(entry.target_path.as_path())
                        && manifest
                            .files
                            .iter()
                            .any(|pin| pin.source_path == entry.source_path
                                && pin.target_path == entry.target_path),
                    "host_native_runtime_entry_pin"
                );
            }
        }
        ensure!(
            assigned == target_paths,
            "host_native_runtime_unassigned_file"
        );
        Ok(())
    }

    fn relative_path(path: &Path) -> Result<()> {
        ensure!(
            path.to_str().is_some_and(|text| !text.is_empty()
                && text.len() <= 4096
                && !text.chars().any(char::is_control)
                && !text.contains('\\')
                && !text.contains(':')
                && text
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."))
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "host_native_runtime_path"
        );
        Ok(())
    }
    fn digest_shape(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
    fn same_identity(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    fn stable(a: &Metadata, b: &Metadata) -> bool {
        same_identity(a, b)
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }

    fn verify(file: &mut File, pin: &FilePin) -> Result<()> {
        let metadata = file
            .metadata()
            .map_err(|_| anyhow!("host_native_runtime_file"))?;
        ensure!(
            metadata.is_file() && metadata.len() == pin.bytes,
            "host_native_runtime_file_pin"
        );
        file.seek(SeekFrom::Start(0))
            .map_err(|_| anyhow!("host_native_runtime_file"))?;
        let (bytes, digest) = stream(file, None)?;
        ensure!(
            bytes == pin.bytes
                && digest == pin.sha256
                && stable(
                    &metadata,
                    &file
                        .metadata()
                        .map_err(|_| anyhow!("host_native_runtime_file"))?
                ),
            "host_native_runtime_file_pin"
        );
        Ok(())
    }
    fn copy_verified(
        source: &mut File,
        metadata: &Metadata,
        output: &mut File,
        pin: &FilePin,
    ) -> Result<()> {
        ensure!(
            stable(
                metadata,
                &source
                    .metadata()
                    .map_err(|_| anyhow!("host_native_runtime_source"))?
            ),
            "host_native_runtime_source_changed"
        );
        source
            .seek(SeekFrom::Start(0))
            .map_err(|_| anyhow!("host_native_runtime_source"))?;
        let (bytes, digest) = stream(source, Some(&mut *output))?;
        ensure!(
            bytes == pin.bytes
                && digest == pin.sha256
                && stable(
                    metadata,
                    &source
                        .metadata()
                        .map_err(|_| anyhow!("host_native_runtime_source"))?
                ),
            "host_native_runtime_source_changed"
        );
        output
            .sync_all()
            .map_err(|_| anyhow!("host_native_runtime_destination_sync"))?;
        Ok(())
    }
    fn stream(source: &mut File, mut output: Option<&mut File>) -> Result<(u64, String)> {
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let bytes = source
                .read(&mut buffer)
                .map_err(|_| anyhow!("host_native_runtime_read"))?;
            if bytes == 0 {
                break;
            }
            total = total
                .checked_add(bytes as u64)
                .ok_or_else(|| anyhow!("host_native_runtime_size"))?;
            ensure!(total <= MAX_FILE_BYTES, "host_native_runtime_size");
            digest.update(&buffer[..bytes]);
            if let Some(output) = output.as_deref_mut() {
                output
                    .write_all(&buffer[..bytes])
                    .map_err(|_| anyhow!("host_native_runtime_write"))?;
            }
        }
        Ok((total, format!("{:x}", digest.finalize())))
    }

    struct Directory {
        path: PathBuf,
        file: File,
    }
    impl Directory {
        fn root(path: &Path) -> Result<Self> {
            ensure!(
                path.is_absolute()
                    && path.to_str().is_some_and(
                        |text| text.len() <= 4096 && !text.chars().any(char::is_control)
                    )
                    && fs::canonicalize(path).map_err(|_| anyhow!("host_native_runtime_root"))?
                        == path,
                "host_native_runtime_root"
            );
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map_err(|_| anyhow!("host_native_runtime_root"))?;
            let result = Self {
                path: path.to_owned(),
                file,
            };
            result.assert_visible()?;
            Ok(result)
        }
        fn assert_visible(&self) -> Result<()> {
            let current = fs::symlink_metadata(&self.path)
                .map_err(|_| anyhow!("host_native_runtime_root"))?;
            ensure!(
                current.is_dir()
                    && same_identity(
                        &current,
                        &self
                            .file
                            .metadata()
                            .map_err(|_| anyhow!("host_native_runtime_root"))?
                    )
                    && fs::canonicalize(&self.path)
                        .map_err(|_| anyhow!("host_native_runtime_root"))?
                        == self.path,
                "host_native_runtime_root_changed"
            );
            Ok(())
        }
        fn duplicate(&self) -> Result<Self> {
            Ok(Self {
                path: self.path.clone(),
                file: self
                    .file
                    .try_clone()
                    .map_err(|_| anyhow!("host_native_runtime_directory"))?,
            })
        }
        fn child(&self, name: &OsStr, create: bool) -> Result<Option<Self>> {
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let file = match open_at(&self.file, name, flags, 0) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    let name_c = c_name(name).map_err(|_| anyhow!("host_native_runtime_path"))?;
                    // SAFETY: the held directory FD and NUL-terminated single component are valid.
                    let status =
                        unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
                    ensure!(status == 0, "host_native_runtime_directory_create");
                    open_at(&self.file, name, flags, 0)
                        .map_err(|_| anyhow!("host_native_runtime_directory"))?
                }
                Err(_) => return Err(anyhow!("host_native_runtime_directory")),
            };
            Ok(Some(Self {
                path: self.path.join(name),
                file,
            }))
        }
        fn parent(&self, relative: &Path, create: bool) -> Result<Option<Self>> {
            let mut directory = self.duplicate()?;
            for component in relative
                .parent()
                .ok_or_else(|| anyhow!("host_native_runtime_path"))?
                .components()
            {
                let Component::Normal(name) = component else {
                    return Err(anyhow!("host_native_runtime_path"));
                };
                let Some(child) = directory.child(name, create)? else {
                    return Ok(None);
                };
                directory = child;
            }
            Ok(Some(directory))
        }
        fn file(&self, relative: &Path) -> Result<Option<File>> {
            let Some(parent) = self.parent(relative, false)? else {
                return Ok(None);
            };
            let name = relative
                .file_name()
                .ok_or_else(|| anyhow!("host_native_runtime_path"))?;
            match open_at(
                &parent.file,
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0,
            ) {
                Ok(file) => {
                    ensure!(
                        file.metadata()
                            .map_err(|_| anyhow!("host_native_runtime_file"))?
                            .is_file(),
                        "host_native_runtime_file"
                    );
                    Ok(Some(file))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(anyhow!("host_native_runtime_file")),
            }
        }
        fn create_file(self, name: &OsStr) -> Result<File> {
            let file = open_at(
                &self.file,
                name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
            .map_err(|_| anyhow!("host_native_runtime_destination_create"))?;
            Ok(file)
        }
    }
    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }
    fn open_at(
        parent: &File,
        name: &OsStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<File> {
        let name = c_name(name)?;
        // SAFETY: parent remains open, name is NUL-terminated, and a mode is supplied for O_CREAT.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a newly owned FD; ownership transfers exactly once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        struct Fixture {
            _temporary: tempfile::TempDir,
            package: NativeRuntimePackage,
            cwd: PathBuf,
        }
        fn fixture() -> Result<Fixture> {
            let temporary = tempfile::tempdir()?;
            let base = temporary.path().canonicalize()?;
            let root = base.join("package");
            fs::create_dir(&root)?;
            let mut manifest = Manifest {
                schema: "gptgrep.native-runtime-package.v1".into(),
                files: vec![],
                skills: vec![],
                agents: vec![],
                supervisor_instructions: "AGENTS.md".into(),
            };
            fn asset(
                root: &Path,
                manifest: &mut Manifest,
                source: &str,
                target: &str,
                bytes: &[u8],
            ) -> Result<()> {
                let path = root.join(source);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, bytes)?;
                manifest.files.push(FilePin {
                    source_path: source.into(),
                    target_path: target.into(),
                    bytes: bytes.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                });
                Ok(())
            }
            asset(
                &root,
                &mut manifest,
                "runtime/codex/AGENTS.md",
                "AGENTS.md",
                b"Synthetic supervisor instructions.\n",
            )?;
            for name in ["synthetic", "document", "ocr", "retrieval"] {
                let source = format!("skills/{name}/SKILL.md");
                let target = format!(".agents/skills/{name}/SKILL.md");
                asset(&root, &mut manifest, &source, &target,
                    format!("---\nname: {name}\ndescription: Synthetic skill.\n---\nUse source evidence.\n").as_bytes())?;
                manifest.skills.push(Entry {
                    name: name.into(),
                    source_path: source.into(),
                    target_path: target.into(),
                });
            }
            for name in ["document", "ocr", "retrieval"] {
                let source = format!("runtime/codex/agents/{name}.toml");
                let target = format!(".codex/agents/{name}.toml");
                asset(&root, &mut manifest, &source, &target,
                    format!("name = \"{name}\"\ndescription = \"Synthetic role\"\ndeveloper_instructions = \"Use source evidence.\"\n").as_bytes())?;
                manifest.agents.push(Entry {
                    name: name.into(),
                    source_path: source.into(),
                    target_path: target.into(),
                });
            }
            let bytes = serde_json::to_vec(&manifest)?;
            fs::write(root.join(MANIFEST_PATH), &bytes)?;
            let cwd = base.join("jobs").join("random-page-3827");
            fs::create_dir_all(&cwd)?;
            Ok(Fixture {
                _temporary: temporary,
                package: NativeRuntimePackage {
                    root,
                    manifest_sha256: format!("{:x}", Sha256::digest(&bytes)),
                },
                cwd,
            })
        }
        fn manifest(fixture: &Fixture) -> Result<Manifest> {
            Ok(serde_json::from_slice(&fs::read(
                fixture.package.root.join(MANIFEST_PATH),
            )?)?)
        }
        fn replace_manifest(fixture: &mut Fixture, manifest: &Manifest) -> Result<()> {
            let bytes = serde_json::to_vec(manifest)?;
            fixture.package.manifest_sha256 = format!("{:x}", Sha256::digest(&bytes));
            fs::write(fixture.package.root.join(MANIFEST_PATH), bytes)?;
            Ok(())
        }
        fn empty(cwd: &Path) -> bool {
            fs::read_dir(cwd).unwrap().next().is_none()
        }

        #[test]
        fn random_page_cwd_gets_exact_assets_and_repeating_same_package_is_idempotent() -> Result<()>
        {
            let fixture = fixture()?;
            let selected = stage(&fixture.package, &fixture.cwd)?;
            assert_eq!(selected.skills.len(), 4);
            assert_eq!(selected.role_capabilities.len(), 3);
            assert_eq!(
                selected.role_capabilities["agents.ocr.config_file"],
                serde_json::json!(fixture.cwd.join(".codex/agents/ocr.toml"))
            );
            for pin in manifest(&fixture)?.files {
                assert_eq!(
                    fs::read(fixture.cwd.join(&pin.target_path))?,
                    fs::read(fixture.package.root.join(&pin.source_path))?
                );
            }
            for skill in &selected.skills {
                assert_eq!(
                    skill.path,
                    fixture
                        .cwd
                        .join(".agents/skills")
                        .join(&skill.name)
                        .join("SKILL.md")
                );
                assert_eq!(skill.path.canonicalize()?, skill.path);
            }
            assert_eq!(stage(&fixture.package, &fixture.cwd)?, selected);
            assert!(!fixture.cwd.join(".codex/config.toml").exists());
            Ok(())
        }
        #[test]
        fn source_or_manifest_tamper_fails_before_any_destination_effect() -> Result<()> {
            let fixture = fixture()?;
            let source = fixture
                .package
                .root
                .join(&manifest(&fixture)?.files.last().unwrap().source_path);
            fs::write(source, b"tampered")?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert!(empty(&fixture.cwd));
            fs::write(fixture.package.root.join(MANIFEST_PATH), b"{}")?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert!(empty(&fixture.cwd));
            Ok(())
        }
        #[test]
        fn traversal_or_unassigned_target_is_rejected_before_copy() -> Result<()> {
            for invalid in [
                "../outside",
                "/absolute",
                "skills/../outside",
                "skills//outside",
                "skills\\outside",
            ] {
                let mut fixture = fixture()?;
                let mut selected = manifest(&fixture)?;
                selected.files.last_mut().unwrap().source_path = invalid.into();
                replace_manifest(&mut fixture, &selected)?;
                assert!(stage(&fixture.package, &fixture.cwd).is_err());
                assert!(empty(&fixture.cwd));
            }
            let mut fixture = fixture()?;
            let mut selected = manifest(&fixture)?;
            selected.files.last_mut().unwrap().target_path = ".codex/config.toml".into();
            replace_manifest(&mut fixture, &selected)?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert!(empty(&fixture.cwd));
            Ok(())
        }
        #[test]
        fn dotted_agent_name_is_rejected_before_projection() -> Result<()> {
            let mut fixture = fixture()?;
            let mut selected = manifest(&fixture)?;
            let role = &mut selected.agents[0];
            let old_source = role.source_path.clone();
            role.name = "document.reader".into();
            role.source_path = "runtime/codex/agents/document.reader.toml".into();
            role.target_path = ".codex/agents/document.reader.toml".into();
            let contents = b"name = \"document.reader\"\ndescription = \"Synthetic role\"\ndeveloper_instructions = \"Use source evidence.\"\n";
            fs::write(fixture.package.root.join(&role.source_path), contents)?;
            let pin = selected
                .files
                .iter_mut()
                .find(|pin| pin.source_path == old_source)
                .unwrap();
            pin.source_path = role.source_path.clone();
            pin.target_path = role.target_path.clone();
            pin.bytes = contents.len() as u64;
            pin.sha256 = format!("{:x}", Sha256::digest(contents));
            replace_manifest(&mut fixture, &selected)?;
            assert_eq!(
                stage(&fixture.package, &fixture.cwd)
                    .unwrap_err()
                    .to_string(),
                "host_native_runtime_entry_name"
            );
            assert!(empty(&fixture.cwd));
            Ok(())
        }
        #[test]
        fn source_and_destination_symlinks_are_rejected_before_copy() -> Result<()> {
            let fixture = fixture()?;
            let source = fixture
                .package
                .root
                .join(&manifest(&fixture)?.files.last().unwrap().source_path);
            fs::remove_file(&source)?;
            std::os::unix::fs::symlink(
                fixture.package.root.join("runtime/codex/AGENTS.md"),
                source,
            )?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert!(empty(&fixture.cwd));
            let second = super::tests::fixture()?;
            let outside = second.package.root.join("outside");
            fs::create_dir(&outside)?;
            std::os::unix::fs::symlink(&outside, second.cwd.join(".agents"))?;
            assert!(stage(&second.package, &second.cwd).is_err());
            assert!(!second.cwd.join("AGENTS.md").exists());
            assert!(empty(&outside));
            Ok(())
        }
        #[test]
        fn existing_collision_and_partial_exact_projection_preserve_existing_files() -> Result<()> {
            let fixture = fixture()?;
            let target = fixture.cwd.join("AGENTS.md");
            fs::write(&target, b"Unrelated existing instructions.")?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert_eq!(fs::read(&target)?, b"Unrelated existing instructions.");
            assert!(!fixture.cwd.join(".agents").exists());
            fs::copy(
                fixture.package.root.join("runtime/codex/AGENTS.md"),
                &target,
            )?;
            assert!(stage(&fixture.package, &fixture.cwd).is_err());
            assert!(!fixture.cwd.join(".agents").exists());
            Ok(())
        }
    }
}
