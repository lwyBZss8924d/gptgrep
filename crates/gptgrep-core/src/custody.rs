//! Owned directory/file custody for retained-generation readers. Errors contain
//! stable codes, never input paths. This is Unix-only; no pathname fallback.

#[cfg(unix)]
mod platform {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::{Result, ensure};
    use sha2::{Digest, Sha256};

    use crate::read_cache::ReadMeter;

    const MAX_PATH_BYTES: usize = 4096;
    const MAX_COMPONENTS: usize = 128;

    struct DirectoryNode {
        file: File,
        parent: Option<(Arc<DirectoryNode>, CString)>,
        identity: (u64, u64),
    }

    #[derive(Clone)]
    pub(crate) struct HeldDirectory(Arc<DirectoryNode>);

    /// Exclusive file cursor plus its held parent and original fingerprint.
    /// This is intentionally neither Debug nor Serialize nor Clone.
    pub(crate) struct VerifiedFile {
        file: File,
        parent: HeldDirectory,
        name: CString,
        stamp: Stamp,
        sha256: String,
    }

    #[derive(PartialEq, Eq)]
    struct Stamp {
        dev: u64,
        ino: u64,
        bytes: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }

    impl Stamp {
        fn capture(file: &File) -> Result<Self> {
            let m = file
                .metadata()
                .map_err(|_| anyhow::anyhow!("custody_metadata_rejected"))?;
            ensure!(m.is_file(), "custody_regular_file_required");
            Ok(Self {
                dev: m.dev(),
                ino: m.ino(),
                bytes: m.len(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
            })
        }
    }

    fn parts(path: &Path, absolute: bool) -> Result<Vec<CString>> {
        let bytes = path.as_os_str().as_bytes();
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= MAX_PATH_BYTES
                && bytes.starts_with(b"/") == absolute,
            "custody_path_rejected"
        );
        let bytes = if absolute { &bytes[1..] } else { bytes };
        if absolute && bytes.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for part in bytes.split(|b| *b == b'/') {
            ensure!(
                !part.is_empty() && part != b"." && part != b".." && result.len() < MAX_COMPONENTS,
                "custody_path_rejected"
            );
            result.push(CString::new(part).map_err(|_| anyhow::anyhow!("custody_path_rejected"))?);
        }
        Ok(result)
    }

