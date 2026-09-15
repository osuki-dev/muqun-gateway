//! Immutable artifact bytes, addressed by content and opened relative to directory descriptors.
//! This is deliberately not an asset scanner: callers supply an authorized task repository.
use std::path::{Component, Path};

use crate::work::model::{ArtifactRef, FailureCode, WorkError, WorkResult};

pub const MAX_ARTIFACT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_SUBMISSION_BYTES: u64 = 100 * 1024 * 1024;
/// A fixed first-version budget: one GiB across all sessions, including orphaned and
/// interrupted captures. Exhaustion refuses new bytes; nothing is garbage-collected.
pub const MAX_STORED_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_STORED_ENTRIES: usize = 16_384;

fn failure(code: FailureCode) -> WorkError {
    WorkError(code)
}

pub fn validate_refs(repo: &Path, artifacts: &[ArtifactRef]) -> WorkResult<()> {
    if artifacts.len() > 32 {
        return Err(failure(FailureCode::ResourceLimit));
    }
    let mut total = 0u64;
    for artifact in artifacts {
        relative_path(repo, &artifact.path)?;
        if artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(failure(FailureCode::InvalidInput));
        }
        total = total
            .checked_add(artifact.size_bytes)
            .ok_or_else(|| failure(FailureCode::ResourceLimit))?;
        if artifact.size_bytes > MAX_ARTIFACT_BYTES || total > MAX_SUBMISSION_BYTES {
            return Err(failure(FailureCode::ResourceLimit));
        }
    }
    Ok(())
}

fn relative_path<'a>(repo: &Path, raw: &'a str) -> WorkResult<&'a Path> {
    if raw.is_empty() || raw.len() > 4096 || raw.contains('\0') {
        return Err(failure(FailureCode::InvalidInput));
    }
    let path = Path::new(raw);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(failure(FailureCode::ScopeMismatch));
    }
    let relative = if path.is_absolute() {
        path.strip_prefix(repo)
            .map_err(|_| failure(FailureCode::ScopeMismatch))?
    } else {
        path
    };
    if !relative
        .components()
        .any(|c| matches!(c, Component::Normal(_)))
    {
        return Err(failure(FailureCode::InvalidInput));
    }
    Ok(relative)
}

