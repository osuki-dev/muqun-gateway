//! Private, content-addressed copies of this Gateway for per-launch reporting.
//! Pinning carries no grant: the reporter still requires its inherited scoped context.
use std::path::Path;

use crate::work::model::{FailureCode, WorkError, WorkResult};

#[derive(Debug, Clone)]
pub struct PinnedReportingExecutable {
    pub executable: String,
    pub sha256: String,
}

pub async fn pin_reporting_executable(directory: &Path) -> WorkResult<PinnedReportingExecutable> {
    let directory = directory.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            // Linux opens the actual running inode, even after an upgrade replaces its path.
            #[cfg(target_os = "linux")]
            let source = std::fs::File::open("/proc/self/exe").map_err(native::io_error)?;
            #[cfg(not(target_os = "linux"))]
            let source = native::open_source(&std::env::current_exe().map_err(native::io_error)?)?;
            native::pin(&directory, source)
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            Err(WorkError(FailureCode::CapabilityUnavailable))
        }
    })
    .await
    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
}

#[cfg(unix)]
mod native {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Component;

    // Includes debug builds while bounding every read and copy to fixed-size buffers.
    const MAX_BYTES: u64 = 512 * 1024 * 1024;
    pub(super) fn io_error(_: std::io::Error) -> WorkError {
        WorkError(FailureCode::StorageUnavailable)
    }
    fn invalid() -> WorkError {
        WorkError(FailureCode::ScopeMismatch)
    }
    #[cfg(any(test, not(target_os = "linux")))]
    pub(super) fn open_source(path: &Path) -> WorkResult<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(io_error)
    }
    fn name(value: &std::ffi::OsStr) -> WorkResult<CString> {
        CString::new(value.as_bytes()).map_err(|_| invalid())
    }
    fn open_at(dir: &File, entry: &CString, flags: i32, mode: libc::mode_t) -> WorkResult<File> {
        // SAFETY: both descriptor and C string are valid; the returned fd has one owner.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                entry.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                mode,
            )
        };
        if fd < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn private_dir(file: &File) -> WorkResult<()> {
        let meta = file.metadata().map_err(io_error)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(invalid());
        }
        Ok(())
    }
    fn directory(path: &Path) -> WorkResult<File> {
        if !path.is_absolute() {
            return Err(invalid());
        }
        let mut dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")
            .map_err(io_error)?;
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(part) => {
                    dir = open_at(&dir, &name(part)?, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
                }
                _ => return Err(invalid()),
            }
        }
        private_dir(&dir)?;
        Ok(dir)
    }
    fn hash(file: &mut File) -> WorkResult<(String, u64)> {
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_BYTES {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(io_error)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_BYTES {
                return Err(WorkError(FailureCode::ResourceLimit));
            }
            hash.update(&buffer[..count]);
        }
        if total != metadata.len() {
            return Err(invalid());
        }
        Ok((format!("{:x}", hash.finalize()), total))
    }
    fn verify(dir: &File, entry: &CString, digest: &str, size: u64) -> WorkResult<()> {
        let mut file = open_at(dir, entry, libc::O_RDONLY, 0)?;
        let meta = file.metadata().map_err(io_error)?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o500 {
            return Err(invalid());
        }
        if hash(&mut file)? != (digest.to_owned(), size) {
            return Err(WorkError(FailureCode::ArtifactChanged));
        }
        Ok(())
    }
    pub(super) fn pin(root: &Path, mut source: File) -> WorkResult<PinnedReportingExecutable> {
        let parent = directory(root)?;
        let child = CString::new("reporting-binaries").unwrap();
        // SAFETY: directory and constant name remain valid throughout the syscall.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), child.as_ptr(), 0o700) };
        if created != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let dir = open_at(&parent, &child, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        private_dir(&dir)?;
        let (digest, size) = hash(&mut source)?;
        let filename = format!("muqun-gateway-{digest}");
        let target = CString::new(filename.as_str()).unwrap();
        let executable = root
            .join("reporting-binaries")
            .join(filename)
            .into_os_string()
            .into_string()
            .map_err(|_| invalid())?;
        // Existing cache entries must pass the same byte and permission checks as new copies.
        match open_at(&dir, &target, libc::O_RDONLY, 0) {
            Ok(_) => {
                verify(&dir, &target, &digest, size)?;
                return Ok(PinnedReportingExecutable {
                    executable,
                    sha256: digest,
                });
            }
            Err(_) => {
                // Distinguish absence from a malicious symlink or unreadable entry via fstatat.
                let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
                let result = unsafe {
                    libc::fstatat(
                        dir.as_raw_fd(),
                        target.as_ptr(),
                        metadata.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if result == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
                {
                    return Err(invalid());
                }
            }
        }
        let temp = CString::new(format!(".pin-{}", uuid::Uuid::new_v4())).unwrap();
        let mut output = open_at(
            &dir,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let result = (|| {
            source.seek(SeekFrom::Start(0)).map_err(io_error)?;
            let copied = std::io::copy(
                &mut Read::by_ref(&mut source).take(MAX_BYTES + 1),
                &mut output,
            )
            .map_err(io_error)?;
            if copied != size {
                return Err(WorkError(FailureCode::ArtifactChanged));
            }
            output.flush().map_err(io_error)?;
            output
                .set_permissions(std::fs::Permissions::from_mode(0o500))
                .map_err(io_error)?;
            output.sync_all().map_err(io_error)?;
            // Close every writable handle before the executable becomes visible (ETXTBSY).
            drop(output);
            // Verify the copied bytes before publication, not just the source's first pass.
            let mut copied = open_at(&dir, &temp, libc::O_RDONLY, 0)?;
            if hash(&mut copied)? != (digest.clone(), size) {
                return Err(WorkError(FailureCode::ArtifactChanged));
            }
            let linked = unsafe {
                libc::linkat(
                    dir.as_raw_fd(),
                    temp.as_ptr(),
                    dir.as_raw_fd(),
                    target.as_ptr(),
                    0,
                )
            };
            if linked != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            Ok(())
        })();
        // No-clobber hard-link publication; always remove our temporary name, including failures.
        let removed = unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        result?;
        if removed != 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        dir.sync_all().map_err(io_error)?;
        verify(&dir, &target, &digest, size)?;
        Ok(PinnedReportingExecutable {
            executable,
            sha256: digest,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("reporter-pin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(root.join("source"), b"immutable reporter fixture").unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn pin(&self) -> WorkResult<PinnedReportingExecutable> {
            native::pin(&self.0, native::open_source(&self.0.join("source"))?)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn exact_hash_private_mode_and_repeat_pin() {
        let fixture = Fixture::new();
        let pinned = fixture.pin().unwrap();
        assert_eq!(
            pinned.sha256,
            format!("{:x}", Sha256::digest(b"immutable reporter fixture"))
        );
        assert_eq!(
            std::fs::read(&pinned.executable).unwrap(),
            b"immutable reporter fixture"
        );
        assert_eq!(
            std::fs::metadata(&pinned.executable)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o500
        );
        assert_eq!(fixture.pin().unwrap().executable, pinned.executable);
    }
    #[test]
    fn rejects_symlink_directory_source_and_cache() {
        let fixture = Fixture::new();
        let alias = fixture.0.join("alias");
        symlink(&fixture.0, &alias).unwrap();
        assert!(native::pin(
            &alias,
            native::open_source(&fixture.0.join("source")).unwrap()
        )
        .is_err());
        symlink(fixture.0.join("source"), fixture.0.join("source-link")).unwrap();
        assert!(native::open_source(&fixture.0.join("source-link")).is_err());
        symlink(&fixture.0, fixture.0.join("reporting-binaries")).unwrap();
        assert!(fixture.pin().is_err());
        std::fs::remove_file(fixture.0.join("reporting-binaries")).unwrap();
        let pinned = fixture.pin().unwrap();
        std::fs::remove_file(&pinned.executable).unwrap();
        symlink(fixture.0.join("source"), &pinned.executable).unwrap();
        assert!(fixture.pin().is_err());
    }
    #[test]
    fn rejects_corrupted_cache_and_nonprivate_root() {
        let fixture = Fixture::new();
        let pinned = fixture.pin().unwrap();
        std::fs::set_permissions(&pinned.executable, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(&pinned.executable, b"corrupted reporter fixture").unwrap();
        std::fs::set_permissions(&pinned.executable, std::fs::Permissions::from_mode(0o500))
            .unwrap();
        assert!(fixture.pin().is_err());
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fixture.pin().is_err());
    }
    #[test]
    fn concurrent_pins_converge_without_temporary_files() {
        let fixture = Fixture::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let root = fixture.0.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    native::pin(&root, native::open_source(&root.join("source")).unwrap()).unwrap()
                })
            })
            .collect();
        let pins: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(pins.iter().all(|pin| pin.executable == pins[0].executable));
        assert_eq!(
            std::fs::read_dir(fixture.0.join("reporting-binaries"))
                .unwrap()
                .count(),
            1
        );
    }
    #[test]
    fn rejects_empty_and_oversized_source_without_publishing() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("source"), []).unwrap();
        assert!(fixture.pin().is_err());
        let source = std::fs::OpenOptions::new()
            .write(true)
            .open(fixture.0.join("source"))
            .unwrap();
        source.set_len(512 * 1024 * 1024 + 1).unwrap();
        assert!(fixture.pin().is_err());
        assert_eq!(
            std::fs::read_dir(fixture.0.join("reporting-binaries"))
                .unwrap()
                .count(),
            0
        );
    }
}