    fn open_at(parent: i32, name: &CString, directory: bool) -> Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | libc::O_NOCTTY
            | if directory { libc::O_DIRECTORY } else { 0 };
        // Name is validated/NUL-terminated; the returned descriptor is adopted
        // exactly once. O_NONBLOCK prevents a final FIFO from waiting for peers.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        ensure!(fd >= 0, "custody_open_rejected");
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[allow(clippy::unnecessary_cast)] // stat ABI integer widths differ on Unix.
    fn entry(parent: &File, name: &CString, directory: bool) -> Result<(u64, u64)> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        ensure!(result == 0, "custody_entry_rejected");
        let stat = unsafe { stat.assume_init() };
        let kind = if directory {
            libc::S_IFDIR
        } else {
            libc::S_IFREG
        };
        ensure!(
            stat.st_mode & libc::S_IFMT == kind,
            "custody_entry_type_rejected"
        );
        Ok((stat.st_dev as u64, stat.st_ino as u64))
    }

    impl HeldDirectory {
        pub(crate) fn open_absolute(path: &Path) -> Result<Self> {
            let components = parts(path, true)?;
            let root_name = CString::new("/").expect("literal has no NUL");
            let file = open_at(libc::AT_FDCWD, &root_name, true)?;
            let meta = file
                .metadata()
                .map_err(|_| anyhow::anyhow!("custody_metadata_rejected"))?;
            let mut held = Self(Arc::new(DirectoryNode {
                file,
                parent: None,
                identity: (meta.dev(), meta.ino()),
            }));
            for name in components {
                held = held.child(name)?;
            }
            held.validate()?;
            Ok(held)
        }

        fn child(&self, name: CString) -> Result<Self> {
            self.validate()?;
            let file = open_at(self.0.file.as_raw_fd(), &name, true)?;
            let meta = file
                .metadata()
                .map_err(|_| anyhow::anyhow!("custody_metadata_rejected"))?;
            ensure!(meta.is_dir(), "custody_directory_required");
            let identity = (meta.dev(), meta.ino());
            ensure!(
                entry(&self.0.file, &name, true)? == identity,
                "custody_directory_changed"
            );
            Ok(Self(Arc::new(DirectoryNode {
                file,
                parent: Some((self.0.clone(), name)),
                identity,
            })))
        }

        pub(crate) fn directory(&self, relative: &Path) -> Result<Self> {
            let mut held = self.clone();
            for name in parts(relative, false)? {
                held = held.child(name)?;
            }
            held.validate()?;
            Ok(held)
        }

        pub(crate) fn validate(&self) -> Result<()> {
            let mut node = &self.0;
            loop {
                let meta = node
                    .file
                    .metadata()
                    .map_err(|_| anyhow::anyhow!("custody_metadata_rejected"))?;
                ensure!(
                    meta.is_dir() && (meta.dev(), meta.ino()) == node.identity,
                    "custody_directory_changed"
                );
                match &node.parent {
                    Some((parent, name)) => {
                        ensure!(
                            entry(&parent.file, name, true)? == node.identity,
                            "custody_directory_changed"
                        );
                        node = parent;
                    }
                    None => break,
                }
            }
            Ok(())
        }

        pub(crate) fn open_verified(
            &self,
            relative: &Path,
            expected_bytes: Option<u64>,
            sha256: &str,
            limit: u64,
            meter: &ReadMeter,
        ) -> Result<VerifiedFile> {
            ensure!(
                sha256.len() == 64
                    && sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "custody_digest_rejected"
            );
            let mut components = parts(relative, false)?;
            let name = components
                .pop()
                .ok_or_else(|| anyhow::anyhow!("custody_path_rejected"))?;
            let mut parent = self.clone();
            for part in components {
                parent = parent.child(part)?;
            }
            parent.validate()?;
            let expected_identity = entry(&parent.0.file, &name, false)?;
            let file = open_at(parent.0.file.as_raw_fd(), &name, false)?;
            let stamp = Stamp::capture(&file)?;
            ensure!(
                (stamp.dev, stamp.ino) == expected_identity,
                "custody_file_changed"
            );
            ensure!(
                stamp.bytes <= limit && expected_bytes.is_none_or(|n| n == stamp.bytes),
                "custody_file_size_rejected"
            );
            let mut held = VerifiedFile {
                file,
                parent,
                name,
                stamp,
                sha256: sha256.into(),
            };
            held.check()?;
            held.file
                .seek(SeekFrom::Start(0))
                .map_err(|_| anyhow::anyhow!("custody_seek_rejected"))?;
            let mut hash = Sha256::new();
            let mut scratch = [0u8; 32 * 1024];
            let mut remaining = held.stamp.bytes;
            while remaining > 0 {
                let count = remaining.min(scratch.len() as u64) as usize;
                let n = held
                    .file
                    .read(&mut scratch[..count])
                    .map_err(|_| anyhow::anyhow!("custody_read_rejected"))?;
                meter.record_physical_read(n)?;
                ensure!(n > 0, "custody_file_size_changed");
                hash.update(&scratch[..n]);
                meter.record_physical_hash(n)?;
                remaining -= n as u64;
            }
            let n = held
                .file
                .read(&mut scratch[..1])
                .map_err(|_| anyhow::anyhow!("custody_read_rejected"))?;
            meter.record_physical_read(n)?;
            ensure!(
                n == 0 && format!("{:x}", hash.finalize()) == sha256,
                "custody_file_digest_changed"
            );
            held.check()?;
            Ok(held)
        }
    }

    impl VerifiedFile {
        pub(crate) fn check(&self) -> Result<()> {
            self.parent.validate()?;
            ensure!(
                entry(&self.parent.0.file, &self.name, false)? == (self.stamp.dev, self.stamp.ino)
                    && Stamp::capture(&self.file)? == self.stamp,
                "custody_file_changed"
            );
            Ok(())
        }
        pub(crate) fn file_mut(&mut self) -> &mut File {
            &mut self.file
        }
        pub(crate) fn bytes(&self) -> u64 {
            self.stamp.bytes
        }

        /// Admission must precede this call; it also independently enforces the
        /// selected bound before reserving and verifies the actual copied bytes.
        pub(crate) fn read_bounded(&mut self, limit: u64, meter: &ReadMeter) -> Result<Vec<u8>> {
            self.check()?;
            ensure!(self.stamp.bytes <= limit, "custody_read_limit_rejected");
            let mut output = Vec::new();
            let bytes = usize::try_from(self.stamp.bytes)
                .map_err(|_| anyhow::anyhow!("custody_read_limit_rejected"))?;
            output
                .try_reserve_exact(bytes)
                .map_err(|_| anyhow::anyhow!("custody_allocation_rejected"))?;
            output.resize(bytes, 0);
            self.file
                .seek(SeekFrom::Start(0))
                .map_err(|_| anyhow::anyhow!("custody_seek_rejected"))?;
            let mut at = 0;
            while at < bytes {
                let n = self
                    .file
                    .read(&mut output[at..])
                    .map_err(|_| anyhow::anyhow!("custody_read_rejected"))?;
                meter.record_physical_read(n)?;
                ensure!(n > 0, "custody_file_size_changed");
                at += n;
            }
            let n = self
                .file
                .read(&mut [0u8; 1])
                .map_err(|_| anyhow::anyhow!("custody_read_rejected"))?;
            meter.record_physical_read(n)?;
            meter.record_physical_hash(bytes)?;
            ensure!(
                n == 0 && format!("{:x}", Sha256::digest(&output)) == self.sha256,
                "custody_file_digest_changed"
            );
            self.check()?;
            Ok(output)
        }
        pub(crate) fn into_file(mut self) -> Result<File> {
            self.check()?;
            self.file
                .seek(SeekFrom::Start(0))
                .map_err(|_| anyhow::anyhow!("custody_seek_rejected"))?;
            Ok(self.file)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::symlink;
        fn root() -> (tempfile::TempDir, std::path::PathBuf) {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().canonicalize().unwrap();
            (temp, path)
        }
        fn sha(bytes: &[u8]) -> String {
            format!("{:x}", Sha256::digest(bytes))
        }
        #[test]
        fn rejects_non_normal_paths_and_symlink_ancestors() {
            let (_temp, path) = root();
            for bad in ["relative", "/tmp/./x", "/tmp/../x", "//tmp", "/tmp/"] {
                assert!(HeldDirectory::open_absolute(Path::new(bad)).is_err());
            }
            fs::create_dir(path.join("real")).unwrap();
            symlink("real", path.join("link")).unwrap();
            assert!(HeldDirectory::open_absolute(&path.join("link")).is_err());
            let held = HeldDirectory::open_absolute(&path).unwrap();
            for bad in ["../real", "./real", "real/", "real//x", "/real", "link"] {
                assert!(held.directory(Path::new(bad)).is_err());
            }
        }
        #[test]
        fn held_directory_replacement_is_detected() {
            let (_temp, path) = root();
            fs::create_dir(path.join("dir")).unwrap();
            let held = HeldDirectory::open_absolute(&path)
                .unwrap()
                .directory(Path::new("dir"))
                .unwrap();
            fs::rename(path.join("dir"), path.join("retained")).unwrap();
            fs::create_dir(path.join("dir")).unwrap();
            assert!(held.validate().is_err());
        }
        #[test]
        fn final_symlink_fifo_and_directory_are_rejected_without_blocking() {
            let (_temp, path) = root();
            fs::write(path.join("file"), b"abc").unwrap();
            symlink("file", path.join("link")).unwrap();
            let fifo = CString::new(path.join("fifo").as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            fs::create_dir(path.join("dir")).unwrap();
            let held = HeldDirectory::open_absolute(&path).unwrap();
            let meter = ReadMeter::default();
            for name in ["link", "fifo", "dir"] {
                assert!(
                    held.open_verified(Path::new(name), Some(3), &sha(b"abc"), 100, &meter)
                        .is_err()
                );
            }
            let file = held
                .open_verified(Path::new("file"), Some(3), &sha(b"abc"), 100, &meter)
                .unwrap();
            assert!(
                unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK
                    != 0
            );
            assert!(
                unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC
                    != 0
            );
        }
        #[test]
        fn verifies_bounded_actual_bytes_and_detects_final_file_replacement() {
            let (_temp, path) = root();
            fs::write(path.join("file"), b"abc").unwrap();
            let held = HeldDirectory::open_absolute(&path).unwrap();
            let meter = ReadMeter::default();
            assert!(
                held.open_verified(Path::new("file"), Some(4), &sha(b"abc"), 100, &meter)
                    .is_err()
            );
            assert!(
                held.open_verified(Path::new("file"), Some(3), &sha(b"abc"), 2, &meter)
                    .is_err()
            );
            let mut file = held
                .open_verified(Path::new("file"), Some(3), &sha(b"abc"), 100, &meter)
                .unwrap();
            assert!(file.read_bounded(2, &meter).is_err());
            assert_eq!(file.read_bounded(3, &meter).unwrap(), b"abc");
            fs::rename(path.join("file"), path.join("old")).unwrap();
            fs::write(path.join("file"), b"abc").unwrap();
            assert!(file.check().is_err());
            let counts = meter.snapshot().unwrap();
            assert_eq!(counts.physical_read_bytes, 6);
            assert_eq!(counts.physical_hash_bytes, 6);
        }
        #[test]
        fn digest_and_in_place_changes_fail_without_exposing_paths() {
            let (_temp, path) = root();
            fs::write(path.join("private-file"), b"abc").unwrap();
            let held = HeldDirectory::open_absolute(&path).unwrap();
            let meter = ReadMeter::default();
            let error = held
                .open_verified(
                    Path::new("private-file"),
                    Some(3),
                    &sha(b"bad"),
                    100,
                    &meter,
                )
                .err()
                .unwrap()
                .to_string();
            assert_eq!(error, "custody_file_digest_changed");
            let file = held
                .open_verified(
                    Path::new("private-file"),
                    Some(3),
                    &sha(b"abc"),
                    100,
                    &meter,
                )
                .unwrap();
            fs::write(path.join("private-file"), b"bad").unwrap();
            assert!(file.check().is_err());
            assert!(!error.contains(path.to_str().unwrap()));
            assert!(!error.contains("private-file"));
        }
    }
}

#[cfg(unix)]
pub(crate) use platform::{HeldDirectory, VerifiedFile};

#[cfg(not(unix))]
mod platform {
    use crate::read_cache::ReadMeter;
    use anyhow::{Result, bail};
    use std::{fs::File, path::Path};
    #[derive(Clone)]
    pub(crate) struct HeldDirectory;
    pub(crate) struct VerifiedFile(File);
    impl HeldDirectory {
        pub(crate) fn open_absolute(_: &Path) -> Result<Self> {
            bail!("capability_unavailable: Unix file custody required")
        }
        pub(crate) fn directory(&self, _: &Path) -> Result<Self> {
            bail!("capability_unavailable: Unix file custody required")
        }
        pub(crate) fn validate(&self) -> Result<()> {
            bail!("capability_unavailable: Unix file custody required")
        }
        pub(crate) fn open_verified(
            &self,
            _: &Path,
            _: Option<u64>,
            _: &str,
            _: u64,
            _: &ReadMeter,
        ) -> Result<VerifiedFile> {
            bail!("capability_unavailable: Unix file custody required")
        }
    }
    impl VerifiedFile {
        pub(crate) fn check(&self) -> Result<()> {
            bail!("capability_unavailable: Unix file custody required")
        }
        pub(crate) fn file_mut(&mut self) -> &mut File {
            &mut self.0
        }
        pub(crate) fn bytes(&self) -> u64 {
            0
        }
        pub(crate) fn read_bounded(&mut self, _: u64, _: &ReadMeter) -> Result<Vec<u8>> {
            bail!("capability_unavailable: Unix file custody required")
        }
        pub(crate) fn into_file(self) -> Result<File> {
            bail!("capability_unavailable: Unix file custody required")
        }
    }
}
#[cfg(not(unix))]
pub(crate) use platform::{HeldDirectory, VerifiedFile};