#[cfg(unix)]
mod native {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    fn io_failure(error: std::io::Error) -> WorkError {
        match error.raw_os_error() {
            Some(libc::ENOENT) => failure(FailureCode::ArtifactMissing),
            Some(libc::ELOOP | libc::ENOTDIR) => failure(FailureCode::ScopeMismatch),
            _ => failure(FailureCode::StorageUnavailable),
        }
    }
    fn name(value: &std::ffi::OsStr) -> WorkResult<CString> {
        CString::new(value.as_bytes()).map_err(|_| failure(FailureCode::InvalidInput))
    }
    fn open_at(
        dir: &File,
        name: &CStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> WorkResult<File> {
        // SAFETY: directory descriptor and NUL-terminated name remain alive through openat;
        // on success the new descriptor has one owner, the returned File.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode,
            )
        };
        if fd < 0 {
            return Err(io_failure(std::io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn directory(path: &Path, create: bool) -> WorkResult<File> {
        if !path.is_absolute() {
            return Err(failure(FailureCode::ScopeMismatch));
        }
        let mut dir = File::open("/").map_err(io_failure)?;
        for component in path.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(value) => {
                    let value = name(value)?;
                    if create {
                        // SAFETY: valid directory fd and CString; EEXIST is checked by openat below.
                        let result =
                            unsafe { libc::mkdirat(dir.as_raw_fd(), value.as_ptr(), 0o700) };
                        if result != 0
                            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                        {
                            return Err(failure(FailureCode::StorageUnavailable));
                        }
                    }
                    dir = open_at(&dir, &value, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                }
                _ => return Err(failure(FailureCode::ScopeMismatch)),
            }
        }
        if create {
            // Only the explicitly configured blob root is chmodded, never its ancestors.
            // SAFETY: dir owns a valid fd.
            if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
                return Err(failure(FailureCode::StorageUnavailable));
            }
        }
        Ok(dir)
    }
    fn source(repo: &File, relative: &Path) -> WorkResult<File> {
        let mut dir = repo.try_clone().map_err(io_failure)?;
        let components: Vec<_> = relative
            .components()
            .filter_map(|c| match c {
                Component::Normal(v) => Some(v),
                _ => None,
            })
            .collect();
        for (index, component) in components.iter().enumerate() {
            let flags = if index + 1 == components.len() {
                libc::O_RDONLY | libc::O_NONBLOCK
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            };
            dir = open_at(&dir, &name(component)?, flags, 0)?;
        }
        Ok(dir)
    }
    fn bytes(mut file: File, expected: &ArtifactRef) -> WorkResult<Vec<u8>> {
        let before = file.metadata().map_err(io_failure)?;
        if !before.is_file() {
            return Err(failure(FailureCode::ScopeMismatch));
        }
        if before.len() > MAX_ARTIFACT_BYTES {
            return Err(failure(FailureCode::ResourceLimit));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_ARTIFACT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_failure)?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(failure(FailureCode::ResourceLimit));
        }
        let after = file.metadata().map_err(io_failure)?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
            || expected.size_bytes != bytes.len() as u64
            || expected.sha256 != format!("{:x}", Sha256::digest(&bytes))
        {
            return Err(failure(FailureCode::ArtifactChanged));
        }
        Ok(bytes)
    }
    fn quota_lock(root: &File) -> WorkResult<File> {
        let lock = open_at(
            root,
            c".quota-lock",
            libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
            0o600,
        )?;
        let metadata = lock.metadata().map_err(io_failure)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(failure(FailureCode::ScopeMismatch));
        }
        // SAFETY: lock owns a valid descriptor. This file is never renamed or unlinked.
        if unsafe { libc::fchmod(lock.as_raw_fd(), 0o600) } != 0 {
            return Err(failure(FailureCode::StorageUnavailable));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: the descriptor remains owned until the returned File is dropped,
            // which also releases the kernel's cross-process advisory lock.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(lock);
            }
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK | libc::EINTR)) {
                return Err(io_failure(error));
            }
            if std::time::Instant::now() >= deadline {
                return Err(failure(FailureCode::ResourceLimit));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    fn errno_slot() -> *mut libc::c_int {
        // SAFETY: libc returns this thread's errno slot.
        unsafe { libc::__errno_location() }
    }
    #[cfg(target_os = "macos")]
    fn errno_slot() -> *mut libc::c_int {
        // SAFETY: libc returns this thread's errno slot.
        unsafe { libc::__error() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn errno_slot() -> *mut libc::c_int {
        std::ptr::null_mut()
    }

    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns the stream and its descriptor.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    struct Usage {
        bytes: u64,
        entries: usize,
    }
    fn quota_usage(root: &File, maximum_bytes: u64, maximum_entries: usize) -> WorkResult<Usage> {
        let errno = errno_slot();
        if errno.is_null() {
            return Err(failure(FailureCode::CapabilityUnavailable));
        }
        let fd = root.try_clone().map_err(io_failure)?.into_raw_fd();
        // SAFETY: fd is owned here; fdopendir takes ownership only on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(failure(FailureCode::StorageUnavailable));
        }
        let stream = DirectoryStream(stream);
        // SAFETY: the stream is uniquely owned and remains valid through this scan.
        unsafe {
            libc::rewinddir(stream.0);
        }
        let mut usage = Usage {
            bytes: 0,
            entries: 0,
        };
        let mut inodes = std::collections::HashSet::new();
        loop {
            // SAFETY: errno belongs to this thread; readdir's result is consumed before
            // the next call. Clearing errno distinguishes EOF from a failed partial scan.
            let entry = unsafe {
                *errno = 0;
                libc::readdir(stream.0)
            };
            if entry.is_null() {
                if unsafe { *errno } != 0 {
                    return Err(failure(FailureCode::StorageUnavailable));
                }
                break;
            }
            // SAFETY: readdir supplied a live dirent with a NUL-terminated name.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            usage.entries += 1;
            if usage.entries > maximum_entries {
                return Err(failure(FailureCode::ResourceLimit));
            }
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: valid fd/name and writable stat buffer; no links are followed.
            if unsafe {
                libc::fstatat(
                    root.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(failure(FailureCode::StorageUnavailable));
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_size < 0 {
                return Err(failure(FailureCode::ScopeMismatch));
            }
            // A crash may leave both a capture name and its published hard link.
            // Charge that inode once, while charging every directory entry above.
            if inodes.insert((stat.st_dev, stat.st_ino)) {
                usage.bytes = usage
                    .bytes
                    .checked_add(stat.st_size as u64)
                    .ok_or_else(|| failure(FailureCode::ResourceLimit))?;
                if usage.bytes > maximum_bytes {
                    return Err(failure(FailureCode::ResourceLimit));
                }
            }
        }
        Ok(usage)
    }

    fn persist(root: &File, artifact: &ArtifactRef, content: &[u8]) -> WorkResult<()> {
        let temp = CString::new(format!(".capture-{}", uuid::Uuid::new_v4())).unwrap();
        let final_name = CString::new(artifact.sha256.as_str()).unwrap();
        let mut file = open_at(
            root,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let result = (|| {
            // SAFETY: file owns a writable descriptor created exclusively above.
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(failure(FailureCode::StorageUnavailable));
            }
            file.write_all(content).map_err(io_failure)?;
            file.sync_all().map_err(io_failure)?;
            // linkat publishes without replacing any existing version; temp stays private until synced.
            // SAFETY: valid directory descriptor and both CStrings remain live.
            let published = unsafe {
                libc::linkat(
                    root.as_raw_fd(),
                    temp.as_ptr(),
                    root.as_raw_fd(),
                    final_name.as_ptr(),
                    0,
                )
            };
            if published != 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(failure(FailureCode::StorageUnavailable));
                }
                bytes(
                    open_at(root, &final_name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?,
                    artifact,
                )?;
            }
            root.sync_all().map_err(io_failure)
        })();
        // SAFETY: temp was generated here, relative to the still-open root.
        let removed = unsafe { libc::unlinkat(root.as_raw_fd(), temp.as_ptr(), 0) };
        if removed != 0 && result.is_ok() {
            return Err(failure(FailureCode::StorageUnavailable));
        }
        result
    }

    /// Publish uploaded bytes through the same quota and immutable publication path.
    pub fn publish_bytes(root: &Path, artifact: &ArtifactRef, content: &[u8]) -> WorkResult<()> {
        publish_bytes_with_quota(
            root,
            artifact,
            content,
            MAX_STORED_BYTES,
            MAX_STORED_ENTRIES,
        )
    }
    fn publish_bytes_with_quota(
        root: &Path,
        artifact: &ArtifactRef,
        content: &[u8],
        maximum_bytes: u64,
        maximum_entries: usize,
    ) -> WorkResult<()> {
        validate_refs(Path::new("/"), std::slice::from_ref(artifact))?;
        if content.len() as u64 != artifact.size_bytes
            || format!("{:x}", Sha256::digest(content)) != artifact.sha256
        {
            return Err(failure(FailureCode::ArtifactChanged));
        }
        let root = directory(root, true)?;
        let _lock = quota_lock(&root)?;
        let mut usage = quota_usage(&root, maximum_bytes, maximum_entries)?;
        publish_locked(
            &root,
            artifact,
            content,
            &mut usage,
            maximum_bytes,
            maximum_entries,
        )
    }
    fn publish_locked(
        root: &File,
        artifact: &ArtifactRef,
        content: &[u8],
        usage: &mut Usage,
        maximum_bytes: u64,
        maximum_entries: usize,
    ) -> WorkResult<()> {
        let digest = CString::new(artifact.sha256.as_str()).unwrap();
        match open_at(root, &digest, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
            Ok(existing) => {
                bytes(existing, artifact)?;
                return Ok(());
            }
            Err(WorkError(FailureCode::ArtifactMissing)) => {}
            Err(error) => return Err(error),
        }
        let charged = usage
            .bytes
            .checked_add(content.len() as u64)
            .ok_or_else(|| failure(FailureCode::ResourceLimit))?;
        if charged > maximum_bytes || usage.entries.saturating_add(2) > maximum_entries {
            return Err(failure(FailureCode::ResourceLimit));
        }
        persist(root, artifact, content)?;
        usage.bytes = charged;
        usage.entries += 1;
        Ok(())
    }

    pub fn capture(repo: &Path, root: &Path, artifacts: &[ArtifactRef]) -> WorkResult<()> {
        capture_with_quota(repo, root, artifacts, MAX_STORED_BYTES, MAX_STORED_ENTRIES)
    }

    fn capture_with_quota(
        repo: &Path,
        root: &Path,
        artifacts: &[ArtifactRef],
        maximum_bytes: u64,
        maximum_entries: usize,
    ) -> WorkResult<()> {
        validate_refs(repo, artifacts)?;
        let repo_fd = directory(repo, false)?;
        let root_fd = directory(root, true)?;
        let _lock = quota_lock(&root_fd)?;
        let mut usage = quota_usage(&root_fd, maximum_bytes, maximum_entries)?;
        let mut total = 0u64;
        for artifact in artifacts {
            let file = source(&repo_fd, relative_path(repo, &artifact.path)?)?;
            let content = bytes(file, artifact)?;
            total += content.len() as u64;
            if total > MAX_SUBMISSION_BYTES {
                return Err(failure(FailureCode::ResourceLimit));
            }
            publish_locked(
                &root_fd,
                artifact,
                &content,
                &mut usage,
                maximum_bytes,
                maximum_entries,
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod quota_tests {
        use super::*;
        struct Fixture(std::path::PathBuf);
        impl Fixture {
            fn new() -> Self {
                let raw =
                    std::env::temp_dir().join(format!("artifact-quota-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir(&raw).unwrap();
                Self(std::fs::canonicalize(raw).unwrap())
            }
            fn artifact(&self, data: &[u8]) -> ArtifactRef {
                let hash = format!("{:x}", Sha256::digest(data));
                std::fs::write(self.0.join(&hash), data).unwrap();
                ArtifactRef {
                    path: hash.clone(),
                    sha256: hash,
                    size_bytes: data.len() as u64,
                }
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn uploaded_bytes_share_result_quota_and_verify_existing_content() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            let expected = fixture.artifact(b"same bytes");
            capture_with_quota(&fixture.0, &root, std::slice::from_ref(&expected), 10, 10).unwrap();
            publish_bytes_with_quota(&root, &expected, b"same bytes", 10, 10).unwrap();
            let other = fixture.artifact(b"x");
            assert_eq!(
                publish_bytes_with_quota(&root, &other, b"x", 10, 10)
                    .unwrap_err()
                    .0,
                FailureCode::ResourceLimit
            );
            assert_eq!(
                publish_bytes_with_quota(&root, &expected, b"changed", 10, 10)
                    .unwrap_err()
                    .0,
                FailureCode::ArtifactChanged
            );
            std::fs::write(root.join(&expected.sha256), b"tampered!!").unwrap();
            assert_eq!(
                publish_bytes_with_quota(&root, &expected, b"same bytes", 10, 10)
                    .unwrap_err()
                    .0,
                FailureCode::ArtifactChanged
            );
        }

        #[test]
        fn orphans_and_interrupted_capture_bytes_are_charged_before_writing() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("orphan"), b"123").unwrap();
            std::fs::write(root.join(".capture-crashed"), b"45678").unwrap();
            let artifact = fixture.artifact(b"new");
            assert!(matches!(
                capture_with_quota(&fixture.0, &root, std::slice::from_ref(&artifact), 8, 16),
                Err(WorkError(FailureCode::ResourceLimit))
            ));
            assert!(!root.join(&artifact.sha256).exists());
            assert_eq!(
                std::fs::read(root.join(".capture-crashed")).unwrap(),
                b"45678"
            );
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        }

        #[test]
        fn a_verified_digest_is_free_and_crash_hardlinks_count_once() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            let artifact = fixture.artifact(b"one");
            capture_with_quota(&fixture.0, &root, std::slice::from_ref(&artifact), 3, 16).unwrap();
            std::fs::hard_link(root.join(&artifact.sha256), root.join(".capture-crashed")).unwrap();
            capture_with_quota(&fixture.0, &root, std::slice::from_ref(&artifact), 3, 16).unwrap();
            let fd = directory(&root, false).unwrap();
            assert_eq!(quota_usage(&fd, 3, 16).unwrap().bytes, 3);
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        }

        #[test]
        fn zero_byte_entries_cannot_exhaust_unbounded_directory_memory() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("orphan-one"), b"").unwrap();
            std::fs::write(root.join("orphan-two"), b"").unwrap();
            let artifact = fixture.artifact(b"");
            assert!(matches!(
                capture_with_quota(&fixture.0, &root, &[artifact], 16, 3),
                Err(WorkError(FailureCode::ResourceLimit))
            ));
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        }

        #[test]
        #[ignore = "Run by the cross-process lock test"]
        fn lock_probe_subprocess() {
            let Some(root) = std::env::var_os("MUQUN_ARTIFACT_LOCK_PROBE") else {
                return;
            };
            let root = directory(Path::new(&root), false).unwrap();
            let lock = open_at(&root, c".quota-lock", libc::O_RDWR | libc::O_NONBLOCK, 0).unwrap();
            // SAFETY: lock owns an open descriptor, independent from the parent process.
            let acquired = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if std::env::var_os("MUQUN_ARTIFACT_LOCK_HELD").is_some() {
                assert_eq!(acquired, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EWOULDBLOCK)
                );
            } else {
                assert_eq!(acquired, 0);
            }
        }

        #[test]
        fn kernel_lock_excludes_another_process_and_releases_on_drop() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            let fd = directory(&root, true).unwrap();
            let lock = quota_lock(&fd).unwrap();
            let probe = |held: bool| {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--ignored",
                        "--exact",
                        "work_artifacts::native::quota_tests::lock_probe_subprocess",
                    ])
                    .env("MUQUN_ARTIFACT_LOCK_PROBE", &root)
                    .env_remove("MUQUN_ARTIFACT_LOCK_HELD");
                if held {
                    command.env("MUQUN_ARTIFACT_LOCK_HELD", "1");
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stdout)
                );
            };
            probe(true);
            drop(lock);
            probe(false);
        }

        #[test]
        fn concurrent_captures_cannot_both_spend_the_same_remaining_budget() {
            let fixture = Fixture::new();
            let root = fixture.0.join("blobs");
            let first = fixture.artifact(b"aaaa");
            let second = fixture.artifact(b"bbbb");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let workers = [first, second]
                .into_iter()
                .map(|artifact| {
                    let repo = fixture.0.clone();
                    let root = root.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        capture_with_quota(&repo, &root, &[artifact], 4, 16)
                    })
                })
                .collect::<Vec<_>>();
            let results = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, Err(WorkError(FailureCode::ResourceLimit))))
                    .count(),
                1
            );
            assert_eq!(
                quota_usage(&directory(&root, false).unwrap(), 4, 16)
                    .unwrap()
                    .bytes,
                4
            );
        }
    }

    #[test]
    fn opened_source_cannot_be_redirected_by_a_later_symlink_swap() {
        let raw = std::env::temp_dir().join(format!("work-artifact-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&raw).unwrap();
        let root = std::fs::canonicalize(raw).unwrap();
        std::fs::write(root.join("result"), b"authorized").unwrap();
        std::fs::write(root.join("outside"), b"different").unwrap();
        let expected = ArtifactRef {
            path: "result".into(),
            sha256: format!("{:x}", Sha256::digest(b"authorized")),
            size_bytes: 10,
        };
        let fd = source(&directory(&root, false).unwrap(), Path::new("result")).unwrap();
        std::fs::rename(root.join("result"), root.join("original")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("result")).unwrap();
        assert_eq!(bytes(fd, &expected).unwrap(), b"authorized");
        std::fs::remove_dir_all(root).unwrap();
    }

    pub fn retrieve(root: &Path, artifact: &ArtifactRef) -> WorkResult<Vec<u8>> {
        // Validate digest before using it as a filename; no client path is opened here.
        validate_refs(Path::new("/"), std::slice::from_ref(artifact))?;
        let root = directory(root, false)?;
        bytes(
            open_at(
                &root,
                &CString::new(artifact.sha256.as_str()).unwrap(),
                libc::O_RDONLY | libc::O_NONBLOCK,
                0,
            )?,
            artifact,
        )
    }
}

#[cfg(unix)]
pub use native::{capture, publish_bytes, retrieve};

#[cfg(not(unix))]
pub fn publish_bytes(_: &Path, _: &ArtifactRef, _: &[u8]) -> WorkResult<()> {
    Err(failure(FailureCode::CapabilityUnavailable))
}

#[cfg(not(unix))]
pub fn capture(_: &Path, _: &Path, _: &[ArtifactRef]) -> WorkResult<()> {
    Err(failure(FailureCode::CapabilityUnavailable))
}
#[cfg(not(unix))]
pub fn retrieve(_: &Path, _: &ArtifactRef) -> WorkResult<Vec<u8>> {
    Err(failure(FailureCode::CapabilityUnavailable))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{symlink, PermissionsExt};
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("work-artifacts-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn artifact(content: &[u8]) -> ArtifactRef {
        ArtifactRef {
            path: "result.txt".into(),
            sha256: format!("{:x}", Sha256::digest(content)),
            size_bytes: content.len() as u64,
        }
    }
    #[test]
    fn capture_survives_source_changes_and_verifies_retrieved_bytes() {
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let root = fixture.0.join("blobs");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("result.txt"), b"first").unwrap();
        let expected = artifact(b"first");
        capture(&repo, &root, std::slice::from_ref(&expected)).unwrap();
        std::fs::write(repo.join("result.txt"), b"second").unwrap();
        assert_eq!(retrieve(&root, &expected).unwrap(), b"first");
        assert!(matches!(
            capture(&repo, &root, std::slice::from_ref(&expected)),
            Err(WorkError(FailureCode::ArtifactChanged))
        ));
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join(&expected.sha256))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(root.join(&expected.sha256), b"altered").unwrap();
        assert!(matches!(
            retrieve(&root, &expected),
            Err(WorkError(FailureCode::ArtifactChanged))
        ));
    }
    #[test]
    fn traversal_and_symlink_components_are_refused() {
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let root = fixture.0.join("blobs");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(fixture.0.join("outside"), b"secret").unwrap();
        let mut expected = artifact(b"secret");
        expected.path = "../outside".into();
        assert!(matches!(
            capture(&repo, &root, std::slice::from_ref(&expected)),
            Err(WorkError(FailureCode::ScopeMismatch))
        ));
        expected.path = "result.txt".into();
        symlink(fixture.0.join("outside"), repo.join("result.txt")).unwrap();
        assert!(matches!(
            capture(&repo, &root, std::slice::from_ref(&expected)),
            Err(WorkError(FailureCode::ScopeMismatch))
        ));
        symlink(&fixture.0, repo.join("linked")).unwrap();
        expected.path = "linked/outside".into();
        assert!(matches!(
            capture(&repo, &root, std::slice::from_ref(&expected)),
            Err(WorkError(FailureCode::ScopeMismatch))
        ));
    }
    #[test]
    fn missing_files_and_oversized_claims_fail_closed() {
        let fixture = Fixture::new();
        let expected = artifact(b"absent");
        assert!(matches!(
            capture(
                &fixture.0,
                &fixture.0.join("blobs"),
                std::slice::from_ref(&expected)
            ),
            Err(WorkError(FailureCode::ArtifactMissing))
        ));
        let mut large = expected;
        large.size_bytes = MAX_ARTIFACT_BYTES + 1;
        assert!(matches!(
            validate_refs(&fixture.0, &[large]),
            Err(WorkError(FailureCode::ResourceLimit))
        ));
    }
}
